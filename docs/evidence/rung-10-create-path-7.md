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

## The train's red (2026-10-22; decision 0003 §BD.7–BD.9)

Gate 73 was green; the train's five `-Dfresh` runs of the landed wave went red twice
(`.gate-train/local-6a1f2fd402dc-20261021T235455Z.log` 751, 1067; `…T000244Z.log` 1184), so the five
commits were reverted and re-landed here with the cause fixed.

**The mechanism.** §AW.4: two in-flight windows apply in either order. The checkpointer's freeze between
the two applies moves the LATER window's record into the base; the EARLIER window's record then lands in
the overlay BELOW a base record of its key. `lookup`'s cold fold, `gather` and the Δ apply's
`foldBelowLocked` read overlay-then-base as one newest-first sequence and `foldNewestFirst` asserted it
(`record.zig:1310`): the applier storm's unit test died there under the gate's load, and THE STRIPED
DIRECTORY's daemon died the same way (its `mkdir hot/sub` answered ENOTCONN). A silent second shape: a
straggler's head (the fold up to its own seq) served over a newer base record. Latent since §AW
(10-20); BD.5 is `self`-only and not on the venue path; BD.4 takes no part in the apply order — the
gate's saturation reached the interleaving more often, that is all.

**The red tests.** Node level: base `[Δ200, Put10]`, overlay `[Δ300]`, the straggler Δ100 lands and clears
Δ300's head; the next read gathers `[300, 100, 200, 10]` — the panic, deterministic (shown on 5b4e210); a
second key where the straggler's head shortcut skips Δ210 and serves ctime 110 for 211. Backend level,
seam `conveyor.test_seams.hold_next_apply`: W1 held before its apply, W2 (Δtime 2002) applied and
acked, the parent's leaf frozen by hand (the checkpointer's lock window), W1 (Δtime 1001) lands below —
5b4e210 answers 1001 for the parent's mtime. The node_cache property test gained stragglers at odd seqs
below the mint, judged against the by-seq fold after every step.

**The fix** (`fix(meta): a key's records fold by SEQ…`): every walk over a key merges the two seq-sorted
groups; a head serves only while the base holds nothing newer (`baseAbove`); the previous record's head
is the Δ's fold start only when no base record lies between (`baseBetweenLocked`); the memo only when the
whole base group is below. Three counters name the inversion's rate. No record class folds by source.

**What the honest fold found next** (`fix(meta,sym): the manager's record-seq space is monotone across
its INCARNATIONS`): with the fold by seq, THE `-o ro` READER and TWO MOUNTS OF ONE SET went red at A's
remount — `alloc_lease: (busy)`, the term-3 lease Put at seq 2907408 under the term-1 release's Delete at
2944057. The manager's open recovered its seq offset from the replay window alone (empty after a clean
unmount) and never from appender 0's page, so an offset raised during the mount (a departing member's
slot floor) was lost and the new incarnation stamped below its old frontier — charter §5.5's "monotone
across rings" broken across incarnations. The open now takes the page's offset before its first
reservation. Red: the raised offset survives the reopen (the frontier ≥ the old one), a new Put outranks
the old before and after a freeze and across another reopen; both rows green.

**Counts.** On `7.2.8-9.33-omarchy-sqz`, the full smoke running beside (load ≈ 24), the fold fix alone:
the three red tests 100/100; the applier storm test 50/50 on the branch and 50/50 on dev + the fix (the
counters 0 in every run); THE STRIPED DIRECTORY 10/10; THE DEAD MANAGER 10/10. On
`7.3.0-1.7-mainline-sqz`, the final tree, the full smoke looped beside: the fold tests 100/100, the
seq-offset test 100/100 (red on the unfixed open), the applier storm 50/50, THE DEAD MANAGER 30/30, THE
STRIPED DIRECTORY 168/170 (dev 40/40 beside it) — the two deaths are the hash-map race below, not the
fold; the whole `zig build test -Dfresh=true` 2871/2982 with 108 skipped and the 3 `fuse.transport.
session` e2e tests red on dev alike on this kernel (the mainline transport, not the engine); rails 32/32.

**Seen, not fixed.** (0) THE STRIPED DIRECTORY's daemon dies about once in 85 runs under load with
`std.hash_map` `pointer_stability` tripped (`reached unreachable code` in `getOrPutContextAdapted`) under
`kv_fs.getattr → foldIfStriped → verdictOf`'s cache insert — two mutators of the stripe verdict cache's
map at once, though every mutation sits under `kv_fs_stripes.lock` (an `sync.RwLock`); the captured log is
`~/tmp/cp7b/striped-hashmap-race-daemon.log`. The train's striped-row death had this symptom too (its
daemon log was not printed); whether it was this or the fold is not established. (1) THE DEAD MANAGER's
hang on the train was not the inversion: B parked twice —
the first reclaim admitted (epoch 1), then `T_self reached` again and the second reclaim answered
`UnknownLease` → POISONED → one cross-owner create parked forever at `[handler_entry]`, the storm child
unkillable (A's log: `tick — 1 evicted at T_owner`); the renewal missed T_owner (3 s) under the gate's
load, and a poisoned joined mount holds a parked op instead of answering it EIO. (2) The replay gate
(`tree.zig`, `.replay`) skips a record whose seq is ≤ the node's newest for the key; a straggler Δ the
node never saw is dropped at crash replay — an exact-seq gate would keep it. (3) Under the doubled load
(the full smoke beside four loops) `A MEMBER'S CACHED COPY…` and `THE ORDERED LAW'S KILL ROW` each
failed once (`~/tmp/cp7b/runs/load-4.log`, `load-6.log`).
