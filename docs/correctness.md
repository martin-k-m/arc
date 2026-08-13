# Cache correctness

Arc's value depends entirely on never serving a wrong result. False misses cost
seconds; a single false hit costs trust. Every design choice below resolves in
favour of executing.

## Invariants

**I1 — Reuse requires equivalence.** Two executions share a result only when the
command, arguments, relative working directory, project input digest,
environment digest, toolchain digest, declared output globs, platform, and Arc
schema version are all identical. `crates/arc-core/src/key.rs`.

**I2 — Objects are atomic.** A blob only appears at its content-addressed path
with complete contents: writes go to a temporary file, are flushed, then renamed.
A partially written object can never be observed as valid.
`crates/arc-core/src/store.rs`.

**I3 — Restoration cannot escape the project.** Recorded paths are rejected if
absolute, containing `..`, or passing through a symlinked parent. All
destinations are resolved before any file is written, so an unsafe entry aborts
the restore rather than half-applying it. `crates/arc-core/src/outputs.rs`.

**I4 — Unknown state executes.** A missing object, a failed hash check, a
corrupt record, or any error during replay drops the cache entry and runs the
command. `try_replay` in `crates/arc-core/src/engine.rs`.

**I5 — Secrets are not persisted.** Environment values are only ever stored as
hashes. Names matching known credential markers are additionally redacted from
all output. `crates/arc-core/src/key.rs`.

**I6 — Metadata optimisations never affect correctness.** The size/mtime
fingerprint cache is an optimisation only: a stale or missing entry costs a
re-hash. Files modified within 2 s are always re-hashed, since a write inside the
filesystem's timestamp granularity could otherwise be invisible.

## What is deliberately not cached

- Non-zero exits, unless `--cache-failures` or `[cache] cache_failures` is set.
  A failure is far more often environment-dependent than a success.
- Executions killed by a signal: the result says nothing about the inputs.
- Output beyond `MAX_CAPTURE` (64 MB), which cannot be faithfully replayed.
- Anything run with `--no-capture`, where output went straight to the terminal.

## Known limits

These are honest gaps, not bugs to be papered over:

- **Inputs are project-wide.** Without filesystem tracing Arc cannot know that
  `cargo test -p foo` ignores `docs/`. Any change to a tracked file is a miss.
  `[inputs] include` narrows this when you know better than Arc does.
- **Undeclared outputs are not restored.** If a command writes files you have not
  declared in `[outputs]`, a hit will not recreate them. Declare them, or do not
  cache that command.
- **Non-hermetic commands.** A command reading the network, the clock, or a
  database can produce a different result from identical inputs. Arc caches what
  it can observe; it cannot make a non-deterministic command deterministic.
- **`.gitignore` is trusted.** Ignored files are not inputs. A build that depends
  on a gitignored file needs it added via `[inputs] include`.

## Adversarial tests

`crates/arc-cli/tests/cli.rs` covers, end to end against the real binary:

- a replayed hit is byte-identical to the original output;
- a changed input forces a miss and `--explain` names the file;
- exit codes survive, and failures are not cached by default;
- deleting stored objects degrades to a miss rather than an error;
- corrupting an object is caught by `cache verify`, quarantined, and its entries
  dropped;
- six concurrent `arc run` processes leave the cache verifiably intact;
- environment values never reach the database or any output;
- Arc's own cache directory is never an input, even when placed inside the
  project.

Unit tests cover hash framing, path traversal refusal, atomic restore, glob
narrowing, size parsing, and fingerprint encoding including truncated data.
