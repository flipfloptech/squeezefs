# The per-second fleet sampler — shakedown row, and what the time series exposed (2026-09-07)

The instrument `.benchmarks/2026-09-06-free-grace-term1-fleet.md` §4 named
as owed: `.benchmarks/rigs/2026-09-07-fleet-sampler.py` samples every
mount's `.stats` once a second into `KEEP/samples/<mount>.jsonl` (the
finding-15 columns — the grace loop, the ack ladder, the lane, the rewrite
epochs, the shipped-free ledger, the write pipeline; nulls for gauges an
older binary lacks), `.benchmarks/rigs/2026-09-07-s11-fleet-row.sh` is the
repo-resident row wrapper that runs it around `s11-mpiio` (fleet up →
sampler → matrix → end snapshots → sampler off → teardown; `SQZ_BIN`,
`KEEP`, `LEVERS` and any `SQUEEZEFS_*` lever pass through), and
`.benchmarks/rigs/2026-09-07-fleet-samples-reduce.py` prints the series
aligned to the ior iteration boundaries. Reading a `.stats` inode is a JSON
render (~250 KB per mount per second here); the reclaim manners law keys
on device bytes, so the poller cannot hold a drain deferred.

## The shakedown row (binary `ee49cf04` = the four term-1 items, quiet box)

Same failure shape as the two rows of the term-1 note: 15 iterations
2,463 → 220 MiB/s, NOT SUSTAINED (1,953 → 491), 211 sampler passes over 9
mounts. The end snapshot alone had left the wrong impression; the series
says this:

**1. The grace ring is not the wall.** During every steady writing window
(25–55 s, 65–106 s, 161–166 s) the authority's bound age sits at 1.8–2.2 s,
every member's own acknowledgement lag at 0.65–1.6 s, renewals at
17–22/s, checkpoints at 2/s, the observed drain promoting 1.2–1.8/s. The
`bound_age` spikes to 9–11 s at 5–20, 55–60, 106–121, 151–161, 171–176
and 186 s are the ring holding an epoch-close BURST (held offsets 2,856–
3,867 ≈ a whole iteration's displacement) for the ~5 s it takes the new
labels to be learned, qualified and acknowledged — after which the burst
releases at 600–1,350 offsets/s. Not a stall of any member: in every spike
every member's `acked_lag` reads ≤ 1.6 s and its passes advance 4–7 epochs
per 5 s.

**2. Two distinct starvation cases on the co-writers**, told apart by the
authority's advertised lane supply (`free_grace_lane_supply_hint` — the
count of this lane's blocks on the authority's free lists, SUMMED over
both data volumes, carried on the renewal grant):

| t | mount | reachable | hint | harvests / 5 s | harvested / 5 s | ENOSPC / 5 s |
|---|---|---|---|---|---|---|
| 96–101 s | m50 | 0 → 35 | **512 → 448** | 230 → **2,264** | 181 → **73** | 213 → **2,261** |
| 45–50 s | m53 | 0 → 63 | **498 → 448** | 274 → 725 | 165 → 65 | 212 → 743 |
| 60 s | m55 | 107 | **0** | 1,785 | 128 | 1,783 |
| 80 s | m50 | 328 | **0** | 1,631 | 374 | 1,625 |

* **(a) hint 0** — a genuine shortage: nothing of the lane is on the
  authority's lists because the co-writer's displaced blocks are parked in
  its open rewrite epoch until the epoch closes (the term-1 note's §3; the
  supply-coupled epoch close is the lever, `perf/rewrite-epoch-supply-close`).
* **(b) hint 448–512 while the co-writer's harvests return ~nothing** —
  NOT a shortage. The authority holds hundreds of the lane's blocks and
  the co-writer cannot reach them. The venue has TWO data volumes, each
  giving the lane a 512-block share, and recycled blocks return PER
  VOLUME; the write pipeline does ONE placement pick (`get_active_backend`
  — round-robin inside the 90 %-of-max `health_effective` band, weighted
  by the DEVICE's fill) and allocates on that volume only, so on a
  lane-partitioned co-writer half the picks land on the volume whose lane
  list is empty while the sibling volume holds the supply: a 1 s
  allocation park, a wasted harvest on the empty volume (the authority's
  serve log shows "served 1 block" on one `vol_tag` while the other holds
  hundreds), a refused write. The single-block serves and the 2,264
  harvests for 73 blocks are that shape. The fix is a co-writer's
  placement weight derived from its LANE's reachable supply per volume
  plus an allocation failover across volumes before any park
  (`fix/cowriter-lane-aware-placement`).

**3. The MPI barrier couples the fleet.** From 127 to 152 s m50 rewrote
nothing (`rewritten/int` 0, reachable flat at 657) while another
co-writer finished its slice through a refusal storm — the row's tail
iterations (226 / 220 MiB/s) are one starving mount holding thirty-one
ranks at the barrier.

## What this row does not say

It is a shakedown of the instrument, labeled as such (the box was quiet —
the subagent builds it was meant to overlap had not started — so its
numbers are usable, but it is not one of a same-boot A/B pair). The
before/after pair for the two levers above is the parent's, on this rig,
from zero. Case (b)'s mechanism is read off the placement code + the
serve log + the samples; its contract is the third subagent's red test.
