# Rung 10 — the staging ring's member scope, a joiner's rejoin, and the shipped free (2026-10-12)

The jobs agent stated two defects while building the fleet READ shard row (`rung-10-jobs.md`
§4): two mounts of one set on one host shared ONE live staging ring, and a joiner's clean
unmount + rejoin with the manager up was refused. The rejoin row's staged files then exposed a
third, worse one: a joiner's cleanly promoted staged files were destroyed by the manager. Design
calls: `0003` §T; reference departures: `0002` row 500. Venue: `sqz-zig` (Ubuntu 24.04, kernel
7.2.6-sqz, 9 vCPU / 16 GiB), the two-daemon rig of `src/cli/smoke.zig` and a pid-tagged shell
repro (A = manager, B = joiner over TCP on one host, one cache path).

## 1. A joiner's clean unmount + rejoin (defect B)

**Repro (dev 37cddb2, ReleaseSafe).** A mounts, B joins and writes 100 files, `squeezefs umount`
B (clean: 64 slots released at g 1, the page Free), B mounts again at the same mount point: 4 of
5 attempts refused `cannot open metadata set …: corruption outside the replay window`, with no
error line in B's log naming a cause.

**Root cause.** The error-return trace of the open: the JOINED backend's first checkpoint →
`publishGuestRoots` → `slots.Plane.publishRegionBeforeTail` → `publishTree0(slot)` →
`PublishRoots` over the wire → the manager's screen refused (`slot 115: cur unleased appender 0
g 1 vs req appender 1 g 1`) → `Corrupt`. B's rejoined incarnation believed it held slots of its
PREVIOUS incarnation: the manager's `appender_hint` hands a rejoin the same appender id (1), and
`armJoined` loads tree 0's PROJECTION — the manager's last ledger record plus whatever tree-0
leaves an SMO had carried onto the device since — into the lease table as-is. Rows `Leased{1,
g 1}` whose releases the manager had journaled but not yet recorded loaded as HELD by the new
appender 1 (the diagnostic run: the projection held `{107 … 128}` as ours at `mounted seq 4`,
the manager's record unchanged between B's two opens). The page plan then published their roots
and the manager refused. Had no publication been due, the door would have admitted writes into
slot trees this mount did not lease — the partition law broken silently. The race window is B's
remount inside A's 1 s checkpoint cadence, hence "most times".

**The law (0003 §T.2).** The joined open refuses a `Live` page under its identity
(`PredecessorLive`), so a rejoin is always a fresh incarnation holding exactly the manager's wire
grants. `lease_table.joinedProjectionRow` loads every projection row naming the joiner's OWN
appender id `unleased` (the words kept as the cold floor, the holder cache never learns it); a
first touch of such a slot asks the manager, the live truth. Counted
`slot_projection_stale_self_rows`, logged at info.

**Red rows.**
- `meta/sym/wire_plane.zig` "linux: A JOINER'S CLEAN UNMOUNT AND REJOIN while the manager stays
  up …" — a TestSet manager with its wire plane up, B a `MetaSet.open(.join)` in-process over
  TCP on 127.0.0.1. The race made deterministic: the manager's cadence held
  (`SQUEEZEFS_CHECKPOINT_MAX_AGE_MS=600000`) and ONE rotor slot (`SQUEEZEFS_SYM_MINT_SLOTS=1`: one
  release, no tree-0 split carries the released row onto the device). The precondition is
  asserted (a reader open reads the slot `Leased{appender 1}` on the device while the manager's
  table says `Unleased`). Before the fix the second open answered `error.Corrupt`; after, the
  rejoin holds exactly its new grant, the stale row reads unleased, the counter moves by 1, and a
  record committed in the new slot survives the next cycle. 10/10 green.
- `meta/sym/slots/lease_table.zig` "the joined projection law …" (portable): a leased or
  releasing row naming the joiner demotes, words kept; another lessee's row and an unleased row
  are verbatim; loaded, the joiner holds nothing.

**Smoke row** (`smoke: A JOINER'S CLEAN UNMOUNT AND REJOIN while the manager stays up`): A up;
B joins and writes 49 files per cycle (40 × 1 KiB inline, 8 × 64 KiB staged, one fsync'd 8 MiB
striped, cut from the rig's random image); three cycles of `umount` B (clean exit) → A reads
every file of every cycle sha-equal → B mounts again (`JOINED as appender 1` in the new log
segment) → B reads every file sha-equal → B writes the next cycle; then B's last unmount, A
reads all 196 files sha-equal, A unmounts, `fsck --offline --scrub` 0 findings (201 inodes, 16
blocks). **10/10 green**; the race was hit in 7 of the 10 runs (16, 39, 17, 8, 14, 10 and 27 stale
rows demoted across a run's three rejoins) — each a refused mount before the fix. Repro after the
fix: 5/5 rejoins mount.

## 2. A joiner's promoted staged files destroyed by the manager (defect C, found by the row)

**Repro.** A up, B joins, B writes four 64 KiB files (staged in B's ring), `umount` B: "dismount
promotion: 4 staged file(s) (278528 B) promoted, 0 failed" — then every one of the four files
read EIO on A, ZEROS on B's rejoin and on A's remount; `fsck --offline --scrub` clean (6 inodes,
4 tenant windows readable). A's `.stats` across B's unmount: `terminal_frees` 0 → 2,
`block_free_file_punches` 0 → 2, `block_free_punch_bytes` 8,388,608, `free_seeded_references` 2.
The same repro with a per-process staging root (a scratch hack) failed identically — not the
shared ring.

**Root cause.** B's dismount sealed its two open packs; the seal releases the pack's pin
(refcount `committed + 1` → `committed`, nonterminal by the pin arithmetic, no
`pack_pin_refcount_drift`). On a non-holder `Reclaimer.freeBlock` SHIPPED every release to the
allocation holder BEFORE its own ladder ran; the holder's executor found the block untracked
(nothing minted it there) with durable population 0 — it cannot read the references a LIVE
lessee's slot trees carry — and its Row 7 ("a former lessee's mint") seeded ONE reference and
freed it: two pack blocks punched under four live tenants. The reference has the same shape
(`free_block_verdict` ships before the ladder; 0002 row 500). The same path frees a joiner's
pack at the holder whenever one of its tenants is released while siblings live.

**The law (0003 §T.3).** Charter §5.5: TERMINAL frees of a non-holder ship. The non-holder now
releases LOCALLY while its own RAM count covers another reference
(`BlockAllocator.releaseUnlessLast`: CAS down from ≥ 2; a SHARED block excluded — its
terminality is the share home's); only the last reference, a SHARED block and an untracked one
ship, and the shipper's verdict seams retire or release the local word as before. A stale-high
local count can only leak (fsck C2 reconciles), never free a referenced block.

**Red row.** `data/reclaim.zig` "a NON-holder ships only its TERMINAL frees …": a block with three
references on a non-holder reclaimer — two releases shipped 2 blocks before the fix; after, both
stay local (refcount 1, allocated, nothing shipped) and the last ships. The repro after the fix:
all four files sha-equal on A after B's unmount, on B's rejoin, with B up, after B's second
unmount and after A's remount; fsck clean.

## 3. Gates (landing 1: §1 + §2, on dev 900caad)

- Linux (venue, `zig build gate`): **46/46 steps; 2,711 / 2,774 tests passed, 63 skipped**; unit
  2,480 pass / 61 skip; smoke 80 pass / 2 skip (the rejoin row and the online fsck row among them).
- macOS (`zig build gate`): **28/28 steps; 2,142 / 2,168 passed, 26 skipped**.
- `smoke: ONLINE FSCK THROUGH THE ADMIN LANE` failed 2 runs in 3 on pristine dev 37cddb2 (attributed
  in a detached worktree) on its own stated gap: the set stage's C9 over a joiner's cross-owner mint
  (`rung-10-jobs.md` §4) fires the coordinator's `fsck_findings` tripwire site and the row asserted
  A's `invariant_tripwires == 0`. The row now requires A's total to equal that site's count (B's
  total stays 0); the gap itself stays its owner's.
