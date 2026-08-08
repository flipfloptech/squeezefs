# 2026-08-08 — SHIM IOPS round 3: THE DRAIN FUNNEL

| | |
|---|---|
| **Charter** | Round 2's field adjudication (`.benchmarks/2026-08-08-shim-reap-fanin.md` §6): `ipc_ingress_ns` attributes ~85 % of the field's 1,561 µs mean clat (32×32 il, 656 k IOPS) to RING INGRESS — ~1,000 of 1,024 in-flight ops pool pre-dequeue while the device runs ~210 concurrent on a 2.486 M raw ceiling. Find and delete the dequeue-side serialization. |
| **Branch / SHAs** | `perf/shim-drain-funnel` off dev `82a9999f` — field addendum `d78cccef`, pass instrument red `98d78737` / green `72dcbc90`, flush split `0977fd6c`, rigs `5b7edc33`, levers `71965b51`, rig pair-support `1c2810bb`, this note |
| **Venue** | tcp devsub, 32-CPU strixhalo, Tctl ≤ 70 °C gates, fio 3.42 dynamic + shim, rand-4k il, 32×qd{8,32}, 60 s + 5 s ramp, fresh format/fileset/remount per leg, engagement FATAL per row (r2 gate set + ingress n ≡ ops). Rigs: `rigs/2026-08-08-drainfunnel-{rig.sh,analyze.py,perf.sh}`. |
| **Verdict** | (§4) |

## 1. Phase 1 — the funnel ledger

### 1.1 The instrument

`ipc_drain_pass_ns` (always-on histogram: every NON-EMPTY svc-thread
sweep's whole ceremony — drains + flush + inline reap; count = pass
count, so `ops ÷ count` = live ops/pass and `mean ÷ (ops/pass)` = per-op
svc-thread WALL cost) + `ipc_drain_flush_ns` (the flush half, so
`pass − flush` attributes by subtraction) + `ipc_drain_empty_passes`
(spin cadence). Contract: `drain_passes_record_duration_and_counts`.

### 1.2 The decomposition grid (r3 pair pre-levers, 32×32, engagement exact)

| leg | config | IOPS | clat mean | ingress mean | ops/pass | pass mean | svc wall µs/op | svc %CPU |
|---|---|---|---|---|---|---|---|---|
| R0 | defaults (12 svc, 8 sessions) | 790,938 | 1,294 µs | 693 µs | 50.7 | 789 µs | 15.6 | ~54 % |
| R1 | `SQUEEZEFS_IPC_SERVICE_THREADS=4` | 726,771 (−8 %) | 1,408 µs | 749 µs | 200.5 | 1,188 µs | 5.9 | ~85 % |
| R2 | `SQUEEZEFS_IL_SESSIONS=12` | 782,598 (−1 %) | 1,308 µs | 701 µs | 50.3 | 789 µs | 15.7 | ~54 % |
| R3 | `SQUEEZEFS_IL_SESSIONS=16` | 773,430 (−2 %) | 1,323 µs | 709 µs | 49.9 | 791 µs | 15.9 | ~54 % |

Falsified: **session width** (R2/R3 no-ops — the charter's "sessions ==
lanes" candidate is dead) and **svc-thread COUNT as the binder** (R1:
one third the threads costs 8 %, and per-op wall FALLS 15.6 → 5.9 µs as
batches grow — the ceremony has a large SHARED-CONTENTION term that
amortizes per batch, not a per-thread capacity wall). The svc threads
are 46–78 % idle while ~700 µs of ingress pools: the funnel is
CONTENTION, not capacity.

### 1.3 The profile (10 s dwarf record on the 12 svc tids mid-row, 51.2 M-op leg)

| term | % of svc cycles | identity |
|---|---|---|
| `mutex_spin_on_owner` + `osq_lock` + `native_queued_spin_lock_slowpath` | **31.2 %** | the dd rings' kernel **`uring_lock`**: `__se_sys_io_uring_enter → __mutex_lock` 16.7 % (the flush-ALL sweep — every svc thread entering every shard's ring) + `get_signal → task_work_run → io_handle_tw_list → __mutex_lock` 9.5 % (TWA_SIGNAL completion task-work delivered onto submitting threads mid-drain) |
| `io_uring_enter` interior (issue path) | ~20 % children | `io_submit_sqes → io_read → blkdev_direct_IO` — the svc thread synchronously ISSUES the bio inside its own enter (+ `blk_finish_plug` 7.3 %) |
| `__vdso_clock_gettime` (+ `Timespec::now`) | **9.3 %** | 4 reads/op on the svc path (ingress stamp read, probe t0, admit close, insert anchor) |
| string build/parse (`fmt::write`, `push_str`, `StrSearcher`, `TwoWay`) | ~7 % | the probe's 3 String builds + `parse_block_key`/`split_key` |
| moka + sip hash | ~6 % | `metadata_cache.get` per op |
| `drain_pass`/`submit`/probe bodies | ~6 % | the actual protocol work |

**The funnel, named:** the svc threads burn a third of their cycles
fighting each other (and the kernel's own completion task-work) for the
12 dd rings' `uring_lock`s, and the loser's spin runs INSIDE the drain
pass — which is exactly the wall-time the rings' waiting ops experience
as ingress. R1 confirms the mechanism from the other side: 4 threads
contend less per enter, per-op wall drops 2.6×.

## 2. Phase 2 — the levers (all landed `71965b51`)

1. **C1 — lane-scoped flush** (default ON, `SQUEEZEFS_IPC_DD_LANE_FLUSH=0`
   = pre-r3 flush-all, registered): a svc thread's sweep-end flush
   enters ONLY its own lane's ring. Every production submitter is a
   lane owner (thread↔lane 1:1 by the shared derivation), so its own
   flush carries its SQEs; foreign-lane stragglers (tests, fallback
   lanes) ride that lane's reaper on its bounded 100 ms EXT_ARG cadence.
2. **C2 — COOP_TASKRUN dd rings**: completion task-work runs on ring
   entry (the reaper's wait — where it belongs) instead of TWA_SIGNAL
   onto whichever svc thread last touched the ring. Pre-5.19 kernels
   refuse EINVAL → plain setup, loudly, once (negotiate-and-degrade).
3. **D — clock economy**: the admit-close/inflight-anchor and the
   CQE-pop/finish-anchor pairs each share ONE `Instant` read
   (`ipc_direct_phase_record_span`), −2 clock reads/op.

## 3. The counted A-B-B-A (C = r3 levers `1c2810bb`, B = dev tip `82a9999f`) + the C1 isolation leg

Engagement exact on every leg (`ops ≡ dd_serves`, ingress n ≡ ops,
tripwires 0). Buckets are ≤-bounds; means are bucket-midpoint.

| leg | qd | IOPS | clat mean | p50 | p99 | **ingress mean** | ops/pass | svc wall µs/op | pass mean |
|---|---|---|---|---|---|---|---|---|---|
| C1 | 8 | 918,382 | 278 | 131 | 1,761 | **40.6** | 5.3 | 11.1 | 59 µs |
| B1 | 8 | 668,965 | 382 | 192 | 1,892 | 161.1 | — | — | — |
| B2 | 8 | 664,406 | 384 | 194 | 1,909 | 161.8 | — | — | — |
| C2 | 8 | 944,170 | 270 | 127 | 1,663 | **39.1** | 5.2 | 10.8 | 56 µs |
| C1 | 32 | 1,125,164 | 909 | 561 | 5,407 | **189.7** | 22.4 | 10.6 | 237 µs |
| B1 | 32 | 907,168 | 1,128 | 897 | 4,014 | 582.7 | — | — | — |
| B2 | 32 | 946,090 | 1,082 | 864 | 3,850 | 560.5 | — | — | — |
| C2 | 32 | 1,246,011 | 821 | 602 | 3,588 | **188.6** | 25.6 | 9.8 | 251 µs |
| CL0 (lane-flush OFF) | 8 | 712,896 | 358 | 192 | 1,696 | 144.9 | 8.1 | 15.5 | 125 µs |
| CL0 (lane-flush OFF) | 32 | 977,010 | 1,047 | 848 | 3,621 | 521.7 | 47.5 | 12.5 | 593 µs |

Sustained confirmation (C pair, 32×32, 90 s, fresh leg): **1,223,389
IOPS flat** (thirds +1.4 %), clat mean 836 µs, p99 3.56 ms, engagement
exact (117.3 M ops ≡ dd serves).

## 4. Verdict + the term arithmetic

* **A-B-B-A medians: 32×8 = 931.3 k vs 666.7 k (+39.7 %), 32×32 =
  1,185.6 k vs 926.6 k (+27.9 %)** — C > B in BOTH orders at both
  depths, clat mean −28 %/−22 %, p50 −34 %/−32 %. The base pair itself
  ran hotter than the r3-instrumented grid (B ≈ 907–946 k vs R0 791 k —
  the grid's binary carried the pass instrument on the FLUSH-ALL
  posture, where the extra pass Instants sit inside the contended
  window; comparisons stay within-bracket per the standing rule).
* **The ingress term collapsed exactly as the profile predicted**:
  mean 161 → 40 µs (32×8), 583/561 → 190/189 µs (32×32) — −67..−75 %.
  Pass ceremony: 789 → ~240 µs mean at 32×32 with ops/pass 51 → ~24
  and per-op svc wall 15.6 → ~10 µs; empty-pass/park cadence up 8×
  (the threads now actually go idle between bursts instead of spinning
  a mutex inside the pass).
* **CL0 isolates C1 as the dominant lever**: lane-flush OFF on the C
  binary gives back most of the win (32×32: 977 k, ingress 522 µs —
  ≈ the base posture; 32×8: 713 k vs 931 k) — C2+D alone ≈ +5..+7 %,
  C1 carries the rest. (C2's share reads directly in the field profile:
  the `get_signal → io_handle_tw_list` term.)
* **One honest regression flag**: C1-qd32's p99 read 5.4 ms (B ≈ 4.0)
  on the FIRST C leg; C2-qd32 (same binary, later order) read 3.59 ms
  — better than base. First-leg-after-format ordering effect (the C1
  legs also show it in thirds); the sustained row's p99 3.56 ms with
  flat thirds is the standing claim. Watch p99 on the field A/B.
* Little's-law closure at C2-qd32: 1024 ÷ 821 µs ≈ 1.247 M ✓; the
  clat mean now decomposes as ingress 189 + total 625 + observation
  ≈ 0 — the funnel term is now SMALLER than the device+engine term on
  this venue, i.e. the next binder is `inflight` again (device at
  ~500 concurrent on zram-tcp), not the drain.

## 4.5 Verification

- Suites ×10 serial, final tree (`ipc_direct_drive_tests` 18 — incl.
  the new pass-instrument contract, `ipc_op_economy_tests` 37,
  `ipc_host_tests` 24, `preload_session_tests` 3): green ×10.
  `env_knob_convention_tests` 21 (the lane-flush knob registered),
  full `cargo test --all-features --test-threads=1` green.
- Clippy `-D warnings` (all-features + shipped + fuse3 workspace)
  clean; fmt clean both workspaces. Preload gate legs 1 + 2 green.
- No lock-free core changed (the flush scope is an existing-atomics
  policy change; COOP_TASKRUN is ring setup) — no loom owed; the
  single-CQ-consumer invariant (`cq_gate`) untouched.
- The KD-7 mid-session lesson recorded: a `-dirty` shim against a clean
  daemon runs silent PASSTHROUGH — the perf leg now GATES on ring-op
  engagement (a profile of a passthrough row profiles nothing).

## 5. Field A/B spec (squeeze-test ACTIVE — untouched; for the user's next window)

1. Same-day raw re-grade, then A-B-B-A vs the shipped pair: rand-4k il
   32×8 + 32×32, 60 s, medians of 3, thirds flat, engagement FATAL
   (incl. ingress n ≡ ops). Per row read: `ipc_ingress_ns` (mean + the
   16–64 ms tail mass — the p99-20 ms accountant), `ipc_drain_pass_ns`
   (count, mean — ops/pass and per-op svc wall), `ipc_drain_flush_ns`,
   `ipc_direct_phase_ns`, svc/dd pidstat.
2. Isolation rows on the C pair: `SQUEEZEFS_IPC_DD_LANE_FLUSH=0`
   (C1 off), and if the kernel is ≥ 5.19 the C2 share reads directly in
   the profile (`get_signal → io_handle_tw_list` gone).
3. Acceptance: field ingress mean 1,330 µs → toward the local
   post-lever class; the 810 k-op 16–64 ms ingress tail collapsing is
   the p99 verdict; IOPS toward the 1 M class (raw ceiling 2.486 M —
   headroom is not the question).

**Projection (arithmetic on the measured constants, adjudicated by the
A/B):** field mean clat 1,561 ≈ ingress 1,330 + device/engine ~330 +
observation ≈ 0. The levers cut the SAME-SHAPE local ingress mean by
67 % (583 → 190 µs) by deleting the uring_lock share of the drain pass
— the term the field profile signature (svc threads near-saturated,
810 k ops of 16–64 ms ingress = contention collapse) says dominates
there too. Transfer arithmetic: ingress 1,330 × (1 − 0.67) ≈ 440 µs ⇒
mean clat ≈ 440 + 330 ≈ 770 µs ⇒ IOPS ≈ 1024 ÷ 770 µs ≈ **1.3 M**;
bounded below by the post-fix svc wall (12 lanes ÷ ~10 µs/op ≈ 1.2 M
dequeue capacity at local wall — the field's per-op wall is the number
to read) and above by the 2.486 M raw. Claim class: **0.95–1.3 M from
656 k (crossing the 1 M class), mean → the ~800 µs class, and the p99
20 ms tail → the ~4 ms class** (the ingress 16–64 ms mass is the tail's
whole accountant and it is contention-made).
