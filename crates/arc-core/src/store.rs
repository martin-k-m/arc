//! Content-addressed blob store.
//!
//! Invariant: a blob file only ever appears at its final path with complete,
//! verified contents. Writes land in `tmp/` and are renamed into place.

use crate::hash::{hash_bytes, hash_file, Digest};
use anyhow::{Context, Result};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct Store {
    pub root: PathBuf,
}

impl Store {
    pub fn open(arc_home: &Path) -> Result<Store> {
        let root = arc_home.join("store");
        fs::create_dir_all(root.join("blobs")).context("creating blob store")?;
        fs::create_dir_all(root.join("tmp")).context("creating store tmp")?;
        Ok(Store { root })
    }

    pub fn blob_path(&self, d: &Digest) -> PathBuf {
        let hex = d.hex();
        self.root.join("blobs").join(&hex[..2]).join(&hex[2..])
    }

    pub fn exists(&self, d: &Digest) -> bool {
        self.blob_path(d).is_file()
    }

    pub fn size_of(&self, d: &Digest) -> Option<u64> {
        fs::metadata(self.blob_path(d)).ok().map(|m| m.len())
    }

    fn tmp(&self) -> PathBuf {
        // Process id plus nanoseconds: unique across concurrent Arc processes.
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        self.root
            .join("tmp")
            .join(format!("{}-{}", std::process::id(), n))
    }

    fn commit(&self, tmp: &Path, d: &Digest) -> Result<()> {
        let dest = self.blob_path(d);
        if dest.is_file() {
            let _ = fs::remove_file(tmp);
            return Ok(());
        }
        fs::create_dir_all(dest.parent().unwrap())?;
        match fs::rename(tmp, &dest) {
            Ok(()) => Ok(()),
            // A concurrent Arc committed the identical blob first; identical
            // content means either copy is correct.
            Err(_) if dest.is_file() => {
                let _ = fs::remove_file(tmp);
                Ok(())
            }
            Err(e) => Err(e).with_context(|| format!("committing blob {}", dest.display())),
        }
    }

    pub fn put_bytes(&self, bytes: &[u8]) -> Result<Digest> {
        let d = hash_bytes(bytes);
        if self.exists(&d) {
            return Ok(d);
        }
        let tmp = self.tmp();
        let mut f = File::create(&tmp).context("writing blob")?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        self.commit(&tmp, &d)?;
        Ok(d)
    }

    /// Ingest an existing file. Returns its digest and size.
    pub fn put_file(&self, path: &Path) -> Result<(Digest, u64)> {
        let d = hash_file(path)?;
        let size = fs::metadata(path)?.len();
        if self.exists(&d) {
            return Ok((d, size));
        }
        let tmp = self.tmp();
        fs::copy(path, &tmp).with_context(|| format!("copying {}", path.display()))?;
        // A write handle is required to flush: `sync_all` on a read-only handle
        // is rejected on Windows.
        fs::OpenOptions::new().write(true).open(&tmp)?.sync_all()?;
        self.commit(&tmp, &d)?;
        Ok((d, size))
    }

    pub fn read(&self, d: &Digest) -> Result<Vec<u8>> {
        fs::read(self.blob_path(d)).with_context(|| format!("reading blob {}", d.short()))
    }

    /// Copy a blob out to `dest`, atomically, verifying nothing about `dest`.
    pub fn materialize(&self, d: &Digest, dest: &Path, exec: bool) -> Result<()> {
        let src = self.blob_path(d);
        anyhow::ensure!(src.is_file(), "cache object {} is missing", d.short());
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = dest.with_extension(format!("arc-tmp-{}", std::process::id()));
        fs::copy(&src, &tmp).with_context(|| format!("restoring {}", dest.display()))?;
        set_exec(&tmp, exec)?;
        fs::rename(&tmp, dest).or_else(|e| {
            // Windows refuses to rename over an existing file.
            let _ = fs::remove_file(dest);
            fs::rename(&tmp, dest).map_err(|_| e)
        })?;
        Ok(())
    }

    /// Re-hash a stored blob. Returns the actual digest when it does not match.
    pub fn verify(&self, d: &Digest) -> Result<Option<Digest>> {
        let actual = hash_file(&self.blob_path(d))?;
        Ok((actual != *d).then_some(actual))
    }

    /// Move a corrupt object out of the way so it can never be served again.
    pub fn quarantine(&self, d: &Digest) -> Result<PathBuf> {
        let dest = self.root.join("quarantine").join(d.hex());
        fs::create_dir_all(dest.parent().unwrap())?;
        fs::rename(self.blob_path(d), &dest)?;
        Ok(dest)
    }

    pub fn iter_blobs(&self) -> Result<Vec<(Digest, u64)>> {
        let mut out = Vec::new();
        let blobs = self.root.join("blobs");
        for shard in fs::read_dir(&blobs)? {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue;
            }
            let prefix = shard.file_name().to_string_lossy().to_string();
            for f in fs::read_dir(shard.path())? {
                let f = f?;
                let hex = format!("{prefix}{}", f.file_name().to_string_lossy());
                if let Ok(d) = Digest::parse(&hex) {
                    out.push((d, f.metadata()?.len()));
                }
            }
        }
        Ok(out)
    }

    pub fn remove(&self, d: &Digest) -> Result<()> {
        let p = self.blob_path(d);
        if p.exists() {
            fs::remove_file(&p).with_context(|| format!("removing {}", p.display()))?;
        }
        Ok(())
    }

    pub fn total_size(&self) -> Result<u64> {
        Ok(self.iter_blobs()?.iter().map(|(_, s)| s).sum())
    }
}

#[cfg(unix)]
fn set_exec(p: &Path, exec: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = fs::metadata(p)?.permissions();
    let mode = perm.mode();
    perm.set_mode(if exec { mode | 0o755 } else { mode & !0o111 });
    fs::set_permissions(p, perm)?;
    Ok(())
}
#[cfg(not(unix))]
fn set_exec(_p: &Path, _exec: bool) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_dedup_verify() {
        let tmp = tempfile::tempdir().unwrap();
        let s = Store::open(tmp.path()).unwrap();
        let a = s.put_bytes(b"hello").unwrap();
        let b = s.put_bytes(b"hello").unwrap();
        assert_eq!(a, b);
        assert_eq!(s.read(&a).unwrap(), b"hello");
        assert_eq!(s.iter_blobs().unwrap().len(), 1);
        assert!(s.verify(&a).unwrap().is_none());

        fs::write(s.blob_path(&a), b"tampered").unwrap();
        assert!(s.verify(&a).unwrap().is_some());
    }

    #[test]
    fn materialize_overwrites_atomically() {
        let tmp = tempfile::tempdir().unwrap();
        let s = Store::open(tmp.path()).unwrap();
        let d = s.put_bytes(b"new").unwrap();
        let dest = tmp.path().join("out/file.txt");
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::write(&dest, b"old").unwrap();
        s.materialize(&d, &dest, false).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"new");
    }
}
