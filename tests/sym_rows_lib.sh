# shellcheck shell=bash
#
# sym_rows_lib.sh — the symmetric acceptance rows' LAWS, in one place.
#
# Sourced by tests/run_mw_matrix.sh (the fleet/box venue — its sym-tarx /
# sym-scale / sym-shared-dir(+-ls) legs) AND by tests/cloud_sym_rows.sh
# (the multi-node cloud venue, PR 15). Every function here is venue-blind:
# it reads NUMBERS or SNAPSHOT FILES (`<rowdir>/m<idx>_p<label><0|1>.json`,
# the `.stats` inode captured before/after a row) and prints the verdict
# text or dies on a violated engagement law. Nothing here touches a mount,
# a daemon or a host — the harness that sources it owns those.
#
# The law lives HERE so the cloud row and the box row can never drift
# (design-symmetric-metadata §8 gates 2 / 3 / 3b; PR 15's brief: "ONE law
# per row with the matrix's own .stats keys and verdict text — do not fork
# the laws"). Change a threshold here and both venues move together.
#
# The sourcing harness must define: die, log, warn, SYM_VENUE
# (laptop | box | cloud), SYM_VENUE_LEDGER (where venue-attributed
# readings are appended) and SYM_LOG_TAG (its stderr prefix — the matrix's
# `[mwmatrix]`, the cloud driver's `[sym-rows]`; default `[sym-rows]`).

# The symmetric must-stay-0 set on one daemon (the sym-storm set + the
# cross-owner and token tripwires); `dlm_rpcs` is asserted ABSOLUTE by the
# rows: an own-slot op never pays a lock round trip (gate 1's law on every
# rung).
SYM_ZERO_KEYS="meta_kv_forest_key_violations appender_fence_breach foreign_frame_overwrite_detected \
    manager_verb_refusals meta_kv_replay_key_violations meta_kv_replay_lease_violations \
    meta_kv_replay_extent_violations fsck_slot_custody_conflicts slot_lease_conflicts \
    appender_park_expiries meta_kv_leaf_lease_refusals dlm_token_recall_timeouts_live \
    appender_flush_ceiling_overruns dead_member_write_deferrals data_alloc_bitmap_drift \
    joined_control_refusals xv_cross_owner_intents_stuck manager_dependency_stalls \
    dlm_token_custody_rejected invariant_tripwires data_dma_fence_refusals \
    extent_grant_conflicts extent_return_live_refusals"

# --- snapshot-file readers ---------------------------------------------------

# Σ-folded delta of one stats key between the `p<label>0` and `p<label>1`
# snapshots (the symmetric families publish PER VOLUME as JSON arrays —
# a scalar is its own sum).
sym_delta() { # rowdir idx label key
    python3 - "$1" "$2" "$3" "$4" <<'PYEOF'
import json, sys
rowdir, idx, label, key = sys.argv[1:5]
def flat(d, out=None, pfx=""):
    out = {} if out is None else out
    for k, v in d.items():
        if isinstance(v, dict): flat(v, out, pfx + k + ".")
        else: out[pfx + k] = v
    return out
def fold(v):
    if isinstance(v, list):
        return sum(x for x in v if isinstance(x, (int, float)))
    return v if isinstance(v, (int, float)) else 0
def load(ph):
    root = json.load(open(f"{rowdir}/m{idx}_p{label}{ph}.json"))
    return flat(root.get("metrics", root))
a, b = load(0), load(1)
print(int(fold(b.get(key, 0)) - fold(a.get(key, 0))))
PYEOF
}

# One flattened stats key of a captured `.stats` FILE (the stats nest under
# "metrics"); empty when absent.
sym_json_field() { # file key
    python3 -c '
import json, sys
def flat(d, out, pfx=""):
    for k, v in d.items():
        if isinstance(v, dict): flat(v, out, pfx + k + ".")
        else: out[pfx + k] = v
    return out
root = json.load(open(sys.argv[1]))
d = flat(root.get("metrics", root), {})
d.update({k: v for k, v in root.items() if not isinstance(v, dict)})  # top-level words (build_commit, …)
print(d.get(sys.argv[2], ""))' "$1" "$2"
}

# A per-volume gauge (a JSON array) of a captured `.stats` FILE, folded:
# the SUM of its numeric elements (a scalar is its own sum; absent = 0).
sym_json_sum() { # file key
    python3 -c '
import json, sys
def flat(d, out, pfx=""):
    for k, v in d.items():
        if isinstance(v, dict): flat(v, out, pfx + k + ".")
        else: out[pfx + k] = v
    return out
root = json.load(open(sys.argv[1]))
v = flat(root.get("metrics", root), {}).get(sys.argv[2], 0)
if isinstance(v, list):
    print(sum(x for x in v if isinstance(x, (int, float))))
elif isinstance(v, (int, float)):
    print(v)
else:
    print(0)' "$1" "$2"
}

# Every element of a per-volume gauge of a captured `.stats` FILE equals
# `want` (a scalar compared directly); prints 1/0.
sym_json_all_eq() { # file key want
    python3 -c '
import json, sys
def flat(d, out, pfx=""):
    for k, v in d.items():
        if isinstance(v, dict): flat(v, out, pfx + k + ".")
        else: out[pfx + k] = v
    return out
root = json.load(open(sys.argv[1]))
v = flat(root.get("metrics", root), {}).get(sys.argv[2], None)
want = sys.argv[3]
vals = v if isinstance(v, list) else [v]
print(1 if vals and all(str(x) == want for x in vals) else 0)' "$1" "$2" "$3"
}

# The first element of a per-volume gauge of a captured `.stats` FILE (a
# scalar is itself; a string element without its repr quotes).
sym_json_first() { # file key
    python3 -c '
import json, sys
def flat(d, out, pfx=""):
    for k, v in d.items():
        if isinstance(v, dict): flat(v, out, pfx + k + ".")
        else: out[pfx + k] = v
    return out
root = json.load(open(sys.argv[1]))
d = flat(root.get("metrics", root), {})
d.update({k: v for k, v in root.items() if not isinstance(v, dict)})
v = d.get(sys.argv[2], "")
if isinstance(v, list):
    v = v[0] if v else ""
print(v)' "$1" "$2"
}

# Did any per-volume element of a CUMULATIVE gauge DECREASE between two
# captured `.stats` files? Prints 1/0. The reader's Token family is summed
# over its per-holder planes, and a plane REPLACED mid-window (its holder's
# endpoint died — a rejoined writer at a fresh port) takes its history
# with it: the row's delta then reads short, never a law's MISS but an
# INSTRUMENT the row must refuse to judge (PR 15's local pass: volume 2's
# `dlm_token_grants` 13,861 → 16,000 with `recalls_received` 7,861 → 0).
# Feed it COUNTERS only — `dlm_token_cached` is a LEVEL (the cache's
# `len()`; a byte-budget eviction or a recall inside the window is a
# legitimate decrease).
sym_json_any_decrease() { # file0 file1 key
    python3 -c '
import json, sys
def flat(d, out, pfx=""):
    for k, v in d.items():
        if isinstance(v, dict): flat(v, out, pfx + k + ".")
        else: out[pfx + k] = v
    return out
def load(f):
    r = json.load(open(f)); return flat(r.get("metrics", r), {})
a, b = load(sys.argv[1]).get(sys.argv[3], 0), load(sys.argv[2]).get(sys.argv[3], 0)
la = a if isinstance(a, list) else [a]
lb = b if isinstance(b, list) else [b]
dec = any(isinstance(x, (int, float)) and isinstance(y, (int, float)) and y < x for x, y in zip(la, lb))
print(1 if dec else 0)' "$1" "$2" "$3"
}

# Did any per-volume element of a LEVEL gauge CHANGE between two captured
# `.stats` files? Prints 1/0 — the reader's `dlm_token_reader_holder_planes`
# per volume is the plane-replacement witness beside the counters above
# (a re-point that keeps the plane count is what the counters catch).
sym_json_any_change() { # file0 file1 key
    python3 -c '
import json, sys
def flat(d, out, pfx=""):
    for k, v in d.items():
        if isinstance(v, dict): flat(v, out, pfx + k + ".")
        else: out[pfx + k] = v
    return out
def load(f):
    r = json.load(open(f)); return flat(r.get("metrics", r), {})
a, b = load(sys.argv[1]).get(sys.argv[3], None), load(sys.argv[2]).get(sys.argv[3], None)
print(0 if a == b else 1)' "$1" "$2" "$3"
}

# --- the VENUE word ------------------------------------------------------------

# The VENUE word's one exemption (PR 13 review round 1, Issue 2 — the
# venue ruling `51bf21e1`): is `key` a timing-shaped gauge the laptop
# reports as venue-attributed instead of failing on? Exactly ONE gauge,
# and only under `--venue=laptop`. The cloud venue is a REAL venue (no
# thermal soak, one daemon per node): every law is fatal there, as on the
# box.
sym_zero_venue_attributed() { # key -> 0 (yes) | 1 (no)
    [ "$SYM_VENUE" = "laptop" ] && [ "$1" = "appender_flush_ceiling_overruns" ]
}

# Record a venue-attributed reading (never a verdict): the ledger every
# row's note copies, and a loud line on stderr.
sym_zero_venue_note() { # label idx key value
    local label="$1" idx="$2" k="$3" v="$4"
    mkdir -p "$(dirname "$SYM_VENUE_LEDGER")" 2>/dev/null || true
    echo "$label m$idx $k=$v venue=$SYM_VENUE" >>"$SYM_VENUE_LEDGER"
    echo "${SYM_LOG_TAG:-[sym-rows]} VENUE-ATTRIBUTED ($label): $k=$v on m$idx — a timing-shaped reading on the $SYM_VENUE; the box bracket decides (51bf21e1), never a MISS here" >&2
}

# Judge ONE must-stay-0 gauge: die, or — the venue word's exemption —
# report it and continue.
sym_zero_judge() { # label idx key value
    local label="$1" idx="$2" k="$3" v="$4"
    [ "$v" = "0" ] && return 0
    if sym_zero_venue_attributed "$k"; then
        sym_zero_venue_note "$label" "$idx" "$k" "$v"
        return 0
    fi
    die "$label: $k=$v on m$idx (must stay 0)"
}

# The must-stay-0 set judged as a per-row DELTA between the row's two
# snapshots (`<label>0` / `<label>1`): a cumulative gauge another row (or
# a previous run on the same fleet) moved is that row's finding, not this
# one's. Prints `key=+delta` per violation (nothing on a clean daemon).
sym_zero_violations_delta() { # rowdir idx label
    local rowdir="$1" idx="$2" label="$3" k v
    for k in $SYM_ZERO_KEYS; do
        v="$(sym_delta "$rowdir" "$idx" "$label" "$k")"
        [ "$v" = "0" ] && continue
        if sym_zero_venue_attributed "$k"; then
            sym_zero_venue_note "$label" "$idx" "$k" "+$v"
            continue
        fi
        printf '%s=+%s ' "$k" "$v"
    done
}

# The must-stay-0 set of one captured `.stats` FILE, judged absolutely
# (the oracle's face — `sym_zero_judge` per key).
sym_zero_keys_file() { # label idx file
    local label="$1" idx="$2" file="$3" k v
    for k in $SYM_ZERO_KEYS; do
        v="$(sym_json_sum "$file" "$k")"
        sym_zero_judge "$label" "$idx" "$k" "$v"
    done
}

# The set plus the WRITER's posture word: `symmetric_meta == 1` on every
# volume. A READER arms no slot leases (`symmetric_meta` is the writer's
# word); its posture word is `reader_staleness_bound_ms == 0` (R-SYM-4) —
# `sym_zero_reader_file` judges that face.
sym_zero_set_file() { # label idx file
    sym_zero_keys_file "$1" "$2" "$3"
    [ "$(sym_json_all_eq "$3" symmetric_meta 1)" = "1" ] ||
        die "$1: symmetric_meta != 1 on every volume of m$2"
}
sym_zero_reader_file() { # label idx file
    sym_zero_keys_file "$1" "$2" "$3"
    [ "$(sym_json_field "$3" reader_staleness_bound_ms)" = "0" ] ||
        die "$1: reader_staleness_bound_ms != 0 on reader m$2 (R-SYM-4)"
}

# --- deleted stays deleted -----------------------------------------------------

# **The ONE deleted-stays-deleted classifier** (PR 13 review round 1, Issue
# 8): a removed name's `stat` through a mount is `deleted` ONLY on ENOENT.
# A resolved name is `resurrected`; a stat past its bound is `hung` (a
# parked lookup — a red with its daemon named, never a leg that hangs);
# any other failure is `error:<text>` — an EIO / EAGAIN / a refused read
# is a daemon that CANNOT ANSWER, never "gone" (defect 29 was found
# because a fail-stopped daemon's EIO on every removed name read as
# GREEN). Every arm of the oracle (storm, scale, crash, the cloud row)
# reads this word. The classification is split from the `stat` so a
# REMOTE harness feeds it the same (rc, stderr) pair its ssh brought home.
sym_stat_deleted_classify() { # rc stderr-text -> deleted|resurrected|hung|error:<text>
    local src="$1" err="$2"
    if [ "$src" = "124" ]; then
        echo hung
    elif [ "$src" = "0" ]; then
        echo resurrected
    elif echo "$err" | grep -q "No such file"; then
        echo deleted
    else
        echo "error:$err"
    fi
}

sym_stat_deleted() { # path [timeout_s] -> deleted|resurrected|hung|error:<text>
    local path="$1" bound="${2:-60}" err="" src
    if err="$(timeout "$bound" stat "$path" 2>&1 >/dev/null)"; then
        src=0
    else
        src=$?
    fi
    sym_stat_deleted_classify "$src" "$err"
}

# --- gate 2: sym-tarx ------------------------------------------------------------

# THE ENGAGEMENT LAW (§8 gate 2): wire verbs per entry ≈ 0 — the
# destination directory's mkdir under `/` is the ONE shipped step (the
# root's dentries are slot 0's), the extent-grant refills a handful of
# manager verbs per thousand leaves; 0.05 is the bound. No handover on
# either side (a handover inside a local extraction), `dlm_rpcs` 0 on the
# extracting writer. Prints verbs/entry; dies on a violation.
sym_law_gate2_engagement() { # label entries wire xv ship pub handovers_joiner handovers_manager rpcs
    local label="$1" entries="$2" wire="$3" xv="$4" ship="$5" pub="$6" h_j="$7" h_m="$8" rpcs="$9"
    local verbs_per
    verbs_per="$(python3 -c "print(f'{($wire+$xv+$ship+$pub)/$entries:.4f}')")"
    python3 -c "import sys; sys.exit(0 if ($wire+$xv+$ship+$pub)/$entries < 0.05 else 1)" ||
        die "sym-tarx $label: $verbs_per wire verbs per entry (wire=$wire xv=$xv ship=$ship pub=$pub over $entries) — the joiner did not extract into its OWN slot tree; the row is INVALID, not slow"
    [ "$h_j" = "0" ] && [ "$h_m" = "0" ] ||
        die "sym-tarx $label: slot_handovers moved (joiner $h_j, manager $h_m) — a handover inside a local extraction"
    [ "$rpcs" = "0" ] || die "sym-tarx $label: dlm_rpcs=$rpcs on the joiner (must be 0)"
    echo "$verbs_per"
}

# The gate-2 verdict over the A-B-B-A (sym-1 local-1 local-2 sym-2 walls,
# seconds): the joined writer's mean wall ≤ 1.10 × the S0 mean. S0 is the
# MANAGER's own extract on its mount with the fleet's other members mounted
# and idle (the matrix's `local_arm`; the cloud driver's too) — the same
# shape on every venue, never a solo mount.
sym_law_gate2_verdict() { # s1 s2 l1 l2
    python3 - "$1" "$2" "$3" "$4" <<'PYGATE'
import sys
s = (float(sys.argv[1]) + float(sys.argv[2])) / 2
l = (float(sys.argv[3]) + float(sys.argv[4])) / 2
r = s / l
print(f"gate 2: joined writer {s:.2f}s vs manager-local {l:.2f}s -> {r:.2f}x of S0 (gate <= 1.10x): {'MET' if r <= 1.10 else 'MISS'}")
print(f"both orders: sym-1 {float(sys.argv[1]):.2f} local-1 {float(sys.argv[3]):.2f} | local-2 {float(sys.argv[4]):.2f} sym-2 {float(sys.argv[2]):.2f}")
PYGATE
}

# --- gate 3: sym-scale ------------------------------------------------------------

# THE ENGAGEMENT LAW (§8 gate 3): every mount in its own slot trees — no
# handover, no ship (≤ 1 per writer: `slot_ships` counts every served
# ship since PR 13 (defect 9); each writer's ONE `mkdir /<top>` under `/`
# is §5.10's "1 ship to volume 0's manager"), no lock RPC. Dies on a
# violation.
sym_law_gate3_engagement() { # n handovers ships rpcs
    local n="$1" handovers="$2" ships="$3" rpcs="$4"
    [ "$handovers" = "0" ] || die "sym-scale N=$n: slot_handovers=$handovers (must be 0 — each mount writes its own trees)"
    [ "$ships" -le "$n" ] || die "sym-scale N=$n: slot_ships=$ships (must be ≈ 0 — at most one per writer: its directory's mkdir under /)"
    [ "$rpcs" = "0" ] || die "sym-scale N=$n: Σ dlm_rpcs=$rpcs (must be 0)"
}

# The per-N verdict: ≥ 0.7 × N × the N=1 rate on BOTH rows (creates/s and
# ingest MiB/s). Prints MET | MISS.
sym_law_gate3_row() { # n create_rate create_rate_n1 ingest_rate ingest_rate_n1
    python3 -c "print('MET' if $2 >= 0.7*$1*$3 and $4 >= 0.7*$1*$5 else 'MISS')"
}

# The gate-3 table header (the `C/CPU-S` column is PR 13c's — the
# co-located venue's reading; on one node per writer it stands beside the
# wall multiple as the per-daemon economy).
sym_gate3_header() {
    printf '%-4s %-10s %-8s %-9s %-10s %-8s %-9s %-8s %-8s %-8s %-6s %s\n' N CREATE_S RATIO C/CPU-S INGEST_MBS RATIO MGR_LOAD MGR_CPU HANDOV SHIPS RPCS VERDICT
}
sym_gate3_row_line() { # n create_rate cr creates_per_cpu_s ingest_rate ir mgr_load mgr_cpu handovers ships rpcs verdict
    printf '%-4s %-10s %-8s %-9s %-10s %-8s %-9s %-8s %-8s %-8s %-6s %s\n' "$1" "$2" "${3}x" "$4" "$5" "${6}x" "$7" "${8}%" "$9" "${10}" "${11}" "${12}"
}
sym_law_gate3_verdict_line() { # verdict_all
    echo "gate 3 (≥ 0.7 × N × the N=1 rate on BOTH rows; the manager's load flat in N): $1"
}

# --- gate 3b: sym-shared-dir (+ -ls) --------------------------------------------

# THE ENGAGEMENT LAW (§8 gate 3b): exactly ONE flip, at the holder; the
# directory striped; every foreign create either a served cross-owner
# step (pre-flip) or a stripe ship (post-flip) — the shipped steps of the
# creators ≡ the served steps at the holders (the closure), stripe ships
# > 0, no handover anywhere. Dies on a violation.
sym_law_gate3b_engagement() { # holder_idx flips flip_at striped stripe_ships shipped served handovers
    local holder="$1" flips="$2" flip_at="$3" striped="$4" stripe_ships="$5" shipped="$6" served="$7" handovers="$8"
    [ "$flips" = "1" ] || die "sym-shared-dir: dir_stripe_flips=$flips (want exactly 1: the holder's flip on the observed creator count) — [$flip_at]"
    [ -n "$flip_at" ] && [ "${flip_at% *}" = "m$holder(1)" ] ||
        die "sym-shared-dir: the flip landed at [$flip_at], not at the holder m$holder"
    [ "$striped" -ge 1 ] || die "sym-shared-dir: dir_striped_dirs=$striped at the holder"
    [ "$stripe_ships" -gt 0 ] || die "sym-shared-dir: dir_stripe_ships=0 — no post-flip create was routed to a stripe holder"
    [ "$shipped" = "$served" ] || die "sym-shared-dir: shipped steps $shipped ≠ served steps $served (the closure) — a step was lost or double-served"
    [ "$handovers" = "0" ] || die "sym-shared-dir: slot_handovers=$handovers (aggregate shipping never triggers a handover)"
}

# THE `-ls` ENGAGEMENT LAW: one token per stripe + one per child (+ the
# directory and its parent's own and the reader's root token — the
# constant beside K + C; the first run read K + C + 3 exactly), and ZERO
# device leaf reads — every record came in a grant. "0 leaf reads" is
# judged NET of the S5 control plane: the reader's poll drops its
# projected images at every epoch step and re-reads tree 0 / the roots
# (`meta_kv_revalidate_nodes_dropped`, ≤ a few nodes per epoch), plus ONE
# tree-0 read per stripe SLOT (the reader resolves each stripe's lessee
# off its own tree 0 before it dials the holder — K reads, cached after).
# Dies on a violation.
sym_law_gate3b_ls() { # grants K C misses dropped epochs merges
    local grants="$1" k="$2" c="$3" misses="$4" dropped="$5" epochs="$6" merges="$7"
    [ "$grants" -ge $((k + c)) ] && [ "$grants" -le $((k + c + 4)) ] ||
        die "sym-shared-dir-ls: dlm_token_grants=$grants ∉ [K + C, K + C + 4] = [$((k + c)), $((k + c + 4))]"
    [ "$misses" -le $((dropped + epochs * 8 + k)) ] ||
        die "sym-shared-dir-ls: meta_kv_node_cache_misses=$misses on the reader exceeds the poll's own re-reads + one tree-0 read per stripe slot (dropped $dropped + 8 × $epochs epochs + K $k) — a DATA leaf was read for the listing"
    [ "$merges" -ge 1 ] || die "sym-shared-dir-ls: dir_stripe_readdir_merges=$merges on the reader (the K-way merge did not run)"
}

# --- acked writes present (the oracle's presence half) --------------------------------

# The census of a directory TREE as one mount sees it: `entries bytes` —
# every entry below the root (files, directories, symlinks; the root
# itself excluded) and Σ regular-file bytes. Run on the node that holds
# the mount (python3 -c "$SYM_TREE_CENSUS_PY" <dir>); the harness runs it
# on the WRITING mount right after the writes were acked and again through
# ANOTHER mount, and `sym_law_acked_tree` judges the two equal — an acked
# entry a peer cannot see is a lost write, never a slow one.
# shellcheck disable=SC2034  # read by the harnesses that source this lib
SYM_TREE_CENSUS_PY='
import os, sys
root = sys.argv[1]
n = 0; b = 0
for d, dirs, files in os.walk(root):
    n += len(dirs) + len(files)
    for f in files:
        p = os.path.join(d, f)
        st = os.lstat(p)
        if not os.path.islink(p):
            b += st.st_size
print(n, b)'

# One file read back whole: `bytes zero_ok` — its size and whether every
# byte is zero (the ingest rows write /dev/zero; a size-consistent file
# of the wrong bytes is the staged-payload-lost class, `zero_ok` = 0).
# shellcheck disable=SC2034
SYM_ZERO_FILE_PY='
import sys
p = sys.argv[1]
n = 0; ok = 1
with open(p, "rb") as f:
    while True:
        c = f.read(4 << 20)
        if not c: break
        n += len(c)
        if ok and c.count(0) != len(c): ok = 0
print(n, ok)'

# THE ACKED-WRITES LAW (a tree): what the writer acked ≡ what another mount
# reads — entries and bytes both. Dies on a difference.
sym_law_acked_tree() { # label want_entries got_entries want_bytes got_bytes via
    local label="$1" we="$2" ge="$3" wb="$4" gb="$5" via="$6"
    [ "$we" = "$ge" ] && [ "$wb" = "$gb" ] ||
        die "$label: ACKED WRITES NOT PRESENT through $via — the writer saw $we entries / $wb bytes, the read-back sees $ge entries / $gb bytes (a lost acked write, never a slow one)"
}

# THE ACKED-WRITES LAW (an ingest file): the fsynced file's bytes read back
# whole through another mount, every byte the zero the writer wrote. Dies
# on a difference.
sym_law_acked_ingest() { # label want_bytes got_bytes zero_ok via
    local label="$1" wb="$2" gb="$3" z="$4" via="$5"
    [ "$wb" = "$gb" ] ||
        die "$label: ACKED INGEST NOT PRESENT through $via — $wb bytes fsynced, $gb bytes read back"
    [ "$z" = "1" ] ||
        die "$label: ACKED INGEST CORRUPT through $via — $gb bytes read back but not the zeros the writer wrote (the staged-payload-lost class)"
}

# --- the oracle's JSON face --------------------------------------------------------

# `squeezefs fsck <mnt> --json` output → the finding count (findings +
# the elided tail — an elided finding IS a finding); a non-JSON body is
# reported as -1 so the caller can fail loud with the transcript.
sym_fsck_json_findings() { # file
    python3 -c '
import json, sys
try:
    r = json.load(open(sys.argv[1]))
except Exception:
    print(-1); sys.exit(0)
print(len(r.get("findings", [])) + int(r.get("findings_elided", 0)))' "$1"
}
