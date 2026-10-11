#!/usr/bin/env bash
# A by-hand probe of one read shape (decision 0003 §K; the evidence note's
# storm numbers): format (FORMAT_OPTS, may be empty) on sparse files, mount
# BIN, lay FILES × FILE_MB down, run fio randread BS for SECS, print the
# daemon's VmRSS/anon before and after with the budget's words and the
# device-read ledger (`device ÷ user` and its arms). One daemon, its own
# scratch, torn down. BRK_HERE = scripts/bracket.
#   read-probe.sh <bin> <scratch> "<format opts>" [FILES] [FILE_MB] [BS] [SECS]
set -euo pipefail
BIN=$1; S=$2; FOPTS=$3; FILES=${4:-4}; FILE_MB=${5:-1024}; BS=${6:-512}; SECS=${7:-30}
mkdir -p "$S/mnt"
truncate -s 2G "$S/meta.img"; truncate -s 12G "$S/data0.img"
# shellcheck disable=SC2086
"$BIN" format $FOPTS "sqmeta://$S/meta.img" "sqdata://$S/data0.img" > "$S/format.log" 2>&1
SQUEEZEFS_MEM_BUDGET_MB=6144 "$BIN" mount --daemon --log-file "$S/daemon.log" "sqmeta://$S/meta.img" "$S/mnt" > "$S/mount.log" 2>&1
for i in $(seq 1 100); do grep -q 'ready: ' "$S/daemon.log" && break; sleep 0.2; done
PID=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["mount"]["pid"])' "$S/mnt/.stats")
cleanup() { "$BIN" umount "$S/mnt" > /dev/null 2>&1 || { kill -9 "$PID" 2>/dev/null; umount -l "$S/mnt" 2>/dev/null; }; }
trap cleanup EXIT
rss() { awk '/VmRSS|RssAnon|RssFile|RssShmem/ {printf "%s %s MiB  ", $1, int($2/1024)}' "/proc/$PID/status"; echo; }
DIRECTORY="$S/mnt" PREFIX=f SIZE=${FILE_MB}m NUMJOBS=$FILES fio --output-format=json --output="$S/prep.json" "$(dirname "$0")/jobs/prepare-seq.fio" > /dev/null
sync -f "$S/mnt"
# An optional warm-up span of another shape first (PROBE_WARM_RW / _BS / _SECS),
# e.g. randwrite 512 B before the measured reads (the read-after-small-write row).
if [ -n "${PROBE_WARM_RW:-}" ]; then
  DIRECTORY="$S/mnt" RW=$PROBE_WARM_RW BS=${PROBE_WARM_BS:-512} BLOCKALIGN=${PROBE_WARM_BS:-512} DIRECT=1 IOENGINE=libaio SIZE=${FILE_MB}m RUNTIME=${PROBE_WARM_SECS:-20} NUMJOBS=$FILES IODEPTH=32 PREFIX=f \
    fio --output-format=json --output="$S/warm.json" "$(dirname "$0")/jobs/rand-bs.fio" > /dev/null
  echo "after warm-up ($PROBE_WARM_RW ${PROBE_WARM_BS:-512}): $(rss)"
fi
echo "after prepare: $(rss)"
cat "$S/mnt/.stats" > "$S/before.json"
DIRECTORY="$S/mnt" RW=randread BS=$BS BLOCKALIGN=$BS DIRECT=1 IOENGINE=libaio SIZE=${FILE_MB}m RUNTIME=$SECS NUMJOBS=$FILES IODEPTH=32 PREFIX=f \
  fio --output-format=json --output="$S/fio.json" "$(dirname "$0")/jobs/rand-bs.fio" > /dev/null &
FIO=$!
for i in $(seq 1 $((SECS / 5))); do sleep 5; echo "t+$((i * 5))s: $(rss)"; done
wait $FIO
cat "$S/mnt/.stats" > "$S/after.json"
echo "after run: $(rss)"
python3 - "$S/before.json" "$S/after.json" "$S/fio.json" <<'PYEOF'
import json, sys, os
sys.path.insert(0, os.environ["BRK_HERE"])
import brk
b = brk.terms(json.load(open(sys.argv[1]))); a_doc = json.load(open(sys.argv[2])); a = brk.terms(a_doc)
d = brk.delta(a, b)
m = brk.fio_metrics(sys.argv[3])
user = m["user_read_bytes"]
dev = sum(d[k] for k in brk.DEVICE_READ_ARMS)
print("user %d B, %d ops, %.0f IOPS; device %d B; device ÷ user %.3f" % (user, m["ops"], m["read"]["iops"], dev, dev / user if user else 0))
print("arms:", {k: d[k] for k in brk.DEVICE_READ_ARMS if d[k]})
paths, _ = brk.flatten(a_doc)
for k in sorted(paths):
    if k.startswith("/mem_budget/") and not "/components/" in k:
        print(k, paths[k])
comps = {k.split("/")[3]: v for k, v in paths.items() if k.startswith("/mem_budget/mem_budget_components/") and k.endswith("/current")}
print("components (MiB):", {k: v >> 20 for k, v in sorted(comps.items(), key=lambda kv: -kv[1]) if v})
PYEOF
