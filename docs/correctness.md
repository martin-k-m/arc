# Cache correctness

Arc's value depends entirely on never serving a wrong result. False misses cost
seconds; a single false hit costs trust. Every design choice below resolves in
favour of executing.

## Invariants

**I1 — Reuse requires equivalence.** Two executions share a result only when the
command, arguments, relative working directory, family identity, input digest,
environment digest, toolchain digest, learned-dependency digest, declared output
globs, platform, Arc schema version, and — when one is in force — the execution
environment's content id are all identical. `crates/arc-core/src/key.rs`.

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

## The remote cache

Arc 0.5 adds a shared cache. The invariant it introduces is one sentence:

> Remote cache data is untrusted until Arc has verified it locally.

Everything below follows from that, and from the fact that a remote cache is an
optimisation. Arc may miss a remote hit, fail an upload, or find the server
gone; none of those may change what the user's command does.

### Trust model

Assume the server is buggy, corrupt, or hostile. It can return anything for any
request. What it cannot do is make Arc replay a result Arc has not verified, or
write a file Arc has not validated.

Three checks stand between a response and a replay.

1. **Metadata validation.** A record is rejected unless its protocol version,
   cache-semantics version, execution key, OS, architecture, digest syntax,
   entry count and every output path pass. An invalid record is a miss.
2. **Object verification.** Every downloaded object is hashed as it is received
   and admitted only if it hashes to the digest that was asked for. A
   `Content-Length`, an `ETag`, a `200` and a server-supplied digest header are
   not evidence of anything.
3. **Restore validation.** Remote outputs go through exactly the same
   `outputs::restore` path as local ones: no absolute paths, no `..`, no
   symlinked ancestor, and every destination resolved before any is written.
   There is no second restoration implementation for remote data.

### How a remote result becomes a local one

```
execution key ─▶ local lookup ─miss─▶ remote lookup ─▶ validate record
                                                         │
                          fetch only objects not held ◀───┘
                                   │
                          hash each on arrival
                                   │
                          commit to local CAS
                                   │
                          write a local cache entry
                                   │
                          ordinary local replay
```

A remote hit is converted into a *valid local hit* before a single output byte
is written. That is why the next run of the same command is a plain local hit
with no network at all, and why remote data can never take a shortcut past a
check that local data must pass.

If any object is missing, corrupt, truncated, or the connection dies partway,
the entry is discarded whole and the command executes. Nothing is partially
restored: half a result is not a result.

### Publishing

Objects first, record last. A record is a promise that its objects can be
fetched, so it is only made once they can be. If the record write then fails,
some objects are orphaned on the server — an acceptable cost, and the opposite
ordering is not acceptable at all.

Uploads are idempotent by construction: an object is named by its content, and a
record is compared byte-for-byte with what is already stored. Two clients
publishing the same result concurrently both succeed. A client publishing a
*different* result for the same execution key is refused rather than allowed to
overwrite, because under Arc's contract at most one of the two can be valid and
the server cannot tell which.

### Server-side verification

The reference server hashes every uploaded object itself and refuses bytes that
do not match the digest in the URL. Client-side verification would catch the
poisoning eventually, but only after the bad object had been served to everyone.
Defence in depth costs one hash per upload.

### Cross-machine compatibility

The execution key already covers OS, architecture, cache-semantics version,
program, arguments, working directory *relative to the project root*, the
environment fingerprint, the content hash of the resolved executable, and every
input Arc knows about. Two machines that agree on all of those agree on the
result; two that do not, do not share a key.

Absolute paths are deliberately absent from the key, so the same project checked
out at `/home/a/repo` and `C:\src\repo` produces the same key — which is what
makes a shared cache useful at all. Machine-specific state does not slip through
this: a dependency outside the project is identified by its content, and a
different toolchain binary is a different toolchain digest.

Namespaces isolate execution records. Objects are content-addressed and may be
deduplicated across namespaces; a client only ever learns the digests named by
records it can read.

### Offline behaviour

DNS failure, connection refused, timeout, TLS failure, 5xx, malformed response,
bad credentials — all of them mean "no remote result", and the run proceeds
locally. Timeouts are bounded, retries are bounded and apply only to transport
faults and 5xx, and a local hit never opens a connection in the first place.
Normal development does not become dependent on a network.

### Credentials

The configuration holds the *name* of an environment variable, never a token.
The name is validated as an identifier, not evaluated as shell syntax. Tokens
are never written to the database, the object store, execution records, history,
JSON output, logs, or error messages, and redirects are not followed so a
credential cannot be forwarded to another host. `arc doctor` and `arc remote
status` report that a token is configured, never what it is.

## CI

`arc ci` is orchestration. It selects work and reports on it; it does not weaken
anything above. Cache keys, completeness, narrowing and unknown-propagation
behave in CI exactly as they behave on a laptop.

### Comparison correctness

The affected analysis is only as good as the diff it is given, so resolving
base and head is where CI can go wrong quietly.

* A pull request compares its head against its **base**, taken from the event
  payload. It is not the checked-out commit against its parent: a workflow that
  checks out GitHub's synthetic merge commit would then be comparing the merge
  against one of its parents, which answers a different question.
* A merge group uses the merge queue's own base and head, not a pull request's.
* A push uses the event's `before`, falling back to `HEAD~1` only when that
  commit is actually present. The all-zero sha — GitHub's way of saying "there
  was no previous commit" — is not a revision.
* `--base`/`--head` override everything, so a CI decision can be reproduced
  locally exactly.

Comparisons are one of two kinds, never a blend: a commit range, or a commit
against the working tree. `--base` alone means the working tree, including
untracked files, which is what a developer reproducing CI wants. In CI the head
always comes from the provider.

Revisions are resolved through `git rev-parse --verify`, as arguments, never
through a shell. A branch name is untrusted input.

### No diff means no proof

Every path that cannot establish a complete diff ends in the same place: the
comparison is unknown, every declared task is raised to `Unknown`, and
everything runs.

* the base commit is absent from a shallow clone
* the event carries no base at all (`workflow_dispatch`)
* the event payload is missing, truncated, malformed, or has the wrong types
* `git diff` itself fails
* the provider names a head this checkout does not contain

Verdicts are only ever raised this way, never lowered — a task the graph already
proved affected stays affected. The reason is reported rather than swallowed.

### Canonical task selection

CI considers only tasks the repository declares. A command Arc happens to have
learned about, but which `arc.toml` does not name, is never executed by
`arc ci` even when a change reaches it. A declared task Arc knows nothing about
is `Unknown`, therefore selected, therefore executed and learned from.

### Trust and remote writes

Trust is a policy input, not a security boundary, and Arc treats it as such: it
decides only whether to *publish*, never whether to believe. A fork pull request
is untrusted. `pull_request_target` is never trusted, because Arc cannot see
which tree the workflow checked out. Anything Arc cannot classify is not
trusted. The policy is enforced on the children too — `arc ci` passes a
read-only remote down to every task it schedules, so a task cannot publish
merely because a trusted workflow started it.

### CI environment variables

CI injects variables that identify a *run* rather than describe the work:
`GITHUB_RUN_ID`, `GITHUB_RUN_ATTEMPT`, `GITHUB_JOB`, `GITHUB_STEP_SUMMARY` and
others. If Arc hashed the whole environment, every CI run would miss.

It does not, and never has: the environment digest covers an allowlist —
`DEFAULT_ENV` plus `[env] include` — so a variable participates in a key only
because someone named it. Nothing needed excluding, and nothing is excluded,
which matters because the alternative (dropping everything matching `GITHUB_*`)
would silently erase a real dependency for any project that legitimately reads
one. A project that *does* include a volatile variable is warned by `arc ci` and
`arc doctor`, and its choice is still honoured.

### Shared task knowledge

A fresh CI runner has no local graph. Arc publishes each learned task's
dependency knowledge to the remote cache and fetches it for the tasks the
checked-out `arc.toml` declares, so a new machine can prove work unaffected
without having run it once.

This metadata is optimisation data and never authority:

* The family key is derived **locally**, from the checked-out configuration. A
  record that does not carry that exact key is discarded.
* `program`, `args` and `rel_cwd` must match the local declaration byte for
  byte. They are compared, never adopted — the command Arc runs always comes
  from the repository.
* Graph, dependency and trace semantics versions must all match this Arc, and
  the record must have been observed on the same OS and architecture.
* Every path in the record goes through the same validation as any other wire
  path: no traversal, no absolute paths, no drive-qualified paths.
* Knowledge that is partial, unparseable, or written in a completeness word this
  Arc does not recognise leaves the task `Unknown`.

A compromised cache server can therefore make Arc run **more** work than
necessary. It cannot make Arc run different work, and it cannot make Arc skip
work. Remote knowledge is used for *selection* only; it never narrows a cache
key, so it cannot cause a false hit either.

### Estimated work avoided

The figure comes from the median of each task's recent recorded durations,
bounded to a fixed window per family. Only real executions are recorded — a
replay measures restoration, not work. A task with no history contributes
nothing and is reported as unmeasured, so a first run cannot claim a saving it
has no evidence for.

### Provider-supplied text

Repository names, branch names and file paths on a fork pull request are written
by whoever opened it. Nothing from a provider reaches a terminal, a Markdown
summary or a workflow command without being stripped of control characters and
escaped for its destination. Workflow-command values additionally encode `%`,
carriage return and newline, so no provider-controlled string can begin a
`::error::`. Identity remains bytes; only presentation is sanitised.

## Execution environments

Full detail is in `docs/environments.md`; this section states only what bears on
correctness.

### Identity

`EnvironmentId` is the digest of the environment manifest's canonical bytes, and
the manifest names every file by content digest. So the id is a function of the
environment's content and of nothing else: not the alias, not the hostname, not
the absolute path it was captured from. The manifest's bytes are themselves a CAS
object under that digest, so verifying an environment is the object verification
Arc already performs — and a server that serves different bytes for an id fails
the hash check rather than producing a toolchain nobody asked for.

Canonical form is checked on read. A manifest that hashes to the requested id but
was serialised differently is refused, so the id cannot drift from being a
function of the content.

### Environment participation in the key

When a command declares an environment, its id enters the execution key. A result
built under environment A is never served to a run under environment B. Arc makes
no attempt to prove two environments equivalent; that is deliberate.

When no environment is declared, nothing is hashed at all, so every key computed
before v0.8 still means exactly what it meant.

`PATH` leaves the environment fingerprint when an environment is in force,
because the environment defines `PATH`. Nothing else is removed: a variable a
command genuinely reads still matters wherever it runs.

### Host versus environment

Arc packages userspace, not kernels and not the C library.

A library a captured binary needs that resolves to a system directory is recorded
as a **host requirement** by soname. One that resolves elsewhere — a toolchain's
own `lib/` reached through `$ORIGIN` — is captured. Mixing a captured `libc.so.6`
with the host's `ld.so` is the failure mode that makes naive `.so` copying
unreliable, and glibc's loader and C library are one unit.

An environment therefore requires: same OS and architecture, same libc flavour, a
dynamic loader at each recorded `PT_INTERP` path, and each recorded system soname
resolvable. The first four are checked — coarsely by the client against the
worker's advertised host capability, and definitively by the worker itself, where
the answer is actually knowable. A worker that cannot satisfy them fails
`Incompatible`, which is safe to run locally.

The kernel is not packaged and not version-gated. Running a userspace newer than
the host kernel fails the way it always does.

### Structural completeness is not hermeticity

`Complete` means every declared tool resolved and every library a captured binary
names was accounted for. It says nothing about whether a *particular* command has
everything it needs. `Partial` lists what was unresolved, and those gaps are part
of identity — two captures that missed different things are different
environments. A `Partial` environment is refused for remote execution.

Hermeticity is a property of one execution, reported per run:

* `hermetic` — a complete trace observed nothing outside the environment, the
  project, the kernel surfaces, and the declared host loader and libraries.
* `host-dependent` — the command read host state the environment does not supply.
  The paths are named. The result is correct here and is not portable, and such a
  command is not remote-eligible, because Arc already refuses to send a command
  whose complete trace reads outside the project.
* `unknown` — the execution was not completely observed, so Arc claims nothing.
  In practice most real compilers land here on Linux, because reading a volatile
  path downgrades the trace below `Complete`. Hermeticity is only ever claimed
  from an execution Arc watched in full.

An environment-backed execution is not called hermetic merely for having used an
environment.

### No silent fallback to the host

A command with an environment resolves its program inside that environment. If
the environment does not provide it, the run fails — locally with an error naming
the environment, and on a worker with `Incompatible`. A fallback to the host's
copy would make the result depend on the machine, which is the one thing the
environment exists to prevent. The same holds for child tools: they resolve
through the environment's `PATH`, which contains environment directories and
nothing else.

### Immutability

A materialised environment is a shared read-only template. Files are written
read-only and directories left traversable but not writable, so a command that
tries to rewrite a compiler gets an error rather than corrupting every future job
that reuses it. Copying a toolchain per execution would cost more than the
executions save.

The mechanism is file permissions. A process running as root, or as the same
user with `chmod`, can defeat them; Arc says so rather than claiming otherwise,
and `arc env verify` re-hashes every object.

Readiness is a sibling marker written last. A directory without one is wreckage
from an interrupted build and is removed rather than used, so a partially
materialised environment can never execute. Concurrent requests for one id
materialise once; across processes the atomic rename decides and the loser
discards its work. Collection never removes an environment with a live lease.

### Environment paths are Arc's own state

An executable Arc materialised is Arc's own state, not the machine's. It is
excluded from learned dependencies, because its *path* is a directory name that
changes with every capture while its *content* is already covered — precisely and
portably — by the environment id in the execution key. Leaving it in would bind
the key to one machine's Arc home, which is the opposite of what an environment
is for.

### Untrusted manifests

An environment manifest arriving from a cache is data from an untrusted server
and is validated before a byte is written: schema version, entry count, duplicate
paths, malformed digests, path traversal, absolute or escaping symlink targets,
symlinks that are also files, and secret-shaped variable names are all refused.
Paths are checked again at the point that writes, so validation and use cannot
drift apart.

An execution request names an environment **by id only**. A coordinator can ask a
worker for an environment that already exists and hashes to what it said; it
cannot describe one.

### What v0.8 does not change

Host-mode remote execution is untouched: a worker with no environment support
advertises no feature, an environment-backed request is refused with a stated
reason, and everything that worked in v0.7 still works. Environments are additive.

## Remote execution

A worker is an execution backend, not a second Arc. It runs a command Arc has
already decided must run, and produces a result Arc verifies exactly as it
verifies any other remote data.

### The trust rule

A worker is untrusted until its result is independently validated. "Success" is
not a claim Arc accepts:

* The job must describe the execution key that was asked for; a worker cannot
  answer a question nobody asked.
* Every output path is validated — no traversal, no absolute paths, no
  drive-qualified paths — before anything is written.
* Every object is fetched from the shared cache and re-hashed on the way into
  the local store. A worker that publishes bytes not matching their digest
  publishes nothing usable.
* Restoration is the ordinary `outputs::restore` path, with the ordinary
  ancestor-symlink and containment checks. There is no remote-specific restore.

The client verifies what the server stored, and the server verifies what the
worker uploaded. Each layer checks the one below it.

### Eligibility

Remote execution is off unless a project turns it on, and even then it happens
only when Arc can build a sufficiently complete and compatible environment. The
gate is one function. It refuses on platform mismatch, semantics mismatch,
protocol mismatch, host-bound arguments, secret-shaped environment
dependencies, reads outside the project observed by a complete trace, an
executable whose contents differ on the worker, and an explicit
`remote = "never"`.

What Arc sends is exactly what it fingerprinted: the learned dependency set when
a complete trace authorises narrowing, otherwise every project file. It never
sends less than the fingerprint covers in order to make a transfer cheaper.

### What compatibility does not establish

Without an Arc environment, the `environment_id` a worker advertises covers the
platform, the architecture, Arc's semantics version and the libc flavour, and
matched executables cover the programs Arc knows the command runs. With one,
eligibility stops asking whether the worker already has the same compiler and
starts asking whether it can run the environment — but the paragraph below still
applies to everything the environment does not contain.

**Neither covers the shared libraries those executables load, the kernel, the
locale data, the CPU's instruction set extensions, or anything else the host
supplies.** Two machines with the same `environment_id` and byte-identical
toolchains can still differ. Arc's position is that this is a property of the
worker fleet rather than something a hash can certify: run workers you have
reason to believe are equivalent, and Arc will not silently paper over the ones
that are not. Where it can tell — OS, architecture, semantics, executable
contents — it checks and refuses.

The limits that have always applied still apply, and remote execution makes them
easier to notice rather than worse: a command that reads the clock, consumes
randomness, or depends on scheduling order is not made deterministic by running
somewhere else.

### Secrets

A variable that looks like a secret and is set makes a command ineligible. Arc
hashes such variables for cache identity; that is not consent to transmit them.
A project may name specific variables in `[remote.execution] allow_env`, which
is deliberately explicit and deliberately per-variable. Nothing else is sent —
the request carries only the variables the execution key depends on.

The worker's own environment is never inherited by a command. The child's
environment is constructed from nothing, so the worker's cache credentials
cannot leak into a build.

### Singleflight and duplicate work

Submission is idempotent on the execution key. A worker already running that
execution returns the same job, so N clients wanting one result cause one
execution, and a retried request after a network hiccup cannot start a second.

Before executing, the worker rechecks the shared cache: a result published while
the job was queued is returned instead of recomputing it.

Cancellation is advisory and waiter-counted. One client giving up does not
discard work the others are still waiting for.

### Failure and fallback

Infrastructure failure and command failure are different answers and are never
flattened together. A command that exits non-zero is a completed execution with
that exit code, preserved exactly, and cached only under the same rules a local
failure would be.

When the environment fails, what Arc may do next depends on whether the command
might have started:

* **Safe to run locally** — incompatible worker, unavailable inputs, sandbox
  setup failure, overloaded queue, unreachable worker. Nothing ran, so Arc runs
  it here.
* **Not safe** — timeout, cancellation, a worker lost mid-job. The command may
  still be running somewhere. Running it again could duplicate whatever it does,
  so Arc fails with an explanation rather than guessing. `--no-remote-execution`
  is the deliberate override.

A worker restart loses the jobs it was running. Arc does not fake recovery: the
client's wait times out and it reports that, because a job whose state is
genuinely unknown is not a job to draw conclusions from.

### Side effects

Arc can only represent a command's effects as files and streams. A command that
deploys, publishes, or writes to a database has effects Arc cannot see, capture,
or restore — and remote execution makes that concrete, because the effect would
happen on a machine the user never looked at. Such commands should carry
`remote = "never"`. Arc does not try to detect them heuristically; it defaults
to the conservative side of everything it can detect and leaves the rest to a
declaration.

### The sandbox's limits

The reference worker isolates the filesystem, the environment and the process
tree. It does not isolate the network, and it says so in its advertised
capabilities rather than implying otherwise. It is not a defence against hostile
code: a command runs as the worker's user. This is stated plainly because a
sandbox believed to be stronger than it is, is worse than one known to be weak.
