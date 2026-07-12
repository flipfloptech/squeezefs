# Read-path PR 2 — result-carrying single-flight (R1a): gate evidence

**Design:** `docs/design-read-path.md` §5.2 / PR 2 entry. **Commits:** `8a882bd` (red
contract phases E–H) + `cdbfab6` (implementation). **Base:** dev @ `8c3f585` (PR 1
baseline). Perf-contract: **neutral** (rows 1–3 within spread) — this PR re-plumbs waiter
correctness only; publish behavior is byte-identical (admission is PR 4).

## Mechanism landed

`inflight_block_reads` carries `broadcast::Sender<Option<FillResult>>` (capacity 4, R-2);
the primary broadcasts `Some(FillResult { bytes: refcount clone, serve_valid })` after the
publishes + final incarnation still-check and flips the guard to `completed`; guard drop is
close-only on success, `None`-then-close on fetch-error and future-drop (the three §5.2
cases). Waiters served from the carried result (`singleflight_waiter_result_serves`, on
`.stats`); `None`/`Lagged`/`Closed`/slice-timeout keep the bounded re-check loop (60 s
deadline unchanged). `TEST_TIER_PUBLISH_DELAY_MS` seam (one relaxed load per ≥64 KiB
publish, `FAIL_NEXT_WRITES` precedent). No new locks; no new atomic protocol ⇒ loom not
required (per the design's note). Forward-only type replacement (2026-07-12 directive —
no compat alias).

## Perf-neutrality vs the PR 1 baseline (same sandbox protocol, quiet-gated, 8 GiB cage, Tctl 50–65 °C)

| Row | PR 1 baseline (dev@4f1c897/8c3f585) | PR 2 (a / b) | Verdict |
|---|---|---|---|
| 1 fresh create | 3817–4282 MiB/s | 3854 / 3857 MiB/s | in-band ✓ |
| 2 cold seq read | **787** MiB/s (canonical cold) | **797 / 794 MiB/s** | in-band (+1%) ✓ |
| 3 rand-4k read | 326–331 IOPS | 321 / 327 IOPS | in-band ✓ |

Ledgers unchanged: row 2 = 15.97 GiB device read (1.00×) + 16.5 GiB tier writes (the tax —
untouched by design until PR 4); row 3 ≈ 27.8–28.6 GiB per 30 s. Counters: row 2
`get_obj/unique = 1.002` (churn contract), **`singleflight_waiter_result_serves` = 3,402**
(adoption: prefetch/foreground cohort waiters now served from the carried fill, not tier
probes), `stale_binding_rebinds` flat at 0.

## Correctness gates

- Churn suite: phases A–D **byte-identical** and green; new phases E–H green (5× serial +
  parallel stability). E pins waiter-serves-from-result under a 400 ms delayed publish
  (1 fetch / 4 resolvers, counter == 3); F pins the close-only late-subscriber path
  (0 fetch, 0 result-serves); G pins prompt whole-cohort failure (None-on-drop, no 60 s
  park, no counter movement); H pins the future-drop case (aborted primary ⇒ exactly one
  recovery refetch, `get_obj == 2`, bounded result-serves — serve *source* legitimately
  nondeterministic because the aborted primary's validated publish closure survives the
  abort; documented in the test).
- `reused_key_stale_fill_tests` 6/6, `staged_identity_visibility_tests` 7/7 (serial),
  `data_path_correctness_tests` green — all inside the full-gate `--test-threads=1` run
  (60/60 suites).
- Full cargo gate: clippy `-D warnings` clean, fmt clean, doc 0 warnings, bench smoke ok.
- fstests: **QUICK = {003, 213}** — exactly the documented platform expected-fail set
  (`.benchmarks/2026-07-11-seek-hole-oom-and-quick-tier.md` item B), no new failures.
  One earlier QUICK iteration hit the **pre-existing** 8 GiB-cage daemon OOM during the
  616 fsx soak (cascading 616+617 that run); the same OOM class fired on the pristine
  dev@8c3f585 baseline run minutes later (journal 21:39:21, `anon-rss:8340988kB`) — same
  signature, both binaries, load-dependent; generic/616 standalone on PR 2: **4/4 pass,
  zero OOM**. This is the known cage-pressure class PR 7 (R5) exists to close.
- Trio + 074: generic/074, 075, 091, 616 **pass** (explicit run).
- LTP filesystem syscalls: **174 PASS / 0 FAIL / 9 SKIPPED** (fresh `/tmp/ltp_build`
  provision; the first attempt ran against a stale empty `/tmp/ltp_install` skeleton and
  reported spurious harness failures — environment, wiped and re-run).

## Rails

Quiet-gate before every timed run; builds `taskset -c 0-15` / `CARGO_BUILD_JOBS=12`;
CPU cap 3.5 GHz untouched; daemons caged (`MemoryMax=8G`, swap 0); `/mnt/squeezefs`
untouched; nothing pushed.
