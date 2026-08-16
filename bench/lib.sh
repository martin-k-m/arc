#!/usr/bin/env bash
# Shared timing helpers for the bench/ scripts.
#
# Every number Arc publishes comes from here. The rules are:
#
#   * one warmup run per measurement, discarded, so page cache and any
#     incremental build state are settled before the clock starts;
#   * REPS timed runs after that;
#   * report the median and the top order statistic, never the mean;
#   * no outlier is removed. If a run was slow it stays in the sample.
#
# A note on "p99". These samples are small. With REPS runs the highest
# observed value IS the 1/REPS tail, so what the tables call p99 is the
# maximum of the sample and nothing more. It is reported because the spread
# between it and the median is the interesting part -- a warm hit whose
# median is 20 ms and whose max is 400 ms is not a 20 ms operation. It is
# not an estimate of a true 99th percentile and must not be read as one.

set -euo pipefail

REPS=${REPS:-11}

ms() { date +%s%3N; }

# _run_timed <setup-fn-or-:> <cmd...>
# Runs the setup hook, then the command, and echoes the elapsed ms.
_run_timed() {
  local prep=$1; shift
  $prep
  local start
  start=$(ms)
  "$@" >/dev/null 2>&1 || true
  echo $(($(ms) - start))
}

# samples <setup-fn-or-:> <cmd...>
# One discarded warmup, then REPS timed runs. Echoes the sorted samples.
samples() {
  local prep=$1; shift
  local -a t=()
  local i
  _run_timed "$prep" "$@" >/dev/null
  for ((i = 0; i < REPS; i++)); do
    t+=("$(_run_timed "$prep" "$@")")
  done
  printf '%s\n' "${t[@]}" | sort -n
}

# stats <sorted samples on stdin> -> "median max min"
stats() {
  awk '{v[NR]=$1} END {
    n=NR
    m = (n % 2) ? v[int((n+1)/2)] : int((v[n/2] + v[n/2+1]) / 2)
    printf "%d %d %d\n", m, v[n], v[1]
  }'
}

# measure <label> <setup-fn-or-:> <cmd...>
# Sets MED, MAX, MIN in the caller.
measure() {
  local label=$1; shift
  local prep=$1; shift
  local s
  s=$(samples "$prep" "$@" | stats)
  MED=${s% * *}; MED=${s%% *}
  MAX=$(echo "$s" | cut -d' ' -f2)
  MIN=$(echo "$s" | cut -d' ' -f3)
  printf '  %-38s median %6s ms   max %6s ms   min %6s ms\n' \
    "$label" "$MED" "$MAX" "$MIN" >&2
}

# pct <a> <b> -> percentage change of a relative to b, one decimal
pct() {
  awk -v a="$1" -v b="$2" 'BEGIN { if (b == 0) print "n/a"; else printf "%+.1f%%", (a - b) * 100.0 / b }'
}

# ratio <a> <b> -> a/b to two decimals
ratio() {
  awk -v a="$1" -v b="$2" 'BEGIN { if (b == 0) print "n/a"; else printf "%.2f", a / b }'
}

# dir_bytes <path>
dir_bytes() { du -sb "$1" 2>/dev/null | cut -f1; }

human() {
  awk -v b="$1" 'BEGIN {
    split("B KB MB GB", u, " "); i = 1
    while (b >= 1024 && i < 4) { b /= 1024; i++ }
    printf (i == 1 ? "%d %s\n" : "%.1f %s\n"), b, u[i]
  }'
}
