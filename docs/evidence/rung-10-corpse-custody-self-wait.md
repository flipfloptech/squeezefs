# Rung 10 — a deleted file's parked bytes waited for their own destroy (2026-10-30; decision 0003 §CW)

**The report** (handoff 2026-10-29). Smoke "FALLOCATE — EVERY MODE'S LAW" failed on `dev` 2 of 3 with one
`error(squeezefs): data volume 1: StorageFull after parking 1000 ms for visible supply (0 block(s) in sight …) —
refusing; the plane is broken, not slow`. On `90094f4` solo: **5/5 red** at host load 53–160, one line each.

**Cause** (an instrumented run: a stack at the park's start and wall, the gather's events, `.stats` at step 6's end).
Step 6 fills the store with fsync'd 16 MiB files; the one that fails keeps its parked blocks (custody the daemon
holds), and `rm` gathers its corpse. §CR made the gather supply in sight, pending until the destroy pass that takes it
RETURNS. The pass began 2 ms after the gather; its first step settles the corpses' write custody (`destroyCorpses`
(1), the generic/013 order), and the parked block's upload — `settleCustody → flushIno → uploadParkedBlock →
pickDest → allocatePlacedMint → allocateBlockGraceBounded(.visible_supply)` on the cadence's fiber — parked while
`pending` held: on its own pass. The 1 s wall (the line), then a second park on the placed mint's second pick. At step
6's end: `fuse_reclaim_destroys` 0, `fuse_reclaim_gathered` 1, one wall. The row passed when its `kill -9` beat the
wall (0.6 s in the passing run): load decides the verdict, not the defect. No supply was lost and the hole's ENOSPC
was right (the corpse held no published block): the destroy waited on itself, 1 s per parked block and candidate
volume, the latch inert and every other visible-supply mint parked behind the gather meanwhile.

**Bisect.** The row: `1df4bdd` 5/5, `a2a8fae` (§CR fix 1) 5/5, `7698b38` 5/5 — the last on a quiet host (load ≈ 5)
where the kill beats the wall. The red row below at load ≈ 4: `a2a8fae` 2/2 green (0 walls), `7698b38` 2/2 red (3
walls, 3 lines). The regression is `7698b38`.

**Red** (`72c3b6e`). Smoke "A DELETED FILE'S PARKED BYTES NEVER WAIT FOR THEIR OWN DESTROY": a store filled to ENOSPC
with fsync'd one-block files, the failed file's block parked (`write_through_fallbacks` +3), `rm` of it, a new file's
write as the kick; the destroy must land with no wall and no error line. On `dev` 2/2 red: 3 walls, 3 lines. The full
store holds the window; no seam.

**Fix** (`7344864`). The gather OWNS its corpses, gathered or inside a destroy pass (each pass links its batch, on
its drainer's stack, into `kv_fs.passes` under `reclaim_lock` until it returns): `Reclaimer.Upstream.owns`. A
`.visible_supply` mint names its file (`allocatePlacedMint` passes `place.ino` to `allocateBlockGraceBounded`), and
`Reclaimer.pendingFor` (via `SpacePending`'s `for_ino`, `freshSupplyVisibleFor`) leaves an upstream that owns it out.
A corpse's custody mint — the settle's, or the write-back timer's retry the settle would join — refuses at once and
stays parked (the ladder); the destroy lands; a live file's mint still parks on the gather until the pass returns. The
owned check scans the gather and the passes, read only on the full-store park path after the cheaper conjuncts
(queued, processing, held) are all empty.

**Checks.** The red row: destroyed 1, gathered 0, walls 0, kicks 4. FALLOCATE ×10 solo; ×3 with `.stats` at step 6's
end: destroys 1, gathered 0, walls 0 — the destroy lands inside the step, so the kill's timing no longer matters.
Units `data.` + `fuse.handler.kv_fs` 726/726 (new: reclaim "a gather's own corpse is no supply for its own bytes",
kv_fs "the corpse gather OWNS its corpses…"). Smoke ×2 over 19 rows (§CR's lay-down, the full data store's ENOSPC, the
freed block's durability, the unlink storm, the full metadata volume, the punch cut, SETID, generic/347's thin pool,
the owed drain, the merged census, §W.2, the data plane ± the write cache, the streaming overwrite, the kill rows, the
ordered law, mid-tar). `zig fmt --check`, `zig build rails`. xfstests generic/013 074 103 299: 4/4, fsck 0; the
scratch alerts are generic/299's full-store regime (17 parked-gate lines, 1 visible-supply wall — it truncates and
never unlinks, so no corpse is in sight; `dev` in one run of 299 + 103: 3 such walls among 387 alert lines) and
generic/103's dismount FAILED (§CR.7 (a), on `dev` too).

**Seen, not changed.** (a) A corpse's parked block outlives its destroy: the timer retries it and, once supply returns,
mints, DMAs the dead bytes and abandons them at the merge's `NotFound` (pre-§CR too) — discarding a corpse's
not-in-flight parked entries at the settle is an owner's call. (b) A kicked drain under `space_deferred` (a full
metadata heap) leaves the gather pending to the tick, so a live mint can still park its wall (§CR.4; not reproduced).
(c) generic/103's dismount FAILED is not this family: `reserveRange`/`reserveMint` never consult
`enospc.storeCoversRing`, so a reservation can take the block a staged resident's claim counted on.
