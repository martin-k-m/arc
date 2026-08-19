//! End-to-end tests driving the real `arc` binary.

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
        std::fs::write(root.join("input.txt"), "one").unwrap();
        Sandbox {
            _tmp: tmp,
            root,
            home,
        }
    }

    fn arc(&self, args: &[&str]) -> Output {
        Command::new(ARC)
            .args(args)
            .current_dir(&self.root)
            .env("ARC_HOME", &self.home)
            .output()
            .expect("running arc")
    }

    fn write(&self, rel: &str, body: &str) {
        let p = self.root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).to_string()
}
fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).to_string()
}

/// A deterministic child command that exists on every supported platform.
fn echo(text: &str) -> Vec<String> {
    if cfg!(windows) {
        vec!["cmd".into(), "/c".into(), format!("echo {text}")]
    } else {
        vec!["sh".into(), "-c".into(), format!("echo {text}")]
    }
}

fn write_file_cmd(rel: &str, text: &str) -> Vec<String> {
    if cfg!(windows) {
        vec![
            "cmd".into(),
            "/c".into(),
            format!("echo {text}> {}", rel.replace('/', "\\")),
        ]
    } else {
        vec![
            "sh".into(),
            "-c".into(),
            format!("mkdir -p $(dirname {rel}) && echo {text} > {rel}"),
        ]
    }
}

/// A command that genuinely reads a project file.
///
/// Where the tracer is complete, only a command that *reads* a file depends on
/// it, so a test about invalidation has to use one. Where it is not, the whole
/// project is the input set and this behaves identically.
fn read_cmd(rel: &str) -> Vec<String> {
    if cfg!(windows) {
        vec![
            "cmd".into(),
            "/c".into(),
            format!("type {}", rel.replace('/', "\\")),
        ]
    } else {
        vec!["sh".into(), "-c".into(), format!("cat {rel}")]
    }
}

fn fail_cmd(code: i32) -> Vec<String> {
    if cfg!(windows) {
        vec!["cmd".into(), "/c".into(), format!("exit {code}")]
    } else {
        vec!["sh".into(), "-c".into(), format!("exit {code}")]
    }
}

fn run(sb: &Sandbox, extra: &[&str], cmd: &[String]) -> Output {
    let mut args: Vec<&str> = vec!["run"];
    args.extend(extra);
    args.extend(cmd.iter().map(|s| s.as_str()));
    sb.arc(&args)
}

#[test]
fn second_identical_run_is_a_hit_and_replays_output_exactly() {
    let sb = Sandbox::new();
    let cmd = echo("hello-arc");

    let first = run(&sb, &[], &cmd);
    assert!(first.status.success(), "{}", stderr(&first));
    assert!(stdout(&first).contains("hello-arc"));
    assert!(!stderr(&first).contains("CACHE HIT"));

    let second = run(&sb, &[], &cmd);
    assert!(stderr(&second).contains("CACHE HIT"), "{}", stderr(&second));
    assert_eq!(
        stdout(&first),
        stdout(&second),
        "replayed output must be byte-identical"
    );
}

#[test]
fn changing_an_input_forces_a_miss_and_explains_which_file() {
    let sb = Sandbox::new();
    let cmd = read_cmd("input.txt");
    run(&sb, &[], &cmd);
    // A second run settles any narrowing: the first execution is what teaches
    // Arc that this command reads the file at all.
    run(&sb, &[], &cmd);
    sb.write("input.txt", "two");

    let out = run(&sb, &["--explain"], &cmd);
    let log = stderr(&out);
    assert!(log.contains("cache miss"), "{log}");
    assert!(log.contains("input.txt changed"), "{log}");
}

#[test]
fn exit_codes_are_preserved_and_failures_are_not_cached_by_default() {
    let sb = Sandbox::new();
    let cmd = fail_cmd(7);

    let first = run(&sb, &[], &cmd);
    assert_eq!(first.status.code(), Some(7));

    let second = run(&sb, &[], &cmd);
    assert_eq!(second.status.code(), Some(7));
    assert!(
        !stderr(&second).contains("CACHE HIT"),
        "a failing command must re-run unless --cache-failures is given"
    );

    let third = run(&sb, &["--cache-failures"], &cmd);
    assert_eq!(third.status.code(), Some(7));
    let fourth = run(&sb, &["--cache-failures"], &cmd);
    assert_eq!(fourth.status.code(), Some(7));
    assert!(stderr(&fourth).contains("CACHE HIT"), "{}", stderr(&fourth));
}

#[test]
fn no_cache_never_hits() {
    let sb = Sandbox::new();
    let cmd = echo("x");
    run(&sb, &[], &cmd);
    let out = run(&sb, &["--no-cache"], &cmd);
    assert!(!stderr(&out).contains("CACHE HIT"));
}

#[test]
fn refresh_replaces_the_cached_result() {
    let sb = Sandbox::new();
    let cmd = echo("x");
    run(&sb, &[], &cmd);
    let refreshed = run(&sb, &["--refresh"], &cmd);
    assert!(!stderr(&refreshed).contains("CACHE HIT"));
    let after = run(&sb, &[], &cmd);
    assert!(stderr(&after).contains("CACHE HIT"));
}

#[test]
fn declared_outputs_are_captured_and_restored() {
    let sb = Sandbox::new();
    sb.write("arc.toml", "[outputs]\ninclude = [\"out/**\"]\n");
    std::fs::create_dir_all(sb.root.join("out")).unwrap();
    let cmd = write_file_cmd("out/artifact.txt", "built");

    let first = run(&sb, &[], &cmd);
    assert!(first.status.success(), "{}", stderr(&first));
    let produced = std::fs::read_to_string(sb.root.join("out/artifact.txt")).unwrap();

    std::fs::remove_file(sb.root.join("out/artifact.txt")).unwrap();

    let second = run(&sb, &[], &cmd);
    assert!(stderr(&second).contains("CACHE HIT"), "{}", stderr(&second));
    assert_eq!(
        std::fs::read_to_string(sb.root.join("out/artifact.txt")).unwrap(),
        produced,
        "restored artifact must match the original"
    );
}

#[test]
fn arc_never_fingerprints_its_own_cache_directory() {
    // Arc home inside the project: writing cache state must not invalidate keys.
    let sb = Sandbox::new();
    let inner = sb.root.join(".arc-home");
    let cmd = echo("x");
    let call = |extra: &[&str]| {
        let mut args: Vec<&str> = vec!["run"];
        args.extend(extra);
        args.extend(cmd.iter().map(|s| s.as_str()));
        Command::new(ARC)
            .args(&args)
            .current_dir(&sb.root)
            .env("ARC_HOME", &inner)
            .output()
            .unwrap()
    };
    call(&[]);
    assert!(stderr(&call(&[])).contains("CACHE HIT"));
}

/// The same directory, spelled two ways.
///
/// `ARC_HOME` and the project root reach Arc from different places, so they can
/// disagree about how to name one directory: macOS resolves `/var` to
/// `/private/var`, and Windows hands out 8.3 short names like `RUNNER~1`. When
/// the home is inside the project and that comparison fails, Arc scans its own
/// database as project content — a miss at best, and on Windows a hard error,
/// because the file is locked by the process reading it.
#[cfg(unix)]
#[test]
fn an_arc_home_spelled_differently_is_still_recognised_as_arcs_own() {
    let sb = Sandbox::new();
    let real = sb.root.join("home-real");
    std::fs::create_dir_all(&real).unwrap();
    let link = sb.root.join("home-link");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    // Pinned to the conservative backend on purpose: with complete tracing the
    // fingerprint is narrowed to what the command read and the home is never
    // scanned, so the bug cannot appear. This is the path Windows and macOS
    // take on every run.
    let cmd = echo("x");
    let call = || {
        let mut args: Vec<&str> = vec!["run", "--trace-backend", "snapshot"];
        args.extend(cmd.iter().map(|s| s.as_str()));
        Command::new(ARC)
            .args(&args)
            .current_dir(&sb.root)
            .env("ARC_HOME", &link)
            .output()
            .unwrap()
    };
    let first = call();
    assert!(first.status.success(), "{}", stderr(&first));
    let second = call();
    assert!(
        stderr(&second).contains("CACHE HIT"),
        "Arc fingerprinted its own home when the path was spelled differently:
{}",
        stderr(&second)
    );
}

#[test]
fn history_inspect_and_stats_report_real_executions() {
    let sb = Sandbox::new();
    run(&sb, &[], &echo("recorded"));

    let history = sb.arc(&["history", "--json"]);
    let rows: serde_json::Value = serde_json::from_str(&stdout(&history)).unwrap();
    let id = rows[0]["id"].as_str().unwrap().to_string();
    assert_eq!(rows[0]["cache_status"], "miss");

    let inspected = sb.arc(&["inspect", &id, "--json"]);
    let rec: serde_json::Value = serde_json::from_str(&stdout(&inspected)).unwrap();
    assert_eq!(rec["id"], id);
    assert_eq!(rec["exit_code"], 0);

    let stats = sb.arc(&["cache", "stats", "--json"]);
    let s: serde_json::Value = serde_json::from_str(&stdout(&stats)).unwrap();
    assert_eq!(s["cache_entries"], 1);
    assert!(s["objects"].as_u64().unwrap() > 0);
}

#[test]
fn environment_values_are_never_persisted() {
    let sb = Sandbox::new();
    sb.write("arc.toml", "[env]\ninclude = [\"MY_API_TOKEN\"]\n");
    let cmd = echo("x");
    Command::new(ARC)
        .args(["run"])
        .args(cmd.iter().map(|s| s.as_str()))
        .current_dir(&sb.root)
        .env("ARC_HOME", &sb.home)
        .env("MY_API_TOKEN", "super-secret-value")
        .output()
        .unwrap();

    let history = sb.arc(&["history", "--json"]);
    assert!(!stdout(&history).contains("super-secret-value"));

    let id = serde_json::from_str::<serde_json::Value>(&stdout(&history)).unwrap()[0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let text = stdout(&sb.arc(&["inspect", &id]));
    assert!(!text.contains("super-secret-value"));
    assert!(text.contains("<redacted>"), "{text}");
}

#[test]
fn a_missing_cache_object_degrades_to_a_miss_not_a_failure() {
    let sb = Sandbox::new();
    let cmd = echo("x");
    run(&sb, &[], &cmd);

    // Simulate cache damage: delete every stored object.
    let blobs = sb.home.join("store/blobs");
    std::fs::remove_dir_all(&blobs).unwrap();
    std::fs::create_dir_all(&blobs).unwrap();

    let out = run(&sb, &[], &cmd);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!stderr(&out).contains("CACHE HIT"));
    assert!(stdout(&out).contains('x'));
}

#[test]
fn verify_quarantines_corrupt_objects_and_drops_their_entries() {
    let sb = Sandbox::new();
    run(&sb, &[], &echo("x"));

    let victim = first_blob(&sb.home.join("store/blobs"));
    std::fs::write(&victim, b"corrupted").unwrap();

    let out = sb.arc(&["cache", "verify"]);
    assert!(
        stdout(&out).contains("corrupted cache object"),
        "{}",
        stdout(&out)
    );

    let stats = sb.arc(&["cache", "stats", "--json"]);
    let s: serde_json::Value = serde_json::from_str(&stdout(&stats)).unwrap();
    assert_eq!(
        s["cache_entries"], 0,
        "entries depending on a corrupt object must be dropped"
    );
}

#[test]
fn concurrent_runs_share_the_cache_without_corrupting_it() {
    let sb = Sandbox::new();
    let children: Vec<_> = (0..6)
        .map(|i| {
            Command::new(ARC)
                .args(["run", "--"])
                .args(echo(&format!("job{}", i % 2)).iter().map(|s| s.as_str()))
                .current_dir(&sb.root)
                .env("ARC_HOME", &sb.home)
                .spawn()
                .unwrap()
        })
        .collect();
    for mut c in children {
        assert!(c.wait().unwrap().success());
    }
    let out = sb.arc(&["cache", "verify"]);
    assert!(
        stdout(&out).contains("No corruption found"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn prune_evicts_entries_until_the_cache_fits() {
    let sb = Sandbox::new();
    for i in 0..5 {
        sb.write("input.txt", &format!("v{i}"));
        run(&sb, &[], &echo(&format!("run{i}")));
    }
    let before: serde_json::Value =
        serde_json::from_str(&stdout(&sb.arc(&["cache", "stats", "--json"]))).unwrap();
    assert_eq!(before["cache_entries"], 5);

    // A limit the cache already fits under evicts nothing.
    assert!(sb
        .arc(&["cache", "prune", "--max-size", "1GB"])
        .status
        .success());
    let kept: serde_json::Value =
        serde_json::from_str(&stdout(&sb.arc(&["cache", "stats", "--json"]))).unwrap();
    assert_eq!(kept["cache_entries"], 5);

    let out = sb.arc(&["cache", "prune", "--max-size", "0"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let after: serde_json::Value =
        serde_json::from_str(&stdout(&sb.arc(&["cache", "stats", "--json"]))).unwrap();
    assert_eq!(
        after["cache_entries"], 0,
        "prune should have evicted entries: {after}"
    );
    assert_eq!(
        after["stored_bytes"], 0,
        "evicted objects must be collected: {after}"
    );
    assert!(stdout(&sb.arc(&["cache", "verify"])).contains("No corruption"));
}

#[test]
fn doctor_and_config_show_run_anywhere() {
    let sb = Sandbox::new();
    assert!(sb.arc(&["doctor"]).status.success());
    let cfg = sb.arc(&["config", "show"]);
    assert!(stdout(&cfg).contains("max_size"), "{}", stdout(&cfg));
}

#[test]
fn a_bad_config_file_is_reported_clearly() {
    let sb = Sandbox::new();
    sb.write("arc.toml", "[cache]\nenabld = true\n");
    let out = run(&sb, &[], &echo("x"));
    assert!(!out.status.success());
    assert!(stderr(&out).contains("arc.toml"), "{}", stderr(&out));
}

#[test]
fn an_unknown_program_explains_itself() {
    let sb = Sandbox::new();
    let out = sb.arc(&["run", "definitely-not-a-real-program-xyz"]);
    assert!(!out.status.success());
    let msg = stderr(&out);
    assert!(msg.contains("could not execute"), "{msg}");
    assert!(msg.contains("PATH"), "{msg}");
}

fn first_blob(dir: &Path) -> PathBuf {
    for shard in std::fs::read_dir(dir).unwrap() {
        let shard = shard.unwrap();
        if shard.file_type().unwrap().is_dir() {
            if let Some(f) = std::fs::read_dir(shard.path()).unwrap().next() {
                return f.unwrap().path();
            }
        }
    }
    panic!("no blobs stored");
}

// A reader that stops early must not turn into a panic. Rust ignores SIGPIPE,
// so `arc doctor | head` used to die at status 101 with "failed printing to
// stdout", and bench/environment.sh, whose awk stops at the section after
// tracing, inherited that status through pipefail and failed the nightly
// benchmark run for three nights.
#[cfg(unix)]
#[test]
fn a_reader_that_stops_early_does_not_panic() {
    use std::io::Read;

    let s = Sandbox::new();
    let mut child = Command::new(ARC)
        .arg("doctor")
        .current_dir(&s.root)
        .env("ARC_HOME", &s.home)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("running arc");

    // Read one chunk and drop the pipe, which is what `| head` does.
    let mut out = child.stdout.take().unwrap();
    let mut buf = [0u8; 64];
    let _ = out.read(&mut buf);
    drop(out);

    let mut err = String::new();
    child.stderr.take().unwrap().read_to_string(&mut err).ok();
    let status = child.wait().expect("waiting for arc");

    assert!(
        !err.contains("panicked"),
        "arc panicked when its reader went away: {err}"
    );
    assert!(
        status.code() != Some(101),
        "arc exited 101, the Rust panic status, when its reader went away"
    );
}
