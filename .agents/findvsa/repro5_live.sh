#!/bin/bash
# FIND-VS-A: is the acked-loss visible LIVE (no crash at all)?
set -u
REPO=/home/justin/Source/squeezefs
BIN="$REPO/target/release/squeezefs"
SB=/var/tmp/sqz_findvsa
MNT="$SB/mnt"
ART="$SB/art/$(date +%s)_live"
mkdir -p "$SB" "$MNT" "$ART"
die() { echo "FATAL: $*" >&2; exit 9; }
meta_uri() { echo "sqmeta://$SB/meta1.img,$SB/meta2.img,$SB/meta3.img,$SB/meta4.img"; }
rm -f "$SB"/meta{1,2,3,4}.img "$SB"/data{1,2,3,4}.img
rm -rf "$SB/staging"; mkdir -p "$SB/staging"
for i in 1 2 3 4; do truncate -s 1G "$SB/meta$i.img"; truncate -s 4G "$SB/data$i.img"; done
"$BIN" format "$(meta_uri)" "sqdata://$SB/data1.img,$SB/data2.img,$SB/data3.img,$SB/data4.img" \
    --disk-cache-paths "$SB/staging" --force >"$ART/format.log" 2>&1 || die format
logf="$ART/mount.log"
systemd-run --user --scope --unit "sqzvsa-live-$$-$RANDOM" -p MemoryMax=16G --quiet \
    "$BIN" mount "$(meta_uri)" "$MNT" --daemon --mem-budget 4096M --disk-cache-size 4096MB \
    --log-file "$logf" >>"$logf" 2>&1
for i in $(seq 1 200); do
    mountpoint -q "$MNT" && grep -q "transport armed" "$logf" 2>/dev/null && break; sleep 0.3
done
mountpoint -q "$MNT" || die mount
SQZ_PID="$(pgrep -f "squeezefs mount sqmeta://$SB" | head -1)"
RUNID="q$(date +%s | tail -c 6)"
python3 "$REPO/.agents/findvsa/storm_creator.py" "$MNT" "$ART" 16 8 1024 "$RUNID" >"$ART/storm.log" 2>&1
cat "$ART"/acked.* | sort >"$ART/tree.acked"
wc -l "$ART/tree.acked"
sleep 2
# LIVE stat check of every acked path (daemon alive, no crash)
python3 - "$ART/tree.acked" <<'EOF' | tee "$ART/live_missing"
import os,sys
missing=0
for line in open(sys.argv[1]):
    p=line.strip()
    try: os.stat(p)
    except FileNotFoundError:
        missing+=1
        if missing<=6: print("MISSING-LIVE:",p)
print("TOTAL-MISSING-LIVE:",missing)
EOF
"$BIN" umount "$MNT" >/dev/null 2>&1 </dev/null || fusermount3 -uz "$MNT" || true
for i in $(seq 1 100); do mountpoint -q "$MNT" || break; sleep 0.2; done
stat "$MNT" >/dev/null 2>&1 || fusermount3 -uz "$MNT" 2>/dev/null || true
pgrep -f "squeezefs mount sqmeta://$SB" | xargs -r kill -9
echo done: $ART