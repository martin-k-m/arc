# Changelog

Arc follows semantic versioning. What that covers, and what it does not, is
described under [Compatibility](#compatibility).

## Unreleased

### Added

- **A miss names which component of the execution key moved.** Previously a
  miss that no input or environment-variable change explained was reported as
  one three-way lump — "toolchain, observed dependencies or execution policy
  changed" — which is three unrelated causes and no way to tell them apart. The
  record now carries the remaining key components (OS, architecture, dependency
  digest, output patterns, environment id), so `--explain` names exactly one:
  the program's own contents changed, the learned dependency set widened, the
  Arc environment changed, the output patterns changed, the platform changed,
  or the cache schema moved. A record written before this existed carries none
  of them, and Arc says it cannot attribute the miss rather than guessing.

### Fixed

- **A restore destination that is itself a symlink is now refused.**
  `docs/security.md` has claimed this for some time and `safe_join` did not do
  it: it walked the destination's ancestors and never looked at the destination.
  No escape actually occurred, because `Store::materialize` renames a temporary
  file over the destination and a rename replaces a symlink rather than
  following it — but that is an accident of a function whose own documentation
  says it verifies "nothing about `dest`", and a property that holds by accident
  elsewhere is one refactor from not holding. The check is now in the function
  that documents it, and covers a dangling symlink too. This is a behaviour
  change: a cached output whose destination in your tree is a symlink now fails
  the restore, with the path named, instead of silently replacing the link with
  a regular file.

- **`docs/remote-protocol.md` said environment manifests are published with
  `POST`.** The server routes only `PUT` for that path and the client sends
  `PUT`; a `POST` gets a 404. The same document also said flatly that "there is
  no remote execution" two sections after specifying `/v1/exec/capabilities` and
  `/v1/exec/{namespace}/jobs`. Both corrected.

- **`scripts/linux-check.sh` ran nothing, and exited 0 doing it.** The script
  the container was to run was inlined into the `docker run` command line
  inside single quotes, and an apostrophe in one of its own comments closed
  that quoting: the container was handed a lone comment, which `bash -c`
  executes successfully and silently, and the caller's own arguments were
  dropped as loose words. Every "checked in the container" claim made through
  it is void. The container script is now a file named on the command line, so
  no quoting of the outer command can truncate it, and
  `crates/arc-cli/tests/harness.rs` asserts on the arguments the harness asks
  docker for. See `docs/BUGS.md` #13.

- **ptrace read a `connect`'s socket address after the syscall, not before.**
  The pointer was captured at the entry stop and the memory behind it was read
  at the exit stop, by which time the calling thread is stopped and its siblings
  are not: a thread that only writes memory makes no syscalls, is never stopped,
  and can rewrite that buffer while the tracer reads it. The result is a
  plausible path that was never connected to, recorded as a dependency, on a
  trace that still calls itself complete. The read now happens at the entry
  stop, where the kernel itself reads it. The seccomp backend was never
  affected: it is notified before the syscall runs. See `docs/BUGS.md` #11.

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
