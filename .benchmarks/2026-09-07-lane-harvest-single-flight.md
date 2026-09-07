# The single-flight lane harvest — finding 15 phase B1's first rung (2026-09-07)

> **Venue caveat (user directive 2026-09-07).** Every number in §4 is an
> IN-PROCESS count of RPCs — a deterministic tally of how many times the
> harvest sink was called, on the dev box, under `cargo test`. It is a
> mechanism-engagement row, not a performance row: the fleet A-B-B-A on
> `squeeze-test` (`tests/mw_fleet.sh` + the day-2 rig, lever on vs
> `SQUEEZEFS_ALLOC_LANE_HARVEST_SINGLE_FLIGHT=0`) is the parent's and
> decides the B1 verdict. Nothing here claims a MiB/s.

Branch `perf/lane-harvest-single-flight` off `dev` `a7cab076`.

## 1. The storm (`.benchmarks/2026-09-07-f15-day2-fleet-pair.md` §2–3)

On the multi-writer fleet (authority + 8 co-writers, two 32 GiB data
volumes, W = 16 ⇒ 512 blocks per lane per volume), the day-2 integrated
tip passed the S11 shared-file gate for the first time and then failed
the `-F` file-per-proc phase B1 (770 → 148 MiB/s) with a legible
signature: **124,240 lane-harvest RPCs in 9.5 minutes (~240/s) for
60,419 blocks — 0.49 blocks per RPC, half the calls empty** — and, in
exactly those windows, the authority's processing of the members'
membership renewals collapsing 22/s → 1–12/s while every member's OWN
acknowledgement stayed ≤ 2.3 s and the authority's view of them read
10–11.6 s. The acknowledgements existed on the members and did not reach
the authority: the renewals that carry them queued behind the serve
plane, which the harvest storm saturated (each RPC runs
`execute_lane_harvest_aged` — three passes: a full free-set scan + sort,
a `reclaim_drain().await`, a pressure harvest). Acks late ⇒ the grace
ring held everything (3,200 offsets) ⇒ every lane starved ⇒ more parks ⇒
more empty harvests — a positive feedback. (A stale-binding storm from a
sibling fix aggravated B1 and is being corrected in parallel; the
samples show the harvest storm starting before the refusals.)

Where the RPCs came from (`src/block_allocator.rs`): every allocation
that hit `StorageFull` ran the ENOSPC-path harvest ITSELF —
`allocate_block_inner`'s `harvest_lane_supply().await` before the
verdict — and the bounded allocation's park loop
(`allocate_block_grace_bounded` → `park_for_reclaimable_supply`)
re-runs `allocate_block` per 50 ms park slice, so N allocations parked
under `BLOCK_FLUSH_LOCKS` on one co-writer issued N harvests per slice.
`BackendRouter::allocate_placed_block` runs the same per-volume harvest
in its failover; the proactive arms (`ahead_refill_tick`,
`pushed_refill_tick`) go through the same function.

## 2. The mechanism

`harvest_lane_supply` is now a **rendezvous per allocator**
(`HarvestFlight`, one per laned allocator's `LanePartition` — per
co-writer, per data volume; a solo / authority allocator has none):

* **One in-flight RPC.** The caller that wins the `inflight` CAS
  (`AtomicBool`) is the leader: it runs `issue_lane_harvest` (the wire
  act, unchanged — grain ask, horizon-hint deposit, adoption, the
  lane-visible stamps), publishes `last_adopted`, bumps `done_gen`,
  clears `inflight`, `notify_waiters()`. Every concurrent caller reads
  `done_gen` BEFORE its CAS attempt, registers on the flight's
  first-party `sqz_notify::Notify` (`notified()` registers at creation),
  then re-reads `done_gen` — a leader that finished between the failed
  CAS and the registration already moved the generation, one that
  finishes later wakes the registered waiter (the enable-then-check
  ordering; no lost-wake window) — and returns the leader's adopted
  count, on which its caller retries its own `try_allocate_block`
  against the refilled list. No lock is held across the RPC
  (`Notify`'s internal mutex guards only the waiter list).
* **A fresh empty reply declines re-issue.** A leader whose reply
  carried 0 blocks stamps `empty_at_witness` with the **supply witness
  generation** read before its RPC: `free_grace::lane_supply_hint_gen()`
  — a new word, +1 per renewal grant that reaches this member, value
  moved or not and whatever `SQUEEZEFS_FREE_GRACE_LANE_PUSH` says (a
  nonzero hint's `lane_supply_wake` is one of these) — plus the
  allocator's `owed_arrivals` (+1 per `note_owed_freed`). While that sum
  has not moved, a caller declines without a wire trip: the authority has
  told this mount nothing new about its lane, so a fresh empty answer is
  the answer. The next grant ends the window for exactly one RPC, so the
  bound is the renewal cadence (≤ 500 ms under an ask) — **never a timer
  of the allocator's own** — and finding 29's promise that the parked
  retries drive the authority's pressure fence holds once per grant per
  volume instead of N times per slice. A grant landing mid-flight makes
  the stamp stale at once (one conservative extra RPC, never a missed
  one). An RPC FAILURE stamps nothing: it is not an answer.
* **No caller waits past the wall.** A joiner's wait is
  `sqz_time::timeout(pressure_park_wall_ms(), notified)`; past it the
  joiner takes its verdict with `0` (its own park then refuses at the
  same wall it always did) and the leader's outcome is untouched. The
  ENOSPC verdict's timing is unchanged in both shapes (§4).
* **The ledger.** `alloc_lane_harvests` stays the RPC count;
  `alloc_lane_harvest_coalesced` counts joiners,
  `alloc_lane_harvest_declined_stale` counts declines — `harvests +
  coalesced + declined_stale` accounts for every would-be call (pinned).
* **The lever.** `SQUEEZEFS_ALLOC_LANE_HARVEST_SINGLE_FLIGHT` (bool,
  default on; registry entry + operations.md row). `0` = one RPC per
  caller — the shipped shape verbatim, the fleet A/B control.

## 3. The contracts (`tests/mw_data_alloc_lane_tests.rs` §10; all red first)

1. `n_parked_allocations_on_one_allocator_issue_one_harvest_rpc` — 8
   parked allocations, supply on the authority ⇒ exactly ONE RPC, all 8
   allocate distinct blocks from its result, `coalesced` +7.
2. `an_empty_harvest_declines_re_issue_until_the_advertisement_moves` —
   one empty RPC, then declines; a grant with the SAME value (0) ends
   the window for one RPC (and does so with `LANE_PUSH=0` too — the
   arrival is the witness, not the value); an owed `Freed` ends it;
   a nonzero hint ends it and the harvest adopts; `harvests + declined +
   coalesced` = the 10 calls made.
3. `two_volumes_are_independent_single_flights` — a flight in progress
   on A coalesces nobody on B; B's decline never declines A.
4. `the_lever_off_issues_one_rpc_per_caller` — 4 concurrent callers ⇒ 4
   RPCs, nobody joins, nobody declines; and the shipped cost made
   legible: the first RPC drains the grain, the other three come back
   empty and REFUSE their callers while 15 harvested blocks sit on the
   local free list (a caller retries its funnel only after its OWN reply
   carried blocks).
5. `no_caller_waits_past_the_wall` — a joiner whose leader's RPC
   outlives the wall takes its `StorageFull` at the wall with one RPC in
   flight and no RPC of its own; a declined park (held ring reported,
   nothing handed out — the m50 shape) still parks and ends at the wall
   with ONE RPC in all.

Pins moved to the new law, each with a lever-off twin re-pinning the
shipped shape: `cowriter_enospc_wedge_tests::
the_bounded_allocation_ends_on_an_exhausted_co_writer_lane` (now: asked
once, every slice declined) + `the_lever_off_re_runs_the_harvest_every_park_slice`;
`cowriter_lane_placement_tests::
both_lanes_exhausted_with_a_held_ring_parks_then_refuses_at_the_wall`
(now: each authority asked once, two independent declines) +
`the_single_flight_lever_off_re_runs_both_harvests_every_slice`.

Suites run (`--test-threads=1`, all green): `mw_data_alloc_lane_tests`
30, `mw_cowriter_lane_tests` 26, `cowriter_lane_placement_tests` 11,
`free_grace_lane_visible_tests` 10, `mw_cowriter_free_tests` 50,
`cowriter_enospc_wedge_tests` 10, `rewrite_shadow_supply_close_tests` 8,
`reader_free_grace_tests` 61, `derivation_sweep_tests` 47,
`env_knob_convention_tests` 21 — 274 tests.

## 4. In-process RPC counts, before / after (deterministic; a probe run once, not committed)

One laned co-writer allocator (lane 1 of 2, 64-block device, lane share
32 minted), the sink a 1 ms simulated authority pass, wall
`pressure_park_wall_ms()` = 1,000 ms (unarmed plane), slice 50 ms.

**PARK** — the m50 / B1 shape: the authority reports a held ring
(`bound_age_hint_ms` 15,096) and hands out nothing; N allocations parked
in `allocate_block_grace_bounded` concurrently.

| parked N | lever | harvest RPCs | coalesced | declined_stale | refused | elapsed |
|---|---|---|---|---|---|---|
| 1 | off | **21** | 0 | 0 | 1 | 1,040 ms |
| 1 | on | **1** | 0 | 20 | 1 | 1,013 ms |
| 8 | off | **168** | 0 | 0 | 8 | 1,041 ms |
| 8 | on | **1** | 7 | 160 | 8 | 1,016 ms |
| 32 | off | **672** | 0 | 0 | 32 | 1,049 ms |
| 32 | on | **1** | 30 | 641 | 32 | 1,024 ms |

Off = 1 + 20 slices per parked allocation (21 × N). On = ONE RPC for the
whole park regardless of N (no grant arrives in-process; on the fleet
each renewal grant re-arms one RPC per volume); `coalesced + declined +
harvests` = 21 × N in every row. Every verdict lands at the same wall.

**FED** — 32 blocks of the lane on the authority's list, N concurrent
callers, grain 64 (one RPC can carry the whole supply).

| callers N | lever | harvest RPCs | coalesced | fed | refused |
|---|---|---|---|---|---|
| 8 | off | 7 | 0 | **2** | 6 |
| 8 | on | **1** | 7 | **8** | 0 |
| 32 | off | 32 | 0 | **1** | 31 |
| 32 | on | **1** | 6 | **32** | 0 |

The lever-off rows are the shipped shape's second cost, beyond the RPC
count: racing callers whose own RPC came back empty REFUSE `StorageFull`
while the grain another caller's RPC adopted sits on the local list (a
caller retries its funnel only on its own reply). Under the lever every
caller is fed from the one reply. (`coalesced` 6 of 32: the rest arrived
after the flight completed and were served from the stocked list with no
harvest at all.)

Scaled to the B1 capture: 8 co-writers × 2 volumes × ≤ 1 RPC per grant
(≤ 2/s under a prod) bounds the fleet's empty-harvest rate at ≤ 32/s
against the observed ~240/s — arithmetic on the mechanism, not a
measurement.

## 5. What this does NOT claim

* Any MiB/s, any B1 verdict, any change to `membership_renewals`/s on
  the authority — the fleet A-B-B-A on `squeeze-test` is the parent's
  row and the only one that decides.
* Rung (2) of the day-2 note's "next term" — renewal serve ISOLATION on
  the authority (the liveness RPC served apart from bulk serve work) —
  is not built; this rung attacks the storm's source, that one the
  robustness.
* The `block_claim_anomalies` growth (1,620 in B1) is untouched.
* The pressure-fence cadence at the authority under a starved lane now
  follows the grant cadence (≤ 1 RPC per grant per volume) instead of
  the park-slice cadence per parked allocation; the finding-29 promise
  holds but its rate changed — the fleet row should read
  `free_grace_{prods,bound_tightenings,forced_releases}` beside the new
  ledger.
