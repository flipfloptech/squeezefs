# Rung 10 — gate blockers, second wave

The release gate (charter §11) needs three consecutive clean runs on one binary. Four rows could
redden one: the joiner rejoin row, the nvmet sigstop row, the online-volume row's tripwire and the
sideband abort (`rung-10-symmetric-defects.md` §6, `rung-10-coherence-defects.md` "the queued
sideband abort"). Decisions: 0003 §AI.1–§AI.4; 0002 row 533.

## 1. The joiner rejoin row: the manager never got a joiner's returns back (§AI.1)

### 1.1 The defect

The row (`smoke.zig` "A JOINER'S CLEAN UNMOUNT AND REJOIN") failed at a fill with B's
`no heap extent for slot N's first tree (heap full)`, or at B's next join with
`JoinAppender refused for capacity` (the joiner maps the manager's `no_space` to capacity). The
symmetric agent measured 4 / 14 on the base before its fixes and 2 / 13 on its landed tree.

`.stats` snapshots of A and B every second over one run (the harness below, every snapshot
kept) on `dev` (`93fedde`), A's volume 0 (990 heap extents at 256 KiB, a 256 MiB volume):

| t | A `heap_free_extents` | A trees | `extent_grant_extents` (carved, cumulative) | `extent_grant_returned` (cumulative) |
|---|---|---|---|---|
| A mounted | 985 | 2 | 0 | 0 |
| B's first fill | 765 | 52 | 218 | 0 |
| B's first leave | 430 | 52 | 553 | 431 |
| … | 233 | 102 | 752 | 652 |
| … | 83 | 152 | 902 | 725 |
| B's fourth join | 9 → 0 | 152 | 983 | 752 |

A's free heap fell by exactly what B RETURNED (985 − 2 − 553 + 431 = 861 expected at the first
leave, 430 read: 431 short). B's fourth join was granted 65 extents of its 72-extent floor
(`JOINED … grant 65 extent(s)`): the carve ran out. On a loaded venue B's SMO rate is higher, its
derived pool target larger (`2 × ewma_smo × failover_bound`, hundreds of extents), it refills and
returns more each cycle, and the heap runs dry one cycle earlier — at B's fill (heap full) or at
the join (capacity).

The cause: `Manager.returnInner` (ReturnExtents, the leave's unclaimed runs, the next join's
residue) and `settlePending` (a GrowRing segment no page names) write the kind-4 `free` deltas in
their control entry and never release the RAM bits. The product's control entry
(`bind.zig` `controlEntryFn`) only JOURNALS allocator deltas — the bits move by `claimInternal` /
`releaseUnpublished` alone — so every extent a joiner handed back stayed allocated in the manager
until its next mount (the replay of the free records; fsck after the umount is clean). The
reference releases each landed chunk (`return_extents_inner`, `backend.rs` :10001–10006;
`settle_pending_ring_segment` :10812–10815); the porting spec named the deltas and not the
release, and the manager's RAM fake (`TestVolume.controlFn`) APPLIED the deltas to its bitmap —
the one place the unit rows could have seen it.

Not a custody leak: the per-incarnation trees are accounted (each incarnation mints ≈ 50 fresh
rotor trees, one extent each; a clean leave moves their images out of B's record —
`extent_grant_images_transferred_out` — and the manager adopts them). Not the row's sizing: with
the returns back, A ends the row with 811 free extents of 990.

### 1.2 The red row and the fix

- Red (`a3edea3`, portable): the RAM fake journals the deltas as the product does (an `alloc`
  delta asserts its claim; a `free` delta is counted, the bit untouched). The `manager` rows
  "grants: … returns … clear bits" (expected 37 allocated, found 49) and "leave: … the unclaimed
  runs return through the law" (expected 1, found 13) go red, beside the new row "a joiner's
  returns reach the manager's FREE heap when their entry lands": four join → top-up → mid-life
  return → leave cycles (one with a GrowRing segment no page names, one leaving a residue the next
  join returns) must end each cycle at the heap's baseline.
- Fix (`df5d081`): `returnInner` and `settlePending` release every returned extent after their
  entry lands; the recovery's return (`volume_bind.returnExtentsFn`) no longer releases a second
  time (a claim between the two releases would have been freed under its owner). The porting spec
  (`sym_appenders_manager.md` §5.3) names the release.

## 2. `nvmet sigstop-preempt`: `healthy` beside a fenced count of 1 (§AI.2)

The row awaited `writer_guard_fenced` = 1 and read `writer_guard_verdict` from the SAME render. The
set has one meta volume (`daemonSet` formats `--single-writer` with one data volume): one lane,
one `FailureLatch`, so "another volume's lane met the conflict first" cannot be it. A render writes
the per-volume verdict (`meta.set.volumes[i].guard`) BEFORE the process-wide counters
(`meta.guard`), and `noteFence` latches the verdict (`fenced`, then `failed`) BEFORE it counts —
so the render that first shows the count can have read the verdict before the latch: `healthy`
beside 1. The product's order is right (a reader that sees the count, then renders again, sees the
verdict); the row judged two fields of one render that one render cannot promise together.

- The row (`7b0f60c`): awaits the count, then judges the count and the verdict on a render begun
  after it — the latch never clears, the count is paid once per latch. The online-volume row
  already reads this way ("a render reads the plane's family before the lifecycle's").
- The pin (`guard/gate.zig` "a stats render is no atomic cut across the fence"): the volume's
  section rendered, the usurper's fence, the process section rendered — `healthy` beside the
  count; a render after it — `fenced`.

## 3. `cluster_wire_service_refusals` in the online-volume row (§AI.3)

### 3.1 Who asked

`this mount does not hold the lease of that volume` is the holder plane's answer to a grant verb
for a `vol_tag` it has not armed. The sender was B, inside A's own `volume add-data`: the online
add commits the record (step 6), then attaches the device, arms the grant plane's row and flips it
Active (step 7, `OnlineAdd.publish`). The first wave set the generation the membership grants carry
(`volume_config_generation`) in `PlaneWorld.commit` — at step 6. B's next renewal (265 ms at the
smoke's T_owner) carried the new generation, B pulled the record (`ResolveSetConfig`, tree 0 already
named d2), attached d2, armed its writer row — whose first act is a PULL (`WriterPlane.armVolume`
→ `topup`) — and A's holder answered `refused` while its arm (the allocation lease's tree-0 entry,
the bitmap's pages through ring 0) was still running. A slow arm under load widens the window; an
idle venue usually closes it before B's renewal.

### 3.2 The red row and the fix

- Red (`6231763`): seam `SQUEEZEFS_TEST_ONLINE_ARM_HOLD` — A's online arm holds (after the commit,
  before the lease) until its holder answered a data-plane verb naming a volume it has not armed,
  bounded by T_owner (a member renews within T_owner or is evicted). The online volume row runs A
  with it. On the old tree: B asks for d2 inside the hold, `TRIPWIRE 'cluster_wire_service_refusals':
  this mount does not hold the lease of that volume`, the hold ends on it — red at the closure's
  must-stay-zero.
- Fix (`13893d1`):
  - The generation moves at step 7. `World.publishPlacement(vol_tag, state, generation)`: the
    runtime act first, then the word (`lifecycle.publishConfigGeneration`, a monotone raise —
    `Gauge.raise` — so two ops publishing out of order never move it back). The rekey publishes
    after its plane's wrap. `lattice` "the fleet's word moves at step 7" pins it over RAM seams (a
    crash after the record tx leaves the record AHEAD of the word; a remount re-reads it).
  - `ResolveSetConfig` never serves a record ahead of the word: `Deferred` (a joiner pulling for
    an earlier bump while another op is mid-flight); the joiner counts
    `volume_config_pull_deferrals` and keeps its word.
  - The holder's answer to a volume it has not armed: `not_holder` (counted
    `block_grant_unarmed_answers`; the writer asks again at its next mint) when volume 0's record
    names it AHEAD of the word — a joiner that MOUNTED inside the window read it off the
    projection — and when no grant plane is armed on the manager at all (the listener precedes
    `bindDataPlanes`; a lease refused busy arms none: the first wave fired the tripwire there
    too). The refusal and its tripwire stay for a volume no such record names and for a verb
    misrouted to a joiner. The reference answers every case `KvError::Busy` (0002 row 533).
  - The holder's and the writer's armed rows publish their count after the row is whole
    (release / acquire): the wire, cadence and stats threads read the rows while an online arm
    writes the next one.
  - The row: A's hold now ends at T_owner with no verb for an unarmed volume (asserted on A's log);
    B learns d2 after A's arm.

## 4. The sideband abort (§AI.4)

### 4.1 The race

Not a double recycle. `Sideband.recycle` was `assert(free.tryPush(idx))` over a bounded MPMC ring
holding the pool's WHOLE population (`sidebandCells` is a power of two; the ring's capacity is the
cell count), and the pool has several takers: any handler thread's `notify` / `notifyEntryOrEpoch`
/ blocking reply, the sideband's own inline reply (the non-blocking pool: `notifyNb` and the loop's
owed epoch). Vyukov's push at position `p` needs the cell at `p & mask` to read `seq == p`, which
the pop at `p − cap` stores only AFTER its `dequeue_pos` CAS. A taker preempted between the two
while a second taker's pop and the CQE's give-back run past it makes the push read the cell still
filled and answer FULL — with the pool one cell short. `mpmc.pushSpin` documents exactly this
class ("2,874 refusals in 400 k pop→push cycles on an 8-slot ring"); the sideband's free lists
missed it. Under 4 × `yes` a preempted taker is common; the shared hot directory row's notify storm
supplies the takers — the abort's row.

The same assert stood in three more pools with several takers: the reclaimer's cell pool (any
freeing thread), the NVMe ranged read slab (any reader) and the zcrx cid pool.

### 4.2 The model, the red row, the fix

- Model `src/sync/mpmc_pool_model.zig` (exhaustive, `explorer.zig`): two takers, a two-cell ring at
  its population, each pops a cell and gives it back. The retried give-back HOLDS (no refusal
  surfaces; the ring ends holding each cell once); the weakening — ONE asserted `tryPush` — FAILS
  (`InvariantViolated`: T0 holds position 0 mid-pop, T1 pops position 1 and its push at position 2
  meets cell 0 still filled).
- Red (`6ef1a73`, venue): the two pools become one `CellPool` (free list + recycle word); the row
  "a sideband cell given back while another taker sits between its dequeue CAS and its sequence
  store" stalls a taker after its CAS (white-box: `dequeue_pos` advanced, the cell's sequence not
  stored), pops the other cell whole and gives it back on a thread — SIGABRT at `give`'s assert, the
  production abort's frame.
- Fix (`cabbcfa`): `CellPool.give` counts `transport_sideband_recycle_respins` and spins
  (`pushSpin`) until the taker stores; the row passes with one respin counted. `64bedb6` does the
  same for the reclaimer's cell pool, the ranged slab and the cid pool.

## 5. Loops

The harness (`/build/tmp/gb2/loop.sh`): one row's test binary run directly (the per-row smoke
binary of the tree under test, its `squeezefs` beside it), odd rounds alone, even rounds under
4 × `yes` (own pids); daemon logs copied every second and the mounts' `.stats` snapshotted; a run
passes when the binary passes and no daemon log names a TRIPWIRE; a failed run's logs and
snapshots are kept. The venue was shared with two other agents' gates and loops throughout (load
average 5–38, the host's 9–28).

| Row | Tree | Alone | Under 4 × `yes` | Tripwire runs | Note |
|---|---|---|---|---|---|
| rejoin | `dev` `93fedde` | 30 / 30 | 29 / 30 | 0 | load 5–8: the leak never exhausted the heap at this SMO rate; the one failure a kmbuf ENOMEM at B's mount (§AG.4's class) |
| rejoin | the fix | 30 / 30 | 30 / 30 | 0 | load 6–17 (two other gates beside it); every join granted its full 72; A ends the row with 811 free extents |
| shared hot directory | the fix | 30 / 30 | 30 / 30 | 0 | load 8–16; no abort; `transport_sideband_recycle_respins` 0 in every snapshot (the window is rare; the red row holds it open) |
| online volume | the fix | 5 / 5 | 5 / 5 | 0 | the row asserts A's hold ended at T_owner with no verb for an unarmed volume, every run |
| nvmet sigstop-preempt | the fix | 13 / 13 | — | — | `[venue-exclusive]` class: the row's binary alone (no other row of this agent; other agents' builds beside it, load 5–16), 53–58 s a run |

## 6. Gates

Before landing, on the tree as landed (`0a11a2b` and this note):

| Gate | Result |
|---|---|
| Linux (`zig build --cache-dir /build/zig-cache-v2 gate`, under the venue's gate lock) | 49/49 steps, 2,898 / 2,977 passed (79 skipped), 6 m 02 s, on `0a11a2b`; the same counts in 6 m 50 s on `0423c0d` (the tree before the rebase over `2814113`, a test-only commit of the no-open rows) |
| macOS (`zig build gate`, uncached) | 29/29 steps, 2,276 / 2,305 passed (29 skipped), on `0a11a2b` and on `0423c0d` |

The first Linux run on this tree failed three rows, none of them these items': `fuse.handler.lifecycle`
"no-open close bursts" (the no-open agent's row, fixed on `dev` by `2814113` — its drain stamp aged
inside the window on a freshly booted venue), `ipc.host.service` "the park ladder has no lost wake"
(130 ops past the 5 ms bound's many, under the gate's load) and `handover_moves` "THE MOVE MATRIX —
offer pushed": B's unlink parked at A for 218 s behind its own `acquire_slot` (the session's serving
half blocked behind a parked serve), then `HolderUnresolved`. The matrix's offered leg has no data
plane and returns nothing to the manager mid-row; run alone, interleaved with `dev` under 4 × `yes`,
it passed 5 / 5 on `dev` and 5 / 5 on this tree, and 3 / 3 alone — owed below. The first macOS run
failed the fabric's "duty cycle adheres within ten percent" (a wall-clock duty under the host's load
average of 13–28; 3 / 3 alone) and the literal rail (line shifts, `0a11a2b`).

## 7. Owed (seen beside this work)

- The handover move matrix's offered leg once parked B's unlink at A for 218 s behind its own
  `acquire_slot` (one session's serving half held by a parked serve) and answered
  `HolderUnresolved` — one gate run in this wave, not reproduced in 13 runs alone (5 under 4 × `yes`
  on `dev`, 8 on this tree).
- `slot_leases_held` (process-wide gauge) read 18,446,744,073,709,551,452 — −164 wrapped — on A
  after the rejoin row: the slot plane adds `leasedCount()` at its arm and `heldCount(me)` at the
  re-adoption but subtracts per release (`slots/plane.zig` :239, :425, :555, :663, :894); a gauge
  only, no decision reads it.
- The venue's kmbuf ENOMEM at a mount (§AG.4's class) failed one rejoin run in 60 and every mount
  during an hour when three LTO links held 13 GB of the venue's 16.
