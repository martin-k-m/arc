#!/usr/bin/env bash
# What each tracing backend costs.
#
# Every number is wall-clock time for the same command, first run directly and
# then through Arc with each backend pinned. Nothing is cached: every round gets
# a fresh Arc home and a changed input, so what is being measured is the cost of
# *learning* dependencies, which is the cost tracing is responsible for.
#
# Linux only: `snapshot` exists everywhere, but `linux-ptrace` and
# `linux-seccomp` are what this is comparing.
set -uo pipefail

cd "$(dirname "$0")/.."
cargo build --release -p arc-cli >/dev/null || exit 1
TARGET=${CARGO_TARGET_DIR:-$PWD/target}
ARC="$TARGET/release/arc"

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
export ARC_NO_ANIM=1

ROUNDS=${ROUNDS:-5}

median() { sort -n | awk '{v[NR]=$1} END {print (NR ? v[int((NR+1)/2)] : 0)}'; }
say() { printf '\n\033[1m%s\033[0m\n' "$1"; }
row() { printf '  %-16s %8s ms   %s\n' "$1" "$2" "${3:-}"; }
ms_now() { date +%s%N; }

mkproject() {
  local dir=$1 n
  rm -rf "$dir"
  mkdir -p "$dir/src" "$dir/out"
  printf '[outputs]\ninclude = ["out/**"]\n' > "$dir/arc.toml"
  for n in $(seq 1 300); do echo "file $n" > "$dir/src/f$n.txt"; done
}

# One timed round: fresh Arc home, changed input, so nothing can hit.
timed() {
  local dir=$1 home=$2 backend=$3 script=$4 start
  rm -rf "$home"
  echo "$RANDOM" > "$dir/src/vary.txt"
  start=$(ms_now)
  if [ "$backend" = direct ]; then
    (cd "$dir" && sh -c "$script" >/dev/null 2>&1)
  else
    (cd "$dir" && ARC_HOME="$home" "$ARC" run --trace-backend "$backend" \
      --refresh -- sh -c "$script" >/dev/null 2>&1)
  fi
  echo $(( ($(ms_now) - start) / 1000000 ))
}

# Which backends can actually run here. A pinned backend falls back rather than
# failing, so asking `doctor` is the only honest way to know.
BACKENDS=(direct snapshot)
DOCTOR=$("$ARC" doctor 2>&1)
grep -q 'linux-ptrace *available' <<<"$DOCTOR" && BACKENDS+=(ptrace)
grep -q 'linux-seccomp *available' <<<"$DOCTOR" && BACKENDS+=(fast)

printf 'backends: %s\n' "${BACKENDS[*]}"
printf 'rounds:   %s (median reported)\n' "$ROUNDS"

# `snapshot` is Arc doing everything except watching syscalls, so the difference
# between a backend and snapshot is what that backend's tracing actually costs.
# Comparing against `direct` instead would charge tracing for Arc's fingerprint,
# capture and bookkeeping as well.
bench() {
  local name=$1 script=$2 b direct=0 snap=0 m
  say "$name"
  mkproject "$WORK/p"
  for b in "${BACKENDS[@]}"; do
    m=$(for _ in $(seq 1 "$ROUNDS"); do
          timed "$WORK/p" "$WORK/home" "$b" "$script"
        done | median)
    case "$b" in
      direct)   direct=$m; row "$b" "$m" "the command alone" ;;
      snapshot) snap=$m;   row "$b" "$m" "Arc, no syscall tracing" ;;
      *)        row "$b" "$m" "tracing costs $((m - snap)) ms" ;;
    esac
  done
}

bench "syscall-dense: read 300 files" \
  'for f in src/*.txt; do cat "$f" > /dev/null; done'

bench "process-heavy: 200 short-lived children" \
  'for i in $(seq 1 200); do /bin/true; done'

bench "directory-heavy: 200 enumerations" \
  'for i in $(seq 1 200); do ls src > /dev/null; done'

bench "write-heavy: produce 200 outputs" \
  'for i in $(seq 1 200); do echo "$i" > "out/o$i.txt"; done'

bench "mixed: read, build, write" \
  'cat src/f1.txt src/f2.txt > out/joined.txt && wc -l out/joined.txt > /dev/null'

# The warm hit is the number users see most often, and it must not have
# regressed: it does not trace at all.
say "warm cache hit (no tracing; the result is already known)"
mkproject "$WORK/w"
rm -rf "$WORK/home-w"
(cd "$WORK/w" && ARC_HOME="$WORK/home-w" "$ARC" run -- sh -c 'cat src/f1.txt > /dev/null' >/dev/null 2>&1)
m=$(for _ in $(seq 1 "$ROUNDS"); do
      start=$(ms_now)
      (cd "$WORK/w" && ARC_HOME="$WORK/home-w" "$ARC" run -- sh -c 'cat src/f1.txt > /dev/null' >/dev/null 2>&1)
      echo $(( ($(ms_now) - start) / 1000000 ))
    done | median)
row hit "$m"

say "startup (nothing should be initialised for these)"
for cmd in --version --help; do
  m=$(for _ in $(seq 1 "$ROUNDS"); do
        start=$(ms_now); "$ARC" $cmd >/dev/null 2>&1
        echo $(( ($(ms_now) - start) / 1000000 ))
      done | median)
  row "arc $cmd" "$m"
done
