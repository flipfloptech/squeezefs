# Kernel per-queue FUSE background accounting — the A A B B (2026-09-06)

**Verdict: B SHIPS.** The per-queue background accounting (the COMMIT-lock
split — `docker/kernel-sqz/patches/0031-…` on the 6.19.14 field track,
`patches-7.1/0031`, `patches-7.2/0026`; design
`docs/design-kernel-bg-per-queue.md`) met every clause of the design's
§5 verdict rule on BOTH B boots: the `fuse3-ur` worker's µs per op fell
WITH the lock class (−18 %, against the expected −10 %), kern rand-4k
IOPS +9 %, p50 −9…−11 %, p99 −13…−17 %, the seq rows par, the il control
par, zero kernel-log lines from fuse/io_uring, no WARN, no lockdep.

## 1. Venue, instrument, arms

| | |
|---|---|
| Box | `squeeze-test` — 32-core Xeon, Rocky 8, 5-node nvme-tcp fabric (`/dev/nvme{0,2,4,6,8}n1`), `governor=performance`, package temp 44–46 °C at every perf row (both arms) |
| Kernel A | `6.19.14-sqz` = sqz series 0001–0030 (the box's 5-day-uptime boot; `fuse_uring_bg_wait` ABSENT in the loaded module) |
| Kernel B | `6.19.14-sqz` built `Sun Sep 6 18:26:37 UTC 2026` = 0001–0031 (`fuse_uring_bg_{wait,kick,limit_changed,abort_waiters}` PRESENT — the rig's arm test; the release string is unchanged, which is why the symbol test, not `uname -r`, tells the arms apart) |
| Daemon | the SAME binary both arms: `squeezefs 1.2.1 (27a396e18ada, tag stable-2026.09.1) profile dist` + its KD-7 shim |
| Rig | `.benchmarks/rigs/2026-09-06-kernel-bg-per-queue-ab.sh` — one boot per invocation: `cluster_reset_v4.sh` (fabric torn down + rebuilt + fresh cache-less format), mount (`--interception --allow-other`), one `write_BW` prep pass minting the set, then the rows. Table: `.benchmarks/rigs/2026-09-06-kernel-bg-ab-table.py` over the row dirs |
| Instrument | fio-3.36, libaio, `direct=1`; kern rows through the kernel FUSE-over-io_uring path (32 queues × depth 32, `transport_max_background` 1024 → the per-queue share under 0031 is **32**; fusectl `max_background=1024 congestion_threshold=768` on both arms), il control through `LD_PRELOAD=libsqueezefs_il.so` |
| Rows per boot | kern rand-4k 24 × qd8 30 s (+10 s ramp) × 2; the 60 s sustained kern rand-4k; 1 MiB seq `read_BW` qd16 × 2; one il rand-4k control; one kern rand-4k with `perf` on the `f3-ur*` threads (flat 8 s + 2 s DWARF) |
| Schedule | **A A B B** across reboots (A-B-B-A is impossible across boots): A boots 16:36 / 16:50 UTC, B boots 19:51 / 19:58 UTC, same day |
| Artifacts | `.benchmarks/rows-kernel-bg-ab-20260906/{A,B}/` — per row `.row`, `.fio.json`, `.stats0/1`, `.procstat0/1`, `.thermal0/1`, `.dmesg`; the perf row's `*.perf/{flat,flat-dso,lock}.txt`; the two `kab-*.log` drivers per arm |

## 2. The table (medians of the two boots per arm; per-boot rows in the artifacts)

`f3-ur µs/fio-op` = `daemon_cpu_ns_by_class[fuse3-ur]` delta ÷ the row's
fio ios (the §5 primary); `µs/transport-op` = the same over
`fuse3_zc_replies` (the READ population the worker actually reaps —
fio's ios include the ramp). Hops are exact means from the `*_ns`
sum/count deltas. `slowpath` / `_raw_spin_lock` are the perf row's flat
self-time shares on the workers (`lock.txt`).

### kern rand-4k, 24 × qd8

| row | arm | IOPS | p50 µs | p99 µs | f3-ur µs/fio-op | µs/transport-op | box busy % | msg_hop µs | device_cq µs | wake_hop µs |
|---|---|---|---|---|---|---|---|---|---|---|
| kern-1 | A | 508,886 | 194.6 | 4,227 | 23.09 | 17.44 | 75.0 | 27.8 | 98.5 | 35.2 |
| kern-1 | B | 554,835 | 174.1 | 3,670 | 18.88 | 14.26 | 72.3 | 24.8 | 91.7 | 36.1 |
| kern-1 | Δ | **+9.0 %** | **−10.5 %** | **−13.2 %** | **−18.2 %** | **−18.2 %** | −3.7 % | −10.8 % | −6.9 % | +2.5 % |
| kern-2 | A | 507,780 | 192.5 | 4,219 | 23.08 | 17.43 | 74.6 | 27.7 | 98.5 | 35.5 |
| kern-2 | B | 555,500 | 175.1 | 3,621 | 18.93 | 14.27 | 72.5 | 25.0 | 91.9 | 36.1 |
| kern-2 | Δ | **+9.4 %** | **−9.0 %** | **−14.2 %** | **−18.0 %** | **−18.1 %** | −2.8 % | −9.6 % | −6.7 % | +1.9 % |
| kern-60 (sustained) | A | 510,128 | 193.5 | 4,260 | 20.26 | 17.47 | 75.5 | 28.0 | 99.1 | 35.3 |
| kern-60 (sustained) | B | 556,494 | 177.2 | 3,555 | 16.76 | 14.43 | 73.4 | 25.3 | 92.0 | 36.6 |
| kern-60 (sustained) | Δ | **+9.1 %** | **−8.5 %** | **−16.5 %** | **−17.3 %** | **−17.4 %** | −2.8 % | −9.7 % | −7.2 % | +3.7 % |
| kern-perf (perf attached) | A | 482,830 | 174.1 | 5,407 | 22.90 | 17.11 | 69.9 | 27.4 | 97.3 | 34.4 |
| kern-perf (perf attached) | B | 498,241 | 153.6 | 5,014 | 19.63 | 14.38 | 67.1 | 25.7 | 92.9 | 34.5 |
| kern-perf (perf attached) | Δ | +3.2 % | −11.8 % | −7.3 % | −14.3 % | −16.0 % | −4.1 % | −6.2 % | −4.5 % | +0.1 % |

Per-boot spread is tight on both arms: A kern-1/2 = 505,014 / 508,006 /
509,766 / 510,545; B = 552,181 / 554,876 / 556,123 / 557,488 — the arms
do not overlap by 42k. The 60 s row is FLAT on both B boots (first/last
third 2,212/2,215 MiB/s = +0.1 %, 2,146/2,293 = +6.9 %; A: +2.4 %,
+2.0 %) — the claim is the sustained figure.

### the lock class (perf row, `f3-ur*` threads, flat self time)

| symbol | A boot 1 | A boot 2 | B boot 1 | B boot 2 |
|---|---|---|---|---|
| `native_queued_spin_lock_slowpath` | 12.56 % | 13.07 % | **0.11 %** | **0.11 %** |
| `_raw_spin_lock` | 3.28 % | 3.23 % | 2.38 % | 2.31 % |
| `fuse_request_end` | 0.53 % | — | 0.10 % | — |

The contended class is gone: 12.8 % of the worker's cycles were the
queued-spinlock slow path on `fc->bg_lock` (taken twice per uring
completion on 6.19 — `fuse_uring_req_end` + the inline finish in
`fuse_request_end`, the R-4 §2 ledger's finding); under 0031 the uring
submit and complete paths take a per-queue budget and the slow path
reads 0.11 %. The residual `_raw_spin_lock` 2.3 % is the other classes
(`fiq->lock`, the queue's own `lock`), not this one.

### the controls

| row | arm | IOPS | p50 µs | p99 µs | f3-ur µs/transport-op | box busy % | msg_hop µs | wake_hop µs |
|---|---|---|---|---|---|---|---|---|
| il rand-4k (shim; takes no FUSE locks) | A | 873,076 | 187.4 | 565 | — | 81.1 | — | — |
| il rand-4k | B | 882,153 | 188.4 | 553 | — | 81.2 | — | — |
| il rand-4k | Δ | +1.0 % | +0.5 % | −2.2 % | — | +0.1 % | — | — |
| seq 1 MiB qd16 (1) | A | 37,094 (37.1 GiB/s) | 8,307 | 38,273 | 53.60 | 84.7 | 396 | 671 |
| seq 1 MiB qd16 (1) | B | 36,883 (36.9 GiB/s) | 8,372 | 35,127 | 52.47 | 84.4 | 324 | 539 |
| seq 1 MiB qd16 (1) | Δ | −0.6 % | +0.8 % | −8.2 % | −2.1 % | −0.3 % | −18.2 % | −19.6 % |
| seq 1 MiB qd16 (2) | A | 38,984 (39.0 GiB/s) | 7,602 | 39,322 | 53.97 | 84.3 | 361 | 601 |
| seq 1 MiB qd16 (2) | B | 38,489 (38.5 GiB/s) | 8,012 | 33,620 | 53.30 | 84.1 | 302 | 496 |
| seq 1 MiB qd16 (2) | Δ | −1.3 % | +5.4 % | −14.5 % | −1.2 % | −0.3 % | −16.4 % | −17.6 % |

The il control is PAR (+1.0 %, inside the arm's own per-boot spread of
871–875k / 882–883k): the shim's data path never enters the FUSE
request path, so it cannot see `bg_lock` — a box-state drift between the
A and B boots (5-day uptime vs a fresh boot, 3 h apart) would have moved
it too, and it did not. The seq rows are par on throughput (their
per-boot spread is 36.2–39.6k on A, 36.7–40.2k on B — the medians'
−0.6/−1.3 % sit inside it) with the tails and the bridge hops improved.

## 3. Reading the verdict rule (design §5)

| clause | expected | measured (both B boots) | met |
|---|---|---|---|
| worker µs/op falls WITH the lock class | −1.5…−2 µs/op (≈ −10 % of 17.7) | **−3.1…−3.2 µs/transport-op (17.4 → 14.3, −18 %)**; slowpath 12.8 % → 0.11 % | yes — more than expected because THIS box's slowpath share was 12.8 %, not the R-4 ledger's 7.8 % (that ledger was read at a lower offered load) |
| IOPS ≥ par | par | **+9.0…+9.4 %** (30 s), **+9.1 %** sustained 60 s | yes |
| p99 ≥ par | par | **−13…−17 %** (4.2 → 3.6 ms) | yes |
| seq rows par | par | −0.6 / −1.3 % IOPS inside per-boot spread; p99 −8…−15 % | yes |
| il control par | par | +1.0 % | yes |
| no WARN / lockdep on a B boot | — | `dmesg` after every row: only the boot's firmware lines; 0 fuse/io_uring lines across both boots and 14 rows; `invariant_tripwires`, `fuse_op_watchdog_overdue`, `transport_cq_overflows`, `transport_lease_overlong` all 0 every row | yes |

Where the recovered cycles went: the worker's per-op path lost 3.1 µs,
and the bridge's `msg_hop` (handler send → worker take) fell 10 % and
`device_cq` (enter → CQE popped) 7 % — the worker reaches its ring
sooner when it is not queued on `bg_lock`; `wake_hop` is unchanged
(+2…+4 %, the run-queue term the R-3/R-4 notes named). Box-wide busy
fell 2.8–3.7 points at +9 % delivered IOPS.

## 4. What this row does not say

* **Not A-B-B-A.** Arms are kernels; the schedule is A A B B across two
  reboots three hours apart. The il control's par, the seq rows' par, the
  identical thermal/governor state (44–46 °C, `performance`) and the lock
  ledger's specificity (one symbol 12.8 % → 0.11 %, its neighbour
  3.3 → 2.3 %) are what pin the delta to the patch rather than to the
  boot. A fresh-boot-vs-5-day-uptime confound cannot be excluded by
  schedule alone; it is excluded by the control that would have moved.
* **Two shapes.** rand-4k at 24 × qd8 (the finding's venue, the R-4
  ledger) and 1 MiB seq qd16. On this daemon the §2.5 semantics change
  is structurally invisible: the INIT reply's `max_background` is
  `queues × q_depth`, so a queue's per-queue share (32) IS its ent count
  and no queue can hold more in flight than its share anyway — the only
  behavior change measured here is the locking. The semantics would
  become visible only under an operator `-o max_background` set BELOW
  `queues × q_depth` (design §2.5's "one slot per queue" floor); that
  posture is not measured.
* **Tier: measured-real** on the field box for THIS shape; the 7.2 dev-box
  canary (design §5a) is the same patch's first boot on the 7.2 track —
  no WARN, tripwires 0, but no A arm there.
* The 7.1 track's 0031 is compile-proven only; the laptop skipped 7.1.8 →
  7.2.3 in one boot, so its 7.1 build never ran.

## 5. Landing

* The patch stays in all three series (`patches/0031`, `patches-7.1/0031`,
  `patches-7.2/0026`); the "compile-proven, boot + A/B owed" status in
  `docker/kernel-sqz/README.md` / `SERIES.md` and the design doc's §5 flip
  to this row.
* `squeeze-test` runs the patched kernel from here; every field row after
  2026-09-06 19:48 UTC is on 0031 — notes comparing against pre-0031 field
  rows (the R-4 ledger, the 2026-09-05 read levers) must say so.
