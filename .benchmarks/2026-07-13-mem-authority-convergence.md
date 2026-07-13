# Memory-authority convergence — finding #2 CLOSED (2026-07-13)

Branch `fix/mem-authority-convergence` (base dev `2303a7a`). Fixes the
saturation-suite cage OOM fingerprinted in
`.benchmarks/2026-07-12-saturation-suite-oom-finding2.md`: 16-thread
bare `squeezefs bench` (32 GiB O_DIRECT) against an 8 GiB-caged daemon
with `--mem-budget 5G` OOM-killed ~1-in-2 with ~7.94 GiB anon while the
R5 authority sat in Red shedding. Box: AMD RYZEN AI MAX+ PRO 395
(32 hw threads, 109 GiB RAM, CPU capped 3.5 GHz), protocol rails
verbatim (taskset 0-15, systemd-run 8G cages, quiet-gate annotations,
fresh file-backed sandboxes `~/tmp/sqfs_oomfix_run`, 0.5 s sampling of
`memory.current` + `.stats` per-component gauges).

## Reproduction + gauge attribution (pre-fix tip = dev 2303a7a)

First-attempt kill (run r1): kernel record `Memory cgroup out of
memory … anon-rss:8334036kB, file-rss:16628kB` at t≈183 s (rand-write
pass), exactly the fingerprint. Per-component 0.5 s trace
(`~/tmp/sqfs_oomfix_run/trace_r1.txt`):

| t | mc | level | gauge owners |
|---|----|-------|--------------|
| +21…77 s (write seq) | 2.8 → **5.4 GiB** | Green→Yellow→Red | gauges only 1.5 GiB — **RSS ran 3.9 GiB over the gauge sum** (jemalloc-retained churn + in-flight upload anon; statm-visible, Red purge dropped mc 5.4→4.8) |
| +88…93 s (read seq) | 7.5 → 8.19 GiB (cage) | Red | **read_tier_mmap 0 → 5.06 GiB in ~10 s** — weight 0, empty shed, no admission response at any level; its 5 GiB cap alone ≥ the whole budget |
| +173 s (rand write) | 8.19 GiB | Red | **parked_write_buffers 5.5 GiB** (measured mid-balloon: 1,937 buffers vs the 256 soft cap; `staging_mmap=0` — spills 100 % refused), gauge_sum 9.5 GiB |

**Convergence root cause:** Red's shed machinery is a 1 Hz,
headroom-bounded response, but the two heaviest growth paths had **no
admission response at any level** — the advisory-soft parked cap let
inserts proceed past it whenever staging refused (always, on this
shape), and the read tier had neither a gate nor a lever. Under
16-stream saturation they grow at device speed; every sheddable
component pins to its floor (total headroom ≈ 0 vs excess ≈ GiBs) and
the remainder rides the cage until the kernel kills the anon side.
The kill is anon; the tier's page cache is kernel-reclaimable and was
already reclaimed at kill time — which also means the tier's *logical*
gauge was **phantom pressure** (see fix 4).

## Mechanism (tip `0c68b8d`, three commits, red-first)

1. **Unreclaimable pressure arm** (`mem_budget.rs`): per-tick cgroup-v2
   `memory.stat` sample of the kill-relevant set — `anon + file_dirty +
   file_writeback + shmem + unevictable + slab_unreclaimable` (clean
   cache excluded) — windowed like RSS (5-slot decay, no ratchet);
   `pressure = max(Σ non-reclaimable gauges, statm window, unreclaimable
   window)`. The gauge-undercount defense: kernel accounting cannot be
   fooled by a missing gauge (dirty page cache is statm-invisible; the
   PR 7 KV-core anon residual is gauge-invisible).
2. **Disk-tier publish pause** (`cache_read_block` funnel — covers
   fill/dehydration/p2p producers by construction): skipped while the
   unreclaimable arm sits in the Red band (95/91 hysteresis). Keyed on
   the arm, NOT the level — gauge-driven Red over clean reclaimable
   cache keeps publishing (PR 3 knee protection). Counted
   (`read_tier_publishes_paused`); never-lossy (read cache).
3. **Hard backstop** (§5.7 Red semantics, documented): unreclaimable ≥
   100 % of budget for 3 consecutive ticks ⇒ Red sheds collapse from the
   85 % target to the **floors** until the window decays
   (`mem_budget_hard_backstops`, `mem_budget_backstop_active`).
4. **Reclaimable component class**: `staging_mmap` / `read_tier_mmap`
   registered `kernel_reclaimable` — attribution-only (stats keep the
   full inventory), excluded from the pressure basis. The first fixed
   tip's verification run survived but showed the inverse defect:
   5 GiB of already-reclaimed tier logical bytes pinned phantom Red
   through the rand passes and throttled rand-write for nothing.
5. **Red blocking parked admission** (`insert_active_block_buffer`):
   at Red the halved cap (128 × 4 MiB = 512 MiB) is a real bound —
   brief drain assist (250 ms, `SQUEEZEFS_PARKED_GATE_ASSIST_MS`), then
   the writer **self-flushes its own block durably**
   (`upload_active_block_bytes`, the fsync staging-refusal escalation,
   legal under the held block guard per the P1-9 extended order) and
   never parks it. The first cut waited on the shared drain with a 10 s
   deadline; leg-B traces showed drain *convoys* producing 36
   ten-second p99 stalls — self-flush bounds the cost by the device,
   not the convoy. Failure parks past the cap **loud**
   (`parked_gate_timeouts`) — RAM stays the never-lossy custody of
   dirty bytes. No spin, no lock acquired while waiting; the drain
   worker takes (2)+(3-other)+(4) in a clean task context and never
   needs the waiter's block (not in the map) — P1-9 by construction.

Sheds stay never-lossy throughout; hysteresis bands untouched;
zero-copy paths untouched (the gate sits at the park decision, not on
the data path).

## Red-test list (`tests/mem_budget_tests.rs`, 15 green)

New: `pressure_counts_windowed_unreclaimable_arm` ·
`tier_publish_pause_tracks_unreclaimable_hysteresis` ·
`hard_backstop_escalates_to_floors_after_sustained_overage` ·
`red_convergence_under_admission_storm` ·
`kernel_reclaimable_components_do_not_drive_pressure` ·
`memory_stat_unreclaimable_parser` · integration Phase E (tier pause
blocks/resumes the `cache_read_block` funnel, counted) · Phase F (Red
parked admission: peak ≤ halved cap, self-flushes > 0, timeouts = 0,
140-block storm byte-exact — never-lossy pinned).

## Acceptance: bare-suite protocol 8× on the fixed tip — 0 OOM kills

Two fresh 8 GiB-caged daemons × 4 consecutive suites each (the
committed protocol shape, which never survived past run 2 pre-fix).
`box` = foreign-session annotation (a parallel worker session was
active on the box; liveness verdicts count under load — contention
makes the OOM race harsher).

| Leg/run | rc | elapsed | daemon | OOM | box |
|---|---|---|---|---|---|
| A2/1 | 0 | 127 s | alive | 0 | quiet |
| A2/2 | 0 | 107 s | alive | 0 | quiet |
| A2/3 | 0 | 127 s | alive | 0 | quiet |
| A2/4 | 0 | 134 s | alive | 0 | quiet |
| B2/1 | 0 | 125 s | alive | 0 | quiet |
| B2/2 | 0 | 163 s | alive | 0 | busy |
| B2/3 | 0 | 195 s | alive | 0 | busy |
| B2/4 | 0 | 136 s | alive | 0 | busy |

Authority coherence (leg A2 stats, cumulative): red_events 2/4/6/8
(entered AND exited per run — Green samples present between passes);
peak parked 1,024 MiB = the Green soft cap, gated to 512 MiB at Red;
`parked_gate_self_flushes` 229→875, `parked_gate_timeouts` **0**;
`hard_backstops` 0 (the admission gates hold before the backstop is
needed — it remains the documented last line); `memory.current`
touches the cage only in read-flood windows (~17 % of samples, clean
page cache — kernel-reclaimed, harmless), vs 100+ s pinned pre-fix.

## Suite rows vs the fingerprint session's clean run

Fingerprint clean run (2026-07-12): write 1,118 MiB/s · read 355 MiB/s
· rand-4k 39.5 k IOPS @ 14 % cov. Fixed tip (leg A2, quiet windows):

| Row | Fingerprint clean | Fixed tip (A2 r1-r4) | Verdict |
|---|---|---|---|
| write seq 1m | 1,118 | 1,001 / 1,615 / 839 / 908 | in-band ✓ |
| read seq 1m | 355 | 975 / 1,016 / 998 / 775 | **2.2–2.9× better** (no Red-window publish tax, no reclaim stalls) ✓ |
| read rand 4k | 39.5 k | 42.6 k / 39.5 k / 37.6 k / 11.3 k† | flat ✓ († run-4 aged-daemon outlier; pre-fix protocol never reached a 4th run) |
| write rand 4k | (not quoted; PR 7 row-5 band 86–152 IOPS) | 119 / 138 / 101 / 112 IOPS | in-band ✓ |

**Red-window throughput cost, stated honestly:** the rand-write pass
runs its box entirely under genuine Red (the shape exceeds the budget
by design); writers past the halved cap pay their own block's
writeback (self-flush) — that IS the backpressure. p99 write latency in
that window is ~1.1 s (vs 10–20 s with the wait-only gate draft, and
vs a dead daemon pre-fix). No cost is visible in any non-Red window.

## PR 7 scenario non-regression

- Row-5 cage-kill class (rand-4k O_DIRECT writes, 8 thr × qd16 × 30 s,
  8 GiB cage, budget = cage × 0.8): **SURVIVES** — elbencho rc=0 at **222 IOPS** (PR 7 band 86–152; above it), daemon alive, 0 OOM, `red=1`, 2,307 gate waits / 0 timeouts, parked drained to 0 at settle
- PR 3 oversubscription knee (3 GiB cage, 1G+1G LRUs + 256 MiB hot,
  16 GiB written, slice A warmed, warm rand-4k): **NO COLLAPSE** — warm rand-4k **54,969 IOPS** (PR 3 knee: 4,485; 12.3× above it), warm-re-read **12,423 MiB/s** (PR 3 band ~5,000; PR 7's 20.4 GB/s was a session-warm sample under a different admission history), Red held (the designed oversubscription state), daemon alive, 0 OOM, tier publishes NOT paused (unreclaimable arm below the band — the PR 3-protection design working as specified)

## Gates

- Cargo gate on the merge tip: clippy `-D warnings` / fmt / full serial
  suite / doc 0 warnings / bench smoke — all green (run on the note commit's tree; see final report).
- `FSTESTS_QUICK=1 SQUEEZEFS_FSTESTS_MEMMAX=8G sudo tests/run_fstests.sh`:
  Failed 2 of 19 = **{generic/003, generic/213} only** — the expected platform set (009/316 not-run: fiemap unsupported). 069/074/075/616/617/618 (the cage-sensitive soak set) all pass under the 8G cage.
- Loom: not required — the new authority state is single-word relaxed
  atomics (windows, flags, counters) with no cross-word invariant, per
  the standing §5.7 loom-scope note.

## SHAs

- `2b809d7` test(mem_budget): convergence contracts (red)
- `295d8f3` fix(mem_budget,cache,fuse): unreclaimable arm + tier pause +
  hard backstop + blocking parked gate
- `e234165` fix(mem_budget): kernel_reclaimable component class
- `0c68b8d` fix(fuse): parked gate self-flush escalation
- merge: `--ff-only` into dev (dev tip after merge = this note's commit)
