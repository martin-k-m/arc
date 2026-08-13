//! One CI decision, as data.
//!
//! `arc ci` is thin on top of this: the CLI renders and executes, it does not
//! decide. Everything the terminal, the JSON output and the GitHub job summary
//! show comes from [`CiAnalysis`].

use super::context::{CiContext, Environment};
use super::revisions::{self, Comparison};
use super::tasks::{self, CanonicalTask, Knowledge};
use crate::affected::{self, Cause, Report, Verdict};
use crate::db::{Db, TaskTiming};
use crate::git;
use crate::graph::{self, TaskNode};
use crate::plan::{self, ExecutionPlan};
use crate::project::{Project, RemoteWrite};
use crate::remote::Remote;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::time::Instant;

#[derive(Debug, Clone, Default)]
pub struct CiOptions {
    pub base: Option<String>,
    pub head: Option<String>,
    /// `--task` filters: task names or tags.
    pub tasks: Vec<String>,
    /// Allow Arc to deepen the repository to obtain the base commit.
    pub fetch: bool,
    pub no_remote: bool,
    /// Publish even where the trust policy would not. Named for its
    /// consequence, because that is the part a user must weigh.
    pub force_remote_write: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Timings {
    pub context_ms: u64,
    pub diff_ms: u64,
    pub knowledge_ms: u64,
    pub graph_ms: u64,
    pub analysis_ms: u64,
    pub plan_ms: u64,
    pub total_ms: u64,
}

/// One selected task, with why it was selected and what is known about it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskPlanEntry {
    pub name: String,
    pub family_key: String,
    pub command: String,
    pub verdict: Verdict,
    pub knowledge: Knowledge,
    pub reasons: Vec<String>,
    /// Median of recent executions, when Arc has any. `None` means unknown, and
    /// unknown is reported as unknown rather than as zero.
    pub estimated_ms: Option<u64>,
    pub after: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CiAnalysis {
    pub provider: String,
    pub event: String,
    pub trust: String,
    pub repository: Option<String>,
    pub branch: Option<String>,
    pub pull_request: Option<u64>,
    pub compare: String,
    pub base: Option<String>,
    pub head: Option<String>,
    pub diff_available: bool,
    /// Why the diff is incomplete, or what had to be worked around.
    pub notes: Vec<String>,
    pub shallow: bool,
    pub changed_files: usize,
    pub changes: Vec<git::Change>,
    pub known_tasks: usize,
    pub selected: Vec<TaskPlanEntry>,
    pub skipped: Vec<String>,
    pub remote_write: bool,
    pub remote_write_reason: String,
    pub knowledge_local: usize,
    pub knowledge_remote: usize,
    pub knowledge_none: usize,
    /// Work Arc expects to avoid by skipping proven-unaffected tasks, summed
    /// from recorded history.
    pub skipped_estimate_ms: u64,
    /// Skipped tasks Arc has never timed. Their contribution is unknown, and is
    /// reported as unknown rather than folded in as zero.
    pub skipped_without_history: usize,
    pub timings: Timings,
    #[serde(skip)]
    pub plan: Option<ExecutionPlan>,
}

impl CiAnalysis {
    pub fn affected(&self) -> usize {
        self.selected
            .iter()
            .filter(|t| t.verdict == Verdict::Affected)
            .count()
    }

    pub fn unknown(&self) -> usize {
        self.selected
            .iter()
            .filter(|t| t.verdict == Verdict::Unknown)
            .count()
    }
}

/// Analyse this CI job: what changed, what it reaches, and what must run.
pub fn analyse(
    project: &Project,
    db: &Db,
    cwd: &Path,
    env: &Environment,
    opts: &CiOptions,
) -> Result<CiAnalysis> {
    let started = Instant::now();
    let mut timings = Timings::default();

    let t = Instant::now();
    let ctx = CiContext::detect(env);
    let canonical = tasks::canonical(project, &opts.tasks)?;
    timings.context_ms = t.elapsed().as_millis() as u64;

    let t = Instant::now();
    let resolution = revisions::resolve(
        cwd,
        &ctx,
        opts.base.as_deref(),
        opts.head.as_deref(),
        opts.fetch,
    );
    let (changes, diff_note) = diff(cwd, &resolution.comparison);
    timings.diff_ms = t.elapsed().as_millis() as u64;

    let (write, write_reason) = write_policy(project, &ctx, opts);

    let t = Instant::now();
    let local: Vec<TaskNode> = db.task_nodes(&crate::project_id(&project.root))?;
    let known: BTreeSet<String> = local.iter().map(|n| n.family_key.clone()).collect();
    let unknown_families: Vec<String> = canonical
        .iter()
        .filter(|c| !known.contains(&c.family_key))
        .map(|c| c.family_key.clone())
        .collect();
    let remote = (!opts.no_remote)
        .then(|| Remote::open(&project.config.remote).ok())
        .flatten()
        .filter(|r| r.read);
    let mut notes: Vec<String> = Vec::new();
    let imported = match (&remote, unknown_families.is_empty()) {
        (Some(r), false) => match r.lookup_tasks(&unknown_families) {
            Ok(v) => v,
            Err(e) => {
                notes.push(format!("remote task knowledge unavailable: {e}"));
                Vec::new()
            }
        },
        _ => Vec::new(),
    };
    timings.knowledge_ms = t.elapsed().as_millis() as u64;

    let t = Instant::now();
    let project_root = crate::paths::display_form(&project.root);
    let by_family: HashMap<&str, &crate::remote::protocol::RemoteTask> = imported
        .iter()
        .map(|t| (t.family_key.as_str(), t))
        .collect();
    let mut knowledge: BTreeMap<String, Knowledge> = BTreeMap::new();
    let mut nodes = local;
    for c in &canonical {
        if known.contains(&c.family_key) {
            knowledge.insert(c.family_key.clone(), Knowledge::Local);
            continue;
        }
        match by_family
            .get(c.family_key.as_str())
            .and_then(|t| tasks::adopt(c, t, &project_root))
        {
            Some(node) => {
                knowledge.insert(
                    c.family_key.clone(),
                    if node.provable() {
                        Knowledge::Remote
                    } else {
                        Knowledge::None
                    },
                );
                nodes.push(node);
            }
            None => {
                knowledge.insert(c.family_key.clone(), Knowledge::None);
                nodes.push(tasks::placeholder(c, &project_root));
            }
        }
    }
    let graph = graph::assemble(project.root.to_string_lossy().to_string(), nodes);
    timings.graph_ms = t.elapsed().as_millis() as u64;

    let t = Instant::now();
    let mut report = affected::analyse(&graph, &git::touched_paths(&changes))?;
    if !resolution.comparison.is_known() {
        let reason = match &resolution.comparison {
            Comparison::Unknown { reason } => reason.clone(),
            _ => unreachable!(),
        };
        force_unknown(&mut report, &canonical, &reason);
        notes.push(reason);
    }
    if let Some(n) = diff_note {
        notes.push(n);
    }
    if let Some(n) = resolution.note.clone() {
        notes.push(n);
    }
    if let Some(n) = ctx.note.clone() {
        notes.push(n);
    }
    notes.extend(super::volatile_env_warnings(&project.config));
    timings.analysis_ms = t.elapsed().as_millis() as u64;

    let t = Instant::now();
    // CI runs the declared set and only the declared set. A task Arc happens to
    // have learned about, but which the repository does not name, is not CI's
    // business even when a change reaches it.
    let selected_keys: BTreeSet<String> = canonical
        .iter()
        .filter(|c| {
            report
                .task(&c.family_key)
                .map(|t| t.verdict != Verdict::Unaffected)
                .unwrap_or(true)
        })
        .map(|c| c.family_key.clone())
        .collect();
    let plan = plan::plan_for(&graph, &report, &selected_keys);
    timings.plan_ms = t.elapsed().as_millis() as u64;

    let history = db.timings()?;
    let entries = entries(&canonical, &report, &plan, &knowledge, &history);
    let skipped: Vec<String> = canonical
        .iter()
        .filter(|c| !selected_keys.contains(&c.family_key))
        .map(|c| c.name.clone())
        .collect();
    let (skipped_estimate_ms, skipped_without_history) = estimate(
        canonical
            .iter()
            .filter(|c| !selected_keys.contains(&c.family_key)),
        &history,
    );

    timings.total_ms = started.elapsed().as_millis() as u64;
    Ok(CiAnalysis {
        provider: ctx.provider.label().into(),
        event: ctx.event.label().into(),
        trust: ctx.trust.label().into(),
        repository: ctx.repository.clone(),
        branch: ctx.branch.clone(),
        pull_request: ctx.pull_request,
        compare: resolution.comparison.label().into(),
        base: resolution.comparison.base().map(str::to_string),
        head: resolution.comparison.head().map(str::to_string),
        diff_available: resolution.comparison.is_known(),
        shallow: resolution.shallow,
        changed_files: changes.len(),
        changes,
        known_tasks: canonical.len(),
        knowledge_local: count(&knowledge, Knowledge::Local),
        knowledge_remote: count(&knowledge, Knowledge::Remote),
        knowledge_none: count(&knowledge, Knowledge::None),
        selected: entries,
        skipped,
        remote_write: write,
        remote_write_reason: write_reason,
        notes,
        timings,
        skipped_estimate_ms,
        skipped_without_history,
        plan: Some(plan),
    })
}

fn count(k: &BTreeMap<String, Knowledge>, want: Knowledge) -> usize {
    k.values().filter(|v| **v == want).count()
}

fn diff(cwd: &Path, comparison: &Comparison) -> (Vec<git::Change>, Option<String>) {
    let result = match comparison {
        Comparison::CommitRange { base, head } => git::changed_between(cwd, base, head),
        Comparison::WorkingTree { base } => git::changed_since(cwd, base),
        Comparison::Unknown { .. } => return (Vec::new(), None),
    };
    match result {
        Ok(c) => (c, None),
        // A diff Arc cannot compute is not an empty diff.
        Err(e) => (Vec::new(), Some(format!("git diff failed: {e}"))),
    }
}

/// Without a diff, nothing can be proven unaffected. Verdicts are raised to
/// unknown, never lowered: a task the graph already called affected stays so.
fn force_unknown(report: &mut Report, canonical: &[CanonicalTask], reason: &str) {
    let wanted: BTreeSet<&str> = canonical.iter().map(|c| c.family_key.as_str()).collect();
    for t in &mut report.tasks {
        if wanted.contains(t.family_key.as_str()) && t.verdict == Verdict::Unaffected {
            t.verdict = Verdict::Unknown;
            t.causes.push(Cause::NotProvable {
                reason: reason.to_string(),
            });
        }
    }
}

fn write_policy(project: &Project, ctx: &CiContext, opts: &CiOptions) -> (bool, String) {
    use super::context::Trust;
    if opts.no_remote {
        return (false, "remote disabled for this run".into());
    }
    if opts.force_remote_write {
        return (true, "--remote-write was given".into());
    }
    match project.config.ci.remote_write {
        RemoteWrite::Never => (false, "[ci] remote_write = \"never\"".into()),
        RemoteWrite::Always => (true, "[ci] remote_write = \"always\"".into()),
        RemoteWrite::Trusted => match ctx.trust {
            Trust::Trusted => (true, "trusted event".into()),
            Trust::Untrusted => (false, "untrusted event (fork pull request)".into()),
            Trust::Unknown => (
                false,
                format!(
                    "trust of the {} event cannot be established",
                    ctx.event.label()
                ),
            ),
        },
    }
}

fn entries(
    canonical: &[CanonicalTask],
    report: &Report,
    plan: &ExecutionPlan,
    knowledge: &BTreeMap<String, Knowledge>,
    history: &HashMap<String, TaskTiming>,
) -> Vec<TaskPlanEntry> {
    let mut out = Vec::new();
    for c in canonical {
        let Some(planned) = plan.task(&c.family_key) else {
            continue;
        };
        let verdict = report
            .task(&c.family_key)
            .map(|t| t.verdict)
            .unwrap_or(Verdict::Unknown);
        out.push(TaskPlanEntry {
            name: c.name.clone(),
            family_key: c.family_key.clone(),
            command: c.command_line(),
            verdict,
            knowledge: knowledge
                .get(&c.family_key)
                .copied()
                .unwrap_or(Knowledge::None),
            reasons: report
                .task(&c.family_key)
                .map(|t| t.causes.iter().map(describe).take(3).collect())
                .unwrap_or_default(),
            estimated_ms: history.get(&c.family_key).and_then(|t| t.median_ms()),
            after: planned.after.clone(),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn estimate<'a>(
    tasks: impl Iterator<Item = &'a CanonicalTask>,
    history: &HashMap<String, TaskTiming>,
) -> (u64, usize) {
    let mut total = 0u64;
    let mut unknown = 0usize;
    for t in tasks {
        match history.get(&t.family_key).and_then(|h| h.median_ms()) {
            Some(ms) => total = total.saturating_add(ms),
            None => unknown += 1,
        }
    }
    (total, unknown)
}

pub fn describe(cause: &Cause) -> String {
    match cause {
        Cause::Changed { path } => format!("{path} changed"),
        Cause::Upstream { task, via, .. } => {
            format!("{} is affected, via {via}", crate::engine::short(task))
        }
        Cause::UpstreamUnknown { task } => {
            format!("{} is unknown", crate::engine::short(task))
        }
        Cause::NotProvable { reason } => reason.clone(),
        Cause::Cycle => "part of a dependency cycle".into(),
    }
}
