#!/usr/bin/env bash
# tests/pv_locate_gate.sh — THE SETUP PRECONDITION, as a gate
# (docs/design-per-volume-claim-admission.md §5.13 and PR 8 item 0,
# rev 3 / Issue 23)
# =============================================================================
#
# The design's words, which this script exists to make executable:
#
#   "The setup is part of the gate — a row run without it is INVALID, not
#    merely disappointing. The extraction target directory MUST be the
#    extracting node's own subtree root, minted on that node's volume by
#    the assignment verb (KD-PV-15, §5.5.1). Extracting into any directory
#    descended from root-on-the-set-authority reproduces the 6.73×
#    baseline BY CONSTRUCTION, because M2 pins every child to the parent's
#    owner. The leg therefore asserts, before the timed run: `volume
#    locate <target>` reports a volume the extracting node owns, and the
#    first ten creates under it show owner_of(child) == the extracting
#    node. This precondition is the single most likely way for a faithful
#    implementation to produce a meaningless row."
#
# So it is a SCRIPT, not a comment in a leg: an assertion that only ever
# runs inside a root-only fleet leg is an assertion nobody can exercise,
# and this one has to be exercisable — unprivileged, offline, against a
# file-backed set — or the acceptance rung ships an unverified gate.
#
# Two halves, in the design's own order:
#
#   (a) LOCATE — `squeezefs volume locate <target> <path> --json` must
#       report an owner, and that owner must be the node the row is about.
#       A path on an UNASSIGNED volume fails here (owner null), and so
#       does a path whose volume a PEER owns — which is exactly what a
#       root-descended extraction target looks like from a partial
#       authority.
#   (b) CREATES — the first N entries created under it must land on a
#       volume the same node owns (M2's engagement, §5.5.1). Live
#       mountpoints only: an sqmeta:// target cannot create, and asking
#       for creates against one is a refusal rather than a silent skip.
#
# Usage:
#   tests/pv_locate_gate.sh <target> <path> --owner <member-id>
#                           [--creates N] [--label TEXT] [--quiet]
#
#   <target>      a LIVE mountpoint (the fleet form: stat(2) answers with
#                 the current global ino) or an sqmeta:// URI (the offline
#                 form: the walk serves the writer's last checkpoint, so a
#                 just-created path may not be visible yet — the verb says
#                 so itself).
#   <path>        the absolute path INSIDE the filesystem (`/owner-m20`),
#                 never a host path under the mountpoint.
#   --owner       the extracting node's durable member id (KD-MW-2:
#                 `node_{16 hex}` or `node_{16 hex}.m{8 hex}`). A bare
#                 node id matches every mount slot of that node, which is
#                 `membership::member_id_matches`' own rule.
#   --creates N   run half (b) with N children (default 0 = half (a)
#                 only; PR 8's legs pass 10, the design's number).
#
# Env: SQZ_BIN (default target/release/squeezefs, falling back to debug —
#      the mw_fleet.sh discipline).
#
# Exit: 0 = the setup is valid. Nonzero = the row that would follow is
# INVALID; the refusal names which half failed, what it found, and what a
# valid setup looks like. Never a warning, never a skip.

set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SQZ="${SQZ_BIN:-$REPO/target/release/squeezefs}"
[ -x "$SQZ" ] || SQZ="$REPO/target/debug/squeezefs"

LABEL="setup"
QUIET=0

log() { [ "$QUIET" = "1" ] || echo "[pv-locate-gate] $*"; }
die() {
    echo "[pv-locate-gate] INVALID SETUP ($LABEL): $*" >&2
    exit 1
}

TARGET="${1:-}"
FSPATH="${2:-}"
{ [ -n "$TARGET" ] && [ -n "$FSPATH" ]; } || {
    awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
    exit 1
}
shift 2
OWNER=""
CREATES=0
while [ $# -gt 0 ]; do
    case "$1" in
    --owner)
        OWNER="${2:?--owner needs a member id}"
        shift 2
        ;;
    --owner=*)
        OWNER="${1#--owner=}"
        shift
        ;;
    --creates)
        CREATES="${2:?--creates needs a count}"
        shift 2
        ;;
    --creates=*)
        CREATES="${1#--creates=}"
        shift
        ;;
    --label)
        LABEL="${2:?--label needs text}"
        shift 2
        ;;
    --label=*)
        LABEL="${1#--label=}"
        shift
        ;;
    --quiet)
        QUIET=1
        shift
        ;;
    *) die "unknown argument '$1'" ;;
    esac
done

[ -n "$OWNER" ] || die "--owner <member-id> is required: the gate asks whether THIS node owns the extraction target, and without a node the question has no answer"
[[ "$CREATES" =~ ^[0-9]+$ ]] || die "--creates takes a non-negative integer (got '$CREATES')"
[[ "$FSPATH" == /* ]] || die "<path> must be absolute INSIDE the filesystem (got '$FSPATH') — never a host path under the mountpoint"
[ -x "$SQZ" ] || die "squeezefs binary not found at '$SQZ' (cargo build --release, or set SQZ_BIN)"
command -v python3 >/dev/null 2>&1 || die "python3 is required (the JSON reader)"

case "$TARGET" in
sqmeta://*)
    LIVE=0
    [ "$CREATES" -eq 0 ] ||
        die "--creates $CREATES against an sqmeta:// target: half (b) creates entries and an offline URI has no namespace to create in. Point the gate at the LIVE mountpoint of the node the row is about"
    ;;
*)
    LIVE=1
    mountpoint -q "$TARGET" ||
        die "'$TARGET' is not a mountpoint (the live form needs the extracting node's OWN mount; pass an sqmeta:// URI for the offline form)"
    ;;
esac

# KD-MW-2's own matching rule (membership::member_id_matches): ids match
# exactly, or a BARE node id (`node_{16 hex}`) names every mount slot of
# that node. Applied in both directions, because the record may carry
# either form and so may the caller.
id_matches() { # actual expected
    local a="$1" e="$2"
    [ "$a" = "$e" ] && return 0
    case "$a" in "$e".m*) return 0 ;; esac
    case "$e" in "$a".m*) return 0 ;; esac
    return 1
}

# One `volume locate` answer, flattened to `owner<TAB>volume_id<TAB>ino`.
# A locate that FAILS is the gate failing: the target must resolve.
#
# Streams stay SEPARATE (the first real --owners acceptance run, 2026-08-23):
# stdout is the machine-readable answer; stderr carries the daemon's startup
# diagnostics — the ENG-10 unregistered-knob announcements (the rig's own
# SQZ_BIN / SQZ_MWMATRIX_TAR_SRC land there by design) and the VAL-7i PATH
# line — and the merged `2>&1` form put those ahead of the JSON, failing the
# gate on a VALID setup. Stderr is still shown, but only in refusals.
locate_row() { # path -> owner \t volume_id \t ino
    local p="$1" out errf rc=0
    errf="$(mktemp)"
    out="$("$SQZ" volume locate "$TARGET" "$p" --json 2>"$errf")" || rc=$?
    if [ "$rc" -ne 0 ]; then
        local diag
        diag="$(cat "$errf")"
        rm -f "$errf"
        die "\`volume locate $TARGET $p\` failed — the extraction target must resolve before anything is timed:
$out
$diag"
    fi
    rm -f "$errf"
    printf '%s' "$out" | python3 -c '
import json, sys
r = json.load(sys.stdin)
print("%s\t%s\t%s" % (r.get("owner") or "", r.get("volume_id") or "?", r.get("ino") or "?"))' ||
        die "\`volume locate $TARGET $p --json\` did not answer JSON on stdout:
$out"
}

# --- half (a): the extraction target itself ---------------------------------
IFS=$'\t' read -r t_owner t_vol t_ino < <(locate_row "$FSPATH")
[ -n "$t_owner" ] ||
    die "\`volume locate\` reports NO OWNER for '$FSPATH' (volume $t_vol, ino $t_ino): this set carries no per-volume ownership assignment on that volume, so the row would measure a single-authority set. Assign it offline with \`squeezefs volume set-owners <uri> <vol-id>=<member-id>:<subtree-root>\`"
id_matches "$t_owner" "$OWNER" ||
    die "the extraction target '$FSPATH' lives on volume $t_vol (ino $t_ino), which '$t_owner' owns — not '$OWNER'. Every create under it ships to that peer and the row reproduces the 6.73x baseline BY CONSTRUCTION (M2 pins every child to the parent's owner). Extract into THIS node's own verb-minted subtree root; \`squeezefs volume get-owners\` names it"
log "(a) $FSPATH -> volume $t_vol (ino $t_ino) owner '$t_owner' == this node — OWNED"

# --- half (b): M2's engagement, on the first N children ---------------------
if [ "$CREATES" -gt 0 ]; then
    [ "$LIVE" = "1" ] || die "half (b) needs a live mountpoint"
    probe_dir="$TARGET/${FSPATH#/}/.pv-locate-gate.$$"
    mkdir -p "$probe_dir" ||
        die "cannot create the probe directory $probe_dir — a node that cannot create under its own subtree root cannot run the row either"
    cleanup() { rm -rf "$probe_dir" 2>/dev/null || true; }
    trap cleanup EXIT
    i=0
    while [ "$i" -lt "$CREATES" ]; do
        : >"$probe_dir/c$i" ||
            die "cannot create $probe_dir/c$i"
        IFS=$'\t' read -r c_owner c_vol c_ino < <(
            locate_row "${FSPATH%/}/.pv-locate-gate.$$/c$i"
        )
        [ -n "$c_owner" ] ||
            die "child c$i landed on volume $c_vol (ino $c_ino), which carries no owner — a partially assigned set cannot host this row"
        id_matches "$c_owner" "$OWNER" ||
            die "child c$i of the extraction target landed on volume $c_vol (ino $c_ino), owned by '$c_owner', not '$OWNER'. M2 says a child shares its parent's owner, so either the mint filter is not armed on this mount or the target is not what \`volume locate\` said it was — the row would be measuring the shipped path it exists to remove"
        i=$((i + 1))
    done
    cleanup
    trap - EXIT
    log "(b) $CREATES creates under $FSPATH -> owner '$OWNER' on every one — M2 ENGAGED"
fi

log "SETUP VALID ($LABEL): '$FSPATH' is '$OWNER''s own subtree on volume $t_vol"
