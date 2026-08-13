#!/usr/bin/env bash
# One branch, one CI job, and everything Arc does with it.
#
# Builds a small repository with three tasks and a real Git history, then walks
# through the cases that matter: an unrelated change, a relevant one, a fresh
# runner served entirely from a shared cache, a fork pull request, a shallow
# clone, and a failure.
set -euo pipefail

cd "$(dirname "$0")/.."
cargo build --release -p arc-cli -p arc-cache >/dev/null
TARGET=${CARGO_TARGET_DIR:-$PWD/target}
ARC="$TARGET/release/arc"
SERVER="$TARGET/release/arc-cache"

WORK=$(mktemp -d)
trap 'kill %1 2>/dev/null || true; rm -rf "$WORK"' EXIT
export ARC_NO_ANIM=1

say() { printf '\n\033[1m== %s\033[0m\n' "$1"; }

mkrepo() {
  local dir=$1
  mkdir -p "$dir/src" "$dir/docs"
  cat > "$dir/arc.toml" <<TOML
[[command]]
name = "gen"
command = "sh"
args = ["-c", "mkdir -p generated && sleep 0.4 && tr a-z A-Z < src/schema.txt > generated/client.txt && echo generated"]
inputs = ["src/schema.txt"]
outputs = ["generated/**"]

[[command]]
name = "test-api"
command = "sh"
args = ["-c", "sleep 0.4 && cat generated/client.txt > /dev/null && echo api ok"]
inputs = ["generated/client.txt"]
after = ["gen"]

[[command]]
name = "test-web"
command = "sh"
args = ["-c", "sleep 0.4 && cat src/web.txt > /dev/null && echo web ok"]
inputs = ["src/web.txt"]

[ci]
tasks = ["gen", "test-api", "test-web"]

[remote]
url = "http://127.0.0.1:7901"
namespace = "demo"
TOML
  printf 'generated/\n' > "$dir/.gitignore"
  echo "the schema" > "$dir/src/schema.txt"
  echo "the web app" > "$dir/src/web.txt"
  echo "documentation" > "$dir/docs/readme.md"
  git -C "$dir" init -q -b main
  git -C "$dir" config user.email demo@example.com
  git -C "$dir" config user.name demo
  git -C "$dir" add -A
  git -C "$dir" commit -qm "initial"
}

say "A: a shared cache server"
"$SERVER" serve --listen 127.0.0.1:7901 --data "$WORK/cache" &
sleep 1

mkrepo "$WORK/repo"
cd "$WORK/repo"
arc() { ARC_HOME="$WORK/home-a" "$ARC" "$@"; }
BASE=$(git rev-parse HEAD)

say "B: the first CI run knows nothing, so it runs everything"
time arc ci --base "$BASE" -j 1

say "C: a documentation-only change proves every task unnecessary"
echo "more documentation" > docs/readme.md
git add -A && git commit -qm docs
time arc ci --base "$BASE~0" --head HEAD

say "D: a source change selects the task it reaches, and what follows it"
DOCS=$(git rev-parse HEAD)
echo "the schema, revised" > src/schema.txt
git add -A && git commit -qm schema
arc ci --base "$DOCS" --head HEAD --dry-run --explain

say "E: and running it reuses what did not change"
arc ci --base "$DOCS" --head HEAD

say "F: a fresh runner, a different checkout path, an empty cache"
mkrepo "$WORK/deep/nested/runner-b" >/dev/null
cd "$WORK/deep/nested/runner-b"
time ARC_HOME="$WORK/home-b" "$ARC" ci --base HEAD
echo "-- knowledge and results both came from the server:"
"$SERVER" stats --data "$WORK/cache"

say "G: a fork pull request runs, and publishes nothing"
cd "$WORK/repo"
cat > "$WORK/event.json" <<JSON
{"pull_request":{"number":42,
  "base":{"sha":"$BASE"},
  "head":{"sha":"$(git rev-parse HEAD)","repo":{"full_name":"stranger/fork","fork":true}}}}
JSON
GITHUB_ACTIONS=true GITHUB_EVENT_NAME=pull_request \
  GITHUB_EVENT_PATH="$WORK/event.json" GITHUB_REPOSITORY=acme/widgets \
  GITHUB_STEP_SUMMARY="$WORK/summary.md" GITHUB_OUTPUT="$WORK/output.txt" \
  ARC_HOME="$WORK/home-c" "$ARC" ci -j 2 || true

say "H: the GitHub job summary it wrote"
cat "$WORK/summary.md"
echo "-- step outputs:"
cat "$WORK/output.txt"

say "I: a shallow clone cannot prove anything, so it runs everything"
git clone -q --depth 1 "file://$WORK/repo" "$WORK/shallow" 2>/dev/null
cd "$WORK/shallow"
ARC_HOME="$WORK/home-d" "$ARC" ci --base "$BASE" --dry-run

say "J: a failing task blocks what depends on it, and fails the job"
cd "$WORK/repo"
sed -i 's|tr a-z A-Z < src/schema.txt > generated/client.txt|exit 7|' arc.toml
git add -A && git commit -qm break
ARC_HOME="$WORK/home-e" "$ARC" ci --base "$BASE" -j 1 || echo "exit: $?"
