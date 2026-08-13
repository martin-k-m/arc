//! GitHub Actions: context, event payload, and the three output channels a
//! workflow gives a tool.
//!
//! The event payload is attacker-influenced on a fork pull request — a branch
//! name is whatever the contributor typed. Everything read from it is treated
//! as data: parsed with no panics, never interpolated into a command, and
//! escaped before it reaches a workflow command or a Markdown summary.

use super::context::{CiContext, Environment, Event, Provider, Trust};
use anyhow::Result;
use std::io::{Read, Write};
use std::path::Path;

/// A payload larger than this is not one GitHub produced.
const MAX_EVENT_BYTES: u64 = 8 << 20;

pub fn context(env: &Environment) -> CiContext {
    let event = Event::parse(env.get("GITHUB_EVENT_NAME").unwrap_or_default());
    let repository = env.get("GITHUB_REPOSITORY").map(str::to_string);
    let mut ctx = CiContext {
        provider: Provider::GithubActions,
        event,
        branch: env
            .get("GITHUB_HEAD_REF")
            .or(env.get("GITHUB_REF_NAME"))
            .map(str::to_string),
        base_branch: env.get("GITHUB_BASE_REF").map(str::to_string),
        sha: env.get("GITHUB_SHA").map(str::to_string),
        run_id: env.get("GITHUB_RUN_ID").map(str::to_string),
        job: env.get("GITHUB_JOB").map(str::to_string),
        attempt: env.get("GITHUB_RUN_ATTEMPT").and_then(|v| v.parse().ok()),
        repository: repository.clone(),
        ..Default::default()
    };

    match env
        .get("GITHUB_EVENT_PATH")
        .map(|p| read_event(Path::new(p)))
    {
        Some(Ok(payload)) => apply_payload(&mut ctx, &payload, repository.as_deref()),
        Some(Err(e)) => ctx.note = Some(format!("event payload unavailable: {e}")),
        None => ctx.note = Some("no event payload; using environment only".into()),
    }

    // Explicit overrides win over anything derived, so a workflow can correct
    // Arc rather than work around it.
    if let Some(v) = env.get("ARC_CI_BASE") {
        ctx.base_sha = Some(v.to_string());
    }
    if let Some(v) = env.get("ARC_CI_HEAD") {
        ctx.head_sha = Some(v.to_string());
    }
    ctx.trust = trust_of(&ctx);
    ctx
}

/// A `pull_request_target` job runs with the base repository's secrets while
/// the pull request's author controls the change under review. Arc cannot see
/// which tree was actually checked out, so it never calls that event trusted.
fn trust_of(ctx: &CiContext) -> Trust {
    if ctx.is_fork {
        return Trust::Untrusted;
    }
    match ctx.event {
        Event::PullRequestTarget => Trust::Unknown,
        Event::Push | Event::MergeGroup | Event::WorkflowDispatch | Event::PullRequest => {
            Trust::Trusted
        }
        Event::Other => Trust::Unknown,
    }
}

fn read_event(path: &Path) -> Result<serde_json::Value> {
    let mut buf = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_EVENT_BYTES + 1)
        .read_to_end(&mut buf)?;
    anyhow::ensure!(
        buf.len() as u64 <= MAX_EVENT_BYTES,
        "event payload is too large"
    );
    Ok(serde_json::from_slice(&buf)?)
}

fn apply_payload(ctx: &mut CiContext, payload: &serde_json::Value, repository: Option<&str>) {
    let sha = |v: Option<&serde_json::Value>| -> Option<String> {
        let s = v?.as_str()?;
        // The all-zero sha is how GitHub says "there was no previous commit".
        (is_sha(s) && s.bytes().any(|b| b != b'0')).then(|| s.to_string())
    };
    match ctx.event {
        Event::PullRequest | Event::PullRequestTarget => {
            let pr = payload.get("pull_request");
            ctx.pull_request = pr.and_then(|p| p.get("number")).and_then(|n| n.as_u64());
            ctx.base_sha = sha(pr.and_then(|p| p.pointer("/base/sha")));
            ctx.head_sha = sha(pr.and_then(|p| p.pointer("/head/sha")));
            let head_repo = pr
                .and_then(|p| p.pointer("/head/repo/full_name"))
                .and_then(|v| v.as_str());
            let flagged = pr
                .and_then(|p| p.pointer("/head/repo/fork"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            ctx.is_fork =
                flagged || matches!((head_repo, repository), (Some(h), Some(r)) if h != r);
        }
        Event::MergeGroup => {
            let mg = payload.get("merge_group");
            ctx.base_sha = sha(mg.and_then(|m| m.get("base_sha")));
            ctx.head_sha = sha(mg.and_then(|m| m.get("head_sha")));
        }
        Event::Push => {
            ctx.base_sha = sha(payload.get("before"));
            ctx.head_sha = sha(payload.get("after"));
        }
        Event::WorkflowDispatch | Event::Other => {}
    }
}

fn is_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Escape a value so it cannot end a workflow command and start another.
/// GitHub decodes these three sequences and nothing else.
pub fn escape_command(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '%' => out.push_str("%25"),
            '\r' => out.push_str("%0D"),
            '\n' => out.push_str("%0A"),
            _ => out.push(c),
        }
    }
    out
}

pub fn warning(message: &str) -> String {
    format!("::warning::{}", escape_command(&super::sanitize(message)))
}

pub fn error(message: &str) -> String {
    format!("::error::{}", escape_command(&super::sanitize(message)))
}

/// Append a Markdown block to the job summary. A summary that cannot be
/// written is not a build failure — it is a missing convenience.
pub fn write_summary(path: &Path, markdown: &str) -> Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(markdown.as_bytes())?;
    if !markdown.ends_with('\n') {
        f.write_all(b"\n")?;
    }
    Ok(())
}

/// Append `name=value` pairs for later steps. Only values Arc generates itself
/// are written, and any that could contain a delimiter are refused rather than
/// escaped, because a value that breaks the file format breaks the whole step.
pub fn write_outputs(path: &Path, pairs: &[(&str, String)]) -> Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    for (name, value) in pairs {
        if value.contains('\n') || value.contains('\r') || !safe_name(name) {
            continue;
        }
        writeln!(f, "{name}={value}")?;
    }
    Ok(())
}

fn safe_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(json: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("event.json");
        std::fs::write(&p, json).unwrap();
        let s = p.to_string_lossy().to_string();
        (dir, s)
    }

    fn env(event: &str, path: &str, repo: &str) -> Environment {
        Environment::from_pairs([
            ("GITHUB_ACTIONS", "true"),
            ("GITHUB_EVENT_NAME", event),
            ("GITHUB_EVENT_PATH", path),
            ("GITHUB_REPOSITORY", repo),
            ("GITHUB_SHA", &"c".repeat(40)),
        ])
    }

    #[test]
    fn a_pull_request_compares_head_against_base() {
        let (_d, p) = payload(&format!(
            r#"{{"pull_request":{{"number":42,"base":{{"sha":"{}"}},"head":{{"sha":"{}","repo":{{"full_name":"me/repo","fork":false}}}}}}}}"#,
            "a".repeat(40),
            "b".repeat(40)
        ));
        let c = CiContext::detect(&env("pull_request", &p, "me/repo"));
        assert_eq!(c.provider, Provider::GithubActions);
        assert_eq!(c.pull_request, Some(42));
        assert_eq!(c.base_sha, Some("a".repeat(40)));
        assert_eq!(c.head_sha, Some("b".repeat(40)));
        assert!(!c.is_fork);
        assert_eq!(c.trust, Trust::Trusted);
    }

    #[test]
    fn a_fork_pull_request_is_untrusted() {
        let (_d, p) = payload(&format!(
            r#"{{"pull_request":{{"number":7,"base":{{"sha":"{}"}},"head":{{"sha":"{}","repo":{{"full_name":"someone/fork","fork":true}}}}}}}}"#,
            "a".repeat(40),
            "b".repeat(40)
        ));
        let c = CiContext::detect(&env("pull_request", &p, "me/repo"));
        assert!(c.is_fork);
        assert_eq!(c.trust, Trust::Untrusted);
    }

    #[test]
    fn pull_request_target_is_never_trusted() {
        let (_d, p) = payload(r#"{"pull_request":{"number":1}}"#);
        let c = CiContext::detect(&env("pull_request_target", &p, "me/repo"));
        assert_eq!(c.trust, Trust::Unknown);
    }

    #[test]
    fn a_merge_group_uses_its_own_base_and_head() {
        let (_d, p) = payload(&format!(
            r#"{{"merge_group":{{"base_sha":"{}","head_sha":"{}"}}}}"#,
            "a".repeat(40),
            "b".repeat(40)
        ));
        let c = CiContext::detect(&env("merge_group", &p, "me/repo"));
        assert_eq!(c.event, Event::MergeGroup);
        assert_eq!(c.base_sha, Some("a".repeat(40)));
        assert_eq!(c.trust, Trust::Trusted);
    }

    #[test]
    fn a_push_uses_before_and_after_but_not_the_null_sha() {
        let (_d, p) = payload(&format!(
            r#"{{"before":"{}","after":"{}"}}"#,
            "0".repeat(40),
            "b".repeat(40)
        ));
        let c = CiContext::detect(&env("push", &p, "me/repo"));
        assert_eq!(c.base_sha, None, "a first push has no previous commit");
        assert_eq!(c.head_sha, Some("b".repeat(40)));
    }

    #[test]
    fn a_malformed_or_missing_payload_is_reported_not_fatal() {
        let (_d, p) = payload("{ this is not json");
        let c = CiContext::detect(&env("pull_request", &p, "me/repo"));
        assert!(c.note.is_some());
        assert_eq!(c.base_sha, None);

        let c = CiContext::detect(&env("pull_request", "/nonexistent/event.json", "me/repo"));
        assert!(c.note.is_some());

        // Wrong types everywhere, and a base sha that is not a sha.
        let (_d, p) = payload(
            r#"{"pull_request":{"number":"x","base":{"sha":["nope"]},"head":{"sha":"zz"}}}"#,
        );
        let c = CiContext::detect(&env("pull_request", &p, "me/repo"));
        assert_eq!(c.pull_request, None);
        assert_eq!(c.base_sha, None);
        assert_eq!(c.head_sha, None);
    }

    #[test]
    fn workflow_command_content_cannot_start_another_command() {
        // Only a `::` at the start of a line is a workflow command, so the
        // invariant is that user content can never begin one.
        let hostile = "release\n::error::owned\r%2Fx\x1b[2J";
        let line = warning(hostile);
        assert_eq!(line.lines().count(), 1);
        assert!(line.starts_with("::warning::"));
        assert!(!line.contains('\x1b'));
        assert!(line.contains("%25"), "a literal percent must not decode");

        // A caller that escapes without sanitising is still safe.
        assert_eq!(escape_command("a\nb\rc%d"), "a%0Ab%0Dc%25d");
    }

    #[test]
    fn outputs_refuse_names_and_values_that_would_break_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("out");
        write_outputs(
            &p,
            &[
                ("executed_count", "3".into()),
                ("bad_value", "a\nEOF\nb".into()),
                ("BadName", "1".into()),
            ],
        )
        .unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(text, "executed_count=3\n");
    }
}
