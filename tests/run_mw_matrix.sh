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
#   s6-journal [--window=S]  (rung 7 — needs a fleet created with
#                       --membership; design row S6-a) the S6 gate LIVE:
#                       a sustained quiet window over the whole fleet in
#                       which `membership_renewals` grows with the
#                       heartbeat while `meta_kv_journal_entries` does NOT
#                       grow proportionally (the pre-S6 plane paid ONE
#                       journal transaction PER BEAT — spec §6.5 item 3's
#                       455 beats/s serialization); registration commits
#                       stay flat, the census serves engage (`squeezefs
#                       clients` probes during the window), self-fences/
#                       evictions stay 0, and every row carries the §5.5
#                       R5-pressure columns. Default window 600 s
#                       (SQZ_MWMATRIX_S6_WINDOW_S / --window=S override —
#                       shorter windows are labeled in the row).
#   s6-fence [--netem=MS] [--victim=IDX]  (rung 7; design row S6-b) the
#                       self-fence clock law under duress: the victim
#                       reader is remounted into its own netns with netem
#                       delay (default 200 ms per veth end), SIGSTOPped
#                       past its T_self and past the owner's TTL; the
#                       owner must evict (+ mint the S7 dead epoch) while
#                       the victim is frozen, and the victim must
#                       SELF-FENCE + purge on resume — self_fences=1 on
#                       the victim, 0 elsewhere, grace refusals 0.
#   s6-vm-fence         (rung 7 — needs --vm=V + --membership; design row
#                       S6-b') the HUNG-KERNEL shape: guest 0 joins the
#                       host fleet as a READ-ONLY member over the fabric
#                       (in-guest operator connects reproduce the
#                       format-time instance numbering), `mw_fleet.sh
#                       pause` freezes the guest kernel past the owner's
#                       TTL (its monotonic domain cannot observe T_self —
#                       the shape kill-9 cannot produce), and on resume
#                       the guest must observe itself dead and self-fence
#                       (purge) BEFORE holding any fresh lease — never
#                       resume as a live member on its stale caches. The
#                       owner side must have evicted + minted the S7 dead
#                       epoch. Real halves: the frozen kernel and the
#                       host-vs-guest clock domains are REAL; large-skew
#                       injection stays on the membership_sim.rs seam.
#   s7-device-fence     (rung 8 — needs a fleet created with --multi-writer;
#                       design row S7-a, spec §6.9 S7 gate R2) the DEVICE-
#                       REJECTION row, SCOPED to the zombie-rejection +
#                       quarantine half (stated posture: in this rig the
#                       WRITER is the membership owner, the custody
#                       authority and the WERO holder — co-writer mounts
#                       and custody HANDOFF are rungs 9-10, so the frozen
#                       victim is the armed writer itself and the recovery
#                       actor is the rig driving the rung-2-proven product
#                       preempt primitive, the same act a successor's
#                       drain proof performs). Steps: sustained write load;
#                       SIGSTOP the armed writer past the membership TTL
#                       (its reader members observe the frozen owner and
#                       SELF-FENCE first — the S6 composition face); the
#                       recovery identity registers + PREEMPTS the
#                       zombie's WERO key on EVERY data namespace (PR
#                       preempt observed on target: report re-read); on
#                       SIGCONT the zombie's resumed DMA must be REJECTED
#                       BY THE DEVICE (reservation-conflict errno class),
#                       the zombie must FAIL-STOP its data plane
#                       (data_dma_fence_refusals moving; epoch_refusals ⊆
#                       fence_refusals — 0 here BY CONSTRUCTION: no
#                       custody moved inside the zombie's process), the
#                       resumed owner's TTL sweep must evict its dead
#                       members + mint their S7 dead epochs, and
#                       dlm_quarantined_offsets stays 0 with the reason
#                       stated (a READER's dead epoch names no offsets;
#                       the offset-holding cohorts are S9's custody
#                       grants and the job wire's destinations — their
#                       no-release-without-a-drain-proof law is pinned in
#                       cargo by tests/dlm_data_fence_tests.rs).
#   s7-kill-matrix [--rounds=N]  (rung 8 — needs --multi-writer; design
#                       row S7-b) kill -9 × N (default 10) of the ARMED
#                       writer at RANDOMIZED phases under sustained write
#                       load; each round: kill → dead-mount sweep →
#                       remount (the successor re-arms MW over the dead
#                       incarnation's STANDING WERO reservation — the
#                       device-observed takeover, fence_mode=1 asserted)
#                       → FULL online fsck with the C8 oracle
#                       (findings: 0; meta_kv_block_refs_drift == 0 — the
#                       --multi-writer format stamps bit 9, so the
#                       durable ledger runs for real) → tripwires flat
#                       (invariant_tripwires, data_dma_fence_refusals,
#                       R5 backstops all 0 on the successor). COUNTED-
#                       RESTART discipline: any failure aborts the count;
#                       the matrix restarts from zero on the fixed
#                       binary. Reader recovery + dirty-skip tripwire
#                       asserted at matrix end.
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
#         [--window=S] [--netem=MS] [--victim=IDX]   (the s6-* legs)
#         [--rounds=N]                               (s7-kill-matrix)
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
S6_WINDOW_S="${SQZ_MWMATRIX_S6_WINDOW_S:-600}"
S6_NETEM_MS=200
S6_VICTIM=""
S7_ROUNDS=10
for a in "$@"; do
    case "$a" in
    --require-host-scoped-subsys) REQUIRE_HS=1 ;;
    --window=*) S6_WINDOW_S="${a#--window=}" ;;
    --netem=*) S6_NETEM_MS="${a#--netem=}" ;;
    --victim=*) S6_VICTIM="${a#--victim=}" ;;
    --rounds=*) S7_ROUNDS="${a#--rounds=}" ;;
    *) die "unknown argument '$a'" ;;
    esac
done
[[ "$S6_WINDOW_S" =~ ^[0-9]+$ ]] || die "--window takes seconds (got '$S6_WINDOW_S')"
[[ "$S6_NETEM_MS" =~ ^[0-9]+$ ]] || die "--netem takes ms (got '$S6_NETEM_MS')"
[[ "$S7_ROUNDS" =~ ^[0-9]+$ ]] && [ "$S7_ROUNDS" -ge 1 ] || die "--rounds takes a positive integer (got '$S7_ROUNDS')"

ensure_root "$LEG" "$@"
# The admin-lane client half of the KD-7 dev override (the daemon half is
# mw_fleet.sh's mount env): dev-tree `-dirty` identities are degenerate,
# and the s7-kill-matrix's online-fsck oracle rides the admin lane.
export SQUEEZEFS_IPC_ALLOW_DEV=1
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
    ("membership_registration_commits", "memb_reg_d"),
    ("membership_self_fences", "memb_fence_d"),
    ("membership_evictions", "memb_evict_d"),
    ("membership_census_serves", "memb_census_d"),
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

# --- rung-7 S6 legs (design-full-multi-writer §7.2) ------------------------
require_membership() {
    [ -n "${MEMBERSHIP:-}" ] ||
        die "this leg needs a membership-armed fleet — create it with: sudo tests/mw_fleet.sh create N=<n> --membership"
    [ "$(stat_field 0 membership_mode)" = "owner" ] ||
        die "member 0 is not the membership OWNER (membership_mode != owner) — the arm did not engage"
}

# Poll one flattened stats field on member <idx> until it is >= <want>,
# within <deadline_s>. Echoes the final value; dies loud on timeout.
wait_stat_ge() { # idx key want deadline_s what
    local idx="$1" key="$2" want="$3" deadline="$4" what="$5" v t0 now
    t0="$(date +%s)"
    while :; do
        v="$(stat_field "$idx" "$key")"
        [[ "$v" =~ ^[0-9]+$ ]] && [ "$v" -ge "$want" ] && {
            echo "$v"
            return 0
        }
        now="$(date +%s)"
        [ $((now - t0)) -lt "$deadline" ] ||
            die "$what: m$idx $key=$v never reached $want within ${deadline}s"
        sleep 1
    done
}

# Poll one flattened stats field on member <idx> until it EQUALS <want>.
wait_stat_eq() { # idx key want deadline_s what
    local idx="$1" key="$2" want="$3" deadline="$4" what="$5" v t0 now
    t0="$(date +%s)"
    while :; do
        v="$(stat_field "$idx" "$key")"
        [ "$v" = "$want" ] && return 0
        now="$(date +%s)"
        [ $((now - t0)) -lt "$deadline" ] ||
            die "$what: m$idx $key=$v never reached $want within ${deadline}s"
        sleep 1
    done
}

# Wait for a literal line (grep -F) to appear in a log file.
wait_log_line() { # file pattern deadline_s what
    local file="$1" pat="$2" deadline="$3" what="$4" t0 now
    t0="$(date +%s)"
    while :; do
        grep -Fq "$pat" "$file" && return 0
        now="$(date +%s)"
        [ $((now - t0)) -lt "$deadline" ] ||
            die "$what: '$pat' never appeared in $file within ${deadline}s"
        sleep 1
    done
}

# The member uuid a mount's LATEST membership join minted (from its log).
member_uuid_of() { # idx
    grep -o "membership MEMBER armed: [a-z-]* '[0-9a-f-]*'" "$STATE/m${1}.log" |
        tail -1 | grep -o "'[0-9a-f-]*'" | tr -d "'"
}

# The owner's renewal cadence estimate, seconds: min(10, T_self/3) — the
# same derivation LeaseClocks ships, read back from the published gauges.
owner_renew_est_s() {
    local tself
    tself="$(stat_field 0 membership_self_deadline_ms)"
    python3 -c "print(max(1, min(10, int($tself) // 3000)))"
}

leg_s6_journal() {
    require_membership
    local rowdir n_members
    rowdir="$STATE/rows/s6journal-$(date +%s)"
    mkdir -p "$rowdir"
    n_members="$(member_idxs | wc -l)"
    [ "$n_members" -ge 2 ] || die "s6-journal needs N>=2"
    local i
    for i in $(member_idxs); do
        [ "$i" = "0" ] && continue
        [ "$(stat_field "$i" membership_mode)" = "member" ] ||
            die "reader $i is not a live membership member — the row would under-count beats"
    done
    local ttl tself renew_est
    ttl="$(stat_field 0 membership_lease_ttl_ms)"
    tself="$(stat_field 0 membership_self_deadline_ms)"
    renew_est="$(owner_renew_est_s)"
    log "s6-journal: N=$n_members (1 owner + $((n_members - 1)) members), window ${S6_WINDOW_S}s, owner clocks T_owner=${ttl}ms T_self=${tself}ms, renew cadence ~${renew_est}s"

    for i in $(member_idxs); do snap "$i" 0 "$rowdir"; done
    # The window is QUIET on purpose: it isolates the liveness plane's
    # journal cost (the S6 gate is about the BEAT plane, and a quiet
    # writer's journal delta is exactly the liveness + own-heartbeat
    # residue). Census probes ride the window — the read side S6 also
    # replaced (`squeezefs clients` = one record + a paged census RPC).
    local t0 now probes=0
    t0="$(date +%s)"
    while :; do
        now="$(date +%s)"
        [ $((now - t0)) -lt "$S6_WINDOW_S" ] || break
        sleep 30
        if "$SQZ" clients "sqmeta://$META_PATHS" >"$rowdir/clients.$probes.out" 2>&1; then
            probes=$((probes + 1))
        else
            die "squeezefs clients probe failed mid-window: $(tail -2 "$rowdir/clients.$probes.out")"
        fi
    done
    for i in $(member_idxs); do snap "$i" 1 "$rowdir"; done
    # Readers must be VISIBLE in the census (the S5 gap this plane closed).
    grep -q "member-reader" "$rowdir/clients.0.out" ||
        die "no member-reader row in squeezefs clients — readers stayed invisible:
$(cat "$rowdir/clients.0.out")"
    # shellcheck disable=SC2046 # member_idxs is a controlled numeric list
    emit_rows "$rowdir" s6-journal $(member_idxs)

    # The S6-a gates (design §7.2 row 1), over the owner's snapshots.
    python3 - "$rowdir" "$n_members" "$S6_WINDOW_S" "$renew_est" "$probes" <<'PYGATE'
import json, sys

rowdir, n, window, renew_est, probes = (
    sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5]))

def flat(d, out=None, pfx=""):
    out = {} if out is None else out
    for k, v in d.items():
        if isinstance(v, dict):
            flat(v, out, pfx + k + ".")
        else:
            out[pfx + k] = v
    return out

def load(p):
    root = json.load(open(p))
    return flat(root.get("metrics", root))

o0, o1 = load(f"{rowdir}/m0_p0.json"), load(f"{rowdir}/m0_p1.json")
d = lambda k: int(o1.get(k, 0)) - int(o0.get(k, 0))

renewals = d("membership_renewals")
jrnl = d("meta_kv_journal_entries")
reg = d("membership_registration_commits")
census = d("membership_census_serves")
evict = d("membership_evictions")
fences = d("membership_self_fences")
refusals = d("membership_grace_refusals")

members = n - 1
expected_beats = members * window // max(renew_est, 1)
per_beat = jrnl / renewals if renewals else float("inf")
# The OWNER's own cadence residue is N-INDEPENDENT (its client:/
# writer_claim heartbeats, echo drains, checkpoints — measured ~3 tx per
# 10 s beat on this tree; the allowance carries 2x headroom). The S6
# regression the gate exists to catch is journal growth COUPLED to the
# member beats (the pre-S6 plane paid exactly 1.0 tx per beat), so the
# bound is: owner allowance + half a tx per beat — N-independent on a
# healthy plane, violated the moment beats start committing.
owner_allowance = (window // 10 + 1) * 6
bound = owner_allowance + renewals // 2

print(f"== s6-journal arithmetic (the spec §6.5 item-3 gate) ==")
print(f"  members (beating)          : {members}")
print(f"  window                     : {window}s, renew cadence ~{renew_est}s")
print(f"  membership_renewals delta  : {renewals} (expected ~{expected_beats})")
print(f"  meta_kv_journal_entries d  : {jrnl}  <- must stay ~= the OWNER's own N-independent residue")
print(f"  owner-cadence allowance    : {owner_allowance} (6 tx / 10 s writer cadence, 2x-headroom)")
print(f"  regression bound           : jrnl < allowance + renewals/2 = {bound}")
print(f"  journal txs PER BEAT       : {per_beat:.4f} (the pre-S6 plane paid 1.0 per beat)")
print(f"  pre-S6 equivalent cost     : ~{renewals} journal txs this window would have paid")
print(f"  registration_commits delta : {reg} (bounded by membership CHANGES; 0 here)")
print(f"  census_serves delta        : {census} over {probes} clients probes")
print(f"  self_fences/evictions/grace: {fences}/{evict}/{refusals}")

bad = []
if renewals < expected_beats // 2:
    bad.append(f"renewals {renewals} < half the expected {expected_beats} — the beat plane is not engaged")
if jrnl >= bound:
    bad.append(f"journal delta {jrnl} >= bound {bound} — journal growth is coupling to the heartbeat (the S6 regression)")
if members >= 8 and per_beat >= 0.25:
    bad.append(f"journal txs per beat {per_beat:.3f} >= 0.25 at N={members} beating members — beat-coupled growth (the sharp at-scale face; at small N the owner residue legitimately dominates this ratio)")
if reg != 0:
    bad.append(f"registration_commits moved ({reg}) with zero membership changes")
if census < probes:
    bad.append(f"census_serves {census} < {probes} probes — the census read side did not engage")
if fences != 0 or evict != 0 or refusals != 0:
    bad.append(f"self_fences={fences} evictions={evict} grace_refusals={refusals} on a healthy window (all must be 0)")
if bad:
    print("S6-a GATE FAILED:", file=sys.stderr)
    for b in bad:
        print(f"  {b}", file=sys.stderr)
    sys.exit(1)
print("S6-a GATE GREEN (heartbeat off the journal; census engaged; R5 columns in the row table above)")
PYGATE
    log "s6-journal leg GREEN (rows + snapshots in $rowdir)"
}

leg_s6_fence() {
    require_membership
    local rowdir victim
    rowdir="$STATE/rows/s6fence-$(date +%s)"
    mkdir -p "$rowdir"
    victim="${S6_VICTIM:-$(member_idxs | awk '$1!=0' | tail -1)}"
    [ -n "$victim" ] && [ "$victim" != "0" ] || die "s6-fence needs a reader victim (N>=2)"
    [ "$(role_of "$victim")" = "reader" ] || die "victim $victim is not a reader"

    local ttl tself renew_est
    ttl="$(stat_field 0 membership_lease_ttl_ms)"
    tself="$(stat_field 0 membership_self_deadline_ms)"
    renew_est="$(owner_renew_est_s)"
    [ "$tself" -lt "$ttl" ] ||
        die "clock law violated in the published gauges: T_self ($tself) must be strictly earlier than T_owner ($ttl)"
    log "s6-fence: victim m$victim, netem ${S6_NETEM_MS}ms/end, T_owner=${ttl}ms T_self=${tself}ms (member fences FIRST by construction)"

    # Remount the victim inside its own netns, with netem shaping its
    # membership wire (design row S6-b: 'netem +200 ms on one member's
    # veth, freeze via SIGSTOP past T_self').
    "$MWFLEET" unmount "$victim"
    "$MWFLEET" mount "$victim" "--netns=$S6_NETEM_MS"
    [ "$(stat_field "$victim" membership_mode)" = "member" ] ||
        die "victim did not re-join through the shaped netns wire"
    local vuuid
    vuuid="$(member_uuid_of "$victim")"
    [ -n "$vuuid" ] || die "cannot read the victim's member uuid from its log"
    log "victim m$victim re-joined through the netem-shaped netns wire as '$vuuid' (delayed renewals still inside the deadlines — the shaping is duress, not partition)"

    # Settle the census FIRST: the unmount->remount dance above leaves the
    # victim's PRIOR incarnations as stale census entries (a clean unmount
    # exits before its renewal loop's next wake can send the leave), and
    # their TTL sweeps would false-match any counter-based eviction wait —
    # so the eviction below is keyed on the victim's OWN member uuid, and
    # p0 is taken only once the census carries exactly the live members.
    local n_members evict_deadline
    n_members="$(member_idxs | wc -l)"
    evict_deadline=$(((ttl / 1000) + 3 * renew_est + 30))
    wait_stat_eq 0 membership_members "$((n_members - 1))" "$evict_deadline" "census settle (stale incarnations swept)"
    local i
    for i in $(member_idxs); do snap "$i" 0 "$rowdir"; done
    local fence0
    fence0="$(stat_field "$victim" membership_self_fences)"

    # Freeze the victim past T_self AND past the owner's TTL. SIGSTOP
    # leaves its monotonic clock RUNNING (unlike the VM pause), so on
    # resume the member observes T_self passed and fences by its OWN
    # clock — the row's 'member fences before the owner re-grants' half
    # is the arithmetic T_self < T_owner asserted above, enforced by the
    # owner acting only at ITS deadline.
    "$MWFLEET" kill "$victim" --sig STOP
    log "victim m$victim SIGSTOPped (frozen daemon, running clock)"
    # The owner must evict THIS incarnation (uuid-keyed — see above) and
    # the eviction line itself names the S6->S7 handoff: the dead lease
    # epoch and the do-not-reallocate quarantine.
    wait_log_line "$STATE/m0.log" "member '$vuuid' (reader) EVICTED" "$evict_deadline" "owner eviction of the frozen victim"
    grep -F "member '$vuuid' (reader) EVICTED" "$STATE/m0.log" | grep -q "quarantined" ||
        die "the victim's eviction line does not name the dead-epoch quarantine"
    grep -q "declared DEAD (membership: member '$vuuid'" "$STATE/m0.log" ||
        die "no dead-epoch mint for the victim's eviction — the S6->S7 handoff did not engage"
    log "owner evicted the frozen victim '$vuuid' + minted its S7 dead epoch (while the victim was still frozen)"

    "$MWFLEET" kill "$victim" --sig CONT
    log "victim m$victim resumed (SIGCONT) — it must now self-fence on its own clock"
    local fences
    fences="$(wait_stat_ge "$victim" membership_self_fences $((fence0 + 1)) $((3 * renew_est + 60)) "victim self-fence")"
    [ "$fences" = "$((fence0 + 1))" ] ||
        die "victim self-fenced $((fences - fence0)) times (want exactly 1)"
    grep -q "SELF-FENCED" "$STATE/m${victim}.log" ||
        die "victim log carries no SELF-FENCED line"
    grep -q "membership self-fence: dropped" "$STATE/m${victim}.log" ||
        die "victim log carries no purge line — the reader fail-stop did not drop its cached blocks"
    log "victim self-fenced + purged (membership_self_fences $fence0 -> $fences)"

    sleep 3 # let every reader's revalidation cadence tick before p1
    for i in $(member_idxs); do snap "$i" 1 "$rowdir"; done
    # Everyone else: ZERO fences, zero grace refusals (the blast radius is
    # exactly the deliberate victim).
    python3 - "$rowdir" "$victim" <<'PYGATE'
import json, sys

rowdir, victim = sys.argv[1], sys.argv[2]

def flat(d, out=None, pfx=""):
    out = {} if out is None else out
    for k, v in d.items():
        if isinstance(v, dict):
            flat(v, out, pfx + k + ".")
        else:
            out[pfx + k] = v
    return out

def load(p):
    root = json.load(open(p))
    return flat(root.get("metrics", root))

import glob, re
bad = []
for p1 in glob.glob(f"{rowdir}/m*_p1.json"):
    i = re.match(r".*/m(\d+)_p1", p1).group(1)
    d1, d0 = load(p1), load(f"{rowdir}/m{i}_p0.json")
    fences = int(d1.get("membership_self_fences", 0)) - int(d0.get("membership_self_fences", 0))
    refusals = int(d1.get("membership_grace_refusals", 0)) - int(d0.get("membership_grace_refusals", 0))
    if i == victim:
        if fences != 1:
            bad.append(f"m{i} (victim): self_fences delta {fences} != 1")
    elif fences != 0:
        bad.append(f"m{i}: self_fences delta {fences} != 0 — the blast radius leaked past the victim")
    if refusals != 0:
        bad.append(f"m{i}: grace_refusals moved ({refusals}) — no failover happened here")
if bad:
    print("S6-b GATE FAILED:", file=sys.stderr)
    for b in bad:
        print(f"  {b}", file=sys.stderr)
    sys.exit(1)
print("S6-b GATE GREEN (fence exactly on the victim; grace quiet)")
PYGATE
    # shellcheck disable=SC2046 # member_idxs is a controlled numeric list
    emit_rows "$rowdir" s6-fence $(member_idxs)

    # Restore the fleet: clear the shaping, remount the victim normally,
    # and require it to re-join live.
    "$MWFLEET" netem "$victim" off
    "$MWFLEET" unmount "$victim"
    "$MWFLEET" mount "$victim"
    [ "$(stat_field "$victim" membership_mode)" = "member" ] ||
        die "victim did not re-join after the restore remount"
    log "s6-fence leg GREEN (victim restored; rows + snapshots in $rowdir)"
}

leg_s6_vm_fence() {
    require_membership
    require_vm_fleet
    [ -n "${MEMBERSHIP_ENDPOINT:-}" ] || die "fleet config carries no MEMBERSHIP_ENDPOINT"
    case "$MEMBERSHIP_ENDPOINT" in
    127.0.0.1:* | 0.0.0.0:*)
        die "the owner advertises $MEMBERSHIP_ENDPOINT, which a guest cannot dial through slirp — this box has no routable primary interface; the S6-b' row needs one"
        ;;
    esac
    local rowdir g_nqn g_id
    rowdir="$STATE/rows/s6vmfence-$(date +%s)"
    mkdir -p "$rowdir"
    g_nqn="$(guest_nqn 70)" g_id="$(guest_id 70)"

    # The in-guest connect PLAN: the reader resolves its DATA volumes by
    # the FORMAT-TIME device paths (the identity-less-reader law), and the
    # host's format-era instance numbers (real local NVMe occupies the low
    # slots) cannot be reproduced by a fresh guest kernel's lowest-free
    # numbering — so the guest connects each fleet NQN, VERIFIES the
    # resolved head serves exactly that NQN (sysfs — the reader-safety
    # check's guest face), and then ALIASES the format-time name to it
    # (a devtmpfs symlink the daemon's open() follows; the verification
    # is what makes the alias safe, and an occupied name refuses loud).
    local plan meta_basenames
    plan="$(python3 - <<PYPLAN
import sys
meta_paths = "$FORMAT_META_PATHS".split(",")
data_paths = "$FORMAT_DATA_PATHS".split(",")
meta_nqns = "$META_NQNS".split()
data_nqns = "$DATA_NQNS".split()
pairs = list(zip(meta_paths, meta_nqns)) + list(zip(data_paths, data_nqns))
rows = []
for path, nqn in pairs:
    base = path.rsplit("/", 1)[-1]
    if not (base.startswith("nvme") and base.endswith("n1")):
        sys.exit(f"unexpected head name {base}")
    rows.append((int(base[4:-2]), base, nqn))
rows.sort()
for k, base, nqn in rows:
    print(f"{k} {base} {nqn}")
PYPLAN
)" || die "connect plan generation failed: $plan"
    echo "$plan" >"$rowdir/connect-plan"
    meta_basenames="$(python3 -c 'import sys
print(",".join("/dev/" + p.rsplit("/", 1)[-1] for p in sys.argv[1].split(",")))' "$FORMAT_META_PATHS")"
    log "s6-vm-fence: guest connect plan ($(echo "$plan" | wc -l) namespaces), meta URI in-guest: $meta_basenames"

    # ---- job A: join the fleet as a READ-ONLY member, in-guest ----
    {
        guest_job_preamble
        echo "PLAN='$plan'"
        cat <<JOBA
echo "\$PLAN" | while read -r k base nqn; do
    [ -n "\$nqn" ] || continue
    \$SQZ nvmeof connect --ip "\$GW" --port "\$SVC" --subnqn "\$nqn" --hostnqn '$g_nqn' --hostid '$g_id'
    head=""
    i=0
    while [ \$i -lt 40 ]; do
        for s in \$(subsys_dirs_for_nqn "\$nqn"); do head=\$(head_of_dir "\$s") && break; done
        [ -n "\$head" ] && [ -b "/dev/\$head" ] && break
        i=\$((i + 1)); sleep 0.5
    done
    [ -n "\$head" ] && [ -b "/dev/\$head" ] || { echo "FAIL: \$nqn resolved no openable head in-guest"; exit 1; }
    if [ "\$head" != "\$base" ]; then
        [ -e "/dev/\$base" ] && { echo "FAIL: format-time name /dev/\$base is already occupied in-guest — cannot alias safely"; exit 1; }
        ln -s "/dev/\$head" "/dev/\$base"
        echo "aliased /dev/\$base -> /dev/\$head (verified serving \$nqn)"
    fi
done || exit 1
echo "connect plan resolved (every format-time name verified against its NQN)"
mkdir -p /mnt/member
\$SQZ mount "sqmeta://$meta_basenames" /mnt/member --read-only --daemon --log-file /tmp/member.log >/tmp/member.mount.out 2>&1 || { cat /tmp/member.mount.out; cat /tmp/member.log 2>/dev/null; exit 1; }
i=0
while [ \$i -lt 240 ]; do grep -q " /mnt/member " /proc/mounts && break; i=\$((i + 1)); sleep 0.5; done
grep -q " /mnt/member " /proc/mounts || { echo "FAIL: member mount never appeared"; cat /tmp/member.log; exit 1; }
i=0
mode=""
while [ \$i -lt 60 ]; do
    mode=\$(grep -o '"membership_mode": *"[a-z]*"' /mnt/member/.stats | grep -o '"[a-z]*"\$' | sed 's/"//g')
    [ "\$mode" = "member" ] && break
    i=\$((i + 1)); sleep 0.5
done
[ "\$mode" = "member" ] || { echo "FAIL: guest membership_mode='\$mode' (want member) — cannot dial the owner at $MEMBERSHIP_ENDPOINT?"; grep -i membership /tmp/member.log; exit 1; }
fences=\$(grep -o '"membership_self_fences": *[0-9]*' /mnt/member/.stats | grep -o '[0-9]*\$')
epoch=\$(grep -o '"membership_epoch": *[0-9]*' /mnt/member/.stats | grep -o '[0-9]*\$')
guuid=\$(grep -o "membership MEMBER armed: reader '[0-9a-f-]*'" /tmp/member.log | tail -1 | grep -o "'[0-9a-f-]*'" | sed "s/'//g")
[ -n "\$guuid" ] || { echo "FAIL: cannot read the guest member uuid from its log"; exit 1; }
echo "GUEST_FENCES_BASE=\$fences"
echo "GUEST_EPOCH_BASE=\$epoch"
echo "GUEST_UUID=\$guuid"
echo "GUEST MEMBER GREEN (RO mount joined the host fleet's membership plane)"
JOBA
    } >"$rowdir/job-a.sh"
    # Settle the census FIRST (the S6-b lesson): stale prior incarnations
    # (a rig re-run's dead guest member, a remounted reader's old lease)
    # sweep on the owner's cadence and would false-match any COUNTER-based
    # eviction wait — so the census must read exactly the live host
    # members before the guest joins, and the eviction below is keyed on
    # the guest's OWN member uuid.
    local ttl renew_est n_host_members
    ttl="$(stat_field 0 membership_lease_ttl_ms)"
    renew_est="$(owner_renew_est_s)"
    n_host_members="$(($(member_idxs | wc -l) - 1))"
    wait_stat_eq 0 membership_members "$n_host_members" $(((ttl / 1000) + 3 * renew_est + 30)) "census settle (stale incarnations swept)"

    "$MWFLEET" vm-exec 0 "$rowdir/job-a.sh" 600 | tee "$rowdir/job-a.out" ||
        die "guest member join FAILED (output: $rowdir/job-a.out)"
    local g_fence0 g_epoch0 g_uuid
    g_fence0="$(awk -F= '/^GUEST_FENCES_BASE=/ {print $2}' "$rowdir/job-a.out" | tr -d '\r')"
    g_epoch0="$(awk -F= '/^GUEST_EPOCH_BASE=/ {print $2}' "$rowdir/job-a.out" | tr -d '\r')"
    g_uuid="$(awk -F= '/^GUEST_UUID=/ {print $2}' "$rowdir/job-a.out" | tr -d '\r')"
    [ -n "$g_fence0" ] && [ -n "$g_epoch0" ] && [ -n "$g_uuid" ] ||
        die "guest job reported no baselines"

    snap 0 0 "$rowdir"
    # The owner census must carry the guest (member-reader, mount point
    # /mnt/member) — the S5 gap closed cross-KERNEL for the first time.
    "$SQZ" clients "sqmeta://$META_PATHS" >"$rowdir/clients-joined.out" 2>&1 ||
        die "clients probe failed"
    grep -q "member-reader" "$rowdir/clients-joined.out" ||
        die "guest member not visible in squeezefs clients:
$(cat "$rowdir/clients-joined.out")"

    # ---- the hung kernel: qemu pause past the owner's TTL ----
    "$MWFLEET" pause 0
    log "guest 0 PAUSED (vcpus + guest clock frozen — the monotonic domain cannot observe T_self)"
    wait_log_line "$STATE/m0.log" "member '$g_uuid' (reader) EVICTED" $(((ttl / 1000) + 3 * renew_est + 60)) "owner eviction of the paused guest"
    grep -F "member '$g_uuid' (reader) EVICTED" "$STATE/m0.log" | grep -q "quarantined" ||
        die "the guest's eviction line does not name the dead-epoch quarantine"
    grep -q "declared DEAD (membership: member '$g_uuid'" "$STATE/m0.log" ||
        die "no dead-epoch mint for the guest's eviction — the S6->S7 handoff did not engage"
    log "owner evicted the paused guest '$g_uuid' + minted its S7 dead epoch (while the guest kernel was frozen)"

    "$MWFLEET" resume 0
    log "guest 0 resumed — it must observe itself dead and self-fence BEFORE holding any fresh lease"

    # ---- job B: the resume law, asserted in-guest ----
    {
        guest_job_preamble
        cat <<JOBB
i=0
fences=""
while [ \$i -lt 120 ]; do
    fences=\$(grep -o '"membership_self_fences": *[0-9]*' /mnt/member/.stats | grep -o '[0-9]*\$')
    [ -n "\$fences" ] && [ "\$fences" -gt "$g_fence0" ] && break
    i=\$((i + 1)); sleep 1
done
[ -n "\$fences" ] && [ "\$fences" -gt "$g_fence0" ] || { echo "FAIL: guest never self-fenced after resume (membership_self_fences=\$fences, base $g_fence0) — it resumed as a live member on its stale caches (the S6-b' falsifier)"; grep -i membership /tmp/member.log | tail -5; exit 1; }
[ "\$fences" = "$((g_fence0 + 1))" ] || { echo "FAIL: guest fenced \$fences times (want exactly $((g_fence0 + 1)))"; exit 1; }
grep -q "SELF-FENCED" /tmp/member.log || { echo "FAIL: no SELF-FENCED line in the guest daemon log"; exit 1; }
grep -q "membership self-fence: dropped" /tmp/member.log || { echo "FAIL: no purge line — the reader fail-stop did not drop its cached blocks"; exit 1; }
# The fixed ladder: fence FIRST, then a FRESH re-join (clean view, new
# epoch) — availability restored without ever serving the stale view.
i=0
mode=""
epoch=""
while [ \$i -lt 60 ]; do
    mode=\$(grep -o '"membership_mode": *"[a-z]*"' /mnt/member/.stats | grep -o '"[a-z]*"\$' | sed 's/"//g')
    epoch=\$(grep -o '"membership_epoch": *[0-9]*' /mnt/member/.stats | grep -o '[0-9]*\$')
    [ "\$mode" = "member" ] && [ -n "\$epoch" ] && [ "\$epoch" != "$g_epoch0" ] && break
    i=\$((i + 1)); sleep 1
done
[ "\$mode" = "member" ] || { echo "FAIL: guest did not re-join fresh after its fence (mode=\$mode)"; exit 1; }
[ "\$epoch" != "$g_epoch0" ] || { echo "FAIL: guest resurrected its dead epoch $g_epoch0"; exit 1; }
echo "GUEST_EPOCH_FRESH=\$epoch"
echo "GUEST RESUME LAW GREEN (fenced + purged FIRST, then re-joined fresh: epoch $g_epoch0 -> \$epoch)"
JOBB
    } >"$rowdir/job-b.sh"
    "$MWFLEET" vm-exec 0 "$rowdir/job-b.sh" 300 | tee "$rowdir/job-b.out" ||
        die "guest resume-law job FAILED (output: $rowdir/job-b.out)"

    snap 0 1 "$rowdir"
    local refusals0 refusals1
    local graceprobe='import json, sys
def flat(d, out=None, pfx=""):
    out = {} if out is None else out
    for k, v in d.items():
        if isinstance(v, dict): flat(v, out, pfx + k + ".")
        else: out[pfx + k] = v
    return out
root = json.load(open(sys.argv[1]))
print(flat(root.get("metrics", root)).get("membership_grace_refusals", 0))'
    refusals0="$(python3 -c "$graceprobe" "$rowdir/m0_p0.json")"
    refusals1="$(python3 -c "$graceprobe" "$rowdir/m0_p1.json")"
    [ "$refusals0" = "$refusals1" ] ||
        die "membership_grace_refusals moved ($refusals0 -> $refusals1) — no failover happened here"

    # ---- job C: cleanup (always) ----
    {
        guest_job_preamble
        cat <<JOBC
set +e
\$SQZ umount /mnt/member >/dev/null 2>&1 || umount -l /mnt/member 2>/dev/null
i=0
while [ \$i -lt 120 ]; do grep -q " /mnt/member " /proc/mounts || break; i=\$((i + 1)); sleep 0.5; done
echo "\$PLAN" >/dev/null 2>&1
for c in /sys/class/nvme/nvme*; do
    [ "\$(cat "\$c/hostnqn" 2>/dev/null)" = '$g_nqn' ] || continue
    echo 1 >"\$c/delete_controller" 2>/dev/null
done
sleep 1
echo "cleanup done"
exit 0
JOBC
    } >"$rowdir/job-cleanup.sh"
    "$MWFLEET" vm-exec 0 "$rowdir/job-cleanup.sh" 300 | tee "$rowdir/job-cleanup.out" ||
        warn "in-guest cleanup reported errors"
    log "s6-vm-fence leg GREEN (evidence in $rowdir)"
}

# --- rung-8 S7 legs (design-full-multi-writer §7.2 rows S7-a / S7-b) --------
require_mw() {
    require_membership
    [ "${MW:-0}" = "1" ] ||
        die "this leg needs a multi-writer-armed fleet — create it with: sudo tests/mw_fleet.sh create N=2 --multi-writer [--lease-ttl-ms=15000]"
    [ "$(stat_field 0 data_plane_fence_mode)" = "1" ] ||
        die "member 0 data_plane_fence_mode != 1 — the S7 WERO hold is not standing"
}

# Controller (nvmeX) serving <subsysnqn> under <hostnqn> — the rung-2
# ctrl-char-dev discipline (the head block node round-robins paths; the
# char device pins the association).
ctrl_for() { # subsysnqn hostnqn -> nvmeX
    local c
    for c in /sys/class/nvme/nvme*; do
        [ -d "$c" ] || continue
        [ "$(cat "$c/subsysnqn" 2>/dev/null)" = "$1" ] || continue
        [ "$(cat "$c/hostnqn" 2>/dev/null)" = "$2" ] || continue
        basename "$c"
        return 0
    done
    return 1
}

# Reservation report probe: prints "<regctl> <rtype> <rkey0>" (rkey0 = the
# first registrant's key, 0x-hex; '-' when none). nvme-cli json spellings
# vary across releases — parse defensively.
resv_probe() { # ctrl-char-dev nsid
    nvme resv-report "$1" -n "$2" -o json 2>/dev/null | python3 -c '
import json, sys
try:
    r = json.load(sys.stdin)
except Exception:
    print("- - -"); raise SystemExit
regctl = r.get("regctl", 0)
rtype = r.get("rtype", 0)
regs = r.get("regctlext") or r.get("regctl_ext") or r.get("regctls") or []
rkey = "-"
if regs:
    k = regs[0].get("rkey", 0)
    rkey = hex(k) if isinstance(k, int) else str(k)
print(regctl, rtype, rkey)'
}

# Wait until <ctrl> has scanned at least one namespace: reservation ioctls
# on a controller whose namespaces have not attached yet answer ENOTTY
# ('Inappropriate ioctl for device') — the run-3 rig lesson. Multipath
# kernels expose per-path namespaces as nvmeXcYnZ under the controller.
wait_ctrl_ns() { # nvmeX
    local t d b
    for t in $(seq 1 60); do
        : "$t"
        for d in "/sys/class/nvme/$1"/nvme*; do
            b="$(basename "$d")"
            if [[ "$b" =~ ^nvme[0-9]+(c[0-9]+)?n[0-9]+$ ]]; then
                return 0
            fi
        done
        sleep 0.25
    done
    die "controller $1 never scanned a namespace (reservation ioctls would answer ENOTTY)"
}

# Delete ONE controller by name (sysfs) — never `nvmeof disconnect <nqn>`,
# which would drop the WRITER's association on the same NQN too.
delete_ctrl() { # nvmeX
    echo 1 >"/sys/class/nvme/$1/delete_controller" 2>/dev/null || true
    local t
    for t in $(seq 1 40); do
        [ -d "/sys/class/nvme/$1" ] || return 0
        sleep 0.25
    done
    warn "controller $1 did not tear down within 10s"
}

# Read one flattened stats field out of a SNAPSHOT file (frozen daemons
# cannot serve their stats inode — snapshots are the frozen-window truth).
snap_field() { # snapfile key
    python3 -c '
import json, sys
def flat(d, out=None, pfx=""):
    out = {} if out is None else out
    for k, v in d.items():
        if isinstance(v, dict): flat(v, out, pfx + k + ".")
        else: out[pfx + k] = v
    return out
root = json.load(open(sys.argv[1]))
print(flat(root.get("metrics", root)).get(sys.argv[2], ""))' "$1" "$2"
}

# Best-effort operator restore when the s7-device-fence leg dies mid-row:
# a `die` inside the frozen window would otherwise strand the writer
# SIGSTOPped (the run-2/run-3 rig lesson — a frozen daemon hangs every
# later mountpoint/umount probe in D-state). Loud, never a verdict.
S7_RESTORE_WRITER_PID=""
S7_RESTORE_DD_PID=""
S7_RECOVERY_HELD=0
S7_RECOVERY_KEY=""
S7_RECOVERY_NQN=""
S7_RECOVERY_ID=""

# Release the recovery identity's WERO hold on every data namespace —
# shared by the leg's own restore section and the fail trap (a stranded
# recovery reservation makes every later MW mount refuse: 'a partial
# fence is not a fence' — the s7-b round-1 cascade).
s7_release_recovery_hold() {
    [ "$S7_RECOVERY_HELD" = "1" ] || return 0
    local nqn rctrl i
    local -a nqns=()
    read -r -a nqns <<<"$DATA_NQNS"
    for nqn in "${nqns[@]}"; do
        "$SQZ" nvmeof connect --ip "$TCP_ADDR" --port "$TCP_SVC" --subnqn "$nqn" \
            --hostnqn "$S7_RECOVERY_NQN" --hostid "$S7_RECOVERY_ID" >/dev/null 2>&1 || true
        rctrl=""
        for i in $(seq 1 40); do
            rctrl="$(ctrl_for "$nqn" "$S7_RECOVERY_NQN")" && break
            sleep 0.25
        done
        if [ -n "$rctrl" ]; then
            wait_ctrl_ns "$rctrl"
            nvme resv-release "/dev/$rctrl" -n 1 --crkey="$S7_RECOVERY_KEY" --rtype=3 >/dev/null 2>&1 || true
            nvme resv-register "/dev/$rctrl" -n 1 --crkey="$S7_RECOVERY_KEY" --rrega=1 >/dev/null 2>&1 || true
            delete_ctrl "$rctrl"
        fi
    done
    S7_RECOVERY_HELD=0
}

s7_restore_on_fail() {
    local rc=$?
    [ "$rc" -eq 0 ] && return 0
    warn "s7-device-fence leg exiting rc=$rc — best-effort restore (SIGCONT writer, kill load, release recovery hold)"
    [ -n "$S7_RESTORE_DD_PID" ] && kill -9 "$S7_RESTORE_DD_PID" 2>/dev/null
    [ -n "$S7_RESTORE_WRITER_PID" ] && kill -CONT "$S7_RESTORE_WRITER_PID" 2>/dev/null
    s7_release_recovery_hold || true
    return 0
}

leg_s7_device_fence() {
    require_mw
    trap s7_restore_on_fail EXIT
    S7_RESTORE_WRITER_PID="$(awk -F'\t' '$1==0 {print $7}' "$MEMBERS")"
    local rowdir victim_reader
    rowdir="$STATE/rows/s7fence-$(date +%s)"
    mkdir -p "$rowdir"
    victim_reader="$(member_idxs | awk '$1!=0' | head -1)"
    [ -n "$victim_reader" ] || die "s7-device-fence needs N>=2 (a reader member observes the frozen owner)"

    local w_mnt ttl renew_est
    w_mnt="$(mnt_of 0)"
    ttl="$(stat_field 0 membership_lease_ttl_ms)"
    renew_est="$(owner_renew_est_s)"
    log "s7-device-fence (SCOPED posture — see header): victim = the ARMED WRITER m0 (owner+authority+WERO holder); recovery actor = the rig via the rung-2 preempt primitive; T_owner=${ttl}ms"

    # --- device-truth p0: the zombie's registrant key per data namespace ----
    # Read through the WRITER's own daemon-owned controllers (reservation
    # REPORT is a read; the char dev pins the association).
    local nqn wctrl regctl rtype zkey="" probe
    local -a data_nqns=()
    read -r -a data_nqns <<<"$DATA_NQNS"
    [ "${#data_nqns[@]}" -ge 1 ] || die "config carries no DATA_NQNS"
    for nqn in "${data_nqns[@]}"; do
        wctrl="$(ctrl_for "$nqn" "$W_HOSTNQN")" ||
            die "no writer-identity controller for data NQN $nqn"
        probe="$(resv_probe "/dev/$wctrl" 1)"
        read -r regctl rtype zkey <<<"$probe"
        [ "$rtype" = "3" ] ||
            die "$nqn: standing reservation rtype=$rtype (want 3 = WERO) — the S7 hold is not what the arm claims"
        [ "$regctl" = "1" ] ||
            die "$nqn: regctl=$regctl (want exactly 1 = the writer) before the recovery actor registers"
        [ "$zkey" != "-" ] || die "$nqn: no registrant key readable"
        log "p0 device truth: $nqn rtype=3 regctl=1 zombie key=$zkey (via /dev/$wctrl)"
    done

    local i
    for i in $(member_idxs); do snap "$i" 0 "$rowdir"; done
    local rfence0
    rfence0="$(stat_field "$victim_reader" membership_self_fences)"

    # --- sustained write load, running when the freeze lands ----------------
    log "starting sustained write load on the writer mount"
    (exec dd if=/dev/zero of="$w_mnt/s7load.dat" bs=1M count=16384 conv=fsync status=none) &
    local dd_pid=$!
    S7_RESTORE_DD_PID="$dd_pid"
    sleep 3 # let the pipeline fill (in-flight DMA to resume later)
    kill -0 "$dd_pid" 2>/dev/null || die "write load exited before the freeze (too small for this box?)"

    # --- freeze the armed writer past the membership TTL --------------------
    "$MWFLEET" kill 0 --sig STOP
    log "armed writer m0 SIGSTOPped (frozen daemon; its kernel keeps draining already-submitted DMA)"
    # S6 composition face: the reader member observes the FROZEN owner and
    # self-fences by its own clock (T_self) before any re-grant could exist.
    local rfences
    rfences="$(wait_stat_ge "$victim_reader" membership_self_fences $((rfence0 + 1)) $((ttl / 1000 + 6 * renew_est + 60)) "reader self-fence against the frozen owner")"
    log "reader m$victim_reader self-fenced against the frozen owner (membership_self_fences $rfence0 -> $rfences) — the S6 owner-death law"
    # Let already-submitted kernel I/O drain to the single existing path
    # before a second path exists (merged-head kernels round-robin).
    sleep 3

    # --- the recovery actor: register + PREEMPT the zombie's key ------------
    # The rung-2-proven product takeover primitive, per data namespace: a
    # RECOVERY identity registers its own key and preempts the zombie's
    # (racqa=1) — the same act a successor's drain proof performs
    # (WeroHold::preempt). The reservation stays HELD by the recovery key:
    # releasing it would re-admit the zombie (WERO rejects only while a
    # reservation stands).
    local r_id r_nqn rkey=0x51e7a8 rctrl
    r_id="$(printf 'cafef1e7-%04d-4000-8000-%012d' 87 "$CREATE_PID")"
    r_nqn="nqn.2014-08.org.nvmexpress:uuid:$r_id"
    S7_RECOVERY_KEY="$rkey" S7_RECOVERY_NQN="$r_nqn" S7_RECOVERY_ID="$r_id" S7_RECOVERY_HELD=1
    local -a rctrls=()
    for nqn in "${data_nqns[@]}"; do
        "$SQZ" nvmeof connect --ip "$TCP_ADDR" --port "$TCP_SVC" --subnqn "$nqn" \
            --hostnqn "$r_nqn" --hostid "$r_id" >/dev/null 2>&1 ||
            die "recovery-identity connect failed for $nqn"
        rctrl=""
        for i in $(seq 1 40); do
            rctrl="$(ctrl_for "$nqn" "$r_nqn")" && break
            sleep 0.25
        done
        [ -n "$rctrl" ] || die "no recovery-identity controller for $nqn"
        rctrls+=("$rctrl")
        wait_ctrl_ns "$rctrl"
        nvme resv-register "/dev/$rctrl" -n 1 --nrkey="$rkey" --cptpl=0 >/dev/null ||
            die "recovery register failed on $nqn"
        nvme resv-acquire "/dev/$rctrl" -n 1 --crkey="$rkey" --prkey="$zkey" \
            --rtype=3 --racqa=1 >/dev/null ||
            die "recovery preempt of the zombie key $zkey failed on $nqn"
        probe="$(resv_probe "/dev/$rctrl" 1)"
        read -r regctl rtype _ <<<"$probe"
        [ "$regctl" = "1" ] && [ "$rtype" = "3" ] ||
            die "$nqn post-preempt report: regctl=$regctl rtype=$rtype (want 1/3) — the preempt did not land"
        log "PR preempt observed on target: $nqn zombie key $zkey removed, recovery key $rkey holds WERO (regctl=1)"
    done
    echo "${rctrls[*]}" >"$rowdir/recovery-ctrls"
    # Drop the recovery PATHS before the zombie resumes: on a merged-head
    # (multipath=Y) kernel the resumed zombie's I/O must ride ITS OWN
    # association only. The reservation is host-keyed device state and
    # stands after the disconnect.
    for rctrl in "${rctrls[@]}"; do delete_ctrl "$rctrl"; done
    log "recovery paths dropped (reservation stands, held by $rkey)"

    # --- resume: the zombie's DMA must be rejected BY THE DEVICE ------------
    "$MWFLEET" kill 0 --sig CONT
    log "zombie writer m0 resumed (SIGCONT) — its in-flight + new DMA now meets the device fence"
    # The reservation-conflict errno class (EBADE, 'Invalid exchange') on a
    # DATA volume, then the zombie's own fail-stop: the fence latch + the
    # custody poison + refusals counted (data_dma_fence_refusals).
    local t0 now deadline=120
    t0="$(date +%s)"
    while :; do
        grep -Eq "os error 52|Invalid exchange" "$STATE/m0.log" && break
        now="$(date +%s)"
        [ $((now - t0)) -lt "$deadline" ] ||
            die "no reservation-conflict errno class (EBADE/os error 52) in the zombie's log within ${deadline}s — the device did not reject the resumed DMA"
        sleep 1
    done
    log "device rejection observed: reservation-conflict errno class in the zombie's log"
    wait_log_line "$STATE/m0.log" "writer guard FENCED" "$deadline" "zombie data-plane fence latch"
    wait_log_line "$STATE/m0.log" "data-plane custody POISONED" "$deadline" "zombie custody poison"
    local refusals
    refusals="$(wait_stat_ge 0 data_dma_fence_refusals 1 "$deadline" "zombie data_dma_fence_refusals")"
    log "zombie FAIL-STOPPED: fence latched, custody poisoned, data_dma_fence_refusals=$refusals"
    kill -9 "$dd_pid" 2>/dev/null || true
    wait "$dd_pid" 2>/dev/null || true

    # --- the S6->S7 handoff on the resumed owner: evict + dead-epoch mint ---
    # RACE, stated honestly: the fenced reader's retry loop re-presents
    # FRESH the moment the owner answers (the rung-8 finding-#1 fix), and a
    # fresh JOIN replaces the dead lease in the owner's RAM table before
    # the TTL sweep can expire it — so the resumed owner mints a dead epoch
    # ONLY when its sweep wins that race. Both outcomes are correct
    # product behavior; the leg accepts EITHER the mint (sweep won) or the
    # reader's fenced-then-fresh re-join (the fix's path won — its own law
    # is pinned in cargo, and the S6→S7 mint composition is rung 7's
    # proven S6-b/S6-b' row). One of the two MUST hold, loudly.
    local mint_deadline mint_seen=0 t0m
    mint_deadline=$((6 * renew_est + 30))
    t0m="$(date +%s)"
    while :; do
        if grep -Fq "declared DEAD (membership: member" "$STATE/m0.log"; then
            mint_seen=1
            break
        fi
        [ $(($(date +%s) - t0m)) -lt "$mint_deadline" ] || break
        sleep 1
    done
    if [ "$mint_seen" = "1" ]; then
        log "resumed owner evicted its expired member(s) + minted the S7 dead epoch(s) (sweep won the race)"
    else
        grep -q "re-joined FRESH after its self-fence" "$STATE/m${victim_reader}.log" ||
            die "neither the owner's dead-epoch mint nor the reader's fenced-then-fresh re-join happened — the S6->S7 handoff is broken on BOTH arms"
        log "reader re-presented FRESH before the owner's sweep could expire its dead lease (the finding-#1 fix's path; the mint arm is rung 7's proven row)"
    fi

    sleep 2
    for i in $(member_idxs); do snap "$i" 1 "$rowdir"; done

    # --- the leg gate --------------------------------------------------------
    python3 - "$rowdir" "$victim_reader" "$mint_seen" <<'PYGATE'
import glob, json, re, sys

rowdir, victim_reader, mint_seen = sys.argv[1], sys.argv[2], sys.argv[3] == "1"

def flat(d, out=None, pfx=""):
    out = {} if out is None else out
    for k, v in d.items():
        if isinstance(v, dict):
            flat(v, out, pfx + k + ".")
        else:
            out[pfx + k] = v
    return out

def load(p):
    root = json.load(open(p))
    return flat(root.get("metrics", root))

bad = []
for p1 in sorted(glob.glob(f"{rowdir}/m*_p1.json")):
    i = re.match(r".*/m(\d+)_p1", p1).group(1)
    d1, d0 = load(p1), load(f"{rowdir}/m{i}_p0.json")
    dd = lambda k: int(d1.get(k, 0)) - int(d0.get(k, 0))
    fence = dd("data_dma_fence_refusals")
    epoch = dd("data_dma_epoch_refusals")
    quarantined = int(d1.get("dlm_quarantined_offsets", 0))
    releases = dd("dlm_quarantine_releases")
    trip = dd("invariant_tripwires")
    backstops = dd("mem_budget_hard_backstops")
    gate_to = dd("parked_gate_timeouts")
    evict = dd("membership_evictions")
    if i == "0":
        # The VICTIM: fenced, refusing, epoch class ⊆ fence class (0 here
        # BY CONSTRUCTION — no custody moved inside the zombie's process:
        # no term bump, no revoked grant; the fence is the POISON latch).
        if fence < 1:
            bad.append(f"m0 (victim): data_dma_fence_refusals delta {fence} < 1")
        if not (0 <= epoch <= fence):
            bad.append(f"m0 (victim): epoch_refusals {epoch} not within [0, fence {fence}]")
        if epoch != 0:
            bad.append(f"m0 (victim): epoch_refusals {epoch} != 0 — nothing advanced the zombie's custody generation in this scoped posture")
        if mint_seen and evict < 1:
            bad.append(f"m0: membership_evictions delta {evict} < 1 — a dead-epoch mint was observed without its eviction")
        if int(d1.get("write_pipeline_fence_drops", 0)) < 0:
            bad.append("m0: fence_drops went negative (counter corruption)")
    else:
        # The BLAST RADIUS: no fence movement anywhere but the victim.
        if fence != 0 or epoch != 0:
            bad.append(f"m{i}: data-plane fence/epoch refusals moved ({fence}/{epoch}) on a non-victim")
        if int(d1.get("data_plane_fence_mode", 0)) != 0:
            bad.append(f"m{i}: data_plane_fence_mode != 0 on a reader")
    # The QUARANTINE law (scoped): a READER's dead epoch names no offsets,
    # so the gauge stays 0 and NOTHING was released without a drain proof.
    # The offset-holding cohorts (S9 custody grants, job-wire destinations)
    # are rungs 9-10; their no-release-without-a-proof law is pinned in
    # cargo (tests/dlm_data_fence_tests.rs).
    if quarantined != 0:
        bad.append(f"m{i}: dlm_quarantined_offsets={quarantined} — nothing at this rung may hold offsets")
    if releases != 0:
        bad.append(f"m{i}: dlm_quarantine_releases moved ({releases}) with no drain proof issued")
    if trip != 0:
        bad.append(f"m{i}: invariant_tripwires moved ({trip})")
    if backstops != 0 or gate_to != 0:
        bad.append(f"m{i}: R5 columns moved (backstops={backstops}, gate_timeouts={gate_to})")

if bad:
    print("S7-a GATE FAILED:", file=sys.stderr)
    for b in bad:
        print(f"  {b}", file=sys.stderr)
    sys.exit(1)
print("S7-a GATE GREEN (device rejection + zombie fail-stop + preempt observed; quarantine law held; blast radius = the victim)")
PYGATE

    # --- restore the fleet ----------------------------------------------------
    # The zombie is DEAD BY DESIGN (a fenced holder is dead until remount):
    # kill it, clear the recovery hold (safe — the zombie is gone), remount
    # the writer fresh (a fresh WERO under a fresh key), and require the
    # reader to re-join the new owner.
    "$MWFLEET" kill 0 --sig 9 || true
    sleep 1
    s7_release_recovery_hold
    log "recovery reservation released + recovery identity unregistered (the zombie is dead; the successor takes its own hold)"
    umount -l "$w_mnt" 2>/dev/null || true
    wait_for_unmounted "$w_mnt"
    "$MWFLEET" mount 0
    [ "$(stat_field 0 data_plane_fence_mode)" = "1" ] ||
        die "restored writer did not re-arm the WERO hold"
    local ttl_s=$((ttl / 1000))
    wait_stat_eq "$victim_reader" membership_mode member $((ttl_s + 6 * renew_est + 90)) "reader re-join of the restored owner"
    trap - EXIT
    log "s7-device-fence leg GREEN (writer restored, reader re-joined; rows + device truth in $rowdir)"
}

wait_for_unmounted() { # mountpoint
    local t
    for t in $(seq 1 120); do
        : "$t"
        mountpoint -q "$1" || return 0
        sleep 0.5
    done
    die "$1 never unmounted"
}

leg_s7_kill_matrix() {
    require_mw
    local rowdir w_mnt reader_idx
    rowdir="$STATE/rows/s7kill-$(date +%s)"
    mkdir -p "$rowdir"
    w_mnt="$(mnt_of 0)"
    reader_idx="$(member_idxs | awk '$1!=0' | head -1)"
    log "s7-kill-matrix: kill -9 x$S7_ROUNDS of the ARMED writer at randomized phases under sustained write load; per round: remount (WERO takeover over the dead incarnation's standing reservation) + FULL online fsck with the C8 oracle. COUNTED-RESTART discipline applies."

    local round phase_ms dd_pid t_kill t_up out findings drift fence_ref trip backstops fm
    printf '%-6s %-9s %-9s %-10s %-6s %-10s %-6s %s\n' ROUND PHASE_MS REMOUNT_S FSCK DRIFT FENCE_REF TRIP VERDICT | tee "$rowdir/matrix.tsv"
    for ((round = 1; round <= S7_ROUNDS; round++)); do
        # Sustained load, randomized kill phase (0.5 .. 8.5 s into it).
        rm -f "$w_mnt/s7kill.dat" 2>/dev/null || true
        (exec dd if=/dev/zero of="$w_mnt/s7kill.dat" bs=1M count=16384 conv=fsync status=none) &
        dd_pid=$!
        phase_ms=$((500 + RANDOM % 8000))
        sleep "$(python3 -c "print($phase_ms/1000)")"
        kill -0 "$dd_pid" 2>/dev/null ||
            die "round $round: write load died before the kill phase (${phase_ms}ms)"
        "$MWFLEET" kill 0 --sig 9
        t_kill="$(date +%s)"
        kill -9 "$dd_pid" 2>/dev/null || true
        wait "$dd_pid" 2>/dev/null || true
        # Sweep the dead FUSE mount, then the successor takes the D0 ladder
        # AND the WERO takeover (same host identity: the register ladder
        # replaces the dead incarnation's registration; the acquire lands
        # on the standing rtype-3 reservation).
        umount -l "$w_mnt" 2>/dev/null || true
        wait_for_unmounted "$w_mnt"
        "$MWFLEET" mount 0 ||
            die "round $round: successor remount FAILED (the WERO takeover or the D0 ladder refused)"
        t_up="$(date +%s)"
        fm="$(stat_field 0 data_plane_fence_mode)"
        [ "$fm" = "1" ] || die "round $round: successor data_plane_fence_mode=$fm (want 1)"
        # The oracle: FULL online fsck (C1-C10, C8 ungated on this stamped
        # format — the durable ledger runs for real).
        out="$("$SQZ" fsck "$w_mnt" 2>&1)" ||
            die "round $round: online fsck FAILED or found:
$out"
        echo "$out" >"$rowdir/fsck-r$round.out"
        echo "$out" | grep -q "findings: 0" ||
            die "round $round: fsck findings != 0:
$out"
        findings=0
        drift="$(stat_field 0 meta_kv_block_refs_drift)"
        [ "$drift" = "0" ] || die "round $round: meta_kv_block_refs_drift=$drift (C8 oracle RED)"
        fence_ref="$(stat_field 0 data_dma_fence_refusals)"
        [ "$fence_ref" = "0" ] || die "round $round: successor data_dma_fence_refusals=$fence_ref (a fresh mount fenced itself)"
        trip="$(stat_field 0 invariant_tripwires)"
        [ "$trip" = "0" ] || die "round $round: invariant_tripwires=$trip on the successor"
        backstops="$(stat_field 0 mem_budget_hard_backstops)"
        [ "$backstops" = "0" ] || die "round $round: mem_budget_hard_backstops=$backstops (R5 column)"
        printf '%-6s %-9s %-9s %-10s %-6s %-10s %-6s %s\n' "$round" "$phase_ms" "$((t_up - t_kill))" "findings:$findings" "$drift" "$fence_ref" "$trip" GREEN | tee -a "$rowdir/matrix.tsv"
    done

    # Matrix-end fleet health: the reader must have re-joined the LAST
    # successor and its rung-6 tripwire must be flat.
    if [ -n "$reader_idx" ]; then
        local ttl ttl_s renew_est
        ttl="$(stat_field 0 membership_lease_ttl_ms)"
        ttl_s=$((ttl / 1000))
        renew_est="$(owner_renew_est_s)"
        wait_stat_eq "$reader_idx" membership_mode member $((ttl_s + 6 * renew_est + 90)) "reader re-join after the matrix"
        [ "$(stat_field "$reader_idx" meta_kv_revalidate_dirty_skips)" = "0" ] ||
            die "reader dirty_skips != 0 after the matrix (the rung-6 finding-#1 tripwire)"
        [ "$(stat_field "$reader_idx" invariant_tripwires)" = "0" ] ||
            die "reader invariant_tripwires != 0 after the matrix"
        log "reader m$reader_idx healthy after the matrix (member, dirty_skips=0, tripwires=0)"
    fi
    log "s7-kill-matrix GREEN: $S7_ROUNDS/$S7_ROUNDS rounds (table + fsck reports in $rowdir)"
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
s6-journal) leg_s6_journal ;;
s6-fence) leg_s6_fence ;;
s6-vm-fence) leg_s6_vm_fence ;;
s7-device-fence) leg_s7_device_fence ;;
s7-kill-matrix) leg_s7_kill_matrix ;;
cowriters-admission) leg_cowriters_admission ;;
vm-hostscope-validate) leg_vm_hostscope_validate ;;
vm-multi-identity) leg_vm_multi_identity ;;
*) die "unknown leg '$LEG' (smoke|multipath-negative|s6-journal|s6-fence|s6-vm-fence|s7-device-fence|s7-kill-matrix|cowriters-admission|vm-hostscope-validate|vm-multi-identity)" ;;
esac
