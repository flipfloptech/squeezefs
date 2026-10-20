# Rung 10 — the staged arm: the promotion keeps the durability law; where its 1.1 k writes/s go

Two items the fsync-law agent left open (`rung-10-staged-fsync-durability.md` "Open"): the
promotion-side hole in the law (release-blocking) and the staged arm's serialization.

## 1. The promotion-side hole (charter §1 item 10; §6.1 amended 2026-10-20)

**The window.** A staged file's bytes are durable in the ring once its fsync returned. A promotion
(the pressure pass, the dismount pass, the fsync lever, the write path's own `promoteToStriped`)
then DMA'd the image to a data block, published the layout naming it (a KV commit the journal's
50 ms cadence makes durable) and tombstoned + punched the ring record at once. Nothing barriered
the DMA: the pack arm never stamped the ino's touched word, so even the lever's own fsync skipped
the device (`data_device_syncs 0` after 8 lever promotions on dev). A durable layout named bytes
sitting in the device's volatile write cache; a cut lost every fsync'd byte the promotion had moved
— and, the second face, a ring punch made durable (another file's ranged sync commits the segment
file's ext4 transaction) before the publish was left the OLD layout naming a hole.

**The fix** (`00ca068`). (a) DUR-1 on the promotion arms: `plane.promotionBarrier` flushes the
volume the image landed on BEFORE the layout publishes — inside `ownBlockStored` (the own-block arm
and the write path's `ownBlock`) and between `pack.prepare` and `commitLocked` (the packed arm) —
through a per-volume `fsync.BarrierCoalescer` on the journal lane's barrier law (a request is
answered by a flush that STARTED after it asked; concurrent promotions on a volume ride one flush;
a write-through volume owes none). (b) The ring record DEPARTS instead of being punched:
`Cell.hold = .departed` (one deferral word with one reason — the cell stays 112 B; a superseded
copy of a departing key departs with it), whole on the segment and live for placement until a
COVER ROUND — `Segment.cover`: the backend's coalesced meta barrier past the publish — lands. A live
promotion runs the round inline after its locks drop; a write-back drain runs one per pass; the
dismount pass runs ONE for all its residents; the filesystem's release face (truncate to zero,
destroy, `promoteToStriped`) spawns it on a task. The departure test models the cut: recovery
after `remove` adopts the synced copy.

**Counted.** `promotion_barriers`, `promotion_barriers_coalesced`, `promotion_barrier_write_through_
skips`, `promotion_cover_rounds_failed`, `writeback_cover_rounds_failed`; `staging_departures_
deferred`, `staging_departed_held{,_bytes}`, `staging_departed_released`, `staging_cover_rounds`.

**Red → green.** `data.staging` "the departure law" (red: the copy tombstoned right after `remove`);
`data.fsync` "the promotion's barrier coalescer" (1 + 16 requests → 2 flushes, 15 coalesced; a
failure reaches its batch alone); the identity-window smoke row pins `promotion_barriers +
write_through_skips = 1`, `departures_deferred = departed_released = cover_rounds = 1`.

**dm-log-writes by hand** (`~/tmp/lw/check.sh`: meta on a plain loop, the data volume under
log-writes — it logs a write only when a FLUSH follows, so a replay to the mark IS the state a cut
leaves —, the ring on the host's filesystem; 64 KiB files + fsync each, kill -9, mark, replay, the
data symlink re-pointed, remount, compare, `fsck --offline`):

| leg | binary | promoted | data flushes before the cut | after the cut |
|---|---|---|---|---|
| lever, 8 files | dev `83d5435` | 8 packed | 0 | **8/8 lost** |
| lever, 8 files | `00ca068` | 8 packed | 8 (one after every tenant DMA, before its publish) | 8/8 byte-exact |
| pressure, 1000 files (`SQUEEZEFS_MEM_BUDGET_MB=512`: a 64 MiB ring) | dev `83d5435` | 415 packed | 0 | **415/415 lost**, the 585 still staged whole |
| pressure, 1000 files | `00ca068` | 415 packed | 415 (`promotion_barriers 415`, `departed_released 415`) | 1000/1000 byte-exact |

Every fsck clean. The cost is one volume flush per promotion batch (the pressure storm's tasks
coalesce; the dismount pass pays one per resident — its residents promote sequentially) and one
coalesced meta barrier per round; neither touches the write path.

## 2. The staged arm's serialization (≈ 1.1 k writes/s whatever the jobs)

**The profile** (`~/tmp/staged-perf`: fio io_uring O_DIRECT QD1, 4 KiB writes round-robined over
64 × 16 KiB files per job — fio creates them at 16 KiB, so every write is a non-extending
overwrite, the re-stage (`restages/op 0.99`); jobs 1/4/16, 20 s rows, the RAM NVMe set, the ring on
btrfs/LUKS/NVMe; `SQUEEZEFS_OP_PROFILE=1`, `perf record -a -g` mid-row). Dev: 1 job 1,044/s at p50
930 µs — `handler_to_backend` 869 µs, the KV commit 49 µs; 16 jobs 583/s at p50 13.4 ms, 9.2 daemon
cores; `stageImage` 57 % of the samples (51 % self), `Segment.get` 26 % self, `sched_yield` 14 %.
**The point: `Segment.lock` held across `findHoleLocked`** — a sort of every live extent plus an
O(n²) first-fit check, per put — over a live set that grew without bound: every re-stage's
superseded copy was HELD (the supersession law) until the key's fsync, which a plain write stream
never issues; every other lane spun on the lock. With an fsync per write the same picture.

**The levers** (red-first, `155d2ec`, `af9bf61`):
1. **A grain bitmap for placement** (one bit per 4 KiB grain; the first clear run from the cursor,
   O(need) where the cursor leads free space, one word pass on a full ring); the `pending` list and
   the sorted scratch — 80 B per cell — are gone (the tables 6.3 → 4.4 % of the ring).
2. **The supersession hold made synced-only**: `Cell.synced`, set by the sync's leader for every
   copy of the key indexed before the sync began — under the segment lock BEFORE the batch is handed
   over, so a re-stage sees the running sync (`SyncCoalescer.covers`) or the mark, never a gap;
   inherited by an in-place append; true for a recovered record. An unsynced superseded copy is
   released at once, as before the fsync law; an unsynced key departs with nothing owed.
3. **A size-neutral overwrite inside a staged image commits nothing** (§L.6 for the staged arm):
   the layout record it published was the one stored (same size, same identity); the write re-stages
   the image and parks its Δ(mtime, ctime) under the shared guard like a striped size-neutral write.
   A growing, a straddling and a KILLPRIV write keep their transaction (`fuse_write_staged_parked`).
4. **Lazy, batched punching**: the per-record `fallocate(PUNCH_HOLE)` + `madvise` on every
   re-stage ran UNDER the segment lock (56 → 150 µs with the jobs on btrfs — the wall after 1–3,
   B1: 11.1 k/s at 1 job, 6.5 k/s at 16). Freed grains are DIRTY (free for placement at once; a
   placement over them clears the bit — nothing to punch); `Segment.sweepDirty` punches whole dirty
   runs, one syscall pair each, under a reservation (the run's grains occupied while the host works),
   run by a plane task when an eighth of the ring is dirty and at every dismount (a cleanly unmounted
   ring still scans empty). `staging_dirty_free_bytes`, `staging_sweep_runs/_punched_bytes`.

**Measured** (dist builds, one session per leg holding the venue, started idle at Tctl ≤ 70 °C —
A2 met another agent's load at its start and still matched A1; `tctl_c` per row in
`/build/perf/staged-perf/*/results.tsv`; legs P1 B1 B2 A1 B3 A2):

| row | A1 / A2 (dev `74e5402`) | B2 / B3 (`af9bf61` / `42d5cae`) | B/A |
|---|---|---|---|
| st4k 1 job | 1,325 / 1,093 /s, p50 717 / 881 µs | **40,979 / 42,000 /s, p50 16 µs**, p99 70 µs | **34×** |
| st4k 4 jobs | 802 / 781 /s, p50 4.6 / 4.8 ms | **74,398 / 70,750 /s, p50 31 µs** | **92×** |
| st4k 16 jobs | 582 / 597 /s, p50 13.3 / 14.1 ms, p99 135 / 123 ms | **44,827 / 46,954 /s, p50 65 µs**, p99 1.6–1.8 ms | **78×** |
| st4k + fsync 1 job | 285 / 283 /s | 1,182 / 1,267 /s (the ring sync 717 µs per fsync: the device) | 4.3× |
| st4k + fsync 16 jobs | 447 / 445 /s | 4,567 / 4,674 /s | 10.4× |

Daemon cores at 16 jobs 9.2 → 4.9 for 78× the writes; every fsck clean; `ring_full 0`.

The write's p50 at 1 job — 930 → 16 µs — is in the striped write's class (17–40 µs). The arm no
longer serializes on its ring: 16 jobs run 77× dev's rate, but below 4 jobs' — the next wall is
the host filesystem under the shared mapping, not the daemon's locks: the 16-job stacks are
`btrfs_page_mkwrite` 10 % (a re-dirtied page's delalloc reservation on every write), the kernel's
queued spinlock 8 %, `memcpy` 12 % (48 KiB moved per 4 KiB write: the image out of the ring, the
rebuild, the image back in), `Segment.get` 11 % and `stageImage` 11 % self (the lock's remaining
contention), `io_uring_enter` 12 %. The target of this wave (≥ 200 k/s at 16 jobs) is not reached.

**The path to parity** (named, not landed): (a) the overwrite IN PLACE for an UNSYNCED resident
record — one 4 KiB copy and a header rewrite instead of a 16 KiB RMW into a fresh slot; it needs a
reader/writer exclusion per record (a reader's lease sees one intact copy today) or a seqlock on
the read side, and keeps the two-copy replace for a synced record (the crash-atomic form); (b) one
copy fewer for the re-stage that stays: `put` from the old lease plus the patch, no heap image
(`residentImage`'s 16 KiB allocation per write goes with it); (c) the ring's record write off the
mapping — an io_uring WRITE on the lane ring, the fiber parked — so the host filesystem's
`page_mkwrite` and the process's `mmap_lock` leave the hot path (the mapping stays the read side);
(d) the extending write's transaction coalesced like the size-neutral one (the staged layout's size
served from the inode's, the promotion sized by the image — fsck's layout/inode size rule first).
