# Limitations

Syscall tracing has sharp edges. This is the list of them: what Arc gets
wrong, what it refuses to handle, and what it cannot see at all. It is here
because a cache that tells you where it stops being trustworthy is worth more
than one that does not.

Every claim is labelled:

- **measured** — established by running the case. `bench/limits-probe.sh`
  reproduces all of them; the environment is the one in
  [docs/BENCHMARKS.md](docs/BENCHMARKS.md).
- **read** — established by reading the source, with the file named. Not
  reproduced here, either because it needs hardware or a kernel this machine
  does not have, or because the case is hard to construct honestly.

Nothing here is inferred from documentation or from what the code ought to do.

---

## 1. Determinism is not modelled at all

Arc observes the filesystem. Everything else that can change a result is
outside it.

| Case | Verdict | |
| --- | --- | --- |
| `date +%s%N` | `TRACE COMPLETE` | **measured** |
| `python3 -c "random.random()"` | `TRACE COMPLETE` | **measured** |
| `echo $$` (the process id) | `TRACE COMPLETE` | **measured** |
| `head -c 8 /dev/urandom` | `TRACE PARTIAL`, volatile read | **measured** |

The asymmetry in the last two rows is real and it is now *reported* rather
than silent. `/dev/urandom` is caught because it is a *file*, and files are
what Arc watches. `getrandom(2)` — which is how glibc and CPython actually
seed themselves — is modelled, but it produces a note in `arc run --trace`
rather than a downgrade:

```
note   the execution took randomness from getrandom
```

Making it a downgrade was implemented and measured, and reverted. glibc calls
`getrandom` during start-up, so **every** command went partial: `date`,
`sh -c "cat in.txt"` and a single `pytest` file all lost completeness, and
nothing narrowed anywhere. **measured.** The syscall is evidence that a process
started, not that its result depends on randomness. See
[docs/DECISIONS.md](docs/DECISIONS.md).

So the position is unchanged and now stated exactly: a command that depends on
the clock or on randomness is non-hermetic in a way no filesystem tracer can
repair. Arc says what it saw and does not pretend to detect nondeterminism.

**What this means for you.** If your command embeds a timestamp, a random
seed, a process id or a hostname in its output, Arc will cache the first
answer and serve it forever, and will call the trace complete while doing so.
Arc does not and cannot know. `[trace] enabled = false` or simply not caching
that command are the only remedies.

## 2. Network access revokes completeness, and a Unix socket is a path

A TCP connection makes the trace partial, whether or not it succeeded: a
refused connection still means the result depended on whether something was
listening. **measured** — `python3` connecting to `127.0.0.1:9` reports
`the execution used the network`.

The two backends used to disagree about `AF_UNIX`: seccomp called it local IPC
and stayed complete, ptrace downgraded. They now apply one rule, and it is
neither of those. The address of a `connect` is a path, so:

| Case | Verdict | |
| --- | --- | --- |
| `connect` to a Unix socket that does not exist | `TRACE COMPLETE`, absence recorded | **measured**, both backends |
| that socket then appears | MISS | **measured**, both backends |
| `connect` to a Unix socket that does exist | `TRACE PARTIAL`, network access | **measured**, both backends |
| `bind`, `sendto`, `sendmsg`, `recvmsg` on any family | `TRACE PARTIAL` | **read** |
| an abstract Unix socket (no filesystem name) | `TRACE PARTIAL` | **read** |

Why it matters that this is not simply "ignore `AF_UNIX`": anything that talks
to a live local daemon — a language server, a build daemon, `sccache`, D-Bus,
systemd's journal — answers with data no filesystem fingerprint describes, and
those still downgrade. What no longer downgrades is the case that dominates in
practice, glibc asking `/var/run/nscd/socket` on every user lookup and being
told `ENOENT`. **measured**: with a blanket downgrade, 32 of 180 runs of the
click hit-rate experiment lost completeness for that alone.

The residual gap is in the seccomp backend only, and it is a race: it decides
by asking the filesystem whether the socket exists at notification time, so a
socket created in the microsecond before the syscall runs would be connected to
under a complete trace. **read**.

## 3. `/proc`, `/sys` and `/dev`

The policy is one function, `policy::verdict` in
`crates/arc-core/src/trace/linux/mod.rs`.

| Path | Verdict | |
| --- | --- | --- |
| `/proc/self/...`, `/proc/thread-self/...`, `/proc/<pid>/...`, `/proc/mounts` | ignored, no downgrade | **measured** |
| `/proc/sys/...`, `/sys/fs/cgroup/...`, `/sys/fs/selinux/...`, `/sys/devices/system/cpu/...`, `/sys/kernel/mm/...` | hashed like an ordinary file | **measured** |
| any other `/proc/...` or `/sys/...` | volatile, trace partial | **measured** (`/proc/uptime`) |
| `/dev/null`, `zero`, `full`, `tty`, `console`, `ptmx`, `stdin`, `stdout`, `stderr` | ignored | **measured** (`/dev/null`) |
| any other `/dev/...`, including `/dev/urandom` | volatile, trace partial | **measured** |

The hashed row is the change that moved the real workloads. Those pseudo-files
are machine configuration: small, re-readable and stable, so Arc fingerprints
them rather than distrusting them. That direction cannot produce a stale hit —
a changed value changes the key — while ignoring them could. `/proc/mounts` is
ignored rather than hashed because it is a symlink to `self/mounts`, which is
the per-process view Arc already ignores. `/proc/filesystems` — what `mkdir`
and `ls` read — is *not* on the list: hashing it was measured to break
cross-checkout remote cache hits, and that trade is refused until the mechanism
is understood. Both in [docs/DECISIONS.md](docs/DECISIONS.md).

What this bought, **measured**, on the three workloads in
[docs/BENCHMARKS.md](docs/BENCHMARKS.md):

| Workload | before | after |
| --- | --- | --- |
| `cargo test` (serde_json) | partial: cgroup, `/proc/sys/vm/overcommit_memory`, transparent hugepage, `/dev/urandom`, network, one unresolved path | partial: `/dev/urandom` and a real TCP connection |
| `pytest`, whole suite (click) | partial: `/sys/fs/selinux`, `/proc/mounts` | partial: one non-UTF-8 filename its own tests create |
| `make -j1` (tinycc) | partial: `/dev/urandom` via GCC | unchanged |

None of them reaches complete, and the reasons that remain are honest ones:
GCC and rustc really do read `/dev/urandom`, serde_json's tests really do open
a TCP connection, and click's tests really do create a filename that is not
valid UTF-8.

Three consequences still worth knowing:

- The harmless-device list is an exact match on the name after `/dev/`, so
  `/dev/shm/...`, `/dev/tty1` and `/dev/ttyS0` are all volatile. **read**
- The prefix test is on literal strings. A procfs bind-mounted elsewhere, or a
  chroot, is invisible to it. **read**
- A *write* to a volatile path is dropped silently: no record, no downgrade.
  Only reads count. **read**

## 4. A process that outlives the command costs the trace its completeness

**measured.** A run script that does:

```sh
setsid sh -c 'sleep 2; cat in.txt > /dev/null' < /dev/null > /dev/null 2>&1 &
exit 0
```

used to produce `TRACE COMPLETE` while the detached grandchild was still
running and had not yet opened `in.txt`. It now reports:

```
◆ TRACE PARTIAL
  not complete        a process could not be followed
```

The two backends reach that answer differently, and the difference is cost, not
claim. **measured**, same script:

| Backend | verdict | wall clock |
| --- | --- | --- |
| `linux-seccomp` | `TRACE PARTIAL` | 100 ms |
| `linux-ptrace` | `TRACE COMPLETE` | 2,077 ms |

ptrace waits for the grandchild and genuinely observes its read, so its
"complete" is true and it pays the two seconds to earn it. The seccomp backend
returns as soon as the command does; the listener hangs up only when every
process holding the filter is gone, so anything else after a short grace means
a process is still alive and the trace stops claiming to have seen everything.

Ordinary subprocesses are unaffected: children and grandchildren are followed
correctly and traced complete (**measured**). The gap was about *outliving*,
not about descending, and it is now reported rather than hidden.

Related, from source: `clone3` passes its flags in a struct rather than a
register, so Arc cannot read them and models the child as sharing nothing with
its parent — which duplicates state rather than losing it, and is the safe
direction. **read**

## 5. mmap: writes through a mapping are invisible, but the open flags save it

Reading a file through `mmap` is recorded. **measured** — a Python script that
maps a file `PROT_READ` and reads it produces a complete trace with the file
as an input.

Writes through a `MAP_SHARED` mapping produce no syscall at all. Arc cannot
see the store instruction, and `mmap`'s `prot` argument is not even decoded —
`Sc::Mmap` captures only the descriptor and the flags. **read**

In practice this does not open a hole, and the reason is worth stating because
it is luck rather than design. To write through a mapping you must have opened
the file writable, and Arc records `FileOp::Write` from the *open flags*,
before any mapping exists. **measured** — a script that opens `shared.bin`
`r+b`, maps it, and stores a byte through memory records both `read` and
`write` on `shared.bin`, and changing the file afterwards is correctly a miss.

The residual gap is narrow but real: Arc's record of *what changed* comes from
intent (the open mode) rather than from observation, so a file opened writable
and never actually written is recorded as written. That direction is safe.

**io_uring** is the case where there is no such backstop. `io_uring_setup`
and `io_uring_enter` are deliberately neither modelled nor dismissed, so they
land in the unsupported-syscall path and revoke completeness. Arc cannot see
io_uring I/O; it refuses to claim it did. **read**

## 6. Symlinks

Arc records the path the program asked for, symlink spelling included, and then
adds the canonicalised target as a second dependency, so both the link and what
it points at are fingerprinted.

| Case | Result | |
| --- | --- | --- |
| edit the target's contents | MISS, correct | **measured** |
| repoint the link at another file | MISS, correct | **measured** |
| a dangling link's target appears | MISS, correct | **measured** |

The last row was a reproducible false hit and is fixed. `canonicalize` fails
for a dangling link, so the target was never added and its later appearance was
invisible, while the link itself hashes to its target *path string*, which had
not changed. The chain is now walked by hand when canonicalisation fails, and
the first name in it that is not there is recorded as a *negative* dependency —
the same mechanism Arc already uses for a file a command looked for and did not
find. `a_dangling_links_target_appearing_is_a_miss` in
`crates/arc-cli/tests/linux_trace.rs` fails without the fix.

Two more from source, not reproduced here:

- Path resolution during the trace is purely lexical and never touches the
  filesystem, deliberately, so that a path already deleted can still be named.
  The cost is that `a/symlink/../b` is normalised to `a/b`, which is not what
  the kernel would have resolved. **read**
- `O_NOFOLLOW` and `AT_SYMLINK_NOFOLLOW` are not decoded anywhere. `stat` and
  `lstat` collapse to the same observation. **read**

## 7. Directory listings and absent files are handled correctly

Included because these are the cases a naive tracer gets wrong, and Arc does
not.

| Case | Result | |
| --- | --- | --- |
| add a file to an enumerated directory | MISS | **measured** |
| create a file the command looked for and did not find | MISS | **measured** |

## 8. What makes a trace incomplete, in full

The complete list of downgrade reasons, from
`crates/arc-core/src/trace/model.rs`. **read**

| Reason | Meaning |
| --- | --- |
| `backend_partial` | the backend cannot observe every dependency class (this is every run on Windows and macOS) |
| `unsupported_syscall` | a syscall this backend does not model, including one a newer kernel added |
| `path_resolution_failure` | a path argument could not be read back, or is not valid UTF-8. `arc run --trace` now names the syscall it came from, up to three per run |
| `child_escape` | a process could not be followed |
| `event_overflow` | the execution exceeded the trace budget |
| `volatile_read` | a read of an unstable `/proc` or `/sys` file, `/dev/urandom` or similar |
| `network_access` | a socket was bound or sent on, or connected to a peer that exists |
| `dependency_disappeared` | a path was read and is now gone |
| `backend_error` | the tracer itself failed |

Two notes on that table. `dependency_disappeared` is declared and never
constructed anywhere in the crate — it is currently dead vocabulary. And a
run's downgrade list is capped at 32 entries, so a pathological execution
reports the first 32 reasons and not the rest; the verdict is unaffected,
since one is enough.

## 9. Budgets

Past any of these, the trace is marked lossy and stops claiming completeness
rather than growing without bound. **read**, from
`trace/linux/recorder.rs`, `dependency.rs`, `exec.rs`.

| Limit | Value |
| --- | --- |
| distinct `(operation, path)` observations | 250,000 |
| processes tracked | 20,000 |
| inputs, outputs, directories (each) | 100,000 |
| absent-path records | 20,000 |
| external paths and executables (each) | 20,000 |
| snapshot-backend entries | 200,000 |
| captured output per run | 64 MB — past this the run is not cached at all |
| path argument length | 4,096 bytes, silently clipped |

There is no wall-clock limit on a trace and no limit on the number of syscall
notifications, only on distinct recorded observations.

## 10. Environment variables are not observed

A syscall tracer cannot see a memory read, and `getenv` is a memory read. Arc
does not attempt it. Instead a curated list of variables is hashed into the
key — `PATH`, `LANG`, `CC`, `CFLAGS`, `RUSTFLAGS`, `PYTHONHASHSEED` and about
a dozen more — and projects add their own with `[env] include`. **read**,
`crates/arc-core/src/key.rs`.

If your command reads a variable that is not on that list and not in your
config, changing it will not invalidate the cache. This is the one limitation
here with a straightforward fix, and it is on you rather than on Arc: name the
variable.

## 11. Platforms without a read-observing backend

On Windows and macOS Arc cannot see reads. The snapshot backend emits
`backend_partial` on every run by construction, so **nothing ever narrows**
and every run pays the whole-project scan. `arc doctor` reports this. **read**

The snapshot backend also has a timestamp-granularity race: two writes inside
one timestamp tick can be missed. The source argues this is miss-safe rather
than hit-unsafe. Not reproduced here. **read**

## 12. Kernel and architecture requirements

**read.** ptrace needs Linux 5.3 or newer for `PTRACE_GET_SYSCALL_INFO`.
Seccomp user notification needs 5.5 for `USER_NOTIF_FLAG_CONTINUE`, and only
supports x86-64 and aarch64. On anything older or otherwise-shaped, Arc falls
back to the snapshot backend and never narrows.

A traced tree cannot gain privilege through `setuid`, because the seccomp
filter is installed with `PR_SET_NO_NEW_PRIVS`. If your build depends on a
`setuid` helper, it will behave differently under `arc run`.

---

## The honest summary

Arc's dependency model is sound for the case it was built for: a command that
reads files, enumerates directories, checks for files that are not there,
spawns children, and writes results. It is correct on all of those, including
the two — directory enumeration and negative dependencies — that a file-level
tracer gets wrong.

There is no known false hit. The one that was here — a dangling symlink whose
target later appears — is fixed and pinned by a test. The two backends now
agree about Unix sockets, and a process that outlives the command costs the
trace its completeness under both.

It is still silent about time, about process identity, and about randomness in
the sense that matters: `getrandom` is reported but does not downgrade, because
making it downgrade was measured to take completeness away from every command
including `sh -c "cat in.txt"`.

And the fast path is still narrower than the feature list suggests. All three
real workloads in [docs/BENCHMARKS.md](docs/BENCHMARKS.md) still trace partial,
but no longer for reasons Arc can do anything about: GCC and rustc read
`/dev/urandom`, serde_json's tests open a TCP connection, and click's own test
suite creates a filename that is not valid UTF-8. Per-file test tasks — the
granularity that actually gets value out of a cache — do trace complete and do
narrow, 180 runs out of 180 in the hit-rate experiment.
