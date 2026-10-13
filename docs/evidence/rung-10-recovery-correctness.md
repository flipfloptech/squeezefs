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

A follow-up (`volume_bind.endRetirementHoldFn`): 7b parks only the held extents NO maintained tree
reaches at that instant, walked under the same SMO-lock window. A root install discards the slot's
cached nodes and stores the page's root; an SMO that retired one of that root's images off the
cache before the install would otherwise see the image parked and freed while the installed tree
still reaches it. Not reproduced (the flush pass touches no clean projection node); the walk is the
law's own check, not a guess.

**The ownership law:** an extent of a dead grant has ONE custodian at every instant — the grant
(named by the record), a tree (reached), the recovery's hold, or the coverage clock — and moves
between them only under the volume's SMO lock. Lock order: the SMO lock, then the manager's verbs
lock; the returns run after the hold ends and outside the SMO lock. No lock-free core changed (the
hold is read and written under the SMO lock alone), so no model was added.

### 1.4 The stage under the manager's maintenance, looped — and what it found

The red row pins ONE interleaving with a seam. The narrowed stage drives the same recovery under a
racing maintenance stream instead: `volume_bind` "linux: THE RECOVERY UNDER THE MANAGER'S
MAINTENANCE, looped" (`SQUEEZEFS_TEST_RECOVERY_RACE_LOOPS=N`, a harness row; skipped unarmed). Per
pass: a fresh set; a WIRE joiner B writes 1,500 records in batches of 100 into its rotor slot (its
splits draw its own grant: a multi-leaf tree whose images are the grant's), cycles after 1,200, and
dies with a 300-record window; the manager reopens the copy, records the death, and recovers it
while a racer thread runs the D4 compaction over every resident leaf of the recovered tree (two
rounds, under the SMO lock, from the root install to the table release). Checked every pass: the
recovery acted, every acked record readable, the budget conserved (`free = total − set bits`), and
ONE custodian per extent (every image any maintained tree reaches is allocated; every claimable
extent — all of them claimed and handed back — is reached by none) before and after two covered
checkpoints. The first 20-pass runs met four more defects of the same law, in order:

| # | Met as | Cause | Fix |
|---|---|---|---|
| 1 | `failed at Corrupt` after step 6 (the debug trace: `walkTree` of the dead slot) | step 6b's tails walk ran without the SMO lock: an SMO between its level read and its pointer read handed it a node of the wrong level | `leafTailsFn` under the SMO lock (as `keptExtents` already was) |
| 2 | `failed at Corrupt` in the walk again — a `RetiredExtent` route | ring 0's seq floor was raised above the dead ring's frontier only at 7b; a pointer flip the manager stamped before it LOST the per-key LWW to the dead window's interior record for the same separator (applied at its own, higher seq): the tree routed to the image the SMO had retired | the raise moved to step 4, before the root install (monotone: a rollback keeps it) |
| 3 | acked records MISSING after the recovery | no SMO lock between the root install and the replay: an SMO on the installed tree before the dead window's interior records mixed a pre-split image with the split's children | the run holds the SMO lock from the first root install to the replay's last record (`Volume.holdStructure`; `installRoot`/`restoreTree` run inside it) — the charter's "from the root install on — the SMO mutex" |
| 4 | the testing allocator's leak report under heap pressure | the conveyor's heap-admission refusal list (`refused`, step A6) was never freed | `defer refused.deinit` |

(A first racer that compacted clean leaves without end met `64 cycles left a recovered slot tree
dirty` — a stream no product SMO produces; the racer runs two rounds.)

The loops on the fixed tree: 20 passes green (20 recoveries, 145 racing compactions), then **150
passes green: 150 recoveries, 1,065 racing compactions, 0 custody breaches, 0 rollbacks, 0 aborts**
(`SQUEEZEFS_TEST_RECOVERY_RACE_LOOPS=150`, 10 min on the shared venue). A test-result cache note
for whoever repeats it: the harness knob is not part of zig's test cache key — a green run re-run
with a larger N answers `cached` unless a source input changed.

### 1.5 Not covered by this item (stated)

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

## 4. An abandoned `link`'s undo (item 4)

### 4.1 The defect

`rung-10-striping-wire.md` §3.6: the roll-forward that re-meets a live refusal compensates (§Q.19,
fix 10) — inverting the steps applied NOW and the fresh mints found applied, because "any other
step's earlier effect cannot be told from a later op's". A `link` is `set_nlink(n → n + 1)` then the
insert; its ship of the insert fails against a dead holder, the intent stays open, the caller gets
EAGAIN — and its `+1` stands. When the roll-forward later meets the name taken by another object,
the re-dispatched count step answers `already_applied` (not a fresh mint: not inverted) or, if a
later link moved the count on, a witness refusal at step 0 — either way the count stays one too
high (fsck C10).

### 4.2 The red row and the fix (0003 §U.4)

`two_holders` "an abandoned LINK whose name another object took meanwhile is compensated by the
roll-forward INCLUDING its count": A's file (`orig`, nlink 1); A links it into B's directory as `l`
with the insert's ship failing (`ship_fail_at = 1`) — nlink 2, no name; meanwhile A links it again
as `again` (nlink 3) and creates a NEW file named `l` in B's directory; the roll-forward (the
cadence tick, and — the second pass — a remount's `recoverOpen`) re-meets `l`, compensates: nlink 2
= `orig` + `again`, `l` the new file's, no open intent, the closure holding. Red on the item-3 tree:
`expected 2, found 3`.

The fix: the op that leaves an intent open at step `i` attests `applied = i` in the record's header
(the former reserved u16) and barriers it; the roll-forward takes attested steps as applied without
re-dispatching them; a compensation rebases every applied count onto the count it reads under the
plan's guards before writing the ABORTING image, so the inverse removes exactly this intent's delta,
once (a re-run after a crash mid-compensation inverts the rebased witness and finds it done).
`intent` "the intent image round-trips" pins the word (it survives the codec; a prefix past the step
count refuses); the decoder's fuzz target re-encodes it.

### 4.3 Not decidable (stated, the leak direction)

- An initiator crash between the failing step and the attestation's barrier leaves `applied = 0`:
  the roll-forward knows fresh mints alone, as before.
- The failing step itself: a ship that failed after the holder applied (the reply lost) and a LATER
  op with the same effect (the same file linked to the same name) answer the same `already_applied`
  — the dentry carries no intent stamp. Treating it as the intent's own is the safe reading (a count
  too high is a leak C10 reports; one too low would let a later unlink free a file a name still
  reaches).

## 5. A dead daemon's address in the manager's endpoint table (item 5)

`Manager.forgetEndpoint` ran on an in-process leave only, so after a member's death
`ResolveEndpoint` kept naming the dead daemon's port (`rung-10-striping-wire.md` §3.6) — a port any
other process may bind next. The rows now carry the identity they were published under; the home
shard's eviction retires every row of the evicted identity on every volume manager in the process
(the death fan-out), and the manager's own page write away from Live retires the appender's row —
the recovery's Recovering for a volume whose manager is not the shard's owner (0003 §U.5,
`manager_endpoints_retired`).

Red rows: `manager` "endpoints: a dead appender's row is retired" (the page written Recovering ⇒
`ResolveEndpoint` answers none, a publish under the dead page refuses; a second appender's row
retired by identity at the eviction; an identity that published nothing retires nothing) — red on
`dev`: the resolve after Recovering still named the port. `wire_plane` "THE SAME-NODE SUCCESSOR IS
REACHED" now asserts the corpse's row gone at the eviction (`expireDue`) and the successor's publish
resolving again.

Stated: a dial still trusts the address it resolved — the handshake proves the DIALER to the
listener (the enrolment secret), never the listener's identity to the dialer, so a stranger at a
recycled port that holds the secret is the set's own member anyway; a listener identity proof is a
protocol change, not made here.
