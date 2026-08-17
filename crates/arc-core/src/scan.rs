//! Input discovery and fingerprinting.

use crate::hash::{hash_bytes, hash_file, Digest, Hasher};
use crate::paths::{canonical_root, under, PathKey};
use crate::project::{Config, DEFAULT_EXCLUDES};
use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub rel: String,
    pub digest: Digest,
    pub size: u64,
    pub exec: bool,
    pub symlink: bool,
}

#[derive(Debug, Default)]
pub struct InputSet {
    pub files: Vec<FileEntry>,
    pub digest: Digest,
    pub bytes_hashed: u64,
    pub reused_fingerprints: usize,
}

/// `rel path -> (size, mtime_millis, digest)`, persisted between runs so
/// unchanged files are not re-read. Correctness never depends on it: a stale or
/// missing entry only costs a re-hash.
pub type FingerprintMap = HashMap<String, (u64, i64, Digest)>;

/// Files whose mtime is younger than this are always re-hashed, since a write
/// within the filesystem's timestamp granularity could otherwise go unseen.
pub const MTIME_TRUST_LAG_MS: i64 = 2_000;

pub fn build_globs(patterns: &[String]) -> Result<GlobSet> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        b.add(Glob::new(p).with_context(|| format!("invalid glob: {p}"))?);
    }
    b.build().context("building glob set")
}

pub fn default_exclude_globs(cfg: &Config) -> Result<GlobSet> {
    let mut pats: Vec<String> = DEFAULT_EXCLUDES.iter().map(|s| s.to_string()).collect();
    pats.extend(cfg.inputs.exclude.iter().cloned());
    // Anything Arc will capture as an output is derived, never an input.
    pats.extend(cfg.outputs.include.iter().cloned());
    build_globs(&pats)
}

/// Walk the project and fingerprint every relevant file.
///
/// `skip` holds absolute directories that are never inputs no matter what the
/// configuration says — in practice the Arc home, which Arc itself writes to
/// during the run and which would otherwise invalidate every key.
pub fn scan_inputs(
    root: &Path,
    cfg: &Config,
    fps: &mut FingerprintMap,
    skip: &[PathBuf],
) -> Result<InputSet> {
    let exclude = default_exclude_globs(cfg)?;
    let include = build_globs(&cfg.inputs.include)?;
    let use_include = !cfg.inputs.include.is_empty();

    // `skip` arrives from configuration and the environment; the walk produces
    // paths derived from `root`. Two spellings of one directory — an 8.3 short
    // name on Windows, macOS's /var symlink to /private/var, or different
    // capitalisation — make a plain prefix test answer no. That is not a
    // cosmetic miss: an Arc home that escapes this list is scanned as project
    // content, and Arc then tries to hash the database it has open.
    //
    // Resolved once, into project-relative prefixes, so the test inside the
    // walk stays a string comparison — canonicalising per entry would cost a
    // syscall for every file in the project.
    let skip: Vec<String> = {
        let root_key = PathKey::of(&canonical_root(root));
        let root_len = root_key.as_str().len();
        skip.iter()
            .filter_map(|s| {
                let key = PathKey::of(&canonical_root(s));
                under(&key, &root_key)
                    .then(|| key.as_str()[root_len..].trim_start_matches('/').to_string())
            })
            .filter(|s| !s.is_empty())
            .collect()
    };

    // The real path is carried alongside its display form. On Unix a filename
    // is bytes, not text, so re-deriving the path from a lossy string would make
    // Arc unable to open the very file it just found.
    let mut paths: Vec<(String, PathBuf)> = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .parents(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .follow_links(false)
        .build();
    for entry in walker {
        let entry = entry.context("walking project")?;
        let Some(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_dir() {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let display = rel.to_string_lossy().replace('\\', "/");
        if display.is_empty() || exclude.is_match(&display) {
            continue;
        }
        let folded = PathKey::from_display(&display);
        let folded = folded.as_str();
        if skip
            .iter()
            .any(|s| folded == s || folded.starts_with(s) && folded[s.len()..].starts_with('/'))
        {
            continue;
        }
        if use_include && !include.is_match(&display) {
            continue;
        }
        paths.push((display, entry.path().to_path_buf()));
    }
    paths.sort();
    paths.dedup();

    let now_ms = now_millis();
    let entries: Vec<FileEntry> = paths
        .par_iter()
        .map(|(rel, path)| fingerprint_one(path, rel, fps, now_ms))
        .collect::<Result<Vec<_>>>()?;

    let mut bytes_hashed = 0;
    let mut reused = 0;
    let mut h = Hasher::new();
    h.field(crate::SCHEMA_VERSION.to_le_bytes());
    for e in &entries {
        h.field(&e.rel);
        h.field([e.exec as u8, e.symlink as u8]);
        h.field(e.digest.bytes());
        match fps.get(&e.rel) {
            Some((_, _, d)) if *d == e.digest => reused += 1,
            _ => bytes_hashed += e.size,
        }
    }
    let digest = h.finish();

    for ((rel, path), e) in paths.iter().zip(&entries) {
        if let Ok(md) = std::fs::symlink_metadata(path) {
            fps.insert(rel.clone(), (md.len(), mtime_millis(&md), e.digest));
        }
    }

    Ok(InputSet {
        files: entries,
        digest,
        bytes_hashed,
        reused_fingerprints: reused,
    })
}

fn fingerprint_one(path: &Path, rel: &str, fps: &FingerprintMap, now_ms: i64) -> Result<FileEntry> {
    let md = std::fs::symlink_metadata(path)
        .with_context(|| format!("reading metadata for {}", path.display()))?;
    let symlink = md.file_type().is_symlink();
    let size = md.len();
    let mtime = mtime_millis(&md);

    if !symlink && now_ms - mtime >= MTIME_TRUST_LAG_MS {
        if let Some((s, m, d)) = fps.get(rel) {
            if *s == size && *m == mtime {
                return Ok(FileEntry {
                    rel: rel.into(),
                    digest: *d,
                    size,
                    exec: is_exec(&md),
                    symlink: false,
                });
            }
        }
    }

    let digest = if symlink {
        let target = std::fs::read_link(path)?;
        hash_bytes(target.to_string_lossy().replace('\\', "/").as_bytes())
    } else if md.file_type().is_file() {
        hash_file(path)?
    } else {
        // A socket, a fifo or a device node has no contents to hash, and
        // opening one either fails or blocks. Its presence is the whole
        // dependency: a project that contains a dev server's socket must still
        // be scannable. See LIMITATIONS.md.
        hash_bytes(b"<not a regular file>")
    };
    Ok(FileEntry {
        rel: rel.into(),
        digest,
        size,
        exec: is_exec(&md),
        symlink,
    })
}

#[cfg(unix)]
fn is_exec(md: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    md.permissions().mode() & 0o111 != 0
}
#[cfg(not(unix))]
fn is_exec(_md: &std::fs::Metadata) -> bool {
    false
}

pub fn mtime_millis(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn digest_changes_with_content_and_ignores_excluded_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "src/a.rs", "fn main() {}");
        write(root, "target/junk.o", "binary");
        let cfg = Config::default();
        let mut fps = FingerprintMap::new();

        let a = scan_inputs(root, &cfg, &mut fps, &[]).unwrap();
        assert_eq!(a.files.len(), 1, "target/ must not be an input");

        let b = scan_inputs(root, &cfg, &mut fps, &[]).unwrap();
        assert_eq!(a.digest, b.digest);

        write(root, "src/a.rs", "fn main() { }");
        let c = scan_inputs(root, &cfg, &mut fps, &[]).unwrap();
        assert_ne!(a.digest, c.digest);
    }

    #[test]
    fn include_globs_narrow_the_input_set() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "src/a.rs", "a");
        write(root, "docs/b.md", "b");
        let mut cfg = Config::default();
        cfg.inputs.include = vec!["src/**".into()];
        let s = scan_inputs(root, &cfg, &mut FingerprintMap::new(), &[]).unwrap();
        assert_eq!(s.files.len(), 1);
        assert_eq!(s.files[0].rel, "src/a.rs");
    }

    #[test]
    fn declared_outputs_are_not_inputs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "a.txt", "a");
        write(root, "out/x.bin", "x");
        let mut cfg = Config::default();
        cfg.outputs.include = vec!["out/**".into()];
        let s = scan_inputs(root, &cfg, &mut FingerprintMap::new(), &[]).unwrap();
        assert_eq!(s.files.len(), 1);
    }
}
