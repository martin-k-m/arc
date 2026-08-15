#!/usr/bin/env bash
# The number that decides whether Arc is worth running: how often does it
# actually hit?
#
# A cache that is fast and never hits is worthless, and a hit rate measured
# by running the same command twice is not a measurement of anything. So
# this replays a real repository's real commit history, oldest first, and
# runs a real task at every commit. A hit means that commit changed nothing
# the task reads.
#
#   bench/hit-rate.sh <project-dir> <commits> <task-name>=<command> [...]
#
# Every task runs at every commit. The result is written as JSON with one
# row per (commit, task) so the raw sequence can be re-examined, plus the
# per-task and overall rates.
#
# Two things this deliberately does NOT do:
#
#   * it does not re-run a commit to manufacture a hit. Each (commit, task)
#     pair is executed exactly once, in history order, which is what a CI
#     system building every commit on a branch would see;
#   * it does not skip merge commits or reorder anything. The history is
#     replayed as it is.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
. "$here/lib.sh"

ARC=${ARC:-/work/arc/target/release/arc}
export ARC_NO_ANIM=1

DIR=$1; shift
N=$1; shift

name=$(basename "$DIR")
export ARC_HOME=${ARC_HOME:-/work/bench/home-hitrate-$name}
out=${JSON_OUT:-$here/results/hit-rate-$name.json}
mkdir -p "$(dirname "$out")"
rm -rf "$ARC_HOME"

cd "$DIR"
start_ref=$(git rev-parse HEAD)
mapfile -t commits < <(git rev-list --reverse -n "$N" HEAD)

echo "hit rate · $name · ${#commits[@]} commits · $# tasks" >&2
echo "  newest $start_ref" >&2
echo "  oldest ${commits[0]}" >&2
echo >&2

rows=()
declare -A hits total
tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT

for c in "${commits[@]}"; do
  git checkout -q --detach "$c" -- 2>/dev/null || git checkout -q --detach "$c"
  # arc.toml is the experiment's own configuration and must survive the
  # replay; the rest are build state that does not belong to any commit.
  git clean -qfd -e arc.toml -e target -e config.mak -e config.h -e '*.egg-info' 2>/dev/null || true
  short=${c:0:8}
  changed=$(git diff-tree --no-commit-id --name-only -r "$c" | wc -l)
  line="  $short ($changed files)"
  for spec in "$@"; do
    task=${spec%%=*}
    cmd=${spec#*=}
    # Arc streams the child's output to stdout and reports on stderr, so the
    # JSON record is on stderr. Reading stdout here silently returns nothing,
    # which scores every run as an error -- a mistake worth naming, because
    # nothing about the output looks wrong when it happens.
    $ARC run --json sh -c "$cmd" >/dev/null 2>"$tmp"
    st=$(grep -o '"cache_status":"[A-Z_]*"' "$tmp" | head -1 | cut -d'"' -f4)
    comp=$(grep -o '"completeness":"[a-z]*"' "$tmp" | head -1 | cut -d'"' -f4)
    nar=$(grep -o '"inputs_narrowed":[a-z]*' "$tmp" | head -1 | cut -d: -f2)
    st=${st:-ERROR}; comp=${comp:-none}; nar=${nar:-false}
    total[$task]=$(( ${total[$task]:-0} + 1 ))
    [ "$st" = "HIT" ] && hits[$task]=$(( ${hits[$task]:-0} + 1 ))
    rows+=("{\"commit\":\"$c\",\"task\":\"$task\",\"status\":\"$st\",\"completeness\":\"$comp\",\"narrowed\":$nar,\"files_changed\":$changed}")
    line="$line  $task=$st"
  done
  echo "$line" >&2
done

git checkout -q --detach "$start_ref"

th=0; tt=0
per=()
for task in "${!total[@]}"; do
  h=${hits[$task]:-0}; t=${total[$task]}
  th=$((th + h)); tt=$((tt + t))
  per+=("{\"task\":\"$task\",\"runs\":$t,\"hits\":$h,\"rate\":$(awk -v h="$h" -v t="$t" 'BEGIN{printf "%.4f", h/t}')}")
done

{
  echo "{"
  echo "  \"project\": \"$name\","
  echo "  \"commits\": ${#commits[@]},"
  echo "  \"oldest\": \"${commits[0]}\","
  echo "  \"newest\": \"$start_ref\","
  echo "  \"task_runs\": $tt,"
  echo "  \"hits\": $th,"
  echo "  \"hit_rate\": $(awk -v h="$th" -v t="$tt" 'BEGIN{printf "%.4f", h/t}'),"
  echo "  \"per_task\": [$(IFS=,; echo "${per[*]}")],"
  echo "  \"runs\": [$(IFS=,; echo "${rows[*]}")]"
  echo "}"
} > "$out"

echo >&2
awk -v h="$th" -v t="$tt" 'BEGIN{printf "  overall: %d/%d = %.1f%% hit rate\n", h, t, 100*h/t}' >&2
echo "  wrote $out" >&2
