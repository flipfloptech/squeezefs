# Rung 10 — a write that parks its stamp holds `I{ino}` shared (rangelock; slabcopy's next limit)

**What the section held.** After its data has landed, every striped write runs a postlude under its
file's 4a `I{ino}` guard, and that guard was taken exclusive. The section holds: `lockPlan` (a heap
`HeldGuards`), a transaction begun and then abandoned, the inode read and folded, the claim, the
stamp park (one clock read and a spin-locked map), and the guard's two hold-timing clock reads.

A (kamrui, RAM-backed NVMe, `.stats` deltas per 30 s row):

| row | exclusive holds | mean held | writes that waited | mean wait |
|---|---|---|---|---|
| 1M seq write ×4 | 98,130 | 69 µs | 51 % | 10.8 ms |
| 4k randwrite QD32×4 | 2.24 M | 3.3 µs | 11.8 % | 792 µs |

Level 1 classed every one of these writes Shared (`write_lock_scope_shared` = writes). The holders:

- the write postludes, ≥ 98 % of the holds;
- the times drain, about 2,300 holds per row, 0.2–0.7 ms each across its commit;
- setattr, truncate, and fsync's drain.

Reads take no hold. A wait is far longer than a hold because a release wakes one waiter and the lock
stays free until that waiter runs: a fresh taker yields to a queued one (charter §9).

**Design (decision 0003 §AT).** A §L.6 park (the coalesced cadence, no KILLPRIV) now takes
`I{ino}` SHARED, with no transaction. Two parks commute: `parkClaim` takes a max per field under its
stripe lock. Every guarded RMW of the parked entry or of the record still takes the guard
exclusive: the drain, setattr and truncate, absorb, and the strict-cadence and KILLPRIV commits.

A range lock was rejected. This guard never ordered data, because the postlude runs after the
bytes landed. The plane's block locks (3) order overlapping writes, and level 1 keeps an extending
write exclusive against truncate. Shrinking the section alone was rejected too: at 3.3 µs held, the
waits were 790 µs. No new lock-free core is involved (the existing `sync.RwLock`, shared mode), so
there is no model.

**Red → green** (`kv_fs` "rangelock: …", seam `SQUEEZEFS_TEST_WRITE_POSTLUDE_HOLD` parks a write
inside the section). Red (`d3de376`): a size-neutral write to a disjoint range of the same file parks
on the guard. Pinned on both sides:

- a truncate and the times drain wait behind the held write;
- an extending write beside it keeps its claim (getattr shows 8,704 B).

Green: `kv_fs`, `economy`, `dlm` and `op_guard` tests, 123/123 on kamrui.

**A/B on kamrui RAM-backed NVMe** (`baseline.sh ram-sqz`, dist builds, every venue lock held, legs
A B B A; A = `e36aa06`, B = this branch; values listed as A1, A2 → B1, B2; change = mean B ÷ mean A).

| row | A | B | change |
|---|---|---|---|
| 4k randwrite QD1 (IOPS) · p50 | 9,437, 9,217 · 100 µs | 9,621, 9,420 · 97 µs | +2 % |
| 4k randwrite QD32×1 | 33,703, 32,673 | 36,113, 36,040 | **+8.7 %** |
| 4k randwrite QD32×4 | 74,434, 74,484 | 80,499, 78,338 | **+6.7 %** |
| 64k randwrite QD16×4 (MiB/s) | 3,354, 3,256 | 3,622, 3,369 | +5.8 % |
| 1M seq write QD16×1 | 2,023, 1,923 | 1,941, 1,925 | −2 % (overlap) |
| 1M seq write QD16×4 | 3,211, 3,145 | 3,318, 3,197 | +2.5 % (overlap) |
| 4M seq write QD4×4 | 2,968, 2,532 | 3,171, 2,599 | +5 % (overlap) |
| 4k randread QD1 / QD32×1 / QD32×4 | 19,824 / 81,550 / 251,436 | 19,392 / 81,619 / 254,745 | −2 % / 0 / +1 % |
| 64k randread · 1M seq read ×1 / ×4 (MiB/s) | 6,294 · 6,807 / 7,820 | 6,327 · 6,812 / 7,791 | 0 |

Read rows show means. No read-path code changed. The −2 % on randread QD1 does not overlap
(A 19,730 and 19,918, B 19,541 and 19,242). It is inside the ±2 % that the slabcopy A/B saw on rows
its change did not touch, and is likely dist-build layout.

**Lock-wait share** (writes that waited on `I{ino}`, A → B):

| row | A | B |
|---|---|---|
| 1M seq write ×4 | 51.1 % and 50.5 % (532 s and 525 s waited per row) | 1.3 % and 1.4 % (4.9 s and 5.9 s) |
| 4k randwrite QD32×4 | 11.8 % and 11.3 % | 0.46 % and 0.47 % |
| 64k randwrite | 12 % | 0.7 % |
| 4M seq write ×4 | 25 % | 1.2 % |

The waits that remain queue behind the drain's exclusive batches (about 77 per second).

**Reading.** The serialization is gone, and the small-block write rows gain 6–9 %. The big-block
rows barely move. On 1M seq write ×4 the daemon's write latency stays 13.8 → 14.7 ms mean, so the
queue moved from the guard into the data plane's write. Device service per write rose 1.07 →
1.29 ms, and the host is 26 % idle (`perf -a`, B). Next: an op-phase breakdown
(`SQUEEZEFS_OP_PROFILE`) of where the write's remaining ≈ 13 ms go. `read_hpet` / `clock_gettime`
is still 8.6 % of the daemon's samples. Every session ended with a clean `fsck --offline`.

**Gate** (kamrui, `-Dfresh=true`, this branch): 55/55 steps succeeded; 2997/3080 tests passed (83 skipped).
