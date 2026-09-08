# 2026-09-07 — the co-writers' `CLAIM ANOMALY` residue is the recompute arm's hygiene window; the served reply now names what it freed

| | |
|---|---|
| **Branch** | `fix/cowriter-claim-anomaly-lineage` off `dev` d603e7ae |
| **Commits** | `8a794cdc` (red contracts + the `cowriter.recomputed_retires` instrument) · `d90fcaab` (the fix: publish schema 14 → 15, `retire_recomputed_parked`) · the docs commit |
| **Evidence** | `.benchmarks/rows-f15-day2-20260907/box-seq2-{1C,4C}/` — the per-phase snapshots `m{0,50,53}_p{0..4}.json.gz` (p0 before the probe, p1..p4 after A1/B1/B2/A2) and the 1 Hz `samples/*.jsonl`; the row's `m50.log` / `m0.log` (kept off-tree); the parent note `2026-09-07-f15-b1-squeeze-test-seq2.md` §4 and the lineage note `2026-09-06-cowriter-free-residual-lineage.md` §2/§6–§7 |
| **Class** | data-path bookkeeping — a co-writer's LOCAL view of a block the authority freed on its behalf lingered until the epoch close; the fleet's free supply came back faster than the close and every re-claim tripped a must-stay-0 tripwire |
| **Fleet row** | **OWED (parent, squeeze-test)** — see §7 |

## 1. What the residue is (the numbers, before any code)

The passing day-2 tip (`886d4e31`) passes the s11-mpiio matrix and logs
`block_claim_anomalies` 1,156–1,402 per file-per-proc phase (Σ 8
co-writers) — `claim_block_idx`'s "the offset was free-listed while a
tracked owner existed" tripwire (`src/block_allocator.rs`). Per co-writer,
per phase, on both C rows (p1..p4 deltas of the snapshots):

| row / mount | A1 (shared) | B1 (fpp) | B2 (fpp) | A2 (shared) |
|---|---|---|---|---|
| 1C m50 | **anom +0**, free_shipped +311, own-lane-untracked ships +35 | **anom +158**, free_shipped **+0**, ships +0 | **anom +129**, free_shipped **+0** | anom +10, free_shipped +444 |
| 1C m53 | +0, +317, +33 | **+133**, **+0**, +0 | **+201**, **+0** | +7, +394 |
| 4C m50 | +0, +336, +35 | **+163**, **+0**, +0 | **+162**, **+0** | +4, +407 |
| 4C m53 | +0, +288, +36 | **+178**, **+0**, +0 | **+154**, **+0** | +16, +435 |
| 1C m0 (authority) | free_recomputed +52,565, free_served +2,198, refused +282 | free_recomputed **+75,497**, free_served **+0**, refused **+0** | **+78,180**, **+0**, **+0** | +55,279, +2,976, +296 |
| 4C m0 | +52,561, +2,266, +291 | **+77,295**, **+0**, **+0** | **+78,715**, **+0**, **+0** | +55,230, +2,976, +303 |

Every anomalous offset in `m50.log` is in m50's OWN lane (297/297 lane 2
of 16, both data volumes), and 166 offsets fired once, 52 twice, 9 three
times (the repeats histogram — an offset re-claimed once per cycle while
its entry lingered). The 1 Hz samples put every anomaly inside the two fpp
windows (t ≈ 80–290 s) plus a trickle at the A2 boundary; `rewrite_shadow_
fence_drops` is 0 on every co-writer for the whole row, so the 2026-09-06
fenced-close lineage is closed and this is something else.

## 2. The lineage, proven by elimination on the fleet and exactly in-process

A co-writer's allocator can hand out a free-listed offset from exactly
two feeders: the lane harvest (`adopt_lane_free_grant` — a block the
AUTHORITY free-listed and served back) and the never-published recycle
arm (`abandon_unpublished_offset` — which REMOVES the refcount entry
before the free-list insert, so it cannot trip the claim). So a claim that
finds an entry lingering means: **the authority free-listed an offset this
co-writer still tracked.** The authority free-lists a co-writer's block on
three arms:

1. the **explicit-ship arm** (`FreeBlocks` → `Freed`): the co-writer
   retires its entry at the reply (`retire_shipped_free_tracking`), one
   RTT after the free, while the offset still sits in the grace ring — it
   cannot be harvested before the retire. On the fleet this arm shipped
   **zero** blocks during either fpp phase (`free_shipped_blocks`,
   `free_served_blocks`, `free_refused_blocks`, `cowriter.free_ship_own_
   lane_untracked` all +0 on both rows), and `free_ship_failures` is 0
   for the row;
2. the **explicit arm's refusals** (`Refused` touches nothing locally):
   +0 in the fpp phases (row-wide 578/594, all in the shared phases, and
   the 2026-09-06 lineage note already showed refusals and anomalies do
   not co-occur);
3. the **recompute arm**: a served publish whose owner replaced the
   caller's frame with its own head→composed diff and freed the true
   displaced set through its own ladder post-commit
   (`free_recomputed_blocks` +75–79 k per fpp phase, ≈ 98 % of every
   displaced co-writer block on the row). The reply said `recomputed` —
   the co-writer stands its frame-derived frees DOWN — but never WHICH
   offsets were freed. For a key parked in an open rewrite epoch
   (`epoch.displaced`, the round-before mint `A` that the epoch's `B`
   displaced) the local hygiene was therefore deferred to the epoch
   CLOSE (`close_rewrite_epoch_counted` → `retire_displaced_locally`).

Arm 3 is the only one left standing in the fpp phases, and its timing is
the anomaly: a write-through merge / flush-leg / overlay publish
(`write_through_blocks` +2.0–2.4 k, `overlay_publishes` +260 per fpp
phase per co-writer) ships the WHOLE dirty RAM map mid-epoch; the
authority's custody-scoped compose frees the parked `A` keys into its
grace ring (`free_grace_hold_ms` ≈ 2 s on the row); the lane harvest
(`harvest_served_blocks` ≈ `free_recomputed_blocks` — every freed block
comes straight back) hands `A` to the same co-writer, whose lane is short
(`alloc_lane_enospc_refusals` growing all phase), and `claim_block_idx`
finds `A`'s entry from the previous lifetime. The close fires later
(`rewrite_shadow_swaps` ≈ 1 per second per file) and — since the leak
fix's dead-lifetime guard — skips the re-minted key.

**Fractions.** The recompute arm explains 100 % of the fpp anomalies (the
other feeders moved 0 — a fleet-wide `+0` on four gauges across two rows,
not a statistical estimate); the anomaly RATE is the fraction of
recompute-freed blocks whose grace → harvest → claim loop completes
before the covering epoch closes: 158 / 9,437 (m50's share of B1's
75,497) ≈ 1.7 % — equivalently 158 / 9,812 free-list claims ≈ 1.6 %. The
never-published recycle arm is structurally excluded (entry removed
first; 0–3 recycles per phase anyway). The A2 trickle (4–16 per
co-writer) is the same arm on the shared file, where fewer own-lane keys
park per epoch. The supply-coupled epoch close's 3× cut (the parent
note) fits: it closes epochs sooner, shrinking the window.

**The exact trace, in-process** (`tests/mw_cowriter_free_leak_tests.rs`
§1c, `a_covering_publish_mid_epoch_retires_the_parked_keys_the_authority_
freed`, red on d603e7ae): the co-writer mints blocks 2..6 of an 8-block
shared file (round 0, fsynced — block 2 landed on lane block idx **15**),
rewrites them (round 1 — the epoch parks the four round-0 keys, refcount
`Some(1)` each), then the writeback path's `persist_dirty_layout_if_needed`
publishes the dirty map WITHOUT closing the epoch. On d603e7ae after that
reply: the authority lists idx 15 free at population 0
(`free_recomputed_blocks` +4), the epoch is still open, and the co-writer's
`refcount(15 × bs)` still reads `Some(1)` — "block 2: the co-writer still
tracks the mint 15 the authority freed" is the red run's first assertion.
Draining the lane's fresh supply harvests the four offsets back and
`allocate_block` re-claims them: `block_claim_anomalies` +4 — the fleet's
line, one per offset.

The row's own logs cannot show this trace per offset: the co-writer's
`allocate_block: offset … (freelist)` and the recompute retire lines are
DEBUG, and the authority's harvest line carries a count, not offsets. The
in-process contract is the offset-level proof; the fleet's contribution
is the elimination above.

## 3. What (b) — a local before/after diff — would have covered, and why it was not chosen

The co-writer already knows its parked keys; retiring ALL of them at any
covering reply would close the fpp shape (every parked key there is an
own-lane predecessor the authority does free). It cannot know which the
authority actually FREED: a claim the compose dropped (out of custody,
a dead incarnation, `retain_live_bindings`) keeps its binding live
durably, a clone sibling answers `NonTerminal`, a foreign-lane
predecessor is a peer's. Retiring those would put the local view wrong
in the OTHER direction (an entry dropped for a block still durably
referenced — silent, and the W1/anomaly instruments would read it as
healthy). On the s11 shared-file phases the range-custody compose drops
claims routinely; on fpp it drops none — so (b) alone would have covered
≈ the fpp ~1 % and guessed on the shared phases. The reply already
computes the exact set; carrying it costs ≤ 18 B per freed block, bounded
by the frame's own displaced set.

## 4. The fix

**Wire (`src/meta_ship/publish.rs`, `PUBLISH_SCHEMA` 14 → 15, KD-7):**
`PublishReply::PutDone { recomputed, freed }`, `DeltaUsed { used, version,
recomputed, freed }` and `MapMigrated { …, recomputed, freed, gen }` (the
schema-12 `released: u64` count became the list) carry
`Vec<WireFreedBlock { vol_tag, block_idx }>` — the blocks the owner's
recompute ladder (`free_recomputed_releases`, now returning them) answered
**`Freed`**, i.e. exactly the ones that entered the owner's free supply.
`NonTerminal` / `Refused` blocks and a missing-executor / failed ladder
travel nothing (nothing was free-listed). A 14-speaker is refused at its
first frame with `PUBLISH_SCHEMA_MISMATCH` naming both numbers, in both
directions — the wire's standing posture. The set is bounded by the served
publish's displaced set (≤ its claimed takes + its RAM-only lifetimes), so
a reply never outgrows the request that produced it.

**Co-writer (`DataRouter::retire_recomputed_parked`, `src/routing.rs`;
`cowriter::retire_recomputed_parked_key`):** every shipped layout publish
consumes its reply inside `save_metadata_to_backend_body` — the three arms
(`set_layout_and_size` → `OwnerVerdict`, `merge_layout_and_size` →
`(used, version, OwnerVerdict, local_released)`, `migrate_block_map` →
`(outcome, freed)`) now hand the freed set to the retire, which walks the
ino's OPEN epoch's parked keys under the caller's held `INODE_META_LOCKS`
(the lock every shadow record parks under): a parked key whose
`(vol_tag, offset ÷ chunk)` is in the set leaves the park and retires
under the `Freed`-verdict discipline verbatim — read tiers purged,
`retire_shipped_free_tracking` (entry gone, incarnation word retired and
republished under a new generation) — counted `cowriter.recomputed_
retires` for entries that existed (a foreign-lane predecessor is untracked
by construction and only its word retires). Keys the owner did not free
stay parked for the close's unchanged arm. The park's `parked_bytes`
follows, so the supply-coupled close's yield plan stops counting blocks
the authority already holds free. The close itself is untouched: it
removes the epoch before its own save, so the body's retire is a no-op
there and `retire_displaced_locally(&deferred)` keeps the close-time arm.

**The inverse (a LIFETIME, never an offset):** a parked key whose stamp is
no longer the offset's live incarnation names a lifetime this mount has
already re-minted — the harvest beat a delayed reply (a lost reply's
bounded resend, a ring released under pressure). Its live entry and word
are touched by nobody; the dead key leaves the park uncounted (its
close-time free would have refused on the dead incarnation anyway). The
same guard `retire_displaced_locally` runs, on the same stamp.

## 5. Contracts and gate (this side)

New / re-pinned (`--all-features -- --test-threads=1`), all in
`tests/mw_cowriter_free_leak_tests.rs` §1c on the §1b two-node harness
(authority + one co-writer driving a real `SqueezefsFilesystem` under range
custody, the write-through pipeline as the epoch's vehicle):

| contract | shape |
|---|---|
| `a_covering_publish_mid_epoch_retires_the_parked_keys_the_authority_freed` | §2's trace end to end: after the covering publish the epoch is STILL open, the four predecessors are free on the authority at population 0, the co-writer tracks none of them, `parked_bytes` dropped by 4 blocks, `cowriter.recomputed_retires` +4, `free_shipped_blocks` +0; the harvest re-claims all four with `block_claim_anomalies` +0; the close converges (fence drops +0, retires still +4); six more rounds; untracked refusals +0, lane ENOSPC +0, `supply_after + live == supply_before`, every live block at population 1 — RED on dev (four `Some(1)`, four anomalies) |
| `a_recompute_verdict_never_touches_a_lifetime_this_mount_re_minted` | the inverse through the product path: one predecessor adopted + re-claimed BEFORE the covering publish (the one staged anomaly asserted exactly), then the reply names it freed — the live entry stays `Some(1)`, the live stamp is unchanged, `recomputed_retires` +3 not +4, the close touches it neither; RED on dev in its gauge |
| `a_peer_speaking_the_previous_publish_schema_is_refused_at_the_first_frame` | `PUBLISH_SCHEMA == 15` pinned; a `PUBLISH_SCHEMA − 1` frame over the real listener answers `PUBLISH_SCHEMA_MISMATCH` naming both numbers — RED on dev (14) |
| `a_genuine_double_release_is_refused_counted_and_named` | extended: the explicit-ship arm's shipped free moves `recomputed_retires` by 0 — the two arms' hygiene stays separately attributable |

Adapted for the wire change (no contract weakened): `kvmap_mw_hazard_tests`
(`PutDone` matched on `recomputed: true, ..`; writer B's train asserts the
released count on `map_recomputed_releases` and an EMPTY freed set —
that rig installs no free executor, so nothing was free-listed, the
leak-safe arm), `kvmap_crossing_tests` / `publish_plane_batching_tests`
(the helpers' new return shapes), `decoder_property_tests` and the fuzz
target's constructive mirror (`arb_freed` / `ArbFreed`).

The three new contracts **10/10 consecutive** (3.7 s per run). Gate lines
run on this branch: `cargo fmt --check` clean (root + fuzz); `cargo clippy
--all-targets --all-features -- -D warnings` exit 0; `cargo clippy
--all-targets -- -D warnings` exit 0; `RUSTDOCFLAGS="-D warnings" cargo doc
--no-deps` exit 0; `cd fuzz && cargo check` exit 0. Suites: §6.

## 6. Suites (`--all-features -- --test-threads=1`, on `d90fcaab`)

`mw_cowriter_free_leak_tests` **12/12** (9 + the three §1c contracts) ·
`mw_cowriter_free_tests` 50/50 · `mw_cowriter_lane_tests` 26/26 ·
`mw_data_alloc_lane_tests` 30/30 · `cowriter_lane_placement_tests` 11/11 ·
`mw_authority_recycled_binding_tests` 4/4 · `dlm_cowriter_tests` 18/18 ·
`dlm_multi_writer_tests` 16/16 · `publish_plane_batching_tests` 8/8 ·
`cluster_wire_tests` 23/23 (+1 ignored) · `rewrite_shadow_supply_close_tests`
8/8 · `derivation_sweep_tests` 47/47 · `env_knob_convention_tests` 21/21 ·
`audit_instruments_tests` 26/26 · `decoder_property_tests` 29/29 ·
`kvmap_mw_hazard_tests` 7/7 · `kvmap_crossing_tests` 9/9 ·
`kvmap_{bounded_save,read,run,sweep,tree,walker}_tests` 11/10/7/9/14/6 ·
`mw_widthn_refs_tests` 15/15 · `mw_authority_assembler_tests` 21/21 ·
`mw_publish_era_gate_tests` 5/5 · `rewrite_shadow_tests` 8/8 ·
`rebind_starvation_tests` 5/5 · `pv_shipped_free_ledger_tests` 2/2 ·
`pv_owner_verb_tests` 21/21 · `pv_partial_open_tests` 22/22 ·
`free_grace_lane_visible_tests` 10/10 · `cowriter_enospc_wedge_tests`
10/10 · `dlm_range_custody_tests` 41/41 · `durable_block_refs_tests` 17/17
· `fsync_writeback_tail_loss_tests` 3/3 · `mw_arbiter_fold_tests` 3/3 ·
`mw_fleet_jobs_tests` 9/9 · `mw_ranged_lease_ladder_tests` 15/15 ·
`overlay_overwrite_tests` 32/32 · `write_through_tests` 26/26. Bench smoke
`cargo bench --bench write_path_bench -- --test` (the publish/free frame
codec groups included) exit 0. Not run here (the parent's): `task check`,
the root/fleet rigs, `task check:fuse3`, `task audit`.

## 7. What is NOT claimed

* **The fleet row.** The mechanism is proven in-process and by the row's
  elimination arithmetic; the acceptance pair (`block_claim_anomalies` 0
  on every co-writer across all four phases, `cowriter.recomputed_retires`
  ≈ the authority's own-lane `free_recomputed_blocks` share, the s11 gate
  unchanged) runs on squeeze-test, A-B-B-A, the parent's — dev-box rows are
  scoping evidence only (2026-09-07 venue ruling). **Run — see §8.**
* **The lane ENOSPC refusals** (7–16 k per fpp phase, the parent's other
  residue) — unchanged by design here; the supply-coupled close's now-honest
  yield estimate may move them and must be measured, not assumed.
* **The close arm's discipline.** `retire_displaced_locally` at the epoch
  close still releases by decrement (never the incarnation word) for keys
  the reply did not free; only keys the reply freed take the full
  `retire_shipped_free_tracking` act, at the reply. Unifying the close arm
  onto the freed set is a follow-on, not a correctness item.
* **A 14-speaker.** A mixed-commit fleet fails loud at the first publish
  frame (KD-7); there is no compatibility shim and none is owed.

## 8. Fleet verdict — RUN 2026-09-07 (row 1D of a D-C-C-D on squeeze-test, D = `0bd03455`): the retire engaged, the gauge did not move

| gauge | 1D (this mechanism) | 2C / 3C (without) |
|---|---|---|
| `cowriter.recomputed_retires` Σ8 over A1/B1/B2/A2 | **21,386 / 5,555 / 7,338 / 8,871** — the mechanism engages exactly as its contract says | 0 |
| `block_claim_anomalies` Σ8 | **0 / 1,108 / 1,117 / 73** | 0 / 1,478 / 1,411 / 67 · 2 / 1,035 / 956 / 58 |
| tripwires, stale refusals, fsck, the s11 gate | 0 / 0 / clean / pass | same |

**Verdict — the parked-key retire is correct and necessary and is NOT
the fleet's population.** §2's elimination was right about the ARM (the
recompute's `free_recomputed_releases`) and wrong about its CALLER: it
read `free_recomputed_blocks` as the SERVED arm's gauge, and that gauge
also counts the authority-local arm — the assembler's fold of a
co-writer's shipped slices, whose local recompute frees the co-writer's
displaced block through a publish the co-writer never issued and no reply
names. On every fpp phase of three rows the anomaly count sits within ±7 %
of the authority's `fold_passes`. That population, its fix (publish
schema 16 — every reply frame carries the authority's lane-free notices,
queued before the ladder runs) and its contracts are
`.benchmarks/2026-09-07-cowriter-claim-anomaly-population.md`. What
schema 15 closed stays closed: a parked key the SERVED recompute frees
mid-epoch reaches the co-writer at that reply, and on the D row 5.5–8.9 k
per phase did.
