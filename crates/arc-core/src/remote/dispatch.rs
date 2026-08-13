//! Running one cache miss on a worker.
//!
//! The result of a remote execution is an ordinary Arc execution: the same
//! exit status, the same captured output, the same output files. Everything
//! downstream — learning, recording, publishing, restoring — is the code that
//! already existed. Nothing here is a second kind of result.

use super::eligibility::Materialisation;
use super::execution::{
    self as wire, ExecutionRequest, ExecutionResult, FailureKind, JobState, Limits, ManifestEntry,
    ToolRequirement,
};
use super::protocol::WirePath;
use super::{Executor, Remote};
use crate::hash::Digest;
use crate::record::OutputFile;
use crate::store::Store;
use anyhow::{anyhow, bail, Result};
use std::path::Path;
use std::time::Instant;

/// What a remote execution actually did, in the shape the engine already knows.
pub struct RemoteOutcome {
    pub exit_code: i32,
    pub signaled: bool,
    pub duration_ms: u64,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub truncated: bool,
    pub outputs: Vec<OutputFile>,
    pub job_id: String,
    pub queued_ms: u64,
    /// The worker published this result to the shared cache, so other machines
    /// will find it without executing anything.
    pub published: bool,
    pub timings: Timings,
}

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct Timings {
    pub stage_ms: u64,
    pub upload_ms: u64,
    pub submit_ms: u64,
    pub queue_ms: u64,
    pub execute_ms: u64,
    pub fetch_ms: u64,
    pub total_ms: u64,
    pub input_bytes: u64,
    pub input_objects: usize,
    pub uploaded_objects: usize,
    pub output_bytes: u64,
}

pub enum Dispatched {
    Executed(Box<RemoteOutcome>),
    /// The worker could not run it, and the command may safely run here.
    Fallback(String),
    /// The command may be running elsewhere, so running it here could duplicate
    /// whatever it does.
    Unsafe(String),
}

pub struct Request<'a> {
    pub execution_key: &'a str,
    pub family_key: &'a str,
    pub program: &'a str,
    pub args: &'a [String],
    pub rel_cwd: &'a str,
    /// `(project-relative path, digest hex)` — exactly what was fingerprinted.
    pub inputs: &'a [(String, String)],
    pub materialisation: Materialisation,
    pub env: Vec<(String, String)>,
    pub tools: Vec<ToolRequirement>,
    pub output_globs: &'a [String],
    pub cache_failures: bool,
    pub limits: Limits,
}

/// Execute `req` on `executor`, publishing inputs through `cache`.
///
/// Every failure mode is reported rather than thrown away, because the caller's
/// next move depends on which one it was: a worker that cannot run the command
/// is a reason to run it locally, and a worker that timed out is not.
#[allow(clippy::too_many_arguments)]
pub fn execute(
    executor: &Executor,
    cache: &Remote,
    store: &Store,
    project_root: &Path,
    req: &Request<'_>,
    progress: &dyn Fn(&str),
    on_log: &mut dyn FnMut(&str),
    cancelled: &dyn Fn() -> bool,
) -> Result<Dispatched> {
    let started = Instant::now();
    let mut timings = Timings::default();

    progress("staging inputs");
    let stage = Instant::now();
    let manifest = stage_inputs(project_root, req.inputs, store)?;
    timings.stage_ms = stage.elapsed().as_millis() as u64;
    timings.input_objects = manifest.len();
    timings.input_bytes = manifest.iter().map(|m| m.size).sum();

    // The worker fetches inputs from the shared cache, so they have to be there
    // first. Only what the cache lacks crosses the wire.
    progress("uploading inputs");
    let upload = Instant::now();
    let digests: Vec<String> = {
        let mut v: Vec<String> = manifest.iter().map(|m| m.digest.clone()).collect();
        v.sort();
        v.dedup();
        v
    };
    let missing = cache.missing(&digests)?;
    timings.uploaded_objects = missing.len();
    cache.upload(store, &digests)?;
    timings.upload_ms = upload.elapsed().as_millis() as u64;

    let request = ExecutionRequest {
        protocol: wire::EXEC_PROTOCOL_VERSION,
        key_semantics: crate::SCHEMA_VERSION,
        execution_key: req.execution_key.to_string(),
        family_key: req.family_key.to_string(),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        program: req.program.to_string(),
        args: req.args.to_vec(),
        rel_cwd: req.rel_cwd.to_string(),
        env: req.env.clone(),
        inputs: manifest,
        output_globs: req.output_globs.to_vec(),
        tools: req.tools.clone(),
        limits: req.limits,
        cache_failures: req.cache_failures,
        arc_version: crate::VERSION.into(),
    };

    progress("submitting");
    let submit = Instant::now();
    let job = match executor.submit(&request) {
        Ok(j) => j,
        // A worker that refuses the job has not started anything, so running
        // the command here cannot duplicate it.
        Err(e) => return Ok(Dispatched::Fallback(e.to_string())),
    };
    timings.submit_ms = submit.elapsed().as_millis() as u64;

    progress("executing remotely");
    let job_id = job.id.clone();
    let job = match executor.wait(job, req.execution_key, |text| on_log(text), cancelled) {
        Ok(j) => j,
        Err(e) => {
            // The client stopped waiting; the command may still be running.
            return Ok(Dispatched::Unsafe(e.to_string()));
        }
    };
    timings.queue_ms = job.queued_ms;

    match job.state {
        JobState::Completed => {}
        JobState::Cancelled => return Ok(Dispatched::Unsafe("the job was cancelled".into())),
        JobState::Lost => {
            return Ok(Dispatched::Unsafe(
                "the worker was lost while the command was running".into(),
            ))
        }
        JobState::Failed => {
            let err = job
                .error
                .ok_or_else(|| anyhow!("worker reported failure without a reason"))?;
            let message = format!("{}: {}", err.kind.label(), err.message);
            return Ok(if err.kind.safe_to_run_locally() {
                Dispatched::Fallback(message)
            } else {
                Dispatched::Unsafe(message)
            });
        }
        JobState::Queued | JobState::Running => bail!("worker returned a non-terminal job"),
    }

    let result = job
        .result
        .ok_or_else(|| anyhow!("completed job carries no result"))?;
    result.validate().map_err(|e| anyhow!(e))?;
    timings.execute_ms = result.duration_ms;

    // Nothing the worker said is believed yet. Every object is fetched from the
    // cache and re-hashed on the way into this machine's store.
    progress("fetching result");
    let fetch = Instant::now();
    cache.download(store, &result.digests())?;
    timings.fetch_ms = fetch.elapsed().as_millis() as u64;

    let outputs = output_files(&result)?;
    for o in &outputs {
        let d = Digest::parse(&o.digest)?;
        if !store.exists(&d) {
            bail!(
                "remote result is incomplete: object {} is missing",
                d.short()
            );
        }
    }
    timings.output_bytes = outputs.iter().map(|o| o.size).sum();

    let read = |b: &Option<super::protocol::WireBlob>| -> Result<Vec<u8>> {
        match b {
            Some(b) => store.read(&Digest::parse(&b.digest)?),
            None => Ok(Vec::new()),
        }
    };
    timings.total_ms = started.elapsed().as_millis() as u64;

    Ok(Dispatched::Executed(Box::new(RemoteOutcome {
        exit_code: result.exit_code,
        signaled: result.signaled,
        duration_ms: result.duration_ms,
        stdout: read(&result.stdout)?,
        stderr: read(&result.stderr)?,
        truncated: result.truncated,
        outputs,
        job_id,
        queued_ms: job.queued_ms,
        published: result.published,
        timings,
    })))
}

fn output_files(result: &ExecutionResult) -> Result<Vec<OutputFile>> {
    let mut out = Vec::with_capacity(result.outputs.len());
    for o in &result.outputs {
        out.push(OutputFile {
            rel: o.path.decode().map_err(|e| anyhow!(e))?,
            digest: o.digest.clone(),
            size: o.size,
            exec: o.exec,
        });
    }
    Ok(out)
}

/// Put every input into the local store so it can be uploaded, and describe it
/// for the worker.
///
/// The digest recorded here is the one the fingerprint already computed; the
/// file is re-hashed by `put_file` on the way in, so a file changing underneath
/// Arc produces a mismatch rather than a silently different execution.
fn stage_inputs(
    root: &Path,
    inputs: &[(String, String)],
    store: &Store,
) -> Result<Vec<ManifestEntry>> {
    let mut out = Vec::with_capacity(inputs.len());
    for (rel, digest) in inputs {
        let path = crate::outputs::safe_join(root, rel)?;
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            // A file that vanished between fingerprinting and staging cannot be
            // sent, and sending an incomplete input set is not an option.
            bail!("input `{rel}` disappeared while preparing a remote execution");
        };
        if !meta.is_file() {
            bail!("input `{rel}` is not a regular file");
        }
        let (actual, size) = store.put_file(&path)?;
        if actual.hex() != *digest {
            bail!("input `{rel}` changed while preparing a remote execution");
        }
        out.push(ManifestEntry {
            path: WirePath::from_rel(rel),
            digest: actual.hex(),
            size,
            exec: is_exec(&path),
        });
    }
    Ok(out)
}

#[cfg(unix)]
fn is_exec(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}
#[cfg(not(unix))]
fn is_exec(_p: &Path) -> bool {
    false
}

/// Map a worker failure onto what the caller may do next.
pub fn fallback_allowed(kind: FailureKind) -> bool {
    kind.safe_to_run_locally()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staging_refuses_an_input_that_changed_underneath_arc() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let store = Store::open(&root.join(".arc")).unwrap();
        std::fs::write(root.join("a.txt"), b"original").unwrap();
        let real = crate::hash::hash_file(&root.join("a.txt")).unwrap().hex();

        let ok = stage_inputs(root, &[("a.txt".into(), real.clone())], &store).unwrap();
        assert_eq!(ok.len(), 1);
        assert_eq!(ok[0].size, 8);

        std::fs::write(root.join("a.txt"), b"changed!").unwrap();
        assert!(stage_inputs(root, &[("a.txt".into(), real)], &store).is_err());
    }

    #[test]
    fn staging_refuses_paths_that_leave_the_project() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join(".arc")).unwrap();
        assert!(stage_inputs(tmp.path(), &[("../x".into(), "0".into())], &store).is_err());
        assert!(stage_inputs(tmp.path(), &[("/etc/passwd".into(), "0".into())], &store).is_err());
    }

    #[test]
    fn a_missing_input_stops_the_dispatch_rather_than_shrinking_it() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join(".arc")).unwrap();
        let e = stage_inputs(tmp.path(), &[("gone.txt".into(), "0".into())], &store)
            .unwrap_err()
            .to_string();
        assert!(e.contains("disappeared"), "{e}");
    }
}
