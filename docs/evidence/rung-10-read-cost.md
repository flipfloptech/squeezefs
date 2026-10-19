# Rung 10 — the read side's per-request cost (what a 4k read pays on the uring worker)

Item (5) of `rung-10-perf-baseline-workstation.md`: 4k random reads run entirely on the FUSE worker
(`sqz-fuse-ur<qid>`) at ≈ 4.8 µs of daemon CPU each; the bar is 942 k IOPS. Venue: the Linux
workstation (`local`), kernel `7.2.8-7-omarchy-sqz`, RAM-backed nvmet-loop (`quiet.sh ram-sqz`, the
whole venue held per session, 4 × 3 GiB, 20 s rows). Profile session `ram-sqz-rrd-prof-r1`
(`/build/perf/mx/`, dev `732a3da`, `perf record -a -g` 10 s mid-row, `incl.py` over the workers).

**The split** (QD32×4, 733 k IOPS in the profiled session, 4.0 worker cores ⇒ 5.46 µs per read;
QD32×1 has the same shape at 2.9 µs). Every read is cold: `direct_leg_offers ≡ ring_reads`, one
`READ_FIXED(device → slot)` of exactly 4,096 B (`dev/user` 1.00), a header-only commit,
`fd_body_fallbacks` 0, 0.2 `io_uring_enter` per read (3 at QD1: the delivery wait, the DMA, the commit).

| where | share | µs | what |
|---|---|---|---|
| kernel inside `io_uring_enter` | 53 % | 2.9 | `io_uring_cmd` (the commit+fetch) 17 %, task work (deliveries, bridge CQEs, zc registration) 18 %, the `READ_FIXED` issue 8 % (≈ 3 % of it the nvmet-loop target's workqueue lock — the substrate's), syscall/cq-wait 10 % |
| the warm ladder's probe | 30 % | 1.63 | `metaFileAt` 16 % (the inode record 9 %, the layout record 7 %: two KV lookups, each asking the foreign-slot divert first); `wsHasRetainedExtents` 8.2 % (`accumulate.Table.blocksOf` 8.1 % SELF — a scan of the whole index, 1,024 slots at this set's parked cap, under the table's spin lock, on an EMPTY table); the tier probes 2.8 % (`Table.has` 2.6 % self: the same lock through `hasCustody`); guards + counters 2.6 % |
| the direct leg + the post-DMA recheck | 10 % | 0.53 | the leg's six tier/custody probes 1.9 %; the recheck's binding re-read 7.5 % (a third divert ask + KV lookup) |
| the transport's own userspace | 6 % | 0.33 | reap, parse, `pushBridge`, the commit |

The three KV lookups cost 10 % together (`NodeSnapshot.lookup` → `groupBounds` → `partitionPoint`:
the leaf index's key compares 6.4 % self; the node pins are cheap, `descend` 1.8 %); the route
table's arc-swap behind every divert ask and `treeOfKey` (six per read) 3.3 % self; `Divert.serve` 3.8 %.

**What changed** (two commits, the data plane's read side).
1. `accumulate.Table` keeps per-ino presence stripes (`presenceStripes`: the power of two ≥ 4 × the
   entries; one set for every entry, one for the extent entries). A stripe is raised under the lock
   BEFORE its entry joins the index (`acq_rel`) and lowered AFTER it leaves (`release`), so a load
   of 0 is an exact "no entry of this ino" without the lock and a non-zero stripe is a maybe the
   locked lookup settles. `hasExtentOf` (the ladder's pre-screen), `has` (the custody probe),
   `holdsIno` (the FLUSH probe) and `lowestBlockOf` read it first; `parked_ino_scans` counts the
   locked fallbacks (0 per read on every B leg). Precedence (§6.3) is unchanged: a write's entry is
   indexed, its stripe raised, before its ACK, so a read after the ACK takes the exact path.
2. `metaFileAt` asks the foreign-slot divert ONCE for the size and the layout record (`.local` ⇒ two
   store lookups; `.foreign` ⇒ both off the one token page); `dlm_token_local_reads` per read 3 → 2.

**Red → green.** The red is the profile: 8 % of a saturated worker in `blocksOf` over a table holding
nothing. `accumulate` "derivations: the presence stripes = …" (the tie test) and "the per-ino presence
stripes: a read's custody probes … never take the lock or scan the index …" pin the mechanism (an
absent ino never scans; a full entry is found and scans only the ino-wide probes; a stripe twin scans
and answers no; an extent scans `hasExtentOf`, a full buffer does not; the escalation clears the
extent stripe, the retire both). Kept green: `accumulate`, `warm_ladder`, `write_side`, `read_path`,
`data.plane`, `write_admit`, `kv_fs`, `transport`, `e2e` (327/329, 2 skipped); the smoke rows KERNEL
AIO on the plain mount, the data plane WITH / WITHOUT a cache path, THE KILL MATRIX (43/43).

**A/B** (A = dev `2edfd47` from the main tree, B = `e28373f` = `732a3da` + the two commits; legs
A B B A B A A B, each a venue-held session, `rr4k|rr64k`; mean, (max − min) ÷ mean, worker µs per op):

| row | A (4 legs) | B (4 legs) | B ÷ A | µs per op A → B |
|---|---|---|---|---|
| 4k randread QD1 | 105.4 k (20 %) | 110.6 k (22 %) | 1.05 | 5.76 → 5.31 |
| 4k randread QD32×1 | 319.5 k (32 %) · median 325 k | 374.7 k (34 %) · median 399 k | 1.17 | 3.18 → 2.72 |
| 4k randread QD32×4 | 827.9 k (13 %) · median 847 k | 897.4 k (15 %) · median 900 k | 1.08 | 4.94 → 4.53 |
| 64k randread QD16×4 | 524.3 k (4 %) | 570.2 k (2 %) | **1.087** | 7.12 → 6.55 |

The 4k rows swing 20–34 % between sessions of ONE binary here (which worker the kernel picks for
fio's CPU and where it sits relative to fio — an SMT sibling, the same CCD, the other); the 64k row
is the low-noise witness of the same per-request saving (−8 % of the worker's CPU per read — the
profile's 8 % + 2.6 % + 1.5 %). Best B legs 950–962 k at QD32×4 (the bar: 942 k); the mean sits at
95 % of it, from 91 %.

**The full matrix** (A B B A, B rebased: `3101a97` = `2edfd47` + the two). The read rows that ran:
4k QD1 85.9 k → 115.5 / 92.4 k, QD32×1 353 k → 405 / 285 k, QD32×4 860 k → 957 / 842 k, 64k 527 k →
570 / 572 k (1.08), seq read 1M ×1 42.7 → 42.3 GB/s, ×4 68.5 → 68.0 (0.99). **Every leg died in the
`sw1m-q16j1` row, A (dev `2edfd47`) and B alike**: 16 WRITEs of one file parked past `write_admitted`
/ `write_locked` for 30 s, the supervisor's SIGABRT, fio ECONNABORTED (the fourth leg met the dead
mount). `732a3da`, before the fsync-route landing `654fd83`, runs the row at 24.9 k: dev's regression,
reported to its owner, not this change's. The write rows were judged on that base (A = `732a3da`,
B = `e28373f`, A B B A, `rw|sw`): 4k randwrite QD1 87.8 k → 95.6 k (1.09), QD32×1 219.5 k → 235.0 k
(1.07), QD32×4 424.4 k → 430.0 k (1.01), 64k randwrite 292.0 k → 290.3 k (0.99), seq write 1M ×1
24.8 → 25.1 GB/s (1.01), ×4 46.4 → 46.2 (0.995), 4M ×4 38.7 → 38.8 (1.00; both bimodal 33/44) — no
write row regressed, the daemon's cores per row equal within 0.2.

**What remains on the read, in order.** (1) The kernel's 2.9 µs (the commit+fetch cmd, the delivery
task work, the zc slot's un/re-registration) — the series' side. (2) The recheck's third lookup
(7.5 %): a per-leaf witness (the probe's leaf snapshot `Arc` compared at the CQE — every apply
publishes a fresh one) or a per-ino layout generation would make it one load, but every publish path
(the plane's merges and truncates, the kvmap windows, the shipped foreign publish, the token recall)
must bump it: a full-tier change with a model. (3) The leaf index's key compares (6.4 % self) and
the route table's arc-swap per ask (3.3 %) — the metadata engine's. (4) ≈ 15 sharded counter
increments per read, each a line 8 KiB from the next, ≈ 3 %.
