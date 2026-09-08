# Finding 15 residues, round 2 — squeeze-test sequence 4 (E-C-C-E): `block_claim_anomalies` reads 0 on every phase of both E rows (2026-09-08)

**Verdict.** E = `96f8d873` (the lane-free notice channel — every reply
frame carries the authority's frees of the client's blocks, publish
schema 16 — with the per-volume epoch close retired and the per-volume
lane hint kept) vs C = `886d4e31`, four rows back to back on the box, the
two-iteration sizing probe in force (no degenerate row: probes 2,435 /
2,251 / 2,736 / 3,180 MiB/s; row 2C's cold iteration 2,163 was discarded
for its warm 2,251). **All four rows pass the full four-phase matrix**
(stale refusals 0, tripwires 0, forced releases 0, fsck clean on all
four), and **the claim-anomaly residue is closed in both orders**: E
reads `block_claim_anomalies` **0 / 0 / 0 / 0** in position 1 and
**0 / 0 / 0 / 0** in position 4, against C's 1 / 881 / 842 / 63 and
3 / 1,203 / 1,246 / 51 in positions 2 and 3. The notice ledger closes
exactly on both E rows — queued ≡ shipped ≡ received — and equals the
authority's fpp `fold_passes` to within 2 (1,208 vs 1,206; 1,153 vs 1,153;
1,387 vs 1,384; 1,506 vs 1,506), `lane_free_notices_reminted` 0: the
population the round-2 investigation named is the whole population.

## 1. Rows

| row | binary | A1 | B1 | B2 | A2 | probe MiB/s | wall |
|---|---|---|---|---|---|---|---|
| 1E | `96f8d873` | steady 3,610 / 56 s | steady 2,252 / 85 s | steady 2,280 / 86 s | steady 3,050 / 65 s | 2,435 | 444 s |
| 2C | `886d4e31` | steady 3,476 / 56 s | steady 2,383 / 77 s | steady 2,294 / 80 s | steady 2,905 / 64 s | 2,251 (warm) | 451 s |
| 3C | `886d4e31` | steady 3,518 / 63 s | steady 2,345 / 92 s | steady 2,292 / 92 s | steady 3,040 / 81 s | 2,736 | 488 s |
| 4E | `96f8d873` | steady 3,522 / 72 s | steady 2,296 / 106 s | steady 2,287 / 108 s | steady 3,061 / 83 s | 3,180 | 523 s |

Package 35 → 37–38 °C flat; settle waits 0 / 95 / 85 / 75 s. Throughput
par across arms on every phase (E − C: A1 +2 %, B1 −4 %, B2 −0.5 %,
A2 +3 %; all inside the C rows' own spread). Artifacts
`.benchmarks/rows-f15-day2-20260907/box-seq4-*/` (per-phase snapshots
`m{0,50,53}_p{0..4}.json.gz`, `matrix.log`, the ior phase + probe
outputs, `fsck.out`; driver `box-seq4-driver.log`).

## 2. The gauges, per phase (Σ over the 8 co-writers; the authority beside)

| row | phase | `block_claim_anomalies` | `cowriter.lane_free_notices` | `…_reminted` | `recomputed_retires` | park slices | harvest RPCs / blocks | authority `fold_passes` | notices queued / shipped |
|---|---|---|---|---|---|---|---|---|---|
| 1E | A1 | **0** | 0 | 0 | 18,937 | 2 | 980 / 43,511 | 0 | 0 / 0 |
| 1E | B1 | **0** | 1,208 | 0 | 5,873 | 14,130 | 2,225 / 66,124 | 1,206 | 1,208 / 1,208 |
| 1E | B2 | **0** | 1,153 | 0 | 5,852 | 8,435 | 2,223 / 66,892 | 1,153 | 1,153 / 1,153 |
| 1E | A2 | **0** | 4 | 0 | 10,691 | 10,619 | 1,733 / 49,937 | 4 | 4 / 4 |
| 2C | A1 / B1 / B2 / A2 | 1 / 881 / 842 / 63 | — | — | — | 1 / 10,756 / 5,828 / 15,818 | 1,134 … 2,460 RPCs | 1 / 924 / 858 / 2 | — |
| 3C | A1 / B1 / B2 / A2 | 3 / 1,203 / 1,246 / 51 | — | — | — | 14 / 12,012 / 20,420 / 14,402 | 1,426 … 3,068 RPCs | 2 / 1,250 / 1,245 / 0 | — |
| 4E | A1 | **0** | 0 | 0 | 28,075 | 2 | 1,300 / 57,225 | 0 | 0 / 0 |
| 4E | B1 | **0** | 1,387 | 0 | 7,055 | 17,052 | 2,877 / 83,643 | 1,384 | 1,387 / 1,387 |
| 4E | B2 | **0** | 1,506 | 0 | 8,701 | 10,009 | 2,904 / 85,299 | 1,506 | 1,506 / 1,506 |
| 4E | A2 | **0** | 1 | 0 | 13,228 | 29,780 | 2,207 / 63,351 | 1 | 1 / 1 |

`writeback_errors_latched` 0 on every mount of every row. C's A2 trickle
(51–73 anomalies with `fold_passes` 0–2) — the population the round-2
note left unattributed — is 0 on both E rows too: those were notices as
well (the A2 phase's few authority-side frees of a client's block reach
the client through the same channel).

## 3. What is settled

* **`block_claim_anomalies` is a must-stay-0 gauge that stays 0** on the
  full `s11-mpiio` matrix on the acceptance venue, in both orders, with
  the mechanism's ledger closing exactly against the authority's own
  fold count. The finding-15 campaign's last named correctness residue is
  closed.
* **The park slices are the capacity law** (round-2's re-attribution,
  `.benchmarks/2026-09-07-cowriter-fpp-supply-residue.md` §8): unchanged
  on E as expected (14k / 8k / 11k vs 11–12k / 6–20k / 14–16k on C —
  inside the C rows' own spread), the phases sustain, no write reaches the
  wall. The venue's lane share is ~10 % under what `-k`'s 640 live
  blocks + a 3.4 s transit need; not code.
* **Not claimed:** anything beyond the single-node proving fleet
  (measured-real at 9 mounts on one box); the range grant's coverage rule
  (why a whole-block writer ships an extent at all — the board item that
  is this population's ORIGIN, as opposed to its bookkeeping, which is
  what closed here).
