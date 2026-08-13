# Remote execution

Arc can run a cache miss on another machine. The result is an ordinary Arc
execution — same exit status, same captured output, same output files, same
cache record — so everything downstream is the code that already existed.

```text
client ── cache miss ──▶ eligibility gate ──▶ worker ──▶ sandbox
   ▲                                            │
   │                                            ▼
   └────── verified objects ◀── shared CAS ◀── outputs
```

Two services, deliberately separate: **`arc-cache`** stores objects and records
(v0.5 protocol), **`arc-worker`** executes. The worker reads inputs from the
cache and publishes results to it, so a result never travels through the client
twice, and every machine afterwards gets a plain remote cache hit.

## Running it locally

```bash
# terminal 1
arc-cache serve --listen 127.0.0.1:7920 --data ./cache-data

# terminal 2
arc-worker serve --listen 127.0.0.1:7921 --data ./worker-data \
  --cache-url http://127.0.0.1:7920 --max-jobs 4
```

```toml
[remote]
url = "http://127.0.0.1:7920"
namespace = "my-project"

[remote.execution]
enabled = true
url = "http://127.0.0.1:7921"
```

```bash
arc run sh -c 'make build'
```

`scripts/remote-exec-demo.sh` runs the whole thing, including the failure cases.

## Security boundary

**Remote execution runs your repository's commands on another machine.** Only
run a worker you are willing to have execute code from the repositories that can
reach it.

The reference worker isolates the filesystem (a fresh workspace per job), the
environment (built from nothing, never inherited), and the process tree (killed
on completion, timeout or cancellation). It does **not** isolate the network,
and does not attempt to contain hostile code: a command runs as the worker's
user with the worker's network access. It is built for a trusted team's own
infrastructure — a dedicated or self-hosted worker — not as a multi-tenant
sandbox. If you need to run untrusted code, put the worker in a VM or a
container and treat that as the boundary.

Authorisation has two scopes. `--token-env` names the token a client must
present to **execute**; `--read-token-env` names one that may query capabilities
and job status but not submit work. Without either, the worker is open, which is
only ever right on a loopback address.

## Eligibility

Arc executes remotely only when it can build an environment complete and
compatible enough that the result means the same thing. The gate lives in one
place (`remote::eligibility`) and refuses for a stated reason:

| Refused when | Because |
|---|---|
| the worker's OS, architecture or cache-semantics version differs | the key describes a different execution |
| the execution protocol version differs | the request would mean something else |
| an argument is an absolute path (`/etc/x`, `C:\x`, `\\host\share`) | it names a path on *this* machine |
| a required environment variable looks like a secret and is set | sending it is a decision the project makes explicitly |
| a complete trace saw the command read a file outside the project | that file cannot be materialised into a sandbox |
| the program, or any executable a complete trace observed, is not byte-identical on the worker | a build that shells out to a different linker is a different build |
| `[[command]] remote = "never"` | the command's effects are not ones Arc can see |
| learned dependencies failed validation | Arc does not act on knowledge it cannot vouch for |

Anything refused runs locally, which is what would have happened without a
worker at all.

### What gets sent

Exactly the files that were fingerprinted — the same set the execution key
describes.

* **Complete trace**: the learned dependency set. Only chosen when Arc could
  also have narrowed the cache key to it.
* **Otherwise**: every project file Arc fingerprinted. Broad, and safe: it can
  only be an over-approximation of what the command reads inside the project.

Arc never sends less than it fingerprinted to make a transfer cheaper. If a file
is outside the fingerprint it is also outside the local cache's model, so the
sandbox matches what Arc believes the input state to be — and when that belief
is wrong, the command fails loudly on the worker rather than producing a wrong
answer quietly.

Inputs are referenced by digest and fetched by the worker from the shared cache,
so a repeated execution transfers nothing.

## Toolchain compatibility

Executables are matched by **content**, never by a version string. The client
sends the content digest of the program it resolved, plus — when a complete
trace is available — every other executable the command was observed running.
The worker resolves each on its own PATH, hashes it, and refuses the job if any
differs. Resolution happens immediately before spawning and the resolved path is
what runs, so PATH changing in between cannot substitute a different binary.

Beyond that, the worker advertises an `environment_id` covering the platform,
the semantics version, and the libc flavour. It is a coarse signal, not a
certificate: see [correctness.md](correctness.md#remote-execution) for exactly
what it does and does not establish.

## Protocol

`/v1/exec`, versioned independently of the cache protocol.

### `GET /v1/exec/capabilities`

```json
{
  "protocol": 1, "key_semantics": 3,
  "worker": "worker-1234", "version": "0.7.0",
  "os": "linux", "arch": "x86_64",
  "environment_id": "8f1c…",
  "max_jobs": 4, "queue_limit": 64, "active": 1, "queued": 0,
  "network": "unrestricted",
  "cache_endpoint": "http://127.0.0.1:7920"
}
```

### `POST /v1/exec/{ns}/jobs`

Submits an `ExecutionRequest` and returns a `Job`. **Idempotent on the execution
key**: a worker already running that exact execution returns the existing job
rather than starting a second one, so a retried request cannot duplicate
expensive work, and two clients that want the same result converge on one.

`400` malformed, `401` not authorised to execute, `422` incompatible worker,
`429` queue full — all of which mean "run it yourself".

Before executing, the worker re-checks the shared cache: a result published
between the client's miss and the job reaching the front of the queue is
returned instead of running the command again.

### `GET /v1/exec/{ns}/jobs/{id}`

```json
{ "id": "j-1234-7", "execution_key": "…", "state": "running",
  "result": null, "error": null, "log_len": 4096, "waiters": 3, "queued_ms": 12 }
```

States: `queued`, `running`, `completed`, `failed`, `cancelled`, `lost`.
**`completed` means the command reached a conclusion**, whatever its exit code.
`failed` means the environment, not the code, is what failed — and carries a
`kind`: `incompatible`, `input_unavailable`, `sandbox`, `timeout`, `cancelled`,
`overloaded`, `internal`.

The distinction decides what the client may do. A worker that could not start
the command has not started it, so running it locally is safe. A timeout or a
cancellation means the command **may still be running**, so Arc refuses to run
it again and fails with an explanation instead.

### `GET /v1/exec/{ns}/jobs/{id}/log?offset=N`

Bounded incremental log. Informational only — nothing about cache correctness
depends on it arriving, and the sink drops its tail rather than growing without
limit.

### `POST /v1/exec/{ns}/jobs/{id}/cancel`

Advisory. Cancellation decrements the waiter count; the job is only stopped when
the last waiter leaves, because one client walking away is not a reason to
discard work the others are still waiting for.

### Limits

| Limit | Value |
|---|---|
| Request body | 64 MiB |
| Manifest entries | 200,000 |
| Arguments / bytes | 4,096 / 1 MiB |
| Environment variables / bytes | 1,024 / 1 MiB |
| Tool requirements | 1,024 |
| Log buffer | 8 MiB |
| Retained job records | 512 |

## The sandbox

```text
worker-data/
  store/                 content-addressed, shared across jobs
  work/<job-id>/
    workspace/           inputs materialised here; cwd
    home/                HOME
    tmp/                 TMPDIR, TEMP, TMP
```

Objects are **copied** out of the worker's store, never hardlinked: a hardlink
would let a command mutate the stored object through its workspace and corrupt
every later job that reused it.

The environment is built from nothing — `env_clear`, then the variables the
execution key depends on, then sandbox `HOME` and temp paths. The worker's own
environment, including its cache credentials, never reaches the child.

Each job's process is put in its own session, and the whole group is killed when
it finishes, times out or is cancelled, so nothing survives the workspace it was
writing into.

The sandbox is removed whether the command succeeded, failed or never ran.
Workspaces left by a crash are cleared at startup.

## Ordering in a task graph

Remote execution changes nothing about graph semantics. `A → B` still means B
runs after A. What differs is where each one's outputs live:

| A | B | Handoff |
|---|---|---|
| remote | remote | shared CAS — no client round trip |
| local | remote | A's outputs upload with B's input manifest |
| remote | local | A's result is verified and restored locally first |
| cache hit | remote | already in the cache |

For B to consume A's output remotely, that output must be part of B's input
fingerprint. It usually is; a generated file excluded from the fingerprint —
gitignored, say — is invisible to the local cache too, and the command fails on
the worker rather than silently producing something else.

`--jobs N` bounds **total** concurrent tasks, wherever they run. Worker capacity
is separate and global across clients: `--max-jobs` executions at once, the rest
queued up to `--queue-limit`, then `429` and the work comes home.

## What it does not do

No container or VM management, no autoscaling, no worker fleet, no scheduler
beyond FIFO, no persistence of job state across a worker restart, no network
isolation, and no claim of hermeticity Arc cannot back up. Clock, randomness and
scheduling still differ between machines exactly as
[correctness.md](correctness.md) has always said they do.
