# rung 10 — uringcell: a request cell given back met a taker mid-pop and was LOST

**Symptom.** A kamrui full gate on `97c81b3` failed `io.uring_fs` "fd cache churn under load …":
it logged `INVARIANT TRIPWIRE 'uring_fs_cell_return_refused'` (`free_cells.tryPush` refused in
`Pool.freeCell`).

**Cause (confirmed by model, then by a deterministic red).** The free list is a Vyukov
`BoundedMpmc` holding all `queue_cap` (4096) cells, and every caller takes from it. A push at
position p needs cell `p & mask` to read `seq == p`. The pop at `p − cap` stores that sequence only
after its `dequeue_pos` CAS. Here is the interleaving the explorer prints (`PoolModel(2, 2, 1,
.push_once)`, schedule `0 0 0 1 1 1 1 1 1`). Taker T0 loads `dequeue_pos`, loads cell 0's `seq`,
and CASes position 0, then is preempted before its release store. Taker T1 pops position 1 whole.
Its give-back pushes at `enqueue_pos` = 2, finds cell 0 still filled (`seq` 1 < 2), and is
answered FULL while one cell is still out. An idle free list is full, so the first give-back after
such a preemption lands on that wrap cell. **The cell was lost, not just logged:** `freeCell`
fired the tripwire and dropped the index, so the pool shrank by one cell for the rest of the
process. This is the class decision 0003 §AI.4 fixed in the sideband, reclaim, NVMe and zcrx
pools. `uring_fs` and `journal_lane` had been missed.

**Oversizing is no fix.** The model shows this with `PoolModel(4, 2, 3, .push_once)`: a ring
twice the population still refuses once T1 laps T0's stalled cell (T0 is preempted mid-pop in its
third round, then T1 runs 22 steps). Preemption is unbounded, so any finite ring can be lapped.

**Fix (`227108d`; red `f53111d`).** Add `mpmc.pushSpinUnlessFull`. It waits out a refusal the way `pushSpin` does, because
the stalled pop is one store from done. It answers false only when the ring really is FULL, which
means the owner's population bound is broken by a double give-back. So the must-stay-0 tripwires
stay live instead of hanging the thread in a spin. The check reads `enqueue_pos` with acquire and
then `dequeue_pos`; `d` can only have grown, so `e − d` under-reads the occupancy. It is used by:
- `uring_fs` `freeCell`, which also counts `uring_fs_cell_return_respins` (in the stats JSON);
- `uring_fs` `ParkedQueue.trySend`: the channel holds at most the pool's cells, but several
  workers pop, so a lapped worker mid-pop caused the same spurious refusal. That surfaced as a
  false `QueueFull` plus the `uring_fs_queue_push_refused_with_cell` tripwire;
- `journal_lane` `freeCell`: the same lost cell, behind the `journal_lane_queue_push_with_cell`
  tripwire.

**Model** (`src/sync/mpmc_pool_model.zig`, now parameterised by ring size, population, rounds and
give-back):

| variant | result |
|---|---|
| `pushSpin`, ring 2 / pop 2 / 1 round | holds, 175 states |
| `pushSpinUnlessFull`, 2/2/2 and 4/2/3 | holds, 4,967 and 11,197 states |
| weakening `.push_once` (the old `freeCell`) 2/2/1 | **fails**, 70 states |
| weakening `.push_once` on an oversized ring 4/2/3 | **fails**, 316 states |
| weakening `.unless_full_d_first` (reads `dequeue_pos` before `enqueue_pos`) 2/2/2 | **fails**: a stale `d` reads as FULL |

**Other queues audited.** Every other `BoundedMpmc` sized to its population either has a single
consumer or already retries or falls back:
- Single consumer: the journal-lane inbound, the NVMe lane queue, the commit queue, the worker
  inboxes, and zcrx `cmds`. With one consumer, lapping its stalled pop would take population + 1
  cells.
- Retries or falls back: reclaim (overflow deque), BlockPool and the severed pool (they free
  instead), and the `pushSpin` users.

`session.zig` and `src/exec/` belong to other agents and were not touched. `session.ready` and the
exec inbox each have a single consumer.

| run (kamrui) | result |
|---|---|
| red `f53111d`, "free-list dequeue CAS" | red: the tripwire logged, `expected 4096, found 4095` (the cell lost) |
| fixed: `uring_fs` + `mpmc` + `journal_lane` filters (incl. the model) | 95 / 95 passed |
| fixed: one binary (churn + red row), looped under 12 × `yes` (load 47) | 150 of 150 green, 127 s |
| full gate (`-Dfresh`) on the fix (rebased over `3b7ab9f`) | 53/53 steps, 2985/3068 tests passed (83 skipped) |
