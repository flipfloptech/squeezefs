# elbencho O_DIRECT read / small-block / overwrite attribution — 2026-07-11

**Scope:** reproduce + attribute the user's five-row elbencho table (real mount `/mnt/squeezefs`,
16 files × 10 GiB, `--direct`): fresh 1M writes fly (2487 MiB/s); sequential 1M reads (443 MiB/s),
1M overwrites (870 MiB/s), random 4k reads (1490 IOPS) and random 4k writes (261 IOPS) crawl.

**Verdict in one line:** every slow row is dominated by **stripe-block (4 MiB) read/write
amplification multiplied by disk-tier churn** (H1+H3+H4 confirmed); the FUSE transport and
kernel-side locking are secondary (H5 falsified as root cause, +29% side win; H2 partially
confirmed — write-side kernel lock flag was missing, reads were never kernel-serialized).

## Sandbox

- dev @ 4e4800e (baseline binary), branch work on top; CPU capped 3.5 GHz (not touched); AMD
  RYZEN AI MAX+ PRO 395, 32 CPUs; kernel 7.1.3-2-cachyos.
- Volumes: 4 × 8 GiB `sqdata` file-backed + 1 GiB `sqmeta`, all on /home NVMe (`~/tmp/sqperf/`),
  staging dir declared at format (`--disk-cache-paths ~/tmp/sqperf/staging`).
  Disk cache defaults = user's exact shape: **5 GiB read tier + 5 GiB staging** (user `.config`
  confirms `disk_cache_size` default 10GB split 50/50; their read tier: 5,368,709,120 B).
- Mount: `--read-mem-cache-size 1G --write-mem-cache-size 1G`, daemon caged
  `systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=0`.
- Load gen: elbencho 8 threads × 2 GiB (16 GiB dataset ≫ 5 GiB tier; RAM LRU only holds ≤256 KiB
  entries on this path, so cold-read rows are tier/device-bound as intended), one file per thread
  (`/tmp/sqperf_mount/f{1..8}`), `--direct`; random rows `--timelimit 30`.
- Every timed run gated on `pgrep rustc/cargo` quiet + Tctl < 80 °C (all runs 44–68 °C, logged
  per run); another subagent's builds forced explicit quiet-waits between phases.

## Reproduction table (2 runs; user's numbers for shape reference)

| # | Shape (essence) | User (16t×10G) | Sandbox a / b (8t×2G) | Shape match |
|---|---|---|---|---|
| 1 | `-w -b 1M --direct` fresh create | 2487 MiB/s | **4153 / 3508 MiB/s** | fast ✓ |
| 2 | `-r -b 1M --direct` sequential | 443 MiB/s (5.6× < row 1) | **97 / 102 MiB/s** (38× < row 1) | much slower ✓ (more extreme) |
| 3 | `-r --rand -b 4k --iodepth 16` | 1490 IOPS | **184 / 183 IOPS** | abysmal ✓ |
| 4 | `-w -b 1M --direct` overwrite | 870 MiB/s (2.9× < row 1) | **681 / 583 MiB/s** (5.9× < row 1) | slower than create ✓ |
| 5 | `-w --rand -b 4k --iodepth 16` | 261 IOPS | **128 / 129 IOPS** | abysmal, < row 3 ✓ |

Shape reproduces qualitatively on every row. Sandbox ratios are *more* extreme than the user's:
the 8 GiB cgroup cap denies the tier's mmap pages residency, so tier churn hits the device
harder than on the user's uncapped ~110 GiB-RAM box (page cache absorbs part of their churn).
Same disease, higher fever. (Also: user's daemon predates 166ed27/4e4800e tier-eviction fixes.)

## Ground truth: device byte accounting (`/proc/<daemon>/io` deltas)

| Row | User-visible I/O | Device READ | Device WRITE | Read amp | Write amp |
|---|---|---|---|---|---|
| 1a fresh write | 16 GiB w | 0 GiB | 16.2 GiB | 0 | **1.01×** |
| 2a seq read | 16 GiB r | **698 GiB** | **97 GiB** | **43.6×** | 6.1× (writes during a pure read) |
| 3a rand 4k read | 21.6 MiB r | **129 GiB** | 18.4 GiB | **≈6,100×** | ≈870× |
| 4a overwrite | 16 GiB w | **17.8 GiB** | 31.9 GiB | 1.11× (reads during pure overwrite) | 2.0× |
| 5a rand 4k write | ≈16.8 MiB w | **42.1 GiB** | 31.0 GiB | **≈2,600×** | ≈1,900× |

Row 1 is the control: write-through works exactly as designed (§5.3) — zero reads, 1.01× writes.
Every slow row is drowning in internal traffic the user never asked for.

## Per-row attribution

### Row 2 — sequential 1M O_DIRECT reads: 43× device-read amplification, tier churn

Mechanism chain (all confirmed by counters):
1. **4 MiB block fetch per miss** (H1): a 1 MiB read that misses fetches its whole 4 MiB block
   (`get_block_for_index` → `get_cached_or_fetch_block`) — 4× baseline amplification.
2. **6.3× refetch churn on top:** `.stats` `get_obj` Δ = 25,830 block fetches for 4,096 unique
   blocks. The 5 GiB tier ring (32 shards × 160 MiB, ring-geometry eviction, not LRU) plus the
   9-block-ahead prefetcher (`schedule_striped_prefetch`, jobs overlap across the 8 files)
   evict blocks fetched moments earlier — before their remaining three 1 MiB reads consume them
   — so the foreground refetches, republises, and evicts someone else's block: 25,830 × 4 MiB
   ≈ 101 GiB of device fetches.
3. **Every fetch is republished to the disk tier**: mmap write into the segment file (page
   faults page COLD segment pages IN, dirty pages write back out) → the 97 GiB of *writes*
   during a pure-read run, plus a matching page-in share of the reads.
4. **Victim materialization on every put** (fixed, see eaee897): the ring eviction copied each
   4 MiB victim payload out of the mmap — a cold page-in + memcpy **inside the shard write
   lock**, stalling same-shard tier readers — and the sole hot caller dropped the vec.
5. RAM LRU is bypassed by design for ≥256 KiB blocks (`get_cached_or_fetch_block` publishes
   only ≤256 KiB to `read_lru`), so 4 MiB striped blocks have **no RAM tier at all**: every
   reuse is an mmap/disk round trip.

Profile evidence (25 s `perf record -g --call-graph dwarf` mid-run, 152k samples):
**43.6% of daemon CPU in libc `memcpy`** (resolved 0x1b1e6b±: `memcpy@GLIBC_2.2.5`),
37.4% kernel (fault/IO paths; kptr_restrict=2 hides symbols), squeezefs' own logic <20%;
99% of samples on `tokio-rt-worker` threads.

Isolation probes:
- **Single thread reads 125 MiB/s ≈ the 8-thread aggregate (100 MiB/s)** — a shared ceiling
  (device saturated by amplification + shard write locks), NOT per-inode serialization.
- Warm-tier re-read of a 2 GiB slice (fits the 5 GiB tier) still pulled 55 GiB from the device
  — the tier defeats itself under its own churn.
- Device sustained ≈4.3 GiB/s reads during row 2 (698 GiB / 163 s) — near-substrate — to
  deliver 100 MiB/s of user data.

FUSE-side H2 facts for this row: `max_read=1048576` (one round trip per 1 MiB read — no
splitting), reads take the kernel inode lock SHARED (never serialized), `uring_queue_full = 0`.
The 36 ms/1 MiB the user sees is daemon-side fetch+churn latency, not transport.

### Row 3 — random 4k O_DIRECT reads: H1 at full strength

Every 4k read = one cold 4 MiB block fetch (1024× by design) + a 4 MiB tier publish + churn:
measured ≈6,100× device-read amplification, device saturated at ≈4.3 GiB/s to serve
**0.7 MiB/s** of user data. `--iodepth 16` cannot help: the work per op is the 4 MiB fetch.
(User's 1490 IOPS × 4 MiB ≈ 5.8 GiB/s internal — their substrate limit, exactly as hypothesized.)

### Row 4 — sequential 1M overwrite: stale-block RMW seed + tier publish (H3)

`write_file_staged`: the FIRST 1 MiB write into each existing 4 MiB block computes
`block_write_needs_existing_data() == true` (write_end < existing block end) and seeds the RMW
buffer by fetching the OLD 4 MiB block (`get_block_for_index`) — even though the next three
sequential writes fully cover the block and write-through then uploads it whole. Fresh create
(row 1) skips the seed (`existing_size ≤ block_start` ⇒ `ActiveBlockBuf::fresh`).

Measured: 17.8 GiB of device *reads* during a pure 16 GiB overwrite (≈16 GiB = one old-block
fetch per block + meta) + 2.0× writes (16 GiB data + 16 GiB tier publishes of the fetched old
blocks — which also churn-evict the read tier) + displaced-key free/alloc churn.
perf: libc memcpy 39%, squeezefs 35% (jemalloc alloc/free 4.4%, block-map `HashMap::clone`
1.6%), kernel 27%. Loadavg spikes to ~300 (spawn_blocking storm: `remove_active_block` +
detached tier publishes per write).

### Row 5 — random 4k O_DIRECT writes: RMW + spill + writeback triple carousel (H4)

Per 4k write: fetch old 4 MiB block (seed) → merge 4k → park buffer; parked buffers (cap 256)
spill 4 MiB to staging mmap under pressure; writeback later uploads 4 MiB per dirty block +
meta tx. Measured ≈2,600× read / ≈1,900× write amplification. Protocol costs are noise in
comparison: DLM acquires are µs (histogram), `meta_kv_*` journal bytes per op are KiB-scale —
**data amplification, not lease/meta protocol, is the cost** (H4's differential confirmed).

Side finding: at the end of run 5b the daemon hit the 8 GiB cgroup cap and was **OOM-killed**
(`sqperf-mount-*.scope: oom-kill`, 8G peak) — parked dirty buffers (256 × 4 MiB) + staging mmap
(5 GiB) + tier pages + payload buffers have no *joint* budget. Uncapped daemons externalize
this as system page-cache pressure instead. Worth its own hardening pass.

### Kernel-side H2 audit (vendored fuse3 + open replies)

- `FUSE_INIT`: `max_write` 1 MiB, `max_pages=256`, `max_read=1048576` (mount option),
  `max_readahead` mirrored back, `FUSE_ASYNC_DIO`/`FUSE_ASYNC_READ`/`FUSE_PARALLEL_DIROPS`
  negotiated — reads and readdir were never kernel-serialized; a 1M O_DIRECT read is one
  round trip.
- `FOPEN_DIRECT_IO` only on the virtual `.stats`/`.config` inodes (correct).
- **`FOPEN_PARALLEL_DIRECT_WRITES` (1<<6, Linux ≥6.2) was never advertised** — regular
  open/create replied `flags: 0`, so the kernel takes the inode lock EXCLUSIVE around every
  O_DIRECT write submission: same-file multi-thread / iodepth>1 O_DIRECT writers serialize
  kernel-side before the daemon sees them. Fixed (see below) — daemon lock model
  (`active_inode_locks` meta-prep + per-block `BLOCK_FLUSH_LOCKS`) already provides the
  ordering the data path needs; extending writes stay kernel-exclusive via fuse_dio_lock's
  past-EOF check.

### H5 — transport concurrency ceiling: falsified as root cause

`SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH=16 SQUEEZEFS_FUSE_OVER_IO_URING_QUEUES=16` (256 slots
vs default 32):

| Row | default env | H5 env | Δ |
|---|---|---|---|
| 1 fresh write | 3508–4153 | 3269 | ≈0 |
| 2 seq read | 97–102 | **129** | **+29%** |
| 3 rand 4k read | 183–184 | 197 | +7% |

`uring_queue_full = 0` in every run; amplification unchanged (446 GiB reads in the H5 row-2
run). Deeper queues merely overlap more of the daemon-side latency. Real but secondary; the
env knobs exist for operators — a default bump costs 8× payload buffer memory per queue and is
not the fix for these rows.

## Falsified / revised hypotheses

- **H5 falsified** as root cause (above): no queue-full backpressure; +29% is latency overlap.
- **H2 read-side falsified:** reads were never split (max_read=1M) nor kernel-serialized
  (shared inode lock); `FUSE_PARALLEL_DIRECT_WRITES` is an INIT-flagless *open* flag and only
  writes were affected. Confirmed missing → fixed for writes.
- **H1 refined:** the 4×/1024× geometric amplification is real but the measured 43×/6,100× is
  geometric amplification **× tier churn** (ring eviction under capacity pressure + overlapping
  prefetch + republish-on-every-fetch + no RAM tier for 4 MiB blocks). The tier is the
  multiplier, the block size is the base.
- **Row-2 lock theory (per-inode serialization) falsified** by the single-thread probe (125
  MiB/s alone vs 100 MiB/s ×8) — the ceiling is shared (device + shard locks), not per-file.

## Bounded fixes shipped (2, per policy)

### 1. `perf(tiering): index-only ring eviction for the read-cache hot path` (eaee897)

`cache_read_block` (the only hot caller) discards `put`'s evicted vec, yet the shard
materialized every victim — `Bytes::copy_from_slice` out of the segment mmap = cold page-in +
memcpy of up to 4 MiB per victim, held **inside the shard write lock**. New
`put_discard_evicted` shares placement/affinity/eviction logic (`route_put`/`put_impl`) and
drops victims index-only. Materializing `put` remains for `offline_device` re-homing (which
consumes victims). TDD red-first (2 contract tests), full gate green.

| Row | before (a/b) | after | Δ |
|---|---|---|---|
| 1 fresh write | 4153/3508 | 3442 | none (within run-to-run spread) |
| 2 seq read | 97/102 | 93 | none — row 2 stays device-bound on refetch churn |
| 3 rand 4k read | 184/183 | **221** | **+20% IOPS** |
| 4 overwrite | 681/583 | **756** | **+15–20% MiB/s** |
| fstests | — | generic/075, 091, 616 **pass** | no regression |

### 2. `perf(fuse): advertise FOPEN_PARALLEL_DIRECT_WRITES on regular opens` (this branch)

Kernel ABI bit 1<<6 on open/create replies for regular files; virtual inodes keep
`FOPEN_DIRECT_IO`. Kernel then takes the inode lock SHARED for non-extending O_DIRECT writes;
extending writes remain exclusive (kernel-side past-EOF check). Safety: daemon-side ordering
comes from `active_inode_locks` (meta-prep) + `BLOCK_FLUSH_LOCKS` (per-block merges); POSIX
gives concurrent overlapping O_DIRECT writes no atomicity guarantee to weaken. TDD red-first
(ABI + reply-flag contract test), full gate green.

Measured (single SHARED file, 8 threads, within-EOF 1M O_DIRECT overwrite — the shape the
kernel lock actually serializes):

| Shape | before | after | Δ |
|---|---|---|---|
| shared-file 1M within-EOF overwrite (8t) | 260 MiB/s | **399 / 476 MiB/s** | **+53% / +83%** |
| shared-file 1M fresh create (8t, extending — still kernel-exclusive) | 908 MiB/s | 1059 MiB/s | +17% |
| shared-file rand-4k iodepth 16 | 395 IOPS | 176 / 511 IOPS | state-noisy: neutral-to-positive (writeback carryover dominates run-to-run) |
| row 1 fresh create 8 files (regression) | 3477–4153 MiB/s band | 3564 MiB/s | none |
| row 4 overwrite 8 files (regression) | 583–756 | 605 | none (within band) |
| fstests trio | — | generic/075, 091, 616 **pass** | — |

## Ranked fix plan (structural — needs sign-off, NOT implemented)

1. **Kill the refetch churn: make the read tier LRU-honest or bypass it for sequential
   streams** (rows 2, 3; expected: row 2 → device-sequential speed, ≈10–20×).
   Options, cheapest first: (a) foreground fetches skip tier *republish* when the block was
   prefetch-published moments ago (dedupe by publish generation); (b) prefetcher writes
   straight into the tier and foreground reads it there (single copy, no double fetch);
   (c) replace ring-geometry eviction with segment-local LRU/clock so a hot block isn't
   evicted by cursor position. Effort M, risk M (cache correctness invariants well-pinned by
   074/075 tests).
2. **Sub-block reads: serve 1 MiB/4 KiB from the device without fetching the whole 4 MiB
   block** (rows 2, 3; the 4×/1024× base). `NvmeBlockDev` already does offset reads —
   plumb a ranged `read_block_range` through `get_block_for_index` for uncompressed volumes
   (crypto/compression require whole-block; gate on passthrough). Effort M, risk M
   (binding/incarnation revalidation must wrap the ranged read identically).
3. **Lazy RMW seed for overwrites** (row 4; removes the old-block fetch when sequential
   writes fully cover the block before flush): coverage tracking (`record_write`/`covered`)
   already exists in `ActiveBlockBuf`; defer the seed to flush-time *merge-if-incomplete*.
   Effort M, risk M-high (flush path must merge old bytes for incomplete buffers or data is
   zeroed — needs its own red suite first; the write-through complete-block case needs none).
4. **RAM tier for hot 4 MiB blocks** (rows 2/3 on re-read; the ≤256 KiB `read_lru` gate means
   striped blocks have no RAM tier at all): admit whole blocks under a small dedicated budget
   (e.g. 25% of read LRU) with the same incarnation validation. Effort S-M, risk M (the PR 6
   no-RAM-repromote rationale must be preserved — only device-validated fills may publish).
5. **Joint memory budget for parked buffers + staging mmap + tier pages** (row 5 OOM):
   account `active_block_buffers` + dirty mmap estimate against one cap; shed by early flush.
   Effort M, risk L.
6. **Operator guidance / env defaults** (row 2, +29%): document
   `SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH=16` for read-heavy O_DIRECT workloads; keep default
   at 4 (memory cost). Effort S, risk L.

## Rails compliance

- Sandbox only (`~/tmp/sqperf`, `/tmp/sqperf_mount`); `/mnt/squeezefs` untouched (read-only
  `.stats`/`.config` peeks); `/tmp/squeezefs_mount` avoided.
- Builds `taskset -c 0-15`, `CARGO_BUILD_JOBS=12`; every timed run behind a rustc/cargo quiet
  gate; Tctl logged per run (44–68 °C, never ≥80); CPU cap 3.5 GHz untouched and noted.
- Daemons caged (`systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=0`) — which is
  also how the row-5 OOM finding surfaced instead of eating the host.
- elbencho as user; sudo only for fstests; nothing pushed.
