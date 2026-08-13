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
     DependencySet::validate      schema, capabilities, version, family match
                │                 ── fails ──▶ discard, treat as no knowledge
                ▼
     scan::scan_inputs            content-hash the project (size/mtime fast path)
                │
                ▼
     key::execution_key           may this exact result be reused?
                │
                ▼
     db.lookup ──── hit ────▶ outputs::restore ──▶ exec::replay
                │
               miss
                │
                ▼
     db.release()                 drop the redb lock before the child runs
                │
                ▼
     trace::start ─▶ exec::run ─▶ tracer.attach(pid) ─▶ tracer.finish()
                │
                ▼
     DependencySet::from_observations ─▶ merge ─▶ db.put_dependency_set
                │
                ▼
     store.put_* + db.put_execution
```

## Module boundaries

| Module | Owns |
| --- | --- |
| `paths` | The only definition of path identity: display form, comparison form, project/external/system/arc classification |
| `project` | Project discovery, `arc.toml`, per-command scoping |
| `family` | `FamilyKey` — execution *identity* |
| `key` | `execution_key` — execution *state*; environment and toolchain fingerprints |
| `scan` | Input discovery and content hashing |
| `trace` | Observation backends behind one capability-declaring interface |
| `dependency` | `DependencySet`, `Completeness`, merge and validation rules |
| `graph` / `affected` | Read-only projections over families and dependency sets |
| `git` | Optional, isolated; nothing in the run pipeline depends on it |
| `store` | Content-addressed blobs |
| `db` | redb metadata, schema versioning, indexes |
| `exec` | Child process spawn, streamed tee, exit status |
| `outputs` | Output capture and path-safe restoration |
| `engine` | Sequences the above; holds no policy of its own |
| `maintenance` | Stats, GC, prune, verify |

## Two keys, two questions

This is the central idea of v0.2 and the easiest thing to get wrong.

```text
FamilyKey                          ExecutionKey
"have I seen this before?"         "may I reuse that result?"

program + args                     everything in FamilyKey
working directory                  + project input digest
project scoping config             + environment digest
OS + arch                          + toolchain digest
                                   + learned dependency digest
                                   + declared output globs
```

The family key must **not** include file contents. If it did, editing one file
would move the execution into a new family and Arc could never find what it had
learned. The execution key includes everything, and is the only thing that may
authorise a cache hit.

`SCHEMA_VERSION`, `FAMILY_KEY_VERSION`, `DEPENDENCY_SCHEMA_VERSION`,
`TRACE_SCHEMA_VERSION` and `DB_SCHEMA_VERSION` are separate on purpose: each can
be bumped without invalidating more than it has to.

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
| `dependency_edges` | `{project}\0{rel}\0{family}` | `""` |
| `counters` / `meta` | name | value |

`dependency_edges` is ordered by key, so "which families depend on this file?"
is a contiguous range scan scoped to one project rather than a full table scan.
Only families whose inputs are actually narrowed get rows, which bounds the
index at the size of the declared scope rather than the size of the repository.

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
semantics.

## Tracing backends

`trace::Tracer` is deliberately small: `attach(pid)` before the child runs,
`finish()` after it exits, plus a `Capabilities` struct that says exactly what
the backend can see. Everything downstream is gated on those capabilities rather
than on which backend happens to be loaded.

### Shipping today

**`snapshot`** (all platforms). Metadata walk of the project before and after
the execution, diffed to yield creates, writes and deletes. No privileges, no
injection, no kernel interface. Cannot see reads.

**`snapshot+jobobject`** (Windows). Adds process-tree observation: the child is
assigned to an anonymous job object with an I/O completion port, and Windows
posts a notification for every descendant process. This is how Arc sees that
`cargo test` is really `cargo`, `rustc`, a linker and the test binaries. The
`unsafe` needed for it is confined to `trace/windows_job.rs`.

### Not built

**Linux.** `ptrace` and seccomp-unotify can observe reads without privileges but
cost a context switch per syscall; `fanotify` needs `CAP_SYS_ADMIN`. A ptrace
backend is the most likely first source of `Completeness::Complete`.

**macOS.** EndpointSecurity requires a signed entitlement Apple grants per
application; `DTrace` needs SIP changes. Neither is viable for a tool installed
with `cargo install`.

**Windows reads.** ETW's kernel file provider requires administrator rights, and
last-access timestamps are disabled by default. Nothing non-privileged and
non-injecting can observe reads, which is why the Windows backend declares
`file_reads: false` rather than approximating it.

When a read-capable backend lands, `Completeness::Complete` becomes reachable and
input narrowing switches on at the single gate in `dependency.rs`. No call site
outside that module needs to change.
