#!/usr/bin/env bash
# Measure what Arc costs and what it saves.
#
# Four numbers per workload, each the median of N runs:
#
#   direct        the command on its own
#   arc cold      first Arc run: trace, learn, store
#   arc miss      an input changed: trace, learn, store again
#   arc warm      everything matched: fingerprint and replay
#
# Plus, where `strace` is available, the same command under `strace -f` — the
# reference point for what syscall-level tracing costs before Arc does anything
# with it.
set -euo pipefail

ARC=${ARC:-$(pwd)/target/release/arc}
REPS=${REPS:-7}
WORK=${WORK:-/tmp/arc-bench}
export ARC_HOME=$WORK/home
export ARC_NO_ANIM=1

ms() { date +%s%3N; }

# Median wall-clock of REPS runs, in milliseconds.
median() {
  local -a t=()
  local i start
  for ((i = 0; i < REPS; i++)); do
    start=$(ms)
    "$@" >/dev/null 2>&1 || true
    t+=($(($(ms) - start)))
  done
  printf '%s\n' "${t[@]}" | sort -n | awk -v n="$REPS" 'NR==int((n+1)/2){print $1}'
}

row() { printf '| %-28s | %8s | %8s | %8s | %8s |\n' "$@"; }

setup_small() {
  rm -rf "$WORK/small" && mkdir -p "$WORK/small"
  echo one > "$WORK/small/input.txt"
  CMD=(sh -c 'cat input.txt')
  DIR=$WORK/small
  TOUCH=$WORK/small/input.txt
}

setup_many() {
  rm -rf "$WORK/many" && mkdir -p "$WORK/many/data"
  for i in $(seq 1 400); do head -c 4096 /dev/zero > "$WORK/many/data/f$i"; done
  CMD=(sh -c 'cat data/* > /dev/null')
  DIR=$WORK/many
  TOUCH=$WORK/many/data/f1
}

# A real compiler, invoked directly. `cargo build` would be more familiar but
# it is incremental, so most of its runs measure cargo deciding there is nothing
# to do rather than a compile — which flatters the cache and tells you nothing.
setup_build() {
  rm -rf "$WORK/build" && mkdir -p "$WORK/build/src"
  : > "$WORK/build/src/lib.rs"
  for i in $(seq 1 120); do
    echo "pub mod m$i;" >> "$WORK/build/src/lib.rs"
    printf 'pub fn f(x: u64) -> u64 { (0..64u64).fold(x, |a, b| a.wrapping_mul(b + %d)) }\n' "$i" \
      > "$WORK/build/src/m$i.rs"
  done
  CMD=(rustc src/lib.rs --crate-type lib --edition 2021 -o /dev/null)
  DIR=$WORK/build
  TOUCH=$WORK/build/src/m1.rs
}

bench() {
  local name=$1 setup=$2
  $setup
  cd "$DIR"

  local direct arc_cold arc_miss arc_warm
  direct=$(median "${CMD[@]}")

  rm -rf "$ARC_HOME"
  arc_cold=$(median_cold)
  # A settled cache: the run after learning is the first that can narrow.
  "$ARC" run "${CMD[@]}" >/dev/null 2>&1 || true
  arc_warm=$(median "$ARC" run "${CMD[@]}")
  arc_miss=$(median_miss)

  row "$name" "${direct}ms" "${arc_cold}ms" "${arc_miss}ms" "${arc_warm}ms"
  cd - >/dev/null
}

# A cold run has to start from an empty cache every time, so it cannot use the
# plain median helper.
median_cold() {
  local -a t=()
  local i start
  for ((i = 0; i < REPS; i++)); do
    rm -rf "$ARC_HOME"
    start=$(ms)
    "$ARC" run "${CMD[@]}" >/dev/null 2>&1 || true
    t+=($(($(ms) - start)))
  done
  printf '%s\n' "${t[@]}" | sort -n | awk -v n="$REPS" 'NR==int((n+1)/2){print $1}'
}

# Each iteration must genuinely change an input, or it would measure a hit.
median_miss() {
  local -a t=()
  local i start
  for ((i = 0; i < REPS; i++)); do
    echo "change $i" >> "$TOUCH"
    start=$(ms)
    "$ARC" run "${CMD[@]}" >/dev/null 2>&1 || true
    t+=($(($(ms) - start)))
  done
  printf '%s\n' "${t[@]}" | sort -n | awk -v n="$REPS" 'NR==int((n+1)/2){print $1}'
}

echo
echo "arc bench · median of $REPS · $(uname -sm)"
"$ARC" doctor 2>/dev/null | grep -E 'backend|available|narrowing' | sed 's/^/  /'
echo
row "workload" "direct" "arc cold" "arc miss" "arc warm"
echo "|------------------------------|----------|----------|----------|----------|"
bench "tiny (cat one file)" setup_small
bench "many (400 file reads)" setup_many
bench "build (rustc, 120 modules)" setup_build

if command -v strace >/dev/null; then
  echo
  echo "syscall tracing reference:"
  setup_many
  cd "$DIR"
  printf '  %-28s %sms\n' "direct" "$(median "${CMD[@]}")"
  printf '  %-28s %sms\n' "strace -f -o /dev/null" "$(median strace -f -o /dev/null "${CMD[@]}")"
  cd - >/dev/null
fi
echo
