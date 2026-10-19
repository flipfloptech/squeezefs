# Rung 10 — lever 5: N workers per kernel FUSE queue (what the kernel admits, what binds)

Lever 5 of `rung-10-perf-baseline-workstation.md` (decision 0003 §AB.6): one io_uring submitter's
whole depth lands on ONE FUSE queue, so the all-own write arm (`SQUEEZEFS_FUSE_WRITE_OWN_DEPTH=0`,
`rung-10-write-lane.md`) made that queue's worker its one saturated CPU and ships at depth 8. This
note settles what the kernel allows, pins the saturation on this box, drafts the lever's kernel
half, and — once that half was booted (`7.2.8-9.30-omarchy-sqz`, series 0001–0030) — lands the
daemon half and measures it (the last two sections). Venue: the Linux workstation (`local`), the
whole venue held per session (`scripts/perf/quiet.sh`), sessions `ram-sqz-rl5-*` (`/build/perf/mx/`).

**The kernel contract (7.2.8 + series 0001–0029, read, not run).** `fuse_uring_task_to_queue`
picks `qid = task_cpu(current)` with no fallback; `fuse_uring_queue_fuse_req` hands the request to
the first entry of `queue->ent_avail_queue` (LIFO: `fuse_uring_ent_avail` moves a returned entry
to the head), whatever io_uring registered it; `fuse_uring_register` on an existing queue checks
only that the buffer flags repeat (`dev_uring.c` ≈ 1826–1836), and a COMMIT_AND_FETCH finds the
entry by `commit_id` and sets `ent->cmd = cmd` (≈ 1542) — nothing names the ring. So on
**UserEnts** several rings per queue work today, as §AB.6 recorded. On **BufRing / ZeroCopy** —
the product's posture on this kernel — they do not: the queue pins ONE kmbuf payload ring at
creation, from the first REGISTER's ctx (`queue->bufring`, `io_uring_buf_ring_pin`, ≈ 425), every
entry selects its payload from it (`fuse_uring_select_buffer`, ≈ 1175), and the recycle
(`io_uring_kmbuf_recycle`, `io_uring/kbuf.c` 110–126) resolves the buffer group in the ENTRY's cmd
ctx — a second ring's entry would select from ring A's list and recycle into ring B's (ring A's
list drains; B's `io_buffer_get_list` with no group is a NULL dereference). The zc headers index
also carries the queue's `zero_copy_depth` (≈ 1461), one ring's table layout. §AB.6's "the code
admits N rings" is therefore UserEnts-only; the row is corrected. Not tested live: the failure
mode is a kernel stall or oops on the owner's workstation.

**The saturation, pinned** (`MX_PROFILE=1`, 10 s of `perf record -a -g` mid-row, 20 s rows). Per
250 ms window the ONE busy worker is the queue of the CPU fio runs on — `sqz-fuse-ur14` at 0.93
cores while fio sits on CPU 14, `ur12` when it moves to 12, `ur15` on 15 (the old depth-0 profile
`ram-sqz-rl4-B-prof-r1` reads the same: 1.00–1.30 `ur` cores per window, the top worker 0.7–1.0,
fio's CPU) — so the per-row "busiest thread 13–40 %" of `results.tsv` is fio's migration over the
16 s window, not a spread. No `iou-wrk` samples: fio's O_DIRECT write is issued on its own thread
(FUSE_ASYNC_DIO, `-EIOCBQUEUED`), one CPU, one queue. The saturated worker's time (write row, depth
8, 181 k IOPS under the profiler): 51 % inside `io_uring_enter` — `io_uring_cmd` 17.5 % (the
commits and re-registrations), the device `WRITE_FIXED` issued inline 8 %, the deliveries' task
work (`__io_run_local_work`, the zc registration) 14 %, `io_cqring_wait` 5.5 % — the first eight
handlers own 25 %, `reap` 14 %, the `ln` lane wakes 7 %. The read row (255 k): the inline warm
ladder 27 % (`metaFileAt`/`layoutPoint`/`NodeSnapshot.lookup` 11–13 %, `accumulate.Table.blocksOf`
9 % self), `io_uring_cmd` 19 %, the direct leg 9.5 %, task work 19 %, cq-wait 19 %. Everything the
queue binds to its ring — delivery, the slot's DMA, the commit — is ≥ half of the thread.

**The in-process alternative, judged, not built.** "A worker hands its surplus to sibling
workers" moves only the handler's CPU: the zc slot is a fixed buffer of the delivering ring, so the
`WRITE_FIXED(slot → device)`, its CQE and the commit stay on the saturated thread whoever runs the
handler — which is what the depth-8 spill already does onto the CPU's `ln` lane, through the same
`Lane.enqueue → inbox + coalesced eventfd` wake a sibling's DRIVEN lane would take (`exec/lane.zig`
`enqueueReporting`). Same wakes, same ring-bound floor (≈ 4.4 µs per write → ≈ 225 k); the
A/B would measure noise, so no legs were spent. All-own at depth 0 beats depth 8 on one thread only
if the write handler's own-lane cost (≈ 3 µs: two `readInodeOpt`, the layout kind, `mapLookup`,
`beginPatch`, the token/route RMWs) falls under the ≈ 1.4 µs the offload's wakes cost — the
write-lane note's "what remains", not this lever.

**The kernel half, drafted.** `scripts/kernel/proposed/0030-sqz-fuse-uring-several-rings-per-queue.patch`
(moved into `patches-7.2/` 2026-10-19 on the owner's word, built as `linux-omarchy-sqz 7.2.8-7.30`, booted as `7.2.8-9.30-omarchy-sqz` the same day; the gate green on it): the
buffer ring becomes a property of the registering ring — a queue keeps `struct fuse_ring_src`
per ring ctx (its pinned kmbuf ring, its zc depth), each entry points at its ring's source,
selection and the headers index go through it, a commit from another ring is refused `-EXDEV`,
and a FEATURES command (cmd_op 4) answers `FUSE_URING_FEAT_RINGS_PER_QUEUE` inline (an unpatched
kernel answers `-EINVAL`: the probe). One ring still serves one queue (`-EALREADY` on a second
pin). Compile-checked `W=1` as an out-of-tree `fuse.ko` against the built 7.2.8 tree; applies at
`--fuzz=0` after 0029. Fuse is a module here, but swapping it means unmounting the desktop's
gvfs/portal FUSE mounts, and a kernel install is a reboot — the owner's.

**The daemon half, landed** (on the booted 0030 kernel). The FEATURES probe runs once per mount
right after the retention probe (`probes.driver.probeFeatures`: the CQE result is the mask; on
this kernel `FEATURES → 1`, an unpatched kernel's `-EINVAL` reads absent, fail-safe), published as
`transport_probe_rings_per_queue`. `geometry.ringsPerQueue(mode, feature)` derives 2 on a
kmbuf/zc session where it reads present, 1 on UserEnts and wherever absent (tie test);
`SQUEEZEFS_FUSE_RINGS_PER_QUEUE` (1–2) is the A/B pin, an explicit 2 the probe refuses fails the
mount loud. Each drain group becomes `rings` groups — two `sqz-fuse-ur<qid>` workers per queue
(the identical comm: the daemon's class is a prefix match, but the perf harness strips trailing
digits, so a suffix would split the class), each with its own io_uring, kmbuf ring and
`geometry.ringShare`: ring 0 `ceil(depth / 2)` entries at queue-level slots `0..`, ring 1 the rest
at `ceil(depth / 2)..` (the queue's depth and the pinned bytes unchanged: 2 × 16 buffers at depth
32). A slot's address `(qid, ent_idx, commit_id)` now carries the queue-level index (`ring_base +
ent`); the kernel's `buf_index`, the slot table and `gent` stay ring-local; the session resolves an
address to the ring that owns it (`locate`). The arm barrier counts member rings. `.stats`:
`fuse_uring_rings_per_queue`. Red → green: `session` e2e "rings per queue" — `depth` LOOKUPs of
distinct park names from threads pinned to one CPU fill both rings' shares exactly (ring 0 owes
`ceil(depth/2)`, ring 1 the rest), the control arm owes the whole depth on its one ring; skipped
naming the kernel feature where the probe reads absent. The executor reserves
`MAX_RINGS_PER_QUEUE` driven lanes per queue. Suites: transport 141, session 126 (+2 skips
unrelated), e2e 110, exec 141, kv_fs 93, lifecycle 80, probes 56, geometry 109, knobs 55, rails
32; the smoke's kernel-aio, data-plane (with/without cache) and KILL MATRIX rows green on the
two-ring transport.

**A/B** (A = `dev` 941d343, B = the branch on it; legs A B B A B A A B, 20 s rows, 4 per side):

| row | A mean (spread) | B mean (spread) | B ÷ A | daemon cores A → B |
|---|---|---|---|---|
| 4k randread QD1 | 89.2 k (33 %) | 97.3 k (31 %) | 1.09 | 0.60 → 0.59 |
| 4k randread QD32×1 | 281 k (17 %) | 487 k (27 %) | **1.73** | 1.02 → 2.01 (two workers; fio's thread at 89–94 %: the submitter is now the ceiling) |
| 4k randread QD32×4 | 871 k (13 %) | **1,249 k** (2 %) | **1.43** | 4.07 → 7.96 — the §1.4 read-IOPS bar (942 k) cleared |
| 4k randwrite QD1 | 72.1 k (13 %) | 77.7 k (9 %) | 1.08 | 0.67 → 0.68 |
| 4k randwrite QD32×1 | 199 k (13 %) | 202 k (12 %) | 1.02 | 1.79 → 2.27 |
| 4k randwrite QD32×4 | 384 k (4 %) | 373 k (11 %) | 0.97 | 6.93 → 8.12 |

Reads scale as designed: one submitter's QD32 spreads over the queue's two workers (p50 118 → 64
µs). Writes at the shipped depth 8 do not move: their bound was never the worker's CPU alone.

**The depth-0 question** (both arms two rings, legs D B B D D B B D on the write rows, D =
`SQUEEZEFS_FUSE_WRITE_OWN_DEPTH=0`):

| row | depth 8 mean (spread) | depth 0 mean (spread) | 0 ÷ 8 | cores 8 → 0 | p50 8 → 0 µs |
|---|---|---|---|---|---|
| 4k randwrite QD1 | 63.2 k (6 %) | 67.2 k (32 %) | 1.06 | 0.64 → 0.65 | 14 → 13 |
| 4k randwrite QD32×1 | 203 k (14 %) | 169 k (11 %) | **0.83** | 2.46 → 1.66 | 139 → 175 |
| 4k randwrite QD32×4 | 345 k (5 %) | 377 k (12 %) | 1.09 | 7.76 → 5.76 | 348 → 312 |

**Ruling: the default stays 8.** All-own still regresses the one-submitter row, and not for the
reason lever 4 named: the queue's two workers sit at 0.83 cores each, not saturated. Per write
the two-ring workers enter 2.4–2.9× and run 1.6–1.9 passes (one ring at depth 8: 1.2 enters, 0.78
passes — a saturated worker batched 1.3 writes per pass; a half-idle one wakes per CQE), and an
all-own write crosses the pass boundary twice (delivery → store SQE; store CQE → postlude →
commit), so its latency is two park/wake cycles of a worker holding 16 requests: p50 175 µs at 16
in flight per worker is the 0.83×. The four-submitter row gains 1.09× at 0.74× the cores, as
before. What binds the all-own write row now is the worker's pass structure under a half-idle
load, not its CPU — the next lever is there (a bounded spin before the park while a fiber waits on
this ring's own device op, the ack-spin's rule; or the knee re-swept with 16 in flight per worker),
and the 4k write IOPS gap (373–384 k vs the 727 k bar) stays open. The full-matrix A B B A on the
shipped default is in the table below.
