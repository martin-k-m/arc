//! The remote *execution* wire format.
//!
//! Layered on the v0.5 cache protocol rather than replacing it: a worker
//! executes a miss and publishes the result into the same content-addressed
//! cache, so every machine afterwards gets an ordinary remote cache hit. The
//! execution endpoints therefore carry control, never file contents.
//!
//! Both directions are untrusted. A worker validates every request before it
//! touches its filesystem; a client validates every result before it touches
//! the project.

use super::protocol::{valid_digest, WireBlob, WireOutput, WirePath};
use serde::{Deserialize, Serialize};

/// Bumped when the meaning of any execution endpoint or payload changes
/// incompatibly. Independent of the cache protocol version: a server may speak
/// one and not the other.
pub const EXEC_PROTOCOL_VERSION: u32 = 1;
pub const EXEC_PREFIX: &str = "/v1/exec";

pub const MAX_MANIFEST_ENTRIES: usize = 200_000;
pub const MAX_ARGS: usize = 4_096;
pub const MAX_ARG_BYTES: usize = 1 << 20;
pub const MAX_ENV_VARS: usize = 1_024;
pub const MAX_ENV_BYTES: usize = 1 << 20;
pub const MAX_TOOLS: usize = 1_024;
pub const MAX_REQUEST_BYTES: usize = 64 << 20;
pub const MAX_LOG_BYTES: u64 = 8 << 20;
pub const MAX_JOB_ID_LEN: usize = 64;

pub const DEFAULT_TIMEOUT_MS: u64 = 30 * 60 * 1_000;

/// What a worker will and will not do. A client compares this against its own
/// platform before it dispatches anything.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capabilities {
    pub protocol: u32,
    /// Arc's cache-semantics version. A worker whose notion of what an
    /// execution key covers differs from the client's may not execute for it.
    pub key_semantics: u32,
    pub worker: String,
    pub version: String,
    pub os: String,
    pub arch: String,
    /// Identity of the execution environment beyond OS and architecture. See
    /// [`environment_id`].
    pub environment_id: String,
    pub max_jobs: usize,
    pub queue_limit: usize,
    pub active: usize,
    pub queued: usize,
    /// Honest statement of what the sandbox does about the network.
    pub network: NetworkPolicy,
    /// The shared cache this worker reads inputs from and publishes results to.
    /// A client whose cache differs cannot use this worker.
    pub cache_endpoint: Option<String>,
}

/// The reference worker isolates the filesystem, the environment and the
/// process tree. It does not isolate the network, and says so rather than
/// implying a guarantee it cannot keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkPolicy {
    /// Commands reach whatever the worker host can reach.
    Unrestricted,
}

impl NetworkPolicy {
    pub fn label(&self) -> &'static str {
        match self {
            NetworkPolicy::Unrestricted => "unrestricted",
        }
    }
}

/// A conservative identity for "an environment where the same command produces
/// the same result".
///
/// It covers what Arc can actually establish: the platform, the semantics
/// version, and the libc flavour where that is knowable. It deliberately does
/// **not** claim to cover every installed shared library — see
/// `docs/correctness.md`. Its job is to make an obviously different environment
/// obviously different, not to certify equivalence.
pub fn environment_id() -> String {
    let mut h = crate::hash::Hasher::new();
    h.field(std::env::consts::OS);
    h.field(std::env::consts::ARCH);
    h.field(std::env::consts::FAMILY);
    h.field(crate::SCHEMA_VERSION.to_le_bytes());
    h.field(libc_flavour());
    h.finish().hex()[..16].to_string()
}

#[cfg(target_os = "linux")]
fn libc_flavour() -> &'static str {
    // musl static binaries have no interpreter; glibc systems have one at a
    // well-known path. Crude, but it separates the two ABIs that matter.
    if std::path::Path::new("/lib/ld-musl-x86_64.so.1").exists()
        || std::path::Path::new("/lib/ld-musl-aarch64.so.1").exists()
    {
        "musl"
    } else {
        "gnu"
    }
}

#[cfg(not(target_os = "linux"))]
fn libc_flavour() -> &'static str {
    "n/a"
}

/// An executable the command needs, identified by content rather than by a
/// version string.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolRequirement {
    /// As the client resolved it: a bare name to look up on the worker's PATH,
    /// or an absolute path that must exist on the worker with this digest.
    pub program: String,
    pub digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub path: WirePath,
    pub digest: String,
    pub size: u64,
    pub exec: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Limits {
    pub timeout_ms: u64,
    pub max_output_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            timeout_ms: DEFAULT_TIMEOUT_MS,
            max_output_bytes: 8 << 30,
        }
    }
}

/// Everything a worker needs to reproduce one execution, and nothing else.
///
/// Inputs are referenced by digest, never inlined: the worker fetches what it
/// does not already hold from the shared cache, so a repeated execution
/// transfers nothing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionRequest {
    pub protocol: u32,
    pub key_semantics: u32,
    pub execution_key: String,
    /// Identity of the execution kind, carried so the worker can publish a
    /// well-formed cache record without inferring one.
    pub family_key: String,
    pub os: String,
    pub arch: String,
    pub program: String,
    pub args: Vec<String>,
    /// Working directory relative to the workspace root. Never a coordinator
    /// absolute path.
    pub rel_cwd: String,
    /// Only the variables this execution's identity actually depends on.
    pub env: Vec<(String, String)>,
    pub inputs: Vec<ManifestEntry>,
    pub output_globs: Vec<String>,
    pub tools: Vec<ToolRequirement>,
    pub limits: Limits,
    /// Whether a non-zero exit may be published to the shared cache.
    pub cache_failures: bool,
    pub arc_version: String,
}

impl ExecutionRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.protocol != EXEC_PROTOCOL_VERSION {
            return Err(format!("unsupported execution protocol {}", self.protocol));
        }
        if !valid_digest(&self.execution_key) {
            return Err("malformed execution key".into());
        }
        if self.program.is_empty() || self.program.len() > 4096 {
            return Err("malformed program".into());
        }
        if self.program.contains('\0') {
            return Err("illegal character in program".into());
        }
        if self.args.len() > MAX_ARGS {
            return Err(format!("{} arguments is too many", self.args.len()));
        }
        let arg_bytes: usize = self.args.iter().map(|a| a.len()).sum();
        if arg_bytes > MAX_ARG_BYTES {
            return Err("argument list is too large".into());
        }
        if self.env.len() > MAX_ENV_VARS {
            return Err("too many environment variables".into());
        }
        let env_bytes: usize = self.env.iter().map(|(k, v)| k.len() + v.len()).sum();
        if env_bytes > MAX_ENV_BYTES {
            return Err("environment is too large".into());
        }
        for (k, _) in &self.env {
            if k.is_empty() || k.contains('\0') || k.contains('=') {
                return Err(format!("malformed environment variable name `{k}`"));
            }
        }
        if self.tools.len() > MAX_TOOLS {
            return Err("too many tool requirements".into());
        }
        for t in &self.tools {
            if !valid_digest(&t.digest) || t.program.contains('\0') {
                return Err("malformed tool requirement".into());
            }
        }
        if self.inputs.len() > MAX_MANIFEST_ENTRIES {
            return Err(format!("{} input entries is too many", self.inputs.len()));
        }
        // Every path is validated here, before anything is written, so a
        // traversal attempt cannot reach the worker's filesystem at all.
        let mut seen = std::collections::HashSet::with_capacity(self.inputs.len());
        for e in &self.inputs {
            let rel = e.path.decode()?;
            if !seen.insert(rel.clone()) {
                return Err(format!("duplicate input entry `{rel}`"));
            }
            if !valid_digest(&e.digest) {
                return Err(format!("malformed digest for `{rel}`"));
            }
        }
        if !self.rel_cwd.is_empty() {
            super::protocol::validate_rel_path(&self.rel_cwd)?;
        }
        for g in &self.output_globs {
            if g.len() > 4096 || g.contains('\0') {
                return Err("malformed output glob".into());
            }
        }
        if self.limits.timeout_ms == 0 {
            return Err("timeout must be positive".into());
        }
        Ok(())
    }

    /// Whether this worker may run this request at all. Checked at submission
    /// and again immediately before execution, because capacity and toolchains
    /// can change in between.
    pub fn compatible_with(&self, caps: &Capabilities) -> Result<(), String> {
        if self.protocol != caps.protocol {
            return Err(format!(
                "worker speaks execution protocol v{}, this Arc speaks v{}",
                caps.protocol, self.protocol
            ));
        }
        if self.key_semantics != caps.key_semantics {
            return Err(format!(
                "worker uses cache semantics v{} (this Arc uses v{})",
                caps.key_semantics, self.key_semantics
            ));
        }
        if self.os != caps.os || self.arch != caps.arch {
            return Err(format!(
                "worker is {}/{}, this execution needs {}/{}",
                caps.os, caps.arch, self.os, self.arch
            ));
        }
        Ok(())
    }

    pub fn digests(&self) -> Vec<String> {
        let mut v: Vec<String> = self.inputs.iter().map(|e| e.digest.clone()).collect();
        v.sort();
        v.dedup();
        v
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Queued,
    Running,
    /// The command reached a conclusion. Says nothing about its exit code.
    Completed,
    /// The command never reached a conclusion: the environment, not the code,
    /// is what failed.
    Failed,
    Cancelled,
    /// The worker restarted while this job was running. Nothing is known about
    /// what the command did.
    Lost,
}

impl JobState {
    pub fn terminal(&self) -> bool {
        !matches!(self, JobState::Queued | JobState::Running)
    }

    pub fn label(&self) -> &'static str {
        match self {
            JobState::Queued => "queued",
            JobState::Running => "running",
            JobState::Completed => "completed",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
            JobState::Lost => "lost",
        }
    }
}

/// Why a job did not produce a result. Deliberately distinct from the command's
/// own exit code: "the tests failed" and "the worker fell over" call for
/// different responses, and flattening both to 1 loses the difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// The worker cannot run this execution: platform, semantics or toolchain.
    Incompatible,
    /// An input object could not be obtained or did not verify.
    InputUnavailable,
    Sandbox,
    Timeout,
    Cancelled,
    Overloaded,
    Internal,
}

impl FailureKind {
    pub fn label(&self) -> &'static str {
        match self {
            FailureKind::Incompatible => "incompatible",
            FailureKind::InputUnavailable => "input unavailable",
            FailureKind::Sandbox => "sandbox",
            FailureKind::Timeout => "timeout",
            FailureKind::Cancelled => "cancelled",
            FailureKind::Overloaded => "overloaded",
            FailureKind::Internal => "internal",
        }
    }

    /// Whether a client may safely run the command itself instead. A timeout or
    /// a cancellation means the command may still be running somewhere, so
    /// re-running it locally could duplicate side effects.
    pub fn safe_to_run_locally(&self) -> bool {
        matches!(
            self,
            FailureKind::Incompatible
                | FailureKind::InputUnavailable
                | FailureKind::Overloaded
                | FailureKind::Sandbox
                | FailureKind::Internal
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobError {
    pub kind: FailureKind,
    pub message: String,
}

/// What the command did. Object contents live in the shared cache; this names
/// them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionResult {
    pub exit_code: i32,
    /// Killed by a signal. The result says nothing about the inputs, so it is
    /// never cacheable.
    pub signaled: bool,
    pub duration_ms: u64,
    pub outputs: Vec<WireOutput>,
    pub stdout: Option<WireBlob>,
    pub stderr: Option<WireBlob>,
    /// Output exceeded the capture limit, so it cannot be replayed faithfully.
    pub truncated: bool,
    /// The worker published an execution record for this key, so other machines
    /// will find it in the cache.
    pub published: bool,
}

impl ExecutionResult {
    pub fn validate(&self) -> Result<(), String> {
        if self.outputs.len() > super::protocol::MAX_OUTPUT_ENTRIES {
            return Err("too many output entries".into());
        }
        let mut seen = std::collections::HashSet::with_capacity(self.outputs.len());
        for o in &self.outputs {
            let rel = o.path.decode()?;
            if !seen.insert(rel.clone()) {
                return Err(format!("duplicate output entry `{rel}`"));
            }
            if !valid_digest(&o.digest) {
                return Err(format!("malformed digest for `{rel}`"));
            }
        }
        for b in [&self.stdout, &self.stderr].into_iter().flatten() {
            if !valid_digest(&b.digest) {
                return Err("malformed stream digest".into());
            }
        }
        Ok(())
    }

    pub fn digests(&self) -> Vec<String> {
        let mut v: Vec<String> = self.outputs.iter().map(|o| o.digest.clone()).collect();
        v.extend(self.stdout.iter().map(|b| b.digest.clone()));
        v.extend(self.stderr.iter().map(|b| b.digest.clone()));
        v.sort();
        v.dedup();
        v
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub execution_key: String,
    pub state: JobState,
    pub result: Option<ExecutionResult>,
    pub error: Option<JobError>,
    /// Bytes of log available, for incremental fetching.
    pub log_len: u64,
    pub worker: String,
    /// How many clients are waiting on this job. A cancellation from one of
    /// several waiters is advisory.
    pub waiters: usize,
    pub queued_ms: u64,
}

impl Job {
    pub fn validate(&self, expect_key: &str) -> Result<(), String> {
        if self.id.is_empty() || self.id.len() > MAX_JOB_ID_LEN {
            return Err("malformed job id".into());
        }
        if !self
            .id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err("malformed job id".into());
        }
        // A worker must not be able to answer a question that was not asked.
        if self.execution_key != expect_key {
            return Err("job does not describe the requested execution".into());
        }
        if let Some(r) = &self.result {
            r.validate()?;
        }
        if self.state == JobState::Completed && self.result.is_none() {
            return Err("completed job carries no result".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogChunk {
    pub offset: u64,
    pub len: u64,
    pub total: u64,
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::protocol::PathEncoding;

    fn request() -> ExecutionRequest {
        ExecutionRequest {
            protocol: EXEC_PROTOCOL_VERSION,
            key_semantics: crate::SCHEMA_VERSION,
            execution_key: "a".repeat(64),
            family_key: "f".repeat(64),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            program: "sh".into(),
            args: vec!["-c".into(), "true".into()],
            rel_cwd: String::new(),
            env: vec![("PATH".into(), "/usr/bin".into())],
            inputs: vec![ManifestEntry {
                path: WirePath::from_rel("src/main.rs"),
                digest: "b".repeat(64),
                size: 3,
                exec: false,
            }],
            output_globs: vec!["out/**".into()],
            tools: vec![ToolRequirement {
                program: "sh".into(),
                digest: "c".repeat(64),
            }],
            limits: Limits::default(),
            cache_failures: false,
            arc_version: crate::VERSION.into(),
        }
    }

    fn capabilities() -> Capabilities {
        Capabilities {
            protocol: EXEC_PROTOCOL_VERSION,
            key_semantics: crate::SCHEMA_VERSION,
            worker: "arc-worker".into(),
            version: crate::VERSION.into(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            environment_id: environment_id(),
            max_jobs: 4,
            queue_limit: 64,
            active: 0,
            queued: 0,
            network: NetworkPolicy::Unrestricted,
            cache_endpoint: None,
        }
    }

    #[test]
    fn a_valid_request_passes_and_a_traversing_one_does_not() {
        assert!(request().validate().is_ok());

        let mut r = request();
        r.inputs[0].path = WirePath::from_rel("../../etc/shadow");
        assert!(r.validate().is_err());

        let mut r = request();
        r.inputs[0].path = WirePath::from_rel("/etc/shadow");
        assert!(r.validate().is_err());

        let mut r = request();
        r.inputs[0].path = WirePath {
            enc: PathEncoding::B64,
            v: crate::remote::protocol::base64_encode(b"../escape"),
        };
        assert!(r.validate().is_err());

        let mut r = request();
        r.rel_cwd = "../elsewhere".into();
        assert!(r.validate().is_err());
    }

    #[test]
    fn oversized_requests_are_refused_rather_than_materialised() {
        let mut r = request();
        r.args = vec!["x".repeat(64); MAX_ARGS + 1];
        assert!(r.validate().is_err());

        let mut r = request();
        r.args = vec!["x".repeat(MAX_ARG_BYTES + 1)];
        assert!(r.validate().is_err());

        let mut r = request();
        r.env = (0..MAX_ENV_VARS + 1)
            .map(|i| (format!("V{i}"), String::new()))
            .collect();
        assert!(r.validate().is_err());

        let mut r = request();
        r.env = vec![("A".into(), "x".repeat(MAX_ENV_BYTES + 1))];
        assert!(r.validate().is_err());

        let mut r = request();
        r.env = vec![("BAD=NAME".into(), "x".into())];
        assert!(r.validate().is_err());
    }

    #[test]
    fn a_duplicate_input_entry_is_refused() {
        let mut r = request();
        r.inputs.push(r.inputs[0].clone());
        assert!(r.validate().is_err());
    }

    #[test]
    fn platform_and_semantics_mismatches_are_refused() {
        let caps = capabilities();
        assert!(request().compatible_with(&caps).is_ok());

        let mut r = request();
        r.os = "plan9".into();
        assert!(r.compatible_with(&caps).is_err());

        let mut r = request();
        r.arch = "sparc64".into();
        assert!(r.compatible_with(&caps).is_err());

        let mut r = request();
        r.key_semantics += 1;
        assert!(r.compatible_with(&caps).is_err());

        let mut r = request();
        r.protocol += 1;
        assert!(r.compatible_with(&caps).is_err());
    }

    #[test]
    fn a_job_cannot_answer_a_question_that_was_not_asked() {
        let job = Job {
            id: "job-1".into(),
            execution_key: "a".repeat(64),
            state: JobState::Completed,
            result: Some(ExecutionResult {
                exit_code: 0,
                signaled: false,
                duration_ms: 5,
                outputs: vec![],
                stdout: None,
                stderr: None,
                truncated: false,
                published: true,
            }),
            error: None,
            log_len: 0,
            worker: "w".into(),
            waiters: 1,
            queued_ms: 0,
        };
        assert!(job.validate(&"a".repeat(64)).is_ok());
        assert!(job.validate(&"b".repeat(64)).is_err());

        let mut bad = job.clone();
        bad.result = None;
        assert!(bad.validate(&"a".repeat(64)).is_err());

        let mut bad = job.clone();
        bad.id = "../../etc".into();
        assert!(bad.validate(&"a".repeat(64)).is_err());
    }

    #[test]
    fn a_result_naming_an_escaping_output_is_refused() {
        let mut r = ExecutionResult {
            exit_code: 0,
            signaled: false,
            duration_ms: 1,
            outputs: vec![WireOutput {
                path: WirePath::from_rel("../escape"),
                digest: "d".repeat(64),
                size: 1,
                exec: false,
            }],
            stdout: None,
            stderr: None,
            truncated: false,
            published: false,
        };
        assert!(r.validate().is_err());
        r.outputs[0].path = WirePath::from_rel("out/ok");
        assert!(r.validate().is_ok());
    }

    #[test]
    fn infrastructure_failures_that_may_have_started_the_command_are_not_safe_to_repeat() {
        assert!(FailureKind::Incompatible.safe_to_run_locally());
        assert!(FailureKind::Overloaded.safe_to_run_locally());
        assert!(!FailureKind::Timeout.safe_to_run_locally());
        assert!(!FailureKind::Cancelled.safe_to_run_locally());
    }
}
