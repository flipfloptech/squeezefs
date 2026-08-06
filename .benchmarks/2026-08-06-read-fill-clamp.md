# 2026-08-06 — The fill-path concurrency clamp: touch-driven-only lane issue (completion-driven refill shipped); the local venue's ceiling adjudicated honestly

Branch `perf/read-fill-clamp` (worktree off `integrate/zcrx-wave` tip
`f8bcdcb4`, **unmerged — the orchestrator merges**). Charter: name the
~50-in-flight fill clamp behind the field's FLAT pinned-depth ladder
(D4/D8/D16 = 22.29/21.94/22.57 GB/s cold, 16 streams, in-flight ≈
22 GB/s ÷ 4.19 MB ÷ 9.64 ms ≈ 50 vs the raw row's 160 on the same
devices), fix by derivation, file the instrument gap. Cluster
READ-ONLY; field rows via report.

Commits: red `fe16d5c0` (contracts 14–15) · green `f12e9a03`
(completion-driven refill + fill_total lane accounting) · this note.
Local artifacts `/tmp/rbw3/` (per-row fio JSON, stats before/after,
2 s in-flight samples).

## 1. THE CLAMP, NAMED — with the audit that cleared everything else

**Systematic bound audit (all cleared):** `NvmeBlockDev` worker ring =
1024 entries/device (`build_worker_ring(1024)`); request channel cap
4096/device (`URING_REQ_QUEUE_CAP`); `ALIGNED_BUF_POOL`/`BUFFER_POOL`
= `max(cores×16, 64)` slots AND overflow-allocate (never block);
`BG_TASK_SEM` = `max(32, cores×8)` = 256 on the 32-CPU field client
with `bg_spawn_rejected = 0` on every row; `STRIPED_IO_SEM` =
`clamp(cores×2,4,64)` but acquired ONLY by the multi-block assemble
arm and write-side paths (1 MiB ops over 4 MiB blocks are
single-block); zcrx is opt-in-off; no other semaphore/`buffer_unordered`
touches the fill path. **The devices take 160 in-flight happily — the
clamp was ours, and it was not a bound at all: it was an issue-cadence
degeneration.**

**Touch-driven-only lane issue.** The pipeline was topped up
exclusively inside `pipeline_touch` (request arrival). On a cold row
the steady state is every stream's whole qd blocked on fills
(field: `sf_wait` 7.4 ms on 699k of 724k `block_fetch` ops — nearly
every op waits), so touches arrive in LOCKSTEP BURSTS right after a
stream's fill lands, and between bursts completions drain the lane
pipe to zero with nothing refilling it. Effective fill concurrency
degenerates to the request-arrival duty cycle — **invariant to the
depth pin**, which is exactly the flat D4/D8/D16 signature and the
probe governor's honest dead-gain verdict. Local repro: the 2 s
`read_lane_inflight_bytes` samples sawtooth `depth-burst → 0 → 0`
while devices sit at qd 1–20 (`/proc/diskstats` ios_in_progress
5/20/1/1 mid-row).

**The closing arithmetic:** per-stream touch bursts arrive once per
fill RTT (~9.6 ms); a burst tops up to depth and the pipe then only
drains, so time-averaged in-flight ≈ streams × (depth × duty + demand
front) ≈ 50 at ANY pinned depth — the number the field measured, and
the reason `dev_service` (4.4 → 7.8 ms) rode a latency curve while
throughput sat at 22 GB/s: the device pipe was never OFFERED more
than the cadence admits.

## 2. THE FIX — converge-by-completion (contract 14, red-first)

`spawn_read_lane_task`'s settle now returns the lane-live verdict and
every COMPLETED settle (fetch, resident-skip, hole, empty-resolve)
re-drives `read_lane_top_up` for its lane — the write-pipeline
admission law's read twin. No new bound and no constant: the refill
re-enters the SAME gates (file-level single issuer, progress-clocked
quiescence, generation fencing, Red, probe-governed depth × streams,
R5 cap) and the **reader-tied horizon still bounds the cursor** — a
stalled reader stops the chain at `edge + 1 + examine_cap`, never EOF
(the round-2 EOF-sprint law intact); an error/stale settle never
re-drives (a failing device cannot self-loop).

Red evidence: `lane_completions_refill_the_pipeline_without_a_touch`
— classify with block 0's four sub-reads, then ZERO further touches;
base issues exactly one top-up (depth 4 = blocks 1..=4, **left: 4**)
and stalls; the contract demands the completion chain walk to the
horizon (block 5, **right: 5**). Green with `f12e9a03`, ×10 stable.

## 3. THE INSTRUMENT GAP (contract 15) — closed exactly

`lane_fetch_block` never recorded `ReadFillPhase::FillTotal`, so the
family's per-fill denominator undercounted the row by the lane's whole
share (field D4: n=24,748 against 372k fills — the demand-arm count
only). Fixed at the fetch funnel; verified EXACT on the local rows:
fix D8 `fill_total` n=158,590 ≡ `fetch_dma` n=158,590; the base row's
undercount closes against the lane share (121,157 + 26,597 lane
fetches + settles ≈ 147,754).

## 4. LOCAL LADDER A/B — the venue's ceiling, adjudicated honestly

**The tcp-devsub venue CANNOT show the slope, on either binary.** The
"raw ceiling" reads on this substrate are polluted by zram
thin-reads (the raw job's own stated caveat): qd1×8j/dev already
"reads" 24.5 GB/s because unwritten pages decompress from nothing.
The WRITTEN-data service curve is what the FS rows ride:
`dev_service` est-mean 12.5 ms at ~30 in-flight (qd1 row) → 22 ms at
~55 (qd8 rows) — an aggregate zram-decompress ceiling ≈ 9.5 GB/s.
Every FS row on BOTH binaries sits at 7.1–9.4 GB/s = 0.85–0.99× that
ceiling, so added fill concurrency has nothing to buy locally — the
same regime as the read-lane campaign's falsified venue, now visible
in the phase table instead of by inference.

| row (16j qd8 cold, session drift ±8 %) | base `f8bcdcb4` | fix `f12e9a03` |
|---|---|---|
| D0 | 8.73 / 7.86 | 8.03 |
| D4 | 9.22 / 7.70 | 7.97 |
| D8 | 8.73 | 9.37 |
| D16 | 8.20 / 9.20 | 8.79 |
| governed | — | 7.13 (probe retreats at the wall — correct) |
| 16j **qd1** (touch-starved discriminator), D4 | 8.08 / 8.02 | 8.13 / 7.88 (clat par 2.05 vs 2.08 ms — no refill latency tax) |

**Mechanism acceptance (what the venue CAN show, all green):**
contract 14 red→green; refill engagement (D8: lane fetches 26.6k →
30.2k, hold serves 68.6k → 87.2k, `read_lane_wasted` 0,
`bg_spawn_rejected` 0, hold retired ≡ holds, evicted 0); instrument
closure exact (§3); no regression on any row (par within the venue's
±8 % drift, both orders sampled). **The slope acceptance belongs to
the field row** — the only venue in this program whose device curve
has headroom above the demand-cadence point (41.8 raw vs 22
delivered).

## 5. Verification

Suites green (serial): read_lane (18 — incl. contracts 14–15),
read_prefetch_pipeline, read_prefetch_window, mem_budget,
read_tier_admission, read_admission_governor, hot_block_tier,
read_copy_ledger, read_tier_refetch_churn, read_saturation,
read_stream_transient, hybrid_io, ranged_read, rebind_starvation,
read_serve_phase, data_path_correctness, reused_key_stale_fill,
staged_identity_visibility, **nvme_dev, nvme_dest_ownership,
backend_health_probe, uring_fs**. ×10 consecutive on read_lane +
read_prefetch_pipeline: 10/10. clippy `-D warnings` both feature
configs; fmt clean. Venue incidents (journaled): the recurring
alternating "cannot create --dir" mount race in the rig scripts
(rows retried; a dir-ready wait added to the driver); the substrate
was rebuilt fresh at session start after the prior session's
exhaustion.

## 6. EXPECTED FIELD GAUGES (pair from `f12e9a03`, the same D-ladder + the governed acceptance row)

* **The ladder SLOPES**: pinned D4 < D8 < D16 cold-row GB/s, with
  in-flight fills sustained near `depth × 16` (the
  `read_lane_inflight_bytes` samples stop sawtoothing to 0 between
  request bursts) and `dev_service` riding the raw row's
  latency-throughput curve as in-flight grows (7.8 ms @ ~50 →
  ~16 ms @ ~160 = the 41.8 GB/s point).
* `read_fill_phase_ns.fill_total` n ≈ total fills (was 24,748 vs
  372k) — the decomposition's denominator is whole-row again.
* `sf_wait`/`block_fetch` est-means collapse as ahead fills
  pre-complete the cohort's blocks; `read_lane_wasted` ≈ 0,
  `read_lane_hold_evicted_unconsumed` ≈ 0 (the round-2 guards stand).
* On the GOVERNED row the probe now has real gain to measure: expect
  `probe_ups > backoffs`, `read_lane_depth_target` climbing past 4,
  and the cold 128 GB row moving 22 → toward the 35.5 bar; if it
  plateaus again, the samples + phase table now name the residual
  term directly (dev_service on-curve = fabric; off-curve = the next
  client-side term).
