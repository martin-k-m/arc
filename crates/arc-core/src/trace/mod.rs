//! Execution tracing.
//!
//! A tracer observes one execution and reports [`Observations`]. Backends are
//! selected per platform and declare exactly what they can see; nothing else in
//! Arc may assume more than a backend's [`Capabilities`] admit.
//!
//! Capability honesty is a correctness property, not documentation. Automatic
//! input narrowing is gated on a backend observing *every* dependency class
//! plus a run that reported no downgrade — see [`crate::dependency::can_narrow`],
//! which is the only place that decision is made.
//!
//! | Backend | Platform | Reads | Dirs | Absences | Tree | Narrows |
//! | --- | --- | --- | --- | --- | --- | --- |
//! | `linux-ptrace` | Linux x86-64 / aarch64 | yes | yes | yes | yes | yes |
//! | `snapshot+jobobject` | Windows | no | no | no | yes | no |
//! | `snapshot` | anywhere | no | no | no | no | no |

pub mod model;
mod snapshot;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows_job;

pub use model::{
    Downgrade, FileObservation, FileOp, Observations, ProcessObservation, TRACE_SCHEMA_VERSION,
    TRACE_SEMANTICS_VERSION,
};

use crate::paths::Classifier;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// What a backend can observe. Every field is a promise the backend must be
/// able to keep for every execution, not a best effort.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Observes files whose contents the execution could read. Required for
    /// input narrowing.
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
    /// Detects that the execution used the network, so a result that may depend
    /// on a remote service is not silently treated as reproducible.
    pub network_detection: bool,
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
        network_detection: false,
    };

    /// Every class a complete dependency model requires. A backend missing any
    /// of these can strengthen a cache key but can never narrow one.
    pub fn observes_everything(&self) -> bool {
        self.file_reads
            && self.file_writes
            && self.directory_reads
            && self.existence_checks
            && self.process_tree
            && self.executables
            && self.outside_project
            && self.network_detection
    }

    /// `(label, supported)` pairs for `arc doctor`.
    pub fn rows(&self) -> [(&'static str, bool); 8] {
        [
            ("file reads", self.file_reads),
            ("file writes", self.file_writes),
            ("directory reads", self.directory_reads),
            ("existence checks", self.existence_checks),
            ("process tree", self.process_tree),
            ("executables", self.executables),
            ("outside project", self.outside_project),
            ("network detection", self.network_detection),
        ]
    }
}

/// How a child must be started for a backend to observe it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Launch {
    /// Spawn normally; the backend attaches afterwards or diffs the filesystem.
    Normal,
    /// The child must put itself under the tracer before `exec`, and the
    /// backend reaps it instead of the standard library.
    Traced,
}

/// Observes one execution.
///
/// A tracer must never fail an execution. A backend that breaks reports the
/// failure through [`Observations::lossy`] and a [`Downgrade`], and the run
/// proceeds with conservative fingerprinting.
pub trait Tracer {
    fn name(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;

    /// How `crate::exec` must start the child for this backend.
    fn launch(&self) -> Launch {
        Launch::Normal
    }

    /// Called after the child is spawned, with its pid, so backends that follow
    /// a process tree can attach. Backends that do not need it ignore it.
    fn attach(&mut self, _pid: u32) -> Result<()> {
        Ok(())
    }

    /// Drive the execution to completion, for backends that must own the wait
    /// loop. `None` means the caller reaps the child as usual.
    ///
    /// Returning an error here is a tracer failure, not a command failure: the
    /// caller falls back to reaping normally.
    fn supervise(&mut self, _pid: u32) -> Option<Result<crate::exec::Wait>> {
        None
    }

    fn finish(self: Box<Self>) -> Observations;
}

/// Which backend to use. `Auto` picks the strongest available one; the rest
/// exist so a user, a test, or `arc doctor` can pin behaviour and see the
/// fallback path work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Selection {
    #[default]
    Auto,
    Snapshot,
    Off,
}

impl Selection {
    pub fn parse(s: &str) -> Option<Selection> {
        match s {
            "auto" => Some(Selection::Auto),
            "snapshot" => Some(Selection::Snapshot),
            "off" | "none" => Some(Selection::Off),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Selection::Auto => "auto",
            Selection::Snapshot => "snapshot",
            Selection::Off => "off",
        }
    }

    /// `ARC_TRACE_BACKEND` overrides the default, so a container or CI job can
    /// pin the conservative backend without editing `arc.toml`.
    pub fn from_env() -> Selection {
        std::env::var("ARC_TRACE_BACKEND")
            .ok()
            .and_then(|v| Selection::parse(&v))
            .unwrap_or_default()
    }
}

/// What the platform's strongest backend is and whether it can actually run
/// here. Computed without starting anything, so `arc doctor` and the run
/// pipeline agree.
#[derive(Debug, Clone)]
pub struct Probe {
    pub name: &'static str,
    pub capabilities: Capabilities,
    pub available: bool,
    /// Why the strongest backend is unavailable, in a form a user can act on.
    pub reason: Option<String>,
    /// The backend that will be used instead when `available` is false.
    pub fallback: &'static str,
}

pub fn probe() -> Probe {
    #[cfg(target_os = "linux")]
    {
        let (available, reason) = linux::availability();
        Probe {
            name: linux::NAME,
            capabilities: if available {
                linux::CAPABILITIES
            } else {
                snapshot::CAPABILITIES
            },
            available,
            reason,
            fallback: "snapshot",
        }
    }
    #[cfg(windows)]
    {
        Probe {
            name: "snapshot+jobobject",
            capabilities: windows_job::extend(snapshot::CAPABILITIES),
            available: true,
            reason: None,
            fallback: "snapshot",
        }
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        Probe {
            name: "snapshot",
            capabilities: snapshot::CAPABILITIES,
            available: true,
            reason: None,
            fallback: "snapshot",
        }
    }
}

/// The backend for this platform, started and ready to attach.
///
/// Returns `None` when tracing is switched off, in which case Arc falls back to
/// conservative project fingerprinting — the v0.1 behaviour.
pub fn start(root: &Path, classifier: &Classifier, sel: Selection) -> Option<Box<dyn Tracer>> {
    if sel == Selection::Off {
        return None;
    }
    #[cfg(target_os = "linux")]
    if sel == Selection::Auto {
        if let Some(t) = linux::start(root, classifier) {
            return Some(t);
        }
    }
    let snap = snapshot::SnapshotTracer::start(root, classifier);
    #[cfg(windows)]
    {
        let _ = sel;
        Some(Box::new(windows_job::JobTracer::start(snap, classifier)))
    }
    #[cfg(not(windows))]
    {
        Some(Box::new(snap))
    }
}

/// What the platform's backend can do, without starting it. Used by `arc doctor`
/// and by capability gating before a run.
pub fn platform_capabilities() -> Capabilities {
    probe().capabilities
}

/// Place the calling process under its parent's control, immediately before
/// `exec`. Called from `crate::exec`'s `pre_exec` hook and nowhere else.
///
/// # Safety
///
/// Must only be called between `fork` and `exec` in the child. The one syscall
/// it makes is async-signal-safe.
#[cfg(target_os = "linux")]
pub(crate) unsafe fn traceme() -> std::io::Result<()> {
    linux::traceme()
}

pub fn platform_backend_name() -> &'static str {
    let p = probe();
    if p.available {
        p.name
    } else {
        p.fallback
    }
}
