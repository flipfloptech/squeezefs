# 2026-08-22 — PR 8: per-volume claim admission, the ACCEPTANCE rung

**Status: SKELETON — every measured cell reads `PENDING`.** The rig,
the fleet builder, the setup gate and this structure landed on
`test/pv-acceptance`; the measured run needs a **PR-capable substrate**
(both per-volume arms refuse on a substrate whose device cannot reject a
fenced host's DMA, and the loop substrate cannot host Persistent
Reservations), which the box this was authored on does not have. The
commands that fill it in are in [How to run this row](#how-to-run-this-row)
— exact, in order, with the fields each gate harvests.

**Branch** `test/pv-acceptance` (off dev `900ce1a4`).
**Design rows**: `docs/design-per-volume-claim-admission.md` — the **PR 8**
row of the PR plan (setup precondition 0, gates (a)–(f), the closure
wording), **§5.12** (the post-recipe ceiling and its remaining funnels),
**§5.13** (the gate, the engagement law, the declared evidence tiers, the
cross-owner refusal table), **§5.1.3 / R14** (the priced W1 loss), and
rulings **D18 / D19 / D20**.
**Operator surface**: `docs/operations.md` §Bringing a multi-owner fleet up.

---

## What this note must contain before it is evidence

Nothing here may be filled in from arithmetic, from a prior run, or from a
different venue. Each row is either measured on the venue named in its own
row-label block, or it stays `PENDING` and the closing statement says so.
The three ways a row in this note becomes INVALID:

1. **The setup precondition** (design item 0, rev 3 / Issue 23) did not
   hold — the extraction target was not the extracting node's own
   verb-minted subtree root. Such a row reproduces the 6.73× baseline **by
   construction** and is INVALID, not disappointing. The rig makes this
   unrepresentable: `tests/pv_locate_gate.sh` runs before anything is
   timed and exits nonzero.
2. **The engagement law** (§5.13) did not hold — see
   [the engagement ledger](#the-engagement-ledger).
3. **The oracle** did not come back clean — `fsck_findings == 0` and
   `meta_kv_block_refs_drift == 0` on every owner, after the sweep.

---

## The venue

| Field | Value |
|---|---|
| Substrate | `PENDING` (nvmet-**tcp** devsub via `tests/dev_substrate.sh`, `resv_enable=1`; the fabric-sensitive venue, never loop) |
| Host | `PENDING` (kernel, CPU, RAM, `nvme_core` posture) |
| Binary | `PENDING` (`squeezefs --version`, both halves; a `--all-features`/`dhat-on` build is never valid for a number) |
| Fleet | `PENDING` (`tests/mw_fleet.sh create N=1 --owners=K`; K = `PENDING`) |
| Instrument (a) | real linux `fs/` tree via `SQZ_MWMATRIX_TAR_SRC` — `PENDING` entries, `PENDING` MB |
| Instrument (c) | `dd conv=fsync,notrunc` full-file overwrite, `PENDING` files × `PENDING` MiB |
| Instrument (d) | python3 `O_DIRECT` rand-4k `pwrite` loop, `PENDING` s over a `PENDING` MiB file |
| Wire shaping | netem 125 µs per veth end = 250 µs RTT on the partial authority's namespace |
| Ordering | A-B-B-A on every comparison over an aging store |
| Date / operator | `PENDING` |

**Tier declaration, per §5.13, fixed before any number lands:**

| Row | Tier |
|---|---|
| (a) `tar -x` gate, 1 set authority + 1 partial authority, netns, one box | **measured-real** (the same venue class as the 6.73× row it replaces) |
| (b) cross-owner refusal-rate table | **measured-real** |
| (c) rewrite/overwrite funnel at K = 2 | **measured-real** |
| (d) R14 rand-4k row | **measured-real** (each arm labeled with its posture) |
| any K ≥ 4 fan-out row on one box | **measured-simulated** (one box, one memory bus) |
| any K = 16 or 15 k statement | **arithmetic-on-measured-constants**, formula published — never measured here |

---

## 0. The setup precondition (part of the gate)

`tests/pv_locate_gate.sh <mount> <subtree-root> --owner <member-id>
--creates 10` runs before every timed row and asserts both halves §5.13
names: (a) `volume locate` reports a volume the extracting node **owns**,
and (b) the first ten creates under it land on a volume the **same** node
owns (M2's engagement).

| Node | Subtree root | `volume locate` volume | Owner | 10 creates same owner |
|---|---|---|---|---|
| set authority (member 0) | `PENDING` | `PENDING` | `PENDING` | `PENDING` |
| partial authority (member 20) | `PENDING` | `PENDING` | `PENDING` | `PENDING` |

Validated on `test/pv-acceptance` as **plumbing** (see
[Plumbing validation](#plumbing-validation-not-acceptance-evidence)): the
gate exits nonzero on a root-descended target, on the filesystem root, on
an unowned volume and on a child that lands on a peer's volume.

---

## (a) The `tar -x` gate — the row this program exists for

**Gate**: the partial authority's median ≤ **1.10×** the authority-LOCAL
S0 baseline at netem 250 µs. **Baseline to beat**: the S10 co-writer row's
**6.73×** (`.benchmarks/2026-08-18-mw-program-closing.md`).

| ARM | WALL_S | OPS_S | ship | pub | verbs/entry | redirects |
|---|---|---|---|---|---|---|
| `pl-on-1` (partial authority, own subtree, 250 µs) | `PENDING` | `PENDING` | `PENDING` | `PENDING` | `PENDING` | `PENDING` |
| `local-1` (set authority, LOCAL, S0) | `PENDING` | `PENDING` | — | — | — | — |
| `local-2` (set authority, LOCAL, S0) | `PENDING` | `PENDING` | — | — | — | — |
| `pl-on-2` (partial authority, own subtree, 250 µs) | `PENDING` | `PENDING` | `PENDING` | `PENDING` | `PENDING` | `PENDING` |

**Verdict**: `PENDING` × of S0 — `PENDING` (gate ≤ 1.10×). Versus the
6.73× it replaces: `PENDING`.

**The inversion** (§5.13's engagement column, and the reason the row is
meaningful rather than merely fast): the extracting node's wire verbs per
entry must go to ≈ 0 for its OWN subtree — the co-writer paid 14.5. The
leg **exits nonzero** if it exceeds 1.0, because a partial authority
extracting into its own subtree that still ships is not a slow row, it is
a broken setup.

**If the gate is missed**, that is a published outcome and not a failure of
the note: state the number, state which term dominates from the
`meta_ship_phase_ns` / `publish_phase_ns` medians in the row directory, and
carry the honest statement into `docs/operations.md` and
`docs/rc-manifest.md` exactly as the S10 row's charter alternative does.

---

## (b) The cross-owner refusal table (D18's obligation)

`meta_ship.cross_owner_refusals` is ONE scalar, so the per-verb split is
produced by the **venue**: each verb class runs in its own snapshot window,
and the window's delta belongs to exactly that verb.

| PHASE | VERB | OPS | REFUSALS | RATE | APP_ERRS | WALL_S |
|---|---|---|---|---|---|---|
| `xo-rename-own` (all-own placement) | rename | `PENDING` | `PENDING` | `PENDING` | `PENDING` | `PENDING` |
| `xo-rename-50` (50/50) | rename | `PENDING` | `PENDING` | `PENDING` | `PENDING` | `PENDING` |
| `xo-rename-all` (adversarial) | rename | `PENDING` | `PENDING` | `PENDING` | `PENDING` | `PENDING` |
| `xo-link-all` (adversarial) | link | `PENDING` | `PENDING` | `PENDING` | `PENDING` | `PENDING` |
| `xo-unlink-own` (all-own) | unlink | `PENDING` | `PENDING` | `PENDING` | `PENDING` | `PENDING` |

**User-visible cost** (§5.13's column): `mv` across the partition degrades
to copy + unlink — correct but slower — so `APP_ERRS` for rename is
expected to be 0 while `REFUSALS` is not, and the wall-clock delta between
`xo-rename-own` and `xo-rename-all` **is** that cost, measured:
`PENDING`. `link` has no fallback and the op fails: its error rate is
`PENDING`.

**The `rm -rf` arm** (the shape Issue 1 exposed): `rm -rf` of a subtree
whose root's own name spans two owners must descend cleanly and then refuse
the ROOT with `EXDEV`, **having destroyed nothing it could not finish** —
the §5.4a total-refusal law's live face.

| Check | Result |
|---|---|
| `rm -rf` of an own-subtree tree | `PENDING` |
| `rm -rf <subtree root>` refuses | `PENDING` |
| the root is still standing afterwards | `PENDING` |
| refusal text names the remedy | `PENDING` |

**The undeletable-in-place population**: `PENDING` cross-owner name(s) at
assignment (the verb's own M3 census, acknowledged verbatim by
`--accept-cross-owner-names`), `PENDING` still standing at the end of the
run (`squeezefs volume get-owners --census`, offline, after the fleet is
down). On a fresh set whose roots the verb mints, that population **is**
the roots — one name per root — which is the supported shape's stated cost.

---

## (c) The rewrite/overwrite funnel (§5.12's relocated wall)

§5.12 is explicit that the recipe removes the metadata-publish term for
self-owned work and does **not** remove the set-authority term: under D20
every partial authority's terminal frees SHIP, and a rewrite-heavy
workload's frees track its ingest. **There is no gate on this row.** The
number is the deliverable.

| ARM | WALL_S | MIB_S | free blocks shipped | served by the authority |
|---|---|---|---|---|
| `partial-1` | `PENDING` | `PENDING` | `PENDING` | `PENDING` |
| `setauth-1` | `PENDING` | `PENDING` | 0 (by construction) | — |
| `setauth-2` | `PENDING` | `PENDING` | 0 (by construction) | — |
| `partial-2` | `PENDING` | `PENDING` | `PENDING` | `PENDING` |

**Ratio**: `PENDING` × (partial vs set authority).
**Closure**: the partial's `meta_ship_publish.free_shipped_blocks` delta
must be accounted for by the authority's `free_served_blocks` delta, and
`free_ship_failures` must be 0 — each failure is a durably-free offset the
authority will not see again until its next derivation.

**The statement this row settles**: `PENDING` — either rewrite-heavy
fleets are inside the recipe's claimed scope at this width, or the program
states explicitly that they are not (§5.12's own alternative).

---

## (d) The R14 rand-4k row (the priced W1 loss)

On a K-node fleet, K−1 nodes are partial authorities and therefore lose the
W1 sole-owner extent patch: a lifetime incarnation retire is durable
ownership state, and the §5.1 clone/patch fence is a two-word
process-local protocol no wire composes. Their isolated small overwrites
ride CoW-rewrite plus a shipped free. **R14 is a named, accepted
regression** — this row publishes its size on this venue, and there is no
gate.

| ARM | IOPS | `patch_writes` | `patch_ineligible_*` (sum) | `cowriter.accounting_refusals` |
|---|---|---|---|---|
| partial authority | `PENDING` | 0 (asserted) | `PENDING` | `PENDING` |
| set authority | `PENDING` | `PENDING` (> 0, asserted) | `PENDING` | `PENDING` |
| single authority today (separate fleet) | `PENDING` | `PENDING` | `PENDING` | `PENDING` |

**Ratio**: `PENDING` × (partial vs set authority).

**Instrument warning, standing**: this row's IOPS come from a python3
`O_DIRECT` `pwrite` loop — the same instrument in every arm, which is what
the comparison needs, and **not** an IOPS-ceiling instrument. The
61–67 k figures the W1 program published came from elbencho/fio and these
numbers must never be spliced with them (the instrument-alignment law).

---

## (e) The oracle

Run after every measured sweep, on the set authority (D20: maintenance has
exactly one coordinator).

| Check | Result |
|---|---|
| `fsck` findings on the set | `PENDING` (must be 0) |
| `meta_kv_block_refs_drift`, per owner | `PENDING` (C8 must be 0) |
| `fsck_repair_refused_multi_owner` | `PENDING` |

---

## The engagement ledger

§5.13's law, verbatim. A row without **all** of these is INVALID whatever
its wall clock says; the rig asserts them and exits nonzero.

| Column | Required | Observed |
|---|---|---|
| placement ledger | closes to the op; `rotor_fallbacks == 0` | `PENDING` |
| the setup | `volume locate <target>` names a volume the extracting node OWNS | `PENDING` |
| the inversion | the extracting node's wire verbs/entry → ≈ 0 for its own subtree (today 14.5) | `PENDING` |
| `mint_redirects` | ≈ 0 per posture (the owned-candidate filter is armed) | `PENDING` |
| `migrations_triggered` / `migrations_failed` | 0 (KD-PV-13's disarmed posture) | `PENDING` |
| `peer_volume_local_commit_refusals` | 0 | `PENDING` |
| `cowriter.local_commit_refusals` | 0 | `PENDING` |
| `alloc_lane_raise_refusals` | 0 | `PENDING` |
| `owner_map_poisoned_volumes` | 0 | `PENDING` |
| `xv_cross_owner_intents` | 0 | `PENDING` |
| `meta_ship_publish.{refusals,owner_panics}` | 0 | `PENDING` |
| `free_grace_deferrals` on partial authorities | 0 (the one grace ring is the set authority's) | `PENDING` |
| `meta_kv_revalidate_dirty_skips` | 0 | `PENDING` |
| fsck oracle after the sweep | `fsck_findings == 0`, `meta_kv_block_refs_drift == 0` | `PENDING` |

---

## Plumbing validation (NOT acceptance evidence)

Run unprivileged on the authoring box against a **file-backed** two-volume
set — no fabric, no PR, no fleet. It proves the rig's plumbing and its
refusals, and it proves **nothing** about performance.

| What | How | Result |
|---|---|---|
| the mount path can select a per-volume posture | `SQUEEZEFS_MW_ROLE=partial-authority squeezefs mount …` | reaches the seven-rung ladder; refused at **rung 2** ("carries no claim set"), which is the ladder's own answer on an unassigned set. Before the fix in this branch it was refused by the mount path itself, so no substrate could have run this row |
| `volume set-owners` end to end | offline, two owners, two subtree roots, `--dry-run` census then `--accept-cross-owner-names 2` | assigned, both roots minted, D20 announcement printed, census parseable |
| the setup gate, positive | `pv_locate_gate.sh <uri> /owner-b --owner <B>` | rc 0 |
| the setup gate, **root-descended target** | `pv_locate_gate.sh <uri> /legacy --owner <B>` | **rc 1**, naming the owning peer and the 6.73×-by-construction reason |
| the setup gate, the filesystem root | `pv_locate_gate.sh <uri> / --owner <B>` | **rc 1** |
| the setup gate, half (b) live | `--creates 10` against a live mount | children located and compared per create; detects a child landing on a peer's volume |
| the setup gate, `--creates` on an offline URI | | **rc 1** (refused, never silently skipped) |
| the setup gate, unresolvable path / missing `--owner` | | **rc 1** each |
| the KD-MW-2 bare-node wildcard | `--owner node_<16hex>` against a slot-decorated record | matches, as `member_id_matches` does |
| `plan_owner_assignment` (the fleet's assignment planner) | sourced, canned rows: K=2/2 vols, K=2/4 vols, K=3/3 vols, and three refusals | all 9 checks pass — slot-0 volume first, round-robin, exactly one root per owner, member indices 0/20/21 |
| the planner against the real product verb | `meta_volume_rows \| plan_owner_assignment 2 …` on the file-backed set | correct specs from real `volume get-owners --json` output |
| `mw_fleet.sh --owners` refusals | K=1, K=17, with `--cowriters`, N>20, non-numeric, missing value | rc 1 each, every refusal naming its cause |
| matrix argument refusals | `--xo-ops=5`, `--rewrite-mb=1`, `--w1-secs=2`, `--w1-mb=8` | rc 1 each |
| `bash -n` on all three scripts | | clean |

---

## How to run this row

Written for someone who knows the cloud rig but not this branch's rig
changes. **Every step is on ONE node.** The fleet is single-box by design:
K daemons over one nvmet-tcp devsub, which is the venue class §5.13
requires (the same one that produced the 6.73× baseline). The cloud's role
is only to supply a **root box with a PR-capable kernel**.

### Preconditions

* A Linux box with **root**, kernel ≥ 6.14 (client `fuse.enable_uring`)
  and ≥ 6.13 nvmet (`resv_enable` — the S9 arm refuses non-PR), `nvme-cli`,
  `python3`, and the `null_blk` / `zram` / `nvmet-tcp` modules available.
* This branch's tree on that box, and a **release** build of `squeezefs`
  (`cargo build --release`, default features — never `--all-features`).
* A real linux `fs/` tree for the (a) instrument.

### If that box is the AWS `mw` preset

`PRESET=mw` gives an Ubuntu 26.04 client with both kernel floors and the
packages. Use only its launch/deploy/teardown steps — **not**
`assemble-mw`, which builds the co-writer fleet shape over a **single**
metadata volume (`N_MDS=1`) and therefore cannot host a multi-owner
assignment at all (see [Design and rig corrections](#design-and-rig-corrections)).

```bash
# operator box
MAX_CLUSTER_HOURS=2 PRESET=mw tests/cloud_bench_cluster.sh launch
PRESET=mw tests/cloud_bench_cluster.sh deploy          # needs dist/ubuntu2604
PRESET=mw tests/cloud_bench_cluster.sh status          # note client0's public IP

# push what this rig needs (the cloud driver only pushes run_mw_matrix.sh)
scp -i "$SSH_KEY_FILE" tests/{mw_fleet.sh,run_mw_matrix.sh,pv_locate_gate.sh,dev_substrate.sh} \
    ubuntu@<client0>:/opt/squeezefs-bench/repo/tests/
scp -i "$SSH_KEY_FILE" dist/ubuntu2604/squeezefs ubuntu@<client0>:/opt/squeezefs-bench/squeezefs
ssh -i "$SSH_KEY_FILE" ubuntu@<client0> \
    'sudo apt-get -qq install -y linux-modules-extra-$(uname -r) || true; \
     sudo modprobe null_blk && sudo modprobe zram && sudo modprobe nvmet-tcp && echo modules ok'
# the (a) instrument
ssh -i "$SSH_KEY_FILE" ubuntu@<client0> \
    'cd /opt/squeezefs-bench && curl -sL https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-6.14.tar.xz \
     | tar -xJ linux-6.14/fs && echo tree ok'
```

Everything below runs **on client0, as root**, with
`SQZ_BIN=/opt/squeezefs-bench/squeezefs` and
`cd /opt/squeezefs-bench/repo` (the rig derives `REPO` from its own
`dirname/..`, so the scripts must sit in `<repo>/tests/`).

### Step 1 — stand the fleet up (~3 min)

```bash
export SQZ_BIN=/opt/squeezefs-bench/squeezefs
sudo -E tests/mw_fleet.sh create N=1 --owners=2
sudo -E tests/mw_fleet.sh owners        # the assignment, ids, roots, MW ports
```

`create` refuses loudly at the first thing that is not right. It ends by
asserting operations.md's verification list on every owner:
`mount_posture` ∈ {`set-authority`, `partial-authority`},
`meta_ship.armed == true`, `meta_ship.not_owner_refusals == 0`,
`meta_ship.owner_panics == 0`, ONE `alloc_lane_writers` width fleet-wide,
`alloc_lane_raise_refusals == 0`. Record the K, the ids and the roots into
[The venue](#the-venue) and section [0](#0-the-setup-precondition-part-of-the-gate).

### Step 2 — gate (a), the `tar -x` row (~25 min)

```bash
sudo -E SQZ_MWMATRIX_TAR_SRC=/opt/squeezefs-bench/linux-6.14/fs \
    tests/run_mw_matrix.sh s10-placement-tarx --partial-authority
```

Quiet-gated (it refuses a box with foreign cargo/fio work or loadavg > 4).
It asserts the setup precondition and the inversion before it publishes,
runs A-B-B-A, prints the verdict table and runs the oracle.

**Harvest into [(a)](#a-the-tar--x-gate--the-row-this-program-exists-for)**:
the printed table (`WALL_S`, `OPS_S`, `ship`, `pub`, `verbs/entry`,
`redirects`) and `s10pl-verdict.txt`. Row artifacts land in
`/run/squeezefs-mwfleet/rows/s10pl-<ts>/` — copy the whole directory home;
it carries the per-arm stats snapshots the engagement ledger is read from.

### Step 3 — gate (c), the rewrite funnel (~10 min)

```bash
sudo -E tests/run_mw_matrix.sh pv-rewrite-funnel --rewrite-files=8 --rewrite-mb=64
```

**Harvest into [(c)](#c-the-rewriteoverwrite-funnel-512s-relocated-wall)**:
the table's `WALL_S` / `MIB_S` / `shipped=` / `served=` per arm and
`pv-rewrite-verdict.txt`. The partial arm's `shipped` must be > 0 (the
funnel engaged) and the authority's `served` must account for it.

### Step 4 — gate (b), the cross-owner table (~5 min)

```bash
sudo -E tests/run_mw_matrix.sh pv-cross-owner --xo-ops=200
```

**Harvest into [(b)](#b-the-cross-owner-refusal-table-d18s-obligation)**:
the per-verb table, `pv-xo-rmrf.txt`, and the census the fleet recorded at
assignment (`OWNER_CENSUS_AT_ASSIGNMENT` in
`/run/squeezefs-mwfleet/config.env`).

### Step 5 — gate (d), the R14 rand-4k row (~5 min)

```bash
sudo -E tests/run_mw_matrix.sh pv-rand4k-w1 --w1-secs=30 --w1-mb=256
```

**Harvest into [(d)](#d-the-r14-rand-4k-row-the-priced-w1-loss)**: the two
armed rows plus `pv-rand4k-verdict.txt`. The leg asserts the postures
themselves — the partial authority must perform **zero** W1 patch writes
and the set authority must perform some.

### Step 6 — the third R14 arm, on a single-authority fleet (~8 min)

```bash
sudo -E tests/mw_fleet.sh teardown                       # zero-residue asserted
sudo -E tests/mw_fleet.sh create N=1 --multi-writer
sudo -E tests/run_mw_matrix.sh pv-rand4k-w1 --w1-secs=30 --w1-mb=256
```

The leg detects the unassigned fleet and emits the
`single-authority-today` arm — the third row of §5.1.3's table. A fleet
cannot be both shapes at once, which is why this is a second incarnation
and not a third arm of step 5.

### Step 7 — the end-of-run census, then down

```bash
sudo -E tests/mw_fleet.sh teardown
# on the assigned set only (step 6 tore it down — re-create + re-assign if
# the closing count is wanted, or take it before step 6 instead):
sudo -E $SQZ_BIN volume get-owners "sqmeta://<meta-uri>" --census
```

Then, on the operator box:

```bash
tests/cloud_bench_cluster.sh teardown    # idempotent; fails loud if anything still bills
```

### Wall-clock and cost

| Step | Wall |
|---|---|
| launch + deploy + push + tree fetch | ~15 min |
| 1 fleet up | ~3 min |
| 2 `tar -x` gate (A-B-B-A over 2,384 entries × 4 arms) | ~25 min |
| 3 rewrite funnel | ~10 min |
| 4 cross-owner | ~5 min |
| 5 rand-4k (multi-owner) | ~5 min |
| 6 rand-4k (single-authority) | ~8 min |
| 7 teardown | ~5 min |
| **total** | **~75 min** ⇒ under 2 cluster-hours |

### If a leg exits nonzero

It is telling you the row would be meaningless, not that the product is
slow. Read the refusal: it names which gate failed (setup, inversion,
engagement, oracle) and what a valid setup looks like. The row directory
under `/run/squeezefs-mwfleet/rows/` survives; copy it home before
tearing down — **evidence before verdict**.

---

## Design and rig corrections

Filled in as the run proceeds. Landed with this branch:

1. **`SQUEEZEFS_MW_ROLE=partial-authority` and `=set-authority` were not
   selectable by the mount path** (fixed on this branch, red-first). PR 3
   left a fail-closed refusal in `cowriter::co_writer_requested()` —
   correct while the partial open was unbuilt — and `src/main.rs` reads
   that function *before* `partial_authority::requested()`, so PRs 4, 5 and
   7b built the open, the derivation and both arms behind a door that was
   still bolted. Every PR 7b contract called `arm_partial_authority`
   directly, so the arm's own suite stepped over the gate an operator hits
   first, and the design's PR 7b row, the knob registry text and
   operations.md's bring-up recipe all stated the posture was reachable end
   to end. **PR 8's fleet could not have been stood up on any substrate.**
   Pinned by `pv_partial_arm_tests::a_declared_per_volume_posture_is_selectable_by_the_mount_path`.
2. **`§5.13`'s cross-owner refusal table asks for a per-verb split the
   counter does not have.** `meta_ship.cross_owner_refusals` is a single
   scalar (`src/meta_ship/mod.rs`), incremented from four sites. The rig
   produces the split from the **venue** — one snapshot window per verb
   class — rather than from a product change, which keeps PR 8 to its
   named files. If the split is wanted as a product surface, that is a
   follow-on, not an acceptance-rung edit.
3. **The AWS `PRESET=mw` cloud shape cannot host a multi-owner set.** It
   provisions `N_MDS=1` and its `mds` node shares only its first
   instance-store device, so the set has exactly ONE metadata volume —
   and a multi-owner assignment needs at least one volume per owner.
   `assemble-mw` is therefore not the path for this rung; the recipe above
   uses the cloud only for a root box and builds the fleet with
   `mw_fleet.sh`'s own devsub. Raising `N_MDS` would also need
   `assemble-mw` to stop hard-coding the co-writer mount shape.
4. **An undeclared write mount of an ASSIGNED set is admitted and does
   not honour M2** (observed, unprivileged, file-backed). With the whole
   fleet down, a plain `squeezefs mount` of an assigned set takes every
   volume's claim and mints children of a peer's subtree root onto any
   volume by the ordinary rotor, creating fresh cross-owner names that then
   cannot be unlinked in place. In a live fleet the D0 guard is the
   protection (the peers hold claims and the mount refuses `FreshForeign`),
   so this is an operator hazard in the same class as
   operations.md's "an older binary does not refuse an assigned set", not a
   plane that failed. Worth a sentence in that section.

---

## Closure (PR 8's own wording, rev 2 / Issue 19c)

`PENDING` until the rows land. When they do, this rung closes **residual
item 3's ADMISSION half**, and the same act re-files the remainder as a
named follow-on on the residual board of
`docs/design-full-multi-writer.md`:

* cross-owner slot migration (D19 defers it — the item's own text ends
  *"Cross-owner slot migration … rides with it"*),
* the offline subtree re-homing pass (open question 3),
* the R15 purge scoping (PR 0 took the half it could; the purge half needs
  PR 4's peer-volume revalidation armed under load),
* R14's W1 recovery on partial authorities.
