# Tracing backends

Arc learns what a command depends on by watching it run. This document describes
the mechanisms that watching uses, what each one can and cannot see, and how Arc
chooses between them.

For what Arc does with the result — how a trace becomes a cache decision, and
what `Complete` is allowed to mean — see [correctness.md](correctness.md).

## The backends

| | `linux-seccomp` | `linux-ptrace` | `snapshot+jobobject` | `snapshot` |
| --- | --- | --- | --- | --- |
| Platform | Linux 5.6+ | Linux | Windows | anywhere |
| Privileges | none | none | none | none |
| File reads | yes | yes | no | no |
| Existence checks | yes | yes | no | no |
| Directory enumeration | yes | yes | no | no |
| Writes, creates, deletes | yes | yes | yes | yes |
| Process tree | yes | yes | yes | no |
| Executables | yes | yes | yes | no |
| Paths outside the project | yes | yes | no | no |
| Network detection | yes | yes | no | no |
| **Automatic narrowing** | **yes** | **yes** | no | no |

The two Linux backends record through the same `Recorder`, so they share one
policy for what is relevant, one deduplication rule and one set of budgets.
Neither can drift from the other by editing only itself.

## Selection

```
arc run --trace-backend auto|fast|ptrace|snapshot|off -- <command>
ARC_TRACE_BACKEND=fast arc run -- <command>
```

`auto` — the default — takes the lowest-overhead backend that is actually
available. A pinned backend that cannot run **falls back rather than failing**:
pinning is a preference, not an assertion about the machine. `arc doctor` reports
which backend is preferred and why each other one is or is not available, and
`arc run --trace` reports the backend that actually ran.

```console
$ arc doctor
  preferred           linux-seccomp
  linux-seccomp       available
  linux-ptrace        available
  snapshot            available
```

`off` disables dependency learning entirely. Arc then falls back to the
conservative project scan, which is correct but never narrows.

## `linux-seccomp`

Seccomp user notification. Arc installs a filter in the child before `exec`; the
kernel then blocks the child on each filtered syscall and hands Arc a
notification describing it. Arc records what it needs and answers
`SECCOMP_USER_NOTIF_FLAG_CONTINUE`, and the syscall proceeds normally.

### It needs no privilege

Installing a filter requires either `CAP_SYS_ADMIN` **or** `no_new_privs`, and
Arc sets `no_new_privs`. So an ordinary user can trace an ordinary command: no
`sudo`, no capability, no daemon, no kernel module, no reboot.

The cost of `no_new_privs` is that a setuid or file-capability program in the
traced tree no longer gains its extra privilege. Arc does not attempt to work
around this — a command must not gain privilege because Arc traced it, and it
must not silently lose it either, so this is documented rather than hidden.

### The filter is inverted

The filter **allows** the syscalls Arc's table proves cache-irrelevant and
**notifies** on everything else — including syscall numbers that do not exist
yet. A future kernel's new file-opening syscall is therefore trapped, decoded as
unknown, and downgrades the trace. The alternative — listing what to trap —
would let exactly that syscall through silently.

### Events cannot be lost

Notification is a synchronous request and response. There is no ring buffer, so
there is nothing to overflow: if Arc is slow, the traced process waits. If Arc
cannot decode a notification it still answers it, and records a downgrade. A
trace can be refused; it cannot be quietly incomplete.

Filters are inherited across `fork` and `exec` and cannot be removed, so no
descendant escapes.

### No return values, no descriptor table

A notification arrives *before* the syscall runs, so Arc never sees its result.
Two consequences, both handled by asking the kernel rather than guessing:

- **Existence** is established by stat-ing the resolved path at notification
  time. The file could in principle change between that stat and the syscall.
  Both directions are safe: recorded-present but actually gone is caught by the
  `DependencyDisappeared` downgrade on the next run, and recorded-absent but
  actually present costs a miss.
- **Descriptors and working directories** are read from `/proc/<pid>/fd/<n>` and
  `/proc/<pid>/cwd`. This is exact, and it removes every way a tracked
  descriptor table could drift out of step with reality — `dup`, `F_DUPFD`,
  `CLONE_FILES`, inheritance across `exec`.

Every read of tracee state is followed by `SECCOMP_IOCTL_NOTIF_ID_VALID`, which
is what makes pid reuse harmless: if the process that made the request is gone,
the notification id is stale and the bytes are discarded.

### When it is unavailable

- Kernels before 5.6, or without `CONFIG_SECCOMP_FILTER`.
- A sandbox whose own seccomp policy denies the `seccomp` syscall — Docker's
  default profile does exactly this.
- Architectures other than x86-64 and aarch64.

Availability is *proved*, not assumed: Arc forks a child, installs a one-syscall
filter, receives a real notification, answers it, and checks that the child's
syscall returned the true value. Anything less than a full round trip counts as
unavailable and Arc uses ptrace.

## `linux-ptrace`

`PTRACE_SEIZE` with `PTRACE_O_TRACESYSGOOD`, stopping the tracee twice per
syscall. It remains in the tree for three reasons: it is the fallback wherever
seccomp notification is unavailable, it is the reference implementation the fast
backend is tested against, and it can see syscall *results*, which makes it the
oracle in the differential suite.

It is roughly three to six times more expensive than `linux-seccomp` on
syscall-heavy work — see [Performance](#performance).

## `snapshot`

Compares the project tree before and after execution. It sees what changed; it
cannot see what was read. It never narrows, and it is always available, which is
what makes it a safe floor.

`snapshot+jobobject` is the Windows variant: the same tree comparison plus a job
object, which gives it the process tree and the executables.

## Differential testing

`crates/arc-cli/tests/trace_differential.rs` runs the same programs under ptrace
and under seccomp and compares the resulting dependency sets. The rule is
asymmetric, deliberately:

- The fast backend may record **more** than ptrace. That is a false miss, which
  costs time.
- It may never record **less**. That would be a false hit, which is a
  correctness bug.
- It may claim `Complete` only where ptrace also does, with one exception:
  ptrace's `path_resolution_failure` is that backend admitting it could not read
  a path out of the tracee. That is a statement about ptrace, not about the
  program, and the fast backend — which reads the same path from `/proc` — has
  nothing to admit.

The corpus covers reads, writes, existence checks, enumeration, children and
grandchildren, renames, deletes, symlinks, changed working directories,
generated intermediates, exec chains and non-UTF-8 names, plus concurrent
sessions and a descriptor-leak check.

## Performance

Median of five rounds, Linux x86-64 in a container, each round with a fresh Arc
home and a changed input so nothing can hit. `snapshot` is Arc doing everything
*except* watching syscalls, so the difference between a backend and `snapshot`
is what that backend's tracing costs.

| workload | direct | snapshot | ptrace | seccomp |
| --- | --- | --- | --- | --- |
| read 300 files | 107 ms | 156 ms | 1316 ms | 484 ms |
| 200 short-lived children | 69 ms | 127 ms | 740 ms | 228 ms |
| 200 directory enumerations | 132 ms | 172 ms | 1524 ms | 509 ms |
| produce 200 outputs | 6 ms | 391 ms | 526 ms | 428 ms |

Isolating the tracing cost — backend minus `snapshot` — seccomp is 3.5×, 6.1×
and 4.0× cheaper than ptrace on the first three. On the fourth the cost is
dominated by Arc capturing 200 output files, which both backends pay equally;
tracing is not the expensive part of that workload.

Reproduce with `scripts/trace-bench.sh` (`ROUNDS=n` to change the sample).

A warm cache hit does not trace at all, and is unaffected by any of this.
