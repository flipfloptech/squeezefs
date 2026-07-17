#!/usr/bin/env bash
# spdkscope PR/PTPL matrix v2 vs the SPDK target guard namespace — probes with
# kernel nvme CLI exactly what the M1 writer guard checks (reservation.rs):
# RESCAP (Identify NS byte 31), Register(+IEKEY,+CPTPL), Acquire WE,
# Report(EDS), Write-Exclusive enforcement + Preempt (cross-host sim via a
# second controller association with distinct hostnqn/hostid), and PTPL across
# a target power cycle (spdk_tgt kill + relaunch + load_config).
#
# Attribution design: native NVMe multipath folds all associations to one
# subsystem into ONE head node, so I/O attribution is only unambiguous with a
# single live association — the protocol therefore serializes: host1 phase,
# host1 disconnect, host2 phase, power cycle, host2 verify, cleanup,
# host1 reconnect (leaves the rig ready for guard-smoke.sh).
set -uo pipefail
STATE=/tmp/spdkscope/state
OUT=/tmp/spdkscope/results/pr-matrix.txt
SPDK_DIR=/var/tmp/spdk-scoping/spdk
RPC="$SPDK_DIR/scripts/rpc.py -s /tmp/spdkscope/spdk.sock"
NQN_GUARD="nqn.2026-07.io.spdkscope:guard-spdk"
KEY1=0xA11CE
KEY2=0xB0B
H2UUID="7b7b7b7b-2222-4222-8222-b2b2b2b2b2b2"
H2NQN="nqn.2014-08.org.nvmexpress:uuid:$H2UUID"
UUID_GMETA="3e3e0001-5c0e-4ee1-8000-000000000001"
UUID_GDATA="3e3e0001-5c0e-4ee1-8000-000000000002"

log()  { echo "[pr] $*" | tee -a "$OUT"; }
step() { echo -e "\n### $*" | tee -a "$OUT"; }
run()  { echo "\$ $*" >> "$OUT"; "$@" >> "$OUT" 2>&1; local rc=$?; echo "(rc=$rc)" >> "$OUT"; return $rc; }

# head node for (nqn, nsid), whoever is connected
finddev() {
    local nqn=$1 nsid=$2 c cname i
    for i in $(seq 1 60); do
        for c in /sys/class/nvme/nvme*; do
            [ -e "$c/subsysnqn" ] || continue
            if [ "$(cat "$c/subsysnqn")" = "$nqn" ]; then
                cname=$(basename "$c")
                [ -b "/dev/${cname}n${nsid}" ] && { echo "/dev/${cname}n${nsid}"; return 0; }
            fi
        done
        sleep 0.5
    done
    return 1
}
guard_ctrls() { # names of controllers on the guard subsystem
    local c
    for c in /sys/class/nvme/nvme*; do
        [ -e "$c/subsysnqn" ] || continue
        [ "$(cat "$c/subsysnqn")" = "$NQN_GUARD" ] && basename "$c"
    done
}

: > "$OUT"
GM_Z=$(grep 'label=guard-meta' "$STATE/manifest" | sed 's/zram=\([0-9]*\).*/\/dev\/zram\1/')
GD_Z=$(grep 'label=guard-data' "$STATE/manifest" | sed 's/zram=\([0-9]*\).*/\/dev\/zram\1/')
log "guard zrams: meta=$GM_Z data=$GD_Z $(date -Is)"

step "0. Rebuild the guard subsystem from scratch (fixed ns UUIDs + ptpl files, fresh PR state)"
for c in $(guard_ctrls); do run nvme disconnect --device="$c"; done
run $RPC nvmf_delete_subsystem "$NQN_GUARD"
run $RPC bdev_aio_delete aio_gmeta
run $RPC bdev_aio_delete aio_gdata
rm -f /tmp/spdkscope/ptpl-gmeta.json /tmp/spdkscope/ptpl-gdata.json
run $RPC bdev_aio_create "$GM_Z" aio_gmeta 4096
run $RPC bdev_aio_create "$GD_Z" aio_gdata 4096
run $RPC nvmf_create_subsystem "$NQN_GUARD" -a -s SPDKSCOPE02
run $RPC nvmf_subsystem_add_ns "$NQN_GUARD" aio_gmeta -n 1 -u "$UUID_GMETA" -p /tmp/spdkscope/ptpl-gmeta.json
run $RPC nvmf_subsystem_add_ns "$NQN_GUARD" aio_gdata -n 2 -u "$UUID_GDATA" -p /tmp/spdkscope/ptpl-gdata.json
run $RPC nvmf_subsystem_add_listener "$NQN_GUARD" -t tcp -a 127.0.0.1 -s 4460 -f ipv4
run nvme connect -t tcp -a 127.0.0.1 -s 4460 -n "$NQN_GUARD"
DEV=$(finddev "$NQN_GUARD" 1) || { log "FATAL: no guard ns1 head node"; exit 1; }
log "guard meta ns (nsid 1) = $DEV"
# record the new mapping for guard-smoke
GDATA_DEV=$(finddev "$NQN_GUARD" 2)
sed -i "s|^DEV_GMETA=.*|DEV_GMETA=$DEV|; s|^DEV_GDATA=.*|DEV_GDATA=$GDATA_DEV|" "$STATE/devices"

step "1. RESCAP (Identify Namespace byte 31 — what resolve_for_mount probes)"
RESCAP=$(nvme id-ns "$DEV" -o json | jq -r .rescap)
log "rescap=$RESCAP ($(printf '0x%02x' "$RESCAP"))"
for b in "0:PTPL-capable" "1:WriteExclusive" "2:ExclusiveAccess" "3:WE-RegistrantsOnly" "4:EA-RegistrantsOnly" "5:WE-AllRegistrants" "6:EA-AllRegistrants"; do
    bit=${b%%:*}; name=${b#*:}
    [ $(( (RESCAP >> bit) & 1 )) = 1 ] && log "  bit$bit $name: YES" || log "  bit$bit $name: no"
done

step "2. Baseline Reservation Report (EDS — fabrics 128-bit hostid form; expect regctl=0 ptpls=0)"
run nvme resv-report "$DEV" --eds -o json

step "3. host1: Register $KEY1 with IEKEY + CPTPL=11b (the guard's register shape)"
run nvme resv-register "$DEV" --nrkey=$KEY1 --rrega=0 --iekey --cptpl=3
run nvme resv-report "$DEV" --eds -o json

step "4. host1: Acquire Write Exclusive (rtype=1)"
run nvme resv-acquire "$DEV" --crkey=$KEY1 --rtype=1 --racqa=0
run nvme resv-report "$DEV" --eds -o json

step "5. Holder write works (host1)"
if run dd if=/dev/zero of="$DEV" bs=4096 count=1 oflag=direct conv=notrunc; then
    log "holder write OK"
else
    log "UNEXPECTED: holder write failed"
fi

step "6. host1 disconnects — registration+reservation must PERSIST on the target"
for c in $(guard_ctrls); do run nvme disconnect --device="$c"; done
sleep 1

step "7. host2 (distinct hostnqn/hostid) connects — sole association; report shows host1 still holder"
run nvme connect -t tcp -a 127.0.0.1 -s 4460 -n "$NQN_GUARD" --hostnqn="$H2NQN" --hostid="$H2UUID"
DEV2=$(finddev "$NQN_GUARD" 1) || { log "FATAL: no head node for host2"; exit 1; }
log "host2 sees $DEV2"
run nvme resv-report "$DEV2" --eds -o json

step "8. Non-holder write MUST fail (WE enforcement — the fence; expect EBADE class)"
if run dd if=/dev/zero of="$DEV2" bs=4096 count=1 oflag=direct conv=notrunc; then
    log "VERDICT: host2 write SUCCEEDED — enforcement BROKEN"
else
    log "VERDICT: host2 (non-holder) write rejected — Write-Exclusive enforcement OK"
fi

step "9. host2: register $KEY2 + PREEMPT victim $KEY1 (the TTL-stale takeover path)"
run nvme resv-register "$DEV2" --nrkey=$KEY2 --rrega=0 --iekey --cptpl=3
run nvme resv-acquire "$DEV2" --crkey=$KEY2 --rtype=1 --racqa=1 --prkey=$KEY1
run nvme resv-report "$DEV2" --eds -o json
if run dd if=/dev/zero of="$DEV2" bs=4096 count=1 oflag=direct conv=notrunc; then
    log "VERDICT: preempting host2 now writes OK — preempt/takeover works; victim key unregistered (see report)"
else
    log "UNEXPECTED: new holder write failed"
fi

step "10. PTPL: reservation survives a target power cycle (save_config -> kill -> relaunch -> load_config)"
$RPC save_config > /tmp/spdkscope/tgt-config.json 2>>"$OUT" && log "config saved ($(wc -c < /tmp/spdkscope/tgt-config.json) B)"
SPDK_PID=$(cat "$STATE/spdk_tgt.pid")
kill "$SPDK_PID"; sleep 2; kill -9 "$SPDK_PID" 2>/dev/null; sleep 1
log "spdk_tgt killed (simulated target power loss)"
"$SPDK_DIR/build/bin/spdk_tgt" -m 0x1000000 -s 1024 -r /tmp/spdkscope/spdk.sock \
    >> /tmp/spdkscope/spdk_tgt.log 2>&1 &
NEWPID=$!
echo "$NEWPID" > "$STATE/spdk_tgt.pid"
for i in $(seq 1 50); do $RPC spdk_get_version >/dev/null 2>&1 && break; sleep 0.2; done
run $RPC load_config -j /tmp/spdkscope/tgt-config.json \
    || { $RPC load_config < /tmp/spdkscope/tgt-config.json >> "$OUT" 2>&1; echo "(stdin load rc=$?)" >> "$OUT"; }
log "spdk_tgt relaunched pid=$NEWPID + config reloaded"
# kernel initiator reconnects the existing association automatically; wait for the ns
DEV2=$(finddev "$NQN_GUARD" 1) || { log "FATAL: guard ns did not come back"; exit 1; }
for i in $(seq 1 60); do nvme resv-report "$DEV2" --eds -o json >/dev/null 2>&1 && break; sleep 2; done
run nvme resv-report "$DEV2" --eds -o json
log "PTPL VERDICT: compare rtype/regctl/rkey/ptpls above vs step 9 (persisted = PTPL works)"

step "11. cleanup: release + unregister, disconnect host2, reconnect host1 for guard-smoke"
run nvme resv-release "$DEV2" --crkey=$KEY2 --rtype=1 --rrela=0
run nvme resv-register "$DEV2" --crkey=$KEY2 --rrega=1
run nvme resv-report "$DEV2" --eds -o json
for c in $(guard_ctrls); do run nvme disconnect --device="$c"; done
sleep 1
run nvme connect -t tcp -a 127.0.0.1 -s 4460 -n "$NQN_GUARD"
DEV=$(finddev "$NQN_GUARD" 1) || { log "FATAL: host1 reconnect failed"; exit 1; }
GDATA_DEV=$(finddev "$NQN_GUARD" 2)
sed -i "s|^DEV_GMETA=.*|DEV_GMETA=$DEV|; s|^DEV_GDATA=.*|DEV_GDATA=$GDATA_DEV|" "$STATE/devices"
run nvme resv-report "$DEV" --eds -o json
log "pr-matrix v2 done (guard devices: meta=$DEV data=$GDATA_DEV)"
