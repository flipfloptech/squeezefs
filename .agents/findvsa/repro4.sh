#!/bin/bash
# FIND-VS-A part 2 forensic: kill -9 at storm peak; copy the meta images
# (page-cache coherent) BEFORE any remount; then compare
#   loss A: in-place remount     (what the scoreboard saw)
#   loss B: remount of the copies in a second sandbox (what WAS knowable at crash)
# If A == B, the acked entries were never in the coherent image => ack-before-write.
set -u
KILL_AFTER="${KILL_AFTER:-4}"
REPO=/home/justin/Source/squeezefs
BIN="$REPO/target/release/squeezefs"
SB=/var/tmp/sqz_findvsa
SB2=/var/tmp/sqz_findvsa_copy
MNT="$SB/mnt"
MNT2="$SB2/mnt"
ART="$SB/art/$(date +%s)_forensic"
mkdir -p "$SB" "$MNT" "$SB2" "$MNT2" "$ART"
die() { echo "FATAL: $*" >&2; exit 9; }
meta_uri() { echo "sqmeta://$SB/meta1.img,$SB/meta2.img,$SB/meta3.img,$SB/meta4.img"; }
meta_uri2() { echo "sqmeta://$SB2/meta1.img,$SB2/meta2.img,$SB2/meta3.img,$SB2/meta4.img"; }

do_format() {
    rm -f "$SB"/meta{1,2,3,4}.img "$SB"/data{1,2,3,4}.img
    rm -rf "$SB/staging"; mkdir -p "$SB/staging"
    for i in 1 2 3 4; do truncate -s 1G "$SB/meta$i.img"; truncate -s 4G "$SB/data$i.img"; done
    "$BIN" format "$(meta_uri)" \
        "sqdata://$SB/data1.img,$SB/data2.img,$SB/data3.img,$SB/data4.img" \
        --disk-cache-paths "$SB/staging" --force >"$ART/format.log" 2>&1 || die format
}
do_mount() { # <tag> <uri> <mnt> <sbdir>
    local logf="$ART/mount_$1.log"
    systemd-run --user --scope --unit "sqzvsa-$1-$$-$RANDOM" -p MemoryMax=16G -p MemorySwapMax=0 --quiet \
        "$BIN" mount "$2" "$3" --daemon \
        --mem-budget 4096M --disk-cache-size 4096MB --log-file "$logf" >>"$logf" 2>&1
    for i in $(seq 1 100); do
        mountpoint -q "$3" && grep -q "transport armed for this session" "$logf" 2>/dev/null && break
        grep -q "mount refused\|Failed to start" "$logf" 2>/dev/null && break
        sleep 0.3
    done
    mountpoint -q "$3" || { tail -8 "$logf" >&2; return 1; }
    LAST_PID="$(pgrep -f "squeezefs mount $2" | head -1)"
    echo "mounted $1 pid=$LAST_PID"
}
teardown_mnt() { # <uri> <mnt>
    "$BIN" umount "$2" >>"$ART/umounts.log" 2>&1 </dev/null || fusermount3 -u "$2" 2>/dev/null || true
    for i in $(seq 1 100); do mountpoint -q "$2" || break; sleep 0.2; done
    stat "$2" >/dev/null 2>&1 || { fusermount3 -uz "$2" 2>/dev/null || umount -l "$2" 2>/dev/null || true; sleep 0.3; }
    pgrep -f "squeezefs mount $1" | xargs -r kill -9 2>/dev/null
    sleep 0.3
}

echo "=== KILL_AFTER=$KILL_AFTER artifacts: $ART"
do_format
do_mount storm "$(meta_uri)" "$MNT" "$SB" || die "storm mount"
SQZ_PID=$LAST_PID
# stats sampler (250 ms cadence) — the crashed daemon's last-known counters
(
    n=0
    while [ -d "/proc/$SQZ_PID" ]; do
        cp "$MNT/.stats" "$ART/stats.sample.$n" 2>/dev/null || true
        n=$((n + 1))
        sleep 0.25
    done
) &
SAMPLER=$!
RUNID="q$(date +%s | tail -c 6)"
echo "RUNID=$RUNID" | tee "$ART/runid"
python3 "$REPO/.agents/findvsa/storm_creator.py" "$MNT" "$ART" 16 8 1024 "$RUNID" >"$ART/storm.log" 2>&1 &
STORM_PID=$!
sleep "$KILL_AFTER"
kill -9 "$SQZ_PID"
T_KILL=$(date +%s.%N)
echo "killed daemon at $T_KILL"
wait $STORM_PID || true
kill $SAMPLER 2>/dev/null; wait $SAMPLER 2>/dev/null
fusermount3 -uz "$MNT" 2>/dev/null || umount -l "$MNT" 2>/dev/null || true
sleep 0.5
cat "$ART"/acked.* | sort >"$ART/tree.acked"
wc -l "$ART/tree.acked"
# page-cache-coherent copies BEFORE any remount
rm -f "$SB2"/meta{1,2,3,4}.img "$SB2"/data{1,2,3,4}.img
for i in 1 2 3 4; do
    cp "$SB/meta$i.img" "$SB2/meta$i.img"
    cp --reflink=always "$SB/data$i.img" "$SB2/data$i.img" 2>/dev/null || cp --sparse=always "$SB/data$i.img" "$SB2/data$i.img"
done
# loss A: in-place remount (tolerate refusal — that is finding FIND-VS-A2b)
if do_mount recheckA "$(meta_uri)" "$MNT" "$SB"; then
    sleep 2
    cp "$MNT/.stats" "$ART/stats.remountA" 2>/dev/null || true
    find "$MNT/ktree" -type f 2>/dev/null | sort >"$ART/tree.postA"
    comm -23 "$ART/tree.acked" "$ART/tree.postA" >"$ART/missing.A"
    echo "LOSS-A(in-place): $(wc -l < "$ART/missing.A")"
    teardown_mnt "$(meta_uri)" "$MNT"
else
    echo "LOSS-A: MOUNT REFUSED (see mount_recheckA.log)"
    : >"$ART/missing.A"
fi
# loss B: remount of the coherent copies (fresh page cache for those files)
if do_mount recheckB "$(meta_uri2)" "$MNT2" "$SB2"; then
    sleep 2
    cp "$MNT2/.stats" "$ART/stats.remountB" 2>/dev/null || true
    find "$MNT2/ktree" -type f 2>/dev/null | sed "s|$MNT2|$MNT|" | sort >"$ART/tree.postB"
    comm -23 "$ART/tree.acked" "$ART/tree.postB" >"$ART/missing.B"
    echo "LOSS-B(copies): $(wc -l < "$ART/missing.B")"
    teardown_mnt "$(meta_uri2)" "$MNT2"
else
    echo "LOSS-B: MOUNT REFUSED (see mount_recheckB.log)"
    : >"$ART/missing.B"
fi
diff <(sort "$ART/missing.A") <(sort "$ART/missing.B") >/dev/null && echo "A==B: identical missing sets" || echo "A!=B: sets differ"
# byte-search the coherent copies for missing vs present names
echo "--- image byte-search (missing sample vs present sample)"
MISS=$(head -1 "$ART/missing.A" | xargs -r basename)
PRES=$(comm -12 "$ART/tree.acked" "$ART/tree.postA" | tail -1 | xargs -r basename)
for name in "$MISS" "$PRES"; do
    [ -n "$name" ] || continue
    hits=0
    for i in 1 2 3 4; do
        c=$(grep -c --binary-files=text -o "$name" "$SB2/meta$i.img" 2>/dev/null || echo 0)
        hits=$((hits + c))
    done
    echo "name=$name image-hits=$hits"
done
# bulk: how many of the missing names appear anywhere in the copies?
found=0; total=0
while read -r p; do
    n=$(basename "$p"); total=$((total + 1))
    hit=0
    for i in 1 2 3 4; do
        if grep -q --binary-files=text "$n" "$SB2/meta$i.img" 2>/dev/null; then hit=1; break; fi
    done
    found=$((found + hit))
done < <(shuf -n 40 "$ART/missing.A" 2>/dev/null)
echo "missing-sample-in-images: $found/$total"
echo "=== done: $ART"
