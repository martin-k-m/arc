#!/usr/bin/env bash
# What remote execution actually costs.
#
# Real binaries, real HTTP, real sandboxes. Every number comes from
# `arc run --json`, which reports its own component timings, so nothing here is
# an estimate of Arc's overhead — it is Arc's overhead.
set -uo pipefail

cd "$(dirname "$0")/.."
cargo build --release -p arc-cli -p arc-cache -p arc-worker >/dev/null
TARGET=${CARGO_TARGET_DIR:-$PWD/target}
ARC="$TARGET/release/arc"
CACHE_BIN="$TARGET/release/arc-cache"
WORKER_BIN="$TARGET/release/arc-worker"

WORK=$(mktemp -d)
CACHE_PID=""; WORKER_PID=""
cleanup() {
  [ -n "$CACHE_PID" ] && kill "$CACHE_PID" 2>/dev/null
  [ -n "$WORKER_PID" ] && kill "$WORKER_PID" 2>/dev/null
  rm -rf "$WORK"
}
trap cleanup EXIT
export ARC_NO_ANIM=1

"$CACHE_BIN" serve --listen 127.0.0.1:7910 --data "$WORK/cache" >/dev/null 2>&1 &
CACHE_PID=$!
start_worker() {
  "$WORKER_BIN" serve --listen 127.0.0.1:7911 --data "$WORK/worker"     --cache-url http://127.0.0.1:7910 --max-jobs 8 >/dev/null 2>&1 &
  WORKER_PID=$!
  sleep 1
}

# A cold worker is one with an empty store. Deleting the directory underneath a
# running worker is not the same thing: that breaks it rather than emptying it.
cold_worker() {
  [ -n "$WORKER_PID" ] && kill "$WORKER_PID" 2>/dev/null && wait "$WORKER_PID" 2>/dev/null
  rm -rf "$WORK/worker"
  start_worker
}

start_worker

ROUNDS=${ROUNDS:-5}
# Roughly two seconds of work, so transfer and control costs are visible
# against it rather than lost in it.
WORKLOAD='mkdir -p out && sleep 2 && cat src/f1.txt > out/built.txt && echo done'

mkproject() {
  local dir=$1 enabled=$2 n pad
  mkdir -p "$dir/src"
  cat > "$dir/arc.toml" <<TOML
[outputs]
include = ["out/**"]

[remote]
url = "http://127.0.0.1:7910"
namespace = "bench"

[remote.execution]
enabled = $enabled
url = "http://127.0.0.1:7911"
TOML
  # A realistic input set: many small files plus one large one.
  pad=$(head -c 2000 /dev/zero | tr '\0' 'x')
  for n in $(seq 1 200); do
    printf 'source file %s\n%s\n' "$n" "$pad" > "$dir/src/f$n.txt"
  done
  head -c 4000000 /dev/urandom | base64 > "$dir/src/big.txt"
}

# The JSON `arc run --json` writes is a single line, and every key read below
# appears in it exactly once — so no JSON parser is needed, and none can be
# assumed to be installed.
field() { grep -o "\"$1\":[0-9]*" | head -1 | cut -d: -f2; }
median() { sort -n | awk '{v[NR]=$1} END {print (NR ? v[int((NR+1)/2)] : 0)}'; }
row() { printf '  %-14s %s\n' "$1" "$2"; }
say() { printf '\n\033[1m%s\033[0m\n' "$1"; }

run() { (cd "$1" && ARC_HOME="$2" "$ARC" run --json sh -c "$WORKLOAD" 2>&1 >/dev/null); }

# --------------------------------------------------------------------------

say "local cold execution (remote execution off)"
mkproject "$WORK/local" false
for i in $(seq 1 "$ROUNDS"); do
  rm -rf "$WORK/home-local"; echo "round $i" > "$WORK/local/src/vary.txt"
  run "$WORK/local" "$WORK/home-local" | field duration_ms
done | median > "$WORK/m"
row total "$(cat "$WORK/m") ms"

say "remote cold execution (the worker holds none of these inputs)"
mkproject "$WORK/remote" true
: > "$WORK/cold.txt"
for i in $(seq 1 "$ROUNDS"); do
  rm -rf "$WORK/home-remote"
  cold_worker
  echo "round $i" > "$WORK/remote/src/vary.txt"
  run "$WORK/remote" "$WORK/home-remote" > "$WORK/out.json"
  for k in stage_ms upload_ms queue_ms execute_ms fetch_ms total_ms input_bytes uploaded_objects; do
    printf '%s ' "$(field "$k" < "$WORK/out.json")"
  done >> "$WORK/cold.txt"
  echo >> "$WORK/cold.txt"
done
col=1
for label in stage upload queue execute fetch total input_bytes uploaded; do
  unit=ms
  case $label in input_bytes) unit=bytes ;; uploaded) unit=objects ;; esac
  row "$label" "$(awk -v c="$col" '{print $c}' "$WORK/cold.txt" | median) $unit"
  col=$((col + 1))
done

say "remote execution, warm worker store (same inputs, different key)"
: > "$WORK/warm.txt"
for i in $(seq 1 "$ROUNDS"); do
  rm -rf "$WORK/home-warm"; echo "warm $i" > "$WORK/remote/src/vary.txt"
  run "$WORK/remote" "$WORK/home-warm" > "$WORK/out.json"
  printf '%s %s %s\n' \
    "$(field upload_ms < "$WORK/out.json")" \
    "$(field uploaded_objects < "$WORK/out.json")" \
    "$(field total_ms < "$WORK/out.json")" >> "$WORK/warm.txt"
done
row upload "$(awk '{print $1}' "$WORK/warm.txt" | median) ms"
row uploaded "$(awk '{print $2}' "$WORK/warm.txt" | median) objects"
row total "$(awk '{print $3}' "$WORK/warm.txt" | median) ms"

say "remote cache hit (another machine; nothing executes)"
echo "shared" > "$WORK/remote/src/vary.txt"
run "$WORK/remote" "$WORK/home-remote" >/dev/null
mkproject "$WORK/hit" true
cp "$WORK/remote/src/vary.txt" "$WORK/hit/src/vary.txt"
for i in $(seq 1 "$ROUNDS"); do
  rm -rf "$WORK/home-hit"
  run "$WORK/hit" "$WORK/home-hit" | field duration_ms
done | median > "$WORK/m"
row restore "$(cat "$WORK/m") ms"

say "local cache hit (same machine; no network)"
for i in $(seq 1 "$ROUNDS"); do
  run "$WORK/remote" "$WORK/home-remote" | field duration_ms
done | median > "$WORK/m"
row restore "$(cat "$WORK/m") ms"

say "singleflight: 8 clients, one execution"
echo "singleflight" > "$WORK/remote/src/vary.txt"
for i in $(seq 1 8); do
  rm -rf "$WORK/sf$i"; mkproject "$WORK/sf$i" true
  cp "$WORK/remote/src/vary.txt" "$WORK/sf$i/src/vary.txt"
done
START=$(date +%s%N)
# Only the clients: the cache and the worker are background jobs of this shell
# too, and a bare `wait` would block on them forever.
CLIENTS=()
for i in $(seq 1 8); do
  (cd "$WORK/sf$i" && ARC_HOME="$WORK/home-sf$i" "$ARC" run --json sh -c "$WORKLOAD" >/dev/null 2>&1) &
  CLIENTS+=($!)
done
for pid in "${CLIENTS[@]}"; do wait "$pid"; done
row wall "$(( ($(date +%s%N) - START) / 1000000 )) ms for 8 clients"

say "requests to the cache for one remote execution"
rm -rf "$WORK/home-count"; echo "count" > "$WORK/remote/src/vary.txt"
run "$WORK/remote" "$WORK/home-count" > "$WORK/out.json"
row requests "$(field requests < "$WORK/out.json")"
row "input objects" "$(field input_objects < "$WORK/out.json")"
