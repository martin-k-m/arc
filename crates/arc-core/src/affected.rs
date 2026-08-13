//! Which known executions a set of working-tree changes may affect.
//!
//! The interesting cases are the negative ones, and they are exactly where a
//! dependency tool can quietly become wrong. Arc only calls a family
//! *unaffected* when its inputs are narrowed — that is, when the user scoped it
//! with `[[command]] inputs` or a complete trace backend observed its reads.
//!
//! A family whose inputs were never narrowed is reported as **unknown**, not as
//! unaffected. "I have not observed a dependency on this file" and "this file
//! does not matter" are different claims, and only the second is safe to act on.

use crate::db::Db;
use crate::dependency::DependencySet;
use crate::family::ExecutionFamily;
use crate::git;
use crate::project::Project;
use crate::scan::build_globs;
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// A changed path is in this family's known input set.
    Affected,
    /// Inputs are narrowed and no changed path is among them.
    Unaffected,
    /// Inputs were never narrowed, so Arc cannot rule the change out.
    Unknown,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FamilyVerdict {
    pub family_key: String,
    pub command: String,
    pub rel_cwd: String,
    pub verdict: Verdict,
    /// Changed paths that put this family in the `Affected` bucket.
    pub matched: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub changes: Vec<git::Change>,
    pub families: Vec<FamilyVerdict>,
    /// Set when Git could not answer, so callers report that rather than
    /// silently showing an empty change list.
    pub git_error: Option<String>,
}

impl Report {
    pub fn of(&self, v: Verdict) -> impl Iterator<Item = &FamilyVerdict> {
        self.families.iter().filter(move |f| f.verdict == v)
    }
}

/// Compute the report for the current working tree.
pub fn compute(project: &Project, db: &Db, cwd: &std::path::Path) -> Result<Report> {
    let (changes, git_error) = match git::changes(cwd) {
        Ok(c) => (c, None),
        Err(e) => (Vec::new(), Some(e.to_string())),
    };
    let touched = git::touched_paths(&changes);

    let project_id = crate::hash::hash_bytes(project.root.to_string_lossy().as_bytes()).hex();
    let mut families = Vec::new();
    for family in db.families()? {
        if family.project_root != project.root.to_string_lossy() {
            continue;
        }
        let deps = db.dependency_set(&family.key)?;
        families.push(verdict_for(
            &family,
            deps.as_ref(),
            &touched,
            &project_id,
            db,
        )?);
    }
    families.sort_by(|a, b| a.command.cmp(&b.command));
    Ok(Report {
        changes,
        families,
        git_error,
    })
}

fn verdict_for(
    family: &ExecutionFamily,
    deps: Option<&DependencySet>,
    touched: &[String],
    project_id: &str,
    db: &Db,
) -> Result<FamilyVerdict> {
    let mut matched = Vec::new();
    let mut verdict = Verdict::Unknown;

    if let Some(deps) = deps {
        if deps.inputs_are_narrowed() {
            let globs = build_globs(&deps.declared_inputs)?;
            for path in touched {
                let hit = globs.is_match(path)
                    || deps.inputs.iter().any(|i| i == path)
                    || db
                        .families_depending_on(project_id, path)?
                        .iter()
                        .any(|f| f == &family.key);
                if hit {
                    matched.push(path.clone());
                }
            }
            verdict = if matched.is_empty() {
                Verdict::Unaffected
            } else {
                Verdict::Affected
            };
        }
    }

    Ok(FamilyVerdict {
        family_key: family.key.clone(),
        command: family.command_line(),
        rel_cwd: family.rel_cwd.clone(),
        verdict,
        matched,
    })
}
