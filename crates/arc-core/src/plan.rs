//! Turning a set of tasks into an order they can safely run in, and running it.
//!
//! The plan is data: inspectable before anything executes, and reusable by
//! anything that needs "given these changes, what is the minimal safe order?".
//! The scheduler is the only part that runs processes, and it runs them through
//! `arc run`, so every task still passes through the ordinary cache, trace and
//! learn pipeline. Affected does not mean executed — a task whose exact state is
//! already cached restores instead.

use crate::affected::{Report, Verdict};
use crate::exec;
use crate::graph::{self, TaskGraph};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedTask {
    pub family_key: String,
    pub label: String,
    pub program: String,
    pub args: Vec<String>,
    pub rel_cwd: String,
    pub verdict: Verdict,
    /// Family keys within the plan that must finish first.
    pub after: Vec<String>,
    /// Members of the same dependency cycle, which run serially in this order.
    pub cycle_group: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionPlan {
    pub schema: u32,
    pub project_root: String,
    pub tasks: Vec<PlannedTask>,
    /// Groups that could start simultaneously, for display. The scheduler uses
    /// the dependency edges directly and does not wait for a whole wave.
    pub waves: Vec<Vec<String>>,
    pub total_known_tasks: usize,
    pub skipped: usize,
}

impl ExecutionPlan {
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    pub fn task(&self, family_key: &str) -> Option<&PlannedTask> {
        self.tasks.iter().find(|t| t.family_key == family_key)
    }
}

/// Build a plan for everything the report did not prove unaffected.
pub fn build(graph: &TaskGraph, report: &Report) -> ExecutionPlan {
    let selected: BTreeSet<String> = report
        .selected()
        .iter()
        .map(|t| t.family_key.clone())
        .collect();
    plan_for(graph, report, &selected)
}

pub fn plan_for(graph: &TaskGraph, report: &Report, selected: &BTreeSet<String>) -> ExecutionPlan {
    let order = graph::topological(graph, selected);
    let verdicts: HashMap<&str, Verdict> = report
        .tasks
        .iter()
        .map(|t| (t.family_key.as_str(), t.verdict))
        .collect();
    let group_of: HashMap<&str, usize> = order
        .iter()
        .enumerate()
        .flat_map(|(i, g)| g.iter().map(move |k| (k.as_str(), i)))
        .collect();

    let mut tasks = Vec::new();
    for (gi, group) in order.iter().enumerate() {
        for key in group {
            let Some(node) = graph.node(key) else {
                continue;
            };
            let mut after: Vec<String> = graph
                .incoming(key)
                .map(|e| e.from.clone())
                .filter(|f| selected.contains(f) && group_of.get(f.as_str()) != Some(&gi))
                .collect();
            after.sort();
            after.dedup();
            tasks.push(PlannedTask {
                family_key: key.clone(),
                label: node.label.clone(),
                program: node.program.clone(),
                args: node.args.clone(),
                rel_cwd: node.rel_cwd.clone(),
                verdict: verdicts
                    .get(key.as_str())
                    .copied()
                    .unwrap_or(Verdict::Unknown),
                after,
                cycle_group: if group.len() > 1 {
                    group.clone()
                } else {
                    Vec::new()
                },
            });
        }
    }

    let waves = waves_of(&tasks);
    ExecutionPlan {
        schema: graph::GRAPH_SCHEMA_VERSION,
        project_root: graph.project_root.clone(),
        total_known_tasks: graph.nodes.len(),
        skipped: graph.nodes.len().saturating_sub(tasks.len()),
        tasks,
        waves,
    }
}

/// Longest-path depth from a root, which is what "could start at the same time"
/// actually means. Presentation only.
fn waves_of(tasks: &[PlannedTask]) -> Vec<Vec<String>> {
    let mut depth: HashMap<&str, usize> = HashMap::new();
    for t in tasks {
        let d = t
            .after
            .iter()
            .filter_map(|a| depth.get(a.as_str()).copied())
            .max()
            .map(|m| m + 1)
            .unwrap_or(0);
        depth.insert(t.family_key.as_str(), d);
    }
    let max = depth.values().copied().max().map(|m| m + 1).unwrap_or(0);
    let mut waves = vec![Vec::new(); max];
    for t in tasks {
        waves[depth[t.family_key.as_str()]].push(t.label.clone());
    }
    for w in &mut waves {
        w.sort();
    }
    waves
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskOutcome {
    Hit,
    Ran,
    Failed,
    /// A prerequisite failed, so this task was never started.
    Blocked,
    /// Scheduling stopped before reaching this task.
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    pub family_key: String,
    pub label: String,
    pub outcome: TaskOutcome,
    pub exit_code: i32,
    pub duration_ms: u64,
    pub stdout: String,
    pub stderr: String,
    /// The prerequisite that blocked this task, when it was blocked.
    pub blocked_by: Option<String>,
    /// `local` or `remote` for a hit, as the child reported it.
    #[serde(default)]
    pub cache_source: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunSummary {
    pub results: Vec<TaskResult>,
    pub ran: usize,
    pub hits: usize,
    pub failed: usize,
    pub blocked: usize,
    pub duration_ms: u64,
}

impl RunSummary {
    /// Zero when every selected task succeeded or was restored. One otherwise:
    /// individual exit codes cannot be preserved when several tasks fail, so a
    /// single failure indicator is the honest summary.
    pub fn exit_code(&self) -> i32 {
        i32::from(self.failed > 0 || self.blocked > 0)
    }
}

#[derive(Debug, Clone)]
pub struct SchedulerOptions {
    pub jobs: usize,
    /// Stop starting new work after the first failure.
    pub fail_fast: bool,
    /// Pass `--refresh` to each task.
    pub refresh: bool,
    pub trace_backend: crate::trace::Selection,
    /// Extra environment for every task, used by `arc ci` to impose a policy —
    /// notably a read-only remote — on work it did not itself perform.
    pub child_env: Vec<(String, String)>,
}

impl Default for SchedulerOptions {
    fn default() -> Self {
        Self {
            jobs: default_jobs(),
            fail_fast: false,
            refresh: false,
            trace_backend: crate::trace::Selection::Auto,
            child_env: Vec::new(),
        }
    }
}

pub fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(16)
}

/// Reported as each task finishes, so a caller can print progress without the
/// scheduler knowing anything about terminals.
pub trait Observer: Send + Sync {
    fn started(&self, _task: &PlannedTask) {}
    fn finished(&self, _result: &TaskResult) {}
}

impl Observer for () {}

struct Shared {
    pending: HashMap<String, PlannedTask>,
    done: HashMap<String, TaskOutcome>,
    running: HashSet<String>,
    results: Vec<TaskResult>,
    stop: bool,
}

/// Execute a plan with bounded parallelism.
///
/// Each task is run by invoking `arc run` on this same executable with a
/// structured argument vector: no shell, no re-implementation of the engine, and
/// no two threads contending for the metadata database inside one process.
pub fn execute(
    plan: &ExecutionPlan,
    project_root: &Path,
    arc_home: &Path,
    opts: &SchedulerOptions,
    observer: &dyn Observer,
) -> Result<RunSummary> {
    let started = Instant::now();
    let arc = std::env::current_exe().context("locating the Arc executable")?;
    let cancelled = Arc::new(AtomicBool::new(false));
    install_cancel(&cancelled);

    let shared = Mutex::new(Shared {
        pending: plan
            .tasks
            .iter()
            .map(|t| (t.family_key.clone(), t.clone()))
            .collect(),
        done: HashMap::new(),
        running: HashSet::new(),
        results: Vec::new(),
        stop: false,
    });
    let wake = Condvar::new();
    let jobs = opts.jobs.max(1);

    std::thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| loop {
                let task = {
                    let mut s = shared.lock().unwrap();
                    loop {
                        if s.pending.is_empty() {
                            wake.notify_all();
                            return;
                        }
                        if cancelled.load(Ordering::Relaxed) || s.stop {
                            drain_cancelled(&mut s, TaskOutcome::Cancelled);
                            wake.notify_all();
                            return;
                        }
                        match claim(&mut s) {
                            Claim::Ready(t) => break t,
                            Claim::Blocked => {
                                wake.notify_all();
                                continue;
                            }
                            Claim::Wait if s.running.is_empty() => {
                                // Nothing running and nothing ready: only
                                // possible if the plan is inconsistent. Release
                                // the rest rather than block forever.
                                drain_cancelled(&mut s, TaskOutcome::Cancelled);
                                wake.notify_all();
                                return;
                            }
                            Claim::Wait => s = wake.wait(s).unwrap(),
                        }
                    }
                };

                observer.started(&task);
                let result = run_task(&arc, &task, project_root, arc_home, opts, &cancelled);

                let mut s = shared.lock().unwrap();
                s.running.remove(&task.family_key);
                s.done.insert(task.family_key.clone(), result.outcome);
                if opts.fail_fast && result.outcome == TaskOutcome::Failed {
                    s.stop = true;
                }
                observer.finished(&result);
                s.results.push(result);
                wake.notify_all();
            });
        }
    });

    let shared = shared.into_inner().unwrap();
    let mut results = shared.results;
    results.sort_by(|a, b| (&a.label, &a.family_key).cmp(&(&b.label, &b.family_key)));
    Ok(RunSummary {
        ran: results
            .iter()
            .filter(|r| r.outcome == TaskOutcome::Ran)
            .count(),
        hits: results
            .iter()
            .filter(|r| r.outcome == TaskOutcome::Hit)
            .count(),
        failed: results
            .iter()
            .filter(|r| r.outcome == TaskOutcome::Failed)
            .count(),
        blocked: results
            .iter()
            .filter(|r| matches!(r.outcome, TaskOutcome::Blocked | TaskOutcome::Cancelled))
            .count(),
        results,
        duration_ms: started.elapsed().as_millis() as u64,
    })
}

enum Claim {
    Ready(PlannedTask),
    /// A task was moved straight to blocked; caller should look again.
    Blocked,
    Wait,
}

fn claim(s: &mut Shared) -> Claim {
    let mut ready: Vec<&PlannedTask> = Vec::new();
    let mut blocked: Option<(String, String)> = None;

    let mut keys: Vec<&String> = s.pending.keys().collect();
    keys.sort();
    for key in keys {
        let t = &s.pending[key];
        // A cycle group runs serially: no member may start while another is
        // running or pending, which keeps the group ordered without pretending
        // the cycle is a DAG.
        if !t.cycle_group.is_empty()
            && t.cycle_group
                .iter()
                .any(|m| m != &t.family_key && s.running.contains(m))
        {
            continue;
        }
        if let Some(dead) = t.after.iter().find(|a| {
            matches!(
                s.done.get(a.as_str()),
                Some(TaskOutcome::Failed)
                    | Some(TaskOutcome::Blocked)
                    | Some(TaskOutcome::Cancelled)
            )
        }) {
            blocked = Some((t.family_key.clone(), dead.clone()));
            break;
        }
        if t.after.iter().all(|a| s.done.contains_key(a.as_str())) {
            ready.push(t);
        }
    }

    if let Some((key, cause)) = blocked {
        let t = s.pending.remove(&key).expect("just found");
        s.done.insert(key.clone(), TaskOutcome::Blocked);
        s.results.push(TaskResult {
            family_key: key,
            label: t.label,
            outcome: TaskOutcome::Blocked,
            exit_code: 1,
            duration_ms: 0,
            stdout: String::new(),
            stderr: String::new(),
            blocked_by: Some(cause),
            cache_source: None,
        });
        return Claim::Blocked;
    }

    ready.sort_by(|a, b| (&a.label, &a.family_key).cmp(&(&b.label, &b.family_key)));
    let Some(next) = ready.first().map(|t| t.family_key.clone()) else {
        return Claim::Wait;
    };
    let task = s.pending.remove(&next).expect("just found");
    s.running.insert(next);
    Claim::Ready(task)
}

fn drain_cancelled(s: &mut Shared, outcome: TaskOutcome) {
    let mut keys: Vec<String> = s.pending.keys().cloned().collect();
    keys.sort();
    for key in keys {
        let t = s.pending.remove(&key).expect("just listed");
        s.done.insert(key.clone(), outcome);
        s.results.push(TaskResult {
            family_key: key,
            label: t.label,
            outcome,
            exit_code: 1,
            duration_ms: 0,
            stdout: String::new(),
            stderr: String::new(),
            blocked_by: None,
            cache_source: None,
        });
    }
}

fn run_task(
    arc: &Path,
    task: &PlannedTask,
    project_root: &Path,
    arc_home: &Path,
    opts: &SchedulerOptions,
    cancelled: &AtomicBool,
) -> TaskResult {
    let started = Instant::now();
    if cancelled.load(Ordering::Relaxed) {
        return TaskResult {
            family_key: task.family_key.clone(),
            label: task.label.clone(),
            outcome: TaskOutcome::Cancelled,
            exit_code: 1,
            duration_ms: 0,
            stdout: String::new(),
            stderr: String::new(),
            blocked_by: None,
            cache_source: None,
        };
    }

    let cwd = task_cwd(project_root, &task.rel_cwd);
    let mut args: Vec<String> = vec!["run".into(), "--json".into()];
    if opts.refresh {
        args.push("--refresh".into());
    }
    if opts.trace_backend != crate::trace::Selection::Auto {
        args.push("--trace-backend".into());
        args.push(opts.trace_backend.name().into());
    }
    args.push("--".into());
    args.push(task.program.clone());
    args.extend(task.args.iter().cloned());

    // Every scheduled task may fetch from the remote at once, so each child's
    // transfer pool is divided by the number of children. Without this,
    // `--jobs 16` would mean sixteen simultaneous transfer pools.
    let transfers = crate::remote::RemoteConfig::default()
        .concurrency
        .div_ceil(opts.jobs.max(1))
        .max(1)
        .to_string();
    let mut env: Vec<(&str, &std::ffi::OsStr)> = vec![
        ("ARC_HOME", arc_home.as_os_str()),
        ("ARC_REMOTE_CONCURRENCY", transfers.as_ref()),
    ];
    env.extend(
        opts.child_env
            .iter()
            .map(|(k, v)| (k.as_str(), std::ffi::OsStr::new(v.as_str()))),
    );
    let out = match exec::capture(arc, &args, &cwd, &env) {
        Ok(o) => o,
        Err(e) => {
            return TaskResult {
                family_key: task.family_key.clone(),
                label: task.label.clone(),
                outcome: TaskOutcome::Failed,
                exit_code: 1,
                duration_ms: started.elapsed().as_millis() as u64,
                stdout: String::new(),
                stderr: format!("{e:#}"),
                blocked_by: None,
                cache_source: None,
            }
        }
    };
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let reported = child_report(&stderr);
    let outcome = match (
        out.exit_code,
        reported.as_ref().map(|r| r.0).unwrap_or(false),
    ) {
        (0, true) => TaskOutcome::Hit,
        (0, false) => TaskOutcome::Ran,
        _ => TaskOutcome::Failed,
    };
    TaskResult {
        cache_source: reported.and_then(|r| r.1),
        family_key: task.family_key.clone(),
        label: task.label.clone(),
        outcome,
        exit_code: out.exit_code,
        duration_ms: started.elapsed().as_millis() as u64,
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: strip_json_line(&stderr),
        blocked_by: None,
    }
}

/// The `--json` line is the scheduler's channel back from the child; parsing it
/// beats scraping the human-facing banner. Returns whether it was a hit, and
/// which cache served it.
fn child_report(stderr: &str) -> Option<(bool, Option<String>)> {
    let v = stderr
        .lines()
        .filter(|l| l.trim_start().starts_with('{'))
        .find_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())?;
    let hit = v.get("cache_status").and_then(|s| s.as_str()) == Some("HIT");
    let source = v
        .pointer("/cache/source")
        .and_then(|s| s.as_str())
        .filter(|s| *s != "none")
        .map(str::to_string);
    Some((hit, source))
}

fn task_cwd(project_root: &Path, rel_cwd: &str) -> PathBuf {
    if rel_cwd.is_empty() {
        project_root.to_path_buf()
    } else {
        project_root.join(rel_cwd)
    }
}

/// `--json` puts one machine-readable line on stderr. It is for the scheduler,
/// not for the user reading a task's log.
fn strip_json_line(s: &str) -> String {
    s.lines()
        .filter(|l| !l.trim_start().starts_with('{'))
        .collect::<Vec<_>>()
        .join("\n")
}

fn install_cancel(flag: &Arc<AtomicBool>) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    let flag = flag.clone();
    ONCE.call_once(move || {
        // Ctrl-C stops new work being scheduled. Children already running are in
        // the same process group and receive the signal from the terminal, and
        // each is reaped normally, so nothing is orphaned.
        let _ = ctrlc_handler(flag);
    });
}

#[cfg(unix)]
fn ctrlc_handler(flag: Arc<AtomicBool>) -> Result<()> {
    // SAFETY: the handler only stores into an `AtomicBool`, which is
    // async-signal-safe. It allocates nothing and takes no locks.
    unsafe {
        CANCEL_FLAG = Some(flag);
        libc::signal(
            libc::SIGINT,
            handle_sigint as *const () as libc::sighandler_t,
        );
    }
    Ok(())
}

#[cfg(unix)]
static mut CANCEL_FLAG: Option<Arc<AtomicBool>> = None;

#[cfg(unix)]
extern "C" fn handle_sigint(_: libc::c_int) {
    // SAFETY: `CANCEL_FLAG` is written once before the handler can run, and the
    // handler only performs a relaxed atomic store through it.
    unsafe {
        if let Some(f) = (*std::ptr::addr_of!(CANCEL_FLAG)).as_ref() {
            f.store(true, Ordering::Relaxed);
        }
    }
}

#[cfg(not(unix))]
fn ctrlc_handler(_flag: Arc<AtomicBool>) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::affected::analyse;
    use crate::dependency::Completeness;
    use crate::graph::{Consumed, Consumes, TaskNode, GRAPH_SCHEMA_VERSION};

    fn node(label: &str, produces: &[&str], consumes: &[&str]) -> TaskNode {
        TaskNode {
            schema: GRAPH_SCHEMA_VERSION,
            family_key: format!("key-{label}"),
            program: "sh".into(),
            args: vec!["-c".into(), label.into()],
            rel_cwd: String::new(),
            project_root: "/repo".into(),
            label: label.into(),
            named: true,
            completeness: Completeness::Complete,
            inputs_narrowed: true,
            downgrades: 0,
            produces: produces.iter().map(|s| s.to_string()).collect(),
            consumes: consumes
                .iter()
                .map(|p| Consumed {
                    path: p.to_string(),
                    kind: Consumes::File,
                })
                .collect(),
            declared_inputs: Vec::new(),
            after: Vec::new(),
            runs: 1,
            last_seen: 0,
            observations: 1,
        }
    }

    fn diamond() -> TaskGraph {
        graph::assemble(
            "/repo".into(),
            vec![
                node("a", &["x"], &["src.txt"]),
                node("b", &["y"], &["x"]),
                node("c", &["z"], &["x"]),
                node("d", &[], &["y", "z"]),
            ],
        )
    }

    #[test]
    fn a_diamond_plans_producers_before_consumers_and_joins_once() {
        let g = diamond();
        let report = analyse(&g, &["src.txt".to_string()]).unwrap();
        let plan = build(&g, &report);
        assert_eq!(plan.tasks.len(), 4);
        assert_eq!(plan.tasks.iter().filter(|t| t.label == "d").count(), 1);

        let pos = |l: &str| plan.tasks.iter().position(|t| t.label == l).unwrap();
        assert!(pos("a") < pos("b"));
        assert!(pos("a") < pos("c"));
        assert!(pos("b") < pos("d"));
        assert!(pos("c") < pos("d"));

        assert_eq!(plan.waves.len(), 3);
        assert_eq!(plan.waves[1], vec!["b", "c"], "b and c can start together");
    }

    #[test]
    fn an_unaffected_task_is_not_planned() {
        let g = graph::assemble(
            "/repo".into(),
            vec![
                node("a", &["x"], &["src.txt"]),
                node("b", &[], &["x"]),
                node("elsewhere", &[], &["other.txt"]),
            ],
        );
        let report = analyse(&g, &["src.txt".to_string()]).unwrap();
        let plan = build(&g, &report);
        assert_eq!(plan.tasks.len(), 2);
        assert_eq!(plan.skipped, 1);
        assert!(plan.task("key-elsewhere").is_none());
    }

    #[test]
    fn an_unknown_task_is_planned_because_unknown_is_not_unaffected() {
        let mut murky = node("murky", &[], &[]);
        murky.inputs_narrowed = false;
        murky.completeness = Completeness::Partial;
        let g = graph::assemble("/repo".into(), vec![node("a", &[], &["x"]), murky]);
        let report = analyse(&g, &[]).unwrap();
        let plan = build(&g, &report);
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(plan.tasks[0].label, "murky");
    }

    #[test]
    fn a_cycle_becomes_one_serial_group_rather_than_a_deadlock() {
        let g = graph::assemble(
            "/repo".into(),
            vec![
                node("a", &["x"], &["y", "src.txt"]),
                node("b", &["y"], &["x"]),
            ],
        );
        let report = analyse(&g, &["src.txt".to_string()]).unwrap();
        let plan = build(&g, &report);
        assert_eq!(plan.tasks.len(), 2);
        for t in &plan.tasks {
            assert_eq!(t.cycle_group.len(), 2);
            assert!(t.after.is_empty(), "a cycle member cannot wait on its peer");
        }
    }

    #[test]
    fn plan_order_is_stable_across_builds() {
        let g = diamond();
        let report = analyse(&g, &["src.txt".to_string()]).unwrap();
        let a: Vec<String> = build(&g, &report)
            .tasks
            .iter()
            .map(|t| t.label.clone())
            .collect();
        let b: Vec<String> = build(&g, &report)
            .tasks
            .iter()
            .map(|t| t.label.clone())
            .collect();
        assert_eq!(a, b);
    }
}
