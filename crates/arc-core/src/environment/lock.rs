//! `arc-env.lock`: which environment an alias currently means.
//!
//! Aliases are mutable human configuration and are never part of a cache key.
//! The lock file exists so a project can pin the *id* — the thing that is part
//! of a cache key — and commit it, which is what makes a CI run reproducible.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const LOCK_NAME: &str = "arc-env.lock";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Lock {
    /// alias -> EnvironmentId. A `BTreeMap` because the file is committed and a
    /// diff should show what changed, not that a hash map was re-ordered.
    pub environments: BTreeMap<String, String>,
}

impl Lock {
    pub fn path(root: &Path) -> PathBuf {
        root.join(LOCK_NAME)
    }

    pub fn load(root: &Path) -> Result<Lock> {
        let p = Lock::path(root);
        if !p.is_file() {
            return Ok(Lock::default());
        }
        let text =
            std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
        let lock: Lock = toml::from_str(&text).map_err(|e| {
            anyhow::anyhow!(
                "{} is not a valid Arc environment lock.\n\n{e}",
                p.display()
            )
        })?;
        for (alias, id) in &lock.environments {
            if !super::materialise::valid_id(id) {
                bail!(
                    "{}: `{alias}` is not pinned to an environment id",
                    p.display()
                );
            }
        }
        Ok(lock)
    }

    pub fn get(&self, alias: &str) -> Option<&str> {
        self.environments.get(alias).map(String::as_str)
    }

    pub fn set(&mut self, alias: &str, id: &str) -> Result<()> {
        if !super::materialise::valid_id(id) {
            bail!("`{id}` is not an environment id");
        }
        self.environments.insert(alias.to_string(), id.to_string());
        Ok(())
    }

    pub fn save(&self, root: &Path) -> Result<PathBuf> {
        let p = Lock::path(root);
        let body = toml::to_string_pretty(self)?;
        let text = format!(
            "# Arc environment lock. Aliases are names; the identity is the id.\n# Commit this file to pin the environments CI runs in.\n{body}"
        );
        let tmp = p.with_extension("lock-tmp");
        std::fs::write(&tmp, text.as_bytes())?;
        std::fs::rename(&tmp, &p).with_context(|| format!("writing {}", p.display()))?;
        Ok(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lock_round_trips_deterministically() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        lock.set("rust", &"a".repeat(64)).unwrap();
        lock.set("node", &"b".repeat(64)).unwrap();
        lock.save(tmp.path()).unwrap();
        let first = std::fs::read_to_string(Lock::path(tmp.path())).unwrap();

        let read = Lock::load(tmp.path()).unwrap();
        assert_eq!(read.get("rust"), Some("a".repeat(64).as_str()));
        read.save(tmp.path()).unwrap();
        assert_eq!(
            first,
            std::fs::read_to_string(Lock::path(tmp.path())).unwrap()
        );
        // Sorted, so a merge conflict is about content rather than ordering.
        assert!(first.find("node").unwrap() < first.find("rust").unwrap());
    }

    #[test]
    fn a_missing_lock_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(Lock::load(tmp.path()).unwrap().environments.is_empty());
    }

    #[test]
    fn an_alias_pinned_to_something_that_is_not_an_id_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            Lock::path(tmp.path()),
            "[environments]\nrust = \"stable\"\n",
        )
        .unwrap();
        assert!(Lock::load(tmp.path()).is_err());

        let mut lock = Lock::default();
        assert!(lock.set("rust", "stable").is_err());
    }

    #[test]
    fn no_machine_local_path_can_reach_the_lock() {
        // The only value shape the file accepts is a digest.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            Lock::path(tmp.path()),
            "[environments]\nrust = \"/home/dev/.rustup\"\n",
        )
        .unwrap();
        assert!(Lock::load(tmp.path()).is_err());
    }
}
