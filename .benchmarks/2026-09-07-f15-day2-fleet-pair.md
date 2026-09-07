# Finding 15, day 2 — the S11 shared-file gate PASSES for the first time; the file-per-proc phase exposes the next term (2026-09-07)

**Verdict.** On the same box and boot, back to back on the sampled rig
(`.benchmarks/rigs/2026-09-07-s11-fleet-row.sh`): row A = the tip before
today (`c60746fb` — term 1 in, nothing from today) fails phase A1 exactly
as every run since the campaign began (1,989 → 903 MiB/s); row B = the
integrated tip `b119ef78` (the supply-coupled epoch close, finding 51,
lane-aware placement + failover, the refill-hint gate) **passes phase A1
— steady 1,683.8 MiB/s over 89 s, 14 iterations 2,269 → 1,722 with no
decay** — and then fails phase **B1**, the `-F` file-per-proc reference
the matrix runs after A1 (770 → 148 MiB/s). B1 had never been reached on
this venue; it fails with a signature the time series makes legible: the
authority's renewal processing head-of-line-blocked behind an
empty-harvest storm. Every coherence tripwire held (finding 51's
`read_settle_lost_serialized` **0** on the authority, was 326/420;
`forced_releases`, `laggard_fences`, `drain_overdue`, fsync failures 0).

## 1. Rows

Venue as the term-1 note (dev box, 32 CPUs, `7.2.3-cachyos-lto`, two
32 GiB data volumes, W = 16 ⇒ 512 blocks per lane per volume, 8
co-writers under S11 range custody, 32 ior ranks, one shared 10 GiB
file in A1, 32 × 320 MiB files in B1). Artifacts
`.benchmarks/rows-f15-day2-20260907/{A,B}/` (per-second samples gzipped,
end snapshots, matrix logs, ior phase outputs, the authority's serve /
refusal lines, the co-writer's log classes).

### Phase A1 — the S11 verdict (the whole-row totals for A; the A1-window deltas from B's samples)

| | A `c60746fb` | B `b119ef78` |
|---|---|---|
| ior iterations (MiB/s) | 2,309 2,558 2,120 1,749 1,529 1,423 1,775 1,827 **270** 793 **251** 740 811 1,233 830 | 2,269 2,422 1,889 1,774 1,823 1,453 1,320 1,400 1,087 1,589 1,604 1,917 1,889 1,722 |
| sustained gate | **NOT SUSTAINED** 1,989 → 903 | **steady 1,683.8 MiB/s over 89 s — PASS** |
| Σ lane-ENOSPC refusals, 8 co-writers | 51,123 | **2,250** (−96 %) |
| harvest RPCs / blocks received (Σ) | 42k / 39k (0.9 per RPC) | 4,323 / 37,742 (**8.7 per RPC**) |
| supply-coupled epoch closes / blocks injected | — | 246 / 19,767 |
| hint-armed proactive refills | — | 294 |
| placement lane failovers / exhausted picks (m50, whole row) | — | 30 / **0** |
| authority `invariant_tripwires` | 420-class (finding 51) | **0**; `served_binding_witnesses` 101,299 |
| `FlushExtents … failed` lines (Σ co-writers, whole row) | 101-class | 52 — all in B1, all `StorageFull` (finding 51's settle class is gone) |
| bound age in steady windows | 1.9–2.4 s | 1.9–2.4 s |

### Phase B1 — the file-per-proc reference (B only; A never reached it)

ior 1,562 899 658 262 1,264 1,250 1,279 **157 148 157 152 148 148 145** MiB/s
— NOT SUSTAINED 770 → 148. B1-window deltas (Σ 8 co-writers): lane-ENOSPC
refusals **121,646**, harvest RPCs **124,240** for 60,419 blocks (0.49
per RPC), epoch closes 2,165 (32,115 blocks), hint refills 2,181,
`block_claim_anomalies` **1,620** (A1: 165).

## 2. What the time series says about B1

Per-second samples across the B1 stall (t = 106–150 s of row B):

```
 t   bound_age  held | member ack_lag min/mean/max | ckpt/s renew/s | m50: epochs/5s acked_lag drain_obs
106     6798   2044 |  4789/ 6057/ 6798           |  3.8    1.0    |  10        1500     0
111    11649   3247 |   790/ 8374/11649           |  3.8    4.6    |  10         786     2
121    10081   3327 |  1072/ 5781/10081           |  4.0   11.4    |  10        1390     0
140    11400   3152 |   916/ 8296/11400           |  3.8    8.2    |  10        1427     0
180     2013    958 |   768/ 1579/ 2013           |  3.8   22.6    |   8         999     8   ← steady again
```

Every member's OWN acknowledgement lag stays ≤ 2.3 s and its passes
advance 8–10 epochs per 5 s throughout (the writer checkpoints at ~4/s
under the ask), yet the authority's view of the members' acknowledgements
reads 10–11.6 s and `membership_renewals` collapse from 22/s to 1–12/s
exactly in those windows. The acknowledgements exist on the members and
do not reach the authority: the renewals that carry them queue behind the
authority's serve plane, which is saturated by **124k harvest RPCs in
nine minutes — ~240/s, half of them empty** (every parked allocation on
every co-writer issues its own harvest; each runs the three-pass
`execute_lane_harvest_aged` — a full free-set scan + sort, a reclaim
drain, a pressure harvest — on the authority's meta lanes) plus the
free/publish bursts. Acknowledgements late ⇒ the ring holds everything
(3,200 offsets) ⇒ every lane starves ⇒ more parks ⇒ more empty harvests:
a positive feedback the grace loop's own instruments show as
`bound_age` spikes to 11 s while `acked_lag` on every member is fresh.
Why B1 tips into it and A1 does not is not settled (file-per-proc's
per-file full-coverage closes deliver supply in 80-block bursts per file;
the lane share per volume is the same) — the mechanism once tipped is.

## 3. What is settled, what is next

* **Settled:** the S11 shared-file gate the finding-15 campaign has
  chased since 2026-08-25 passes on the composed tip; lane-ENOSPC on
  that phase fell 96 %; the harvest path returns 8.7 blocks per RPC
  instead of 0.9; finding 51's authority read failures are gone
  (`served_binding_witnesses` accounts for every served block, tripwires
  0). The four levers each carry an A/B knob for the per-lever legs
  (`SQUEEZEFS_REWRITE_SUPPLY_CLOSE`, `SQUEEZEFS_COWRITER_LANE_PLACEMENT`,
  `SQUEEZEFS_ALLOC_LANE_REFILL_HINT`; finding 51 has none — a fix).
* **Next term (named, not built):** liveness traffic must not queue
  behind bulk serve work on the authority. Two rungs: (1) **single-flight
  harvest per allocator** — one in-flight harvest RPC per (co-writer,
  volume); parked allocations wait on ITS outcome instead of each issuing
  its own (cuts the storm by the park population), with a decline on a
  fresh empty result until the next hint/grant changes; (2) **renewal
  serve isolation** on the authority — the membership renewal (a RAM
  lease op carrying the acknowledgement) served ahead of / apart from the
  publish/harvest verbs so a serve storm can never age the ring's bound.
  (1) attacks the cause, (2) the robustness; both are levers with A/B
  knobs.
* **Also owed:** `block_claim_anomalies` 1,620 in B1 (165 in A1, 136 in
  A) — the own-lane-untracked lineage (a recycled block claimed while a
  stale local refcount lingers) grows with the recycle rate; a
  must-stay-0 whose lineage note is on the board. And the per-lever legs
  of the four landings on this rig, which this pair does not separate.
* **Not claimed:** anything about B1 before today (it was never reached);
  a fleet A-B-B-A (arms are binaries, two rows back to back, same boot).
