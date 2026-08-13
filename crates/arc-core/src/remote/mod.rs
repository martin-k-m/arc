//! The remote cache client.
//!
//! Trust rule for this module: nothing a server says is believed. A record is
//! validated before it is read, every object is re-hashed before it enters the
//! local store, and any failure at all — network, protocol, corruption, auth —
//! resolves to "no remote result", never to a guess. The caller then does what
//! it would have done without a remote at all.

// `ureq::Error` is large by value and is the HTTP client's own type. Boxing it
// at every call site would add noise without changing what Arc does with it,
// which is convert it to a message and fall back.
#![allow(clippy::result_large_err)]

pub mod dispatch;
pub mod eligibility;
pub mod execution;
pub mod protocol;

use crate::hash::{Digest, Hasher};
use crate::record::{BlobRef, ExecutionRecord, OutputFile};
use crate::store::Store;
use anyhow::{anyhow, bail, Context, Result};
use protocol::{
    Info, MissingRequest, MissingResponse, RemoteExecution, WireBlob, WireOutput, WirePath,
};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const USER_AGENT: &str = concat!("arc/", env!("CARGO_PKG_VERSION"));
const RETRY_ATTEMPTS: u32 = 3;
const RETRY_BASE_MS: u64 = 40;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RemoteConfig {
    pub enabled: bool,
    pub url: String,
    /// Logical isolation for execution metadata. Required: there is no safe way
    /// to guess one, and guessing wrong either leaks results between unrelated
    /// projects or silently caches nothing.
    pub namespace: String,
    /// Name of the environment variable holding the bearer token. The token
    /// itself is never written to configuration.
    pub token_env: String,
    pub read: bool,
    pub write: bool,
    pub connect_timeout_ms: u64,
    pub request_timeout_ms: u64,
    pub concurrency: usize,
    pub execution: ExecutionConfig,
}

/// Remote *execution*, which is off unless a project turns it on. A shared
/// cache is a safe default; sending commands to another machine is a decision
/// somebody has to make.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ExecutionConfig {
    pub enabled: bool,
    /// The worker endpoint. Separate from the cache URL: a worker and a cache
    /// are different services even when one machine runs both.
    pub url: String,
    /// Name of the environment variable holding the worker's bearer token.
    /// Falls back to the cache's `token_env` when empty.
    pub token_env: String,
    /// Variables that may be sent to a worker despite looking like secrets.
    /// Empty by default, and deliberately awkward to fill in.
    pub allow_env: Vec<String>,
    pub timeout_ms: u64,
    /// How often to ask a running job for its state.
    pub poll_ms: u64,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        ExecutionConfig {
            enabled: false,
            url: String::new(),
            token_env: String::new(),
            allow_env: Vec::new(),
            timeout_ms: execution::DEFAULT_TIMEOUT_MS,
            poll_ms: 250,
        }
    }
}

impl Default for RemoteConfig {
    fn default() -> Self {
        RemoteConfig {
            enabled: true,
            url: String::new(),
            namespace: String::new(),
            token_env: String::new(),
            read: true,
            write: true,
            connect_timeout_ms: 3_000,
            request_timeout_ms: 30_000,
            concurrency: 8,
            execution: ExecutionConfig::default(),
        }
    }
}

/// Why a remote is not in use, when it is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disabled {
    NotConfigured,
    TurnedOff,
    NoNamespace,
    Invalid(String),
}

impl Disabled {
    pub fn reason(&self) -> String {
        match self {
            Disabled::NotConfigured => "no [remote] url configured".into(),
            Disabled::TurnedOff => "disabled".into(),
            Disabled::NoNamespace => "[remote] namespace is required".into(),
            Disabled::Invalid(e) => e.clone(),
        }
    }
}

/// The configuration actually in force, after environment overrides.
pub fn effective_config(cfg: &RemoteConfig) -> RemoteConfig {
    let mut c = cfg.clone();
    if let Ok(v) = std::env::var("ARC_REMOTE_URL") {
        c.url = v;
    }
    if let Ok(v) = std::env::var("ARC_REMOTE_NAMESPACE") {
        c.namespace = v;
    }
    if let Ok(v) = std::env::var("ARC_REMOTE_TOKEN_ENV") {
        c.token_env = v;
    }
    if let Some(v) = flag("ARC_REMOTE_READ") {
        c.read = v;
    }
    if let Some(v) = flag("ARC_REMOTE_WRITE") {
        c.write = v;
    }
    if let Some(v) = flag("ARC_REMOTE_ENABLED") {
        c.enabled = v;
    }
    if let Some(v) = number("ARC_REMOTE_CONCURRENCY") {
        c.concurrency = (v as usize).clamp(1, 64);
    }
    if let Some(v) = number("ARC_REMOTE_TIMEOUT_MS") {
        c.request_timeout_ms = v;
        c.connect_timeout_ms = c.connect_timeout_ms.min(v);
    }
    if let Some(v) = flag("ARC_REMOTE_EXECUTION") {
        c.execution.enabled = v;
    }
    if let Ok(v) = std::env::var("ARC_REMOTE_EXECUTION_URL") {
        c.execution.url = v;
    }
    if let Some(v) = number("ARC_REMOTE_EXECUTION_TIMEOUT_MS") {
        c.execution.timeout_ms = v;
    }
    if let Some(v) = number("ARC_REMOTE_EXECUTION_POLL_MS") {
        c.execution.poll_ms = v;
    }
    c
}

fn flag(name: &str) -> Option<bool> {
    match std::env::var(name).ok()?.as_str() {
        "1" | "true" | "yes" => Some(true),
        "0" | "false" | "no" => Some(false),
        _ => None,
    }
}

fn number(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.parse().ok()
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Metrics {
    pub requests: u64,
    pub objects_downloaded: u64,
    pub objects_uploaded: u64,
    pub bytes_downloaded: u64,
    pub bytes_uploaded: u64,
    pub wire_bytes_downloaded: u64,
    pub wire_bytes_uploaded: u64,
    pub lookup_ms: u64,
    pub transfer_ms: u64,
    pub verify_ms: u64,
}

#[derive(Default)]
struct Counters {
    requests: AtomicU64,
    objects_downloaded: AtomicU64,
    objects_uploaded: AtomicU64,
    bytes_downloaded: AtomicU64,
    bytes_uploaded: AtomicU64,
    wire_bytes_downloaded: AtomicU64,
    wire_bytes_uploaded: AtomicU64,
    lookup_ms: AtomicU64,
    transfer_ms: AtomicU64,
    verify_ms: AtomicU64,
}

pub struct Remote {
    agent: ureq::Agent,
    base: String,
    endpoint: String,
    namespace: String,
    token: Option<String>,
    pub read: bool,
    pub write: bool,
    concurrency: usize,
    counters: Counters,
    /// Objects this process already failed to obtain from this server. Bounded
    /// to one invocation: a transient server problem must not become a
    /// persistent local belief.
    poisoned: Mutex<std::collections::HashSet<String>>,
}

impl Remote {
    /// Build a client, or say why there is none. A missing configuration is not
    /// an error: local-only is Arc's default mode.
    pub fn open(cfg: &RemoteConfig) -> std::result::Result<Remote, Disabled> {
        let cfg = effective_config(cfg);
        if !cfg.enabled {
            return Err(Disabled::TurnedOff);
        }
        if cfg.url.trim().is_empty() {
            return Err(Disabled::NotConfigured);
        }
        if cfg.namespace.trim().is_empty() {
            return Err(Disabled::NoNamespace);
        }
        if !protocol::valid_namespace(cfg.namespace.trim()) {
            return Err(Disabled::Invalid(format!(
                "invalid [remote] namespace `{}`",
                cfg.namespace
            )));
        }
        let url = parse_endpoint(cfg.url.trim()).map_err(Disabled::Invalid)?;
        let token = read_token(&cfg.token_env).map_err(Disabled::Invalid)?;
        let agent = ureq::AgentBuilder::new()
            .user_agent(USER_AGENT)
            // A redirect could send the Authorization header to another host.
            // Arc talks to the endpoint it was configured with, or to nothing.
            .redirects(0)
            .timeout_connect(Duration::from_millis(cfg.connect_timeout_ms))
            .timeout_read(Duration::from_millis(cfg.request_timeout_ms))
            .timeout_write(Duration::from_millis(cfg.request_timeout_ms))
            .build();
        Ok(Remote {
            endpoint: url.host.clone(),
            base: format!("{}{}", url.base, protocol::PROTOCOL_PREFIX),
            namespace: cfg.namespace.trim().to_string(),
            token,
            read: cfg.read,
            write: cfg.write,
            concurrency: cfg.concurrency.clamp(1, 64),
            agent,
            counters: Counters::default(),
            poisoned: Mutex::new(std::collections::HashSet::new()),
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
    pub fn namespace(&self) -> &str {
        &self.namespace
    }
    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    pub fn metrics(&self) -> Metrics {
        let c = &self.counters;
        Metrics {
            requests: c.requests.load(Ordering::Relaxed),
            objects_downloaded: c.objects_downloaded.load(Ordering::Relaxed),
            objects_uploaded: c.objects_uploaded.load(Ordering::Relaxed),
            bytes_downloaded: c.bytes_downloaded.load(Ordering::Relaxed),
            bytes_uploaded: c.bytes_uploaded.load(Ordering::Relaxed),
            wire_bytes_downloaded: c.wire_bytes_downloaded.load(Ordering::Relaxed),
            wire_bytes_uploaded: c.wire_bytes_uploaded.load(Ordering::Relaxed),
            lookup_ms: c.lookup_ms.load(Ordering::Relaxed),
            transfer_ms: c.transfer_ms.load(Ordering::Relaxed),
            verify_ms: c.verify_ms.load(Ordering::Relaxed),
        }
    }

    fn url(&self, tail: &str) -> String {
        format!("{}/{}/{}", self.base, self.namespace, tail)
    }

    fn auth(&self, r: ureq::Request) -> ureq::Request {
        match &self.token {
            Some(t) => r.set("Authorization", &format!("Bearer {t}")),
            None => r,
        }
    }

    pub fn info(&self) -> Result<Info> {
        let url = format!("{}/info", self.base);
        let resp = self.send(|| self.auth(self.agent.get(&url)).call())?;
        let info: Info = read_json(resp, 64 * 1024)?;
        if info.protocol != protocol::PROTOCOL_VERSION {
            bail!(
                "server speaks remote-cache protocol v{}, this Arc speaks v{}",
                info.protocol,
                protocol::PROTOCOL_VERSION
            );
        }
        Ok(info)
    }

    /// `Ok(None)` is a plain cache miss. `Err` means the remote could not be
    /// consulted, which the caller treats identically but may report.
    pub fn lookup(&self, execution_key: &str) -> Result<Option<RemoteExecution>> {
        if !protocol::valid_digest(execution_key) {
            return Ok(None);
        }
        let start = Instant::now();
        let url = self.url(&format!("executions/{execution_key}"));
        let resp = match self.send(|| self.auth(self.agent.get(&url)).call()) {
            Ok(r) => r,
            Err(e) if is_not_found(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        let rec: RemoteExecution = read_json(resp, protocol::MAX_METADATA_BYTES)?;
        self.counters
            .lookup_ms
            .fetch_add(start.elapsed().as_millis() as u64, Ordering::Relaxed);
        rec.validate(Some(execution_key)).map_err(|e| anyhow!(e))?;
        rec.compatible_with_host().map_err(|e| anyhow!(e))?;
        Ok(Some(rec))
    }

    /// Dependency knowledge for families this machine already intends to run.
    ///
    /// One request per batch rather than one per task: a CI job asks about every
    /// canonical task at once, and the answer is only worth having if getting it
    /// is cheaper than the work it avoids.
    pub fn lookup_tasks(&self, families: &[String]) -> Result<Vec<protocol::RemoteTask>> {
        let mut out = Vec::new();
        for chunk in families.chunks(protocol::MAX_BATCH_TASKS) {
            let url = self.url("tasks/lookup");
            let bytes = serde_json::to_vec(&protocol::TaskLookupRequest {
                families: chunk.to_vec(),
            })?;
            let start = Instant::now();
            let resp = match self.send(|| {
                self.auth(self.agent.post(&url))
                    .set("Content-Type", "application/json")
                    .send_bytes(&bytes)
            }) {
                Ok(r) => r,
                // A server that does not serve task knowledge is a server Arc
                // works without: the tasks stay unknown and therefore run.
                Err(e) if is_not_found(&e) => return Ok(out),
                Err(e) => return Err(e),
            };
            let body: protocol::TaskLookupResponse = read_json(resp, protocol::MAX_METADATA_BYTES)?;
            self.counters
                .lookup_ms
                .fetch_add(start.elapsed().as_millis() as u64, Ordering::Relaxed);
            for t in body.tasks {
                if t.validate(None).is_ok() && t.compatible_with_host().is_ok() {
                    out.push(t);
                }
            }
        }
        Ok(out)
    }

    pub fn publish_task(&self, task: &protocol::RemoteTask) -> Result<()> {
        task.validate(None).map_err(|e| anyhow!(e))?;
        let url = self.url(&format!("tasks/{}", task.family_key));
        self.send(|| {
            self.auth(self.agent.put(&url))
                .set("Content-Type", "application/json")
                .send_bytes(&task.canonical())
        })?;
        Ok(())
    }

    pub fn missing(&self, digests: &[String]) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for chunk in digests.chunks(protocol::MAX_BATCH_DIGESTS) {
            let url = self.url("objects/missing");
            let body = MissingRequest {
                digests: chunk.to_vec(),
            };
            let bytes = serde_json::to_vec(&body)?;
            let resp = self.send(|| {
                self.auth(self.agent.post(&url))
                    .set("Content-Type", "application/json")
                    .send_bytes(&bytes)
            })?;
            let r: MissingResponse = read_json(resp, protocol::MAX_METADATA_BYTES)?;
            out.extend(r.missing);
        }
        Ok(out)
    }

    /// Fetch every digest not already in the local store, verifying each before
    /// it is committed. All-or-nothing from the caller's point of view: a
    /// partial result is reported as an error and no replay follows.
    pub fn download(&self, store: &Store, digests: &[String]) -> Result<()> {
        let wanted: Vec<String> = digests
            .iter()
            .filter(|d| Digest::parse(d).map(|p| !store.exists(&p)).unwrap_or(false))
            .cloned()
            .collect();
        if wanted.is_empty() {
            return Ok(());
        }
        {
            let poisoned = self.poisoned.lock().unwrap();
            if let Some(d) = wanted.iter().find(|d| poisoned.contains(*d)) {
                bail!("object {} already failed verification", &d[..12]);
            }
        }
        let start = Instant::now();
        let r = self.each(&wanted, |d| self.download_one(store, d));
        self.counters
            .transfer_ms
            .fetch_add(start.elapsed().as_millis() as u64, Ordering::Relaxed);
        r
    }

    fn download_one(&self, store: &Store, digest: &str) -> Result<()> {
        let expect = Digest::parse(digest)?;
        let url = self.url(&format!("objects/{digest}"));
        let resp = self.send(|| {
            self.auth(self.agent.get(&url))
                .set(protocol::HEADER_ACCEPT_ENCODING, protocol::ENCODING_DEFLATE)
                .call()
        })?;
        let compressed = resp
            .header(protocol::HEADER_ENCODING)
            .map(|e| e == protocol::ENCODING_DEFLATE)
            .unwrap_or(false);
        let wire = Counted::new(resp.into_reader());
        let seen = wire.seen.clone();
        let mut reader: Box<dyn Read> = if compressed {
            Box::new(flate2::read::DeflateDecoder::new(wire))
        } else {
            Box::new(wire)
        };
        let verify = Instant::now();
        let size = match store.put_verified(&mut reader, &expect) {
            Ok(n) => n,
            // Refusing this object again within the same invocation stops one
            // bad server from being asked for it once per referencing record.
            Err(e) => {
                self.poisoned.lock().unwrap().insert(digest.to_string());
                return Err(e);
            }
        };
        self.counters
            .verify_ms
            .fetch_add(verify.elapsed().as_millis() as u64, Ordering::Relaxed);
        self.counters
            .objects_downloaded
            .fetch_add(1, Ordering::Relaxed);
        self.counters
            .bytes_downloaded
            .fetch_add(size, Ordering::Relaxed);
        self.counters
            .wire_bytes_downloaded
            .fetch_add(seen.load(Ordering::Relaxed), Ordering::Relaxed);
        Ok(())
    }

    /// Upload the objects the server says it lacks. Called before any execution
    /// record is published, so a record never references an absent object.
    pub fn upload(&self, store: &Store, digests: &[String]) -> Result<()> {
        let missing = self.missing(digests)?;
        if missing.is_empty() {
            return Ok(());
        }
        let start = Instant::now();
        let r = self.each(&missing, |d| self.upload_one(store, d));
        self.counters
            .transfer_ms
            .fetch_add(start.elapsed().as_millis() as u64, Ordering::Relaxed);
        r
    }

    fn upload_one(&self, store: &Store, digest: &str) -> Result<()> {
        let d = Digest::parse(digest)?;
        let size = store
            .size_of(&d)
            .ok_or_else(|| anyhow!("local object {} is missing", &digest[..12]))?;
        let url = self.url(&format!("objects/{digest}"));
        let compress = size >= protocol::COMPRESS_MIN_BYTES;
        let wire = if compress {
            deflate(&mut store.open_blob(&d)?)?
        } else {
            let mut buf = Vec::new();
            store.open_blob(&d)?.read_to_end(&mut buf)?;
            buf
        };
        self.send(|| {
            let mut r = self
                .auth(self.agent.put(&url))
                .set("Content-Type", "application/octet-stream");
            if compress {
                r = r.set(protocol::HEADER_ENCODING, protocol::ENCODING_DEFLATE);
            }
            r.send_bytes(&wire)
        })?;
        self.counters
            .objects_uploaded
            .fetch_add(1, Ordering::Relaxed);
        self.counters
            .bytes_uploaded
            .fetch_add(size, Ordering::Relaxed);
        self.counters
            .wire_bytes_uploaded
            .fetch_add(wire.len() as u64, Ordering::Relaxed);
        Ok(())
    }

    pub fn publish(&self, rec: &RemoteExecution) -> Result<()> {
        rec.validate(None).map_err(|e| anyhow!(e))?;
        let url = self.url(&format!("executions/{}", rec.execution_key));
        self.send(|| {
            self.auth(self.agent.put(&url))
                .set("Content-Type", "application/json")
                .send_bytes(&rec.canonical())
        })?;
        Ok(())
    }

    /// Run `f` over every item with bounded concurrency. The first failure is
    /// reported; remaining work is allowed to finish rather than being torn
    /// down, since every unit is independent and idempotent.
    fn each<T: Sync>(&self, items: &[T], f: impl Fn(&T) -> Result<()> + Sync) -> Result<()> {
        let workers = self.concurrency.min(items.len()).max(1);
        if workers == 1 {
            for it in items {
                f(it)?;
            }
            return Ok(());
        }
        let next = AtomicU64::new(0);
        let failure: Mutex<Option<String>> = Mutex::new(None);
        std::thread::scope(|s| {
            for _ in 0..workers {
                s.spawn(|| loop {
                    let i = next.fetch_add(1, Ordering::Relaxed) as usize;
                    let Some(item) = items.get(i) else { return };
                    if failure.lock().unwrap().is_some() {
                        return;
                    }
                    if let Err(e) = f(item) {
                        *failure.lock().unwrap() = Some(e.to_string());
                    }
                });
            }
        });
        match failure.into_inner().unwrap() {
            Some(e) => Err(anyhow!(e)),
            None => Ok(()),
        }
    }

    /// Bounded retry. Only transport faults and 5xx are retried: a 401 will
    /// stay a 401, and retrying a hash mismatch just wastes the build's time.
    fn send(
        &self,
        f: impl Fn() -> std::result::Result<ureq::Response, ureq::Error>,
    ) -> Result<ureq::Response> {
        let mut last = None;
        for attempt in 0..RETRY_ATTEMPTS {
            self.counters.requests.fetch_add(1, Ordering::Relaxed);
            match f() {
                Ok(r) => return Ok(r),
                Err(e) => {
                    let retryable = match &e {
                        ureq::Error::Status(code, _) => *code >= 500,
                        ureq::Error::Transport(_) => true,
                    };
                    last = Some(describe(e));
                    if !retryable {
                        break;
                    }
                    if attempt + 1 < RETRY_ATTEMPTS {
                        std::thread::sleep(Duration::from_millis(RETRY_BASE_MS << attempt));
                    }
                }
            }
        }
        Err(anyhow!(last.unwrap_or_else(|| "request failed".into())))
    }
}

/// Client for a remote execution worker.
///
/// Separate from [`Remote`] because they are separate services: a worker
/// executes, a cache stores. The worker publishes into the cache, so results
/// never flow through this connection — only control.
pub struct Executor {
    agent: ureq::Agent,
    base: String,
    endpoint: String,
    namespace: String,
    token: Option<String>,
    poll: Duration,
    timeout: Duration,
    requests: AtomicU64,
}

impl Executor {
    pub fn open(cfg: &RemoteConfig) -> std::result::Result<Executor, Disabled> {
        let cfg = effective_config(cfg);
        let exec = &cfg.execution;
        // Whether remote execution is *wanted* is the caller's decision — a
        // command-line flag can turn it on for one run. This only answers
        // whether a worker could be reached at all.
        if exec.url.trim().is_empty() {
            return Err(Disabled::NotConfigured);
        }
        if cfg.namespace.trim().is_empty() {
            return Err(Disabled::NoNamespace);
        }
        if !protocol::valid_namespace(cfg.namespace.trim()) {
            return Err(Disabled::Invalid(format!(
                "invalid [remote] namespace `{}`",
                cfg.namespace
            )));
        }
        let url = parse_endpoint(exec.url.trim()).map_err(Disabled::Invalid)?;
        let token_env = if exec.token_env.trim().is_empty() {
            cfg.token_env.as_str()
        } else {
            exec.token_env.as_str()
        };
        let token = read_token(token_env).map_err(Disabled::Invalid)?;
        let agent = ureq::AgentBuilder::new()
            .user_agent(USER_AGENT)
            // Same rule as the cache client: a redirect must never carry the
            // Authorization header to another host.
            .redirects(0)
            .timeout_connect(Duration::from_millis(cfg.connect_timeout_ms))
            .timeout_read(Duration::from_millis(cfg.request_timeout_ms))
            .timeout_write(Duration::from_millis(cfg.request_timeout_ms))
            .build();
        Ok(Executor {
            endpoint: url.host,
            base: format!("{}{}", url.base, execution::EXEC_PREFIX),
            namespace: cfg.namespace.trim().to_string(),
            token,
            poll: Duration::from_millis(exec.poll_ms.clamp(20, 10_000)),
            timeout: Duration::from_millis(exec.timeout_ms.max(1_000)),
            requests: AtomicU64::new(0),
            agent,
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
    pub fn namespace(&self) -> &str {
        &self.namespace
    }
    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }
    pub fn requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    fn auth(&self, r: ureq::Request) -> ureq::Request {
        self.requests.fetch_add(1, Ordering::Relaxed);
        match &self.token {
            Some(t) => r.set("Authorization", &format!("Bearer {t}")),
            None => r,
        }
    }

    pub fn capabilities(&self) -> Result<execution::Capabilities> {
        let url = format!("{}/capabilities", self.base);
        let resp = retry(|| self.auth(self.agent.get(&url)).call())?;
        let caps: execution::Capabilities = read_json(resp, 256 * 1024)?;
        if caps.protocol != execution::EXEC_PROTOCOL_VERSION {
            bail!(
                "worker speaks execution protocol v{}, this Arc speaks v{}",
                caps.protocol,
                execution::EXEC_PROTOCOL_VERSION
            );
        }
        Ok(caps)
    }

    /// Submit an execution. Idempotent on the execution key: a worker already
    /// running this exact execution returns that job rather than starting a
    /// second one, so a retried request cannot duplicate expensive work.
    pub fn submit(&self, req: &execution::ExecutionRequest) -> Result<execution::Job> {
        req.validate().map_err(|e| anyhow!(e))?;
        let url = format!("{}/{}/jobs", self.base, self.namespace);
        let body = serde_json::to_vec(req)?;
        let resp = retry(|| {
            self.auth(self.agent.post(&url))
                .set("Content-Type", "application/json")
                .send_bytes(&body)
        })?;
        let job: execution::Job = read_json(resp, protocol::MAX_METADATA_BYTES)?;
        job.validate(&req.execution_key).map_err(|e| anyhow!(e))?;
        Ok(job)
    }

    pub fn job(&self, id: &str, expect_key: &str) -> Result<execution::Job> {
        let url = format!("{}/{}/jobs/{}", self.base, self.namespace, job_id(id)?);
        let resp = retry(|| self.auth(self.agent.get(&url)).call())?;
        let job: execution::Job = read_json(resp, protocol::MAX_METADATA_BYTES)?;
        job.validate(expect_key).map_err(|e| anyhow!(e))?;
        Ok(job)
    }

    pub fn log(&self, id: &str, offset: u64) -> Result<execution::LogChunk> {
        let url = format!(
            "{}/{}/jobs/{}/log?offset={offset}",
            self.base,
            self.namespace,
            job_id(id)?
        );
        let resp = retry(|| self.auth(self.agent.get(&url)).call())?;
        read_json(resp, (execution::MAX_LOG_BYTES as usize) + 4096)
    }

    /// Ask the worker to stop. Advisory: other clients may be waiting on the
    /// same execution, and their work is not this client's to discard.
    pub fn cancel(&self, id: &str) -> Result<()> {
        let url = format!(
            "{}/{}/jobs/{}/cancel",
            self.base,
            self.namespace,
            job_id(id)?
        );
        self.auth(self.agent.post(&url)).call().ok();
        Ok(())
    }

    /// Poll until the job reaches a terminal state, forwarding new log output
    /// as it appears.
    pub fn wait(
        &self,
        job: execution::Job,
        expect_key: &str,
        mut on_log: impl FnMut(&str),
        cancelled: &dyn Fn() -> bool,
    ) -> Result<execution::Job> {
        let started = Instant::now();
        let mut job = job;
        let mut offset = 0u64;
        while !job.state.terminal() {
            if cancelled() {
                let _ = self.cancel(&job.id);
                bail!("cancelled");
            }
            if started.elapsed() > self.timeout {
                let _ = self.cancel(&job.id);
                bail!("remote execution exceeded the client timeout");
            }
            std::thread::sleep(self.poll);
            job = self.job(&job.id, expect_key)?;
            while offset < job.log_len {
                let chunk = match self.log(&job.id, offset) {
                    Ok(c) => c,
                    // Logs are informational; losing them must not fail a run.
                    Err(_) => break,
                };
                if chunk.text.is_empty() {
                    break;
                }
                on_log(&chunk.text);
                offset = chunk.offset + chunk.len;
            }
        }
        while offset < job.log_len {
            let Ok(chunk) = self.log(&job.id, offset) else {
                break;
            };
            if chunk.text.is_empty() {
                break;
            }
            on_log(&chunk.text);
            offset = chunk.offset + chunk.len;
        }
        Ok(job)
    }
}

fn job_id(id: &str) -> Result<&str> {
    anyhow::ensure!(
        !id.is_empty()
            && id.len() <= execution::MAX_JOB_ID_LEN
            && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "malformed job id"
    );
    Ok(id)
}

/// Bounded retry for the execution endpoints. Submission is included because it
/// is idempotent on the execution key.
fn retry(
    f: impl Fn() -> std::result::Result<ureq::Response, ureq::Error>,
) -> Result<ureq::Response> {
    let mut last = None;
    for attempt in 0..RETRY_ATTEMPTS {
        match f() {
            Ok(r) => return Ok(r),
            Err(e) => {
                let retryable = match &e {
                    ureq::Error::Status(code, _) => *code >= 500,
                    ureq::Error::Transport(_) => true,
                };
                last = Some(describe(e));
                if !retryable {
                    break;
                }
                if attempt + 1 < RETRY_ATTEMPTS {
                    std::thread::sleep(Duration::from_millis(RETRY_BASE_MS << attempt));
                }
            }
        }
    }
    Err(anyhow!(last.unwrap_or_else(|| "request failed".into())))
}

/// Errors never carry the request URL's credentials because Arc never puts any
/// there, and never carry the Authorization header because it is not echoed.
fn describe(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(401, _) => "not authorised (401)".into(),
        ureq::Error::Status(403, _) => "forbidden (403)".into(),
        ureq::Error::Status(404, _) => "not found (404)".into(),
        ureq::Error::Status(code, resp) => {
            let body = resp
                .into_string()
                .ok()
                .and_then(|s| serde_json::from_str::<protocol::ErrorBody>(&s).ok())
                .map(|b| b.error)
                .unwrap_or_default();
            if body.is_empty() {
                format!("server returned {code}")
            } else {
                format!("server returned {code}: {body}")
            }
        }
        ureq::Error::Transport(t) => format!("{t}"),
    }
}

fn is_not_found(e: &anyhow::Error) -> bool {
    e.to_string().contains("(404)")
}

fn read_json<T: serde::de::DeserializeOwned>(resp: ureq::Response, limit: usize) -> Result<T> {
    let mut buf = Vec::new();
    resp.into_reader()
        .take(limit as u64 + 1)
        .read_to_end(&mut buf)?;
    if buf.len() > limit {
        bail!("response exceeds the {limit} byte metadata limit");
    }
    serde_json::from_slice(&buf).context("malformed response from remote cache")
}

fn deflate(r: &mut impl Read) -> Result<Vec<u8>> {
    let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
    std::io::copy(r, &mut enc)?;
    Ok(enc.finish()?)
}

struct Counted<R> {
    inner: R,
    seen: std::sync::Arc<AtomicU64>,
}

impl<R: Read> Counted<R> {
    fn new(inner: R) -> Counted<R> {
        Counted {
            inner,
            seen: std::sync::Arc::new(AtomicU64::new(0)),
        }
    }
}

impl<R: Read> Read for Counted<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.seen.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

struct Endpoint {
    base: String,
    host: String,
}

/// Accept exactly what Arc will talk to: an `http`/`https` origin with an
/// optional path prefix. Credentials in the URL are refused rather than sent.
fn parse_endpoint(raw: &str) -> std::result::Result<Endpoint, String> {
    let url = url::Url::parse(raw).map_err(|e| format!("invalid [remote] url `{raw}`: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "[remote] url must be http or https, not `{}`",
            url.scheme()
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("[remote] url must not embed credentials".into());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("[remote] url must not carry a query or fragment".into());
    }
    let host = url
        .host_str()
        .ok_or_else(|| format!("[remote] url `{raw}` has no host"))?
        .to_string();
    let mut base = url.as_str().trim_end_matches('/').to_string();
    if base.ends_with(protocol::PROTOCOL_PREFIX) {
        base.truncate(base.len() - protocol::PROTOCOL_PREFIX.len());
    }
    let host = match url.port() {
        Some(p) => format!("{host}:{p}"),
        None => host,
    };
    Ok(Endpoint { base, host })
}

fn read_token(var: &str) -> std::result::Result<Option<String>, String> {
    let var = var.trim();
    if var.is_empty() {
        return Ok(None);
    }
    if !var.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Err(format!("invalid [remote] token_env name `{var}`"));
    }
    match std::env::var(var) {
        Ok(v) if !v.trim().is_empty() => Ok(Some(v.trim().to_string())),
        _ => Ok(None),
    }
}

/// The local record a remote one authorises, once its key has already been
/// matched against the key this machine computed.
pub fn to_record(rec: &RemoteExecution, local: &ExecutionRecord) -> Result<ExecutionRecord> {
    let mut outputs = Vec::with_capacity(rec.outputs.len());
    for o in &rec.outputs {
        outputs.push(OutputFile {
            rel: o.path.decode().map_err(|e| anyhow!(e))?,
            digest: o.digest.clone(),
            size: o.size,
            exec: o.exec,
        });
    }
    Ok(ExecutionRecord {
        exit_code: rec.exit_code,
        duration_ms: rec.duration_ms,
        stdout: rec.stdout.as_ref().map(blob),
        stderr: rec.stderr.as_ref().map(blob),
        outputs,
        ..local.clone()
    })
}

fn blob(b: &WireBlob) -> BlobRef {
    BlobRef {
        digest: b.digest.clone(),
        size: b.size,
    }
}

pub fn from_record(rec: &ExecutionRecord) -> RemoteExecution {
    RemoteExecution {
        protocol: protocol::PROTOCOL_VERSION,
        key_semantics: crate::SCHEMA_VERSION,
        execution_key: rec.key.clone(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        program: rec.program.clone(),
        args: rec.args.clone(),
        rel_cwd: rec.rel_cwd.clone(),
        family_key: rec.family_key.clone(),
        exit_code: rec.exit_code,
        duration_ms: rec.duration_ms,
        outputs: rec
            .outputs
            .iter()
            .map(|o| WireOutput {
                path: WirePath::from_rel(&o.rel),
                digest: o.digest.clone(),
                size: o.size,
                exec: o.exec,
            })
            .collect(),
        stdout: rec.stdout.as_ref().map(wire),
        stderr: rec.stderr.as_ref().map(wire),
        arc_version: rec.arc_version.clone(),
    }
}

/// Publishable form of a learned task row.
pub fn task_from_node(node: &crate::graph::TaskNode) -> protocol::RemoteTask {
    use crate::graph::Consumes;
    protocol::RemoteTask {
        protocol: protocol::PROTOCOL_VERSION,
        graph_semantics: crate::graph::GRAPH_SCHEMA_VERSION,
        dependency_semantics: crate::dependency::DEPENDENCY_SCHEMA_VERSION,
        trace_semantics: crate::trace::TRACE_SEMANTICS_VERSION,
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        family_key: node.family_key.clone(),
        program: node.program.clone(),
        args: node.args.clone(),
        rel_cwd: node.rel_cwd.clone(),
        completeness: node.completeness.label().to_string(),
        inputs_narrowed: node.inputs_narrowed,
        produces: node
            .produces
            .iter()
            .map(|p| WirePath::from_rel(p))
            .collect(),
        consumes: node
            .consumes
            .iter()
            .map(|c| protocol::WireConsumed {
                path: WirePath::from_rel(&c.path),
                kind: match c.kind {
                    Consumes::File => protocol::WireConsumes::File,
                    Consumes::Directory => protocol::WireConsumes::Directory,
                    Consumes::Existence => protocol::WireConsumes::Existence,
                },
            })
            .collect(),
        declared_inputs: node.declared_inputs.clone(),
        observations: node.observations,
        arc_version: crate::VERSION.to_string(),
    }
}

fn wire(b: &BlobRef) -> WireBlob {
    WireBlob {
        digest: b.digest.clone(),
        size: b.size,
    }
}

/// Identity of a namespace derived from a project, for the default `arc.toml`
/// example and for `arc remote status` to show. Never derived from a URL that
/// might carry credentials.
pub fn suggested_namespace(project_root: &std::path::Path) -> String {
    let name = project_root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut h = Hasher::new();
    h.field(crate::paths::display_form(project_root).as_bytes());
    let sanitised: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(32)
        .collect();
    if sanitised.is_empty() {
        h.finish().hex()[..16].to_string()
    } else {
        sanitised
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_are_validated() {
        assert!(parse_endpoint("http://user:pw@host/").is_err());
        assert!(parse_endpoint("ftp://host/").is_err());
        assert!(parse_endpoint("not a url").is_err());
        assert!(parse_endpoint("http://host/?a=1").is_err());
        assert_eq!(
            parse_endpoint("http://127.0.0.1:7890/").unwrap().base,
            "http://127.0.0.1:7890"
        );
        // A user who pastes the versioned URL gets the same endpoint.
        assert_eq!(
            parse_endpoint("https://cache.example.com/v1").unwrap().base,
            "https://cache.example.com"
        );
        assert_eq!(
            parse_endpoint("https://cache.example.com").unwrap().host,
            "cache.example.com"
        );
    }

    #[test]
    fn a_remote_without_a_namespace_is_not_used() {
        let cfg = RemoteConfig {
            url: "http://127.0.0.1:1".into(),
            ..Default::default()
        };
        assert_eq!(Remote::open(&cfg).err(), Some(Disabled::NoNamespace));
        let cfg = RemoteConfig::default();
        assert_eq!(Remote::open(&cfg).err(), Some(Disabled::NotConfigured));
    }

    #[test]
    fn a_token_env_name_is_not_shell_syntax() {
        assert!(read_token("$(cat /etc/passwd)").is_err());
        assert!(read_token("ARC_CACHE_TOKEN").is_ok());
        assert_eq!(read_token("").unwrap(), None);
    }
}
