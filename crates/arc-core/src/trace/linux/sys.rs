//! Every unsafe operation the Linux backend performs, and nothing else.
//!
//! The rest of the backend is safe Rust over the types below. Keeping the
//! unsafe surface in one small module is what makes its invariants reviewable:
//! there are four kernel interfaces here — `ptrace`, `waitpid`, `fork` for a
//! one-shot capability probe, and `process_vm_readv` — and each is wrapped so a
//! caller cannot pass a malformed argument.
//!
//! Syscall arguments are read with `PTRACE_GET_SYSCALL_INFO` rather than by
//! decoding a register set. That interface reports whether a stop is an entry
//! or an exit, which removes the classic ptrace bug of losing entry/exit
//! alignment on a freshly cloned child, and it is architecture-independent, so
//! Arc needs no per-architecture register mapping and cannot mis-trace on an
//! architecture it was never built for.

use std::io;

/// A stop reported by `waitpid`, already classified. Raw wait statuses are
/// bit-packed three different ways depending on the stop; decoding once here
/// means the event loop never re-derives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// Entering or leaving a syscall. `PTRACE_O_TRACESYSGOOD` is set, so this
    /// is unambiguous.
    Syscall,
    /// A `PTRACE_EVENT_*` stop; the payload is the event code.
    Event(u32),
    /// An ordinary signal, to be delivered to the tracee.
    Signal(i32),
    Exited(i32),
    Killed(i32),
}

/// Options set on every tracee. `EXITKILL` is the one that matters for
/// cleanliness: if Arc dies, the kernel kills the tracees rather than leaving a
/// process tree stopped forever.
const OPTIONS: libc::c_long = (libc::PTRACE_O_TRACESYSGOOD
    | libc::PTRACE_O_TRACEFORK
    | libc::PTRACE_O_TRACEVFORK
    | libc::PTRACE_O_TRACECLONE
    | libc::PTRACE_O_TRACEEXEC
    | libc::PTRACE_O_EXITKILL) as libc::c_long;

/// `PTRACE_GET_SYSCALL_INFO`, Linux 5.3 and later. Not exposed by `libc`, and
/// part of the stable UAPI, so it is spelled out rather than pulling in another
/// bindings crate for one integer.
const PTRACE_GET_SYSCALL_INFO: libc::c_uint = 0x420e;

const INFO_NONE: u8 = 0;
const INFO_ENTRY: u8 = 1;
const INFO_EXIT: u8 = 2;

/// Layout of `struct ptrace_syscall_info` from `<linux/ptrace.h>`.
///
/// The trailing union is the largest of its variants — `seccomp`, at seven
/// `__u64` plus a `__u32` — rounded up to eight `__u64`. Reading it through a
/// `u64` array rather than a Rust union keeps this a plain POD type with no
/// enum discriminant to get wrong; the two accessors below are the only places
/// that interpret it.
#[repr(C)]
#[derive(Clone, Copy)]
struct RawSyscallInfo {
    op: u8,
    pad: [u8; 3],
    arch: u32,
    instruction_pointer: u64,
    stack_pointer: u64,
    u: [u64; 8],
}

/// What a syscall stop is, with the data that stop carries.
#[derive(Debug, Clone, Copy)]
pub enum SyscallStop {
    Entry {
        nr: u64,
        args: [u64; 6],
    },
    Exit {
        ret: i64,
        is_error: bool,
    },
    /// The kernel reported a stop that is not a syscall boundary.
    Other,
}

/// Put the calling process under its parent's control, then let `exec` proceed.
///
/// # Safety
///
/// Must only be called between `fork` and `exec` in the child. Everything it
/// does is async-signal-safe, which is exactly what `pre_exec` requires: one
/// `ptrace` syscall, no allocation, no locks, no libc state.
pub unsafe fn traceme() -> io::Result<()> {
    if libc::ptrace(libc::PTRACE_TRACEME, 0, 0, 0) < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Apply Arc's tracing options to a stopped tracee.
pub fn set_options(pid: i32) -> io::Result<()> {
    // SAFETY: `PTRACE_SETOPTIONS` reads only the integer option mask and ignores
    // `addr`. `pid` names a tracee that is stopped, which is the kernel's
    // precondition; a tracee that has since died returns ESRCH rather than
    // corrupting anything.
    let rc = unsafe { libc::ptrace(libc::PTRACE_SETOPTIONS, pid, 0, OPTIONS) };
    ok(rc).map(|_| ())
}

/// Resume a stopped tracee until its next syscall boundary, delivering
/// `signal` (0 for none).
pub fn resume(pid: i32, signal: i32) -> io::Result<()> {
    // SAFETY: `PTRACE_SYSCALL` ignores `addr`; `data` is a signal number the
    // kernel validates. ESRCH on a dead tracee is reported to the caller, which
    // treats it as "process gone".
    let rc = unsafe { libc::ptrace(libc::PTRACE_SYSCALL, pid, 0, signal as libc::c_long) };
    ok(rc).map(|_| ())
}

/// Stop tracing `pid`, leaving it running. Used when the tracer gives up
/// mid-execution: the user's command must still finish normally.
pub fn detach(pid: i32) {
    // SAFETY: detaching a pid that is not a tracee, or has already exited,
    // fails with ESRCH and changes nothing. The result is ignored because this
    // runs on the failure path, where there is nothing further to be done.
    unsafe {
        libc::ptrace(libc::PTRACE_DETACH, pid, 0, 0);
    }
}

/// The message attached to a `PTRACE_EVENT_*` stop: the new pid for the clone
/// events.
pub fn event_message(pid: i32) -> io::Result<u64> {
    let mut msg: libc::c_ulong = 0;
    // SAFETY: `data` must point at a `c_ulong` the kernel may write once. `msg`
    // is a live, unaliased local of exactly that type.
    let rc = unsafe { libc::ptrace(libc::PTRACE_GETEVENTMSG, pid, 0, &mut msg) };
    ok(rc).map(|_| msg as u64)
}

/// Ask the kernel what kind of syscall stop this is, and for its data.
pub fn syscall_info(pid: i32) -> io::Result<SyscallStop> {
    let mut info = RawSyscallInfo {
        op: INFO_NONE,
        pad: [0; 3],
        arch: 0,
        instruction_pointer: 0,
        stack_pointer: 0,
        u: [0; 8],
    };
    let size = std::mem::size_of::<RawSyscallInfo>() as libc::c_long;
    // SAFETY: `addr` is the size of the buffer and `data` points at exactly
    // that many bytes of live, unaliased, correctly aligned storage. The kernel
    // writes at most `size` bytes and returns how many it wrote; the struct is
    // fully initialised beforehand, so a short write leaves valid zeros rather
    // than uninitialised memory.
    let rc = unsafe { libc::ptrace(PTRACE_GET_SYSCALL_INFO, pid, size, &mut info) };
    ok(rc)?;
    Ok(match info.op {
        INFO_ENTRY => SyscallStop::Entry {
            nr: info.u[0],
            args: [
                info.u[1], info.u[2], info.u[3], info.u[4], info.u[5], info.u[6],
            ],
        },
        INFO_EXIT => SyscallStop::Exit {
            ret: info.u[0] as i64,
            is_error: info.u[1] & 0xff != 0,
        },
        _ => SyscallStop::Other,
    })
}

/// Wait for any tracee to stop or exit.
///
/// `__WALL` is required: without it, clone-based threads are invisible to
/// `waitpid` and a multithreaded tracee would stall forever.
pub fn wait_any() -> io::Result<(i32, Stop)> {
    let mut status: libc::c_int = 0;
    // SAFETY: `status` is a live local `c_int` the kernel writes at most once.
    let pid = unsafe { libc::waitpid(-1, &mut status, libc::__WALL) };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((pid, classify(status)))
}

fn classify(status: libc::c_int) -> Stop {
    if libc::WIFEXITED(status) {
        return Stop::Exited(libc::WEXITSTATUS(status));
    }
    if libc::WIFSIGNALED(status) {
        return Stop::Killed(libc::WTERMSIG(status));
    }
    let sig = libc::WSTOPSIG(status);
    // A `PTRACE_EVENT_*` stop is SIGTRAP with the event code in the high byte.
    let event = (status >> 16) & 0xff;
    if sig == libc::SIGTRAP && event != 0 {
        return Stop::Event(event as u32);
    }
    // With TRACESYSGOOD the kernel sets bit 7 on syscall stops, which is the
    // only way to tell one from a SIGTRAP the program raised itself.
    if sig == libc::SIGTRAP | 0x80 {
        return Stop::Syscall;
    }
    Stop::Signal(sig)
}

/// Longest path Arc will read back from a tracee. `PATH_MAX` is 4096; a longer
/// string cannot name a file the kernel would accept.
const PATH_LIMIT: usize = 4096;

/// Read a NUL-terminated string out of another process's address space.
///
/// `process_vm_readv` needs no ptrace peek loop and no privileges beyond
/// already being the tracer. It fails atomically per iovec, so the read is done
/// in page-bounded chunks: a string ending one byte before an unmapped page
/// must still be readable.
pub fn read_cstr(pid: i32, addr: u64) -> Option<Vec<u8>> {
    if addr == 0 {
        return None;
    }
    const PAGE: u64 = 4096;
    let mut out: Vec<u8> = Vec::with_capacity(64);
    let mut cursor = addr;
    while out.len() < PATH_LIMIT {
        let chunk = ((PAGE - (cursor % PAGE)) as usize).min(PATH_LIMIT - out.len());
        let mut buf = vec![0u8; chunk];
        let n = read_into(pid, cursor, &mut buf);
        if n == 0 {
            return (!out.is_empty()).then_some(out);
        }
        match buf[..n].iter().position(|b| *b == 0) {
            Some(end) => {
                out.extend_from_slice(&buf[..end]);
                return Some(out);
            }
            None => out.extend_from_slice(&buf[..n]),
        }
        cursor += n as u64;
    }
    Some(out)
}

/// The address family of a socket, read out of a `sockaddr` in the tracee. Only
/// the first two bytes are needed, and every `sockaddr` variant begins with
/// them.
pub fn read_sa_family(pid: i32, addr: u64, len: u64) -> Option<u16> {
    if addr == 0 || len < 2 {
        return None;
    }
    let mut buf = [0u8; 2];
    (read_into(pid, addr, &mut buf) == 2).then(|| u16::from_ne_bytes(buf))
}

/// Bytes actually read; 0 on any failure. The only place `process_vm_readv` is
/// called.
fn read_into(pid: i32, addr: u64, buf: &mut [u8]) -> usize {
    let local = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let remote = libc::iovec {
        iov_base: addr as *mut libc::c_void,
        iov_len: buf.len(),
    };
    // SAFETY: `local` describes the caller's live, uniquely borrowed buffer and
    // its length matches exactly. `remote` describes memory in another process,
    // which the kernel validates itself — an unmapped address returns EFAULT
    // rather than faulting here. Both iovec counts are 1, matching the pointers.
    let n = unsafe { libc::process_vm_readv(pid, &local, 1, &remote, 1, 0) };
    if n <= 0 {
        0
    } else {
        (n as usize).min(buf.len())
    }
}

fn ok(rc: libc::c_long) -> io::Result<libc::c_long> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

/// Whether ptrace works here at all, decided by trying it once rather than by
/// guessing from kernel version or capability bits.
///
/// A container may permit ptrace, forbid it through seccomp, or forbid it
/// through `kernel.yama.ptrace_scope`, and only an attempt distinguishes them.
/// The probe forks a child that puts itself under tracing and stops; the parent
/// learns the answer from how that child stopped.
pub fn probe() -> Result<(), String> {
    // SAFETY: the child branch calls only `ptrace`, `raise` and `_exit`, all of
    // which are async-signal-safe and therefore legal between `fork` and
    // `_exit` in a process that has other threads. It never returns, never
    // allocates, and never touches inherited locks.
    let pid = unsafe {
        let pid = libc::fork();
        if pid == 0 {
            if libc::ptrace(libc::PTRACE_TRACEME, 0, 0, 0) < 0 {
                libc::_exit(NO_PTRACE);
            }
            libc::raise(libc::SIGSTOP);
            libc::_exit(0);
        }
        pid
    };
    if pid < 0 {
        return Err(io::Error::last_os_error().to_string());
    }

    let mut status: libc::c_int = 0;
    // SAFETY: `status` is a live local the kernel writes at most once, and
    // `pid` is this process's own child.
    let rc = unsafe { libc::waitpid(pid, &mut status, 0) };
    if rc < 0 {
        return Err(io::Error::last_os_error().to_string());
    }

    if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == NO_PTRACE {
        reap(pid);
        return Err(ptrace_denied_reason());
    }
    if !libc::WIFSTOPPED(status) {
        reap(pid);
        return Err("ptrace did not take effect".into());
    }

    // The child is a stopped tracee, so this is also the one chance to confirm
    // that the syscall-info interface exists before committing to it.
    let info = syscall_info(pid);
    // SAFETY: `pid` is this process's own stopped child; killing it is the
    // documented way to end a probe tracee.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    detach(pid);
    reap(pid);

    match info {
        Ok(_) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::EIO) => {
            Err("kernel does not support PTRACE_GET_SYSCALL_INFO (needs Linux 5.3)".into())
        }
        Err(e) => Err(format!("PTRACE_GET_SYSCALL_INFO unavailable: {e}")),
    }
}

const NO_PTRACE: libc::c_int = 42;

fn reap(pid: i32) {
    let mut status: libc::c_int = 0;
    // SAFETY: reaping this process's own child. `WNOHANG` keeps the probe from
    // blocking if the child is already gone.
    unsafe { while libc::waitpid(pid, &mut status, libc::WNOHANG) > 0 {} }
}

/// Turn a refused `PTRACE_TRACEME` into something a user can act on. The three
/// realistic causes are distinguishable from `/proc`, and naming the right one
/// is the difference between a fixable message and a shrug.
fn ptrace_denied_reason() -> String {
    match std::fs::read_to_string("/proc/sys/kernel/yama/ptrace_scope") {
        Ok(v) if v.trim() == "3" => {
            "ptrace is disabled system-wide (kernel.yama.ptrace_scope = 3)".into()
        }
        _ => "ptrace denied, most likely by a container seccomp profile".into(),
    }
}
