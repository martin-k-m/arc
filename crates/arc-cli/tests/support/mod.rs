//! Shared helpers for the end-to-end suites.
//!
//! A directory rather than a file, so cargo does not compile it as a test
//! binary of its own.

use std::process::{Child, Command, Output, Stdio};

/// Spawn a child with its output captured, for [`wait_ok`].
///
/// Concurrency tests spawn several `arc` processes at once and then wait on
/// them in turn, so a child that outran the pipe buffer while an earlier
/// sibling was still being waited on would deadlock. Every command driven this
/// way prints a few hundred bytes at most, well inside the buffer; a test that
/// makes `arc` verbose must not use this.
pub fn spawn_captured(cmd: &mut Command) -> Child {
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning arc")
}

/// Wait for a child spawned by [`spawn_captured`] and fail with what it did.
///
/// This replaces `assert!(c.wait().unwrap().success())`, which throws away the
/// exit status and every byte the child wrote — and worse, with inherited
/// stdio the child's output goes to the test harness's own stdout rather than
/// to the failing test's captured block, so it is not merely unreported but
/// genuinely gone.
///
/// Two of these assertions failed in a container run of the workspace with
/// nothing to show for it: `concurrent_traced_runs_stay_independent` and
/// `concurrent_traced_runs_do_not_corrupt_dependency_metadata`. `docs/BUGS.md`
/// #11 and #12 both say in as many words that what those investigations lacked
/// was diagnostic output at the moment of failure. This is the cheapest half of
/// that, and it costs a passing run nothing.
pub fn wait_ok(label: &str, child: Child) -> Output {
    let out = child.wait_with_output().expect("waiting for arc");
    assert!(
        out.status.success(),
        "{label} exited {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "by signal".into()),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    out
}
