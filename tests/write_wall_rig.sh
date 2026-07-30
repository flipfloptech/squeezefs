#!/usr/bin/env bash
# tests/write_wall_rig.sh — write-wall campaign acceptance brackets
# (.benchmarks/2026-07-31-write-wall.md).
#
# A-B-B-A alternating-order brackets (standing 2026-07-27 comparison rule)
# on an nvmet-tcp substrate (the two-substrate rule: fabric-sensitive
# write rows are MANDATORY tcp — either the standard tcp devsub or the
# DIALED 235 µs fabric-latency rig; STATE WHICH in the evidence note):
# BINARY_A (campaign tip) vs BINARY_B (dev c9921f1), fresh format per
# run, kernel path, medians of 3.
#
# Campaign instruments on every write pass:
#   * conviction 1 (rewrite wall): block_free_reclaim_{queued,commands,
#     batches,sync_drains} deltas, block_free_reclaim_cap_parks
#     (MUST be 0 — the at-cap engagement tripwire), and a 20 Hz sampler
#     recording the MAX block_free_reclaim_queue_bytes seen during the
#     pass (the queue-never-caps proof).
#   * conviction 2 (fresh wall): per-pass write_pipeline_phase_ns
#     snapshots (before/after JSON) + a bucket-midpoint residence table
#     per pass (phase, spans, est-mean, est-total-ms) — where the
#     residence went, named by numbers.
#
# Cells: stream (fresh + rewrite + sustained >=60 s rewrite), rand4k
# (non-regression), read (non-regression).
#
# Instrument (stated): elbencho 3.1-10 (dynamic), --direct, sync driver.
#
# Usage:
#   sudo WW_BIN_A=... WW_BIN_B=... WW_META=sqmeta://... WW_DATA=sqdata://... \
#        WW_META_DEVS="nvmeXn1 ..." WW_DATA_DEVS="nvmeYn1 ..." \
#        tests/write_wall_rig.sh <results-dir> [cell-filter-regex]
set -u

RESULTS="${1:?results dir}"
FILTER="${2:-.}"
REPO="$(cd "$(dirname "$0")/.." && pwd)"
BIN_A="${WW_BIN_A:-$REPO/target/release/squeezefs}"
BIN_B="${WW_BIN_B:?baseline (dev c9921f1) binary}"
META="${WW_META:?sqmeta uri}"
DATA="${WW_DATA:?sqdata uri}"
read -r -a META_DEVS <<<"${WW_META_DEVS:?meta namespaces}"
read -r -a DATA_DEVS <<<"${WW_DATA_DEVS:?data namespaces}"
MNT="${WW_MNT:-/mnt/sqz_ww}"
THREADS="${WW_THREADS:-16}"
FILE_MB="${WW_FILE_MB:-512}"
CSV="$RESULTS/bracket.csv"

[ "$(id -u)" -eq 0 ] || { echo "run as root"; exit 1; }
[ -x "$BIN_A" ] && [ -x "$BIN_B" ] || { echo "missing binaries"; exit 1; }
mkdir -p "$RESULTS" "$MNT"
[ -f "$CSV" ] || echo "cell,pass,binary,rep,mibs_or_iops,meta_writes,meta_wbytes,journal_entries,journal_bytes,pub_batches,pub_blocks,wt_blocks,adm_waits,rq_queued,rq_parks,rq_cmds,rq_batches,rq_syncdrains,rq_qbytes_max,data_aqu,data_wareq_kib,data_amp" >"$CSV"

ds_snap() { # <devs...> -> "writes sectors weighted_ms"
    awk -v devs="$*" '
        BEGIN { split(devs, d, " "); for (i in d) want[d[i]] = 1 }
        want[$3] { w += $8; s += $10; q += $14 }
        END { print w+0, s+0, q+0 }' /proc/diskstats
}

stat_get() { # <key>
    python3 -c "import json,sys;print(json.load(open('$MNT/.stats'))['metrics'].get('$1',0))" 2>/dev/null || echo 0
}

snap_gauges() { # -> one line of campaign gauges
    echo "$(stat_get meta_kv_journal_entries) $(stat_get meta_kv_journal_bytes) \
$(stat_get layout_publish_batches) $(stat_get layout_publish_batched_blocks) \
$(stat_get write_through_blocks) $(stat_get write_pipeline_admission_waits) \
$(stat_get block_free_reclaim_queued) $(stat_get block_free_reclaim_cap_parks) \
$(stat_get block_free_reclaim_commands) $(stat_get block_free_reclaim_batches) \
$(stat_get block_free_reclaim_sync_drains)"
}

phase_snap() { # <file>
    python3 -c "
import json
try:
    m = json.load(open('$MNT/.stats'))['metrics']
    json.dump(m.get('write_pipeline_phase_ns', {}), open('$1','w'))
except Exception:
    json.dump({}, open('$1','w'))"
}

phase_table() { # <before.json> <after.json> <label>
    python3 - "$1" "$2" "$3" <<'EOF'
import json, sys
b, a, label = json.load(open(sys.argv[1])), json.load(open(sys.argv[2])), sys.argv[3]
if not a:
    print(f"[phases] {label}: family absent (baseline binary)"); sys.exit(0)
order = ["admit_wait","detach_lag","lock_wait","crypto","allocate","dma",
         "publish","displaced_free","inval_tail","total"]
# CANONICAL bucket order (serde_json maps serialize alphabetically — a
# positional read over the JSON key order misprices every bucket).
labels = ["<=1us","<=2us","<=4us","<=8us","<=16us","<=32us","<=64us",
          "<=128us","<=256us","<=512us","<=1024us","<=2ms","<=4ms","<=8ms",
          "<=16ms","<=32ms","<=64ms","<=128ms","<=256ms","<=512ms",
          "<=1024ms","<=2s","<=4s","<=8s","<=16s",">16s"]
def mid_us(i):  # bucket i spans (2^(i-1), 2^i] us; 0 = <=1us
    return 1.0 if i == 0 else 1.5 * (1 << (i - 1))
print(f"[phases] {label}: phase, spans, est_mean_ms, est_total_ms")
for ph in order:
    if ph not in a: continue
    d = [a[ph].get(k, 0) - (b.get(ph, {}).get(k, 0) if b else 0) for k in labels]
    n = sum(d)
    tot_us = sum(c * mid_us(i) for i, c in enumerate(d))
    mean_ms = (tot_us / n / 1000.0) if n else 0.0
    print(f"[phases] {label}: {ph:14s} {n:8d} {mean_ms:10.3f} {tot_us/1000.0:12.1f}")
EOF
}

sampler_start() { # <outfile> — 20 Hz reclaim queue-bytes gauge sampler
    (while :; do stat_get block_free_reclaim_queue_bytes; sleep 0.05; done) >"$1" &
    echo $!
}

sampler_stop() { # <pid> <outfile> -> max
    kill "$1" 2>/dev/null
    wait "$1" 2>/dev/null
    sort -n "$2" | tail -1
}

mount_fresh() { # <bin> <tag>
    local bin="$1" tag="$2"
    "$bin" format "$META" "$DATA" --force >>"$RESULTS/$tag.log" 2>&1 || { echo "format FAILED ($tag)"; exit 1; }
    udevadm settle --timeout=10 2>/dev/null || true
    local ok=0
    for _ in 1 2 3 4 5; do
        if "$bin" mount "$META" "$MNT" --daemon --allow-other \
            --log-file "$RESULTS/$tag.daemon.log" >>"$RESULTS/$tag.log" 2>&1; then
            ok=1; break
        fi
        sleep 2
    done
    [ "$ok" = 1 ] || { echo "mount FAILED ($tag)"; exit 1; }
    sleep 1
    mkdir -p "$MNT/bench"
}

unmount_bin() { # <bin> <tag>
    local bin="$1" tag="$2"
    "$bin" umount "$MNT" >>"$RESULTS/$tag.log" 2>&1 || umount "$MNT" || true
    for _ in $(seq 1 60); do
        pgrep -f "squeezefs mount .* $MNT" >/dev/null || break
        sleep 0.5
    done
    sleep 1
}

emit_row() { # cell pass blabel rep mibs g0 g1 s0 s1 m0 m1 elapsed user_bytes qmax
    local cell="$1" pass="$2" blabel="$3" rep="$4" mibs="$5"
    local g0="$6" g1="$7" s0="$8" s1="$9" m0="${10}" m1="${11}" el="${12}" ub="${13}" qmax="${14}"
    python3 -c "
g0='$g0'.split(); g1='$g1'.split()
je,jb,pb,pbl,wt,aw,rq,sp,cm,ba,sd = [int(b)-int(a) for a,b in zip(g0,g1)]
w0,s0,q0 = '$s0'.split(); w1,s1,q1 = '$s1'.split()
m0w,m0s,_ = '$m0'.split(); m1w,m1s,_ = '$m1'.split()
el=$el; ub=$ub
dw=int(w1)-int(w0); ds=int(s1)-int(s0); dq=int(q1)-int(q0)
mw=int(m1w)-int(m0w); ms=(int(m1s)-int(m0s))*512
aqu = dq/1000.0/el if el>0 else 0
wareq = ds*512/dw/1024.0 if dw>0 else 0
amp = ds*512/ub if ub>0 else 0
print(f'$cell,$pass,$blabel,$rep,$mibs,{mw},{ms},{je},{jb},{pb},{pbl},{wt},{aw},{rq},{sp},{cm},{ba},{sd},$qmax,{aqu:.2f},{wareq:.0f},{amp:.3f}')" >>"$CSV"
    tail -1 "$CSV"
}

bench_files() {
    local f=()
    for i in $(seq 1 "$THREADS"); do f+=("$MNT/bench/f$i"); done
    echo "${f[@]}"
}

run_stream() { # blabel bin rep
    local blabel="$1" bin="$2" rep="$3" tag="stream_${1}_r${3}"
    mount_fresh "$bin" "$tag"
    local files; read -r -a files <<<"$(bench_files)"
    local ub=$((THREADS * FILE_MB * 1024 * 1024))

    for pass in fresh rewrite sustained; do
        local g0 g1 s0 s1 m0 m1 t0 t1 out mibs spid qmax
        phase_snap "$RESULTS/$tag.$pass.phases_before.json"
        g0=$(snap_gauges); s0=$(ds_snap "${DATA_DEVS[@]}"); m0=$(ds_snap "${META_DEVS[@]}")
        spid=$(sampler_start "$RESULTS/$tag.$pass.qbytes.samples")
        t0=$(date +%s.%N)
        case "$pass" in
        fresh | rewrite)
            out=$(elbencho --write --direct -t "$THREADS" -b 1m -s "${FILE_MB}m" --nolive "${files[@]}" 2>&1)
            ;;
        sustained)
            out=$(elbencho --write --direct -t "$THREADS" -b 1m -s "${FILE_MB}m" \
                --timelimit 60 --infloop --nolive "${files[@]}" 2>&1)
            ;;
        esac
        t1=$(date +%s.%N)
        qmax=$(sampler_stop "$spid" "$RESULTS/$tag.$pass.qbytes.samples")
        g1=$(snap_gauges); s1=$(ds_snap "${DATA_DEVS[@]}"); m1=$(ds_snap "${META_DEVS[@]}")
        phase_snap "$RESULTS/$tag.$pass.phases_after.json"
        echo "$out" >"$RESULTS/$tag.$pass.elbencho.txt"
        mibs=$(echo "$out" | awk '/Throughput MiB\/s/ { print $NF }' | tail -1)
        local el; el=$(python3 -c "print($t1-$t0)")
        local pass_ub=$ub
        [ "$pass" = sustained ] && pass_ub=$(python3 -c "print(int(${mibs:-0}*1048576*$el))")
        emit_row stream "$pass" "$blabel" "$rep" "${mibs:-0}" "$g0" "$g1" "$s0" "$s1" "$m0" "$m1" "$el" "$pass_ub" "${qmax:-0}"
        phase_table "$RESULTS/$tag.$pass.phases_before.json" "$RESULTS/$tag.$pass.phases_after.json" "$tag.$pass" | tee -a "$RESULTS/phases.txt"
    done
    unmount_bin "$bin" "$tag"
}

run_rand4k() { # blabel bin rep
    local blabel="$1" bin="$2" rep="$3" tag="rand4k_${1}_r${3}"
    mount_fresh "$bin" "$tag"
    local files; read -r -a files <<<"$(bench_files)"
    elbencho --write --direct -t "$THREADS" -b 1m -s "${FILE_MB}m" --nolive "${files[@]}" \
        >"$RESULTS/$tag.prefill.txt" 2>&1
    local g0 g1 s0 s1 m0 m1 t0 t1 out iops
    g0=$(snap_gauges); s0=$(ds_snap "${DATA_DEVS[@]}"); m0=$(ds_snap "${META_DEVS[@]}")
    t0=$(date +%s.%N)
    out=$(elbencho --write --direct --rand -t "$THREADS" -b 4k -s "${FILE_MB}m" \
        --timelimit 30 --nolive "${files[@]}" 2>&1)
    t1=$(date +%s.%N)
    g1=$(snap_gauges); s1=$(ds_snap "${DATA_DEVS[@]}"); m1=$(ds_snap "${META_DEVS[@]}")
    echo "$out" >"$RESULTS/$tag.elbencho.txt"
    iops=$(echo "$out" | awk '/IOPS/ { print $NF }' | tail -1)
    local el; el=$(python3 -c "print($t1-$t0)")
    emit_row rand4k write "$blabel" "$rep" "${iops:-0}" "$g0" "$g1" "$s0" "$s1" "$m0" "$m1" "$el" 0 0
    unmount_bin "$bin" "$tag"
}

run_read() { # blabel bin rep
    local blabel="$1" bin="$2" rep="$3" tag="read_${1}_r${3}"
    mount_fresh "$bin" "$tag"
    local files; read -r -a files <<<"$(bench_files)"
    elbencho --write --direct -t "$THREADS" -b 1m -s "${FILE_MB}m" --nolive "${files[@]}" \
        >"$RESULTS/$tag.prefill.txt" 2>&1
    sync
    local g0 g1 s0 s1 m0 m1 t0 t1 out mibs
    g0=$(snap_gauges); s0=$(ds_snap "${DATA_DEVS[@]}"); m0=$(ds_snap "${META_DEVS[@]}")
    t0=$(date +%s.%N)
    out=$(elbencho --read --direct -t "$THREADS" -b 1m -s "${FILE_MB}m" --nolive "${files[@]}" 2>&1)
    t1=$(date +%s.%N)
    g1=$(snap_gauges); s1=$(ds_snap "${DATA_DEVS[@]}"); m1=$(ds_snap "${META_DEVS[@]}")
    echo "$out" >"$RESULTS/$tag.elbencho.txt"
    mibs=$(echo "$out" | awk '/Throughput MiB\/s/ { print $NF }' | tail -1)
    local el; el=$(python3 -c "print($t1-$t0)")
    emit_row read seq "$blabel" "$rep" "${mibs:-0}" "$g0" "$g1" "$s0" "$s1" "$m0" "$m1" "$el" 0 0
    unmount_bin "$bin" "$tag"
}

bracket() { # cell fn
    local cell="$1" fn="$2"
    echo "$cell" | grep -Eq "$FILTER" || return 0
    # A-B-B-A + B-A: three reps each, both orders represented.
    "$fn" A "$BIN_A" 1
    "$fn" B "$BIN_B" 1
    "$fn" B "$BIN_B" 2
    "$fn" A "$BIN_A" 2
    "$fn" B "$BIN_B" 3
    "$fn" A "$BIN_A" 3
}

bracket stream run_stream
bracket rand4k run_rand4k
bracket read run_read

echo "DONE -> $CSV"
