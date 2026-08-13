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
arc run --explain pytest        # why did Arc rerun this?
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
| Platform | OS and architecture |
| Arc schema version | Bumped whenever key semantics change |

Derived directories (`target/`, `node_modules/`, `.git/`, `dist/`, …) are not
inputs. Declared outputs are never inputs.

## Configuration

Arc works with zero configuration. When you need more, put `arc.toml` at the
project root — its location also defines the project root.

```toml
[cache]
enabled = true
max_size = "20GB"
cache_failures = false     # cache non-zero exits too

[inputs]
include = ["src/**", "Cargo.toml", "Cargo.lock"]   # empty = whole project
exclude = ["docs/**"]

[outputs]
include = ["target/release/mybin"]   # captured on a miss, restored on a hit

[env]
include = ["MY_FEATURE_FLAG"]
exclude = ["TERM"]
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
[docs/correctness.md](docs/correctness.md) for the invariants and the adversarial
tests that enforce them.

Notably, Arc does **not** cache when:

- the command exited non-zero (unless you ask with `--cache-failures`);
- it was killed by a signal;
- output exceeded the capture limit;
- any stored object it would need is missing or fails verification.

## Status

Working today: local execution caching, content-addressed storage with
deduplication, output capture and restore, cache statistics, LRU pruning,
garbage collection, integrity verification, execution history and inspection,
JSON output for tooling and agents, and safe concurrent use from several
terminals.

Not built yet, and deliberately not stubbed: filesystem tracing to narrow inputs
per command, dependency graphs and `arc affected`, remote caching, and
distributed execution. `arc doctor` reports capabilities honestly.

## License

MIT
