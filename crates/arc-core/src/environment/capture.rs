//! Building an environment out of a machine that already has one.
//!
//! Capture is explicit and narrow by construction: it takes named tools and
//! named directories, never "everything this command touched" and never a home
//! directory. What it adds on its own is the runtime closure of the binaries it
//! was told to take — and only the parts of that closure which are not the
//! host's own C library.

use super::elf;
use super::host;
use super::manifest::{
    Completeness, EnvFile, EnvTool, EnvironmentManifest, HostRequirements, ENV_SCHEMA_VERSION,
    MAX_ENV_FILES,
};
use crate::remote::protocol::WirePath;
use crate::store::Store;
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// A directory to take wholesale, at a path chosen by the project rather than
/// by the machine it came from.
#[derive(Debug, Clone, Default)]
pub struct Tree {
    pub from: PathBuf,
    /// Where it lands inside the environment. Relative, and part of identity.
    pub to: String,
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Spec {
    /// Programs to resolve on this machine and capture into `bin/`.
    pub tools: Vec<String>,
    pub trees: Vec<Tree>,
    pub env: Vec<(String, String)>,
    /// Extra `PATH` entries inside the environment, after the ones Arc derives.
    pub path: Vec<String>,
}

impl Spec {
    /// Read a `[environment.<alias>]` block. `~` expands here, at the one point
    /// where a machine-local path is legitimate: capture time.
    pub fn from_config(cfg: &crate::project::EnvironmentConfig) -> Spec {
        Spec {
            tools: cfg.tools.clone(),
            trees: cfg
                .trees
                .iter()
                .map(|t| Tree {
                    from: expand_home(&t.from),
                    to: t.to.clone(),
                    exclude: t.exclude.clone(),
                })
                .collect(),
            env: cfg
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            path: cfg.path.clone(),
        }
    }
}

fn expand_home(p: &str) -> PathBuf {
    let Some(rest) = p.strip_prefix('~') else {
        return PathBuf::from(p);
    };
    let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) else {
        return PathBuf::from(p);
    };
    PathBuf::from(home).join(rest.trim_start_matches(['/', '\\']))
}

#[derive(Debug)]
pub struct Captured {
    pub manifest: EnvironmentManifest,
    pub id: String,
    pub files: usize,
    pub bytes: u64,
    pub elapsed_ms: u64,
    /// Where each captured file came from. Diagnostics only: absolute source
    /// paths are deliberately not part of the manifest or of identity.
    pub sources: Vec<(String, PathBuf)>,
}

/// Take everything `spec` names, plus the runtime closure of the executables
/// among it, and put the bytes in `store`.
pub fn capture(spec: &Spec, store: &Store) -> Result<Captured> {
    let started = Instant::now();
    if spec.tools.is_empty() && spec.trees.is_empty() {
        bail!("an environment must declare at least one tool or tree");
    }
    for (k, _) in &spec.env {
        if crate::key::looks_secret(k) {
            bail!("`{k}` looks like a secret and will not be captured into an environment");
        }
    }

    let mut b = Builder::new(store);
    for t in &spec.trees {
        b.tree(t)?;
    }
    for name in &spec.tools {
        b.tool(name)?;
    }
    b.closure()?;

    let mut path_entries = vec!["bin".to_string()];
    for t in &spec.trees {
        let bin = format!("{}/bin", t.to.trim_end_matches('/'));
        if b.has_dir(&bin) {
            path_entries.push(bin);
        }
    }
    path_entries.extend(spec.path.iter().cloned());
    path_entries.retain(|p| !p.is_empty());

    let mut library_path = vec!["lib".to_string()];
    library_path.extend(b.extra_library_dirs.iter().cloned());

    let complete = b.gaps.is_empty();
    let mut manifest = EnvironmentManifest {
        schema_version: ENV_SCHEMA_VERSION,
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        tools: b.tools.clone(),
        files: b.files.values().cloned().collect(),
        env: spec.env.clone(),
        path_entries,
        library_path,
        host: HostRequirements {
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            libc: host::libc_flavour().into(),
            interpreters: b.interpreters.iter().cloned().collect(),
            libraries: b.host_libraries.iter().cloned().collect(),
        },
        completeness: if complete {
            Completeness::Complete
        } else {
            Completeness::Partial
        },
        gaps: b.gaps.iter().cloned().collect(),
    };
    manifest.canonicalise();
    manifest.validate().map_err(|e| anyhow::anyhow!(e))?;

    Ok(Captured {
        id: manifest.id(),
        files: manifest.files.len(),
        bytes: manifest.total_bytes(),
        elapsed_ms: started.elapsed().as_millis() as u64,
        sources: b.sources,
        manifest,
    })
}

struct Builder<'a> {
    store: &'a Store,
    files: BTreeMap<String, EnvFile>,
    tools: Vec<EnvTool>,
    /// Executables whose runtime closure still has to be walked.
    pending: VecDeque<(String, PathBuf)>,
    /// Sonames already accounted for, captured or host.
    seen_libs: BTreeSet<String>,
    interpreters: BTreeSet<String>,
    host_libraries: BTreeSet<String>,
    extra_library_dirs: BTreeSet<String>,
    gaps: BTreeSet<String>,
    sources: Vec<(String, PathBuf)>,
}

impl<'a> Builder<'a> {
    fn new(store: &'a Store) -> Builder<'a> {
        Builder {
            store,
            files: BTreeMap::new(),
            tools: Vec::new(),
            pending: VecDeque::new(),
            seen_libs: BTreeSet::new(),
            interpreters: BTreeSet::new(),
            host_libraries: BTreeSet::new(),
            extra_library_dirs: BTreeSet::new(),
            gaps: BTreeSet::new(),
            sources: Vec::new(),
        }
    }

    fn has_dir(&self, rel: &str) -> bool {
        let prefix = format!("{rel}/");
        self.files.keys().any(|k| k.starts_with(&prefix))
    }

    fn tool(&mut self, name: &str) -> Result<()> {
        let cwd = std::env::current_dir()?;
        let resolved = crate::key::which(name, &cwd)
            .with_context(|| format!("`{name}` is not on this machine's PATH"))?;
        let base = Path::new(name)
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| name.to_string());
        let rel = format!("bin/{base}");
        let digest = self.add_file(&rel, &resolved, true)?;
        self.tools.push(EnvTool {
            name: name.to_string(),
            rel: rel.clone(),
            digest,
        });
        self.pending.push_back((rel, resolved));
        Ok(())
    }

    fn tree(&mut self, t: &Tree) -> Result<()> {
        let root = t
            .from
            .canonicalize()
            .with_context(|| format!("resolving {}", t.from.display()))?;
        let to = t.to.trim_matches('/').to_string();
        if to.is_empty() {
            bail!("a captured tree needs a destination inside the environment");
        }
        crate::remote::protocol::validate_rel_path(&to).map_err(|e| anyhow::anyhow!(e))?;
        let excludes = crate::scan::build_globs(&t.exclude)?;
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in
                std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))?
            {
                let entry = entry?;
                let path = entry.path();
                let Ok(rel_src) = path.strip_prefix(&root) else {
                    continue;
                };
                let rel_str = rel_src.to_string_lossy().replace('\\', "/");
                if excludes.is_match(&rel_str) {
                    continue;
                }
                let rel = format!("{to}/{rel_str}");
                let meta = std::fs::symlink_metadata(&path)?;
                let ty = meta.file_type();
                if ty.is_dir() {
                    stack.push(path);
                } else if ty.is_symlink() {
                    self.add_symlink(&rel, &path)?;
                } else if ty.is_file() {
                    let exec = is_exec(&meta);
                    self.add_file(&rel, &path, exec)?;
                    // Only executables directly on the tree's PATH are treated
                    // as entry points. Walking the closure of every binary in a
                    // toolchain would pull in the whole distribution.
                    if exec && rel_str.starts_with("bin/") && !rel_str[4..].contains('/') {
                        self.pending.push_back((rel, path));
                    }
                } else {
                    // Device nodes, sockets and FIFOs are host state, not
                    // content. An environment claiming to reproduce one would
                    // be lying about what it is.
                    bail!(
                        "{} is not a regular file, symlink or directory and cannot be captured",
                        path.display()
                    );
                }
                if self.files.len() > MAX_ENV_FILES {
                    bail!("environment would exceed {MAX_ENV_FILES} files");
                }
            }
        }
        Ok(())
    }

    /// Walk `DT_NEEDED` from every captured executable, capturing the libraries
    /// that belong to the toolchain and recording the ones that belong to the
    /// host.
    fn closure(&mut self) -> Result<()> {
        while let Some((rel, source)) = self.pending.pop_front() {
            let Some(needs) = elf::needs(&source) else {
                continue;
            };
            if let Some(interp) = &needs.interpreter {
                self.interpreters.insert(interp.clone());
            }
            let origin = source.parent().map(Path::to_path_buf).unwrap_or_default();
            for soname in &needs.needed {
                if !self.seen_libs.insert(soname.clone()) {
                    continue;
                }
                match resolve_library(soname, &needs.runpath, &origin) {
                    Some(path) if host::is_system_library(&path) => {
                        self.host_libraries.insert(soname.clone());
                    }
                    Some(path) => {
                        // Already inside a captured tree? Then it materialises
                        // with the tree; it only needs to be findable.
                        if let Some(dir) = self.captured_dir_of(&path) {
                            self.extra_library_dirs.insert(dir);
                            continue;
                        }
                        let dest = format!("lib/{soname}");
                        self.add_file(&dest, &path, true)?;
                        self.pending.push_back((dest, path));
                    }
                    None => {
                        self.gaps
                            .insert(format!("{rel} needs {soname}, which could not be resolved"));
                    }
                }
            }
        }
        Ok(())
    }

    /// If `path` is a file this capture already took, the environment-relative
    /// directory it landed in.
    fn captured_dir_of(&self, path: &Path) -> Option<String> {
        let name = path.file_name()?.to_string_lossy().to_string();
        self.sources
            .iter()
            .find(|(_, src)| src == path)
            .and_then(|(rel, _)| rel.strip_suffix(&format!("/{name}")))
            .map(str::to_string)
    }

    fn add_file(&mut self, rel: &str, source: &Path, exec: bool) -> Result<String> {
        let (digest, size) = self
            .store
            .put_file(source)
            .with_context(|| format!("capturing {}", source.display()))?;
        self.files.insert(
            rel.to_string(),
            EnvFile {
                path: WirePath::from_rel(rel),
                digest: digest.hex(),
                size,
                exec,
                link: None,
            },
        );
        self.sources.push((rel.to_string(), source.to_path_buf()));
        Ok(digest.hex())
    }

    fn add_symlink(&mut self, rel: &str, source: &Path) -> Result<()> {
        let target = std::fs::read_link(source)?;
        let target_str = target.to_string_lossy().replace('\\', "/");
        // An absolute or escaping link would reintroduce a host path into
        // something that is supposed to be relocatable. Follow it instead: if
        // it lands on a regular file, that file's *content* is portable.
        let escapes = target_str.starts_with('/') || target_str.starts_with("..");
        if escapes {
            match std::fs::metadata(source) {
                Ok(m) if m.is_file() => {
                    self.add_file(rel, source, is_exec(&m))?;
                    return Ok(());
                }
                _ => {
                    self.gaps
                        .insert(format!("{rel} links outside the captured tree"));
                    return Ok(());
                }
            }
        }
        self.files.insert(
            rel.to_string(),
            EnvFile {
                path: WirePath::from_rel(rel),
                digest: String::new(),
                size: 0,
                exec: false,
                link: Some(target_str),
            },
        );
        Ok(())
    }
}

/// Loader-style resolution, restricted to what a capture can reproduce:
/// `RUNPATH`/`RPATH` with `$ORIGIN` expanded, then the system directories.
///
/// `LD_LIBRARY_PATH` is deliberately ignored — honouring an ambient variable
/// would make the same machine capture two different environments.
fn resolve_library(soname: &str, runpath: &[String], origin: &Path) -> Option<PathBuf> {
    if soname.contains('/') || soname.contains('\0') {
        return None;
    }
    for entry in runpath {
        let expanded = entry
            .replace("$ORIGIN", &origin.to_string_lossy())
            .replace("${ORIGIN}", &origin.to_string_lossy());
        let candidate = PathBuf::from(expanded).join(soname);
        if candidate.is_file() {
            return candidate.canonicalize().ok().or(Some(candidate));
        }
    }
    host::find_system_library(soname)
}

#[cfg(unix)]
fn is_exec(m: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    m.permissions().mode() & 0o111 != 0
}
#[cfg(not(unix))]
fn is_exec(_m: &std::fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let s = Store::open(&tmp.path().join("home")).unwrap();
        (tmp, s)
    }

    #[test]
    fn an_empty_specification_is_refused() {
        let (_t, s) = store();
        assert!(capture(&Spec::default(), &s).is_err());
    }

    #[test]
    fn a_secret_shaped_variable_is_refused_at_capture() {
        let (_t, s) = store();
        let spec = Spec {
            tools: vec!["arc-nonexistent-tool".into()],
            env: vec![("AWS_SECRET_ACCESS_KEY".into(), "x".into())],
            ..Default::default()
        };
        assert!(capture(&spec, &s)
            .unwrap_err()
            .to_string()
            .contains("secret"));
    }

    #[test]
    fn a_tree_is_captured_by_content_at_a_declared_relative_path() {
        let (tmp, s) = store();
        let src = tmp.path().join("toolchain");
        std::fs::create_dir_all(src.join("bin")).unwrap();
        std::fs::create_dir_all(src.join("share/doc")).unwrap();
        std::fs::write(src.join("bin/tool"), b"#!/bin/sh\necho hi\n").unwrap();
        std::fs::write(src.join("share/data.txt"), b"data").unwrap();
        std::fs::write(src.join("share/doc/manual"), b"docs").unwrap();

        let spec = Spec {
            trees: vec![Tree {
                from: src.clone(),
                to: "tc".into(),
                exclude: vec!["share/doc/**".into()],
            }],
            ..Default::default()
        };
        let c = capture(&spec, &s).unwrap();
        let paths: Vec<String> = c.manifest.files.iter().map(|f| f.path.v.clone()).collect();
        assert!(paths.contains(&"tc/bin/tool".to_string()), "{paths:?}");
        assert!(paths.contains(&"tc/share/data.txt".to_string()));
        assert!(
            !paths.iter().any(|p| p.contains("doc")),
            "excluded: {paths:?}"
        );
        assert!(c.manifest.path_entries.contains(&"tc/bin".to_string()));

        // Capturing the same bytes again gives the same identity, and the
        // absolute source path is nowhere in it.
        let again = capture(&spec, &s).unwrap();
        assert_eq!(c.id, again.id);
        let json = String::from_utf8(c.manifest.canonical_bytes()).unwrap();
        assert!(!json.contains(&src.to_string_lossy().replace('\\', "\\\\")));
    }

    #[test]
    fn changing_one_captured_byte_changes_the_environment_id() {
        let (tmp, s) = store();
        let src = tmp.path().join("tc");
        std::fs::create_dir_all(src.join("bin")).unwrap();
        std::fs::write(src.join("bin/tool"), b"one").unwrap();
        let spec = Spec {
            trees: vec![Tree {
                from: src.clone(),
                to: "tc".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let before = capture(&spec, &s).unwrap().id;
        std::fs::write(src.join("bin/tool"), b"two").unwrap();
        assert_ne!(before, capture(&spec, &s).unwrap().id);
    }

    #[cfg(unix)]
    #[test]
    fn a_special_file_is_refused_rather_than_captured_as_something_else() {
        let (tmp, s) = store();
        let src = tmp.path().join("tc");
        std::fs::create_dir_all(&src).unwrap();
        let fifo = src.join("pipe");
        let c = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
        // SAFETY: `mkfifo` takes a NUL-terminated path and touches nothing else.
        if unsafe { libc::mkfifo(c.as_ptr(), 0o644) } != 0 {
            return;
        }
        let spec = Spec {
            trees: vec![Tree {
                from: src,
                to: "tc".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let e = capture(&spec, &s).unwrap_err().to_string();
        assert!(e.contains("not a regular file"), "{e}");
    }

    #[cfg(unix)]
    #[test]
    fn a_relative_symlink_is_kept_and_an_escaping_one_is_not() {
        let (tmp, s) = store();
        let src = tmp.path().join("tc");
        std::fs::create_dir_all(src.join("bin")).unwrap();
        std::fs::write(src.join("bin/real"), b"real").unwrap();
        std::os::unix::fs::symlink("real", src.join("bin/alias")).unwrap();
        // The escape target has to exist, and has to exist everywhere: a
        // dangling link is skipped rather than followed, which would make this
        // test silently prove nothing. `/etc/hostname` is absent on macOS, so
        // the target is one this test creates, outside the captured tree.
        let outside = tmp.path().join("outside.txt");
        std::fs::write(&outside, b"outside").unwrap();
        std::os::unix::fs::symlink(&outside, src.join("bin/escape")).unwrap();

        let spec = Spec {
            trees: vec![Tree {
                from: src,
                to: "tc".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let c = capture(&spec, &s).unwrap();
        let alias = c
            .manifest
            .files
            .iter()
            .find(|f| f.path.v == "tc/bin/alias")
            .unwrap();
        assert_eq!(alias.link.as_deref(), Some("real"));

        // The escaping link was followed and captured by content, so no host
        // path survives into the environment.
        let escape = c
            .manifest
            .files
            .iter()
            .find(|f| f.path.v == "tc/bin/escape")
            .unwrap();
        assert!(escape.link.is_none());
        c.manifest.validate().unwrap();
    }

    #[test]
    fn a_resolvable_tool_lands_in_bin_with_its_contents() {
        let (_t, s) = store();
        let program = if cfg!(windows) { "cmd" } else { "sh" };
        let spec = Spec {
            tools: vec![program.into()],
            ..Default::default()
        };
        let Ok(c) = capture(&spec, &s) else { return };
        let tool = c.manifest.tool(program).unwrap();
        assert_eq!(tool.rel, format!("bin/{program}"));
        let real = crate::key::which(program, Path::new(".")).unwrap();
        assert_eq!(tool.digest, crate::hash::hash_file(&real).unwrap().hex());
    }

    #[test]
    fn an_unresolvable_tool_fails_loudly() {
        let (_t, s) = store();
        let spec = Spec {
            tools: vec!["arc-definitely-not-a-program".into()],
            ..Default::default()
        };
        assert!(capture(&spec, &s).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_dynamically_linked_tool_records_its_loader_and_libraries() {
        let (_t, s) = store();
        let spec = Spec {
            tools: vec!["sh".into()],
            ..Default::default()
        };
        let Ok(c) = capture(&spec, &s) else { return };
        let sh = crate::key::which("sh", Path::new(".")).unwrap();
        let Some(needs) = elf::needs(&sh) else { return };
        if needs.interpreter.is_some() {
            assert!(!c.manifest.host.interpreters.is_empty());
            // glibc is a host requirement, never something Arc packages.
            assert!(!c.manifest.host.libraries.is_empty());
            assert!(!c
                .manifest
                .files
                .iter()
                .any(|f| f.path.v.starts_with("lib/libc.so")));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_private_library_is_captured_and_a_missing_one_is_a_gap() {
        // Resolution is what is under test, not linking: RUNPATH pointing at a
        // directory Arc did not capture is what makes a library "private".
        let dir = tempfile::tempdir().unwrap();
        let priv_dir = dir.path().join("private");
        std::fs::create_dir_all(&priv_dir).unwrap();
        // `$ORIGIN/../private` is resolved by the kernel, which needs every
        // component of the path to exist — `bin` included.
        std::fs::create_dir_all(dir.path().join("bin")).unwrap();
        std::fs::write(priv_dir.join("libthing.so.1"), b"not really a library").unwrap();

        let found = resolve_library(
            "libthing.so.1",
            &["$ORIGIN/../private".into()],
            &dir.path().join("bin"),
        );
        // Canonicalised on both sides: a temp directory reached through a
        // symlink is still the same file.
        let found = found.expect("the private library");
        assert_eq!(
            found.canonicalize().unwrap(),
            priv_dir.join("libthing.so.1").canonicalize().unwrap()
        );
        assert!(!host::is_system_library(&found));

        assert!(resolve_library("libnope.so.99", &[], Path::new("/")).is_none());
    }
}
