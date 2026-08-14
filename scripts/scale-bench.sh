#!/usr/bin/env bash
# How Arc behaves as a repository gets large.
#
# The question this answers is not "how fast is Arc" but "does anything here
# grow faster than the repository does". Every workload runs at 1k, 10k and
# 100k files, and the per-file cost is printed next to the total: if that column
# is flat, the operation is linear, and if it climbs, something is quadratic.
#
# Two paths are measured separately, because they are genuinely different:
#
#   narrowed      a complete trace proved the command reads one file, so Arc
#                 fingerprints one file
#   conservative  no complete trace, so Arc scans the whole project
#
# The second is the one that has to stay linear — it is what every platform
# without a read-observing backend does on every run.
set -uo pipefail

cd "$(dirname "$0")/.."
cargo build --release -q -p arc-cli || exit 1
TARGET=${CARGO_TARGET_DIR:-$PWD/target}
ARC="$TARGET/release/arc"

WORK=${WORK:-$(mktemp -d)}
# Git leaves its object store read-only, which plain `rm -rf` will not remove.
trap 'chmod -R u+w "$WORK" 2>/dev/null; rm -rf "$WORK"' EXIT
export ARC_NO_ANIM=1

REPS=${REPS:-3}
SIZES=${SIZES:-"1000 10000 100000"}

ms() { date +%s%3N; }
median() { sort -n | awk '{v[NR]=$1} END {print (NR ? v[int((NR+1)/2)] : 0)}'; }
say() { printf '\n\033[1m%s\033[0m\n' "$1"; }
head4() { printf '  %-14s %10s %10s %12s\n' "$1" "$2" "$3" "$4"; }

# A project of `n` files spread over 100 directories, so no single directory
# holds all of them — a flat directory of 100k files is its own filesystem
# benchmark and not what a repository looks like.
mkproject() {
  local dir=$1 n=$2 i d
  rm -rf "$dir"
  mkdir -p "$dir/src"
  printf '[outputs]\ninclude = ["out/**"]\n\n[trace]\nbackend = "%s"\n' "${3:-auto}" \
    > "$dir/arc.toml"
  mkdir -p "$dir/out"
  for d in $(seq 0 99); do mkdir -p "$dir/src/d$d"; done
  i=0
  while [ "$i" -lt "$n" ]; do
    printf 'package %s\nconst value = %s\n' "$i" "$i" > "$dir/src/d$((i % 100))/f$i.txt"
    i=$((i + 1))
  done
  echo "seed" > "$dir/src/main.txt"
}

# One timed run of the real binary.
timed() {
  local dir=$1 home=$2
  shift 2
  local start
  start=$(ms)
  (cd "$dir" && ARC_HOME="$home" "$ARC" "$@" >/dev/null 2>&1)
  echo $(( $(ms) - start ))
}

# Peak resident set of one run, in MB. Reported only where GNU time exists;
# a made-up number would be worse than none.
peak_mb() {
  local dir=$1 home=$2
  shift 2
  if ! command -v /usr/bin/time >/dev/null; then echo "-"; return; fi
  local kb
  kb=$( (cd "$dir" && ARC_HOME="$home" /usr/bin/time -f '%M' "$ARC" "$@" >/dev/null) 2>&1 |
        tail -1 )
  case "$kb" in
    ''|*[!0-9]*) echo "-" ;;
    *) echo $(( kb / 1024 )) ;;
  esac
}

# `total  per-1k-files` — the second column is what to read.
per() {
  local total=$1 n=$2
  awk -v t="$total" -v n="$n" 'BEGIN{printf "%.2f", t / (n / 1000)}'
}

"$ARC" --version
printf 'sizes: %s   reps: %s (median)\n' "$SIZES" "$REPS"
# Roughly 13 ms of every number below is process start and database open, and
# does not scale with the project. A per-1k column that *falls* as the project
# grows is that fixed cost being amortised; one that rises is the problem this
# script exists to catch.

# --------------------------------------------------------------------------

say "building the projects"
for n in $SIZES; do
  start=$(ms)
  mkproject "$WORK/p$n" "$n"
  printf '  %-14s %s files in %s ms\n' "$n" "$n" $(( $(ms) - start ))
done

say "cold run — scan, trace, learn, store (command reads one file)"
head4 files total per-1k peak-MB
for n in $SIZES; do
  m=$(for _ in $(seq 1 "$REPS"); do
        rm -rf "$WORK/h$n"
        timed "$WORK/p$n" "$WORK/h$n" run --refresh -- sh -c 'cat src/main.txt'
      done | median)
  rm -rf "$WORK/h$n"
  mb=$(peak_mb "$WORK/p$n" "$WORK/h$n" run --refresh -- sh -c 'cat src/main.txt')
  head4 "$n" "$m ms" "$(per "$m" "$n") ms" "$mb"
done

say "warm hit — the result is already known"
head4 files total per-1k peak-MB
for n in $SIZES; do
  # Prime it, then measure only hits.
  timed "$WORK/p$n" "$WORK/h$n" run -- sh -c 'cat src/main.txt' >/dev/null
  m=$(for _ in $(seq 1 "$REPS"); do
        timed "$WORK/p$n" "$WORK/h$n" run -- sh -c 'cat src/main.txt'
      done | median)
  mb=$(peak_mb "$WORK/p$n" "$WORK/h$n" run -- sh -c 'cat src/main.txt')
  head4 "$n" "$m ms" "$(per "$m" "$n") ms" "$mb"
done

say "miss after touching one unrelated file"
head4 files total per-1k ''
for n in $SIZES; do
  m=$(for i in $(seq 1 "$REPS"); do
        echo "changed $i" > "$WORK/p$n/src/d0/f0.txt"
        timed "$WORK/p$n" "$WORK/h$n" run -- sh -c 'cat src/main.txt'
      done | median)
  head4 "$n" "$m ms" "$(per "$m" "$n") ms" ''
done

say "conservative path — no complete trace, so the whole project is scanned"
head4 files total per-1k peak-MB
for n in $SIZES; do
  mkproject "$WORK/c$n" "$n" snapshot
  rm -rf "$WORK/ch$n"
  timed "$WORK/c$n" "$WORK/ch$n" run -- sh -c 'cat src/main.txt' >/dev/null
  m=$(for _ in $(seq 1 "$REPS"); do
        timed "$WORK/c$n" "$WORK/ch$n" run -- sh -c 'cat src/main.txt'
      done | median)
  mb=$(peak_mb "$WORK/c$n" "$WORK/ch$n" run -- sh -c 'cat src/main.txt')
  head4 "$n" "$m ms" "$(per "$m" "$n") ms" "$mb"
done

say "arc affected — against a real git history"
head4 files total per-1k ''
for n in $SIZES; do
  d=$WORK/p$n
  if [ ! -d "$d/.git" ]; then
    (cd "$d" && git init -q && git config user.email b@e && git config user.name b &&
     git add -A >/dev/null 2>&1 && git commit -qm base >/dev/null 2>&1)
  fi
  echo "touched" > "$d/src/d0/f0.txt"
  m=$(for _ in $(seq 1 "$REPS"); do
        timed "$d" "$WORK/h$n" affected
      done | median)
  head4 "$n" "$m ms" "$(per "$m" "$n") ms" ''
done

say "arc graph, history, cache stats — should not depend on project size"
head4 files graph history cache-stats
for n in $SIZES; do
  g=$(for _ in $(seq 1 "$REPS"); do timed "$WORK/p$n" "$WORK/h$n" graph; done | median)
  h=$(for _ in $(seq 1 "$REPS"); do timed "$WORK/p$n" "$WORK/h$n" history; done | median)
  c=$(for _ in $(seq 1 "$REPS"); do timed "$WORK/p$n" "$WORK/h$n" cache stats; done | median)
  head4 "$n" "$g ms" "$h ms" "$c ms"
done

say "cache size on disk"
head4 files objects bytes ''
for n in $SIZES; do
  objects=$(find "$WORK/h$n" -type f 2>/dev/null | wc -l)
  bytes=$(du -sk "$WORK/h$n" 2>/dev/null | cut -f1)
  head4 "$n" "$objects" "$(( ${bytes:-0} / 1024 )) MB" ''
done
