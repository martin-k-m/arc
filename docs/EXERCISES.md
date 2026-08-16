# Exercises

Questions I should be able to answer cold about Arc, without opening the source.
Ordered easy to hard. Answers are in
[EXERCISES-ANSWERS.md](EXERCISES-ANSWERS.md), in a separate file so this one can
be handed to someone else.

Every question is grounded in code that exists and in results from
[BENCHMARKS.md](BENCHMARKS.md), [BUGS.md](BUGS.md), [DECISIONS.md](DECISIONS.md)
or [LIMITATIONS.md](../LIMITATIONS.md).

---

## Part 1: ten questions

**1.** State what `TRACE COMPLETE` claims, and what it authorises Arc to do that
`TRACE PARTIAL` does not. Then say why it is a binary rather than a confidence
score.

**2.** Arc content-hashes inputs with BLAKE3 rather than comparing mtime and size.
`make` does the opposite and is faster for it. Give the two cases mtime gets wrong
and say which of the two decided it.

**3.** The warm speedups are 104x, 141x and 55x on click, tinycc and serde_json.
All three of those workloads trace partial. Explain how they hit at all, and say
what their hit rate would be if replayed over real project history.

**4.** On click's real history, 60 commits, three per-file test tasks: 23.3% hit
rate, all 180 traces complete, 177 of 180 narrowed. Explain what the three
non-narrowed runs are, and why one of the three tasks is excluded from the rate
rather than folded into it.

**5.** `arc run --trace cat input.txt` used to report `TRACE PARTIAL` with the
reason "a path argument could not be read back from the process", while
`/bin/true` traced complete. Name the syscall, the libc change behind it, and the
specific descriptor that made it fail.

**6.** That bug had been red in the suite the whole time and I read the suite as
green. Explain what the passing assertion next to it was asserting, and why a
cache hit is the wrong thing to assert on.

**7.** `date +%s%N` traces complete. `head -c 8 /dev/urandom` traces partial.
`python3 -c "random.random()"` traces complete. Explain the asymmetry and say why
it is not principled.

**8.** Debian's `ls`, `mv` and `cp` never narrow. Give the mechanism, and connect
it to why all three benchmark workloads trace partial.

**9. (design)** The two Linux backends disagree about Unix-domain sockets: seccomp
treats `AF_UNIX` and `AF_NETLINK` as local IPC, ptrace downgrades unconditionally.
Meanwhile the check that decides whether a learned dependency set may still be
used compares *capabilities*, not backend names. Design the fix. Say what the
capability model becomes, what happens to dependency sets already learned under
the other backend, whether a set learned under a more permissive backend may be
reused under a stricter one and vice versa, and what the differential suite has to
assert for "complete" to be backend-independent. Then say whether the current
situation is unsafe or merely imprecise, and defend the answer.

**10. (design)** Arc has one reproducible false hit: a dangling symlink whose
target later appears. Design the fix, and be specific about the three mechanisms
that combine to cause it (skipped canonicalisation with no downgrade, hashing the
link by its target path string, and `symlink_metadata` making a dangling link count
as present). Say what each observation should record instead, what new dependency
role you need if any, what it costs on the ordinary paths, and how you would test
it without a test that only checks the one reproduction from `LIMITATIONS.md`.

---

## Part 2: predict the failure

For each scenario, say what Arc does, and why. "Why" means the mechanism.

**Scenario A: a command spawns a detached background worker that reads an input
file three seconds after the command returns.**

```sh
setsid sh -c 'sleep 3; cat in.txt > /dev/null' < /dev/null > /dev/null 2>&1 &
exit 0
```

What verdict does the trace get, how long does `arc run` take, and what is
recorded about `in.txt`? What happens on the next run if `in.txt` has changed?
Then say why ordinary subprocesses are not affected, and name the mechanism that
makes them safe under each backend.

**Scenario B: the same project is cached by two developers, one on a machine where
the temp directory has an 8.3 short name and one where it does not, and the remote
cache is shared.**

What does the second developer observe? Does anything report an error? Name the
function at fault, the fallback that caused it, and say why the failure was
invisible to every test that ran locally.

**Scenario C: a command opens a file `r+b`, maps it `MAP_SHARED`, and stores a
byte through the mapping. Nothing else writes the file.**

What does Arc record for that file, and is the record correct? Now change the file
on disk afterwards: hit or miss? Name the thing Arc cannot observe here, the thing
that saves it, and say in which direction the residual imprecision errs. Then say
what the answer would be if the same I/O went through `io_uring` instead.

---

## Part 3: delete it and write it again

**Component: `crates/arc-core/src/key.rs`.**

Delete it and reimplement it from scratch. Keep the tests. It is one file, it has
no I/O beyond path resolution, and it is the file where the most expensive
portability bug in the project lived, so getting it right twice is worth the
exercise.

You are reimplementing:

- The **family** key and the **execution** key, and the split between them: the
  family covers what stays the same across runs of the same command, the execution
  key adds what the world looked like this time.
- What the execution key hashes: schema version, OS and architecture, program and
  arguments with boundaries preserved, working directory *relative to the project
  root*, input digest, environment digest, toolchain digest, dependency-set digest,
  declared output globs.
- `rel_cwd`, including the fallback for a working directory outside the project.
- Environment handling: variables hashed and never stored, and a variable whose
  name looks like a credential redacted.

**Verification.** A correct reimplementation passes:

```sh
cargo test -p arc-core
cargo test -p arc-cli --test remote
cargo test -p arc-cli --test redaction
```

The unit tests in the file are the semantics, and two of them are the regression
tests for bug #2: `a_root_reached_through_a_symlink_still_yields_a_relative_path`
asserts `rel_cwd` in both directions, resolved root against symlinked cwd and the
reverse, and `a_working_directory_outside_the_project_keeps_its_absolute_name` pins
the fallback to the case it is actually for.

The remote suite is the one that matters, because it is the only place a
machine-local key shows up as a symptom rather than as a wrong hash: it is a second
client failing to get a remote hit, which is always a legal thing for a cache to do
and therefore reports no error at all. If you can make the units pass and the
remote suite fail, you have reproduced the original bug.
