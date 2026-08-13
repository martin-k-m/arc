//! CI integration: what changed, what that reaches, and what actually ran.
//!
//! Provider knowledge stops at [`context`] and [`github`]. Everything else
//! works from [`CiContext`](context::CiContext) and the ordinary task graph, so
//! supporting another CI system means adding a detector, not a code path.

pub mod analysis;
pub mod context;
pub mod github;
pub mod revisions;
pub mod tasks;

use crate::affected::Verdict;
use crate::plan::{self, RunSummary};
use analysis::CiAnalysis;
use serde::{Deserialize, Serialize};
use tasks::Knowledge;

/// Variables that identify a *run* rather than describe the work.
///
/// Arc never fingerprints these by accident: the environment digest is an
/// allowlist, so a variable participates in a cache key only when `DEFAULT_ENV`
/// or `[env] include` names it. This list exists so Arc can say something when a
/// project has explicitly included one, because doing so guarantees a miss on
/// every CI run and it is rarely what anyone meant.
pub const VOLATILE_CI_ENV: &[&str] = &[
    "GITHUB_ACTION",
    "GITHUB_EVENT_PATH",
    "GITHUB_JOB",
    "GITHUB_OUTPUT",
    "GITHUB_RUN_ATTEMPT",
    "GITHUB_RUN_ID",
    "GITHUB_RUN_NUMBER",
    "GITHUB_SHA",
    "GITHUB_STEP_SUMMARY",
    "RUNNER_NAME",
    "RUNNER_TEMP",
    "BUILD_ID",
    "BUILD_NUMBER",
];

pub fn volatile_env_warnings(cfg: &crate::project::Config) -> Vec<String> {
    cfg.env
        .include
        .iter()
        .filter(|n| VOLATILE_CI_ENV.iter().any(|v| v.eq_ignore_ascii_case(n)))
        .map(|n| {
            format!("[env] include lists {n}, which changes every CI run; every task will miss")
        })
        .collect()
}

/// Strip anything that could move a cursor, clear a screen, or start a workflow
/// command when a provider-supplied string is printed. Presentation only:
/// nothing in Arc's identity model ever passes through here.
pub fn sanitize(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| match c {
            '\n' | '\t' => ' ',
            c if c.is_control() => '\u{fffd}',
            c => c,
        })
        .collect();
    if cleaned.chars().count() > 200 {
        cleaned.chars().take(197).collect::<String>() + "..."
    } else {
        cleaned
    }
}

/// Escape a value for a Markdown table cell, where a stray `|` would forge a
/// column and a backtick would end a code span.
pub fn markdown_cell(s: &str) -> String {
    sanitize(s)
        .replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace('`', "'")
}

/// What became of one task. Distinct variants rather than flags, because
/// "skipped because proven unaffected" and "hit because the state was already
/// computed" are different kinds of saved work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskOutcome {
    SkippedUnaffected,
    LocalHit,
    RemoteHit,
    ExecutedSuccess,
    /// Executed because Arc could not prove it unaffected, and succeeded.
    UnknownExecuted,
    ExecutedFailure,
    Blocked,
}

impl TaskOutcome {
    pub fn label(&self) -> &'static str {
        match self {
            TaskOutcome::SkippedUnaffected => "SKIPPED",
            TaskOutcome::LocalHit => "LOCAL HIT",
            TaskOutcome::RemoteHit => "REMOTE HIT",
            TaskOutcome::ExecutedSuccess => "EXECUTED",
            TaskOutcome::UnknownExecuted => "EXECUTED",
            TaskOutcome::ExecutedFailure => "FAILED",
            TaskOutcome::Blocked => "BLOCKED",
        }
    }

    pub fn failed(&self) -> bool {
        matches!(self, TaskOutcome::ExecutedFailure | TaskOutcome::Blocked)
    }

    /// Whether the work was avoided rather than performed.
    pub fn avoided(&self) -> bool {
        matches!(
            self,
            TaskOutcome::SkippedUnaffected | TaskOutcome::LocalHit | TaskOutcome::RemoteHit
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskReport {
    pub name: String,
    pub family_key: String,
    pub outcome: TaskOutcome,
    pub verdict: Option<Verdict>,
    pub knowledge: Option<Knowledge>,
    pub duration_ms: u64,
    /// Recorded median for this task, when Arc has one.
    pub estimated_ms: Option<u64>,
    pub reasons: Vec<String>,
}

/// Every count `arc ci` reports, derived once so the terminal, the JSON and the
/// job summary cannot disagree.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Counts {
    pub known: usize,
    pub selected: usize,
    pub skipped: usize,
    pub affected: usize,
    pub unknown: usize,
    pub local_hits: usize,
    pub remote_hits: usize,
    pub executed: usize,
    pub failed: usize,
    pub blocked: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CiRunSummary {
    pub counts: Counts,
    pub tasks: Vec<TaskReport>,
    /// Recorded work Arc did not have to repeat. An estimate, from medians of
    /// past executions of the same tasks.
    pub estimated_avoided_ms: u64,
    /// Avoided tasks Arc has never timed, so their contribution is unknown.
    pub avoided_without_history: usize,
    pub work_ms: u64,
    pub arc_ms: u64,
    pub exit_code: i32,
}

impl CiRunSummary {
    /// Fold a scheduler run into the analysis that produced its plan.
    pub fn build(analysis: &CiAnalysis, run: Option<&RunSummary>) -> CiRunSummary {
        let mut tasks = Vec::new();
        let mut counts = Counts {
            known: analysis.known_tasks,
            selected: analysis.selected.len(),
            skipped: analysis.skipped.len(),
            affected: analysis.affected(),
            unknown: analysis.unknown(),
            ..Default::default()
        };

        for name in &analysis.skipped {
            tasks.push(TaskReport {
                name: name.clone(),
                family_key: String::new(),
                outcome: TaskOutcome::SkippedUnaffected,
                verdict: Some(Verdict::Unaffected),
                knowledge: None,
                duration_ms: 0,
                estimated_ms: None,
                reasons: vec!["no known dependency intersects the change".into()],
            });
        }

        for entry in &analysis.selected {
            let result = run.and_then(|r| {
                r.results
                    .iter()
                    .find(|res| res.family_key == entry.family_key)
            });
            let outcome = match result {
                None => continue,
                Some(r) => match r.outcome {
                    plan::TaskOutcome::Hit => match r.cache_source.as_deref() {
                        Some("remote") => TaskOutcome::RemoteHit,
                        _ => TaskOutcome::LocalHit,
                    },
                    plan::TaskOutcome::Ran if entry.verdict == Verdict::Unknown => {
                        TaskOutcome::UnknownExecuted
                    }
                    plan::TaskOutcome::Ran => TaskOutcome::ExecutedSuccess,
                    plan::TaskOutcome::Failed => TaskOutcome::ExecutedFailure,
                    plan::TaskOutcome::Blocked | plan::TaskOutcome::Cancelled => {
                        TaskOutcome::Blocked
                    }
                },
            };
            match outcome {
                TaskOutcome::LocalHit => counts.local_hits += 1,
                TaskOutcome::RemoteHit => counts.remote_hits += 1,
                TaskOutcome::ExecutedSuccess | TaskOutcome::UnknownExecuted => counts.executed += 1,
                TaskOutcome::ExecutedFailure => counts.failed += 1,
                TaskOutcome::Blocked => counts.blocked += 1,
                TaskOutcome::SkippedUnaffected => {}
            }
            tasks.push(TaskReport {
                name: entry.name.clone(),
                family_key: entry.family_key.clone(),
                outcome,
                verdict: Some(entry.verdict),
                knowledge: Some(entry.knowledge),
                duration_ms: result.map(|r| r.duration_ms).unwrap_or(0),
                estimated_ms: entry.estimated_ms,
                reasons: entry.reasons.clone(),
            });
        }
        tasks.sort_by(|a, b| a.name.cmp(&b.name));

        // Avoided work is counted from history only. A task Arc has never run
        // contributes nothing and is reported as unmeasured, so a first CI run
        // cannot claim to have saved anything.
        let mut avoided = analysis.skipped_estimate_ms;
        let mut unmeasured = analysis.skipped_without_history;
        for t in &tasks {
            if !matches!(t.outcome, TaskOutcome::LocalHit | TaskOutcome::RemoteHit) {
                continue;
            }
            match t.estimated_ms {
                Some(ms) => avoided = avoided.saturating_add(ms),
                None => unmeasured += 1,
            }
        }

        let work_ms = run.map(|r| r.duration_ms).unwrap_or(0);
        CiRunSummary {
            exit_code: i32::from(counts.failed > 0 || counts.blocked > 0),
            estimated_avoided_ms: avoided,
            avoided_without_history: unmeasured,
            work_ms,
            arc_ms: analysis.timings.total_ms,
            counts,
            tasks,
        }
    }
}

/// The GitHub job summary: compact by design. A workflow log already holds the
/// per-task detail, and a summary nobody scrolls is a summary nobody reads.
pub fn summary_markdown(analysis: &CiAnalysis, summary: &CiRunSummary, dry_run: bool) -> String {
    let c = &summary.counts;
    let mut out = String::from("## Arc\n\n");
    if dry_run {
        out.push_str("_analysis only; nothing was executed_\n\n");
    }
    let short = |s: &Option<String>| {
        s.as_deref()
            .map(|v| markdown_cell(&v[..v.len().min(12)]))
            .unwrap_or_else(|| "unknown".into())
    };
    out.push_str(&format!(
        "- `{}` .. `{}` ({} on {})\n",
        short(&analysis.base),
        short(&analysis.head),
        markdown_cell(&analysis.event),
        markdown_cell(&analysis.provider)
    ));
    out.push_str(&format!("- {} changed files\n", analysis.changed_files));
    out.push_str(&format!(
        "- {} tasks known, {} selected, {} skipped\n",
        c.known, c.selected, c.skipped
    ));
    if !dry_run {
        out.push_str(&format!(
            "- {} local hits, {} remote hits, {} executed\n",
            c.local_hits, c.remote_hits, c.executed
        ));
    }
    if summary.estimated_avoided_ms > 0 {
        out.push_str(&format!(
            "- ~{} of recorded work avoided{}\n",
            human_ms(summary.estimated_avoided_ms),
            if summary.avoided_without_history > 0 {
                format!(" ({} tasks unmeasured)", summary.avoided_without_history)
            } else {
                String::new()
            }
        ));
    } else if summary.avoided_without_history > 0 {
        out.push_str("- work avoided: unknown, no execution history yet\n");
    }
    if !analysis.diff_available {
        out.push_str("- **no complete diff; every declared task was selected**\n");
    }
    if !analysis.remote_write {
        out.push_str(&format!(
            "- remote cache is read-only: {}\n",
            markdown_cell(&analysis.remote_write_reason)
        ));
    }

    let failures: Vec<&TaskReport> = summary
        .tasks
        .iter()
        .filter(|t| t.outcome.failed())
        .collect();
    if !failures.is_empty() {
        out.push_str("\n### Failed\n\n");
        for t in failures {
            out.push_str(&format!(
                "- `{}` — {}\n",
                markdown_cell(&t.name),
                t.outcome.label().to_lowercase()
            ));
        }
    }
    out
}

pub fn human_ms(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}m {}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

/// Values worth handing to later workflow steps. Names and values are both
/// Arc's own, so nothing here can carry provider-controlled text.
pub fn outputs(summary: &CiRunSummary) -> Vec<(&'static str, String)> {
    let c = &summary.counts;
    vec![
        ("selected_count", c.selected.to_string()),
        ("skipped_count", c.skipped.to_string()),
        ("affected_count", c.affected.to_string()),
        ("cache_hits", (c.local_hits + c.remote_hits).to_string()),
        ("executed_count", c.executed.to_string()),
        ("failed_count", (c.failed + c.blocked).to_string()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_sequences_never_survive_into_output() {
        let hostile = "main\x1b[2Jrelease\u{7}\r\nx";
        let clean = sanitize(hostile);
        assert!(!clean.contains('\x1b'));
        assert!(!clean.contains('\u{7}'));
        assert!(!clean.contains('\n'));
        assert_eq!(sanitize(&"a".repeat(500)).chars().count(), 200);
    }

    #[test]
    fn a_markdown_cell_cannot_forge_a_column() {
        assert_eq!(markdown_cell("a|b`c"), "a\\|b'c");
    }

    fn analysis(selected: &[(&str, Verdict, Option<u64>)], skipped: &[&str]) -> CiAnalysis {
        CiAnalysis {
            provider: "local".into(),
            event: "other".into(),
            trust: "trusted".into(),
            repository: None,
            branch: None,
            pull_request: None,
            compare: "commit-range".into(),
            base: Some("a".repeat(40)),
            head: Some("b".repeat(40)),
            diff_available: true,
            notes: Vec::new(),
            shallow: false,
            changed_files: 1,
            changes: Vec::new(),
            known_tasks: selected.len() + skipped.len(),
            selected: selected
                .iter()
                .map(|(name, verdict, ms)| analysis::TaskPlanEntry {
                    name: (*name).into(),
                    family_key: format!("key-{name}"),
                    command: "sh -c x".into(),
                    verdict: *verdict,
                    knowledge: Knowledge::Local,
                    reasons: Vec::new(),
                    estimated_ms: *ms,
                    after: Vec::new(),
                })
                .collect(),
            skipped: skipped.iter().map(|s| (*s).to_string()).collect(),
            remote_write: true,
            remote_write_reason: "trusted event".into(),
            knowledge_local: selected.len(),
            knowledge_remote: 0,
            knowledge_none: 0,
            skipped_estimate_ms: 5_000,
            skipped_without_history: 1,
            timings: analysis::Timings::default(),
            plan: None,
        }
    }

    fn result(key: &str, outcome: plan::TaskOutcome, source: Option<&str>) -> plan::TaskResult {
        plan::TaskResult {
            family_key: format!("key-{key}"),
            label: key.into(),
            outcome,
            exit_code: 0,
            duration_ms: 10,
            stdout: String::new(),
            stderr: String::new(),
            blocked_by: None,
            cache_source: source.map(str::to_string),
        }
    }

    #[test]
    fn every_selected_task_is_counted_exactly_once() {
        let a = analysis(
            &[
                ("local", Verdict::Affected, Some(1_000)),
                ("remote", Verdict::Affected, Some(2_000)),
                ("ran", Verdict::Affected, None),
                ("murky", Verdict::Unknown, None),
                ("broken", Verdict::Affected, None),
                ("stuck", Verdict::Affected, None),
            ],
            &["idle"],
        );
        let run = RunSummary {
            results: vec![
                result("local", plan::TaskOutcome::Hit, Some("local")),
                result("remote", plan::TaskOutcome::Hit, Some("remote")),
                result("ran", plan::TaskOutcome::Ran, None),
                result("murky", plan::TaskOutcome::Ran, None),
                result("broken", plan::TaskOutcome::Failed, None),
                result("stuck", plan::TaskOutcome::Blocked, None),
            ],
            duration_ms: 42,
            ..Default::default()
        };
        let s = CiRunSummary::build(&a, Some(&run));
        let c = &s.counts;
        assert_eq!(c.known, 7);
        assert_eq!(c.selected, 6);
        assert_eq!(c.skipped, 1);
        assert_eq!((c.local_hits, c.remote_hits), (1, 1));
        assert_eq!(c.executed, 2, "an unknown task that ran is still executed");
        assert_eq!((c.failed, c.blocked), (1, 1));
        assert_eq!(
            c.local_hits + c.remote_hits + c.executed + c.failed + c.blocked,
            c.selected,
            "no task may fall into two buckets or none"
        );
        assert_eq!(
            s.tasks.len(),
            7,
            "every task appears once, skipped included"
        );
        assert_eq!(s.exit_code, 1);
        assert_eq!(
            s.counts.affected + s.counts.unknown,
            c.selected,
            "every selection has a verdict behind it"
        );
    }

    #[test]
    fn avoided_work_counts_only_what_arc_has_actually_measured() {
        let a = analysis(
            &[
                ("timed", Verdict::Affected, Some(3_000)),
                ("untimed", Verdict::Affected, None),
                ("ran", Verdict::Affected, Some(9_999)),
            ],
            &[],
        );
        let run = RunSummary {
            results: vec![
                result("timed", plan::TaskOutcome::Hit, Some("local")),
                result("untimed", plan::TaskOutcome::Hit, Some("remote")),
                result("ran", plan::TaskOutcome::Ran, None),
            ],
            ..Default::default()
        };
        let s = CiRunSummary::build(&a, Some(&run));
        // 5s from the skipped tasks plus 3s from the one reused task Arc has
        // timed. The task that actually ran contributes nothing, and the reused
        // one with no history is reported as unmeasured rather than as zero.
        assert_eq!(s.estimated_avoided_ms, 8_000);
        assert_eq!(s.avoided_without_history, 2);
        assert_eq!(s.exit_code, 0);
    }

    #[test]
    fn a_dry_run_reports_the_plan_without_inventing_outcomes() {
        let a = analysis(&[("selected", Verdict::Affected, Some(100))], &["idle"]);
        let s = CiRunSummary::build(&a, None);
        assert_eq!(s.counts.selected, 1);
        assert_eq!(s.counts.executed, 0);
        assert_eq!(s.tasks.len(), 1, "only the skipped task has a real outcome");
        assert_eq!(s.exit_code, 0);
        let md = summary_markdown(&a, &s, true);
        assert!(md.contains("analysis only"));
        assert!(
            !md.contains("local hits"),
            "a dry run has no cache outcomes to report: {md}"
        );
    }

    #[test]
    fn a_volatile_variable_is_flagged_only_when_the_project_opted_in() {
        let mut cfg = crate::project::Config::default();
        assert!(volatile_env_warnings(&cfg).is_empty());
        cfg.env.include = vec!["GITHUB_RUN_ID".into(), "RUSTFLAGS".into()];
        let w = volatile_env_warnings(&cfg);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("GITHUB_RUN_ID"));
    }
}
