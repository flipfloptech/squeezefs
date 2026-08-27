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
