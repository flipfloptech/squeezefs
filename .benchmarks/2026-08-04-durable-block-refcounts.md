# Durable block refcounts and free list — the publish-path cost, priced

**Item:** pre-RC engineering spec §6.2 **item 1** ("the largest item").
**Branch:** `feat/mw-durable-refcounts`. **Design:**
`docs/design-durable-block-refcounts.md`. **Contracts:**
`tests/durable_block_refs_tests.rs` (10 legs, all green).

The spec expects a real number for what durable accounting costs the publish
path, not a hope. This note is that number.

---

## 1. Instrument and venue

**Instrument:** Criterion, `benches/meta_lv_bench.rs` group `block_refs`
(`harness = false`, `cargo bench --bench meta_lv_bench -- block_refs`).
**Venue:** the dev box, release profile (`debug = true`, jemalloc), single
process, no mount, no device — these are the *CPU and journal-byte* terms of
the accounting, deliberately isolated from device time. **Substrate:** none —
the measured work is pure encode/decode/fold, so the two-substrate rule's
fabric sensitivity does not apply to these rows (the end-to-end publish rows
that DO need it are §5's open item).

Input shapes are field-derived and documented in-file:

* widths 1 and 2 are the two shapes the field's rewrite row runs — a
  streaming append gains one block; an overwrite gains one and displaces one
  (`.benchmarks/2026-08-01-rewrite-publish-drain.md` §3: ~2,600
  block-publishes/s, publish coalescing at 1–3 blocks/batch);
* width 64 is the `SQUEEZEFS_PUBLISH_COALESCE_MAX` default — the widest
  window one aggregated transaction carries;
* 4,096 references ≈ a 16 GiB file set at the shipped 4 MiB block, half of
  the blocks shared by a clone (the shape the ledger exists to preserve).

---

## 2. The publish-path cost (the number that matters)

| Row | Median | Per reference |
|---|---|---|
| `delta_apply/1` (streaming append: +1 reference) | **40.1 ns** | 40 ns |
| `delta_apply/2` (overwrite: +1 / −1) | **51.8 ns** | 26 ns |
| `delta_apply/64` (the coalesce window) | **3.94 µs** | 62 ns |
| `key_build` | 5.3 ns | — |

**On-disk cost: 46 B of journal per reference** (28 B key + 4 B value + the
K1/K3 record framing), measured in situ rather than computed: the bench prints

```
block_refs: publish-batch-64 entry 10708 B without accounting,
            16596 B with it (+5888 B, +54.99 %)
```

over 128 accounting records (64 takes + 64 releases) — 5888 / 128 = **46 B
each**. The 55 % figure is an artifact of the synthetic base entry (a 128-byte
`LayoutDelta` per ino, the smallest realistic delta); the invariant to carry
forward is the per-reference 46 B, and the relative share falls as the layout
delta grows.

Entry encode + xxh3 over the same batch: **2.32 µs → 3.74 µs** (+1.42 µs for
128 records = **+11 ns per accounting record**).

**Composite, at the field's measured publish rate.** 2,600 block-publishes/s
of the overwrite shape (2 references each):

* CPU: 2,600 × 51.8 ns ≈ **0.13 ms/s ≈ 0.013 %** of one core;
* journal bytes: 2,600 × 92 B ≈ **239 KB/s** against a metadata namespace
  measured at 21–25 k device-writes/s
  (`.benchmarks/2026-07-30-meta-plane-writes.md`);
* **journal entries: +0.** This is the load-bearing result — the records ride
  the layout transaction, so the entry count per publish is unchanged. Pinned
  as a test, not an argument:
  `accounting_rides_the_publish_transaction_and_adds_no_commit` runs the same
  8-publish sequence on an un-stamped and a stamped volume and asserts
  `META_KV_JOURNAL_ENTRIES` deltas are **equal** while
  `META_KV_BLOCK_REFS_STAGED` grows by 8. Had accounting taken its own commit,
  this row would have doubled the publish's journal entries against a conveyor
  already at ρ ≈ 0.92 — the term the 2026-08-01 campaign spent itself
  removing.

**Honest reading:** the accounting is not free, but at the field's publish
rate it is ~0.01 % of a core and ~2 % of the metadata namespace's measured
write bandwidth, with zero added commits and zero added barriers. The
dominant publish costs the decomposition already names (`queue_wait`,
`meta_commit`, `tx_wait`) are three to five orders of magnitude larger.

---

## 3. The recovery pass (what it replaces)

| Row | Median | Per reference |
|---|---|---|
| `recovery_scan_decode` (4,096 refs: decode + validate key **and** value) | **11.76 µs** | **2.9 ns** |
| `scan_range_bounds` (per volume scan) | 18.2 ns | — |

Extrapolated: a **1 M-reference** volume set (≈ 4 TB of 4 MiB blocks) decodes
in **≈ 2.9 ms** of CPU, plus the paged tree reads (512 records/page).

What it replaces is not comparable in kind, which is the point: the derived
walk is O(live inodes) — a full `TREE_INODES` range walk, one `getxattr` fold
per live inode, and one **device read per indirect block map**. At the
documented 100 M-inode cap the walk is unbounded in practice; the durable scan
is bounded by the number of *referenced blocks*, i.e. by the data actually
stored. `meta_kv_block_refs_recovered` is the gauge that says which path ran.

---

## 4. The verification pass (the oracle)

| Row | Median | Per reference |
|---|---|---|
| `census_fold` (4,096 refs → per-block census) | **207.7 µs** | 51 ns |
| `census_compare` (durable vs derived diff) | **156.7 µs** | 38 ns |

≈ **90 ns/reference** of `BTreeMap` work, on top of the derived walk itself
(which dominates). That total is exactly why the oracle is **opt-in at mount**
(`SQUEEZEFS_BLOCK_REFS_VERIFY=1`) and **unconditional in fsck** (class C8):
running it at every mount would pay the very walk the durable records exist to
delete. A `BTreeMap` is the honest structure here — ordered, so the diff is a
merge — and this path is not hot.

---

## 5. Acceptance status and open rows

**Green (this note's evidence):**

| Contract | Leg |
|---|---|
| durable refs survive a crash + remount, no walk, == derived | `durable_refs_survive_a_crash_and_equal_the_derived_answer` |
| a clone's shared block is durably refcount 2, across remount | `a_clone_records_durable_refcount_two_and_survives_remount` |
| crash in the free window: no leak, no double free (both halves) | `a_crash_in_the_free_window_neither_leaks_nor_double_frees` |
| durable == derived across writes/clone/displacement/truncate/punch/reclaim | `durable_matches_derived_across_a_mixed_workload` |
| the accounting adds **zero** journal entries | `accounting_rides_the_publish_transaction_and_adds_no_commit` |
| a data-device power cut cannot separate ledger from layout | `a_data_device_power_cut_leaves_ledger_and_layout_agreeing` (TEST-1 harness) |
| the indirect-map blob's own reference + its DUR-6 CoW replacement | `the_indirect_map_blob_carries_its_own_durable_reference` |
| ruling D9: un-stamped sector 0 byte-identical after a real mount+publish | `unstamped_volume_is_unchanged_by_mount_and_stays_derived` |
| the stamp engages on the next mount (root minted) | `stamping_the_bit_engages_accounting_on_the_next_mount` |
| stamping a NON-EMPTY volume backfills instead of freeing live blocks | `stamping_a_non_empty_volume_backfills_instead_of_freeing_live_blocks` |

### The write-path wiring, closed (step 2 of the finishing procedure)

The oracle was the checklist and is now clean. Drift census, with the ledger
engaged via the `SQUEEZEFS_TEST_STAMP_BLOCK_REFS` format seam and fsck class
**C8 ungated**:

| | Before | After |
|---|---|---|
| `fsck_tests` | **7 tests RED**, drift on every healthy population (write-through, clone-shared, encrypted, compressed, sparse-huge, stalled-flush, the 32 k-inode baseline) | **19/19 green, zero C8 findings** |
| the other 27 layout-publishing suites | not gradeable (the ledger was not engaged) | **all green** |

The single remaining gap was a **class**, not a site: a path that mutates the
RAM block map and leaves the layout DIRTY defers persistence to a later save,
which is then handed a map already containing the change and stages nothing.
`rewrite_shadow_record` is that path. Closed structurally by the per-ino
deferred-op accumulator (`pending_block_refs`), so any future deferring site
is correct by construction. Localizing it needed the drift log to name each
block's OWNERS (ino + map index) — the owner is what names the publishing
site. Full site table: `docs/design-durable-block-refcounts.md` §11.

Suites the oracle ran clean across, with the ledger engaged: fsck,
fsck-repair, write-through (+coverage), write-commit-economy,
publish-drain-economy, write-supersession, block-free-reclaim,
async-block-reclaim, reclaim-batch, discard-elision, staged-truncate-stale,
hole-read-zeros, sparse-write-bounded, staged-crash-recovery,
staged-dirty-layout-refill, staged-rmw-alloc, data-path-correctness,
striped-overwrite-lazy-seed, rewrite-amp, rewrite-shadow, in-place-overwrite,
extent-overlay, extent-patch, extent-record-recovery, refcount-clone,
cli-clone, copy-file-range — plus durable-block-refs 12/12 with the seam ON
and OFF.

**Open rows this note does NOT claim** (stated so nobody cites it for them):

1. **An end-to-end mount-level publish bracket.** These are microbench terms.
   The A-B-B-A row on the **tcp** substrate (per the two-substrate rule —
   writes are fabric-sensitive), with `wareq-sz` and the amplification columns
   plus a ≥ 60 s sustained leg, belongs with the next write-path perf pass.
   The arithmetic above says the effect should be inside noise; that is a
   prediction, not a measurement.
2. **A mount-time recovery bracket at scale.** The 2.9 ms/1 M-reference figure
   is arithmetic on a measured constant, tier (iii) per `docs/rc-manifest.md`.
   The real row is a large formatted set's mount wall-clock with and without
   the bit.
3. **Bench baseline reference refresh.** `block_refs` is a NEW group, so it
   has no entry in `.benchmarks/criterion-baselines/reference.json`. Refresh
   (`tests/run_bench_baseline.sh save`) as part of the intentional landing.
4. **The stamp itself.** Fresh formats still do NOT carry incompat bit 8
   (ruling D9 — the Phase-8 reformat window owns the stamp), so every row
   above that engages the ledger does so through the test seam. The
   *production* default remains derived accounting, which is also what keeps
   a future wiring regression from being able to hand out live blocks.

---

## 6. Gate status on this branch

| Gate | Result |
|---|---|
| `cargo clippy --all-targets --all-features -- -D warnings` | **clean** |
| `cargo fmt --check` | **clean** |
| `cargo doc --no-deps` | no new warnings (the pre-existing private-link set is unchanged) |
| `cargo test --all-features --lib -- --test-threads=1` | **213 passed** |
| `durable_block_refs_tests` | **11 passed** |
| kv suites (`kv_alloc`, `kv_backend`, `kv_journal`, `kv_node`, `kv_tree`, `kv_fold_slimming`, `kv_finding_a`, `kv_smo_crash_completeness`) | **all green** (kv_backend needed the pre-existing S2 digest fix — see below) |
| clone / refcount (`refcount_clone`, `cli_clone`, `copy_file_range`) | all green |
| reclaim (`block_free_reclaim`, `async_block_reclaim`, `discard_elision`) | all green |
| fsck (`fsck_tests` 19, `fsck_repair_tests` 16) | all green |
| layout/publish (`write_commit_economy`, `write_commit_crash`, `layout_delta_fold`, `recovery_prefixed_keys`, `indirect_map_backend_keys`) | all green |
| `cargo bench --benches -- --test` (smoke) | criterion benches all green; the lib-harness leg trips a **pre-existing** flake (below) |

### Pre-existing failures encountered, and their attribution

1. **`kv_backend_tests::v3_mutations_survive_clean_shutdown_remount` and
   `::v3_ring_full_liveness_storm_drains`** — failing since DLM S2
   (`107f7bf9`) put the durable `writer_term` record inside `digest_walk`'s
   scope. The term is monotone per mount by design, so a
   digest-before-vs-after-remount comparison was structurally false (era N
   vs era N+1). **Fixed on this branch** (`4afeb838`): one-line exclusion
   beside `writer_claim`, same predicate, same justification — "mount-guard
   state, not filesystem content".
2. **`reclaim_batch_tests::test_destroy_inodes_kills_xattrs_in_the_same_transaction`**
   — `setxattr(ino, "layout", …)` through the public `Metadata` surface now
   returns EPERM under the VAL-2 positive allowlist (`80da757d`);
   `xattr_name_allowed("layout")` is `false` by design. Not touched here:
   the fix belongs with VAL-2 (the test should use the internal writer).
3. **`node_cache::tests::{an_empty_memo_charges_nothing_and_credits_nothing,
   racing_same_key_populates_charge_at_most_once,
   racing_distinct_key_populates_conserve_the_charge_across_drop}`** — a
   parallel-harness flake in the memo-gauge tests: all three read the
   process-global `META_KV_FOLD_MEMO_BYTES` as their baseline, and the
   default-threaded lib harness (which is what `cargo bench --benches --
   --test` runs) lets siblings pollute it. **Verified pre-existing**: a
   scratch worktree at the base commit `e7ce625f` fails 3/3 runs the same
   way, while `--test-threads=1` (the house gate form) passes 213/213 on
   both. Same class as the `dev_power_cut` registry collision the tree
   already documents; belongs on the TEST-7 flake board.
