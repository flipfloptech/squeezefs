# Rung 10 — a late shipped free meets the successor's derivation (0003 §AQ)

**The defect.** The must-stay-0 `block_untracked_free_refusals` fired once in a kamrui gate
(`4243eee`) in "THE KVDA PAGE SINK's fallback" (`rung-10-storm-custody.md` §3; 0 of 36 alone since).
A writer's terminal free sits in its shipper's batch — consumed, unpublished as a free, unanswered —
or on the wire across the holder's death. It is in neither the grant window nor the in-flight
registrations, so no renewal declaration names it; the same-node successor's derivation (refs census,
the predecessor's pages — stale in the fallback —, the kept window, the declarations) reads it
nobody's (bit CLEAR). The batch then flushes to the successor, whose only answer for a free of a block
it never tracked was row 6, the double-release refusal. `4243eee` predates the rows' at-rest kill
(`d8dfc2e`), so its kill could meet such a batch. The pages-held twin: a deferred candidate (carried
by the pages, declared by no one) that a late free cleared and the successor re-carved was RELEASED by
the sole writer's exact census — a live grant's bit cleared.

**The law (§AQ).** AQ.1 the holder's bitmap records, under its ordering lock, which bits its term
moved since the arm sealed the derivation (`DataBitmap.moved`, `sealDerivation`). AQ.2 a holder that
succeeded a predecessor (`term > 1 ∨ already`) answers a shipped free of an untracked, unreferenced
block whose bit is CLEAR (or trim-windowed) and unmoved `Freed` with no bit flipped
(`free_derivation_settled`); the settle marks the block moved. A CLEAR bit the term moved stays row 6;
the volume's first holder has no probe. AQ.3 a declaration never re-learns a block the term cleared
(`block_grant_stale_declared_blocks` — the reason the naive fix, declaring the batch in flight, was
unsafe) and the exact census never releases a candidate the term moved
(`data_alloc_bitmap_leaks_settled`). AQ.4 records why a pending-free declaration class and a
`lease_epoch` screen were rejected (the flush can precede the first declaration; this wave's writer
knows no successor term). 0002 row 541.

**Red** (`374b823`, macOS + kamrui units): the TwoWriters loopback — B frees a block, the batch held
across its declaration (`heldBlockRanges` names it nowhere) and A's death; the successor arms over the
census alone and re-learns B's window; the flush ⇒ `block_untracked_free_refusals` "expected 0, found
1". Twin: candidate 40 freed late and re-carved ⇒ `released` 1. **Green** (`31e3c93`): settled 1,
tripwire 0, bit and population unmoved, a word built before the answer re-learns nothing, a re-shipped
free of a block the term freed still refused (row 6); the twin `released 0, settled 1`; a stale word
re-learns neither a returned run nor a freed block. kamrui: 123 targeted unit tests (sym_planes,
plane_arm, wire_plane) green.

**Loops** (kamrui on kernel 0029, both KVDA rows, `SQZ_VENUE_SLOT=1`, six venue calls of five):
30 / 30 iterations green — 60 row runs, every one with both shas equal from A, the must-stay-0 set 0
on both daemons (after the remount, at rest, after B's leave), `fsck --offline --scrub` clean; the
pages-held row's online census (whole + two shards merged ≡ whole) clean 30 / 30. The rows kill at
rest, so `free_derivation_settled` read 0 in every run: the late free itself is the unit row's.

**Gate.** One full venue gate on kamrui (kernel 0029) on the branch tip before the rebase
(`c0d0ce9`): 50 / 53 steps, 2,964 / 3,048 tests (83 skipped), 1 red — the literal rail: two `64`s in
the moved-bit helpers undocumented on their lines; documented (`// 64 bits per word`) in the fix
commit, rails 28 / 28. Every unit shard and smoke shard green.

**Owed (seen beside this work).** (1) The fallback successor's live bitmap lacks the blocks a live
writer published before the kill that the arm's census could not see (its trees uncheckpointed) and no
declaration names: the KVDA fallback row's online census read 50 referenced-but-clear (C2/C6 LOSS,
plus one set-but-unreferenced block per volume) after B's leave; the offline fsck re-derives them and
the carve frontier sits above them, but nothing re-learns them live — the row judges the online census
with the pages only. (2) The executor's dedup window keys `(lease_epoch, request_id)` with the request
id = the frame's correlation (`dialer.correlationOf`: a mux cell's generation and index, the same
small values on every writer's dialer) and a lease epoch every writer sends as 1: two writers'
batches with equal correlations inside the 256-entry window collide — the second gets the first's
verdicts, nothing executed, and a verdict count unequal to its batch meets the shipper's two-slice
`for`.
