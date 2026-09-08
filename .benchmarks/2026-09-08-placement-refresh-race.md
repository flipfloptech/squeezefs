# 2026-09-08 — the placement-refresh flake: a trim claim window read as a lane deficit

| | |
|---|---|
| **Branch** | `fix/placement-refresh-race` off `dev` `36d517f3` |
| **Commits** | `4d3a3a31` (red: contracts 8a/8b) · `e761c242` (fix: `LaneCountedSet`'s reachable count) · this record |
| **Trigger** | `tests/rewrite_shadow_supply_close_tests.rs::a_starving_volume_publishes_the_mounts_largest_epoch_whose_keys_restock_the_sibling` (landed `f230eb3a`, the fpp re-attribution's contract 7) failing at line 1074 — `the pick reached the restocked sibling: left "volB", right "volA"` — 4/12 and 7/12 alone on two ×12 loops here (reported 5/12 alone, 10/12 in-suite) |
| **Class** | **a genuine product defect in a gauge**, selected by the test's schedule, not a test-independence fault and not the health worker: `BlockAllocator::lane_reachable_blocks` — the number the §5.9 lane weight, the ahead-refill watermark, the supply-close deficit, the pushed refill and the capacity law all read — dipped by the trim batch for the duration of every KD-4.4 claim window, while the allocation funnel itself treats a windowed offset as pending supply (it parks on the return edge — `.benchmarks/2026-09-07-overlay-enospc-convergence-flake.md`) |
| **Fix** | the counting set's trim edges move MEMBERSHIP, not the reachable count (`LaneCountedSet::{remove_for_trim,insert_from_trim}`; the recount includes open windows; the membership-exact `lane_owned` is derived); red-first contracts 8a/8b of `tests/cowriter_lane_placement_tests.rs` |
| **Venue** | dev box, `cargo test --release --all-features`, the built test binary looped directly with `--test-threads=1`; scoping evidence per the venue rule — the claim is a correctness law, not a number |

## 1. The candidates and what the instruments said

The four suspects, in the order they were weighed:

| # | Candidate | Verdict | Evidence |
|---|---|---|---|
| (a) | a lost update on the `ArcSwap<PlacementTable>` — the health worker's tick building from a pre-restock census and storing after the test's refresh | **ruled out** | `placement_table_refreshes` moved by exactly 1 across the test's refresh (3 → 4 in every failing run: three from `publish_backend` ×2 + `arm_rewrite_supply_close`, one the test's own) and the snapshot the pick read was `Arc::ptr_eq` to the one the test's refresh stored (`same_table=true` in all 7 failures). The worker's first tick is 5 s after `DataRouter::new` (`sqz_time::interval` — first tick after one full period) and never landed inside the window |
| (c) | the band arithmetic on 6 vs 4 of 32 | **ruled out** | with the true counts A = `6 × 1000 ÷ 32 = 187`, B = 125, cutoff `187 × 9 / 10 = 168` ⇒ band `[volA]`; every passing run's snapshot read exactly that |
| (d) | the co-writer's own ahead/pushed tick changing stock between the settle and the pick | **ruled out** | `EmptyAuthority` grants nothing; `alloc_lane_*harvest*` counters flat; A's count read 6 immediately before the refresh in every failing run |
| (b) | a counter the weights read that a background path moves | **CONVICTED** | below |

A temporary probe (not committed; `tests/rewrite_shadow_supply_close_tests.rs` is byte-identical to `dev` apart from one doc comment) captured, in order: A's and B's counts + debt immediately before the test's refresh, the stored snapshot's rows/band + A's count after it, and the same after the pick. Seven failures in twelve runs, every one the same shape:

```
PRE-REFRESH  a.reach=6 a.debt=16384 a.free=6 a.lane_owned=6  b.reach=4 b.debt=0
             refreshes=3 drain_passes=1 pressure_drains=0|1
SNAP         rows=[("volA", 62), ("volB", 125)]  band=["volB"]  a.reach=2 a.debt=0  refreshes=4
POST         same_table=true  rows=[("volA", 62), ("volB", 125)]  band=["volB"]
             a.reach=2 a.debt=0  refreshes=4 drain_passes=1 pressure_drains=1
```

(one run read `volA` at 93 = `3 × 1000 ÷ 32` — three of the four claimed at the census instant; one run's PRE line read `a.debt=12288` — the batch already half taken). Three of the five passing runs also read `a.reach=2..3, a.debt=0` right AFTER the refresh — the window opened a few microseconds later and the pick still landed on A because the band was already `[volA]` and A's `lane_reachable_blocks() > 0` admitted it in pass 1.

## 2. The interleaving

1. B's `ahead_refill_tick` runs the supply-coupled close: F1's four displaced A keys enter `free_blocks` on A. On this harness elision is on (`set_elision_class_all(true)` — the sanctioned bdev-classification seam), so each free records **debt** (`record_elided_debt`) and `finish_free` publishes the offset; A reads 6 = 2 virgin + 4 listed. `settled_reachable(a, 6)` returns.
2. The debt drainer wakes (`DebtDrainer::record`). Its venue law (KD-4.5/4.6) is "foreground active + debt within the watermark ⇒ defer", where the watermark is `debt ≤ virgin tail`. A's virgin tail is **2 blocks = 8 KiB**; the debt is **4 blocks = 16 KiB** — past the watermark, so the PRESSURE venue drains on its first pass regardless of foreground (`block_free_debt_pressure_drains` 0 → 1 across the failing window). `drain_debt_sync` runs on the blocking pool: `take_debt_batch` (debt → 0), then `claim_free_for_trim` per offset — **the four leave the free list** (KD-4.4: no discard may race a new owner's DMA) — then the file punch, then `return_from_trim`.
3. The test's `refresh_placement_table()` census reads `lane_reachable_blocks()` = virgin + `free_blocks.lane_owned()` = 2 + 0 = **2** ⇒ weight 62; B's 125 ⇒ cutoff 112 ⇒ band `[volB]`.
4. The pick: pass 1 over the band admits B (4 > 0) — pass 2 (an out-of-band volume with supply) is entered only when pass 1 finds nothing — so the allocation lands on B. A's four return microseconds to milliseconds later.

The doc on `settled_reachable` already described the mechanism ("a read mid-command under-counts by the batch") and the helper waited for it — but a spin that returns on the first read of the expected value cannot tell "the window has returned" from "the window has not opened yet". Fixing THAT would have been the test-race fix. It is not the right fix, because the same read is the product's.

## 3. Classification

**A product defect in the gauge, not in the test.** `lane_reachable_blocks` is documented as "exactly `try_allocate_block`'s own reachable set", and since 2026-09-07 the funnel's own verdict on a windowed offset is *reachable* — `allocate_block` parks on the window's return edge instead of refusing `StorageFull` (contract 9, `tests/discard_elision_tests.rs`). The gauge disagreed with the funnel for the duration of every device command the trim issued. Its readers on a laned co-writer:

* the §5.9 lane weight (`refresh_placement_table`) — a refresh inside a window mis-bands the volume for one health cadence (≤ 5 s), the shape here;
* pass 1 / pass 2 of `PlacementTable::pick` and `failover_candidates` (`> 0` / `== 0`) — a volume whose whole listed supply sits inside a window was skipped for its sibling;
* `supply_close_deficit` (`watermark − reachable`) — a tick inside a window closes MORE rewrite epochs than the true supply required (each an early durable publish + a recycle-loop transit);
* `should_harvest_ahead` and `pushed_refill_tick` — a spurious harvest RPC;
* `sample_alloc_rate`'s capacity law (`live = share − reachable − owed`, the published `alloc_lane_share_needed_blocks` / `alloc_lane_headroom_pct`).

Reachability on the fleet: a co-writer's displaced frees SHIP (`free_block`'s co-writer arm) and never record local debt, so the pressure venue's window does not open on a co-writer's allocator for rewrites; the `trim --full` / defrag venue (`BackendRouter::trim_elided(full = true)`) claims **every backend's whole free list in 64-block windows** — co-writers included — so an operator trim on a co-writer moved every one of the decisions above by up to 64 blocks per window. Under the test the harness's elision seam selects the same schedule the fleet's trim would.

## 4. The fix

`src/block_allocator.rs`, all inside `LaneCountedSet` (KD-FG-10's counting set) and the two allocator trim edges:

* the maintained owned count is now `reachable_owned` — free-listed **or inside an open trim claim window** — and `lane_reachable_blocks` = virgin + that word (one load, exact at every instant with respect to the window: the trim edges never touch it);
* `remove_for_trim` (the claim): membership out, `per_lane` −1, the index recorded in `trim_windowed` (the set's `remove` arbitrates the claim, so only the winner records the window — a racing venue's lost claim never erases it); `reachable_owned` untouched;
* `insert_from_trim` (the return): membership back, `per_lane` +1; `reachable_owned` untouched for a windowed block (it was counted throughout), −1 if the block was re-listed mid-window by a plain insert (that insert counted the member; the window's share leaves with the window), +1 only for a return no claim preceded (a plain insert — the accumulation shape `mw_cowriter_free_tests` and `benches/write_path_bench.rs` seed with). Returns whether a window closed, so `return_from_trim` decrements `trim_claimed` only for a real return (the seeding shape used to wrap the funnel's window counter to `u64::MAX`);
* `set_partition` (the `engage_alloc_lanes` / `adopt_lane` recount): `reachable_owned` = owned members + owned windowed blocks under the mask in force NOW — an adoption mid-trim owns a windowed block from here, and its return then moves nothing;
* `lane_owned()` — the membership-exact count the KD-FG-10 drift tripwire and `foreign_lane_free_blocks` read — is derived: `reachable_owned − |owned ∩ trim_windowed|` (a scan of ≤ one trim batch on a diagnostic path; never on the pick path).

Unchanged: `trim_claimed` and its counted-before-the-remove law (contract 9's park), the KD-4.4 claim itself (a discard still never races a new owner's DMA), `per_lane` and the authority's lane-supply hint (membership-exact — a harvest cannot take a windowed block, so the hint should not name it), every hot-path `insert`/`remove` (one atomic on the same word as before, renamed), single-writer allocation (unpartitioned counts everything; the recount law is the one already documented for mutations racing it).

Consequence, stated: a lane-governed volume whose WHOLE listed supply is inside a window now admits in pass 1 and its allocation parks for one device command (≤ `PARK_BOUND_CEILING_MS`) instead of the pick moving to a banded sibling. That is the funnel's law applied consistently (the volume has supply; the park is how the funnel reaches it); it arises only with ≤ one trim batch listed and the virgin tail exhausted.

## 5. Contracts

* **Red-first** — `tests/cowriter_lane_placement_tests.rs` contract 8a `a_trim_claim_window_is_reachable_supply_not_a_placement_deficit`: the fpp numbers (A 6/32, B 4/32) with the drain's claim phase held open by hand on four of A's six (`claim_free_for_trim`, the protocol `drain_debt_sync` runs); asserts `lane_reachable_blocks` 6, `lane_owned_free_blocks` 2, `foreign_lane_free_blocks` 0, the router sum unchanged, a refresh INSIDE the window weighing A 187 / B 125 with band `[volA]`, the placed allocation landing on A's listed remainder with no refusal/park/RPC/failover/rebuild and never on a windowed offset, and the return edge restoring membership without moving the gauge. Contract 8b `the_lane_recount_across_an_open_trim_window_stays_exact`: one owned and one foreign windowed block, lane 0 adopted mid-window — reachable 4 → 5 across the adoption, 5 after both return, the C6-style recount agreeing at quiescence. **RED on `36d517f3`**: `left: 2, right: 6` and `left: 3, right: 4` (the membership read). Green with the fix.
* The victim is untouched apart from `settled_reachable`'s doc comment (its "under-counts by the batch" premise is now false; the spin remains for the close's asynchronous `finish_free`). No retry, no sleep.

## 6. Loop counts

| Shape (`rewrite_shadow_supply_close_tests`, `--test-threads=1`) | Before (`36d517f3`) | After |
|---|---|---|
| victim alone ×12 (two loops) | 4 / 12, 7 / 12 (reported 5 / 12) | — |
| victim alone ×40 | — | **0 / 40** |
| full suite ×40 (8 tests, the victim inside) | (reported 10 / 12) | **0 / 40** |

## 7. Suites and gates (this worktree's `target/`, `--release --all-features -- --test-threads=1`)

| Suite | Result |
|---|---|
| `cowriter_lane_placement_tests` | 14 passed (8a/8b new) |
| `rewrite_shadow_supply_close_tests` | 8 passed (×40 above) |
| `mw_data_alloc_lane_tests` | 32 passed |
| `placement_tests` | 12 passed |
| `volume_drain_tests` | 17 passed |
| `phantom_backend0_tests` | 14 passed |
| `audit_instruments_tests` | 26 passed |
| `mw_cowriter_free_tests` (the KD-FG-10 drift tripwire: "a trim claim" / "the trim return" stages) | 50 passed |
| `discard_elision_tests` (contract 9, the funnel side) | 9 passed |

`cargo fmt --check` clean; `cargo clippy --all-targets --all-features -- -D warnings` and `cargo clippy --all-targets -- -D warnings` clean; `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps` clean; `tests/check_markdown_links.sh` on the two touched markdown files PASS. Not run here: `task check`, root/fleet rigs, the fuse3/loom/fuzz sub-gates — the change touches none of those crates.

## 8. Not claimed

* No field row: the dev box is scoping evidence only (venue rule 2026-09-07). The hot-path cost is unchanged by construction (the same single atomic per owned insert/remove; the trim edges are one device command per batch; the window scan sits on diagnostics and the recount).
* The fleet's `supply_close_deficit` / harvest over-firing during an operator `trim --full` on a co-writer is inferred from the code path, not measured — `rewrite_shadow_supply_closes` and `alloc_lane_ahead_harvests` across a trim on the s11 rig would show it.
* The recount racing a trim edge (a claim or return in flight while `adopt_lane` walks the set) can leave `reachable_owned` off by one until the next recount — the class the counting set already documents for every mutation racing `set_partition`; contract 8b holds the window still across the adoption, it does not race it.
* The health worker's own refresh cadence is not the flake's cause and is not changed; a generation-guarded swap was not needed (the store the pick read was the test's own).
