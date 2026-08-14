//! One execution, in a directory of its own.
//!
//! Trust boundary: everything in a request comes from a client the worker has
//! no reason to believe. Paths are validated before anything is created, the
//! workspace is the only writable location the command is given, and the
//! process tree is torn down whether the command finishes, times out, or is
//! cancelled.
//!
//! What this is not: a security sandbox against hostile code. The command runs
//! as the worker's user with the worker's network. See `docs/remote-execution.md`.

use anyhow::{bail, Context, Result};
use arc_core::environment::Materialised;
use arc_core::hash::Digest;
use arc_core::record::OutputFile;
use arc_core::remote::execution::{ExecutionRequest, Limits, ManifestEntry, ToolRequirement};
use arc_core::store::Store;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Beyond this, a command's output is streamed to the log but not kept for
/// replay. Matches the local engine's limit so a remote result is not a
/// different kind of result.
pub const MAX_CAPTURE: usize = arc_core::exec::MAX_CAPTURE;

pub struct Sandbox {
    root: PathBuf,
    workspace: PathBuf,
    home: PathBuf,
    temp: PathBuf,
}

impl Sandbox {
    /// Create the directory tree for one execution. `root` must be a fresh path
    /// inside the worker's work area.
    pub fn create(root: PathBuf) -> Result<Sandbox> {
        let workspace = root.join("workspace");
        let home = root.join("home");
        let temp = root.join("tmp");
        for d in [&workspace, &home, &temp] {
            std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
        }
        Ok(Sandbox {
            root,
            workspace,
            home,
            temp,
        })
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Write every input into the workspace.
    ///
    /// Objects are copied out of the worker's CAS, never linked: a hardlink
    /// would let the command mutate the stored object through the workspace and
    /// silently corrupt every future execution that reuses it.
    pub fn materialise(&self, store: &Store, inputs: &[ManifestEntry]) -> Result<u64> {
        let mut planned: Vec<(PathBuf, &ManifestEntry)> = Vec::with_capacity(inputs.len());
        for e in inputs {
            let rel = e.path.decode().map_err(|m| anyhow::anyhow!(m))?;
            // The same check the client applies when restoring, applied here so
            // a malicious manifest cannot reach outside the workspace even if
            // protocol validation were bypassed.
            planned.push((arc_core::outputs::safe_join(&self.workspace, &rel)?, e));
        }
        let mut bytes = 0;
        for (dest, e) in planned {
            let digest = Digest::parse(&e.digest)?;
            if !store.exists(&digest) {
                bail!("input object {} is missing", digest.short());
            }
            store.materialize(&digest, &dest, e.exec)?;
            bytes += e.size;
        }
        Ok(bytes)
    }

    /// The environment the command sees. Built from nothing: the worker's own
    /// environment — which holds its service credentials — is never inherited.
    ///
    /// With an Arc environment the request's variables come first and the
    /// environment's definition overrides them, because the environment is what
    /// the execution key describes and a coordinator's `PATH` is not.
    fn environment(
        &self,
        req: &ExecutionRequest,
        program: &Path,
        env: Option<&Materialised>,
    ) -> Vec<(String, String)> {
        let mut vars: Vec<(String, String)> = req.env.clone();
        if let Some(m) = env {
            let defined = m.child_env(&self.home, &self.temp);
            vars.retain(|(k, _)| !defined.vars.iter().any(|(d, _)| d == k));
            vars.extend(defined.vars);
            return vars;
        }
        let has = |env: &[(String, String)], k: &str| env.iter().any(|(n, _)| n == k);
        if !has(&vars, "PATH") {
            if let Some(dir) = program.parent() {
                vars.push(("PATH".into(), dir.to_string_lossy().to_string()));
            }
        }
        // Sandbox-local locations, so a command cannot read or pollute the
        // worker's real user state.
        vars.push(("HOME".into(), self.home.to_string_lossy().to_string()));
        for k in ["TMPDIR", "TEMP", "TMP"] {
            vars.push((k.into(), self.temp.to_string_lossy().to_string()));
        }
        vars
    }

    /// Run the command, capturing output and enforcing the timeout.
    pub fn run(
        &self,
        req: &ExecutionRequest,
        program: &Path,
        limits: &Limits,
        log: &LogSink,
        cancelled: &Arc<AtomicBool>,
        env: Option<&Materialised>,
    ) -> Result<Completion> {
        let cwd = if req.rel_cwd.is_empty() {
            self.workspace.clone()
        } else {
            arc_core::outputs::safe_join(&self.workspace, &req.rel_cwd)?
        };
        std::fs::create_dir_all(&cwd)?;

        let mut cmd = Command::new(program);
        cmd.args(&req.args)
            .current_dir(&cwd)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in self.environment(req, program, env) {
            cmd.env(k, v);
        }
        new_process_group(&mut cmd);

        let started = Instant::now();
        let mut child = cmd
            .spawn()
            .with_context(|| format!("starting {}", program.display()))?;
        let pid = child.id();

        let out = child.stdout.take().expect("piped");
        let err = child.stderr.take().expect("piped");
        let pump_out = pump(out, log.clone());
        let pump_err = pump(err, log.clone());

        let timeout = Duration::from_millis(limits.timeout_ms);
        let mut timed_out = false;
        let status = loop {
            match child.try_wait()? {
                Some(s) => break s,
                None => {
                    if started.elapsed() > timeout {
                        timed_out = true;
                        terminate_tree(pid);
                        break child.wait()?;
                    }
                    if cancelled.load(Ordering::Relaxed) {
                        terminate_tree(pid);
                        break child.wait()?;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        };
        // Whatever happened to the parent, nothing it started may outlive the
        // sandbox: a surviving process would keep writing into a workspace that
        // is about to be deleted, and its writes would never be captured.
        terminate_tree(pid);

        let (stdout, out_truncated) = pump_out.join().unwrap_or_default();
        let (stderr, err_truncated) = pump_err.join().unwrap_or_default();

        Ok(Completion {
            exit_code: exit_code_of(&status),
            signaled: signaled(&status),
            timed_out,
            cancelled: cancelled.load(Ordering::Relaxed),
            duration_ms: started.elapsed().as_millis() as u64,
            stdout,
            stderr,
            truncated: out_truncated || err_truncated,
        })
    }

    /// Collect declared outputs into the worker's CAS.
    pub fn capture(&self, globs: &[String], store: &Store, limit: u64) -> Result<Vec<OutputFile>> {
        let files = arc_core::outputs::capture(&self.workspace, globs, store)?;
        let total: u64 = files.iter().map(|f| f.size).sum();
        if total > limit {
            bail!("outputs total {total} bytes, over the worker's limit of {limit}");
        }
        Ok(files)
    }

    pub fn remove(self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[derive(Debug, Default)]
pub struct Completion {
    pub exit_code: i32,
    pub signaled: bool,
    pub timed_out: bool,
    pub cancelled: bool,
    pub duration_ms: u64,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub truncated: bool,
}

/// A bounded, shared view of a job's output, for clients following along.
///
/// Logs are informational: nothing about cache correctness depends on them
/// arriving, so the sink drops the tail rather than growing without limit.
#[derive(Clone)]
pub struct LogSink(Arc<Mutex<LogBuffer>>);

#[derive(Default)]
struct LogBuffer {
    text: String,
    total: u64,
    dropped: bool,
}

impl Default for LogSink {
    fn default() -> Self {
        LogSink(Arc::new(Mutex::new(LogBuffer::default())))
    }
}

impl LogSink {
    pub fn append(&self, bytes: &[u8]) {
        let Ok(mut b) = self.0.lock() else { return };
        if b.total >= arc_core::remote::execution::MAX_LOG_BYTES {
            if !b.dropped {
                b.dropped = true;
                b.text.push_str("\n[arc: log truncated]\n");
            }
            return;
        }
        b.total += bytes.len() as u64;
        b.text.push_str(&String::from_utf8_lossy(bytes));
    }

    pub fn len(&self) -> u64 {
        self.0.lock().map(|b| b.text.len() as u64).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes from `offset`, clamped to a character boundary so the chunk is
    /// always valid UTF-8.
    pub fn slice(&self, offset: u64, max: usize) -> (u64, String) {
        let Ok(b) = self.0.lock() else {
            return (0, String::new());
        };
        let start = (offset as usize).min(b.text.len());
        let mut start = start;
        while start < b.text.len() && !b.text.is_char_boundary(start) {
            start += 1;
        }
        let mut end = (start + max).min(b.text.len());
        while end > start && !b.text.is_char_boundary(end) {
            end -= 1;
        }
        (b.text.len() as u64, b.text[start..end].to_string())
    }
}

fn pump(
    mut src: impl Read + Send + 'static,
    log: LogSink,
) -> std::thread::JoinHandle<(Vec<u8>, bool)> {
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 32 * 1024];
        let mut captured = Vec::new();
        let mut truncated = false;
        loop {
            match src.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    log.append(&buf[..n]);
                    if captured.len() + n <= MAX_CAPTURE {
                        captured.extend_from_slice(&buf[..n]);
                    } else {
                        truncated = true;
                    }
                }
            }
        }
        (captured, truncated)
    })
}

/// Resolve a required executable and prove it is the one the client meant.
///
/// Resolution happens once, immediately before spawning, and the resolved path
/// is what runs — so PATH changing between the check and the spawn cannot
/// substitute a different binary.
pub fn resolve_tool(t: &ToolRequirement, cwd: &Path) -> Result<PathBuf> {
    let path = arc_core::key::which(&t.program, cwd)
        .with_context(|| format!("`{}` is not available on this worker", t.program))?;
    let actual =
        arc_core::hash::hash_file(&path).with_context(|| format!("hashing {}", path.display()))?;
    if actual.hex() != t.digest {
        bail!(
            "`{}` on this worker is {}, the client needs {}",
            t.program,
            actual.short(),
            &t.digest[..12]
        );
    }
    Ok(path)
}

#[cfg(unix)]
fn new_process_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: `setsid` is async-signal-safe and touches no process state the
    // forked child shares with the parent. A session of its own is what makes
    // whole-tree termination possible: killing the group reaches every
    // descendant, including ones that reparented.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn new_process_group(_cmd: &mut Command) {}

#[cfg(unix)]
fn terminate_tree(pid: u32) {
    // Negative pid addresses the whole process group, which `setsid` made this
    // child the leader of.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn terminate_tree(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(unix)]
fn exit_code_of(s: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(0))
}
#[cfg(not(unix))]
fn exit_code_of(s: &std::process::ExitStatus) -> i32 {
    s.code().unwrap_or(1)
}

#[cfg(unix)]
fn signaled(s: &std::process::ExitStatus) -> bool {
    use std::os::unix::process::ExitStatusExt;
    s.signal().is_some()
}
#[cfg(not(unix))]
fn signaled(_s: &std::process::ExitStatus) -> bool {
    false
}

/// Write a file, used by the worker to persist small metadata atomically.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_traversing_manifest_entry_never_reaches_the_filesystem() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("cas")).unwrap();
        let d = store.put_bytes(b"payload").unwrap();
        let sb = Sandbox::create(tmp.path().join("job")).unwrap();

        let entry = |rel: &str| ManifestEntry {
            path: arc_core::remote::protocol::WirePath::from_rel(rel),
            digest: d.hex(),
            size: 7,
            exec: false,
        };
        for bad in ["../escape", "a/../../escape", "/etc/passwd"] {
            assert!(
                sb.materialise(&store, &[entry(bad)]).is_err(),
                "{bad} should be refused"
            );
        }
        assert!(!tmp.path().join("escape").exists());
        sb.materialise(&store, &[entry("src/ok.txt")]).unwrap();
        assert_eq!(
            std::fs::read(sb.workspace().join("src/ok.txt")).unwrap(),
            b"payload"
        );
    }

    #[test]
    fn materialisation_copies_so_a_command_cannot_corrupt_the_store() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("cas")).unwrap();
        let d = store.put_bytes(b"original").unwrap();
        let sb = Sandbox::create(tmp.path().join("job")).unwrap();
        sb.materialise(
            &store,
            &[ManifestEntry {
                path: arc_core::remote::protocol::WirePath::from_rel("f.txt"),
                digest: d.hex(),
                size: 8,
                exec: false,
            }],
        )
        .unwrap();

        std::fs::write(sb.workspace().join("f.txt"), b"vandalised").unwrap();
        assert_eq!(store.read(&d).unwrap(), b"original");
        assert!(store.verify(&d).unwrap().is_none());
    }

    #[test]
    fn a_log_sink_stops_growing_and_stays_valid_utf8() {
        let log = LogSink::default();
        log.append("héllo wörld".as_bytes());
        let (total, text) = log.slice(0, 3);
        assert!(total > 0);
        assert!(text.chars().count() <= 3);

        for _ in 0..2000 {
            log.append(&vec![b'x'; 8192]);
        }
        assert!(log.len() < arc_core::remote::execution::MAX_LOG_BYTES + (1 << 20));
    }

    #[test]
    fn a_tool_whose_contents_differ_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("tool");
        std::fs::write(&exe, b"#!/bin/sh\n").unwrap();
        let real = arc_core::hash::hash_file(&exe).unwrap().hex();

        let ok = ToolRequirement {
            program: exe.to_string_lossy().to_string(),
            digest: real,
        };
        assert!(resolve_tool(&ok, tmp.path()).is_ok());

        let wrong = ToolRequirement {
            program: exe.to_string_lossy().to_string(),
            digest: "f".repeat(64),
        };
        assert!(resolve_tool(&wrong, tmp.path()).is_err());
    }
}
