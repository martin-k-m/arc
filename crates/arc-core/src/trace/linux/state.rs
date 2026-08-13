//! Per-process state needed to turn syscall arguments into paths.
//!
//! A syscall argument is rarely a path. `openat(dirfd, "b.txt", …)` names a file
//! only in combination with that process's working directory or with whatever
//! `dirfd` refers to, and both can be changed by the process or inherited
//! across a fork. Resolving against Arc's own working directory instead — the
//! obvious shortcut — silently produces the wrong dependency.
//!
//! Sharing follows the kernel's own rules: `CLONE_FS` shares the working
//! directory, `CLONE_FILES` shares the descriptor table. Threads therefore share
//! both and processes share neither, which falls out of the flags rather than
//! being guessed from whether something "looks like" a thread.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// What a descriptor refers to. Only enough to resolve `*at` syscalls, classify
/// mappings, and notice network use.
#[derive(Debug, Clone)]
pub enum Fd {
    Path(Rc<PathBuf>),
    Socket {
        family: u16,
    },
    /// A pipe, an eventfd, an anonymous mapping: nothing that can name a file.
    Anonymous,
}

#[derive(Debug, Default)]
pub struct Fds(HashMap<i32, Fd>);

impl Fds {
    pub fn insert(&mut self, fd: i32, entry: Fd) {
        if fd >= 0 {
            self.0.insert(fd, entry);
        }
    }
    pub fn remove(&mut self, fd: i32) {
        self.0.remove(&fd);
    }
    pub fn get(&self, fd: i32) -> Option<&Fd> {
        self.0.get(&fd)
    }
    pub fn path(&self, fd: i32) -> Option<Rc<PathBuf>> {
        match self.0.get(&fd) {
            Some(Fd::Path(p)) => Some(p.clone()),
            _ => None,
        }
    }
    pub fn dup(&mut self, from: i32, to: i32) {
        if let Some(e) = self.0.get(&from).cloned() {
            self.insert(to, e);
        }
    }
}

/// One syscall in flight, remembered at the entry stop so the exit stop can be
/// interpreted.
///
/// The exit stop is where success is known — whether an open produced a
/// descriptor, whether a `stat` said ENOENT — but by then the arguments are
/// gone on some architectures, so they are captured here.
#[derive(Debug, Clone, Copy)]
pub struct Pending {
    pub nr: u64,
    pub args: [u64; 6],
}

pub struct Proc {
    /// Shared with threads created via `CLONE_FS`.
    pub cwd: Rc<RefCell<PathBuf>>,
    /// Shared with threads created via `CLONE_FILES`.
    pub fds: Rc<RefCell<Fds>>,
    pub pending: Option<Pending>,
    /// Whether the path of a creating open already existed, established at the
    /// entry stop because by the exit stop the answer has changed.
    pub existed: Option<bool>,
    /// The image of an `execve` in flight. A successful `execve` never returns,
    /// so it has no exit stop; the `PTRACE_EVENT_EXEC` stop collects this
    /// instead.
    pub pending_exec: Option<PathBuf>,
    /// Clone flags captured at the entry to `clone`/`clone3`, so the child's
    /// sharing can be decided when it appears.
    pub clone_flags: u64,
    /// True once options have been applied; a freshly reported child is stopped
    /// before that.
    pub configured: bool,
}

impl Proc {
    pub fn root(cwd: &Path) -> Proc {
        Proc {
            cwd: Rc::new(RefCell::new(cwd.to_path_buf())),
            fds: Rc::new(RefCell::new(Fds::default())),
            pending: None,
            existed: None,
            pending_exec: None,
            clone_flags: 0,
            configured: false,
        }
    }

    /// A child of this process, sharing exactly what `flags` says it shares.
    pub fn child(&self, flags: u64) -> Proc {
        let cwd = if flags & libc::CLONE_FS as u64 != 0 {
            self.cwd.clone()
        } else {
            Rc::new(RefCell::new(self.cwd.borrow().clone()))
        };
        let fds = if flags & libc::CLONE_FILES as u64 != 0 {
            self.fds.clone()
        } else {
            // A fork copies the table; later opens in either process are
            // independent.
            let copy = Fds(self.fds.borrow().0.clone());
            Rc::new(RefCell::new(copy))
        };
        Proc {
            cwd,
            fds,
            pending: None,
            existed: None,
            pending_exec: None,
            clone_flags: 0,
            configured: false,
        }
    }

    pub fn cwd(&self) -> PathBuf {
        self.cwd.borrow().clone()
    }

    pub fn set_cwd(&self, p: PathBuf) {
        *self.cwd.borrow_mut() = p;
    }
}

/// Where a relative path in a syscall is resolved from.
pub enum Base {
    Cwd,
    Dir(Rc<PathBuf>),
    /// The descriptor was not a directory Arc knows about, so nothing can be
    /// resolved. The caller records a path-resolution failure rather than
    /// guessing.
    Unknown,
}

impl Proc {
    /// Resolve the `(dirfd, path)` pair every `*at` syscall takes.
    pub fn base_for(&self, dirfd: i32) -> Base {
        if dirfd == libc::AT_FDCWD {
            return Base::Cwd;
        }
        match self.fds.borrow().path(dirfd) {
            Some(p) => Base::Dir(p),
            None => Base::Unknown,
        }
    }

    /// Join a syscall path argument onto its base, without touching the
    /// filesystem: the file may already have been deleted, and a dependency
    /// that no longer exists is still a dependency.
    pub fn resolve(&self, base: &Base, path: &Path) -> Option<PathBuf> {
        if path.is_absolute() {
            return Some(PathBuf::from(crate::paths::display_form(path)));
        }
        let root = match base {
            Base::Cwd => self.cwd(),
            Base::Dir(p) => (**p).clone(),
            Base::Unknown => return None,
        };
        Some(PathBuf::from(crate::paths::display_form(&root.join(path))))
    }
}

/// Every process currently under observation.
#[derive(Default)]
pub struct Table {
    procs: HashMap<i32, Proc>,
}

impl Table {
    pub fn insert(&mut self, pid: i32, p: Proc) {
        self.procs.insert(pid, p);
    }
    pub fn get(&self, pid: i32) -> Option<&Proc> {
        self.procs.get(&pid)
    }
    pub fn get_mut(&mut self, pid: i32) -> Option<&mut Proc> {
        self.procs.get_mut(&pid)
    }
    pub fn remove(&mut self, pid: i32) {
        self.procs.remove(&pid);
    }
    pub fn is_empty(&self) -> bool {
        self.procs.is_empty()
    }
    pub fn pids(&self) -> Vec<i32> {
        self.procs.keys().copied().collect()
    }
}

/// The executable a process is actually running, after the kernel has resolved
/// a shebang line. For `#!/usr/bin/env python3` this is the Python binary, not
/// the script — which is why the script itself is recorded separately, as a
/// file the execution read.
pub fn exe_of(pid: i32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .filter(|p| p.is_absolute())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_resolve_against_the_process_working_directory() {
        let p = Proc::root(Path::new("/repo/sub"));
        let base = p.base_for(libc::AT_FDCWD);
        assert_eq!(
            p.resolve(&base, Path::new("../a.txt")).unwrap(),
            PathBuf::from("/repo/a.txt")
        );
        assert_eq!(
            p.resolve(&base, Path::new("/etc/hosts")).unwrap(),
            PathBuf::from("/etc/hosts")
        );
    }

    #[test]
    fn an_unknown_directory_descriptor_resolves_to_nothing() {
        let p = Proc::root(Path::new("/repo"));
        assert!(p.resolve(&p.base_for(9), Path::new("a.txt")).is_none());
        p.fds
            .borrow_mut()
            .insert(9, Fd::Path(Rc::new(PathBuf::from("/repo/plugins"))));
        assert_eq!(
            p.resolve(&p.base_for(9), Path::new("a.txt")).unwrap(),
            PathBuf::from("/repo/plugins/a.txt")
        );
    }

    #[test]
    fn threads_share_state_and_forks_do_not() {
        let parent = Proc::root(Path::new("/repo"));
        let thread = parent.child((libc::CLONE_FS | libc::CLONE_FILES) as u64);
        let forked = parent.child(0);
        thread.set_cwd(PathBuf::from("/repo/x"));
        assert_eq!(parent.cwd(), PathBuf::from("/repo/x"));
        assert_eq!(forked.cwd(), PathBuf::from("/repo"));

        thread
            .fds
            .borrow_mut()
            .insert(3, Fd::Path(Rc::new(PathBuf::from("/repo/a"))));
        assert!(parent.fds.borrow().path(3).is_some());
        assert!(forked.fds.borrow().path(3).is_none());
    }
}
