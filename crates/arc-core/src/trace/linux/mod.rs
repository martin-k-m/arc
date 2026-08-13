//! Complete dependency observation on Linux, via `ptrace`.
//!
//! # Why ptrace
//!
//! Four mechanisms can see a process read a file. Only one of them is available
//! to an ordinary developer tool:
//!
//! | Mechanism | Sees reads | Sees `stat` | Follows children | Needs privilege |
//! | --- | --- | --- | --- | --- |
//! | `ptrace` | yes | yes | yes | none |
//! | seccomp user notification | yes | yes | yes | `CAP_SYS_ADMIN` |
//! | `fanotify` | yes | no | n/a — system-wide | `CAP_SYS_ADMIN` |
//! | eBPF / kprobes | yes | yes | yes | `CAP_BPF`, kernel headers |
//! | `LD_PRELOAD` | partly | partly | partly | none |
//!
//! `fanotify` and eBPF need capabilities Arc must not ask for, and both observe
//! the whole machine rather than one process tree, which is a privacy problem as
//! much as a correctness one. Seccomp user notification is the natural
//! successor to ptrace and is far cheaper, but installing a listener has
//! required `CAP_SYS_ADMIN` since it was introduced. `LD_PRELOAD` misses static
//! binaries, misses anything that issues a raw syscall, and is defeated by
//! `setuid`; a tracer that silently misses Go binaries cannot claim
//! completeness.
//!
//! ptrace is the slow option and the honest one. It observes every syscall of
//! every descendant, needs nothing but being the parent, and works under
//! `kernel.yama.ptrace_scope = 1` because the child puts *itself* under
//! observation before `exec` rather than Arc attaching to a stranger.
//!
//! # What "complete" means here
//!
//! Every file the execution could read, every existence check, every directory
//! it enumerated, every binary it ran, and every descendant process. It does
//! **not** mean the execution is deterministic: the clock, the scheduler and
//! `getrandom` are outside any filesystem tracer. Use of the network, of
//! `/proc`, of `/sys`, of a syscall this backend does not model, or of anything
//! that overflowed the trace budget all revoke the claim — see
//! [`Downgrade`](crate::trace::Downgrade).

mod backend;
mod state;
mod sys;
mod syscalls;

pub use backend::{CAPABILITIES, NAME};

use crate::paths::Classifier;
use crate::trace::Tracer;
use std::path::Path;
use std::sync::OnceLock;

/// Whether ptrace tracing can run here, and why not when it cannot.
///
/// The answer is cached: it is a property of the kernel and the container, and
/// establishing it costs a `fork`.
pub fn availability() -> (bool, Option<String>) {
    static RESULT: OnceLock<Result<(), String>> = OnceLock::new();
    match RESULT.get_or_init(sys::probe) {
        Ok(()) => (true, None),
        Err(e) => (false, Some(e.clone())),
    }
}

/// # Safety
///
/// Between `fork` and `exec` in the child only. See [`sys::traceme`].
pub(crate) unsafe fn traceme() -> std::io::Result<()> {
    sys::traceme()
}

pub fn start(cwd: &Path, classifier: &Classifier) -> Option<Box<dyn Tracer>> {
    availability()
        .0
        .then(|| Box::new(backend::LinuxTracer::new(cwd, classifier)) as Box<dyn Tracer>)
}

/// What Arc does with a path, before anything else looks at it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// An ordinary file. Fingerprint it.
    Normal,
    /// Not a dependency at all, and not a reason to distrust the trace.
    Ignore,
    /// Real, consulted, and impossible to fingerprint meaningfully. Recording a
    /// digest of it would either be a lie or guarantee a miss on every run, so
    /// the trace stops claiming completeness instead.
    Volatile,
}

/// Policy for the synthetic filesystems, which behave nothing like files.
pub mod policy {
    use super::Verdict;

    /// Character devices whose contents are constant or are the terminal. A
    /// program writing to `/dev/null` has not acquired a dependency.
    ///
    /// `/dev/random` and `/dev/urandom` are deliberately absent: reading them
    /// makes a result irreproducible, which is exactly the case Arc must not
    /// quietly cache.
    const HARMLESS_DEV: &[&str] = &[
        "null", "zero", "full", "tty", "console", "ptmx", "stdin", "stdout", "stderr",
    ];

    pub fn verdict(path: &str) -> Verdict {
        if let Some(rest) = path.strip_prefix("/proc/") {
            // A process asking about itself is not consulting shared state: the
            // answer is a function of this execution, not of anything a
            // previous run could have left behind. Treating `/proc/self/maps`
            // as machine state would downgrade essentially every Rust or Go
            // program for no gain in safety.
            let first = rest.split('/').next().unwrap_or("");
            if first == "self" || first == "thread-self" || first.parse::<u32>().is_ok() {
                return Verdict::Ignore;
            }
            return Verdict::Volatile;
        }
        if path.starts_with("/sys/") {
            return Verdict::Volatile;
        }
        if let Some(rest) = path.strip_prefix("/dev/") {
            if rest.starts_with("fd/") || rest.starts_with("pts/") {
                return Verdict::Ignore;
            }
            return if HARMLESS_DEV.contains(&rest) {
                Verdict::Ignore
            } else {
                Verdict::Volatile
            };
        }
        Verdict::Normal
    }
}

#[cfg(test)]
mod tests {
    use super::policy::verdict;
    use super::Verdict;

    #[test]
    fn self_introspection_is_not_a_dependency_but_shared_procfs_is_volatile() {
        assert_eq!(verdict("/proc/self/maps"), Verdict::Ignore);
        assert_eq!(verdict("/proc/thread-self/stat"), Verdict::Ignore);
        assert_eq!(verdict("/proc/4711/cmdline"), Verdict::Ignore);
        assert_eq!(verdict("/proc/cpuinfo"), Verdict::Volatile);
        assert_eq!(verdict("/sys/fs/cgroup/cpu.max"), Verdict::Volatile);
    }

    #[test]
    fn randomness_is_volatile_and_the_null_device_is_not() {
        assert_eq!(verdict("/dev/urandom"), Verdict::Volatile);
        assert_eq!(verdict("/dev/random"), Verdict::Volatile);
        assert_eq!(verdict("/dev/null"), Verdict::Ignore);
        assert_eq!(verdict("/dev/pts/3"), Verdict::Ignore);
        assert_eq!(verdict("/dev/sda1"), Verdict::Volatile);
    }

    #[test]
    fn ordinary_paths_are_ordinary() {
        assert_eq!(verdict("/repo/src/lib.rs"), Verdict::Normal);
        assert_eq!(verdict("/usr/lib/libc.so.6"), Verdict::Normal);
        // A path that merely starts with the same letters as a synthetic mount
        // must not be swept up by a prefix test.
        assert_eq!(verdict("/procession/notes.txt"), Verdict::Normal);
    }
}
