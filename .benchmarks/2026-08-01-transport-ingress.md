# 2026-08-01 — Transport ingress economy: the pinned-thread hostage killed, the write inference falsified, and the EXA-shape expectation honestly missed

Branch `perf/transport-ingress` (off dev tip `3cd528b`, **unmerged — the
orchestrator merges**). Charter: the serve-latency decomposition's #1
ranked build (`.benchmarks/2026-08-01-serve-decomposition.md` §6.1) —
Phase 1 the WRITE transport instrument, Phase 2 the mechanism hunt for
the 3.25 ms/op ingress term (queue_wait 1.55 + dispatch_lag 1.70 at the
EXA cold-read shape), Phase 3 the build. Field window
2026-08-01T03:37Z–05:15Z, journaled SESSION START/END + every row in
`/scratch/tmp/agent_runs.log`; artifacts `/scratch/tmp/tingress_campaign/`.

Commits: red `b9e9a11` (write-family contracts) · green `544ecb1`
(`write_transport_phase_ns`) · red `32add5e` (pin-scope + in-place-WRITE
contracts) · green `0e80245` (the build) · this note.

## 1. Phase 1 — the WRITE transport family (shipped, always-on)

`read_phase.rs` generalized to a two-op-class table (one shared
`TransportPhase` enum — queue_wait / dispatch_lag / reply_commit /
transport_total — one table per op class, shared `latency_core` buckets):
the dispatch loop stamps WRITE queue_wait + parks the arrival exactly
like READ (its own atomic, same sequential-loop exactness argument);
`handle_write` records dispatch_lag at first poll and
reply_commit/transport_total at the reply commit. Root stats inode
carries `write_transport_phase_ns` ungated. Suite:
`tests/transport_ingress_tests.rs` contracts 1–3 (shape, phase-exact,
family independence, shared-core bucket tie, stats surface) +
fork-local `transport_phase_tests`. Field engagement exact: every write
row's family n == the row's WRITE count.

## 2. Phase 2 — the mechanism hunt (three counted discriminators)

Local venue (24-CPU box, devsub-**tcp** — 4× zram oss over nvmet-tcp
localhost, cache-less format; instrument fio-3.42, libaio 1M qd8 nj16,
cold remount per row; **scoping evidence per the two-substrate rule** —
this venue is device-bound at ~8 GB/s, so only the TERM is read here,
never row conversion):

| leg | queue_wait | dispatch_lag | verdict |
|---|---|---|---|
| baseline (instrument binary `544ecb1` — pre-fix posture, identical pins to dev) | 3.117 ms | 2.902 ms | the field term reproduces |
| D1: daemon threads `chrt -f 30` | 0.042 | 0.045 | **−98.6 %** — scheduling, not tokio |
| X1: `fuse3-tpcN` affinity widened to all CPUs (pins otherwise intact) | 0.047 | 0.041 | **−98.6 % — lanes alone** |
| X2: + queue workers widened | 0.051 | 0.067 | no further change |
| qd1 idle floor | 0.040 | 0.064 | hops are µs-class when unloaded |

Lane schedstats during the baseline row: **2.3–13.7 s of OS-runqueue
wait vs ~2.4 s CPU per lane per 35 s window**. Mechanism, named: **the
pinned-thread runqueue hostage** — every `fuse3-tpcN` handler lane (and
every `fuse-over-uring-N` queue worker) was hard-pinned to ONE core, so
every cross-thread wake (inbound push → dispatch pop; TPC spawn →
handler first poll) waited ms-class for that one specific core's
runqueue while sibling cores idled.

## 3. Phase 3 — the build (default-on, lever-carrying)

1. **Node-scoped transport-thread affinity** (`crates/fuse3/src/raw/affinity.rs`):
   lanes, queue workers, and the watch thread keep their home NUMA node —
   `node_lanes` grouping, payload-arena `mbind`, `tpc_spawn_on_node`
   locality all unchanged — but may run on any process-mask CPU of it.
   Derivation is pure + runtime-only (process mask × sysfs topology; no
   constants); single-node maps degrade to the whole available set
   structurally; unknown nodes degrade to freedom (a 1-CPU pin is the
   measured failure mode, so freedom is the safe direction).
   **`SQUEEZEFS_FUSE_PIN_SCOPE=core`** is the A0 measurement lever and
   operational escape (the exact pre-campaign posture, contract-pinned
   live at mount level). Lever-carrying (not binary-only A/B) because the
   posture's interaction with cache-hot shapes is workload-dependent and
   the no-regression set needed a single-binary control.
2. **In-place WRITE replies** (the READ P2 arm's twin): armed-session
   WRITE replies are a synchronous COMMIT enqueue from the handler task —
   the reply-channel + reply-task hop (one more cross-thread wake per op,
   on the core-pinned main runtime) is gone. `fuse3_write_inplace_replies`
   is the engagement gauge that keeps it wired (the READ arm's
   silent-disengagement lesson). §5.4 lease law untouched (the commit
   gate still parks while a payload lease lives); per-qid commit
   discipline, wake coalescing (`transport_wake_*`), and batch submit
   byte-identical. No lock-free core changed — no loom owed.

Suites: `tests/transport_ingress_tests.rs` 6/6 (incl. live mount posture
+ engagement + round-trip), fuse3 fork suite 47/47, targeted root suites
(read_serve_phase, transport_concurrency, multi_queue, tpc, tpc_scheduler,
transport_lease_overlong, session_connection_share, numa_affinity,
metrics×2) green; clippy `-D warnings` + fmt clean at both workspace
roots. Local single-binary A/B on the fix binary: node default
0.110+0.106 ms; `core` lever 3.202+2.940 ms (the lever IS the prior
posture, live).

## 4. Field venue (labeled once)

Client squeeze-test (32 CPU / 2 NUMA / dual-200GbE), out of production;
substrate reset-v3 — nullblk 4-wide over **nvme-tcp** (8 × 48 GiB
namespaces `nvme{4,6,8,10,12,14,16,18}n1`), meta `nvme{0,2}n1`,
DATA_ON_MDS=1, cache-less, 4 MiB blocks. Instrument fio-3.36 via
`run_fio_row.sh` (NUMA fan-out, amp columns); 60 s + 10 ramp headline
rows; settle discipline (reclaim queue AND `meta_kv_pending_free` AND
mem level 0 ×3) between write rows; cold = fresh remount on the
cache-less volume. Pairs (rocky8 container builds, KD-7 verified):
**T** = dev tip `3cd528b` (`squeezefs.tip3cd`), **C** = campaign
`0e80245` (`squeezefs.tingress`). Fill: fresh 32×8 g (256 GiB) through
the mount (29.19 GB/s pass — matches the decomposition session's 29.04
anchor). Flatness: per-10 s device-byte series on rd-C1 flat at
144–155 GB/10 s across the window (no decay trend).

## 5. The counted brackets (A-B-B-A, binary-vs-binary)

**READ (gap_probe_read, libaio 1M qd8 nj32, cold):**

| leg (order) | GB/s | clat | read_amp | queue_wait | dispatch_lag |
|---|---|---|---|---|---|
| T1 | 25.72 | 10.401 | 0.699 | 1.480 | 1.548 |
| C1 | 26.02 | 10.287 | 0.681 | 1.246 | 1.770 |
| C2 | 26.23 | 10.205 | 0.691 | 1.114 | 1.472 |
| T2 | 25.32 | 10.566 | 0.717 | 1.533 | 1.614 |

Side medians **26.13 vs 25.52 GB/s = +2.4 %**, order-independent.
Engagement exact every leg (`fuse3_read_inplace_replies` == ops).

**WRITE (exa_write_bw, libaio 1M qd8 nj32; rm + settle + fresh dir per
leg):**

| leg (order) | GB/s | clat | write_amp |
|---|---|---|---|
| T1w | 31.80 | 8.218 | 1.133 |
| C1w | 31.79 | 8.339 | 1.128 |
| C2w | 31.66 | 8.375 | — |
| T2w | 31.95 | 8.182 | — |

**Flat (−0.4 %, band).** C legs' write family (n = 2,116,641 ==
the row's WRITEs; in-place gauge == n, engagement exact): queue_wait
**0.630** + dispatch_lag **0.874** + reply_commit 0.006;
transport_total 6.681; clat 8.339 ⇒ kernel-side residue 1.66 ms.

## 6. Adjudication (honest)

* **Phase 1 delivered its purpose by FALSIFYING the §4.1 inference**:
  the write wall's ~5.3 ms outside-handler leg is **NOT** ~3.3 ms of
  daemon transport ingress. Measured: daemon ingress **1.50 ms**
  (0.63 + 0.87), kernel-side **1.66 ms**, and the remainder of
  transport_total (~5.2 ms) sits INSIDE `fs.write` (lease merge/copy +
  amortized admission park — the decomposition's own w1 numbers, now
  correctly attributed). There is no 5 ms pre-handler prize on the
  write wall.
* **Acceptance bar 1 (queue_wait+dispatch_lag < 0.5 ms at the EXA
  cold-read shape): NOT MET on this venue** — 3.03 → 2.59–3.02 ms
  (queue_wait trimmed ~20 %, dispatch_lag noise-level). The field's
  term is a DIFFERENT mixture than the local capture: a **field FIFO
  ceiling probe** (diagnosis-only, `chrt -f 30` on all transport
  threads, one warm row, journaled) left the term at 1.11 + 1.50 ms —
  on a venue running ~87 % busy (mpstat: 23 usr / 46 sys / 17 iowait /
  7 soft at the read row), thread-priority suppression does not
  collapse it, so the residual is **in-tokio dispatch/lane task
  queueing under CPU saturation** (batched CQE arrivals serialize
  through per-queue dispatch iterations and lane-local poll bursts —
  the ~0.4 ms slice_out class), not OS wake latency.
* **Acceptance bar 2 (read row ≥ ~27 GB/s): NOT MET — and the +40 %
  expectation is falsified by direct experiment.** The FIFO ceiling
  probe row ran **26.00 GB/s at clat 10.30 ms** — statistically
  identical to the untouched row — i.e. even removing the ingress term
  wholesale does not lift this row. The A0-lever leg makes the same
  point from the other side: term 3.74 ms vs node-scope 3.02 ms, row
  26.15 vs 26.0–26.2 — flat. At this shape the row is governed by the
  venue's per-byte CPU work (kernel RX copy + K1 copy + slice-out ≈
  3 copies/byte at 25+ GB/s on 32 CPUs) and by fill-chain overlap
  (cohorts of ~6.6 ops/fill mean only the cohort primary's ingress is
  critical-path); per-op wait terms that overlap other in-flight work
  do not convert at fixed offered load. The decomposition's §6.1
  additive-critical-path arithmetic was wrong, with counted evidence.
* **Where the fix DOES convert (the latency-bound rows):**
  rand-4k cold beyond-budget **+15.2 %** (235,748 → 271,473 IOPS,
  clat 2.04 → 1.77 ms) and qd32 cold 1M **+5.4 %** (21.69 → 22.86 GB/s,
  amp 0.985 → 0.923) — small-op / deep-queue rows where the ingress
  hops sit in series with short serves. il psync 1M: C 31.46 GB/s
  (engagement 0.958, valid) ≥ kernel path — the parity law holds.
* The pinned-runqueue hostage itself is real and killed: −98.6 % on
  any venue with idle in-node capacity (the local discriminators; the
  field's own low-load posture benefits identically), at zero measured
  cost anywhere.

## 7. No-regression table

| row | T (tip) | C (campaign) | Δ | verdict |
|---|---|---|---|---|
| EXA write 1M qd8 nj32 (GB/s, medians) | 31.88 | 31.73 | −0.5 % | band (A-B-B-A) |
| EXA read 1M qd8 nj32 cold (GB/s, medians) | 25.52 | 26.13 | +2.4 % | ≥ par (A-B-B-A) |
| qd32 read 1M cold (GB/s) | 21.69 | 22.86 | +5.4 % | WIN; read_amp 0.985 → 0.923 |
| rand-4k cold beyond-budget (IOPS) | 235,748 | 271,473 | +15.2 % | WIN |
| warm fit-small 4k (IOPS, 2 reps each) | 252,654 / 242,888 | 242,539 / 242,334 | −1.9 % on medians | par — C reps byte-stable, T reps disagree by 4 % (single-rep spread) |
| il psync 1M nj32 (GB/s) | 30.15 (engagement 0.759, **INVALID — labeled**) | 31.46 (engagement 0.958, valid) | — | il ≥ kernel-path both sides; parity law holds on the valid leg |
| read-lane qd32 armed ledger (3cd528b fix check) | — | holds 250 k, serves 217 k, retired 103 k, `read_lane_fetches` 0 (by design), amp 0.923 | — | ledger-visibility fix unmoved on this cache-less venue, as its reasoning predicted |

Loaded soak (the merge-bar leg): campaign pair, 900 s — fio write
1M qd8 nj16 time_based + 8-worker metadata storm (create/write/stat/
rename/unlink/dir-churn) + `syncfs` every 10 s; wedge indicators
(`fuse_op_watchdog_overdue`, `transport_lease_overlong`,
`tpc_lane_redispatches`, `ipc_sessions_poisoned`, `writer_guard_fenced`)
sampled every 30 s — see §8 for the result.

## 8. Loaded-soak result (PASS)

Campaign pair, **900 s**: fio write 1M qd8 nj16 `time_based` (the data
plane, via the row runner) + 8-worker metadata storm
(create/write-4k/stat/rename/unlink + dir churn, running the whole
window) + `syncfs` every 10 s + wedge indicators sampled every 30 s.
Result: **31.09 GB/s sustained for the full 900 s** (clat 4.2 ms,
p99 19.8 ms); every indicator flat at ZERO across the window
(`fuse_op_watchdog_overdue` 0, `transport_lease_overlong` 0,
`tpc_lane_redispatches` 0, `ipc_sessions_poisoned` 0,
`writer_guard_fenced` [0,0], `ipc_descriptor_rejects` 0); zero
non-kernel D-state processes after quiesce; all 8 storm workers alive;
the subsequent umount + daemon exit clean (observed at the standing-pair
restore). One aborted first attempt (35 s in) was a HARNESS defect — a
malformed raw-fio invocation that wrote no data — journaled and re-run
from zero per the counted-run discipline.

## 9. Residual + ranked next steps (for charter revision)

1. **The EXA-shape walls are NOT transport-ingress-bound.** Both walls
   at 1M qd8 nj32 are governed by per-byte CPU work + in-handler
   residency; the FIFO ceiling probe is the counted kill of the §6.1
   expectation. Candidate successors, in measured-leverage order:
   (a) the **write in-handler ~5.2 ms** (lease merge + admission park —
   the write-commit-economy / write-pipeline surface, now correctly
   attributed by the new instrument); (b) the read **fill-issue
   economy** (dev_queue 1.37 + oneshot-wake 1.63 — the decomposition's
   §6.2, untouched by this campaign and, note, the oneshot-wake leg
   lands on the SAME lanes: node-scope may already have trimmed it —
   re-measure before building); (c) per-byte copy count (kernel RX +
   K1 + slice-out) — kernel-interface work, priced in the census.
2. **Ingress term residual (~1.1–1.5 ms under saturation)** is in-tokio
   dispatch/lane task queueing. Economically removable only by a
   deeper restructure (per-op direct dispatch from the reap thread,
   opcode-specialized first-poll on the queue's lane) whose row payoff
   this venue's counted probes bound at ≈ 0 for the EXA shapes and at
   most the rand-4k class for small ops — recommend AGAINST building it
   until a venue/shape where the term is critical-path is named.
3. **Node-scope posture on other fleets**: single-socket or low-load
   fleets get the full hostage kill (local: −98.6 %); NPS-4/CXL shapes
   inherit the numa_core map. The `core` lever preserves the exact old
   posture for any pathological venue.

## 10. Client state

Standing mount (`b4edafc` pair from `/scratch/tmp/squeezefs`) restored
and verified at SESSION END; campaign pair retained at
`/scratch/tmp/{squeezefs,libsqueezefs_il.so}.tingress` (`0e80245`), tip
pair at `.tip3cd` (`3cd528b`); artifacts (per-row job/json/stats
before-after/meta, flatness series, soak indicators + fio json, probe
schedstats/mpstat) under `/scratch/tmp/tingress_campaign/`; row runner +
jobs + analyzer under `/scratch/tmp/fio_tingress/`. Bench filesets
`tingress/` (32×8 g), `wr_ti/`, `warm/`, `soak/`, `mdstorm/` left on the
reset-v3 store (normal bench artifacts). No resets, no reformats, no
raw-device writes, no storage-node changes. One incident, journaled: a
remount raced the D0 writer-guard release and was REFUSED LOUD (the
guard working as designed); the helper gained a guard-release retry.
