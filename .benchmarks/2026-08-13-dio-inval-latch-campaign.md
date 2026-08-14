# 2026-08-13 — the DIO-inval quiescent latch: evidence note (NOT MERGED)

Branch: `perf/dio-inval-latch` (parked, DO NOT MERGE — bistability unresolved).
Venue: strixhalo devsub-**tcp** (fresh substrate teardown/create per cell after the
zram-aging conviction below). Instrument: fio, invocations verbatim in the rows.
Binaries: A = dev `87b2b7ba` (tokio-free tip); L = A + the quiescent-lineage
inval latch (this branch).

## What is PROVEN (counted, clean cells)

1. **The per-write kernel invalidation is real and enormous**: on dev tip,
   `fuse_dio_write_invals` ≈ every O_DIRECT write on any ino with buffered-open
   history (fio's layout arms it; nothing disarms until FORGET). A 25 s
   rand-4k row pays 200k+ AWAITED sideband round trips, each serialized on the
   kernel's per-inode invalidate mutex.
2. **The latch works mechanically**: 1 inval / 43k ops measured; the 7-test
   coherence suite (incl. the two new red-first rails: quiescent-storm-elides,
   read-serve-unlatches) is green; the generic/451 law arms are unchanged
   (live buffered handle ⇒ per-segment ranged invals).
3. **psync rows: the latch is a large clean win** (no kernel aio involved):
   32-thread separate-files **77.2k → 150k IOPS** (invals 2.3M → 32), p99
   1500 → 1156 µs at 2× throughput. One-file psync 7.4k → 9.0k.
4. **libaio one-file qd32: the latch is BISTABLE** — a ~2.3k mode
   (2034/2199/2283/2440/2473/2625/1540) and a ~30–44k mode
   (30.2k/30.6k/31.5k/33.9k/44.1k/44.2k/44.4k), against A's stable
   15.6–20.9k. The apparent flip variable is FILE LINEAGE: same-invocation
   fio layout→run tends to the 2.3k mode; prior-invocation layout (or
   create_only + stat) tends to the 30–44k mode. NOT yet 100 % attributed.
5. **In the collapsed mode the wall is daemon-side**: up to 32 WRITEs
   concurrent in the daemon (`fuse_write_inflight`), `block_lock_wait` 0.6 µs,
   but `write_lock_wait_exclusive` (the `active_inode_locks` fair-FIFO guard)
   mean **23 ms** vs A's 1.2 ms on the same lock/binary-family — the exclusive
   handoff cycles ~20× slower without A's inval-induced getattr interleave
   (A's request mix is ~48 % writes / ~52 % kernel getattrs; L is pure writes).
   Little's law: ~26 ops resident in the guard queue at ~900 µs/handoff.

## Theories FALSIFIED (counted)

- **Stale kernel attrs serialize aio submits**: 10 Hz stat loop → still 3.6k.
  60 s attr TTL + pre-stat → wash within the fallocate-shape cells (44.2 vs
  44.4k). fio parks in io_getevents; io_submit never blocks.
- **fallocate (unmapped) vs written (mapped) layout as the collapse trigger**:
  both shapes hit both modes.
- **Kernel starves the daemon**: disproven by the in-flight histogram.
- **Shared-write admission as the day-1 wall**: the Shared probe DOES refuse
  ~91 % on this shape for the first ~15 s (candidate counters; engages late,
  +23 % when it does) — real but secondary, and orthogonal to the latch.

## Instruments/venue lessons (bank these)

- **zram substrate AGES across `format`** — only teardown/create resets
  physical pages; several early "collapse" rows were full-zram artifacts and
  the 32×1G psync layout blew capacity into ENOSPC (`fio err=28`).
- **A failed mount poisons the RAW MOUNTPOINT DIRECTORY** (fio writes it at
  RAM speed → fake 30k+ "recoveries", then `mountpoint not empty` MOUNT-FAILs
  forever after). Every cell must scrub `/mnt/…/*` and assert `mountpoint -q`.
- The every-≥30k-IOPS "recovery" rows measured BEFORE the mountpoint guard
  are INVALID except where re-reproduced under it (the 44k fallocate cells
  and 30k psync-laid cells were re-run clean).

## The open question (next session's first probe)

Why does the fair exclusive inode-guard handoff run ~20× slower in L's pure
write stream than in A's write+getattr interleave — and what lineage variable
flips L between its two modes? Candidates left standing: attr/metadata cache
expiry moving `fetch_metadata` under the guard per-op (the publish-elision
TTL note in `publish_attr`), and a guard-queue wake/poll venue interaction at
depth. The decomposition instrument set is proven: `fuse_write_phase_ns` +
`write_lock_wait_*` + the per-op counter-rate diff (script shapes in this
session's terminal logs).

## The other banked finding (independent of the latch)

Multi-file writes scale: 16 files @ qd8 = 90–103k on this box. The single-file
aio wall (A: ~16–21k) remains the campaign target; the Shared-admission
late-engagement (91 % refusals, first ~15 s) is a named secondary lever.
