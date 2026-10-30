# Rung 10 — a reservation took the block the write cache's resident needed: generic/103's dismount FAILED (2026-10-30; decision 0003 §CZ)

**The report** (§CR.7 (a), §CW.5 (c)). xfstests generic/103 passes its golden output; its scratch daemon logs
`data store FULL: the promotion of ino … (65536 B resident) found no free block on any data volume (0 B free in
sight, 81920 B resident in the write cache)` and `dismount promotion: 0 staged file(s) (0 B) promoted, 1 FAILED`.
Reproduced on dev `72f125f` (`FSTESTS_TESTS=generic/103`).

**The counters** (`~/tmp/g103/manual.sh`: the test's sequence on the suite's set shape — 1 GiB meta, 2 × 8 GiB
data of 4 MiB blocks, `--supervise` — with `.stats` and `df -B1` after each step). After `pwrite 0 64k` (one
resident, never synced): `df` used 65,536 B (the resident's packed-slot claim), available 4,092 blocks − 64 KiB.
`_consume_freesp` then asks `falloc 0 16760256k` (4,092 × 4,096 − 576 KiB → 4,092 blocks): `fallocate_blocks_reserved`
= `alloc_fresh_mints` = 4,092, both volumes 2,046/2,046 used, `write_cache_claim_enospc_refusals` 0, the resident
still staged (81,920 B). The dismount pass then has no block for the resident's pack.

**Cause.** Two halves. (1) `WritePath.reserveRange`/`reserveMint` never read the write cache's claim on the store
(`enospc.storeCoversRing`): the allocator's verdict was the reservation's only one, so it took the block the
resident's promotion counted on. (2) `df` subtracted the claim in BYTES: a block-grain request for `df`'s
available (generic/103's, any "fill to `df`" test's) needed every free block, so the law alone would have turned
the dismount FAILED into a `falloc` ENOSPC in the test's output. Every fallocate smoke row mounted foreground (no
write cache), which is why §BH and §CR never met it.

**Red** (`f702867`). Smoke "A RESERVATION LEAVES THE WRITE CACHE ROOM TO LAND": a supervised 2 × 48-block set
(4 MiB), a 64 KiB file written and never synced, generic/103's `falloc` of `df`'s available less 512 KiB, a `falloc`
of one block more, the orderly unmount, the remount's `cmp`. Dev: available 385,810,432 B (92 blocks − 64 KiB), the
`falloc` silent and the store's free blocks 0 — red at the law's first observable.

**Fix** (`605e0e0`). Each fresh block of `reserveRange` is admitted only while `storeCoversRing(free, claim,
block)`; else ENOSPC, counted `fallocate_claim_enospc_refusals`, the chunk's mints returned (the reservation's
existing refusal shape). While supply the free count does not hold is in sight (`Valve.pending`: the reclaim queue,
a delete's gathered corpses, a held free) the check runs one valve pass a slice up to the mint's 1 s wall (§CR.4's
law kept); nothing in sight refuses at once. An empty ring costs one atomic load. `statfs` shows the free less the
claim rounded up to the block (`enospc.freeBesideRing`, tie-tested against `storeCoversRing` for every `k ≥ 1`):
a reservation of `df`'s available fits beside the residents; no residents, nothing changes.

**Checks** (the fix's head). The red row ×10: available 381,681,664 B (91 blocks), free 1, one block more
ENOSPC by the claim, the dismount `1 promoted, 0 failed`, the bytes whole, fsck clean — 10/10. FALLOCATE — EVERY
MODE'S LAW ×5: 5/5. Smoke ×2 over the related rows (§CR's lay-down, §CW's parked bytes, the full data store's
ENOSPC, the freed block's durability hold, the unlink storm, the data plane ± the write cache, the kill matrix, the
kill between a promotion's commit and its ring release, the orderly unmount ± the write cache, the punch power cut,
SETID, the full metadata volume, the handover ack, two mounts on one host, §W.2, the merged census, both mid-tar
kills, the ordered law, the streaming overwrite, generic/347's refused DMAs, the two syncfs rows, the new row):
26 rows, both passes green (66/66 tests each). Units `data.` + `fuse.handler.kv_fs` 731/731 (`enospc`, `staging` among them); `zig fmt --check`; `zig build
rails` 32/32. xfstests generic/013 074 103 299: 4/4, fsck 0 on both sets; generic/103's daemon `dismount promotion:
1 staged file(s) (69632 B) promoted, 0 failed`, 0 alert lines; the scratch's 21 alert lines are generic/299's
full-store regime (20 parked-gate walls, 1 `transport_lease_overlong`; §CR.7 (b)/(c), on dev too).

**The other mint paths** (§CZ.6). Judged at the write's prelude (`free ≥ claim`): the overlay's fresh destination and
every parked buffer's landing (acked custody, never re-judged). The ring's own landing, never gated: the pack refill
and the promotion's own-block arm. Not judged: `promoteToStriped` (an inline file turning striped takes a block) and
the jobs' movers (copy-then-free, one block per move in flight).

**Seen, not changed** (§CZ.9). The prelude's `free ≥ claim` counts no growth of the write's own (a striped write can
take the claim's block, bounded by the writes in flight); a joiner judges against its own view of the free (a stale
grant population can refuse a reservation the holder could serve, as its staged writes already can).
