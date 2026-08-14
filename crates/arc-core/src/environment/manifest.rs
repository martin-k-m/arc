//! What an execution environment *is*, and what its identity covers.
//!
//! Identity invariant: `EnvironmentId` is the digest of this manifest's
//! canonical bytes, and the manifest names every file by content. So the id is
//! a CAS object digest — publishing an environment is publishing blobs, and
//! verifying one is re-hashing what arrived. There is no separate registry to
//! trust, and no name anywhere in the identity.

use crate::hash::hash_bytes;
use crate::remote::protocol::{valid_digest, validate_rel_path, WirePath};
use serde::{Deserialize, Serialize};

/// Bumped when the meaning of any manifest field changes. Part of the canonical
/// bytes, so an environment captured under different semantics has a different
/// id rather than a subtly different meaning.
pub const ENV_SCHEMA_VERSION: u32 = 1;

pub const MAX_ENV_FILES: usize = 200_000;
pub const MAX_ENV_VARS: usize = 512;

/// A file the environment materialises. `link` makes it a relative symlink
/// instead, which is how toolchains that share a binary between two names stay
/// the size they were captured at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvFile {
    pub path: WirePath,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub digest: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub exec: bool,
    /// Relative symlink target. Mutually exclusive with `digest`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

/// A program the environment promises to provide, by the name a command will
/// invoke it as.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvTool {
    pub name: String,
    /// Where it lives inside the environment root.
    pub rel: String,
    pub digest: String,
}

/// What the environment cannot supply and the host must.
///
/// Kept separate from the environment itself because it is a different kind of
/// claim: the environment *is* these bytes, whereas this is a statement about
/// machines that can run them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostRequirements {
    pub os: String,
    pub arch: String,
    /// `gnu`, `musl`, or `n/a` off Linux. A userspace captured against one is
    /// not loadable against the other.
    pub libc: String,
    /// Absolute `PT_INTERP` paths the captured binaries were linked against.
    /// Arc does not relocate the dynamic loader; it requires one.
    pub interpreters: Vec<String>,
    /// Sonames that resolved to system library directories at capture time and
    /// are therefore expected to exist on the host too.
    pub libraries: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Completeness {
    /// Every declared tool resolved and every library it names was either
    /// captured or classified as a host requirement.
    Complete,
    /// Something a captured binary asks for could not be accounted for. Says
    /// nothing about whether a given command would work; see `hermeticity` in
    /// `docs/correctness.md`.
    Partial,
}

impl Completeness {
    pub fn label(&self) -> &'static str {
        match self {
            Completeness::Complete => "complete",
            Completeness::Partial => "partial",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentManifest {
    pub schema_version: u32,
    pub os: String,
    pub arch: String,
    pub tools: Vec<EnvTool>,
    pub files: Vec<EnvFile>,
    /// Variables every command in this environment gets. Never secrets: capture
    /// refuses secret-shaped names, and a manifest travels to other machines.
    pub env: Vec<(String, String)>,
    /// Directories inside the environment root that form `PATH`, in order.
    pub path_entries: Vec<String>,
    /// Directories inside the environment root that form `LD_LIBRARY_PATH`.
    pub library_path: Vec<String>,
    pub host: HostRequirements,
    pub completeness: Completeness,
    /// What made it `Partial`. Part of identity: two environments that captured
    /// different amounts of the same toolchain are different environments.
    pub gaps: Vec<String>,
}

impl EnvironmentManifest {
    /// Sort everything that has no meaningful order, so two captures of the
    /// same content on two machines produce the same bytes and the same id.
    pub fn canonicalise(&mut self) {
        self.tools.sort_by(|a, b| a.name.cmp(&b.name));
        self.tools.dedup_by(|a, b| a.name == b.name);
        self.files.sort_by(|a, b| a.path.v.cmp(&b.path.v));
        self.files.dedup_by(|a, b| a.path.v == b.path.v);
        self.env.sort();
        self.env.dedup();
        self.host.interpreters.sort();
        self.host.interpreters.dedup();
        self.host.libraries.sort();
        self.host.libraries.dedup();
        self.gaps.sort();
        self.gaps.dedup();
        self.path_entries.dedup();
        self.library_path.dedup();
    }

    /// The bytes that *are* the environment's identity.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut c = self.clone();
        c.canonicalise();
        serde_json::to_vec(&c).expect("manifest is serialisable")
    }

    pub fn id(&self) -> String {
        hash_bytes(&self.canonical_bytes()).hex()
    }

    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    /// Everything the environment needs from a CAS before it can materialise.
    pub fn digests(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .files
            .iter()
            .filter(|f| f.link.is_none())
            .map(|f| f.digest.clone())
            .collect();
        v.sort();
        v.dedup();
        v
    }

    /// Reject anything that could not have come from a capture, before a single
    /// byte is written to disk. A manifest is data from an untrusted server.
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != ENV_SCHEMA_VERSION {
            return Err(format!(
                "environment manifest schema v{} (this Arc speaks v{ENV_SCHEMA_VERSION})",
                self.schema_version
            ));
        }
        if self.files.len() > MAX_ENV_FILES {
            return Err(format!(
                "{} environment files is too many",
                self.files.len()
            ));
        }
        if self.env.len() > MAX_ENV_VARS {
            return Err("too many environment variables".into());
        }
        let mut seen = std::collections::HashSet::with_capacity(self.files.len());
        for f in &self.files {
            let rel = f.path.decode()?;
            if !seen.insert(rel.clone()) {
                return Err(format!("duplicate environment entry `{rel}`"));
            }
            match &f.link {
                Some(target) => {
                    if !f.digest.is_empty() {
                        return Err(format!("`{rel}` is both a symlink and a file"));
                    }
                    validate_link(&rel, target)?;
                }
                None => {
                    if !valid_digest(&f.digest) {
                        return Err(format!("malformed digest for `{rel}`"));
                    }
                }
            }
        }
        for t in &self.tools {
            if t.name.is_empty() || t.name.contains('\0') {
                return Err("malformed tool name".into());
            }
            validate_rel_path(&t.rel)?;
            if !valid_digest(&t.digest) {
                return Err(format!("malformed digest for tool `{}`", t.name));
            }
            if !seen.contains(&t.rel) {
                return Err(format!("tool `{}` is not among the files", t.name));
            }
        }
        for d in self.path_entries.iter().chain(&self.library_path) {
            validate_rel_path(d)?;
        }
        for (k, v) in &self.env {
            if k.is_empty() || k.contains('\0') || k.contains('=') || v.contains('\0') {
                return Err(format!("malformed environment variable `{k}`"));
            }
            if crate::key::looks_secret(k) {
                return Err(format!("`{k}` looks like a secret"));
            }
        }
        for p in &self.host.interpreters {
            if p.contains('\0') || p.len() > 4096 {
                return Err("malformed interpreter path".into());
            }
        }
        Ok(())
    }

    pub fn tool(&self, name: &str) -> Option<&EnvTool> {
        self.tools.iter().find(|t| t.name == name)
    }
}

/// A symlink may point anywhere *inside* the environment and nowhere outside
/// it. Absolute targets are host paths by definition, so they are refused.
fn validate_link(from: &str, target: &str) -> Result<(), String> {
    if target.is_empty() || target.len() > 4096 || target.contains('\0') {
        return Err(format!("malformed symlink target for `{from}`"));
    }
    if target.starts_with('/') || target.starts_with('\\') || target.contains(':') {
        return Err(format!("`{from}` points outside the environment"));
    }
    let mut depth: i64 = from.matches('/').count() as i64;
    for part in target.split('/') {
        match part {
            ".." => depth -= 1,
            "." | "" => {}
            _ => depth += 1,
        }
        if depth < 0 {
            return Err(format!("`{from}` points outside the environment"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> EnvironmentManifest {
        EnvironmentManifest {
            schema_version: ENV_SCHEMA_VERSION,
            os: "linux".into(),
            arch: "x86_64".into(),
            tools: vec![EnvTool {
                name: "cc".into(),
                rel: "bin/cc".into(),
                digest: "a".repeat(64),
            }],
            files: vec![EnvFile {
                path: WirePath::from_rel("bin/cc"),
                digest: "a".repeat(64),
                size: 10,
                exec: true,
                link: None,
            }],
            env: vec![("LANG".into(), "C.UTF-8".into())],
            path_entries: vec!["bin".into()],
            library_path: vec!["lib".into()],
            host: HostRequirements {
                os: "linux".into(),
                arch: "x86_64".into(),
                libc: "gnu".into(),
                interpreters: vec!["/lib64/ld-linux-x86-64.so.2".into()],
                libraries: vec!["libc.so.6".into()],
            },
            completeness: Completeness::Complete,
            gaps: Vec::new(),
        }
    }

    #[test]
    fn identity_is_content_and_ordering_is_not() {
        let a = manifest();
        let mut b = manifest();
        b.files.push(EnvFile {
            path: WirePath::from_rel("lib/libx.so"),
            digest: "b".repeat(64),
            size: 1,
            exec: false,
            link: None,
        });
        let mut c = b.clone();
        c.files.reverse();
        c.host.libraries.push("libc.so.6".into());

        assert_eq!(
            b.id(),
            c.id(),
            "order and duplicates must not change identity"
        );
        assert_ne!(a.id(), b.id(), "an extra file must change identity");
    }

    #[test]
    fn one_changed_byte_changes_identity() {
        let a = manifest();
        let mut b = manifest();
        b.files[0].digest = "c".repeat(64);
        assert_ne!(a.id(), b.id());
    }

    #[test]
    fn a_name_is_not_part_of_identity() {
        // There is nowhere in the manifest to put one, which is the point.
        let json = serde_json::to_string(&manifest()).unwrap();
        assert!(!json.contains("\"name\":\"rust\""));
    }

    #[test]
    fn a_manifest_that_escapes_is_refused() {
        for bad in ["../etc/passwd", "/etc/passwd"] {
            let mut m = manifest();
            m.files[0].path = WirePath::from_rel(bad);
            m.tools[0].rel = bad.into();
            assert!(m.validate().is_err(), "{bad} should be refused");
        }

        let mut m = manifest();
        m.files.push(EnvFile {
            path: WirePath::from_rel("bin/sh"),
            digest: String::new(),
            size: 0,
            exec: false,
            link: Some("../../../../bin/sh".into()),
        });
        assert!(m.validate().is_err());

        let mut m = manifest();
        m.files.push(EnvFile {
            path: WirePath::from_rel("bin/sh"),
            digest: String::new(),
            size: 0,
            exec: false,
            link: Some("/bin/sh".into()),
        });
        assert!(m.validate().is_err());
    }

    #[test]
    fn a_relative_symlink_inside_the_environment_is_allowed() {
        let mut m = manifest();
        m.files.push(EnvFile {
            path: WirePath::from_rel("bin/gcc"),
            digest: String::new(),
            size: 0,
            exec: false,
            link: Some("cc".into()),
        });
        m.files.push(EnvFile {
            path: WirePath::from_rel("lib/x/libz.so"),
            digest: String::new(),
            size: 0,
            exec: false,
            link: Some("../libz.so.1".into()),
        });
        m.validate().unwrap();
    }

    #[test]
    fn duplicates_malformed_digests_and_oversize_manifests_are_refused() {
        let mut m = manifest();
        m.files.push(m.files[0].clone());
        assert!(m.validate().is_err());

        let mut m = manifest();
        m.files[0].digest = "nope".into();
        assert!(m.validate().is_err());

        let mut m = manifest();
        m.schema_version += 1;
        assert!(m.validate().is_err());

        let mut m = manifest();
        m.env = (0..MAX_ENV_VARS + 1)
            .map(|i| (format!("V{i}"), String::new()))
            .collect();
        assert!(m.validate().is_err());
    }

    #[test]
    fn a_secret_shaped_variable_never_survives_into_a_manifest() {
        let mut m = manifest();
        m.env.push(("GITHUB_TOKEN".into(), "ghp_x".into()));
        assert!(m.validate().unwrap_err().contains("GITHUB_TOKEN"));
    }

    #[test]
    fn a_tool_must_be_one_of_the_files() {
        let mut m = manifest();
        m.tools[0].rel = "bin/other".into();
        assert!(m.validate().is_err());
    }
}
