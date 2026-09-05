#!/usr/bin/env bash
# Build and test Arc on Linux, in a container.
#
# Two reasons this is a container rather than the host: the Linux tracer can
# only be exercised on Linux, and a container is also the environment where
# ptrace is most likely to be *refused* — so running the suite here proves both
# that the backend works and that its unavailable path is real.
#
#   scripts/linux-check.sh test --workspace
#   scripts/linux-check.sh clippy --workspace --all-targets -- -D warnings
#
# `ARC_DOCKER_ARGS=--security-opt=seccomp=unconfined` relaxes the default
# seccomp profile, for comparing behaviour with and without it.
#
# The work the container does lives in `linux-check-inner.sh`, named here as a
# path rather than inlined as a quoted string. It used to be inlined, and an
# apostrophe inside one of its comments ended the quoting early: docker was
# handed a lone comment to run, the caller's arguments were dropped as loose
# words, and the whole thing exited 0 without printing anything. See
# docs/BUGS.md #13.
set -euo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
# The full image ships git, which several tests need to ask what changed.
IMAGE=${ARC_IMAGE:-rust:1}

# A TTY is only requested when there is one to attach to: with output
# redirected, `-t` can swallow it entirely on some Docker hosts.
#
# Expanded as ${TTY[@]+"${TTY[@]}"} rather than "${TTY[@]}" because macOS ships
# bash 3.2, where an EMPTY array expanded under `set -u` is an unbound variable
# rather than nothing at all. That is not a hypothetical: the plain spelling
# passed on Linux and on this repository's own container, and failed all four
# harness tests on the macOS runner with `TTY[@]: unbound variable`.
TTY=()
[ -t 1 ] && TTY=(-t)

exec docker run --rm ${TTY[@]+"${TTY[@]}"} \
  -v "$HERE":/src:ro \
  -v arc-linux-target:/target \
  -v arc-linux-cargo:/usr/local/cargo/registry \
  -e CARGO_TARGET_DIR=/target \
  -e CARGO_TERM_COLOR=always \
  ${ARC_DOCKER_ARGS:-} \
  -w /work \
  "$IMAGE" \
  bash /src/scripts/linux-check-inner.sh "$@"
