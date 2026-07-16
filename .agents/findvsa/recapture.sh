#!/bin/bash
# FIND-VS-A crash-image RE-CAPTURE (docs/design-smo-replay-currency.md §6 PR 1
# first act): the raw post-kill images from the original forensics no longer
# exist, so this script regenerates them — the repro4.sh storm + kill -9 shape
# with page-cache-coherent image copies taken BEFORE any remount — and loops
# rounds until one round shows acked loss across a successful clean-replay
# remount (the sub-mechanism (i) stranding face; child-seq refusal rounds are
# kept too, tagged REFUSED — the (ii) face).
#
# Output (under $SANDBOX/capture_round<N>/):
#   meta{1..4}.img        page-cache-coherent post-kill meta volumes (sparse)
#   data{1..4}.img        matching data volumes (sparse; needed for remounts)
#   tree.acked            union of per-worker ack logs at kill
#   missing.txt           acked names ENOENT after the in-place remount
#   stats.remount         remount .stats (replay_dropped_torn etc.)
#   mount_recheck.log     the remount daemon log (refusals show here)
#   expectations.txt      kvparse.py ledger/ring/chain output per image
#
# Rails: unique sandbox (never /mnt/squeezefs, /mnt/juicefs, ~/tmp/nvme);
# kills by PID only; systemd-run --user scope cages the daemon; full CPU
# mask (FIND-VS-B precedent — no taskset on repro runs).
#
# Usage: SANDBOX=~/tmp/smo_b1_$$ KILL_AFTER=4 MAX_ROUNDS=6 .agents/findvsa/recapture.sh
set -u
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
BIN="${BIN:-$REPO/target/release/squeezefs}"
SANDBOX="${SANDBOX:-$HOME/tmp/smo_b1_$$}"
KILL_AFTER="${KILL_AFTER:-4}"
MAX_ROUNDS="${MAX_ROUNDS:-6}"
MNT="$SANDBOX/mnt"
mkdir -p "$SANDBOX" "$MNT"
die() { echo "FATAL: $*" >&2; exit 9; }
[ -x "$BIN" ] || die "binary not built: $BIN"
meta_uri() { echo "sqmeta://$SANDBOX/meta1.img,$SANDBOX/meta2.img,$SANDBOX/meta3.img,$SANDBOX/meta4.img"; }

do_format() {
    rm -f "$SANDBOX"/meta{1,2,3,4}.img "$SANDBOX"/data{1,2,3,4}.img
    rm -rf "$SANDBOX/staging"; mkdir -p "$SANDBOX/staging"
    for i in 1 2 3 4; do truncate -s 1G "$SANDBOX/meta$i.img"; truncate -s 4G "$SANDBOX/data$i.img"; done
    "$BIN" format "$(meta_uri)" \
        "sqdata://$SANDBOX/data1.img,$SANDBOX/data2.img,$SANDBOX/data3.img,$SANDBOX/data4.img" \
        --disk-cache-paths "$SANDBOX/staging" --force >"$ART/format.log" 2>&1 || die format
}
do_mount() { # <tag>
    local logf="$ART/mount_$1.log"
    systemd-run --user --scope --unit "sqzsmo-$1-$$-$RANDOM" -p MemoryMax=16G -p MemorySwapMax=0 --quiet \
        "$BIN" mount "$(meta_uri)" "$MNT" --daemon \
        --mem-budget 4096M --disk-cache-size 4096MB --log-file "$logf" >>"$logf" 2>&1
    for i in $(seq 1 100); do
        mountpoint -q "$MNT" && grep -q "transport armed for this session" "$logf" 2>/dev/null && break
        grep -q "mount refused\|Failed to start\|refusing" "$logf" 2>/dev/null && break
        sleep 0.3
    done
    mountpoint -q "$MNT" || { tail -8 "$logf" >&2; return 1; }
    SQZ_PID="$(pgrep -f "squeezefs mount sqmeta://$SANDBOX" | head -1)"
    echo "mounted $1 pid=$SQZ_PID"
}
teardown_mnt() {
    "$BIN" umount "$MNT" >>"$ART/umounts.log" 2>&1 </dev/null || fusermount3 -u "$MNT" 2>/dev/null || true
    for i in $(seq 1 100); do mountpoint -q "$MNT" || break; sleep 0.2; done
    stat "$MNT" >/dev/null 2>&1 || { fusermount3 -uz "$MNT" 2>/dev/null || umount -l "$MNT" 2>/dev/null || true; sleep 0.3; }
    pgrep -f "squeezefs mount sqmeta://$SANDBOX" | xargs -r kill -9 2>/dev/null
    sleep 0.3
}

CAPTURED=""
for round in $(seq 1 "$MAX_ROUNDS"); do
    ART="$SANDBOX/round$round"
    mkdir -p "$ART"
    echo "=== round $round (KILL_AFTER=$KILL_AFTER) artifacts: $ART"
    do_format
    do_mount storm || die "storm mount round $round"
    RUNID="s$(date +%s | tail -c 6)"
    python3 "$REPO/.agents/findvsa/storm_creator.py" "$MNT" "$ART" 16 8 1024 "$RUNID" >"$ART/storm.log" 2>&1 &
    STORM_PID=$!
    sleep "$KILL_AFTER"
    kill -9 "$SQZ_PID"
    echo "killed daemon pid=$SQZ_PID"
    wait $STORM_PID || true
    fusermount3 -uz "$MNT" 2>/dev/null || umount -l "$MNT" 2>/dev/null || true
    sleep 0.5
    cat "$ART"/acked.* | sort >"$ART/tree.acked"
    echo "acked: $(wc -l < "$ART/tree.acked")"
    # page-cache-coherent copies BEFORE any remount (repro4.sh:79-84 shape)
    CAP="$SANDBOX/capture_round$round"
    mkdir -p "$CAP"
    for i in 1 2 3 4; do
        cp --sparse=always "$SANDBOX/meta$i.img" "$CAP/meta$i.img"
        cp --sparse=always "$SANDBOX/data$i.img" "$CAP/data$i.img"
    done
    cp "$ART/tree.acked" "$CAP/tree.acked"
    # in-place remount: the loss/refusal adjudication
    if do_mount recheck; then
        sleep 2
        cp "$MNT/.stats" "$ART/stats.remount" 2>/dev/null || true
        cp "$ART/stats.remount" "$CAP/stats.remount" 2>/dev/null || true
        find "$MNT/ktree" -type f 2>/dev/null | sort >"$ART/tree.postcrash"
        comm -23 "$ART/tree.acked" "$ART/tree.postcrash" >"$ART/missing.txt"
        cp "$ART/missing.txt" "$CAP/missing.txt"
        MISS=$(wc -l < "$ART/missing.txt")
        DT=$(grep -o '"replay_dropped_torn":[0-9]*' "$ART/stats.remount" 2>/dev/null | head -1)
        echo "round $round: MISSING-ACKED=$MISS $DT"
        teardown_mnt
        cp "$ART/mount_recheck.log" "$CAP/mount_recheck.log" 2>/dev/null || true
        if [ "$MISS" -ge 1 ]; then
            echo "LOSS" >"$CAP/verdict"; CAPTURED="$CAP"
            echo "=== LOSS round captured: $CAP (missing=$MISS)"; break
        fi
        echo "CLEAN" >"$CAP/verdict"
        rm -f "$CAP"/data{1,2,3,4}.img   # keep clean rounds' meta only (cheap)
    else
        echo "round $round: REMOUNT REFUSED (the (ii) face) — captured, continuing for a loss round"
        cp "$ART/mount_recheck.log" "$CAP/mount_recheck.log" 2>/dev/null || true
        echo "REFUSED" >"$CAP/verdict"
        pgrep -f "squeezefs mount sqmeta://$SANDBOX" | xargs -r kill -9 2>/dev/null
        fusermount3 -uz "$MNT" 2>/dev/null || true
        sleep 0.5
    fi
done

[ -n "$CAPTURED" ] || { echo "NO LOSS ROUND in $MAX_ROUNDS rounds — see $SANDBOX/round*/"; exit 3; }
# kvparse.py-verified expectations + the stranding fixture over the captured
# images (ledger slots, ring census, chain walk, lost-key window records).
python3 "$REPO/.agents/findvsa/extract_stranding_fixture.py" "$CAPTURED" >"$CAPTURED/expectations.txt" || die "extractor failed"
tail -25 "$CAPTURED/expectations.txt"
echo "=== capture complete: $CAPTURED"
