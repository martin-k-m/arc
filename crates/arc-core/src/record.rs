//! The durable record of one execution.

use crate::dependency::Completeness;
use crate::key::{EnvFingerprint, Toolchain};
use serde::{Deserialize, Serialize};

/// Render a program and its arguments the way a shell would show them.
pub fn format_command(program: &str, args: &[String]) -> String {
    let mut s = program.to_string();
    for a in args {
        s.push(' ');
        if a.contains(' ') {
            s.push('"');
            s.push_str(a);
            s.push('"');
        } else {
            s.push_str(a);
        }
    }
    s
}

/// What one traced execution observed, in counts. The paths themselves live in
/// the family's dependency set, so history does not carry a copy per run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceSummary {
    pub backend: String,
    pub completeness: Completeness,
    pub processes: usize,
    pub files_observed: usize,
    pub inputs: usize,
    #[serde(default)]
    pub directories: usize,
    #[serde(default)]
    pub absent: usize,
    pub outputs: usize,
    pub executables: usize,
    pub lossy: bool,
    /// Human-readable reasons the model is not complete. Empty when it is.
    #[serde(default)]
    pub downgrades: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheStatus {
    Hit,
    Miss,
    /// Caching was disabled or unsafe for this run.
    Bypass,
}

impl CacheStatus {
    pub fn label(&self) -> &'static str {
        match self {
            CacheStatus::Hit => "HIT",
            CacheStatus::Miss => "MISS",
            CacheStatus::Bypass => "BYPASS",
        }
    }
}

/// Where a hit's result came from. Records written before v0.5 predate remote
/// caching entirely, so they default to local.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheSource {
    #[default]
    Local,
    Remote,
}

impl CacheSource {
    pub fn label(&self) -> &'static str {
        match self {
            CacheSource::Local => "local",
            CacheSource::Remote => "remote",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlobRef {
    pub digest: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputFile {
    pub rel: String,
    pub digest: String,
    pub size: u64,
    pub exec: bool,
}

/// What an execution's environment was, for `arc inspect`. The id is the part
/// that matters — the alias is a name this project happened to use.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedEnvironment {
    pub alias: String,
    pub id: String,
    /// `hermetic`, `host-dependent` or `unknown`, per this execution.
    #[serde(default)]
    pub hermeticity: String,
}

/// The parts of the execution key that a record does not otherwise carry.
///
/// Inputs, environment variables and the toolchain each already have a field on
/// [`ExecutionRecord`], so a miss caused by one of those could always be named.
/// The rest of `key::KeyInputs` could not be, and a miss caused by any of them
/// was reported as one three-way lump: "toolchain, observed dependencies or
/// execution policy changed". Recording them makes every component of the key
/// attributable.
///
/// `Option` on the record and `default` on every field, because a record
/// written before this existed must still read — it simply cannot be diffed
/// this way, and the reason Arc reports says so rather than guessing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyComponents {
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub arch: String,
    /// Digest of the learned dependencies that participate in the key.
    #[serde(default)]
    pub dependency_digest: String,
    #[serde(default)]
    pub output_globs: Vec<String>,
    /// Empty when the host's toolchain was used rather than an Arc environment.
    #[serde(default)]
    pub environment_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionRecord {
    pub schema: u32,
    pub id: String,
    pub key: String,
    pub program: String,
    pub args: Vec<String>,
    pub project_root: String,
    pub rel_cwd: String,
    pub started_at: i64,
    pub duration_ms: u64,
    pub exit_code: i32,
    pub input_digest: String,
    pub input_file_count: usize,
    pub env: EnvFingerprint,
    pub toolchain: Toolchain,
    pub stdout: Option<BlobRef>,
    pub stderr: Option<BlobRef>,
    pub outputs: Vec<OutputFile>,
    pub cache_status: CacheStatus,
    #[serde(default)]
    pub cache_source: CacheSource,
    /// For a hit, the execution whose result was reused.
    pub replayed_from: Option<String>,
    /// Blob holding the `(path, digest)` list of inputs, for change explanation.
    pub input_manifest: Option<BlobRef>,
    pub arc_version: String,
    /// Identity of the execution kind this run belongs to. `default` keeps
    /// records written by older versions readable.
    #[serde(default)]
    pub family_key: String,
    #[serde(default)]
    pub trace: Option<TraceSummary>,
    /// The execution environment this ran inside, when one was in force.
    /// `default` so records written before v0.8 still read.
    #[serde(default)]
    pub environment: Option<RecordedEnvironment>,
    /// The remaining execution-key components, so a miss can name which one
    /// moved. `None` for records written before Arc recorded them.
    #[serde(default)]
    pub key_components: Option<KeyComponents>,
}

impl ExecutionRecord {
    pub fn command_line(&self) -> String {
        format_command(&self.program, &self.args)
    }

    /// Every blob this record depends on. Used by garbage collection.
    pub fn blob_digests(&self) -> Vec<String> {
        let mut v = self.replay_digests();
        v.extend(self.input_manifest.iter().map(|b| b.digest.clone()));
        v
    }

    /// The blobs a replay actually needs. The input manifest is excluded: it
    /// only explains *why* a past run missed, so losing it must not force a
    /// re-execution of a result that is otherwise complete.
    pub fn replay_digests(&self) -> Vec<String> {
        let mut v: Vec<String> = self.outputs.iter().map(|o| o.digest.clone()).collect();
        v.extend(self.stdout.iter().map(|b| b.digest.clone()));
        v.extend(self.stderr.iter().map(|b| b.digest.clone()));
        v
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    pub execution_id: String,
    pub created_at: i64,
    pub last_accessed: i64,
    pub hits: u64,
    /// Wall-clock milliseconds the original execution took.
    pub original_duration_ms: u64,
}
