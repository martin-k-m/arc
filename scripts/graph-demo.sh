#!/usr/bin/env bash
# Walk through the v0.4 task graph end to end.
#
#   cargo build --release && ARC=target/release/arc scripts/graph-demo.sh
set -uo pipefail

ARC=${ARC:-$(pwd)/target/release/arc}
WORK=${WORK:-/tmp/arc-graph-demo}
export ARC_HOME=$WORK/home
export ARC_NO_ANIM=${ARC_NO_ANIM:-1}

rm -rf "$WORK" && mkdir -p "$WORK/repo/schema" "$WORK/repo/src" "$WORK/repo/generated"
cd "$WORK/repo"

cat > arc.toml <<'EOF'
[[command]]
name = "generate-schema"
match = "*generate-schema*"

[[command]]
name = "generate-client"
match = "*generate-client*"

[[command]]
name = "test-api"
match = "*test-api*"

[[command]]
name = "test-web"
match = "*test-web*"
EOF

echo "openapi: 3.0" > schema/api.yaml
echo "fn api() {}" > src/api.rs
echo "fn web() {}" > src/web.rs

mk() { printf '#!/bin/sh\n%s\n' "$2" > "$1"; chmod +x "$1"; }
mk generate-schema.sh 'while IFS= read -r l; do echo "schema:$l"; done < schema/api.yaml > generated/schema.json'
mk generate-client.sh 'while IFS= read -r l; do echo "client:$l"; done < generated/schema.json > generated/client.ts'
mk test-api.sh       'while IFS= read -r l; do echo "api-test:$l"; done < generated/client.ts; while IFS= read -r l; do :; done < src/api.rs'
mk test-web.sh       'while IFS= read -r l; do :; done < src/web.rs; echo web-ok'

step() { printf '\n\033[1m-- %s\033[0m\n$ %s\n' "$1" "$2"; }

step "A · learn three tasks" "arc run ./generate-schema.sh (etc)"
for t in generate-schema generate-client test-api test-web; do
  "$ARC" run sh -c "./$t.sh" >/dev/null 2>&1
  "$ARC" run sh -c "./$t.sh" >/dev/null 2>&1
done
"$ARC" graph

step "B · affected" "edit schema/api.yaml; arc affected"
git init -q . 2>/dev/null
git config user.email demo@example.com && git config user.name demo
git add -A >/dev/null && git commit -qm init
echo "openapi: 3.1" > schema/api.yaml
"$ARC" affected

step "C · why" "arc affected --explain"
"$ARC" affected --explain

step "D · plan" "arc affected --run --dry-run"
"$ARC" affected --run --dry-run

step "E · selective run" "arc affected --run"
"$ARC" affected --run
echo "exit=$?"

step "F · nothing left to do" "arc affected --run"
"$ARC" affected --run
echo "exit=$?"

step "G · parallelism" "diamond: b and c concurrently"
mk diamond-a.sh 'echo a > generated/a.txt'
mk diamond-b.sh 'while IFS= read -r l; do :; done < generated/a.txt; sleep 1; echo b > generated/b.txt'
mk diamond-c.sh 'while IFS= read -r l; do :; done < generated/a.txt; sleep 1; echo c > generated/c.txt'
mk diamond-d.sh 'while IFS= read -r l; do :; done < generated/b.txt; while IFS= read -r l; do :; done < generated/c.txt; echo d'
for t in diamond-a diamond-b diamond-c diamond-d; do "$ARC" run sh -c "./$t.sh" >/dev/null 2>&1; done
git add -A >/dev/null && git commit -qm diamond
echo "a2" > generated/a.txt.seed && echo 'echo a2 > generated/a.txt' >> diamond-a.sh
echo "  jobs=1:"; time "$ARC" affected --run --jobs 1 >/dev/null 2>&1
echo 'echo a3 > generated/a.txt' >> diamond-a.sh
echo "  jobs=4:"; time "$ARC" affected --run --jobs 4 >/dev/null 2>&1

step "H · failure blocks dependents" "upstream fails"
mk failing.sh 'echo boom >&2; exit 3'
mk after-failing.sh 'while IFS= read -r l; do :; done < generated/fail.txt; echo never'
mk fail-producer.sh 'echo x > generated/fail.txt; exit 3'
"$ARC" run sh -c ./fail-producer.sh >/dev/null 2>&1
"$ARC" run sh -c ./after-failing.sh >/dev/null 2>&1
git add -A >/dev/null && git commit -qm fail 2>/dev/null
echo 'echo more' >> fail-producer.sh
"$ARC" affected --run
echo "exit=$?"

step "I · ambiguity and cycles" "arc doctor"
"$ARC" doctor 2>&1 | sed -n '/task graph/,/capabilities/p'
