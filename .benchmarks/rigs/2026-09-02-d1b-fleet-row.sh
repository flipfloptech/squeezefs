#!/usr/bin/env bash
# D-1b fleet row: the CONCURRENT-STREAM co-writer ingest shape the publish
# plane's framing lever engages on (docs/design-e2e-perf-audit.md §3 DLM
# board #8; .benchmarks/2026-09-02-d1b-publish-plane-batching.md).
#
# The s9-fanout row is ONE dd stream per member — a single ino's publishes
# serialize on its 3.5 stripe, so the client-side frame never carries more
# than one call there and the lever is structurally invisible. This row
# runs STREAMS concurrent `dd conv=fsync` files per co-writer (the
# charter's 24-file streaming co-writer), so each co-writer's per-block
# publishes ARRIVE concurrently at its publish lane.
#
# Source is /dev/zero ON PURPOSE: the data namespaces are zram, zeros
# compress to nothing, and that is what removes the DEVICE term and exposes
# the metadata/publish-plane ceiling this row is about (label it so).
#
# Per leg (one fleet at a time, torn down to zero residue between legs):
#   create fleet(SQZ_BIN) N=1 --multi-writer --cowriters=K  ->  row  ->  teardown
# A-B-B-A over two binaries: A = the control (dev tip), B = the D-1b branch.
#
# Usage (root; quiet box — refuses while `task check` / foreign cargo run):
#   A_BIN=/path/squeezefs.dev B_BIN=/path/squeezefs.d1b \
#     COWRITERS=8 STREAMS=24 MB=128 \
#     sudo -n -E env "PATH=$PATH" bash .benchmarks/rigs/2026-09-02-d1b-fleet-row.sh
#   ROW_ONLY=1 ... : one row on an ALREADY-CREATED fleet (no create/teardown).
#
# Row columns (per co-writer and aggregate): MiB/s, publishes shipped,
# frames shipped, frames/publish (the engagement instrument — <<1 is the
# lever engaging; a control binary without the ledger reads 1.0 by
# construction, one frame per publish), owner passes/publish
# (meta_conveyor_leader_passes delta / meta_ship_publish.served delta),
# served_chains/served_frames, ledger closure (shipped ~= served),
# refusals/panics 0, ship_depth_waits, ship_session_dials.
set -eu
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
FLEET="$REPO/tests/mw_fleet.sh"
STATE="${SQZ_MWFLEET_STATE_DIR:-/run/squeezefs-mwfleet}"
MNT_ROOT="${SQZ_MWFLEET_MNT_ROOT:-/mnt/sqz-mwfleet}"
MEMBERS="$STATE/members.tsv"
OUT="${OUT:-$REPO/target/d1b-fleet-$(date +%Y%m%d-%H%M%S)}"
COWRITERS="${COWRITERS:-8}"
STREAMS="${STREAMS:-24}"
MB="${MB:-128}"
OSS_GB="${SQZ_MWFLEET_OSS_GB:-64}"
ROW_ONLY="${ROW_ONLY:-0}"
mkdir -p "$OUT"

log() { echo "[d1b-row] $*"; }
die() { echo "[d1b-row] ERROR: $*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || die "root required (fleet create/teardown, mounts)"
if pgrep -f "task check" >/dev/null || pgrep -x cargo >/dev/null; then
    die "the box is not quiet (task check / cargo running) — a fleet row is a MEASUREMENT"
fi

mnt_of() { echo "$MNT_ROOT/m$1"; }
cowriter_idxs() { awk -F'\t' '$2=="cowriter" {print $1}' "$MEMBERS" | sort -n; }
snap() { cat "$(mnt_of "$1")/.stats" >"$2/m$1_$3.json"; }

run_row() { # leg-label
    local leg="$1" rowdir="$OUT/$1" cws idx i pids=() t0 t1
    mkdir -p "$rowdir"
    mapfile -t cws < <(cowriter_idxs)
    [ "${#cws[@]}" -ge 1 ] || die "no co-writer members in $MEMBERS"
    log "$leg: ${#cws[@]} co-writers x $STREAMS streams x ${MB} MiB (dd bs=1M conv=fsync, /dev/zero); loadavg=$(cut -d' ' -f1-3 /proc/loadavg)"
    # Every member mounts the SAME filesystem: per-member directories, or
    # eight co-writers create the same 24 names in one directory and the
    # row measures S10 intent EEXIST refusals + custody conflicts instead
    # (the s9-fanout row's per-member naming, for the same reason).
    for idx in "${cws[@]}"; do
        mkdir -p "$(mnt_of "$idx")/d1b-m$idx"
    done
    snap 0 "$rowdir" p0
    for idx in "${cws[@]}"; do snap "$idx" "$rowdir" p0; done
    t0="$(date +%s.%N)"
    for idx in "${cws[@]}"; do
        (
            local st=() j rc=0 a b
            a="$(date +%s.%N)"
            for ((j = 0; j < STREAMS; j++)); do
                dd if=/dev/zero of="$(mnt_of "$idx")/d1b-m$idx/s$j.dat" bs=1M count="$MB" \
                    conv=fsync status=none 2>"$rowdir/m$idx-s$j.err" &
                st+=("$!")
            done
            for p in "${st[@]}"; do wait "$p" || rc=$?; done
            b="$(date +%s.%N)"
            echo "$rc $a $b" >"$rowdir/wall-m$idx"
        ) &
        pids+=("$!")
    done
    wait "${pids[@]}" || true
    t1="$(date +%s.%N)"
    sleep 2
    snap 0 "$rowdir" p1
    for idx in "${cws[@]}"; do snap "$idx" "$rowdir" p1; done
    for idx in "${cws[@]}"; do
        read -r rc _ _ <"$rowdir/wall-m$idx"
        [ "$rc" = "0" ] || die "$leg: co-writer m$idx had a failed stream (rc=$rc): $(cat "$rowdir"/m$idx-s*.err | head -3)"
    done
    echo "$t0 $t1" >"$rowdir/wall"
    python3 - "$rowdir" "$STREAMS" "$MB" "$leg" "${cws[@]}" <<'PY' | tee "$rowdir/table.txt"
import json, sys
rowdir, streams, mb, leg = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4]
cws = sys.argv[5:]
def flat(d, out=None, pfx=""):
    out = {} if out is None else out
    for k, v in d.items():
        if isinstance(v, dict): flat(v, out, pfx + k + ".")
        else: out[pfx + k] = v
    return out
def load(i, ph):
    root = json.load(open(f"{rowdir}/m{i}_{ph}.json"))
    return flat(root.get("metrics", root))
def d(a, b, k):
    return int(b.get(k, 0) or 0) - int(a.get(k, 0) or 0)
t0, t1 = map(float, open(f"{rowdir}/wall").read().split())
total_mib = 0
sum_ship = sum_frames = sum_framed = sum_waits = sum_dials = 0
print(f"== D-1b leg {leg}: {len(cws)} co-writers x {streams} streams x {mb} MiB conv=fsync (/dev/zero: device term removed by design) ==")
print(f"{'member':<7}{'MiB/s':<9}{'pub_ship':<10}{'frames':<8}{'fr/pub':<8}{'calls/fr':<9}{'depth_w':<8}{'dials':<7}{'free_ship':<10}")
for i in cws:
    a, b = load(i, "p0"), load(i, "p1")
    rc, wa, wb = open(f"{rowdir}/wall-m{i}").read().split()
    mibs = streams * mb / (float(wb) - float(wa))
    total_mib += streams * mb
    ship = d(a, b, "meta_ship_publish.shipped")
    frames = d(a, b, "meta_ship_publish.ship_frames")
    framed = d(a, b, "meta_ship_publish.ship_framed_calls")
    has_ledger = "meta_ship_publish.ship_frames" in b
    if not has_ledger:
        frames, framed = ship, ship  # a pre-D-1b binary: one call per frame by construction
    waits = d(a, b, "meta_ship_publish.ship_depth_waits")
    dials = d(a, b, "meta_ship_publish.ship_session_dials")
    free_ship = d(a, b, "meta_ship_publish.free_shipped_blocks")
    fr_pub = frames / ship if ship else 0.0
    cpf = framed / frames if frames else 0.0
    tag = "" if has_ledger else " (no ledger: control)"
    print(f"m{i:<6}{mibs:<9.1f}{ship:<10}{frames:<8}{fr_pub:<8.3f}{cpf:<9.2f}{waits:<8}{dials:<7}{free_ship:<10}{tag}")
    sum_ship += ship; sum_frames += frames; sum_framed += framed; sum_waits += waits; sum_dials += dials
a0, a1 = load(0, "p0"), load(0, "p1")
served = d(a0, a1, "meta_ship_publish.served")
passes = d(a0, a1, "meta_conveyor_leader_passes")
entries = d(a0, a1, "meta_kv_journal_entries")
sframes = d(a0, a1, "meta_ship_publish.served_frames")
schains = d(a0, a1, "meta_ship_publish.served_chains")
refusals = int(a1.get("meta_ship_publish.refusals", 0) or 0)
panics = int(a1.get("meta_ship_publish.owner_panics", 0) or 0)
agg = total_mib / (t1 - t0) / 1024
print(f"\naggregate co-writer ingest {agg:.2f} GiB/s over {t1 - t0:.1f} s ({total_mib} MiB)")
print(f"client: shipped {sum_ship}, frames {sum_frames} -> frames/publish {sum_frames / sum_ship if sum_ship else 0:.3f}, calls/frame {sum_framed / sum_frames if sum_frames else 0:.2f}, depth waits {sum_waits}, session dials {sum_dials}")
print(f"owner : served {served}, served_frames {sframes}, served_chains {schains} (chains/frame {schains / sframes if sframes else 0:.2f}), conveyor passes {passes} -> passes/publish {passes / served if served else 0:.3f}, journal entries {entries}")
bad = []
if abs(served - sum_ship) > 4 * (len(cws) + 1):
    bad.append(f"ledger closure: shipped {sum_ship} vs served {served}")
if refusals or panics:
    bad.append(f"refusals={refusals} owner_panics={panics} (must stay 0)")
if sum_ship == 0:
    bad.append("no publish shipped — the row did not engage the publish plane")
if bad:
    print("ROW INVALID: " + "; ".join(bad))
    sys.exit(1)
print("row VALID (closure within instrument skew, tripwires flat)")
PY
    for idx in "${cws[@]}"; do rm -rf "$(mnt_of "$idx")/d1b-m$idx"; done
}

if [ "$ROW_ONLY" = "1" ]; then
    [ -f "$MEMBERS" ] || die "no fleet at $STATE (ROW_ONLY needs a created fleet)"
    run_row "row"
    exit 0
fi

: "${A_BIN:?A_BIN (the control binary) is required}"
: "${B_BIN:?B_BIN (the D-1b binary) is required}"
[ -x "$A_BIN" ] && [ -x "$B_BIN" ] || die "A_BIN/B_BIN must be executables"
[ -f "$MEMBERS" ] && die "a fleet already exists at $STATE — tear it down first (one fleet at a time)"

leg() { # label bin
    local label="$1" bin="$2"
    log "== leg $label: $("$bin" --version | head -1)"
    SQZ_BIN="$bin" SQZ_MWFLEET_OSS_GB="$OSS_GB" bash "$FLEET" create N=1 --multi-writer --cowriters="$COWRITERS" \
        >"$OUT/$label.create.log" 2>&1 || die "fleet create failed for $label — see $OUT/$label.create.log"
    run_row "$label" || { bash "$FLEET" teardown >"$OUT/$label.teardown.log" 2>&1 || true; die "row $label invalid"; }
    SQZ_BIN="$bin" bash "$FLEET" teardown >"$OUT/$label.teardown.log" 2>&1 || die "fleet teardown failed for $label"
    [ -f "$MEMBERS" ] && die "residue after teardown: $MEMBERS still exists"
    log "leg $label torn down to zero residue"
}
leg A1 "$A_BIN"
leg B1 "$B_BIN"
leg B2 "$B_BIN"
leg A2 "$A_BIN"
log "A-B-B-A complete — tables in $OUT/*/table.txt"
for l in A1 B1 B2 A2; do echo "--- $l"; grep -E "aggregate|client:|owner :" "$OUT/$l/table.txt"; done
