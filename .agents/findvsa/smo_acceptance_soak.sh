#!/bin/bash
# SMO replay-currency program — PR 5 ACCEPTANCE soak
# (docs/design-smo-replay-currency.md §6 PR 5 row: the charter's
# STOP-condition inverse, widened by the §1b signature).
#
# Runs N consecutive rounds of the FIND-VS-A storm shape (the recapture.sh
# round body: 16-worker acked-create storm, kill -9 at peak, page-cache-
# coherent pre-remount image capture, in-place remount, audit) and asserts
# JOINTLY per round:
#
#     acked-loss == 0   AND   mount-refusals == 0   AND
#     replay_dropped_torn == 0 on every volume
#
# — never the counter alone (the §1b newest-entry edge: a mid-entry tail on
# the newest entry can read dropped_torn == 0, so loss and counter are
# asserted together). Each round also RECORDS the replay-window sizes
# (meta_kv_replay_entries per volume) and the post-remount pending_free
# gauge with a drain re-sample (Branch 3 contract: parked window frees drain
# to 0 at the first post-mount durable checkpoints).
#
# ANY failed round: STOP immediately (multi-run discipline — a failure is a
# new finding, not a retry), preserve that round's pre-remount images + all
# logs under $SANDBOX/capture_round<N>/, exit 1. Rounds that pass delete
# their image captures (logs + stats kept).
#
# Mask split is the CALLER's job (PR 5 row: e.g. 5 rounds full mask +
# 5 rounds `taskset -c 0-15 …` — FIND-VS-B precedent); the daemon, storm,
# and remount all inherit the caller's affinity.
#
# Rails: unique sandbox (never /mnt/squeezefs, /mnt/juicefs, ~/tmp/nvme);
# kills by PID only; systemd-run --user scope cages the daemon; fresh
# mountpoint per round (armor against the Branch-3 stale-mountpoint harness
# hiccup); Tctl >= 88 C pauses before each round until cool.
#
# Usage: SANDBOX=~/tmp/smo_accept_$$ N=5 .agents/findvsa/smo_acceptance_soak.sh
set -u
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
BIN="${BIN:-$REPO/target/release/squeezefs}"
SANDBOX="${SANDBOX:-$HOME/tmp/smo_accept_$$}"
N="${N:-5}"
KILL_AFTER="${KILL_AFTER:-4}"
mkdir -p "$SANDBOX"
die() { echo "FATAL: $*" >&2; exit 9; }
[ -x "$BIN" ] || die "binary not built: $BIN"
meta_uri() { echo "sqmeta://$SANDBOX/meta1.img,$SANDBOX/meta2.img,$SANDBOX/meta3.img,$SANDBOX/meta4.img"; }

thermal_gate() {
    while :; do
        t=$(sensors 2>/dev/null | awk '/Tctl/ {gsub(/[+°C]/,"",$2); print int($2)}')
        [ -z "$t" ] && return 0
        [ "$t" -lt 88 ] && return 0
        echo "THERMAL: Tctl=${t}C >= 88C — pausing 30s"
        sleep 30
    done
}

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
    systemd-run --user --scope --unit "sqzacc-$1-$$-$RANDOM" -p MemoryMax=16G -p MemorySwapMax=0 --quiet \
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

# stats_field <stats-json> <metrics-key>  -> "v1,v2,v3,v4" (or scalar)
stats_field() {
    python3 - "$1" "$2" <<'EOF'
import json, sys
try:
    obj = json.load(open(sys.argv[1]))
    v = obj.get("metrics", {}).get(sys.argv[2])
    if isinstance(v, list):
        print(",".join(str(x) for x in v))
    else:
        print(v)
except Exception as e:
    print(f"PARSE-ERROR:{e}")
EOF
}

PASS=0
echo "=== SMO PR-5 acceptance soak: N=$N KILL_AFTER=$KILL_AFTER sandbox=$SANDBOX bin=$BIN"
echo "=== affinity: $(taskset -pc $$ 2>/dev/null || echo unknown)"
for round in $(seq 1 "$N"); do
    thermal_gate
    ART="$SANDBOX/round$round"
    MNT="$SANDBOX/mnt$round"          # fresh mountpoint per round
    mkdir -p "$ART" "$MNT"
    echo "=== round $round artifacts: $ART"
    do_format
    do_mount storm || die "storm mount round $round (harness, not adjudication)"
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
    ACKED=$(wc -l < "$ART/tree.acked")
    echo "acked: $ACKED"
    # page-cache-coherent copies BEFORE any remount (kept only on failure)
    CAP="$SANDBOX/capture_round$round"
    mkdir -p "$CAP"
    for i in 1 2 3 4; do
        cp --sparse=always "$SANDBOX/meta$i.img" "$CAP/meta$i.img"
        cp --sparse=always "$SANDBOX/data$i.img" "$CAP/data$i.img"
    done
    cp "$ART/tree.acked" "$CAP/tree.acked"

    # ---- in-place remount: the joint adjudication --------------------------
    if ! do_mount recheck; then
        echo "ROUND $round FAILED: REMOUNT REFUSED — joint assertion violated (mount-refusals == 0)"
        cp "$ART/mount_recheck.log" "$CAP/" 2>/dev/null || true
        echo "REFUSED" >"$CAP/verdict"
        echo "preserved: $CAP"
        pgrep -f "squeezefs mount sqmeta://$SANDBOX" | xargs -r kill -9 2>/dev/null
        exit 1
    fi
    sleep 2
    cp "$MNT/.stats" "$ART/stats.remount" 2>/dev/null || true
    find "$MNT/ktree" -type f 2>/dev/null | sort >"$ART/tree.postcrash"
    comm -23 "$ART/tree.acked" "$ART/tree.postcrash" >"$ART/missing.txt"
    MISS=$(wc -l < "$ART/missing.txt")
    DT=$(stats_field "$ART/stats.remount" meta_kv_replay_dropped_torn)
    RE=$(stats_field "$ART/stats.remount" meta_kv_replay_entries)
    PF0=$(stats_field "$ART/stats.remount" meta_kv_pending_free)
    # pending_free drain re-sample (Branch 3: drains to 0 at the first
    # post-mount durable checkpoints; cadence <= 1 s)
    PF1="$PF0"
    if [ "$PF0" != "0,0,0,0" ]; then
        sleep 3
        cp "$MNT/.stats" "$ART/stats.remount2" 2>/dev/null || true
        PF1=$(stats_field "$ART/stats.remount2" meta_kv_pending_free)
    fi
    teardown_mnt
    cp "$ART/stats.remount" "$ART/missing.txt" "$ART/mount_recheck.log" "$CAP/" 2>/dev/null || true

    TORN_OK=0
    [ "$DT" = "0,0,0,0" ] && TORN_OK=1
    echo "round $round: acked=$ACKED missing=$MISS dropped_torn=[$DT] replay_entries=[$RE] pending_free=[$PF0]->[$PF1]"
    if [ "$MISS" -eq 0 ] && [ "$TORN_OK" -eq 1 ]; then
        PASS=$((PASS + 1))
        echo "round $round: PASS (joint) [$PASS/$N]"
        echo "PASS" >"$CAP/verdict"
        rm -rf "$CAP"                 # images not needed for passing rounds
        rm -rf "$MNT"
    else
        echo "ROUND $round FAILED: missing=$MISS dropped_torn=[$DT] — joint assertion violated"
        echo "FAIL missing=$MISS dropped_torn=$DT" >"$CAP/verdict"
        echo "preserved: $CAP (pre-remount images) + $ART (logs/stats)"
        exit 1
    fi
done
echo "=== ACCEPTANCE $PASS/$N (all rounds jointly green)"
[ "$PASS" -eq "$N" ]
