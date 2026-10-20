# Rung 10 — the dismount pass judges its residents: a corpse's or an unnamed ring record is destroyed, never kept (THE KILL MATRIX's two-ring `[C4]`)

**The red** (gate train `local-039ae6807ad5-20261019T172805Z`, THE KILL MATRIX under
`SQUEEZEFS_FUSE_RINGS_PER_QUEUE=2`, 1 in 11): the row's final clean-unmount `fsck --offline` reported
`[C4] staging custody 02000000000e0000f091c656c9fedf18 ino 15393162788866 — orphan: the ino has no live
record` and daemon A's clean unmount had logged `dismount promotion: 0 promoted, 1 FAILED`. The
orchestrator read it as a staged resident promoted before the reclaim cadence destroyed its corpse. It
is not that: `fs.destroy` (the gathered corpses' destroy) runs inside `session.close()`, BEFORE
`plane.dismount()` promotes anything.

**The ordering, found** (the kept daemon log `/build/tmp/fsck-refused-1635378/daemon.log`; the same
shape again at 19:06 in `fsck-refused-3529740`; then reproduced under 6-wide load with the promotion's
decline reasons logged, `~/tmp/dc/before-par/r5-k5.log`):

1. The storm kill lands between a storm file's promotion to its own block — the 33rd write past 4 MiB,
   `promoteToStriped`: own-block DMA, striped layout publish, `commit` — and the ring release that
   follows in `afterCommit`. The release is OUTSIDE the transaction: the ring keeps a 4 MiB record
   (`generation 184`) the striped layout no longer names. Under load the storm kill leaves a ring record
   in ~10 of 180 runs (mostly a legitimately staged 4 MiB file, promoted by the next pass); the
   unnamed shape in 1 of 180.
2. The post-kill fsck's close runs the dismount pass: `promotion ino 15393162788866: stale — layout kind
   striped, file id DIFFERENT` → counted FAILED, kept as "never-wrong". The daemon's remount recovers it.
3. `rm` → FORGET → the cadence's `destroyCorpses` destroys the inode; its destroy rider releases only the
   record the layout names (`after.staged` iff `kind == .staged`) — the residue stays.
4. The daemon's dismount: `stale — no layout record` → FAILED, kept. The fsck after the clean unmount:
   the ino has no live record → `[C4]`.

Two rings per queue only widen the kill's landing window (more concurrent writes per daemon under
the train's load); the bug is independent of the ring count.

**What changed** (`fix(data)`, this branch).
- **The dismount pass judges** (`staging.DismountVerdict`; `PromotionHooks.dismount`;
  `plane.judgeAtDismount` → `promoteUnder(.dismount)`), under the promotion's (3.5)+(3.7) locks:
  the ino's inode absent or `nlink == 0` ⇒ `.corpse` — a corpse's custody is destroyed, never promoted
  (`dismount_corpse_residents_destroyed`); the layout not naming the record (none, kind not staged,
  another file id) ⇒ `.unnamed` — residue no read can reach, destroyed
  (`dismount_unnamed_residents_destroyed`); a resource decline ⇒ `.kept` (FAILED, resident — the
  never-wrong rule, now for the one case it is right). A foreign slot's ino: `.gone` at its holder is a
  corpse; a held token is judged by its carried layout alone. A spilled block (`writeback.blockKey`) is
  judged by its ino alone. The live points (pressure, fsync, syncfs) are unchanged: an unlinked-open
  file reads its custody until its FORGET. The log line carries the tally; `staging promote` prints it.
- **The seam** `SQUEEZEFS_TEST_PROMOTE_KILL_AFTER_COMMIT` (read by `cli/mount.zig` → `kv_fs.Options`):
  the daemon SIGKILLs itself after the write path's promotion commit, before the ring release — the
  window pinned.
- **The red test**: smoke `KILL -9 BETWEEN A PROMOTION'S COMMIT AND ITS RING RELEASE` — a 4 MiB file
  staged by fsync'd writes, one write past the block, the seam kills; (a) remount without fsck → `rm`
  → the clean unmount's pass: `0 FAILED, 1 corpse record(s)` → fsck clean; (b) fsck's own pass:
  `1 unnamed record(s)` → the remount recovers 0 → the file byte-exact (the promotion's own block) →
  `rm` → fsck clean. On `21d2e25` the row ends exactly as the train did: `dismount promotion: … 1
  FAILED`, then `[C4] staging custody 020000000002000082801f107814e018 ino 2199023255554 — orphan`.
  The staging unit test of the pass asserts the four verdicts' tallies and the ring's end state.
- Decision 0002 row 547; `data_layouts.md` "Clean unmount" row and the gauge table.

**Before / after** (THE KILL MATRIX, two rings, the smoke binary run 6-wide, `~/tmp/dc/par.sh`;
each run is the whole row: three mid-dd kills, the storm kill, the clean unmount's fsck):
- before (`21d2e25` + logging only): 16 serial + 180 parallel runs — 1 `[C4]` (`r5-k5`), the FAILED
  promotion at the storm kill's fsck in that run alone; a ring record at the storm kill in 10 runs.
- after: 180 parallel runs — 0 `[C4]`, 0 FAILED; the storm kill's fsck pass destroyed an unnamed
  record in 2 runs (`r7-k2`, `r11-k5`: `1 unnamed record(s) destroyed`, the remount recovered 0, the
  clean unmount's fsck clean) and promoted one or two legitimately staged files (4 MiB, once 3.5 MiB)
  in 10 runs.

**The kill -9 path for a NAMED record of an unlinked ino** was already right: the mount's
`sweepCorpsesAtMount` destroys every `nlink == 0` inode with its destroy rider, whose `after.staged`
releases the ring record the staged layout names; the kill rows are green on it. What it never covered
is the unnamed record, which only the dismount pass sees — now destroyed there.

**Tests**: `scripts/venue test` for `staging` 68/68, `kv_fs` 93/93, `fsck` 139/139, `mount` 277/277,
`plane` 172/172, `knobs` 55/55; rails 29/29; the smoke rows data plane with/without a cache path,
`sync -f`, kill -9 mid-tar, two mounts of one set, fsck and scrub, the staged read's identity window,
THE KILL MATRIX (two rings, 180 runs) and the new row — green.

**Owed**: a spilled block record (`SQZBLOCK` key) that a kill leaves in the ring has no adopter at the
next mount (`blockOfKey` has no product caller): it is kept FAILED at every dismount while its ino
lives and destroyed as a corpse's once the ino is gone. The flush unit's adoption at mount is the
remaining piece of the never-lossy spill across a crash.
