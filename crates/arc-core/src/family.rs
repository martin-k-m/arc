//! Execution *identity*, as opposed to execution *state*.
//!
//! `cargo test` in this project is one **family**. The current contents of
//! `src/lib.rs` are state within that family. Arc needs both, and they answer
//! different questions:
//!
//! * [`FamilyKey`] — "have I seen this kind of execution here before?" It must
//!   stay stable when files change, or Arc could never find the dependency
//!   knowledge it learned on the previous run.
//! * `execution_key` (see [`crate::key`]) — "may I reuse *that* result?" It
//!   covers everything that could change the outcome, contents included.
//!
//! The family key therefore deliberately excludes file contents. It is never
//! used to authorise a cache hit; it only locates knowledge.

use crate::hash::{Digest, Hasher};
use crate::project::Config;
use serde::{Deserialize, Serialize};

/// Bumped when the family key's inputs change meaning. Old families then stop
/// matching instead of being misread.
pub const FAMILY_KEY_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionFamily {
    pub key: String,
    pub program: String,
    pub args: Vec<String>,
    pub project_root: String,
    pub rel_cwd: String,
    pub first_seen: i64,
    pub last_seen: i64,
    pub runs: u64,
}

impl ExecutionFamily {
    pub fn command_line(&self) -> String {
        crate::record::format_command(&self.program, &self.args)
    }
}

/// Configuration that changes *which* inputs an execution has, as opposed to
/// configuration that only changes cache policy.
///
/// A change here must invalidate learned dependencies, because the dependency
/// set was derived under the old rules. `cache.enabled` and `max_size` are
/// excluded on purpose: they alter whether Arc caches, never what an execution
/// depends on.
fn scoping_digest(cfg: &Config) -> Digest {
    let mut h = Hasher::new();
    let list = |h: &mut Hasher, v: &[String]| {
        h.field((v.len() as u64).to_le_bytes());
        for s in v {
            h.field(s);
        }
    };
    list(&mut h, &cfg.inputs.include);
    list(&mut h, &cfg.inputs.exclude);
    list(&mut h, &cfg.outputs.include);
    list(&mut h, &cfg.env.include);
    list(&mut h, &cfg.env.exclude);
    h.field([cfg.trace.enabled as u8]);
    h.field((cfg.commands.len() as u64).to_le_bytes());
    for c in &cfg.commands {
        h.field(&c.match_);
        list(&mut h, &c.inputs);
        list(&mut h, &c.exclude);
        list(&mut h, &c.outputs);
        list(&mut h, &c.env);
    }
    h.finish()
}

/// Identity of an execution kind within one project.
pub fn family_key(program: &str, args: &[String], rel_cwd: &str, cfg: &Config) -> Digest {
    let mut h = Hasher::new();
    h.field(FAMILY_KEY_VERSION.to_le_bytes());
    h.field(std::env::consts::OS);
    h.field(std::env::consts::ARCH);
    h.field(program);
    h.field((args.len() as u64).to_le_bytes());
    for a in args {
        h.field(a);
    }
    h.field(rel_cwd);
    h.field(scoping_digest(cfg).bytes());
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn identity_is_stable_across_content_changes_but_not_across_commands() {
        let cfg = Config::default();
        let a = family_key("cargo", &args(&["test"]), "", &cfg);
        assert_eq!(a, family_key("cargo", &args(&["test"]), "", &cfg));
        assert_ne!(a, family_key("cargo", &args(&["build"]), "", &cfg));
        assert_ne!(a, family_key("cargo", &args(&["test"]), "sub", &cfg));
        // Argument boundaries must matter here too.
        assert_ne!(a, family_key("cargo", &args(&["te", "st"]), "", &cfg));
    }

    #[test]
    fn scoping_config_changes_identity_but_cache_policy_does_not() {
        let base = Config::default();
        let key = family_key("cargo", &args(&["test"]), "", &base);

        let mut policy = Config::default();
        policy.cache.max_size = "1GB".into();
        policy.cache.enabled = false;
        assert_eq!(key, family_key("cargo", &args(&["test"]), "", &policy));

        let mut scoping = Config::default();
        scoping.inputs.include = vec!["src/**".into()];
        assert_ne!(key, family_key("cargo", &args(&["test"]), "", &scoping));
    }
}
