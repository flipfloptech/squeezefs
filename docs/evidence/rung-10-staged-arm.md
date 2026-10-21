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

## 3. Wave 2 — the in-place arm (`perf/staged-arm-2`; A = dev `a43bda8`)

The branch was measured on dev `329a3c9` and rebased onto `a43bda8` (kernel patch files only) at
the end; the shas below are the rebased ones (the perf logs stamp the same trees as
`93e448c`…`904a24a`).

**Re-profile on the head** (A1w2, dev `329a3c9`, the same rows): 1 job 43.7 k/s, 4 jobs 74.2 k/s,
16 jobs 48.5 k/s at p50 64 µs — the wave-1 numbers, now dev's. The 16-job stacks: `stageImage`
28.7 % (10.6 self: the lock), `residentImage` 15.6 %, `btrfs_page_mkwrite` 10.8 %, `Segment.get`
10.9 % self, `sweepDirty` 5.7 %, `asm_sysvec_call_function` 5.8 % (the punch's TLB shootdowns),
`memcpy` 12.6 %, the KV lookups (`readInodeOpt`, `layoutKindView`, `readLayout`) ≈ 20 %.

**The levers** (red-first; the 16-job rate after each, legs alone on the venue):
1. **The in-place overwrite** (`f0bbc8d`; 48 → 222 k/s): a write inside ONE grain of an UNSYNCED
   resident record lands in the record's own grain — `Segment.overwriteInPlace`: the header with
   the new checksum (over the old grains plus the new one) into the slot that is NOT current, then
   the grain through `Backing.write` → `SqzIo.writeAt` (IORING_OP_WRITE on the fiber's lane ring:
   one page's copy in the kernel, one atom against the daemon's death; no page fault, no
   `mmap_lock` — lever (a) for the grain), then the RAM flip. A kill between the header and the
   grain recovers the OLD record (the current slot still names its bytes), after it the NEW;
   readers see a grain old or new, never torn (`Cell.seq`, the record's seqlock; `Lease.copyRange`
   on the read path — 64 spins, then a fiber park bounded by 200 µs: `579cebe`, after a thread
   yield wedged a reader and a writer sharing one lane in `killcheck.sh` run 2). A synced record, a
   record a running sync covers (registered before the probe; the sync leader parks until the
   writes in flight drain), a span across two grains, a write past the value decline to the
   two-copy re-stage — the durability law is untouched (synced bytes keep the two-copy form). No
   new slot, no 48 KiB of copies, no superseded cell, nothing dirty, nothing to punch, no mkwrite.
2. **The fast path reads nothing** (`1654331`; → 268 k/s): `KvFs.write` asks the plane for the
   staged identity and the size readers see (`DataPlane.stagedPoint`, off the read view) and lands
   the grain with the stamp parked — no layout record copied and decoded, no inode record read.
3. **The staged point cache** (`0e007af`; → 281 k/s): the view's 4 × 64 slots thrash under a
   thousand hot files (16 jobs: 5.3 M misses to 18 k hits), so the point keeps its own per-ino line
   under the view's validation law (witness generation + slot-move epoch read before the lookups, a
   seqlock per entry, every word an acquire load): 4,096 direct-mapped entries, 256 KiB. The fast
   path asks the CACHE alone (`6332ece`): its first form went through `resolveAt` on a miss, and
   the striped pair paid for it (rr4k −6 %, rw4k −13 %: w2-striped-B); a miss now falls through at
   two atomic loads and the slow path warms the point.
4. **The times drain under a thousand re-parking files** (`1b3d2fc`, `bf60b53`, `6398c52`;
   → 281–290 k/s, the journal lane's CPU 10.5 → 2.0 s per 20 s row): the apply's witness set names
   256 inos (was 32: every larger apply touched EVERY stripe, staling every point and view twenty
   times a second — 63 % of the point lookups missed); the drain cap 512 → 4,096 (at 512 every
   second park woke the drain ahead of its tick: 100 k inode records a second journaled for
   µs-grade time polish — the count bound on a kill's time-polish loss is now 4,096 inos within the
   50 ms tick); the tick drains its population ONCE (`snapshotParked`; the batch-at-a-time loop
   re-snapshotted from the first stripe until a batch came back short and never ended: 1.9–2.1 M
   records in 20 s, now 28 k). On the final row: hits 3.98 M, misses 1.65 M (29 %); in-place
   overwrites 5.62 M of 5.63 M staged writes; read-view fills 1.08 M.

Named, not landed: put-from-lease (b) for the two-copy re-stage that remains (a growing,
straddling or synced record — under an fsync-per-write stream its cost is the ring sync, 0.7–1.1 ms,
and the row did not move); the extending write's transaction coalesced (d) (the staged layout's
size would lag the inode's: the promotion's `prev.size`, truncate's clip and fsck's size rule must
read the inode first); per-stripe placement locks (e) (`Segment.lock` left the profile with the
in-place arm: no placement on the hot path).

**Measured** (dist builds, each leg a session alone on the venue behind the quiet gate — no
benchmark on the host, Tctl ≤ 70 °C at the start — every session started at 42–57 °C —, and fewer
than 3 BUSY cores over 3 s: the load average read 4–12 for hours while another agent's gate ran
with the CPUs idle, so the harness reads `/proc/stat` instead; `tctl` before>after per row in
`~/tmp/staged-perf/*.log`, 47–78 °C; legs interleaved A1w2 B1w2p B2w2p B4w2p A2w2 B5w2 A3w2 B6w2p
A4w2 B7w2 B8w2p A5w2 B9w2 B10w2p A6w2 B11w2; the RAM NVMe set, the ring on btrfs/LUKS/NVMe):

| row | A (dev `a43bda8`, 5 legs) | B (`6398c52`, B10w2p / B11w2) | B/A | ext4 (`ext4-w2`) | B/ext4 |
|---|---|---|---|---|---|
| st4k 1 job | 38.3–46.1 k/s, p50 15–18 µs | **60.4 / 47.3 k/s**, p50 12.6 / 17.0 µs, p99 40–46 µs | 1.2–1.5× | 209 k/s, p50 2.9 µs | 0.23–0.29 |
| st4k 4 jobs | 73.3–75.5 k/s, p50 30–33 µs | **156.0 / 147.8 k/s**, p50 21–22 µs, p99 43–46 µs | 2.0× | 740 k/s | 0.20–0.21 |
| st4k 16 jobs | 45.7–56.8 k/s, p50 64–66 µs, p99 1.25–1.88 ms | **281.4 / 290.5 k/s, p50 46 µs, p99 96–103 µs** | **5.2–6.2×** | 570 k/s | 0.49–0.51 |
| + fsync 1 job | 1.15–1.24 k/s | 1.23 / 1.15 k/s (the ring sync 686 µs) | 1.0× | 120 k/s | 0.01 |
| + fsync 16 jobs | 3.3–4.9 k/s | 6.6 / 6.3 k/s (the ring sync 1.09 ms) | 1.3–1.9× | 562 k/s | 0.011 |

Daemon cores at 16 jobs 4.7–5.4 → 12.6 (sqz-fuse-ln 4.8, sqz-fuse-ur 4.5, iou-wrk 3.1 — 45 µs of
CPU per 4 KiB write); every fsck clean; `ring_full 0`. The 1-job row is bimodal across legs
(44–49 k vs 59–60 k: CPU placement of fio against its lane). The ext4 reference is the same fio
shape on ext4 over one RAM NVMe namespace (`ext4rows.sh`), started AT 70 °C and run hot (to 84–95).
**The target (≥ 500 k/s at 16 jobs) is not reached: 290 k/s, 0.5× ext4.**

**The striped pair must not regress** (`quiet.sh ram-sqz`, rr4k/rw4k q32 j4, sw1m q16 j4, 20 s):
A (`a43bda8`) 1.731 M / 1.087 M / 49.1 k; w2-striped-B (before `6332ece`) 1.625 M / 0.940 M /
47.9 k; B2 (`bf60b53`) 1.747 M / 1.136 M / 52.7 k; B3 (`6398c52`) 1.740 M / 1.142 M / 52.8 k.

**The split at 16 jobs after the levers** (B10w2p: 156 k host samples, 78 k the daemon's; the
daemon's inclusive shares): the ring round trip per op — `io_uring_enter` 35 %, `Lane.parkOnce`
16 %, `Group.park`/`flushNonBlocking` 11 %, `io_cqring_wait` 19 %, `__schedule` 12 %,
`try_to_wake_up` 7.5 %, `eventfd_write` 3.8 % (`wakeFromForeign`), `Lane.spawn` 3.6 % — about
45 % of the daemon's samples; `overwriteStaged` 9.4 % (sqzhash `updateLanes` 4.3 % self: the
header's checksum re-hashes the whole 16 KiB record for a 4 KiB grain); `clock_gettime` 6.7 %
(the phase stamps); the slow path on a point miss 9 % (`writeStaged` 6.6: `readInodeOpt` 3.6,
`layoutKindView` 2.1, `readLayout` 2.0, `resolveAt` 1.6; `NodeSnapshot.lookup` 4.7); `memcpy`
1.7 %; `btrfs_page_mkwrite` gone. Whole host: `io_wq_worker` 15 % — the grain's IORING_OP_WRITE
is PUNTED to an io-wq worker on every write (btrfs's buffered write has no NOWAIT path:
`io_write` → io-wq; `btrfs_file_write_iter` 8 %, `down_write` 4.4 % = `btrfs_inode_lock`: sixteen
lanes' grains serialize on ONE segment file's inode rwsem, `seg-0`), `_raw_spin_lock` 5 %, fio
10.5 %, idle 23 %.

**The remainder, named** (290 → 500 k/s): (1) the one segment file — per-lane segment files
(`bindRoot` already takes a list) end the inode-lock serialization, and a host filesystem that
honours NOWAIT for a buffered write (or the ring on its own device) ends the io-wq punt and with
it the fiber park per write (the CQE completes inline): 3.1 of the 12.6 daemon cores and the lane
wake per op; (2) per-grain checksums in the record header, so a grain write hashes 4 KiB (4.3 %);
(3) fewer clock reads per op (the phase stamps: 6.7 %); (4) the 29 % of point lookups
that still miss — a times-only landing stales the point though size and identity stand: a
times-only generation on the witness would keep it (9 %); (5) the FUSE transport's per-op cost
itself (`rung-10-seq-parity.md`'s territory).

**Kill -9 under the in-place stream** (`~/tmp/lw/killcheck.sh`: 64 × 16 KiB files written 'A',
four fio jobs rewriting random grains 'B' in place — 100–130 k in-place writes before the kill —,
a live O_DIRECT reader racing them, kill -9 mid-stream, fsck, remount): ten runs over the wave
(four on `6398c52`), 0.56–0.83 M grains read live per run with 0 torn, every grain all-'A' or
all-'B' after the remount (256/256), every size 16 KiB, fsck clean. dm-log-writes (`check.sh`) on
`6398c52`: lever 8/8, pressure 1000/1000 byte-exact, `staged_payload_lost_reads 0`. Suites on
`6398c52`: `data.*` 572/572, `kv_fs` 96/96, `write_admit` 78/78, `data.fsync` 48/48,
`data.staging` 62/62, `cli.mount` 41/41, `meta.economy` 61/61, `e2e` 116/116 (3 skipped); on
`bf60b53`: `meta.node_cache` 75/75, `read_view` 46/46 (1 skipped), `exec.sqz_io` 60/60. The smoke
rows named by the task (`sync -f`, THE KILL MATRIX, kill -9 mid-tar, both WRITE-BACK TIMER rows,
THE ORDERED LAW, the pack row, THE STAGED READ'S IDENTITY WINDOW) green on `6398c52`.

Risks: the point cache's correctness rests on the view's validation law (the witness generation
and the slot-move epoch read BEFORE the fill's lookups) — a KV landing that moves neither stales
nothing, by design; the in-place path's crash atomicity rests on the header-into-the-other-slot
ordering before the grain write and on the page cache surviving the daemon (a power cut owes an
unsynced record nothing); a reader meeting a grain mid-write parks at most 200 µs per retry; a
kill loses the parked time polish of up to 4,096 inos (was 512) within one 50 ms tick.
