# Bugs

Defects I actually shipped, and what each one taught me. Every fixed entry
names the commit that fixed it and the test that keeps it fixed. There are
twelve, which is a thin history, and I would rather it read thin and true than
long and padded — nothing here is a hypothetical or a near miss. The last two
are the odd ones out. #10 was a defect in the tests rather than in Arc, and is
now fixed. #11 and #12 are failures I have seen but not explained, so they carry
no fix and no root cause, and they say so. #14 is a failure I can reproduce on
demand and have not explained, which is a better position than those two and
still not a fix.

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

**Fix.** The fixtures, not the tracer and not the assertions. Nothing in Arc
changed and no test was weakened. Each of the five now exercises the tracer with
something whose syscalls the test controls:

- Four only needed *a* file read, so they read with shell builtins, which touch
  the file and nothing else.
- `asking_about_a_descriptor_does_not_cost_the_trace_its_completeness` could not
  take that route. Its point is that a stdio program issues
  `newfstatat(fd, "", AT_EMPTY_PATH)`, and a builtin loop would have made it pass
  while testing nothing. It compiles a C fixture that calls `fstat(1)` outright,
  so the syscall is issued by construction.

Finding the second one cost a wrong guess of its own. After replacing `cat`,
`a_process_that_outlives_the_command_costs_the_trace_its_completeness` still
reported the same `/proc/filesystems` read, and I assumed `setsid` was the
culprit. It is not: `setsid` here is util-linux 2.41.3 and traces clean. The
reader was **`sleep`**, which is coreutils and so is also uutils on this box.
`arc run --trace sh -c "sleep 1"` reproduces it on its own. That fixture now
waits and reads in C as well.

Verified both ways: 35 of 35 pass on the uutils machine, and 35 of 35 in a
`rust:1-bookworm` container with GNU coreutils 9.1, so the repair did not simply
move the host dependency somewhere else. That is the same defect as
[#5](#5-a-ci-test-inherited-the-ci-it-was-running-inside), which was a test
inheriting the environment it ran in, and it is the third time in this file
that a test has asserted on the host rather than on Arc.

**Reproduction environment.** Ubuntu on WSL2, kernel
`6.18.33.2-microsoft-standard-WSL2`, glibc 2.43, rustc 1.97.1, uutils coreutils
0.8.0. The suite was green where `cat` is GNU and red here; it is now green on
both.


---

## 11. A traced `connect` recorded a path truncated to the project root

**Status: open.** Reproduced once, not yet reproduced on demand, and the
mechanism below is a hypothesis fitted to a single failure rather than
something I have demonstrated.

**Symptom.** `both_backends_say_the_same_thing_about_a_unix_socket` in
`crates/arc-cli/tests/trace_differential.rs` failed during a full-workspace
run:

```
thread 'both_backends_say_the_same_thing_about_a_unix_socket' panicked at
crates/arc-cli/tests/trace_differential.rs:475:9:
linux-ptrace did not record the socket's absence: {""}
```

**What the empty string actually is.** Not an empty path. The test relativises
every path against the project root in `Learned::of`, and a path *equal* to the
root strips to `""`. So the absent set held exactly one entry and that entry was
the project root.

That matters because a healthy run holds exactly one entry too, and it is the
socket. Printed from a passing run:

```
DIAG backend=linux-ptrace   complete=true absent={"not-there.sock"}
DIAG backend=linux-seccomp  complete=true absent={"not-there.sock"}
```

So the connect was not missed and the set was not emptied. One path was
recorded, and it was the socket path cut off exactly where the project root
ends, losing the trailing `/not-there.sock`.

**Why it cannot be an empty readback.** `read_unix_path` in
`trace/linux/sys.rs` ends with `(!name.is_empty()).then(...)`, so it returns
`None` rather than an empty path, and a `None` there records nothing at all.
The empty string cannot have come from it. An earlier version of this entry
claimed the readback "returned, and what it returned was empty"; that was wrong
and is the reason this entry now leads with where the string comes from.

**Hypothesis, untested.** The name is bounded by the syscall's `addrlen`
argument:

```rust
let want = (len as usize).min(2 + 108);
if read_into(pid, addr, &mut buf) != want { return None; }
let end = path.iter().position(|b| *b == 0).unwrap_or(path.len());
```

If `len` arrives as `2 + root.len()`, the buffer stops at the root boundary, no
NUL is found, `end` falls back to `path.len()`, and the result is the root
exactly. Every observed detail follows from a short `len`, including the run
still being called complete: a truncated but non-empty path is a *successful*
readback, not a `path_resolution_failure`, so nothing downgrades. That would
make this a syscall-argument read taken at the wrong moment rather than
anything about sockets — which would also fit its load sensitivity.

I have not tested this. It is the first thing to check if it reproduces.

**Nothing can be said about the fast backend.** The loop runs
`Selection::Ptrace` first and the panic ends the test, so `linux-seccomp` never
executed in the failing run.

**Frequency.** Failed once in two `cargo test --workspace --no-fail-fast` runs;
460 passed and 1 failed of 461 in the failing run, 461 passed in the other.
Passed three times out of three run alone in the same container, and CI has
passed it since. In the failing run the sibling case
`a_thousand_sessions_leak_nothing` had been going for over sixty seconds, so
the machine was tracing hard, but that is timing rather than a demonstrated
cause.

**Why CI does not catch it.** `.github/workflows/ci.yml` runs
`cargo test -p arc-cli --test trace_differential` as its own step, which is the
isolated configuration that passes. `.github/workflows/release.yml` runs
`cargo test --workspace --all-features --no-fail-fast`, which is the shape that
failed, and it passed on 2026-08-22.

**Reproduction attempt: CPU load is not it.** Twelve runs of this case alone in
the container, six idle and six with four `yes` processes saturating all four
CPUs. Zero failures in both arms, and every run recorded
`absent={"not-there.sock"}` on both backends. Wall-clock pressure on its own
does not produce it.

That is a useful negative, because it leaves one variable untested. Every run
that has ever passed used a test filter, so twelve of the thirteen cases in the
binary were filtered out and nothing else in the process was spawning children.
The only run that failed had all thirteen running at once alongside the rest of
the workspace. The suspicion therefore moves from load to concurrent siblings,
and the file already names the hazard: ptrace reaps with `waitpid(-1, __WALL)`,
which is process-wide, so any other thread in this binary spawning a child can
have its status stolen. The reaper lock in the test serialises tracing sessions
against each other; it does not cover a child spawned by anything else.

**Reproduction attempt: concurrent siblings are not it either.** Ten iterations
of the whole binary unfiltered, all thirteen cases each time, alone in the
container. Zero failures, and the case ran on both backends every iteration.
So the twelve neighbours inside the process do not produce it, and the
`waitpid(-1)` suspicion above is not supported by anything I have measured.

**Where that leaves it.** Two theories proposed and two knocked down. The only
configuration that has ever failed is a full `cargo test --workspace`, where
other test binaries run as separate processes at the same time, and that has
been seen exactly once.

Eighteen further workspace runs did not reproduce it, ten of them with the
instrumentation below in place, so the standing count is one occurrence in
twenty full workspace runs. That is a denominator rather than a rate
I would defend: a single event against ten trials bounds it loosely and says
nothing about the mechanism. The 0/10 unfiltered runs above do not prove the
configuration matters either, only that that configuration did not fail ten
times.

Those same eight runs did surface a different intermittent failure in the same
configuration — a cache hit that re-ran, in `taskgraph.rs` — which is recorded
separately as [#12](#12-a-task-that-should-have-hit-the-cache-re-ran-under-a-full-workspace-run).
Whether the two share a cause is unknown, and I have not assumed they do.

**What would settle it.** Loop the full workspace run itself, which is the only
shape that has ever failed, and accept that it costs minutes per iteration and
may need many. With a reproduction in hand, log `len` and the raw sockaddr
bytes inside `read_unix_path` and see whether `len` is short. Without one, the
hypothesis above stays a hypothesis, and this entry stays open.

**Found since: ptrace read the sockaddr at the wrong stop.** Not the hypothesis
above, and not proof of it either, but a defect in its own right and the only
one of the two backends that can have it.

`prepare_entry` exists for work that can only be done before a syscall runs,
and it held two cases: whether a path existed before a creating open, and the
image of an `execve` that will never return. The `sockaddr` of a `connect`
belonged there and was not there. The pointer was captured at the entry stop
and the *memory it points at* was read at the exit stop, through
`process_vm_readv`, after the syscall was over.

By then the thread that made the call is stopped and its siblings are not. Arc
traces every thread, but a thread doing nothing but writing to memory makes no
syscalls and so is never stopped: it can rewrite that buffer while the tracer
is reading it. What comes back is whatever is there now, which is a different
question from what the syscall was asked to connect to. A shorter string there
reads back as a perfectly plausible path that was never connected to, and a
truncated-but-non-empty readback is a *successful* one, so nothing downgrades
and the trace stays complete. That is the exact shape of the failure: one path
recorded, complete, wrong.

The seccomp backend never had this. It is notified *before* the syscall runs
and reads the address then, which is also why the failing run said nothing
about it -- and why the two backends could disagree at all.

The read now happens in `prepare_entry` and the result is carried on `Proc` as
`pending_connect`, the same way `pending_exec` is. This makes the class
impossible rather than unlikely: the buffer is read at the only moment its
contents are guaranteed, which is the moment the kernel itself reads it.

**It is still not a reproduction, and this entry stays open.** A test was
written to summon the race -- `connect` called through `ctypes` so the sockaddr
is a Python buffer, with a second thread rewriting it between a decoy path and
the real one, four hundred times -- and it **passed against the old code**. It
was deleted rather than kept: a test that passes with and without the fix
proves nothing and would tell the next reader it was covered. The window
between the kernel finishing the call and the tracer reading is evidently too
small for a Python thread to win reliably, and a C helper would be needed to
close it. So the mechanism above is demonstrated by construction, not by
observation, and whether it is what happened on 2026-08-22 is still unknown.

**Reproduction environment.** Arc at `70e7be1`. Debian 13 container on Docker
29.6.2, run with `--cap-add=SYS_PTRACE --security-opt seccomp=unconfined`, four
CPUs, kernel `6.18.33.2-microsoft-standard-WSL2`, glibc 2.41, rustc 1.97.1. Both
Linux backends reported available by `arc doctor`.

---

## 12. A task that should have hit the cache re-ran under a full workspace run

**Status: open.** Seen once, not diagnosed, and no reproduction attempt has been
made yet beyond the runs described here.

**Symptom.** `an_upstream_cache_hit_still_lets_downstream_proceed` in
`crates/arc-cli/tests/taskgraph.rs` failed on the eighth of eight consecutive
`cargo test --workspace --no-fail-fast` runs:

```
crates/arc-cli/tests/taskgraph.rs:744
assertion `left == right` failed
  left: "ran"
 right: "hit"
```

The summary it asserts against shows nothing hit at all:

```
{"blocked":0,"duration_ms":1270,"failed":0,"hits":0,"ran":3, ...}
```

All three tasks ran, each with `"cache_source":null` and `"outcome":"ran"`, on a
run where the upstream task was expected to be served from the cache.

**Why it is worth an entry.** Every other defect in this file that mattered was
a cache quietly not doing its job while reporting success, and this is that
shape again: the work completed, the exit codes were zero, and the only thing
that went wrong was the cache being skipped. It was caught because the test
asserts on the *claim* — `outcome == "hit"` — rather than on the run succeeding.
That is the discipline the closing section of this file argues for, and it is
the reason this one was visible at all.

**Root cause.** Not established. Nothing has been ruled out: a key that differs
between the producing and consuming run, a cache directory shared with or
cleared by a concurrent test, or an upstream trace that came back incomplete and
so was never cacheable, are all still open. The failing summary records a
`family_key` per task, which is where I would start.

**Frequency.** Once in twenty full workspace runs, ten of them with the
instrumentation below in place. Not seen in the other nineteen.

**Reproduction attempt: the binary alone does not fail.** Twenty iterations of
the whole `taskgraph` binary in the container, ten idle and ten with four `yes`
processes saturating all four CPUs. Zero failures in both arms. Its own
concurrent cases — `concurrent_scheduler_runs_do_not_corrupt_the_graph`,
`independent_tasks_run_concurrently` — do not provoke it, and neither does CPU
pressure.

**The one thing #11 and #12 share.** Both have been seen only during
`cargo test --workspace`, and neither reproduces when its own binary is looped
alone: #11 survived ten unfiltered runs, #12 twenty. What the workspace run adds
is other test binaries executing as separate processes on the same four cores.
That is a boundary condition, not a cause, and it is the same for two failures
that otherwise have nothing in common. I am recording the coincidence without
claiming it explains either.

**The instrumentation to apply, written but never triggered.** Ten workspace
runs were done with the failing assertion wired to dump, at the moment it
fails: the populating run's status and output, `arc cache list --limit 50`
taken after the populate and before the scheduled run, and the summary. That is
the fork worth capturing. An empty cache list means the populating run never
cached at all, and the wrong-key theory dies with it; an entry present with
`hits 0` means the cache was there and was not consulted, and comparing its key
against the summary's `family_key` becomes the next question. None of it
printed, because none of the ten runs failed.

The same loop carried a hook in `read_unix_path` for
[#11](#11-a-traced-connect-recorded-a-path-truncated-to-the-project-root),
behind `ARC_INSTR_SOCK`, logging `len`, `want`, `end` and whether a NUL was
found. Also silent, for the same reason.

**What would settle it.** Not more unattended loops. Twenty workspace runs have
now produced two failures between the two entries and no diagnostic output from
either, because the instrumentation only speaks when the assertion fails. The
cheap version of this is to leave the dump in place behind an environment
variable and let CI's own `release.yml` workspace run carry it, so the next
natural occurrence is captured instead of hunted.

**The workspace configuration is flaky on a second machine, and not always in
the same place.** Two `cargo test --workspace --no-fail-fast` runs in a `rust:1`
container on aarch64 Docker (four CPUs, emulated) failed three tests each, and
the sets were not identical:

| run | failed |
|---|---|
| baseline `e3fb1e3` | `two_ci_runs_sharing_one_cache_do_not_corrupt_it`, `concurrent_traced_runs_stay_independent`, `independent_tasks_run_concurrently` |
| the same tree plus the #11 fix | `concurrent_traced_runs_do_not_corrupt_dependency_metadata`, `concurrent_traced_runs_stay_independent`, `independent_tasks_run_concurrently` |

Every one of them is a concurrency test, and none is this entry's case. What
that adds is a caution about the framing above: "only ever seen under a full
workspace run" reads like a property of the two defects, and this says the
configuration is simply where contention lives. A machine slow enough will fail
*something* there.

One of the three was a test-design problem rather than a defect, and is fixed.
`independent_tasks_run_concurrently` asserts that two leaves genuinely overlap
under `--jobs 4`, and each leaf watched for the other for **one second** before
giving up. One second is a guess about a machine: starting a traced child in an
emulated container costs more than that, so both leaves gave up before either
had announced itself and the test failed reporting "alone, alone" -- a scheduler
that had done its job exactly right.

Measured before and after, in the same container. Before: failed idle, and three
times out of three with six `yes` processes on four CPUs. After: passes idle,
and three times out of three under the same load. It still fails when
concurrency is genuinely absent -- pointing the concurrent half at `--jobs 1`
reproduces "alone, alone" -- so it has not been made vacuous.

The watch is now budgeted from a file the test writes before each run: thirty
seconds where there should be something to see, one where there should not. A
leaf stops watching the moment it sees its peer, so the large budget costs a
machine that really did overlap them nothing, and the cost only lands where
there is nothing to find. The serial half keeps the short budget because arc
has not started the peer and will not until the task exits.

That leaves two of the three failures above unexplained, and they remain
concurrency tests failing under contention.

Whether the same contention explains #11 and #12 is still not established, and
these runs did not reproduce either of them.

**Reproduction environment.** Arc at `70e7be1` plus a diagnostic print in an
unrelated test file. Debian 13 container on Docker 29.6.2, run with
`--cap-add=SYS_PTRACE --security-opt seccomp=unconfined`, four CPUs, kernel
`6.18.33.2-microsoft-standard-WSL2`, glibc 2.41, rustc 1.97.1. That iteration
reported 460 passed and 1 failed of 461.

---

## 14. Six concurrent `arc run` invocations, and one of them cannot open the database at all

**Status: open.** Reproduced on demand, mechanism only partly established, and
no fix attempted here.

**Symptom.** `concurrent_traced_runs_stay_independent`
(`crates/arc-cli/tests/linux_trace.rs`) and
`concurrent_traced_runs_do_not_corrupt_dependency_metadata`
(`crates/arc-cli/tests/dependency.rs`) both fail in a container run of the
workspace. Both spawn six `arc run` processes against one `ARC_HOME` and assert
each exits zero. One does not:

```
concurrent traced run 1 exited 1
--- stderr ---
◆ Arc could not open its metadata database.
  /tmp/.tmpGE8CVb/archome/arc.redb
Database already open. Cannot acquire lock.
```

**This is Arc failing a command, not a test being fussy.** Running the same
project's commands concurrently is the ordinary case — `make -j`, a watch loop,
two terminals — and `arc run` is documented as safe under it. `db.rs` opens with
the comment that "short critical sections plus bounded retry is what makes two
concurrent `arc run` invocations safe rather than corrupt". The retry is
`LOCK_TIMEOUT`, twenty seconds, polled every fifteen milliseconds with no
jitter. One of the six exhausts it.

**What is established.**

- It reproduces readily in this container. Across two separate measurements of
  the same case run alone: **four failures in three iterations** of the two
  cases together, and **two failures in three** of
  `concurrent_traced_runs_stay_independent` on its own. Nothing else was running
  on the machine. `docs/BUGS.md` #12 records these same two tests failing once each in
  a workspace run and never reproducing; they reproduce here readily.
- The losing child is not a fixed one. Children 0, 1 and 2 have each been the
  one that failed, so it is contention rather than anything positional.
- The wait is not skipped. The three timed runs of that case took 31.18 s
  (failed), 31.12 s (passed) and 30.36 s (failed), against a twenty-second
  timeout. A failing run is consistent with a process waiting the whole budget
  and then giving up, and not with an instant refusal. It is worth noting that
  the passing run took the same thirty seconds, so the contention is present
  whether or not anyone loses.

**The budget is too short; the wait is not broken.** One variable changed,
`LOCK_TIMEOUT`, and nothing else. Same container, same case, run alone, three
iterations each:

| `LOCK_TIMEOUT` | result | wall clock per run |
| --- | --- | --- |
| 20 s (as shipped) | 2 failed, 1 passed | 31.18 s, 31.12 s, 30.36 s |
| 300 s | 3 passed, 0 failed | 36.46 s, 36.49 s, 36.51 s |

So a contender waits *more than twenty seconds* and then succeeds. This is a
budget exhausted, not a lock that can never be acquired, and the five or six
extra seconds in the second arm are where that waiting shows up. The passing run
in the first arm took the same thirty seconds as the failing ones, so the
contention is present whether or not anybody loses.

**What is still not established, and why no fix is here.** Why any holder keeps
an exclusive lock for twenty seconds when every critical section is supposed to
be short. Two candidates, neither tested: the retry polls every fifteen
milliseconds with no jitter, so six contenders can stay in lockstep and one can
lose every round; or some phase of a traced run holds the handle far longer than
intended, meaning `db.release()` is called in the wrong place, or not at all, on
some path. Distinguishing them means logging how long each holder keeps the
database, which has not been done.

Raising `LOCK_TIMEOUT` is *not* the fix, and the table above is not an argument
for it. A twenty-second hold of an exclusive lock is the defect; a longer
timeout only converts a failed command into a slow one. [#9](#9-the-tracer-loop-could-exit-with-a-child-still-stopped-and-hang-the-whole-command)
is this file's own argument for not editing a concurrency defect before the
mechanism is known, and it cost three wrong attempts to learn.

**Whether this is #12.** Unknown, and not assumed. #12 is a cache hit that
re-ran, which is a different symptom, and this failure is loud rather than
silent. What they share is only the configuration.

**How it was caught.** By fixing two things that were not this. The container
harness ran nothing ([#13](#13-the-linux-container-harness-ran-nothing-and-exited-0-doing-it)),
so the workspace had not actually been exercised on Linux from this machine;
once it was, these two failed. And the assertion they failed on was
`assert!(c.wait().unwrap().success())`, which reported nothing at all — the
diagnostic above exists only because those assertions now say what the child
did. Both of those were repairs to the instruments rather than to Arc, and
neither was made in order to find this.

**Reproduction.** `rust:1` container on Docker Desktop 29.4.1, arm64, twelve
CPUs, macOS host. Copy the tree in, then loop either case alone:

```sh
cargo test -q -p arc-cli --test linux_trace \
  concurrent_traced_runs_stay_independent -- --exact
```

It does not need the rest of the workspace, and it does not need load.

---

## What I would do differently

Six of these twelve were silent, and the two most serious — #1 and #2 — were
both a cache that had stopped doing the one thing it exists to do while
reporting success. That is the failure mode this project has to defend
against, and the defence is not more tests. It is tests that assert on the
*claim* rather than on the outcome: `TRACE COMPLETE` rather than `CACHE HIT`,
"the unrelated file did not invalidate" rather than "the run succeeded". A
green assertion that a correct-looking thing happened is compatible with the
feature being entirely switched off, and I have now shipped that twice.
