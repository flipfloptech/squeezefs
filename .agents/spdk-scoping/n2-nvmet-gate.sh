#!/usr/bin/env bash
# [SUPERSEDED 2026-07-18, PR 5/N5] standing coverage now tests/run_nvmeof_fidelity.sh (round-trip + loud-fail + crash-window nvmet legs) — kept as evidence lineage; do not extend.
# PR 2 (N2) root-tier gate — rebuilt kernel-nvmet path via PRODUCT VERBS
# (docs/design-nvmeof-target-management.md, PR-plan PR 2 gate row):
#   share (file + block backing) -> connect -> IO -> unshare -> ZERO residue
#   + missing-backing / unledgered-unshare / --nsid refusal legs
#   + foreign-port-squat probe-past leg (foreign object untouched)
#   + restore replay leg (same device_uuid re-presented; data intact)
#   + live coexistence with tests/dev_substrate.sh create (both up, neither
#     disturbed, both torn down clean)
#
# Ownership conventions (dev_substrate/spdkscope style): every object this
# script creates carries the n2gate marker or is recorded by exact id in
# $STATE/manifest; teardown removes ONLY manifest entries + product-ledger
# shares. NEVER touches: foreign nvmet trees, zram0, user mounts, the live
# /etc/squeezefs/nvmeof_shares.json (the product's retire hook skips it
# under a relocated SQUEEZEFS_NVMEOF_STATE_DIR — asserted below).
# Rig arm until PR 5's tests/ harness supersedes it.
set -uo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
BIN="$REPO/target/release/squeezefs"
STATE=/tmp/sqz-n2gate
LEDGER_DIR="$STATE/ledger"
NVMET_CFS=/sys/kernel/config/nvmet
NQN_FILE="nqn.2026-07.io.squeezefs:share-n2gate-file"
NQN_BLOCK="nqn.2026-07.io.squeezefs:share-n2gate-block"
NQN_SQUAT="nqn.2026-07.io.squeezefs:share-n2gate-squat"
PORT_MAIN=4462          # fnv1a(tcp:127.0.0.1:4462) -> id 53079
PORT_SQUAT=4463         # fnv1a(tcp:127.0.0.1:4463) -> id 53168
EXPECT_ID_MAIN=53079
EXPECT_ID_SQUAT=53168
BACKING_FILE="$STATE/backing.img"

export SQUEEZEFS_NVMEOF_STATE_DIR="$LEDGER_DIR"

PASS=0; FAIL=0
log()  { echo "[n2gate $(date +%H:%M:%S)] $*"; }
ok()   { PASS=$((PASS+1)); log "PASS: $*"; }
bad()  { FAIL=$((FAIL+1)); log "FAIL: $*"; }
die()  { log "FATAL: $*"; exit 1; }
manifest() { echo "$1" >> "$STATE/manifest"; }

[ "$(id -u)" = 0 ] || die "run as root"
[ -x "$BIN" ] || die "build first: cargo build --release"
command -v nvme >/dev/null || die "nvme-cli required"
command -v jq >/dev/null || die "jq required"
mkdir -p "$STATE" "$LEDGER_DIR"
: > "$STATE/manifest"

ETC_REG_MD5="$(md5sum /etc/squeezefs/nvmeof_shares.json 2>/dev/null || echo absent)"

snapshot_sections() { # label -> writes $STATE/snap-<label>.txt (stable sections only)
    local out="$STATE/snap-$1.txt"
    {
        echo "--- nvmet subsystems ---"
        ls -1 "$NVMET_CFS/subsystems/" 2>/dev/null | sort
        echo "--- nvmet ports ---"
        ls -1 "$NVMET_CFS/ports/" 2>/dev/null | sort
        echo "--- zram ---"
        ls -1 /dev/zram* 2>/dev/null | sort
        echo "--- loop over our backing ---"
        losetup -j "$BACKING_FILE" 2>/dev/null
        echo "--- nvme fabric controllers (n2gate) ---"
        for c in /sys/class/nvme/nvme*; do
            [ -e "$c/subsysnqn" ] || continue
            grep -q n2gate "$c/subsysnqn" 2>/dev/null && basename "$c"
        done
        echo "--- devsub state dir ---"
        ls /run/squeezefs-devsub 2>/dev/null || echo "(absent)"
    } > "$out" 2>&1
    echo "$out"
}

mkzram() { # size-bytes label -> echoes /dev/zramN
    local size=$1 label=$2 idx
    idx=$(cat /sys/class/zram-control/hot_add)
    [ "$idx" != "0" ] || die "hot_add returned zram0 (user swap slot!)"
    echo "$size" > "/sys/block/zram$idx/disksize"
    manifest "zram=$idx label=$label"
    echo "/dev/zram$idx"
}

finddev() { # nqn -> /dev/nvmeXn1 head node
    local nqn=$1 c cname
    for _ in $(seq 1 40); do
        for c in /sys/class/nvme/nvme*; do
            [ -e "$c/subsysnqn" ] || continue
            if [ "$(cat "$c/subsysnqn")" = "$nqn" ]; then
                cname=$(basename "$c")
                [ -b "/dev/${cname}n1" ] && { echo "/dev/${cname}n1"; return 0; }
            fi
        done
        sleep 0.25
    done
    return 1
}

manual_wipe_subsystem() { # nqn  (the documented removal-first runbook, ours only)
    local nqn=$1 p
    case "$nqn" in *n2gate*) ;; *) die "manual_wipe refused: $nqn is not ours" ;; esac
    for p in "$NVMET_CFS"/ports/*/subsystems/"$nqn"; do
        [ -L "$p" ] && rm "$p"
    done
    if [ -d "$NVMET_CFS/subsystems/$nqn" ]; then
        echo 0 > "$NVMET_CFS/subsystems/$nqn/namespaces/1/enable" 2>/dev/null
        rmdir "$NVMET_CFS/subsystems/$nqn/namespaces/1" 2>/dev/null
        rmdir "$NVMET_CFS/subsystems/$nqn" 2>/dev/null
    fi
}

teardown() {
    log "teardown (manifest-scoped)"
    for nqn in "$NQN_FILE" "$NQN_BLOCK" "$NQN_SQUAT"; do
        nvme disconnect -n "$nqn" >/dev/null 2>&1
    done
    sleep 1
    # Product-first teardown; manual sweep as fallback.
    for nqn in "$NQN_FILE" "$NQN_BLOCK" "$NQN_SQUAT"; do
        "$BIN" nvmeof unshare "$nqn" >/dev/null 2>&1
        manual_wipe_subsystem "$nqn" 2>/dev/null
    done
    # Our reserved-range ports, if leaked and link-free.
    for id in $EXPECT_ID_MAIN $((EXPECT_ID_MAIN+1)) $EXPECT_ID_SQUAT $((EXPECT_ID_SQUAT+1)); do
        local_p="$NVMET_CFS/ports/$id"
        if [ -d "$local_p" ] && [ -z "$(ls -A "$local_p/subsystems" 2>/dev/null)" ]; then
            rmdir "$local_p" 2>/dev/null
        fi
    done
    # The squat fixture port (manifest-recorded).
    grep '^squat_port=' "$STATE/manifest" 2>/dev/null | cut -d= -f2 | while read -r id; do
        P="$NVMET_CFS/ports/$id"
        [ -d "$P" ] && { rm -f "$P"/subsystems/* 2>/dev/null; rmdir "$P" 2>/dev/null; }
    done
    # Loop over our backing file.
    losetup -j "$BACKING_FILE" 2>/dev/null | cut -d: -f1 | while read -r lo; do
        losetup -d "$lo" 2>/dev/null
    done
    # Our zram devices (manifest-recorded; never index 0).
    grep '^zram=' "$STATE/manifest" 2>/dev/null | sed 's/^zram=//' | awk '{print $1}' | while read -r idx; do
        [ "$idx" != "0" ] || continue
        [ -b "/dev/zram$idx" ] || continue
        echo 1 > "/sys/block/zram$idx/reset" 2>/dev/null
        echo "$idx" > /sys/class/zram-control/hot_remove 2>/dev/null
    done
}
trap teardown EXIT

# ---------------------------------------------------------------------------
log "=== leg 0: snapshot before ==="
SNAP_BEFORE=$(snapshot_sections before)
log "wrote $SNAP_BEFORE"

# ---------------------------------------------------------------------------
log "=== leg 1: refusal legs (loud-fail UX on a real box) ==="
OUT=$("$BIN" nvmeof share "$STATE/definitely-missing.img" --ip 127.0.0.1 --port $PORT_MAIN --target-stack nvmet 2>&1)
if [ $? -ne 0 ] && echo "$OUT" | grep -q "does not exist" && echo "$OUT" | grep -q -- "--create-size"; then
    ok "missing backing refuses loud, names --create-size"
else bad "missing-backing refusal: $OUT"; fi
[ ! -e "$STATE/definitely-missing.img" ] && ok "refusal conjured no file" || bad "refusal created the file"

OUT=$("$BIN" nvmeof unshare nqn.2026-07.io.squeezefs:share-n2gate-ghost 2>&1)
if [ $? -ne 0 ] && echo "$OUT" | grep -q "not in the share ledger"; then
    ok "unledgered unshare refuses (ownership = ledger membership)"
else bad "unledgered unshare: $OUT"; fi

OUT=$("$BIN" nvmeof share /dev/null --ip 127.0.0.1 --target-stack nvmet --nsid 2 2>&1)
if [ $? -ne 0 ] && echo "$OUT" | grep -q "structurally fixed at 1"; then
    ok "--nsid 2 with nvmet refuses loud"
else bad "--nsid refusal: $OUT"; fi

# N3 updated the remediation text: sharing lands with N4, the target
# lifecycle verbs are live (docs/design-nvmeof-target-management.md PR 3).
OUT=$("$BIN" nvmeof share /dev/null --ip 127.0.0.1 2>&1)
if [ $? -ne 0 ] && echo "$OUT" | grep -q "SPDK share management lands with milestone N4"; then
    ok "default (spdk) stack fails loud with the milestone message"
else bad "spdk loud-fail: $OUT"; fi

# ---------------------------------------------------------------------------
log "=== leg 2: share file backing (--create-size) + block backing (zram) ==="
OUT=$("$BIN" nvmeof share "$BACKING_FILE" --create-size 1G --ip 127.0.0.1 --port $PORT_MAIN \
      --subnqn "$NQN_FILE" --target-stack nvmet 2>&1) || die "file share failed: $OUT"
echo "$OUT" | grep -q "detection-grade\| DETECTION-grade" && ok "loop guarantee-class note printed" \
    || bad "loop note missing: $OUT"
[ -f "$BACKING_FILE" ] && ok "sparse backing created via explicit opt-in" || bad "backing missing"

ZR_BLOCK=$(mkzram $((2*1024*1024*1024)) n2gate-block)
OUT=$("$BIN" nvmeof share "$ZR_BLOCK" --ip 127.0.0.1 --port $PORT_MAIN \
      --subnqn "$NQN_BLOCK" --target-stack nvmet 2>&1) || die "block share failed: $OUT"

for nqn in "$NQN_FILE" "$NQN_BLOCK"; do
    [ -d "$NVMET_CFS/subsystems/$nqn" ] && ok "subsystem live: $nqn" || bad "subsystem missing: $nqn"
done
if [ -d "$NVMET_CFS/ports/$EXPECT_ID_MAIN" ]; then
    LINKS=$(ls "$NVMET_CFS/ports/$EXPECT_ID_MAIN/subsystems" | wc -l)
    [ "$LINKS" = 2 ] && ok "deterministic port id $EXPECT_ID_MAIN shared by both listeners" \
        || bad "port $EXPECT_ID_MAIN links=$LINKS (want 2)"
else bad "expected port id $EXPECT_ID_MAIN absent"; fi
RESV=$(cat "$NVMET_CFS/subsystems/$NQN_BLOCK/namespaces/1/resv_enable" 2>/dev/null)
[ "$RESV" = 1 ] && ok "resv_enable=1 on block share (enforcement-grade PR)" || bad "resv_enable=$RESV"
UUID_BLOCK_CFS=$(cat "$NVMET_CFS/subsystems/$NQN_BLOCK/namespaces/1/device_uuid")
UUID_BLOCK_LEDGER=$(jq -r ".shares[] | select(.subnqn==\"$NQN_BLOCK\") | .ns_uuid" "$LEDGER_DIR/shares.json")
[ "$UUID_BLOCK_CFS" = "$UUID_BLOCK_LEDGER" ] && ok "device_uuid == ledger ns_uuid ($UUID_BLOCK_CFS)" \
    || bad "uuid mismatch cfs=$UUID_BLOCK_CFS ledger=$UUID_BLOCK_LEDGER"

# ---------------------------------------------------------------------------
log "=== leg 3: connect -> IO -> verify ==="
"$BIN" nvmeof connect --ip 127.0.0.1 --port $PORT_MAIN --subnqn "$NQN_FILE" >/dev/null 2>&1
"$BIN" nvmeof connect --ip 127.0.0.1 --port $PORT_MAIN --subnqn "$NQN_BLOCK" >/dev/null 2>&1
DEV_FILE=$(finddev "$NQN_FILE")   || die "no device for $NQN_FILE"
DEV_BLOCK=$(finddev "$NQN_BLOCK") || die "no device for $NQN_BLOCK"
log "devices: file=$DEV_FILE block=$DEV_BLOCK"

dd if=/dev/urandom of="$STATE/io-src" bs=1M count=64 status=none
MD5_SRC=$(md5sum "$STATE/io-src" | awk '{print $1}')
dd if="$STATE/io-src" of="$DEV_FILE" bs=1M oflag=direct status=none  || bad "write to file-backed dev"
dd if="$STATE/io-src" of="$DEV_BLOCK" bs=1M oflag=direct status=none || bad "write to block-backed dev"
MD5_F=$(dd if="$DEV_FILE" bs=1M count=64 iflag=direct status=none | md5sum | awk '{print $1}')
MD5_B=$(dd if="$DEV_BLOCK" bs=1M count=64 iflag=direct status=none | md5sum | awk '{print $1}')
[ "$MD5_F" = "$MD5_SRC" ] && ok "64 MiB O_DIRECT round-trip on file-backed share" || bad "file IO md5"
[ "$MD5_B" = "$MD5_SRC" ] && ok "64 MiB O_DIRECT round-trip on block-backed share" || bad "block IO md5"

# ---------------------------------------------------------------------------
log "=== leg 4: foreign-port squat -> probe past, never touch ==="
SQUAT="$NVMET_CFS/ports/$EXPECT_ID_SQUAT"
mkdir "$SQUAT" || die "cannot plant squat port"
manifest "squat_port=$EXPECT_ID_SQUAT"
echo tcp        > "$SQUAT/addr_trtype"
echo ipv4       > "$SQUAT/addr_adrfam"
echo 10.99.99.9 > "$SQUAT/addr_traddr"
echo 9999       > "$SQUAT/addr_trsvcid"
ZR_SQUAT=$(mkzram $((512*1024*1024)) n2gate-squat)
OUT=$("$BIN" nvmeof share "$ZR_SQUAT" --ip 127.0.0.1 --port $PORT_SQUAT \
      --subnqn "$NQN_SQUAT" --target-stack nvmet 2>&1) || die "squat-leg share failed: $OUT"
if [ -d "$NVMET_CFS/ports/$((EXPECT_ID_SQUAT+1))/subsystems/$NQN_SQUAT" ]; then
    ok "allocator probed past the squatted id to $((EXPECT_ID_SQUAT+1))"
else bad "squat probe: expected id $((EXPECT_ID_SQUAT+1))"; fi
[ "$(cat "$SQUAT/addr_traddr")" = "10.99.99.9" ] && ok "foreign port untouched" || bad "foreign port modified!"
"$BIN" nvmeof unshare "$NQN_SQUAT" >/dev/null 2>&1 && ok "squat-leg unshare" || bad "squat-leg unshare"
[ -d "$SQUAT" ] && ok "foreign port survived our unshare" || bad "our unshare removed the foreign port!"
[ ! -d "$NVMET_CFS/ports/$((EXPECT_ID_SQUAT+1))" ] && ok "our squat-leg port removed (link-free last-out)" \
    || bad "our squat-leg port leaked"
rm -f "$SQUAT"/subsystems/* 2>/dev/null; rmdir "$SQUAT" && log "squat fixture removed"

# ---------------------------------------------------------------------------
log "=== leg 5: restore replay (same identity re-presented; data intact) ==="
nvme disconnect -n "$NQN_FILE" >/dev/null; nvme disconnect -n "$NQN_BLOCK" >/dev/null; sleep 1
manual_wipe_subsystem "$NQN_FILE"; manual_wipe_subsystem "$NQN_BLOCK"
[ ! -d "$NVMET_CFS/subsystems/$NQN_BLOCK" ] || die "manual wipe failed"
OUT=$("$BIN" nvmeof restore --target-stack nvmet 2>&1) || die "restore failed: $OUT"
echo "$OUT" | grep -q "restored" && ok "restore replayed the wiped shares" || bad "restore output: $OUT"
UUID_AFTER=$(cat "$NVMET_CFS/subsystems/$NQN_BLOCK/namespaces/1/device_uuid" 2>/dev/null)
[ "$UUID_AFTER" = "$UUID_BLOCK_CFS" ] && ok "device_uuid re-presented verbatim ($UUID_AFTER)" \
    || bad "uuid changed across restore: $UUID_AFTER != $UUID_BLOCK_CFS"
OUT=$("$BIN" nvmeof restore --target-stack nvmet 2>&1) || bad "second restore errored: $OUT"
echo "$OUT" | grep -q "verified no-op" && ok "second restore is a verified no-op (idempotent)" \
    || bad "idempotent restore output: $OUT"
"$BIN" nvmeof connect --ip 127.0.0.1 --port $PORT_MAIN --subnqn "$NQN_BLOCK" >/dev/null 2>&1
DEV_BLOCK=$(finddev "$NQN_BLOCK") || die "no device post-restore"
MD5_B2=$(dd if="$DEV_BLOCK" bs=1M count=64 iflag=direct status=none | md5sum | awk '{print $1}')
[ "$MD5_B2" = "$MD5_SRC" ] && ok "pre-restore data readable after restore (same backing)" || bad "post-restore md5"

# ---------------------------------------------------------------------------
log "=== leg 6: live coexistence with dev_substrate.sh create ==="
DEVSUB_WAS_UP=0
[ -s /run/squeezefs-devsub/manifest ] && DEVSUB_WAS_UP=1
if [ "$DEVSUB_WAS_UP" = 0 ]; then
    "$REPO/tests/dev_substrate.sh" create >/dev/null 2>&1 || bad "dev_substrate create failed"
fi
if [ -d "$NVMET_CFS/ports/52026" ]; then
    ok "devsub port 52026 up beside our reserved-range ports"
else bad "devsub port 52026 missing"; fi
[ -d "$NVMET_CFS/subsystems/$NQN_BLOCK" ] && ok "our share undisturbed by devsub create" \
    || bad "our share vanished under devsub create"
DEVSUB_SUBS=$(ls -1 "$NVMET_CFS/subsystems/" | grep -c devsub)
[ "$DEVSUB_SUBS" -ge 1 ] && ok "devsub subsystems live ($DEVSUB_SUBS)" || bad "no devsub subsystems"
"$BIN" nvmeof list >/dev/null 2>&1 && ok "list runs beside devsub (foreign shown, never touched)" \
    || bad "list failed beside devsub"
LIST_JSON=$("$BIN" nvmeof list --json 2>/dev/null)
echo "$LIST_JSON" | jq -e '.foreign_live[] | select(.subnqn | contains("devsub"))' >/dev/null \
    && ok "devsub subsystems classified foreign/unmanaged" || bad "devsub not in foreign_live"
if [ "$DEVSUB_WAS_UP" = 0 ]; then
    "$REPO/tests/dev_substrate.sh" teardown >/dev/null 2>&1 && ok "dev_substrate teardown clean" \
        || bad "dev_substrate teardown failed"
    [ -d "$NVMET_CFS/subsystems/$NQN_BLOCK" ] && ok "our share undisturbed by devsub teardown" \
        || bad "our share vanished under devsub destroy"
fi

# ---------------------------------------------------------------------------
log "=== leg 7: unshare -> zero residue ==="
nvme disconnect -n "$NQN_BLOCK" >/dev/null 2>&1; sleep 1
"$BIN" nvmeof unshare "$NQN_FILE"  >/dev/null 2>&1 && ok "unshare file share"  || bad "unshare file"
"$BIN" nvmeof unshare "$NQN_BLOCK" >/dev/null 2>&1 && ok "unshare block share" || bad "unshare block"
LEDGER_LEFT=$(jq '.shares | length' "$LEDGER_DIR/shares.json" 2>/dev/null || echo 0)
[ "$LEDGER_LEFT" = 0 ] && ok "ledger empty after teardown" || bad "ledger records left: $LEDGER_LEFT"
[ -z "$(losetup -j "$BACKING_FILE" 2>/dev/null)" ] && ok "loop detached" || bad "loop leaked"
for nqn in "$NQN_FILE" "$NQN_BLOCK"; do
    [ ! -d "$NVMET_CFS/subsystems/$nqn" ] && ok "subsystem gone: $nqn" || bad "subsystem residue: $nqn"
done
[ ! -d "$NVMET_CFS/ports/$EXPECT_ID_MAIN" ] && ok "port $EXPECT_ID_MAIN removed (link-free)" \
    || bad "port $EXPECT_ID_MAIN residue"

# The live host registry stayed untouched (relocated-state-dir seam).
ETC_REG_MD5_AFTER="$(md5sum /etc/squeezefs/nvmeof_shares.json 2>/dev/null || echo absent)"
[ "$ETC_REG_MD5" = "$ETC_REG_MD5_AFTER" ] && ok "/etc/squeezefs/nvmeof_shares.json untouched" \
    || bad "/etc registry mutated: $ETC_REG_MD5 -> $ETC_REG_MD5_AFTER"

# Remove our zram fixtures BEFORE the final snapshot so the diff is clean.
grep '^zram=' "$STATE/manifest" | sed 's/^zram=//' | awk '{print $1}' | while read -r idx; do
    [ "$idx" != "0" ] || continue
    [ -b "/dev/zram$idx" ] || continue
    echo 1 > "/sys/block/zram$idx/reset" 2>/dev/null
    echo "$idx" > /sys/class/zram-control/hot_remove 2>/dev/null && log "removed zram$idx"
done
sed -i '/^zram=/d' "$STATE/manifest"

log "=== leg 8: snapshot after + residue diff ==="
SNAP_AFTER=$(snapshot_sections after)
if diff -u "$SNAP_BEFORE" "$SNAP_AFTER" > "$STATE/residue.diff"; then
    ok "ZERO residue: before/after snapshots identical"
else
    bad "residue detected:"; cat "$STATE/residue.diff"
fi

log "=== n2gate result: PASS=$PASS FAIL=$FAIL ==="
[ "$FAIL" = 0 ] || exit 1
exit 0
