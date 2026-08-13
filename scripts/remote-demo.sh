#!/usr/bin/env bash
# Two machines, one cache.
#
# Simulates two developers by giving each their own ARC_HOME and their own
# checkout, pointed at one reference server. Also demonstrates what happens when
# the server lies, loses an object, or disappears.
set -euo pipefail

cd "$(dirname "$0")/.."
cargo build --release -p arc-cli -p arc-cache >/dev/null
TARGET=${CARGO_TARGET_DIR:-$PWD/target}
ARC="$TARGET/release/arc"
SERVER="$TARGET/release/arc-cache"

WORK=$(mktemp -d)
trap 'kill %1 2>/dev/null || true; rm -rf "$WORK"' EXIT

say() { printf '\n\033[1m== %s\033[0m\n' "$1"; }

say "A: the server"
"$SERVER" serve --listen 127.0.0.1:7899 --data "$WORK/cache" &
sleep 1

mkproject() {
  mkdir -p "$1/src"
  cat > "$1/arc.toml" <<TOML
[outputs]
include = ["out/**"]

[remote]
url = "http://127.0.0.1:7899"
namespace = "demo"
TOML
  echo "the source of truth" > "$1/src/input.txt"
}

mkproject "$WORK/checkout-a"
mkproject "$WORK/deep/nested/checkout-b"
export ARC_NO_ANIM=1
BUILD='mkdir -p out && sleep 1 && tr a-z A-Z < src/input.txt > out/built.txt && echo built'

a() { (cd "$WORK/checkout-a" && ARC_HOME="$WORK/home-a" "$ARC" "$@"); }
b() { (cd "$WORK/deep/nested/checkout-b" && ARC_HOME="$WORK/home-b" "$ARC" "$@"); }

a remote status

say "B: first machine executes and publishes"
time a run sh -c "$BUILD"

say "C: second machine, different checkout path, empty cache"
time b run sh -c "$BUILD"
echo "restored: $(cat "$WORK/deep/nested/checkout-b/out/built.txt")"

say "D: promotion — the server is stopped, and it still hits"
kill %1
sleep 1
b run sh -c "$BUILD"

say "E: server down entirely, on work it has never seen"
echo "changed" > "$WORK/deep/nested/checkout-b/src/input.txt"
b run sh -c "$BUILD"
echo "exit: $?"

"$SERVER" serve --listen 127.0.0.1:7899 --data "$WORK/cache" &
sleep 1

say "F: a corrupted server object is refused"
OBJ=$(find "$WORK/cache/objects" -type f | head -1)
printf 'tampered' > "$OBJ"
rm -rf "$WORK/home-c"
(cd "$WORK/checkout-a" && ARC_HOME="$WORK/home-c" "$ARC" run --explain sh -c "$BUILD" 2>&1 | tail -20)

say "G: a missing server object falls back"
find "$WORK/cache/objects" -type f -delete
rm -rf "$WORK/home-d"
(cd "$WORK/checkout-a" && ARC_HOME="$WORK/home-d" "$ARC" run sh -c "$BUILD")

say "H: what the server holds"
"$SERVER" stats --data "$WORK/cache"
