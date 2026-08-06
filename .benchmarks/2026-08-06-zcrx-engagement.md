# 2026-08-06 — The zcrx ENGAGEMENT campaign (copy-elimination phase 2): geometry re-derived to serve the bulk; dest-lease compose adjudicated; field rows specified

Branch `perf/zcrx-engagement` (worktree off `integrate/zcrx-wave` tip
`3415378b`, **unmerged — the orchestrator merges**). Charter: invert the
round-8 engagement economics (`.benchmarks/2026-08-05-zcrx-z3-field-rows.md`
— the lane parked correct-and-safe at **0.05 %** engagement, unable to pay
its RSS rent) under the read CPU-wall ruling
(`.benchmarks/2026-08-06-read-cpu-wall.md`: reads whole-box CPU-bound at
27.9 of 41.8 raw after the dest-lease; the bar is **35.5 = 85 %**; every
zero-copy RX byte is direct capacity). Cluster READ-ONLY — the field rows
run via report (§5). Design: `docs/design-zcrx-read-lane.md` Rev 4.

Commits: red `39f02100` (engagement-geometry contracts, compile-red at
base) · green `be284bf5` (the derivations + design Rev 4 + this note).

## 1. The engagement failure, decomposed (round-8 arithmetic)

The 0.05 % engagement was THREE derivation gaps stacked, none a bug:

| term | round-8 state | effect on the field row |
|---|---|---|
| pool width | flat `channels/4` = 8 of 32 | 10 devices > 8 queues ⇒ **2 of 10 devices laneless** — 20 % of row bytes structurally kernel-path forever |
| admission window | `fill_window / 2` = 32 MiB/queue | the cold row offers ~128 × 4 MiB / 10 ≈ **51.2 MiB/device in flight** — whole-read atomic admission (round 8, correct) DECLINED most of it |
| slack accounting | implicit in the /2 | the halving was really an unpriced delivery-slack budget — retiring it required pricing the slack explicitly |

Wire capacity was NOT a term: Phase-1 measured ~6 GB/s/connection
(4-queue matched row) and 20.9 GB/s single-queue open-loop vs the
~2.8 GB/s/device share the 28 GB/s row needs.

## 2. The derivations landed (all derived, no constants)

* **Census-driven eligible pool** (`steering::lane_eligible_queues`):
  width `clamp(devices_via_nic, channels/4, channels/2)`, highest-indexed
  slice. The finding-C/D device census (already probed at arm) now WIDENS
  the pool exactly as far as fabric breadth demands. Floor = the standing
  ¼ posture (sole-device arms byte-identical to Rev 3 — pinned); ceiling
  = the kernel path keeps ≥ HALF the NIC's RSS width. Field:
  `clamp(10, 8, 16) = 10` → **every device leases one queue** (two-rail
  5/5 split stays at the floor 8, all covered); `fair_queue_want`
  unchanged (`clamp(10/10, 1, 4) = 1` — breadth beats depth). Threaded
  through `rxq_alloc::acquire` and `lane_queue_picks` (the arbiter
  arbitrates within the census pool).
* **Full-window admission** (`area::admission_permits`): the admitted
  payload window is the WHOLE `depth × max_xfer` (the /2 retired). The
  admitted window and the CID namespace are now the SAME arithmetic
  (depth commands of max_xfer). Field: **64 MiB/queue admitted ≥
  51.2 MiB offered** — ≥ 16 concurrent whole 4 MiB reads admit per queue
  (was 8 vs ~12.8 offered, worst case fewer under the round-8
  window-shred the atomic admission fixed).
* **Explicit derived delivery slack** (`area::delivery_slack_bytes` over
  `area::burst_geometry`): an MTU-grain burst lands in `⌈mtu/chunk⌉`
  page-grain niovs (HDS: payload starts a fresh niov), so the fills'
  chunk budget carries `window × (burst − payload)/payload` extra bytes,
  PMD-rounded — **24 MiB at the 9000/4096 field shape** (occupancy
  ~73 %); unknown MTU degrades to `window` (occupancy ½ — the retired
  posture's exact budget). Area = window + slack + ring-standing
  (rounds 6–7 untouched underneath): ≈ **184 MiB/queue** on the field
  rail (8192 descs × 3 × 4 KiB ring standing = 96 MiB), R5-gauged;
  ~1.84 GiB across 10 sessions — Red still blocks arms.
* **Occupancy-aware CQ arithmetic** (`cq_entries_for` /
  `cq_admitted_window_bytes`): worst-case chunk-touch per command =
  `max(⌈max_xfer/chunk⌉, ⌈max_xfer/mtu⌉ × ⌈mtu/chunk⌉)` (351 vs the flat
  256 at the field shape — same 32,768 CQ, now honest); the round-7
  either/or clamp returns spans × occupancy (the `× 2` half-window
  compensation retired with the /2). Field check: CQ-admitted bound
  97.9 MB > the 64 MiB window ⇒ no clamp engages.
* **BDP depth ADJUDICATED AGAINST** (the §8 line superseded in Rev 4):
  the filed follow-on named "BDP depth"; the record rules it out — the
  wire-BDP class is a self-fulfilling equilibrium under demand-concurrent
  venues, falsified twice on the read side (2026-08-01 read-lane
  falsification; the 2026-08-06 queue-wall audit credits the ABSENCE of
  measured-BW-derived terms), and at fabric RTT it derives a ~6 MiB
  window that would decline nearly everything the row offers. Depth
  stays the offered-concurrency slope `clamp(cpus × 2, 4, 64)`
  (MQES-clamped at connect): depth × max_xfer IS the admitted window,
  so the client-concurrency slope is the term that scales admission to
  offered demand. "Admission scaled to offered concurrency" is the §8
  derivation, discharged.
* **Unchanged by design**: whole-read atomic admission + clean declines
  (round 8), the park governor + failover bounds (rounds 5–6), the
  structural-starvation teardown (no-harm — the automatic exit if the
  field row disproves the rent arithmetic), poison lattice, MEM-3
  custody, gather fusion. Lazy re-arm after structural teardown stays a
  filed follow-on: with a sized window the structural trigger should not
  fire, and if it does the row is INVALID by the engagement law and gets
  diagnosed, not presented.

## 3. The dest-lease compose adjudication (campaign item 4 — design §12)

**COMPOSE, not conflict — and the compose's floor on this kernel is ONE
fused gather, not zero.** A leased window arrives at the funnel as a
dest-carrying LBA-aligned ranged read — exactly the Z3 fused-gather
shape; the lane's one requester-side gather lands in the leased ent
window, the lease's binding ceremony is vehicle-blind, and
`read_dest_lease_bytes ⊆ read_dest_dma_bytes` count at the routing serve
regardless of vehicle (ledger closure untouched). Zero daemon passes
end-to-end is NOT expressible: the zcrx area is a NIC-owned
arrival-ordered page pool (no mechanism aims a specific read's C2HData
at a specific ent window) and the FUSE commit copy imports the
REGISTER-time ent VA (the dest-lease kernel adjudication §1A), so the
area can never BE reply-reachable memory.

**What phase 2 alone buys, priced**: on lane-served bytes the kernel
nvme-tcp RX skb copy — 0.69–0.72 passes/byte at the measured
~2.7 GB/s/core skb-walk class, the CPU-wall ruling's named residual —
is replaced by NIC DMA (0.035 B/B zcrx-side residual, Phase-1) plus one
userspace NT gather at memcpy class (~3–4× cheaper/byte,
non-cache-polluting). Dest-leg daemon passes go 0 → 1 while kernel
passes go 0.69 → 0: net ≈ −0.2 to −0.3 core-s/GB per lane byte on a box
measured 92 % busy at 27.9 GB/s — ~6–8 cores freed at line rate, the
capacity the 35.5 bar needs if the row is CPU-elastic as the ruling's
arithmetic says. Pooled fills (tier/hold) keep the Z2 two-pass shape
with the kernel pass killed. Pinned:
`test_compose_dest_lease_shape_rides_the_fused_lane` (the lease's exact
window shape — 4 KiB-aligned non-block offset, odd 4 KiB-multiple
sub-block length — through the fused arm, byte-exact closure).

## 4. Red evidence + verification

Red `39f02100` — compile-red at base (`lane_eligible_queues` had no
census input; `area::admission_permits`/`delivery_slack_bytes` did not
exist) and assert-red where signatures survive: the halved window
admitted 8 whole 4 MiB reads vs the ≥ 12 the offered row needs.

Suites green (serial, dev profile, from the campaign worktree):

* `zcrx_lane_tests` **63** (58 + the 5 engagement/compose contracts),
  `zcrx_steering_tests` **23** (incl. the census-widened picks law),
  in-module `--lib zcrx` **37** (TEST-6 venue: full-window admission,
  occupancy-aware CQ pins).
* `derivation_sweep_tests` **20** (incl. the new
  `zcrx_engagement_geometry_derives_on_canonical_shapes` drift-is-red
  tie test).
* Read-path battery: read_lane, read_dest_lease, read_copy_ledger,
  ranged_read, hybrid_io, read_prefetch_pipeline, read_prefetch_window,
  read_tier_admission, read_admission_governor, read_saturation,
  rebind_starvation, read_serve_phase, nvme_dev, nvme_dest_ownership —
  green (counts in the session log).
* ×10 consecutive on the touched async suites (`zcrx_lane_tests`) —
  10/10.
* `cargo clippy --all-targets --all-features -- -D warnings` AND the
  shipped default-features config — clean; `cargo fmt --check` clean;
  markdown link check clean.

Sim scope stated honestly (design §13): the sim pins every law up to
the io_uring syscall seam; the ENGAGEMENT geometry itself (real mlx5
queues, real RSS width, real ntuple verdicts, real C2HData grain) is
field-only.

## 5. THE FIELD ACCEPTANCE ROWS (via report — cluster read-only)

Instrument: `tests/fio/zcrx_field_rows.sh` (now printing share/closure/
waits/parks/failovers per row) on squeeze-test (sqz kernel, ntuple ON on
both rails from the earlier campaign, 10 data namespaces, MTU 9000,
32 RX queues/rail), pair built from this branch. A-B-B-A lane-on/off
remounts over one prefilled beyond-RAM fileset; 60 s+ sustained cold
seq-read + rand-4k per side; verdicts read as SIDE MEDIANS (the
aging-store rule):

1. **Arm-time geometry snapshot**: 10 sessions armed (one per device —
   the census pool), RSS width 22/32 per rail recorded, `zcrx_area_bytes`
   ≈ 10 × ~184 MiB, mount log naming the derived numbers.
2. **Engagement laws (any violation ⇒ INVALID row, loud)**:
   `zcrx_fill_bytes` ≥ 50 % of row user bytes (round 8: 0.0005);
   `gather ≡ fill` byte-exact; `dest_gather` accounts the dest-leg share
   (≈ gather on lease-armed rows); `poisoned = 0`,
   `frame_violations = 0`, `fallbacks ≈ 0`; `admission_waits` bounded
   (the sizing instrument — sustained growth at < 100 % engagement names
   an under-derived term, never a hand-tune); parks/failovers bounded
   episodes; pristine teardown after every mount (0 rules, full RSS).
3. **The performance verdict**: daemon cores/GB + %sys/%soft DOWN on the
   lane side at ≥ par GB/s (the RSS rent accepted or refused by exactly
   this row — the no-harm teardown stays the automatic exit); then the
   COMPOSED row (lane + dest-lease) against the **35.5 GB/s bar**.
   Expected shape per §3's pricing: the ~16 cores of %sys RX copy on the
   27.9 GB/s row shrink toward the zcrx softirq residual, and the freed
   passes convert to GB/s if the row stays CPU-elastic.
4. **A rand-4k face per side** (the IOPS regression guard — round 8 lost
   −7.6 % to pure rent at 0.05 % engagement; an engaged lane must hold
   par or better).
5. **After the row**: default-on adjudication per the user sequencing
   (2026-08-03 — default-on last, only on a counted win).
