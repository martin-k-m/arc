#!/usr/bin/env bash
# What environments cost, and what they cost once they are warm.
#
# Every number comes from `arc env capture --json` or `arc run --json`, which
# report their own component timings. Medians over ROUNDS.
set -uo pipefail

cd "$(dirname "$0")/.."
cargo build --release -p arc-cli -p arc-cache -p arc-worker >/dev/null || exit 1
TARGET=${CARGO_TARGET_DIR:-$PWD/target}
ARC="$TARGET/release/arc"
CACHE_BIN="$TARGET/release/arc-cache"
WORKER_BIN="$TARGET/release/arc-worker"

WORK=$(mktemp -d)
CACHE_PID=""; WORKER_PID=""
cleanup() {
  [ -n "$CACHE_PID" ] && kill "$CACHE_PID" 2>/dev/null
  [ -n "$WORKER_PID" ] && kill "$WORKER_PID" 2>/dev/null
  chmod -R u+w "$WORK" 2>/dev/null
  rm -rf "$WORK"
}
trap cleanup EXIT
export ARC_NO_ANIM=1

ROUNDS=${ROUNDS:-3}
field() { grep -o "\"$1\":[0-9]*" | head -1 | cut -d: -f2; }
sfield() { grep -o "\"$1\":\"[^\"]*\"" | head -1 | cut -d'"' -f4; }
median() { sort -n | awk '{v[NR]=$1} END {print (NR ? v[int((NR+1)/2)] : 0)}'; }
row() { printf '  %-22s %s\n' "$1" "$2"; }
say() { printf '\n\033[1m%s\033[0m\n' "$1"; }

# The *sysroot*, not whatever directory `rustc` happens to sit in: on a rustup
# install that is a shim, and capturing the shim would capture no compiler at
# all. This is the trap docs/environments.md warns about.
TOOLCHAIN_ROOT=$(rustc --print sysroot)

"$CACHE_BIN" serve --listen 127.0.0.1:7940 --data "$WORK/cache" >/dev/null 2>&1 &
CACHE_PID=$!
start_worker() {
  "$WORKER_BIN" serve --listen 127.0.0.1:7941 --data "$WORK/worker" \
    --cache-url http://127.0.0.1:7940 --max-jobs 8 >/dev/null 2>&1 &
  WORKER_PID=$!
  sleep 1
}
cold_worker() {
  [ -n "$WORKER_PID" ] && kill "$WORKER_PID" 2>/dev/null && wait "$WORKER_PID" 2>/dev/null
  chmod -R u+w "$WORK/worker" 2>/dev/null
  rm -rf "$WORK/worker"
  start_worker
}

mkproject() {
  local dir=$1 exec_enabled=$2
  mkdir -p "$dir/src"
  cat > "$dir/arc.toml" <<TOML
[outputs]
include = ["*.rmeta"]

[remote]
url = "http://127.0.0.1:7940"
namespace = "env-bench"

[remote.execution]
enabled = $exec_enabled
url = "http://127.0.0.1:7941"

[environment.rust]
tools = []

[[environment.rust.tree]]
from = "$TOOLCHAIN_ROOT"
to = "rust"
exclude = ["**/share/doc/**", "**/lib/rustlib/src/**"]

[[command]]
name = "build"
match = "rustc*"
environment = "rust"
TOML
  printf 'fn main() { println!("hello"); }\n' > "$dir/src/main.rs"
}

# Real compilation with no linker: an environment captures a toolchain,
# not a C toolchain. See docs/environments.md.
BUILD=(rustc --edition 2021 --crate-type lib --emit=metadata -o app.rmeta src/main.rs)
run() { (cd "$1" && ARC_HOME="$2" "$ARC" run --json "${BUILD[@]}" 2>&1 >/dev/null); }

start_worker
mkproject "$WORK/repo" false

# --------------------------------------------------------------------------

say "environment capture (a real Rust toolchain)"
: > "$WORK/cap.txt"
for i in $(seq 1 "$ROUNDS"); do
  rm -rf "$WORK/home-cap"
  (cd "$WORK/repo" && ARC_HOME="$WORK/home-cap" "$ARC" env capture rust --json) > "$WORK/c.json"
  printf '%s %s %s\n' \
    "$(field capture_ms < "$WORK/c.json")" \
    "$(field files < "$WORK/c.json")" \
    "$(field bytes < "$WORK/c.json")" >> "$WORK/cap.txt"
done
row "capture" "$(awk '{print $1}' "$WORK/cap.txt" | median) ms"
row "files" "$(awk '{print $2}' "$WORK/cap.txt" | median)"
row "bytes" "$(awk '{print $3}' "$WORK/cap.txt" | median)"
row "completeness" "$(sfield completeness < "$WORK/c.json")"

say "environment publish (upload to the shared cache)"
rm -rf "$WORK/home-pub"
S=$(date +%s%N)
(cd "$WORK/repo" && ARC_HOME="$WORK/home-pub" "$ARC" env capture rust --publish --json) >/dev/null
row "capture + publish" "$(( ($(date +%s%N) - S) / 1000000 )) ms"

say "local execution inside the environment"
for i in $(seq 1 "$ROUNDS"); do
  rm -rf "$WORK/home-l"
  printf 'fn main() { println!("l%s"); }\n' "$i" > "$WORK/repo/src/main.rs"
  run "$WORK/repo" "$WORK/home-l" | field duration_ms
done | median > "$WORK/m"
row "total" "$(cat "$WORK/m") ms"

say "local execution, warm environment (same machine, second run)"
for i in $(seq 1 "$ROUNDS"); do
  printf 'fn main() { println!("w%s"); }\n' "$i" > "$WORK/repo/src/main.rs"
  run "$WORK/repo" "$WORK/home-l" | field duration_ms
done | median > "$WORK/m"
row "total" "$(cat "$WORK/m") ms"

say "local cache hit"
run "$WORK/repo" "$WORK/home-l" >/dev/null
for i in $(seq 1 "$ROUNDS"); do
  run "$WORK/repo" "$WORK/home-l" | field duration_ms
done | median > "$WORK/m"
row "restore" "$(cat "$WORK/m") ms"

# --------------------------------------------------------------------------

mkproject "$WORK/remote" true
cp "$WORK/repo/arc-env.lock" "$WORK/remote/arc-env.lock"

say "remote execution, cold worker (it must fetch the whole environment)"
: > "$WORK/cold.txt"
for i in $(seq 1 "$ROUNDS"); do
  rm -rf "$WORK/home-rc"
  cold_worker
  printf 'fn main() { println!("rc%s"); }\n' "$i" > "$WORK/remote/src/main.rs"
  run "$WORK/remote" "$WORK/home-rc" > "$WORK/o.json"
  for k in stage_ms upload_ms queue_ms execute_ms fetch_ms total_ms; do
    printf '%s ' "$(field "$k" < "$WORK/o.json")"
  done >> "$WORK/cold.txt"
  echo >> "$WORK/cold.txt"
done
col=1
for label in stage upload queue execute fetch total; do
  row "$label" "$(awk -v c="$col" '{print $c}' "$WORK/cold.txt" | median) ms"
  col=$((col + 1))
done
row "worker environments" "$(ls "$WORK/worker/environments" 2>/dev/null | grep -c '^[0-9a-f]\{64\}$')"

say "remote execution, warm worker (the environment is already there)"
: > "$WORK/warm.txt"
for i in $(seq 1 "$ROUNDS"); do
  rm -rf "$WORK/home-rw"
  printf 'fn main() { println!("rw%s"); }\n' "$i" > "$WORK/remote/src/main.rs"
  run "$WORK/remote" "$WORK/home-rw" > "$WORK/o.json"
  for k in stage_ms upload_ms execute_ms total_ms; do
    printf '%s ' "$(field "$k" < "$WORK/o.json")"
  done >> "$WORK/warm.txt"
  echo >> "$WORK/warm.txt"
done
col=1
for label in stage upload execute total; do
  row "$label" "$(awk -v c="$col" '{print $c}' "$WORK/warm.txt" | median) ms"
  col=$((col + 1))
done

say "remote cache hit (another machine, nothing executes)"
printf 'fn main() { println!("shared"); }\n' > "$WORK/remote/src/main.rs"
run "$WORK/remote" "$WORK/home-rw" >/dev/null
mkproject "$WORK/hit" true
cp "$WORK/remote/arc-env.lock" "$WORK/hit/arc-env.lock"
cp "$WORK/remote/src/main.rs" "$WORK/hit/src/main.rs"
for i in $(seq 1 "$ROUNDS"); do
  rm -rf "$WORK/home-hit"
  run "$WORK/hit" "$WORK/home-hit" | field duration_ms
done | median > "$WORK/m"
row "restore" "$(cat "$WORK/m") ms"

say "ten tasks sharing one environment"
for i in $(seq 1 10); do
  rm -rf "$WORK/t$i"; mkproject "$WORK/t$i" true
  cp "$WORK/remote/arc-env.lock" "$WORK/t$i/arc-env.lock"
  printf 'fn main() { println!("t%s"); }\n' "$i" > "$WORK/t$i/src/main.rs"
done
cold_worker
START=$(date +%s%N)
CLIENTS=()
for i in $(seq 1 10); do
  (cd "$WORK/t$i" && ARC_HOME="$WORK/home-t$i" "$ARC" run --json "${BUILD[@]}" >/dev/null 2>&1) &
  CLIENTS+=($!)
done
for pid in "${CLIENTS[@]}"; do wait "$pid"; done
row "wall" "$(( ($(date +%s%N) - START) / 1000000 )) ms for 10 tasks"
row "materialisations" "$(ls "$WORK/worker/environments" 2>/dev/null | grep -c '^[0-9a-f]\{64\}$')"

say "no-environment regression (v0.7 host mode, unchanged)"
mkdir -p "$WORK/plain/src"
cat > "$WORK/plain/arc.toml" <<TOML
[outputs]
include = ["*.rmeta"]

[remote]
url = "http://127.0.0.1:7940"
namespace = "plain"
TOML
printf 'fn main() { println!("plain"); }\n' > "$WORK/plain/src/main.rs"
for i in $(seq 1 "$ROUNDS"); do
  rm -rf "$WORK/home-p"
  printf 'fn main() { println!("p%s"); }\n' "$i" > "$WORK/plain/src/main.rs"
  run "$WORK/plain" "$WORK/home-p" | field duration_ms
done | median > "$WORK/m"
row "local miss" "$(cat "$WORK/m") ms"
for i in $(seq 1 "$ROUNDS"); do
  run "$WORK/plain" "$WORK/home-p" | field duration_ms
done | median > "$WORK/m"
row "local hit" "$(cat "$WORK/m") ms"

echo
