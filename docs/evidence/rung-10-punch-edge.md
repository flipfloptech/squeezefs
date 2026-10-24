# Rung 10 — a punch at a block's own start, and the same-size truncate (2026-10-24; charter §6.6; decision 0003 §BP)

**The finding.** xfstests generic/695 (FIEMAP agent, KVM guest, 3/3): `truncate 12M`, `pwrite -S 0xab 2M 8M`,
`sync`, `fpunch 2M 2M`, `fpunch 8M 2M`, `fsync`, a dm-flakey power cut, remount — `[8M, 10M)` read 0xab. It
looked like an fsync that returned without the second punch durable.

**The mechanism: no durability miss — the second punch did nothing.** On 4 MiB blocks `[8M, 10M)` starts ON
block 2's boundary and ends inside it. `kv_fs.fallocateZero`'s striped arm had three pieces: a head edge iff
`off % bs ≠ 0`, a tail edge iff `upto % bs ≠ 0 ∧ upto/bs > off/bs`, whole blocks `[⌈off/bs⌉, ⌊upto/bs⌋)` —
none matched, so the call answered 0 with the bytes standing, before any fsync or cut (the smoke row shows
`p`/`z` differing at byte 8,388,609 on the live mount). The first punch `[2M, 4M)` is a head edge and worked.
`fzero 8M 2M` had the same hole (ZERO_RANGE's whole-block reservation is empty there too).

**The law (`RangeTiling.of`).** Every byte of the in-bounds range lies in exactly one piece: the head
`[off, min(upto, block end))` iff `off` is unaligned; the whole blocks; the tail `[⌊upto/bs⌋·bs, upto)` iff
`upto` is unaligned and its block is not the head's (`⌊upto/bs⌋ ≥ ⌈off/bs⌉`). What fsync owes a punch is
unchanged: the edge is a write of zeros through the admission (W1 in place or accumulation) that fsync's data
leg lands and barriers; the whole-block frees ride `MapSeam.punch`'s tx under the meta barrier. The power-cut
row proves both after a REAL cut (fsck replayed the journal; no clean-unmount mark reached the device).

**Item 2: the same-size truncate.** `stageTruncate`'s striped arm returned at `size ≥ old size`, so
`falloc -k 0 12m` + `pwrite 0 5m` + `truncate 5m` kept block 2's reservation charged. ext4 truncates at
`ia_size <= oldsize` "even if i_size didn't change to truncate possible preallocated blocks"
(`fs/ext4/inode.c:6219`, 7.3) — generic/092's purpose. Now only a grow returns early; at the same size every
binding at or past `⌈size/bs⌉` (past EOF only reservations can be bound) leaves the map in the truncate's tx
and is freed after it; with nothing past EOF the inline map is not republished. A tree-mapped file re-arms its
sweep cursor at the same size (one sweep chunk tx; files over the 1,364-entry inline cap only).

## Red (on `origin/dev` 5d3e411)

- kv_fs "a PUNCH/ZERO range that STARTS on a block boundary": `expected 4, found 1` zero writes.
- kv_fs "a truncate to the SAME size": `expected 2, found 3` map entries.
- smoke "A PUNCH, A ZERO RANGE AND A SAME-SIZE TRUNCATE SURVIVE A POWER CUT" (a set over dm slices, dm-flakey
  `drop_writes` for the cut): `falloc 12 same-size 12`, then `p differs: char 8388609` and `z differs: char
  8388609` before the cut.
- kv_fs "RangeTiling" (the tie test: every `[off, upto)` over five blocks of 8 and of 1).

## Counts

- The power-cut row **100/100** (looped under the venue's load: a full gate and other agents' smoke runs
  beside it); the three new unit tests **100/100**.
- Smoke rows green beside it: FALLOCATE — EVERY MODE'S LAW, FALLOCATE STRIPS THE SETID BITS, THE KILL MATRIX,
  UNDER THE WRITE-BACK TIMER fsync/fdatasync/syncfs, `sync -f` IS A DURABILITY POINT, THE ORDERED LAW'S KILL
  ROW, O_TRUNC UNDER NO-OPEN / SERVED OPENS, AN ORDERLY UNMOUNT LANDS EVERY ACKED WRITE (both), THE BLOCK-MAP
  TREE (52/52 in one run).
- Unit filters: `kv_fs` 117/117, `fallocate` 51/51, `plane` 183/183, `fsync` 104/104; `zig build rails` 29/29;
  `zig fmt --check` clean.
- xfstests through the harness (binary 956427d): generic/091 and 263 PASSED; 009, 092 and 695 `_notrun`
  ("xfs_io fiemap failed": FIEMAP is not served on the host's kernel) — their sequences are the smoke row's;
  0 failures, 0 alert lines, fsck clean on both sets.

## Seen, not fixed

- **695's layout lines stay red once FIEMAP is served.** Blocks 0 and 2 stay mapped after the punches
  (written zeros at the 4 MiB grain), so FIEMAP reports `[0..24575]: data` where the golden wants holes at
  `[0, 4M)` and `[8M, 12M)`; `stat -f %S` reports 4096 (`STATFS_BSIZE`), so `_require_congruent_file_oplen`
  lets 2 MiB punches through. OWNER-CALL: report the stripe block as the allocation unit (the 2 MiB rows
  `_notrun`, honestly), or free a block a punch leaves with no written byte (needs per-block coverage).
- **Suspected, not verified: a released block is reallocatable before its releasing record is durable.**
  `afterCommit` → `freeBindings` → the reclaim ladder runs right after the RAM-apply ack (§BB); nothing in
  `block_alloc`/`reclaim` gates `finishFree` on the lane's landing. A whole-block write-through that mints the
  block DMAs at once, so a power cut inside the ≤ 50 ms flush window could leave the OLD owner's durable map
  naming bytes the new owner wrote (charter §1.10 "never another file's bytes"; ext4 holds freed blocks busy
  until the commit). Every release (truncate, unlink, punch, CoW displacement), not this branch's: a
  dm-flakey red row is owed by the reclaim's owner.
- A same-size truncate of a tree-mapped file (> 1,364 blocks) runs one sweep chunk tx even with nothing past
  EOF (no cheap "anything bound past k" probe on the Data face).
- `tests/costs/` has no rows for the four new tests (well under the stale-table rail's 1/10).
