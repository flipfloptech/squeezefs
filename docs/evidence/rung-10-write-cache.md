# Rung 10 — the write cache (the staging ring in RAM, on every mount)

The owner's ruling (2026-10-21), after `rung-10-staged-arm.md` §3 named the wall under the
file-backed ring (the host filesystem's `page_mkwrite`, one segment file's inode rwsem across
sixteen lanes, the io-wq punt of every grain written through the host's write path: 290 k
4 KiB rewrites/s against ext4's 570 k on the same box): a production client has no local
filesystem and no local device, so disk-based caching goes and every mount holds a WRITE
cache in RAM — the staging ring in a kernel-held, size-sealed memfd — beside the READ cache
(another agent's), each a knob in percent of system RAM, 25 + 25 (Lustre's `max_cached_mb`
posture). Charter §6.1 "Staged" and item 10 amended; decision 0002 row 551. Branch
`perf/staged-arm-3` on dev `12768c8`:

| sha | piece |
|---|---|
| `f03c997` | `SQUEEZEFS_WRITE_CACHE_PCT` (default 25): `mem_budget.writeCacheBytesFrom` over the budget's own system root (the cgroup cage when one binds, else RAM, fleet-shared before the percentage), never above the R5 budget; 100 GiB ⇒ 25 GiB (the tie); the mount resolves it beside the budget |
| `17b8fcb` | the memfd ring: `staging.Ring.Ram` — grain 0 the marker, the rest the image; the member's claim an abstract socket; the memfd and the claim handed to the supervisor; after a `kill -9` the supervisor listens on the claim and hands the ring to the member's next mount; the promote-at-fsync law; recovered records unsynced; the pool's generation seeded past the recovered ones; the segment's cell tables grow in chunks |
| `60a8d9d` | disk-based caching removed: the file ring, `--disk-cache-paths`, the config's cache-path list (a record naming any is refused: reformat), `config set-cache-paths`, the `staging` verb, the member-root binding, the KD-8 staging phases, the NVMe read tier's disk binding (`SQUEEZEFS_READ_DISK_CACHE_MB` retired), fsck's C5, the suites' `-cache` slice, `perf_mount --cache-dir`; the ring's sync law, holds and cover rounds; `SQUEEZEFS_FSYNC_PROMOTE_STAGED` retired |
| `81e1930` | the recall: a foreign reader's token grant promotes the holder's residents of the ino first — no foreign read meets zeros for staged bytes that exist |
| `f7871ce` `cca6196` `53a97c5` `403ef11` | the ONE cache core `mem/cache.zig` (the read-cache agent's commits, cherry-picked; they rebase away under that branch's landing) |
| `4b78483` | the write cache is `Cache(Resident, PromoteSink)` over the memfd ring: the core's striped exact LRU is the recency, its bound the 25 %, its eviction through the Sink the pressure promotion; the 75 % high-water pass, the pool's cap and its promote pass go |
| `f953ddd` | the bound is the core's law (`cache.resolveBound`); budget.zig's duplicate derivation goes; the core synced to the read-cache branch's `05fad44` plus the outside-lock lookup fix |

## 1. The write cache

**The ring.** `Ring.Ram.bind` creates a memfd (`MFD_ALLOW_SEALING`, `F_SEAL_SHRINK|GROW|SEAL`)
of `SQUEEZEFS_WRITE_CACHE_PCT` of system RAM (27.9 GiB on this 109 GiB box at the default, the
budget-capped 9.4 GiB under the kill check's budget), maps it shared, stamps the marker in
grain 0 and hands the segment the rest. A grain write is a memcpy into the mapping (shmem has
no `page_mkwrite`; nothing takes an inode lock on the path; no io-wq); a freed run is punched
back (`fallocate(PUNCH_HOLE)` frees shmem pages); the recovery scan skips holes
(`SEEK_DATA`). The `Segment`'s cell tables grow in chunks of 4,096 cells: `maxCells` of a
quarter of RAM would have pinned a gigabyte of empty cells and index at mount; `tablesBound`
stays the ceiling (24.3 MB per 546 MiB: 4.2 %), the live figure the gauge.

**The claim and the handover (the kill -9 law).** The member's live claim is an abstract unix
socket `\0squeezefs-staging-<identity>-<slot>-<member>` (the member identity: the fabric host
pair the mount presents, the PR registrant — decision 0003 §BG, from the first gate's one red:
a second identity on the same host preempting a SIGSTOPped holder past T_owner met the
holder's live claim under the shared slot 0 and died `StagingOwnerLive`; now it binds its own
ring, and the fenced holder's residents stay the holder's exposure, loud): bound by the live
daemon and never listening
(a second mount presenting the member scope connects, meets a silent holder — ECONNREFUSED
through 2 s of retries — and is refused `OwnerLive`); the mount sends the claim and the memfd
to its supervisor as two tail-socket frames. At the daemon's death the supervisor LISTENS on
the claim at once (before the journal tails' replay: a successor mounting meanwhile connects
instead of reading a bound silent name as a live owner), and holds the memfd until the
member's next mount connects, takes the one frame, adopts the ring (the marker judged by
`bindVerdict`: a foreign member's refused, a dead identity wiped, an older epoch upgraded),
recovers the residents and binds the claim itself; a dismount that drained the ring says so
(`write_cache_drained`) and the supervisor discards it, as it discards a ring with no data
past the marker. A held ring has no deadline (it is acked custody); the log names the pid
whose `kill` discards it. Measured end to end (`~/tmp/lw/killcheck.sh`, 64 × 16 KiB files,
four fio jobs rewriting grains in place, a live O_DIRECT reader, `kill -9`): 190 k in-place
rewrites, 600 k grains read live, 0 torn; the supervisor held 9,369 MiB of memfd; the successor
recovered 64 records; every grain whole after the remount, fsck clean.

**The durability law.** A RAM record cannot be made durable where it lies: `fsync`/`fdatasync`
PROMOTE the ino's resident (`staging.promoteAtFsync` → the ONE primitive: the block DMA'd and
its volume flushed before the layout naming it publishes, DUR-1) BEFORE the data barrier, and
`syncfs` promotes every resident; a promotion the plane declines is an fsync that cannot be
acked — EIO, `fsync_promote_refusals`. The ranged sync of a segment file, its coalescer, the
`synced` flag with the supersession and departure holds and their cover rounds went with the
file ring: a key that leaves is released at once (the layout publish rides the journal tail,
which outlives the daemon like the ring). A recovered record adopts UNSYNCED (never promised
durable: the in-place arm stays open to it), and the pool's generation counter is seeded past
every generation the segment still carries — before, a counter restarting at 1 let a cut that
kept both copies of a re-staged recovered record hand the OLD one back on the next scan.

**The budget and the LRU (the ONE cache core).** The write cache is `Cache(Resident,
PromoteSink)` (`staging.WriteCache`): a `Resident` is a staged record's key, ino and bytes; the
key hashes whole for the index and by ino for the shard (`StagingKey.shardHash`: one ino's
records share a shard, so a release of an ino's keys takes one lock). The core's striped EXACT
LRU is the recency (`stageWrite` inserts, every append/overwrite/lease `touch`es: O(1), the
shard's spin lock; the sharding is the one stated approximation — the global order is exact
within a shard and approximate across the `shardsFor(cpus, bound, 16 MiB)` shards, 32 here),
its bound the knob's 25 % (`cache.resolveBound`: 25 % of min(RAM, budget ÷ 0.7), so a cgroup
cage or a fleet share scales it with the budget), its eviction the pressure promotion:
`PromoteSink.onEvict` runs OUTSIDE the lock (`runs_under_lock = false`, the eviction protocol:
the victim stays indexed and servable while the primitive promotes it; `.released` frees the
ring's cells, a declined promotion is `.kept` — relinked at the head, the pass backs off,
bytes never lost). A write's own insert never promotes inline (`inline_insert`: an eviction
met from `stageWrite` declines, `write_cache_inline_evictions_declined`, and the write falls
to its own block — the direct path's small cost under a bound the lanes have filled); the
drain task (`armDrain`: one in flight, `staging_pressure_kicks`) evicts to bound − bound/4
when an insert is refused at the bound or the ring passes bound − bound/8. The budget
component `write_cache` is PINNED (shmem pages are swap-only), weight 1; its shed lowers the
core's bound (`setBound`) and arms the drain; `staging_cell_state` and `staging_tables` beside
it as before. The pressure promotion follows the LRU: the dismount pass judges every resident,
the recall its ino's. Stats family `write_cache_*` (hits, fills, evictions, invalidations,
sink_kept, insert_refusals, bytes, bound_bytes, shards, census_visits).

**Coherence (the recall).** The token plane is the only foreign-read method, so the recall
rides the grant: `Holder.prepare` runs before a grant of an object is registered —
`DataPlane.recallResidents(ino)` promotes the ino's resident under the primitive's locks, the
promotion's publish runs its pass over the object, the grant reads post-commit. THE WRITE
CACHE'S RECALL row (two daemons over TCP): B stages 64 KiB, A reads it at once — sha equal,
A's `staged_payload_lost_reads` 0, B's `recall_promotions` 1; a second read asks nothing more;
the reverse direction; an unread staged file stays staged. Before: A read 64 KiB of zeros.

**Removed with the disk cache.** The format's cache paths and the "FORMAT decision" clause
(a `set_config` record naming cache paths is refused by name), `config set-cache-paths` and
the directory precheck, the `staging` verb, `Ring.bindMember/bindRoot/openSegment` and the
marker files, the residue census, the KD-8 rebind's staging phases (a supervisor-held ring
binds at the new epoch as an upgrade), the NVMe read tier's binding under the cache dir (the
tier object stays disabled, its RAM double serving the read ladder's tests until the unified
cache replaces the tiers), fsck's C5 (C4 binds over the write cache; `--repair --apply`
quarantines under `/var/lib/squeezefs/quarantine/<set uuid>` or `--quarantine-dir`), the
suites' `-cache` ext4 slice (the one-device layout is three slices), `perf_mount --cache-dir`,
the bracket's and dev_mount's staging roots, the porting specs' cache-dir language. The
smoke rows named WITH/WITHOUT a cache path are the write-cache rows (`SQUEEZEFS_WRITE_CACHE_PCT=0`
is the cache-less posture).

## 2. Measured

`scripts/perf_mount.sh --ram --fresh` (2 MDT + 2 OST on brd over nvmet-loop, the DIST
profile), fio `--rw=randwrite --bs=4k --ioengine=io_uring --iodepth=1 --direct=1` over 64 ×
16 KiB files (`st4k`: every write a 4 KiB rewrite inside a staged record), 20 s rows, with
`--fsync=1` per write (`fs1`); the thermal rule (ONE benchmark on the host; `pgrep -f
"matrix.sh|fio --name=mx|mdtest|fio "` empty, k10temp Tctl ≤ 70 °C and busy cores < 3 before
each session — "load < 3" read as busy cores because another agent's gate inflates the load
average with I/O wait; `tctl` start > end per row), interleaved A B B A, two sessions per
arm. A = dev `051c994` (the file-backed ring, `rung-10-staged-arm.md` §3); B = the write
cache (B1/B2 `81e1930`: the ring with the 75 % high-water pass; B3/B4 `4b78483`: the ONE
cache core's LRU); C = `SQUEEZEFS_WRITE_CACHE_PCT=0` on `81e1930` (cache-less: a 4 KiB
rewrite is the direct path's in-place grain into its own block); ext4 the same shape on the
same brd (`ext4-w2`). IOPS; p99 µs in parentheses.

| row | A dev (×2) | B ring (×2) | B LRU core (×2) | C cache-less (×2) | ext4 |
|---|---|---|---|---|---|
| st4k j1 | 44.2 k / 47.6 k (50/44) | 59.3 k / 53.0 k (34/38) | 57.0 k / 54.3 k (35/37) | 61.9 k / 53.4 k (41/50) | 209 k (13) |
| st4k j4 | 147.7 k / 146.5 k (44/46) | 240.1 k / 196.8 k (30/34) | 214.8 k / 194.0 k (33/35) | 194.5 k / 202.2 k (38/37) | 740 k (6) |
| st4k j16 | 293.5 k / 239.0 k (98/149) | **399.7 k / 383.1 k** (72/79) | **350.7 k / 348.6 k** (86/89) | 81.4 k / 90.0 k (1,565/1,483) | 570 k (8) |
| fs1 j1 | 1.1 k / 1.1 k (123/123) | 10.5 k / 8.3 k (111/151) | 9.9 k / 10.4 k (119/114) | 14.1 k / 15.5 k (46/43) | 120 k (6) |
| fs1 j16 | 5.1 k / 4.7 k (2,212/2,056) | 58.1 k / 47.2 k (309/424) | 55.9 k / 55.7 k (330/326) | 97.7 k / 96.6 k (85/89) | 562 k (15) |

tctl: every A/B/C row started at 48–69 °C and ended ≤ 89 °C (one C row peaked 89; the ext4
reference rows ran at 69–70 start). Daemon cores at j16: A 10.9–12.6, B 11.5–12.5, C 5.9–6.1.

**Reading it.** The write cache takes st4k j16 from 240–294 k to 383–400 k (1.4×; p99
149 → 72 µs) and the fsync-per-write rows 10–12× (every `fsync` is now ONE promotion of a
16 KiB record out of RAM instead of the file ring's ranged sync: 1.1 k → 10 k at j1, 5 k →
58 k at j16). The ONE cache core costs ~12 % at j16 against the ring's own pass (400 k →
351 k; j1/j4 within the session spread): the core's own time is 0.6 % of the daemon
(`staging.touch` 0.54 %, `Shard.touch` 0.1 % in `perf`), the rest is the write's insert
and invalidate on the stage path (two shard lock rounds per re-stage where the ring had none)
— named, kept: the LRU and the bound are the ruling. The cache-less column collapses at j16
(81–90 k, p99 1.5 ms: sixteen lanes' own-block publishes per 4 KiB write serialize on the
layout publish) and leads only where the write ends in the device anyway (fs1: 97 k vs
56 k — the direct path's grain is already durable where the cache pays a promotion per
fsync). Promotion cost: an fsync'd 4 KiB rewrite costs 1/5.6 of an unsynced one at j16
(55.9 k vs 350.7 k), 1/5.8 at j1.

**The 1 M bar is not met**: 350–400 k against ext4's 570 k on the same device (0.6–0.7×).
The j16 profile of B (`perf record -g`, 15 s): the FUSE transport (the uring submit/reap,
the request dispatch, the reply) 52 %, `io_uring_enter` 30 % of that, `overwriteStaged`
~10 % (the memcpy into the memfd and the record re-hash), sqzhash `updateLanes` 4.6 %
(the whole 16 KiB record hashed per 4 KiB grain), `clock_gettime` 6.8 % (the stamps on the
hot path); the cache core 0.6 %. The remainder is the transport's per-request cost and the
three levers `rung-10-staged-arm.md` §3 named (per-grain checksums, fewer stamps, the
times-only witness) — not taken this wave.

**The striped rows** (`quiet.sh ram-sqz`, rr4k-q32j4 | rw4k-q32j4 | sw1m-q16j4, 4 × 3 GiB
laid down; A = dev `9361279`, B = `4b78483`): rr4k 1.880 M vs 1.865 M IOPS (p99 131/130 µs),
rw4k 1.328 M vs 1.325 M (226/235 µs), sw1m 50.6 vs 52.2 GB/s (p99 5.7/5.3 ms); fsck clean
after both; tctl 53–80 °C. Unchanged within the session spread: the striped path never meets
the write cache. (B's tree carried the uncommitted bound-law edit of `f953ddd` at the time —
mount-time resolution only, no hot path.)

Stats of the B LRU-core j16 row: `write_cache_hits` 12.4 M, `fills` 2,688, `bound_bytes`
9.82 GB (the budget-capped bound under the harness's budget), 32 shards, 0 kicks, 0 pressure
promotions: the 64 records never approach the bound; the 12 % is the stage path's bookkeeping.

## 3. Verification

**Red tests, then green** (unit, `scripts/venue test`): `data.staging` 58 (the ring's memfd
bind/seal/recover, the marker verdicts, the hole punch and the `SEEK_DATA` scan, the
generation seed past the recovered records, the write cache over the core: the 1-shard bound
of 4 × 8 KiB — insert refused at the bound with the inline decline, `touch(1)` then
`evictUntil(bound/2)` promotes 2 and 3 in LRU order, a declined promotion is `.kept`, the
family's counts), `mem.cache` 46, `data.writeback` 44, `data.pack` 52, `data.plane` 49,
`data.fsync` 48 (promote-at-fsync before the barrier, `syncfs` promotes every resident, a
declined promotion is EIO), `kv_fs` 96, `write_admit` 78, `cli.mount` 41, `e2e` 116 (3
skipped), `mem.budget` 82, `core.knobs` 52; the rails green but for ONE row that lands with the
read-cache branch (the retired `SQUEEZEFS_READ_DISK_CACHE_MB` names `SQUEEZEFS_READ_CACHE_PCT`
as its successor). Smoke rows green on `4b78483`: WITH the write cache (×6 after the row
learned to read its pack count at the umount — the LRU drain packs beside it), WITHOUT
(`SQUEEZEFS_WRITE_CACHE_PCT=0`), THE WRITE CACHE'S RECALL, THE STAGED READ'S IDENTITY WINDOW,
FSCK AND SCRUB, the durability point, THE KILL MATRIX, kill -9 mid-tar, THE REPLAY PROOF,
both WRITE-BACK TIMER rows, THE ORDERED LAW.

**Crash checks by hand** (the gate binary of `4b78483`, scratch under `~/tmp/lw/`):
`killcheck-w3.sh` ×2 — four fio jobs rewriting grains in place under a live O_DIRECT reader,
`kill -9`, the supervisor holds the memfd, the successor adopts it: 0 torn reads, every grain
whole after the remount, fsck clean; `check-w3.sh lever 8` and `pressure 1000` PASS (the
fsync law: a promoted record's bytes on the device before the ack; 1,000 unsynced files
promoted under pressure then byte-exact after `kill -9` + remount); `draincheck.sh` (a 96 MiB
budget ⇒ a 96 MiB bound in 4 shards, 1,000 × 64 KiB unsynced writes): 828 evict-promotions,
108 pressure kicks, 170 inline declines (172 own-block falls), `kill -9`, fsck clean, 1,000
files byte-exact after the remount — the LRU eviction loses nothing.

**Not verified here**: a full gate (the train's); the kamrui/aarch64 venue; the cache under a
cgroup cage (the bound's scaling is the core's tie test only); a recall under the two-daemon
matrix at scale (the RECALL row is one file each way).

## 4. Open

- Per-lane regions of the ring, per-grain checksums, fewer clock stamps and the times-only
  witness generation (`rung-10-staged-arm.md` §3's remainder) — named, not taken: the wave's
  budget went to the law and the removal.
- The NVMe read tier's object and the read ladder's plumbing for it stay for the unified
  cache's replacement of the tiers.
