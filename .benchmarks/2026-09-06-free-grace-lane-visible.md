# 2026-09-06 — finding 15, term 2: the fleet-only `min_acked→released` hop, and the co-writer lane hop behind it

| | |
|---|---|
| **Branch** | `perf/free-grace-lane-visible` off `dev` `c1450c66` |
| **Commits** | `32dcbbf0` (red: the ledger's and the lever's contracts, the co-writer-lane model) · `d070558e` (the instrument + the lever) · docs (this note, the design section, operations) |
| **Design** | `docs/design-free-grace-sustain.md` §"Lane-visible campaign" (this note's home) |
| **Program input** | `.benchmarks/2026-09-06-free-grace-hold-time.md` §6 (the 2026-09-06 09:57 s11-mpiio row: `free_grace_hold_phase_ns.min_acked_released` mean **2,500 ms** over 45,540 samples where the in-process model read 9 ms; artifacts `.benchmarks/rows-holdtime-s11-20260906/`) |
| **Instrument** | `tests/free_grace_lane_visible_tests.rs` — the D-4 closed loop with the lane leg the fleet has: CO-WRITER lanes whose supply is RPC-mediated, an authority that mints nothing itself, product code deciding every step (the ring, the ladder, the owner's renewal, the pushed decision), the sim performing only the acts. Release build, `cargo test --release --all-features --test free_grace_lane_visible_tests -- --nocapture --test-threads=1`. No substrate, no wire, no wall clock; deterministic (bit-identical across runs and profiles) |
| **Evidence tier (rc-manifest)** | every number in §3–§5 is *measured-real in-process*; the fleet numbers in §1 are the 2026-09-06 row's, *measured-real* on the single-node proving fleet. The fleet acceptance row is **OWED (parent)** — §6 |
| **Class** | perf — the release latency of the freed-offset grace loop on a lane-partitioned fleet, and the co-writer's visibility of its released supply |

## 1. What the fleet said, read against the code

The row's third-stage histogram is not a cadence; it is a **tail**:

| `min_acked_released` bucket | samples | share |
|---|---|---|
| ≤ 1 µs (released in the SAME harvest pass that recomputed the bound — lever (d)'s same-pass release) | 6,407 | 14 % |
| 1 µs … 512 ms | 28,147 | 62 % |
| 512 ms … 4 s | 6,527 | 14 % |
| 4 s … 16 s | 610 | 1 % |
| **> 16 s** | **3,849** | **8 %** |

The mean of 2,500 ms is ≈ 75 % the `> 8 s` tail. And the code says what
the stage IS: `GraceRing::harvest_with` stamps `released` at the POP, and
the allocator publishes the popped offset to its free list in the same
synchronous act (`harvest_grace` → `publish_free_list`) — so
`min_acked→released` is **the wait for the next HARVEST EVENT after the
covering bound advance**. The harvest is DEMAND-driven: a terminal free on
the authority (a co-writer's shipped free landing — `finish_free` →
`defer` → `harvest_grace`, on the elided reclaim path the fleet's bdev
namespaces take), an allocation on the authority (`try_allocate_block`'s
head), or a co-writer's `HarvestLaneFree` RPC (`execute_lane_harvest`'s
pass 0). Nothing harvests on a timer, and lever (d) only marks the bound
DIRTY for the next harvest — the membership owner's 10 s sweep is the one
periodic recompute, and it records an ADVANCE without releasing anything.

The fleet spent **≈ 250 of its 304 s in `close`** (ior's per-iteration
`close(s)` column: 2.3 → 36–40 s on iterations 7, 8, 12, 13 — one
co-writer's fsync wedged 30–35 s each time: m52 ×2, m53, m55 ×2, see §7).
In those phases no free lands, nothing allocates, and the healthy
co-writers — holding adopted supply above their decayed watermark — issue
no RPC (`alloc_lane_ahead_harvests` 115–143 per co-writer over 300 s;
`reachable` 30–247 against `watermark` 0–13 at capture): the authority's
rings sit covered-and-unreleased from the sweep's advance to the next
iteration's first demand. That is the `> 16 s` tail (4 long closes ×
≈ 1,000 offsets), and the 0.5–4 s band is the short closes.

**Behind the release sits a hop no ledger measured.** A released
co-writer-lane block lands on the AUTHORITY's free list, reachable by
nobody but that co-writer's next harvest RPC: its ENOSPC park slices
(`pressure_park_slice_ms` on a mount with no plane clamps to its 50 ms
floor — m50 issued 2,398 RPCs for 4,558 blocks, 1.9 per RPC, ≈ 95 %
empty), or the 1 s watermark tick — which fires only while
`lane_reachable < watermark`, i.e. never while the co-writer holds supply.
The co-writer's allocator can mint the block only when the reply is
adopted.

### Hypotheses, adjudicated by the instrument

| Hypothesis | Verdict |
|---|---|
| **(a)** the mark is on the authority, the wait is the co-writer's lane refresh | **CONVICTED, with a correction.** The third stage itself is the RELEASE's wait for DEMAND (the sweep covers, the next demand releases — §3 row 1: 3,408 ms mean, 8,392 of 11,304 samples past 1 s with the storm pausing; 0 ms with release-on-ack). The co-writer's lane hop is a SECOND term behind it, larger still and unmeasured until now: 30.7 s in the same shape (the co-writers never asked), 1.0 s in the continuous 4 GiB shape, 0.75 s recycle-bound (the grain: 64 adopted blocks = 1.3 s before the next poll) |
| **(b)** the free's downstream ladder (reclaim / `finish_free` / quarantine) | **Excluded by the code**: the label is stamped AT `finish_free`, after the reclaim, so the ladder is upstream of `defer`; the fleet's frees ride the elided path (`block_free_reclaim_elided` 45,775 ≡ deferrals); no quarantine engaged (`dlm_custody_quarantined_offsets` 0) |
| **(c)** the shipped acknowledgement's durable commit | **Excluded**: nothing durable sits between an acknowledgement and a release — `membership_registration_commits` 20 over the row (the joins), acks are RAM (`membership_renewals` 3,578) |
| **(d)** lever (d)'s per-pass rate limit | **Excluded by arithmetic**: 493 of 560 refreshes were on-ack; the limit is `floor ÷ members` = 125 ms at 8 members, two orders below 2.5 s; and the limit governs the RECOMPUTE, whose same-pass release is the 6,407-sample ≤ 1 µs bucket |

## 2. The instrument — `alloc_lane_visible_phase_ns`

| Stage | Clock | Read from |
|---|---|---|
| `released_served` | authority | `BlockAllocator::publish_grace_release` marks a grace-released block of a lane this mount does NOT own (`free_grace::mark_lane_release`, one `scc` insert per foreign-lane release — the authority's held-for-peers supply); `take_lane_free_blocks` (the lane harvest's take, `execute_lane_harvest_aged`) removes the mark and stamps the age (`take_lane_release`) |
| `served_visible` | co-writer | the harvest round trip (`LaneHarvest::rtt_ms`); the reply's adoption is the visibility instant |
| `total` | co-writer | `released_served + served_visible` per sample, by construction |

The reply carries each block's age (`PublishReply::LaneFreeGrant::release_ages_ms`,
publish schema 13 → 14 — KD-7 same-commit fleets, the schema-8 posture
verbatim) so the co-writer stamps the authority's measurement beside its
own RTT and **the two clocks never mix**. The authority's `.stats` carries
`released_served` alone; the co-writer's all three, exact-sum.
`alloc_lane_visible_unplaced` counts served blocks with no mark (a block
that reached the list other than through a grace release on this plane);
`alloc_lane_release_marks` is the blocks waiting for their co-writers;
`alloc_lane_supply_blocks` the authority's whole foreign-lane free-list
population (per-lane counters — see §4). The hold ledger's `released` and
this ledger's start are ONE instant (pinned:
`a_deferred_lane_block_waits_for_the_ack_then_the_two_ledgers_chain`,
on real allocators — and a deferred block is served by NO harvest before
its acknowledgement, the safety law).

## 3. The decomposition, measured (release, deterministic; every row closure OK, forced = fences = 0, every served block placed, `pushed_empty` 0)

Every shape: 8 co-writers (lanes 1–8 of 16), each a reader, 1 s
checkpoint, phases staggered, the shipped configuration of every other
lever; `lane_push` OFF is the pre-campaign tree's behaviour verbatim (zero
engagement — pinned).

| Shape | lever | `min_acked→released` mean / samples > 1 s | lane hop `released→served` (n) | hold total | `bound_age` | per-cw blk/s | stalls | RPCs (empty / pushed) | marks left |
|---|---|---|---|---|---|---|---|---|---|
| **fleet** (50 blk/s, spare 700, **3 s write / 21 s close**) | off | **3,408 ms / 8,392 of 11,304** | **30,736 ms** (9,216) | 22,562 | 17,778 | 5.63 | 0 | 144 (0 / 0) | 2,088 |
| | **on** | **0 ms / 0** | **1,540 ms** (11,926) | **14,746** | **10,446** | 5.63 | 0 | 237 (0 / 237) | 66 |
| adequate (20 blk/s, spare 2,000, continuous) | off | 20 ms / 0 | **never served** (0) | 9,107 | 8,668 | 20.02 | 0 | **0** | **13,200** |
| | on | 0 ms / 0 | 361 ms (13,122) | 8,219 | 7,500 | 20.02 | 0 | 817 (0 / 817) | 70 |
| 4 GiB-equivalent (50 blk/s, spare 700, continuous) | off | 2 ms / 0 | 1,028 ms (43,640) | 8,117 | 7,722 | 50.01 | 480 | 1,392 (559 / 0) | 176 |
| | on | 0 ms / 0 | 381 ms (43,779) | 8,073 | 7,500 | 50.01 | 464 | 2,279 (743 / 938) | 173 |
| edge (50 blk/s, spare 450 ≈ hold × churn) | off | 3 ms / 0 | 2,425 ms (22,832) | 12,727 | 10,623 | 25.10 | 9,208 | 10,199 (9,795 / 0) | 2,368 |
| | on | 0 ms / 0 | 1,220 ms (23,456) | 12,703 | 10,556 | 25.00 | 9,208 | 10,337 (9,924 / 213) | 1,744 |
| recycle-bound (50 blk/s, spare 200) | off | 19 ms / 0 | 747 ms (8,000) | 15,240 | 12,553 | 13.33 | 10,800 | 11,544 (11,384 / 0) | 0 |
| | on | 0 ms / 0 | 386 ms (8,000) | 14,965 | 12,078 | 13.33 | 10,800 | 11,589 (11,429 / 70) | 0 |

Reading the rows:

* **The third stage is the fleet's quiet phases.** Continuous shapes read
  2–20 ms (every landing free is a harvest); the write/close shape reads
  3,408 ms with three quarters of its samples past a second — the fleet's
  histogram shape, from its iteration structure. Release-on-ack takes it
  to **0 ms** in every shape (the release IS the recompute's pass).
* **The lane hop is the larger term and was invisible.** With the fleet's
  structure a released block waited **30.7 s** on the authority for a
  co-writer that never asked (2,088 of 11,304 releases were still there
  at the end); in the supply-adequate shape it was NEVER asked for
  (13,200 releases, 0 RPCs — the shipped tree's only refill trigger is
  the watermark tick, and a supplied lane sits above it). The pushed
  refill brings it to the renewal cadence: 1.5 s (quiet phases relax the
  prod toward the routine beat), 0.36–0.39 s under a storm (the 1 s
  prodded beat plus the carriage renewals).
* **The hold itself moves on the quiet shape** (22.6 → 14.7 s;
  `bound_age` 17.8 → 10.4 s): the dirty recompute no longer waits for a
  demand event either — on a quiet fleet that was the 10 s sweep.
* **Exhaustion does not move**: stalls and throughput are identical on the
  edge and recycle-bound rows under both settings. The lever moves the
  release's demand-coupling and the lane hop, never the hold's 6 s of
  derived windows (the hold-time note's §7 adjudication items stand) —
  the 4 GiB lane at 50 blk/s is at the capacity law's edge because the
  HOLD is ≈ 8 s, and no lane-side lever changes that.
* **Economy**: a pushed RPC never finds nothing (`pushed_empty` 0 on
  every row — the hint is the authority's exact per-lane count); the
  RPC total rises by the pushed count on shapes where the co-writers
  previously never asked, and is flat where they were already polling.

## 4. The lever — `SQUEEZEFS_FREE_GRACE_LANE_PUSH` (default on; `0` = the pre-campaign tree verbatim)

| Half | Mechanism | Derivation / law |
|---|---|---|
| **release on ack** (authority) | `free_grace::note_member_ack_advanced` — lever (d)'s one-compare gate (a member whose recorded ack sat ≤ the published bound advanced it) — runs the installed `ReleaseHook` (`multi_writer::release_hook`: every allocator's `harvest_grace_to_front`, the routine harvest repeated until the ring's front is uncovered). The hook's first harvest IS lever (d)'s dirty recompute, so the two arms are one act; `min_acked→released` ≡ 0 for everything the ack covered | rate-limited by exactly lever (d)'s `refresh_on_ack_interval_ms` (`max(floor ÷ members, 2 × scan)`), sharing its refresh instant; RAM only (ring locks, free-list inserts, the mark ledgers); installed only by the multi-writer authority arm — structurally inert everywhere else |
| **the lane-supply hint** (wire) | `membership::Grant::lane_supply_blocks` on the RENEWAL grant — the 1 Hz prodded beat, lever (b)'s carriage renewals included: the member's lane population on the authority's free lists | `LaneCountedSet::per_lane` — one `fetch_add`/`fetch_sub` beside the existing lane-owned count, recounted with it at `set_partition` — read through `multi_writer::lane_supply_source` (`LaneAssignment::lane_of(member_id)`, the id the custody path keys on; O(volumes) loads in the renewal hot op, never a scan — KD-FG-4 stands); 0 on a join grant, to a reader, and on every authority with no partition. A HINT, never a grant: blocks travel only on the harvest verb under its lane checks |
| **the pushed refill** (co-writer) | `MemberSession::renewed*` → `free_grace::note_lane_supply_hint` → `lane_supply_wake` (a first-party `Notify`); the ahead task (`alloc_lane_grant`) and the bounded allocation park (`allocate_block_grace_bounded`) both wait on it beside their own cadence; `BlockAllocator::pushed_refill_tick` runs the SAME harvest (sink → adopt → hint deposit) when `lane_push_wants_harvest(hint, owed)` | pure: lever on ∧ hint > 0 ∧ owed > 0 — the watermark is not consulted (a quiet lane's rate-derived watermark decays to 0, exactly when the routine tick goes dark while the supply sits on the authority). `alloc_lane_pushed_harvests` ⊆ `alloc_lane_harvests` |

Lane laws untouched: the co-writer derives no width (the hint carries a
COUNT, the lane stays the lease's), the authority commits the frontier
(`RaiseAllocLane` unchanged, `alloc_lane_raise_refusals` 0 on every
suite), contiguity picks unchanged (`take_lane_free_blocks` is the
lowest-first take the executor already ran), the safety law by
construction (a block reaches the authority's list only through a harvest,
and a harvest releases only covered labels — pinned on real allocators).

RED evidence: with the lever forced off in the model, contracts 5 and 6
fail exactly where they should — "the release follows the ack within
lever (d)'s rate limit (125 ms) — got 3408 ms" and "the hints woke pushed
refills" — the lever-on assertions are load-bearing.

## 5. Gate (this side)

* Suites, `--all-features -- --test-threads=1`: `free_grace_lane_visible_tests`
  **9/9** (new), `reader_free_grace_tests` 48/48, `mw_cowriter_lane_tests`
  26/26, `mw_cowriter_free_tests` 50/50, `mw_cowriter_free_leak_tests`
  8/8, `cowriter_enospc_wedge_tests` 9/9, `mw_data_alloc_lane_tests`
  23/23, `dlm_cowriter_tests` 18/18, `dlm_membership_tests` 51/51,
  `membership_liveness_tests` 4/4, `derivation_sweep_tests` 45/45,
  `env_knob_convention_tests` 21/21, `no_tokio_convention_tests` 2/2.
* Counted ×20 (release): the 9 new contracts — result in the closing
  report; the rows are bit-identical across runs and profiles.
* `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D
  warnings`, `cargo clippy --all-targets -- -D warnings`,
  `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps`, `cd fuzz && cargo
  check` (the `publish_wire` target's `LaneFreeGrant` shape widened): all
  clean.
* No `task check`, no root rigs (the parent's).

## 6. Fleet acceptance — RUN 2026-09-06 12:10 (dev box, 32 CPUs): the hop collapses as predicted

`s11-mpiio` from zero on the term-2 + term-3 stack (`a24b59a8`, release;
`LANE_PUSH` on), same fleet; artifacts `.benchmarks/rows-t2t3-s11-20260906/`.
Probe 1,802 MiB/s → 10 GiB, 14 iterations.

| gauge | pre (hold-time row `0723b3ea`) | this row | predicted |
|---|---|---|---|
| `free_grace_hold_phase_ns.min_acked→released` mean | **2,500 ms** | **15 ms** (42,012 samples) | ≤ 125 ✓ |
| `free_grace_bound_age_ms` | 7,936 | **7,274** | < 7,936 ✓ |
| hold total / `defer→checkpointed` / `checkpointed→min_acked` | 10,958 / 503 / 7,956 | 7,431 / 415 / **7,001** | |
| `alloc_lane_pushed_harvests` per co-writer | — | 211–255 (every co-writer, engagement exact) | > 0 ✓ |
| `alloc_lane_visible_phase_ns.released→served` mean (m0 clock) | — | 12,285 ms (35,561 samples) | ≤ 1,000 ✗ — see below |
| closure | | deferrals 42,164 ≡ releases 42,012 + offsets 152 ✓; forced 0, stalls 0 | |
| lane ENOSPC (log) m50 / m57 (the clean pair) | 23 / 30 | 110 / 163 (`alloc_lane_enospc_refusals` 9,640 / 15,958) | → 0 ✗ |
| sustained gate | FAIL 1,214 → 410 | **FAIL 1,613 → 622 MiB/s** | |

**Verdict — the lever LANDS (measured, engaged, no loss); the row's
verdict is unchanged, as the in-process model predicted.** The third
stage went 2,500 → 15 ms and the hold 10.9 → 7.4 s; the remaining
7.0 s is the `checkpointed→min_acked` coherence windows (term 1 — the
user's decision). `released→served` still reads 12.3 s on the authority's
clock because it measures a released block's wait until SOME co-writer's
harvest takes it — and with every co-writer's lane at 3–94 % headroom the
demand is bursty (the iterations' bimodality), so blocks released during
a co-writer's own-supply phase wait for the next exhaustion; the pushed
refill (`pushed_harvests` 211–255) fires exactly when a lane runs dry,
which is the law it was built to. Exhaustion did not move
(`alloc_lane_enospc_refusals` 3,042–15,958 per co-writer, the correct
counter — the earlier `volume … full` log count is a subsample), because
hold × churn against `cap/W` is set by the 7 s that term 1 owns.

## 7. Found beside the term, not fixed here (for the parent)

1. **The row's `close` time IS the FAIL**, and it is a co-writer wedge,
   not the grace loop: iterations 7/8/12/13 spent 36–40 s in `close` while
   ONE co-writer's fsync sat ≥ 30 s (`FUSE op watchdog: fsync (ino 2) in
   flight for 30002 ms … parked past station [entry]`, `wedge census:
   pipeline_inflight_blocks=1 pipeline_admission_waits=460` — m52 at
   13:59:00 and 14:01:15, m53 at 13:59:41, m55 at 14:01:53/58). Each
   follows the same sequence 30 s earlier: the co-writer's bounded
   allocation park expires at its 1 s wall (`bounded allocation refusing
   StorageFull after 1014 ms parked … wall backstop (1000 ms)`), the
   flush's write-through fails `StorageFull` and "degrad[es] to the
   staging leg" on a CACHE-LESS mount, and the fsync waits ~30 s for the
   retry. Two candidates for the parent to route: (i) the co-writer's park
   WALL is `pressure_park_wall_ms()` = `fence_bound_base_ms × 2` floored
   at 1 s — and a co-writer has no plane, so `fence_bound_base_ms` is 0
   and the floor governs: a 1 s park against an 8–9 s hold expires on
   EVERY exhausted allocation (the wedge note's law was written with the
   authority's numbers in mind; the co-writer's honest wall is its own
   measured refill horizon, `harvest_horizon_ms`, which the harvest reply
   already carries — 16–17 s on the row); (ii) whatever the staging-leg
   fallback's retry cadence is on a cache-less mount (the 30 s).
2. **`alloc_lane_enospc_refusals` on the fleet was 2,230–10,216 per
   co-writer**, not the 23–133 the hold-time note's §6 table lists (those
   were a different column — likely the ahead-harvest count); the refusal
   counter counts every park-slice retry (20 per parked write at the 50 ms
   slice / 1 s wall), so the row had ≈ 110–500 parked writes per co-writer.
3. The empty-RPC economy: m50 issued 2,398 harvest RPCs for 4,558 blocks
   (≈ 95 % empty) — the parked poll at 50 ms against an 8 s hold. The
   pushed refill does not change the parked poll's cadence (that is the
   wedge note's park design); it removes the polls a co-writer with supply
   never issued and would have needed.
