# Architecture

Arc is two crates. `arc-core` holds every decision; `arc-cli` is a thin
presentation layer over it. Nothing in `arc-core` prints to a terminal or reads
`argv`, so the same logic serves the CLI, tests, and any future programmatic
consumer without a rewrite.

## The run pipeline

```text
             arc-cli
                │  parses argv, chooses what to display
                ▼
     project::discover            nearest arc.toml, else nearest .git, else cwd
                │
                ▼
     Project::config_for          fold matching [[command]] blocks into config
                │
                ▼
     family::family_key           identity of this *kind* of execution
                │
                ▼
     db.dependency_set            what Arc learned about this family before
                │
                ▼
     DependencySet::validate      schema, semantics, capabilities, version
                │                 ── fails ──▶ discard, treat as no knowledge
                ▼
     dependency::can_narrow       the one gate: may learned knowledge be trusted?
                │
        ┌───────┴────────┐
       yes               no
        │                │
        ▼                ▼
 dependency::         scan::scan_inputs
 fingerprint          content-hash the whole project
 files, dirs,                │
 absences, execs            │
        └───────┬────────────┘
                ▼
     key::execution_key           may this exact result be reused?
                │
                ▼
     db.lookup ──── hit ────▶ outputs::restore ──▶ exec::replay
                │                                  (the tracer never starts)
               miss
                │
                ▼
     db.release()                 drop the redb lock before the child runs
                │
                ▼
     trace::start ─▶ exec::run ─▶ tracer.supervise(pid) ─▶ tracer.finish()
                │
                ▼
     DependencySet::from_observations ─▶ merge ─▶ db.put_dependency_set
                │
                ▼
     store.put_* + db.put_execution
```

## The task graph

```text
Execution family
      |
      +-- consumes (files, directories, existence)
      +-- produces (temporally classified outputs)
                |
                v
        graph::assemble          producer index, keyed by PathKey
                |
                v
          TaskGraph              nodes + derived edges + cycles + ambiguities
                |
        +-------+--------+
        v                v
 affected::analyse    arc graph
        |
        v
   plan::build                   topological, deterministic, cycle-condensed
        |
        v
   plan::execute                 bounded parallelism, per-task buffering
        |
        v
      arc run                    one child per task, ordinary cache semantics
```

One durable row per task, in `task_graph`; edges are derived on every load and
never stored, so rewriting a row cannot leave a stale edge. `plan::execute`
spawns `arc run` per task rather than calling the engine in-process: the engine
takes an exclusive lock on the metadata database, and cross-process locking was
already proven safe by the concurrency tests.

## Module boundaries

| Module | Owns |
| --- | --- |
| `paths` | The only definition of path identity: display form, comparison form, project/external/system/arc classification |
| `project` | Project discovery, `arc.toml`, per-command scoping |
| `family` | `FamilyKey` — execution *identity* |
| `key` | `execution_key` — execution *state*; environment and toolchain fingerprints |
| `scan` | Project-wide input discovery and content hashing |
| `trace` | Observation backends behind one capability-declaring interface |
| `dependency` | `DependencySet`, temporal classification, `can_narrow`, narrowed fingerprinting |
| `graph` | `TaskNode`, edge derivation, cycles, ambiguity, topological order |
| `affected` | Direct and transitive propagation with provenance; pure, reusable |
| `plan` | `ExecutionPlan` and the bounded-parallel scheduler |
| `git` | Optional, isolated; nothing in the run pipeline depends on it |
| `store` | Content-addressed blobs |
| `db` | redb metadata, schema versioning, indexes |
| `exec` | Child process spawn, streamed tee, exit status, the `Supervisor` hook |
| `outputs` | Output capture and path-safe restoration |
| `engine` | Sequences the above; holds no policy of its own |
| `maintenance` | Stats, GC, prune, verify |

## Two keys, two questions

The easiest thing in Arc to get wrong.

```text
FamilyKey                          ExecutionKey
"have I seen this before?"         "may I reuse that result?"

program + args                     everything in FamilyKey
working directory                  + input digest      ← narrowed or project-wide
project scoping config             + environment digest
OS + arch                          + toolchain digest
                                   + learned dependency digest
                                   + declared output globs
```

The family key must **not** include file contents. If it did, editing one file
would move the execution into a new family and Arc could never find what it had
learned. The execution key includes everything, and is the only thing that may
authorise a cache hit.

The input digest is where narrowing lives. Under `can_narrow` it covers the
learned dependency set; otherwise it covers a project walk. Those are different
digests over different ground, which is why `SCHEMA_VERSION` went to 3: a v2
entry for the same command in the same project is not comparable.

`SCHEMA_VERSION`, `FAMILY_KEY_VERSION`, `DEPENDENCY_SCHEMA_VERSION`,
`TRACE_SCHEMA_VERSION`, `TRACE_SEMANTICS_VERSION`, `GRAPH_SCHEMA_VERSION`,
`PROTOCOL_VERSION` and
`DB_SCHEMA_VERSION` are separate on purpose: each can be bumped without invalidating more than it has to.
`TRACE_SEMANTICS_VERSION` is the subtle one — it exists because the *rules* a
backend applies can change while the stored shape stays identical, and a set
called "complete" under the old rules must not be reused under the new ones.

## Storage

Content-addressed blobs in `$ARC_HOME/store/blobs/<2>/<62>`, metadata in
`$ARC_HOME/arc.redb`.

| Table | Key | Value |
| --- | --- | --- |
| `executions` | execution id | `ExecutionRecord` |
| `history` | `{started_at:013}-{id}` | execution id |
| `cache_entries` | execution key | `CacheEntry` |
| `fingerprints` | project id | packed size/mtime/digest blob |
| `execution_families` | family key | `ExecutionFamily` |
| `dependency_sets` | family key | `DependencySet` |
| `task_graph` | `{project}\0{family}` | `TaskNode` |
| `counters` / `meta` | name | value |

`task_graph` is ordered by key, so loading one project's graph is a contiguous
range scan and never touches another project's rows. Each row is compact —
labels, structured command, produced and consumed paths — rather than a copy of
the dependency set, so a ten-thousand-task graph loads without deserialising ten
thousand full dependency sets. It is written in the same transaction as the
dependency set it derives from.

Raw syscall streams are never persisted. A dependency set holds normalised paths
and counts; `ExecutionRecord.trace` holds a summary and the downgrade reasons as
text.

### Concurrency

redb takes an exclusive file lock. Arc keeps one handle open per *phase* of a
run and calls `Db::release()` before spawning the child, so the lock is never
held across an arbitrarily long command. Acquisition retries for up to 20s.
This is what makes six concurrent `arc run` processes safe, and it is why the
open handle lives behind a `RefCell` rather than being threaded through every
call.

### Migration

A database whose `meta.schema` marker does not match `DB_SCHEMA_VERSION` — or
which cannot be read at all — is deleted and rebuilt. Cache contents are
disposable; misreading them is not. Arc never interprets an old record under new
semantics, and never fails a user's command because its own cache is damaged.

## Tracing

### The interface

```rust
trait Tracer {
    fn name(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;
    fn launch(&self) -> Launch;                       // Normal | Traced
    fn attach(&mut self, pid: u32) -> Result<()>;
    fn supervise(&mut self, pid: u32) -> Option<Result<Wait>>;
    fn finish(self: Box<Self>) -> Observations;
}
```

`Capabilities` is the contract: every field is a promise the backend must keep
for *every* execution, not a best effort. Everything downstream is gated on those
rather than on which backend happens to be loaded.

Two hooks exist because two very different mechanisms have to fit. A snapshot
diff needs neither; a job object needs `attach`; a ptrace backend needs
`Launch::Traced` — which tells `exec` to have the child put itself under tracing
before `exec` — and needs to own the wait loop, because for ptrace the wait loop
*is* the event loop.

### `linux-ptrace`

The v0.3 backend, and the first to reach `Completeness::Complete`.

```text
exec::run
   │  pre_exec: ptrace(PTRACE_TRACEME)          ← the only moment this is possible
   ▼
child stops at exec ─▶ PTRACE_SETOPTIONS
   │                    TRACESYSGOOD | TRACEFORK | TRACEVFORK
   │                    TRACECLONE   | TRACEEXEC | EXITKILL
   ▼
┌─ waitpid(-1, __WALL) ──────────────────────────────────────┐
│                                                            │
│  syscall stop ─▶ PTRACE_GET_SYSCALL_INFO                   │
│      entry ─▶ remember (nr, args); stat existence for      │
│               creating opens; capture execve image         │
│      exit  ─▶ classify against the fd table and cwd,       │
│               record a FileObservation                     │
│                                                            │
│  PTRACE_EVENT_FORK/VFORK/CLONE ─▶ register child,          │
│               sharing cwd/fds per CLONE_FS/CLONE_FILES     │
│  PTRACE_EVENT_EXEC ─▶ record script + /proc/pid/exe        │
│  signal stop ─▶ forward it unchanged                       │
│                                                            │
└─▶ all tracees gone ─▶ Observations                         ┘
   │
   ▼
dependency::from_observations
   │  temporal classification: ordered events ─▶ input | output |
   │  directory | existence | intermediate
   ▼
DependencySet
```

Files:

| File | Contains |
| --- | --- |
| `trace/linux/sys.rs` | Every `unsafe` line: `ptrace`, `waitpid`, `fork` (probe only), `process_vm_readv`. Nothing else in the backend is unsafe |
| `trace/linux/syscalls.rs` | Which syscalls carry a dependency, which are provably irrelevant, and the fall-through that downgrades everything else |
| `trace/linux/state.rs` | Per-process working directory and descriptor table, with kernel sharing rules |
| `trace/linux/backend.rs` | The event loop and the rules turning syscalls into observations |
| `trace/linux/mod.rs` | Availability probing and the `/proc`, `/sys`, `/dev` policy |

Three decisions worth stating:

**Why ptrace and not something faster.** Seccomp user notification is the
natural successor and is far cheaper, but installing a listener has required
`CAP_SYS_ADMIN` since it was introduced. `fanotify` and eBPF need capabilities a
developer tool must not ask for, and observe the whole machine rather than one
process tree — a privacy problem as much as a correctness one. `LD_PRELOAD`
misses static binaries, misses raw syscalls, and is defeated by `setuid`; a
tracer that silently misses Go binaries cannot claim completeness. ptrace is the
slow option and the honest one: it needs nothing but being the parent, and it
works under `kernel.yama.ptrace_scope = 1` because the child puts *itself* under
observation rather than Arc attaching to a stranger.

**Why `PTRACE_GET_SYSCALL_INFO` and not registers.** It reports whether a stop is
an entry or an exit, which removes the classic ptrace bug of losing entry/exit
alignment on a freshly cloned child. It is also architecture-independent, so Arc
carries no per-architecture register mapping and cannot mis-trace on an
architecture it was never built for. The cost is a Linux 5.3 floor, which the
availability probe checks rather than assumes.

**Why availability is probed by trying it.** A container may permit ptrace,
forbid it via seccomp, or forbid it via `kernel.yama.ptrace_scope`, and only an
attempt distinguishes them. `sys::probe` forks a child that calls
`PTRACE_TRACEME` and stops; the answer comes from how that child stopped. The
result is cached for the process, and `arc doctor` reports it verbatim.

### `snapshot+jobobject` (Windows)

Metadata walk of the project before and after the execution, diffed to yield
creates, writes and deletes, plus process-tree observation: the child is assigned
to an anonymous job object with an I/O completion port, and Windows posts a
notification for every descendant. That is how Arc sees that `cargo test` is
really `cargo`, `rustc`, a linker and the test binaries. The `unsafe` is confined
to `trace/windows_job.rs`. It cannot see reads, so it never narrows.

### `snapshot` (everywhere)

The portable fallback and the pinned backend for `--trace-backend snapshot`.
Writes only.

### Still not built

**macOS.** EndpointSecurity requires a signed entitlement Apple grants per
application; DTrace needs SIP changes. Neither is viable for a tool installed
with `cargo install`, so macOS runs the snapshot backend and says so.

**Windows reads.** ETW's kernel file provider requires administrator rights, and
last-access timestamps are disabled by default. Nothing non-privileged and
non-injecting can observe reads.

## Testing on Linux from elsewhere

`scripts/linux-check.sh` runs the workspace inside a container:

```bash
scripts/linux-check.sh test --workspace
scripts/linux-check.sh clippy --workspace --all-targets --all-features -- -D warnings
ARC_DOCKER_ARGS=--security-opt=seccomp=unconfined scripts/linux-check.sh test --workspace
```

A container is not just convenient — it is also the environment most likely to
*refuse* ptrace, so the same run exercises both the backend and its unavailable
path. `scripts/bench.sh` and `scripts/demo.sh` are meant to be run the same way.

## The remote cache

```text
                        ExecutionKey
                             │
                             ▼
                       local metadata ──hit──▶ restore ──▶ done
                             │ miss
                             ▼
                       remote client
                             │
              ┌──────────────┼───────────────┐
              ▼              ▼               ▼
       GET execution   POST objects/  GET objects/{digest}
         metadata         missing        (bounded parallel)
              │              │               │
              └──────────────┴───────┬───────┘
                                     ▼
                            hash every object
                                     │
                                     ▼
                               local CAS commit
                                     │
                                     ▼
                            local cache entry written
                                     │
                                     ▼
                          ordinary local restore path
```

Upload runs the same pipeline backwards, and in a fixed order:

```text
execution succeeds ─▶ local CAS + metadata commit
                            │
                            ▼
                  POST objects/missing        (one round trip for the set)
                            │
                            ▼
                  PUT objects/{digest} ...    (bounded parallel, deflate above 4 KB)
                            │
                            ▼
                  PUT executions/{key}        (last, always)
```

| Module | Responsibility |
|---|---|
| `remote::protocol` | Wire types, limits, and the validation both ends run |
| `remote` | HTTP client, retry, bounded transfers, metrics, config resolution |
| `arc-cache` | Reference server: filesystem CAS, namespaced records, auth |

The protocol types live in `arc-core` so the client and the reference server
share one definition of the format and one implementation of its validation.
The server crate is separate, so the `arc` binary does not ship an HTTP server
it will never run.

**Why blocking HTTP.** `ureq` with `rustls` is pure Rust: no OpenSSL, no C
toolchain, no async runtime. Arc's engine is synchronous, and transfer
concurrency is a bounded worker pool over a slice of digests — a few dozen lines
against the cost of making unrelated core modules async.

**Why the scheduler needs no remote code.** `arc affected --run` spawns
`arc run` per task, so each task gets remote lookup, verification and upload
through the ordinary engine. The scheduler's only remote-aware decision is
dividing the transfer pool by `--jobs`, so sixteen tasks do not open sixteen
full-sized transfer pools.
