#!/usr/bin/env bash
# The hit-rate experiment on click, in two arms.
#
#   zero-config   no arc.toml. Arc learns what it can by tracing, and falls
#                 back to hashing the whole project where it cannot.
#   scoped        bench/configs/click-scoped-arc.toml, which declares what
#                 each task reads.
#
# Same commits, same tasks, same order. The difference between the two
# numbers is what configuration buys on a workload Arc cannot narrow by
# itself.
#
#   bench/hit-rate-click.sh [commits]
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
BENCH=${BENCH:-/work/bench}
N=${1:-50}
PY=$BENCH/venv-click/bin/python
DIR=$BENCH/click

tasks=(
  "basic=$PY -m pytest -q -p no:cacheprovider tests/test_basic.py"
  "options=$PY -m pytest -q -p no:cacheprovider tests/test_options.py"
  "arguments=$PY -m pytest -q -p no:cacheprovider tests/test_arguments.py"
)

echo "########## arm 1: zero-config"
rm -f "$DIR/arc.toml"
# Arc needs a project root marker or it walks up and adopts something else.
printf '# zero configuration: Arc is told nothing\n' > "$DIR/arc.toml"
JSON_OUT=$here/results/hit-rate-click-zeroconfig.json \
ARC_HOME=$BENCH/home-hr-click-zero \
  bash "$here/hit-rate.sh" "$DIR" "$N" "${tasks[@]}"

echo
echo "########## arm 2: scoped inputs"
cp "$here/configs/click-scoped-arc.toml" "$DIR/arc.toml"
JSON_OUT=$here/results/hit-rate-click-scoped.json \
ARC_HOME=$BENCH/home-hr-click-scoped \
  bash "$here/hit-rate.sh" "$DIR" "$N" "${tasks[@]}"

rm -f "$DIR/arc.toml"
