#!/usr/bin/env bash
# tests/pug_funnel_bracket.sh — multi-backend write-funnel acceptance
# (.benchmarks/2026-07-29-probe-up-governor.md §funnel).
#
# The field's second wall: on a 4-wide data plane the pipe sits FULL
# (write_pipeline_inflight ≈ target) while per-device aqu-sz is 2–4.5 —
# admitted-but-not-submitted custody parking between admission and the
# wire. Conviction: the ENOSPC valve's inline drain froze fuse3 tpc
# lanes at steady-state-rewrite fill (sync_drains storms). This bracket
# proves the fix on the venue the dialed rig structurally cannot see:
# 4 × 6 GiB memory-backed null_blk (completion 1.5 ms, discard on) over
# nvmet-tcp — FAST service, MANY backends, 16 GiB fileset (~2/3 fill),
# sustained il rewrite (≥60 s per the 2026-07-29 sustained-state rule;
# thirds flatness reported).
#
# Cells: {forced depth 128 (the field lever), default governor} ×
# {A = branch, B = dev-tip}, A-B-B-A + B-A, 3 reps each.
# Per run: sustained MiB/s + per-third MiB/s, aggregate aqu-sz,
# sync_drains / admission_waits / probe gauges / fence_drops deltas.
#
# Usage: sudo tests/pug_funnel_bracket.sh <results-dir> [cell-filter]
set -u

RESULTS="${1:?results dir}"
FILTER="${2:-.}"
BIN_A="${PUG_BIN_A:-/tmp/sqz-pug-target/release/sqzpugd}"
SO_A="${PUG_SO_A:-/tmp/sqz-pug-target/preload-release/libsqueezefs_il.so}"
BIN_B="${PUG_BIN_B:-/tmp/sqz-dev-tip/target/release/squeezefs}"
SO_B="${PUG_SO_B:-/tmp/sqz-dev-tip/target/preload-release/libsqueezefs_il.so}"
META="${PUG_META:-sqmeta:///dev/nvme17n1,/dev/nvme18n1,/dev/nvme19n1,/dev/nvme20n1}"
DATA="${PUG_FAST_DATA:-sqdata:///dev/nvme25n1,/dev/nvme26n1,/dev/nvme27n1,/dev/nvme28n1}"
MNT=/mnt/sqz_pug
CSV="$RESULTS/funnel.csv"
SECS="${PUG_FUNNEL_SECS:-60}"

[ "$(id -u)" -eq 0 ] || { echo "run as root"; exit 1; }
[ -x "$BIN_A" ] && [ -x "$BIN_B" ] || { echo "missing binaries"; exit 1; }
mkdir -p "$RESULTS" "$MNT"
echo "cell,binary,rep,mibs,third1,third2,third3,aqu,sync_drains,adm_waits,probe_ups,probe_backoffs,fence_drops,inflight_mid,ipc_w,engage" >"$CSV"

wsec() { awk '$3 ~ /nvme2[5-8]n1/ {s+=$10} END {print s}' /proc/diskstats; }
wq() { awk '$3 ~ /nvme2[5-8]n1/ {q+=$14} END {print q}' /proc/diskstats; }
stat_get() { python3 -c "import json;print(json.load(open('$MNT/.stats'))['metrics'].get('$1',0))" 2>/dev/null || echo 0; }

run_one() { # cell blabel bin so rep depth
    local cell="$1" blabel="$2" bin="$3" so="$4" rep="$5" depth="$6"
    local tag="${cell}_${blabel}_r${rep}"
    echo "=== $tag"
    "$bin" format "$META" "$DATA" --force >>"$RESULTS/$tag.log" 2>&1 || { echo "format FAILED"; exit 1; }
    udevadm settle --timeout=10 2>/dev/null || true
    local envp=(env)
    [ -n "$depth" ] && envp+=("SQUEEZEFS_WRITE_PIPELINE_DEPTH_BLOCKS=$depth")
    "${envp[@]}" "$bin" mount "$META" "$MNT" --daemon --allow-others -o interception \
        --log-file "$RESULTS/$tag.daemon.log" >>"$RESULTS/$tag.log" 2>&1 || { echo "mount FAILED"; exit 1; }
    sleep 1; mkdir -p "$MNT/bench"
    env LD_PRELOAD="$so" elbencho --write --direct -t 32 -b 4M -s 512m --nolive \
        "$MNT"/bench/f{1..32} >"$RESULTS/$tag.pre.txt" 2>&1

    local sd0 aw0 pu0 pb0 fd0 iw0
    iw0=$(stat_get ipc_ops_write)
    sd0=$(stat_get block_free_reclaim_sync_drains)
    aw0=$(stat_get write_pipeline_admission_waits)
    pu0=$(stat_get write_pipeline_depth_probe_ups)
    pb0=$(stat_get write_pipeline_depth_probe_backoffs)
    fd0=$(stat_get write_pipeline_fence_drops)
    local s0 q0 t0
    s0=$(wsec); q0=$(wq); t0=$(date +%s.%N)
    env LD_PRELOAD="$so" elbencho --write --direct -t 32 -b 4M -s 512m \
        --timelimit "$SECS" --infloop --nolive "$MNT"/bench/f{1..32} >"$RESULTS/$tag.re.txt" 2>&1 &
    local B=$!
    local third=$((SECS / 3))
    sleep "$third"; local s1; s1=$(wsec)
    sleep "$third"; local s2 im; s2=$(wsec); im=$(stat_get write_pipeline_inflight_blocks)
    wait $B
    local t1 s3 q1
    t1=$(date +%s.%N); s3=$(wsec); q1=$(wq)
    local mibs
    mibs=$(awk '/Throughput MiB\/s/ {print $NF}' "$RESULTS/$tag.re.txt" | tail -1)
    local sd aw pu pb fd
    sd=$(( $(stat_get block_free_reclaim_sync_drains) - sd0 ))
    aw=$(( $(stat_get write_pipeline_admission_waits) - aw0 ))
    pu=$(( $(stat_get write_pipeline_depth_probe_ups) - pu0 ))
    pb=$(( $(stat_get write_pipeline_depth_probe_backoffs) - pb0 ))
    fd=$(( $(stat_get write_pipeline_fence_drops) - fd0 ))
    local iw engage=ok
    iw=$(( $(stat_get ipc_ops_write) - iw0 ))
    # Charter rule 4 (KD-7 tripwire): a mismatched daemon/shim pair
    # HELLO-refuses into silent passthrough and the il row is INVALID.
    [ "$iw" -gt 0 ] || { engage=INVALID-passthrough; ENGAGE_FAIL=1; }
    local row
    row=$(python3 -c "
el=$t1-$t0
t1=($s1-$s0)*512/1048576.0/$third
t2=($s2-$s1)*512/1048576.0/$third
t3=($s3-$s2)*512/1048576.0/(el-2*$third)
aqu=($q1-$q0)/1000.0/el
print(f'{t1:.0f},{t2:.0f},{t3:.0f},{aqu:.1f}')")
    echo "$cell,$blabel,$rep,${mibs:-0},$row,$sd,$aw,$pu,$pb,$fd,$im,$iw,$engage" >>"$CSV"
    tail -1 "$CSV"
    "$bin" umount "$MNT" >>"$RESULTS/$tag.log" 2>&1 || umount "$MNT" || true
    for _ in $(seq 1 120); do
        pgrep -f "mount .* $MNT" >/dev/null || break
        sleep 0.5
    done
    sleep 1
}

bracket() { # cell depth
    local cell="$1" depth="$2"
    echo "$cell" | grep -Eq "$FILTER" || return 0
    run_one "$cell" A "$BIN_A" "$SO_A" 1 "$depth"
    run_one "$cell" B "$BIN_B" "$SO_B" 1 "$depth"
    run_one "$cell" B "$BIN_B" "$SO_B" 2 "$depth"
    run_one "$cell" A "$BIN_A" "$SO_A" 2 "$depth"
    run_one "$cell" B "$BIN_B" "$SO_B" 3 "$depth"
    run_one "$cell" A "$BIN_A" "$SO_A" 3 "$depth"
}

ENGAGE_FAIL=0
bracket storm_f128 128
bracket storm_default ""

echo "DONE -> $CSV"
[ "$ENGAGE_FAIL" = 0 ] || { echo "ENGAGEMENT INVALID rows present (KD-7 pair mismatch?)"; exit 1; }
