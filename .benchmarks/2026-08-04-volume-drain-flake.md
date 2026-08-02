# 2026-08-04 — volume_drain flake: the claim-release reclaim law

Fix-loop for the Z2 diligence filing: `volume_drain_tests::
test_offline_remove_data_drains_to_retired` failing ~2/4 whole-file
`--test-threads=1` runs on the untouched dev tip. Branch
`fix/volume-drain-flake` off dev tip `d806e5c`. Instrument: the
`volume_drain_tests` debug test binary run whole-file, dev box, thermal
law observed (`taskset -c 8-15`, `CARGO_BUILD_JOBS=8`, `nice -n 10`;
a sibling campaign's `cargo test` co-ran the whole window, loadavg ≈ 7–8).
Substrate: tempfile-backed volumes (in-process fixtures — no device
substrate; the corrupting device command is `fallocate(PUNCH_HOLE)` on
the file backing).

## 1. Signature (counted at dev tip `d806e5c`, from zero)

Whole-file runs, pinned 8 cores, `--test-threads=1`, ×10: **6/10 FAIL**
(runs 1, 6, 7, 8, 9, 10). Identical signature every failure, at
`tests/volume_drain_tests.rs:1319` (the post-offline-drain read-back):

* **exactly ONE 4 KiB block reads back all-zeros** where the expected
  `(b as u8) ^ 0x5C` fill should be;
* the zeroed block index ROTATES run to run (15, 15, 13, 3, 11, 3) —
  whichever moved victim block's destination collided;
* single-test runs pass (the Z2 note's "green standalone" observation),
  because the racy actor needs a co-resident successor custody holder in
  the same process lifetime and a schedule that defers the punch into
  the post-drain window.

(The Z2 note's "sibling-file bytes" reading was an artifact of eyeballing
truncated arrays — block-wise diffing shows pure zeros, i.e. a punched
hole, not another file's fill.)

## 2. Attribution (A/B counted; mechanism read out of the code)

**A/B:** `SQUEEZEFS_RECLAIM_BATCH_MS=600000` (parks the background
reclaim worker only; explicit drains unaffected): **0/10 fail** vs
**6/10** with the worker live. The corrupting device command is the
background reclaim worker's punch.

**Mechanism (test-hygiene defect with a product-law face):**

1. `striped_burst` displaces overwritten striped blocks → their terminal
   frees queue `PUNCH_HOLE` entries on the fixture router's
   `ReclaimQueue` (async block-reclaim, 2026-07-27).
2. The **manners law defers the worker** while foreground device bytes
   move — and the probe is the **process-global METRICS sum**, so the
   *offline coordinator's own mover traffic* (a different router!) holds
   the fixture's worker deferred, scheduling the punch INTO the exact
   post-publish quiet window. The law actively selects the corrupting
   schedule; CPU starvation widens it (the OQ-5/pug-wedge lineage:
   load selects schedules, it doesn't cause them).
3. `Fx::close()` released the meta claims **without draining the
   queue** — unlike the product dismount, which drains before custody
   release (`src/fuse_client.rs` dismount, "async block-reclaim
   conservation"). The queue then **outlives the fixture**: each queued
   `ReclaimEntry` holds `Arc<BlockAllocator>`, and each allocator's
   ENOSPC valve closure holds `Arc<ReclaimQueue>` — a non-empty queue is
   an immortal cycle, and its worker keeps ticking on the test runtime.
4. `remove_data_volume_offline` (the §5.8 D0-guarded coordinator, same
   process) recovers fresh allocators from durable layout maps:
   `recover_block`'s **gap-fill free-lists every unmapped offset** below
   the recovered highest — exactly the displaced offsets whose punches
   are still queued — and `try_allocate_block` prefers the free list.
   The mover republishes moved victim blocks onto those offsets
   (write → verify → publish, all clean).
5. The fixture's deferred worker wakes in the post-drain quiet and
   punches the reused offset → one block of durable zeros → the 1319
   read-back assert.

**Why this is not a live-product data-loss bug:** the cross-instance
overlap requires the prior custody holder's process to stay alive past
its claim release. Real mounts drain before release (dismount law); real
kill-9 takes the worker with the process (the documented crash posture:
un-punched thin space, re-covered on reuse). The D0 claims exclude
concurrent holders. The product already names the hazard class at the
fence-halt: *"a zombie's discard can land on offsets the successor
writer has reallocated."* The fixture violated the law the product
upholds — and the offline coordinator's error paths lacked the belt.

## 3. Fix (red-first)

**Red (commit `test(volume-drain)`)** — three deterministic contracts,
all red ×3 pre-fix (deterministic via the injected always-advancing
foreground signal — the manners-law deferral made schedulable — plus a
retained `Arc<BackendRouter>` as the late-engagement seam):

* `test_close_returns_queued_reclaims_before_custody_release` — close()
  must return-or-cease every queued device range before the claims
  release (per-block conservation: `queued ≡ punches + discards +
  skipped + fence_halts`). Red: `left: 0, right: 17`.
* `test_late_reclaim_after_close_cannot_zero_republished_blocks` — the
  flake's schedule verbatim: survivor-volume freed offsets with queued
  punches, close, offline remove-data, THEN the late engagement.
  Red: the field signature exactly (zeroed republished blocks).
* `test_crash_analog_ceases_device_reclaims` — kill-9 fidelity: a dead
  process never issues another device command; the backlog drops
  command-free into `block_free_reclaim_fence_halts`.
  Red: `left: 17, right: 0`.

**Green (commit `fix(volume-drain)`)** — fix class: **test-hygiene
(fixture fidelity) + small product belt**:

* `Fx::close()` — `reclaim_drain().await` before volume shutdown
  (mount-faithful: the dismount law).
* `Fx::crash()` — new `BackendRouter::reclaim_cease()` →
  `ReclaimQueue::halt_device_reclaims()` (the fence-halt latch exposed
  as the in-process kill-9/teardown analog), then a command-free drain
  that drops the backlog into `fence_halts` and breaks the keep-alive
  cycle.
* `src/config_ops.rs` `offline_drain_body` — belt: `reclaim_drain()`
  before every claim-release exit (terminal + paused-capacity). The
  Completed path was already covered by the mover's own pass drains
  (`src/jobs.rs` drain-arm); the belt makes the law hold on ALL
  terminal outcomes. No red exists for it by construction (empty-queue
  drain is a no-op); documented here instead.

## 4. Acceptance (multi-run discipline: counted from zero on the final binary)

* The three new contracts: green ×3 (and in every whole-file run below).
* `volume_drain_tests` whole-file (17 tests), `--test-threads=1`:
  **×10 pinned (`taskset -c 8-15`) 0 failures; ×10 unpinned 0
  failures** — vs 6/10 at dev tip under the identical harness.
* `cargo clippy --all-targets --all-features -- -D warnings` clean;
  `cargo fmt --check` clean.
* VL suite family green ×1 at `--test-threads=1`: `volume_drain_tests`,
  `defrag_tests`, `fsck_tests`, `fsck_repair_tests`,
  `interaction_tests`, `job_fabric_tests`, `job_wire_tests`,
  `volume_lifecycle_tests` (see the merge-gate record in the final
  commit).

## 5. The fuse3-zc campaign's other load-flakes — shared mechanism?

From `.benchmarks/2026-08-04-fuse3-zc-adoption.md`:

* `volume_drain_tests` (its pass-2 singleton) — **THIS mechanism**,
  fixed here.
* `multi_queue_tests::storm::test_single_queue_qdepth4_…`
  (`transport_parked_commits` 1–3 vs the exact-0 assert, 5/6 dev-tip
  fails under load) — **DISTINCT**: a transport commit parking when a
  reply races its payload-severance drop by microseconds. No reclaim
  queue, no custody handoff, no shared code with this fix. Follow-up
  candidate: either the assert is over-exact for a legitimately
  schedule-dependent gauge, or the park is a real economy leak — needs
  its own red-first loop on the fuse3 transport surface.
* `cli_clients_df_tests::test_df_answers_against_live_mounted_volume`
  (daemon teardown missed its exit timeout at loadavg ≈ 10) —
  **DISTINCT**: a wall-clock teardown wait (the banned wait-on-time
  class) on the CLI harness surface. Follow-up: wait on process state,
  not a fixed timeout.

**Same-class follow-up (not fixed here — copy-pasted, not shared,
code):** `tests/defrag_tests.rs` and `tests/interaction_tests.rs` carry
copies of this fixture with the pre-fix `close()`/`crash()`.
`defrag_tests` has a live crash→reopen shape (`fx.crash()` then
`open_fixture` over the same backings) — the identical hazard, one
deferred punch away from the same flake. Recipe: port this commit's
`close()`/`crash()` bodies verbatim (both routers expose
`reclaim_drain`/`reclaim_cease` now).
