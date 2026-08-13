//! What the CI provider says about the job Arc is running inside.
//!
//! Provider detection is the only place in Arc that reads provider-specific
//! environment variables. Everything downstream sees [`CiContext`], so adding a
//! provider never means touching analysis, planning or reporting.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The provider variables Arc reads. Listing them makes the surface auditable
/// and keeps the rest of the process environment — which holds the job's
/// secrets — out of Arc's hands entirely.
pub const READS: &[&str] = &[
    "CI",
    "GITHUB_ACTIONS",
    "GITHUB_REPOSITORY",
    "GITHUB_EVENT_NAME",
    "GITHUB_EVENT_PATH",
    "GITHUB_SHA",
    "GITHUB_REF_NAME",
    "GITHUB_BASE_REF",
    "GITHUB_HEAD_REF",
    "GITHUB_RUN_ID",
    "GITHUB_RUN_ATTEMPT",
    "GITHUB_JOB",
    "GITHUB_STEP_SUMMARY",
    "GITHUB_OUTPUT",
    "ARC_CI_BASE",
    "ARC_CI_HEAD",
];

/// A snapshot of the variables above. Constructed from the process or from a
/// fixture, so provider behaviour is testable without a CI system.
#[derive(Debug, Clone, Default)]
pub struct Environment(BTreeMap<String, String>);

impl Environment {
    pub fn process() -> Environment {
        Environment(
            READS
                .iter()
                .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
                .collect(),
        )
    }

    pub fn from_pairs<I, K, V>(pairs: I) -> Environment
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Environment(
            pairs
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        )
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .get(key)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
    GithubActions,
    /// No recognised provider: a developer's machine, or a CI system Arc has no
    /// specific support for. Base and head come from the command line or from
    /// `ARC_CI_BASE`/`ARC_CI_HEAD`.
    Local,
}

impl Provider {
    pub fn label(&self) -> &'static str {
        match self {
            Provider::GithubActions => "github-actions",
            Provider::Local => "local",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    PullRequest,
    PullRequestTarget,
    Push,
    MergeGroup,
    WorkflowDispatch,
    Other,
}

impl Event {
    pub fn parse(name: &str) -> Event {
        match name {
            "pull_request" => Event::PullRequest,
            "pull_request_target" => Event::PullRequestTarget,
            "push" => Event::Push,
            "merge_group" => Event::MergeGroup,
            "workflow_dispatch" => Event::WorkflowDispatch,
            _ => Event::Other,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Event::PullRequest => "pull_request",
            Event::PullRequestTarget => "pull_request_target",
            Event::Push => "push",
            Event::MergeGroup => "merge_group",
            Event::WorkflowDispatch => "workflow_dispatch",
            Event::Other => "other",
        }
    }
}

/// Whether the code being built is code the repository's own maintainers
/// control. This is a policy input, not a security boundary: Arc uses it only
/// to decide whether to *write* to a shared cache, and defaults to the
/// cautious answer whenever the provider does not say clearly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trust {
    Trusted,
    Untrusted,
    Unknown,
}

impl Trust {
    pub fn label(&self) -> &'static str {
        match self {
            Trust::Trusted => "trusted",
            Trust::Untrusted => "untrusted",
            Trust::Unknown => "unknown",
        }
    }
}

/// Everything Arc needs from a CI provider, and nothing else. No tokens, no
/// event payload, no arbitrary environment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CiContext {
    pub provider: Provider,
    pub event: Event,
    pub repository: Option<String>,
    pub branch: Option<String>,
    pub base_branch: Option<String>,
    /// Commit the provider says is being built.
    pub sha: Option<String>,
    /// Revision the change should be compared against, as the provider reports
    /// it. Still resolved against the local repository before use.
    pub base_sha: Option<String>,
    pub head_sha: Option<String>,
    pub pull_request: Option<u64>,
    pub run_id: Option<String>,
    pub job: Option<String>,
    pub attempt: Option<u32>,
    pub is_fork: bool,
    pub trust: Trust,
    /// Why the event payload could not be used, when it could not. Reported,
    /// never fatal.
    pub note: Option<String>,
}

impl Default for CiContext {
    fn default() -> Self {
        CiContext {
            provider: Provider::Local,
            event: Event::Other,
            repository: None,
            branch: None,
            base_branch: None,
            sha: None,
            base_sha: None,
            head_sha: None,
            pull_request: None,
            run_id: None,
            job: None,
            attempt: None,
            is_fork: false,
            trust: Trust::Unknown,
            note: None,
        }
    }
}

impl CiContext {
    pub fn detect(env: &Environment) -> CiContext {
        if env.get("GITHUB_ACTIONS") == Some("true") {
            return super::github::context(env);
        }
        CiContext {
            provider: Provider::Local,
            base_sha: env.get("ARC_CI_BASE").map(str::to_string),
            head_sha: env.get("ARC_CI_HEAD").map(str::to_string),
            // A local run builds whatever the developer has checked out, which
            // is as trusted as anything gets.
            trust: Trust::Trusted,
            ..Default::default()
        }
    }

    pub fn in_ci(&self) -> bool {
        self.provider != Provider::Local
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_provider_variables_means_local_and_trusted() {
        let c = CiContext::detect(&Environment::default());
        assert_eq!(c.provider, Provider::Local);
        assert_eq!(c.trust, Trust::Trusted);
        assert!(!c.in_ci());
    }

    #[test]
    fn generic_environment_overrides_supply_base_and_head() {
        let c = CiContext::detect(&Environment::from_pairs([
            ("ARC_CI_BASE", "abc"),
            ("ARC_CI_HEAD", "def"),
        ]));
        assert_eq!(c.base_sha.as_deref(), Some("abc"));
        assert_eq!(c.head_sha.as_deref(), Some("def"));
    }
}
