# Threat model

What Arc defends, what it does not, and what leaves your machine. For how to
report a vulnerability, see [SECURITY.md](../SECURITY.md).

## The asset Arc protects

A cached result is an **executable output**. Anyone who can write to a cache
Arc reads from can decide what your build produces. Every boundary below exists
to serve one property:

> Arc never returns a cached result unless the current execution state is
> authorized to reuse it.

Everything else — digest verification, restore validation, trace completeness —
is a means to that end.

## Trust boundaries

| | trusted | why |
| --- | --- | --- |
| the local project | yes | you already run its commands |
| the local Arc home | yes | same user, same machine |
| a remote CAS object | **no**, until its digest is verified | bytes arrive over a network |
| a remote execution record | **no**, until every referenced object verifies | the server could be wrong or hostile |
| a worker's result | **no**, until published and verified | a worker executes arbitrary code |
| an environment manifest | **no**, until parsed under bounds and its blobs verify | it travels between machines |

### Remote objects

Every object fetched from a shared cache is hashed and compared to the digest
that was asked for, **before** it becomes local state. A digest mismatch is a
rejection, not a warning: the object is discarded and the command executes. An
object is never trusted because its filename looks like a digest.

### Remote workers

A worker executes your repository's commands as the worker's user, with the
worker's network access. It isolates the filesystem, the environment and the
process tree — see [remote-execution.md](remote-execution.md#the-sandbox) — and
it does **not** attempt to contain hostile code. It is built for a trusted
team's own infrastructure, not as a multi-tenant sandbox. To run untrusted code,
put the worker in a VM or container and treat that as the boundary.

The worker's own environment, including its cache credentials, is cleared before
a command starts. A job cannot read them.

### Servers

The reference cache and worker support bearer-token authentication
(`--token-env`) and neither requires it. Both bind to localhost by default; both
warn on startup if serving unauthenticated on a reachable address. Neither
terminates TLS — put a reverse proxy in front of one that crosses a network you
do not control.

## Secrets

Arc's execution key depends on environment variables, and some of them are
secrets. Arc's rule is that a secret-shaped variable is **used but never
recorded**: its value contributes to the key as a hash and appears nowhere a
human or another machine can read it.

Specifically, a secret value must not appear in:

- terminal output, at any verbosity
- `--json` output
- `arc history`, `arc inspect`, `arc doctor`
- execution records, the metadata database, or the CAS
- environment manifests, which travel to other machines
- cache or worker server logs
- error messages

`arc doctor` is designed to be paste-safe for bug reports.

## What remote mode sends

With a shared cache configured, Arc uploads:

- **content-addressed blobs of the inputs** — this includes your source files
- execution metadata: the command, its arguments, its working directory, the
  names of the environment variables in the key and hashes of their values
- outputs, as blobs, and the record that ties them to a key

With remote execution enabled, the same inputs are additionally materialised in
a workspace on the worker and the command runs there.

**Source code leaves the machine.** That is not a side effect; it is how a
shared cache works. Decide accordingly where the cache and workers run.

Arc has no telemetry, no accounts, and contacts nothing you have not configured.
With no remote configured, nothing leaves the machine.

## Filesystem safety

Restoring an output writes into your project, so restoration validates before it
applies anything:

- a path containing `..`, an absolute path, or a path escaping the project is
  rejected
- a destination whose parent is a symlink is rejected
- a destination that is itself a symlink is rejected
- validation happens for **every** path first; a set that fails anywhere is
  applied nowhere

Destructive commands — `arc clean`, cache prune, environment GC — delete only
paths that pass the same ownership and scope checks.

## Tracing and privilege

The seccomp backend sets `no_new_privs`, so a setuid or file-capability program
inside a traced tree does not gain its extra privilege. Arc does not work around
this. A command must not gain privilege because Arc traced it, and Arc does not
install a setuid helper or a privileged daemon to avoid losing it.

Tracing requires no `sudo`, no capability, no kernel module and no reboot.

## What Arc does not guarantee

- **Not a sandbox.** Arc caches commands; it does not confine them.
- **Not multi-tenant safe.** A shared worker is shared trust.
- **Not a defence against a compromised cache server** beyond digest
  verification: a server that serves a *valid* object for the wrong key is
  trusted, because the key is what the client asked for.
- **Not reproducibility.** Clock, scheduling, hardware and network responses
  remain outside Arc's model. See
  [correctness.md](correctness.md#known-limits).
