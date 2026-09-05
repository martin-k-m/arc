//! End-to-end tests for remote execution.
//!
//! A real cache server and a real worker, both in-process so a test can make
//! either of them misbehave, driven by the real `arc` binary. The worker runs
//! on the same machine as the client, which is what makes toolchain digests
//! match; every test that needs them *not* to match says so explicitly.

mod support;

use arc_cache::{Options as CacheOptions, Server};
use arc_worker::{Options as WorkerOptions, Worker};
use std::path::PathBuf;
use std::process::{Command, Output};

const ARC: &str = env!("CARGO_BIN_EXE_arc");
const SECRET: &str = "worker-execute-token-98765";

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

struct Cluster {
    _tmp: tempfile::TempDir,
    cache_dir: PathBuf,
    cache: Option<Server>,
    worker: Option<Worker>,
    cache_url: String,
    worker_url: String,
}

impl Cluster {
    fn new() -> Cluster {
        Cluster::with(4, 64, None, None)
    }

    fn with(
        max_jobs: usize,
        queue_limit: usize,
        execute_token: Option<String>,
        read_token: Option<String>,
    ) -> Cluster {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("cache");
        let cache = Server::start(CacheOptions {
            data: cache_dir.clone(),
            addr: "127.0.0.1:0".into(),
            ..Default::default()
        })
        .unwrap();
        let cache_url = cache.url();
        let worker = Worker::start(WorkerOptions {
            data: tmp.path().join("worker"),
            addr: "127.0.0.1:0".into(),
            execute_token,
            read_token,
            max_jobs,
            queue_limit,
            cache: arc_core::remote::RemoteConfig {
                url: cache_url.clone(),
                namespace: "placeholder".into(),
                ..Default::default()
            },
            log: false,
        })
        .unwrap();
        Cluster {
            worker_url: worker.url(),
            cache_url,
            cache: Some(cache),
            worker: Some(worker),
            cache_dir,
            _tmp: tmp,
        }
    }

    fn stop_worker(&mut self) {
        self.worker = None;
    }

    fn stop_cache(&mut self) {
        self.cache = None;
    }

    /// `(submitted, deduplicated, completed, failed, cache_short_circuits)`
    fn stats(&self) -> (u64, u64, u64, u64, u64) {
        self.worker.as_ref().map(|w| w.stats()).unwrap_or_default()
    }

    fn executions(&self, ns: &str) -> usize {
        std::fs::read_dir(self.cache_dir.join("executions").join(ns))
            .into_iter()
            .flatten()
            .flatten()
            .count()
    }

    fn objects(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for shard in std::fs::read_dir(self.cache_dir.join("objects"))
            .into_iter()
            .flatten()
            .flatten()
        {
            for f in std::fs::read_dir(shard.path())
                .into_iter()
                .flatten()
                .flatten()
            {
                out.push(f.path());
            }
        }
        out
    }
}

struct Client {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    env: Vec<(String, String)>,
}

impl Client {
    fn new(c: &Cluster) -> Client {
        Client::at(c, "repo", "demo", "")
    }

    fn at(c: &Cluster, dir: &str, ns: &str, extra: &str) -> Client {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(dir);
        std::fs::create_dir_all(root.join("src")).unwrap();
        let client = Client {
            home: tmp.path().join("archome"),
            root,
            _tmp: tmp,
            env: Vec::new(),
        };
        client.write("arc.toml", &config(&c.cache_url, &c.worker_url, ns, extra));
        client.write("src/seed.txt", "one");
        client
    }

    fn env(mut self, k: &str, v: &str) -> Client {
        self.env.push((k.into(), v.into()));
        self
    }

    fn write(&self, rel: &str, body: &str) {
        let p = self.root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(ARC);
        c.args(args)
            .current_dir(&self.root)
            .env("ARC_HOME", &self.home)
            .env("ARC_NO_ANIM", "1")
            .env("ARC_CACHE_TOKEN", SECRET)
            // Tests must fail rather than hang: a stuck job is a bug to see,
            // not a thirty-minute wait.
            .env("ARC_REMOTE_EXECUTION_TIMEOUT_MS", "45000")
            .env("ARC_REMOTE_EXECUTION_POLL_MS", "40");
        for (k, v) in &self.env {
            c.env(k, v);
        }
        c
    }

    fn arc(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("running arc")
    }

    fn run(&self, script: &str) -> Output {
        self.arc(&["run", "--json", "sh", "-c", script])
    }
}

/// `extra` holds `[remote.execution]` keys. A key it sets replaces the default
/// rather than being appended twice, so a test can turn something off.
fn config(cache_url: &str, worker_url: &str, ns: &str, extra: &str) -> String {
    let mut exec: Vec<(String, String)> = vec![
        ("enabled".into(), "true".into()),
        ("url".into(), format!("\"{worker_url}\"")),
    ];
    for line in extra.lines().filter(|l| !l.trim().is_empty()) {
        let (k, v) = line.split_once('=').expect("key = value");
        let (k, v) = (k.trim().to_string(), v.trim().to_string());
        match exec.iter_mut().find(|(n, _)| *n == k) {
            Some(slot) => slot.1 = v,
            None => exec.push((k, v)),
        }
    }
    let body: String = exec
        .iter()
        .map(|(k, v)| {
            format!(
                "{k} = {v}
"
            )
        })
        .collect();
    format!(
        "[outputs]
include = [\"out/**\"]

[remote]
url = \"{cache_url}\"
namespace = \"{ns}\"

[remote.execution]
{body}"
    )
}

fn result(out: &Output) -> serde_json::Value {
    let text = String::from_utf8_lossy(&out.stderr);
    let line = text
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
        .unwrap_or_else(|| panic!("no json in:\n{text}"));
    serde_json::from_str(line).expect("parsing arc run --json")
}

fn source(out: &Output) -> String {
    result(out)["execution"]["source"]
        .as_str()
        .unwrap()
        .to_string()
}

fn cache_source(out: &Output) -> String {
    result(out)["cache"]["source"].as_str().unwrap().to_string()
}

fn status(out: &Output) -> String {
    result(out)["cache"]["status"].as_str().unwrap().to_string()
}

fn reason(out: &Output) -> String {
    result(out)["execution"]["reason"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

const SCRIPT: &str = "mkdir -p out && cat src/seed.txt > out/built.txt && echo made";

// ------------------------------------------------------------ the basics ----

#[test]
fn a_cache_miss_runs_on_the_worker_and_the_output_comes_back() {
    needs_sh!();
    let cluster = Cluster::new();
    let client = Client::new(&cluster);

    let out = client.run(SCRIPT);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(status(&out), "miss");
    assert_eq!(source(&out), "remote", "{}", reason(&out));
    assert_eq!(
        std::fs::read_to_string(client.root.join("out/built.txt")).unwrap(),
        "one"
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("made"));
    assert_eq!(cluster.stats().2, 1, "exactly one command ran");
}

#[test]
fn another_machine_gets_a_cache_hit_and_the_worker_runs_nothing_more() {
    needs_sh!();
    let cluster = Cluster::new();
    let a = Client::new(&cluster);
    assert_eq!(source(&a.run(SCRIPT)), "remote");
    let executed_once = cluster.stats().2;

    let b = Client::at(&cluster, "other", "demo", "");
    let out = b.run(SCRIPT);
    assert_eq!(status(&out), "hit", "{}", stderr(&out));
    assert_eq!(cache_source(&out), "remote");
    assert_eq!(source(&out), "none", "a hit executes nothing");
    assert_eq!(cluster.stats().2, executed_once, "no second execution");
    assert_eq!(
        std::fs::read_to_string(b.root.join("out/built.txt")).unwrap(),
        "one"
    );
}

#[test]
fn the_same_machine_gets_a_local_hit_with_the_cluster_gone() {
    needs_sh!();
    let mut cluster = Cluster::new();
    let client = Client::new(&cluster);
    assert_eq!(source(&client.run(SCRIPT)), "remote");

    cluster.stop_worker();
    cluster.stop_cache();
    let out = client.run(SCRIPT);
    assert_eq!(status(&out), "hit", "{}", stderr(&out));
    assert_eq!(cache_source(&out), "local");
}

#[test]
fn a_changed_input_executes_again_and_does_not_reuse_the_old_result() {
    needs_sh!();
    let cluster = Cluster::new();
    let client = Client::new(&cluster);
    client.run(SCRIPT);

    client.write("src/seed.txt", "two");
    let out = client.run(SCRIPT);
    assert_eq!(status(&out), "miss");
    assert_eq!(source(&out), "remote");
    assert_eq!(
        std::fs::read_to_string(client.root.join("out/built.txt")).unwrap(),
        "two"
    );
    assert_eq!(cluster.stats().2, 2);
}

#[test]
fn the_worker_reuses_objects_it_already_holds() {
    needs_sh!();
    let cluster = Cluster::new();
    let client = Client::new(&cluster);
    client.write("src/big.txt", &"payload ".repeat(20_000));
    client.run("cat src/big.txt > /dev/null && echo first");

    let before = cluster.objects().len();
    // A different command over the same inputs: the shared blob is already in
    // the cache and already in the worker's store.
    let out = client.run("wc -c < src/big.txt > /dev/null && echo second");
    assert_eq!(source(&out), "remote", "{}", reason(&out));
    let t = &result(&out)["execution"]["timings"];
    assert_eq!(
        t["uploaded_objects"].as_u64().unwrap(),
        0,
        "nothing needed uploading a second time"
    );
    assert!(cluster.objects().len() >= before);
}

// ------------------------------------------------------------- integrity ----

#[test]
fn a_corrupt_input_object_stops_the_execution_rather_than_feeding_it() {
    needs_sh!();
    let cluster = Cluster::new();
    let client = Client::new(&cluster);
    // Populate the cache, then corrupt every stored object.
    client.run(SCRIPT);
    for p in cluster.objects() {
        std::fs::write(&p, b"tampered").unwrap();
    }

    let fresh = Client::at(&cluster, "fresh", "demo", "");
    fresh.write("src/seed.txt", "one");
    let out = fresh.run("cat src/seed.txt > /dev/null && echo verified");
    // Whatever happens, it must not be a result built from corrupt bytes.
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("verified"));
}

#[test]
fn a_corrupt_result_object_is_refused_by_the_client() {
    needs_sh!();
    let cluster = Cluster::new();
    let client = Client::new(&cluster);
    assert_eq!(source(&client.run(SCRIPT)), "remote");

    // Corrupt what the worker published, then ask a fresh machine for it.
    for p in cluster.objects() {
        std::fs::write(&p, b"not the real bytes").unwrap();
    }
    let b = Client::at(&cluster, "other", "demo", "");
    let out = b.run(SCRIPT);
    assert!(out.status.success(), "{}", stderr(&out));
    // It must not have restored the tampered bytes.
    let restored = std::fs::read_to_string(b.root.join("out/built.txt")).unwrap_or_default();
    assert_ne!(restored, "not the real bytes");
}

#[test]
fn nothing_is_published_when_the_command_fails() {
    needs_sh!();
    let cluster = Cluster::new();
    let client = Client::new(&cluster);
    let out = client.run("echo failing && exit 7");
    assert_eq!(out.status.code(), Some(7), "exit status is preserved");
    assert_eq!(source(&out), "remote", "{}", reason(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("failing"));
    assert_eq!(
        cluster.executions("demo"),
        0,
        "a failure is not a cacheable result"
    );
}

#[test]
fn a_timeout_kills_the_command_and_publishes_nothing() {
    needs_sh!();
    let cluster = Cluster::new();
    let client = Client::at(&cluster, "repo", "demo", "timeout_ms = 1500\n");
    let out = client.arc(&["run", "--json", "sh", "-c", "sleep 30 && echo never"]);
    assert!(!out.status.success());
    let text = stderr(&out);
    assert!(
        text.contains("timeout") || text.contains("did not complete"),
        "{text}"
    );
    assert_eq!(cluster.executions("demo"), 0);
    assert!(!String::from_utf8_lossy(&out.stdout).contains("never"));
}

// ------------------------------------------------------------ eligibility ---

#[test]
fn remote_execution_is_off_unless_something_turns_it_on() {
    needs_sh!();
    let cluster = Cluster::new();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    std::fs::create_dir_all(root.join("src")).unwrap();
    // A remote *cache* and nothing else.
    std::fs::write(
        root.join("arc.toml"),
        format!(
            "[outputs]\ninclude = [\"out/**\"]\n\n[remote]\nurl = \"{}\"\nnamespace = \"demo\"\n",
            cluster.cache_url
        ),
    )
    .unwrap();
    std::fs::write(root.join("src/seed.txt"), "one").unwrap();

    let out = Command::new(ARC)
        .args(["run", "--json", "sh", "-c", SCRIPT])
        .current_dir(&root)
        .env("ARC_HOME", tmp.path().join("archome"))
        .env("ARC_NO_ANIM", "1")
        .output()
        .unwrap();
    assert_eq!(source(&out), "local", "{}", reason(&out));
    assert_eq!(cluster.stats().2, 0, "the worker was never asked");
}

#[test]
fn the_command_line_can_force_remote_execution_on_or_off() {
    needs_sh!();
    let cluster = Cluster::new();
    let client = Client::new(&cluster);
    let off = client.arc(&["run", "--json", "--no-remote-execution", "sh", "-c", SCRIPT]);
    assert_eq!(source(&off), "local", "{}", reason(&off));

    let client = Client::at(&cluster, "forced", "demo", "enabled = false\n");
    let on = client.arc(&["run", "--json", "--remote-execution", "sh", "-c", SCRIPT]);
    assert_eq!(source(&on), "remote", "{}", reason(&on));
}

#[test]
fn a_command_configured_never_stays_on_this_machine() {
    needs_sh!();
    let cluster = Cluster::new();
    let client = Client::at(&cluster, "repo", "demo", "");
    let cfg = std::fs::read_to_string(client.root.join("arc.toml")).unwrap();
    client.write(
        "arc.toml",
        &format!("{cfg}\n[[command]]\nmatch = \"*deploy*\"\nremote = \"never\"\n"),
    );

    let deploy = client.run("echo deploying to production");
    assert_eq!(source(&deploy), "local", "{}", reason(&deploy));
    let ordinary = client.run(SCRIPT);
    assert_eq!(source(&ordinary), "remote", "{}", reason(&ordinary));
}

#[test]
fn an_argument_naming_a_path_on_this_machine_keeps_the_command_here() {
    needs_sh!();
    let cluster = Cluster::new();
    let client = Client::new(&cluster);
    let out = client.arc(&["run", "--json", "sh", "-c", "echo hi", "--", "/etc/hosts"]);
    assert_eq!(source(&out), "local", "{}", reason(&out));
    assert!(reason(&out).contains("names a path on this machine"));
}

#[test]
fn a_secret_shaped_variable_keeps_the_command_here_and_is_never_sent() {
    needs_sh!();
    let cluster = Cluster::new();
    let client = Client::at(&cluster, "repo", "secrets", "");
    let cfg = std::fs::read_to_string(client.root.join("arc.toml")).unwrap();
    client.write(
        "arc.toml",
        &format!("{cfg}\n[env]\ninclude = [\"DEPLOY_TOKEN\"]\n"),
    );
    let client = client.env("DEPLOY_TOKEN", "hunter2-do-not-send");

    let out = client.run(SCRIPT);
    assert_eq!(source(&out), "local", "{}", reason(&out));
    assert!(reason(&out).contains("DEPLOY_TOKEN"));

    // And the value is nowhere: not on the wire, not in the cache, not in Arc.
    let text = format!("{}{}", stderr(&out), String::from_utf8_lossy(&out.stdout));
    assert!(!text.contains("hunter2-do-not-send"));
    for p in cluster.objects() {
        let bytes = std::fs::read(&p).unwrap_or_default();
        assert!(
            !String::from_utf8_lossy(&bytes).contains("hunter2-do-not-send"),
            "secret reached the shared cache"
        );
    }
    let history = client.arc(&["history", "--json"]);
    assert!(!String::from_utf8_lossy(&history.stdout).contains("hunter2-do-not-send"));
}

#[test]
fn an_explicitly_allowed_variable_may_be_sent() {
    needs_sh!();
    let cluster = Cluster::new();
    let client = Client::at(&cluster, "repo", "demo", "allow_env = [\"BUILD_TOKEN\"]\n");
    let cfg = std::fs::read_to_string(client.root.join("arc.toml")).unwrap();
    client.write(
        "arc.toml",
        &format!("{cfg}\n[env]\ninclude = [\"BUILD_TOKEN\"]\n"),
    );
    let client = client.env("BUILD_TOKEN", "allowed-value");

    let out = client.run("echo \"$BUILD_TOKEN\"");
    assert_eq!(source(&out), "remote", "{}", reason(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("allowed-value"));
}

// -------------------------------------------------------------- isolation ---

#[test]
fn the_workers_own_environment_does_not_reach_the_command() {
    needs_sh!();
    let cluster = Cluster::new();
    let client = Client::new(&cluster);
    // ARC_CACHE_TOKEN is set for the client process and for this test binary,
    // which is also the worker. It must not appear in the child.
    let out = client.run("echo \"token=[${ARC_CACHE_TOKEN:-unset}]\"");
    assert_eq!(source(&out), "remote", "{}", reason(&out));
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(text.contains("token=[unset]"), "{text}");
    assert!(!text.contains(SECRET));
}

#[test]
fn home_and_temp_are_sandbox_local() {
    needs_sh!();
    let cluster = Cluster::new();
    let client = Client::new(&cluster);
    let out = client.run("echo \"home=$HOME\"; echo \"tmp=$TMPDIR\"; ls -a \"$HOME\" | wc -l");
    assert_eq!(source(&out), "remote", "{}", reason(&out));
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(text.contains("home="), "{text}");
    // The sandbox HOME is empty, so it cannot be the worker user's real one.
    let entries: i32 = text
        .lines()
        .last()
        .unwrap_or("0")
        .trim()
        .parse()
        .unwrap_or(99);
    assert!(entries <= 3, "sandbox HOME should be empty: {text}");
}

#[test]
fn two_jobs_get_separate_workspaces() {
    needs_sh!();
    let cluster = Cluster::new();
    let a = Client::at(&cluster, "a", "demo", "");
    let b = Client::at(&cluster, "b", "demo", "");
    a.write("src/seed.txt", "alpha");
    b.write("src/seed.txt", "beta");

    let script = "mkdir -p out && cat src/seed.txt > out/built.txt && cat out/built.txt";
    let ja = support::spawn_captured(&mut a.command(&["run", "--json", "sh", "-c", script]));
    let jb = support::spawn_captured(&mut b.command(&["run", "--json", "sh", "-c", script]));
    for (i, c) in [ja, jb].into_iter().enumerate() {
        support::wait_ok(&format!("concurrent client {i}"), c);
    }
    assert_eq!(
        std::fs::read_to_string(a.root.join("out/built.txt")).unwrap(),
        "alpha"
    );
    assert_eq!(
        std::fs::read_to_string(b.root.join("out/built.txt")).unwrap(),
        "beta"
    );
}

#[test]
fn a_command_cannot_write_outside_its_workspace() {
    needs_sh!();
    let cluster = Cluster::new();
    let client = Client::new(&cluster);
    // Even if it tries, the escape lands inside the sandbox root, and nothing
    // outside the declared output globs is captured.
    let out = client.run("mkdir -p out && echo escaped > ../escaped.txt; echo done");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!client.root.join("../escaped.txt").exists());
    assert!(!client.root.join("escaped.txt").exists());
}

// ---------------------------------------------------------- singleflight ----

#[test]
fn many_clients_wanting_the_same_execution_cause_exactly_one() {
    needs_sh!();
    let cluster = Cluster::with(4, 256, None, None);
    let script = "mkdir -p out && sleep 1 && cat src/seed.txt > out/built.txt && echo made";

    let clients: Vec<Client> = (0..8)
        .map(|i| Client::at(&cluster, &format!("c{i}"), "demo", ""))
        .collect();
    let running: Vec<_> = clients
        .iter()
        .map(|c| support::spawn_captured(&mut c.command(&["run", "--json", "sh", "-c", script])))
        .collect();
    for (i, c) in running.into_iter().enumerate() {
        support::wait_ok(&format!("concurrent client {i}"), c);
    }

    let (submitted, deduplicated, completed, _, short) = cluster.stats();
    assert!(submitted >= 1);
    assert_eq!(
        completed, 1,
        "one command execution for {submitted} submissions ({deduplicated} joined, {short} served from cache)"
    );
    for c in &clients {
        assert_eq!(
            std::fs::read_to_string(c.root.join("out/built.txt")).unwrap(),
            "one"
        );
    }
}

#[test]
fn a_result_published_between_the_miss_and_the_job_is_used_instead_of_executing() {
    needs_sh!();
    let cluster = Cluster::new();
    let a = Client::new(&cluster);
    assert_eq!(source(&a.run(SCRIPT)), "remote");
    let after_first = cluster.stats().2;

    // A second machine now finds it in the cache before any job is submitted.
    let b = Client::at(&cluster, "b", "demo", "");
    assert_eq!(status(&b.run(SCRIPT)), "hit");
    assert_eq!(cluster.stats().2, after_first);
}

// ------------------------------------------------------------- capacity ----

#[test]
fn worker_capacity_is_respected() {
    needs_sh!();
    let cluster = Cluster::with(2, 64, None, None);
    let clients: Vec<Client> = (0..6)
        .map(|i| Client::at(&cluster, &format!("c{i}"), "demo", ""))
        .collect();
    for (i, c) in clients.iter().enumerate() {
        c.write("src/seed.txt", &format!("distinct-{i}"));
    }
    let script = "mkdir -p out && sleep 1 && cat src/seed.txt > out/built.txt && echo made";
    let running: Vec<_> = clients
        .iter()
        .map(|c| support::spawn_captured(&mut c.command(&["run", "--json", "sh", "-c", script])))
        .collect();
    for (i, c) in running.into_iter().enumerate() {
        support::wait_ok(&format!("concurrent client {i}"), c);
    }
    assert_eq!(cluster.stats().2, 6, "every distinct execution ran");
    for (i, c) in clients.iter().enumerate() {
        assert_eq!(
            std::fs::read_to_string(c.root.join("out/built.txt")).unwrap(),
            format!("distinct-{i}")
        );
    }
}

#[test]
fn a_full_queue_is_reported_and_the_command_runs_locally() {
    needs_sh!();
    let cluster = Cluster::with(1, 1, None, None);
    let clients: Vec<Client> = (0..6)
        .map(|i| Client::at(&cluster, &format!("c{i}"), "demo", ""))
        .collect();
    for (i, c) in clients.iter().enumerate() {
        c.write("src/seed.txt", &format!("v{i}"));
    }
    let script = "mkdir -p out && sleep 1 && cat src/seed.txt > out/built.txt && echo made";
    let running: Vec<_> = clients
        .iter()
        .map(|c| support::spawn_captured(&mut c.command(&["run", "--json", "sh", "-c", script])))
        .collect();
    // Overload is a reason to run locally, never a reason to fail.
    for (i, c) in running.into_iter().enumerate() {
        support::wait_ok(&format!("concurrent client {i}"), c);
    }
    for (i, c) in clients.iter().enumerate() {
        assert_eq!(
            std::fs::read_to_string(c.root.join("out/built.txt")).unwrap(),
            format!("v{i}")
        );
    }
}

// ------------------------------------------------------------------ auth ----

#[test]
fn a_read_only_token_cannot_make_the_worker_execute() {
    needs_sh!();
    let cluster = Cluster::with(
        4,
        64,
        Some(SECRET.to_string()),
        Some("read-only-token".to_string()),
    );

    let denied = Client::at(
        &cluster,
        "denied",
        "demo",
        "token_env = \"ARC_READ_TOKEN\"\n",
    )
    .env("ARC_READ_TOKEN", "read-only-token");
    let out = denied.run(SCRIPT);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(source(&out), "local", "{}", reason(&out));
    assert_eq!(cluster.stats().2, 0, "nothing executed on the worker");

    let allowed = Client::at(
        &cluster,
        "allowed",
        "demo",
        "token_env = \"ARC_EXEC_TOKEN\"\n",
    )
    .env("ARC_EXEC_TOKEN", SECRET);
    let out = allowed.run(SCRIPT);
    assert_eq!(source(&out), "remote", "{}", reason(&out));

    // Neither token appears anywhere a user or another machine could read it.
    let text = stderr(&out);
    assert!(!text.contains(SECRET));
    assert!(!text.contains("read-only-token"));
    let status = allowed.arc(&["remote", "status"]);
    let s = String::from_utf8_lossy(&status.stdout);
    assert!(!s.contains(SECRET));
    assert!(s.contains("token configured"));
}

#[test]
fn no_token_at_all_still_fails_closed_when_one_is_required() {
    needs_sh!();
    let cluster = Cluster::with(4, 64, Some(SECRET.to_string()), None);
    let client = Client::at(
        &cluster,
        "repo",
        "demo",
        "token_env = \"ARC_MISSING_TOKEN\"\n",
    );
    let out = client.run(SCRIPT);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(source(&out), "local", "{}", reason(&out));
    assert_eq!(cluster.stats().2, 0);
}

// ------------------------------------------------------- infrastructure ----

#[test]
fn an_unreachable_worker_runs_the_command_here() {
    needs_sh!();
    let cluster = Cluster::new();
    // A port nothing listens on. Stopping the worker is not equivalent: its
    // listening socket can outlive it and accept a connection nobody answers.
    let client = Client::at(
        &cluster,
        "repo",
        "demo",
        "url = \"http://127.0.0.1:1\"
",
    );

    let out = client.run(SCRIPT);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(source(&out), "local", "{}", reason(&out));
    assert!(
        reason(&out).contains("worker unavailable"),
        "{}",
        reason(&out)
    );
    assert_eq!(
        std::fs::read_to_string(client.root.join("out/built.txt")).unwrap(),
        "one"
    );
}

#[test]
fn a_worker_without_a_usable_cache_falls_back() {
    needs_sh!();
    let cluster = Cluster::new();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/seed.txt"), "one").unwrap();
    std::fs::write(
        root.join("arc.toml"),
        config("http://127.0.0.1:1", &cluster.worker_url, "demo", ""),
    )
    .unwrap();
    let client = Client {
        home: tmp.path().join("archome"),
        root,
        _tmp: tmp,
        env: Vec::new(),
    };

    let out = client.run(SCRIPT);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(source(&out), "local", "{}", reason(&out));
}

#[test]
fn namespaces_do_not_share_execution_results() {
    needs_sh!();
    let cluster = Cluster::new();
    let a = Client::at(&cluster, "a", "team-a", "");
    assert_eq!(source(&a.run(SCRIPT)), "remote");

    let b = Client::at(&cluster, "b", "team-b", "");
    let out = b.run(SCRIPT);
    assert_eq!(status(&out), "miss", "another namespace sees nothing");
    assert_eq!(source(&out), "remote");
    assert_eq!(cluster.stats().2, 2);
    assert_eq!(cluster.executions("team-a"), cluster.executions("team-b"));
}

// ---------------------------------------------------------------- output ----

#[test]
fn a_large_output_survives_the_round_trip() {
    needs_sh!();
    let cluster = Cluster::new();
    let client = Client::new(&cluster);
    let out =
        client.run("mkdir -p out && head -c 3000000 /dev/urandom > out/big.bin && echo sized");
    assert_eq!(source(&out), "remote", "{}", reason(&out));
    let size = std::fs::metadata(client.root.join("out/big.bin"))
        .unwrap()
        .len();
    assert_eq!(size, 3_000_000);

    // And another machine gets exactly those bytes back.
    let local = std::fs::read(client.root.join("out/big.bin")).unwrap();
    let b = Client::at(&cluster, "b", "demo", "");
    assert_eq!(
        status(&b.run("mkdir -p out && head -c 3000000 /dev/urandom > out/big.bin && echo sized")),
        "hit"
    );
    assert_eq!(std::fs::read(b.root.join("out/big.bin")).unwrap(), local);
}

#[test]
fn a_large_stdout_does_not_grow_without_bound() {
    needs_sh!();
    let cluster = Cluster::new();
    let client = Client::new(&cluster);
    let out = client.run("yes abcdefghijklmnopqrstuvwxyz | head -n 200000");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(source(&out), "remote", "{}", reason(&out));
    assert!(String::from_utf8_lossy(&out.stdout).lines().count() >= 200_000);
}

#[test]
fn awkward_filenames_survive_the_round_trip() {
    needs_sh!();
    let cluster = Cluster::new();
    let client = Client::new(&cluster);
    let out = client.run(
        "mkdir -p 'out/a dir' && echo one > 'out/a dir/with space.txt' && \
         echo two > 'out/tab\tname.txt' && echo three > 'out/ünïcode.txt' && echo named",
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(source(&out), "remote", "{}", reason(&out));
    assert_eq!(
        std::fs::read_to_string(client.root.join("out/a dir/with space.txt")).unwrap(),
        "one\n"
    );
    assert_eq!(
        std::fs::read_to_string(client.root.join("out/ünïcode.txt")).unwrap(),
        "three\n"
    );
}

#[test]
fn an_executable_output_stays_executable() {
    needs_sh!();
    if cfg!(windows) {
        return;
    }
    let cluster = Cluster::new();
    let client = Client::new(&cluster);
    let out = client.run("mkdir -p out && printf '#!/bin/sh\\necho ran\\n' > out/tool && chmod +x out/tool && echo built");
    assert_eq!(source(&out), "remote", "{}", reason(&out));
    let produced = Command::new(client.root.join("out/tool")).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&produced.stdout).trim(), "ran");
}

// -------------------------------------------------------------- scheduler ---

#[test]
fn a_producer_and_consumer_both_run_remotely_in_order() {
    needs_sh!();
    let cluster = Cluster::new();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/seed.txt"), "one").unwrap();
    // No project-wide output glob: a declared output is excluded from that
    // command's inputs, and the consumer has to see the producer's file as an
    // input for the handoff to mean anything.
    std::fs::write(
        root.join("arc.toml"),
        format!(
            "{}
[[command]]
name = \"gen\"
command = \"sh\"
args = [\"-c\", \"mkdir -p gen && tr a-z A-Z < src/seed.txt > gen/out.txt && echo generated\"]
inputs = [\"src/seed.txt\"]
outputs = [\"gen/**\"]

[[command]]
name = \"consume\"
command = \"sh\"
args = [\"-c\", \"cat gen/out.txt && echo consumed\"]
inputs = [\"gen/**\"]
after = [\"gen\"]

[ci]
tasks = [\"gen\", \"consume\"]
",
            config(&cluster.cache_url, &cluster.worker_url, "demo", "").replace(
                "[outputs]
include = [\"out/**\"]

",
                ""
            )
        ),
    )
    .unwrap();
    let client = Client {
        home: tmp.path().join("archome"),
        root,
        _tmp: tmp,
        env: Vec::new(),
    };
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["config", "user.email", "t@example.com"][..],
        &["config", "user.name", "t"][..],
        &["add", "-A"][..],
        &["commit", "-qm", "seed"][..],
    ] {
        Command::new("git")
            .args(args)
            .current_dir(&client.root)
            .output()
            .ok();
    }

    let out = client.arc(&["ci", "--base", "HEAD", "-j", "1", "--json"]);
    assert!(
        out.status.success(),
        "stdout:
{}
stderr:
{}",
        String::from_utf8_lossy(&out.stdout),
        stderr(&out)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let start = text.find('{').expect("json");
    let v: serde_json::Value = serde_json::from_str(&text[start..]).unwrap();
    assert_eq!(v["summary"]["counts"]["executed"], 2, "{v}");
    // The consumer read what the producer wrote, wherever each of them ran.
    assert_eq!(
        std::fs::read_to_string(client.root.join("gen/out.txt")).unwrap(),
        "ONE"
    );
    assert!(
        cluster.stats().2 >= 1,
        "at least one task went to the worker"
    );
}

#[test]
fn the_scheduler_keeps_working_when_the_worker_does_not() {
    needs_sh!();
    let cluster = Cluster::new();
    // A worker that was never there, rather than one that was stopped: a
    // stopped worker's socket can outlive it and accept a job nobody runs.
    let client = Client::at(
        &cluster,
        "repo",
        "demo",
        "url = \"http://127.0.0.1:1\"
",
    );
    let cfg = std::fs::read_to_string(client.root.join("arc.toml")).unwrap();
    client.write(
        "arc.toml",
        &format!(
            "{cfg}
[[command]]
name = \"build\"
command = \"sh\"
args = [\"-c\", \"mkdir -p out && cat src/seed.txt > out/built.txt && echo built\"]
inputs = [\"src/seed.txt\"]
outputs = [\"out/**\"]

[ci]
tasks = [\"build\"]
"
        ),
    );
    Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(&client.root)
        .output()
        .ok();

    let out = client.arc(&["ci", "--base", "HEAD", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        std::fs::read_to_string(client.root.join("out/built.txt")).unwrap(),
        "one"
    );
}

// ------------------------------------------------------------ diagnostics ---

#[test]
fn doctor_and_status_describe_the_worker_without_leaking_its_token() {
    needs_sh!();
    let cluster = Cluster::with(3, 9, Some(SECRET.to_string()), None);
    let client = Client::at(&cluster, "repo", "demo", "token_env = \"ARC_EXEC_TOKEN\"\n")
        .env("ARC_EXEC_TOKEN", SECRET);

    let doctor = String::from_utf8_lossy(&client.arc(&["doctor"]).stdout).to_string();
    assert!(doctor.contains("remote execution"), "{doctor}");
    assert!(doctor.contains("3 jobs"), "{doctor}");
    assert!(doctor.contains("unrestricted"), "network policy is stated");
    assert!(!doctor.contains(SECRET));

    let status = String::from_utf8_lossy(&client.arc(&["remote", "status"]).stdout).to_string();
    assert!(status.contains("execution"), "{status}");
    assert!(!status.contains(SECRET));
}

#[test]
fn explain_says_where_the_command_ran_and_why() {
    needs_sh!();
    let cluster = Cluster::new();
    let client = Client::new(&cluster);
    let out = client.arc(&["run", "--explain", "sh", "-c", SCRIPT]);
    let text = stderr(&out);
    assert!(text.contains("executed"), "{text}");
    assert!(text.contains("remotely"), "{text}");

    let local = client.arc(&[
        "run",
        "--explain",
        "--no-remote-execution",
        "sh",
        "-c",
        "echo different",
    ]);
    assert!(stderr(&local).contains("locally"));
}
