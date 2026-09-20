# Symmetric shared-disk metadata — PR 13, the acceptance rung (gates 1–8b)

**Date:** 2026-09-19. **Branch:** `perf/sym-acceptance` off `dev` @ `8ead0244`
(every rung landed: PR 1/2/3/4/11/16, level 4 (5/6/7/8), level 5 (7b/9/10 +
the flip fix), PR 12, PR 12b). **Design:** `docs/design-symmetric-metadata.md`
§8 (the gate table — the contract), §1.6 (the measured constants), §5.10 (the
D1 arithmetic), §7.3/§7.4 (the flip decision's inputs), PR-plan row 13.
**Status: IN PROGRESS** — this record is written incrementally as gates
complete; every row states its venue class (the 2026-09-14 venue law: the
dev box is SCOPING evidence, `squeeze-test` is the acceptance venue for the
brackets the design marks measured-real/box).

## 0. The verdict in one paragraph (updated last)

_(pending — see §9, the flip decision.)_

## 1. Venue block

| Venue | What ran here | Host / kernel / fabric / substrate |
|---|---|---|
| **Dev box** (SCOPING; every local leg, every from-zero count, SIM-1, the fidelity tier) | `tests/run_mw_matrix.sh` `sym-*` legs on `tests/mw_fleet.sh` (`create N=2 --symmetric --writers=7 --token-readers --lease-ttl-ms=15000`, `SQZ_MWFLEET_OSS_GB=16`), `cargo test --release` pins, `membership_sim::run_sharded` | 32 CPUs (throttles to 3.1–3.7 GHz at 95 °C under sustained load — wall-clock rows are scoping only), kernel `7.2.3-cachyos-lto`, **nvmet-tcp on `127.0.0.1` (`resv_enable=1`)** — the fleet's own tcp devsub instance `mwfleet` (memory-backed null_blk metadata namespaces ×2, zram data namespaces ×2 × 16 GiB); the in-process pins on file-backed sandboxes |
| **squeeze-test** (ACCEPTANCE brackets) | gate 1's A-B-B-A (PR 1's rig verbatim, `.benchmarks/rigs/2026-09-13-sym-pr1-solo-regate.sh`) | `memp-s3ds-aqs-37`, 32 cores, the reset-v5 converged fabric (`/scratch/tmp/cluster_reset_v4.sh`: 5 nodes × (1 meta null_blk + 2 data null_blk) over nvme-tcp), mountpoint `/scratch/tmp/test`; the box has no toolchain — arms built on the laptop with `task build:rocky8` (the `release` profile, both arms — the two-profile law), checksummed, copied to `/scratch/tmp/sym-pr13/` |

**Binaries.** Arm A (gate 1) = `3228fcb8` (the pre-program dev tip PR 1's
bracket used — comparability), rocky8 `release` profile, built in a throwaway
worktree (`/tmp/pr13-armA`, `task build:rocky8`, artifact checks passed).
Arm B = this branch's tip (the SHA and sha256 are in §8 once the box row ran).
Every local leg ran `target/release/squeezefs` of the commit named in its row.

**Instruments.** `tests/run_mw_matrix.sh` (the `sym-*` legs — every leg prints
its engagement gauges and exits nonzero when a law is violated; a row without
its engagement is INVALID, not a number), `tests/run_mdstorm.sh` (the metadata
storm — `SYM_STORM`), `dd conv=fsync` (the acked-writes oracle and the ingest
rows), `tar -x` (gate 2), `getfattr`/`setfattr` (the stripe flip), the `.stats`
inode (every gauge named below), `squeezefs fsck` (the C1–C17 census after every
kill), `squeezefs appenders`, SIM-1 = `membership_sim::run_sharded` (release).

## 2. Gate table

_(MET / MISS per row with its engagement gauges; rows fill in as they run.
"dev" = scoping venue; "box" = squeeze-test.)_

| Gate | Row | Venue | Verdict | Engagement (the law's gauges) |
|---|---|---|---|---|
| 1 | solo re-gate (flat A vs flat B: mdstorm, mount, w_fresh, rr4k, rw4k, remount) | box | _pending_ | `dlm_rpcs == 0`, `meta_kv_forest_*` 0 on flat, Δtripwires 0 |
| 2 | `sym-tarx` (N = 2, netem 250 µs, the extracting node NOT the manager) | dev → box | _pending_ | `wire_verbs_per_entry` < 0.05, `slot_handovers == 0` |
| 3 | `sym-scale` N = 1/2/4/8 | dev → box | **N ≤ 4 MET, N = 8 MISS (defect 5)** — see §4 | `appenders == N`, `manager_load_pct`, handovers/ships/rpcs 0 |
| 3b | `sym-shared-dir` (+ `-ls`) | dev → box | _pending_ | `dir_stripe_flips == 1`, `dir_stripe_ships ≡ foreign creates`, `slot_handovers == 0`; ls: `dlm_token_grants ≡ K + C` |
| 3c | `sym-foreign-touch` | dev → box | _pending_ | handovers/s, `slot_handover_phase_ns`, a paused live job keeps its tree |
| 4 | `sym-crash` / `sym-storm` (a)–(f) ×10 from zero | dev (LOCAL by the venue law) | _pending_ | must-stay-0 set; `appender_recoveries ≡ regions of the killed nodes`; acked-loss 0; `fsck_findings == 0`; C8/bitmap drift 0; `replay_dropped_torn == 0` |
| 5 | `sym-readers` (exactness; 1 × 31 broadcast; `free_grace_hold_ms`) | dev → box | _pending_ | `dlm_recall_fanout ≡ readers`, `reader_staleness_bound_ms == 0`, tokens held on `-o ro` |
| 6 | format cost at N = 8 / 32 (+ the 46-volume width row) | dev (LOCAL) | _pending_ | per-slot extent floor, `slot_tree_bytes` p99 vs `A_max`, ring space, page writes |
| 7 | relocated walls (terminal-free rate per holder under `w_rewrite` N = 8; the manager verb rate under a 32-mount join storm) | dev → box | _pending_ | `block_free_*`, `manager_verbs_per_s`, `manager_load_pct` |
| 8 | SIM-1 `SimConfig { clients: 12_500, shards: 64 }` | dev (tier (ii)) | **MET** — §5 | beat p99, eviction fan-out, the free-grace V-fan-in, the death ledger's reach |
| 8b | fidelity `full` (nvmet; `pr-registrants` ≥ 1,024 + the emulated cap refusal; `sym-join-ladder` N = 3) | dev (LOCAL) | _pending_ | the tier's own verdicts |

## 3. The local legs — from-zero counts

_(filled per leg: rounds run, greens, the counted-restart events.)_

### 3.1 `sym-scale` (gate 3) — dev box, SCOPING

Four full-ladder runs on the fleet (7 joiners + the manager, one token
reader). Per row: N RW mounts each creating 30,000 files (4 threads) in its
OWN directory (`tests/run_mdstorm.sh` `create`), then ingesting 512 MiB each
(`dd bs=4M conv=fsync`); exactly N appenders live per row
(`sym_ensure_joiners`); every mount's `.stats` snapped before/after.

| run (binary) | N=1 | N=2 | N=4 | N=8 | note |
|---|---|---|---|---|---|
| r2 (`c2c5e663`) | 7,227 c/s · 2,065 MiB/s | 19,162 (2.65×) · 4,769 (2.31×) | 27,548 (3.81×) · 7,433 (3.60×) | storm completed (8 × ~5,100 c/s = 40,700 = 5.6×) | must-stay-0 tripped at N=8: `appender_flush_ceiling_overruns=2` on the manager (§4.3) |
| r4 (`c2c5e663`) | 6,661 · 1,015 | 18,626 (2.80×) · 2,199 (2.17×) | 27,314 (4.10×) · 4,855 (4.78×) | a joiner FAIL-STOPPED at its 20,250th create (§4.2, defect 5) | N=4 also read `appender_flush_ceiling_overruns=+2` |

| **r5 (`2a94abbc` + the return belt = `d00db50b`'s tree; defect 5 a+b landed)** | 6,874 · 2,481 | 19,192 (2.79×) · 5,679 (2.29×) | 27,909 (4.06×) · 8,015 (3.23×) | **39,778 (5.79×) · 10,309 (4.16×)** — the row COMPLETES: no fail-stop, no contamination, must-stay-0 set flat | create rate MET at every N (5.79× ≥ 5.6×); ingest at N = 8 MISS on the RATE law (4.16× < 5.6×) — 10.3 GB/s into zram over nvmet-tcp on `127.0.0.1` is the single 32-CPU box's data path (8 daemons × 4 MiB `dd conv=fsync` + their FUSE queues on the same cores), the box row decides; **deleted-stays-deleted 0 / 3,000** sampled removed names after every joiner's clean unmount, judged at the manager AND at a remounted joiner |

| **r7 (`8d7fd3c0` — the FINAL binary, from zero, `pr13-batch7`; defects 20–26 landed)** | 7,023 · 2,991 | 19,018 (2.71×) · 6,212 (2.08×) | 30,071 (4.28×) · 6,300 (2.11×) | **20,517 (2.92×) · 5,023 (1.68×)** — the row COMPLETES (defect 24 gone: `/` auto-striped under the seven `mkdir`s and every joiner's `stat /` folded through the divert), must-stay-0 flat, `appender_flush_ceiling_overruns` 0, `manager_load_pct` 1 %, 453 stripe tokens served at the manager once | create rate MET at N ≤ 2, MISS at N = 4 (ingest) and N = 8 (both — every writer at 2,600–3,100 c/s uniformly, the manager included, against 7,600–8,400 at N = 4: the throttling box after four hours of fleets (86 °C idle), not a serialization — no product change touches the create path between r5 and r7; the box row decides); deleted-stays-deleted 0 / 3,000 at the manager AND at the remounted joiner |

`slot_handovers == 0`, `slot_ships == 0`, Σ `dlm_rpcs == 0`,
`manager_load_pct` 0–2 % on every row (the manager's verbs cost nothing
measurable at N ≤ 8 — its CPU is its OWN storm's). The 0.7 × N law is MET
at N = 2 and 4 on r2/r4/r5; N = 8 is the defect-5 row on r4 and MET on
r5; r7 (the final binary) MISSES it on the throttled box — a dev-box
RATE reading, the mechanism rows (completion, the tripwires, the deleted
law) GREEN.

## 4. Issues found (each with its PR and its red pin)

### 4.1 Fixed on this branch

1. **PR 12 — the `-o ro` reader's per-holder plane served before its recall
   channel's first round** (`e0133a07`). `EIO "the recall channel to the
   holder is not fresh"` on the FIRST resolve of a joiner's object. Found by
   the first `--token-readers` fleet. Pin: `sym_mount_posture_tests::
   a_readers_first_resolve_of_a_freshly_dialed_holder_serves_without_a_hand_wait`.
2. **PR 12 — the reader-side `dlm_token_*` stats face folded the manager's
   plane only** (`502aa859`); gate 5's engagement law was unreadable. Folds
   every plane now.
3. **PR 12b — the granted-extents cache barrier (`drop_nodes_in_extents`)
   refused a grant over a DIRTY projection node** (`502aa859`): a joiner that
   opens over a non-empty ring-0 window folds the manager's records into its
   projection, which reads dirty; the manager compacting such a leaf,
   retiring and re-granting the extent is the ordinary lifecycle → `EINVAL`
   on the joiner's create (the fleet: a rejoined joiner's 27th create). Refuses
   only a dirty node of a tree this mount WRITES. Pin:
   `sym_n_daemon_tests::a_joiners_dirty_projection_of_a_retired_manager_leaf_never_refuses_its_grant`.
4. **PR 12b (P0) — a JOINED appender's threshold maintenance ran over its
   PROJECTIONS** (`c2c5e663`). The checkpoint task's `maintenance_pass`, its
   wake check and `tick` step 1 walked `all_trees()`, so a joiner compacted
   its projection of the manager's tree 0 (and other appenders' slot trees)
   into successor images claimed off its stale projected bitmap — extents the
   manager had since granted elsewhere. The fleet's N = 8 row: finding 41's
   bounds refusal, the tail pinned, D1.b fail-stop. In process: the
   four-writer storm pin reproduced it in 0.2 s and its ATTRIBUTION output
   named tree 0 of every daemon at the corrupt address. Fix:
   `maintains_slot(None)` = `is_manager()` under an armed plane; the three
   maintenance loops walk `maintainable_trees()`. Two tripwires beside it
   (`extent_grant_conflicts` must-stay-0; `extent_grant_stale_page_words`) and
   the threshold pass's §5.3.3 reactive refill (`maintenance_grant_refill` —
   before it every threshold wake on a drained grant failed at WARN per entry,
   66/s in the pin). Pin: `sym_n_daemon_tests::
   concurrent_storms_on_four_writers_never_cross_a_record_into_another_appenders_leaf`.

### 4.2 Defect 5 — FIXED (both halves, PR 12b, P0, the flip-blocking class): two appenders' frames under ONE `node_seq`

**The evidence** (fleet r4, N = 8, joiner m62 = appender 3, meta volume 0 =
`/dev/nvme1n1`, extent `0x5a80000`, read raw off the device after the row):

```
header  node_seq=4057602432666822354 level=0 min='' max=0c35:ec (slot 0xc35's leftmost leaf)
frame@4096    node_seq_at_write=…354 SAME padded=241664 bset=237806 appender=3 g=1   ← m62's base bset
frame@245760  node_seq_at_write=…354 SAME padded=8192   bset=4160   appender=4 g=1   ← m63 APPENDED
frame@253952  node_seq_at_write=…354 SAME padded=4096   bset=848    appender=4 g=1
frame@258048  node_seq_at_write=…354 SAME padded=4096   bset=3440   appender=4 g=1
```

The instrumented refusal (`179ad23e` — the finding-41 message now names the
tree, the node's stamp and each offender's slot + provenance) read: `tree slot
Some(3125) … source node 0x5a80000 stamped Some(3125) level 0 … records
outside the bounds: [slot 3203 …, disk]` — i.e. appender 4's dentries of ITS
slot 0xc83 (`squeezefs appenders`: 3202/3203 is appender 4's rotor) folded
from the DEVICE into appender 3's slot-0xc35 leaf. `extent_grant_conflicts` /
`extent_grant_stale_page_words` 0 on the manager — the double custody is not
a double grant RECORD.

**Two halves, one class.**

(a) *The design half — one node-seq space per VOLUME* (FIXED on this branch,
red-first, `2a94abbc`). Every appender seeded its node-seq handle from the
same ledger word at its open (`backend.rs`, `ledger.seq.max(ledger.
node_seq_watermark)`), so under N daemons every seq guard the CoW law rests
on — the §4.2 child-pointer and root-pointer checks, the §4.5
frame-incarnation check that ends a recycled extent's log at a previous
node's frames, PR 11's residue-seq ceiling — was void ACROSS appenders:
lockstep storms mint the same `node_seq` in every daemon (the pin below: two
joiners' first mints carried ONE seq on `8af38eda`). **The law now**
(`kv::node_seq`, design §5.3.2 amended): the volume's uuid base `B` starts
incarnation 0 — the manager's and every flat / unarmed mount's legacy space,
seeded and raised exactly as before (`NodeSeqHandle::shared` IS the old
`AtomicU64`; a forest manager's is bounded by `B + 2^K`); every `JoinAppender`
(a rejoin included) is minted a fresh ordinal `o ≥ 1` from the durable
tree-0 counter `node_seq_incarnations` (one control entry, barriered before
the reply) and the joiner mints in the disjoint `[B + o·2^K, B + (o+1)·2^K)`,
its handle never raised. `K = 38` derived from the 63 usable bits (2^38
mints per incarnation = 200 SMOs/s for 43 years; 2^25 incarnations = 15 k
mounts re-joining daily for six years; either exhausted refuses loud, never
wraps — tie test `node_seq_incarnation_space_partitions_the_63_usable_bits`).
Every order comparison classified: the pointer checks / frame walk / frame
screen compare equality (unaffected), PR 10's root choice is by generation
(unaffected), `install_recovered_root`'s "older → re-read" became a mismatch
test, every raise goes through `raise_to` (a joined handle ignores it, the
shared handle confines it to its own space — PR 10's raise to a dead
joiner's root / residue stamps is the disjointness now). Pin:
`sym_n_daemon_tests::two_joined_appenders_never_mint_an_equal_node_seq`
(manager in span 0, joiner 1 in span 1, joiner 2 in span 2, a rejoin in span
3 — never back in its dead space). The wire's `Joined` reply carries the base;
the page layout is unchanged (a root installs by pointer + seq equality).

(b) *The grant half — a refill run that partially overlaps HELD extents
re-unclaimed them* (FIXED on this branch, red-first). The manager's §5.3.5
answer is the caller's page word verbatim (or its coalesced carve), and the
page word is the remainder at the caller's last page WRITE; `wire_extent_
refill`'s `fresh` filter kept a run unless EVERY extent of it was held, and
`RegionGrant::add_runs` inserted every extent of a kept run into `unclaimed`
— a live image claimable again by the next mint, or trimmed into the return
batch and RETURNED while its node stood. `add_runs` now skips claimed /
pending / returnable extents. Pin: `sym_manager_tests::
a_grant_run_overlapping_held_extents_adds_only_the_extents_the_grant_does_not_hold`.

**The fleet's verdict** (§3.1 r5): with (a) + (b) landed the N = 8 row runs
to completion — 2/2 red before, 1/1 green after, on the same fleet shape
and the same instrument. The two halves' contributions are NOT separated
(a (b)-only fleet row was not run — the counted-run law forbade spending
another red-first row on it, and the in-process storm never reproduced the
fleet's contamination on either tree): the on-disk evidence — two
appenders' frames under one `node_seq` in one extent — is (a)'s class by
construction, and (b)'s partially-held-run re-unclaiming is pinned by its
unit contract. With distinct seq spaces a second custodian's frames are
now `StaleIncarnation` at the frame walk (seen as harmless residue in the
defect-6 dumps) instead of folded.

### 4.3 Defect 6 — FIXED (KV core, PR K6/§4.6 — an acked-loss class every layout can reach; found at N = 8): a checkpoint flush that appended a PARKED frozen delta took the dirty floor of the records applied since

**The symptom.** The eight-writer in-process storm pin
(`sym_n_daemon_tests::concurrent_storms_on_eight_writers_at_the_fleets_
depth`, `--ignored`, ~25 s) with the fleet's row boundary — every writer
UNLINKS its previous round's 24,000 files before the next round — read,
after a joiner's CLEAN LEAVE, 21–280 of the 192,000 unlinked names
resolving at the manager at `nlink 0`, always the LAST unlinks of one
leaving daemon's directory; N = 1 and N = 2 clean, N = 8 red 5/5 on the
tree carrying defects 7–14 (the durable state lacked the tombstones: a
fresh writer resolved the same names — the pin's DUAL verdict, added for
exactly this attribution). The fleet's `sym-scale` row never showed it.

**The attribution** (three instruments, each added to the pin this round):
(1) the per-frame CENSUS of the leaf the released tree routes the name to
— its base a compaction output (e.g. 59 puts / 383 dels), ONE appended
frame of 8 dels, and the remaining tombstones in NO frame; (2)
`LEAVE-DIFF` — the leaving daemon's own tree, read BEFORE its leave,
routes the name to the SAME leaf (same address, same `node_seq`) and its
RAM fold says `Tombstone`; the manager's post-leave read of that leaf says
`Live` — the tombstone sat in the leaf's OPEN overlay and the leave
released the tree without appending it; (3) the Heisenbug that named the
step: a 192,000-name walk before the leave delayed it by seconds, the
daemon's own cadence flushed the leaf first, and the pin went green.

**The mechanism** (`KvTree::checkpoint_flush_node`, the checkpoint's
per-node step — flat code, every layout): `freeze_locked` answers a
PRE-EXISTING frozen delta when one is parked — an SMO froze the node for
its fold (`freeze_for_smo`, a REAL freeze-swap that leaves the dirty
floor intact) and then FAILED before its swap: a merge or compaction
refused an extent (`GrantExhausted` — the joined appender's common case
under the N = 8 grant storm: 100–150 wire refills refused per joiner per
run, `merge_after_flush`'s `claim_internal`). Commits keep applying into
the OPEN delta (`mark_dirty` admits an apply while FREEZING; only
SUPERSEDED refuses). The next flush step got the parked delta back,
`take_dirty_floor()` cleared the WHOLE floor, and `append_frozen` wrote
the parked delta alone — the newer records stayed in the overlay with
`dirty_floor == MAX`: no later dirty walk saw them, nothing clamped the
tail, `flush_slot_clear_of_region`'s `tail ≥ frontier` held, the release
recorded the leaf's tail at the parked frame's end, and the tree moved
without them (the joiner's RING — their only durable home — released
with the region). Why flat volumes never showed it: an SMO fails mid-way
there only on `NoSpace` / `JournalReserveExhausted` (rare); a joiner's
`GrantExhausted` is routine. Why N = 1/2 never showed it: no grant
pressure.

**The fix**: in the same lock window, when the freeze returned a delta
and the open overlay still holds records, the floor of THOSE records is
restored (`NodeDirty::overlay_floor` — `min(entry_floor, seq)` over the
open delta, the exact per-record stamp `apply_locked` set) so the node
stays dirty for them and the next pass appends them; engagement
`meta_kv_flush_floor_kept` (0 on a solo mount whose SMOs never fail
mid-way). Pins: `kv_freeze_wedge_tests::a_flush_of_a_parked_frozen_delta_
keeps_the_floor_of_the_records_applied_since` (the exact shape by hand on
the flat harness: a parked freeze, newer deletes, the flush step —
red-first: the node read CLEAN with the deletes only in RAM; green: dirty
until the second pass, every record on the device) and the eight-writer
storm pin (`--ignored`, the fleet's depth; §3 lists its from-zero count on
the fix). The two instruments — `LEAVE-DIFF` and the per-frame census —
stay in the pin's attribution.

### 4.4 Defect 7 — FIXED (PR 12b): a joined holder's lease projection is loaded once and never refreshed on its own — every joiner→joiner cross-owner create into a slot a LATER joiner minted was refused

Found by `sym-shared-dir` on a fresh fleet: m61's very first create into
m60's directory `EINVAL`, m60's log `cross-owner step … insert names child
ino …, which has no inode record and lives in a slot no appender leases — a
dentry nobody could have minted a target for is refused
(xv_cross_owner_steps_rejected)`, the intent left open and re-refused by
the roll-forward cadence every second. `sym-foreign-touch` read the same
class as "only 92 shipped steps for 192 foreign creates". The served
insert's Issue-8a screen (`screen_insert_child`) judges the child's slot
by the SERVING mount's lease table — on a joiner a PROJECTION of tree 0,
loaded at its open and advanced only by an event (a re-dial, a divert
failure, a `NotHolder` redirect, the join arm) — so every slot a LATER
joiner acquired read `Unleased` at every earlier joiner for as long as no
event fired; the second clause (the child's record on its volume) reads
through the same stale table (an "unleased" slot is read at the manager,
which has no such record). Fix: `KvMetaBackend::resolve_slot_holder_fresh`
— the table's answer; on a joined appender that reads `Unleased` the
projection is refreshed ONCE (`refresh_control_projection`), and when the
refreshed table STILL says `Unleased` — the ledger lags a grant by up to
one checkpoint (a grant is a ring-0 control entry; tree 0's moved root
reaches the ledger at the manager's next cycle, so a re-read of the ledger
right after the grant names the OLD root; the pin's first run read
`Unleased { g: 0 }` after the refresh) — ONE wire `ResolveSlot` asks the
manager's table, the lease's own word (`slot_resolve_rpcs` at the
manager); the screen reads it, the manager's table is never refreshed. Pin
(red-first): `sym_n_daemon_tests::a_joined_holder_resolves_a_later_
joiners_slot_at_a_served_step` (the raw table's `Unleased` as the premise,
the fresh resolve's `Holder { 2 }`, joiner 2's create into joiner 1's
seeded directory SERVED at joiner 1's own listener — the dentry lands in
joiner 1's tree, the record at its creator, `steps_rejected` unmoved).
PR 12b's sym-storm `--cross-owner` never saw it because its mover renames
into a directory the MANAGER holds (whose table is authoritative); this is
the first joiner→joiner cross-owner row.

### 4.4a Defect 8 — FIXED (PR 7b under N daemons): a foreign create into a STRIPED directory judged the stripe's record by the initiator's projection and refused `ENOENT`

Found by `sym-shared-dir` on the fixed defect-7 binary: every foreign
writer created 1–3 files into m60's directory, m60 flipped it to 64
stripes ("1 supplied by creators [0, 6, 7], 63 minted by the holder" —
appenders 6 and 7 DECLINED "no endpoint bound on this mount", a
`SupplyStripeIno` path that does not run `bind_holder_endpoint_on_demand`;
noted, benign: the holder mints the remainder), and every later foreign
create answered `ENOENT` "directory … was removed (no record)" with
`dir_stripe_dying_refusals` +1 at each initiator (m0, m61, m62 — the
manager too, whose RAM tree of m60's slot is a stale image like any
non-holder's). `refuse_dying_parent` (R26's closer, at every insert path)
read the KEY parent — the stripe, minted in the holder's rotor AFTER the
other daemons' projections loaded — through `read_inode_value_routed`,
the cross-volume plan's OWN-record witness, i.e. this mount's projection
of the slot: no record. PR 7b's fixture (declared regions, one process,
one RAM tree) could not see it. Fix: `key_parent_nlink` reads the record
through `getattr` — the writer's read divert (PR 12b: the holder's token
plane, exact until recalled); an own-slot parent and every unarmed mount
take the local read verbatim (`token_serve` answers `None`); the same
read serves `dying_parent_errno`, whose local read had a second face — a
REAL `EEXIST` on a foreign striped parent was reported as `ENOENT` when
the projection lacked the stripe. Pin: `sym_n_daemon_tests::
a_foreign_create_into_a_striped_directory_reads_the_stripes_record_at_its_
holder` (joiner 2 the process's reading writer under PR 9's custody arm —
its reads of joiner 1's slot are token reads at joiner 1's listener, the
fleet's shape; joiner 1 flips its directory after joiner 2's projection
loaded; 12 post-flip foreign creates land in their stripes,
`dying_refusals` unmoved).

### 4.4b Defect 9 — FIXED (PR 6 + PR 12b): the served ship never fed the holder's dominance window, and a JOINED holder's offer refused — no idle tree ever moved on a fleet

Found by `sym-foreign-touch`'s IDLE phase (gate 3c) on the same binary:
12 dominating bursts of 64 foreign creates over 133 s into m60's idle
tree, `slot_offers` 0 at every daemon, no handover — while the LIVE phase
passed (192 ships, 0 handovers; defect 7's class gone). Four defects
under it (the fourth found by the pin: `slot_handovers` counted the
in-process accept's release + grant alone — a WIRE holder's release on
its recall ran uncounted, so the leg's handover signal could never move
even with the offer landing; the manager now counts a `ReleaseSlot` that
SPENDS a standing recall as the handover and the requester sets the
never-thrash cooldown on ITS plane at the accept — `joined_accept_offers`):
(1) PR 4's `note_slot_ship(slot, requester, ship_ns)` — "the
holder decides who moves a slot … at the served ship" — had NO product
caller: PR 4's contracts drove it directly and PR 6's served step
(`xv_serve_step`, the production ship) never called it, so `ops_q` never
accumulated on any fleet and neither offer arm could fire; (2) a JOINED
holder's verdict reached `manager_offer_slot`, a manager verb's local
executor, which `manager_gate` refuses on a joined appender
(`joined_control_refusals`) — the offer never reached the manager's
table; (3) found by the pin once (1) counted: the door's holder-op note
(PR 12b round 1 Issue 11 — INSIDE the door, for every commit naming the
slot) counted the SERVED step's own commit as the holder's activity, so
`ops_h` grew 1 : 1 with `ops_q` and `ops_q ≥ 2 × ops_h` held for no
requester ever — the law's premise is the holder's OWN work. Fix: the
served step notes the ship after its apply with the served wall as
`ship_ns` (the requester = the shipping mount's appender: its member id's
identity where the plane learnt it — `SlotLeasePlane::appender_of_
identity` — else, for an insert, the creator through the child's slot,
the stripe census's own resolution; the wire `ResolveSlot` answer of
defect 7's fresh resolve is LEARNT into the projection so that read
answers without another verb), a joined holder's offer travels as the
wire `OfferSlot` (`joined_offer_slot`), and `KvTx::served_step` — set by
`xv_apply_step(.., served = true)` at the served side alone — skips the
door's holder-op note; the accept, the recall on the holder's carriage
and the flush-then-transfer are PR 12b's existing member side. Pin:
`sym_n_daemon_tests::a_dominating_requester_earns_an_idle_joined_holders_
tree_through_served_ships` (joiner 2 ships `N_floor × 4` creates into
joiner 1's idle directory served at joiner 1's listener: the holder's
`ships` count every one, ONE idle offer, the manager's table `Offered`
to joiner 2 on its carriage, `joined_control_refusals` 0; the accept
recalls, the holder's carriage sink hands over, joiner 2 holds the slot
at `g + 1` with every acked name).

### 4.4c Defect 10 — FIXED (PR 4 under N daemons): a joined holder's `N_floor` sat at its absolute floor of 2 for the mount's life

Found by `sym-shared-dir` on the defect-8/9 binary: every create landed
(20,000 in 4.49 s) but `slot_handovers` read 1 — the directory moved to
the first requester whose 2 ships beat the holder's second own op, in
the storm's first milliseconds. `N_floor = max(2, ceil(ewma_handover /
ewma_ship))` is seeded ONCE at the plane's arm from the always-on tables
(`seed_n_floor_inputs`: the barrier EWMA and the S8 ship RTT); a JOINED
appender arms before its first device write and before any S8 ship, so
both read 0, the seed took nothing, and `n_floor(0, 0)` = 2 — the
"single touch never moves anything" floor doing duty as the handover
price. The leg's own log said so: `N_floor(A)=2`. And a WIRE holder's
handover cost was never folded: `fold_handover_ns` ran at the manager's
in-process accept alone, so the holder that PAYS the flush-then-transfer
learnt nothing from it. Fix: `note_slot_ship` re-seeds while
`ewma_handover_ns` is 0 (the tables are populated by the first served
ship), and `transfer_slot_locked` folds `flush + page + tree 0` at the
holder. The dominance law itself is unchanged (a dominator wins over a
12,000-creator crowd by design — `dominance_over_a_common_window_decides_
every_offer`); what changed is that its price is the DERIVED one from the
first ship, and the measured one after the first handover (5.5 ms on the
fleet against ≈ 0.3 ms ships ⇒ ≈ 18).

### 4.4d Defect 11 — FIXED (PR 6 + PR 12b): a stale holder view at the travelling guards surfaced EAGAIN to the application

Found by `sym-shared-dir` on the defect-10 binary: m61 created 52 of
2,500 then `EAGAIN`; its log: `cross-owner guards for scope … name forest
slot 3, which this mount does not lease — the initiator's holder view is
stale; it re-resolves through tree 0`. The manager had handed its stripe's
slot 3 to a dominating requester between two of m61's creates (a legal
verdict — §4.4e below is what makes it moot for stripes); m61's
projection still named the manager, the manager refused the `XvGuards`
with the text above, and NOTHING re-resolved: `acquire_guards_leased`
returned the refusal and the create failed. Fix: the refusal IS the
re-resolve's trigger — every key's slot is re-resolved at the manager
(`KvMetaBackend::reresolve_slot_holder`: one wire `ResolveSlot`, its
`Holder` / `Unleased` answer LEARNT into the projection — table, gate,
holder cache) and the acquisition retried, bounded at 2
(`xv_cross_owner_guard_stale_reresolves`; a third stale answer is the
retryable class the caller sees). Pin: `sym_n_daemon_tests::
a_stale_holder_view_at_the_guards_re_resolves_and_lands_the_create`
(joiner 2's projection names the manager for a directory's slot; the
manager hands it to joiner 1; joiner 2's create lands after ONE
re-resolve, its projection names joiner 1, the dentry is in joiner 1's
tree).

### 4.4d' Defect 12 — FIXED (PR 12b): a joiner's projection dirt refused every transfer-in of a manager slot

Found by defect 11's pin (RED at its premise): joiner 1's accept of the
manager's offer failed `corrupt KV encoding: slot 4's cache barrier: node
… is dirty or locked on the recoverer — a mount wrote to a slot it did not
lease`. The cross-daemon adoption barrier (`adopt_transferred_slot_tree`
→ `NodeCache::drop_slot_nodes`) rests on "a foreign slot is never dirty
here — the door refused every commit"; on a JOINED appender that is
false: its open REPLAYS ring 0's un-checkpointed window into its
projection (the manager's records, dirty in RAM as every replayed record
is), so any joiner that joined while the manager's window stood held
dirty projection nodes of the manager's slots, and every later grant of
such a slot to it — an accepted offer, a first touch after the manager
released it — refused, for the mount's life (PR 12b's fixtures
checkpointed the manager before the joins). On a joined appender the
barrier DISCARDS the slot's cached nodes (`discard_slot_nodes`, PR 10's
failed-recovery inverse — the dirt is never this mount's own); the
manager's barrier keeps the refusal (its projections are never dirty).

### 4.4e Defect 13 — FIXED (PR 4 + PR 7b): ships into a STRIPED directory fed the handover's dominance window

`sym-shared-dir` on the defect-10 binary: `slot_handovers` 1 — the
manager's supplied stripe (slot 3) moved to the first requester whose
few ships into that 1/K shard beat the manager's own few. A legal verdict
of §5.1.4's law over a slot the striping already spread K ways, and the
gate-3b law says "no handover (aggregate never triggers)": §5.6.5's own
split — ONE dominating creator is a handover candidate, MANY are a
striping one. `xv_serve_step` feeds no dominance window for a ship into a
known stripe or a directory whose map this mount has read
(`RoutedMetaBackend::is_striping_domain` — two `scc` probes); an ordinary
directory's ships are unchanged (gate 3c).

### 4.4g Defect 14 — FIXED (PR 7b + PR 12's `-o ro` reader): a token reader listed a striped directory's RAW tree

Found by `sym-shared-dir-ls` once every create law of gate 3b held
(20,000 creates, 4,610/s, flip at the holder, 17,459 stripe ships,
closure `shipped ≡ served`, 0 handovers): the token reader's `ls -l`
statted 0 of 20,000 children — it listed 65 entries: the 64 nameless
stripe directories and the NUL-named markers rendered as empty names.
PR 7b's `stripes_armed` = `slot_lease_armed()`, and a `-o ro` reader has
no slot-lease plane, so the map was never read there, the markers never
filtered, the K-way merge never run — the row the design's gate 3b
`-ls` leg exists to price ("K stripe tokens + C inode tokens, 0 leaf
reads") could not run on a reader at all (PR 7b's owed "the wire reader's
per-stripe token merge", PR 12's). Fix: `KvMetaBackend::striping_plane_
armed()` = the plane OR a token reader; the map, the merge, the marker
filter and the `stat` fold key on it (never a mutation gate — the reader
writes nothing, `persist_striped_times` is holder-only). The reader's
map, stripes and children come as tokens from their holders. Pin:
`sym_coherence_tests::a_token_reader_lists_a_striped_directory_as_the_
merge_of_its_stripes` (48 names over 4 stripes: the reader lists exactly
the user names, resolves and stats every child, `stat D` folds, ≥ K + C
grants).

### 4.4h Defect 15 — FIXED (PR 12's `-o ro` reader / PR 5's plane): a token reader's plane never followed a manager failover — every read `EIO` for the rest of the reader's life

Found by `sym-crash` round 1 on the fixed-defect-6 binary: after the
manager's `kill -9` + remount the token reader (m1) answered `EIO` to
every op — `.stats` unreadable, `membership_readers` never 1 again
(attempt 1 read "never reached 1 within 129 s"). Its log: `token recall
channel to 192.168.86.52:40325 could not connect … retry in 5s` for ever,
`read token unavailable: the recall channel to the holder is not fresh`
— the DEAD manager's ephemeral listener, while the successor had armed
on `:34261` and the reader's membership had already re-pointed to the
successor (era 3). The manager's plane is the reader's DEFAULT
(`tokens_reader`, a `OnceLock` with a fixed `cfg.endpoint`, dialed at the
arm on the endpoint appender 0's claim-set entry named); PR 12b gave the
WRITER's per-holder planes a follow arm (`data_grant::foreign_read_plane`
→ `rebind_holder_endpoint_if_moved`: "a manager failover keeps appender
0's identity and publishes a NEW listener") and the READER's planes —
the default and the per-holder ones alike — got none. Fix (`KvMetaBackend::
reader_plane_follow_holder`, `TokenReaderPlane::repoint`): a resolve whose
plane's channel FAILED (its dial refused — `ChannelWait::Failed`, early,
never the whole window at a dead address) or never freshened re-resolves
the holder's endpoint off DURABLE state (`sym_join::resolve_holder_
endpoint`: its page identity → its claim-set entry, a control xattr read
off the reader's own S5 projection — no wire; the poll refreshes it from
the successor's checkpoints), and a MOVED endpoint re-points the plane
IN PLACE: identity, gauges, data sink and R5 registration kept; the
endpoint word and its generation swapped (every pooled grant session and
the channel's session were dialed under the old one and drop before
their next call), every cached token dropped with its purge (a holder
that moved may have re-granted — PR 5's dead-holder law), the channel
task woken out of its backoff to dial the successor at once; the binding
for the holder moves with it. A per-holder plane (a rejoined joiner at a
new port) is stopped dead and the successor's dialed. The same address
keeps the shipped fail-closed window verbatim. Gauge `dlm_token_holder_
repoints` (0 on a fleet that never failed over). Pin (RED-first with the
fleet's exact text against the shipped shape, GREEN in 4.2 s): `sym_mount_
posture_tests::a_token_reader_follows_a_manager_failover_to_the_successors_
listener` — the manager enrolled at A, the reader served from A; the
manager dies, a successor at the same identity enrolls B, the reader's
poll adopts it; the next `getattr` SERVES from B, the plane `Arc::ptr_eq`
the one armed, `grants` continuous, `holder_repoints` 1, holder 0's
binding moved.

### 4.4i Defect 16 — FIXED (PR 5's `-o ro` reader): a token reader's `listxattr` served the token's CARRIED names alone — no `client:` registration was ever discoverable on a reader, so its census shard never re-enrolled at a successor's coordinator

The same `sym-crash` round, the gate after defect 15: with the reader's
planes following the successor (both volumes re-pointed `:33703 →
:34493`, the acked-writes oracle GREEN, `self_fences 0`), the round
failed "no fleet worker enrolled at the successor's coordinator 80 s
after its arm (`job_remote_workers=0`)" — the reader's worker logged
`no coordinator endpoint published yet` for the rest of the round. The
worker's discovery is `cluster_wire::discover_endpoint` →
`mount_registrations` = `listxattr(1)` + one `getxattr` per `client:`
record; on a token reader `KvMetaBackend::listxattr` DIVERTED to the
token serve and returned the token's carried names (the user-visible
class) ALONE, so ino 1 listed as `[]` and no registration existed on the
reader — the worker enrolled once at mount (its first discovery ran
before the token arm) and never again. `getxattr` of a control name
already read the projection (`token_carried_xattr` is the one list);
the listing now does too: the token's names + the CONTROL names off the
reader's own trees. Pin: the failover contract's second half — the
successor's `client:` record with its job endpoint, checkpointed, the
reader's poll adopts it, `listxattr(1)` names it, `mount_registrations`
carries it, `discover_endpoint` answers the successor's coordinator
(RED: `[]`). The standing PR 12b finding stays: a dead incarnation's
record is heartbeat-fresh for `CLIENT_STALE_TTL` (45 s) and sorts first
by id, so re-enrollment lands within 45 s + the retry grain; the leg's
80 s bound covers it.

### 4.4j Defect 17 — FIXED (PR 7b's read paths on PR 12's `-o ro` reader): a token reader took the HOLDER's arms in the stripe-map read — its negative "unstriped" hint outlived the holder's flip, and the root listed EMPTY

The same `sym-crash` round, its last gate: with defects 15/16 fixed the
round reached "the reader m1 does not read joined writer m60's
post-failover name 60 s after it landed" — `ls /` on the reader listed
NOTHING, no error (`stat /r1`: ENOENT), while every joiner listed the
same root through the successor's token plane correctly. The successor's
log named it: `directory 1 STRIPED into 64 stripes (4 supplied by
creators [1, 2, 3, 4], 60 minted by the holder)` — seven joiners'
post-failover `mkdir /after-failover-*` had striped the ROOT under the
reader's cached root token. `dir_stripe::served_here` answered `true`
for a mount with NO slot-lease plane (the unarmed writer's law, where the
striping paths are off anyway), so on a token reader `stripe_map` ran
the holder's arms: the reader's first (pre-flip) read of `/` cached the
negative "unstriped" hint under lease `(0, 0)`, and the hint is
invalidated only by the holder's OWN flip — the successor's flip cleared
nothing at the reader, every later `stripe_map(1)` answered `None` off
the hint, and the listing fell to the token's raw dentry set with the
markers filtered: EMPTY (every name had migrated into stripes). Fix: a
plane-less mount serves everything it WRITES and nothing it only READS
(`served_here` → `!is_read_only()` without a plane) — a reader is never
a holder: no negative hint, no migration kick, the markers re-read off
its cached token (one `find` per marker, no wire). Beside it: the recall
channel's reconnect backoff resets at a completed ROUND, not at the dial
— a successor that accepts the connection and refuses every frame (the
failover's membership re-assertion window: `holds no live membership
lease with this set's owner`) was re-dialed at the 50 ms floor 20× a
second. Pin (RED-first, `left: []` — the fleet's shape in one process):
`sym_coherence_tests::a_token_reader_holding_a_directorys_token_across_its_
flip_lists_the_merge` — the reader lists the unstriped ROOT (12 names,
its token cached), the holder flips root to 4 stripes and migrates, 7
more names land, the flip's inserts recall the token; the reader's next
listing is the exact 19-name merge and every name resolves.

### 4.4k Defect 18 — FIXED (PR 12b's projection on PR 13's granted-extent barrier): a joined appender's PROJECTION tree spun its whole restart budget on a RECYCLED root — `restarts [root-seq] = 256`, EIO on the user op

Found by `sym-scale` N = 8 on the defect-17 binary (attempt 3 — attempts
1/2 passed N = 8: a race): joiner m63 (appender 4, joined 11 s earlier)
read `traversal retry budget exhausted descending to level 0 (routing
loop — SMO protocol bug) restarts [root-retired, root-seq, routing-hole,
child-retired, child-seq] = [0, 256, 0, 0, 0]` twice on user ops one
second after `joined appender 4 dropped 1 stale projection node(s)
inside a fresh extent grant` ×3, and its create storm died. The shape: a
joiner holds tree 0 (and the manager's native slot tree) as a PROJECTION
— a `KvTree` whose root pointer is the one it adopted at open or at its
last `refresh_control_projection` (keyed on the ledger seq, run at the
cadence and at F3's re-dials). The manager compacts the tree (a root
swap), frees the old root's extent at the covering checkpoint, and the
free extent is RE-GRANTED — here to m63 itself, whose granted-extent
barrier (defect 3's `drop_nodes_in_extents`) dropped the stale image and
whose next mint wrote a fresh node there (a peer's write landing on the
device is the same shape). The image under the projection's root address
now carries ANOTHER node's seq, and `KvTree::descend` — which re-reads the
root from the tree's own pointer at every restart — exhausts its budget
on `root-seq`: nothing between restarts moves a projection's root. Before
the barrier the stale image was served (a bounded-staleness read, the S5
law); the barrier turned it into a loop. Fix: `NodeCache::install_
projection_refresh` (a JOINED appender installs `refresh_control_
projection` behind the SMO mutex's `try_lock` — a holder of the mutex
on a joiner that reaches tree 0 is a refresh already in flight, whose
root the next restart reads; the flush pass never traverses a
projection), and `descend` runs it every `PROJECTION_REFRESH_EVERY` = 8
root-pointer restarts of a tree this mount does not WRITE
(`NodeCache::is_projection`: tree 0 on a non-manager, a slot tree whose
structural verdict is not this mount's) — the walk restarts on the root
it installs (the ledger record that named the new root is the
checkpoint whose coverage freed the old extent). A writer's own trees
never take the arm (their root is live; a loop there IS the SMO protocol
bug the budget names); the exhaustion error now names the tree, its
slot, the root pointer and "a PROJECTION here". Gauge `meta_kv_
projection_root_refreshes` (0 on the manager and every flat mount). Pins
(new suite `tests/sym_projection_refresh_tests.rs`, tree-level and
deterministic where the fleet's race is not): the recycled-root shape
stood by hand (a projection-posture cache, tree A's root image dropped by
the barrier, its extent released and re-claimed by tree B under a fresh
seq) exhausts the budget with no refresh installed and names the shape;
follows B's root through the installed refresh (one refresh, the gauge
+1, the next lookup restarts nothing) — RED-first against the base
traversal (`if false &&` on the arm: the budget exhausted); a writer's
own tree never consults the hook.

### 4.4l Defect 19 — FIXED (PR 12b's F2 liveness word): a fresh SUCCESSOR read every live joiner as DEAD inside its re-assertion window — the acked-writes oracle lost 1,318 of 1,318

`sym-crash` round 4 (rounds 1–3 GREEN): right after the manager's
`kill -9` + remount the oracle read every joined writer's acked file
through the SUCCESSOR and every read was refused `EAGAIN` — `object …'s
slot is leased to appender 4, which the membership owner lists DEAD —
its slots are the recovery's within the ledger poll` — for every joiner,
1,318 of 1,318 "lost". The successor's S6 census is EMPTY until the
joiners' renewals re-assert (a beat, up to 4.3 s here), and
`foreign_slot_holder_live` read `MembershipOwner::member_is_live` — the
`RecordDeath` screen's word, where "not listed" is `false` on purpose
(a peer's death word never overrules a member held LIVE; an unknown
member proves nothing). Rounds 1–3 read after the beat landed. Fix:
`MembershipOwner::member_liveness` — THREE-valued: `Live` inside its
deadline, `Dead` past it / departed here (the departed memo, inside its
retention) / absent with the re-assertion window CLOSED (the durable
roster re-asserted or was recorded dead at the deadline), `Unknown` while
the window is open (the successor has not heard from it YET) — and the
F2 predicate refuses only `Dead`: an `Unknown` lessee is dialed once
(bounded; the manager's word if the dial fails), never refused. Pin:
`dlm_membership_tests::a_successors_liveness_word_is_unknown_inside_its_
reassertion_window` (a successor with its window open: an unheard-of
member `Unknown` while `member_is_live` stays `false`; a joined one
`Live`; after its clean leave `Dead`; past the deadline an absent one
`Dead`). The fleet round was the RED.

### 4.4n Defect 20 — FIXED (S9's served free under PR 12b's plane): the manager's count for a joiner's shipped free walked EVERY slot tree — its PROJECTION of the joiner's tree included — a leak class, and a routing loop once the projection's root was recycled

Found by the new `sym-walls` leg's first run (gate 7 row (a), attempt 4):
7 joiners × 16 × 64 MiB rewritten in place — 1,792 displaced blocks —
and `shipped = 0`, `served = 0`: every joiner's terminal free was
ABANDONED after 3 attempts (`free_ship_failures` +405 on one joiner,
`free_replays` +5,426 at the manager — the retries answered from the
dedup window): `durable block-reference count failed on /dev/nvme2n1
while serving a shipped free: … tree 0 (slot Some(1042), root …, a
PROJECTION here): traversal retry budget exhausted`. The served free
(`cowriter::durable_block_refcounts_with` → `KvMetaBackend::block_ref_
count`, S9's owner-side validation "the ledger, not the peer's claim")
counted the block's references over EVERY slot tree of the volume — on
the manager that includes the trees JOINERS lease, which are PROJECTIONS
there (the grant-time image; the lessee appends into its images and
moves its root under its own page). Two faces: (a) STALE — a reference
the joiner RELEASED still read as held in the manager's image, so the
free was `NonTerminal` for ever and the block leaked (pinned); (b) the
loop — the joiner's SMO retired the projection's root, `ReturnExtents`
returned the extent, the manager re-granted it, and the manager's
traversal of the projection spun the budget (defect 18's shape at the
MANAGER, which installs no refresh for a lessee's tree). PR 7 §5.4.3 law
2 is the law: an unshared block's references live in its OWNER's slot
tree and the lessee's terminal free carries that tree's verdict; a block
two slots share is the index's (`ReleaseShared`), never a count's. Fix:
`KvMetaBackend::block_ref_count_maintained` — the count over the slot
trees this mount WRITES (`!gate.is_foreign(slot)`; `SlotTrees::refs_
window_where`), the served free's word; unarmed and on a flat volume ≡
`block_ref_count`. Pin: `sym_n_daemon_tests::the_served_frees_refcount_
skips_a_joiners_projection_tree` — the manager publishes a block on a
file in one of its rotor slots, releases the slot, the joiner acquires it
(the transfer barrier adopts the live root) and RELEASES the block in its
ring; the manager's union count still reads the STALE 1 (asserted — the
old executor's word), the maintained count 0, and a reference in a tree
the manager writes counts on both.

### 4.4o Defect 21 — FIXED (PR 5's reader plane): a single-flight token fetch LOSER could lose the winner's wake — a joiner's `lookup(1)` parked 455 s

Found by `sym-walls` on the defect-20 binary (`/tmp/grok-justin/
pr13-walls2`): row (a)'s rewrite wedged on joiner m64 — the FUSE watchdog
named ONE `lookup(1)` overdue at 455 s and climbing while a fresh
`lookup` of the same name on the same daemon served at once; the census
was clean (no conveyor window, no pipeline permit, no ring park, every
other op on m64 served). `TokenReaderPlane::fetch` is single-flight per
object: the loser reads the in-flight entry, then `n.notified()`, then
awaits. The winner that FINISHED between the loser's entry read and its
`notified()` removed the entry and bumped the epoch BEFORE the loser
registered — and `sqz_notify` registers at creation, so a
`notify_waiters` before creation is lost: the loser parked for ever
(the tick re-polls the epoch-gated future alone, which never fires
again for a gone entry). The register-recheck-await idiom (the
`long-running` law every other parked wait in the tree already follows):
register FIRST, re-check the entry is still the winner's
(`fetching.read_sync(&object, |_, v| Arc::ptr_eq(v, &n))`), await only
then; gone ⇒ the winner finished ⇒ re-read the cache. Seam
`TEST_FETCH_LOSER_HOLD` parks the loser exactly in the window. Pin:
`sym_coherence_tests::a_single_flight_fetch_loser_registers_before_it_
rechecks_the_winner` — RED at its 5 s bound on the base ordering, green
with one grant (the loser re-read the cache).

### 4.4p Defect 22 — FIXED (PR 12b's joiner under PR 8's allocation lease): the W1 ladders ran a non-holder's eligible overwrite into the allocator's ERROR-logging gate

The same m64 log: a burst of `W1 in-place sub-block patch refused: this
armed symmetric writer does not hold the ALLOCATION LEASE …`
(`plane_gate` from `begin_patch_sole_owner`, reached from
`try_inplace_rewrite` / `try_sole_owner_patch` during the row's in-place
rewrite), one ERROR line + one `cowriter_accounting_refusals` — the
must-stay-≈0 tripwire — per eligible overwrite on every joiner. The
2026-08-19 mw-fleet storm fix made exactly this class a counted
DECISION for the CO-WRITER posture (`patch_ineligible_posture`, checked
before any allocator arm); PR 12b's joiner is a `writer` posture whose
allocation plane is PER VOLUME (the lease, never the posture word), so
the posture clause never fired for it. Fix: `BlockAllocator::
holds_ownership_plane` (the gate's armed question — `alloc_lease::
holding(vol_tag)` on a grant-armed allocator — answered without its
refusal), `SoleOwnerVerdict::NonHolder` decided FIRST in
`DataRouter::sole_owner_verdict` (before the custody clause and before
any probe), and all three W1 sites take it: the sub-block patch's match,
the whole-block `try_inplace_rewrite` (which now runs the durable clause
too — it had relied on the RAM predicate alone on an armed set, PR 7's
gap), and the dd probe's armed face (one relaxed load unarmed). The gate
stays defense-in-depth (pinned: reached directly it refuses and counts).
Pin: `sym_shared_refs_tests::the_w1_ladders_decline_a_non_holders_patch_
as_a_counted_posture_decision`.

### 4.4q Defect 23 — FIXED (PR 12b's joiner under PR 8's grant window): a joiner's never-published mint was abandoned INTO the allocator's terminal-free gate — an ERROR per abandon and a leaked grant block

Found by the first green `sym-walls` run on the defect-22 binary
(`/tmp/grok-justin/pr13-walls3`, both rows MET): the fleet's ERROR census
read ONE class left — `block free refused: this armed symmetric writer
does not hold the ALLOCATION LEASE …` (`free_block`'s `plane_gate`), 2
across two joiners, mid-rewrite. `BlockAllocator::abandon_unpublished_
offset` — the ACK-early overlay's superseded destination / a
failed-publish upload, a mint NO ledger ever named — has the co-writer's
lane recycle and the quiet counted abandon, then falls to
`self.free_block(offset)`; a JOINED appender is the `writer` posture,
so it took the terminal-free ladder and the gate refused (Err, the
caller's `let _ =`): one ERROR per abandoned mint and the block left SET
in the holder's bitmap with no reference and no window naming it — the
deferred leak release converges on it only after this mount's LEAVE
(PR 12b round 5's law: a live peer's window is adopted, the rest released
once every live peer declared). Fix: the recycle arm's GRANT-WINDOW face
— on a grant-armed allocator whose plane this mount does not hold, the
RAM reference goes, the incarnation word is retired, and the block is
given back to the window (`GrantWindow::give_back`: merged onto an
adjacent range, `consumed` un-counted, `installed` untouched — the next
lowest-first mint takes it, the leave's remainder returns it, a renewal
declares it inside the window); counted `block_grant_window_recycles`
(0 on every holder and every unarmed mount); a fenced era keeps the
quiet counted abandon; a second give-back of one block is the
double-handout lineage (refused, `cowriter_unpublished_abandons`). Pins:
`block_grant::tests::a_given_back_block_is_the_next_mint_and_merges_
onto_its_neighbours` and `sym_block_grant_tests::a_joined_appenders_
never_published_mint_returns_to_its_grant_window` (RED on the base: the
abandon's `Err` from the gate).

### 4.4r Defect 24 — FIXED (PR 7b under PR 12b): a joiner's `stat` of a striped directory folded every stripe's record off its PROJECTION — 256 `root-seq` restarts, EIO on the storm's create

`sym-scale` N = 8 from zero on `ef95bedc` (`pr13-batch5`): the create
storm on joiner m62 failed `EINVAL` at its 4,931st file — `tree 0 (slot
Some(103), root 0x1e00000@…, a PROJECTION here): traversal retry budget
exhausted … restarts [root-seq] = 256` — 1 s after the manager
auto-STRIPED `/` (the seven joiners' `mkdir`s were seven foreign creates
from seven creators — the PR 7b trigger; 61 stripes minted in the
manager's rotors, 3 supplied by m60/m61/m62). Defect 18's shape at a
DIFFERENT tree: `fold_striped_attrs` (`stat D` — every kernel attr
revalidation of `/`) and the rmdir's stripe count probe read every
stripe's record through `read_inode_value_routed` — the joiner's
projection of the manager's rotor slot trees, loaded at its join; the
lessee's compaction had retired a projected root's extent, `ReturnExtents`
+ a re-grant handed it to m62 (its barrier dropped the stale image at
09:05:03, the mint wrote there), and the pointer named another node's
seq. Defect 18's refresh cannot heal it: KD-SYM-3 — a LEASED slot's root
rides its lessee's PAGE, never tree 0, so `refresh_control_projection`
re-adopts tree 0 and the native tree only. The rule is defect 8's:
**a stripe is minted in ANOTHER appender's slot, so at every non-holder
its record is a FOREIGN read** — `dir_stripe::stripe_record` (`getattr`
through the writer's read divert, the holder's token plane; an own-slot
stripe and every unarmed mount read locally) is the ONE read the fold
and the rmdir probe run. Pin: `sym_n_daemon_tests::a_joiners_stat_of_a_
striped_directory_folds_the_stripes_at_their_holder` — RED `nlink 2 vs
3` (the base's fold saw no stripe record: the stripes were minted after
the reader's projection loaded). Standing, same class, not per-op:
`is_stripe`'s reverse dentry scan (`find_parent_of_child`) walks every
slot tree on the flip candidate's holder — over projections on a joiner
(§7 Owed).

### 4.4r' Defect 24's second face — FIXED: the fold read at the holder made the MANAGER's `stat /` fail for a dead supplier's whole death window

`sym-storm` from zero on `8d7fd3c0` (`pr13-batch7`, the batch whose
`sym-crash` ran 10/10 GREEN): the seven joiners killed; `/` had been
auto-striped under their `mkdir`s with THREE stripes they supplied; and
the harness's first `cat /mnt/…/m0/.stats` — the MANAGER's — read `EIO`:
`read token unavailable: the recall channel to the holder is not fresh`.
Defect 24 made every per-stripe record read a token read at the
stripe's holder — three holders were dead for the 15 s before the
recovery, and the fold failed the whole `stat /`; the kernel revalidates
`/`'s attrs on every path walk, so EVERY op under `/` at the manager
(`.stats` included) failed for the window. The fold is a DERIVED
attribute of an object this mount HOLDS, and a dead lessee's stripe
cannot move: a stripe whose holder cannot be reached contributes NOTHING
for the window (its `nlink` term, its times — bounded by the recovery,
which makes the slot the manager's), counted `dir_stripe_fold_
unreachable`; the stripe's DENTRIES stay exact-or-nothing (R-SYM-4 is a
law about a foreign object's user-visible metadata, not about a derived
term of an own object). Pin: `sym_n_daemon_tests::a_striped_directorys_
stat_at_the_holder_survives_a_suppliers_death` — RED with the fleet's
exact text. Harness: the storm's death window is exactly where the
harness reads every daemon's `.stats`.

### 4.4s Defect 25 — FIXED (PR 5's ledger poll vs PR 2/10/12b's checkpoint-class steps): a consumed checkpoint seq left a LEDGER GAP, and every token reader's poll stopped on it for a whole ring of checkpoints

`sym-storm` round 1 from zero on `ef95bedc`: the seven joiners killed
at 09:13:09; their regions recovered 09:13:22–27 (`Unleased` in tree 0,
`64 slot(s) released` × 7 × 2 volumes); the acked-writes oracle GREEN;
then at 09:14:08 the token reader m1 could not `stat` a recovered file —
`read token unavailable: the recall channel to the holder is not fresh`
— its tree 0 STILL naming appender 1 as the lessee 41 s after the
release; live 27 minutes later it resolved. The mechanism, verified on
the code: `release_recovered_regions` (PR 10), `grow_stalled_regions`
(PR 2), the in-process leave and the wire `LeaveAppender` (PR 12b) each
CONSUME a checkpoint seq for their bitmap write ("ledger slots are seq %
32, so the gap is harmless") — and PR 5's predicted-slot poll
(`read_newest_ledger_from`) reads slot `(adopted + 1) % 32` and STOPS on
an older record there ("the writer has not written that seq"): a gap of
one parks the reader until the writer's seq wraps the ring (32
checkpoints ≈ 32 s at the shipped cadence — UNBOUNDED on a quiet
writer); the storm's seven releases at 09:13:23 parked m1 before the
09:13:27 releases landed, and every read of the recovered slots dialed
the dead lessee for the whole window. Two halves, one law: **a consumed
seq is a ledger seq** — `KvMetaBackend::consume_checkpoint_seq_for_
bitmap` writes the bitmap at `ckpt_seq`, then a record at `ckpt_seq`
RESTATING the last cycle's word (the roots as they stand, the last
record's tail, `next_ino`, the watermark) under the SMO mutex (no cycle
mid-flight: content-equivalent to the record it follows, so a crash
after it replays exactly what a crash after the last cycle would);
`meta_kv_ledger_restatements` counts them (0 on a flat mount). And **the
belt**: a crash between a bitmap write and its record leaves one gap for
the volume's life, so after `ROOT_LEDGER_SLOTS` consecutive stopped polls
the reader reads the whole ledger once and adopts the newest record
anywhere (`meta_kv_revalidate_gap_scans`; a truly idle writer costs one
128 KiB read per 32 idle polls). Pins: `sym_n_daemon_tests::a_readers_
ledger_poll_walks_across_a_consumed_checkpoint_seq` (a joiner's wire
leave then the manager's next cycle; RED `None` between seqs 11 and 13
— the walk stopped) and `sym_coherence_tests::a_readers_poll_scans_the_
whole_ledger_after_a_ring_of_stopped_polls` (a forged gap; RED 0 scans).
The reader's own staleness bound (S5's `interval + ceiling`, PR 5's 0
for metadata) holds again.

### 4.4t Defect 26 — FIXED (PR 12b's mount path): a successor remounting inside a killed manager's exit window JOINED the dying listener and the mount refused

`sym-crash` round 1 from zero on `ef95bedc`: `mount 0` after the
manager's kill -9 — `dialing the manager at …:39623 for JoinAppender
failed: Connection reset by peer`, the successor remount FAILED.
`symmetric_join_target` calls a heartbeat-fresh claim whose pid is not
yet provably dead a LIVE manager (the D0 ladder's own word); a `kill -9`
returns before a daemon with gigabytes of dirty pages has exited, the
harness remounted inside that window, and the dying process's listener
accepted-then-reset the dial (RST, not ECONNREFUSED — the process was
still there). The join's transport failure was `KvError::Busy` — the
class of a manager that REFUSED — so the mount refused. Fix: the dial and
the `JoinAppender` call itself (the open's first act — nothing of the
join exists yet) answer `KvError::ManagerUnreachable` (errno
`EHOSTUNREACH`, `meta_backend::join_dial_failed`), and the mount path
re-reads the join target ONCE on it: a manager the probe no longer calls
live (the pid gone — the dead-pid proof) makes this mount the D0
ladder's; a manager still live-looking keeps the refusal (a cross-host
crash waits the claim's TTL exactly as the D0 ladder always did).
Harness: `mw_fleet.sh kill` waits for the victim's pid to vanish
(bounded 60 s, the exit wall logged) — a supervisor's restart never
starts inside the exit window. Pin: `sym_n_daemon_tests::a_join_at_an_
unreachable_manager_is_the_transport_class_the_mount_path_retries`.

### 4.4u Defect 27 — FIXED (PR 8/12b, under PR 14's owed terminal-free-engine swap): a former lessee's blocks were "lost" to fsck at the manager, and a local free of one REFUSED — leaked

`sym-scale`'s fsck oracle from zero on `f38776a7` (`pr13-batch8`,
attempt 8 — attempt 7's oracle had recorded NO block-plane verdict: `0
block(s), 0 refcount(s) checked`): after every joiner's clean leave the
manager's online fsck raised **2,816 C2 "lost block … referenced offset
is not allocator-tracked"** on `nvme3n1`, every one an ingest block of a
departed joiner (128 per 512 MiB file). The manager's RAM refcount map
knows its OWN mints and its mount-time by-block census alone (PR 7 kept
the scan as the free list's derivation; PR 12b's joined open performs
none); a block a joiner minted from its grant window is SET in the
holder's bitmap (PR 8 — the bitmap IS the free list there) and durably
referenced, and once the joiner's slot is released, handed over or
recovered to the manager, its tree is the manager's to read and its
blocks are untracked THERE. Two faces: fsck's C2 read RAM alone — a
FALSE finding class on every N-daemon set with departed writers; and the
LEAK — `begin_free` on such a block is the double-release REFUSAL (an
ERROR per block, `block_untracked_free_refusals` — the must-stay-≈0
tripwire — and the bit SET for ever), so every `rm` at the manager of a
file a departed joiner wrote leaked its blocks (unexercised by the legs:
their deleted files are inline). Fix: (i) `BackendRouter::untracked_free_
gate` — a terminal free at the allocation HOLDER of an untracked offset
runs `cowriter::execute_shipped_frees`' ladder LOCALLY (finding 13's law
for a shipped free of an untracked offset: the durable ledger population
decides; 0 ⇒ seed one reference and run the ladder; > 0 ⇒ non-terminal;
already free ⇒ the refusal), installed by `arm_shared_refs` beside the
shared-block gate, counted `block_untracked_free_adjudicated`; (ii) fsck's
C2 on a grant-armed HOLDER reads a SET bit in the held bitmap as TRACKED
(`fsck_alloc_bitmap_tracked_exempted`). Pin: `sym_shared_refs_tests::
a_holders_free_of_a_former_lessees_block_runs_the_owner_ladder_instead_
of_refusing` (RED: the refusal, the bit SET). The engine swap itself —
the bitmap as the terminal-free engine everywhere, the zero-census open —
stays PR 14's (§7).

### 4.4m Defect 16's regression, caught by the same batch and narrowed

`sym-shared-dir-ls` on the defect-16 binary read `meta_kv_node_cache_
misses = 1,219` on the reader against the law's `dropped + 8 × epochs +
K = 324` — "a DATA leaf was read for the listing". `ls -l` probes the
ACL names per file (`getxattr` / `listxattr` of `system.posix_acl_*`),
and defect 16's fix read the CONTROL names off the reader's projection
at EVERY `listxattr` — one projection leaf per listed file. The control
class lives on ino 1 alone (and its slot-0 guest keyspace after a
migration): the merge is now confined to those inos; every other
object's listing is its token's, no leaf read. The failover pin's second
half (ino 1) stands; the `-ls` law is judged again in the from-zero
batch.

### 4.4f Harness — `sym-foreign-touch` LIVE's storm died at launch

On the defect-10 binary the LIVE phase read `slot_handovers` 1 "a live
holder was recalled by a touch": the holder's storm was launched BEFORE
its target directory's `mkdir -p` and died on its first `mkdir` (ENOENT,
`live-a.txt`), so the "live" holder was IDLE and the law moved the slot to
B correctly (`N_floor` 15, the dominated arm, `rotor_mints` +1 at A over
the phase). The leg creates the directory first, runs the storm in
ROUNDS (a fresh subdirectory each — one storm of `SYM_FILES` ends in
seconds at the holder's own rate) for the phase's whole length, and
refuses the LIVE verdict if the storm is not alive at the last touch.

### 4.5 `appender_flush_ceiling_overruns` (must-stay-0) — 2 overruns, venue-attributed pending the box

At the tail of the N = 8 (and once the N = 4) create storm the manager's
volume 1 counted 2 overruns: the oldest dirty slot-tree leaf aged 1,110 /
1,129 ms at the covering barrier against the 1,100 ms landing ceiling
(trigger 1,000 + 2 × 50 ms ticks) — the checkpoint task's pass ran 10–29 ms
past its 2-tick margin under 64 creator threads + 8 daemons on the throttling
32-CPU laptop (`manager_load_pct` ≤ 2 %). The margin is
`checkpoint_landing_ceiling_ms`'s fixed 2 ticks, not a measured pass time. If
the box row trips it too it is a PR 14 item (derive the margin from the
measured pass wall); the sym-scale leg reports it per row in the VERDICT
column (per-row deltas — a previous row's count never bleeds into the next).

### 4.6 Harness findings

* A joiner whose volume fail-stopped WEDGES the product `umount`
  (`mw_fleet.sh unmount` hangs; `fusermount3 -uz` is the teardown) — an
  operability note for PR 14.
* The manager's `ExtentGrant` control entry admits `Try` in the USER class;
  under eight reactive refills at once on a small ring it answers
  `JournalReserveExhausted` (the caller's retry class, `joined_wire_failures`
  — 140–154 per joiner in the in-process two-round storm), retried at the
  joiner's cadence. Not a defect; stated because the in-process heavy pin
  reports it.
* `kv_freeze_wedge_tests::a_dropped_forced_compaction_leaves_the_node_
  freezable` is RED under `SQUEEZEFS_TEST_STAMP_SYMMETRIC=1` (green flat)
  on this tree AND on the base: its census probe is flat-shaped —
  `candidates.iter().find(|(t, _)| *t == TREE_INODES)`, while every forest
  slot tree's id is 0 (PR 1: the kind bytes are the tree ids; PR 4 round 4
  made the census resolve a leaf by id AND slot). The suite is not in the
  matrix's stamped list and was never run stamped; not a product finding.
  Making the probe layout-blind is a one-line harness item for the matrix's
  next widening.
* The `SupplyStripeIno` path declines a creator "no endpoint bound on this
  mount" instead of binding it on demand (`bind_holder_endpoint_on_demand`
  runs at the guards and the steps, not at the supply): the holder mints
  the remainder, so the flip is unaffected — 63 of 64 stripes land in the
  holder's rotor instead of the creators'. Owed to PR 14 (one call at the
  supply).

## 5. SIM-1 (gate 8) — `SimConfig { clients: 12_500, shards: 64 }`, release, dev box (measured-simulated, tier (ii))

`sym_block_grant_tests::sim1_at_the_operating_point_12500_members_over_64_shards`
(`--ignored`; `run_sharded` with the slot-lease carriage, the broadcast token
recall, the death ledger and the free-grace V-fan-in — landed in `502aa859`):

```
clients=12500 beats=2 renewals=25000 wall=0.077s (324121 renewals/s offered)
volume-0 journal tx/s: 0.000 (delta 0 entries)                      ← the S6 gate: liveness off the journal
renewal latency (WITH the slot-lease carriage): p50 0.41 µs, p99 0.58 µs, max 10.21 µs
grace completion: 547.78 µs; shards=64 parked=196 reclaimed=196 park_expiries=0
slot-lease carriage: 50000 lease word(s) on the grants (M = 2 per member per beat)
token recall fan-out: 12500 holder(s) of one object recalled in 24656 µs, 12500 ack(s)  (in-process — no wire RTT)
death ledger: 12 record(s), sink ≤ 15.16 µs, poll cadence 1100 ms, read by 64 shard(s)
free-grace fan-in: every shard closed on the label in 5925 µs
```

Envelope: beat p99 0.58 µs (S6-a's envelope is the wire RTT-dominated one; the
in-process number is the CPU term), eviction fan-out 12 deaths → 64 shards
within one poll cadence (1,100 ms), the V-fan-in closing in 5.9 ms on the
on-change cadence, parked ≡ reclaimed, expiries 0.

## 6. The D1 arithmetic at fleet N vs §5.10, and the 15 k arithmetic re-derived

Every row is design §5.10's per-op law read off the fleet's gauges (the
dev-box tcp substrate — tier (ii) measured-simulated; the box brackets of
§8 are the counted rows, and every number here is a constant the box
rows re-measure, never a verdict):

| §5.10 row | Design | Measured (fleet, this record) | Holds? |
|---|---|---|---|
| `create`/`mkdir` under an OWN directory | 0 wire verbs | `sym-tarx`: **0.0122 wire verbs per entry** on 2,468 entries (30 manager verbs = the join + the extent-grant refills; `xv`/`ship`/`pub` 0), `slot_handovers` 0, `dlm_rpcs` 0 | yes — the ≈ 0 law (gate 2's bound 0.05) |
| `create` in a SHARED (striped) directory | 1 ship + 1 barrier per foreign create; never a handover | `sym-shared-dir`: 20,000 creates by 8 writers, `xv_shipped ≡ xv_served` (17,537), `dir_stripe_ships` 17,204 (the 1/64 own-stripe lands are the difference), **`slot_handovers` 0** after defect 13 | yes |
| `lookup`/`stat`/`readdir` of foreign objects | 1 token per object first touch, 0 while cached; `readdir + stat` of a striped directory = `K + C` tokens cold | `sym-shared-dir-ls`: **20,067 grants for K = 64 + C = 20,000** (K + C + 3: the directory, its parent, the root), `dir_stripe_readdir_merges` 43, 0 data-leaf reads (the reader's `node_cache_misses` = the S5 poll's re-reads + one tree-0 lessee read per stripe slot), `dlm_token_hits` 284,483 | yes |
| foreign touch of an IDLE tree (`mkdir /jobs/X`'s shape) | 1 handover if idle, then 0 | `sym-foreign-touch` IDLE: handed over after 1–2 bursts of 64, `slot_handover_phase_ns` total **5.5–7.8 ms** (flush 1.9–4.1, page 0.15–0.18, tree 0 3.5) | yes; the cost inside §1.6's 5–20 ms |
| foreign touch of a LIVE tree | ships, never a handover | `sym-foreign-touch` LIVE: 192 ships, 0 handovers with the holder's storm alive | yes |
| Manager verbs per volume (per-op work = 0) | steady ≪ 100/s | `manager_load_pct` 0–2 % at N = 8; the eight-writer in-process storm's grant/return refills 100–150 per joiner per run under `JournalReserveExhausted` (the retry class) | yes; the manager's ring window is the N = 8 storm's only pressure point (§4.6) |
| Recall fan-out (holder side) | R recalls per mutated object, batched per pass | `sym-readers` (1 reader): `recalls ≡ mutations × holders` 5/5, `fanout_p99` 1, `recall_rtt_mean` 125.5 µs; SIM-1: 12,500 holders recalled in 24.7 ms in process | yes (the N = 32 reader shape is §7's) |
| Handover reclaim by a bursty owner | ≤ 1 handover per burst per direction | `N_floor` ≈ 18 after the first measured handover (5.5 ms ÷ ≈ 0.3 ms ships) — defect 10's seed made it 2 for a joiner's life before | yes after defect 10 |
| Aggregate creates scale with N | ≥ 0.7 × N × the N = 1 rate | `sym-scale` r5 (2a94abbc): 6,874 / 19,192 (2.79×) / 27,909 (4.06×) / **39,778 (5.79×)** creates/s at N = 1/2/4/8 — the law MET at every N; ingest 2,481 / 5,679 / 8,015 / 10,309 MiB/s (N = 8 4.16× — one box's memory bus; the box decides) | yes for creates; ingest is the box's row |

**The 15 k arithmetic re-derived** (§1.6's table, the constants this
record moved): `N_floor`'s cold start is no longer 2 on a joiner (defect
10) — the handover price at the operating point is the MEASURED 5.5–7.8 ms
÷ the served ship's ≈ 0.3 ms ⇒ ≈ 18–26 ships, so a `/jobs/X` touch never
moves a stripe (defect 13 makes it moot: a striped directory's ships feed
no window) and an idle tree moves only to a requester past that floor;
the per-holder ship cost stands at the §5.10 shape (1 ship + 1 barrier;
`meta_ship_owner_phase_ns` on the fleet ≈ 0.3 ms served) so 12,500
`mkdir /jobs/X` over 64 stripe holders is 195 × 0.3 ms ≈ **60 ms of each
holder's time** per wave (§1.6 wrote ≈ 6 ms at a 30 µs verb — the served
INSERT is a commit + its durability lane, not a verb: the 10× is the
barrier, and it is per WAVE); a reader's `ls -l` of the result is `K + C
+ 3` grants (§5.7.5's `K + C`, exact to the constant); death propagation,
the join storm and the ring budget are untouched by this record (their
rows are gates 7/8b's — §7). Nothing in §1.6 breaks first at a different
resource than it did.

## 7. Owed (what PR 14 / PR 15 inherit)

Product (each named to its rung, none flip-blocking — every one has a
counted decline, a bounded window or a stated venue):

1. **`is_stripe`'s reverse dentry scan over projections** (PR 7b on a
   joiner): `find_parent_of_child` walks every slot tree of the flip
   candidate's holder — a projection on a joiner, defect 24's class once
   per flip candidate, never per op. The fix shape is a divert-aware
   reverse scan or the `known_stripes` set fed at every map read on every
   mount; the candidate's own `stripe_map` read already learns it.
2. **`SupplyStripeIno`'s on-demand binding** (§4.6): the supply declines a
   creator with no endpoint bound instead of `bind_holder_endpoint_on_
   demand`; the holder mints the remainder, so a flip still lands — with
   the stripes in the holder's rotor instead of the creators'.
3. **`appender_flush_ceiling_overruns`** (§4.5): the landing ceiling's
   fixed 2-tick margin against the checkpoint pass wall on a throttling
   box — one overrun on `sym-walls` row (a) in attempt 5, none in attempt
   7; derive the margin from the measured pass wall if the box trips it.
4. **The manager's zero-census open** (PR 14 by design — the RAM refcount
   map's mount-time by-block scan replaced by PR 8's bitmap as the
   terminal-free engine) and **PR 7's un-share of a surviving sole
   owner**: unchanged from PR 12b's owed list.
5. **A second SHARD** (a home ≠ volume 0 — the owner-role fusion, the
   `shared_ref:` migration, the cross-shard rejoin retirement, the
   departure sink's cross-shard face; PR 12b's owed list) — the fleet
   rig stands one shard; the N = 32-member reader broadcast (gate 5's
   1 × 31 row) and the 32-mount join storm (gate 7's wall (b) at N = 32)
   are BOX rows (§8).
6. **`kv_freeze_wedge_tests`' flat-shaped census probe** and the joiner
   dead-manager checkpoint contract's 1-in-N flake (§4.6) — harness items
   for the matrix's next widening.
7. **The reader's per-op cost on a striped root**: the fold of a striped
   `/` is `K` token serves per `stat /` from the plane cache (one grant
   each per holder per token lifetime); a `stat`-heavy reader of a
   64-stripe root pays 64 cache hits per attr revalidation — measured
   nothing on the legs, stated for the box's `ls -l` row.

Records the box owes (§8): gate 1's solo re-gate A-B-B-A on the final
binary; gates 2 / 3 / 3b / 3c / 5 / 7's counted brackets — every local
number in §3 is a dev-box RATE reading (the mechanism rows are GREEN;
the rates are the box's).

## 8. Box footprint

_(pending — every file placed on squeeze-test.)_

## 9. The flip decision (for PR 14)

_(pending — the exact list of gates MET and NOT MET.)_
