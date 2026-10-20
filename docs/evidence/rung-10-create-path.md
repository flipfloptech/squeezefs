# Rung 10 — the create path: the metadata engine under mdtest (2026-10-20)

**The bar** (owner, 2026-10-19): on the in-memory substrate SqueezeFS is the fastest of {ext4, xfs,
SqueezeFS} or on par, every row. This row: `mdtest -np 32 -n 10000 -u` file creation 41.2 k/s against
ext4's 711 k and xfs's 471 k (`rung-10-localfs-comparison.md`); removal 36.7 k against 573 k / 183 k.
Venue: this workstation (32 threads, `7.2.8-9.30-omarchy-sqz`), the RAM NVMe set (2 MDT + 2 OST),
the venue held, every phase started at Tctl ≤ 70 °C, one benchmark at a time. Scratch and raw data
under `~/tmp/createpath/` (`session.sh`: one mdtest phase at a time with `.stats` and per-thread CPU
before and after, `perf record -a -g` mid-phase; `sdiff.py`: the per-op split).

## The split (dev `830e02d`, file creation, 320,000 creates in 7.3 s)

- **FUSE**: 2.00 ops per create — one negative LOOKUP (28 µs) and the CREATE (664 µs mean); no
  FLUSH (the no-open posture). 1.3 handler fibers per create on the drain worker's own lane; 0.31
  probes demoted (the stripe verdict's try-only lock, 12 %).
- **The engine**: 1.005 journal entries, 225 B of journal and 257 B of device per create; 12.2 node
  reads (cache hits) and 12.7 record decodes per create; 3 token-plane local reads (0 RPCs); the
  mint: 0.85 rotor and 0.15 affinity per create; the 4a guards never waited (34 waits in 320 k).
- **Where the wall went**: the daemon ran 2.5 of 32 cores. The volume's journal lane (`sqz-jrnl0`;
  every file of the tree homes on one volume — children stay with their directory, §5.4) ran 95 %
  of a core: 21.8 µs of CPU per create, and the pass's phases closed the create's latency — queue
  wait 107 + pass 182 (for 9.1 txs) + window 243 µs. The lane's profile: the RAM apply 54 %
  (`remergeStable` 22 %, the snapshot's destroy 15 %, the pre-image lookups 15 %), the acks' wakes
  10 %, resolve 5 %. 32 ranks closed-loop over 730 µs of latency IS 44 k/s: the lane was the
  ceiling, the committers idle.
- **Why the apply was that dear**: a scratch probe of the overlay at apply read mean 230 records,
  max 4,483, 17 % of the applies on a leaf past 360. The parent's Δ(mtime, ctime) merge record
  landed on the leaf every child of the directory folds — the 32 rank directories' inodes sit in one
  leaf, 43 k Δs/s between two writebacks — and every apply re-merged the whole overlay into a fresh
  stable run every 8 records (an 80 KiB array mmap'd and unmapped with a TLB shootdown across the
  daemon's threads).

## Lever 1 — the parent's stamp is parked (decision 0003 §AV; `3c3ceb6`)

A create, symlink or unlink of a non-directory child stages inode + dentry — two records on fresh,
exclusively guarded keys — and PARKS the parent's stamp (`PendingTimes.park` under the shared
`I{parent}` guard, before the commit), the drain / `fsyncdir` / the dismount journaling it as
§L.6's write stamps. Red test: 64 creates = 64 entries of 2 records, the parent's stored record
unmoved, its served `mtime`/`ctime` advanced, a mkdir's exclusive Put carries the fold, fsyncdir
drains ONE entry, strict mode stages the Δ as before. Measured: creates 43.7 k → 61.9 k/s, removals
38.2 k → 53.3 k/s; journal 225 → 182 B per create; the lane 21.8 → 15.1 µs per create (still 93 % of
a core).

## Lever 2 — the apply leaves the lane (decision 0003 §AW; `a2350ce`)

The pass drains, recalls, admits, RESERVES and stamps, then hands the window to an applier fiber
(a fixed set of four handler lanes, `clamp(cpus/8, 2, 4)`) that resolves, locks the union, captures
the pre-images, applies, encodes and submits; the heap admission is the committer's before it
enqueues; the lane completes the reservation after the landing, after the apply (the tail law
stands); the tail fast path is per key; the §L hold rides a window on the device, ends at the next
landing, its gate's line scales with the batch (`c ≥ n/3`), and a batch drained with the appliers
full waits for one to leave. The contracts' applier is inline (stage A whole, as before).

What the measurements said on the way (all `-np 32 -u`, one session each):

| variant | creates/s | removals/s | lane µs/create | note |
|---|---|---|---|---|
| lever 1 alone | 61.9 k | 53.3 k | 15.1 | the lane at 93 % |
| + the committer as applier (its own worker lane) | 56–60 k | 53–63 k | 4.7 | the §L hold held 62 % of the passes 170 µs each; the appliers' leaf locks held 114 µs per 1.4-entry window at 32 concurrent, and the workers' FUSE queues stalled behind them |
| + no hold at all | 57 k | 63 k | 10.5 | 230 k windows of 1.4 entries; `leaf_lock_hold` 114 µs |
| + appliers on the 2 `sqz-meta` lanes | 63.7 k | **33.8 k** | 5.8 | the reclaim's destroy batches share the pool |
| + appliers over all 32 handler lanes | 63.1 k | 59.4 k | 6.4 | cold threads: the apply's CPU doubled |
| **+ four pinned handler lanes (landed)** | **68.4 k** | **63.1 k** | 6.3 | `leaf_lock_hold` 67 µs per 2.1 entries; create 375 µs |

The lane is no longer the ceiling (`sqz-jrnl` 2.0 s of a 7 s phase). The ceiling now: the apply's
intrinsic cost on the appliers (15–17 µs of CPU per create: the O(overlay) re-merge and snapshot
destroy, the pre-image lookups, two allocations per record, a snapshot publish per record group)
and the closed loop's latency (LOOKUP 41 µs + CREATE 375 µs at 32 ranks).

## A/B against dev

TBD-AB

## The remaining gap and what closes it

1. **The overlay's shape** (the largest term, every variant): `NodeDirty.overlay` is one sorted array
   re-cloned into the stable run every 8 applies and freed with the previous snapshot; a leaf that
   outruns its writeback holds thousands of records. A persistent run (immutable chunks shared
   between snapshots, an insert copying one chunk) makes an apply O(log n) and the publish O(1):
   the apply's CPU per create drops from ≈ 15 to ≈ 3 µs by the profile's shares. With it the
   applier venue carries ≥ 1 M txs/s and four appliers hold their leaves microseconds, not tens.
2. **The closed loop**: at 32 ranks the rate is 32 ÷ (lookup + create latency). After 1, the create
   is ≈ the window round trip (device 14 µs + the lane's issue and ack ≈ 50 µs) plus the handler's
   own ≈ 60 µs: ≈ 110 µs ⇒ ≈ 250 k/s at 32 ranks; ext4's 711 k needs the handler's pre-commit work
   cut too (the `user.squeezefs.gather` xattr range per create, the stripe verdict's process-wide
   mutex, `allocateIno`'s lease-table scan and allocation per mint — ≈ 10 µs of the worker's 40).
3. **The ack fan-out** (one eventfd write per member on the lane, 10 % of it): the applier parks for
   the landing and fans the window's acks out itself.
4. **Directories**: a mkdir's parent nlink shift stays an exclusive Put per op on the shared leaf
   (fsck's census forbids parking it); 1 is what moves that row.

## Suites

Unit families (`conveyor`, `meta.backend`, `kv_fs`, `kv_fs_stripes`, `economy`, `meta.tx`,
`journal_lane`, `lifecycle`, `e2e`, `fsck`, `node_cache`, `exec`): 535/564 passed, 29 skipped. Smoke
rows (THE KILL MATRIX and its two-ring row, A STRIPED DIRECTORY ON ONE DAEMON, THE SHARED HOT
DIRECTORY ACROSS DAEMONS, A DEAD MANAGER HOLDING A STRIPED HOT DIRECTORY, kill -9 mid-tar on both
cadences, KILL -9 BETWEEN A PROMOTION'S COMMIT AND ITS RING RELEASE): 46/46. `fsck --offline` clean
after every session. No full gate (the train gates).
