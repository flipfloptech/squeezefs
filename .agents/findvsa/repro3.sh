#!/bin/bash
# FIND-VS-A part 2, discriminating experiment: kill -9 the daemon AT STORM
# PEAK (creates in flight, acked set = per-worker ack logs), remount, diff.
# KILL_AFTER=<seconds into the create storm>
set -u
KILL_AFTER="${KILL_AFTER:-4}"
REPO=/home/justin/Source/squeezefs
BIN="$REPO/target/release/squeezefs"
SB=/var/tmp/sqz_findvsa
MNT="$SB/mnt"
ART="$SB/art/$(date +%s)_kill9peak"
mkdir -p "$SB" "$MNT" "$ART"
die() { echo "FATAL: $*" >&2; exit 9; }
meta_uri() { echo "sqmeta://$SB/meta1.img,$SB/meta2.img,$SB/meta3.img,$SB/meta4.img"; }

do_format() {
    rm -f "$SB"/meta{1,2,3,4}.img "$SB"/data{1,2,3,4}.img
    rm -rf "$SB/staging"; mkdir -p "$SB/staging"
    for i in 1 2 3 4; do truncate -s 1G "$SB/meta$i.img"; truncate -s 4G "$SB/data$i.img"; done
    "$BIN" format "$(meta_uri)" \
        "sqdata://$SB/data1.img,$SB/data2.img,$SB/data3.img,$SB/data4.img" \
        --disk-cache-paths "$SB/staging" --force >"$ART/format.log" 2>&1 || die format
}
do_mount() {
    local logf="$ART/mount_$1.log"
    systemd-run --user --scope --unit "sqzvsa-$1-$$-$RANDOM" -p MemoryMax=16G -p MemorySwapMax=0 --quiet \
        "$BIN" mount "$(meta_uri)" "$MNT" --daemon \
        --mem-budget 4096M --disk-cache-size 4096MB --log-file "$logf" >>"$logf" 2>&1
    for i in $(seq 1 200); do
        mountpoint -q "$MNT" && grep -q "transport armed for this session" "$logf" 2>/dev/null && break
        sleep 0.3
    done
    mountpoint -q "$MNT" || { tail -5 "$logf" >&2; die mount; }
    SQZ_PID="$(pgrep -f "squeezefs mount sqmeta://$SB" | head -1)"
    echo "mounted pid=$SQZ_PID"
}

echo "=== KILL_AFTER=$KILL_AFTER artifacts: $ART"
do_format
do_mount storm
# create storm w/ ack logs; kill daemon mid-storm
python3 "$REPO/.agents/findvsa/storm_creator.py" "$MNT" "$ART" 16 8 1024 >"$ART/storm.log" 2>&1 &
STORM_PID=$!
sleep "$KILL_AFTER"
T_KILL=$(date +%s.%N)
kill -9 "$SQZ_PID"
echo "killed daemon at $T_KILL"
wait $STORM_PID || true
tail -3 "$ART/storm.log"
fusermount3 -uz "$MNT" 2>/dev/null || umount -l "$MNT" 2>/dev/null || true
sleep 0.5
cat "$ART"/acked.* | sort >"$ART/tree.acked"
wc -l "$ART/tree.acked"
# remount + diff
do_mount recheck
sleep 2
cp "$MNT/.stats" "$ART/stats.remount" 2>/dev/null || true
find "$MNT/ktree" -type f 2>/dev/null | sort >"$ART/tree.postcrash"
wc -l "$ART/tree.postcrash"
comm -23 "$ART/tree.acked" "$ART/tree.postcrash" >"$ART/tree.missing"
echo "MISSING-ACKED: $(wc -l < "$ART/tree.missing")"
awk -F/ '{print $(NF-2)"/"$(NF-1)}' "$ART/tree.missing" | sort | uniq -c | sort -rn | head -8
"$BIN" umount "$MNT" >"$ART/umount.recheck.log" 2>&1 </dev/null || fusermount3 -uz "$MNT" || true
for i in $(seq 1 100); do mountpoint -q "$MNT" || break; sleep 0.2; done
stat "$MNT" >/dev/null 2>&1 || { fusermount3 -uz "$MNT" 2>/dev/null || true; }
pgrep -f "squeezefs mount sqmeta://$SB" | xargs -r kill -9
echo "=== done: $ART"
