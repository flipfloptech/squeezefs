# Rung 10 — an idle daemon's resident memory follows its budget (2026-10-24)

The finding (the load-reds agent): a mounted, IDLE daemon held 2.0–2.1 GB of RSS under a 64 MiB budget, so on a
shared host every daemon's budget read Red / HARD BACKSTOP, the write cache shed to its floor, the read cache never
judged, and residency rows failed under load. Venue: this workstation (32 threads, 109 GiB, THP `always`),
`7.3.0-1.7-mainline-sqz`; branch `fix/idle-rss-follows-budget` on dev `7761bb0`. Scratch `~/tmp/idle-rss/`
(`trace.sh`: `strace -f -k` of every mmap + `/proc/<pid>/pagemap` per mapping, resolved with `addr2line -i`).

## Where the 2 GB went (dev `2fbc3ec`, `SQUEEZEFS_MEM_BUDGET_MB=64`, idle, VmRSS 2,035 MB)

| component | idle RSS | sized by | follows the budget? |
|---|---|---|---|
| read cache item tables (32 shards) | 494 MiB | 25 % of MACHINE RAM ÷ 4 KiB (7.1 M items × 72 B) | no — `page_allocator.alloc` is still `Allocator.alloc`: ReleaseSafe's `@memset(undefined)` wrote it (0677e2b's "lazy" table) |
| read cache key + inode maps | 464 + 208 MiB | the same 7.1 M, `ensureTotalCapacity` | no (written at birth) |
| read cache slab: free ring + per-slot arrays | 128 + 96 MiB | `slabSlots(reservation)` = 7.2 M slots | no |
| read cache slab bytes | 1.7 MiB | 27 GiB anonymous, lazy | yes |
| TLS, 146 user threads × 1.31 MiB | 191 MiB | `SharedLink.reply_buf` 1 MiB + std's sigaltstack 256 KiB + pack lookup 64 KiB, `threadlocal` (glibc zero-fills the static TLS at every spawn) | no |
| FUSE mainline payload pool | 128 MiB | `possible_cpus × clamp(budget/8 ÷ (nq × 1 MiB), 4, 32) × 1 MiB`, pinned by fixed-buffer registration | floor: 32 × 4 × 1 MiB |
| nvme ranged bounce slab | 128 MiB | `max(cores × 64, 512) × 64 KiB`, pinned | no (cores) |
| layout publish scratch | 44.5 MiB | `poolCap(cpus)` × 356 KiB, `alignedAlloc` | no (cpus), written at birth |
| read view bodies | 16 MiB | 256 slots × the record cap | fixed, written at birth |
| image + fixed tables | ≈ 84 MiB | text/libc 14, `.bss` 16 (THP), fsck verifier 8, holder/token tables 11, lanes/rings 10, metadata engine 10, rest | fixed |

At the default budget (78.7 GiB, solo) dev idled at 3.67 GB: the same plus the FUSE pool at depth 32 (1 GiB) and
the write cache's index sized by its 27 GiB ring.

## The law

An idle daemon's resident memory is its BUDGET's plus a floor no budget can shrink (`mem/sizing.idleFloorBytes`,
tie-tested): `possible_cpus × Q_DEPTH_FLOOR × 1 MiB` (the payload pool's pinned floor, one queue per possible CPU)
+ `threads × (std's 256 KiB signal stack + 64 KiB idle stack)` + 96 MiB of image and fixed tables (measured 84).
Every reservation sized from RAM is touched lazily (`mem/lazy.zig`: never `Allocator.alloc`, which writes every
byte in the safe modes) and counted when resident; every hashed index holds the entries it HAS (seeded at 1,024 a
shard, doubled by the insert that finds it full, up to the live bound's); every pinned buffer is a budget
derivation with a stated floor.

## The red test and the fixes

`AN IDLE DAEMON'S RESIDENT MEMORY FOLLOWS ITS BUDGET` (smoke): 64 MiB budget, two sampler ticks, VmRSS ≤ 64 MiB +
the floor (333 MiB here). Red on the test-only commit: **1,991 MiB**. Green on the branch: **257 MiB**.

1. cache core: lazy item tables; the index on demand (≤ log2 doublings a shard, moved under the lock, allocated
   outside it; `<prefix>_index_growths`); the write cache's index on demand too (`staging.writeCacheItems`).
2. read cache: `GrainSlab` — one stripe per core shard, bump cursor + warm/cold chains under a spin lock, lazy
   refcounts/links/stamps, residency counted (no per-tick `mincore` over 27 GiB).
3. per-thread borrow buffers (`lazy.PerThread`, unmapped by a pthread key's destructor) for the reply and the pack
   lookup.
4. the ranged slab: `clamp(budget / 16 / 64 KiB, 64, max(cores × 64, 512))` slots (unchanged from 2 GiB up on 32
   cores).
5. publish scratch slabs and read-view bodies lazy (the scratch gauge counts touched slabs).

| idle VmRSS | dev | branch |
|---|---|---|
| 64 MiB budget | 2,035 MB | 262 MB (transport floor 128, 146 threads × 0.27, ranged slab 4, image 84) |
| default budget (78.7 GiB, share 1) | 3,673 MB | 1,411 MB (the FUSE pool at depth 32: 1 GiB pinned by design; ranged slab 128; reclaimer 64) |

## The cgroup arm

The pressure's unreclaimable arm read the WHOLE cgroup's `memory.stat` over the daemon census. On this venue
`sudo` keeps the desktop session's scope: 6–11 GB of compilers, browsers and agents judged every daemon (the
red row's daemon: "unreclaimable 6,365,257,048 B ≥ 95 % of the 67,108,864 B budget"). In production a daemon owns
its service cage with its `sqz-supervise`, where the arm is right (it sees the memfd tails and the kernel's slab
that RSS misses). The law (decision 0002 row 556): the census counts the cage's foreign live processes; the arm
reads the cage only when there are none, else the pressure is the daemon's own gauges and RSS (`statm`; PSS
differs by the shared text, ≤ 14 MiB, and walks every VMA). The full-store row now counts "+0 of the memory
authority" warnings; the write-cache row "0 Red sheds".

## Runs

Unit filters `budget` 161, `cache` 193, `staging` 71, `read_cache` 53, `transport` 158, `nvme` 172, `read_view`
46, `merge_scratch` 43, `lazy` 52, `joined` 56 — all pass; `zig build rails` and `zig fmt --check` clean. Smoke rows
48/48: THE INTERCEPTION HOST (both), `umount --lazy` of a BUSY mount, A FULL DATA STORE, the data plane WITH the
write cache, TWO MOUNTS OF ONE SET, THE TWO-DAEMON MATRIX, the idle row.

Throughput (`rr4k-q32j4`, `sw1m-q16j4`): NOT measured — the venue never quieted (load 5–36 from other agents'
smoke loops and unit runs, Tctl 60–87 °C, 07:05). Owed, A-B-B-A on a quiet cooled host: `sudo MX_RUNTIME=20 MX_ROWS='rr4k-q32j4|sw1m-q16j4' scripts/perf/quiet.sh ram-sqz
idle-<leg>` in each tree. At the default budget the FUSE geometry, the ranged slab and the write path are unchanged;
what the rows exercise anew is the read cache's deposit/serve on the grain slab (a stripe spin lock where the fill
pool's ring was lock-free) and the index's on-demand doublings during the warm-up (≤ 8 a shard at 27 GiB).

Seen, not fixed: the FUSE pool's floor exceeds a sub-128 MiB budget on 32 CPUs (the charter's 1 MiB payload
base; a budget-derived `max_write` below it is a FUSE-contract change); std's 256 KiB signal stack per thread
(40 MiB at 146 threads); the fsck verifier's 8 MiB bound at mount; the reclaimer's 64 MiB at the default budget.
