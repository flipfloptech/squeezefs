# Rung 10 — a joiner's claim carries the holder's size epoch; ring appends across members (2026-10-24)

**1. A truncate racing a joiner's claim.** A foreign-slot write publishes its bytes, then ships its size as a
claim the holder raises to `max(size, claim)` (the claim law). A holder truncate between the two clipped the
write's block, then the claim regrew the file over it: `own/f` (6 MiB) truncated to 1 MiB on A while B's
4 KiB write at 5 MiB sat between its publish and its claim (`SQUEEZEFS_TEST_FOREIGN_CLAIM_HOLD`) read back
5,246,976 B on both mounts — the clipped block as zeros, A's truncated tail as data.

**The law (charter §5.5 amended): THE CLAIM'S EPOCH.** The holder's grant page carries its size epoch of
the object, read BEFORE the records (a size set between them only makes a claim look stale); the joiner's
write reads it off its token before the size its base comes from, and its claim carries it. The holder
refuses a claim made under an older epoch than a size set it landed since (`record_size_claims_refused`) and
runs its own size path at its current size, clipping any block the late publish bound past the cut — the
truncate wins ("write, then truncate"); a claim without an epoch is never judged.

**Frames changed** (decoders total; no compatibility owed, decision 0002): `verbs.GrantPageHead` gains
`size_epoch: u64` after `reserved2` (the holder's epoch PLUS ONE — a fresh set's epochs start at 0; 0 =
none); `verbs.SetattrWire` gains `claim_epoch: u64` after `reserved` (the same biased value; the head screen
refuses a non-zero `claim_epoch` without `SETATTR_SIZE_CLAIM`), so `RecordShipHead` grows by 8 B. Readers:
`token_cache.Entry.size_epoch`; the seam `tokens.table.PrepareSeam.size_epoch`; the judge
`record_ship.TruncateFace.stale_claim` (`KvFs.servedStaleClaim`).

**2. Interposer (ring) appends into another member's file.** Re-checked on `origin/dev` 365dfe2: GREEN — the
IPC host's write handoff enters `kv_fs.write` (`ipc/host/service.zig` `writeHandoff`), so the claim law and
register-then-check govern the ring's extending writes; an `O_APPEND` fd never rides the ring (the shim's
screen refuses it to the kernel). The guard row: shim smoke "RING APPENDS ACROSS TWO MEMBERS LOSE NOTHING".

| red test | before | branch |
|---|---|---|
| smoke A HOLDER'S TRUNCATE BETWEEN A JOINER'S PUBLISH AND ITS CLAIM WINS (the epoch carried, not judged) | 5,246,976 B on both mounts, content differs | 1 MiB, byte-exact; zeros past the cut after a grow; fsck 0 |
| unit two_holders "THE CLAIM'S EPOCH" | — | stale refused + clipped; current raises; none unjudged; the wire screen |
| shim smoke RING APPENDS ACROSS TWO MEMBERS (guard) | green on 365dfe2 | green |

Under load (beside a full xfstests `-g auto` on the venue): three concurrent loops × 7 = 21/21 runs, each the claim-epoch row and the whole shim battery (the ring guard included); the cross-owner and two-mount rows with the foreign-write, alternating-append and resolve-fault rows 68/68; unit filters 722/722; rails 32/32.
