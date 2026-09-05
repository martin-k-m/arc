# Troubleshooting

Three commands answer most questions:

```bash
arc run --explain -- <command>   # why this run hit or missed
arc inspect <key>                # everything recorded about one execution
arc doctor                       # what works on this machine
```

## Why did this miss?

`--explain` names the cause rather than saying the key changed:

```console
$ arc run --explain -- cargo test
  result              cache miss
  reason              src/parser.rs changed
  changed
    src/parser.rs changed
```

The `reason` line names the specific cause, and the `changed` block lists every
path that moved. Common reasons:

| reason | what happened |
| --- | --- |
| `<path>` changed / added / removed | a file the command is known to depend on moved |
| `<VAR>` changed | a variable in `[env] include` has a different value |
| no previous execution of this command was recorded | the first run of a new command |
| tracked inputs changed | several inputs moved at once |
| all inputs match execution `<id>` | this is a hit, and which record it came from |
| the program changed: `<path>` | the executable's own contents differ — a compiler or interpreter upgrade |
| what Arc observed this command execute changed | the learned dependency set widened; the usual cause of the miss right after a first trace |
| the Arc environment changed | a different `[environment]` is in force, or one was gained or lost |
| the configured output patterns changed | `[outputs] include` was edited |
| the platform changed | a different OS or CPU architecture than the recorded run |
| Arc's cache format changed | Arc's stored schema version moved; older results are not reused |

Every component of the execution key is in that table. If Arc reports that it
*cannot say which* component changed, the previous record predates Arc storing
them: run the command once more and the next miss will name it.

The `dependencies` line above it is the other half of the story. `complete`
means Arc narrowed to exactly what the command reads; `partial` or `unsupported`
means it compared the whole project instead, and names why.

A miss is never a bug in itself — Arc takes a false miss over a false hit
deliberately. A miss you cannot explain is worth reporting.

## Why is my trace partial?

`arc run --trace` reports the reason. A partial trace is correct; it just means
Arc compares the whole project instead of a narrowed set, which is slower.

Most common on a stock Linux system: **volatile reads**. Debian's coreutils
probe SELinux through `/sys/fs/selinux` and `/proc/filesystems`, so `mv`, `ls`
and `cp` produce partial traces. Nothing is wrong; Arc is refusing to pretend
that reading a kernel interface is a stable dependency.

Others: a network connection, a path that is not valid UTF-8, a syscall the
backend does not model, the trace budget overflowing, or the tracer failing.
[correctness.md](correctness.md#downgrade-reasons) lists them all.

On **Windows and macOS** there is no read-observing backend at all, so traces
never narrow. That is a platform limitation, not a fault — see
[tracing.md](tracing.md).

## Why is tracing slower than I expected?

`arc doctor` says which backend is preferred. If it is `linux-ptrace` where you
expected `linux-seccomp`, doctor also says why — most often a container seccomp
profile denying the `seccomp` syscall. Docker's default profile does exactly
this. Run the container with `--security-opt seccomp=unconfined` if you want
the fast backend inside it.

## Why is the remote cache unavailable?

```bash
arc remote ping
```

Arc treats an unreachable remote as a reason to work locally, not a reason to
fail: the command still runs and the local cache still works. If you need the
opposite — a build that fails rather than silently going local — configure the
remote as required.

If `ping` succeeds but nothing hits, check that both machines use the same
`namespace`, and that the executions really are the same: a different toolchain
or a different set of environment variables is a different key, correctly.

## Why is a worker ineligible?

```bash
arc remote status
```

A task is not sent to a worker when the worker cannot run it. Usual causes: the
worker lacks the platform the command needs, the command is excluded by
configuration, the worker is at `--max-jobs` and its queue is full (the work
comes home rather than waiting), or the command depends on host state the
worker does not have.

## Why didn't `affected` skip this?

`arc affected --explain` shows why each task is in its bucket. A task Arc cannot
*prove* irrelevant is reported as **unknown**, not unaffected, and unknown work
runs. Usual cause: the last trace of that task was partial, so Arc does not know
what it reads.

## Why was an environment rejected?

```bash
arc env verify <alias>
```

An environment is content-addressed and immutable. It is rejected when a blob
is missing or its digest does not match, when the manifest version is newer
than this Arc understands, or when the host cannot satisfy the environment's
requirements. Recapture with `arc env capture <alias>`.

## Why was a cache entry discarded?

Because something did not verify. A digest mismatch on a local object, a
corrupt metadata record, or a truncated database all lead to the same place:
the entry is rejected and the command executes. Arc never repairs a cached
result into something it thinks is probably right.

```bash
arc cache verify    # re-hash every object and quarantine what fails
```

## Arc is doing something to my repository I did not expect

```bash
arc config show     # the effective configuration, and where each value came from
```

Configuration comes from the command line, then environment variables, then
`arc.toml`, then defaults. `config show` resolves all of it.

## Starting over

```bash
arc cache clear     # drop cached results, keep the configuration
arc clean --all     # remove the Arc home entirely
```

Neither touches your project's source. `arc clean` validates every path it is
about to delete against the Arc home's own boundary first.

## Reporting a bug

Include `arc --version`, `arc doctor` and the smallest reproduction you have.
`arc doctor` is designed to be safe to paste: it prints the shape of your
configuration, not its secrets. For anything exploitable, use the repository's
Security tab rather than a public issue — see [SECURITY.md](../SECURITY.md).
