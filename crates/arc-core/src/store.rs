//! Content-addressed blob store.
//!
//! Invariant: a blob file only ever appears at its final path with complete,
//! verified contents. Writes land in `tmp/` and are renamed into place.

use crate::hash::{hash_bytes, hash_file, Digest};
use anyhow::{Context, Result};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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
        // Process id and nanoseconds separate concurrent Arc processes; the
        // counter separates threads within one. The clock alone is not enough:
        // its granularity is about 15ms on Windows, so two threads committing
        // *different* blobs in the same tick used to get the same tmp name.
        // The winner renamed it into place and the loser's rename then failed
        // with "the system cannot find the file specified", which the
        // `dest.is_file()` arm below does not absorb because the loser's
        // destination is a different digest.
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        self.tmp_at(n)
    }

    /// The naming itself, with the clock reading passed in so a test can hold
    /// it still. Two calls in the same tick must still differ.
    fn tmp_at(&self, nanos: u128) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        self.root
            .join("tmp")
            .join(format!("{}-{}-{}", std::process::id(), nanos, seq))
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

    /// Stream bytes from an untrusted source into the store, admitting them
    /// only if they hash to `expect`.
    ///
    /// The data is hashed as it lands in `tmp/`, so nothing is buffered whole in
    /// memory and no unverified byte ever appears at a blob path. A mismatch,
    /// a truncated stream and an interrupted process all leave the same state:
    /// a temporary file and no object.
    pub fn put_verified(&self, r: &mut dyn std::io::Read, expect: &Digest) -> Result<u64> {
        let tmp = self.tmp();
        let mut f = File::create(&tmp).context("writing incoming object")?;
        let mut hasher = crate::hash::Hasher::new();
        let mut buf = vec![0u8; 256 * 1024];
        let mut total = 0u64;
        let outcome = (|| -> Result<()> {
            loop {
                let n = r.read(&mut buf)?;
                if n == 0 {
                    return Ok(());
                }
                total += n as u64;
                anyhow::ensure!(
                    total <= crate::remote::protocol::MAX_OBJECT_BYTES,
                    "object is too large"
                );
                hasher.raw(&buf[..n]);
                f.write_all(&buf[..n])?;
            }
        })();
        let actual = hasher.finish();
        let ok = outcome.is_ok() && actual == *expect;
        if ok {
            f.sync_all()?;
        }
        drop(f);
        if !ok {
            let _ = fs::remove_file(&tmp);
            outcome?;
            anyhow::bail!(
                "object {} does not match its digest (got {})",
                expect.short(),
                actual.short()
            );
        }
        self.commit(&tmp, expect)?;
        Ok(total)
    }

    pub fn open_blob(&self, d: &Digest) -> Result<File> {
        File::open(self.blob_path(d)).with_context(|| format!("reading blob {}", d.short()))
    }

    /// Remove abandoned transfer files. Nothing in `tmp/` is ever a valid
    /// object, so age is the only thing worth checking.
    pub fn sweep_tmp(&self, older_than: std::time::Duration) -> Result<usize> {
        let mut removed = 0;
        let now = std::time::SystemTime::now();
        for e in fs::read_dir(self.root.join("tmp"))? {
            let e = e?;
            let stale = e
                .metadata()
                .and_then(|m| m.modified())
                .map(|m| now.duration_since(m).unwrap_or_default() > older_than)
                .unwrap_or(false);
            if stale && fs::remove_file(e.path()).is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
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

    // The tmp name must not depend on the clock alone. Where the clock is
    // coarse, as on Windows, two threads that ask in the same tick used to get
    // the same path, and whichever renamed second failed outright.
    #[test]
    fn tmp_names_are_distinct_within_one_tick() {
        let tmp = tempfile::tempdir().unwrap();
        let s = Store::open(tmp.path()).unwrap();
        // The clock is held still, which is what a coarse-grained clock does
        // on its own. Without the counter every one of these is the same path,
        // so this fails on any platform rather than only where the tick is
        // long enough to catch it by luck.
        let frozen = 1_700_000_000_000_000_000u128;
        let names: std::collections::HashSet<_> = (0..1_000).map(|_| s.tmp_at(frozen)).collect();
        assert_eq!(names.len(), 1_000, "tmp() handed out a duplicate path");
    }

    // Concurrent commits of *different* blobs. The failure this pins is not a
    // lost blob but an error return: the loser of a tmp-name collision saw
    // "the system cannot find the file specified" because the winner had
    // already renamed the shared tmp file to a different digest's path.
    #[test]
    fn concurrent_puts_of_distinct_blobs_all_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let s = std::sync::Arc::new(Store::open(tmp.path()).unwrap());
        let mut handles = Vec::new();
        for t in 0..8 {
            let s = std::sync::Arc::clone(&s);
            handles.push(std::thread::spawn(move || {
                for i in 0..64 {
                    let body = format!("thread-{t}-item-{i}");
                    s.put_bytes(body.as_bytes())
                        .unwrap_or_else(|e| panic!("put failed: {e:#}"));
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(s.iter_blobs().unwrap().len(), 8 * 64);
    }

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
