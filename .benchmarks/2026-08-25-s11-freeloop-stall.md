# The s11-mpiio acceptance row: the blocker is the free loop, not the thermals (finding 15)

**Date:** 2026-08-25 · **Binary:** `fc9707e0` (release, default features) ·
**Venue:** 1 authority + 8 co-writers, range custody armed, one box,
nvmet-tcp devsub, `SQZ_MWFLEET_OSS_GB=32` (2 × 32 GiB data namespaces) ·
**Instrument:** `tests/run_mw_matrix.sh s11-mpiio` (ior 4.0.0 pinned, 32
ranks, one shared file, 4 MiB block-cyclic; A-B-B-A vs file-per-proc) ·
**Evidence:** `rows-s11-freeloop-evidence/` (the stock-clock run's
snapshots, probe and aborted-phase output + m53's ENOSPC log; the capped
run's row directory was cleaned by the control fleet's create before the
harvest — its numbers below are quoted from the leg's published table)

## What this session set out to do, and what it found instead

Residual 1's open path read *"re-run under thermal mitigation"* — the
2026-08-19 record said this box reaches the ≥ 750 MiB/s domain but fails
the sustained-flatness gate thermally. Two runs re-graded that story:

1. **The clock-cap arm (3.0 GHz all-core, boost off) is FALSIFIED as a
   mitigation**: the probe collapsed 95× (16.6 MiB/s vs 1,582 on
   2026-08-19) — far beyond the 1.7× frequency cut — and the row limped
   through all four phases at rates dominated by something other than
   CPU (per-iteration spreads like 120/33/33/103 MiB/s, one wild
   1,414 MiB/s outlier in a "flat" phase). Ratio gate failed
   (0.217 bracket). Clocks restored to stock (boost on, 5.1875 GHz).
2. **The stock-clock control on a fresh fleet probes 2,240 MiB/s** —
   ABOVE the 2026-08-19 number, so no binary regression and no
   bandwidth problem — and then **aborts in phase A1: repeated
   `fsync(15) failed` → `close(15) failed` → MPI_ABORT** while the
   co-writers log lane ENOSPC storms (m53: 856 refusals, m57: 1,185 —
   "data volume full: 8179 of 8192 blocks allocated — lane N of 16 is
   exhausted while 0 free block(s) belong to lanes this mount does not
   own") plus 30 s write-watchdog overruns. The fsync EIO is the
   POSIX-16 close-time reporting law working as designed; the shortage
   is real.

## Finding 15 — sustained rewrite outruns the freed-offset loop ~34×, and the pressure valve cannot see it

The steady-state LIVE data fits easily (≤ 20 GiB against 36 GiB of
owned-lane supply). What starves the lanes is the RECYCLE path: a
2.2 GiB/s shared-file rewrite displaces ~560 blocks/s, and every
displaced offset must ride ship → authority free ladder → **the S6
freed-offset grace ring** → release → free list → **lane harvest** back
to the co-writer that needs it. Authority-side gauges at abort:

| Gauge | Value | Reading |
|---|---|---|
| `free_grace_deferrals` / `releases` / `offsets` | 8,161 / 7,596 / 565 | closure holds; ~30 GiB recycled over ~470 s ≈ **65 MiB/s** against 2.2 GiB/s of demand — **~34× short** |
| `free_grace_bound_tightenings` | 7,179 (≈ releases) | the 2026-08-21 pressure valve is doing essentially ALL the releasing — at its routine cadence |
| `free_grace_pressure_pct` | **0, the whole run** | the forecast never registered danger while writers ENOSPC'd |
| `free_grace_forced_releases` / `laggard_fences` | 0 / 0 | rungs (b)+(c) never armed — consistent with pressure 0 |
| `free_grace_reader_acks` (per co-writer) | ~60 (≈ 1 per 7 s) | acks FLOW — the ladder's qualification cadence (staleness 2 s + purge + drain windows) is the loop's clock, not a stuck reader |
| co-writer harvests (m53 / m57) | 860 attempts → 110 blocks / 1,194 → 324 | harvest polls hammer an empty free list |
| `alloc_lane_writers` / owned | 16 / 1 per mount (9 writers) | each writer reaches 1/16 of capacity; the recycle loop is its only refill |

**The crisp defect:** the valve's scarcity forecast measures the ring's
headroom against the volume's GLOBAL free supply, but allocation
starves per-LANE — a writer's reachable supply is `cap/W` plus whatever
the loop returns to its residue class. Per-lane exhaustion therefore
never moves `free_grace_pressure_pct`, the valve tightens at routine
cadence instead of escalating through its rungs, and the loop's
throughput floor (ack-ladder qualification latency × epoch quantization)
becomes the fleet's rewrite ceiling. `docs/design-full-multi-writer.md`
rung-19 residual 3 predicted exactly this shape ("a storm's deferrals
outrun releases … lane-share ENOSPC follows") — this is its first
capture ON the acceptance row, post-valve: the valve alone is
insufficient at acceptance-row demand.

Secondary observation (the first, capped run): with the loop stalled the
"flat" 33 MiB/s phases were paced by grace-release cadence, not by CPU —
which retroactively explains the 2026-08-19 "thermal" flatness failure's
shape (decay/sawtooth as lanes drain and trickle-refill). Thermal was
the wrong suspect; the box was never the bottleneck.

## Residual-board effect

- **Residual 1's open path is REWRITTEN**: not thermal mitigation — the
  acceptance row is blocked on finding 15 (the free-loop sustain
  campaign: a lane-aware pressure signal + a demand-coupled release/
  harvest path fast enough for rewrite-rate recycling). The bandwidth
  domain and the blob-aware composition are both proven reachable on
  this box (probe 2,240 MiB/s, indirect domain sizing engaged).
- **Residual 2** (the `SQUEEZEFS_RANGE_CUSTODY` default flip) inherits
  finding 15 as a precondition alongside the fabricated-contention
  finding (item 7): both gate the same row.
- The next fix loop is red-first per the TDD law: a repro pinning
  per-lane starvation invisible to `free_grace_pressure_pct` while
  ENOSPC fires, then the lane-aware signal, then this row from zero.

## Part 1 landed; the row re-graded (addendum, same day)

**Part 1** (`8d2bcd3b`, red tests `76ec1cf4`, full gate green, merged):
the structural half. `execute_lane_harvest` — the co-writers' ONLY
refill, polled 860× against an empty free list in the capture — never
ran the grace funnel: the ring is harvested only from the authority's
own allocation/free contexts, which stop running exactly when the
fleet's writers are the starving ones. It now runs the same ring head
the local allocation funnel does, as a three-pass ladder (routine →
reclaim-drain + routine → PRESSURE), so an empty lane harvest evaluates
the pressure deadline (promise kept pre-deadline, reading at the cliff,
laggard fenced past it) and feeds the valve's gauge. Contracts:
`tests/mw_cowriter_free_tests.rs` §finding 15. This CORRECTS one line of
the attribution above: the forecast's supply input was already
lane-scoped (`virgin_bytes` divides by the partition width) — the
missing piece was the remote funnel arm, not the supply arithmetic.

**The from-zero row on the fixed binary fails EARLIER, and quantifies
part 2.** Quiet box, fresh fleet: probe 202 MiB/s → phase A1 refused by
the sustained-window gate — steady iterations DECAY 138 → 80 MiB/s.
Fleet gauges at the failure: `alloc_lane_harvests` **0 on every mount**
(part 1 inert on this shape — no regression and no engagement: the lanes
never exhausted this time), `free_grace` deferrals 4,773 / releases
4,201 / held 572 / tightenings 2,073 / `pressure_pct` 0. The releases
(~16.4 GiB over the run) bracket exactly the 80 MiB/s floor the phase
decayed to: **the sustained shared-rewrite ceiling IS the grace loop's
release rate** — the ring re-supplies at the ack-qualification cadence
(staleness + purge + drain, seconds per cycle) regardless of demand, so
throughput decays to it long before any lane ENOSPCs. Part 2 is
therefore a DESIGN question, not a wiring gap: making the release rate
track the deferral rate (the ack ladder's qualification lag is the loop
latency; Little's law bounds throughput at held ÷ latency) without
breaking the never-release-unacknowledged promise. That wants the
design/review loop, not a point fix.

**Instrument honesty:** the probe itself swung 33 → 202 → 2,240 MiB/s
across box states (one attempt was launched into a leftover writeback
storm — io PSI ~100 %, load 19 from D-state tasks — and is label-only;
the quiet-box 202-probe run is the counted one). Any future row on this
venue must gate on io PSI as well as load/thermals.
