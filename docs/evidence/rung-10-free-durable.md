# Rung 10 — a freed block is held until the record that freed it is durable

Decision 0003 §BT; charter §6.2 "Block reclaim" (amended 2026-10-24). Branch
`fix/free-held-until-durable`: the red row first, then the hold, then this
note.

## The defect

Under the ack point (§5.2, §BB) a truncate, a punch or an unlink is answered
at its RAM apply; its journal window reaches stable media up to one flush
interval later. The terminal free ran right after the answer
(`kv_fs.afterCommit` → `DataPlane.freeBindings` → `Reclaimer.freeBlock` →
`beginFree` → elide + `finishFree`), so the block was reallocatable while the
durable map still named it. A new owner's DMA into it followed by a power cut
replays the old map over another file's bytes. The data plane's own commits
(write-path saves, `MapSeam.punch`) answer at the LANDING, not the barrier:
the same hole on a device with a volatile write cache.

How wide it was, by posture:

- `--single-writer` (the flat allocator): a mint takes the free list FIRST,
  so the next allocation on the volume takes the block at once. Shown below.
- The symmetric holder: a block re-enters allocation only through a carve,
  whose SET delta is a structural entry plus an `fdatasync` of the metadata
  device. That fdatasync covers the freeing window only if its write had
  COMPLETED when the flush was issued. Nothing orders the two, so this was an
  accident of timing, not a law.
- A joiner's free ships to the holder at its apply. The holder's flush covers
  nothing the joiner's lane has in flight on another host.
- The debt drainer could DISCARD the freed block before the free was durable.
  After the cut the un-truncated or un-unlinked file then read zeros.

## The red

Smoke row `A FREED BLOCK IS NOT REUSED BEFORE THE RECORD THAT FREED IT IS
DURABLE`. The set is `--single-writer` over dm slices, with no write cache and
`SQUEEZEFS_META_WINDOW_HOLD=none`. The metadata slice gets dm-delay (12 s on
writes and flushes). Then: `fpunch` of the second file, then `truncate 0` and
`rm` of the others, then eight O_DIRECT one-block writers of 0xbb. The cut is
dm-flakey drop_writes plus kill -9 of a foreground daemon (no supervisor).

| | after the cut: freed blocks hold | file reads | fsck |
|---|---|---|---|
| `2d44c33` (red) | `bb bb 00` | truncated file: 4 MiB of **0xbb** (another file's bytes); unlinked file: **zeros** | clean (silent) |
| `6204e51` | `a1 a2 a3` | each file its own bytes; the landed punch a hole | clean |

The red run's counters: `alloc_from_freelist = 2`, `block_free_reclaim_elided = 3`.
The green run's: `alloc_from_freelist = 0`, `block_free_reclaim_elided = 0`.
The writers took fresh blocks.

## The fix

The durability hold (`reclaim.zig`). A terminal free still retires its
incarnation and purges the read tiers at once, so a stale reader fails as
before. Then it is pushed on an OPEN list under the hold's lock, and the
block stays in `limbo` with an in-flight registration. A release takes the
lock. When the SEALED list is empty, the release moves the open list there
and has the gate snapshot every metadata member's reservation high-water mark
(`MetaSet.durabilityMarks`). Every free on that list was pushed after its
commit's answer, and the commit reserved its window before its apply, so the
snapshot covers it. The sealed list goes to the ladder's device side (elide
or queue) once every member's `JournalLane.durableFloor()` passes its mark.
That floor is the landed floor of the last completed barrier, published in
`barrierDone`.

Releases ride three paths:

- the producer's own free;
- the worker's 50 ms tick while frees are held;
- `drainSync` (the ENOSPC valve and the dismount), which seals everything and
  pulls a barrier first (`MetaSet.fsync`).

A full store never answers ENOSPC while frees are held. A non-holder's ship
is held the same way. `df` and the write admission count held frees as free
(`BlockAllocator.durable_held` in `reachableFreeBlocks`; ext4 also shows its
running transaction's frees as free). `pending()` sees them. New `.stats`:
`block_free_durable_{holds,releases,forced}` and the gauge
`block_free_durable_held`. The ledger identity reads
`queued + elided + held ≡ terminal`.

## The model

`free_hold_model.zig` runs two freers, the lander, two barriers, the releaser
(three rounds) and a new owner. The invariant: no DMA into a block whose
freeing window is not yet durable. The model holds; it is small enough to
re-run on every gate. Each weakening fails (`InvariantViolated`):

- `no_hold`: the shipped ladder.
- `seal_outside_lock`: the mark is read before the lock that swaps the lists.
- `landed_floor`: the release compares the landed floor, not the barrier's.

## Numbers and what is owed

- The red row passed 50/50 with the fix (`-Dfresh=true`, run in sequence;
  each run: the freed blocks hold `a1 a2 a3` after the cut).
- Smoke rows passed 57/57 (with warnings), the selection plus the always-run
  rows: the full-store ENOSPC row, FALLOCATE (every mode), the punch/zero
  power cut, the streaming overwrite, the unlink storm, the KVDA page sink,
  the kill matrix, the block-map tree, the jobs matrix, the promotion kill,
  the transformed append, O_TRUNC under no-open, the cache-less data plane,
  THE REPLAY PROOF, and the fallocate setid strip.
- Unit filters, all green:

  | filter | tests passed | skipped |
  |---|---|---|
  | `block_alloc` | 68/68 | 0 |
  | `reclaim` | 105/114 | 9 |
  | `enospc` | 49/50 | 1 |
  | `fallocate` | 49/51 | 2 |
  | `free_hold_model` | 41/41 | 0 |
  | `sym_planes` | 119/119 | 0 |
  | `meta_set` | 47/47 | 0 |
  | `journal_lane` | 64/64 | 0 |
  | `data.plane` | 49/49 | 0 |
  | `write_e2e` | 44/44 | 0 |
  | `kv_fs` | 117/117 | 0 |
  | `fsck` | 117/167 | 50 |
  | `write_admit` | 83/83 | 0 |
  | `pack` | 99/107 | 8 |

- `zig build rails` passed 32/32, and `zig fmt --check` is clean.
- The cost on the free path is one spinlock push and one release check per
  terminal free. A block now returns ≤ one flush interval plus one manners
  tick later. The mint path is unchanged.
- **Owed:** the mdtest creates/removes row and `sw1m` before and after, on a
  quiet host (load < 4, one benchmark at a time, cooled).
