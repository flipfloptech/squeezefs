# Rung 10 — the write side's custody tables under a writer storm (0003 §AP)

**1. The storm's two tripwires.** "THE WRITE-BACK TIMER UNDER A WRITER STORM" (16 writers × 12
files of 1 MiB + 1000 B, `SQUEEZEFS_PARKED_BUFFERS=8`) fired the must-stay-0
`overlay_registry_full` on every run: the device overlay registry was sized verbatim by the parked
cap (8), so 16 writers' 1 MiB heads met it full — the install minted a destination, abandoned it,
tripped and declined (8 of 192 heads on kamrui). Its sibling `active_block_table_full` fired at the
same cap whenever W2 page overlays held every table entry without a pool cell: a full checkout held
a fresh cell and found no entry. **Fixed** (§AP): the registry holds
`max(parked_cap, ⌈capBytes(budget) ÷ B⌉)` records (`registryCapacity`: an open record is ≤ one
block of write custody the timer lands, and every store in flight holds one), a fresh install
reserves its record before it mints, and a full registry declines to the accumulation arm
(`overlay_registry_declines`) whose parked gate parks the writer; the table holds the parked cap's
cells plus as many extents (`activeBlockEntries = cap + extentEntryCap`; a first small piece past
the extent cap takes a full buffer, `extent_cap_fallbacks`), so a checkout holding a cell always
finds its entry. Both tripwires stay must-stay-0, now unreachable by construction.

**Red** (`ab3dd98`): unit "a full registry declines a fresh install BEFORE it mints" (a registry of
2, a third install: `overlay_registry_full` fired) and "the active-block table holds the parked
cap's buffers AND the extents beside them" (8 extents at a cap of 8, then a half-block piece:
`active_block_table_full` fired); the smoke row now judges the daemon's must-stay-0 set — red on
kamrui (`overlay_registry_full` ×2). Green: both units, tie tests for both derivations, the row on
kamrui (gate waits 207, cold flushes 207, residency max 1,037 ms, no tripwire).

**2. The row on kamrui: uutils dd.** The row's `gate waits + cold flushes ≥ 1` failed on kamrui
alone (`parked ≤ 1`, gate waits 0; the flakes row read it as a fast device). The cause is the
userland: uutils dd 0.10 (Ubuntu 26.04) `ftruncate`s its output to the bytes written AFTER the last
write (`strace`: `write 1048576`, `write 1000`, `ftruncate(3, 1049576)`) — a size-setting SETATTR,
which settles the file's custody, so no tail ever parked. GNU dd truncates at open. The row writes
with `conv=notrunc` (both skip the truncate): gate waits 274 on dev's product.

**3. The KVDA page-sink rows.** *Pages held* (`data_alloc_bitmap_leaks_released` 1): 2 of 6 red on
kamrui, each with "3 attempts failed at the holder's venue (1 blocks; leak-safe)" — B's free of a
mint its dd abandoned was in flight at A's kill. The rows now kill A only at the matrix row's rest
(`a3677e7`: every shipped block answered, none pending, A's grace ring empty): both rows 12 of 12
on kamrui, no free in flight at any kill. *Fallback*
(`block_untracked_free_refusals` 1, once in a kamrui gate): **not reproduced** — 0 of 36 alone on
kamrui on `83f43dc` (6 beside the pages-held row, 30 alone). Analysis: a block in B's shipper batch
is consumed, unpublished and unanswered — outside the window and the mint's registration — so no
declaration names it, and a successor re-learning from declarations reads it as nobody's (the
executor never screens `req.lease_epoch` either). The obvious fix (register the batch in flight so
`heldBlockRanges` names it) was written and withdrawn: `HolderPlane.declareWindows` re-learns every
CLEAR declared block for the holder's whole lifetime, so a free the LIVE holder answered while the
declaration was in flight would be SET again and leak. The flakes row stays, owned by `storm`.

**Gate.** One full venue gate on kamrui on `d4142ef`: 53/53 steps, 2,946 / 3,028 tests (82 skipped); the storm row there: gate
waits 269, cold flushes 269, timer drains 198, residency max 1,045 ms, after kill -9 `bad=0`.
