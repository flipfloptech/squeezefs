# Rung 10 — the maintenance queue coalesces: one entry per leaf (2026-10-27)

Decision 0003 §BY. Branch `perf/maint-queue` off `dev` `a4b0345`. **No benchmark was run**: correctness runs only
(the rig `~/tmp/create-work/mdrig.sh`: 2 MDT + 2 OST on 512-B loop files under `/build/tmp`, supervised,
`mpirun mdtest -u`, `.stats` snapshots around each phase, `perf record -g` mid-phase on the first run; both arms
back to back under one host posture — the laptop on battery for every pair below —, fsck `--offline` clean after
every run, 0 tripwires). The owner's A-B-B-A judges the rates.

## What the counters said on dev

The edge-split verdict named the queue running full as the shape behind the late freezes. At -np 32 × 40k the
threshold queue refused **1.3–2.5 M** crossings per phase against 250–900 k pops, and 97 % of the pops found a
node with nothing to freeze (fc: 552 k pops, 11.1 k appends + 1.6 k SMOs). Every apply that left a leaf's open
delta at or past the 4 KiB writeback threshold pushed the leaf's address again, so each tree's 32 k-entry queue
(`clamp(next_pow2(budget_nodes), 64, 2^20)`; the capacity was never the cause) held one or two hot leaves
thousands of times over: a leaf's first crossing was refused, or waited behind the duplicates to find at its pop
an overlay the checkpoint cycle had already frozen. The drain is the `sqz-ckpt` thread (one per volume, the
`.stats` class `other`), which also runs the cycles: their flush pass over EVERY dirty leaf took 150–350 ms,
one cycle every ~400 ms (the 32 MiB ring at 35–40 MB/s of journal: `ring/2`), 53–73 % of the thread. The
duplicates' pops froze whatever few records had arrived since: appends averaged 5.8–9.6 KiB.

## The change

`CachedNode.maintenance_queued`, a coalescing word: `enqueueMaintenance(n)` claims it by CAS and pushes only on a
win (the losses are `meta_kv_maintenance_coalesced`; a refused push gives it back); the drain clears it right after
`tryGet`, BEFORE the node's lock — a committer applies under that lock, unlocks, then claims, so one that claims
after the clear pushes again and one that found the word set applied before the drain's lock section, which
freezes its records. A popped node back below the threshold with nothing frozen is left alone
(`FlushDisposition.idle`; `flushDirty` keeps its forced posture). Model `MaintenanceCoalesceModel`: holds (one
entry per leaf, no crossing lost); clearing after the freeze fails (`LostCrossing`). The ring's own model and the
capacity derivation are unchanged.

**Red** `fccc923`: a two-leaf tree over a 64-entry queue; 200 inserts on one leaf fill it (depth 64 for 1, 53
refusals), the cold leaf's one crossing is refused (70), and after the drain the cold leaf still holds 4,900 B
over the 4,096 B threshold (appends 1 for 2). Green `5a9450a`.

## Before → after (per phase fc / fr / dc / dr)

| -np 32 × 40k | dev `a4b0345` | the branch |
|---|---|---|
| queue refusals | 1,317 k / 897 k / 2,139 k / 2,046 k | **0 / 0 / 0 / 0** |
| drain pops (crossings 2.0 M / 1.5 M / 2.9 M / 2.6 M) | 251 k / 797 k / 460 k / 660 k | 2.8 k / 4.0 k / 2.6 k / 3.8 k (depth max 1,158–2,140) |
| appends, KiB each | 12.7 k / 22.5 k / 14.4 k / 19.2 k at 9.2 / 5.8 / 9.6 / 6.6 | 7.8 k / 13.8 k / 11.2 k / 14.9 k at 15.0 / 8.1 / 12.1 / 7.8 |
| compactions | 663 / 1,452 / 791 / 1,698 | 399 / 1,338 / 552 / 1,520 |
| compactions + splits per 10k ops | 12.0 / 11.4 / 14.8 / 13.4 | 11.2 / 10.5 / 13.9 / 11.9 |
| rewrite bytes per op | 352 / 154 / 449 / 201 | 336 / 137 / 426 / 177 |
| leaf write-lock time taken by freezes, s | 1.71 / 2.60 / 2.52 / 2.60 | 1.46 / 2.15 / 2.25 / 2.39 |
| daemon CPU per op, µs (`sqz-ckpt` 4.9 / 9.0 / 6.5 / 9.8 → 4.6 / 8.6 / 6.1 / 9.4) | 88.5 / 71.7 / 90.0 / 79.4 | 86.6 / 72.7 / 85.9 / 78.2 |

| -np 64 × 20k | dev | the branch |
|---|---|---|
| queue refusals | 1,216 k / 1,225 k / 2,487 k / 2,212 k | **0 / 0 / 0 / 0** |
| drain pops | 156 k / 902 k / 48 k / 667 k | 1.5 k / 4.2 k / 5.5 k / 4.1 k |
| compactions + splits per 10k ops | 11.3 / 10.1 / 16.4 / 11.0 | 11.0 / 10.2 / 14.6 / 10.8 |
| rewrite bytes per op | 335 / 137 / 478 / 163 | 333 / 129 / 438 / 168 |
| daemon CPU per op, µs | 93.2 / 63.8 / 92.1 / 85.4 | 85.7 / 62.7 / 90.6 / 76.9 |

The new instruments (`meta_kv_threshold_to_freeze`, `meta_kv_freezes_{threshold,checkpoint}` and their bytes,
`meta_kv_maintenance_idle_pops`, `meta_kv_maintenance_item`, the checkpointer's `maintenance_depth{,_max}`) say
what stays: a crossing is frozen 117–227 ms (mean) after it happens, 13–40 % within 8 ms; 75–88 % of the freezes
are the cycle's (22–39 KiB of delta each) and the drain, at 200–730 µs per real item on the same thread (the
freeze's encode and index merge under the leaf lock, one synchronous append), serves the rest in the gaps. With
~1,200 hot leaves each crossing every ~150 ms, no share of one thread serves every crossing at 4 KiB. §BY.5 names
the levers (a bounded flush pass, an O(1) freeze off the lock, pipelined appends, one queue per volume).

## Checks

Unit: smo 216, leaf_merge 49, tree 168, node_cache 77, checkpoint 121, conveyor 116, meta.backend 87, §BX 55, §BW 50,
journal_tail 51, model 209 — green; rails 18/18; `zig fmt --check` clean. Smoke and xfstests: see the report.
Sessions: `~/tmp/create-work/runs/{base,fix}-np32b`, `{base,fix}-np64`, `base-np32` (perf), `fix-np32` (perf).
