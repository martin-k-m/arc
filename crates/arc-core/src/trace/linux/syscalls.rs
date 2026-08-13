//! Which syscalls carry a dependency, and which are provably irrelevant.
//!
//! A tracer that models `openat` and ignores everything else is not complete,
//! it is merely quiet. Every syscall a tracee executes falls into exactly one of
//! three buckets here:
//!
//! * **modelled** — [`decode`] describes where its paths and descriptors are,
//!   and the backend records the dependency;
//! * **irrelevant** — enumerated in [`is_irrelevant`], on the grounds that the
//!   call cannot name a file, cannot create a process, and cannot reach outside
//!   the process tree. Descriptor I/O belongs here because the dependency was
//!   already recorded when the descriptor was opened;
//! * **unknown** — anything else, including syscalls a newer kernel adds. These
//!   downgrade the trace. Assuming an unrecognised syscall is harmless is
//!   precisely how a tracer becomes wrong on a kernel it was never tested on.
//!
//! Syscall numbers come from `libc` and are never hardcoded. Numbers that exist
//! only on x86-64 live in the `legacy` functions, which compile to `None`/`false`
//! elsewhere — so no sentinel value can ever collide with a real number.

/// Where a `*at` syscall resolves a relative path from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    /// The process's working directory.
    Cwd,
    /// The descriptor in argument `n`.
    Arg(usize),
}

/// A syscall Arc understands, with the argument positions it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sc {
    /// `open`/`openat`/`openat2`. `flags` decides input versus output.
    Open {
        dir: Dir,
        path: usize,
        flags: usize,
        /// `openat2` passes an `open_how` struct rather than a flags word, so
        /// the flags cannot be read from a register and the open is treated as
        /// readable — the conservative direction.
        flags_indirect: bool,
    },
    /// Metadata or existence was consulted: `stat`, `statx`, `access`,
    /// `readlink`, `statfs`, `getxattr`.
    Stat {
        dir: Dir,
        path: usize,
    },
    /// `execve`/`execveat`.
    Exec {
        dir: Dir,
        path: usize,
        at_flags: Option<usize>,
    },
    /// `getdents64`: the entry set of the directory behind a descriptor.
    ListDir {
        fd: usize,
    },
    Chdir {
        path: usize,
    },
    Fchdir {
        fd: usize,
    },
    Close {
        fd: usize,
    },
    /// `dup`: the new descriptor is the return value.
    Dup {
        from: usize,
    },
    /// `dup2`/`dup3`: the new descriptor is an argument.
    Dup2 {
        from: usize,
        to: usize,
    },
    Fcntl {
        fd: usize,
        cmd: usize,
    },
    /// A file mapping. The descriptor was resolved when it was opened; this is
    /// recorded so a mapped input is visible in `--trace --verbose`.
    Mmap {
        fd: usize,
        flags: usize,
    },
    /// The path's contents or metadata were modified: `truncate`, `chmod`,
    /// `utimensat`, `setxattr`, `mkdir`, `symlink`, `link`, `mknod`, `creat`.
    Modify {
        dir: Dir,
        path: usize,
    },
    Delete {
        dir: Dir,
        path: usize,
    },
    Rename {
        from_dir: Dir,
        from: usize,
        to_dir: Dir,
        to: usize,
    },
    Socket {
        family: usize,
    },
    /// The socket in `fd` was used to reach something outside this execution.
    Network {
        fd: usize,
        addr: Option<usize>,
        len: Option<usize>,
    },
    /// Creates a descriptor that can never name a file.
    Anonymous,
}

/// Decode a syscall number, or `None` when Arc does not model it.
pub fn decode(nr: i64) -> Option<Sc> {
    use Dir::{Arg, Cwd};
    if let Some(sc) = decode_legacy(nr) {
        return Some(sc);
    }
    let sc = match nr {
        // ---- opening --------------------------------------------------------
        libc::SYS_openat => Sc::Open {
            dir: Arg(0),
            path: 1,
            flags: 2,
            flags_indirect: false,
        },
        libc::SYS_openat2 => Sc::Open {
            dir: Arg(0),
            path: 1,
            flags: 2,
            flags_indirect: true,
        },

        // ---- metadata and existence -----------------------------------------
        libc::SYS_statfs => Sc::Stat { dir: Cwd, path: 0 },
        libc::SYS_getxattr | libc::SYS_lgetxattr | libc::SYS_listxattr | libc::SYS_llistxattr => {
            Sc::Stat { dir: Cwd, path: 0 }
        }
        libc::SYS_newfstatat
        | libc::SYS_readlinkat
        | libc::SYS_faccessat
        | libc::SYS_faccessat2
        | libc::SYS_statx => Sc::Stat {
            dir: Arg(0),
            path: 1,
        },

        // ---- directory enumeration -------------------------------------------
        libc::SYS_getdents64 => Sc::ListDir { fd: 0 },

        // ---- execution --------------------------------------------------------
        libc::SYS_execve => Sc::Exec {
            dir: Cwd,
            path: 0,
            at_flags: None,
        },
        libc::SYS_execveat => Sc::Exec {
            dir: Arg(0),
            path: 1,
            at_flags: Some(4),
        },

        // ---- working directory -------------------------------------------------
        libc::SYS_chdir => Sc::Chdir { path: 0 },
        libc::SYS_fchdir => Sc::Fchdir { fd: 0 },

        // ---- descriptors --------------------------------------------------------
        libc::SYS_close => Sc::Close { fd: 0 },
        // `close_range` can only drop descriptors. A stale entry left in Arc's
        // table is harmless: the next open at that number overwrites it, and a
        // descriptor is only ever read to resolve a path the tracee itself just
        // used.
        libc::SYS_close_range => Sc::Anonymous,
        libc::SYS_dup => Sc::Dup { from: 0 },
        libc::SYS_dup3 => Sc::Dup2 { from: 0, to: 1 },
        libc::SYS_fcntl => Sc::Fcntl { fd: 0, cmd: 1 },
        libc::SYS_mmap => Sc::Mmap { fd: 4, flags: 3 },

        // ---- mutation ------------------------------------------------------------
        libc::SYS_truncate => Sc::Modify { dir: Cwd, path: 0 },
        libc::SYS_setxattr | libc::SYS_lsetxattr | libc::SYS_removexattr => {
            Sc::Modify { dir: Cwd, path: 0 }
        }
        libc::SYS_mkdirat
        | libc::SYS_mknodat
        | libc::SYS_fchmodat
        | libc::SYS_utimensat
        | libc::SYS_fchownat => Sc::Modify {
            dir: Arg(0),
            path: 1,
        },
        // For `symlinkat` the first argument is the link *text*, which is data
        // rather than a path to resolve; the entry being created is what the
        // filesystem gains.
        libc::SYS_symlinkat => Sc::Modify {
            dir: Arg(1),
            path: 2,
        },
        libc::SYS_linkat => Sc::Modify {
            dir: Arg(2),
            path: 3,
        },
        libc::SYS_unlinkat => Sc::Delete {
            dir: Arg(0),
            path: 1,
        },
        libc::SYS_renameat | libc::SYS_renameat2 => Sc::Rename {
            from_dir: Arg(0),
            from: 1,
            to_dir: Arg(2),
            to: 3,
        },

        // ---- network ---------------------------------------------------------------
        libc::SYS_socket => Sc::Socket { family: 0 },
        libc::SYS_connect | libc::SYS_bind => Sc::Network {
            fd: 0,
            addr: Some(1),
            len: Some(2),
        },
        libc::SYS_sendto | libc::SYS_recvfrom => Sc::Network {
            fd: 0,
            addr: Some(4),
            len: Some(5),
        },
        libc::SYS_sendmsg | libc::SYS_recvmsg => Sc::Network {
            fd: 0,
            addr: None,
            len: None,
        },

        // ---- descriptors that can never name a file ----------------------------------
        libc::SYS_pipe2
        | libc::SYS_socketpair
        | libc::SYS_eventfd2
        | libc::SYS_epoll_create1
        | libc::SYS_signalfd4
        | libc::SYS_timerfd_create
        | libc::SYS_memfd_create
        | libc::SYS_inotify_init1 => Sc::Anonymous,

        _ => return None,
    };
    Some(sc)
}

/// Syscalls x86-64 kept from before the `*at` family existed. Compiled away
/// entirely on architectures that never had them.
#[cfg(target_arch = "x86_64")]
fn decode_legacy(nr: i64) -> Option<Sc> {
    use Dir::Cwd;
    let sc = match nr {
        libc::SYS_open => Sc::Open {
            dir: Cwd,
            path: 0,
            flags: 1,
            flags_indirect: false,
        },
        // `creat` is `open(path, O_CREAT|O_WRONLY|O_TRUNC)`: output only.
        libc::SYS_creat => Sc::Modify { dir: Cwd, path: 0 },
        libc::SYS_stat | libc::SYS_lstat | libc::SYS_access | libc::SYS_readlink => {
            Sc::Stat { dir: Cwd, path: 0 }
        }
        libc::SYS_getdents => Sc::ListDir { fd: 0 },
        libc::SYS_mkdir
        | libc::SYS_symlink
        | libc::SYS_chmod
        | libc::SYS_chown
        | libc::SYS_lchown
        | libc::SYS_mknod
        | libc::SYS_utimes => Sc::Modify { dir: Cwd, path: 0 },
        libc::SYS_link => Sc::Modify { dir: Cwd, path: 1 },
        libc::SYS_unlink | libc::SYS_rmdir => Sc::Delete { dir: Cwd, path: 0 },
        libc::SYS_rename => Sc::Rename {
            from_dir: Cwd,
            from: 0,
            to_dir: Cwd,
            to: 1,
        },
        libc::SYS_dup2 => Sc::Dup2 { from: 0, to: 1 },
        libc::SYS_pipe
        | libc::SYS_epoll_create
        | libc::SYS_inotify_init
        | libc::SYS_eventfd
        | libc::SYS_signalfd => Sc::Anonymous,
        _ => return None,
    };
    Some(sc)
}

#[cfg(not(target_arch = "x86_64"))]
fn decode_legacy(_nr: i64) -> Option<Sc> {
    None
}

/// Syscalls that cannot affect what an execution depends on.
pub fn is_irrelevant(nr: i64) -> bool {
    irrelevant_legacy(nr)
        // Descriptor I/O on paths already resolved at open time.
        || matches!(
            nr,
            libc::SYS_read
                | libc::SYS_write
                | libc::SYS_pread64
                | libc::SYS_pwrite64
                | libc::SYS_readv
                | libc::SYS_writev
                | libc::SYS_preadv
                | libc::SYS_pwritev
                | libc::SYS_preadv2
                | libc::SYS_pwritev2
                | libc::SYS_lseek
                | libc::SYS_fstat
                | libc::SYS_fstatfs
                | libc::SYS_fsync
                | libc::SYS_fdatasync
                | libc::SYS_ftruncate
                | libc::SYS_fallocate
                | libc::SYS_flock
                | libc::SYS_fadvise64
                | libc::SYS_readahead
                | libc::SYS_sendfile
                | libc::SYS_splice
                | libc::SYS_tee
                | libc::SYS_vmsplice
                | libc::SYS_copy_file_range
                | libc::SYS_fchmod
                | libc::SYS_fchown
                | libc::SYS_fgetxattr
                | libc::SYS_flistxattr
                | libc::SYS_fsetxattr
                | libc::SYS_fremovexattr
                | libc::SYS_ioctl
                | libc::SYS_sync
                | libc::SYS_syncfs
                | libc::SYS_sync_file_range
        )
        // Memory.
        || matches!(
            nr,
            libc::SYS_mprotect
                | libc::SYS_munmap
                | libc::SYS_mremap
                | libc::SYS_madvise
                | libc::SYS_mlock
                | libc::SYS_munlock
                | libc::SYS_mlockall
                | libc::SYS_munlockall
                | libc::SYS_msync
                | libc::SYS_mincore
                | libc::SYS_brk
                | libc::SYS_membarrier
        )
        // Signals, and the calls a runtime makes about itself.
        || matches!(
            nr,
            libc::SYS_rt_sigaction
                | libc::SYS_rt_sigprocmask
                | libc::SYS_rt_sigreturn
                | libc::SYS_rt_sigpending
                | libc::SYS_rt_sigtimedwait
                | libc::SYS_rt_sigqueueinfo
                | libc::SYS_rt_sigsuspend
                | libc::SYS_rt_tgsigqueueinfo
                | libc::SYS_sigaltstack
                | libc::SYS_kill
                | libc::SYS_tkill
                | libc::SYS_tgkill
                | libc::SYS_restart_syscall
                | libc::SYS_prctl
                | libc::SYS_set_tid_address
                | libc::SYS_set_robust_list
                | libc::SYS_get_robust_list
                | libc::SYS_rseq
                | libc::SYS_personality
        )
        // Process and thread lifecycle. Creation is observed through ptrace
        // events, not through the syscall itself.
        || matches!(
            nr,
            libc::SYS_clone
                | libc::SYS_clone3
                | libc::SYS_exit
                | libc::SYS_exit_group
                | libc::SYS_wait4
                | libc::SYS_waitid
                | libc::SYS_getpid
                | libc::SYS_gettid
                | libc::SYS_getppid
                | libc::SYS_setpgid
                | libc::SYS_getpgid
                | libc::SYS_getsid
                | libc::SYS_setsid
                | libc::SYS_getcwd
        )
        // Identity, limits and scheduling: read-only machine facts already
        // covered by the platform component of the execution key.
        || matches!(
            nr,
            libc::SYS_getuid
                | libc::SYS_geteuid
                | libc::SYS_getgid
                | libc::SYS_getegid
                | libc::SYS_getgroups
                | libc::SYS_setuid
                | libc::SYS_setgid
                | libc::SYS_setresuid
                | libc::SYS_setresgid
                | libc::SYS_getresuid
                | libc::SYS_getresgid
                | libc::SYS_umask
                | libc::SYS_capget
                | libc::SYS_capset
                | libc::SYS_prlimit64
                | libc::SYS_getrusage
                | libc::SYS_sysinfo
                | libc::SYS_uname
                | libc::SYS_sched_yield
                | libc::SYS_sched_getaffinity
                | libc::SYS_sched_setaffinity
                | libc::SYS_sched_getparam
                | libc::SYS_sched_setparam
                | libc::SYS_sched_getscheduler
                | libc::SYS_sched_setscheduler
                | libc::SYS_sched_get_priority_max
                | libc::SYS_sched_get_priority_min
                | libc::SYS_sched_rr_get_interval
                | libc::SYS_getpriority
                | libc::SYS_setpriority
                | libc::SYS_times
        )
        // Waiting, timers and clocks. A command that depends on the clock is
        // non-hermetic in a way no filesystem tracer can repair; that limit is
        // documented rather than turned into a downgrade on every single run.
        || matches!(
            nr,
            libc::SYS_futex
                | libc::SYS_ppoll
                | libc::SYS_pselect6
                | libc::SYS_epoll_pwait
                | libc::SYS_epoll_ctl
                | libc::SYS_nanosleep
                | libc::SYS_clock_nanosleep
                | libc::SYS_clock_gettime
                | libc::SYS_clock_getres
                | libc::SYS_gettimeofday
                | libc::SYS_timer_create
                | libc::SYS_timer_settime
                | libc::SYS_timer_gettime
                | libc::SYS_timer_delete
                | libc::SYS_timerfd_settime
                | libc::SYS_timerfd_gettime
                | libc::SYS_getrandom
        )
        // Socket calls that neither create a socket nor reach a peer. Asking a
        // socket about itself is not a dependency; reaching a peer is modelled.
        || matches!(
            nr,
            libc::SYS_listen
                | libc::SYS_accept
                | libc::SYS_accept4
                | libc::SYS_getsockname
                | libc::SYS_getpeername
                | libc::SYS_setsockopt
                | libc::SYS_getsockopt
                | libc::SYS_shutdown
        )
}

#[cfg(target_arch = "x86_64")]
fn irrelevant_legacy(nr: i64) -> bool {
    matches!(
        nr,
        libc::SYS_arch_prctl
            | libc::SYS_fork
            | libc::SYS_vfork
            | libc::SYS_getpgrp
            | libc::SYS_poll
            | libc::SYS_select
            | libc::SYS_epoll_wait
            | libc::SYS_time
            | libc::SYS_alarm
            | libc::SYS_pause
            | libc::SYS_getrlimit
            | libc::SYS_setrlimit
    )
}

#[cfg(not(target_arch = "x86_64"))]
fn irrelevant_legacy(_nr: i64) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openat_is_modelled_and_its_arguments_are_where_we_think() {
        assert_eq!(
            decode(libc::SYS_openat),
            Some(Sc::Open {
                dir: Dir::Arg(0),
                path: 1,
                flags: 2,
                flags_indirect: false
            })
        );
    }

    #[test]
    fn a_syscall_is_never_both_modelled_and_irrelevant() {
        // Overlap would mean the backend's fall-through order silently decided
        // whether a dependency was recorded.
        for nr in 0..512i64 {
            assert!(
                !(decode(nr).is_some() && is_irrelevant(nr)),
                "syscall {nr} is classified twice"
            );
        }
    }

    #[test]
    fn an_unrecognised_syscall_is_neither_modelled_nor_dismissed() {
        // Far beyond any allocated number, standing in for a syscall a future
        // kernel adds. It must fall through to the downgrade path.
        assert!(decode(9_999).is_none());
        assert!(!is_irrelevant(9_999));
    }

    #[test]
    fn descriptor_io_is_dismissed_but_mapping_is_not() {
        assert!(is_irrelevant(libc::SYS_read));
        assert!(decode(libc::SYS_mmap).is_some());
    }

    #[test]
    fn calls_that_can_hide_file_access_are_not_dismissed() {
        // io_uring can perform arbitrary file I/O without any further syscall,
        // so it must reach the downgrade path rather than being waved through.
        for nr in [libc::SYS_io_uring_setup, libc::SYS_io_uring_enter] {
            assert!(!is_irrelevant(nr));
            assert!(decode(nr).is_none());
        }
    }
}
