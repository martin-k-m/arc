//! The durable record of one execution.

use crate::key::{EnvFingerprint, Toolchain};
use serde::{Deserialize, Serialize};

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
    /// For a hit, the execution whose result was reused.
    pub replayed_from: Option<String>,
    /// Blob holding the `(path, digest)` list of inputs, for change explanation.
    pub input_manifest: Option<BlobRef>,
    pub arc_version: String,
}

impl ExecutionRecord {
    pub fn command_line(&self) -> String {
        let mut s = self.program.clone();
        for a in &self.args {
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

    /// Every blob this record depends on. Used by garbage collection.
    pub fn blob_digests(&self) -> Vec<String> {
        let mut v: Vec<String> = self.outputs.iter().map(|o| o.digest.clone()).collect();
        v.extend(self.stdout.iter().map(|b| b.digest.clone()));
        v.extend(self.stderr.iter().map(|b| b.digest.clone()));
        v.extend(self.input_manifest.iter().map(|b| b.digest.clone()));
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
