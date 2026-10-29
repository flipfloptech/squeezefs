# Rung 10 — create 7b's owed list: the double park, the straggler Δ, the setid strip, the load-only reds (2026-10-28)

The list (`rung-10-create-path-7.md` "Seen, not fixed"; handoff 10-21 item 2). Venue: this workstation,
`7.3.0-1.7-mainline-sqz`; branch `fix/meta-owed` off dev `7693bab`; scratch `~/tmp/meta-owed/`.
Decision 0003 §CG, 0002 row 562.

## 1. The DEAD MANAGER hang — B parks twice (closed on dev before this branch; verified)

**Scenario** (the 10-22 train): B's renewal missed `T_owner` (3 s) under load → A evicted the LIVE B →
B's second `T_self` park → the reclaim met the closed grace → `UnknownLease` → POISONED → a cross-owner
create parked at `[handler_entry]` for good, the storm child unkillable. **Causes, each pinned by a seam
and fixed:** the beat ran every member wire duty inline (FOLLOW_HOLD row red on `638b9a6`: A evicted
the live B 4.1 s into the hold; `13e3334`); the committers never judged `T_self`, the poison left the
move gate holding each foreign op `T_park_max`, the fenced leave wrote, a stale epoch answered
`UnknownLease` (BEAT_HOLD row, a create HELD; `c3e53e2`: the poison closes the move epoch, EIO in
2–6 ms here); a member evicted holding nothing un-landed re-joins in place (§BS). No change here.

**Verified:** the three DEAD MANAGER rows 30× beside 24 `yes` and four agents' builds (load 40–75):
**0 hangs**, 28 green. Runs 19 and 21 (the F4 row) re-joined in place where the row pins the fail-stop —
a test defect: its custody precondition was unpinned against a venue tenant's sync(2) (§5 (a)).

## 2. The replay gate dropped a straggler Δ (fixed here)

**Scenario.** W1 < W2 apply in either order (§AW.4). W2's Δ applied, a checkpoint flushed it while W1's
open range held the tail, W1's Δ applied after; a crash. The gate skipped every record at or below the
key's newest on the node: W1's acked Δ, never on the node, was dropped. **Sibling:** a compaction
between the applies folded W2's Δ into a Put at W2's seq; W1's Δ landed beneath it, shadowed — lost live.

**Reds** (`710a2be`, `397f662`; §CG.3): replay size 10 / mtime 10 for 4096 / 1001; the backend's
self-apply cut mtime 1000 for 1001; the compaction's LIVE read size 10 / mtime 10.
**Fixes:** `5202d4b` the exact gate (`holdsSeqOf`); `f9f02de` the apply floor at the freeze cut and
`compactFold`'s retention of inode Δs at or past it; the property test and its failing weakening.

## 3. The shipped setattr's `kill_suidgid` (closed on dev; verified)

`9d9e3ce` (§BK.3/BK.4). xfstests on `f9f02de`: generic/001 013 193 355 683 684 685 **pass**; 673/675
(reflink), 674 (dedupe), 686/687 (finsert/fcollapse) not run; fsck 0 findings both sets.

## 4. The load-only reds

`A MEMBER'S CACHED COPY…` (`~/tmp/cp7b/runs/load-6.log`) and `THE ORDERED LAW'S KILL ROW`
(`load-4.log`) were one panic — `cpu_class.zig:101 finishPass` `index 170, len 13`, two `.stats` renders
racing in the CPU ledger at umount — fixed by `2188b4d` and §CC. **10/10 each** on `f9f02de`, beside
the 24 `yes`.

## Counts (on `f9f02de`)

Unit: `meta.` 1096/1099 (3 skipped); the whole unit step 3111/3277 (166 skipped, 0 failed); `setattr`
47/50, `killpriv` 40/41, `setid` 42/43, `wire` 241/259 (skips by kernel feature or step); rails 32/32;
`zig fmt --check` clean. Smoke ×3 each: the re-join rows (`RE-JOIN`, `REJOIN`), THE REPLAY PROOF, THE KILL
ROWS (a)(b)(c), THE KILL MATRIX, TWO MOUNTS OF ONE SET, the four LIVE MEMBER rows — all green.

## 5. The follow-ups (`fix/meta-owed-followups`; §CG.6–CG.9)

- **(a) The F4 row's precondition.** Cause: any process's sync(2) reaches B as FUSE_SYNCFS and lands the
  custody the row pins; with no intent open at the judge B re-joins. Proof: 25 loaded runs with `.stats`
  probes — the re-join carried `fuse_syncfs` 3 (own 1 + two foreign), every fail-stop 1. Red: the row
  issues the sync after its custody write — `syncfs_custody_inos` 0 → 1. Fix: B arms
  `SQUEEZEFS_TEST_SYNCFS_KEEPS_CUSTODY` (F4 and the negative arm); the legitimate re-join under the dead
  manager has its own row.
- **(b) `extent_grant_conflicts` after the re-join.** Cause: JoinAppender settled the pending GrowRing
  word against the REUSED pair's page; the predecessor's Recovered page still named the segment — freed
  under it, carved into the successor's ring, freed again by the release. Red: the manager unit row (the
  successor's ring extent freed). Fix: settle against `pending.appender_id`'s page; the release settles too.
- **(c) The unmount panic.** Cause: the teardown cancels the times drain's fiber inside `runBarrierLegs`;
  `Group.await` → `Canceled` → `unreachable`. Red: a barrier task cancelled before its join panics at
  `fsync.zig:734`. Fix: the legs' (and the reclaim lanes') join under blocked cancel protection; the
  other 18 sites are tests, protected ring waits or `*Uncancelable` variants.
- **(d) The rollback edge.** Cause: 'escaped' judged by the key's newest seq. Red: A's Δsize frozen under
  B's Δatime, A fails — size 4242 for 10. Fix: `NodeSnapshot.spanUndo` (undo the span over everything
  else; a Δ of its own fields); no pre-image on the apply's path.

**Counts (this branch on dev `b2820d7`):** the whole unit step 3159/3331 (172 skipped, 0 failed); rails
32/32; `zig fmt --check` clean. THE F4 ROW 30× beside 24 `yes` inside one slot command (trap EXIT): 30
poisoned, 0 re-joined, 0 hung — and 30/30 on each of the three earlier bases. Smoke, green (×3 on `74d419c`,
×1 here): the DEAD MANAGER rows (the new re-join row among them), the re-join rows, REJOIN, THE REPLAY PROOF,
THE KILL ROWS (a)(b)(c), THE KILL MATRIX, TWO MOUNTS OF ONE SET, the LIVE MEMBER rows, A MEMBER'S CACHED
COPY…, THE ORDERED LAW'S KILL ROW. xfstests generic/001 013 193 355 683 684 685: 7/7, no daemon alert, fsck
0 findings (a pass on `74d419c` was SIGKILLed whole 4 s in by a concurrent suite's harness; rerun, green).
