//! The one place a Linux backend turns a path into an observation.
//!
//! Both Linux backends funnel through here, which is what makes "a complete
//! trace is a complete trace" true rather than aspirational: scope
//! classification, `/proc` and `/dev` policy, deduplication, budgets and
//! process bookkeeping are decided once. A backend supplies *what happened*;
//! this decides *what it means*.

use super::{policy, Verdict};
use crate::paths::{display_form, Classifier, Scope};
use crate::trace::model::{
    Downgrade, FileObservation, FileOp, Observations, ProcessObservation, TRACE_SEMANTICS_VERSION,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Distinct `(operation, path)` pairs one execution may produce. A pathological
/// program can issue tens of millions of syscalls; past this point Arc stops
/// recording, reports an overflow, and refuses to call the trace complete
/// rather than growing without bound.
pub const MAX_OBSERVATIONS: usize = 250_000;

/// Processes one execution may spawn before Arc stops tracking their images.
pub const MAX_PROCESSES: usize = 20_000;

pub struct Recorder {
    classifier: Classifier,
    pub cwd: PathBuf,
    pub obs: Observations,
    /// First-seen deduplication. Order is preserved in `obs.files`, because it
    /// is what separates a generated intermediate from a genuine input. `Arc`
    /// rather than `Rc` because the seccomp backend records from its notifier
    /// thread.
    seen: HashSet<(FileOp, Arc<str>)>,
    processes: HashMap<i32, Option<String>>,
    pub root: i32,
    /// Set by the last path-argument read when the argument was the empty
    /// string rather than something unreadable. It lets the caller tell
    /// `fstat(fd)` — which arrives as an empty path plus `AT_EMPTY_PATH` — from
    /// a path Arc genuinely failed to resolve, without reading the tracee's
    /// memory twice.
    pub empty_path_arg: bool,
    /// The syscall currently being decoded. Only used to say *which* call could
    /// not have its path read back, which is otherwise unanswerable from the
    /// outside: `path_resolution_failure` names no path by construction.
    pub syscall: i64,
    reported: usize,
    randomness: bool,
}

/// Unresolved-path notes kept per run. The downgrade already says the trace is
/// partial; these say what to go and look at.
const MAX_UNRESOLVED_NOTES: usize = 3;

impl Recorder {
    pub fn new(cwd: &Path, classifier: &Classifier) -> Recorder {
        Recorder {
            classifier: classifier.clone(),
            cwd: cwd.to_path_buf(),
            obs: Observations::default(),
            seen: HashSet::new(),
            processes: HashMap::new(),
            root: 0,
            empty_path_arg: false,
            syscall: -1,
            reported: 0,
            randomness: false,
        }
    }

    /// `getrandom` is reported, once, and does not downgrade. Measured reason:
    /// glibc calls it during start-up, so every command including `sh -c cat`
    /// would lose completeness and nothing would ever narrow. See
    /// LIMITATIONS.md.
    pub fn note_randomness(&mut self) {
        if !self.randomness {
            self.randomness = true;
            self.obs
                .notes
                .push("the execution took randomness from getrandom".into());
        }
    }

    /// A path argument that could not be turned into a name.
    pub fn unresolved(&mut self, why: &str) {
        if self.reported < MAX_UNRESOLVED_NOTES {
            self.reported += 1;
            self.obs.notes.push(format!(
                "unresolved path: {why} on syscall {}",
                self.syscall
            ));
        }
        self.obs.downgrade(Downgrade::PathResolutionFailure);
    }

    /// Record one observation, applying scope and volatility policy.
    ///
    /// Everything Arc writes itself is dropped here, before it can reach a
    /// dependency set: the cache database sits inside `$ARC_HOME`, which is
    /// frequently inside the project during testing, and an execution that
    /// learned Arc's own files as inputs would invalidate itself on every run.
    pub fn record(&mut self, path: &Path, op: FileOp) {
        let scope = self.classifier.classify(path);
        if scope == Scope::ArcInternal {
            return;
        }
        let display = display_form(path);
        match policy::verdict(&display) {
            // Process-private introspection: its contents are a function of this
            // execution, not of any prior state, so it is not a dependency.
            Verdict::Ignore => return,
            Verdict::Volatile => {
                if op.is_input() {
                    self.obs.downgrade(Downgrade::VolatileRead(display));
                }
                return;
            }
            Verdict::Normal => {}
        }

        if self.obs.files.len() >= MAX_OBSERVATIONS {
            self.obs.lossy = true;
            self.obs.downgrade(Downgrade::EventOverflow);
            return;
        }
        let key: Arc<str> = Arc::from(display.as_str());
        if !self.seen.insert((op, key)) {
            return;
        }
        self.obs.files.push(FileObservation {
            rel: self.classifier.relative(path),
            path: display,
            op,
            scope,
            existed_before: None,
        });
    }

    pub fn note_process(&mut self, pid: i32, image: Option<String>) {
        if self.processes.len() >= MAX_PROCESSES {
            self.obs.lossy = true;
            self.obs.downgrade(Downgrade::EventOverflow);
            return;
        }
        self.processes.insert(pid, image);
    }

    /// Attach an image to a process, whether or not it is already known, and
    /// without resurrecting one the budget dropped or erasing one already
    /// learned.
    pub fn set_image(&mut self, pid: i32, image: Option<String>) {
        match self.processes.get_mut(&pid) {
            Some(slot) => {
                if image.is_some() {
                    *slot = image;
                }
            }
            None => self.note_process(pid, image),
        }
    }

    pub fn at_process_limit(&self) -> bool {
        self.processes.len() >= MAX_PROCESSES
    }

    pub fn knows(&self, pid: i32) -> bool {
        self.processes.contains_key(&pid)
    }

    pub fn fail(&mut self, what: &str, e: &dyn std::fmt::Display) {
        self.obs.lossy = true;
        self.obs
            .downgrade(Downgrade::BackendError(format!("{what}: {e}")));
    }

    pub fn finish(self, backend: &str) -> Observations {
        let mut obs = self.obs;
        for (pid, image) in self.processes {
            let scope = image
                .as_ref()
                .map(|p| self.classifier.classify(Path::new(p)));
            obs.processes.push(ProcessObservation {
                pid: pid as u32,
                image,
                scope,
                descendant: pid != self.root,
            });
        }
        obs.processes.sort_by_key(|p| p.pid);
        obs.notes
            .push(format!("{backend} semantics v{TRACE_SEMANTICS_VERSION}"));
        obs
    }
}
