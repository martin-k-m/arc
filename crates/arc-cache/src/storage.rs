//! On-disk state for the reference server: a content-addressed object store
//! and namespace-scoped execution records.
//!
//! Both are plain directories. Nothing here needs a database, a daemon or a
//! native library, so a cache can be moved, backed up and inspected with the
//! tools already on the machine.

use anyhow::{bail, Context, Result};
use arc_core::hash::Hasher;
use arc_core::remote::protocol::{self, RemoteExecution, RemoteTask};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub struct Storage {
    root: PathBuf,
    seq: AtomicU64,
}

/// What happened to a `PUT /executions/{key}`.
#[derive(Debug, PartialEq, Eq)]
pub enum Publish {
    Created,
    /// Byte-identical to what is already stored.
    Identical,
    /// A different result for the same execution key. Under Arc's contract at
    /// most one of the two can be valid, and the server cannot tell which, so
    /// it keeps the first and refuses the second rather than serving a result
    /// that contradicts one already handed out.
    Conflict,
}

impl Storage {
    pub fn open(root: &Path) -> Result<Storage> {
        fs::create_dir_all(root.join("objects")).context("creating object store")?;
        fs::create_dir_all(root.join("executions")).context("creating record store")?;
        fs::create_dir_all(root.join("tasks")).context("creating task store")?;
        fs::create_dir_all(root.join("tmp")).context("creating tmp")?;
        Ok(Storage {
            root: root.to_path_buf(),
            seq: AtomicU64::new(0),
        })
    }

    fn tmp(&self) -> PathBuf {
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        self.root
            .join("tmp")
            .join(format!("{}-{n}", std::process::id()))
    }

    /// Digests are validated before they reach here, so the two-character shard
    /// and the remainder are both known-safe hex.
    pub fn object_path(&self, digest: &str) -> PathBuf {
        self.root
            .join("objects")
            .join(&digest[..2])
            .join(&digest[2..])
    }

    fn record_path(&self, ns: &str, key: &str) -> PathBuf {
        self.root
            .join("executions")
            .join(ns)
            .join(format!("{key}.json"))
    }

    fn task_path(&self, ns: &str, family: &str) -> PathBuf {
        self.root
            .join("tasks")
            .join(ns)
            .join(format!("{family}.json"))
    }

    pub fn get_task(&self, ns: &str, family: &str) -> Option<RemoteTask> {
        serde_json::from_slice(&fs::read(self.task_path(ns, family)).ok()?).ok()
    }

    /// Task knowledge is an observation, not a claim of exclusivity: a later
    /// publisher with at least as many observations replaces an earlier one,
    /// and neither is a conflict. Nothing downstream trusts it without
    /// re-deriving the family key locally.
    pub fn put_task(&self, ns: &str, family: &str, task: &RemoteTask) -> Result<()> {
        if let Some(prev) = self.get_task(ns, family) {
            if prev.observations > task.observations {
                return Ok(());
            }
        }
        let dest = self.task_path(ns, family);
        fs::create_dir_all(dest.parent().unwrap())?;
        let tmp = self.tmp();
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&task.canonical())?;
            f.sync_all()?;
        }
        if fs::rename(&tmp, &dest).is_err() {
            let _ = fs::remove_file(&tmp);
        }
        Ok(())
    }

    pub fn has_object(&self, digest: &str) -> bool {
        self.object_path(digest).is_file()
    }

    pub fn object_size(&self, digest: &str) -> Option<u64> {
        fs::metadata(self.object_path(digest)).ok().map(|m| m.len())
    }

    pub fn open_object(&self, digest: &str) -> Result<fs::File> {
        Ok(fs::File::open(self.object_path(digest))?)
    }

    /// Store an incoming object, hashing it independently. A client that asks
    /// to write digest D may only write bytes that hash to D — otherwise the
    /// cache could be poisoned by anyone allowed to upload.
    pub fn put_object(&self, digest: &str, body: &mut dyn Read) -> Result<u64> {
        if !protocol::valid_digest(digest) {
            bail!("malformed digest");
        }
        let tmp = self.tmp();
        let mut f = fs::File::create(&tmp)?;
        let mut hasher = Hasher::new();
        let mut buf = vec![0u8; 256 * 1024];
        let mut total = 0u64;
        let outcome = (|| -> Result<()> {
            loop {
                let n = body.read(&mut buf)?;
                if n == 0 {
                    return Ok(());
                }
                total += n as u64;
                if total > protocol::MAX_OBJECT_BYTES {
                    bail!("object exceeds the size limit");
                }
                hasher.raw(&buf[..n]);
                f.write_all(&buf[..n])?;
            }
        })();
        let actual = hasher.finish().hex();
        let ok = outcome.is_ok() && actual == digest;
        if ok {
            f.sync_all()?;
        }
        drop(f);
        if !ok {
            let _ = fs::remove_file(&tmp);
            outcome?;
            bail!("uploaded bytes hash to {}, not {digest}", &actual[..12]);
        }
        let dest = self.object_path(digest);
        if dest.is_file() {
            let _ = fs::remove_file(&tmp);
            return Ok(total);
        }
        fs::create_dir_all(dest.parent().unwrap())?;
        if fs::rename(&tmp, &dest).is_err() && !dest.is_file() {
            bail!("could not commit object");
        }
        let _ = fs::remove_file(&tmp);
        Ok(total)
    }

    pub fn get_record(&self, ns: &str, key: &str) -> Option<Vec<u8>> {
        fs::read(self.record_path(ns, key)).ok()
    }

    pub fn put_record(&self, ns: &str, key: &str, rec: &RemoteExecution) -> Result<Publish> {
        let canonical = rec.canonical();
        let dest = self.record_path(ns, key);
        if let Ok(existing) = fs::read(&dest) {
            return Ok(agreement(&existing, rec));
        }
        fs::create_dir_all(dest.parent().unwrap())?;
        let tmp = self.tmp();
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&canonical)?;
            f.sync_all()?;
        }
        // Two clients publishing concurrently both write the same bytes, so
        // whichever rename lands last is still correct. A rename that loses to
        // a *different* record is caught by the comparison on the next read.
        match fs::rename(&tmp, &dest) {
            Ok(()) => Ok(Publish::Created),
            Err(_) if dest.is_file() => {
                let existing = fs::read(&dest).unwrap_or_default();
                let _ = fs::remove_file(&tmp);
                Ok(agreement(&existing, rec))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// `(objects, bytes, executions, tasks)`.
    pub fn stats(&self) -> Result<(usize, u64, usize, usize)> {
        let mut objects = 0;
        let mut bytes = 0;
        for shard in fs::read_dir(self.root.join("objects"))? {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue;
            }
            for f in fs::read_dir(shard.path())? {
                let f = f?;
                objects += 1;
                bytes += f.metadata()?.len();
            }
        }
        let count = |dir: PathBuf| -> Result<usize> {
            let mut n = 0;
            for ns in fs::read_dir(dir)? {
                let ns = ns?;
                if ns.file_type()?.is_dir() {
                    n += fs::read_dir(ns.path())?.count();
                }
            }
            Ok(n)
        };
        Ok((
            objects,
            bytes,
            count(self.root.join("executions"))?,
            count(self.root.join("tasks"))?,
        ))
    }
}

/// A stored record that cannot even be parsed is treated as a conflict rather
/// than overwritten: something wrote it, and guessing is worse than refusing.
fn agreement(existing: &[u8], incoming: &RemoteExecution) -> Publish {
    match serde_json::from_slice::<RemoteExecution>(existing) {
        Ok(stored) if stored.identity() == incoming.identity() => Publish::Identical,
        _ => Publish::Conflict,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_core::hash::hash_bytes;

    fn storage() -> (tempfile::TempDir, Storage) {
        let t = tempfile::tempdir().unwrap();
        let s = Storage::open(t.path()).unwrap();
        (t, s)
    }

    #[test]
    fn an_object_whose_bytes_do_not_match_its_digest_is_refused() {
        let (_t, s) = storage();
        let claimed = hash_bytes(b"good").hex();
        assert!(s.put_object(&claimed, &mut &b"evil"[..]).is_err());
        assert!(!s.has_object(&claimed));
        assert_eq!(fs::read_dir(s.root.join("tmp")).unwrap().count(), 0);

        s.put_object(&claimed, &mut &b"good"[..]).unwrap();
        assert!(s.has_object(&claimed));
    }

    #[test]
    fn republishing_the_same_record_is_idempotent_and_a_different_one_conflicts() {
        let (_t, s) = storage();
        let mut rec = RemoteExecution {
            protocol: protocol::PROTOCOL_VERSION,
            key_semantics: arc_core::SCHEMA_VERSION,
            execution_key: "a".repeat(64),
            os: "linux".into(),
            arch: "x86_64".into(),
            program: "echo".into(),
            args: vec![],
            rel_cwd: String::new(),
            family_key: "f".into(),
            exit_code: 0,
            duration_ms: 1,
            outputs: vec![],
            stdout: None,
            stderr: None,
            arc_version: "0.5.0".into(),
        };
        let key = rec.execution_key.clone();
        assert_eq!(s.put_record("ns", &key, &rec).unwrap(), Publish::Created);
        assert_eq!(s.put_record("ns", &key, &rec).unwrap(), Publish::Identical);
        rec.exit_code = 1;
        assert_eq!(s.put_record("ns", &key, &rec).unwrap(), Publish::Conflict);
        // The first record still stands.
        let stored: RemoteExecution =
            serde_json::from_slice(&s.get_record("ns", &key).unwrap()).unwrap();
        assert_eq!(stored.exit_code, 0);
        // And another namespace cannot see it at all.
        assert!(s.get_record("other", &key).is_none());
    }
}
