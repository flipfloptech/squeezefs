# Rung 10 — the create path, wave 7: the work under the leaf lock (2026-10-21)

Continues `rung-10-create-path-6.md`. Venue: this workstation (32 threads, `7.2.8-9.33-omarchy-sqz`),
the RAM NVMe set (512-B grain); A = dev `cc21b45`, B = `perf/create-path-7`. Scratch
`~/tmp/createpath2/` (`w7-*`), `~/tmp/w4cp/`.

## The hold, split (decision 0003 §BD.4–BD.6)

`meta_kv_freeze_locked_ns` measures the checkpointer's freeze under the leaf lock: **28 380 freezes ×
24.8 µs = 0.70 s** per 320 k creates. The admission's locked law on leaves at capacity (45 % of
admissions after wave 6's fit check): **261 018 takes × 57.8 µs = 15.1 s** of waiting, and its exact
walk under the lock was the hold the committers and appliers queued behind. The apply's own hold: 81 k
windows × 119 µs = 9.7 s, of which 6.5 s waiting for leaves. The freeze was never the serializer; the
admission's walk was.

## Lever 1 — the admission on a volume with room takes no lock (ships)

The fit check was the wrong guard: a leaf that does not fit in place on a volume with room splits at
writeback from extents the room guarantees (`fastPathRoom`: free ≥ 2× the reserve + four extents per
transaction the conveyor can hold), so no promise is owed and no lock is taken; the locked law only near
the reserve or in the heap-full posture. **After:** admission takes 0 (100 % lock-free), the apply's
hold 109 µs per 4.5-tx window — 62 µs waiting for leaves other appliers hold (7.5 takes at 8.3 µs),
47 µs of work (≈ 10 µs per transaction) — the freeze 0.6 s per phase.

| `mdtest -np 32 -n 10000 -u` | dev | venue (ships) | `self` |
|---|---|---|---|
| file creation | 100.7 / 101.7 k | 95.8 k | **139.9 k** / 110.4 k |
| file removal | 106.6 / 107.8 k | 83.0 k | 81.1 / 83.7 k |
| dir creation / removal | 57.9 / 59.3 k | 66.1 / 39.8 k | — |
| -np 64 creation / removal | 108.4 / 102.5 k | — | 96.7 / 19.9 k |

(Single legs under other tenants' load, L2–9; the venue arm's removal legs met load 4–9.) The venue's
rate did not move: its four appliers now wait on each other over the rotor's leaves (62 of 109 µs).
`self` removes them: **+40 % file creation at 32 ranks**, but removals 0.78× and a collapse at 64 ranks
on removals (the reclaim's grouped destroy windows take the venue and hold leaves across the
self-appliers). `venue` still ships; `self` stays the lever, one law away.

## Lever 2 — the turns fan out as a tree (ships, §BC)

The pass wakes member 0; each member wakes 2i+1 and 2i+2 before its own apply. At 64 ranks the serial
fan-out took 254 µs of every member's queue wait and parked every spinner past its bound; the tree:
116 µs, `self` file creation 57 → 97 k.

## Not done this wave

- **O(1) freeze** (item 1): measured unnecessary at this rate (0.7 s of 320 k creates' lock time); the
  design (a third read source for the frozen run; the encode and the base's extension off the lock)
  stays the next step once the appliers' mutual waits are gone.
- **The apply's remerge** (item 2): the apply is `RunEdit` (chunk copies) + an O(1) snapshot publish;
  ≈ 10 µs per transaction of work under the lock, no O(overlay) term left to remove.
- **The rewrite's trigger** (item 3): untouched — with the admission lock-free the leaves at capacity
  cost no lock; 198 MB rewritten per 320 k creates stays the checkpointer's CPU (2.2 s per phase).
- **Kernel patch 0034**: not drafted — the wave's budget went to the hold's measurement and the two
  levers above; its saving stands as named (one of the two round trips under the parent's `i_rwsem`).
- The shared-directory and stat shapes, the data rows (no data path touched), the NVMe substrate.

## Tests

Unit `fmt test rails`: **2901/2991 passed, 90 skipped**, rails 29/29. Smoke (coherence, shared hot dir,
dead manager, kill rows (a)(b), kill matrix, kill -9 mid-tar ×2, replay proof, durability points):
**50/50**; INVALIDATION COHERENCE ×5. The §BD leaf-set test (≤ 2 takes at the apply, none at the
admission), the §BC storm, the §BB rows green. fsck clean after every session.

## Risks

- `fastPathRoom` is extent-based; a small volume (under 2× reserve + 4 × caps) takes the locked law for
  every admission — safe, slower, and the first wave's measurements apply to it.
- `self` on removals: the venue-pathed reclaim groups hold leaves across the self-appliers; the lever is
  not shippable until the reclaim's destroy batches apply themselves too.
