# Rung 10 — recovery correctness (§5.9 after the striped shape)

Charter §5.5 "Death and recovery (§5.9)", §5.3 "Space" (the pending-free coverage gate), "Tokens",
"Directory striping", "Cross-owner namespace ops". The items close what
`docs/evidence/rung-10-striping-wire.md` §3.6 left owed around a dead member's recovery. Decision
rows: 0002 row 508; 0003 §U. Venue: the macOS `container` (`7.2.6-sqz`, 9 vCPU, 16 GiB), shared
live with the `.wt-jobs2` and `.wt-gatefix` agents.

## 1. The recovering manager's abort (item 1)

### 1.1 The defect

`rung-10-striping-wire.md` §3.6: about once in 75 runs of the shared hot directory row, daemon A
aborted while recovering the killed B, in one of `alloc_core.ExtCore.freePending`'s three asserts,
reached from `smo.smoReplace` ← `SmoContext.maintain` ← `Checkpointer.flush` ← `drainMaintenance` ←
`CheckpointTask.runOnce`. The FIFO's producers all hold the SMO lock, so the search for "a second
producer" found none.

The race is not between producers. The recovery driver (`sym/membership/recovery.zig`) takes the SMO
lock only inside its root install and its rollback; between them the volume's SMOs run on the
recovered slot trees — the recovery's own flush cycles (step 6: a leaf whose log is full compacts),
the checkpoint task's maintenance and merge sweep, the D4 arm. Such an SMO retires one of the dead
appender's images `X` into the manager's pending-free FIFO (bit set, gated on the free record's
position). At step 7 the record was rewritten as `grant − (the recovered trees' images)` — `X` is in
the grant and no root reaches it — and step 7c's census read `X`: allocated (the FIFO holds its
bit), unreached, a residue the dead appender stamped ⇒ an ORPHAN, returned through the manager's
verb: its bit cleared, the budget credited. Two custodians. The heap's hint now points at `X`; the
next claim — an SMO successor under A's storm — takes it; the FIFO's drain at the next covered
checkpoint `release(X)`s the LIVE node's bit; the node's next retirement asserts `bitIsSet`
(`alloc_core.zig:342`). With asserts compiled out it is a double allocation.

A second, independent hole on the same path: `returnExtentsFn` released EVERY extent of a batch
whenever the manager's verb cleared ANY (`out.cleared != 0`), so a page word naming an extent the
appender had already returned (and the heap re-issued) would release a live extent too.

### 1.2 The red rows

- `volume_bind` "linux: THE RECOVERY'S CENSUS AGAINST THE MANAGER'S OWN SMO" — two-member loopback:
  a WIRE joiner B (a second `MetaSet` joined over TCP) writes into its rotor slot (the tree minted on
  its first touch, in B's grant), cycles, writes a window more and dies (the image copied with its
  page Live). The manager reopens the copy with `SQUEEZEFS_TEST_RECOVERY_HOLD=park_after_flush` (the
  new seam: the recovery parks at the end of step 6, the trees installed, replayed and clean, tree 0
  still Leased, until the harness releases it); the poll runs on a thread; at the hold the test runs
  the manager's D4 compaction (`smo.compactForDefrag`) on the recovered root leaf `X` — B's image;
  then releases. Asserted: B's records all present; `dead_appender_orphans` unchanged; `X` still
  allocated; the record no longer names `X`; the budget conserved (`free = total − set bits`, read
  under the SMO lock); two covered checkpoints later `X` free, the budget conserved. Red on `dev`:
  `expected 0, found 1` orphans. A scratch variant (not landed) carried the red to the abort itself:
  `claimInternal` handed back `X` at once, two checkpoints later the claim's bit read clear, and
  `freePending` of the claimed extent aborted at `alloc_core.zig:342` `assert(self.bitIsSet(extent))`
  — the production frame.
- `recovery` "the volume's own SMO retires a dead grant's image mid-recovery" — the RAM fixture's
  flush cycle replaces slot 10's image 101 (the grant's) by 120 and retires 101: red on `dev` (101
  returned as an unwritten extent while the coverage clock parked it); green: 101 never returned,
  never named by the record, parked exactly once.

### 1.3 The fix and the law (0003 §U.1, 0002 row 508)

From its first root install to its table release the recovery HOLDS every retirement of a dead
grant's extent (`smo.RetireHold` on the volume's `SmoContext`, consulted first by
`SmoContext.freePending`; its capacity reserved at install, so an SMO allocates nothing after its
swap; a second retirement of one held extent is the `meta_kv_retire_hold_breaches` tripwire). Step 7
reads `keptExtents` in ONE SMO-lock window — the images of every tree this process maintains (the
recovered slots', the unleased ones, its own regions', control and native; a live foreign lessee's
projection is never walked: its interior may name extents its lessee retired long ago) plus the
held — and the record is the grant minus them; the census judges only that remainder. 7b parks the
held extents in the FIFO gated on ring 0's reservation head: each one's own free record and every
gate the FIFO ever took are positions below it, so the gate is conservative and the single producer's
gates stay monotone. A rollback drops the hold: the extents stay allocated, since the restored tree
may still reach them. Every return keeps only the extents the record names and clears exactly those
bits (`recovery_return_mismatches` must-stay-0).

**The ownership law:** an extent of a dead grant has ONE custodian at every instant — the grant
(named by the record), a tree (reached), the recovery's hold, or the coverage clock — and moves
between them only under the volume's SMO lock. Lock order: the SMO lock, then the manager's verbs
lock; the returns run after the hold ends and outside the SMO lock. No lock-free core changed (the
hold is read and written under the SMO lock alone), so no model was added.

### 1.4 Not covered by this item (stated)

- **A slot release does not move the tree's images out of the releaser's grant record.**
  `slots.acquire.Verbs.releaseSlot` writes `Unleased` + the tails and never touches `extent_grant:`;
  the record keeps naming images whose tree the manager (or an accepter) now maintains. Inside a
  recovery `keptExtents` covers the images that are still tree nodes; an image the new maintainer
  retired AND the heap re-issued to a non-tree structure (a tails spill, a minted root before its
  tree joins the set) before the releaser died stays judged by content. Re-issue to another
  appender's grant is refused (`heldElsewhere`, the `extent_grant_conflicts` tripwire) — which is
  the same stale attribution surfacing as a refused carve. Owed: the release names the tree's images
  and the manager subtracts them from the record in the same entry.
- **A crashed recoverer** (5d of `rung-10-symmetric-completion.md`): the SMO entries a recovery's
  cycles journal on ring 0 still carry `free` records of held extents; a manager crash before 7b
  replays them as parked frees while tree 0 still leases the slots to the dead appender. The hold is
  RAM; the durable side is the stash 5d owes.

## 2. Stale reads after a recovery (item 2)

### 2.1 The window

`rung-10-striping-wire.md` §3.6: a joiner C drops the tokens a dead holder B granted it only at its
census commit that retires B (fix 11 of that note); the commit comes with C's next census pull, once
per renew interval (10 s at the shipped `T_owner` of 45 s: `renew = clamp(T_self / 3, 1 ms,
10 s)`). B's grant table died with B, so nobody recalls those tokens. The manager A recovers B's
slots ≈ 2.4 s after the eviction (the poll's 1.1 s tick + the 1.3 s bound) and the slots are
`Unleased` — A's first write to an object C still caches from B is served at once, and C keeps
answering the pre-recovery value from its cache until its pull.

### 2.2 The law chosen (0003 §U.2)

The successor holds every recovered slot's WRITES until the death record's ts +
`VALIDITY_RENEWALS × renew + 2 × skew_max` (`recovery.coherenceHoldMs`): every member pulls the
owner's census once per renew interval and a view answers for two of them, so by then every member
whose view stands has committed one that names the death and dropped B's tokens; `2 × skew_max`
absorbs the recorder's wall clock (the custody quarantine's term). The hold is installed at the
table release (7b) on the volume's monotonic clock (`LeaseTable.holdCoherence`); the door answers a
commit's first touch `.held` → `SlotBusy` (EAGAIN, the class the recovery's own window answers),
`AcquireSlot` refuses a joiner's first touch naming the manager, and the rotor pick skips the slot.
Reads are never held. The derivation that rejects the alternative — pushing the eviction on the
renewal carriage — is in 0003 §U.2: the carriage reaches a member at its next renewal, after the
recovery has landed at the shipped clocks.

Tie: `recovery` "the coherence hold derives from the census law" (20,044 ms at `T_owner` 45 s:
renew 10,000 ms, skew 22 ms; 534 ms at the smoke's 3 s: renew 266 ms, skew 1 ms), restating the
formula over `shard.LeaseClocks.derive`. The RAM end-to-end row asserts the hold installed on both
recovered slots at the record's ts + the hold.

### 2.3 The red row

`volume_bind` "linux: THE COHERENCE HOLD" — the dead in-process region's copy recovered by the
manager's poll with the death recorded NOW: a commit into the recovered slot right after the
recovery answers `SlotBusy` (`slot_door_coherence_refusals` + 1, the slot still `Unleased`), and a
joiner's first touch over `AcquireSlot` is refused naming the manager
(`slot_acquire_coherence_refusals` + 1); the same copy with the death recorded one hold and a
second ago admits the commit at once (the slot leased by the manager, the record readable). Red on
the item-1 tree: `expected error.SlotBusy, found void` — the write landed the instant the slot was
`Unleased`. A first draft refused only at `AcquireSlot`: the manager's own door re-read the slot
`Unleased`, re-tried the first touch, and SPUN until the hold ended (the debug trace showed the
refusal repeating for 20 s) — hence the door's own `.held` verdict.

### 2.4 Not covered (stated)

- **A member whose census pulls keep failing** keeps the dead holder's cached tokens past the hold
  (its view goes `unknown` after two intervals, but nothing drops the tokens). Its renewals ride
  the same link, so it parks at `T_self` — and a parked member still serves reads (§2.6 of the
  recovery spec). Closing it is a reader-side law: a cached token of a non-manager holder is served
  only while the member's census view is valid. Owed.
- **The early end:** the owner sees every member's renewals; ending the hold once every live member
  renewed after the eviction with the departure aboard would cut the shipped ≈ 17.6 s to ≈ one
  renew interval. Owed as an availability refinement (it needs the departure on the carriage and
  the recovering manager to be the census owner, which a multi-volume set does not guarantee).

## 3. The merged `readdir`'s window (item 3)

### 3.1 The defect

`rung-10-striping-wire.md` §3.6 / §Q.20: the K-way merge filled every stripe's stream first and
`D`'s own last, and dropped `D`'s copy of a name when a FRESH scan of its stripe found it. A name the
lazy migration moved between its stripe's read and `D`'s read (the move inserts into the stripe, then
removes from `D`) was in neither snapshot and missing from the listing; a name moved after the
stripe's snapshot but before `D`'s copy popped was dropped from `D` by the fresh scan while the
stripe's stale snapshot lacked it — missing again. The reference's merge has the same order (0002
row 509).

### 3.2 The red row and the fix (0003 §U.3)

`stripe` "readdirMerged: a name the lazy migration MOVES between any two of the merge's stream
reads": a RAM dentry source (`MoveSource`) with K = 2 stripes and 40 names — the even ones in their
stripes, the odd ones still in `D` (`D` spans several merge pages at `max` = 6) — runs the move of
each odd name before EVERY page read index in turn (1 … the run's read count + 2), lists the
directory to its end through the continuations, and asserts every name exactly once. Red on `dev`:
`expected 1, found 0`.

The fix: `D`'s stream is read first; a stripe's entry past `D`'s loaded range waits for `D`'s next
page; on that page every stripe whose loaded page reached past `D`'s old range is read again from its
last consumed cookie (`dir_stripe_readdir_restarts`); the order is (cookie, the stripe's copy first)
and a name listed once in its hash group is not listed again — so the copy `D`'s snapshot and a
stripe's both hold is listed once, and a name `D`'s snapshot lacks is in every later stripe
snapshot. The fresh per-entry stripe scan (one stripe walk per `D` name) is gone, and with it the
merge's `name_key` argument.

### 3.3 Residual (stated)

The dedupe meets two copies of a name in one HASH GROUP (`cookie >> 8`): their cookies differ only
when the name's collision index differs between `D` and its stripe — a 54-bit keyed-hash collision
inside the directory. If such a pair also straddles a reply boundary with the stripe's copy first,
the next reply can list `D`'s copy again (a duplicate, never a miss). Not met; stated.
