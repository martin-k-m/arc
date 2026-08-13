//! What a tracer observed, before any policy is applied.
//!
//! This is the raw vocabulary. It is deliberately wider than any current
//! backend can fill, so a future backend that sees more does not force a
//! database migration — unsupported variants simply never appear.

use crate::paths::Scope;
use serde::{Deserialize, Serialize};

/// Bumped when the meaning of an observation changes. A dependency set recorded
/// under an older trace schema is not reinterpreted under a newer one.
pub const TRACE_SCHEMA_VERSION: u32 = 1;

/// Operations a tracer may report against a path.
///
/// `Read`, `Rename` and `Execute` exist in the model but are not produced by
/// any backend shipping in this version; see [`Capabilities`](super::Capabilities).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileOp {
    Read,
    Write,
    Create,
    Delete,
    Rename,
    Execute,
}

impl FileOp {
    pub fn label(&self) -> &'static str {
        match self {
            FileOp::Read => "read",
            FileOp::Write => "write",
            FileOp::Create => "create",
            FileOp::Delete => "delete",
            FileOp::Rename => "rename",
            FileOp::Execute => "execute",
        }
    }

    /// Whether the operation, on its own, makes the path a candidate *input*.
    /// Writes and deletes describe what the command produced, not what it
    /// consumed.
    pub fn is_input(&self) -> bool {
        matches!(self, FileOp::Read | FileOp::Execute)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileObservation {
    /// Display-form absolute path (see `crate::paths`).
    pub path: String,
    /// Project-relative path when the file is inside the project.
    pub rel: Option<String>,
    pub op: FileOp,
    pub scope: Scope,
    /// Whether the path existed when the execution started. `None` when the
    /// backend could not tell.
    pub existed_before: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessObservation {
    pub pid: u32,
    /// Display-form path of the executable image, when it could be resolved
    /// before the process exited.
    pub image: Option<String>,
    pub scope: Option<Scope>,
    /// True when the process was observed being created during the run rather
    /// than being the command Arc spawned itself.
    pub descendant: bool,
}

/// Everything one backend saw during one execution.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Observations {
    pub files: Vec<FileObservation>,
    pub processes: Vec<ProcessObservation>,
    /// Set when the backend knows it dropped events (a process exited before it
    /// could be identified, a buffer overflowed, a directory could not be
    /// walked). Forces the resulting dependency set to be treated as lossy even
    /// within the backend's declared capabilities.
    pub lossy: bool,
    /// Human-readable notes surfaced by `arc run --trace --verbose`.
    pub notes: Vec<String>,
}

impl Observations {
    pub fn merge(&mut self, other: Observations) {
        self.files.extend(other.files);
        self.processes.extend(other.processes);
        self.lossy |= other.lossy;
        self.notes.extend(other.notes);
    }
}
