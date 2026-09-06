# D-4 — the free-grace sustain campaign: the rate equation closed in-process; fleet acceptance owed

**Date:** 2026-09-05 · **Campaign:** `perf/free-grace-accept` (e2e perf
audit ladder row 13, DLM board #6 — `docs/design-e2e-perf-audit.md`) ·
**Design:** `docs/design-free-grace-sustain.md` (Status flipped here to
*Implemented — fleet acceptance owed*) · **Branch base:** `dev` `27a396e1`
· **Instrument:** `tests/reader_free_grace_tests.rs` §"The sustain
campaign's closed loop" — a deterministic closed-loop harness on the
manual owner clock (release build, `cargo test --release --test
reader_free_grace_tests -- --nocapture --test-threads=1 the_recycle_bound_loop
an_uncoupled_fleet a_still_bound_stream a_mid_supply_lane`); no substrate,
no wire, no wall clock · **Evidence tier (rc-manifest):** every number
below is *measured-real in-process* — one process, product code deciding
every step, the test supplying only the allocator's free-list counts. The
fleet rows this note names are *measured-real* on the single-node proving
fleet or the cloud venue and are cited from their own notes.

## 1. What the brief asked, and what the tree already held

The brief read the design as unbuilt ("execute the PR ladder in order").
The audit found the ladder **landed 2026-08-25**, before the design's
Status row was ever flipped:

| Design PR | Lever(s) | Landed SHA | Red commit |
|---|---|---|---|
| PR 1 | instruments: `bound_age_ms`, `residence_ms`, `demand_waits` (site 0 as a counted observation), `alloc_from_freelist`/`alloc_fresh_mints`, the counting-set wrapper + `alloc_lane_reachable_blocks` | `f992c2e1` | `223feed3` |
| PR 2 | **L1** — the pipelined acknowledgement ladder (`SQUEEZEFS_FREE_GRACE_ACK_PIPELINE`) | `11dafe2f` | `a67e16b3` |
| PR 3 | **L4 + L2 + L2b + L3** — the demand arm: site 0's consumer, rung a′, elastic passes, the widened refresh gate, the KD-FG-10 runway re-base (`SQUEEZEFS_FREE_GRACE_DEMAND`, `SQUEEZEFS_FREE_GRACE_PASS_ELASTIC`) | `29788e55` | `a695063c` |
| PR 4 | **L5** — ahead-of-stall lane refill + the OQ 2 measured horizon (`SQUEEZEFS_ALLOC_LANE_HARVEST_AHEAD`, publish schema 7 → 8) | `69b2fe16` | `ad01e8d0` |
| — | finding 18: an expired prod DECAYS toward routine while offsets are held (`free_grace_prod_decays`) | `6ce59456` | `8da6ac11` |
| — | finding 29: the write path performs the bounded wait the pressure ruling promises (`free_grace_pressure_parks`) | `a50da1e4`, `37bf6036`, `d6c69def` | `d13306b7` |

All four knobs are registered (`src/env_knobs.rs:307-310`); the stats
families are on the inode and in `docs/operations.md`. The field record
since (`.benchmarks/2026-08-25-s11-freeloop-stall.md`, attempts 1–16b):
the levers verified live on the 2 × 32 GiB venue on 2026-08-26 (row 1:
`acked_lag_ms` ≈ 6.7 s, demand site-0 2,258, rung a′ prods, 112
demand-path refreshes, `alloc_from_freelist` 300–950 per member, every
harvest carrying the horizon hint), and the s11-mpiio row went **GREEN
once** — attempt 16b, 2026-08-30, cloud i4i.8xlarge, binary `8e4a2cab`,
`.benchmarks/cloud/2026-08-30-094130` — with findings 16–37 closed under
it.

**So D-4's honest scope is not "build the levers"; it is the two things
the ladder never produced**: (a) the §3 rate equation instrumented as a
closed loop, before/after each lever, on one deterministic clock (the
design's PR 5 named only the fleet row — no in-process row ever drove
labels → ladder → renewal → min-composition → bound → release end to end),
and (b) the closing note + Status flip + ladder-row update, with the
fleet acceptance stated as owed to the parent. One seam defect fell out
of (a) and is fixed here (§5).

## 2. Reading the GREEN row's free-grace columns (why gate (c) reads 23–29 s)

The GREEN row's authority snapshots
(`.benchmarks/cloud/2026-08-30-094130/mw-rows/s11mpiio-1788097295/m0_p{0..4}.json`):

| phase | deferrals | releases | offsets | `bound_age_ms` | `demand_waits` | prods | refreshes | forced / fences / stalls | `alloc_from_freelist` (m0) | `alloc_lane_reachable_blocks` (m0) |
|---|---|---|---|---|---|---|---|---|---|---|
| p0 | 158,325 | 158,066 | 259 | 28,787 | 0 | 16 | 1 | 0 / 0 / 0 | 1,273 | 223,511 |
| p1 | 192,621 | 179,597 | 13,024 | 23,560 | 0 | 16 | 1 | 0 / 0 / 0 | 1,878 | — |
| p2 | 232,187 | 215,468 | 16,719 | 28,632 | 0 | 16 | 1 | 0 / 0 / 0 | 2,058 | — |
| p3 | 271,696 | 257,922 | 13,774 | 23,614 | 0 | 16 | 1 | 0 / 0 / 0 | 2,225 | — |
| p4 | 308,542 | 295,349 | 13,193 | 25,696 | 0 | 16 | 1 | 0 / 0 / 0 | 2,841 | 223,275 |

Closure exact at every snapshot; 100 % of releases in the `>16s`
residence bucket; co-writers `alloc_from_freelist` 0 / `alloc_lane_harvests`
0 / `alloc_fresh_mints` climbing (virgin-only), `free_grace_acked_lag_ms`
6.2–7.0 s, `ack_pipeline_depth` 0–1. **Nothing on that fleet was
recycle-bound** — the authority's lane held 223 k reachable blocks
(≈ 870 GiB) — so site 0 never fired, no prod was issued after the
bootstrap 16, the bound published on the 10 s sweep alone, and the loop
ran at the ROUTINE composite (§3.2's T1–T8: ≈ 24–29 s). The row is green
because the venue's supply dwarfs λ × L_lag (13 k blocks ≈ 52 GiB parked
without consequence), not because the loop tightened. PR 5's gate (c)
(`bound_age ≤ 12 s sustained`) is therefore a statement about a **coupled**
storm — §4's uncoupled row reproduces this reading in-process under every
lever configuration — and the from-zero row that can adjudicate it is the
finding-15 venue (2 × 32 GiB, 4 GiB lanes), where the lanes DO couple.
That row is the owed one (§6).

## 3. The in-process closed loop (the instrument)

`LoopShape` (`tests/reader_free_grace_tests.rs`): one armed owner + N sim
readers on one manual `LeaseClock`; one `GraceRing`; a **starving stream**
(the authority's lane 0 — spare `S` blocks, offered demand μ blk/s)
allocating through the funnel's order — routine harvest → free list →
virgin mint → the `StorageFull` arm's pressure harvest → counted refusal +
the finding-29 park slice — with every landed rewrite displacing one
lane-0 block into the ring; a **storm** (the co-writers' shipped frees,
λ blk/s, lanes 1–8) entering the same ring with the per-terminal-free
routine harvest; readers passing every revalidation interval (1 s, every
pass an epoch step — a storming writer checkpoints continuously) through
`ReaderAckLadder::note_pass` with the shipped `qualify_lag`/`drain_lag`,
renewing through `MembershipOwner::renew` on the cadence their last grant
carried (the prod's delivery), and the owner sweeping `refresh_free_grace_bound`
every `renew_interval`. Foreign-lane releases accumulate on the passed-global
number and are reachable to nobody (the row's `alloc_lane_harvests 0`);
`grace_supply_blocks`' lever resolution is mirrored (lane-reachable under
`DEMAND=1`, passed-global under `DEMAND=0`). Not in the loop: **L5** (rides
the publish wire; its contracts are `tests/mw_cowriter_free_tests.rs` +
`tests/mw_cowriter_lane_tests.rs`) and **L2b** (structurally inert here —
routine pass = the 1 s floor, the s11 venue's own shape). Step 1 ms;
duration 300 s owner-clock; steady window = the last two thirds; renewal
phases staggered across one beat (§3.2's T8 assumption). Shipped clocks:
B = 10 s, S = 2 s, skew 22.5 ms, D_purge 2 s, physics floor 8.02 s,
routine fence 76 s.

Configurations = PR 5 (e)'s A/B: **A0** `ACK_PIPELINE=0 DEMAND=0`
(pre-campaign — the finding-15 part-1 binary), **A1** L1 alone, **A2** the
demand arm alone (§5.2's "degrades to depth-1 pace"), **A3** shipped.

## 4. Rows (release build, `39bdf9e6`; deterministic — bit-identical across runs)

Columns: lane = the starving stream's sustained rate (steady window; thirds
mid / last); stalls = `free_grace_alloc_stalls` Δ over the steady window;
closure = `deferrals ≡ releases + held`; bound_age = steady mean / max;
residence = `free_grace_residence_ms.mean_ns` (the per-offset loop latency
— the §3.3 self-check against bound_age); prods / acks / renewals over the
steady window.

### 4.1 `coupled` — S = 256 blocks (1 GiB), μ = 20 blk/s (80 MiB/s), λ = 500 blk/s (≈ 2 GiB/s), 8 readers

The finding-15 shape scaled: the lane's spare is below μ × the routine
latency, so the pre-campaign stream is paced by releases.

| cfg | lane MiB/s (mid / last) | stalls | held | closure | forced / fences | bound_age mean / max (ms) | residence mean (ms) | prods | demand_waits | refreshes | acks / renewals |
|---|---|---|---|---|---|---|---|---|---|---|---|
| A0 | **72.1** (71.7 / 72.4) | 179 | 4,859 | OK | 0 / 0 | 11,751 / 14,790 | 12,414 | 1,600 | 105,133 | 127 | 232 / 1,600 |
| A1 | 69.2 (69.5 / 68.8) | 163 | 6,586 | OK | 0 / 0 | 11,887 / 17,663 | 12,522 | 1,108 | 109,385 | 121 | 1,176 / 1,158 |
| A2 | 73.2 (72.9 / 73.4) | 234 | 6,741 | OK | 0 / 0 | 13,170 / 14,750 | 12,830 | 1,602 | 134,943 | 300 | 225 / 1,602 |
| **A3** | **80.0** (80.0 / 80.0) | **0** | 4,552 | OK | 0 / 0 | **8,646 / 8,750** | 8,397 | 1,602 | 3,827 | 300 | 1,608 / 1,602 |

* **The shipped configuration UNBINDS the stream**: 80.0 MiB/s = the
  offered rate, flat to the third decimal across thirds, zero stalls, with
  `bound_age` 8.65 s (max 8.75 s — the min-composition at floor beats plus
  the L3 refresh at the floor leaves no sawtooth) against the design's
  post-fix budget of 9–12 s and the 8.02 s physics floor. **Gate (c) is
  MET in-process on the coupled shape.**
* **Neither lever alone does it.** A1 (pipeline, no demand): the prods
  come only from the cliff's pressure harvests and lapse between stalls,
  so members draw routine 10 s beats and the pipeline has nothing to
  pipeline — 69.2 MiB/s, max bound_age 17.7 s (the finding-18 sawtooth).
  A2 (demand, no pipeline): the floor beat is delivered continuously
  (1,602 renewals ≈ 8 × 200 s) but the depth-1 ladder promotes one label
  per qualify+drain (≈ 7 s), and the min over 8 readers adds up to one
  such quantum — 13.2 s, 73.2 MiB/s. Exactly §5.2's stated degradation;
  the composition is the campaign.
* `demand_prods` reads 0 on every row **by the ledger's own definition**:
  it counts prods the space arm would NOT have issued, and under the
  KD-FG-10 re-base the space arm reads the same lane-reachable trough and
  asks the floor itself. Site 0's observation (`demand_waits`) is what
  says the coupling was seen; A3's 3,827 (vs A2's 134,943) is the trough
  being LEFT — the arm working itself out of a job.
* A0 here is NOT the field's 28.6 s: the in-process stream reaches the
  `StorageFull` cliff, where part 1's valve (residual 6) already prods at
  the floor through the pressure harvest — 11.8 s with 179 stall slices.
  The field's decay row was paced WITHOUT reaching a refusal (the term
  §3.3 could not pin; PR 1's capture falsified hypothesis (a) on the
  self-sized row). The pre-campaign field composite is §4.4's uncoupled
  row, below.

### 4.2 `bound` — S = 128 blocks (512 MiB): still recycle-bound post-fix — Little's law

| cfg | lane MiB/s (blk/s) | stalls | held | closure | forced / fences | bound_age mean (ms) | residence mean (ms) | `S ÷ bound_age` (blk/s) | acks / renewals |
|---|---|---|---|---|---|---|---|---|---|
| A0 | 36.4 (9.10) | 503 | 4,849 | OK | 0 / 0 | 11,398 | 12,264 | 11.2 | 232 / 1,600 |
| A1 | 60.0 (15.01) | 660 | 4,585 | OK | 0 / 0 | 8,286 | 9,042 | 15.4 | 1,608 / 1,600 |
| A2 | 37.3 (9.33) | 540 | 6,613 | OK | 0 / 0 | 13,170 | 12,832 | 9.7 | 225 / 1,602 |
| **A3** | **61.8 (15.46)** | 570 | 4,504 | OK | 0 / 0 | **8,646** | 8,392 | 14.8 | 1,608 / 1,602 |

The §3.3 reconciliation holds on live gauges: sustained rate ≈ spare ÷
bound_age within the bucket/park quantization (pinned at ±30 %), and the
shipped ceiling sits above the pre-campaign one (+70 %) by the latency it
removes (11.4 → 8.65 s). Here A1 ≈ A3 because a continuously stalling
stream keeps the cliff arm's floor prods alive — the pipeline is the
dominant lever ONCE the beats are at the floor; the demand arm's job is
to get them there before the cliff (§4.1: A1 163 stalls → A3 0).

### 4.3 `midsupply` — S = 4,096 blocks (16 GiB): never bound — finding D4-1 priced

| cfg | lane MiB/s | stalls | held | bound_age mean (ms) | residence mean (ms) | prods / renewals (steady) | tightenings |
|---|---|---|---|---|---|---|---|
| A0 | 80.0 | 0 | 12,232 | 17,999 | 18,397 | 0 / 160 | 46,254 |
| A1 | 80.0 | 0 | 13,589 | 20,609 | 20,067 | 0 / 160 | 46,273 |
| A2 | 80.0 | 0 | 6,762 | 13,170 | 12,833 | 1,602 / 1,602 | 153,542 |
| A3 | 80.0 | 0 | 4,552 | 8,646 | 8,398 | 1,602 / 1,602 | 153,385 |

**Finding D4-1 (economy class; pinned as current behavior by
`a_mid_supply_lane_is_asked_at_the_floor_by_the_fleet_rate_divisor`).**
The re-based runway (KD-FG-10) reads ONE lane's reachable supply but
divides it by the RING's deferral rate — on an authority that
`finish_free`s the fleet's shipped frees, the FLEET's rate:
`runway = lane_reachable ÷ fleet_rate`. A lane whose own demand would take
200 s to spend 4,096 blocks reads a ≈ 8 s runway under a 500 blk/s storm,
`cadence_for` answers the floor, and every member beats at 1 s for the
storm's whole duration (1,602 of 1,602 steady renewals prodded; `DEMAND=0`
reads the foreign accumulation and never prods). **What it buys is
inventory, not throughput**: the stream runs at its offered rate in every
row, but parked inventory falls 12,232 → 4,552 blocks (≈ 30 GiB of the
fleet's spare un-parked), because held = λ × L_lag. **What it costs** is
the floor beat on the lease lane for the storm's duration — §5.8's
"pressure-scoped" 10× term becomes storm-scoped — and rung (b)
tightenings at 3.3× (never a fence: the floor is one honest ack cycle,
`forced = fences = 0` on every row). Structurally absent on a solo writer
(its ring rate IS its allocation rate). The per-lane divisor that would
make the runway a forecast is the lane's own claim-rate EWMA L5 already
keeps for the co-writer watermark. **Not changed here**: a divisor change
alters rung (a)/(b)'s reading on the coupled shape too and needs its own
counted fleet A/B (does the un-parked 30 GiB matter at the field's lane
sizes more than the 10× lease-lane load?) — the adjudication input is
this row.

### 4.4 `uncoupled` — S = 100,000 blocks: the GREEN row's shape

| cfg | lane MiB/s | stalls | held | bound_age mean / max (ms) | residence mean (ms) | prods / demand_waits / refreshes | acks / renewals |
|---|---|---|---|---|---|---|---|
| A0 … A3 (identical) | 80.0 | 0 | 14,952 | 23,228 / 27,750 | 23,858 (A0/A2) · 23,812 (A1/A3) | 0 / 0 / 0 | 161 / 161 |

The demand arm stays dark under every lever, the bound publishes on the
sweep alone, and the loop runs the routine composite — 23.2 s mean,
27.75 s max, inside §3.2's 24–29 s derived band and matching the GREEN
row's 23–29 s to the second. The residence self-check (γ) closes: mean
residence ≈ bound_age on every row of every shape once the reset seam
(§5) was fixed. Inventory = λ × L_lag ≈ 15 k blocks (≈ 58 GiB at the
field's rate) is what such a fleet pays, in parked space — the venue's
capacity statement, not a loop defect (the design's non-goal 4: the
routine beat is not retuned).

## 5. The seam defect the rows found (fixed, red-first)

`free_grace::reset_for_test` zeroed the residence histogram's BUCKETS by
hand and left the exact `count`/`sum_ns`/`mean_ns` words that `160650e3`
(audit A, 2026-09-02) added to every latency histogram afterwards — so
`free_grace_residence_ms.mean_ns` accumulated across contracts in one
process: the uncoupled A0 row read a 23,858 ms mean in one test order and
13,889 ms in another for the same deterministic schedule (the first
release capture of §4 carried the polluted column; the table above is the
fixed binary's). Red: `the_residence_histogram_resets_with_the_plane`
(`cc8fa69e`); fix: `LatencyHistogram::reset` — the ONE reset path, buckets
and exact words together — called by the seam (`39bdf9e6`). Product
behavior unchanged (no production reset path exists).

## 6. Fleet acceptance — RUN 2026-09-05 (post-reboot): **NOT MET — finding 15 reproduces, plus a co-writer wedge**

Two from-zero runs on the restored 32-CPU box (`19a651ea`, release; fleet
`SQZ_MWFLEET_OSS_GB=32 SQZ_MWFLEET_RANGE_CUSTODY=1 … --cowriters=8`, then
`tests/run_mw_matrix.sh s11-mpiio`), both identical in outcome; evidence
(all nine daemon logs, every mount's `.stats` at the wedge, the ior
output) in `.benchmarks/rows-d4-s11-20260905/`.

* Probe **1,751 MiB/s** aggregate (well above the ≥ 750 floor; 26-CPU
  caveat gone). File sized 10 GiB (the zram-budget clamp), 13 iterations.
* **Phase A1 fails at 23 s**: `WARNING: fsync(15) failed` on 5 ranks; the
  matrix declares the invocation failed. Chronology on m57 (worst): 13 s
  after arm a fencing-token refusal on a write to the shared file
  (`Lock expired or invalid fencing token: …366 (expected ≥ …367)`), 4 s
  later `rewrite epoch for ino 2 FENCED at close … acked un-fsynced
  rewrite bytes discard with the fenced era`, then the authority
  refusing shipped frees as `already free/graced/quarantined` (the
  double-release lineage — 174–252 refusals PER co-writer), then at 23 s
  **`data volume 'nvme4n1' full: 8183 of 8192 blocks allocated — lane 6 of
  16 is exhausted while 0 free block(s) belong to lanes this mount does
  not own`** on every co-writer (m57: 2,366 refusals). This is the
  08-25 note's signature verbatim (`2026-08-25-s11-freeloop-stall.md`:
  m53 856 / m57 1,185 refusals, `8179 of 8192`): **the sustain levers
  landed 08-25 (PRs 1–4) were never fleet-tested, and the row says they
  do not close finding 15.** The authority at the wedge:
  `free_grace_bound_tightenings` 280,447, `free_grace_demand_waits`
  88,082, `free_grace_prods` 992, `deferrals` 26,395 / `releases` 26,121
  / `offsets` 274 — the valve saturated and the loop still lost to the
  churn (1.75 GiB/s of CoW displacement against 16 lanes of 4 GiB).
* **A second, first-class finding — the co-writers WEDGE.** Five of eight
  co-writers parked writes past station `route-dispatched` (m57: 37,024
  watchdog reports, 100 writes to ino 2 in flight for 30 min); `write-
  phase census … parked in phase entry` ×11,866 and `lock-wait census:
  block/write_checkout … stripe genuinely held — hunt the holder` ×1,044
  per block; m57's FUSE connection sat at `waiting=135`, a `cat .stats`
  blocked in uninterruptible sleep for 30 min, and only the fleet
  teardown's connection abort freed it. An ENOSPC'd write on a co-writer
  lane must FAIL (the ENOSPC-not-corruption ruling — `StorageFull` is a
  refusal), not park indefinitely holding the block stripe lock and its
  write-pipeline in-flight budget; the park is what turns a full lane
  into a dead mount. Load-dependent hangs are first-class product bugs
  (AGENTS): this one has a deterministic repro (two of two).

**Disposition.** D-4's product deliverable (the harness + seam fix) stays
landed; the sustain design's Status goes back to **"Implemented — fleet
acceptance NOT MET"**. Two follow-on items, in priority order:
(1) **the co-writer ENOSPC wedge** — red-first: a co-writer whose lane is
exhausted must return ENOSPC to the writer promptly, release the stripe
lock and the pipeline permit, and keep the mount responsive (the fleet
repro is `rows-d4-s11-20260905/`; an in-process repro needs a full-lane
allocator on a co-writer posture); (2) **finding 15 proper** — either the
loop's release latency (the design's §3 terms, now measurable on this
fleet with the PR-1 instruments) or the capacity law (a lane of
`cap/W` must hold the churn × release-latency product — the operator page
must say so with the numbers), adjudicated on this venue from zero.

### Prior attempt (2026-09-05 15:33, pre-reboot — blocked by the box)

**Attempt 2026-09-05 15:33 (dev box, `perf/five` stack `d551f1ba`):**
`sudo SQZ_MWFLEET_OSS_GB=32 SQZ_MWFLEET_RANGE_CUSTODY=1 tests/mw_fleet.sh
create N=1 --cowriters=8` built its tcp devsub instance and then failed
at the writer-identity meta connect (`nvme nvme1: creating 26 I/O queues
… Connect command failed, errno: -18 … failed to connect queue: 15`) — the
same kernel-level `nvme connect` fault the zc gate's live NVMe-reservation
leg hit the same hour: the 1.2.1 fstests CPU-hotplug test (`generic/650`,
09-04 22:48) left CPUs 8, 10, 20, 22, 28, 30 firmware-latched offline
(26 of 32; every re-online attempt "failed to report alive state"), and
default-queue-count nvme-tcp connects now fail at queue 15 while 4-queue
connects (the plain dev substrate's) succeed. Not a product fault (the
D-1c fleet brackets ran on this box the day before; the matrix leg was
never reached) and not this campaign's. **Re-run from zero after the
reboot the latched cores need**, on the quiet box the venue demands
(probe ≥ 750 MiB/s). Recipe unchanged below.


The design's PR 5 rung. Not run here (root + devsub + fleet scripts are the
parent's; four campaigns share this box). **PR 5's harness rung is NOT
landed** — `tests/run_mw_matrix.sh s11-mpiio` gates neither the sustain
columns nor the ≥ 750 MiB/s probe precondition; the columns are read
off the row's per-phase snapshots with the PR 1 rig.

**Venue (finding 15's, verbatim):** one box, quiet (load / thermal /
io-PSI), nvmet-tcp devsub, 1 authority + 8 co-writers, range custody
armed, `SQZ_MWFLEET_OSS_GB=32` (2 × 32 GiB data namespaces — the lanes
that COUPLE; the 2 × 64 GiB venue-sizing lever softens the row and the
cloud venue never coupled at all), release build default features (state
the profile per the two-profile law).

```bash
sudo SQZ_DEVSUB_TRANSPORT=tcp tests/dev_substrate.sh create
sudo SQZ_MWFLEET_OSS_GB=32 SQZ_MWFLEET_RANGE_CUSTODY=1 tests/mw_fleet.sh create N=1 --cowriters=8
sudo tests/run_mw_matrix.sh s11-mpiio            # from zero; A-B-B-A is the leg's own discipline
# the sustain columns, per member, from the row's snapshots (m0 = the authority):
.benchmarks/rigs/free-grace-sustain-rig.sh <rowdir>/m0_p0.json <rowdir>/m0_p4.json authority
for m in 50 51 52 53 54 55 56 57; do .benchmarks/rigs/free-grace-sustain-rig.sh <rowdir>/m${m}_p0.json <rowdir>/m${m}_p4.json m$m; done
# the A/B leg (PR 5 (e) — the pre-campaign shape must reproduce):
#   the same row with SQUEEZEFS_FREE_GRACE_DEMAND=0 SQUEEZEFS_FREE_GRACE_ACK_PIPELINE=0 on every mount
```

**Probe precondition (PR 5 / Issue 3):** the row's own probe ≥ 750 MiB/s,
else the run is label-only venue evidence and restarts from a quiet box
(the 33 → 202 → 2,240 MiB/s swing across box states is the finding-15
note's unresolved venue hazard — separate it from a campaign verdict).

**Verdict columns:**

| Column | Source | PASS reads |
|---|---|---|
| s11-mpiio gate | the leg's own output | shared/disjoint ≥ 0.8× both brackets, sustained thirds flatness ≤ 30 %, engagement exact, cold-authority fsck clean, C8 drift 0 |
| `free_grace_reader_acks` | every co-writer, p0 → p4 | MOVING on every member (≈ 1 per floor beat under a coupled storm, ≈ 1 per 7–10 s uncoupled); a flat member is a wedged ladder |
| `free_grace_forced_releases` / `free_grace_laggard_fences` | authority | **0 / 0** — faster honest acks, never faster fences |
| `free_grace_alloc_stalls` (+ `free_grace_pressure_parks`) | authority + every co-writer | 0 stalls; parks bounded and ending in release, never in fsync EIO |
| release MiB/s vs churn | `free_grace_releases` Δ × 4 MiB ÷ wall vs `free_grace_deferrals` Δ × 4 MiB ÷ wall | releases track deferrals within one floor beat (closure `deferrals ≡ releases + offsets` exact at every snapshot; `offsets` flat, not climbing) |
| `free_grace_bound_age_ms` | authority, every phase | **coupled** (`demand_waits` growing): ≤ 12 s sustained (in-process 8.65 s); **uncoupled** (`demand_waits` flat): the routine 24–29 s composite is the expected reading, NOT a failure — record which regime the row ran in |
| `free_grace_demand_waits` / `free_grace_prods` / `free_grace_bound_refreshes` | authority | the engagement statement: growth = coupled storm seen and answered; 0 = the venue never coupled (then the row adjudicates the acceptance gate but not the campaign's levers) |
| A/B leg | `DEMAND=0 ACK_PIPELINE=0` | reproduces the pre-campaign decay-to-release-rate shape (A1 decay, `offsets` frozen, `bound_age` ≈ 24–29 s) — the counted attribution |

Gate (d) has held on every attempt whose note quotes it (attempt 1,
attempt 3, the f28/f29 probes — forced 0 / fences 0 / stalls 0) and on the
GREEN row's snapshots (§2). Gate (c) has read 12.8–28.8 s on every
attempt that recorded it (attempt 1's end-of-row 12.8 s, the f16a row's
15.5–22.6 s, the GREEN row's 23.6–28.8 s) — each either uncoupled or
aborted for another finding — so it is adjudicated only by the coupled
from-zero row above.

## 7. Gate (this side)

* `cargo fmt --check` clean; `cargo clippy --all-targets --all-features -- -D warnings` and `cargo clippy --all-targets -- -D warnings` clean (verbatim in the report).
* Suites: `reader_free_grace_tests` 39/39 (34 prior + the 5 D-4 contracts), `mw_cowriter_free_tests` 49/49, `dlm_membership_tests`, `derivation_sweep_tests`, `env_knob_convention_tests`, `no_tokio_convention_tests` — verbatim in the report.
* Counted: the five new contracts ×20 (deterministic — bit-identical rows every run).
* Loom: not run — no lock-free core changed (`LatencyHistogram::reset` is a test-seam-only path over relaxed stores).

## 8. What this does NOT do

* It does not run the fleet row (§6 is the parent's) and does not land
  PR 5's harness rung (sustain columns / probe precondition in
  `run_mw_matrix.sh`); the rig reads the snapshots instead.
* It does not change the runway divisor (finding D4-1) — priced and
  pinned, not fixed; the fix needs a counted fleet A/B on the coupled
  venue.
* It does not drive L5 or L2b in the closed loop (wire-bound / inert
  here); their contracts are the landed suites'.
* It does not resolve the venue's probe swing or the field's unpinned
  "paced without refusal" pre-campaign mechanism (§3.3 / PR 1 capture α).
* It does not measure 15 k members; §5.8's arithmetic stands as
  arithmetic.
