//! The task graph: which executions feed which.
//!
//! A node is an execution *family*, so it survives input changes, output
//! changes, hits and misses. An edge `A → B` means B consumes something A
//! produces, and therefore that A must precede B.
//!
//! Edges are derived, never stored. A family's row records what it produces and
//! consumes; edges are computed from those rows on load. Rewriting a family's
//! row therefore cannot leave a stale edge behind, which is the failure mode a
//! persisted edge table invites.

use crate::dependency::{Completeness, DependencySet};
use crate::family::ExecutionFamily;
use crate::paths::PathKey;
use crate::project::Config;
use crate::scan::build_globs;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap};

/// Bumped when the meaning of a stored task-graph row changes. A row written
/// under a different version is discarded rather than reinterpreted.
pub const GRAPH_SCHEMA_VERSION: u32 = 1;

/// How a task consumes a path. The kind decides which producer outputs can
/// reach it, which is not the same question for a file and for a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Consumes {
    /// The file's contents were read.
    File,
    /// The directory's entries were enumerated, so anything appearing *under*
    /// it is a dependency, including a path that did not exist at trace time.
    Directory,
    /// The path's presence or absence was consulted.
    Existence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Consumed {
    pub path: String,
    pub kind: Consumes,
}

/// The durable, compact description of one task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskNode {
    pub schema: u32,
    pub family_key: String,
    /// Structured, so a task can be re-executed without parsing a display
    /// string back into arguments.
    pub program: String,
    pub args: Vec<String>,
    pub rel_cwd: String,
    pub project_root: String,
    /// Optional `[[command]] name`, else derived from the command line.
    pub label: String,
    pub named: bool,
    pub completeness: Completeness,
    pub inputs_narrowed: bool,
    pub downgrades: usize,
    /// Project-relative paths this task is known to produce.
    pub produces: Vec<String>,
    pub consumes: Vec<Consumed>,
    pub declared_inputs: Vec<String>,
    /// Task names this task must follow, from `[[command]] after`.
    pub after: Vec<String>,
    pub runs: u64,
    pub last_seen: i64,
    pub observations: u64,
}

impl TaskNode {
    pub fn command_line(&self) -> String {
        crate::record::format_command(&self.program, &self.args)
    }

    /// Whether Arc knows enough about this task to rule a change out.
    pub fn provable(&self) -> bool {
        self.inputs_narrowed
    }

    pub fn from_family(family: &ExecutionFamily, deps: &DependencySet, cfg: &Config) -> TaskNode {
        let command_line = family.command_line();
        let matching = cfg.commands_matching(&command_line);
        let named = matching.iter().find_map(|c| c.name.as_deref());
        let after: Vec<String> = matching
            .iter()
            .flat_map(|c| c.after.iter().cloned())
            .collect();

        let mut consumes: Vec<Consumed> = Vec::new();
        for p in &deps.inputs {
            consumes.push(Consumed {
                path: p.clone(),
                kind: Consumes::File,
            });
        }
        for p in &deps.directories {
            consumes.push(Consumed {
                path: p.clone(),
                kind: Consumes::Directory,
            });
        }
        // Existence entries are absolute; only those inside the project can be
        // produced by another task in it.
        for p in &deps.existence {
            if let Some(rel) = relative_to(&family.project_root, p) {
                consumes.push(Consumed {
                    path: rel,
                    kind: Consumes::Existence,
                });
            }
        }
        consumes.sort_by(|a, b| (&a.path, a.kind).cmp(&(&b.path, b.kind)));
        consumes.dedup();

        TaskNode {
            schema: GRAPH_SCHEMA_VERSION,
            family_key: family.key.clone(),
            program: family.program.clone(),
            args: family.args.clone(),
            rel_cwd: family.rel_cwd.clone(),
            project_root: family.project_root.clone(),
            label: named.map(str::to_string).unwrap_or(command_line),
            named: named.is_some(),
            completeness: deps.completeness,
            inputs_narrowed: deps.inputs_are_narrowed(),
            downgrades: deps.downgrades.len(),
            produces: deps.outputs.clone(),
            consumes,
            declared_inputs: deps.declared_inputs.clone(),
            after,
            runs: family.runs,
            last_seen: family.last_seen,
            observations: deps.observations,
        }
    }
}

fn relative_to(root: &str, abs: &str) -> Option<String> {
    let root = PathKey::from_display(root.trim_end_matches('/'));
    let child = PathKey::from_display(abs);
    let (r, c) = (root.as_str(), child.as_str());
    if c.len() > r.len() && c.starts_with(r) && c.as_bytes()[r.len()] == b'/' {
        Some(abs[r.len() + 1..].to_string())
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// A produced file is read by the consumer.
    Output,
    /// A produced path lands inside a directory the consumer enumerates.
    Directory,
    /// A produced path is one whose presence the consumer consulted.
    Existence,
    /// A produced path matches a glob the consumer declared as an input.
    Declared,
    /// Declared in `arc.toml` with `after`, not observed.
    Manual,
}

impl EdgeKind {
    pub fn label(&self) -> &'static str {
        match self {
            EdgeKind::Output => "output",
            EdgeKind::Directory => "directory",
            EdgeKind::Existence => "existence",
            EdgeKind::Declared => "declared",
            EdgeKind::Manual => "manual",
        }
    }
}

/// `from` must precede `to`; `to` depends on `from`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskEdge {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
    /// The path that justifies the edge, for provenance.
    pub via: String,
    /// The path has more than one known producer, so which task really feeds
    /// this consumer is not established.
    pub ambiguous: bool,
}

/// A path produced by more than one task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ambiguity {
    pub path: String,
    pub producers: Vec<String>,
}

/// A strongly connected component with more than one member, or a task that
/// depends on itself through another path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cycle {
    pub members: Vec<String>,
}

/// A reference to a task name in `after` that matches no known task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnresolvedEdge {
    pub task: String,
    pub after: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskGraph {
    pub schema: u32,
    pub project_root: String,
    pub nodes: Vec<TaskNode>,
    pub edges: Vec<TaskEdge>,
    pub ambiguities: Vec<Ambiguity>,
    pub cycles: Vec<Cycle>,
    pub unresolved: Vec<UnresolvedEdge>,
    #[serde(skip)]
    index: Index,
}

#[derive(Debug, Clone, Default)]
struct Index {
    by_key: HashMap<String, usize>,
    downstream: HashMap<String, Vec<usize>>,
    upstream: HashMap<String, Vec<usize>>,
}

pub fn build(project: &crate::project::Project, db: &crate::db::Db) -> Result<TaskGraph> {
    let project_id = crate::project_id(&project.root);
    let mut nodes: Vec<TaskNode> = db
        .task_nodes(&project_id)?
        .into_iter()
        .filter(|n| n.schema == GRAPH_SCHEMA_VERSION)
        .collect();
    nodes.sort_by(|a, b| (&a.label, &a.family_key).cmp(&(&b.label, &b.family_key)));
    Ok(assemble(project.root.to_string_lossy().to_string(), nodes))
}

/// Derive edges from a set of task rows. Separated from `build` so the graph
/// algorithms can be tested without a database.
pub fn assemble(project_root: String, nodes: Vec<TaskNode>) -> TaskGraph {
    let producers = producer_index(&nodes);
    let mut edges = Vec::new();

    for (i, consumer) in nodes.iter().enumerate() {
        let mut seen: BTreeSet<(usize, EdgeKind, &str)> = BTreeSet::new();
        for c in &consumer.consumes {
            let kind = match c.kind {
                Consumes::File => EdgeKind::Output,
                Consumes::Directory => EdgeKind::Directory,
                Consumes::Existence => EdgeKind::Existence,
            };
            match c.kind {
                Consumes::Directory => {
                    for (path, prods) in producers.under(&c.path) {
                        for p in prods {
                            if *p != i {
                                seen.insert((*p, kind, path));
                            }
                        }
                    }
                }
                _ => {
                    for p in producers.exact(&c.path) {
                        if *p != i {
                            seen.insert((*p, kind, &c.path));
                        }
                    }
                }
            }
        }
        // A declared glob is a claim about inputs the trace may never have seen,
        // so a produced path matching one is a dependency just as much as an
        // observed read.
        if let Ok(globs) = build_globs(&consumer.declared_inputs) {
            if !consumer.declared_inputs.is_empty() {
                for (path, prods) in producers.all() {
                    if !globs.is_match(path) {
                        continue;
                    }
                    for p in prods {
                        if *p != i {
                            seen.insert((*p, EdgeKind::Declared, path));
                        }
                    }
                }
            }
        }
        for (p, kind, via) in seen {
            edges.push(TaskEdge {
                from: nodes[p].family_key.clone(),
                to: consumer.family_key.clone(),
                kind,
                via: via.to_string(),
                ambiguous: producers.is_ambiguous(via),
            });
        }
    }

    let (manual, unresolved) = manual_edges(&nodes);
    edges.extend(manual);
    edges.sort_by(|a, b| (&a.from, &a.to, a.kind, &a.via).cmp(&(&b.from, &b.to, b.kind, &b.via)));
    edges.dedup();

    let ambiguities = producers.ambiguities(&nodes);
    let index = index_of(&nodes, &edges);
    let cycles = find_cycles(&nodes, &edges, &index);

    TaskGraph {
        schema: GRAPH_SCHEMA_VERSION,
        project_root,
        nodes,
        edges,
        ambiguities,
        cycles,
        unresolved,
        index,
    }
}

fn manual_edges(nodes: &[TaskNode]) -> (Vec<TaskEdge>, Vec<UnresolvedEdge>) {
    let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, n) in nodes.iter().enumerate() {
        if n.named {
            by_name.entry(n.label.as_str()).or_default().push(i);
        }
    }
    let mut edges = Vec::new();
    let mut unresolved = Vec::new();
    for (i, n) in nodes.iter().enumerate() {
        for name in &n.after {
            match by_name.get(name.as_str()) {
                Some(producers) => {
                    for p in producers {
                        if *p != i {
                            edges.push(TaskEdge {
                                from: nodes[*p].family_key.clone(),
                                to: n.family_key.clone(),
                                kind: EdgeKind::Manual,
                                via: name.clone(),
                                ambiguous: false,
                            });
                        }
                    }
                }
                None => unresolved.push(UnresolvedEdge {
                    task: n.label.clone(),
                    after: name.clone(),
                }),
            }
        }
    }
    (edges, unresolved)
}

/// `produced path -> node indices`, keyed by authoritative path identity rather
/// than by the display string.
struct Producers {
    exact: BTreeMap<PathKey, (String, Vec<usize>)>,
}

impl Producers {
    fn exact(&self, path: &str) -> &[usize] {
        self.exact
            .get(&PathKey::from_display(path))
            .map(|(_, v)| v.as_slice())
            .unwrap_or(&[])
    }

    /// Every produced path lying inside `dir`.
    ///
    /// The upper bound is `'0'`, the code point immediately after `'/'`, so the
    /// range covers exactly the children of `dir` and stops before a sibling
    /// whose name merely starts with the same letters.
    fn under(&self, dir: &str) -> Vec<(&str, &Vec<usize>)> {
        let prefix = PathKey::from_display(dir.trim_end_matches('/'));
        let lo = PathKey::from_display(&format!("{}/", prefix.as_str()));
        let hi = PathKey::from_display(&format!("{}0", prefix.as_str()));
        self.exact
            .range(lo..hi)
            .map(|(_, (display, v))| (display.as_str(), v))
            .collect()
    }

    fn all(&self) -> impl Iterator<Item = (&str, &Vec<usize>)> {
        self.exact.values().map(|(d, v)| (d.as_str(), v))
    }

    fn is_ambiguous(&self, path: &str) -> bool {
        self.exact
            .get(&PathKey::from_display(path))
            .is_some_and(|(_, v)| v.len() > 1)
    }

    fn ambiguities(&self, nodes: &[TaskNode]) -> Vec<Ambiguity> {
        let mut out: Vec<Ambiguity> = self
            .exact
            .values()
            .filter(|(_, v)| v.len() > 1)
            .map(|(display, v)| Ambiguity {
                path: display.clone(),
                producers: v.iter().map(|i| nodes[*i].family_key.clone()).collect(),
            })
            .collect();
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }
}

fn producer_index(nodes: &[TaskNode]) -> Producers {
    let mut exact: BTreeMap<PathKey, (String, Vec<usize>)> = BTreeMap::new();
    for (i, n) in nodes.iter().enumerate() {
        for p in &n.produces {
            let entry = exact
                .entry(PathKey::from_display(p))
                .or_insert_with(|| (p.clone(), Vec::new()));
            if !entry.1.contains(&i) {
                entry.1.push(i);
            }
        }
    }
    Producers { exact }
}

fn index_of(nodes: &[TaskNode], edges: &[TaskEdge]) -> Index {
    let by_key: HashMap<String, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.family_key.clone(), i))
        .collect();
    let mut downstream: HashMap<String, Vec<usize>> = HashMap::new();
    let mut upstream: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, e) in edges.iter().enumerate() {
        downstream.entry(e.from.clone()).or_default().push(i);
        upstream.entry(e.to.clone()).or_default().push(i);
    }
    Index {
        by_key,
        downstream,
        upstream,
    }
}

impl TaskGraph {
    pub fn node(&self, family_key: &str) -> Option<&TaskNode> {
        self.index.by_key.get(family_key).map(|i| &self.nodes[*i])
    }

    pub fn find(&self, needle: &str) -> Vec<&TaskNode> {
        self.nodes
            .iter()
            .filter(|n| {
                n.label == needle
                    || n.family_key.starts_with(needle)
                    || n.label.contains(needle)
                    || n.command_line().contains(needle)
            })
            .collect()
    }

    /// Edges leaving `family_key`: the tasks that depend on it.
    pub fn outgoing(&self, family_key: &str) -> impl Iterator<Item = &TaskEdge> {
        self.index
            .downstream
            .get(family_key)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
            .iter()
            .map(|i| &self.edges[*i])
    }

    /// Edges entering `family_key`: the tasks it depends on.
    pub fn incoming(&self, family_key: &str) -> impl Iterator<Item = &TaskEdge> {
        self.index
            .upstream
            .get(family_key)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
            .iter()
            .map(|i| &self.edges[*i])
    }

    pub fn upstream_of(&self, family_key: &str) -> Vec<&TaskNode> {
        let mut v: Vec<&TaskNode> = self
            .incoming(family_key)
            .filter_map(|e| self.node(&e.from))
            .collect();
        v.sort_by(|a, b| a.label.cmp(&b.label));
        v.dedup_by(|a, b| a.family_key == b.family_key);
        v
    }

    pub fn downstream_of(&self, family_key: &str) -> Vec<&TaskNode> {
        let mut v: Vec<&TaskNode> = self
            .outgoing(family_key)
            .filter_map(|e| self.node(&e.to))
            .collect();
        v.sort_by(|a, b| a.label.cmp(&b.label));
        v.dedup_by(|a, b| a.family_key == b.family_key);
        v
    }

    /// The cycle group containing `family_key`, if any.
    pub fn cycle_of(&self, family_key: &str) -> Option<&Cycle> {
        self.cycles
            .iter()
            .find(|c| c.members.iter().any(|m| m == family_key))
    }

    pub fn roots(&self) -> Vec<&TaskNode> {
        self.nodes
            .iter()
            .filter(|n| self.incoming(&n.family_key).next().is_none())
            .collect()
    }
}

/// Tarjan's algorithm, iteratively. A deep chain must not overflow the stack,
/// and real dependency chains get deep.
fn find_cycles(nodes: &[TaskNode], edges: &[TaskEdge], index: &Index) -> Vec<Cycle> {
    #[derive(Clone, Copy)]
    struct State {
        index: u32,
        lowlink: u32,
        on_stack: bool,
    }
    const UNVISITED: u32 = u32::MAX;

    let n = nodes.len();
    let mut state = vec![
        State {
            index: UNVISITED,
            lowlink: 0,
            on_stack: false
        };
        n
    ];
    let mut stack: Vec<usize> = Vec::new();
    let mut next_index: u32 = 0;
    let mut components: Vec<Cycle> = Vec::new();

    let succ = |v: usize| -> Vec<usize> {
        index
            .downstream
            .get(&nodes[v].family_key)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
            .iter()
            .filter_map(|i| index.by_key.get(&edges[*i].to).copied())
            .collect()
    };

    for start in 0..n {
        if state[start].index != UNVISITED {
            continue;
        }
        let mut call: Vec<(usize, Vec<usize>, usize)> = vec![(start, succ(start), 0)];
        state[start] = State {
            index: next_index,
            lowlink: next_index,
            on_stack: true,
        };
        next_index += 1;
        stack.push(start);

        while let Some((v, children, cursor)) = call.last_mut() {
            let v = *v;
            if *cursor < children.len() {
                let w = children[*cursor];
                *cursor += 1;
                if state[w].index == UNVISITED {
                    state[w] = State {
                        index: next_index,
                        lowlink: next_index,
                        on_stack: true,
                    };
                    next_index += 1;
                    stack.push(w);
                    call.push((w, succ(w), 0));
                } else if state[w].on_stack {
                    state[v].lowlink = state[v].lowlink.min(state[w].index);
                }
                continue;
            }

            call.pop();
            if state[v].lowlink == state[v].index {
                let mut members = Vec::new();
                while let Some(w) = stack.pop() {
                    state[w].on_stack = false;
                    members.push(nodes[w].family_key.clone());
                    if w == v {
                        break;
                    }
                }
                let self_loop = members.len() == 1 && succ(v).contains(&v);
                if members.len() > 1 || self_loop {
                    members.sort();
                    components.push(Cycle { members });
                }
            }
            if let Some((parent, _, _)) = call.last() {
                let p = *parent;
                state[p].lowlink = state[p].lowlink.min(state[v].lowlink);
            }
        }
    }
    components.sort_by(|a, b| a.members.cmp(&b.members));
    components
}

/// Deterministic topological order over a subset of the graph.
///
/// Members of a cycle are emitted together, in stable order, after everything
/// outside the cycle that they depend on: an SCC is one scheduling group, which
/// keeps ordering total without pretending the cycle is not there.
pub fn topological(graph: &TaskGraph, subset: &BTreeSet<String>) -> Vec<Vec<String>> {
    let group_of: HashMap<&str, usize> = graph
        .cycles
        .iter()
        .enumerate()
        .flat_map(|(i, c)| c.members.iter().map(move |m| (m.as_str(), i)))
        .collect();

    let mut groups: Vec<Vec<String>> = Vec::new();
    let mut group_index: HashMap<String, usize> = HashMap::new();
    let mut seen_cycle: HashMap<usize, usize> = HashMap::new();
    for key in subset {
        match group_of.get(key.as_str()) {
            Some(c) => {
                let gi = *seen_cycle.entry(*c).or_insert_with(|| {
                    groups.push(Vec::new());
                    groups.len() - 1
                });
                groups[gi].push(key.clone());
                group_index.insert(key.clone(), gi);
            }
            None => {
                groups.push(vec![key.clone()]);
                group_index.insert(key.clone(), groups.len() - 1);
            }
        }
    }
    for g in &mut groups {
        g.sort();
    }

    let mut deps: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); groups.len()];
    let mut dependents: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); groups.len()];
    for e in &graph.edges {
        let (Some(&a), Some(&b)) = (group_index.get(&e.from), group_index.get(&e.to)) else {
            continue;
        };
        if a != b {
            deps[b].insert(a);
            dependents[a].insert(b);
        }
    }

    // Kahn's algorithm with a ready set ordered by label, so the result is a
    // total order that does not depend on hash iteration. Scanning every
    // remaining group each round would be quadratic on a deep chain, which is
    // exactly the shape a build graph takes.
    let mut label: Vec<String> = groups
        .iter()
        .map(|g| {
            g.iter()
                .filter_map(|k| graph.node(k).map(|n| n.label.clone()))
                .min()
                .unwrap_or_default()
        })
        .collect();
    let mut indegree: Vec<usize> = deps.iter().map(|d| d.len()).collect();
    let mut ready: BinaryHeap<Reverse<(String, usize)>> = (0..groups.len())
        .filter(|g| indegree[*g] == 0)
        .map(|g| Reverse((std::mem::take(&mut label[g]), g)))
        .collect();

    let mut out: Vec<Vec<String>> = Vec::with_capacity(groups.len());
    let mut emitted = 0usize;
    while let Some(Reverse((_, g))) = ready.pop() {
        out.push(std::mem::take(&mut groups[g]));
        emitted += 1;
        for d in &dependents[g] {
            indegree[*d] -= 1;
            if indegree[*d] == 0 {
                ready.push(Reverse((std::mem::take(&mut label[*d]), *d)));
            }
        }
    }
    // Unreachable once cycles are condensed into single groups, but emitting the
    // remainder in stable order beats returning a partial plan if it ever were.
    if emitted < groups.len() {
        let mut rest: Vec<usize> = (0..groups.len())
            .filter(|g| !groups[*g].is_empty())
            .collect();
        rest.sort_by(|a, b| (&label[*a], a).cmp(&(&label[*b], b)));
        for g in rest {
            out.push(std::mem::take(&mut groups[g]));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(label: &str, produces: &[&str], consumes: &[&str]) -> TaskNode {
        detailed(label, produces, consumes, &[], Completeness::Complete, true)
    }

    fn detailed(
        label: &str,
        produces: &[&str],
        files: &[&str],
        dirs: &[&str],
        completeness: Completeness,
        narrowed: bool,
    ) -> TaskNode {
        let mut consumes: Vec<Consumed> = files
            .iter()
            .map(|p| Consumed {
                path: p.to_string(),
                kind: Consumes::File,
            })
            .collect();
        consumes.extend(dirs.iter().map(|p| Consumed {
            path: p.to_string(),
            kind: Consumes::Directory,
        }));
        TaskNode {
            schema: GRAPH_SCHEMA_VERSION,
            family_key: format!("key-{label}"),
            program: "sh".into(),
            args: vec!["-c".into(), label.into()],
            rel_cwd: String::new(),
            project_root: "/repo".into(),
            label: label.into(),
            named: true,
            completeness,
            inputs_narrowed: narrowed,
            downgrades: 0,
            produces: produces.iter().map(|s| s.to_string()).collect(),
            consumes,
            declared_inputs: Vec::new(),
            after: Vec::new(),
            runs: 1,
            last_seen: 0,
            observations: 1,
        }
    }

    fn graph(nodes: Vec<TaskNode>) -> TaskGraph {
        assemble("/repo".into(), nodes)
    }

    fn edge_pairs(g: &TaskGraph) -> Vec<(String, String)> {
        g.edges
            .iter()
            .map(|e| (e.from.clone(), e.to.clone()))
            .collect()
    }

    #[test]
    fn an_output_consumed_by_another_task_is_an_edge() {
        let g = graph(vec![
            node("a", &["gen/x.json"], &[]),
            node("b", &[], &["gen/x.json"]),
        ]);
        assert_eq!(edge_pairs(&g), vec![("key-a".into(), "key-b".into())]);
        assert_eq!(g.edges[0].kind, EdgeKind::Output);
        assert!(!g.edges[0].ambiguous);
    }

    #[test]
    fn a_task_reading_its_own_output_is_not_its_own_dependency() {
        let g = graph(vec![node("a", &["gen/x"], &["gen/x"])]);
        assert!(g.edges.is_empty());
        assert!(g.cycles.is_empty());
    }

    #[test]
    fn an_unproduced_input_creates_no_edge() {
        let g = graph(vec![
            node("a", &["gen/x"], &["src/lib.rs"]),
            node("b", &[], &["src/other.rs"]),
        ]);
        assert!(g.edges.is_empty());
    }

    #[test]
    fn a_new_file_in_an_enumerated_directory_is_an_edge() {
        let g = graph(vec![
            node("producer", &["plugins/new.so"], &[]),
            detailed(
                "consumer",
                &[],
                &[],
                &["plugins"],
                Completeness::Complete,
                true,
            ),
        ]);
        assert_eq!(
            edge_pairs(&g),
            vec![("key-producer".into(), "key-consumer".into())]
        );
        assert_eq!(g.edges[0].kind, EdgeKind::Directory);
    }

    #[test]
    fn a_directory_edge_does_not_reach_a_sibling_with_a_shared_prefix() {
        let g = graph(vec![
            node("producer", &["plugins-old/new.so"], &[]),
            detailed(
                "consumer",
                &[],
                &[],
                &["plugins"],
                Completeness::Complete,
                true,
            ),
        ]);
        assert!(g.edges.is_empty());
    }

    #[test]
    fn a_producer_of_a_path_whose_absence_mattered_is_an_edge() {
        let mut consumer = node("consumer", &[], &[]);
        consumer.consumes = vec![Consumed {
            path: "gen/config.json".into(),
            kind: Consumes::Existence,
        }];
        let g = graph(vec![node("producer", &["gen/config.json"], &[]), consumer]);
        assert_eq!(g.edges.len(), 1);
        assert_eq!(g.edges[0].kind, EdgeKind::Existence);
    }

    #[test]
    fn two_producers_of_one_path_are_reported_rather_than_resolved() {
        let g = graph(vec![
            node("a", &["dist/app.js"], &[]),
            node("b", &["dist/app.js"], &[]),
            node("c", &[], &["dist/app.js"]),
        ]);
        assert_eq!(g.ambiguities.len(), 1);
        assert_eq!(g.ambiguities[0].producers.len(), 2);
        // Both edges exist: covering both producers is the conservative answer.
        assert_eq!(g.edges.len(), 2);
        assert!(g.edges.iter().all(|e| e.ambiguous));
    }

    #[test]
    fn a_cycle_is_detected_and_traversal_terminates() {
        let g = graph(vec![node("a", &["x"], &["y"]), node("b", &["y"], &["x"])]);
        assert_eq!(g.cycles.len(), 1);
        assert_eq!(g.cycles[0].members, vec!["key-a", "key-b"]);

        let subset: BTreeSet<String> = g.nodes.iter().map(|n| n.family_key.clone()).collect();
        let order = topological(&g, &subset);
        assert_eq!(order.len(), 1, "a cycle is one scheduling group");
        assert_eq!(order[0].len(), 2);
    }

    #[test]
    fn topological_order_puts_producers_first_and_is_deterministic() {
        let g = graph(vec![
            node("a", &["x"], &[]),
            node("b", &["y"], &["x"]),
            node("c", &["z"], &["x"]),
            node("d", &[], &["y", "z"]),
        ]);
        let subset: BTreeSet<String> = g.nodes.iter().map(|n| n.family_key.clone()).collect();
        let flat: Vec<String> = topological(&g, &subset).into_iter().flatten().collect();
        let pos = |k: &str| flat.iter().position(|x| x == k).unwrap();
        assert!(pos("key-a") < pos("key-b"));
        assert!(pos("key-a") < pos("key-c"));
        assert!(pos("key-b") < pos("key-d"));
        assert!(pos("key-c") < pos("key-d"));
        assert_eq!(
            flat,
            topological(&g, &subset)
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_deep_chain_does_not_overflow_the_stack() {
        let nodes: Vec<TaskNode> = (0..20_000)
            .map(|i| {
                let mut n = node(&format!("t{i:05}"), &[], &[]);
                n.produces = vec![format!("out{i}")];
                if i > 0 {
                    n.consumes = vec![Consumed {
                        path: format!("out{}", i - 1),
                        kind: Consumes::File,
                    }];
                }
                n
            })
            .collect();
        let g = graph(nodes);
        assert_eq!(g.edges.len(), 19_999);
        assert!(g.cycles.is_empty());
    }

    #[test]
    fn manual_edges_come_from_names_and_unknown_names_are_reported() {
        let mut b = node("package", &[], &[]);
        b.after = vec!["build".into(), "nonexistent".into()];
        let g = graph(vec![node("build", &[], &[]), b]);
        assert_eq!(
            edge_pairs(&g),
            vec![("key-build".into(), "key-package".into())]
        );
        assert_eq!(g.edges[0].kind, EdgeKind::Manual);
        assert_eq!(g.unresolved.len(), 1);
        assert_eq!(g.unresolved[0].after, "nonexistent");
    }

    #[test]
    fn a_declared_input_glob_matching_a_produced_path_is_an_edge() {
        let mut consumer = node("test", &[], &[]);
        consumer.declared_inputs = vec!["gen/**".into()];
        let g = graph(vec![node("gen", &["gen/client.ts"], &[]), consumer]);
        assert_eq!(g.edges.len(), 1);
        assert_eq!(g.edges[0].kind, EdgeKind::Declared);
    }
}
