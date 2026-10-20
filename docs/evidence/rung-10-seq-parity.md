# Rung 10 — sequential bandwidth against ext4/xfs on the RAM substrate: the latency split, the levers, the binder

The owner's bar (2026-10-19): on an in-memory substrate the fastest of {ext4, xfs, SqueezeFS} or on
par, every row. The sequential rows (`rung-10-localfs-comparison.md`; 1 MiB O_DIRECT QD16 × 4 jobs over
4 × 3 GiB laid-down files, the RAM NVMe over nvme-loop): seq write ext4 70.5 GB/s / xfs 70.6 / **SqueezeFS
49.7 (70 %)**, seq read 81.8 / 82.9 / **72.7 (89 %)**. This note measures where a 1 MiB request spends
its time on both sides of the FUSE boundary, lands two levers, measures two more, and names the binder.

**The split, instrumented.** `transport_held_write_phase_ns` (`80232cc`): a held write's life on its
worker in three exact-sum phases — `to_store` (delivery → its `WRITE_FIXED(device ← slot)` pushed), `store`
(the push → the device CQE), `to_commit` (the CQE → the commit pushed). A bpftrace script (`~/tmp/seq/
trace.bt`, `trace2.bt`: kprobes on the sqz kernel's `fuse_uring_*`, the io_uring tracepoints, 10 s
mid-row) measured the kernel's half per request, on SqueezeFS and on ext4 alike. Means, 1M seq write ×4:

| per 1 MiB write | SqueezeFS (49 GB/s) | ext4 (68–70 GB/s) |
|---|---|---|
| fio clat (fio's own stamp) | 1,237 µs (p50 1,057) | 853 (p50 733) |
| `io_uring_submit_req → io_uring_complete` on fio's ring (the kernel's whole view) | **805** (p50 ≈ 600) | **295–462** (p50 ≈ 250–300; two sessions) |
| the gap: a filled CQE waiting for fio's one thread to finish its submits | **432** | **≈ 400–560** |
| fio's thread CPU per write (`fio%` ÷ writes) | 60 µs (73 % busy) | 49.5 µs (87 % busy) |
| queued → `fuse_uring_send` (the delivery CQE; an entry was free within 1 µs 96 % of the time) | 56 | — |
| `fuse_uring_send → fuse_uring_req_end` (the daemon, as the kernel sees it) | 680 | — |
| — `to_store`: dispatch, the handler's prelude, the store pushed | 198–238 (p50 ≤ 64, p90 ≤ 1 ms) | — |
| — `store`: the push → the device CQE | 492 → **447** after lever 3 (p50 ≤ 512) | the device: ≈ ext4's whole path |
| — `to_commit`: the postlude and the commit pushed | 50–58 (p50 ≤ 32) | — |
| the doomed NOWAIT attempt → io-wq enqueue (`io_uring_queue_async_work`) | 12 µs, every DMA | **none** (iomap sets no `REQ_NOWAIT`) |
| io-wq enqueue → the re-issue on an `iou-wrk` thread | 192 mean, p50 8, 13 % > 64 µs | none |

Three readings. (1) **Both rows are fio-thread-bound to a degree the clat hides**: ext4's device path is
≈ 300 µs and fio's own reaping delay ≈ 550 — at 87 % busy the submits (GUP of 256 pages, the bio) are
what fio does; our FUSE submit costs that thread ≈ 10 µs more (the request, two kmallocs, the daemon
ring's `uring_lock` for a kmbuf select a zc WRITE never uses). (2) **Our kernel-side path is 805 µs
against ext4's ≈ 300–460**; the device DMA itself (`store`, ≈ 450–490) is NOT slower than ext4's whole
path at the same load — the ≈ 350 µs we add are `to_store` + `to_commit` + the delivery (the daemon's
per-request CPU, ≈ 106 µs per MiB serialized on a queue's two workers and one lane, and its hops), and
the io-wq re-issue every block-device DMA over `max_sectors` paid (nvme-loop's is 1020 KiB; this host's
real NVMe's 128 KiB, nvme-tcp's ≈ 508 KiB — a REQ_NOWAIT bio cannot be split, `bio_split_rw`). (3) The
read row's worker spends 50 µs per 1 MiB read — the commit's 256 `unpin_folio` + `folio_mark_dirty_lock`
+ `io_buffer_unregister` **28 %**, the warm-ladder probe and the post-DMA recheck (two `layoutPoint`s)
15 %, the doomed NOWAIT attempt + the io-wq wake 8 %, `schedule` 7 %.

**Lever 1 — the overlay core's bitmaps word-wise** (`c9ca7a7`): `beginStore`/`completeStore`/
`releaseStore` ran one SeqCst RMW per PAGE (3 × 256 per 1 MiB store, 14 % of the write handler's CPU);
now one per 64-page word (4 each). Same semantics; the tie test runs 2,000 random ranges against a
per-page reference. Inside the session spread.

**Lever 2 — the streaming write's lane placement** (`76a499f`, `7dfddc2`; `SQUEEZEFS_FUSE_STREAM_LANES`):
a spilled overlay-class write on a node-round-robin lane (`node`) or never on the worker's own lane
(`node_only`), against the queue's CPU lane (`cpu`). The hypothesis — one submitter's depth on one queue,
its spilled handlers on ONE lane — was wrong: `cpu` 49.07 GB/s at 6.02 daemon cores, `node` 47.69 at 6.3
(+0.4 cores of `sqz-fuse-ln`), `node_only` 46.72 at 7.44 (`to_store` 244, `to_commit` 85: the hops) —
the `store` phase stood at ≈ 490 with the worker freed of every handler. **Ships as `cpu`**; the arms stay
measured levers.

**Lever 3 — the bridge in NOWAIT-sized pieces** (`dcf0428`): the worker probes each bridge fd's ceiling
once (`max_sectors_kb`, `max_segments × page`; `worker.NowaitCeilings`), `zc.bridgePieces` cuts a DMA at
or under it (≤ 8 pieces: 1 MiB over a 128 KiB NVMe), every piece an SQE under the bridge's tag, the pend
resolving at the last CQE (the bytes moved or the first failure; a deadline cancel is `CANCEL_ALL`), the
SQ sized for it (`geometry.sqEntries`). Pinned: `SQUEEZEFS_TEST_ZC_BRIDGE_CEILING` = 256 KiB makes a 1 MiB
held write four `WRITE_FIXED`s byte-exact, a sealed memfd's short piece fails the whole bridge and the
write still answers. Measured (`ram-sqz-rbw-pieces-r1` vs `rbw-cpu-r1`, one session each): io-wq punts per
10 s 495 k → 1.9 k, `iou-wrk` 0.86 → 0.06 cores (the submit moved onto the worker: `sqz-fuse-ur` 3.69 →
4.17), **daemon cores 6.02 → 5.75 on the write row, 3.86 → 3.54 on the read row**, `store` 492 → 447 µs,
sw1m ×4 49.07 → 49.53 GB/s, sr1m ×4 70.76 → 70.83. The CPU and the hop are gone; the row barely moved —
the io-wq hand-off's 192 µs mean was a tail (p50 8 µs) and the submit's CPU now sits on the worker.

**A/B** (`scripts/perf/quiet.sh ram-sqz`, 20 s rows, the whole venue held, `tctl_c` 58–64 °C at every
start; A = the branch's base `830e02d` in a worktree of its own, B = this branch; `MX_ROWS='sw1m|sw4m|
sr1m'`, legs A B B A back to back — the sessions `seqbase-{A,B}-r{1,2}`; mean B ÷ mean A, daemon cores
A → B). ext4/xfs are `rung-10-localfs-comparison.md`'s rows on the same substrate (ext4 re-measured
today at 70,527 / 83,044).

| row | ext4 | xfs | A (legs) | B (legs) | B ÷ A | daemon cores |
|---|---|---|---|---|---|---|
| seq write 1M QD16×4 (MiB/s) | 70,480 | 70,563 | 49,320, 48,844 | 49,123, 49,546 | 1.005 | 6.12 → 5.60 (−8 %) |
| seq write 4M QD4×4 | — | — | 43,124, 39,862 | 41,205, 40,255 | 0.98 (A's spread 8 %) | 4.76 → 4.27 (−10 %) |
| seq read 1M QD16×4 | 81,833 | 82,903 | 72,522, 69,402 | 70,224, 71,306 | 0.997 (A's spread 4 %) | 3.85 → 3.56 (−8 %) |

Against `dev`'s head of the hour (`a1ebd3a`, carrying the 4k-write lever's Shared-class prelude — legs
`seqab-{A,B}`): dev 50,679 / 51,393 on sw1m where this branch at its base read 49,970 / 50,010; the two
compose at the rebase (both are on the branch now). **The levers hold bandwidth and take 8–10 % of the
daemon's CPU; parity was not reached: 70 % on the write row, 87 % on the read row.**

**The binder, named.** Parity was not reached; what stands between 49 and 70 GB/s is (a) the daemon's
≈ 290 µs of `to_store` + `to_commit` per write — the handler's prelude (two KV lookups, four token
reads, the overlay claim: ≈ 45 µs of CPU, the same per 4 KiB — the 4k-write lever's territory) and the
pass structure that runs it serialized with the ring's work (`to_store`'s p90 ≤ 1 ms against a p50 ≤ 64);
(b) the zero-copy transport's per-page kernel bookkeeping on the worker thread — the install of 256 bvecs
at delivery (a kmalloc + memset per request), the commit's 256 unpins (+ dirty-locks on a read) and the
unregister, ≈ 20 µs per request and 28 % of the read row's worker: a large-folio path (`fuse_get_user_
pages` builds one folio per page even when the app's buffer is THP-backed) or a deferred release — a
sqz series patch; (c) fio's single thread per job: ≈ 430 µs of every write's clat is a filled CQE
waiting for fio to finish submitting, and a FUSE submit costs it ≈ 10 µs more than ext4's — the kmbuf
select under the daemon ring's `uring_lock` for a zc WRITE that selects no buffer is the series' half of
that. The ceiling under today's fio with the kernel path at ext4's 300 µs would be ≈ 58 GB/s on this row
(fio's 60 µs per write); parity needs (c) as well as (a).

**Suites** (`scripts/venue test`): transport / write_admit / device_overlay / data.plane / nvme_dev /
kv_fs / e2e 366 pass (2 skipped), the pieces rows 52/52, overlay_core 64/64, rails 29/29. Smoke rows
green: the data plane WITH / WITHOUT a cache path, A STREAMING OVERWRITE NEVER WEDGES THE DAEMON, `sync
-f` IS A DURABILITY POINT (43/43), THE KILL MATRIX (40/40, fsck clean); every perf session's `fsck
--offline` clean, `direct-store failures 0`, `bridge cancels 0`.

## Wave 2 (2026-10-20, `perf/seq-parity-2`): the split re-taken, the hop lever, the kernel lever

**The split on `329a3c9`** (the read view `7c7b0f8` and the Shared-class prelude `6eebd2c` landed between the
waves; same rows, `trace.bt` on the write row, one session each). 1M seq write ×4 **52.4 GB/s** at 4.93
daemon cores (was 49–50 at 5.6–6.1): fio clat 1,157 µs = the kernel's view **711** (was 805) + **446** of a
filled CQE waiting on fio's thread; the kernel's: delivery 69, the daemon 572 (was 680) — `to_store`
**92** (was 198–238; p50 ≤ 32, p90 ≤ 256: the prelude is two read-view hits and no token read, 16 µs of
CPU on the worker where it was 45), `store` 485 (the device, unchanged), `to_commit` 50 — and
`fuse_uring_req_end` 11. The worker's 68 µs per write: the handler 23 % (was 46), the inline two-piece
`WRITE_FIXED` 17 % (`bio_split_io_at`'s 255-bvec scan 4.8 %, the sg table 5 %), the commit + delivery
(`io_uring_cmd`) 17 % (`unpin_folio` 5.9 %, `io_buffer_unregister` 3.8 %, the imu memset 3.4 %), the local
task work 14 %, `schedule` 5 %. The read row: 46 µs per 1 MiB read, **52 % of it the per-page terms**
(`unpin_folio` 15.2, `folio_mark_dirty_lock` + `folio_unlock` 7.4, `io_buffer_unregister` 4.8, memset 5.6,
`io_buffer_register_bvec` 3.2, `bio_split_io_at` 6.1, the sg table 7.8, kmalloc 2.2); the io-wq core of
the first wave is gone (0.00 `iou-wrk`; it WAS the NOWAIT split punt of every 1 MiB `READ_FIXED`),
`transport_fd_body_fallbacks` 0, the probe and the recheck two view hits per read. What remains
serialized on the worker: the kernel's per-page zero-copy bookkeeping (the install's 256-bvec imu, the
commit's 256 unpins), the two-piece submission, the handler's 16 µs.

**Correction to binder (c).** The daemon ring's `uring_lock` is NOT taken for a kmbuf select on a zc
WRITE: `fuse_uring_req_has_copyable_payload` already answers false for `in_pages` with two in-args, so
`fuse_uring_prep_buffer` returns before any select. fio's extra ≈ 10 µs per FUSE submit is the request's
build — `fuse_io_priv`, the 256-entry folio and descriptor arrays, the `pages[256]` scratch, the request
— and the 256-iteration descriptor loop; 0033 shortens the loop's product, not the allocations.

**Daemon levers measured.** (1) `SQUEEZEFS_FUSE_RINGS_PER_QUEUE` 2 / 3 / 4 (admitted by `b17f162`) on
sw1m ×4: 53.4 / 50.9 / 52.0 GB/s at 4.93 / 5.00 / 5.41 cores — the queue's depth over more workers buys
nothing on the sequential rows (the device phase stands); the derivation stays 2. (2) **The streaming
write's lane: `own`** (`211ab11`, `3ed3f5b`): the overlay class always on its delivering worker's own lane,
past the own-depth share — with the handler at 16 µs against a ≈ 500 µs device phase, a spill is two
wakes each way for nothing. Legs own cpu cpu own: sw1m 52,675 / 52,403 vs 52,107 / 51,070 (**1.018×** at
4.69 vs 4.83 cores), sw4m 41,418 / 48,244 vs 40,396 / 46,168 (1.036×; the row's own spread 15 %), sr1m
untouched by construction (no held write; its 63–66 vs 65–66 is the row's session swing). Ships; W1
keeps the share (the 4k write knee). (3) The commit's unpins cannot be deferred by the daemon: they run in
`fuse_request_end` inside the kernel's COMMIT processing, before the request's completion reaches fio —
a kernel lever (0033 below, or `IOCB_DIO_CALLER_COMP`: the release in fio's completion task work). (4) The
kmbuf select: already skipped (above).

**Kernel lever 0033** (`scripts/kernel/proposed/0033-sqz-fuse-coalesce-large-folio-user-pages.patch`,
`e11d0d5`; compile-checked W=1 as the whole fuse module — fuse.ko, cuse.ko, virtiofs.ko — against 7.2.8 +
0001–0030 + the proposed 0031, and applying to the pristine tree; unbooted). `fuse_get_user_pages` records
one descriptor PER PAGE even inside a 2 MiB THP folio ("Currently, all folios in FUSE are one page"); the
patch coalesces a page continuing the previous descriptor's folio into it, and `fuse_release_user_pages`
gives back a descriptor's pins at once (`unpin_user_folio(folio, pages spanned)`: GUP pins a large folio
once per page). Every consumer already takes a `(folio, offset, length)` triple whose length may exceed a
page — `fuse_copy_folio`, the uring copy path, the zc bvec (a multi-page bvec; `io_import_kbuf` imports
it with `iov_iter_bvec` + `iov_iter_advance`), virtiofs's `sg_set_folio`. **Expected saving, from the
trace, when the app's buffer is THP-backed (fio's is; `transparent_hugepage=always` here):** the ring
worker's CPU −≈ 24 µs of 46 per 1 MiB read (−52 %) and −≈ 17 of 68 per write (−25 %); no imu allocation
per request (1–2 bvecs fit 0031's 32-folio scratch); the DMA's bio one bvec, so `bio_split_rw`'s scan,
the sg mapping and nvmet's `bio_add_page` loop are O(1) — a shorter `store` phase; and on the ring thread
that serializes a queue's depth, latency beyond the CPU share. Nothing changes for 4 KiB-page buffers.
Probe plan in the patch header.

**A/B** (A = `dev` `561bb90` in the main tree, B = this branch `3ed3f5b`; legs A B B A, `sw1m|sw4m|sr1m`;
then the full matrix on each).

| row | ext4 | xfs | A (legs) | B (legs) | B ÷ A | daemon cores A → B |
|---|---|---|---|---|---|---|
| seq write 1M QD16×4 (MiB/s) | 70,480 | 70,563 | 53,249, 53,047 | 52,172, 53,556 | 0.995 | 4.90 → 4.66 (−5 %) |
| seq write 4M QD4×4 | — | — | 39,798, 52,921 | 52,862, 52,089 | 1.13 (A's spread 28 %: the row's two states) | 3.82 → 4.08 |
| seq read 1M QD16×4 | 81,833 | 82,903 | 62,719, 62,799 | 62,717, 66,052 | 1.03 (untouched by construction) | 2.93 → 3.00 |

The full matrices (`w2full-{A,B}-r1`, the seq rows after the 4k rows): sw1m ×4 A 42,550 (p99 8.4 ms, a
disturbed leg) vs B **57,361** (81 % of ext4), sw4m 46,526 vs 40,553, sr1m ×4 73,373 vs 71,052 (87–89 %),
sr1m ×1 53,650 vs 52,617, sw1m ×1 33,548 vs 34,075; the 4k and 64k rows within their known session swings
(rr4k QD1 / QD32×1 / QD32×4 0.92 / 0.84 / 1.01, rw4k 1.22 / 1.01 / 1.03, 64k 1.01 / 1.00 — one pair each,
the QD1 and QD32×1 rows swing 12–43 % between sessions of one binary; the lever touches no read and no
W1 write by construction). **Standing after wave 2: 1M seq write 53–57 GB/s = 75–81 % of ext4 at 4.7
daemon cores (the first wave's 49–50 at 5.6–6.1); 1M seq read 63–73 = 77–89 %.** The next 20 % is the
kernel's: 0033's per-folio bookkeeping on the ring thread (≈ 17–24 µs of 46–68 per request), and fio's
own submit cost through FUSE (≈ 440 µs of every clat is a CQE waiting on fio's thread).

**Suites** (`scripts/venue test`, the rebased branch): transport / write_admit / device_overlay / data.plane /
nvme_dev / kv_fs / e2e 351 pass (3 skip); placement + knobs 81/81; the overlay-class and pieces e2e rows,
the SQPOLL row 67/67 alone (one KILL by the hang limit inside a 190-test shard under a loaded host — not
reproduced); rails 29/29. Smoke rows green: the data plane WITH / WITHOUT a cache path, A STREAMING
OVERWRITE NEVER WEDGES THE DAEMON, `sync -f` (43/43), THE KILL MATRIX (40/40, fsck clean); every perf
session's `fsck --offline` clean.
