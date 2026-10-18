# Rung 10 — the per-inode size epoch (a truncate against a late size raise)

**The symptom.** xfstests generic/112 (fsx, AIO) and generic/075 (fsx): `Size error: expected
0x49b4e0 stat 0x800000` right after a `TRUNCATE DOWN` — the size read back is a 4 MiB block's end
(`flakes.md`, 1 of 3 full passes; generic/075 showed `stat 0x939576` after `truncate 0x1636fb`).
A size the truncate had set was raised again by something that formed its raise BEFORE the
truncate and landed it after.

**The raisers, read in the code.** Three actors raise a file's size from a snapshot taken before
the raise lands: (1) the times drain journaled the claim it snapshotted before its guards —
closed by `482300c` (`min(parked.size, claims[i])`), the only one of the three reachable from a
FUSE workload, since level (1) holds a write's op guard across its whole postlude and a truncate
takes it exclusive; (2) a striped write's postlude parks `max(claimed size, its end)` under
`I{ino}` after its bytes landed — a truncate crossing between the landing and the postlude leaves
the end stale (reachable by any entry that does not hold level (1) across the postlude: the tests'
direct `fs.write`, a future ring host); (3) a write-back unit (a RAM-pressure spill in the worker
queue) carries `min_size = block · bs + app_end` and its block from its formation. The truncate's
settle (`flushIno`) lands or joins every RAM custody before its clip, but the spilled unit sits in
the worker's queue, which no settle drains: it merges after the clip, re-inserts a block at or past
the new EOF and raises the layout size — a later grow reads that block's bytes instead of zeros.
The layout size never reaches `stat` (the attr is the inode record folded with the claim), so (3)
is a data hazard, not the size symptom.

**Design.** `src/data/size_epoch.zig`: per-ino epochs — a 4096-way striped table of monotonic
counters (the shape rows' mix; one acquire load on the write path, zero allocation per op) plus the
EXACT per-ino record of the counter value each ino's last size set wrote, under one spin lock. A
colliding ino's truncate moves the shared row and nothing else; `setSince(ino, seen)` consults the
exact record only when the row moved — a false "stale" would drop a legitimate write's growth, so
the rule is exact, and the common path never takes the lock. Size setters (`setattr` size, `open`
O_TRUNC) bump under their exclusive guards BEFORE their commit (`DataPlane.noteSizeSet`); the
bump is never undone (a failed commit's raisers re-read a size nothing changed). A striped write's
landing stamps the epoch into its outcome after the last piece lands (`write_admit.Outcome.
size_epoch`); the postlude asks `sizeSetSince` under its guard and claims nothing when a set
crossed (`fuse_write_size_claims_dropped`) — the only serialization left is "write, then
truncate". A spill unit captures the epoch at formation (`writeback.Unit.size_epoch` →
`MergeRequest.size_epoch`); a stale unit's merge inserts only the blocks below the ino's CURRENT
size (record + parked claim: a grow since keeps its blocks), raises no size, and reports the rest
`clipped` — the flush returns the mint and releases the ring entry (`Disposition.eof_discarded`,
`writeback_eof_discards`, `layout_map_merge_clipped_blocks`). Never lossy: a byte below EOF is
still published. Callers inside the setter's ordering (uploads the settle joins, overlay drains,
rewrite-epoch saves, promotions) pass null.

The fencing field stays the DLM credential (`FencingTokenExpired` ⇒ re-present the current one
and retry; spec §7.3): a formation-time epoch in it would be re-presented away by every caller's
own retry, so the epoch rides its own field. The `482300c` cap stays: it is the drain's own
correctness (its snapshot predates its guards whatever the epoch says) and costs one `min`.

**Red → green.** `data.size_epoch` (the tie: unchanged ⇒ current; a same-ino set after `seen`
⇒ stale; a set before ⇒ current; a COLLIDING ino's set alone ⇒ current; forget drops the record).
`fuse.handler.kv_fs` "a truncate down that lands BETWEEN a striped write's data landing and its
postlude" (seam `write_hold_pre_guard`): red `expected 4096, found 16896`; green: the attr, the
claim, the stored record and the read's EOF all at 4096, the next write claims its end.
`data.writeback` "a spill unit formed BEFORE a truncate merges after it": red — block 3 re-inserted
past a size inside block 1; green — block 0 (below EOF) lands with `min_size 0`, block 3 is clipped,
its mint returned, both ring entries released. Modules (writeback, write_admit, plane, overlay,
accumulate, write_epoch, kv_fs, economy, fsync, e2e): 231/231; rails green.

**Oracle.** The fsx subset (generic/075, 112, 127; `SQUEEZEFS_FSYNC_PROMOTE_STAGED=on`) on `dev`
`082761b`: 8 runs, no `Size error` (one generic/127 `READ BAD DATA` at 0x27000 after a COPY — a
staged-arm content defect, not this class; results kept at
`/build/tmp/suites-fstests-20261018-124545-2304451`). On the fix: 6 of 6 passed; the three earlier
sightings' op logs (`~/tmp/sqz-075*.fsxops`, `sqz-127.fsxops`) replay clean. The subset never
reproduced the size race on either binary — the race was 1 in 3 full passes before `482300c` and
the FUSE path cannot reach the postlude window; the unit pins are the evidence.
