# Rung 10 — the FUSE boundary on 4 KiB random reads (what the round trip costs, what the daemon costs, patch 0031)

The owner's bar (2026-10-19): on an in-memory substrate SqueezeFS is on par with ext4/xfs on every row.
`rung-10-localfs-comparison.md` read the 4k randread QD32×4 row at 58 % (1.19 M vs 2.03 M IOPS, the
daemon 8.0 cores) and named the FUSE round trip. Venue: the workstation (`local`, `7.2.8-9.30-omarchy-sqz`,
series 0001–0030), the RAM NVMe set of `scripts/perf/ramnvme.sh`, the venue held per session, every row
started at Tctl ≤ 70 °C, 20 s rows (profiles 30 s); fio io_uring O_DIRECT 4k randread QD32 × 4 jobs. Raw
rows in `/build/perf/mx/results.tsv` (`ram-*-rfb-*`), the cs/IPI samples under `…/cs.txt`.

**The ruler.** `SQUEEZEFS_TEST_READ_ZERO_FILL` (test seam): every O_DIRECT ring READ is answered at its
delivery CQE on the worker with no filesystem — `prefilled` commits a header claiming the length and moves
no bytes (the FUSE-over-io_uring round trip alone), `bounce` zero-fills the bounce window and bridges it
(the round trip plus one memory-source `READ_FIXED`). The e2e row pins both arms and `off`.

| 4k randread QD32×4 | IOPS | host cores | daemon cores | host µs/read | cs/read | CAL IPI/read | cycles/read |
|---|---|---|---|---|---|---|---|
| ext4 (same namespace) | 1.99–2.04 M, fio 99 % | 8.5 | — | 4.15 | 0.69 | 0.105 | 19.7 k |
| SqueezeFS `prefilled` (the round trip) | 2.05–2.95 M, fio 100 % | 9.1–9.5 | 5.4–5.6 | **3.0** | 0.54 | 0.095 | **13.6 k** |
| SqueezeFS `bounce` (+ one memfd bridge) | 2.09 M, fio 98 % | 13.9 | 10.3 (ur 7.7 + **io-wq 2.6**) | 6.7 | 1.20 | 0.081 | 31.5 k |
| SqueezeFS off (the product) | 1.25 M, fio 60 % | 13.2 | 8.0 (all `sqz-fuse-ur`) | 10.5 | 1.26 | 0.099 | 47.4 k |

The round trip is not the barrier: a FUSE-over-io_uring request with nothing behind it costs 3.0 µs of host
CPU — less than ext4's whole read (4.15: the nvmet-loop + brd substrate is 2.3 µs of that on either side,
`kworker/u129`: the target's lock 0.3–0.6, brd's memcpy 0.3) — and the row is fio-bound at 2–3 M. The daemon
pays 1.9–2.3 µs per round trip (≈ 0.3 userspace; the kernel: spinlocks 0.34, the header copies and
`fuse_uring_args_to_ring` ≈ 0.25, request end + the completion to fio's ring ≈ 0.30, zc register/unregister
+ its 2 allocs ≈ 0.18, io_uring plumbing and the park/wake ≈ 0.45). The bounce arm's io-wq: a `READ_FIXED`
from the memfd slab is punted to io-wq (2.6 cores at 2.1 M) — the fd-source warm serve
(`SQUEEZEFS_READ_ZC_SERVE`) rides the same path; a lever of its own, not pulled here.

**Where the product's 6.4 µs per read goes** (flat `perf` self time on the workers, 1.25 M reads/s):

| | µs/read | what |
|---|---|---|
| the round trip | ≈ 2.3 | as above |
| the metadata lookups | **≈ 2.4** | three KV lookups per read (`metaFileAt`'s inode + layout, the recheck's binding): `entryKeyLess/Eq` 0.60, `NodeCache.tryGetMapped` 0.28, `MetaSet.volumeOfSlot` 0.26 (the route table's arc-swap: two contended RMWs per ask), `layoutPointLocal` 0.24, `Divert.serve` 0.18, `partitionPoint` 0.12, `bindsBlock` 0.10, `descendCounted` 0.08, `inodeSizeLocal` 0.08, `groupBounds` 0.07, `NodeSnapshot.lookup` 0.07, `resolvePoint` 0.07, srcu/rwlock 0.13 |
| the device leg | ≈ 1.7 | the `READ_FIXED` issue (`blk_mq_submit_bio`, `nvme_ns_head_submit_bio`, `nvmet_req_init`, `queue_work_on`, `io_import_reg_buf`, `__io_prep_rw` ≈ 0.35 — ext4 pays the same in fio's context), its completion's task work into the worker's ring and the wake, the bridge bookkeeping (`pushBridge` 0.08, `handleBridgeCqe` 0.12, `clearPend`), the second pass |

So two workers per queue at 6.4 µs cap one submitter at ≈ 310 k reads/s and the row at 1.25 M.

**Counted, not sampled** (bpftrace, 5 s of the product's row at 1.2 M reads/s; `/build/perf/mx/ram-sqz-rfb-trace-2/trace.txt`):
per read 1.0 background request, 1.0 delivery task work (`fuse_uring_send_in_task`), 1.0 `COMMIT_AND_FETCH`,
1.0 `fuse_uring_req_end`; the FUSE `queue->lock` is taken 6 times (5 by the worker — `send_in_task`, `send`,
`commit_fetch`, `req_end`, `get_next` — and once by the submitter) and enters the slowpath 0.5 times (the
worker's five sections 40 % of its slowpath entries, the submitter's `queue_bq_req` 95 % of fio's); 59 % of
the worker's slowpath entries are `__queue_work` — the nvmet-loop target's pool lock under the daemon's
`READ_FIXED`, the substrate's, which ext4 pays in fio's context (0.43 µs/read there); `fi->lock`
(`fuse_aio_complete`) 0.5 %. The workers issue 1.07 wakeups per read (fio's ring at the request end, the
nvmet kworker at the submit) and receive 0.67 (the kworker's completion into their rings); they sleep
0.01 times per read — saturated, as the cores say.

**Daemon A/B — rings per queue** (`SQUEEZEFS_FUSE_RINGS_PER_QUEUE` now admits 3 and 4; the derivation stays
2; legs A B B A B A A B, A = 2, B = 4, both this branch `17f36f4`; mean (min–max), daemon cores, µs/op):

| row | A (2 rings) | B (4 rings) | B ÷ A | daemon cores A → B | daemon µs/op A → B |
|---|---|---|---|---|---|
| rr4k-q32j4 | 1,251,350 (1,244,820–1,256,185) | 1,582,067 (1,539,798–1,623,668) | **1.26** | 7.98 → 14.48 | 6.38 → 9.15 |
| rr4k-q32j1 | 562,522 (556,376–569,916) | 566,188 (458,556–836,993) | 1.01 | 2.01 → 3.70 | 3.57 → 6.53 |
| rr4k-q1j1 | 102,620 | 99,847 | 0.97 | 0.74 → 0.75 | — |
| rr64k-q16j4 | 771,039 | 809,595 | 1.05 | 6.87 → 10.59 | 8.91 → 13.07 |
| rw4k-q32j4 | 431,033 (423,907–437,052) | 321,071 (309,982–351,277) | **0.74** | 9.34 → 11.53 | 21.7 → 35.9 |

More workers buy 26 % on the row at 1.8× the cores (a worker holding a quarter of a queue amortizes its
pass over fewer requests: 9.2 µs per read) and cost the 4k write row 26 % — the half-loaded worker of
`rung-10-workers-per-queue.md`, worse. 4 stays the A/B arm; the lever is the per-read cost, in this order:
(1) the metadata lookups — a per-inode cached (size, first-block binding) view for the hot read path would
take ≈ 2.4 µs to one load, but every publish must bump it (full tier, with a model; the read agent's "per-ino
layout generation"); the cheap half is `Published.volumeOf`'s arc-swap pair (0.26) read through a seqlock'd
plain copy, and the divert ask short-circuited on a solo mount; (2) fewer, fatter passes (the 4-ring and
the write-row numbers are the same pass tax); (3) the kernel's share (0031).

**Patch 0031** (`scripts/kernel/proposed/0031-sqz-fuse-uring-cheaper-request-round-trip.patch`; compile-checked
W=1 as an out-of-tree fuse module against the 7.2.8 tree + 0001–0030, unbooted — the owner builds and
boots): one 288-byte header copy per direction (three `copy_to_iter` passes per send and two
`copy_from_iter` per commit become one each), per-entry zero-copy scratch (the folio carrier and bvec
array reused across requests; the carrier claimed FREE→BUSY with acquire at registration, handed back by
the imu release, orphaned by an entry freed busy — exactly one of the two frees it), one `queue->lock`
section for the plain commit's end-and-fetch (four trips per request → three, against the submitting CPU on
the same lock), and `fuse_aio_complete` bumping `fc->attr_version` under `fi->lock` only for writes (the
synchronous path's rule; a global atomic64 RMW per completed read today — the lock itself is 0.5 % of the
slowpath). Expected from the profile: 0.25–0.35 µs of the worker's ≈ 2.0 µs kernel share per read (12–17 %
of the round trip's kernel cost, 4–5 % of today's 6.4 µs); the remaining `queue->lock` sections hold another
0.1–0.2 µs for a follow-up (one section per commit needs the header read under the lock or an entry lookup
by `buf_index` instead of the pq hash). Probe plan: the `prefilled` row before and after the boot (daemon cores per read at equal IOPS; the
flat profile's `_copy_to_iter` / `iov_iter_advance` / `__kmalloc_noprof` / `kfree` / `_raw_spin_lock` lines on
`sqz-fuse-ur`), then the full row, THE KILL MATRIX, the zc e2e rows and the dismount pass for the carrier's
orphan arm. The structural kernel lever — a READ reply naming a device range the kernel reads into the
request's folios itself (no zc slot, no `READ_FIXED` on the daemon's ring, no completion task work into the
daemon, one pass per read; ≈ 1–1.5 µs) — waits on the data plane's post-DMA binding recheck becoming a
pre-check with an in-flight pin (charter §6.3, full tier).

**Red → green.** The e2e row "read boundary ruler" (`bounce` zeros of the asked length, one bridge, the
probe never serves; `prefilled` the length with no bridge and the app's pages untouched; a buffered READ
keeps the law; `off` the text); `geometry` "rings per queue" (the ceiling 4, the total floor/ceil split).
