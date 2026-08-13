//! The run pipeline.
//!
//! ```text
//! discover project ─▶ normalise command ─▶ family identity
//!        │
//!        ▼
//! load learned dependencies ──▶ validate ──▶ (unsafe → discard)
//!        │
//!        ▼
//! fingerprint inputs ─▶ execution key ─▶ lookup
//!        │                                 │
//!        │                            hit ─┴─ miss
//!        ▼                             │      │
//!     restore  ◀────────────────────────      ▼
//!                                          execute + trace
//!                                             │
//!                                             ▼
//!                                       learn dependencies
//! ```
//!
//! Each stage below is a function of the previous stage's output, so the
//! ordering constraint that matters — the execution key is fixed *before* the
//! command runs, from knowledge stored *before* the run — is visible rather
//! than implied.

use crate::db::Db;
use crate::dependency::{Completeness, DependencySet};
use crate::exec;
use crate::family;
use crate::hash::Digest;
use crate::key::{self, EnvFingerprint, KeyInputs, Toolchain};
use crate::outputs;
use crate::paths::Classifier;
use crate::project::{Config, Project};
use crate::record::{
    format_command, BlobRef, CacheEntry, CacheStatus, ExecutionRecord, OutputFile, TraceSummary,
};
use crate::scan::{self, InputSet};
use crate::store::Store;
use crate::trace;
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
    /// Force observation even on a cache hit, and report what was seen.
    pub trace: bool,
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
    pub inputs_narrowed: bool,
    pub fingerprint_ms: u64,
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
}

/// One entry per input file, stored as a blob so `--explain` can say which file
/// changed without bloating every execution record.
#[derive(Serialize, Deserialize)]
struct InputManifest {
    files: Vec<(String, String)>,
}

/// Everything fixed before the command can run.
struct Plan {
    cfg: Config,
    classifier: Classifier,
    family_key: String,
    resolved: PathBuf,
    rel_cwd: String,
    project_id: String,
    command_line: String,
    deps: DependencySet,
    dep_state: Completeness,
}

pub fn run(
    project: &Project,
    cwd: &Path,
    program: &str,
    args: &[String],
    opts: &RunOptions,
    arc_home: &Path,
) -> Result<RunReport> {
    let store = Store::open(arc_home)?;
    let db = Db::open(arc_home)?;
    let plan = plan(project, cwd, program, args, arc_home, &db)?;

    let cacheable = !opts.no_cache && plan.cfg.cache.enabled && !opts.no_capture;
    if !cacheable {
        return bypass(project, cwd, program, args, opts, &db, &plan);
    }

    // ---- fingerprint -------------------------------------------------------
    let fp_start = Instant::now();
    let mut fps = db.load_fingerprints(&plan.project_id)?;
    let skip = [arc_home.to_path_buf()];
    let inputs = scan::scan_inputs(&project.root, &plan.cfg, &mut fps, &skip)?;
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
        reused_fingerprints: inputs.reused_fingerprints,
        env_digest: env.digest.clone(),
        toolchain_digest: toolchain.digest.clone(),
        dependency_digest: dep_digest.hex(),
        execution_key: exec_key.hex(),
        result: "cache miss".into(),
        reason: String::new(),
        changed: Vec::new(),
        ignored_changes: Vec::new(),
        dependency_state: plan.dep_state.label().into(),
        inputs_narrowed: plan.deps.inputs_are_narrowed(),
        fingerprint_ms,
    };

    // ---- lookup ------------------------------------------------------------
    if !opts.refresh {
        if let Some((entry, prev)) = db.lookup(&exec_key.hex())? {
            match try_replay(&store, project, &prev) {
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
                        replayed_from: Some(prev.id.clone()),
                        ..prev.clone()
                    };
                    db.put_execution(&record, None)?;
                    explain.result = "cache hit".into();
                    explain.reason = format!("all inputs match execution {}", short(&prev.id));
                    return Ok(RunReport {
                        record,
                        explain,
                        restore_ms,
                        saved_ms: saved,
                        restored_bytes,
                        restored_files,
                        observations: None,
                        dependencies: Some(plan.deps),
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
    let mut tracer = (plan.cfg.trace.enabled || opts.trace)
        .then(|| trace::start(&project.root, &plan.classifier))
        .flatten();
    let outcome = {
        let mut attach = |pid: u32| {
            if let Some(t) = tracer.as_mut() {
                let _ = t.attach(pid);
            }
        };
        exec::run(&plan.resolved, args, cwd, true, &mut attach)?
    };
    let observations = tracer.map(|t| {
        let caps = t.capabilities();
        (caps, t.name(), t.finish())
    });

    let captured_outputs: Vec<OutputFile> =
        outputs::capture(&project.root, &plan.cfg.outputs.include, &store)?;

    // ---- learn -------------------------------------------------------------
    let (trace_summary, learned) = match &observations {
        Some((caps, name, obs)) => {
            let fresh = DependencySet::from_observations(&plan.family_key, name, *caps, obs, now);
            let merged = learn(&db, &plan, fresh, now)?;
            (
                Some(TraceSummary {
                    backend: name.to_string(),
                    completeness: merged.completeness,
                    processes: obs.processes.len(),
                    files_observed: obs.files.len(),
                    inputs: merged.inputs.len(),
                    outputs: merged.outputs.len(),
                    executables: merged.executables.len(),
                    lossy: obs.lossy,
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
    let effective_key = match &learned {
        Some(merged) => key::execution_key(&KeyInputs {
            program,
            args,
            rel_cwd: &plan.rel_cwd,
            family_key: &plan.family_key,
            input_digest: &inputs.digest,
            env_digest: &env.digest,
            toolchain_digest: &toolchain.digest,
            dependency_digest: &merged.key_digest(),
            output_globs: &plan.cfg.outputs.include,
        }),
        None => exec_key,
    };

    let store_cacheable = !outcome.truncated
        && !outcome.signaled
        && (outcome.exit_code == 0 || opts.cache_failures || plan.cfg.cache.cache_failures);

    let (stdout, stderr, manifest) = if store_cacheable {
        let manifest = InputManifest {
            files: inputs
                .files
                .iter()
                .map(|f| (f.rel.clone(), f.digest.hex()))
                .collect(),
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

    Ok(RunReport {
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
    let project_id = crate::hash::hash_bytes(project.root.to_string_lossy().as_bytes()).hex();
    let family_key = family::family_key(program, args, &rel_cwd, &cfg).hex();
    let classifier = Classifier::new(&project.root, arc_home);

    let caps = trace::platform_capabilities();
    let now = scan::now_millis();
    db.touch_family(&family::ExecutionFamily {
        key: family_key.clone(),
        program: program.to_string(),
        args: args.to_vec(),
        project_root: project.root.to_string_lossy().to_string(),
        rel_cwd: rel_cwd.clone(),
        first_seen: now,
        last_seen: now,
        runs: 1,
    })?;
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

    Ok(Plan {
        cfg,
        classifier,
        family_key,
        resolved,
        rel_cwd,
        project_id,
        command_line,
        deps,
        dep_state,
    })
}

/// Fold a fresh observation into stored knowledge and reindex the family.
fn learn(db: &Db, plan: &Plan, fresh: DependencySet, now: i64) -> Result<DependencySet> {
    let mut merged = if plan.dep_state == Completeness::Invalid || plan.deps.observations == 0 {
        fresh
    } else {
        let mut m = plan.deps.clone();
        m.merge(&fresh, now);
        m
    };
    merged.declared_inputs = plan.cfg.inputs.include.clone();

    // Only a family whose inputs are actually narrowed earns index rows.
    // Indexing a whole project would make `arc affected` answer "everything",
    // at the cost of a row per file per family.
    let indexed: Vec<String> = if merged.inputs_are_narrowed() {
        merged.inputs.clone()
    } else {
        Vec::new()
    };
    db.put_dependency_set(&plan.project_id, &merged, &indexed)?;

    Ok(merged)
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
    let outcome = exec::run(&plan.resolved, args, cwd, !opts.no_capture, &mut |_| {})?;
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
        inputs_narrowed: false,
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
    for d in prev.blob_digests() {
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
    inputs: &InputSet,
    env: &EnvFingerprint,
) -> Result<(String, Vec<String>)> {
    let history = db.history(200)?;
    let Some(prev) = history
        .into_iter()
        .find(|r| r.family_key == plan.family_key && r.cache_status != CacheStatus::Bypass)
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
            None => "project inputs changed".into(),
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
fn changed_inputs(store: &Store, prev: &ExecutionRecord, inputs: &InputSet) -> Result<Vec<String>> {
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
    let old_map: std::collections::HashMap<_, _> = old.files.into_iter().collect();
    let mut new_paths = std::collections::HashSet::new();
    for f in &inputs.files {
        new_paths.insert(f.rel.as_str());
        match old_map.get(&f.rel) {
            Some(d) if *d == f.digest.hex() => {}
            Some(_) => changed.push(format!("{} changed", f.rel)),
            None => changed.push(format!("{} added", f.rel)),
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
/// family's narrowed input set.
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
    let mut fps = scan::FingerprintMap::new();
    let all = scan::scan_inputs(&project.root, &scoped, &mut fps, &[arc_home.to_path_buf()])?;
    let considered = scan::build_globs(&cfg.inputs.include)?;
    Ok(all
        .files
        .into_iter()
        .filter(|f| !considered.is_match(&f.rel) && !deps.inputs.contains(&f.rel))
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
