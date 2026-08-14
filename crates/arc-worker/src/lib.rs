//! Reference remote execution worker.
//!
//! One process that is both the execution server and the worker: it accepts
//! jobs, queues them, runs each in a sandbox, and publishes results into the
//! shared cache. The split is internal — [`sandbox`] knows nothing about HTTP
//! and this module knows nothing about process groups — so the two can become
//! separate services without changing the protocol.
//!
//! Trust: a client may be malicious, so every request is validated before
//! anything is created. A worker may be malicious too, which is why the result
//! it publishes is content-addressed and re-verified by whoever reads it.

pub mod sandbox;

use anyhow::{Context, Result};
use arc_core::hash::Digest;
use arc_core::record::{BlobRef, ExecutionRecord, OutputFile};
use arc_core::remote::execution::{
    self as wire, ExecutionRequest, ExecutionResult, FailureKind, Job, JobError, JobState, LogChunk,
};
use arc_core::remote::protocol::{self, ErrorBody, WireBlob, WireOutput, WirePath};
use arc_core::remote::{Remote, RemoteConfig};
use arc_core::store::Store;
use sandbox::{LogSink, Sandbox};
use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;
use tiny_http::{Header, Method, Request, Response, StatusCode};

/// What a token is allowed to do. Reading capabilities and following a job is
/// not the same authority as making a machine run arbitrary commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Read,
    Execute,
}

pub struct Options {
    pub data: PathBuf,
    pub addr: String,
    /// Token authorising execution. `None` leaves the worker open, which is
    /// only ever right on a loopback address.
    pub execute_token: Option<String>,
    /// Token authorising capability and job queries but not submission.
    pub read_token: Option<String>,
    pub max_jobs: usize,
    pub queue_limit: usize,
    /// The shared cache: where inputs come from and results go.
    pub cache: RemoteConfig,
    pub log: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            data: PathBuf::from("arc-worker-data"),
            addr: "127.0.0.1:7891".into(),
            execute_token: None,
            read_token: None,
            max_jobs: 4,
            queue_limit: 64,
            cache: RemoteConfig::default(),
            log: false,
        }
    }
}

pub struct Worker {
    http: Arc<tiny_http::Server>,
    addr: SocketAddr,
    state: Arc<State>,
    stopping: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

struct State {
    store: Store,
    cache: RemoteConfig,
    execute_token: Option<String>,
    read_token: Option<String>,
    max_jobs: usize,
    queue_limit: usize,
    log: bool,
    id: String,
    jobs: Mutex<Registry>,
    wake: Condvar,
    stopping: Arc<AtomicBool>,
    active: AtomicUsize,
    counter: AtomicU64,
    metrics: Metrics,
}

#[derive(Default)]
struct Metrics {
    submitted: AtomicU64,
    deduplicated: AtomicU64,
    completed: AtomicU64,
    failed: AtomicU64,
    cache_short_circuits: AtomicU64,
}

#[derive(Default)]
struct Registry {
    /// `execution_key` -> job id, for singleflight. An entry exists only while
    /// the job is a useful answer to a new request.
    by_key: HashMap<String, String>,
    jobs: HashMap<String, Arc<Slot>>,
    queue: VecDeque<String>,
    order: VecDeque<String>,
}

/// One job's mutable state. The registry lock covers membership; each slot's
/// own lock covers its progress, so a long-running job never blocks lookups.
struct Slot {
    id: String,
    namespace: String,
    request: ExecutionRequest,
    log: LogSink,
    cancel: Arc<AtomicBool>,
    waiters: AtomicUsize,
    submitted: Instant,
    progress: Mutex<Progress>,
}

#[derive(Default)]
struct Progress {
    state: Option<JobState>,
    result: Option<ExecutionResult>,
    error: Option<JobError>,
    queued_ms: u64,
}

/// Completed jobs kept for late pollers. Bounded: results live in the cache,
/// so job metadata is a convenience, not a record of anything.
const RETAIN_JOBS: usize = 512;

impl Worker {
    pub fn start(opts: Options) -> Result<Worker> {
        let store = Store::open(&opts.data).context("opening worker store")?;
        // A worker that crashed mid-job leaves a workspace behind. Nothing in
        // there is a result, so it is all removable.
        let work = opts.data.join("work");
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work)?;

        let http = tiny_http::Server::http(&opts.addr)
            .map_err(|e| anyhow::anyhow!("listening on {}: {e}", opts.addr))?;
        let addr = http
            .server_addr()
            .to_ip()
            .context("server has no address")?;
        let http = Arc::new(http);
        let stopping = Arc::new(AtomicBool::new(false));
        let state = Arc::new(State {
            store,
            cache: opts.cache,
            execute_token: opts.execute_token,
            read_token: opts.read_token,
            max_jobs: opts.max_jobs.max(1),
            queue_limit: opts.queue_limit.max(1),
            log: opts.log,
            id: format!("worker-{}", std::process::id()),
            jobs: Mutex::new(Registry::default()),
            wake: Condvar::new(),
            stopping: stopping.clone(),
            active: AtomicUsize::new(0),
            counter: AtomicU64::new(0),
            metrics: Metrics::default(),
        });

        let mut threads = Vec::new();
        for _ in 0..state.max_jobs {
            let state = state.clone();
            threads.push(std::thread::spawn(move || run_loop(&state)));
        }
        // HTTP is served by its own pool so a full execution queue never stops
        // the worker answering questions about the jobs already in it.
        for _ in 0..4 {
            let http = http.clone();
            let state = state.clone();
            threads.push(std::thread::spawn(move || {
                while let Ok(req) = http.recv() {
                    if let Err(e) = route(&state, req) {
                        eprintln!("arc-worker: {e}");
                    }
                }
            }));
        }

        Ok(Worker {
            http,
            addr,
            state,
            stopping,
            threads,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn data(&self) -> &std::path::Path {
        &self.state.store.root
    }

    pub fn stats(&self) -> (u64, u64, u64, u64, u64) {
        let m = &self.state.metrics;
        (
            m.submitted.load(Ordering::Relaxed),
            m.deduplicated.load(Ordering::Relaxed),
            m.completed.load(Ordering::Relaxed),
            m.failed.load(Ordering::Relaxed),
            m.cache_short_circuits.load(Ordering::Relaxed),
        )
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Relaxed);
        // Running commands are killed rather than orphaned: their sandboxes are
        // about to disappear.
        if let Ok(reg) = self.state.jobs.lock() {
            for slot in reg.jobs.values() {
                slot.cancel.store(true, Ordering::Relaxed);
            }
        }
        self.state.wake.notify_all();
        self.http.unblock();
        let threads = std::mem::take(&mut self.threads);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for t in threads {
                let _ = t.join();
            }
            let _ = tx.send(());
        });
        let _ = rx.recv_timeout(std::time::Duration::from_secs(5));
    }
}

// ----------------------------------------------------------------- routing --

fn route(state: &Arc<State>, mut req: Request) -> Result<()> {
    let url = req.url().to_string();
    let method = req.method().clone();
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p, q),
        None => (url.as_str(), ""),
    };
    let Some(rest) = path.strip_prefix(wire::EXEC_PREFIX) else {
        return fail(req, 404, "unknown protocol version");
    };
    if state.log {
        eprintln!("{method} {path}");
    }
    let segs: Vec<&str> = rest.trim_start_matches('/').split('/').collect();

    if segs.as_slice() == ["capabilities"] {
        if !authorised(state, &req, Scope::Read) {
            return fail(req, 401, "missing or invalid credentials");
        }
        return json(req, 200, &capabilities(state));
    }

    let (ns, rest) = match segs.split_first() {
        Some((ns, rest)) if protocol::valid_namespace(ns) && !rest.is_empty() => (*ns, rest),
        _ => return fail(req, 404, "unknown resource"),
    };

    match (&method, rest) {
        (Method::Post, ["jobs"]) => {
            if !authorised(state, &req, Scope::Execute) {
                return fail(req, 401, "this token may not execute");
            }
            let request: ExecutionRequest = match read_json(&mut req, wire::MAX_REQUEST_BYTES) {
                Ok(r) => r,
                Err(e) => return fail(req, 400, &e.to_string()),
            };
            if let Err(e) = request.validate() {
                return fail(req, 400, &e);
            }
            if let Err(e) = request.compatible_with(&capabilities(state)) {
                return fail(req, 422, &e);
            }
            match submit(state, ns, request) {
                Ok(job) => json(req, 200, &job),
                Err(Overloaded) => fail(req, 429, "worker queue is full"),
            }
        }
        (Method::Get, ["jobs", id]) => {
            if !authorised(state, &req, Scope::Read) {
                return fail(req, 401, "missing or invalid credentials");
            }
            match snapshot(state, ns, id) {
                Some(job) => json(req, 200, &job),
                None => fail(req, 404, "no such job"),
            }
        }
        (Method::Get, ["jobs", id, "log"]) => {
            if !authorised(state, &req, Scope::Read) {
                return fail(req, 401, "missing or invalid credentials");
            }
            let offset = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("offset="))
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            let Some(slot) = slot_of(state, ns, id) else {
                return fail(req, 404, "no such job");
            };
            let (total, text) = slot.log.slice(offset, 256 * 1024);
            json(
                req,
                200,
                &LogChunk {
                    offset,
                    len: text.len() as u64,
                    total,
                    text,
                },
            )
        }
        (Method::Post, ["jobs", id, "cancel"]) => {
            if !authorised(state, &req, Scope::Execute) {
                return fail(req, 401, "this token may not cancel");
            }
            let Some(slot) = slot_of(state, ns, id) else {
                return fail(req, 404, "no such job");
            };
            // Advisory. One waiter walking away is not a reason to discard work
            // the others are still waiting for.
            let before = slot.waiters.fetch_sub(1, Ordering::SeqCst);
            if before <= 1 {
                slot.cancel.store(true, Ordering::Relaxed);
                state.wake.notify_all();
            }
            match snapshot(state, ns, id) {
                Some(job) => json(req, 200, &job),
                None => fail(req, 404, "no such job"),
            }
        }
        _ => fail(req, 404, "unknown resource"),
    }
}

fn capabilities(state: &State) -> wire::Capabilities {
    let reg = state.jobs.lock().expect("registry");
    wire::Capabilities {
        protocol: wire::EXEC_PROTOCOL_VERSION,
        key_semantics: arc_core::SCHEMA_VERSION,
        worker: state.id.clone(),
        version: arc_core::VERSION.into(),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        environment_id: wire::environment_id(),
        max_jobs: state.max_jobs,
        queue_limit: state.queue_limit,
        active: state.active.load(Ordering::Relaxed),
        queued: reg.queue.len(),
        network: wire::NetworkPolicy::Unrestricted,
        cache_endpoint: (!state.cache.url.is_empty()).then(|| state.cache.url.clone()),
        features: vec![wire::FEATURE_ENVIRONMENT.into()],
        host: Some(arc_core::environment::host_capability()),
    }
}

/// A token proves a scope. Execute implies read; read does not imply execute.
fn authorised(state: &State, req: &Request, want: Scope) -> bool {
    let presented =
        header(req, "authorization").and_then(|v| v.strip_prefix("Bearer ").map(str::to_string));
    let matches = |expected: &Option<String>| match (expected, &presented) {
        (None, _) => false,
        (Some(e), Some(p)) => constant_time_eq(e.as_bytes(), p.as_bytes()),
        (Some(_), None) => false,
    };
    if state.execute_token.is_none() && state.read_token.is_none() {
        return true;
    }
    match want {
        Scope::Execute => matches(&state.execute_token),
        Scope::Read => matches(&state.execute_token) || matches(&state.read_token),
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// -------------------------------------------------------------- scheduling --

struct Overloaded;

fn submit(
    state: &Arc<State>,
    ns: &str,
    request: ExecutionRequest,
) -> std::result::Result<Job, Overloaded> {
    let mut reg = state.jobs.lock().expect("registry");
    state.metrics.submitted.fetch_add(1, Ordering::Relaxed);

    // Singleflight. Two clients that want the same execution get the same job,
    // so the command runs once no matter how many are waiting on it.
    let key = format!("{ns}\0{}", request.execution_key);
    if let Some(existing) = reg
        .by_key
        .get(&key)
        .and_then(|id| reg.jobs.get(id))
        .cloned()
    {
        let terminal = existing
            .progress
            .lock()
            .map(|p| p.state.map(|s| s.terminal()).unwrap_or(false))
            .unwrap_or(false);
        if !terminal {
            existing.waiters.fetch_add(1, Ordering::SeqCst);
            existing.cancel.store(false, Ordering::Relaxed);
            state.metrics.deduplicated.fetch_add(1, Ordering::Relaxed);
            return Ok(job_of(&existing));
        }
        // A finished job still answers, so a client that arrives late does not
        // pay for the work again.
        let completed = existing
            .progress
            .lock()
            .map(|p| p.state == Some(JobState::Completed))
            .unwrap_or(false);
        if completed {
            state.metrics.deduplicated.fetch_add(1, Ordering::Relaxed);
            return Ok(job_of(&existing));
        }
    }

    if reg.queue.len() >= state.queue_limit {
        return Err(Overloaded);
    }

    let n = state.counter.fetch_add(1, Ordering::Relaxed);
    let id = format!("j-{}-{n}", std::process::id());
    let slot = Arc::new(Slot {
        id: id.clone(),
        namespace: ns.to_string(),
        request,
        log: LogSink::default(),
        cancel: Arc::new(AtomicBool::new(false)),
        waiters: AtomicUsize::new(1),
        submitted: Instant::now(),
        progress: Mutex::new(Progress {
            state: Some(JobState::Queued),
            ..Default::default()
        }),
    });
    reg.by_key.insert(key, id.clone());
    reg.jobs.insert(id.clone(), slot.clone());
    reg.queue.push_back(id.clone());
    reg.order.push_back(id);
    evict(&mut reg);
    drop(reg);
    state.wake.notify_one();
    Ok(job_of(&slot))
}

/// Keep completed job metadata bounded. Never evicts anything still queued or
/// running, and never evicts the newest entries.
fn evict(reg: &mut Registry) {
    while reg.order.len() > RETAIN_JOBS {
        let Some(id) = reg.order.pop_front() else {
            return;
        };
        let done = reg
            .jobs
            .get(&id)
            .map(|s| {
                s.progress
                    .lock()
                    .map(|p| p.state.map(|st| st.terminal()).unwrap_or(false))
                    .unwrap_or(false)
            })
            .unwrap_or(true);
        if !done {
            reg.order.push_back(id);
            return;
        }
        if let Some(slot) = reg.jobs.remove(&id) {
            let key = format!("{}\0{}", slot.namespace, slot.request.execution_key);
            if reg.by_key.get(&key) == Some(&id) {
                reg.by_key.remove(&key);
            }
        }
    }
}

fn run_loop(state: &Arc<State>) {
    loop {
        let slot = {
            let mut reg = state.jobs.lock().expect("registry");
            loop {
                if state.stopping.load(Ordering::Relaxed) {
                    return;
                }
                match reg.queue.pop_front() {
                    Some(id) => match reg.jobs.get(&id) {
                        Some(s) => break s.clone(),
                        None => continue,
                    },
                    None => {
                        reg = match state.wake.wait(reg) {
                            Ok(r) => r,
                            Err(_) => return,
                        };
                    }
                }
            }
        };

        state.active.fetch_add(1, Ordering::Relaxed);
        let queued_ms = slot.submitted.elapsed().as_millis() as u64;
        set(&slot, |p| {
            p.state = Some(JobState::Running);
            p.queued_ms = queued_ms;
        });

        let outcome = execute(state, &slot);
        state.active.fetch_sub(1, Ordering::Relaxed);

        match outcome {
            Ok(result) => {
                state.metrics.completed.fetch_add(1, Ordering::Relaxed);
                set(&slot, |p| {
                    p.state = Some(JobState::Completed);
                    p.result = Some(result.clone());
                });
            }
            Err(e) => {
                state.metrics.failed.fetch_add(1, Ordering::Relaxed);
                let kind = e.kind;
                set(&slot, |p| {
                    p.state = Some(if kind == FailureKind::Cancelled {
                        JobState::Cancelled
                    } else {
                        JobState::Failed
                    });
                    p.error = Some(JobError {
                        kind,
                        message: e.message.clone(),
                    });
                });
            }
        }
    }
}

fn set(slot: &Arc<Slot>, f: impl FnOnce(&mut Progress)) {
    if let Ok(mut p) = slot.progress.lock() {
        f(&mut p);
    }
}

struct Failure {
    kind: FailureKind,
    message: String,
}

fn failure(kind: FailureKind, message: impl std::fmt::Display) -> Failure {
    Failure {
        kind,
        message: message.to_string(),
    }
}

// -------------------------------------------------------------- execution --

fn execute(state: &Arc<State>, slot: &Arc<Slot>) -> std::result::Result<ExecutionResult, Failure> {
    let req = &slot.request;
    let cache = open_cache(state, &slot.namespace);

    // The cache may have been populated between the client's miss and this
    // job reaching the front of the queue. Running the command anyway would be
    // correct but wasteful.
    if let Some(c) = cache.as_ref() {
        if let Ok(Some(existing)) = c.lookup(&req.execution_key) {
            state
                .metrics
                .cache_short_circuits
                .fetch_add(1, Ordering::Relaxed);
            return Ok(ExecutionResult {
                exit_code: existing.exit_code,
                signaled: false,
                duration_ms: existing.duration_ms,
                outputs: existing.outputs,
                stdout: existing.stdout,
                stderr: existing.stderr,
                truncated: false,
                published: true,
            });
        }
    }

    // Capabilities can change while a job waits, so eligibility is rechecked
    // here rather than trusted from submission time.
    req.compatible_with(&capabilities(state))
        .map_err(|e| failure(FailureKind::Incompatible, e))?;

    let sb = Sandbox::create(sandbox_root(state, &slot.id))
        .map_err(|e| failure(FailureKind::Sandbox, format!("{e:#}")))?;

    let result = execute_in(state, slot, &sb, cache.as_ref());
    // The sandbox goes whether the command succeeded, failed, or never ran.
    sb.remove();
    result
}

fn execute_in(
    state: &Arc<State>,
    slot: &Arc<Slot>,
    sb: &Sandbox,
    cache: Option<&Remote>,
) -> std::result::Result<ExecutionResult, Failure> {
    let req = &slot.request;

    // Inputs the worker does not already hold come from the shared cache and
    // are verified on the way in, exactly as any other Arc download.
    let wanted = req.digests();
    let missing: Vec<String> = wanted
        .iter()
        .filter(|d| {
            Digest::parse(d)
                .map(|p| !state.store.exists(&p))
                .unwrap_or(true)
        })
        .cloned()
        .collect();
    if !missing.is_empty() {
        let Some(c) = cache else {
            return Err(failure(
                FailureKind::InputUnavailable,
                format!(
                    "{} inputs are missing and no cache is configured",
                    missing.len()
                ),
            ));
        };
        c.download(&state.store, &missing)
            .map_err(|e| failure(FailureKind::InputUnavailable, format!("{e:#}")))?;
    }

    sb.materialise(&state.store, &req.inputs)
        .map_err(|e| failure(FailureKind::Sandbox, format!("{e:#}")))?;

    let environment = match &req.environment {
        Some(id) => Some(materialise_environment(state, id, cache, &slot.log)?),
        None => None,
    };

    // Resolve every required executable and check its contents. A worker with a
    // different compiler is not a worker that can answer this question.
    //
    // Inside an Arc environment there is nothing to match: the program comes
    // from the environment or the job does not run. Falling back to the
    // worker's own copy would make the result depend on this machine, which is
    // exactly what the environment exists to prevent.
    let mut program = None;
    for t in &req.tools {
        let resolved = sandbox::resolve_tool(t, sb.workspace())
            .map_err(|e| failure(FailureKind::Incompatible, format!("{e:#}")))?;
        if t.program == req.program {
            program = Some(resolved);
        }
    }
    let program = match (&environment, program) {
        (Some(env), _) => env.resolve(&req.program).ok_or_else(|| {
            failure(
                FailureKind::Incompatible,
                format!(
                    "environment {} does not provide `{}`",
                    &env.id[..12],
                    req.program
                ),
            )
        })?,
        (None, Some(p)) => p,
        (None, None) => arc_core::key::which(&req.program, sb.workspace()).ok_or_else(|| {
            failure(
                FailureKind::Incompatible,
                format!("`{}` is not available on this worker", req.program),
            )
        })?,
    };

    if slot.cancel.load(Ordering::Relaxed) {
        return Err(failure(FailureKind::Cancelled, "cancelled before starting"));
    }

    let completion = sb
        .run(
            req,
            &program,
            &req.limits,
            &slot.log,
            &slot.cancel,
            environment.as_ref(),
        )
        .map_err(|e| failure(FailureKind::Sandbox, format!("{e:#}")))?;

    if completion.timed_out {
        return Err(failure(
            FailureKind::Timeout,
            format!("command exceeded {}ms", req.limits.timeout_ms),
        ));
    }
    if completion.cancelled {
        return Err(failure(FailureKind::Cancelled, "cancelled"));
    }

    let outputs = sb
        .capture(&req.output_globs, &state.store, req.limits.max_output_bytes)
        .map_err(|e| failure(FailureKind::Sandbox, format!("{e:#}")))?;
    let stdout = blob(&state.store, &completion.stdout)
        .map_err(|e| failure(FailureKind::Internal, format!("{e:#}")))?;
    let stderr = blob(&state.store, &completion.stderr)
        .map_err(|e| failure(FailureKind::Internal, format!("{e:#}")))?;

    let mut result = ExecutionResult {
        exit_code: completion.exit_code,
        signaled: completion.signaled,
        duration_ms: completion.duration_ms,
        outputs: outputs.iter().map(wire_output).collect(),
        stdout: Some(stdout),
        stderr: Some(stderr),
        truncated: completion.truncated,
        published: false,
    };

    // Objects always; the record only when the result is one Arc would cache
    // locally. Uploading objects for an uncacheable failure is harmless — they
    // are content-addressed and unreferenced — and it lets the client show the
    // output without a second transfer path.
    if let Some(c) = cache.filter(|c| c.write) {
        let digests = result.digests();
        if let Err(e) = c.upload(&state.store, &digests) {
            return Err(failure(
                FailureKind::Internal,
                format!("publishing objects: {e:#}"),
            ));
        }
        if cacheable(&completion, req) {
            let record = record_for(req, &result, &outputs);
            // Objects first, record last: a record is a promise its objects can
            // be fetched, so it is only made once they can be.
            match c.publish(&record) {
                Ok(()) => result.published = true,
                // A conflicting record means another worker already published a
                // different result for this key. The client will read that one.
                Err(e) => slot
                    .log
                    .append(format!("\n[arc: not published: {e}]\n").as_bytes()),
            }
        }
    }

    Ok(result)
}

/// Fetch, verify and materialise an environment, once per worker.
///
/// Nothing about the request describes the environment: it names one by
/// content id, and the manifest is read back out of the shared cache under that
/// digest. A coordinator therefore cannot make a worker build an environment;
/// it can only ask for one that already exists and hashes to what it said.
fn materialise_environment(
    state: &Arc<State>,
    id: &str,
    cache: Option<&Remote>,
    log: &LogSink,
) -> std::result::Result<arc_core::environment::Materialised, Failure> {
    let fetch = |digests: &[String]| -> std::result::Result<(), Failure> {
        if digests.is_empty() {
            return Ok(());
        }
        let c = cache.ok_or_else(|| {
            failure(
                FailureKind::InputUnavailable,
                "an environment was requested and this worker has no cache to fetch it from",
            )
        })?;
        c.download(&state.store, digests)
            .map_err(|e| failure(FailureKind::InputUnavailable, format!("{e:#}")))
    };

    let d = Digest::parse(id).map_err(|e| failure(FailureKind::Incompatible, e))?;
    if !state.store.exists(&d) {
        fetch(std::slice::from_ref(&id.to_string()))?;
    }
    let manifest = arc_core::environment::load_manifest(id, &state.store)
        .map_err(|e| failure(FailureKind::Incompatible, format!("{e:#}")))?;

    // Whether this host can run it is knowable here and nowhere else: the
    // loader and system libraries the environment needs are on this machine or
    // they are not.
    arc_core::environment::host::supports(
        &arc_core::environment::host_capability(),
        &manifest.host,
    )
    .map_err(|e| failure(FailureKind::Incompatible, e))?;

    let envs = arc_core::environment::Environments::open(
        state.store.root.parent().unwrap_or(&state.store.root),
    )
    .map_err(|e| failure(FailureKind::Sandbox, format!("{e:#}")))?;
    if !envs.ready(id) {
        let missing: Vec<String> = manifest
            .digests()
            .into_iter()
            .filter(|x| {
                Digest::parse(x)
                    .map(|p| !state.store.exists(&p))
                    .unwrap_or(true)
            })
            .collect();
        if !missing.is_empty() {
            log.append(
                format!(
                    "[arc: fetching environment {} ({} objects)]
",
                    &id[..12],
                    missing.len()
                )
                .as_bytes(),
            );
        }
        fetch(&missing)?;
    }
    envs.materialise(&manifest, &state.store)
        .map_err(|e| failure(FailureKind::Sandbox, format!("{e:#}")))
}

fn cacheable(c: &sandbox::Completion, req: &ExecutionRequest) -> bool {
    !c.truncated && !c.signaled && !c.timed_out && (c.exit_code == 0 || req.cache_failures)
}

fn record_for(
    req: &ExecutionRequest,
    result: &ExecutionResult,
    outputs: &[OutputFile],
) -> protocol::RemoteExecution {
    let local = ExecutionRecord {
        schema: arc_core::SCHEMA_VERSION,
        id: String::new(),
        key: req.execution_key.clone(),
        program: req.program.clone(),
        args: req.args.clone(),
        project_root: String::new(),
        rel_cwd: req.rel_cwd.clone(),
        started_at: 0,
        duration_ms: result.duration_ms,
        exit_code: result.exit_code,
        input_digest: String::new(),
        input_file_count: req.inputs.len(),
        env: arc_core::key::EnvFingerprint {
            vars: Vec::new(),
            digest: String::new(),
        },
        toolchain: arc_core::key::Toolchain {
            program: req.program.clone(),
            resolved_path: None,
            digest: String::new(),
        },
        stdout: result.stdout.as_ref().map(|b| BlobRef {
            digest: b.digest.clone(),
            size: b.size,
        }),
        stderr: result.stderr.as_ref().map(|b| BlobRef {
            digest: b.digest.clone(),
            size: b.size,
        }),
        outputs: outputs.to_vec(),
        cache_status: arc_core::record::CacheStatus::Miss,
        cache_source: arc_core::record::CacheSource::Local,
        replayed_from: None,
        input_manifest: None,
        arc_version: arc_core::VERSION.into(),
        family_key: req.family_key.clone(),
        trace: None,
        environment: None,
    };
    arc_core::remote::from_record(&local)
}

fn wire_output(o: &OutputFile) -> WireOutput {
    WireOutput {
        path: WirePath::from_rel(&o.rel),
        digest: o.digest.clone(),
        size: o.size,
        exec: o.exec,
    }
}

fn blob(store: &Store, bytes: &[u8]) -> Result<WireBlob> {
    Ok(WireBlob {
        digest: store.put_bytes(bytes)?.hex(),
        size: bytes.len() as u64,
    })
}

fn open_cache(state: &State, ns: &str) -> Option<Remote> {
    if state.cache.url.trim().is_empty() {
        return None;
    }
    let cfg = RemoteConfig {
        namespace: ns.to_string(),
        ..state.cache.clone()
    };
    Remote::open(&cfg).ok()
}

fn sandbox_root(state: &State, id: &str) -> PathBuf {
    state
        .store
        .root
        .parent()
        .unwrap_or(&state.store.root)
        .join("work")
        .join(id)
}

// ------------------------------------------------------------- job lookup --

fn slot_of(state: &State, ns: &str, id: &str) -> Option<Arc<Slot>> {
    let reg = state.jobs.lock().ok()?;
    let slot = reg.jobs.get(id)?;
    // A job belongs to the namespace that created it; another namespace may not
    // even learn that it exists.
    (slot.namespace == ns).then(|| slot.clone())
}

fn snapshot(state: &State, ns: &str, id: &str) -> Option<Job> {
    slot_of(state, ns, id).map(|s| job_of(&s))
}

fn job_of(slot: &Arc<Slot>) -> Job {
    let p = slot.progress.lock().expect("progress");
    Job {
        id: slot.id.clone(),
        execution_key: slot.request.execution_key.clone(),
        state: p.state.unwrap_or(JobState::Queued),
        result: p.result.clone(),
        error: p.error.clone(),
        log_len: slot.log.len(),
        worker: String::new(),
        waiters: slot.waiters.load(Ordering::Relaxed),
        queued_ms: p.queued_ms,
    }
}

// ------------------------------------------------------------------ http ---

fn read_json<T: serde::de::DeserializeOwned>(req: &mut Request, limit: usize) -> Result<T> {
    let mut buf = Vec::new();
    req.as_reader()
        .take(limit as u64 + 1)
        .read_to_end(&mut buf)?;
    anyhow::ensure!(buf.len() <= limit, "request body is too large");
    serde_json::from_slice(&buf).context("malformed request body")
}

fn json<T: serde::Serialize>(req: Request, code: u16, body: &T) -> Result<()> {
    let bytes = serde_json::to_vec(body)?;
    Ok(req.respond(
        Response::from_data(bytes)
            .with_status_code(StatusCode(code))
            .with_header(content_type("application/json")),
    )?)
}

fn fail(req: Request, code: u16, message: &str) -> Result<()> {
    let body = serde_json::to_vec(&ErrorBody {
        error: message.to_string(),
    })
    .unwrap_or_default();
    let _ = req.respond(
        Response::from_data(body)
            .with_status_code(StatusCode(code))
            .with_header(content_type("application/json")),
    );
    Ok(())
}

fn content_type(v: &str) -> Header {
    Header::from_bytes(&b"Content-Type"[..], v.as_bytes()).expect("static header")
}

fn header(req: &Request, name: &'static str) -> Option<String> {
    req.headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str().to_string())
}
