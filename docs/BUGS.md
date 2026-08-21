# Bugs

Defects I actually shipped, and what each one taught me. Every entry names
the commit that fixed it and the test that keeps it fixed. There are ten,
which is a thin history, and I would rather it read thin and true than long
and padded — nothing here is a hypothetical or a near miss. The tenth is the
odd one out: it is a defect in the tests rather than in Arc, it is still open,
and it says so.

The pattern across almost all of them is the same, and it is the reason I
keep this file: **the failure was silent**. Arc kept working. It cached, it
hit, it reported success. What it stopped doing was the specific thing it
exists to do, and only a test that asked the exact question could tell.

---

## 1. `fstat` looked like a path I could not resolve, and automatic narrowing quietly stopped working

**Symptom.** Nothing looked broken. `arc run` cached, hit, and reported
correct results. But `arc run --trace cat input.txt` said `TRACE PARTIAL`,
`dependency model partial`, `next run narrows no`, with the reason "a path
argument could not be read back from the process". So did `python3 -c
print(1)`. So did every `pytest` run. The headline feature — learn what a
command really reads, then fingerprint only that — was off for essentially
every real program, and Arc was silently doing the conservative
whole-project scan instead.

**Root cause.** Since glibc 2.33, `fstat(fd)` does not issue `SYS_fstat`. It
issues `newfstatat(fd, "", …, AT_EMPTY_PATH)`. I model `newfstatat` as a path
syscall — `Sc::Stat { dir, path }` in `trace/linux/syscalls.rs` — with no
flags field, so what arrived was a path syscall whose path argument was the
empty string, to be resolved against a descriptor. For a regular file that
resolved harmlessly. For stdout it did not: stdout is a pipe or a terminal,
`/proc/<pid>/fd/1` links to `pipe:[…]`, which is not a path, so the base was
unknown and both backends recorded `Downgrade::PathResolutionFailure`. One
downgrade makes a trace partial, and a partial trace never narrows.

Nearly every program that uses stdio calls `fstat` on its own stdout to
decide how to buffer. That is why `/bin/true` traced complete and `cat`
did not.

**How it was caught.** By the suite's own
`a_file_that_was_read_is_a_dependency_and_one_that_was_not_is_free`, in
`crates/arc-cli/tests/linux_trace.rs`. Its most valuable line is the one
that fails:

```rust
assert_hit(
    &sb.sh("cat input.txt"),
    "a file the execution never read cannot change its result",
);
```

That assertion *is* the product. It uses `cat`, which is exactly the smallest
command that triggers the bug, and it had been red on any glibc past 2.33.

What let it hide is worth stating, because it is the same shape as most of
this file. The assertion immediately before it is that an unchanged re-run
hits — and that one passed the whole time, because a whole-project scan hits
too when nothing has changed. Only the *unrelated change* case could tell the
difference between narrowing and not narrowing, and I had been reading a
mostly-green suite as a working feature.

**Fix.** `127aa2e` — an empty path argument on a metadata syscall is a
question about a descriptor, not about a name, so it is dismissed with no
record and no downgrade. `SYS_fstat` itself had always been on the dismissed
list as descriptor I/O; this is the same question through a different number.

**Regression test.**
`asking_about_a_descriptor_does_not_cost_the_trace_its_completeness`. It
asserts on `TRACE COMPLETE` rather than on a cache hit, deliberately —
asserting on a hit is what let the original bug survive.

**Compromise.** The dismissal keys on the path being empty rather than on
`AT_EMPTY_PATH` being set. [DECISIONS.md](DECISIONS.md) records why that is
safe and what it costs.

---

## 2. Two spellings of one directory made every execution key local to one machine

**Symptom.** The Windows remote-cache tests failed on CI and passed locally.
Every one of them was a second client failing to get a remote hit. Thirteen
tests, one cause, and nothing anywhere reported an error — the second machine
simply ran the command, which is always a legal thing for a cache to do.

**Root cause.** `rel_cwd` is what makes an execution key portable: two
checkouts of one repository at different absolute paths have to produce the
same family. It compared the working directory against the project root with
a plain `strip_prefix`, and fell back to the *absolute* working directory when
that failed. The project root arrives resolved; the working directory arrives
however the process was handed it. GitHub's Windows runners have an 8.3 short
temp path (`RUNNER~1`); macOS resolves `/var` to `/private/var`. On either,
the comparison failed, the fallback made every key machine-local, and
cross-machine reuse stopped happening.

**How it was caught.** The Windows remote-cache suite on CI, which is the
only place the two spellings occur naturally. It could not be reproduced
locally at all, which is the argument for running the suite on the platform
rather than on the platform you like.

**Fix.** `03fe436` — both sides are resolved before comparing. The whole
Windows suite then passed under a short-name temp: 379 tests, where 13 had
failed.

**Regression test.**
`a_root_reached_through_a_symlink_still_yields_a_relative_path` in
`crates/arc-core/src/key.rs`, which asserts `rel_cwd` in both directions —
resolved root against symlinked cwd, and the reverse — alongside
`a_working_directory_outside_the_project_keeps_its_absolute_name`, so the
fallback that caused the bug is still pinned to the case it is for.

---

## 3. Arc scanned its own database as project content

**Symptom.** macOS and Windows each failed one test that Linux passed. On
macOS the effect was a permanent cache miss: Arc could never hit, ever. On
Windows it was louder — the run failed outright with "another process has
locked a portion of the file".

**Root cause.** `ARC_HOME` and the project root reach Arc from different
places: one from the environment, one by walking up from the working
directory. They can name the same directory two ways — the same `/private/var`
and `RUNNER~1` conditions as above. `scan_inputs` compared its skip list using
a lexical `Path::starts_with`, which answers no to both, so an Arc home
sitting inside the project escaped the skip list and was scanned as project
content. Arc writes its database *during* the run, so the next fingerprint
could never match. On Windows the scan tried to hash a database the same
process had open.

**How it was caught.** A cross-platform test that was red on two platforms
and green on the third. My first instinct was that the test was
platform-sensitive; it was not, it was reporting a real defect, and the
difference between those two conclusions is the whole value of running CI on
more than one operating system.

**Fix.** `c2166d3` — both roots are resolved through the filesystem once, and
the skip list is reduced to project-relative prefixes so the per-entry test
inside the walk stays a string comparison. Canonicalising per entry would have
cost a syscall for every file in the project.

**Regression test.**
`an_arc_home_spelled_differently_is_still_recognised_as_arcs_own` in
`crates/arc-cli/tests/cli.rs`, which pins `--trace-backend snapshot`. That detail
matters: with complete tracing the fingerprint is narrowed and the Arc home is
never scanned, so the bug is invisible. The test has to force the conservative
path to be able to see it.

---

## 4. Fixing #3 broke the project root, and the fix reclassified project files as external

**Symptom.** None visible. That is what makes it worth writing down.

**Root cause.** The fix for #3 canonicalised both roots the classifier holds.
That was right for the Arc home and wrong for the project root: `classify` and
`relative` are handed *observed* paths, which keep whatever spelling the
program used. Normalising only the root makes `under` answer no whenever the
two disagree, and a project file that answers no to `under` is quietly
reclassified as external.

**How it was caught.** By reading the diff of my own fix the same day, not by
a test. I am recording it as a bug rather than as an amendment because it
shipped as a commit and because the honest version of "how it was caught" here
is "I got lucky" — no assertion in the suite distinguished a project input
from an external one by spelling.

**Fix.** `c2a33a7` — the Arc home carries its resolved name as an *additional*
key, checked alongside the given one rather than replacing it. Nothing that
matched before stops matching. The project root goes back to being compared as
given.

**Also in that commit**, and the more useful lesson: `--no-fail-fast` in CI.
Twice a real cross-platform bug had sat behind an unrelated failing test
binary, invisible until that one was fixed. One failure should not decide how
much of the suite gets to run.

---

## 5. A CI test inherited the CI it was running inside

**Symptom.** `a_working_tree_comparison_sees_uncommitted_and_untracked_work`
had been failing on every platform since at least 0.7.

**Root cause.** Not a bug in Arc. The test builds a throwaway repository and
asks Arc what it would compare. Run inside a real CI job it also inherits that
job's provider variables, so `GITHUB_SHA` names a commit the fixture has never
heard of. Arc then correctly refuses to guess and reports `unknown` — the
right answer to the question it was actually asked, which was not the question
the test meant to ask.

**How it was caught.** It was never hidden; it was long-running and tolerated,
which is worse. It got fixed when `--no-fail-fast` (see #4) made it stop
masking other failures.

**Fix.** `e5ae0ad` — the fixture clears the provider environment and each test
states the one it wants. Confirmed by reproducing the failure under a
simulated CI environment and watching it go green.

**Also in that commit.** The `ptrace forbidden` sandbox check grepped for the
word "backend", which a `doctor` rewrite had removed — so it passed on any
output containing that word, which is to say it had stopped testing anything.
Its premise was also stale: with ptrace denied, Arc no longer degrades, it
traces completely through seccomp. The step now asserts that, and a second one
denies both Linux backends so the snapshot fallback is still exercised.

**Regression test.** The provider-environment fixture in
`crates/arc-cli/tests/ci.rs`, plus the two rewritten sandbox steps in
`.github/workflows/ci.yml`.

---

## 6. The release archives and the script that installs them disagreed about their own shape

**Symptom.** Windows installation failed with "No such file or directory",
exit 127.

**Root cause.** The zip was built with `7z a "./$stage/*"`, which puts the
files at the archive root, while the tarball kept the
`arc-v<version>-<target>/` directory. Both install scripts and the smoke test
expect the directory.

**How it was caught.** By dry-running the release workflow before tagging.
This is the only bug in this file that was caught before a user could meet it,
and the only reason is that the release was rehearsed rather than trusted.

**Fix.** `bc2c36c` — one shape everywhere: the directory, so two extracted
archives cannot overwrite each other.

**Also in that commit.** The smoke test then failed on macOS for a better
reason. Its throwaway project had no `arc.toml`, so Arc walked up and adopted
the repository checkout as the project; the first run wrote into it, the
second correctly missed, and the test blamed the binary for working properly.
The project now carries a root marker and the log is written outside it.

---

## 7. A release job asked for a machine that no longer exists

**Symptom.** The first full release dry run sat for six hours with four of
five targets built and the fifth never scheduled.

**Root cause.** `macos-13` is retired. A job requesting a retired runner image
is not rejected — it is queued indefinitely.

**How it was caught.** By waiting, and then by noticing that waiting was the
symptom. There is no test for this and I do not think there can be one; the
mitigation is that the release was rehearsed at all.

**Fix.** `aae29d8` — `macos-15-intel`, which is the current Intel image.

---

## 8. A reader that stopped early turned Arc's own output into a panic

**Symptom.** The nightly `bench` workflow failed three nights running, at
`bench/environment.sh`, with exit code 101 and no error text. The script had
printed its host, toolchain and tracing sections first, so it looked like it
had finished its work and then died anyway.

**Root cause.** 101 is Rust's panic status. The script pipes `arc doctor` into
an `awk` that stopped at the section after `tracing` with `exit`, which closes
the read end while `doctor` still has the remote-cache and CI sections to
write. Rust ignores SIGPIPE, so that write returned `EPIPE`, and `println!`
panics on a failed write: `failed printing to stdout: Broken pipe (os error
32)`. `set -o pipefail` then made the panic the status of the script.

It is a race between `doctor` finishing its writes and `awk` reaching its
`exit`, which is why it was invisible for two days after the CI section landed
and why it does not reproduce on a fast machine: in a container here the old
pipeline wins 30 times out of 30, and on the four-core CI runner it lost three
nights out of three.

**How it was caught.** By the workflow, and only because it is scheduled.
Nothing in the test suite piped Arc's output anywhere.

**Fix.** `c5690a3` (`Die quietly when a reader stops early, instead of
panicking`). Two changes, because the script was only where it surfaced.

The script now clears its flag at the next section instead of exiting at it,
so `awk` always reads to EOF and no reader ever goes away early.

Arc now restores the default `SIGPIPE` disposition at startup. Rust ignoring
SIGPIPE is right for a library and wrong for a command: `arc doctor | head`
exited 101 and printed a panic where every other Unix command exits quietly.
It now dies on the signal, status 141, as `head` expects. The traced children
inherit the same disposition a shell would have given them, which is closer to
what Arc is trying to observe in the first place.

`a_reader_that_stops_early_does_not_panic` in `crates/arc-cli/tests/cli.rs`
reads one chunk of `arc doctor`, drops the pipe, and fails if Arc panicked or
exited 101. It fails without the startup change and passes with it.

**What it taught me.** The measurement harness is code, and it was the only
code here nothing tested. The bug was in Arc, not in the script: the script
just happened to be the one caller that stopped reading.

---

## 9. The tracer loop could exit with a child still stopped, and hang the whole command

**Symptom.** `arc run` hung for good, not slowly. One `linux_trace` suite run
took 36 minutes at 0% CPU. It was load-dependent and looked like flakiness:
under eight concurrent runs of the outliving-grandchild script from
[LIMITATIONS.md](../LIMITATIONS.md) §4 on two pinned CPUs, **12 hangs in 80**.

**Root cause.** The tracer loop stopped when the root had exited and its
`table` was empty, but `table` is not the whole process tree. A child announced
by its parent's `PTRACE_EVENT` but not yet stopped lives in `announced`, and
one that stopped before that event arrived lives in `orphans`. Either can be
outstanding while `table` is empty, so the loop returned with that child still
parked in signal-delivery-stop and nothing left running to release it.

What that cost was not the trace, it was the command. The stopped child still
held the write end of the command's stdout and stderr, so those pipes never
reached EOF, so the pump threads in `exec::run` blocked in `read` and the join
never returned.

**How it was caught.** By taking a stack instead of reading the code. `gdb` on
a hung run showed thread 1 in `JoinHandle::join` at `exec.rs:117`, threads 4
and 5 in `pump` at `exec.rs:207` blocked in `read`, no thread in `waitpid` at
all, and the tracee at state `t` with `TracerPid` pointing back at Arc. That
stack names the mechanism outright, which two rounds of reading the code had
not.

**Fix.** `580e1ce` (`Do not stop tracing while a child is still attached`). The
termination test now covers all three sets, and whatever is still attached when
the loop does finish is detached rather than abandoned. Same measurement after
the change: **0 hangs in 80**.

**Two earlier attempts, both wrong.** Releasing the orphan on the
`ChildEscape` path gave 21 in 80. Draining `orphans` at loop exit gave 16 in
80. Both were reasoned from the code rather than from a stack, both looked
right, and both were wrong about which set the stuck child was in. That is the
entry's real lesson: for a hang, get the stack and a measured rate before
editing anything, because a fix that moves 12 to 16 is indistinguishable from
noise if you are not counting.

**Regression test.** None dedicated, and that is a gap I am recording rather
than papering over. `a_process_that_outlives_the_command_costs_the_trace_its_completeness`
(`crates/arc-cli/tests/linux_trace.rs:473`) exercises the same shape and would
hang rather than fail if this regressed, but nothing asserts on the hang rate,
so a partial regression would show up as flakiness rather than as a failure.

---

## 10. Five trace tests depend on the host's `coreutils` being GNU

**Symptom.** `cargo test --release` fails 5 of the 35 tests in
`crates/arc-cli/tests/linux_trace.rs` on Ubuntu under WSL2, while the same
commit is green on CI:

```
a_file_that_was_read_is_a_dependency_and_one_that_was_not_is_free       FAILED
asking_about_a_descriptor_does_not_cost_the_trace_its_completeness      FAILED
graph::a_cycle_between_two_tasks_is_reported_and_still_schedulable      FAILED
graph::an_observed_write_and_read_form_an_edge_with_no_configuration    FAILED
a_process_that_outlives_the_command_costs_the_trace_its_completeness    FAILED
```

Every one of them reduces to the same assertion, that tracing `cat` produces a
complete trace, and the same recorded reason for why it did not:

```
  dependency model    partial
  not complete        read volatile path /proc/filesystems
```

**Root cause.** Not Arc. This Ubuntu ships **uutils coreutils 0.8.0** rather
than GNU coreutils, and `/usr/bin/cat` is a symlink to
`../lib/cargo/bin/coreutils/cat`, the uutils multi-call binary. That
implementation reads `/proc/filesystems`. Arc classifies `/proc` as a volatile
path, which is correct and is the documented behaviour in
[LIMITATIONS.md](../LIMITATIONS.md), so the trace is honestly reported as
partial. The tests assume the `cat` on the box does not touch `/proc`, which is
true of GNU coreutils and false here.

**How it was caught.** By building and running the suite on a machine that is
not the CI runner. It is worth recording that my first attempt to diagnose it
was wrong: I ran `strace` to prove `cat` never opened `/proc/filesystems`, got
a clean result, and nearly wrote this up as the tracer inventing a read.
`strace` was not installed, so the pipeline had been grepping an empty stream
and every check I based on it was vacuous.

What settled it was a positive control instead of a negative one. Compile a
three-line C program that opens the same file and nothing else, trace it, and
compare:

```sh
cc -O2 -o reader r.c
arc run --trace-backend ptrace --trace -- ./reader   # dependency model complete, 3 files read
arc run --trace-backend ptrace --trace -- cat input.txt  # partial, read volatile path /proc/filesystems
```

Both Linux backends agree, which also rules out a backend-specific fault:
`ptrace` and the seccomp backend independently record the same `/proc` read.

**Fix.** None applied. Arc is doing the right thing, so there is nothing to fix
in the tracer, and I am not going to weaken a test to make a machine happy. The
correct repair is to the fixtures: they should exercise the tracer with a
helper whose syscalls the test controls, rather than with whatever `cat` the
host distribution supplies. That is the same defect as
[#5](#5-a-ci-test-inherited-the-ci-it-was-running-inside), which was a test
inheriting the environment it ran in, and it is the third time in this file
that a test has asserted on the host rather than on Arc.

**Reproduction environment.** Ubuntu on WSL2, kernel
`6.18.33.2-microsoft-standard-WSL2`, glibc 2.43, rustc 1.97.1, uutils coreutils
0.8.0. The suite is green where `cat` is GNU.


---

## What I would do differently

Six of these ten were silent, and the two most serious — #1 and #2 — were
both a cache that had stopped doing the one thing it exists to do while
reporting success. That is the failure mode this project has to defend
against, and the defence is not more tests. It is tests that assert on the
*claim* rather than on the outcome: `TRACE COMPLETE` rather than `CACHE HIT`,
"the unrelated file did not invalidate" rather than "the run succeeded". A
green assertion that a correct-looking thing happened is compatible with the
feature being entirely switched off, and I have now shipped that twice.
