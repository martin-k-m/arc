# Exercises: answers

Answers to [EXERCISES.md](EXERCISES.md). Sources named so every claim is
checkable.

---

## Part 1

**1. What complete means.**

"Complete" is not a quality score. It is the claim that the recorded set is
*everything* the execution could have depended on, and it is the only thing that
authorises Arc to fingerprint that set instead of the whole project. A partial
trace never narrows: Arc falls back to hashing the project.

The bar is absolute, and any of these revokes it: a syscall the backend does not
model, including one a newer kernel added; a read of `/proc`, `/sys`,
`/dev/urandom` or another volatile path; a socket connected, bound or sent on; a
path that could not be resolved or is not valid UTF-8; a process that could not be
followed; the trace budget overflowing; the tracer itself failing.

Binary rather than a score because there is no correct thing for a cache to do with
90% confidence. Either the recorded set is authoritative or the project scan is. A
score would have made the most important guarantee a matter of taste.

**2. Content, not timestamps.**

Mtime is wrong in both directions. A checkout or a branch switch rewrites mtimes
without changing content, which turns every `git checkout` into a full rebuild. And
a file restored from an archive, or written twice inside one timestamp granularity,
changes content without changing mtime, which is a **false hit**.

The false hit decided it. False misses are annoying; false hits are the failure
mode that makes a cache untrustworthy, and Arc's whole value is being trusted
enough to skip work.

The cost is that Arc hashes with BLAKE3, bounded by narrowing (a complete trace
usually reduces "the project" to a few dozen files) and by the scale numbers for
the conservative path.

**3. Partial traces still hit.**

They hit by hashing the whole project and finding nothing changed. That is a legal
and useful cache, and the 104x, 141x and 55x numbers are real. It is just not the
dependency-learning cache the front page leads with.

Replayed over real history the hit rate would be **the fraction of commits that
change nothing at all**, which is close to zero, because any commit touching any
file in the project invalidates a whole-project fingerprint. That gap is what the
dependency model buys, stated as plainly as it can be.

**4. The click hit rate.**

The three non-narrowed runs are the **first run of each task**, which has nothing
learned yet. Narrowing needs a dependency set from a previous complete trace, so
run one of a task is always a whole-project fingerprint. This is the same subtlety
that makes the benchmark scripts do three runs before measuring a warm hit: the run
*after* learning is still a miss, because narrowing for the first time changes the
key.

`test_basic.py` is excluded because it returned a non-zero exit at every one of the
60 commits, and not because of Arc: pytest 9.1.1 removed support for passing an
iterator to `parametrize`. Arc never caches a non-zero exit, so those 60 runs could
only ever be misses. Folding them in would report a rate depressed by a broken test
file; deleting them would hide that a fifth of the sample was unusable. Both numbers
are reported for that reason.

Two things about the quality of the hits that matter more than the 23.3%: every hit
is a **narrowed** hit, meaning Arc decided the commit was irrelevant by checking a
learned dependency set rather than by hashing the project and finding it unchanged.
Before the `fstat` fix, none of them narrowed.

Worth also knowing why the tasks are per-file. Running the whole suite as one task
would have measured how often a click contributor pushes a commit touching nothing
the tests read, which is a fact about click's contributors. It also would not have
narrowed at all: the whole suite shells out to enough tools that it reads
`/sys/fs/selinux` and `/proc/mounts`. The granularity of your tasks decides whether
Arc's central feature works on them at all.

**5. The `fstat` bug.**

Since glibc 2.33, `fstat(fd)` does not issue `SYS_fstat`. It issues
`newfstatat(fd, "", …, AT_EMPTY_PATH)`. Arc models `newfstatat` as a path syscall,
`Sc::Stat { dir, path }`, with no flags field, so what arrived was a path syscall
whose path argument was the empty string, to be resolved against a descriptor.

For a regular file that resolved harmlessly. For **stdout** it did not: stdout is a
pipe or a terminal, `/proc/<pid>/fd/1` links to `pipe:[…]`, which is not a path, so
the base was unknown and both backends recorded `Downgrade::PathResolutionFailure`.
One downgrade makes a trace partial, and a partial trace never narrows.

Nearly every program using stdio calls `fstat` on its own stdout to decide how to
buffer. That is why `/bin/true` traced complete and `cat` did not, and why the
headline feature was off for essentially every real program.

The fix dismisses an empty path argument on a metadata syscall: no record, no
downgrade, consistent with `SYS_fstat` itself having always been on the dismissed
list as descriptor I/O. The compromise is that the dismissal keys on the path being
empty rather than on `AT_EMPTY_PATH` being set. A call passing an empty path without
the flag always fails with `ENOENT`, so what is lost is a negative dependency on a
path that can never exist. Safe, less precise than the code should be, and written
down as such.

**6. Why I read the suite as green.**

The failing assertion was
`a_file_that_was_read_is_a_dependency_and_one_that_was_not_is_free`, and the line
that failed was:

```rust
assert_hit(
    &sb.sh("cat input.txt"),
    "a file the execution never read cannot change its result",
);
```

The assertion immediately before it is that an unchanged re-run hits, and that one
passed the whole time, **because a whole-project scan hits too when nothing has
changed**. Only the unrelated-change case can tell the difference between narrowing
and not narrowing.

That is why a cache hit is the wrong thing to assert on. A green assertion that a
correct-looking thing happened is compatible with the feature being entirely
switched off. The regression test asserts on `TRACE COMPLETE` rather than on a hit,
deliberately. Six of the seven bugs in `BUGS.md` were silent, and the defence is not
more tests, it is tests that assert on the *claim* rather than on the outcome.

**7. Determinism.**

Arc observes the filesystem and nothing else. `/dev/urandom` is caught because it is
a **file**, and files are what Arc watches. `getrandom(2)`, which is how glibc and
CPython actually seed themselves, is on the explicitly dismissed syscall list
alongside `clock_gettime`, `gettimeofday`, `getpid` and the `sched_*` family. So a
program that reads randomness the old way loses its completeness claim and a program
that reads it the modern way does not.

Not principled: nothing about `/dev/urandom` being a path makes the program more
cacheable than one calling `getrandom`. The reasoning in the source is that a
command depending on the clock is non-hermetic in a way no filesystem tracer can
repair, so the limit is documented rather than turned into a downgrade on every run.
That is defensible, and it is not the same as detecting nondeterminism.

Practical consequence: a command embedding a timestamp, a random seed, a process id
or a hostname in its output gets cached once and served forever, with the trace
called complete. `[trace] enabled = false` or not caching that command are the only
remedies.

**8. Why `ls` never narrows.**

Volatile paths. The policy is one function, `policy::verdict`. Anything under
`/proc/` other than `/proc/self`, `/proc/thread-self` and `/proc/<pid>`, anything
under `/sys/`, and anything under `/dev/` other than an exact-match harmless-device
list (`null`, `zero`, `full`, `tty`, `console`, `ptmx`, `stdin`, `stdout`, `stderr`)
is a volatile read and makes the trace partial.

On a stock Debian system `ls`, `mv` and `cp` probe SELinux through `/sys` and read
`/proc/mounts` and `/proc/filesystems`.

Same mechanism on all three benchmark workloads: `cargo test` reads
`/sys/fs/cgroup/cpu.max`, `/proc/sys/vm/overcommit_memory` and `/dev/urandom`;
`pytest` reads `/sys/fs/selinux` and `/proc/mounts`; `make` reads `/dev/urandom`
through GCC. That is why all three trace partial and none of them narrow, and it is
the dominant reason real builds fall off the fast path.

Two sharp edges in the same policy: the harmless-device list is an exact match after
`/dev/`, so `/dev/shm/...`, `/dev/tty1` and `/dev/ttyS0` are all volatile; and the
prefix test is on the literal strings, so a procfs bind-mounted elsewhere, or a
chroot, is invisible to it.

**9. (design) Backend capability disagreement.**

*The current situation.* Both backends advertise identical capabilities, and the
reuse check compares capabilities rather than backend names, so a set learned under
seccomp is considered valid for a ptrace run and vice versa, while the two disagree
about whether the execution that produced it was fully observed.

*Safe or imprecise?* **Imprecise, not unsafe, in the current direction only.** The
disagreement makes ptrace strictly more conservative: it downgrades on
`AF_UNIX`/`AF_NETLINK` where seccomp does not. A more conservative backend produces
fewer complete traces and therefore fewer narrowed hits, so the cost is hit rate,
not correctness. But that is a property of which backend happens to be stricter
today, not a property of the design, and the moment the asymmetry runs the other way
it becomes a correctness bug. The right answer says both halves.

*The capability model.* Capabilities have to become an *observation* set rather than
a flag: what classes of dependency this backend can see, and what classes it treats
as revoking completeness. Socket address families belong in that set explicitly, as
do the volatile-path policy version and the modelled-syscall list version. Two
backends with identical observation sets are interchangeable; two with different
ones are not.

*Reusing sets across backends.* A dependency set learned under backend A may be
reused under backend B only when B's observation set is a superset of A's, which is
the direction that cannot lose a dependency. A set learned under a *stricter*
backend is safe to reuse under a more permissive one, because the stricter backend
would have refused to call it complete unless it saw everything. The reverse is not.
The tempting shortcut, reuse whenever the sets are non-empty, is exactly the bug.

*What the differential suite must assert.* Not that both backends produce the same
verdict on a corpus, which is what it can do today, but that for every case in the
corpus the two produce the **same recorded dependency set and the same downgrade
set**, with any difference being a test failure rather than a note. `trace_differential.rs`
is the right home. The unix-socket case should be a named case in it, currently
failing, rather than a paragraph in `LIMITATIONS.md`.

*The immediate fix* is smaller than all of that: `backend.rs` computes `family` and
then does `let _ = family;`. Making ptrace use it is a few lines. The design work is
making sure the next divergence cannot be silent.

**10. (design) The dangling symlink.**

*The three mechanisms.* Symlink expansion canonicalises each candidate and **skips
it when `canonicalize()` fails, with no downgrade recorded**, so a dangling link's
target is never added as a dependency. The link itself is hashed by its **target
path string**, which does not change when the target appears. And existence probes
use `symlink_metadata`, i.e. `lstat` semantics, so a dangling link counts as
**present**, which is why it becomes an input rather than a negative dependency on
the target.

Together: `ln -sf missing.txt link.txt`, a script that tests `-e link.txt`, learn,
then `printf 'appeared' > missing.txt`, and the next run is a CACHE HIT replaying
"no".

*What each should record instead.* A failed canonicalisation is not nothing. The
unresolved target should be recorded as a **negative dependency** on the path the
link points at, so its later appearance is a miss. That is the same `Role::Existence`
mechanism that already exists for "the command looked for `optional.cfg` and did not
find it", so no new role is needed; the bug is that the symlink path does not reach
it. The link itself should keep being hashed by its target string, which is correct
and catches repointing. And the `lstat` semantics should stay, because a dangling
link genuinely is present as a link; the fix is to add the negative dependency, not
to change what the link counts as.

*Cost on the ordinary paths.* Near zero: the failing `canonicalize()` call already
happens, so this adds a record on a path that currently adds nothing. The one real
cost is record count on projects with many broken links, which is bounded by the
existing 20,000 absent-path budget.

*Testing it.* Reproducing the one case from `LIMITATIONS.md` is necessary and not
sufficient, and this is the whole lesson of `BUGS.md`. The cases that must be
asserted are the matrix: target appears (miss), target appears and is then removed
again (miss, then the original answer is legitimately reusable), link repointed at
another missing path (miss), link repointed at an existing path (miss, already
covered), chain of two links where the middle one dangles, and a link inside an
enumerated directory. And each should assert on the recorded dependency set, not
only on hit or miss, because a hit for the right reason and a hit for the wrong
reason look identical from outside.

---

## Part 2

**Scenario A: the detached worker.**

The trace reports **`TRACE COMPLETE`**, `arc run` returns in **54 ms**, and
**nothing is recorded about `in.txt`**, because the grandchild has not opened it
yet. On the next run with `in.txt` changed, Arc **hits** when it should miss.

This is measured, and it is the honest limit: Arc does not wait for processes that
outlive the command, and the trace still claims completeness. If that worker's reads
genuinely affect the result, for instance a daemon writing a file the next command
consumes, Arc has no record of the dependency.

Ordinary subprocesses are fine, and the distinction is *outliving*, not *descending*.
Children and grandchildren are followed correctly: a file read only by
`sh -c "cat in.txt"` inside the traced command is recorded as a dependency and the
trace is complete. ptrace follows them through
`PTRACE_O_TRACEFORK`/`VFORK`/`CLONE`; the seccomp filter is inherited across `fork`,
survives `exec`, and cannot be removed.

Related, and the safe direction: `clone3` passes its flags in a struct rather than a
register, so Arc cannot read them and models the child as sharing nothing with its
parent, which duplicates state rather than losing it.

**Scenario B: two spellings of one directory.**

The second developer observes **a miss**, and **nothing reports an error**, because
running the command is always a legal thing for a cache to do. That is what made
this bug expensive: thirteen tests failing on CI, one cause, and no diagnostic
anywhere.

The function is **`rel_cwd`**, which is what makes an execution key portable: two
checkouts of one repository at different absolute paths have to produce the same
family. It compared the working directory against the project root with a plain
`strip_prefix`, and **fell back to the absolute working directory** when that
failed. The project root arrives resolved; the working directory arrives however the
process was handed it. GitHub's Windows runners have an 8.3 short temp path
(`RUNNER~1`), and macOS resolves `/var` to `/private/var`. On either, the comparison
failed, the fallback made every key machine-local, and cross-machine reuse stopped.

Invisible locally because the two spellings only occur naturally on those platforms.
It could not be reproduced on a development machine at all, which is the argument for
running the suite on the platform rather than on the platform you like. The fix
resolves both sides before comparing; the whole Windows suite then passed under a
short-name temp, 379 tests where 13 had failed.

A near neighbour worth remembering: the same two-spellings condition caused Arc to
scan **its own database** as project content, because `scan_inputs` compared its skip
list with a lexical `Path::starts_with`. On macOS that was a permanent cache miss,
on Windows a hard failure with "another process has locked a portion of the file".
And the fix for *that* broke the project root, silently reclassifying project files
as external, because `classify` and `relative` are handed observed paths that keep
whatever spelling the program used. Canonicalising a root you compare against
observed paths is the general trap.

**Scenario C: mmap.**

Arc records **both `read` and `write`** on the file, and the record is correct.
Changing the file afterwards is a **MISS**, correctly. Both measured.

What Arc cannot observe is the store through the mapping: a write through a
`MAP_SHARED` mapping produces **no syscall at all**, and `mmap`'s `prot` argument is
not even decoded, since `Sc::Mmap` captures only the descriptor and the flags.

What saves it is **the open flags**, and this is luck rather than design: to write
through a mapping you must have opened the file writable, and Arc records
`FileOp::Write` from the open mode before any mapping exists.

The residual imprecision errs **safe**: the record of what changed comes from intent
rather than from observation, so a file opened writable and never actually written is
recorded as written. That over-records an output, which costs precision, not
correctness. Reading a file through `mmap` is recorded correctly and produces a
complete trace.

`io_uring` has no such backstop, and Arc's answer is to refuse rather than to guess.
`io_uring_setup` and `io_uring_enter` are deliberately neither modelled nor
dismissed, so they land in the unsupported-syscall path and **revoke completeness**.
Arc cannot see io_uring I/O and does not claim it did. That is the concrete case
behind the "an unrecognised syscall is never assumed harmless" decision: io_uring can
perform arbitrary file I/O with no further syscall, so a default-allow tracer would
report a complete trace for a program whose entire I/O it did not see.

---

## Part 3: the key reimplementation

Notes for whoever does it.

**The split is the load-bearing idea.** The family key covers what stays the same
across runs of the same command; the execution key adds what the world looked like
this time. Without that split there is nowhere to hang a learned dependency set: Arc
has to find "what did this command depend on last time" before it can know which
inputs to hash, and the thing it looks that up by cannot itself contain the inputs.
A reimplementation that hashes everything into one key will pass a surprising number
of tests and will have destroyed narrowing.

**Relative, not absolute, working directory.** Hashing the absolute path is simpler
and is what Arc did originally, and it makes every key local to one machine so a
shared cache never hits across checkouts. Resolve both sides before comparing. Keep
the fallback for a genuinely outside-the-project working directory, and keep it
pinned to that case by its own test, because the fallback is not the bug; reaching
it by accident was.

**Argument boundaries are preserved** in the hash. Concatenating argv with a
separator that can appear inside an argument makes two different commands collide,
which is a false hit.

**Environment variables are hashed, never stored**, and a name that looks like a
credential is redacted. A cache record is not a safe place for a token, and
`redaction.rs` is the suite that says so.

**Env vars are not observed, only listed.** A syscall tracer cannot see a memory
read and `getenv` is a memory read, so a curated list is hashed into the key
(`PATH`, `LANG`, `CC`, `CFLAGS`, `RUSTFLAGS`, `PYTHONHASHSEED` and about a dozen
more), extended per project with `[env] include`. Know that this is a limitation
with a fix that is on the user rather than on Arc: name the variable.
