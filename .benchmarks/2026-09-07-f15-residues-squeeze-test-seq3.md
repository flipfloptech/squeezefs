# Finding 15 residues — squeeze-test sequence 3 (D-C-C-D): all four rows pass; the two residue gauges did NOT move (2026-09-07)

**Verdict.** D = `0bd03455` (the claim-anomaly lineage fix — the served
reply names the freed offsets, publish schema 15 — plus the per-volume
fpp supply landings: per-volume epoch close, per-volume lane-supply hint
on the grant at `CLUSTER_WIRE_SCHEMA` 3, the `lane_allocators` dedup) vs
C = `886d4e31`, four rows back to back on the box with the settle-fixed
wrapper. **Every row passes the full four-phase `s11-mpiio` matrix**
(tripwires 0, stale refusals 0, forced releases 0, fsck clean on all
four), and the new mechanisms engage on D exactly as built — but
**neither residue gauge moved**: `block_claim_anomalies` and the lane
park slices read the same on D as on C. Both landings closed a population
they proved in-process that is evidently not (most of) the fleet's;
their investigations were resumed against this row's per-mount evidence
(`fix/cowriter-claim-anomaly-population`,
`perf/cowriter-fpp-supply-reattribution`). Row 4D is a **degenerate
control** (its sizing probe read 235 MiB/s — a 4.4 s cold-start on the
probe's 1 GiB — and sized the row to 5 iterations of a 5 GiB file, 11–12 s
phases), the second such row today despite the wrapper's settle wait; the
matrix's probe now runs two iterations and sizes from the warm one
(`tests/run_mw_matrix.sh`). The valid D row is 1D.

## 1. Rows

| row | binary | A1 | B1 | B2 | A2 | probe MiB/s | wall |
|---|---|---|---|---|---|---|---|
| 1D | `0bd03455` | steady 3,592 / 59 s | steady 2,215 / 91 s | steady 2,239 / 93 s | steady 2,851 / 76 s | 2,590 | 397 s |
| 2C | `886d4e31` | steady 3,476 / 72 s | steady 2,276 / 107 s | steady 2,298 / 108 s | steady 3,061 / 84 s | 3,185 | 531 s |
| 3C | `886d4e31` | steady 3,268 / 57 s | steady 2,288 / 79 s | steady 2,321 / 80 s | steady 2,970 / 65 s | 2,211 | 451 s |
| 4D | `0bd03455` | (11 s) | (12 s) | (12 s) | (12 s) | **235 — degenerate** | 216 s |

Package 35 → 37–38 °C flat; settle waits 0 / 80 / 90 / 95 s. Artifacts
`.benchmarks/rows-f15-day2-20260907/box-seq3-*/` (per-phase snapshots
`m{0,50,53}_p{0..4}.json.gz`, `matrix.log`, the ior phase + probe
outputs, `fsck.out`; driver log `box-seq3-driver.log`).

## 2. The residue gauges, per phase (Σ over the 8 co-writers, from the matrix's own snapshots)

| row | phase | `block_claim_anomalies` | `cowriter.recomputed_retires` | park slices (`alloc_lane_enospc_refusals`) | `declined_stale` | harvest RPCs / blocks | `volume_hint_skips` | per-volume close blocks / `declined_offvolume` |
|---|---|---|---|---|---|---|---|---|
| 1D | A1 | 0 | 21,386 | 5 | 3 | 1,075 / 46,253 | 205 | 15,612 / 17 |
| 1D | B1 | **1,108** | 5,555 | **20,469** | 19,888 | 2,302 / 68,990 | 434 | 25,907 / 45 |
| 1D | B2 | **1,117** | 7,338 | 9,497 | 9,159 | 2,363 / 69,271 | 411 | 26,467 / 58 |
| 1D | A2 | 73 | 8,871 | **26,430** | 26,039 | 1,855 / 53,028 | 339 | 22,997 / 90 |
| 2C | B1 / B2 / A2 | 1,478 / 1,411 / 67 | — | 12,370 / 13,670 / 13,292 | 11,778 / 13,027 / 12,928 | 3,535 / 3,552 / 2,610 RPCs | — | — |
| 3C | B1 / B2 / A2 | 1,035 / 956 / 58 | — | 11,893 / 10,604 / 18,384 | 11,251 / 10,139 / 17,870 | 2,559 / 2,470 / 1,997 RPCs | — | — |

Authority, whole row 1D: `overlay_superseded_by_served_publish` 2,194,
`free_recomputed_blocks` 236,394, `harvest_served_blocks` 237,542,
`block_key_incarnation_refusals` 0, `invariant_tripwires` 0,
`free_grace_forced_releases` 0.

Reading: the reply-carried retire fires 5.5–21k times per phase and the
anomalies stay at 1.1k per fpp phase — so the anomalous claims are NOT
(mostly) parked keys of the open epoch that the recompute freed; the
population is elsewhere (candidates handed to the resumed investigation:
RAM-only overlay lifetimes outside the parked set, the served-displacement
sink's frees, a post-retire re-insert, the range-custody reply path). The
per-volume hint aims the harvests better (29–43 blocks per RPC vs 24–34;
RPC count −30 %) and the per-volume close moves 15–26k blocks per phase,
yet the park slices are unchanged on the fpp phases and DOUBLE on A2
(26,430 vs 13,292 / 18,384) — the decline is per volume now and still
covers the park, which means either the exhausted volume's supply
genuinely does not move for the whole park (the capacity law: lane share
per volume vs churn × the ring hold) or the per-volume witness moves too
rarely; and A2's growth names something specific to the shared-file
phase under the per-volume close (`declined_offvolume` grows there). The
resumed investigation owes the table.

## 3. What is settled

The two landings are correct and stay (the reply-carried retire closes a
proven in-process population at the right site; the per-volume hint and
close are the honest accounting the summed forms were not — fewer,
better-aimed harvests); every coherence gate held; the full matrix passes
on both binaries in both C positions and in D's valid position. What is
NOT settled is the residues' fleet populations — the next notes.
