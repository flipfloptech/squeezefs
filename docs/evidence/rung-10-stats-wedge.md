# Rung 10 — the `.stats` herd wedge: a render on the drain worker, a lookup livelock in the VFS (2026-10-28)

**The sighting** (decision 0003 §CB.5 (b)): 32 mdtest ranks read `<mount>/.stats` at once in a phase
hook on a plain `--daemon --supervise` mount (`8b14376`); one `sqz-fuse-ur28` worker ran 30 s (wchan 0),
the supervisor's control-inode stat went unanswered and it SIGABRTed the daemon.

**Reproduced** on `bc569d4`, 3/3 within 60 s: `~/tmp/stats-wedge/rig.sh` — a 2 MDT + 2 OST set on loop
images under `/build/tmp`, mdtest `-np 32 -n 10000 -u` in a loop beside 32 `cat .stats` loops. Twice with
the supervisor: `unresponsive for 30 s (no answer to stat of the control inode)`, wait status 0x86. The
readers' walls at mount: 1.2, 1.6, 2.0, 2.5, 3.6 … 10 s, then 27–39 s until the abort. On an IDLE box
(no mdtest) 32 readers alone: p50 4.7 s, max 21.5 s a `cat`, 2,179 renders for 55 completed reads.

**The cause, two halves.**
1. *The render ran on the drain worker and walked the kernel.* A `.stats` LOOKUP's handler is a fiber on
   the worker's driven lane; its render (FIFO under the 10-23 `render_lock`) took 27 ms idle — strace of
   five reads: ≈ 1,560 `mincore` + ≈ 680 `/proc` opens per render — and hundreds of ms under load, the
   ring unpumped meanwhile. The `mem_budget` family called every component's gauge live: `mincore` over
   the fill pool's 13,149 × 4 MiB = 51 GiB slab, the ranged slab, the LRU/cache slabs, every fiber stack;
   `cpu_class.sample` walked `/proc/self/task`; the nvmet sampler `/sys/class/nvme`. Proof: a stop-the-
   world gdb of the wedge (`w3/fibers-wedge.txt`): `sqz-fuse-ur21` inside `read_fill_pool.Pool.residency`
   (the `mincore` syscall) ← `gaugeFn` ← `Component.current` ← `MemBudget.writeStatsJson` ←
   `Renderer.render` ← `lookupVirtual`, 19 more `.stats` fibers parked on `render_lock`; perf of the
   loaded daemon with the workers' cycles in `__mincore_unmapped_range` / `filemap_get_entry`.
2. *The kernel's lookup livelock.* Every LOOKUP minted a new nodeid at entry TTL 0. `__lookup_slow`
   revalidates the dentry a sibling's lookup just created; a revalidation answered a different nodeid is
   invalidated and retried — O(N) lookups per read, each a render. The supervisor's probe was one more
   LOOKUP of `.stats` in the same queue: six 5 s misses, SIGABRT, while mdtest kept running.

**The fix** (§CC.1–3): the budget tick stores each component's reading (`Component.last`), the document
reports it; the CPU census and the fabric sampler run as a `mem_budget.Sampler` on the budget thread and
publish through a seqlock (`stats.SampledCpu`, `sampled_model.zig` — the weakening without the re-read
keeps a torn copy); the render — atomics and sampled words, no lock — runs on the blocking pool from the
lookup's fiber. A lookup within one render-time of the class's newest generation shares it (same nodeid,
one more kernel reference; the registry counts the kernel's references and the evicting FORGET returns
them all). The supervisor probes `.config`. New gauge `transport_pass_stalest_ms`: the longest drain-
worker pass in flight at the render (the lane-hold reading).

**The red tests** (§CC.4). The smoke row "32 CONCURRENT READERS OF .stats UNDER A CREATE STORM NEVER
WEDGE THE MOUNT" on `bc569d4`: 48 reads in 8 s, p50 10,255 ms, max 17,730 ms, 1,110 renders, 0 shares,
1 probe miss. On the fix: 37,233 reads, p50 5 ms, max 27 ms, longest worker pass 11 ms, 56,108 shared
lookups, 0 misses, 0 overdue. Unit: the registry's share window, the FUSE-path share through kv_fs,
the budget's sampled gauges, the renderer's sampler law, the seqlock model.

**The counts** (the fix, supervised, mdtest -np 32 in a loop beside the readers, 10 min each).

| readers | reads | p50 | p99 | max read | longest worker pass | probe misses | watchdog / tripwires |
|---|---|---|---|---|---|---|---|
| 32 | 1,910,198 | 6 ms | 20 ms | 95 ms | (gauge predates the run) | 0 | 0 / 0 |
| 64 | see `~/tmp/stats-wedge/f64` | | | | | 0 | 0 / 0 |
| 128 | see `~/tmp/stats-wedge/f128` | | | | | 0 | 0 / 0 |

Idle box: one read 27 → 1–4 ms; 32 readers p50 4.7 s / max 21.5 s → p50 5 ms / max 36 ms; every
`mincore` and `/proc` open on `sqz-membudget`, none on a lane.

**Seen, not changed** (§CC.5): the op watchdog's `4 × p99` threshold is defeated by a slow-but-completing
storm of its own class (59 ops 26 s in flight, no line); the fill pool's 51 GiB `mincore` is now the
sampler tick's largest term; `stats_generations_live` reads 257 under a herd (the ring plus the kernel's one).
