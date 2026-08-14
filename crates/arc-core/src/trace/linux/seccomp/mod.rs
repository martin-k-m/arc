//! Complete dependency observation on Linux, via seccomp user notification.
//!
//! # Why this and not ptrace
//!
//! ptrace stops a tracee twice for every syscall it makes. A compiler issues
//! millions; a `futex` or a `read` costs two context switches even though Arc
//! has already proven it cannot carry a dependency. Seccomp user notification
//! moves that decision into the kernel: a cBPF program classifies each syscall
//! in nanoseconds and only the ones Arc must see reach userspace.
//!
//! # Why it is rootless
//!
//! Installing a filter that can return `SECCOMP_RET_USER_NOTIF` needs either
//! `CAP_SYS_ADMIN` **or** `no_new_privs`, and Arc takes the second. The
//! consequence is stated plainly rather than hidden: a traced process tree
//! cannot gain privilege through `setuid`, exactly as under ptrace.
//!
//! # Requirements, all detected rather than assumed
//!
//! * `SECCOMP_FILTER_FLAG_NEW_LISTENER` — Linux 5.0
//! * `SECCOMP_USER_NOTIF_FLAG_CONTINUE` — Linux 5.5, and without it a notifier
//!   can only fake return values, which is useless for observation
//! * a container policy that does not block `seccomp()` itself
//! * an architecture Arc has a syscall vocabulary for
//!
//! [`availability`] proves all of it by running a complete notification round
//! trip in a forked child, because a version check is a guess and a container
//! seccomp profile can refuse the call regardless of kernel version.

mod backend;
mod filter;
mod sys;

pub use backend::NAME;

use crate::paths::Classifier;
use crate::trace::Tracer;
use std::io;
use std::path::Path;
use std::sync::OnceLock;

pub fn availability() -> (bool, Option<String>) {
    static RESULT: OnceLock<Result<(), String>> = OnceLock::new();
    match RESULT.get_or_init(probe) {
        Ok(()) => (true, None),
        Err(e) => (false, Some(e.clone())),
    }
}

pub fn start(cwd: &Path, classifier: &Classifier) -> Option<Box<dyn Tracer>> {
    if !availability().0 {
        return None;
    }
    backend::SeccompTracer::new(cwd, classifier)
        .ok()
        .map(|t| Box::new(t) as Box<dyn Tracer>)
}

/// Prove the mechanism end to end: install a listener in a child, receive its
/// notification here, answer with `CONTINUE`, and confirm the child's syscall
/// then produced the real answer.
///
/// Anything less would be a guess. A kernel can advertise the flag and a
/// container profile still refuse the call; `CONTINUE` can be missing on 5.0
/// through 5.4; and a filter can install and still never deliver.
fn probe() -> Result<(), String> {
    if !sys::ARCH_SUPPORTED {
        return Err(format!(
            "no seccomp syscall vocabulary for {}",
            std::env::consts::ARCH
        ));
    }
    match sys::sizes_match() {
        Ok(true) => {}
        Ok(false) => return Err("this kernel's seccomp notification ABI differs".into()),
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {
            return Err("this kernel has no seccomp user notification".into())
        }
        Err(e) => return Err(format!("seccomp is unavailable: {e}")),
    }

    let (parent, child) = backend::socketpair().map_err(|e| e.to_string())?;
    let program = filter::probe_program(libc::SYS_getppid as u32);
    let prog = sys::sock_fprog {
        len: program.len() as u16,
        filter: program.as_ptr(),
    };
    let parent_fd = std::os::unix::io::AsRawFd::as_raw_fd(&parent);
    let child_fd = std::os::unix::io::AsRawFd::as_raw_fd(&child);

    // SAFETY: the child branch calls only `prctl`, `seccomp`, `sendmsg`,
    // `getppid`, `close` and `_exit`, all async-signal-safe, so it is legal
    // between `fork` and `_exit` in a process that has other threads. It never
    // returns, never allocates and never touches an inherited lock. `program`
    // was built before the fork and is only read.
    let pid = unsafe {
        let pid = libc::fork();
        if pid == 0 {
            let real_parent = libc::getppid();
            match sys::install_listener(&prog) {
                Ok(fd) => {
                    if sys::announce(child_fd, fd).is_err() {
                        libc::_exit(EXIT_NO_SEND);
                    }
                    libc::close(fd);
                }
                // A forked child shares no memory with its parent, so the exit
                // status is the only channel it has. Small errno values encode
                // cleanly and are all that matter here.
                Err(e) => libc::_exit(INSTALL_FAILED + e.raw_os_error().unwrap_or(0).clamp(0, 120)),
            }
            // Trapped by the filter. It only returns the right answer if the
            // notification was answered with CONTINUE.
            let got = libc::getppid();
            libc::_exit(if got == real_parent {
                0
            } else {
                EXIT_WRONG_RESULT
            });
        }
        pid
    };
    if pid < 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    drop(child);

    let outcome = probe_round_trip(parent_fd);
    // Closing this end before waiting is load-bearing. The child blocks in
    // `read`, waiting for the acknowledgement that Arc holds its listener; if
    // the round trip failed that acknowledgement is never coming, and the only
    // thing that unblocks the child is end-of-file. Waiting first would hang
    // here forever — which is exactly what a sandbox permitting `seccomp` but
    // denying `pidfd_getfd` produces.
    drop(parent);
    let mut status: libc::c_int = 0;
    // SAFETY: `status` is a live local and `pid` is this process's own child.
    unsafe { libc::waitpid(pid, &mut status, 0) };

    // The child's exit status is the specific answer; a failed round trip is
    // usually just the shape that failure took, so it is reported only when the
    // child itself had nothing to say.
    if !libc::WIFEXITED(status) {
        return Err("the seccomp probe child did not exit normally".into());
    }
    match libc::WEXITSTATUS(status) {
        0 => outcome,
        // The child reports this whenever the handover did not complete, which
        // includes the case where *Arc* was the side that failed. Arc's own
        // error is the specific one, so it wins where there is one.
        EXIT_NO_SEND => match outcome {
            Err(e) => Err(e),
            Ok(()) => Err("the probe child could not hand over its listener".into()),
        },
        EXIT_WRONG_RESULT => {
            Err("this kernel does not support SECCOMP_USER_NOTIF_FLAG_CONTINUE".into())
        }
        code if code >= INSTALL_FAILED => Err(denied_reason(code - INSTALL_FAILED)),
        other => Err(format!("the seccomp probe child exited with {other}")),
    }
}

fn probe_round_trip(parent: std::os::unix::io::RawFd) -> Result<(), String> {
    let listener = match sys::acquire(parent) {
        Ok(fd) => fd,
        Err(e) => return Err(e.to_string()),
    };
    // SAFETY: the descriptor came from `pidfd_getfd` and is owned here.
    let listener = unsafe { std::os::unix::io::OwnedFd::from_raw_fd(listener) };
    let fd = std::os::unix::io::AsRawFd::as_raw_fd(&listener);
    if !sys::wait_readable(fd, 5_000) {
        return Err("the kernel delivered no seccomp notification".into());
    }
    let n = sys::recv(fd).map_err(|e| format!("no notification arrived: {e}"))?;
    let resp = sys::seccomp_notif_resp {
        id: n.id,
        val: 0,
        error: 0,
        flags: sys::SECCOMP_USER_NOTIF_FLAG_CONTINUE,
    };
    sys::send(fd, &resp).map_err(|e| format!("the kernel refused CONTINUE: {e}"))
}

use std::os::unix::io::FromRawFd;

const EXIT_NO_SEND: i32 = 92;
const EXIT_WRONG_RESULT: i32 = 93;
/// `INSTALL_FAILED + errno`: the child's only way to report why.
const INSTALL_FAILED: i32 = 100;

/// Why installing a filter was refused, in terms a user can act on.
fn denied_reason(errno: i32) -> String {
    if std::fs::read_to_string("/proc/sys/kernel/seccomp/actions_avail")
        .map(|s| !s.contains("user_notif"))
        .unwrap_or(false)
    {
        return "this kernel does not offer seccomp user notification".into();
    }
    let e = io::Error::from_raw_os_error(errno);
    match errno {
        libc::EPERM | libc::EACCES => format!(
            "a seccomp listener was refused ({e}); a container seccomp profile is the usual cause"
        ),
        libc::EINVAL => "this kernel does not support seccomp user notification".into(),
        _ => format!("installing a seccomp listener failed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn availability_is_decided_once_and_gives_a_reason_when_it_says_no() {
        let (ok, reason) = super::availability();
        assert_eq!(ok, reason.is_none());
        assert_eq!(super::availability().0, ok, "the answer must be cached");
        if let Some(r) = reason {
            assert!(!r.is_empty());
        }
    }

    /// The probe blocks a child on a socket while it copies a descriptor out.
    /// A sandbox that permits `seccomp` but denies `pidfd_getfd` used to leave
    /// both sides waiting for each other, which turned `arc doctor` into a
    /// hang. Whatever the answer is, it has to arrive.
    #[test]
    fn deciding_availability_terminates() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(super::availability().0);
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(60)).is_ok(),
            "the seccomp probe did not finish"
        );
    }
}
