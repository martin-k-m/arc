//! The run pipeline.
//!
//! ```text
//! discover project ─▶ normalise command ─▶ family identity
//!        │
//!        ▼
//! load learned dependencies ──▶ validate ──▶ (unsafe → discard)
//!        │
//!        ▼
//!   can_narrow? ──yes──▶ fingerprint learned deps only
//!        │ no                    │
//!        ▼                       │
//! fingerprint whole project ─────┤
//!                                ▼
//!                         execution key ─▶ lookup
//!                                          │
//!                                     hit ─┴─ miss
//!                                      │      │
//!     restore  ◀────────────────────────      ▼
//!                                          execute + trace
//!                                             │
//!                                             ▼
//!                                       learn dependencies
//! ```
//!
//! Each stage is a function of the previous stage's output, so the ordering
//! constraint that matters — the execution key is fixed *before* the command
//! runs, from knowledge stored *before* the run — is visible rather than
//! implied.
//!
//! A cache hit never starts the tracer. Learned knowledge is only refreshed by
//! an execution, which is sound because a hit means every dependency Arc knows
//! about is in the state it was in when that knowledge was gathered.

use crate::db::Db;
use crate::dependency::{self, Completeness, DependencySet, Narrow};
use crate::exec::{self, Supervisor, Wait};
use crate::family;
use crate::hash::Digest;
use crate::key::{self, EnvFingerprint, KeyInputs, Toolchain};
use crate::outputs;
use crate::paths::Classifier;
use crate::project::{Config, Project};
use crate::record::{
    format_command, BlobRef, CacheEntry, CacheSource, CacheStatus, ExecutionRecord, OutputFile,
    TraceSummary,
};
use crate::remote::{self, Remote};
use crate::scan::{self, FingerprintMap};
use crate::store::Store;
use crate::trace::{self, Selection, Tracer};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// Neither read nor write the cache.
    pub no_cache: bool,
    /// Ignore any existing entry, execute, and overwrite it.
    pub refresh: bool,
    /// Give the child the terminal directly. Output cannot be replayed, so
    /// nothing is written to the cache.
    pub no_capture: bool,
    pub cache_failures: bool,
    /// Report what was observed.
    pub trace: bool,
    /// Pin a tracing backend, for testing the fallback path.
    pub backend: Selection,
    /// Ignore any configured remote cache for this run.
    pub no_remote: bool,
    /// Override the configured remote *execution* policy. `None` follows
    /// configuration; remote execution is off unless something turns it on.
    pub remote_execution: Option<bool>,
}

/// Why Arc decided what it decided, in a form both the terminal renderer and
/// `--json` consume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Explain {
    pub command: String,
    pub cwd: String,
    pub project_root: String,
    pub family_key: String,
    pub input_digest: String,
    pub input_files: usize,
    pub input_bytes_hashed: u64,
    pub reused_fingerprints: usize,
    pub env_digest: String,
    pub toolchain_digest: String,
    pub dependency_digest: String,
    pub execution_key: String,
    pub result: String,
    pub reason: String,
    pub changed: Vec<String>,
    /// Project changes Arc could prove were irrelevant. Only computed when the
    /// user explicitly asks, since it costs a second comparison pass.
    pub ignored_changes: Vec<String>,
    pub dependency_state: String,
    /// `local` or `remote` for a hit; `none` otherwise.
    #[serde(default)]
    pub cache_source: String,
    /// Where the command ran, when it ran: `local`, `remote`, or `none`.
    #[serde(default)]
    pub execution_source: String,
    /// Why remote execution was or was not used.
    #[serde(default)]
    pub remote_execution: String,
    pub inputs_narrowed: bool,
    /// Why narrowing was or was not applied, from the single gate.
    pub narrow_reason: String,
    pub fingerprint_ms: u64,
}

/// A place for the front end to say what Arc is doing while it does it.
///
/// The engine reports stages; it never decides how — or whether — they are
/// shown. That keeps a spinner out of the pipeline and out of piped output.
pub trait Progress {
    fn stage(&self, _label: &str) {}
    /// Output arriving from somewhere the terminal cannot see directly, such as
    /// a command running on a worker. Informational: nothing about correctness
    /// depends on it being shown.
    fn log(&self, _text: &str) {}
}

/// A run nobody is watching.
impl Progress for () {}

/// What the remote cache did during one run, when one was configured. A remote
/// is an optimisation, so everything here is reporting, never a result.
#[derive(Debug, Clone)]
pub struct RemoteStatus {
    pub endpoint: String,
    pub namespace: String,
    pub read: bool,
    pub write: bool,
    pub metrics: remote::Metrics,
    /// Why the remote could not be used, if it could not.
    pub error: Option<String>,
}

pub struct RunReport {
    pub record: ExecutionRecord,
    pub explain: Explain,
    pub restore_ms: u64,
    pub saved_ms: u64,
    pub restored_bytes: u64,
    pub restored_files: usize,
    /// Present when this run observed an execution.
    pub observations: Option<trace::Observations>,
    pub dependencies: Option<DependencySet>,
    pub remote: Option<RemoteStatus>,
    pub remote_execution: Option<RemoteExecStatus>,
}

/// What remote execution did, when it was considered at all.
#[derive(Debug, Clone)]
pub struct RemoteExecStatus {
    pub endpoint: String,
    /// Whether the command actually ran on a worker.
    pub used: bool,
    /// Eligibility verdict, or the failure that sent the work back here.
    pub reason: String,
    pub job_id: Option<String>,
    pub queued_ms: u64,
    pub published: bool,
    pub timings: Option<remote::dispatch::Timings>,
}

/// One entry per input file, stored as a blob so `--explain` can say which file
/// changed without bloating every execution record.
#[derive(Serialize, Deserialize)]
struct InputManifest {
    files: Vec<(String, String)>,
    /// Which input set this manifest describes. A narrowed manifest and a
    /// project-wide one cover different ground, so comparing them file by file
    /// reports every difference between the two *methods* as a change to the
    /// project — which is noise, and misleading noise at that.
    #[serde(default)]
    narrowed: bool,
}

/// The fingerprint of everything this run considers an input, from whichever
/// source was authorised.
struct Inputs {
    digest: Digest,
    files: Vec<(String, String)>,
    bytes_hashed: u64,
    reused: usize,
    narrowed: bool,
}

/// Everything fixed before the command can run.
struct Plan {
    cfg: Config,
    classifier: Classifier,
    family: family::ExecutionFamily,
    family_key: String,
    resolved: PathBuf,
    rel_cwd: String,
    project_id: String,
    command_line: String,
    deps: DependencySet,
    dep_state: Completeness,
    narrow: Narrow,
}

pub fn run(
    project: &Project,
    cwd: &Path,
    program: &str,
    args: &[String],
    opts: &RunOptions,
    arc_home: &Path,
    progress: &dyn Progress,
) -> Result<RunReport> {
    progress.stage("resolving project");
    let store = Store::open(arc_home)?;
    let db = Db::open(arc_home)?;
    let plan = plan(project, cwd, program, args, arc_home, &db)?;

    let cacheable = !opts.no_cache && plan.cfg.cache.enabled && !opts.no_capture;
    if !cacheable {
        return bypass(project, cwd, program, args, opts, &db, &plan);
    }

    // ---- fingerprint -------------------------------------------------------
    progress.stage(if plan.narrow.allowed() {
        "fingerprinting learned dependencies"
    } else {
        "fingerprinting project"
    });
    let fp_start = Instant::now();
    let mut fps = db.load_fingerprints(&plan.project_id)?;
    let inputs = fingerprint_inputs(
        project,
        arc_home,
        &plan.cfg,
        &plan.deps,
        plan.narrow.allowed(),
        &mut fps,
    )?;
    let env = key::fingerprint_env(&plan.cfg);
    let toolchain = key::fingerprint_toolchain(program, cwd)?;
    let dep_digest = plan.deps.key_digest();
    let exec_key = key::execution_key(&KeyInputs {
        program,
        args,
        rel_cwd: &plan.rel_cwd,
        family_key: &plan.family_key,
        input_digest: &inputs.digest,
        env_digest: &env.digest,
        toolchain_digest: &toolchain.digest,
        dependency_digest: &dep_digest,
        output_globs: &plan.cfg.outputs.include,
    });
    let fingerprint_ms = fp_start.elapsed().as_millis() as u64;
    db.save_fingerprints(&plan.project_id, &fps)?;

    let mut explain = Explain {
        command: plan.command_line.clone(),
        cwd: cwd.to_string_lossy().to_string(),
        project_root: project.root.to_string_lossy().to_string(),
        family_key: plan.family_key.clone(),
        input_digest: inputs.digest.hex(),
        input_files: inputs.files.len(),
        input_bytes_hashed: inputs.bytes_hashed,
        reused_fingerprints: inputs.reused,
        env_digest: env.digest.clone(),
        toolchain_digest: toolchain.digest.clone(),
        dependency_digest: dep_digest.hex(),
        execution_key: exec_key.hex(),
        result: "cache miss".into(),
        reason: String::new(),
        changed: Vec::new(),
        ignored_changes: Vec::new(),
        dependency_state: plan.dep_state.label().into(),
        cache_source: "none".into(),
        execution_source: "none".into(),
        remote_execution: String::new(),
        // True whenever Arc has grounds to call some change irrelevant, whether
        // it earned them by observation or was told them in `arc.toml`.
        // `narrow_reason` says which.
        inputs_narrowed: inputs.narrowed || plan.deps.inputs_are_narrowed(),
        narrow_reason: plan.narrow.reason().into(),
        fingerprint_ms,
    };

    // ---- lookup ------------------------------------------------------------
    progress.stage("checking cache");
    let remote = (!opts.no_remote)
        .then(|| Remote::open(&plan.cfg.remote).ok())
        .flatten();
    let mut remote_error: Option<String> = None;
    let mut source = CacheSource::Local;

    if !opts.refresh {
        let mut local = db.lookup(&exec_key.hex())?;
        // A local hit never touches the network. The remote is consulted only
        // for work this machine would otherwise have to perform.
        if local.is_none() {
            if let Some(r) = remote.as_ref().filter(|r| r.read) {
                progress.stage("checking remote cache");
                match materialise_remote(r, &store, &db, &plan, program, args, &exec_key.hex()) {
                    Ok(true) => {
                        source = CacheSource::Remote;
                        local = db.lookup(&exec_key.hex())?;
                    }
                    Ok(false) => {}
                    Err(e) => remote_error = Some(e.to_string()),
                }
            }
        }
        if let Some((entry, prev)) = local {
            let mut replay = try_replay(&store, project, &prev);
            // A local record whose objects have gone missing can often be
            // completed from the remote, which beats re-running the command.
            if matches!(replay, Ok(None)) {
                if let Some(r) = remote.as_ref().filter(|r| r.read) {
                    progress.stage("repairing from remote cache");
                    match r.download(&store, &prev.replay_digests()) {
                        Ok(()) => {
                            source = CacheSource::Remote;
                            replay = try_replay(&store, project, &prev);
                        }
                        Err(e) => remote_error = Some(e.to_string()),
                    }
                }
            }
            match replay {
                Ok(Some((restored_files, restored_bytes, restore_ms))) => {
                    let now = scan::now_millis();
                    db.touch_entry(&exec_key.hex(), now)?;
                    let saved = entry.original_duration_ms.saturating_sub(restore_ms);
                    db.add_saved_ms(saved)?;
                    let record = ExecutionRecord {
                        id: new_id(&exec_key.hex(), now),
                        started_at: now,
                        duration_ms: restore_ms,
                        cache_status: CacheStatus::Hit,
                        cache_source: source,
                        replayed_from: Some(prev.id.clone()),
                        ..prev.clone()
                    };
                    db.put_execution(&record, None)?;
                    explain.result = "cache hit".into();
                    explain.cache_source = source.label().into();
                    explain.reason = match source {
                        CacheSource::Local => {
                            format!("all inputs match execution {}", short(&prev.id))
                        }
                        CacheSource::Remote => {
                            "all inputs match a result published to the remote cache".into()
                        }
                    };
                    return Ok(RunReport {
                        record,
                        explain,
                        restore_ms,
                        saved_ms: saved,
                        restored_bytes,
                        restored_files,
                        observations: None,
                        dependencies: Some(plan.deps),
                        remote: status(remote.as_ref(), remote_error),
                        remote_execution: None,
                    });
                }
                // A hit that cannot be safely served is a miss, never a guess.
                Ok(None) => {
                    explain.reason = "cached objects are missing; re-executing".into();
                    db.drop_entry(&exec_key.hex())?;
                }
                Err(e) => {
                    explain.reason = format!("cached result could not be restored: {e}");
                    db.drop_entry(&exec_key.hex())?;
                }
            }
        }
    }

    if explain.reason.is_empty() {
        let (reason, changed) = diff_reason(&db, &store, &plan, &inputs, &env)?;
        explain.reason = reason;
        explain.changed = changed;
    }

    // ---- execute and observe ----------------------------------------------
    // Nothing more is read from the database until the child exits, and holding
    // redb's exclusive lock across an arbitrarily long command would serialise
    // every other Arc process on this machine.
    db.release();
    let now = scan::now_millis();

    // Remote execution is only ever reached from here: after a local miss and a
    // remote miss. It never runs work that was already available.
    let mut remote_exec: Option<RemoteExecStatus> = None;
    let remote_run = try_remote_execution(
        opts,
        &plan,
        project,
        &store,
        remote.as_ref(),
        program,
        args,
        &exec_key.hex(),
        &inputs,
        &env,
        progress,
        &mut remote_exec,
    )?;
    explain.remote_execution = remote_exec
        .as_ref()
        .map(|s| s.reason.clone())
        .unwrap_or_else(|| "not enabled".into());

    let (outcome, observations, captured_outputs) = match remote_run {
        Some(r) => {
            progress.stage("restoring outputs");
            // The ordinary restore path, with the ordinary safety checks: a
            // worker's result is not a special kind of result.
            outputs::restore(&project.root, &r.outputs, &store)?;
            exec::replay(&r.stdout, &r.stderr)?;
            explain.execution_source = "remote".into();
            (
                exec::Outcome {
                    exit_code: r.exit_code,
                    stdout: r.stdout,
                    stderr: r.stderr,
                    duration_ms: r.duration_ms,
                    truncated: r.truncated,
                    signaled: r.signaled,
                },
                None,
                r.outputs,
            )
        }
        None => {
            progress.stage("executing");
            let tracer = (plan.cfg.trace.enabled || opts.trace)
                .then(|| trace::start(&project.root, &plan.classifier, opts.backend))
                .flatten();
            let mut sup = TraceSupervisor {
                tracer,
                failures: Vec::new(),
            };
            let outcome = exec::run(&plan.resolved, args, cwd, true, &mut sup)?;
            let observations = sup.collect();
            explain.execution_source = "local".into();
            progress.stage("capturing outputs");
            let captured: Vec<OutputFile> =
                outputs::capture(&project.root, &plan.cfg.outputs.include, &store)?;
            (outcome, observations, captured)
        }
    };

    // ---- learn -------------------------------------------------------------
    progress.stage("learning dependencies");
    let mut learned_node = None;
    let (trace_summary, learned) = match &observations {
        Some((caps, name, obs)) => {
            let fresh = DependencySet::from_observations(
                &plan.family_key,
                name,
                *caps,
                obs,
                &project.root,
                now,
            );
            let (merged, node) = learn(&db, &plan, fresh, now)?;
            learned_node = Some(node);
            (
                Some(TraceSummary {
                    backend: name.to_string(),
                    completeness: merged.completeness,
                    processes: obs.processes.len(),
                    files_observed: obs.files.len(),
                    inputs: merged.inputs.len() + merged.external.len(),
                    directories: merged.directories.len(),
                    absent: merged.existence.len(),
                    outputs: merged.outputs.len(),
                    executables: merged.executables.len(),
                    lossy: obs.lossy,
                    downgrades: merged.downgrades.iter().map(|d| d.describe()).collect(),
                }),
                Some(merged),
            )
        }
        None => (None, None),
    };

    // Learning changes what the *next* run will compute, so the entry is filed
    // under the key that run will ask for. Without this, first observing a
    // dependency would guarantee a miss on the following run — correct, but a
    // needless one, and it would take two runs before anything ever hit.
    // What the next run will compute, and therefore what this result must be
    // filed under — key *and* manifest together, so the stored record describes
    // one consistent view rather than a key over one input set and a file list
    // over another.
    let filed = match &learned {
        Some(merged) => post_learn_inputs(project, arc_home, &plan, merged, &inputs)?,
        None => None,
    };
    // Two things can differ from what this run computed: the learned dependency
    // digest, which grows as executables are observed, and — the first time
    // narrowing switches on — the input set itself.
    let next_inputs = filed.as_ref().unwrap_or(&inputs);
    let (effective_key, manifest_files, manifest_narrowed) = match &learned {
        Some(merged) => (
            key::execution_key(&KeyInputs {
                program,
                args,
                rel_cwd: &plan.rel_cwd,
                family_key: &plan.family_key,
                input_digest: &next_inputs.digest,
                env_digest: &env.digest,
                toolchain_digest: &toolchain.digest,
                dependency_digest: &merged.key_digest(),
                output_globs: &plan.cfg.outputs.include,
            }),
            next_inputs.files.clone(),
            next_inputs.narrowed,
        ),
        None => (exec_key, inputs.files.clone(), inputs.narrowed),
    };

    let store_cacheable = !outcome.truncated
        && !outcome.signaled
        && (outcome.exit_code == 0 || opts.cache_failures || plan.cfg.cache.cache_failures);

    let (stdout, stderr, manifest) = if store_cacheable {
        let manifest = InputManifest {
            files: manifest_files,
            narrowed: manifest_narrowed,
        };
        let mbytes = serde_json::to_vec(&manifest)?;
        (
            Some(blob(&store, &outcome.stdout)?),
            Some(blob(&store, &outcome.stderr)?),
            Some(blob(&store, &mbytes)?),
        )
    } else {
        (None, None, None)
    };

    let record = ExecutionRecord {
        schema: crate::SCHEMA_VERSION,
        id: new_id(&effective_key.hex(), now),
        key: effective_key.hex(),
        program: program.to_string(),
        args: args.to_vec(),
        project_root: project.root.to_string_lossy().to_string(),
        rel_cwd: plan.rel_cwd.clone(),
        started_at: now,
        duration_ms: outcome.duration_ms,
        exit_code: outcome.exit_code,
        input_digest: inputs.digest.hex(),
        input_file_count: inputs.files.len(),
        env,
        toolchain,
        stdout,
        stderr,
        outputs: captured_outputs,
        cache_status: CacheStatus::Miss,
        cache_source: CacheSource::Local,
        replayed_from: None,
        input_manifest: manifest,
        arc_version: crate::VERSION.to_string(),
        family_key: plan.family_key.clone(),
        trace: trace_summary,
    };

    let entry = store_cacheable.then(|| CacheEntry {
        execution_id: record.id.clone(),
        created_at: now,
        last_accessed: now,
        hits: 0,
        original_duration_ms: outcome.duration_ms,
    });
    db.put_execution(&record, entry.as_ref())?;
    db.record_duration(&plan.family_key, outcome.duration_ms)?;

    // The command has already succeeded. Publishing is best-effort from here:
    // a remote that rejects, times out or is simply absent changes nothing
    // about the result the user just got.
    if let Some(r) = remote.as_ref().filter(|r| r.write) {
        progress.stage("publishing to remote cache");
        if store_cacheable {
            if let Err(e) = publish(r, &store, &record, &exec_key.hex()) {
                remote_error = Some(e.to_string());
            }
        }
        // Dependency knowledge is published even when the result is not
        // cacheable: knowing what a failing test reads is still worth sharing.
        if let Some(node) = &learned_node {
            if let Err(e) = r.publish_task(&remote::task_from_node(node)) {
                remote_error.get_or_insert(e.to_string());
            }
        }
    }

    Ok(RunReport {
        remote: status(remote.as_ref(), remote_error),
        remote_execution: remote_exec,
        record,
        explain,
        restore_ms: 0,
        saved_ms: 0,
        restored_bytes: 0,
        restored_files: 0,
        observations: observations.map(|(_, _, o)| o),
        dependencies: learned,
    })
}

/// Decide whether this miss should run on a worker, and run it there if so.
///
/// Returns `None` for every reason there is — not configured, not eligible, not
/// compatible, worker refused — and the command then runs locally, which is
/// what would have happened without a worker at all. The one case that does not
/// fall back is a command that may still be running elsewhere: re-running it
/// here could duplicate whatever it does.
#[allow(clippy::too_many_arguments)]
fn try_remote_execution(
    opts: &RunOptions,
    plan: &Plan,
    project: &Project,
    store: &Store,
    cache: Option<&Remote>,
    program: &str,
    args: &[String],
    exec_key: &str,
    inputs: &Inputs,
    env: &EnvFingerprint,
    progress: &dyn Progress,
    status: &mut Option<RemoteExecStatus>,
) -> Result<Option<remote::dispatch::RemoteOutcome>> {
    let cfg = &plan.cfg.remote.execution;
    let wanted = opts.remote_execution.unwrap_or(cfg.enabled);
    if !wanted || opts.no_remote {
        return Ok(None);
    }
    let mut note = |endpoint: &str, reason: String| {
        *status = Some(RemoteExecStatus {
            endpoint: endpoint.to_string(),
            used: false,
            reason,
            job_id: None,
            queued_ms: 0,
            published: false,
            timings: None,
        });
        Ok(None::<remote::dispatch::RemoteOutcome>)
    };

    let executor = match remote::Executor::open(&plan.cfg.remote) {
        Ok(e) => e,
        Err(d) => return note("", d.reason()),
    };
    // Inputs travel through the shared cache, so a worker without one is a
    // worker Arc cannot feed.
    let Some(cache) = cache.filter(|c| c.write) else {
        return note(
            executor.endpoint(),
            "the remote cache is not writable".into(),
        );
    };

    progress.stage("checking worker");
    let caps = match executor.capabilities() {
        Ok(c) => c,
        Err(e) => return note(executor.endpoint(), format!("worker unavailable: {e}")),
    };

    let eligibility = remote::eligibility::can_remote_execute(
        &remote::eligibility::Candidate {
            program,
            args,
            rel_cwd: &plan.rel_cwd,
            deps: &plan.deps,
            narrow: &plan.narrow,
            dep_state: plan.dep_state,
            env,
            allow_env: &cfg.allow_env,
        },
        &caps,
    );
    let Some(materialisation) = eligibility.materialisation() else {
        return note(executor.endpoint(), eligibility.reason());
    };

    // The manifest is exactly what was fingerprinted, so the environment the
    // worker builds is the one the execution key describes.
    let complete = materialisation == remote::eligibility::Materialisation::Narrowed;
    // The *content* hash of the resolved executable, which is what a worker can
    // check against its own copy. The toolchain fingerprint is a composite over
    // the program name as well, so it means nothing on another machine.
    let program_digest = match crate::hash::hash_file(&plan.resolved) {
        Ok(d) => d.hex(),
        Err(e) => return note(executor.endpoint(), format!("{e}")),
    };
    let tools =
        remote::eligibility::tool_requirements(program, &program_digest, &plan.deps, complete);
    let names = remote::eligibility::transmittable_env(env, &cfg.allow_env);
    let sent_env: Vec<(String, String)> = names
        .iter()
        .filter_map(|n| std::env::var(n).ok().map(|v| (n.clone(), v)))
        .collect();

    let request = remote::dispatch::Request {
        execution_key: exec_key,
        family_key: &plan.family_key,
        program,
        args,
        rel_cwd: &plan.rel_cwd,
        inputs: &inputs.files,
        materialisation,
        env: sent_env,
        tools,
        output_globs: &plan.cfg.outputs.include,
        cache_failures: opts.cache_failures || plan.cfg.cache.cache_failures,
        limits: remote::execution::Limits {
            timeout_ms: cfg.timeout_ms,
            ..Default::default()
        },
    };

    let endpoint = executor.endpoint().to_string();
    let result = remote::dispatch::execute(
        &executor,
        cache,
        store,
        &project.root,
        &request,
        &|s| progress.stage(s),
        &mut |text| progress.log(text),
        &|| false,
    );
    match result {
        Ok(remote::dispatch::Dispatched::Executed(outcome)) => {
            *status = Some(RemoteExecStatus {
                endpoint,
                used: true,
                reason: format!("executed on {}", caps.worker),
                job_id: Some(outcome.job_id.clone()),
                queued_ms: outcome.queued_ms,
                published: outcome.published,
                timings: Some(outcome.timings.clone()),
            });
            Ok(Some(*outcome))
        }
        Ok(remote::dispatch::Dispatched::Fallback(reason)) => {
            note(&endpoint, format!("running locally: {reason}"))
        }
        // The command may still be running on the worker. Running it again here
        // could duplicate whatever it did — a deploy done twice, a package
        // published twice — so the run fails rather than guessing.
        Ok(remote::dispatch::Dispatched::Unsafe(reason)) => {
            *status = Some(RemoteExecStatus {
                endpoint,
                used: false,
                reason: format!("did not complete: {reason}"),
                job_id: None,
                queued_ms: 0,
                published: false,
                timings: None,
            });
            anyhow::bail!(
                "remote execution did not complete: {reason}

The command may still be running on the worker, so Arc will not run it again here.
Run it locally with --no-remote-execution."
            )
        }
        Err(e) => note(&endpoint, format!("running locally: {e:#}")),
    }
}

fn status(remote: Option<&Remote>, error: Option<String>) -> Option<RemoteStatus> {
    let r = remote?;
    Some(RemoteStatus {
        endpoint: r.endpoint().to_string(),
        namespace: r.namespace().to_string(),
        read: r.read,
        write: r.write,
        metrics: r.metrics(),
        error,
    })
}

/// Objects first, record last. A record is a promise that its objects can be
/// fetched, so it is only made once they can be.
///
/// The result is published under both keys this run is valid for. `record.key`
/// is what the *next* run of this family on this machine will ask for, once it
/// has today's learned dependencies. `computed` is what a machine that has
/// never run this family will ask for — which is every machine seeing the
/// project for the first time, and therefore exactly the case a shared cache
/// exists to serve. Both keys were computed from the same pre-execution state
/// of the same inputs, so the same result answers both.
fn publish(r: &Remote, store: &Store, record: &ExecutionRecord, computed: &str) -> Result<()> {
    let wire = remote::from_record(record);
    r.upload(store, &wire.digests())?;
    r.publish(&wire)?;
    if computed != record.key && !computed.is_empty() {
        let mut alias = wire;
        alias.execution_key = computed.to_string();
        r.publish(&alias)?;
    }
    Ok(())
}

/// Turn a remote result into a local cache entry, or nothing at all.
///
/// Returns `true` only when every object the record names is present locally
/// and verified, at which point the ordinary local-hit path takes over. A
/// partial download is discarded rather than written: half a result is not a
/// result.
fn materialise_remote(
    r: &Remote,
    store: &Store,
    db: &Db,
    plan: &Plan,
    program: &str,
    args: &[String],
    exec_key: &str,
) -> Result<bool> {
    let Some(wire) = r.lookup(exec_key)? else {
        return Ok(false);
    };
    r.download(store, &wire.digests())?;
    let now = scan::now_millis();
    let template = ExecutionRecord {
        schema: crate::SCHEMA_VERSION,
        id: new_id(exec_key, now),
        key: exec_key.to_string(),
        program: program.to_string(),
        args: args.to_vec(),
        project_root: plan.family.project_root.clone(),
        rel_cwd: plan.rel_cwd.clone(),
        started_at: now,
        duration_ms: 0,
        exit_code: 0,
        input_digest: String::new(),
        input_file_count: 0,
        env: EnvFingerprint {
            vars: Vec::new(),
            digest: String::new(),
        },
        toolchain: Toolchain {
            program: program.to_string(),
            resolved_path: None,
            digest: String::new(),
        },
        stdout: None,
        stderr: None,
        outputs: Vec::new(),
        cache_status: CacheStatus::Miss,
        cache_source: CacheSource::Remote,
        replayed_from: None,
        input_manifest: None,
        arc_version: crate::VERSION.to_string(),
        family_key: plan.family_key.clone(),
        trace: None,
    };
    let base = remote::to_record(&wire, &template)?;
    for d in base.replay_digests() {
        if !store.exists(&Digest::parse(&d)?) {
            anyhow::bail!("remote result is incomplete");
        }
    }
    db.put_execution(
        &base,
        Some(&CacheEntry {
            execution_id: base.id.clone(),
            created_at: now,
            last_accessed: now,
            hits: 0,
            original_duration_ms: wire.duration_ms,
        }),
    )?;
    Ok(true)
}

/// Adapts a tracer to `exec`'s lifecycle hooks, and records the fact if it
/// broke. A tracer failure must reach the dependency set — a run Arc could not
/// fully observe is a run whose knowledge must not be trusted to narrow.
struct TraceSupervisor {
    tracer: Option<Box<dyn Tracer>>,
    failures: Vec<String>,
}

impl Supervisor for TraceSupervisor {
    fn traced(&self) -> bool {
        self.tracer
            .as_ref()
            .is_some_and(|t| t.launch() == trace::Launch::Traced)
    }

    fn on_spawn(&mut self, pid: u32) {
        if let Some(t) = self.tracer.as_mut() {
            if let Err(e) = t.attach(pid) {
                self.failures.push(format!("attach failed: {e}"));
            }
        }
    }

    fn wait(&mut self, pid: u32) -> Option<Result<Wait>> {
        self.tracer.as_mut()?.supervise(pid)
    }

    fn disable(&mut self, reason: String) {
        self.failures.push(reason);
    }
}

impl TraceSupervisor {
    fn collect(self) -> Option<(trace::Capabilities, &'static str, trace::Observations)> {
        let t = self.tracer?;
        let caps = t.capabilities();
        let name = t.name();
        let mut obs = t.finish();
        for f in self.failures {
            obs.lossy = true;
            obs.downgrade(trace::Downgrade::BackendError(f));
        }
        Some((caps, name, obs))
    }
}

/// Fingerprint whatever this run is allowed to treat as its input set.
fn fingerprint_inputs(
    project: &Project,
    arc_home: &Path,
    cfg: &Config,
    deps: &DependencySet,
    narrowed: bool,
    fps: &mut FingerprintMap,
) -> Result<Inputs> {
    let skip = [arc_home.to_path_buf()];
    if narrowed {
        let fp = dependency::fingerprint(deps, &project.root, fps)?;
        let mut files = fp.files;
        let mut digest = fp.digest;
        let mut bytes = fp.bytes_hashed;
        let mut reused = fp.reused;
        // Declared inputs are additive, even here. `[[command]] inputs` is the
        // user asserting a fact about their build; a trace that did not happen
        // to read one of those files this time is not grounds to overrule them.
        if !cfg.inputs.include.is_empty() {
            let declared = scan::scan_inputs(&project.root, cfg, fps, &skip)?;
            let mut h = crate::hash::Hasher::new();
            h.field(digest.bytes());
            h.field(declared.digest.bytes());
            digest = h.finish();
            bytes += declared.bytes_hashed;
            reused += declared.reused_fingerprints;
            let known: std::collections::HashSet<&String> = files.iter().map(|(p, _)| p).collect();
            let extra: Vec<(String, String)> = declared
                .files
                .iter()
                .filter(|f| !known.contains(&f.rel))
                .map(|f| (f.rel.clone(), f.digest.hex()))
                .collect();
            files.extend(extra);
        }
        return Ok(Inputs {
            digest,
            files,
            bytes_hashed: bytes,
            reused,
            narrowed: true,
        });
    }
    let set = scan::scan_inputs(&project.root, cfg, fps, &skip)?;
    Ok(Inputs {
        digest: set.digest,
        files: set
            .files
            .iter()
            .map(|f| (f.rel.clone(), f.digest.hex()))
            .collect(),
        bytes_hashed: set.bytes_hashed,
        reused: set.reused_fingerprints,
        narrowed: false,
    })
}

/// The input set the *next* run of this family will fingerprint, given what was
/// just learned.
///
/// After a first complete trace the next run will narrow, and a narrowed
/// fingerprint is a different digest over different ground. Filing this result
/// under the old key would guarantee a miss the moment narrowing switches on.
///
/// `None` means keep what this run already computed.
fn post_learn_inputs(
    project: &Project,
    arc_home: &Path,
    plan: &Plan,
    merged: &DependencySet,
    inputs: &Inputs,
) -> Result<Option<Inputs>> {
    // An execution that rewrote something it read leaves the filesystem in a
    // state it never actually consumed. Fingerprinting *now* would file this
    // result under a key describing the world after it ran, and the next run
    // would replay a result produced from different input. That is a false hit,
    // so such a run keeps what it computed before it started.
    if !merged.self_modified.is_empty() {
        return Ok(None);
    }
    let caps = trace::platform_capabilities();
    let next_narrow = dependency::can_narrow(merged, &caps, &plan.family_key).allowed();
    if next_narrow == inputs.narrowed {
        return Ok(None);
    }
    Ok(Some(fingerprint_inputs(
        project,
        arc_home,
        &plan.cfg,
        merged,
        next_narrow,
        &mut FingerprintMap::new(),
    )?))
}

/// Everything decided before the command may run.
fn plan(
    project: &Project,
    cwd: &Path,
    program: &str,
    args: &[String],
    arc_home: &Path,
    db: &Db,
) -> Result<Plan> {
    let command_line = format_command(program, args);
    let cfg = project.config_for(&command_line)?;
    let resolved = key::find_program_or_explain(program, cwd)?;
    let rel_cwd = key::rel_cwd(&project.root, cwd);
    let project_id = crate::project_id(&project.root);
    let family_key = family::family_key(program, args, &rel_cwd, &cfg).hex();
    let classifier = Classifier::new(&project.root, arc_home);

    let caps = trace::platform_capabilities();
    let now = scan::now_millis();
    let family = family::ExecutionFamily {
        key: family_key.clone(),
        program: program.to_string(),
        args: args.to_vec(),
        project_root: crate::paths::display_form(&project.root),
        rel_cwd: rel_cwd.clone(),
        first_seen: now,
        last_seen: now,
        runs: 1,
    };
    db.touch_family(&family)?;
    // Knowledge that fails validation is discarded rather than repaired: a
    // dependency set Arc cannot vouch for is worth exactly as much as none.
    let (deps, dep_state) = match db.dependency_set(&family_key)? {
        Some(set) => match set.validate(&family_key, &caps) {
            Completeness::Invalid => (
                DependencySet::empty(&family_key, now),
                Completeness::Invalid,
            ),
            state => (set, state),
        },
        None => (
            DependencySet::empty(&family_key, now),
            Completeness::Unsupported,
        ),
    };
    // The single narrowing decision for this run. Everything downstream reads
    // it; nothing re-derives it.
    let narrow = dependency::can_narrow(&deps, &caps, &family_key);

    Ok(Plan {
        cfg,
        classifier,
        family,
        family_key,
        resolved,
        rel_cwd,
        project_id,
        command_line,
        deps,
        dep_state,
        narrow,
    })
}

/// Fold a fresh observation into stored knowledge and reindex the family.
fn learn(
    db: &Db,
    plan: &Plan,
    fresh: DependencySet,
    now: i64,
) -> Result<(DependencySet, crate::graph::TaskNode)> {
    let mut merged = if plan.dep_state == Completeness::Invalid || plan.deps.observations == 0 {
        fresh
    } else {
        let mut m = plan.deps.clone();
        m.merge(&fresh, now);
        m
    };
    merged.declared_inputs = plan.cfg.inputs.include.clone();
    let node = crate::graph::TaskNode::from_family(&plan.family, &merged, &plan.cfg);
    db.put_dependency_set(&plan.project_id, &merged, &node)?;
    Ok((merged, node))
}

/// Run without touching cache state, but still record what happened.
fn bypass(
    project: &Project,
    cwd: &Path,
    program: &str,
    args: &[String],
    opts: &RunOptions,
    db: &Db,
    plan: &Plan,
) -> Result<RunReport> {
    db.release();
    let now = scan::now_millis();
    let outcome = exec::run(&plan.resolved, args, cwd, !opts.no_capture, &mut ())?;
    let record = ExecutionRecord {
        schema: crate::SCHEMA_VERSION,
        id: new_id(program, now),
        key: String::new(),
        program: program.to_string(),
        args: args.to_vec(),
        project_root: project.root.to_string_lossy().to_string(),
        rel_cwd: plan.rel_cwd.clone(),
        started_at: now,
        duration_ms: outcome.duration_ms,
        exit_code: outcome.exit_code,
        input_digest: String::new(),
        input_file_count: 0,
        env: EnvFingerprint {
            vars: Vec::new(),
            digest: String::new(),
        },
        toolchain: Toolchain {
            program: program.to_string(),
            resolved_path: Some(plan.resolved.to_string_lossy().to_string()),
            digest: String::new(),
        },
        stdout: None,
        stderr: None,
        outputs: Vec::new(),
        cache_status: CacheStatus::Bypass,
        cache_source: CacheSource::Local,
        replayed_from: None,
        input_manifest: None,
        arc_version: crate::VERSION.to_string(),
        family_key: plan.family_key.clone(),
        trace: None,
    };
    db.put_execution(&record, None)?;
    let explain = Explain {
        command: plan.command_line.clone(),
        cwd: cwd.to_string_lossy().to_string(),
        project_root: record.project_root.clone(),
        family_key: plan.family_key.clone(),
        input_digest: String::new(),
        input_files: 0,
        input_bytes_hashed: 0,
        reused_fingerprints: 0,
        env_digest: String::new(),
        toolchain_digest: String::new(),
        dependency_digest: String::new(),
        execution_key: String::new(),
        result: "bypass".into(),
        reason: "caching disabled for this run".into(),
        changed: Vec::new(),
        ignored_changes: Vec::new(),
        dependency_state: plan.dep_state.label().into(),
        cache_source: "none".into(),
        execution_source: "local".into(),
        remote_execution: "not enabled".into(),
        inputs_narrowed: false,
        narrow_reason: plan.narrow.reason().into(),
        fingerprint_ms: 0,
    };
    Ok(RunReport {
        record,
        explain,
        restore_ms: 0,
        saved_ms: 0,
        restored_bytes: 0,
        restored_files: 0,
        observations: None,
        dependencies: None,
        remote: None,
        remote_execution: None,
    })
}

/// Replay a cached execution. `Ok(None)` means the entry is unusable and the
/// command must run; nothing has been written to the project in that case.
fn try_replay(
    store: &Store,
    project: &Project,
    prev: &ExecutionRecord,
) -> Result<Option<(usize, u64, u64)>> {
    let start = Instant::now();
    for d in prev.replay_digests() {
        if !store.exists(&Digest::parse(&d)?) {
            return Ok(None);
        }
    }
    let restored_bytes = outputs::restore(&project.root, &prev.outputs, store)?;
    let out = match &prev.stdout {
        Some(b) => store.read(&Digest::parse(&b.digest)?)?,
        None => Vec::new(),
    };
    let err = match &prev.stderr {
        Some(b) => store.read(&Digest::parse(&b.digest)?)?,
        None => Vec::new(),
    };
    exec::replay(&out, &err)?;
    Ok(Some((
        prev.outputs.len(),
        restored_bytes,
        start.elapsed().as_millis() as u64,
    )))
}

/// Explain a miss by diffing against the most recent run of the same family.
fn diff_reason(
    db: &Db,
    store: &Store,
    plan: &Plan,
    inputs: &Inputs,
    env: &EnvFingerprint,
) -> Result<(String, Vec<String>)> {
    let history = db.history(200)?;
    // Only executions, not replays. A hit's record is a copy of the execution it
    // replayed, so its input manifest describes that older run — which may have
    // been fingerprinted over entirely different ground, before Arc learned
    // enough to narrow.
    let Some(prev) = history
        .into_iter()
        .find(|r| r.family_key == plan.family_key && r.cache_status == CacheStatus::Miss)
    else {
        return Ok((
            "no previous execution of this command was recorded".into(),
            Vec::new(),
        ));
    };

    if prev.input_digest != inputs.digest.hex() {
        let mut changed = changed_inputs(store, &prev, inputs)?;
        changed.sort();
        let reason = match changed.first() {
            Some(first) if changed.len() == 1 => first.clone(),
            Some(first) => format!("{first} (and {} more)", changed.len() - 1),
            None if inputs.narrowed => {
                "Arc narrowed this command's inputs for the first time".into()
            }
            None => "tracked inputs changed".into(),
        };
        return Ok((reason, changed.into_iter().take(20).collect()));
    }
    if prev.env.digest != env.digest {
        let changed: Vec<String> = env
            .vars
            .iter()
            .filter(|v| {
                !prev
                    .env
                    .vars
                    .iter()
                    .any(|p| p.name == v.name && p.value_digest == v.value_digest)
            })
            .map(|v| format!("{} changed", v.name))
            .collect();
        let reason = changed
            .first()
            .cloned()
            .unwrap_or_else(|| "environment changed".into());
        return Ok((reason, changed));
    }
    Ok((
        "toolchain, observed dependencies or execution policy changed".into(),
        Vec::new(),
    ))
}

/// `path changed|added|removed` lines, by comparing against the stored input
/// manifest of a previous execution.
fn changed_inputs(store: &Store, prev: &ExecutionRecord, inputs: &Inputs) -> Result<Vec<String>> {
    let mut changed = Vec::new();
    let Some(m) = &prev.input_manifest else {
        return Ok(changed);
    };
    let Ok(bytes) = store.read(&Digest::parse(&m.digest)?) else {
        return Ok(changed);
    };
    let Ok(old) = serde_json::from_slice::<InputManifest>(&bytes) else {
        return Ok(changed);
    };
    if old.narrowed != inputs.narrowed {
        return Ok(changed);
    }
    let old_map: std::collections::HashMap<_, _> = old.files.into_iter().collect();
    let mut new_paths = std::collections::HashSet::new();
    for (rel, digest) in &inputs.files {
        new_paths.insert(rel.as_str());
        match old_map.get(rel) {
            Some(d) if d == digest => {}
            Some(_) => changed.push(format!("{rel} changed")),
            None => changed.push(format!("{rel} added")),
        }
    }
    for path in old_map.keys() {
        if !new_paths.contains(path.as_str()) {
            changed.push(format!("{path} removed"));
        }
    }
    Ok(changed)
}

/// Project files that changed since the cached execution but are outside this
/// family's input set.
///
/// Only meaningful when the family's inputs are narrowed; otherwise Arc has no
/// grounds to call anything irrelevant and returns nothing. Deliberately a
/// separate pass so a normal cache hit never pays for it.
pub fn ignored_changes(
    project: &Project,
    arc_home: &Path,
    command_line: &str,
    family_key: &str,
) -> Result<Vec<String>> {
    let db = Db::open(arc_home)?;
    let Some(deps) = db.dependency_set(family_key)? else {
        return Ok(Vec::new());
    };
    if !deps.inputs_are_narrowed() {
        return Ok(Vec::new());
    }
    let cfg = project.config_for(command_line)?;
    let mut scoped = cfg.clone();
    scoped.inputs.include.clear();
    let mut fps = FingerprintMap::new();
    let all = scan::scan_inputs(&project.root, &scoped, &mut fps, &[arc_home.to_path_buf()])?;
    let considered = scan::build_globs(&cfg.inputs.include)?;
    let dirs: Vec<&String> = deps.directories.iter().collect();
    Ok(all
        .files
        .into_iter()
        .filter(|f| {
            !considered.is_match(&f.rel)
                && !deps.inputs.contains(&f.rel)
                // A file inside an enumerated directory is covered by that
                // directory's entry set, so it is not "outside the inputs".
                && !dirs.iter().any(|d| f.rel.starts_with(&format!("{d}/")))
        })
        .map(|f| f.rel)
        .collect())
}

fn blob(store: &Store, bytes: &[u8]) -> Result<BlobRef> {
    Ok(BlobRef {
        digest: store.put_bytes(bytes)?.hex(),
        size: bytes.len() as u64,
    })
}

fn new_id(seed: &str, now: i64) -> String {
    let mut h = crate::hash::Hasher::new();
    h.field(seed);
    h.field(now.to_le_bytes());
    h.field(std::process::id().to_le_bytes());
    h.finish().hex()[..10].to_string()
}

pub fn short(id: &str) -> &str {
    &id[..id.len().min(6)]
}
