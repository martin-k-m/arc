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

The asymmetry in the last two rows is the important part, and it is not
principled. `/dev/urandom` is caught because it is a *file*, and files are
what Arc watches. `getrandom(2)` — which is how glibc and CPython actually
seed themselves — is on the explicitly-dismissed syscall list in
`crates/arc-core/src/trace/linux/syscalls.rs`, alongside `clock_gettime`,
`gettimeofday`, `getpid` and the whole `sched_*` family. So a program that
reads randomness the old way loses its completeness claim and a program that
reads it the modern way does not.

The source states the reasoning: a command that depends on the clock is
non-hermetic in a way no filesystem tracer can repair, so the limit is
documented rather than turned into a downgrade on every run. That is a
defensible position. It is not the same as detecting nondeterminism, and Arc
should not be read as detecting it.

**What this means for you.** If your command embeds a timestamp, a random
seed, a process id or a hostname in its output, Arc will cache the first
answer and serve it forever, and will call the trace complete while doing so.
Arc does not and cannot know. `[trace] enabled = false` or simply not caching
that command are the only remedies.

## 2. Network access revokes completeness — and the two backends disagree about what counts

A TCP connection makes the trace partial, whether or not it succeeded: a
refused connection still means the result depended on whether something was
listening. **measured** — `python3` connecting to `127.0.0.1:9` reports
`the execution used the network`.

A Unix-domain socket is where it goes wrong. Same command, same machine, two
answers depending on which backend ran (**measured**):

```
ARC_TRACE_BACKEND=fast    python3 un.py   →  TRACE COMPLETE
ARC_TRACE_BACKEND=ptrace  python3 un.py   →  TRACE PARTIAL
                                              the execution used the network
```

The seccomp backend checks the address family and treats `AF_UNIX` and
`AF_NETLINK` as local IPC rather than as the network. The ptrace backend reads
the address family, discards it, and downgrades unconditionally —
`crates/arc-core/src/trace/linux/backend.rs` computes `family` and then does
`let _ = family;`.

This is a defect, not a design choice, and I have not fixed it. It matters
more than a cosmetic inconsistency because both backends advertise identical
capabilities, and the check that decides whether a learned dependency set may
still be used compares capabilities rather than backend names. So a set
learned under seccomp is considered valid for a ptrace run and vice versa,
while the two disagree about whether the execution that produced it was fully
observed. In practice this makes Arc *more* conservative under ptrace, never
less, so it costs cache hits rather than correctness — but it means
"complete" is not currently backend-independent, and that is exactly the
property the differential suite exists to guarantee.

Anything that talks to a local daemon over a Unix socket — a language server,
a build daemon, `sccache`, D-Bus, systemd's journal — is in this class.

## 3. `/proc`, `/sys` and `/dev`

The policy is one function, `policy::verdict` in
`crates/arc-core/src/trace/linux/mod.rs`, and it is short enough to state
completely.

| Path | Verdict | |
| --- | --- | --- |
| `/proc/self/...`, `/proc/thread-self/...`, `/proc/<pid>/...` | ignored, no downgrade | **measured** |
| any other `/proc/...` | volatile, trace partial | **measured** (`/proc/uptime`) |
| any `/sys/...` | volatile, trace partial | **measured** (`/sys/devices/system/cpu/online`) |
| `/dev/null`, `zero`, `full`, `tty`, `console`, `ptmx`, `stdin`, `stdout`, `stderr` | ignored | **measured** (`/dev/null`) |
| any other `/dev/...`, including `/dev/urandom` | volatile, trace partial | **measured** |

Three consequences worth knowing:

- The harmless-device list is an exact match on the name after `/dev/`, so
  `/dev/shm/...`, `/dev/tty1` and `/dev/ttyS0` are all volatile. **read**
- The prefix test is on the literal strings `"/proc/"`, `"/sys/"`, `"/dev/"`.
  A procfs bind-mounted elsewhere, or a chroot, is invisible to it. **read**
- A *write* to a volatile path is dropped silently: no record, no downgrade.
  Only reads count. **read**

This is why ordinary commands fall off the fast path. On a stock Debian
system `ls`, `mv` and `cp` probe SELinux through `/sys` and read
`/proc/mounts` and `/proc/filesystems`, so they never narrow. **measured** —
`/bin/ls` reports all three.

It is also the dominant reason real builds do not narrow. Of the three
workloads in [docs/BENCHMARKS.md](docs/BENCHMARKS.md), all three trace
partial, and volatile reads are the reason in every case: `cargo test` reads
`/sys/fs/cgroup/cpu.max`, `/proc/sys/vm/overcommit_memory` and
`/dev/urandom`; `pytest` reads `/sys/fs/selinux` and `/proc/mounts`; `make`
reads `/dev/urandom` through GCC. **measured**

## 4. Processes that outlive the command are not waited for, and the trace still claims completeness

**measured.** A run script that does:

```sh
setsid sh -c 'sleep 3; cat in.txt > /dev/null' < /dev/null > /dev/null 2>&1 &
exit 0
```

produces `TRACE COMPLETE`, and `arc run` returns in 54 ms while the detached
grandchild is still running and has not yet opened `in.txt`.

So a command that spawns a background worker gets a complete-looking trace
that does not include anything the worker went on to read. If that worker's
reads genuinely affect the result — a daemon that writes a file the next
command consumes — Arc has no record of the dependency and will hit when it
should miss.

Ordinary subprocesses are fine, and this is worth separating clearly.
Children and grandchildren are followed correctly (**measured**: a file read
only by `sh -c "cat in.txt"` inside the traced command is recorded as a
dependency, and the trace is complete). ptrace follows them through
`PTRACE_O_TRACEFORK`/`VFORK`/`CLONE`; the seccomp filter is inherited across
`fork` and survives `exec` and cannot be removed. **read** The gap is
specifically about *outliving*, not about *descending*.

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

## 6. Symlinks: correct for the ordinary cases, wrong for a dangling link

Arc records the path the program asked for, symlink spelling included, and
then adds the canonicalised target as a second dependency, so both the link
and what it points at are fingerprinted.

| Case | Result | |
| --- | --- | --- |
| edit the target's contents | MISS, correct | **measured** |
| repoint the link at another file | MISS, correct | **measured** |
| a dangling link's target appears | **HIT, wrong** | **measured** |

The last row is a genuine false hit and it is reproducible:

```sh
ln -sf missing.txt link.txt
# run.sh:  if [ -e link.txt ]; then echo yes; else echo no; fi
# learn, settle, then:
printf 'appeared' > missing.txt
# arc run  →  CACHE HIT, replays "no"
```

**Root cause** (**read**): symlink expansion canonicalises each candidate and
skips it when `canonicalize()` fails, with no downgrade recorded. For a
dangling link that always fails, so the target is never added as a dependency
and its later appearance is invisible. The link itself is hashed by its target
*path string*, which did not change. Separately, existence probes use
`symlink_metadata`, i.e. `lstat` semantics, so a dangling link counts as
present — which is why it becomes an input rather than a negative dependency
on the target.

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
| `path_resolution_failure` | a path argument could not be read back, or is not valid UTF-8 |
| `child_escape` | a process could not be followed |
| `event_overflow` | the execution exceeded the trace budget |
| `volatile_read` | a read of `/proc`, `/sys`, `/dev/urandom` or similar |
| `network_access` | a socket was connected, bound or sent on |
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

It is silent about time, randomness through `getrandom`, process identity and
anything that happens after the command returns. It disagrees with itself
about Unix sockets depending on which backend ran. It has one reproducible
false hit, on a dangling symlink whose target later appears.

And in practice the fast path is narrower than the feature list suggests: none
of the three real workloads measured in
[docs/BENCHMARKS.md](docs/BENCHMARKS.md) produce a complete trace, so none of
them narrow, so all of them fall back to hashing the project. Arc is still
useful there — the numbers show why — but it is useful as a whole-project
cache, not as the dependency-learning one the front page leads with.
