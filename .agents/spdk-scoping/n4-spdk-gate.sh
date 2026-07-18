#!/usr/bin/env bash
# [SUPERSEDED 2026-07-18, PR 5/N5] standing coverage now tests/run_nvmeof_fidelity.sh (roundtrip-spdk + G2 persistence + guard legs) — kept as evidence lineage; do not extend.
# PR 4 (N4) root-tier gate — SPDK share/unshare/restore via PRODUCT VERBS
# (docs/design-nvmeof-target-management.md, PR-plan PR 4 gate row): the
# FULL G2 acceptance plus the guard clause of G4 that activates at N4:
#
#   target install (reuse-if-pinned) -> setup -> start
#   -> product `share` x2 (zram meta+data backings, DEFAULT stack = spdk;
#      pinned nsid/uuid/ptpl asserted from the ledger + tgt-config)
#   -> duplicate-guard legs (ledger source, cross-stack ledger source,
#      live bdev_get_bdevs filename scan, live configfs walk both ways)
#   -> restore idempotency (verified no-op; no save_config on a no-op)
#   -> product `connect` -> squeezefs format + mount on the shared
#      namespaces -> IO -> writer_guard_mode == flock+pr (G4 clause)
#   -> unshare-with-live-consumer refusal (R7)
#   -> G2: SIGKILL spdk_tgt (OUR pidfile pid) -> `target start`
#      (load_config replays) -> IO RESUMES; reservation intact via PTPL
#      (writer_guard_fenced=0, writer_guard_pr_reacquires=0)
#   -> kill -9 daemon -> remount (S1 register ladder on a PRODUCT-shared
#      namespace) -> clean unmount -> zero PR residue
#   -> disconnect -> unshare x2 -> zero-residue teardown (hugepages
#      restored, /opt per create-discipline, snapshot diff empty).
#
# Ownership conventions (dev_substrate/spdkscope style): every object this
# script creates carries the n4gate marker or is recorded by exact id in
# $STATE/manifest; teardown removes ONLY manifest entries. NEVER touches:
# foreign nvmet trees, zram0, user mounts (/mnt/squeezefs, /mnt/juicefs),
# ~/tmp/nvme, /var/tmp/spdk-scoping (the sanctioned scoping build is
# READ-ONLY here: its rpc.py drives foreign-object fixtures against OUR
# socket), or the live /etc/squeezefs registry. /opt/squeezefs is reused
# when a verified pinned install pre-exists (install idempotency) and
# removed at teardown ONLY when this gate created it.
# Rig arm until PR 5's tests/ harness supersedes it.
set -uo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
BIN="$REPO/target/release/squeezefs"
STATE=/tmp/sqz-n4gate
LEDGER_DIR="$STATE/state"          # SQUEEZEFS_NVMEOF_STATE_DIR (relocated)
RUN_DIR="$STATE/run"               # SQUEEZEFS_NVMEOF_RUN_DIR (relocated)
MNT="$STATE/mnt"
DLOG="$STATE/daemon.log"
PREFIX=/opt/squeezefs/spdk/v26.05  # the REAL pinned install prefix
PIN_SHA=d519b163cbc0e2f28c35d9bc86d610da368b032c
RPCPY="/var/tmp/spdk-scoping/spdk/scripts/rpc.py -s $RUN_DIR/spdk.sock"
NQN_META="nqn.2026-07.io.squeezefs:share-n4gate-meta"
NQN_DATA="nqn.2026-07.io.squeezefs:share-n4gate-data"
NQN_XSPDK="nqn.2026-07.io.foreign:n4gate-xspdk"     # rpc.py-built foreign
NQN_XNVMET="nqn.2026-07.io.foreign:n4gate-xnvmet"   # hand-built configfs
PORT_META=4466
PORT_DATA=4467
HP_SYSFS=/sys/kernel/mm/hugepages/hugepages-2048kB
NVMET_ROOT=/sys/kernel/config/nvmet

export SQUEEZEFS_NVMEOF_STATE_DIR="$LEDGER_DIR"
export SQUEEZEFS_NVMEOF_RUN_DIR="$RUN_DIR"
unset SQUEEZEFS_SPDK_TGT_BIN SQUEEZEFS_NVMEOF_TARGET_STACK

PASS=0; FAIL=0
log()  { echo "[n4gate $(date +%H:%M:%S)] $*"; }
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
# Fresh gate state EVERY run (n3 run-1 lesson: leftover tgt-config/ledger
# is stale rig state, not a product precondition). Artifacts survive
# until the next invocation for post-mortem.
[ -d "$STATE" ] && { umount -l "$MNT" 2>/dev/null; find "$STATE" -mindepth 1 -delete; }
mkdir -p "$STATE" "$LEDGER_DIR" "$RUN_DIR" "$MNT"
: > "$STATE/manifest"

ETC_REG_MD5="$(md5sum /etc/squeezefs/nvmeof_shares.json 2>/dev/null || echo absent)"
SCOPING_MD5="$(md5sum /var/tmp/spdk-scoping/spdk/build/bin/spdk_tgt 2>/dev/null || echo absent)"

snapshot_sections() { # label -> writes $STATE/snap-<label>.txt
    local out="$STATE/snap-$1.txt"
    {
        echo "--- hugepages 2M nr/free is-restored ---"
        cat "$HP_SYSFS/nr_hugepages" 2>/dev/null
        echo "--- /opt/squeezefs (present?) ---"
        [ -d /opt/squeezefs ] && echo present || echo absent
        echo "--- spdk_tgt processes (non-scoping) ---"
        pgrep -a spdk_tgt 2>/dev/null | grep -v spdk-scoping || echo "(none)"
        echo "--- squeezefs daemons on n4gate devices ---"
        pgrep -af "squeezefs.*mount sqmeta" 2>/dev/null | grep n4gate || echo "(none)"
        echo "--- zram ---"
        ls -1 /dev/zram* 2>/dev/null | sort
        echo "--- n4gate fabric controllers ---"
        for c in /sys/class/nvme/nvme*; do
            [ -e "$c/subsysnqn" ] || continue
            grep -q n4gate "$c/subsysnqn" 2>/dev/null && basename "$c"
        done
        echo "--- n4gate nvmet configfs objects ---"
        ls -1 "$NVMET_ROOT/subsystems" 2>/dev/null | grep n4gate || echo "(none)"
        echo "--- listeners 4460-4699 ---"
        ss -ltn 2>/dev/null | awk '$4 ~ /:(44[6-9][0-9]|4[5-6][0-9][0-9])$/ {print $4}' | sort
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

stat_field() { # field -> first scalar off the .stats inode
    jq -r ".metrics.$1[0] // .metrics.$1 // .$1[0] // .$1 // empty" "$MNT/.stats" 2>/dev/null
}
daemon_pid() { pgrep -f "squeezefs.*mount sqmeta://$DEV_META" | head -1; }
mount_it()   { RUST_LOG=info "$BIN" --log-file "$DLOG" mount "sqmeta://$DEV_META" "$MNT" --daemon --allow-other >> "$STATE/mount-out.txt" 2>&1; }
wait_mounted() { local i; for i in $(seq 1 60); do awk -v m="$MNT" '$2==m{f=1} END{exit !f}' /proc/mounts && return 0; sleep 0.5; done; return 1; }

teardown() {
    log "teardown (manifest-scoped)"
    umount "$MNT" 2>/dev/null; umount -l "$MNT" 2>/dev/null
    P=$(daemon_pid 2>/dev/null); [ -n "${P:-}" ] && kill -9 "$P" 2>/dev/null
    "$BIN" nvmeof disconnect "$NQN_META" >/dev/null 2>&1
    "$BIN" nvmeof disconnect "$NQN_DATA" >/dev/null 2>&1
    "$BIN" nvmeof unshare "$NQN_META" --force >/dev/null 2>&1
    "$BIN" nvmeof unshare "$NQN_DATA" --force >/dev/null 2>&1
    # foreign fixtures (ours by marker)
    $RPCPY nvmf_delete_subsystem "$NQN_XSPDK" >/dev/null 2>&1
    $RPCPY bdev_aio_delete n4gate_xbdev >/dev/null 2>&1
    $RPCPY bdev_aio_delete n4gate_xaio >/dev/null 2>&1
    if [ -d "$NVMET_ROOT/subsystems/$NQN_XNVMET" ]; then
        rmdir "$NVMET_ROOT/subsystems/$NQN_XNVMET/namespaces/1" 2>/dev/null
        rmdir "$NVMET_ROOT/subsystems/$NQN_XNVMET" 2>/dev/null
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
    # The /opt prefix ONLY if this gate created it (manifest-recorded).
    if grep -q '^created_opt_prefix=1' "$STATE/manifest" 2>/dev/null; then
        find /opt/squeezefs -depth -delete 2>/dev/null
        log "removed /opt/squeezefs (created by this gate)"
    fi
}
trap teardown EXIT

# ---------------------------------------------------------------------------
log "=== leg 0: snapshot before ==="
SNAP_BEFORE=$(snapshot_sections before)
T0=$(tctl); log "Tctl before: ${T0:-n/a}°C"
HP_PRIOR=$(cat "$HP_SYSFS/nr_hugepages")
log "wrote $SNAP_BEFORE (hugepages prior: $HP_PRIOR)"

# ---------------------------------------------------------------------------
log "=== leg 1: install (reuse-if-pinned) -> setup -> start ==="
if [ -d /opt/squeezefs ]; then
    log "reusing pre-existing /opt/squeezefs (install idempotency leg)"
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
ok "target setup (hugepages reserved, prior recorded)"
OUT=$("$BIN" nvmeof target start 2>&1); RC=$?
echo "$OUT" > "$STATE/start-out.txt"
[ $RC -eq 0 ] && echo "$OUT" | grep -q "SPDK target started" && ok "target start" \
    || die "target start: $OUT"
PID=$(cat "$RUN_DIR/spdk_tgt.pid"); manifest "spdk_pid=$PID"

# ---------------------------------------------------------------------------
log "=== leg 2: product share x2 — DEFAULT stack (spdk), pinned identity ==="
DEV_ZMETA=$(mkzram $((1024*1024*1024)) n4gate-meta)
DEV_ZDATA=$(mkzram $((2*1024*1024*1024)) n4gate-data)
log "zram backings: $DEV_ZMETA (meta) $DEV_ZDATA (data)"

OUT=$("$BIN" nvmeof share "$DEV_ZMETA" --ip 127.0.0.1 --port $PORT_META --subnqn "$NQN_META" 2>&1); RC=$?
echo "$OUT" > "$STATE/share-meta.txt"
if [ $RC -eq 0 ] && echo "$OUT" | grep -q "spdk stack" && echo "$OUT" | grep -q "$NQN_META"; then
    ok "share (meta) rode the DEFAULT spdk stack"
else die "share meta: $OUT"; fi
UUID_META=$(jq -r ".shares[] | select(.subnqn==\"$NQN_META\") | .ns_uuid" "$LEDGER_DIR/shares.json")
OUT=$("$BIN" nvmeof share "$DEV_ZDATA" --ip 127.0.0.1 --port $PORT_DATA --subnqn "$NQN_DATA" 2>&1) \
    || die "share data: $OUT"
ok "share (data)"

REC=$(jq -c ".shares[] | select(.subnqn==\"$NQN_META\")" "$LEDGER_DIR/shares.json")
echo "$REC" | jq -e ".state==\"active\" and .stack==\"spdk\" and .nsid==1 \
    and .ns_uuid!=null and .bdev_name!=null \
    and .ptpl_file==\"spdk/ptpl/$UUID_META.json\" and .loop_device==null" >/dev/null \
    && ok "ledger record pins the section-6.4 SPDK presence shape (nsid=1, uuid, ptpl, bdev)" \
    || bad "ledger record shape: $REC"
grep -q "$NQN_META" "$LEDGER_DIR/spdk/tgt-config.json" \
    && grep -q "$UUID_META" "$LEDGER_DIR/spdk/tgt-config.json" \
    && ok "tgt-config.json (save_config) captured subsystem + pinned uuid" \
    || bad "tgt-config after share: $(head -c 400 "$LEDGER_DIR/spdk/tgt-config.json")"
# The REAL target reports the pinned identity back (nsid/uuid live).
$RPCPY nvmf_get_subsystems > "$STATE/live-subs.json" 2>/dev/null
jq -e ".[] | select(.nqn==\"$NQN_META\") | .namespaces[0] | (.nsid==1 and ((.uuid|ascii_downcase)==(\"$UUID_META\"|ascii_downcase)))" \
    "$STATE/live-subs.json" >/dev/null \
    && ok "live target serves nsid 1 under the pinned ns UUID" \
    || bad "live namespace identity: $(jq -c ".[] | select(.nqn==\"$NQN_META\") | .namespaces" "$STATE/live-subs.json")"
ST=$("$BIN" nvmeof target status --json 2>/dev/null)
echo "$ST" | jq -e '.ledger.managed==2 and .subsystems==2' >/dev/null \
    && ok "status: 2 managed subsystems" || bad "status: $(echo "$ST" | jq -c '{subsystems, ledger}')"

log "=== leg 2b: list reconciliation shows both managed ==="
"$BIN" nvmeof list --json > "$STATE/list.json" 2>/dev/null
jq -e "[.shares[] | select(.classification==\"managed\" and .live==true)] | length==2" "$STATE/list.json" >/dev/null \
    && ok "list: both shares managed+live" || bad "list: $(jq -c .shares "$STATE/list.json")"

# ---------------------------------------------------------------------------
log "=== leg 3: duplicate guard — the triple source, live on a real box ==="
# (a) ledger source: same backing, same stack.
OUT=$("$BIN" nvmeof share "$DEV_ZMETA" --ip 127.0.0.1 --port 4470 --subnqn "${NQN_META}-dup" 2>&1)
if [ $? -ne 0 ] && echo "$OUT" | grep -q "$NQN_META" && echo "$OUT" | grep -q "unshare"; then
    ok "dup guard (ledger): same backing refused naming the holder + unshare exit"
else bad "ledger dup: $OUT"; fi
# (b) cross-stack ledger source: same backing via nvmet.
OUT=$("$BIN" nvmeof share "$DEV_ZMETA" --ip 127.0.0.1 --port 4470 --target-stack nvmet --subnqn "${NQN_META}-xdup" 2>&1)
if [ $? -ne 0 ] && echo "$OUT" | grep -q "never be double-served\|already shared"; then
    ok "dup guard (cross-stack ledger): nvmet share of the spdk-served backing refused"
else bad "cross-stack ledger dup: $OUT"; fi
# (c) live bdev filename scan: foreign UNLEDGERED bdev opens a zram.
DEV_ZC=$(mkzram $((256*1024*1024)) n4gate-xbdev)
$RPCPY bdev_aio_create "$DEV_ZC" n4gate_xbdev 4096 >/dev/null || bad "fixture bdev create"
OUT=$("$BIN" nvmeof share "$DEV_ZC" --ip 127.0.0.1 --port 4471 --subnqn "${NQN_META}-c" 2>&1)
if [ $? -ne 0 ] && echo "$OUT" | grep -q "n4gate_xbdev" && echo "$OUT" | grep -q "bdev_aio_delete"; then
    ok "dup guard (live bdev_get_bdevs scan): foreign bdev refused w/ rpc.py steps"
else bad "live bdev dup: $OUT"; fi
$RPCPY bdev_aio_delete n4gate_xbdev >/dev/null
# (d) cross-stack live configfs walk: hand-built nvmet subsystem serves a zram.
modprobe nvmet 2>/dev/null; mount -t configfs none /sys/kernel/config 2>/dev/null
DEV_ZD=$(mkzram $((256*1024*1024)) n4gate-xnvmet)
mkdir -p "$NVMET_ROOT/subsystems/$NQN_XNVMET/namespaces/1" || bad "configfs fixture"
echo "$DEV_ZD" > "$NVMET_ROOT/subsystems/$NQN_XNVMET/namespaces/1/device_path"
OUT=$("$BIN" nvmeof share "$DEV_ZD" --ip 127.0.0.1 --port 4472 --subnqn "${NQN_META}-d" 2>&1)
if [ $? -ne 0 ] && echo "$OUT" | grep -q "$NQN_XNVMET" && echo "$OUT" | grep -q "nvmet" \
   && echo "$OUT" | grep -q "foreign"; then
    ok "dup guard (cross-stack configfs walk): nvmet-live backing refused the spdk share"
else bad "configfs-walk dup: $OUT"; fi
rmdir "$NVMET_ROOT/subsystems/$NQN_XNVMET/namespaces/1" "$NVMET_ROOT/subsystems/$NQN_XNVMET" 2>/dev/null
# (e) reverse direction: rpc.py-built UNLEDGERED spdk subsystem serves a zram;
#     an nvmet share of it must refuse via the spdk live walk.
DEV_ZE=$(mkzram $((256*1024*1024)) n4gate-xspdk)
$RPCPY bdev_aio_create "$DEV_ZE" n4gate_xaio 4096 >/dev/null || bad "fixture xaio"
$RPCPY nvmf_create_subsystem "$NQN_XSPDK" -a -s N4GATEX1 >/dev/null || bad "fixture xsub"
$RPCPY nvmf_subsystem_add_ns "$NQN_XSPDK" n4gate_xaio >/dev/null || bad "fixture xns"
OUT=$("$BIN" nvmeof share "$DEV_ZE" --ip 127.0.0.1 --port 4473 --target-stack nvmet --subnqn "${NQN_META}-e" 2>&1)
if [ $? -ne 0 ] && echo "$OUT" | grep -q "$NQN_XSPDK" && echo "$OUT" | grep -q "rpc.py"; then
    ok "dup guard (cross-stack spdk walk): spdk-live backing refused the nvmet share w/ rpc.py steps"
else bad "spdk-walk dup: $OUT"; fi
"$BIN" nvmeof list --json > "$STATE/list-foreign.json" 2>/dev/null
jq -e ".foreign_live[] | select(.subnqn==\"$NQN_XSPDK\" and .stack==\"spdk\")" "$STATE/list-foreign.json" >/dev/null \
    && ok "list: rpc.py-built subsystem surfaces as foreign (stack=spdk, never touched)" \
    || bad "foreign list: $(jq -c .foreign_live "$STATE/list-foreign.json")"
$RPCPY nvmf_delete_subsystem "$NQN_XSPDK" >/dev/null
$RPCPY bdev_aio_delete n4gate_xaio >/dev/null

# ---------------------------------------------------------------------------
log "=== leg 4: restore idempotency — verified no-op skips save_config ==="
CFG_MD5_BEFORE=$(md5sum "$LEDGER_DIR/spdk/tgt-config.json" | cut -d' ' -f1)
OUT=$("$BIN" nvmeof restore --target-stack spdk 2>&1); RC=$?
echo "$OUT" > "$STATE/restore-noop.txt"
N_NOOP=$(grep -c "verified no-op" "$STATE/restore-noop.txt")
CFG_MD5_AFTER=$(md5sum "$LEDGER_DIR/spdk/tgt-config.json" | cut -d' ' -f1)
if [ $RC -eq 0 ] && [ "$N_NOOP" = 2 ] && [ "$CFG_MD5_BEFORE" = "$CFG_MD5_AFTER" ]; then
    ok "restore: 2x verified no-op, tgt-config untouched (no-op skips save_config)"
else bad "restore no-op: rc=$RC noop=$N_NOOP cfg-same=$([ "$CFG_MD5_BEFORE" = "$CFG_MD5_AFTER" ] && echo y || echo n): $OUT"; fi

# ---------------------------------------------------------------------------
log "=== leg 5: product connect -> format -> mount -> IO -> flock+pr (G4 clause) ==="
modprobe nvme-tcp 2>/dev/null
"$BIN" nvmeof connect --ip 127.0.0.1 --port $PORT_META --subnqn "$NQN_META" >/dev/null 2>&1 \
    || bad "product connect meta"
"$BIN" nvmeof connect --ip 127.0.0.1 --port $PORT_DATA --subnqn "$NQN_DATA" >/dev/null 2>&1 \
    || bad "product connect data"
DEV_META=$(finddev "$NQN_META") || die "no initiator device for $NQN_META"
DEV_DATA=$(finddev "$NQN_DATA") || die "no initiator device for $NQN_DATA"
log "initiator devices: $DEV_META (meta) $DEV_DATA (data)"

"$BIN" format "sqmeta://$DEV_META" "sqdata://$DEV_DATA" --force > "$STATE/format-out.txt" 2>&1 \
    || die "format failed: $(tail -3 "$STATE/format-out.txt")"
ok "squeezefs format on the product-shared namespaces"
mount_it
wait_mounted || die "mount did not appear: $(tail -5 "$STATE/mount-out.txt") $(tail -5 "$DLOG")"
sleep 2
MODE=$(stat_field writer_guard_mode)
[ "$MODE" = "flock+pr" ] \
    && ok "writer_guard_mode=flock+pr on a PRODUCT-shared namespace (G4 product-verb clause)" \
    || bad "writer_guard_mode=$MODE (want flock+pr)"
dd if=/dev/urandom of="$MNT/n4gate.bin" bs=1M count=16 2>/dev/null || bad "IO write"
sync
MD5=$(md5sum "$MNT/n4gate.bin" | cut -d' ' -f1)
log "n4gate.bin md5=$MD5"
[ -f "$LEDGER_DIR/spdk/ptpl/$UUID_META.json" ] \
    && ok "ptpl_file materialized at <state>/spdk/ptpl/<uuid>.json (reservation persisted)" \
    || bad "ptpl file missing after guard registration"

log "=== leg 5b: unshare-with-live-consumer refusal (R7) ==="
OUT=$("$BIN" nvmeof unshare "$NQN_META" 2>&1)
if [ $? -ne 0 ] && echo "$OUT" | grep -q "$NQN_META" && echo "$OUT" | grep -q -- "--force" \
   && echo "$OUT" | grep -q "disconnect"; then
    ok "unshare refused with a live consumer (names NQN + --force + the disconnect sequence)"
else bad "live-consumer unshare refusal: $OUT"; fi

# ---------------------------------------------------------------------------
log "=== leg 6: G2 — SIGKILL spdk_tgt -> target start (load_config) -> IO resumes, PTPL intact ==="
TGT_PID=$(cat "$RUN_DIR/spdk_tgt.pid")
kill -9 "$TGT_PID" || die "SIGKILL spdk_tgt"
log "spdk_tgt pid $TGT_PID SIGKILLed (the target power-cut)"
sleep 1
OUT=$("$BIN" nvmeof target start 2>&1); RC=$?
echo "$OUT" > "$STATE/restart-out.txt"
if [ $RC -eq 0 ] && echo "$OUT" | grep -q "load_config applied"; then
    ok "target start after SIGKILL: load_config replayed the SPDK source of truth"
else die "restart: $OUT"; fi
NEWPID=$(cat "$RUN_DIR/spdk_tgt.pid"); manifest "spdk_pid=$NEWPID"
$RPCPY nvmf_get_subsystems > "$STATE/live-after-restart.json" 2>/dev/null
jq -e ".[] | select(.nqn==\"$NQN_META\") | .namespaces[0] | ((.uuid|ascii_downcase)==(\"$UUID_META\"|ascii_downcase))" \
    "$STATE/live-after-restart.json" >/dev/null \
    && ok "share reappeared under the SAME NQN/nsid/UUID (G2 persistence clause)" \
    || bad "post-restart identity: $(jq -c ".[] | select(.nqn==\"$NQN_META\")" "$STATE/live-after-restart.json")"

IO_OK=""
for i in $(seq 1 60); do
    dd if="$MNT/n4gate.bin" of=/dev/null bs=1M count=1 2>/dev/null && { IO_OK=1; break; }
    sleep 2
done
sleep 12   # one heartbeat re-check past reattach (guard-smoke cadence)
MD5B=$(md5sum "$MNT/n4gate.bin" 2>/dev/null | cut -d' ' -f1)
FENCED=$(stat_field writer_guard_fenced)
REACQ=$(stat_field writer_guard_pr_reacquires)
if [ -n "$IO_OK" ] && [ "$MD5B" = "$MD5" ]; then
    ok "IO RESUMED through the target bounce without operator action (data intact)"
else bad "IO resume: ok=${IO_OK:-no} md5=$MD5B (want $MD5)"; fi
if [ "${FENCED:-1}" = "0" ] && [ "${REACQ:-1}" = "0" ]; then
    ok "reservation INTACT via PTPL (writer_guard_fenced=0, pr_reacquires=0 — section 6.7 signal)"
else bad "PTPL survival: fenced=$FENCED pr_reacquires=$REACQ (want 0/0)"; fi
dd if=/dev/urandom of="$MNT/n4gate-post.bin" bs=1M count=4 2>/dev/null && sync \
    && ok "post-bounce writes land" || bad "post-bounce write failed"

# ---------------------------------------------------------------------------
log "=== leg 7: kill -9 daemon -> remount (S1 ladder on a product-shared namespace) ==="
DPID=$(daemon_pid); [ -n "$DPID" ] || die "no daemon pid"
kill -9 "$DPID"; sleep 2
umount -l "$MNT" 2>/dev/null; sleep 1
mount_it
if wait_mounted; then
    sleep 2
    MODE2=$(stat_field writer_guard_mode)
    MD5C=$(md5sum "$MNT/n4gate.bin" 2>/dev/null | cut -d' ' -f1)
    if [ "$MODE2" = "flock+pr" ] && [ "$MD5C" = "$MD5" ]; then
        ok "kill-9 -> remount recovered (flock+pr, data intact) — S1 ladder on product shares"
    else bad "remount: mode=$MODE2 md5=$MD5C"; fi
else
    bad "remount did not appear after kill -9"
    timeout 30 "$BIN" mount "sqmeta://$DEV_META" "$MNT" --daemon --allow-other >> "$STATE/mount-out.txt" 2>&1
fi
LADDER_HITS=$(grep -c "register conflicted with our own stale" "$DLOG" 2>/dev/null || true)
[ "${LADDER_HITS:-0}" -ge 1 ] \
    && ok "register ladder fired on the spec-strict SPDK target (log hits=$LADDER_HITS)" \
    || bad "register ladder never fired (expected on spec-strict SPDK)"

# ---------------------------------------------------------------------------
log "=== leg 8: clean unmount -> zero PR residue ==="
umount "$MNT" || bad "clean unmount"
sleep 1
REG=$(nvme resv-report "$DEV_META" --eds -o json 2>/dev/null | jq -r .regctl)
[ "${REG:-x}" = "0" ] && ok "zero PR residue after clean unmount (regctl=0)" \
                      || bad "PR residue (regctl=$REG)"

# ---------------------------------------------------------------------------
log "=== leg 9: disconnect -> unshare x2 -> stop -> hugepage restore ==="
"$BIN" nvmeof disconnect "$NQN_META" >/dev/null 2>&1 && ok "product disconnect meta" || bad "disconnect meta"
"$BIN" nvmeof disconnect "$NQN_DATA" >/dev/null 2>&1 && ok "product disconnect data" || bad "disconnect data"
sleep 1
OUT=$("$BIN" nvmeof unshare "$NQN_META" 2>&1) && ok "unshare meta" || bad "unshare meta: $OUT"
OUT=$("$BIN" nvmeof unshare "$NQN_DATA" 2>&1) && ok "unshare data" || bad "unshare data: $OUT"
ST=$("$BIN" nvmeof target status --json 2>/dev/null)
echo "$ST" | jq -e '.subsystems==0 and .ledger.managed==0 and .ledger.foreign_live==0' >/dev/null \
    && ok "target empty after unshare (subsystems=0, ledger clean)" \
    || bad "post-unshare status: $(echo "$ST" | jq -c '{subsystems, ledger}')"
grep -q "$NQN_META\|$NQN_DATA" "$LEDGER_DIR/spdk/tgt-config.json" \
    && bad "tgt-config still carries an unshared subsystem (resurrection hazard)" \
    || ok "tgt-config no longer describes the unshared subsystems (resurrection law)"
BDEVS=$($RPCPY bdev_get_bdevs 2>/dev/null | jq length)
[ "${BDEVS:-x}" = "0" ] && ok "zero bdev residue" || bad "bdev residue: $BDEVS"
OUT=$("$BIN" nvmeof target stop 2>&1) && ok "target stop" || bad "stop: $OUT"
OUT=$("$BIN" nvmeof target setup --restore-prior 2>&1) && ok "hugepages restore-prior" || bad "restore-prior: $OUT"
NR=$(cat "$HP_SYSFS/nr_hugepages")
[ "$NR" = "$HP_PRIOR" ] && ok "nr_hugepages back to prior ($HP_PRIOR)" || bad "nr=$NR want $HP_PRIOR"

# ---------------------------------------------------------------------------
log "=== leg 10: teardown -> zero residue ==="
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

log "=== n4gate result: PASS=$PASS FAIL=$FAIL ==="
[ "$FAIL" = 0 ] || exit 1
exit 0
