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
**0 hangs**, 28 green. Runs 19 and 21 (the F4 row) re-joined in place where the row expects the
fail-stop: B's close-posture custody had cleared before the eviction (§CG.5 (a); the row's precondition is unpinned under load — a test defect, fixed on a follow-up branch with a seam, never a flakes row). Not a
syncfs (85 `sync -f` of B over three runs: custody held); not reproduced beside 16 `yes` — `9435fe3`
15/15 and `7693bab` 15/15 poisoned.

## 2. The replay gate dropped a straggler Δ (fixed here)

**Scenario.** W1 < W2 apply in either order (§AW.4). W2's Δ applied, a checkpoint flushed it while W1's
open range held the tail, W1's Δ applied after; a crash. The gate skipped every record at or below the
key's newest on the node: W1's acked Δ, never on the node, was dropped. **Sibling:** a compaction
between the applies folded W2's Δ into a Put at W2's seq; W1's Δ landed beneath it, shadowed — lost live.

**Reds** (`06ae152`, `cc6ef5e`; §CG.3): replay size 10 / mtime 10 for 4096 / 1001; the backend's
self-apply cut mtime 1000 for 1001; the compaction's LIVE read size 10 / mtime 10.
**Fixes:** `087c3ef` the exact gate (`holdsSeqOf`); `9435fe3` the apply floor at the freeze cut and
`compactFold`'s retention of inode Δs at or past it; the property test and its failing weakening.

## 3. The shipped setattr's `kill_suidgid` (closed on dev; verified)

`9d9e3ce` (§BK.3/BK.4). xfstests on `9435fe3`: generic/001 013 193 355 683 684 685 **pass**; 673/675
(reflink), 674 (dedupe), 686/687 (finsert/fcollapse) not run; fsck 0 findings both sets.

## 4. The load-only reds

`A MEMBER'S CACHED COPY…` (`~/tmp/cp7b/runs/load-6.log`) and `THE ORDERED LAW'S KILL ROW`
(`load-4.log`) were one panic — `cpu_class.zig:101 finishPass` `index 170, len 13`, two `.stats` renders
racing in the CPU ledger at umount — fixed by `2188b4d` and §CC. **10/10 each** on `9435fe3`, beside
the 24 `yes`.

## Counts (on `9435fe3`)

Unit: `meta.` 1096/1099 (3 skipped); the whole unit step 3111/3277 (166 skipped, 0 failed); `setattr`
47/50, `killpriv` 40/41, `setid` 42/43, `wire` 241/259 (skips by kernel feature or step); rails 32/32;
`zig fmt --check` clean. Smoke ×3 each: the re-join rows (`RE-JOIN`, `REJOIN`), THE REPLAY PROOF, THE KILL
ROWS (a)(b)(c), THE KILL MATRIX, TWO MOUNTS OF ONE SET, the four LIVE MEMBER rows — all green.

## Seen, not fixed (§CG.5)

- The F4 row's re-join (above); after one, B's `GrowRing` was refused every cycle and A fired
  `extent_grant_conflicts` ×19 (`~/tmp/meta-owed/dead-19.log`).
- `data/fsync.zig:734` `error.Canceled => unreachable`: the tick fiber, cancelled at the orderly unmount
  inside `drainPendingTimes → barrierData`, panicked generic/013's daemon (the test passed).
- A rollback across a straggler keeps a failed W1's Δ fields (`UndoKey.escaped` judges by seq alone).
