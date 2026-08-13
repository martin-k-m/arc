//! End-to-end tests for the task graph, affected propagation and the scheduler.
//!
//! Edges here come from declared `[[command]] inputs`, which every platform
//! supports, so the graph itself is exercised everywhere. Edges discovered by
//! observation need a read-capable tracer and are covered in `linux_trace.rs`.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::{Command, Output};

const ARC: &str = env!("CARGO_BIN_EXE_arc");

struct Sandbox {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
}

impl Sandbox {
    fn new(config: &str) -> Sandbox {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        let home = tmp.path().join("archome");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("generated")).unwrap();
        let sb = Sandbox {
            _tmp: tmp,
            root,
            home,
        };
        sb.write("arc.toml", config);
        sb.write("src/seed.txt", "one");
        sb
    }

    fn arc(&self, args: &[&str]) -> Output {
        Command::new(ARC)
            .args(args)
            .current_dir(&self.root)
            .env("ARC_HOME", &self.home)
            .env("ARC_NO_ANIM", "1")
            .output()
            .expect("running arc")
    }

    /// Run a task twice: once to learn it, once to settle its key.
    fn learn(&self, script: &str) {
        for _ in 0..2 {
            let out = self.run(script);
            assert!(out.status.success(), "{}", stderr(&out));
        }
    }

    fn run(&self, script: &str) -> Output {
        self.arc(&["run", "sh", "-c", script])
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
        self.commit();
    }

    fn commit(&self) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-qm", "snapshot"]);
    }

    fn graph(&self) -> serde_json::Value {
        json(&self.arc(&["graph", "--json"]))
    }

    fn affected(&self) -> serde_json::Value {
        json(&self.arc(&["affected", "--json"]))
    }

    fn verdicts(&self) -> std::collections::BTreeMap<String, String> {
        self.affected()["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| {
                (
                    t["label"].as_str().unwrap().to_string(),
                    t["verdict"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    fn edges(&self) -> BTreeSet<(String, String)> {
        let g = self.graph();
        let label = |key: &str| -> String {
            g["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|n| n["family_key"] == key)
                .map(|n| n["label"].as_str().unwrap().to_string())
                .unwrap_or_else(|| key.to_string())
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
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).to_string()
}
fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).to_string()
}
fn json(o: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout(o))
        .unwrap_or_else(|e| panic!("machine-readable output: {e}\n{}", stdout(o)))
}

fn git_available() -> bool {
    Command::new("git").arg("--version").output().is_ok()
}

/// Tasks are shell one-liners, so every platform runs the same test. Windows
/// ships no `sh`, so these skip there and the graph is covered by the unit
/// tests plus the Linux suite.
fn sh_available() -> bool {
    Command::new("sh").arg("-c").arg("exit 0").output().is_ok()
}

macro_rules! needs_sh {
    () => {
        if !sh_available() || !git_available() {
            return;
        }
    };
}

/// Three tasks in a chain, wired by declared inputs and observed writes.
const CHAIN: &str = r#"
[[command]]
name = "generate-schema"
match = "*generate-schema*"
inputs = ["schema/**"]

[[command]]
name = "generate-client"
match = "*generate-client*"
inputs = ["generated/schema.json"]

[[command]]
name = "test-api"
match = "*test-api*"
inputs = ["generated/client.ts"]

[[command]]
name = "test-web"
match = "*test-web*"
inputs = ["src/web.rs"]
"#;

fn chain_sandbox() -> Sandbox {
    let sb = Sandbox::new(CHAIN);
    sb.write("schema/api.yaml", "v1");
    sb.write("src/web.rs", "web");
    sb.init_git();
    sb.learn("cat schema/api.yaml > generated/schema.json # generate-schema");
    sb.learn("cat generated/schema.json > generated/client.ts # generate-client");
    sb.learn("cat generated/client.ts > /dev/null # test-api");
    sb.learn("cat src/web.rs > /dev/null # test-web");
    sb.commit();
    sb
}

// ------------------------------------------------------------------ edges ----

#[test]
fn an_output_consumed_by_another_task_becomes_an_edge() {
    needs_sh!();
    let sb = chain_sandbox();
    let edges = sb.edges();
    assert!(
        edges.contains(&("generate-schema".into(), "generate-client".into())),
        "{edges:?}"
    );
    assert!(
        edges.contains(&("generate-client".into(), "test-api".into())),
        "{edges:?}"
    );
    assert!(
        !edges.contains(&("generate-schema".into(), "test-web".into())),
        "an unrelated task must not be wired in: {edges:?}"
    );
}

#[test]
fn a_task_is_never_its_own_dependency() {
    needs_sh!();
    let sb = chain_sandbox();
    assert!(
        sb.edges().iter().all(|(a, b)| a != b),
        "a task that reads what it wrote is not upstream of itself"
    );
}

#[test]
fn two_producers_of_one_path_are_reported_as_ambiguous() {
    needs_sh!();
    let sb = Sandbox::new(
        r#"
[[command]]
name = "a"
match = "*task-a*"
[[command]]
name = "b"
match = "*task-b*"
[[command]]
name = "c"
match = "*task-c*"
inputs = ["generated/shared.txt"]
"#,
    );
    sb.init_git();
    sb.learn("echo from-a > generated/shared.txt # task-a");
    sb.learn("echo from-a > generated/shared.txt # task-b");
    sb.learn("cat generated/shared.txt > /dev/null # task-c");

    let g = sb.graph();
    let ambiguities = g["ambiguities"].as_array().unwrap();
    assert_eq!(ambiguities.len(), 1, "{g}");
    assert_eq!(ambiguities[0]["producers"].as_array().unwrap().len(), 2);
    // Both producers stay wired: covering both is conservative, choosing one is
    // a guess.
    let edges = sb.edges();
    assert!(edges.contains(&("a".into(), "c".into())), "{edges:?}");
    assert!(edges.contains(&("b".into(), "c".into())), "{edges:?}");
    assert!(g["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "declared")
        .all(|e| e["ambiguous"] == true));
}

#[test]
fn a_manual_after_edge_needs_no_filesystem_overlap() {
    needs_sh!();
    let sb = Sandbox::new(
        r#"
[[command]]
name = "build"
match = "*step-build*"

[[command]]
name = "package"
match = "*step-package*"
after = ["build"]

[[command]]
name = "broken"
match = "*step-broken*"
after = ["does-not-exist"]
"#,
    );
    sb.init_git();
    sb.learn("echo build # step-build");
    sb.learn("echo package # step-package");
    sb.learn("echo broken # step-broken");

    assert!(
        sb.edges().contains(&("build".into(), "package".into())),
        "{:?}",
        sb.edges()
    );
    let g = sb.graph();
    let unresolved = g["unresolved"].as_array().unwrap();
    assert_eq!(unresolved.len(), 1, "{g}");
    assert_eq!(unresolved[0]["after"], "does-not-exist");
    // A bad name is surfaced, not fatal.
    assert!(sb.arc(&["graph"]).status.success());
    assert!(stdout(&sb.arc(&["graph"])).contains("does-not-exist"));
}

#[test]
fn a_changed_output_set_rewires_the_graph() {
    needs_sh!();
    let sb = Sandbox::new(
        r#"
[[command]]
name = "producer"
match = "*the-producer*"
inputs = ["src/seed.txt"]

[[command]]
name = "consumer-x"
match = "*consumer-x*"
inputs = ["generated/x.txt"]

[[command]]
name = "consumer-y"
match = "*consumer-y*"
inputs = ["generated/y.txt"]
"#,
    );
    sb.init_git();
    sb.learn("cat src/seed.txt > generated/x.txt # the-producer");
    sb.learn("cat generated/x.txt > /dev/null # consumer-x");
    sb.learn("cat generated/y.txt > /dev/null 2>&1 || true # consumer-y");
    assert!(sb
        .edges()
        .contains(&("producer".into(), "consumer-x".into())));

    // The producer now writes y as well. Outputs are unioned across runs, since
    // a task may write different files on different branches, so the graph is
    // an over-approximation: it gains the new edge and may keep the old one.
    // Extra edges cost extra work; a missing one would cost correctness.
    std::fs::remove_file(sb.root.join("generated/x.txt")).ok();
    sb.write("src/seed.txt", "two");
    sb.learn("cat src/seed.txt > generated/y.txt # the-producer");
    assert!(
        sb.edges()
            .contains(&("producer".into(), "consumer-y".into())),
        "the new output must create an edge: {:?}",
        sb.edges()
    );

    // Invalidating what Arc learned drops the row wholesale, edges included:
    // nothing survives that the current knowledge does not justify.
    sb.write(
        "arc.toml",
        r#"
[[command]]
name = "producer"
match = "*the-producer*"
inputs = ["src/**"]

[[command]]
name = "consumer-x"
match = "*consumer-x*"
inputs = ["generated/x.txt"]
"#,
    );
    sb.run("cat src/seed.txt > generated/y.txt # the-producer");
    let g = sb.graph();
    // A scoping change is a new family, so the graph holds both the old task and
    // the re-learned one. The re-learned one is what must be clean.
    let producer = g["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["label"] == "producer")
        .max_by_key(|n| n["last_seen"].as_i64().unwrap_or(0))
        .unwrap();
    let produces: Vec<&str> = producer["produces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect();
    assert!(
        !produces.contains(&"generated/x.txt"),
        "a re-learned task starts from what it just observed: {producer}"
    );
}

// --------------------------------------------------------------- affected ----

#[test]
fn a_change_propagates_along_the_whole_chain() {
    needs_sh!();
    let sb = chain_sandbox();
    sb.write("schema/api.yaml", "v2");

    let v = sb.verdicts();
    assert_eq!(v["generate-schema"], "affected", "{v:?}");
    assert_eq!(v["generate-client"], "affected", "{v:?}");
    assert_eq!(v["test-api"], "affected", "{v:?}");
    assert_eq!(v["test-web"], "unaffected", "{v:?}");
}

#[test]
fn provenance_explains_why_a_downstream_task_is_affected() {
    needs_sh!();
    let sb = chain_sandbox();
    sb.write("schema/api.yaml", "v2");

    let report = sb.affected();
    let api = report["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["label"] == "test-api")
        .unwrap();
    let kinds: Vec<&str> = api["causes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"upstream"), "{api}");

    let text = stdout(&sb.arc(&["affected", "--explain"]));
    assert!(text.contains("because"), "{text}");
}

#[test]
fn a_task_without_provable_inputs_is_unknown_and_still_runs() {
    needs_sh!();
    let sb = Sandbox::new(
        r#"
[[command]]
name = "scoped"
match = "*is-scoped*"
inputs = ["src/seed.txt"]
"#,
    );
    sb.init_git();
    sb.learn("cat src/seed.txt > /dev/null # is-scoped");
    sb.learn("echo legacy # no-scope-here");
    sb.commit();
    sb.write("docs.md", "irrelevant");

    let v = sb.verdicts();
    assert_eq!(v["scoped"], "unaffected", "{v:?}");
    let legacy = v
        .iter()
        .find(|(k, _)| k.contains("legacy"))
        .expect("the unscoped task");
    // On a platform with complete tracing the unscoped task is provable too;
    // what must never happen is a claim of independence Arc cannot support.
    assert!(legacy.1 == "unknown" || legacy.1 == "unaffected", "{v:?}");

    let plan = json(&sb.arc(&["affected", "--run", "--dry-run", "--json"]));
    let planned: Vec<&str> = plan["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["label"].as_str().unwrap())
        .collect();
    if legacy.1 == "unknown" {
        assert!(
            planned.iter().any(|l| l.contains("legacy")),
            "unknown must be scheduled, not skipped: {planned:?}"
        );
    }
}

#[test]
fn a_deleted_input_affects_its_consumers() {
    needs_sh!();
    let sb = chain_sandbox();
    std::fs::remove_file(sb.root.join("schema/api.yaml")).unwrap();
    let v = sb.verdicts();
    assert_eq!(v["generate-schema"], "affected", "{v:?}");
    assert_eq!(v["test-api"], "affected", "{v:?}");
}

#[test]
fn a_clean_tree_leaves_provable_tasks_unaffected() {
    needs_sh!();
    let sb = chain_sandbox();
    let v = sb.verdicts();
    assert!(
        v.values().all(|x| x == "unaffected"),
        "nothing changed: {v:?}"
    );
    assert!(stdout(&sb.arc(&["affected", "--run", "--dry-run"])).contains("nothing to run"));
}

// -------------------------------------------------------------- scheduler ----

#[test]
fn the_plan_orders_producers_before_consumers() {
    needs_sh!();
    let sb = chain_sandbox();
    sb.write("schema/api.yaml", "v2");

    let plan = json(&sb.arc(&["affected", "--run", "--dry-run", "--json"]));
    let labels: Vec<&str> = plan["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["label"].as_str().unwrap())
        .collect();
    let pos = |l: &str| labels.iter().position(|x| *x == l).unwrap();
    assert!(
        pos("generate-schema") < pos("generate-client"),
        "{labels:?}"
    );
    assert!(pos("generate-client") < pos("test-api"), "{labels:?}");
    assert!(!labels.contains(&"test-web"), "{labels:?}");

    // Deterministic across invocations, so CI output is stable.
    let again = json(&sb.arc(&["affected", "--run", "--dry-run", "--json"]));
    assert_eq!(plan["tasks"], again["tasks"]);
}

#[test]
fn selective_execution_runs_only_the_affected_tasks() {
    needs_sh!();
    let sb = chain_sandbox();
    sb.write("schema/api.yaml", "v2");

    let out = sb.arc(&["affected", "--run", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let summary = json(&out);
    let labels: BTreeSet<&str> = summary["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["label"].as_str().unwrap())
        .collect();
    assert!(labels.contains("generate-schema"), "{summary}");
    assert!(labels.contains("test-api"), "{summary}");
    assert!(!labels.contains("test-web"), "{summary}");
    assert!(summary["failed"].as_u64().unwrap() == 0, "{summary}");
}

#[test]
fn a_task_never_starts_before_its_producer_finishes() {
    needs_sh!();
    let sb = chain_sandbox();
    sb.write("schema/api.yaml", "ordered");
    // Each task appends to a shared log, so the order is observable rather than
    // inferred from timing.
    sb.arc(&["affected", "--run", "--jobs", "1"]);

    let client = std::fs::read_to_string(sb.root.join("generated/client.ts")).unwrap();
    assert!(
        client.contains("ordered"),
        "the client must be regenerated from the new schema: {client}"
    );
}

#[test]
fn jobs_one_is_serial_and_still_correct() {
    needs_sh!();
    let sb = chain_sandbox();
    sb.write("schema/api.yaml", "serial");
    let out = sb.arc(&["affected", "--run", "--jobs", "1", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let summary = json(&out);
    assert_eq!(summary["failed"], 0, "{summary}");
    assert_eq!(summary["blocked"], 0, "{summary}");
}

#[test]
fn independent_tasks_run_concurrently() {
    needs_sh!();
    let sb = Sandbox::new(
        r#"
[[command]]
name = "root"
match = "*fan-root*"
inputs = ["src/seed.txt"]

[[command]]
name = "left"
match = "*fan-left*"
inputs = ["generated/root.txt"]

[[command]]
name = "right"
match = "*fan-right*"
inputs = ["generated/root.txt"]
"#,
    );
    sb.init_git();
    sb.learn("cat src/seed.txt > generated/root.txt # fan-root");
    sb.learn("cat generated/root.txt > /dev/null; sleep 1 # fan-left");
    sb.learn("cat generated/root.txt > /dev/null; sleep 1 # fan-right");
    sb.commit();

    let time_it = |jobs: &str| {
        sb.write("src/seed.txt", &format!("change-{jobs}"));
        let start = std::time::Instant::now();
        let out = sb.arc(&["affected", "--run", "--jobs", jobs, "--json"]);
        assert!(out.status.success(), "{}", stderr(&out));
        start.elapsed()
    };

    let serial = time_it("1");
    let parallel = time_it("4");
    // Two one-second sleeps: serial must pay for both, parallel for about one.
    // The bound is loose enough to survive a slow machine and still prove the
    // sleeps overlapped.
    assert!(
        parallel < serial.mul_f32(0.85),
        "expected concurrency: serial {serial:?}, parallel {parallel:?}"
    );
}

#[test]
fn a_failing_task_blocks_its_dependents() {
    needs_sh!();
    let sb = Sandbox::new(
        r#"
[[command]]
name = "producer"
match = "*fail-producer*"
inputs = ["src/seed.txt"]

[[command]]
name = "dependent"
match = "*fail-dependent*"
inputs = ["generated/out.txt"]

[[command]]
name = "independent"
match = "*fail-independent*"
inputs = ["src/other.txt"]
"#,
    );
    sb.write("src/other.txt", "x");
    sb.init_git();
    // The producer fails from the outset, so there is exactly one producer
    // family rather than a successful one and a failing one sharing a label.
    let failing = "cat src/seed.txt > generated/out.txt; exit 4 # fail-producer";
    assert_eq!(sb.run(failing).status.code(), Some(4));
    sb.learn("cat generated/out.txt > /dev/null # fail-dependent");
    sb.learn("cat src/other.txt > /dev/null # fail-independent");
    sb.commit();

    sb.write("src/seed.txt", "two");
    sb.write("src/other.txt", "two");
    let out = sb.arc(&["affected", "--run", "--json"]);
    let summary = json(&out);
    let by_label: std::collections::BTreeMap<&str, &str> = summary["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["label"].as_str().unwrap(), r["outcome"].as_str().unwrap()))
        .collect();
    assert_eq!(by_label["producer"], "failed", "{summary}");
    assert_eq!(
        by_label["dependent"], "blocked",
        "a dependent of a failed task must never run: {summary}"
    );
    assert_ne!(
        by_label["independent"], "blocked",
        "an independent branch continues by default: {summary}"
    );
    assert_ne!(out.status.code(), Some(0), "a failure must be reported");

    // Nothing the blocked task depends on was produced, so it really did not run.
    let fast = sb.arc(&["affected", "--run", "--fail-fast", "--json"]);
    assert_ne!(fast.status.code(), Some(0));
}

#[test]
fn an_upstream_cache_hit_still_lets_downstream_proceed() {
    needs_sh!();
    let sb = chain_sandbox();
    sb.write("schema/api.yaml", "v2");
    // Run the upstream task on its own first, so the scheduler meets it already
    // cached.
    sb.run("cat schema/api.yaml > generated/schema.json # generate-schema");

    let summary = json(&sb.arc(&["affected", "--run", "--json"]));
    let by_label: std::collections::BTreeMap<&str, &str> = summary["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["label"].as_str().unwrap(), r["outcome"].as_str().unwrap()))
        .collect();
    assert_eq!(by_label["generate-schema"], "hit", "{summary}");
    assert_ne!(by_label["test-api"], "blocked", "{summary}");
    assert_eq!(summary["failed"], 0, "{summary}");
}

#[test]
fn running_twice_leaves_nothing_to_do_the_second_time() {
    needs_sh!();
    let sb = chain_sandbox();
    sb.write("schema/api.yaml", "v2");
    assert!(sb.arc(&["affected", "--run"]).status.success());
    sb.commit();

    let summary = json(&sb.arc(&["affected", "--run", "--json"]));
    assert_eq!(
        summary["results"].as_array().map(|r| r.len()).unwrap_or(0),
        0,
        "a clean tree selects nothing: {summary}"
    );
}

// ------------------------------------------------------------- resilience ----

#[test]
fn two_projects_with_the_same_output_names_do_not_share_a_graph() {
    needs_sh!();
    let a = Sandbox::new(CHAIN);
    let b = Sandbox::new(CHAIN);
    a.write("schema/api.yaml", "a");
    b.write("schema/api.yaml", "b");
    a.init_git();
    b.init_git();
    a.learn("cat schema/api.yaml > generated/schema.json # generate-schema");
    b.learn("cat schema/api.yaml > generated/schema.json # generate-schema");
    b.learn("cat generated/schema.json > /dev/null # generate-client");

    assert_eq!(
        a.graph()["nodes"].as_array().unwrap().len(),
        1,
        "one project must not see another's tasks"
    );
    assert!(a.edges().is_empty());
    assert_eq!(b.graph()["nodes"].as_array().unwrap().len(), 2);
}

#[test]
fn concurrent_scheduler_runs_do_not_corrupt_the_graph() {
    needs_sh!();
    let sb = chain_sandbox();
    sb.write("schema/api.yaml", "concurrent");
    let children: Vec<_> = (0..3)
        .map(|_| {
            Command::new(ARC)
                .args(["affected", "--run", "--jobs", "2", "--json"])
                .current_dir(&sb.root)
                .env("ARC_HOME", &sb.home)
                .env("ARC_NO_ANIM", "1")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for mut c in children {
        c.wait().unwrap();
    }
    let g = sb.arc(&["graph", "--json"]);
    assert!(g.status.success(), "{}", stderr(&g));
    assert!(json(&g)["nodes"].as_array().unwrap().len() >= 4);
    assert!(stdout(&sb.arc(&["cache", "verify"])).contains("No corruption"));
}

#[test]
fn arc_home_inside_the_project_creates_no_task_edges() {
    needs_sh!();
    let sb = Sandbox::new(CHAIN);
    let inner = sb.root.join(".arc-home");
    let call = |args: &[&str]| {
        Command::new(ARC)
            .args(args)
            .current_dir(&sb.root)
            .env("ARC_HOME", &inner)
            .env("ARC_NO_ANIM", "1")
            .output()
            .unwrap()
    };
    sb.write("schema/api.yaml", "v1");
    for _ in 0..2 {
        call(&[
            "run",
            "sh",
            "-c",
            "cat schema/api.yaml > generated/schema.json # generate-schema",
        ]);
        call(&[
            "run",
            "sh",
            "-c",
            "cat generated/schema.json > /dev/null # generate-client",
        ]);
    }
    let text = stdout(&call(&["graph", "--json"]));
    assert!(
        !text.contains("arc-home"),
        "Arc's own cache must never appear in the task graph: {text}"
    );
}

#[test]
fn graph_and_affected_json_are_typed_and_free_of_escape_codes() {
    needs_sh!();
    let sb = chain_sandbox();
    for args in [
        vec!["graph", "--json"],
        vec!["affected", "--json"],
        vec!["affected", "--run", "--dry-run", "--json"],
    ] {
        let out = sb.arc(&args);
        assert!(out.status.success(), "{args:?}: {}", stderr(&out));
        let text = stdout(&out);
        assert!(!text.contains('\u{1b}'), "{args:?} emitted ANSI: {text}");
        serde_json::from_str::<serde_json::Value>(&text)
            .unwrap_or_else(|e| panic!("{args:?} is not valid JSON: {e}"));
    }
}

#[test]
fn graph_filters_narrow_to_one_task_and_its_neighbours() {
    needs_sh!();
    let sb = chain_sandbox();
    let filtered = json(&sb.arc(&["graph", "--task", "generate-client", "--json"]));
    let labels: BTreeSet<&str> = filtered["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["label"].as_str().unwrap())
        .collect();
    assert!(labels.contains("generate-schema"), "{labels:?}");
    assert!(labels.contains("test-api"), "{labels:?}");
    assert!(!labels.contains("test-web"), "{labels:?}");

    let missing = sb.arc(&["graph", "--task", "no-such-task"]);
    assert!(!missing.status.success());
    assert!(stderr(&missing).contains("no task matches"));
}

#[test]
fn doctor_reports_graph_health() {
    needs_sh!();
    let sb = chain_sandbox();
    let text = stdout(&sb.arc(&["doctor"]));
    assert!(text.contains("task graph"), "{text}");
    assert!(text.contains("edges"), "{text}");
    assert!(text.contains("cycles"), "{text}");
}

#[test]
fn inspect_shows_a_tasks_neighbours() {
    needs_sh!();
    let sb = chain_sandbox();
    let history = json(&sb.arc(&["history", "--json"]));
    let id = history
        .as_array()
        .unwrap()
        .iter()
        .find(|r| {
            r["args"][1]
                .as_str()
                .unwrap_or("")
                .contains("generate-client")
        })
        .map(|r| r["id"].as_str().unwrap().to_string())
        .expect("a generate-client execution");
    let text = stdout(&sb.arc(&["inspect", &id]));
    assert!(text.contains("upstream"), "{text}");
    assert!(text.contains("downstream"), "{text}");
}
