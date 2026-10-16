# Rung 10 — one fine clock read where a time is genuinely fine (clockreads; perf-baseline lever 3)

> **Reverted for the move checkpoint (2026-10-16).** The two-daemon teardown wedge (`docs/gates/flakes.md`, "THE TWO-DAEMON MATRIX") appeared only after these cuts — 3 of ~9 VM gates vs 0 of ~88 before — while `lanepark`'s audit found no stall in the clock arithmetic: the cuts most likely expose an older teardown race by changing timing. Re-land `57d9090` + `fa3fe60` (`git revert` the revert) after that wedge is root-caused and fixed; the numbers below are what to expect back.

**Why.** kamrui's clocksource is HPET, which has no vDSO mode. Every `monotonicNs` is a
`clock_gettime` syscall plus a device read (~1 µs). Before this change a 4 KiB read cost the
transport 11 such reads and a 4 KiB write 40. `read_hpet` was 30–38 % of the daemon's samples on
the 4k write rows.

**What changed.** `src/core/clock.zig` gains three clocks. `monotonicCoarseNs` is
CLOCK_MONOTONIC_COARSE: the vDSO's last tick, never the clocksource. `fineNear(deadline, horizon)`
reads the fine clock only when the deadline may be within one coarse tick plus `horizon`. `phaseNs`
reads the CPU's counter (the TSC when CPUID leaf `0x8000_0007` EDX bit 8 says it is invariant;
CNTVCT on aarch64), scaled by this process's own calibration against `monotonicNs`: a 100 ms
window, with both read brackets ≤ 1/1000 of the span. Until then it is `monotonicNs`. Callers:
- the coarse clock for ages judged in ms or more: the worker pass's watchdog stamp and census, the
  owed stamp, the lease's 1 s rule, the bridge's 30 s deadlines, read admission and prefetch epochs;
- the phase clock for spans on one thread: fiber `poll_ns`, the read serve and fill phases, and the
  write Prof legs;
- a lane or worker timer check reads the fine clock only near its deadline; REGISTER retries read
  it only while one is pending; the governor's completion reads it once (BDP sample and epochs).
- `fine_reads` (test builds only) counts fine reads; `scripts/perf/clockreads.sh` breaks them down
  by call site (the `sys_enter_clock_gettime` tracepoint, frame-pointer call graphs; `-Dframe-pointers`).

**Fine reads per op** (kamrui, RAM nvmet-loop, dist + frame pointers, QD1, 4000 ops; measured on
the cut before the rebase onto `e36aa06`, same diff):

| row | A (dev) | B | left in B |
|---|---:|---:|---|
| 4k randread | 11.2 (worker 11.0) | **1.15** (worker 1.0) | op_guard tryAcquire |
| 4k randwrite | 39.7 (lane 20.0, worker 19.2) | **9.9** (worker 0.0, lane 9.0) | op_trace 2, dlm 2, op_guard 2, futex deadline, writeFixed |
| 1M write | 47.3 | **23.9** | as 4k write + mintPermit |

**Red → green.** The e2e test `the transport's clock budget` (session.zig) does 128 O_DIRECT 4 KiB
pwrite+pread pairs and allows ≤ 1 fine read per FUSE request, process-wide. Red: the red commit
alone, run on kamrui on dev `9205013`: `clock budget: 3892 fine reads over 256 requests`, so
`error.ClockBudgetExceeded` (15.2 per request). Green: passes on the landed head (0 skipped). The full
gate on kamrui (`-Dfresh=true`): `55/55 steps succeeded; 3002/3085 tests passed (83 skipped)`.

**A/B** (kamrui RAM nvmet-loop, `matrix.sh`, dist builds, 15 s rows, 1 GiB regions, A = dev
`e36aa06`, B = this branch, legs A B A B; IOPS):

| row | A | B | Δ | p50 µs A → B |
|---|---|---|---|---|
| rr4k-q1j1 | 19,720 / 19,284 | 25,542 / 27,247 | +35 % | 47 → 34 |
| rr4k-q32j4 | 254,363 / 254,526 | 321,306 / 317,087 | +25 % | 469 → 373 |
| rw4k-q1j1 | 8,834 / 9,474 | 12,003 / 11,891 | +31 % | 100 → 77 |
| rw4k-q32j4 | 71,660 / 75,837 | 94,494 / 96,495 | +29 % | 1,516 → 1,163 |
| rr64k-q16j4 | 103,173 / 102,164 | 111,025 / 109,301 | +7 % | 557 → 514 |
| rw64k-q16j4 | 55,376 / 55,364 | 61,067 / 58,175 | +8 % | 1,012 → 938 |

No row's ranges overlap. `read_hpet` self share of the daemon's threads (`MX_PROFILE=1`, 10 s
`perf record -a -g` mid-row) falls from 29.8 % to 13.6 % on rw4k-q1j1 and from 37.8 % to 20.0 % on
rw4k-q32j4. Box-wide it falls from 37.8 % to 27.9 % on q32j4 and stays flat on q1j1 (18.1 → 18.9 %),
where the kernel's own `ktime_get` grows with the op rate. HPET is kamrui's quirk. On a TSC host
every read is a ~20 ns vDSO call, so the gain there is the syscall-free phase spans, not these
numbers.

**Risks.**
- Read admission and prefetch epochs run on the coarse clock. The epochs are seconds long, and a
  tick (4 ms at HZ=250) is noise against them.
- A lane or worker timer whose deadline is far bounds its wait from the coarse side
  (`d − (coarse + coarseLagNs)`). So it may wake up to the lag bound early, re-check and park
  again. It does not fire late, because a deadline that is near is always judged on the fine clock.
  The lag bound is 3 ticks, not the one tick `clock_getres` reports. The timekeeper accumulates
  whole tick intervals, so the coarse clock trails the fine one by up to 2 ticks. On kamrui
  (HZ=1000) the lag reached 2.01 ms, and 18 % of reads lagged by more than one tick. The first full
  gate caught this: `fineNear(now, 0)` returned null under load. The third tick absorbs a late tick
  interrupt. A longer timekeeping stall (no CPU ticking) could make a far timer late by the stall's
  excess.
- A phase clock shortcut: a thread that migrates mid-span between CPUs whose counters are not
  synchronized skews that one span, clamped at 0 by the callers' `-|`. Both venues are
  single-socket. A phase stamp is never a deadline and is never compared across threads.
- Not cut (left fine on purpose): op_trace begin/finish (the op's latency), op_guard/dlm hold
  timing, the futex-wait deadline, uring_fs's cross-thread device stamps, and the accumulator's
  born/touch stamps. Those stamps order fsyncs and need their own review before they can move to
  the coarse clock.
