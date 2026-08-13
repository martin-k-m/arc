#!/usr/bin/env bash
# Remote execution, end to end, on one machine and without Docker.
#
# A cache, a worker, and several Arc homes standing in for several developers,
# then the cases that matter: a miss executed on a worker, the shared hit that
# follows it, an incompatible worker, a corrupted result, singleflight, a
# timeout, and a task graph.
set -uo pipefail

cd "$(dirname "$0")/.."
cargo build --release -p arc-cli -p arc-cache -p arc-worker >/dev/null
TARGET=${CARGO_TARGET_DIR:-$PWD/target}
ARC="$TARGET/release/arc"
CACHE_BIN="$TARGET/release/arc-cache"
WORKER_BIN="$TARGET/release/arc-worker"

WORK=$(mktemp -d)
CACHE_PID=""
WORKER_PID=""
cleanup() {
  [ -n "$CACHE_PID" ] && kill "$CACHE_PID" 2>/dev/null
  [ -n "$WORKER_PID" ] && kill "$WORKER_PID" 2>/dev/null
  rm -rf "$WORK"
}
trap cleanup EXIT
export ARC_NO_ANIM=1
# A demo should end, whatever happens. This is the ordinary client-side wait
# bound, set low so a stuck stage reports rather than sits there.
export ARC_REMOTE_EXECUTION_TIMEOUT_MS=60000

say() { printf '\n\033[1m== %s\033[0m\n' "$1"; }

start_cache() {
  "$CACHE_BIN" serve --listen 127.0.0.1:7920 --data "$WORK/cache" >"$WORK/cache.log" 2>&1 &
  CACHE_PID=$!
}
start_worker() {
  "$WORKER_BIN" serve --listen 127.0.0.1:7921 --data "$WORK/worker" \
    --cache-url http://127.0.0.1:7920 --max-jobs "${1:-2}" >"$WORK/worker.log" 2>&1 &
  WORKER_PID=$!
}
stop_services() {
  [ -n "$CACHE_PID" ] && kill "$CACHE_PID" 2>/dev/null && wait "$CACHE_PID" 2>/dev/null
  [ -n "$WORKER_PID" ] && kill "$WORKER_PID" 2>/dev/null && wait "$WORKER_PID" 2>/dev/null
  CACHE_PID=""; WORKER_PID=""
}

mkproject() {
  mkdir -p "$1/src"
  cat > "$1/arc.toml" <<TOML
[outputs]
include = ["out/**"]

[remote]
url = "http://127.0.0.1:7920"
namespace = "demo"

[remote.execution]
enabled = true
url = "http://127.0.0.1:7921"
TOML
  echo "the source of truth" > "$1/src/input.txt"
}

BUILD='mkdir -p out && sleep 2 && tr a-z A-Z < src/input.txt > out/built.txt && echo built'

say "A: a cache and a worker"
start_cache
start_worker 2
sleep 1

mkproject "$WORK/dev-a"
mkproject "$WORK/deep/nested/dev-b"
a() { (cd "$WORK/dev-a" && ARC_HOME="$WORK/home-a" "$ARC" "$@"); }
b() { (cd "$WORK/deep/nested/dev-b" && ARC_HOME="$WORK/home-b" "$ARC" "$@"); }

a remote status | sed -n '/execution/,$p'

say "B: a cache miss runs on the worker"
time a run sh -c "$BUILD"
echo "restored locally: $(cat "$WORK/dev-a/out/built.txt")"

say "C: another machine, a different checkout path — a cache hit, nothing runs"
time b run sh -c "$BUILD"
echo "restored: $(cat "$WORK/deep/nested/dev-b/out/built.txt")"

say "D: and again with both services stopped — a local hit"
stop_services
b run sh -c "$BUILD"
start_cache
start_worker 2
sleep 1

say "E: an argument naming a path on this machine keeps the command here"
echo "changed" > "$WORK/dev-a/src/input.txt"
a run --explain sh -c 'echo host-bound' /etc/hostname 2>&1 |
  grep -E "executed|remote execution" || true

say "F: a worker that cannot produce the same toolchain sends the work back"
mkdir -p "$WORK/fakebin"
printf '#!/bin/sh\nexec /bin/sh "$@"\n' > "$WORK/fakebin/sh"
chmod +x "$WORK/fakebin/sh"
(cd "$WORK/dev-a" && PATH="$WORK/fakebin:$PATH" ARC_HOME="$WORK/home-f" \
  "$ARC" run --explain sh -c 'echo mismatch' 2>&1) |
  grep -E "executed|remote execution" || true

say "G: a corrupted result is refused and nothing unsafe is restored"
echo "corrupt me" > "$WORK/dev-a/src/input.txt"
a run sh -c "$BUILD" >/dev/null 2>&1
find "$WORK/cache/objects" -type f -exec sh -c 'printf tampered > "$1"' _ {} \;
mkproject "$WORK/dev-g"
echo "corrupt me" > "$WORK/dev-g/src/input.txt"
(cd "$WORK/dev-g" && ARC_HOME="$WORK/home-g" "$ARC" run sh -c "$BUILD" 2>&1 | tail -3)
echo "restored: $(cat "$WORK/dev-g/out/built.txt" 2>/dev/null || echo '<nothing>')"
echo "(the bytes are 'CORRUPT ME', never 'tampered')"

say "H: eight clients, one execution"
echo "singleflight" > "$WORK/dev-a/src/input.txt"
for i in $(seq 1 8); do
  rm -rf "$WORK/sf$i"; mkproject "$WORK/sf$i"
  cp "$WORK/dev-a/src/input.txt" "$WORK/sf$i/src/input.txt"
done
START=$(date +%s%N)
# Wait for exactly these, not for every background job: the cache and the
# worker are also children of this shell, and they never exit.
CLIENTS=()
for i in $(seq 1 8); do
  (cd "$WORK/sf$i" && ARC_HOME="$WORK/home-sf$i" "$ARC" run sh -c "$BUILD" >/dev/null 2>&1) &
  CLIENTS+=($!)
done
for pid in "${CLIENTS[@]}"; do wait "$pid"; done
printf 'eight clients finished in %s ms for one 2s command\n' $(( ($(date +%s%N)-START)/1000000 ))
for i in 1 8; do echo "  client $i: $(cat "$WORK/sf$i/out/built.txt")"; done

say "I: a timeout kills the process tree and publishes nothing"
mkproject "$WORK/dev-t"
echo 'timeout_ms = 2000' >> "$WORK/dev-t/arc.toml"
(cd "$WORK/dev-t" && ARC_HOME="$WORK/home-t" "$ARC" run sh -c 'sleep 60 && echo never' 2>&1 | tail -4)
echo "exit: $?"
sleep 1
echo "stray sleepers: $(pgrep -c -f 'sleep 60' 2>/dev/null || echo 0)"

say "J: a graph — producer and consumer"
mkdir -p "$WORK/graph/src"
cat > "$WORK/graph/arc.toml" <<'TOML'
[remote]
url = "http://127.0.0.1:7920"
namespace = "demo"

[remote.execution]
enabled = true
url = "http://127.0.0.1:7921"

[[command]]
name = "gen"
command = "sh"
args = ["-c", "mkdir -p gen && tr a-z A-Z < src/input.txt > gen/out.txt && echo generated"]
inputs = ["src/input.txt"]
outputs = ["gen/**"]

[[command]]
name = "consume"
command = "sh"
args = ["-c", "cat gen/out.txt && echo consumed"]
inputs = ["gen/**"]
after = ["gen"]

[ci]
tasks = ["gen", "consume"]
TOML
echo "graph input" > "$WORK/graph/src/input.txt"
(cd "$WORK/graph" && git init -q -b main >/dev/null 2>&1
 git config user.email d@e.com; git config user.name d
 git add -A >/dev/null 2>&1; git commit -qm seed >/dev/null 2>&1
 ARC_HOME="$WORK/home-graph" "$ARC" ci --base HEAD -j 1)
echo "produced: $(cat "$WORK/graph/gen/out.txt" 2>/dev/null || echo '<none>')"

say "K: what the cache and the worker hold"
"$CACHE_BIN" stats --data "$WORK/cache"
"$WORKER_BIN" stats --data "$WORK/worker"
