# 2026-09-03 — R-4: reap-thread economy — the worker's per-op ledger, a CPU diet that ships (+0.7…+2.4 % kern rand-4k, −3…−4 % CPU/op), and a spin-before-park that the field convicted (69 % of parks deleted, no latency term moved)

**Program:** [`docs/design-e2e-perf-audit.md`](../docs/design-e2e-perf-audit.md)
§3.3 (the wall R-2 and R-3 converged on — the FUSE-over-io_uring queue
worker's per-op serialization,
[`.benchmarks/2026-09-03-r3-fill-issue-economy.md`](2026-09-03-r3-fill-issue-economy.md)
§9.5). **Inputs:** R-3 §9 (the composition verdict), R-2 §5.3 (the K1
tail lives in the PARKED worker's wake path — `transport_reap_gap_ns`
`park` > 1 ms on 1.2–1.5 % of parks), the 4 KiB-random attribution
(`.benchmarks/2026-09-03-4k-random-attribution.md` §4/§7). **Branch:**
`perf/r4-reap-thread-economy` from dev `96156b15`; commits `e064d14b`
(rigs), `cc7fb61a` (contracts), `e7e7a542` (diet + spin, spin default
derived-on), `d064813d` (docs + A-B-B-A rigs), `65694a4e` (spin default
OFF on the field verdict). Field binaries: rocky8 `task build` of
`e7e7a542` (B/C arms) and `65694a4e` (D arm), both `release` (thin LTO),
clean trees; A = the shipped dev tip `/scratch/tmp/squeezefs.kvmap` =
`96156b15` (clean, `release`). Every mean below is an A1 exact
`Δsum_ns / Δcount`; per-op ratios divide by the STATS population (row +
ramp) unless labeled `fio-op`.

**Verdict up front.** Step 1 measured where the worker's µs/op go
(§2): at the field the `f3-ur*` thread costs **17.7 µs per kern 4 KiB
random read, 61 % of it in the kernel** — the COMMIT_AND_FETCH's own
work with **1.9 µs of spinlock contention** on the fuse connection's
`bg_lock` / the ring queue's lock (`native_queued_spin_lock_slowpath`
7.8 % of the worker's cycles), the nvme-tcp submit of the fetch 2.5 µs,
two futex wakes toward the lane 1.7 µs, six syscalls' entry 1.3 µs — and
37 % in the daemon: the loop's own scans and shared-line atomics 2.1 µs,
the R-2 probe ladder 1.8 µs (structurally 0 serves on this shape),
instruments 1.0 µs. Per op the worker paid ~10 clock reads, ~20
process-global atomic RMWs (32 cores on one line each), 2.07 eventfd
`read(2)` against 1.01 wakes, a `Vec` collect per pass and a 2 × 32-ent
deadline scan with five pool-shared stores on 1.76 passes/op. **The diet
that removes those (§4) ships**: field kern rand-4k 24×8 **+1.9 % /
+1.9 % / +0.7 % / +1.1 % IOPS across four brackets (both orders),
+2.4 % / +0.7 % on the 60 s sustained pairs, CPU/op −2.8…−4.1 %, the
worker's pass work (`blind`) 6.2 → 4.7 µs (−24 %), p99 −3…−6 %; il and
1 MiB seq par.** **The spin-before-park (§5) does NOT ship**: at the
200 µs rail it absorbed **0.50 parks/op — 69 % of the worker's parks
(0.76 → 0.24/op) — and `msg_hop` / `device_cq` / `wake_hop` did not
move by a µs** (27.1 → 27.6 / 97.5 → 96.2 / 36.6 → 38.8), at +5.5 %
CPU/op (+12.6 % on the workers), −2.7 % / +0.6 % IOPS and p99.9 +5…+21 %.
The class it covered was the ≤ 16 µs waits a `DEFER_TASKRUN`
`submit_and_wait` already returns from without sleeping; the class that
carries the wake latency (0.24 parks/op, 140 µs mean) sits past any
window a 75–80 %-busy box can afford — the 2026-07-26 theft verdict,
re-measured on the transport. **The worker's latency terms are not
park-shaped**: the lane's message waits ~10 µs (p50) / 200 µs (p99) to
be TAKEN by a worker that is spinning or passing, i.e. the terms are the
worker's KERNEL residence per enter (the COMMIT's work, the fetch's
submit, the lock contention) and its own run-queue standing on a
saturated box — §7 names the levers that follow from that.
`SQUEEZEFS_FUSE_IO_URING_SPIN_US` stays as the registered A/B lever
(default `0` = off; any value engages the governor with that cap).
**Tier: measured-real** (field), the local rows noise-limited (§1).

---

## 1. Instruments, venues, tier

| | |
|---|---|
| **Instrument** | `perf record -e cpu-clock -F 4999 -t <f3-ur tids>` mid-row (flat: the per-symbol self-time ledger; a 2 s `--call-graph dwarf` capture for caller attribution — perf 4.18 on the box needs `rustc-demangle` offline for v0 symbols), `perf stat -e syscalls:sys_enter_{io_uring_enter,read,futex,write},context-switches,cpu-migrations -t <tids>` (per-op syscall counts), box-wide `/proc/stat` busy over every row; fio 3.36 (field, the attribution note's jobs verbatim: `randread_iops.job` 24 × `size=1g` 4 KiB qd8 libaio `direct=1`, 30 s + 10 s ramp; `read_BW.job` 1 MiB qd16; the 60 s legs a `sed`'ed `runtime=60` copy). il mode = `LD_PRELOAD` of the same build's shim (`SQUEEZEFS_IPC_ALLOW_DEV=1` both ends — arm A's box build is the shipped one). Per row: pre/post `.stats` (exact histograms), fio JSON, 1 s bw logs; `SQUEEZEFS_OP_TRACE=1` on one leg per arm with a mid-row `.trace` drain (dentry drop first), stitched offline (`tests/op_trace_stitch.py` needs Python ≥ 3.8 — the box's 3.6 lacks `statistics.fmean`, §8). |
| **Rigs** | [`.benchmarks/rigs/2026-09-03-r4-field-perf.sh`](rigs/2026-09-03-r4-field-perf.sh) (step 1), [`2026-09-03-r4-classify-worker-profile.py`](rigs/2026-09-03-r4-classify-worker-profile.py) (the ledger classes), [`2026-09-03-r4-perf-workers-local.sh`](rigs/2026-09-03-r4-perf-workers-local.sh), [`2026-09-03-r4-field-abba.sh`](rigs/2026-09-03-r4-field-abba.sh) + [`2026-09-03-r4-row-delta.py`](rigs/2026-09-03-r4-row-delta.py) (the A-B-B-A), [`2026-09-03-r4-field-confirm.sh`](rigs/2026-09-03-r4-field-confirm.sh) (the shipped-default bracket). |
| **Field venue** | squeeze-test (32-thread Xeon 6426Y, 2 sockets × 16 cores no SMT, 251 GB, 2×200 GbE, **6.19.14-sqz**, `preempt=full`, `acpi_idle` C1-only — C-state exit is NOT a wake-latency source here) → 5 storage nodes over nvme-tcp, memory-backed nullblk targets, `cluster_reset_v4.sh` fresh (5 meta + 10 data namespaces, cache-less format), one `write_BW.job` pass (arm A) minting 24 × 8 GiB; FUSE-over-io_uring 32 × 32, zc + kmbuf negotiated, `data_read_lanes = 4`. Box otherwise idle (loadavg 0.00 at every start). Windows: 06:26–06:46 UTC (step 1), 07:40–07:58 (A-B-B-A), 12:43–12:49 box clock (the confirm bracket). Box-wide busy during a kern rand-4k row: **75 %** (user 27, sys 36, irq 4, softirq 7). |
| **Local venue** | tcp devsub (4 × 1 GiB null_blk meta + 4 × 8 GiB zram data over nvmet-tcp 127.0.0.1), cache-less format, 32-core AMD Strix Halo, kernel 7.1.8-cachyos-lto zc-armed — a SHARED dev box carrying foreign release builds (loadavg 5–40 throughout): the local ledger's SHARES are the evidence, its absolute numbers are not (the local A0 row decayed −32 % first→last third under a concurrent build; the local sanity row of the fix read the box at > 80 % busy and the governor refused every spin decision — `refused_busy` 845 k, absorbed 0 — the guard working, not a measurement). |
| **Arms** | **A** = `96156b15` kvmap (the R-3 §9.4 default binary — the shipped dev tip). **B** = `e7e7a542`: the diet + the spin governor at its derived default (cap = the 200 µs rail). **C** = the SAME `e7e7a542` binary with `SQUEEZEFS_FUSE_IO_URING_SPIN_US=0` — the diet-only control (the two levers separable). **D** = `65694a4e`, the shipped default (spin off by default — code path identical to C; re-measured as the default, not as a knob). |
| **Tier** | measured-real (field); local rows scoping only. |

**Artifacts:** `~/sqz-field-artifacts/2026-09-03/r4-field-artifacts.tgz`
(every field row's stats pair, fio JSON, bw logs, `.row` tables, the
perf flat/dso/stat/caller outputs, the driver logs, the rigs as run) and
`r4-local-and-traces.tgz` (the local A0 perf flat + call graphs, the
field traced legs' `.trace.json` + stitch, the local rows). The box's
`/scratch/tmp/sqz-agent/r4/` was removed at the end; the box returned
unmounted.

---

## 2. Step 1 — the ledger: where the reap worker's 17.7 µs/op go (field, `96156b15`)

Row A0 (kern rand-4k 24×8 with perf attached): 492.0 k IOPS, clat 386 µs,
daemon CPU 40.6 µs/fio-op = 30.4 µs/stats-op, of which **`fuse3-ur`
350.1 s / 19.73 M ops = 17.7 µs/op** and `fuse3-tpc` 12.6 µs/op. perf
`cpu-clock` on the 32 workers, 8 s mid-row (28 % busy per worker):

### 2.1 By DSO and by class (µs/op of the worker's 17.7)

| DSO | share | µs/op |
|---|---|---|
| kernel | 62.9 % | 11.1 |
| squeezefs (daemon) | 32.5 % | 5.8 |
| libc + libpthread (syscall stubs, memmove) | 3.4 % | 0.6 |
| vdso (clock) | 1.1 % | 0.2 |

| Class | share | µs/op | What it is (top symbols) |
|---|---|---|---|
| **k: syscall entry + LOCK CONTENTION** | 18.1 % | **3.20** | `native_queued_spin_lock_slowpath` **7.8 %** + `_raw_spin_lock` 3.1 % — the callers are **`fuse_uring_req_end` (3.8 %, the ring queue's lock) and `fuse_request_end` (3.1 %, the connection's background-request lock)** under `fuse_uring_commit_fetch` ← `io_uring_cmd` ← `io_submit_sqes`: every COMMIT_AND_FETCH the worker submits ends the request under two locks the 24 submitting fio threads and the other 31 workers also take (≈ **1.9 µs/op of pure contention**); `do_syscall_64` 1.4 %, `fget`/`fput`/`fdget_pos` 2.4 % (six syscalls per op — the eventfd `read(2)`s' file lookups) |
| **k: nvme/blk submit** | 14.4 % | **2.55** | `nvme_tcp_queue_rq` 0.7 %, `queue_work_on` 1.4 % (kicking the nvme-tcp io_work), `nvme_round_robin_path` 0.9 % (multipath selection per I/O), `__alloc_skb`/`skb_*`/`selinux_socket_sendmsg` — the zc fetch's `READ_FIXED` submission runs the fabric's send path inline in the worker's enter |
| **d: queue_worker self** | 11.7 % | **2.06** | the pass loop: the 32-ent parked scan per pass, `member_of_qid`, slot/ent bookkeeping, the inlined shared-line atomics (`zc_bridge_pends` ±1 per op, `STATS_REQUESTS`, the five scan gauges per pass), the per-pass `Vec` collect (0.36 %) |
| **d: the R-2 probe ladder** | 10.0 % | **1.77** | `NvmeCache::get_static` **3.0 %** (two RwLock reads + xxh3 per probe, called for the staging leg AND the read-cache leg — on a cache-less zc mount both are structurally empty), `DashMap::_get` 1.0 %, `try_read_range_sync` 0.6 %, `ReadMostlyCache::peek_with` 0.35 %, `LockCore::try_acquire`/release 0.47 %, `CachedMetadata::clone` 0.32 %, `from_utf8` 0.41 %, `StackKey`/`fmt` 0.4 % — the seven lookups R-2 §8 named, pure cost on this shape (0 serves) |
| **k: sched/wake** | 9.5 % | **1.67** | `_raw_spin_unlock_irqrestore` 6.1 % = **`aio_complete_rw` 2.5 %** (the app's libaio completion + wake, INSIDE the COMMIT) + **`try_to_wake_up` via `futex_wake` 2.1 %** (`oneshot::send` from `zc_fetch_complete` 1.0 % + `TpcScheduler::dispatch` 1.0 % — the two wakes the worker pays toward the lane per op) |
| k: other | 6.8 % | 1.21 | `handle_softirqs` 1.5 %, `mlx5e_txwqe_complete`/`mlx5e_poll_tx_cq` 1.6 % — the NIC's TX completion softirq charged to whichever thread it lands on |
| **k: fuse commit/fetch (the COMMIT's own work)** | 6.0 % | **1.06** | `fuse_uring_set_up_zero_copy` 0.9 %, `fuse_uring_cmd` 0.8 %, `fuse_aio_complete` 0.7 %, `fuse_put_request` 0.6 %, `fuse_request_end` 0.4 %, `fuse_zc_pages_release` 0.4 %, `fuse_uring_send_in_task` 0.4 %, `fuse_uring_commit_fetch` 0.3 %, `fuse_uring_args_to_ring` 0.3 % |
| **d: worker instruments** | 5.6 % | **1.00** | `note_handler_bridge_passbottom` **1.3 %** (a fresh clock read + a record into ONE process-global `bridge_rtt` histogram — 32 cores on one line), `ReapCadence::enter_begin/enter_end/cqes_surfaced` 1.9 %, `SubmitBatch::note_flush` 0.9 % (three global RMWs per flush), `op_trace::stamp` 0.45 % (disarmed), `zc_bridge_phase_record_ns` 0.4 %, `BridgeDeadlines::overdue` 0.5 % + `register_retries_due` 0.3 % (per-pass walks) |
| k: mm/slab/memcg | 3.5 % | 0.62 | |
| k: io_uring core | 2.9 % | 0.51 | `io_submit_sqes`, `io_get_ext_arg`, `io_init_req`, `io_buffer_unregister` |
| d: syscall stubs (libc/pthread) | 2.1 % | 0.38 | `syscall` 1.3 %, `__libc_read` + the pthread cancel wrappers 0.9 % — the eventfd `read(2)`s |
| d: dispatch / lane hand-off | 2.0 % | 0.35 | `ExecShared::enqueue`, `LaneTask::wake`, `TpcScheduler::dispatch`, `oneshot::send` |
| d: alloc (jemalloc) | 1.5 % | 0.27 | `_rjem_malloc`, `edata_heap_remove`, `eset_remove` — the boxed handler future + `header_and_op` per op, the `Vec` collect per pass |
| d: memmove | 0.8 % | 0.14 | |
| d: clock reads (vdso + `Instant`) | 1.8 % | 0.32 | ≈ 10 `clock_gettime` per op (`enter_begin`/`enter_end` per enter × 1.76, the pop stamp, the mint stamp, the bridge-taken stamp, the flush stamp, the passbottom record, `cf_t0`, the scan's `now`) |

**Kernel vs daemon on the worker: 11.1 µs (63 %) vs 6.6 µs (37 %)** —
and of the kernel share, **≈ 3.5 µs is the COMMIT_AND_FETCH's own work
(1.9 of it lock contention), 2.5 the fetch's fabric submit, 1.7 the
wakes, 1.3 syscall entry** — none of it userspace-removable except by
doing fewer enters per op or by moving the submit off this thread.

### 2.2 Per-op counts (the same row)

| | per op | note |
|---|---|---|
| `io_uring_enter` | **1.76** | `transport_reap_gap_ns.blind` count ÷ ops; perf stat agrees (2.60 M / 3 s) |
| eventfd `read(2)` | **2.07** | against **1.01** wake writes/op — `drain_wake_eventfd` looped to EAGAIN (one useful read + one EAGAIN per drain) |
| `futex` | 2.07 | the two lane wakes (spawn + oneshot) |
| context switches | 1.36 | |
| cpu migrations | 0.22 | node-scoped affinity |
| blocking enters (`park`) | **0.76** | mean 52 µs — **21 % ≤ 1 µs, 47 % ≤ 8, 68 % ≤ 16, 84 % ≤ 32, 90 % ≤ 64, 94 % ≤ 128; 1.6 % > 512 µs, 0.6 % > 1 ms** |
| CQEs surfaced | 2.91 | delivery + device + wake-poll |
| COMMIT_AND_FETCH per flush | **1.71** | 11.5 M flushes carried 19.7 M commits; **64 % of flushes carry exactly one** (7.33 M) — the commits arrive one lane message at a time and the worker wakes per message; batching them means delaying replies (§7) |
| pass work (`blind` mean) | 6.1 µs | `blind_cqe` 14.3 |

### 2.3 The local ledger (tcp devsub, cachyos 7.1.8, foreign load 5+)

Worker 17.3 µs/op (222.4 s / 12.85 M), kernel 52 % / daemon 46 %: sched/wake
2.74, **probe ladder 2.63** (`NvmeCache::get_static` 3.4 %), worker self
2.12, nvme/blk 1.91, fuse 1.29, mm 1.13, io_uring 0.87, syscall entry 0.74,
jemalloc 0.73, clock 0.66, instruments 0.57, dispatch 0.47. The local
kernel lacks the field's lock contention (one nvmet-tcp target, one
socket) and pays more scheduler (the loaded box); the daemon classes agree
with the field to within the noise.

---

## 3. Step 2 — the levers the ledger convicts

| Lever (the task's order) | Ledger answer | Decision |
|---|---|---|
| **(a) per-op CPU diet on the worker** | 6.6 µs/op of daemon time, of which ≈ 2 µs is removable without touching the FS: the contended process-global atomics (~20/op), the eventfd double read, the per-pass deadline scan + five pool stores, the per-pass `Vec`, one redundant clock read per bridge CQE | **built, shipped** (§4) |
| **(a′) the commit re-arm batching / the inline-serve `Bytes`** | commits/flush 1.71 with 64 % singletons — the batching is engaged exactly as far as the arrival pattern allows (one lane message per wake); the inline serve arm is structurally 0 on this cache-less shape, so its bookkeeping is not on the row | not a lever on this shape |
| **(b) the parked wake path** | 0.76 parks/op, 84 % ≤ 32 µs — the distribution a spin-before-park exists for | **built as an adaptive governor, measured NOT fat, ships OFF** (§5, §6) |
| **(c) COMMIT_AND_FETCH batching depth** | 1.76 enters/op; the kernel share per enter ≈ 6.3 µs, dominated by the COMMIT's own work (which is per commit, not per enter) — fewer enters would save ≈ 0.7 µs of syscall entry per enter avoided while delaying replies | not built (a latency trade the ledger does not justify) |
| the R-2 probe short-circuit (R-5's item) | 1.77 µs/op = 10 % of the worker, structurally 0 serves on a cache-less zc mount; `NvmeCache::get_static` alone 0.53 µs (two RwLock reads + xxh3 × 2 legs) | named for R-5 (§7) — a population gate on the NVMe cache is the first cut |

---

## 4. Step 3/4 — the diet (`e7e7a542`, shipped)

| Item | Before | After |
|---|---|---|
| Engagement counters the worker bumps per op (`STATS_REQUESTS`/`REPLIES`, `zc_replies`, `transport_fast_dispatch_{serves,demotes}`, the commit-batch histogram + `flushes` + `commits`, the fused `bridge_rtt` histogram + `midpass`/`passbottom` reaps) | process-global `AtomicU64`s — 32 cores RMW'ing one line per counter per op | **per-thread SHARDED** (`ShardedCounter`, `crates/fuse3/src/raw/read_phase.rs`; the phase tables' PERF-3 argument applied to the plain counters), folded at export — the stats JSON is byte-identical in shape and exact in value |
| The pool pend gauge (`zc_bridge_pends`) | `fetch_add` at every bridge stamp + `fetch_sub` at every clear — 2 contended RMWs/op | **one `fetch_add(delta)` per pass** with the pass's net delta, flushed before the deadline-scan gate so every `> 0` gate and the watch thread read the exact count at every park; a steady-state pass stamps one and clears one — **0 RMWs** |
| The bridge-deadline scan | every pass with a live pend: `overdue()` (a `Vec` mint + 32 loads), the orphan sweep (32 loads), five pool-shared gauge stores — 1.76×/op | **on a period derived from the deadline** (`timeout / 1024` — 29 ms at the 30 s default, 98 µs at the 100 ms floor), clocked by the cadence's last-enter stamp (no extra read); the bounded-park backstop is unchanged |
| `drain_wake_eventfd` | read-until-EAGAIN: 2.07 `read(2)`/op | **ONE read** — a non-semaphore eventfd read returns and zeroes the counter atomically; the second read only ever answered EAGAIN, and the race it might seem to close (a write after the last read) the loop never closed either (the level-triggered PollAdd surfaces it at the next park) |
| `note_handler_bridge_passbottom` | a fresh `transport_now_ns()` per bridge CQE | the drain's ONE pop stamp (already read) |
| the pass-bottom completion list | `collect()` per pass (one jemalloc alloc + free) | hoisted, reused (`clear` + `extend`) |
| the pool-level `stats_requests/replies/cqe_err/register` twins | written per op, never read | deleted (no dead code) |

Suites: fuse3 229/229 (the fork's own; the new `ShardedCounter`,
`spin` and `spin_governor_core` tests among them); root
`read_fast_dispatch_tests` 6/6 (with one pre-existing load-sensitive flake
— §8), `read_serve_phase_tests`, `transport_lease_overlong_tests`,
`kernel_op_economy_tests`, `ipc_op_economy_tests`, `op_trace_tests` 16,
`audit_instruments_tests` 26 (+1: the spin ledger on the stats inode),
`env_knob_convention_tests` 21 (the new knob registered),
`multi_queue_tests` 8, `transport_ingress_tests`, `bench_tests` 95, the
new `reap_thread_economy_tests` 2/2 (live mount — §5.2); clippy clean in
both workspaces (all-features + shipped), fmt clean, fuse3 bench smoke
clean, markdown links clean. `zc_bridge_phase_tests` self-skips
unprivileged (capability class).

---

## 5. Step 4b — the spin-before-park governor (`e7e7a542` on, `65694a4e` off)

### 5.1 The mechanism (`crates/fuse3/src/raw/connection/spin.rs`; the law in `crates/squeezefs-ipc/src/spin_governor_core.rs`, `#[path]`-shared with the root crate's ipc governor — `src/spin_governor.rs` moved there)

Where the worker would block in `submit_and_wait`, it may first SPIN a
bounded window watching **`IORING_SQ_TASKRUN`** (the ring's "deferred
task work landed" flag — the queue rings are now built with
`IORING_SETUP_TASKRUN_FLAG`; under `DEFER_TASKRUN` every completion from
another context raises it, and reading it is one acquire load), the CQ
tail (plain-ring postings), the wake coalescer's armed flag (a lane
published — its eventfd write is in flight) and the shutdown word; a
catch is surfaced by the work-conserving arm's non-blocking GETEVENTS
enter (no sleep, no wake), an expired window falls into the blocking
enter as before. The pass's SQEs are launched (`submit`) before the spin
so their echoes can arrive inside it. **Adaptive by derivation, never a
constant**: window = `min(2 × the worker's own park-gap EWMA, cap)` (a
reaction clock — every event the worker waits for echoes something it
just did), engaged **only while this worker's queues hold ops in
flight** (a slot `Delivered`/`Parked` or a bridge pend — an idle queue
never spins; `SlotTable::any_owing`), gated by the 200 µs rail (idle
spacing) and **refused past the box's queueing knee** (whole-box busy
> 80 % — the Erlang-C wait onset for c ≥ 16 servers; `/proc/stat` at the
shared 100 ms cadence, the ipc governor's `HeadroomGauge`). The worker
population is one thread per CPU, so the ipc formula ("every spinner
spinning the whole window must fit in the idle capacity") derives 0 for
it by construction — the knee is the guard that applies. Ledger:
`transport_spin_{absorbed,expired,ns,refused_busy,window_us}`; closure
`absorbed + expired ≡` the spins run. Knob
`SQUEEZEFS_FUSE_IO_URING_SPIN_US` = the cap, µs.

### 5.2 Contracts (`tests/reap_thread_economy_tests.rs`, live mount; `audit_instruments_tests`; the fork's `spin::tests` + `spin_governor_core::tests`)

| Law | Pin |
|---|---|
| a quiet queue burns nothing — governor ARMED (cap 200) on an idle mount: `fuse3-ur` CPU flat over 3 s (< 20 ms), spin ledger 0 | green (the in-flight gate + the rail) |
| under an 8-stream cold O_DIRECT burst with a cap the governor engages (spins, or the knee refuses — counted), every spin's cost lands in `spin_ns` at or under the cap; the default leaves every spin word flat | green (the shared dev box sat past the knee during the run: refusals counted, the ledger closed) |
| the commit-batch and reap-gap ledgers close on the sharded counters (commits ≥ reads, flushes ≤ commits, enters ≥ parks) | green |
| the five words ride the stats inode reading the fuse3 fold | green |
| `worker_window_ns`: 2 × EWMA under the cap, cap 0 = off, no ops in flight / never parked / past the rail / past the knee ⇒ 0; the idle→busy regime transition re-opens the window within a bounded fold count; the knob's absent/`0`/value/malformed postures | green (pure) |

RED against `96156b15`: the spin words do not exist (the audit test does
not compile — `fuse3::transport_spin_stats` is absent).

---

## 6. Step 6 — the field A-B-B-A (squeeze-test, kern rand-4k 24×8, 30 s + 10 s ramp; order A1 B1 C1 B2 A2 C2, then the D bracket)

### 6.1 rand-4k

| Row | Binary / lever | IOPS | clat mean / p50 / p99 / **p99.9** µs | CPU µs/fio-op (worker µs/stats-op) | parks/op · spin abs/op · spin µs/op | `blind` | box busy |
|---|---|---|---|---|---|---|---|
| **A1** | 96156b15 | **514,378** | 368.9 / 191.5 / 4,424 / 13,828 | 40.01 (17.73) | 0.760 · — · — | 6.2 | 75.1 % |
| **B1** | R-4, spin on (cap 200) | 500,388 | 379.2 / 193.5 / 4,358 / **16,712** | 42.23 (19.97) | **0.240 · 0.503 · 4.71** (expired 0.019, refused_busy 0.128) | 4.7 | 75.3 % |
| **C1** | R-4, spin off | **524,249** | **361.3** / 191.5 / **4,145** / **12,517** | **38.53 (16.90)** | 0.802 · 0 · 0 | **4.7** | 75.5 % |
| **B2** (traced) | R-4, spin on | 511,329 | 371.3 / 189.4 / 4,751 / 14,484 | 40.9 (19.9) | 0.242 · 0.508 · 4.77 | 4.7 | |
| **A2** (traced) | 96156b15 | 508,400 | 373.0 / 195.6 / 4,358 / 13,566 | 40.24 (17.93) | 0.755 · — · — | 6.2 | 74.7 % |
| **C2** | R-4, spin off | **517,940** | **365.5** / 193.5 / **4,227** / 13,042 | **38.88 (17.28)** | 0.795 · 0 · 0 | 4.7 | 75.5 % |
| **D1** | 65694a4e default (= C) | 509,664 | 371.3 / 195.6 / 4,227 / 13,697 | 39.42 (17.68) | 0.791 · 0 · 0 | 4.7 | 75.4 % |
| **A3** | 96156b15 | 506,276 | 374.5 / 199.7 / 4,358 / 12,911 | 40.56 (18.23) | 0.755 · — · — | 6.2 | 75.0 % |
| **D2** | 65694a4e default | **511,673** | **370.1** / 189.4 / 4,227 / 15,139 | **38.90 (17.18)** | 0.801 · 0 · 0 | 4.6 | 74.3 % |

**The diet (C/D vs A), both orders, four brackets: +1.9 % (C1/A1), +1.9 %
(C2/A2), +0.7 % (D1/A3), +1.1 % (D2/A3) IOPS; clat −2.1 / −2.0 / −0.9 /
−1.2 %; p99 −6 / −3 / −3 / −3 %; CPU/fio-op −3.7 / −3.4 / −2.8 / −4.1 %;
worker µs/op −4.7 / −3.6 / −3.0 / −5.8 %; pass work (`blind`) 6.2 → 4.6–4.7
µs (−25 %), `blind_cqe` 14.4 → 11.1–11.6.** p99.9 is the row's own noise
class (12.5–15.1 ms across all A/C/D rows; the B rows 14.5–16.7).

**The spin (B vs A): −2.7 % / +0.6 % IOPS, clat +2.8 % / −0.5 %, CPU/op
+5.5 %, worker µs/op +12.6 %, p99.9 +21 % / +7 %.** Engagement exact:
absorbed 10.0 M + expired 0.39 M = 10.4 M spins over 19.9 M ops (0.52/op),
mean spin 9.0 µs, `spin_ns` 4.71 µs/op; the parks fell 0.760 → 0.240/op
and the mean park rose 49 → 141 µs (the short class gone, the long class
left).

### 6.2 What the spin did to the per-op terms (exact means; B1 vs A1)

| Term | A1 | **B1** | C1 | reading |
|---|---|---|---|---|
| `zc_bridge_phase_ns.msg_hop` (lane send → worker take) | 27.1 | **27.6** | 27.0 | unmoved — the lane's message does not wait on a park |
| `zc_bridge_phase_ns.device_cq` (enter → CQE popped; device ≈ 40–45) | 97.5 | **96.2** | 97.1 | −1.3 µs |
| `zc_bridge_phase_ns.wake_hop` (popped → lane resumed) | 36.6 | **38.8** | 34.8 | +2 (the lanes lose CPU to spinning workers) |
| `read_transport_phase_ns.dispatch_lag` (mint → lane's first poll) | 41.0 | **45.1** | 39.9 | +4, same cause |
| `read_transport_phase_ns.transport_total` | 216.5 | **220.6** | 211.8 | +4 |
| `transport_reap_gap_ns.park` mean · parks/op | 49.3 · 0.760 | **140.6 · 0.240** | 46.0 · 0.802 | 69 % of parks deleted |
| `transport_reap_gap_ns.blind` (pass work) | 6.2 | 4.7 | 4.7 | the diet |

The traced legs (B2 vs A2, 1-in-10 sampling, 46 k complete chains each)
read the same per op: `bridge_sent → bridge_taken` **p50 9.6 vs 10.3 µs,
p99 201 vs 208, mean 29.6 vs 28.0**; `dev_submit → dev_complete` p50 56.6
vs 59.7, mean 95.6 vs 100.3; `dev_complete → block_fetched` 39.9 vs 39.7;
`transport_recv → handler_entry` **51.1 vs 45.4** (the lane hop, worse).

**The attribution this forces.** With the worker spinning instead of
parking on two thirds of its waits, a lane's message STILL takes 10 µs
(p50) / 200 µs (p99) to be taken and a device CQE STILL waits ≈ 50 µs
past the device to be reaped. So the worker's ingress latency is not its
park/wake — it is (i) the worker's **kernel residence per enter**: each
COMMIT_AND_FETCH enter runs the whole commit (request end under two
contended locks, the app's aio completion + wake, the next delivery's zc
setup — §2.1's ≈ 3.5 µs) plus the fetch's nvme-tcp send (2.5 µs), and
everything queued for the worker waits behind that syscall; and (ii) the
worker's **own run-queue standing** on a box at 75–80 % busy with 24 fio
+ 64 daemon threads + the nvme-tcp kworkers — a preempted or
descheduled worker is what the p99 200 µs message wait and the > 1 ms
`blind` tail are. The spin's absorbed class (mean 9 µs) is the waits a
`DEFER_TASKRUN` `submit_and_wait` already returns from without sleeping
(pending task work runs under the GETEVENTS enter), so there was no
scheduler wake to save; the class that carries the wake latency
(0.24 parks/op, 140 µs mean) sits past any window a box at the knee can
afford — the ipc governor's 2026-07-26 theft verdict, re-measured on the
transport. **Shipped default `0` (off).**

### 6.3 The regression rows and the sustained legs

| Row | A | R-4 | verdict |
|---|---|---|---|
| **il rand-4k** IOPS · clat | 867.8 k · 220.3 (A1), 887.9 k · 215.4 (A2) | 868.7 k · 220.1 (B1), 884.4 k · 216.2 (B2), **899.3 k · 212.6 (D1)** | par (il never touches the worker: `fuse3-ur` 0.00 s; spin absorbed 6 over the row) |
| **seq 1 MiB qd16** GiB/s | 37.13 (A1), 38.06 (A2) | 37.88 (B1), 37.32 (B2), 36.73 (D1) | par (the 36.7–38.1 band is the row's noise; spin absorbed 0.005–0.009/op — the ceiling-class fetch, mostly refused by the knee at 85 % busy) |
| **sustained 60 s kern** | 511,046 / 371.2 µs / p99 4,358 / p99.9 12,780 (A2, flat −3.7 %) | **C1 523,260 / 362.0 / 4,178 / 13,042 (flat +4.6 %); D2 514,764 / 367.9 / 4,358 / 14,877 (flat +8.4 %)**; B1 502,095 / 378.0 / 4,555 / 15,401 (flat +5.8 %) | diet **+2.4 % / +0.7 %** sustained, CPU/fio-op 35.57 → 33.76 / 33.91 (−5 %); spin −1.8 % |
| sustained 60 s il | 872.7 k / 219.1 (A2) | 878.6 k / 217.6 (B1) | par |

Tripwires 0 on every row (`invariant_tripwires`, `transport_lease_overlong`,
`fuse_op_watchdog_overdue`, `transport_cq_overflows`, `read_dest_overruns`,
`ipc_direct_reap_stalls`); engagement exact on every kern row
(`transport_fast_dispatch_demotes ≡ fuse3_read_inplace_replies ≡ fuse3_zc_replies`,
fusions 0, `read_zc_serve_bytes` = ops × 4 KiB, `read_copy_dest_bytes` = 0);
il closure `read_dest_dma_bytes + ipc_arena_copy_bytes ≡ ipc_bytes_out`,
bounce 0.

---

## 7. What is landed, what the ledger leaves on the board

**Landed:** the per-symbol ledger instruments (step 1 rig + classifier);
the worker CPU diet (§4); the spin governor as a registered, default-off
lever with its ledger (§5); the shared `spin_governor_core`; the
`SQUEEZEFS_FUSE_IO_URING_SPIN_US` registry entry + `docs/operations.md`
paragraph; the contracts (§5.2); the A-B-B-A and confirm rigs.

**The next wall, named by this campaign's measurements:**

1. **The worker's kernel residence per enter is the term, not its
   park.** 11.1 of 17.7 µs/op is kernel, ≈ 3.5 of it the COMMIT's own
   work with **1.9 µs of lock contention** on `fuse_request_end`'s
   connection-wide background lock and `fuse_uring_req_end`'s queue lock
   — 32 workers ending requests against 24 submitters. That is
   **sqz-kernel territory** (per-CPU/per-queue background accounting in
   `fs/fuse/dev_uring.c` — the series already carries the zc/retention
   patches, `docker/kernel-sqz/`). Expected: −1.5…−2 µs/op on the worker
   AND on every fio thread's submit path (they take the same locks).
2. **The fetch's fabric submit rides the worker** (2.5 µs/op:
   `nvme_tcp_queue_rq` + the io_work kick + `nvme_round_robin_path`).
   It is on the worker because the zc slot is registered on the queue's
   ring; a per-lane fetch ring would need the zc slot registration to
   travel — a kernel-interface question (the `FUSE_URING_ZERO_COPY`
   registration is per queue), not a daemon one.
3. **The worker's run-queue standing.** The msg-take p99 of 200 µs and
   the `blind` > 128 µs tail (0.2 % of windows) on a box at 75–80 %
   busy with `preempt=full` and no deep C-states are a descheduled
   worker. A scheduling-class lever for `f3-ur*` (a negative nice — the
   reap thread gates every other thread's progress, so its wakeup
   preemption should win against a fio thread's or a lane's) is cheap to
   build and to A/B; it was NOT built here because it needs its own
   inversion analysis against the lanes (which the worker also wakes
   into) and its own bracket. This is the K1-tail campaign's lever, now
   with the park path ruled out.
4. **The R-2 probe ladder on a cache-less zc mount**: 1.77 µs/op = 10 %
   of the worker for 0 serves. `NvmeCache::get_static` is 0.53 µs of it
   (two RwLock reads + xxh3, twice per probe): a population gate on the
   NVMe cache (an atomic entry count maintained under the shard write
   lock; `0 ⇒ None` before any lock) is the first cut, then the
   active-buffer `DashMap` + `SipHash` (0.35 µs). R-5's item, sharpened.
5. **Commits arrive one per lane message** (1.71 commits/flush, 64 %
   singletons). The only way to carry more per enter is to hold replies
   back — a latency trade the closed-loop row cannot win; not a lever.

---

## 8. Instrument findings

1. **`perf` on the box cannot demangle v0 Rust symbols** (perf 4.18): the
   classifier runs the flat dump through `rustc-demangle` offline (a
   10-line binary against the registry crate) — the field ledger's
   daemon classes are wrong without it (the mangled names miss every
   regex).
2. **`tests/op_trace_stitch.py` needs Python ≥ 3.8** (`statistics.fmean`);
   the box's 3.6 fails at the first table. The traced legs were stitched
   offline from the `.trace.json` drains — the rigs should not run the
   stitch on the box.
3. **The daemon pid match must tolerate a suffixed binary name**
   (`squeezefs.kvmap mount …` is not matched by `[s]queezefs mount`); two
   perf rows were lost to it before the fix (`[s]queezefs[^ ]* mount`).
4. **Per-op divisors**: the R-2/R-3 tables' CPU µs/op divides by fio's
   `ios` (row only) while every stats delta includes the 10 s ramp — the
   row table now prints both (`us/fio-op | us/stats-op`) and divides every
   per-op ratio by the stats population.
5. **`read_fast_dispatch_tests::cold_block_demotes_then_the_tier_resident_block_serves_into_the_dest_window`
   is load-sensitive at `96156b15`** (1 of 3 runs failed on the unmodified
   dev tip with the shared dev box at loadavg 35–40; 2 of 3 on this
   branch): the handler's R1b admission ceremony is dispatched
   asynchronously and the probe races it. Pre-existing; not this
   campaign's; owed a settle in the test (poll the tier gauge before the
   probe).
6. **The shared dev box is not a measurement venue while foreign release
   builds run**: the local A0 row decayed −32 % first→last third, and the
   fix's local sanity row read the box past the knee (the governor refused
   every decision — `refused_busy` 845 k, absorbed 0). The field carried
   every number in this note.

## Landing-law checklist

A-B-B-A — field, arm = binary, both orders around the diet (A1/C1,
A2/C2, then D1/A3/D2 on the shipped default), the spin bracketed inside
the same run (A1 B1 … B2 A2) with the same-binary knob control (C) making
the two levers separable; substrates — the field fabric (nvme-tcp,
nullblk, cache-less); the local tcp devsub ran the ledger and the
functional sanity only (§1 — noise-limited); sustained — 60 s kern legs
on A, B, C and D (flat; C +2.4 %, D +0.7 %) + 60 s il on A and B (par);
amplification — N/A (reads); engagement — exact on every row (the
partition, the byte closure, the spin ledger's `absorbed + expired`);
`read_copy_*` closure — kern zero passes, il `dest_dma + arena ≡
bytes_out`, bounce 0; instrument + substrate + binary + profile
(`release`, thin LTO, every arm) + kernel + tier stated; tripwires 0 on
every row; the box returned unmounted with `/scratch/tmp/sqz-agent/r4/`
removed; artifacts `~/sqz-field-artifacts/2026-09-03/r4-*.tgz`.
