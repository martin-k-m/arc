#!/usr/bin/env bash
# The full Linux gate, run inside the container by `linux-check.sh`.
#
#   docker run ... rust:1 bash /src/scripts/linux-verify.sh
set -uo pipefail

tar -C /src -cf - --exclude=./target --exclude=./.git . | tar -C /work -xf -
cd /work
rustup component add clippy rustfmt >/dev/null 2>&1

cargo fmt --all -- --check && echo LINUX_FMT_OK || echo LINUX_FMT_FAILED
cargo clippy --workspace --all-targets --all-features -- -D warnings 2>&1 |
  grep -E '^(error|warning)' -A 8 | head -40
echo LINUX_CLIPPY_DONE
git config --global user.email ci@example.com
git config --global user.name ci
git config --global init.defaultBranch main
cargo test --workspace -- --test-threads=2 2>&1 |
  grep -E '^test result|^error|FAILED|panicked'
echo LINUX_TESTS_DONE
cargo run --release -q -p arc-core --example ci_bench
echo LINUX_BENCH_DONE
bash scripts/remote-exec-demo.sh 2>&1 | tail -70
echo LINUX_DEMO_DONE
bash scripts/exec-bench.sh 2>&1 | tail -50
echo LINUX_EXEC_BENCH_DONE
