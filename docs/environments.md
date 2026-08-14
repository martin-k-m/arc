# Execution environments

An Arc environment is a content-addressed answer to the question *what does this
command run inside*. Without one, "the same command" means "the same command on a
machine that happens to have the same compiler installed". With one, it means the
same bytes, wherever they run.

This is not a container runtime, a package manager, or a Nix replacement. Arc
captures bytes that already exist on a machine you trust, addresses them by
content, and materialises them again elsewhere. It never downloads a toolchain,
resolves a version constraint, or installs a system package.

```text
[environment.rust]              arc env capture rust
  tools / trees   ────────────────────▶   capture
                                             │
                                             ▼
                                    EnvironmentManifest
                                             │  digest of canonical bytes
                                             ▼
                                       EnvironmentId ──▶ arc-env.lock
                                             │
                                        Arc CAS blobs
                                             │
                                  ┌──────────┴──────────┐
                                  ▼                     ▼
                          local materialiser     worker materialiser
                                  └──────────┬──────────┘
                                             ▼
                                    deterministic PATH
                                   isolated HOME and TMP
```

## Identity

`EnvironmentId` is the BLAKE3 digest of the manifest's canonical bytes. Because
the manifest names every file by content digest, the id is a function of the
environment and of nothing else — not of the alias, not of the machine that
captured it, not of the absolute path it was captured from.

The manifest's bytes are themselves a CAS object whose digest *is* the id. That
is why v0.8 adds no new transport: publishing an environment is uploading blobs,
and fetching one is `GET /v1/{ns}/objects/{digest}` for the manifest followed by
the objects it names. Verification is the object verification Arc already does.

Canonical form is enforced on read. A manifest that hashes to the right id but
was serialised differently is refused, so the id stays a function of the content
rather than of an encoder.

## Configuration

```toml
[environment.rust]
# Programs resolved on this machine's PATH and captured into bin/.
tools = ["cargo", "rustc"]
# Variables every command in this environment gets. Secret-shaped names are
# refused: a manifest travels to other machines.
env = { RUST_BACKTRACE = "1" }

# A directory taken wholesale, at a destination this project chooses.
[[environment.rust.tree]]
from = "~/.rustup/toolchains/stable-x86_64-unknown-linux-gnu"
to = "rust"
exclude = ["**/doc/**", "**/*.rlib.d"]

[[command]]
name = "test"
command = "cargo"
args = ["test"]
environment = "rust"
```

`from` is a path on the capturing machine and is deliberately *not* part of
identity; `to` is a path inside the environment and *is*.

## Commands

```bash
arc env capture rust           # capture, pin the id in arc-env.lock
arc env capture rust --publish # and upload it to the remote cache
arc env list                   # what this project pins, what this machine holds
arc env inspect rust           # tools, PATH, host requirements, gaps
arc env verify rust            # re-hash every object and prove it materialises
arc env status                 # would capturing here still produce the pinned id?
arc env gc                     # drop environments this project no longer pins
```

`arc-env.lock` maps alias → id and is meant to be committed. Aliases are mutable
human configuration; the id is what enters an execution key.

## What capture takes

1. **Declared tools.** Each is resolved on the capturing machine's `PATH` and
   copied to `bin/<name>`.
2. **Declared trees.** Walked and captured by content at `to/…`. Relative
   symlinks inside the tree are preserved; an absolute or escaping symlink is
   followed and captured by content, so no host path survives. Device nodes,
   sockets and FIFOs are refused — an environment claiming to reproduce one
   would be lying about what it is.
3. **The runtime closure** of every captured executable that is a declared tool
   or sits directly in a tree's `bin/`. `DT_NEEDED` is read from the ELF file
   itself rather than by running `ldd`, which executes the dynamic loader.
   `$ORIGIN` in `DT_RUNPATH`/`DT_RPATH` is expanded; `LD_LIBRARY_PATH` is
   deliberately ignored so the same machine cannot capture two different
   environments.

Capture never walks `$HOME`, `/etc`, `~/.ssh`, `~/.aws`, or any tool cache. If a
compiler genuinely needs configuration from a home directory, the environment is
host-dependent and Arc will say so rather than package the directory.

You are responsible for having the right to redistribute any toolchain you
capture and publish.

## What capture does *not* take

Arc does not package the C library or the dynamic loader.

A library that resolves to a system directory (`/lib`, `/usr/lib`,
`/usr/lib/x86_64-linux-gnu`, …) is recorded as a **host requirement** by soname.
A library that resolves anywhere else — a toolchain's own `lib/` reached through
`$ORIGIN` — is captured. The split is deliberate: mixing a captured `libc.so.6`
with the host's `ld.so` is exactly the failure mode that makes naive "copy the
.so files" portability unreliable, and glibc's loader and C library are one unit.

So an environment's compatibility contract is:

```text
same OS and architecture
same libc flavour (gnu or musl)
a dynamic loader at each recorded PT_INTERP path
each recorded system soname resolvable
+ a Linux kernel new enough for the captured binaries
```

The first four are checked. The kernel is not packaged and not version-gated;
running a userspace newer than the host kernel fails the way it always does.

`HostRequirements` in the manifest states all of this. It is separate from the
environment's own content because it is a different kind of claim: the
environment *is* those bytes, whereas this is a statement about machines that can
run them.

## Completeness

`Complete` — every declared tool resolved and every library a captured binary
names was either captured or classified as a host requirement.

`Partial` — something could not be accounted for. The unresolved names are listed
in `gaps`, and `gaps` is part of identity, so two captures that missed different
things are different environments. A `Partial` environment is refused for remote
execution.

Completeness is structural. It says nothing about whether a *particular* command
run inside the environment has all it needs — see hermeticity below.

## Execution

A command with `environment = "…"`:

- resolves its program **inside** the environment. If the environment does not
  provide it, the run fails. There is no fallback to the host's copy, because a
  fallback would make the result depend on the machine.
- gets a `PATH` built only from the environment's `path_entries`.
- gets `LD_LIBRARY_PATH` from the environment's `library_path`.
- gets `HOME`, `TMPDIR`/`TEMP`/`TMP` and the `XDG_*` directories pointed at
  locations Arc owns, per environment id.
- starts from an empty environment: the variables it sees are the ones the
  execution key covers plus the environment's own, and nothing else.

`PATH` drops out of the execution key when an environment is in force, because
the environment defines it. That is the whole point: a result built here is
reusable on a machine whose `PATH` looks nothing like this one.

The environment id enters the execution key. A result built under environment A
is never served to a run under environment B. Arc makes no attempt to prove two
environments equivalent.

## Immutability

A materialised environment is a shared, read-only template. Files are written
read-only and directories are left traversable but not writable, so a command
that tries to rewrite a compiler gets an error instead of corrupting every future
job that reuses the environment. Copying a toolchain per execution would cost
more than the executions save.

The mechanism is file permissions, and permissions are not a defence against a
process that can ignore them. A command running as root — or as the same user
with `chmod` — can modify a shared environment. Arc states this rather than
implying a guarantee it does not have; `arc env verify` re-hashes every object
and will find such a change. Do not run workers as root if the repositories that
can reach them are not trusted with the machine.

Readiness is a sibling marker file written last. A directory without one is
wreckage from an interrupted build and is removed, so a partially materialised
environment can never be used. Concurrent requests for the same id materialise
once and the rest wait; across processes the atomic rename decides and the loser
discards its work.

## Workers

A worker advertises the `environment` feature, its host capability, and nothing
about which toolchains it has installed. An execution request names an
environment **by id only** — a coordinator cannot describe an environment, only
ask for one that already exists and hashes to what it said.

```text
request names environment E
        │
        ▼
manifest in worker store? ──no──▶ fetch object E from shared cache
        │                              │
        ▼                              ▼
   verify id == digest(bytes)   verify, parse, validate
        │
        ▼
   host supports E?  ──no──▶ Incompatible → client runs it locally
        │
        ▼
   fetch missing objects, materialise once, execute
```

Materialised environments persist in `<worker-data>/environments/` across jobs
and across restarts. The first job pays for the transfer; the rest do not.

## Hermeticity

Structural completeness is a property of an environment. **Hermeticity is a
property of one execution**, and Arc reports it separately:

- `hermetic` — a complete trace observed nothing outside the environment, the
  project, and the kernel surfaces (`/proc`, `/sys`, `/dev`) plus the declared
  host loader and system libraries.
- `host-dependent` — the command read host state the environment does not
  supply. The paths are listed. The result is correct on this machine and is not
  portable; such a command is not remote-eligible, because Arc already refuses to
  send a command whose complete trace reads outside the project.
- `unknown` — the execution was not completely observed, so Arc claims nothing.
  This is what you get wherever complete tracing is unavailable, including
  Windows and macOS.

`unknown` is also what a real compiler usually produces on Linux, because any
read of a volatile path — `rustc` consults `/proc/sys/vm/overcommit_memory` —
downgrades the trace below `Complete`. That is deliberate: hermeticity is only
claimed from an execution Arc watched in full, and "probably hermetic" is not a
claim worth making.

Arc does not call an environment-backed execution hermetic just because it used
an environment.

## Platform support

**Linux** is where this is implemented and tested: ELF closure discovery,
read-only environment trees, complete tracing to judge hermeticity.

**macOS and Windows** get environment capture, identity, materialisation and
deterministic `PATH`/`HOME`/`TMP`, but no runtime-closure discovery (there is no
ELF to parse) and no complete trace to judge hermeticity with. Symlinks in a
manifest are materialised as copies on Windows, which preserves the content the
manifest promised but not the link. Treat environment portability on those
platforms as unsupported: use host mode.

## Limitations

- Arc does not package the kernel or the C library. See the compatibility
  contract above.
- Capture is from a local machine only. There is no registry and no download.
- No equivalence proving: two environments differing by one byte are two
  environments.
- The reference worker's sandbox isolates the filesystem, the environment and the
  process tree. It does not isolate the network and runs as the worker's user.
  See `docs/remote-execution.md`.
- Tool caches (`~/.cargo`, npm, pip) are not part of an environment. They are an
  acceleration layer, and packaging them into identity would make every cache
  write a new environment.
