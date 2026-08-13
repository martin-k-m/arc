//! Input discovery and fingerprinting.

use crate::hash::{hash_bytes, hash_file, Digest, Hasher};
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
const MTIME_TRUST_LAG_MS: i64 = 2_000;

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

    let mut paths: Vec<String> = Vec::new();
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
        if skip.iter().any(|s| entry.path().starts_with(s)) {
            continue;
        }
        if ft.is_dir() {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if rel.is_empty() || exclude.is_match(&rel) {
            continue;
        }
        if use_include && !include.is_match(&rel) {
            continue;
        }
        paths.push(rel);
    }
    paths.sort();
    paths.dedup();

    let now_ms = now_millis();
    let entries: Vec<FileEntry> = paths
        .par_iter()
        .map(|rel| fingerprint_one(root, rel, fps, now_ms))
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

    for e in &entries {
        if let Ok(md) = std::fs::symlink_metadata(root.join(&e.rel)) {
            fps.insert(e.rel.clone(), (md.len(), mtime_millis(&md), e.digest));
        }
    }

    Ok(InputSet {
        files: entries,
        digest,
        bytes_hashed,
        reused_fingerprints: reused,
    })
}

fn fingerprint_one(root: &Path, rel: &str, fps: &FingerprintMap, now_ms: i64) -> Result<FileEntry> {
    let path = root.join(rel);
    let md = std::fs::symlink_metadata(&path)
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
        let target = std::fs::read_link(&path)?;
        hash_bytes(target.to_string_lossy().replace('\\', "/").as_bytes())
    } else {
        hash_file(&path)?
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

fn mtime_millis(md: &std::fs::Metadata) -> i64 {
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
