# Rung 10 — the read path's lifetime law (2026-10-23; decision 0003 §BI; charter §6.3 amended)

**The symptom.** The first full xfstests run after fallocate landed (`f49ef90`) stopped at generic/095:
fio's 512 B direct reads got `Input/output error` ×4; the scratch daemon fired the must-stay-0
`read_settle_lost_serialized` once and logged `read of ino … block 0: did not settle after 4
serialized settle attempts` ×4 (`read_path.zig:168`). 095 had passed on 2026-10-22 (`d848fb5`).

**The confirmation.** (a) By hand (fresh 2 × 1 GiB set, the write cache on), 095's fio job
(`size=1m`, 9 job types × 5, iodepth 16: buffered / aio-dio / posixaio / splice / mmap writers,
512 B and 1 KiB readers) with fio's default `fallocate=native`: 15 `io_u error`s, the tripwire,
5 "did not settle" lines. (b) The same job with `fallocate=none`: 0 errors, 0 alerts — the 1 MiB
files stay STAGED (the write cache, RAM, no device word) as they did before fallocate existed. (b2)
`fallocate=none` on a cache-less mount (`SQUEEZEFS_WRITE_CACHE_PCT=0`: the files striped from their
first write, NO reservation anywhere): the same EIO and tripwire. So the reservation's flip is not the
mechanism — the defect is the read path's, latent for every striped block under a sustained in-place
write storm; fallocate exposed it by making a 1 MiB file striped (the reservation promotes it).

**The mechanism.** The block's incarnation word is a seqlock (`gen << 1 | stable`): an in-place write
(W1, the in-place drains, a reservation's landing) retires it, DMAs, republishes it under a new
generation. A read snapshots the word before its DMA (null if unstable) and requires the same word
after. That is the TIERS' law — a cached copy must be of one generation — but every serve was judged
by it, including the ones that deposit nothing: the ranged window, the escalated device-true leg, the
settle. Under 095's storm the word is unstable more often than stable, so a 512 B read lost its ranged
ladder (8 attempts, 15 ms of backoff), its whole-block walk (8) and the settle (4) and answered EIO
for a block whose lifetime never moved. The settle's stripe, moreover, was a `MetaStripes` table of the
read path's own (`.settle = .{}`), serializing settling readers against each other and no publish — the
tripwire's premise held only by the settle's rarity.

**The law chosen** (over "every in-place write takes the read's stripe"): a device-true serve is valid
when the block's LIFETIME read before its DMA stands after it — the allocator cell allocated under the
same stamp (`ReadPath.lifetime`, `deviceServeValid`) — and the map still binds the key; an untracked key
(another member's lifetime) keeps the seqlock law. Its bytes are the device's at the DMA's instant,
torn at the device's sector grain against a concurrent in-place write of the SAME file at most — the
O_DIRECT law ext4 and XFS give a direct read racing a direct overwrite — never another lifetime's (a
free + re-mint carries a new stamp: a loss). A deposit still waits for a stable generation. Why this
and not the lock: W1's whole value is zero journal, zero lock on the hot 4k write path (354–397 k IOPS);
a stripe acquire per 4 KiB patch would convoy every writer of a file behind the read settle's FIFO,
and the guarantee it would buy (no sector-torn concurrent direct read) is one POSIX does not give. The
hot read path's cost is unchanged: the view's seqlock and one more atomic load of the cell's stamp on
the device-true legs. The settle now holds the plane's (3.5) stripes (`plane.locks.stripes`, passed at
`ReadPath.init`), so a loss under it is a true anomaly.

**Red first.** `data.read_path` "the lifetime law": a hook retires and republishes the word under EVERY
DMA — on `origin/dev` the test dies at `getBlockSettled` with the tripwire and "did not settle after 4
serialized settle attempts" (the 095 shape exactly); with the law the 512 B direct read serves on its
first window (nothing deposited, no rebind), the whole-block read serves through the escalated
device-true leg (no settle, no tripwire, nothing in the hot tier), an unstable word at the resolve is
read all the same, and a free + re-mint under the read stays a loss. The fixture's `bind` now stamps
its mappings with the cell's real lifetime stamp, as a minted binding is.

**The fix** — `9048313` fix(data): the lifetime law for device-true serves; the settle on the plane's
stripes.

**Counts.** By hand after the fix: (a) 0 errors 0 alerts; (b2) 0 errors 0 alerts. The new test
100/100 under the venue's load. Unit filters green: `data.read_path` (64), `data.accumulate`,
`data.layout`, `data.plane`, `data.warm_ladder`, `data.write_admit`, `ipc.host`; rails; fmt. Smoke
rows and the xfstests subset: RESULTS BELOW.
