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
