#!/usr/bin/env bash
# PR 4b (N4b) root-tier gate — `nvmeof adopt` foreign-share absorption
# (docs/design-nvmeof-target-management.md §6.10, PR-plan PR 4b gate row),
# the two §6.10 pt-5 fidelity legs + the harness-owned refusal live:
#
#   leg 1: hand-built PRE-REBUILD-STYLE configfs share (small-int port id,
#          namespaces/1, no ledger) -> connect + IO -> adopt WHILE SERVING
#          (zero-interruption; configfs shape+mtime snapshot identical
#          across the adopt = the zero-target-mutation proof) -> list
#          managed with adopted_from provenance -> re-adopt refuses
#          already_ledgered -> restore verified-no-op -> disconnect ->
#          unshare clean INCLUDING the out-of-range port-id removal
#          (link-free law).
#   leg 2: ADOPT-AFTER-SIMULATED-LEDGER-LOSS on SPDK: product share ->
#          delete shares.json -> list shows foreign -> adopt (class
#          ledger-loss; live rpc.py inventory identical across the adopt;
#          ptpl re-bound from the surviving state-dir file; save_config
#          truth capture) -> restore verified-no-op (config untouched) ->
#          unshare -> zero target residue.
#   leg 3: HARNESS-OWNED refusal live against tests/dev_substrate.sh:
#          adopt of a devsub NQN refuses by name (adopt_harness_owned),
#          the substrate is observed, never absorbed, never disturbed
#          (subtree snapshot identical; teardown only if this gate
#          created it).
#   leg 4: teardown -> zero residue (before/after snapshot diff empty).
#
# Ownership conventions (dev_substrate/spdkscope style): every object this
# script creates carries the n4bgate marker or is recorded by exact id in
# $STATE/manifest; teardown removes ONLY manifest entries. NEVER touches:
# foreign nvmet trees, zram0, user mounts (/mnt/squeezefs, /mnt/juicefs),
# ~/tmp/nvme, /var/tmp/spdk-scoping (READ-ONLY here: its rpc.py drives
# live-inventory reads against OUR socket), or the live /etc/squeezefs
# registry. /opt/squeezefs is reused when a verified pinned install
# pre-exists and removed at teardown ONLY when this gate created it.
# Rig arm until PR 5's tests/ harness supersedes it.
set -uo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
BIN="$REPO/target/release/squeezefs"
STATE=/tmp/sqz-n4bgate
LEDGER_DIR="$STATE/state"          # SQUEEZEFS_NVMEOF_STATE_DIR (relocated)
RUN_DIR="$STATE/run"               # SQUEEZEFS_NVMEOF_RUN_DIR (relocated)
PREFIX=/opt/squeezefs/spdk/v26.05
PIN_SHA=d519b163cbc0e2f28c35d9bc86d610da368b032c
RPCPY="/var/tmp/spdk-scoping/spdk/scripts/rpc.py -s $RUN_DIR/spdk.sock"
NQN_PRE="nqn.2026-06.io.squeezefs:subsystem-n4bgate"     # pre-rebuild shape
NQN_LOSS="nqn.2026-07.io.squeezefs:share-n4bgate-loss"   # ledger-loss shape
PORT_PRE=4468
PORT_LOSS=4469
HP_SYSFS=/sys/kernel/mm/hugepages/hugepages-2048kB
NVMET_ROOT=/sys/kernel/config/nvmet
DEVSUB_STATE=/run/squeezefs-devsub

export SQUEEZEFS_NVMEOF_STATE_DIR="$LEDGER_DIR"
export SQUEEZEFS_NVMEOF_RUN_DIR="$RUN_DIR"
unset SQUEEZEFS_SPDK_TGT_BIN SQUEEZEFS_NVMEOF_TARGET_STACK

PASS=0; FAIL=0
log()  { echo "[n4bgate $(date +%H:%M:%S)] $*"; }
ok()   { PASS=$((PASS+1)); log "PASS: $*"; }
bad()  { FAIL=$((FAIL+1)); log "FAIL: $*"; }
die()  { log "FATAL: $*"; exit 1; }
manifest() { echo "$1" >> "$STATE/manifest"; }
tctl() { sensors 2>/dev/null | awk '/Tctl/ {gsub(/[+°C]/,"",$2); print $2}'; }

[ "$(id -u)" = 0 ] || die "run as root"
[ -x "$BIN" ] || die "build first: cargo build --release"
command -v nvme >/dev/null || die "nvme-cli required"
command -v jq >/dev/null || die "jq required"
[ -f /var/tmp/spdk-scoping/spdk/scripts/rpc.py ] || die "sanctioned scoping rpc.py missing"
# Fresh gate state EVERY run (n3 lesson); artifacts survive until the
# next invocation for post-mortem.
[ -d "$STATE" ] && find "$STATE" -mindepth 1 -delete
mkdir -p "$STATE" "$LEDGER_DIR" "$RUN_DIR"
: > "$STATE/manifest"

ETC_REG_MD5="$(md5sum /etc/squeezefs/nvmeof_shares.json 2>/dev/null || echo absent)"
SCOPING_MD5="$(md5sum /var/tmp/spdk-scoping/spdk/build/bin/spdk_tgt 2>/dev/null || echo absent)"

snapshot_sections() { # label -> writes $STATE/snap-<label>.txt
    local out="$STATE/snap-$1.txt"
    {
        echo "--- hugepages 2M nr ---"
        cat "$HP_SYSFS/nr_hugepages" 2>/dev/null
        echo "--- /opt/squeezefs (present?) ---"
        [ -d /opt/squeezefs ] && echo present || echo absent
        echo "--- spdk_tgt processes (non-scoping) ---"
        pgrep -a spdk_tgt 2>/dev/null | grep -v spdk-scoping || echo "(none)"
        echo "--- zram ---"
        ls -1 /dev/zram* 2>/dev/null | sort
        echo "--- n4bgate fabric controllers ---"
        for c in /sys/class/nvme/nvme*; do
            [ -e "$c/subsysnqn" ] || continue
            grep -q n4bgate "$c/subsysnqn" 2>/dev/null && basename "$c"
        done
        echo "--- n4bgate nvmet configfs objects ---"
        ls -1 "$NVMET_ROOT/subsystems" 2>/dev/null | grep n4bgate || echo "(none)"
        ls -1d "$NVMET_ROOT/ports/$PORT_ID_PRE" 2>/dev/null || echo "(port gone)"
        echo "--- devsub substrate presence (observed, never touched) ---"
        ls -1 "$NVMET_ROOT/subsystems" 2>/dev/null | grep devsub || echo "(none)"
        echo "--- listeners 4460-4699 ---"
        ss -ltn 2>/dev/null | awk '$4 ~ /:(44[6-9][0-9]|4[5-6][0-9][0-9])$/ {print $4}' | sort
    } > "$out" 2>&1
    echo "$out"
}

# Shape+mtime snapshot of one configfs subtree (the zero-target-mutation
# witness: adopt must leave every path, type, content and mtime as-is).
configfs_snapshot() { # dir out-file
    local dir=$1 out=$2
    if [ -d "$dir" ]; then
        (cd "$dir" && find . -printf '%P|%y|%T@' -exec sh -c \
            '[ -f "$1" ] && printf "|%s" "$(cat "$1" 2>/dev/null | tr -d "\n")"; echo' _ {} \; \
            | sort) > "$out" 2>/dev/null
    else
        echo "(absent)" > "$out"
    fi
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
    for _ in $(seq 1 60); do
        for c in /sys/class/nvme/nvme*; do
            [ -e "$c/subsysnqn" ] || continue
            if [ "$(cat "$c/subsysnqn" 2>/dev/null)" = "$nqn" ]; then
                cname=$(basename "$c")
                [ -b "/dev/${cname}n1" ] && { echo "/dev/${cname}n1"; return 0; }
            fi
        done
        sleep 0.5
    done
    return 1
}

teardown() {
    log "teardown (manifest-scoped)"
    "$BIN" nvmeof disconnect "$NQN_PRE" >/dev/null 2>&1
    "$BIN" nvmeof disconnect "$NQN_LOSS" >/dev/null 2>&1
    "$BIN" nvmeof unshare "$NQN_PRE" --force >/dev/null 2>&1
    "$BIN" nvmeof unshare "$NQN_LOSS" --force >/dev/null 2>&1
    # Hand-built leftovers (ours by marker), if unshare never adopted them.
    if [ -d "$NVMET_ROOT/subsystems/$NQN_PRE" ]; then
        for p in "$NVMET_ROOT"/ports/*/subsystems/"$NQN_PRE"; do
            [ -e "$p" ] && rm -f "$p" 2>/dev/null
        done
        echo 0 > "$NVMET_ROOT/subsystems/$NQN_PRE/namespaces/1/enable" 2>/dev/null
        rmdir "$NVMET_ROOT/subsystems/$NQN_PRE/namespaces/1" 2>/dev/null
        rmdir "$NVMET_ROOT/subsystems/$NQN_PRE" 2>/dev/null
    fi
    if [ -n "${PORT_ID_PRE:-}" ] && [ -d "$NVMET_ROOT/ports/$PORT_ID_PRE" ]; then
        rmdir "$NVMET_ROOT/ports/$PORT_ID_PRE/subsystems" 2>/dev/null
        rmdir "$NVMET_ROOT/ports/$PORT_ID_PRE" 2>/dev/null
    fi
    # Our target: product verb first, recorded pid as fallback.
    "$BIN" nvmeof target stop --force >/dev/null 2>&1
    if [ -f "$RUN_DIR/spdk_tgt.pid" ]; then
        P=$(cat "$RUN_DIR/spdk_tgt.pid" 2>/dev/null)
        [ -n "${P:-}" ] && kill -9 "$P" 2>/dev/null
    fi
    grep '^spdk_pid=' "$STATE/manifest" 2>/dev/null | cut -d= -f2 | while read -r p; do
        kill -9 "$p" 2>/dev/null
    done
    # zram fixtures (ours only; never index 0).
    grep '^zram=' "$STATE/manifest" 2>/dev/null | sed 's/^zram=//' | awk '{print $1}' | while read -r idx; do
        [ "$idx" != "0" ] || continue
        [ -b "/dev/zram$idx" ] || continue
        echo 1 > "/sys/block/zram$idx/reset" 2>/dev/null
        echo "$idx" > /sys/class/zram-control/hot_remove 2>/dev/null
    done
    # Hugepages: restore via the product record if present.
    if [ -f "$LEDGER_DIR/spdk/hugepages-prior" ]; then
        PR=$(cat "$LEDGER_DIR/spdk/hugepages-prior")
        echo "$PR" > "$HP_SYSFS/nr_hugepages" 2>/dev/null
        log "hugepages force-restored to recorded prior $PR"
    fi
    # devsub substrate: torn down ONLY if this gate created it.
    if grep -q '^created_devsub=1' "$STATE/manifest" 2>/dev/null; then
        SQZ_DEVSUB_FORCE=1 "$REPO/tests/dev_substrate.sh" teardown >/dev/null 2>&1 \
            && log "devsub substrate removed (created by this gate)"
    fi
    # The /opt prefix ONLY if this gate created it.
    if grep -q '^created_opt_prefix=1' "$STATE/manifest" 2>/dev/null; then
        find /opt/squeezefs -depth -delete 2>/dev/null
        log "removed /opt/squeezefs (created by this gate)"
    fi
}
trap teardown EXIT

# ---------------------------------------------------------------------------
log "=== leg 0: snapshot before ==="
PORT_ID_PRE=""   # chosen in leg 1; referenced by snapshots
SNAP_BEFORE=$(snapshot_sections before)
T0=$(tctl); log "Tctl before: ${T0:-n/a}°C"
HP_PRIOR=$(cat "$HP_SYSFS/nr_hugepages")
log "wrote $SNAP_BEFORE (hugepages prior: $HP_PRIOR)"

# ---------------------------------------------------------------------------
log "=== leg 1: hand-built pre-rebuild configfs share -> adopt while serving ==="
modprobe nvmet nvmet-tcp 2>/dev/null
mount -t configfs none /sys/kernel/config 2>/dev/null
[ -d "$NVMET_ROOT/subsystems" ] || die "nvmet configfs tree unavailable"

# A small-int (pre-rebuild-allocator-style) port id, verified free first.
for cand in 4 5 6 7 8 9; do
    [ -d "$NVMET_ROOT/ports/$cand" ] || { PORT_ID_PRE=$cand; break; }
done
[ -n "$PORT_ID_PRE" ] || die "no free small-int port id (4..9 all occupied by tenants)"
log "using small-int port id $PORT_ID_PRE (pre-rebuild allocator shape)"

DEV_ZPRE=$(mkzram $((512*1024*1024)) n4bgate-pre)
UUID_PRE=$(uuidgen)
mkdir -p "$NVMET_ROOT/subsystems/$NQN_PRE/namespaces/1" || die "hand-build subsystem"
manifest "handbuilt_subsystem=$NQN_PRE"
echo 1 > "$NVMET_ROOT/subsystems/$NQN_PRE/attr_allow_any_host"
echo "$DEV_ZPRE" > "$NVMET_ROOT/subsystems/$NQN_PRE/namespaces/1/device_path"
echo "$UUID_PRE" > "$NVMET_ROOT/subsystems/$NQN_PRE/namespaces/1/device_uuid" 2>/dev/null \
    || UUID_PRE=$(cat "$NVMET_ROOT/subsystems/$NQN_PRE/namespaces/1/device_uuid")
echo 1 > "$NVMET_ROOT/subsystems/$NQN_PRE/namespaces/1/enable"
mkdir -p "$NVMET_ROOT/ports/$PORT_ID_PRE"
manifest "handbuilt_port=$PORT_ID_PRE"
echo tcp        > "$NVMET_ROOT/ports/$PORT_ID_PRE/addr_trtype"
echo ipv4       > "$NVMET_ROOT/ports/$PORT_ID_PRE/addr_adrfam"
echo 127.0.0.1  > "$NVMET_ROOT/ports/$PORT_ID_PRE/addr_traddr"
echo $PORT_PRE  > "$NVMET_ROOT/ports/$PORT_ID_PRE/addr_trsvcid"
ln -s "$NVMET_ROOT/subsystems/$NQN_PRE" "$NVMET_ROOT/ports/$PORT_ID_PRE/subsystems/$NQN_PRE"
ok "hand-built pre-rebuild-style share ($NQN_PRE, ns index 1, port id $PORT_ID_PRE)"

modprobe nvme-tcp 2>/dev/null
"$BIN" nvmeof connect --ip 127.0.0.1 --port $PORT_PRE --subnqn "$NQN_PRE" >/dev/null 2>&1 \
    || bad "connect to the hand-built share"
DEV_PRE=$(finddev "$NQN_PRE") || die "no initiator device for $NQN_PRE"
dd if=/dev/urandom of="$DEV_PRE" bs=1M count=8 oflag=direct 2>/dev/null || bad "pre-adopt IO write"
MD5_PRE=$(dd if="$DEV_PRE" bs=1M count=8 iflag=direct 2>/dev/null | md5sum | cut -d' ' -f1)
ok "share serves IO before adopt (initiator $DEV_PRE, md5 $MD5_PRE)"

"$BIN" nvmeof list --json > "$STATE/list-pre.json" 2>/dev/null
jq -e ".foreign_live[] | select(.subnqn==\"$NQN_PRE\" and .stack==\"nvmet\")" "$STATE/list-pre.json" >/dev/null \
    && ok "list shows the hand-built share as foreign (stack=nvmet)" \
    || bad "foreign list: $(jq -c .foreign_live "$STATE/list-pre.json")"

configfs_snapshot "$NVMET_ROOT" "$STATE/configfs-before-adopt.txt"
OUT=$("$BIN" nvmeof adopt "$NQN_PRE" 2>&1); RC=$?
echo "$OUT" > "$STATE/adopt-pre.txt"
if [ $RC -eq 0 ] && echo "$OUT" | grep -q "pre-rebuild" && echo "$OUT" | grep -q "nvmet stack"; then
    ok "adopt absorbed the pre-rebuild share (stack auto-detected, class pre-rebuild)"
else bad "adopt: rc=$RC $OUT"; fi
configfs_snapshot "$NVMET_ROOT" "$STATE/configfs-after-adopt.txt"
if diff -u "$STATE/configfs-before-adopt.txt" "$STATE/configfs-after-adopt.txt" > "$STATE/configfs-adopt.diff"; then
    ok "ZERO target mutation: configfs shape+content+mtime snapshot identical across adopt"
else bad "configfs mutated across adopt:"; cat "$STATE/configfs-adopt.diff"; fi

REC=$(jq -c ".shares[] | select(.subnqn==\"$NQN_PRE\")" "$LEDGER_DIR/shares.json")
echo "$REC" | jq -e ".state==\"active\" and .stack==\"nvmet\" and .nsid==null \
    and (.ns_uuid|ascii_downcase)==(\"$UUID_PRE\"|ascii_downcase) \
    and .listeners[0].nvmet_port_id==$PORT_ID_PRE \
    and .adopted_from.class==\"pre-rebuild\" and (.adopted_from.utc|type)==\"string\"" >/dev/null \
    && ok "adopted record: active, live identity recorded, out-of-range port id $PORT_ID_PRE as-is, provenance" \
    || bad "adopted record shape: $REC"

MD5_PRE2=$(dd if="$DEV_PRE" bs=1M count=8 iflag=direct 2>/dev/null | md5sum | cut -d' ' -f1)
[ "$MD5_PRE2" = "$MD5_PRE" ] \
    && ok "zero serving interruption: connected initiator IO identical across adopt" \
    || bad "IO across adopt: md5 $MD5_PRE2 (want $MD5_PRE)"

"$BIN" nvmeof list --json > "$STATE/list-post.json" 2>/dev/null
jq -e ".shares[] | select(.subnqn==\"$NQN_PRE\") | .classification==\"managed\" and .live==true \
    and .adopted_from.class==\"pre-rebuild\"" "$STATE/list-post.json" >/dev/null \
    && ok "list: adopted share is managed+live with surfaced provenance" \
    || bad "list post-adopt: $(jq -c .shares "$STATE/list-post.json")"

OUT=$("$BIN" nvmeof adopt "$NQN_PRE" 2>&1)
if [ $? -ne 0 ] && echo "$OUT" | grep -q "adopt_already_ledgered"; then
    ok "re-adopt refuses [adopt_already_ledgered]"
else bad "re-adopt: $OUT"; fi

OUT=$("$BIN" nvmeof restore --target-stack nvmet 2>&1); RC=$?
if [ $RC -eq 0 ] && echo "$OUT" | grep -q "$NQN_PRE: already live — verified no-op"; then
    ok "restore treats the adopted share as a verified no-op"
else bad "restore after adopt: rc=$RC $OUT"; fi

"$BIN" nvmeof disconnect "$NQN_PRE" >/dev/null 2>&1 && ok "disconnect" || bad "disconnect"
sleep 1
OUT=$("$BIN" nvmeof unshare "$NQN_PRE" 2>&1); RC=$?
if [ $RC -eq 0 ] && [ ! -d "$NVMET_ROOT/subsystems/$NQN_PRE" ] \
   && [ ! -d "$NVMET_ROOT/ports/$PORT_ID_PRE" ]; then
    ok "unshare of the ADOPTED share tears down cleanly incl. out-of-range port id $PORT_ID_PRE (link-free law)"
else bad "unshare adopted: rc=$RC sub=$([ -d "$NVMET_ROOT/subsystems/$NQN_PRE" ] && echo present || echo gone) \
port=$([ -d "$NVMET_ROOT/ports/$PORT_ID_PRE" ] && echo present || echo gone): $OUT"; fi
jq -e ".shares | length==0" "$LEDGER_DIR/shares.json" >/dev/null 2>&1 \
    && ok "ledger empty after unshare" || bad "ledger residue: $(cat "$LEDGER_DIR/shares.json" 2>/dev/null)"

# ---------------------------------------------------------------------------
log "=== leg 2: SPDK ledger-loss -> adopt -> restore no-op -> unshare ==="
if [ -d /opt/squeezefs ]; then
    log "reusing pre-existing /opt/squeezefs (install idempotency)"
else
    manifest "created_opt_prefix=1"
fi
OUT=$(taskset -c 0-15 "$BIN" nvmeof target install 2>&1); RC=$?
echo "$OUT" > "$STATE/install-out.txt"
if [ $RC -eq 0 ] && { echo "$OUT" | grep -q "verified HEAD == pinned $PIN_SHA" \
   || echo "$OUT" | grep -q "already installed and verified"; }; then
    ok "target install (pinned build present + verified)"
else die "target install: rc=$RC $(tail -5 "$STATE/install-out.txt")"; fi
OUT=$("$BIN" nvmeof target setup --hugemem-mb 2048 2>&1) || die "setup failed: $OUT"
ok "target setup"
OUT=$("$BIN" nvmeof target start 2>&1); RC=$?
[ $RC -eq 0 ] && echo "$OUT" | grep -q "SPDK target started" && ok "target start" \
    || die "target start: $OUT"
PID=$(cat "$RUN_DIR/spdk_tgt.pid"); manifest "spdk_pid=$PID"

DEV_ZLOSS=$(mkzram $((1024*1024*1024)) n4bgate-loss)
OUT=$("$BIN" nvmeof share "$DEV_ZLOSS" --ip 127.0.0.1 --port $PORT_LOSS --subnqn "$NQN_LOSS" 2>&1) \
    || die "product share: $OUT"
ok "product share on the DEFAULT spdk stack"
UUID_LOSS=$(jq -r ".shares[] | select(.subnqn==\"$NQN_LOSS\") | .ns_uuid" "$LEDGER_DIR/shares.json")
BDEV_LOSS=$(jq -r ".shares[] | select(.subnqn==\"$NQN_LOSS\") | .bdev_name" "$LEDGER_DIR/shares.json")
[ -f "$LEDGER_DIR/spdk/ptpl/$UUID_LOSS.json" ] || log "note: ptpl file not yet materialized (no PR activity)"

# THE LEDGER-LOSS SIMULATION: the share ledger is destroyed; the live
# target keeps serving; the state-dir SPDK config + ptpl files survive.
rm -f "$LEDGER_DIR/shares.json"
ok "ledger loss simulated (shares.json deleted; target still serving)"

"$BIN" nvmeof list --json > "$STATE/list-loss.json" 2>/dev/null
jq -e ".foreign_live[] | select(.subnqn==\"$NQN_LOSS\" and .stack==\"spdk\")" "$STATE/list-loss.json" >/dev/null \
    && ok "list shows the orphaned share as foreign (stack=spdk)" \
    || bad "foreign after ledger loss: $(jq -c .foreign_live "$STATE/list-loss.json")"

$RPCPY nvmf_get_subsystems > "$STATE/live-before-adopt.json" 2>/dev/null
$RPCPY bdev_get_bdevs      > "$STATE/bdevs-before-adopt.json" 2>/dev/null
OUT=$("$BIN" nvmeof adopt "$NQN_LOSS" 2>&1); RC=$?
echo "$OUT" > "$STATE/adopt-loss.txt"
if [ $RC -eq 0 ] && echo "$OUT" | grep -q "ledger-loss" && echo "$OUT" | grep -q "spdk stack"; then
    ok "adopt absorbed the orphaned share (stack auto-detected, class ledger-loss)"
else bad "adopt after ledger loss: rc=$RC $OUT"; fi
$RPCPY nvmf_get_subsystems > "$STATE/live-after-adopt.json" 2>/dev/null
$RPCPY bdev_get_bdevs      > "$STATE/bdevs-after-adopt.json" 2>/dev/null
if diff -q "$STATE/live-before-adopt.json" "$STATE/live-after-adopt.json" >/dev/null \
   && diff -q "$STATE/bdevs-before-adopt.json" "$STATE/bdevs-after-adopt.json" >/dev/null; then
    ok "ZERO target mutation: live subsystem+bdev inventory identical across adopt"
else bad "live inventory changed across adopt"; fi

REC=$(jq -c ".shares[] | select(.subnqn==\"$NQN_LOSS\")" "$LEDGER_DIR/shares.json")
echo "$REC" | jq -e ".state==\"active\" and .stack==\"spdk\" and .nsid==1 \
    and (.ns_uuid|ascii_downcase)==(\"$UUID_LOSS\"|ascii_downcase) \
    and .bdev_name==\"$BDEV_LOSS\" \
    and .adopted_from.class==\"ledger-loss\"" >/dev/null \
    && ok "adopted record re-binds the live identity (nsid 1, uuid, bdev name, provenance)" \
    || bad "adopted record: $REC"
if [ -f "$LEDGER_DIR/spdk/ptpl/$UUID_LOSS.json" ]; then
    echo "$REC" | jq -e ".ptpl_file==\"spdk/ptpl/$UUID_LOSS.json\"" >/dev/null \
        && ok "surviving state-dir ptpl file RE-BOUND to the adopted record" \
        || bad "ptpl not re-bound: $REC"
else
    echo "$REC" | jq -e ".ptpl_file==null" >/dev/null \
        && ok "no state-dir ptpl file -> recorded null (loud note in adopt output)" \
        || bad "ptpl shape: $REC"
fi
grep -q "$NQN_LOSS" "$LEDGER_DIR/spdk/tgt-config.json" \
    && ok "save_config truth capture: tgt-config.json describes the adopted share" \
    || bad "tgt-config after adopt misses $NQN_LOSS"

CFG_MD5_BEFORE=$(md5sum "$LEDGER_DIR/spdk/tgt-config.json" | cut -d' ' -f1)
OUT=$("$BIN" nvmeof restore --target-stack spdk 2>&1); RC=$?
CFG_MD5_AFTER=$(md5sum "$LEDGER_DIR/spdk/tgt-config.json" | cut -d' ' -f1)
if [ $RC -eq 0 ] && echo "$OUT" | grep -q "verified no-op" && [ "$CFG_MD5_BEFORE" = "$CFG_MD5_AFTER" ]; then
    ok "restore: adopted share verified no-op, tgt-config untouched (no-op skips save)"
else bad "restore after adopt: rc=$RC cfg-same=$([ "$CFG_MD5_BEFORE" = "$CFG_MD5_AFTER" ] && echo y || echo n) $OUT"; fi

OUT=$("$BIN" nvmeof unshare "$NQN_LOSS" 2>&1) && ok "unshare adopted spdk share" || bad "unshare: $OUT"
SUBS=$($RPCPY nvmf_get_subsystems 2>/dev/null | jq '[.[] | select(.subtype!="Discovery")] | length')
BDEVS=$($RPCPY bdev_get_bdevs 2>/dev/null | jq length)
[ "${SUBS:-x}" = "0" ] && [ "${BDEVS:-x}" = "0" ] \
    && ok "zero target residue after unshare (subsystems=0, bdevs=0)" \
    || bad "target residue: subs=$SUBS bdevs=$BDEVS"
grep -q "$NQN_LOSS" "$LEDGER_DIR/spdk/tgt-config.json" \
    && bad "tgt-config still carries the unshared subsystem" \
    || ok "tgt-config no longer describes the unshared share (resurrection law)"

# ---------------------------------------------------------------------------
log "=== leg 3: harness-owned refusal — observed live against dev_substrate ==="
CREATED_DEVSUB=0
if [ ! -d "$DEVSUB_STATE" ] || ! ls "$NVMET_ROOT/subsystems" 2>/dev/null | grep -q devsub; then
    log "no devsub substrate present — creating one to observe (torn down after)"
    "$REPO/tests/dev_substrate.sh" create > "$STATE/devsub-create.txt" 2>&1 \
        || die "dev_substrate create failed: $(tail -5 "$STATE/devsub-create.txt")"
    manifest "created_devsub=1"
    CREATED_DEVSUB=1
fi
NQN_DEVSUB=$(ls -1 "$NVMET_ROOT/subsystems" | grep devsub | head -1)
[ -n "$NQN_DEVSUB" ] || die "no devsub subsystem visible in configfs"
log "observing devsub subsystem: $NQN_DEVSUB"

configfs_snapshot "$NVMET_ROOT/subsystems/$NQN_DEVSUB" "$STATE/devsub-before.txt"
OUT=$("$BIN" nvmeof adopt "$NQN_DEVSUB" 2>&1); RC=$?
echo "$OUT" > "$STATE/adopt-devsub.txt"
if [ $RC -ne 0 ] && echo "$OUT" | grep -q "adopt_harness_owned" && echo "$OUT" | grep -q "devsub"; then
    ok "adopt of the dev-substrate share REFUSED [adopt_harness_owned], marker named"
else bad "harness-owned refusal: rc=$RC $OUT"; fi
echo "$OUT" | grep -q "teardown" \
    && ok "refusal points at the harness's own teardown" \
    || bad "refusal remediation: $OUT"
configfs_snapshot "$NVMET_ROOT/subsystems/$NQN_DEVSUB" "$STATE/devsub-after.txt"
diff -q "$STATE/devsub-before.txt" "$STATE/devsub-after.txt" >/dev/null \
    && ok "devsub substrate observed, never disturbed (subtree snapshot identical)" \
    || bad "devsub subtree changed!"
jq -e ".shares | length==0" "$LEDGER_DIR/shares.json" >/dev/null 2>&1 \
    && ok "nothing absorbed into the ledger" \
    || bad "ledger gained a record: $(cat "$LEDGER_DIR/shares.json")"
if [ "$CREATED_DEVSUB" = 1 ]; then
    SQZ_DEVSUB_FORCE=1 "$REPO/tests/dev_substrate.sh" teardown > "$STATE/devsub-teardown.txt" 2>&1 \
        && { ok "devsub substrate torn down (created by this gate)"; sed -i '/^created_devsub=/d' "$STATE/manifest"; } \
        || bad "devsub teardown: $(tail -3 "$STATE/devsub-teardown.txt")"
fi

# ---------------------------------------------------------------------------
log "=== leg 4: teardown -> zero residue ==="
OUT=$("$BIN" nvmeof target stop 2>&1) && ok "target stop" || bad "stop: $OUT"
OUT=$("$BIN" nvmeof target setup --restore-prior 2>&1) && ok "hugepages restore-prior" || bad "restore-prior: $OUT"
NR=$(cat "$HP_SYSFS/nr_hugepages")
[ "$NR" = "$HP_PRIOR" ] && ok "nr_hugepages back to prior ($HP_PRIOR)" || bad "nr=$NR want $HP_PRIOR"
grep '^zram=' "$STATE/manifest" | sed 's/^zram=//' | awk '{print $1}' | while read -r idx; do
    [ "$idx" != "0" ] || continue
    [ -b "/dev/zram$idx" ] || continue
    echo 1 > "/sys/block/zram$idx/reset" 2>/dev/null
    echo "$idx" > /sys/class/zram-control/hot_remove 2>/dev/null && log "removed zram$idx"
done
sed -i '/^zram=/d' "$STATE/manifest"
if grep -q '^created_opt_prefix=1' "$STATE/manifest" 2>/dev/null; then
    find /opt/squeezefs -depth -delete 2>/dev/null && log "removed /opt/squeezefs (created by this gate)"
    sed -i '/^created_opt_prefix=/d' "$STATE/manifest"
fi

ETC_REG_MD5_AFTER="$(md5sum /etc/squeezefs/nvmeof_shares.json 2>/dev/null || echo absent)"
[ "$ETC_REG_MD5" = "$ETC_REG_MD5_AFTER" ] && ok "/etc/squeezefs registry untouched" \
    || bad "/etc registry mutated"
SCOPING_MD5_AFTER="$(md5sum /var/tmp/spdk-scoping/spdk/build/bin/spdk_tgt 2>/dev/null || echo absent)"
[ "$SCOPING_MD5" = "$SCOPING_MD5_AFTER" ] && ok "sanctioned scoping build untouched" \
    || bad "scoping build changed!"

SNAP_AFTER=$(snapshot_sections after)
if diff -u "$SNAP_BEFORE" "$SNAP_AFTER" > "$STATE/residue.diff"; then
    ok "ZERO residue: before/after snapshots identical"
else
    bad "residue detected:"; cat "$STATE/residue.diff"
fi
TN=$(tctl); log "Tctl end: ${TN:-n/a}°C"

log "=== n4bgate result: PASS=$PASS FAIL=$FAIL ==="
[ "$FAIL" = 0 ] || exit 1
exit 0
