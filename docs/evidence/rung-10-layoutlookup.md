# Rung 10 — lever 1: one layout entry per op, off the borrowed record (layoutlookup)

Bottleneck #1 of `rung-10-perf-baseline.md`: every read and write copied its file's whole layout
record out of the KV tree (36 KiB on a 3 GiB file: mmap, first-touch faults, ReleaseSafe's memset,
munmap under the process mmap_lock), decoded and validated all 768 entries, then binary-searched one.

**What changed.** `KvTree`/`MetaSet`/`MemStore.lookupView` run a visitor over the live value borrowed
under the leaf snapshot's pin (no copy; `gpa` only on a delta fold). `layout.resolvePoint` applies
`decode`'s checks to the header and to the one inline-map entry the search lands on (`decode` =
`decodeShape` + `validate`; the codec stays canonical and total). `DataPlane.layoutPoint` serves
`mapLookup`, `mapView`, `metaFile`, `metaFileAt` and `inodeSize` from the view. A foreign (token)
record or a collided chain stays on the owned path. `KvFs.write` reads a striped file's kind off
the view and skips `readLayout`. The faster write path then overflows the rewrite epoch more often
(per-block publishes 887 → 4,279 per 10 s on sw1m ×4). Each publish's ref diff (`namesRef`) scanned
every entry per entry, 768² per merge (37 % self in `NamedIterator.next`), so it is now a binary search.

**Red → green.** `kv_fs` "a striped write reads its layout's kind off the borrowed record": the store
copied 37,111 B for a 512 B write into a 768-block map and 295 B into a 1-block map; after the fix the
two are equal. Also pinned: `resolvePoint = decode ∘ lookup` on every shape and every block of a
768-block map, and an entry that the full decode refuses leaves every other block resolvable.
`lookupView` reads overlay-head and base values through an allocator that refuses every allocation.

**A/B on kamrui, RAM-backed nvmet-loop** (`baseline.sh ram-sqz`, 4 × 3 GiB, 30 s rows; A = `dev`
d7c4121 ×4, B ×2 interleaved A3 B3 A4 B4; median, spread). Both arms are dist builds, and each session
held every venue lock. `ramnvme.sh down` ran after each session.

| row | A | B | B ÷ A |
|---|---|---|---|
| 4k randread QD1 (IOPS) · p50 | 8,302 (3 %) · 109 µs | 19,350 (1 %) · 47 µs | 2.33× |
| 4k randread QD32×1 | 13,620 (1 %) | 81,053 (0 %) | 5.95× |
| 4k randread QD32×4 | 33,162 (2 %) | 251,352 (1 %) | **7.58×** |
| 4k randwrite QD1 · p50 | 3,867 (4 %) · 230 µs | 9,420 (1 %) · 99 µs | 2.44× |
| 4k randwrite QD32×1 | 6,198 (1 %) | 32,400 (1 %) | 5.23× |
| 4k randwrite QD32×4 | 11,476 (2 %) | 72,537 (5 %) | **6.32×** |
| 64k randread QD16×4 (MiB/s) | 1,765 (18 %) | 6,214 (1 %) | 3.52× |
| 64k randwrite QD16×4 | 719 (2 %) | 3,257 (0 %) | 4.53× |
| 1M seq read QD16×1 / ×4 | 5,454 / 6,682 | 6,811 / 7,800 | 1.25× / 1.17× |
| 1M seq write QD16×1 / ×4 | 1,212 / 2,191 (15 %) | 1,935 / 2,536 | 1.60× / 1.16× |
| 4M seq write QD4×4 | 2,172 (7 %) | 2,466 (0 %) | 1.14× |

A's sw1m ×4 is bimodal (2,364/2,262 vs 2,120/2,042): lane CPU 0.57 vs 0.9 ms/MiB, depending on how
often the epoch overflows. A B built without the `namesRef` fix fell into the slow mode (2,131), and B
with the fix does not (lanes 0.18 ms/MiB).

**Per-op CPU** (thread CPU ÷ ops; shares of the class's `perf record -g` samples, `incl.py`):
- **4k randread QD32×4, workers: 108 → 15.3 µs.** A: layout 53 %. Allocation churn: memset 32 %,
  munmap 24 %, mmap 19 %. mmap_lock wait 21 %, `io_uring_enter` 6 %. B: `io_uring_enter` 43 % (6.6 µs),
  `read_hpet` 14 %, `layoutPoint` 9 % (1.4 µs), no mmap/munmap, memset 0.07 %. QD1 workers: 98 → 35 µs.
- **4k randwrite QD32×4.** Lanes 272 → 49 µs. A: `mapLookup` 39 % + `readLayout` 32 %, memset 30 %,
  munmap 26 %, mmap_lock wait 17 %. B: `read_hpet` 36 % (≈ 18 µs, as in A: lever 3 is next),
  `layoutPoint` 7 %, `io_uring_enter` 7 %. Workers: 81 → 39 µs.
- **1M seq read ×4, workers: 313 → 163 µs/MiB.**

The hypothesis held for reads: ≥ 7× and ≤ 16 µs per read. Writes beat the expected ≈ 3×: the lanes
fell to 49 µs (expected ≈ 80), and the workers' cost per write halved too. SSD rows were not re-measured.
