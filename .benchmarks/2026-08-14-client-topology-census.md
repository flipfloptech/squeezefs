# 2026-08-14 — client worker/reaper topology: the opening scheduler census

**Context:** the wake-economy campaign closed with its central theory weakened
(`.benchmarks/2026-08-14-wake-economy-pr2-latch.md`): removing 22–70 % of daemon completion
FUTEX_WAKEs does not move the 32-proc fleet rows. This note opens the successor investigation
with scheduler-truth attribution. Binary pair `6e58154c` (dev tip class), TCP devsub,
written-fileset rows, instrument `/tmp/sched_census.sh` (per-tid `/proc/<tid>/schedstat`
deltas over a 10 s mid-row window, classed by comm).

## Fact 1 — the shim spawns NO client threads (code-verified)

`crates/squeezefs-preload` contains zero `thread::spawn`: the libaio "reaper" IS the
application's own worker inside `io_getevents` (parks on the session doorbell). Every prior
mention of "32 client reaper threads" in this campaign's notes was wrong; client-side thread
topology is not ours to change — the client half of any fix is affinity/placement, not
threading.

## Fact 2 — the fan-in wall is DAEMON-side runqueue wait (counted)

Per-class scheduler ledger, 10 s window (box: 32 CPUs = 320 CPU-seconds available):

| class | 32×8 (175k IOPS): cpu / runq-wait / wait-per-slice | 4×64 (255k): same |
|---|---|---|
| daemon `sqz-ipc-svc` ×12 | 15.8 s / **11.0 s** / **10.8 µs** | 12.9 s / 2.2 s / 3.6 µs |
| fio workers | 15.6 s / 10.1 s / 2.9 µs | 5.1 s / 0.5 s / 1.3 µs |
| daemon `fuse3-tpc` ×31 | 14.8 s / 6.2 s / 5.0 µs | 8.6 s / 1.6 s / 1.3 µs |
| daemon `sqz-ipc-dd` ×12 | 8.0 s / 4.8 s / 7.5 µs | 10.0 s / 1.8 s / 2.8 µs |
| **totals** | ~55 s cpu / **~34 s wait** | ~38 s cpu / **~7 s wait** |

Same offered depth (256), same fileset discipline: 32 procs accrue ~5× the runqueue wait,
concentrated in the daemon's svc/tpc/dd threads (wait-per-slice ×3 on svc), and the deficit
matches the 175k→255k row gap.

## Fact 3 — the wait accrues WHILE THE BOX IS ~17 % BUSY

55 s of CPU in a 320 CPU-second window, with 34 s of runqueue wait: threads are queueing
behind busy CPUs while most CPUs idle. That is a WAKEUP-PLACEMENT signature (the wake-affine
pile-up class: 32 waker processes pull the daemon's threads toward their own core
neighborhoods, convoying them), not a capacity problem — consistent with the width sweep's
finding that 12 svc lanes is the interior optimum and with wake-syscall COUNT being exonerated
by the latch bracket.

Note: the NUMA campaign's service-thread pins are NODE-level (`numa_core` nearest-map ∩
process mask) — structurally inert on this 1-node box, so nothing currently constrains
placement here.

## Open next (the attribution still owed before any lever)

1. **Wake-placement attribution**: `perf sched` (or `sched:sched_wakeup` tracepoints) over a
   short 32×8 window — count wakee-placement decisions (same-CPU-as-waker vs idle-CPU) and
   migrations/s per class; confirm or kill the pile-up theory against the idle-CPU census.
2. If confirmed, the lever class is **daemon thread placement** (derived per-CPU spread for
   svc/dd/tpc within the process mask — topology-derived, never a constant; possibly
   `SCHED_IDLE`-class demotion for non-latency lanes), with the qd1/1×32 latency shapes as
   hard gates.
3. The client half, if any, is affinity HINTS only (the shim owns no threads) — and may be
   nothing.
