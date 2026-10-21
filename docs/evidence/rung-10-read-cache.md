# Rung 10 — the read cache: one LRU core, 25 % of RAM, device grains

Owner's rulings (2026-10-21): two caches in percent of SYSTEM RAM (`SQUEEZEFS_READ_CACHE_PCT`,
`SQUEEZEFS_WRITE_CACHE_PCT`, each default 25 — Lustre's `max_cached_mb` posture) under the memory budget;
the read cache evicts on **plain LRU**; ONE cache model, used forward for reading and reversed for writing.
Charter §6.3, decision 0003 §BD. Venue: the workstation (`7.2.8-9.33-omarchy-sqz`), the RAM NVMe set, the
venue held alone per session on a daemon-free box, Tctl ≤ 70 °C per row, 20 s rows (`results.tsv` rows
`rc-*`). A = dev `12768c8`, B = this branch.

**The core** (`src/mem/cache.zig`, `Cache(Entry, Sink)`): a striped EXACT LRU — an intrusive list over a
preallocated item table per shard, a spin lock per shard, O(1) touch, nothing allocated per op; the one
approximation is the sharding (a victim is the oldest of a 1/S slice — tie-tested). The shard is
`Key.shardHash()`, the index `Key.hash()`: one hash for both made a block's grains one probe cluster and the
first A/B burned 96 % of the box in the lanes. The bound `pct % × min(RAM, budget ÷ 0.7)` follows the
authority's live budget each tick (Yellow freezes, Red sheds). Epoch validation on lookup. The Sink is the
direction: under the lock it DROPS; outside it (the write direction) the victim stays indexed and servable
while the sink runs, `.kept` relinks it at the head and the pass backs off — bytes never lost.

**The read cache** (`src/data/read_cache.zig` = `Cache(Grain, DropSink)`): 4 KiB grains (the LBA grain of
every ranged window; a 4 KiB read is one lookup, a deposit is exactly the device bytes paid for), windows ≤
64 KiB (the bounce slot; the hot tier keeps whole blocks). Deposits: the lane's ranged read after its DMA
(validated against the pre-DMA incarnation, stamped with the lease epoch read before it), the IPC
direct-drive READ at its CQE hook. Serves: the lane's walk and the warm ladder's queue-thread probe (the
FUSE sessions and the IPC ring arm share it) — one memcpy into the reply. The fifth tier of the ONE purge,
with a RANGED face (W1 purges exactly its window). Component `read_cache` (floor 16 MiB, weight 4). The slab
is reserved from the MACHINE's RAM, lazily populated: a supervised mount resolves its first budget at a
third (CLI + supervisor + daemon are three `squeezefs` in the fleet census), which fixed the first A/B's
cache at 9.2 GiB against a 12 GiB set (55 % hits); the live bound reads 27.4 GiB here.

**Coherence**: valid iff (a) the incarnation at the insert, (b) the binding after the copy, (c) the filling
inode's lease epoch at the lookup. A foreign write rides the token recall: the reader's sink purges the
layout's keys AND bumps the inode's epoch before it acks. Stated, not solved: a foreign in-place W1 patch
reaches readers at the parked stamp's drain (≈ 50 ms) — the hot tier's and the kernel cache's posture.

**Red tests**: the core's bound tie / LRU order under a counting seam / sink-before-release / kept back-off /
stale refusal / setBound; the grain cache's serves, purges, census, pin across an eviction, live bound;
`read_path`'s cold-deposit-then-hit (O_DIRECT too), the W1 ranged purge seeing new device bytes, two members
with A's recall purging B's grains before the ack under the verify seam (`SQUEEZEFS_TEST_READ_CACHE_VERIFY`
re-reads the device after every serve; `read_cache_stale_served` 0); THE TWO-DAEMON MATRIX's new step (A's
O_DIRECT re-read of B's window a grain hit, B's shrink + rewrite recalls A, A's third read is B's zeros).
THE READ VIEW row caught the wave's one defect: the FUSE zero-copy leg's hook deposited the worker's BOUNCE
(its DMA lands in the app's pages this process never sees) and a re-read served zeros — the FUSE leg now
deposits nothing; `SQUEEZEFS_READ_CACHE_FILL=lane` (demote the cold read so the lane deposits) measured a
regression at an 80 % hit rate (4k 1.53 M vs 1.92 M, 64k 0.39 vs 0.97 M: the lane hop + io-wq punt per miss
cost more than the device leg), so `leg` ships and the FUSE zero-copy rows are dev's.

| FUSE zc (`leg`) — A2 dev / B5 | cold | warm |
|---|---|---|
| 4k randread QD32×4 (M IOPS) | 1.93 / 1.92 | 1.78 / 1.70 |
| 64k randread QD16×4 (k) | 922 / 935 | 892 / 902 |
| 1 MiB seq read (GB/s) | 70.4 / 73.5 | 70.9 / 71.2 |
| 4k randwrite (M) · 1 MiB seq write (GB/s) | 1.31 / 1.37 · 50.8 / 50.9 | |

**The ring arm** (`-o interception`, psync, 100 % engaged, direct arm off): cold 3,145,728 deposits = the
set's grains each missed once, 54.0 M hits; warm 64.7 M hits, 0 misses, 0 evictions, 0 stale.

| ring arm — A2 dev / B5 (cold / warm) | B/A warm | ext4 same box |
|---|---|---|
| 4k randread ×32: 1.35 / 1.18 M → **2.86 / 3.23 M** (p50 18.6 → 4.6 µs) | **2.7×** | 1.99 M O_DIRECT · 1.21 M page-cache-warm (fio's 4 jobs copy-bound) · 3.05 M psync (10-20) |
| 64k randread ×16: 547 / 539 k → 580 / 580 k | 1.08× | 1,020 k · 207 k warm |
| 1 MiB seq read: 89.1 / 89.3 → 87.4 / 88.7 GB/s | 0.99× | 82.6 · 13.0 warm |
| 4k randwrite 1.17 → 1.10 M · seq write 52.5 → 52.9 GB/s | 0.94× · 1.01× | 1.92 M · 67.8 GB/s |

Daemon cost at 3.23 M: `sqz-ipc-svc` 10.2 cores (3.2 µs per serve) vs 9.5 at 1.35 M on dev. The ring arm
through the cache sits at ext4's psync figure with no device trust (the direct arm's 3.08 M needs it).

**Owed**: the leg that lands in the cache's own memfd slab and replies by `READ_FIXED` (no lane hop cold,
zero-pass warm — the FUSE zero-copy row's path to warm); a lock-free tokened-inode predicate so a holder's
W1 recalls readers at once (§BD.8); one order over the hot tier's slots and the grains (§BD.9).
