#!/usr/bin/env bash
# Establish, empirically, where Arc's dependency model stops being able to
# describe an execution.
#
# Every claim in LIMITATIONS.md marked "measured" comes from a case here.
# Each case gets its own project directory, its own empty cache, and a
# `run.sh` written to disk rather than passed through three levels of shell
# quoting, so the thing that ran is the thing you can read.
#
# Two kinds of case:
#
#   probe   one traced run; reports what Arc concluded about completeness
#   change  learn, settle, mutate something, run again; reports HIT or MISS,
#           which is the only question that matters for a mutation
#
#   bench/limits-probe.sh [name ...]
set -euo pipefail

ARC=${ARC:-/work/arc/target/release/arc}
ROOT=${ROOT:-/tmp/arc-limits}
export ARC_NO_ANIM=1

mk() {
  local name=$1
  DIR=$ROOT/$name
  rm -rf "$DIR" && mkdir -p "$DIR"
  : > "$DIR/arc.toml"
  export ARC_HOME=$ROOT/home-$name
  rm -rf "$ARC_HOME"
}

# probe <name>   -- reads run.sh, and setup.sh if present, from stdin sections
probe() {
  local name=$1
  echo "### $name"
  ( cd "$DIR" && [ -f setup.sh ] && sh setup.sh >/dev/null 2>&1 || true )
  ( cd "$DIR" && "$ARC" run --trace sh run.sh 2>&1 ) \
    | grep -E 'TRACE (COMPLETE|PARTIAL)|not complete' \
    | sed 's/^/    /' || echo "    (no trace line)"
  echo
}

# change <name> <mutation-shell>
change() {
  local name=$1 mutate=$2
  echo "### $name"
  ( cd "$DIR" && [ -f setup.sh ] && sh setup.sh >/dev/null 2>&1 || true )
  # Three runs, not two. The first observes, the second is the first that
  # can narrow -- and narrowing for the first time is itself a miss, because
  # the key now describes a different input set. The third is the baseline.
  ( cd "$DIR" && "$ARC" run sh run.sh >/dev/null 2>&1 ) || true
  ( cd "$DIR" && "$ARC" run sh run.sh >/dev/null 2>&1 ) || true
  local settle
  settle=$( cd "$DIR" && "$ARC" run sh run.sh 2>&1 ) || true
  if ! echo "$settle" | grep -q 'CACHE HIT'; then
    echo "    inconclusive: the unchanged re-run did not hit"
    echo
    return
  fi
  ( cd "$DIR" && eval "$mutate" )
  local out
  out=$( cd "$DIR" && "$ARC" run sh run.sh 2>&1 ) || true
  if echo "$out" | grep -q 'CACHE HIT'; then
    echo "    HIT   -- Arc did not notice the change"
  else
    echo "    MISS  -- Arc noticed the change"
  fi
  echo
}

sel=("$@")
runs() { [ ${#sel[@]} -eq 0 ] && return 0; local s; for s in "${sel[@]}"; do [ "$s" = "$1" ] && return 0; done; return 1; }

echo "arc limits probe · $($ARC --version) · $(uname -srm)"
echo "glibc: $(/lib/x86_64-linux-gnu/libc.so.6 2>/dev/null | head -1 | sed 's/.*version //;s/\.$//' || echo unknown)"
echo

# ---------------------------------------------------------- nondeterminism --

if runs clock; then
  mk clock; echo 'date +%s%N > /dev/null' > "$DIR/run.sh"; probe clock
fi
if runs urandom; then
  mk urandom; echo 'head -c 8 /dev/urandom > /dev/null' > "$DIR/run.sh"; probe urandom
fi
if runs getrandom; then
  # Python seeds its RNG through the getrandom(2) syscall, not through
  # /dev/urandom, so this is the case that shows whether the syscall itself
  # is noticed.
  mk getrandom
  echo 'python3 -c "import random,sys; sys.stdout.write(str(random.random()))" > /dev/null' > "$DIR/run.sh"
  probe getrandom
fi
if runs pid; then
  mk pid; echo 'echo $$ > /dev/null' > "$DIR/run.sh"; probe pid
fi

# ------------------------------------------------------------ volatile fs --

if runs proc; then
  mk proc; echo 'cat /proc/uptime > /dev/null' > "$DIR/run.sh"; probe proc
fi
if runs proc_self; then
  mk proc_self; echo 'cat /proc/self/status > /dev/null' > "$DIR/run.sh"; probe proc_self
fi
if runs sysfs; then
  mk sysfs
  printf 'cat /sys/devices/system/cpu/online > /dev/null\n' > "$DIR/run.sh"
  probe sysfs
fi
if runs cgroup; then
  # On the hashable list: a real dependency Arc fingerprints rather than
  # distrusts, so this must stay complete.
  mk cgroup; echo 'cat /sys/fs/cgroup/cpu.max > /dev/null' > "$DIR/run.sh"; probe cgroup
fi
if runs proc_sys; then
  mk proc_sys
  echo 'cat /proc/sys/vm/overcommit_memory > /dev/null' > "$DIR/run.sh"
  probe proc_sys
fi
if runs proc_mounts; then
  # Ignored, not hashed: it is a symlink to the per-process view.
  mk proc_mounts; echo 'cat /proc/mounts > /dev/null' > "$DIR/run.sh"; probe proc_mounts
fi
if runs proc_filesystems; then
  # Deliberately still volatile. See docs/DECISIONS.md.
  mk proc_filesystems
  echo 'cat /proc/filesystems > /dev/null' > "$DIR/run.sh"
  probe proc_filesystems
fi
if runs devnull; then
  mk devnull; echo 'echo x > /dev/null; cat /dev/null' > "$DIR/run.sh"; probe devnull
fi

# ----------------------------------------------------------------- network --

if runs network_tcp; then
  mk network_tcp
  cat > "$DIR/net.py" <<'PY'
import socket
s = socket.socket()
s.settimeout(0.2)
try:
    s.connect(("127.0.0.1", 9))
except Exception:
    pass
PY
  echo 'python3 net.py' > "$DIR/run.sh"
  probe network_tcp
fi
if runs network_unix; then
  mk network_unix
  cat > "$DIR/un.py" <<'PY'
import socket
s = socket.socket(socket.AF_UNIX)
try:
    s.connect("/tmp/arc-does-not-exist.sock")
except Exception:
    pass
PY
  echo 'python3 un.py' > "$DIR/run.sh"
  probe network_unix
fi
if runs network_unix_live; then
  # A socket that is there answers with something no filesystem fingerprint
  # describes, so this one must downgrade where the absent one did not.
  mk network_unix_live
  cat > "$DIR/setup.sh" <<'SH'
python3 -c "import socket; s=socket.socket(socket.AF_UNIX); s.bind('live.sock')"
SH
  cat > "$DIR/live.py" <<'PY'
import socket
s = socket.socket(socket.AF_UNIX)
try:
    s.connect("live.sock")
except Exception:
    pass
PY
  echo 'python3 live.py' > "$DIR/run.sh"
  probe network_unix_live
fi

# ------------------------------------------------------------- subprocesses --

if runs child; then
  mk child; echo 'one' > "$DIR/in.txt"
  echo 'sh -c "cat in.txt" > /dev/null' > "$DIR/run.sh"; probe child
fi
if runs detached_child; then
  # A grandchild that detaches from the session and outlives its parent.
  mk detached_child; echo 'one' > "$DIR/in.txt"
  cat > "$DIR/run.sh" <<'SH'
setsid sh -c 'sleep 0.4; cat in.txt > /dev/null' < /dev/null > /dev/null 2>&1 &
exit 0
SH
  probe detached_child
fi

# -------------------------------------------------------------------- mmap --

if runs mmap_read; then
  mk mmap_read; echo 'hello' > "$DIR/in.txt"
  cat > "$DIR/m.py" <<'PY'
import mmap
f = open("in.txt", "rb")
m = mmap.mmap(f.fileno(), 0, prot=mmap.PROT_READ)
_ = m[:]
m.close(); f.close()
PY
  echo 'python3 m.py' > "$DIR/run.sh"; probe mmap_read
fi

# --------------------------------------------------------- mutation cases ---

if runs mmap_write; then
  mk mmap_write
  cat > "$DIR/setup.sh" <<'SH'
printf 'aaaa' > shared.bin
SH
  cat > "$DIR/w.py" <<'PY'
import mmap
f = open("shared.bin", "r+b")
m = mmap.mmap(f.fileno(), 4)
m[0:1] = b"z"
m.flush(); m.close(); f.close()
PY
  # The question: shared.bin is only ever written through memory, never
  # through write(2). Is it recorded as an output, or as an input?
  echo 'python3 w.py' > "$DIR/run.sh"
  change mmap_write "printf 'bbbb' > shared.bin"
fi

if runs symlink_target; then
  mk symlink_target
  cat > "$DIR/setup.sh" <<'SH'
mkdir -p d; printf 'one' > d/real.txt; ln -sf d/real.txt link.txt
SH
  echo 'cat link.txt > /dev/null' > "$DIR/run.sh"
  change symlink_target "printf 'two' > d/real.txt"
fi

if runs symlink_retarget; then
  mk symlink_retarget
  cat > "$DIR/setup.sh" <<'SH'
printf 'one' > a.txt; printf 'two' > b.txt; ln -sf a.txt link.txt
SH
  echo 'cat link.txt > /dev/null' > "$DIR/run.sh"
  change symlink_retarget "ln -sf b.txt link.txt"
fi

if runs dangling_symlink; then
  mk dangling_symlink
  cat > "$DIR/setup.sh" <<'SH'
ln -sf missing.txt link.txt
SH
  # The command's OUTPUT depends on whether the link resolves, so a hit here
  # is not a harmless one: Arc would replay "no" after the answer became
  # "yes".
  echo 'if [ -e link.txt ]; then echo yes; else echo no; fi' > "$DIR/run.sh"
  change dangling_symlink "printf 'appeared' > missing.txt"
fi

if runs unix_socket_appears; then
  mk unix_socket_appears
  cat > "$DIR/un.py" <<'PY'
import socket
s = socket.socket(socket.AF_UNIX)
try:
    s.connect("daemon.sock")
    print("connected")
except OSError:
    print("refused")
PY
  # The absence is recorded as a negative dependency, so the socket turning up
  # has to be a miss: that is when the answer changes.
  echo 'python3 un.py > /dev/null' > "$DIR/run.sh"
  change unix_socket_appears "python3 -c \"import socket; s=socket.socket(socket.AF_UNIX); s.bind('daemon.sock')\""
fi

if runs dirlist; then
  mk dirlist
  cat > "$DIR/setup.sh" <<'SH'
mkdir -p plugins; printf 'a' > plugins/a
SH
  echo 'ls plugins > /dev/null' > "$DIR/run.sh"
  change dirlist "printf 'b' > plugins/b"
fi

if runs absent; then
  mk absent
  echo 'if [ -f optional.cfg ]; then echo yes; else echo no; fi > /dev/null' > "$DIR/run.sh"
  change absent "printf 'x' > optional.cfg"
fi

echo done
