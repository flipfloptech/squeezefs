# 2026-08-05 — Read-throughput campaign: the binding term named (demand-bounded fill concurrency), R2's structural disengagement root-caused, and the engage-governor ahead lane (default-on)

Branch `perf/read-throughput` (worktree off `integrate/zcrx-wave` tip
`e4c7d798`, **unmerged — the orchestrator merges**). Charter: the user's
"revisit read throughput since its still slower then kernel by a large
margin" — kernel-path seq-read 27.4 GB/s sustained vs the 41.8 GB/s raw
fabric ceiling (~65 %) while writes run 34+ (~85 %). Inputs: the
2026-08-06 field row (binary `10c904aa`, fio libaio bs=1M nj16 qd8,
60 s, `/scratch/tmp/logs/wsweep_20260806_005415` — cluster READ-ONLY
this campaign; the field row below is cited from that capture, the
follow-up field row is via report), `.benchmarks/2026-08-01-serve-
decomposition.md`, `.benchmarks/2026-08-01-read-lane.md`,
`.benchmarks/2026-08-02-read-copy-count.md`.

Commits: red `fcd2619a` (contracts) · green `349bb585` (the
engage-governor + sub-start-window routing) · this note.

## 1. THE BINDING TERM, NAMED: demand-bounded fill concurrency

The field row's own deltas close the arithmetic at every level:

* **In-flight fills (Little's law):** 213k fills / 60 s = 3,550 fills/s
  × 5.85 ms `fill_total` = **20.8 in-flight 4 MiB device reads**
  (≈ 87 MiB in flight on the device plane). Delivered fill bandwidth:
  20.8 × 4.19 MB / 5.85 ms = **14.9 GB/s ≡ 894 GB / 60 s** (exact).
* **The raw ceiling row** (bs=4m qd16 × 10 jobs) sustains **160
  in-flight 4 MiB reads → 41.8 GB/s** (implied saturated service:
  160 × 4.19 MB / 41.8 GB/s = 16.1 ms). The FS runs the device pipe at
  **13 % of the raw row's depth** (21 vs 160) for 36 % of its
  bandwidth — and the fill service time (5.85 ms, dev_service 4.44)
  sits far BELOW the saturated 16 ms: the device is under-offered, not
  slow. Anomaly 2's dev_queue 1.12 ms is the `NvmeBlockDev` worker's
  `submit_and_wait(1)` park picking up new channel requests only on a
  completion — a latency adder at ~2-per-device depth that shrinks as
  concurrency rises; the 4096-cap channel is nowhere near full. Not
  the capacity term.
* **Why demand can only express ~21:** 16 jobs × qd8 × 1 MiB = 128 MiB
  of client in-flight, but sequential 1 MiB ops dedupe 4:1 into 4 MiB
  block fills through the single-flight — a job's 8 in-flight ops span
  ~2–3 distinct blocks (sf_wait cohort: 761k of 1.567M ops at 5.4 ms —
  half the ops WAITING on fills), so the demand front can hold at most
  ~16 × 3 ≈ 48 fills and measures 21 with warm serves absorbing the
  rest. **Demand in flight (128 MiB) < fabric BDP (41.8 GB/s × 5.85 ms
  ≈ 245 MB)** — the client's own concurrency structurally cannot cover
  the BDP on this shape.
* **The user-rate closure:** 27.4 GB/s = cold fill-fed serves
  (dest − warm_serve = 798 GB / 60 ≈ 13.3 GB/s ≈ the 14.9 GB/s fill
  stream ramp-adjusted) + warm re-serves (845 GB / 60 ≈ 14.1 GB/s —
  the time_based loop's cross-pass retention + same-block sub-read
  locality: hot 511k + hold 295k serves). The cold half IS the fill
  stream; the row's ceiling tracks fill delivery. 27.4/41.8 = 0.655
  closes as (14.9 + 14.1)/41.8 with the warm term retention-bounded.
* **Copy ledger cross-check (anomaly 3 concurred):** dest = 1.00
  passes/byte, all NT (`nt_read_serve_bytes ≡ dest`), bounce 0,
  `fuse3_read_inplace_replies ≡ ops` — the lawful-copy floor is
  engaged; at %sys 46.8 / %usr 16.7 with tpc lanes ≤ 34 % the copy
  path is not this gap's owner.
* **Fix target arithmetic:** saturation needs in-flight fills ≈
  BDP/block ≈ 245 MB / 4 MiB ≈ **58–60 at unsaturated latency**
  (rising toward 160 as service inflates to 16 ms — Little
  self-consistency). Shortfall over demand ≈ (58 − 21) / 16 streams ≈
  **2–3 blocks/stream of ahead depth** — the governed probe ladder
  reaches that in ~4–6 adopted epochs (2–3 s).

## 2. THE PREFETCH DISENGAGEMENT, ROOT-CAUSED (three stacked causes)

1. **R2's issue bound derives from MEMORY, never bandwidth×latency**
   (`src/routing.rs pipeline_touch`): `resident_share = 50 % ×
   hot_budget / 4 MiB / active_streams`. At the cluster's 128 MiB hot
   tier and `active` 16–34 (the two-epoch gauge counts K=4
   reorder-minted sibling lanes, so 16 jobs gauge above 16), the share
   truncates to **0–1**: `prefetch_issue_admits` either refuses
   outright (0) or allows one landed-unconsumed fill (1). 823 issues
   against 1.567M ops is that trickle.
2. **The landing zone is structurally wrong for the regime:** R2 fills
   land in hot-tier PROBATION — a 128 MiB clock churning at the row's
   14.9 GB/s aggregate fill rate ⇒ **~8.6 ms residency**. Any fill
   landing > ~9 ms ahead of its consumer is evicted first: the
   consume-time detector counted **575 of 823 (70 %) real evictions**
   (post-detector-fix), so AIMD halving + progress-clocked quiescence
   (suppress spans doubling to 64 blocks) kept every lane suppressed.
   **The R-5 spiral defense worked exactly as designed — against a
   landing zone that cannot work at this shape.**
3. **The vehicle built for exactly this regime was opt-in-off:** the
   ahead lane lands in the coverage-retired, ledger-invisible hold
   (retirement-by-consumption — demand fills cannot clock-churn it),
   but `read_lane_depth_blocks(None, ..) = 0` — the 2026-08-01
   falsification generalized past its venue. The falsifying brackets
   ran **32 jobs × qd8 = 268 MB in flight ≥ BDP** (ahead speculation
   correctly lost there); the field row runs **128 MB < BDP**, the
   high-latency/low-qd case the pin was explicitly left for — and
   nothing engages it (`read_lane_fetch_bytes = 0` on the field row).

Local repro (tcp devsub, §4) reproduced both signatures on the base
binary: 16j qd8 → `prefetch_issued` 1,040–1,122 with 63–65 % evicted-
unconsumed (the field's 70 % shape); 4j qd2 → R2 active-but-wasteful
(45–53k issues, 7.3–9.4k evicted ≈ 31–39 GB of wasted device reads per
60 s row).

## 3. THE FIX (red-first; the follow-on the read-lane note §8.2 named)

**The engage-governor: probe-governed ahead-lane depth, default-on**
(`349bb585`; contracts `fcd2619a`):

* `ReadLaneGovernor` gains the write-side `ProbeCore` **verbatim**
  (loom-modeled, weakening-verified — the 2026-07-29 probe-up
  machinery). Delivery = TOTAL completed fill bytes (demand + lane —
  a lane that merely displaces demand fetches reads as dead gain and
  RETREATS with an 8-epoch cool-down: the falsified venue becomes a
  duty-cycle-bounded probe arm, ~1 epoch in 9 at +¼ depth). Saturation
  = readers catching in-flight fills / depth-bound issue exits /
  zero-depth declines (snapshot pattern); headroom = below the
  per-stream R5 cap share and not Red. Unsaturated epochs bleed to
  ×1.0 = depth 0 (the latency guard — low-offered-load mounts never
  inherit streaming depth).
* `probe_governed_depth` maps the multiplier to blocks/stream: ×1.0 →
  0 (**exact hold-only prior behavior until a probe MEASURES a win**),
  each +¼ gain ≈ +1 block, compounding (64→0, 80→1, 100→2, 125→3,
  156→5, 195→8 — pinned tables).
* `pipeline_touch` routes the WHOLE sub-start-window regime
  (`resident_share < 2` — R2's AIMD start, now the shared
  `R2_WINDOW_START`) to the hold-landing lane when armed: a share of 1
  cannot express R2's starting plan and its landings are structurally
  evicted-before-consume (cause 2). Under `SQUEEZEFS_READ_LANE=0` the
  boundary stays the pre-campaign `== 0` — the A0 lever is exact prior
  behavior on BOTH regimes.
* `SQUEEZEFS_READ_LANE_DEPTH` pins verbatim (probe layer dormant under
  a pin — the write-pipeline override precedent); `0` = ahead off (the
  hold-only A/B control). New gauges
  `read_lane_depth_probe_{ups,backoffs}` (stats inode).

Red evidence: compile-red for the new API contracts
(`probe_governed_depth` unresolved; `read_lane_depth_blocks` arity)
plus behavioral red — `lane_engages_below_the_r2_start_window` fails
under the base `== 0` condition ("R2 must not issue hot-landing
speculation below its start window": R2 issued, lane fetches = 0);
`engage_governor_probe_cycle_drives_the_default_depth` red against the
opt-in-0 derivation.

## 4. Local A/B (tcp devsub — the venue-mandated fabric-sensitive substrate)

**Venue (labeled):** 32-CPU dev box, `SQZ_DEVSUB_TRANSPORT=tcp`
substrate (4 memory-backed null_blk mds + 4× 8 GiB zram-zstd oss over
nvmet-tcp localhost), cache-less format (field posture), 4 MiB blocks,
fill = fresh 16 × 1 GiB written through the mount (0.83 GB/s pass-bound
fill). Instrument fio-3.42 via `tests/fio/run_fio_row.sh` (per-row
stats before/after/delta persisted, `/tmp/rbw_rows/`); every row 60 s +
10 s ramp, **cold remount per rep**, interleaved F/B ordering, medians
of 3. Sides: **base** = `e4c7d798` (the worktree base) vs **fix** =
`349bb585`. This substrate's localhost fabric cannot reproduce the
field's 235 µs-class BDP; the two shapes below bracket the mechanism's
two regimes instead.

**S2 — under-offered (nj4 qd2 bs=1M; demand ≪ local BDP — the field
regime's proxy):**

| rep | fix GB/s | base GB/s | pairwise |
|---|---|---|---|
| r1 | 10.89 | 10.68 | +2.0 % |
| r2 | 10.70 | 9.92 | +7.9 % |
| r3 | 10.10 | 9.73 | +3.8 % |
| **median** | **10.70** | **9.92** | **+7.9 % (median-of-side)** |

p99 clat 4.36–4.56 ms (fix) vs 5.41–5.73 ms (base) — **−20 % p99 on
every rep**. Device-fetch economy: `get_obj` medians 103.8k (fix) vs
109.0k (base) = **−4.8 % device fetches for +8 % user bytes** — the
win is waste-kill + pipelining, not extra device spend. Engagement
(fix side): `read_lane_depth_probe_ups` 9–12 with adopts
(`depth_target` up to 14 mid-row), `read_lane_fetches` 1.1–6.2k;
base-side R2 waste (45–53k issues, 7.3–9.4k evicted-unconsumed)
replaced by ~1k issues + ~300 evicted.

**S1 — demand-covered/saturated (nj16 qd8 bs=1M; local device
saturates at 7.2 GB/s, clat 18.5 ms — the 2026-08-01 falsified-venue
shape):**

| rep | fix GB/s | base GB/s |
|---|---|---|
| r1 | 7.25 | 7.24 |
| r2 | 7.26 | 7.27 |
| r3 | 7.29 | 7.28 |
| **median** | **7.26** | **7.27** |

**Par (−0.1 %, inside spread)** — the retreat arm engaging exactly as
designed: probe ups ≈ backoffs (13–14 each, every probe adjudicated
dead and retreated), `get_obj` par (122.3k vs 122.7k — no
amplification regression), and the base's R2 signature (1,040–1,122
issues, 63–65 % evicted) collapsed to 44–48 issues / ≤ 7 evicted.

## 5. Verification

* Red-first: `fcd2619a` red against the base (compile + behavioral,
  §3); green with `349bb585`.
* Suites (serial, `--test-threads=1`, all green): `read_lane_tests`
  (13 — incl. the 4 new contracts), `read_prefetch_pipeline_tests`
  (Phase C re-pinned under the `SQUEEZEFS_READ_LANE=0` lever — the
  sibling-suite precedent; R2's spiral detector still exercised in the
  posture the lever restores verbatim), `read_prefetch_window_tests`,
  `read_tier_admission_tests`, `read_admission_governor_tests`,
  `hot_block_tier_tests`, `read_copy_ledger_tests`,
  `read_tier_refetch_churn_tests`, `read_saturation_tests`,
  `read_stream_transient_tests`, `hybrid_io_tests`,
  `ranged_read_tests`, `mem_budget_tests`, `rebind_starvation_tests`,
  `read_serve_phase_tests`.
* ×10 on the touched async suites (`read_lane_tests` +
  `read_prefetch_pipeline_tests`): 10/10 green, consecutive, final
  binary.
* `cargo clippy --all-targets --all-features -- -D warnings` AND the
  shipped default-features config: clean. `cargo fmt --check`: clean.
  `cargo doc --no-deps`: builds (pre-existing unrelated warnings
  only). No new loom model owed: the only atomic protocol added rides
  the existing loom-included `ProbeCore`; the sat-mark snapshot is a
  single-word racy-tolerant gauge (the `probe_waits_snap` class).

## 6. THE FIELD ROW (via report — cluster READ-ONLY this campaign)

Deploy `349bb585` (rocky8 container pair, KD-7) and re-run the exact
capture row: fio libaio bs=1M nj16 qd8, 60 s + 10 ramp, the
wsweep fileset, cold. Expected movement per §1's arithmetic:

* `prefetch_issued` stays ≈ 0 (R2 correctly declined) while
  `read_lane_fetches` × 4 MiB accounts the ahead stream;
  `read_lane_depth_probe_ups` > backoffs with `read_lane_depth_target`
  settling 2–4; `read_lane_hold_evicted_unconsumed` ≈ 0 (the
  consume-window budget carries 16 streams × depth).
* `block_fetch` est-mean collapses (5.44 ms on 761k ops → the ahead
  lane fronting the demand cohort); in-flight fills 21 → 55+.
* Sequential read moves from 27.4 toward the 41.8 ceiling; the
  fill-side bound clears at ≈ 58 in-flight fills, after which the next
  term (transport ingress queueing, 3.25 ms/op — serve-decomposition
  §6.1) owns the residual.
* Guard rows: qd32 (cohort-stability — the hold's regime, expect ≥
  par), rand-4k cold (governor posture identical — expect par), the
  write rows (untouched machinery — par), `read_amp` ≤ base on every
  row (S2 locally showed device fetches DOWN).

## 7. Client state

No cluster access this campaign (READ-ONLY ruling) — no mounts, no
deploys, no storage-node changes. Local tcp devsub left up
(`/run/squeezefs-devsub-tcp`, product-owned teardown available);
`/mnt/sqz-rb` unmounted at session end; A/B artifacts under
`/tmp/rbw_rows/` (per-row job/json/stats-before-after/meta); A/B
binaries `/tmp/sqz-readbw-bin-{base,fixed}`; base worktree
`/tmp/sqz-readbw-base` (`e4c7d798`).
