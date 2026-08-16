#!/usr/bin/env bash
# Fetch and prepare the three real projects the benchmarks run against.
#
# Each is pinned to the commit that was actually measured. Re-running this
# script reproduces the same trees; changing a pin changes the numbers, so
# the pins and the results in docs/BENCHMARKS.md move together.
#
#   serde_json  https://github.com/serde-rs/json
#   click       https://github.com/pallets/click
#   tinycc      https://github.com/TinyCC/tinycc
#
# Prerequisites inside the container:
#   apt-get install -y git make gcc python3 python3-venv strace procps
#   plus a Rust toolchain (the rust:1-bookworm image has one)
set -euo pipefail

BENCH=${BENCH:-/work/bench}

SERDE_JSON_COMMIT=afdf6fc67247dd7fa4fcde1381e6ecc6bcc7a30e
CLICK_COMMIT=8b44edfff7d9a6c895fa804148c16b3a0bc9efb5
TINYCC_COMMIT=2ba12e83b3599ca8f5d50c179fe5138fe956f0c9

mkdir -p "$BENCH"
cd "$BENCH"

get() {
  local url=$1 dir=$2 commit=$3
  [ -d "$dir/.git" ] || git clone -q "$url" "$dir"
  git -C "$dir" fetch -q origin
  git -C "$dir" checkout -q --detach "$commit"
  echo "  $dir  $(git -C "$dir" rev-parse --short HEAD)  $(git -C "$dir" log -1 --format=%ad --date=short)"
}

echo "projects:"
get https://github.com/serde-rs/json.git serde_json "$SERDE_JSON_COMMIT"
get https://github.com/pallets/click.git  click      "$CLICK_COMMIT"
get https://github.com/TinyCC/tinycc.git  tinycc     "$TINYCC_COMMIT"

# The virtualenv lives OUTSIDE the checkout on purpose. Inside it, a
# conservative whole-project scan would hash several thousand interpreter
# files on every run, which would measure the venv rather than the project.
if [ ! -x "$BENCH/venv-click/bin/pytest" ]; then
  python3 -m venv "$BENCH/venv-click"
  "$BENCH/venv-click/bin/pip" install -q --upgrade pip
  "$BENCH/venv-click/bin/pip" install -q pytest
fi
"$BENCH/venv-click/bin/pip" install -q -e "$BENCH/click"
echo "  click venv  $("$BENCH/venv-click/bin/python" -m pytest --version)"

# tinycc needs one configure before make will work. It is not part of any
# measurement; every timed iteration starts from `make clean`, which keeps
# config.mak.
[ -f "$BENCH/tinycc/config.mak" ] || ( cd "$BENCH/tinycc" && ./configure >/dev/null )
echo "  tinycc configured"

# serde_json's dependencies are fetched once so that no measured run is
# waiting on crates.io.
( cd "$BENCH/serde_json" && cargo fetch -q )
echo "  serde_json deps fetched"
