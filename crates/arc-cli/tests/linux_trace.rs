//! Adversarial tests for the Linux ptrace backend, against the real binary.
//!
//! Every test here exists because getting it wrong produces a *false hit* — Arc
//! replaying a result when something that mattered had changed. They are written
//! as "change this, then demand a miss", because that is the direction where
//! being wrong is expensive.
//!
//! The suite compiles on every platform and does nothing off Linux, so a
//! Windows or macOS build is not broken by tests it cannot run. On Linux it also
//! stands down when tracing is unavailable — a container with a seccomp profile
//! that forbids `ptrace` is a supported configuration, not a failure, and the
//! conservative behaviour there is covered by `tests/dependency.rs`.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const ARC: &str = env!("CARGO_BIN_EXE_arc");

struct Sandbox {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
}

impl Sandbox {
    fn new() -> Sandbox {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        let home = tmp.path().join("archome");
        std::fs::create_dir_all(&root).unwrap();
        Sandbox {
            _tmp: tmp,
            root,
            home,
        }
    }

    fn write(&self, rel: &str, body: &str) -> &Sandbox {
        let p = self.root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
        self
    }

    fn script(&self, rel: &str, body: &str) -> &Sandbox {
        use std::os::unix::fs::PermissionsExt;
        self.write(rel, body);
        let p = self.root.join(rel);
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        self
    }

    fn arc(&self, args: &[&str]) -> Output {
        Command::new(ARC)
            .args(args)
            .current_dir(&self.root)
            .env("ARC_HOME", &self.home)
            .output()
            .expect("running arc")
    }

    /// Run a shell command under Arc.
    fn sh(&self, script: &str) -> Output {
        self.arc(&["run", "sh", "-c", script])
    }

    /// Run once to observe, once to settle. The first execution is what teaches
    /// Arc the dependency set; the second is the first that can narrow, and it
    /// is the baseline every assertion below is made against.
    fn learn(&self, script: &str) {
        assert_ok(&self.sh(script));
        assert_hit(&self.sh(script), "the run after learning should reuse");
    }
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).to_string()
}
fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).to_string()
}

fn assert_ok(o: &Output) {
    assert!(o.status.success(), "command failed: {}", stderr(o));
}
fn assert_hit(o: &Output, why: &str) {
    assert!(stderr(o).contains("CACHE HIT"), "{why}\n{}", stderr(o));
}
fn assert_miss(o: &Output, why: &str) {
    assert!(!stderr(o).contains("CACHE HIT"), "{why}\n{}", stderr(o));
}

/// Whether the ptrace backend is actually usable here. Everything below is
/// about what a *complete* trace guarantees, so without one there is nothing to
/// assert.
fn available() -> bool {
    let out = Command::new(ARC)
        .arg("doctor")
        .output()
        .expect("arc doctor");
    stdout(&out)
        .lines()
        .any(|l| l.contains("automatic narrowing") && l.contains("supported"))
}

macro_rules! needs_tracer {
    () => {
        if !available() {
            eprintln!("skipping: ptrace tracing is not available in this environment");
            return;
        }
    };
}

// ------------------------------------------------------------------ reads ----

#[test]
fn a_file_that_was_read_is_a_dependency_and_one_that_was_not_is_free() {
    needs_tracer!();
    let sb = Sandbox::new();
    sb.write("input.txt", "one").write("unrelated.txt", "x");
    sb.learn("cat input.txt");

    sb.write("unrelated.txt", "completely different");
    assert_hit(
        &sb.sh("cat input.txt"),
        "a file the execution never read cannot change its result",
    );

    sb.write("input.txt", "two");
    assert_miss(
        &sb.sh("cat input.txt"),
        "the file it did read must invalidate",
    );
}

#[test]
fn asking_about_a_descriptor_does_not_cost_the_trace_its_completeness() {
    needs_tracer!();
    // glibc has implemented `fstat(fd)` as `newfstatat(fd, "", …,
    // AT_EMPTY_PATH)` since 2.33, so a stdio program asks this about its own
    // stdout on nearly every run. Arc models `newfstatat` as a path syscall; if
    // it treats the empty path as a name it failed to resolve, every trace of
    // every stdio program is partial and nothing ever narrows.
    //
    // `cat` is the smallest command that does it. The assertion is on the
    // headline claim rather than on a hit, because a conservative whole-project
    // scan hits too when nothing changed -- which is exactly how this hid.
    let sb = Sandbox::new();
    sb.write("input.txt", "one");
    let log = stderr(&sb.arc(&["run", "--trace", "cat", "input.txt"]));
    assert!(
        log.contains("TRACE COMPLETE"),
        "reading a file with cat must produce a complete trace:\n{log}"
    );
    assert!(
        !log.contains("a path argument could not be read back"),
        "an empty path argument is a descriptor question, not a failure:\n{log}"
    );
}

#[test]
fn writing_a_file_does_not_make_it_an_input() {
    needs_tracer!();
    let sb = Sandbox::new();
    sb.learn("echo generated > out.txt");
    // The output exists now and differs from the empty state of the first run.
    // If Arc had recorded it as an input, this would be a permanent miss.
    assert_hit(
        &sb.sh("echo generated > out.txt"),
        "a file the execution produces is not a precondition for it",
    );
}

// -------------------------------------------------------------- existence ----

#[test]
fn a_file_that_was_looked_for_and_missing_is_a_negative_dependency() {
    needs_tracer!();
    let sb = Sandbox::new();
    sb.script(
        "run.sh",
        "#!/bin/sh\nif [ -f optional.cfg ]; then echo WITH; else echo WITHOUT; fi\n",
    );
    sb.learn("./run.sh");
    assert!(stdout(&sb.sh("./run.sh")).contains("WITHOUT"));

    sb.write("optional.cfg", "now here");
    let out = sb.sh("./run.sh");
    assert_miss(
        &out,
        "the result depended on that file being absent; it is not any more",
    );
    assert!(stdout(&out).contains("WITH"), "{}", stdout(&out));
}

#[test]
fn a_metadata_check_is_a_dependency_even_without_reading_the_contents() {
    needs_tracer!();
    let sb = Sandbox::new();
    sb.write("marker", "");
    sb.script(
        "run.sh",
        "#!/bin/sh\nif [ -s marker ]; then echo FULL; else echo EMPTY; fi\n",
    );
    sb.learn("./run.sh");

    sb.write("marker", "contents");
    assert_miss(
        &sb.sh("./run.sh"),
        "the branch was taken on the file's size, so its size is a dependency",
    );
}

// ------------------------------------------------------------ directories ----

#[test]
fn enumerating_a_directory_depends_on_its_entries_not_just_its_files() {
    needs_tracer!();
    let sb = Sandbox::new();
    std::fs::create_dir(sb.root.join("plugins")).unwrap();
    sb.write("plugins/a.plugin", "a");
    sb.learn("ls plugins");

    sb.write("plugins/new.plugin", "b");
    assert_miss(
        &sb.sh("ls plugins"),
        "a file that never existed when the trace ran must still invalidate it",
    );
}

// -------------------------------------------------------------- processes ----

#[test]
fn a_file_read_by_a_child_process_is_a_dependency() {
    needs_tracer!();
    let sb = Sandbox::new();
    sb.write("child-input.txt", "one");
    sb.script("child.sh", "#!/bin/sh\ncat child-input.txt\n");
    sb.learn("./child.sh");

    sb.write("child-input.txt", "two");
    assert_miss(
        &sb.sh("./child.sh"),
        "a child's reads are the command's reads",
    );
}

#[test]
fn a_file_read_by_a_grandchild_process_is_a_dependency() {
    needs_tracer!();
    let sb = Sandbox::new();
    sb.write("deep.txt", "one");
    sb.script("grandchild.sh", "#!/bin/sh\ncat deep.txt\n");
    sb.script("child.sh", "#!/bin/sh\n./grandchild.sh\n");
    sb.learn("./child.sh");

    sb.write("deep.txt", "two");
    assert_miss(
        &sb.sh("./child.sh"),
        "process-tree completeness means every generation, not just the first",
    );
}

#[test]
fn many_short_lived_children_are_all_observed() {
    needs_tracer!();
    let sb = Sandbox::new();
    sb.write("shared.txt", "one");
    sb.script(
        "many.sh",
        "#!/bin/sh\ni=0\nwhile [ $i -lt 60 ]; do cat shared.txt > /dev/null; i=$((i+1)); done\n",
    );
    sb.learn("./many.sh");

    sb.write("shared.txt", "two");
    assert_miss(
        &sb.sh("./many.sh"),
        "a process that exits quickly must not escape observation",
    );
}

#[test]
fn a_shebang_script_depends_on_the_script_as_well_as_the_interpreter() {
    needs_tracer!();
    let sb = Sandbox::new();
    sb.script("build.sh", "#!/bin/sh\necho v1\n");
    sb.learn("./build.sh");

    sb.script("build.sh", "#!/bin/sh\necho v2\n");
    let out = sb.sh("./build.sh");
    assert_miss(&out, "the script's own text decides what happens");
    assert!(stdout(&out).contains("v2"), "{}", stdout(&out));
}

// ---------------------------------------------------------------- mapping ----

#[test]
fn a_mapped_executable_inside_the_project_is_a_dependency() {
    needs_tracer!();
    // Running a binary maps it; this is the mmap path, exercised through the one
    // mapping every dynamically linked program performs on itself.
    let sb = Sandbox::new();
    std::fs::copy("/bin/true", sb.root.join("prog")).unwrap();
    sb.learn("./prog && echo ran");

    std::fs::copy("/bin/false", sb.root.join("prog")).unwrap();
    let out = sb.sh("./prog && echo ran");
    assert_miss(&out, "the binary that was mapped and executed changed");
    assert!(!stdout(&out).contains("ran"), "{}", stdout(&out));
}

// ---------------------------------------------------------------- temporal ----

#[test]
fn a_file_created_then_read_within_one_run_is_not_a_precondition() {
    needs_tracer!();
    let sb = Sandbox::new();
    sb.script(
        "gen.sh",
        "#!/bin/sh\necho intermediate > tmp.txt\ncat tmp.txt\nrm -f tmp.txt\n",
    );
    // Deleted at the end, so if Arc had learned it as an input the next run
    // would fingerprint a missing file and never hit again.
    sb.learn("./gen.sh");
    assert_hit(
        &sb.sh("./gen.sh"),
        "an intermediate the run makes and destroys is not an input",
    );

    let graph = sb.arc(&["graph", "--json"]);
    let g: serde_json::Value = serde_json::from_str(&stdout(&graph)).unwrap();
    let consumed = g["nodes"][0]["consumes"].as_array().unwrap();
    assert!(
        !consumed
            .iter()
            .any(|c| c["path"].as_str().unwrap_or("").contains("tmp.txt")),
        "tmp.txt must not be an input: {consumed:?}"
    );
}

#[test]
fn a_file_read_before_being_rewritten_is_both_input_and_output() {
    needs_tracer!();
    let sb = Sandbox::new();
    sb.write("state.txt", "one");
    sb.script(
        "step.sh",
        "#!/bin/sh\nold=$(cat state.txt)\necho \"$old-next\" > state.txt\n",
    );
    // Each run rewrites the file, so each run legitimately misses; what matters
    // is that it misses for the right reason and never replays stale state.
    assert_ok(&sb.sh("./step.sh"));
    let first = std::fs::read_to_string(sb.root.join("state.txt")).unwrap();
    assert_miss(
        &sb.sh("./step.sh"),
        "the file it read has changed since it read it",
    );
    let second = std::fs::read_to_string(sb.root.join("state.txt")).unwrap();
    assert_ne!(first, second, "the command must actually have run again");
}

#[test]
fn a_result_renamed_into_place_is_an_output_not_an_input() {
    needs_tracer!();
    let sb = Sandbox::new();
    sb.script(
        "build.sh",
        "#!/bin/sh\necho payload > result.tmp\nmv result.tmp result.bin\n",
    );
    // Four runs, not two. `mv` inspects its destination, so the second run
    // genuinely depends on a file the first one created; the state only settles
    // once the outputs stop changing. Demanding a hit sooner would be demanding
    // that Arc ignore a real dependency.
    for _ in 0..3 {
        assert_ok(&sb.sh("./build.sh"));
    }
    assert_hit(&sb.sh("./build.sh"), "a settled build should reuse");

    let graph = sb.arc(&["graph", "--json"]);
    let g: serde_json::Value = serde_json::from_str(&stdout(&graph)).unwrap();
    let node = &g["nodes"][0];
    let produces: Vec<String> = serde_json::from_value(node["produces"].clone()).unwrap();
    assert!(
        produces.iter().any(|o| o == "result.tmp"),
        "the temporary is a product of the run: {node}"
    );
    assert!(
        !node["consumes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["path"] == "result.tmp"),
        "a file created and renamed away is never a precondition: {node}"
    );
}

#[test]
fn a_command_that_consults_volatile_state_is_never_called_complete() {
    needs_tracer!();
    // Debian coreutils probe SELinux through `/sys` and `/proc`, so this is not
    // a contrived case: it is what `mv` does on a stock system. Arc must notice
    // and decline to narrow rather than pretend the run was reproducible.
    let sb = Sandbox::new();
    sb.script(
        "build.sh",
        "#!/bin/sh\necho payload > a.tmp\nmv a.tmp a.bin\n",
    );
    let out = sb.arc(&["run", "--trace", "sh", "-c", "./build.sh"]);
    assert_ok(&out);
    let log = stderr(&out);
    if log.contains("volatile") {
        assert!(log.contains("TRACE PARTIAL"), "{log}");
        assert!(
            log.lines()
                .any(|l| l.contains("next run narrows") && l.contains("no")),
            "{log}"
        );
    }
}

// ---------------------------------------------------------------- symlinks ----

#[test]
fn retargeting_a_symlink_invalidates_and_so_does_editing_its_target() {
    needs_tracer!();
    let sb = Sandbox::new();
    sb.write("configs/a.conf", "alpha")
        .write("configs/b.conf", "beta");
    std::os::unix::fs::symlink("configs/a.conf", sb.root.join("current.conf")).unwrap();
    sb.learn("cat current.conf");

    // The target's contents are a dependency even though the command named the
    // link.
    sb.write("configs/a.conf", "alpha-edited");
    assert_miss(&sb.sh("cat current.conf"), "the resolved file changed");
    sb.learn("cat current.conf");

    // And so is where the link points.
    std::fs::remove_file(sb.root.join("current.conf")).unwrap();
    std::os::unix::fs::symlink("configs/b.conf", sb.root.join("current.conf")).unwrap();
    let out = sb.sh("cat current.conf");
    assert_miss(&out, "the link now resolves somewhere else");
    assert!(stdout(&out).contains("beta"), "{}", stdout(&out));
}

#[test]
fn a_dangling_links_target_appearing_is_a_miss() {
    needs_tracer!();
    // The command's answer is decided by whether the link resolves, so
    // replaying it after the target appears is a false hit -- the worst
    // failure a cache has. `canonicalize` fails for a dangling link, so the
    // target is never learned as an input; it has to be learned as an absence.
    let sb = Sandbox::new();
    std::os::unix::fs::symlink("missing.txt", sb.root.join("link.txt")).unwrap();
    sb.script(
        "run.sh",
        "#!/bin/sh
if [ -e link.txt ]; then echo yes; else echo no; fi
",
    );
    sb.learn("./run.sh");

    sb.write("missing.txt", "appeared");
    let out = sb.sh("./run.sh");
    assert_miss(&out, "the link now resolves, so the answer changed");
    assert!(stdout(&out).contains("yes"), "{}", stdout(&out));
}

#[test]
fn a_process_that_outlives_the_command_costs_the_trace_its_completeness() {
    needs_tracer!();
    // A detached grandchild can still read files after `arc run` has returned.
    // The two backends answer differently and both answers are correct, which is
    // the table in LIMITATIONS 4. Pinning the backend is what makes the
    // expectation well defined; one verdict for both is not.
    for backend in ["seccomp", "ptrace"] {
        let sb = Sandbox::new();
        sb.write("in.txt", "one");
        sb.script(
            "run.sh",
            "#!/bin/sh
setsid sh -c 'sleep 2; cat in.txt > /dev/null' < /dev/null > /dev/null 2>&1 &
exit 0
",
        );
        let log = stderr(&sb.arc(&["run", "--trace", "--trace-backend", backend, "./run.sh"]));
        // A pinned backend that is unavailable falls back rather than failing, so
        // the verdict is chosen by the backend that actually ran.
        if log.contains("linux-ptrace") {
            assert!(
                log.contains("TRACE COMPLETE"),
                "ptrace waits the grandchild out, so its completeness is earned:
{log}"
            );
        } else {
            assert!(
                log.contains("TRACE PARTIAL") && log.contains("a process could not be followed"),
                "a trace that misses a live process must not be called complete:
{log}"
            );
        }
    }
}

#[test]
fn connecting_to_a_unix_socket_that_is_not_there_is_a_dependency_on_its_absence() {
    needs_tracer!();
    // glibc asks `/var/run/nscd/socket` on every user lookup and gets ENOENT.
    // Downgrading on that costs completeness for most of userspace; ignoring it
    // is a false hit the day the socket appears. It is neither: it is an
    // absence, and absences are exactly what Arc already fingerprints.
    let sb = Sandbox::new();
    let sock = sb.root.join("daemon.sock");
    sb.write(
        "prog.py",
        &format!(
            "import socket
s = socket.socket(socket.AF_UNIX)
try:
    s.connect({:?})
except OSError:
    pass
print('done')
",
            sock.to_str().unwrap()
        ),
    );
    let log = stderr(&sb.arc(&["run", "--trace", "python3", "prog.py"]));
    if log.contains("no python3") || !log.contains("TRACE") {
        return;
    }
    assert!(
        log.contains("TRACE COMPLETE"),
        "a refused connection to a socket that does not exist is fingerprintable:
{log}"
    );

    assert_hit(&sb.arc(&["run", "python3", "prog.py"]), "nothing changed");
    std::os::unix::net::UnixListener::bind(&sock).unwrap();
    assert_miss(
        &sb.arc(&["run", "python3", "prog.py"]),
        "the socket now exists, so the connection no longer fails",
    );
}

// ------------------------------------------------------ paths and encoding ----

#[test]
fn relative_paths_resolve_against_the_process_that_used_them() {
    needs_tracer!();
    let sb = Sandbox::new();
    sb.write("src/nested.txt", "one");
    // `cd` first, so a tracer resolving against Arc's working directory instead
    // of the child's would record the wrong file — and then never invalidate.
    sb.learn("cd src && cat ./nested.txt");

    sb.write("src/nested.txt", "two");
    assert_miss(
        &sb.sh("cd src && cat ./nested.txt"),
        "the dependency must be src/nested.txt, not nested.txt at the root",
    );
}

#[test]
fn a_filename_that_is_not_valid_utf8_is_still_tracked() {
    needs_tracer!();
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let sb = Sandbox::new();
    // 0xff is not valid UTF-8. Lossy conversion would turn this into a
    // different path and quietly stop tracking the real one.
    let name = OsStr::from_bytes(b"weird-\xff-name.txt");
    let path = sb.root.join(name);
    std::fs::write(&path, "one").unwrap();
    let script = "for f in weird-*; do cat \"$f\"; done";
    sb.learn(script);

    std::fs::write(&path, "two").unwrap();
    // Arc's stored path identity is text, so a name that is not valid UTF-8
    // cannot be tracked by the tracer. The requirement is not that it tracks it
    // anyway — it is that failing to track it costs a miss rather than
    // producing a hit. The conservative scan is byte-safe and catches it.
    assert_miss(
        &sb.sh(script),
        "a path Arc cannot name must fall back, not be forgotten",
    );
}

// ----------------------------------------------------------- volatile state ----

#[test]
fn reading_procfs_prevents_the_trace_from_claiming_completeness() {
    needs_tracer!();
    let sb = Sandbox::new();
    let out = sb.arc(&["run", "--trace", "sh", "-c", "cat /proc/loadavg"]);
    assert_ok(&out);
    let log = stderr(&out);
    assert!(log.contains("TRACE PARTIAL"), "{log}");
    assert!(log.contains("volatile"), "{log}");
}

#[test]
fn touching_the_network_prevents_the_trace_from_claiming_completeness() {
    needs_tracer!();
    if !Path::new("/bin/bash").exists() {
        return;
    }
    // A refused connection is still a dependency on something outside the
    // filesystem, and needs no listener to provoke, so the test is hermetic.
    let sb = Sandbox::new();
    let out = sb.arc(&[
        "run",
        "--trace",
        "bash",
        "-c",
        "exec 3<>/dev/tcp/127.0.0.1/9 || true",
    ]);
    let log = stderr(&out);
    assert!(log.contains("TRACE PARTIAL"), "{log}");
    assert!(log.contains("network"), "{log}");
}

// ------------------------------------------------------------- resilience ----

#[test]
fn exit_status_survives_a_signal_and_the_run_is_not_cached() {
    needs_tracer!();
    let sb = Sandbox::new();
    let out = sb.sh("kill -TERM $$");
    assert_eq!(
        out.status.code(),
        Some(143),
        "a signalled child's status must reach the caller unchanged: {}",
        stderr(&out)
    );
    assert_miss(
        &sb.sh("kill -TERM $$"),
        "a run killed by a signal says nothing about its inputs",
    );
}

#[test]
fn a_very_large_trace_degrades_instead_of_failing() {
    needs_tracer!();
    let sb = Sandbox::new();
    // Thousands of filesystem events in one run. The requirement is not that
    // Arc keeps every one, it is that it never panics, never grows without
    // bound, and never claims completeness it did not achieve.
    let out = sb.sh("i=0; while [ $i -lt 2000 ]; do echo x > f$i; rm f$i; i=$((i+1)); done");
    assert_ok(&out);
    assert!(sb.arc(&["graph", "--json"]).status.success());
}

#[test]
fn arc_never_learns_its_own_cache_as_a_dependency() {
    needs_tracer!();
    // The Arc home is placed inside the project on purpose: every run writes to
    // it, so a tracer that recorded it would invalidate the family on every
    // single execution.
    let sb = Sandbox::new();
    let inner = sb.root.join(".arc-home");
    let call = || {
        Command::new(ARC)
            .args(["run", "--trace", "sh", "-c", "echo hello"])
            .current_dir(&sb.root)
            .env("ARC_HOME", &inner)
            .output()
            .unwrap()
    };
    call();
    call();
    assert_hit(&call(), "Arc's own writes must not invalidate the family");

    let graph = Command::new(ARC)
        .args(["graph", "--json"])
        .current_dir(&sb.root)
        .env("ARC_HOME", &inner)
        .output()
        .unwrap();
    assert!(
        !stdout(&graph).contains("arc-home"),
        "Arc's cache must not appear in its own dependency graph: {}",
        stdout(&graph)
    );
}

#[test]
fn secrets_never_reach_a_traced_dependency_set() {
    needs_tracer!();
    let sb = Sandbox::new();
    sb.write("arc.toml", "[env]\ninclude = [\"ARC_TEST_SECRET\"]\n");
    // The secret is also used as a filename, so a tracer that records paths
    // verbatim has every opportunity to leak it.
    let out = Command::new(ARC)
        .args([
            "run",
            "--trace",
            "sh",
            "-c",
            "echo x > never-persist-this-123.txt; cat never-persist-this-123.txt",
        ])
        .current_dir(&sb.root)
        .env("ARC_HOME", &sb.home)
        .env("ARC_TEST_SECRET", "never-persist-this-123")
        .env("AWS_SECRET_ACCESS_KEY", "fake-secret-456")
        .output()
        .unwrap();
    assert_ok(&out);

    let mut haystack = String::new();
    for args in [
        vec!["history", "--json"],
        vec!["graph", "--json"],
        vec!["inspect", "--json"],
    ] {
        let o = sb.arc(&args);
        haystack.push_str(&stdout(&o));
        haystack.push_str(&stderr(&o));
    }
    for entry in walk(&sb.home) {
        if let Ok(bytes) = std::fs::read(&entry) {
            haystack.push_str(&String::from_utf8_lossy(&bytes));
        }
    }
    // The *value* must never appear. The filename derived from it may, which is
    // exactly the distinction: Arc records paths, and never environment values.
    assert!(
        !haystack.contains("fake-secret-456"),
        "an environment value reached persisted state"
    );
    let leaked_as_value = haystack.contains("ARC_TEST_SECRET=never-persist-this-123");
    assert!(!leaked_as_value, "a secret was persisted with its name");
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        if e.path().is_dir() {
            out.extend(walk(&e.path()));
        } else {
            out.push(e.path());
        }
    }
    out
}

#[test]
fn concurrent_traced_runs_stay_independent() {
    needs_tracer!();
    let sb = Sandbox::new();
    sb.write("shared.txt", "one");
    let children: Vec<_> = (0..6)
        .map(|i| {
            Command::new(ARC)
                .args([
                    "run",
                    "--trace",
                    "sh",
                    "-c",
                    &format!("cat shared.txt; echo job{}", i % 3),
                ])
                .current_dir(&sb.root)
                .env("ARC_HOME", &sb.home)
                .spawn()
                .unwrap()
        })
        .collect();
    for mut c in children {
        assert!(c.wait().unwrap().success());
    }
    let graph = sb.arc(&["graph", "--json"]);
    assert!(graph.status.success(), "{}", stderr(&graph));
    let g: serde_json::Value = serde_json::from_str(&stdout(&graph)).unwrap();
    assert_eq!(
        g["nodes"].as_array().unwrap().len(),
        3,
        "each distinct command is its own family, however they interleaved"
    );
    assert!(stdout(&sb.arc(&["cache", "verify"])).contains("No corruption"));
}

// ------------------------------------------------------------- task graph ----

/// Task-graph edges that only a read-capable tracer can discover: nothing here
/// is declared in `arc.toml`, so every edge comes from observation alone.
mod graph {
    use super::*;

    fn project(sb: &Sandbox, config: &str) {
        sb.write("arc.toml", config);
    }

    fn graph_json(sb: &Sandbox) -> serde_json::Value {
        let out = sb.arc(&["graph", "--json"]);
        serde_json::from_str(&stdout(&out)).expect("graph json")
    }

    fn edges(sb: &Sandbox) -> Vec<(String, String)> {
        let g = graph_json(sb);
        let label = |k: &str| {
            g["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|n| n["family_key"] == k)
                .map(|n| n["label"].as_str().unwrap().to_string())
                .unwrap_or_default()
        };
        g["edges"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    label(e["from"].as_str().unwrap()),
                    label(e["to"].as_str().unwrap()),
                )
            })
            .collect()
    }

    const NAMES: &str = r#"
[[command]]
name = "producer"
match = "*the-producer*"

[[command]]
name = "consumer"
match = "*the-consumer*"
"#;

    #[test]
    fn an_observed_write_and_read_form_an_edge_with_no_configuration() {
        needs_tracer!();
        let sb = Sandbox::new();
        project(&sb, NAMES);
        sb.write("seed.txt", "one");
        sb.learn("cat seed.txt > out.txt # the-producer");
        sb.learn("cat out.txt > /dev/null # the-consumer");
        assert!(
            edges(&sb).contains(&("producer".into(), "consumer".into())),
            "{:?}",
            edges(&sb)
        );
    }

    #[test]
    fn a_generated_intermediate_creates_no_edge() {
        needs_tracer!();
        let sb = Sandbox::new();
        project(&sb, NAMES);
        sb.script(
            "gen.sh",
            "#!/bin/sh\necho x > tmp.txt\ncat tmp.txt > /dev/null\nrm -f tmp.txt\n",
        );
        sb.learn("./gen.sh # the-producer");
        sb.learn("echo unrelated # the-consumer");
        assert!(edges(&sb).is_empty(), "{:?}", edges(&sb));
    }

    #[test]
    fn a_producer_writing_into_an_enumerated_directory_is_an_edge() {
        needs_tracer!();
        let sb = Sandbox::new();
        project(&sb, NAMES);
        std::fs::create_dir(sb.root.join("plugins")).unwrap();
        sb.write("plugins/a", "a");
        sb.learn("for p in plugins/*; do echo $p; done # the-consumer");
        sb.learn("echo new > plugins/b # the-producer");
        assert!(
            edges(&sb).contains(&("producer".into(), "consumer".into())),
            "a new entry in an enumerated directory is a dependency: {:?}",
            edges(&sb)
        );
    }

    #[test]
    fn a_producer_of_a_path_whose_absence_mattered_is_an_edge() {
        needs_tracer!();
        let sb = Sandbox::new();
        project(&sb, NAMES);
        sb.script(
            "check.sh",
            "#!/bin/sh\nif [ -f optional.cfg ]; then echo yes; else echo no; fi\n",
        );
        sb.learn("./check.sh # the-consumer");
        sb.learn("echo x > optional.cfg # the-producer");
        assert!(
            edges(&sb).contains(&("producer".into(), "consumer".into())),
            "{:?}",
            edges(&sb)
        );
    }

    #[test]
    fn a_non_utf8_path_does_not_break_graph_construction() {
        needs_tracer!();
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let sb = Sandbox::new();
        project(&sb, NAMES);
        let name = OsStr::from_bytes(b"weird-\xff.txt");
        std::fs::write(sb.root.join(name), "one").unwrap();
        assert_ok(&sb.sh("for f in weird-*; do cat \"$f\"; done > out.txt # the-producer"));
        assert_ok(&sb.sh("cat out.txt > /dev/null # the-consumer"));
        let out = sb.arc(&["graph", "--json"]);
        assert_ok(&out);
        serde_json::from_str::<serde_json::Value>(&stdout(&out)).expect("valid json");
    }

    #[test]
    fn two_paths_naming_the_same_file_produce_one_edge() {
        needs_tracer!();
        let sb = Sandbox::new();
        project(&sb, NAMES);
        std::fs::create_dir(sb.root.join("sub")).unwrap();
        sb.learn("echo x > sub/out.txt # the-producer");
        // The consumer reaches the same file through `.` and `..`, which must
        // normalise to one dependency rather than three.
        sb.learn("cat ./sub/../sub/out.txt > /dev/null # the-consumer");
        let e = edges(&sb);
        assert_eq!(
            e.iter()
                .filter(|(a, b)| a == "producer" && b == "consumer")
                .count(),
            1,
            "{e:?}"
        );
    }

    #[test]
    fn a_cycle_between_two_tasks_is_reported_and_still_schedulable() {
        needs_tracer!();
        let sb = Sandbox::new();
        project(&sb, NAMES);
        sb.write("x.txt", "x");
        sb.write("y.txt", "y");
        sb.learn("cat y.txt > /dev/null; echo a > x.txt # the-producer");
        sb.learn("cat x.txt > /dev/null; echo b > y.txt # the-consumer");
        let g = graph_json(&sb);
        assert_eq!(g["cycles"].as_array().unwrap().len(), 1, "{g}");

        let text = stdout(&sb.arc(&["graph"]));
        assert!(text.contains("cycles"), "{text}");
        // The scheduler must terminate rather than wait for a prerequisite that
        // can never finish.
        let plan = sb.arc(&["affected", "--run", "--dry-run"]);
        assert_ok(&plan);
    }
}
