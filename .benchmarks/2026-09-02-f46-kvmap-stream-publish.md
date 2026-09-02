# Finding 46 — the kvmap streaming-extend publish collapse (2026-09-02)

**Venue:** squeeze-test — 5 storage nodes over nvme-tcp (memory-backed
NVMe targets: 5 meta namespaces + 10 data namespaces, `cluster_reset_v4`
converged shape), 32-core client, **cacheless mount with
`--interception`**. **Instrument:** `/scratch/tmp/fio_jobs/write_BW.job`
(24 jobs × 8 GiB, sequential 1 MiB libaio `direct=1`, iodepth 16,
`time_based` 30 s + 10 s ramp; kernel FUSE path — the il row was
IDENTICAL on the baseline), `.stats` snapshots pre/post, per-job bw logs
at `log_avg_msec=1000`. Baseline row artifacts:
`/scratch/tmp/e2e-baseline-20260902-145345/kern.w_fresh.*` (binary
`aecf1561`); after rows: `/scratch/tmp/f46-20260902-161150/`,
`/scratch/tmp/f46acc-20260902-164056/`, `/scratch/tmp/f46acc-20260902-164304/`
(binary `08130de3`, `task build:rocky8`).

## Attribution (from the baseline row's stats deltas)

| instrument | baseline value | reading |
|---|---|---|
| crossings | 24 at t+7 s (log), 1,484–1,498 record ops each, `mode=whole-map` | all files entered the tree inside the ramp |
| `layout_publish_batches` = `batched_blocks` | 38,587 | one publish per 4 MiB block, no coalescing |
| `meta_kv_block_map_lookup_range` | 13,037 pages over ≈2,830 post-crossing publishes | **≈4.6 pages ≈ the ino's whole population per publish** (chunk 512) |
| `meta_kv_block_map_lookup_exact` / `_floor` | 0 / 0 | no claims-mode probe ever ran (no partial inos) |
| `layout_delta_commits` | 35,201 = 24 × ~1,467 | the PRE-crossing inline deltas — H3 falsified |
| journal bytes per post-crossing publish | ≈300 B | the journal side was already O(window) |
| `publish_phase_ns.total` ≥ 128 ms | 2,979 samples (554/1,245/983/197 at ≤128/256/512/1024 ms) | ≈ the post-crossing publish count; mean ≈275 ms |
| publish venue | `spawn_meta` — the 2-thread `sqz-meta` pool | O(map) CPU per publish serialized ACROSS the 24 inos |

Arithmetic: 24 inos × (4 blocks in flight / ≈275 ms per serialized
publish) ≈ 90 blocks/s ≈ 0.36 GiB/s — the row's mean exactly; 4 queued
publishes ≈ the 986 ms clat.

## Root cause

Every save of a sticky `kvmap:` head ran Rev 1.3 #2's whole-map
delete-by-absence diff (`claims: None` in `migrate_block_map_train`):
clone the RAM map, encode every entry, `block_map_range` the whole
population, decode it. The 6c-i bounded-probe fix applied to the
overlay (partial) claims train only. Fix: `MapTrainClaims::window` — the
publish-class save of a whole-map kvmap ino ships only the window's
bindings as take claims, exact-only probes, frame verbatim (design §18).

## In-process red/green (`tests/f46_kvmap_stream_publish_tests.rs`)

Two interleaved sequential writers (one allocator — the field's
point-dominated maps; a lone writer run-collapses to ONE record), 256-block
extend window per ino after the crossing:

| | tip `dafa82ca` | fix `08130de3` |
|---|---|---|
| publishes / blocks | 511 / 511 | 508 / 511 |
| exact lookups | 0 | 511 |
| range pages / records | 1,305 / **268,914** (≈526 = the map per publish) | 0 / 0 |
| `kvmap_window_saves` | — | 508 (= publishes) |

## Field before/after (the standard `write_BW.job` row, fresh reset each)

| row | GiB/s | clat mean | clat p99 | crossings in window | exact / window publish | range pages post-crossing | publish tail ≥ 128 ms |
|---|---|---|---|---|---|---|---|
| baseline `aecf1561` 14:54 | **0.36** | 986 ms | 2,433 ms | 24 | 0 (whole-map) | 13,037 | 2,979 |
| f46 `08130de3` 16:11 | **32.76** | 11.26 ms | 48.5 ms | 24 | 1.00 (13,349 / 13,349) | 0 (window) | **0** (max bucket 64 ms, 19 samples) |
| f46 16:40 | 32.72 | 11.33 ms | 47.4 ms | 24 | 1.00 | 0 | — |
| f46 16:43 | 32.67 | 11.32 ms | 48.0 ms | 24 | 1.00 | 0 | — |

Per-second aggregate bw (run 1): `16.0 22.3 30.3 33.7 34.7 33.6 32.2 34.3
31.8 30.7 33.4 33.9 32.1 35.2 32.2 33.7 32.7 33.4 34.5 31.3 33.1 34.8
32.3 34.3 31.0 32.7 34.9 32.0 34.6 31.6 29.2`. Flatness: thirds
30.0 / 33.2 / 32.7 GiB/s (first-vs-last 8.5 %, 9.5 %, 8.6 % across the
three rows) — the first two log samples are the bw log's partial-interval
artifact at the ramp boundary (a 9–16 GiB/s sample followed by a 37–38
GiB/s one, above any steady value); over samples 3–30 the rows read
0.8 % / 3.2 % / 4.6 % first-vs-last, min 30.3 / 27.6 / 25.1 GiB/s.
Pre-ladder reference (two days earlier, same job): 34 GiB/s.
`meta_kv_block_map_range_records` still moves during the row (≈1.0 M): the
time_based wrap-around's REWRITE epoch closes and the fsync/persist saves
are whole-map by law (residual (c) below). `meta_kv_block_refs_drift` 0,
`invariant_tripwires` 0, `fuse_op_watchdog_overdue` 0, no Red.

Remount check (run 1): `md5sum` of `test.0.0.root` byte-identical across
umount/remount; session-2 `meta_kv_block_refs_drift = 0`,
`fsck_findings = 0`, `layout_indirect_map_reads = 0`.

## Boarded from this train (NOT fixed here)

1. **Same-mount warm read ≠ durable bytes on a first-write-promoted
   block (pre-existing — reproduces on the BASELINE `aecf1561`).** Shape:
   a fresh file's FIRST 1 MiB write rides the layout-promotion arm
   (`write_lock_scope_entire` 24, `patch_ineligible_oversize` 24), the
   block's other three segments install an OVERWRITE-shape overlay record
   (`overlay_overwrite_installs` 24, `overlay_overwrite_bytes` = 24 ×
   3 MiB) whose 4th quarter is a permanent gap, so one record per file
   stays Open after `close` (`overlay_open` 24, `overlay_stores` =
   writes − 24). A warm `md5sum` of such a file composes the gap from the
   old binding (`overlay_read_gap_serves`, then `overlay_read_drains`)
   and DIFFERS from the cold post-remount md5 — while the cold bytes are
   crc32c-`verify_only` clean (196,608 blocks, 0 bad) in every run. A/B
   on fresh resets, 24 × 8 GiB with crc32c headers, warm md5 of two files
   then cold: baseline `ab.1.0.root` warm `e67592b7…` vs cold
   `16f69193…` (`ab.0.0.root` identical); f46 the same fingerprint
   (34 `overlay_read_serves`, 12 gap serves, 2 MiB gap bytes, 2 drains,
   2 gap seeds — identical on both binaries). The durable store is right;
   the same-mount read of an Open gap-bearing record is what is wrong.
   Repro: `/scratch/tmp/f46_ab.sh <label> 8g`.
   **CORRECTED by finding 48 (`.benchmarks/2026-09-02-f48-warm-cold-overlay-gap.md`,
   fixed `aa464b7e`): the attribution was inverted.** The warm read was
   right; the DURABLE image was wrong — the read-fed rewrite epoch was
   RAM-only and a clean unmount inside the 30 s idle horizon dropped it
   (23 of 24 files' block 0 read one segment + zeros cold on this
   binary). The `--verify_only` "0 bad" above verified nothing (it
   reports err=0 on a corrupted local file); a READ job with
   `verify=crc32c` fails the cold image (err=84).
2. The 6c-i overlay train's per-claim floor probe is O(RUN_LEN_MAX)
   records on a point-dense map (the partial class's face of f46).
3. The shipped `MigrateBlockMap` verb still carries the whole map per
   co-writer publish; the rewrite epoch close and fsync-persist saves keep
   the whole-map diff (correct — they own deletes/canonicalization — but
   O(map) each; §14's 6c-ii windowed canonicalizer is the owed economy).
