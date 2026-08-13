//! Child process execution: direct spawn, streamed output, captured copy.

use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

/// Output beyond this is streamed but not cached: holding an unbounded build
/// log in memory to make it reusable is a bad trade.
pub const MAX_CAPTURE: usize = 64 * 1024 * 1024;

pub struct Outcome {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub duration_ms: u64,
    /// Output exceeded `MAX_CAPTURE`, or capture was off.
    pub truncated: bool,
    /// Killed by a signal; the result says nothing about the inputs.
    pub signaled: bool,
}

/// Run `program` with `args` in `cwd`. Output is streamed to this process's
/// stdout/stderr as it arrives and, when `capture` is set, copied into memory.
pub fn run(program: &Path, args: &[String], cwd: &Path, capture: bool) -> Result<Outcome> {
    let mut cmd = Command::new(program);
    cmd.args(args).current_dir(cwd);
    if capture {
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    }
    let start = Instant::now();
    let mut child = cmd
        .spawn()
        .with_context(|| format!("Arc could not start `{}`.", program.display()))?;

    let (stdout, stderr, truncated) = if capture {
        let out = child.stdout.take().expect("piped");
        let err = child.stderr.take().expect("piped");
        let h_out = std::thread::spawn(move || pump(out, Sink::Out));
        let h_err = std::thread::spawn(move || pump(err, Sink::Err));
        let (o, o_trunc) = h_out.join().unwrap()?;
        let (e, e_trunc) = h_err.join().unwrap()?;
        (o, e, o_trunc || e_trunc)
    } else {
        (Vec::new(), Vec::new(), true)
    };

    let status = child.wait().context("waiting for child process")?;
    Ok(Outcome {
        exit_code: exit_code_of(&status),
        stdout,
        stderr,
        duration_ms: start.elapsed().as_millis() as u64,
        truncated,
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
