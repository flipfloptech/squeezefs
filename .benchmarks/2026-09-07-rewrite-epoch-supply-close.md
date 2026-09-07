# The rewrite epoch's supply-coupled close on co-writer lanes — finding 15's parked-supply term (2026-09-07)

**Verdict (in-process; the fleet row is the parent's):** the lever is
built, contract-pinned and modeled. A laned co-writer's ahead-refill tick
now closes the mount's open rewrite epochs when the lane's reachable
supply sits below the refill watermark — the KD-1.7 early-close made
AHEAD of the `StorageFull`, on the signal the refill already samples —
largest epoch first until the yield covers the deficit, which is also the
per-tick bound. The close is the shipped one-publish swap verbatim, so
every §5.6 crash window holds; nothing new is durable; the write hot path
gains nothing (the tick's reads only). Single-writer and authority mounts
are byte-identical (pinned). On the s11 per-volume closed-loop model the
parked residence falls **2.3 → 0.85 s** and the starvation **−54…−59 %**
where the parking is the binding term. Branch
`perf/rewrite-epoch-supply-close` off `dev` `2a486273`. Lever
`SQUEEZEFS_REWRITE_SUPPLY_CLOSE` (default on; `0` = the shipped
KD-1.6/1.7 triggers — the A/B lever for the fleet row).

## 1. The term (input: `.benchmarks/2026-09-06-free-grace-term1-fleet.md` §3)

With the four KD-FG-11 re-derivations the grace ring's hold fell 9.4 →
1.8 s and the row still failed: the authority's harvest-serve timeline
showed a co-writer lane's recycled supply arriving in **bursts with 9–31 s
gaps** aligned with the co-writers' rewrite-epoch closes, every
lane-ENOSPC storm inside a gap. The free-grace rate equation's stage list
(design-free-grace-sustain §3.2) starts at `finish_free` — the stage
BEFORE it, the epoch's PARKING of the displaced key, was unmeasured and
had become the dominant one.

Why the shipped triggers cannot fire in time on this posture:

| Trigger (KD-1.6/1.7) | On the s11 co-writer |
|---|---|
| full coverage (`epoch blocks × bs ≥ file size`) | a co-writer's 1.25 GiB slice of a 10 GiB shared file never reaches it |
| fsync / RELEASE | the iteration's END — the boundary |
| idle (30 s) | never idle mid-iteration |
| ENOSPC early-close, retry once | fires only AFTER the `StorageFull`; the retry finds nothing because on a co-writer the freed A key returns only through the recycle loop (ship → grace ring → lane list → harvest RPC), one whole transit later — the refusal storm's exact shape |

So each co-writer parks its whole per-iteration displacement (≈ 320
blocks over two volumes) until the boundary, and its lane must hold live
+ new + parked + the previous burst still in flight: 160 + 160 + ≤ 160 +
≤ 160 against a 512-block per-volume share. The margin is gone before the
recycled burst returns, whatever the ring's hold is.

## 2. The mechanism

```
laned co-writer allocator, every refill tick (ahead_refill_tick / pushed_refill_tick, AFTER the harvest):
  watermark  = ceil(claim-rate EWMA × refill horizon), capped at lane-share/4   (the ahead-harvest's own threshold — L5)
  reachable  = lane_reachable_blocks()                                          (free-list lane population + lane-scoped virgin tail)
  if lever off ∨ no supply-close sink            → nothing (uncounted — the shipped shape)
  if watermark == 0 ∨ reachable ≥ watermark       → declined_covered
  deficit    = watermark − reachable
  candidates = open rewrite epochs as (ino, parked blocks)
  plan       = supply_close_plan(deficit, candidates)   — largest parked first, until Σ yield ≥ deficit
  if plan empty                                   → declined_no_parked
  for ino in plan: close_rewrite_epoch(ino, current token)   — THE shipped swap: one whole-tx publish + the §5.2 deferred frees
  bounded   += candidates left open
```

* **The signal is the lane's own numbers, not a new knob.** The watermark
  is `rate × horizon` — the blocks one recycle-loop transit consumes —
  already derived per tick by `BlockAllocator::sample_alloc_rate`
  (design-free-grace-sustain §5.5); `reachable < watermark` is the very
  condition that fires an ahead harvest. A quiet writer's watermark decays
  to 0 and nothing fires (nothing to protect).
* **The deficit is the derivation of "worth" AND the bound.** At the end
  of one transit the lane will have consumed `watermark` blocks; what the
  closes must return to leave it at one transit's cover is `watermark −
  reachable` (at exhaustion: the watermark itself). Closing largest-first
  until that is covered is the fewest publishes for the most supply. Every
  candidate yields ≥ 1 block, so closes per tick ≤ deficit ≤ watermark ≤
  share/4 — a co-writer never publishes more epochs in a tick than blocks
  it is short. The only shape that reaches the bound is `share/4` distinct
  open epochs each parking one block, which is exactly the shape on which
  the un-shadowed path would already have published once per block.
  Candidates left open are counted (`rewrite_shadow_supply_close_bounded`).
* **Nothing new is durable.** The close IS `close_rewrite_epoch` — the
  publish + the deferred frees under the §5.2 law (the frees on a
  co-writer ship as `FreeBlocks` or ride the owner's recompute exactly as
  at every other close). W1–W6 stay true verbatim; W5 is never entered
  from here (the tick checks the D0 poison first — a fenced close is the
  fence tripwires' story); the dismount's own closes and a concurrent tick
  compose on the epoch registry's `remove_sync` under the meta lock.
* **Scope is enforced in one place.** `DataRouter::arm_rewrite_supply_close`
  (called from `cowriter::install_client_halves` right after the lane
  engagement — both client postures) hands every allocator a Weak-held
  sink; `BlockAllocator::set_lane_supply_close_sink` installs it ONLY on
  a lane that has a harvest sink, i.e. a co-writer's. The authority's
  lane 0 and every unpartitioned mount keep KD-1.6/1.7 byte-identically
  — pinned, gauges included.
* **Hot-path cost: none.** The record path (`rewrite_shadow_record`) is
  untouched; the decision reads two atomics on the tick and walks the
  epoch registry (O(open epochs)) only when the lane is short. Parked
  keys are counted mount-wide per epoch (`parked_bytes ÷ block size` —
  the gauge the record already maintains); each key returns to the lane
  of the volume it lives on, and with two data volumes each allocator's
  tick asks for its own deficit against the same candidate set (the
  second tick sees the first's closes: `Ok(None)` on an epoch already
  gone).

Files: `src/routing.rs` (lever cell, `supply_close_plan`,
`close_rewrite_epoch_counted`, `supply_close_epochs`,
`arm_rewrite_supply_close`), `src/block_allocator.rs`
(`LanePartition::supply_close`, `set_lane_supply_close_sink`,
`supply_close_deficit`, the tick hook), `src/data_alloc_lane.rs`
(`SupplyCloseSink`), `src/cowriter.rs` (the arm), `src/fuse_client.rs`
(gauges), `src/env_knobs.rs` + `docs/operations.md` (the knob + rows).

## 3. Contracts (`tests/rewrite_shadow_supply_close_tests.rs`, 8 tests, red against `2a486273`)

| # | Contract | Pinned by |
|---|---|---|
| 1 | a laned co-writer (lane 1 of 2, 32-block share, watermark at its cap 8) with 5 reachable and 13 parked closes its epoch on the tick BEFORE any `StorageFull`: `supply_closes` +1, `supply_close_blocks` +13, `swaps` +1, `fallbacks` +0, the 13 keys reach the free path, the durable map names B; the restocked lane's next tick is `covered` | `a_starving_laned_co_writer_closes_its_epoch_ahead_of_the_storage_full` |
| 2a | a stocked lane (64-block share, 4 parked) never closes — `declined_covered` +1, the epoch stays open, fsync closes it as KD-1.6 | `a_stocked_lane_never_closes_and_the_ledger_says_covered` |
| 2b | a starving lane with no epoch — `declined_no_parked` +1, nothing else moves | `a_starving_lane_with_nothing_parked_declines_no_parked` |
| 2c | `SQUEEZEFS_REWRITE_SUPPLY_CLOSE=0`: the same starving tick moves NO gauge; fsync closes the epoch as shipped | `the_lever_off_is_the_shipped_kd16_kd17_shape_verbatim` |
| 3 | an unpartitioned mount and an authority's lane 0 (no harvest sink): the arm installs nothing, the tick moves nothing, the routine close is the only close | `a_single_writer_and_an_authority_never_see_the_trigger` |
| 4 | nine epochs (eight × 1 parked, one × 3), 1 reachable, deficit 7: five closes (3 + 1 + 1 + 1 + 1), the 3-parked epoch first, four left open and counted `bounded`, closed later by their fsync | `many_small_epochs_close_largest_first_and_the_deficit_bounds_the_tick` |
| 5 | the planner is pure: order, stop rule, tie-break by ino, zero-parked never a candidate, zero deficit closes nothing | `the_plan_closes_largest_first_until_the_deficit_is_covered` |
| 6 | the closed-loop model below | `the_iteration_model_bounds_the_parked_term_by_the_lanes_headroom` |

Every existing rewrite-shadow / lane / free-grace contract stays green
(the suites run are listed in §6).

## 4. The closed-loop model (in-process, deterministic; the product decides every step)

One co-writer lane on one data volume at the fleet's per-volume numbers
(§1): share 512, 160 displaced blocks per iteration, 25 blocks/s, a 1 s
boundary, 12 iterations. The model performs only the acts (a mint, a
park, a close); the watermark is `sample_alloc_rate`'s arithmetic (EWMA
α = 1/4 over 1 s of claims, `ceil(rate × horizon)`, cap share/4) and the
closing set is `routing::supply_close_plan`. The SHIPPED shape has both
of its closes: KD-1.6 at the boundary and KD-1.7 at the `StorageFull`
(the parked keys enter the loop at the stall; the retry finds nothing).
The loop transit is swept over the fleet's measured values: 3 s (the
term-1 row's hold + RTT + one floor) and the serve timeline's gap class.

| loop transit | lever | starvation (ms of blocked demand) | closes | parked residence, mean | mean reachable |
|---|---|---|---|---|---|
| 3 s | off | 0 | 12 | 3,180 ms | 223 |
| | **on** | 0 | 12 | 3,180 ms | 223 — **identical: the lane never dips below the watermark, the lever fires nothing** |
| 9 s | off | 0 | 12 | 3,180 ms | 105 |
| | **on** | 0 | 39 | **1,381 ms** | **145** |
| 12 s | off | 6,572 | 20 | 2,335 ms | 81 |
| | **on** | **2,686 (−59 %)** | 76 | **847 ms** | **105** |
| 15 s | off | 18,776 | 23 | 2,021 ms | 66 |
| | **on** | **8,688 (−54 %)** | 78 | **844 ms** | 70 |
| 24 s | off | 63,778 | 24 | 1,999 ms | 46 |
| | **on** | 53,364 (−16 %) | 112 | **827 ms** | 44 |

Reading it: at the 3 s transit the shipped shape does not starve on this
share and the lever is inert — the control row. From ≈ 10 s the previous
burst stops returning before the next iteration needs it; the shipped
shape parks the whole 160 and starves, KD-1.7 then closes at the stall
(20–24 closes vs 12 boundary closes). The lever closes ≈ once per tick
while the lane is short (76–112 closes over the run — the bound holds: one
epoch per tick, its yield ≈ one second's displacement), the parked
residence is bounded by the tick (≈ 0.85 s), and where the PARKING is the
binding term (12–15 s) the starvation more than halves. At 24 s the loop
itself binds — Little: 352 circulating blocks ÷ 24 s ≈ 14.7 blocks/s
against a 25 blocks/s demand — and the lever can only stop adding to it
(−16 %); that regime is the recycle loop's own (terms T1–T8), not this
lever's.

The cost the model prices: 76–112 publishes over ≈ 90 s instead of 12–24
— ≈ 1 per second per starving volume, against a rewrite rate of 25
blocks/s that the un-shadowed path would publish per block.

## 5. What is NOT claimed

* **The fleet row.** No `mw_fleet`/`run_mw_matrix` run was made here (the
  parent owns it). The s11-mpiio A/B is `SQUEEZEFS_REWRITE_SUPPLY_CLOSE`
  on vs `0`, reading on each co-writer `rewrite_shadow_supply_closes` /
  `_blocks` (must account for the parked keys the row would otherwise
  park to the boundary), the decline ledger, `alloc_lane_enospc_refusals`
  (the 35,007 of the term-1 row), and on the authority the harvest-serve
  timeline's gaps — a lane's supply should now arrive at ≈ the tick
  cadence while the co-writer is short, not in iteration-sized bursts.
* **The return path's other gate.** The lever injects the parked keys into
  the loop; whether the co-writer's REFILL reaches them ahead of its
  ENOSPC depends on the owed ledger seeing them: `note_owed_freed` is
  incremented only by `ship_displaced_frees`' `Freed` verdicts, and a
  close whose covering publish the owner RECOMPUTED retires the keys
  locally (finding 36) without noting them owed — so on that arm the ahead
  harvest and the pushed refill (both `owed > 0`-gated) stay dark and the
  supply returns on the ENOSPC harvest only. Whether the s11 co-writers'
  closes take that arm is a fleet read (`publish_recomputed`), not settled
  here; if they do, the owed ledger's recompute face is the next item.
* **Per-volume attribution of parked keys.** The yield is counted
  mount-wide per epoch; a block key's volume is not parsed at the
  decision (that would be a hot-path string parse per record, or a
  per-epoch per-volume map). With two volumes a tick may close an epoch
  whose keys mostly return to the other lane. The bound still holds per
  tick; the model is single-volume.
* **The model is a model**: one lane, one file, a deterministic demand,
  the loop transit as a parameter. It reproduces the mechanism, not the
  fleet's numbers.

## 6. Gate

`cargo fmt --check` clean; `cargo clippy --all-targets --all-features --
-D warnings` clean; suites run with `--test-threads=1`:
`rewrite_shadow_supply_close_tests` (8), `rewrite_shadow_tests`,
`rewrite_shadow_supersede_tests`, `rebind_starvation_tests`,
`env_knob_convention_tests`, `mw_data_alloc_lane_tests`,
`cowriter_enospc_wedge_tests`, `free_grace_lane_visible_tests`,
`mw_cowriter_lane_tests`, `write_supersession_tests`,
`overlay_overwrite_tests`, `discard_elision_tests`,
`mw_cowriter_free_tests`, `mw_cowriter_free_leak_tests` (counts in the
branch report). Not run: `task check` (the parent's), the fleet rigs.
