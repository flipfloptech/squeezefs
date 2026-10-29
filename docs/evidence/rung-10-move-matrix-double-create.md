# Rung 10 — THE MOVE MATRIX's `drained: create answered PathAlreadyExists`: a holder answered for a slot it no longer held

**Seen.** Gate 173 (2026-10-29; the branch was FUSE-sideband-only, so dev's code), unit
`meta.sym.handover_moves` "THE MOVE MATRIX — step 0 drained …": `create answered PathAlreadyExists`
(green in gates 165–172); once more in a sibling agent's `meta.` run. On `578d73c` under load
(`loop.sh`: 6 copies of the matrix binary, 12 busy loops, an fsync loop) 2 failures in 108 runs: the
create's EEXIST and the drained UNLINK answering success with `f.unlink` still named.

**Not a double apply** — the create was applied zero times (its child compensated). A DIAG trace of
a failing run (the lease words, every dispatch, ship and serve): B's `InsertDentry` parked at A's door
(`Releasing`), was refused `SlotMoved` after A's release, and was shipped again; A's `serveStep`
passed its lease check INSIDE A's grant of the slot to B, then read the parent after the grant had
forgotten A's tree: no record → "dying parent" → `ForeignSkipped` → compensated → EEXIST.

1. **The witness had no hold on its slot (the law).** A lease check is a snapshot; a release and a
   grant can land between it and the read, and a DECLINED verdict commits nothing, so no door ever
   judged it. The reference re-checked the lease after the insert's dying-parent read
   (`mod.rs:1976–2010`) — lost in the port, and one read of one kind anyway. The same window answers
   an unlink's or a rename's absent source `AlreadyApplied` (0 returned, the name LEFT, the child's
   nlink dropped: a dangling dentry once reclaimed) and a shipped chmod/setattr's base absent (EIO,
   tripwire `record_refusals`).
2. **The manager's table showed the slot free mid-grant (the trigger).** `LeaseTable.acquire` ended
   the accepter's stand before it stored `leased{B}`; `reservedFor`'s latch-free gate saw no stand
   while the word still read `unleased`: the manager's resolver called the slot its own and
   `ResolveSlot` told B "unleased" (B shipped to A again). `release` had the mirror window.

**Reachable in a real two-mount set:** yes — a holder offers a slot BECAUSE the requester's ships
dominate it, so ships are in flight across every handover (the rig is two members over TCP; B's
member follower even accepts the pushed offer on its own). The application sees EEXIST for a name
that does not exist, an unlink or rename that returns 0 and leaves the name, or EIO.

**Red.** The matrix's new step **straddled** (`ops.ServeHold`): B's request is held at A past A's
lease check while A's transfer, B's grant and B's install land, then A reads; the matrix's files are
gathered into the directory's slot so every kind's object moves. Dev, deterministically: create
`PathAlreadyExists`; chmod, setattr `InputOutput` + `record_refusals`; unlink, rename
`TestExpectedEqual` (the name still there).

**Fix (decision 0003 §CQ).** The applier takes the step slot's door token BEFORE the witness and
carries it in `KvTx.door` into the entry to its terminal outcome (`enterFor` takes pre-held tokens);
the record-ship serve reads its base the same way; a slot no longer this mount's refuses at the door
→ `SlotMoved` → the initiator re-resolves. The release counts its stand before the word reads
`unleased`, the grant stores `leased{B}` before the stand ends, readers take the pair through
`viewReserved`; `slots/stand_model.zig` holds, `count_after_word`, `stand_before_word` (dev) and
`no_reread` fail.

**Verified.** Each loop run is the whole matrix (five steps × five kinds): the fix **444 runs, 0
failures** (186 on a private tmpfs `/build/tmp`, 138 + 120 after the rebase on the shared zram); dev **2 in
393** (2/108 at load ≈ 50; 0/150, 0/32, 0/103 quieter or slower) — rare, hence the straddled step as
the proof. Units `meta.sym` 364/365, `meta.` + `fuse.` 1,455/1,477 (skips only); `fmt rails`; smoke
rows THE TWO-DAEMON MATRIX, TWO MOUNTS OF ONE SET, the joiner rename matrix, A JOINER'S CLEAN UNMOUNT
AND REJOIN, A LIVE MEMBER … RE-JOINS IN PLACE, A DEAD MANAGER HOLDING A STRIPED HOT DIRECTORY pass.
The retired sibling (`offer pushed` → `rename answered HolderUnresolved`): not reproduced; same
exposure, same cause plausible, not proven (§CQ.6). Seen, not changed: §CQ.7.
