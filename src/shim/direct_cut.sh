#!/usr/bin/env bash
# direct_cut.sh <squeezefs-binary> <libsqueezefs_il.so> <direct_writer-binary> <workdir> — the direct
# WRITE arm's power-cut row (decision 0003 §AY): the data volume under dm-log-writes (every write and
# FLUSH logged in order), the set mounted `-o interception=direct`, a root writer under LD_PRELOAD
# pwrites 4 KiB pages straight to the device and fsyncs every page (the shim's fsync: the note acked,
# `fdatasync` on the device = a FLUSH in the log), kill -9 the daemon mid-run (the cut), mark, replay
# the log onto a FRESH device up to the LAST FLUSH before the mark, remount on the replay, and every
# page the writer reported `synced` must read back with a sequence ≥ its own. Prints PASS / FAIL /
# SKIP (no dm-log-writes, no replay-log, no loop devices). Root. Cleans every loop, target and mount.
set -uo pipefail
BIN=$1 SO=$2 WRITER=$3 W=$4
REPLAY=${REPLAY:-/build/suites/xfstests-dev/src/log-writes/replay-log}
if ! modprobe dm-log-writes 2>/dev/null; then echo "direct_cut: SKIP (no dm-log-writes)"; exit 0; fi
if [ ! -x "$REPLAY" ]; then echo "direct_cut: SKIP (no replay-log at $REPLAY)"; exit 0; fi
mkdir -p "$W/mnt"
LOG=$W/daemon.log
truncate -s 256M "$W/meta.img"; truncate -s 1G "$W/data.img"; truncate -s 2G "$W/log.img"; truncate -s 1G "$W/replay.img"
LM=$(losetup -f --show "$W/meta.img") || { echo "direct_cut: SKIP (no loop device)"; exit 0; }
LD=$(losetup -f --show "$W/data.img"); LL=$(losetup -f --show "$W/log.img"); LR=$(losetup -f --show "$W/replay.img")
DM=sqzdcut-$$
pid=""
cleanup() {
  set +e
  if grep -q " $W/mnt " /proc/mounts; then "$BIN" umount "$W/mnt" >/dev/null 2>&1 || fusermount3 -uz "$W/mnt" || umount -i -l "$W/mnt"; fi
  [ -n "$pid" ] && kill -9 "$pid" 2>/dev/null
  dmsetup remove --retry "$DM" 2>/dev/null
  for l in "$LM" "$LD" "$LL" "$LR"; do losetup -d "$l" 2>/dev/null; done
  rm -f "$W"/*.img
}
trap cleanup EXIT
SZ=$(blockdev --getsz "$LD")
dmsetup create "$DM" --table "0 $SZ log-writes $LD $LL" || { echo "direct_cut: SKIP (dmsetup create failed)"; exit 0; }
ln -sfn "/dev/mapper/$DM" "$W/ost0"
"$BIN" format --force --single-writer "sqmeta://$LM" "sqdata://$W/ost0" > "$W/format.log" 2>&1 || { cat "$W/format.log"; echo "direct_cut: FAIL (format)"; exit 1; }
SQUEEZEFS_TEST_IPC_ALLOW_DIRTY=1 "$BIN" mount --daemon --supervise --log-file "$LOG" -o default_permissions,allow_other,interception=direct "sqmeta://$LM" "$W/mnt" || { tail -20 "$LOG"; echo "direct_cut: FAIL (mount)"; exit 1; }
pid=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["mount"]["pid"])' "$W/mnt/.stats")
# A resident striped file: 8 MiB laid down through the kernel, synced.
dd if=/dev/zero of="$W/mnt/f" bs=1M count=8 oflag=direct conv=fsync status=none
sync -f "$W/mnt"
# The writer: pages over the 8 MiB, every page fsync'd, under the shim; the cut lands mid-run ~0.5 s in.
LD_PRELOAD=$SO SQUEEZEFS_TEST_IPC_ALLOW_DIRTY=1 "$WRITER" "$W/mnt/f" 200000 1 > "$W/writer.out" 2> "$W/writer.err" &
wpid=$!
sleep 0.5
kill -9 "$pid"; pid=""
wait $wpid; wrc=$?
synced=$(grep -c '^synced ' "$W/writer.out")
acked=$(grep -c '^acked ' "$W/writer.out")
fusermount3 -uz "$W/mnt" 2>/dev/null || umount -i -l "$W/mnt" 2>/dev/null || true
dmsetup message "$DM" 0 mark cut
dmsetup remove --retry "$DM"
mark=$("$REPLAY" --log "$LL" --find --end-mark cut | cut -d@ -f1)
last_flush=""; start=0
while :; do
  nf=$("$REPLAY" --log "$LL" --find --next-flush --start-entry "$start" 2>/dev/null | cut -d@ -f1) || true
  [ -z "$nf" ] && break
  [ "$nf" -gt "$mark" ] && break
  last_flush=$nf; start=$((nf + 1))
done
if [ -z "$last_flush" ]; then echo "direct_cut: FAIL (no flush before the cut; writer rc=$wrc acked=$acked synced=$synced)"; exit 1; fi
"$REPLAY" --log "$LL" --replay "$LR" --limit $((last_flush + 1)) > "$W/replay.log" 2>&1 || { cat "$W/replay.log"; echo "direct_cut: FAIL (replay)"; exit 1; }
ln -sfn "$LR" "$W/ost0"
"$BIN" mount --daemon --supervise --log-file "$LOG" -o default_permissions,allow_other "sqmeta://$LM" "$W/mnt" || { tail -20 "$LOG"; echo "direct_cut: FAIL (remount)"; exit 1; }
pid=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["mount"]["pid"])' "$W/mnt/.stats")
# Every synced page: its sequence (every u64 of the page) ≥ the synced sequence for its offset.
python3 - "$W/writer.out" "$W/mnt/f" <<'PY'
import struct, sys
out, path = sys.argv[1], sys.argv[2]
synced = {}
last_ack = {}
for line in open(out):
    p = line.split()
    if len(p) == 3 and p[0] == 'acked': last_ack[int(p[1])] = int(p[2])
    elif len(p) == 2 and p[0] == 'synced':
        s = int(p[1])
        for off, seq in list(last_ack.items()):
            if seq <= s: synced[off] = max(synced.get(off, 0), seq)
bad = 0
with open(path, 'rb') as f:
    for off, seq in sorted(synced.items()):
        f.seek(off); page = f.read(4096)
        vals = set(struct.unpack('<512Q', page))
        if len(vals) != 1 or min(vals) < seq:
            bad += 1
            if bad <= 3: print("direct_cut: page @%d: want seq >= %d, found %s" % (off, seq, sorted(vals)[:3]))
print("direct_cut: %d synced pages checked, %d bad" % (len(synced), bad))
sys.exit(1 if bad else 0)
PY
vrc=$?
"$BIN" umount "$W/mnt"; pid=""
"$BIN" fsck --offline "sqmeta://$LM" > "$W/fsck.log" 2>&1 && frc=0 || frc=$?
echo "direct_cut: writer rc=$wrc acked=$acked synced=$synced mark=$mark last_flush=$last_flush fsck rc=$frc"
[ $vrc = 0 ] && [ $frc = 0 ] && [ "$synced" -gt 0 ] && echo "direct_cut: PASS" || { echo "direct_cut: FAIL"; exit 1; }
