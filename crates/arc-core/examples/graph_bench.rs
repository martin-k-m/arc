//! Timings for graph construction, affected propagation and planning.
//!
//!   cargo run --release -p arc-core --example graph_bench

use arc_core::affected::analyse;
use arc_core::dependency::Completeness;
use arc_core::graph::{self, Consumed, Consumes, TaskNode, GRAPH_SCHEMA_VERSION};
use std::collections::BTreeSet;
use std::time::Instant;

fn node(i: usize, produces: Vec<String>, consumes: Vec<String>) -> TaskNode {
    TaskNode {
        schema: GRAPH_SCHEMA_VERSION,
        family_key: format!("family-{i:07}"),
        program: "sh".into(),
        args: vec!["-c".into(), format!("task {i}")],
        rel_cwd: String::new(),
        project_root: "/repo".into(),
        label: format!("task-{i:07}"),
        named: false,
        completeness: Completeness::Complete,
        inputs_narrowed: true,
        downgrades: 0,
        produces,
        consumes: consumes
            .into_iter()
            .map(|path| Consumed {
                path,
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

/// A layered graph: each task consumes a few outputs from the previous layer,
/// which is what a real monorepo looks like.
fn layered(n: usize, fanin: usize) -> Vec<TaskNode> {
    let width = (n as f64).sqrt().ceil() as usize;
    (0..n)
        .map(|i| {
            let consumes = if i < width {
                vec![format!("src/root-{i}.txt")]
            } else {
                (0..fanin)
                    .map(|k| format!("out/{:07}", i - width - k * 3 % width.max(1)))
                    .collect()
            };
            node(i, vec![format!("out/{i:07}")], consumes)
        })
        .collect()
}

fn chain(n: usize) -> Vec<TaskNode> {
    (0..n)
        .map(|i| {
            let consumes = if i == 0 {
                vec!["src/root.txt".into()]
            } else {
                vec![format!("out/{:07}", i - 1)]
            };
            node(i, vec![format!("out/{i:07}")], consumes)
        })
        .collect()
}

fn wide(n: usize) -> Vec<TaskNode> {
    let mut v = vec![node(
        0,
        vec!["out/root".into()],
        vec!["src/root.txt".into()],
    )];
    v.extend((1..n).map(|i| node(i, vec![format!("out/{i:07}")], vec!["out/root".into()])));
    v
}

fn ms(t: Instant) -> String {
    format!("{:>7.1}ms", t.elapsed().as_secs_f64() * 1000.0)
}

fn measure(name: &str, nodes: Vec<TaskNode>, changed: &[&str]) {
    let n = nodes.len();
    let t = Instant::now();
    let g = graph::assemble("/repo".into(), nodes);
    let build = ms(t);
    let edges = g.edges.len();

    let changed: Vec<String> = changed.iter().map(|s| s.to_string()).collect();
    let t = Instant::now();
    let report = analyse(&g, &changed).unwrap();
    let affected = ms(t);

    let t = Instant::now();
    let plan = arc_core::plan::build(&g, &report);
    let planning = ms(t);

    let selected: BTreeSet<String> = g.nodes.iter().map(|n| n.family_key.clone()).collect();
    let t = Instant::now();
    let order = graph::topological(&g, &selected);
    let topo = ms(t);

    println!(
        "{name:<28} {n:>7} nodes {edges:>8} edges | build {build} | affected {affected} \
         ({:>6} hit) | plan {planning} ({:>6} tasks) | topo {topo} ({:>5} groups)",
        report
            .tasks
            .iter()
            .filter(|t| t.verdict != arc_core::affected::Verdict::Unaffected)
            .count(),
        plan.tasks.len(),
        order.len()
    );
}

fn main() {
    println!("arc graph bench\n");
    for n in [100usize, 1_000, 10_000] {
        measure(&format!("layered/{n}"), layered(n, 3), &["src/root-0.txt"]);
    }
    for n in [1_000usize, 10_000] {
        measure(&format!("chain/{n} (deep)"), chain(n), &["src/root.txt"]);
    }
    for n in [1_000usize, 10_000] {
        measure(&format!("wide/{n}"), wide(n), &["src/root.txt"]);
    }
    measure("layered/10000 no change", layered(10_000, 3), &[]);
}
