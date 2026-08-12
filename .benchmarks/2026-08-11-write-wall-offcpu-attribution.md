# 2026-08-11 — The 630–650k write wall: off-CPU/wait attribution

**Campaign:** il rand-4k write IOPS (target 1 M). Continues
`.benchmarks/2026-08-08-shim-drain-funnel-r3.md` and the direct-drive lane
(`docs/design-il-direct-write.md`).

**Instrument + substrate (standing rule):** rig `squeeze-test`
(memp-s3ds-aqs-37, 32 CPU, kernel 6.19.14-sqz), nvme-tcp fabric to 3 remote
targets × 2 paths (2×200GbE), 10 data + 5 meta namespaces. elbencho 3.1-11
dynamic, `-w --rand -t 32 --iodepth 32 -b 4k --direct`, engaged-il rows via
`row_diag.sh` (posture `SQUEEZEFS_WRITE_SHARED=1` verified per row; every row
passed the charter-rule-4 engagement check — `ipc_ops_write` Δ = 16,777,216 =
the row's exact op count on every leg). Daemon+shim paired from one
`dist/rocky8/` at `d28a0435` (KD-7 verified: build-id `d7c7871b…` on both
ends). Captures: `perf record -e sched:sched_switch --filter "prev_state!=0"
-g --call-graph dwarf,512` (0.15 s @ plateau, 106,578 voluntary-park stacks),
`perf sched record` 0.5 s system-wide (2.98 M events), `syscalls:sys_enter_futex`
1 s (325,858 events) + `/proc/<pid>/maps` resolution, and
`io_uring:{submit_req,task_add,complete}` + `block:block_rq_{issue,complete}`
0.3 s system-wide (740,753 events). All rows within a leg share one aged
store; cross-leg deltas below are same-direction repeats, not keep/revert
verdicts (no A-B-B-A owed — every lever leg below is a WASH/negative).

## Verdict in one paragraph

**There is no single parked-on word.** The futex census (1 s, every
`sys_enter_futex` in the daemon) shows zero contended userspace lock words:
the [heap] words are 32 per-thread parkers (one per NvmeBlockDev worker,
waits ≈ wakes ≈ 2.1 k/s each), the memfd words are the session doorbells
doing their job (svc ingress parks + 143 k/s completion wakes ≈
`ipc_cqe_wake_writes`), and the [anon] words are tokio parkers. The D-state
census names the only real kernel-mutex convoy — the dd shards'
`ctx->uring_lock` between each lane's svc submitter and its reaper (~12 k/s
at 68–186 µs means, ≈ 1.1 s/s fleet-wide) — but **both lane-shape
discriminators falsify it as the wall** (below). The wall is not a token at
all: it is the **closed-loop residence identity**. 32×32 = 1,024 outstanding
÷ measured avg latency 1.57–1.68 ms = 610–650 k IOPS on every leg, exactly
the plateau. The ~1.6 ms/op residence is conserved because it is mostly
**queueing for the per-lane drain-pass cadence**, and that cadence is set by
per-op ceremony cost × ops-per-pass — not by any single waited-on resource.
Raising IOPS past the plateau requires **cutting per-op daemon residence**
(the 23–25 µs/op CPU and the pass/flush/reap quantization), not widening any
pool. Candidate-set disposition: BUFFER_POOL/SeveredPool — RULED OUT for the
dd path (severs fire only on the 14.5 % fallback arm: `ipc_severed_pool_hits`
Δ ≡ `ipc_async_handoffs` Δ on every row); session doorbell words — RULED OUT
(per-session, elide ratio healthy at 77 %, wake cost measured and spread);
`begin_patch_sole_owner` incarnation word + 4-tier `purge_block_key` — not
parked on (no futex traffic; they are burned-CPU candidates, already visible
to cycle profiles, and the wall's signature is waited-not-burned).

## The wall arithmetic (measured, this session)

| Quantity | Value | Source |
|---|---|---|
| Plateau (all 6 engaged legs) | 578–616 k IOPS | row_diag rows |
| Client avg latency | 1.57–1.68 ms | elbencho `--lat` |
| Outstanding | 32 × 32 = 1,024 | shape |
| Little's law | 1,024 ÷ 1.64 ms = **624 k** | identity — matches every leg |
| `ipc_direct_phase_ns` total | ~1,050 µs (admit 32 + inflight 1,013 + finish 4) | phase_read |
| `ipc_ingress_ns` | 151–221 µs | phase_read |
| Drain passes | 970,611/row = 12 lanes × 2.9 k/s; **17.3 ops/pass, ~345 µs/cycle → 12 × 17.3/345 µs = 601 k** | `ipc_drain_pass_ns` n + row wall time |
| Flush (io_uring_enter incl. inline nvme-tcp TX) | 88 µs/pass ≈ 5 µs/op ring tenure | `ipc_drain_flush_ns` |
| Kernel `submit_req → complete` (uring CQE) | mean 239 µs, p50 141 | io_uring tracepoints |
| Block layer `rq_issue → rq_complete` | mean 102 µs, p50 40 | block tracepoints |
| Device+fabric headroom | raw rand-4k **read** on the same 10 namespaces: **3.107 M IOPS** @ 328 µs avg, same shape | raw control row |
| Box CPU during plateau | ~89 % non-idle but 34 % iowait; daemon 13.6–15.7 cores | mpstat + row_diag |

The daemon-side surplus above the kernel's 239 µs — i.e. `inflight` 1,013 −
239 ≈ **770 µs of CQE/SQE quantization** (unflushed-tail wait for the
sweep-end flush + CQ residence until the owning thread's next reap point) —
plus `ingress` ~200 µs plus admit is the whole 1.6 ms story. Nothing else is
big enough to matter.

## Falsified this session (counted rows, engagement exact)

1. **Lane width / session partition** (the funnel-capacity theory):
   `SQUEEZEFS_IL_SESSIONS=16` (16 sessions over 12 svc lanes, vs default 8)
   → **578 k** (slightly *worse* than the 599–614 k default legs). Composes
   with the standing width washes (svc 12→28, 2-process fleet) — the wall
   does not respond to parallelism in ANY direction. A queueing wall on a
   specific pool would.
2. **Park/wake hop cost** (`SQUEEZEFS_IPC_SPIN_US=100`, svc spin instead of
   park) → **604 k**, wash. The wake hop is not the residence term.
3. **Reaper/svc ring convoy sign** (`SQUEEZEFS_IPC_DD_INLINE_REAP=0`,
   reaper-only CQ drain) → **616 k**, wash-to-slightly-plus; `inflight`
   grew to 1,211 µs while `ingress` fell to 151 µs and passes got shorter
   (161 µs at 1.18 M passes) — the same conserved-residence reshuffle the
   eager-flush K sweep showed. The uring_lock convoy is real (D-state census)
   but immaterial at this operating point.

## What the captures DID name (the residence ledger, per op)

At 600 k IOPS the daemon spends 23–25 µs CPU/op and the op spends ~1.6 ms
resident. The off-CPU + tracepoint evidence decomposes the residence:

- **SQE ingress quantization** (~150–220 µs): publish → svc dequeue
  (`ipc_ingress_ns`) — known since r3.
- **SQE flush quantization + CQE reap quantization** (~700–800 µs
  combined): an op's SQE waits for its lane's sweep-end flush; its CQE
  waits for the lane's next inline-reap/reaper cycle. Direct evidence:
  reapers carry ~45 % of all `io_uring_submit_req` events (their `enter`
  flushes SQEs the svc sweep staged — submit tracepoint attribution table
  in the analysis), and `submit→complete` p50 141 µs vs block-layer p50
  40 µs says ~100 µs of that is kernel-side task-work/posting cadence, the
  rest daemon-side drain cadence.
- **Per-op postlude ceremony on the reap path** (burned, serialized within
  each lane): `finish_write` runs `publish_block` + 4-tier
  `purge_block_key` + 2 whole-file LRU removes + `publish_attr` +
  train-pump synchronously on the reaping thread, then dispatches
  `spawn_dd_write_times_park` to a fuse3-tpc lane **per op** — the sched
  capture's dd→tpc wake edge (104 k wakes/0.5 s ≈ 208 k/s ≈ 0.35/op) and
  the tpc lanes' 354 k parks/s at 30–50 µs each are this tail's fan-out
  churn. tpc lanes burn 2.8 cores at ~4.4 k parks/s/lane servicing
  micro-tasks.
- **Inline nvme-tcp TX under the ring flush** (~5 µs/op, exclusive ring
  tenure): the park stacks show svc/dd threads preempted inside
  `io_submit_sqes → blk_mq → nvme_tcp_queue_rq → tcp_sendmsg →
  __dev_queue_xmit` at ~13 k/s — the whole TCP transmit runs on the
  flusher under `uring_lock`.
- **The fallback arm** (14.5 % of ops): handoff → tpc handler →
  NvmeBlockDev workers (the 32 bare-`squeezefs` threads; parked 80 % of
  the window, woken by tpc at 379 µs mean — an underutilized, long-latency
  path riding on top of everything above).

## Consequences (the fix direction — no code this session)

The wall is **per-op daemon residence quantized by lane cadence**, so the
levers that move it are the ones that cut quanta per op or ceremony per op,
not pools/widths:

1. **Cut the CQE→ACK quantum**: the completion path pays up to a full lane
   cycle (~345 µs). Single-issuer dd rings (`IORING_SETUP_SINGLE_ISSUER` +
   `DEFER_TASKRUN`, owner does submit+reap on one thread, backstop on a
   registered eventfd instead of a second ring tenant) deletes both the
   uring_lock convoy AND the two-tenant reap cadence — but leg 3 above says
   the convoy alone is not the win; this pays only if it lets the owner
   run a tighter reap loop.
2. **Cut per-op ceremony on the reap path**: batch `finish_write`
   postludes per reap sweep (one `publish_attr` coalesce per (ino, sweep),
   one times-park dispatch per sweep instead of per op — the 208 k/s tpc
   dispatch churn), batch doorbell wakes per (session, sweep).
3. **The client-side term**: elbencho clat 1.64 ms at 624 k means the
   client fleet itself holds 1,024 ops against a daemon that can only keep
   ~24 in flight at the devices (row_diag device gauge; raw control shows
   the devices take 300+ at this shape happily). Everything between is
   daemon queueing. The honest headline: **residence, not bandwidth**.

Deferred (standing): conveyor A-B-B-A (rails-green wash-to-minus leg-1 —
verdict still owed before keep), clock-ceremony (~23 % of lane cycles) and
`clear_page_erms` (6.3 %) levers, qd-knee governor row. Watch item
(size-0 stat after kernel-abort unmount): checked this session across 4
kernel-abort unmount cycles — all 32 files stat 2 GiB every time; NOT
reproduced.

## Addendum (same day): the yield board re-measured — two stale numbers retired

**1. The "~23 % clock ceremony" deferred lever is DEAD on the current tip.**
Fresh cycles profile (`perf record -F 3997 -g --call-graph dwarf,512`, svc+dd
lanes, 8 s @ plateau, 149 k lane samples): clock-containing stacks are
**0.8–1.0 %** of lane cycles — the r5 single-read law + span-form records
already killed it, and the residue is kernel-internal (`tcp_write_xmit`'s own
mstamp). `clear_page_erms` likewise ≈ 0 on the lanes now. The fresh lane
ledger: **inline nvme-tcp TCP transmit ~34 %** (38 % svc / 30 % dd — the
whole `io_submit_sqes → blk_mq → nvme_tcp_queue_rq → tcp_sendmsg` transmit
runs on the submitting thread), io_uring enter/CQ ceremony ~24 %,
futex/wake ~10 %, memcpy ≈ 0.

**2. The inline-TX offload (`nvme_tcp.wq_unbound=Y`) is a WASH at steady
state — and the A-B-B-A law caught the false positive.** Substrate flip via
ordered full-fabric reconnect (mapping verified byte-identical modulo the
`host_traddr` field `nvme connect -w` adds; flip script
`/scratch/tmp/wq_flip.sh`, restore verified). Bracket (B-B-B-A-A, all
engaged, posture verified): B1 **690 k** @ 1.42 ms / 20.6 µs/op CPU,
B2 638 k, B3 635 k | A-close 637 k, 636 k @ 1.56 ms / 23.9 µs/op. B1 was a
**fresh-TCP-connection transient** (every first-row-after-reconnect runs
hot); steady-state B ≡ steady-state A at ~636 k. The REAL signal: daemon
CPU/op fell ~1.5–3 µs with TX on unbound kworkers, and IOPS did not follow —
**direct confirmation the loop is residence-bound, not lane-CPU-bound**.
Also note the day-drift: morning A band 598–616 k vs evening 636–637 k on
both postures (profiler-attached rows and connection age both depress rows —
same-bracket comparison is the only valid read; the note's morning/evening
bands must never be cross-compared).

**3. Where the residence actually sits (the refined ledger).** With kernel
`submit→CQE` at 239 µs and `admit` span 33 µs vs ~7 µs svc CPU/op, the
inflight surplus (~760 µs) is the **sweep-serialized admit structure**: a
pass admits ~17 ops sequentially (~33 µs each, span not CPU — the balance is
preemption/stall on an ~89 %-busy box) and flushes ONCE at sweep end, so an
op staged early in a sweep waits most of the sweep before its SQE enters the
kernel, then its CQE waits for a reap point. Mid-sweep eager enters are
already falsified (r5, eager-K sweep); cutting the admit span's stall term
and the per-op post-ACK fan-out (the 208 k/s `spawn_dd_write_times_park`
dispatch churn + 292 k/s wake edges toward tpc — pure preemption pressure on
the very threads whose pass length sets the wall) is the surviving yield
board, in that order.

## Row ledger (engagement exact on every row)

| Leg | IOPS (first/last) | daemon CPU | notes |
|---|---|---|---|
| offcpu capture row | 605,885 / 599,332 | 13.85 cores, 22.9 µs/op | dwarf park capture 0.15 s mid-row |
| sched capture row | 607,351 / 604,540 | 14.84 cores, 24.4 µs/op | perf sched 0.5 s system-wide |
| futex capture row | (row output discarded — capture leg) | — | 1 s futex census |
| sess16 discriminator | 577,916 / 561,411 | 14.53 cores, 25.1 µs/op | `SQUEEZEFS_IL_SESSIONS=16` |
| spin100 discriminator | 604,535 / 601,306 | 15.71 cores, 26.0 µs/op | `SQUEEZEFS_IPC_SPIN_US=100` |
| noinline discriminator | 616,508 / 614,003 | 15.52 cores, 25.2 µs/op | `SQUEEZEFS_IPC_DD_INLINE_REAP=0` |
| uring tracepoint row | 598,444 / 595,929 | — | io_uring+block tracepoints 0.3 s |
| raw control | 3,106,730 (read) | — | 10 namespaces direct, same shape |
| cycles capture row | 620,352 / 616,100 (capture-free) · 567,464 / 564,508 (profiler attached) | — | lane-cycle ledger source |
| wqY B1 (fresh connect) | 689,800 / 681,839 | 14.24 cores, 20.6 µs/op | TRANSIENT — see addendum |
| wqY B2 / B3 | 638,144 / 634,625 | ~14.3 cores, 22.5 µs/op | steady-state B |
| wqN A-close ×2 | 637,014 / 635,741 | 15.23 cores, 23.9 µs/op | bracket closed: WASH |

Numbers stay internal until a release battery (standing rule).

## Addendum 2 (2026-08-12): the split instrument landed — the wall is the reap cycle

The `ipc_direct_phase_ns` family gained `sq_wait` (slab insert → the
`io_uring_enter` that carried the SQE; stamped at the `pending_submits`
claim under the shard state lock, ONE clock read per enter) and `device_cq`
(that enter → CQE pop; `n(device_cq) ≡ n(inflight)` exactly, the unstamped
racer class records full-span device_cq and no sq_wait — measured racer
rate 133 of 14.16 M = 0.001 %). Contract test:
`direct_drive_inflight_splits_into_sq_wait_and_device_cq` (red-first).

First decomposed rows (engaged, posture verified, 636–641 k @ 1.57 ms —
the evening band): `admit` 31–35 µs, **`sq_wait` 78–86 µs**, **`device_cq`
877–964 µs**, `finish` 5 µs. Same-row kernel `submit→complete`
(tracepoints, dd rings): 209 µs mean. Therefore **CQE-posted → popped
≈ 670 µs — two thirds of the daemon residence** — and the population
arithmetic corroborates: 542 k/s × 877 µs ≈ 475 ops resident in device_cq
vs ~113 inside the kernel window (Little's law on the tracepoint lag) vs
~24 at the devices (diskstats). The sweep-flush quantum every prior lever
targeted (eager-K, lane-flush, inline-reap) is the SMALL half (78 µs);
the CQ-side reap cycle is the wall: one consumer per shard pops a batch
and runs each op's FULL `finish_write` postlude (publish_block, 4-tier
`purge_block_key`, two whole-file LRU removes, `publish_attr`, W1 inval,
doorbell, train pump, times-park dispatch) inline between pops, so a CQE
posted mid-cycle waits out the entire cycle before its ACK.

Next lever (named, measured, in the residence path): shorten the reap
cycle — either price-and-cut the postlude's dominant term (the cycles
profile points at the tiering::memory purge machinery: the moka/scc
removes) or restructure the drain into pop-ACK-fast/defer-heavy-tail
shapes that preserve the per-op purge-before-ACK law. Every candidate now
has a per-quantum readout (`device_cq` must fall; `sq_wait` must not
grow) instead of an IOPS-only wash.

## Addendum 3 (2026-08-12): ACK-fast batch drain — the plateau moves (+22 %, A-B-B-A kept)

The reap-cycle lever landed (`perf/dd-ack-fast-drain`): the drain now
collects each write postlude's POST-ACK tails into a per-batch
`DrainBatch` — the durable-times handoff coalesces to ONE ino-deduped
fuse3-lane dispatch per batch (`ipc_dd_write_times_dispatches`; live
coalesce factor 20.3 on the counted row — the 208 k/s dd→tpc wake edge
is gone) and the conveyor pump's follower re-drives (SQE prep + inline
TX) run after the batch's last ACK. Every pre-ACK law is unchanged and
per-op: publish_block, the 4-tier purge, LRU drops, attr publish, W1
inval, then the ACK. Rails: `drain_batch_coalesces_the_times_park_dispatch`
(red-first, with the new tests-only CQ-ready probe seam
`test_ddw_cq_ready` for a deterministic batch), byte-exactness through a
tier purge, all direct-write/direct-drive/op-economy suites green.

**A-B-B-A on the aged store (engaged, posture verified, same fabric):**
A 636–641 k (pre-change binary `dd67a1f1`, evening band) → **B 783.1 k**
→ **B 775.7 k** → A-close **624.6 / 642.0 k** (re-deployed `dd67a1f1`).
The +21–23 % survives the reversed order; the closing A legs reproduce
the old plateau exactly. No fresh-connection transient this time (the B
legs repeat within 1 %).

**The per-quantum readout the split instrument was built for:**
`device_cq` 877 → **591 µs** (the targeted quantum fell), `sq_wait` held
(86 → 108 µs), clat 1.57 → **1.27 ms**, daemon CPU/op 24.1 → **19.2 µs**,
drain passes 923 k → 360 k/row (39 ops/pass — deeper batches, cheaper
per op). Little's law closes: 1,024 ÷ 1.27 ms = 806 k ≈ the measured
783 k.

**Standing rows after this landing:** engaged il rand-4k **783 k/776 k**
(was 630–650 k); the remaining residence board is ingress 326 µs (grew
as the queue moved upstream — the next board's top), device_cq 591 µs
(kernel 209 + reap ~380), sq_wait ~108 µs. The 1 M target needs
residence ≤ 1.02 ms at qd 1024; current 1.27 ms.
