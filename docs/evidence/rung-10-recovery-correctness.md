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
