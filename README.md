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

## Install

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

◆ DOCTOR  0.3.0
  platform            linux / x86_64
  ...
tracing
  backend             linux-ptrace
  available           yes
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

The backend needs no privileges: the child places *itself* under observation
before `exec`, so it works under `kernel.yama.ptrace_scope = 1` and in ordinary
containers. Where a seccomp profile forbids `ptrace`, `arc doctor` says so and
names the reason, and Arc falls back rather than failing:

```console
tracing
  backend             linux-ptrace
  available           no
  reason              ptrace denied, most likely by a container seccomp profile
  fallback            snapshot
```

**A trace only counts as complete if it really was.** Any of these makes it
partial, and a partial trace never narrows:

- a syscall the backend does not model, including one a newer kernel added;
- a read of `/proc`, `/sys`, `/dev/urandom`, or another volatile path;
- a socket connected, bound, or sent on;
- a path that could not be resolved, or is not valid UTF-8;
- a process that could not be followed;
- the trace budget overflowing;
- the tracer itself failing.

This is not hypothetical. Debian's coreutils probe SELinux through `/sys`, so
`mv`, `ls` and `cp` produce partial traces on a stock system — Arc reports that
and stays conservative rather than pretending otherwise.

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

Measured in a `rust:1-slim` container on Linux x86-64, median of 7
(`scripts/bench.sh`):

| Workload | Direct | Arc cold | Arc miss | **Arc warm** |
| --- | --- | --- | --- | --- |
| Read one file | 2 ms | 52 ms | 38 ms | **20 ms** |
| Read 400 files | 6 ms | 269 ms | 249 ms | **19 ms** |
| `rustc`, 120 modules | 67 ms | 471 ms | 384 ms | **26 ms** |

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

Read the tracing numbers honestly: **ptrace tracing is expensive.** It stops the traced process
twice per syscall, so syscall-dense work slows by roughly 40× — the same
workload under `strace -f` takes 243 ms against Arc's 269 ms, so essentially all
of that cost is ptrace itself, not Arc's bookkeeping. On a compile, where the
work is CPU rather than syscalls, it is closer to 7×.

What makes the trade worth it is the last column. **A cache hit never starts the
tracer**: it fingerprints the learned dependency set and replays. That is why a
400-file workload costs 19 ms warm regardless of how expensive it was to learn.
`[trace] enabled = false` removes tracing entirely, at the cost of never learning
anything.

## Status

Working today: local execution caching; complete dependency tracing on Linux with
automatic input narrowing; a task graph inferred from observed output-to-input
relationships, with transitive affected analysis and bounded-parallel selective
execution; content-addressed storage with deduplication; output
capture and restore; execution-family identity; learned dependency sets with
explicit completeness and structured downgrade reasons; process-tree and write
observation on Windows; a verified remote cache with a reference server, so one
machine's result is another machine's hit; shared task knowledge, so a fresh CI
runner can prove work unnecessary without having run it once; remote execution
of cache misses on compatible workers, with a reference worker; branch-aware CI
selection with GitHub Actions support, fork-safe cache-write policy and job
summaries; the dependency graph; `arc affected` against Git; cache
statistics, LRU pruning, garbage collection, integrity verification; execution
history and inspection; JSON output for tooling; and safe concurrent use from
several terminals.

Not built, and deliberately not stubbed: read-capable tracing on macOS or
Windows (and therefore automatic narrowing there), hermetic worker environments,
container or VM management for workers, distributed worker scheduling, cloud
storage backends for the cache server, and any agent protocol. `arc doctor` reports capabilities honestly.

## License

MIT
