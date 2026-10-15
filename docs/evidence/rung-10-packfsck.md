# rung 10 — packfsck: C8's exemptions on the block's own volume; the refill's OOM leak

Both defects were found by reading during leak67 (`rung-10-leak67.md`, "Found by reading").

**1. An fsck false negative (C8).** `verifyRefs` judges ONE volume, but the runner's C8 exemption
faces took only a block index. `isOpenPackAny` asked every volume's open-pack face for the index, and
`isInflightAny` asked every volume's in-flight registry. So an open pack or an in-flight owner on
volume B at index N also exempted volume A's block N. A real drift on A's block N was hidden whenever
B happened to hold that index. Both faces were used twice: by the first pass (`C8Exempt`) and by the
confirm ladder's fresh re-check (`Pass.exemptions` → `freshC8`). The mount-time C8 (`MountExempt`)
already matched open packs by `(vol, base)` through `Packer.isOpenBlock`, but its in-flight face also
asked every volume.

*Fix.* Both runner sites now judge through ONE `C8Exempt{pass, vol_tag}`. Its open-pack face asks
the host's pack face for `(vol_tag, idx)`. On a mounted set that face is `Packer.isOpenBlock`
(`runner.isOpenPack`). Its in-flight face asks that volume's allocator, bounds-checked.
`Pass.exemptions`, `isOpenPackAny` and `isInflightAny` are deleted. `MountExempt.isInflight` now asks
only its own volume, with the same capacity bound (an out-of-range durable index would otherwise
index past `cells`). The contract is unchanged: the exemptions were always per volume
(`C8DurableRefDrift{vol, offset}`), and `ops_fsck.md` now says so. That is why there is no 0003 entry.

**2. A leak on allocation failure (pack refill).** `RefillCtx.compute` minted the pack block
(`allocate_placed`) and then called `gpa.create(OpenPack)`. When the create failed (OOM), the error
propagated with the mint still at refcount 1, referenced by nothing: a C2 leak until the next fsck
repair. *Fix.* The struct is created before the mint, and a failed mint destroys it. An OOM now
leaves the allocator untouched and reaches `reserve`'s caller as `OutOfMemory`. A retry then opens
the pack.

*Sweep* (`src/data`, `src/ops/fsck`: every mint or reserve followed by a fallible allocation). No
other product site has the shape. `device_overlay` / `writeback` / `accumulate` arm a
`MintedBlockGuard` and abandon on every exit. `ownBlockStored` uses `errdefer`. `packAllocatePlaced`
and `repair.c2LostVerify` allocate nothing after their mint. `opened.append`'s failure in the refill
already abandoned. `write_testing.Fixture.publishBlock` (a test fixture) is left alone: an OOM there
fails its test.

**Red → green.**

| Check | Before the fix (`525c1af`) | After (`e9ed8e8`) |
|---|---|---|
| unit "C8's exemptions match the block's OWN volume …" (B open pack at 5, then B in-flight at 5; A's block 5 leaked + a stray record) | findings `[C2] vol-…10 block 5 leaked` only — the C8 drift was suppressed | `{C2, C8}` in both legs |
| unit "a refill whose OpenPack allocation fails mints nothing …" (`FailingAllocator`, fail at the OpenPack) | `refcounts.count()` expected 0, found 1 | 0; the retry opens the pack (1) |
| kamrui unit filters `fsck pack mover defrag C8` | — | 7/7 steps, 186/213 passed (27 skipped: other-step smoke) |
| kamrui smoke "the data plane WITH a cache path" | — | green; 7 files packed online, `fsck --online` 0 findings after rm |

**Gate** (kamrui, `-Dfresh=true`, on `e9ed8e8`): `53/53 steps succeeded; 2981/3064 tests passed (83 skipped)`.
