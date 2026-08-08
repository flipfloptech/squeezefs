# 2026-08-08 — SHIM IOPS round 5: recalibrated emulator + the internal-time hotspot program

| | |
|---|---|
| **Charter** | The user directive verbatim: "we should be using our micro benchmarks on all functions to see if we can reduce their overall time of execution focusing on hotspots... the machinery tied with the network is limiting our iops and the only way is to lower our internal times since I can't lower the network." Frame: `IOPS = 1,024 ÷ clat`; field clat 1,545 µs = network+device ~330 µs (incompressible) + internal ~1.2 ms (ours). Plus the third-party calibration adjudication (accepted): r4's venue ran ~2.5× slow per-op — its conclusions re-adjudicated here. |
| **Branch / SHAs** | `perf/iops-internal-time` off dev `b92f2133` (contains `a5d47bc6`) — r4 carry `d7c30059`, re-adjudication `d282090a`, perf-rig `0e748cf8`, levers L1+L2 + bench group `7470f740`, this note |
| **Field row (verbatim from the r5 brief)** | 32×32 rand-4k il, binary `82a9999f`+r3/r4 class: **663 k @ clat 1,545 µs**, ingress mean ~1.25 ms, `ipc_direct_inline_reaps` 77 k → **4.0 M** (r3 fusion engaged), lane flush live. |
| **Verdict** | (§5) |

## 1. Emulator recalibration (the calibration law, applied)

**Falsifications recorded first:** netem-on-lo caps ~330–390 k IOPS
regardless of delay (delay-0 control row — the lo root-qdisc lock, not
hrtimers); the r4 additive 300 µs backing timer measured 821.9–1,019 µs
dd inflight vs the field's ~330 µs (the adjudication, confirmed). The
r4 backing also hid TWO geometry caps found while recalibrating: configfs
null_blk defaults (1 submit queue × depth 64 = 256 concurrent — exactly
the measured 827 k raw) and the timer-completion rate ceiling (~850 k/s
per 4 devices; scales sub-linearly with submit_queues, ~1.55× with
device count).

**The calibrated venue** (`SQZ_DEVSUB_INSTANCE=emu`, oss = 8× memory-backed
null_blk, `completion_nsec=240 µs` timer, 32 submit queues × depth 256,
`io_queues=8`): measured AT THE OPERATING POINT (the law's axis — never
a nominal constant):

| probe | value | field target |
|---|---|---|
| rate-capped 672 k through backing | clat **318.6 µs** flat | — |
| rate-capped 1 M through backing | clat **318.4 µs** flat | — |
| rate-capped 1 M through nvme-tcp | clat **316.4 µs** | ≈ 330 µs inflight ✓ |
| open-loop raw 32×32 | 1.049 M @ 957 µs | field 2.486 M — **NOT reproducible locally** (backing timer rate ceiling + loopback stack; documented gap — the venue's raw license is the OPERATING-POINT match, and absolute-IOPS claims stay ratio-only) |

**License row (b) — the equilibrium reproduction verdict (honest):** the
unpatched binary reads **955 k @ ingress 200 µs** here, NOT the field's
663 k @ 1.25 ms. The field's internal time is ~5× the local internal
time at the same shape — a DELAY knob cannot manufacture internal
slowness (real-NIC send path, interrupt pressure). Per the a5d47bc6
companion directive, **a FIELD perf profile of the ingress path is
owed**; it is §6's first row. The internal-time program itself is
venue-robust (per-function ns cuts compose anywhere) and proceeds.

## 2. The r4 re-adjudication (calibrated)

32×32, engagement exact: sweep-only **955 k** vs r4-adaptive **822 k**
(−14 %) vs K=16 **878 k**. Every mid-sweep enter taxes the svc thread
more than issue promptness pays once the sweep cadence is prompt —
**r4's +19 % was its miscalibrated venue's artifact.** Disposition:
sweep-only default RESTORED, the adaptive derivation DELETED
(no-dead-code law), `SQUEEZEFS_IPC_DD_EAGER_FLUSH` stays the counted
measurement lever (`d282090a`).

## 3. The hotspot ledger (calibrated venue, 32×32, engagement exact 32.3 M ops; dwarf, svc + dd + client)

| rank | symbol | svc % | dd % | disposition |
|---|---|---|---|---|
| 1 | `__vdso_clock_gettime` (+`Timespec::now`) | **8.9** | **7.1** | **L1 landed** — 5–6 reads/op → 3 (single-read law) |
| 2 | probe string machinery (`fmt::write`, `push_str`, `StrSearcher::new`, `TwoWaySearcher`) | **~8.0** | — | **L2 landed** — zero-heap StackKeys + parse-carry |
| 3 | moka + sip/ahash hashing (`get_with_hash`, `sip::Hasher`, `hash_one`, `Cache::get`, `do_run_pending_tasks`) | ~7.0 | ~9.4 | filed (L3): 2× moka get/op (probe + revalidate); `NvmeCache::get_static` rides scc w/ sip — hasher swap is the next lever, blast radius = shared read tiers |
| 4 | `mutex_spin_on_owner` (+`lock_contended`) | 2.9 | **7.1+1.0** | post-r3 residue: reaper↔svc on `uring_lock`/shard state — filed |
| 5 | `DirectDriveEngine::submit` / `drain_cq_locked` bodies | 2.8 | 3.6 | protocol work (shrinks via L1/L2 constituents) |
| 6 | `IpcHost::drain_pass` body | 2.0 | — | pass walk — already alloc-free (r3) |
| 7 | `NvmeCache::get_static` | 1.7 | 2.0 | with #3 |
| 8 | kernel block sched (`dd_has_work`, `dd_request_merge`, `wbt_track`) | ~2.8 | ~3.8 | VENUE (mq-deadline + wbt on nullb) — filed as substrate hygiene, not product |
| 9 | `ipc_direct_read_probe` body | 1.4 | — | shrinks via L2 |
| 10 | `BackendRouter::allocator_for_key` | 0.9 | 0.8 | incarnation-probe split — with #3's hasher pass |

## 4. Levers landed (`7470f740`) + bench instruments

* **L1 — the single-read clock law**: ONE `CLOCK_MONOTONIC` ns read per
  boundary crossing — the drain's read closes `ipc_ingress_ns` AND
  anchors `t0_ns` (`DataOp.t0_ns`; the probe pays no read), the submit's
  read closes `admit` AND anchors `inflight`, the CQE pop's read closes
  `inflight` AND anchors `finish`, ONE end read closes `finish` AND
  `total`. 5–6 reads/op → 3. Bench `r5_internal_time/clock`: anchor pair
  39.15 → 35.48 ns (and two fewer pairs per op).
* **L2 — zero-heap probe + parse-carry**: snapshot keys are `StackKey`
  (ino-direct `active_block[_ext]_stack`; new ext form) + inline
  `CompactString` block key; `(be_id, dev_off)` parsed ONCE at the probe
  via the new borrow-form `split_block_key` and CARRIED — the submit's
  per-op re-parse (the StrSearcher/TwoWay term) is deleted. Bench
  `r5_internal_time/probe_keys`: **118.4 → 63.9 ns (−46 %)** and 3
  heap allocs + 1 clone/op → 0.
* Bench group `r5_internal_time` added to `benches/ipc_hop_bench.rs`
  (field shapes in-file per the microbench-program convention).

## 5. Acceptance rows — INTERRUPTED, discarded with attribution

The A-B-B-A was launched twice and BOTH attempts were taken by a
foreign campaign on the box (`sqz-fusedrtt`: its own daemon + 32-job
fio fleets, Tctl 91–95 °C; the first attempt's mountpoint was
unmounted from under the leg, the second attempt's C1 leg ran with the
foreign fleet live — its row read inflight mean 1,138 µs vs the
calibrated 316 µs operating point, i.e. the VENUE was gone). Per the
counted-run discipline those rows are DISCARDED WITH ATTRIBUTION and
never cited; the pre-interference §2 rows (955 k sweep-only baseline
class on the calibrated venue, engagement exact) stand as the
composition reference. **Owed on a quiet box** (one command each, rig
unchanged): the C-vs-B A-B-B-A at 32×32, the 90 s sustained row, the
un-emulated no-regression leg, the from-zero full-suite acceptance
pass, and `tests/run_bench_baseline.sh save` for the intentional bench
improvements (the script's own quiet-box gates refuse the current
state). Gates that DID run to completion between interference windows:
the touched suites ×10 (ipc_direct_drive 18 / ipc_host 37 /
ipc_op_economy 3 / preload_parity 16 — green ×10), clippy
`-D warnings` all-features AND shipped config + the fuse3 workspace,
fmt both workspaces, env-knob convention (the re-graded eager-flush
entry), and the r5 bench group.

## 6. Field spec (squeeze-test untouched — user active)

1. **The owed field perf profile of the ingress path** (the a5d47bc6
   companion directive — the local venue CANNOT reproduce the ~1.2 ms
   internal equilibrium, §1): 10 s dwarf on svc+dd tids mid-row
   (`rigs/2026-08-08-drainfunnel-perf.sh` pattern, engagement-gated),
   plus the standing stat pins per row.
2. A-B-B-A vs the shipped pair at 32×32: the r5 pair's L1+L2 cut
   ~17 % of svc-thread cycles (ledger #1+#2) — at the field's queueing
   equilibrium (ingress = the M/M/1-ish amplification of per-op svc
   time) the row delta is the composition instrument.
3. Projection arithmetic (per the charter frame): field internal
   ≈ 1.2 ms is queueing-amplified svc service time; cutting the
   ledger's landed ~17 % of per-op svc cycles cuts the service time
   ~1/6 — at ρ≈0.9-class utilization the wait falls superlinearly
   (ρ 0.90 → ~0.75-class ⇒ wait ÷ ~2.5). Honest band pending the
   field profile: **clat 1,545 → ~900–1,100 µs ⇒ 0.93–1.14 M IOPS**
   (the 1 M class), with the field profile adjudicating the residual
   ladder (L3 hashing ~7–9 %, dd `uring_lock` residue ~8 %).
