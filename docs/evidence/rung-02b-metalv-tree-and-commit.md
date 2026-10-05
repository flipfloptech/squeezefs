# Rung 2b — MetaLV: sync primitives, object locks, journal lane, node cache, btree + SMOs, checkpoint, the commit conveyor, the backend

Charter §13 rung 2, second half. Landed 2026-10-05 on `dev` in four waves
(`7f11f47`, `982f9fc`, `4672316`, and the wave-6 commit). Together with rung 2a this is the
complete metadata plane for the `--single-writer` class on one volume: format → open → replay →
commit through the conveyor → checkpoint → close, torn-write immune, with the reference binary's
`fsck` clean after every kill.

## What landed

| Module | Lines | Tests | Contract |
|---|---|---|---|
| `sync/*` (mutex, rwlock, tick, notify, semaphore, flight, once, channel, taskset, arc_swap) | 4,500 | 65 | Charter §9 primitives over `std.Io`'s futex ops (decision 0001): release flips free and wakes the queue front with tokens only; woken waiters re-contend; bounded barging; write-preferring; every unbounded park a 2 s ticked wait counting `lock_ticked_reregisters`; Notify permit + epoch with enable-then-recheck; single-flight with leader re-election; once-cell (a check-then-elect race found red-first); channels registering waiters under the senders' lock; `ArcSwap` with a split-count claim protocol |
| `meta/dlm.zig` | 727 | 9 | The 4a `I{ino}` / `D{parent,name}` striped tables, width `next_pow2(max(4096, 16 × cpus × q_depth))`, `lockMany` in the ONE canonical order, `tryLockMany`, census closure `dlm_guard_wait.count ≡ Σ census` |
| `meta/journal_lane.zig` (Linux) | 2,100 | 15 | One OS thread per writable volume (`sqz-jrnl<N>`) owning its io_uring and parking in it; lane-native aligned-run writes under the O_DIRECT refusal/bounce law; stage B: complete all → one prefix wait → hole checkpoints → ONE barrier (strict / coalesced `SQUEEZEFS_META_FLUSH_INTERVAL_MS` 50 ms) → acks in journal order; FAILED latch at 3 |
| `meta/tx.zig`, `heap_law.zig`, `inos.zig` | 2,650 | 39 | `KvTx` staging + RYOW (`buildQueuedTx` byte-identical to `journal.buildEntryWrite`); the heap-promise law (`heap_full` is a posture, never terminal); the monotonic ino watermark, the DUR-8c recovery fold, destroy batching ≤ 1.05 entries/op |
| `meta/node_snapshot.zig`, `node_cache.zig` | 4,800 | 40 | `RecordIndex` k-way merge, the 8-cell `FoldMemo` (memo ≡ fold property), `CachedNode` = `ArcSwap(NodeSnapshot)` + the 4b rwlock + the lifecycle word, `applyLocked` as THE choke point, freeze-under-lock/append-outside, sharded COW map with latch-free reads, single-flight loads with the pre-read epoch snapshot, clock eviction with pinned classes, budget `max(budget/16, 512 MiB)`; warm lookup = 0 allocations |
| `meta/economy.zig` | 1,980 | 21 | Rename = ONE entry; unlink/rmdir with the batched-destroy rider; the SETATTR-echo absorber (absorb iff the echo equals committed/in-flight times); `SharedTimes` Δ records under the SHARED parent guard |
| `meta/tree.zig`, `smo.zig`, `leaf_merge.zig` | 5,160 | 39 | Latch-free `descend`; `applyAtSeqClass` = the one lock-window primitive; two-phase replay; `smoReplace` P0–P5 with publish→flip→retire; root grow with the dying floor; the keep law; the merge law `f(L)+f(R) ≤ ¾C ∧ min ≤ ¼C`, the bounded cursor-resumed sweep, root collapse |
| `meta/checkpoint.zig`, `checkpoint_cycle.zig` | 2,200 | 21 | Derivations with ties; `decideCheckpoint` pure; `CycleTape` exact-sum; the 14-step cycle over typed sinks with DUR-3 `reusable_upto` epochs and coverage advance; park-mark kick; flush-ceiling tripwire; the wedge rung at 8 barriered cycles |
| `meta/conveyor.zig`, `device_loader.zig` (Linux), `backend.zig` (Linux) | 4,800 | 29 | Stage A (A0–A13 verbatim: Σ admission before any node lock → union lock → heap admission → pre-images → ONE registered reservation → in-order apply with roll-out → submit → handoff); stage B through the lane; guards drop BEFORE the ack; seq-conditional rollback + compensation; the device `NodeLoader` over `uring_fs`; `KvBackend` open/replay/commit/fsync/checkpoint/close with the D0 layer-A `flock` |

## Evidence

Instrument: `zig build gate`, Debug test profile. Venues: `sqz-zig` (Ubuntu 24.04, 7.2.6-sqz,
aarch64, ext4 regular files under O_DIRECT, grain 512 B) and the macOS dev box (portable modules).

| Venue | Result |
|---|---|
| Linux | 10/10 steps; **724/725** (722 unit + 2 rails; 1 skip = the crash child's own routine when not re-exec'd); exit 0 |
| macOS | 10/10 steps; 638 pass + 6 venue-only skips + 2 rails; exit 0 |

### The kill matrix (charter §1.10) — reopen + reference `fsck --offline` on every post-cut image

| Cut point | Outcome | fsck |
|---|---|---|
| after ack, before the barrier (coalesced) | 20/20 acked txs whole; 12 entries replayed; `next_ino` > max minted | clean |
| mid-checkpoint, before the ledger write | previous ledger record selected; 20/20 whole | clean |
| after the ledger write, before `reusable_upto` | new record selected; 0 replayed | clean |
| mid-SMO entry (swap done, entry unlanded) | 32/32 whole through the OLD structure; 24 replayed | clean |
| `kill -9` of a child at 20 / 75 / 160 acked creates | every acked create + dentry present; no partial tx; torn dropped 0 | clean ×3 |
| portable power cut (`RamLane` over `VolatileImage`) | a coalesced acked-not-barriered tx is lost WHOLE; a strict ack survives; never a partial tx | — |

Contract stated by these rows: in coalesced mode an ack precedes durability until the trailing
barrier (strict mode acks after the barrier); whole-transaction atomicity holds on every cut.
`replay_dropped_torn` is 0 after every `close → open`, and the clean-unmount tripwire never fired.

### Closure identities asserted at quiesce

`lane_writes ≡ lane_handoffs`; `leader_passes ≡ Σ group-size buckets`; `acks ≡ drained − refusals`;
`windows_inflight = queued = commit_parked = heap_promised = 0`; registry `openCount = 0` and
`completedUpto == head`; acks in journal order; a group of 8 = one pass, 8 acks, one strict barrier;
`dlm_guard_wait.count ≡ Σ census`; the memo-serve ≡ fold property over 48 random histories; the
SMO oracle (6,000 ops ≡ `AutoHashMap`; crash-replay reproduces the final routing).

### SMO completeness

An SMO entry torn ⇒ the OLD routing on replay; the ring cut right after it ⇒ the NEW routing;
never a mix. The stranded-build-window commit survives a crash; a merge and a root collapse
converge across every crash window.

### Measured in tests

Wake coalescing on the lane: 8 publishes between parks → 1 eventfd write, 7 elided. Parks per
window: 2 coalesced (wake + CQE), 3 strict (+ the barrier CQE); a 4×32 strict storm ran 1.6–2.0
parks/window with 0 tick rescues. (Scoping rows on a virtual aarch64 box; not acceptance numbers.)

## Model checks added this half (with the weakening that fails each)

rwlock: a parking waiter racing release is never stranded (try-then-register split; park's
compare split from its sleep); a dead waiter never wedges (ownership handed to the dead front
waiter — the tokio protocol); mutual exclusion (grant's check/set split); writer preference (fresh
shared ignoring a queued writer). notify: enable-then-recheck (epoch snapshotted at first wait;
recheck before enable); permit (`no_permit_store`). arc_swap: 2,146 states (naive load; split
swap; flush by store). tree: the maintenance queue (stamp before payload).

## Bugs found red-first in this half

- `OnceCell.getOrInit` could elect a second leader after the first published (check-then-elect).
- `node_cache.freezeLocked`: an overlay emptied by a rollback left the word DIRTY with nothing to
  write; the flush pass re-collected the node forever and a hole checkpoint never converged.
- `smo`: the write-failure seam fired on USER commits (shared `commitEntry`); SMO frames are now
  pre-built before the lock window so nothing after the swap can fail; `descend` leaked a reference
  on a mid-walk device error.
- Test expectations corrected: `257,984/39 = 6,614`; the greedy ¾ partition shape; the dying-floor
  fold; a 64-bit oracle seed packed into 56 bits.

## Deviations (deliberate) and owed

- The conveyor pass runs inline on the election winner's thread; both stages on ONE OS thread per
  volume arrives with the executor (`src/exec/`, decision 0001, rung 3). Stage B's ack IS already
  delivered on the lane thread.
- `journal_lane` owns its own ring (`uring_fs.OwnedRing` lacks an FSYNC SQE and fixed files); a
  ~15-line `OwnedRing.submitFdatasync` lets it move back — owed.
- Only the `--single-writer` class mounts; the symmetric forest refuses `UnsupportedFormat` until
  rung 6. D0 layers B1 (PR) and B2 (the claim record) are later rungs; layer A (`flock`) is in.
- Zig has no unwinding: the reference's panic-guarded sentinels are explicit failure arms
  (admission released / reservation completed-as-abandoned).
- `wait_completed_upto` is a poll token + yield-spin until the executor lands.
- Observation against the reference (identical in both codebases): the DUR-8c fold raises the
  local watermark with the dentry's GLOBAL child ino.
