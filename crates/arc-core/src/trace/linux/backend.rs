//! The ptrace event loop and the rules that turn syscalls into dependencies.

use super::recorder::Recorder;
use super::state::{exe_of, Base, Fd, Pending, Proc, Table};
use super::sys::{self, Stop, SyscallStop};
use super::syscalls::{self, Dir, Sc};
use crate::exec::Wait;
use crate::paths::{display_form, Classifier};
use crate::trace::model::{Downgrade, FileOp, Observations};
use crate::trace::{Capabilities, Launch, PreExec, Tracer};
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

pub const NAME: &str = "linux-ptrace";

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

pub struct LinuxTracer {
    rec: Recorder,
}

impl LinuxTracer {
    pub fn new(cwd: &Path, classifier: &Classifier) -> LinuxTracer {
        LinuxTracer {
            rec: Recorder::new(cwd, classifier),
        }
    }

    fn record(&mut self, path: &Path, op: FileOp) {
        self.rec.record(path, op)
    }

    fn fail(&mut self, what: &str, e: &dyn std::fmt::Display) {
        self.rec.fail(what, e)
    }
}

impl Tracer for LinuxTracer {
    fn name(&self) -> &'static str {
        NAME
    }

    fn capabilities(&self) -> Capabilities {
        CAPABILITIES
    }

    fn launch(&self) -> Launch {
        Launch::Traced
    }

    fn pre_exec(&self) -> Option<PreExec> {
        Some(Box::new(|| {
            // SAFETY: run between `fork` and `exec` in the child. `traceme`
            // issues one `ptrace` syscall and does nothing else — no
            // allocation, no locking, no libc state.
            unsafe { sys::traceme() }
        }))
    }

    fn supervise(&mut self, pid: u32) -> Option<Result<Wait>> {
        self.rec.root = pid as i32;
        Some(Ok(run_loop(self)))
    }

    fn finish(self: Box<Self>) -> Observations {
        self.rec.finish(NAME)
    }
}

/// Everything the loop needs that is not in the tracer itself.
struct Loop {
    table: Table,
    /// Children announced by a `PTRACE_EVENT_*` on the parent but not yet
    /// stopped themselves.
    announced: HashMap<i32, Proc>,
    /// Children that stopped before their parent's event arrived. They are held
    /// stopped rather than resumed with a guessed working directory.
    orphans: HashSet<i32>,
    exit: Wait,
    seen_root_exit: bool,
}

/// Drive the whole process tree to completion.
///
/// The user's command must finish whatever happens to the tracer, so every
/// error path here detaches and falls back to an ordinary wait rather than
/// propagating. A broken optimisation is not a broken build.
fn run_loop(t: &mut LinuxTracer) -> Wait {
    let mut l = Loop {
        table: Table::default(),
        announced: HashMap::new(),
        orphans: HashSet::new(),
        exit: Wait {
            code: 0,
            signaled: false,
        },
        seen_root_exit: false,
    };
    let root = t.rec.root;
    l.table.insert(root, Proc::root(&t.rec.cwd));
    t.rec
        .note_process(root, exe_of(root).map(|p| display_form(&p)));

    loop {
        let (pid, stop) = match sys::wait_any() {
            Ok(v) => v,
            Err(e) if e.raw_os_error() == Some(libc::ECHILD) => break,
            Err(e) if e.raw_os_error() == Some(libc::EINTR) => continue,
            Err(e) => {
                t.fail("waitpid", &e);
                detach_all(&l);
                return reap_remaining(root, l.exit);
            }
        };

        match stop {
            Stop::Exited(code) => {
                finish_proc(&mut l, pid, root, code, false);
                if l.done() {
                    break;
                }
                continue;
            }
            Stop::Killed(sig) => {
                finish_proc(&mut l, pid, root, 128 + sig, true);
                if l.done() {
                    break;
                }
                continue;
            }
            Stop::Event(ev) => {
                if let Err(e) = on_event(t, &mut l, pid, ev) {
                    t.fail("ptrace event", &e);
                }
                resume(t, pid, 0);
            }
            Stop::Syscall => {
                on_syscall(t, &mut l, pid);
                resume(t, pid, 0);
            }
            Stop::Signal(sig) => {
                let deliver = on_signal(t, &mut l, pid, sig);
                if let Some(sig) = deliver {
                    resume(t, pid, sig);
                }
            }
        }
    }
    release_pending(&mut l);
    l.exit
}

impl Loop {
    /// Whether the tree is finished. `table` alone is not enough: a child that
    /// has been announced by its parent's event, or one parked waiting for that
    /// event, is still attached and still holds the command's output pipes.
    /// Stopping while either is outstanding leaves it stopped for good, and the
    /// pump threads then read those pipes until the end of time.
    fn done(&self) -> bool {
        self.seen_root_exit
            && self.table.is_empty()
            && self.announced.is_empty()
            && self.orphans.is_empty()
    }
}

/// Let go of anything still attached once the loop is finished.
fn release_pending(l: &mut Loop) {
    for pid in l.orphans.drain() {
        sys::detach(pid);
    }
    for pid in l.announced.keys() {
        sys::detach(*pid);
    }
}

fn finish_proc(l: &mut Loop, pid: i32, root: i32, code: i32, signaled: bool) {
    l.table.remove(pid);
    l.announced.remove(&pid);
    l.orphans.remove(&pid);
    if pid == root {
        l.exit = Wait { code, signaled };
        l.seen_root_exit = true;
    }
}

/// Resume a tracee, tolerating the one error that is not a bug: the process
/// exited between its stop being reported and this call.
fn resume(t: &mut LinuxTracer, pid: i32, sig: i32) {
    if let Err(e) = sys::resume(pid, sig) {
        if e.raw_os_error() != Some(libc::ESRCH) {
            t.fail("resuming tracee", &e);
        }
    }
}

fn detach_all(l: &Loop) {
    for pid in l.table.pids() {
        sys::detach(pid);
    }
    for pid in &l.orphans {
        sys::detach(*pid);
    }
    for pid in l.announced.keys() {
        sys::detach(*pid);
    }
}

/// After the tracer has given up, wait for the command to finish normally so
/// its exit status is still Arc's exit status.
fn reap_remaining(root: i32, fallback: Wait) -> Wait {
    let mut status: libc::c_int = 0;
    // SAFETY: `root` is this process's own child and `status` is a live local.
    let rc = unsafe { libc::waitpid(root, &mut status, 0) };
    if rc <= 0 {
        return fallback;
    }
    if libc::WIFEXITED(status) {
        Wait {
            code: libc::WEXITSTATUS(status),
            signaled: false,
        }
    } else if libc::WIFSIGNALED(status) {
        Wait {
            code: 128 + libc::WTERMSIG(status),
            signaled: true,
        }
    } else {
        fallback
    }
}

fn on_signal(t: &mut LinuxTracer, l: &mut Loop, pid: i32, sig: i32) -> Option<i32> {
    if l.table.get(pid).is_some_and(|p| p.configured) {
        // An ordinary signal for a process already under observation. Forward
        // it: suppressing signals would change the program's behaviour, and a
        // tracer that changes behaviour is worse than no tracer.
        return Some(sig);
    }

    if pid == t.rec.root {
        // The root's first stop is the SIGTRAP the kernel raises when a
        // `PTRACE_TRACEME` child reaches `exec`. Options can only be set on a
        // stopped tracee, so this is the moment.
        if let Err(e) = sys::set_options(pid) {
            t.fail("setting ptrace options", &e);
        }
        if let Some(p) = l.table.get_mut(pid) {
            p.configured = true;
        }
        return Some(0);
    }

    // A descendant's first stop. Its options are already inherited; the SIGSTOP
    // that announces it is an artefact of tracing and must not be delivered.
    match l.announced.remove(&pid) {
        Some(mut proc) => {
            proc.configured = true;
            l.table.insert(pid, proc);
            if !t.rec.at_process_limit() {
                t.rec.set_image(pid, exe_of(pid).map(|p| display_form(&p)));
            } else {
                t.rec.obs.downgrade(Downgrade::EventOverflow);
            }
            Some(0)
        }
        // The child stopped before its parent's event was reported. Leaving it
        // stopped is the only correct move: resuming it now would mean
        // resolving its relative paths against a working directory Arc has not
        // established yet. The parent's event releases it a moment later.
        None => {
            l.orphans.insert(pid);
            None
        }
    }
}

fn on_event(t: &mut LinuxTracer, l: &mut Loop, pid: i32, ev: u32) -> Result<()> {
    match ev as i32 {
        libc::PTRACE_EVENT_FORK | libc::PTRACE_EVENT_VFORK | libc::PTRACE_EVENT_CLONE => {
            let child = sys::event_message(pid)? as i32;
            let flags = l.table.get(pid).map(|p| p.clone_flags).unwrap_or(0);
            let Some(proc) = l.table.get(pid).map(|p| p.child(flags)) else {
                t.rec.obs.lossy = true;
                t.rec.obs.downgrade(Downgrade::ChildEscape);
                return Ok(());
            };
            if l.orphans.remove(&child) {
                let mut proc = proc;
                proc.configured = true;
                l.table.insert(child, proc);
                if !t.rec.at_process_limit() {
                    t.rec
                        .set_image(child, exe_of(child).map(|p| display_form(&p)));
                }
                resume(t, child, 0);
            } else {
                l.announced.insert(child, proc);
            }
        }
        libc::PTRACE_EVENT_EXEC => {
            // The `execve` succeeded, so the image captured at its entry stop
            // is a real dependency: for `./build.sh` that is the script, whose
            // contents decide what happens.
            if let Some(path) = l.table.get_mut(pid).and_then(|p| p.pending_exec.take()) {
                t.record(&path, FileOp::Execute);
            }
            // `/proc/<pid>/exe` is the binary the kernel actually runs, which
            // for `#!/usr/bin/env python3` is the Python interpreter rather
            // than the script. Recording both is what makes a shebang script's
            // dependency set correct.
            if let Some(exe) = exe_of(pid) {
                t.rec.set_image(pid, Some(display_form(&exe)));
            }
        }
        _ => {}
    }
    Ok(())
}

fn on_syscall(t: &mut LinuxTracer, l: &mut Loop, pid: i32) {
    let info = match sys::syscall_info(pid) {
        Ok(i) => i,
        Err(e) => {
            if e.raw_os_error() != Some(libc::ESRCH) {
                t.fail("reading syscall info", &e);
            }
            return;
        }
    };
    match info {
        SyscallStop::Entry { nr, args } => {
            t.rec.syscall = nr as i64;
            let known = syscalls::decode(nr as i64).is_some() || syscalls::is_irrelevant(nr as i64);
            if !known {
                // A syscall this backend does not model may have done anything,
                // including reading a file through an interface Arc cannot see.
                // Refusing to call the trace complete is the only safe answer.
                t.rec.obs.downgrade(Downgrade::UnsupportedSyscall(nr));
            }
            if let Some(p) = l.table.get_mut(pid) {
                p.pending = Some(Pending { nr, args });
                // `clone` flags are argument 0 on every architecture Linux
                // supports; `clone3` passes a struct, so its sharing is unknown
                // and the child is modelled as sharing nothing, which can only
                // duplicate state rather than lose it.
                if nr as i64 == libc::SYS_clone {
                    p.clone_flags = args[0];
                } else if nr as i64 == libc::SYS_clone3 {
                    p.clone_flags = 0;
                }
            }
            // Whether a path exists can only be established before the syscall
            // that may create it. This is the one place Arc touches the
            // filesystem during a trace, and only for creating opens.
            prepare_entry(t, l, pid, nr, &args);
        }
        SyscallStop::Exit { ret, is_error } => {
            let Some(pending) = l.table.get_mut(pid).and_then(|p| p.pending.take()) else {
                return;
            };
            on_exit(t, l, pid, pending, ret, is_error);
        }
        SyscallStop::Other => {}
    }
}

/// Work that can only be done before the syscall runs.
///
/// Three things qualify. Whether a path exists can only be established before
/// the call that may create it — this is the one place Arc touches the
/// filesystem during a trace, and only for creating opens. A successful
/// `execve` never returns, so its image has to be captured now and confirmed at
/// the resulting `PTRACE_EVENT_EXEC`. And the `sockaddr` a `connect` names is
/// the caller's own memory: the kernel reads it here, while the thread is
/// stopped at entry, and nothing keeps it intact past that point.
fn prepare_entry(t: &mut LinuxTracer, l: &mut Loop, pid: i32, nr: u64, args: &[u64; 6]) {
    match syscalls::decode(nr as i64) {
        Some(Sc::Open {
            dir,
            path,
            flags,
            flags_indirect: false,
        }) if args[flags] as i32 & libc::O_CREAT != 0 => {
            let Some(resolved) = resolve(t, l, pid, dir, args, path) else {
                return;
            };
            let existed = std::fs::symlink_metadata(&resolved).is_ok();
            if let Some(p) = l.table.get_mut(pid) {
                p.existed = Some(existed);
            }
        }
        Some(Sc::Connect { addr, len }) => {
            // Read now, recorded at the exit stop once the return value says
            // what happened. Reading it at the exit stop instead means reading
            // a buffer the tracee has owned again since the syscall returned,
            // and a thread sharing the address space can have written anything
            // into it -- including a shorter string, which reads back as a
            // perfectly plausible path that was never connected to.
            let named = sys::read_unix_path(pid, args[addr], args[len]);
            if let Some(p) = l.table.get_mut(pid) {
                p.pending_connect = named;
            }
        }
        Some(Sc::Exec {
            dir,
            path,
            at_flags,
        }) => {
            let empty = at_flags
                .map(|i| args[i] as i32 & libc::AT_EMPTY_PATH != 0)
                .unwrap_or(false);
            let resolved = if empty {
                l.table
                    .get(pid)
                    .and_then(|p| p.fds.borrow().path(args[0] as i32))
                    .map(|p| (*p).clone())
            } else {
                resolve(t, l, pid, dir, args, path)
            };
            if let Some(p) = l.table.get_mut(pid) {
                p.pending_exec = resolved;
            }
        }
        _ => {}
    }
}

fn on_exit(t: &mut LinuxTracer, l: &mut Loop, pid: i32, p: Pending, ret: i64, is_error: bool) {
    let Some(sc) = syscalls::decode(p.nr as i64) else {
        return;
    };
    t.rec.syscall = p.nr as i64;
    let args = &p.args;
    let existed = l.table.get_mut(pid).and_then(|pr| pr.existed.take());

    match sc {
        Sc::Open {
            dir,
            path,
            flags,
            flags_indirect,
        } => {
            let Some(resolved) = resolve(t, l, pid, dir, args, path) else {
                return;
            };
            if is_error {
                if missing(ret) {
                    t.record(&resolved, FileOp::Absent);
                }
                return;
            }
            // `openat2` carries its flags in a struct rather than a register.
            // Treating that open as readable is the conservative direction: the
            // file becomes an input, which can only cost a miss.
            let f = if flags_indirect {
                libc::O_RDONLY
            } else {
                args[flags] as i32
            };
            let created = f & libc::O_CREAT != 0 && existed == Some(false);
            let readable = f & libc::O_ACCMODE != libc::O_WRONLY;
            let path_only = f & libc::O_PATH != 0;

            if created {
                // The execution brought this file into existence. Recording it
                // as an input would make the next run demand a file that only
                // exists because of the previous one.
                t.record(&resolved, FileOp::Create);
            } else if path_only {
                t.record(&resolved, FileOp::Stat);
            } else if readable {
                t.record(&resolved, FileOp::Read);
            }
            if f & (libc::O_WRONLY | libc::O_RDWR | libc::O_TRUNC) != 0 && !created {
                t.record(&resolved, FileOp::Write);
            }
            if ret >= 0 {
                set_fd(l, pid, ret as i32, Fd::Path(Rc::new(resolved)));
            }
        }

        Sc::Stat { dir, path } => {
            let resolved = match resolve_arg(t, l, pid, dir, args, path) {
                PathArg::Resolved(p) => p,
                // `fstat` in another syscall's clothing. Descriptor metadata is
                // not a dependency on a name, and it must not cost the trace
                // its completeness: almost every program that uses stdio asks
                // this about its own stdout.
                PathArg::Empty | PathArg::Unresolved => return,
            };
            t.record(
                &resolved,
                if is_error && missing(ret) {
                    FileOp::Absent
                } else if is_error {
                    return;
                } else {
                    FileOp::Stat
                },
            );
        }

        Sc::Exec { .. } => {
            // A successful `execve` never returns, so reaching its exit stop
            // means it failed. The successful case is recorded from the
            // `PTRACE_EVENT_EXEC` stop instead.
            let attempted = l.table.get_mut(pid).and_then(|p| p.pending_exec.take());
            if let (Some(path), true) = (attempted, is_error && missing(ret)) {
                t.record(&path, FileOp::Absent);
            }
        }

        Sc::ListDir { fd } => {
            if is_error {
                return;
            }
            let dir = l
                .table
                .get(pid)
                .and_then(|p| p.fds.borrow().path(args[fd] as i32));
            if let Some(d) = dir {
                t.record(&d, FileOp::ListDir);
            }
        }

        Sc::Chdir { path } => {
            if is_error {
                return;
            }
            let Some(resolved) = resolve(t, l, pid, Dir::Cwd, args, path) else {
                return;
            };
            if let Some(p) = l.table.get(pid) {
                p.set_cwd(resolved);
            }
        }

        Sc::Fchdir { fd } => {
            if is_error {
                return;
            }
            let target = l
                .table
                .get(pid)
                .and_then(|p| p.fds.borrow().path(args[fd] as i32));
            match target {
                Some(d) => {
                    if let Some(p) = l.table.get(pid) {
                        p.set_cwd((*d).clone());
                    }
                }
                // The working directory is now something Arc cannot name, so
                // every later relative path in this process is unresolvable.
                None => t.rec.unresolved("the new working directory has no name"),
            }
        }

        Sc::Close { fd } => {
            if let Some(p) = l.table.get(pid) {
                p.fds.borrow_mut().remove(args[fd] as i32);
            }
        }

        Sc::Dup { from } => {
            if ret >= 0 {
                if let Some(p) = l.table.get(pid) {
                    p.fds.borrow_mut().dup(args[from] as i32, ret as i32);
                }
            }
        }

        Sc::Dup2 { from, to } => {
            if !is_error {
                if let Some(p) = l.table.get(pid) {
                    p.fds.borrow_mut().dup(args[from] as i32, args[to] as i32);
                }
            }
        }

        Sc::Fcntl { fd, cmd } => {
            if ret >= 0 && matches!(args[cmd] as i32, libc::F_DUPFD | libc::F_DUPFD_CLOEXEC) {
                if let Some(p) = l.table.get(pid) {
                    p.fds.borrow_mut().dup(args[fd] as i32, ret as i32);
                }
            }
        }

        Sc::Mmap { fd, flags } => {
            // A private anonymous mapping has no file behind it. A file mapping
            // does, and it was already recorded when the descriptor was opened;
            // this records it again as a read so a mapped input is visible.
            if is_error || args[flags] as i32 & libc::MAP_ANONYMOUS != 0 {
                return;
            }
            let mapped = l
                .table
                .get(pid)
                .and_then(|p| p.fds.borrow().path(args[fd] as i32));
            if let Some(m) = mapped {
                t.record(&m, FileOp::Read);
            }
        }

        Sc::Modify { dir, path } => {
            if is_error {
                if missing(ret) {
                    if let Some(r) = resolve(t, l, pid, dir, args, path) {
                        t.record(&r, FileOp::Absent);
                    }
                }
                return;
            }
            if let Some(r) = resolve(t, l, pid, dir, args, path) {
                t.record(&r, FileOp::Write);
            }
        }

        Sc::Delete { dir, path } => {
            if is_error {
                return;
            }
            if let Some(r) = resolve(t, l, pid, dir, args, path) {
                t.record(&r, FileOp::Delete);
            }
        }

        Sc::Rename {
            from_dir,
            from,
            to_dir,
            to,
        } => {
            if is_error {
                return;
            }
            if let Some(r) = resolve(t, l, pid, from_dir, args, from) {
                t.record(&r, FileOp::Rename);
                t.record(&r, FileOp::Delete);
            }
            if let Some(r) = resolve(t, l, pid, to_dir, args, to) {
                t.record(&r, FileOp::Rename);
            }
        }

        Sc::Connect { .. } => {
            // glibc asks `/var/run/nscd/socket` for every user and group
            // lookup, and on a machine with no nscd the answer is ENOENT. That
            // is a fact about the filesystem, and it is fingerprintable: record
            // the absence and keep the trace complete. A socket that IS there
            // answers with something Arc cannot reproduce, so it downgrades.
            //
            // The path was read at the entry stop; see `prepare_entry`.
            match l.table.get_mut(pid).and_then(|p| p.pending_connect.take()) {
                Some(p) if !p.exists() => t.record(&p, FileOp::Absent),
                _ => t.rec.obs.downgrade(Downgrade::NetworkAccess),
            }
        }

        Sc::Network { .. } => {
            // Deliberately not gated on success, and deliberately not gated on
            // the address family. A refused connection still means the result
            // depended on whether something was listening, and a peer on a Unix
            // socket answers with something Arc cannot reproduce from the
            // filesystem. See DECISIONS.md.
            t.rec.obs.downgrade(Downgrade::NetworkAccess);
        }

        Sc::Random => t.rec.note_randomness(),

        Sc::Anonymous => {
            if ret >= 0 {
                set_fd(l, pid, ret as i32, Fd::Anonymous);
            }
        }
    }
}

fn set_fd(l: &mut Loop, pid: i32, fd: i32, entry: Fd) {
    if let Some(p) = l.table.get(pid) {
        p.fds.borrow_mut().insert(fd, entry);
    }
}

/// `ENOENT` and `ENOTDIR` both mean "this path does not name a file", which is
/// the negative dependency Arc must remember.
fn missing(ret: i64) -> bool {
    let err = -ret;
    err == libc::ENOENT as i64 || err == libc::ENOTDIR as i64
}

/// What a path argument turned out to be.
pub(crate) enum PathArg {
    /// A path Arc can name.
    Resolved(PathBuf),
    /// The empty string, which names no file. Only ever legal with
    /// `AT_EMPTY_PATH`, where the question is about the descriptor rather than
    /// about a name.
    Empty,
    /// Unreadable, not valid UTF-8, or relative to a base Arc cannot name.
    Unresolved,
}

/// Read a path argument out of the tracee and resolve it the way the kernel
/// will: against that process's working directory or the directory a descriptor
/// names, never against Arc's own.
///
/// The caller decides what an empty path means, because it is not the same
/// question in every syscall.
fn resolve_arg(
    t: &mut LinuxTracer,
    l: &mut Loop,
    pid: i32,
    dir: Dir,
    args: &[u64; 6],
    idx: usize,
) -> PathArg {
    match resolve(t, l, pid, dir, args, idx) {
        Some(p) => PathArg::Resolved(p),
        None if t.rec.empty_path_arg => PathArg::Empty,
        None => PathArg::Unresolved,
    }
}

/// Read a path argument out of the tracee and resolve it the way the kernel
/// will: against that process's working directory or the directory a descriptor
/// names, never against Arc's own.
fn resolve(
    t: &mut LinuxTracer,
    l: &mut Loop,
    pid: i32,
    dir: Dir,
    args: &[u64; 6],
    idx: usize,
) -> Option<PathBuf> {
    t.rec.empty_path_arg = false;
    // A null path pointer names no file: see the seccomp backend's `read_path`.
    if args[idx] == 0 {
        t.rec.empty_path_arg = true;
        return None;
    }
    let raw = match sys::read_cstr(pid, args[idx]) {
        Some(r) => r,
        None => {
            t.rec
                .unresolved("the argument could not be read out of the process");
            return None;
        }
    };
    // An empty path names no file. `fstat(fd)` reaches the kernel as
    // `newfstatat(fd, "", …, AT_EMPTY_PATH)` on every glibc since 2.33, so this
    // is overwhelmingly a question about a descriptor, and `fstat` is already
    // dismissed as descriptor I/O. Report it as such rather than resolving the
    // empty string against a base and inventing a dependency on a directory
    // nothing looked at.
    if raw.is_empty() {
        t.rec.empty_path_arg = true;
        return None;
    }
    // Linux paths are bytes; Arc's stored path identity is text. A filename that
    // is not valid UTF-8 cannot make that round trip — the stored name would
    // refer to a file that does not exist, which fingerprints identically
    // forever and would hit when it should miss. Rather than record a path it
    // cannot name, the trace stops claiming completeness and the conservative
    // project scan takes over, which handles arbitrary bytes correctly.
    if std::str::from_utf8(&raw).is_err() {
        t.rec.unresolved("the path is not valid UTF-8");
        return None;
    }
    let raw = Path::new(OsStr::from_bytes(&raw));
    let proc = l.table.get(pid)?;
    let base = match dir {
        Dir::Cwd => Base::Cwd,
        Dir::Arg(i) => proc.base_for(args[i] as i32),
    };
    match proc.resolve(&base, raw) {
        Some(p) => Some(p),
        None => {
            let detail = format!("{} has no base Arc can name", raw.display());
            t.rec.unresolved(&detail);
            None
        }
    }
}
