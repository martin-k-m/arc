//! How long CI analysis takes as the graph and the diff grow.
//!
//! Measures the part Arc controls — diff interpretation, affected propagation,
//! plan construction — with no Git, no database and no execution, so the numbers
//! say what orchestration costs rather than what a runner's disk was doing.

use arc_core::affected;
use arc_core::dependency::Completeness;
use arc_core::graph::{self, Consumed, Consumes, TaskGraph, TaskNode, GRAPH_SCHEMA_VERSION};
use arc_core::plan;
use std::collections::BTreeSet;
use std::time::Instant;

/// A layered graph: each task reads a few source files and the outputs of a few
/// tasks in the layer below, which is the shape a real build takes.
fn build_graph(tasks: usize) -> TaskGraph {
    let per_layer = (tasks as f64).sqrt() as usize + 1;
    let nodes: Vec<TaskNode> = (0..tasks)
        .map(|i| {
            let layer = i / per_layer;
            let mut consumes: Vec<Consumed> = (0..3)
                .map(|k| Consumed {
                    path: format!("src/mod{}/file{}.rs", i % 64, (i * 7 + k) % 512),
                    kind: Consumes::File,
                })
                .collect();
            if layer > 0 {
                for k in 0..2 {
                    let upstream = (layer - 1) * per_layer + (i + k) % per_layer;
                    consumes.push(Consumed {
                        path: format!("generated/out{upstream}.bin"),
                        kind: Consumes::File,
                    });
                }
            }
            TaskNode {
                schema: GRAPH_SCHEMA_VERSION,
                family_key: format!("{i:064x}"),
                program: "sh".into(),
                args: vec!["-c".into(), format!("task {i}")],
                rel_cwd: String::new(),
                project_root: "/repo".into(),
                label: format!("task-{i:06}"),
                named: true,
                completeness: Completeness::Complete,
                inputs_narrowed: true,
                downgrades: 0,
                produces: vec![format!("generated/out{i}.bin")],
                consumes,
                declared_inputs: Vec::new(),
                after: Vec::new(),
                runs: 1,
                last_seen: 0,
                observations: 1,
            }
        })
        .collect();
    graph::assemble("/repo".into(), nodes)
}

fn changed_paths(n: usize) -> Vec<String> {
    (0..n)
        .map(|i| format!("src/mod{}/file{}.rs", i % 64, (i * 13) % 512))
        .collect()
}

fn median(mut v: Vec<u128>) -> u128 {
    v.sort_unstable();
    v[v.len() / 2]
}

fn main() {
    println!(
        "{:>8}  {:>8}  {:>10}  {:>10}  {:>10}  {:>10}  {:>9}",
        "tasks", "changed", "assemble", "analyse", "plan", "total", "selected"
    );
    for (tasks, changed) in [(100, 10), (1_000, 100), (10_000, 1_000)] {
        let paths = changed_paths(changed);
        let rounds = if tasks > 5_000 { 5 } else { 20 };

        let mut assemble_us = Vec::new();
        let mut analyse_us = Vec::new();
        let mut plan_us = Vec::new();
        let mut selected = 0;

        for _ in 0..rounds {
            let t = Instant::now();
            let g = build_graph(tasks);
            assemble_us.push(t.elapsed().as_micros());

            let t = Instant::now();
            let report = affected::analyse(&g, &paths).expect("analysis");
            analyse_us.push(t.elapsed().as_micros());

            let keys: BTreeSet<String> = report
                .selected()
                .iter()
                .map(|t| t.family_key.clone())
                .collect();
            selected = keys.len();
            let t = Instant::now();
            let p = plan::plan_for(&g, &report, &keys);
            plan_us.push(t.elapsed().as_micros());
            std::hint::black_box(p);
        }

        let (a, b, c) = (median(assemble_us), median(analyse_us), median(plan_us));
        println!(
            "{tasks:>8}  {changed:>8}  {:>9.1}ms  {:>9.1}ms  {:>9.1}ms  {:>9.1}ms  {selected:>9}",
            a as f64 / 1000.0,
            b as f64 / 1000.0,
            c as f64 / 1000.0,
            (a + b + c) as f64 / 1000.0,
        );
    }
}
