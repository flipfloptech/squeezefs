# 2026-09-08 — D-5 owner dispatch venue: the fleet row on squeeze-test

D-5 (`.benchmarks/2026-09-08-d5-owner-hop-and-depth.md`, e2e perf audit
row 18 / DLM board #7) landed the owner-side dispatch split
`meta_ship_owner_dispatch_ns` and the venue lever
`SQUEEZEFS_META_SHIP_INLINE_SERVE` — a served frame (S8 verb frame; S9
publish call / group / free / harvest) polled on the accepting connection's
own thread instead of hopped onto the two shared `sqz-meta` lanes and
joined. It shipped **default on** on in-process evidence: under an
artificial 4 × 200 µs lane hog the S8 dispatch fell 768–781 → 40–65 µs and
verbs/s rose 1.9×; quiet, par. This note is its field row. **The field
says the opposite, in both orders of two brackets: the lever ships OFF.**

## 1. Venue

| | |
|---|---|
| box | squeeze-test, 32-core Xeon, kernel `6.19.14-sqz` + patch 0031, idle before each bracket (load 0.0–0.3 at the first leg) |
| substrate | the mw proving fleet's tcp devsub (nvmet-tcp on localhost, zram OSS `lzo-rle`): `tests/mw_fleet.sh create N=1 --multi-writer --cowriters=8` per leg, torn down to zero residue between legs |
| binary | ONE binary both arms — `squeezefs-F` = `e0bdd35f` (the campaign tip), `profile release`; the arms are the lever's two values |
| row | the D-1b/D-2/C-2 fleet row verbatim (`.benchmarks/rigs/2026-09-02-d1b-fleet-row.sh`): 8 co-writers × 24 concurrent `dd bs=1M conv=fsync` streams from `/dev/zero` — the device term removed by design, the row IS the metadata/publish-plane ceiling |
| brackets | **bracket 1**: 128 MiB streams (3 GiB per co-writer), OSS 64 GiB, order **1 0 0 1** (L1a L0a L0b L1b), rows 3.1–3.3 s; **bracket 2**: 256 MiB streams (6 GiB per co-writer), OSS 128 GiB, order **0 1 1 0** (R0a R1a R1b R0b), rows 6.8–6.9 s. The reverse order is what stops a position effect posing as the lever's |
| rig / analyzer | `.benchmarks/rigs/2026-09-08-d5-fleet-lever-abba.sh` (the D-1c lever rig's shape; `LEGS` names the order) → `.benchmarks/rigs/2026-09-08-d5-fleet-analyze.py` — every mean EXACT (Δsum ÷ Δcount of the authority's `m0_p0`/`m0_p1` snapshots), the p99 a bucket bound |
| validity | every leg: row VALID (closure `shipped ≡ served` to the call on all eight legs: +0), `refusals` 0, `owner_panics` 0, tripwires flat, every stream rc 0; venue engagement EXACT — `owner_dispatch_inline` ≡ dispatches on the `=1` legs, `owner_dispatch_hops` ≡ dispatches on the `=0` legs, the other 0 |
| artifacts | `squeeze-test:/scratch/tmp/sqz-agent/campaign/d5/lever-abba{,-rev}/` (+ `.log`) |

A first attempt at bracket 1 on a 32 GiB OSS ENOSPC'd in its first leg
(kept as `lever-abba.enospc-32g`): 9 writers round the lane partition to
W = 16, so a 32 GiB OSS is a 2 GiB per-co-writer share against a 3 GiB
row — the capacity law (`docs/operations.md` §Multi-writer capacity
planning) doing exactly what it says, not a finding. The 64 / 128 GiB
sizes above satisfy it (4 / 8 GiB shares).

## 2. The owner's dispatch — the rung's own columns

`meta_ship_owner_dispatch_ns`, authority `m0`, exact means per dispatch
(both planes; n = 44–101 k per leg):

| leg | lever | dispatches | queue_hop µs | run µs | wake_hop µs | **total µs** | run p99 bucket | S8 `owner_phase_ns.dispatch` µs |
|---|---|---|---|---|---|---|---|---|
| L1a | 1 | 46,330 | 0 | 1,211 | 0 | **1,211** | ≤ 32 ms | 385 |
| L0a | 0 | 44,142 | 332 | 536 | 187 | **1,055** | ≤ 16 ms | 1,089 |
| L0b | 0 | 45,354 | 418 | 539 | 183 | **1,139** | ≤ 16 ms | 1,314 |
| L1b | 1 | 46,719 | 0 | 1,325 | 0 | **1,325** | ≤ 32 ms | 521 |
| R0a | 0 | 97,566 | 418 | 643 | 211 | **1,272** | ≤ 16 ms | 1,360 |
| R1a | 1 | 100,802 | 0 | 1,473 | 0 | **1,473** | ≤ 32 ms | 473 |
| R1b | 1 | 99,112 | 0 | 1,335 | 0 | **1,335** | ≤ 32 ms | 613 |
| R0b | 0 | 96,754 | 459 | 770 | 237 | **1,466** | ≤ 32 ms | 1,696 |

The mechanism does exactly what it was designed to do: on the inline legs
`queue_hop` and `wake_hop` are **0 to the nanosecond**, and the hops they
delete are real — 515–696 µs per dispatch on the hop legs, half of the
hop-arm dispatch. But **`run` grows by more than the hops shrink**:
536–770 µs on the lanes → 1,211–1,473 µs on the connection thread
(+0.6–0.9 ms, p99 bucket 16 → 32 ms), so the dispatch **total** is
+7–16 % in bracket 1 and par-to-+16 % in bracket 2. The S8-plane frame's
`dispatch` (the C-2 term this campaign set out to remove) DOES fall
1.09–1.70 → 0.39–0.61 ms — on a plane that carries 2–5 % of the row's
dispatches; the publish plane carries the rest, and there the arithmetic
above governs.

Why `run` grows: on a lane, a wake inside the served work (the conveyor's
durability fan-out at the ack, a 4a guard release) is a task re-queue on a
thread that is already running; on the connection thread it is an OS
unpark of a dedicated parked thread — and the thread that pays the unpark
is the volume's durability lane. The CPU ledger says so:

| leg | lever | authority CPU s | `sqz-meta` | `sqz-jrnl` | `other` (RPC / connection threads) |
|---|---|---|---|---|---|
| L1a / L1b | 1 | 2.42 / 2.42 | 0.28 / 0.27 | **0.55 / 0.54** | 1.56 / 1.57 |
| L0a / L0b | 0 | 2.41 / 2.58 | 0.89 / 0.97 | **0.44 / 0.47** | 1.05 / 1.11 |
| R1a / R1b | 1 | 6.13 / 5.88 | 0.71 / 0.69 | **1.42 / 1.35** | 3.90 / 3.75 |
| R0a / R0b | 0 | 6.27 / 6.22 | 2.36 / 2.36 | **1.19 / 1.17** | 2.65 / 2.61 |

Total authority CPU is par (the work moved, it did not shrink); the
journal lane pays **+17–25 %** for the fan-out into parked threads, and
the conveyor's `tx_queue_wait` reads 275–352 µs inline vs 169–299 µs on
the hop legs. Apply-stage utilisation is 0.18–0.23 on every leg — the
lanes the in-process hog saturated are, on this fleet, four-fifths idle.

## 3. What the co-writers and the row see

| leg | lever | ingest GiB/s | verbs/s (authority) | co-writer `publish_phase_ns.total` ms | `meta_ship` rtt ms | conveyor passes / served frame |
|---|---|---|---|---|---|---|
| L1a | 1 | 7.28 | 14,351 | **66.1** | 3.07 | 0.390 |
| L0a | 0 | 7.84 | 14,762 | **57.7** | 3.24 | 0.402 |
| L0b | 0 | 7.44 | 14,456 | **57.9** | 3.42 | 0.393 |
| L1b | 1 | 7.33 | 14,600 | **68.6** | 3.34 | 0.379 |
| R0a | 0 | 7.11 | 14,931 | **43.0** | 3.38 | 0.383 |
| R1a | 1 | 6.98 | 15,079 | **64.2** | 3.52 | 0.401 |
| R1b | 1 | 6.94 | 14,727 | **57.0** | 3.24 | 0.375 |
| R0b | 0 | 7.00 | 14,562 | **48.7** | 3.82 | 0.382 |

Medians, inline vs hop: ingest **−1.5 % / −7.0 %** (bracket 2 / bracket
1), verbs/s par (−0.3 % / −0.6 %), co-writer publish latency **+32 % /
+16 %** (bracket 2: 43.0–48.7 → 57.0–64.2 ms; bracket 1: 57.7–57.9 →
66.1–68.6 ms), frame RTT par. Both orders of both brackets rank the same
way on every column that moves; nothing on the inline arm is better than
par.

## 4. Verdict

`SQUEEZEFS_META_SHIP_INLINE_SERVE` **ships OFF** (landed with this note):
the `spawn_meta_join` hop is the default, `=1` is the same-binary A/B lever
for a venue whose `sqz-meta` lanes actually ARE saturated. The in-process
4 × 200 µs lane hog was the wrong model of the fleet — it made the lanes
the queue, and on the fleet they are not (ρ 0.18–0.23); the field's cost of
a served dispatch is the served work's own wakes, and a dedicated parked
thread is a worse place to receive them than a running lane. This is the
`SQUEEZEFS_PUBLISH_SHIP_MULTIPLEX` outcome again inside the same campaign:
both D-5 venue levers now ship as measurement levers.

What D-5 keeps, unchanged and load-bearing: the split instrument
(`meta_ship_owner_dispatch_ns` — this row is its first field reading),
the accept tick (`poll(2)` on the socket: dial 100 → 0.2 ms), the owner
session as a socket-parked lane, `TCP_NODELAY` on both sockets. C-2's
"2.0–2.3 ms per verb" was read on the pre-D-1c/pre-C-2 fleet; on this
fleet the hop is 0.5–0.7 ms of a 1.1–1.5 ms dispatch, and the rest is
`run` — the served commit's conveyor wait and apply, which is the
verb-plane grouping rung's term (the S8 `run_batch` as one conveyor group
per frame, the publish plane's D-1c shape) and then D-6.

Not claimed: a sustained (≥ 60 s) row — the two brackets are 3.3 s and
6.9 s per leg, sized by the lane share; a longer row on this substrate
needs a larger OSS, and nothing in the two brackets suggests the sign
would change with wall time (every column is flat across the doubled row).
