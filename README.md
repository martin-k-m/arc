# Arc

**Never repeat work that is already done.**

Arc sits between you and the commands you run. It watches what a command
actually reads, and when none of that has changed it replays the previous result
instead of running the command again.

```console
$ arc run --trace ./build.sh
one
plugin plugins/a.plugin

◆ TRACE COMPLETE
  command             sh -c ./build.sh
  backend             linux-ptrace
  processes           2
  files read          5
  directories         1
  existence checks    3
  executables         1
  dependency model    complete
  next run narrows    yes

$ echo "unrelated" >> docs/design.md

$ arc run ./build.sh
one
plugin plugins/a.plugin

◆  CACHE HIT   sh -c ./build.sh
  restored in 0ms · saved 12ms
```

Arc is language-agnostic. It caches `cargo test`, `npm test`, `pytest`,
`go test ./...`, `make`, or anything else that reads files and writes output.

On real projects, not that toy one:

| Workload | Direct | **Arc, cache hit** | |
| --- | --- | --- | --- |
| `pytest` on [click](https://github.com/pallets/click), 1957 tests | 3.6 s | **35 ms** | 104× |
| `make` on [tinycc](https://github.com/TinyCC/tinycc), from clean | 5.8 s | **41 ms** | 141× |
| `cargo test` on [serde_json](https://github.com/serde-rs/json) | 6.2 s | **112 ms** | 55× |

And the number that decides whether any of that matters: replaying **60
commits of click's real history** with three per-file test tasks and no
configuration at all, **23.3% of runs hit** — roughly one in four did no
work, because Arc had learned that the commit changed nothing that task
reads.

Learning is not free, and a hit is not free either. The medians above are
from a four-core container that was not idle, the maxima are two to five
times worse, and a first run costs three to five times the command itself.
[docs/BENCHMARKS.md](docs/BENCHMARKS.md) has the machine, the method, the
spread, and the two rows that came out wrong.

## Install

Linux and macOS:

```bash
curl -fsSLO https://raw.githubusercontent.com/martin-k-m/arc/main/scripts/install.sh
sh install.sh
```

Windows:

```powershell
irm https://raw.githubusercontent.com/martin-k-m/arc/main/scripts/install.ps1 -OutFile install.ps1
.\install.ps1
```

Both download the release archive for your platform, **verify its SHA-256
against the published checksum file**, and install into a user directory —
no root, no administrator. Read the script before running it; neither needs to
be piped into a shell.

You can also take an archive directly from the
[releases page](https://github.com/martin-k-m/arc/releases), or build from
source:

```bash
cargo install --path crates/arc-cli
```

Arc builds with stable Rust and has no runtime dependencies — no C toolchain, no
kernel module, no daemon, no root. The binary is self-contained; cache state
lives in `~/.arc` (override with `ARC_HOME`).

## Use

```bash
arc run cargo test              # cache a test run
arc run -- cargo test -- --nocapture   # everything after -- goes to the child
arc run --trace cargo test      # run and report what Arc observed
arc run --explain pytest        # why did Arc rerun this?
arc graph                       # the task graph Arc has learned
arc affected                    # which tasks do my changes reach?
arc affected --run              # run exactly those, in order, in parallel
arc history                     # what has Arc executed
arc inspect a82f1e              # everything recorded about one execution
arc cache stats                 # size, reuse, time saved
arc cache prune                 # evict least-recently-used entries
arc cache verify                # re-hash every object, quarantine corruption
arc doctor                      # what works on this machine
```

Arc preserves the child's exit code, streams its output live, and never hides a
failure. Piping and CI output stay plain text; colour and motion appear only on a
terminal, and are disabled by `NO_COLOR` and `ARC_NO_ANIM`.

## How Arc decides

On Linux, the first run of a command is **observed**. Arc traces every syscall of
every process in the tree and records what the execution genuinely depended on:
files it could read, directories it listed, paths it looked for and did not find,
and the binaries it ran. From then on, only those things are fingerprinted.

```console
$ arc graph

◆ DEPENDENCY GRAPH
/tmp/arc-demo/repo

sh -c ./build.sh  5 runs · linux-ptrace · complete
  Inputs
  ├── build.sh
  ├── input.txt
  ├── optional.cfg
  ├── /etc/ld.so.cache
  ├── /lib/x86_64-linux-gnu/libc.so.6
  └── /usr/lib/x86_64-linux-gnu/libc.so.6
  Directories
  └── plugins
  Existence checks
  ├── /etc/ld.so.preload (absent)
  └── /tmp/arc-demo/repo (present)
  Executables
  └── /usr/bin/dash
```

Everything in that list invalidates the cache when it changes. Everything not in
it does not. That includes cases a naive file-level tracer gets wrong:

- `optional.cfg` was **absent** on the first run and the script branched on it.
  Creating it is a miss.
- `plugins/` was **enumerated**. Adding a file to it is a miss, even a file that
  did not exist when the trace ran.
- `libc.so.6` is outside the project and is hashed anyway. A libc upgrade is a
  miss.
- A file the script *creates* and then reads back is not an input, so it does not
  demand its own output as a precondition on the next run.

Where reads cannot be observed — Windows, macOS — Arc falls back to hashing the
whole project, which is what v0.1 and v0.2 did. `arc doctor` tells you which.

| Input | How |
| --- | --- |
| Command and arguments | Hashed with argument boundaries preserved |
| Working directory | Relative to the project root, so the repo can move |
| Files | Content-hashed (BLAKE3): the observed set, or the whole project |
| Directories | The set of entry names and types |
| Absent paths | Their continued absence |
| Environment | A curated variable set, values hashed and never stored |
| Toolchain and observed binaries | Contents of the executables, not version strings |
| Platform | OS and architecture |
| Arc schema version | Bumped whenever key semantics change |

Once several commands are known, those same facts become a graph: see
[the task graph](#the-task-graph).

## Tracing

```console
$ arc doctor
...
tracing

  preferred           linux-seccomp
  available           yes
  linux-seccomp       available
  linux-ptrace        available
  snapshot            available
  file reads          supported
  file writes         supported
  directory reads     supported
  existence checks    supported
  process tree        supported
  executables         supported
  outside project     supported
  network detection   supported
  best completeness   complete
  automatic narrowing supported
```

Neither Linux backend needs privileges — no `sudo`, no capability, no daemon, no
kernel module. The child places *itself* under observation before `exec`, so
tracing works under `kernel.yama.ptrace_scope = 1` and in ordinary containers.

Arc prefers `linux-seccomp`, which uses seccomp user notification and is three
to six times cheaper than ptrace on the syscall-heavy synthetic workloads in
`scripts/trace-bench.sh`. On real work the advantage is smaller, because real
work spends most of its time somewhere other than in syscalls: on click's
pytest suite the measured ratio is 1.8×, and on tinycc's build the sample was
too noisy to establish an ordering at all. See
[docs/BENCHMARKS.md](docs/BENCHMARKS.md). Where a sandbox forbids
one of them — Docker's default profile denies the `seccomp` syscall — `arc
doctor` names the reason and Arc falls back rather than failing:

```console
tracing

  preferred           linux-ptrace
  available           yes
  linux-seccomp       unavailable: seccomp denied, most likely by a container profile
  linux-ptrace        available
  snapshot            available
```

Availability is proved rather than assumed: Arc forks a child, installs a
one-syscall filter, receives a real notification and checks the answer took
effect. [docs/tracing.md](docs/tracing.md) covers both backends, how they are
selected, and the differential suite that holds them to the same meaning.

**A trace only counts as complete if it really was.** Any of these makes it
partial, and a partial trace never narrows:

- a syscall the backend does not model, including one a newer kernel added;
- a read of `/proc`, `/sys`, `/dev/urandom`, or another volatile path;
- a socket connected, bound, or sent on;
- a path that could not be resolved, or is not valid UTF-8;
- a process that could not be followed;
- the trace budget overflowing;
- the tracer itself failing.

This is not hypothetical, and it is not rare. Debian's coreutils probe SELinux
through `/sys`, so `mv`, `ls` and `cp` produce partial traces on a stock
system — Arc reports that and stays conservative rather than pretending
otherwise.

**How often that happens decides whether narrowing works for you, so here is
the measurement rather than the claim.** Of the three real workloads in
[docs/BENCHMARKS.md](docs/BENCHMARKS.md), none traces completely: `cargo test`
reads `/sys/fs/cgroup/cpu.max` and `/dev/urandom`, `pytest` over the whole
suite reads `/sys/fs/selinux` and `/proc/mounts`, and `make` reads
`/dev/urandom` through GCC. All three still cache and still hit — the warm
numbers are real — but they hit by hashing the project, not by narrowing.

Granularity is what changes the answer. A single pytest *file* does trace
completely, and in a 180-run replay of click's history every trace was
complete and 177 narrowed. If narrowing matters to you, the lever is smaller
tasks. [LIMITATIONS.md](LIMITATIONS.md) is the full list of what revokes
completeness and what Arc does not see at all.

Complete also does not mean *deterministic*. The clock, `getrandom` and the
scheduler are outside any filesystem tracer's reach; see
[docs/correctness.md](docs/correctness.md) for the exact contract.

## The task graph

Arc watches what each command reads and writes, so it also learns how commands
feed each other. One task's output being another's input *is* the edge; nothing
has to be declared.

```console
$ arc run ./generate-schema.sh    # writes generated/schema.json
$ arc run ./generate-client.sh    # reads it, writes generated/client.ts
$ arc run ./test-api.sh           # reads that
$ arc run ./test-web.sh           # reads src/web.rs

$ arc graph

◆ TASK GRAPH
/work/repo

generate-schema
└── generate-client
    └── test-api
test-web

  4 tasks · 2 edges · 4 complete · 0 partial
```

Change something, and Arc propagates:

```console
$ vim schema/api.yaml

$ arc affected

◆ AFFECTED
  changed
    modified   schema/api.yaml

  affected
    generate-client
    generate-schema
    test-api

  unaffected
    test-web
```

`--explain` shows the chain it followed:

```console
$ arc affected --explain
...
  affected
    test-api
      because generate-client is affected, via generated/client.ts
        because generate-schema is affected, via generated/schema.json
          because schema/api.yaml changed
```

Then run exactly that subgraph, producers before consumers, independent tasks
concurrently:

```console
$ arc affected --run --dry-run

◆ EXECUTION PLAN
  step 1
    generate-schema
  step 2
    generate-client
  step 3
    test-api

  3 of 4 tasks would run; 1 skipped

$ arc affected --run

◆ RUNNING
  3 of 4 tasks · 8 at a time

       RAN generate-schema                          41ms
       RAN generate-client                          37ms
       RAN test-api                                 60ms

◆ 3 ran  140ms
```

Every task still goes through the ordinary cache, so **affected does not mean
executed**: a task whose exact state is already stored reports `HIT` and restores
instead, and its consumers carry on from the restored files.

`--jobs N` bounds concurrency (default: available parallelism, capped at 16);
`--jobs 1` is serial and deterministic. A task whose prerequisite failed is
reported `BLOCKED` and never starts; independent branches continue unless you
pass `--fail-fast`. `arc affected --run` exits non-zero if anything failed.

What Arc will not do is guess. Two tasks writing the same path is reported as an
ambiguity rather than resolved; a cycle is reported and scheduled as one serial
group rather than pretended away; and a task whose dependencies were never fully
observed is `unknown`, which **runs**, because unknown is not unaffected.

`arc graph --task <name>`, `arc graph --affected` and `--json` narrow or export
it. `[[command]] name` gives a task a readable label, and `after = ["build"]`
adds an edge no filesystem observation could reveal.

## Scoping a command

Telling Arc what a command depends on still works, and is the only way to narrow
on platforms without a complete backend:

```toml
[[command]]
match = "cargo test*"
inputs = ["src/**", "tests/**", "Cargo.toml", "Cargo.lock"]
```

Declared inputs are fingerprinted **in addition** to anything observed, even when
the trace is complete. An explicit include is a statement of fact; a trace that
did not happen to read that file this time is not grounds to overrule you.

## Configuration

Arc works with zero configuration. When you need more, put `arc.toml` at the
project root — its location also defines the project root.

```toml
[cache]
enabled = true
max_size = "20GB"
cache_failures = false     # cache non-zero exits too

[trace]
enabled = true             # observe executions to learn their dependencies
backend = "auto"           # auto, fast, ptrace, snapshot or off

[inputs]
include = ["src/**", "Cargo.toml", "Cargo.lock"]   # empty = whole project
exclude = ["docs/**"]

[outputs]
include = ["target/release/mybin"]   # captured on a miss, restored on a hit

[env]
include = ["MY_FEATURE_FLAG"]
exclude = ["TERM"]

[[command]]
match = "cargo test*"
inputs = ["src/**", "tests/**"]
```

`arc config show` prints the effective configuration and where it came from.
`--trace-backend snapshot|off` and `ARC_TRACE_BACKEND` pin the backend, which is
useful for comparing behaviour or for a CI job that would rather not pay for
tracing.

## Caching build artifacts

By default Arc caches stdout, stderr, and the exit code — exactly right for test
runs. To reuse build products, declare them:

```toml
[outputs]
include = ["target/release/**"]
```

On a hit those files are restored from the content-addressed store. Restoration
refuses any path that would land outside the project, including through `..`, an
absolute path, or a symlinked parent directory, and aborts before writing
anything if any entry is unsafe.

## Sharing a cache between machines

A result produced on one machine can be replayed on another. Start the reference
server anywhere both can reach:

```bash
arc-cache serve --listen 127.0.0.1:7890 --data ./arc-cache-data
```

Point the project at it:

```toml
[remote]
url = "http://127.0.0.1:7890"
namespace = "demo"
```

Then the second machine does not repeat the first machine's work:

```console
$ ARC_HOME=/tmp/a arc run sh -c 'sleep 1 && build'
built

$ ARC_HOME=/tmp/b arc run sh -c 'sleep 1 && build'
✦ REMOTE HIT  sh -c sleep 1 && build
  restored in 4ms · saved 1.0s · 1 file (20 B)
  fetched 20 B from 127.0.0.1:7890 in 3ms

$ ARC_HOME=/tmp/b arc run sh -c 'sleep 1 && build'
✦ CACHE HIT  sh -c sleep 1 && build
  restored in 2ms · saved 1.0s · 1 file (20 B)
```

The third run makes **no network requests at all**: a remote hit is promoted
into the local cache, and a local hit never opens a connection.

```console
$ arc remote status
  configured    ✓
  endpoint      127.0.0.1:7890
  namespace     demo
  read          enabled
  write         enabled
  auth          none
  reachable     ✓
  protocol      v1
  server        arc-cache 0.5.0
```

`arc affected --run` benefits automatically: every scheduled task goes through
the same engine, so tasks another machine has already built come back as hits.

Full options:

```toml
[remote]
enabled = true
url = "https://cache.example.com"
namespace = "my-project"
token_env = "ARC_CACHE_TOKEN"   # the variable's *name*, never a token
read = true                     # untrusted CI: read = true, write = false
write = true
```

`--no-remote` skips the remote for one run. `ARC_REMOTE_URL`,
`ARC_REMOTE_NAMESPACE`, `ARC_REMOTE_READ`, `ARC_REMOTE_WRITE` and
`ARC_REMOTE_ENABLED` override the file, which is usually how CI configures it.

**A remote cache is an optimisation and is treated as untrusted.** Every
downloaded object is re-hashed before it is admitted; every record is validated
before it is read; every restored path goes through the same safety checks as a
local one. A corrupt object, a missing object, a malformed record, an expired
credential or an unreachable server all mean the same thing: run the command.
See [docs/remote-protocol.md](docs/remote-protocol.md) for the wire format.

## Running work on another machine

Arc can execute a cache miss on a remote worker. The result comes back as an
ordinary cache record, so the next machine to want it gets a plain remote hit
and nothing runs at all.

```bash
# terminal 1 — the shared cache
arc-cache serve --listen 127.0.0.1:7920 --data ./cache-data

# terminal 2 — a worker
arc-worker serve --listen 127.0.0.1:7921 --data ./worker-data \
  --cache-url http://127.0.0.1:7920 --max-jobs 4
```

```toml
[remote]
url = "http://127.0.0.1:7920"
namespace = "my-project"

[remote.execution]
enabled = true
url = "http://127.0.0.1:7921"
```

```text
◆ REMOTE EXEC  sh -c "make build"
  ran in 2.1s · queued 0ms · 812 KB in, 4.1 MB out · 127.0.0.1:7921
```

**Remote execution is off unless you turn it on**, and even then Arc only sends
a command it can send honestly. It refuses — and runs the command locally — when
the worker is a different platform, when an argument names a path on your
machine, when a required variable looks like a secret, when a complete trace saw
the command read something outside the project, or when any executable it needs
is not byte-identical on the worker. Executables are matched by content, never
by a version string.

A command with an [environment](#giving-a-command-an-environment) is not held to
that last rule: the worker does not need the toolchain, because the environment
*is* the toolchain. It needs only a host that can run it.

What crosses the wire is exactly what Arc fingerprinted, referenced by digest,
fetched by the worker from the shared cache. A second execution over the same
inputs transfers nothing.

Nothing the worker says is believed. Every object is re-hashed on the way into
your store and restored through the same path a local cache hit uses.

Eight machines wanting the same miss cause one execution: submission is
idempotent on the execution key, and the rest wait for the first.

> Remote execution runs your repository's commands on another machine. The
> reference worker isolates the filesystem, the environment and the process
> tree, but it is not a defence against hostile code and does not isolate the
> network. Run workers you would trust with the repositories that can reach
> them.

[docs/remote-execution.md](docs/remote-execution.md) covers the protocol,
eligibility, the sandbox, and the failure modes.
`scripts/remote-exec-demo.sh` runs all of it locally.

## Giving a command an environment

A cache hit means "the same inputs". Without help, "the same toolchain" means
"the machine happened to have the same compiler installed". An Arc environment
replaces that coincidence with content.

```toml
[environment.rust]
tools = ["cargo", "rustc"]
env = { RUST_BACKTRACE = "1" }

[[environment.rust.tree]]
from = "~/.rustup/toolchains/stable-x86_64-unknown-linux-gnu"
to = "rust"
exclude = ["**/doc/**"]

[[command]]
name = "test"
command = "cargo"
args = ["test"]
environment = "rust"
```

```bash
arc env capture rust          # captures the bytes, pins the id in arc-env.lock
arc run cargo test
```

```text
◆ ENVIRONMENT  rust · 8ac31f04b2d7
  hermetic: nothing outside the environment
```

The environment's id is the digest of a manifest that names every file by
content, so it does not depend on the alias, the hostname, or the path it was
captured from. That id enters the execution key: a result built under one
environment is never served to a run under another.

A command with an environment resolves its program **inside** it, gets a `PATH`
built only from it, and gets `HOME`, `TMPDIR` and the `XDG_*` directories pointed
somewhere Arc owns. If the environment does not provide a tool, the run fails —
there is no silent fallback to the host's copy, because that would put the
machine back into the answer.

Commit `arc-env.lock` and a worker with no Rust installed can run your Rust
build: it fetches the environment from the shared cache by digest, verifies every
object, materialises it once, and reuses it for every job afterwards.

```text
worker host:            environment 8ac31f04b2d7:
  no cargo, no rustc      cargo, rustc, the toolchain's own libraries
  glibc, a loader   ───▶  + a deterministic PATH
                          = the build runs
```

**Arc packages userspace, not kernels and not the C library.** A library that
resolves to a system directory is recorded as a host requirement by soname; one
that belongs to the toolchain is captured. So an environment needs a host with
the same OS, architecture and libc flavour, a dynamic loader where the captured
binaries expect one, and the recorded system libraries present. All of that is
checked, and a worker that cannot satisfy it sends the work back rather than
running something else.

Using an environment does not by itself make an execution hermetic, and Arc does
not pretend otherwise. Under complete tracing it reports per run whether the
command stayed inside the environment (`hermetic`), read host state it does not
supply (`host-dependent`, with the paths), or was not fully observed (`unknown`).

A real Rust toolchain, in a `rust:1` container (`scripts/env-bench.sh`, medians
of 5; 605 MB, 135 files):

| | |
| --- | --- |
| Capture | 1.6 s |
| Capture and publish to the shared cache | 4.0 s |
| Remote execution, cold worker (fetches all 605 MB) | 2.0 s |
| **Remote execution, warm worker** | **288 ms** |
| Remote cache hit | 0 ms |
| Ten tasks, one cold environment | 24.8 s, one materialisation |

The cold number is the price of portability and is paid once per worker per
environment. The warm number is the one that decides whether it is worth it.

[docs/environments.md](docs/environments.md) covers capture, the runtime closure
model, the compatibility contract, worker materialisation, and the limits.
`scripts/env-demo.sh` runs the whole thing locally, including a worker with no
Rust on its PATH.

## Continuous integration

`arc ci` is the CI entry point. It works out what a branch changed, runs only
the work that change requires, reuses everything the local and remote caches
already hold, and reports why each task ran or did not.

Declare what CI should run:

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

[ci]
tasks = ["test-core", "lint"]
```

Then, in GitHub Actions:

```yaml
- uses: actions/checkout@v4
  with:
    fetch-depth: 0        # Arc compares two commits; give it the history

- run: arc ci
  env:
    ARC_CACHE_TOKEN: ${{ secrets.ARC_CACHE_TOKEN }}
```

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

Arc reads GitHub's event payload directly, so pull requests, pushes and merge
queues each get the right base — no API token, no network. Anything it cannot
resolve, including a shallow clone missing the base commit, means it cannot
prove anything and every declared task runs. It never quietly calls work
unnecessary.

**Fork pull requests never publish to the shared cache.** Arc's default policy
publishes only from events it can positively identify as trusted, and enforces
that on every task it schedules. Do not hand a cache-write token to a fork's
pull request; [docs/ci.md](docs/ci.md#fork-pull-requests) has the safe pattern.

A fresh runner has no task graph of its own, so Arc publishes each task's
learned dependencies alongside its results and fetches them in one batched
request. That knowledge is used to *select* work, never to authorise a cache
hit: a compromised cache server can make Arc run more work, never different
work and never less.

Reproduce any CI decision locally:

```bash
arc ci --base origin/main --head HEAD --dry-run --explain
```

[docs/ci.md](docs/ci.md) covers events, trust policy, shallow clones, job
summaries and JSON output.

## Correctness

A fast wrong answer is worthless, so Arc executes whenever it is unsure. See
[docs/correctness.md](docs/correctness.md) for the invariants, exactly what
"complete" does and does not mean, the temporal rules that separate an
intermediate from an input, the race limits Arc cannot close, and the adversarial
tests that enforce the rest. [docs/architecture.md](docs/architecture.md) covers
module boundaries, the tracing design, and the storage schema.

Notably, Arc does **not** cache when:

- the command exited non-zero (unless you ask with `--cache-failures`);
- it was killed by a signal;
- output exceeded the capture limit;
- any stored object it would need is missing or fails verification;
- its metadata database is unreadable or written by an incompatible version.

## Performance

**[docs/BENCHMARKS.md](docs/BENCHMARKS.md) is the authoritative version of
this section**: it names the machine, the method, the repetition count, the
spread, and the measurements that came out wrong. What follows is the
summary.

Three real projects, pinned at named commits (`bench/real-workloads.sh`), in
a `rust:1-bookworm` container on a four-core WSL2 VM that was not idle:

| Workload | Direct | Arc cold | Arc miss | **Arc warm** |
| --- | --- | --- | --- | --- |
| `pytest` (click, 1957 tests), median of 7 | 3,639 ms | 17,636 ms | 5,916 ms | **35 ms** |
| `make -j1` (tinycc, from clean), median of 7 | 5,773 ms | 8,844 ms | 8,139 ms | **41 ms** |
| `cargo test` (serde_json), median of 3 | 6,188 ms | 17,635 ms | 31,880 ms | **112 ms** |

Cache on disk after settling: 4.3 MB, 5.7 MB and 6.4 MB respectively — small
because the default captures stdout, stderr and the exit code rather than
build products.

The hit rate, which is the number that decides whether the rest matters:
**23.3%** over 60 commits of click's real history, three per-file test tasks,
zero configuration, one run per commit and task
(`bench/hit-rate-click.sh`). All 180 traces were complete and 177 narrowed,
so every hit was decided by the learned dependency set rather than by hashing
the project.

The synthetic numbers below were measured on different hardware than the
tables above and are kept for their shape rather than their absolute values.

Arc has to stay usable as a repository gets large. `scripts/scale-bench.sh`,
median of 3 on Linux x86-64, at 1k / 10k / 100k files:

| | 1k | 10k | 100k | peak RSS at 100k |
| --- | --- | --- | --- | --- |
| Cold run (scan, trace, learn, store) | 63 ms | 101 ms | 590 ms | 68 MB |
| **Warm hit** | **18 ms** | **26 ms** | **133 ms** | 47 MB |
| Miss on an unrelated change | 17 ms | 29 ms | 131 ms | — |
| Conservative path (whole-project scan) | 29 ms | 79 ms | 595 ms | 75 MB |
| `arc affected` | 16 ms | 24 ms | 121 ms | — |
| `arc graph` / `history` / `cache stats` | 11 ms | 14 ms | 12 ms | — |

The column that matters is *cost per thousand files*, and it falls at every
step — 63 → 10.1 → 5.9 ms per 1k on the cold run — because roughly 12 ms of
each number is process start and database open, which does not scale with the
project. Ten times the files costs about 7.5× the time on the conservative
whole-project scan, the path every platform without a read-observing backend
takes on every run. Nothing here is quadratic, and memory is bounded at well
under a megabyte per thousand files.

Graph operations are not where the time goes (x86-64, release,
`cargo run -p arc-core --example graph_bench`):

| Graph | Build | Affected | Plan | Topological order |
| --- | --- | --- | --- | --- |
| 100 tasks, 261 edges | 0.2 ms | 0.1 ms | 0.1 ms | 0.0 ms |
| 1,000 tasks, 2,895 edges | 1.7 ms | 0.7 ms | 0.8 ms | 0.4 ms |
| 10,000 tasks, 29,691 edges | 26 ms | 13 ms | 14 ms | 6 ms |
| 10,000-deep chain | 10 ms | 6 ms | 11 ms | 4 ms |

Remote cache, reference server on loopback with injected round-trip latency
(`cargo run --release -p arc-cache --example remote_bench`):

| Payload | RTT | Wire | Upload | Lookup | Download | Second fetch | Requests |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1 object, 1 MB | 0 ms | 6.2 KB | 11 ms | 6 ms | 4 ms | 0 ms | 5 |
| 100 objects, 1.6 MB | 0 ms | 16.8 KB | 110 ms | 8 ms | 122 ms | 3 ms | 203 |
| 1,000 objects, 1.1 MB | 0 ms | 1.1 MB | 805 ms | 8 ms | 2,080 ms | 42 ms | 2,003 |
| 1 object, 1 MB | 50 ms | 6.2 KB | 107 ms | 50 ms | 57 ms | 0 ms | 5 |
| 100 objects, 1.6 MB | 50 ms | 16.8 KB | 756 ms | 51 ms | 706 ms | 4 ms | 203 |

Two things to read from this. **Missing-object negotiation is one round trip**,
not one per object, so the second fetch of the same set costs almost nothing at
any latency — that column is what a warm CI runner actually pays. And
**compression is decided per object**: source-like data above 4 KB shrinks by
~99%, while the 1 KB objects fall below the threshold and are sent raw, because
deflating them costs more than it saves.

What the numbers also show is that the protocol is still one request per
distinct object. A thousand tiny objects is 2,003 requests, and at 50 ms that
dominates everything else.

Graph work is `O(V + E)`: edge derivation joins a producer index rather than
comparing task pairs, traversal is breadth-first over a three-level lattice, and
ordering is Kahn's algorithm with a label-ordered ready set. Cycle detection is
iterative Tarjan, so a ten-thousand-deep chain does not touch the stack.

The scheduler's own overhead is small next to the work it schedules — twelve
independent 200 ms tasks, each a full `arc run` with its own cache lookup:

| `--jobs` | Wall clock |
| --- | --- |
| 1 | 2900 ms |
| 2 | 1458 ms |
| 4 | 809 ms |
| 8 | 572 ms |

Read the tracing numbers honestly: **learning dependencies is expensive.** How
expensive depends on the backend. `scripts/trace-bench.sh`, median of 5 on Linux
x86-64, where `snapshot` is Arc doing everything *except* watching syscalls:

| Workload | Direct | snapshot | ptrace | **seccomp** |
| --- | --- | --- | --- | --- |
| Read 300 files | 107 ms | 156 ms | 1316 ms | **484 ms** |
| 200 short-lived children | 69 ms | 127 ms | 740 ms | **228 ms** |
| 200 directory enumerations | 132 ms | 172 ms | 1524 ms | **509 ms** |
| Produce 200 outputs | 6 ms | 391 ms | 526 ms | **428 ms** |

Subtracting `snapshot` isolates what tracing itself costs: seccomp is 3.5×, 6.1×
and 4.0× cheaper than ptrace on the first three. On the fourth the time goes on
capturing 200 output files, which both backends pay equally — tracing is not the
expensive part of that workload.

What makes the trade worth it is the last column. **A cache hit never starts the
tracer**: it fingerprints the learned dependency set and replays. That is why a
400-file workload costs 19 ms warm regardless of how expensive it was to learn.
`[trace] enabled = false` removes tracing entirely, at the cost of never learning
anything.

## Status

Arc is 1.0. The command line, `arc.toml`, `--json` output, the remote protocols
and the stored formats are stable surfaces from here on — see
[CHANGELOG.md](CHANGELOG.md) for exactly what that covers and what it does not.

Working today: local execution caching; complete dependency tracing on Linux
through two rootless backends, with automatic input narrowing; a task graph inferred from observed output-to-input
relationships, with transitive affected analysis and bounded-parallel selective
execution; content-addressed storage with deduplication; output
capture and restore; execution-family identity; learned dependency sets with
explicit completeness and structured downgrade reasons; process-tree and write
observation on Windows; a verified remote cache with a reference server, so one
machine's result is another machine's hit; shared task knowledge, so a fresh CI
runner can prove work unnecessary without having run it once; remote execution
of cache misses on compatible workers, with a reference worker; content-addressed
execution environments that let a worker with no toolchain installed run the
build, with deterministic PATH, isolated HOME/TMP, and per-execution hermeticity
reporting; branch-aware CI
selection with GitHub Actions support, fork-safe cache-write policy and job
summaries; the dependency graph; `arc affected` against Git; cache
statistics, LRU pruning, garbage collection, integrity verification; execution
history and inspection; JSON output for tooling; and safe concurrent use from
several terminals.

Not built, and deliberately not stubbed: read-capable tracing on macOS or
Windows (and therefore automatic narrowing there), environment portability on
macOS or Windows (capture and materialisation work; runtime-closure discovery and
hermeticity reporting do not, so use host mode there), packaging of the kernel or
the C library, downloading toolchains from anywhere,
container or VM management for workers, distributed worker scheduling, cloud
storage backends for the cache server, and any agent protocol. `arc doctor` reports capabilities honestly.

### Platform support

| | tracing | automatic narrowing | cache | shared cache | remote execution | environments |
| --- | --- | --- | --- | --- | --- | --- |
| Linux x86-64 | seccomp / ptrace | yes | yes | yes | yes | yes |
| Linux aarch64 | seccomp / ptrace | yes | yes | yes | yes | yes |
| Windows x86-64 | snapshot + job object | no | yes | yes | yes | host mode |
| macOS x86-64 / arm64 | snapshot | no | yes | yes | yes | host mode |

Only what CI exercises is listed as supported. Where a platform cannot observe
reads, Arc does not narrow — it falls back to the conservative project scan,
which is slower and always correct.

## Documentation

| | |
| --- | --- |
| [BENCHMARKS.md](docs/BENCHMARKS.md) | every number on this page: the machine, the method, the scripts |
| [LIMITATIONS.md](LIMITATIONS.md) | where the dependency model stops being trustworthy |
| [BUGS.md](docs/BUGS.md) | defects that shipped, and what each one taught |
| [DECISIONS.md](docs/DECISIONS.md) | why Arc is built this way, and what the alternative cost |
| [correctness.md](docs/correctness.md) | what authorizes a cache hit, and what Arc does not guarantee |
| [architecture.md](docs/architecture.md) | how the pieces fit together |
| [tracing.md](docs/tracing.md) | the tracing backends, their limits and their cost |
| [environments.md](docs/environments.md) | content-addressed toolchains |
| [remote-execution.md](docs/remote-execution.md) | workers, eligibility, the sandbox |
| [remote-protocol.md](docs/remote-protocol.md) | the shared-cache wire format |
| [ci.md](docs/ci.md) | `arc ci`, trust policy, GitHub Actions |
| [security.md](docs/security.md) | threat model, what leaves your machine |
| [troubleshooting.md](docs/troubleshooting.md) | why did this miss, why is my trace partial |

[CHANGELOG.md](CHANGELOG.md) records what is stable and what compatibility
means. [CONTRIBUTING.md](CONTRIBUTING.md) covers building and testing.

## License

MIT
