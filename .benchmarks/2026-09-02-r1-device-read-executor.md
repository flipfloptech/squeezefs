# 2026-09-02 — R-1: the per-device-read timer + channel mutex class (candidate finding 48) — MEASURED, NOT FAT

**Program:** [`docs/design-e2e-perf-audit.md`](../docs/design-e2e-perf-audit.md)
§3.2 Tier 1 rank 4 / §4.3 / §5.3 order 4 (`perf/read-fill-timerless`, run as
`perf/r1-device-read-executor`). Read ledger fat #1 (Appendix B).
**Branch / binary:** `perf/r1-device-read-executor` — the instruments
commit (`59c558cf` at ship time; rewritten as `f75dd950` by a clippy-only
test fixup before landing — daemon and shim code byte-identical) on top of
dev tip `38c49b98`; A1's exact-sum
histograms are in, so every mean below is `Δsum_ns / Δcount`, never a
bucket midpoint). No lever landed — this note is the adjudication.
**Verdict up front:** **NOT convicted at the field posture. Re-scoped.**
The hypothesis' named site (`src/nvme_dev.rs` `read_block` →
`sqz_time::timeout(30 s, rx)`) is **cold on the sqz-kernel field posture
in BOTH modes** (24 arms per 9.9 M kernel reads; ≈ 7 k per 15 M il
reads): kern rand-4k rides the FUSE-zc direct leg
(`zc_device_fetch`, `read_zc_serve_bytes`), il rand-4k rides the
direct-drive engine — neither passes through `NvmeBlockDev::read_block`.
The per-op timer arm that DOES exist is at a different site:
**`InboundQueue::pop`'s ticked `mpsc::recv` park** in the fuse3 transport
(`crates/fuse3/src/raw/connection/fuse_over_uring.rs:1412-1435`), on the
fuse3 fork's own `#[path]`-shared `sqz_time` registry — a second global
lock/heap/`sqz-timer` thread the ledger did not know about — at **0.32–0.45
arms per kernel READ** (1.12 per 1 MiB READ), costing **≈ 2–3 % of daemon
CPU and < 0.2 % of per-op latency** on the kern rows. Below the ≥ ~5 %
conviction bar; the registry lock's contention knee (microbench: it
saturates at 0.8–1.0 M arm cycles/s) sits at ≈ 1.8–2.2 M kernel IOPS at
that arm rate — above the field device ceiling (2.03 M at 32×8), so it
cannot be the wall before the device is. **Adjudicated
load-bearing-at-cost; Tier-3 economy levers named below (§6).**

---

## 1. Instruments, venue, tier

| | |
|---|---|
| **Instrument** | fio 3.42 (`/nix/store/…-fio-3.42/bin/fio`), libaio, `direct=1`, `norandommap=0`, `numjobs=24`, `ramp_time=3`; per-job bw logs at 1 s for the first/last-third flatness column. **il mode** = `LD_PRELOAD=libsqueezefs_il.so` built from the SAME tree (`--profile preload-release --features interposers`), `SQUEEZEFS_IPC_ALLOW_DEV=1` on both ends for the `-dirty` attribution rows. |
| **Rig** | `.benchmarks/rigs/2026-09-02-r1-device-read-executor-rig.sh` + `2026-09-02-r1-row-delta.py` (pre/post `.stats` snapshots → exact phase means, timer gauges, CPU by thread class, read-copy closure). |
| **Substrate** | **tcp devsub, private instance `r1`** (`SQZ_DEVSUB_TRANSPORT=tcp SQZ_DEVSUB_INSTANCE=r1`): 4 × 1 GiB null_blk meta (`/dev/nvme9-12n1`) + 4 × 8 GiB zram data (`/dev/nvme13-16n1`) over nvmet-tcp on 127.0.0.1. The two-substrate rule: tcp is the mandatory venue for the multi-connection shape; a loop bracket was not run (no lever to bracket). |
| **Mount** | cacheless format (no `--disk-cache-paths`, the field posture), `mount --daemon --interception --allow-other`; **kernel 7.1.8-cachyos-lto with FUSE_URING_ZERO_COPY negotiated** (`fuse3_zc_negotiated=1`, `fuse3_kmbuf_negotiated=1`) — the same zc posture as the field's `6.19.14-sqz`. `SQUEEZEFS_FUSE_ZC=0` rows are the **stock-kernel (bufring) posture**, labeled. |
| **File set** | 24 × 256 MiB (6 GiB) laid out once by 1 MiB O_DIRECT writes; every row is cold-by-construction on the device (cacheless mount; the RAM read tier is what `cache_hits` counts). |
| **Box** | 32-core AMD (Strix Halo), 123 GB, **SHARED dev box** — foreign rustc/cargo load throughout (`loadavg 3.7–39` at row start, recorded per row in `*.quiet`). Absolute IOPS/clat here are noise-limited and are NOT claims; the **ratios** (arms/op, CPU share by class, phase means as fractions of clat) are the evidence. |
| **Tier** | measured-real for the local rows; the field row (§5) is measured-real on squeeze-test. |
| **perf** | `perf 7.2` (nix `linuxPackages.perf`), `-e cpu-clock -F 997 -p <daemon>` for the flat profiles (AMD IBS hardware sampling perturbed the rows and under-sampled — cpu-clock is the software clock); **uprobes** on both `Sleep::new` symbols (`_RNvMs2_NtCs5OLuHykLK2v_13squeezefs_ipc8sqz_timeNtB5_5Sleep3new`, `_RNvMs2_NtCs6R2ZZMI7Otg_5fuse38sqz_timeNtB5_5Sleep3new`) with `--call-graph dwarf` for the exact per-site arm attribution. |

Same-venue raw controls (fio libaio direct on the four r1 data namespaces,
prefilled 2 GiB each): **qd1 4k RTT 7.6 µs mean** (p50 6.6, p99 15.8);
**24×8 4k randread 1.78 M IOPS, 105 µs mean clat** (p99 379 µs).

---

## 2. Microbench — the primitives in isolation (`read_fill_executor_prims`, `benches/high_concurrency_bench.rs`)

Criterion medians, this box, foreign load present (`cargo bench --bench
high_concurrency_bench -- read_fill_executor_prims`). `timeout_cycle` =
one device read's timer ceremony exactly as the shipped code pays it: arm
(`Box::pin` + global lock + live-map insert + heap push) → first poll
Pending (global lock + waker clone) → completion poll Ready → drop (global
lock + live-map remove; the heap entry stays as a **tombstone** until the
service thread pops it at the deadline).

| Row | ns / cycle | aggregate | Reading |
|---|---|---|---|
| `timeout_30s_cycle_1t` | **91.6** | 10.9 M/s | uncontended: 3 lock acquisitions + hash map + heap push + alloc ≈ 90 ns — cheap |
| `timeout_2ms_cycle_1t` | 157.5 | 6.4 M/s | the service thread popping tombstones concurrently (steady-state lock sharing) adds ≈ 65 ns |
| `timeout_30s_cycle_8t` | 1,170 (amortized) | **0.85 M/s** | **the contention knee: 8 threads on ONE registry lock ⇒ 12.8× the uncontended per-op cost, aggregate capped at ≈ 0.85 M cycles/s** |
| `timeout_30s_cycle_32t` | 1,249 | 0.80 M/s | flat from 8 → 32 threads: the lock, not the thread count, is the ceiling |
| `timeout_2ms_cycle_8t` / `_32t` | 948 / 1,221 | 1.05 / 0.82 M/s | same ceiling with the popper live |
| `tombstone_drain_200k_heap_200k` | **78.9 / pop** | 12.7 M/s | the service thread's per-tombstone cost (`BinaryHeap::pop` + live-map miss, under the lock) |
| `tombstone_drain_200k_heap_4m` | 86.8 / pop | 11.5 M/s | a 64 MiB / 4 M-entry resident heap costs +10 %: log₂n sift-downs are cache-friendly at the top — heap DEPTH is not the term |
| `sqz_oneshot_cycle` | 100.7 | 9.9 M/s | mint + poll-pending + send + poll-ready + both drops (5 uncontended per-channel mutex ops + one Arc) |
| `futures_oneshot_cycle` | 107.2 | 9.3 M/s | **the lock-free reference is NOT faster** — the per-channel mutex is uncontended by construction (one sender, one receiver); nothing to gain here |
| `crossbeam_lane_try_send_recv` | **12.4** | 81 M/s | the lane channel is crossbeam's lock-free array queue, NOT an sqz mpsc — the ledger's "per-channel Mutex on the lane mpsc" is falsified |
| `deadline_stamp_cycle` (1t / 32t) | 47.1 / **5.6** | 21 / 180 M/s | the timer-less alternative (one `Instant::now()` store + compare, no shared word): the 32-thread aggregate is 225× the registry's |

**Field arithmetic from these constants** (arithmetic-on-measured-constants):
at the field's 441 k kern IOPS × 0.45 arms/op (§3) ≈ **200 k arms/s** on the
fuse3 registry ⇒ ≈ 20–25 % of the lock's saturation capacity; the knee
(≈ 0.8–1.0 M/s) is reached at **≈ 1.8–2.2 M kernel IOPS**, i.e. at/above
the raw device ceiling of the field venue (2.03 M at 32×8, 2.73 M at
32×32). The tombstone pops at 200 k/s cost the service thread ≈ 16 ms/s of
pop work — but see §4: the thread's REAL cost is the fine-grained wakeup
stream, not the pops.

---

## 3. In-daemon attribution — where the arms come from (the uprobe rows)

`perf record -e probe_squeezefs:{ipc,f3}_sleep_new --call-graph dwarf -p <daemon>`
during live rows; counts are exact event counts, callers from the dwarf
unwind.

| Row | Registry | Arms in window | Arms / op | Callers (dwarf) |
|---|---|---|---|---|
| il rand-4k qd8 (2 s window, 589 k IOPS) | squeezefs-ipc | 67,268 | 0.06 | **97.4 % `sqz_channel::mpsc::recv` in `cache::new` async block #7 — the tier EVICTION worker's ticked park** (`(String, Bytes, EvictClass)` channel), off the read critical path; the remainder = `read_block` timeouts on the 8.4 k non-direct handoffs + other ticked parks |
| il rand-4k qd8 | fuse3 | 33 | ≈ 0 | transport ticks |
| kern rand-4k qd8 (4 s window, 314 k IOPS) | squeezefs-ipc | 1,654 | 0.001 | `read_block` is COLD: the zc leg serves every read |
| kern rand-4k qd8 | fuse3 | **482,435** | **0.38** | `InboundQueue::pop` → `sqz_channel::mpsc::recv` → `ticked` → `timeout(TICK)` → `Sleep::new` (the resolved 3–4 %; the remaining samples are on the `fuse3-tpc*` lanes with an unresolvable dwarf stack — same site by the ledger arithmetic below) |

Cross-check by the stats-inode ledger (60 s kern row): `transport_timer_arms`
+6,856,150 over 15.18 M fio ops (16.45 M `fuse3_zc_replies` incl. ramp) =
**0.45 arms/op**, `transport_timer_tombstones_skipped` +6,672,289 (≈ every
arm is won by the recv and popped as a tombstone 2 s later; the fuse3 heap
sat at 183,877 entries = 2 s × the arm rate). `timer_arms` (squeezefs-ipc
face) +26,910 over the same row = 0.002/op.

**Why the kern hot path never reaches `nvme_dev.rs:2426`:** the READ
handler mints a `ZcReadServe` on a zc-armed session and the router's
single-block arm takes the **FUSE-zc direct leg** (`src/routing.rs:16689`,
`zcs.fetch(fd, dev_base + slice_start, slice_len)` →
`FuseConnection::zc_device_fetch`, `fuse_over_uring.rs:3225`): the transport
queue worker (`f3-ur*`) DMAs the device window straight into the request's
pages; its completion is a plain `sqz_channel::oneshot` with **no per-op
timer** — the zc bridge already runs the ledger's proposed design (a
worker-side deadline: `SQUEEZEFS_ZC_BRIDGE_TIMEOUT_MS`, AsyncCancel past
it, `fuse3_zc_bridge_cancels`). `read_zc_serve_bytes` accounts for every
byte of every kern row here. The il hot path is the direct-drive engine
(`ipc_direct_phase_ns` n ≡ `ranged_reads`), own rings, no `read_block`.

**Where the ledger's mechanism DOES engage — the stock-kernel posture**
(`SQUEEZEFS_FUSE_ZC=0`, "stock kernels run the bufring path byte-identically"):
kern rand-4k `timer_arms` +5,233,002 over 4.59 M ops = **1.14 arms/op** on
the squeezefs-ipc face (one `read_block` timeout per dest-lease read +
extras), the ipc heap at **4.56 M tombstones** (30 s × 153 k/s — the
ledger's "≈ 190 MB" realized as 73 MB at this rate), plus 0.57/op on the
fuse3 face. This is the regime the hypothesis described; it is not the
field's.

---

## 4. In-daemon cost — CPU by class and per-op latency

### 4.1 Rows (`.stats` deltas; CPU = `daemon_cpu_ns_by_class`; means exact)

| Row (mode, shape, secs) | IOPS | clat mean / p50 / p99 µs | daemon CPU µs/op | arms/op ipc / fuse3 | `sqz-timer` class | Engagement |
|---|---|---|---|---|---|---|
| **zc kern rr4k qd8 60 s** | 253 k | 755 / 322 / 4,555 | 37.2 (fuse3-ur 50.4 %, fuse3-tpc 48.0 %, **sqz-timer 1.55 %**) | 0.002 / **0.452** | 8.73 s of 565 s | `fuse3_zc_replies` 16.45 M = every op; `read_zc_serve_bytes` 67.4 GB; `ranged_reads` 24 |
| **zc il rr4k qd8 60 s** | 479 k | 399 / 117 / 3,981 | 23.2 (svc 43.8 %, tpc 25.6 %, blk 14.9 %, dd 11.4 %, **sqz-timer 0.89 %**) | **0.104** / 0.000 | 5.94 s of 667 s | `ipc_ops_read` 30.6 M; direct-drive n = 18.41 M = `ranged_reads`; fast-path serves 12.17 M |
| zc kern rr4k qd32 30 s | 287 k | 2,672 / 1,434 / 15,008 | 36.6 (sqz-timer 1.23 %) | 0.002 / 0.320 | 3.89 s | zc replies 9.84 M |
| zc il rr4k qd32 30 s | 476 k | 1,610 / 469 / 13,828 | 21.7 (sqz-timer 0.78 %) | 0.095 / 0.000 | 2.43 s | `ipc_ops_read` 16.29 M |
| zc kern seq 1 MiB qd8 30 s | 9.96 k (9.73 GiB/s) | 19,248 | 123.9 (0.48 µs/4 KiB; sqz-timer 2.4 %) | 0.048 / **1.116** | 0.89 s | zc replies 342 k; `read_zc_serve_bytes` 350 GB |
| zc "il" seq 1 MiB qd8 30 s | 9.56 k | 20,044 | 122.7 | 0.049 / 1.128 | 0.81 s | **INVALID as an il row**: `ipc_ops_read` = 0 — the shim routes 1 MiB reads to the kernel lane by its size rule; it is a second kern row |
| **no-zc kern rr4k qd8 30 s** (stock posture) | 153 k | 1,249 / 807 / 6,849 | 53.4 (tpc 47.8 %, fuse3-ur 27.3 %, **sqz-nvme 23.2 %**, sqz-timer 1.64 %) | **1.140** / 0.566 | 4.02 s | `ranged_reads` 5.22 M = every op; `read_dest_lease_bytes` 21.4 GB |
| no-zc kern rr4k qd8 30 s (perf'd) | 189 k | 1,013 / 725 / 4,227 | 54.8 (sqz-timer 1.96 %) | 1.133 / 0.543 | 6.09 s | same shape |

Flatness (first/last third of the aggregate bw log): il 60 s **+1.8 %**
(flat); kern 60 s +18.7 % — NOT flat, the foreign load moved under it
(loadavg 3.7 at start); no absolute IOPS here is a sustained claim.

### 4.2 perf flat profiles (`cpu-clock`, 12 s windows, daemon-wide)

| Symbol class | zc kern (116,985 samples) | zc il (112,711) | no-zc kern (101,977) |
|---|---|---|---|
| fuse3 `sqz_time::service` loop (the fuse3 `sqz-timer` thread) | 0.88 % | — | 0.79 % |
| `Sleep::new` / `::drop` / `::poll` / `SleepState` map ops (both faces) | 0.56 % | 0.09 % | 1.94 % |
| `std::sync::Mutex::lock_contended` (ALL std mutexes) | 0.81 % | 0.51 % | 0.81 % |
| futex kernel side (`futex_hash`/`futex_wake`/`futex_wait*`, ALL wakers) | ≈ 1.2 % | ≈ 4.0 % | ≈ 1.6 % |
| `sqz_channel::oneshot` (send/poll/drops) | 0.45 % (`Receiver<i32>`, the zc fetch) | 0.14 % | 0.43 % (the `read_block` completion) |
| crossbeam lane channel (`try_send`/`start_recv`/`SyncWaker`) | — | — | 1.50 % |
| jemalloc (`_rjem_*`, all allocs incl. the `Box::pin` per park) | ≈ 2.0 % | ≈ 1.9 % | ≈ 1.9 % |
| **R-1 class, generous upper bound** (timer both faces + oneshot + ½ of contended/futex/malloc) | **≈ 2.5–3 %** | **≈ 1 %** | **≈ 5–6 %** |

`sqz-timer` by the CPU class ledger (the exact face): 1.55 % kern, 0.89 %
il, 1.6–2.0 % no-zc. Its cost is NOT the pops (200 k × 87 ns ≈ 17 ms/s
would be 0.15 %): it is the **wakeup stream** — deadlines arrive 2 s after
each arm at the arm rate, so the service thread's `Condvar::wait_timeout`
fires every ≈ 8 µs and pays a futex syscall + `Instant::now()` + lock per
one or two pops (≈ 1.3 µs per tombstone measured: 8.73 s / 6.67 M).

### 4.3 Per-op latency

The class's on-path cost per READ is the arm/poll/drop cycle
(≈ 90–160 ns uncontended, §2) × 0.45 arms/op ≈ **≤ 0.1 µs/op**, plus a
share of `lock_contended` (0.81 % × ≈ 11 daemon cores ≈ 90 ms/s over
250–300 k ops/s ≈ 0.3 µs/op across ALL mutexes). Against a 755 µs kern
clat (local) / 430 µs (field) that is **< 0.2 % of the per-op FS
overhead**. The terms that ARE the per-op overhead, by the exact means:

| Term (zc kern rr4k qd8 60 s) | mean µs | of clat |
|---|---|---|
| `read_transport_phase_ns.transport_total` | 533 | 71 % |
| `read_serve_phase_ns.total` (handler entry → return) | 407 | 54 % |
| `read_serve_phase_ns.block_fetch` (the zc fetch RTT) | 397 | 53 % |
| `read_transport_phase_ns.queue_wait` + `dispatch_lag` | 64 + 60 = 124 | 16 % — read board #2 |
| fio clat − `transport_total` (kernel-side residue) | ≈ 220 | 29 % |

And on the stock posture (no-zc, where `read_block` runs):
`read_fill_phase_ns.dev_queue` = **500 µs** (enqueue → SQE build) vs
`dev_service` 285 µs — the client-side device queueing behind the lane's
`submit_and_wait(1)` park is 40 % of a 1,249 µs clat. That is **read board
#3 (fill-issue economy, `perf/fill-poll-cohort`)**, not #1; the timer
class is invisible beside it.

---

## 5. Field rows (squeeze-test, `6.19.14-sqz`) — attribution-only, no lever, no A/B

**Venue:** squeeze-test (32-core Xeon 6426Y, 2×200 GbE) → 5 storage nodes
over nvme-tcp, `cluster_reset_v4.sh` fresh (5 meta + 10 data namespaces,
cache-less format), `squeezefs.r1` = the **rocky8 `task build` of
`59c558cf`** (the instruments commit as shipped — `f75dd950` after the
clippy-only test fixup; clean commit, daemon + shim same build, KD-7) mounted with
`--interception --allow-other`; `fuse3_zc_negotiated=1`, `data_read_lanes=4`.
File set 24 × 1 GiB laid out by the rig (same shape as
`/scratch/tmp/fio_jobs/randread_iops.job`: 4k randread libaio qd8 ×24,
`size=1g`, `direct=1`, `norandommap=0`); fio 3.36. Box idle (loadavg 0.0).
Artifacts: `/scratch/tmp/r1rows/` (pre/post `.stats`, fio JSON, bw logs,
`perf-field-kern.data`, `perf-field-dwarf.data`). **Tier: measured-real.**
The mount was returned to the resident `/scratch/tmp/squeezefs` afterwards.

| Row | IOPS | clat mean / p50 / p99 µs | flat (first/last third) | daemon CPU µs/op | arms/op ipc / fuse3 | `sqz-timer` class | Engagement |
|---|---|---|---|---|---|---|---|
| **kern rr4k qd8 60 s** | **464 k** | 410 / 281 / 2,900 | **−4.3 % (flat)** | 36.9 (fuse3-tpc 52.5 %, **fuse3-ur 45.3 %**, sqz-timer **1.14 %**) | 0.019 / **0.352** | 11.71 s of 1,029 s | `fuse3_zc_replies` ≡ ops; `ranged_reads` = **0** (the named site ran ZERO times) |
| **il rr4k qd8 60 s** | **910 k** | 210 / 187 / 553 | **−1.5 % (flat)** | 18.0 (svc 50.8 %, dd 29.9 %, tpc 10.0 %, sqz-timer **0.44 %**) | **0.056** / 0.000 | 4.35 s of 980 s | `ipc_ops_read` ≡ ops; direct-drive n ≡ `ranged_reads` |
| kern rr4k qd8 30 s | 468 k | 406 / 272 / 3,097 | −8.4 % | 37.9 (sqz-timer 0.95 %) | 0.001 / 0.373 | 5.07 s of 532 s | `read_zc_serve_bytes` 63.3 GB = every byte |
| il rr4k qd8 30 s | 910 k | 210 / 187 / 545 | +10.5 % | 18.7 (sqz-timer 0.40 %) | 0.057 / 0.000 | 2.03 s of 509 s | `ipc_ops_read` 30.05 M |

Against the 2026-09-02 baseline of record (441.5 k / 0.43 ms kern,
873.8 k / 0.22 ms il on `aecf1561`): 464 k / 0.41 ms and 910 k / 0.21 ms —
the instruments cost nothing measurable (4 relaxed counters + one
occupancy probe per stats read).

**Field perf** (`cpu-clock -F 997`, 12 s in the kern row, 174,056 samples):
`sqz_time` + `SleepState` symbols **1.92 %** (fuse3 `service` loop 0.84,
`Condvar::wait_timeout` 0.62, `Sleep::new` 0.59, map remove 0.17,
drop/poll 0.21; the squeezefs-ipc `service` loop 0.09), the `sqz-timer`
threads 1.34 % by comm, the zc-fetch oneshot 0.55 %,
`Mutex::lock_contended` 2.51 % of which the dwarf capture resolves 0.49 %
to `InboundQueue::pop`'s guard and 0.50 % to the pop's `Notified::drop`
(the `race2` loser side) — the registry lock's share is inside the 0.75 %
unresolved remainder at most. **R-1 class on the field kern row: ≤ 3 % of
daemon CPU; il: ≈ 0.5 %.** The named site: 0 arms per 27.9 M kernel reads.

---

## 6. Adjudication

**Candidate finding 48 does not promote to a finding at the field
posture.** By the §4.3 rule ("if the share is < the instrument's
resolution … adjudicated load-bearing-at-cost and closed") the share is
resolvable (≈ 2–3 % daemon CPU) but below the ≥ ~5 % conviction bar, and
the per-op latency share (< 0.2 %) is at the instrument's floor. The
ledger's mechanism is real; its SITE and RATE were wrong:

1. **`nvme_dev.rs:2426` is not on the field hot path.** kern → zc direct
   leg (worker-side deadline, oneshot completion, no timer); il →
   direct-drive engine. Only the stock-kernel (`SQUEEZEFS_FUSE_ZC=0`) and
   transform-volume / unaligned / device-true postures pay it (1.14
   arms/op there).
2. **The per-op arm is `InboundQueue::pop`'s ticked recv** on the fuse3
   registry (0.32–0.45/op at 4 KiB, 1.12/op at 1 MiB). This is the exact
   "per-pull timer registration" that **L3 lever C retired**
   (`.benchmarks/2026-07-18-l3-transport-economy.md`; the comment at
   `fuse_over_uring.rs:1407-1411` still says "no per-pull timer
   registration") — the 2026-08-13 rip-tokio-TOTAL sweep reintroduced it
   through `sqz_channel::recv`'s `ticked()` backstop. Its cost today is
   the ≈ 1.5 % `sqz-timer` class + ≈ 0.6 % lane-side arm/drop work.
3. **The lane mpsc is crossbeam (lock-free, 12 ns/op)**, not an sqz mpsc;
   the oneshot's per-channel mutex is uncontended and a lock-free oneshot
   is not faster (§2). Both "mutex" items of the ledger are retired.
4. **The registry lock's contention knee is real** (0.8–1.0 M cycles/s per
   registry, §2) and sits at ≈ 1.8–2.2 M kernel IOPS at the measured arm
   rate — a latent structural limit, not a term at 441 k.

**Levers (Tier 3 — `perf/read-handler-economy` R-5 batch or a transport
economy PR; none landed here, per the measure-first rule):**

- **Timer-thread wakeup coalescing** (`sqz_time::service_loop`): park at
  least ≈ 1 ms past the earliest deadline before firing (the module's own
  contract is "ms-class accuracy"; every caller tolerates it). Expected:
  the ≈ 200 k wakeups/s collapse to ≤ 1 k batches/s ⇒ the 1.5 % `sqz-timer`
  class → ≈ 0.1 %. Zero hot-path change; the tombstone counter is the
  engagement instrument.
- **A timer-less inbound park**: `InboundQueue::pop` already has a
  first-party shutdown wake (`race2(recv, notified)`); the ticked backstop
  there protects against a lost `rx_waker` wake only. A `recv_untimed()`
  (or an amortized single `Sleep` re-armed only when a tick fires) removes
  0.45 arms/op ⇒ ≈ 0.6 % lane CPU + the `Box::pin` per park, at the cost
  of the sqz-sync law's uniform backstop on that one park — a design
  decision for the transport-economy PR, not this one.
- **The stock-kernel posture's `dev_queue` (500 µs)** is read board #3's
  item and dwarfs everything here on that posture.

**What this PR lands:** the instruments (`timer_*` / `transport_timer_*`
gauges; `fuse3-ur` and `sqz-timer` CPU classes — `fuse3-ur` was 51 % of the
kern row's CPU hidden in `other`), the `read_fill_executor_prims`
microbench group, the rig, and this note. `tests/audit_instruments_tests.rs::stats_inode_carries_both_timer_registries_reading_their_own_words`
pins the export law.

**Landing-law checklist:** A-B-B-A — N/A (no lever); sustained 60 s rows —
run (il flat +1.8 %; kern +18.7 % under foreign load, labeled not-flat);
amplification — N/A (reads); `read_copy_*` closure — kern rows: every byte
in `read_zc_serve_bytes` (zero daemon passes), il rows:
`read_dest_dma_bytes` 75.4 GB + `ipc_arena_copy_bytes` 49.9 GB ≈
`ipc_bytes_out` 125.3 GB ✓; engagement — every row's ops accounted
(`fuse3_zc_replies` / `ipc_ops_read` / `ranged_reads` columns above);
instrument + substrate + tier stated; field row in §5.
