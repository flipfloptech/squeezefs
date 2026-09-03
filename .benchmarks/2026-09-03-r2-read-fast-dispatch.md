# 2026-09-03 — R-2: READ fast-dispatch from the reap thread — kern rand-4k **+14.8 % IOPS (A-B-B-A), 514 k sustained**, ingress 158 → 40 µs, tail par

**Program:** [`docs/design-e2e-perf-audit.md`](../docs/design-e2e-perf-audit.md)
§3.3 Tier 2 rank 7 (read #2 — transport ingress) / §5.3 order 7
(`perf/read-fast-dispatch`, run as `perf/r2-read-fast-dispatch`); Appendix
B read ledger fat #2. **Input:** the 4 KiB-random attribution pass
[`.benchmarks/2026-09-03-4k-random-attribution.md`](2026-09-03-4k-random-attribution.md)
§4/§7.2 — per op, the kern READ paid `queue_wait` 69.3 + `dispatch_lag`
89.2 = **158 µs (36 % of a 439 µs op)** between the reap CQE and the
handler's first poll, with the device inside the op at 40 µs, plus a
`send → transport_recv` residue (≈ 56 µs mean, p50 8) that **owned the
tail** (61 % of > 3 ms ops) and had no instrument. **Branch / binary:**
`perf/r2-read-fast-dispatch` on dev `12f6d3f5`; field binary `850ce8b0`
(`release` = thin-LTO profile — same profile as the A leg; `task
build:rocky8`, `dist/rocky8/squeezefs`, glibc ≤ 2.28). Commits per step:
instrument `7e7ef84e` → bench `ddfed6b9` → red contract `417de0c3` → fix
`9da65f42` → lane-homing correction `850ce8b0` (the first field bracket's
finding, §5.1).

**Verdict up front.** The lever landed and pays at the field shape:
**kern 4 KiB randread 24×qd8 446.3 k → 512.2 k IOPS (+14.8 %, means of
both bracket orders), clat mean 426 → 370 µs (−13 %), p50 259 → 189
(−27 %), p90 709 → 545 (−23 %), p99 3.6 → 4.4 ms (+0.8 ms — the SAME
stall population, §6), p99.9 par (12.4 vs 13.2 ms), daemon CPU/op 45.7 →
39.9 µs (−13 %)**; **sustained 60 s 514.3 k flat (+4.2 % first→last
third) vs the same-binary lever-off 441.5 k (+0.6 %)** — +16.5 %. The
1 MiB sequential regression row is **at par** (36.1 vs 35.8 GiB/s, both
orders). Engagement is **exact** on every row: `transport_fast_dispatch_
demotes ≡ fuse3_read_inplace_replies` (every kern READ on this cache-less
zc shape is cold, so the SERVE arm is 0 by construction and the whole win
is the demote arm), `queue_wait` is an exact 0 on 100 % of READs (n =
35.9 M, Σ = 0 ns), `dispatch_lag` 78.7 → 40.3 µs, `transport_total` 319
→ 215 µs. The re-attribution trace (§4) reads the same per op: `transport
_recv → handler_entry` 141 → 51 µs, containment 1.00–1.03. **The first
build lost** (§5.1: −7.5 % IOPS, tail +40 %) — the node round-robin
hand-off woke a different, often parked lane per op; homing the minted
handler on the lane of the queue's CPU (one lane per queue) is what the
win rests on. **The K1 tail is attributed, not attacked** (§6): the new
`transport_reap_gap_ns` shows the worker's CQ-blind windows exceed 1 ms on
0.001–0.004 % of enters and 4 ms on ~1e-6 — the 3–10 ms reap stalls are
NOT the worker being busy or preempted mid-pass; they live in the parked
worker's wake path (the `park` tail: 1.2–1.5 % of parks > 1 ms), so the
tail campaign's instrument is `perf sched` on `f3-ur*` + the io_uring wait
wakeup, and its lever is not a dispatch change. **Tier: measured-real.**

---

## 1. Instruments, venue, tier

| | |
|---|---|
| **Instrument** | fio 3.36 (`/usr/bin/fio` on the box), the FIELD job files verbatim: `/scratch/tmp/fio_jobs/randread_iops.job` (libaio, `direct=1`, 24 jobs × `size=1g`, 4 KiB randread, iodepth 8, 30 s + 10 s ramp, `norandommap=0`) and `read_BW.job` (24 × qd16 1 MiB seq read, `size=8g`); the 60 s rows are the same job with only `runtime=` rewritten (`rows/*.job`, kept). Per-job bw logs at 1 s for the first/last-third flatness column. Kern mode only (kernel FUSE-over-io_uring; the il rows never touch the FUSE dispatch and are unchanged by R-2). |
| **Rig** | [`.benchmarks/rigs/2026-09-03-r2-read-fast-dispatch-rig.sh`](rigs/2026-09-03-r2-read-fast-dispatch-rig.sh) + [`2026-09-03-r2-row-delta.py`](rigs/2026-09-03-r2-row-delta.py) (pre/post `.stats` snapshots → exact phase means (`Δsum_ns/Δcount` — the A1 histograms), engagement closure, reap-gap family, CPU by thread class, tripwires, read-copy closure; the trace legs run the attribution pass's drain recipe: one discard at t = 15 s, ten 0.5 s captures, dentry drop before each). |
| **Substrate** | squeeze-test (client 32-core Xeon 6426Y, 251 GB, 2×200 GbE, kernel **6.19.14-sqz**) → 5 storage nodes over **nvme-tcp**, memory-backed **nullblk** targets, `cluster_reset_v4.sh` fresh at 02:33 UTC (5 meta + 10 data namespaces, cache-less format). FUSE-over-io_uring 32 queues × depth 32, `fuse3_zc_negotiated = 1`, `fuse3_kmbuf_negotiated = 1`, `--interception --allow-other`. File set: 24 × 8 GiB laid out ONCE by `write_BW.job` under the A leg (32.1 GiB/s); the rand rows read each file's first 1 GiB. Box otherwise idle (loadavg 0.00 at start, no foreign daemon). |
| **Binaries** | **A** = `/scratch/tmp/squeezefs.kvmap` (dev `7fe9fde2-dirty`, `release`, the attribution pass's binary; `SQUEEZEFS_IPC_ALLOW_DEV=1` for its `-dirty` shim pairing — no il rows ran). **B** = `squeezefs.r2b` = `850ce8b0` (`release`, clean tree). **C** = B with `SQUEEZEFS_FUSE_READ_FAST_DISPATCH=0` (the same-binary control — the inbound-queue path, byte-identical). B1 (`9da65f42`, the pre-correction build) is reported in §5.1 as the falsified first bracket. |
| **Tier** | measured-real. **Rows are 30 s + 10 s ramp unless labeled 60 s**; the 60 s rows are the sustained claim. Amplification columns N/A (reads). |
| **Artifacts** | `~/sqz-field-artifacts/2026-09-03-r2-artifacts.tgz` (every `.stats` pre/post, fio JSON, bw logs, `.row` tables, both trace legs' ten drains + merged dumps + stitch JSON, daemon logs, reset log, the rig as run). The box's `/scratch/tmp/sqz-agent/` was removed at the end; the mount was down. |

## 2. The mechanism (what landed)

The fuse3 queue worker (`fuse3-urN`) now dispatches a FUSE_READ itself at
the delivery CQE ([`crates/fuse3/src/raw/connection/fast_dispatch.rs`](../crates/fuse3/src/raw/connection/fast_dispatch.rs)):

1. **Probe inline** — `Filesystem::read_fast_probe`, the daemon's SYNC
   try-only warm ladder: the il §5.5.1 sync fast path's exact legs
   (per-inode `try_read()` — a writer holding the lock is a Demote, never
   a wait — attr/meta size coherence, EOF, single block, no device
   overlay, active-buffer snapshot, then staging ring → hot → read-lane
   hold → NVMe read-cache INTO the transport's reply window); virtual
   inodes, device-true O_DIRECT, retained extents and a window too small
   for the request demote. Page-coherence latch, lane touch and
   `fuse_ops` mirror the handler's warm serve.
2. **Served ⇒ commit inline** through the worker's own lease-gated
   `commit_ready_reply` (the served `Bytes` alias the window, so
   `apply_reply` / the zc bounce arm elide the copy): no channel, no
   wake, no lane. `queue_wait` and `dispatch_lag` are recorded as EXACT
   zeros (the phase counts keep closing against the READ population),
   `transport_recv ≡ fast_dispatch → reply_commit` in the trace ring
   (`Stage::FastDispatch` = 6; the stitch aliases it).
3. **Demote ⇒ mint the full READ handler** (`fast_read_future` — the
   extracted `read_handler_body` both venues share) and hand it straight
   to the `fuse3-tpc` lane **homed on the queue's CPU** (qid = the
   requester's CPU on queue-per-CPU sessions — one lane per queue, the
   same-lane posture the retired dispatch task had), node round-robin
   only when no lane homes there; `tpc_dispatch_boxed` re-boxes nothing.
   The inbound queue and the session dispatch task are not on the path:
   `queue_wait` is recorded 0 at the first poll, `dispatch_lag` anchors
   on the mint instant (= the arrival stamp, ONE clock read on the
   worker).

Preserved: the §5.4 lease law (READ deliveries carry no payload lease; the
inline commit runs the gate every reply runs), the commit-batch drain and
the wake coalescing (the inline commit rides the pass-bottom flush like
every SQE), the handoff-economy venue (a lane spawn, never a runtime-
handle spawn), the fused write lane (untouched). The reap thread never
blocks (pinned: a held writer lock demotes in bounded time, the lock IS
the seam). Lever `SQUEEZEFS_FUSE_READ_FAST_DISPATCH` (default on; `0` =
the A/B control), registered. Engagement pair `transport_fast_dispatch_
{serves,demotes}`; `serves + demotes ≡` the READs delivered on an armed
session with the lever on, and a served READ never takes the handler's
in-place arm (`serves + fuse3_read_inplace_replies ≡ READs`).

**Contracts** ([`tests/read_fast_dispatch_tests.rs`](../tests/read_fast_dispatch_tests.rs),
6 green incl. one live mount; fuse3 `fast_dispatch::tests` + `reap_cadence_*`):
warm active-buffer reads serve byte-exact and EOF-clamped (the probe
answers exactly what `Filesystem::read` answers); a tier-resident block
serves INTO the dest window and aliases it, a too-small window demotes;
cold demotes with zero device touches and the handler still serves; a
writer holding the inode lock demotes in < 50 ms without waiting; virtual
inodes demote; the served-op accounting (exact zeros, exact total, the
three stamps, never `dispatch`/`handler_entry`); live: a staged 1 MiB file
serves 48/48 inline with `queue_wait` Σ = 0, the 8 MiB O_DIRECT rand
shape demotes every READ exactly once with in-place replies ≡ demotes,
and the lever's `0` leaves both counters flat. Suites re-run green:
`crates/fuse3` (214), `read_serve_phase_tests`, `read_lane_tests`,
`transport_lease_overlong_tests`, `kernel_op_economy_tests`,
`ipc_op_economy_tests`, `audit_instruments_tests` (24), `op_trace_tests`,
`transport_ingress_tests`, `multi_queue_tests`,
`transport_concurrency_tests`, `env_knob_convention_tests`; clippy clean in
both workspaces (all-features + shipped). `fuse_zc_write_fusion_tests`
self-skips its zc arms unprivileged (capability class — sqz kernel +
CAP_SYS_ADMIN; the zc-capability gate is the venue).

### 2.1 The instrument landed first: `transport_reap_gap_ns`

No CQE carries a kernel completion timestamp, and under `DEFER_TASKRUN` a
completion is not even in the CQ until the ring owner enters with
GETEVENTS — so the honest per-worker measure of the K1 residue is the
**enter cadence** (`read_phase::ReapCadence`, two clock reads per enter,
never per op): `blind` = previous enter's return → this enter's call (the
window a landed completion waits out; its mean is the pass's own work plus
any preemption of the worker), `blind_cqe` = the same span weighted per
CQE the enter surfaced (one 3-RMW record; `sum/count` = the mean
per-completion reap-gap BOUND), `park` = each blocking enter's wall time.
Pinned exact in `audit_instruments_tests` and the fuse3 suite.

### 2.2 The microbench (`read_ingress`, `crates/fuse3/benches`)

One op in flight — the mechanism's floor, not the loaded row:

| arm | ns/op | what it prices |
|---|---|---|
| `hop_queue_dispatch_spawn` | **6,683** | the shipped hop: channel push → dispatch task on a lane → decode → spawn → first poll |
| `mint_direct_lane_spawn` | **4,008** | decode + mint on the reap thread → ONE lane hop → first poll |
| `inline_probe_commit_4k` | **53** | decode + snapshot-slice probe + out-header encode + 4 KiB body copy |

The dispatch-task hop is ≈ 2.7 µs of floor; the field's 158 µs was the
queueing behind it — which is what §3 shows the demote arm removing.

## 3. The A-B-B-A bracket (kern, `randread_iops.job` 24×qd8 and `read_BW.job` 24×qd16)

Order as run: A1 → B1 (`9da65f42`, §5.1) → C1 → **B2 → A2 → B3** (the
bracket of record is A1/B2/B3/A2 — both orders around the CORRECTED
binary) → the sustained pair → the trace legs. Every row: tripwires 0
(`invariant_tripwires`, `transport_lease_overlong`,
`fuse_op_watchdog_overdue`, `transport_cq_overflows`, `read_dest_overruns`,
`transport_requests_abandoned`, `fuse3_zc_bridge_cancels`); read-copy
closure: every byte in `read_zc_serve_bytes` (zero daemon passes —
`read_copy_dest_bytes` = `read_copy_bounce_bytes` = 0; the 24–31
`ranged_reads` per row are the per-file kvmap window loads, dest-leased).

### 3.1 rand-4k

| Row | Binary | IOPS | clat mean / p50 / p90 / p99 / p99.9 µs | CPU µs/op (tpc / ur s) | `queue_wait` / `dispatch_lag` / `transport_total` µs (exact) | demotes ≡ inplace | flat (⅓→⅓) |
|---|---|---|---|---|---|---|---|
| **A1** | kvmap | **448,741** | 423.9 / 264 / 750 / 3,260 / 9,241 | 45.9 (317.6 / 292.4) | 59.3 / 82.8 / 320.1 | — (0 / 18.02 M) | −5.1 % |
| **B2** | r2b | **509,157** | 372.6 / 189 / 545 / 4,555 / 14,221 | 39.9 (248.9 / 359.6) | **0.0 / 40.8 / 215.2** | 20.21 M ≡ 20.21 M | +6.8 % |
| **B3** | r2b | **515,219** | 368.3 / 189 / 545 / 4,293 / 13,959 | 39.9 (257.1 / 358.7) | **0.0 / 41.5 / 215.3** | 20.45 M ≡ 20.45 M | −5.6 % |
| **A2** | kvmap | **443,904** | 428.7 / 253 / 668 / 3,981 / 15,532 | 45.6 (321.3 / 279.4) | 61.9 / 76.2 / 311.0 | — (0 / 17.68 M) | +10.6 % |
| C1 | r2 lever 0 | 438,731 | 433.9 / 257 / 668 / 4,489 / 13,304 | 46.6 (319.8 / 287.0) | 62.6 / 78.9 / 317.0 | 0 / 17.50 M | +16.1 % |
| C2 | r2b lever 0 | 448,833 | 423.9 / 268 / 692 / 3,424 / 11,600 | 48.1 (345.6 / 295.9) | 62.9 / 79.4 / 318.7 | 0 / 17.87 M | +2.3 % |

**A mean 446.3 k / 426.3 µs → B mean 512.2 k / 370.5 µs: +14.8 % IOPS,
−13 % clat, both orders (B2 vs A1 +13.5 %, B3 vs A2 +16.1 %).** The
same-binary control (C1/C2: 438.7 k / 448.8 k) sits ON the A leg, so the
whole delta is the lever, not the tree between `7fe9fde2` and `850ce8b0`.
CPU moved, not grew: −6 µs/op total, the lanes −70 s and the workers
+70 s per row (the worker now pays the mint + lane push; the lanes lost
the dispatch task's pop/reconstruct/re-spawn).

### 3.2 1 MiB sequential (the regression row)

| Row | Binary | GiB/s | clat mean / p50 / p99 / p99.9 µs | CPU µs/op | `queue_wait` / `dispatch_lag` / `transport_total` µs |
|---|---|---|---|---|---|
| A1 | kvmap | 36.16 | 10,342 / 8,356 / 36,962 / 143,655 | 115.4 | 762 / 244 / 7,958 |
| B2 | r2b | 36.42 | 10,264 / 8,356 / 36,438 / 85,459 | 109.8 | **0** / 672 / 7,297 |
| B3 | r2b | 35.77 | 10,450 / 8,585 / 36,438 / 66,322 | 109.3 | **0** / 699 / 7,428 |
| A2 | kvmap | 35.50 | 10,527 / 8,585 / 38,535 / 95,945 | 115.4 | 730 / 240 / 7,908 |

**Par (+0.7 % GiB/s, −5 % CPU/op).** The queue_wait's 730–762 µs moved
INTO `dispatch_lag` (240 → 685 µs): on this row every queue's lane is
saturated by the 1 MiB handlers, so the hop's cost is the lane's queue
whichever side of the mint it is recorded on — `transport_total` still
fell 8 % (7,933 → 7,363 µs) because the pop + reconstruct + re-spawn is
gone. This row is device-bound (36 GiB/s ≈ the fabric); the p99.9 spread
(66–144 ms) is the row's own noise class in both legs.

### 3.3 Sustained (60 s, the claim)

| Row | IOPS | clat mean / p50 / p90 / p99 / p99.9 µs | first/last third | CPU µs/op | `dispatch_lag` / `transport_total` |
|---|---|---|---|---|---|
| **B-60s** (lever on) | **514,273** | 368.8 / 194 / 553 / 4,489 / 12,517 | 1,908 / 1,989 MiB/s (**+4.2 %**) | 35.2 | 40.3 / 215.2 |
| **C-60s** (lever off, same binary) | 441,544 | 431.1 / 261 / 684 / 3,883 / 13,173 | 1,717 / 1,728 MiB/s (+0.6 %) | 42.0 | 78.7 / 318.7 |

**+16.5 % sustained, flat, −62 µs clat, −16 % CPU/op.** n = 35.9 M
demotes ≡ 35.9 M in-place replies; `queue_wait` Σ = 0 over 35.9 M
records.

## 4. Re-attribution (the A2 trace ring, `SQUEEZEFS_OP_TRACE=1`, 1-in-10)

Same recipe as the attribution pass (mid-row 0.5 s drains, the dentry
drop before each). Rows: AT (kvmap) 442.9 k / 429.8 µs, BT (r2b)
502.1 k / 377.6 µs (the traced rows cost ≈ −1 % / −2 % vs their clean
twins). Stage transitions per op (consecutive stamps; 252,861 / 308,558
complete daemon chains):

| Stage | A (kvmap) p50 / p99 / **mean** µs | B (r2b) p50 / p99 / **mean** µs |
|---|---|---|
| `transport_recv → dispatch` (`queue_wait`) | 23.5 / 307 / **68.9** | **0 / 0 / 0** (the mint IS the arrival) |
| `dispatch → handler_entry` (`dispatch_lag`) | 39.8 / 370 / **71.9** | 14.5 / 219 / **51.0** |
| **ingress total** (arrival → handler entry) | — / — / **140.8** | — / — / **51.0** (**−90 µs**) |
| `handler_entry → keys_resolved` (prelude + meta + key) | 2.6 / — / 4.0 | 2.9 / — / 4.5 |
| `keys_resolved → block_fetched` (the zc bridge: device 40 + software) | 103.0 / 592 / 158.7 | 106.6 / 708 / 165.6 |
| `block_fetched → reply_commit` | 2.8 / — / 3.7 | 2.8 / — / 3.8 |
| **`transport_recv → reply_commit`** (`transport_total`) | 195.5 / 1,152 / **309.0** | 137.3 / 889 / **226.7** (**−82 µs**) |

Containment (trace vs the window's exact histograms): A `queue_wait`
1.03 OK, `transport_total` 1.00 OK, `dispatch_lag` 1.00 ~; B `queue_wait`
2 ns vs 0 (OK — the stitch's exact-zero arm, §7 item 2), `dispatch_lag`
1.02 ~, `transport_total` 1.01 OK. **The two ingress stages the board
named collapsed from 141 to 51 µs per op, and the whole daemon-visible
span fell 82 µs; the zc bridge (R-3's term) is unchanged at 159–166 µs,
now 73 % of the daemon-visible op.** The attribution pass expected −80 …
−130 µs from this lever; the measured −82 (trace) / −104 (exact
histograms, 319 → 215) is inside the band.

## 5. What was learned on the way

### 5.1 The first bracket LOST — the lane hand-off's wake cost (falsified, fixed)

B1 (`9da65f42`: node round-robin hand-off, double-boxed future) read
**406.1 k / 469.3 µs** against C1's 438.7 k / 433.9 — −7.5 % IOPS, mean
+35 µs, p99 4.5 → 6.2 ms, p99.9 13.3 → 17.7 ms — while `queue_wait` was
already 0 and the **p50 fell 257 → 140 µs**. The reap-gap family named
the mechanism in one read: `blind_cqe` 5.5 → 18.8 µs (the per-CQE bound
tripled) and the > 64 µs blind class ≈ 1.5 → ≈ 5 % of CQEs — the worker's
pass grew, not from the probe (53 ns) but because every demoted READ woke
a DIFFERENT lane picked round-robin from the node's 16, often a parked one
(inferred from the wake count and the fix's effect, not measured
directly): a futex from the reap thread plus a scheduler wakeup on a box running
24 fio + 32 `f3-ur` + 32 `fuse3-tpc` threads on 32 cores. The retired
dispatch task had fed ONE lane per queue (spawn_local), so a burst's
handlers queued on an already-running lane. `850ce8b0` homes the minted
handler on the lane whose core is the queue's CPU (`lane_of_cpu`) and
drops the second `Box::pin` (`LaneExec::spawn_boxed`); B2/B3/B-60s are
that build. Standing law for the next dispatch lever: **a per-op
cross-thread wake from the reap thread is the term, whichever hop it
replaces — measure `blind_cqe` before crediting a hop deletion.**

### 5.2 The SERVE arm is structurally 0 on this row

Every kern READ on a cache-less zc-armed mount is a `zc_device_fetch`
into the request's pages — nothing is ever deposited in a RAM tier
(`hot_block_hits` = `read_lane_serves` = `cache_hits` = 0 across all rows),
so `read_fast_probe` demotes 100 % and the probe's ladder is pure cost
on the reap thread here (≈ 1–2 µs: seven lock-free lookups). The win is
entirely the demote arm. The serve arm engages on warm shapes (the live
contract: a staged file serves 48/48 inline with `queue_wait` Σ = 0) and
is the il §5.5.1 fast path's kernel twin; a warm-row field bracket (a
fitting working set, or a staging-format mount) is owed before its
share is credited.

### 5.3 Where the K1 residue is NOT

`transport_reap_gap_ns`, 60 s rows: the worker's `blind` window is 3.6 µs
(lever off) / 6.0 µs (lever on) mean at 69 M / 64 M enters; **> 1 ms on
0.0011 % / 0.0037 % of enters, > 4 ms on 9e-7 / 3e-7** — and the per-CQE
bound `blind_cqe` > 1 ms on 0.013 % / 0.035 %. The > 3 ms daemon-visible
ops are 0.40–0.47 % of the population (trace, both legs), dominated in
55–65 % of cases by the zc bridge span (`keys_resolved → block_fetched`
p50 3.4–3.8 ms — a device/queue term, R-3's) and in 31–41 % by the
ingress stage. So the worker being busy or preempted mid-pass cannot be
the 3–10 ms `send → transport_recv` class; that class lives while the
worker is PARKED (`park` > 1 ms on 1.15 % / 1.53 % of parks, > 4 ms on
0.045 % / 0.062 % — an idle park and a late wake are indistinguishable
from userspace). The tail campaign's instrument is therefore `perf sched`
on `f3-ur*` (wakeup → running latency) joined to the fuse tracepoints, and
its lever is in the park/wake path (CPU oversubscription, the wait's
wake), not in dispatch.

## 6. The tail verdict (p99 / p99.9)

| | A mean (A1, A2) | C mean (C1, C2, C-60s) | B mean (B2, B3, B-60s) |
|---|---|---|---|
| p99 µs | 3,621 | 3,932 | **4,446** |
| p99.9 µs | 12,386 | 12,692 | **13,566** |
| `transport_total` > 1 ms (60 s rows) | — | 12.42 % | **3.71 %** |
| `transport_total` > 4 ms (60 s rows) | — | 0.354 % | **0.324 %** |

**p99.9 par within the rows' spread (9.2–16.2 ms across all A/C rows;
12.5–14.6 across B); p99 +0.5–0.8 ms.** The > 4 ms stall population is
the SAME size on both sides (0.32–0.35 % of ops) while the > 1 ms
population fell 3.3×: the bulk of the distribution moved left and the
99th percentile now lands nearer the unchanged stall class — a
distributional artifact of a faster bulk over an untouched tail, not a
new tail. Per the attribution pass's law the tail class (the reap
stalls) belongs to the K1 campaign (§5.3), which R-2 instrumented and
did not attack.

## 7. Instrument findings

1. **`fio --runtime` cannot override a job-section `runtime=`** (before
   or after the job file): the first "60 s" rows ran 30 s. The rig now
   derives a job file with only `runtime=` rewritten (kept in the
   artifacts). A sustained claim's `runtime` must be read from the fio
   JSON, never from the command line.
2. **The stitch crashed on an exact-zero histogram mean** (R-2's
   `queue_wait`): the containment ratio is undefined at 0; the stitch now
   passes the phase when the trace agrees within one clock quantum
   (`tests/op_trace_stitch.py`), and aliases `fast_dispatch` as
   `dispatch`/`handler_entry` for served ops.
3. **A lazy `umount -l` in the rig tore the NEXT mount down** at its own
   ready line (both at 02:51:34): the plain umount is now retried before
   the lazy detach, and the daemon exit + `mountpoint` are verified.
4. `read_serve_phase_ns.{key_resolve,meta_resolve,prelude}` containment
   is `~approx` at 1.3–5× because their histogram t0 is read inside the
   handler after the trace's stamp (the attribution pass's standing
   caveat; `total` is 1.00).
5. The dispatcher-thread ring saturation the attribution pass named (§8
   item 4) does not apply to the B leg — served/demoted ops stamp
   `transport_recv`/`dispatch` on the lane, not on one thread — and the
   B trace kept 308 k complete chains vs A's 253 k at the same divisor.

## 8. Residual board (for R-3 / the tail campaign / R-5)

- **R-3 (the zc bridge)** now owns 73 % of the daemon-visible op (166 of
  227 µs; device 40). Its first commit still owes the `dev_submit` /
  `dev_complete` stamps on the zc leg.
- **The K1 tail**: `perf sched` on `f3-ur*` + the fuse tracepoint join;
  candidate levers are in the park/wake path (the worker's park bound,
  CPU oversubscription — 24 fio + 64 daemon threads on 32 cores).
- **The demote-arm floor**: the mint still allocates the `header_and_op`
  Vec + the boxed future per op and takes the lane's mutex; on a
  cache-less zc mount the probe's seven lookups are pure cost — a
  per-mount "no RAM tier can hold kernel READ bytes" short-circuit is the
  R-5 economy item. `dispatch_lag` 40 µs is the lane's queue behind a
  burst (14.5 µs p50).
- **A warm-row bracket** for the serve arm (§5.2).

## Landing-law checklist

A-B-B-A — yes (rand-4k and 1 MiB seq, both orders, plus the same-binary
lever control on both sides of the bracket); substrates — the field
fabric (nvme-tcp, nullblk, cache-less; the two-substrate rule's tcp
venue); sustained — **yes**, 60 s flat (+4.2 % first→last third) on the
claim row and the control; amplification — N/A (reads); engagement —
exact on every row (`demotes ≡ inplace_replies`, `queue_wait` Σ = 0);
`read_copy_*` closure — every byte in `read_zc_serve_bytes`, dest/bounce
0; instrument + substrate + binary + profile + kernel + tier stated;
tripwires 0 on every row; the box returned unmounted with
`/scratch/tmp/sqz-agent/` removed.
