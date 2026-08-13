# Cache correctness

Arc's value depends entirely on never serving a wrong result. False misses cost
seconds; a single false hit costs trust. Every design choice below resolves in
favour of executing.

## Invariants

**I1 — Reuse requires equivalence.** Two executions share a result only when the
command, arguments, relative working directory, family identity, project input
digest, environment digest, toolchain digest, learned-dependency digest,
declared output globs, platform, and Arc schema version are all identical.
`crates/arc-core/src/key.rs`.

**I2 — Objects are atomic.** A blob only appears at its content-addressed path
with complete contents: writes go to a temporary file, are flushed, then renamed.
A partially written object can never be observed as valid.
`crates/arc-core/src/store.rs`.

**I3 — Restoration cannot escape the project.** Recorded paths are rejected if
absolute, containing `..`, or passing through a symlinked parent. All
destinations are resolved before any file is written, so an unsafe entry aborts
the restore rather than half-applying it. `crates/arc-core/src/outputs.rs`.

**I4 — Unknown state executes.** A missing object, a failed hash check, a
corrupt record, an unreadable metadata database, or any error during replay drops
the cache entry and runs the command. `try_replay` in
`crates/arc-core/src/engine.rs`.

**I5 — Secrets are not persisted.** Environment values are only ever stored as
hashes. Names matching known credential markers are additionally redacted from
all output. Dependency sets and trace records hold paths and counts, never
environment values. `crates/arc-core/src/key.rs`.

**I6 — Metadata optimisations never affect correctness.** The size/mtime
fingerprint cache is an optimisation only: a stale or missing entry costs a
re-hash. Files modified within 2 s are always re-hashed, since a write inside the
filesystem's timestamp granularity could otherwise be invisible.

**I7 — Learned knowledge may add to a key, never subtract from it.** Observed
dependencies can only make the execution key cover *more*. Narrowing the input
set requires `Completeness::Complete`, which no backend shipping today reports.
`crates/arc-core/src/dependency.rs`.

## Dependency learning

### Family identity vs execution state

`cargo test` in a given project is an **execution family**. The contents of
`src/lib.rs` are **state** within it. The family key deliberately excludes file
contents so that Arc can still find what it learned after a file changes. It is
never used to authorise a hit — only to locate knowledge. Full detail in
[architecture.md](architecture.md).

### Trace completeness

Every dependency set carries a completeness state:

| State | Meaning | May narrow inputs |
| --- | --- | --- |
| `Complete` | Every read, directory enumeration, existence check and descendant process was observed | yes |
| `Partial` | Real observations, but the backend cannot see everything | no |
| `Unsupported` | No backend available, or tracing disabled | no |
| `Invalid` | Stored data failed validation | no |

A run that lost events is `Partial` regardless of what the backend claims.

**No backend shipping in this version reports `Complete`.** Arc therefore never
narrows the input set from a trace. What tracing *does* contribute in v0.2:

- **Executable dependencies.** Observed process images are hashed into the
  execution key. This is strictly stronger than v0.1, which covered only the
  command Arc spawned itself. Missing an executable leaves the key exactly where
  v0.1 was, so an incomplete process tree cannot cause a false hit.
- **Observed outputs.** Files the execution created, modified or deleted are
  recorded and shown by `arc graph`, `arc inspect` and `arc run --trace`.

### Learned dependency invalidation

A stored dependency set is discarded — not repaired — when any of these differ
from the current run: the dependency schema version, the trace schema version,
the family key, the Arc version, or the backend's declared capabilities. Changing
`[inputs]`, `[outputs]`, `[env]`, `[trace]`, or any `[[command]]` block changes
the family key, which invalidates the set as a consequence.

Retracing costs one execution. Trusting a stale set costs correctness.

### Merging observations

Inputs, directories, absences and outputs are **unioned** across runs: an
execution may take a different branch next time, and forgetting a dependency
observed once is the unsafe direction. Executables are unioned by path with the
newest digest winning, so a toolchain upgrade is reflected rather than pinned.
Completeness is the weakest of everything merged, never the best run's.

A file observed as both read and written stays an input. Demoting it to an output
would be the unsafe direction.

### Where narrowing does come from

Explicit configuration, which is user knowledge rather than inference:

```toml
[[command]]
match = "cargo test*"
inputs = ["src/**", "tests/**", "Cargo.toml", "Cargo.lock"]
```

Declared inputs are **additive** with `[inputs] include` and are always
fingerprinted. Excludes are applied to the walk only; they can never remove a
path that a trace observed as a read. An explicit include is a statement of fact,
an exclude is a hint.

Narrowing is what lets Arc report a change as irrelevant. Without it, `arc
affected` reports `unknown` rather than `unaffected`, because "I have not
observed a dependency on this file" and "this file does not matter" are different
claims and only the second is safe to act on.

## Modelled but not observed

These concepts exist in the data model so that a future backend does not force a
schema migration. Every one of them is empty under the backends shipping today,
and each is a reason Arc refuses to narrow.

- **Negative / existence dependencies.** A program that does `if
  config.local.toml exists: load it` depended on the file's *absence*. Replaying
  across its appearance would be a false hit. Arc's conservative project scan
  catches this today because a new file changes the input digest; a narrowed
  input set would not, which is precisely why narrowing requires
  `existence_checks`.
- **Directory reads.** A program that enumerates `plugins/` depends on the set of
  entries, not only on the files it opened. File-open tracing alone does not
  prove a complete dependency set.
- **Environment reads.** Arc keeps its conservative configured variable set
  rather than guessing which variables were actually consulted.

## Paths, symlinks and reparse points

All path identity goes through `crates/arc-core/src/paths.rs`: one normalisation
(absolute, `\\?\` stripped, `/` separators, lexical `.`/`..` resolution) and one
comparison form (case-folded on Windows, case-sensitive elsewhere). Containment
compares whole segments, so `/repo-old` is never treated as inside `/repo`.

Symlinks are fingerprinted by their target path, not their target's contents, so
retargeting a link changes the input digest. Symlinks are not captured as
outputs, since restoring one would recreate a pointer Arc never validated.
Restoration refuses any destination whose ancestors include a symlink, which
covers Windows junctions and reparse points.

## Paths outside the project

Classified as `Project`, `ArcInternal`, `External` or `System`. Arc fingerprints
only `Project` paths plus explicitly resolved executables. It does not hash
system directories because a process touched a DLL, and it does not silently
treat an external read as irrelevant — external observations are recorded and
surfaced, and their existence is one reason completeness stays `Partial`.

`ArcInternal` is checked first, so Arc's own cache directory can never become one
of its own inputs even when placed inside the project.

## Races and their limits

Arc does not take a filesystem snapshot, so a genuinely atomic view of the
project does not exist. The exposures, stated plainly:

- **Fingerprint-then-execute.** A file modified between fingerprinting and the
  command reading it produces a result attributed to the earlier content. The 2 s
  mtime trust lag narrows the window for the *next* run's detection but does not
  close this one.
- **Execute-then-store.** A file modified while the command runs may be captured
  in either state.
- **Snapshot granularity.** A write landing inside the same timestamp tick as the
  post-execution snapshot is not seen as a write. A missed write means the file
  stays an ordinary input, which is the v0.1 behaviour, not a hit.
- **Job assignment window.** On Windows a descendant process created between
  `spawn` and `AssignProcessToJobObject` is outside the job. Sub-millisecond, not
  zero, and it sets the trace lossy.

None of these can be closed without filesystem snapshots or kernel interception.
They are documented rather than papered over.

## What is deliberately not cached

- Non-zero exits, unless `--cache-failures` or `[cache] cache_failures` is set.
- Executions killed by a signal: the result says nothing about the inputs.
- Output beyond `MAX_CAPTURE` (64 MB), which cannot be faithfully replayed.
- Anything run with `--no-capture`, where output went straight to the terminal.

## Known limits

- **Inputs are project-wide by default.** Without a read-capable backend Arc
  cannot know that `cargo test -p foo` ignores `docs/`. Scope it with
  `[[command]] inputs` when you know better than Arc does.
- **Undeclared outputs are not restored.** A hit will not recreate files you have
  not declared in `[outputs]`. `arc run --trace` shows what a command actually
  wrote, which is the fastest way to find out what to declare.
- **Non-hermetic commands.** A command reading the network, the clock, or a
  database can produce different results from identical inputs. Arc caches what
  it can observe; it cannot make a non-deterministic command deterministic.
- **`.gitignore` is trusted** for input discovery. A build that depends on a
  gitignored file needs it added via `[inputs] include`. The trace snapshot
  deliberately ignores `.gitignore`, since generated files are usually ignored
  and those are the writes worth seeing.

## Adversarial tests

`crates/arc-cli/tests/cli.rs` and `tests/dependency.rs` cover, end to end against
the real binary:

- a replayed hit is byte-identical to the original output;
- a changed input forces a miss and `--explain` names the file;
- family identity survives content changes, separates different arguments, and is
  invalidated by a scoping change;
- a scoped command ignores changes outside its inputs, and still misses on a
  change inside them;
- deleting a dependency, adding a file inside the scope, emptying a file, and
  replacing a file with a directory all force a miss;
- an unscoped command is invalidated by any project change;
- tracing never reports `complete` while reads are unobservable;
- observed writes become outputs, never inputs;
- Arc's cache directory is neither fingerprinted nor graphed, even inside the
  project;
- a corrupt metadata database executes rather than replaying, and is rebuilt;
- six concurrent traced runs leave families, dependency sets and objects intact;
- fake secrets injected via the environment appear in no output and in no file
  under `$ARC_HOME`, including the database and every stored object;
- Unicode, spaces and deep nesting survive a fingerprint round trip;
- `arc affected` maps Git changes onto scoped executions, and reports `unknown`
  rather than `unaffected` for families without narrowing;
- exit codes survive, failures are not cached by default, deleted objects degrade
  to a miss, and corrupt objects are quarantined by `cache verify`.

Unit tests cover hash framing, path normalisation and case rules, containment,
family identity, completeness gating, merge semantics, dependency validation,
Git porcelain parsing, path traversal refusal, atomic restore, glob narrowing,
size parsing, and fingerprint encoding including truncated data.
