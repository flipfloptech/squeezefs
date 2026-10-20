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

## Lever 1 — the parent's stamp is parked (decision 0003 §AV; `96d8b98`)

A create, symlink or unlink of a non-directory child stages inode + dentry — two records on fresh,
exclusively guarded keys — and PARKS the parent's stamp (`PendingTimes.park` under the shared
`I{parent}` guard, before the commit), the drain / `fsyncdir` / the dismount journaling it as
§L.6's write stamps. Red test: 64 creates = 64 entries of 2 records, the parent's stored record
unmoved, its served `mtime`/`ctime` advanced, a mkdir's exclusive Put carries the fold, fsyncdir
drains ONE entry, strict mode stages the Δ as before. Measured: creates 43.7 k → 61.9 k/s, removals
38.2 k → 53.3 k/s; journal 225 → 182 B per create; the lane 21.8 → 15.1 µs per create (still 93 % of
a core).

## Lever 2 — the pass reserves first; the apply can leave the lane (decision 0003 §AW; `e9519fb`)

The pass drains, recalls, admits, RESERVES and stamps, then hands the window to an applier
(`Conveyor.applyWindow`: resolve, the union lock, the pre-images, the apply, the threshold, the
encode, the submit); the heap admission is the committer's before it enqueues; the lane completes
the reservation after the landing, after the apply (the tail law stands); the tail fast path is per
key; the §L hold rides a window on the device, ends at the next landing, its gate's line scales with
the batch (`c ≥ n/3`), and a batch drained with the appliers full waits for one to leave. The
applier VENUE is the A/B lever `SQUEEZEFS_META_APPLIERS` = N handler lanes (a window on the lane its
parent-side tree hashes to; a window alone in flight applies inline); **0 ships**: the inline
applier on the pass's thread.

What the venue's arms measured on the way (`-np 32 -u`, one session each unless noted):

| arm | creates/s | removals/s | lane µs/create | note |
|---|---|---|---|---|
| lever 1 alone (the inline arm) | 61.9 k | 53.3 k | 15.1 | the lane at 93 % |
| the committer as applier (its own worker lane) | 56–60 k | 53–63 k | 4.7 | the §L hold held 62 % of the passes 170 µs each; 32 concurrent appliers held their leaves 114 µs per 1.4-entry window and stalled the workers' FUSE queues |
| no hold at all | 57 k | 63 k | 10.5 | 230 k windows of 1.4 entries; `leaf_lock_hold` 114 µs |
| the 2 `sqz-meta` lanes | 63.7 k | **33.8 k** | 5.8 | the reclaim's destroy batches share the pool |
| all 32 handler lanes, round-robin | 63.1 k | 59.4 k | 6.4 | cold threads: the apply's CPU per create doubled |
| four pinned handler lanes (3 legs) | 69.0 k | 62.2 k | 6.3 | files 1.48× / 1.55× and directory removal 1.33× against dev — but **directory creation 0.93×** (41.8–42.4 k against 41.2–47.7 k) and a shared directory's creates 0.84× |
| + affinity by the parent-side tree | 63.9 k | 62.8 k | — | the shared directory's creates back to 0.97×, its mkdirs 0.72× |
| + a window alone in flight applies inline | 60.2 k | 57.9 k | — | the shared mkdirs 0.91×; `-u` directory creation 0.89× |

The lane stops being the ceiling under the lever (`sqz-jrnl` 2.0 s of a 7 s phase), but the serial
hot-leaf shapes — every rank's mkdir rewriting a parent whose record shares one leaf with 31 others,
a shared directory's creates — pay the handoff and the appliers' lock handoffs (an applier's
`leaf_lock_hold` 67 µs per 2.1 entries against the serial pass's 13 µs per entry), and the owner's
rule is that a regression is a defect: the lever ships off, its arm pinned by the `§AW` storm test
(`SQUEEZEFS_META_APPLIERS=2`: every ack present, the reopen replays every record).

## A/B against dev (the shipped default)

A = dev (`74e5402`, the main tree's dist build), B = this branch (`e9519fb`, the create path identical to the landed `c86f5bc`); `mdtest -n 10000 -i 1`,
each leg a fresh format on the RAM set, legs interleaved, every phase started at Tctl ≤ 70 °C.

| row | A (dev) | B | B ÷ A |
|---|---|---|---|
| `-np 32 -u` file creation /s | 36.5–49.8 k, mean 44.0 k (4 legs) | 61.3–71.8 k, mean 64.2 k (4) | **1.46** |
| `-np 32 -u` file removal /s | 32.8–41.8 k, mean 38.2 k (4) | 53.3–56.3 k, mean 54.9 k (4) | **1.44** |
| `-np 32 -u` directory creation /s | 43.3–44.5 k, mean 43.9 k (3) | 45.4–51.2 k, mean 49.0 k (3) | **1.12** |
| `-np 32 -u` directory removal /s | 37.2–40.7 k, mean 38.4 k (3) | 38.8–44.4 k, mean 42.2 k (3) | **1.10** |
| `-np 32` shared directory, file creation /s | 14.4–16.5 k, mean 15.2 k (3) | 15.1–15.7 k, mean 15.3 k (3) | 1.01 |
| `-np 32` shared directory, directory creation /s | 11.1–13.3 k, mean 12.0 k (3) | 10.5–12.2 k, mean 11.6 k (3) | 0.97 (inside A's spread) |
| file stat /s (`-np 32 -u`, unchanged path) | 10.9 M | 10.9 M | 1.00 |

Against the substrate's local filesystems (`rung-10-localfs-comparison.md`): file creation 64 k
against ext4's 711 k (9 %, was 5.8 %) and xfs's 471 k; file removal 55 k against 573 k / 183 k;
directory creation 49 k against 246 k / 369 k; directory removal 42 k against 456 k / 158 k. The
target of 200 k was not reached; the bars are not on par.

The data rows (`scripts/perf/quiet.sh ram-sqz cr-{A,B}`, 20 s each, A then B at the same dev head):
4k randread QD32×4 1.07 M → 1.24 M IOPS, 4k randwrite QD32×4 1.042 M → 1.040 M, seq write 1 MiB
QD16×4 52.5 → 52.9 GB/s — no regression.

## The remaining gap and what closes it

At 32 ranks the rate is 32 ÷ (the LOOKUP + the CREATE latency): 41 + 375–430 µs today. The engine's
serial stage is gone under the lever; what is left is the apply's intrinsic cost and the closed
loop's latency.

1. **The overlay's shape** (the largest term under every arm): `NodeDirty.overlay` is one sorted
   array re-cloned into the stable run every 8 applies and freed with the previous snapshot; a leaf
   that outruns its writeback holds thousands of records (mean 150–230 at apply after lever 1, max
   4,299). A persistent run — immutable chunks shared between snapshots, an insert copying one
   chunk — makes an apply O(log n) and the publish O(1); by the profile's shares the apply's CPU per
   create drops from ≈ 15 to ≈ 3 µs and an applier holds a hot leaf microseconds, which is what
   lets the lever ship ON for the directory rows too (the path to ≈ 250 k/s at 32 ranks).
2. **The handler's own ≈ 60 µs per create**: the `user.squeezefs.gather` xattr range per create,
   the stripe verdict's process-wide mutex, `allocateIno`'s lease-table scan and allocation per
   mint, the negative LOOKUP's 41 µs (28 on dev) — the FUSE side's share of ext4's 711 k.
3. **The ack fan-out** (one eventfd write per member on the lane): the applier parks for the landing
   and fans its window's acks out itself.
4. **Directories**: a mkdir's parent nlink shift stays an exclusive Put per op on the shared leaf
   (fsck's census forbids parking it); 1 is what moves that row.

## Wave 2 (2026-10-20; A = dev `0e3249b`, B = `perf/create-path-2`; scratch `~/tmp/createpath2/`)

**Lever 1, the overlay's shape (`e0fd515`).** `NodeDirty.overlay` is a persistent chunked run
(`node_snapshot.Run`): a spine of refcounted chunks of ≤ 32 records, immutable once published,
shared by reference between the dirty half and every snapshot; an apply derives the next run
(`RunEdit`) by copying the one chunk its record lands in and the spine, a publish is one reference.
The first cut kept `OwnedRec`s inside the chunks and measured the SAME lane CPU as dev: with §AV the
overlays at apply are small, so the O(n) re-merge of a small n was already cheap, and a 32-record
chunk copy cost what it cost — three refcount atomics on three scattered `SharedBuf`s per record
cloned, three more per record released (`applyLockedClass` self 3 → 18 %, `OwnedRec.deinit` 7 → 10 %
of the lane; file removal 0.96×, directory removal 0.92×). The shape that pays is the chunk owning
its records' BYTES in one arena (`ChunkRec`: 48 bytes of offsets, the head a range — a bset image in
RAM): a copy is two memcpys, a chunk's death two frees, zero reference traffic. Red test: 256
single-record applies on a leaf past 4,096 open records allocate within `Run.applyBytesBound(n,
rec_bytes)` and no block reaches `smp_allocator`'s 32 KiB page-mapping class (dev: 86 KiB per apply at
4 K). Measured alone (`-np 32 -u`, B/A, two legs each, interleaved): file creation 73.0–75.1 k
against 63.6–70.9 k, removal 63.8–67.5 k against 61.3–63.6 k, directory creation 51.3–55.5 k against
40.3–45.9 k, directory removal 38.9–41.0 k against 34.3–39.5 k; the lane's CPU per create 12.5 →
11.5 µs, `leaf_lock_hold` −14 % (files) / −22 % (directories); the head's decoded inode value is no
longer carried (one 64-byte decode per inode-delta apply).

**Lever 2, the applier venue re-judged (`9078cf1`).** With the arena run the inline arm's lock
handoffs no longer pay for an O(overlay) apply. `SQUEEZEFS_META_APPLIERS` at 0 / 2 / 4 on B
(`-np 32`, legs interleaved, k/s):

| row | 0 (inline) | 2 | 4 |
|---|---|---|---|
| `-u` file creation | 72.6–83.6 (3) | 75.5–78.8 | 76.1–81.2 |
| `-u` file removal | 64.8–66.4 | 76.2–79.8 | 83.1–90.5 |
| `-u` directory creation | 52.1–61.1 | 59.2–64.6 | 58.4–58.6 |
| `-u` directory removal | 38.8–45.0 | 51.8–56.3 | 49.4–51.6 |
| shared directory, file creation | 16.3–18.8 | 15.9–16.1 | 17.4–18.3 |
| shared directory, directory creation | 10.3–11.1 | 10.7 | 10.7–12.3 |

No row at 4 leaves its inline spread; 2 dips under it on the shared creates. The derived width
`clamp(cpus / 8, 2, 4)` (= 4 here; `conveyor.applierParallelism`, decision 0003 §AW.3) SHIPS; 0
stays the control arm. With it the create's `meta_tx_queue_wait` 55 → 26 µs and `meta_window_total`
94 → 26 µs; the lane's CPU 3.8 → 2.2 s per 320 k creates, the four `sqz-fuse-ln` lanes 3.1 s.

**Lever 3, the handler (`0c6147c`).** The thin-LTO profile of the 32 workers (the dist profile folds
identical functions, so its symbols lie): `createChild` is 44 % of their CPU — `Conveyor.build` 12.6 %
(the committer's §AW.2 heap admission: a resolve, the leaves' write locks and `projectedLogEnd` per
tx — ≈ 12 µs), the dentry chain scan 11.6 % (`findChild` → `rangeCounted`/`nextLive`, once in the
LOOKUP and once in the CREATE), the stripe verdict 8.4 % (its process-wide mutex 3.2 %), `readInodeOpt`
5.1 %, `allocateIno` 4.7 % (the mint's census: `usedLeafBytes` over the trees, `heldBy`'s lock + copy +
sort per create), the gather xattr range 4.4 %, `insertParentOf` 4.1 %. Taken: the verdict cache's
mutex is a reader-writer lock — the probe (two per create, on every worker) holds it shared. It was
the negative LOOKUP's cost: the drain worker's try-only probe found the mutex held on a third to a
half of the lookups (`dir_stripe_verdict_try_refusals` 0.39–0.53 per op) and was DEMOTED to a handler
fiber (a cross-lane handoff) — demotions 159–169 k per 320 k creates on A, 10–16 on B; the LOOKUP
86–93 → 7.2–7.7 µs (A/B, same hour). Measured and not taken, with the reasons: the gather xattr (a
range read of the parent's xattr chain per create, needed by the mint's gather arm — serving it from
the inode record is an on-disk flag, a format change; a negative cache would have to be invalidated
on the holder's shipped-record path too), the mint's census (≈ 3.5 µs; a per-lane ino window changes
the rotor's balancing grain and must follow the lease generation — wave 3), the committer's heap
admission (≈ 12 µs; it is §AW.2's law — a refusal after the reservation would be a hole — and the
leaves' dirty state needs their locks; the tx's own resolved leaves could seed the resolve).

**Lever 4, the ack fan-out** — not taken: with the venue on, the lane runs 2.2 s per 320 k creates
(6.6 µs per create, half of it the acks' wakes) at ≈ 50 % of a core; it is no longer on the create's
critical path, and the fan-out after the landing is the same chain wherever it runs.

**A/B against dev** (`-np 32 -n 10000 -i 1`, each leg a fresh format on the RAM set, legs
interleaved A B B A inside one hour, every phase started at Tctl ≤ 70 °C with no build or test of
another tree running; this afternoon's host ran every leg of both arms ≈ 25 % below the midday
level, so the ratios are the measurement):

| row | A (dev) | B | B ÷ A |
|---|---|---|---|
| `-u` file creation /s | 48.6–49.6 k | 56.9–60.1 k | **1.19** |
| `-u` file removal /s | 46.4–50.8 k | 58.3–59.4 k | **1.21** |
| `-u` directory creation /s | 31.9–36.5 k | 42.5–49.3 k | **1.34** |
| `-u` directory removal /s | 23.6–26.6 k | 34.1–37.9 k | **1.43** |
| shared directory, file creation /s | 15.5–19.4 k | 16.3–17.3 k | 0.96 (inside A's spread) |
| shared directory, directory creation /s | 10.4–12.3 k | 12.6–13.7 k | 1.16 |
| the classic run: creation / removal / dir creation / dir removal | 45.0 / 41.6 / 35.9 / 29.7 k | 57.0 / 64.0 / 48.9 / 40.1 k | 1.27 / 1.54 / 1.36 / 1.35 |
| file stat /s (the classic run) | 9.1 M | 7.8 M | the kernel's dcache serves it (4.6–7.4 k getattrs per 320 k stats reach the daemon); within the day's swing |

The midday legs, at the host's faster level, put B at file creation 73–84 k, removal 83–91 k,
directory creation 58–65 k, removal 50–56 k. Against ext4 / xfs on the substrate: file creation 58–84 k
against 711 k / 471 k (8–12 %), removal 59–91 k against 573 k / 183 k, directory creation 46–65 k
against 246 k / 369 k, directory removal 36–56 k against 456 k / 158 k. The bars are not on par: at 32
ranks the rate is 32 ÷ (the LOOKUP + the CREATE), and the CREATE is 300–470 µs of which ≈ 42 µs is
the handler's CPU and ≈ 100 µs the engine's (queue, window, the landing, the ack) — the rest is the
closed loop's wakes: the kernel → worker dispatch, the committer's park and its ack wake, the
request's demotions. ext4 acknowledges a create from RAM; the charter's ack is after the landing.

The data rows (`scripts/perf/quiet.sh ram-sqz cr2-{A,B}-r1`, 20 s each, A then B): 4k randread
QD32×4 959 k → 1.06 M IOPS, 4k randwrite QD32×4 754 k → 875 k, seq write 1 MiB QD16×4 33.9 → 39.3
GB/s — no regression (both arms below the wave-1 level; the host's afternoon).

## Suites

Wave 1: unit families (`conveyor`, `meta.backend`, `kv_fs`, `kv_fs_stripes`, `economy`, `meta.tx`,
`journal_lane`, `lifecycle`, `e2e`, `fsck`, `node_cache`, `exec`, `rails`, `knobs`, `stats`):
610/646 passed, 36 skipped (`-Dfresh=true`). Smoke rows (THE KILL MATRIX and its two-ring row, A STRIPED DIRECTORY ON ONE DAEMON, THE
SHARED HOT DIRECTORY ACROSS DAEMONS, A DEAD MANAGER HOLDING A STRIPED HOT DIRECTORY, kill -9 mid-tar
on both cadences, KILL -9 BETWEEN A PROMOTION'S COMMIT AND ITS RING RELEASE): 46/46 (`-Dfresh=true`). `fsck
--offline` clean after every session (≈ 40 of them). No full gate (the train gates).

Wave 2: the same families plus `node_snapshot`, `smo`, `leaf_merge`, `checkpoint_cycle`, `meta.tree`
(`-Dfresh=true`, with the applier venue on by default): 733/800 passed, 67 skipped; `rails` 29/29;
the same smoke rows 46/46 (`-Dfresh=true`); `fsck --offline` clean after every one of the 41
sessions. No full gate (the train gates).
