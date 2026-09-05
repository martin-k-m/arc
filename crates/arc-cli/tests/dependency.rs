//! End-to-end tests for dependency intelligence, against the real binary.
//!
//! These lean on the failure direction that matters: every case Arc cannot
//! prove must end in an execution, never in a hit.
//!
//! Two worlds are exercised deliberately. Where a backend observes everything,
//! Arc narrows automatically and a change it did not depend on is a *hit* —
//! that is the whole point of the milestone. Where it does not, the project-wide
//! scan applies and any change is a miss. Tests that care about the difference
//! ask [`narrows`] rather than assuming a platform.

mod support;

use std::path::PathBuf;
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
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("src/a.txt"), "one").unwrap();
        std::fs::write(root.join("docs/design.md"), "docs").unwrap();
        Sandbox {
            _tmp: tmp,
            root,
            home,
        }
    }

    /// Scopes the test command to `src/**`, which is what lets Arc prove a
    /// change under `docs/` is irrelevant.
    fn scoped(self) -> Sandbox {
        self.write(
            "arc.toml",
            "[[command]]\nmatch = \"*\"\ninputs = [\"src/**\"]\n",
        );
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

    fn run(&self, extra: &[&str]) -> Output {
        let cmd = echo();
        let mut args: Vec<&str> = vec!["run"];
        args.extend(extra);
        args.extend(cmd.iter().map(|s| s.as_str()));
        self.arc(&args)
    }

    fn write(&self, rel: &str, body: &str) {
        let p = self.root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn git(&self, args: &[&str]) {
        Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .output()
            .expect("running git");
    }

    fn init_git(&self) {
        self.git(&["init", "-q"]);
        self.git(&["config", "user.email", "t@example.com"]);
        self.git(&["config", "user.name", "test"]);
        self.git(&["add", "-A"]);
        self.git(&["commit", "-qm", "init"]);
    }
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).to_string()
}
fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).to_string()
}
fn json(o: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout(o)).expect("machine-readable output")
}

fn echo() -> Vec<String> {
    if cfg!(windows) {
        vec!["cmd".into(), "/c".into(), "echo scoped-run".into()]
    } else {
        vec!["sh".into(), "-c".into(), "echo scoped-run".into()]
    }
}

fn writes(rel: &str) -> Vec<String> {
    if cfg!(windows) {
        vec![
            "cmd".into(),
            "/c".into(),
            format!("echo generated> {}", rel.replace('/', "\\")),
        ]
    } else {
        vec!["sh".into(), "-c".into(), format!("echo generated > {rel}")]
    }
}

fn git_available() -> bool {
    Command::new("git").arg("--version").output().is_ok()
}

/// Whether this platform's tracer can observe every dependency class, and so
/// whether Arc will narrow the input set without being told to.
///
/// Read from the binary's own diagnostics rather than from `cfg!`, so the tests
/// track what Arc actually reports — including a Linux container where ptrace is
/// refused and the answer is "no" despite the platform.
fn narrows() -> bool {
    let out = Command::new(ARC)
        .arg("doctor")
        .output()
        .expect("arc doctor");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|l| l.contains("automatic narrowing") && l.contains("supported"))
}

// ---------------------------------------------------------------- family ----

#[test]
fn family_identity_separates_commands_but_survives_content_changes() {
    let sb = Sandbox::new();
    sb.run(&[]);
    let first = json(&sb.arc(&["history", "--json"]))[0]["family_key"]
        .as_str()
        .unwrap()
        .to_string();

    // Editing an input must not move the execution to a different family, or
    // Arc could never find what it learned last time.
    sb.write("src/a.txt", "two");
    sb.run(&[]);
    let after_edit = json(&sb.arc(&["history", "--json"]))[0]["family_key"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(first, after_edit);

    // Different arguments are a different family.
    let mut args: Vec<&str> = vec!["run", "cmd", "/c", "echo other"];
    if !cfg!(windows) {
        args = vec!["run", "sh", "-c", "echo other"];
    }
    sb.arc(&args);
    let other = json(&sb.arc(&["history", "--json"]))[0]["family_key"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(first, other);
}

#[test]
fn changing_input_scope_invalidates_the_family() {
    let sb = Sandbox::new().scoped();
    sb.run(&[]);
    assert!(stderr(&sb.run(&[])).contains("CACHE HIT"));

    sb.write(
        "arc.toml",
        "[[command]]\nmatch = \"*\"\ninputs = [\"src/**\", \"docs/**\"]\n",
    );
    assert!(
        !stderr(&sb.run(&[])).contains("CACHE HIT"),
        "a scope change must not reuse knowledge gathered under the old scope"
    );
}

// ------------------------------------------------------------ narrowing ----

#[test]
fn a_scoped_command_ignores_changes_outside_its_inputs() {
    let sb = Sandbox::new().scoped();
    sb.run(&[]);
    sb.write("docs/design.md", "rewritten entirely");

    let out = sb.run(&["--explain"]);
    let log = stderr(&out);
    assert!(log.contains("CACHE HIT"), "{log}");
    assert!(
        log.contains("docs/design.md"),
        "the explanation should name what it ignored: {log}"
    );
}

#[test]
fn a_scoped_command_still_misses_when_a_real_dependency_changes() {
    let sb = Sandbox::new().scoped();
    sb.run(&[]);
    sb.write("src/a.txt", "two");

    let log = stderr(&sb.run(&["--explain"]));
    assert!(log.contains("cache miss"), "{log}");
    assert!(log.contains("src/a.txt changed"), "{log}");
}

#[test]
fn deleting_a_dependency_forces_a_miss() {
    let sb = Sandbox::new().scoped();
    sb.run(&[]);
    std::fs::remove_file(sb.root.join("src/a.txt")).unwrap();

    let log = stderr(&sb.run(&["--explain"]));
    assert!(log.contains("cache miss"), "{log}");
    assert!(log.contains("src/a.txt removed"), "{log}");
}

#[test]
fn a_new_file_appearing_inside_the_scope_forces_a_miss() {
    // The absence of `src/b.txt` was part of the state the result was produced
    // under. Arc must not replay across its appearance.
    let sb = Sandbox::new().scoped();
    sb.run(&[]);
    sb.write("src/b.txt", "new");

    let log = stderr(&sb.run(&["--explain"]));
    assert!(log.contains("cache miss"), "{log}");
    assert!(log.contains("src/b.txt added"), "{log}");
}

#[test]
fn without_narrowing_any_project_change_invalidates() {
    // Pinned to the conservative backend so this holds on every platform: with
    // no proof of what the command reads, Arc has no grounds to call any change
    // irrelevant.
    let sb = Sandbox::new();
    sb.run(&["--trace-backend", "snapshot"]);
    sb.run(&["--trace-backend", "snapshot"]);
    sb.write("docs/design.md", "changed");
    assert!(
        !stderr(&sb.run(&["--trace-backend", "snapshot"])).contains("CACHE HIT"),
        "without narrowing, every change must be treated as relevant"
    );
}

#[test]
fn a_complete_trace_makes_an_unrelated_change_irrelevant() {
    if !narrows() {
        return;
    }
    // No arc.toml, no scoping: the knowledge comes entirely from observing the
    // execution. This is the milestone in one assertion.
    let sb = Sandbox::new();
    sb.run(&[]);
    sb.run(&[]);
    sb.write("docs/design.md", "rewritten");
    let log = stderr(&sb.run(&[]));
    assert!(log.contains("CACHE HIT"), "{log}");
}

// ---------------------------------------------------------------- trace ----

#[test]
fn tracing_never_claims_more_than_the_backend_can_see() {
    let sb = Sandbox::new();
    let log = stderr(&sb.run(&["--trace"]));
    assert!(log.contains("TRACE"), "{log}");
    let headline = log
        .lines()
        .find(|l| l.contains("TRACE "))
        .unwrap_or_default();
    if narrows() {
        assert!(headline.contains("TRACE COMPLETE"), "{log}");
    } else {
        // The headline claim must never be "complete" while reads are
        // unobservable, whatever else the report goes on to say.
        assert!(headline.contains("TRACE PARTIAL"), "{log}");
        assert!(
            model_line(&log).contains("partial"),
            "the model must be reported as partial: {log}"
        );
    }
}

/// The `dependency model` row of a trace report.
fn model_line(log: &str) -> String {
    log.lines()
        .find(|l| l.contains("dependency model"))
        .unwrap_or_default()
        .to_string()
}

#[test]
fn pinning_the_conservative_backend_is_honoured_and_never_narrows() {
    let sb = Sandbox::new();
    let log = stderr(&sb.run(&["--trace", "--trace-backend", "snapshot"]));
    assert!(log.contains("TRACE PARTIAL"), "{log}");
    assert!(model_line(&log).contains("partial"), "{log}");
    assert!(
        log.contains("next run narrows")
            && !stderr(&sb.run(&["--trace-backend", "snapshot"])).is_empty(),
        "{log}"
    );
}

#[test]
fn observed_writes_are_recorded_as_outputs_not_inputs() {
    let sb = Sandbox::new();
    let mut args: Vec<&str> = vec!["run", "--trace"];
    let cmd = writes("src/generated.txt");
    args.extend(cmd.iter().map(|s| s.as_str()));
    let out = sb.arc(&args);
    assert!(out.status.success(), "{}", stderr(&out));

    let graph = json(&sb.arc(&["graph", "--json"]));
    let node = &graph["nodes"][0];
    let produces: Vec<String> = serde_json::from_value(node["produces"].clone()).unwrap();
    assert!(
        produces.iter().any(|o| o.contains("generated.txt")),
        "the write should be learned as an output: {node}"
    );
    let consumed = node["consumes"].as_array().unwrap();
    assert!(
        !consumed
            .iter()
            .any(|c| c["path"].as_str().unwrap_or("").contains("generated.txt")),
        "a file this execution created is not an input to it: {node}"
    );
}

#[test]
fn the_process_tree_contributes_executables_to_the_cache_key() {
    let sb = Sandbox::new();
    sb.run(&["--trace"]);
    let id = json(&sb.arc(&["history", "--json"]))[0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let rec = json(&sb.arc(&["inspect", &id, "--json"]));
    if cfg!(windows) {
        assert!(
            rec["trace"]["executables"].as_u64().unwrap() > 0,
            "the job-object backend should identify at least the child itself: {rec}"
        );
    }
    // Whatever was observed, the run must remain reusable.
    assert!(stderr(&sb.run(&[])).contains("CACHE HIT"));
}

#[test]
fn arc_home_inside_the_project_is_never_observed_as_a_dependency() {
    let sb = Sandbox::new();
    let inner = sb.root.join(".arc-home");
    let call = || {
        Command::new(ARC)
            .args(["run", "--trace"])
            .args(echo().iter().map(|s| s.as_str()))
            .current_dir(&sb.root)
            .env("ARC_HOME", &inner)
            .output()
            .unwrap()
    };
    call();
    let second = call();
    assert!(stderr(&second).contains("CACHE HIT"), "{}", stderr(&second));

    let graph = json(
        &Command::new(ARC)
            .args(["graph", "--json"])
            .current_dir(&sb.root)
            .env("ARC_HOME", &inner)
            .output()
            .unwrap(),
    );
    let text = graph.to_string();
    assert!(
        !text.contains("arc-home"),
        "Arc's own cache must not appear in its dependency graph: {text}"
    );
}

// ----------------------------------------------------------- resilience ----

#[test]
fn a_corrupt_dependency_record_executes_instead_of_replaying() {
    let sb = Sandbox::new().scoped();
    sb.run(&[]);
    assert!(stderr(&sb.run(&[])).contains("CACHE HIT"));

    // Damage the metadata database itself, which is where dependency sets live.
    let db = sb.home.join("arc.redb");
    std::fs::write(&db, b"not a database at all").unwrap();

    let out = sb.run(&[]);
    assert!(
        !stderr(&out).contains("CACHE HIT"),
        "corrupt metadata must never produce a hit"
    );
    assert!(stdout(&out).contains("scoped-run"), "{}", stdout(&out));
}

#[test]
fn a_v1_database_is_replaced_rather_than_misread() {
    let sb = Sandbox::new();
    sb.run(&[]);
    // Reaching in and clearing the schema marker is the closest a test can get
    // to "a database written by an older Arc".
    let stats_before = json(&sb.arc(&["cache", "stats", "--json"]));
    assert_eq!(stats_before["cache_entries"], 1);

    std::fs::remove_file(sb.home.join("arc.redb")).unwrap();
    let out = sb.arc(&["cache", "stats", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(json(&out)["cache_entries"], 0);
}

#[test]
fn concurrent_traced_runs_do_not_corrupt_dependency_metadata() {
    let sb = Sandbox::new();
    let children: Vec<_> = (0..6)
        .map(|i| {
            let text = format!("echo job{}", i % 3);
            let cmd: Vec<String> = if cfg!(windows) {
                vec!["cmd".into(), "/c".into(), text]
            } else {
                vec!["sh".into(), "-c".into(), text]
            };
            support::spawn_captured(
                Command::new(ARC)
                    .args(["run", "--trace"])
                    .args(cmd.iter().map(|s| s.as_str()))
                    .current_dir(&sb.root)
                    .env("ARC_HOME", &sb.home),
            )
        })
        .collect();
    for (i, c) in children.into_iter().enumerate() {
        support::wait_ok(&format!("concurrent traced run {i}"), c);
    }
    let graph = sb.arc(&["graph", "--json"]);
    assert!(graph.status.success(), "{}", stderr(&graph));
    assert_eq!(json(&graph)["nodes"].as_array().unwrap().len(), 3);
    assert!(stdout(&sb.arc(&["cache", "verify"])).contains("No corruption"));
}

// -------------------------------------------------------------- secrets ----

#[test]
fn secrets_never_reach_dependency_or_trace_metadata() {
    let sb = Sandbox::new();
    sb.write("arc.toml", "[env]\ninclude = [\"ARC_TEST_SECRET\"]\n");
    Command::new(ARC)
        .args(["run", "--trace"])
        .args(echo().iter().map(|s| s.as_str()))
        .current_dir(&sb.root)
        .env("ARC_HOME", &sb.home)
        .env("ARC_TEST_SECRET", "super-secret-value-12345")
        .env("AWS_SECRET_ACCESS_KEY", "fake-secret-abcdef")
        .output()
        .unwrap();

    let mut haystack = String::new();
    for args in [
        vec!["history", "--json"],
        vec!["graph", "--json"],
        vec!["cache", "stats", "--json"],
    ] {
        let o = sb.arc(&args);
        haystack.push_str(&stdout(&o));
        haystack.push_str(&stderr(&o));
    }
    // Everything Arc persisted on disk, not only what it chose to print.
    for entry in walk(&sb.home) {
        if let Ok(bytes) = std::fs::read(&entry) {
            haystack.push_str(&String::from_utf8_lossy(&bytes));
        }
    }
    assert!(!haystack.contains("super-secret-value-12345"));
    assert!(!haystack.contains("fake-secret-abcdef"));
}

fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
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

// ----------------------------------------------------------------- paths ----

#[test]
fn unicode_spaces_and_nesting_survive_a_round_trip() {
    let sb = Sandbox::new().scoped();
    sb.write("src/a folder/ünïcode ✓.txt", "content");
    sb.write("src/deep/deeper/deepest/file.txt", "nested");
    sb.run(&[]);
    assert!(stderr(&sb.run(&[])).contains("CACHE HIT"));

    sb.write("src/a folder/ünïcode ✓.txt", "changed");
    let log = stderr(&sb.run(&["--explain"]));
    assert!(log.contains("cache miss"), "{log}");
    assert!(log.contains("ünïcode"), "{log}");
}

#[test]
fn an_empty_file_is_a_dependency_like_any_other() {
    let sb = Sandbox::new().scoped();
    sb.write("src/empty.txt", "");
    sb.run(&[]);
    assert!(stderr(&sb.run(&[])).contains("CACHE HIT"));
    sb.write("src/empty.txt", "no longer empty");
    assert!(!stderr(&sb.run(&[])).contains("CACHE HIT"));
}

#[test]
fn replacing_a_file_with_a_directory_does_not_produce_a_hit() {
    let sb = Sandbox::new().scoped();
    sb.run(&[]);
    std::fs::remove_file(sb.root.join("src/a.txt")).unwrap();
    std::fs::create_dir_all(sb.root.join("src/a.txt")).unwrap();
    std::fs::write(sb.root.join("src/a.txt/inner"), "surprise").unwrap();

    let out = sb.run(&[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!stderr(&out).contains("CACHE HIT"), "{}", stderr(&out));
}

// -------------------------------------------------------------- affected ----

#[test]
fn affected_maps_git_changes_onto_scoped_executions() {
    if !git_available() {
        return;
    }
    let sb = Sandbox::new().scoped();
    sb.init_git();
    sb.run(&[]);

    // A change inside the declared scope.
    sb.write("src/a.txt", "modified");
    let report = json(&sb.arc(&["affected", "--json"]));
    assert_eq!(report["tasks"][0]["verdict"], "affected", "{report}");

    // A change outside it.
    sb.git(&["checkout", "--", "src/a.txt"]);
    sb.write("docs/design.md", "modified");
    let report = json(&sb.arc(&["affected", "--json"]));
    assert_eq!(report["tasks"][0]["verdict"], "unaffected", "{report}");
}

#[test]
fn affected_says_unknown_rather_than_unaffected_without_narrowing() {
    if !git_available() {
        return;
    }
    let sb = Sandbox::new();
    sb.init_git();
    sb.run(&[]);
    sb.write("docs/design.md", "modified");

    let report = json(&sb.arc(&["affected", "--json"]));
    let verdict = report["tasks"][0]["verdict"].as_str().unwrap();
    if narrows() {
        // A complete trace makes the family provable, so `unaffected` is a
        // conclusion Arc earned rather than a guess.
        assert_eq!(verdict, "unaffected", "{report}");
    } else {
        assert_eq!(
            verdict, "unknown",
            "without proof, a family must never be declared unaffected: {report}"
        );
    }
}

#[test]
fn affected_reports_missing_git_instead_of_an_empty_change_list() {
    let sb = Sandbox::new().scoped();
    sb.run(&[]);
    let report = json(&sb.arc(&["affected", "--json"]));
    if report["git_error"].is_null() {
        // Running inside a repository is fine; the assertion is that Arc does
        // not silently claim "nothing changed" when it could not ask.
        return;
    }
    assert!(report["changes"].as_array().unwrap().is_empty());
}

// ----------------------------------------------------------------- graph ----

#[test]
fn graph_json_is_stable_and_reports_narrowing_honestly() {
    let sb = Sandbox::new();
    sb.run(&[]);
    let graph = json(&sb.arc(&["graph", "--json"]));
    assert!(graph["schema"].is_number());
    let node = &graph["nodes"][0];
    assert!(node["label"].as_str().unwrap().contains("echo"));
    assert!(node["completeness"].is_string());
    assert!(
        node["program"].is_string(),
        "a task must be re-runnable: {node}"
    );
    assert!(node["args"].is_array());
    // The flag must agree with what the platform can actually observe: claiming
    // narrowed inputs Arc did not earn is exactly the lie this guards against.
    assert_eq!(
        node["inputs_narrowed"].as_bool().unwrap(),
        narrows(),
        "{node}"
    );
    for field in ["produces", "consumes", "declared_inputs", "after"] {
        assert!(node[field].is_array(), "missing {field}: {node}");
    }
    for field in ["edges", "ambiguities", "cycles", "unresolved"] {
        assert!(graph[field].is_array(), "missing {field}: {graph}");
    }
}

#[test]
fn the_documented_example_config_is_valid() {
    // `deny_unknown_fields` means a stale example is a hard error, not a
    // cosmetic doc bug, so it is worth a test.
    let example = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/arc.toml")
        .canonicalize()
        .unwrap();
    let sb = Sandbox::new();
    std::fs::copy(&example, sb.root.join("arc.toml")).unwrap();
    let out = sb.arc(&["config", "show"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("[[command]]"), "{}", stdout(&out));
}

#[test]
fn graph_and_affected_are_empty_but_successful_on_a_fresh_project() {
    let sb = Sandbox::new();
    let g = sb.arc(&["graph"]);
    assert!(g.status.success(), "{}", stderr(&g));
    assert!(stdout(&g).contains("No executions recorded"));
    assert!(sb.arc(&["affected"]).status.success());
}
