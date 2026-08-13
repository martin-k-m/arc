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

/// Resolve a revision expression to a commit id. The expression is passed as a
/// single argument to `git rev-parse`, never through a shell, so a branch name
/// containing metacharacters is data rather than syntax.
pub fn resolve(cwd: &Path, rev: &str) -> std::result::Result<String, Unavailable> {
    if rev.trim().is_empty() {
        return Err(Unavailable::Failed("empty revision".into()));
    }
    let spec = format!("{rev}^{{commit}}");
    Ok(git(cwd, &["rev-parse", "--verify", "--quiet", &spec])?
        .trim()
        .to_string())
}

/// Whether a commit is present in this checkout. A shallow clone answers `false`
/// for anything outside its truncated history, which is exactly the case CI must
/// notice rather than treat as "nothing changed".
pub fn has_commit(cwd: &Path, rev: &str) -> bool {
    resolve(cwd, rev).is_ok_and(|s| !s.is_empty())
}

pub fn is_shallow(cwd: &Path) -> bool {
    git(cwd, &["rev-parse", "--is-shallow-repository"])
        .map(|s| s.trim() == "true")
        .unwrap_or(false)
}

/// Fetch history from a remote. Only ever called when the user asked for it:
/// Arc does not mutate a repository as a side effect of analysing it.
pub fn fetch(cwd: &Path, remote: &str, refspec: &str, depth: Option<u32>) -> Result<()> {
    let deepen;
    let mut args = vec!["fetch", "--no-tags", "--quiet", remote, refspec];
    if let Some(d) = depth {
        deepen = format!("--deepen={d}");
        args.insert(1, &deepen);
    }
    git(cwd, &args).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

/// Changes between two commits, from `--name-status -z`: machine-readable,
/// NUL-delimited, rename and copy detection on.
pub fn changed_between(
    cwd: &Path,
    base: &str,
    head: &str,
) -> std::result::Result<Vec<Change>, Unavailable> {
    let range = format!("{base}..{head}");
    let raw = git(
        cwd,
        &[
            "diff",
            "--name-status",
            "-z",
            "-M",
            "-C",
            "--no-renames-empty",
            &range,
        ],
    )
    // `--no-renames-empty` is not universal; retry without it rather than
    // reporting no changes because of a Git version difference.
    .or_else(|_| git(cwd, &["diff", "--name-status", "-z", "-M", "-C", &range]))?;
    Ok(parse_name_status(&raw))
}

/// Changes between a commit and the working tree, including untracked files.
/// This is what a developer running `arc ci --base origin/main` locally means.
pub fn changed_since(cwd: &Path, base: &str) -> std::result::Result<Vec<Change>, Unavailable> {
    let raw = git(cwd, &["diff", "--name-status", "-z", "-M", "-C", base])?;
    let mut out = parse_name_status(&raw);
    out.extend(changes(cwd)?);
    out.sort_by(|a, b| (&a.path, a.kind.label()).cmp(&(&b.path, b.kind.label())));
    out.dedup_by(|a, b| a.path == b.path && a.kind == b.kind && a.from == b.from);
    Ok(out)
}

/// `X\0path\0` records, with `R100\0old\0new\0` and `C100\0src\0dst\0` for
/// renames and copies. The status letter is followed by a similarity score, so
/// only its first byte is significant.
fn parse_name_status(raw: &str) -> Vec<Change> {
    let mut out = Vec::new();
    let mut fields = raw.split('\0').filter(|f| !f.is_empty());
    while let Some(status) = fields.next() {
        let Some(code) = status.as_bytes().first().copied() else {
            continue;
        };
        match code {
            b'R' | b'C' => {
                let (Some(from), Some(to)) = (fields.next(), fields.next()) else {
                    break;
                };
                out.push(Change {
                    path: to.to_string(),
                    kind: ChangeKind::Renamed,
                    from: Some(from.to_string()),
                });
            }
            _ => {
                let Some(path) = fields.next() else { break };
                let kind = match code {
                    b'A' => ChangeKind::Added,
                    b'D' => ChangeKind::Deleted,
                    // T (type change, including a gitlink becoming a file) and
                    // U (unmerged) both mean the path is not what it was.
                    _ => ChangeKind::Modified,
                };
                out.push(Change {
                    path: path.to_string(),
                    kind,
                    from: None,
                });
            }
        }
    }
    out
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

    #[test]
    fn name_status_parses_every_change_kind_including_renames() {
        let raw = "M\0src/a.rs\0A\0src/b.rs\0D\0docs/old.md\0R100\0old.rs\0new.rs\0T\0link\0";
        let c = parse_name_status(raw);
        assert_eq!(c.len(), 5);
        assert_eq!(
            (c[0].kind, c[0].path.as_str()),
            (ChangeKind::Modified, "src/a.rs")
        );
        assert_eq!(c[1].kind, ChangeKind::Added);
        assert_eq!(c[2].kind, ChangeKind::Deleted);
        assert_eq!(c[3].kind, ChangeKind::Renamed);
        assert_eq!(c[3].path, "new.rs");
        assert_eq!(c[3].from.as_deref(), Some("old.rs"));
        assert_eq!(c[4].kind, ChangeKind::Modified);
        assert!(touched_paths(&c).contains(&"old.rs".to_string()));
    }

    #[test]
    fn a_truncated_name_status_stream_does_not_panic() {
        for cut in 0..24 {
            let raw = "R100\0old.rs\0new.rs\0M\0a.rs\0";
            parse_name_status(&raw[..cut.min(raw.len())]);
        }
    }

    #[test]
    fn name_status_handles_newlines_and_tabs_in_paths() {
        let c = parse_name_status("M\0weird\tname\nwith/newline.rs\0");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].path, "weird\tname\nwith/newline.rs");
    }
}
