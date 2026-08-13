//! The run pipeline: fingerprint, look up, restore or execute, record.

use crate::db::Db;
use crate::exec;
use crate::hash::Digest;
use crate::key::{self, EnvFingerprint, KeyInputs, Toolchain};
use crate::outputs;
use crate::project::Project;
use crate::record::{BlobRef, CacheEntry, CacheStatus, ExecutionRecord, OutputFile};
use crate::scan::{self, InputSet};
use crate::store::Store;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Explain {
    pub command: String,
    pub cwd: String,
    pub project_root: String,
    pub input_digest: String,
    pub input_files: usize,
    pub input_bytes_hashed: u64,
    pub reused_fingerprints: usize,
    pub env_digest: String,
    pub toolchain_digest: String,
    pub execution_key: String,
    pub result: String,
    pub reason: String,
    pub changed: Vec<String>,
    pub fingerprint_ms: u64,
}

pub struct RunReport {
    pub record: ExecutionRecord,
    pub explain: Explain,
    pub restore_ms: u64,
    pub saved_ms: u64,
    pub restored_bytes: u64,
    pub restored_files: usize,
}

/// One entry per input file, stored as a blob so `--explain` can say which file
/// changed without bloating every execution record.
#[derive(Serialize, Deserialize)]
struct InputManifest {
    files: Vec<(String, String)>,
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
    let resolved = key::find_program_or_explain(program, cwd)?;
    let rel_cwd = key::rel_cwd(&project.root, cwd);
    let project_id = crate::hash::hash_bytes(project.root.to_string_lossy().as_bytes()).hex();

    let cacheable = !opts.no_cache && project.config.cache.enabled && !opts.no_capture;

    // Bypass: run and record, but never read or write cache state.
    if !cacheable {
        let now = scan::now_millis();
        let outcome = exec::run(&resolved, args, cwd, !opts.no_capture)?;
        let record = ExecutionRecord {
            schema: crate::SCHEMA_VERSION,
            id: new_id(program, now),
            key: String::new(),
            program: program.to_string(),
            args: args.to_vec(),
            project_root: project.root.to_string_lossy().to_string(),
            rel_cwd,
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
                resolved_path: Some(resolved.to_string_lossy().to_string()),
                digest: String::new(),
            },
            stdout: None,
            stderr: None,
            outputs: Vec::new(),
            cache_status: CacheStatus::Bypass,
            replayed_from: None,
            input_manifest: None,
            arc_version: crate::VERSION.to_string(),
        };
        db.put_execution(&record, None)?;
        let explain = Explain {
            command: format!("{program} {}", args.join(" ")),
            cwd: cwd.to_string_lossy().to_string(),
            project_root: record.project_root.clone(),
            input_digest: String::new(),
            input_files: 0,
            input_bytes_hashed: 0,
            reused_fingerprints: 0,
            env_digest: String::new(),
            toolchain_digest: String::new(),
            execution_key: String::new(),
            result: "bypass".into(),
            reason: "caching disabled for this run".into(),
            changed: Vec::new(),
            fingerprint_ms: 0,
        };
        return Ok(RunReport {
            record,
            explain,
            restore_ms: 0,
            saved_ms: 0,
            restored_bytes: 0,
            restored_files: 0,
        });
    }

    // Fingerprint.
    let fp_start = Instant::now();
    let mut fps = db.load_fingerprints(&project_id)?;
    // Arc writes to its own home during the run; it can never be an input.
    let skip = [arc_home.to_path_buf()];
    let inputs: InputSet = scan::scan_inputs(&project.root, &project.config, &mut fps, &skip)?;
    let env = key::fingerprint_env(&project.config);
    let toolchain = key::fingerprint_toolchain(program, cwd)?;
    let exec_key = key::execution_key(&KeyInputs {
        program,
        args,
        rel_cwd: &rel_cwd,
        input_digest: &inputs.digest,
        env_digest: &env.digest,
        toolchain_digest: &toolchain.digest,
        output_globs: &project.config.outputs.include,
    });
    let fingerprint_ms = fp_start.elapsed().as_millis() as u64;
    db.save_fingerprints(&project_id, &fps)?;

    let mut explain = Explain {
        command: format!("{program} {}", args.join(" ")),
        cwd: cwd.to_string_lossy().to_string(),
        project_root: project.root.to_string_lossy().to_string(),
        input_digest: inputs.digest.hex(),
        input_files: inputs.files.len(),
        input_bytes_hashed: inputs.bytes_hashed,
        reused_fingerprints: inputs.reused_fingerprints,
        env_digest: env.digest.clone(),
        toolchain_digest: toolchain.digest.clone(),
        execution_key: exec_key.hex(),
        result: "cache miss".into(),
        reason: String::new(),
        changed: Vec::new(),
        fingerprint_ms,
    };

    // Lookup.
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
        let (reason, changed) = diff_reason(&db, &store, program, args, &rel_cwd, &inputs, &env)?;
        explain.reason = reason;
        explain.changed = changed;
    }

    // Execute.
    let now = scan::now_millis();
    let outcome = exec::run(&resolved, args, cwd, true)?;
    let captured_outputs: Vec<OutputFile> =
        outputs::capture(&project.root, &project.config.outputs.include, &store)?;

    let store_cacheable = !outcome.truncated
        && !outcome.signaled
        && (outcome.exit_code == 0 || opts.cache_failures || project.config.cache.cache_failures);

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
        id: new_id(&exec_key.hex(), now),
        key: exec_key.hex(),
        program: program.to_string(),
        args: args.to_vec(),
        project_root: project.root.to_string_lossy().to_string(),
        rel_cwd,
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

/// Explain a miss by diffing against the most recent run of the same command.
fn diff_reason(
    db: &Db,
    store: &Store,
    program: &str,
    args: &[String],
    rel_cwd: &str,
    inputs: &InputSet,
    env: &EnvFingerprint,
) -> Result<(String, Vec<String>)> {
    let history = db.history(200)?;
    let Some(prev) = history.into_iter().find(|r| {
        r.program == program
            && r.args == args
            && r.rel_cwd == rel_cwd
            && r.cache_status != CacheStatus::Bypass
    }) else {
        return Ok((
            "no previous execution of this command was recorded".into(),
            Vec::new(),
        ));
    };

    if prev.input_digest != inputs.digest.hex() {
        let mut changed = Vec::new();
        if let Some(m) = &prev.input_manifest {
            if let Ok(bytes) = store.read(&Digest::parse(&m.digest)?) {
                if let Ok(old) = serde_json::from_slice::<InputManifest>(&bytes) {
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
                }
            }
        }
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
    Ok(("toolchain or execution policy changed".into(), Vec::new()))
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
