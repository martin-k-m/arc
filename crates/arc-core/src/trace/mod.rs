//! Execution tracing.
//!
//! A tracer observes one execution and reports [`Observations`]. Backends are
//! selected per platform and declare exactly what they can see; nothing else in
//! Arc may assume more than a backend's [`Capabilities`] admit.
//!
//! Capability honesty is a correctness property, not documentation. Input
//! narrowing is gated on `file_reads`, and no backend shipping today sets it,
//! so v0.2 never narrows the conservative input set from a trace.

pub mod model;
mod snapshot;

#[cfg(windows)]
mod windows_job;

pub use model::{FileObservation, FileOp, Observations, ProcessObservation, TRACE_SCHEMA_VERSION};

use crate::paths::Classifier;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// What a backend can observe. Every field is a promise the backend must be
/// able to keep for every execution, not a best effort.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Observes files the execution read. Required for input narrowing.
    pub file_reads: bool,
    /// Observes files the execution created, modified or deleted.
    pub file_writes: bool,
    /// Observes directory enumeration. A program that lists `plugins/` depends
    /// on the set of entries, not only on files it opened.
    pub directory_reads: bool,
    /// Observes whether a path the execution consulted was absent.
    pub existence_checks: bool,
    /// Observes processes created by the command, recursively.
    pub process_tree: bool,
    /// Resolves the executable image of observed processes.
    pub executables: bool,
    /// Observes paths outside the project root.
    pub outside_project: bool,
}

impl Capabilities {
    pub const NONE: Capabilities = Capabilities {
        file_reads: false,
        file_writes: false,
        directory_reads: false,
        existence_checks: false,
        process_tree: false,
        executables: false,
        outside_project: false,
    };

    /// `(label, supported)` pairs for `arc doctor`.
    pub fn rows(&self) -> [(&'static str, bool); 7] {
        [
            ("file reads", self.file_reads),
            ("file writes", self.file_writes),
            ("directory reads", self.directory_reads),
            ("existence checks", self.existence_checks),
            ("process tree", self.process_tree),
            ("executables", self.executables),
            ("outside project", self.outside_project),
        ]
    }
}

/// Observes one execution. `start` is called immediately before the child is
/// spawned and `finish` immediately after it exits.
///
/// A tracer must never fail an execution: a backend that breaks reports the
/// failure through [`Observations::lossy`] and a note, and the run proceeds
/// with conservative fingerprinting.
pub trait Tracer {
    fn name(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;
    /// Called after the child is spawned, with its pid, so backends that follow
    /// a process tree can attach. Backends that do not need it ignore it.
    fn attach(&mut self, _pid: u32) -> Result<()> {
        Ok(())
    }
    fn finish(self: Box<Self>) -> Observations;
}

/// The backend for this platform, started and ready to attach.
///
/// Returns `None` when tracing is disabled or unavailable, in which case Arc
/// falls back to conservative project fingerprinting — the v0.1 behaviour.
pub fn start(root: &Path, classifier: &Classifier) -> Option<Box<dyn Tracer>> {
    let snap = snapshot::SnapshotTracer::start(root, classifier);
    #[cfg(windows)]
    {
        Some(Box::new(windows_job::JobTracer::start(snap, classifier)))
    }
    #[cfg(not(windows))]
    {
        Some(Box::new(snap))
    }
}

/// What the platform's backend can do, without starting it. Used by
/// `arc doctor` and by capability gating before a run.
pub fn platform_capabilities() -> Capabilities {
    let base = snapshot::CAPABILITIES;
    #[cfg(windows)]
    {
        windows_job::extend(base)
    }
    #[cfg(not(windows))]
    {
        base
    }
}

pub fn platform_backend_name() -> &'static str {
    #[cfg(windows)]
    {
        "snapshot+jobobject"
    }
    #[cfg(not(windows))]
    {
        "snapshot"
    }
}
