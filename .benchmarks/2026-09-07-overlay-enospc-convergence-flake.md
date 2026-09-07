# 2026-09-07 — the R7 ENOSPC-convergence flake: a trim claim window read as fullness

| | |
|---|---|
| **Branch** | `fix/overlay-enospc-convergence-flake` off `dev` `886d4e31` |
| **Commits** | `bf852deb` (red: contract 9) · `7345ebf0` (fix: the window park + gauge) · this record |
| **Trigger** | `tests/overlay_overwrite_tests.rs::enospc_overwrite_loop_converges_via_epoch_close` (R7) failing at ≈ 3–7 % ONLY inside the full suite binary (`c60746fb` 1/30, `7cf8433b` 2/30, `886d4e31` 1/15; alone 0/40) with `cycle {i}: the overwrite loop starved (spurious ENOSPC …): StorageFull "data volume 'b4b_enospc' full: 3 of 3 blocks allocated"` |
| **Class** | **a genuine product race**, not a test-independence defect: the discard-elision trim venue's KD-4.4 claim window (offset OUT of the free list for the duration of one device command) is invisible to the allocation funnel, so a full store whose whole free list sits inside a window refused `StorageFull` — TERMINAL for that write (ENOSPC to the application, `.benchmarks/2026-09-06-cowriter-enospc-wedge.md`) — while the block came back a device command later |
| **Fix** | the funnel parks on the window's return edge (`BlockAllocator::await_trim_return`; gauge `alloc_trim_window_parks`); red-first contract 9 of `tests/discard_elision_tests.rs` |
| **Venue** | dev box (32 cores), `cargo test --release --all-features`, test binary looped directly with `--test-threads=1 -q`; scoping evidence per the venue rule — the claim here is a correctness law, not a number |

## 1. Reproduction and the dependency

Built once, looped the test binary (`overlay_overwrite_tests-8131c274e8344158`, tip `886d4e31`):

| Shape (`--test-threads=1`) | Fails | Note |
|---|---|---|
| full suite (32 tests) | **3 / 46** | 1/1 first run + 2/40 loop; all three at line 847, cycle 2 or 3 |
| R7 alone | 0 / 40 | matches the report |
| `hazard1_single_displaced_park` → R7 | 0 / 40 | an elided-free predecessor that does NOT amplify |
| `discarding_belt_racing_settle_never_double_owns` → R7 | 1 / 40 | |
| `discarding_op_undrained_supersedes_and_retires_detached` → R7 | **4 / 40** | the strongest single amplifier |
| both immediate predecessors → R7 | **5 / 40** | higher than the full suite |

The two immediate predecessors are the dependency — but not through state. Every lever guard in the suite (`LeverGuard`, `live_levers()`) resets `set_elision_class_all(false)` and the overlay levers on drop; the reclaim queue, the debt drainer and the block allocator are all per-router (per harness), the `BLOCK_FLUSH_LOCKS` / `INODE_META_LOCKS` stripes are held by nobody between tests, and R7 arms `set_elision_class_all(true)` itself. What the predecessors leave is **scheduling warmth**: their detached retires, reclaim workers and blocking-pool jobs leave the `sqz-meta` lanes and the `sqz-blk` pool spawned and hot, so the debt drainer's wake → `run_blocking(drain_debt_sync)` → claim latency shrinks into R7's own close-to-next-install window. A cold process (R7 alone) pays thread spawn on that path and the allocation always wins the race. That is why the amplification is graded (0 → 1 → 4 → 5 / 40) rather than a switch, and why no `--skip` bisect names a leaked word.

## 2. The interleaving, from the instruments

A temporary probe (not committed) captured `block_free_debt_pressure_drains`, `block_free_debt_drain_passes`, `block_free_trim_discards`, `block_free_elided_debt_bytes` and the allocator's free list immediately before the failing install, at the refusal, and 200 ms later (3 captures in 40 runs):

```
                 (pressure_drains, drain_passes, trim_discards, debt_bytes, free_list)
run 13, cycle 2  before install: (2, 11, 1, 49152, [])
                 at refusal:     (2, 12, 2, 49152, [2])
                 +200 ms:        (2, 12, 2, 49152, [2])
run 40, cycle 2  before install: (2, 11, 1, 49152, [])
                 at refusal:     (2, 11, 2, 49152, [2])
                 +200 ms:        (2, 12, 2, 49152, [2])
run 6,  cycle 3  before install: (3, 13, 2, 49152, [0])
                 at refusal:     (3, 13, 3, 32768, [0])
                 +200 ms:        (3, 14, 3, 32768, [0])
```

Read: in runs 13 and 40 the free list is **already empty** before the install starts and the trim discard count is one behind the cycle count — the drainer has CLAIMED the block cycle 2's close just freed and is punching it; by the time the refusal is observed the trim has completed (`trim_discards` +1) and the block is **back on the free list** (`[2]`), where it stays. In run 6 the block was on the list at the probe and the claim → punch → return all happened inside the install's own allocation attempt (`trim_discards` 2 → 3 across it, list `[0]` on both sides). Three of three refusals name a store with a free block in it.

Why the drainer runs at all mid-loop: the debt drainer's venue law is "foreground active + debt within the watermark ⇒ defer", where the watermark is `debt ≤ virgin tail` (KD-4.6). R7's store has capacity 3 with all 3 minted, so the virgin tail is **0** and every elided free's debt exceeds it: the PRESSURE venue drains on the very first pass after each close's free, regardless of foreground (`block_free_debt_pressure_drains` = cycles − 1 in every capture). That is by design — a full store's unreturned debt is its dominant thin exposure — and it is exactly the shape a real full volume under rewrite presents.

Why the allocation refuses: `try_allocate_block` scans the free list empty; `next_fresh_block` refuses at the cap; the grace ring is empty (unarmed mount) and there is no lane sink; the ENOSPC valve's `pending` reads the RECLAIM QUEUE, which the elided path never enters, so the valve drains nothing and the "nothing was owed" exit returns the second failed try; `allocate_block_grace_bounded` then asks `reclaimable_supply_exists()` — false — and the refusal is terminal. The trim's `return_from_trim` lands microseconds to milliseconds later. `drain_debt_sync`'s own doc said "claim windows are bounded to one batch so the ENOSPC valve is never starved by a long trim" — the valve was never TOLD about the window; only its length was bounded.

`tests/discard_elision_tests.rs` had already met this shape and stepped around it: its `pin_foreground` comment keeps the drainer "deferring forever (within-watermark stores …), so every assertion below races nothing" — a within-watermark (uncapped) store. R7 is the first capped-store elision test, so it is the first to hit the venue that does not defer.

## 3. Classification

**Product race.** The state the failing allocation observed (an empty free list, a cursor at the cap, nothing queued, no grace) is a legal intermediate state of the elision protocol on a full store, and the funnel's verdict on it was wrong. The prior tests only select the schedule; the same schedule is selected by any warm daemon on any full bdev-class volume under rewrite with elision engaged (default ON for bdev backings) — the write fails with ENOSPC, `write_enospc_refusals` grows, and `df` shows free space. A retry or a sleep in R7 would have hidden a field bug.

## 4. The fix

`src/block_allocator.rs`:

* `trim_claimed: AtomicU64` counts offsets inside a claim window; `claim_free_for_trim` increments **before** its `remove` (an allocation that scans the list empty and then reads the word must see the claim that emptied it) and decrements on a lost claim; `return_from_trim` inserts, then decrements, then `notify_waiters` on `trim_returned` (the insert precedes the release so a rescan after reading the window closed finds the offset).
* `await_trim_return` — the create-recheck-await idiom on `sqz_notify::Notify` (registers at creation; lost-wake-free by construction, ticked) bounded by `block_reclaim::PARK_BOUND_CEILING_MS` (a window is one batch of device commands; one that outlives the reclaimer's shipped park ceiling is a stalled trim and the attempt cap then owns the verdict). Counts `alloc_trim_window_parks`.
* `allocate_block_inner`'s ENOSPC arm: after each failed try, an open window is awaited BEFORE the valve and whether or not a valve is wired (a bare allocator has the same protocol); the valve's "nothing was owed" exit re-checks the window after its final try, since a claim can open during the drain. The attempt cap `ENOSPC_VALVE_MAX_ATTEMPTS` is unchanged and now bounds both arms; the exhausted-attempts log names the window.

Unchanged: `claim_free_for_trim` / `return_from_trim` semantics for the trim (KD-4.4 still claims out of the free list — a discard still never races a new owner's DMA), the queued-reclaim valve, `reclaimable_supply_exists` and the grace-bounded wall (a window is not grace; it is awaited inside `allocate_block`), every hot-path allocation (the word is read only after an empty scan on a full store).

## 5. Contracts

* **Red-first** — `tests/discard_elision_tests.rs` contract 9 `allocation_racing_a_trim_claim_window_parks_and_lands_on_the_returned_offset`: capacity 2, both minted, one elided terminal free (the allocator half of `free_block`'s elision arm verbatim), the claim window held by hand through the allocator's own `claim_free_for_trim`; a spawned `allocate_block` must PARK (`alloc_trim_window_parks` moves, the task is not finished), and on `return_from_trim` land on exactly the returned offset; a subsequent LOST claim parks nobody and a genuinely full store refuses promptly. Behavioural red confirmed by running it with the gauge present and `src/block_allocator.rs` stashed: `panicked at tests/discard_elision_tests.rs:664 — the allocation found the supply inside a claim window and PARKED … it never refused StorageFull` (it refused). Green with the fix.
* R7 itself is untouched (no retry, no sleep) and remains the stochastic sentinel.

## 6. Loop counts and engagement

| Shape (`overlay_overwrite_tests`, `--test-threads=1 -q`) | Before (tip `886d4e31`) | After (`7345ebf0`) |
|---|---|---|
| full suite ×60 | 3 / 46 | **0 / 60** |
| both immediate predecessors → R7 ×40 | 5 / 40 | **0 / 40** |

Engagement (a temporary `eprintln!` of the gauge at the end of R7, amplifier shape ×40, then reverted — `tests/overlay_overwrite_tests.rs` is byte-identical to `dev`): **`alloc_trim_window_parks` moved in 28 of 40 runs**, 0 failures. The window was open at an allocation's empty scan in most runs; pre-fix, most of those were rescued by the valve's off-thread `drain_off_thread` round trip on an EMPTY queue (a blocking-pool hop long enough for the trim to return before the "final try"), and the 12 % that were not were the flake. The park is now the explicit edge, not an accidental delay.

## 7. Suites and gates (this worktree's `target/`, `--release --all-features -- --test-threads=1`)

| Suite | Result |
|---|---|
| `discard_elision_tests` | 9 passed (contract 9 new) |
| `overlay_overwrite_tests` | 32 passed (×60 loop above) |
| `async_block_reclaim_tests` | 21 passed, 1 ignored |
| `derivation_sweep_tests` | 47 passed |
| `device_overlay_tests` | 10 passed |
| `f44_overlay_rewrite_tests` | 2 passed |
| `f48_warm_read_overlay_gap_tests` | 3 passed |
| `overlay_ack_early_tests` | 14 passed |
| `overlay_core_tests` | 19 passed |
| `overlay_length_floor_tests` | 7 passed |
| `overlay_settle_wait_tests` | 2 passed |
| `rewrite_shadow_supersede_tests` | 3 passed |
| `rewrite_shadow_supply_close_tests` | 8 passed |
| `rewrite_shadow_tests` | 8 passed |

`cargo fmt --check` clean; `cargo clippy --all-targets --all-features -- -D warnings` and `cargo clippy --all-targets -- -D warnings` clean; `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps` clean (after one private-intra-doc-link fix). Not run here: `task check` (the full gate), root/fleet rigs, the fuse3/loom/fuzz sub-gates — the change touches none of those crates.

## 8. Not claimed

* No field row: the dev box is scoping evidence only (venue rule 2026-09-07). The mechanism is a correctness law with no hot-path cost to measure — the new word is read only after an empty free-list scan on a store at its cap.
* The window park is bounded by the attempt cap (≤ 32 × 1 s) against a WEDGED trim (a device command that never returns) — that box is wedged on every other path too, and the honest `StorageFull` past the cap is logged loudly. Not exercised by a contract: a stalled device seam for the trim venue does not exist and the shape has no product schedule (the trim's ioctl is synchronous).
* The real drainer's pressure venue racing a real allocation is exercised only stochastically (R7's loop); the deterministic contract holds the same window through the allocator's own protocol, which is what `drain_debt_sync` runs between its claim and return phases.
* Whether the pressure venue SHOULD trim a full store's every displaced block one command at a time under an active rewrite (each is a device round trip the write path now waits on when it needs that exact block) is a pacing question, not answered here — the venue law (KD-4.5/4.6) stands as designed; the window is now honest supply rather than fullness.
