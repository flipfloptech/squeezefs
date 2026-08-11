# 2026-08-11 — Op-registry sharding (write-IOPS campaign, day 1 cont.)

**Branch** `perf/op-registry-shard` (`2be9bd77` red, `7889e743` impl,
`0cab9c21` bench). **Instrument**: elbencho rand-4k qd32 t32 O_DIRECT
60 s infloop over f{1..32} (2 GiB, preconditioned), `leg.sh` protocol —
fresh remount + posture verify + 30 s warm per leg, `row_diag.sh` stats
deltas + the daemon CPU face (`/proc/<pid>/stat` utime+stime around the
row, zero observation overhead). **Substrate**: squeeze-test
(memp-s3ds-aqs-37), 32 CPUs, 5 meta + 10 data nvme-tcp namespaces.
Posture `SQUEEZEFS_WRITE_SHARED=1` everywhere (stated per the pairing
law). Rows are internal evidence — no public citation before a release
battery (user ruling 2026-08-11).

## The lever

The 525 k-era worker profile (`perf_worker2.data`) named
`OpRegistry::claim` the largest userspace self line: **6.65 %** of
fused-worker cycles — a flat 256-slot slab 4× UNDER the delivered ring
capacity (32 possible CPUs × depth 32 = 1024), one global round-robin
cursor, 24-byte slots ~2.7/cache line. Fix (`7889e743`): shards =
possible CPUs (shipped-256 floor), slots/shard = 2 × `Q_DEPTH_DESIRED`,
64-byte slots, per-shard cursors, TLS home shard. Contracts:
`tests/op_registry_shard_tests.rs` (geometry tie, cache-line law, full
delivered-capacity registration, exhaustion/foreign-release/watchdog
carried forward); bench `op_registry/claim_release_storm_held32`.

## The bracket (B = 7889e743, A = 09c679f4 matched pair)

Leg order B1 B2 A2 B3 A3 (alternating both directions at the seams):

| Row | B1 | B2 | A2 | B3 | A3 |
|---|---|---|---|---|---|
| kern rand-4k IOPS | 517,853 | 533,563 | 527,383 | 527,253 | 519,972 |
| il rand-4k IOPS (engaged) | 273,220 | 275,651 | 294,523 | 272,368 | 279,180 |
| il seq-1m MiB/s | 39,001 | 38,700 | 38,538 | 38,736 | 38,559 |
| kern seq-4k IOPS | 403,386 | 405,682 | 406,946 | 407,145 | 408,566 |

CPU face (added at B3; B3/A3 only):

| Row | B3 (sharded) | A3 (flat slab) |
|---|---|---|
| kern rand-4k | **31.93 µs/op** @ 527.3 k | 33.19 µs/op @ 520.0 k |
| il rand-4k | 60.13 µs/op @ 272.4 k | 61.64 µs/op @ 279.2 k |
| il seq-1m | 381.2 µs/op | 383.2 µs/op |
| kern seq-4k | 44.11 µs/op | 43.98 µs/op |

## Verdict

- **kern rand-4k: KEEP — CPU-at-line-rate win** (the ruling lens:
  "a CPU drop while keeping the same line rate is a win"): −3.8 %
  daemon CPU/op at +1.4 % IOPS in the paired leg; IOPS medians
  527.3 k (B) vs 523.7 k (A) across the bracket. The full profiled
  3.4 µs did not convert — the claim still CASes (now on local lines)
  and the profile % was worker-SELF, not whole-daemon.
- **il rows: wash within venue noise** (A's own points span 5.5 %).
  seq rows: par. No regression anywhere.
- The lever also deletes a derivation-law violation (fixed 256) and the
  >8-CPU cliff where claims degraded to unregistered (watchdog-invisible)
  above 256 in flight — on this rig 768 of 1024 in-flight ops could not
  register.

## The KD-7 passthrough invalidation (found by this bracket's audit)

Every prior day-1 "il" row was a **silent passthrough**: the rig's shim
was still `4362ea74` while the daemon advanced, KD-7 refused every
session (announced on stderr, unread), LD_PRELOAD rows rode the kernel
ring, `ipc_ops_write` delta 0. Correction addendum in
`2026-08-11-write-iops-campaign-day1.md`; `row_diag.sh` now exits 9 on
any row whose output announces passthrough; deploys are paired by law.

## The next named target: engaged-il parity

First honest engaged-il ledger on this shape: **60 µs/op daemon CPU vs
kern's 32 at 0.52× the rate** (272–295 k vs 518–534 k). The il write
path burns ~2× CPU per op — the fattest single target on the road to
1 M, ahead of the kern qd-knee (527 k @ qd32 vs 368 k @ qd64) and the
remaining worker self lines (clock reads ~4 %, saa bucket spin 1.7 %,
timeline instrument fetch_adds ~1.5 %, kernel `fuse_request_end`
spinlocks 5.2 % — custom-kernel surface).

## Addendum — the direct-drive WRITE lane's first counted leg (same day)

Lane landed (`499ef67b`/`c1bd0ebe`/`65eb36f5`, docs/design-il-direct-write.md
§6): eligible il writes DMA on the svc lane's dd shards, postlude at CQE,
handoff fallback intact. Leg ddw2 (leg.sh protocol, Shared ON, engagement
exact — 32.1 M dd serves + 6.8 M handoffs ≡ 38.9 M ops, fence refusals 0):

| row | pre-lane | ddw2 |
|---|---|---|
| il rand-4k | 272–295 k @ 60–62 µs/op | **648,410 @ 23.46 µs/op** (clat 1.58 ms) |
| kern rand-4k | 518–534 k | 536,921 @ 31.39 µs/op |
| il seq-1m / kern seq-4k | par | par |

**+2.2× il IOPS, −61 % daemon CPU/op; il now leads kern +21 %** — the fan-in
wall named by the C1 investigation (lanes 42 % util, 3.68 ms clat) is gone
from the eligible shape. Residuals: `ineligible_custody` 6.78 M/38.9 M
(17 % — RAM lease-cache misses under churn; the next shaving), the one-time
size-0 stat anomaly after a kernel-abort unmount (watch item; clean-remount
discriminator showed durable sizes intact), single-leg so far — the A-B-B-A
closure leg + sustained row still owed before any headline.

## Addendum 2 — custody split + conveyor leg 1

Split (`836083bc`) attributed the 17% fallback class: **99.8% = block_lock**
(6.90M same-block try-lock losses; lease 50, killpriv/range/fence 0).
Conveyor (`d28a0435`, leg conv1): ledger closes exactly (parks ≡ redrives =
1.32M, fence 0, seq/kern par) but captures only ~18% of the contention —
il 628.8k @ 24.75µs vs the split leg's 652.8k @ 23.46µs (single legs, no
A-B-B-A yet). Mechanism: a fallback becomes a HANDLER write holding the
same stripe, so subsequent probes see a foreign holder and fall back too —
handler-mode is self-sustaining per contended block. The park predicate
(holder = open lane train) aims too narrow. Next design: classify same-
block ops at the SINK by (ino, block) BEFORE any guard exists (svc-side
coalescing into the block's train regardless of current holder class), or
arm train adoption on handler release. Conveyor verdict deferred to its
A-B-B-A; correctness rails all green (11/11 ×3).
