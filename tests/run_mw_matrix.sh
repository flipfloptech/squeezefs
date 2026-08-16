#!/usr/bin/env bash
# shellcheck disable=SC2153  # host-side vars interpolated into generated guest scripts (directives cannot reach heredoc bodies; see the vm-hostscope leg)
# tests/run_mw_matrix.sh — the fleet-row emitter (PR 6, rung 6)
# =============================================================================
#
# design-full-multi-writer §5.5: runs one named leg on the LIVE fleet built
# by tests/mw_fleet.sh and emits per-mount stats DELTAS with the mandatory
# columns — a row without its engagement (`dlm_*` / `meta_ship_*` /
# `membership_*`) deltas is INVALID, and every N > 1 row carries the
# R5-PRESSURE columns (`mem_budget_red_events` bounded, `hard_backstops ==
# 0`, `parked_gate_timeouts == 0`) so fleet oversubscription can never be a
# row's silent story. Snapshots are `cat $MNT/.stats` (never cp — the aging
# trap) and are preserved under the fleet state dir per row.
#
# Legs (this rung ships exactly these):
#   smoke               N=2 acceptance: writer (explicit identity, the
#                       proven rung-2 N=1 shape) does basic I/O; the S5
#                       read-only reader observes it coherently WITHIN THE
#                       PUBLISHED STALENESS BOUND (`reader_staleness_bound_ms`
#                       read from the reader's own stats — S5 visibility:
#                       readers do not appear in `squeezefs clients` until
#                       S6 arms, so posture/identity come from each mount's
#                       stats inode); per-mount deltas + R5 columns emitted;
#                       distinct client slots asserted.
#   multipath-negative  a THIRD explicit identity attempts to mount the
#                       shared volume set: on a stock multipath=Y kernel the
#                       only openable meta node is the (merged) subsystem
#                       head, served by the writer's identity — the rung-2
#                       rule-2 refusal MUST fire. Pinned LOOSELY (class +
#                       rule number, not byte-exact: rung 5b part 3 upgrades
#                       the message to name remedies). The leg does NOT add
#                       a second identity path to the live head (a live
#                       writer's I/O would round-robin onto an unregistered
#                       association and be PR-rejected) — the two-identity
#                       merge itself is probed once, pre-mount, by
#                       mw_fleet.sh create (the recorded host_scoped
#                       verdict, which this leg consults: on a 5b
#                       host-scoped kernel the merged-head shape does not
#                       exist and the leg SKIPs loud).
#   cowriters-admission GATED (the 5b gate): the multi-identity legs rungs
#                       7-10 build on. Probes the recorded host-scoped
#                       verdict; on this kernel it SKIPs loud with the
#                       reason + remedy. `--require-host-scoped-subsys`
#                       turns the skip into a hard failure (automation on
#                       5b-kernel boxes). On a capable kernel the body
#                       still refuses: it lands with rungs 7-10.
#   vm-hostscope-validate  (rung 6b — needs a fleet created with --vm=V)
#                       BOOT-VALIDATES sqz kernel patch 0030 inside the
#                       qemu guest (design-mw-multipath-kernel §6):
#                       POSITIVE arm on fleet guest 0 (param=Y): the 5b
#                       probe's param face answers host-scoped=true
#                       in-guest; two identities' connects to ONE subnqn
#                       land in TWO subsystems (distinct sqz_host_scope,
#                       one openable head each, each dir's controller
#                       links carrying only its identity); a same-
#                       identity duplicate_connect still MERGES (same-
#                       identity multipath preserved); dmesg carries no
#                       "duplicate IDs" refusal. NEGATIVE arm on an
#                       ephemeral param-OFF guest (idx 90): the same two
#                       connects MERGE into one subsystem (both hostnqns
#                       under one dir, empty scope) and an explicit-
#                       identity mount over the merged head refuses with
#                       the upgraded rule-2 text naming the shape + both
#                       remedies. All against the RESERVED guest-leg
#                       namespace — no live host writer's subsystem is
#                       ever touched.
#   vm-multi-identity   (rung 6b — needs --vm=V) the N>=2-identity mount
#                       shape LIVE inside guest 0 on the 0030 kernel:
#                       writer A formats --multi-writer over the reserved
#                       guest pair (records name $VM_GW — the guest-
#                       domain fabric address), mounts with explicit
#                       identity (daemon-owned data connects resolve A's
#                       OWN scoped head); writer-candidate B's explicit-
#                       identity mount gets ITS OWN scoped head, passes
#                       rule 2 (the 5b/6b stack unblocks the fabric
#                       layer) and refuses BEYOND identity at the D0
#                       single-writer guard (arming is rungs 7-10 —
#                       posture/admission stay gated). Asserts the two
#                       heads are distinct and B's refusal is NOT the
#                       rule-2 class.
#
# Usage:  sudo tests/run_mw_matrix.sh <leg> [--require-host-scoped-subsys]
# Exit:   0 green (or a loud SKIP), nonzero on any INVALID row / violation.
#
# Requires: root, a live fleet (sudo tests/mw_fleet.sh create N=2), python3.

set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SQZ="${SQZ_BIN:-$REPO/target/release/squeezefs}"
[ -x "$SQZ" ] || SQZ="$REPO/target/debug/squeezefs"

STATE="${SQZ_MWFLEET_STATE_DIR:-/run/squeezefs-mwfleet}"
MEMBERS="$STATE/members.tsv"
CONF="$STATE/config.env"

log() { echo "[mwmatrix] $*"; }
warn() { echo "[mwmatrix] WARN: $*" >&2; }
die() {
    echo "[mwmatrix] ERROR: $*" >&2
    exit 1
}
skip() {
    echo "[mwmatrix] SKIP: $*" >&2
    exit 0
}

ensure_root() {
    [ "$(id -u)" -eq 0 ] && return 0
    log "root required (fleet mounts, stats inodes) — re-executing via sudo"
    local knobs=()
    while IFS= read -r kv; do knobs+=("$kv"); done \
        < <(env | grep -E '^(SQZ_MWFLEET_|SQZ_BIN=)' || true)
    exec sudo env "${knobs[@]}" bash "$0" "$@"
}

LEG="${1:-}"
[ -n "$LEG" ] || {
    awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
    exit 1
}
shift || true
REQUIRE_HS=0
for a in "$@"; do
    case "$a" in
    --require-host-scoped-subsys) REQUIRE_HS=1 ;;
    *) die "unknown argument '$a'" ;;
    esac
done

ensure_root "$LEG" "$@"
command -v python3 >/dev/null 2>&1 || die "python3 is required (the row emitter)"
[ -f "$CONF" ] || die "no live fleet at $STATE — run: sudo tests/mw_fleet.sh create N=2"
# shellcheck disable=SC1090 # generated by mw_fleet.sh create
. "$CONF"
HOST_SCOPED="$(cat "$STATE/host_scoped" 2>/dev/null || echo 0)"

mnt_of() { awk -F'\t' -v i="$1" '$1==i {print $3}' "$MEMBERS"; }
role_of() { awk -F'\t' -v i="$1" '$1==i {print $2}' "$MEMBERS"; }
member_idxs() { awk -F'\t' '{print $1}' "$MEMBERS" | sort -n; }

snap() { # idx phase rowdir  — cat, never cp (the aging trap)
    local mnt
    mnt="$(mnt_of "$1")"
    cat "$mnt/.stats" >"$3/m$1_p$2.json" ||
        die "cannot snapshot member $1's stats inode"
}

stat_field() { # idx json_key -> value (flattened key)
    local mnt
    mnt="$(mnt_of "$1")"
    cat "$mnt/.stats" | python3 -c '
import json, sys
def flat(d, out, pfx=""):
    for k, v in d.items():
        if isinstance(v, dict): flat(v, out, pfx + k + ".")
        else: out[pfx + k] = v
    return out
root = json.load(sys.stdin)
d = flat(root.get("metrics", root), {})  # stats nest under "metrics"
print(d.get(sys.argv[1], ""))' "$2"
}

# --- the row emitter ---------------------------------------------------------
# Per-mount deltas + the §5.5 mandatory columns; exits nonzero on any
# INVALID row (missing engagement, R5 tripwire movement, dlm_rpcs != 0).
emit_rows() { # rowdir leg idx...
    local rowdir="$1" leg="$2"
    shift 2
    python3 - "$rowdir" "$leg" "$@" <<'PYEOF'
import json, sys

rowdir, leg, idxs = sys.argv[1], sys.argv[2], sys.argv[3:]

def flat(d, out=None, pfx=""):
    out = {} if out is None else out
    for k, v in d.items():
        if isinstance(v, dict):
            flat(v, out, pfx + k + ".")
        else:
            out[pfx + k] = v
    return out

def num(v):
    return v if isinstance(v, (int, float)) else 0

# The §5.5 mandatory columns. Engagement: this rung's fleet runs every
# distributed plane dark by construction, so the dlm_*/meta_ship_*/
# membership_* columns are asserted-zero rows, not omitted rows.
DELTA_COLS = [
    ("meta_kv_journal_entries", "jrnl_d"),
    ("write_through_bytes", "wt_bytes_d"),
    ("meta_kv_revalidate_polls", "reval_polls_d"),
    ("meta_kv_revalidate_epochs", "reval_epochs_d"),
    ("meta_kv_revalidate_dirty_skips", "reval_dirty_d"),
    ("membership_renewals", "memb_renew_d"),
    ("meta_ship.shipped_verbs", "ship_d"),
    ("mem_budget_red_events", "r5_red_d"),
    ("mem_budget_hard_backstops", "r5_backstop_d"),
    ("parked_gate_timeouts", "r5_gate_to_d"),
    ("invariant_tripwires", "tripwire_d"),
]
GAUGE_COLS = [
    ("mount_posture", "posture"),
    ("client_slot", "slot"),
    ("dlm_rpcs", "dlm_rpcs"),
    ("dlm_mode", "dlm_mode"),
    ("membership_mode", "memb_mode"),
    ("mem_budget_level", "r5_lvl"),
    ("reader_staleness_bound_ms", "stale_ms"),
]

def load(path):  # the stats JSON nests under a top-level "metrics" object
    root = json.load(open(path))
    return flat(root.get("metrics", root))

rows, violations, slots = [], [], {}
for i in idxs:
    p0 = load(f"{rowdir}/m{i}_p0.json")
    p1 = load(f"{rowdir}/m{i}_p1.json")
    row = {"m": i}
    for key, col in DELTA_COLS:
        row[col] = num(p1.get(key, 0)) - num(p0.get(key, 0))
    for key, col in GAUGE_COLS:
        row[col] = p1.get(key, "-")
    rows.append(row)
    slots[i] = row["slot"]

    # Row validity (§5.5): R5 tripwires + the dark-plane invariants.
    if row["r5_backstop_d"] != 0 or num(p1.get("mem_budget_hard_backstops", 0)) != 0:
        violations.append(f"m{i}: mem_budget_hard_backstops moved (R5 column)")
    if row["r5_gate_to_d"] != 0:
        violations.append(f"m{i}: parked_gate_timeouts moved (R5 column)")
    if num(p1.get("dlm_rpcs", 0)) != 0:
        violations.append(f"m{i}: dlm_rpcs != 0 (solo-invariant violated)")
    if row["tripwire_d"] != 0:
        violations.append(f"m{i}: invariant_tripwires moved")
    if num(p1.get("meta_kv_revalidate_dirty_skips", 0)) != 0:
        violations.append(
            f"m{i}: meta_kv_revalidate_dirty_skips != 0 — the FIXED rung-6 "
            "pinned-node finding regressed (must stay 0 on every posture; "
            "cargo pin readonly_mount_tests::"
            "reader_bootstrap_into_a_dirty_journal_tail_never_pins_nodes)"
        )
    posture = p1.get("mount_posture", "?")
    if posture == "writer" and row["jrnl_d"] <= 0:
        violations.append(f"m{i}: writer emitted no journal entries (row not engaged)")
    if posture == "reader" and row["reval_polls_d"] <= 0:
        violations.append(f"m{i}: reader revalidation never polled (row not engaged)")

if len(set(slots.values())) != len(slots):
    violations.append(f"client slots not distinct: {slots}")

cols = ["m"] + [c for _, c in GAUGE_COLS] + [c for _, c in DELTA_COLS]
widths = {c: max(len(c), max((len(str(r[c])) for r in rows), default=0)) for c in cols}
print(f"== {leg} rows (deltas p0->p1; engagement + R5-pressure columns) ==")
print("  ".join(c.ljust(widths[c]) for c in cols))
for r in rows:
    print("  ".join(str(r[c]).ljust(widths[c]) for c in cols))

if violations:
    print("INVALID ROW(S):", file=sys.stderr)
    for v in violations:
        print(f"  {v}", file=sys.stderr)
    sys.exit(1)
print("rows VALID (R5 tripwires flat, dark planes at 0, per-role engagement present)")
PYEOF
}

# --- legs --------------------------------------------------------------------
leg_smoke() {
    local rowdir
    rowdir="$STATE/rows/smoke-$(date +%s)"
    mkdir -p "$rowdir"
    local w_mnt r_idx r_mnt
    w_mnt="$(mnt_of 0)"
    [ -n "$w_mnt" ] || die "no writer member"
    [ "$(role_of 0)" = "writer" ] || die "member 0 is not the writer"
    r_idx="$(member_idxs | awk '$1!=0' | head -1)"
    [ -n "$r_idx" ] || die "smoke needs N>=2 (a reader member)"
    r_mnt="$(mnt_of "$r_idx")"

    # Identity/posture assertions — writer via `squeezefs clients` (the D0
    # claim + heartbeat record), reader via its OWN stats inode (S5: readers
    # are invisible to `clients` until S6 arms — stated in the header).
    local meta0 clients_out
    meta0="${FORMAT_META_PATHS%%,*}"
    : "$meta0" # clients probes the CURRENT writer-identity paths:
    clients_out="$("$SQZ" clients "sqmeta://$META_PATHS" 2>/dev/null)" ||
        die "squeezefs clients probe failed"
    echo "$clients_out" >"$rowdir/clients.out"
    echo "$clients_out" | grep -q "live" ||
        die "writer not visible live in squeezefs clients:
$clients_out"
    [ "$(stat_field 0 mount_posture)" = "writer" ] || die "member 0 posture != writer"
    [ "$(stat_field "$r_idx" mount_posture)" = "reader" ] ||
        die "member $r_idx posture != reader"
    [ "$(stat_field "$r_idx" read_only_mount)" = "True" ] ||
        die "member $r_idx is not a read-only mount"
    local w_slot r_slot
    w_slot="$(stat_field 0 client_slot)"
    r_slot="$(stat_field "$r_idx" client_slot)"
    [ -n "$w_slot" ] && [ -n "$r_slot" ] && [ "$w_slot" != "$r_slot" ] ||
        die "client slots not distinct (writer=$w_slot reader=$r_slot)"
    log "identities: writer slot=$w_slot (clients: live), reader slot=$r_slot (stats: posture=reader, ro=true)"

    local bound_ms
    bound_ms="$(stat_field "$r_idx" reader_staleness_bound_ms)"
    [[ "$bound_ms" =~ ^[0-9]+$ ]] && [ "$bound_ms" -gt 0 ] ||
        die "reader publishes no staleness bound (got '$bound_ms')"

    # p0 → writer I/O → reader coherence within the published bound → p1.
    local i
    for i in $(member_idxs); do snap "$i" 0 "$rowdir"; done

    mkdir -p "$w_mnt/mwsmoke"
    dd if=/dev/urandom of="$w_mnt/mwsmoke/coh.dat" bs=64K count=32 conv=fsync \
        status=none || die "writer I/O failed"
    local want_sum t0 now deadline got_sum="" observed_ms=-1
    want_sum="$(sha256sum "$w_mnt/mwsmoke/coh.dat" | awk '{print $1}')"
    t0="$(date +%s%3N)"
    # Deadline: the published bound + the reader's 1 s kernel attr/entry TTL
    # + grace. Exceeding it is a FAILED row, not a retry.
    deadline=$((bound_ms + 1000 + 5000))
    while :; do
        now="$(date +%s%3N)"
        if [ -f "$r_mnt/mwsmoke/coh.dat" ]; then
            got_sum="$(sha256sum "$r_mnt/mwsmoke/coh.dat" 2>/dev/null | awk '{print $1}')" || got_sum=""
            if [ "$got_sum" = "$want_sum" ]; then
                observed_ms=$((now - t0))
                break
            fi
        fi
        [ $((now - t0)) -lt "$deadline" ] ||
            die "reader did not observe the writer's data within ${deadline}ms (published staleness bound ${bound_ms}ms + TTL + grace) — coherence FAILED"
        sleep 0.2
    done
    log "coherence: reader observed the write in ${observed_ms}ms (published bound ${bound_ms}ms + 1000ms kernel TTL; sha256 match)"

    # Let the reader's revalidation cadence tick at least once more so the
    # engagement column is unambiguous, then p1.
    sleep 2
    for i in $(member_idxs); do snap "$i" 1 "$rowdir"; done
    # shellcheck disable=SC2046 # member_idxs is a controlled numeric list
    emit_rows "$rowdir" smoke $(member_idxs)
    log "smoke leg GREEN (rows + snapshots preserved in $rowdir)"
}

leg_multipath_negative() {
    if [ "$HOST_SCOPED" = "1" ]; then
        skip "this kernel scopes fabric subsystems by host identity (rung 5b present) — the merged-head refusal shape does not exist here; the 5b kernel's own validation legs live in rung 6b"
    fi
    local rowdir
    rowdir="$STATE/rows/mpneg-$(date +%s)"
    mkdir -p "$rowdir"
    local hostid hostnqn mnt out rc=0
    hostid="$(printf 'cafef1e7-%04d-4000-8000-%012d' 91 "$CREATE_PID")"
    hostnqn="nqn.2014-08.org.nvmexpress:uuid:$hostid"
    mnt="$(mktemp -d /tmp/sqz-mwneg-XXXXXX)"
    # The merged head: the meta paths are subsystem head nodes served by the
    # WRITER's identity (the create-time probe recorded that a second
    # identity MERGES rather than getting its own subsystem). A second
    # explicit identity's mount attempt must refuse with the rule-2 class.
    out="$(timeout 120 "$SQZ" mount "sqmeta://$META_PATHS" "$mnt" \
        -o "hostnqn=$hostnqn,hostid=$hostid" 2>&1)" || rc=$?
    echo "$out" >"$rowdir/refusal.out"
    if [ "$rc" -eq 0 ] || mountpoint -q "$mnt"; then
        "$SQZ" umount "$mnt" >/dev/null 2>&1 || umount -l "$mnt" 2>/dev/null || true
        rmdir "$mnt" 2>/dev/null || true
        die "a second explicit identity MOUNTED on the merged head — the rule-2 refusal did not fire"
    fi
    # Loose pin: the refusal CLASS + rule number (rung 5b part 3 upgrades
    # the message text to name the shape + remedies — do not pin bytes).
    if ! echo "$out" | grep -Eq 'mount refused \(rule 2'; then
        rmdir "$mnt" 2>/dev/null || true
        die "mount refused, but not with the rule-2 class:
$out"
    fi
    rmdir "$mnt" 2>/dev/null || true
    log "second explicit identity refused with the rule-2 class (refusal preserved in $rowdir/refusal.out)"
    log "multipath-negative leg GREEN"
}

# --- rung-6b guest legs --------------------------------------------------
MWFLEET="$REPO/tests/mw_fleet.sh"

require_vm_fleet() {
    [ "${VM_COUNT:-0}" -ge 1 ] ||
        die "this leg needs a fleet created with --vm=V (sudo tests/mw_fleet.sh create N=2 --vm=1) — the 0030 kernel boots only in the qemu guest"
    [ -n "${GUEST_META_NQN:-}" ] || die "fleet config carries no reserved guest-leg meta NQN"
}

guest_id() { printf 'cafef1e7-%04d-4000-8000-%012d' "$1" "$CREATE_PID"; }
guest_nqn() { echo "nqn.2014-08.org.nvmexpress:uuid:$(guest_id "$1")"; }

# The busybox-sh helper preamble every in-guest job shares: the scoped-
# subsystem walk (head + controller-link census) in shell.
guest_job_preamble() {
    cat <<PREAMBLE
set -e
export LD_LIBRARY_PATH=/share/lib
SQZ=/share/bin/squeezefs
GW='$VM_GW'
SVC='$TCP_SVC'
subsys_dirs_for_nqn() { # nqn -> subsystem dir paths
    for s in /sys/class/nvme-subsystem/nvme-subsys*; do
        [ -r "\$s/subsysnqn" ] || continue
        [ "\$(cat "\$s/subsysnqn")" = "\$1" ] && echo "\$s"
    done
}
head_of_dir() { # subsystem dir -> head name (strict nvme<X>n<Y>)
    for c in "\$1"/nvme*; do
        b=\$(basename "\$c")
        echo "\$b" | grep -qE '^nvme[0-9]+n[0-9]+\$' && { echo "\$b"; return 0; }
    done
    return 1
}
ctrl_links_of_dir() { # subsystem dir -> "ctrl:hostnqn" lines
    for c in "\$1"/nvme*; do
        b=\$(basename "\$c")
        echo "\$b" | grep -qE '^nvme[0-9]+\$' || continue
        echo "\$b:\$(cat "\$c/hostnqn" 2>/dev/null)"
    done
}
head_for_scope() { # nqn hostnqn-scope -> head name
    for s in \$(subsys_dirs_for_nqn "\$1"); do
        [ "\$(cat "\$s/sqz_host_scope" 2>/dev/null)" = "\$2" ] || continue
        head_of_dir "\$s" && return 0
    done
    return 1
}
disconnect_nqn() { # nqn — delete every controller serving it
    for c in /sys/class/nvme/nvme*; do
        [ "\$(cat "\$c/subsysnqn" 2>/dev/null)" = "\$1" ] || continue
        echo 1 >"\$c/delete_controller" 2>/dev/null || true
    done
    sleep 1
}
PREAMBLE
}

leg_vm_hostscope_validate() {
    require_vm_fleet
    local rowdir a_nqn a_id b_nqn b_id
    rowdir="$STATE/rows/vmhs-$(date +%s)"
    mkdir -p "$rowdir"
    a_nqn="$(guest_nqn 80)" a_id="$(guest_id 80)"
    b_nqn="$(guest_nqn 81)" b_id="$(guest_id 81)"

    # ---------------- POSITIVE arm: fleet guest 0 (param=Y) ----------------
    log "positive arm: 0030 grouping proof in fleet guest 0 (param=Y)"
    {
        guest_job_preamble
        cat <<POS
P=/sys/module/nvme_core/parameters/fabrics_host_scoped_subsystems
[ -r "\$P" ] || { echo "FAIL: 0030 module param file absent — not the patched kernel"; exit 1; }
echo "param fabrics_host_scoped_subsystems=\$(cat \$P)"
[ "\$(cat \$P)" = "Y" ] || { echo "FAIL: param not Y on the fleet guest"; exit 1; }
# The rung-6 5b probe's PARAM FACE (mw_fleet.sh probe_host_scoped arm a),
# verbatim glob — must answer host-scoped=true in-guest:
probe=0
for f in /sys/module/nvme_core/parameters/*host*scope* /sys/module/nvme_core/parameters/*scope*host*; do
    [ -r "\$f" ] || continue
    case "\$(cat "\$f")" in Y|y|1) probe=1 ;; esac
done
echo "5b-probe-param-face: host_scoped=\$probe"
[ "\$probe" = 1 ] || { echo "FAIL: the 5b probe would not unlock multi-identity legs here"; exit 1; }
NQN='$GUEST_META_NQN'
\$SQZ nvmeof connect --ip "\$GW" --port "\$SVC" --subnqn "\$NQN" --hostnqn '$a_nqn' --hostid '$a_id'
\$SQZ nvmeof connect --ip "\$GW" --port "\$SVC" --subnqn "\$NQN" --hostnqn '$b_nqn' --hostid '$b_id'
sleep 1
echo "=== scoped census (two identities, one subnqn) ==="
count=0
scopes=""
for s in \$(subsys_dirs_for_nqn "\$NQN"); do
    count=\$((count + 1))
    scope=\$(cat "\$s/sqz_host_scope" 2>/dev/null)
    head=\$(head_of_dir "\$s") || { echo "FAIL: subsystem \$s has no openable head"; exit 1; }
    [ -b "/dev/\$head" ] || { echo "FAIL: /dev/\$head is not a block device"; exit 1; }
    heads_n=0
    for c in "\$s"/nvme*; do b=\$(basename "\$c"); echo "\$b" | grep -qE '^nvme[0-9]+n[0-9]+\$' && heads_n=\$((heads_n + 1)); done
    [ "\$heads_n" = 1 ] || { echo "FAIL: subsystem \$s carries \$heads_n heads (want 1)"; exit 1; }
    links=\$(ctrl_links_of_dir "\$s")
    echo "SUBSYS \$(basename "\$s") scope=\$scope head=\$head ctrls: \$links"
    [ -n "\$links" ] || { echo "FAIL: subsystem \$s carries no controller links"; exit 1; }
    for l in \$links; do
        [ "\${l#*:}" = "\$scope" ] || { echo "FAIL: controller \$l under scope \$scope — the dir is NOT identity-dedicated"; exit 1; }
    done
    scopes="\$scopes \$scope"
done
echo "subsys_count=\$count scopes=\$scopes"
[ "\$count" = 2 ] || { echo "FAIL: want TWO host-scoped sibling subsystems, got \$count"; exit 1; }
echo "\$scopes" | grep -q '$a_nqn' || { echo "FAIL: identity A's scope missing"; exit 1; }
echo "\$scopes" | grep -q '$b_nqn' || { echo "FAIL: identity B's scope missing"; exit 1; }
if dmesg | grep -i "duplicate IDs"; then
    echo "FAIL: the kernel refused a scoped sibling's namespace as a duplicate ID (the 0030 dup-ID skip did not engage)"
    exit 1
fi
echo "=== same-identity multipath preservation (duplicate_connect) ==="
printf 'transport=tcp,traddr=%s,trsvcid=%s,nqn=%s,hostnqn=%s,hostid=%s,duplicate_connect' \
    "\$GW" "\$SVC" "\$NQN" '$a_nqn' '$a_id' >/dev/nvme-fabrics
sleep 1
count2=0
for s in \$(subsys_dirs_for_nqn "\$NQN"); do count2=\$((count2 + 1)); done
[ "\$count2" = 2 ] || { echo "FAIL: same-identity second path minted a THIRD subsystem (\$count2)"; exit 1; }
a_ctrls=0
for s in \$(subsys_dirs_for_nqn "\$NQN"); do
    [ "\$(cat "\$s/sqz_host_scope" 2>/dev/null)" = '$a_nqn' ] || continue
    for l in \$(ctrl_links_of_dir "\$s"); do a_ctrls=\$((a_ctrls + 1)); done
    heads_n=0
    for c in "\$s"/nvme*; do b=\$(basename "\$c"); echo "\$b" | grep -qE '^nvme[0-9]+n[0-9]+\$' && heads_n=\$((heads_n + 1)); done
    [ "\$heads_n" = 1 ] || { echo "FAIL: A's subsystem grew a second head"; exit 1; }
done
[ "\$a_ctrls" = 2 ] || { echo "FAIL: A's subsystem carries \$a_ctrls controller links (want 2 — N paths, one identity, ONE subsystem)"; exit 1; }
echo "same-identity multipath preserved: 2 paths, 1 subsystem, 1 head"
disconnect_nqn "\$NQN"
echo "POSITIVE ARM GREEN"
POS
    } >"$rowdir/pos-arm.sh"
    "$MWFLEET" vm-exec 0 "$rowdir/pos-arm.sh" 420 | tee "$rowdir/pos-arm.out" ||
        die "positive arm FAILED (output: $rowdir/pos-arm.out)"

    # ------------- NEGATIVE arm: ephemeral param-OFF guest (idx 90) --------
    log "negative arm: param-off merge control on ephemeral guest 90"
    "$MWFLEET" vm-boot 90 --no-hostscope
    {
        guest_job_preamble
        cat <<NEG
P=/sys/module/nvme_core/parameters/fabrics_host_scoped_subsystems
[ -r "\$P" ] || { echo "FAIL: param file absent — not the patched kernel"; exit 1; }
echo "param fabrics_host_scoped_subsystems=\$(cat \$P)"
[ "\$(cat \$P)" = "N" ] || { echo "FAIL: negative arm expects the param OFF"; exit 1; }
NQN='$GUEST_META_NQN'
\$SQZ nvmeof connect --ip "\$GW" --port "\$SVC" --subnqn "\$NQN" --hostnqn '$a_nqn' --hostid '$a_id'
\$SQZ nvmeof connect --ip "\$GW" --port "\$SVC" --subnqn "\$NQN" --hostnqn '$b_nqn' --hostid '$b_id'
sleep 1
echo "=== merged census (param off) ==="
count=0
merged_dir=""
for s in \$(subsys_dirs_for_nqn "\$NQN"); do
    count=\$((count + 1))
    merged_dir="\$s"
    echo "SUBSYS \$(basename "\$s") scope='\$(cat "\$s/sqz_host_scope" 2>/dev/null)' ctrls: \$(ctrl_links_of_dir "\$s")"
done
[ "\$count" = 1 ] || { echo "FAIL: param-off control expects ONE merged subsystem, got \$count"; exit 1; }
[ -z "\$(cat "\$merged_dir/sqz_host_scope" 2>/dev/null)" ] || { echo "FAIL: scope not empty with the param off"; exit 1; }
ctrl_links_of_dir "\$merged_dir" | grep -q '$a_nqn' || { echo "FAIL: A's controller missing from the merged dir"; exit 1; }
ctrl_links_of_dir "\$merged_dir" | grep -q '$b_nqn' || { echo "FAIL: B's controller missing from the merged dir"; exit 1; }
head=\$(head_of_dir "\$merged_dir") || { echo "FAIL: merged subsystem has no head"; exit 1; }
echo "merged shape reproduced: 1 subsystem, head \$head, 2 hostnqns"
echo "=== upgraded rule-2 refusal over the merged head ==="
# The mount reads the format config BEFORE the identity ladder — format
# the reserved pair first so the probe reaches the ladder (offline
# format over the merged head is fine; no identity in play).
# GUEST_DATA_NQN is a HOST-side variable, interpolated at guest-script
# generation time (the unescaped dollar in this expanding heredoc) —
# not a misspelling of GUEST_META_NQN (SC2153 disabled file-wide;
# directives cannot reach heredoc bodies).
DNQN='$GUEST_DATA_NQN'
\$SQZ nvmeof connect --ip "\$GW" --port "\$SVC" --subnqn "\$DNQN" --hostnqn '$a_nqn' --hostid '$a_id'
sleep 1
dhead=""
for s in \$(subsys_dirs_for_nqn "\$DNQN"); do dhead=\$(head_of_dir "\$s") && break; done
[ -n "\$dhead" ] || { echo "FAIL: no head for the reserved data NQN"; exit 1; }
\$SQZ format "sqmeta:///dev/\$head" "sqdata:///dev/\$dhead" --force >/tmp/format.out 2>&1 || { cat /tmp/format.out; exit 1; }
mkdir -p /mnt/neg
rc=0
timeout 90 \$SQZ mount "sqmeta:///dev/\$head" /mnt/neg -o 'hostnqn=$a_nqn,hostid=$a_id' >/tmp/refusal.out 2>&1 || rc=\$?
cat /tmp/refusal.out
grep -q " /mnt/neg " /proc/mounts && { echo "FAIL: mounted on the merged head"; exit 1; }
[ "\$rc" != 0 ] || { echo "FAIL: mount exited 0"; exit 1; }
grep -q 'MULTIPATH-MERGED' /tmp/refusal.out || { echo "FAIL: refusal does not name the shape"; exit 1; }
grep -q 'fabrics_host_scoped_subsystems=Y' /tmp/refusal.out || { echo "FAIL: refusal does not name the sqz-kernel remedy"; exit 1; }
grep -q 'multipath=N' /tmp/refusal.out || { echo "FAIL: refusal does not name the stock workaround"; exit 1; }
disconnect_nqn "\$NQN"
disconnect_nqn "\$DNQN"
echo "NEGATIVE ARM GREEN"
NEG
    } >"$rowdir/neg-arm.sh"
    local neg_rc=0
    "$MWFLEET" vm-exec 90 "$rowdir/neg-arm.sh" 420 | tee "$rowdir/neg-arm.out" || neg_rc=$?
    "$MWFLEET" vm-stop 90
    [ "$neg_rc" = 0 ] || die "negative arm FAILED (output: $rowdir/neg-arm.out)"
    log "vm-hostscope-validate GREEN (both arms; evidence in $rowdir)"
}

leg_vm_multi_identity() {
    require_vm_fleet
    local rowdir a_nqn a_id b_nqn b_id
    rowdir="$STATE/rows/vmmid-$(date +%s)"
    mkdir -p "$rowdir"
    a_nqn="$(guest_nqn 85)" a_id="$(guest_id 85)"
    b_nqn="$(guest_nqn 86)" b_id="$(guest_id 86)"

    # ---- job 1: writer A — the full explicit-identity mount, in-guest ----
    {
        guest_job_preamble
        cat <<JOBA
P=/sys/module/nvme_core/parameters/fabrics_host_scoped_subsystems
[ "\$(cat \$P 2>/dev/null)" = "Y" ] || { echo "FAIL: this leg needs the 0030 kernel armed"; exit 1; }
MNQN='$GUEST_META_NQN'
DNQN='$GUEST_DATA_NQN'
\$SQZ nvmeof connect --ip "\$GW" --port "\$SVC" --subnqn "\$MNQN" --hostnqn '$a_nqn' --hostid '$a_id'
\$SQZ nvmeof connect --ip "\$GW" --port "\$SVC" --subnqn "\$DNQN" --hostnqn '$a_nqn' --hostid '$a_id'
sleep 1
MH=\$(head_for_scope "\$MNQN" '$a_nqn') || { echo "FAIL: no scoped meta head for A"; exit 1; }
DH=\$(head_for_scope "\$DNQN" '$a_nqn') || { echo "FAIL: no scoped data head for A"; exit 1; }
echo "A heads: meta=/dev/\$MH data=/dev/\$DH"
\$SQZ format --multi-writer "sqmeta:///dev/\$MH" "sqdata:///dev/\$DH" --force >/tmp/format.out 2>&1 || { cat /tmp/format.out; exit 1; }
VOL=\$(\$SQZ volume list "sqmeta:///dev/\$MH" | awk -v b="/dev/\$DH" 'NR>1 && \$NF==b {print \$1}')
[ -n "\$VOL" ] || { echo "FAIL: no durable volume id for /dev/\$DH"; \$SQZ volume list "sqmeta:///dev/\$MH"; exit 1; }
# Records carry the GUEST-DOMAIN fabric address (\$GW — THE VM LEG note).
ep_ok=0
for t in 1 2 3 4 5; do
    if \$SQZ config set-fabric-endpoints "sqmeta:///dev/\$MH" "\$VOL=\$GW:\$SVC:\$DNQN" >/tmp/ep.out 2>&1; then ep_ok=1; break; fi
    grep -q "holds the writer lock" /tmp/ep.out || { cat /tmp/ep.out; exit 1; }
    sleep 2
done
[ "\$ep_ok" = 1 ] || { echo "FAIL: set-fabric-endpoints never cleared the post-format guard"; cat /tmp/ep.out; exit 1; }
# Un-pre-connect the DATA plane: the writer's daemon-owned connect is the point.
disconnect_nqn "\$DNQN"
mkdir -p /mnt/a
\$SQZ mount "sqmeta:///dev/\$MH" /mnt/a -o 'hostnqn=$a_nqn,hostid=$a_id' --daemon --log-file /tmp/a.log >/tmp/a.mount.out 2>&1 || { cat /tmp/a.mount.out; exit 1; }
i=0
while [ \$i -lt 240 ]; do grep -q " /mnt/a " /proc/mounts && break; i=\$((i + 1)); sleep 0.5; done
grep -q " /mnt/a " /proc/mounts || { echo "FAIL: writer A never mounted"; cat /tmp/a.log; exit 1; }
grep -q "daemon-owned controller resolved" /tmp/a.log || { echo "FAIL: no daemon-owned connect line (rung-2 engagement)"; exit 1; }
found=0
for c in /sys/class/nvme/nvme*; do
    [ "\$(cat "\$c/subsysnqn" 2>/dev/null)" = "\$DNQN" ] || continue
    [ "\$(cat "\$c/hostnqn" 2>/dev/null)" = '$a_nqn' ] && found=1
done
[ "\$found" = 1 ] || { echo "FAIL: no data controller under A's identity post-mount"; exit 1; }
echo mw-guest-proof >/mnt/a/proof.txt && sync
grep -q '"mount_posture": *"writer"' /mnt/a/.stats || { echo "FAIL: posture != writer"; exit 1; }
echo "A_HEAD=\$MH"
echo "WRITER A GREEN (mounted, daemon-owned data connect under A, posture=writer)"
JOBA
    } >"$rowdir/job-a.sh"
    "$MWFLEET" vm-exec 0 "$rowdir/job-a.sh" 600 | tee "$rowdir/job-a.out" ||
        die "writer-A job FAILED (output: $rowdir/job-a.out)"
    local a_head
    a_head="$(awk -F= '/^A_HEAD=/ {print $2}' "$rowdir/job-a.out" | tr -d '\r')"
    [ -n "$a_head" ] || die "writer-A job reported no head"

    # ---- job 2: writer-candidate B — past rule 2, refused beyond identity ----
    {
        guest_job_preamble
        cat <<JOBB
MNQN='$GUEST_META_NQN'
\$SQZ nvmeof connect --ip "\$GW" --port "\$SVC" --subnqn "\$MNQN" --hostnqn '$b_nqn' --hostid '$b_id'
sleep 1
BH=\$(head_for_scope "\$MNQN" '$b_nqn') || { echo "FAIL: no scoped meta head for B"; exit 1; }
echo "B head: /dev/\$BH (A's was /dev/$a_head)"
[ "\$BH" != '$a_head' ] || { echo "FAIL: B resolved A's head — identities merged"; exit 1; }
mkdir -p /mnt/b
rc=0
timeout 120 \$SQZ mount "sqmeta:///dev/\$BH" /mnt/b -o 'hostnqn=$b_nqn,hostid=$b_id' >/tmp/b.out 2>&1 || rc=\$?
echo "=== B refusal (rc=\$rc) ==="
cat /tmp/b.out
grep -q " /mnt/b " /proc/mounts && { echo "FAIL: writer-candidate B MOUNTED under a live writer"; exit 1; }
[ "\$rc" != 0 ] || { echo "FAIL: B's mount exited 0"; exit 1; }
# The rule-2 pin matches the REFUSAL class only (the KD-MW-3 engagement
# banner legitimately says "verified (rule 2)"): both the ladder's
# "mount refused (rule 2" and the daemon-connect resolution family
# ("produced no namespace under this mount's identity" / "the fabric is
# mis-sharing the subsystem") are fabric-layer blocks.
if grep -Eq 'mount refused \(rule 2|produced no namespace under this|mis-sharing the subsystem' /tmp/b.out; then
    echo "FAIL: B refused AT the fabric layer — scoped-sibling resolution regressed"
    exit 1
fi
grep -Eqi 'claimed by a live writer|holds the writer lock|single-writer' /tmp/b.out ||
    { echo "FAIL: B's refusal is not the D0 writer-guard class (beyond identity)"; exit 1; }
echo "WRITER-CANDIDATE B GREEN (own scoped head, PAST rule 2, refused at the single-writer guard)"
JOBB
    } >"$rowdir/job-b.sh"
    local b_rc=0
    "$MWFLEET" vm-exec 0 "$rowdir/job-b.sh" 600 | tee "$rowdir/job-b.out" || b_rc=$?

    # ---- job 3: cleanup (always) ----
    {
        guest_job_preamble
        cat <<JOBC
set +e
\$SQZ umount /mnt/a >/dev/null 2>&1 || umount -l /mnt/a 2>/dev/null
i=0
while [ \$i -lt 120 ]; do grep -q " /mnt/a " /proc/mounts || break; i=\$((i + 1)); sleep 0.5; done
disconnect_nqn '$GUEST_META_NQN'
disconnect_nqn '$GUEST_DATA_NQN'
echo "cleanup done"
exit 0
JOBC
    } >"$rowdir/job-cleanup.sh"
    "$MWFLEET" vm-exec 0 "$rowdir/job-cleanup.sh" 300 | tee "$rowdir/job-cleanup.out" ||
        warn "in-guest cleanup reported errors"
    [ "$b_rc" = 0 ] || die "writer-candidate-B job FAILED (output: $rowdir/job-b.out)"
    log "vm-multi-identity GREEN (evidence in $rowdir)"
}

leg_cowriters_admission() {
    if [ "$HOST_SCOPED" != "1" ]; then
        local reason="multi-identity (co-writer) legs need host-scoped fabric subsystems: this kernel merges controllers by subsysnqn ignoring hostnqn (nvme_core.multipath=Y), so co-located identities share one head — rung 5b (the sqz-kernel fix, validated in the rung-6b qemu guest) unlocks them. Stock-kernel workaround: nvme_core.multipath=N (boot parameter)"
        [ "$REQUIRE_HS" = "1" ] && die "--require-host-scoped-subsys: $reason"
        skip "$reason"
    fi
    die "host-scoped subsystems present, but the co-writer leg bodies land with rungs 7-10 (S6 arm onward) — this rung ships only the gate"
}

case "$LEG" in
smoke) leg_smoke ;;
multipath-negative) leg_multipath_negative ;;
cowriters-admission) leg_cowriters_admission ;;
vm-hostscope-validate) leg_vm_hostscope_validate ;;
vm-multi-identity) leg_vm_multi_identity ;;
*) die "unknown leg '$LEG' (smoke|multipath-negative|cowriters-admission|vm-hostscope-validate|vm-multi-identity)" ;;
esac
