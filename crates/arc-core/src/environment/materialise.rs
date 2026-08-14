//! Turning a manifest into a directory, once.
//!
//! Two invariants hold this together. An environment directory is *ready* only
//! when a sibling marker exists, which is written last — so a crash mid-build
//! leaves something removable, never something usable. And a ready environment
//! is immutable: files are materialised read-only and every job reads the same
//! copy, because copying a toolchain per execution would cost more than the
//! executions save.

use super::manifest::EnvironmentManifest;
use crate::hash::Digest;
use crate::store::Store;
use anyhow::{bail, Context, Result};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// Held for as long as an environment is in use. Collection skips anything with
/// a live lease; nothing more elaborate is needed, because an environment is
/// only ever used by the process that materialised it.
#[derive(Clone)]
pub struct Lease(Arc<Held>);

struct Held(String);

impl Drop for Held {
    fn drop(&mut self) {
        let map = live();
        let mut m = map.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = m.get_mut(&self.0) {
            *n -= 1;
            if *n == 0 {
                m.remove(&self.0);
            }
        }
    }
}

impl Lease {
    fn take(id: &str) -> Lease {
        let map = live();
        let mut m = map.lock().unwrap_or_else(|e| e.into_inner());
        *m.entry(id.to_string()).or_insert(0) += 1;
        drop(m);
        Lease(Arc::new(Held(id.to_string())))
    }

    pub fn id(&self) -> &str {
        &self.0 .0
    }
}

pub struct Materialised {
    pub id: String,
    pub root: PathBuf,
    pub path_entries: Vec<PathBuf>,
    pub library_path: Vec<PathBuf>,
    pub env: Vec<(String, String)>,
    pub tools: Vec<(String, PathBuf)>,
    /// Bytes and files written by *this* call. Zero when the environment was
    /// already present, which is the case that has to be fast.
    pub written_bytes: u64,
    pub written_files: usize,
    pub reused: bool,
    lease: Lease,
}

impl Materialised {
    /// The `PATH` a command in this environment sees: environment directories
    /// and nothing else. A host `PATH` entry here would make the environment a
    /// suggestion rather than a definition.
    pub fn path_value(&self) -> std::ffi::OsString {
        std::env::join_paths(self.path_entries.iter()).unwrap_or_default()
    }

    pub fn library_path_value(&self) -> std::ffi::OsString {
        std::env::join_paths(self.library_path.iter()).unwrap_or_default()
    }

    /// Resolve a program *within* the environment. `None` means the environment
    /// does not provide it, which is a refusal, never a reason to look at the
    /// host.
    pub fn resolve(&self, program: &str) -> Option<PathBuf> {
        if let Some((_, p)) = self.tools.iter().find(|(n, _)| n == program) {
            return Some(p.clone());
        }
        let base = Path::new(program).file_name()?;
        self.path_entries
            .iter()
            .map(|d| d.join(base))
            .find(|p| p.is_file())
    }

    pub fn lease(&self) -> Lease {
        self.lease.clone()
    }

    /// The complete environment a command in here gets: the manifest's own
    /// variables, a `PATH` built only from the environment, and user state
    /// pointed at directories Arc owns.
    ///
    /// `HOME` and the caches under it are isolated because a compiler that
    /// reads `~/.cargo/config.toml` from whichever machine it landed on is not
    /// running in the environment Arc says it is.
    pub fn child_env(&self, home: &Path, tmp: &Path) -> crate::exec::ChildEnv {
        let s = |p: &Path| p.to_string_lossy().to_string();
        let mut vars: Vec<(String, String)> = self.env.clone();
        vars.push(("PATH".into(), self.path_value().to_string_lossy().into()));
        if !self.library_path.is_empty() {
            vars.push((
                "LD_LIBRARY_PATH".into(),
                self.library_path_value().to_string_lossy().into(),
            ));
        }
        vars.push(("HOME".into(), s(home)));
        for k in ["TMPDIR", "TEMP", "TMP"] {
            vars.push((k.into(), s(tmp)));
        }
        for (k, sub) in [
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_STATE_HOME", "state"),
        ] {
            vars.push((k.into(), s(&home.join(sub))));
        }
        crate::exec::ChildEnv { clear: true, vars }
    }
}

pub struct Environments {
    root: PathBuf,
}

impl Environments {
    pub fn open(arc_home: &Path) -> Result<Environments> {
        let root = arc_home.join("environments");
        std::fs::create_dir_all(&root).context("creating environment directory")?;
        Ok(Environments { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path_of(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    fn marker(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.ready"))
    }

    /// Readiness is the marker, never the directory. A directory that exists
    /// without one is a failed build.
    pub fn ready(&self, id: &str) -> bool {
        std::fs::read(self.marker(id))
            .map(|b| b == id.as_bytes())
            .unwrap_or(false)
    }

    pub fn list(&self) -> Result<Vec<(String, u64)>> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(&self.root)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().to_string();
            if !e.file_type()?.is_dir() || !valid_id(&name) || !self.ready(&name) {
                continue;
            }
            out.push((name.clone(), dir_size(&e.path())));
        }
        out.sort();
        Ok(out)
    }

    /// Materialise `manifest`, or return the existing copy.
    ///
    /// Concurrent callers within a process serialise on the id; across
    /// processes the atomic rename decides, and the loser discards its work.
    pub fn materialise(
        &self,
        manifest: &EnvironmentManifest,
        store: &Store,
    ) -> Result<Materialised> {
        let id = manifest.id();
        manifest.validate().map_err(|e| anyhow::anyhow!(e))?;
        let flight = flight(&id);
        let _held = flight.lock().unwrap_or_else(|e| e.into_inner());

        let root = self.path_of(&id);
        if self.ready(&id) {
            return Ok(self.describe(manifest, id, root, 0, 0, true));
        }
        // A directory with no marker is wreckage from an interrupted build.
        if root.exists() {
            let _ = std::fs::remove_dir_all(&root);
        }

        let staging = self
            .root
            .join(format!(".building-{}-{}", std::process::id(), &id[..12]));
        let _ = std::fs::remove_dir_all(&staging);
        let built = self.build(manifest, store, &staging);
        let (files, bytes) = match built {
            Ok(v) => v,
            Err(e) => {
                let _ = remove_all(&staging);
                return Err(e);
            }
        };

        match std::fs::rename(&staging, &root) {
            Ok(()) => {}
            Err(_) if root.exists() => {
                // Another process finished first. Identical content by
                // construction, so its copy is as good as this one.
                let _ = remove_all(&staging);
                if !self.ready(&id) {
                    write_marker(&self.marker(&id), &id)?;
                }
                return Ok(self.describe(manifest, id, root, 0, 0, true));
            }
            Err(e) => {
                let _ = remove_all(&staging);
                return Err(e).context("publishing environment");
            }
        }
        write_marker(&self.marker(&id), &id)?;
        Ok(self.describe(manifest, id, root, files, bytes, false))
    }

    fn build(
        &self,
        manifest: &EnvironmentManifest,
        store: &Store,
        staging: &Path,
    ) -> Result<(usize, u64)> {
        std::fs::create_dir_all(staging)?;
        let mut files = 0;
        let mut bytes = 0;
        let mut links: Vec<(PathBuf, String)> = Vec::new();
        for f in &manifest.files {
            let rel = f.path.decode().map_err(|e| anyhow::anyhow!(e))?;
            // The same check the manifest already passed, applied again at the
            // one place that writes: validation and use must not drift apart.
            let dest = crate::outputs::safe_join(staging, &rel)?;
            match &f.link {
                Some(target) => links.push((dest, target.clone())),
                None => {
                    let d = Digest::parse(&f.digest)?;
                    if !store.exists(&d) {
                        bail!("environment object {} is missing", d.short());
                    }
                    store.materialize(&d, &dest, f.exec)?;
                    files += 1;
                    bytes += f.size;
                }
            }
        }
        for (dest, target) in links {
            if let Some(p) = dest.parent() {
                std::fs::create_dir_all(p)?;
            }
            symlink(&target, &dest)?;
            files += 1;
        }
        seal(staging)?;
        Ok((files, bytes))
    }

    fn describe(
        &self,
        manifest: &EnvironmentManifest,
        id: String,
        root: PathBuf,
        written_files: usize,
        written_bytes: u64,
        reused: bool,
    ) -> Materialised {
        let join = |dirs: &[String]| -> Vec<PathBuf> {
            dirs.iter()
                .filter_map(|d| crate::outputs::safe_join(&root, d).ok())
                .collect()
        };
        let tools = manifest
            .tools
            .iter()
            .filter_map(|t| {
                crate::outputs::safe_join(&root, &t.rel)
                    .ok()
                    .map(|p| (t.name.clone(), p))
            })
            .collect();
        Materialised {
            lease: Lease::take(&id),
            id,
            path_entries: join(&manifest.path_entries),
            library_path: join(&manifest.library_path),
            env: manifest.env.clone(),
            tools,
            root,
            written_bytes,
            written_files,
            reused,
        }
    }

    /// Remove environments nobody is using. `keep` is the set a caller wants
    /// preserved regardless; live leases are honoured on top of it.
    pub fn gc(&self, keep: &BTreeSet<String>) -> Result<Vec<String>> {
        let live = live_ids();
        let mut removed = Vec::new();
        for (id, _) in self.list()? {
            if keep.contains(&id) || live.contains(&id) {
                continue;
            }
            let _ = std::fs::remove_file(self.marker(&id));
            remove_all(&self.path_of(&id))?;
            removed.push(id);
        }
        // Staging directories from a process that died mid-build are never
        // anything but wreckage.
        for e in std::fs::read_dir(&self.root)? {
            let e = e?;
            if e.file_name().to_string_lossy().starts_with(".building-") {
                let _ = remove_all(&e.path());
            }
        }
        Ok(removed)
    }
}

fn write_marker(path: &Path, id: &str) -> Result<()> {
    let tmp = path.with_extension("ready-tmp");
    std::fs::write(&tmp, id.as_bytes())?;
    std::fs::rename(&tmp, path).context("marking environment ready")?;
    Ok(())
}

pub fn valid_id(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Per-id build locks, so eight jobs wanting the same toolchain fetch it once.
fn flight(id: &str) -> Arc<Mutex<()>> {
    static FLIGHTS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    let map = FLIGHTS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut m = map.lock().unwrap_or_else(|e| e.into_inner());
    m.entry(id.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

fn live() -> &'static Mutex<HashMap<String, usize>> {
    static LIVE: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
    LIVE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn live_ids() -> BTreeSet<String> {
    let m = live().lock().unwrap_or_else(|e| e.into_inner());
    m.keys().cloned().collect()
}

/// Make the tree read-only. A command that tries to rewrite a compiler gets an
/// error rather than corrupting every future job that reuses the environment.
#[cfg(unix)]
fn seal(root: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut stack = vec![root.to_path_buf()];
    let mut dirs = Vec::new();
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir)? {
            let e = e?;
            let ty = e.file_type()?;
            if ty.is_symlink() {
                continue;
            }
            if ty.is_dir() {
                stack.push(e.path());
                dirs.push(e.path());
            } else {
                let mode = e.metadata()?.permissions().mode();
                let ro = if mode & 0o111 != 0 { 0o555 } else { 0o444 };
                std::fs::set_permissions(e.path(), std::fs::Permissions::from_mode(ro))?;
            }
        }
    }
    // Directories last, and left traversable but not writable — the root
    // included, so no file can be added to the environment either.
    for d in dirs.into_iter().rev() {
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o555))?;
    }
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o555))?;
    Ok(())
}

#[cfg(not(unix))]
fn seal(root: &Path) -> Result<()> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir)? {
            let e = e?;
            if e.file_type()?.is_dir() {
                stack.push(e.path());
                continue;
            }
            let mut p = e.metadata()?.permissions();
            p.set_readonly(true);
            std::fs::set_permissions(e.path(), p)?;
        }
    }
    Ok(())
}

/// Read-only trees do not delete on Unix without restoring write permission on
/// their directories first.
pub fn remove_all(root: &Path) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    unseal(root);
    match std::fs::remove_dir_all(root) {
        Ok(()) => Ok(()),
        Err(e) if root.exists() => Err(e).with_context(|| format!("removing {}", root.display())),
        Err(_) => Ok(()),
    }
}

fn unseal(root: &Path) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755));
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(ty) = e.file_type() else { continue };
            if ty.is_dir() {
                stack.push(e.path());
            } else if !ty.is_symlink() {
                restore_write(&e.path());
            }
        }
    }
}

#[cfg(unix)]
fn restore_write(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o644));
}

#[cfg(not(unix))]
fn restore_write(p: &Path) {
    if let Ok(m) = std::fs::metadata(p) {
        let mut perm = m.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perm.set_readonly(false);
        let _ = std::fs::set_permissions(p, perm);
    }
}

fn dir_size(root: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(t) if t.is_file() => total += e.metadata().map(|m| m.len()).unwrap_or(0),
                _ => {}
            }
        }
    }
    total
}

#[cfg(unix)]
fn symlink(target: &str, dest: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, dest).with_context(|| format!("linking {}", dest.display()))
}

#[cfg(not(unix))]
fn symlink(target: &str, dest: &Path) -> Result<()> {
    // Symlink creation needs a privilege Windows does not grant by default.
    // Copying preserves the *content* the manifest promised, which is what the
    // environment is defined by.
    let src = dest
        .parent()
        .map(|p| p.join(target))
        .unwrap_or_else(|| PathBuf::from(target));
    std::fs::copy(&src, dest).with_context(|| format!("materialising link {}", dest.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment::manifest::*;
    use crate::remote::protocol::WirePath;

    /// `tag` keeps each test's environment id distinct. Leases are recorded
    /// process-wide, so two tests sharing an id would see each other's.
    fn fixture(tag: &str) -> (tempfile::TempDir, Store, EnvironmentManifest) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("home")).unwrap();
        let tool = store.put_bytes(b"#!/bin/sh\necho captured\n").unwrap();
        let data = store.put_bytes(tag.as_bytes()).unwrap();
        let mut m = EnvironmentManifest {
            schema_version: ENV_SCHEMA_VERSION,
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            tools: vec![EnvTool {
                name: "tool".into(),
                rel: "bin/tool".into(),
                digest: tool.hex(),
            }],
            files: vec![
                EnvFile {
                    path: WirePath::from_rel("bin/tool"),
                    digest: tool.hex(),
                    size: 24,
                    exec: true,
                    link: None,
                },
                EnvFile {
                    path: WirePath::from_rel("share/data"),
                    digest: data.hex(),
                    size: tag.len() as u64,
                    exec: false,
                    link: None,
                },
            ],
            env: vec![("LANG".into(), "C.UTF-8".into())],
            path_entries: vec!["bin".into()],
            library_path: vec!["lib".into()],
            host: HostRequirements {
                os: std::env::consts::OS.into(),
                arch: std::env::consts::ARCH.into(),
                libc: crate::environment::host::libc_flavour().into(),
                ..Default::default()
            },
            completeness: Completeness::Complete,
            gaps: Vec::new(),
        };
        m.canonicalise();
        (tmp, store, m)
    }

    #[test]
    fn materialisation_is_idempotent_and_the_second_call_writes_nothing() {
        let (tmp, store, m) = fixture("idempotent");
        let envs = Environments::open(&tmp.path().join("home")).unwrap();
        let first = envs.materialise(&m, &store).unwrap();
        assert!(!first.reused);
        assert_eq!(first.written_files, 2);
        assert_eq!(
            std::fs::read(first.root.join("share/data")).unwrap(),
            b"idempotent"
        );
        assert!(envs.ready(&m.id()));

        let second = envs.materialise(&m, &store).unwrap();
        assert!(second.reused);
        assert_eq!(second.written_files, 0);
        assert_eq!(second.root, first.root);
    }

    #[test]
    fn a_missing_object_leaves_no_environment_behind() {
        let (tmp, store, mut m) = fixture("missing");
        m.files[0].digest = "f".repeat(64);
        m.tools[0].digest = "f".repeat(64);
        let envs = Environments::open(&tmp.path().join("home")).unwrap();
        assert!(envs.materialise(&m, &store).is_err());
        assert!(!envs.ready(&m.id()));
        assert!(!envs.path_of(&m.id()).exists());
        assert!(envs.list().unwrap().is_empty());
    }

    #[test]
    fn a_directory_without_a_marker_is_never_treated_as_ready() {
        let (tmp, store, m) = fixture("marker");
        let envs = Environments::open(&tmp.path().join("home")).unwrap();
        let id = m.id();
        // Exactly what an interrupted build leaves: content, no marker.
        std::fs::create_dir_all(envs.path_of(&id).join("bin")).unwrap();
        std::fs::write(envs.path_of(&id).join("bin/tool"), b"half").unwrap();
        assert!(!envs.ready(&id));

        let done = envs.materialise(&m, &store).unwrap();
        assert!(!done.reused);
        assert_eq!(
            std::fs::read(done.root.join("bin/tool")).unwrap(),
            b"#!/bin/sh\necho captured\n"
        );
    }

    #[test]
    fn concurrent_requests_materialise_once() {
        let (tmp, store, m) = fixture("concurrent");
        let home = tmp.path().join("home");
        let reused = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        std::thread::scope(|s| {
            for _ in 0..8 {
                let home = home.clone();
                let m = m.clone();
                let reused = reused.clone();
                let store = &store;
                s.spawn(move || {
                    let envs = Environments::open(&home).unwrap();
                    let r = envs.materialise(&m, store).unwrap();
                    if r.reused {
                        reused.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                });
            }
        });
        assert_eq!(reused.load(std::sync::atomic::Ordering::Relaxed), 7);
    }

    #[cfg(unix)]
    #[test]
    fn a_materialised_environment_is_written_read_only() {
        use std::os::unix::fs::PermissionsExt;
        let (tmp, store, m) = fixture("readonly");
        let envs = Environments::open(&tmp.path().join("home")).unwrap();
        let e = envs.materialise(&m, &store).unwrap();

        let mode = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode();
        assert_eq!(mode(e.root.join("bin/tool")) & 0o222, 0);
        assert_ne!(mode(e.root.join("bin/tool")) & 0o111, 0, "still executable");
        assert_eq!(mode(e.root.join("share/data")) & 0o222, 0);
        assert_eq!(mode(e.root.clone()) & 0o222, 0, "no new files either");

        // Permission bits are the mechanism, and root ignores them — see
        // docs/environments.md. The write check only means something as an
        // ordinary user.
        // SAFETY: `geteuid` reads process state and cannot fail.
        if unsafe { libc::geteuid() } != 0 {
            assert!(std::fs::write(e.root.join("bin/tool"), b"vandalised").is_err());
            assert!(std::fs::write(e.root.join("bin/new"), b"extra").is_err());
        }
        assert_eq!(
            std::fs::read(e.root.join("bin/tool")).unwrap(),
            b"#!/bin/sh\necho captured\n"
        );
    }

    #[test]
    fn resolution_never_leaves_the_environment() {
        let (tmp, store, m) = fixture("resolve");
        let envs = Environments::open(&tmp.path().join("home")).unwrap();
        let e = envs.materialise(&m, &store).unwrap();
        assert_eq!(e.resolve("tool"), Some(e.root.join("bin/tool")));
        assert!(e.resolve("definitely-not-here").is_none());
        // A host path is not a way to reach outside it either.
        assert!(
            e.resolve("/bin/sh").is_none() || e.resolve("/bin/sh") == Some(e.root.join("bin/sh"))
        );
    }

    #[test]
    fn collection_removes_unused_environments_and_build_wreckage() {
        let (tmp, store, m) = fixture("gc");
        let home = tmp.path().join("home");
        let envs = Environments::open(&home).unwrap();
        let e = envs.materialise(&m, &store).unwrap();
        let id = e.id.clone();
        std::fs::create_dir_all(envs.root().join(".building-1-abc")).unwrap();

        // An environment in use survives collection even when nothing asked for
        // it to be kept.
        assert_eq!(envs.gc(&BTreeSet::new()).unwrap(), Vec::<String>::new());
        assert!(envs.ready(&id));
        drop(e);

        assert_eq!(
            envs.gc(&BTreeSet::from([id.clone()])).unwrap(),
            Vec::<String>::new()
        );
        assert!(envs.ready(&id));

        assert_eq!(envs.gc(&BTreeSet::new()).unwrap(), vec![id.clone()]);
        assert!(!envs.ready(&id));
        assert!(!envs.path_of(&id).exists());
        assert!(!envs.root().join(".building-1-abc").exists());
    }
}
