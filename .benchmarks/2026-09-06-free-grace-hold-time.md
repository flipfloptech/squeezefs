# 2026-09-06 — finding 15's remaining half: the free-grace loop's HOLD TIME, decomposed and cut

| | |
|---|---|
| **Branch** | `perf/free-grace-hold-time` off `dev` `a05998c5` |
| **Commits** | `ba42f3f9` (red: the decomposition instrument) · `d570376a` (the instrument) · `c2163d04` (red: the fleet-cadence loop, the capacity law, levers b/d, the co-writer gauges) · `0104e7cb` (levers b + d, the capacity-law gauges) · `a1687b54` (lever (a) measured inert) · `7ec426b8` (lever (d)'s derivation + tie test) · `30041cd2` + `70d0ca6c` (docs) |
| **Design** | `docs/design-free-grace-sustain.md` §"Hold-time campaign" (this note's home); the §3 rate equation is the input |
| **Program input** | `.benchmarks/2026-09-06-cowriter-free-refcount-leak.md` §6 (the 2026-09-06 00:15 fleet row: the ring HOLDS the supply — `free_grace_bound_age_ms` 8,994 at every 1 Hz cadence, `offsets` 3,171, lane ENOSPC refusals 76–129 per co-writer, A1 not sustained 1,270 → 482 MiB/s); `.benchmarks/2026-09-05-d4-free-grace-sustain.md` (the closed-loop instrument this campaign extends); `.benchmarks/2026-08-25-s11-freeloop-stall.md` (finding 15) |
| **Instrument** | `tests/reader_free_grace_tests.rs` §"The hold-time campaign" — the D-4 closed loop on the manual owner clock, now with the writer's checkpoint model (a member's pass advances iff a checkpoint landed since its previous pass), staggered pass AND renewal phases, and the per-stage decomposition read off the product's own `free_grace_hold_phase_ns`. Release build, `cargo test --release --all-features --test reader_free_grace_tests -- --nocapture --test-threads=1 the_hold_at_fleet the_hold_time_levers a_lane_exhausts a_faster_checkpoint`. No substrate, no wire, no wall clock; deterministic (bit-identical across runs and profiles). |
| **Evidence tier (rc-manifest)** | every number below is *measured-real in-process* — one process, product code deciding every step; the fleet numbers cited are the 2026-09-06 row's, *measured-real* on the single-node proving fleet. The fleet acceptance row is **OWED (parent)** — §6 |
| **Class** | perf — the release latency of the freed-offset grace loop (the sustained shared-file rewrite ceiling on a lane-partitioned fleet is `spare ÷ hold`) |

## 1. What the fleet said, read against the code

The parent's reading was "a sum of fixed ~1 Hz cadences: free → journal →
the next checkpoint → the member's next poll → its ack → the harvest ≈ 9 s".
Half right. The reader's acknowledgement ladder does **not** wait on the
checkpoint or on an event at all — it waits out two DERIVED windows
measured from the instant it LEARNED a label (`src/free_grace.rs`
`ReaderAckLadder::note_pass`, the three gates):

| Term | Value on the fleet | Derivation |
|---|---|---|
| qualify | `S + skew` = **2,022 ms** | `S = reader_staleness_bound = P + CHECKPOINT_MAX_AGE_MS` = 1,000 + 1,000; skew 22.5 ms (`revalidate.rs:156`, `membership.rs:1128`) |
| drain | `S + D_purge` = **4,000 ms** | `D_purge = 2 × P` (`membership.rs:1140`) |
| **the windows** | **6,022 ms** | the "physics" the sustain design left untouched (KD-FG-1/KD-FG-11) |

Everything else IS cadence quantization, and the fleet's own gauges place
it: the members' `free_grace_acked_lag_ms` 6,578–7,031 (learn → promote:
the windows plus the qualify pass's grid rounding), `learned − acked` ≈
7,000 on every member, `bound_age` 8,994 — so ≈ 2 s sits BETWEEN a member's
promotion and the bound advancing: the carry to its next beat (≤ 1 s), the
min-composition over 8 members, and the bound refresh (`bound_refreshes`
140 over the phase — one per ≈ 1.5 s). The checkpoint plays no part once
every pass advances, and on the storm volume it does: `meta_kv_revalidate_polls`
438 over 2 meta volumes = 219 passes, `epochs` 264 — the storm volume
advances on ≈ every pass (the idle sibling supplies the rest).

## 2. The instrument (`d570376a`)

`free_grace_hold_phase_ns` — the shared `latency_core` histogram in the
`*_phase_ns` shape, stamped per released offset, read off the machinery:

| Phase | Read from |
|---|---|
| `defer_checkpointed` | the first KV checkpoint completed at or after the defer — a mark the checkpoint task records the instant the ledger record naming the new roots is WRITTEN (`checkpoint.rs`, after `META_KV_CHECKPOINTS`; `free_grace::note_checkpoint_completed`, one `ArcSwap` load on a mount with no plane) |
| `checkpointed_min_acked` | the first bound ADVANCE covering the label — recorded in `publish_bound` itself |
| `min_acked_released` | the harvest's visit |
| `total` | `release − defer` |

Exact-sum per placed sample (`free_grace_hold_unplaced` counts the rest —
0 on every in-process row). Both mark deques prune to the routine fence
bound (the longest an offset can be held), so their population derives
from `fence ÷ period` — never a constant — and each lookup is one
partition point taken with the ring lock released. Beside it:
`free_grace_hold_ms` (the live EWMA of the residence — the measured hold
the capacity law multiplies churn by), `free_grace_checkpoint_marks` /
`free_grace_checkpoint_cycle_ms` (the marks and the measured cycle cost),
and **`free_grace_member_ack_lag_ms`** `{max, mean, min, members}` — owner
clock `now − acked` per live member (`now − joined` for one that acked
nothing), the min-composition's culprit finder — with the per-member census
under `SQUEEZEFS_STATS_KEY_CENSUS` (it names peers; the
`dlm_custody_grant_census` law). Contracts 23–25: three stages exact-sum
with the total (700 / 4,300 / 400 = 5,400), the lag names the binding
member, a real `KvMetaBackend::checkpoint_now` records exactly one mark on
an armed plane and none without one.

## 3. The decomposition, measured (release, deterministic)

The fleet-cadence shape: 8 members, checkpoint 1 s, pass 1 s, prodded beat
1 s (the floor), lane-0 stream 80 MiB/s against a 500 blk/s storm, spare
256 blocks, 300 s owner clock, phases staggered. The 2026-09-05 binary's
configuration (D-4's A3 — pipeline + demand on, the hold-time levers off):

| Stage | ms (mean) | What it is |
|---|---|---|
| `defer_checkpointed` | 498 | ½ a checkpoint period — **in the shadow of the qualify window** (§1: the reader waits the bound, not the event) |
| `checkpointed_min_acked` | 7,888 | learn (≤ 1 beat, mean 0.5 s) + qualify 2,022 rounded up to the pass grid (mean ≈ 2.5 s) + drain 4,000 + carry (≤ 1 beat) + the min over 8 members + the refresh (≤ 1 floor) |
| `min_acked_released` | 9 | the harvest runs per free and per allocation |
| **residence** | **8,396** | `free_grace_hold_ms` 8,010; `bound_age` mean 8,646 / max 8,750 (the fleet: 8,994) |
| per-member ack lag | max 7,750 / mean 7,375 | the fleet's `acked_lag_ms` 6.6–7.0 s + carry |

So of the 8.6 s: **6.0 s is the two derived windows**, ≈ 0.5 s the label
learn, ≈ 0.5 s the qualify pass's grid rounding, ≈ 0.5 s the carry, ≈ 0.5 s
the refresh, and the min over 8 members stacks the jitter. The parent's
model was right about the cadences and wrong about their size: the four
sub-second terms sum to ≈ 2.5 s, not 9; the 6 s core is `S + skew + S +
D_purge`, and every one of those four is itself `P` or the 1 s checkpoint
ceiling — "fixed 1 Hz cadences" at the root, which is exactly why no lever
below the windows can move more than ≈ 2 s.

## 4. The levers

### 4.1 (b) carriage renewal — `SQUEEZEFS_FREE_GRACE_ACK_RENEWAL` (default on)

A promotion (`reader_pass_completed`) wakes the member's renewal loop
(`membership::renewal_wake`, a first-party `Notify` the loop now parks on
beside its beat — `sqz_time::timeout(due, notified())`) and the tick renews
as **carriage** (`MemberClient::renew_carriage` →
`MemberLeaseWords::renewed_carriage`): the lease renews, a prod riding the
grant is honoured (the beat only ever comes forward), and the grant's
**label is not learned**. That last point was measured, not designed: the
first cut re-anchored the beat and learned the label, and read **+125 ms**
(8,771 vs 8,646) — a label learned just after a pass (which is when a
promotion happens) qualifies a whole pass later than one the routine beat
learns at its own phase, and the ladder adopts the LAST learned pair. Pure
carriage keeps the beat's phase and leaves the label to it. Cost: ≤ 1
extra renewal per promotion (one per pass at most); renewing early is
always safe under §6.7 (`T_self` bounds NOT renewing). Contract 28.

### 4.2 (d) refresh on a binding ack — `SQUEEZEFS_FREE_GRACE_REFRESH_ON_ACK` (default on)

`MembershipOwner::renew` marks the bound dirty when a member whose recorded
ack sat **at or below the published bound** advances it — one compare in
the plane's hot op (KD-FG-4's no-scan-in-renew law stands) — and the next
harvest recomputes the minimum at once instead of at the next floor beat /
sweep, rate-limited by `refresh_on_ack_interval_ms(floor, members, scan) =
max(floor ÷ members, 2 × scan)` (the rate the min can change at; never a
busy recompute — tie-tested `derivation_sweep_tests`). The harvest now reads
the bound AFTER both recompute arms, so a recompute releases in the same
pass. Contract 29.

### 4.3 The rows (release, the fleet-cadence shape; every row closure OK, forced = fences = 0, stalls 0)

| Config | `bound_age` mean / max | residence | `defer→ckpt` | `ckpt→min_acked` | `min_acked→rel` | `hold_ms` | ack lag max / mean | engagement |
|---|---|---|---|---|---|---|---|---|
| A3 (2026-09-05 binary: pipeline + demand, hold levers off) | **8,646** / 8,750 | 8,396 | 498 | 7,888 | 9 | 8,010 | 7,750 / 7,375 | — |
| H1 = A3 + (b) | 8,396 / 8,500 | 8,153 | 499 | 7,645 | 9 | 7,762 | 7,500 / 7,000 | `ack_renewals` 2,315 |
| H2 = A3 + (d) | 7,974 / 8,000 | 8,025 | 498 | 7,523 | 3 | 7,820 | 7,750 / 7,375 | `bound_refreshes_on_ack` 1,144 (`refreshes` 300 → 1,158) |
| **H3 = A3 + (b) + (d) — SHIPPED** | **7,724** / 7,750 | **7,841** | 498 | 7,339 | 3 | 7,695 | 7,500 / 7,000 | 2,315 / 1,431 |

**−922 ms on `bound_age` (−10.7 %), −555 ms on the residence**; the two
levers compose (each pays alone, neither undoes the other — contract 30).
(d) pays more than (b) because the carriage renewal delivers the ack to an
owner that still refreshed on the 1 s cadence; with (d) the arrival IS the
refresh. Lane-lane cost: renewals 1,602 → 3,203 over the storm (one carriage
per promotion), recomputes 300 → 1,446 (≤ `members` per floor by the limit).

### 4.4 (a) the checkpoint cadence — measured INERT alone, not landed

Contract 31 runs the shipped configuration with the writer checkpointing at
1,000 / **500** / 1,650 ms:

| checkpoint period | `bound_age` | `defer→ckpt` | reading |
|---|---|---|---|
| 1,000 (shipped) | 7,724 | 498 | — |
| 500 (the parent's lever (a)) | **7,724** — identical to the tick | 248 | only the shadowed stage shortens: the reader qualifies on `learned + S + skew`, a TIME bound derived from the checkpoint CEILING constant, not on observing the checkpoint |
| 1,650 (the epochs/polls ≈ 0.6 shape) | 8,386 (+662) | 824 | checkpoints sparser than the passes DO cost: a pass that finds no new root cannot qualify, and the rounding grows |

So a pressure-coupled checkpoint cadence, alone, buys nothing on the fleet
(whose storm volume already advances every pass). It pays only as the
**writer → member composite**: the writer's live checkpoint ceiling under
demand (`P/2` — the Nyquist bound that makes every pass advance) carried
to the members as the prod floor AND L2b's pass floor, so passes and beats
run at `P/2` too — cutting the learn, qualify-rounding and carry terms by
half each (≈ −0.6 s predicted). That changes L2b's pinned floor law
(`a_prodded_grant_tightens_the_pass_cadence_with_the_checkpoint_floor`:
"an ask below `CHECKPOINT_MAX_AGE_MS` clamps up — physics") — whose
INPUT (the writer's ceiling) would become demand-elastic — and the prod
floor's derivation (`ack_refresh_floor = max(P, skew)`), at 2× the
lease-lane load and 2× the checkpoint cycles under demand. An adjudication
item, filed in §7; not landed here.

### 4.5 (c) a sub-second prod floor — not landed, by the same arithmetic

With (b) the ack no longer waits on the beat, so the beat carries only the
LABEL. A faster beat learns a fresher label, but the ladder adopts it at the
next PASS — so a beat faster than the pass cadence shortens only the free →
learn term (mean 0.5 → 0.25 s at 2× the lease-lane load) and nothing
downstream. It belongs to the composite of §4.4, not on its own.

### 4.6 (e) the capacity law, published (`0104e7cb`)

`alloc_lane_share_needed_blocks = ceil(claim-rate × horizon) + live`
(`live = share − reachable − owed`; the horizon is L5's measured hold + RTT
+ one floor) and `alloc_lane_headroom_pct = (share − needed) ÷ share`,
saturating at 0 — re-derived with the watermark at every rate sample on a
laned co-writer, 0/0 unpartitioned. A lane exhausts exactly when the
headroom reaches 0. Pinned on the closed loop (contract 27, the hold
MEASURED): spare 0.6 × demand × hold → 636 stall slices, 11.9 blk/s; spare
1.5 × demand × hold → 0 stalls, the offered 20.0 blk/s. And on the
allocator (`mw_cowriter_free_tests::the_lane_share_needed_and_headroom_publish_the_capacity_law`):
needed = watermark + live to the block, the stats inode publishes both, an
exhausted lane reads 0 %.

**The fleet's numbers through the law:** 4 GiB lanes (1,024 blocks), a
co-writer's live share ≈ 1.25 GiB (320 blocks), churn ≈ 200 MiB/s (50
blk/s), hold 9.0 s → needed = 450 + 320 = 770 of 1,024 (headroom 25 %) —
at the edge, exactly where the row's refusals sit once the min-composition
tails past 9 s. At the shipped H3 hold (7.7 s): 385 + 320 = 705 (31 %).
At the §7 composite's predicted 7.1 s: 675 (34 %). At the windows'
re-derivation (§7, ≈ 3.5 s): 495 (52 %).

## 5. Gate (this side)

* Suites, `--all-features -- --test-threads=1`: `reader_free_grace_tests`
  **48/48** (39 + the 9 hold-time contracts), `dlm_membership_tests`,
  `mw_cowriter_free_tests` **50/50**, `mw_cowriter_free_leak_tests` 8/8,
  `cowriter_enospc_wedge_tests` 9/9, `mw_data_alloc_lane_tests`,
  `derivation_sweep_tests` (+1 tie test), `env_knob_convention_tests`,
  `no_tokio_convention_tests`, `membership_liveness_tests`,
  `mw_cowriter_lane_tests`, `dlm_cowriter_tests` — results verbatim in the
  report.
* Counted ×20: the 9 new `reader_free_grace_tests` contracts — deterministic
  (bit-identical rows every run).
* `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D
  warnings`, `cargo clippy --all-targets -- -D warnings` on the root crate:
  verbatim in the report.
* Loom: `lease_clock_core.rs` gained `renewed_carriage` (three `Release`
  stores + one `fetch_min`; it does not touch the label/anchor pair whose
  order the model verifies) — the loom crate `#[path]`-includes the core,
  `task check:loom` is the parent's gate line; the model itself is
  unchanged.
* No `task check`, no root rigs (the parent's).

## 6. Fleet acceptance — RUN 2026-09-06 09:57 (dev box, 32 CPUs): the levers do what they predicted; the row still fails; the fleet adds a term

`s11-mpiio` from zero on `0723b3ea` (release, both levers at their shipped
defaults), same fleet (1 authority + 8 co-writers, range custody,
`OSS_GB=32`); artifacts `.benchmarks/rows-holdtime-s11-20260906/`. Probe
2,021 MiB/s → 10 GiB file, 15 iterations.

| gauge (m0 unless noted) | pre-campaign (`380ea732`, 00:15 row) | this row | predicted |
|---|---|---|---|
| `free_grace_bound_age_ms` | 8,994 | **7,936 (−12 %)** | ≈ 8.0 s ✓ |
| `free_grace_hold_ms` (residence estimator) | — | 8,809 | |
| `free_grace_hold_phase_ns` mean: `defer→checkpointed` / `checkpointed→min_acked` / `min_acked→released` / total | — | **503 / 7,956 / 2,500 / 10,958 ms** (45,540 samples) | in-process: 498 / 7,888 / **9** / 8,396 |
| engagement: `ack_renewals` per member / `bound_refreshes_on_ack` | — | 212–219 (= every ack rode a renewal) / 493 of 560 refreshes | |
| closure | | deferrals 45,777 ≡ releases 45,545 + offsets 232 ✓; `forced_releases` 0, `alloc_stalls` 0, `laggard_fences` 0 | |
| valve | tightenings 185,054 / demand waits 49,528 | 200,867 / 51,715 (saturated, `pressure_pct` 66 at capture) | |
| lane ENOSPC refusals per co-writer | 76–129 | **23 / 99 / 120 / 109 / 133 / 77 / 130 / 30** (m50…m57) | → 0 ✗ |
| `alloc_lane_headroom_pct` per co-writer | — | 0 / 4 / 3 / 5 / 0 / 10 / 23 / 6 — at the edge, as the capacity law computes | > 0 |
| `CLAIM ANOMALY` per co-writer | 1 / 183 / 33 (three sampled) | 0 / 7 / 323 / 205 / 52 / 3 / 323 / 0 | |
| sustained-window gate | FAIL 1,270 → 482 MiB/s | **FAIL 1,214 → 410 MiB/s** (iterations 1,920 / 1,880 / 1,160 / 1,048 / 770 / 805 / 697 / 260 / 240 / 618 / 582 / 569 / 249 / 252 / 570) | PASS ✗ |

**Verdict — the levers LAND (measured, engaged, no loss); finding 15
stays open with its terms now NAMED on the fleet.** Levers (b) and (d)
took the fleet's hold from 8,994 to 7,936 ms — the −12 % the in-process
rows priced, engagement exact, closure exact, nothing forced or stalled.
The two co-writers with no `CLAIM ANOMALY` lineage (m50, m57) fell from
129 / 76 refusals to **23 / 30** (−82 % / −61 %); the six that carry the
residual refused-free lineage (7–323 anomalies) did not improve — the
remaining leak (`block_untracked_free_refusals` 163, the 148 of the
previous row) is concentrated on them and costs each its headroom.

Three terms remain, in the order the fleet decomposition ranks them:

1. **The coherence windows — 6 s of the 8 s `checkpointed→min_acked`
   stage** (§7's four adjudication items: the qualify window's
   double-counted `P`, the drain's `S` as a cache TTL a purge could
   replace, `D_purge = 2P` reused as a serve drain, the writer→member
   `P/2` composite). Together ≈ 3–3.5 s of fleet hold at the same
   safety, 4 GiB lanes at ≈ 50 % headroom. **These are pinned
   derivations (KD-FG-11); changing them is the user's call.**
2. **A fleet-only `min_acked→released` term of 2.5 s** (9 ms in-process):
   after every member has acked the epoch, the offset waits another 2.5 s
   before it is released — a stage the in-process model does not have.
   Candidate: released offsets re-enter the AUTHORITY's supply, but a
   co-writer's LANE learns of recycled supply only on its own lane
   refresh round trip (the raise/harvest cadence) — the co-writer side of
   the loop, unmeasured until now. Red-first next: instrument the
   release→lane-visible hop per co-writer, then make it event-driven
   (the release carries the lane's refreshed frontier on the next
   custody renewal, which now rides every ack).
3. **The residual refused-free lineage** (`CLAIM ANOMALY` 3–323 per
   co-writer; 163 refusals) — six of eight co-writers carry it, and their
   ENOSPC did not move while the two clean ones fell 60–80 %. Named in
   `2026-09-06-cowriter-free-refcount-leak.md` §6; now the co-writer's
   supply-side face of finding 15.

## 7. What this does NOT do — the adjudication items (the remaining 6 s)

The levers landed remove ≈ 0.9 s of a 8.6 s hold; the shipped predicted
fleet hold ≈ 8.0 s still leaves 4 GiB lanes at ≈ 31 % headroom by the §4.6
arithmetic. What remains is the two derived windows, **6,022 ms**, whose
derivations the sustain design pinned unmoved (KD-FG-11) and this campaign
did not touch — each a coherence-proof change for the user to adjudicate,
named here with its code anchor and its size:

1. **The qualify window double-counts the poll interval.** Qualify =
   `S + skew` with `S = P + CHECKPOINT_MAX_AGE_MS`. The argument for the
   window (`ReaderAckLadder` docs, gate 2) is that the pass's ledger read
   must post-date a checkpoint containing the dereference: the dereference
   commit precedes the free (the reclaim queue sits between), the writer
   checkpoints within `CHECKPOINT_MAX_AGE_MS` of the commit, so a pass
   BEGINNING ≥ `label + ceiling + skew` adopts a root carrying it. The `P`
   term in `S` is the READER's poll interval — the "how stale can a reader
   be" bound — but for qualification the pass itself IS the poll. Honest
   qualify lag = `CHECKPOINT_MAX_AGE_MS + skew` = **1,022 ms**; the extra
   1,000 ms is conservatism, not physics. −1.0 s.
2. **The drain window's `S` is a TTL that could be a purge.** Drain =
   `S + D_purge`: `S` because the daemon layout/attr caches (reader TTL =
   `S`, §6.8 item 4) may still serve a binding for up to `S` after the
   epoch-step purge dropped the block-key census. Dropping those caches
   AT the epoch step (the R-6 purge already drops the five block-key
   stores) would make the term the purge's own duration. −2.0 s.
3. **`D_purge = 2 × P` is a lease-clock reserve reused as a serve drain.**
   `LeaseClocks::derive` sizes `D_purge` as "two revalidation cadences —
   observe, then finish" for the §6.7 fail-stop budget; the ladder reuses
   it as "serves in flight when the purge ran must finish", which is
   milliseconds. A drain term derived from the measured read-serve
   residence (`read_serve_phase_ns.total` p99.9) would be ≈ 0.1 s. −1.9 s.

Together: 6.0 → ≈ 1.1–1.5 s of windows, a fleet hold of ≈ 3–3.5 s, and the
§4.6 headroom at 4 GiB lanes ≈ 50 % — the lane law comfortably met. The
writer→member composite of §4.4 (≈ −0.6 s) is the fourth item, smaller and
also a pinned-law change. None of the four is a lever; each is a change to
the proof `docs/design-free-grace-sustain.md` §"Hold-time campaign" states
verbatim for the adjudication.

Also not done: the fleet row (§6, the parent's); L5's horizon is unchanged
(it already carries the measured hold); `free_grace_member_ack_lag_census`
is exported but the mw rig's harvester does not yet print it.
