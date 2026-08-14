# Changelog

Arc follows semantic versioning. What that covers, and what it does not, is
described under [Compatibility](#compatibility).

## 1.0.0

The first stable release. Everything below already existed across the 0.x
series; 1.0 is the point at which the command line, the configuration file, the
remote protocol and the stored formats become things you can depend on.

### Stable

- **Local execution cache.** `arc run -- <command>` fingerprints the command,
  its arguments, its working directory, the configured environment variables and
  everything it is known to depend on. A hit restores the recorded output and
  exit status instead of running the command.
- **Dependency intelligence.** Arc observes what a command reads, stats,
  enumerates and executes, and narrows the next run's fingerprint to exactly
  that — but only when it can prove the observation was complete.
- **Task graph.** Producer/consumer edges are learned from what tasks actually
  read and write. `arc graph` shows it; `arc affected` uses it.
- **Selective execution.** `arc affected --run` executes what a change reaches
  and nothing else, in dependency order, with bounded parallelism.
- **Shared cache.** A content-addressed store over HTTP, with digest
  verification on every object before it becomes local state.
- **CI integration.** `arc ci` determines the work a branch requires, reuses
  what the shared cache already holds, and reports what was reused.
- **Remote execution.** Cache misses can be sent to a worker. Results are
  published only after every artifact is uploaded and verified.
- **Execution environments.** Content-addressed, immutable toolchain bundles
  that participate in the execution key, so a toolchain change cannot hit.
- **Linux tracing.** Two backends: `linux-seccomp`, which uses seccomp user
  notification and needs no privilege, and `linux-ptrace`, which remains the
  fallback and the reference. See [docs/tracing.md](docs/tracing.md).

### Experimental

- **Remote execution** end-to-end deployment. The mechanism is tested and its
  correctness contract is defined, but the reference server has no
  authentication and must not be exposed to an untrusted network. See
  [docs/security.md](docs/security.md).

### Platforms

| | tracing | narrowing | cache | remote | environments |
| --- | --- | --- | --- | --- | --- |
| Linux x86-64 | seccomp / ptrace | yes | yes | yes | yes |
| Linux aarch64 | seccomp / ptrace | yes | yes | yes | yes |
| Windows x86-64 | snapshot + job object | no | yes | yes | yes |
| macOS | snapshot | no | yes | yes | yes |

Only platforms exercised by CI are listed as supported. Where a platform cannot
observe reads, Arc does not narrow: it falls back to the conservative project
scan, which is slower and always correct.

### Known limitations

- Automatic narrowing is Linux-only. Windows and macOS use the conservative
  project scan.
- `no_new_privs` is required by the seccomp backend, so a setuid program in a
  traced tree does not gain its extra privilege. Arc documents this rather than
  working around it.
- Environment variable reads cannot be observed by any syscall tracer. Arc uses
  the configured variable set instead.
- Shared memory and message queues are not modelled.

## Compatibility

Covered by semantic versioning from 1.0.0 onwards:

- the command line: command names, flags, defaults and exit codes
- `arc.toml` keys and their meanings
- `--json` output field names and meanings
- the remote cache and remote execution wire protocols
- stored cache records, to the extent that a newer Arc reads an older one

Not covered:

- the Rust API of `arc-core`, `arc-cache` and `arc-worker`. These are internal
  crates published only because the binaries need them. Their public items are
  `pub` for visibility, not as an API promise.
- human-readable terminal output, which is formatting rather than interface.

An older Arc reading newer local metadata rebuilds rather than misreading.
Downgrading is not tested and is not supported.

## Earlier releases

The 0.x series was developed as a sequence of milestones — local cache,
dependency intelligence, task graph, shared cache, CI, remote execution,
environments, fast tracing — and did not carry compatibility guarantees between
them. Their history is in the git log.
