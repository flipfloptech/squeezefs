# Rung 10 — the read view: zero metadata KV lookups on the read's hot path

The owner's bar (2026-10-19): on an in-memory substrate SqueezeFS is the fastest of {ext4, xfs,
SqueezeFS} or on par on every row. `rung-10-fuse-boundary.md` split the 4k randread QD32×4 row's
6.4 µs of worker CPU per read: the round trip ≈ 2.3, **three KV lookups ≈ 2.4** (`metaFileAt`'s inode
+ layout records, the post-DMA recheck's binding — each a divert verdict, a route-table arc-swap
pair, a tree descent, a leaf binary search), the device leg ≈ 1.7. This note takes the middle term.
Venue: the workstation (`local`, `7.2.8-9.30-omarchy-sqz`), the RAM NVMe set of
`scripts/perf/ramnvme.sh`, the venue held per session, every row started at Tctl ≤ 70 °C, 20 s rows
(profiles 30 s). The profile of dev (`ram-sqz-rrvprof-A-r1`) confirms the split: `metaFileAt` 23.8 %
+ `metaBinding` 13.0 % of the workers' 7.99 cores at 1.25 M = 2.35 µs per read, `entryKeyLess` 6.5 %
self, `foreignLayoutFn` 5.3 %, `inodeSizeLocal` 11.6 % incl.

**The view** (`src/data/read_view.zig`, portable). A per-ino cached projection of what one data-plane
op needs: the inode record's size; the layout record's kind, file id, durable copy, kvmap head and
its WHOLE inline block map (or inline payload) — 64 sets × 4 ways, each slot two header lines plus
the record cap (≤ 64 KiB; 16.4 MiB at the 256 KiB node, allocated once, a fixed budget component
`read_view_bytes`). A hit is acquire loads and no RMW: the slot's seq, the ino, the header words, a
binary search of the map by block, the seq again (an odd or moved seq is a torn read, a miss), then
the two generations below. `DataPlane.resolveAt` serves `metaFileAt`, `metaFile`, `metaBinding`,
`mapLookup`, `mapView`, `inodeSize` and `metaInlineData` from it — the read AND write seams, so the
write prelude rides it too; a miss runs the old path — one divert verdict, two store lookups off the
borrowed records — and fills the slot on the way out (never for a foreign-slot record, a destroy in
flight or a `-o ro` reader). Replacement is FIFO by fill time with a 1 ms floor (five hot files in
one set cost one 64 KiB fill per millisecond, not one per read), and a fill is ONE per generation: a
way already holding the ino as of the filler's generations is left alone (`read_view_fill_redundant`).

**The staleness proof.** Two generations are captured BEFORE the filler's lookups and compared AFTER
every hit's loads: (1) the ino witness (`src/meta/ino_witness.zig`, 1,024 line-sized stripes) that the
node cache bumps for every record of an ino-major kind (inode, xattr) after the leaf snapshot that
carries it publishes — `CachedNode.applyLockedClass` (every path into RAM: a conveyor commit, a replay,
a shipped publish served here) and `removeOverlayRecordsLocked` (a rollback) — so the view's exactness
does not depend on naming every mutator; (2) the mount's slot-move epoch (decision 0003 §X.1), so a
slot this mount read locally that became another appender's stales every view filled before the
transition. The witness is post-visibility and that suffices: a record applied but not yet bumped
belongs to a commit nobody has been acked for (the bump runs under the leaf write lock, before the
journal write; the write side's custody releases and the displaced block's free come after the ack),
so a read lagging it is concurrent with the write. `read_view_model.zig` explores mutator, filler and
reader exhaustively: the law holds; the weakenings `bump_before_apply`, `reader_no_gen_check` and
`reader_no_seq_recheck` each serve a stale view; the filler's post-lookup re-read is an economy. The
post-DMA recheck (charter §6.3) is unchanged in law and is now the cheap compare: `metaBinding` at the
CQE is a view hit — the witness and the slot's generation equal — or a KV re-read.

**The io-wq punt** (the task's device-leg item). Under zero copy every byte the daemon produces in RAM
reaches the app's pages by one `READ_FIXED(memfd → zc slot)`: the bounce window, the fill-pool slab of a
warm serve, an inline or staged body. In 7.2.8 `shmem_file_open` sets `FMODE_CAN_ODIRECT` alone and
shmem is not pollable, so `io_file_supports_nowait()` is false and `io_read()` answers `-EAGAIN` at
issue for every such read: io_uring punts it to io-wq, whose worker then performs a blocking read of a
page that was resident all along (2.6 cores at 2.1 M on the ruler's `bounce` arm). The device leg
itself is not punted (the block device supports NOWAIT: `blk_mq_submit_bio` runs on the worker), so the
cold row never paid it. No daemon-side source escapes it — ramfs lacks `FMODE_NOWAIT` too, and a
registered buffer cannot be a READ's source — so the fix is the kernel's: proposed patch 0032
(`scripts/kernel/proposed/0032-sqz-shmem-nowait-reads.patch`: `shmem_file_read_iter` honours
`IOCB_NOWAIT` — a resident up-to-date folio or a hole's zeros inline, anything that would wait
`-EAGAIN` — and `shmem_file_open` declares `FMODE_NOWAIT`), compile-checked W=1 as `mm/shmem.o` against
the built tree, unbooted; its probe plan is in the patch.

**Red → green.** `read_view` "a filled ino hits … the witness moving stales it until a refill … the
slot-move epoch likewise … forget empties the way", "the kinds …", "replacement: the four ways …",
"the seqlock under a racing filler …" (4 readers against a filler alternating two records for 200 ms:
every hit one fill's words); `ino_witness` (the key shapes); `read_view_model` (above); the smoke row
THE READ VIEW on a real mount: 512 random 4 KiB O_DIRECT reads of a 16 MiB striped file → `read_view_hits`
1,024 (exactly two per read: the probe's `metaFileAt`, the recheck's `metaBinding`), misses 0, fills 0,
stale 0, 512 direct legs; a truncate stales it (the refill after it, a read at the old length EOF), an
overwrite's publish stales it again (the new bytes read back). Kept green: `read_view`, `ino_witness`,
`layout`, `node_cache` (73/73), `warm_ladder`, `read_path`, `accumulate`, `data.plane`, `kv_fs`,
`transport`, `stats` (101), `e2e` (114, 2 skipped), `sym` (415, 3 skipped); the smoke rows KERNEL AIO,
the data plane WITH / WITHOUT a cache path, THE STAGED READ'S IDENTITY WINDOW, THE KILL MATRIX.

**A defect the matrix found.** The first full-matrix B legs (`rrvfull-B-r11/12`) lost the 4k randwrite
QD32×1 row 554 k → 151 k IOPS (p99 117 → 2,113 µs; 707 k view fills for 3 M writes): a slot read
mid-fill counted as a MISS, so every op that missed queued a 36 KiB fill of its own behind the set lock,
and 32 writers on one file kept it busy for good. The one-fill-per-generation rule (`0c287d6`) ends it:
on the same row 11.5 M writes → 11.5 M hits, 0 misses, 400 fills.

**Also on the lane path**: `KvFs.read` reads a routed file's kind off the borrowed record
(`layoutKindView`, as `write` has since `rung-10-layoutlookup.md`) and copies the layout record only
for the inline arm, whose record is the payload.

**A/B — the read rows** (A = dev `0e3249b` from the main tree, B = this branch; legs A B B A B A A B,
each a venue-held session; mean (min–max), daemon cores, worker µs per read; E = ext4 on one namespace
of the same set, one leg, io_uring O_DIRECT; `/build/perf/mx/results.tsv` `ram-sqz-rrv-{A,B}-r1[1-4]`,
`ram-ext4-rrv-E-r1`):

| row | A | B | B ÷ A | cores A → B | µs/read A → B | ext4 | B ÷ ext4 |
|---|---|---|---|---|---|---|---|
| 4k randread QD1 | 120.7 k (97–129 k) | 126.0 k (112–145 k) | 1.04 | 0.73 → 0.73 | 6.07 → 5.83 | 237.8 k | 53 % |
| 4k randread QD32×1 | 540.7 k (470–568 k) | 712.6 k (497–803 k) | 1.32 | 2.01 → 2.00 | 3.71 → 2.81 | 778.2 k | 92 % |
| **4k randread QD32×4** | **1,247,021** (1,244–1,250 k) | **1,740,783** (1,728–1,747 k) | **1.40** | 7.99 → 7.85 | **6.41 → 4.51** | 1,983,752 | **88 %** |
| 64k randread QD16×4 | 772.6 k (767–776 k) · 48.3 GB/s | 929.1 k (922–940 k) · 58.1 GB/s | 1.20 | 6.88 → 6.27 | 8.90 → 6.75 | 903.1 k · 56.4 GB/s | **103 %** |

The QD32×4 row at 1 % spread on both sides: p50 97 → 69 µs, fio 59 → 83 % busy (ext4 is fio-bound at
99 %). The QD32×1 row keeps its known two modes (where fio's thread lands against the worker): B 803 /
798 / 752 k against a 497 k mode at fio 98 %, A 568 / 568 / 556 / 470 k. Over the row the view served
2 hits per read, 0 misses, 3–7 fills, 0 torn; the divert's local verdicts fell from 2 per read to 152 per
30 s row (`ram-sqz-rrvprof-B-r1`).

**The full matrix** (A A at `0e3249b`; B B at `0c287d6`): 4k randwrite QD1 92.6 / 91.5 k → 93.7 / 96.4 k,
QD32×1 554 / 536 k → 575 / 574 k (1.05), QD32×4 1,033 / 994 k → 1,129 / 1,120 k (**1.11**, cores 12.1 →
11.6: the write prelude's `mapView` + `mapLookup` are hits), 64k randwrite 526 / 480 k → 562 / 568 k
(1.12); seq read 1M ×1 51.5 / 49.4 → 52.6 / 52.8 GB/s, ×4 73.0 / 71.1 → 70.4 / 72.7; seq write 1M ×1 33.4 /
32.9 → 32.9 / 33.5, ×4 48.6 / 49.3 → 42.0 / 56.8 (both arms bimodal, as recorded before), 4M ×4 45.0 /
42.9 → 45.1 / 40.3. No write row regressed; the 4k randread QD32×4 row 1,253 / 1,243 k → 1,729 / 1,726 k.

**The remainder** (the B profile `ram-sqz-rrvprof-B-r1`: 7.84 worker cores at 1.75 M = 4.47 µs per read,
`resolveAt` 3.4 % self ≈ 0.15 µs and the rest of userspace ≈ 0.45 µs): the kernel's. `io_uring_cmd` (the
FUSE commit + fetch) 23 % ≈ 1.0 µs, the delivery and bridge-CQE task work (`io_run_local_work`) 18 % ≈
0.8, the `READ_FIXED` issue 12.5 % ≈ 0.56 of which the nvmet-loop target's workqueue lock
(`queue_work_on` + `native_queued_spin_lock_slowpath`) 11 % ≈ 0.5 — the substrate's, which ext4 pays in
fio's context — and the park/wake. In order: patch 0031 (0.25–0.35 µs), fewer and fatter passes (the
worker enters the ring twice per read), the structural kernel lever of `rung-10-fuse-boundary.md`. The
view's own tail: the recheck could compare the probe's generation carried in the leg's token instead of
re-searching the map (≈ 0.05 µs); the inline arm of `KvFs.read` still copies its record.
