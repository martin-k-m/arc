//! The tasks CI is meant to run, and the dependency knowledge Arc has about
//! them.
//!
//! CI does not run "every command anyone has ever run here". It runs a declared
//! set from `arc.toml`, so a workflow's meaning lives in the repository rather
//! than in whatever happens to be in a cache database.

use crate::dependency::Completeness;
use crate::graph::{Consumed, Consumes, TaskNode, GRAPH_SCHEMA_VERSION};
use crate::project::Project;
use crate::remote::protocol::{RemoteTask, WireConsumes};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// A task the repository declares and CI may run. Its command comes from the
/// checked-out configuration and from nowhere else.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanonicalTask {
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
    pub rel_cwd: String,
    pub family_key: String,
    pub tags: Vec<String>,
    /// Task names this one must follow, from `[[command]] after`.
    pub after: Vec<String>,
}

impl CanonicalTask {
    pub fn command_line(&self) -> String {
        crate::record::format_command(&self.program, &self.args)
    }
}

/// Resolve the declared CI task set, applying `--task` filters.
///
/// A `[ci] tasks` entry naming a command that does not exist is a configuration
/// error, not something to quietly drop: silently running fewer tasks than the
/// repository asked for is the one failure mode CI must never have.
pub fn canonical(project: &Project, filters: &[String]) -> Result<Vec<CanonicalTask>> {
    let mut runnable = Vec::new();
    for c in &project.config.commands {
        let (Some(name), Some(program)) = (c.name.as_deref(), c.command.as_deref()) else {
            continue;
        };
        let command_line = crate::record::format_command(program, &c.args);
        let cfg = project.config_for(&command_line)?;
        runnable.push(CanonicalTask {
            name: name.to_string(),
            family_key: crate::family::family_key(program, &c.args, "", &cfg).hex(),
            program: program.to_string(),
            args: c.args.clone(),
            rel_cwd: String::new(),
            tags: c.tags.clone(),
            after: c.after.clone(),
        });
    }

    let named = |wanted: &[String], what: &str| -> Result<Vec<CanonicalTask>> {
        let mut out = Vec::new();
        for name in wanted {
            match runnable.iter().find(|t| &t.name == name) {
                Some(t) => out.push(t.clone()),
                None => bail!(
                    "{what} names `{name}`, which is not a runnable [[command]].\n\nA runnable task needs both `name` and `command`:\n\n  [[command]]\n  name = \"{name}\"\n  command = \"...\"\n  args = [...]"
                ),
            }
        }
        Ok(out)
    };

    let selected = if !project.config.ci.tasks.is_empty() {
        named(&project.config.ci.tasks, "[ci] tasks")?
    } else {
        runnable.clone()
    };
    if filters.is_empty() {
        return Ok(dedup(selected));
    }
    // A filter may name a task or a tag; both are ways of saying "this subset".
    let mut out = Vec::new();
    for f in filters {
        let matched: Vec<CanonicalTask> = selected
            .iter()
            .filter(|t| &t.name == f || t.tags.contains(f))
            .cloned()
            .collect();
        if matched.is_empty() {
            bail!("--task `{f}` matches no configured CI task or tag");
        }
        out.extend(matched);
    }
    Ok(dedup(out))
}

fn dedup(mut v: Vec<CanonicalTask>) -> Vec<CanonicalTask> {
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v.dedup_by(|a, b| a.family_key == b.family_key);
    v
}

/// Where a task's dependency knowledge came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Knowledge {
    Local,
    Remote,
    /// Nothing is known, so the task is unknown and therefore runs.
    None,
}

/// Adopt remote dependency knowledge for a locally declared task.
///
/// Security rule, and the reason this function takes the local task rather than
/// reading the command out of the record: remote metadata may describe a task
/// this repository already defines, and may never introduce one. The family key
/// is derived locally from the checked-out configuration; a record that does not
/// carry that exact key, or whose command does not match the local declaration
/// byte for byte, is discarded. A compromised cache server can therefore cause
/// Arc to run *more* work, never different work.
pub fn adopt(local: &CanonicalTask, remote: &RemoteTask, project_root: &str) -> Option<TaskNode> {
    remote.validate(Some(&local.family_key)).ok()?;
    remote.compatible_with_host().ok()?;
    if !remote.matches_local(&local.program, &local.args, &local.rel_cwd) {
        return None;
    }
    let completeness = match remote.completeness.as_str() {
        "complete" => Completeness::Complete,
        "partial" => Completeness::Partial,
        // Anything else — including a word this Arc does not know — is not a
        // basis for proving a task unaffected.
        _ => return Some(placeholder(local, project_root)),
    };
    let mut consumes = Vec::with_capacity(remote.consumes.len());
    for c in &remote.consumes {
        consumes.push(Consumed {
            path: c.path.decode().ok()?,
            kind: match c.kind {
                WireConsumes::File => Consumes::File,
                WireConsumes::Directory => Consumes::Directory,
                WireConsumes::Existence => Consumes::Existence,
            },
        });
    }
    let mut produces = Vec::with_capacity(remote.produces.len());
    for p in &remote.produces {
        produces.push(p.decode().ok()?);
    }
    Some(TaskNode {
        completeness,
        inputs_narrowed: remote.inputs_narrowed,
        produces,
        consumes,
        declared_inputs: remote.declared_inputs.clone(),
        observations: remote.observations,
        runs: 0,
        ..placeholder(local, project_root)
    })
}

/// A task Arc knows how to run and nothing else about. Unknown, therefore
/// selected, therefore executed and learned from.
pub fn placeholder(local: &CanonicalTask, project_root: &str) -> TaskNode {
    TaskNode {
        schema: GRAPH_SCHEMA_VERSION,
        family_key: local.family_key.clone(),
        program: local.program.clone(),
        args: local.args.clone(),
        rel_cwd: local.rel_cwd.clone(),
        project_root: project_root.to_string(),
        label: local.name.clone(),
        named: true,
        completeness: Completeness::Unsupported,
        inputs_narrowed: false,
        downgrades: 0,
        produces: Vec::new(),
        consumes: Vec::new(),
        declared_inputs: Vec::new(),
        // Declared ordering comes from the repository, so it holds even for a
        // task Arc has never observed.
        after: local.after.clone(),
        runs: 0,
        last_seen: 0,
        observations: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::protocol::{PathEncoding, WireConsumed, WirePath, PROTOCOL_VERSION};

    fn local() -> CanonicalTask {
        CanonicalTask {
            name: "test".into(),
            program: "cargo".into(),
            args: vec!["test".into()],
            rel_cwd: String::new(),
            family_key: "a".repeat(64),
            tags: vec![],
            after: vec![],
        }
    }

    fn remote() -> RemoteTask {
        RemoteTask {
            protocol: PROTOCOL_VERSION,
            graph_semantics: GRAPH_SCHEMA_VERSION,
            dependency_semantics: crate::dependency::DEPENDENCY_SCHEMA_VERSION,
            trace_semantics: crate::trace::TRACE_SEMANTICS_VERSION,
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            family_key: "a".repeat(64),
            program: "cargo".into(),
            args: vec!["test".into()],
            rel_cwd: String::new(),
            completeness: "complete".into(),
            inputs_narrowed: true,
            produces: vec![WirePath::from_rel("gen/x")],
            consumes: vec![WireConsumed {
                path: WirePath::from_rel("src/lib.rs"),
                kind: WireConsumes::File,
            }],
            declared_inputs: vec![],
            observations: 3,
            arc_version: "0.6.0".into(),
        }
    }

    #[test]
    fn complete_remote_knowledge_becomes_a_provable_node() {
        let n = adopt(&local(), &remote(), "/repo").unwrap();
        assert!(n.provable());
        assert_eq!(n.label, "test");
        assert_eq!(n.produces, vec!["gen/x"]);
        assert_eq!(n.consumes.len(), 1);
    }

    #[test]
    fn a_record_describing_a_different_command_is_discarded() {
        let mut r = remote();
        r.program = "rm".into();
        r.args = vec!["-rf".into(), "/".into()];
        assert!(adopt(&local(), &r, "/repo").is_none());
    }

    #[test]
    fn a_record_for_another_family_is_discarded() {
        let mut r = remote();
        r.family_key = "b".repeat(64);
        assert!(adopt(&local(), &r, "/repo").is_none());
    }

    #[test]
    fn stale_semantics_are_discarded() {
        for mut r in [remote(), remote(), remote()]
            .into_iter()
            .enumerate()
            .map(|(i, mut r)| {
                match i {
                    0 => r.graph_semantics += 1,
                    1 => r.dependency_semantics += 1,
                    _ => r.trace_semantics += 1,
                }
                r
            })
        {
            r.observations = 9;
            assert!(adopt(&local(), &r, "/repo").is_none());
        }
        let mut foreign = remote();
        foreign.os = "plan9".into();
        assert!(adopt(&local(), &foreign, "/repo").is_none());
    }

    #[test]
    fn partial_remote_knowledge_cannot_prove_a_task_unaffected() {
        let mut r = remote();
        r.completeness = "partial".into();
        r.inputs_narrowed = false;
        let n = adopt(&local(), &r, "/repo").unwrap();
        assert!(!n.provable());

        let mut unknown_word = remote();
        unknown_word.completeness = "something-new".into();
        let n = adopt(&local(), &unknown_word, "/repo").unwrap();
        assert!(!n.provable());
        assert!(
            n.consumes.is_empty(),
            "unreadable knowledge is not knowledge"
        );
    }

    #[test]
    fn a_traversing_path_invalidates_the_record() {
        let mut r = remote();
        r.produces = vec![WirePath {
            enc: PathEncoding::Utf8,
            v: "../../etc/passwd".into(),
        }];
        assert!(adopt(&local(), &r, "/repo").is_none());
    }
}
