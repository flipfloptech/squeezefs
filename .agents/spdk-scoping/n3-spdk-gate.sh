#!/usr/bin/env bash
# PR 3 (N3) root-tier gate — SPDK target lifecycle via PRODUCT VERBS
# (docs/design-nvmeof-target-management.md, PR-plan PR 3 gate row):
#   install (pinned clone+build, provenance, idempotency, dirty refusal)
#   -> setup (hugepage reservation, recorded prior) -> start (pidfile,
#   RPC liveness, socket 0600) -> status (§6.9 payload) -> stop
#   (save_config -> TERM; refuse-with-live-consumers) -> restart
#   persistence sanity (load_config) -> restore-prior -> ZERO residue.
#   + the G3 loud-fail matrix legs (no binary / no hugepages / unpinned
#   override / already-running) executed on a real box.
#
# Ownership conventions (dev_substrate/spdkscope style): every object this
# script creates carries the n3gate marker or is recorded by exact id in
# $STATE/manifest; teardown removes ONLY manifest entries. NEVER touches:
# foreign nvmet trees, zram0, user mounts, /var/tmp/spdk-scoping (the
# sanctioned scoping build), or the live /etc/squeezefs registry. The
# /opt/squeezefs prefix is created by THIS gate's install leg and removed
# at teardown (recorded in the manifest).
# Rig arm until PR 5's tests/ harness supersedes it.
set -uo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
BIN="$REPO/target/release/squeezefs"
STATE=/tmp/sqz-n3gate
LEDGER_DIR="$STATE/state"          # SQUEEZEFS_NVMEOF_STATE_DIR (relocated)
RUN_DIR="$STATE/run"               # SQUEEZEFS_NVMEOF_RUN_DIR (relocated)
PREFIX=/opt/squeezefs/spdk/v26.05  # the REAL pinned install prefix
PIN_SHA=d519b163cbc0e2f28c35d9bc86d610da368b032c
SCOPING_TGT=/var/tmp/spdk-scoping/spdk/build/bin/spdk_tgt
RPCPY="/var/tmp/spdk-scoping/spdk/scripts/rpc.py -s $RUN_DIR/spdk.sock"
NQN_LIVE="nqn.2026-07.io.squeezefs:share-n3gate-live"
PORT_LIVE=4464
HP_SYSFS=/sys/kernel/mm/hugepages/hugepages-2048kB

export SQUEEZEFS_NVMEOF_STATE_DIR="$LEDGER_DIR"
export SQUEEZEFS_NVMEOF_RUN_DIR="$RUN_DIR"
unset SQUEEZEFS_SPDK_TGT_BIN SQUEEZEFS_NVMEOF_TARGET_STACK

PASS=0; FAIL=0
log()  { echo "[n3gate $(date +%H:%M:%S)] $*"; }
ok()   { PASS=$((PASS+1)); log "PASS: $*"; }
bad()  { FAIL=$((FAIL+1)); log "FAIL: $*"; }
die()  { log "FATAL: $*"; exit 1; }
manifest() { echo "$1" >> "$STATE/manifest"; }
tctl() { sensors 2>/dev/null | awk '/Tctl/ {gsub(/[+°C]/,"",$2); print $2}'; }

[ "$(id -u)" = 0 ] || die "run as root"
[ -x "$BIN" ] || die "build first: cargo build --release"
command -v nvme >/dev/null || die "nvme-cli required"
command -v jq >/dev/null || die "jq required"
[ -x "$SCOPING_TGT" ] || die "sanctioned scoping build missing at $SCOPING_TGT"
mkdir -p "$STATE" "$LEDGER_DIR" "$RUN_DIR"
: > "$STATE/manifest"

ETC_REG_MD5="$(md5sum /etc/squeezefs/nvmeof_shares.json 2>/dev/null || echo absent)"

snapshot_sections() { # label -> writes $STATE/snap-<label>.txt
    local out="$STATE/snap-$1.txt"
    {
        echo "--- hugepages 2M nr/free ---"
        cat "$HP_SYSFS/nr_hugepages" "$HP_SYSFS/free_hugepages" 2>/dev/null
        echo "--- /opt/squeezefs ---"
        ls -1 /opt/squeezefs 2>/dev/null || echo "(absent)"
        echo "--- spdk_tgt processes ---"
        pgrep -a spdk_tgt 2>/dev/null | grep -v spdk-scoping || echo "(none)"
        echo "--- zram ---"
        ls -1 /dev/zram* 2>/dev/null | sort
        echo "--- n3gate fabric controllers ---"
        for c in /sys/class/nvme/nvme*; do
            [ -e "$c/subsysnqn" ] || continue
            grep -q n3gate "$c/subsysnqn" 2>/dev/null && basename "$c"
        done
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

teardown() {
    log "teardown (manifest-scoped)"
    nvme disconnect -n "$NQN_LIVE" >/dev/null 2>&1
    # Stop our target: product verb first, recorded pid as fallback.
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
    # Hugepages: restore via the product record if present, else assert 0.
    if [ -f "$LEDGER_DIR/spdk/hugepages-prior" ]; then
        PR=$(cat "$LEDGER_DIR/spdk/hugepages-prior")
        echo "$PR" > "$HP_SYSFS/nr_hugepages" 2>/dev/null
        log "hugepages force-restored to recorded prior $PR"
    fi
    # The /opt prefix this gate created (manifest-recorded).
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
[ -d /opt/squeezefs ] && die "/opt/squeezefs pre-exists — refusing (this gate wants to prove install from zero and tear down clean)"

# ---------------------------------------------------------------------------
log "=== leg 1: G3 no-binary loud fail (before any install) ==="
OUT=$("$BIN" nvmeof target start 2>&1)
if [ $? -ne 0 ] && echo "$OUT" | grep -q "pinned spdk_tgt binary not installed" \
   && echo "$OUT" | grep -q "nvmeof target install" \
   && echo "$OUT" | grep -q "never falls back between target stacks"; then
    ok "no-binary refusal names target install + the no-fallback law"
else bad "no-binary refusal: $OUT"; fi

OUT=$("$BIN" nvmeof target systemd-unit 2>&1)
if [ $? -ne 0 ] && echo "$OUT" | grep -q "target install"; then
    ok "systemd-unit refuses without a binary (ExecStart must exist)"
else bad "systemd-unit no-binary: $OUT"; fi

# ---------------------------------------------------------------------------
log "=== leg 2: G3 no-hugepages loud fail + unpinned-override warning ==="
FREE_HP=$(cat "$HP_SYSFS/free_hugepages")
if [ "$FREE_HP" -lt 512 ]; then
    OUT=$(SQUEEZEFS_SPDK_TGT_BIN="$SCOPING_TGT" "$BIN" nvmeof target start 2>&1)
    if [ $? -ne 0 ] && echo "$OUT" | grep -q "not enough free hugepages" \
       && echo "$OUT" | grep -q "need 512 free 2 MiB pages" \
       && echo "$OUT" | grep -q "target setup --hugemem-mb"; then
        ok "no-hugepages refusal carries the arithmetic + names target setup"
    else bad "no-hugepages refusal: $OUT"; fi
else
    log "SKIP no-hugepages leg: box already has $FREE_HP free pages"
fi

UNIT_OUT=$(SQUEEZEFS_SPDK_TGT_BIN="$SCOPING_TGT" "$BIN" nvmeof target systemd-unit 2>"$STATE/unit-err.txt")
if [ $? -eq 0 ] && echo "$UNIT_OUT" | grep -q "ExecStart=$SCOPING_TGT -r $RUN_DIR/spdk.sock" \
   && grep -q "unpinned" "$STATE/unit-err.txt" && ! echo "$UNIT_OUT" | grep -q unpinned; then
    ok "systemd-unit bakes the override + warns unpinned on stderr only"
else bad "override unit emission: $UNIT_OUT / $(cat "$STATE/unit-err.txt")"; fi
echo "$UNIT_OUT" | grep -q "RuntimeDirectoryMode=0700" && ok "unit carries the 0700 runtime dir" \
    || bad "unit runtime-dir mode missing"

# ---------------------------------------------------------------------------
log "=== leg 3: target install — REAL pinned clone+build into $PREFIX ==="
manifest "created_opt_prefix=1"
OUT=$(taskset -c 0-15 "$BIN" nvmeof target install 2>&1); RC=$?
echo "$OUT" > "$STATE/install-out.txt"
if [ $RC -eq 0 ] && echo "$OUT" | grep -q "verified HEAD == pinned $PIN_SHA"; then
    ok "install verified the pinned sha after clone"
else bad "install run: rc=$RC $(tail -5 "$STATE/install-out.txt")"; fi
[ -x "$PREFIX/build/bin/spdk_tgt" ] && ok "pinned binary at the canonical §6.5 path" \
    || bad "binary missing at $PREFIX/build/bin/spdk_tgt"
HEAD=$(git -C "$PREFIX/src" rev-parse HEAD 2>/dev/null)
[ "$HEAD" = "$PIN_SHA" ] && ok "checkout HEAD == pinned sha" || bad "HEAD=$HEAD"
if [ -f "$PREFIX/build-info.txt" ] && grep -q "tag: v26.05" "$PREFIX/build-info.txt" \
   && grep -q "commit: $PIN_SHA" "$PREFIX/build-info.txt" \
   && grep -q "configure: --disable-tests" "$PREFIX/build-info.txt"; then
    ok "build-info provenance (tag/commit/configure/cc/built)"
else bad "build-info: $(cat "$PREFIX/build-info.txt" 2>/dev/null)"; fi

OUT=$("$BIN" nvmeof target install 2>&1)
if [ $? -eq 0 ] && echo "$OUT" | grep -q "already installed and verified"; then
    ok "second install is a verified no-op (idempotent)"
else bad "idempotent install: $OUT"; fi

echo "# n3gate dirt" >> "$PREFIX/src/README.md"
OUT=$("$BIN" nvmeof target install 2>&1)
if [ $? -ne 0 ] && echo "$OUT" | grep -q "DIRTY"; then
    ok "dirty checkout refuses loud"
else bad "dirty refusal: $OUT"; fi
git -C "$PREFIX/src" checkout -- README.md
OUT=$("$BIN" nvmeof target install 2>&1)
echo "$OUT" | grep -q "already installed" && ok "clean again after git checkout" \
    || bad "post-clean install: $OUT"

OUT=$("$BIN" nvmeof target install --version v99.01 2>&1)
if [ $? -ne 0 ] && echo "$OUT" | grep -q "pin bumps are deliberate PRs"; then
    ok "--version != pin refuses naming the pin policy"
else bad "version-pin refusal: $OUT"; fi
T1=$(tctl); log "Tctl after build: ${T1:-n/a}°C"

# ---------------------------------------------------------------------------
log "=== leg 4: target setup — reservation + recorded prior ==="
OUT=$("$BIN" nvmeof target setup --hugemem-mb 2048 2>&1) || die "setup failed: $OUT"
echo "$OUT" | grep -q "recorded prior nr_hugepages = $HP_PRIOR" \
    && ok "prior ($HP_PRIOR) recorded by the product" || bad "prior record: $OUT"
NR=$(cat "$HP_SYSFS/nr_hugepages")
[ "$NR" -ge 1024 ] && ok "reserved >= 1024 x 2MiB pages (nr=$NR)" || bad "nr=$NR"
[ "$(cat "$LEDGER_DIR/spdk/hugepages-prior")" = "$HP_PRIOR" ] \
    && ok "recorded-prior file carries the pre-mutation value" || bad "prior file wrong"
OUT=$("$BIN" nvmeof target setup --hugemem-mb 2048 2>&1)
if echo "$OUT" | grep -q "verified no-op" && [ "$(cat "$LEDGER_DIR/spdk/hugepages-prior")" = "$HP_PRIOR" ]; then
    ok "second setup: verified no-op, prior record untouched (write-once)"
else bad "idempotent setup: $OUT"; fi

# ---------------------------------------------------------------------------
log "=== leg 5: target start (pidfile mode, pinned binary) ==="
OUT=$("$BIN" nvmeof target start 2>&1); RC=$?
echo "$OUT" > "$STATE/start-out.txt"
if [ $RC -eq 0 ] && echo "$OUT" | grep -q "SPDK target started"; then
    ok "target start"
else die "target start failed: $OUT"; fi
PID=$(cat "$RUN_DIR/spdk_tgt.pid" 2>/dev/null)
manifest "spdk_pid=$PID"
kill -0 "$PID" 2>/dev/null && ok "pidfile pid $PID alive" || bad "pid dead"
SOCK_MODE=$(stat -c %a "$RUN_DIR/spdk.sock" 2>/dev/null)
[ "$SOCK_MODE" = 600 ] && ok "RPC socket mode 0600" || bad "socket mode $SOCK_MODE"
RUN_MODE=$(stat -c %a "$RUN_DIR")
[ "$RUN_MODE" = 700 ] && ok "run dir mode 0700" || bad "run dir mode $RUN_MODE"
[ -s "$RUN_DIR/spdk_tgt.log" ] && ok "spdk_tgt log captured (never null)" || bad "log empty"

OUT=$("$BIN" nvmeof target start 2>&1)
if [ $? -ne 0 ] && echo "$OUT" | grep -q "already running"; then
    ok "second start refuses (already running, pid named)"
else bad "already-running refusal: $OUT"; fi

# ---------------------------------------------------------------------------
log "=== leg 6: target status --json (§6.9 payload) ==="
ST=$("$BIN" nvmeof target status --json 2>/dev/null)
echo "$ST" > "$STATE/status.json"
jq_ok() { echo "$ST" | jq -e "$1" >/dev/null 2>&1; }
jq_ok '.stack == "spdk"' && ok "status.stack" || bad "stack: $ST"
jq_ok ".pinned.tag == \"v26.05\" and .pinned.commit == \"$PIN_SHA\"" \
    && ok "status.pinned tag+commit" || bad "pinned"
jq_ok ".running.mode == \"pidfile\" and .running.pid == $PID" \
    && ok "status.running pidfile mode + pid" || bad "running"
jq_ok '.rpc.live == true and .rpc.drift == false' && ok "status.rpc live, drift=false" || bad "rpc"
jq_ok '.rpc.version | contains("26.05")' && ok "status.rpc.version is the pin" || bad "version"
jq_ok '.rpc.latency_us | type == "number"' && ok "status.rpc.latency_us" || bad "latency"
jq_ok '.reactors | length >= 1 and (.[0].busy_pct | type == "number")' \
    && ok "status.reactors busy_pct sampled" || bad "reactors"
jq_ok '.hugepages.total_2m >= 1024 and .hugepages.dpdk_mem_mb == 1024' \
    && ok "status.hugepages totals + -s from cmdline" || bad "hugepages"
jq_ok '.subsystems == 0 and .ledger.managed == 0 and .ledger.foreign_live == 0' \
    && ok "status: empty target, empty ledger" || bad "counts"

# ---------------------------------------------------------------------------
log "=== leg 7: stop refuses with live consumers; save_config on stop ==="
ZR=$(mkzram $((1024*1024*1024)) n3gate-live)
$RPCPY nvmf_create_transport -t TCP || bad "create_transport"
$RPCPY bdev_aio_create "$ZR" n3gate_aio 4096 || bad "bdev_aio_create"
$RPCPY nvmf_create_subsystem "$NQN_LIVE" -a -s N3GATE01 || bad "create_subsystem"
$RPCPY nvmf_subsystem_add_ns "$NQN_LIVE" n3gate_aio || bad "add_ns"
$RPCPY nvmf_subsystem_add_listener "$NQN_LIVE" -t tcp -a 127.0.0.1 -s $PORT_LIVE -f ipv4 || bad "add_listener"
cat > "$LEDGER_DIR/shares.json" <<EOF
{
  "format": 1,
  "shares": [
    {
      "subnqn": "$NQN_LIVE",
      "stack": "spdk",
      "state": "active",
      "backing_path": "$ZR",
      "backing_canonical": "$ZR",
      "listeners": [{"ip": "127.0.0.1", "port": $PORT_LIVE}],
      "created_utc": "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    }
  ]
}
EOF
modprobe nvme-tcp 2>/dev/null
nvme connect -t tcp -a 127.0.0.1 -s $PORT_LIVE -n "$NQN_LIVE" || bad "initiator connect"
DEV=$(finddev "$NQN_LIVE") || bad "no device for $NQN_LIVE"
log "initiator connected: $DEV"

OUT=$("$BIN" nvmeof target stop 2>&1)
if [ $? -ne 0 ] && echo "$OUT" | grep -q "$NQN_LIVE" && echo "$OUT" | grep -q -- "--force"; then
    ok "stop refuses with a live ledgered consumer (names NQN + --force)"
else bad "live-consumer refusal: $OUT"; fi

nvme disconnect -n "$NQN_LIVE" >/dev/null 2>&1; sleep 1
OUT=$("$BIN" nvmeof target stop 2>&1); RC=$?
echo "$OUT" > "$STATE/stop-out.txt"
if [ $RC -eq 0 ] && echo "$OUT" | grep -q "config saved" && echo "$OUT" | grep -q "SPDK target stopped"; then
    ok "stop: save_config -> TERM"
else bad "stop: $OUT"; fi
kill -0 "$PID" 2>/dev/null && bad "pid $PID survived stop" || ok "spdk_tgt gone"
[ ! -f "$RUN_DIR/spdk_tgt.pid" ] && ok "pidfile removed" || bad "pidfile residue"
[ ! -S "$RUN_DIR/spdk.sock" ] && ok "socket removed" || bad "socket residue"
grep -q "$NQN_LIVE" "$LEDGER_DIR/spdk/tgt-config.json" \
    && ok "tgt-config.json captured the subsystem (SPDK source of truth)" \
    || bad "tgt-config missing the subsystem"

# ---------------------------------------------------------------------------
log "=== leg 8: restart persistence sanity (load_config on start) ==="
OUT=$("$BIN" nvmeof target start 2>&1)
if [ $? -eq 0 ] && echo "$OUT" | grep -q "load_config applied"; then
    ok "start replayed tgt-config.json via load_config"
else bad "restart load_config: $OUT"; fi
PID=$(cat "$RUN_DIR/spdk_tgt.pid"); manifest "spdk_pid=$PID"
ST=$("$BIN" nvmeof target status --json 2>/dev/null)
echo "$ST" | jq -e '.subsystems == 1 and .ledger.managed == 1' >/dev/null \
    && ok "status after restart: subsystem restored, ledger managed=1" \
    || bad "post-restart status: $(echo "$ST" | jq -c '{subsystems, ledger}')"

# Cleanup the fixture through RPC, then stop (config re-saved WITHOUT it).
$RPCPY nvmf_delete_subsystem "$NQN_LIVE" || bad "delete_subsystem"
$RPCPY bdev_aio_delete n3gate_aio || bad "bdev_aio_delete"
rm -f "$LEDGER_DIR/shares.json"
OUT=$("$BIN" nvmeof target stop 2>&1) || bad "final stop: $OUT"
grep -q "$NQN_LIVE" "$LEDGER_DIR/spdk/tgt-config.json" \
    && bad "tgt-config still carries the deleted subsystem" \
    || ok "final stop re-saved the config without the fixture"

OUT=$("$BIN" nvmeof target stop 2>&1)
if [ $? -eq 0 ] && echo "$OUT" | grep -q "nothing to stop"; then
    ok "stop of a stopped target: loud no-op, exit 0"
else bad "idempotent stop: $OUT"; fi

# ---------------------------------------------------------------------------
log "=== leg 9: hugepage restore path ==="
OUT=$("$BIN" nvmeof target setup --restore-prior 2>&1)
if [ $? -eq 0 ] && echo "$OUT" | grep -q "restored to the recorded prior"; then
    ok "restore-prior verb"
else bad "restore-prior: $OUT"; fi
NR=$(cat "$HP_SYSFS/nr_hugepages")
[ "$NR" = "$HP_PRIOR" ] && ok "nr_hugepages back to prior ($HP_PRIOR)" || bad "nr=$NR"
[ ! -f "$LEDGER_DIR/spdk/hugepages-prior" ] && ok "prior record cleared" || bad "record residue"
OUT=$("$BIN" nvmeof target setup --restore-prior 2>&1)
[ $? -ne 0 ] && echo "$OUT" | grep -q "no recorded prior" \
    && ok "second restore refuses loud (no record)" || bad "double restore: $OUT"

# ---------------------------------------------------------------------------
log "=== leg 10: teardown -> zero residue ==="
# zram fixtures out before the snapshot.
grep '^zram=' "$STATE/manifest" | sed 's/^zram=//' | awk '{print $1}' | while read -r idx; do
    [ "$idx" != "0" ] || continue
    [ -b "/dev/zram$idx" ] || continue
    echo 1 > "/sys/block/zram$idx/reset" 2>/dev/null
    echo "$idx" > /sys/class/zram-control/hot_remove 2>/dev/null && log "removed zram$idx"
done
sed -i '/^zram=/d' "$STATE/manifest"
# The /opt prefix this gate created.
find /opt/squeezefs -depth -delete 2>/dev/null && log "removed /opt/squeezefs"
sed -i '/^created_opt_prefix=/d' "$STATE/manifest"

ETC_REG_MD5_AFTER="$(md5sum /etc/squeezefs/nvmeof_shares.json 2>/dev/null || echo absent)"
[ "$ETC_REG_MD5" = "$ETC_REG_MD5_AFTER" ] && ok "/etc/squeezefs registry untouched" \
    || bad "/etc registry mutated"
[ -x "$SCOPING_TGT" ] && ok "sanctioned scoping build untouched" || bad "scoping build gone!"

SNAP_AFTER=$(snapshot_sections after)
if diff -u "$SNAP_BEFORE" "$SNAP_AFTER" > "$STATE/residue.diff"; then
    ok "ZERO residue: before/after snapshots identical"
else
    bad "residue detected:"; cat "$STATE/residue.diff"
fi
TN=$(tctl); log "Tctl end: ${TN:-n/a}°C"

log "=== n3gate result: PASS=$PASS FAIL=$FAIL ==="
[ "$FAIL" = 0 ] || exit 1
exit 0
