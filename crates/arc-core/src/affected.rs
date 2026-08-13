//! Which tasks a set of working-tree changes may affect, directly or through
//! the task graph.
//!
//! Three answers, and the distinction between the last two is the whole point:
//! *affected* means Arc found a path from a change to the task; *unaffected*
//! means Arc can prove there is none; *unknown* means it cannot prove either.
//! A task whose dependencies were never fully observed is unknown, and unknown
//! is never treated as unaffected.
//!
//! Uncertainty propagates downstream. If Arc cannot rule out that a task was
//! affected, it cannot rule out that its consumers were either.

use crate::db::Db;
use crate::git;
use crate::graph::{self, EdgeKind, TaskGraph, TaskNode};
use crate::paths::PathKey;
use crate::project::Project;
use crate::scan::build_globs;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// A change reaches this task, directly or through the graph.
    Unaffected,
    /// Arc cannot prove the task is unaffected.
    Unknown,
    /// A change reaches this task.
    Affected,
}

impl Verdict {
    pub fn label(&self) -> &'static str {
        match self {
            Verdict::Affected => "affected",
            Verdict::Unaffected => "unaffected",
            Verdict::Unknown => "unknown",
        }
    }
}

/// Why a task ended up in its bucket. Enough to answer "why is this running?"
/// without re-deriving the graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Cause {
    /// A changed path is one of this task's own dependencies.
    Changed { path: String },
    /// An upstream task is affected, and this task consumes something it
    /// produces.
    Upstream {
        task: String,
        via: String,
        edge: EdgeKind,
    },
    /// The task's dependency knowledge is not complete enough to rule a change
    /// out.
    NotProvable { reason: String },
    /// An upstream task's own status is unknown.
    UpstreamUnknown { task: String },
    /// The task belongs to a dependency cycle.
    Cycle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskVerdict {
    pub family_key: String,
    pub label: String,
    pub command: String,
    pub rel_cwd: String,
    pub verdict: Verdict,
    pub causes: Vec<Cause>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub schema: u32,
    pub project_root: String,
    pub changes: Vec<git::Change>,
    pub tasks: Vec<TaskVerdict>,
    pub edges: Vec<graph::TaskEdge>,
    pub ambiguities: Vec<graph::Ambiguity>,
    pub cycles: Vec<graph::Cycle>,
    /// Set when Git could not answer, so callers report that rather than
    /// silently showing an empty change list.
    pub git_error: Option<String>,
}

impl Report {
    pub fn of(&self, v: Verdict) -> impl Iterator<Item = &TaskVerdict> {
        self.tasks.iter().filter(move |t| t.verdict == v)
    }

    pub fn task(&self, family_key: &str) -> Option<&TaskVerdict> {
        self.tasks.iter().find(|t| t.family_key == family_key)
    }

    /// Everything that must run: affected plus unknown, because unknown is not
    /// a licence to skip.
    pub fn selected(&self) -> Vec<&TaskVerdict> {
        self.tasks
            .iter()
            .filter(|t| t.verdict != Verdict::Unaffected)
            .collect()
    }
}

pub fn compute(project: &Project, db: &Db, cwd: &std::path::Path) -> Result<Report> {
    let (changes, git_error) = match git::changes(cwd) {
        Ok(c) => (c, None),
        Err(e) => (Vec::new(), Some(e.to_string())),
    };
    let g = graph::build(project, db)?;
    let mut report = analyse(&g, &git::touched_paths(&changes))?;
    report.changes = changes;
    report.git_error = git_error;
    Ok(report)
}

/// The reusable core: given a graph and a set of changed project-relative
/// paths, decide what must run. No Git, no database, no terminal.
pub fn analyse(g: &TaskGraph, changed: &[String]) -> Result<Report> {
    let changed_keys: BTreeSet<PathKey> =
        changed.iter().map(|p| PathKey::from_display(p)).collect();

    let mut verdicts: BTreeMap<&str, (Verdict, Vec<Cause>)> = BTreeMap::new();
    let mut queue: VecDeque<&str> = VecDeque::new();

    for node in &g.nodes {
        let (v, causes) = direct(node, changed, &changed_keys)?;
        if v != Verdict::Unaffected {
            queue.push_back(node.family_key.as_str());
        }
        verdicts.insert(node.family_key.as_str(), (v, causes));
    }

    // Breadth-first over edges. A node is only re-queued when its verdict
    // strengthens, so each edge is relaxed a bounded number of times and a cycle
    // cannot spin: Affected > Unknown > Unaffected is a lattice with three
    // levels.
    while let Some(key) = queue.pop_front() {
        let current = verdicts
            .get(key)
            .map(|(v, _)| *v)
            .unwrap_or(Verdict::Unaffected);
        if current == Verdict::Unaffected {
            continue;
        }
        for edge in g.outgoing(key) {
            let Some((v, causes)) = verdicts.get_mut(edge.to.as_str()) else {
                continue;
            };
            let (proposed, cause) = match current {
                Verdict::Affected => (
                    Verdict::Affected,
                    Cause::Upstream {
                        task: key.to_string(),
                        via: edge.via.clone(),
                        edge: edge.kind,
                    },
                ),
                _ => (
                    Verdict::Unknown,
                    Cause::UpstreamUnknown {
                        task: key.to_string(),
                    },
                ),
            };
            if proposed > *v {
                *v = proposed;
                causes.push(cause);
                queue.push_back(edge.to.as_str());
            }
        }
    }

    let mut tasks: Vec<TaskVerdict> = g
        .nodes
        .iter()
        .map(|n| {
            let (verdict, mut causes) = verdicts
                .remove(n.family_key.as_str())
                .unwrap_or((Verdict::Unknown, Vec::new()));
            if g.cycle_of(&n.family_key).is_some() && !causes.contains(&Cause::Cycle) {
                causes.push(Cause::Cycle);
            }
            causes.truncate(8);
            TaskVerdict {
                family_key: n.family_key.clone(),
                label: n.label.clone(),
                command: n.command_line(),
                rel_cwd: n.rel_cwd.clone(),
                verdict,
                causes,
            }
        })
        .collect();
    tasks.sort_by(|a, b| (&a.label, &a.family_key).cmp(&(&b.label, &b.family_key)));

    Ok(Report {
        schema: graph::GRAPH_SCHEMA_VERSION,
        project_root: g.project_root.clone(),
        changes: Vec::new(),
        tasks,
        edges: g.edges.clone(),
        ambiguities: g.ambiguities.clone(),
        cycles: g.cycles.clone(),
        git_error: None,
    })
}

/// Whether a change touches this task's own dependencies, without considering
/// the graph.
fn direct(
    node: &TaskNode,
    changed: &[String],
    changed_keys: &BTreeSet<PathKey>,
) -> Result<(Verdict, Vec<Cause>)> {
    let mut causes = Vec::new();

    for c in &node.consumes {
        let key = PathKey::from_display(&c.path);
        let hit = match c.kind {
            // Anything appearing, changing or vanishing inside an enumerated
            // directory changes its entry set.
            graph::Consumes::Directory => changed_keys.iter().any(|k| under(k, &key)),
            _ => changed_keys.contains(&key),
        };
        if hit {
            causes.push(Cause::Changed {
                path: c.path.clone(),
            });
        }
    }

    if !node.declared_inputs.is_empty() {
        let globs = build_globs(&node.declared_inputs)?;
        for p in changed {
            if globs.is_match(p) {
                causes.push(Cause::Changed { path: p.clone() });
            }
        }
    }

    causes.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    causes.dedup();
    causes.truncate(8);

    if !causes.is_empty() {
        return Ok((Verdict::Affected, causes));
    }
    if node.provable() {
        return Ok((Verdict::Unaffected, Vec::new()));
    }
    Ok((
        Verdict::Unknown,
        vec![Cause::NotProvable {
            reason: format!("dependency model is {}", node.completeness.label()),
        }],
    ))
}

fn under(child: &PathKey, root: &PathKey) -> bool {
    let (c, r) = (child.as_str(), root.as_str());
    c.len() > r.len() && c.starts_with(r) && c.as_bytes()[r.len()] == b'/'
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dependency::Completeness;
    use crate::graph::{Consumed, Consumes, TaskNode, GRAPH_SCHEMA_VERSION};

    fn node(label: &str, produces: &[&str], consumes: &[&str], provable: bool) -> TaskNode {
        TaskNode {
            schema: GRAPH_SCHEMA_VERSION,
            family_key: format!("key-{label}"),
            program: "sh".into(),
            args: vec!["-c".into(), label.into()],
            rel_cwd: String::new(),
            project_root: "/repo".into(),
            label: label.into(),
            named: true,
            completeness: if provable {
                Completeness::Complete
            } else {
                Completeness::Partial
            },
            inputs_narrowed: provable,
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

    fn verdicts(nodes: Vec<TaskNode>, changed: &[&str]) -> BTreeMap<String, Verdict> {
        let g = graph::assemble("/repo".into(), nodes);
        let changed: Vec<String> = changed.iter().map(|s| s.to_string()).collect();
        analyse(&g, &changed)
            .unwrap()
            .tasks
            .into_iter()
            .map(|t| (t.label, t.verdict))
            .collect()
    }

    #[test]
    fn a_change_propagates_along_a_chain() {
        let v = verdicts(
            vec![
                node("a", &["gen/x"], &["schema.yaml"], true),
                node("b", &["gen/y"], &["gen/x"], true),
                node("c", &[], &["gen/y"], true),
                node("unrelated", &[], &["other.txt"], true),
            ],
            &["schema.yaml"],
        );
        assert_eq!(v["a"], Verdict::Affected);
        assert_eq!(v["b"], Verdict::Affected);
        assert_eq!(v["c"], Verdict::Affected);
        assert_eq!(v["unrelated"], Verdict::Unaffected);
    }

    #[test]
    fn a_diamond_marks_both_branches_and_the_join() {
        let v = verdicts(
            vec![
                node("a", &["x"], &["src.txt"], true),
                node("b", &["y"], &["x"], true),
                node("c", &["z"], &["x"], true),
                node("d", &[], &["y", "z"], true),
            ],
            &["src.txt"],
        );
        for t in ["a", "b", "c", "d"] {
            assert_eq!(v[t], Verdict::Affected, "{t}");
        }
    }

    #[test]
    fn a_task_without_provable_inputs_is_unknown_not_unaffected() {
        let v = verdicts(vec![node("legacy", &[], &["something"], false)], &["x.txt"]);
        assert_eq!(v["legacy"], Verdict::Unknown);
    }

    #[test]
    fn uncertainty_propagates_downstream_as_uncertainty() {
        let v = verdicts(
            vec![
                node("murky", &["gen/x"], &[], false),
                node("downstream", &[], &["gen/x"], true),
            ],
            &["unrelated.txt"],
        );
        assert_eq!(v["murky"], Verdict::Unknown);
        assert_eq!(
            v["downstream"],
            Verdict::Unknown,
            "a consumer of an unknown producer cannot be proven unaffected"
        );
    }

    #[test]
    fn a_change_inside_an_enumerated_directory_affects_the_enumerator() {
        let mut consumer = node("lister", &[], &[], true);
        consumer.consumes = vec![Consumed {
            path: "plugins".into(),
            kind: Consumes::Directory,
        }];
        let v = verdicts(vec![consumer], &["plugins/new.so"]);
        assert_eq!(v["lister"], Verdict::Affected);
    }

    #[test]
    fn a_cycle_terminates_and_marks_every_member() {
        let v = verdicts(
            vec![
                node("a", &["x"], &["y", "src.txt"], true),
                node("b", &["y"], &["x"], true),
            ],
            &["src.txt"],
        );
        assert_eq!(v["a"], Verdict::Affected);
        assert_eq!(v["b"], Verdict::Affected);
    }

    #[test]
    fn provenance_records_why_a_task_is_affected() {
        let g = graph::assemble(
            "/repo".into(),
            vec![
                node("gen", &["gen/x"], &["schema.yaml"], true),
                node("test", &[], &["gen/x"], true),
            ],
        );
        let report = analyse(&g, &["schema.yaml".to_string()]).unwrap();
        let test = report.tasks.iter().find(|t| t.label == "test").unwrap();
        assert!(matches!(
            test.causes.first(),
            Some(Cause::Upstream { via, .. }) if via == "gen/x"
        ));
    }

    #[test]
    fn nothing_changed_leaves_provable_tasks_unaffected() {
        let v = verdicts(vec![node("a", &[], &["x"], true)], &[]);
        assert_eq!(v["a"], Verdict::Unaffected);
    }
}
