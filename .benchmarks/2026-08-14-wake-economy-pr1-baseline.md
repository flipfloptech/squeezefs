# 2026-08-14 — wake-economy PR 1: instruments + the pre-campaign baseline grid

**Branch:** `perf/il-wake-economy-pr1` (`c263a777`). Design: `docs/design-il-wake-economy.md`.
**Venue:** TCP devsub, 32-CPU box. **Instrument:** `.benchmarks/rigs/2026-08-14-wake-economy-rig.sh`
(fresh substrate per row, pre-fill rule, fixed 4 GiB total fileset, 25 s rows, engagement exact
on every row, `tick_delta=0` everywhere). Pair `c263a777` (KD-7).

## What landed

`ipc_cqe_wake_collapsed` + `ipc_cqe_pass_wake_flushes` registered and exported at 0 ahead of
their mechanisms (instruments-first; contract test
`wake_economy_instruments_register_and_export_at_zero`), the campaign rig, and the stats-surface
doc entries. **No behavior change — no perf claim.** The `il_slot_reroutes` column is chartered
in the rig and lands with PR 2's ABI bump.

## The baseline grid (rand-4k, libaio + shim; read row marked -r)

| row | IOPS | p99 (µs) | wakes/s | gauge w/(w+e+c) | PSI-some | collapsed | pass_flushes |
|---|---|---|---|---|---|---|---|
| 32×8 | 181k | 24,249 | 168,168 | **0.924** | 10.0 % | 0 | 0 |
| 16×16 | 205k | 22,414 | 178,689 | **0.866** | 10.5 % | 0 | 0 |
| 8×32 | 219k | 20,841 | 13,322 | 0.061 | 7.4 % | 0 | 0 |
| 4×64 | 255k | 11,863 | 7,325 | 0.029 | 4.6 % | 0 | 0 |
| 1×32 | 120k | 725 | 19,884 | 0.166 | 0.9 % | 0 | 0 |
| 1×1 | 21.1k | 70 | 21,062 | 1.000 | 0.7 % | 0 | 0 |
| 32×32 | 149k | 92,799 | 1,235 | 0.008 | 10.1 % | 0 | 0 |
| **32×8-r** | **830k** | 4,883 | **634,397** | 0.765 | **36.2 %** | 0 | 0 |

## What the baseline confirms (the design's Background, now counted through its own rig)

1. **The regime map is real**: sparse-arm rows (qd ≤ `REAP_EVENT_PARK_MAX` = 24) pay ≈ 1 wake
   per op (gauge 0.87–1.00); deep-arm rows (qd > 24) are already elided (0.008–0.061). The L1
   latch's target population is exactly the sparse arm.
2. **The read fleet is the largest latch prize**: 634k daemon FUTEX_WAKEs/s at 36 % PSI —
   bigger than any write row. The G2 gauge target (≤ 0.25 at 32×8) applies per the design;
   PR 2's bracket must carry the read row.
3. **G3's citable read floor is now this grid's 830k** (32×8-r, this rig, this venue — the
   earlier 713–733k sweep numbers are superseded as unfiled).
4. **qd1 is gauge 1.000 by design** — the wake IS the contract; PR 2's era semantics keep the
   first completion paying (hard gate: p99 70 µs here).
5. Box-load note: quieter box than the 2026-08-13 rows (PSI 10 % vs 32 % at 32×8) — cross-day
   IOPS compares go through this rig's same-day brackets, never across notes.

## Next

PR 2 (`perf/il-wake-economy-pr2`): the wake-collapse latch, IPC_ABI 5→6, loom-modeled;
acceptance = A-B-B-A vs this pair on 32×8 (target ≥ +10 %, gauge ≤ 0.25) with the full grid as
hard gates and the read row carried.
