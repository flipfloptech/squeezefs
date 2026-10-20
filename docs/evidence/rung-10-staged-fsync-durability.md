# Rung 10 — fsync makes STAGED bytes durable across a power cut (the owner's ruling, 2026-10-19)

**What changed.** A staged file's bytes live in the staging ring's mmap'd segment file and nothing
synced them: `kill -9` durable (the page cache keeps them), power-cut lost — xfstests' 65 dm-flakey
rows failed by design with `SQUEEZEFS_FSYNC_PROMOTE_STAGED` off (generic/057 the canonical one). The
owner rejected running the suites with the lever on and ledgering the rows: the law changes (charter
§1 item 10, §6.1 "Durability", §6.4; decision 0002 row 549). The reference's fsync had a
`staging.sync_key` leg — a WHOLE-shard `File::sync_all` on the blocking pool per call, uncoalesced —
that the port had dropped (spec §10.2 still named it); restoring that shape was the wrong design.

**The mechanism.** `fsync.zig` gains a STAGED SYNC leg after the promote lever and strictly before
the meta barrier (a durable layout never names bytes a cut took): `Segment.syncResident` issues ONE
`IORING_OP_FSYNC|DATASYNC` with the resident record's span in `off`/`len` on the handler fiber's
lane ring (`SqzIo.fsyncRange`: the kernel's `vfs_fsync_range`, what `msync(MS_SYNC)` runs for a
shared mapping; the fiber parks on its CQE, the lane thread never blocks). `syncfs` syncs the whole
ring once when it has residents. A promoted record (the lever, kept as the alternative posture —
promote instead of sync, cross-client visibility at fsync) owes no sync; an ino with no resident pays
nothing. Counted `staging_fsync_{syncs,bytes,coalesced}` (requests ≡ syncs + coalesced),
`staging_fsync_sync_ns`; the phase `staged_sync` joins both exact-sum families.

**The coalescing** (`staging.SyncCoalescer`, the journal lane's barrier law): a request joins the
PENDING batch (an intrusive FIFO of on-frame waiters + the union span of their records); with no
sync running it leads at once, else it parks on its tick word. The leader issues one sync over the
span, hands every member its result under the lock (one wake each) and makes the oldest request that
arrived meanwhile the next leader — no herd, and never a request answered by a sync that started
before it. Nothing allocated. Unit: 1 + 16 requests → 2 syncs, 15 coalesced.

**Ranged, by measurement** (`~/tmp/syncbench`: a 512 MiB shared mapping, one 8 KiB record dirtied
per iteration; p50 µs, clean ring / 64 other dirty pages never synced): btrfs on LUKS/NVMe msync
471/2322, ring FSYNC 491/2594, whole-file fdatasync 493/1870; ext4 on a loop 999/998, 1009/1040,
997/**2832**; ext4 on zram 25/62, 33/81, 20/**307**. The device flush is the cost everywhere; the
ranged sync never pays for a foreign writer's unsynced pages where the filesystem honours the range,
and the ring op is the same kernel path as `msync` with a fiber parked instead of a thread.
`sync_file_range` was rejected on semantics (no flush, no metadata).

**The supersession** (the second commit). A same-key re-stage (an overwrite inside a staged image)
tombstoned the old copy at once and let its last lease punch and reuse the slot — so an UNSYNCED
overwrite could un-durable the file's SYNCED bytes (another file's fsync, or ext4's commit, made the
punch durable while the new copy was still the page cache's; a cut found neither). A superseded copy
now holds a reference on the segment's retired list — no tombstone, no punch, its extent live for
placement — until the key's fsync covers its successor's generation, a `syncAll` that began after
the successor's index flip (the segment's index sequence, acquire/release against the flip), or the
key leaves; recovery after a cut takes the higher generation that survived. Retired cells on the
list also close the placement note of `rung-10-staged-unreachable.md`. Gauged
`staging_superseded_held{,_bytes}`, counted `staging_superseded_released`; the cell 80 → 112 B
(tables 5.5 → 6.3 % of the ring).

**Red → green.** `data.fsync` "the staged leg": ONE sync covering the record, ordered `intent →
staged_sync → meta_sync` (red: `expected 1, found 0`); none without a resident or after the lever's
promotion; syncfs once. `data.staging`: the law (the span = both header slots, key, value; EIO on
failure), the coalescer, the supersession (the cut recovers the old copy byte-exact; the chain
releases at the key's sync; a re-stage DURING a syncAll stays held), the Linux ring's real sync.
`exec.sqz_io` fsyncRange (one ring op from a fiber; the raw fdatasync off a lane).

**dm-flakey by hand** (`~/tmp/flakey/check.sh`, generic/057's shape on the suites' one-device
layout: write + fsync → `drop_writes` → the orderly umount → allow → remount → compare; a 64 KiB
fsync'd file, a 12 KiB fdatasync'd one, a 2 KiB inline one, a 64 KiB synced file then overwritten
UNSYNCED before another file's fsync). Before (dev `944f5e2`): the three staged files read as ZEROS
(`staged_payload_lost_reads 3`, `recovery_records 0`); inline whole. After: every file byte-exact,
the overwritten one as its synced image — `staging_fsync_syncs 3`, `staging_fsync_bytes 155,648`
(the records' spans), `recovery_records 3`, `lost_reads 0`. The xfstests rows stay the release
window's oracle.

**A/B cost** (`~/tmp/staged-ab`: the RAM NVMe set, the ring on `/var/tmp` = btrfs on LUKS/NVMe;
fio io_uring O_DIRECT QD1 20 s rows; A = dev `81e9f21` dist, B = `d64103a`; A B B A, each session
alone on the venue, Tctl 64/57/53/56 °C at the starts, 59–77 during rows; every fsck clean).
Striped rows (nothing staged, `ring_syncs 0`): 4k+fsync 1 job A 13.1k/14.6k vs B 13.3k/12.7k, 4 jobs
36.8k/36.6k vs 37.6k/38.5k, 16 jobs 89.9k/89.0k vs 91.6k/92.6k (fsync p99 293/297 → 293/289 µs),
1 MiB fsync/32 6021/5629 vs 6077/6167 MiB/s — inside their spreads: no new cost. Staged rows (4 KiB
write + fsync each into 16 KiB files, 64 per job: inline → re-stage → in-place append → re-stage on
the wrap):

| row | IOPS A → B | write p50 A → B | fsync p50 / p99 A → B (µs) | syncs / fsyncs (B) | sync mean |
|---|---|---|---|---|---|
| 1 job | 1198, 1177 → 651, 659 (**0.55**) | 750 → 709 | 42 / 106 → **717 / 1483** | 12885 / 13017 (1.0) | 654 µs |
| 4 jobs | 1143, 1117 → 1088, 1160 (0.99) | 3064 → 1958 | 76 / 3031 → 1597 / 3818 | 13674 / 21753 (1.6) | 668 µs |
| 16 jobs | 1067, 1063 → 1001, 1041 (0.96) | 7504 → 5931 | 94 / 40632 → 7111 / **29491** | 6463 / 20027 (3.1) | 1734 µs |

The law costs one device flush per fsync of a staged file: 0.65 ms on this ring, so a lone
fsync-per-write small-file stream halves (1.2 k → 0.65 k/s, the daemon idle on the flush: 0.94 →
0.61 cores). Under concurrency the coalescer absorbs it — 1.6 and 3.1 fsyncs per sync, IOPS within
4 %, and the fsync p99 at 16 jobs FALLS 41 → 29 ms (the write p99 60 → 23 ms): the storm is bounded
by the staged arm's own serialization, not the sync. That serialization — ≈ 1.1 k staged writes/s
whatever the job count, a 4 KiB write at 0.75–7.5 ms p50 against 17–40 µs on the striped path — is
dev's today and the next lever.

**Suites.** data.* + kv_fs + e2e + exec.sqz_io 696/698 (2 skipped); journal_lane 61/61; the smoke
rows `sync -f`, THE KILL MATRIX, kill -9 mid-tar, THE ORDERED LAW'S KILL ROW, the WRITE-BACK TIMER
rows, THE STAGED READ'S IDENTITY WINDOW, the pack rows 134/134. No full gate (the train gates).

**Open.** A pressure promotion releases a synced staged copy once its layout commits, before any
barrier covers the block's DMA on a volatile-write-cache device — the same class on the promotion's
side; the next fsync of the ino barriers it, a cut before that loses the file's synced bytes. The
pool's ledger does not count held superseded copies: the ring can be fuller than the pool thinks
(`staging_ring_full_refusals` is the loud sign; the write takes the own-block arm, never lossy).
