# 2026-09-07 — the co-writers' `CLAIM ANOMALY` population is the authority's OWN publish: the assembler's fold frees a co-writer's block and nothing tells the co-writer

| | |
|---|---|
| **Branch** | `fix/cowriter-claim-anomaly-population` off `dev` 0bd03455 |
| **Commits** | `0ecfa3f6` (red fleet-shape contracts + the `cowriter.lane_free_notices*` instruments) · `9ef1af9e` (the fix: publish schema 15 → 16, the per-client lane-free notice ledger) · the docs commit |
| **Evidence** | `/tmp/five/d4/box-seq3/keep3-{1D,2C,3C,4D}/rows/s11mpiio-*/m{0,50..57}_p{0..4}.json` (every mount's per-phase snapshots on the D-C-C-D sequence; D = `0bd03455`, C = `886d4e31`), `keep3-1D/m50.log` + `m0.log`; the predecessor note `2026-09-07-cowriter-claim-anomaly-lineage.md` |
| **Class** | data-path bookkeeping — a co-writer's LOCAL tracking of a block the authority displaced and freed by its OWN publish (the assembler's fold of the co-writer's shipped slices); the fleet's free supply hands the block back before anything retires the entry, and every re-claim trips a must-stay-0 tripwire |
| **Fleet row** | **OWED (parent, squeeze-test)** — see §7 |

## 1. The D row: the schema-15 retire engaged and the gauge did not move

Row 1D ran `0bd03455` (the schema-15 parked-key retire landed) as the D
of a D-C-C-D on squeeze-test. `cowriter.recomputed_retires` Σ8 = 21,386 /
5,555 / 7,338 / 8,871 over A1/B1/B2/A2 — the mechanism engages exactly as
its contract says. `block_claim_anomalies` Σ8 = **0 / 1,108 / 1,117 / 73**
against 0 / 1,478 / 1,411 / 67 (2C) and 2 / 1,035 / 956 / 58 (3C). The
parked-key population the previous note closed was real (its in-process
contract reproduced the fleet's line) and is **not the fleet's**: on the
fleet, 5.5–8.9 k parked keys per phase are retired at the reply and none
of them was the anomaly's. Tripwires 0, stale refusals 0, fsck clean, the
full matrix passing on every row.

## 2. The population, from every mount's snapshots

The per-phase deltas (`p1..p4` − `p0..p3`) of all eight co-writers and the
authority, Σ8 where the gauge is a co-writer's:

| row | phase | anomalies Σ8 | **m0 `fold_passes`** | m0 `overlay_installs` | m0 `overlay_superseded_by_served_publish` | Σ8 `extent_shipped` | m0 `extent_served` | Σ8 `extent_flush_forces` |
|---|---|---|---|---|---|---|---|---|
| 1D | A1 | 0 | 0 | 4 | 4 | 4 | 8 | 4 |
| 1D | **B1** | **1,108** | **1,183** | 1,139 | 1,136 | 2,357 | 2,900 | 543 |
| 1D | **B2** | **1,117** | **1,111** | 1,049 | 1,049 | 2,184 | 2,739 | 555 |
| 1D | A2 | 73 | 0 | 5 | 5 | 5 | 10 | 5 |
| 2C | B1 | 1,478 | 1,546 | 1,416 | 1,414 | 3,008 | 3,691 | 683 |
| 2C | B2 | 1,411 | 1,404 | 1,510 | 1,510 | 2,967 | 3,644 | 677 |
| 3C | B1 | 1,035 | 1,098 | 991 | 980 | 2,125 | 2,644 | 519 |
| 3C | B2 | 956 | 939 | 1,005 | 1,005 | 1,974 | 2,478 | 504 |
| 4D (short) | B1 | 8 | 16 | 39 | 39 | 55 | 91 | 36 |

Per fpp phase, on three full rows, the anomaly count sits within **−7 % …
+2 %** of the authority's `fold_passes`, and the authority's overlay
installs and served-publish supersessions run beside it — all three are
counts of ONE event per assembled block. The co-writers ship ~2× that many
extents (one per block per phase, ≈ 4 files × 80 blocks per co-writer:
the first iteration's kernel-split 1 MiB segment, written before the
block's range grant covers it — `write_touches_shared_block` reads the
block as not solely covered), chunked at the wire cap into ≈ 1.25 frames
each (`extent_served`).

The chain, per block `b` of a co-writer's fpp file, first iteration of
the phase (`src/fuse_client.rs`, `src/routing.rs`, `src/meta_ship/publish.rs`):

1. the co-writer's first 1 MiB segment of `b` ships as an extent
   (`write_shared_striped` → `extent_ship::ship_extent`, retained); its
   other segments take the normal path and publish `b → D` (the
   co-writer's mint, tracked `Some(1)` locally);
2. the authority's `assemble_shipped_extent` writes the slice by proxy
   (`write_file_staged` → the W2 extent park) and its fold —
   `fold_extent_block` → `fold_upload_block` — mints an authority block,
   seeds from the head (`D`), applies the slice and runs
   `merge_block_mappings(b → A'')` on the AUTHORITY's router: the
   range-episode compose (`save_metadata_to_backend_ext` →
   `compose_episode_save`) recomputes, `local_released = [D]`, and
   `free_recomputed_releases(ino, [D])` frees `D` through the authority's
   ladder (`m0 free_recomputed_blocks` — the same gauge the served arm
   moves; nothing splits the two);
3. **no message reaches the co-writer.** The fold is the authority's own
   publish — no call of the co-writer's produced it, so no served reply
   carries `D`; the `WriteExtent` ack carries only `covering_version`;
   the co-writer's extent release hook (`install_cowriter_extent_hooks`)
   answers coverage with `discard_layout_cache(ino)`, dropping the RAM
   binding `D` with no hygiene. `D`'s local entry lingers;
4. `D` rides grace (~2 s) → the authority's free list → the lane harvest
   → the co-writer's funnel; `claim_block_idx` finds the entry:
   `CLAIM ANOMALY … count=Some(1)`. Once per assembled block.

The `overlay_superseded_by_served_publish` count beside it is the same
block's OTHER face: the co-writer's own publish of `D` (its normal
segments' settle) lands while the authority's overlay record for `b` is
open, the finding-51 screen supersedes the record (the authority's own
lane-0 dest — no co-writer entry involved), and the authority's fold
re-applies the parked slice onto `D`. Both counters are "one per
assembled block"; only the fold's local recompute frees a co-writer
block.

**Fractions against the ~1,100 per phase**, every candidate the task
named:

| class | count on 1D B1 | verdict |
|---|---|---|
| (a) RAM-only overlay lifetimes the recompute frees but the epoch never parked | handled at the merge's own reply (`retire_displaced_locally` on the merge's `displaced`, the leak fix's arm); `cowriter.unpublished_recycles` 10 Σ8, `free_ship_own_lane_untracked` 0 | not the population |
| (b) the served-displacement sink / the dead-binding probe | `overlay_superseded_dead_old_binding` 0; the sink supersedes the AUTHORITY's own dest records (lane 0), freeing no co-writer block — but its **sibling**, the assembler fold's LOCAL recompute, is the population: `fold_passes` 1,183 vs 1,108 anomalies (**≈ 100 %**, −7 % … +2 % on six fpp phases) | **THE population** |
| (c) an epoch closed between publish and reply | the close removes the epoch before its own save; the reply's retire finds nothing and the close's `retire_displaced_locally(deferred)` keeps its arm; keys parked after the save's snapshot are not in that reply's `freed` and stay parked | no gap, 0 |
| (d) an entry re-created after the retire | the only inserters are the claim, `allocate_specific_block`, `recover_block`, `seed_shipped_free_reference`, `fsck_set_refcount` (grep); on a co-writer only the claim is reachable | 0 |
| (e) a reply site the schema-15 retire did not reach | all three shipped arms (`SetLayoutAndSize`, `MergeLayoutAndSize`, `MigrateBlockMap`) call `retire_recomputed_parked`; the fpp files (80 blocks) never engage kvmap; the fpp phases are the same three arms A1/A2 use | 0 |
| the A2 trickle (73 / 67 / 58 Σ8) and the fpp remainder (±7 %) | unattributed — same magnitude on the C rows, untouched by schema 15 | not claimed |

The in-process reproduction (§4) is the offset-level proof the row's
INFO logs cannot give: block 2 of an 8-block file → the co-writer's mint
at lane idx 15, `Some(1)`; the authority assembles a 64 KiB slice and
`flush_shipped_extents` folds + publishes; the authority lists idx 15
free at population 0 with `free_recomputed_blocks` +1; the co-writer's
`refcount(15 × bs)` still reads `Some(1)`; the harvest hands 15 back and
the claim trips — `block_claim_anomalies` +1, the fleet's line, red on
`0bd03455`.

## 3. Why the fix is a notice ledger on the reply frame, and why it is not option (c)

The party that freed `D` is the authority; the party holding the stale
tracking is the lane's owner; the design's only authority → co-writer
channel is a reply to something the co-writer sent (pull-based
everything). The notice therefore rides **every reply frame** the
authority sends that client (`PublishReplyFrame::lane_frees`), drained
from a per-client ledger fed by the authority's own frees of the
client's lane blocks — and the frame rate on a rewriting co-writer
(~215 frames/s per co-writer in the fpp phases) makes the delivery ms,
against a ~2 s grace hold. The ordering that makes the harvest race
impossible: the notice is queued **before** the ladder that frees the
block runs (`note_lane_frees` at the top of `free_recomputed_releases`,
and in the `FreeBlocks` serve before its executor), so a reply that hands
the offset back through a harvest was built after the notice was queued
and drains it, and the co-writer applies a frame's notices before any of
its outcomes reaches a caller (`ship_frame`) — the grant is adopted after
the stale entry is gone. Nothing marks a harvested block as anyone's
former lifetime and the claim never learns anything special: the
tripwire keeps its full meaning (a notice lost, a client id that does
not match its lane, a new free arm — each would still trip).

The inverse (a notice for a lifetime the co-writer already re-minted —
the notice's reply reordered behind the grant's across the ship depth's
sessions) is a **grant sequence**, not a stamp the authority does not
have (`witness_served_binding` publishes the word, never the foreign
stamp): `LaneFreeGrant.grant_seq` is the authority's per-client count of
served grants, bumped strictly after the harvest executor hands the
blocks out and cached with the dedup outcome (a replay re-answers the
same sequence); every adopted block carries it
(`adopt_lane_free_grant_at`); a notice carries the client's count at
queue time (`after_grants`). Any grant that can hand an offset back was
served after the offset's notice was queued, so it carries a higher
sequence — a notice below the offset's tag names the previous lifetime
and touches nothing (`cowriter.lane_free_notices_reminted`); at or above
it, this lifetime, released. The co-writer's act is the
`retire_displaced_locally` decrement, never the incarnation word: the
notice precedes the ladder's verdict, and a NonTerminal block's word
must stay.

Wire: `WireLaneFree { vol_tag, block_idx, after_grants }` — ≤ 27 B per
notice, ≈ 1,150 notices per fpp phase Σ8; the ledger dedups on the block
(a later release of a re-minted lifetime supersedes an earlier undrained
one — safe because the grant that re-minted it was a reply build, which
drained the earlier notice first), so a client's backlog is bounded by
its lane's blocks, never by time. `PUBLISH_SCHEMA` 15 → 16 (KD-7): a
15-speaker would read the notices as absent and keep the lineage; the
mismatch refuses loud at the first frame in both directions.

The served requester is skipped (`note_lane_frees(Some(client), …)`) —
its own blocks travel on the per-call `freed` set (schema 15), so a
notice would be a second delivery; the ledger takes the requester's
publishes' displacement of OTHER co-writers' mints (a peer's block at an
index this client rewrote) and every authority-local publish's
displacement (`None`). The lane → client map is the era's
`LaneAssignment` (`custody_owner().lane_assignment()`), the same map the
lease grant carries.

## 4. Contracts (this side)

`tests/mw_authority_recycled_binding_tests.rs` §4 — the suite whose rig
has the PRODUCTION assembler on an authority fs beside a co-writer fs
over one meta backend (the one-process venue: the co-writer's halves
stand down while the authority acts and re-arm after, because the
posture latch, the ownership map and the client installs are
process-global):

| contract | shape |
|---|---|
| `an_authority_fold_of_shipped_slices_reaches_the_co_writers_tracking_on_its_next_reply` | the co-writer owns whole blocks 2 and 3 (`Some(1)` each); the authority assembles a 64 KiB slice of each and `flush_shipped_extents` folds + publishes (`free_recomputed_blocks` +2, both free-listed at population 0, both entries still `Some(1)` — the fleet's state); the co-writer's next publish-plane round trip of ANY kind (a whole-block write of block 5 + fsync) releases exactly those two entries (`cowriter.lane_free_notices` +2); the harvest re-claims both with `block_claim_anomalies` +0; `block_untracked_free_refusals` +0 — **RED on 0bd03455** (both entries linger, both re-claims trip) |
| `a_harvest_that_hands_back_an_authority_freed_block_carries_its_notice_first` | the same fold, then NO round trip before the harvest: the harvest frame itself carries the notice ahead of its grant — `block_claim_anomalies` +0, `cowriter.lane_free_notices` +1, the re-mint tracked exactly once — **RED on 0bd03455** (the claim trips) |
| `a_lane_free_notice_below_the_offsets_grant_sequence_touches_nothing` | the inverse, on the offsets the harvest tagged: a notice with `after_grants = tag − 1` leaves the live entry and its tag untouched (`lane_free_notices_reminted` +1); one at `tag` releases it and prunes the tag; a repeat is a no-op on both gauges — the mechanism's own pin, added with the fix |

The schema pin in `mw_cowriter_free_leak_tests` moves to 16; the fuzz
target's constructive mirror and `decoder_property_tests` grow the
frame's notices and the grant's sequence; six test-side harvest-sink
mocks carry `grant_seq: 0` (an untagged adoption never shields a
re-mint).

Gate lines run on `9ef1af9e`: `cargo fmt --check` (root + fuzz) clean;
`cargo clippy --all-targets --all-features -- -D warnings` exit 0;
`cargo clippy --all-targets -- -D warnings` exit 0;
`RUSTDOCFLAGS="-D warnings" cargo doc --no-deps` exit 0; `cd fuzz &&
cargo check` exit 0. Suites: §5.

## 5. Suites (`--all-features -- --test-threads=1`, on `9ef1af9e`)

`mw_authority_recycled_binding_tests` **7/7** (4 + the three §4
contracts) · `mw_cowriter_free_leak_tests` 12/12 · `mw_cowriter_free_tests`
50/50 · `mw_cowriter_lane_tests` 26/26 · `mw_data_alloc_lane_tests` 32/32 ·
`cowriter_lane_placement_tests` 12/12 · `cowriter_enospc_wedge_tests`
10/10 · `dlm_cowriter_tests` 18/18 · `dlm_multi_writer_tests` 16/16 ·
`publish_plane_batching_tests` 8/8 · `cluster_wire_tests` 23/23 (+1
ignored) · `rewrite_shadow_supply_close_tests` 10/10 ·
`free_grace_lane_visible_tests` 10/10 · `derivation_sweep_tests` 47/47 ·
`env_knob_convention_tests` 21/21 · `audit_instruments_tests` 26/26 ·
`decoder_property_tests` 29/29 · `kvmap_mw_hazard_tests` 7/7 ·
`kvmap_crossing_tests` 9/9 · `mw_widthn_refs_tests` 15/15 ·
`mw_authority_assembler_tests` 21/21 · `mw_arbiter_fold_tests` 3/3 ·
`mw_publish_era_gate_tests` 5/5 · `rewrite_shadow_tests` 8/8 ·
`rebind_starvation_tests` 5/5 · `pv_shipped_free_ledger_tests` 2/2 ·
`pv_owner_verb_tests` 21/21 · `pv_partial_open_tests` 22/22 ·
`dlm_range_custody_tests` 41/41 · `durable_block_refs_tests` 17/17 ·
`mw_ranged_lease_ladder_tests` 15/15 · `overlay_overwrite_tests` 32/32 ·
`write_through_tests` 26/26 · `mw_fleet_jobs_tests` 9/9 ·
`fsync_writeback_tail_loss_tests` 3/3. Bench smoke `cargo bench --bench
write_path_bench -- --test` (120 groups, the publish/free frame codecs
included) exit 0. The three §4 contracts 10/10 consecutive. Not run here
(the parent's): `task check`, the root/fleet rigs, `task check:fuse3`,
`task audit`.

## 6. What the previous note's mechanism is, restated

The schema-15 parked-key retire (`retire_recomputed_parked`) is correct
and necessary — a parked rewrite-epoch key the SERVED recompute frees
mid-epoch is a real lineage, its in-process contract reproduced the
fleet's line, and on the D row it retired 5.5–8.9 k parked keys per
phase at the reply — and it did not move the fleet gauge, because the
fleet's population is freed by a publish the co-writer never issued.
The predecessor note's §2 elimination ("the only remaining path that
free-lists a co-writer-tracked offset on the authority is the recompute
arm") was right about the ARM and wrong about the CALLER: it read
`free_recomputed_blocks` as the served arm's gauge, and the gauge counts
the authority-local arm's frees too. Its §7 fleet verdict is restated
in that note.

## 7. What is NOT claimed

* **The fleet row.** The population is proven by the row arithmetic (three
  rows, six fpp phases, three authority-side counters within ±7 % of the
  anomaly count) and the in-process reproduction; the acceptance pair
  (`block_claim_anomalies` 0 on every co-writer across all four phases,
  `cowriter.lane_free_notices` ≈ the authority's fpp `fold_passes`,
  `lane_free_notices_reminted` ≈ 0, the s11 gate unchanged) runs on
  squeeze-test, D-C-C-D, the parent's.
* **The A2 trickle** (58–73 Σ8) and the fpp remainder (±7 % of
  `fold_passes`): unattributed here. The notice ledger also covers a
  peer's `FreeBlocks` displacing this client's mint (the shared-file
  handoff shape), which may or may not be the trickle — the row says.
* **Why a whole-block sequential writer ships an extent at all** — the
  first-iteration segment written before the block's range grant covers
  it (`write_touches_shared_block`). That is the S11 range-custody
  grant's coverage rule, not a free-path fact; its cost (one authority
  fold + one displaced co-writer block per block per phase, ≈ 2× the
  publish traffic on those blocks) is a separate board item.
* **The lane ENOSPC refusals** (the parent's other residue) — unchanged
  by design here.
