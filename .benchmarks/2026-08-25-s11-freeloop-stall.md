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
venue must gate on io PSI as well as load/thermals — with the caveat the
capture below added: THIS laptop's io-PSI `some` reading is structurally
poisoned by a permanently-D-state touchpad IRQ thread
(`irq/117-SYNA3133`), so the gate must read the DISK's own delta, not
the PSI aggregate.

## The PR 1 instrumented capture (α/β/γ — the design's capture step)

Binary `f992c2e1` (PR 1's instruments), fresh fleet, quiet box; all four
phases ran (the ratio gate's failure is expected — the capture is an
input, not a pass/fail gate); row `rows-f15-capture-1787694808/`
(per-phase snapshots; ior outputs dropped). Rig columns, m0 p0→p4:
bound_age 23,009 ms; residence p50/p90 > 16 s over 3,738 samples;
deferrals 4,043 / releases 3,738 / held 305 (closure OK);
**alloc split: fresh 0 / freelist 0 / harvested 0** (m50: fresh 544 /
freelist 0 / harvested 0); lane_reachable 1,024 (m50: 448);
demand_waits 0; prods 274 / tightenings 3,659 / pressure 6.

* **γ — the loop-latency model is CONFIRMED by the new instruments**:
  `bound_age` 23 s and residence p50 > 16 s sit inside §3.2's derived
  24–29 s band (the T1–T8 composition) on live gauges rather than
  arithmetic. PR 2/PR 3's latency levers stand justified as designed.
* **α — hypothesis (a) is FALSIFIED on the self-sized row**: NOTHING was
  recycle-bound — the whole fleet allocated virgin-only (freelist 0,
  harvested 0 on every mount; the authority allocated nothing at all),
  the ring released 3,738 of 4,043 shipped displacements, and nobody
  consumed a single recycled block (the 1 GiB self-sized file never
  approached the 8 GiB lane shares). The §3.3 exclusion argument's
  premise (some stream consumed recycled supply) does not hold on THIS
  shape; it held on the ENOSPC row (32 GiB churn against 4 GiB lanes),
  so finding 15's recycle-bound class remains real WHERE lanes exhaust —
  but it is not what paces the self-sized row.
* **β — no trough, and site 0 correctly stayed silent**: lane-reachable
  never dropped below 448 blocks (≫ `HARVEST_BATCH` = 64), and
  `demand_waits` read 0 — the detector's specificity half proven live;
  its sensitivity half (the trough) needs the lane-exhaustion venue.
* **The row's ACTUAL pacer is residual item 7 — fabricated range-custody
  contention — now reproduced ON LOCALHOST with instruments beside it**:
  602 `dlm_custody_conflicts` + 2,505 `range_custody_desired_trims` +
  demotions waiting 1–4 s each (the demotion-wait histogram) on a fully
  4 MiB-aligned, rank-disjoint shared file, with the iteration series
  BIMODAL (32.8/33.1/… floors against 100–2,260 MiB/s bursts — the 33
  floor is the demotion-wait cadence, not a bandwidth). The 2026-08-19
  fabric capture's shape, no fabric required.

## The PRs 2–4 live verification (2026-08-26, binary `69b2fe16`)

Two counted rows on fresh range-custody fleets. Row 2's per-phase
snapshots + the end-of-run live read are committed at
`rows-f15-levers-live/` (ior outputs dropped); **row 1's directory was
cleaned by row 2's fleet create before the harvest** (the same trap the
first capture hit — its numbers below are quoted from the live stats
reads taken at the abort, recorded verbatim in the session transcript):

**Row 1 (2 × 32 GiB, the original venue):** the probe reached
**1,695 MiB/s over the FULL 10 GiB domain** (the recycle-bound shape the
finding was captured on) and every lever verified live before A1
aborted: ack pipeline depth 3 with **`acked_lag_ms` ≈ 6.7 s** (the
reader's whole qualify+drain+promote path — the beat-quantized ladder
could not go below ~12 s and measured ~28 s end-to-end); reader acks
~120/member (was ~60 for a whole run); demand site-0 counted 2,258 with
**rung a′ prods issued (`demand_prods` 9, prods 797)** and **112
demand-path bound refreshes**; the loop recycled **10,867 releases
(~42 GiB) with `alloc_from_freelist` 300–950 per member** (the original
capture: 0 everywhere); every one of the 300–600 harvests per co-writer
carried the measured horizon hint (`horizon_hints ≡ harvests`), and
19–24 AHEAD harvests fired per member. The abort is §5.7's own stated
**inventory margin** (per-lane spare vs rate × loop latency ≈ 98 % at
full probe rate): co-writers ENOSPC'd with owed supply still inside the
in-flight window — the design's named answer is the venue-sizing lever,
never a gate relaxation.

**Row 2 (2 × 64 GiB — the venue-sizing lever):** all four phases ran to
completion, **zero ENOSPC, zero forced releases, zero laggard fences**,
closure exact (15,364 ≡ 13,671 + 1,693; ~53 GiB recycled). The ratio
gate still fails (1.484/0.401) — and the pacer is now UNAMBIGUOUSLY
residual 7: **+5,449 fabricated custody conflicts and +8,610 desired
trims** on fully-aligned disjoint writes, with 67 GiB of RAM free and
the free loop a bystander (the 64 GiB zram pair also softens the venue:
probe 137 MiB/s — substrate arithmetic, stated for honesty).

**Verdict: finding 15's loop half is FIXED and verified live; the
acceptance row's remaining gate is residual 7 alone** (plus the
substrate-sizing arithmetic the row itself prints). One residual note:
`bound_age` still reads ~14–23 s under storm because the ~2 s
checkpoint-qualification physics plus the reader's promote cadence
dominate once the beat quantization is gone — `acked_lag` ≈ 6.7 s is
the reader-side truth, and the ~12 s target's remaining gap lives in
the owner-side min-composition across 8 readers under CONTINUOUS
deferral churn (each release re-arms the window). The loop is no longer
the row's constraint, so that residue is priced, not chased.

## Finding 16 — residual 7's remaining mechanism, attributed from row 2's own ledger (2026-08-26)

Residual 7 already has a landed fix layer (MW rungs 17/18 + §9.3a: the
v2 desired law, the tail-shrink barrier reserving sticky demotion for
true sharing, and the LEARNED STRETCH CEILING that is supposed to cap
re-collisions at one shrink round per episode). Row 2's range ledger
says why it still fabricates at scale:

| Gauge (row 2, m0) | Δ | Reading |
|---|---|---|
| `range_custody_tail_shrinks` | +51 | the barrier classifies correctly (demotions stayed ~0) |
| `range_custody_tail_shrink_acks` | +11 | …but only 11 notices ever REACHED their incumbent |
| `range_custody_tail_shrink_fence_resolves` | **+40** | **the healthy-fleet law says 0**: 40 of 51 notices died with their grant — the incumbent released before its next renewal carried the notice |
| `range_custody_stretch_ceiling_clamps` | **0** | the ceiling NEVER learned, so every stride episode re-collides |
| `range_custody_desired_trims` | +8,610 | the authority clips the stretched desire on the acquire REPLY itself — a teacher the client currently ignores |
| `range_custody_grants` ≡ `releases` | 3,105 | grants churn (the per-file geometry cap retires spans constantly at 32 ranks), so the renewal-riding notice structurally misses |
| `dlm_custody_conflicts` | +5,449 | each a full-budget arbitration park — the row's stall engine |

**The defect class: the shrink notice's only carrier is the incumbent's
RENEWAL reply, but under the block-cyclic interleave grants live shorter
than a renewal cadence** (the geometry cap churns them), so §9.3a's
learning loop is structurally dark on exactly the workload it was built
for — the ceiling stays unlearned, the desire keeps minting into peers'
future stripes, and every episode pays the full park.

**The fix shape (the next red-first loop):** (a) widen the notice
CARRIER — shrink/demotion notices ride EVERY custody-channel reply to
the incumbent (acquire/extend/release acks, not just renewals), so a
stride-churning writer hears within one interaction instead of one
cadence; and (b) teach the ceiling from the acquire reply's OWN TRIM
(granted span < stretched desired ⇒ the surviving stretch length is the
lesson, zero wire change) — the 8,610-trims teacher that covers the
steady state even when no shrink round ever runs.

**Campaign consequence (recorded here, to fold into the design doc as
the PR 1 capture's output): the s11 acceptance row is gated by residual
7 REGARDLESS of the free-grace levers.** The loop's 24–29 s latency is
real and confirmed (PR 2/PR 3 proceed), the lane-exhaustion ENOSPC class
is real and part-1-fixed at reachability, but the self-sized row's
ceiling is the ranged-custody desire/trim/demotion churn on aligned
disjoint writes — the fabricated-contention fix (rung-20 residual 7)
must join this campaign's path before PR 5's acceptance rung can run in
the ≥ 750 MiB/s domain.

## Finding 16 verification — fix half (b), the trim teacher, live (2026-08-26)

Fix half (b) landed as dev `0afabcff` (contract
`the_acquire_replys_trim_teaches_the_ceiling_without_a_shrink_round`,
`tests/mw_ranged_lease_ladder_tests.rs`; full gate green, clock-capped):
the acquire reply's own trim teaches the learned stretch ceiling, and
the `desired_trims` gauge was redefined in the same commit to count
FOREIGN clips only (a same-scope wall — the client's own earlier grant
clipping its abutting-union window — is the admit-time merge's own
bookkeeping, not contention evidence). Verification row
`s11mpiio-1787731593` on the same 2×64 GiB venue, binary `0afabcff`,
rows archived at `.benchmarks/rows-f16-trim-teacher/` (m0 deltas
p0→live, recomputed from the archived JSON):

| Gauge (m0) | Row 2 (pre-fix) | This row | Reading |
|---|---|---|---|
| `dlm_custody_conflicts` | +5,449 | **+505** | the same-instrument verdict: full-budget arbitration parks down ~10.8× — the treadmill collapsed |
| `range_custody_desired_trims` | +8,610 | +29 | NOT same-instrument (the gauge now counts foreign clips only) — read it as "29 true foreign collisions", not an 8,610→29 fix delta |
| `range_custody_grants` | +3,105 (≡ releases) | +1,163 (releases +1,153) | grant churn ~2.7× lower; `extensions` +2,507 — spans now live and EXTEND instead of being re-minted |
| `range_custody_stretch_ceiling_clamps` | 0 | **0** | the teacher fires on the trim itself, and with foreign trims down to 29 the doubling rarely re-collides — a clamp only counts when a LATER stretch hits the learned ceiling, so 0 here is consistent with the teacher working, not proof of it (the cargo contract is the proof the clamp arm fires) |
| `range_custody_tail_shrinks` / `_acks` / `_fence_resolves` | +51 / +11 / +40 | +31 / +2 / **+29** | **fix half (a) is still owed**: 29 of 31 shrink notices again died with their grant — the renewal-riding carrier still structurally misses under grant churn |
| `range_custody_waits` | — | +46 | residual arbitration, see below |

Throughput faces (same venue, same phases): probe 32.90 MiB/s — the
probe writes a self-sized ~1 GiB file into the 2×64 GiB venue, so this
is a small-domain number, NOT the ≥ 750 MiB/s acceptance precondition
domain (venue-state honesty: PR 5's rung still needs the properly-sized
probe). A1 105.2 MiB/s; B1 622.9; B2 707.3 steady; **A2 FAILED the
sustained-window gate** (311 → 33 MiB/s decay across the window).

**Verdict: finding 16's half (b) is live and the fabricated-contention
treadmill collapsed** (conflicts −90.7 %, grant churn −63 %, spans
extending instead of re-minting). **Residual, in order:** (1) fix half
(a) — widen the shrink/demotion notice carrier to every custody-channel
reply (the 29/31 fence-resolves are its standing proof); (2) attribute
the remaining +505 conflicts and the A2 sustained-window decay — with
the range treadmill gone these are the next constraint, and A2's
311→33 shape says something still degrades within the phase; (3) only
then PR 5's acceptance rung from zero.

## Finding 16 verification — fix half (a), the carrier widening, live (2026-08-26)

Fix half (a) landed as dev `e08893be` (custody schema 6 — shrink/demotion
notices ride EVERY custody-channel reply: `AcquireReplyFrame`,
`ReleaseReplyFrame`, the renewal unchanged; contract
`shrink_notices_ride_acquire_and_release_replies_not_just_renewals`; full
gate green). The SAME gate also caught and fixed **finding 17** — a
pre-existing read-tier deposit-window wrong-data race, its own note at
`.benchmarks/2026-08-26-read-tier-deposit-window.md` (dev `976ddca8`) —
so this verification row carries BOTH fixes. Row `s11mpiio-1787746893`,
2×64 GiB venue, binary `976ddca8`, archived at
`.benchmarks/rows-f16a-carrier/` (m0 deltas p0→live from the archived
JSON; per-phase splits from p0..p3):

| Gauge (m0) | Trim-teacher row | This row | Reading |
|---|---|---|---|
| `tail_shrinks` / `_acks` / `_fence_resolves` | +31 / +2 / +29 | **+85 / +84 / +1** | **the carrier works**: the fence column collapsed 94 % → 1.2 % — notices now reach their incumbents within one interaction, and the ledger closes exactly (85 ≡ 84 + 1) |
| `range_custody_waits` | +46 | +85 (≡ shrinks) | every wait is an honest shrink round — zero fabricated parks on the ranged phase |
| `desired_trims` | +29 | +86 | foreign clips only (same instrument as the trim-teacher row); ~10× the work, ~3× the trims |
| `range_custody_grants` / `extensions` | +1,163 / +2,507 | +17,728 / +65,149 | ~15× the row's work moved through the plane |
| `dlm_custody_conflicts` | +505 | +84,770 — **but split per phase: A1 (shared, ranged) +1,050; B1 +43,435; B2 +40,145; A2 +140** | 98.6 % of the conflicts sit in the FILE-PER-PROCESS phases, which carry no byte-range sharing at all — a whole-file/allocation-pressure class, NOT range custody; the ranged phase's own conflicts are ~1 k at 12.4 k grants |

Throughput faces: **probe 1,729 MiB/s** (was 32.9 — the ≥ 750 MiB/s
acceptance precondition domain is now REACHED; the self-sizer wanted
38 GiB and was capped at the 10 GiB zram-budget clamp). **A1 shared
915.6 MiB/s steady** (was 105.2 — ~9×), with an honestly BIMODAL
iteration table (~300 MiB/s iterations alternating with ~2,700 bursts;
the flatness gate passed on first-vs-last steady, both bursts). B1
261.5, B2 255.5 (lower than the prior row's 620–700 — the venue re-sized
10× larger, not same-shape). **A2 died on honest capacity**: rank 7's
mount (m51) hit `alloc_lane_enospc_refusals` — `nvme3n1` full at
16,374/16,384 blocks, lane 5 of 16 exhausted with **0 foreign-lane free
blocks** (so NOT the lane-reachability class finding 15 part-1 fixed) —
fsync failed, ior aborted. The free-grace board at death: deferrals
142,896 ≡ releases 141,588 + offsets 1,308 (closed), residence dominated
by the >16 s bucket, `free_grace_bound_age_ms` 21.5 s. The arithmetic is
the design's own inventory-margin statement at the NEW rate: ~900 MiB/s
of rewrite churn × ~21 s of grace residence ≈ 19 GiB of deferred
inventory per wave against 2×64 GiB — the fixes raised throughput ~9×,
so the venue that survived the old rate starves at the new one. One new
tripwire observed once: the authority refused 1 shipped free as
"already free/graced/quarantined" (the double-release lineage,
leak-safe direction) — filed for the next loop.

**Verdict: finding 16 is CLOSED — both halves live.** The §9.3a learning
loop is no longer structurally dark: notices arrive (fence 1/85), the
teacher fires at the source, the ranged phase runs at 9× with its
conflicts down to noise. **Residual board, in order:** (1) the s11 row's
constraint is now GRACE-INVENTORY CAPACITY at the fixed rate — either a
venue sized to churn_rate × residence (≥ 2×128 GiB at ~900 MiB/s) or the
free-grace residence itself (bound_age ~21 s under churn = the
owner-side min-composition already named in the PR 2/3 residue); (2) the
fpp phases' whole-file conflict class (+83 k, no ranges involved); (3)
the once-seen double-release refusal; (4) PR 5's acceptance rung from
zero on a venue that closes A-B-B-A.

## Finding 18 — the beat terms miss their budgets: bound_age 15.5–22.6 s vs PR 5's ≤ 12 s gate (2026-08-26, attributed from the f16a row's own words)

PR 5's gate (c) demands `free_grace_bound_age_ms` ≤ 12 s sustained. The
f16a row ran 15.5–22.6 s at every phase snapshot — so the acceptance
rung cannot pass today regardless of venue sizing. The row's own words
decompose the overrun (all from `.benchmarks/rows-f16a-carrier/`,
m51@p3 + m0 phase snaps):

| Term | Design budget (§5.7 post-fix) | Measured | Reading |
|---|---|---|---|
| member ladder (qualify+drain+pass grain) | ~6–8 s | `free_grace_acked_lag_ms` **6,293** | ON budget — the PR 2 pipeline works (`ack_pipeline_depth` 3, promote lag ≈ its own arithmetic) |
| learn→acked label distance (member-side end-to-end) | — | learned 1,648,211 − acked 1,633,864 = **14,347 label-ms** | ~8 s of beat terms stack AROUND the on-budget ladder |
| T1 learn + T5 carry (the renewal beat, both directions) | ≤ 1 s each (prodded) | mean membership beat ≈ **2.3 s** (m0 renewals p1→p3: 4,005 over 1,167 s ÷ 8 members) while `free_grace_prod_renew_ms` = 1,000 is in force and 4,113 prods fired | the 1 s prodded cadence covers only ~3 of 8 members at any instant (prods ÷ wall ≈ 2.6/s); on a fleet where EVERY member defers frees continuously, the waiting-on set rotates across all 8, so most beats run wider than the prod's ask |
| T8 min-composition + publish | ≤ +1–2 s | bound (min across 8) trails m51's own acked (1,633,640 vs 1,633,864 at p3); `bound_age` 15,475–22,621 | the min inherits the WIDEST member's stacked beats, not the mean |

Also live in the row: `free_grace_pressure_pct` 94 at p3 with
`fence_bound_ms` 40,396 tightened from base 76,045 (the valve at work),
tightenings 172 k, and the never-corrupt arms 0 throughout
(`forced_releases`/`laggard_fences`/`alloc_stalls`). m51's
`free_grace_pass_prods` = 0 — L2b correctly inert (pass interval already
at the 1 s floor).

**The finding:** the ladder (PR 2) meets its budget; the BEAT terms
(T1/T5/T8 — the prod's coverage, not its cadence) do not. The prod
reaches the members the owner is CURRENTLY waiting on, but on an
all-writers fleet the min rotates, so the composed bound_age lands at
15–22 s. Fix shape to investigate red-first: prod COVERAGE on a fleet
where every member holds deferred labels (the "newest label" watermark
reads everyone as behind, yet delivered prods cover ~⅓ of beats — either
the prod is rate-limited below the fleet's need or its waiting-on set is
computed against the wrong watermark), and the min-composition's
publish-per-beat quantization. Both are owner-side, venue-independent.

**Finding 18's landed half (2026-08-26, dev `6ce59456`):** the precise
lapse mechanism was the ASK'S EXPIRY ARM — under a storm the runway
reading sawtooths, every release crest let the prod lapse, and the very
next renewal beat drew the ROUTINE cadence (one routine grant = one
routine-beat acknowledgement hole; the min-composition inherits the
widest member's hole). `take_prod_cadence` now DECAYS an expired ask one
doubling step per reading window while the plane holds offsets (the
probe governor's bleed-to-routine pattern — derived, never a knob),
re-tightens fully at the next reading, retires at routine, and a drained
ring retires immediately (contracts
`an_expired_prod_decays_toward_routine_while_offsets_are_held` +
`a_drained_ring_retires_the_expired_ask_immediately`; engagement gauge
`free_grace_prod_decays`). Full gate green.

## PR 5 acceptance attempt 1 — FAILED at A2 (2026-08-26; fail-fast, fix, restart from zero)

Row `s11mpiio-1787762878`, binary `6ce59456` (carrier + trim teacher +
tier fix + prod decay), 2×64 GiB, quiet box (load 1.0, Tctl 57 °C),
archived at `.benchmarks/rows-pr5-attempt1/`. Probe **1,592.9 MiB/s**
(precondition ≥ 750 met). A1 shared 698.1 steady (bimodal — ~310
iterations with 2,288/2,713 bursts); B1 193.3; B2 211.7; **A2 aborted**
(rank 24, fsync EIO).

**What the row PROVED for the campaign:**
- Gate (d) HOLDS end to end: `forced_releases` 0, `laggard_fences` 0,
  `alloc_stalls` 0.
- The decay fix ENGAGED (`prod_decays` 20, prods 5,506), and a mid-row
  live read during B1 showed the loop at its ideal: `bound_age` **0**
  with the ring EMPTY under storm (deferrals ≡ releases at 52,550).
- BUT cumulatively 96.5 k of 134.8 k released offsets resided **> 16 s**
  (the burst phases still outrun the loop), so gate (c) — bound_age
  ≤ 12 s sustained — is NOT demonstrably met; end-of-row bound_age read
  12.8 s with 1,729 offsets stranded by the abort.

**What killed A2 — the ENOSPC spiral, now decomposed:** a burst
iteration's displaced inventory fills one volume → write-through falls
back to STAGING (the fsync-durability degrade) → rewrite epoch closes
fail transiently on allocation and RETRY (m55: 455 retries) → displaced
frees ship late or never (m55 shipped **1,392** blocks against m56's
18,844 — its lane starves: 36,075 `alloc_lane_enospc_refusals`, 71,757
ENOSPC log lines) → the lane never refills → fsync EIO → abort. The
free-grace loop is no longer the first domino; the spiral's entry is
burst-rate inventory against the 64 GiB volume and its non-recovery is
the frees not shipping under the degraded posture.

**Two NEW findings filed from this row (both must-fix before attempt 2):**
- **Finding 19 — the double-release lineage at scale**: the authority
  refused **6,321** shipped frees fleet-wide as "already
  free/graced/quarantined" (m0 log census; `free_replays` 0 and
  `free_stale_refusals` 0, so these are neither wire retries nor era
  fences — one offset genuinely enters the free pipeline twice).
  Correlates per-mount with the epoch-close retry census (m55: 455
  retries / 135 refusals logged locally … every co-writer shows both).
  Leak-safe at the authority BY CONSTRUCTION, but the second local act
  on the co-writer is suspect — see finding 20.
- **Finding 20 — `read_settle_lost_serialized` tripwires (must-stay-0)**:
  m56 counted **48** `invariant_tripwires` — the incarnation word moved
  under `BLOCK_FLUSH_LOCKS + INODE_META_LOCKS`, the outcome the design
  says cannot happen — escalating to "did not settle after 4 serialized
  attempts" EIO, the latched writeback error (POSIX-16 working as
  designed), and the ior abort. Working hypothesis: findings 19 and 20
  share a root — a double-entered free retires the offset's incarnation
  word a second time while its next owner is mid-settle. Red-first loop
  next; the counted-run law restarts the acceptance count after the fix.

## Findings 19/20 fixed; attempt 2 (2026-08-26, dev `c344e517`) — a NEW class, finding 21

Findings 19/20 landed as the verdict-aware shipped-free retire (dev
`c344e517`: `Refused` retires nothing locally, `NonTerminal` releases one
reference and never touches the word; contracts
`a_refused_free_verdict_retires_nothing_locally` +
`a_nonterminal_free_verdict_never_destabilizes_the_word`; full gate
green). **Attempt 2** (row `s11mpiio-1787770373`, archived at
`.benchmarks/rows-pr5-attempt2/`): probe **2,083 MiB/s** (highest yet),
fleet-wide error census ZERO tripwires / ZERO double-releases / ZERO
ENOSPC at the abort instant — findings 19/20's classes are DEAD. A1
itself then died on **finding 21**: the ack-early overlay publish's
coalesced record breached the KV per-volume value cap ("record value
length 66,109 exceeds the per-volume cap 65,792") and the never-lossy
retry recomposed the SAME over-cap value for ever — a permanent fsync
failure (errno 7 latched, ior abort). Attributed code-exact: the
NON-rehydrated tail of `custody_scoped_layout` composed 32 scoped
writers' Puts past the cap with NO spill arm (the K2 fix's site-(c)
comment claimed it spilled; it did not). Fixed as dev `1ddc9fda` —
the tail runs the SAME ceiling + CoW-blob spill as the rehydrated arm,
counted `publish_compose_spills`; contract
`an_armed_scoped_put_crossing_the_inline_cap_composes_to_indirect`.

## PR 5 acceptance attempt 3 (2026-08-26, dev `1ddc9fda`) — ALL FOUR PHASES COMPLETE; the A-B-B-A gate PASSES; the row is INVALID on engagement — finding 22

Row `s11mpiio-1787775532`, 2×64 GiB, archived at
`.benchmarks/rows-pr5-attempt3/`. **The first row in the campaign to
complete A-B-B-A end to end**: probe 1,918 MiB/s; A1 shared 605.9 steady;
B1 248.1; B2 264.3; A2 shared 425.0 — **shared ≥ 0.8× disjoint in BOTH
brackets (min 1.608×)**, engagement per-mount exact on the ranged
ledgers (acquires+extensions ≈ 15.7 k/mount, publishes ≈ 121 k/mount),
authority `conflicts d=0` on the ranged phases, and the shrink ledgers
PERFECT (A1: 82 ≡ 82 acks + 0 fence; A2: 80 ≡ 80 + 0 — finding 16's
carrier at steady state). Free-grace: `forced_releases` 0,
`laggard_fences` 0, `alloc_stalls` 0 across the row; `prod_decays` +18
(finding 18 engaging); compose spills 0 with `publish_blob_composes`
+5,055 (the rehydrated arm carried every compose — finding 21's arm is
the guard, not the steady path, on this shape).

**The row is INVALID on the matrix's own engagement gate**: the
range-shared clauses moved on the AUTHORITY's mount (prs +26, ors +8)
plus 24 `read_settle_lost_serialized` tripwires — ALL in the fpp phases
(per-phase split: B1 carries prs/ors + 12 tripwires, B2 the other 12;
the ranged phases are clean), all on m0, all co-writers at 0.

**Finding 22 — the bridge-ask refusal storm on the fpp shape**: m0's log
carries **285,495** "bridge ask" refusal lines — a co-writer's required
span overlapping TWO of its own live grants, the §9.2 shape the design
says "a client whose range cache answers covering probes never builds";
the fpp clients build it in a LOOP (the POSIX-5 retry ladder re-asks the
same span), driving `dlm_custody_conflicts` +92.6 k in the fpp phases,
and the unacquirable writes fall through to the authority-assembled
path — which is what moves prs/ors and races m0's settle reads (the 24
tripwires, one wedged block retried across both phases). Root-cause
directions for the red loop: (a) the client's covering probe must
answer from the union of its OWN two grants; (b) the admit-time merge
left two abutting same-holder grants unmerged (why?); (c) the bridge
ask could legally be admitted by splitting the required across the
holder's own grants. The fpp inos' grants come from the doubling/trim
interplay on single-writer files — no peer is involved at all.

## Attempt 4 (2026-08-26, binary `80c2430e` — f22 landed): finding 22 CLOSED live; finding 23 found

Two launches, both from zero (counted-run law):

* **Launch 1** (stock clocks): the probe ran 1,918 → phase A1 opened at
  **2,626 MiB/s** — the f22 union-cover live for the first time — and
  the package hit **94 °C**: the boost clock decayed and the sustained
  gate correctly refused the row (2,626 → 1,421 MiB/s > 30 %). A venue
  artifact, not a product decay; rows archived
  (`/tmp/rows-pr5-attempt4a-venuefail`). Posture correction: acceptance
  rows now run at a FIXED 2.2 GHz all-core cap (boost off) so the clock
  cannot be the decaying term.
* **Launch 2** (fixed 2.2 GHz, row `s11mpiio-1787784901`, archived
  `/tmp/rows-pr5-attempt4b` + fleet logs): **finding 22 verified closed
  live** — "bridge ask" refusal lines **285,495 → 0**, A1 opened ~3× the
  attempt-3 rate. A1 still failed the sustained gate (1,799 → 1,211
  MiB/s) — and this time the decay is real: **finding 23**.

## Finding 23 — the indirect-map blob's free has ONE owner (fixed `1de4f86a`, red `99ea60c6`)

The f22 fix unmasked a blob double-free lineage at full rate — the
`f19-root` board item's double-ENTRY source, found:

* **The mint**: the owner's scoped compose (`custody_scoped_layout`)
  re-spills a fresh blob per served Put and frees the durable
  predecessor itself (`free_after_commit`) — m0 ran **1,314 composes**.
  Every co-writer whose cache invalidation refetched that
  owner-composed head then claimed the SAME blob as its own
  `old_indirect_to_free` lifecycle: up to 9 frees of one offset
  (1 owner + 8 co-writers — the observed 3–9× refusal bursts, 325
  `block_untracked_free_refusals`, ~40 refused frees per co-writer).
* **The blast**: a duplicate landing after the offset was REALLOCATED
  hit the executor's RAM-tracked arm (`refcount > 0`) and freed the
  LIVE successor's block — **192 `read_settle_lost_serialized`
  tripwires** all on block 547 of the shared ino, 48 settle-EIO each on
  m50/m55, fsync EIO into ior, and a **112 MiB aggregate-size loss**
  (28 × 4 MiB abandoned publishes downstream of the EIO churn).
* **The third face** (found by the red loop's own fixture):
  `persist_dirty_layout_if_needed`/`grow_layout_size` re-inserted their
  pre-save clones after the save, clobbering the save's republished
  entry — the cache then lied about the persisted head (next save
  re-releases the old record, leaks the fresh blob).

Fix (contracts `tests/mw_cowriter_free_tests.rs`, 3 red + 2 pins):
provenance-gated mint (`CachedMetadata::block_map_id_own_mint`, stamped
only by the save republish; a range-shared save frees only its OWN
mints — skip counted on `publish_blob_foreign_free_skips`; solo and
whole-file custody verbatim), ledger-shielded executor (a tracked free
with `population ≥ refcount` names a dead lifetime — `Refused`, counted
on `block_live_free_refusals`, must-stay-≈0), and clobber-free save
wrappers. Residual (board): the free wire still carries no incarnation
witness — the narrow freed→reallocated→unpublished window is closed by
no known mint but not by construction; a witness is a publish-schema
candidate if `block_live_free_refusals` ever moves in the field.

## Attempt 5 (2026-08-27, binary `a434b856` — f23 landed): f23 verified engaged; finding 24 found

From zero at the fixed 2.2 GHz posture (row `s11mpiio-1787872718`,
archived `/tmp/rows-pr5-attempt5` + m0/m56 logs; watchdog silent).
**f22 and f23 both verified live**: bridge asks 0, the mint gate skipped
97 foreign blob frees on m50 alone, untracked-free refusals **325 → 4**,
zero fsync-EIO wedges on six of eight co-writers. A1 opened at **2,746
MiB/s** (~3× attempt 3) and still failed the sustained gate (→ 1,325).

## Finding 24 — the residual duplicate's full causal chain (fixed `fcc13c50`, red `a4a13332`)

The four residual duplicates came through f23's DOCUMENTED holes, and
one of them toppled the fleet:

1. **The mint's sampling gap**: the gate keyed on `range_span_hull` —
   LIVE grants — and the trim/doubling churn retires an ino's whole
   grant set for an instant; a save in that gap ships the refetched-blob
   duplicate again (the 4 refusals).
2. **The shield's unpublished window**: one duplicate landed after the
   offset was freed → reallocated → written-but-unpublished. No ledger
   reference exists yet (`population 0 < refcount 1`), so the f23 guard
   passed and the executor freed the MID-WRITE block (block 2266 of the
   shared ino — 32 `read_settle_lost_serialized` tripwires, m56's 16
   settle-EIO fsync failures).
3. **The amplification** (designed behavior meeting a wedged member):
   m56's wedge stalled its freed-offset acks at 23:19:26; the grace
   ring's min-acked release lag ballooned to **17.5 s** while the row
   displaced ~675 blocks/s ⇒ ~47 GiB of freed-but-unreleased supply on a
   64 GiB volume; by 23:19:40 the fleet hit lane ENOSPC on a
   99.96 %-allocated volume ("0 free blocks belong to lanes this mount
   does not own"), 28 fsync `StorageFull` failures, the same 112 MiB
   (28 × 4 MiB) aggregate shortfall, and the throughput sawtooth the
   sustained gate refused. The valve gauges (`pressure_pct` 0,
   `alloc_stalls` 0, `forced_releases` 0) are honest: the ring itself
   was nowhere near its designed limits — the laggard-fence ladder
   (76 s) would have fenced m56 had the row lived that long.

Fix (contracts in `tests/mw_cowriter_free_tests.rs`, both red on the
exact mechanisms): the mint gate's discriminator is now the **monotone
per-mount range-episode latch** (`tokens::range_episode` — set at every
grant record, never cleared in production; a stale latch costs one
leak-safe skipped free), and the executor refuses a tracked free whose
**incarnation word is unstable** (claimed/mid-write; the claim tail
marks it, the DMA-complete publish stabilizes it — no legitimate
displaced free names an unstable offset). Fixture truth:
`authority_file_with_block` now stabilizes the word the way production's
publish does.

## Attempt 6 (2026-08-27, binary `7d38e33a` — f24 landed): correctness signal valid, perf contaminated; finding 25

From zero at 2.2 GHz — but the watchdog fired 3× mid-row (92–98 °C,
cap dropped to 1.8 GHz mid-phase): the PERF verdict is venue-invalid.
The correctness columns stand (row `s11mpiio-1787880454`, archived
`/tmp/rows-pr5-attempt6` + logs): **f24 verified** — bridge asks 0,
foreign-blob skips engaged (m50: 47), duplicate refusals present but
both shields SILENT (`block_live_free_refusals` 0 — no duplicate
executed; the 11 untracked refusals are the harmless free-list arm).
Remaining: **480 `read_settle_lost_serialized` tripwires across FOUR
blocks, settle-exhaustion fsync EIO on six of eight co-writers** — with
no duplicate free executed, a different class entirely.

## Finding 25 — the stale-cached-head settle wedge (fixed on `fix/f25-settle-stale-head`, red `08ff1e62`)

An S9 SERVED publish commits the ino's layout DIRECTLY on the backend
(outside the router's merge domain) and invalidates the authority's RAM
cache as a SEPARATE act — a refill racing the pair leaves the router's
`metadata_cache` holding a superseded head, permanently. The settle
interior's premise ("any cached entry is ≥ every completed merge") is
FALSE on a serving authority: every attempt re-read the same stale
entry, resolved the dead binding, lost to its retired incarnation, and
the 4-loss exhaustion EIO'd the co-writer's fsync barrier while the
tripwire mis-attributed legal serve traffic (the per-block persistence —
m50 retried block 1834 for seconds — is the stale cache's signature).
Fix: loss classification by HEAD PROVENANCE (cache-served loss = legal
race → `read_settle_stale_head_refetches` + entry drop + backend retry;
tripwire reserved for backend-fresh losses) and the settle arms take the
per-ino serve stripe for custody-armed inos (a served commit cannot move
the binding mid-window). Red contract
`a_stale_cached_head_never_wedges_the_settle_into_eio`
(tests/rebind_starvation_tests.rs) — red on the exact live signature.

Thermal note: the box now trips 92 °C+ even at the 2.2 GHz cap under
fleet rows (it survived two full gates at this cap earlier the same
day) — attempt 7 runs at a FIXED 1.8 GHz all-core posture, whose floor
throughput (≈1,400 MiB/s observed while throttled) still clears the
750 MiB/s domain gate with margin.

## Attempt 8 (2026-08-28, binary `a4fe9617` = f25 tip + the cloud-driver override): THE PERF GATE PASSES — the venue was the decay

The row moved to AWS (user ruling: sustained verdicts need a thermally
honest venue; this laptop heat-soaks on the same timescale the
sustained window measures). Fleet: 4 × i4i.8xlarge on-demand
(1 client + 1 mds + 2 oss, uniform one-template shape), baked AMI
`squeezefs-mw-base-v2-2026-08-20`, artifacts `task build:ubuntu2604`
at the tip, 8 co-writers + 32 ior ranks co-located on the client —
the field/design shape. Rows `.benchmarks/cloud/2026-08-28-001801/`.

* **A-B-B-A: 0.966 / 0.937 — the S11 gate holds** (shared ≥ 0.8×
  disjoint in BOTH brackets); B-phases dead flat (1,931/1,937 MiB/s
  over 74–75 s windows), A-phases steady 1,865/1,816 with brief dips.
  Probe 1,839 MiB/s (the ≥750 domain gate clears ×2.4).
* **Every correctness column clean**: zero tripwires, zero fsync
  failures, zero settle EIOs, zero untracked/live free refusals, zero
  cap refusals/demotion storms — findings 22–25 all verified live at
  the 8-writer fan-in.
* **ONE engagement failure — finding 26**: the authority's
  `patch/overlay_ineligible_range_shared` moved (prs 15 / ors 29 across
  ~121k ranged acquires), phase-exact with `fold_passes` ≡
  `extent_parks` (A1 2, B1 15, B2 12, A2 0): the fold of
  shipped-assembly extents consulted the range-shared clauses with its
  OWN token, so the holder whose bytes it executed read foreign — the
  clause declined its own designed vehicle on every pass, and the
  demoted-region arm blocked the very publisher the demotion machinery
  routes work to.

## Finding 26 — the arbiter's fold is the holder's proxy (red `b095260f`, fix `e8c56ad4`)

Contracts `tests/mw_arbiter_fold_tests.rs` (single-holder proxy fold
keeps the fast paths; demoted-region fold keeps them; a TWO-holder span
still declines — the over-relaxation pin). Fix: the `sqz_task_local`
arbiter-fold scope (the `AUTHORITY_FREE_SCOPE` precedent) armed by the
rung-17 executors; inside it the clauses ask
`span_range_shared_for_arbiter` — grants from two or more DISTINCT
holders (`owner_nonce`, the merge-scope identity) overlap the span.
Holder-side semantics untouched; spawned subtasks fall back to the
conservative holder form by construction.

Cloud-venue notes: the mw preset now honors `INSTANCE_TYPE` (uniform
fleet, bigger client — `a4fe9617`); campaign cost ≈ $9 (55 min of
4 × i4i.8xlarge + margin). The laptop stays the correctness-crucible
venue; sustained verdicts are cloud rows from here.

## Attempt 9 (2026-08-28, binary `6a616bf4` — f26 landed): f26 verified live; finding 27 named

Same venue/shape as attempt 8 (rows `.benchmarks/cloud/2026-08-28-085328/`,
cluster `sqzbench-20260828-084808`, ~$9). **Finding 26 verified live: no
engagement failure** — the arbiter-fold clauses stayed quiet across all
four phases. A1 1,991 MiB/s (the best shared figure yet), B-phases flat
1,933/1,947. The row failed ONLY A2's sustained window: iteration 14 ran
976 MiB/s (total 10.5 s vs the steady 4.9 s).

**Finding 27 — the recurring shared-phase dip** (a dip-placement lottery
that attempt 8 happened to win): every shared phase carries 2–3
iterations at roughly HALF bandwidth (+~6 s wall). The instrument
columns for A1 (the same shape in every shared phase):
`meta_ship_owner_phase_ns.total.<=8s +122` where every sub-phase
(admit/dispatch/execute/reply_encode) stays sub-16 ms — the 4–8 s lives
BEFORE the first stamped sub-phase (pre-admit queueing) — beside
`dlm_custody_phase_ns.arbitrate.<=8s +1` and the A2-only appearance of
the delegation/recall plane (201 grants, 27 revokes with ack_wait).
Working hypothesis: a custody arbitration that triggers the §9.3
demotion barrier parks waiting for incumbent acks while the ranks run
lockstep (ior inter-iteration barriers) — one ~8 s arbitration stalls
one rank, the iteration's aggregate halves, and ~122 sibling serves
ride out the window queued pre-admit. The red loop owns the exact lock.

Standing shared-phase shape (board, benign-or-not unadjudicated): ior's
"inconsistent file size" warning (stat 112 MiB short — likely attr-cache
lag across mounts, present on PASSING rows) + 4–8 "fsync(15) failed"
warnings per shared phase — identical columns on attempt 8's green row.

## Finding 27 — the standing custody notice poll (red `15c4dee2`, fix `4d1d4b0f`)

Custody schema 7, `VERB_CUSTODY_NOTICE_POLL`: every custody client parks
ONE client-initiated RPC on its authority (spawned at connect,
Weak-held); the authority clamps the park to one renewal cadence,
gathers the client's demotion/shrink notice sets under the same
`FileCustody` serialization every f16a carrier uses, and answers the
instant the barrier's pending-mark hook fires
(`dlm::install_range_pending_hook`, installed at owner arm). The client
absorbs through the same quiesce+ack ladder — the §9.3 ledger still
closes through the ACK column, the renewal remains the worst-case
carrier, the fence column remains the crash backstop, and no push
backchannel exists (the §9.3 paragraph's own vocabulary: "reply-carried
on a client-initiated RPC is not a push" — the delegation recall
channel's exact shape). Contract:
`a_quiet_incumbents_demotion_resolves_at_poll_latency_not_renewal`
(red: the asker burned its full 3 s budget; green: resolution in
milliseconds, ledger closed through acks). New gauges
`dlm_custody_notice_polls` / `dlm_custody_notice_poll_notices`.

## Attempt 10 (2026-08-28, binary `92ac6c37` — f27 landed): finding 27b, the poll starved its own client

The row collapsed at the probe (3.2 MiB/s aggregate, 250–300 s write
latencies — rows `.benchmarks/cloud/2026-08-28-105918/`, torn down at
the probe verdict, ~$4): the f27 standing poll rode `call_once`, whose
lock is the client's WORKLOAD session mutex — a 10 s park held the
session for 10 s and every custody verb queued behind the parked poll.
Fixed on `fix/f27b-notice-session` (red `2f7717e5`, fix `ec6a6612`):
the poll parks on its OWN dedicated `RpcClient` (`notice_session` — the
`lease_session` precedent applied to the third long-lived caller
class); contract
`the_notice_polls_park_never_starves_the_clients_own_verbs` red on the
starvation, green with both f27 contracts after.

## Local probe of `db4e01c8` (f27+f27b, 2026-08-28): f27 verified engaged; finding 28 found FOR FREE

New economy policy (user ruling): every binary passes a FREE local
fleet probe before any cloud spend; the i4i.8xlarge shape runs only the
final sustained verdict. The first probe paid immediately (evidence
`/tmp/rows-probe11`: row `s11mpiio-1787936707`, m0/m51 logs + live
stats; teardown zero-residue):

* **f27 verified engaged**: `dlm_custody_notice_polls` 73 (the standing
  channel lives), `dlm_custody_notice_poll_notices` 1 — and no
  session-starvation (f27b holds).
* **Finding 28** (a shape the fast cloud substrate never surfaced —
  the slow local nvmet-tcp stretches publish latency): in the B2 fpp
  phase, co-writer m51's fold seed-fetch for block 19 of ITS OWN
  private ino (101360) lost 12 settle rounds BACKEND-FRESH
  (`read_settle_lost_serialized` ×12 after 4 f25 stale-head refetches),
  then the fsync `FlushExtents` barrier failed loud — "block key
  '6039797760@163bqi5c9' names a dead incarnation of its device
  offset" — the writeback error latched (POSIX-16), `close()` reported
  EIO, and ior's rank 5 called MPI_ABORT.

  Working hypothesis for the red loop: the f25 heal is
  AUTHORITY-motivated (served commits bypass the merge domain there) —
  on a CO-WRITER a cache-served loss drops the client's own newest
  truth and refetches the SHIPPED durable head, which lags the
  co-writer's in-flight/coalesced publishes; the loop then loses
  backend-fresh forever because the durable cannot name the new binding
  until the very publish parked behind this fsync lands. The settle
  interior's loss handling needs posture awareness: a co-writer's
  cached head is its own coherent truth (its merges republish under
  the merge domain), never the stale-cache class.

### Finding 28 refined (log forensics, same probe)

The refusal originates on the AUTHORITY (m0, 48× "STALE BLOCK-KEY
BINDING refused: key '6039797760@163bqi5c9' names incarnation
3298534883721 (era 3) ... whose live incarnation is 3298534888..."),
one second BEFORE m51 logs its barrier failure — and the SAME stamped
key repeats across retries a minute apart. A re-resolve-per-attempt
would pick up the current head, so the fold's seed reference is
CAPTURED state: the rung-17 assembly for (ino 35825, block) pinned the
block's binding when the extent arrived; the co-writer's later rewrite
displaced that binding (its old offset legally freed post-publish and
re-minted), and every subsequent fold retry re-presents the pinned dead
key — fsync EIO forever on that ino. Red-loop shape: assemble an
extent, displace the block with a newer publish + free, force the fold
— today EIO-forever; the law: the fold re-resolves the CURRENT head per
attempt (or a newer covering publish supersedes/retires the assembly).
Second face (possibly the same root seen from the settle arm): m51's
12 backend-fresh tripwires on ino 101360 block 19.

### Finding 28 corrected (code walk): head REGRESSION, not a pinned seed

`fetch_seed_image` re-resolves the head per attempt (cache ≤1 s, else
the routed fetch), and the "STALE BLOCK-KEY BINDING refused" text is the
ladder's PROPAGATE arm — a fetch refusal on a binding the ladder proved
CURRENT. So the durable head itself named the dead key for a minute:
the authority's fold published a NEW binding for the block and (post-
publish, legally) freed the old offset — then a co-writer's shipped
MERGE whose cached map still named the OLD binding REGRESSED the head
(per-block last-writer-wins in the compose; the rung-20 demoted-region
retention protects assembled blocks only while the region stays
demoted). Every subsequent fold/read of that block then propagates EIO,
and the head cannot heal because the co-writer's next publish is parked
behind the failing fsync. The law for the red test: **the arbiter's
compose never adopts a caller's block binding whose stamped incarnation
is dead** (or older than the durable entry's) — the caller's entry
drops, the durable's stands, and the shipper's stale cache heals
through the served-layout invalidation it already rides. Venue:
`tests/mw_authority_assembler_tests.rs` (owner + compose + demotion
machinery): assemble + fold-publish B2 + free B1, ship a merge naming
B1, assert the head still names B2 and the fold serves clean.

## Local probe of `71f7d749` (f28, 2026-08-28): f28 verified; finding 29 — the transit wall the valve never saw

Evidence `/tmp/rows-probe12` (row `s11mpiio-1787944833`, m0/m53 logs +
live stats). **f28 verified live**: the "dead incarnation" class is
GONE fleet-wide (0 on all nine mounts; A1 completed at 2,519 MiB/s
local — the wedge class is dead). The row then died in B1 (fpp) on
**ENOSPC**: both 64 GiB volumes at 16,372/16,384 blocks with only
~20 GiB live — and `df` AFTER the abort read **11 % used**: the supply
RECOVERED, so this is the free pipeline's TRANSIT population, never a
leak. At the churn rate (~2.5 GB/s) with `free_grace_bound_age_ms`
22,371 (the PR 5 gate (c) ceiling is 12,000), ≈ 55 GiB per volume sat
between `begin_free` and reallocatable; the fleet ate the whole device.

**The valve read 0 the entire time**: `free_grace_pressure_pct` 0 at
99.9 % allocated (its own spec: "100 = the supply is gone at the ring's
own measured deferral rate against the smaller of its headroom and the
volume's free supply"), `forced_releases` 0, `alloc_stalls` 0 — while
`prods` 1,577 and `bound_tightenings` 113,207 churned without ever
biting (bound_age stayed 22 s, fence_bound 76 s). Post-abort gauges
show grace holding only 251 offsets — the transit population lives
DOWNSTREAM of the ring too (release → reclaim → finish_free), so the
pressure model's supply term must count the WHOLE in-transit
population, not the ring occupancy. Red-loop shape: drive churn at a
rate × lag product ≥ supply, assert the valve's graded signal rises and
the tightening floors the bound BEFORE allocation refuses (today:
pressure 0, ENOSPC, fsync EIO, MPI abort). Note: gate (c)'s bound_age
≤ 12 s fails on this venue for the same reason — one fix, two gates.

### Finding 29 corrected (instrument honesty + code walk)

The "pressure read 0 the whole time" claim was a SNAPSHOT ARTIFACT: the
runway reading is TTL-gated (`live_runway_ms` expires within one
routine beat), and the gauges were read AFTER the abort — a stopped
row's pressure always reads 0. The routine harvest DOES carry the real
free supply (`harvest_grace` → `harvest_with_supply`), so the graded
signal was likely high mid-row. What the CUMULATIVE counters prove
(valid post-abort): at the allocation cliff, with the in-transit
population holding ~99.9 % of both volumes and `bound_age` 22.4 s, the
ladder's LAST-RESORT arms never fired — `free_grace_forced_releases` 0,
`free_grace_laggard_fences` 0, `free_grace_alloc_stalls` 0 — while
StorageFull escaped through 47 retries into the application's fsync as
EIO (rank abort). §6.8's own law ("a reader that fails to acknowledge
is FENCED, not waited on, because an unbounded wait converts a slow
reader into the writer's ENOSPC") says exactly this shape must resolve
by release-or-evict at the pressure bound. Red-loop shape (venue
`tests/reader_free_grace_tests.rs`): a cliff harvest
(`harvest_pressure`) against held offsets older than the pressure
bound with a member that has not acked — today the refusal escapes to
the caller with forced_releases 0; the law: the offsets release, the
laggard is evicted, and the allocation retry succeeds — ENOSPC is
reserved for genuinely-live data.

## Probe 3 of `71f7d749` (110 GiB substrate): capacity acquitted; finding 30 isolated

Evidence `/tmp/rows-probe13` (row `s11mpiio-1787945982`). With 2×110 GiB
volumes the ENOSPC class is GONE (2 % used at the abort — finding 29's
cliff was VENUE OVERSUBSCRIPTION at the new post-f27 churn rate; its
filed law, the cliff ladder's release-or-evict, stays on the board as a
correctness item but is not this row's killer). A1 completed at 2,815
MiB/s (13 flat iterations). B1 (fpp) still aborts — **finding 30**, the
f28 note's "second face", now isolated: co-writer m55's fsync died
"block 13 of inode_58464 did not settle after 4 serialized stripe-held
settle attempts" with ZERO dead-incarnation refusals fleet-wide (f28
holds). Mechanism (the f25 heal's posture blindness): on a CO-WRITER
the settle's backend head is the SHIPPED durable — the authority's
committed truth, which lags the co-writer's own in-flight publishes —
while the f25 heal actively DROPS the co-writer's coherent cache in
favor of that lagging durable; under fpp rewrite churn the four
attempts each fetch a binding the mount's own writes already retired,
and the exhaustion EIOs the fsync barrier (rank abort). The law for the
red loop: the f25 cache-drop heal applies only where served publishes
exist (the authority); a CO-WRITER's cached head is its own coherent
truth (its merges republish under the very merge domain the settle
holds), and a backend-fresh loss there is the mount's own publish LAG —
resolved by draining/awaiting its own in-flight publish, never by
exhaustion EIO.

## Probe 4 of `198dbbea` (f30, 2026-08-28): the abort class is DEAD — the local venue is now the binding constraint

Row from zero (2×110 GiB, 8 co-writers): **zero settle tripwires, zero
"did not settle" exhaustions, NO MPI abort** — finding 30's class is
gone (the enriched tripwire named the mover in one probe: the Freed
retire's orphaned unstable word; fix = the W1 restore idiom). A1
completed at 2,774.9 MiB/s (15 iterations); B1 ran ALL 15 iterations
and failed only the SUSTAINED gate (1,343 → ~600 MiB/s), with the decay
tracking (a) the thermal driver's clamp (3.0 → 2.14 GHz, Tctl 68 °C)
and (b) the free-pipeline transit population refilling the 110 GiB
volumes to 28,153/28,160 blocks by B1's tail (three fsync StorageFull
EIOs on the last iterations, absorbed without abort). Both terms are
the VENUE's: the laptop cannot hold the post-f30 churn rate inside its
RAM budget or its thermal envelope. Residual (standing board): two
FlushExtents "Lock expired or invalid fencing token" retries on the
shared ino during A1, absorbed by the POSIX-5 ladder.

Verdict per the cheap-first pipeline: the FUNCTIONAL probe is green —
attempt 11 (the sustained verdict) belongs to the cloud venue
(3.75 TB devices ⇒ no transit wall; datacenter cooling ⇒ honest
sustain).
