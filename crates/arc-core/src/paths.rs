//! Path identity.
//!
//! Every part of Arc that compares, stores, or classifies a path goes through
//! this module. Ad-hoc `to_string_lossy().to_lowercase()` scattered across a
//! codebase is how dependency systems grow silent mismatches, so there is
//! exactly one answer here to "are these the same path?".
//!
//! Two representations exist and they are not interchangeable:
//!
//! * **display form** — what a human reads and what is persisted. Absolute,
//!   `\\?\` stripped, separators normalised to `/`.
//! * **comparison form** — [`PathKey`], used for equality and map lookups. On
//!   Windows it is additionally case-folded, because `SRC\Main.rs` and
//!   `src/main.rs` name the same file there and must not become two
//!   dependencies.
//!
//! Case folding is deliberately *not* applied on Unix, where those really are
//! different files.

use crate::hash::{hash_bytes, Digest};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

/// Where a path sits relative to the things Arc is allowed to reason about.
///
/// The distinction drives policy, not cosmetics: only `Project` paths take part
/// in cache keys today, and `ArcInternal` paths must never do so or Arc would
/// invalidate itself on every run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Inside the project root.
    Project,
    /// Inside Arc's own home. Written by Arc during every run.
    ArcInternal,
    /// Outside the project but under the user's control (home directory,
    /// a sibling checkout, a toolchain install).
    External,
    /// Operating-system owned: system directories, temp, package caches.
    System,
}

impl Scope {
    pub fn label(&self) -> &'static str {
        match self {
            Scope::Project => "project",
            Scope::ArcInternal => "arc",
            Scope::External => "external",
            Scope::System => "system",
        }
    }
}

/// A path in comparison form. Construct with [`PathKey::of`]; never build one
/// by hand from a string that has not been normalised.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PathKey(String);

impl PathKey {
    pub fn of(p: &Path) -> PathKey {
        PathKey(fold(&display_form(p)))
    }

    /// For paths already normalised to display form (for example, ones read
    /// back out of the database).
    pub fn from_display(s: &str) -> PathKey {
        PathKey(fold(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn digest(&self) -> Digest {
        hash_bytes(self.0.as_bytes())
    }
}

#[cfg(windows)]
fn fold(s: &str) -> String {
    s.to_lowercase()
}
#[cfg(not(windows))]
fn fold(s: &str) -> String {
    s.to_string()
}

/// Normalise a path for display and storage: absolute where possible, `\\?\`
/// stripped, `.`/`..` resolved lexically, separators as `/`.
///
/// This is purely lexical and never touches the filesystem, so it works for
/// paths that have already been deleted — which matters, because a deleted
/// dependency is still a dependency.
pub fn display_form(p: &Path) -> String {
    let s = p.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    let mut out: Vec<String> = Vec::new();
    let mut prefix = String::new();
    for c in Path::new(s).components() {
        match c {
            Component::Prefix(p) => prefix = p.as_os_str().to_string_lossy().replace('\\', "/"),
            Component::RootDir => out.push(String::new()),
            Component::CurDir => {}
            // Popping is only correct when there is a real segment to pop; a
            // leading `..` must survive or a relative path changes meaning.
            Component::ParentDir => match out.last() {
                Some(seg) if !seg.is_empty() && seg != ".." => {
                    out.pop();
                }
                _ => out.push("..".into()),
            },
            Component::Normal(seg) => out.push(seg.to_string_lossy().to_string()),
        }
    }
    let joined = out.join("/");
    let joined = if joined.is_empty() && !prefix.is_empty() {
        "/".to_string()
    } else {
        joined
    };
    format!("{prefix}{joined}")
}

/// Resolve `p` against the filesystem when it exists, falling back to a lexical
/// join when it does not. Never fails: an unresolvable path is still a path Arc
/// may need to record.
pub fn absolute(p: &Path, base: &Path) -> PathBuf {
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
    match joined.canonicalize() {
        Ok(c) => PathBuf::from(display_form(&c)),
        Err(_) => PathBuf::from(display_form(&joined)),
    }
}

/// Classifies paths against one project. Holding the roots once avoids
/// recomputing normalised prefixes for every observation in a trace.
#[derive(Debug, Clone)]
pub struct Classifier {
    project: PathKey,
    arc_home: PathKey,
    system: Vec<PathKey>,
}

impl Classifier {
    pub fn new(project_root: &Path, arc_home: &Path) -> Classifier {
        Classifier {
            project: PathKey::of(&canonical_root(project_root)),
            arc_home: PathKey::of(&canonical_root(arc_home)),
            system: system_roots().iter().map(|p| PathKey::of(p)).collect(),
        }
    }

    pub fn classify(&self, p: &Path) -> Scope {
        let key = PathKey::of(p);
        // Arc home is checked first: it is frequently placed inside a project
        // during testing, and it must win there.
        if under(&key, &self.arc_home) {
            return Scope::ArcInternal;
        }
        if under(&key, &self.project) {
            return Scope::Project;
        }
        if self.system.iter().any(|r| under(&key, r)) {
            return Scope::System;
        }
        Scope::External
    }

    /// The project-relative form of a path, or `None` if it is not in the
    /// project. Always `/`-separated so records move between platforms.
    pub fn relative(&self, p: &Path) -> Option<String> {
        let key = PathKey::of(p);
        if !under(&key, &self.project) {
            return None;
        }
        let full = display_form(p);
        let root_len = self.project.as_str().len();
        Some(full[root_len..].trim_start_matches('/').to_string())
    }
}

/// The filesystem's own name for a root Arc compares other paths against.
///
/// Only the two roots go through this, never observed paths: those must keep
/// the name the program actually used, symlinks included. The roots are
/// different. They arrive from different places — the project root by walking
/// up from the working directory, the Arc home from `ARC_HOME` or a default —
/// and two spellings of the same directory make `under` answer no.
///
/// That is not cosmetic. An Arc home inside the project that fails to be
/// recognised as Arc's own is scanned as project content, and on Windows the
/// scan then tries to hash the database this process has open and locked. The
/// two spellings that occur in practice are 8.3 short names on Windows
/// (`RUNNER~1`) and macOS's `/var` symlink to `/private/var`.
///
/// The home may not exist yet on a first run, so the deepest ancestor that does
/// exist is canonicalised and the rest re-joined lexically.
pub(crate) fn canonical_root(p: &Path) -> PathBuf {
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    let mut here = p;
    loop {
        if let Ok(real) = here.canonicalize() {
            let mut out = real;
            for seg in tail.iter().rev() {
                out.push(seg);
            }
            return out;
        }
        match (here.file_name(), here.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name);
                here = parent;
            }
            _ => return p.to_path_buf(),
        }
    }
}

/// True when `child` is `root` or lies beneath it. Compares whole segments, so
/// `/repo-old` is not treated as being inside `/repo`.
pub(crate) fn under(child: &PathKey, root: &PathKey) -> bool {
    let (c, r) = (child.as_str(), root.as_str());
    let r = r.strip_suffix('/').unwrap_or(r);
    c == r || (c.len() > r.len() && c.starts_with(r) && c.as_bytes()[r.len()] == b'/')
}

/// Directories owned by the OS or a package manager. Used only to classify
/// observations for reporting: Arc does not fingerprint anything here, so this
/// list being incomplete costs nothing but a less precise label.
fn system_roots() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    let mut push = |s: Option<std::ffi::OsString>| {
        if let Some(s) = s {
            if !s.is_empty() {
                v.push(PathBuf::from(s));
            }
        }
    };
    if cfg!(windows) {
        push(std::env::var_os("SystemRoot"));
        push(std::env::var_os("ProgramData"));
        push(std::env::var_os("TEMP"));
        push(std::env::var_os("TMP"));
    } else {
        for p in [
            "/usr", "/lib", "/lib64", "/bin", "/sbin", "/etc", "/proc", "/sys", "/dev", "/tmp",
        ] {
            v.push(PathBuf::from(p));
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_form_normalises_separators_and_dot_segments() {
        assert_eq!(display_form(Path::new("a/b/../c/./d")), "a/c/d");
        assert_eq!(display_form(Path::new("../x")), "../x");
        assert_eq!(display_form(Path::new("a/../../x")), "../x");
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_are_case_insensitive_and_verbatim_prefixes_are_stripped() {
        assert_eq!(
            PathKey::of(Path::new(r"C:\Repo\Src\Main.rs")),
            PathKey::of(Path::new(r"c:/repo/src/main.rs"))
        );
        assert_eq!(
            display_form(Path::new(r"\\?\C:\repo\a.txt")),
            "C:/repo/a.txt"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_paths_are_case_sensitive() {
        assert_ne!(
            PathKey::of(Path::new("/repo/Main.rs")),
            PathKey::of(Path::new("/repo/main.rs"))
        );
    }

    #[test]
    fn sibling_directories_are_not_inside_each_other() {
        let root = if cfg!(windows) { r"C:\repo" } else { "/repo" };
        let home = if cfg!(windows) { r"C:\arc" } else { "/arc" };
        let c = Classifier::new(Path::new(root), Path::new(home));
        let sibling = if cfg!(windows) {
            r"C:\repo-old\a.txt"
        } else {
            "/repo-old/a.txt"
        };
        assert_eq!(c.classify(Path::new(sibling)), Scope::External);
        let inside = if cfg!(windows) {
            r"C:\repo\src\a.rs"
        } else {
            "/repo/src/a.rs"
        };
        assert_eq!(c.classify(Path::new(inside)), Scope::Project);
        assert_eq!(c.relative(Path::new(inside)).unwrap(), "src/a.rs");
    }

    #[test]
    fn arc_home_inside_the_project_is_classified_as_arc_internal() {
        let root = if cfg!(windows) { r"C:\repo" } else { "/repo" };
        let home = if cfg!(windows) {
            r"C:\repo\.arc-home"
        } else {
            "/repo/.arc-home"
        };
        let c = Classifier::new(Path::new(root), Path::new(home));
        let p = if cfg!(windows) {
            r"C:\repo\.arc-home\arc.redb"
        } else {
            "/repo/.arc-home/arc.redb"
        };
        assert_eq!(c.classify(Path::new(p)), Scope::ArcInternal);
    }
}
