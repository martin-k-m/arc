set -e
ARC=$1
WORK=/tmp/arc-sched-bench
rm -rf $WORK && mkdir -p $WORK/repo/src && cd $WORK/repo
export ARC_HOME=$WORK/home ARC_NO_ANIM=1
cfg=""
for i in $(seq 1 12); do
  printf '[[command]]\nname = "t%02d"\nmatch = "*task-%02d *"\ninputs = ["src/seed.txt"]\n\n' $i $i >> arc.toml
done
echo seed > src/seed.txt
git init -q . && git config user.email b@e.com && git config user.name b && git add -A && git commit -qm i
for i in $(seq 1 12); do
  "$ARC" run sh -c "sleep 0.2; echo task-$i > /dev/null # task-$(printf %02d $i) " >/dev/null 2>&1
  "$ARC" run sh -c "sleep 0.2; echo task-$i > /dev/null # task-$(printf %02d $i) " >/dev/null 2>&1
done
for j in 1 2 4 8; do
  echo "change-$j" > src/seed.txt
  s=$(date +%s%3N)
  "$ARC" affected --run --jobs $j --json >/dev/null 2>&1
  e=$(date +%s%3N)
  echo "  jobs=$j  $((e-s))ms"
done
