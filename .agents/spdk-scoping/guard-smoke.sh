#!/usr/bin/env bash
# spdkscope writer-guard smoke on SPDK-served namespaces: format + mount a
# real squeezefs volume on the SPDK guard namespaces, assert
# writer_guard_mode == "flock+pr" (enforcement grade), kill -9 the daemon,
# verify claim classification + dead-pid reclaim on remount.
# Output: /tmp/spdkscope/results/guard-smoke.txt
set -uo pipefail
STATE=/tmp/spdkscope/state
OUT=/tmp/spdkscope/results/guard-smoke.txt
SQZ=/home/justin/Source/squeezefs/target/release/squeezefs
MNT=/tmp/spdkscope/mnt
. "$STATE/devices"   # DEV_GMETA DEV_GDATA

log()  { echo "[guard] $*" | tee -a "$OUT"; }
run()  { echo "\$ $*" >> "$OUT"; "$@" >> "$OUT" 2>&1; local rc=$?; echo "(rc=$rc)" >> "$OUT"; return $rc; }
is_mounted() { awk -v m="$MNT" '$2==m{f=1} END{exit !f}' /proc/mounts; }
: > "$OUT"
mkdir -p "$MNT"

log "meta=$DEV_GMETA data=$DEV_GDATA $(date -Is)"

log "0a. kill any stale smoke daemon + unmount"
pkill -f "squeezefs mount sqmeta://$DEV_GMETA" && sleep 2
umount "$MNT" 2>/dev/null; umount -l "$MNT" 2>/dev/null; sleep 1

log "0. scrub reservation state + wipe superblocks"
run dd if=/dev/zero of="$DEV_GMETA" bs=1M count=16 oflag=direct
run dd if=/dev/zero of="$DEV_GDATA" bs=1M count=16 oflag=direct

log "1. format (cache-less: no --disk-cache-paths)"
run "$SQZ" format "sqmeta://$DEV_GMETA" "sqdata://$DEV_GDATA" --force || { log "FORMAT FAILED"; exit 1; }

log "2. mount --daemon --allow-other (QUICKSTART root-mount convention)"
run "$SQZ" mount "sqmeta://$DEV_GMETA" "$MNT" --daemon --allow-other
ok=""
for i in $(seq 1 60); do is_mounted && { ok=1; break; }; sleep 0.5; done
[ -n "$ok" ] || { log "MOUNT DID NOT APPEAR"; grep " $MNT " /proc/mounts >> "$OUT"; exit 1; }
sleep 2

log "3. writer_guard_mode from .stats (expect flock+pr)"
MODE=$(jq -r '.writer_guard_mode // empty' "$MNT/.stats" 2>/dev/null)
log "writer_guard_mode=$MODE"
jq '{writer_guard_mode, writer_guard_fenced, writer_guard_pr_reacquires, meta_volume_atomicity, meta_volume_atomicity_physical}' "$MNT/.stats" >> "$OUT" 2>&1
if [ "$MODE" = "flock+pr" ]; then log "VERDICT: enforcement-grade PR guard ACTIVE on SPDK target"; else log "VERDICT: mode=$MODE (NOT flock+pr) — investigate"; fi

log "4. basic IO through the mount"
run dd if=/dev/urandom of="$MNT/smoke.bin" bs=1M count=8
run dd if="$MNT/smoke.bin" of=/dev/null bs=1M
run sync

log "5. kill -9 the daemon (crash sim)"
DPID=$(pgrep -f "squeezefs mount sqmeta://$DEV_GMETA" | head -1)
log "daemon pid=$DPID"
[ -n "$DPID" ] && kill -9 "$DPID"
sleep 2
run umount -l "$MNT"
sleep 1

log "6. claim classification after crash (squeezefs clients)"
run "$SQZ" clients "sqmeta://$DEV_GMETA"

log "7. remount (dead-pid auto-reclaim + PR retake — THE verdict)"
run "$SQZ" mount "sqmeta://$DEV_GMETA" "$MNT" --daemon --allow-other
ok=""
for i in $(seq 1 60); do is_mounted && { ok=1; break; }; sleep 0.5; done
if [ -n "$ok" ]; then
    sleep 2
    MODE2=$(jq -r '.writer_guard_mode // empty' "$MNT/.stats" 2>/dev/null)
    log "remount OK, writer_guard_mode=$MODE2"
    run cat "$MNT/smoke.bin" >/dev/null 2>&1 || true
    [ "$MODE2" = "flock+pr" ] && log "VERDICT: crash -> dead-pid reclaim -> PR retake WORKS UNCHANGED on SPDK" \
                              || log "VERDICT: remount mode=$MODE2 — investigate"
else
    log "VERDICT: REMOUNT FAILED after kill -9 — guard does NOT reclaim on SPDK (blocker-grade finding)"
fi

log "8. clean unmount + claim clear negative check"
run umount "$MNT"
sleep 1
run "$SQZ" claim clear "sqmeta://$DEV_GMETA"
log "guard-smoke done"
