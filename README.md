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
arc graph                       # what Arc has learned about this project
arc affected                    # which executions do my changes touch?
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

Read that honestly: **ptrace tracing is expensive.** It stops the traced process
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
automatic input narrowing; content-addressed storage with deduplication; output
capture and restore; execution-family identity; learned dependency sets with
explicit completeness and structured downgrade reasons; process-tree and write
observation on Windows; the dependency graph; `arc affected` against Git; cache
statistics, LRU pruning, garbage collection, integrity verification; execution
history and inspection; JSON output for tooling; and safe concurrent use from
several terminals.

Not built, and deliberately not stubbed: read-capable tracing on macOS or
Windows (and therefore automatic narrowing there), remote caching, distributed
execution, and any agent protocol. `arc doctor` reports capabilities honestly.

## License

MIT
