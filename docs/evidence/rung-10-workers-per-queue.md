# Rung 10 — lever 5: N workers per kernel FUSE queue (what the kernel admits, what binds)

Lever 5 of `rung-10-perf-baseline-workstation.md` (decision 0003 §AB.6): one io_uring submitter's
whole depth lands on ONE FUSE queue, so the all-own write arm (`SQUEEZEFS_FUSE_WRITE_OWN_DEPTH=0`,
`rung-10-write-lane.md`) made that queue's worker its one saturated CPU and ships at depth 8. This
note settles what the kernel allows, pins the saturation on this box, and leaves the lever's
kernel half compile-checked but unbooted. Venue: the Linux workstation (`local`), kernel
`7.2.8-7-omarchy-sqz`, head `464025a` (dist), the whole venue held (`scripts/perf/quiet.sh`),
session `ram-sqz-rl5-prof-r1` (`/build/perf/mx/`). No default changes; nothing landed in the product.

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
(moved into `patches-7.2/` 2026-10-19 on the owner's word, built as `linux-omarchy-sqz 7.2.8-7.30`; unbooted at this writing): the
buffer ring becomes a property of the registering ring — a queue keeps `struct fuse_ring_src`
per ring ctx (its pinned kmbuf ring, its zc depth), each entry points at its ring's source,
selection and the headers index go through it, a commit from another ring is refused `-EXDEV`,
and a FEATURES command (cmd_op 4) answers `FUSE_URING_FEAT_RINGS_PER_QUEUE` inline (an unpatched
kernel answers `-EINVAL`: the probe). One ring still serves one queue (`-EALREADY` on a second
pin). Compile-checked `W=1` as an out-of-tree `fuse.ko` against the built 7.2.8 tree; applies at
`--fuzz=0` after 0029. Fuse is a module here, but swapping it means unmounting the desktop's
gvfs/portal FUSE mounts, and a kernel install is a reboot — the owner's. The daemon half
(unwritten): probe → `drainGroupPlan` with 2 groups per queue under kmbuf/zc (the LIFO entry pick
engages the second ring only once the first's entries are all in userspace, so N = 2 doubles the
single-submitter ceiling and costs nothing idle; N > 2 needs its row), `ceil(depth / 2)` entries
per ring so the queue's depth and pinned memory are unchanged, the slot address gaining the ring
index, both threads named `sqz-fuse-ur<qid>` (the class contract), an e2e row skipped where the
probe reads absent — then the depth-0 A/B this lever was for.
