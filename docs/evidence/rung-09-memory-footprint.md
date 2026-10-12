# Rung 9 — the daemon's resident footprint: profile, the idle-trim law, the return

Charter §6.2 ("Memory budget authority (R5)": every pool sized from the budget, pressure =
max(Σ gauges, RSS, unreclaimable), Yellow freezes growth, Red sheds, memory RETURNS under
pressure), §1.5 (every constant derived). The trigger: `rung-09-external-suites.md` "Daemon
RSS" — ≈ 1.9 GB idle → 3.0–3.7 GB under the suites, never returning, the kernel's OOM victim on
the shared 16 GiB venue (LTP `syscalls` runs 2–3 died that way). HEAD `853b20c` + this series.

Venue: `7.2.6-sqz_aarch64_apple-vm_9c_virtio-blk` (9 vCPU, 16 GiB, cgroup `memory.max` 16 GiB
⇒ budget 13.74 GB solo; the stalls agent's two daemons shared the cage during most rows, so the
mount-time fleet census divided it by 3 → 4.58 GB and sized the pools from that). Set: 1 GiB
meta + 2 × 8 GiB data (sparse, `/build` ext4) + a cache path (staging ring 546 MiB, NVMe tier
16 × 256 MiB shards), `-o default_permissions,allow_other --supervise` — the suites' posture.
Shape: `fsstress -p 20 -n 2000 -s <seed>` rounds back to back (the task's 10-min shape), then
idle, then `rm -rf` of everything. Sampler: `.stats`, `/proc/<pid>/status`, `smaps_rollup` and a
per-VMA aggregation of `/proc/<pid>/smaps` every 30 s. Scripts: `.wt-rss/.rss-profile.sh` and
`.vma.py` (venue copies under `/build/tmp/`; not product paths, not committed).

## 1. The finding that reorders everything: the gate binary was a DEBUG build

`build.zig` promised "`gate` (default): ReleaseSafe + thin LTO" and used
`standardOptimizeOption(.{ .preferred_optimize_mode = .ReleaseSafe })` — which returns
**Debug** unless `--release` is on the command line (`std.Build.standardOptimizeOption`: the
preferred mode applies only under `-Drelease`/`--release`; the documented `-Doptimize=Debug`
was not even a declared option). Every `zig build`, every gate smoke, every bracket and every
external-suite run since rung 1 drove a Debug daemon. The evidence notes said "debug build" for
their measured rows; the OOM problem was measured on it too. In Debug:

- `fiber.canary_enabled` fills every mapped fiber stack with `0xA5` at map time → every slot a
  spawn ever touched is **fully resident** (643 × 1 MiB = 674 MB in the baseline's peak; the
  gauge's 752 MB "mapped" was ≈ 752 MB resident).
- Zig's Debug `undefined` fill (`0xAA`) runs on every `gpa.alloc` → every pool slab is resident
  at birth whatever it holds.
- `SafeAllocator` (std's default `gpa` in Debug AND ReleaseSafe) keeps 7 stack-trace frames per
  allocation and checks writes-after-free in Debug.

Fix (`build.zig`, the first commit of this series): the product binary and the SDK libraries are
ReleaseSafe by default (the library module is built once per optimize mode); the unit/rails/smoke
TEST artifacts stay Debug (the lock-order rail and the canary are debug-only rails);
`-Doptimize=<mode>` / `--release=<mode>` sets every artifact. Cost: the ReleaseSafe binary builds
in 2m21s on the venue (Debug 1m48s). Decision 0003 §I.

The ReleaseSafe binary alone does NOT fix the footprint: the pristine `853b20c` built
`--release=safe` still mounts at **2.81 GB idle** (§2's row "RS pristine"): the read-LRU slab is
fully resident even without the Debug fill (the safe-mode `@memset(undefined)` of
`Allocator.alloc` touches it), the bounce is `MAP_POPULATE`d, the staging scan faults the whole
ring, and nothing returns under pressure. Those are §3.

## 2. The profile (component → peak → after → accounted?)

Baseline = the Debug gate binary (what the suites ran). The baseline's fsstress round 4 wedged on
the write-path liveness stall the `.wt-stalls` agent owns (`setattr`/`write` parked past
`handler_entry` for 75 s, `lock_ticked_reregisters` climbing — its daemon log is kept at
`/build/tmp/rss-before-2383574/daemon.log`); the "peak" column is the plateau the wedged daemon
held for 7 minutes, so "after idle" for the baseline is that plateau: nothing returned, as the
suites saw.

RSS by `/proc/<pid>/status` and `smaps` (bytes; MB = 10⁶):

| Phase (Debug baseline) | VmRSS | RssAnon | RssFile | RssShmem | Σ gauges | unaccounted |
|---|---|---|---|---|---|---|
| idle at mount | 3,108,560 kB (3.18 GB) | 1.95 GB | 0.86 GB | 0.29 GB | 0.76 GB | 2.42 GB |
| peak (4 fsstress rounds, then the wedge plateau) | 5,627,340 kB (5.76 GB) | 4.37 GB | 0.88 GB | 0.38 GB | 1.97 GB | 3.79 GB |

Attribution by VMA (the baseline's peak; the `n` column is the mapping count):

| component | bytes resident (peak) | after idle | budget arm | what it is / why |
|---|---|---|---|---|
| read-LRU slab (`read_lru`, `fill_pool.Pool.initRam`, 6457 × 256 KiB) | 1,730 MB (one VMA, 100 % resident at MOUNT) | 1,730 MB | **not counted** (`read_lru` gauge = logical entry bytes = 0) | sized `RAM / 10` = 1.69 GB whatever the budget; a `gpa.alignedAlloc` touched at birth |
| fiber stacks (643 × 1 MiB mapped + 8 × 4 MiB) | 674 + 34 MB | same | counted as 752 MB MAPPED (over-reports the virtual) | every slot the FIFO pool handed out stays mapped; Debug canary ⇒ fully resident |
| staging ring mmap (`seg-0`, 546 MiB file) | 560 MB (RssFile) | same | `staging_mmap` kernel-reclaimable = staged bytes (0 at mount; the ring was full at the wedge) | the recovery scan read every grain of an all-holes ring |
| kmbuf regions (9 × 32 MiB `anon_inode:[io_uring]`) | 302 MB | same | counted (`transport_kmbuf_region`) | the kernel's delivery buffers: `nqueues × ringEntriesFor(32) × 1 MiB`, charter §4 geometry — required, correct |
| zc bounce memfd (`sqz-fuse-zc-bounce`) | 302 MB (RssShmem) | same | counted at its reserve | `MAP_POPULATE`: 9 × 32 × 1 MiB resident before any body was staged |
| SafeAllocator heap (merged 8 KiB-bucket mappings: 358 + 236 + 166 + 109 + 70 + 47 + 31 MB VMAs) + gpa large allocs (37 × 4 MiB + 27 × (4 MiB + 68 KiB) + 2143 × 64 KiB node images + 847 × 128 KiB + 283 × 192 KiB) | ≈ 1.3 GB | same | node cache **not registered** (its `kv_node_cache` registration existed only in a test); parked buffers **not registered**; the rest is allocator retention | the "pooled, not leaked" growth |
| parked-buffer pool (`accumulate.Pool`, 4 MiB cells) | 390 MB entries + the cached free cells | same | **not counted** (`parked_full_buffers` never registered) | cap `max(256, budget/16 ÷ block)` = 256 × 4 MiB; freed cells cached forever |
| nvme block cache (`nvme_dev.BlockPool`, mapAnon 4 MiB × ≤ 144) | 84 MB (20 mappings) | same | **not counted** | cached below the cap, never unmapped |
| nvme ranged slab (fixed buffer 0) | 38 MB | same | **not counted** | pinned for life (registered buffer) |
| fill-pool memfd (`sqz-read-fill-pool`, 381 × 4 MiB = 1.6 GB reserve) | 82 MB | same | `zc_fill_pool` = checked-out bytes (12.6 MB) | evicted slots stay resident shmem |
| thread stacks (30 × 16 MiB), binary, tables | ≈ 110 MB | same | — | the floor |
| `block_free_reclaim_queue_bytes` "up to 2 GB" | **0 RAM** | — | n/a | the gauge is DEVICE bytes of queued frees (`e.size` = the block's bytes), not RAM; the queue's RAM is `capCeiling × (Entry + u32)` ≈ 13 MB preallocated — the evidence note's attribution was a misreading of the gauge |

The deepest fiber frame the canary measured under fsstress: `exec_fiber_stack_hwm_bytes` =
209,824 B (205 KiB) against the 1 MiB handler class (the 512 KiB floor is std's measured panic
path ×2; the half-stack tripwire has 2.5× headroom). The size stands; the RESIDENCY was the
defect.

## 3. The fixes (biggest first; each a pure derivation with a tie test and a return path)

**3.1 The idle-trim law** (`src/mem/budget.zig`; 0002 row 466; 0003 §I). Every pool is a
component with a `trim` hook. The sampler's tick count is the idle clock (`mem_budget_ticks`);
a pool stamps an entry at its release, so `now − stamp` = 1 means "freed inside the last
period" and 2 "free for a full one". A **Yellow** tick trims entries idle ≥ 2
(`trim_yellow_min_idle_ticks`: what the live workload touched inside the last second is never
refaulted under "freeze growth"); a **Red** tick trims everything idle (0), after the shed pass
so what the shed evicted returns in the same tick. Green trims nothing: an idle pool keeps its
hot slots. Test: the recording trim fixture over G/Y/R ticks (order: shed then trim; counts per
component in `mem_budget_components.*.trims`). Every pool returns one entry out of its FIFO at a
time (pop → return pages → push), so a concurrent acquirer is at most one slot short for the
microseconds of a syscall and never sees a false exhaustion.

**3.2 The read slabs** (`read_fill_pool.zig`, `read_lru.zig`, `hot_tier.zig`, `read_path.zig`;
0002 rows 465, 467). Both slabs are ONE lazily populated mapping (anonymous via `mmap`, or the
memfd) — never a `gpa.alloc`. Per slot: `resident` (set at acquire, cleared by the trim) and
`freed_tick`. Gauges: `idle_resident_bytes` (free slots the kernel still backs — the pool's own
retention) and `residentBytes = checked_out + idle`. Components: `zc_fill_pool` and
`ranged_buf_pool` gauge their idle-resident bytes, shed = `trimToward(target)`, trim = the law;
`read_lru` gauges its slab's resident bytes (the logical `read_lru_current_bytes` undercounted
a fully resident slab by 1.7 GB), shed = evict to target + trim, trim = the law. Return:
`MADV_DONTNEED` (anonymous) / `FALLOC_FL_PUNCH_HOLE | KEEP_SIZE` (memfd); the Linux test reads
residency back through `mincore` and the returned slot as zeros. **The gauges MEASURE**: one
`mincore` over the slab per read, split by the slots' refcounts into idle and in-use (0003 §I
"what a gauge counts") — the first cut counted a slot resident from its acquire and read 1.3 GB
of "idle" fill slots against 75 MB of shmem (fills that never landed: the single-flight's
refused and short fills release untouched slots). Off-Linux (the portable tests) a touched slot
counts whole. Derivation:
`read_mem = min(RAM / 10, budget / 8)` — the reference's `RAM / 10` ignored the budget (a
cgroup cage, a fleet share); the budget's eighth is the transport arena's share (charter §4);
tie test at the venue's 4369 MiB mount-time budget (546 MiB, not 1.6 GiB) and a whole-machine
one. Stats: `read_fill_pool_idle_resident_bytes`, `read_fill_pool_trimmed_bytes`.

**3.3 Fiber stacks** (`exec/fiber.zig`, `lane.zig`, `sqz_io.zig`). `Stack.trim` =
`MADV_DONTNEED` over the usable region (the guard and the reserve stay mapped);
`Lane.trimIdleStacks(min)` pops free slots one at a time (stamped at recycle), trims the idle
ones, pushes them back; the `exec_fiber_stacks` component's trim hook runs it over every lane.
`exec_fiber_stack_bytes` stays the mapped reserve (minus trimmed usable regions),
`exec_fiber_stack_resident_bytes` is the component's gauge — the pages the kernel holds per
mapped stack, one `mincore` each at every sampler tick (≈ 1,000 syscalls, ≈ 1 ms, on the
sampler thread) — and `exec_fiber_stack_trimmed_bytes` the returns; a re-spawn re-arms the slot
(and re-lays the canary in debug/test). Derivation stays the per-lane in-flight ceiling —
`fuseCapacity = q_depth × ceil(nqueues / fuse_lanes) + 16` (the kernel's depth is the
backpressure), `meta 256`, `lease/main 16`, each ≤ `budget / 8 / stack` — restated by the new
tie test: the venue's whole population is 1,077,739,520 B of VIRTUAL reserve (≈ the 752 MB the
suites' gauge was converging to), of which the kernel holds the touched depth (205 KiB max
measured; ReleaseSafe averaged 176 KiB per touched stack under fsstress) until the trim. Linux
test: four stacks mapped, nothing returns at idle 1, all four at idle 2 (`mincore` agrees), a
re-spawn re-arms one.

**3.4 The zc bounce** (`fuse/transport/worker.zig`, `session.zig`; 0002 row 467). The memfd is
no longer `MAP_POPULATE`d; every bounce bridge (a staged body read out, an extraction written in)
notes its window's page-rounded high-water; `transport_zc_bounce_bytes` =
Σ populated (the component's gauge, floor 0 — the reserve is
`transport_zc_bounce_reserve_bytes`). The sampler's trim hook leaves a request on every drain
group and wakes it; the worker pass punches the populated windows of ents that owe nothing
(registered / replied / retained — no handler holds the window, no bridge moves through it) and
went idle ≥ min ticks ago (stamped at the commit push). Stat:
`transport_zc_bounce_trimmed_bytes`. The 288 MiB of resident bounce at an idle mount became
96 KiB (one INIT-time window).

**3.5 The pools that were not components** (`io/nvme_dev.zig`, `data/accumulate.zig`,
`data/plane.zig`, `cli/mount.zig`). `aligned_buf_pool` (the reference's name): the whole-block
cache's CACHED bytes (floor 16 blocks, weight 2, shed = unmap toward the target, trim = unmap
cached entries idle ≥ min — the free list now carries `(addr, idle_tick)`);
`nvme_ranged_slab`: the pinned 36 MiB fixed buffer, attribution only (a registered buffer cannot
be returned); `parked_full_buffers`: the accumulate pool's RAM = live cells × block (parked AND
cached; the cached cells free under the trim in batches taken under the spinlock and freed
outside it; weight 0 until the write path binds its drain-target shed — 0003 §I);
`write_pipeline_inflight` (non-sheddable); `kv_node_cache` per metadata volume (floor 64 MiB,
weight 1, shed = a checkpoint kick — the registration existed only in a test).

**3.6 The staging recovery scan** (`data/staging.zig`; 0002 row 468). `Backing.nextData` =
`lseek(SEEK_DATA)` on the segment fd: the scan jumps to the next data run instead of reading
every grain of an all-holes ring through the shared mapping (546 MiB of RssFile at every mount).
`/build`'s ext4 reports holes (probed: `SEEK_DATA(0)` on a punched file → 8192); the venue's
virtiofs answers the query offset and the scan degrades to the grain walk (the Linux ring test
asserts `mincore` residency only where holes are reported).

**3.7 A second popper on a population-sized ring** (`sync/mpmc.zig`). The first fixed binary
died 2.5 minutes into an fsstress round, at the ENOSPC storm of the filled set: four lane
threads at once, `reached unreachable code` in `Lane.runReady` (the daemon log is at
`/build/tmp/rss-after-2484081/daemon.log`). The disassembly branched into the panic from
`Lane.recycle`'s `assert(self.free.tryPush(f))`: the idle trimmer pops a free slot and pushes it
back, a SECOND consumer on a Vyukov ring sized to its whole population, and Vyukov's producer
answers "full" when its wrap-around cell was CAS'd by a consumer that has not yet stored the new
sequence. Red-first: the mpmc test cycles pop→push from two threads over an 8-slot ring and
REQUIRES refusals (2,874 in 400 k cycles); the lane race test (`trimIdleStacks(0)` in a tight
loop beside 200 × 6 spawn/park/wake/finish rounds) reproduced the assert in Debug. `pushSpin`
spins through the window; the lane's recycle / spawn-refusal / trim, the fill pool's release /
trim and the block cache's trim use it. The pristine `853b20c --release=safe` ran the same storm
without it (it wedged on the stall 16 s later).

Not changed, and why: the kmbuf region (288 MiB, the kernel's delivery buffers, the charter's
`clamp(cap / (nqueues × payload), 4, 32)` depth at the measured-best 32 — required, counted);
the reclaim queue (RAM-bounded by `capCeiling`, its gauge is device bytes); the thread stacks
(30 × 16 MiB, ≈ 42 MB touched).

## 4. Before / after

Same set, same shape (fsstress rounds; after the first wedge the rounds' trees are removed
between rounds — the suites' shape, each test cleans up — so the 16 GiB set never fills to
ENOSPC, where the write-path stall sits), then idle, then `rm -rf` of everything. GB = 10⁹.

| row | binary | idle at mount | peak (VmHWM) | after idle + unlink | Σ gauges at the end | arm |
|---|---|---|---|---|---|---|
| Debug baseline (what the suites ran) | `853b20c` gate build | 3.18 GB | 5.80 GB (wedged at round 4: the plateau held 7 min) | 5.76 GB — nothing returned | 1.97 GB | Green (13.74 GB budget) |
| RS pristine | `853b20c --release=safe` | 2.88 GB | 4.36 GB (3 rounds, then the wedge) | — (wedged) | — | Green |
| **fixed, default budget** | this series | **0.51 GB** | **1.74 GB** (12 rounds in 10 min) | **1.11 GB** | 0.36 GB | Green at 13.74 GB (one Yellow and one Red event while two co-tenant daemons divided the cage: 325 trims fired, the daemon lived) |
| **fixed, 1200 MiB budget** (`SQUEEZEFS_MEM_BUDGET_MB=1200`) | this series | **0.34 GB** | **1.18 GB** (7 rounds in 8 min) | **0.96 GB** | 0.21 GB | Red throughout (the cage's unreclaimable arm reads every tenant's anon: 5–6 GB against a 1.26 GB budget); 827 trims per component |

What came back, and what did not (the fixed binary, after idle + unlink):

| component | default budget (Green) | 1200 MiB (Red) |
|---|---|---|
| fiber stacks resident (`exec_fiber_stack_resident_bytes`) | 7.0 MB of 403 MB mapped (the Red tick earlier in the run trimmed them) | 0.74 MB of 42.5 MB mapped |
| zc bounce resident | 0.9 MB (reserve 302 MB) | 0 (reserve 151 MB) |
| fill pool idle resident | 0 | 0 |
| parked-buffer pool RAM | 0 | 0 |
| nvme block cache | 0 | 0 |
| staging ring (RssFile) | 0 (the unlinks punched it; 727 → 301 MB of RssFile at the first `rm -rf`) | 0 |
| kmbuf regions | 302 MB — the kernel's delivery buffers, counted, required | 151 MB (depth 16 at this budget) |
| ranged slab (pinned) | 38 MB | 38 MB |
| node cache (`kv_node_cache`) | 17 MB | 17 MB |
| **live 64–256 KiB heap allocations (RssAnon)** | ≈ 0.55 GB: 3,035 × 64 KiB + 1,444 × 128 KiB + 522 × 192 KiB + 127 × 256 KiB | ≈ 0.58 GB: 3,077 × 64 KiB + 1,530 × 128 KiB + 569 × 192 KiB + 150 × 256 KiB |
| thread stacks (30 × 16 MiB), binary, tables | ≈ 0.1 GB | ≈ 0.1 GB |

So: every POOL returns (the pressure row's pools read 0 after idle, 7 of 7 trim hooks fired 827
times each; the default-budget row's pools are at their Green floor), the two mapped-at-mount
reservations are gone (−1.7 GB LRU slab, −0.55 GB staging scan, −0.29 GB bounce populate), and
the daemon settles at idle + ONE residual: ≈ 0.55 GB of live 64–256 KiB heap allocations of the
metadata layer that no gauge names (§6). The idle footprint went 3.18 → 0.51 GB, the peak under
the shape 5.80 → 1.74 GB, and the after-idle footprint 5.76 → 1.11 GB.

The cost side: under sustained Red every idle entry is trimmed every tick and the next use
refaults it (a fiber spawn ≈ 45 pages ≈ 10–20 µs; a 4 MiB fill ≈ 1 ms of zero-fill); the
pressure row's rounds ran 65–80 s against the Green row's 50–55 s. Under Yellow only entries
idle a full period are trimmed, and the live set is never touched.

## 4.1 The generic/074 shape (the stalls agent's hand-off: anon RSS "at the user write rate")

`fstest -n 3 -F -l 10 -f 5 -s 31457280 -b 512` (three children, 512 B FRAGMENTED writes +
truncate loops into 30 MiB files) against one daemon on the same set, `.rss-074.sh`:

| binary | 4 min | ops / user bytes in those 4 min | RSS growth | where |
|---|---|---|---|---|
| Debug `853b20c` (what the stall notes measured) | 3.10 → 4.19 GB | 227 k ops / 98 MB | **+1.1 GB ≈ 5 KB per op** | merged anonymous VMAs of 354 + 236 + 157 + 105 + 74 + 47 MB — the Debug `SafeAllocator`'s 8 KiB buckets (7 stack-trace frames per allocation AND per free, write-after-free checks, no reuse inside a bucket); Σ gauges flat at 1.36 GB |
| fixed (this series, ReleaseSafe) | 0.50 → 0.69 GB | 1.73 M ops / 854 MB (26,053 folds, 109 GB of device writes — the W2 fold's 128× is the write path's) | +190 MB in 4 min, **+234 MB over 8 min, flattening** (734 → 736 → 737 MB in the last samples) | fiber stacks' touched depth +30 MB, node images / fold buffers (64–128 KiB classes) +42 MB, the NVMe tier and fill memfd +34 MB |

The "growth at the user write rate OUTSIDE every component" was the Debug allocator's
retention per allocation (≈ 5 KB per FUSE op), i.e. the build mode of §1 — not a pool and not
a gauge gap; at ReleaseSafe the same shape is bounded (§4.1's 20-minute run below).

**The full invocation to completion** (`fstest -n 3 -F -l 10 -f 5 -s 31457280 -b 512`, 3
children × 10 loops × 5 files, "Total data size 471.9 Mbyte"; `/build/tmp/rss-g074full-3981521/`):
**rc 0 in 10 min 14 s**, RSS 502 → 833 MB (VmHWM 833 MB), flat over the last two samples and
after 60 s idle; the daemon log carries no error or warning. The Debug daemon the stall notes
ran was killed at 14.2 GB on the same shape.

## 5. The LTP `syscalls` run

`SQZ_BIN=/build/tmp/squeezefs-fixed LTP_FILES=syscalls SUITE_RUNS=1 scripts/suites/ltp.sh`
(results `/build/tmp/suites-ltp-20261011-121420/`; `/build/tmp/ltp-rss/rss.log` samples the
daemon's VmRSS/VmHWM every 30 s). **PASSED**: kirk rc 0, 1,551 tests — 1,240 pass, 88
fail/brok/timeout (4 OURS, all four adjudicated by the ledger; 84 the venue's — they fail on
ext4 too), 223 conf, 2,694 s wall; `fsck --offline --scrub` 0 findings; ledger verdict 0
unadjudicated, 0 stale. **The daemon survived** (no OOM kill, no panic, no watchdog line); its
RSS peaked at **VmHWM 4.43 GB** (4,432,320 kB; 3.5 GB at the end of the suite) — on a venue
with 10.7 GB free during the run, so the 3-run discipline is unblocked on this binary. The
peak's attribution (a `.stats` + `smaps` sample at 4.1 GB): Σ gauges 1.53 GB (`kv_node_cache`
535 MB, `parked_full_buffers` 482 MB — 115 cached 4 MiB cells, Green keeps them —, kmbuf 302 MB,
stacks 103 MB resident, bounce 67 MB) and ≈ 2.6 GB of the 64–332 KiB heap class (§6: the node
snapshot indexes the cache does not charge: 11,782 × 64 KiB, 6,005 × 128 KiB, 2,535 × 192 KiB,
1,128 × 260 KiB, 837 × 256 KiB, 487 × 324 KiB). That residual is the whole of what remains
unaccounted, and it is the metadata layer's.

## 6. What stays open

- ~~**The residual, and the one gauge still wrong: the node cache charges its extent images,
  not its snapshot INDEXES.**~~ **Resolved 2026-10-12 (`352f7f2`; 0003 §I).** `RecordIndex`
  charges `indexBytes(entries, sources) = entries × 24 B + sources × 16 B` to the owning
  cache's `kv_node_cache` gauge at `build`/`extendWith` and credits it exactly at `deinit`
  (the same deinit-exact law the memos and heads follow), so an index charges for as long
  as any snapshot over it lives — the superseded indexes a reader still holds included —
  and eviction returns it with the extent. The stat `meta_kv_node_index_bytes` names the
  live index bytes process-wide; `index ÷ mapped nodes` on a quiet mount is now the
  direct read of the retained-index question below. Measured by the new R5 test
  (`node_cache.zig` "R5 measure: the charge of N cached nodes with M records each …",
  16 nodes × 400 records over a 64 KiB layout, a live-byte allocator around the cache):

  | | charge (`kv_node_cache`) | allocator's live bytes above the empty cache | uncharged | per node |
  |---|---|---|---|---|
  | before | 1,048,576 B (16 extents) | 1,220,136 B | **171,560 B (14.1 %)** | 10,722 B, of which the index 9,616 B |
  | after | **1,202,432 B** (16 extents + 153,856 B of indexes) | 1,220,136 B | 17,704 B (1.4 %) | 1,106 B of fixed objects: the node, the snapshot's 8 memo cells, three Arc headers, the key bounds, a map and a clock slot |

  Red first: the test fails on the old charge by exactly the index bytes (`expected
  1202432, found 1048576`); after `evictToBudget` at budget 0 the charge reads 0 and the
  allocator holds 984 B — the shard maps' and the clock ring's grown capacity, no node. The
  per-node tie (`RecordIndex.indexBytes`, `node_snapshot.zig` "RecordIndex charge tie …")
  pins build, extend (a second index charges while the first stands), deinit and the
  standalone (cache-less) case; the freeze test restates a node's charge as extent + index
  + overlay + memo and that the superseded index dies with its snapshot. What remains
  of the audit ("why ≈ 5 indexes per mapped node"): the index of a node is a MERGE that
  keeps every superseded record as an entry until a compaction rewrites the node, so a
  leaf that absorbed k freezes carries `Σ frozen records × 24 B` — the 64 → 128 → 192 →
  256 → 324 KiB ladder is one node's index growing, charged now; whether retired indexes
  also outlive their snapshots is what the new stat answers on the next profile.

  The original finding, kept for the record: after idle + unlink the fixed daemon keeps ≈ 0.55 GB of live
  64–256 KiB heap allocations (the pressure row: 3,077 × 64 KiB, 1,530 × 128 KiB, 569 × 192 KiB,
  150 × 256 KiB; stable over the idle minutes); under LTP `syscalls` the same class reaches
  ≈ 1.7 GB (7,722 × 64 KiB, 3,567 × 128 KiB, 1,441 × 192 KiB, 507 × 256 KiB, 313 × 324 KiB …)
  while `kv_node_cache` reads 366 MB — Σ gauges 1.32 GB against 2.98 GB of RSS, the whole gap.
  Live, not retained (the SafeAllocator unmaps a large allocation at its free). By the sizes
  and `node_snapshot.zig`: `SharedBuf.createExtent` (the 256 KiB node image + footer = the
  814 × 260 KiB class ≈ the mapped nodes) is charged through `cached_bytes`; the
  `RecordIndex.entries` arrays (`IdxRec` 24 B × the node's records ≈ 64 KiB for a 256 KiB node,
  and a NEW merged array per append — `entries.len + fresh.len`, the 64 → 128 → 192 → 256 →
  324 KiB ladder) are not, and ≈ 5 of them per mapped node stay alive (the retired-snapshot set
  / fold memos: `meta_kv_fold_memo_misses` 170 k, `meta_kv_node_freeze_shadow_dropped` 281 k).
  Consequence: the cache's own budget (`max(budget/16, 512 MiB)` = 859 MB) is enforced on a
  charge that is a quarter of its RAM. Owed (the node cache's owner): charge the index arrays
  to `cached_bytes` (and audit why the old indexes outlive their snapshots), then the Red shed
  (a checkpoint kick) and the clock will act on the real bytes; until then Σ gauges undercounts
  the daemon by the index arrays and the RSS arm carries the difference.
- **The staging segment's fixed per-cell tables are a component — resolved 2026-10-12
  (`1d91625`; 0003 §I; `rung-09-sqzhash.md` §3.1 named them beside the cell states).**
  `staging_tables` (attribution-only, weight 0, heap): the cells (80 B), the free list (4 B),
  the pending list (40 B) and the placement scratch (40 B, one extra) at `maxCells`, and the
  key index's `ceilPow2(n / 0.8 + 1)` slots × 33 B — `Segment.tablesBound(capacity)`, charged
  at `Segment.init` and credited at `deinit`, registered beside `staging_cell_state` at
  `openStaging`, the `.stats` row `staging_tables_bytes`, the `data_layouts.md` §8 row.
  Measured: **31,574,220 B at the venue's 546 MiB ring** (139,777 cells; 30.1 MiB) and
  3,768,524 B at 64 MiB — 5.5 % of the ring either way (the index's power-of-two rounding is
  the swing), none of it a component before (Σ gauges did not see it; the RSS arm carried it).
  The tie test (`staging.zig` "the segment's fixed per-cell tables are gauged (R5) …") pins the
  formula term by term against the live tables, the std hash map's capacity law against the
  live map's `capacity()`, the two measured sizes, stillness through puts and removes, and the
  return at `deinit`; the pending list is now sized precisely so every table IS the formula.
- **Two defects the faster mount surfaced in the kill rows** (fixed in this series): the
  `kv_node_cache` component held `&cache.cached_bytes` while a joiner's
  `reopenControlProjection` replaced its cache (a freed gauge read by the sampler; now registered
  through `KvBackend.budgetComponent` under the swap lock), and the recovery screen refused a
  dead ring whose window held a cross-owner intent at the slot's HOME (local 0 by the home law;
  `meta_kv_replay_key_violations` every poll, kill row (c) ≈ 50 % of runs; the window scan
  marks xattr-kind records at local 0 as home records and the screen exempts them). The kill
  row (b)'s storm is open-ended now: a fixed 2,000-file storm outlasted `T_self` only at a Debug
  create rate.
- The write-path liveness stall (`.wt-stalls`): the baseline's fsstress round 4 wedged on it
  (`setattr` + `write` parked past `handler_entry` 75 s, `lock_ticked_reregisters` 118+), and
  every later run that let the set fill to ENOSPC wedged the same way within 20 s of the storm
  (`setattr` parked past `handler_entry`); logs at `/build/tmp/rss-before-2383574/daemon.log`,
  `/build/tmp/rss-rsbase-3180202/daemon.log`, `/build/tmp/rss-after3-3265269/daemon.log`.
- The build mode: every evidence note before this one measured a Debug daemon. The brackets'
  committed baselines (`bench/brackets/<venue>/`) and the suites' pass-2 counts were produced on
  it; the ReleaseSafe default changes the measured posture of every later row.
- `parked_full_buffers` carries weight 0 until `write_admit.zig` binds the drain-target shed
  (`drain_target.fetch_min` + notify, the parked gate) — the owner's file (0003 §I).
- The in-flight fill headroom (`max(cores × 16, 64)` slots of the fill pool) is the one part of
  the fill pool's RAM no gauge names (0003 §I).
- The mount-time fleet census counts every co-tenant daemon alive at mount (correct) — the
  pools are sized from THAT budget while the sampler re-reads the share every tick; a daemon
  mounted beside two others sizes its pools at a third of the budget it later enforces.
