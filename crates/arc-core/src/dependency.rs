//! What Arc has learned an execution family depends on.
//!
//! A dependency set is never a bare `Vec<PathBuf>`. Knowing *what* Arc observed
//! is useless without knowing *how much* it could observe, so every set carries
//! a [`Completeness`] that decides what the set may be used for.
//!
//! The rule that keeps this safe is one-directional:
//!
//! * knowledge may **add** to a cache key (more executables, more environment)
//!   under any completeness, because a key covering more can only cause misses;
//! * knowledge may **narrow** the input set only under [`Completeness::Complete`],
//!   because dropping an input that mattered is exactly a false hit.
//!
//! No backend shipping in this version reports `Complete`, so v0.2 narrows
//! nothing from tracing. The gate exists so that a read-capable backend can be
//! added without revisiting every call site.

use crate::hash::{Digest, Hasher};
use crate::trace::{Capabilities, FileOp, Observations};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Bumped when the meaning of a stored dependency set changes. A set recorded
/// under a different version is discarded, never reinterpreted.
pub const DEPENDENCY_SCHEMA_VERSION: u32 = 1;

/// How much of the execution the backend could actually see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Completeness {
    /// Every read, directory enumeration, existence check and descendant
    /// process was observed. Only this state permits input narrowing.
    Complete,
    /// Real observations, but the backend cannot see everything an execution
    /// depends on. Usable to strengthen a key, never to weaken one.
    Partial,
    /// No backend was available, or tracing was switched off.
    Unsupported,
    /// Stored data did not survive validation. Treated as no knowledge at all.
    Invalid,
}

impl Completeness {
    pub fn label(&self) -> &'static str {
        match self {
            Completeness::Complete => "complete",
            Completeness::Partial => "partial",
            Completeness::Unsupported => "unsupported",
            Completeness::Invalid => "invalid",
        }
    }

    /// The single gate protecting every false-hit risk in this module.
    pub fn may_narrow_inputs(&self) -> bool {
        matches!(self, Completeness::Complete)
    }

    /// Derived from what the backend claims plus whether this particular run
    /// lost events. A lossy run can never be `Complete`.
    pub fn of(caps: &Capabilities, lossy: bool) -> Completeness {
        if lossy {
            return Completeness::Partial;
        }
        let full = caps.file_reads
            && caps.file_writes
            && caps.directory_reads
            && caps.existence_checks
            && caps.process_tree
            && caps.executables;
        if full {
            Completeness::Complete
        } else {
            Completeness::Partial
        }
    }
}

/// An executable observed taking part in the execution, with the hash of its
/// contents at the time it was observed.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ExecutableDep {
    pub path: String,
    pub digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencySet {
    pub schema: u32,
    pub trace_schema: u32,
    pub family_key: String,
    pub backend: String,
    pub capabilities: Capabilities,
    pub completeness: Completeness,
    /// Project-relative files observed being read. Empty under every backend
    /// shipping today.
    pub inputs: Vec<String>,
    /// Directories whose contents were enumerated. Empty under every backend
    /// shipping today.
    pub directories: Vec<String>,
    /// Paths whose *absence* the execution depended on. Empty under every
    /// backend shipping today; modelled so a later backend does not need a
    /// schema change.
    pub absent: Vec<String>,
    /// Project-relative files the execution created, modified or deleted.
    pub outputs: Vec<String>,
    /// Input globs the user declared for this family via `[inputs]` or a
    /// matching `[[command]]` block. Present here so `arc affected` and
    /// `arc graph` can tell a narrowed family from an unscoped one.
    pub declared_inputs: Vec<String>,
    pub executables: Vec<ExecutableDep>,
    /// Environment variable names the execution was observed to read. Empty
    /// today; Arc keeps its conservative configured set instead.
    pub env: Vec<String>,
    pub created_at: i64,
    pub last_validated_at: i64,
    pub arc_version: String,
    /// Executions folded into this set. Higher counts mean more observations
    /// have agreed, which `arc graph` surfaces.
    pub observations: u64,
}

impl DependencySet {
    pub fn empty(family_key: &str, now: i64) -> DependencySet {
        DependencySet {
            schema: DEPENDENCY_SCHEMA_VERSION,
            trace_schema: crate::trace::TRACE_SCHEMA_VERSION,
            family_key: family_key.to_string(),
            backend: "none".into(),
            capabilities: Capabilities::NONE,
            completeness: Completeness::Unsupported,
            inputs: Vec::new(),
            directories: Vec::new(),
            absent: Vec::new(),
            outputs: Vec::new(),
            declared_inputs: Vec::new(),
            executables: Vec::new(),
            env: Vec::new(),
            created_at: now,
            last_validated_at: now,
            arc_version: crate::VERSION.to_string(),
            observations: 0,
        }
    }

    /// Build a set from one traced execution.
    pub fn from_observations(
        family_key: &str,
        backend: &str,
        caps: Capabilities,
        obs: &Observations,
        now: i64,
    ) -> DependencySet {
        let mut set = DependencySet::empty(family_key, now);
        set.backend = backend.to_string();
        set.capabilities = caps;
        set.completeness = Completeness::of(&caps, obs.lossy);
        set.observations = 1;

        let mut inputs = BTreeSet::new();
        let mut outputs = BTreeSet::new();
        let mut absent = BTreeSet::new();
        for f in &obs.files {
            let Some(rel) = &f.rel else { continue };
            match f.op {
                FileOp::Read | FileOp::Execute => {
                    inputs.insert(rel.clone());
                }
                FileOp::Write | FileOp::Create | FileOp::Rename => {
                    outputs.insert(rel.clone());
                }
                FileOp::Delete => {
                    outputs.insert(rel.clone());
                    absent.insert(rel.clone());
                }
            }
        }
        // A file both read and written is an input first: dropping it would be
        // the unsafe direction.
        for rel in &inputs {
            outputs.remove(rel);
        }
        set.inputs = inputs.into_iter().collect();
        set.outputs = outputs.into_iter().collect();
        set.absent = absent.into_iter().collect();

        let mut execs: BTreeSet<ExecutableDep> = BTreeSet::new();
        for p in &obs.processes {
            let Some(image) = &p.image else { continue };
            let digest = crate::hash::hash_file(std::path::Path::new(image))
                .map(|d| d.hex())
                .unwrap_or_default();
            execs.insert(ExecutableDep {
                path: image.clone(),
                digest,
            });
        }
        set.executables = execs.into_iter().collect();
        set
    }

    /// Fold a new observation into stored knowledge.
    ///
    /// Inputs, directories and absences are unioned: an execution may take a
    /// different branch on a later run, and forgetting a dependency observed
    /// once is the unsafe direction. Executables are unioned by path with the
    /// newest digest winning, so a toolchain upgrade is reflected rather than
    /// pinned forever.
    pub fn merge(&mut self, new: &DependencySet, now: i64) {
        fn union(dst: &mut Vec<String>, src: &[String]) {
            let mut set: BTreeSet<String> = std::mem::take(dst).into_iter().collect();
            set.extend(src.iter().cloned());
            *dst = set.into_iter().collect();
        }
        union(&mut self.inputs, &new.inputs);
        union(&mut self.directories, &new.directories);
        union(&mut self.absent, &new.absent);
        union(&mut self.outputs, &new.outputs);
        union(&mut self.env, &new.env);
        // Declared scope is replaced, not unioned: it mirrors the current
        // configuration, and a glob the user deleted must stop applying.
        self.declared_inputs = new.declared_inputs.clone();

        let mut by_path: std::collections::BTreeMap<String, String> = self
            .executables
            .drain(..)
            .map(|e| (e.path, e.digest))
            .collect();
        for e in &new.executables {
            by_path.insert(e.path.clone(), e.digest.clone());
        }
        self.executables = by_path
            .into_iter()
            .map(|(path, digest)| ExecutableDep { path, digest })
            .collect();

        // A file becoming an input demotes it from the output list.
        let inputs: BTreeSet<&String> = self.inputs.iter().collect();
        self.outputs.retain(|o| !inputs.contains(o));

        self.backend = new.backend.clone();
        self.capabilities = new.capabilities;
        // Confidence is the weakest of everything folded in, never the best
        // run's.
        self.completeness = match (self.completeness, new.completeness) {
            (Completeness::Complete, Completeness::Complete) => Completeness::Complete,
            (Completeness::Unsupported, c) | (c, Completeness::Unsupported) => c,
            _ => Completeness::Partial,
        };
        self.last_validated_at = now;
        self.arc_version = crate::VERSION.to_string();
        self.observations += new.observations;
    }

    /// Reject stored knowledge that may no longer describe reality.
    ///
    /// Anything that changes how Arc traces, what it can observe, or what
    /// counts as an input invalidates the set. Retracing costs one execution;
    /// trusting a stale set costs correctness.
    pub fn validate(&self, family_key: &str, caps: &Capabilities) -> Completeness {
        if self.schema != DEPENDENCY_SCHEMA_VERSION
            || self.trace_schema != crate::trace::TRACE_SCHEMA_VERSION
            || self.family_key != family_key
            || self.arc_version != crate::VERSION
            || &self.capabilities != caps
        {
            return Completeness::Invalid;
        }
        self.completeness
    }

    /// The part of a dependency set that participates in the execution key.
    ///
    /// Only executables contribute today. Their digests are taken from the
    /// *stored* set rather than from the current run, so the key is decided
    /// before the command runs and does not drift between observations.
    pub fn key_digest(&self) -> Digest {
        let mut h = Hasher::new();
        h.field(DEPENDENCY_SCHEMA_VERSION.to_le_bytes());
        h.field((self.executables.len() as u64).to_le_bytes());
        for e in &self.executables {
            h.field(&e.path);
            h.field(&e.digest);
        }
        h.finish()
    }

    /// Whether Arc knows enough to say a change is *irrelevant* to this family.
    ///
    /// Without this, "not in the dependency list" means "not observed yet",
    /// which is not the same as "does not matter" — and reporting the second
    /// when only the first is true is how a dependency tool starts lying.
    pub fn inputs_are_narrowed(&self) -> bool {
        !self.declared_inputs.is_empty() || self.completeness.may_narrow_inputs()
    }

    pub fn is_empty(&self) -> bool {
        self.inputs.is_empty()
            && self.outputs.is_empty()
            && self.executables.is_empty()
            && self.directories.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Scope;
    use crate::trace::model::{FileObservation, ProcessObservation};

    fn caps(reads: bool) -> Capabilities {
        Capabilities {
            file_reads: reads,
            file_writes: true,
            directory_reads: reads,
            existence_checks: reads,
            process_tree: true,
            executables: true,
            outside_project: false,
        }
    }

    fn file(rel: &str, op: FileOp) -> FileObservation {
        FileObservation {
            path: format!("/repo/{rel}"),
            rel: Some(rel.to_string()),
            op,
            scope: Scope::Project,
            existed_before: Some(false),
        }
    }

    #[test]
    fn narrowing_requires_complete_observation() {
        assert!(!Completeness::of(&caps(false), false).may_narrow_inputs());
        assert!(Completeness::of(&caps(true), false).may_narrow_inputs());
        // A lossy run is never complete, however capable the backend claims to be.
        assert!(!Completeness::of(&caps(true), true).may_narrow_inputs());
    }

    #[test]
    fn a_file_that_is_read_and_written_stays_an_input() {
        let obs = Observations {
            files: vec![file("gen.rs", FileOp::Write), file("gen.rs", FileOp::Read)],
            ..Default::default()
        };
        let set = DependencySet::from_observations("f", "test", caps(true), &obs, 0);
        assert_eq!(set.inputs, vec!["gen.rs"]);
        assert!(set.outputs.is_empty());
    }

    #[test]
    fn merging_unions_knowledge_and_keeps_the_weakest_confidence() {
        let now = 1;
        let a = DependencySet::from_observations(
            "f",
            "t",
            caps(true),
            &Observations {
                files: vec![file("a.rs", FileOp::Read)],
                ..Default::default()
            },
            now,
        );
        let b = DependencySet::from_observations(
            "f",
            "t",
            caps(true),
            &Observations {
                files: vec![file("b.rs", FileOp::Read)],
                lossy: true,
                ..Default::default()
            },
            now,
        );
        let mut merged = a.clone();
        merged.merge(&b, now);
        assert_eq!(merged.inputs, vec!["a.rs", "b.rs"]);
        assert_eq!(merged.completeness, Completeness::Partial);
        assert_eq!(merged.observations, 2);
    }

    #[test]
    fn stale_or_foreign_sets_are_invalid() {
        let set = DependencySet::from_observations(
            "family-a",
            "t",
            caps(false),
            &Observations::default(),
            0,
        );
        assert_eq!(
            set.validate("family-a", &caps(false)),
            Completeness::Partial
        );
        assert_eq!(
            set.validate("family-b", &caps(false)),
            Completeness::Invalid
        );
        // A backend upgrade that changes what can be seen invalidates knowledge
        // gathered under the old capabilities.
        assert_eq!(set.validate("family-a", &caps(true)), Completeness::Invalid);
    }

    #[test]
    fn executables_contribute_to_the_key_and_upgrade_changes_it() {
        let mk = |digest: &str| DependencySet {
            executables: vec![ExecutableDep {
                path: "/bin/cargo".into(),
                digest: digest.into(),
            }],
            ..DependencySet::empty("f", 0)
        };
        assert_eq!(mk("aa").key_digest(), mk("aa").key_digest());
        assert_ne!(mk("aa").key_digest(), mk("bb").key_digest());
    }

    #[test]
    fn observed_processes_become_executable_dependencies() {
        let obs = Observations {
            processes: vec![ProcessObservation {
                pid: 4,
                image: Some("/bin/rustc".into()),
                scope: Some(Scope::External),
                descendant: true,
            }],
            ..Default::default()
        };
        let set = DependencySet::from_observations("f", "t", caps(false), &obs, 0);
        assert_eq!(set.executables.len(), 1);
        assert_eq!(set.executables[0].path, "/bin/rustc");
    }
}
