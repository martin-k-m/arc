//! Optional Git awareness.
//!
//! Arc's execution caching never depends on Git. This module exists so that
//! `arc affected` can ask "what has changed?" when a repository happens to be
//! there, and answer honestly when it is not.
//!
//! Everything here parses `--porcelain=v1 -z`, the documented machine-readable
//! form: NUL-delimited, never localised, never quoted. Parsing human-formatted
//! Git output would break the first time someone sets a different locale.

use crate::key::which;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
}

impl ChangeKind {
    pub fn label(&self) -> &'static str {
        match self {
            ChangeKind::Modified => "modified",
            ChangeKind::Added => "added",
            ChangeKind::Deleted => "deleted",
            ChangeKind::Renamed => "renamed",
            ChangeKind::Untracked => "untracked",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    /// Repository-relative, `/`-separated, as Git reports it.
    pub path: String,
    pub kind: ChangeKind,
    /// Previous path for a rename.
    pub from: Option<String>,
}

/// Why Git information is unavailable, so callers can say so instead of
/// pretending there were no changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unavailable {
    NotInstalled,
    NotARepository,
    Failed(String),
}

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unavailable::NotInstalled => write!(f, "git is not on PATH"),
            Unavailable::NotARepository => write!(f, "not a git repository"),
            Unavailable::Failed(e) => write!(f, "git failed: {e}"),
        }
    }
}

pub fn available(cwd: &Path) -> bool {
    which("git", cwd).is_some()
}

fn git(cwd: &Path, args: &[&str]) -> std::result::Result<String, Unavailable> {
    let exe = which("git", cwd).ok_or(Unavailable::NotInstalled)?;
    let out = Command::new(exe)
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| Unavailable::Failed(e.to_string()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if err.contains("not a git repository") {
            Unavailable::NotARepository
        } else {
            Unavailable::Failed(err)
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

pub fn head(cwd: &Path) -> std::result::Result<String, Unavailable> {
    Ok(git(cwd, &["rev-parse", "HEAD"])?.trim().to_string())
}

/// Working-tree changes against HEAD, including untracked files.
pub fn changes(cwd: &Path) -> std::result::Result<Vec<Change>, Unavailable> {
    let raw = git(
        cwd,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    Ok(parse_status(&raw))
}

/// `XY path\0` records, with an extra `\0`-terminated field carrying the old
/// path when the status is a rename or copy.
fn parse_status(raw: &str) -> Vec<Change> {
    let mut out = Vec::new();
    let mut fields = raw.split('\0').filter(|f| !f.is_empty());
    while let Some(entry) = fields.next() {
        if entry.len() < 4 {
            continue;
        }
        let code = &entry[..2];
        let path = entry[3..].to_string();
        let staged = code.as_bytes()[0] as char;
        let worktree = code.as_bytes()[1] as char;
        let (kind, from) = match (staged, worktree) {
            ('?', _) => (ChangeKind::Untracked, None),
            ('R', _) | ('C', _) => (ChangeKind::Renamed, fields.next().map(str::to_string)),
            ('A', _) => (ChangeKind::Added, None),
            // A deletion staged but restored in the tree is not a deletion.
            ('D', _) | (_, 'D') if worktree != 'M' => (ChangeKind::Deleted, None),
            _ => (ChangeKind::Modified, None),
        };
        out.push(Change { path, kind, from });
    }
    out
}

/// Every path a change touches, including a rename's origin: renaming a
/// dependency changes the execution just as much as editing it.
pub fn touched_paths(changes: &[Change]) -> Vec<String> {
    let mut v: Vec<String> = Vec::with_capacity(changes.len());
    for c in changes {
        v.push(c.path.clone());
        if let Some(f) = &c.from {
            v.push(f.clone());
        }
    }
    v.sort();
    v.dedup();
    v
}

pub fn repo_root(cwd: &Path) -> Result<Option<std::path::PathBuf>> {
    Ok(match git(cwd, &["rev-parse", "--show-toplevel"]) {
        Ok(s) => Some(std::path::PathBuf::from(crate::paths::display_form(
            Path::new(s.trim()),
        ))),
        Err(_) => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_v1_is_parsed_including_renames_and_untracked() {
        let raw = " M src/a.rs\0A  src/b.rs\0 D docs/old.md\0R  new.rs\0old.rs\0?? scratch.txt\0";
        let c = parse_status(raw);
        assert_eq!(c.len(), 5);
        assert_eq!(c[0].path, "src/a.rs");
        assert_eq!(c[0].kind, ChangeKind::Modified);
        assert_eq!(c[1].kind, ChangeKind::Added);
        assert_eq!(c[2].kind, ChangeKind::Deleted);
        assert_eq!(c[3].kind, ChangeKind::Renamed);
        assert_eq!(c[3].from.as_deref(), Some("old.rs"));
        assert_eq!(c[4].kind, ChangeKind::Untracked);
        assert!(touched_paths(&c).contains(&"old.rs".to_string()));
    }

    #[test]
    fn paths_with_spaces_survive_nul_delimited_parsing() {
        let c = parse_status(" M my docs/a b.md\0");
        assert_eq!(c[0].path, "my docs/a b.md");
    }
}
