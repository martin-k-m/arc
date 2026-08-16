#!/usr/bin/env bash
# The hit-rate experiment on click.
#
# Three test tasks, replayed across click's real recent history, oldest
# commit first, one run per (commit, task). A hit means that commit changed
# nothing the task reads.
#
# Why per-file tasks rather than one `pytest`: running the whole suite is a
# single task that every source change invalidates, so its hit rate is a
# measurement of how often a commit touches nothing, which is a fact about
# click's contributors and not about Arc. Splitting the suite is what a
# project actually does to get value from a cache, and it is also the only
# arrangement where the interesting case exists at all -- a commit that
# invalidates one task and not another.
#
# It is also the arrangement where a complete trace matters. The whole suite
# shells out to enough tools that it reads /sys and /proc and never narrows;
# a single test file does not, so Arc learns its real dependency set.
#
#   bench/hit-rate-click.sh [commits]
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
BENCH=${BENCH:-/work/bench}
N=${1:-60}
PY=$BENCH/venv-click/bin/python
DIR=$BENCH/click

tasks=(
  "basic=$PY -m pytest -q -p no:cacheprovider tests/test_basic.py"
  "options=$PY -m pytest -q -p no:cacheprovider tests/test_options.py"
  "arguments=$PY -m pytest -q -p no:cacheprovider tests/test_arguments.py"
)

# No arc.toml content beyond a project-root marker. Arc is told nothing: what
# it narrows to, it learned by watching.
printf '# zero configuration: Arc is told nothing about this project\n' > "$DIR/arc.toml"

JSON_OUT=$here/results/hit-rate-click.json \
ARC_HOME=$BENCH/home-hr-click \
  bash "$here/hit-rate.sh" "$DIR" "$N" "${tasks[@]}"

rm -f "$DIR/arc.toml"
