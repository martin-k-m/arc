#!/usr/bin/env bash
# Walk through what Arc v0.3 does, end to end, on a real Linux tracer.
#
#   scripts/linux-check.sh build --release   # or cargo build --release
#   ARC=target/release/arc scripts/demo.sh
#
# Every step prints the command it is running, so the output can be read as a
# transcript rather than taken on trust.
set -uo pipefail

ARC=${ARC:-$(pwd)/target/release/arc}
WORK=${WORK:-/tmp/arc-demo}
export ARC_HOME=$WORK/home
export ARC_NO_ANIM=${ARC_NO_ANIM:-1}

rm -rf "$WORK" && mkdir -p "$WORK/repo/plugins" "$WORK/repo/docs"
cd "$WORK/repo"
echo "one" > input.txt
echo "docs" > docs/design.md
echo "a" > plugins/a.plugin
# Shell built-ins only. Debian's coreutils probe SELinux through `/sys` and
# `/proc`, which correctly downgrades a trace to partial — worth demonstrating
# (step I), but not while demonstrating everything else.
cat > build.sh <<'EOF'
#!/bin/sh
while IFS= read -r line; do echo "$line"; done < input.txt
if [ -f optional.cfg ]; then echo "with-optional"; fi
for p in plugins/*; do echo "plugin $p"; done
EOF
chmod +x build.sh

step() { printf '\n\033[1m── %s\033[0m\n$ %s\n' "$1" "$2"; }

step "A · complete trace" "arc run --trace ./build.sh"
"$ARC" run --trace sh -c ./build.sh

step "B · unrelated change is irrelevant" "edit docs/design.md; arc run"
echo "rewritten" > docs/design.md
"$ARC" run sh -c ./build.sh

step "C · a real dependency changed" "edit input.txt; arc run --explain"
echo "two" > input.txt
"$ARC" run --explain sh -c ./build.sh

step "D · negative dependency" "create optional.cfg; arc run"
echo x > optional.cfg
"$ARC" run sh -c ./build.sh

step "E · directory dependency" "add plugins/new.plugin; arc run"
echo b > plugins/new.plugin
"$ARC" run sh -c ./build.sh

step "F · child process dependency" "child reads its own file"
echo "child" > child.txt
printf '#!/bin/sh\ncat child.txt\n' > child.sh && chmod +x child.sh
printf '#!/bin/sh\n./child.sh\n' > parent.sh && chmod +x parent.sh
"$ARC" run sh -c ./parent.sh >/dev/null
"$ARC" run sh -c ./parent.sh >/dev/null
echo "changed" > child.txt
"$ARC" run sh -c ./parent.sh

step "G · what Arc learned" "arc graph"
"$ARC" graph

step "H · affected" "git status drives arc affected"
git init -q . 2>/dev/null
git config user.email demo@example.com && git config user.name demo
git add -A && git commit -qm init
echo "irrelevant edit" >> docs/design.md
"$ARC" affected

step "I · volatile state downgrades a trace" "coreutils probe SELinux via /sys"
"$ARC" run --trace sh -c 'ls plugins > /dev/null'

step "J · tracer pinned off" "arc run --trace-backend snapshot"
"$ARC" run --trace --trace-backend snapshot sh -c ./build.sh

step "K · network access" "a connection makes the trace partial"
if [ -x /bin/bash ]; then
  "$ARC" run --trace bash -c 'exec 3<>/dev/tcp/127.0.0.1/9 || true'
else
  echo "(bash not available; skipped)"
fi

step "L · diagnostics" "arc doctor"
"$ARC" doctor
