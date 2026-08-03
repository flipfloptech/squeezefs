#!/usr/bin/env bash
# tests/wce_bracket.sh — write-commit-economy acceptance brackets
# (.benchmarks/2026-07-30-write-commit-economy.md).
#
# A-B-B-A alternating-order brackets (standing 2026-07-27 comparison rule)
# on the nvmet-tcp devsub rig (the two-substrate rule: fabric-sensitive
# write rows are MANDATORY tcp): BINARY_A (campaign tip) vs BINARY_B
# (dev e076db7), fresh format per run, kernel path, medians of 3.
#
# Cells:
#   stream  — fresh streaming pass (16t x 1M x FILE_MB) + rewrite pass
#             (same files) + a SUSTAINED >=60 s rewrite window
#             (--timelimit 60 --infloop; the 2026-07-29 sustained-state
#             rule). Per pass: elbencho MiB/s, META-namespace
#             device-writes/block + journal bytes/entries deltas (the
#             campaign gauges), conveyor group median, coalesce factor,
#             delta-commit engagement, DATA-namespace aqu/wareq/amp.
#   rand4k  — prefilled rand-4k --direct write window (non-regression).
#   read    — prefilled seq-read pass (non-regression).
#   meta    — create+unlink storm; journal entries/op (G4-class economy
#             non-regression).
#
# Instrument (stated): elbencho 3.1-10 (dynamic), --direct, sync driver;
# substrate: SQZ_DEVSUB_TRANSPORT=tcp (nvmet-tcp on localhost).
#
# Usage:
#   sudo WCE_BIN_A=... WCE_BIN_B=... WCE_META=sqmeta://... WCE_DATA=sqdata://... \
#        WCE_META_DEVS="nvmeXn1 ..." WCE_DATA_DEVS="nvmeYn1 ..." \
#        tests/wce_bracket.sh <results-dir> [cell-filter-regex]
set -u

RESULTS="${1:?results dir}"
FILTER="${2:-.}"
REPO="$(cd "$(dirname "$0")/.." && pwd)"
BIN_A="${WCE_BIN_A:-$REPO/target/release/squeezefs}"
BIN_B="${WCE_BIN_B:-/tmp/sqz-dev-tip/target/release/squeezefs}"
META="${WCE_META:?sqmeta uri}"
DATA="${WCE_DATA:?sqdata uri}"
read -r -a META_DEVS <<<"${WCE_META_DEVS:?meta namespaces}"
read -r -a DATA_DEVS <<<"${WCE_DATA_DEVS:?data namespaces}"
MNT="${WCE_MNT:-/mnt/sqz_wce}"
THREADS="${WCE_THREADS:-16}"
FILE_MB="${WCE_FILE_MB:-512}"
CSV="$RESULTS/bracket.csv"

[ "$(id -u)" -eq 0 ] || { echo "run as root"; exit 1; }
[ -x "$BIN_A" ] && [ -x "$BIN_B" ] || { echo "missing binaries"; exit 1; }
mkdir -p "$RESULTS" "$MNT"
[ -f "$CSV" ] || echo "cell,pass,binary,rep,mibs_or_ops,meta_writes,meta_wbytes,journal_entries,journal_bytes,delta_commits,full_commits,delta_bytes,pub_batches,pub_blocks,group_median,adm_waits,wt_blocks,data_aqu,data_wareq_kib,data_amp" >"$CSV"

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
$(stat_get layout_delta_commits) $(stat_get layout_full_commits) $(stat_get layout_delta_bytes) \
$(stat_get layout_publish_batches) $(stat_get layout_publish_batched_blocks) \
$(stat_get write_pipeline_admission_waits) $(stat_get write_through_blocks)"
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

emit_row() { # cell pass blabel rep mibs g0 g1 s0 s1 m0 m1 elapsed user_bytes
    local cell="$1" pass="$2" blabel="$3" rep="$4" mibs="$5"
    local g0="$6" g1="$7" s0="$8" s1="$9" m0="${10}" m1="${11}" el="${12}" ub="${13}"
    local gm; gm=$(stat_get meta_commit_group_size_median_lb)
    python3 -c "
g0='$g0'.split(); g1='$g1'.split()
je,jb,dc,fc,db,pb,pbl,aw,wt = [int(b)-int(a) for a,b in zip(g0,g1)]
w0,s0,q0 = '$s0'.split(); w1,s1,q1 = '$s1'.split()
m0w,m0s,_ = '$m0'.split(); m1w,m1s,_ = '$m1'.split()
el=$el; ub=$ub
dw=int(w1)-int(w0); ds=int(s1)-int(s0); dq=int(q1)-int(q0)
mw=int(m1w)-int(m0w); ms=(int(m1s)-int(m0s))*512
aqu = dq/1000.0/el if el>0 else 0
wareq = ds*512/dw/1024.0 if dw>0 else 0
amp = ds*512/ub if ub>0 else 0
print(f'$cell,$pass,$blabel,$rep,$mibs,{mw},{ms},{je},{jb},{dc},{fc},{db},{pb},{pbl},$gm,{aw},{wt},{aqu:.2f},{wareq:.0f},{amp:.3f}')" >>"$CSV"
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
        local g0 g1 s0 s1 m0 m1 t0 t1 out mibs
        g0=$(snap_gauges); s0=$(ds_snap "${DATA_DEVS[@]}"); m0=$(ds_snap "${META_DEVS[@]}")
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
        g1=$(snap_gauges); s1=$(ds_snap "${DATA_DEVS[@]}"); m1=$(ds_snap "${META_DEVS[@]}")
        echo "$out" >"$RESULTS/$tag.$pass.elbencho.txt"
        mibs=$(echo "$out" | awk '/Throughput MiB\/s/ { print $NF }' | tail -1)
        local el; el=$(python3 -c "print($t1-$t0)")
        local pass_ub=$ub
        [ "$pass" = sustained ] && pass_ub=$(python3 -c "print(int(${mibs:-0}*1048576*$el))")
        emit_row stream "$pass" "$blabel" "$rep" "${mibs:-0}" "$g0" "$g1" "$s0" "$s1" "$m0" "$m1" "$el" "$pass_ub"
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
    emit_row rand4k write "$blabel" "$rep" "${iops:-0}" "$g0" "$g1" "$s0" "$s1" "$m0" "$m1" "$el" 0
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
    emit_row read seq "$blabel" "$rep" "${mibs:-0}" "$g0" "$g1" "$s0" "$s1" "$m0" "$m1" "$el" 0
    unmount_bin "$bin" "$tag"
}

run_meta() { # blabel bin rep — create+unlink storm, journal entries/op
    local blabel="$1" bin="$2" rep="$3" tag="meta_${1}_r${3}"
    mount_fresh "$bin" "$tag"
    mkdir -p "$MNT/bench/meta"
    local n=2000
    local g0 g1 t0 t1 m0 m1
    g0=$(snap_gauges); m0=$(ds_snap "${META_DEVS[@]}")
    t0=$(date +%s.%N)
    elbencho -w -t 8 -n 250 -N 1 -s 4k -d --nolive "$MNT/bench/meta" >"$RESULTS/$tag.create.txt" 2>&1
    elbencho -F -D -t 8 -n 250 -N 1 -d --nolive "$MNT/bench/meta" >"$RESULTS/$tag.del.txt" 2>&1
    t1=$(date +%s.%N)
    g1=$(snap_gauges); m1=$(ds_snap "${META_DEVS[@]}")
    local el; el=$(python3 -c "print($t1-$t0)")
    local opss; opss=$(python3 -c "print(f'{2*$n/$el:.0f}')")
    emit_row meta create_unlink "$blabel" "$rep" "$opss" "$g0" "$g1" "0 0 0" "0 0 0" "$m0" "$m1" "$el" 0
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
bracket meta run_meta

echo "DONE -> $CSV"
