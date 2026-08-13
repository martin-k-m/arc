//! Portable write observer.
//!
//! Records `(size, mtime)` for every file under the project before and after the
//! execution and diffs the two. That yields creates, writes and deletes on every
//! platform with no privileges, no injection and no kernel interface.
//!
//! It cannot observe reads. There is no non-privileged, non-injecting way to
//! observe file reads on Windows — last-access times are disabled by default and
//! the kernel file provider requires administrator rights — so this backend
//! declares `file_reads: false` and Arc never narrows inputs from it.
//!
//! Two files written inside the same filesystem timestamp granularity as the
//! snapshot can be missed. Arc treats a missed *write* as a missed output, which
//! degrades to the v0.1 behaviour of that file remaining an ordinary input; it
//! can never turn into a cache hit that should have been a miss.

use super::model::{Downgrade, FileObservation, FileOp, Observations};
use super::{Capabilities, Tracer};
use crate::paths::{display_form, Classifier, Scope};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const CAPABILITIES: Capabilities = Capabilities {
    file_reads: false,
    file_writes: true,
    directory_reads: false,
    existence_checks: false,
    process_tree: false,
    executables: false,
    outside_project: false,
    network_detection: false,
};

/// Snapshotting a tree with more entries than this is not worth the wall-clock
/// cost of the thing it is meant to accelerate. Past the cap the backend
/// reports itself lossy rather than silently truncating.
const MAX_ENTRIES: usize = 200_000;

type Snap = HashMap<String, (u64, i64)>;

pub struct SnapshotTracer {
    root: PathBuf,
    before: Snap,
    truncated: bool,
    classifier: Classifier,
}

impl SnapshotTracer {
    pub fn start(root: &Path, classifier: &Classifier) -> SnapshotTracer {
        let (before, truncated) = snapshot(root, classifier);
        SnapshotTracer {
            root: root.to_path_buf(),
            before,
            truncated,
            classifier: classifier.clone(),
        }
    }
}

impl Tracer for SnapshotTracer {
    fn name(&self) -> &'static str {
        "snapshot"
    }

    fn capabilities(&self) -> Capabilities {
        CAPABILITIES
    }

    fn finish(self: Box<Self>) -> Observations {
        let (after, truncated) = snapshot(&self.root, &self.classifier);
        let mut files = Vec::new();
        for (rel, (size, mtime)) in &after {
            let op = match self.before.get(rel) {
                None => FileOp::Create,
                Some(prev) if prev != &(*size, *mtime) => FileOp::Write,
                Some(_) => continue,
            };
            files.push(observation(&self.root, rel, op, op == FileOp::Write));
        }
        for rel in self.before.keys() {
            if !after.contains_key(rel) {
                files.push(observation(&self.root, rel, FileOp::Delete, true));
            }
        }
        files.sort_by(|a, b| (&a.path, a.op).cmp(&(&b.path, b.op)));

        let lossy = self.truncated || truncated;
        let mut notes = Vec::new();
        // This backend never observes reads, so every trace it produces is
        // partial by construction. Saying so here rather than inferring it from
        // the capability flags keeps one code path for "why is this not
        // complete?".
        let mut downgrades = vec![Downgrade::BackendPartial];
        if lossy {
            notes.push(format!(
                "project has more than {MAX_ENTRIES} files; write observation is incomplete"
            ));
            downgrades.push(Downgrade::EventOverflow);
        }
        Observations {
            files,
            processes: Vec::new(),
            lossy,
            downgrades,
            notes,
        }
    }
}

fn observation(root: &Path, rel: &str, op: FileOp, existed_before: bool) -> FileObservation {
    FileObservation {
        path: display_form(&root.join(rel)),
        rel: Some(rel.to_string()),
        op,
        scope: Scope::Project,
        existed_before: Some(existed_before),
    }
}

/// Metadata-only walk of the project. `.gitignore` is deliberately *not*
/// honoured: generated files are usually ignored, and those are precisely the
/// writes worth seeing.
fn snapshot(root: &Path, classifier: &Classifier) -> (Snap, bool) {
    let classifier = classifier.clone();
    let root = root.to_path_buf();
    let shared = Arc::new(Mutex::new((Snap::with_capacity(4096), false)));
    ignore::WalkBuilder::new(&root)
        .hidden(false)
        .parents(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .follow_links(false)
        .threads(std::thread::available_parallelism().map_or(4, |n| n.get().min(8)))
        .filter_entry(move |e| {
            e.file_name() != ".git" && classifier.classify(e.path()) != Scope::ArcInternal
        })
        .build_parallel()
        .run(|| {
            let mut batch = Batch {
                shared: shared.clone(),
                local: Vec::with_capacity(BATCH),
            };
            let root = root.clone();
            Box::new(move |entry| {
                if let Ok(entry) = entry {
                    if entry.file_type().is_some_and(|t| t.is_file()) {
                        if let (Ok(rel), Ok(md)) =
                            (entry.path().strip_prefix(&root), entry.metadata())
                        {
                            batch.local.push((
                                rel.to_string_lossy().replace('\\', "/"),
                                (md.len(), mtime_nanos(&md)),
                            ));
                        }
                    }
                }
                if batch.local.len() >= BATCH && batch.flush() >= MAX_ENTRIES {
                    return ignore::WalkState::Quit;
                }
                ignore::WalkState::Continue
            })
        });

    let mut guard = shared.lock().unwrap();
    let (map, truncated) = &mut *guard;
    if map.len() >= MAX_ENTRIES {
        *truncated = true;
    }
    (std::mem::take(map), *truncated)
}

const BATCH: usize = 512;

/// Accumulates one worker's entries so the shared map is locked once per batch
/// rather than once per file. The `Drop` impl is load-bearing: without it every
/// worker's final partial batch would be silently lost, and a lost entry looks
/// exactly like a deleted file.
struct Batch {
    shared: Arc<Mutex<(Snap, bool)>>,
    local: Vec<(String, (u64, i64))>,
}

impl Batch {
    fn flush(&mut self) -> usize {
        let mut guard = self.shared.lock().unwrap();
        guard.0.extend(self.local.drain(..));
        guard.0.len()
    }
}

impl Drop for Batch {
    fn drop(&mut self) {
        self.flush();
    }
}

/// Nanosecond resolution where the platform provides it, so a rewrite within
/// the same millisecond is still seen as a write.
fn mtime_nanos(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}
