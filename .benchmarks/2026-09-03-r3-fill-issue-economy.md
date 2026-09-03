# 2026-09-03 — R-3: fill-issue economy — the zc bridge's software half named, the READ fused onto the queue worker, the NvmeBlockDev funnel's arrival wake

**Program:** [`docs/design-e2e-perf-audit.md`](../docs/design-e2e-perf-audit.md)
§3.3 Tier 2 rank 8 (read board #3 "fill-issue economy"), Appendix B #3,
§5 ladder row 8 (`perf/fill-poll-cohort`, run as `perf/r3-fill-issue-economy`).
**Input:** [`.benchmarks/2026-09-03-4k-random-attribution.md`](2026-09-03-4k-random-attribution.md)
— K5 `keys_resolved → block_fetched` = 165.7 µs mean of which the device
is 40 (§4), "the zc bridge's software half is NOT named on the board" (§7.1),
instrument gap 2 (§8: no `dev_submit`/`dev_complete` on the zc leg); and
[`.benchmarks/2026-09-02-r1-device-read-executor.md`](2026-09-02-r1-device-read-executor.md)
§4.3 (the stock-kernel posture's `dev_queue` = 500 µs; the executor mutex
class adjudicated NOT fat — not re-chased here). **Branch:**
`perf/r3-fill-issue-economy` from dev `12f6d3f5`; commits `b5bd982f`
(instrument), `d7fad72a` (microbench), `83ee6664` (funnel), `fb105d17`
(READ fusion + worker economy), `adc462ae` (rigs). Field binary = the
**rocky8 `task build` of `fb105d17`** (clean, `release` = thin-LTO profile;
daemon + shim one build, KD-7). Every mean below is an A1 exact
`Δsum_ns / Δcount`; every per-op number is an A2 trace join.

**Composition addendum (§9, 2026-09-03 04:27–05:05 UTC):** R-2
(`perf/read-fast-dispatch`, dev `197eb9d9`) landed on dev while this
branch was in flight and re-shaped the same delivery CQE. Rebased and
composed into ONE dispatch law — (a) warm: served inline (R-2); (b) cold
zc under the ceiling: fused onto the queue worker (R-3); (c) else: the
lane homed on the queue's CPU (R-2) — and re-measured against R-2 alone
on the field: **the two levers do NOT compound.** With fusion on the
composed binary read **481 k vs R-2's 514 k (−6.5 %, p50 +70 µs, p99
−66 %)**; the traced legs put the loss where the gain went — the fused
pass's run-queue waits (`dispatch_lag` 47 → 92, `wake_hop` ≈ 100) replace
R-2's lane hop and the bridge's two worker hops at a net +15 µs. Both
levers converge on the reap thread's per-op serialization from two
sides. **The READ fusion lever therefore ships default OFF** (arm (c) for
demoted READs); the composed default binary re-measured at **507–510 k vs
503–507 k for R-2 alone (par, +0.9 %)**, seq 1 MiB and il par, and the
bridge instrument reads R-2's shape per op: `msg_hop` 28 / `device_cq`
99 (device 40) / `wake_hop` 36 — the attribution's ≈ 126 µs, now a
measurement. Everything below §9 is the standalone campaign as run
(fusion vs the pre-R-2 dispatch), kept as the record.

**Verdict up front.** The instrument decomposed the bridge into four
hops (`zc_bridge_phase_ns`: `msg_hop` / `sq_wait` / `device_cq` /
`wake_hop`, a partition of `total` to the ns) and said what the ≈ 126 µs
was: **three cross-thread wakes around one DMA** — handler lane → queue
worker (`msg_hop` ≈ 32–35 µs local), CQE → worker pop (the reap share of
`device_cq`, ≈ 30), worker → handler lane (`wake_hop` ≈ 42) — plus the
COMMIT message back. They are scheduler-latency terms (one cross-thread
oneshot hop is 4.2 µs at idle on the microbench and 35–47 µs at the
field's CPU load), so the lever is HOP REMOVAL, not batching: armed READs
now run on the queue worker's fused lane (the D16 write-fusion machinery,
whose module doc names this exact term), and the funnel half of the board
item — the NvmeBlockDev worker parking on `submit_and_wait(1)` so a new
request waited for an UNRELATED completion — got its arrival wake put
INTO the ring (red-first: a fast fill beside a 400 ms slow fill took
378.7 ms; now device speed). **Field A-B-B-A (squeeze-test, 24×8 rand-4k):
kern 441.3 k / 445.0 k → 509.6 k / 509.7 k IOPS (+15.5 %, both orders),
clat 431 → 372 µs, p99 2.93–4.11 → 1.37 ms, daemon CPU/op −27 %; il par
(864–891 k vs 843–877 k); 1 MiB seq par (35.5–37.3 vs 36.7–37.7 GiB/s,
fusions = 0 by the ceiling); sustained 60 s: kern 510.4 k flat (+6.6 %
first/last third) vs 440.1 k, il 879 k vs 876 k.** The bridge's own
software half went 126 → ≈ 111 µs (the device is 40; `msg_hop` 0.8 +
`sq_wait` 0.4 + reap ≈ 10 + `wake_hop` 101) — but the fusion ALSO deleted
K2/K3 (158 → 83 µs) and the handler-lane class: the daemon-visible span
fell 336 → 247 µs. **What remains is ONE term: the fused pass's run-queue
wait** (`queue_wait` 83 + `wake_hop` 101 = 184 of 247 µs), i.e. every
READ's two polls each wait behind the burst ahead of them on the worker
(poll + enter + the kernel's 9–10 µs COMMIT work per task). The il reap
term (≈ 45 µs) is confirmed and named (§6), not landed.

---

## 1. Instruments, venues, tier

| | |
|---|---|
| **Instrument** | fio 3.36 (field, the attribution note's jobs verbatim: `randread_iops.job` 24 × `size=1g` 4 KiB qd8 libaio `direct=1`, 30 s + 10 s ramp; `read_BW.job` 1 MiB qd16; the 60 s legs run a `sed`'ed copy with `runtime=60` — the job file's `runtime=` wins over `--runtime` on either side of it, which is why the first sustained pass came back 30 s) / fio 3.42 (local, the R-1 rig's shape: 24 × 256 MiB, `--ramp_time=3`). il mode = `LD_PRELOAD=libsqueezefs_il.so` of the SAME build; `SQUEEZEFS_IPC_ALLOW_DEV=1` on both ends (arm A's box build is `-dirty`). Per row: pre/post `.stats` (exact histograms), fio JSON, 1 s bw logs; `SQUEEZEFS_OP_TRACE=1` on one leg per arm with a mid-row `.trace` drain (dentry drop first — §8 item 1 of the attribution note). |
| **Rigs** | `.benchmarks/rigs/2026-09-03-r3-fill-issue-economy-rig.sh` (+ `2026-09-03-r3-row-delta.py`), `2026-09-03-r3-local-abba.sh`, `2026-09-03-r3-field-abba.sh`, `2026-09-03-r3-field-sustained.sh`. |
| **Local venue** | **tcp devsub** (default instance: 4 × 1 GiB null_blk meta `/dev/nvme1-4n1` + 4 × 8 GiB zram data `/dev/nvme5-8n1` over nvmet-tcp on 127.0.0.1 — the mandatory venue for multi-connection rows), cacheless format, `mount --daemon --interception --allow-other`; kernel **7.1.8-cachyos-lto with FUSE_URING_ZERO_COPY negotiated** (`fuse3_zc_negotiated = 1`, `fuse3_kmbuf_negotiated = 1`, 32 queues × depth 32, `transport_max_write` 1 MiB, `data_read_lanes` 8). 32-core AMD (Strix Halo), **SHARED dev box** — foreign load 5–25 throughout (recorded per row); local absolute numbers are noise-limited, the ratios and the exact means are the evidence. Device controls (nvme tracepoints, this venue, mid-row): kern 306 k cmds/s p50 19.6 / mean 192 µs (p90 691); il 249 k cmds/s p50 28.3 / mean 330 µs — the local nvmet-tcp target is tail-heavy under this CPU load, so local `device_cq` cannot be split cleanly; the field can. |
| **Field venue** | squeeze-test (32-thread Xeon 6426Y, 251 GB, 2×200 GbE, **6.19.14-sqz**) → 5 storage nodes over nvme-tcp, memory-backed nullblk targets, `cluster_reset_v4.sh` fresh (5 meta + 10 data namespaces, cache-less format), one `write_BW.job` pass (arm A) minting 24 × 8 GiB; FUSE-over-io_uring 32 × 32, zc + kmbuf negotiated, `data_read_lanes = 4`, `transport_max_write` 1 MiB (fusion ceiling = 128 KiB). Box otherwise idle (loadavg 0.0 at start; R-2's agent had finished and removed its files; no other daemon). Window 03:13–03:36 UTC. |
| **Arms** | **A** = the box's attribution binary `/scratch/tmp/squeezefs.kvmap` + `libsqueezefs_il.so.kvmap` (dev `7fe9fde2` `-dirty` — the attribution note's binary, so the A rows ARE the note's rows re-run same-day). **B** = `fb105d17` rocky8 (instrument + funnel wake + READ fusion default on + worker economy). Different BINARIES, so the lever is the arm (the local A-B-B-A is the same-binary knob bracket: `SQUEEZEFS_FUSE_ZC_READ_FUSION=0` vs default). |
| **Tier** | measured-real (both venues). |

**Artifacts:** `~/sqz-field-artifacts/2026-09-03/r3-field-artifacts.tgz` (every field row's stats pair, fio JSON, bw logs, both `.trace` drains, the driver logs) and `r3-local-artifacts.tgz` (local rows, `perf sched timehist`, the nvme/syscall captures, the microbench table). The box's `/scratch/tmp/sqz-agent/` was removed at the end; the box returned unmounted.

---

## 2. Step 1 — the instrument: `zc_bridge_phase_ns` + the per-op stamps (`b5bd982f`)

The zc direct leg (`FuseConnection::zc_device_fetch` → `WorkerMsg::ZcFetch`
→ the queue worker's `READ_FIXED(device → slot)` → CQE → oneshot →
handler) rides the fuse3 queue ring, not the `NvmeBlockDev` funnel, so the
board's `dev_queue`/`dev_service` never existed on it. Now, exact-sum,
always-on, fetch leg only, ONE clock read per boundary shared with the
op-trace stamp on that boundary:

| Phase | Boundary (stage) | What it holds |
|---|---|---|
| `msg_hop` | handler send (`bridge_sent`) → worker take (`bridge_taken`) | channel + eventfd + the worker's park/pass queueing |
| `sq_wait` | take → the `io_uring_enter` that carries the SQE (`dev_submit`) | the pass remainder before the flush |
| `device_cq` | enter → CQE popped (`dev_complete`) | device + fabric + the reap latency of a parked/busy worker |
| `wake_hop` | popped (oneshot sent) → the handler resumed | the lane hop (classic) / the run-queue wait (fused) |
| `total` | send → resumed | `≡ msg_hop + sq_wait + device_cq + wake_hop` to the ns (pinned live) |

Mechanics: `ZcFetchMsg` carries the op's trace id + send stamp;
`SubmitBatch` keeps the pushed-not-flushed bridges and stamps them at the
flush that carries them (any of the three enter sites), settling
`submitted_ns` into the member's `BridgeDeadlines` before the drain reads
it; the pop stamp is read ONCE per CQ drain and travels back in the
oneshot (`BridgeCqe { res, popped_ns }`). Zero hot-path allocation. New
stages `bridge_sent = 39` / `bridge_taken = 40` (the funnel block; the
stitch tool gained the five spans, `FAMILY_CLASS` read). Contracts:
`tests/audit_instruments_tests.rs::zc_bridge_family_exports_five_exact_phases_and_its_stages_are_in_the_table`
(export law, shard fold, stage table), the fork-local
`read_phase::zc_bridge_family_is_phase_exact_and_folds_across_threads`,
and the new capability-class live suite `tests/zc_bridge_phase_tests.rs`
(256 cold 4 KiB O_DIRECT reads on a zc-armed mount: `read_zc_serve_bytes`
≡ reads × 4 KiB, one sample per phase per fetch, `Σ hops ≡ total` exact,
every traced chain `keys_resolved → bridge_sent → bridge_taken →
dev_submit → dev_complete → block_fetched → read_validated` ordered; named
in `tests/run_zc_capability_gate.sh`).

### 2.1 The decomposition (local tcp devsub, kern rand-4k 24×8, release, classic dispatch)

| Row | IOPS | clat mean / p50 / p99 | `msg_hop` | `sq_wait` | `device_cq` | `wake_hop` | `total` (= `block_fetch`) | device (nvme tp, mean / p50) |
|---|---|---|---|---|---|---|---|---|
| A0 traced (b5bd982f) | 342.1 k | 558 / 253 / 3,129 | **35.7** | 1.0 | **223.8** | **46.8** | 307.4 | 192 / 19.6 (at 306 k) |
| A1 (fb105d17, fusion off) | 376.6 k | 506 / 216 / 2,933 | 31.9 | 1.0 | 202.0 | 41.6 | 276.4 | — |
| A4 (fusion off) | 360.1 k | 530 / 245 / 2,933 | 34.7 | 1.1 | 214.6 | 42.1 | 292.5 | — |

Trace p50s (A0, 16 k chains): `msg_hop` 4.9, `sq_wait` 0.9, `device_cq`
32.4 (device p50 19.6 ⇒ reap ≈ 12), `wake_hop` 12.4 — **every hop's mean
is 3–7× its p50**: the software is three thread hops, tail-dominated by
scheduler latency. `perf sched timehist` on the same row (2 s, 64 % box
utilization): f3-ur workers 608 k switches/s (**1.74 parks per op per
worker**), mean sched delay 10.7 µs with 1,739 wakeups > 1 ms; fuse3-tpc
lanes 0.73 switches/op, 8.8 µs. `perf trace` on one worker: 1.19
`io_uring_enter` + **1.77 eventfd `read`** (1.18 EAGAIN) + 0.90 `futex`
per op. The 3.5 stripe lock, allocation and the executor class are absent
from the timeline (R-1 stands).

---

## 3. Step 2 — the microbench (`d7fad72a`, `benches/read_path_bench.rs` `fill_issue_economy`)

This box, foreign load present, Criterion medians:

| Row | ns / op | Reading |
|---|---|---|
| `enter_per_cqe_{1,8,32}` | 166 / 158 / 158 | N × `submit_and_wait(1)` on a real ring (NOP SQEs) — the retired per-completion park |
| `poll_drain_batch_{1,8,32}` | 157 / **57** / **44** | one `submit_and_wait(N)` + one drain: 2.8× at the field's per-queue depth (8), 3.6× at 32 |
| `oneshot_cross_thread_hop` | **4,231** | a `sqz_channel::oneshot` resolution awaited on a PARKED foreign thread (futex wake + schedule + resume) — the mechanism floor of ONE bridge hop at idle |
| `oneshot_same_thread_hop` | **127** | the same resolution consumed on the sending thread (the fused venue): 33× cheaper |

The field pays the cross-thread hop three times per zc READ at 35–47 µs
each (the scheduler's price at load, not the mechanism's).

---

## 4. Step 3/4 — the funnel half: the arrival wake IN the ring (`83ee6664`)

Read board #3's named term. The `NvmeBlockDev` lane worker pumped its
channel, then parked in `submit_and_wait(1)`; a request arriving while
ANY fill was in flight sat in the channel until an UNRELATED completion
woke the worker — `dev_queue` ≈ one device RTT under load (500 µs on the
stock-kernel row, R-1 §4.3; ≈ 1.37 ms in Appendix B). On the sqz kernel
neither field mode reaches this worker for rand-4k (kern → zc leg, il →
direct-drive), so its field face is the transform / unaligned /
stock-kernel postures and the kvmap window loads; it is the board's
letter, fixed as written, io_uring intact.

**Fix:** each lane owns an eventfd (`LaneWake`) the enqueue side writes
through the L3 `WakeCoalescer` (publish → arm → write-if-clear); the
worker keeps ONE `READ(eventfd)` SQE armed at every park, so
`submit_and_wait(1)` returns on the first of {a device CQE, a new
request}. The pump never blocks on the channel; the pass-bottom enter
flushes the pass's SQEs AND parks; the drain resolves every ready
completion (one wake batch) and re-arms the eventfd read; the worker
consumes the wake CQE → `disarm()` → scans (the fuse3 worker's
loom-verified drain → disarm → scan order). SQ-full flushes with a plain
`submit()` (the pass-bottom GETEVENTS enter reaps). Teardown writes the fd
unconditionally after the sender drops. Instruments: `dev_enters` /
`dev_fills` (enters ÷ fills ≪ 1 under concurrency), `dev_wake_batches`,
`dev_wake_writes` / `dev_wakes_elided`. Seam `set_test_read_slow(count,
ms)`: a linked `TIMEOUT(ETIME_SUCCESS)` ahead of the read's SQE makes the
DEVICE completion late while the worker stays free.

**Red-first (`tests/nvme_fill_issue_economy_tests.rs`, against the old worker):**

| Contract | RED (old worker) | GREEN (fix) |
|---|---|---|
| a fast fill issued beside a 400 ms slow fill completes at device speed | fast read took **378.7 ms** — it waited for the slow fill's CQE to be ISSUED | < 150 ms bound met (device speed) |
| a 64-fill concurrent burst rides fewer enters than fills; `dev_wake_batches` ∈ [1, fills] | `dev_wake_batches` 0 (no instrument) | enters < fills, batches counted |
| an idle lane wakes on its first request through the ring; teardown from the parked state joins promptly | no ring wake existed | `dev_wake_writes` moves; join < 5 s |

Device-lane suites green: nvme_dev, nvme_dest_ownership, nvme_zero_len_dma,
data_device_flush, data_device_power_cut, fsync_single_barrier,
dismount_teardown, backpressure (fixed-file + registered-buffer laws
unchanged — the SQE shapes are untouched; zcrx `gather ≡ fill` unchanged
— zcrx_lane 70/70, zcrx_steering 20/20).

---

## 5. Step 4 — the zc-leg half: the READ fused onto the queue worker (`fb105d17`)

The named term is hop latency, so the lever is the D16 fused lane, whose
module doc describes this term verbatim for writes ("two cross-thread
wakes and two schedules per 4 KiB op"). `FusedReadDispatch` +
`set_fused_read_dispatcher` (pool / conn / session); `handle_read`'s
spawned body factored into `read_handler_body` (the write twin — both
venues run the SAME body); at delivery an armed READ whose
`fuse_read_in.size` sits at or under the fusion ceiling
(`SQUEEZEFS_FUSE_ZC_FUSION_MAX`, payload/8 = 128 KiB at 1 MiB — the write
lane's bound: a WARM serve copies `size` bytes on the worker, and the
non-blocking-drain law forbids payload-scale memcpys there) mints onto
the worker's fused lane. Same pass: poll → `ZcFetch` msg pumped → SQE →
eager flush; the mid-pass reap resumes it; its prefilled COMMIT is pumped
in that pass. Lever `SQUEEZEFS_FUSE_ZC_READ_FUSION` (default **on**; `0` =
the classic dispatch, the A/B control); engagement `fuse3_zc_read_fusions`
≡ zc READs, `fuse3_zc_read_fusion_demotions` ≈ 0. Worker economy in the
same commit: a fused task's own wake (a bridge CQE resolved at the
worker's pass bottom) no longer writes the eventfd (`CURRENT_FUSED_LANE`
thread-local vs the lane id), admission never wakes, and the pass-top
eventfd drain runs only when the coalescer was armed or the wake-poll CQE
was seen (`WakeCoalescer::is_armed` peek; `disarm`'s RMW and the drain →
disarm → scan order unchanged — a write the peek misses stays in the
counter, the level-triggered PollAdd completes at the park, the next pass
drains).

### 5.1 Local A-B-B-A (tcp devsub, kern rand-4k 24×8, 30 s + 3 s ramp, same binary, lever via env, foreign load 5–7)

| Arm | IOPS | clat mean / p50 / p99 / p99.9 µs | CPU µs/op (class) | `msg_hop` | `sq_wait` | `device_cq` | `wake_hop` | `total` | `queue_wait` + `dispatch_lag` | `transport_total` | wake writes/op | midpass reaps |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| A1 off | 376.6 k | 506 / 216 / 2,933 / 4,293 | 32.9 (ur 50 %, tpc 49 %) | 31.9 | 1.0 | 202.0 | 41.6 | 276.4 | 38.7 + 47.3 | 372.7 | 1.57 | 0 % |
| **B2 on** | **445.5 k** | **428** / 198 / 2,343 / 3,523 | **23.3** (ur 100 %) | **0.6** | **0.2** | **108.0** | 84.5 | **193.2** | **74.7 + 0.4** | **276.8** | **0.62** | 92.5 % |
| **B3 on** | **420.4 k** | 453 / 212 / 2,441 / 3,654 | 24.9 | 0.6 | 0.2 | 118.0 | 88.4 | 207.2 | — | — | 0.62 | — |
| A4 off | 360.1 k | 530 / 245 / 2,933 / 4,358 | 35.7 | 34.7 | 1.1 | 214.6 | 42.1 | 292.5 | — | — | 1.54 | 0 % |

**+17.5 % / +16.8 % IOPS (both orders), CPU/op −29 %, `transport_total`
−96 µs.** `fuse3_zc_read_fusions` ≡ `fuse3_zc_replies` − 1 (the `.stats`
read), demotions 0. seq 1 MiB qd8: B 14.09 / 13.82 vs A 14.41 / 12.13
GiB/s — par within the loaded box's noise (A3 collapsed −27 % first/last
third under foreign load), `fuse3_zc_read_fusions` = 0 (the ceiling).

### 5.2 Field A-B-B-A (squeeze-test, 6.19.14-sqz, 24×8, 30 s + 10 s ramp; A = `7fe9fde2` kvmap, B = `fb105d17`)

| Row | A1 | B1 | B2 | A2 | Δ B vs A |
|---|---|---|---|---|---|
| **kern rand-4k** IOPS | 441,343 | **509,562** | **509,652** (traced) | 444,995 (traced) | **+15.5 % / +14.5 %** |
| kern clat mean / p50 / p99 / p99.9 µs | 431 / 235 / 2,933 / 16,319 | **372** / 255 / **1,368** / 11,600 | 372 / 259 / 1,384 / 9,503 | 428 / 259 / 4,112 / 13,042 | −14 % mean, **−53…−67 % p99** |
| kern daemon CPU µs/op (class) | 44.8 (tpc 53 %, ur 46 %) | **33.2** (ur 100 %) | 34.1 | 45.8 | **−27 %** |
| kern `transport_total` / `queue_wait` / `dispatch_lag` | — | 234.3 / 74.0 / 0.6 | 233.2 / 72.7 / 0.6 | 311.0 / 61.2 / 76.4 | −77 µs |
| kern `zc_bridge_phase_ns` msg / sq / device_cq / wake / total | (no family on A) | 0.6 / 0.3 / **49.7** / **96.5** / 147.2 | 0.6 / 0.3 / 49.5 / 97.1 / 147.6 | `block_fetch` 161.6 | bridge −14 µs; K2+K3 −62 µs |
| kern `transport_wake_writes`/op · midpass reaps | 1.56 · 0 % | **0.48 · 93.0 %** | 0.48 · 93.1 % | 1.51 · 0 % | |
| **il rand-4k** IOPS · clat · `device_cq` | 863,921 · 221.3 · — | 876,941 · 218.0 · 92.9 | 843,173 · 226.8 · 82.3 | 891,464 · 214.5 · 93.6 | **par** (+1.5 % / −5.4 %) |
| il inline reaps / direct ops | 7.6 % | 7.7 % | 5.9 % | 7.9 % | par |
| **seq 1 MiB qd16** GiB/s | 35.54 | 37.70 | 36.70 | 37.31 | **par** (+3 %, fusions = 0) |

Engagement exact on every row (§1 of the attribution note's law): B kern
`fuse3_zc_read_fusions` ≡ `fuse3_zc_replies` ≡ `fuse3_read_inplace_replies`
(20,243,087 / 20,243,086 / 20,243,087 on B1 incl. ramp), demotions 0;
`read_zc_serve_bytes` = ops × 4 KiB (every byte, zero daemon passes,
`read_copy_dest_bytes` 0); il `read_dest_dma_bytes` + `ipc_arena_copy_bytes`
= `ipc_bytes_out` (B-il-60: 217.39 + 34.07 = 251.46 vs 251.49 GB),
`read_copy_bounce_bytes` 0. Tripwires 0 throughout (`invariant_tripwires`,
`transport_lease_overlong`, `fuse_op_watchdog_overdue`,
`transport_cq_overflows`, `read_dest_overruns`, `ipc_direct_reap_stalls`).

### 5.3 Sustained 60 s legs (the landing law; real 60 s + 10 s ramp, B then A)

| Row | IOPS | clat mean / p50 / p99 / p99.9 | first/last third | CPU µs/op |
|---|---|---|---|---|
| **B kern 60 s** | **510,393** | **371.5** / 255 / **1,352** / 11,076 | 1,974 / 2,104 MiB/s (**+6.6 %, flat**) | 29.3 |
| A kern 60 s | 440,133 | 432.5 / 255 / 4,014 / 16,319 | 1,623 / 1,689 (+4.1 %) | 40.0 |
| B il 60 s | 879,251 | 217.4 / 185 / 553 / 2,277 | 3,443 / 3,297 (−4.2 %) | 20.9 |
| A il 60 s | 876,187 | 218.2 / 187 / 545 / 2,310 | 3,425 / 3,252 (−5.0 %) | 20.9 |

The 30 s bracket holds sustained: **+16.0 % kern IOPS, −14 % clat, −66 %
p99, −27 % CPU/op; il par.**

### 5.4 The field per-op chain, fused (B2 traced leg — 37.7 k complete chains of 20.3 M ops; divisor 10; containment 1.02–1.06 on the exact spans)

| Stage | p50 | p99 | mean | vs the attribution note's A |
|---|---|---|---|---|
| `send → transport_recv` (K1, kernel side; by subtraction 372 − 247 ≈ 125 incl. K7/K8) | — | — | ≈ 125 | was ≈ 103 (56 + 22 + 25): **+22 µs** — the fused worker is busier, so delivery CQEs wait longer to be reaped |
| `transport_recv → dispatch` = the FIRST fused poll (run-queue wait) | 37.1 | 401 | **82.7** | K2 69.3 + K3 89.2 = 158.5 → 83.6 (**−75**) |
| `dispatch → handler_entry` | 0.6 | 8.8 | 0.9 | |
| `handler_entry → keys_resolved` (prelude + meta + keys) | ~3 | ~26 | 5.2 | 3.6 |
| `keys_resolved → bridge_sent` | 2.6 | 14.0 | 3.4 | (inside K5) |
| `bridge_sent → bridge_taken` (`msg_hop`) | **0.52** | 5.9 | **0.77** | ≈ 32–35 locally on the classic dispatch |
| `bridge_taken → dev_submit` (`sq_wait`) | 0.25 | 4.3 | 0.38 | 1.0 |
| `dev_submit → dev_complete` (`device_cq`) | 38.6 | 171 | **50.0** | device 40.0 (§3 of the note) ⇒ reap ≈ 10 (was ≈ 30) |
| `dev_complete → block_fetched` (`wake_hop` = the RESUME poll's run-queue wait) | 62.3 | 444 | **101.4** | ≈ 42 locally as a thread hop |
| `block_fetched → reply_commit` | ~2.5 | ~18 | 3.2 | 8.5 |
| **`transport_recv → reply_commit`** | **172.5** | 824 | **247.0** | **336.3 → 247 (−89 µs, −26 %)** |
| fio clat | 259 | 1,384 | **372** | 439 → 372 (**−67**) |

**Where the 192 in-flight ops sit now (Little, kern B):** fused run-queue
wait ≈ 94 (83 + 101 of 372), kernel pre-daemon reap ≈ 30–35, device ≈ 20,
prelude/glue ≈ 6. The three thread hops are gone; the pass IS the queue.

---

## 6. The il reap term — verdict (lever (c), not landed)

Field il direct-drive (both arms, every row): `device_cq` 92.8–94.2 µs
against a 50.8 µs device (§3 of the attribution note) ⇒ **reap ≈ 42–44
µs**, unchanged by R-3 (il rides the direct-drive engine, not the fuse3
worker or the NvmeBlockDev lane). `ipc_direct_inline_reaps` = **7.6–7.9 %
of direct ops** on the field (28.8 % locally, where the svc passes are
denser) — the per-shard **reaper thread is the CQ consumer for 92 % of
completions**, and each completion pays that thread's wake latency (the
same scheduler class as the kern hops: ≈ 40 µs at this load). Mechanism
(`src/ipc_direct.rs`): the reaper parks in `submit_with_args(1, 100 ms)`
and wakes on the FIRST CQE; the fusion arm (`flush` → `try_lock(cq_gate)`
→ `drain_cq_locked`) only finds a CQE if one landed between the reaper's
wakes, so at load the reaper wins the race almost always. **Lever (the
read fusion's il twin, for the shim campaign's r4):** make the svc thread
the primary consumer — the reaper's wait `want` scaled to the shard's
in-flight (`max(1, inflight/2)`, bounded ≈ 2× device RTT) so at load the
owning svc thread's next flush pass drains the CQ (its passes are ~10–20
µs apart at 66 % busy) and the reaper wakes only for stragglers; at qd1
`want = 1` keeps today's latency. Expected −20…−30 µs of the 218 µs il
clat (the note's §7.3 row 2). Not built here: the field il shape is at
par and the campaign's budget went to the kern term.

---

## 7. What is landed, what is left

**Landed:** the zc-leg instrument (`zc_bridge_phase_ns` + `bridge_sent`/
`bridge_taken` + `dev_submit`/`dev_complete` on the queue worker; the
capability suite); the `fill_issue_economy` microbench rows; the
NvmeBlockDev arrival wake + `dev_*` counters + the slow-read seam (red →
green); READ fusion (`SQUEEZEFS_FUSE_ZC_READ_FUSION`, default on) +
`fuse3_zc_read_fusions`/`_demotions`; the worker's eventfd-drain and
self-wake economy; the rigs; `docs/operations.md` knob paragraph.

**The next term, named by this instrument:** the fused pass's run-queue
wait — `queue_wait` 83 + `wake_hop` 101 µs of the 247 µs daemon-visible
span. Per task the pass pays a poll (2–4 µs) + an eager
`flush_submit_getevents` (≈ 1.5 µs syscall + the kernel's COMMIT work:
`commit_flush` 9–10 µs mean — `fuse_uring_commit_fetch`'s request end +
app wake + the next delivery's page registration) — the ~6–12 ready tasks
of a burst serialize at ≈ 5–15 µs each, so a task waits half a pass. The
candidates, for R-2/R-5's board: (a) batch the COMMIT flushes per pass
(one enter, the kernel work stays per commit — a latency trade against
the committed op); (b) the kernel-side COMMIT cost itself (sqz-kernel
patch territory); (c) the worker's per-op kernel work by class (`perf` on
`f3-ur*` in the B arm — the 33 µs/op is now the whole daemon and mostly
inside `io_uring_enter`); (d) the `.trace` dispatcher-ring drop
(16.9 M dropped in the B2 window: the fused worker stamps EVERY stage of
its queue's ops on one ring — role-aware ring depth, gap 4 of the
attribution note, now sharper). K1 (`send → transport_recv`) grew ≈ 22 µs
for the same reason and stays the tail's owner.

## 9. The composition with R-2 (rebased onto dev `2b011d91`)

### 9.1 The composed READ dispatch law (`fast_dispatch.rs` module doc; the delivery arm's comment)

A fetched READ on the reap thread takes exactly ONE arm, in this order:

| arm | condition | venue | counter |
|---|---|---|---|
| **(a) served inline** | the filesystem's SYNC try-only probe finds the bytes warm (R-2) | committed on the reap thread through the lease-gated `commit_ready_reply`; `queue_wait == dispatch_lag == 0` | `transport_fast_dispatch_serves` |
| **(b) fused** | zc-armed session ∧ `fuse_read_in.size ≤` the fusion ceiling ∧ `SQUEEZEFS_FUSE_ZC_READ_FUSION=1` (R-3) | the SAME `ReadFastDispatch::mint` future, polled on the queue worker's fused lane — fetch message, CQE resume and prefilled COMMIT same-thread | `fuse3_zc_read_fusions` (+ `_demotions` on a capacity refusal, which then takes (c)) |
| **(c) handed to a lane** | everything else (R-2's lane-homing law) | the `fuse3-tpc` lane homed on the queue's CPU; inbound queue + session dispatcher skipped | `transport_fast_dispatch_demotes` |

**Partition law (pinned):** `serves + zc_read_fusions + demotes ≡` the
READs delivered on an armed session with R-2's lever on, and
`zc_read_fusions + demotes ≡ fuse3_read_inplace_replies` — pinned by
`tests/read_fast_dispatch_tests.rs` (R-2's live contract, now reading the
composed partition; unprivileged zc never arms so fusions = 0) and the
capability suite's `composed_dispatch_law_partitions_every_read_on_a_zc_session`
(both arms zc-armed, both lever values: serves 0, fusions + demotes ≡
in-place replies, fusions ≥ the data reads only when on, mid-pass bridge
reaps > 0 only when fused). The reap thread never blocks on any arm (the
probe is try-only, a fused future parks in the slab, the hand-off is a
queue push). The instruments coexist without a double-counted stage:
`transport_reap_gap_ns` brackets the worker's enters; `read_transport_
phase_ns` records `queue_wait ≡ 0` on (b)/(c) with `dispatch_lag` =
arrival → first handler poll (the run-queue wait on (b), the lane hop on
(c)); `zc_bridge_phase_ns` splits the handler's `block_fetch` on the zc
leg (`Σ hops ≡ total`; stages 39/40 vs R-2's `fast_dispatch` = 6). R-3's
separate `FusedReadDispatch`/`fused_read_future` were deleted — one READ
mint serves both handler venues (`read_handler_body`, R-2's extraction).
The funnel contract was made deterministic in the same step
(`dev_submit_enters`, the issue-side batching face: ≤ 4 submit enters for
64 fills queued behind the read-stall seam; the total enter count follows
the device's completion clustering and was not a law).

Suites on the composed tree: fuse3 215/215; `read_fast_dispatch_tests`
6/6 (also as root under zc), `zc_bridge_phase_tests` 2/2 (root, zc),
`fuse_zc_write_fusion_tests` 5/5 (root), `zc_bridge_cqe_wedge_tests` 3/3
(root), `nvme_fill_issue_economy_tests` 3/3 (8× stable under load),
`audit_instruments_tests` 25, `op_trace_tests` 16, read_serve_phase,
read_lane, zcrx_lane 70, read_dest_lease, rebind_starvation,
transport_lease_overlong, kernel/ipc_op_economy, transport_ingress,
multi_queue, skip_ledger, bench_tests 95, `read_path_bench --test` 29/29;
clippy clean both workspaces (all-features + shipped), fmt clean.
`env_knob_convention_tests` fails on dev `2b011d91` itself (D-2's
unregistered `SQUEEZEFS_D2_*` test knobs in `tests/conveyor_two_stage_tests.rs`)
— not this branch's.

### 9.2 Field A-B-B-A, fusion ON: A = R-2 alone (`2b011d91`), B = composed (`55a7bdbb`) — squeeze-test, 04:27–04:42 UTC

Same protocol (fresh cluster, `write_BW` pass under A, 24 × 8 GiB, 30 s + 10 s ramp; both rocky8 `release` builds, clean, KD-7 shim pairing).

| Row | A1 | B1 | B2 (traced) | A2 (traced) | Δ B vs A |
|---|---|---|---|---|---|
| **kern rand-4k** IOPS | **514,438** | **481,251** | 481,358 | 513,944 | **−6.5 % / −6.3 %** |
| kern clat mean / p50 / p99 / p99.9 µs | 369 / 198 / 4,358 / 11,862 | 395 / 268 / **1,483** / 11,207 | 395 / 268 / 1,466 / 11,600 | 369 / 194 / 4,293 / 12,648 | +26 mean, **+70 p50, −66 % p99** |
| kern daemon CPU µs/op | 40.1 (ur 59 %, tpc 41 %) | 34.3 (ur 100 %) | 34.6 | 40.0 | −14 % (all on the worker) |
| partition | demotes 20.37 M ≡ inplace, fusions 0 | **fusions 19.04 M ≡ inplace**, demotes 0 | fusions ≡ inplace | demotes ≡ inplace | exact |
| `transport_total` / `dispatch_lag` / `block_fetch` | 216 / 40.4 / 164 | 249 / **88.0** / 149 | 248 / 87.1 / 148 | 214 / 38.5 / 164 | +33 / +48 / −15 |
| `zc_bridge_phase_ns` msg / sq / device_cq / wake | (pre-instrument) | 0.6 / 0.3 / 49.2 / **98.3** | 0.6 / 0.3 / 49.0 / 97.9 | — | |
| `transport_reap_gap_ns` blind_cqe / park µs | 14.2 / 48.7 | 11.7 / 309 | 11.5 / 302 | 14.0 / 48.7 | |
| wake writes/op · mid-pass reaps | 1.33 · 0 | 0.45 · 93.7 % | 0.45 · 93.7 % | 1.35 · 0 | |
| **il rand-4k** IOPS · clat | 873,918 · 218.8 | 864,768 · 221.1 | 870,773 · 219.6 | 882,852 · 216.6 | par |
| **seq 1 MiB** GiB/s | 36.69 | 39.36 | 37.92 | 35.51 | par (+5 %, fusions 3) |
| **sustained 60 s kern** | (A2) **515,422** / 367.9 / p99 4,227 / flat +4.1 % | (B1) **479,182** / 396.9 / p99 1,483 / flat +1.1 % | | | **−7.0 %** |
| sustained 60 s il | 890,494 / 214.7 | 859,346 / 222.5 | | | −3.5 % |

Tripwires 0 on every row; kern closure every byte in `read_zc_serve_bytes`; il closure `dest_dma + arena ≡ bytes_out`, bounce 0.

### 9.3 Where the gain went (the traced legs, per op)

| Stage | A2 = R-2 alone p50 / mean | B2 = composed p50 / mean | reading |
|---|---|---|---|
| `transport_recv → handler_entry` (ingress to the first handler poll) | 14.4 / **46.6** (the lane hop) | 50.9 / **91.7** (the fused first poll's run-queue wait) | **+45** |
| `keys_resolved → block_fetched` (the bridge) | 105 / **180.9** (msg 28 + device 40 + reap ≈ 60 + wake 36 + probes) | `bridge_sent → taken` 0.7 · `sq_wait` 0.3 · `device_cq` **49.2** · `wake_hop` **99.7** = 153 | **−28** |
| `transport_recv → reply_commit` | 135 / **238.7** | 179 / **253.7** | **+15** |
| fio clat | 198 / 369 | 268 / 395 | +26 |

The bridge's two worker hops (msg_hop 28, the reap share ≈ 60 → 0.7 + ≈ 9)
are gone, as designed — but the fused arm pays for them with two
run-queue waits inside the worker's pass (92 + 100 µs) where R-2 pays one
lane hop (47) plus the handler's own thread hops. The worker is now the
whole daemon (34 µs/op of CPU on one thread per queue, 50 % utilized at
15 k ops/s/queue, bursty arrivals ⇒ queueing ≈ a pass per poll). **The
two levers attacked the same critical path from two sides and converge
on the same wall — the reap thread's per-op serialization — rather than
compounding.** The kernel-side residue (`clat − transport_total`) also
grew ≈ 10 µs (the busier worker reaps delivery CQEs later).

### 9.4 The confirming bracket: the composed DEFAULT (fusion off, `71b7b6d9`) vs R-2 alone — 04:55–05:05 UTC

| Row | A1 (R-2) | B1 (composed, default) | B2 | A2 | verdict |
|---|---|---|---|---|---|
| kern rand-4k IOPS | 506,610 | 506,932 | **510,500** | 502,774 | **par, +0.9 %** |
| kern clat / p50 / p99 | 374 / 198 / 4,424 | 374 / 198 / 4,424 | 372 / 196 / 4,620 | 377 / 198 / 4,293 | identical shape |
| kern CPU µs/op | 40.5 | 40.8 | 40.5 | 40.9 | par |
| partition | demotes ≡ inplace | demotes ≡ inplace, fusions 0 | same | same | exact |
| `zc_bridge_phase_ns` msg / device_cq / wake (R-2's shape, first per-op read) | — | **28.1 / 99.8 / 36.4** | 27.9 / 99.4 / 35.9 | — | device 40 ⇒ reap ≈ 60; Σ software ≈ 125 = the attribution's ≈ 126 |
| sustained 60 s kern | (A2) 502,104 / 377.3 / flat +3.8 % | (B1) **508,183** / 373.2 / flat +1.5 % | | | **+1.2 %** |
| seq 1 MiB GiB/s | 35.70 | 35.75 | | | par |
| il rand-4k | — | (B2) 888,230 / 215.3 | | | par |

Same-day drift vs §9.2's A: −1.5 % on both arms (the box, not the
binaries). **Acceptance verdict:** the composed default binary ≥ R-2
alone (par within noise, slightly ahead), never below it; it does NOT
reach the 750–950 k the attribution predicted, and §9.3 says why: the
prediction assumed the ingress and bridge stages were independent
queues; both are the same thread's serialization.

### 9.5 What the composition leaves on the board

- **The reap thread's per-op serialization is the wall for BOTH levers.**
  R-2's demote arm and R-3's fused arm each move the handler's hops
  around it; neither shortens the worker's own per-op work (≈ 18 µs of
  it on the R-2 shape, ≈ 34 µs fused — the kernel's COMMIT_AND_FETCH
  work `commit_flush` 9–10 µs, the delivery + nvme task-work under the
  enters, the pass ceremony). Cutting that CPU is what would let either
  arm win: the kernel COMMIT cost (sqz-kernel), the per-enter task-work
  batching, the probe short-circuit R-2's residual board named (on the
  cold row the probe is 1–2 µs of pure cost per READ on the worker —
  in the composed-ON row it sat on top of the fused pass).
- **Fusion's tail** (p99 1.5 vs 4.3 ms; p99.9 par) is real and is the
  reason the lever stays registered: a latency-shaped consumer can turn
  it on. The tail campaign (K1's `park` wake path — R-2 §5.3) is the
  IOPS-neutral way to that tail.
- **The bridge instrument now reads the R-2 shape per op** (§9.4): the
  next bridge lever's target is `device_cq − device ≈ 60 µs` (the parked
  worker's CQE wake) + `msg_hop` 28 + `wake_hop` 36 — three wake latencies,
  scheduler-bound, on the lane arm.

**Landing-law checklist:** A-B-B-A — local (same binary, knob) both orders
+ field (arm = binary) both orders, cited; substrates — tcp devsub (the
mandatory multi-connection venue; no loop bracket — the lever is a thread
hop, not a fabric term) + field; sustained — 60 s kern + il legs on both
arms, flat (§5.3); amplification — N/A (reads); engagement — exact on
every row (`fuse3_zc_read_fusions` ≡ zc READs, `read_zc_serve_bytes` ≡
bytes, il closure); `read_copy_*` closure — kern zero passes, il dest_dma
+ arena = bytes_out, bounce 0; instrument + substrate + binary + profile
(`release`, thin LTO, both arms) + kernel + tier stated; tripwires 0;
the box returned unmounted with `/scratch/tmp/sqz-agent/` removed.
Composition (§9): A-B-B-A both orders × two brackets (fusion on, then
the default) with R-2 alone as the A arm; sustained 60 s on both;
engagement partition exact on every row; artifacts
`~/sqz-field-artifacts/2026-09-03/r3-compose*-artifacts.tgz`.
