# 2026-09-07 — lane-aware placement on a laned co-writer: the pick weighed the device, the lane exhausts per volume

| | |
|---|---|
| **Branch** | `fix/cowriter-lane-aware-placement` off `dev` 2a486273 |
| **Commits** | `c6ce59f5` (red contract suite, 18 missing-symbol errors against `dev`) · fix + docs commits (see the branch log) |
| **Evidence** | the per-second fleet samples `/tmp/five/d4/keep-sampler-shakedown/` — `samples/m5*.jsonl` (one JSON line per mount per second), `m*.stats.json`, `m0.log`; produced by `.benchmarks/rigs/2026-09-07-fleet-sampler.py` on the s11-mpiio row (the parent's) |
| **Class** | a write-path placement defect on the multi-writer data plane: thousands of `StorageFull` refusals per 5 s while the refused lane's supply sat reachable on the sibling volume — finding 15's "hold the supply" half seen from the placement side |
| **Fleet repro** | NOT RUN here — the parent owns the fleet row (`tests/mw_fleet.sh` / `run_mw_matrix.sh s11-mpiio` were not invoked in this worktree). The A/B lever for that row is `SQUEEZEFS_COWRITER_LANE_PLACEMENT=0` |

## 1. The finding, read off the samples

Venue: authority m0 + 8 co-writers m50–m57 (`tests/mw_fleet.sh create N=1
--cowriters=8`, `SQZ_MWFLEET_OSS_GB=32`, S11 range custody),
`run_mw_matrix.sh s11-mpiio` (32 ior ranks on the co-writers rewriting ONE
shared 10 GiB file, 4 MiB block-cyclic). TWO data volumes (nvme3n1,
nvme4n1), each 32 GiB = 8,192 blocks, `W = 16` ⇒ each co-writer's lane owns
512 blocks PER VOLUME. Recycled blocks return to the co-writer PER VOLUME:
the authority's grace ring releases a freed block onto the free list of the
volume it lives on, and the co-writer harvests per allocator
(`BlockAllocator::harvest_lane_supply`, keyed by `vol_tag`).

Columns are 5 s deltas of `alloc_lane_{harvests,harvested_blocks,
enospc_refusals}`; `reachable` is `alloc_lane_reachable_blocks` (this
mount's lane, SUMMED over both volumes) at the window's start; `hint` is
`free_grace_lane_supply_hint` (the authority's count of this lane's blocks
on its free lists, summed over both volumes, carried on the renewal grant):

```text
mount  window     reachable  hint     harvests  harvested  ENOSPC
m50    t=95–100   0→35       512→448  +2,264    +73        +2,261
m53    t=45–50    0→63       498→448  +725      +65        +743
m55    t=55–60    63→107     0        +1,785    +128       +1,783   (genuine shortage — hint 0)
m51    t=107–112  255        —        +2,057    +75        +2,053
m53    t=91–96    183        —        +1,197    +303       +1,177
m54    t=91–96    194        —        +1,435    +510       +1,426
m56    t=152–157  110        —        +1,247    +217       +1,242
```

Whole row (≈ 210 s), summed over the eight co-writers: **42,014 harvest
RPCs, 39,846 `StorageFull` refusals, 40,682 blocks harvested** — one RPC per
refusal, and on m51/m53/m54/m56 the refusals fire while **hundreds of blocks
of the refused lane are LOCALLY reachable** (no RPC needed at all) on the
volume the pick did not choose. The refusal text names one volume per line
(`data volume 'nvme4n1' full: 8185 of 8192 blocks allocated — lane 8 of 16
is exhausted while 0 free block(s) belong to lanes this mount does not
own`); both volumes appear over the row. The authority's serve log
(`m0.log`, rate-limited to 832 lines) shows lane-8 serves of 1–3 blocks on
one `vol_tag` beside 64-block (one grain) serves on the other.

## 2. The mechanism (verified in source, then fixed)

The write path did ONE §5.9 placement pick and allocated on that volume
only: `get_active_backend()` (the ArcSwap'd `PlacementTable`, round-robin
inside the 90 %-of-max `health_effective` band, `src/routing.rs`) then
`allocate_block_grace_bounded()` on that allocator; a `StorageFull`
propagated (`alloc_res?`) into the caller's ladder and never tried the
sibling. Fourteen sites paired the two the same way (`src/fuse_client.rs`
write-through upload / overlay install / overwrite-overlay seam /
staging-refusal escalation / fold upload / flush unit; `src/routing.rs`
indirect blob / staged promotion / staged spill / `write_striped` /
parallel upload / rider-fold spill / staged-clone spill / truncate clip;
`src/multi_writer.rs` the authority's served blob mint).

On a single-writer mount that is correct: every volume's whole free list is
that mount's, so "full" is full and the device fill IS the right weight
(VL4b, KD-16). On a LANE-PARTITIONED co-writer three things compose:

1. **the lane's share exhausts and refills PER VOLUME** (the partition is
   declared per allocator; frees are lane-blind per volume; the harvest asks
   one `vol_tag`), independently of the device;
2. **the device-fill weight says nothing about this mount's lane on that
   volume** — and on a dense-full co-writer view (`get_used_blocks` =
   dense frontier − free list, the frontier at the cap) it reads ≈ 0 for
   BOTH volumes: `(1 − 8185/8192) × 1000` truncates to 0, so the cutoff is
   0, every volume is in the band, and the composition is noise;
3. **the band is stale for the health worker's 5 s cadence** while the
   lane supply churns at hundreds of blocks per second: a volume drained to
   0 keeps its band slot until the next refresh, and a volume a harvest just
   refilled stays out of it.

Each pick landing on the lane-exhausted volume cost a wasted harvest RPC on
the empty volume (`allocate_block_inner`'s ENOSPC arm), a refused write, and
— when the authority reported a held ring — a bounded park of up to 1 s
(`pressure_park_wall_ms`), with the sibling's supply untouched.

## 3. The fix

**Lane-governed placement** (`BlockAllocator::lane_placement_governed`):
an allocator is lane-governed iff its partition is engaged AND the lane free
harvest is wired — only the co-writer engagement wires it
(`alloc_lane_grant::engage_allocator_lane`) — and
`SQUEEZEFS_COWRITER_LANE_PLACEMENT` is on. Every single-writer and
authority allocator answers `false` in one `OnceLock` probe.

1. **The weight** (refresh time, `refresh_placement_table`): a lane-governed
   row weighs `lane_reachable_blocks × 1000 ÷ lane_share_blocks`
   (`routing::lane_placement_weight`; share = Σ owned lanes'
   `lane_capacity_blocks`) on `health_effective`'s 0..1000 scale, no
   balance penalty (the set mean is a device term too). An exhausted lane
   weighs 0 and leaves the band; the 90 %-band + round-robin among the rest
   stays. Ungoverned rows keep `health_effective(device_health, fill,
   set_mean)` byte-identically (pinned).
2. **The pick** (`PlacementTable::pick`, three passes, still table-only):
   pass 1 = the shipped round-robin over the band, skipping a lane-governed
   row whose `lane_reachable_blocks() == 0` (the DRAIN event); pass 2 —
   entered only when pass 1 found nothing — the first eligible healthy
   lane-governed row with reachable supply, in or out of the band (the
   REFILL event); pass 3 = the shipped round-robin verbatim (a dry set still
   places, so the picked volume's ENOSPC harvest and the park run there).
   The chosen trigger is re-weighting inside the pick from the allocator's
   maintained O(1) counters — no generation word, no rebuild.
3. **Failover before the park** (`BackendRouter::allocate_placed_block`,
   the ONE pick+allocate act every fresh-block site now calls): on an
   ungoverned pick it is the shipped pair instruction for instruction (the
   pick, then `allocate_block_grace_bounded`). On a lane-governed pick a
   `StorageFull` tries the remaining eligible healthy volumes IN THE SAME
   ATTEMPT — stocked lanes first, then dry ones (whose own ENOSPC harvest
   may reach supply the authority holds for them on that volume) — and only
   when every volume refused does it park, through the same
   `park_for_reclaimable_supply` slice/wall policy as the single-volume form
   (factored out of `allocate_block_grace_bounded`, which now calls it),
   with the SET's reclaimable verdict, retrying the whole set each slice.
4. **The harvest targets where the supply is.** The renewal grant's hint
   is SUMMED over volumes and cannot name the one holding the supply, and a
   peer's rewrites of this lane's blocks put supply on a list no owed ledger
   here knows about. So the pushed refill's decision is per volume
   (`free_grace::lane_push_wants_harvest_on_volume(hint, owed,
   reachable)`): owed ⇒ ask (the shipped rule, `lane_push_wants_harvest`
   untouched and still pinned); DRY (`reachable == 0`) with a nonzero hint
   ⇒ ask even when owed nothing — one RPC that either refills the volume or
   proves the supply is its sibling's, which the lane-aware placement
   carries meanwhile; stocked and owed nothing ⇒ never ask (no wasted RTT).
   The ahead refill (`should_harvest_ahead`) treats the hint as evidence
   beside the owed ledger (`owed > 0 || hint > 0`, then the watermark test
   as before). Composed with (3), "the authority holds N blocks of my lane"
   implies "my next allocation can reach them": every volume's ENOSPC
   harvest runs in the same attempt, and the off-path refill asks every dry
   or low volume within one renewal. The exact form — a per-volume hint
   vector on the grant (`CLUSTER_WIRE_SCHEMA` bump) — is deferred: `Grant`
   is `Copy` and travels through ~40 sites; the dry-volume rule reaches the
   same volume at ≤ 1 RPC per dry volume per renewal.
5. **Lever + ledger.** `SQUEEZEFS_COWRITER_LANE_PLACEMENT` (bool, default
   on; `0` = the shipped device-fill pick + no failover — the fleet A/B
   lever; registry entry + `docs/operations.md` row).
   `backend_placement_lane_failovers` (allocations that landed on a volume
   other than the pick) and `backend_placement_lane_exhausted_picks` (picks
   that landed on a lane-exhausted volume while a sibling had lane supply —
   ≈ 0 by construction of the pick; growth = predicate rot), both on the
   stats inode's `placement` object, 0 on every other posture.

### Per-pick cost

`PlacementTable::pick` is still one ArcSwap load + one relaxed
`fetch_add` on the round-robin cursor + an O(#band) scan with zero locks
and zero syscalls. Per candidate examined it adds `lane_supply_admits`:

* **ungoverned row** (every single-writer and authority volume): one
  `OnceLock::get` acquire load (`lanes.get()` → `None`) — the first
  candidate is taken exactly as before, so the steady-state pick pays ONE
  extra load;
* **lane-governed row**: `lanes.get()` + `harvest.get()` (two `OnceLock`
  loads) + the lever latch (one relaxed load) + `lane_reachable_blocks()`
  (`capacity_blocks`, `highest_block`, the owned mask, `lane_owned` — four
  atomic loads) ≈ 7 loads on counters the allocator already maintains; pass
  2 is O(#backends) and runs only when every banded lane is dry.

`allocate_placed_block` adds nothing on the ungoverned path. On a governed
pick it clones the sibling rows once (O(#backends), Arcs) before the first
`allocate_block` — a cold-path cost paid per fresh-block allocation on a
co-writer, not per write byte — and the failover/park loop runs only after
a `StorageFull`.

## 4. Contracts (`tests/cowriter_lane_placement_tests.rs`, 9 tests, all green; red = 18 missing-symbol errors against `dev`)

The rig: a two-volume `BackendRouter` whose allocators are one co-writer's
laned allocators (lane 1 of 2, 64-block devices ⇒ 32-block shares), each
wired to a fake authority holding THAT volume's lane list (the
`cowriter_enospc_wedge_tests` sink shape, with supply).

1. `supply_on_one_volume_places_every_allocation_there` — A exhausted, B
   holds 8 adopted lane blocks: weights 0 / 250 (`8 × 1000 ÷ 32`), band
   `{B}`, 8 placed allocations all on B, no refusal, no park, zero RPCs on
   either authority, no rebuild, both gauges flat.
2. `an_allocation_draining_the_last_lane_block_moves_the_pick_before_the_refresh`
   — both in the band, A drained outside placement, the table stale: 8
   placed allocations all on B, no RPC on A, no rebuild.
3. `a_harvest_refilling_an_out_of_band_volume_is_reachable_before_the_refresh`
   — A out of band, B drained, A refilled by adoption: the next allocations
   land on A with no rebuild and no RPC.
4. `both_lanes_exhausted_and_nothing_held_refuses_at_once_after_asking_both`
   — no park; each authority asked exactly once (the pick's ENOSPC harvest,
   then the failover's); a failed failover is not a failover.
5. `both_lanes_exhausted_with_a_held_ring_parks_then_refuses_at_the_wall` —
   the field's held-ring shape: parks, re-runs the harvest on BOTH volumes
   each slice, refuses `StorageFull` at the wall (≤ wall + 2 s).
6. `a_single_writer_router_keeps_the_device_fill_weights_and_never_fails_over`
   — weights equal `health_effective(free-fraction × 1000, fill, mean)` to
   the point; a full set refuses with no failover and no lane gauge moving.
7. `the_lever_off_restores_the_shipped_pick_and_no_failover` — contract 2's
   shape under `=0`: the stale band lands on the drained volume, each such
   pick pays its RPC and refuses, no failover counted.
8. `the_allocation_harvests_the_volume_whose_authority_holds_the_supply` —
   both dry locally, the authority holds 8 on B's list: the allocation
   lands on B via B's harvest; a dry pick on A is exactly one wasted RPC and
   one counted failover, a pick on B none.
9. `the_pushed_refill_asks_a_dry_volume_on_a_hint_and_never_a_stocked_unowed_one`
   — the per-volume decision pure and on real allocators (a dry unowed
   volume harvests 8 on the hint; a stocked unowed one sends no RPC); the
   ahead decision fires on `hint > 0 ∧ reachable < watermark` with the owed
   ledger at 0, and stays dark with no hint and nothing owed.

`tests/placement_tests.rs::stats_inode_carries_the_placement_family` also
pins both gauges exporting as 0 on a single-writer mount.

Suites run green in this worktree (`--test-threads=1`):
`cowriter_lane_placement_tests` 9 · `placement_tests` 12 ·
`cowriter_enospc_wedge_tests` 9 · `free_grace_lane_visible_tests` 9 ·
`env_knob_convention_tests` 21 · `mw_cowriter_lane_tests` 26 ·
`mw_data_alloc_lane_tests` 23 · `mw_cowriter_free_tests` 50 ·
`mw_cowriter_free_leak_tests` 9 · `dlm_multi_writer_tests` 16 ·
`data_path_correctness_tests` 27 · `device_overlay_tests` 10 ·
`extent_overlay_tests` 14 · `meta_lock_free_hoist_tests` 2 ·
`overlay_ack_early_tests` 14 · `phantom_backend0_tests` 14 ·
`reader_free_grace_tests` 61 · `rebind_starvation_tests` 5 ·
`rw5a_never_lossy_tests` 10 · `statfs_live_accounting_tests` 1 ·
`volume_drain_tests` 17 · `volume_lifecycle_tests` 9 ·
`backend_health_probe_tests` 2; `cargo fmt --check`, `cargo clippy
--all-targets --all-features -- -D warnings`, `cargo clippy --all-targets
-- -D warnings`, `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps` clean.
The full `task check` was NOT run here (the parent's gate).

## 5. What is NOT claimed

* **No fleet number.** The s11 row's verdict — refusals per 5 s, harvest
  RPCs per refusal, `bound_age`, aggregate ingest — under the fix vs
  `SQUEEZEFS_COWRITER_LANE_PLACEMENT=0` is the parent's row to run (A-B-B-A,
  the same 1.2.1 profile both legs, substrate stated). In-process the
  contracts prove the mechanism, not the rate.
* **Finding 15 itself is unchanged.** When the lane's supply is genuinely
  gone on EVERY volume (m55 t=55: hint 0), the refusal is correct and this
  change only makes it honest sooner (one attempt asks every volume, then
  the same bounded park). The recycle loop's hold time is the hold-time /
  ladder campaigns' business.
* **The authority's own placement is untouched by decision.** An authority
  with enrolled co-writers is laned too (lane 0, `W > 1`) and its lane
  exhausts per volume the same way, but it wires no harvest sink (its lane-0
  supply is its own free list) and the deliverable pins its weights
  byte-identical. If a fleet row ever shows the authority refusing while
  its sibling volume holds lane-0 supply, the governed predicate is the one
  line to widen.
* **The per-volume hint on the grant is deferred** (§3 item 4) — the
  dry-volume rule reaches the same volume at ≤ 1 RPC per dry volume per
  renewal; the exact vector needs a `Grant` shape change through ~40 sites
  and a `CLUSTER_WIRE_SCHEMA` bump.
* **The `Allocate` pipeline phase now spans the pick** (one ArcSwap load) —
  invisible at the phase's µs resolution, stated so a phase-ledger diff is
  not misread.
