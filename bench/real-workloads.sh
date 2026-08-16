#!/usr/bin/env bash
# Arc on three real projects, not on a synthetic file-reading loop.
#
#   serde_json   cargo test   Rust
#   click        pytest       Python
#   tinycc       make         C
#
# bench/fetch-projects.sh clones them at the pinned commits and prepares
# each one. This script measures, per project:
#
#   direct        the command with no Arc at all
#   arc cold      empty cache: run, trace, learn, store
#   arc miss      an input changed: run, trace, learn, store again
#   arc warm      nothing changed: fingerprint the learned set and replay
#   overhead      each tracing backend against direct, as a percentage
#   cache size    bytes in ARC_HOME once the cache has settled
#
# Every project gets an arc.toml declaring its outputs where the command
# produces artifacts, because a cache hit that restores only stdout would
# not be a replay of the build.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
. "$here/lib.sh"

ARC=${ARC:-/work/arc/target/release/arc}
BENCH=${BENCH:-/work/bench}
export ARC_NO_ANIM=1
export ARC_HOME

DEFAULT_REPS=$REPS

json_out=${JSON_OUT:-$here/results/real-workloads.json}
mkdir -p "$(dirname "$json_out")"

# ---------------------------------------------------------------- projects

# Each project defines: DIR, CMD, TOUCH (an input to perturb), PREP (a hook
# run before every timed iteration, or ":").

project_serde_json() {
  NAME="serde_json"
  DIR=$BENCH/serde_json
  # A miss here means cargo recompiles the crate and every test binary and
  # then runs the doc tests: about ninety seconds on four cores. Seven
  # repetitions of that across four phases is over an hour, so this project
  # runs fewer. The count travels with the numbers rather than being hidden,
  # because a median of three is a weaker claim than a median of seven and
  # the reader should be able to see which one they have.
  REPS=${SERDE_REPS:-3}
  CMD=(cargo test)
  TOUCH=$DIR/src/lib.rs
  COMMENT="//"
  PREP=:
  # cargo test is incremental. The honest baseline is the one a developer
  # actually re-runs: a warm target directory, where the time is compiling
  # whatever changed plus running the tests. A from-scratch target/ is a
  # different measurement and is not what a cache in a dev loop competes
  # with.
  ( cd "$DIR" && cargo test >/dev/null 2>&1 || true )
}

project_click() {
  NAME="click"
  DIR=$BENCH/click
  # -p no:cacheprovider stops pytest writing .pytest_cache into the project,
  # which is state the next run reads back. Arc handles that correctly -- a
  # path written before it is read is an intermediate, not an input -- but it
  # is noise in a measurement of the project, not of pytest's scratch space.
  CMD=("$BENCH/venv-click/bin/python" -m pytest -q -p no:cacheprovider)
  TOUCH=$DIR/src/click/core.py
  COMMENT="#"
  PREP=:
}

project_tinycc() {
  NAME="tinycc"
  DIR=$BENCH/tinycc
  CMD=(make -j1)
  TOUCH=$DIR/tccgen.c
  COMMENT="//"
  # make on an up-to-date tree does nothing, which would measure make
  # deciding there is no work rather than a build. Every timed iteration
  # starts from a cleaned tree, so both direct and Arc face a real build.
  PREP=clean_tinycc
}

clean_tinycc() { ( cd "$BENCH/tinycc" && make clean >/dev/null 2>&1 || true ); }

# ------------------------------------------------------------------ phases

fresh_home() { rm -rf "$ARC_HOME"; }

# The cold run needs an empty cache on every iteration, and the miss run
# needs a genuinely changed input on every iteration. Both are expressed as
# prep hooks so they run outside the timed window.
cold_prep()  { fresh_home; $PREP; }
# The perturbation has to stay valid in the language it lands in, or the
# "miss" phase measures a compile error rather than a rebuild.
miss_prep()  { echo "$COMMENT arc bench $RANDOM" >> "$TOUCH"; $PREP; }
warm_prep()  { $PREP; }

run_project() {
  local setup=$1
  # A project may lower REPS for itself; restore the default first so one
  # project's override cannot leak into the next.
  REPS=$DEFAULT_REPS
  $setup
  cd "$DIR"

  echo "=== $NAME: ${CMD[*]}" >&2
  echo "    commit $(git -C "$DIR" rev-parse HEAD)" >&2

  ARC_HOME=$BENCH/home-$NAME
  fresh_home

  measure "direct" "$PREP" "${CMD[@]}"
  local d_med=$MED d_max=$MAX

  measure "arc cold (trace, learn, store)" cold_prep "$ARC" run "${CMD[@]}"
  local c_med=$MED c_max=$MAX

  # Settle: one full learn, then one more run so the narrowed set is in use.
  fresh_home
  "$ARC" run "${CMD[@]}" >/dev/null 2>&1 || true
  $PREP
  "$ARC" run "${CMD[@]}" >/dev/null 2>&1 || true

  measure "arc warm (cache hit)" warm_prep "$ARC" run "${CMD[@]}"
  local w_med=$MED w_max=$MAX

  local size; size=$(dir_bytes "$ARC_HOME")

  measure "arc miss (input changed)" miss_prep "$ARC" run "${CMD[@]}"
  local m_med=$MED m_max=$MAX

  # Tracing overhead. Each backend runs a forced miss, so the child really
  # executes and the difference from direct is what observation costs.
  local ov_seccomp ov_ptrace ov_snapshot
  ARC_TRACE_BACKEND=fast    measure "  backend seccomp" miss_prep "$ARC" run "${CMD[@]}"; ov_seccomp=$MED
  ARC_TRACE_BACKEND=ptrace  measure "  backend ptrace"  miss_prep "$ARC" run "${CMD[@]}"; ov_ptrace=$MED
  ARC_TRACE_BACKEND=snapshot measure " backend snapshot" miss_prep "$ARC" run "${CMD[@]}"; ov_snapshot=$MED

  # What the trace actually concluded.
  local completeness
  # The trace report is aligned with a variable number of spaces, so match on
  # the label and take the last field rather than counting columns.
  completeness=$("$ARC" run --trace "${CMD[@]}" 2>&1 \
    | awk '/dependency model/ { print $NF; exit }')
  [ -n "$completeness" ] || completeness=unknown

  # One record per line. A multi-line record here produces a stray comma per
  # line when the parts are joined, and the result is not valid JSON -- which
  # is exactly what the first version of this script wrote.
  {
    printf '{"project":"%s","command":"%s","commit":"%s","reps":%s,' \
      "$NAME" "${CMD[*]}" "$(git -C "$DIR" rev-parse HEAD)" "$REPS"
    printf '"direct_ms":{"median":%s,"max":%s},' "$d_med" "$d_max"
    printf '"cold_ms":{"median":%s,"max":%s},' "$c_med" "$c_max"
    printf '"warm_ms":{"median":%s,"max":%s},' "$w_med" "$w_max"
    printf '"miss_ms":{"median":%s,"max":%s},' "$m_med" "$m_max"
    printf '"backend_miss_ms":{"seccomp":%s,"ptrace":%s,"snapshot":%s},' \
      "$ov_seccomp" "$ov_ptrace" "$ov_snapshot"
    printf '"overhead_pct":{"seccomp":"%s","ptrace":"%s","snapshot":"%s"},' \
      "$(pct "$ov_seccomp" "$d_med")" "$(pct "$ov_ptrace" "$d_med")" \
      "$(pct "$ov_snapshot" "$d_med")"
    printf '"speedup_warm":"%s","cache_bytes":%s,"dependency_model":"%s"}\n' \
      "$(ratio "$d_med" "$w_med")" "$size" "$completeness"
  } >> "$json_out.parts"

  # The miss phases appended a line to an input on every iteration. Put the
  # tree back, or the next script to run here measures a modified checkout.
  git -C "$DIR" checkout -q -- "$TOUCH"
  $PREP

  echo >&2
  cd - >/dev/null
}

rm -f "$json_out.parts"
list=${*:-"project_serde_json project_click project_tinycc"}
for p in $list; do run_project "$p"; done

{ echo "["; paste -sd, "$json_out.parts"; echo "]"; } > "$json_out"
rm -f "$json_out.parts"
echo "wrote $json_out" >&2
