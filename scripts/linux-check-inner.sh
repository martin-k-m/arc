#!/usr/bin/env bash
# One cargo invocation, run inside the container by `linux-check.sh`.
#
# This is a file rather than a string passed to `bash -c` on purpose. The
# previous arrangement inlined it into the `docker run` command line inside
# single quotes, and an apostrophe in one of its own comments closed those
# quotes: the container was handed a single comment, ran it, printed nothing
# and exited 0, and the caller's arguments never arrived at all. A file cannot
# be truncated by the quoting of the command that names it.
set -euo pipefail

# The host target directory belongs to another platform and can be gigabytes,
# so the tree is copied without it. /src is mounted read-only; /work is where
# the build actually happens.
tar -C /src -cf - --exclude=./target --exclude=./.git . | tar -C /work -xf -
cd /work

# Several tests exercise `arc affected`, which needs a real repository with an
# identity to commit under. CI configures the same three.
git config --global user.email ci@example.com
git config --global user.name ci
git config --global init.defaultBranch main

# The image ships no clippy or rustfmt; add them only when asked for, so a
# plain `test` run does not pay for a component download.
case "${1:-}" in
clippy) rustup component add clippy >/dev/null 2>&1 ;;
fmt) rustup component add rustfmt >/dev/null 2>&1 ;;
esac

exec cargo "$@"
