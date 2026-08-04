# Transport-ingress lever 1: same-lane READ dispatch (2026-08-04)

**Branch** `perf/transport-ingress-dispatch` · **binary** dev `552c437d` + this
lever · **venue** local dev box (25 CPUs, 2× the fabric shape below), dev
substrate **tcp** (`SQZ_DEVSUB_TRANSPORT=tcp` — the fabric-sensitive venue per
the two-substrate rule) · **instrument** fio 3.42 via
`tests/fio/transport_ingress_sweep.sh` (randread libaio bs=4k qd=8, prefilled
512 MiB/job files, 20 s time_based points; phases = est-mean of the ALWAYS-ON
`read_transport_phase_ns`/`read_serve_phase_ns` histogram deltas).

## The named venue

`.benchmarks/2026-08-01-transport-ingress.md` §9.2 deferred per-op dispatch
work "until a venue/shape where the term is critical-path is named." The
2026-08-04 squeeze-test EXA battery named it: randread-kernel (libaio 4k qd8
njobs=32 = 256 in-flight) runs `queue_wait ≈ 293 µs + dispatch_lag ≈ 412 µs`
in front of a 363 µs serve — **66 % of user clat is pre-handler transport
queueing**, super-linear in in-flight (local 64-in-flight: 42 + 35 µs).

## The lever

`Session::handle_read`'s handler future was spawned through
`TPC_SCHEDULER.spawn` — a **global round-robin** onto some *other*
`fuse3-tpcN` lane's unbounded channel: one channel push + one cross-thread
wake + the target lane's drain/`spawn_local`, per op, while the dispatch loop
itself already runs ON a lane thread. Lever 1: when the dispatcher is on a
lane (`IS_TPC_LANE` thread-local, set in the lane body) the READ future
`spawn_local`s onto the **current** lane — the hand-off becomes a local queue
push; the kernel's qid ≈ submitting-CPU spread already provides load
distribution. Other opcodes keep the rotation until their venues are named.

Knob: `SQUEEZEFS_FUSE_SAME_LANE_DISPATCH` (registry entry; default **1**;
`0` = the rotation — the A0 control + operational escape). Parse rides the
ONE boolean convention (`env_knob_core::parse_bool`).

## Counted A-B-B-A (order-alternating; saturated points 16x8/24x8 = 128/192 in-flight)

| side | 16x8 IOPS | 24x8 IOPS | queue_wait | dispatch_lag |
|---|---|---|---|---|
| **ON** (run 1) | 321,768 | 310,165 | 40–48 µs | **27–32 µs** |
| OFF (run 1) | 272,595 | 233,969 | 60–69 µs | 49–57 µs |
| OFF (run 2) | 202,880 | 227,913 | 68–93 µs | 54–78 µs |
| **ON** (run 2) | 258,210 | 267,958 | 38–43 µs | 28–30 µs |

Both brackets ON-ahead, order-independent: ON avg 290.0k/289.1k vs OFF avg
237.7k/231.0k ⇒ **+22 % / +25 % IOPS at saturation**; dispatch_lag **halved**
(the deleted wake), queue_wait −35 % (the dispatch loop re-polls sooner with
its lane's channel no longer receiving foreign futures). Venue noise across
runs is visible (ON run 2 < run 1) — the verdict rides both-brackets-agree,
not any single pair.

## Guards (A-B-B-A, same substrate)

| side | seq_write 1M | seq_read 1M |
|---|---|---|
| ON / ON2 | 1.11 / 1.07 GB/s | 11.44 / 11.62 GB/s |
| OFF / OFF2 | 0.99 / 0.77 GB/s | 11.40 / 11.44 GB/s |

seq_read flat (the lever touches READ dispatch only; large serves await
device fills, so same-lane residency does not pile them); seq_write untouched
by construction (WRITE dispatch unchanged).

## Standing follow-ups

* Field verification on squeeze-test rides the next battery deploy (expected:
  the 293/412 µs terms compress; acceptance is the serve-decomposition note's
  `queue_wait + dispatch_lag < 0.5 ms` on the randread-kernel row).
* Lever 2 (READ fast-dispatch from the reap thread — kills the per-qid
  channel + reconstruction) stays queued; re-measure AFTER this lever's field
  row (its payoff bound shrank with queue_wait).
* The `.stats` torn-JSON-under-churn bug found while building the sweep
  (kernel clamps buffered reads at a stale `i_size` despite FOPEN_DIRECT_IO;
  `dd` reads the full fresh payload while `cat` tears mid-string) is queued
  red-first — it poisons every stats consumer on busy mounts; the sweep's
  `snap()` carries a parse-retry meanwhile.
