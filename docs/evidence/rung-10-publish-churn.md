# Rung 10 — the block-map publish: one slab, no heap per block, a save as wide as the journal entry (lever (c) of `rung-10-seqwrite.md`)

**Diagnosis** (`rung-10-zero-copy-writes.md`'s profile, retaken here as `lc-prof-r1`: 1M seq write ×4,
26.5 GB/s, daemon 5.72 cores). Every block of the overwrite stream published a FULL layout record: a
3 GiB file's inline map is 768 × 48 B = 37 KiB, read, folded, encoded, journaled and applied per
block. (1) The rewrite epoch's ONE save held 64 blocks (`MergeOutcome.max_displaced`, the port's first
buffer); at 1,650 blocks/s per file the write-back timer's 50 ms cadence closed it every ~78 ms with
~53 inside and the other 57 % of the blocks (`rewrite_shadow_fallbacks` 78,718 of 138,436) took a
per-block publish — 3,600/s. (2) Each publish allocated ~8 × 37–140 KiB: the record's copy, the fold,
the tx arena's two chunks (the record encoded, then DUPLICATED by `stagePut`), and on the journal lane
the undo pre-image, the `OwnedRec`, the batch image. All past `smp_allocator`'s 32 KiB slab classes:
each an `mmap`, its first-touch faults, ReleaseSafe's `undefined` memset and an `munmap` under the
process mmap_lock — in the lanes `munmap` 8 %, `mmap` 6.5 %, memset 9 %, faults 7 %, `rwsem_down_write_
slowpath` 8 %; the journal lane 60 % allocation (`QueuedTx.deinit` 18 %, `buildBatchWrite` 21 %); the
checkpoint lane 60 % allocation + `smoReplace` 53 % (37 KiB Puts filling 256 KiB leaves at 133 MB/s).
(3) `publishLayout`'s `±ref` diff searched every entry of both maps per publish — `namesRef` +
`lookupChecked` 21 % of the lanes' samples (2,300 binary searches, 4,600 decodes, ~127 µs).

**What changed** (`b8d3b61`, `1f98985`).
- **The save's width is DERIVED** (`write_seams.MergeOutcome.max_displaced` = 213): the save is ONE
  journal entry (charter §5.1, ≤ 128 KiB); half the entry is the window's (`kvmap.CROSSING_CHUNK`'s
  convention), less four map frames of neighbourhood, over the widest per-block cost of either map
  shape — a tree-mapped insert's three kind-7 Puts, a Delete, `+ref`, `−ref` = 306 B. Comptime asserts
  pin both shapes (the inline record at its 65,826 B frame + 213 × 96 B fits; the kvmap window + head
  stays under its half); the tie test restates the arithmetic. Epochs now hold a 50 ms window whole.
- **One pooled slab per merge** (`data/merge_scratch.zig`): the record copied off the node cache's
  borrowed view (`layoutRecordWith`, `lookupView`), the fold, and the tx's arena all carve from one
  slab; the slab rides the queue entry (`tx.ArenaBacking`, `KvTx.initBacked`, `MetaSet.beginBacked`)
  and returns at the tx's terminal outcome on the journal lane. The arena is PRIMED with one node of
  the slab's remainder (`Slab.primeArena`: an arena grows its nodes ×1.5 — 56, 85, 128 KiB — so the
  first cut overflowed 163 MB per row into the heap). Slab = 2 records at the cap + 3/2 × (a record +
  the widest save's tx) = 356 KiB at 256 KiB nodes; pool = two conveyor batches (`2 × max(64, 2 ×
  cpus)` = 128 here, 44.5 MiB — the count follows the merge concurrency, the budget offers no pool
  tier); a merge past the pool parks (`layout_publish_scratch_waits`), bytes past a slab fall to the
  heap counted (`layout_publish_scratch_overflow_bytes`) — both 0 on every row. The pool is the
  budget's (charter §6.2 R5, `rung-09-memory-footprint.md` §3.5): component `layout_publish_scratch`,
  its bytes as gauge and floor, weight 0, no shed (a slab in flight backs a queued tx), tie-tested at
  pool × slab; the transform's `ScratchPool` registers the same way (`transform_scratch_pool`).
- **Staged in place**: `KvTx.stagePutArena` stages the arena-built value without the second copy.
- **The diff is ONE merge walk** of the two ascending named sequences (`layout.RefDiff` over
  `NamedIterator.peek`/`skip`): a same-block same-reference pair costs a 16 B compare off the wire,
  only a difference decodes. Same `+ref`/`−ref` set, same proof check (before anything is staged).

**Red → green.** `write_seams`: the width tie (213; 214 would cross the half). `merge_scratch`: the
slab/pool/backing lifetime (a backed tx returns its slab once, after its arena; the one past the cap
parks; a primed arena lands a cap-sized record and the widest save's refs in the slab — overflow +0).
`layout`: the merge-walk diff ≡ the per-entry search over 200 random map pairs, a staged durable copy
and a fresh file (red on the old code's shape by construction; the walk skips equal pairs undecoded); a
1,000-entry publish grows the tx arena by less than two copies of itself (red: `stagePut`'s duplicate).
`meta.tx`: `stagePutArena` frames as `stagePut`'s twin; the backing's release count through deinit, a
queue entry and a refused build. The epoch tests size their fixtures by the width. Green: `write_seams`
42, `merge_scratch` 42, `data.layout` 59, `data.plane` 50, `write_epoch` 55, `kvmap` 49, `writeback`
53, `device_overlay` 61, `write_admit` 77, `accumulate` 56, `meta.tx` 50, `economy` 65, `checkpoint`
103, `journal_lane` 60, `kv_fs` 91, `fsck` 113; smoke 45/45: the data plane WITH / WITHOUT a cache path,
THE KILL MATRIX (every acked byte present, C8 0 drift), the `sync -f` rows, THE ORDERED LAW'S KILL ROW,
kill -9 mid-tar; every perf session's `fsck --offline` clean, `invariant_tripwires` 0.

**A/B on this workstation** (`scripts/perf/quiet.sh ram-sqz`, dist builds, 4 × 3 GiB lay-down, 20 s
rows, the whole venue held; A = `dev` `acc4b55` from the main tree, B = this branch; legs A B B A over
the full matrix, `lc-A-r2 lc-B-r3 lc-B-r4 lc-A-r3`; mean B ÷ mean A; daemon cores with `sqz-ckpt` and
`sqz-jrnl` in parentheses).

| row | A (per leg) | B (per leg) | B ÷ A | daemon cores A → B (ckpt, jrnl) |
|---|---|---|---|---|
| 1M seq write QD16×1 (MiB/s) | 13,973, 13,998 | 26,276, 26,195 | **1.88×** | 2.00 → 2.13 (0.43→0.34, 0.28→0.24) |
| 1M seq write QD16×4 | 27,410, 27,686 | 47,145, 47,093 | **1.71×** | 5.77 → 5.94 (1.10→0.42, 0.89→0.30) |
| 4M seq write QD4×4 | 22,082, 28,812 | 41,988, 43,434 | **1.68×** | 4.88 → 5.83 (0.91→0.55, 0.71→0.49) |
| 64k randwrite QD16×4 | 19,590, 19,547 | 19,407, 19,669 | 1.00× | 7.15 → 7.15 |
| 4k randwrite QD1 / QD32×1 / QD32×4 (IOPS) | 46.0 k / 228.4 k / 410.7 k | 54.7 k / 217.6 k / 413.3 k | 1.19× / 0.95× / 1.01× | — |
| 4k randread QD1 / QD32×1 / QD32×4 | 114.7 k / 333.9 k / 815.1 k | 107.8 k / 340.4 k / 839.5 k | 0.94× / 1.02× / 1.03× | — |
| 64k randread / 1M seq read ×1 / ×4 (MiB/s) | 32,765 / 42,269 / 66,309 | 32,517 / 42,222 / 67,680 | 0.99× / 1.00× / 1.02× | — |

Sequential write ×4 stands at **47.1 GB/s = 80 % of the raw 58.7** (the charter's bar is 36.4 GB/s,
on this loopback RAM substrate; the fabric venue decides); daemon CPU per MiB 0.209 → 0.126 ms (−40 %)
while the lanes carry 1.7× the bytes. The rows the change cannot reach move within their session
swing (the 4k rows swing 12–43 % between sessions of one binary on this box). The mechanism on sw1m
×4 (B-r2's `.stats`): publishes per block 0.585 → 0.086 (20,104 merges for 234,835 blocks, 11.7 per
save; `rewrite_shadow_fallbacks` 7.8 % of the blocks, from the save-in-flight window), metadata device
bytes per block 17.2 KiB → 2.3 KiB, scratch waits 0, overflow 0. Mid-row profile (B-r1, ×4): the
lanes' top cost is now the per-write metadata read — `readInodeOpt` 22 %, `layoutPoint` 12 %, the
token divert 8 % (lever 4's path); `mapMerge` is off the top forty; `sqz-ckpt` 10,859 → 1,211 samples,
`sqz-jrnl` 8,662 → 984 (both still allocation-shaped: `captureUndo` 20 %, the `OwnedRec` copy 25 %,
`smoReplace` 59 % — the journal-side 37 KiB copies, now 9× rarer).

**Owed, not this lever's.** (1) A FRESH file's blocks never ride the epoch (`old_binding == null`):
the lay-down's per-block publish stays (12–13 GB/s on every leg, unchanged) — charter §5.6's per-ino
publish coalescing (`SQUEEZEFS_PUBLISH_COALESCE_MAX`, `layout.PublishWindow`, unwired) and the layout
DELTA records (`layout.LayoutDelta` — the KV fold still folds the reference's bincode class, never the
port's binary one) are the mechanisms. (2) The journal lane's three per-publish copies (`captureUndo`'s
dupe, `OwnedRec.fromRecordRef`, `buildBatchWrite`'s image) and the checkpoint's bset/snapshot buffers
are still heap per op above 32 KiB. (3) `MetaSet.begin`'s `KvTx` box is one slab-class allocation per
merge. (4) The transform's `CodecPool` is still outside the budget: its contexts are the zstd
library's own allocations, unmeasured.
