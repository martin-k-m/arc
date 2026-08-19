#!/usr/bin/env bash
# Print the execution environment for a benchmark run.
#
# Everything in docs/BENCHMARKS.md's environment block comes from this
# script, so the block can be regenerated rather than remembered.
set -euo pipefail

ARC=${ARC:-/work/arc/target/release/arc}

echo "host"
echo "  uname               $(uname -srm)"
echo "  kernel              $(uname -r)"
if [ -r /proc/cpuinfo ]; then
  echo "  cpu model           $(awk -F': ' '/^model name/ {print $2; exit}' /proc/cpuinfo)"
  echo "  cpus visible        $(nproc)"
fi
if [ -r /proc/meminfo ]; then
  echo "  memtotal            $(awk '/^MemTotal/ {printf "%.2f GiB\n", $2/1048576}' /proc/meminfo)"
fi
echo "  container           $(cat /etc/os-release 2>/dev/null | awk -F= '/^PRETTY_NAME/ {gsub(/"/,"",$2); print $2}')"
echo "  filesystem (/work)  $(stat -f -c %T /work 2>/dev/null || echo unknown)"
echo
echo "toolchains"
command -v rustc >/dev/null && echo "  rustc               $(rustc --version)"
command -v cargo >/dev/null && echo "  cargo               $(cargo --version)"
command -v gcc >/dev/null && echo "  gcc                 $(gcc --version | head -1)"
command -v make >/dev/null && echo "  make                $(make --version | head -1)"
command -v python3 >/dev/null && echo "  python3             $(python3 --version)"
command -v git >/dev/null && echo "  git                 $(git --version)"
command -v strace >/dev/null && echo "  strace              $(strace --version 2>&1 | head -1)"
echo
echo "arc"
echo "  version             $("$ARC" --version)"
# doctor puts a blank line immediately after the section heading, so a
# /^tracing/,/^$/ range prints the heading and nothing else.
#
# This clears the flag at the next section rather than exiting at it. Exiting
# closes the pipe while doctor is still writing, and doctor dies on EPIPE with
# a Rust panic and status 101, which pipefail then makes the status of this
# script.
"$ARC" doctor 2>/dev/null | awk '
  /^tracing/ { on = 1 }
  on && /^[a-z]/ && !/^tracing/ { on = 0 }
  on { print "  " $0 }
'
