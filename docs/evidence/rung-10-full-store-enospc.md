# rung 10 — a full data store answers ENOSPC (never EIO), and the trim-window park is a count

**Finding** (`rung-10-adjudication-audit.md` F7; generic/750 and generic/475 on 2026-10-22, kernel
`7.3.0-1.7-mainline-sqz`). Under fsstress on the filled 2 × 8 GiB scratch the daemon fired the
must-stay-0 `alloc_trim_window_parks` (×3 / ×7) and logged `syncfs: a leg failed (InputOutput)` ×235 /
×1,085 and `fsync ino N: a leg failed (InputOutput)` ×79 / ×3,176: the write cache admitted every byte,
the sync point's promotion found no block, and the application got EIO where ENOSPC is the honest word
— the dismount pass FAILED 1,481 residents (750) and the supervisor held a 7 GiB memfd of them (475).
Shown red on `origin/dev f11cd41` with the smoke row grafted: 3,891 × 100 KiB files admitted unsynced
onto a 382 MiB store with `df` still showing 389,120 KiB available, `sync -f` → `Input/output error`,
`syncfs: a leg failed (InputOutput)` in the log; the fill's cap of 6,000 files never met a refusal.

## Law 1 — the class of a trim-window park (`enospc.awaitTrimReturn`, `block_alloc.stats`)

A full store under rewrite holds its free list inside the windows its trim passes claim; an allocation
that meets `StorageFull` there parks for the window's return (R7: a window is supply, never a deficit).
That is the regime's ordinary outcome, so it is a `ShardedCounter` `alloc_trim_window_parks` with the
gauge `alloc_trim_window_parked` (the fence-class ruling of 2026-10-19 applied), not a tripwire. A trim
pass holds its window for about one manners tick (reclaim.zig's pacing), so a park reaching the 1 s
ceiling with the window still open is the device stalled on its discards: `alloc_trim_window_park_walls`,
ONE warning per stall episode on the volume (`trim_wall_warned`, cleared at the close), the park goes on.
No "cannot happen" remains at the site: the caller's predicate is the only gate (asserted) and a lost wake
is the Notify's contract. Red → green: "THE CLASS OF A TRIM-WINDOW PARK" (the park counts, the gauge reads 1
during and 0 after, `invariant_tripwires` unmoved, no error line — before: the tripwire and its line) and
the wall test (a 20 ms ceiling, the window held three ceilings: one park, ≥ 3 walls, the latch cleared).

## Law 2 — the terminal law at the write cache and at the sync point (charter §6.1/§6.2, amended)

*The sync point.* The promotion primitive's declines carry their class: an arm that found no free block
(the own-block mint's `StorageFull`, the pack refill's stop) is `LiveVerdict.no_space` →
`FsyncPromote.kept_no_space` → `error.NoSpaceLeft` from `fsync.zig`'s two points (`fsync_promote_enospc`;
a resource decline keeps EIO and `fsync_promote_refusals`). The bytes stay resident and dirty; the next sync
point retries. A FAILED promotion of a corpse's resident (unlinked, its FORGET's destroy on its way) owes
the point nothing (`LiveVerdict.corpse`, `fsync_promote_corpses`): the syncfs right after the `rm` of what
could not be synced answered ENOSPC. The daemon says it ONCE per full-store episode
(`enospc.FullStoreEpisode` on the plane, the ring-full latch's shape): opened by the first refused
promotion, held by refusals, closed with its tally by a landing a park wall (1 s) after the last refusal
— never one line per sync point or per leg. The packed arm's OQ-1 stop is re-armed at every batch's start
(`PromotionHooks.begin_batch`: an fsync, a syncfs pass, a dismount pass, a drain pass, a recall) — it was
never re-armed, so the arm stayed dead for the mount after the first full-store refill; its per-batch
warning is a debug line now (`pack_refill_storage_full` counts).

*The admission.* `enospc.admitWrite` did not account the ring against the store — the real defect. Every
resident now carries its claim on the store (`staging.StoreClaim`: inline nothing, a packed slot
(`ceilGrain`), past the pack's max slot a whole block; summed as `Pool.claimedBytes`; a recovered record
claims nothing until re-staged). `enospc.storeCoversRing(free, claim, extra)`: a staged write is admitted
only while the set's reachable free bytes cover the residents' claim plus its growth of it
(`DataPlane.writeCacheAdmits` ahead of the staged arm, `fuse_write_staged_store_full_refusals`); a striped
fresh custody only while they cover the claim (`WritePath.ringUncovered` in `write()`'s prelude,
`write_cache_claim_enospc_refusals`); `statfs` subtracts the claim from free, so `df` shows what a write
is admitted against. The claim is the honest estimate between the arms: the sync point keeps the terminal
verdict for the packs' slack (ten 100 KiB tenants leave 24 KiB per 1 MiB block).

## The smoke row (20/20; every run the same numbers)

Two 192 MiB data volumes of 1 MiB blocks, the write cache on: **3,891** × 100 KiB files admitted unsynced,
the 3,892nd refused ENOSPC at `write(2)` (`df` avail 20 KiB) → `sync -f` promotes 3,800 into packs and
answers ENOSPC for the **91** the packs' slack cannot hold (`fsync_promote_enospc` 91, `promotions_enospc`
91, one `data store FULL` warning, 0 error lines, 0 tripwires) → every file's own fsync: the 91 answer
ENOSPC (still one line) and are removed → `sync -f` exits 0 → nine files in ten removed, a quiet wall, a
fresh file lands and the episode closes with its tally → kill -9 → `fsck --offline --scrub` clean →
remount → every survivor byte-equal. Red on dev as above.

Unit: `enospc` 50/50, `block_alloc` 68/68, `fsync` 97/97, `staging` 70/70, `pack` 105/105, `kv_fs` 101/101,
`write_admit` 79/79, `plane` 176/176, `data.` 587/587, `stats` 112/112; `zig build rails` green; `zig fmt
--check` clean. Smoke beside the new row: the ENOSPC fill (the re-use row), the two raced-syncfs rows, the
two data-plane passes, the kill matrix, the two-ring [C4] row (its `df` baseline after the rm is why a
recovered record claims nothing), the ordered/orderly-unmount rows, the write-back timer rows, both
two-daemon matrices, the recall, the replay proof, FSCK AND SCRUB, the jobs matrix, the kill rows (a).

## The xfstests subset

See the report row for `generic/750 475 476 015 027` (015/027 are `[not run]` here: `_scratch_mkfs_sized`
is not supported for `fuse.squeezefs`).

**Seen, not fixed.** `data volume N: fresh supply EXHAUSTED (StorageFull latched …)` is an info line per
re-latch (142 in 19 s under generic/750; the latch clears on every landed allocation and re-arms on the
next refusal) — not an alert, but a candidate for the same once-per-episode shape. A recovered-but-unnamed
ring record (a kill between a promotion's commit and its release) is judged only by a promotion point or
the dismount pass; a mount-time judgement would retire it sooner.
