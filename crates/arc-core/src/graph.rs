//! The dependency graph Arc has learned, assembled for presentation.
//!
//! This is a read-only projection over families and their dependency sets. It
//! holds no knowledge of its own, so it can never claim more than the stored
//! sets justify.

use crate::db::Db;
use crate::dependency::DependencySet;
use crate::family::ExecutionFamily;
use crate::project::Project;
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub family_key: String,
    pub command: String,
    pub rel_cwd: String,
    pub runs: u64,
    pub last_seen: i64,
    pub backend: String,
    pub completeness: String,
    pub inputs_narrowed: bool,
    pub declared_inputs: Vec<String>,
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub executables: Vec<String>,
    pub observations: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Graph {
    pub schema: u32,
    pub project_root: String,
    pub nodes: Vec<Node>,
}

pub fn build(project: &Project, db: &Db) -> Result<Graph> {
    let root = project.root.to_string_lossy().to_string();
    let mut nodes = Vec::new();
    for family in db.families()? {
        if family.project_root != root {
            continue;
        }
        let deps = db.dependency_set(&family.key)?;
        nodes.push(node(&family, deps.as_ref()));
    }
    nodes.sort_by(|a, b| a.command.cmp(&b.command));
    Ok(Graph {
        schema: crate::dependency::DEPENDENCY_SCHEMA_VERSION,
        project_root: root,
        nodes,
    })
}

fn node(family: &ExecutionFamily, deps: Option<&DependencySet>) -> Node {
    let empty = DependencySet::empty(&family.key, family.last_seen);
    let d = deps.unwrap_or(&empty);
    Node {
        family_key: family.key.clone(),
        command: family.command_line(),
        rel_cwd: family.rel_cwd.clone(),
        runs: family.runs,
        last_seen: family.last_seen,
        backend: d.backend.clone(),
        completeness: d.completeness.label().to_string(),
        inputs_narrowed: d.inputs_are_narrowed(),
        declared_inputs: d.declared_inputs.clone(),
        inputs: d.inputs.clone(),
        outputs: d.outputs.clone(),
        executables: d.executables.iter().map(|e| e.path.clone()).collect(),
        observations: d.observations,
    }
}
