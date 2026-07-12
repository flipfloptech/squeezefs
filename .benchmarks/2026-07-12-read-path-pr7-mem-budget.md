# PR 7 — R5 joint memory authority: backpressure instead of OOM (2026-07-12)

**Branch:** `feat/read-path-pr7` (base dev @ `ae052b2` = PR 6 tip). Design:
`docs/design-read-path.md` §5.7 + the PR 7 plan entry. Lineage: PR 1 baseline, PR 3
(3 GiB-cage oversubscription probe — the recorded thrash knee), PR 5 (row-5 cage-kill
A/B'd pre-existing on `299eb98`), PR 6 rows. Protocol verbatim (3.5 GHz cap, 8 GiB cages
via `systemd-run --user --scope -p MemoryMax=…`, `taskset 0-15`, quiet-gate, fresh format
per session). Raw artifacts: `~/tmp/sqperf/results/p7*`.

## Scenario obligation 1 — the row-5 cage-kill class: **SURVIVES**

Shape: rand-4k O_DIRECT writes, 8 threads × qd16 × 30 s over 8 × 2 GiB striped files,
daemon caged at `MemoryMax=8G` (budget resolves to 6.4 GiB = cage × 0.8 via the per-tick
cgroup re-read). Pre-PR 7: daemon OOM-killed in 19–29 s at 8.33 GiB anon on BOTH the PR 5
tip and `299eb98` (A/B in the PR 5 note).

| Run | elbencho | Daemon | Authority counters |
|---|---|---|---|
| `p7f_row5` | rc=0, 139 IOPS (in-band 63–152) | **alive** | red=1, 34 parked sheds, drain 4,460 MiB → 0 in 3 s, level back to Green |
| `p7g_row5` (definitive, clean pid protocol) | rc=0, 86 IOPS (state-noisy row) | **alive** (verified live daemon + zero kernel kill records) | red=1, yellow=1, 17 sheds, parked 0 at settle, Green ≤ 2 s after load stops |

The two §5.7 timescales, visible in the committed traces (`p7f/p7g_trace.txt`): (i) the
parked DRAIN (early `flush_memory_buffers_*`) frees pool/heap bytes at upload completion
— parked 4,460 MiB → 0 across three ticks; (ii) staging stayed ~0 because this shape's
spill-to-staging is structurally starved (measured: 42 % of spill attempts REFUSED — a
4 MiB active-block entry against ~10 MiB staging shards — and the rand-write revisit
carousel pulls spilled entries straight back), which is exactly why the drain, not the
spill, is the Red mechanism.

What it took beyond the first implementation (each measured, committed separately):
1. **The drain itself** (`7d94f5c`): cap-halving alone let parked buffers grow 250 → 1,750
   through the halved cap (it only gates inserts). The parked component's shed now posts a
   target and wakes a drain worker that flushes inode-by-inode through the existing
   durable path (never-lossy, fencing-checked; no-forward-progress guard).
2. **Idle-only pool gauges** (`3fb6591`): handed-out pool backings were billed twice
   (consumer gauge + pool gauge), inflating pressure ~2× and overstating sheds.

## Scenario obligation 2 — the PR 3 oversubscription thrash knee: **NO COLLAPSE**

Same synthetic probe as the PR 3 note (`MemoryMax=3G` against 1G+1G RAM LRUs + 5 GiB tier
mmap + 256 MiB hot; 16 GiB written, slice A warmed, then warm rand-4k):

| | PR 3 (recorded) | PR 7 |
|---|---|---|
| warm-re-read (3G cage) | 4,963–5,064 MiB/s | **20,407 MiB/s** |
| warm rand-4k (2 GiB set) | **4,485 IOPS — the 8× collapse** | **113,549 IOPS (25×)**, zero device reads at steady state |
| rand-4k over the full 16 GiB (uncacheable superset) | — | 49,006 IOPS, daemon alive |
| authority state | (no authority) | Red held (pressure 2.9 GiB vs 2.4 GiB budget), floors kept the tier serving, daemon alive |

Reading: the authority's floors/weights keep the RAM tiers clamped so the tier mmap keeps
residency — the knee never forms. (R3 ranged reads also serve this shape's misses at 4 KiB
instead of 4 MiB faults, compounding with R5 — both lineage-attributed above.)

## Rows 1–4 vs lineage (8 GiB cage, `p7i` + `p7f/p7g` row-1 samples)

| Row | Lineage band (PR 5/6) | PR 7 | Verdict |
|---|---|---|---|
| 1 fresh create | 3,532–4,282 | 3,914 / 3,733 / 3,765 (one 3,101 sample — first-run-after-format low outlier, same class as PR 5's 3,532) | flat ✓ |
| 2 cold seq read | 6,598–6,644 (inverted) | **6,633** vs same-session row 1 → **still inverted** | flat ✓ |
| 3 rand-4k read | 37,066 (PR 6 gate: ≥ 30× = 9,180) | **60,568** (session-warm tier assist; ≥ 30× with 6.6× margin) | ≥ gate ✓ |
| 4 overwrite | 649–869 | 716 | in-band ✓ |
| warm-re-read | 19,495–19,499 (PR 5/6) | 20,321 / 20,476 | within 10 % (better) ✓ |

## Mechanism (delivered per §5.7)

`src/mem_budget.rs`: ArcSwap component registry (gauge/floor/weight/shed closures) ·
budget = flag → env → **cgroup `memory.max` × 0.8 re-read every tick** → 70 % RAM ·
floor-sum proportional clamp (loud, never fatal) · pressure = max(Σ gauges, windowed-max
RSS over 5 × 1 Hz samples — decays, never ratchets) · Green/Yellow/Red with 4-point
hysteresis + entry-edge events · Red sheds excess-over-85 % to weights over floors ·
advisory-at-admission (one relaxed load): Yellow freezes prefetch growth, LRU/hot
inserts evict-first, **dehydration paused entirely (protected included, counted)**; Red
stops prefetch issue, halves the parked cap, and the sheds run (hot/LRU `shed_to` clamp
with source-dropped victims, prefetch plan clear via lane invalidation, pool `trim_to`
of idle backings, the parked drain) · registration at FUSE init + 1 Hz sampler ·
`--mem-budget` / `SQUEEZEFS_MEM_BUDGET_MB` · stats: budget/pressure/gauge-sum/level/
events/floors-clamped + per-component `{current,floor,weight,sheds}` +
`mem_budget_dehydrate_paused`. jemalloc watch stays out per approved OQ #4 (RSS sampler
covers it; recorded follow-up trigger = windowed RSS sustaining ≳ 10 % above the gauge
sum on quiet workloads). Loom: not required — single-word relaxed atomics + ArcSwap
loads only; no cross-word invariant.

## Red-test list

`tests/mem_budget_tests.rs`: resolution order (pure fn) · floor-sum proportional clamp
(loud) · hysteresis transitions + entry-edge event counts · windowed-RSS decay (no
ratchet) · Red shed-to-weights over floors, once per tick, at-floor components skipped,
Yellow never sheds · `level()` lock-free/immediate under concurrent readers ·
`effective_parked_cap` halving · advisory integration phases (one counter-isolated fn):
Yellow dehydration pause drops protected victims (counted, tier untouched) · Red stops /
Green resumes prefetch issue · Yellow freezes window growth (hwm pinned) · the parked
DRAIN empties striped sub-block overwrite parks byte-exactly.

## Gates

- Cargo gate (tip): see final report (clippy/fmt/full serial suite/doc/bench smoke).
- fstests QUICK + LTP: see final report — **the cage-OOM cascade class is this PR's own
  gate now** (618-class cascades = failure, not attribution).
