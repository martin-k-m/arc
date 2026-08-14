//! Child process execution: direct spawn, streamed output, captured copy.

use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

/// Output beyond this is streamed but not cached: holding an unbounded build
/// log in memory to make it reusable is a bad trade.
pub const MAX_CAPTURE: usize = 64 * 1024 * 1024;

/// How a child ended.
#[derive(Debug, Clone, Copy)]
pub struct Wait {
    pub code: i32,
    /// Killed by a signal; the result says nothing about the inputs.
    pub signaled: bool,
}

pub struct Outcome {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub duration_ms: u64,
    /// Output exceeded `MAX_CAPTURE`, or capture was off.
    pub truncated: bool,
    pub signaled: bool,
}

/// A tracer's hooks into the child's lifecycle.
///
/// The two interesting moments are before `exec`, which is the only point a
/// process can put itself under a tracer, and the wait itself, which a
/// ptrace-style backend must own because it is also the event loop. Backends
/// that need neither — a filesystem-snapshot diff, a Windows job object — take
/// the defaults.
pub trait Supervisor {
    /// The child must place itself under this process's control before `exec`.
    fn traced(&self) -> bool {
        false
    }
    /// A hook the child runs between `fork` and `exec`, supplied by whichever
    /// tracing backend is in use.
    fn pre_exec(&self) -> Option<crate::trace::PreExec> {
        None
    }
    /// Called with the child's pid as soon as it exists and before any of its
    /// output is read.
    fn on_spawn(&mut self, _pid: u32) {}
    /// Take over waiting for the child. `None` means the caller waits normally.
    fn wait(&mut self, _pid: u32) -> Option<Result<Wait>> {
        None
    }
    /// Tracing could not be started. The command still runs; only the
    /// observation is lost.
    fn disable(&mut self, _reason: String) {}
}

/// A run with no observation at all.
impl Supervisor for () {}

/// The environment a child is given, when Arc is supplying one rather than
/// inheriting this process's.
///
/// `clear` is the whole point: an execution environment that merely *adds* to
/// the ambient environment is not an execution environment, because whatever
/// the caller happened to export would still reach the command.
#[derive(Debug, Default, Clone)]
pub struct ChildEnv {
    pub clear: bool,
    pub vars: Vec<(String, String)>,
}

/// Run `program` with `args` in `cwd`. Output is streamed to this process's
/// stdout/stderr as it arrives and, when `capture` is set, copied into memory.
pub fn run(
    program: &Path,
    args: &[String],
    cwd: &Path,
    capture: bool,
    sup: &mut dyn Supervisor,
    env: Option<&ChildEnv>,
) -> Result<Outcome> {
    let start = Instant::now();
    let (mut child, traced) = spawn(program, args, cwd, capture, sup, env)?;
    let pid = child.id();
    sup.on_spawn(pid);

    // The pumps run on their own threads because a supervising backend needs
    // the calling thread for its event loop: ptrace requires every request to
    // come from the thread that owns the tracee, and a child blocked writing to
    // a full pipe would otherwise deadlock against a tracer that is waiting for
    // it to make a syscall.
    let pumps = capture.then(|| {
        let out = child.stdout.take().expect("piped");
        let err = child.stderr.take().expect("piped");
        (
            std::thread::spawn(move || pump(out, Sink::Out)),
            std::thread::spawn(move || pump(err, Sink::Err)),
        )
    });

    let wait = match traced.then(|| sup.wait(pid)).flatten() {
        Some(Ok(w)) => w,
        Some(Err(e)) => {
            // The tracer broke. The command is still running and its exit status
            // is still the answer the user needs.
            sup.disable(format!("{e:#}"));
            wait_normally(&mut child)?
        }
        None => wait_normally(&mut child)?,
    };

    let (stdout, stderr, truncated) = match pumps {
        Some((o, e)) => {
            let (o, ot) = o.join().unwrap_or_else(|_| Ok((Vec::new(), true)))?;
            let (e, et) = e.join().unwrap_or_else(|_| Ok((Vec::new(), true)))?;
            (o, e, ot || et)
        }
        None => (Vec::new(), Vec::new(), true),
    };

    Ok(Outcome {
        exit_code: wait.code,
        stdout,
        stderr,
        duration_ms: start.elapsed().as_millis() as u64,
        truncated,
        signaled: wait.signaled,
    })
}

/// Start the child, falling back to an untraced spawn if the traced one is
/// refused. Whatever a sandbox thinks of `ptrace`, the user's command runs.
fn spawn(
    program: &Path,
    args: &[String],
    cwd: &Path,
    capture: bool,
    sup: &mut dyn Supervisor,
    env: Option<&ChildEnv>,
) -> Result<(std::process::Child, bool)> {
    let build = || {
        let mut cmd = Command::new(program);
        cmd.args(args).current_dir(cwd);
        if capture {
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        }
        if let Some(e) = env {
            if e.clear {
                cmd.env_clear();
            }
            for (k, v) in &e.vars {
                cmd.env(k, v);
            }
        }
        cmd
    };
    if let Some(hook) = sup.pre_exec() {
        let mut cmd = build();
        install_hook(&mut cmd, hook);
        match cmd.spawn() {
            Ok(c) => return Ok((c, sup.traced())),
            Err(e) => sup.disable(format!("tracing could not be started: {e}")),
        }
    }
    let child = build()
        .spawn()
        .with_context(|| format!("Arc could not start `{}`.", program.display()))?;
    Ok((child, false))
}

#[cfg(unix)]
fn install_hook(cmd: &mut Command, hook: crate::trace::PreExec) {
    use std::os::unix::process::CommandExt;
    // SAFETY: `pre_exec` requires the closure to be async-signal-safe, because
    // it runs in the forked child before `exec` while the parent's threads and
    // locks are still notionally present. Every backend that supplies a hook
    // documents that its own is — see `trace::PreExec`.
    unsafe {
        cmd.pre_exec(hook);
    }
}

#[cfg(not(unix))]
fn install_hook(_cmd: &mut Command, _hook: crate::trace::PreExec) {}

fn wait_normally(child: &mut std::process::Child) -> Result<Wait> {
    let status = child.wait().context("waiting for child process")?;
    Ok(Wait {
        code: exit_code_of(&status),
        signaled: signaled(&status),
    })
}

enum Sink {
    Out,
    Err,
}

fn pump(mut src: impl Read, sink: Sink) -> Result<(Vec<u8>, bool)> {
    let mut buf = vec![0u8; 32 * 1024];
    let mut captured: Vec<u8> = Vec::new();
    let mut truncated = false;
    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            break;
        }
        match sink {
            Sink::Out => {
                let mut o = std::io::stdout().lock();
                o.write_all(&buf[..n])?;
                o.flush()?;
            }
            Sink::Err => {
                let mut e = std::io::stderr().lock();
                e.write_all(&buf[..n])?;
                e.flush()?;
            }
        }
        if captured.len() + n <= MAX_CAPTURE {
            captured.extend_from_slice(&buf[..n]);
        } else {
            truncated = true;
        }
    }
    Ok((captured, truncated))
}

/// Run a command to completion, capturing its output instead of streaming it.
///
/// Used by the scheduler, where several tasks run at once and interleaving
/// their output onto one terminal would make all of it unreadable.
pub fn capture(
    program: &Path,
    args: &[String],
    cwd: &Path,
    env: &[(&str, &std::ffi::OsStr)],
) -> Result<Outcome> {
    let start = Instant::now();
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd
        .output()
        .with_context(|| format!("Arc could not start `{}`.", program.display()))?;
    Ok(Outcome {
        exit_code: exit_code_of(&out.status),
        stdout: out.stdout,
        stderr: out.stderr,
        duration_ms: start.elapsed().as_millis() as u64,
        truncated: false,
        signaled: signaled(&out.status),
    })
}

/// Replay captured output on a cache hit, byte for byte.
pub fn replay(stdout: &[u8], stderr: &[u8]) -> Result<()> {
    let mut o = std::io::stdout().lock();
    o.write_all(stdout)?;
    o.flush()?;
    let mut e = std::io::stderr().lock();
    e.write_all(stderr)?;
    e.flush()?;
    Ok(())
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
