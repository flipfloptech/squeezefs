# Rung 10 — a random rewriter's drain lands its dirty pages; a superseded upload publishes (0003 §AJ.7)

The write-back timer's owed item (`rung-10-writeback-timer.md`, 0003 §AJ.6): fio's random 4 KiB
rewrites on a held-open file cost 1.32–1.36 device bytes per user byte under `timer` (0.13 under
`close`). Owner ruling (0003): a measured write amplification is a defect.

**Where the bytes went** (dev `1a81131`, dist, kamrui loop rig, cache set, one leg's `.stats`
delta): 1,197 MB acked → 2,150 MB on the OSTs = W1 patches 941 MB (1:1) + W2 in-place folds 85 MB
+ **141 whole-block uploads 591 MB + 127 superseded uploads 533 MB thrown away**. The uploads came
from a rewrite that happened to be stream-adjacent (457 in 15 s): it parks a DEFERRED full buffer
(arm G), W1 then declines every later write of that block on its RAM custody, the 65-run table
overflows into a whole-block seed (`NeedsSeed`), and each timer pass uploaded the block whole —
superseded by the next write ≈ 90 % of the time, the DMA freed and redone (plus a 4 MiB CoW copy
per supersession).

**What changed.** (1) Every content-valid full buffer tracks `dirty` (pages written since the cell
last matched `clean_base`) over `clean_base`, the binding its clean pages equal (the seed's base, an
escalated overlay's base, a published snapshot's key). `flushParkedBlock` lands it IN PLACE when
W1's state half passes and the map still names `clean_base`: one DMA per contiguous dirty run from
the cell under (3), the W1 postlude, the entry retires (`drain_inplace_dirty_drains`). (2) A write
landing during an upload's DMA no longer frees it: the snapshot (one consistent image, taken under
(3)) publishes (`write_pipeline_superseded_publishes`), the entry stays parked with the pages written
since over the published key; the upload re-drives only for writes its caller owes
(`flushParkedBlockBy`: durable flushes all, the timer its cutoff, the write-through trigger none).
`write_pipeline_supersessions` / `_superseded_bytes` and the `superseded` disposition are retired.

**The bound.** Per pass `≤ pages(dirty) × P ≤ Σ_w pages(w) × P`: device ÷ user ≤ 1 for page-aligned
rewrites (each acked page lands at most once per write). Visibility: the write-side view outranks
the map for every byte of a content-valid entry, so the older published image is never served after
the newer write was acked. Crash: a dirty run is one DMA and a write's pages are contiguous, so a cut
between DMAs leaves each FUSE write whole or absent; a failed DMA leaves the entry parked, loud.

**Red rows** (against dev's product, `8ba35d3`): "random 4 KiB rewrites under the write-back timer"
(`expected 20480, found 65536` — the block for five dirty pages); "a superseded upload publishes its
snapshot" (`expected 0, found 1` — the mint abandoned). Green with the fix, plus "a superseded
upload's snapshot publishes and its successor drains only what changed after it" (one page in place
after the epoch saved; the read seam serves the entry's newer page over the published image) and the
kill rows "the in-place dirty drain's crash law" (cut after DMA 0, 1, 2 and a torn 2-page DMA: every
page old or new, the 8 KiB write whole or absent unless its own DMA tore, still parked, the next drain
exact). Two rows whose law was "a sole block lands CoW" now land in place (the W2 CoW row once its
clone is gone: 9 pages, not the block; the save window: B takes the parked pages, one free not two).

**The A-B-B-A** (kamrui x86_64, 12 threads, dist, loop devices on NVMe; 2 MDT + 2 OST; A = this
build `timer`, B = this build `close`, D = dev `1a81131` `timer`, 2 legs each; `oc` = open/close/s):

| row | cache: A / B / D | A ÷ B | cache-less: A / B / D | A ÷ B |
|---|---|---|---|---|
| randwrite 4 KiB QD32 IOPS | 21.4 k / 22.9 k / 20.4 k | 0.93 | 18.5 k / 20.4 k / 20.7 k | 0.91 |
| **randwrite device ÷ user** | **0.996** / 0.503 / 1.652 | — | **0.996** / 0.592 / 1.763 | — |
| randread 4 KiB QD32 IOPS | 80.2 k / 79.2 k / 72.2 k | 1.01 | 62.1 k / 60.3 k / 79.9 k | 1.03 |
| open/close, 1 / 4 threads | 302 k / 918 k vs 37.9 k / 139 k | 8.0 / 6.6 | 301 k / 827 k vs 34.3 k / 119 k | 8.8 / 7.0 |
| open + 4 KiB read + close, 1 / 4 | 259 k / 719 k vs 38.0 k / 137 k | 6.8 / 5.2 | 246 k / 703 k vs 34.4 k / 114 k | 7.1 / 6.2 |
| mdtest dir create / stat / rename / remove | 3,940 / 1.01 M / 3,792 / 4,377 vs 3,842 / 970 k / 3,661 / 4,379 | 1.03 / 1.05 / 1.04 / 1.00 | 2,724 / 803 k / 2,593 / 3,105 vs 2,916 / 824 k / 2,628 / 2,910 | 0.93 / 0.98 / 0.99 / 1.07 |
| mdtest file create / stat / read / remove | 3,825 / 994 k / 601 k / 4,378 vs 3,730 / 1.00 M / 123 k / 4,477 | 1.03 / 0.99 / 4.87 / 0.98 | 2,382 / 805 k / 455 k / 2,685 vs 2,513 / 895 k / 106 k / 2,844 | 0.95 / 0.90 / 4.31 / 0.94 |
| IOR write / read MiB/s | 1,013 / 2,464 vs 999 / 2,942 | 1.01 / 0.84 | 513 / 2,282 vs 347 / 2,098 | 1.48 / 1.09 |
| IOR device ÷ user | 1.000 / 1.000 | 1.00 | 1.000 / 1.000 | 1.00 |
| `echo >> f` × 500: device bytes | 0 / 0 (cache) | — | 40,960 / 45,056 | 0.91 |

The timer leg's randwrite breakdown (cache, leg 1): 1,234 MB acked → W1 1,052 MB + 145 dirty
drains 178 MB (43,399 pages, 25,866 DMAs, 0.98 per accumulated byte); zero whole-block uploads,
zero superseded DMAs, zero mint abandons. The remaining gap to `close` is W1's 1:1 (80 % of the
bytes): under `close` a block's custody lived until fio's close and coalesced in RAM; under the timer
a drained block is custody-free and W1 lands each rewrite at once — at the ≤ 1 bound, not under it.
IOPS on this rig sit at ~20 k in every arm (the FUSE path, not the drain: randread's legs vary
25 % with host load the same way), so the A ÷ B IOPS ratio moved from dev's 0.83 to 0.91–0.93
within noise; the macOS container's 0.60–0.66 was the whole-block upload storm on a virtio disk.
`writeback_residency_max_ns` 1.44 s under the storm (D = the pass's 145 drains; dev 1.24 s).

**Not done.** Keeping a drained buffer resident (clean, re-dirtied in RAM) would coalesce below 1
under the timer (≈ distinct pages per pass ÷ writes per pass) at the cost of the custody W1 avoids;
not taken here.

**Gate.** One full kamrui gate through the lock on `fc74586` (over `0b67bf7`): 48/53 steps,
2,941 / 3,023 tests (80 skipped), 2 failed — both `docs/gates/flakes.md` rows: "THE KVDA PAGE
SINK's fallback" (`block_untracked_free_refusals` = 1, the shipped-free row) and "THE WRITE-BACK
TIMER UNDER A WRITER STORM" on kamrui (gate waits 0, cold flushes 0: the cap of 8 never met).
Rebased over `e1c334f` (`flushBornBy`; its `.superseded` arm dropped): kamrui data units 503/503,
the write-back timer smoke rows 41/42 (the same writer-storm row).
