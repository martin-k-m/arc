//! End-to-end tests for execution environments.
//!
//! The "toolchain" here is a directory of shell scripts. That is enough to test
//! everything v0.8 actually claims — content identity, deterministic PATH,
//! isolation, worker materialisation — without needing a compiler installed,
//! and it makes "the host has a *different* version of this tool" trivial to
//! arrange, which is the case that matters most.

// The execution and worker suites are Unix-only; on other platforms their
// helpers are compiled out rather than duplicated.
#![allow(dead_code, unused_imports, unused_macros)]

use arc_cache::{Options as CacheOptions, Server};
use arc_worker::{Options as WorkerOptions, Worker};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const ARC: &str = env!("CARGO_BIN_EXE_arc");

fn have_sh() -> bool {
    Command::new("sh").arg("-c").arg("exit 0").output().is_ok()
}

macro_rules! needs_sh {
    () => {
        if !have_sh() {
            return;
        }
    };
}

/// Whether a *copy* of the shell can be executed on this machine.
///
/// Materialising an environment copies each captured tool into the store and
/// runs the copy. On macOS with SIP enabled that is refused for Apple's own
/// binaries: `/bin/sh` is a platform binary, a copy of it is not, and AMFI
/// SIGKILLs it — the run reports exit 137 and no output at all. Reproducible in
/// three lines, with no Arc involved:
///
/// ```text
/// $ cp /bin/sh /tmp/sh-copy && /tmp/sh-copy -c 'echo hi'
/// $ echo $?
/// 137
/// ```
///
/// A copy of a non-Apple binary runs fine, and re-signing the copy ad hoc
/// (`codesign -f -s -`) makes it run — which is a possible fix for the
/// materialiser and a decision about identity, since the bytes then stop
/// matching the ones that were captured. See LIMITATIONS.md.
///
/// This probes the capability rather than the operating system on purpose. CI's
/// macOS job passes these tests, so whatever it runs on does allow it, and a
/// test skipped by `cfg!(target_os = "macos")` would stop covering the platform
/// that actually works. The probe answers for the machine in front of it.
fn can_execute_a_copy_of_sh() -> bool {
    use std::io::Write;
    let Ok(dir) = tempfile::tempdir() else {
        return false;
    };
    let Ok(sh) = which_sh() else { return false };
    let copy = dir.path().join("sh-probe");
    if std::fs::copy(&sh, &copy).is_err() {
        return false;
    }
    make_executable(&copy);
    // Written and flushed before use: a stale file handle is a different
    // failure from the one being probed for.
    let _ = std::io::stderr().flush();
    matches!(
        Command::new(&copy).arg("-c").arg("exit 0").status(),
        Ok(status) if status.success()
    )
}

/// The `sh` the materialiser would capture: the first one on PATH.
fn which_sh() -> Result<PathBuf, ()> {
    let path = std::env::var_os("PATH").ok_or(())?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("sh");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(())
}

/// A test that materialises an environment containing the host's `sh`.
///
/// Skips, loudly, where the platform will not execute a copy of it. Skipping is
/// the honest answer rather than a weakened assertion: the capability the test
/// asserts genuinely does not exist on such a machine, and the failure it would
/// otherwise report says nothing about Arc.
macro_rules! needs_a_runnable_copy_of_sh {
    () => {
        needs_sh!();
        if !can_execute_a_copy_of_sh() {
            eprintln!(
                "SKIP: this machine refuses to execute a copy of `sh` \
                 (macOS + SIP kills a copied Apple platform binary), so an \
                 environment containing the host shell cannot be materialised. \
                 See LIMITATIONS.md."
            );
            return;
        }
    };
}

// ------------------------------------------------------------------ fixture --

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    /// The "installed toolchain" a capture reads from.
    toolchain: PathBuf,
    env: Vec<(String, String)>,
}

impl Fixture {
    fn new() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let f = Fixture {
            root: tmp.path().join("repo"),
            home: tmp.path().join("archome"),
            toolchain: tmp.path().join("opt/greeter"),
            env: Vec::new(),
            _tmp: tmp,
        };
        std::fs::create_dir_all(f.root.join("src")).unwrap();
        std::fs::create_dir_all(f.toolchain.join("bin")).unwrap();
        std::fs::create_dir_all(f.toolchain.join("share")).unwrap();
        f.tool("greet", "#!/bin/sh\necho \"greet v1\"\n");
        std::fs::write(f.toolchain.join("share/data.txt"), "shared").unwrap();
        f.write("src/seed.txt", "one");
        f.config("");
        f
    }

    fn tool(&self, name: &str, body: &str) {
        let p = self.toolchain.join("bin").join(name);
        std::fs::write(&p, body).unwrap();
        make_executable(&p);
    }

    /// `extra` is appended to arc.toml, for per-test command blocks.
    fn config(&self, extra: &str) {
        self.write(
            "arc.toml",
            &format!(
                // `sh` is in the environment because the command *is* `sh -c`.
                // An environment that does not contain the shell a task runs
                // through is not an environment that task can run in — Arc will
                // not quietly borrow the host's.
                "[environment.greeter]\n\
                 tools = [\"sh\"]\n\n\
                 [[environment.greeter.tree]]\n\
                 from = {from:?}\n\
                 to = \"greeter\"\n\
                 exclude = [\"share/skip/**\"]\n\
                 {extra}",
                from = self.toolchain.to_string_lossy().replace('\\', "/"),
            ),
        );
    }

    fn write(&self, rel: &str, body: &str) {
        let p = self.root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn with_env(mut self, k: &str, v: &str) -> Fixture {
        self.env.push((k.into(), v.into()));
        self
    }

    fn arc(&self, args: &[&str]) -> Output {
        let mut c = Command::new(ARC);
        c.args(args)
            .current_dir(&self.root)
            .env("ARC_HOME", &self.home)
            .env("ARC_NO_ANIM", "1")
            .env("ARC_REMOTE_EXECUTION_TIMEOUT_MS", "45000")
            .env("ARC_REMOTE_EXECUTION_POLL_MS", "40");
        for (k, v) in &self.env {
            c.env(k, v);
        }
        c.output().expect("running arc")
    }

    fn capture(&self) -> serde_json::Value {
        let out = self.arc(&["env", "capture", "greeter", "--json"]);
        assert!(out.status.success(), "{}", text(&out));
        serde_json::from_slice(&out.stdout).expect("capture json")
    }

    fn pinned(&self) -> String {
        let text = std::fs::read_to_string(self.root.join("arc-env.lock")).unwrap();
        text.lines()
            .find_map(|l| {
                l.split_once('=')
                    .map(|(_, v)| v.trim().trim_matches('"').to_string())
            })
            .expect("a pinned id")
    }

    fn run(&self, script: &str) -> Output {
        self.arc(&["run", "--json", "sh", "-c", script])
    }
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

/// `arc run --json` writes one JSON object to stderr.
fn report(o: &Output) -> serde_json::Value {
    let err = String::from_utf8_lossy(&o.stderr);
    let line = err
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
        .unwrap_or_else(|| panic!("no json in:\n{err}"));
    serde_json::from_str(line).expect("run json")
}

#[cfg(unix)]
fn make_executable(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
}
#[cfg(not(unix))]
fn make_executable(_p: &Path) {}

// -------------------------------------------------------------- identity ----

#[test]
fn capture_pins_a_content_id_and_recapturing_unchanged_bytes_is_stable() {
    let f = Fixture::new();
    let first = f.capture();
    let id = first["id"].as_str().unwrap().to_string();
    assert_eq!(id.len(), 64);
    assert_eq!(f.pinned(), id);
    assert_eq!(first["completeness"], "complete");
    assert!(first["files"].as_u64().unwrap() >= 2);

    let again = f.capture();
    assert_eq!(again["id"], serde_json::Value::String(id.clone()));
    assert_eq!(again["previous"], serde_json::Value::String(id));
}

#[test]
fn one_changed_byte_of_a_tool_changes_the_environment_id() {
    let f = Fixture::new();
    let before = f.capture()["id"].as_str().unwrap().to_string();
    f.tool("greet", "#!/bin/sh\necho \"greet v2\"\n");
    let after = f.capture()["id"].as_str().unwrap().to_string();
    assert_ne!(before, after);
    assert_eq!(f.pinned(), after);
}

#[test]
fn an_alias_is_not_part_of_identity() {
    let f = Fixture::new();
    let id = f.capture()["id"].as_str().unwrap().to_string();

    // Same content under a different alias: same id.
    let text = std::fs::read_to_string(f.root.join("arc.toml")).unwrap();
    f.write(
        "arc.toml",
        &text
            .replace("greeter]", "tools]")
            .replace("environment.greeter.tree", "environment.tools.tree"),
    );
    let out = f.arc(&["env", "capture", "tools", "--json"]);
    assert!(out.status.success(), "{}", text_of(&out));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["id"], serde_json::Value::String(id));
}

fn text_of(o: &Output) -> String {
    text(o)
}

#[test]
fn inspect_reports_content_and_never_the_machine_it_came_from() {
    let f = Fixture::new();
    let id = f.capture()["id"].as_str().unwrap().to_string();
    let out = f.arc(&["env", "inspect", "greeter", "--json"]);
    assert!(out.status.success(), "{}", text(&out));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["id"], serde_json::Value::String(id.clone()));
    let path: Vec<&str> = v["path"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e.as_str().unwrap())
        .collect();
    assert!(path.contains(&"greeter/bin"), "{path:?}");

    // The absolute capture source is diagnostics, never manifest content.
    let source = f.toolchain.to_string_lossy().replace('\\', "/");
    assert!(!serde_json::to_string(&v).unwrap().contains(&source));

    // An id resolves as well as an alias.
    assert!(f.arc(&["env", "inspect", &id, "--json"]).status.success());
}

#[test]
fn verify_passes_on_a_fresh_capture_and_fails_on_a_corrupted_object() {
    let f = Fixture::new();
    f.capture();
    assert!(f.arc(&["env", "verify", "greeter"]).status.success());

    // Corrupt one object in the local store. Nothing may materialise from it.
    let blobs = f.home.join("store/blobs");
    let mut victim = None;
    for shard in std::fs::read_dir(&blobs).unwrap().flatten() {
        for file in std::fs::read_dir(shard.path()).unwrap().flatten() {
            if std::fs::read(file.path())
                .map(|b| b == b"shared")
                .unwrap_or(false)
            {
                victim = Some(file.path());
            }
        }
    }
    std::fs::write(victim.expect("the share/data.txt object"), b"tamper").unwrap();

    let out = f.arc(&["env", "verify", "greeter"]);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("corrupt"), "{}", text(&out));
}

#[test]
fn a_command_naming_an_undefined_environment_is_refused_before_it_runs() {
    let f = Fixture::new();
    f.config("\n[[command]]\nmatch = \"sh*\"\nenvironment = \"nope\"\n");
    let out = f.run("echo hi");
    assert!(!out.status.success());
    assert!(text(&out).contains("nope"), "{}", text(&out));
}

#[test]
fn an_unpinned_environment_names_the_command_that_would_fix_it() {
    let f = Fixture::new();
    f.config("\n[[command]]\nmatch = \"sh*\"\nenvironment = \"greeter\"\n");
    let out = f.run("echo hi");
    assert!(!out.status.success());
    assert!(
        text(&out).contains("arc env capture greeter"),
        "{}",
        text(&out)
    );
}

#[test]
fn a_lock_pinned_to_something_that_is_not_an_id_is_refused() {
    let f = Fixture::new();
    f.capture();
    f.write("arc-env.lock", "[environments]\ngreeter = \"stable\"\n");
    f.config("\n[[command]]\nmatch = \"sh*\"\nenvironment = \"greeter\"\n");
    let out = f.run("echo hi");
    assert!(!out.status.success());
    assert!(
        text(&out).contains("not pinned to an environment id"),
        "{}",
        text(&out)
    );
}

// -------------------------------------------------------------- execution ---

#[cfg(unix)]
mod execution {
    use super::*;

    /// A project whose `greet` command runs inside the captured environment.
    fn with_command(f: &Fixture) {
        f.config("\n[[command]]\nname = \"greet\"\nmatch = \"sh*\"\nenvironment = \"greeter\"\n");
    }

    #[test]
    fn the_captured_tool_runs_and_the_host_copy_does_not() {
        needs_a_runnable_copy_of_sh!();
        let f = Fixture::new();
        with_command(&f);
        f.capture();

        // A *different* `greet`, first on the host's PATH. If the environment
        // leaked, this is what would answer.
        let host_bin = f.root.parent().unwrap().join("hostbin");
        std::fs::create_dir_all(&host_bin).unwrap();
        let p = host_bin.join("greet");
        std::fs::write(&p, "#!/bin/sh\necho \"HOST GREET\"\n").unwrap();
        make_executable(&p);
        let path = format!(
            "{}:{}",
            host_bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );

        let f = f.with_env("PATH", &path);
        let out = f.run("greet");
        assert!(out.status.success(), "{}", text(&out));
        assert!(text(&out).contains("greet v1"), "{}", text(&out));
        assert!(!text(&out).contains("HOST GREET"), "{}", text(&out));

        let r = report(&out);
        assert_eq!(r["environment"]["alias"], "greeter");
        assert_eq!(r["explain"]["environment_id"], f.pinned());
    }

    #[test]
    fn a_radically_different_host_path_changes_nothing() {
        needs_a_runnable_copy_of_sh!();
        let f = Fixture::new();
        with_command(&f);
        f.capture();
        let base = f.run("greet");
        assert!(base.status.success(), "{}", text(&base));
        let key = report(&base)["key"].as_str().unwrap().to_string();

        // The same project, the same environment, a PATH that shares nothing
        // with the first run's. Refreshing forces the key to be recomputed
        // rather than answered from the entry the first run wrote.
        let mut other = Fixture {
            _tmp: tempfile::tempdir().unwrap(),
            root: f.root.clone(),
            home: f.home.clone(),
            toolchain: f.toolchain.clone(),
            env: vec![("PATH".into(), "/nonexistent:/definitely/not/here".into())],
        };
        other.env.push(("ARC_NO_ANIM".into(), "1".into()));
        let out = other.arc(&["run", "--json", "--refresh", "sh", "-c", "greet"]);
        assert!(out.status.success(), "{}", text(&out));
        assert_eq!(report(&out)["key"], key, "the key must not depend on PATH");
    }

    #[test]
    fn re_capturing_a_changed_tool_invalidates_the_cache_safely() {
        needs_a_runnable_copy_of_sh!();
        let f = Fixture::new();
        with_command(&f);
        f.capture();
        assert!(f.run("greet").status.success());
        assert_eq!(report(&f.run("greet"))["cache_status"], "HIT");

        f.tool("greet", "#!/bin/sh\necho \"greet v2\"\n");
        f.capture();
        let out = f.run("greet");
        assert_eq!(report(&out)["cache_status"], "MISS", "{}", text(&out));
        assert!(text(&out).contains("greet v2"), "{}", text(&out));
        assert_eq!(report(&f.run("greet"))["cache_status"], "HIT");
    }

    #[test]
    fn home_and_tmp_are_the_environments_own() {
        needs_sh!();
        let f = Fixture::new();
        with_command(&f);
        f.capture();

        // Host home holds something that would change the command's answer.
        let host_home = f.root.parent().unwrap().join("hosthome");
        std::fs::create_dir_all(&host_home).unwrap();
        std::fs::write(host_home.join(".greetrc"), "SECRET FROM HOST").unwrap();

        let f = f.with_env("HOME", &host_home.to_string_lossy());
        let out = f.run("cat \"$HOME/.greetrc\" 2>/dev/null || echo NO-HOST-CONFIG");
        assert!(text(&out).contains("NO-HOST-CONFIG"), "{}", text(&out));

        let out = f.run("echo \"$TMPDIR\"; echo \"$XDG_CACHE_HOME\"");
        let t = text(&out);
        assert!(t.contains("/tmp\n") || t.contains("tmp"), "{t}");
        assert!(!t.contains(&host_home.to_string_lossy().to_string()), "{t}");
    }

    #[test]
    fn a_tool_the_environment_does_not_provide_is_not_taken_from_the_host() {
        needs_sh!();
        let f = Fixture::new();
        f.config(
            "\n[[command]]\nname = \"missing\"\nmatch = \"definitely-not-a-tool*\"\nenvironment = \"greeter\"\n",
        );
        f.capture();
        let out = f.arc(&["run", "definitely-not-a-tool"]);
        assert!(!out.status.success());
        let t = text(&out);
        assert!(t.contains("does not provide"), "{t}");
        assert!(t.contains("never falls back"), "{t}");
    }

    #[test]
    fn a_materialised_environment_is_written_read_only() {
        use std::os::unix::fs::PermissionsExt;
        needs_a_runnable_copy_of_sh!();
        let f = Fixture::new();
        with_command(&f);
        let id = f.capture()["id"].as_str().unwrap().to_string();
        // Materialise it by using it.
        assert!(f.run("greet").status.success());
        let root = f.home.join("environments").join(&id);
        let tool = root.join("greeter/bin/greet");

        let mode = std::fs::metadata(&tool).unwrap().permissions().mode();
        assert_eq!(mode & 0o222, 0, "{tool:?} is writable: {mode:o}");
        assert_ne!(mode & 0o111, 0, "the tool must stay executable");
        assert_eq!(
            std::fs::metadata(&root).unwrap().permissions().mode() & 0o222,
            0,
            "the environment root must not accept new files"
        );

        // Permission bits are the mechanism, and root ignores them. Arc says so
        // in docs/environments.md rather than claiming a guarantee it does not
        // have, so the write test only runs where it means something.
        if unsafe { libc::geteuid() } != 0 {
            let out = f.run(&format!(
                "echo VANDAL > {} 2>/dev/null; greet",
                tool.display()
            ));
            assert!(text(&out).contains("greet v1"), "{}", text(&out));
            assert_eq!(
                std::fs::read_to_string(&tool).unwrap(),
                "#!/bin/sh\necho \"greet v1\"\n"
            );
        }
    }

    #[test]
    fn reading_host_state_downgrades_hermeticity_rather_than_being_ignored() {
        needs_a_runnable_copy_of_sh!();
        let f = Fixture::new();
        with_command(&f);
        f.capture();
        let out = f.run("greet");
        assert!(out.status.success(), "{}", text(&out));
        let r = report(&out);
        // Whatever the tracer on this platform can see, Arc never claims more
        // than it observed.
        let h = r["environment"]["hermeticity"].as_str().unwrap_or("");
        assert!(
            ["hermetic", "host-dependent", "unknown"].contains(&h),
            "unexpected hermeticity {h}"
        );
    }
}

// ------------------------------------------------------------------ remote --

#[cfg(unix)]
mod remote {
    use super::*;

    struct Cluster {
        _tmp: tempfile::TempDir,
        cache: Option<Server>,
        worker: Option<Worker>,
        cache_url: String,
        worker_url: String,
        worker_data: PathBuf,
    }

    impl Cluster {
        fn new() -> Cluster {
            let tmp = tempfile::tempdir().unwrap();
            let cache = Server::start(CacheOptions {
                data: tmp.path().join("cache"),
                addr: "127.0.0.1:0".into(),
                ..Default::default()
            })
            .unwrap();
            let cache_url = cache.url();
            let worker_data = tmp.path().join("worker");
            let worker = Worker::start(WorkerOptions {
                data: worker_data.clone(),
                addr: "127.0.0.1:0".into(),
                max_jobs: 4,
                cache: arc_core::remote::RemoteConfig {
                    url: cache_url.clone(),
                    namespace: "placeholder".into(),
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap();
            Cluster {
                worker_url: worker.url(),
                cache_url,
                cache: Some(cache),
                worker: Some(worker),
                worker_data,
                _tmp: tmp,
            }
        }

        fn environments(&self) -> Vec<String> {
            std::fs::read_dir(self.worker_data.join("environments"))
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        }
    }

    impl Drop for Cluster {
        fn drop(&mut self) {
            self.worker = None;
            self.cache = None;
        }
    }

    fn remote_config(f: &Fixture, c: &Cluster) {
        f.config(&format!(
            "\n[[command]]\nname = \"greet\"\nmatch = \"sh*\"\nenvironment = \"greeter\"\n\n\
             [remote]\nurl = {cache:?}\nnamespace = \"env-demo\"\n\n\
             [remote.execution]\nenabled = true\nurl = {worker:?}\n",
            cache = c.cache_url,
            worker = c.worker_url,
        ));
    }

    #[test]
    fn a_worker_without_the_tool_runs_the_command_by_materialising_the_environment() {
        needs_a_runnable_copy_of_sh!();
        let c = Cluster::new();
        let f = Fixture::new();
        remote_config(&f, &c);
        f.capture();
        let id = f.pinned();

        // Nothing named `greet` exists on this machine's PATH at all, so the
        // worker can only succeed by using the environment.
        assert!(
            arc_core::key::which("greet", &f.root).is_none(),
            "the host must not provide the tool under test"
        );

        let out = f.run("greet");
        assert!(out.status.success(), "{}", text(&out));
        assert!(text(&out).contains("greet v1"), "{}", text(&out));
        let r = report(&out);
        assert_eq!(r["execution"]["source"], "remote", "{}", text(&out));
        assert!(
            c.environments().contains(&id),
            "worker environments: {:?}",
            c.environments()
        );
    }

    #[test]
    fn a_second_job_reuses_the_workers_copy_of_the_environment() {
        needs_a_runnable_copy_of_sh!();
        let c = Cluster::new();
        let f = Fixture::new();
        remote_config(&f, &c);
        f.capture();

        assert!(f.run("greet").status.success());
        f.write("src/seed.txt", "two");
        let out = f.run("greet");
        assert!(out.status.success(), "{}", text(&out));
        assert_eq!(report(&out)["execution"]["source"], "remote");
        // One environment directory, two executions.
        assert_eq!(c.environments().len(), 1, "{:?}", c.environments());
    }

    #[test]
    fn a_result_built_here_is_a_remote_hit_on_a_machine_that_never_had_the_tool() {
        needs_a_runnable_copy_of_sh!();
        let c = Cluster::new();
        let f = Fixture::new();
        remote_config(&f, &c);
        f.capture();
        assert!(f.run("greet").status.success());

        // A different checkout, a different Arc home, the same environment id.
        let other = Fixture {
            _tmp: tempfile::tempdir().unwrap(),
            root: f.root.parent().unwrap().join("elsewhere"),
            home: f.root.parent().unwrap().join("archome-other"),
            toolchain: f.toolchain.clone(),
            env: Vec::new(),
        };
        std::fs::create_dir_all(other.root.join("src")).unwrap();
        for rel in ["arc.toml", "arc-env.lock", "src/seed.txt"] {
            std::fs::copy(f.root.join(rel), other.root.join(rel)).unwrap();
        }
        // The environment itself has to travel too.
        assert!(other
            .arc(&["env", "capture", "greeter", "--publish", "--json"])
            .status
            .success());

        let out = other.run("greet");
        assert!(out.status.success(), "{}", text(&out));
        let r = report(&out);
        assert_eq!(r["cache_status"], "HIT", "{}", text(&out));
        assert_eq!(r["cache"]["source"], "remote");
    }

    #[test]
    fn a_corrupted_environment_object_stops_the_worker_rather_than_running_something_else() {
        needs_sh!();
        let c = Cluster::new();
        let f = Fixture::new();
        remote_config(&f, &c);
        f.capture();
        // Publish, then rewrite one of the environment's objects in the shared
        // cache. The worker must refuse it rather than execute a tool it cannot
        // verify.
        assert!(f
            .arc(&["env", "capture", "greeter", "--publish"])
            .status
            .success());
        let mut corrupted = false;
        for shard in std::fs::read_dir(c._tmp.path().join("cache/objects"))
            .into_iter()
            .flatten()
            .flatten()
        {
            for file in std::fs::read_dir(shard.path())
                .into_iter()
                .flatten()
                .flatten()
            {
                if std::fs::read(file.path())
                    .map(|b| b.starts_with(b"#!/bin/sh"))
                    .unwrap_or(false)
                {
                    std::fs::write(file.path(), b"#!/bin/sh\necho PWNED\n").unwrap();
                    corrupted = true;
                }
            }
        }
        assert!(corrupted, "nothing to corrupt");

        // A fresh client has to fetch everything, so it meets the tampered copy.
        let other = Fixture {
            _tmp: tempfile::tempdir().unwrap(),
            root: f.root.clone(),
            home: f.root.parent().unwrap().join("archome-fresh"),
            toolchain: f.toolchain.clone(),
            env: Vec::new(),
        };
        let out = other.run("greet");
        assert!(!text(&out).contains("PWNED"), "{}", text(&out));
    }
}
