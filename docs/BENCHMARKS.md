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

RESULTS_PLACEHOLDER

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
