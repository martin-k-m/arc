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
set -euo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
# The full image ships git, which several tests need to ask what changed.
IMAGE=${ARC_IMAGE:-rust:1}

exec docker run --rm -t \
  -v "$HERE":/src:ro \
  -v arc-linux-target:/target \
  -v arc-linux-cargo:/usr/local/cargo/registry \
  -e CARGO_TARGET_DIR=/target \
  -e CARGO_TERM_COLOR=always \
  ${ARC_DOCKER_ARGS:-} \
  -w /work \
  "$IMAGE" \
  bash -c '
    # The host target directory is another platform's and can be gigabytes.
    tar -C /src -cf - --exclude=./target --exclude=./.git . | tar -C /work -xf -
    # The slim image ships no clippy or rustfmt; add them only when asked for,
    # so a plain `test` run does not pay for a component download.
    case "$1" in
      clippy) rustup component add clippy >/dev/null 2>&1 ;;
      fmt)    rustup component add rustfmt >/dev/null 2>&1 ;;
    esac
    exec cargo "$@"' -- "$@"
