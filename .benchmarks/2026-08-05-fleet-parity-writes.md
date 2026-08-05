# 2026-08-05 — Fleet parity at bs=1M ×256 psync: reads WON, the write residual named open

**Venue:** squeeze-test (real nvme-tcp fabric, 10 data namespaces).
**Instrument:** `tests/fio/fleet_parity_row.sh` — 256-PROCESS psync qd1 bs=1M
fleets (one shim session per process — the field shape), il vs kernel in
K-I/I-K/K-I alternating pairs, per-row engagement (`ipc_ops_*`/`ipc_bytes_*`
deltas; INVALID rows printed, never averaged), the arena-prep ledger, a
settle between fleets (prep ledger closed + reclaim drained + R5 Green),
and a fatal mountpoint+.stats gate per row (two earlier fabricated runs —
fio against a bare directory after a mount raced the previous daemon's
drain — are the reason the gate exists).
**Binary:** wave `649af374` (deferred THP prep `f19df263` + prep-job
liveness `68ddfa8b` + the parked-overlay reclaim fix `649af374`).
Artifacts: `/scratch/tmp/logs/fleet_parity{,2,3,5,6}` on the cluster.

## The regression chain this campaign closed (D12 board item 2)

1. **Admission-inline THP prep** (the named launch term): 256 sessions ×
   ~120 MiB populate+collapse serialized on ctl threads before `SessionOk`
   — ~20 GiB of memory work in front of every fleet's first op. Fixed:
   deferred to the single-lane `sqz-ipc-thp` worker (`f19df263`), local
   knee il/kernel 0.914 → 0.975 create, 0.970 → 1.153 sustained.
2. **Prep-queue dead-arena pinning** (found by the first field run's
   ledger): queued jobs' mapping Arcs pinned exited fleets' arenas
   (~30 GiB) → R5 Red → admission clamps. Fixed: Weak upgrade-or-skip +
   Red-only pressure skip (`68ddfa8b`); field `skipped_dead=285` engaging.
3. **The 230 GiB parked-overlay leak** (the campaign's biggest find, its
   own note + fix `649af374`): inode reclaim orphaned RAM-parked overlay
   buffers forever on cache-less mounts; the R5 Red drain wedged on
   dead-ino NotFound flushes. Post-fix the same rows run leak-free and
   BOTH arms roughly doubled on reads (kern 7.9 → 17.7, il 13.3 → 25.3).

## Final valid rows (attempt 5, THP on — medians of 3, engagement exact)

| shape | kernel | il | il/kernel |
|---|---|---|---|
| write bs=1M ×256 | 23.11 GB/s | 19.09 GB/s | **0.83** |
| read bs=1M ×256 | 17.66 GB/s | 25.26 GB/s | **1.43** |

**READ VERDICT: the field regression (−18..−21 %) is closed and INVERTED**
(reproduced at +43 %..+82 % across three valid runs). WRITE: ~0.8×, below
the write_matrix parity law — residual OPEN.

## The controlled THP-0 discriminator (attempt 6, same instrument, same mount class)

| shape | kernel | il | il/kernel |
|---|---|---|---|
| write | 21.91 | 17.39 | 0.79 |
| read | 18.45 | 23.39 | 1.27 |

`prep_{q,done} = 0` on every row (THP genuinely off), all rows engaged.
**Prep-during-row is FALSIFIED as the write residual** — the ratio does
not move with THP off. (An earlier ad-hoc THP-0 bracket was INVALID —
28 % engagement from budget refusals with 15 s gaps; the instrument's
settle exists for exactly this and the invalid rows are not cited.)

## Open: the ~0.8× write residual

Facts to explain: (a) il write VARIANCE is large (14.7–23.1 GB/s across
valid rows) while kernel rows are tight (19.5–26.1, mostly 22–23) — the
loss is not a stable per-byte tax; (b) THP on/off does not move it;
(c) reads at the same width WIN by 27–43 %, so it is write-path-specific,
not ring/transport-generic. Best-instrumented candidate (parity
investigation, 2026-08-05): the per-op W1 attrs-only invalidation —
every growing seq ring write pays `hook_runtime.spawn` onto the
multi-thread runtime's GLOBAL INJECT QUEUE plus a `/dev/fuse` notify
(`ipc_inval_notifies`), the exact venue the 2026-07-26 handoff-economy
fix banned for handoffs. Width-invariant locally at w8, but the field
fleet's 256 writers × per-op spawns compose differently. Second
candidate: fleet-launch session-establishment jitter (the variance
face). Next step: decompose one il write row with `SQUEEZEFS_OP_PROFILE=1`
+ the `ipc_inval_*` deltas, then A/B the inval venue (tpc-lane vs global
inject) if it prices.
