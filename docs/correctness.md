# Cache correctness

Arc's value depends entirely on never serving a wrong result. False misses cost
seconds; a single false hit costs trust. Every design choice below resolves in
favour of executing.

## Invariants

**I1 — Reuse requires equivalence.** Two executions share a result only when the
command, arguments, relative working directory, family identity, input digest,
environment digest, toolchain digest, learned-dependency digest, declared output
globs, platform, and Arc schema version are all identical.
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
environment values, and a syscall tracer cannot see environment access at all
because it happens in memory. `crates/arc-core/src/key.rs`.

**I6 — Metadata optimisations never affect correctness.** The size/mtime
fingerprint cache is an optimisation only: a stale or missing entry costs a
re-hash. Files modified within 2 s are always re-hashed, since a write inside the
filesystem's timestamp granularity could otherwise be invisible.

**I7 — Learned knowledge may add to a key freely, and narrow it only under
proof.** Observed dependencies always make the execution key cover *more*.
Replacing the project-wide input set with a learned one requires passing
[`can_narrow`](#the-narrowing-gate), which is the single place in Arc that
decision is made. `crates/arc-core/src/dependency.rs`.

**I8 — A tracer failure is never a command failure.** If tracing cannot start,
breaks mid-run, or is refused by the environment, the command still runs and its
exit status is still Arc's exit status. What is lost is the observation, and a
run Arc could not fully observe never narrows.

## Dependency learning

### Family identity vs execution state

`cargo test` in a given project is an **execution family**. The contents of
`src/lib.rs` are **state** within it. The family key deliberately excludes file
contents so that Arc can still find what it learned after a file changes. It is
never used to authorise a hit — only to locate knowledge. Full detail in
[architecture.md](architecture.md).

### What `Complete` means

A trace is `Complete` when, for the execution just observed:

- every descendant process was followed, through `fork`, `vfork`, `clone`,
  `clone3`, `execve` and `execveat`;
- every syscall executed was either modelled by the backend or is on the
  explicit list of calls that cannot name a file, create a process, or reach
  outside the process tree;
- every path argument was reconstructed from the tracee and resolved against
  *that process's* working directory and descriptor table;
- every readable open, mapping, `stat`, `access`, `readlink`, directory
  enumeration, executed binary and failed lookup was recorded;
- nothing volatile was read, nothing reached the network, no budget overflowed,
  and the tracer reported no error.

### What `Complete` does not mean

It is a claim about **filesystem and process dependencies**, not about
determinism. Arc does not, and a filesystem tracer cannot, account for:

- the clock, timers, or sleep;
- `getrandom`, `RDRAND`, or any other entropy source;
- process ids, thread scheduling, or address-space layout;
- the hostname, uptime, or kernel state;
- anything already read into memory before Arc started watching.

A command whose output depends on the time of day will be cached and replayed,
and that replay will be stale. That is the same limit v0.1 had; complete tracing
does not change it. Where such a dependency leaves a filesystem trace — reading
`/dev/urandom`, reading `/proc`, opening a socket — Arc *does* notice, and
downgrades.

### Completeness states

| State | Meaning | May narrow inputs |
| --- | --- | --- |
| `Complete` | Everything above held | yes |
| `Partial` | Real observations, but something was unobservable or downgraded | no |
| `Unsupported` | No backend available, or tracing switched off | no |
| `Invalid` | Stored data failed validation | no |

### Downgrade reasons

Structured, not prose, so the engine can gate on them and `arc run --trace` can
show them:

| Reason | Cause |
| --- | --- |
| `backend_partial` | The backend cannot observe some dependency class at all — every snapshot-based trace |
| `unsupported_syscall` | A syscall this backend does not model, including one a newer kernel added |
| `path_resolution_failure` | A path argument could not be read back, or is not valid UTF-8, or a `fchdir` moved a process somewhere Arc cannot name |
| `child_escape` | A process was created that could not be followed |
| `event_overflow` | The run exceeded the trace budget |
| `volatile_read` | `/proc`, `/sys`, `/dev/urandom`, or another path whose contents Arc cannot meaningfully fingerprint |
| `network_access` | A socket was connected, bound, or sent on |
| `dependency_disappeared` | A dependency was read and is now gone, so it can no longer be fingerprinted |
| `backend_error` | The tracer itself failed |

Any one of these makes the trace `Partial`, and a `Partial` trace never narrows.

### The narrowing gate

`dependency::can_narrow` is the only function in Arc that decides whether
learned knowledge may replace the project-wide scan. It requires *all* of:

- the dependency schema, trace schema and trace-semantics versions match;
- the Arc version matches;
- the family key matches;
- the backend's declared capabilities match those the set was learned under;
- those capabilities cover every dependency class;
- the stored completeness is `Complete`;
- the downgrade list is empty;
- at least one execution has been observed, and it observed at least one input.

The last condition matters: a set naming nothing would narrow to nothing and hit
on every change. An empty observation is a sign something went wrong, not proof
that nothing matters.

### What a narrowed fingerprint covers

Exactly the same ground the project scan covered, and no less:

| Recorded | Fingerprinted as |
| --- | --- |
| Files read, inside the project | contents |
| Files read, outside the project | contents, with the same size/mtime cache |
| Executed binaries | contents |
| Directories enumerated | the sorted set of entry names and types |
| Paths whose presence or absence was consulted | the boolean |
| Symlinks | the link target *and* the resolved file, both |

A dependency Arc cannot read is fingerprinted as a distinct "missing" marker, not
skipped — a file that vanishes must change the key, not disappear from it.

Declared `[[command]] inputs` are fingerprinted **in addition**, even when a
complete trace is available. An explicit include is the user asserting a fact
about their build; a trace that did not happen to read that file on this run is
not grounds to overrule them.

### Temporal classification

Order is what separates an input from an intermediate, and it is why observations
are stored as an ordered stream rather than a set.

| Sequence | Classified as | Why |
| --- | --- | --- |
| read, then written | input *and* self-modified | It was consumed before it was changed |
| created, then read | output | It only exists because this run made it |
| created, then deleted | output | An intermediate |
| looked for, absent, then created | output | The lookup was about its own intermediate |
| enumerated | directory | The entry set is the dependency, not any one file |
| stat'd, is a directory, never enumerated | existence | Its presence mattered; its contents did not |
| written only | output | |

A run that rewrote something it read is marked `self_modified`, and such a run
keeps the execution key computed *before* it started. Filing it under a key
computed afterwards would describe the world the run left behind rather than the
one it consumed, and the next run would replay a result produced from different
input. That is a false hit, and it is the reason the flag exists.

### Learned dependency invalidation

A stored dependency set is discarded — not repaired — when any of these differ
from the current run: the dependency schema version, the trace schema version,
the trace *semantics* version, the family key, the Arc version, or the backend's
declared capabilities. Changing `[inputs]`, `[outputs]`, `[env]`, `[trace]`, or
any `[[command]]` block changes the family key, which invalidates the set as a
consequence.

The semantics version is separate from the schema version on purpose. The data
shape can stay identical while the *rules* change — which syscalls count as a
read, what makes a path volatile — and a set learned under the old rules was
called complete for reasons that no longer hold.

Retracing costs one execution. Trusting a stale set costs correctness.

### Merging observations

Inputs, directories, existence checks, outputs and externals are **unioned**
across runs: an execution may take a different branch next time, and forgetting a
dependency observed once is the unsafe direction. Executables and external files
are unioned by path with the newest digest winning, so a toolchain upgrade is
reflected rather than pinned. Completeness is the weakest of everything merged,
never the best run's, and downgrade reasons accumulate.

The consequence is that a conditional dependency set only grows. A command that
reads `a.toml` under one flag and `b.toml` under another ends up depending on
both, which costs misses and never costs correctness.

### Refreshing

A cache hit does not execute, so it cannot discover a dependency that a different
branch would have introduced. That is safe precisely because a hit means every
dependency Arc knows about is in the state it was in when that knowledge was
gathered — the branch cannot have changed. As soon as any of it differs, Arc
executes, observes again, and folds the result in.

## Modelled and observed by platform

| | `linux-ptrace` | `snapshot+jobobject` | `snapshot` |
| --- | --- | --- | --- |
| File reads | yes | no | no |
| Existence and metadata checks | yes | no | no |
| Directory enumeration | yes | no | no |
| Writes, creates, deletes | yes | yes | yes |
| Process tree | yes | yes | no |
| Executables | yes | yes | no |
| Paths outside the project | yes | no | no |
| Network detection | yes | no | no |
| **Automatic narrowing** | **yes** | no | no |

Environment reads are modelled by nobody. Reading `getenv` touches memory the
process already holds, so no syscall tracer can see it; Arc keeps its
conservative configured variable set instead.

## Linux tracing specifics

### The read rule

**A successful open that permits reading makes the path an input**, whether or
not any bytes were subsequently read.

This is deliberately conservative and it is what makes `mmap` correct for free: a
file cannot be mapped without first being opened, so the mapping was already
recorded. It also means a program that opens a file and reads nothing acquires a
dependency it does not really have, which costs a miss.

`openat2` carries its flags in a struct rather than a register. Arc does not read
tracee structs, so such an open is treated as readable — again the conservative
direction.

### Path resolution

Relative paths are resolved against the *traced process's* working directory or
the directory a descriptor names, never against Arc's own. Working directories
and descriptor tables follow the kernel's sharing rules: `CLONE_FS` shares the
former, `CLONE_FILES` the latter, so threads share both and forks share neither.
`clone3` passes its flags in a struct, so its children are modelled as sharing
nothing, which can only duplicate state rather than lose it.

An `fchdir` to a descriptor Arc cannot name leaves that process's later relative
paths unresolvable, and downgrades the trace.

### `/proc`, `/sys` and devices

- `/proc/self`, `/proc/thread-self` and `/proc/<pid>` are **ignored**: their
  contents are a function of the execution itself, not of any state a previous
  run could have left behind. Treating `/proc/self/maps` as machine state would
  downgrade essentially every Rust and Go program for no gain in safety.
- Everything else under `/proc` and `/sys` is **volatile**, and downgrades. This
  is not rare in practice: Debian's coreutils probe SELinux through
  `/sys/fs/selinux` and `/proc/filesystems`, so `mv`, `ls` and `cp` produce
  partial traces on a stock system. Arc reports that rather than papering over
  it.
- `/dev/null`, `/dev/zero`, `/dev/full`, the terminal and `/dev/fd` are ignored.
  Every other device, `/dev/random` and `/dev/urandom` included, is volatile.

### Network and IPC

Any `connect`, `bind`, `sendto`, `recvfrom`, `sendmsg` or `recvmsg` downgrades
the trace, whether or not it succeeded — a refused connection still means the
result depended on whether something was listening, and that is not a fact Arc
can fingerprint. Pipes and socket pairs internal to the process tree are not
socket *connections* and do not downgrade.

Arc does not model shared memory or message queues. A process using them to reach
outside its own tree is a gap; it is listed under known limits rather than
claimed as covered.

### Non-UTF-8 filenames

Linux filenames are bytes. Arc's stored path identity is text, and a name that is
not valid UTF-8 cannot make that round trip: the stored name would refer to a
file that does not exist, which fingerprints identically forever and would hit
when it should miss. Such a path therefore downgrades the trace, and the
conservative project scan — which carries real `PathBuf`s alongside display
strings and so handles arbitrary bytes — takes over.

### Budgets

One trace records at most 250,000 distinct `(operation, path)` pairs and tracks
at most 20,000 processes; one learned set holds at most 100,000 inputs, 20,000
existence checks and 20,000 external paths. Exceeding any of them sets
`event_overflow` and forbids narrowing. Nothing is silently truncated into a
smaller set that still claims to be complete.

## Paths, symlinks and reparse points

All path identity goes through `crates/arc-core/src/paths.rs`: one normalisation
(absolute, `\\?\` stripped, `/` separators, lexical `.`/`..` resolution) and one
comparison form (case-folded on Windows, case-sensitive elsewhere). Containment
compares whole segments, so `/repo-old` is never treated as inside `/repo`.

A symlink is fingerprinted by its target path, and the resolved target is added
to the dependency set in its own right. Retargeting the link changes the first;
editing the target changes the second. Symlinks are not captured as outputs,
since restoring one would recreate a pointer Arc never validated. Restoration
refuses any destination whose ancestors include a symlink, which covers Windows
junctions and reparse points.

## Paths outside the project

Classified as `Project`, `ArcInternal`, `External` or `System`. `ArcInternal` is
checked first, so Arc's own cache directory can never become one of its own
inputs even when placed inside the project — the regression test for that is not
optional.

`External` and `System` files that a complete trace observed being read are
fingerprinted by content, using the same size/mtime cache as project files. That
covers `libc.so.6`, the dynamic loader's cache, interpreters, CA bundles and
`/etc` configuration. Arc does not hash a system *directory* because a process
touched a library in it, and it does not treat an external read as irrelevant
merely because it is inconvenient.

## Races and their limits

Arc does not take a filesystem snapshot, so a genuinely atomic view of the
project does not exist. The exposures, stated plainly:

- **Fingerprint-then-execute.** A file modified between fingerprinting and the
  command reading it produces a result attributed to the earlier content. The 2 s
  mtime trust lag narrows the window for the *next* run's detection but does not
  close this one.
- **Execute-then-store.** A file modified while the command runs may be captured
  in either state.
- **Observe-then-fingerprint.** A dependency read during the run and modified
  before Arc hashes it is recorded with the later contents. This is the same
  window as above seen from the other end.
- **Snapshot granularity.** For the snapshot backends, a write landing inside the
  same timestamp tick as the post-execution snapshot is not seen as a write. A
  missed write means the file stays an ordinary input, which is the v0.1
  behaviour, not a hit.
- **Job assignment window.** On Windows a descendant process created between
  `spawn` and `AssignProcessToJobObject` is outside the job. Sub-millisecond, not
  zero, and it sets the trace lossy.

The ptrace backend closes none of these and creates none of them: it observes
syscalls as they happen, but Arc still hashes files afterwards.

## What is deliberately not cached

- Non-zero exits, unless `--cache-failures` or `[cache] cache_failures` is set.
- Executions killed by a signal: the result says nothing about the inputs.
- Output beyond `MAX_CAPTURE` (64 MB), which cannot be faithfully replayed.
- Anything run with `--no-capture`, where output went straight to the terminal.

## Known limits

- **Non-hermetic commands.** The clock, randomness and the network can make a
  command produce different results from identical inputs. Arc caches what it can
  observe; it cannot make a non-deterministic command deterministic. Where the
  non-determinism is visible as a syscall, Arc downgrades and stops narrowing.
- **Undeclared outputs are not restored.** A hit will not recreate files you have
  not declared in `[outputs]`. `arc run --trace` shows what a command actually
  wrote, which is the fastest way to find out what to declare.
- **Tracing costs.** ptrace stops a tracee twice per syscall. On syscall-dense
  work that is roughly a 40× slowdown, essentially the same cost as `strace -f`;
  on a compile it is closer to 7×. Warm hits pay none of it, because a hit never
  starts the tracer. `[trace] enabled = false` removes it entirely at the cost of
  never learning anything.
- **`.gitignore` is trusted** for input discovery. A build that depends on a
  gitignored file needs it added via `[inputs] include`, unless a complete trace
  observed the read — which it will.
- **macOS and Windows do not narrow automatically.** Neither has a
  non-privileged, non-injecting way to observe reads. `arc doctor` says so.

## Adversarial tests

`crates/arc-cli/tests/` covers, end to end against the real binary:

**Portable** (`cli.rs`, `dependency.rs`) — a replayed hit is byte-identical; a
changed input forces a miss and `--explain` names the file; family identity
survives content changes, separates arguments, and is invalidated by a scoping
change; a scoped command ignores changes outside its inputs and still misses
inside them; deleting a dependency, adding a file in scope, emptying a file, and
replacing a file with a directory all miss; pinning the conservative backend
never narrows and any change then invalidates; tracing never claims more than the
platform can see; observed writes become outputs, never inputs; Arc's cache
directory is neither fingerprinted nor graphed; a corrupt database executes and is
rebuilt; six concurrent traced runs leave metadata intact; fake secrets appear in
no output and in no file under `$ARC_HOME`; Unicode, spaces and deep nesting
survive; `arc affected` reports `unknown` rather than `unaffected` without
narrowing; exit codes survive and failures are not cached.

**Linux** (`linux_trace.rs`, skipped where ptrace is unavailable) — a file that
was read invalidates and one that was not does not; a file the run wrote is not
an input; a negative dependency invalidates when the file appears; a metadata-only
check is a dependency; adding a directory entry invalidates an enumeration; a
child's and a grandchild's reads count; sixty short-lived children are all seen; a
shebang script depends on its own text; a mapped executable in the project is a
dependency; a created-then-read intermediate is not a precondition; a
read-then-rewritten file never replays stale state; a renamed-into-place result is
an output; retargeting a symlink *and* editing its target both invalidate;
relative paths resolve against the child's working directory; a non-UTF-8 filename
falls back rather than being forgotten; reading `/proc` and touching the network
both prevent a completeness claim; a signalled child's status survives and is not
cached; a two-thousand-event run degrades without panicking; Arc never learns its
own cache; secrets never reach a traced dependency set; concurrent traced runs
stay independent.

**Unit** — hash framing, path normalisation, case rules, containment, family
identity, temporal classification, the narrowing gate under every downgrade,
merge semantics, dependency validation, narrowed fingerprinting of contents,
directories, absences and disappearances, syscall classification exhaustiveness
(no syscall may be both modelled and dismissed; an unknown number must be
neither), volatile-path policy, per-process path resolution and thread sharing,
Git porcelain parsing, path traversal refusal, atomic restore, glob narrowing,
size parsing, and fingerprint encoding including truncated data.

## The task graph

### Node and edge identity

A node is an execution **family**, the same identity the cache uses, so it
survives input changes, output changes, hits and misses. An edge `A → B` means
**B depends on A, and A must precede B**. That direction is the same in the
model, the JSON, the CLI and here.

Edges are derived from stored per-task rows every time the graph is loaded, and
never persisted. Rewriting a task's row therefore cannot leave a stale edge
behind — the failure mode a persisted edge table invites, and the reason this
one does not exist.

### When an edge exists

An edge is created only when a path one task **produces** intersects something
another task **consumes**:

| Consumer's dependency | Matching producer output | Edge kind |
| --- | --- | --- |
| A file it read | the same path | `output` |
| A directory it enumerated | any path inside that directory | `directory` |
| A path whose presence it consulted | the same path | `existence` |
| A `[[command]] inputs` glob | any produced path the glob matches | `declared` |
| — | — | `manual`, from `[[command]] after` |

Path matching goes through `paths::PathKey`, the same authoritative identity the
cache uses: case-folded on Windows only, `.`/`..` resolved, never a display
string comparison.

Three consequences worth stating:

- **Directory and existence edges are what make the graph honest.** A task that
  lists `plugins/` depends on a producer writing `plugins/new.so` even though
  that file did not exist when either was traced. A task that branched on
  `generated/config.json` being absent depends on whoever can create it.
- **Outputs come from the temporal classification, not from raw writes.** A file
  a task creates and then reads back is its own intermediate, so it produces no
  edge to anyone. This is the v0.3 rule reused, not a second one.
- **Self-edges are suppressed.** A task consuming what it produced is not its own
  prerequisite.

### Scope

Only paths inside the project take part. Existence dependencies outside it are
dropped when the row is written, and external reads were never outputs of
anything. Two projects that both touch `/tmp/shared` do not become one graph:
rows are keyed by project, and the graph is loaded per project.

### Multiple producers

When two tasks produce the same path, Arc does not choose. Both edges are
created — covering both is conservative, picking one is a guess — and the path is
reported as an ambiguity by `arc graph`, `arc doctor` and the JSON. Every edge
through an ambiguous path carries `ambiguous: true`.

### Cycles

Cycles are found with Tarjan's algorithm, iteratively, so a deep chain cannot
overflow the stack. A strongly connected component of more than one task, or a
task that reaches itself, is reported as a cycle by `arc graph` and `arc doctor`.

For scheduling, an SCC is condensed into **one group** that runs serially in
stable order, and the group as a whole waits for everything outside it that it
depends on. Arc does not pretend the cycle is a DAG, and it never deadlocks
waiting for a prerequisite that cannot finish.

### Affected propagation

`affected::analyse` takes a graph and a set of changed paths and returns one of
three verdicts per task. It is pure: no Git, no database, no terminal, which is
what makes it reusable for anything that needs "given these changes, what is the
minimal safe order?".

1. **Direct.** A change is a task's own dependency if it is a file it read, lies
   inside a directory it enumerated, is a path whose presence it consulted, or
   matches one of its declared input globs. Deletions and additions count the
   same way; a rename contributes both its old and new path.
2. **Transitive.** Verdicts propagate along edges, breadth first over the lattice
   `Unaffected < Unknown < Affected`. A task is only re-queued when its verdict
   strengthens, so each edge is relaxed at most twice and a cycle terminates.
3. **Fallback.** A task no change reached is `Unaffected` only if its inputs are
   provable — a complete trace, or declared scope. Otherwise it is `Unknown`.

**Unknown is never unaffected**, and uncertainty propagates: a task consuming
something an unknown task produces is itself unknown. `arc affected --run`
selects affected *and* unknown, so the only tasks skipped are those Arc proved
irrelevant.

Every verdict carries its causes, so `arc affected --explain` can answer "why is
this running?" by walking back to the changed path.

### Selective execution

The plan is a topological order of the selected subgraph, deterministic to the
tie-break: ready tasks are drained in label order, never in hash order. Each
task then runs through `arc run`, so it still passes the ordinary cache, trace
and learn pipeline. **Affected does not mean executed**: a task whose exact state
is already cached restores and reports `hit`, and its consumers proceed from the
restored outputs.

Bounded parallelism, defaulting to available parallelism capped at 16, with
`--jobs N` to override and `--jobs 1` for a deterministic serial run. Tasks are
run as child `arc run` processes, which is also what keeps the metadata database
free of in-process contention; concurrent Arc processes were already safe.

Output is buffered per task and printed as a block when it finishes, so two
tasks running at once never interleave mid-line.

### Failure and cancellation

A task whose prerequisite failed, was blocked, or was cancelled is reported
`blocked` and **never started**. Independent branches continue by default;
`--fail-fast` stops scheduling new work after the first failure. `arc affected
--run` exits non-zero if anything failed or was blocked — individual exit codes
cannot be preserved when several tasks fail, so a single indicator is the honest
summary.

Ctrl-C stops new work being scheduled. Children already running are in the same
process group, receive the signal from the terminal, and are reaped normally.

### Dynamic graph changes

Running a task can change what it produces, which can change the graph the plan
was built from. Arc plans once, from the pre-run graph, and does not re-plan
mid-flight. The conservative consequence is accepted deliberately: a task whose
relevance only appears *after* an upstream task runs is not picked up until the
next `arc affected`. What cannot happen is the reverse — a task being dropped
from a plan because an upstream run made it look irrelevant — because the plan is
never narrowed after it is built.

Between runs, a changed output set is picked up immediately: the task's row is
rewritten wholesale, so its old edges disappear with it.

### Invariants

- An edge exists only where produced and consumed paths intersect, or where
  `after` declares one.
- Unknown dependency knowledge never proves independence, for a task or for
  anything downstream of it.
- An ambiguous producer never silently becomes a single producer.
- Cycle detection and every traversal are iterative and terminating.
- A dependent of a failed task never executes.
- Every scheduled task goes through the normal engine, so cache correctness is
  unchanged by scheduling.
- Graph rows are per project; two projects never share a graph.
