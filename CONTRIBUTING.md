# Contributing

## Build and test

```bash
cargo build --workspace
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all --check
```

Some tests need a real git repository and a git identity:

```bash
git config --global user.email you@example.com
git config --global user.name you
```

## Platform notes

**Linux** is where the tracing backends live. `scripts/linux-check.sh` runs any
cargo command in a container, which is also the environment most likely to
*refuse* tracing — useful for exercising the fallback paths:

```bash
scripts/linux-check.sh test --workspace
ARC_DOCKER_ARGS=--security-opt=seccomp=unconfined scripts/linux-check.sh test -p arc-cli --test trace_differential
```

Docker's default seccomp profile blocks the `seccomp` syscall, so the fast
backend is unavailable inside a default container and Arc falls back to ptrace.
That is the correct behaviour, and it means the differential suite silently
skips unless you pass `--security-opt=seccomp=unconfined`.

**Windows** needs Developer Mode or an elevated shell for the symlink tests;
without it they skip rather than fail.

## Where things are

| | |
| --- | --- |
| `crates/arc-core` | the engine: keys, CAS, tracing, graph, scheduler, remote client |
| `crates/arc-cli` | the `arc` binary and the end-to-end tests |
| `crates/arc-cache` | the reference shared-cache server |
| `crates/arc-worker` | the remote execution worker |
| `docs/` | architecture, correctness, tracing, security, protocols |
| `scripts/` | benchmarks and demos |

Most end-to-end tests live in `crates/arc-cli/tests/` and drive the real binary.

## Benchmarking

```bash
scripts/trace-bench.sh      # tracing backends
scripts/scale-bench.sh      # 1k / 10k / 100k files — watch the per-1k column
scripts/bench.sh            # cache hits and misses
scripts/sched-bench.sh      # the scheduler
scripts/exec-bench.sh       # remote execution
scripts/env-bench.sh        # environments
```

Report medians, not single runs, and say which machine produced them.

## What a change should come with

- a test that fails without it
- `cargo fmt`, `clippy -D warnings` and the full suite passing
- no new comments explaining what the code already says

Comments are for things the code cannot state: unsafe invariants, kernel and
wire-format constraints, security boundaries, and non-obvious cache correctness.

## Correctness

One rule governs everything else:

> Arc never returns a cached result unless the current execution state is
> authorized to reuse it.

A **false miss** — running something that did not need running — is a
performance bug. A **false hit** is a correctness bug, and is treated as a
release blocker. When completeness cannot be proven, the answer is to become
more conservative, never more optimistic.

If a change makes Arc faster by narrowing what it checks, the pull request needs
to explain why the narrowing is sound. [docs/correctness.md](docs/correctness.md)
is the reference for what the existing guarantees are.

## Pull requests

Describe what changed and why. Link an issue if there is one. Keep unrelated
refactors separate — they make a correctness review harder than it needs to be.
