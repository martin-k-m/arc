//! End-to-end tests for the remote cache.
//!
//! Every test runs a real `arc` binary against a real reference server over
//! loopback HTTP. The server is in-process so that a test can make it lie —
//! serve corrupt bytes, truncate a body, fail transiently, or never answer —
//! which is the only way to check that Arc distrusts it.

use arc_cache::{Faults, Options, Server};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const ARC: &str = env!("CARGO_BIN_EXE_arc");
const SECRET: &str = "remote-super-secret-12345";

struct Cache {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    server: Option<Server>,
    url: String,
}

impl Cache {
    fn new() -> Cache {
        Cache::with(Faults::default(), None)
    }

    fn with(faults: Faults, token: Option<String>) -> Cache {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("data");
        let server = Server::start(Options {
            data: dir.clone(),
            addr: "127.0.0.1:0".into(),
            token,
            threads: 8,
            faults,
            log: false,
        })
        .unwrap();
        Cache {
            url: server.url(),
            server: Some(server),
            dir,
            _tmp: tmp,
        }
    }

    fn stop(&mut self) {
        self.server = None;
    }

    /// Restart on the same data directory, at a new port. Callers that need the
    /// old URL to keep working must not use this.
    fn restart(&mut self) {
        self.stop();
        let server = Server::start(Options {
            data: self.dir.clone(),
            addr: "127.0.0.1:0".into(),
            ..Default::default()
        })
        .unwrap();
        self.url = server.url();
        self.server = Some(server);
    }

    fn objects(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let root = self.dir.join("objects");
        for shard in std::fs::read_dir(&root).into_iter().flatten().flatten() {
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

    fn record_path(&self, ns: &str, key: &str) -> PathBuf {
        self.dir
            .join("executions")
            .join(ns)
            .join(format!("{key}.json"))
    }

    /// A result is published under every key it is valid for, so a test that
    /// tampers with remote metadata must tamper with all of them.
    fn corrupt_records(&self, ns: &str, mutate: impl Fn(&mut serde_json::Value)) {
        let dir = self.dir.join("executions").join(ns);
        let mut found = 0;
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let mut rec: serde_json::Value =
                serde_json::from_slice(&std::fs::read(e.path()).unwrap()).unwrap();
            mutate(&mut rec);
            std::fs::write(e.path(), serde_json::to_vec(&rec).unwrap()).unwrap();
            found += 1;
        }
        assert!(found > 0, "no records published under {ns}");
    }
}

struct Client {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    env: Vec<(String, String)>,
}

impl Client {
    fn new(cache: &Cache, namespace: &str) -> Client {
        Client::at(cache, namespace, "repo")
    }

    /// A checkout at a specific directory name, so relocation can be tested.
    fn at(cache: &Cache, namespace: &str, dir: &str) -> Client {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(dir);
        std::fs::create_dir_all(root.join("src")).unwrap();
        let c = Client {
            home: tmp.path().join("archome"),
            root,
            _tmp: tmp,
            env: Vec::new(),
        };
        c.write(
            "arc.toml",
            &format!(
                "[outputs]\ninclude = [\"out/**\"]\n\n[remote]\nurl = \"{}\"\nnamespace = \"{namespace}\"\n",
                cache.url
            ),
        );
        c.write("src/seed.txt", "one");
        c
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
            .env("ARC_CACHE_TOKEN", SECRET);
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

/// The `--json` line `arc run` writes to stderr.
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
    result(out)["cache"]["source"].as_str().unwrap().to_string()
}

fn status(out: &Output) -> String {
    result(out)["cache"]["status"].as_str().unwrap().to_string()
}

fn key(out: &Output) -> String {
    result(out)["key"].as_str().unwrap().to_string()
}

/// The key this run computed before it executed — what a machine that has never
/// run this command will ask for.
fn execution_key(out: &Output) -> String {
    result(out)["explain"]["execution_key"]
        .as_str()
        .unwrap()
        .to_string()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

const SCRIPT: &str = "mkdir -p out && cat src/seed.txt > out/built.txt && echo made";

// ---------------------------------------------------------------------------
// The shared-cache path
// ---------------------------------------------------------------------------

#[test]
fn a_result_produced_on_one_machine_is_reused_on_another() {
    let cache = Cache::new();
    let a = Client::new(&cache, "demo");
    let b = Client::new(&cache, "demo");

    let first = a.run(SCRIPT);
    assert!(first.status.success(), "{}", stderr(&first));
    assert_eq!(status(&first), "miss");

    let second = b.run(SCRIPT);
    assert_eq!(status(&second), "hit", "{}", stderr(&second));
    assert_eq!(source(&second), "remote");
    // The output bytes, not merely the exit status, must survive the trip.
    assert_eq!(
        std::fs::read_to_string(b.root.join("out/built.txt")).unwrap(),
        "one"
    );
    assert!(String::from_utf8_lossy(&second.stdout).contains("made"));
}

#[test]
fn a_remote_hit_promotes_the_result_into_the_local_cache() {
    let mut cache = Cache::new();
    let a = Client::new(&cache, "demo");
    let b = Client::new(&cache, "demo");
    a.run(SCRIPT);
    assert_eq!(source(&b.run(SCRIPT)), "remote");

    cache.stop();
    let offline = b.run(SCRIPT);
    assert_eq!(status(&offline), "hit", "{}", stderr(&offline));
    assert_eq!(source(&offline), "local");
    // A local hit must not have gone looking for the server at all.
    assert_eq!(result(&offline)["remote"]["metrics"]["requests"], 0);
}

#[test]
fn a_local_hit_never_contacts_the_remote() {
    let cache = Cache::new();
    let a = Client::new(&cache, "demo");
    a.run(SCRIPT);
    let again = a.run(SCRIPT);
    assert_eq!(source(&again), "local");
    assert_eq!(result(&again)["remote"]["metrics"]["requests"], 0);
}

#[test]
fn an_empty_remote_is_a_miss_that_uploads() {
    let cache = Cache::new();
    let a = Client::new(&cache, "demo");
    let out = a.run(SCRIPT);
    assert_eq!(status(&out), "miss");
    assert!(!cache.objects().is_empty());
    assert!(cache.record_path("demo", &key(&out)).is_file());
    assert!(cache.record_path("demo", &execution_key(&out)).is_file());
    assert!(
        result(&out)["remote"]["metrics"]["objects_uploaded"]
            .as_u64()
            .unwrap()
            > 0
    );
}

#[test]
fn the_same_project_at_a_different_path_still_hits() {
    let cache = Cache::new();
    let a = Client::at(&cache, "demo", "checkout-a");
    let b = Client::at(&cache, "demo", "somewhere/else/checkout-b");
    a.run(SCRIPT);
    let out = b.run(SCRIPT);
    assert_eq!(source(&out), "remote", "{}", stderr(&out));
    assert_eq!(
        std::fs::read_to_string(b.root.join("out/built.txt")).unwrap(),
        "one"
    );
}

#[test]
fn a_different_input_does_not_hit() {
    let cache = Cache::new();
    let a = Client::new(&cache, "demo");
    let b = Client::new(&cache, "demo");
    a.run(SCRIPT);
    b.write("src/seed.txt", "two");
    assert_eq!(status(&b.run(SCRIPT)), "miss");
}

// ---------------------------------------------------------------------------
// Failing toward local execution
// ---------------------------------------------------------------------------

#[test]
fn an_unreachable_remote_does_not_break_the_command() {
    let mut cache = Cache::new();
    let a = Client::new(&cache, "demo");
    cache.stop();
    let started = std::time::Instant::now();
    let out = a.run(SCRIPT);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(status(&out), "miss");
    assert!(
        started.elapsed().as_secs() < 20,
        "an unreachable remote should not stall the build"
    );
}

#[test]
fn a_remote_that_never_answers_times_out_and_falls_back() {
    let cache = Cache::with(
        Faults {
            hang: true,
            ..Default::default()
        },
        None,
    );
    let a = Client::new(&cache, "demo").env("ARC_REMOTE_TIMEOUT_MS", "400");
    let started = std::time::Instant::now();
    let out = a.run(SCRIPT);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(started.elapsed().as_secs() < 30);
}

#[test]
fn a_corrupt_remote_object_is_refused_and_the_command_runs() {
    let source_cache = Cache::new();
    let a = Client::new(&source_cache, "demo");
    a.run(SCRIPT);

    // Same data, served by a server that flips a byte on the way out.
    let evil = Cache::with(
        Faults {
            corrupt_objects: true,
            ..Default::default()
        },
        None,
    );
    copy_dir(&source_cache.dir, &evil.dir);
    let b = Client::new(&evil, "demo");
    let out = b.run(SCRIPT);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(status(&out), "miss");
    // Nothing unverified may be left behind.
    assert!(b.arc(&["cache", "verify"]).status.success());
    assert!(!stderr(&out).contains("panic"));
}

#[test]
fn a_truncated_remote_object_is_refused() {
    let source_cache = Cache::new();
    let a = Client::new(&source_cache, "demo");
    a.write("src/seed.txt", &"payload ".repeat(4096));
    a.run(SCRIPT);

    let evil = Cache::with(
        Faults {
            truncate_objects: true,
            ..Default::default()
        },
        None,
    );
    copy_dir(&source_cache.dir, &evil.dir);
    let b = Client::new(&evil, "demo");
    b.write("src/seed.txt", &"payload ".repeat(4096));
    let out = b.run(SCRIPT);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(status(&out), "miss");
    let tmp = b.home.join("store/tmp");
    assert_eq!(
        std::fs::read_dir(&tmp).into_iter().flatten().count(),
        0,
        "a failed transfer must not leave a temporary file behind"
    );
}

#[test]
fn a_record_whose_objects_are_missing_falls_back() {
    let cache = Cache::new();
    let a = Client::new(&cache, "demo");
    a.run(SCRIPT);
    for p in cache.objects() {
        std::fs::remove_file(p).unwrap();
    }
    let b = Client::new(&cache, "demo");
    let out = b.run(SCRIPT);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(status(&out), "miss");
    assert!(!b.root.join("out/built.txt").exists() || out.status.success());
}

#[test]
fn a_server_object_corrupted_on_disk_is_detected_by_the_client() {
    let cache = Cache::new();
    let a = Client::new(&cache, "demo");
    a.write("src/seed.txt", &"x".repeat(20_000));
    a.run(SCRIPT);
    for p in cache.objects() {
        std::fs::write(&p, b"replaced").unwrap();
    }
    let b = Client::new(&cache, "demo");
    b.write("src/seed.txt", &"x".repeat(20_000));
    let out = b.run(SCRIPT);
    assert_eq!(status(&out), "miss", "{}", stderr(&out));
}

#[test]
fn malformed_remote_metadata_is_rejected_without_panicking() {
    for (name, mutate) in mutations() {
        let cache = Cache::new();
        let a = Client::new(&cache, "demo");
        a.run(SCRIPT);
        cache.corrupt_records("demo", mutate);

        let b = Client::new(&cache, "demo");
        let out = b.run(SCRIPT);
        assert!(out.status.success(), "{name}: {}", stderr(&out));
        assert_eq!(status(&out), "miss", "{name} should not have been replayed");
        assert!(!stderr(&out).contains("panicked"), "{name} panicked");
    }
}

type Mutation = (&'static str, fn(&mut serde_json::Value));

fn mutations() -> Vec<Mutation> {
    vec![
        ("traversal", |r| {
            r["outputs"][0]["path"]["v"] = "../../evil.txt".into();
        }),
        ("absolute path", |r| {
            r["outputs"][0]["path"]["v"] = "/tmp/evil.txt".into();
        }),
        ("unsupported protocol", |r| {
            r["protocol"] = serde_json::json!(999);
        }),
        ("stale key semantics", |r| {
            r["key_semantics"] = serde_json::json!(arc_core::SCHEMA_VERSION - 1);
        }),
        ("foreign platform", |r| {
            r["os"] = "plan9".into();
        }),
        ("foreign architecture", |r| {
            r["arch"] = "s390x".into();
        }),
        ("mismatched key", |r| {
            r["execution_key"] = "a".repeat(64).into();
        }),
        ("malformed digest", |r| {
            r["outputs"][0]["digest"] = "not-a-digest".into();
        }),
        ("wrong type", |r| {
            r["exit_code"] = "zero".into();
        }),
        ("missing field", |r| {
            r.as_object_mut().unwrap().remove("outputs");
        }),
    ]
}

#[test]
fn a_remote_record_cannot_write_outside_the_project() {
    let cache = Cache::new();
    let a = Client::new(&cache, "demo");
    a.run(SCRIPT);
    cache.corrupt_records("demo", |r| {
        r["outputs"][0]["path"]["v"] = "../../escaped.txt".into();
    });

    let b = Client::new(&cache, "demo");
    let out = b.run(SCRIPT);
    assert!(out.status.success(), "{}", stderr(&out));
    let escaped = b
        .root
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("escaped.txt");
    assert!(!escaped.exists(), "a remote record escaped the project");
}

// ---------------------------------------------------------------------------
// Repair
// ---------------------------------------------------------------------------

#[test]
fn a_local_record_missing_its_objects_is_repaired_from_the_remote() {
    let cache = Cache::new();
    let a = Client::new(&cache, "demo");
    a.run(SCRIPT);
    // Lose the local objects but keep the metadata, as an over-eager cleanup
    // would.
    let blobs = a.home.join("store/blobs");
    std::fs::remove_dir_all(&blobs).unwrap();
    std::fs::create_dir_all(&blobs).unwrap();
    std::fs::remove_file(a.root.join("out/built.txt")).unwrap();

    let out = a.run(SCRIPT);
    assert_eq!(status(&out), "hit", "{}", stderr(&out));
    assert_eq!(source(&out), "remote");
    assert_eq!(
        std::fs::read_to_string(a.root.join("out/built.txt")).unwrap(),
        "one"
    );
}

// ---------------------------------------------------------------------------
// Modes, namespaces and authentication
// ---------------------------------------------------------------------------

#[test]
fn read_only_clients_hit_but_never_upload() {
    let cache = Cache::new();
    let a = Client::new(&cache, "demo");
    a.run(SCRIPT);
    let published = cache.objects().len();

    let b = Client::new(&cache, "demo").env("ARC_REMOTE_WRITE", "0");
    assert_eq!(source(&b.run(SCRIPT)), "remote");

    let c = Client::new(&cache, "demo").env("ARC_REMOTE_WRITE", "0");
    c.write("src/seed.txt", "unpublished");
    assert_eq!(status(&c.run(SCRIPT)), "miss");
    assert_eq!(cache.objects().len(), published);
}

#[test]
fn write_only_clients_upload_but_never_look() {
    let cache = Cache::new();
    let a = Client::new(&cache, "demo");
    a.run(SCRIPT);
    let b = Client::new(&cache, "demo").env("ARC_REMOTE_READ", "0");
    let out = b.run(SCRIPT);
    assert_eq!(status(&out), "miss");
    assert_eq!(result(&out)["remote"]["read"], false);
}

#[test]
fn a_disabled_remote_makes_no_requests() {
    let cache = Cache::new();
    let a = Client::new(&cache, "demo");
    a.run(SCRIPT);
    let b = Client::new(&cache, "demo").env("ARC_REMOTE_ENABLED", "0");
    let out = b.run(SCRIPT);
    assert_eq!(status(&out), "miss");
    assert!(result(&out)["remote"].is_null());

    let c = Client::new(&cache, "demo");
    let out = c.arc(&["run", "--json", "--no-remote", "sh", "-c", SCRIPT]);
    assert_eq!(status(&out), "miss");
    assert!(result(&out)["remote"].is_null());
}

#[test]
fn namespaces_isolate_execution_records() {
    let cache = Cache::new();
    let a = Client::new(&cache, "team-a");
    a.run(SCRIPT);
    // Same namespace: shared.
    let b = Client::new(&cache, "team-a");
    assert_eq!(source(&b.run(SCRIPT)), "remote");
    // Different namespace: the record is invisible even though the execution
    // key and the objects are identical.
    let c = Client::new(&cache, "team-b");
    assert_eq!(status(&c.run(SCRIPT)), "miss");
}

#[test]
fn objects_are_deduplicated_across_namespaces() {
    let cache = Cache::new();
    let a = Client::new(&cache, "team-a");
    a.run(SCRIPT);
    let before = cache.objects().len();
    let b = Client::new(&cache, "team-b");
    b.run(SCRIPT);
    assert_eq!(
        cache.objects().len(),
        before,
        "identical content should be stored once"
    );
    assert!(cache.record_path("team-a", &key(&a.run(SCRIPT))).is_file());
}

#[test]
fn a_correct_token_works_and_a_wrong_one_falls_back_without_leaking() {
    let cache = Cache::with(Faults::default(), Some(SECRET.to_string()));
    let a = Client::new(&cache, "demo");
    a.write(
        "arc.toml",
        &format!(
            "[outputs]\ninclude = [\"out/**\"]\n\n[remote]\nurl = \"{}\"\nnamespace = \"demo\"\ntoken_env = \"ARC_CACHE_TOKEN\"\n",
            cache.url
        ),
    );
    let out = a.run(SCRIPT);
    assert_eq!(status(&out), "miss");
    assert!(
        result(&out)["remote"]["error"].is_null(),
        "{}",
        stderr(&out)
    );
    assert!(!cache.objects().is_empty());

    let b = Client::new(&cache, "demo").env("ARC_CACHE_TOKEN", "wrong-token");
    b.write(
        "arc.toml",
        &format!(
            "[outputs]\ninclude = [\"out/**\"]\n\n[remote]\nurl = \"{}\"\nnamespace = \"demo\"\ntoken_env = \"ARC_CACHE_TOKEN\"\n",
            cache.url
        ),
    );
    let out = b.run(SCRIPT);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(status(&out), "miss");
    let text = stderr(&out);
    assert!(!text.contains("wrong-token"));
    assert!(!text.contains(SECRET));
}

#[test]
fn a_token_never_reaches_disk_or_output() {
    let cache = Cache::with(Faults::default(), Some(SECRET.to_string()));
    let a = Client::new(&cache, "demo");
    a.write(
        "arc.toml",
        &format!(
            "[outputs]\ninclude = [\"out/**\"]\n\n[remote]\nurl = \"{}\"\nnamespace = \"demo\"\ntoken_env = \"ARC_CACHE_TOKEN\"\n",
            cache.url
        ),
    );
    a.run(SCRIPT);
    for extra in [
        vec!["history", "--json"],
        vec!["remote", "status", "--json"],
        vec!["doctor"],
        vec!["config", "show"],
        vec!["cache", "stats"],
    ] {
        let out = a.arc(&extra);
        let text = format!("{}{}", stderr(&out), String::from_utf8_lossy(&out.stdout));
        assert!(!text.contains(SECRET), "{extra:?} leaked the token");
    }
    for dir in [&a.home, &cache.dir, &a.root] {
        assert!(!contains_secret(dir), "{} holds the token", dir.display());
    }
}

fn contains_secret(dir: &Path) -> bool {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        let hit = if p.is_dir() {
            contains_secret(&p)
        } else {
            std::fs::read(&p)
                .map(|b| find(&b, SECRET.as_bytes()))
                .unwrap_or(false)
        };
        if hit {
            return true;
        }
    }
    false
}

fn find(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

// ---------------------------------------------------------------------------
// Protocol behaviour
// ---------------------------------------------------------------------------

#[test]
fn transient_failures_are_retried_and_auth_failures_are_not() {
    let cache = Cache::with(
        Faults {
            fail_first: 2,
            ..Default::default()
        },
        None,
    );
    let a = Client::new(&cache, "demo");
    let out = a.run(SCRIPT);
    assert!(out.status.success(), "{}", stderr(&out));
    // Two 503s were absorbed, so the upload still happened.
    assert!(!cache.objects().is_empty());

    let guarded = Cache::with(Faults::default(), Some("expected".into()));
    let b = Client::new(&guarded, "demo").env("ARC_CACHE_TOKEN", "wrong");
    b.write(
        "arc.toml",
        &format!(
            "[outputs]\ninclude = [\"out/**\"]\n\n[remote]\nurl = \"{}\"\nnamespace = \"demo\"\ntoken_env = \"ARC_CACHE_TOKEN\"\n",
            guarded.url
        ),
    );
    let out = b.run(SCRIPT);
    assert!(out.status.success());
    // One lookup, one publish attempt: a 401 is never retried.
    assert!(
        result(&out)["remote"]["metrics"]["requests"]
            .as_u64()
            .unwrap()
            <= 4
    );
}

#[test]
fn a_large_object_survives_the_round_trip() {
    let cache = Cache::new();
    let a = Client::new(&cache, "demo");
    let big = "abcdefgh".repeat(2_000_000); // 16 MB
    a.write("src/seed.txt", &big);
    let out = a.run(SCRIPT);
    assert!(out.status.success(), "{}", stderr(&out));

    let b = Client::new(&cache, "demo");
    b.write("src/seed.txt", &big);
    let out = b.run(SCRIPT);
    assert_eq!(source(&out), "remote", "{}", stderr(&out));
    assert_eq!(
        std::fs::read(b.root.join("out/built.txt")).unwrap().len(),
        big.len()
    );
}

#[test]
fn compressible_content_is_compressed_and_restored_exactly() {
    let cache = Cache::new();
    let a = Client::new(&cache, "demo");
    let body = "the same line over and over\n".repeat(4000);
    a.write("src/seed.txt", &body);
    let out = a.run(SCRIPT);
    let m = &result(&out)["remote"]["metrics"];
    assert!(
        m["wire_bytes_uploaded"].as_u64().unwrap() < m["bytes_uploaded"].as_u64().unwrap(),
        "compressible data should shrink on the wire: {m}"
    );

    let b = Client::new(&cache, "demo");
    b.write("src/seed.txt", &body);
    let out = b.run(SCRIPT);
    assert_eq!(source(&out), "remote");
    assert_eq!(
        std::fs::read_to_string(b.root.join("out/built.txt")).unwrap(),
        body
    );
}

#[test]
fn tiny_objects_round_trip_unharmed() {
    let cache = Cache::new();
    let a = Client::new(&cache, "demo");
    a.write("src/seed.txt", "x");
    a.run(SCRIPT);
    let b = Client::new(&cache, "demo");
    b.write("src/seed.txt", "x");
    assert_eq!(source(&b.run(SCRIPT)), "remote");
    assert_eq!(
        std::fs::read_to_string(b.root.join("out/built.txt")).unwrap(),
        "x"
    );
}

#[test]
fn many_objects_are_negotiated_in_few_requests() {
    let cache = Cache::new();
    let a = Client::new(&cache, "demo");
    let script =
        "mkdir -p out && for i in $(seq 1 200); do cp src/seed.txt out/$i.txt; done && echo done";
    let out = a.arc(&["run", "--json", "sh", "-c", script]);
    assert!(out.status.success(), "{}", stderr(&out));
    // 200 outputs, but they are all the same content, so one object.
    let requests = result(&out)["remote"]["metrics"]["requests"]
        .as_u64()
        .unwrap();
    assert!(requests < 20, "{requests} requests for one distinct object");

    let b = Client::new(&cache, "demo");
    let out = b.arc(&["run", "--json", "sh", "-c", script]);
    assert_eq!(source(&out), "remote", "{}", stderr(&out));
    let requests = result(&out)["remote"]["metrics"]["requests"]
        .as_u64()
        .unwrap();
    assert!(requests < 20, "{requests} requests for one remote hit");
    assert_eq!(std::fs::read_dir(b.root.join("out")).unwrap().count(), 200);
}

#[test]
fn uploading_a_shared_object_twice_does_not_duplicate_it() {
    let cache = Cache::new();
    let a = Client::new(&cache, "demo");
    a.run("mkdir -p out && cp src/seed.txt out/one.txt");
    let after_first = cache.objects().len();
    let b = Client::new(&cache, "demo");
    b.run("mkdir -p out && cp src/seed.txt out/two.txt");
    // Different execution, same content: no new object for the payload.
    assert!(cache.objects().len() <= after_first + 2);
}

#[test]
fn concurrent_clients_do_not_corrupt_the_cache() {
    let cache = Cache::new();
    let clients: Vec<Client> = (0..4).map(|_| Client::new(&cache, "demo")).collect();
    let mut running: Vec<_> = clients
        .iter()
        .map(|c| {
            c.command(&["run", "--json", "sh", "-c", SCRIPT])
                .spawn()
                .unwrap()
        })
        .collect();
    for child in &mut running {
        let out = child.wait_with_output_ref();
        assert!(out.status.success(), "{}", stderr(&out));
    }
    // Whatever raced, the stored objects must still hash to their names.
    for p in cache.objects() {
        let name = format!(
            "{}{}",
            p.parent().unwrap().file_name().unwrap().to_string_lossy(),
            p.file_name().unwrap().to_string_lossy()
        );
        let bytes = std::fs::read(&p).unwrap();
        assert_eq!(arc_core::hash::hash_bytes(&bytes).hex(), name);
    }
    let fresh = Client::new(&cache, "demo");
    assert_eq!(source(&fresh.run(SCRIPT)), "remote");
}

/// `wait_with_output` consumes the child; these tests only need to wait.
trait WaitOutput {
    fn wait_with_output_ref(&mut self) -> Output;
}

impl WaitOutput for std::process::Child {
    fn wait_with_output_ref(&mut self) -> Output {
        use std::io::Read;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        if let Some(mut o) = self.stdout.take() {
            let _ = o.read_to_end(&mut stdout);
        }
        if let Some(mut e) = self.stderr.take() {
            let _ = e.read_to_end(&mut stderr);
        }
        let status = self.wait().unwrap();
        Output {
            status,
            stdout,
            stderr,
        }
    }
}

#[test]
fn a_server_restart_preserves_its_cache() {
    let mut cache = Cache::new();
    let a = Client::new(&cache, "demo");
    // Built before the restart so both checkouts hold identical configuration:
    // `arc.toml` is itself an input, and a rewritten URL would change the key.
    let b = Client::new(&cache, "demo");
    a.run(SCRIPT);
    cache.restart();

    let b = b.env("ARC_REMOTE_URL", &cache.url);
    let out = b.run(SCRIPT);
    assert_eq!(source(&out), "remote", "{}", stderr(&out));
}

#[test]
fn a_cancelled_transfer_leaves_no_object_behind() {
    let cache = Cache::with(
        Faults {
            delay_ms: 400,
            ..Default::default()
        },
        None,
    );
    let a = Client::new(&cache, "demo");
    let mut child = a
        .command(&["run", "--json", "sh", "-c", SCRIPT])
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(250));
    let _ = child.kill();
    let _ = child.wait();

    assert!(a.arc(&["cache", "verify"]).status.success());
    let out = a.run(SCRIPT);
    assert!(out.status.success(), "{}", stderr(&out));
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

#[test]
fn remote_status_reports_configuration_without_secrets() {
    let cache = Cache::new();
    let a = Client::new(&cache, "demo");
    let out = a.arc(&["remote", "status", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["configured"], true);
    assert_eq!(v["namespace"], "demo");
    assert_eq!(v["reachable"], true);
    assert_eq!(v["protocol"], 1);
    assert_eq!(v["auth"], false);

    let text = String::from_utf8_lossy(&a.arc(&["remote", "status"]).stdout).to_string();
    assert!(text.contains("demo"));
    let ping = a.arc(&["remote", "ping"]);
    assert!(ping.status.success(), "{}", stderr(&ping));
}

#[test]
fn doctor_reports_an_unreachable_remote_without_failing() {
    let cache = Cache::new();
    // Port 1 is reserved and never listening, which is a more reliable
    // "unreachable" than a stopped server whose socket may still be draining.
    let a = Client::new(&cache, "demo").env("ARC_REMOTE_URL", "http://127.0.0.1:1");
    let out = a.arc(&["doctor"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("remote cache"));
    assert!(text.contains("local cache still functional"), "{text}");
}

#[test]
fn an_unconfigured_project_reports_local_only() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("arc.toml"), "[cache]\nenabled = true\n").unwrap();
    let out = Command::new(ARC)
        .args(["remote", "status", "--json"])
        .current_dir(&root)
        .env("ARC_HOME", tmp.path().join("home"))
        .env("ARC_NO_ANIM", "1")
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["configured"], false);
    assert_eq!(v["reason"], "no [remote] url configured");
}

#[test]
fn inspect_reports_where_a_hit_came_from() {
    let cache = Cache::new();
    let a = Client::new(&cache, "demo");
    a.run(SCRIPT);
    let b = Client::new(&cache, "demo");
    let out = b.run(SCRIPT);
    let id = result(&out)["id"].as_str().unwrap().to_string();
    let text = String::from_utf8_lossy(&b.arc(&["inspect", &id]).stdout).to_string();
    assert!(text.contains("remote"), "{text}");
}

// ---------------------------------------------------------------------------
// The scheduler
// ---------------------------------------------------------------------------

#[test]
fn the_scheduler_gets_remote_hits_transparently() {
    let cache = Cache::new();
    let config = |url: &str| {
        format!(
            "[remote]\nurl = \"{url}\"\nnamespace = \"demo\"\n\n\
             [[command]]\nmatch = \"*out/api*\"\nname = \"build-api\"\ninputs = [\"src/**\"]\noutputs = [\"out/api\"]\n\n\
             [[command]]\nmatch = \"*out/web*\"\nname = \"build-web\"\ninputs = [\"src/**\"]\noutputs = [\"out/web\"]\n"
        )
    };
    let tasks = [
        "mkdir -p out && cat src/seed.txt > out/api",
        "mkdir -p out && cat src/seed.txt > out/web",
    ];
    let a = Client::new(&cache, "demo");
    a.write("arc.toml", &config(&cache.url));
    init_git(&a.root);

    let b = Client::new(&cache, "demo");
    b.write("arc.toml", &config(&cache.url));
    init_git(&b.root);

    // Both machines learn the tasks. B does so offline, so its knowledge of the
    // graph is local while its knowledge of results is not.
    for script in tasks {
        for _ in 0..2 {
            assert!(a.arc(&["run", "sh", "-c", script]).status.success());
            assert!(b
                .arc(&["run", "--no-remote", "sh", "-c", script])
                .status
                .success());
        }
    }

    // The same edit on both machines. A rebuilds and publishes; B has never
    // built this content and must not have to.
    for c in [&a, &b] {
        c.write("src/seed.txt", "changed");
    }
    for script in tasks {
        assert!(a.arc(&["run", "sh", "-c", script]).status.success());
    }

    let out = b.arc(&["affected", "--run", "--json", "--jobs", "4"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let start = text.find('{').unwrap_or(0);
    let v: serde_json::Value =
        serde_json::from_str(&text[start..]).unwrap_or(serde_json::json!({}));
    assert_eq!(
        v["hits"], 2,
        "the scheduler should have hit remotely: {text}"
    );
    assert_eq!(v["ran"], 0, "{text}");
    assert_eq!(
        std::fs::read_to_string(b.root.join("out/api")).unwrap(),
        "changed"
    );
    assert!(b.arc(&["cache", "verify"]).status.success());
}

fn init_git(root: &Path) {
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("running git");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "test"]);
    git(&["add", "-A"]);
    git(&["commit", "-qm", "snapshot"]);
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).into_iter().flatten().flatten() {
        let dest = to.join(e.file_name());
        if e.path().is_dir() {
            copy_dir(&e.path(), &dest);
        } else {
            std::fs::copy(e.path(), &dest).unwrap();
        }
    }
}
