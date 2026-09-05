//! Tests for the repository's own container harness, `scripts/linux-check.sh`.
//!
//! The Linux tracer cannot be built on macOS or Windows, so that script is the
//! only gate the two Linux backends ever pass through on a developer machine.
//! It ran nothing, and exited 0 while doing it: an apostrophe in a comment
//! closed the single-quoted script it handed to `bash -c`, so the container
//! received one comment line and the caller's own arguments were dropped as
//! loose words. A harness that silently runs nothing is worse than no harness,
//! because it answers "did you check this on Linux" with yes.
//!
//! These tests substitute a `docker` that records its arguments instead of
//! starting a container, so they assert on what the harness *asks for* without
//! needing Docker, a network, or Linux.

#![cfg(unix)]

use std::path::PathBuf;
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

/// Run `scripts/linux-check.sh` with a fake `docker` first on `PATH`, and
/// return the argument vector the harness handed it.
fn docker_argv(args: &[&str]) -> Vec<String> {
    let tmp = tempfile::tempdir().unwrap();
    let fake = tmp.path().join("docker");
    std::fs::write(
        &fake,
        "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

    let path = format!(
        "{}:{}",
        tmp.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new("bash")
        .arg(repo_root().join("scripts/linux-check.sh"))
        .args(args)
        .current_dir(repo_root())
        .env("PATH", path)
        .output()
        .expect("running linux-check.sh");
    assert!(
        out.status.success(),
        "harness failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|s| s.to_string())
        .collect()
}

/// The defect, stated directly: whatever the caller asked for has to arrive.
#[test]
fn the_container_harness_passes_the_callers_arguments_through() {
    let argv = docker_argv(&["test", "--workspace", "--no-fail-fast"]);
    for want in ["test", "--workspace", "--no-fail-fast"] {
        assert!(
            argv.iter().any(|a| a == want),
            "`{want}` never reached docker; the harness asked for:\n{argv:#?}"
        );
    }
}

/// The harness must name something the container can actually execute. It used
/// to hand over a comment, which `bash -c` runs successfully and silently.
#[test]
fn the_container_harness_runs_a_real_script_not_a_comment() {
    let argv = docker_argv(&["test"]);
    assert!(
        !argv
            .iter()
            .any(|a| a.trim_start().starts_with('#') || a.contains("# The host target directory")),
        "the harness handed the container a comment to run:\n{argv:#?}"
    );
    assert!(
        argv.iter().any(|a| a.ends_with(".sh")),
        "the harness should name a script file, so no quoting can truncate it:\n{argv:#?}"
    );
}

/// Loose words after the image are the signature of quoting that ended early.
/// `gigabytes.` was an argument to `docker run` for as long as this was broken.
#[test]
fn the_container_harness_leaks_no_fragments_of_its_own_comments() {
    let argv = docker_argv(&["test", "--workspace"]);
    for junk in ["gigabytes.", "and", "can", "platforms"] {
        assert!(
            !argv.iter().any(|a| a == junk),
            "`{junk}` is a fragment of a comment, not an argument:\n{argv:#?}"
        );
    }
}

/// Every script the harness names must exist and parse, or the failure is
/// again deferred to a container nobody is watching.
#[test]
fn every_script_the_harness_names_exists_and_parses() {
    let argv = docker_argv(&["test"]);
    let named: Vec<&String> = argv.iter().filter(|a| a.ends_with(".sh")).collect();
    assert!(!named.is_empty(), "no script named: {argv:#?}");
    for s in named {
        // The path is the container's; map /src back onto the checkout.
        let host = repo_root().join(s.strip_prefix("/src/").unwrap_or(s));
        assert!(host.exists(), "harness names {s}, which does not exist");
        let check = Command::new("bash")
            .arg("-n")
            .arg(&host)
            .output()
            .expect("bash -n");
        assert!(
            check.status.success(),
            "{s} is not valid bash: {}",
            String::from_utf8_lossy(&check.stderr)
        );
    }
}
