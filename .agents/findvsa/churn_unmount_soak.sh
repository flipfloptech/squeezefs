#!/bin/bash
# FIND-M11-A-shape churn-unmount soak for FIND-VS-A acceptance:
# mount -> metadata churn (create/write/unlink storm) -> clean unmount, xN.
# Passes when every unmount is clean (no SIGBUS, daemon exits, no leftovers).
set -u
N="${1:-10}"
REPO="${SQZ_REPO:-/home/justin/Source/squeezefs}"
BIN="${SQZ_BIN:-$REPO/target/release/squeezefs}"
SB=/var/tmp/sqz_findvsa_soak
MNT="$SB/mnt"
ART="$SB/art_$(date +%s)"
mkdir -p "$SB" "$MNT" "$ART"
die() { echo "FATAL: $*" >&2; exit 9; }
meta_uri() { echo "sqmeta://$SB/meta1.img,$SB/meta2.img"; }

rm -f "$SB"/meta{1,2}.img "$SB"/data{1,2}.img
rm -rf "$SB/staging"; mkdir -p "$SB/staging"
truncate -s 1G "$SB/meta1.img"; truncate -s 1G "$SB/meta2.img"
truncate -s 4G "$SB/data1.img"; truncate -s 4G "$SB/data2.img"
"$BIN" format "$(meta_uri)" "sqdata://$SB/data1.img,$SB/data2.img" \
    --disk-cache-paths "$SB/staging" --force >"$ART/format.log" 2>&1 || die format

PASS=0
for i in $(seq 1 "$N"); do
    logf="$ART/mount_$i.log"
    T0=$(date +%s)
    "$BIN" mount "$(meta_uri)" "$MNT" --daemon --mem-budget 2048M \
        --disk-cache-size 1024MB --log-file "$logf" >>"$logf" 2>&1
    for k in $(seq 1 200); do
        mountpoint -q "$MNT" && grep -q "transport armed" "$logf" 2>/dev/null && break
        sleep 0.3
    done
    mountpoint -q "$MNT" || die "mount $i"
    PID="$(pgrep -f "squeezefs mount sqmeta://$SB" | head -1)"
    # churn: create/write/overwrite/unlink mix + partial staged blocks
    python3 - "$MNT" <<'EOF'
import os, sys, random
mnt = sys.argv[1]
random.seed()
base = f"{mnt}/churn"
os.makedirs(base, exist_ok=True)
# FIND-M11-A shape: metadata-heavy create/write/unlink churn (inline +
# staged small files). Multi-MiB writes are deliberately excluded — the
# 'did not settle after 8 binding rebinds' EIO they trip is the standing
# pre-existing rand_write family (baseline 4421f06 fails it at round 1;
# scoreboard Loss 2 charter), out of scope for the teardown soak.
names = []
for i in range(1500):
    p = f"{base}/f{i}"
    with open(p, "wb") as f:
        f.write(os.urandom(random.choice([4096, 16384, 65536])))
    names.append(p)
    if i % 3 == 0 and names:
        victim = names.pop(random.randrange(len(names)))
        os.unlink(victim)
EOF
    rc=$?
    [ $rc -eq 0 ] || die "churn $i"
    "$BIN" umount "$MNT" >"$ART/umount_$i.log" 2>&1 </dev/null ||
        fusermount3 -u "$MNT" 2>/dev/null || true
    for k in $(seq 1 300); do mountpoint -q "$MNT" || break; sleep 0.2; done
    mountpoint -q "$MNT" && die "unmount $i stuck"
    ok=1
    stat "$MNT" >/dev/null 2>&1 || { echo "round $i: ENOTCONN residue"; ok=0; fusermount3 -uz "$MNT" || true; }
    for k in $(seq 1 300); do [ -d "/proc/$PID" ] || break; sleep 0.2; done
    [ -d "/proc/$PID" ] && { echo "round $i: daemon still alive - SIGKILL"; kill -9 "$PID"; ok=0; }
    NEWSIG=$(coredumpctl list --since "@$T0" --no-pager 2>/dev/null | grep -c "squeezefs" || true)
    [ "${NEWSIG:-0}" -gt 0 ] && { echo "round $i: COREDUMP"; ok=0; }
    [ $ok -eq 1 ] && { PASS=$((PASS + 1)); echo "round $i: clean"; }
    rm -rf "$MNT/churn" 2>/dev/null || true
done
echo "SOAK $PASS/$N clean"
[ "$PASS" -eq "$N" ]
