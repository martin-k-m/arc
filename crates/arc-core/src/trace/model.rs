//! What a tracer observed, before any policy is applied.
//!
//! This is the raw vocabulary: one flat stream of observations in the order the
//! kernel reported them. Ordering is load-bearing — a file created by an
//! execution and then read back is an intermediate, not an input, and the only
//! way to tell is that the create came first. Normalisation into a dependency
//! set happens in [`crate::dependency`], never here.

use crate::paths::Scope;
use serde::{Deserialize, Serialize};

/// Bumped when the meaning of an observation changes. A dependency set recorded
/// under an older trace schema is not reinterpreted under a newer one.
///
/// v2 added directory enumeration, negative lookups, ordered events and
/// structured downgrade reasons.
pub const TRACE_SCHEMA_VERSION: u32 = 2;

/// Bumped when the *rules* a backend applies change, even if the data shape
/// does not: which syscalls count as a read, what makes a path volatile, how
/// `/proc` is treated. A dependency set learned under different semantics is
/// discarded rather than reinterpreted, because "complete" meant something else
/// when it was written.
///
/// v2: a `connect` to a Unix socket that is not there is a negative dependency
/// rather than network use, stable pseudo-files under `/proc` and `/sys` are
/// hashed rather than treated as volatile, and a null path argument is a
/// question about a descriptor.
pub const TRACE_SEMANTICS_VERSION: u32 = 2;

/// Operations a tracer may report against a path.
///
/// Backends produce only what their [`Capabilities`](super::Capabilities)
/// admit; the remaining variants stay empty rather than being approximated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileOp {
    /// Contents were, or could be, read: a readable open, a mapping, a
    /// successful `readlink`.
    Read,
    Write,
    Create,
    Delete,
    Rename,
    Execute,
    /// Metadata or existence was consulted and the path was there.
    Stat,
    /// Metadata or existence was consulted and the path was *not* there. The
    /// absence is the dependency.
    Absent,
    /// The directory's entries were enumerated. The dependency is the entry
    /// set, not any one file in it.
    ListDir,
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
            FileOp::Stat => "stat",
            FileOp::Absent => "absent",
            FileOp::ListDir => "listdir",
        }
    }

    /// Whether the operation, on its own, makes the path a candidate *input*.
    ///
    /// `Stat` counts: a program that branches on a file's existence or size
    /// depends on it as surely as one that reads its bytes.
    pub fn is_input(&self) -> bool {
        matches!(
            self,
            FileOp::Read | FileOp::Execute | FileOp::Stat | FileOp::ListDir
        )
    }

    /// Whether the operation establishes the path's content or existence as
    /// something this execution produced rather than consumed.
    pub fn is_producing(&self) -> bool {
        matches!(
            self,
            FileOp::Write | FileOp::Create | FileOp::Delete | FileOp::Rename
        )
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

/// Why a trace is not complete. Structured rather than free text so the engine
/// can gate on it, `arc doctor` can explain it, and a later version can act on
/// specific reasons without parsing prose.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Downgrade {
    /// The backend does not observe some dependency class at all.
    BackendPartial,
    /// A syscall the backend does not model was executed, and it could not be
    /// ruled out as irrelevant.
    UnsupportedSyscall(u64),
    /// A path argument could not be reconstructed from the tracee.
    PathResolutionFailure,
    /// A process was created that the backend could not follow.
    ChildEscape,
    /// The per-run event or path budget was exhausted.
    EventOverflow,
    /// The execution read a filesystem whose contents Arc cannot fingerprint
    /// meaningfully — `/proc`, `/sys`, a character device.
    VolatileRead(String),
    /// The execution talked to something outside itself.
    NetworkAccess,
    /// An observed dependency no longer exists and the execution was not seen
    /// deleting it, so its contents can no longer be fingerprinted.
    DependencyDisappeared(String),
    /// The tracer itself failed. The command's own result is unaffected.
    BackendError(String),
}

impl Downgrade {
    /// Short machine-readable kind, stable across versions, for JSON consumers
    /// and for grouping in the CLI.
    pub fn kind(&self) -> &'static str {
        match self {
            Downgrade::BackendPartial => "backend_partial",
            Downgrade::UnsupportedSyscall(_) => "unsupported_syscall",
            Downgrade::PathResolutionFailure => "path_resolution_failure",
            Downgrade::ChildEscape => "child_escape",
            Downgrade::EventOverflow => "event_overflow",
            Downgrade::VolatileRead(_) => "volatile_read",
            Downgrade::NetworkAccess => "network_access",
            Downgrade::DependencyDisappeared(_) => "dependency_disappeared",
            Downgrade::BackendError(_) => "backend_error",
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Downgrade::BackendPartial => "the backend cannot observe every dependency class".into(),
            Downgrade::UnsupportedSyscall(nr) => {
                format!("syscall {nr} is not modelled by this backend")
            }
            Downgrade::PathResolutionFailure => {
                "a path argument could not be read back from the process".into()
            }
            Downgrade::ChildEscape => "a process could not be followed".into(),
            Downgrade::EventOverflow => "the execution exceeded the trace budget".into(),
            Downgrade::VolatileRead(p) => format!("read volatile path {p}"),
            Downgrade::NetworkAccess => "the execution used the network".into(),
            Downgrade::DependencyDisappeared(p) => {
                format!("{p} was read and is now gone; it can no longer be fingerprinted")
            }
            Downgrade::BackendError(e) => format!("tracer error: {e}"),
        }
    }
}

/// Everything one backend saw during one execution.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Observations {
    /// In the order observed. Ordering is what distinguishes a generated
    /// intermediate from a genuine input, so it must not be sorted away.
    pub files: Vec<FileObservation>,
    pub processes: Vec<ProcessObservation>,
    /// Set when the backend knows it dropped events. Forces the resulting
    /// dependency set to be treated as lossy even within the backend's declared
    /// capabilities.
    pub lossy: bool,
    /// Everything preventing this trace from being `Complete`. Empty means the
    /// backend is claiming it saw all of it.
    pub downgrades: Vec<Downgrade>,
    /// Human-readable notes surfaced by `arc run --trace --verbose`.
    pub notes: Vec<String>,
}

impl Observations {
    pub fn merge(&mut self, other: Observations) {
        self.files.extend(other.files);
        self.processes.extend(other.processes);
        self.lossy |= other.lossy;
        self.downgrades.extend(other.downgrades);
        self.notes.extend(other.notes);
    }

    /// Record a downgrade, keeping the list bounded and free of duplicates. A
    /// pathological run can hit the same reason millions of times; the reason is
    /// what matters, not the count.
    pub fn downgrade(&mut self, reason: Downgrade) {
        const MAX: usize = 32;
        if self.downgrades.len() < MAX && !self.downgrades.contains(&reason) {
            self.downgrades.push(reason);
        }
    }
}
