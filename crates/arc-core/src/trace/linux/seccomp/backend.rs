//! The seccomp user-notification event loop.
//!
//! # How this differs from ptrace, and why it is still complete
//!
//! ptrace stops the tracee twice for *every* syscall and reads the result at
//! the exit stop. This backend is notified only for syscalls the filter selects
//! — everything the syscall table proves irrelevant never leaves the kernel —
//! and the notification arrives *before* the syscall runs, with no exit hook.
//!
//! Two consequences, and how each is handled:
//!
//! * **No return value.** Whether a descriptor was produced, and whether a
//!   lookup found anything, is not observable. Both are recovered from the
//!   kernel's own state instead: `/proc/<pid>/fd/<n>` and `/proc/<pid>/cwd`
//!   resolve descriptors exactly, and existence is established by asking the
//!   filesystem at notification time, before the syscall that might change it.
//!   The remaining race — another process creating or removing that exact path
//!   in the microsecond before the syscall runs — is safe in both directions: a
//!   path recorded as present that is gone at fingerprint time downgrades the
//!   trace, and a path recorded as absent that exists produces a miss.
//!
//! * **No event queue.** Notification is synchronous request/response; a
//!   tracee blocks until Arc answers. There is no ring buffer and therefore no
//!   drop counter to check — events cannot be lost, only refused. If this
//!   backend stops answering, the tracee stalls rather than escaping
//!   observation, so the loop answers every notification whatever happens to
//!   the decoding of it.
//!
//! Descendants cannot escape: a seccomp filter is inherited across `fork` and
//! survives `execve`, and no process can remove one.

use super::filter;
use super::sys::{self, seccomp_notif, seccomp_notif_resp};
use crate::paths::display_form;
use crate::trace::linux::recorder::Recorder;
use crate::trace::linux::syscalls::{self, Dir, Sc};
use crate::trace::model::{Downgrade, FileOp, Observations};
use crate::trace::{Capabilities, PreExec, Tracer};
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub const NAME: &str = "linux-seccomp";

pub const CAPABILITIES: Capabilities = Capabilities {
    file_reads: true,
    file_writes: true,
    directory_reads: true,
    existence_checks: true,
    process_tree: true,
    executables: true,
    outside_project: true,
    network_detection: true,
};

/// Longest path argument Arc will read out of a tracee. `PATH_MAX` is the
/// kernel's own limit for a path the syscall could act on.
pub struct SeccompTracer {
    shared: Arc<Shared>,
    /// The child's end of the descriptor-passing socket. The `pre_exec` closure
    /// captures its number and the fork inherits it; this copy is closed as
    /// soon as the child exists, so a child that dies without sending a
    /// listener closes the socket rather than leaving Arc waiting forever.
    child_sock: Option<OwnedFd>,
    parent_sock: OwnedFd,
    program: Arc<Vec<sys::sock_filter>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

struct Shared {
    rec: Mutex<Option<Recorder>>,
    stop: AtomicBool,
    events: AtomicU64,
    /// A process was still holding the filter when the command returned.
    survivors: AtomicBool,
}

/// How long a still-live filter holder is given to exit after the command
/// returns, before the trace stops claiming to have seen everything. Long
/// enough for a child mid-exit, far short of a background worker's lifetime.
const SURVIVOR_GRACE_MS: i32 = 200;

/// The recorder, or a scratch one if the worker somehow lost it. Never panics
/// and never blocks a trace on a poisoned lock: a broken tracer must not break
/// the user's command.
fn lock(shared: &Shared) -> std::sync::MutexGuard<'_, Option<Recorder>> {
    shared.rec.lock().unwrap_or_else(|p| p.into_inner())
}

impl SeccompTracer {
    /// The notifier thread starts here, before the child exists.
    ///
    /// It has to. The child installs its filter and then calls `execve`, which
    /// is itself a filtered syscall, so it blocks before `Command::spawn`
    /// returns — and `spawn` does not return until the child has exec'd.
    /// Starting the thread after the spawn would deadlock the two against each
    /// other on every single run.
    pub fn new(cwd: &Path, classifier: &crate::paths::Classifier) -> io::Result<SeccompTracer> {
        let (parent, child) = socketpair()?;
        let shared = Arc::new(Shared {
            rec: Mutex::new(Some(Recorder::new(cwd, classifier))),
            stop: AtomicBool::new(false),
            events: AtomicU64::new(0),
            survivors: AtomicBool::new(false),
        });
        let worker = {
            let shared = shared.clone();
            let cwd = cwd.to_path_buf();
            let sock = parent.try_clone()?;
            std::thread::spawn(move || {
                let listener = match sys::acquire(sock.as_raw_fd()) {
                    // SAFETY: the descriptor came from `pidfd_getfd` and is
                    // owned by this thread from here on.
                    Ok(fd) => unsafe { OwnedFd::from_raw_fd(fd) },
                    Err(e) => {
                        if let Some(rec) = lock(&shared).as_mut() {
                            rec.fail("the child could not install a seccomp listener", &e);
                        }
                        return;
                    }
                };
                Session::new(listener, cwd).run(&shared);
            })
        };
        Ok(SeccompTracer {
            shared,
            child_sock: Some(child),
            parent_sock: parent,
            program: Arc::new(filter::program()),
            worker: Some(worker),
        })
    }
}

impl Tracer for SeccompTracer {
    fn name(&self) -> &'static str {
        NAME
    }

    fn capabilities(&self) -> Capabilities {
        CAPABILITIES
    }

    /// Installed by the child itself, between `fork` and `exec`.
    ///
    /// It must be the child: a seccomp filter applies to the process that
    /// installs it and to its descendants, and Arc must not put itself under
    /// one. Doing it before `exec` is also what makes the very first syscalls of
    /// the new image visible.
    fn pre_exec(&self) -> Option<PreExec> {
        let program = self.program.clone();
        let sock = self
            .child_sock
            .as_ref()
            .map(|f| f.as_raw_fd())
            .unwrap_or(-1);
        Some(Box::new(move || {
            let prog = sys::sock_fprog {
                len: program.len() as u16,
                filter: program.as_ptr(),
            };
            // SAFETY: between `fork` and `exec`. `prctl`, `seccomp`, `sendmsg`
            // and `close` are async-signal-safe syscalls; nothing here
            // allocates, locks, or touches inherited state. `program` was built
            // before the fork and is only read.
            //
            // Failure is reported by sending nothing rather than by returning
            // an error: an error here would abort the spawn, and a tracer that
            // cannot start must never stop the user's command from running.
            unsafe {
                match sys::install_listener(&prog) {
                    Ok(fd) => {
                        let _ = sys::announce(sock, fd);
                        libc::close(fd);
                    }
                    Err(_) => {
                        libc::close(sock);
                    }
                }
            }
            Ok(())
        }))
    }

    /// By now the child has exec'd and the notifier thread is already running:
    /// all that is left is to say which process is the root of the tree.
    fn attach(&mut self, pid: u32) -> Result<(), anyhow::Error> {
        // The parent's copy of the child's socket. Closing it is what turns a
        // child that died without sending a listener into an end-of-file for
        // the notifier thread rather than a five-second wait.
        self.child_sock = None;
        if let Some(rec) = lock(&self.shared).as_mut() {
            rec.root = pid as i32;
            let image = super::super::state::exe_of(pid as i32).map(|p| display_form(&p));
            rec.set_image(pid as i32, image);
        }
        Ok(())
    }

    fn finish(self: Box<Self>) -> Observations {
        let me = *self;
        me.shared.stop.store(true, Ordering::Relaxed);
        drop(me.parent_sock);
        if let Some(w) = me.worker {
            let _ = w.join();
        }
        let events = me.shared.events.load(Ordering::Relaxed);
        let Some(rec) = lock(&me.shared).take() else {
            // Only reachable if the worker never handed the recorder back,
            // which is a tracer failure and must not read as a complete trace.
            let mut obs = Observations {
                lossy: true,
                ..Default::default()
            };
            obs.downgrade(Downgrade::BackendError(
                "the seccomp session did not shut down".into(),
            ));
            return obs;
        };
        let mut obs = rec.finish(NAME);
        if me.shared.survivors.load(Ordering::Relaxed) {
            obs.downgrade(Downgrade::ChildEscape);
        }
        obs.notes.push(format!("{events} notifications"));
        obs
    }
}

/// One notification loop, owning the listener for its lifetime.
struct Session {
    listener: OwnedFd,
    cwd: PathBuf,
}

impl Session {
    fn new(listener: OwnedFd, cwd: PathBuf) -> Session {
        Session { listener, cwd }
    }

    fn run(&mut self, shared: &Shared) {
        let fd = self.listener.as_raw_fd();
        loop {
            if shared.stop.load(Ordering::Relaxed) {
                // The command has returned. The listener hangs up only once
                // every process holding the filter is gone, so anything else
                // here is a process that outlived the command and can still
                // read files this trace will never see.
                if !matches!(poll_readable(fd, SURVIVOR_GRACE_MS), Poll::Hangup) {
                    shared.survivors.store(true, Ordering::Relaxed);
                }
                return;
            }
            match poll_readable(fd, 100) {
                Poll::Ready => {}
                Poll::Timeout => continue,
                // Every process holding the filter is gone: nothing more can
                // arrive, and the loop is done.
                Poll::Hangup => return,
                Poll::Failed(e) => {
                    if let Some(r) = lock(shared).as_mut() {
                        r.fail("waiting for seccomp notifications", &e);
                    }
                    return;
                }
            }
            let notif = match sys::recv(fd) {
                Ok(n) => n,
                Err(e) => match e.raw_os_error() {
                    // The target died between the poll and the receive.
                    Some(libc::ENOENT) | Some(libc::EINTR) => continue,
                    _ => {
                        if let Some(r) = lock(shared).as_mut() {
                            r.fail("receiving a seccomp notification", &e);
                        }
                        return;
                    }
                },
            };
            shared.events.fetch_add(1, Ordering::Relaxed);
            self.handle(shared, &notif);
            // Answering is not optional: an unanswered notification blocks the
            // tracee forever. This runs whatever the decoding above did.
            let resp = seccomp_notif_resp {
                id: notif.id,
                val: 0,
                error: 0,
                flags: sys::SECCOMP_USER_NOTIF_FLAG_CONTINUE,
            };
            if let Err(e) = sys::send(fd, &resp) {
                // ENOENT means the tracee died while we were deciding, which is
                // not a failure of the trace.
                if e.raw_os_error() != Some(libc::ENOENT) {
                    if let Some(r) = lock(shared).as_mut() {
                        r.fail("answering a seccomp notification", &e);
                    }
                    return;
                }
            }
        }
    }

    fn handle(&mut self, shared: &Shared, n: &seccomp_notif) {
        let nr = n.data.nr as i64;
        let pid = n.pid;
        let unmodelled = n.data.arch != sys::NATIVE_ARCH || syscalls::decode(nr).is_none();
        self.with(shared, |r| r.syscall = nr);
        let known_pid = {
            let mut guard = lock(shared);
            let Some(rec) = guard.as_mut() else { return };
            if unmodelled {
                // Anything the filter allowed is provably irrelevant, so
                // reaching here means this build does not model the syscall —
                // or it came from an architecture whose numbers mean something
                // else entirely. Either way the trace stops being complete.
                rec.obs
                    .downgrade(Downgrade::UnsupportedSyscall(nr.max(0) as u64));
            }
            rec.knows(pid as i32)
        };
        if !known_pid {
            let image = super::super::state::exe_of(pid as i32).map(|p| display_form(&p));
            self.with(shared, |r| r.note_process(pid as i32, image));
        }
        if unmodelled {
            return;
        }
        if let Some(sc) = syscalls::decode(nr) {
            self.dispatch(shared, n, sc);
        }
    }

    fn dispatch(&mut self, shared: &Shared, n: &seccomp_notif, sc: Sc) {
        let args = n.data.args;
        let pid = n.pid;
        // A null path pointer names no file. Rust's standard library probes for
        // `statx` support with exactly this call, twice per process, and the
        // kernel answers `EFAULT` without having looked at anything. With
        // `AT_EMPTY_PATH` on 6.11 and newer it is a question about a
        // descriptor, which is dismissed as descriptor I/O either way.
        if matches!(sc, Sc::Stat { path, .. } | Sc::Open { path, .. } if args[path] == 0) {
            return;
        }
        match sc {
            Sc::Open {
                dir,
                path,
                flags,
                flags_indirect,
            } => {
                let Some(p) = self.resolve(n, dir, args[path]) else {
                    return self.unresolved(shared, n);
                };
                let f = if flags_indirect {
                    libc::O_RDONLY
                } else {
                    args[flags] as i32
                };
                let writing = f & (libc::O_WRONLY | libc::O_RDWR) != 0;
                let creating = f & libc::O_CREAT != 0;
                let truncating = f & libc::O_TRUNC != 0;
                let existed = exists(&p);
                self.with(shared, |rec| {
                    if creating && !existed {
                        rec.record(&p, FileOp::Create);
                    } else if !writing || (!truncating && !creating) {
                        // Readable, or opened for update without discarding
                        // what is there: the previous contents are an input.
                        rec.record(
                            &p,
                            if existed {
                                FileOp::Read
                            } else {
                                FileOp::Absent
                            },
                        );
                    }
                    if writing || truncating {
                        rec.record(&p, FileOp::Write);
                    }
                });
            }
            Sc::Stat { dir, path } => {
                // An empty path names no file. `fstat(fd)` reaches the kernel
                // as `newfstatat(fd, "", …, AT_EMPTY_PATH)` on every glibc
                // since 2.33, and `fstat` is already dismissed as descriptor
                // I/O. Dismissing it here too keeps the two backends agreeing,
                // and keeps the trace complete: almost every program that uses
                // stdio asks this about its own stdout, whose descriptor links
                // to `pipe:[…]` and can never be named.
                let Some(raw) = self.read_path(n, args[path]) else {
                    return self.unresolved(shared, n);
                };
                if raw.is_empty() {
                    return;
                }
                let Some(p) = self.join(n, dir, &raw) else {
                    return self.unresolved(shared, n);
                };
                let op = if exists(&p) {
                    FileOp::Stat
                } else {
                    FileOp::Absent
                };
                self.with(shared, |r| r.record(&p, op));
            }
            Sc::Exec {
                dir,
                path,
                at_flags,
            } => {
                let empty = at_flags
                    .map(|i| args[i] as i32 & libc::AT_EMPTY_PATH != 0)
                    .unwrap_or(false);
                let resolved = if empty {
                    self.fd_path(pid, args[0] as i32)
                } else {
                    self.resolve(n, dir, args[path])
                };
                let Some(p) = resolved else {
                    return self.unresolved(shared, n);
                };
                self.with(shared, |rec| {
                    rec.record(&p, FileOp::Execute);
                    rec.set_image(pid as i32, Some(display_form(&p)));
                });
            }
            Sc::ListDir { fd } => {
                let Some(p) = self.fd_path(pid, args[fd] as i32) else {
                    return self.unresolved(shared, n);
                };
                self.with(shared, |r| r.record(&p, FileOp::ListDir));
            }
            Sc::Modify { dir, path } => {
                let Some(p) = self.resolve(n, dir, args[path]) else {
                    return self.unresolved(shared, n);
                };
                let op = if exists(&p) {
                    FileOp::Write
                } else {
                    FileOp::Create
                };
                self.with(shared, |r| r.record(&p, op));
            }
            Sc::Delete { dir, path } => {
                let Some(p) = self.resolve(n, dir, args[path]) else {
                    return self.unresolved(shared, n);
                };
                self.with(shared, |r| r.record(&p, FileOp::Delete));
            }
            Sc::Rename {
                from_dir,
                from,
                to_dir,
                to,
            } => {
                let a = self.resolve(n, from_dir, args[from]);
                let b = self.resolve(n, to_dir, args[to]);
                match (a, b) {
                    (Some(a), Some(b)) => {
                        self.with(shared, |rec| {
                            rec.record(&a, FileOp::Rename);
                            rec.record(&b, FileOp::Create);
                        });
                    }
                    _ => self.unresolved(shared, n),
                }
            }
            Sc::Mmap { fd, flags } => {
                if args[flags] as i32 & libc::MAP_ANONYMOUS != 0 {
                    return;
                }
                let raw = args[fd] as i32;
                if raw < 0 {
                    return;
                }
                if let Some(p) = self.fd_path(pid, raw) {
                    self.with(shared, |r| r.record(&p, FileOp::Read));
                }
            }
            // Creating a socket reaches nothing; using one reaches a peer,
            // whatever its family. Both backends apply exactly this rule, which
            // is what makes "complete" mean the same thing under either.
            Sc::Connect { addr, len } => {
                // See the ptrace backend: a Unix socket that is not there is a
                // negative path dependency, not a reason to distrust the trace.
                match super::super::sys::read_unix_path(pid as i32, args[addr], args[len]) {
                    Some(p) if !exists(&p) => self.with(shared, |r| r.record(&p, FileOp::Absent)),
                    _ => self.with(shared, |r| r.obs.downgrade(Downgrade::NetworkAccess)),
                };
            }
            Sc::Random => {
                self.with(shared, |r| r.note_randomness());
            }
            Sc::Network { .. } => {
                self.with(shared, |r| r.obs.downgrade(Downgrade::NetworkAccess));
            }
            // Descriptor and working-directory bookkeeping is the kernel's,
            // read back from /proc when a path actually needs resolving.
            Sc::Chdir { .. }
            | Sc::Fchdir { .. }
            | Sc::Close { .. }
            | Sc::Dup { .. }
            | Sc::Dup2 { .. }
            | Sc::Fcntl { .. }
            | Sc::Anonymous => {}
        }
    }

    fn with<R>(&self, shared: &Shared, f: impl FnOnce(&mut Recorder) -> R) -> Option<R> {
        lock(shared).as_mut().map(f)
    }

    fn unresolved(&self, shared: &Shared, n: &seccomp_notif) {
        // A notification whose id has stopped being valid belongs to a process
        // that died before its syscall ran. The call acquired no dependency, so
        // there is nothing the trace can have missed by not naming its path.
        if !sys::id_valid(self.listener.as_raw_fd(), n.id) {
            return;
        }
        self.with(shared, |r| {
            r.unresolved("the argument could not be read back, or has no base Arc can name")
        });
    }

    /// Read a path argument out of the tracee and join it onto its base.
    ///
    /// The `id_valid` check after the read is the load-bearing part: without it
    /// a dead pid could be reused and the bytes read from a stranger.
    fn resolve(&mut self, n: &seccomp_notif, dir: Dir, ptr: u64) -> Option<PathBuf> {
        let raw = self.read_path(n, ptr)?;
        self.join(n, dir, &raw)
    }

    /// Join an already-read path argument onto its base.
    fn join(&mut self, n: &seccomp_notif, dir: Dir, raw: &[u8]) -> Option<PathBuf> {
        let path = Path::new(OsStr::from_bytes(raw));
        if path.is_absolute() {
            return Some(PathBuf::from(display_form(path)));
        }
        let base = match dir {
            Dir::Cwd => self.cwd_of(n.pid),
            Dir::Arg(i) => {
                let fd = n.data.args[i] as i32;
                if fd == libc::AT_FDCWD {
                    self.cwd_of(n.pid)
                } else {
                    self.fd_path(n.pid, fd)
                }
            }
        }?;
        Some(PathBuf::from(display_form(&base.join(path))))
    }

    /// Read a NUL-terminated path out of the tracee.
    ///
    /// The same `process_vm_readv` reader the ptrace backend uses, so the two
    /// cannot disagree about what a path argument says. It also avoids
    /// `/proc/<pid>/mem`, which container security profiles routinely deny even
    /// to a process's own parent.
    fn read_path(&mut self, n: &seccomp_notif, ptr: u64) -> Option<Vec<u8>> {
        let raw = super::super::sys::read_cstr(n.pid as i32, ptr)?;
        // A path Arc cannot name is a path Arc cannot check again later. Every
        // record it holds is a UTF-8 string, so lossy conversion here would
        // produce a name that never matches the file it came from — an input
        // that silently stops being checked. The ptrace backend refuses the
        // same bytes for the same reason; both fall back to the conservative
        // project scan, which handles arbitrary bytes correctly.
        if std::str::from_utf8(&raw).is_err() {
            return None;
        }
        // Only now is the path known to belong to the process still waiting on
        // this notification, rather than to a stranger that reused its pid.
        sys::id_valid(self.listener.as_raw_fd(), n.id).then_some(raw)
    }

    fn cwd_of(&self, pid: u32) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/{pid}/cwd"))
            .ok()
            .filter(|p| p.is_absolute())
            .or_else(|| Some(self.cwd.clone()))
    }

    /// What a descriptor refers to, asked of the kernel rather than tracked.
    ///
    /// This is what removes the need for a descriptor table, and with it every
    /// way one can drift out of step with reality — `dup`, `fcntl(F_DUPFD)`,
    /// `CLONE_FILES`, a descriptor inherited across `exec`.
    fn fd_path(&self, pid: u32, fd: i32) -> Option<PathBuf> {
        if fd < 0 {
            return None;
        }
        let target = std::fs::read_link(format!("/proc/{pid}/fd/{fd}")).ok()?;
        // A socket or a pipe links to `socket:[…]`, which is not a path. Bytes
        // Arc cannot name are refused here for the same reason as in
        // `read_path`.
        target.to_str()?;
        target
            .is_absolute()
            .then(|| PathBuf::from(display_form(&target)))
    }

}

fn exists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

enum Poll {
    Ready,
    Timeout,
    Hangup,
    Failed(io::Error),
}

fn poll_readable(fd: RawFd, timeout_ms: i32) -> Poll {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `pfd` is a live local and `fd` is owned by the caller.
    let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    if rc < 0 {
        let e = io::Error::last_os_error();
        return match e.raw_os_error() {
            Some(libc::EINTR) => Poll::Timeout,
            _ => Poll::Failed(e),
        };
    }
    if rc == 0 {
        return Poll::Timeout;
    }
    if pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
        return Poll::Hangup;
    }
    Poll::Ready
}

pub fn socketpair() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: `fds` is a live two-element array, which is what the kernel
    // writes. `SOCK_CLOEXEC` keeps the parent's end out of the exec'd image;
    // the child's end is used before `exec` and closed by it.
    let rc = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both descriptors were just created by the kernel and are owned
    // from here.
    unsafe { Ok((OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]))) }
}
