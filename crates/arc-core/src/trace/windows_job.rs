//! Windows process-tree observation via a job object.
//!
//! The child is assigned to an anonymous job object with an associated I/O
//! completion port. Windows then posts `JOB_OBJECT_MSG_NEW_PROCESS` for every
//! descendant, however deeply nested, which is how Arc sees that `cargo test`
//! is really `cargo` plus `rustc` plus a linker plus the test binaries.
//!
//! This needs no privileges and no injection. Two limits are real and are
//! reported rather than hidden:
//!
//! * A descendant created between `spawn` and `AssignProcessToJobObject` is
//!   outside the job. The window is sub-millisecond but not zero.
//! * A process that exits before its image path is read is recorded with an
//!   unknown image.
//!
//! Either case sets [`Observations::lossy`]. Both are safe directions to be
//! wrong in: process observations only ever *add* executables to a cache key,
//! and a key that covers less is exactly the v0.1 baseline, never a false hit.

use super::model::{Observations, ProcessObservation};
use super::snapshot::SnapshotTracer;
use super::{Capabilities, Tracer};
use crate::paths::{display_form, Classifier};
use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectAssociateCompletionPortInformation,
    SetInformationJobObject, JOBOBJECT_ASSOCIATE_COMPLETION_PORT,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA,
    PROCESS_TERMINATE,
};
use windows_sys::Win32::System::IO::{
    CreateIoCompletionPort, GetQueuedCompletionStatus, PostQueuedCompletionStatus,
};

pub fn extend(base: Capabilities) -> Capabilities {
    Capabilities {
        process_tree: true,
        executables: true,
        ..base
    }
}

/// Job completion message codes from `winnt.h`. They are part of the stable
/// Win32 ABI but are not surfaced by `windows-sys`, so they are spelled out
/// here rather than pulling in a second bindings crate for two integers.
const JOB_OBJECT_MSG_NEW_PROCESS: u32 = 6;
const JOB_OBJECT_MSG_EXIT_PROCESS: u32 = 7;

/// Completion key used for the shutdown packet, distinct from the job's key.
const STOP_KEY: usize = 1;
const JOB_KEY: usize = 0;

/// `HANDLE` is a raw pointer and therefore not `Send`. The handles here are
/// kernel objects with no thread affinity, so moving one to the collector
/// thread is sound; this wrapper carries that promise explicitly rather than
/// scattering `unsafe impl Send` over the module.
#[derive(Clone, Copy)]
struct SendHandle(HANDLE);
// SAFETY: Win32 kernel handles (job objects, completion ports) may be used from
// any thread in the process. The only requirement is that the handle outlives
// its use, which the join before `CloseHandle` in `finish` guarantees.
unsafe impl Send for SendHandle {}

pub struct JobTracer {
    /// `Option` only so `finish` can take it while `JobTracer` still has a
    /// `Drop` impl, which otherwise forbids moving a field out.
    inner: Option<Box<SnapshotTracer>>,
    classifier: Classifier,
    state: Option<Running>,
    notes: Vec<String>,
    lossy: bool,
}

struct Running {
    job: SendHandle,
    port: SendHandle,
    collector: std::thread::JoinHandle<()>,
    seen: Arc<Mutex<HashMap<u32, Option<String>>>>,
    unresolved: Arc<Mutex<usize>>,
}

impl JobTracer {
    pub fn start(inner: SnapshotTracer, classifier: &Classifier) -> JobTracer {
        let mut t = JobTracer {
            inner: Some(Box::new(inner)),
            classifier: classifier.clone(),
            state: None,
            notes: Vec::new(),
            lossy: false,
        };
        match create_job() {
            Ok((job, port)) => {
                let seen: Arc<Mutex<HashMap<u32, Option<String>>>> = Arc::default();
                let unresolved: Arc<Mutex<usize>> = Arc::default();
                let (ready_tx, ready_rx) = mpsc::channel();
                let collector = std::thread::spawn({
                    let seen = seen.clone();
                    let unresolved = unresolved.clone();
                    move || {
                        let _ = ready_tx.send(());
                        collect(port, &seen, &unresolved)
                    }
                });
                // Do not spawn the child until the collector is actually
                // waiting, or the first messages race the thread's startup.
                let _ = ready_rx.recv_timeout(Duration::from_secs(1));
                t.state = Some(Running {
                    job,
                    port,
                    collector,
                    seen,
                    unresolved,
                });
            }
            Err(e) => {
                t.lossy = true;
                t.notes
                    .push(format!("process-tree observation unavailable: {e}"));
            }
        }
        t
    }
}

impl Tracer for JobTracer {
    fn name(&self) -> &'static str {
        "snapshot+jobobject"
    }

    fn capabilities(&self) -> Capabilities {
        let base = super::snapshot::CAPABILITIES;
        if self.state.is_some() {
            extend(base)
        } else {
            base
        }
    }

    fn attach(&mut self, pid: u32) -> Result<()> {
        let Some(state) = &self.state else {
            return Ok(());
        };
        // SAFETY: `pid` names a process this call owns a reference to for the
        // duration of the block; both handles are checked before use and the
        // process handle is closed on every path.
        let assigned = unsafe {
            let proc = OpenProcess(
                PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                pid,
            );
            if proc.is_null() {
                false
            } else {
                let ok = AssignProcessToJobObject(state.job.0, proc) != 0;
                CloseHandle(proc);
                ok
            }
        };
        if assigned {
            state.seen.lock().unwrap().insert(pid, image_of(pid));
        } else {
            self.lossy = true;
            self.notes
                .push("child could not be assigned to a job object".into());
        }
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Observations {
        let mut obs = match self.inner.take() {
            Some(inner) => inner.finish(),
            None => Observations::default(),
        };
        let root_pid = self
            .state
            .as_ref()
            .and_then(|s| s.seen.lock().unwrap().keys().min().copied());

        if let Some(state) = self.state.take() {
            // SAFETY: the port is open until after the join below.
            unsafe {
                PostQueuedCompletionStatus(state.port.0, 0, STOP_KEY, std::ptr::null_mut());
            }
            let _ = state.collector.join();
            let unresolved = *state.unresolved.lock().unwrap();
            let seen = std::mem::take(&mut *state.seen.lock().unwrap());
            for (pid, image) in seen {
                let path = image.map(|p| display_form(Path::new(&p)));
                obs.processes.push(ProcessObservation {
                    pid,
                    scope: path
                        .as_ref()
                        .map(|p| self.classifier.classify(Path::new(p))),
                    image: path,
                    descendant: Some(pid) != root_pid,
                });
            }
            obs.processes.sort_by_key(|p| p.pid);
            if unresolved > 0 {
                self.lossy = true;
                self.notes.push(format!(
                    "{unresolved} process(es) exited before their executable could be identified"
                ));
            }
            // SAFETY: the collector has been joined, so no thread can touch
            // either handle after this point.
            unsafe {
                CloseHandle(state.port.0);
                CloseHandle(state.job.0);
            }
        }

        obs.lossy |= self.lossy;
        obs.notes.extend(std::mem::take(&mut self.notes));
        obs
    }
}

fn create_job() -> Result<(SendHandle, SendHandle)> {
    // SAFETY: both creation calls take well-formed arguments and their results
    // are checked before any further use.
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        anyhow::ensure!(!job.is_null(), "CreateJobObject failed");
        let port = CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1);
        if port.is_null() {
            CloseHandle(job);
            anyhow::bail!("CreateIoCompletionPort failed");
        }
        let assoc = JOBOBJECT_ASSOCIATE_COMPLETION_PORT {
            CompletionKey: JOB_KEY as *mut _,
            CompletionPort: port,
        };
        let ok = SetInformationJobObject(
            job,
            JobObjectAssociateCompletionPortInformation,
            &assoc as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_ASSOCIATE_COMPLETION_PORT>() as u32,
        );
        if ok == 0 {
            CloseHandle(port);
            CloseHandle(job);
            anyhow::bail!("SetInformationJobObject failed");
        }
        Ok((SendHandle(job), SendHandle(port)))
    }
}

/// Drain job notifications until the shutdown packet arrives.
fn collect(
    port: SendHandle,
    seen: &Mutex<HashMap<u32, Option<String>>>,
    unresolved: &Mutex<usize>,
) {
    loop {
        let mut bytes = 0u32;
        let mut key = 0usize;
        let mut overlapped = std::ptr::null_mut();
        // SAFETY: all three out-parameters are valid for the duration of the
        // call; the port handle is owned by the caller and outlives this thread.
        let ok = unsafe {
            GetQueuedCompletionStatus(port.0, &mut bytes, &mut key, &mut overlapped, 250) != 0
        };
        if !ok && overlapped.is_null() {
            // Timeout with nothing dequeued: keep waiting for the stop packet.
            continue;
        }
        if key == STOP_KEY {
            return;
        }
        // For job notifications the "overlapped" slot carries the process id.
        let pid = overlapped as usize as u32;
        match bytes {
            JOB_OBJECT_MSG_NEW_PROCESS => {
                let image = image_of(pid);
                if image.is_none() {
                    *unresolved.lock().unwrap() += 1;
                }
                seen.lock().unwrap().entry(pid).or_insert(image);
            }
            JOB_OBJECT_MSG_EXIT_PROCESS => {
                seen.lock().unwrap().entry(pid).or_insert(None);
            }
            _ => {}
        }
    }
}

/// Resolve a live process's executable image. Returns `None` once the process
/// has exited, which the caller counts as a lossy observation.
fn image_of(pid: u32) -> Option<String> {
    // SAFETY: the handle is closed on every path, and `QueryFullProcessImageNameW`
    // is given a buffer of exactly the length it is told about.
    unsafe {
        let proc = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if proc.is_null() {
            return None;
        }
        let mut buf = [0u16; 32_768];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(proc, 0, buf.as_mut_ptr(), &mut len);
        CloseHandle(proc);
        (ok != 0).then(|| String::from_utf16_lossy(&buf[..len as usize]))
    }
}

/// The stop packet must be delivered even if `finish` is never reached, or the
/// collector thread would outlive the run.
impl Drop for JobTracer {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            // SAFETY: same ownership argument as `finish`.
            unsafe {
                PostQueuedCompletionStatus(state.port.0, 0, STOP_KEY, std::ptr::null_mut());
            }
            let _ = state.collector.join();
            // SAFETY: collector joined; no other thread holds these handles.
            unsafe {
                CloseHandle(state.port.0);
                CloseHandle(state.job.0);
            }
        }
    }
}
