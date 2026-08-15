# Benchmarks

Every number here came from a command run on the machine described below.
Nothing is extrapolated, scaled, or carried over from another machine. The
scripts that produce them are in [`bench/`](../bench) and are committed, so
each table can be regenerated rather than trusted.

Where a measurement could not be made, this document says so instead of
estimating.

---

## Environment

Regenerate with `bench/environment.sh`.

**This is a Linux container on a Windows host, inside a virtual machine.**
That matters more than it usually would, because Arc measures syscall
interception and file I/O, and both are affected:

- Docker Desktop runs the container inside a WSL2 utility VM. Syscalls are
  handled by a real Linux kernel, so `ptrace` and seccomp user notification
  behave normally — both were confirmed available — but scheduling and
  interrupt latency are a VM's, not bare metal's.
- The container is given 4 of the host's 16 logical processors and 5.8 GiB of
  its 15.4 GiB. Everything below is therefore a 4-core measurement, and the
  heavier workloads run close to the memory limit.
- All benchmark files live on the VM's own overlay filesystem, not on a
  Windows bind mount. A bind mount would have measured the 9p/virtiofs bridge
  rather than Arc. This was deliberate and it is the single most important
  setup choice in this document.
- **The machine was not idle.** It is an interactive desktop with a browser
  and an editor running. Variance is visible in the tables and the spread
  between median and maximum is reported for that reason.

| | |
| --- | --- |
| Host CPU | Intel Core Ultra 9 285H, 16 logical processors, 2.9 GHz base |
| Host RAM | 15.4 GiB |
| Host storage | NVMe SSD (Timetec 35TT2280GEN4P-2TB) |
| Host OS | Windows 11 Pro 10.0.26200 |
| Virtualisation | Docker Desktop 4.83.0, engine 29.6.2, WSL2 |
| Container image | `rust:1-bookworm` (Debian 12) |
| Kernel | 6.18.33.2-microsoft-standard-WSL2, x86-64 |
| CPUs visible to container | 4 |
| RAM visible to container | 5.79 GiB |
| Container filesystem | overlayfs on the VM's ext4 |
| Rust | 1.97.1 (release profile, `codegen-units = 1`, `strip = symbols`) |
| glibc | 2.36 (Debian 12) |
| GCC | 12.2.0 |
| Python | 3.11.2, pytest 9.1.1 |
| make | GNU Make 4.3 |
| Arc | 1.0.0 at `harden/evidence` |
| Tracing backend | `linux-seccomp` preferred, `linux-ptrace` also available |

### Reproducing

```bash
docker run -d --name arcbench \
  --cap-add=SYS_PTRACE --security-opt seccomp=unconfined \
  -w /work rust:1-bookworm sleep infinity

docker exec arcbench apt-get update
docker exec arcbench apt-get install -y \
  git make gcc python3 python3-venv strace procps less

# Copy the repository to /work/arc inside the container -- not a bind mount.
docker exec arcbench sh -c 'cd /work/arc && cargo build --release'

docker exec arcbench bash /work/arc/bench/fetch-projects.sh
docker exec arcbench bash /work/arc/bench/environment.sh
docker exec arcbench sh -c 'cd /work/arc && REPS=7 bash bench/real-workloads.sh'
docker exec arcbench sh -c 'cd /work/arc && bash bench/hit-rate-click.sh 50'
docker exec arcbench bash /work/arc/bench/limits-probe.sh
```

`--cap-add=SYS_PTRACE` and `--security-opt seccomp=unconfined` are what let
both tracing backends work. Without the second, Docker's default profile
denies the `seccomp` syscall and Arc falls back to ptrace, which is slower —
`arc doctor` names the reason.

---

## Methodology

- **Warmup.** One untimed run per measurement, discarded. It settles the page
  cache and any incremental build state, so the timed runs measure steady
  state rather than a cold filesystem.
- **Repetitions.** `REPS=7` timed runs after the warmup, unless stated.
- **What is reported.** Median and maximum. Never the mean.
- **On "p99".** These samples are small. With 7 runs the highest observed
  value *is* the 1-in-7 tail; calling it p99 would be a lie about the sample
  size. The maximum is reported instead, and it is reported because the
  spread is the point: a warm hit whose median is 40 ms and whose maximum is
  400 ms is not a 40 ms operation on this machine.
- **Outliers.** None are removed. Every timed run is in the sample.
- **Setup is outside the clock.** Emptying the cache before a cold run,
  perturbing an input before a miss, and `make clean` before a build all
  happen before the timer starts.
- **Each phase is a real execution.** The "miss" phases append a line to a
  source file on every iteration, in a comment syntax valid for that
  language, so every iteration genuinely recompiles rather than measuring a
  build error.

### The four phases

| Phase | What happens |
| --- | --- |
| direct | the command with no Arc at all |
| arc cold | empty cache: run the command, trace it, store the result |
| arc miss | an input changed: run, trace and store again |
| arc warm | nothing changed: fingerprint and replay from the cache |

There is a subtlety in "warm" that is easy to get wrong. The run *after*
learning is still a miss, because narrowing for the first time changes the
key. Three runs are needed before a hit is the baseline, and the scripts do
three.

---

## The workloads

Three real projects, pinned. `bench/fetch-projects.sh` clones them at exactly
these commits.

| Project | Command | Language | Commit |
| --- | --- | --- | --- |
| [serde-rs/json](https://github.com/serde-rs/json) | `cargo test` | Rust | `afdf6fc67247dd7fa4fcde1381e6ecc6bcc7a30e` |
| [pallets/click](https://github.com/pallets/click) | `pytest` | Python | `8b44edfff7d9a6c895fa804148c16b3a0bc9efb5` |
| [TinyCC/tinycc](https://github.com/TinyCC/tinycc) | `make -j1` | C | `2ba12e83b3599ca8f5d50c179fe5138fe956f0c9` |

Notes on each baseline, because the baseline is where a benchmark lies most
easily:

- **serde_json.** `cargo test` is incremental. The honest baseline is the one
  a developer actually re-runs — a warm `target/` — where the time is
  recompiling what changed and running the tests. A from-scratch `target/` is
  a different measurement and is not what a cache competes with in a dev loop.
- **click.** `-p no:cacheprovider` stops pytest writing `.pytest_cache` into
  the project. `less` must be installed or 24 pager tests fail; a failing
  command is never cached, so without it every run is a miss for a reason
  that has nothing to do with Arc. That cost an hour to find.
- **tinycc.** `make` on an up-to-date tree does nothing, which would measure
  make deciding there is no work. Every timed iteration starts from `make
  clean`, so both the direct run and Arc face a real build.

---

## Results: what Arc costs and what it saves

`bench/real-workloads.sh`. Median of 7 for click and tinycc, median of 3 for
serde_json — a miss there is a ninety-second recompile and seven of those
across four phases is over an hour, so it gets fewer repetitions and the
count is stated rather than hidden.

| Workload | reps | direct | arc cold | arc miss | **arc warm** | warm speedup |
| --- | --- | --- | --- | --- | --- | --- |
| `pytest` (click, 1957 tests) | 7 | 3,639 ms | 17,636 ms | 5,916 ms | **35 ms** | **104×** |
| `make -j1` (tinycc, from clean) | 7 | 5,773 ms | 8,844 ms | 8,139 ms | **41 ms** | **141×** |
| `cargo test` (serde_json, warm target) | 3 | 6,188 ms | 17,635 ms | 31,880 ms | **112 ms** | **55×** |

The same numbers with their maxima, because the spread is the honest part on
a machine like this one:

| Workload | direct med / max | cold med / max | miss med / max | warm med / max |
| --- | --- | --- | --- | --- |
| click | 3,639 / 4,163 | 17,636 / 19,289 | 5,916 / **18,189** | 35 / 42 |
| tinycc | 5,773 / 6,649 | 8,844 / **14,404** | 8,139 / 9,171 | 41 / 82 |
| serde_json | 6,188 / 7,276 | 17,635 / **25,013** | 31,880 / 34,594 | 112 / 140 |

The bolded maxima are real. A miss on click took 18.2 seconds once against a
median of 5.9, and a cold tinycc build took 14.4 against a median of 8.8.
That is a four-core container under a load average that reached twelve during
these runs. Take the medians as the signal and the maxima as the reminder
that this is not a benchmarking rig.

### Cache size on disk

Measured after the cache has settled, `du -sb $ARC_HOME`.

| Workload | cache on disk |
| --- | --- |
| click | 4.29 MB |
| tinycc | 5.71 MB |
| serde_json | 6.44 MB |

These are small because the default captures stdout, stderr and the exit
code, which is the right thing for a test run. Declaring `[outputs]` to
restore build artifacts is what makes a cache large; tinycc's 5.7 MB is
mostly the trace's own dependency records over an 849-file, 74-process build.

## Results: what tracing costs

Each backend runs a forced miss, so the command really executes and the
difference from `direct` is what observation costs. `snapshot` is Arc doing
everything *except* watching syscalls, so the gap between it and a tracing
backend is the price of the tracing itself.

| Workload | direct | snapshot | seccomp | ptrace |
| --- | --- | --- | --- | --- |
| click (`pytest`, 71 processes) | 3,639 ms | 4,802 ms (+32%) | 6,418 ms (**+76%**) | 7,736 ms (**+113%**) |
| tinycc (`make`, 74 processes) | 5,773 ms | 6,235 ms (+8%) | 10,001 ms (+73%) | 6,760 ms (+17%) |
| serde_json (`cargo test`, 3147 processes) | 6,188 ms | 5,211 ms (-16%) | 12,376 ms (+100%) | 15,822 ms (+156%) |

**Read the tinycc row with suspicion.** It puts seccomp at 10.0 s and ptrace
at 6.8 s, which is the wrong way round and contradicts both the click row and
the design. Its seccomp sample ran from 6,649 ms to 15,034 ms; the spread is
larger than the effect. At seven repetitions on a contended four-core VM this
row does not resolve the ordering, and the honest conclusion is that it
measured the machine rather than the backend. It is left in rather than
dropped, because dropping the row that disagrees is how benchmark tables
become fiction.

The serde_json row has its own defect, in the other direction: `snapshot` at
5,211 ms is *faster* than `direct` at 6,188 ms, and that is impossible.
Snapshot does everything `direct` does plus scanning and storing. What it
records is that `direct` was measured at a moment when the machine was
busier, not that Arc is free. The two tracing figures in that row are
internally consistent — seccomp cheaper than ptrace, by 3.4 s — but the
percentages against that baseline are inflated by however much the baseline
was wrong, and should be read as an upper bound rather than a value.

The click row is the only one of the three that is clean, and it does support
the design: with the non-tracing work subtracted, seccomp costs 1,616 ms of
tracing and ptrace costs 2,934 ms,
so ptrace is about 1.8× more expensive on that workload. That is a smaller
ratio than the 3–6× the README claims from the synthetic syscall-heavy
benchmarks, which is what you would expect — pytest spends most of its time
in Python, not in syscalls, so the tracer has proportionally less to
intercept.

## Results: the hit rate

This is the number that decides whether any of the above matters. A cache
that is fast and never hits is worthless.

`bench/hit-rate-click.sh 60`. click's real history, 60 commits, replayed
oldest first. Three test-file tasks run at every commit, one run per
(commit, task), never re-run to manufacture a hit. Zero configuration: no
`arc.toml` beyond a project-root marker, so everything Arc narrowed to, it
learned by watching.

| Task | runs | hits | hit rate |
| --- | --- | --- | --- |
| `pytest tests/test_options.py` | 60 | 14 | 23.3% |
| `pytest tests/test_arguments.py` | 60 | 14 | 23.3% |
| `pytest tests/test_basic.py` | 60 | 0 | see below |
| **total, excluding the failing task** | **120** | **28** | **23.3%** |

**23.3%.** Roughly one run in four of a real test task, across two months of
a real project's history, did no work.

`test_basic.py` returned a non-zero exit at every one of the 60 commits, and
it is not Arc's doing: pytest 9.1.1 removed support for passing an iterator
to `parametrize`, and the file collects with
`PytestRemovedIn10Warning: Passing a non-Co…`. Arc never caches a non-zero
exit, so those 60 runs could only ever be misses. They are reported rather
than deleted, and excluded from the rate rather than folded into it — either
choice alone would be misleading, so both numbers are here.

Two things about the quality of those hits:

- **All 180 traces were complete, and 177 of 180 narrowed.** Every hit above
  is a narrowed hit: Arc decided the commit was irrelevant by checking the
  dependency set it had learned, not by hashing the project and finding it
  unchanged. Before the fix in [BUGS.md](BUGS.md#1) none of them narrowed,
  because pytest calls `fstat` on its own stdout.
- The three non-narrowed runs are the first run of each task, which has
  nothing learned yet.

### Why the tasks are per-file

Running the whole suite as one task would have measured how often a click
contributor pushes a commit touching nothing the tests read, which is a fact
about click's contributors. Splitting the suite is what a project actually
does to get value from a cache, and it is the only arrangement in which the
interesting case — a commit that invalidates one task and not another —
exists at all.

It also matters for completeness. The whole suite shells out to enough tools
that it reads `/sys/fs/selinux` and `/proc/mounts` and never narrows. A
single test file does not, so Arc learns its real dependency set. That is a
sharp edge worth knowing: **the granularity of your tasks decides whether
Arc's central feature works on them at all.**

### What a partial trace does to the same question

The three whole-project workloads in the tables above all trace partial, for
reasons listed in [LIMITATIONS.md](../LIMITATIONS.md#3). They still hit — the
104× and 141× warm numbers are real — but they hit by hashing the project and
finding nothing changed, which means any commit touching any file misses.
Replayed over history their hit rate would be the fraction of commits that
change nothing at all, which is close to zero. That is the difference the
dependency model makes, stated as plainly as I can put it.

---

## What could not be measured here

- **macOS and Windows.** Neither is available. Every platform claim about
  them in the README is from CI, not from this document.
- **Remote cache and remote execution over a real network.** The reference
  server and worker run on loopback here, so the numbers would measure
  loopback, and the README's existing remote table already says it injects
  synthetic latency. Nothing new was measured.
- **Bare metal.** Everything here is inside a VM. Absolute syscall-
  interception costs on bare metal will be lower; the ratios between backends
  should hold, but that is an expectation and it is not measured.
- **A quiet machine.** The host was in interactive use throughout. The
  maximum column is the honest record of what that did.
