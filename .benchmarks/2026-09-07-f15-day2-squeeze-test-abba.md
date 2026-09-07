# Finding 15, day 2 — the A-B-B-A on squeeze-test: the S11 gate passes in both B positions; B1 fails on B (2026-09-07)

**The deciding row** (venue rule 2026-09-07: A/B and A-B-B-A rows run on
`squeeze-test`). Same rig as the dev-box scoping pair
(`.benchmarks/2026-09-07-f15-day2-fleet-pair.md`), same binaries, four
rows back to back on the box: **A = `c60746fb`** (the tip before
2026-09-07), **B = `b119ef78`** (the supply-coupled epoch close, finding
51, lane-aware placement + failover, the refill-hint gate).

**Verdict on phase A1 — the S11 shared-file sustained gate: A FAILS in
positions 1 and 4, B PASSES in positions 2 and 3.** B sustains
3,193 MiB/s over 72 s and 3,417 MiB/s over 102 s (20+ iterations without
decay) where A decays 2,087 → 1,062 and 2,162 → 919 (NOT SUSTAINED).
Order-independent, thermally flat (package 35 °C cold → 39–40 °C on every
row), no heat-soak confound.

**Phase B1 — the file-per-proc reference — FAILS on both B rows** (1,023
→ 513; 2,271 → 332, the last third pinned at ~160 MiB/s) with **6,964 /
11,564 `STALE BLOCK-KEY BINDING refused`** on the authority — the
finding-51 regression the dev-box pair exposed (0 on both A rows), plus
the harvest storm; both under fix (`fix/finding-51-adopt-key-incarnation`,
`perf/lane-harvest-single-flight`, `fix/membership-renewal-isolation`).
A never reaches B1.

## 1. Venue

`squeeze-test`: 32-core Xeon, `6.19.14-sqz` (+ patch 0031), Rocky 8, 251 GiB
RAM; fleet on the tcp dev substrate (2 mds null_blk + 2 oss zram
`lzo-rle`, `SQZ_MWFLEET_OSS_GB=32` ⇒ 512 blocks per lane per volume, W =
16), 8 co-writers under S11 range custody, mount root under `/scratch`
(the box's root fs is full), Open MPI 4.1.5a1 (vendor build), the pinned
ior 4.0.0 built by the matrix. Sampler on every row. Pre-flight: 0 nvmet
subsystems / 0 daemons / 0 zram / 0 nullb; post: the two module default
devices only (zram0 disksize 0, the nullb `features` attr) — zero
residue. Driver `box-abba-driver.log`; rows under
`.benchmarks/rows-f15-day2-20260907/box-{1A,2B,3B,4A}/` (authority + m50
+ m53 samples gzipped, every mount's end `.stats`, `matrix.log`, the ior
phase outputs, the authority's serve/refusal lines, m50's log classes).

## 2. The four rows

| row | binary | A1 iterations (MiB/s) | A1 gate | B1 gate | `STALE … refused` (authority) | `INVARIANT TRIPWIRE` lines | `FlushExtents … failed` (Σ co-writers) | Σ lane-ENOSPC (whole row) | wall | pkg °C |
|---|---|---|---|---|---|---|---|---|---|---|
| 1A | `c60746fb` | 4,215 4,255 2,596 3,291 265 1,319 797 1,061 1,155 1,238 2,071 1,201 1,546 971 824 835 1,585 1,316 842 875 921 | **NOT SUSTAINED** 2,087 → 1,062 | not reached | 0 | 48 | 100 | 116,660 | 253 s | 35 → 39 |
| 2B | `b119ef78` | 4,167 3,962 2,049 3,659 3,233 3,558 3,357 2,969 3,305 3,054 3,095 3,334 3,083 3,118 3,457 2,756 3,045 3,044 3,534 3,427 2,828 | **steady 3,193 over 72 s — PASS** | NOT SUSTAINED 1,023 → 513 | **6,964** | 0 | 29 (all `StorageFull`, B1) | 180,842 (B1) | 630 s | 40 → 39 |
| 3B | `b119ef78` | 4,131 4,517 1,879 3,509 288 4,204 5,084 2,781 3,719 3,404 3,245 3,663 3,446 3,569 3,049 3,551 3,605 3,263 3,644 3,414 4,061 3,873 | **steady 3,417 over 102 s — PASS** | NOT SUSTAINED 2,271 → 332 | **11,564** | 0 | 59 (all `StorageFull`, B1) | 51,378 (B1) | 727 s | 39 → 39 |
| 4A | `c60746fb` | 4,023 4,348 2,078 3,456 2,024 1,220 1,060 949 803 1,208 935 1,948 737 1,488 784 1,528 796 1,297 242 889 1,298 799 1,019 890 | **NOT SUSTAINED** 2,162 → 919 | not reached | 0 | 119 | 278 | 129,679 | 290 s | 39 → 39 |

The A1 phase on the B rows, from m50's per-second samples (the A1
window): **ENOSPC refusals +0 (2B) / +656 (3B)** against 116k–130k on the
A rows' A1; **`block_claim_anomalies` +0 on both** (they appear only in
B1: +141 / +179); harvests 266 / 947 RPCs for 6,335 / 6,813 blocks
(**24 / 7 blocks per RPC**); 53 / 71 supply-coupled epoch closes;
tripwires 0 (A rows: 48 / 119 finding-51 lines each). The box's A1 runs
~2× the dev box's rate (3.2–3.4 vs 1.7 GiB/s) and the shape is the same.

## 3. What this decides, and what it does not

* **Decided:** the four landings of 2026-09-07 take the S11 shared-file
  gate from NOT SUSTAINED to PASS on the acceptance venue, in both
  orders, with every coherence gate clean on that phase (tripwires 0,
  fsync failures 0, anomalies 0). The finding-15 campaign's original
  target (`tests/run_mw_matrix.sh s11-mpiio` phase A1) is met on
  `b119ef78`.
* **Not decided:** the row as a whole — the matrix's B1 reference phase
  fails on B, and A never reaches it, so B1 has no A-side comparison. B1
  on B carries the finding-51 regression (the witness re-stabilizes the
  authority's incarnation word but keeps the authority's own number, so
  every re-validation of a co-writer-minted key is refused as stale —
  6,964 / 11,564 refusals) and the empty-harvest storm (18,297 refused
  writes on m50 in 2B's B1, 18,704 harvest RPCs for 7,912 blocks). Those
  are the three fixes in flight; the A-B-B-A re-runs on their binary.
* **Not separated:** the four levers' individual contributions — the
  per-lever legs (`SQUEEZEFS_REWRITE_SUPPLY_CLOSE=0`,
  `SQUEEZEFS_COWRITER_LANE_PLACEMENT=0`, `SQUEEZEFS_ALLOC_LANE_REFILL_HINT=0`)
  are owed on this venue after the B1 fixes land, so one set of rows
  answers both.
