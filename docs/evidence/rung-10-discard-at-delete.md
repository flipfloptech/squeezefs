# Rung 10 — a deleted file's parked bytes are discarded at its delete, never uploaded after it (2026-11-02; decision 0003 §DC)

**The ruling** (the owner, 2026-11-02): a deleted file's parked data is DISCARDED at the delete — never uploaded
after it. §CW.5 (a) had a corpse's parked block outlive its destroy: the write-back timer retried it and, once
supply returned, minted a block, DMA'd the dead bytes and dropped them at the merge's `NotFound`.

**The delete** is the final FORGET of an inode at nlink 0 (`kv_fs.forget`: the open table's `evicted` and the
try-only read's `.corpse`). The kernel forgets an inode only at its eviction — for an nlink-0 inode its last
`iput`, every fd, mmap and cwd gone — so the FORGET is the first instant the daemon can prove no reader or writer
reaches the bytes (no nodeid in the kernel, `vfs_link` refuses `i_nlink == 0`, an export handle answers ENOENT,
the interposer and the SDK hold a real kernel fd). The unlink is not the delete (an open fd still reads and
writes); a deferred judgement is proven by the destroy's blocking re-read.

**Red** (two rows, each committed alone; on the red tree = dev `baf1fcc` + the rows, Linux 7.3.1):
- A DELETED FILE'S PARKED BYTES ARE DISCARDED AT ITS DELETE, NEVER UPLOADED AFTER IT (`473a8c9`): §CW's fill to
  ENOSPC (92 one-block files, no write cache), the failed file's block parked, `rm`, its destroy at the tick,
  every other file removed. Dev: one parked buffer after the destroy; supply back (92 free blocks), **1 block
  minted and 1 upload dropped at the merge's `NotFound`** (`expected 92, found 93` mints).
- A DELETED FILE'S WRITE-CACHE RESIDENT LEAVES WITH ITS CLAIM, NEVER PROMOTED (`8e6d253`): a supervised set whose
  write cache is the 64 MiB floor (`WRITE_CACHE_PCT=1`, a 4,480 MiB budget: four shards); the 64 KiB `r` first
  (its shard's oldest), 468 younger residents with nothing evicted, a probe's delete aligning with a reclaim tick,
  `rm r`, 468 more at once. Dev: 344 evictions before `r`'s destroy, and **the destroy freed 1 binding** — the LRU
  drain had packed the corpse into a slot (`expected 0, found 1`). Its first form used a syncfs (dev: `1
  promoted`, patched 7.3); Linux 7.3 sends no FUSE_SYNCFS (§DB), so it now takes the LRU drain.

**Fix** (`24d1783`; §DC.3–DC.6). `accumulate.Deleted` marks the ino at its FORGET (`DataPlane.discardAtDelete`):
every parked block whose guard is free is discarded there (try-locks: the drain worker never parks); every landing
point — the upload before its mint, the in-place drains and the escalation, the fold — discards a marked ino's
entry under its guard instead of landing it, so the timer, syncfs, the gate's cold flush and the unmount land
nothing of it. The promotion primitive declines a marked ino's resident and releases it. The destroy's settle
(`discardAtDestroy`) discards what is left (never joining an in-flight upload: its publish runs under the same
guard), releases the resident with its claim, lets the overlay records and the epoch (bytes already on the
device) publish so the plan names their blocks, then clears the mark. `.stats`:
`discard_at_delete_{blocks,bytes,residents,resident_bytes,mark_refusals}`, gauge `discard_at_delete_marks`.
With nothing minted for a corpse inside its destroy, §CW's ownership exception (`Upstream.owns`, `pendingFor`,
`freshSupplyVisibleFor`, the mint's `for_ino`, `kv_fs.passes`) was dead and is deleted (`reclaim.zig`,
`block_alloc.zig` == `7344864^`); §CW's row still passes (walls 0) and now asserts the discard.

**Checks.** Final code under Linux 7.3.1: the reds ×3 (3/3: 0 minted, 0 `NotFound` uploads, the block's
4,194,304 B discarded at the delete; 260–352 evictions before `r`'s destroy, which freed nothing,
`discard_at_delete_residents` 2), FALLOCATE ×2, AN UNLINKED FILE STILL OPEN READS AND WRITES ITS BYTES UNTIL ITS
CLOSE (both files byte-exact through their descriptors after the unlink, nothing discarded while open, both
destroyed after the closes) and §CW's row; units `data.` + `fuse.handler.kv_fs` 732/732; `zig fmt --check`;
`zig build rails` 32/32. Before the rebase (patched 7.3, the same code): the reds ×10, FALLOCATE ×5, 29 related
rows over 5 runs (all green but the row below), xfstests generic/013 074 103 299 4/4 with fsck 0 (the scratch's 18
alert lines generic/299's full-store regime).

**Seen, not changed** (§DC.9). A deleted file's overlay record still seeds its gaps at the destroy's settle; the
proof is this kernel's (a joined member's open fd and a cross-owner link of a corpse are not consulted); the
resident's claim lasts until the destroy; on the patched 7.3, A NO-OPEN CLOSE IS THE WRITE-BACK DRAIN failed in
the 29-row group — dev 1 of 4 runs, this branch 5 of 5, this branch plus one print in that row 0 of 3, alone 5/5 green: its no-open arm
reads `meta_kv_times_echo_pending` at once while the create of `f` parked the root's times (`parkParentTimes`) for
the times drain's next tick, which the row never waits for, so its verdict is the script's phase against that tick
(§CO's law; the row deletes nothing — no path of this change runs in its daemon).
