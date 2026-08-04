#!/usr/bin/env bash
# tests/repro_rewrite_refdrift.sh — 2026-08-04 cluster field repro:
# time_based sequential-overwrite storm over PRE-EXISTING striped files
# (every write rides the rewrite-shadow CoW path), then reads + fsck.
#
# Field signature being chased (squeeze-test, daemon d0451b4d):
#   * fio read rows EIO ("did not settle after 4 serialized settle attempts")
#   * invariant_tripwires = 4 x stale_binding_escalations (settle NEVER wins)
#   * fsck: C2Lost (live map binds allocator-untracked offset — the EIO face),
#     C2Leaked (allocated, zero referencers), C3 (2 map refs, count 1)
#   * poison persists on a quiet mount (deterministic dd EIO on named blocks)
#
# Venue: local dev substrate (loop). Scaled: 8 files x 256 MiB, bs=1M,
# qd8 libaio overwrite storm 20 s, sweeper horizon is 30 s so we also
# quiesce 40 s and re-read (the field's randread-face). PASS = zero fio
# EIO on every read leg AND zero fsck findings. Exit nonzero on any.
set -u
SQZ=${SQZ:-target/release/squeezefs}
MNT=${MNT:-/mnt/sqz_refdrift}
META="sqmeta:///dev/nvme1n1,/dev/nvme2n1,/dev/nvme3n1,/dev/nvme4n1"
DATA="sqdata:///dev/nvme5n1,/dev/nvme6n1,/dev/nvme7n1,/dev/nvme8n1"
LOG=${LOG:-/tmp/sqz_refdrift.log}
DIR="$MNT/exa_perf"
NJOBS=${NJOBS:-8}
SIZE=${SIZE:-256m}
RUNTIME=${RUNTIME:-20}
STORM_ROUNDS=${STORM_ROUNDS:-1}
MOUNT_FLAGS=${MOUNT_FLAGS:-}
FAIL=0

say() { echo "[repro] $*"; }
stat_grep() { grep -oE "\"$1\": [0-9]+" "$MNT/.stats" | head -1; }

cleanup() {
  "$SQZ" umount "$MNT" >/dev/null 2>&1 || true
}
trap cleanup EXIT

say "format + mount (fresh volumes, from-zero state)"
"$SQZ" umount "$MNT" >/dev/null 2>&1 || true
"$SQZ" format "$META" "$DATA" --force >/dev/null 2>&1 || { say "FORMAT FAILED"; exit 2; }
mkdir -p "$MNT"
"$SQZ" mount "$META" "$MNT" --daemon --allow-other $MOUNT_FLAGS --log-file "$LOG" >/dev/null 2>&1 || { say "MOUNT FAILED"; exit 2; }
sleep 2
mountpoint -q "$MNT" || { say "NOT A MOUNTPOINT"; exit 2; }
mkdir -p "$DIR"

say "phase 0: prefill $NJOBS x $SIZE (creates the striped files the storm overwrites)"
fio --name=prefill --directory="$DIR" --filename_format='sqzfio.$jobnum.$filenum' \
    --rw=write --bs=1M --size="$SIZE" --numjobs="$NJOBS" --iodepth=8 --ioengine=libaio \
    --direct=1 --fallocate=none --group_reporting --minimal >/dev/null 2>&1 \
    || { say "PREFILL FAILED"; FAIL=1; }

for round in $(seq 1 "$STORM_ROUNDS"); do
  say "phase 1.$round: time_based overwrite storm (${RUNTIME}s — every block is a shadow-epoch rewrite)"
  fio --name=storm --directory="$DIR" --filename_format='sqzfio.$jobnum.$filenum' \
      --rw=write --bs=1M --size="$SIZE" --numjobs="$NJOBS" --iodepth=8 --ioengine=libaio \
      --direct=1 --fallocate=none --time_based --runtime="$RUNTIME" --group_reporting \
      --minimal >/dev/null 2>&1 || { say "STORM FAILED"; FAIL=1; }
  say "  $(stat_grep rewrite_blocks) $(stat_grep rewrite_shadow_swaps) $(stat_grep rewrite_shadow_open_epochs)"
done

say "phase 2: immediate read-behind (the field read_bw face)"
R1=$(fio --name=readback --directory="$DIR" --filename_format='sqzfio.$jobnum.$filenum' \
    --rw=read --bs=1M --size="$SIZE" --numjobs="$NJOBS" --iodepth=8 --ioengine=libaio \
    --direct=1 --time_based --runtime=10 --group_reporting 2>&1)
E1=$(echo "$R1" | grep -c "Input/output error" || true)
say "  read leg 1: $E1 EIO lines; $(stat_grep stale_binding_escalations) $(stat_grep invariant_tripwires)"
[ "$E1" -gt 0 ] && FAIL=1

say "phase 3: quiesce 40s (past the 30s epoch-sweeper horizon), then re-read (the field randread face)"
sleep 40
say "  post-quiesce: $(stat_grep rewrite_shadow_open_epochs) $(stat_grep rewrite_shadow_swaps)"
R2=$(fio --name=rereads --directory="$DIR" --filename_format='sqzfio.$jobnum.$filenum' \
    --rw=randread --bs=1M --size="$SIZE" --numjobs="$NJOBS" --iodepth=8 --ioengine=libaio \
    --direct=1 --group_reporting 2>&1)
E2=$(echo "$R2" | grep -c "Input/output error" || true)
say "  read leg 2: $E2 EIO lines; $(stat_grep stale_binding_escalations) $(stat_grep invariant_tripwires)"
[ "$E2" -gt 0 ] && FAIL=1

say "phase 4: online fsck (the corruption oracle — C2/C3 must be 0)"
FSCK_OUT=$("$SQZ" fsck "$MNT" --online 2>&1)
FSCK_RC=$?
echo "$FSCK_OUT" | tail -15
if [ $FSCK_RC -ne 0 ]; then
  say "FSCK FINDINGS (rc=$FSCK_RC)"
  FAIL=1
fi

say "daemon-log EIO signature:"
grep -c "did not settle" "$LOG" 2>/dev/null || echo 0

if [ $FAIL -ne 0 ]; then say "RESULT: REPRODUCED / FAILED"; else say "RESULT: CLEAN"; fi
exit $FAIL
