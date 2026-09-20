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

`slot_handovers == 0`, `slot_ships == 0`, Σ `dlm_rpcs == 0`,
`manager_load_pct` 0–2 % on every row (the manager's verbs cost nothing
measurable at N ≤ 8 — its CPU is its OWN storm's). The 0.7 × N law is MET
at N = 2 and 4 on both runs; N = 8 is the defect-5 row.

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

### 4.3 Defect 6 — OPEN (PR 12b, P0: "deleted stays deleted" violated across a joiner's CLEAN LEAVE at N = 8)

Found by the eight-writer in-process storm pin (`sym_n_daemon_tests::
concurrent_storms_on_eight_writers_at_the_fleets_depth`, `--ignored`, ~25 s)
once it gained the fleet's row boundary — every writer UNLINKS its previous
round's 24,000 files before the next round — and a "deleted stays deleted"
arm at three points. **While every daemon is live, no removed name resolves
anywhere** (each daemon's RAM fold has its tombstones). **After a joiner's
CLEAN LEAVE** (`leave_joined_regions`: 64 × `transfer_slot_locked` —
flush-then-transfer — then `LeaveAppender`; no error logged, the ring
covered), the manager resolves 21–280 of the 192,000 unlinked names at
`nlink 0`, always the LAST unlinks of ONE leaving daemon's directory (the
tail of creator `c3`'s range), one or a few leaves' worth; a fresh writer
open of the volume resolves them too (the durable state lacks the
tombstones — 89 `C10ZeroNlinkNamed` findings in the offline census). N = 1
and N = 2 with the same unlink boundary are CLEAN (5 runs); N = 8 fails 6/6.
Two pre-checkpoints of the leaving daemon before its leave change nothing.

On-disk attribution (the pin dumps the leaf the released tree routes the
name to): the leaf's header is the owner's node, its frames are the owner's
(`appender 3, g 1`: the base bset + the APPEND carrying the name's `Put`),
and **no `Delete` frame follows** — the tail ends at the `Put`'s frame; in
two of five runs the extent also carries a stale-incarnation frame of
another appender PAST the walk (a previous tenant's residue, screened as
`StaleIncarnation` by the seq-space law — harmless, and the reason defect
5(a) had to land first: before it those frames FOLDED). `foreign_frames_
screened` / `appender_fence_breach` stay 0 across the leaves; the
return-of-a-live-image belt (`extent_return_live_refusals`, landed for this
attribution at every return site) never fires; `extent_grant_conflicts` 0.
So the last tombstones the leaving joiner applied in RAM were **never
appended** to the leaf image its own release named — the joined flush /
leave sequence (`joined_checkpoint_cycle`'s dirty walk, `checkpoint_flush_
node`'s freeze + append + `merge_after_flush`, the `flush_slot_clear_of_
region` post-condition) loses a leaf's final delta under the N = 8
grant-refusal storm (`ExtentGrant` / `ReturnExtents` refused
`JournalReserveExhausted` 100–150× per joiner as the manager's ring window
fills). Not yet attributed to the step; the pin is the reproducer, the
fleet's `sym-scale` leg gained the same arm (every joiner's product umount,
then the removed sample judged at the manager and at a remounted joiner —
§3.1's next row says whether the fleet shows it). **Routed to PR 12b's
leave / flush law; flip-blocking until fixed** (§9).

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

_(pending the box rows; the dev-box scoping constants: manager verb load
0–2 % at N ≤ 8, `slot_handovers == 0` on every own-directory row, the
recall fan-out of 12,500 holders in 24.7 ms in process.)_

## 7. Owed (what PR 14 / PR 15 inherit)

_(pending — filled at the close.)_

## 8. Box footprint

_(pending — every file placed on squeeze-test.)_

## 9. The flip decision (for PR 14)

_(pending — the exact list of gates MET and NOT MET.)_
