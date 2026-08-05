# 2026-08-05 — Drain-group queue workers (ingress-queue-spread lever 2)

**Lever:** `.benchmarks/2026-08-05-ingress-queue-spread.md` candidate (a) —
cross-queue drain aggregation, with (c)'s aggregate-keyed wake coalescing
folded in structurally. **Branch:** `perf/queue-spread-drain` (off
`integrate/zcrx-wave`). **Venue for the rows below:** the LOCAL tcp dev
substrate (`SQZ_DEVSUB_TRANSPORT=tcp`, zram oss / null_blk mds,
localhost nvme-tcp; 25-online/32-possible-CPU single-NUMA dev box) —
**a ~340k-IOPS venue that cannot reproduce the field's 3.36 M-fabric
headroom; the field ladder re-run is the ACCEPTANCE bracket** (§5).
Instruments: `tests/fio/transport_ingress_sweep.sh` (fio 3.42 libaio
randread-4k) + the stats inode (`transport_commit_batch*`,
`transport_wake_*`, and the new `transport_drain_groups`/`_width`).

## 1. What changed

One drain context (OS thread + io_uring + eventfd + `WakeCoalescer`) per
**group** of FUSE-over-io_uring queues instead of one per possible CPU:
all member queues' REGISTER / COMMIT_AND_FETCH SQEs ride ONE ring
(kernel-legal — `fuse_uring_cmd_req.qid` rides the SQE, `task_cpu`
routing and per-queue capacity untouched), one commit channel + one
eventfd per group, wake elision keyed on the group's aggregate state.
Per-queue ordering laws hold (one thread services each queue's slots in
pass order; `SlotTable`/lease words/parked slots stay per-queue), the
§5.4 lease/park ledger is untouched, and `Modern`
(SINGLE_ISSUER+DEFER_TASKRUN) ring posture survives (one ring, one
owner). kmbuf (`BufRing`) sessions derive width 1 — today's posture
byte-identical (per-ring per-queue registration; grouping under kmbuf is
the named follow-on).

Width is **DERIVED, never a constant** (hard-constant ruling 2026-08-05):
`drain_group_width(node_possible_cpus) = max(node_possible_cpus / 4, 1)`
— the house `cpus/4` drain-parallelism SLOPE (`il_sessions_default` /
`dd_shards_from` lineage), evaluated per NODE RUN (groups never span a
NUMA node; offline-CPU holes join runs — sysfs node cpulists carry
online CPUs only; floor 1 = a context owns at least one queue, and the
node-span ceiling is implicit). On the bracket venue (32-possible/1-node)
the slope evaluates to **8 — byte-identical to the counted bracket
winner**, so the §3/§4 rows carry over verbatim for this shape; the
field ladder re-grades the SLOPE, not a constant. Canonical shapes
(drift-is-red tie test): 32-possible/1-node ⇒ 8; 96-possible/2-node ⇒ 12
per node; 4-CPU box ⇒ 1 (today's per-queue posture).
`SQUEEZEFS_FUSE_DRAIN_GROUP` (registry, 1..=512) wins verbatim; `=1` is
the A0 per-queue-worker control.

## 2. Protocol lesson (recorded so the next bracket does not repay it)

Protocol v1 (fresh blkdiscard+format+prefill per leg) produced ±30 %
leg-to-leg swings on the SAME configuration — the zram pools' allocation
state ages across format/prefill cycles and swamps the effect. All
protocol-v1 rows were DISCARDED. Protocol v2: one blkdiscard + format +
prefill under a throwaway mount, then **mount-only alternation over the
standing store** (randread does not age it), both orders.

## 3. The counted rows (protocol v2, final binary)

IOPS (A = width 1 control, B = width 8 default; A-B-B-A order):

| point | A1 | B1 | B2 | A2 |
|---|---|---|---|---|
| 16×8 | 323,235 | 332,026 | 312,507 | 322,485 |
| 32×8 | 345,544 | 350,286 | 344,950 | 341,725 |
| 8×32 | 385,522 | 377,647 | 382,298 | 394,988 |

Verdict: **IOPS par within venue noise (±2–3 %) at every point, both
orders** — no regression anywhere, including the 8×32 guard point. Width
scan on the same standing store: width 4 ≈ par, width 16 ≈ par-to-−2 %,
**width 32 (whole node, one context) −15 % at every point** (281k/292k/
308k) — the single-thread drain ceiling, which is what falsified the
first-draft `min(Q_DEPTH_DESIRED, node span)` (whole-node) derivation;
the shipped `cpus/4`-per-node slope evaluates to the bracket winner (8)
on this shape.

## 4. Mechanism engagement (same 32×8 point, per side)

| side | commit flushes | commits | **mean batch** | wake writes | elided | **wakes/op** |
|---|---|---|---|---|---|---|
| A0 width 1 | 4,717,370 | 6,961,085 | **1.48** | 4,782,855 | 2,178,230 (31 %) | **0.687** |
| B width 8 | 2,192,510 | 6,538,805 | **2.98** | 2,223,323 | 4,315,482 (66 %) | **0.340** |

The lever does exactly what the field diagnosis asked: **−54 %
`io_uring_enter`s (2× the COMMIT_AND_FETCH batch) and −51 % eventfd
wakes per op** at the spread shape — this venue simply has no fabric
headroom for that saved CPU to buy IOPS (device service ~150–400 µs of a
~740 µs clat here vs 221–260 µs of 936 µs on the field's real fabric,
and 25 online CPUs serve both fio and the daemon).

## 5. Field acceptance row (owed — this note's numbers do NOT close the lever)

Re-run the evidence note's ladder + pin discriminator on squeeze-test
(wave binary with this landing): `tests/fio/transport_ingress_sweep.sh
--points "16x8 32x8 8x32"`, medians of 3, plus the
`transport_drain_groups`/`transport_commit_batch*`/`transport_wake_*`
deltas per point. Expected: 32×8 moves toward the 8×32 357k class
(wakes/op and enters/op halve at minimum — the mechanism rows above are
venue-independent); 8×32 must not regress. The field box (2 nodes × 16
possible) derives width 4 per node — if its ladder ranks a different
width, re-grade the **SLOPE** in `drain_group_width` (and its canonical-
shape tie test), never a constant — the local venue has proven it cannot
rank widths 1–8 by IOPS.

## 6. Verification

fuse3 suite 137/137 (`--all-features`, `--test-threads=1`), clippy
(all-features) clean, fmt clean; root: `derivation_sweep_tests` (width
tie test), `env_knob_convention_tests` (registry entry),
`metrics_tests`, and the live-mount transport suites
(`multi_queue_tests`, `transport_concurrency_tests`,
`transport_ingress_tests`, `transport_lease_overlong_tests`) green; the
storm/concurrency pair ×10 green on the final code; both root clippy
configs + fmt clean. New contracts: `drain_group_tests` (9 — plan
coverage/ordering, node clamp, offline-hole law, kmbuf singleton law,
explicit-width law, gent round-trip, stats gauge) + the root width tie
test.
