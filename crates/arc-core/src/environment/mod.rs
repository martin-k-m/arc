//! Execution environments: a content-addressed answer to "what does this
//! command run inside".
//!
//! ```text
//! [environment.rust]        arc env capture rust
//!   tools / trees      ─────────────────▶  capture
//!                                             │
//!                                             ▼
//!                                    EnvironmentManifest
//!                                             │  hash of canonical bytes
//!                                             ▼
//!                                       EnvironmentId ───▶ arc-env.lock
//!                                             │
//!                                        Arc CAS (blobs)
//!                                             │
//!                                  ┌──────────┴──────────┐
//!                                  ▼                     ▼
//!                          local materialiser     worker materialiser
//!                                  └──────────┬──────────┘
//!                                             ▼
//!                                    deterministic PATH,
//!                                   isolated HOME and TMP
//! ```
//!
//! The manifest's canonical bytes *are* a CAS object, and their digest *is* the
//! environment id. So transporting an environment needs no new service, and
//! verifying one is the object verification Arc already does everywhere else.

pub mod capture;
pub mod elf;
pub mod host;
pub mod lock;
pub mod manifest;
pub mod materialise;

pub use host::{host_capability, HostCapability};
pub use manifest::{Completeness, EnvironmentManifest};
pub use materialise::{Environments, Materialised};

use crate::hash::Digest;
use crate::store::Store;
use anyhow::{bail, Context, Result};

/// Put an environment into a store and return its id.
pub fn store_manifest(m: &EnvironmentManifest, store: &Store) -> Result<String> {
    Ok(store.put_bytes(&m.canonical_bytes())?.hex())
}

/// Read an environment back out, believing nothing.
///
/// The bytes are re-hashed against the requested id, so a store or server that
/// serves different content for an id fails here rather than producing a
/// toolchain nobody asked for.
pub fn load_manifest(id: &str, store: &Store) -> Result<EnvironmentManifest> {
    if !materialise::valid_id(id) {
        bail!("`{id}` is not an environment id");
    }
    let d = Digest::parse(id)?;
    if !store.exists(&d) {
        bail!("environment {} is not in this store", &id[..12]);
    }
    let bytes = store.read(&d)?;
    let actual = crate::hash::hash_bytes(&bytes);
    if actual != d {
        bail!(
            "environment {} does not match its digest (got {})",
            &id[..12],
            actual.short()
        );
    }
    let m: EnvironmentManifest = serde_json::from_slice(&bytes)
        .map_err(|e| anyhow::anyhow!("environment {} is malformed: {e}", &id[..12]))?;
    m.validate().map_err(|e| anyhow::anyhow!(e))?;
    // Round-tripping proves the bytes were canonical. Without it two different
    // serialisations of the same environment would be two different ids, and
    // the id would stop being a function of the environment.
    if m.id() != id {
        bail!("environment {} is not in canonical form", &id[..12]);
    }
    Ok(m)
}

/// What Arc can claim about *one execution*, which is a different question from
/// whether the environment is structurally complete.
///
/// An environment can be complete and a command run inside it still read
/// `/etc/whatever`. Completeness is a property of the environment; hermeticity
/// is a property of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hermeticity {
    /// Everything the command touched outside the project came from the
    /// environment or from a surface Arc already treats as volatile.
    Hermetic,
    /// It read host state the environment does not supply. The result is
    /// correct here and is not portable.
    HostDependent,
    /// Arc could not see the whole execution, so it claims nothing.
    Unknown,
}

impl Hermeticity {
    pub fn label(&self) -> &'static str {
        match self {
            Hermeticity::Hermetic => "hermetic",
            Hermeticity::HostDependent => "host-dependent",
            Hermeticity::Unknown => "unknown",
        }
    }
}

/// Kernel and process interfaces, which no environment can package and every
/// traced execution touches. Already treated as volatile by the tracer; named
/// again here so hermeticity and narrowing agree about what "host state" means.
const KERNEL_SURFACES: &[&str] = &["/proc/", "/sys/", "/dev/"];

/// Judge one execution against the environment it claimed to run in.
pub fn hermeticity(
    deps: &crate::dependency::DependencySet,
    complete: bool,
    env_root: &std::path::Path,
    host: &manifest::HostRequirements,
) -> (Hermeticity, Vec<String>) {
    if !complete {
        return (Hermeticity::Unknown, Vec::new());
    }
    let root = env_root.to_string_lossy().replace('\\', "/");
    let expected = |p: &str| -> bool {
        let n = p.replace('\\', "/");
        n.starts_with(&root)
            || KERNEL_SURFACES.iter().any(|k| n.starts_with(k))
            || host.interpreters.iter().any(|i| i == &n)
            || host::is_system_library(std::path::Path::new(p))
    };
    let mut leaks: Vec<String> = deps
        .external
        .iter()
        .map(|e| e.path.clone())
        .chain(deps.external_directories.iter().cloned())
        .chain(deps.executables.iter().map(|e| e.path.clone()))
        .filter(|p| !expected(p))
        .collect();
    leaks.sort();
    leaks.dedup();
    if leaks.is_empty() {
        (Hermeticity::Hermetic, leaks)
    } else {
        (Hermeticity::HostDependent, leaks)
    }
}

/// An environment chosen for one command, materialised and ready to run in.
pub struct Selected {
    pub alias: String,
    pub id: String,
    pub manifest: EnvironmentManifest,
    pub materialised: Materialised,
}

impl Selected {
    pub fn child_env(
        &self,
        home: &std::path::Path,
        tmp: &std::path::Path,
    ) -> crate::exec::ChildEnv {
        self.materialised.child_env(home, tmp)
    }
}

/// Resolve an alias to a materialised environment.
///
/// The alias is looked up in the lock file, so what runs is whatever the
/// project pinned — never "whatever is installed here now". A manifest or an
/// object this machine lacks is fetched from the shared cache and verified on
/// the way in, which is the only reason a `remote` is wanted here.
pub fn select(
    project_root: &std::path::Path,
    arc_home: &std::path::Path,
    alias: &str,
    store: &Store,
    remote: Option<&crate::remote::Remote>,
) -> Result<Selected> {
    let lock = lock::Lock::load(project_root)?;
    let Some(id) = lock.get(alias) else {
        bail!(
            "environment `{alias}` is not captured.\n\nRun:\n  arc env capture {alias}\n\nand commit {}.",
            lock::LOCK_NAME
        );
    };
    let id = id.to_string();

    if !store.exists(&Digest::parse(&id)?) {
        let r = remote.filter(|r| r.read).with_context(|| {
            format!(
                "environment {} is not on this machine and no remote cache is readable",
                &id[..12]
            )
        })?;
        r.download(store, std::slice::from_ref(&id))?;
    }
    let manifest = load_manifest(&id, store)?;
    host::supports(&host_capability(), &manifest.host).map_err(|e| anyhow::anyhow!(e))?;

    let envs = Environments::open(arc_home)?;
    if !envs.ready(&id) {
        let missing: Vec<String> = manifest
            .digests()
            .into_iter()
            .filter(|d| Digest::parse(d).map(|p| !store.exists(&p)).unwrap_or(true))
            .collect();
        if !missing.is_empty() {
            let r = remote.filter(|r| r.read).with_context(|| {
                format!(
                    "environment {} needs {} objects this machine does not have, and no remote cache is readable",
                    &id[..12],
                    missing.len()
                )
            })?;
            r.download(store, &missing)?;
        }
    }
    let materialised = envs.materialise(&manifest, store)?;
    Ok(Selected {
        alias: alias.to_string(),
        id,
        manifest,
        materialised,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::protocol::WirePath;

    fn manifest() -> EnvironmentManifest {
        let mut m = EnvironmentManifest {
            schema_version: manifest::ENV_SCHEMA_VERSION,
            os: "linux".into(),
            arch: "x86_64".into(),
            tools: Vec::new(),
            files: vec![manifest::EnvFile {
                path: WirePath::from_rel("share/x"),
                digest: "a".repeat(64),
                size: 1,
                exec: false,
                link: None,
            }],
            env: Vec::new(),
            path_entries: vec!["bin".into()],
            library_path: Vec::new(),
            host: manifest::HostRequirements::default(),
            completeness: Completeness::Complete,
            gaps: Vec::new(),
        };
        m.canonicalise();
        m
    }

    #[test]
    fn a_stored_manifest_round_trips_under_its_own_id() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let m = manifest();
        let id = store_manifest(&m, &store).unwrap();
        assert_eq!(id, m.id());
        assert_eq!(load_manifest(&id, &store).unwrap(), m);
    }

    #[test]
    fn a_tampered_manifest_is_refused_rather_than_used() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let id = store_manifest(&manifest(), &store).unwrap();

        let mut evil = manifest();
        evil.files[0].digest = "b".repeat(64);
        std::fs::write(
            store.blob_path(&Digest::parse(&id).unwrap()),
            evil.canonical_bytes(),
        )
        .unwrap();

        let e = load_manifest(&id, &store).unwrap_err().to_string();
        assert!(e.contains("does not match its digest"), "{e}");
    }

    #[test]
    fn a_non_canonical_encoding_of_the_right_content_is_still_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        // Same fields, different byte order: a valid JSON encoding that is not
        // the one the id was derived from.
        let bytes = br#"{"os":"linux","schema_version":1,"arch":"x86_64","tools":[],"files":[],"env":[],"path_entries":[],"library_path":[],"host":{"os":"","arch":"","libc":"","interpreters":[],"libraries":[]},"completeness":"complete","gaps":[]}"#;
        let id = store.put_bytes(bytes).unwrap().hex();
        let e = load_manifest(&id, &store).unwrap_err().to_string();
        assert!(e.contains("canonical"), "{e}");
    }

    #[test]
    fn a_malformed_id_never_reaches_the_store() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        for bad in ["../../etc/passwd", "", "zz", &"g".repeat(64)] {
            assert!(load_manifest(bad, &store).is_err());
        }
    }
}
