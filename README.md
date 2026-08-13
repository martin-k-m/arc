# Arc

**Never repeat work that is already done.**

Arc sits between you and the commands you run. It fingerprints everything that
could change a command's result, and when nothing has changed it replays the
previous result instead of running the command again.

```console
$ arc run npm test
... 18.4s ...

$ arc run npm test

◆  CACHE HIT   npm test
  restored in 84ms · saved 18.3s
```

Arc is language-agnostic. It caches `cargo test`, `npm test`, `pytest`,
`go test ./...`, `make`, or anything else that reads files and writes output.

## Install

```bash
cargo install --path crates/arc-cli
```

Arc builds with stable Rust and has no runtime dependencies. The binary is
self-contained; cache state lives in `~/.arc` (override with `ARC_HOME`).

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
failure. Piping and CI output stay plain text; colour appears only on a terminal
and is disabled by `NO_COLOR`.

## What Arc considers

An execution is only reused when **every** one of these is identical:

| Input | How |
| --- | --- |
| Command and arguments | Hashed with argument boundaries preserved |
| Working directory | Relative to the project root, so the repo can move |
| Project files | Content-hashed (BLAKE3), honouring `.gitignore` |
| Environment | A curated variable set, values hashed and never stored |
| Toolchain | Contents of the resolved executable, not its version string |
| Observed executables | Every process in the tree Arc could identify |
| Platform | OS and architecture |
| Arc schema version | Bumped whenever key semantics change |

Derived directories (`target/`, `node_modules/`, `.git/`, `dist/`, …) are not
inputs. Declared outputs are never inputs.

## Scoping a command

By default a change anywhere in the project invalidates every cached execution.
That is safe but blunt: editing `README.md` should not rerun your tests. Tell Arc
what a command actually depends on:

```toml
[[command]]
match = "cargo test*"
inputs = ["src/**", "tests/**", "Cargo.toml", "Cargo.lock"]
```

```console
$ arc run cargo test
... 24.1s ...

$ vim README.md

$ arc run --explain cargo test

◆ EXPLAIN
  result             cache hit
  reason             all inputs match execution 4239b2

  outside this execution's inputs
    README.md

◆  CACHE HIT   cargo test
  restored in 41ms · saved 24.0s
```

Scoping is also what makes `arc affected` able to say *unaffected*. Without it,
Arc reports `unknown` — it will not claim a file is irrelevant when it has no
grounds to.

## Tracing

`arc run --trace` observes an execution and reports what it saw:

```console
$ arc run --trace cargo test

◆ TRACE
  command            cargo test
  backend            snapshot+jobobject
  processes          38 observed
  files written      412 observed
  executables        6 known
  dependency model   partial
  not observed       file reads, directory reads, existence checks
```

Read that last line literally. On Windows there is no non-privileged,
non-injecting way to observe file reads, so Arc's backend does not claim to. What
it *can* do is real: a portable snapshot diff sees every create, write and delete
in the project, and on Windows a job object sees every descendant process,
however deeply nested — which is how `cargo test` resolves to `cargo`, `rustc`, a
linker and the test binaries.

Observed executables are hashed into the cache key, which is strictly stronger
than v0.1. Observed writes are recorded as outputs, which is the fastest way to
find out what belongs in `[outputs]`.

Arc does **not** narrow the input set from a trace, and will not until a backend
can observe reads, directory enumeration and existence checks. `arc doctor`
reports exactly what the current platform supports.

## Configuration

Arc works with zero configuration. When you need more, put `arc.toml` at the
project root — its location also defines the project root.

```toml
[cache]
enabled = true
max_size = "20GB"
cache_failures = false     # cache non-zero exits too

[trace]
enabled = true             # costs one metadata walk per run

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
[docs/correctness.md](docs/correctness.md) for the invariants, the dependency
model, the race limits Arc cannot close, and the adversarial tests that enforce
the rest. [docs/architecture.md](docs/architecture.md) covers module boundaries
and the storage schema.

Notably, Arc does **not** cache when:

- the command exited non-zero (unless you ask with `--cache-failures`);
- it was killed by a signal;
- output exceeded the capture limit;
- any stored object it would need is missing or fails verification;
- its metadata database is unreadable or written by an incompatible version.

## Performance

Measured on Windows 11, x86_64, against a 2.06 s command in this repository
(median of 7 runs):

| | Wall clock |
| --- | --- |
| Direct command | 2060 ms |
| Arc, cold, tracing on | 2191 ms |
| Arc, learned miss | 2159 ms |
| **Arc, warm hit** | **49 ms** |

Arc's overhead on a miss is ~130 ms, most of it the two metadata walks that
tracing needs; `[trace] enabled = false` removes them. Fingerprinting this
repository takes 9 ms warm.

## Status

Working today: local execution caching, content-addressed storage with
deduplication, output capture and restore, execution-family identity, learned
dependency sets with explicit completeness states, process-tree and write
observation, the dependency graph, `arc affected` against Git, cache statistics,
LRU pruning, garbage collection, integrity verification, execution history and
inspection, JSON output for tooling and agents, and safe concurrent use from
several terminals.

Not built, and deliberately not stubbed: read-capable tracing on any platform
(and therefore automatic input narrowing), remote caching, distributed execution,
and any agent protocol. `arc doctor` reports capabilities honestly.

## License

MIT
