//! End-to-end tests for `arc ci`.
//!
//! Every test drives the real binary against a real Git repository, and the
//! remote ones against a real reference server. Tasks are shell one-liners so
//! the same repository shape works everywhere; Windows has no `sh`, so those
//! tests skip there and the provider, escaping and summary logic is covered by
//! unit tests instead.

use arc_cache::{Options, Server};
use std::path::PathBuf;
use std::process::{Command, Output};

const ARC: &str = env!("CARGO_BIN_EXE_arc");

fn have_tools() -> bool {
    Command::new("sh").arg("-c").arg("exit 0").output().is_ok()
        && Command::new("git").arg("--version").output().is_ok()
}

macro_rules! needs_sh {
    () => {
        if !have_tools() {
            return;
        }
    };
}

/// Three tasks: a generator, a test that consumes what it produces, and a test
/// that has nothing to do with either.
const CONFIG: &str = r#"
[[command]]
name = "gen"
command = "sh"
args = ["-c", "mkdir -p generated && cat src/schema.txt > generated/client.txt && echo generated"]
inputs = ["src/schema.txt"]
outputs = ["generated/**"]

[[command]]
name = "test-api"
command = "sh"
args = ["-c", "cat generated/client.txt > /dev/null && echo api-ok"]
inputs = ["generated/client.txt"]
after = ["gen"]

[[command]]
name = "test-web"
command = "sh"
args = ["-c", "cat src/web.txt > /dev/null && echo web-ok"]
inputs = ["src/web.txt"]

[ci]
tasks = ["gen", "test-api", "test-web"]
"#;

struct Repo {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    env: Vec<(String, String)>,
}

impl Repo {
    fn new(config: &str) -> Repo {
        Repo::at("repo", config, None)
    }

    fn at(dir: &str, config: &str, remote: Option<(&str, &str)>) -> Repo {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(dir);
        std::fs::create_dir_all(root.join("src")).unwrap();
        let repo = Repo {
            home: tmp.path().join("archome"),
            root,
            _tmp: tmp,
            env: Vec::new(),
        };
        let remote_block = match remote {
            Some((url, ns)) => {
                format!("\n[remote]\nurl = \"{url}\"\nnamespace = \"{ns}\"\n")
            }
            None => String::new(),
        };
        repo.write("arc.toml", &format!("{config}{remote_block}"));
        repo.write("src/schema.txt", "v1");
        repo.write("src/web.txt", "web");
        repo.write("docs/readme.md", "hello");
        // Generated files are outputs, not sources. Committing them would make
        // every build show up as a change to the next diff.
        repo.write(
            ".gitignore",
            "generated/
out/
",
        );
        repo.git(&["init", "-q", "-b", "main"]);
        repo.git(&["config", "user.email", "t@example.com"]);
        repo.git(&["config", "user.name", "test"]);
        repo.git(&["config", "commit.gpgsign", "false"]);
        repo.commit();
        repo
    }

    fn env(mut self, k: &str, v: &str) -> Repo {
        self.env.push((k.into(), v.into()));
        self
    }

    fn write(&self, rel: &str, body: &str) {
        let p = self.root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn git(&self, args: &[&str]) -> Output {
        Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .output()
            .expect("running git")
    }

    fn commit(&self) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-qm", "snapshot", "--no-gpg-sign"]);
        self.head()
    }

    fn head(&self) -> String {
        String::from_utf8_lossy(&self.git(&["rev-parse", "HEAD"]).stdout)
            .trim()
            .to_string()
    }

    fn arc(&self, args: &[&str]) -> Output {
        let mut c = Command::new(ARC);
        c.args(args)
            .current_dir(&self.root)
            .env("ARC_HOME", &self.home)
            .env("ARC_NO_ANIM", "1");
        for (k, v) in &self.env {
            c.env(k, v);
        }
        c.output().expect("running arc")
    }

    fn ci(&self, extra: &[&str]) -> serde_json::Value {
        let mut args = vec!["ci", "--json"];
        args.extend_from_slice(extra);
        let out = self.arc(&args);
        parse(&out)
    }

    /// Populate the graph, then settle it.
    ///
    /// Two passes because the first creates generated files that are themselves
    /// inputs to later tasks. Serially because a snapshot-based tracer sees the
    /// whole project: two tasks writing at once can each be credited with the
    /// other's output, which is safe — it only ever adds edges — but makes what
    /// Arc learns depend on scheduling.
    fn warm(&self) {
        for _ in 0..2 {
            let out = self.arc(&["ci", "--base", "HEAD", "-j", "1", "--json"]);
            assert!(
                out.status.success(),
                "warm failed:
{}
{}",
                String::from_utf8_lossy(&out.stdout),
                stderr(&out)
            );
        }
    }
}

fn parse(out: &Output) -> serde_json::Value {
    let text = String::from_utf8_lossy(&out.stdout);
    let start = text
        .find('{')
        .unwrap_or_else(|| panic!("no json in:\nstdout: {text}\nstderr: {}", stderr(out)));
    serde_json::from_str(&text[start..]).expect("parsing arc ci --json")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// `name -> outcome` for every task the run reported on.
fn outcomes(v: &serde_json::Value) -> std::collections::BTreeMap<String, String> {
    v["summary"]["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["name"].as_str().unwrap().to_string(),
                t["outcome"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn selected(v: &serde_json::Value) -> Vec<String> {
    v["analysis"]["selected"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

fn counts(v: &serde_json::Value) -> serde_json::Value {
    v["summary"]["counts"].clone()
}

struct Cache {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    server: Option<Server>,
    url: String,
}

impl Cache {
    fn new() -> Cache {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("data");
        let server = Server::start(Options {
            data: dir.clone(),
            addr: "127.0.0.1:0".into(),
            ..Default::default()
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

    fn tasks(&self, ns: &str) -> Vec<PathBuf> {
        std::fs::read_dir(self.dir.join("tasks").join(ns))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .collect()
    }

    fn mutate_tasks(&self, ns: &str, f: impl Fn(&mut serde_json::Value)) {
        let files = self.tasks(ns);
        assert!(!files.is_empty(), "no task knowledge published to {ns}");
        for p in files {
            let mut v: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
            f(&mut v);
            std::fs::write(&p, serde_json::to_vec(&v).unwrap()).unwrap();
        }
    }
}

// ------------------------------------------------------------- selection ----

#[test]
fn a_change_selects_what_it_reaches_and_skips_what_it_does_not() {
    needs_sh!();
    let repo = Repo::new(CONFIG);
    let base = repo.head();
    repo.warm();
    repo.write("src/schema.txt", "v2");
    let head = repo.commit();

    let v = repo.ci(&["--base", &base, "--head", &head]);
    let names = selected(&v);
    assert!(names.contains(&"gen".to_string()), "{names:?}");
    assert!(
        names.contains(&"test-api".to_string()),
        "transitively, {names:?}"
    );
    assert!(!names.contains(&"test-web".to_string()), "{names:?}");
    assert_eq!(v["analysis"]["skipped"], serde_json::json!(["test-web"]));
    assert_eq!(v["summary"]["exit_code"], 0);
}

#[test]
fn an_unrelated_documentation_change_runs_nothing() {
    needs_sh!();
    let repo = Repo::new(CONFIG);
    let base = repo.head();
    repo.warm();
    repo.write("docs/readme.md", "different words entirely");
    let head = repo.commit();

    let v = repo.ci(&["--base", &base, "--head", &head]);
    assert!(selected(&v).is_empty(), "{:?}", selected(&v));
    assert_eq!(counts(&v)["skipped"], 3);
    assert_eq!(v["summary"]["exit_code"], 0);
}

#[test]
fn a_task_that_was_never_learned_is_selected_rather_than_skipped() {
    needs_sh!();
    let repo = Repo::new(CONFIG);
    let base = repo.head();
    repo.write("docs/readme.md", "unrelated");
    let head = repo.commit();

    // Nothing has ever run, so nothing can be proven unaffected.
    let v = repo.ci(&["--base", &base, "--head", &head, "--dry-run"]);
    assert_eq!(selected(&v).len(), 3);
    assert_eq!(counts(&v)["unknown"], 3);
    assert_eq!(counts(&v)["affected"], 0);
}

#[test]
fn a_deleted_dependency_selects_its_consumer() {
    needs_sh!();
    let repo = Repo::new(CONFIG);
    let base = repo.head();
    repo.warm();
    std::fs::remove_file(repo.root.join("src/web.txt")).unwrap();
    let head = repo.commit();

    let v = repo.ci(&["--base", &base, "--head", &head, "--dry-run"]);
    assert!(selected(&v).contains(&"test-web".to_string()));
}

#[test]
fn a_rename_selects_tasks_depending_on_either_path() {
    needs_sh!();
    let repo = Repo::new(CONFIG);
    let base = repo.head();
    repo.warm();
    repo.git(&["mv", "src/web.txt", "src/web-renamed.txt"]);
    let head = repo.commit();

    let v = repo.ci(&["--base", &base, "--head", &head, "--dry-run"]);
    assert!(
        selected(&v).contains(&"test-web".to_string()),
        "the old path is still a dependency"
    );
}

#[test]
fn a_new_file_matching_a_declared_glob_selects_the_task() {
    needs_sh!();
    let config = r#"
[[command]]
name = "plugins"
command = "sh"
args = ["-c", "ls plugins | wc -l"]
inputs = ["plugins/**"]

[ci]
tasks = ["plugins"]
"#;
    let repo = Repo::new(config);
    repo.write("plugins/a.txt", "a");
    let base = repo.commit();
    repo.warm();
    repo.write("plugins/b.txt", "b");
    let head = repo.commit();

    let v = repo.ci(&["--base", &base, "--head", &head, "--dry-run"]);
    assert_eq!(selected(&v), vec!["plugins".to_string()]);
}

#[test]
fn a_configuration_appearing_where_none_existed_selects_its_consumer() {
    needs_sh!();
    let config = r#"
[[command]]
name = "build"
command = "sh"
args = ["-c", "cat config/override.txt 2>/dev/null; echo built"]
inputs = ["config/**"]

[ci]
tasks = ["build"]
"#;
    let repo = Repo::new(config);
    let base = repo.commit();
    repo.warm();
    repo.write("config/override.txt", "now it exists");
    let head = repo.commit();

    let v = repo.ci(&["--base", &base, "--head", &head, "--dry-run"]);
    assert_eq!(selected(&v), vec!["build".to_string()]);
}

// ------------------------------------------------------------- execution ----

#[test]
fn a_second_run_of_unchanged_work_is_a_local_hit() {
    needs_sh!();
    let repo = Repo::new(CONFIG);
    let base = repo.head();
    repo.write("src/schema.txt", "v2");
    let head = repo.commit();

    let first = repo.ci(&["--base", &base, "--head", &head]);
    assert!(counts(&first)["executed"].as_u64().unwrap() > 0);

    let second = repo.ci(&["--base", &base, "--head", &head]);
    let c = counts(&second);
    assert_eq!(c["executed"], 0, "{:?}", outcomes(&second));
    assert!(c["local_hits"].as_u64().unwrap() > 0);
    assert_eq!(c["remote_hits"], 0, "a local hit never touches the network");
}

#[test]
fn a_mixed_plan_reports_each_kind_of_outcome_exactly_once() {
    needs_sh!();
    let repo = Repo::new(CONFIG);
    let base = repo.head();
    repo.warm();
    // gen's input changes, so gen re-executes and test-api follows it; test-web
    // is untouched and is skipped.
    repo.write("src/schema.txt", "v2");
    let head = repo.commit();

    let v = repo.ci(&["--base", &base, "--head", &head]);
    let o = outcomes(&v);
    assert_eq!(o.len(), 3, "{o:?}");
    assert_eq!(o["test-web"], "skipped_unaffected");
    assert!(o["gen"].starts_with("executed"), "{o:?}");
    let c = counts(&v);
    assert_eq!(
        c["local_hits"].as_u64().unwrap()
            + c["remote_hits"].as_u64().unwrap()
            + c["executed"].as_u64().unwrap()
            + c["failed"].as_u64().unwrap()
            + c["blocked"].as_u64().unwrap(),
        c["selected"].as_u64().unwrap(),
        "every selected task is counted once and only once: {c}"
    );
}

#[test]
fn a_failing_task_blocks_its_dependents_and_fails_the_job() {
    needs_sh!();
    let config = r#"
[[command]]
name = "gen"
command = "sh"
args = ["-c", "cat src/schema.txt && exit 3"]
inputs = ["src/schema.txt"]
outputs = ["generated/**"]

[[command]]
name = "consume"
command = "sh"
args = ["-c", "echo consuming"]
after = ["gen"]

[ci]
tasks = ["gen", "consume"]
"#;
    let repo = Repo::new(config);
    let out = repo.arc(&["ci", "--base", "HEAD", "--json"]);
    let v = parse(&out);
    let o = outcomes(&v);
    assert_eq!(o["gen"], "executed_failure", "{o:?}");
    assert_eq!(o["consume"], "blocked", "{o:?}");
    assert_eq!(v["summary"]["exit_code"], 1);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn a_dry_run_executes_nothing() {
    needs_sh!();
    let repo = Repo::new(CONFIG);
    let v = repo.ci(&["--base", "HEAD", "--dry-run"]);
    assert_eq!(counts(&v)["executed"], 0);
    assert!(outcomes(&v).values().all(|o| o == "skipped_unaffected"));
    assert!(
        !repo.root.join("generated/client.txt").exists(),
        "a dry run must not write outputs"
    );
    assert_eq!(v["dry_run"], true);
}

#[test]
fn jobs_and_fail_fast_reach_the_scheduler() {
    needs_sh!();
    let repo = Repo::new(CONFIG);
    let out = repo.arc(&["ci", "--base", "HEAD", "-j", "2", "--fail-fast", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(counts(&parse(&out))["failed"], 0);
}

#[test]
fn two_ci_runs_sharing_one_cache_do_not_corrupt_it() {
    needs_sh!();
    let a = Repo::new(CONFIG);
    let b = Repo::at("other", CONFIG, None);
    // Deliberately the same Arc home, which is what two jobs on one runner do.
    let home = a.home.clone();
    let run = |root: &PathBuf| {
        Command::new(ARC)
            .args(["ci", "--base", "HEAD", "--json"])
            .current_dir(root)
            .env("ARC_HOME", &home)
            .env("ARC_NO_ANIM", "1")
            .spawn()
            .expect("spawning arc")
    };
    let (x, y) = (run(&a.root), run(&b.root));
    for mut child in [x, y] {
        assert!(child.wait().unwrap().success());
    }
    let after = a.arc(&["ci", "--base", "HEAD", "--json"]);
    assert!(after.status.success(), "{}", stderr(&after));
}

// ------------------------------------------------------- revision handling ---

#[test]
fn a_missing_base_commit_selects_everything_and_says_why() {
    needs_sh!();
    let repo = Repo::new(CONFIG);
    repo.warm();
    let v = repo.ci(&[
        "--base",
        "0000000000000000000000000000000000000000",
        "--dry-run",
    ]);
    assert_eq!(v["analysis"]["diff_available"], false);
    assert_eq!(selected(&v).len(), 3, "no diff means no proof, so all run");
    let notes = v["analysis"]["notes"].as_array().unwrap();
    assert!(
        notes.iter().any(|n| n.as_str().unwrap().contains("base")),
        "{notes:?}"
    );
}

#[test]
fn a_shallow_clone_without_the_base_is_conservative() {
    needs_sh!();
    let origin = Repo::new(CONFIG);
    origin.warm();
    let deep = origin.head();
    origin.write("src/schema.txt", "v2");
    origin.commit();
    origin.write("src/schema.txt", "v3");
    origin.commit();

    let tmp = tempfile::tempdir().unwrap();
    let shallow = tmp.path().join("shallow");
    let out = Command::new("git")
        .args([
            "clone",
            "-q",
            "--depth",
            "1",
            &format!(
                "file://{}",
                origin.root.display().to_string().replace('\\', "/")
            ),
            &shallow.display().to_string(),
        ])
        .output()
        .expect("cloning");
    if !out.status.success() {
        return;
    }
    let v = parse(
        &Command::new(ARC)
            .args(["ci", "--json", "--dry-run", "--base", &deep])
            .current_dir(&shallow)
            .env("ARC_HOME", tmp.path().join("archome"))
            .env("ARC_NO_ANIM", "1")
            .output()
            .expect("running arc"),
    );
    assert_eq!(v["analysis"]["diff_available"], false);
    assert_eq!(v["analysis"]["shallow"], true);
    assert_eq!(selected(&v).len(), 3);
}

#[test]
fn a_working_tree_comparison_sees_uncommitted_and_untracked_work() {
    needs_sh!();
    let repo = Repo::new(CONFIG);
    let base = repo.head();
    repo.warm();
    repo.write("src/web.txt", "edited but not committed");

    let v = repo.ci(&["--base", &base, "--dry-run"]);
    assert_eq!(v["analysis"]["compare"], "working-tree");
    assert!(selected(&v).contains(&"test-web".to_string()));
}

#[test]
fn explicit_revisions_beat_provider_environment() {
    needs_sh!();
    let repo = Repo::new(CONFIG);
    let base = repo.head();
    repo.warm();
    repo.write("src/web.txt", "changed");
    let head = repo.commit();

    let with_env = Repo {
        _tmp: tempfile::tempdir().unwrap(),
        root: repo.root.clone(),
        home: repo.home.clone(),
        env: vec![
            ("ARC_CI_BASE".into(), head.clone()),
            ("ARC_CI_HEAD".into(), head.clone()),
        ],
    };
    let v = with_env.ci(&["--base", &base, "--head", &head, "--dry-run"]);
    assert_eq!(v["analysis"]["base"], serde_json::json!(base));
    assert!(selected(&v).contains(&"test-web".to_string()));
}

// -------------------------------------------------------- github actions ----

fn github_env(
    repo: &Repo,
    event: &str,
    payload: &str,
) -> (tempfile::TempDir, Vec<(String, String)>) {
    let dir = tempfile::tempdir().unwrap();
    let event_path = dir.path().join("event.json");
    std::fs::write(&event_path, payload).unwrap();
    let env = vec![
        ("GITHUB_ACTIONS".into(), "true".into()),
        ("GITHUB_EVENT_NAME".into(), event.into()),
        ("GITHUB_EVENT_PATH".into(), event_path.display().to_string()),
        ("GITHUB_REPOSITORY".into(), "acme/widgets".into()),
        ("GITHUB_SHA".into(), repo.head()),
        (
            "GITHUB_STEP_SUMMARY".into(),
            dir.path().join("summary.md").display().to_string(),
        ),
        (
            "GITHUB_OUTPUT".into(),
            dir.path().join("out.txt").display().to_string(),
        ),
    ];
    (dir, env)
}

fn with_env(repo: &Repo, env: Vec<(String, String)>) -> Repo {
    Repo {
        _tmp: tempfile::tempdir().unwrap(),
        root: repo.root.clone(),
        home: repo.home.clone(),
        env,
    }
}

#[test]
fn a_github_pull_request_event_drives_base_and_head() {
    needs_sh!();
    let repo = Repo::new(CONFIG);
    let base = repo.head();
    repo.warm();
    repo.write("src/web.txt", "changed on the branch");
    let head = repo.commit();

    let payload = format!(
        r#"{{"pull_request":{{"number":12,"base":{{"sha":"{base}"}},"head":{{"sha":"{head}","repo":{{"full_name":"acme/widgets","fork":false}}}}}}}}"#
    );
    let (_d, env) = github_env(&repo, "pull_request", &payload);
    let v = with_env(&repo, env).ci(&["--dry-run"]);

    assert_eq!(v["analysis"]["provider"], "github-actions");
    assert_eq!(v["analysis"]["event"], "pull_request");
    assert_eq!(v["analysis"]["pull_request"], 12);
    assert_eq!(v["analysis"]["base"], serde_json::json!(base));
    assert_eq!(v["analysis"]["head"], serde_json::json!(head));
    assert_eq!(selected(&v), vec!["test-web".to_string()], "{v}");
    assert_eq!(v["analysis"]["remote_write"], true);
}

#[test]
fn a_fork_pull_request_never_writes_to_the_shared_cache() {
    needs_sh!();
    let cache = Cache::new();
    let repo = Repo::at("repo", CONFIG, Some((&cache.url, "forks")));
    let base = repo.head();
    let payload = format!(
        r#"{{"pull_request":{{"number":9,"base":{{"sha":"{base}"}},"head":{{"sha":"{base}","repo":{{"full_name":"stranger/widgets","fork":true}}}}}}}}"#
    );
    let (_d, env) = github_env(&repo, "pull_request", &payload);
    let v = with_env(&repo, env).ci(&[]);

    assert_eq!(v["analysis"]["trust"], "untrusted");
    assert_eq!(v["analysis"]["remote_write"], false);
    assert!(v["analysis"]["remote_write_reason"]
        .as_str()
        .unwrap()
        .contains("untrusted"));
    // The work ran, and published nothing.
    assert!(counts(&v)["executed"].as_u64().unwrap() > 0);
    assert!(
        cache.tasks("forks").is_empty(),
        "an untrusted event must not publish task knowledge"
    );
    assert!(
        !cache.dir.join("executions/forks").exists(),
        "an untrusted event must not publish results"
    );
}

#[test]
fn a_trusted_event_publishes_and_a_policy_can_still_forbid_it() {
    needs_sh!();
    let cache = Cache::new();
    let repo = Repo::at("repo", CONFIG, Some((&cache.url, "trusted")));
    let base = repo.head();
    let payload = format!(
        r#"{{"pull_request":{{"number":1,"base":{{"sha":"{base}"}},"head":{{"sha":"{base}","repo":{{"full_name":"acme/widgets","fork":false}}}}}}}}"#
    );
    let (_d, env) = github_env(&repo, "pull_request", &payload);
    let v = with_env(&repo, env.clone()).ci(&[]);
    assert_eq!(v["analysis"]["remote_write"], true);
    assert!(!cache.tasks("trusted").is_empty());

    // `remote_write = "never"` is absolute, trust or no trust.
    let strict = Repo::at("strict", CONFIG, Some((&cache.url, "strict")));
    strict.write(
        "arc.toml",
        &format!(
            "{CONFIG}remote_write = \"never\"\n\n[remote]\nurl = \"{}\"\nnamespace = \"strict\"\n",
            cache.url
        ),
    );
    strict.commit();
    let v = strict.ci(&[]);
    assert_eq!(v["analysis"]["remote_write"], false);
    assert!(!cache.dir.join("tasks/strict").exists());
}

#[test]
fn a_merge_group_uses_the_merge_queue_revisions() {
    needs_sh!();
    let repo = Repo::new(CONFIG);
    let base = repo.head();
    repo.warm();
    repo.write("src/schema.txt", "queued change");
    let head = repo.commit();

    let payload = format!(r#"{{"merge_group":{{"base_sha":"{base}","head_sha":"{head}"}}}}"#);
    let (_d, env) = github_env(&repo, "merge_group", &payload);
    let v = with_env(&repo, env).ci(&["--dry-run"]);
    assert_eq!(v["analysis"]["event"], "merge_group");
    assert_eq!(v["analysis"]["base"], serde_json::json!(base));
    assert!(selected(&v).contains(&"gen".to_string()));
}

#[test]
fn a_workflow_dispatch_has_no_base_and_therefore_runs_everything() {
    needs_sh!();
    let repo = Repo::new(CONFIG);
    repo.warm();
    let (_d, env) = github_env(&repo, "workflow_dispatch", "{}");
    let v = with_env(&repo, env).ci(&["--dry-run"]);
    assert_eq!(v["analysis"]["diff_available"], false);
    assert_eq!(selected(&v).len(), 3);
}

#[test]
fn a_malformed_event_payload_does_not_stop_the_job() {
    needs_sh!();
    let repo = Repo::new(CONFIG);
    repo.warm();
    let (_d, env) = github_env(&repo, "pull_request", "{\"pull_request\": [nonsense");
    let out = with_env(&repo, env).arc(&["ci", "--dry-run", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let v = parse(&out);
    assert_eq!(selected(&v).len(), 3, "conservative, not empty");
}

#[test]
fn the_job_summary_is_markdown_with_no_escape_sequences_or_secrets() {
    needs_sh!();
    let repo = Repo::new(CONFIG).env("ARC_CACHE_TOKEN", "super-secret-value");
    let base = repo.head();
    let payload = format!(
        r#"{{"pull_request":{{"number":3,"base":{{"sha":"{base}"}},"head":{{"sha":"{base}","repo":{{"full_name":"acme/widgets","fork":false}}}}}}}}"#
    );
    let (dir, env) = github_env(&repo, "pull_request", &payload);
    let mut env = env;
    env.push(("ARC_CACHE_TOKEN".into(), "super-secret-value".into()));
    let out = with_env(&repo, env).arc(&["ci", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));

    let summary = std::fs::read_to_string(dir.path().join("summary.md")).unwrap();
    assert!(summary.starts_with("## Arc"), "{summary}");
    assert!(summary.contains("tasks known"));
    assert!(!summary.contains('\x1b'), "no ANSI in a job summary");
    assert!(!summary.contains("super-secret-value"));

    let outputs = std::fs::read_to_string(dir.path().join("out.txt")).unwrap();
    assert!(outputs.contains("executed_count="));
    assert!(outputs.lines().all(|l| l.split('=').count() == 2));
    assert!(!outputs.contains("super-secret-value"));
}

#[test]
fn a_hostile_branch_name_cannot_forge_a_workflow_command() {
    needs_sh!();
    let repo = Repo::new(CONFIG);
    let hostile = "feature/\u{1b}[2J::error::pwned";
    let (dir, mut env) = github_env(&repo, "push", "{}");
    env.push(("GITHUB_REF_NAME".into(), hostile.into()));
    env.push(("GITHUB_HEAD_REF".into(), hostile.into()));
    let out = with_env(&repo, env).arc(&["ci", "--dry-run"]);

    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), stderr(&out));
    assert!(
        !text.contains('\u{1b}'),
        "escape sequences must not reach a log"
    );
    for line in text.lines() {
        assert!(
            !line.trim_start().starts_with("::error::"),
            "forged workflow command: {line}"
        );
    }
    let summary = std::fs::read_to_string(dir.path().join("summary.md")).unwrap_or_default();
    assert!(!summary.contains('\u{1b}'));
}

// --------------------------------------------------- configuration errors ---

#[test]
fn a_ci_task_naming_a_missing_command_is_a_configuration_error() {
    needs_sh!();
    let repo = Repo::new("[ci]\ntasks = [\"does-not-exist\"]\n");
    let out = repo.arc(&["ci", "--base", "HEAD", "--dry-run"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("does-not-exist"), "{}", stderr(&out));
}

#[test]
fn only_declared_tasks_are_considered_not_every_command_ever_run() {
    needs_sh!();
    let repo = Repo::new(CONFIG);
    let base = repo.head();
    // A one-off command, learned by the cache but never declared for CI.
    let out = repo.arc(&["run", "sh", "-c", "cat src/schema.txt > /dev/null"]);
    assert!(out.status.success(), "{}", stderr(&out));
    repo.write("src/schema.txt", "v2");
    let head = repo.commit();

    let v = repo.ci(&["--base", &base, "--head", &head, "--dry-run"]);
    assert_eq!(v["analysis"]["known_tasks"], 3);
    for name in selected(&v) {
        assert!(
            ["gen", "test-api", "test-web"].contains(&name.as_str()),
            "{name}"
        );
    }
}

#[test]
fn task_filters_narrow_the_run_and_an_unknown_filter_is_an_error() {
    needs_sh!();
    let repo = Repo::new(CONFIG);
    let v = repo.ci(&["--base", "HEAD", "--dry-run", "--task", "test-web"]);
    assert_eq!(v["analysis"]["known_tasks"], 1);
    assert_eq!(selected(&v), vec!["test-web".to_string()]);

    let out = repo.arc(&["ci", "--base", "HEAD", "--dry-run", "--task", "nope"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("nope"));
}

#[test]
fn a_project_with_no_declared_tasks_says_so_instead_of_guessing() {
    needs_sh!();
    let repo = Repo::new("[outputs]\ninclude = [\"out/**\"]\n");
    let out = repo.arc(&["ci", "--base", "HEAD"]);
    assert!(out.status.success());
    assert!(
        stderr(&out).contains("no CI tasks are declared") || {
            let s = String::from_utf8_lossy(&out.stdout);
            s.contains("no CI tasks are declared")
        }
    );
}

// ---------------------------------------------------------------- remote ----

#[test]
fn a_fresh_machine_reuses_both_the_results_and_the_task_knowledge() {
    needs_sh!();
    let cache = Cache::new();
    let a = Repo::at("machine-a", CONFIG, Some((&cache.url, "shared")));
    a.warm();
    assert_eq!(
        cache.tasks("shared").len(),
        3,
        "every learned task publishes its dependency knowledge"
    );

    // A different checkout root, an empty Arc home, the same project state.
    let b = Repo::at(
        "deeply/nested/machine-b",
        CONFIG,
        Some((&cache.url, "shared")),
    );
    let base = b.head();
    b.write("docs/readme.md", "docs only");
    let head = b.commit();

    let v = b.ci(&["--base", &base, "--head", &head]);
    // Knowledge came from the remote, so a docs-only change proves the tasks
    // unaffected without this machine ever having run them.
    assert_eq!(v["analysis"]["knowledge_remote"], 3, "{v}");
    assert_eq!(v["analysis"]["knowledge_local"], 0);
    assert!(selected(&v).is_empty(), "{:?}", selected(&v));
    assert_eq!(counts(&v)["skipped"], 3);
}

/// A task with no declared inputs can never be proven unaffected, so it is
/// always selected. That makes it the clean way to show the other half of the
/// story: selection avoided nothing here, and the remote cache did.
const PROBE: &str = r#"
[[command]]
name = "probe"
command = "sh"
args = ["-c", "mkdir -p out && cat src/schema.txt > out/probe.txt && echo probed"]
outputs = ["out/**"]

[ci]
tasks = ["probe"]
"#;

#[test]
fn a_selected_task_another_machine_already_ran_is_a_remote_hit() {
    needs_sh!();
    let cache = Cache::new();
    let a = Repo::at("machine-a", PROBE, Some((&cache.url, "hits")));
    let first = a.ci(&["--base", "HEAD"]);
    assert_eq!(counts(&first)["executed"], 1, "{first}");

    // A different checkout root and an empty Arc home, at the same state.
    let b = Repo::at("deeply/nested/machine-b", PROBE, Some((&cache.url, "hits")));
    let v = b.ci(&["--base", "HEAD"]);
    assert_eq!(
        selected(&v),
        vec!["probe".to_string()],
        "unknown, so selected"
    );
    assert_eq!(outcomes(&v)["probe"], "remote_hit", "{v}");
    assert_eq!(counts(&v)["executed"], 0);
    assert_eq!(
        std::fs::read_to_string(b.root.join("out/probe.txt"))
            .unwrap()
            .trim(),
        "v1",
        "the outputs, not just the exit status, crossed the wire"
    );
}

#[test]
fn remote_task_metadata_can_never_change_what_ci_executes() {
    needs_sh!();
    let cache = Cache::new();
    let a = Repo::at("machine-a", CONFIG, Some((&cache.url, "evil")));
    a.warm();

    // The server now claims these tasks run something else entirely.
    let marker = "pwned.txt";
    cache.mutate_tasks("evil", |t| {
        t["program"] = serde_json::json!("sh");
        t["args"] = serde_json::json!(["-c", format!("touch {marker}")]);
    });

    let b = Repo::at("machine-b", CONFIG, Some((&cache.url, "evil")));
    let base = b.head();
    b.write("docs/readme.md", "docs");
    let head = b.commit();
    let v = b.ci(&["--base", &base, "--head", &head]);

    assert!(
        !b.root.join(marker).exists(),
        "the remote command must not run"
    );
    // The records were rejected, so nothing was proven and everything runs.
    assert_eq!(v["analysis"]["knowledge_remote"], 0);
    assert_eq!(selected(&v).len(), 3);
    for t in v["summary"]["tasks"].as_array().unwrap() {
        assert_ne!(t["outcome"], "skipped_unaffected");
    }
}

#[test]
fn stale_remote_task_semantics_are_ignored_and_relearned() {
    needs_sh!();
    let cache = Cache::new();
    let a = Repo::at("machine-a", CONFIG, Some((&cache.url, "stale")));
    a.warm();
    cache.mutate_tasks("stale", |t| {
        t["dependency_semantics"] = serde_json::json!(99);
    });

    let b = Repo::at("machine-b", CONFIG, Some((&cache.url, "stale")));
    let base = b.head();
    b.write("docs/readme.md", "docs");
    let head = b.commit();
    let v = b.ci(&["--base", &base, "--head", &head, "--dry-run"]);
    assert_eq!(v["analysis"]["knowledge_remote"], 0);
    assert_eq!(selected(&v).len(), 3, "unknown, so everything runs");
}

#[test]
fn partial_remote_knowledge_cannot_prove_a_task_unaffected() {
    needs_sh!();
    let cache = Cache::new();
    let a = Repo::at("machine-a", CONFIG, Some((&cache.url, "partial")));
    a.warm();
    cache.mutate_tasks("partial", |t| {
        t["inputs_narrowed"] = serde_json::json!(false);
        t["completeness"] = serde_json::json!("partial");
    });

    let b = Repo::at("machine-b", CONFIG, Some((&cache.url, "partial")));
    let base = b.head();
    b.write("docs/readme.md", "docs");
    let head = b.commit();
    let v = b.ci(&["--base", &base, "--head", &head, "--dry-run"]);
    assert_eq!(v["analysis"]["knowledge_remote"], 0);
    assert_eq!(v["analysis"]["knowledge_none"], 3);
    assert_eq!(selected(&v).len(), 3);
}

#[test]
fn corrupt_remote_task_metadata_is_ignored_rather_than_fatal() {
    needs_sh!();
    let cache = Cache::new();
    let a = Repo::at("machine-a", CONFIG, Some((&cache.url, "corrupt")));
    a.warm();
    for p in cache.tasks("corrupt") {
        std::fs::write(&p, b"{ not json at all").unwrap();
    }

    let b = Repo::at("machine-b", CONFIG, Some((&cache.url, "corrupt")));
    let out = b.arc(&["ci", "--base", "HEAD", "--dry-run", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(parse(&out)["analysis"]["knowledge_remote"], 0);
}

#[test]
fn an_unreachable_remote_degrades_to_local_without_failing_the_job() {
    needs_sh!();
    let mut cache = Cache::new();
    let repo = Repo::at("repo", CONFIG, Some((&cache.url, "down")));
    cache.stop();

    let out = repo.arc(&["ci", "--base", "HEAD", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let v = parse(&out);
    assert!(counts(&v)["executed"].as_u64().unwrap() > 0);
    assert_eq!(counts(&v)["failed"], 0);
}

#[test]
fn no_remote_configured_still_runs_ci() {
    needs_sh!();
    let repo = Repo::new(CONFIG);
    let out = repo.arc(&["ci", "--base", "HEAD", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(parse(&out)["analysis"]["knowledge_remote"], 0);
}

#[test]
fn no_remote_flag_disables_both_reuse_and_publishing() {
    needs_sh!();
    let cache = Cache::new();
    let repo = Repo::at("repo", CONFIG, Some((&cache.url, "off")));
    let v = repo.ci(&["--base", "HEAD", "--no-remote"]);
    assert_eq!(v["analysis"]["remote_write"], false);
    assert!(!cache.dir.join("tasks/off").exists());
}

// ------------------------------------------------------------ environment ---

#[test]
fn run_metadata_that_differs_every_job_does_not_cost_a_cache_hit() {
    needs_sh!();
    let repo = Repo::new(CONFIG);
    let base = repo.head();
    repo.warm();
    repo.write("src/web.txt", "changed");
    let head = repo.commit();

    let first = repo.ci(&["--base", &base, "--head", &head]);
    assert!(counts(&first)["executed"].as_u64().unwrap() > 0);

    // The same commits, a different job. Nothing about the work changed.
    let second = with_env(
        &repo,
        vec![
            ("GITHUB_RUN_ID".into(), "999999".into()),
            ("GITHUB_RUN_ATTEMPT".into(), "4".into()),
            ("GITHUB_JOB".into(), "another-job".into()),
        ],
    )
    .ci(&["--base", &base, "--head", &head]);
    assert_eq!(counts(&second)["executed"], 0, "{:?}", outcomes(&second));
    assert!(counts(&second)["local_hits"].as_u64().unwrap() > 0);
}

#[test]
fn an_environment_variable_the_project_declared_still_invalidates() {
    needs_sh!();
    let config = format!(
        "{CONFIG}
[env]
include = [\"FEATURE_FLAGS\"]
"
    );
    let repo = Repo::new(&config);
    let base = repo.head();
    repo.write("src/web.txt", "changed");
    let head = repo.commit();
    let range = ["--base", base.as_str(), "--head", head.as_str()];
    let run =
        |flags: &str| with_env(&repo, vec![("FEATURE_FLAGS".into(), flags.into())]).ci(&range);

    let first = run("a");
    assert!(counts(&first)["executed"].as_u64().unwrap() > 0);
    let same = run("a");
    assert_eq!(counts(&same)["executed"], 0, "{:?}", outcomes(&same));
    let changed = run("b");
    assert!(
        counts(&changed)["executed"].as_u64().unwrap() > 0,
        "a declared variable is part of the key: {:?}",
        outcomes(&changed)
    );
}

#[test]
fn declaring_a_volatile_ci_variable_is_reported_as_a_warning() {
    needs_sh!();
    let config = format!("{CONFIG}\n[env]\ninclude = [\"GITHUB_RUN_ID\"]\n");
    let repo = Repo::new(&config);
    let v = repo.ci(&["--base", "HEAD", "--dry-run"]);
    let notes = v["analysis"]["notes"].as_array().unwrap();
    assert!(
        notes
            .iter()
            .any(|n| n.as_str().unwrap().contains("GITHUB_RUN_ID")),
        "{notes:?}"
    );
}

// ---------------------------------------------------------------- doctor ----

#[test]
fn doctor_reports_ci_context_without_exposing_a_token() {
    needs_sh!();
    let repo = Repo::new(CONFIG).env("ARC_CACHE_TOKEN", "another-secret");
    let (_d, mut env) = github_env(&repo, "push", "{}");
    env.push(("ARC_CACHE_TOKEN".into(), "another-secret".into()));
    let out = with_env(&repo, env).arc(&["doctor"]);
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(text.contains("continuous integration"), "{text}");
    assert!(text.contains("github-actions"));
    assert!(text.contains("gen, test-api, test-web"));
    assert!(!text.contains("another-secret"));
}

#[test]
fn explain_says_why_each_task_is_in_its_bucket() {
    needs_sh!();
    let repo = Repo::new(CONFIG);
    let base = repo.head();
    repo.warm();
    repo.write("src/schema.txt", "v2");
    let head = repo.commit();
    let out = repo.arc(&[
        "ci",
        "--base",
        &base,
        "--head",
        &head,
        "--dry-run",
        "--explain",
    ]);
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(text.contains("src/schema.txt changed"), "{text}");
}
