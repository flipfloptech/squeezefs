#!/bin/bash
# FIND-VS-A part 2: acked-create loss. Scoreboard-faithful order:
# dataset -> rand-write residue -> create storm -> IMMEDIATE teardown.
# MODE=umount (SIGBUS via the umount CLI) or MODE=kill9 (pure process death).
set -u
MODE="${MODE:-umount}"
REPO=/home/justin/Source/squeezefs
BIN="$REPO/target/release/squeezefs"
SB=/var/tmp/sqz_findvsa
MNT="$SB/mnt"
ART="$SB/art/$(date +%s)_$MODE"
mkdir -p "$SB" "$MNT" "$ART"
die() { echo "FATAL: $*" >&2; exit 9; }
meta_uri() { echo "sqmeta://$SB/meta1.img,$SB/meta2.img,$SB/meta3.img,$SB/meta4.img"; }

do_format() {
    rm -f "$SB"/meta{1,2,3,4}.img "$SB"/data{1,2,3,4}.img
    rm -rf "$SB/staging"; mkdir -p "$SB/staging"
    for i in 1 2 3 4; do
        truncate -s 1G "$SB/meta$i.img"; truncate -s 4G "$SB/data$i.img"
    done
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
do_umount() {
    "$BIN" umount "$MNT" >"$ART/umount.$1.log" 2>&1 </dev/null || fusermount3 -u "$MNT" 2>/dev/null || true
    for i in $(seq 1 150); do mountpoint -q "$MNT" || break; sleep 0.2; done
    stat "$MNT" >/dev/null 2>&1 || { fusermount3 -uz "$MNT" 2>/dev/null || umount -l "$MNT" 2>/dev/null || true; sleep 0.5; }
    for i in $(seq 1 300); do [ -n "${SQZ_PID:-}" ] && [ -d "/proc/$SQZ_PID" ] || break; sleep 0.2; done
    [ -n "${SQZ_PID:-}" ] && [ -d "/proc/$SQZ_PID" ] && { echo "SIGKILL leftover"; kill -9 "$SQZ_PID" || true; }
}

echo "=== MODE=$MODE artifacts: $ART"
do_format
do_mount storm
# 1. dataset
mkdir -p "$MNT/vsdata"; files=(); for i in $(seq 1 16); do files+=("$MNT/vsdata/f$i"); done
elbencho -w -t 16 -s 256m -b 1m --direct "${files[@]}" >"$ART/dataset.log" 2>&1 || die dataset
# 2. rand-write residue
elbencho -w --rand -t 16 -s 256m -b 4k --iodepth 16 --direct --timelimit 10 "${files[@]}" >"$ART/randwrite.log" 2>&1 || true
# 3. create storm LAST (the acked tail is fresh at teardown)
mkdir -p "$MNT/vstree"
elbencho -w -d -t 16 -n 8 -N 1024 -s 4k "$MNT/vstree" >"$ART/tree.log" 2>&1 || die tree
T_TREE_DONE=$(date +%s.%N)
# capture acked state fast (dir listing via readdir, not stat-heavy)
find "$MNT/vstree" -type f | sort >"$ART/tree.acked" &
FIND_PID=$!
cp "$MNT/.stats" "$ART/stats.pre_teardown" 2>/dev/null || true
wait $FIND_PID
wc -l "$ART/tree.acked"
T_TEARDOWN=$(date +%s.%N)
echo "tree-done->teardown gap: $(echo "$T_TEARDOWN - $T_TREE_DONE" | bc)s"
if [ "$MODE" = kill9 ]; then
    kill -9 "$SQZ_PID"; sleep 0.5
    fusermount3 -uz "$MNT" 2>/dev/null || umount -l "$MNT" 2>/dev/null || true
    sleep 0.5
else
    do_umount storm
fi
sleep 1
coredumpctl list --since "@${T_TEARDOWN%.*}" --no-pager 2>/dev/null | grep squeezefs | tee "$ART/coredump.line" || true
# forensic image copies BEFORE remount (reflink: cheap, physical-disk view is NOT what we
# want -- we want the page-cache-coherent view, which cp gives)
for i in 1 2 3 4; do cp --reflink=auto "$SB/meta$i.img" "$ART/meta$i.img.postcrash"; done
do_mount recheck
sleep 2
cp "$MNT/.stats" "$ART/stats.postcrash_remount" 2>/dev/null || true
find "$MNT/vstree" -type f | sort >"$ART/tree.postcrash"
wc -l "$ART/tree.postcrash"
comm -23 "$ART/tree.acked" "$ART/tree.postcrash" >"$ART/tree.missing"
echo "MISSING: $(wc -l < "$ART/tree.missing")"
awk -F/ '{print $(NF-1)}' "$ART/tree.missing" | sort | uniq -c | sort -rn | head -8
do_umount recheck
echo "=== done: $ART"
