# Read-path program — closing report (2026-07-12, PR 8)

**Tip measured:** dev @ `3c89cd7` (PR 7 merged; PR 8 adds docs only — the binary under
test is the finished program). **Baseline:** `.benchmarks/2026-07-11-read-path-baseline.md`
(PR 1, dev @ `4f1c897` content). Protocol verbatim from the attribution doc / PR 1: AMD
RYZEN AI MAX+ PRO 395 (32 CPUs) **capped 3.5 GHz (untouched)**, kernel 7.1.3-2-cachyos,
`taskset -c 0-15`, rustc/cargo quiet-gate per timed row, Tctl 52–67 °C (rail < 80 °C),
daemons caged `systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=0`, 4 × 8 GiB
file-backed `sqdata` + 1 GiB `sqmeta`, staging declared at format, mount
`--read-mem-cache-size 1G --write-mem-cache-size 1G`, fresh format per session, elbencho
last-done column. Raw artifacts: `~/tmp/sqperf/results/p8{a,b,c}_*`.

## The closing row table (fresh, same-session, tip) vs the PR 1 baseline

| Row | PR 1 baseline | Closing (tip) | Change |
|---|---|---|---|
| 1 fresh seq write (8t × 2 GiB, 1 MiB O_DIRECT) | 3,817–4,282 (program band 3,488–4,153) | 3,441 / 3,433 / 3,426 † | flat (see †) |
| **2 cold seq read** (same shape) | **787** | **6,512** (A) / 4,151 (B, t1-first ordering) | **8.3×** |
| **row 2 ÷ same-session row 1** | **0.18×** | **1.89×** (A) / 1.21× (B) | **the inversion — reads > writes** |
| **3 rand-4k read** (qd16 × 8t, 30 s, cold 16 GiB) | **302–331 IOPS** | **59,545 IOPS** | **~195×** |
| row-3 device-read amplification | ≈ 730–1000× | **0.98×** (6,844 MiB device / 6,975 MiB user) | design-owned gate ≤ 2× ✓ |
| row-3 tier writes during the read | 27.7 GiB | **0** | eliminated |
| 4 seq overwrite | 800–869 | 831 | in-band ✓ |
| 5 rand-4k write (8 GiB cage) | 63–108 (and the OOM-kill class) | 130, rc=0, **daemon alive**, `mem_budget_red_events`=1 | in-band + survives ✓ |
| single-stream cold (t1) | 1,060 | **5,196** (ledger 2,064/2,048 MiB = 1.008×) | **4.9×** |
| warm-re-read (slice A ×2) | 16,588–16,915 | 19,430 / 19,195 | +15 % |
| interleaved-scan variant (×3 iters vs concurrent 16 GiB cold scan) | 9,098 → 9,341 → **758 (collapse)** | A: **11,619 → 12,812 → 12,655**; B: 7,816 → 9,389 → 8,992 (scan itself 7,189 / 1,182) | **no collapse** ✓ |
| raw-substrate controls (same session) | seq-w 5,204 / seq-r 6,603–6,605 / rand-4k 363–815 k | seq-w 5,290 / seq-r 6,485 / rand-4k **395,159** | control ✓ |
| FUSE-round-trip fallback control (warm hot-tier rand-4k, zero device work — R-10) | — | **294,275 IOPS** (device ledger 0) | recorded ✓ |

† Row 1 sampled 3,426–3,441 across three sessions on a Tctl-52–67 °C box — 1.4–1.8 %
below the program band's low edge (3,488), within the band's own session noise (PR 3
recorded 3,557–4,214 the same way; the write path is untouched by this program — the
write-through counters and ledger are byte-identical in shape: 16,593 MiB device writes
per 16 GiB, zero reads). The inversion verdict uses the SAME session's row 1, so it is
insensitive to this drift.

## Program-gate verdicts (Goals #1)

**Gate A — the headline inversion: PASS.** Cold sequential striped reads beat the same
session's fresh sequential writes: **6,512 vs 3,441 MiB/s = 1.89×** (clean ordering, run
A); 4,151 vs 3,433 = 1.21× (run B, t1-first ordering — its row 2 pays the recorded
one-time ghost-convergence publishes for the t1-warmed file, 1.9 GiB of tier writes
mid-read, and still inverts). Against the baseline's own framing (row 2 ≈ 4.4–5.3× of
787 needed): landed at **8.3×**. Mechanism ledger for run A: `get_obj` 4,108 for 4,096
unique (1.003×), `prefetch_issued/completed` 4,072/4,072, `prefetch_wasted` 0,
`prefetch_evicted_unconsumed` 7, `read_fill_publishes_skipped` 4,088 (the 16.9-GiB-per-
16-GiB tier-write tax: gone — device writes 65 MiB), window hwm 4 (share-capped).

**Gate B — rand-4k device-IOPS-bound, two parts (R-10 judged here, cumulatively):**
- **Design-owned hard gate: PASS with margin.** Amplification **0.98× ≤ 2×** (was
  ≈ 730–1000×) and **59,545 IOPS ≥ 30× baseline** (gate floor ~9,180; landed ~195×,
  6.5× above the floor). Zero tier writes; `ranged_reads` 1.75 M ≈ one 4 KiB window per
  op; bounces 0; rebinds 0.
- **Substrate-coupled target (≥ 50 % of the same-session raw control): NOT MET —
  15.1 %** (59,545 / 395,159). The pre-agreed R-10 fallback control isolates why: the
  FUSE-round-trip-bound row (warm hot-tier 4 KiB reads, same transport, **zero device
  work**) measures **294,275 IOPS = 74 % of raw** — the transport round trip alone
  forfeits a quarter of the raw control, and row 3 runs at 20.2 % of that transport
  ceiling. The residual per-op cost past the transport is binding resolution (metadata +
  block-map lookup + current-binding recheck per ranged serve — the 074-family
  correctness discipline this program explicitly keeps). Per Goals #1's own text — "the
  design owns amplification; it does not own the transport round trip" — the
  **cumulative judgment is: hard gate passed decisively; the 50 %-of-raw stretch target
  is recorded as not met, with both controls committed** and per-op resolution cost
  named as the (out-of-program) follow-up lever.

**Goal 2 — single-flight contract: PASS, strengthened.** Waiters serve from the carried
`FillResult` (2,556 waiter-result serves on the closing rows); churn suite `get_obj`
assertions byte-identical throughout the program.

**Goal 3 — zero correctness regression: PASS.** 074/075/091/616/617 re-run green (PR 6);
`reused_key_stale_fill`, `staged_identity_visibility`, `data_path_correctness`, churn,
hole-read suites green on every PR tip and the closing tip; `stale_binding_rebinds` 0 on
the closing rows. (The program also FIXED a latent product-wide bug the pins exposed:
unframed transform images made every cold device read of compressed/encrypted blocks
fail — PR 6's `9ff64c2`.)

**Goal 4 — no write-row regression: PASS** (rows 1/4/5 above; write-through ledger shape
unchanged).

**Goal 5 — warm workloads keep their tier: PASS.** Warm-re-read +15 % over baseline;
second-touch ghost admission converges (16 admissions on run A's re-touched blocks);
interleaved-scan restored with no collapse — and the polluting scan itself now runs
7.2 GiB/s (was ~0.9).

**Goal 6 — bounded memory by authority: PASS with named residual.** The attribution
row-5 cage scenario completes alive with Red events and sheds (PR 7 traces; re-confirmed
on the closing row 5); the PR 3 3 GiB-cage thrash knee is gone (4,485 → 113,549 IOPS).
Residual: the CoW-KV metadata-core allocation flood (below).

**Goal 7 — AGENTS non-negotiables: held.** All device I/O through the uring workers
(ranged reads included); no new blocking locks on the read path (scc/moka/ArcSwap/
single-word relaxed atomics; loom judged not required at every step — no cross-word
invariants); no dead code (probes removed in-session); TDD red-first per PR; bench smoke
in every gate.

## /proc/<daemon>/io ledgers (closing, set p8a)

| Row | User I/O | Device READ | Device WRITE |
|---|---|---|---|
| 1 | 16 GiB w | 4 MiB | 16,593 MiB (1.01×, write-through) |
| 2 | 16 GiB r | **16,416 MiB (1.00×)** | **65 MiB** (was 16.5 GiB — the tax) |
| 3 | 6,975 MiB r (1.79 M ops) | **6,844 MiB (0.98×)** | **0** (was ~28 GiB) |
| t1 | 2 GiB r | 2,064 MiB (1.008×) | 20 MiB |
| 5 | ~15 MiB w | — | rc=0, daemon alive, red=1 |

## Counter appendix (closing tip, cumulative at end of p8a rows 1–3)

`singleflight_waiter_result_serves` 2,556 · `hot_block_hits` 41,441 / misses 4,104 ·
`read_fill_publishes_skipped` 4,088 · `read_tier_admissions` 16 (= ghost hits 16) ·
`prefetch_issued/completed` 4,072/4,072, `wasted` 0, `evicted_unconsumed` 7, hwm 4 ·
`ranged_reads` 1,751,956, `ranged_read_bytes` 7.18 GB, `bounces` 0, `rebinds` 0 ·
`stale_binding_rebinds` 0 · row-5 authority: `mem_budget_red_events` 1, sheds fired
across parked/hot/pools/metadata-cache components (per-component ledger in
`p8a_row5.stats.*`).

## Follow-up dispositions

| Item | Source | Disposition |
|---|---|---|
| **CoW-KV metadata-core allocation flood** (aged daemon, setattr storms: RSS +1.3 GiB/1.3 s with data I/O frozen, journal ~1.3 k entries/s) | PR 7 fingerprint (`p7_fast_trace`) | **Open — named for the metadata program.** Outside this program's §5.7 component inventory; the §5.7-shaped fix is a per-volume KV write-path gauge + admission. Until then it surfaces only as the QUICK tier-tail capacity flake (below). |
| Row-3 per-op resolution cost (two map resolutions per ranged serve) | R-10 closing judgment | **Open — out of program scope** (transport + resolution, not amplification). Named lever: a per-file binding snapshot cache keyed on map generation. |
| LBA probe for ranged windows (ship conservative 4096) | approved OQ #1 | **Closed — no trigger:** `ranged_read_unaligned_bounces` = 0 on the closing rows (elbencho O_DIRECT shape); per-backend probe deferred until a workload shows real 512-native waste. |
| Ghost-table sizing / 2-way tag set / time-based window | approved OQ #2 (+ §5.3 pre-agreed upgrade) | **Closed — no trigger:** interleaved-scan variant restored above PR 3 with the 2¹⁶ direct-mapped table; the 2-way upgrade and the `read_tier_admission_*`-driven window revisit stay recorded, untriggered. |
| Per-stream prefetch rings (bypass hot tier) | approved OQ #3 | **Closed — no trigger:** the contention phase and closing rows show bounded overshoot (`evicted_unconsumed` ≈ 0–7) under the shared-probation model. |
| jemalloc watch via mallctl | approved OQ #4 | **Stays out, as approved.** The RSS sampler covers detection; PR 7 added the Red-tick `arena.<all>.purge` as a *shed lever* (not a watch). Watch trigger unchanged: windowed RSS sustaining ≳ 10 % above the gauge sum on quiet workloads. |
| Sub-block single-flight (per (key, window) dedupe) | Alternatives D | **Rejected as designed** — closing amplification 0.98× without it. |
| QUICK tier-tail capacity flake (618-class, victim rotates) | PR 7 note + harness ledger | **Mitigated + attributed:** two root causes fixed in PR 7 (eviction-channel byte-bound/arm-on-take; Red allocator purge) → first fully-clean QUICK runs of the program; residual intermittent single-kill is the KV-core flood above (harness ledger: "NOT a leak and NOT new; pristine dev peaks HIGHER"). Disposition tracked with item 1. |
| Full `-g auto` fstests, full LTP, `run_elbencho_mount.sh` | PR 8 plan entry | **Nightly/release tier per AGENTS test tiering** (wall-clock-bound suites); this gate ran QUICK + LTP per the closing instruction. The K7-style full sweep remains the standing nightly obligation. |

## Suite results on the tip (PR 8, docs-only — the binary is PR 7's)

- **Cargo gate:** full serial suite **0 failures**; clippy `--all-targets --all-features
  -D warnings` clean; `cargo fmt --check` clean; `cargo doc --no-deps` 0 warnings;
  `cargo bench --benches -- --test` green; churn suite byte-identical.
- **LTP (`MEMMAX=8G`): PASS 174 / FAIL 0 / BROKEN 0 / SKIP 9** ✓.
- **fstests QUICK (`MEMMAX=8G`), three tip runs:** {003, 213, 616, 618} · {003, 069,
  074, 213, 616, 617} · {003, 213, 616, 618}. {003, 213} = the documented platform
  expected-fail set. Every other failure in all three runs is ONE cage kill per run in
  the late soak block plus its mount cascade — the kernel record is the identical
  KV-flood fingerprint each time (anon-rss 8.33 GiB, uid 0, the PR 7 `p7_fast_trace`
  shape), and the middle run's 074 entry is verified as post-kill TRUNCATION (fstest.3/4
  sections never ran; zero stale-fill/BAD-DATA signatures — the signature the harness
  ledger says would be a regression). Disposition: the KV-core allocation-flood
  follow-up above (metadata program); the class is dispositioned, not attributed away —
  PR 7's fixes took full QUICK from deterministic-fail to intermittently fully clean
  ({003, 213}-only runs recorded on the PR 7 tip), and the standalone soak trio +
  10-minute 1.17 M-op fsx reproduction pass on a fresh caged daemon.
