#!/usr/bin/env bash
# A by-hand probe of a TRANSFORMED volume's read amplification (decision 0003
# §K row 8): format a set with FORMAT_OPTS on sparse files, mount, lay down
# FILES × FILE_MB with O_DIRECT 1 MiB writes, run fio randread BS for SECS and
# print the device-read ledger. One daemon, its own scratch, torn down.
# PROBE_SRC=<file> feeds the prepare from that file instead of /dev/urandom
# (a compressible source shows the ratio-ρ bound beside the incompressible one).
#   xform-probe.sh <bin> <scratch> "<format opts>" [FILES] [FILE_MB] [BS] [SECS]
set -euo pipefail
BIN=$1; S=$2; FOPTS=$3; FILES=${4:-4}; FILE_MB=${5:-256}; BS=${6:-4k}; SECS=${7:-20}
mkdir -p "$S/mnt"
truncate -s 2G "$S/meta.img"; truncate -s 8G "$S/data0.img"
KEYARGS=()
if printf '%s' "$FOPTS" | grep -q '@KEY'; then head -c 32 /dev/urandom > "$S/key"; chmod 0600 "$S/key"; FOPTS=${FOPTS//@KEY/$S/key}; KEYARGS=(--encrypt-key "$S/key"); fi
# shellcheck disable=SC2086
"$BIN" format $FOPTS "sqmeta://$S/meta.img" "sqdata://$S/data0.img" > "$S/format.log" 2>&1
SQUEEZEFS_MEM_BUDGET_MB=6144 "$BIN" mount --daemon --log-file "$S/daemon.log" "${KEYARGS[@]}" "sqmeta://$S/meta.img" "$S/mnt" > "$S/mount.log" 2>&1
for i in $(seq 1 100); do grep -q 'ready: ' "$S/daemon.log" && break; sleep 0.2; done
PID=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["mount"]["pid"])' "$S/mnt/.stats")
cleanup() { "$BIN" umount "$S/mnt" > /dev/null 2>&1 || { kill -9 "$PID" 2>/dev/null; umount -l "$S/mnt" 2>/dev/null; }; }
trap cleanup EXIT
t0=$(date +%s)
for i in $(seq 0 $((FILES - 1))); do dd if="${PROBE_SRC:-/dev/urandom}" of="$S/mnt/f.$i" bs=1M count="$FILE_MB" iflag=fullblock oflag=direct status=none; done
sync -f "$S/mnt"
echo "prepare: $FILES × $FILE_MB MiB in $(( $(date +%s) - t0 )) s; daemon errors $(grep -c 'error(' "$S/daemon.log" || true)"
cat "$S/mnt/.stats" > "$S/before.json"
DIRECTORY="$S/mnt" RW=randread BS=$BS BLOCKALIGN=$BS DIRECT=1 IOENGINE=libaio SIZE=${FILE_MB}m RUNTIME=$SECS NUMJOBS=$FILES IODEPTH=16 PREFIX=f \
  fio --output-format=json --output="$S/fio.json" "$(dirname "$0")/jobs/rand-bs.fio" > /dev/null
cat "$S/mnt/.stats" > "$S/after.json"
python3 - "$S/before.json" "$S/after.json" "$S/fio.json" <<'PYEOF'
import json, sys, os
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(sys.argv[0])) if False else os.environ.get("BRK_HERE", "."), ""))
sys.path.insert(0, os.environ["BRK_HERE"])
import brk
b = brk.terms(json.load(open(sys.argv[1]))); a = brk.terms(json.load(open(sys.argv[2])))
d = brk.delta(a, b)
m = brk.fio_metrics(sys.argv[3])
user = m["user_read_bytes"]
dev = sum(d[k] for k in brk.DEVICE_READ_ARMS)
print("user %d B, %d ops, %.0f IOPS; device %d B; device ÷ user %.3f" % (user, m["ops"], m["read"]["iops"], dev, dev / user if user else 0))
print("arms:", {k: d[k] for k in brk.DEVICE_READ_ARMS if d[k]})
print("beside:", {k: d[k] for k in brk.DEVICE_READ_BESIDE if d[k]})
PYEOF
