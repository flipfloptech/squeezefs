# PR 5 — R2 per-stream pipelined prefetch: the reads>writes inversion (2026-07-12)

**Branch:** `feat/read-path-pr5` (base dev @ `299eb98`). Design: `docs/design-read-path.md`
§5.5 + the PR 5 plan entry. Lineage: PR 1 baseline
(`.benchmarks/2026-07-11-read-path-baseline.md`), PR 3
(`…-read-path-pr3-hot-block-tier.md`), PR 4 (`…-read-path-pr4-tier-admission.md`).
Substrate/protocol verbatim from the baseline note (3.5 GHz cap, 8 GiB cage,
`taskset 0-15`, quiet-gate, Tctl < 61 °C across all runs, fresh format per session,
counters via `.stats` + `/proc/<pid>/io` ledgers; raw artifacts
`~/tmp/sqperf/results/p5{e,f,g,h,k,l,m,n}_*`).

## THE GATE — row 2 cold seq read ≥ same-session row 1 fresh write

| Session (fresh format each) | Row 1 write | Row 2 cold read | Ratio | Verdict |
|---|---|---|---|---|
| **Run 1** (`p5k`) | 3 867 MiB/s | **6 598 MiB/s** | **1.71×** | **INVERTED** ✓ |
| **Run 2** (`p5m`, t1-first ordering) | 4 018 MiB/s | **4 419 MiB/s** | **1.10×** | **INVERTED** ✓ |
| (pre-final-binary confirmations: `p5g` 4 075→6 593 = 1.62×, `p5h` 3 986→6 468 = 1.62×) | | | | |

PR 1 recorded 787 vs 4 282 = **0.18×** — the program's headline shape. PR 5 lands the
inversion at **1.71×** (row 2 = **8.4× PR 1's row 2**). Run 2's ordering follows PR 1's
run-B protocol (single-stream t1 between row 1 and row 2): its row 2 carries t1's ghost
records for f1 (511 ghost-hit publishes = 2.0 GiB of tier writes mid-read — the
convergence tax paid exactly once, visible as first-done 6 445 vs last-done 4 419), and
still inverts. Never-mode (`p5n`, no admission at all): 3 708 → **6 537 = 1.76×**.

Row-2 counter shape (run 1, `p5k`): `get_obj` +4 093 for 4 096 unique (**0.999×**);
`prefetch_issued/completed` 4 086/4 086; **`prefetch_wasted` 0**;
**`prefetch_evicted_unconsumed` 5**; ghost hits 5; publishes skipped 4 088; foreground
waits 2 657 (the growth signal working); `window_hwm` 4 (the contention-scaled share cap:
50 % × 256 MiB hot ÷ 8 streams ÷ 4 MiB = 4 — the AIMD never even needed the 16 cap);
device ledger 16 372 MiB read for 16 384 MiB user (**1.00×**), 20 MiB written.

**Single-stream row (t1, clean cold, `p5m`):** **5 804 MiB/s** vs the 1 060 lineage
(**5.5×**); 2 044 MiB device read for 2 048 MiB user (1.00×), zero writes, window grew
2→8, `get_obj` = issued+foreground = 519 for 512 blocks.

## The three inherited PR 4 obligations

1. **Evict-before-consume control — CLOSED.** Per-lane resident-unconsumed accounting
   (consume-cursor per block, not per request), contention-scaled `effective_window`
   (two-epoch increment-only `active_streams` gauge, exported), consume-time
   evicted-unconsumed detection (residency probe across hot/read_lru/NVMe/in-flight,
   at most one probe per consumed block, issued-span-only), AIMD ÷2 + **progress-clocked**
   quiescence (suppression span 2^(streak+1) blocks of consume-edge advance, cap 64;
   streak resets on a clean consume). `prefetch_evicted_unconsumed` = 5–13 across every
   clean row-2 run (≈ 0 as the gate demands); the multi-stream contention phase
   (`tests/read_prefetch_pipeline_tests.rs` phase D — the doc's one home) pins the
   bounded spiral. **This is what let the admission default flip to `second-touch`**
   (PR 4's pin test updated: `default_admission_is_second_touch`).
2. **Interleaved-scan restoration — CLOSED, better than PR 3.** Lineage protocol
   (fresh format → row 1 → warm slice A → 3 timed re-read iterations vs a concurrent
   16 GiB cold scan): **11 924 → 12 659 → 12 464 MiB/s** (run A) / 8 886 → 8 801 → 9 300
   (run B, scan overlap covering more iterations) vs PR 3's 12 084 → 9 957 → 11 687 and
   PR 4's **0.87 → 1.8 → 0.44 collapse**. No pollution collapse in either run; the
   concurrent scan itself finished at 7 255 (A) / 1 189 (B) MiB/s vs the ~900 MiB/s
   PR 3-era polluter — the safe replacement for PR 3's accidental (OOM-prone)
   consumption-promotion scan resistance is the pipeline + admission + grace stack, and
   it also makes the polluter itself 1.3–8× faster.
3. **Overshoot → ~1.0× — CLOSED.** Never-mode row 2 (`p5n`): device reads
   **16 428 MiB for 16 384 MiB user = 1.003×** (PR 4 recorded 1.86× hot-evict refetch
   churn), zero tier writes, `evicted_unconsumed` 11. The paced windowed fetches keep
   fills ahead of the reader instead of racing the clock behind it.

## Warm rows (gate: within 10 % of PR 3)

| Row | PR 3 | PR 5 | Verdict |
|---|---|---|---|
| warm-re-read ×2 (converged) | 17 628 / 17 406 | **19 499 / 19 495** (`p5l`, zero device I/O) | **+11 %, improves** ✓ |
| interleaved-scan variant | 12 084 → 9 957 → 11 687 | **11 924 → 12 659 → 12 464** | **restored, no collapse** ✓ |

Convergence note, stated honestly: under `second-touch` the warm set needs **one more
pass** than `always` did (pass 1 records, pass 2 ghost-hits + publishes at 627 MiB/s —
the awaited-publish pass, pass 3+ serve at 19.5 GiB/s with zero device I/O). The
steady-state warm row beats lineage; the transition pass is the documented price of the
16.9 GiB-per-16 GiB streaming tax kill.

## Rows 1/3/4/5 (gate: flat)

| Row | PR 1 lineage | PR 4 default | PR 5 (`p5k`) | Verdict |
|---|---|---|---|---|
| 1 fresh create | 3 817–4 282 | 3 806–4 124 | 3 867 (+ 3 532–4 036 across sessions) | flat ✓ |
| 3 rand-4k read | 326–331 IOPS | 325 | **369** | **+12 %** ✓ |
| 4 overwrite | 649–869 band | 762 | 815 | flat/in-band ✓ |
| 5 rand-4k write | 63–146 band | 126/130 | 129 | in-band ✓ († see below) |

† **Row-5 cage instability — PRE-EXISTING, A/B'd.** `p5k_row5` returned its 129 IOPS but
the daemon hit the 8 GiB cage at the run's tail (kernel OOM record, anon-rss 8.3 GiB); a
standalone row 1→row 5 reproduction on this tip died mid-run (`p5o`, 19 s), and the
**identical shape on the PR 4 binary (`299eb98`, `p4ab`) died the same way** (28.9 s,
anon-rss 8.33 GiB — same class, same magnitude). Rand-4k-write RMW memory behavior under
the cage is a write-path issue this read-path PR does not touch; recorded for the program
ledger (the PR 4-era 616/618 cage-cascade class).

## What it took beyond §5.5's letter (each measured, each pinned by a test)

The pipeline mechanism itself (lanes → issue path → single-flight fetches → hot-probation
fills) landed in one commit and was **not** the hard part. Four response-mechanism
corrections were forced by live rows, in order:

1. **Consume-time residency detection** (`ff0b14e`) — the first detector (serve-exit
   "covered" predicate) counted the reader's own plan-rebase spans as evictions:
   ~1 700 false `evicted_unconsumed` per 16 GiB pass froze lanes via quiescence
   (issued ≈ 33, row 2 at 1.0 GiB/s). Replaced with a residency probe over
   hot/read_lru/NVMe/in-flight at consume-edge advance, `issued_base`-bounded; plus the
   settle-path unconsumed increment gated on the consume edge (slow fills completing
   behind the reader otherwise leak window budget forever — a permanent lane stall).
2. **Progress-clocked quiescence + shed rollback** (`8d553ce`) — the 2 s wall-clock
   AIMD arm froze healthy lanes wholesale (69 detections → issued 283/4 096, row 2 at
   0.64× row 1). Suppression is now denominated in stream progress (geometric span,
   streak reset on clean consume). `spawn_bg` reports shedding and the issue loop rolls
   back lane accounting (phase G pins convergence under a saturated pool). The
   contention formula's `.max(1)` floor removed: share truncating to 0 IS the off signal
   (phase D: 104 → bounded). This commit also tried a ghost **bypass** for speculative
   fills —
3. **— which overcorrected and was reverted** (`e42b8dd`): with the pipeline fetching
   every block of every pass, bypassed fills meant re-read streams **never** admitted to
   the tier — warm-re-read ran device-bound forever (9.2 GiB/s vs 16.6 lineage; R-1's
   stack broken). Speculative fills keep full ghost semantics; same-pass fake heat
   (614 fake hits → 4.5 GiB mid-read publishes, measured) is prevented **mechanically**
   by items 2 and 4 (one pass ≈ one miss per key — phase A's zero-ghost-hit purity pin;
   phase A2 flipped to assert cross-pass convergence ≥ 14/16).
4. **One-lap clock grace for speculative fills** (`61d0644`) — the residual 595-refetch
   overshoot (row 2 at 0.90× row 1 with every policy counter clean) was structural:
   consumed stream residue re-arms the clock's `referenced` bit at every sub-read serve,
   while pipeline fills inserted plain-probationary carry no second chance — the clock
   evicted the pipeline's **future** to keep the stream's consumed **past**.
   `put_probationary_referenced` (probation class + one lap, never sticky; victims still
   source-drop) gives fills clock parity; FIFO then evicts consumed-older first.
5. **Dehydration dedupe at the channel mouth** (`82af5e8`) — first full-ladder run:
   rand-4k row at **37 IOPS** (vs 326 lineage; same-binary A/B `always` = 310). Under
   second-touch every ghost-admitted fill publishes at fetch time AND lands protected in
   hot, so ~100 % of hot evictions were protected victims re-writing tier-resident bytes
   (duplicate multi-MiB `spawn_blocking` writes + channel-parked payloads under the cage
   — PR 4's "third head" in a new coat; `always` never hit it because its hot puts are
   probationary source-drops). The worker now probes tier residency (index-only) and
   drops duplicates (`hot_block_dehydrate_skips`, new stats field). Row 3: **369 IOPS**.

**Deleted (forward-only, no compat):** the legacy 9-ahead prefetch cursor
(`should_prefetch_after_striped_read`, `schedule_striped_prefetch`,
`sequential_read_state`, `PREFETCH_BLOCK_CONCURRENCY`) — the classifier lanes own the
role; GDS materialize + tier-resident `MADV_WILLNEED` arms preserved verbatim in the
pipeline task. Env: `SQUEEZEFS_READ_PREFETCH_WINDOW` (cap 16, `0` disables),
`SQUEEZEFS_READ_PREFETCH_SHARE_PCT` (default 50). Stats: the 8 `prefetch_*` fields per
§Observability + `hot_block_dehydrate_skips`.

## Test evolution in this PR (red-first)

`tests/read_prefetch_pipeline_tests.rs` — one counter-isolated test fn (churn-suite
discipline), phases: **A** clean-stream dedupe/zero-waste/zero-evicted/accounting +
ghost-purity; **A2** cross-pass warmth convergence (≥ 14/16 admitted after purge);
**B** abandonment (≤ 16 extra issues); **C** evict-before-consume under protected
pressure (detection + < 2× bound); **D** 4-stream contention (< 2× collectively);
**E** `WINDOW=0` kill switch; **F** lane-leak self-repair (gauge ≤ 1 within 2 epochs);
**G** shed-task accounting convergence. Plus:
`hot_block_tier_tests::probationary_referenced_fill_survives_one_lap_but_stays_probation_class`,
`read_tier_admission_tests::dehydration_skips_tier_resident_protected_victims`,
`default_admission_is_second_touch` (flipped pin), churn fixture pinned to `WINDOW=0`
(its `get_obj` assertions byte-identical), `bg_admit_tests` updated for the deleted
constant + `spawn_bg -> bool`.

## Gates

- **Full cargo gate (tip):** clippy `--all-targets --all-features -D warnings` clean;
  `cargo fmt --check` clean; `cargo test --all-features -- --test-threads=1` full-project
  green (exit 0, 432 s); `cargo doc --no-deps` 0 warnings; `cargo bench --benches --
  --test` Success. Pipeline suite additionally 5× consecutive green at each mechanism
  step.
- **`FSTESTS_QUICK=1` (`MEMMAX=8G`):** 19 ran — failures {**generic/003, generic/213**}
  = the documented platform expected-fail set, plus **generic/618 in-suite** =
  the PR 4-era accumulated-state cage cascade: **618 standalone on this tip passes 3/3**
  (A/B recorded; same attribution as the PR 4 gate).
- **LTP syscalls (`MEMMAX=8G`):** **PASS 174 / FAIL 0 / BROKEN 0 / SKIP 9** ✓.
- **Churn suite:** `get_obj` assertions byte-identical (fixture pinned to
  `SQUEEZEFS_READ_PREFETCH_WINDOW=0` — request-driven shape preserved verbatim). ✓
- **074-family / reused-key / stale-fill suites:** green. ✓
- Loom: judged not required — every new atomic is single-word `Relaxed` racy-tolerant
  (lane fields, gauge, ghost, grace bit); no cross-word invariant, per the doc's
  loom-scope note.
