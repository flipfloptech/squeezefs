# 2026-08-14 — wake-economy PR 2: the wake-collapse latch + the sparse-marks adjudication

**Branch:** `perf/il-wake-economy-pr2` (`e3f373b1`). Design: `docs/design-il-wake-economy.md`
(L1 + OQ2). Baseline pair: PR 1 `c263a777`. Venue: TCP devsub, the campaign rig
(`.benchmarks/rigs/2026-08-14-wake-economy-rig.sh`), engagement exact and `tick_delta=0` on
every row cited.

## What landed

1. **The wake-collapse latch (CqeDoorbell v3, IPC_ABI 5→6, default ON)** — the `_pad` word is
   the per-park-era `wake_paid` CAS latch: one FUTEX_WAKE per era, later mark-passed
   completions return `Collapsed` (counted). Loom: era-scoped-witness strand model (asserts
   covered AND exactly-one-syscall-per-admitted-era) + the two-parker no-permanent-strand
   model; kernel-strength RMW admission documented as a model-fidelity necessity (loom's plain
   SeqCst load under-models FUTEX_WAIT's bucket-lock read). Weakening ledger: both Dekker
   fence drops fail 2 models each; DELETING the era clear fails `latch_new_era_is_payable`;
   the design's clear-before-register strand prediction was **falsified** (the parked-gate
   precedes the CAS) — recorded at the clear site.
2. **ClientStatsPage trio** (`il_submit_harvested` / `il_park_eras` / `il_slot_reroutes`) on
   the same coarse bump, aggregated reaped-fold + live-sum into the stats inode; the reroute
   count's poison gate lives inside `Session::note_slot_reroute` (one policy point).
3. **Sparse-arm batch marks (OQ2), built and counted — default OFF** (below).

## The latch bracket (A-B-B-A vs PR 1, 25 s rows)

| Row | baseline IOPS | latch IOPS | wakes/s | gauge | verdict |
|---|---|---|---|---|---|
| 32×8 write | 172–177k | 172–173k | 165k → 129k (−22 %) | 0.92 → 0.74 | **wash** |
| 32×8 read | 723k | 714k | 553k → 350k (−37 %) | 0.76 → 0.49 | par (−1.2 %, band) |
| 1×32 | 114k | 113k | 19k → 8.4k | 0.17 → 0.07 | par |
| qd1 | 21.1k, p99 101 µs | 21.1k, p99 100 µs | — | 1.000 (inert) | **hard gate ✓** |

**G1 (≥+10 % at 32×8) NOT MET locally; G2 (gauge ≤0.25) NOT MET at k=1** — the gauge floor is
**era-rate-bound** (a k=1 era spans ~1.3 completions). The latch ships **default ON as a
wash-priced pure syscall economy** (the reap-economy precedent): −22..−37 % daemon FUTEX_WAKEs
with no losing shape, loom-proven, and `SQUEEZEFS_IPC_CQE_WAKE_LATCH=0` as the A/B control.
The field A/B (saturated-reaper fleets) remains the design's named target venue.

## The sparse-marks bracket (latch+marks vs baseline — the era-rate fix, counted NEGATIVE)

| Row | baseline | latch+marks | gauge | verdict |
|---|---|---|---|---|
| 32×8 write | 175–177k | **171k both legs (−3 %)** | 0.92 → **0.29** | LOSS |
| 32×8 read | 841–860k | **809–827k (−3.5 %)** | 0.76 → **0.27** | LOSS |
| qd1 | 21.1k | 20.5–21.8k, p99 63–75 µs | 1.000 (inert) | ✓ |

The marks hit G2's gauge target exactly — and lost IOPS doing it: **the k-th-completion
delivery delay outprices the syscall savings when reapers are not CPU-starved.** Per the
falsified-lever rule: **default OFF**, `SQUEEZEFS_IL_SPARSE_BATCH_MARKS=1` is the field
measurement lever (the SQPOLL precedent), the counted loss on the registry line. OQ2's
remaining half is the field A/B.

## Campaign verdict so far

The wake-syscall theory of the fan-in wall is **weakened on this venue**: removing 22–70 % of
the wakes (either mechanism) does not move IOPS at 32 procs while PSI stays ~12–14 % — the
scheduling term the process sweep counted (185k→257k by client topology) is NOT primarily the
daemon's completion FUTEX_WAKEs. The design's decision-gated PRs 3–5 remain gated; the honest
next candidates are the client-side worker/reaper topology itself (out of this campaign's
lever set) and the field venue A/B for the two shipped levers.

## Bonus catches (this PR's collateral)

- The 047783a0 PARK_MAX retune had missed the knob-convention spot-check table (a latent red
  on dev — the deferred-gate class); fixed here.
- A latent parallel-mode race between the two shipped wake tests (PR 1's collateral, fixed
  with the SERIAL-guard pattern).
