//! Capturing declared output files after an execution, and restoring them on a
//! hit. Every restored path is checked to stay inside the project root.

use crate::record::OutputFile;
use crate::scan::build_globs;
use crate::store::Store;
use anyhow::{Context, Result};
use std::path::{Component, Path, PathBuf};

/// Collect files under `root` matching `globs`.
pub fn capture(root: &Path, globs: &[String], store: &Store) -> Result<Vec<OutputFile>> {
    if globs.is_empty() {
        return Ok(Vec::new());
    }
    let set = build_globs(globs)?;
    let mut out = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .parents(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .follow_links(false)
        .build();
    for entry in walker {
        let entry = entry.context("scanning outputs")?;
        let Some(ft) = entry.file_type() else {
            continue;
        };
        // Symlinks are not captured: restoring one would recreate a pointer
        // whose target Arc never validated.
        if !ft.is_file() {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if !set.is_match(&rel) {
            continue;
        }
        let (digest, size) = store.put_file(entry.path())?;
        out.push(OutputFile {
            rel,
            digest: digest.hex(),
            size,
            exec: is_exec(entry.path()),
        });
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(out)
}

/// Restore captured outputs into the project. Refuses any path that escapes
/// `root`, whether through `..`, an absolute path, a symlinked parent, or a
/// destination that is itself a symlink.
pub fn restore(root: &Path, outputs: &[OutputFile], store: &Store) -> Result<u64> {
    let mut bytes = 0;
    // Resolve every destination before writing any, so an unsafe entry aborts
    // the restore instead of leaving it half applied.
    let mut planned: Vec<(PathBuf, &OutputFile)> = Vec::with_capacity(outputs.len());
    for o in outputs {
        planned.push((safe_join(root, &o.rel)?, o));
    }
    for (dest, o) in planned {
        let digest = crate::hash::Digest::parse(&o.digest)?;
        anyhow::ensure!(
            store.exists(&digest),
            "cache object {} for {} is missing",
            digest.short(),
            o.rel
        );
        store.materialize(&digest, &dest, o.exec)?;
        bytes += o.size;
    }
    Ok(bytes)
}

/// Join a recorded relative path onto `root`, rejecting anything that could
/// land outside it, and any destination that is itself a symlink.
pub fn safe_join(root: &Path, rel: &str) -> Result<PathBuf> {
    let p = Path::new(rel);
    anyhow::ensure!(
        !p.is_absolute(),
        "refusing to restore absolute path `{rel}`"
    );
    for c in p.components() {
        match c {
            Component::Normal(_) => {}
            _ => anyhow::bail!(
                "refusing to restore `{rel}`: cache entries may only contain paths inside the project"
            ),
        }
    }
    let dest = root.join(p);
    // A symlinked ancestor could redirect the write outside the project.
    let mut cur = dest.parent();
    while let Some(dir) = cur {
        if !dir.starts_with(root) {
            break;
        }
        if let Ok(md) = std::fs::symlink_metadata(dir) {
            if md.file_type().is_symlink() {
                anyhow::bail!(
                    "refusing to restore `{rel}`: `{}` is a symlink",
                    dir.display()
                );
            }
        }
        if dir == root {
            break;
        }
        cur = dir.parent();
    }
    // And the destination itself. `docs/security.md` has always said this was
    // refused; until now only the ancestors were checked, and the property
    // survived by accident because `Store::materialize` renames over the
    // destination rather than opening it. Checking it here makes the guarantee
    // belong to the function that documents it.
    //
    // `symlink_metadata` does not follow the link, so a dangling one is caught
    // too -- which matters, because a dangling symlink is the easy version of
    // the attack: create it pointing anywhere, and let the write create the
    // target.
    if let Ok(md) = std::fs::symlink_metadata(&dest) {
        if md.file_type().is_symlink() {
            anyhow::bail!(
                "refusing to restore `{rel}`: `{}` is a symlink",
                dest.display()
            );
        }
    }
    Ok(dest)
}

#[cfg(unix)]
fn is_exec(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}
#[cfg(not(unix))]
fn is_exec(_p: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traversal_is_refused() {
        let root = Path::new("/repo");
        assert!(safe_join(root, "../../.ssh/config").is_err());
        assert!(safe_join(root, "a/../../b").is_err());
        assert!(safe_join(root, "/etc/passwd").is_err());
        assert!(safe_join(root, "target/debug/app").is_ok());
    }

    /// `docs/security.md` has claimed for some time that "a destination that
    /// is itself a symlink is rejected". It was not: `safe_join` walked
    /// `dest.parent()` upward and never looked at `dest`. The write did not in
    /// fact escape, because `Store::materialize` renames a temp file over the
    /// destination and a rename replaces a symlink rather than following it --
    /// but that is an accident of an unrelated function, whose own doc comment
    /// says it verifies "nothing about `dest`". A security property that holds
    /// by accident somewhere else is one refactor away from not holding.
    #[cfg(unix)]
    #[test]
    fn a_destination_that_is_itself_a_symlink_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        std::fs::create_dir_all(root.join("out")).unwrap();
        let outside = tmp.path().join("outside.txt");
        std::fs::write(&outside, b"do not overwrite me").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("out/a.bin")).unwrap();

        let err = safe_join(&root, "out/a.bin")
            .expect_err("a symlinked destination must be refused, not resolved");
        let msg = err.to_string();
        assert!(msg.contains("symlink"), "the refusal should say why: {msg}");

        // A dangling symlink is the same question: the name exists and is not a
        // regular file, so Arc has no business writing through it either.
        std::fs::remove_file(root.join("out/a.bin")).unwrap();
        std::os::unix::fs::symlink("nowhere-at-all", root.join("out/a.bin")).unwrap();
        assert!(safe_join(&root, "out/a.bin").is_err());

        // An ordinary existing file is still perfectly restorable over.
        std::fs::remove_file(root.join("out/a.bin")).unwrap();
        std::fs::write(root.join("out/a.bin"), b"old").unwrap();
        assert!(safe_join(&root, "out/a.bin").is_ok());
        // As is a path that does not exist yet, which is the common case.
        assert!(safe_join(&root, "out/new.bin").is_ok());
    }

    /// The rejection has to happen in the same pass as every other check, or
    /// the "applied nowhere" half of the documented guarantee is lost.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_destination_aborts_the_restore_before_anything_is_written() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let store = Store::open(&tmp.path().join(".store")).unwrap();
        let d = store.put_bytes(b"x").unwrap();
        let outside = tmp.path().join("outside.txt");
        std::fs::write(&outside, b"original").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("evil.txt")).unwrap();

        let outputs = vec![
            OutputFile {
                rel: "first.txt".into(),
                digest: d.hex(),
                size: 1,
                exec: false,
            },
            OutputFile {
                rel: "evil.txt".into(),
                digest: d.hex(),
                size: 1,
                exec: false,
            },
        ];
        assert!(restore(&root, &outputs, &store).is_err());
        assert!(
            !root.join("first.txt").exists(),
            "the safe entry was applied before the unsafe one was rejected"
        );
        assert_eq!(std::fs::read(&outside).unwrap(), b"original");
    }

    #[test]
    fn capture_and_restore_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let store = Store::open(&root.join(".store")).unwrap();
        std::fs::create_dir_all(root.join("out")).unwrap();
        std::fs::write(root.join("out/a.bin"), b"artifact").unwrap();

        let globs = vec!["out/**".to_string()];
        let captured = capture(root, &globs, &store).unwrap();
        assert_eq!(captured.len(), 1);

        std::fs::remove_file(root.join("out/a.bin")).unwrap();
        restore(root, &captured, &store).unwrap();
        assert_eq!(std::fs::read(root.join("out/a.bin")).unwrap(), b"artifact");
    }

    #[test]
    fn restore_aborts_before_writing_when_an_entry_is_unsafe() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let store = Store::open(&root.join(".store")).unwrap();
        let d = store.put_bytes(b"x").unwrap();
        let outputs = vec![
            OutputFile {
                rel: "ok.txt".into(),
                digest: d.hex(),
                size: 1,
                exec: false,
            },
            OutputFile {
                rel: "../escape.txt".into(),
                digest: d.hex(),
                size: 1,
                exec: false,
            },
        ];
        assert!(restore(root, &outputs, &store).is_err());
        assert!(!root.join("ok.txt").exists());
    }
}
