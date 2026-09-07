# Finding 15, B1 term — squeeze-test sequence 2: C-B-B-C and the six lever legs (2026-09-07)

**Verdict.** The B1 tip **C = `886d4e31`** (the single-flight lane harvest,
the finding-51 phase-B1 containment, the renewal cadence's caught-up relax
+ `sqz-lease-io` venue) **passes the full four-phase `s11-mpiio` matrix in
both C positions** — the first full passes ever recorded (A1 3,497 / B1
2,324 / B2 2,301 / A2 2,842 MiB/s, then A1 3,538 / B1 2,361 / B2 2,236 /
A2 3,003) with stale-binding refusals 0, tripwires 0, forced releases 0,
fsck clean — while **B = `b119ef78`** (the day-2 tip) passes A1 and fails
B1 in both B positions, exactly as in the morning's A-B-B-A. Order-
independent, thermally flat (package 35 °C cold → 37–40 °C on every row).
The six per-lever legs on C: **lane-aware placement is load-bearing**
(A1 FAILS without it), the **supply-coupled epoch close** and the
**caught-up relax** each pay measurably, the **refill-hint gate**, the
**single-flight harvest** and the **renewal venue** are throughput-par on
this venue (their value is refusal / RPC economy, stated below).

## 1. Venue and sequence

`squeeze-test` (32-core Xeon, `6.19.14-sqz` + 0031), the same rig and
substrate as the morning's A-B-B-A (`.benchmarks/2026-09-07-f15-day2-squeeze-test-abba.md`):
two 32 GiB zram data volumes, W = 16 lanes ⇒ 512 blocks per lane per
volume, 8 co-writers under S11 range custody, sampler on every row. The
sequence `run-seq2.sh` (driver log `box-seq2-driver.log`): rows 1C 2B 3B
4C, then 5L1 6L2 7L3 8C 9L4 10L5 11L6, each lever leg = C with ONE knob
at `0`. Rows under `.benchmarks/rows-f15-day2-20260907/box-seq2-*/` (the
matrix's own per-phase snapshots `m{0,50,53}_p{0..4}.json.gz` — p0 before
the probe, p1..p4 after A1/B1/B2/A2 — the ior phase outputs, `fsck.out`,
`matrix.log`; authority + m50 samples for 1C/4C/5L1/6L2/10L5). Row 1C
also had its end snapshot mis-captured (the matrix re-creates the fleet
for its closing fsck, so the wrapper's snapshot saw a fresh fleet) — the
per-phase snapshots are the authoritative counters for GREEN rows.

Two rig defects surfaced and were fixed in `.benchmarks/rigs/2026-09-07-s11-fleet-row.sh`
/ the driver: the driver's row wrapper inherited the heredoc on stdin and
ate the remaining rows after 1C (relaunched with stdin detached — hence
two driver logs), and **row 8C is a degenerate control**: its sizing
probe ran while the previous row's teardown was still settling (1-minute
load 30), read 299 MiB/s, and sized the phases to 5 iterations of a
6.5 GiB file (12–46 s phases). The wrapper now waits (bounded) for the
load to fall below a quarter of the CPUs before the probe. 8C is excluded
from every comparison below; the C controls are 1C and 4C.

## 2. C-B-B-C

| row | binary | A1 (shared) | B1 (fpp) | B2 (fpp) | A2 (shared) | verdict |
|---|---|---|---|---|---|---|
| 1C | `886d4e31` | steady 3,497 / 66 s | steady 2,324 / 96 s | steady 2,301 / 99 s | steady 2,842 / 89 s | **FULL PASS** |
| 2B | `b119ef78` | steady 3,419 / 59 s | NOT SUSTAINED 2,363 → 1,045 | — | — | B1 FAIL |
| 3B | `b119ef78` | steady 3,487 / 61 s | NOT SUSTAINED 1,236 → 608 | — | — | B1 FAIL |
| 4C | `886d4e31` | steady 3,538 / 65 s | steady 2,361 / 95 s | steady 2,236 / 101 s | steady 3,003 / 77 s | **FULL PASS** |

Per-phase deltas from the matrix's snapshots (Σ over the 8 co-writers;
authority gauges beside):

| row | phase | lane-ENOSPC refusals | claim anomalies | harvest RPCs | epoch closes | stale refusals (auth) | tripwires (auth) | `overlay_superseded_by_served_publish` (auth) |
|---|---|---|---|---|---|---|---|---|
| 1C | A1 / B1 / B2 / A2 | 17 / 13,101 / 8,439 / 14,879 | 1 / 1,156 / 1,348 / 63 | 1,448 / 3,131 / 3,173 / 2,440 | 391 / 3,393 / 3,508 / 816 | 0 / 0 / 0 / 0 | 0 | 0 / **1,263** / **1,259** / 6 |
| 4C | A1 / B1 / B2 / A2 | 14 / 7,486 / 8,062 / 16,065 | 1 / 1,292 / 1,258 / 71 | 1,504 / 3,229 / 3,202 / 2,401 | 449 / 3,451 / 3,255 / 860 | 0 | 0 | 2 / 1,360 / 1,363 / 0 |
| 2B | A1 | 4,259 | 2 | 6,100 | 431 | 0 | 0 | 0 |
| 3B | A1 | 3,193 | 5 | 5,171 | 499 | 0 | 0 | 0 |

The finding-51 containment fires 1,259–1,363 times per fpp phase on C —
exactly the population that produced 6,964–11,564 stale refusals per B
row this morning — and the refusals are 0. C's A1 is essentially
refusal-free (14–17 vs 3,193–4,259 on B) with 1,448–1,504 harvest RPCs
for the phase (B: 5,171–6,100).

## 3. The lever legs (C with one knob at 0; controls 1C / 4C)

| leg | knob at 0 | A1 | B1 | B2 | A2 | Σ ENOSPC per phase (A1/B1/B2/A2) | Σ anomalies (B1/B2) | Σ harvest RPCs (B1/B2) | reading |
|---|---|---|---|---|---|---|---|---|---|
| controls | — | 3,497–3,538 | 2,324–2,361 | 2,236–2,301 | 2,842–3,003 | 14–17 / 7.5–13k / 8.1–8.4k / 15–16k | 1.2–1.3k / 1.3k | 3.1–3.2k / 3.2k | — |
| 5L1 | `REWRITE_SUPPLY_CLOSE` | 3,495 | **1,798** | **1,756** | **2,423** | 37 / **46,603** / **44,915** / **142,189** | **3,398 / 3,724** | 4.1k / 4.3k | **pays**: fpp −24 %, A2 −19 %, refusals 5–9×, anomalies 3× without it |
| 6L2 | `COWRITER_LANE_PLACEMENT` | **NOT SUSTAINED 3,233 → 2,056** | — | — | — | — | — | — | **load-bearing**: the S11 gate fails without it |
| 7L3 | `ALLOC_LANE_REFILL_HINT` | 3,607 | 2,445 | 2,342 | 3,067 | 248 / 20,572 / 17,783 / 28,873 | 1,270 / 1,367 | 2.9k / 3.1k | throughput par (+3 %, within row spread); refusals 2× — the gate's value is fewer refused writes, not MiB/s |
| 9L4 | `ALLOC_LANE_HARVEST_SINGLE_FLIGHT` | 3,529 | 2,366 | 2,292 | 3,011 | 417 / 4,512 / 7,459 / 9,668 | 1,402 / 1,361 | **9,009 / 12,061** (A2 13,071) | throughput par; RPCs 3–5× without it — the lever's value is the authority's serve-plane economy, which this venue does not saturate |
| 10L5 | `FREE_GRACE_CAUGHT_UP_RELAX` | **3,309** (109 s) | **2,202** | **2,179** | **2,363** (138 s) | **5,043 / 74,398 / 79,170 / 132,586** | 1,084 / 1,314 | 3.5k / 3.6k | **pays**: A2 −21 %, fpp −6 %, refusals 5–10×; the routine-beat hole is real on every phase |
| 11L6 | `MEMBERSHIP_RENEW_LANE` | 3,528 | 2,361 | 2,354 | 3,034 | 18 / 20,740 / 11,153 / 20,600 | 1,238 / 1,387 | 3.4k / 3.3k | wash (as its note predicted: the samples showed no venue wait) — kept as the structural isolation of the ack carrier |

All six legs kept tripwires 0 and stale refusals 0 (the finding-51
containment has no lever; it fired 983–1,424 times per fpp phase on every
leg).

## 4. What is settled, what remains

* **Settled:** `s11-mpiio` — all four phases — passes on `886d4e31` on
  the acceptance venue in both orders; the day-2 tip passes A1 and fails
  B1 in both orders; the finding-15 campaign's original target and the
  matrix's reference phase are both met. Three of the six levers are
  individually worth their cost on this venue (placement: the gate;
  epoch close and caught-up relax: 20–25 % on the affected phases and
  5–10× fewer refusals); three are throughput-neutral here and stay on
  for the economy they buy (refusals, RPCs) and the liveness law they
  encode.
* **Remaining residues (must-stay-0 gauges that are not 0):**
  `block_claim_anomalies` 1,156–1,402 per fpp phase on every C row (the
  own-lane-untracked lineage — a recycled block claimed while a stale
  local refcount lingers; the epoch close cuts it 3× but it is still the
  largest open correctness smell), and the fpp phases' 7–16k lane-ENOSPC
  refusals per phase (the never-lossy path absorbs them and the phase
  sustains, but each is a parked write). Both are the next board items.
* **Not claimed:** anything from row 8C; a multi-machine fleet (this is
  the single-node proving fleet, measured-real at 32 mounts); the levers'
  interaction terms (each leg toggles one knob against the full C).
