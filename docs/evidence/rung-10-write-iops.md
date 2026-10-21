# Rung 10 — 4 KiB random writes: the convoy behind 21 % of ext4 (write-iops)

The owner's bar (2026-10-19): on an in-memory substrate SqueezeFS is the fastest of {ext4, xfs,
SqueezeFS} or on par, on every row. The 4k randwrite row (fio io_uring O_DIRECT, QD32 × 4 jobs into
4 × 3 GiB laid-down files) read 415 k IOPS at 11.0 host cores against ext4's 1.96 M at 8.1
(`rung-10-localfs-comparison.md`). Venue: the workstation (`local`, `7.2.8-9.30-omarchy-sqz`,
RAM-NVMe over nvme-loop, `scripts/perf/quiet.sh ram-sqz`, 20 s rows, every row started at Tctl
≤ 70 °C; dist builds; the whole venue held per session). A = `dev` 81e9f21 from a pinned worktree, B
= this branch rebased on it. Sessions `ram-sqz-rw4-*` in `/build/perf/mx/` (`results.tsv`).

**The split, profiled** (`rw4-prof-r1`, dev 830e02d, QD32×4: 437.6 k at 11.42 host cores = 26.1 µs
per write; `perf record -a -g` mid-row, `incl.py` per thread class; the `.stats` deltas over 8.75 M
writes). fio 3.0 µs, the nvmet-loop/brd kworkers 2.8 µs, the daemon 21.5 µs: the uring workers 13.4
(kernel inside `io_uring_enter` 7.8 — the FUSE commit cmd 1.3, the `WRITE_FIXED` issue 1.1 and its
kworker kick 0.7, task work 1.1, cq-wait + schedule 1.9, 3.21 enters per write — the own handler
3.7 averaged over the 32 % of writes it ran, the pass's own userspace ≈ 1.7 of which clock reads
0.7 and the owed spin 1.2) and the handler lanes 8.0 averaged (11.7 per spilled write: the handler
fiber 10.2 — `writeStriped` 6.6: two `readInodeOpt` 2.4, `WritePath.write` 3.3 of which `mapLookup`
1.3 and the store request's eventfd write 0.65, the (3.7) layout lock ≈ 1.6 WITH ITS PARK — the
dispatcher, op guard and reply's eventfd write ≈ 3.6; the lane thread's own park/wake 1.5). Ledger
per write: KV lookups **4.0** (the layout kind, the inode before and after the data, the plane's
binding), divert asks 4.0, journal entries **0** (808 per 8.75 M: §5.2 holds — the comparison note's
"a journal entry per write" was wrong), patch DMAs 1.0 of exactly 4,096 B (`patch_edge_rmw_reads` 0:
no read-modify-write), held stores ≡ writes, enters 3.21, passes 2.55, sleeps 1.10 (worker) + 0.21
(lane), own 32 % / spilled 68 %, cross-lane wakes 1.75, eventfd wake writes 1.09, **timer arms
1.02**, owed spins 0.50 (all hits, 2.4 µs each). Nothing saturated: fio's threads 31 %, the eight
workers 0.74 cores each, the lanes 0.11 — yet p50 = 272 µs with 128 in flight.

**The finding.** The timer arm per write was a `tick.parkUntil` under `RwLock.acquire` inside
`KvFs.write`: the **(3.7) layout lock** (`data.lockLayout`, an exclusive per-ino `sync.Mutex`) taken
at the top of EVERY write for the inline, staged and promotion layout RMWs, held across the kind
lookup and the inode probe (≈ 1 µs). Its FIFO courtesy (`tryAcquire` refuses while anyone is queued,
a woken waiter re-contends only once its lane runs it) convoyed one file's 32 writers: once one
queued, every arrival in its wake latency queued behind it, and the convoy never drained — a ticked
fiber park (a timer arm, a cross-lane wake) per write, and the whole pipeline latency-bound on it.
The three KV reads around it protected the same thing the admission already proved: a Shared-class
write (charter §6.2: a resident striped write with `end ≤ size`, admitted under the ino's SHARED op
guard) cannot meet a layout transition, a size set or an extension — each holds the guard exclusive.

**What changed** (`fa1aff1`). (1) `data/shape_table.zig`: the shape row (the hold gate's and the
shared probe's snapshot) becomes a seqlock whose odd state is the writer's lock — a publisher takes
the row by CAS, a reader takes it only between two equal even reads of `seq`, past 3 attempts null
(the exclusive arm); the snapshot is now EXACT and may be load-bearing. `ShapeRowModel` proves it and
that a plain entry bump, or none, tears under a colliding publish (a create's child landing on a
sibling's row). (2) `SharedProbe.striped_passthrough` names an exact striped-passthrough snapshot;
the lifecycle passes its size as `WriteArgs.shared_size` with the Shared class. (3)
`kv_fs.writeStripedShared`: no (3.7) lock, no kind lookup, no inode read before or after the data;
the plane's FileView is the snapshot; the postlude parks the stamp alone under the shared `I{ino}`
guard (a claim of 0 under the max law leaves any parked claim standing — what the record read would
have re-parked) and republishes no shape. A foreign-slot file, a KILLPRIV write, the strict cadence
and a non-exact snapshot (a transformed set) keep the record path. Counted
`fuse_write_shared_snapshot`.

**Red → green.** `kv_fs` "a Shared-class write on the admission's EXACT snapshot reads no record
and takes no layout lock …" (red on dev: `expected 9, found 12` store reads); `shape_table` (the model
with both weakenings failing, the table's whole-publish and in-flight-publish rows). Kept green:
kv_fs 94, op_guard 47, data.plane 49, lifecycle 80, write_admit 78, sole_owner_patch 44, rails 42,
e2e 111/113 (2 skipped), shape 118/122 (4 skipped).

**A/B, cooled, one leg at a time** (legs A1 B1 B2 A2; IOPS; daemon cores; `ur` = the uring workers'
cores):

| row | A (2 legs) | B (2 legs) | B ÷ A | daemon cores A → B |
|---|---|---|---|---|
| rw4k-q1j1 | 93,660 / 91,171 | 99,577 / 73,902 | 0.94 (B spread 30 %) | 0.82 → 0.77 |
| rw4k-q32j1 | 241,548 / 221,332 | 547,114 / 398,957 | **2.04** | 2.63 → 2.78 |
| rw4k-q32j4 | 434,985 / 433,848 | **1,047,472 / 1,043,412** | **2.41** | 9.41 → 12.02 (ur 5.9 → 8.2) |
| rw64k-q16j4 | 321,521 / 326,435 | 526,281 / 532,680 | **1.63** | 8.74 → 9.75 |

B's ledger on QD32×4 (20.95 M writes): `fuse_write_shared_snapshot` ≡ writes, KV lookups **1.00** per
write, divert asks 1.00, journal entries 0, timer arms 0.07, enters **0.57**, passes 0.45, sleeps 0.056
+ 0.055, worker CPU per write 7.8 µs (from 13.4), lane 3.6 (from 8.0): the eight workers are
saturated (8.20 cores) and batch by themselves; the held write's phases (`transport_held_write_phase_ns`)
to_store 32 µs, store 29, to_commit 27 of fio's 109 µs p50.

**The own-lane depth, re-derived** (`SQUEEZEFS_FUSE_WRITE_OWN_DEPTH`; B = 8, the shipped knee; legs
interleaved: Bd1 B Bd1 B, then B0 B0; IOPS mean of the legs, daemon cores):

| row | depth 1 (2 legs) | depth 8 (4 legs) | depth 0, all-own (2 legs) |
|---|---|---|---|
| rw4k-q1j1 | 102.9 k, 0.75 | 91.6 k (74–103 k), 0.77 | 92.3 k, 0.77 |
| rw4k-q32j1 | 459 k (397–521 k), 2.93 | 515 k (399–563 k), 2.85 | 428 k (397–460 k), **2.02** |
| rw4k-q32j4 | 1,005 k, 11.98 | **1,040 k** (1,034–1,047 k), 12.04 | 1,031 k, **8.10** |
| rw64k-q16j4 | 536 k, 10.65 | 531 k, 9.75 | **553 k**, 7.48 |

The worker is the bound now, whatever the depth: 7.8 µs of worker CPU per 4 KiB write on every arm
(depth 1 trades handler CPU for the lane round trips' passes, enters and wakes — 0.95 passes and 1.18
enters per write against 0.45 / 0.57; all-own has 0.19 passes, 0.30 enters, 0 wake writes and 0.009
sleeps per write — the handler's CPU replaced the wakes' exactly) and the eight workers of four queues
cap at ≈ 1.03–1.05 M. All-own reaches it at 8.1 daemon cores (ext4's whole host: 8.1) and its p99 is
272 µs against 330, but the one-submitter row loses ≈ 15 % (one queue's two workers saturate; the
depth-8 arm borrows lane CPUs), so the knee stays 8 and 0 stays the A/B lever — the orchestrator's
call under "a regression is a defect".

**Tried and dropped: the batching park.** A pass owing ≥ 2 page-sized completions would park for all
of them in ONE enter (`IORING_FEAT_MIN_TIMEOUT`'s `min_wait_usec`, `cq_wait_nr = n` on a
`DEFER_TASKRUN` ring) instead of per CQE — fewer, fatter passes. Its e2e row (eight writers pinned to
one CPU) measured the premise false: the ring owes ≈ 1 completion at a time (a RAM device lands a
store faster than the handler runs the next; a commit's fetch returns a delivery only once fio
resubmits), so there was nothing to batch; with the convoy gone the saturated worker batches by
itself (0.57 → 0.30 enters per write). Removed, not shipped.

**The full matrix, cooled** (legs A3 B5 B6 A4, one leg at a time; mean B ÷ mean A; daemon cores):

| row | A legs 1/4 | B legs 2/3 | B ÷ A | daemon cores A → B |
|---|---|---|---|---|
| rr4k-q1j1 / q32j1 / q32j4 | 98.8 / 78.3 k · 560 / 433 k · 1,271 / 1,254 k | 110 / 127 k · 563 / 554 k · 1,242 / 1,251 k | 1.33 · 1.12 · 0.99 | 0.70 → 0.77 · 2.01 → 2.01 · 7.99 → 8.00 |
| rw4k-q1j1 / q32j1 / q32j4 | 80.7 / 80.2 k · 234 / 238 k · 433 / 433 k | 91.3 / 94.1 k · 555 / 553 k · **1,031 / 1,055 k** | 1.15 · **2.35** · **2.41** | 0.71 → 0.71 · 2.52 → 2.93 · 9.36 → 12.02 |
| rr64k / rw64k QD16×4 | 772 / 746 k · 330 / 296 k | 769 / 764 k · 487 / 487 k | 1.01 · **1.55** | 6.91 → 6.88 · 8.86 → 9.77 |
| sr1m ×1 / ×4 (MiB/s) | 59,000 / 58,684 · 71,574 / 71,679 | 58,408 / 58,211 · 72,020 / 72,196 | 0.99 · 1.01 | 1.98 → 1.94 · 3.96 → 3.92 |
| sw1m ×1 / ×4 · sw4m ×4 (MiB/s) | 31,020 / 30,909 · 45,755 / 46,737 · 38,706 / 38,283 | 32,566 / 32,592 · 48,770 / 45,876 · 44,871 / 41,418 | 1.05 · 1.02 · 1.12 | 2.10 → 1.85 · 5.69 → 5.09 · 4.34 → 4.38 |

No read row moved outside its spread (the reads never took the lock); every write row rose (the
sequential writes' overwrite class takes the same arm: 1.02–1.12× at fewer cores).

**The row against ext4 and xfs** (`rung-10-localfs-comparison.md`, same substrate): 4k randwrite
QD32×4 ext4 1,963 k at 8.1 host cores, xfs 1,941 k, SqueezeFS **415 k → 1,040 k** (depth 8, 16.9 host
cores of which the daemon 12.0) or 1,031 k at 12.5 host cores (all-own, daemon 8.1) — **21 % → 53 % of
ext4**; the wave's ≥ 1.0 M bar is met. Smoke rows green on the branch: KERNEL AIO on the plain mount,
the data plane WITH / WITHOUT a cache path, THE KILL MATRIX, A STREAMING OVERWRITE NEVER WEDGES THE
DAEMON.

**What remains, and the path to 1.9 M.** The bound is the uring worker: 7.8 µs of CPU per 4 KiB write
on eight saturated workers (two rings × the four queues fio's threads land on), ≈ 5 µs of it kernel
inside `io_uring_enter` — the FUSE commit+fetch cmd, the `WRITE_FIXED` issue and its nvmet kworker
kick, the completions' task work — and ≈ 2.8 µs ours (the own handler: the plane's binding lookup
with its divert ask and route-table RMW pair 1.3 µs, the op guard and `I{ino}` guard, the W1 fence
pair and the four-tier purge; the pass). In order: (1) more workers per queue — the series' patch 0030
allows N rings per queue, `SQUEEZEFS_FUSE_RINGS_PER_QUEUE` admits 3 and 4 since `b17f162` and
`geometry.ringsPerQueue` still derives 2; three or four would put 12–16 saturated workers behind four
submitters (fio's threads sit at 61 %, so ≈ 1.6 M is the submitter ceiling before fio itself binds) —
the derivation, measured on the write rows as the read agent measures the read rows;
(2) the kernel's per-op cost in the series (the commit cmd's path, the task-work hop per delivery:
the read side's 2.9 µs is the same floor); (3) the metadata engine's per-lookup cost (the route
table's arc-swap RMW, `read-cost` item 3) and a per-ino binding witness for the W1 patch (one load
instead of a KV descent: full tier, with a model). Parity through FUSE is not on this list: the
client bypass (charter §3) is the row's parity path, as the comparison note said.

## Wave 2 (2026-10-20): four rings per queue, all-own — 1.15 M → 1.50 M

Head `329a3c9` (the read view serves the write prelude; two rings, depth 8). Owner's direction:
"performance oriented first — squeeze out the last bit we can." Sessions `ram-sqz-rw4b-*`, the same
venue and thermal rule (one benchmark on the host, Tctl ≤ 70 °C and load < 3 before each session; the
host was shared with four other agents' builds and sessions, so the legs waited and a few are single).

**The split, re-profiled** (`rw4b-prof-r1`, QD32×4, 1,149.6 k at 16.6 host cores = 14.4 µs per write;
22.99 M writes). The daemon 10.0 µs: the uring workers **7.05** (8.10 cores, saturated) + the lanes
2.96 averaged (4.8 per spilled write; 62 % spilled at depth 8). The worker's 7.05: kernel inside
`io_uring_enter` **4.7** — the FUSE commit+fetch cmd 1.4 (its `queue->lock` 0.25 of it), the
`WRITE_FIXED` issue 1.3 (`blk_mq` → nvme-loop → the nvmet workqueue's pool lock 0.26 and its kworker
kick 0.2: the substrate's, paid by ext4 in fio's context), the completions' task work 1.1, cq-wait
0.45, the eventfd reads/writes 0.3 — and userspace 2.35: the own handler 1.76 averaged (4.6 per own
write: `WritePath.write` 0.9 of which the W1 fence pair, the block lock, the `I{ino}` guard and
`resolveAt` 0.3; the store request's inbox push and the fiber's park), the reap, dispatch and fiber
spawn 0.7, the pass 0.17, clock reads 0.23, counters 0.1. Ledger per write: KV lookups **0.0005**
(the read view: 1.00 hits), divert asks 0 (lever (b), the route-table RMW, is off the write path
already), journal entries 0, enters 0.44, passes 0.37, sleeps 0.04 + 0.08, timer arms 0.07, own 38 %,
cross-lane wakes 1.29, wake writes 0.27, owed spins 0.08; held-write phases to_store 24.7 µs, store
30.6, to_commit 21.1 of fio's 101 µs p50. Lever (d): the device write is issued inline on the worker
(`blkdev_direct_IO` under `io_write`, no `IOSQE_ASYNC`, 31 io-wq samples of 150 k on the host), so
the 4k patch pays no punt; the 1.3 µs is the block layer plus the loop target's hand-off.

**Lever (a), rings per queue × the own depth** (the knob pair on one binary — `SQUEEZEFS_FUSE_RINGS_PER_QUEUE`
2/3/4, `SQUEEZEFS_FUSE_WRITE_OWN_DEPTH` 8/0 — legs interleaved, cooled; IOPS per leg; daemon cores;
`tctl` start>end per row in `results.tsv`):

| arm | rw4k-q32j4 | daemon cores | rw64k-q16j4 | rw4k-q32j1 | rw4k-q1j1 | worker µs/write · enters · spins per write |
|---|---|---|---|---|---|---|
| 2 rings, depth 8 (dev) | 1,147 k / 1,145 k | 11.58 / 11.56 | 579 k / 578 k | 604 k / 427 k | 123 k / 90 k | 7.07 · 0.46 · 0.06 |
| 3 rings, depth 8 | 1,270 k / 1,220 k | 14.85 / 14.37 | 626 k / 633 k | 416 k / 725 k | 107 k / 101 k | 8.88 · 0.94 · 0.21 |
| 4 rings, depth 8 | 1,255 k / 1,230 k | 17.28 / 17.01 | 636 k / 607 k | 768 k / 436 k | 123 k / 122 k | 10.92 · 1.49 · 0.78 |
| 3 rings, all-own | 1,396 k | 11.66 | 678 k | 694 k | 117 k | 8.33 · 0.54 · 0.12 |
| **4 rings, all-own** | **1,506 k / 1,504 k** | 14.83 / 14.83 | 681 k / 675 k | 430 k / 761 k | 89 k / 97 k | 9.83 · 0.86 · 0.50 |
| 4 rings, all-own, no spin | 1,522 k / 1,504 k | 14.51 / 14.58 | 682 k / 678 k | 425 k / 428 k | 110 k / 111 k | — |
| 3 rings, all-own, no spin | 1,409 k | 11.67 | 691 k | 706 k | 110 k | — |

At depth 8 a third or fourth ring buys 7–9 % for 25–50 % more cores: the workers come off saturation
and go back to the per-CQE regime (enters 0.46 → 1.49, spins 0.06 → 0.78 per write — 3.9 of the 4-ring
arm's 14.8 worker cores were the owed spin). All-own fills those gaps with the handler: three rings
reach 1.40 M (1,396 / 1,397 k) at the two-ring arm's cores, four rings 1.50 M (fio at 89 %) at a
proportional 14.8. The owed spin is neutral on every arm (the no-spin rows), as the owner found
before; it stays. The one-submitter row keeps its two placement modes on every arm (0.42–0.43 M and
0.69–0.77 M). **The read rows decide between 3 and 4** (all-own, cooled, one leg each; the two-ring
numbers are `rung-10-read-view.md`'s):

| arm | rr4k-q1j1 | rr4k-q32j1 | rr4k-q32j4 (daemon cores, fio %) | rr64k-q16j4 |
|---|---|---|---|---|
| 2 rings (dev) | 126 k | 713 k | 1,741 k (7.9, 83 %) | 929 k |
| 3 rings, all-own | 114 k | 772 k | **1,902 k** (11.3, 96 %) | **963 k** (7.9) |
| 4 rings, all-own | 141 k / 142 k | 856 k / 806 k | 1,810 k / 1,807 k (13.5, 96 %) | 928 k / 909 k (8.8) |

Three rings take the 4k read row to 96 % of ext4 (1.98–2.03 M, fio-bound there at 99 % as here at
96 %) and the 64k read row above ext4 (903 k); the fourth ring gives 5 % of it back (fio and four
workers on one `queue->lock`) for 20 % more cores, and buys 7 % on the 4k write row. **Shipped: three
rings and depth 0** (`c680fb4`): `RINGS_PER_QUEUE_DERIVED = 3` with the reason on the line — the
workers one submitter keeps busy, the daemon's per-op cost over the submitter's (≈ 7 µs against
fio's 2.5), rounded up; a static cores ÷ queues reads 1 on every host (the queues are the CPUs,
charter §4); 4 stays the write row's lever. The knee of 8 was the one- and two-ring measurement, and
with the convoy gone (wave 1) the own handler is what a half-loaded worker should be doing.

**A/B, the shipped defaults** (A = `329a3c9` from a pinned worktree — two rings, depth 8; B = `c680fb4`
— three rings, depth 0; one leg at a time, cooled; IOPS per leg, (max − min) ÷ mean, daemon cores, Tctl
start>end per leg):

| row | A legs | B legs | B ÷ A | daemon cores A → B | tctl A / B |
|---|---|---|---|---|---|
| rw4k-q1j1 | 123 / 90 / 120 / 123 / 121 k (29 %) | 117 / 108 / 125 / 123 k (14 %) | 1.03 | 0.75 → 0.73 | 55>69 56>59 60>70 58>70 64>65 / 66>69 64>68 59>63 62>69 |
| rw4k-q32j1 | 604 / 427 / 596 / 599 / 598 k (31 %) | 694 / 688 / 697 / 701 k (2 %) | **1.23** | 2.77 → 3.00 | 69>62 59>72 70>60 70>59 65>60 / 69>67 68>64 63>60 69>68 |
| rw4k-q32j4 | 1,147 / 1,145 / 1,139 / 1,135 / 1,150 k (1 %) | **1,396 / 1,397 / 1,393 / 1,404 k** (1 %) | **1.22** | 11.58 → 11.67 | 62>67 53>70 60>68 59>68 60>68 / 67>74 64>71 60>69 68>71 |
| rw64k-q16j4 | 579 / 578 / 568 / 570 / 570 k (2 %) | 678 / 684 / 684 / 682 k (1 %) | **1.19** | 9.25 → 9.39 | 67>71 70>71 68>76 68>71 68>70 / 54>77 70>79 69>72 70>75 |

**The full matrix** (legs A4 B7 B8 A5; mean B ÷ mean A; MiB/s on the sequential rows):

| row | A legs 1/4 | B legs 2/3 | B ÷ A | daemon cores A → B |
|---|---|---|---|---|
| rr4k-q1j1 / q32j1 / q32j4 | 142 / 114 k · 800 / 467 k · 1,750 / 1,685 k | 130 / 127 k · 502 / 917 k · **1,879 / 1,879 k** | 1.01 · 1.12 · **1.09** | 0.72 → 0.66 · 2.00 → 2.77 · 7.77 → 11.20 |
| rw4k-q1j1 / q32j1 / q32j4 | 100 / 92 k · 594 / 520 k · 1,148 / 1,025 k | 97 / 99 k · 694 / 698 k · **1,402 / 1,401 k** | 1.02 · 1.25 · **1.29** | 0.69 → 0.69 · 2.82 → 3.00 · 11.11 → 11.67 |
| rr64k / rw64k QD16×4 | 920 / 883 k · 572 / 520 k | 964 / 959 k · 697 / 681 k | 1.07 · **1.26** | 6.15 → 7.88 · 8.81 → 9.43 |
| sr1m ×1 / ×4 | 51,577 / 49,141 · 72,576 / 67,897 | 58,481 / 58,656 · 70,683 / 70,608 | 1.16 · 1.01 | 1.60 → 1.94 · 3.18 → 3.46 |
| sw1m ×1 / ×4 · sw4m ×4 | 33,990 / 32,638 · 48,789 / 48,667 · 41,046 / 42,179 | 34,008 / 33,805 · 58,332 / 58,283 · 42,072 / 40,182 | 1.02 · **1.20** · 0.99 | 1.62 → 1.62 · 4.48 → 5.10 · 3.55 → 3.46 |

No row regresses: the one-submitter read row keeps its two placement modes on both arms (A 800 / 467 k,
B 502 / 917 k), `sw4m` 0.99 is inside its 3–5 % spreads; the sequential 1 MiB ×4 write rose 20 % (the
held overlay class runs on the worker's own lane too under depth 0, with three workers per queue).
The 4k write row sits at 1 % spread on four legs; the read row at 0 %.

**The row against ext4/xfs**: 4k randwrite QD32×4 ext4 1,963 k at 8.1 host cores, xfs 1,941 k,
SqueezeFS **415 k (wave 0) → 1,040 k (wave 1) → 1,400 k** (18.1 host cores, the daemon 11.7) —
**71 % of ext4** (four rings: 1,505 k, 77 %, the lever). Beside it: 4k randread QD32×4 1,879 k = 93–95 %
of ext4/xfs, 64k randread 964 k above ext4's 903 k, 64k randwrite 690 k.

**Smoke rows green on the branch**: KERNEL AIO on the plain mount, the data plane WITH / WITHOUT a cache
path, THE KILL MATRIX, A STREAMING OVERWRITE NEVER WEDGES THE DAEMON, THE READ VIEW. Suites: geometry
109, knobs 55, rails 42, transport 149, session 129 (3 skipped), e2e 113 (3 skipped), exec 142,
lifecycle 80, kv_fs 96, write_admit 78, sole_owner_patch 44, data.plane 49, shape 118 (4 skipped).

**What remains on the row, and the path to 1.9 M.** At three rings the workers are at 0.97 cores each
and fio at 81 %; the per-write worker CPU is 8.3 µs of which ≈ 5.5 kernel (the commit+fetch cmd and
its queue lock, the `WRITE_FIXED` issue and the loop target's hand-off, the task work, the park/wake)
and ≈ 2.8 ours (the own handler 4.6 per write less the lane it replaced). In order: (1) the series'
per-op cost — proposed patch 0031 (the round trip), the `queue->lock` the fetch and the submitter
share, the task-work hop per delivery (read + write pay the same ≈ 2.3–2.9 µs floor); (2) the handler's
remaining 4.6 µs: the fiber spawn and two parks per write (`Lane.spawn` 0.28, the store cell's
`futexWait` 0.3, the reply's finalize), the W1 fence pair and the four-tier purge, ≈ 12 clock reads
and ≈ 20 counter increments per write (0.3 + 0.1 µs; the exact-sum phase families are the
observability contract, so a `now` carried through the pass rather than their removal); (3) lever (c)
as measured: an all-own write's passes are 0.35 per write and its enters 0.54 — the pass boundary is
no longer where the time goes, and the batching park's premise (≥ 2 owed completions per pass) still
reads false (0.12 spins per write, each for one completion). Parity through FUSE is the client
bypass's row (`rung-10-client-direct-writes.md`: 3.0 M).

**Gate 57 on this branch (four reds), attributed.** THE WRITE-BACK TIMER UNDER A WRITER STORM failed at
`rig.stats()` with `Transport endpoint is not connected` — its daemon died mid-storm — and three unit
shards (`meta.backend` D4, `wire.tls_e2e` SCOPING, the session's late-commit seam row) ended
`terminated with signal KILL`: no `HUNG TEST:` verdict (the hang watch exits 124 with every stack), no
OOM record in the journal, four unrelated processes. Reproduced on the branch: the storm row **10/10**
fresh on the defaults (fsck clean, 0 tripwire lines; parked ≤ 8 / 32 MiB, gate waits 1.7–2.0 k, cold
flushes 1.65–1.93 k, residency max 1,049–1,068 ms), 1/1 at `RINGS_PER_QUEUE=2` and 1/1 at
`WRITE_OWN_DEPTH=8`; the three tests 5/5 each alone and 3/3 each under a parallel dist build (walls
within their cost rows once the venue's slot waits are subtracted; neither D4 nor the TLS row mounts a
FUSE session). Two later targeted runs on the box lost one DIFFERENT session e2e row each to a bare
SIGKILL, and one of this note's own `grep … squeezefs-unit` pipelines was `Killed` — the sweep the smoke
harness already names (`judgeDeath`: "a concurrent runner swept the venue, `pkill -9 -x squeezefs` /
`umount -l`"); a `bpftrace` probe on `kill(…, SIGKILL)` is armed for the next one. The
`fsck_findings` / `block_untracked_free_refusals` lines beside the storm failure are the merged-census
row's seam-ON arm, printed for the record in every green gate.

**What the gate did find** (`506527c`): a ring's kmbuf region is `nextPow2(share) × payload`, and the
ceil/rest split of 32 over three rings (11/11/10) pinned 16 + 16 + 16 MiB per queue — **+512 MiB pinned,
+570 MiB RSS per mount** (1.53 → 2.10 GiB measured on a format+mount of the gate binary), which a
16-shard gate pays sixteen times over. The shares are now halvings of the largest (16/8/8; 8/8/8/8; two
rings unchanged), so the regions never exceed the one-ring region (the tie test over depths 3–32):
a three-ring mount is +45 MiB RSS and +32 threads (1.57 GiB, 152 threads, mount 180–215 ms vs 1.53
GiB, 120, 160–180). A static cores ÷ queues bound was not taken: the queues are the CPUs, so it reads 1
on every host. Two e2e rigs pin one ring per queue by name (the SQPOLL ladder: every ring a kernel
poller; the late-commit seam: spent by the worker that ticks, which must be the owing one), and the
smoke rig's `.stats` read judges an external SIGKILL of its daemon as the venue's sweep (SKIP, loud).
