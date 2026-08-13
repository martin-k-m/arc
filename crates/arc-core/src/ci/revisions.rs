//! Deciding what to compare against what.
//!
//! Getting this wrong is the one CI mistake Arc must not make quietly: compare
//! against the wrong base and tasks look unaffected when they are not. Every
//! path that cannot establish a complete diff therefore ends in
//! [`Comparison::Unknown`], which the analysis treats as "run everything".

use super::context::{CiContext, Event, Provider};
use crate::git;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Comparison {
    /// Two commits, both present locally. The normal CI case.
    CommitRange { base: String, head: String },
    /// A commit and whatever is in the working tree, including untracked files.
    /// What a developer reproducing CI locally means.
    WorkingTree { base: String },
    /// No usable comparison. Carries the reason, which is shown rather than
    /// swallowed.
    Unknown { reason: String },
}

impl Comparison {
    pub fn label(&self) -> &'static str {
        match self {
            Comparison::CommitRange { .. } => "commit-range",
            Comparison::WorkingTree { .. } => "working-tree",
            Comparison::Unknown { .. } => "unknown",
        }
    }

    pub fn base(&self) -> Option<&str> {
        match self {
            Comparison::CommitRange { base, .. } | Comparison::WorkingTree { base } => Some(base),
            Comparison::Unknown { .. } => None,
        }
    }

    pub fn head(&self) -> Option<&str> {
        match self {
            Comparison::CommitRange { head, .. } => Some(head),
            _ => None,
        }
    }

    pub fn is_known(&self) -> bool {
        !matches!(self, Comparison::Unknown { .. })
    }
}

/// How the base and head were arrived at, for `arc ci --explain` and doctor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resolution {
    pub comparison: Comparison,
    pub shallow: bool,
    /// Which revision expressions were tried, in order, and what became of them.
    pub note: Option<String>,
}

/// Resolve the comparison for this run.
///
/// `base`/`head` are the command line's overrides and win over everything the
/// provider says, so a CI decision can be reproduced locally exactly.
pub fn resolve(
    cwd: &Path,
    ctx: &CiContext,
    base: Option<&str>,
    head: Option<&str>,
    fetch: bool,
) -> Resolution {
    let shallow = git::is_shallow(cwd);
    let mut note = None;

    let base_spec = base
        .map(str::to_string)
        .or_else(|| ctx.base_sha.clone())
        .or_else(|| default_base(ctx, cwd));
    let Some(base_spec) = base_spec else {
        return Resolution {
            comparison: Comparison::Unknown {
                reason: missing_base(ctx),
            },
            shallow,
            note,
        };
    };

    let mut base_id = git::resolve(cwd, &base_spec).ok();
    if base_id.is_none() && fetch {
        // Only ever on request, only the one revision the analysis needs.
        match git::fetch(cwd, "origin", &base_spec, Some(50)) {
            Ok(()) => base_id = git::resolve(cwd, &base_spec).ok(),
            Err(e) => note = Some(format!("fetch failed: {e}")),
        }
    }
    let Some(base_id) = base_id.filter(|s| !s.is_empty()) else {
        return Resolution {
            comparison: Comparison::Unknown {
                reason: if shallow {
                    format!("base commit {base_spec} is not in this shallow clone")
                } else {
                    format!("base revision {base_spec} could not be resolved")
                },
            },
            shallow,
            note: note.or_else(|| {
                shallow.then(|| {
                    "check out with fetch-depth: 0, or pass --fetch to deepen it".to_string()
                })
            }),
        };
    };

    // An explicit `--base` with no `--head` means "compare the tree I have",
    // which is the only reading that lets a developer see their uncommitted work.
    let head_spec = head.map(str::to_string).or_else(|| {
        ctx.in_ci()
            .then(|| ctx.head_sha.clone().or_else(|| ctx.sha.clone()))
            .flatten()
    });
    let Some(head_spec) = head_spec else {
        return Resolution {
            comparison: Comparison::WorkingTree { base: base_id },
            shallow,
            note,
        };
    };
    match git::resolve(cwd, &head_spec).ok().filter(|s| !s.is_empty()) {
        Some(head_id) => Resolution {
            comparison: Comparison::CommitRange {
                base: base_id,
                head: head_id,
            },
            shallow,
            note,
        },
        // The provider named a head this checkout does not contain. Comparing
        // against HEAD instead would silently answer a different question.
        None => Resolution {
            comparison: Comparison::Unknown {
                reason: format!("head revision {head_spec} could not be resolved"),
            },
            shallow,
            note,
        },
    }
}

/// The provider gave no base. A push event can usually fall back to the commit
/// before the one being built; nothing else can guess safely.
fn default_base(ctx: &CiContext, cwd: &Path) -> Option<String> {
    match ctx.event {
        Event::Push if git::has_commit(cwd, "HEAD~1") => Some("HEAD~1".into()),
        _ => None,
    }
}

fn missing_base(ctx: &CiContext) -> String {
    match ctx.provider {
        Provider::Local => "no base revision; pass --base <rev>".into(),
        _ => format!(
            "the {} event does not carry a base revision",
            ctx.event.label()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unresolvable_base_is_unknown_rather_than_empty() {
        let dir = tempfile::tempdir().unwrap();
        let r = resolve(
            dir.path(),
            &CiContext::default(),
            Some("does-not-exist"),
            None,
            false,
        );
        assert!(!r.comparison.is_known());
        assert!(r.comparison.base().is_none());
    }

    #[test]
    fn no_base_at_all_is_unknown_with_an_actionable_reason() {
        let dir = tempfile::tempdir().unwrap();
        let r = resolve(dir.path(), &CiContext::default(), None, None, false);
        match r.comparison {
            Comparison::Unknown { reason } => assert!(reason.contains("--base")),
            other => panic!("{other:?}"),
        }
    }
}
