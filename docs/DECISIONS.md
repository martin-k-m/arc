# Decisions

Why Arc is built the way it is, and what the alternative cost. Each entry
names one decision, the option that was seriously considered instead, and the
reason it lost. Where a decision is a compromise, the compromise is stated
rather than hidden.

## 1. Observe syscalls with ptrace, and prefer seccomp user notification

Four mechanisms can watch a process open a file: `ptrace`, seccomp user
notification, `fanotify`, and eBPF. A fifth, `LD_PRELOAD`, only appears to.
The comparison is in `crates/arc-core/src/trace/linux/mod.rs`.

`LD_PRELOAD` lost first and lost badly. It interposes on libc, so it sees
nothing a statically linked binary does, nothing a program that issues raw
syscalls does, and nothing at all across a `setuid` boundary. Go binaries
alone would make it useless. A tracer that silently misses a whole class of
programs cannot report "complete", and Arc's entire narrowing story is built
on being able to say that word truthfully.

`fanotify` and eBPF both lost on privilege and scope. They need
`CAP_SYS_ADMIN` or `CAP_BPF`, which a developer tool has no business asking
for, and both observe the whole machine rather than one process tree. That is
a privacy problem before it is a correctness one: Arc would be watching files
that have nothing to do with the build.

`ptrace` needs no privilege beyond being the parent, follows every descendant,
and works under `kernel.yama.ptrace_scope = 1` because the child places
*itself* under observation before `exec`. It is also slow. Seccomp user
notification is the same observation for a fraction of the cost and is now
the preferred backend; ptrace remains as the fallback where a container
profile denies `seccomp`. Both funnel through one recorder, so "complete"
means the same thing whichever ran.

## 2. Reject FUSE or an overlay filesystem

An alternative to watching syscalls is to change what the filesystem is: mount
the project through FUSE or an overlay and log every VFS operation. Bazel-like
systems do a version of this.

It was rejected on setup cost and on honesty. Mounting anything needs
privilege on most systems, and a build that runs inside a mount Arc controls
is no longer the build the developer runs outside it — path spellings change,
`st_dev` changes, and anything that inspects its own filesystem sees something
different. Arc's promise is that `arc run cargo test` is `cargo test` with the
result recorded, and a filesystem swap breaks that promise for a class of
programs nobody can enumerate in advance.

The cost of the decision is real: Arc pays per-syscall interception where a
filesystem could have batched, and the tracing tables in
[BENCHMARKS.md](BENCHMARKS.md) show what that costs.

## 3. Directory listings and existence checks are first-class dependencies

The naive dependency model is "the set of files that were read". It is wrong
in two directions and both are silent.

A command that enumerates `plugins/` depends on the *set of entry names*, not
on the contents of the files it happened to open. Adding a plugin must be a
miss even though no existing file changed. So `getdents64` records the
directory, and the directory's fingerprint is its entry names and types
(`dependency.rs`, `Role::Directory`).

A command that looks for `optional.cfg`, does not find it, and takes the other
branch depends on that file's *absence*. Creating it must be a miss even
though the command never read anything. So a failed lookup records the path as
a negative dependency (`Role::Existence`).

The alternative considered was to treat both conservatively by falling back to
the whole-project scan whenever a directory was enumerated. That is correct
but it gives up narrowing for almost every real build, since every compiler
enumerates something. Modelling them explicitly costs two extra dependency
classes in the stored record and one extra rule in the fingerprint, which is
cheap next to what it buys.

## 4. Key on content, and split the key into a family and an execution

The execution key hashes the schema version, OS and architecture, the program
and its arguments with boundaries preserved, the working directory *relative
to the project root*, the input digest, the environment digest, the toolchain
digest, the dependency-set digest, and the declared output globs
(`crates/arc-core/src/key.rs`).

Two choices inside that are worth stating. First, the working directory is
relative, not absolute. The alternative — hashing the absolute path — is
simpler and was what Arc did originally, and it makes every key local to one
machine, so a shared cache never hits across checkouts. That was a real bug;
see [BUGS.md](BUGS.md).

Second, identity is split. The *family* key covers what stays the same across
runs of the same command, and the *execution* key adds what the world looked
like this time. Without that split there is nowhere to hang a learned
dependency set: Arc has to find "what did this command depend on last time"
before it can know which inputs to hash, and the thing it looks that up by
cannot itself contain the inputs.

Environment variables are hashed, never stored, and a variable whose name
looks like a credential is redacted. A cache record is not a safe place for a
token.

## 5. Timestamps are not evidence

Inputs are content-hashed with BLAKE3, not compared by mtime and size. `make`
does the opposite and is fast because of it.

Mtime lost because it is wrong in the two cases that matter most: a checkout
or a branch switch rewrites mtimes without changing content, which turns every
`git checkout` into a full rebuild; and a file restored from an archive or
written twice inside one timestamp granularity changes content without
changing mtime, which is a false hit. False misses are annoying; false hits
are the failure mode that makes a cache untrustworthy.

The cost is that Arc hashes. That cost is bounded by narrowing — a complete
trace usually reduces "the project" to a few dozen files — and by the scale
numbers in [BENCHMARKS.md](BENCHMARKS.md) for the conservative path.

## 6. A trace is complete or it is worthless

"Complete" is not a quality score. It is a claim that the recorded set is
*everything* the execution could have depended on, and it is the only thing
that authorises Arc to fingerprint that set instead of the whole project.

So the bar is absolute. Any of these revokes it, and a revoked trace never
narrows: a syscall the backend does not model, including one a newer kernel
added; a read of `/proc`, `/sys`, `/dev/urandom` or another volatile path; a
socket connected, bound or sent on; a path that could not be resolved or is
not valid UTF-8; a process that could not be followed; the trace budget
overflowing; the tracer itself failing. The variants are
`Downgrade` in `crates/arc-core/src/trace/model.rs`.

The alternative was a confidence score — "mostly complete, 3 unknown
syscalls" — and letting the user decide. It was rejected because there is no
correct thing for a cache to do with 90% confidence. Either the recorded set
is authoritative or the project scan is. A score would have made Arc's most
important guarantee a matter of taste.

The visible cost is that unremarkable commands fall off the fast path.
Debian's `ls`, `mv` and `cp` probe SELinux through `/sys`, so they never
narrow. Arc reports that instead of pretending otherwise.

## 7. An unrecognised syscall is never assumed harmless

When the Linux backend meets a syscall number it has neither modelled nor
explicitly dismissed, it records `UnsupportedSyscall` and the trace stops
being complete. The dismissal list is written out by hand, syscall by syscall,
and an invariant test refuses to let a number appear on both lists
(`syscalls.rs`, `a_syscall_is_never_both_modelled_and_irrelevant`).

The alternative — default-allow, dismiss anything not recognised — is much
less work and is how a tracer becomes quietly wrong on a kernel it was never
tested against. `io_uring` is the concrete case: it can perform arbitrary file
I/O with no further syscall, so a default-allow tracer would report a complete
trace for a program whose entire I/O it did not see. Arc cannot observe
io_uring either, but it refuses to claim it did.

The cost is maintenance. Every kernel release can add a number that pushes
some workload off the fast path until the list is updated.

## 8. The compromise: an empty path argument is a question about a descriptor

This one is a genuine trade and it is stated as such.

Since glibc 2.33, `fstat(fd)` does not issue `SYS_fstat`. It issues
`newfstatat(fd, "", …, AT_EMPTY_PATH)`. Arc models `newfstatat` as a path
syscall, so what arrives is a path syscall whose path is the empty string,
against a descriptor that is very often a pipe or a terminal and therefore has
no name at all. Treating that as a path Arc failed to resolve makes the trace
partial, and since almost every program using stdio does it, that meant almost
nothing ever narrowed. See [BUGS.md](BUGS.md) for how that was found.

Arc now dismisses an empty path argument on metadata syscalls: no record, no
downgrade. That is consistent — `SYS_fstat` itself has always been on the
dismissed list as descriptor I/O — but it is a compromise, because the
dismissal is keyed on the path being empty rather than on `AT_EMPTY_PATH`
actually being set.

The alternative was to decode the flag properly, which means giving
`Sc::Stat` an `at_flags` field and threading the right argument index through
`newfstatat`, `statx` and `faccessat2` separately. That is the more precise
fix and it should happen.

The stated cost of the shortcut: a call that passes an empty path *without*
`AT_EMPTY_PATH` is dismissed too. Such a call always fails with `ENOENT`, and
what is lost is a negative dependency on a path that can never exist, so the
compromise is safe. It is still less precise than the code should be, and
the reason it is acceptable is written down rather than assumed.
