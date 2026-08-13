//! Environment, toolchain, and execution-key computation.

use crate::hash::{hash_bytes, hash_file, Digest, Hasher};
use crate::project::Config;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Variables that plausibly change the result of a build or test on any
/// ecosystem. Kept short on purpose: every extra name is a source of false
/// misses. Projects add their own with `[env] include`.
pub const DEFAULT_ENV: &[&str] = &[
    "PATH",
    "LANG",
    "LC_ALL",
    "CC",
    "CXX",
    "CFLAGS",
    "CXXFLAGS",
    "LDFLAGS",
    "RUSTFLAGS",
    "RUSTC_WRAPPER",
    "CARGO_BUILD_TARGET",
    "NODE_ENV",
    "NODE_OPTIONS",
    "GOFLAGS",
    "GOOS",
    "GOARCH",
    "PYTHONPATH",
    "PYTHONHASHSEED",
    "JAVA_HOME",
];

const SECRET_MARKERS: &[&str] = &[
    "SECRET",
    "TOKEN",
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
    "PRIVATE_KEY",
    "API_KEY",
    "AUTH",
];

pub fn looks_secret(name: &str) -> bool {
    let n = name.to_ascii_uppercase();
    SECRET_MARKERS.iter().any(|m| n.contains(m))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvVar {
    pub name: String,
    /// Hash of the value. Raw values are never persisted: any variable can hold
    /// a credential, and a cache record is not a safe place for one.
    pub value_digest: String,
    pub present: bool,
    pub redacted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvFingerprint {
    pub vars: Vec<EnvVar>,
    pub digest: String,
}

pub fn fingerprint_env(cfg: &Config) -> EnvFingerprint {
    let mut names: Vec<String> = DEFAULT_ENV.iter().map(|s| s.to_string()).collect();
    names.extend(cfg.env.include.iter().cloned());
    names.retain(|n| !cfg.env.exclude.iter().any(|e| e.eq_ignore_ascii_case(n)));
    names.sort();
    names.dedup();

    let mut h = Hasher::new();
    let mut vars = Vec::with_capacity(names.len());
    for name in names {
        let value = std::env::var(&name).ok();
        let digest = hash_bytes(value.as_deref().unwrap_or("").as_bytes());
        h.field(&name);
        h.field([value.is_some() as u8]);
        h.field(digest.bytes());
        vars.push(EnvVar {
            redacted: looks_secret(&name),
            present: value.is_some(),
            value_digest: digest.hex(),
            name,
        });
    }
    EnvFingerprint {
        digest: h.finish().hex(),
        vars,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Toolchain {
    pub program: String,
    pub resolved_path: Option<String>,
    pub digest: String,
}

/// Fingerprint the executable Arc is about to run by hashing its contents,
/// which is strictly stronger than trusting a `--version` string.
pub fn fingerprint_toolchain(program: &str, cwd: &Path) -> Result<Toolchain> {
    let resolved = which(program, cwd);
    let mut h = Hasher::new();
    h.field(program);
    let resolved_path = match &resolved {
        Some(p) => {
            h.field(
                p.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .as_bytes(),
            );
            match hash_file(p) {
                Ok(d) => {
                    h.field(d.bytes());
                }
                // Unreadable executable: fall back to its path so the key still
                // distinguishes different binaries.
                Err(_) => {
                    h.field(p.to_string_lossy().as_bytes());
                }
            }
            Some(p.to_string_lossy().to_string())
        }
        None => {
            h.field(b"<unresolved>");
            None
        }
    };
    Ok(Toolchain {
        program: program.to_string(),
        resolved_path,
        digest: h.finish().hex(),
    })
}

/// Resolve a program the way the OS will: relative/absolute paths directly,
/// bare names through PATH (with PATHEXT on Windows).
pub fn which(program: &str, cwd: &Path) -> Option<PathBuf> {
    let has_sep = program.contains('/') || program.contains('\\');
    if has_sep {
        let p = cwd.join(program);
        return with_extensions(&p).into_iter().find(|c| c.is_file());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|dir| with_extensions(&dir.join(program)))
        .find(|c| c.is_file())
}

fn with_extensions(base: &Path) -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let mut out = vec![base.to_path_buf()];
        let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".into());
        for ext in exts.split(';').filter(|e| !e.is_empty()) {
            let mut s = base.as_os_str().to_os_string();
            s.push(ext.to_ascii_lowercase());
            out.push(PathBuf::from(s));
        }
        out
    }
    #[cfg(not(windows))]
    {
        vec![base.to_path_buf()]
    }
}

/// Everything that may change the result of an execution, in one digest.
#[derive(Debug, Clone)]
pub struct KeyInputs<'a> {
    pub program: &'a str,
    pub args: &'a [String],
    /// Working directory relative to the project root, so a cache stays valid
    /// when the same repository lives at a different absolute path.
    pub rel_cwd: &'a str,
    /// Identity of the execution kind. Included so two families can never share
    /// a cache entry even if every other component coincides.
    pub family_key: &'a str,
    pub input_digest: &'a Digest,
    pub env_digest: &'a str,
    pub toolchain_digest: &'a str,
    /// Learned dependencies that participate in the key — today the executables
    /// observed in the process tree. Strictly additive: covering more can only
    /// cause misses.
    pub dependency_digest: &'a Digest,
    pub output_globs: &'a [String],
}

pub fn execution_key(k: &KeyInputs<'_>) -> Digest {
    let mut h = Hasher::new();
    h.field(crate::SCHEMA_VERSION.to_le_bytes());
    h.field(std::env::consts::OS);
    h.field(std::env::consts::ARCH);
    h.field(k.program);
    h.field((k.args.len() as u64).to_le_bytes());
    for a in k.args {
        h.field(a);
    }
    h.field(k.rel_cwd);
    h.field(k.family_key);
    h.field(k.input_digest.bytes());
    h.field(k.env_digest);
    h.field(k.toolchain_digest);
    h.field(k.dependency_digest.bytes());
    h.field((k.output_globs.len() as u64).to_le_bytes());
    for g in k.output_globs {
        h.field(g);
    }
    h.finish()
}

pub fn rel_cwd(root: &Path, cwd: &Path) -> String {
    cwd.strip_prefix(root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| cwd.to_string_lossy().replace('\\', "/"))
}

pub fn find_program_or_explain(program: &str, cwd: &Path) -> Result<PathBuf> {
    which(program, cwd).with_context(|| {
        let path = std::env::var("PATH").unwrap_or_default();
        let entries: Vec<String> =
            std::env::split_paths(&path).map(|p| format!("  {}", p.display())).collect();
        format!(
            "Arc could not execute `{program}`.\n\nThe executable was not found in PATH.\n\nPATH:\n{}\n\nTry:\n  {program} --version",
            entries.join("\n")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::hash_bytes;

    fn key_with(args: &[&str], input: &Digest, deps: &Digest, family: &str) -> Digest {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        execution_key(&KeyInputs {
            program: "cargo",
            args: &args,
            rel_cwd: "",
            family_key: family,
            input_digest: input,
            env_digest: "e",
            toolchain_digest: "t",
            dependency_digest: deps,
            output_globs: &[],
        })
    }

    fn key(args: &[&str], input: &Digest) -> Digest {
        key_with(args, input, &Digest::default(), "f")
    }

    #[test]
    fn key_covers_args_and_inputs() {
        let i1 = hash_bytes(b"1");
        let i2 = hash_bytes(b"2");
        assert_eq!(key(&["test"], &i1), key(&["test"], &i1));
        assert_ne!(key(&["test"], &i1), key(&["build"], &i1));
        assert_ne!(key(&["test"], &i1), key(&["test"], &i2));
        // Argument boundaries must matter.
        assert_ne!(key(&["a b"], &i1), key(&["a", "b"], &i1));
    }

    #[test]
    fn key_covers_family_and_learned_dependencies() {
        let i = hash_bytes(b"1");
        let d1 = hash_bytes(b"deps-1");
        let d2 = hash_bytes(b"deps-2");
        assert_ne!(
            key_with(&["test"], &i, &d1, "f"),
            key_with(&["test"], &i, &d2, "f")
        );
        assert_ne!(
            key_with(&["test"], &i, &d1, "f1"),
            key_with(&["test"], &i, &d1, "f2")
        );
    }

    #[test]
    fn secret_names_are_flagged() {
        assert!(looks_secret("AWS_SECRET_ACCESS_KEY"));
        assert!(looks_secret("gh_token"));
        assert!(!looks_secret("PATH"));
    }
}
