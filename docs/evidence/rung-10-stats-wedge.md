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
   ring unpumped meanwhile. The `mem_budget` family called every component's gauge live, kernel walks
   among them: `mincore` over the fill pool's 13,149 × 4 MiB = 51 GiB slab, the ranged slab, the read
   LRU's slab, every fiber stack; `cpu_class.sample` walked `/proc/self/task`; the nvmet sampler
   `/sys/class/nvme`. Proof: a stop-the-world gdb of the wedge (`w3/fibers-wedge.txt`): `sqz-fuse-ur21`
   inside `read_fill_pool.Pool.residency` (the `mincore` syscall) ← `gaugeFn` ← `Component.current` ←
   `MemBudget.writeStatsJson` ← `Renderer.render` ← `lookupVirtual`, 19 more `.stats` fibers parked on
   `render_lock`; perf of the loaded daemon with the workers' cycles in `__mincore_unmapped_range` /
   `filemap_get_entry`.
2. *The kernel's lookup livelock.* Every LOOKUP minted a new nodeid at entry TTL 0. `__lookup_slow`
   revalidates the dentry a sibling's lookup just created; a revalidation answered a different nodeid is
   invalidated and retried — O(N) lookups per read, each a render. The supervisor's probe was one more
   LOOKUP of `.stats` in the same queue: six 5 s misses, SIGABRT, while mdtest kept running.

**The fix** (§CC.1–3, §CC.6). The budget's components split into CHARGES, read at the render
(`kv_node_cache`, `hot_block_tier`, `read_tier_mmap`, `write_cache`, `read_cache`, `parked_full_buffers`,
`block_alloc_table`, every `.gauge` source) and KERNEL WALKS, read by the sampler's tick and reported with
their age (`zc_fill_pool`, `ranged_buf_pool`, `read_lru` — a `mincore` per slab —, `exec_fiber_stacks` —
a `mincore` per stack; `Component.kernelWalk()`, `mem_budget_sampled_age_ms`, a `sampled` flag per
component). The CPU census and the fabric sampler run as a `mem_budget.Sampler` on the budget thread and
publish through a seqlock (`stats.SampledCpu`, `sampled_model.zig` — the weakening without the re-read
keeps a torn copy), `daemon_cpu_sampled_age_ms` / `fabric_sampled_age_ms` beside them. The render — no
lock — runs on the blocking pool from the lookup's fiber. ONE render in flight per class, in arrival
order (`Registry.lookup`): a lookup arriving during a render waits for the next one and shares it (one
nodeid, a kernel reference each; the evicting FORGET returns them all); a completed generation is never
reused. The supervisor probes `.config`. New gauge `transport_pass_stalest_ms`: the longest drain-worker
pass in flight at the render (the lane-hold reading).

**The first cut was wrong** (gate 159, three reds): sharing a COMPLETED generation for one render-time and
reporting every component from the tick answered a lookup with state older than its arrival — §AF's two
sequential opens read one generation, the direct arm's `ipc_direct_write_notes` missed the write it had
just made, §BS's `kv_node_cache` charge showed its eviction a tick late. The contract is §CC.6:
read-your-own-effects for every counter and charge; only the sampled kernel walks may be a tick old, and
the document says so.

**The red tests** (§CC.4). The smoke row "32 CONCURRENT READERS OF .stats UNDER A CREATE STORM NEVER
WEDGE THE MOUNT" on `bc569d4`: 48 reads in 8 s, p50 10,255 ms, max 17,730 ms, 1,110 renders, 0 shares,
1 probe miss. On the fix: 4,992 reads, p50 36 ms, max 361 ms, longest worker pass 2 ms, 14,520 renders
for 136,014 shared lookups, 0 misses, 0 overdue. Unit: the flight protocol (a held render, eight waiters,
one next render), read-your-own-effects under a herd (16 threads × 50 bump→lookup), the abandoned flight,
the budget's two kinds, the renderer's sampler law, the seqlock model. Gate 159's three rows pass unchanged.

**The counts** (the fix, supervised, mdtest -np 32 in a loop beside the readers, 3 min each).

| readers | reads | p50 | p99 | max read | longest worker pass | probe misses | watchdog / tripwires | mdtest runs |
|---|---|---|---|---|---|---|---|---|
| 32 | 49,084 | 78 ms | 555 ms | 1.29 s | 1 ms | 0 | 0 / 0 | 9 |
| 64 | 53,948 | 144 ms | 1.03 s | 4.2 s | 8 ms | 0 | 0 / 0 | 9 |
| 128 | 56,110 | 292 ms | 1.9 s | 4.2 s | 2 ms | 0 | 0 / 0 | 9 |

No read failed in any run (the 0-byte reads of the dev runs were `ENOTCONN` on the aborted mount). The
per-read latency is the contract's price: consecutive batches carry distinct nodeids, so a sibling's
revalidation still fails and a read costs ≈ N batched renders of a few ms (the first cut's generation
reuse read in 5 ms and was wrong). Idle box, one read: 27 → 1–4 ms; every `mincore` and `/proc` open on
`sqz-membudget`, none on a lane. The smoke rows that read `.stats`, the supervised and kill rows, the
kill matrix, the direct arm, xfstests generic/001 and 013 (fsck 0 findings), the unit filters fuse / cli
/ stats / budget / virtual / supervisor / nvmet / exec / counters, `zig build rails` and `zig fmt` pass.

**Seen, not changed** (§CC.5): the op watchdog's `4 × p99` threshold is defeated by a slow-but-completing
storm of its own class (59 ops 26 s in flight, no line); the fill pool's 51 GiB `mincore` is now the
sampler tick's largest term; the write/read caches' charge reads re-derive their bounds at the render (a
side effect, now on the pool thread); `stats_generations_live` reads 257 under a herd (the ring plus the
kernel's one).
