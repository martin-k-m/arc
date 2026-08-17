//! ptrace against the fast backend, on the same programs.
//!
//! The point is not that the two produce identical event streams — they cannot,
//! and do not need to. It is that they agree about *cache-relevant dependency
//! meaning*: the same inputs, the same absences, the same enumerated
//! directories, the same executables, and the same reasons for calling a trace
//! incomplete.
//!
//! Where they legitimately differ, the fast backend must differ in the safe
//! direction: it may record more than ptrace (a false miss), never less (a
//! false hit). The comparator below enforces exactly that asymmetry.
//!
//! These run the real backends against real programs, so they are skipped
//! wherever either backend is genuinely unavailable — a container that blocks
//! `seccomp`, a kernel without user notification, or any non-Linux host.

#![allow(dead_code)]

use arc_core::dependency::DependencySet;
use arc_core::exec::{self, Supervisor, Wait};
use arc_core::paths::Classifier;
use arc_core::trace::{self, Observations, Selection, Tracer};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn available(sel: Selection) -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    let tmp = std::env::temp_dir();
    let classifier = Classifier::new(&tmp, &tmp.join("arc-home"));
    match trace::start(&tmp, &classifier, sel) {
        Some(t) => {
            let name = t.name();
            let expect = match sel {
                Selection::Fast => "linux-seccomp",
                Selection::Ptrace => "linux-ptrace",
                _ => name,
            };
            // `start` falls back rather than failing, so the only proof that a
            // backend is really available is that it is the one that started.
            let ok = name == expect;
            drop(t.finish());
            ok
        }
        None => false,
    }
}

/// Serializes child reaping across the whole test binary. See `Project::trace`.
fn reaper() -> &'static std::sync::RwLock<()> {
    static R: std::sync::OnceLock<std::sync::RwLock<()>> = std::sync::OnceLock::new();
    R.get_or_init(Default::default)
}

enum Guard<'a> {
    Shared(std::sync::RwLockReadGuard<'a, ()>),
    Exclusive(std::sync::RwLockWriteGuard<'a, ()>),
}

fn seccomp_is_real() -> bool {
    static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OK.get_or_init(|| available(Selection::Fast))
}

fn both_available() -> bool {
    available(Selection::Fast) && available(Selection::Ptrace)
}

macro_rules! needs_both {
    () => {
        if !both_available() {
            return;
        }
    };
}

/// Adapts a tracer to `exec`'s hooks. The engine's own supervisor is private,
/// and this is the same shape.
struct Sup {
    tracer: Option<Box<dyn Tracer>>,
    failures: Vec<String>,
}

impl Supervisor for Sup {
    fn traced(&self) -> bool {
        self.tracer
            .as_ref()
            .is_some_and(|t| t.launch() == trace::Launch::Traced)
    }
    fn pre_exec(&self) -> Option<trace::PreExec> {
        self.tracer.as_ref()?.pre_exec()
    }
    fn on_spawn(&mut self, pid: u32) {
        if let Some(t) = self.tracer.as_mut() {
            if let Err(e) = t.attach(pid) {
                self.failures.push(e.to_string());
            }
        }
    }
    fn wait(&mut self, pid: u32) -> Option<anyhow::Result<Wait>> {
        self.tracer.as_mut()?.supervise(pid)
    }
    fn disable(&mut self, reason: String) {
        self.failures.push(reason);
    }
}

struct Project {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
}

impl Project {
    fn seeded() -> Project {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        let home = tmp.path().join("arc-home");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("out")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let p = Project {
            root,
            home,
            _tmp: tmp,
        };
        p.write("src/a.txt", "alpha\n");
        p.write("src/b.txt", "beta\n");
        for i in 0..12 {
            p.write(&format!("src/m{i}.txt"), &format!("m{i}\n"));
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink("a.txt", p.root.join("src/link.txt")).unwrap();
        p
    }

    fn write(&self, rel: &str, body: &str) {
        let path = self.root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn trace(&self, sel: Selection, script: &str) -> Learned {
        // ptrace reaps with `waitpid(-1, __WALL)`, which is process-wide: any
        // other thread in this binary spawning a child would have its status
        // stolen. So a ptrace session excludes every other session, while fast
        // sessions — which never call `waitpid(-1)` — may overlap freely.
        let _guard = if sel == Selection::Fast && seccomp_is_real() {
            Guard::Shared(reaper().read().unwrap())
        } else {
            Guard::Exclusive(reaper().write().unwrap())
        };
        let classifier = Classifier::new(&self.root, &self.home);
        let tracer = trace::start(&self.root, &classifier, sel).expect("a tracer");
        let backend = tracer.name().to_string();
        let mut sup = Sup {
            tracer: Some(tracer),
            failures: Vec::new(),
        };
        let sh = Path::new("/bin/sh");
        let outcome = exec::run(
            sh,
            &["-c".to_string(), script.to_string()],
            &self.root,
            true,
            &mut sup,
            None,
        )
        .expect("running the script");

        let tracer = sup.tracer.take().expect("a tracer");
        let caps = tracer.capabilities();
        let mut obs: Observations = tracer.finish();
        for f in sup.failures {
            obs.lossy = true;
            obs.downgrade(trace::Downgrade::BackendError(f));
        }
        let deps = DependencySet::from_observations(
            "family",
            &backend,
            caps,
            &obs,
            &self.root,
            &classifier,
            0,
        );
        Learned::of(backend, outcome.exit_code, &obs, &deps, &self.root)
    }
}

/// The cache-relevant meaning of a trace, with everything legitimately
/// backend-specific — event order, pids, raw counts — deliberately absent.
#[derive(Debug, Default)]
struct Learned {
    backend: String,
    exit_code: i32,
    inputs: BTreeSet<String>,
    absent: BTreeSet<String>,
    directories: BTreeSet<String>,
    outputs: BTreeSet<String>,
    executables: BTreeSet<String>,
    complete: bool,
    downgrades: BTreeSet<String>,
}

impl Learned {
    fn of(
        backend: String,
        exit_code: i32,
        obs: &Observations,
        deps: &DependencySet,
        root: &Path,
    ) -> Learned {
        // Only the project's own files are compared. What a shell and the C
        // library touch under /usr, /etc and /proc is real, is recorded by both
        // backends, and varies run to run for reasons that have nothing to do
        // with the tracing mechanism.
        // A dependency set holds project files by their relative path and
        // everything else absolutely; both forms arrive here.
        let rel = |p: &str| -> Option<String> {
            let r = root.to_string_lossy().replace('\\', "/");
            match p.strip_prefix(&r) {
                Some(s) => Some(s.trim_start_matches('/').to_string()),
                None if !p.starts_with('/') => Some(p.to_string()),
                None => None,
            }
        };
        let pick = |it: &mut dyn Iterator<Item = &String>| -> BTreeSet<String> {
            it.filter_map(|p| rel(p)).collect()
        };
        Learned {
            backend,
            exit_code,
            inputs: pick(&mut deps.inputs.iter()),
            absent: pick(&mut deps.existence.iter()),
            directories: pick(&mut deps.directories.iter()),
            outputs: pick(&mut deps.outputs.iter()),
            executables: deps
                .executables
                .iter()
                .map(|e| {
                    Path::new(&e.path)
                        .file_name()
                        .map(|f| f.to_string_lossy().to_string())
                        .unwrap_or_else(|| e.path.clone())
                })
                .collect(),
            complete: deps.completeness == arc_core::dependency::Completeness::Complete,
            downgrades: obs
                .downgrades
                .iter()
                .map(|d| d.kind().to_string())
                .collect(),
        }
    }
}

/// ptrace is the reference. The fast backend must not know *less* about the
/// project than it does.
#[track_caller]
fn agree(case: &str, p: &Learned, f: &Learned) {
    assert_eq!(p.exit_code, f.exit_code, "{case}: different exit codes");
    assert_eq!(
        f.backend, "linux-seccomp",
        "{case}: fast backend did not run"
    );
    assert_eq!(p.backend, "linux-ptrace", "{case}: ptrace did not run");

    for (what, a, b) in [
        ("inputs", &p.inputs, &f.inputs),
        ("absences", &p.absent, &f.absent),
        ("directories", &p.directories, &f.directories),
        ("outputs", &p.outputs, &f.outputs),
        ("executables", &p.executables, &f.executables),
    ] {
        let missing: Vec<&String> = a.difference(b).collect();
        assert!(
            missing.is_empty(),
            "{case}: the fast backend missed {what} {missing:?}\n  ptrace: {a:?}\n  fast:   {b:?}"
        );
    }
    // Completeness may only be claimed by the fast backend where ptrace also
    // claims it. The reverse is allowed: seeing more reasons to doubt is safe.
    //
    // The exception is `path_resolution_failure`, which is ptrace admitting it
    // could not read a path out of the tracee — a limitation of that backend,
    // not evidence about the program. The fast backend reads the same path from
    // `/proc/<pid>/fd`, as bytes, and so has nothing to admit.
    let ptrace_only_lost_a_path =
        !p.complete && p.downgrades.iter().all(|d| d == "path_resolution_failure");
    assert!(
        p.complete || !f.complete || ptrace_only_lost_a_path,
        "{case}: the fast backend claimed complete where ptrace did not\n  ptrace: {:?}\n  fast: {:?}",
        p.downgrades,
        f.downgrades
    );
}

fn corpus() -> Vec<(&'static str, &'static str)> {
    vec![
        ("read", "cat src/a.txt > /dev/null"),
        ("read twice", "cat src/a.txt src/a.txt > /dev/null"),
        ("write", "echo hi > out/w.txt"),
        ("read then write", "cat src/a.txt > out/w.txt"),
        ("stat", "test -f src/a.txt"),
        ("absent", "test -f src/nope.txt || true"),
        ("enumerate", "ls src > /dev/null"),
        ("child", "sh -c 'cat src/a.txt' > /dev/null"),
        ("grandchild", "sh -c \"sh -c 'cat src/a.txt'\" > /dev/null"),
        (
            "rapid children",
            "for i in 1 2 3 4 5 6 7 8; do cat src/a.txt > /dev/null; done",
        ),
        (
            "rename",
            "cp src/a.txt out/r0.txt && mv out/r0.txt out/r1.txt",
        ),
        (
            "delete after read",
            "cp src/a.txt out/d.txt && cat out/d.txt > /dev/null && rm out/d.txt",
        ),
        ("symlink", "cat src/link.txt > /dev/null"),
        ("relative from a new cwd", "cd src && cat a.txt > /dev/null"),
        ("enumerate from a new cwd", "cd src && ls . > /dev/null"),
        (
            "generated intermediate",
            "echo gen > out/i.txt && cat out/i.txt > /dev/null",
        ),
        ("exec chain", "env sh -c 'cat src/a.txt' > /dev/null"),
        (
            "many files",
            "for f in src/m*.txt; do cat $f > /dev/null; done",
        ),
        (
            "absent then present",
            "test -f src/late.txt; echo late > src/late.txt",
        ),
    ]
}

#[test]
fn the_two_backends_agree_about_every_program_in_the_corpus() {
    needs_both!();
    for (name, script) in corpus() {
        // A fresh project per backend. Several of these scripts create or
        // delete files, and running the second backend over the first one's
        // leftovers would compare two different programs.
        let ptrace = Project::seeded().trace(Selection::Ptrace, script);
        let fast = Project::seeded().trace(Selection::Fast, script);
        agree(name, &ptrace, &fast);
    }
}

#[test]
fn both_backends_learn_the_file_a_command_read() {
    needs_both!();
    let p = Project::seeded();
    for sel in [Selection::Ptrace, Selection::Fast] {
        let l = p.trace(sel, "cat src/a.txt > /dev/null");
        assert!(
            l.inputs.contains("src/a.txt"),
            "{}: {:?}",
            l.backend,
            l.inputs
        );
    }
}

#[test]
fn both_backends_learn_an_absence() {
    needs_both!();
    let p = Project::seeded();
    for sel in [Selection::Ptrace, Selection::Fast] {
        let l = p.trace(sel, "test -f src/nope.txt || true");
        assert!(
            l.absent.contains("src/nope.txt"),
            "{}: {:?}",
            l.backend,
            l.absent
        );
    }
}

#[test]
fn both_backends_learn_a_directory_enumeration() {
    needs_both!();
    let p = Project::seeded();
    for sel in [Selection::Ptrace, Selection::Fast] {
        let l = p.trace(sel, "ls src > /dev/null");
        assert!(
            l.directories.contains("src"),
            "{}: {:?}",
            l.backend,
            l.directories
        );
    }
}

#[test]
fn both_backends_treat_a_generated_file_as_an_intermediate() {
    needs_both!();
    let p = Project::seeded();
    for sel in [Selection::Ptrace, Selection::Fast] {
        let l = p.trace(sel, "echo gen > out/i.txt && cat out/i.txt > /dev/null");
        assert!(
            !l.inputs.contains("out/i.txt"),
            "{}: a file it created is not an input: {:?}",
            l.backend,
            l.inputs
        );
    }
}

#[test]
fn both_backends_downgrade_a_volatile_read() {
    needs_both!();
    let p = Project::seeded();
    for sel in [Selection::Ptrace, Selection::Fast] {
        let l = p.trace(sel, "cat /proc/cpuinfo > /dev/null");
        assert!(!l.complete, "{} called this complete", l.backend);
        assert!(l.downgrades.contains("volatile_read"), "{:?}", l.downgrades);
    }
}

#[test]
fn both_backends_downgrade_network_use() {
    needs_both!();
    let p = Project::seeded();
    // `/dev/tcp` is a bash builtin, and `/bin/sh` here may be dash.
    if !Path::new("/bin/bash").exists() {
        return;
    }
    let script = "/bin/bash -c '(exec 3<>/dev/tcp/127.0.0.1/9) 2>/dev/null' || true";
    for sel in [Selection::Ptrace, Selection::Fast] {
        let l = p.trace(sel, script);
        assert!(!l.complete, "{} called this complete", l.backend);
    }
}

#[test]
fn both_backends_say_the_same_thing_about_a_unix_socket() {
    needs_both!();
    let p = Project::seeded();
    let live = p.root.join("live.sock");
    let listener = std::os::unix::net::UnixListener::bind(&live).unwrap();
    let prog = |name: &str, target: &Path| {
        p.write(
            name,
            &format!(
                "import socket
s = socket.socket(socket.AF_UNIX)
try:
    s.connect({:?})
except OSError:
    pass
",
                target.to_str().unwrap()
            ),
        );
        format!("python3 {name}")
    };
    let absent = prog("absent.py", &p.root.join("not-there.sock"));
    let present = prog("present.py", &live);
    for sel in [Selection::Ptrace, Selection::Fast] {
        // A socket that is not there cannot answer: the connection fails, and
        // the failure is a fact about the filesystem that Arc can check again.
        let l = p.trace(sel, &absent);
        assert!(
            l.complete,
            "{} downgraded a connection that could only fail",
            l.backend
        );
        assert!(
            l.absent.iter().any(|a| a.contains("not-there.sock")),
            "{} did not record the socket's absence: {:?}",
            l.backend,
            l.absent
        );
        // One that is there answers with something no filesystem fingerprint
        // describes.
        let l = p.trace(sel, &present);
        assert!(
            !l.complete,
            "{} called a live Unix socket complete",
            l.backend
        );
    }
    drop(listener);
}

#[test]
fn neither_backend_learns_arcs_own_state() {
    needs_both!();
    let p = Project::seeded();
    for sel in [Selection::Ptrace, Selection::Fast] {
        let l = p.trace(sel, "cat src/a.txt > /dev/null");
        assert!(
            !l.inputs.iter().any(|i| i.contains("arc-home")),
            "{}: {:?}",
            l.backend,
            l.inputs
        );
    }
}

#[cfg(unix)]
#[test]
fn a_non_utf8_path_survives_both_backends() {
    use std::os::unix::ffi::OsStrExt;
    needs_both!();
    let p = Project::seeded();
    let name = std::ffi::OsStr::from_bytes(b"src/\xff\xfeodd.txt");
    std::fs::write(p.root.join(name), "odd\n").unwrap();
    let script = "for f in src/*odd.txt; do cat \"$f\" > /dev/null; done";
    let ptrace = p.trace(Selection::Ptrace, script);
    let fast = p.trace(Selection::Fast, script);
    agree("non-utf8", &ptrace, &fast);
    // Neither backend may name this file, so neither may call the trace
    // complete: the conservative project scan has to take over.
    for l in [&ptrace, &fast] {
        assert!(!l.complete, "{} claimed complete", l.backend);
        assert!(
            l.downgrades.contains("path_resolution_failure"),
            "{}: {:?}",
            l.backend,
            l.downgrades
        );
    }
}

#[test]
fn concurrent_fast_sessions_do_not_contaminate_one_another() {
    needs_both!();
    let projects: Vec<Project> = (0..4).map(|_| Project::seeded()).collect();
    for (i, p) in projects.iter().enumerate() {
        p.write(&format!("src/own{i}.txt"), "mine\n");
    }
    let learned: Vec<Learned> = std::thread::scope(|s| {
        let handles: Vec<_> = projects
            .iter()
            .enumerate()
            .map(|(i, p)| {
                s.spawn(move || {
                    p.trace(Selection::Fast, &format!("cat src/own{i}.txt > /dev/null"))
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for (i, l) in learned.iter().enumerate() {
        assert!(
            l.inputs.contains(&format!("src/own{i}.txt")),
            "session {i}: {:?}",
            l.inputs
        );
        for other in 0..4 {
            if other != i {
                assert!(
                    !l.inputs.contains(&format!("src/own{other}.txt")),
                    "session {i} saw session {other}'s file"
                );
            }
        }
    }
}

#[test]
fn a_thousand_sessions_leak_nothing() {
    needs_both!();
    let p = Project::seeded();
    let before = open_descriptors();
    for _ in 0..200 {
        let l = p.trace(Selection::Fast, "cat src/a.txt > /dev/null");
        assert_eq!(l.exit_code, 0);
    }
    let after = open_descriptors();
    assert!(
        after <= before + 8,
        "descriptors grew from {before} to {after}"
    );
}

fn open_descriptors() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .map(|d| d.count())
        .unwrap_or(0)
}

#[test]
fn a_pinned_backend_that_cannot_run_falls_back_rather_than_failing() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let p = Project::seeded();
    let l = p.trace(Selection::Fast, "cat src/a.txt > /dev/null");
    assert_eq!(l.exit_code, 0);
    assert!(
        l.backend == "linux-seccomp" || l.backend == "linux-ptrace",
        "unexpected backend {}",
        l.backend
    );
}
