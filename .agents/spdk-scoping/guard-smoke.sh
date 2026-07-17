#!/usr/bin/env bash
# spdkscope writer-guard smoke — productized both-stack matrix for the
# single-writer guard on fabric-served namespaces:
#
#   GUARD_ARM=spdk  (default)  SPDK v26.05 TCP target — SPEC-STRICT Register
#   GUARD_ARM=nvmet            kernel nvmet TCP target — lenient IEKEY replace
#
# Legs (each asserted; the script exits nonzero on any FAIL):
#   1. format + mount, writer_guard_mode == flock+pr (enforcement grade)
#   2. double-mount refusal while the first daemon serves
#   3. claim clear refused while live-mounted
#   4. kill -9 -> remount recovery, GUARD_KILL9_LOOPS times (default 1;
#      data integrity md5-checked each cycle). On the spdk arm the remount
#      exercises the PR register ladder (own-stale unregister); on nvmet
#      the plain IEKEY fast path — the daemon log is checked for the
#      expected divergence.
#   5. (GUARD_PTPL=1, spdk arm only) target power cycle while the holder
#      is alive: save_config -> SIGKILL spdk_tgt -> relaunch -> load_config
#      => holder keeps serving (no fail-stop; reservation restored from
#      the ptpl_file); then kill -9 the daemon => remount recovers.
#   6. clean unmount + claim clear no-op check + zero PR residue.
#
# Output: /tmp/spdkscope/results/guard-smoke-<arm>.txt
set -uo pipefail
STATE=/tmp/spdkscope/state
ARM="${GUARD_ARM:-spdk}"
LOOPS="${GUARD_KILL9_LOOPS:-1}"
PTPL="${GUARD_PTPL:-0}"
OUT=/tmp/spdkscope/results/guard-smoke-$ARM.txt
SQZ="${SQZ:-/home/justin/Source/squeezefs/target/release/squeezefs}"
MNT=/tmp/spdkscope/mnt
MNT2=/tmp/spdkscope/mnt2
DLOG=/tmp/spdkscope/daemon-$ARM.log
SPDK_DIR=/var/tmp/spdk-scoping/spdk
RPC="$SPDK_DIR/scripts/rpc.py -s /tmp/spdkscope/spdk.sock"
. "$STATE/devices"   # DEV_GMETA DEV_GDATA DEV_NGMETA DEV_NGDATA

case "$ARM" in
    spdk)  META=$DEV_GMETA;  DATA=$DEV_GDATA ;;
    nvmet) META=$DEV_NGMETA; DATA=$DEV_NGDATA ;;
    *) echo "GUARD_ARM must be spdk|nvmet"; exit 2 ;;
esac

FAILS=0
log()  { echo "[guard-$ARM] $*" | tee -a "$OUT"; }
pass() { log "PASS: $*"; }
fail() { log "FAIL: $*"; FAILS=$((FAILS+1)); }
run()  { echo "\$ $*" >> "$OUT"; "$@" >> "$OUT" 2>&1; local rc=$?; echo "(rc=$rc)" >> "$OUT"; return $rc; }
is_mounted() { awk -v m="$MNT" '$2==m{f=1} END{exit !f}' /proc/mounts; }
daemon_pid() { pgrep -f "squeezefs mount sqmeta://$META" | head -1; }
mount_it()   { RUST_LOG=info run "$SQZ" --log-file "$DLOG" mount "sqmeta://$META" "$MNT" --daemon --allow-other; }
wait_mounted() { local i; for i in $(seq 1 60); do is_mounted && return 0; sleep 0.5; done; return 1; }

: > "$OUT"; : > "$DLOG"
mkdir -p "$MNT" "$MNT2"
log "arm=$ARM meta=$META data=$DATA loops=$LOOPS ptpl=$PTPL $(date -Is)"
log "binary: $SQZ ($(md5sum "$SQZ" | cut -d' ' -f1))"

log "0. scrub: kill stale daemons, unmount, wipe superblocks"
pkill -f "squeezefs mount sqmeta://$META" && sleep 2
umount "$MNT" 2>/dev/null; umount -l "$MNT" 2>/dev/null; sleep 1
run dd if=/dev/zero of="$META" bs=1M count=16 oflag=direct
run dd if=/dev/zero of="$DATA" bs=1M count=16 oflag=direct

log "1. format + mount (cache-less)"
run "$SQZ" format "sqmeta://$META" "sqdata://$DATA" --force || { fail "format"; exit 1; }
mount_it
wait_mounted || { fail "initial mount did not appear"; exit 1; }
sleep 2
MODE=$(jq -r '.writer_guard_mode[0] // .writer_guard_mode // empty' "$MNT/.stats" 2>/dev/null)
[ "$MODE" = "flock+pr" ] && pass "writer_guard_mode=flock+pr (enforcement grade)" \
                         || fail "writer_guard_mode=$MODE (want flock+pr)"
run dd if=/dev/urandom of="$MNT/smoke.bin" bs=1M count=8
run sync
MD5=$(md5sum "$MNT/smoke.bin" | cut -d' ' -f1)
log "smoke.bin md5=$MD5"

log "2. double-mount refusal while first daemon serves"
if run "$SQZ" mount "sqmeta://$META" "$MNT2" --daemon --allow-other; then
    fail "second concurrent mount was NOT refused"
    umount "$MNT2" 2>/dev/null
else
    grep -q "single-writer\|writer lock\|claimed by a live writer" "$OUT" \
        && pass "double-mount refused naming the guard" \
        || pass "double-mount refused (rc!=0)"
fi

log "3. claim clear refused while live-mounted"
if run "$SQZ" claim clear "sqmeta://$META"; then
    fail "claim clear succeeded against a LIVE mount"
else
    pass "claim clear refused while live"
fi

for i in $(seq 1 "$LOOPS"); do
    log "4.$i kill -9 -> remount (cycle $i/$LOOPS)"
    DPID=$(daemon_pid)
    [ -n "$DPID" ] || { fail "no daemon pid at cycle $i"; break; }
    kill -9 "$DPID"; sleep 2
    umount -l "$MNT" 2>/dev/null; sleep 1
    mount_it
    if wait_mounted; then
        sleep 2
        MODE2=$(jq -r '.writer_guard_mode[0] // .writer_guard_mode // empty' "$MNT/.stats" 2>/dev/null)
        MD5B=$(md5sum "$MNT/smoke.bin" 2>/dev/null | cut -d' ' -f1)
        [ "$MODE2" = "flock+pr" ] || fail "cycle $i: remount mode=$MODE2 (want flock+pr)"
        [ "$MD5B" = "$MD5" ] || fail "cycle $i: data integrity ($MD5B != $MD5)"
        [ "$MODE2" = "flock+pr" ] && [ "$MD5B" = "$MD5" ] && pass "cycle $i: kill-9 -> remount recovered (flock+pr, data intact)"
    else
        fail "cycle $i: REMOUNT did not appear after kill -9"
        # capture the refusal verbatim (the daemon parent surfaces the
        # child's bootstrap error on its own stderr)
        run timeout 30 "$SQZ" mount "sqmeta://$META" "$MNT" --daemon --allow-other
        break
    fi
done

LADDER_HITS=$(grep -c "register conflicted with our own stale" "$DLOG" 2>/dev/null || true)
if [ "$ARM" = "spdk" ]; then
    [ "${LADDER_HITS:-0}" -ge 1 ] && pass "register ladder fired on spdk remounts (log hits=$LADDER_HITS)" \
                                  || fail "register ladder never fired on spdk (expected on strict Register)"
else
    [ "${LADDER_HITS:-0}" -eq 0 ] && pass "register ladder did NOT fire on nvmet (lenient fast path, hits=0)" \
                                  || fail "register ladder fired on nvmet (hits=$LADDER_HITS) — behavior divergence"
fi

if [ "$PTPL" = "1" ] && [ "$ARM" = "spdk" ]; then
    log "5. PTPL: target power cycle while holder ALIVE (save_config -> SIGKILL -> relaunch -> load_config)"
    $RPC save_config > /tmp/spdkscope/tgt-config.json 2>>"$OUT" && log "config saved ($(wc -c < /tmp/spdkscope/tgt-config.json) B)"
    SPID=$(cat "$STATE/spdk_tgt.pid")
    kill -9 "$SPID" 2>/dev/null; sleep 1
    log "spdk_tgt killed (pid $SPID)"
    "$SPDK_DIR/build/bin/spdk_tgt" -m 0x1000000 -s 1024 -r /tmp/spdkscope/spdk.sock \
        >> /tmp/spdkscope/spdk_tgt.log 2>&1 &
    NEWPID=$!
    echo "$NEWPID" > "$STATE/spdk_tgt.pid"
    sed -i "s/^spdk_pid=.*/spdk_pid=$NEWPID/" "$STATE/manifest" 2>/dev/null
    for i in $(seq 1 50); do $RPC spdk_get_version >/dev/null 2>&1 && break; sleep 0.2; done
    run $RPC load_config -j /tmp/spdkscope/tgt-config.json
    log "spdk_tgt relaunched pid=$NEWPID + config reloaded; waiting for initiator reattach"
    OK=""
    for i in $(seq 1 60); do
        dd if="$MNT/smoke.bin" of=/dev/null bs=1M count=1 2>/dev/null && { OK=1; break; }
        sleep 2
    done
    sleep 12   # one heartbeat re-check past reattach
    FENCED=$(jq -r '.writer_guard_fenced[0] // .writer_guard_fenced // 0' "$MNT/.stats" 2>/dev/null)
    REACQ=$(jq -r '.writer_guard_pr_reacquires[0] // .writer_guard_pr_reacquires // 0' "$MNT/.stats" 2>/dev/null)
    MD5C=$(md5sum "$MNT/smoke.bin" 2>/dev/null | cut -d' ' -f1)
    if [ -n "$OK" ] && [ "$MD5C" = "$MD5" ] && [ "${FENCED:-1}" = "0" ]; then
        pass "holder survived the target power cycle (PTPL; fenced=0, pr_reacquires=$REACQ, data intact)"
    else
        fail "holder did not survive target power cycle (io=${OK:-no} md5=$MD5C fenced=$FENCED)"
    fi
    log "5b. kill -9 AFTER the power cycle -> remount (ladder against ptpl-restored state)"
    DPID=$(daemon_pid); [ -n "$DPID" ] && kill -9 "$DPID"; sleep 2
    umount -l "$MNT" 2>/dev/null; sleep 1
    mount_it
    if wait_mounted; then
        sleep 2
        MD5D=$(md5sum "$MNT/smoke.bin" 2>/dev/null | cut -d' ' -f1)
        [ "$MD5D" = "$MD5" ] && pass "post-power-cycle kill-9 remount recovered (data intact)" \
                             || fail "post-power-cycle remount data integrity ($MD5D)"
    else
        fail "post-power-cycle kill-9 remount did not appear"
    fi
fi

log "6. clean unmount + claim clear no-op + zero PR residue"
run umount "$MNT"; sleep 1
if run "$SQZ" claim clear "sqmeta://$META"; then
    grep -q "no writer claim present" "$OUT" && pass "claim clear after clean unmount: no-op" \
                                             || pass "claim clear rc=0 after clean unmount"
else
    fail "claim clear errored after clean unmount"
fi
REG=$(nvme resv-report "$META" --eds -o json 2>/dev/null | jq -r .regctl)
[ "${REG:-x}" = "0" ] && pass "zero PR residue after clean unmount (regctl=0)" \
                      || fail "PR residue after clean unmount (regctl=$REG)"

log "verdict: $FAILS failure(s)"
exit $((FAILS > 0 ? 1 : 0))
