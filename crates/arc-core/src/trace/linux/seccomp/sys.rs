//! Raw seccomp user-notification ABI.
//!
//! Every structure here is a kernel ABI type and must keep its exact layout;
//! every ioctl number is derived from `sizeof` of that type, which is how the
//! kernel checks that userspace and kernel agree. Nothing in this file
//! interprets a path or makes a policy decision.

#![allow(non_camel_case_types)]

use std::io;
use std::os::unix::io::RawFd;

pub const SECCOMP_SET_MODE_FILTER: u32 = 1;
pub const SECCOMP_GET_NOTIF_SIZES: u32 = 3;
pub const SECCOMP_FILTER_FLAG_NEW_LISTENER: u32 = 1 << 3;

pub const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
pub const SECCOMP_RET_USER_NOTIF: u32 = 0x7fc0_0000;

/// Let the syscall run unmodified. Added in Linux 5.5; without it a notifier
/// can only fake a return value, which is useless for observation.
pub const SECCOMP_USER_NOTIF_FLAG_CONTINUE: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct sock_filter {
    pub code: u16,
    pub jt: u8,
    pub jf: u8,
    pub k: u32,
}

#[repr(C)]
pub struct sock_fprog {
    pub len: u16,
    pub filter: *const sock_filter,
}

// SAFETY: the pointer is only ever read, points at a `Vec<sock_filter>` kept
// alive by the owner of this struct, and the kernel copies the program in
// during the `seccomp` call.
unsafe impl Send for sock_fprog {}
unsafe impl Sync for sock_fprog {}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct seccomp_data {
    pub nr: i32,
    pub arch: u32,
    pub instruction_pointer: u64,
    pub args: [u64; 6],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct seccomp_notif {
    pub id: u64,
    pub pid: u32,
    pub flags: u32,
    pub data: seccomp_data,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct seccomp_notif_resp {
    pub id: u64,
    pub val: i64,
    pub error: i32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct seccomp_notif_sizes {
    pub seccomp_notif: u16,
    pub seccomp_notif_resp: u16,
    pub seccomp_data: u16,
}

const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;
const SECCOMP_IOC_MAGIC: u32 = b'!' as u32;

const fn ioc(dir: u32, nr: u32, size: usize) -> u64 {
    ((dir << 30) | ((size as u32) << 16) | (SECCOMP_IOC_MAGIC << 8) | nr) as u64
}

pub const SECCOMP_IOCTL_NOTIF_RECV: u64 = ioc(
    IOC_READ | IOC_WRITE,
    0,
    std::mem::size_of::<seccomp_notif>(),
);
pub const SECCOMP_IOCTL_NOTIF_SEND: u64 = ioc(
    IOC_READ | IOC_WRITE,
    1,
    std::mem::size_of::<seccomp_notif_resp>(),
);
pub const SECCOMP_IOCTL_NOTIF_ID_VALID: u64 = ioc(IOC_WRITE, 2, std::mem::size_of::<u64>());

/// The architecture token the kernel puts in `seccomp_data.arch`. A filter that
/// does not check it can be fooled by a process issuing 32-bit syscalls, whose
/// numbers mean something else entirely.
pub const NATIVE_ARCH: u32 = native_arch();

const fn native_arch() -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        0xc000_003e
    }
    #[cfg(target_arch = "aarch64")]
    {
        0xc000_00b7
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        0
    }
}

/// Whether Arc has a syscall vocabulary for this architecture at all.
pub const ARCH_SUPPORTED: bool = NATIVE_ARCH != 0;

/// # Safety
///
/// `prog` must describe a valid cBPF program that remains live for the duration
/// of the call. On success the returned descriptor owns a kernel notification
/// queue; dropping it makes every filtered syscall in the tracee fail with
/// `ENOSYS`, so it must outlive the traced process tree.
pub unsafe fn install_listener(prog: &sock_fprog) -> io::Result<RawFd> {
    if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = libc::syscall(
        libc::SYS_seccomp,
        SECCOMP_SET_MODE_FILTER as libc::c_long,
        SECCOMP_FILTER_FLAG_NEW_LISTENER as libc::c_long,
        prog as *const sock_fprog,
    );
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd as RawFd)
}

pub fn notif_sizes() -> io::Result<seccomp_notif_sizes> {
    let mut sizes = seccomp_notif_sizes::default();
    // SAFETY: `sizes` is a live local of exactly the type the kernel writes.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            SECCOMP_GET_NOTIF_SIZES as libc::c_long,
            0 as libc::c_long,
            &mut sizes as *mut seccomp_notif_sizes,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(sizes)
}

/// Whether the kernel's notion of these structures matches this build's.
///
/// The ioctl numbers embed `sizeof`, so a mismatch would make every call fail
/// with `ENOTTY`. Checking up front turns that into one clear reason.
pub fn sizes_match() -> io::Result<bool> {
    let s = notif_sizes()?;
    Ok(
        s.seccomp_notif as usize == std::mem::size_of::<seccomp_notif>()
            && s.seccomp_notif_resp as usize == std::mem::size_of::<seccomp_notif_resp>()
            && s.seccomp_data as usize == std::mem::size_of::<seccomp_data>(),
    )
}

/// Whether anything is queued within `timeout_ms`. Used only by the probe: the
/// real loop must not give up on a tracee that is simply idle.
pub fn wait_readable(fd: RawFd, timeout_ms: i32) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `pfd` is a live local and `fd` is owned by the caller.
    unsafe { libc::poll(&mut pfd, 1, timeout_ms) > 0 && pfd.revents & libc::POLLIN != 0 }
}

pub fn recv(fd: RawFd) -> io::Result<seccomp_notif> {
    let mut n = seccomp_notif::default();
    // SAFETY: `fd` is a notification descriptor and `n` is a live, correctly
    // sized local. The kernel zeroes it before filling it in.
    let rc = unsafe { libc::ioctl(fd, SECCOMP_IOCTL_NOTIF_RECV, &mut n as *mut seccomp_notif) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n)
}

pub fn send(fd: RawFd, resp: &seccomp_notif_resp) -> io::Result<()> {
    // SAFETY: `resp` is a live, correctly sized value the kernel only reads.
    let rc = unsafe {
        libc::ioctl(
            fd,
            SECCOMP_IOCTL_NOTIF_SEND,
            resp as *const seccomp_notif_resp,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Whether the notification is still outstanding.
///
/// The reason every read of tracee state must be followed by this: between
/// receiving a notification and reading `/proc/<pid>/…`, the target can die and
/// its pid be reused by an unrelated process. Without this check Arc would
/// happily record a path resolved against a stranger.
pub fn id_valid(fd: RawFd, id: u64) -> bool {
    // SAFETY: `id` is a live local the kernel only reads.
    unsafe { libc::ioctl(fd, SECCOMP_IOCTL_NOTIF_ID_VALID, &id as *const u64) == 0 }
}

/// Handing the listener from the child to Arc.
///
/// The obvious way — `sendmsg` with `SCM_RIGHTS` — deadlocks, and the reason is
/// worth stating: by the time the child has a listener to send, the filter that
/// produced it is already in force, and `sendmsg` is one of the syscalls Arc
/// traps. The child would block in the very call that delivers the means to
/// unblock it.
///
/// So the child writes only its pid and descriptor number — `write` is on the
/// irrelevant list and runs at full speed — and Arc copies the descriptor out
/// with `pidfd_getfd`. The child then waits for one byte back, so the listener
/// cannot be closed by `exec` before Arc holds its own reference.
pub const HANDOVER_LEN: usize = 8;

/// # Safety
///
/// Async-signal-safe: `write` and `read` only, no allocation and no locks.
/// Callable between `fork` and `exec`.
pub unsafe fn announce(sock: RawFd, listener: RawFd) -> io::Result<()> {
    let mut msg = [0u8; HANDOVER_LEN];
    msg[..4].copy_from_slice(&(libc::getpid() as u32).to_ne_bytes());
    msg[4..].copy_from_slice(&(listener as u32).to_ne_bytes());
    write_all(sock, &msg)?;
    // Block until Arc confirms it holds its own reference. Without this the
    // listener could close at `exec` first, and every filtered syscall in the
    // tree would then fail with ENOSYS.
    let mut ack = [0u8; 1];
    read_exact(sock, &mut ack)
}

/// Take the child's listener. Bounded: a child that neither announces nor exits
/// must not be able to wedge Arc.
pub fn acquire(sock: RawFd) -> io::Result<RawFd> {
    if !wait_readable(sock, HANDOVER_TIMEOUT_MS) {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the child never announced a seccomp listener",
        ));
    }
    let mut msg = [0u8; HANDOVER_LEN];
    read_exact(sock, &mut msg)?;
    let pid = u32::from_ne_bytes(msg[..4].try_into().expect("4 bytes")) as i32;
    let remote = u32::from_ne_bytes(msg[4..].try_into().expect("4 bytes")) as RawFd;

    // SAFETY: both are plain syscalls taking scalars. `pidfd_open` refers to a
    // process this one just created, and `pidfd_getfd` requires ptrace-level
    // access to it, which a parent has over its own child.
    let listener = unsafe {
        let pidfd = libc::syscall(SYS_PIDFD_OPEN, pid as libc::c_long, 0 as libc::c_long);
        if pidfd < 0 {
            return Err(io::Error::last_os_error());
        }
        let got = libc::syscall(
            SYS_PIDFD_GETFD,
            pidfd as libc::c_long,
            remote as libc::c_long,
            0 as libc::c_long,
        );
        libc::close(pidfd as RawFd);
        if got < 0 {
            return Err(io::Error::last_os_error());
        }
        got as RawFd
    };
    // Only now may the child proceed: the descriptor is ours.
    if let Err(e) = write_all(sock, &[1u8]) {
        // SAFETY: `listener` was just created by `pidfd_getfd` and is unshared.
        unsafe { libc::close(listener) };
        return Err(e);
    }
    Ok(listener)
}

/// `pidfd_open` (Linux 5.3) and `pidfd_getfd` (Linux 5.6). Not in every `libc`
/// release, and the numbers are architecture-independent for both.
const SYS_PIDFD_OPEN: libc::c_long = 434;
const SYS_PIDFD_GETFD: libc::c_long = 438;

/// The child announces before `exec`, so this is microseconds in practice. The
/// bound exists so a wedged child is a failed trace rather than a hung Arc.
const HANDOVER_TIMEOUT_MS: i32 = 5_000;

fn write_all(fd: RawFd, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        // SAFETY: `buf` is a live slice and `fd` is owned by the caller.
        let n = unsafe { libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
        if n > 0 {
            buf = &buf[n as usize..];
            continue;
        }
        let e = io::Error::last_os_error();
        if n == 0 || e.raw_os_error() != Some(libc::EINTR) {
            return Err(e);
        }
    }
    Ok(())
}

fn read_exact(fd: RawFd, mut buf: &mut [u8]) -> io::Result<()> {
    while !buf.is_empty() {
        // SAFETY: `buf` is a live slice and `fd` is owned by the caller.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n > 0 {
            buf = &mut buf[n as usize..];
            continue;
        }
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the child closed the handover socket",
            ));
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINTR) {
            return Err(e);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_the_kernel_abi() {
        // Values from <linux/seccomp.h> on x86-64/aarch64, where all three
        // structures have the same size.
        assert_eq!(std::mem::size_of::<seccomp_data>(), 64);
        assert_eq!(std::mem::size_of::<seccomp_notif>(), 80);
        assert_eq!(std::mem::size_of::<seccomp_notif_resp>(), 24);
        assert_eq!(SECCOMP_IOCTL_NOTIF_RECV, 0xc0502100);
        assert_eq!(SECCOMP_IOCTL_NOTIF_SEND, 0xc0182101);
        assert_eq!(SECCOMP_IOCTL_NOTIF_ID_VALID, 0x40082102);
    }
}
