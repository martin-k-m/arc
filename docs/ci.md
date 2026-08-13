# Arc in CI

`arc ci` answers one question: *given what this branch changed, what work
actually has to happen?* It resolves the comparison from the CI provider,
computes the affected tasks, reuses whatever the local and remote caches already
hold, executes the rest, and reports what it did.

```text
provider context ─▶ base/head ─▶ changed paths ─▶ affected + unknown
                                                          │
                                          local cache ◀───┤
                                         remote cache ◀───┤
                                              execute ◀───┘
                                                  │
                                                  ▼
                                             CI summary
```

## Declaring what CI runs

CI runs a declared set, not "every command anyone has ever run in this
repository". A `[[command]]` block becomes runnable when it has both a `name`
and a `command`:

```toml
[[command]]
name = "test-core"
command = "cargo"
args = ["test", "-p", "arc-core"]
inputs = ["crates/arc-core/**", "Cargo.toml", "Cargo.lock"]

[[command]]
name = "lint"
command = "cargo"
args = ["clippy", "--workspace", "--all-targets"]
tags = ["fast"]

[ci]
tasks = ["test-core", "lint"]
```

`[ci] tasks` selects which runnable commands CI considers. Omit it and every
runnable command is used. Naming a command that does not exist is a
configuration error, reported before anything runs — silently running less than
the repository asked for is the one failure CI must not have.

`--task` narrows a run further, by name or by tag, which is how you split work
across jobs:

```bash
arc ci --task lint
```

## GitHub Actions

```yaml
- uses: actions/checkout@v4
  with:
    # Arc compares two commits. Without enough history it cannot compute a
    # diff, and falls back to running every declared task.
    fetch-depth: 0

- run: arc ci
  env:
    ARC_CACHE_TOKEN: ${{ secrets.ARC_CACHE_TOKEN }}
```

Arc reads the event payload GitHub writes to `GITHUB_EVENT_PATH`, so it works
without an API token and without network access.

| Event | Base | Head |
|---|---|---|
| `pull_request`, `pull_request_target` | `pull_request.base.sha` | `pull_request.head.sha` |
| `merge_group` | `merge_group.base_sha` | `merge_group.head_sha` |
| `push` | `before`, else `HEAD~1` | `after` |
| `workflow_dispatch` | none — every task runs | — |

The pull-request case is deliberately *not* "the checked-out commit against its
parent". A workflow that checks out GitHub's synthetic merge commit would then
compare the merge against one of its parents, which is a different question from
"what does this pull request change".

Anything Arc cannot resolve — a missing base, a shallow clone, an unparseable
payload — produces no diff, and no diff means no proof, so every declared task
is selected. The reason is printed, added to the job summary, and emitted as a
workflow annotation.

## Fork pull requests

**Never expose a cache-write credential to a fork's pull request.** A fork PR
runs code the repository's maintainers have not reviewed.

Arc's default policy is `remote_write = "trusted"`: it publishes only from
events it can positively identify as trusted, and a fork pull request is not
one. `pull_request_target` is never trusted either, because Arc cannot see which
tree the workflow checked out.

```toml
[ci]
remote_write = "trusted"   # or "always", or "never"
```

A fork PR still runs, still reads the shared cache if the server allows
anonymous reads, and still uses the local cache. It just publishes nothing. The
safe workflow pattern is to not pass the secret at all on untrusted events:

```yaml
- run: arc ci
  env:
    ARC_CACHE_TOKEN: ${{ github.event.pull_request.head.repo.fork && '' || secrets.ARC_CACHE_TOKEN }}
```

`--remote-write` overrides the policy. It is named for its consequence rather
than for its permission, because that is the part worth thinking about.

## Shallow clones

```bash
arc ci --fetch
```

lets Arc deepen the repository to obtain the base commit. It is off by default:
Arc does not mutate a repository as a side effect of analysing one. Without it,
a shallow clone missing the base is reported and every task runs.

## Reproducing a CI decision locally

```bash
arc ci --base origin/main --head HEAD --dry-run --explain
```

`--base` with no `--head` compares against the **working tree**, including
uncommitted and untracked files — what a developer means. Supplying both
compares two commits, which is what CI means. `--dry-run` analyses and plans
without executing anything and without publishing anything.

Outside a recognised provider the provider reads `local`. `ARC_CI_BASE` and
`ARC_CI_HEAD` supply revisions for CI systems Arc has no specific support for.

## What it reports

```text
◆ ARC CI
  provider            github-actions · pull_request
  base                a1b2c3d4e5f6
  head                f6e5d4c3b2a1
  changed             8 files

  plan
    4 affected
    1 unknown
    23 unaffected

  cache
    1 local
    4 remote
    1 executed

  time
    arc        142ms
    work       1.8s
    saved      ~24.6s estimated
```

`saved` is an estimate from the median of each task's recent recorded durations.
A task Arc has never timed contributes nothing and is counted as unmeasured
rather than as zero, so a first CI run does not claim to have saved anything.

In GitHub Actions, Arc additionally writes a compact Markdown block to
`GITHUB_STEP_SUMMARY` and a few counts to `GITHUB_OUTPUT`
(`selected_count`, `skipped_count`, `affected_count`, `cache_hits`,
`executed_count`, `failed_count`). Both are conveniences: if either file is
missing or unwritable, the build carries on. `--no-summary` turns them off.

`--json` prints the analysis, the plan and the run summary as one document, with
no terminal styling.

## Exit codes

`0` when every selected task succeeded or was served from a cache. `1` when a
task failed or was blocked by one that did.

Analysis uncertainty is not failure. A missing base, an unreachable remote or a
task Arc knows nothing about all lead to *more* work, not to a failed build.

## Shared task knowledge

A brand-new CI runner has no local task graph, so on its own it could not prove
anything unaffected and would run everything. Arc publishes each task's learned
dependency knowledge to the remote cache alongside its results, and a fresh
machine fetches the knowledge for the tasks its own `arc.toml` declares — one
batched request.

That metadata is optimisation data, never authority. See
[correctness.md](correctness.md#shared-task-knowledge) for what Arc checks
before believing any of it, and why a compromised cache server can make Arc run
*more* work but never different work.

## Caching the Arc binary

Use `actions/cache` for the Arc binary if you like. Do not use it, or workflow
artifacts, as Arc's execution cache — Arc already has a content-addressed store
designed for sharing, and it verifies everything it receives. See
[remote-protocol.md](remote-protocol.md).
