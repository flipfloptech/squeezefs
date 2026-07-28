# 2026-07-28 — `test_small_block_map_stays_inline` "72 vs 71" flake: root cause + fix

**Status:** closed (harness race; test-side rendezvous fix). Branch `fix/writeback-inline-flake` off dev `9d6a680`.
**Verdict:** HARNESS, not product. No product change; no counter/path blast radius beyond test counting discipline.

## The flake

`writeback_tests::test_small_block_map_stays_inline` failed intermittently at the MID-test
used-blocks assert (`tests/writeback_tests.rs:387` pre-fix): `used_blocks == 72`, expected 71.
Observed twice in the field (op-economy from-zero gate roll; untracked dev baseline `22c31ac`,
1-of-3). Reproduced here on dev `9d6a680`:

- **Firing rate (pre-fix):** 4/30 solo rolls (~13 %), debug build, `--test-threads=1`, test run
  ALONE — so not inter-test state leakage. Every failure identical: `left: 72, right: 71` at the
  mid count. Instrument: the test binary direct, nice -n 19, loaded shared box.
- **Captured failing tape (`RUST_LOG=debug`):** 72 `allocate_block` lines, ONE
  `router free_block: key=4194304` + `begin_free terminal: offset 4194304` with **no matching
  `finish_free`** before the count. A passing run's tape shows the IDENTICAL free with its
  `finish_free` landing before the count — the free itself is deterministic; only the drain
  timing raced.

## Mechanism

1. The initial 4097-byte striped-transition write publishes chunks for **both** blocks it touches
   on the direct striped path: block 0 (offset 0) and block 1 (offset 4194304, seeded from the
   1-byte tail).
2. The 70 subsequent 1-byte writes park as overlays (block 1's is an overlay on a *mapped* block).
3. `force_flush_all_staged_data`'s fold of block 1's parked overlay is write-before-publish CoW:
   it allocates a fresh chunk, publishes, and **displaces + terminally frees offset 4194304**.
4. Since the 2026-07-27 async block-reclaim campaign (`src/block_reclaim.rs`,
   `.benchmarks/2026-07-27-async-block-reclaim.md`), that terminal free takes `begin_free` inline
   but its device reclaim + `finish_free` ride the background `ReclaimQueue` (2 ms accumulation
   window, worker batch).
5. `BlockAllocator::get_used_blocks() = highest_block − free_list.len()` counts a
   begin_free-limbo offset as **used** — by design (the offset is not reallocatable until after
   its reclaim).
6. The test's END count already rendezvouses (`fs.router.backend_router.reclaim_drain().await`,
   added by the same campaign). The MID count did not — it raced the 2 ms batch window.
   71 live blocks + 1 begin_free-limbo block = 72.

**Deterministic proof (schedule forcing, not statistics):** parking the harness's reclaim worker
via the module-documented test lever `SQUEEZEFS_RECLAIM_BATCH_MS=600000` (read at router
construction) made the pre-fix test fail **5/5 with exactly 72**; with the fix it is green
under the same parked worker — red-without/green-with, every run.

## The fix (test-only, two commits, red-first)

- `test(writeback)` **2782272** — park the harness's reclaim worker at router construction so the
  mid-count race is a deterministic contract (no drain ⇒ fail every run, never 1-in-N again).
- `fix(tests)` **38efec6** — the same `reclaim_drain` rendezvous the end count already has, now
  before the mid count. No sleeps (`drain_sync` waits out in-flight batches by contract).

## Blast-radius survey (counting `get_used_blocks` near async frees)

- Same file: `test_block_allocator_recovery` (==5) writes each block exactly once — no
  displacement, no free, not exposed. `test_stale_token_writeback_adopts_current_epoch_no_leak`
  uses `used ≤ mapped + 1` — tolerates one limbo free by construction.
- Other suites (`phantom_backend0_tests`, `indirect_map_backend_keys_tests`,
  `recovery_prefixed_keys_tests`, `volume_drain_tests`, `placement_tests`) count on fresh
  allocators/recovery walks or after explicit drains; no same-shape mid-count found. Standing
  rule worth keeping in mind: **any `get_used_blocks()` assert downstream of a displaced/terminal
  free must `reclaim_drain` first** (begin_free limbo counts as used).

## Acceptance (counted from zero, post-fix binary)

- Fixed test **green ×30 consecutive** solo (`--test-threads=1`), 0 aborts, 0 restarts.
- Whole `writeback_tests` file **×5 single-thread (file order)** + **×5 default multi-thread
  harness**: all green (test-independence check).
- Full gate (nice'd, from zero): `cargo clippy --all-targets --all-features -- -D warnings` clean;
  `cargo fmt --check` clean; `cargo test --all-features -- --test-threads=1` green;
  `cargo doc --no-deps` clean; `cargo bench --benches -- --test` smoke green.
