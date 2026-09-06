# 2026-09-06 — the co-writer ENOSPC wedge: the bounded allocation park was unbounded

| | |
|---|---|
| **Branch** | `fix/cowriter-enospc-wedge` off `dev` 8168e26e |
| **Commits** | `f3e6710d` (red repro suite, 8 of 9 time out against `dev`) · `09bdfb06` (fix) · docs commit |
| **Evidence** | `.benchmarks/rows-d4-s11-20260905/` — `m57.log` (the 30-minute wedge), `m50.log`/`m50.stats.json`, `m53.log`, `m0.log`/`m0.stats.json` (the authority); recorded in `.benchmarks/2026-09-05-d4-free-grace-sustain.md` §6 |
| **Class** | load-dependent hang on the write path — a first-class product bug (AGENTS.md); finding 15 (the lane exhaustion itself) stays OPEN |
| **Fleet repro** | RUN — see §6 (the wedge is gone; finding 15's root cause moved to `.benchmarks/2026-09-06-cowriter-free-refcount-leak.md`: the refusals were duplicate own-mint blob reclaims, the leak a RAM-only lifetime under recomputed publishes) |

## 1. The finding

Fleet: 1 authority + 8 co-writers (`tests/mw_fleet.sh create N=1 --cowriters=8`,
range custody), `tests/run_mw_matrix.sh s11-mpiio` (32 ior ranks, one shared
file, 4 MiB block-cyclic, rewrite iterations). Deterministic, 2 of 2 runs.

23 s in, every co-writer's allocation lane exhausts (m57 23:32:05):

```
I/O error: data volume 'nvme4n1' full: 8183 of 8192 blocks allocated — lane 6 of 16 is
exhausted while 0 free block(s) belong to lanes this mount does not own (alloc_lane_enospc_refusals …)
```

That refusal is CORRECT (finding 15). What is wrong is what follows on five
of eight co-writers (m50 m51 m53 m55 m57):

| m57 signal | count | meaning |
|---|---|---|
| `full:` refusal lines | 2,366 over 41 min, exactly 1/s | the log is rate-limited to 1/s (`refuse_lane_enospc`); m50's counters say the true rate: `alloc_lane_enospc_refusals` 16,769, `alloc_lane_harvests` 16,899 — **one authority harvest RPC per refusal, ≈ 7/s, forever** |
| `FUSE op watchdog: write (ino 2) … parked past station [route-dispatched]` | 37,024 (47,836 watchdog lines total) | 100 writes in flight for 30 min, never cancelled (D1.b) |
| `write-phase census … parked in phase` | `entry` 15,947 · `checkout` 8,442 · **`ov_alloc` 4,691** · `materialize` 936 | the HOLDERS sit in `ov_alloc`; the writes behind them wait at `checkout` (the stripe); the unit-level `entry` rows are those same writes' unit keys; `materialize` is the slot-extraction queue starved by the parked slot writes |
| `lock-wait census: block/write_checkout … stripe last-holder site=write_checkout` | 29,952 | a genuinely held `BLOCK_FLUSH_LOCKS` stripe (order 3) — the holder is the `ov_alloc` write |
| FUSE connection `waiting=135`, `cat .stats` in D-state 30 min | — | the FUSE-over-io_uring queue entries are consumed by the never-replying writes; the `.stats` READ parks in the kernel for a ring entry (m57's `stats.json` is 0 bytes) |

Only the fleet teardown's connection abort freed the mount.

## 2. The exact park

**Function:** `BlockAllocator::allocate_block_grace_bounded`
(`src/block_allocator.rs:2297` on the fix; the finding-29 "wait the
pressure ruling promises", `a50da1e4` + `37bf6036`).

**Lock held:** the write's `BLOCK_FLUSH_LOCKS` guard (order 3), taken at
`block_lock_acquire_timed(ino, b, BlockLockSite::WriteCheckout)`
(`src/fuse_client.rs:17782`) and held across `try_device_overlay_store`
(`src/fuse_client.rs:16020–16021`, phase `WP_OV_ALLOC`) — and identically
across every other write-path mint (`routing.rs` `write_striped` /
`durable_write_sparse_blocks` / `upload_full_block`, the fsync flush legs,
the pipeline upload: 14 sites).

**Permit:** the `ov_alloc` holder sits BEFORE `WP_OV_ADMIT` (the W-3 permit
is taken after the mint), so it holds no write-pipeline permit. The DETACHED
pipeline upload (`pipeline_upload_parked_block`) is the site that DOES park
holding its `PipelinePermit` — in-flight bytes that never return, which is
why new writes park at admission (`write_pipeline_admission_waits` 861 on
m50) and the census shows the `entry` phase. Both faces are one park.

**Why "bounded" was a lie — two compounding defects:**

1. `free_grace::pressure_park_wall_ms()` (`dev` `src/free_grace.rs:1791`)
   was `(bound().saturating_mul(2)).max(1_000)`. `bound()` is the published
   **reallocation LABEL** — an owner-clock instant, whose "nothing owed"
   sentinel is `u64::MAX` (`static BOUND: AtomicU64 = AtomicU64::new(u64::MAX)`,
   `src/free_grace.rs:186`) on every mount that is not a free-grace OWNER
   with members. A co-writer is `free_grace_mode: "reader"`. Read as a
   duration: **the wall was `u64::MAX` ms** on every co-writer, reader and
   unarmed writer. The only test of the wall
   (`a_frozen_plane_bounds_the_park_by_wall_time`) armed an owner plane on
   a manual clock whose labels were ~10,000, so 2 × label ≈ 20 s passed its
   10 s bound by luck of the fixture.
2. `reclaimable_supply_exists()` (`dev` `src/block_allocator.rs:1901`) on a
   laned co-writer was `self.lanes.get().is_some_and(|l| l.harvest.get().is_some())`
   — TRUE whenever a harvest SINK is installed, i.e. on every laned
   co-writer, regardless of what the authority holds. The existence of the
   wire was taken as evidence of supply.

Together: an exhausted co-writer lane enters the park unconditionally and
never leaves it — one 50 ms slice + one harvest RPC per pass, forever, under
the block stripe. The `waited_ms` accumulator also summed only the slices
(never the RTT), a third under-read that the `Instant` measurement retires.

**Not the park:** the writeback ladder's classifier
(`writeback_error_is_terminal`, ENOSPC = transient) governs STAGED units
whose bytes are safe and whose drain frees space (FIND-RW5-A) — correct,
untouched. A co-writer arms no writeback flusher. The `sync_drain` ENOSPC
valve is bounded by `ENOSPC_VALVE_MAX_ATTEMPTS` and was not the loop.

## 3. The fix (`09bdfb06`)

* `free_grace::pressure_park_wall_ms()` = `fence_bound_base_ms() × 2`, floored
  1 s — **a duration** (the routine fence bound, the comment's own words:
  "twice the ROUTINE bound"). No plane ⇒ 1,000 ms.
* `BlockAllocator::reclaimable_supply_exists()` — the lane shape now also
  requires `horizon_composed_ms != 0`: the last harvest reply's
  `bound_age_hint_ms` (the authority's LIVE `free_grace_bound_age_ms`,
  nonzero iff its ring holds offsets a fence can still release; deposited
  by `harvest_lane_supply` on every pass BEFORE the verdict). The exact
  co-writer analog of the authority's `!grace.is_empty()` arm: an authority
  holding nothing is genuine exhaustion and refuses at once.
* `allocate_block_grace_bounded` measures the wall with an `Instant` (the
  harvest RPC and the valve's passes count). Past the wall: `StorageFull`
  is TERMINAL for that write (ENOSPC propagates, the stripe releases, the
  permit — where held — returns with the task).
* Gauge **`write_enospc_refusals`** (stats inode, `docs/operations.md`):
  WRITE replies refused `ENOSPC` — the synchronous face (promotion /
  sparse-stripe mints the write itself owns). Custody the never-lossy
  ladder ACKed reports at `fsync`, not here (the existing law, kept).

Not changed: the allocation partition, the harvest verb, the free-grace
ring/valve/fence machinery, the KD-B4-8 overlay decline-to-accumulation,
the never-lossy ladder for ACKed custody.

## 4. Contracts (`tests/cowriter_enospc_wedge_tests.rs`, 9)

RED against `dev` 8168e26e — 8 of 9 time out at their 10 s bounds
(`THE WEDGE: the bounded allocation must end within the bound: Elapsed`;
the wall assertion reads `18446744073709551615`); the solo control passes.
GREEN on the fix: 9/9, **×20 bit-identical** (2.60–2.67 s per run).

| contract | shape |
|---|---|
| `the_bounded_allocation_ends_on_an_exhausted_co_writer_lane` | the exact park, field shape: lane 1 of 2 exhausted, empty harvests, authority hint 15,096 ms (m50's `alloc_lane_harvest_horizon_ms`) ⇒ parks (`free_grace_pressure_parks` grows, harvests re-run) and refuses `StorageFull` within `wall + 2 s` |
| `an_authority_holding_nothing_refuses_the_co_writer_without_a_park` | hint 0 ⇒ `StorageFull` in < 500 ms, zero park slices |
| `the_park_wall_is_a_duration_never_the_reallocation_label` | no plane ⇒ 1,000; armed fence 4 s ⇒ 8,000; fence 400 ms ⇒ 1,000 |
| `the_solo_control_a_full_single_writer_store_refuses_at_once` | full solo store: prompt, no park (today's law) |
| `exhausted_lane_writes_terminate_and_the_mount_keeps_answering` | cache-less striped mount over a laned allocator, `SQUEEZEFS_WRITE_PIPELINE_DEPTH_BLOCKS=0` (the write owns the mint under its guard — the field's holder shape): 4 concurrent whole-block writes past the exhausted lane each TERMINATE within 10 s (Ok under the never-lossy ladder or ENOSPC); `getattr` + `.stats` answer during and after; every touched stripe `try_lock`s free; `write_pipeline_inflight_bytes` → 0; `fsync` → `ENOSPC` within the bound |
| `a_parked_pipeline_upload_returns_its_permit_within_the_bound` | the ACK-early shape (default governor): the detached upload's permit returns, stripes free, inflight 0 |
| `an_enospc_write_does_not_poison_its_sibling_block` | one lane block left, two concurrent writes: exactly one lands and reads back exact, `fsync` reports the other |
| `freeing_space_after_enospc_lets_new_writes_land` | unlink + release + reclaim drain returns the lane's blocks; a fresh write lands and is durable — no latched dead state |
| `the_write_enospc_refusals_gauge_counts_exactly_the_refused_writes` | a fresh file's first striped write against the exhausted lane is refused synchronously and counts 1; fsync's ENOSPC counts 0; read through the stats inode |

Note on the in-process posture: the mount-level rig runs the WRITER posture
over a LANED allocator with a stubbed harvest sink — the park is
posture-independent (`reclaimable_supply_exists` keys on the lane state,
not the posture), so it is the same park; the co-writer posture's
difference (no writeback flusher, shipped metadata) is exercised by the
allocator-level contracts and stays for the fleet row.

## 5. Gate (this side)

* `cargo fmt --check` clean.
* `cargo clippy --all-targets --all-features -- -D warnings` exit 0;
  `cargo clippy --all-targets -- -D warnings` exit 0.
* Suites, `--all-features -- --test-threads=1`: `write_through_coverage_tests`
  8/8 · `posix_semantics_tests` 13/13 · `data_path_correctness_tests` 27/27 ·
  `mw_cowriter_free_tests` 49/49 · `mw_data_alloc_lane_tests` 23/23 ·
  `mw_cowriter_lane_tests` 26/26 · `dlm_cowriter_tests` 18/18 ·
  `async_block_reclaim_tests` 21/21 (+1 pre-existing ignored) ·
  `rebind_starvation_tests` 5/5 · `reader_free_grace_tests` 39/39 (owns the
  finding-29 park contracts incl. `a_frozen_plane_bounds_the_park_by_wall_time`)
  · `cowriter_enospc_wedge_tests` 9/9 ×20.
* No `task check`, no root rigs (parent's).

## 6. Fleet repro — RUN 2026-09-05 22:03 (post-reboot, 32 CPUs): the wedge is GONE

Same fleet (1 authority + 8 co-writers, range custody, `OSS_GB=32`),
`tests/run_mw_matrix.sh s11-mpiio` from zero on `4ca040aa` (release);
artifacts `.benchmarks/rows-wedgefix-s11-20260906/`.

| | before (`8168e26e`, `rows-d4-s11-20260905/`) | after (this fix) |
|---|---|---|
| ior outcome | `fsync(15) failed` ×5, invocation FAILED at 23 s | **no fsync failure; all 18 iterations ran**; fails the sustained-window gate (below) |
| `FUSE op watchdog` reports | 37,024 on m57; 5 of 8 co-writers wedged | **2–4 per co-writer** (single overdue ops, not a storm) |
| mount responsiveness | m57 `waiting=135`, `.stats` in D-state 30 min | **every mount's `.stats` answered** at capture |
| `write_pipeline_inflight_bytes` at capture | never drained | **0 on all 8** |
| lane ENOSPC refusals (`volume … full`) | 2,366 on m57 | 114–225 per co-writer — the SHORTAGE persists (finding 15) |
| `write_enospc_refusals` | — | 0: every refused mint was an ACK-early upload, reported at fsync/close per the custody law — and ior's fsync did NOT fail, so the re-present-and-retry ladder recovered every one within the row |

**Verdict: the wedge is fixed** — an exhausted lane now costs a bounded
park (≤ 1 s wall on a co-writer, refused if the authority holds nothing to
release) and the mount stays alive; the row degrades to finding 15's
ORIGINAL 08-19 signature instead of hanging: **phase A1 NOT SUSTAINED —
the last third's mean decays 920 → 557 MiB/s (> 30 %)**, iterations
bimodal (2,390 / 2,231 MiB/s while the lanes have room, 226–252 MiB/s in
the lane-exhausted iterations, 600–1,000 in between).

**What this run says about finding 15 — the leak, not the ack cadence.**
The free-grace ring is NOT the bottleneck this time: `deferrals` 54,530 /
`releases` 54,487 / `offsets` 43 outstanding at capture, `alloc_stalls`
0, `forced_releases` 0. Yet the lanes exhaust. The authority refused
**3,449 shipped frees** as `block_untracked_free_refusals` (367–542 per
co-writer, the §7(b) "no refcount entry" class, mirrored on the co-writers
as `CLAIM ANOMALY … refcount entry lingers`). A block whose shipped free
is refused is neither referenced by a layout nor on the free list — it
LEAKS from the lane's recycle supply until an ownership-recovery walk
re-derives it, which a co-writer never runs. At ~4 MiB × 3,449 ≈ 13.5 GiB
per row that is a third of the fleet's 36 GiB of lane supply gone in one
phase — the shortage's arithmetic. **Finding 15's root cause is therefore
most likely a correctness bug in the S9 co-writer free path (the
authority's RAM refcount map not learning the reference a harvested
offset took via the shipped publish), not a rate problem** — the next
item, red-first: a harvested-then-published-then-freed block must be
accepted by the authority's `FreeBlocks` and re-enter the lane's supply.
## 7. Recorded, not fixed (the pre-ENOSPC signals on the same rows)

Chronology on m57 (every wedged co-writer shares it within seconds):
23:31:55 fencing-token refusal → 23:31:59 `rewrite epoch for ino 2 FENCED
at close` → 23:32:01 double-release refusals begin → 23:32:05 first lane
ENOSPC → 23:32:11 `CLAIM ANOMALY` ×34 → 23:34:22 first watchdog.

**(a) Fencing-token refusals** — `Refused { errno: 5, msg: "Lock expired or
invalid fencing token: …366 (expected >= …367)" }`, 1–3 per co-writer, gaps
of +1…+7 (m55: +7; m57 second event: +85). All on ino 2 (the one shared
file) under S11 range custody, where m50 records `dlm_custody_grants` 968 /
`renewals` 121 for that ino — the token advances with every span
re-acquire, and an in-flight verb carrying the prior token loses. The write
handler's FIND-RW5-A face-3 retry forgives ONE fence; the second stays
loud (EIO). m50/m53 also show it on `FUSE Fsync: FlushExtents barrier for
ino 2 failed`. **Not the wedge** (the park needs no fence; the wedge
reproduces in-process with zero fences) but a live-mount item: the
downstream `rewrite epoch … FENCED at close: publishing nothing, freeing
nothing … acked un-fsynced rewrite bytes discard with the fenced era` applies
the W5 REMOUNT law to a same-process/same-fleet lease rotation — on m53,
m54, m57 that is ACKed bytes discarded on a live mount. The FIND-M11-A
disposition ("re-present the current generation and retry") that the write
and pipeline paths carry does not reach the epoch close. Board item.

**(b) Double-release refusals** — co-writers: `S9: the authority refused 1
shipped free(s) … already free/graced/quarantined — the double-release
lineage` 174–252 per co-writer; authority m0: `begin_free REFUSED untracked
offset …: no refcount entry — double-release lineage`
(`block_untracked_free_refusals`) **1,687**, against
`meta_ship_publish.free_served_blocks` **1,901** and `harvest_served_blocks`
**20,073**. **89 % of every shipped free the authority served was refused as
untracked.** Co-writer side, the mirror: `CLAIM ANOMALY: offset X claimed
from the free list while a refcount entry lingers (count=Some(1))` (m53/m57
×34 each, the offsets repeating in pairs). Hypothesis for the finding-15
campaign, not adjudicated here: harvested offsets (handed out by the
authority, removed from its free list) take their durable reference through
the co-writer's SHIPPED publish, and the authority's RAM refcount map never
learns the reference — so the co-writer's later displaced free of that
offset finds "no refcount entry" at `begin_free` and is refused, and the
offset is LEAKED from the lane's recycle supply. If so, this IS the
mechanism by which "the free-grace recycle loop loses to the churn": the
lane's supply drains by refused frees, not by slow acknowledgements
(`free_grace_forced_releases` 0 / `laggard_fences` 0 / `alloc_stalls` 0 on
m0 while `demand_waits` 88,082 and `bound_tightenings` 280,447). Same ino,
same seconds as (a): the FENCED-at-close "freeing nothing" is the other
candidate source of the lineage. Both stay with finding 15.
