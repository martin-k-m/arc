#!/usr/bin/env bash
# What an Arc execution environment actually buys you.
#
# A real Rust toolchain is captured, addressed by content, published to a shared
# cache, and used by a worker whose PATH contains no Rust at all. Everything
# here is real binaries over real HTTP; nothing is simulated.
#
# The workload is `rustc --emit=metadata`, which is real compilation and needs
# no linker. Linking would need a C toolchain, and Arc does not capture one —
# see docs/environments.md on what an environment leaves to the host.
set -uo pipefail

cd "$(dirname "$0")/.."
cargo build --release -p arc-cli -p arc-cache -p arc-worker >/dev/null || exit 1
TARGET=${CARGO_TARGET_DIR:-$PWD/target}
ARC="$TARGET/release/arc"
CACHE_BIN="$TARGET/release/arc-cache"
WORKER_BIN="$TARGET/release/arc-worker"

WORK=$(mktemp -d)
CACHE_PID=""; WORKER_PID=""
cleanup() {
  [ -n "$CACHE_PID" ] && kill "$CACHE_PID" 2>/dev/null
  [ -n "$WORKER_PID" ] && kill "$WORKER_PID" 2>/dev/null
  chmod -R u+w "$WORK" 2>/dev/null
  rm -rf "$WORK"
}
trap cleanup EXIT
export ARC_NO_ANIM=1

say() { printf '\n\033[1m== %s\033[0m\n' "$1"; }
note() { printf '  %s\n' "$1"; }
# One JSON object per line, each key once: no parser needed and none assumed.
jstr() { grep -o "\"$1\":\"[^\"]*\"" "$2" | head -1 | cut -d'"' -f4; }
jnum() { grep -o "\"$1\":[0-9]*" "$2" | head -1 | cut -d: -f2; }
envs_on() { ls "$1/environments" 2>/dev/null | grep -c '^[0-9a-f]\{64\}$'; }

BUILD=(rustc --edition 2021 --crate-type lib --emit=metadata -o app.rmeta src/main.rs)
build_in() { ( cd "$WORK/repo" && ARC_HOME="$1" "$ARC" run --json "${BUILD[@]}" ); }

# The *sysroot*, not whatever directory `rustc` happens to sit in: on a rustup
# install that is a shim, and capturing the shim would capture no compiler at
# all. This is the trap docs/environments.md warns about.
TOOLCHAIN_ROOT=$(rustc --print sysroot)
note "capturing the toolchain at $TOOLCHAIN_ROOT"

"$CACHE_BIN" serve --listen 127.0.0.1:7930 --data "$WORK/cache" >/dev/null 2>&1 &
CACHE_PID=$!
sleep 1

# ---------------------------------------------------------------------------
say "A: a project that declares an environment"

mkdir -p "$WORK/repo/src"
cat > "$WORK/repo/arc.toml" <<TOML
[outputs]
include = ["*.rmeta"]

[remote]
url = "http://127.0.0.1:7930"
namespace = "env-demo"

[remote.execution]
enabled = false
url = "http://127.0.0.1:7931"

[environment.rust]
tools = []

[[environment.rust.tree]]
from = "$TOOLCHAIN_ROOT"
to = "rust"
exclude = ["**/share/doc/**", "**/lib/rustlib/src/**", "**/etc/**"]

[[command]]
name = "build"
match = "rustc*"
environment = "rust"
TOML
printf 'pub fn hello() -> &%sstatic str { "built by the environment" }\n' "'" \
  > "$WORK/repo/src/main.rs"
sed -n '13,22p' "$WORK/repo/arc.toml"

# ---------------------------------------------------------------------------
say "B: capture"
( cd "$WORK/repo" && ARC_HOME="$WORK/home-a" "$ARC" env capture rust --publish --json ) \
  > "$WORK/capture.json"
ENV_ID=$(jstr id "$WORK/capture.json")
note "id            $ENV_ID"
note "files         $(jnum files "$WORK/capture.json")"
note "bytes         $(jnum bytes "$WORK/capture.json")"
note "completeness  $(jstr completeness "$WORK/capture.json")"
note "capture ms    $(jnum capture_ms "$WORK/capture.json")"
note "lock          $(grep rust "$WORK/repo/arc-env.lock")"

say "C: what it requires of a host"
( cd "$WORK/repo" && ARC_HOME="$WORK/home-a" "$ARC" env inspect rust ) | \
  grep -E "libc|loader|system libraries|completeness|PATH" | head -8

# ---------------------------------------------------------------------------
say "D: run it locally, inside the environment"
build_in "$WORK/home-a" 2>"$WORK/d.json" >/dev/null
note "exit          $(jnum exit_code "$WORK/d.json")"
note "environment   $(jstr id "$WORK/d.json" | cut -c1-12)"
note "hermeticity   $(grep -o '"hermeticity":"[a-z-]*"' "$WORK/d.json" | head -1 | cut -d'"' -f4)"
note "trace         $(grep -o '"completeness":"[a-z]*"' "$WORK/d.json" | tail -1 | cut -d'"' -f4)"
note "downgrades    $(grep -o '"downgrades":\[[^]]*\]' "$WORK/d.json" | head -1 | cut -c14-120)"
note "produced      $(stat -c%s "$WORK/repo/app.rmeta" 2>/dev/null || echo missing) bytes"

say "E: which rustc ran"
note "environment   $(sha256sum "$WORK/home-a/environments/$ENV_ID/rust/bin/rustc" 2>/dev/null | cut -c1-12)"
note "host          $(sha256sum "$(command -v rustc)" | cut -c1-12)"
note "(the same bytes here, because this is the machine it was captured from —"
note " the point is that the worker below has neither)"

# ---------------------------------------------------------------------------
say "F: a worker with no Rust on its PATH"

mkdir -p "$WORK/emptybin"
for t in sh cat ls; do ln -sf "$(command -v $t)" "$WORK/emptybin/$t" 2>/dev/null; done
env -i PATH="$WORK/emptybin" HOME="$WORK/workerhome" \
  "$WORKER_BIN" serve --listen 127.0.0.1:7931 --data "$WORK/worker" \
  --cache-url http://127.0.0.1:7930 --max-jobs 4 >/dev/null 2>&1 &
WORKER_PID=$!
sleep 1
note "worker PATH   $WORK/emptybin"
note "rust there?   $(ls "$WORK/emptybin" | grep -cE 'rustc|cargo') entries"

sed -i 's/^enabled = false/enabled = true/' "$WORK/repo/arc.toml"
printf '// touched\n' >> "$WORK/repo/src/main.rs"

build_in "$WORK/home-b" 2>"$WORK/f.json" >/dev/null
note "exit          $(jnum exit_code "$WORK/f.json")"
note "ran           $(grep -o '"execution":{[^}]*}' "$WORK/f.json" | grep -o '"source":"[a-z]*"' | cut -d'"' -f4)"
note "worker env    $(envs_on "$WORK/worker") materialised"
note "produced      $(stat -c%s "$WORK/repo/app.rmeta" 2>/dev/null || echo missing) bytes"

say "G: a second remote job reuses the worker's copy"
printf '// again\n' >> "$WORK/repo/src/main.rs"
build_in "$WORK/home-c" 2>"$WORK/g.json" >/dev/null
note "ran           $(grep -o '"execution":{[^}]*}' "$WORK/g.json" | grep -o '"source":"[a-z]*"' | cut -d'"' -f4)"
note "worker env    $(envs_on "$WORK/worker") materialised, still"

say "H: another machine gets a plain cache hit"
mkdir -p "$WORK/other"
cp -r "$WORK/repo/src" "$WORK/other/"
cp "$WORK/repo/arc.toml" "$WORK/repo/arc-env.lock" "$WORK/other/"
( cd "$WORK/other" && ARC_HOME="$WORK/home-other" "$ARC" run --json "${BUILD[@]}" ) \
  2>"$WORK/h2.json" >/dev/null
note "result        $(jstr cache_status "$WORK/h2.json")"
note "source        $(grep -o '"cache":{[^}]*}' "$WORK/h2.json" | grep -o '"source":"[a-z]*"' | cut -d'"' -f4)"

# ---------------------------------------------------------------------------
say "I: a corrupted environment object never executes"
for f in $(find "$WORK/cache/objects" -type f); do
  if head -c 4 "$f" | grep -q ELF && [ "$(stat -c%s "$f")" -gt 1000000 ]; then
    printf 'PWNED' > "$f"
    note "corrupted     $(basename "$f" | cut -c1-12) in the shared cache"
    break
  fi
done
kill "$WORKER_PID" 2>/dev/null; wait "$WORKER_PID" 2>/dev/null
chmod -R u+w "$WORK/worker" 2>/dev/null; rm -rf "$WORK/worker"
env -i PATH="$WORK/emptybin" HOME="$WORK/workerhome" \
  "$WORKER_BIN" serve --listen 127.0.0.1:7931 --data "$WORK/worker" \
  --cache-url http://127.0.0.1:7930 --max-jobs 4 >/dev/null 2>&1 &
WORKER_PID=$!
sleep 1
printf '// third\n' >> "$WORK/repo/src/main.rs"
build_in "$WORK/home-d" 2>"$WORK/i.json" >/dev/null
if grep -q "does not match its digest" "$WORK/i.json"; then
  note "refused       the worker would not execute an unverified toolchain"
else
  note "reason        $(grep -o '"remote_execution":"[^"]*"' "$WORK/i.json" | head -1 | cut -d'"' -f4)"
fi
note "worker env    $(envs_on "$WORK/worker") materialised"

# ---------------------------------------------------------------------------
say "J: changing the toolchain changes the id"
BEFORE=$ENV_ID
cp -r "$TOOLCHAIN_ROOT" "$WORK/toolchain-copy"
chmod -R u+w "$WORK/toolchain-copy"
printf '\n' >> "$WORK/toolchain-copy/bin/rustc"
sed -i "s|^from = .*|from = \"$WORK/toolchain-copy\"|" "$WORK/repo/arc.toml"
( cd "$WORK/repo" && ARC_HOME="$WORK/home-a" "$ARC" env capture rust --json ) > "$WORK/j.json"
note "before        $(echo "$BEFORE" | cut -c1-24)"
note "after         $(jstr id "$WORK/j.json" | cut -c1-24)"
note "(one byte of one file; every result built under the old id stays cached"
note " under it, and none of them can answer for the new one)"

say "K: what everyone is holding"
note "cache objects $(find "$WORK/cache/objects" -type f 2>/dev/null | wc -l)"
note "client env    $(envs_on "$WORK/home-a")"
note "worker env    $(envs_on "$WORK/worker")"

echo
