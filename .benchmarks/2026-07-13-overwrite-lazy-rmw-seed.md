# Item B — the overwrite lazy-RMW seed (row-4 write-path gap)

**Branch:** `perf/overwrite-lazy-rmw-seed` off dev@b945a4d.
**Commits:** `2587a7a` (red suite) → `bae38bc` (deferral mechanism) → `fa5fcc4` (unstable-incarnation contract moved to the item-B surface, RED) → `b916ace` (never-lossy custody across a refused materialize) → `8319990` (overlay-never-invisible red tests — the QUICK 075/112 transient) → `9ffc1b8` (fetch-visible / fill-sync / stage-then-remove).

## 1. The gap and the mechanism

Sequential 1M overwrites of EXISTING striped files ran ~4x slower than fresh
creates (PR 1 baseline row 4; original elbencho row 4: 870 vs 2,487 MiB/s).
Attribution (`.benchmarks/2026-07-11-elbencho-odirect-read-smallblock-attribution.md`
H3): the FIRST write into each existing block RMW-seeded eagerly —
`block_write_needs_existing_data` → `get_block_for_index` — fetching the old
4 MiB block from the device even though a sequential stream fully covers the
block before its write-through. A pure 16 GiB overwrite performed ~17.8 GiB
of device READS.

**Fix (defer to the last responsible moment):**

- The checkout arm that eagerly seeded now parks a
  `ActiveBlockBuf::deferred(block_size)` — `deferred_seed: bool` rides the
  existing coverage machinery (`covered` interval, §5.3 memset elision), no
  new atomics, all mutation under `BLOCK_FLUSH_LOCKS`.
- `complete_coverage` (sequential stream reaches full block coverage) clears
  the deferral: the seed read is **skipped entirely** — counted
  `overwrite_seed_skipped`. The write-through then proceeds exactly as
  before (`zero_complete` → `upload_full_block`), preserving the
  1-userspace-copy + 1-DMA contract of docs/design-zero-copy-write-path.md
  (only a read was removed; no copy or staging detour added).
- Every other escape path **materializes first** via
  `materialize_deferred_seed` — the verbatim binding-validated fetch the
  eager seed used (metadata re-fetch → `block_map`/`block_map_id` →
  `get_block_for_index` single-flight validated fill under the incarnation
  seqlock; hole rebind ⇒ zeros — the block IS a hole now), just moved in
  time: gap write, partial-coverage write-through trigger, fsync stage loop,
  RAM-cap spill, Red parked-gate self-flush, dismount teardown, and sparse
  reads overlapping the uncovered complement (the reader pays the read the
  writer deferred). `fill_complement_from(old)` copies only the uncovered
  head/tail and zero-fills past the old block's length — counted
  `overwrite_seed_materialized`.
- `debug_assert!(!deferred_seed)` guards `zero_complete` and the
  `record_write` gap-degrade arm: any future path that forgets to
  materialize fails loudly in debug/test builds instead of codifying zeros.

**Never-lossy custody (b916ace):** a REFUSED materialize (e.g. the leg-5
unstable-incarnation refusal in `get_block_for_index`) keeps the ACKed bytes
in RAM custody at every site — fsync's stage loop re-parks + propagates
(delayed-write error; a healed retry flushes the SAME preserved bytes), a
gap write re-parks the old covered bytes unmerged and fails only ITSELF, the
write-through trigger parks instead of writing through (staging-refusal
precedent — the write ACKs, fsync surfaces the error), the read serve
materializes on a checked-out buffer (remove/own/re-insert under the block
lock — never a dashmap guard across an await), spill/Red-gate/teardown keep
their park/skip semantics. Pinned by the re-choreographed
`test_rmw_seed_fill_must_not_publish_unstable_incarnation`.

**Counters (stats inode `metrics.*`):** `overwrite_seed_deferred` /
`overwrite_seed_skipped` / `overwrite_seed_materialized`.

**Overlay never invisible (9ffc1b8) — found by the QUICK tier doing its
job:** the first QUICK ×3 roll hit generic/075 twice (and a later roll
generic/112, fsx `-A`) with a signature no unit test had: `.bad` ≡ `.good`
— the durable state was NEVER wrong; a read served transient stale/zeros
and the file healed itself. A/B pinned scope (base ×3 = {003,213} only),
standalone 075 ×6 + tape replays ×5 passed (needs the aged-volume QUICK
context), and a debug-assertions build refused to reproduce (timing).
Root cause: the deferral-window materialize sites took the parked buffer
OUT of the map across the seed fetch (a device-read await) — concurrent
single-block FUSE reads (kernel readahead crossing a CFR/ZERO page-cache
invalidation boundary — 112's mismatch split exactly at 0x41f000) probe
the overlay lock-free, missed it, and served the backend's pre-merge
bytes. fsync's stage exit (remove-then-stage) had the same window
PRE-item-B (its red test fails on dev@b945a4d too) but item B widened it
from a µs staging put to a ms device read, which is what surfaced it.
Fix: `fetch_seed_image` (the awaited binding-validated fetch) takes no
buffer; every exit keeps the buffer PARKED across every await, fills
synchronously under the held block lock via short-lived map guards, and
transfers authority STAGE-THEN-REMOVE — at every instant one readable
copy exists, and whenever two exist they are identical. Custody on a
refused fetch became trivial (the buffer never leaves the map). Pinned by
`concurrent_reads_never_lose_the_overlay_during_{materialize,flush}`
(the first = item-B regression, green on base; the second = pre-existing,
red on base, item-B-widened — both green at the tip).

## 2. Red suite (committed first, `2587a7a`; test 1 verified RED pre-fix)

`tests/striped_overwrite_lazy_seed_tests.rs` (BS=64 KiB; fixture builds a
6-block durable striped file then purges write_lru/read_lru + the NVMe read
tier for every mapping so the `get_obj` ledger is honest):

1. `full_coverage_overwrite_reads_nothing_and_is_byte_exact` — get_obj
   delta == 0 across a full sequential overwrite + byte-exact after fsync
   (**RED pre-fix: 6 device reads**).
2. `partial_coverage_seeds_old_bytes_at_flush` — uncovered remainder is the
   OLD bytes after flush, byte-exact.
3. `truncate_inside_deferral_window` — down into a deferred block,
   re-extend: zeros + patch exact.
4. `punch_inside_deferral_window` — partial-edge and whole-block punches
   mid-window.
5. `cfr_inside_deferral_window` — copy_file_range source & dest blocks
   deferred mid-window.
6. `reads_during_window_serve_merged_content` — overlay authority for
   interior/prefix/straddle/whole-block reads during the window.
7. `fsync_forces_merge_durably` — cold re-read after tier invalidation.
8. `crash_inside_window_leaves_old_block_intact` — two sessions over the
   same meta+backing: an unfsynced deferred patch is dropped by a crash and
   the old durable block is fully intact (write-through ordering — nothing
   acked-durable lost, no torn seed).

Plus the moved leg-5 contract (`tests/data_path_correctness_tests.rs::
test_rmw_seed_fill_must_not_publish_unstable_incarnation`, `fa5fcc4`): write
ACKs deferred → fsync fails LOUD over a never-settling incarnation → no
tier publish → ACKed bytes still served from custody → owner publishes →
the SAME preserved bytes flush durably with no application re-write (was
RED against `bae38bc`: the refused flush dropped the parked buffer).

## 3. Row 4 before/after with ledger proof

Substrate: file-backed sandbox (`~/tmp/owlazy`, 2G meta + 24G data loop
files, staging dir), caged daemon (8G MemoryMax), taskset 0-15, CPU capped
3.5 GHz. elbencho `-w -b 1m -t 16 -s 1g --direct` over the same 16 files;
`get_obj` = device block reads from the stats ledger. Absolute MiB/s on
this substrate is loop-file-bound (not the historical NVMe rows); the
ledger and the overwrite:create ratio are the transferable evidence.

**Paired, order-controlled, cold page cache before the overwrite pass**
(`sync; echo 3 > drop_caches` so BEFORE's seed reads hit the backing device):

| Pass | BEFORE (dev@b945a4d) | AFTER (b916ace) | AFTER (final tip 9ffc1b8) |
|---|---|---|---|
| Row 1 create 16×1G | 1,648 MiB/s | 1,735 MiB/s | 1,563 MiB/s |
| Row 4 overwrite (existing) | 1,531 MiB/s | **1,826 MiB/s** | **1,667 MiB/s** |
| overwrite : create ratio | 0.93× | **1.05×** | **1.07×** |
| **get_obj delta during row 4** | **4,083 reads (≈16 GiB)** | **1 read** | **2 reads** |
| overwrite_seed_deferred | n/a | 4,112 | 4,112 |
| overwrite_seed_skipped | n/a | **4,096** (= exactly 16 GiB / 4 MiB) | **4,096** |
| overwrite_seed_materialized | n/a | 16 (file-tail partials) | 16 |

Device-read bytes for fully-covered blocks: **~0** (1-2 stray reads across
4,096 blocks; the acceptance ledger goal). Same protocol without the
cold-cache step reproduces identically (get_obj delta 4,082 → 1-2 across
five runs this session, two of them on the final tip).

**Honest recording on the ≥2,000 MiB/s target:** on this loop-file sandbox
the write path saturates at ~1.6-1.8 GiB/s for CREATES too, so the absolute
target is substrate-bound, not seed-bound. The structural gap item B owns —
overwrite pays the create price plus one old-block device read per 4 MiB —
is **closed on the ledger** (4,083 → 1 reads) and overwrite now runs at
**1.05-1.19×** create throughput (paired: 1,826 vs 1,735; same-session
uncontrolled runs: 1,745-1,842 vs 1,559-1,683). On the historical NVMe rows
where the seed reads were the measured 4x (870 vs 2,487-4,100), removing
17.8 GiB of device reads from the pass is the whole attribution (H3); the
remaining delta there, if any, needs re-measurement on that hardware.

## 4. Rows 1/2/3/5

Same-session, same substrate (uncontrolled tier warmth on read rows):

| Row | BEFORE (session baseline) | AFTER (final tip 9ffc1b8) | Verdict |
|---|---|---|---|
| 1 create 1M seq | 1,353-1,648 | 1,563-1,735 MiB/s | flat-to-up |
| 2 seq read 1M | 1,053 | 3,897-4,918 MiB/s | improved (side-effect: overwrite pass no longer floods the read tiers with 16 GiB of seed blocks, so the read rows start from honest tier state; not claimed as an item-B win) |
| 3 rand-4k read | 9,888 IOPS | 52,279-83,787 IOPS | improved (same tier-state effect) |
| 5 rand-4k write | 131-140 IOPS | 113-150 IOPS | **flat-to-up** — partial blocks still seed, now at flush: row-5 pass deferred 3,207 / materialized 3,207 / skipped 0 (the mandated behavior; the RMW read moved, it did not disappear) |

## 5. Acceptance evidence (this session, final tip 9ffc1b8)

| Gate | Result |
|---|---|
| Red suite (10 scenarios) | 10/10 green (test 1 RED pre-fix: 6 reads; tests 9/10 RED pre-9ffc1b8 — test 9 also green on base = item-B regression pinned, test 10 red on base = pre-existing pinned) |
| Moved leg-5 contract | RED against bae38bc → green from b916ace on |
| Full serial cargo gate | **686 passed / 0 failed** (`--all-features -- --test-threads=1`) |
| clippy -D warnings / fmt | clean |
| cargo doc --no-deps | 0 warnings |
| bench smoke | 118 ok |
| loom | 19/19 (flush-unit atomics unchanged — `deferred_seed` is a plain bool mutated only under BLOCK_FLUSH_LOCKS; run anyway) |
| legs-1..5 suites | all green in the serial gate (ring_pressure 6, aba 2, refill 4, truncate_stale 6, identity 7, crash_recovery 7, rmw_alloc 1, hole_read_zeros 7, write_visibility 9, writeback 9, write_through 28, cfr 5, data_path 24, sparse 5) |
| Aged fsx protocol | **3/3 CLEAN** on the final tip (fsstress age -n 30000 -p 8 + fsx -S 0 -U -N 10M -p 100000 -o 128000 -l 600000 --duration=120 ×3, caged) — leg-5 finish line does not regress (also 3/3 on b916ace earlier) |
| QUICK ×3 | **{003,213}-only ×3** on the final tip (count restarted post-9ffc1b8 per the multi-run directive; the pre-fix roll that caught 075 ×2 + 112 ×1 is §Overlay-never-invisible) |
| trio 616/075/091 + 074 | green inside every QUICK run (075 was the caught-and-fixed transient) |
| kill9 deep churn | SQUEEZEFS_CRASH_ROUNDS=60 test_kill9_remount_soak_v3: ok, replay 0 dropped every round |
| unmount kill soak | PASS — 30 cycles, 0 coredumps / 0 SIGABRT / 0 panics |
| LTP | **174 PASS / 0 FAIL / 0 BROKEN** (9 skipped — standing conf skips) |
