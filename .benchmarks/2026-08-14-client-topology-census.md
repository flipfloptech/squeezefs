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

## The attribution rounds (same day — three theories killed, one term counted)

**Round 1 — wake placement (tracepoints, 5 s window, 3.26 M wakeups):** the pile-up theory is
DEAD: 90–96 % of wakeups land on idle CPUs (svc 90 %, tpc 91 %, fio 96 %); the busy-target-
with-idle-available arm is only 4–10 %. What the trace DID show: **~360k migrations/s**
(fio 206k/s, tpc 73k/s, svc 44k/s) and interrupt-context wakers dominating ("other" class =
softirq/irq on the nvme-tcp and futex paths).

**Round 2 — static partitions (taskset scouts): every variant LOSES.** free 174k; daemon
pinned 0–15 = 119k; hard partition 16/16 = 115k; clients pinned = 159k; partition 24/8 = 162k.
The scheduler's own placement beats every static mask — the placement-lever class is dead on
this box (and the NUMA campaign's node pins stay the only sanctioned placement machinery).

**Round 3 — C-state exit latency: falsified.** `/dev/cpu_dma_latency = 0` clamp: 164k vs
168k free (wash; box idle states POLL/C1/C2/C3, 0/1/18/350 µs).

**Round 4 — park-cycle latency: COUNTED REAL.** The svc threads park ~50k/s at fan-in; every
park costs the next burst a wake→run cycle. `SQUEEZEFS_IPC_SPIN_US` dose-response (single
legs): 20 → +2 %, **100 → +6 %**, 200 → +4 %, 400 → +2 %, 800 → +4 % (plateau ≈ 100–200 µs).
Counted A-B-B-A at 100 µs: **183/182k vs 166/176k (+6.7 % median), p99 23,987 vs 24,773 µs,
both orders, engagement exact.** The 2026-07-26 record priced this lever as a CPU-taxing
explicit fleet lever — at 17 % box utilization the tax is free, which is exactly the
derivation opportunity.

## The lever design this hands the next PR

**Adaptive svc spin derived from observed idleness** — never a constant default: spin the
empty-pass window only while the recent pass-occupancy/park-rate says the lane is in the
park-churn regime and the process has CPU headroom (utilization-derived, cores-scaled, the
probe-governor pattern); bleed to 0 under saturation (the CPU-theft posture the record
demands) and on latency shapes (qd1 hard gate). The +6.7 % counted ceiling at 32×8 is the
acceptance bar; the remaining topology gap (185k → 257k by client count) past that term is
dominated by client-side worker park/wake cycles (fio's own threads — not ours) and the
migration churn the scheduler chooses (counted, but every static override loses).

## The governor rounds (same day — built, iterated, adjudicated default OFF)

`src/spin_governor.rs` + wiring (`SQUEEZEFS_IPC_SPIN_ADAPTIVE`, instruments
`ipc_spin_{window_us,absorbed_parks,disengaged_busy}`). Three counted iterations, each a
falsification honestly kept in the code docs:

1. **2×EWMA sizing — falsified**: fan-in parks are micro (µs), so the proportional window
   derived ~10–20 µs and counted a WASH against its own control. The engaged magnitude became
   the measured 100 µs plateau constant; regime membership stays derived.
2. **2× headroom margin — falsified by its own engagement probe**: `disengaged_busy` = 1.64M
   of 1.82M park cycles — /proc/stat busy includes the workload's own nvme-tcp softirq, so the
   25 % ceiling refused the exact venue where the static window won. Re-derived at 1×
   (busy + lanes/cores ≤ 100 → 63 % here).
3. **Temporal-only regime test — falsified by the qd1 hard gate**: qd1 parks are RTT-spaced
   (≈ 44 µs, INSIDE the rail) and the engaged governor cost qd1 **2.5×** (22.7k → 9.0k, p99
   70 → 157 µs) and 1×32 −13 %. The structural guard landed: **multi-session lanes only**
   (fan-in interleave is what a spin absorbs; a single session's park is the sync lane's
   productive RTT wait — the 2026-07-26 protected row).

**Final guarded form, counted (32×8 A-B-B-A ×2 + guards)**: qd1 22.3k/p99 77 µs ✓, 1×32 par ✓,
fleet parks **−40 %** (1.01–1.14M vs 1.83–1.92M) with absorbed 547–698k — and **IOPS wash**
(G 180/182/183/186/186/190 vs C 172/177/180/181/184/189 across the day). Decisively: the
morning's static-100 **+6.7 % did not replicate** across the afternoon (the control band alone
spans 166–189k — foreign-load variance the user accepted working through).

**Adjudication (the falsified-lever rule): default OFF.** The machinery ships as a field
measurement lever with live instruments and the three guards (the SQPOLL precedent); the
park-cycle term is real (parks −40 %, absorbed counts) but is not the fleet wall's dominant
term on this venue. The topology campaign's honest residual: the 185k→257k client-count gap
is dominated by client-side effects (fio's own park/wake cadence at qd8/proc — not ours to
thread) and scheduler-chosen migration churn that every static override made WORSE.
