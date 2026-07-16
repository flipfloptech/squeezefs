#!/bin/bash
# FIND-VS-A repro: storm + immediate `squeezefs umount` teardown SIGBUS.
# Full CPU mask (NO taskset — FIND-VS-B precedent). Unique sandbox.
set -u
REPO=/home/justin/Source/squeezefs
BIN="$REPO/target/release/squeezefs"
SB=/var/tmp/sqz_findvsa
MNT="$SB/mnt"
ART="$SB/art/$(date +%s)"
mkdir -p "$SB" "$MNT" "$ART"

die() { echo "FATAL: $*" >&2; exit 9; }

meta_uri() { echo "sqmeta://$SB/meta1.img,$SB/meta2.img,$SB/meta3.img,$SB/meta4.img"; }

do_format() {
    rm -f "$SB"/meta{1,2,3,4}.img "$SB"/data{1,2,3,4}.img
    rm -rf "$SB/staging"
    mkdir -p "$SB/staging"
    for i in 1 2 3 4; do
        truncate -s 1G "$SB/meta$i.img" || die "truncate meta$i"
        truncate -s 4G "$SB/data$i.img" || die "truncate data$i"
    done
    "$BIN" format "$(meta_uri)" \
        "sqdata://$SB/data1.img,$SB/data2.img,$SB/data3.img,$SB/data4.img" \
        --disk-cache-paths "$SB/staging" --force >"$ART/format.log" 2>&1 || die "format"
}

do_mount() { # <tag>
    local tag="$1" logf="$ART/mount_$1.log"
    systemd-run --user --scope --unit "sqzvsa-$tag-$$-$RANDOM" -p MemoryMax=16G -p MemorySwapMax=0 --quiet \
        "$BIN" mount "$(meta_uri)" "$MNT" --daemon \
        --mem-budget 4096M --disk-cache-size 4096MB \
        --log-file "$logf" >>"$logf" 2>&1
    for i in $(seq 1 200); do
        mountpoint -q "$MNT" && grep -q "transport armed for this session" "$logf" 2>/dev/null && break
        sleep 0.3
    done
    mountpoint -q "$MNT" || { tail -5 "$logf" >&2; die "mount failed"; }
    SQZ_PID="$(pgrep -f "squeezefs mount sqmeta://$SB" | head -1)"
    echo "mounted pid=$SQZ_PID"
}

storm() {
    # metadata storm FIRST: 16 threads x 8 dirs x 1024 files x 4KiB = 131072 inline files
    mkdir -p "$MNT/vstree"
    elbencho -w -d -t 16 -n 8 -N 1024 -s 4k "$MNT/vstree" >"$ART/tree.log" 2>&1 || die "tree storm"
    # staged/striped content (populates block maps)
    mkdir -p "$MNT/vsdata"
    local files=()
    for i in $(seq 1 16); do files+=("$MNT/vsdata/f$i"); done
    elbencho -w -t 16 -s 256m -b 1m --direct "${files[@]}" >"$ART/dataset.log" 2>&1 || die "dataset storm"
    # rand-write LAST: partial 4 MiB active blocks linger staged at high ring
    # offsets — the drain content the teardown SIGBUS needs (scoreboard shape)
    elbencho -w --rand -t 16 -s 256m -b 4k --iodepth 16 --direct --timelimit 10 \
        "${files[@]}" >"$ART/randwrite.log" 2>&1 || true
    grep -o '"active_writes_count":[0-9]*\|"staged_writes_in_flight":[0-9]*' "$MNT/.stats" 2>/dev/null | head -2 || true
    python3 - <<'EOF' || true
import json
try:
    d=json.load(open("/var/tmp/sqz_findvsa/mnt/.stats"))
    aw=d.get("active_writes",{})
    print("active_writes inodes:",len(aw),"blocks:",sum(len(v) for v in aw.values()))
except Exception as e:
    print("stats read:",e)
EOF
}

snapshot_segments() { # <label>
    stat -c '%n %s' "$SB"/staging/squeezefs/*/staging_segment/segment_*.bin >"$ART/segsize.$1" 2>&1 || true
}

do_umount() { # <label>
    local label="${1:-x}"
    "$BIN" umount "$MNT" >"$ART/umount.$label.log" 2>&1 </dev/null ||
        fusermount3 -u "$MNT" 2>/dev/null || true
    for i in $(seq 1 150); do mountpoint -q "$MNT" || break; sleep 0.2; done
    if ! stat "$MNT" >/dev/null 2>&1; then
        echo "WARN: ENOTCONN mountpoint — lazy detach"
        fusermount3 -uz "$MNT" 2>/dev/null || umount -l "$MNT" 2>/dev/null || true
        sleep 0.5
    fi
    for i in $(seq 1 300); do
        [ -n "${SQZ_PID:-}" ] && [ -d "/proc/$SQZ_PID" ] || break
        sleep 0.2
    done
    if [ -n "${SQZ_PID:-}" ] && [ -d "/proc/$SQZ_PID" ]; then
        echo "WARN: daemon $SQZ_PID alive after unmount — SIGKILL"
        kill -9 "$SQZ_PID" 2>/dev/null || true
    fi
}

echo "=== artifacts: $ART"
do_format
do_mount storm
storm
snapshot_segments before_umount
find "$MNT/vstree" -type f | sort >"$ART/tree.acked"  # what the fs served pre-unmount
wc -l "$ART/tree.acked"
UMOUNT_T0=$(date +%s.%N)
do_umount storm
UMOUNT_T1=$(date +%s.%N)
snapshot_segments after_umount
echo "umount window: $UMOUNT_T0 .. $UMOUNT_T1"
# crash check
sleep 1
coredumpctl list --since "@${UMOUNT_T0%.*}" --no-pager 2>/dev/null | grep squeezefs | tee "$ART/coredump.line"
if [ -s "$ART/coredump.line" ]; then echo "SIGBUS-REPRO: YES"; else echo "SIGBUS-REPRO: NO"; fi

# journal forensics BEFORE remount: raw meta images copy
for i in 1 2 3 4; do cp --reflink=auto "$SB/meta$i.img" "$ART/meta$i.img.postcrash"; done

# remount + count the tree
do_mount recheck
sleep 2
find "$MNT/vstree" -type f | sort >"$ART/tree.postcrash"
wc -l "$ART/tree.postcrash"
comm -23 "$ART/tree.acked" "$ART/tree.postcrash" >"$ART/tree.missing"
echo "missing files: $(wc -l < "$ART/tree.missing")"
head -5 "$ART/tree.missing"
do_umount recheck
echo "=== done: $ART"
