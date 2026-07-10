# 2026-07-10 — FUSE_COPY_FILE_RANGE wedge on long-churned persistent mounts: root cause + fix evidence

Companion to the fix branch `fix/hang-cfr-staging-shard-park`. Methodology
per `.benchmarks/2026-07-08-unmount-stuck-request-rootcause.md` (gdb on the
live wedged daemon). Raw forensics: `~/tmp/sqfsx/results/persist1/`
(`wedge2_gdb.txt`, `wedge2_kstacks.txt`, `wedge2_threads.txt`,
`wedge2_fsx_stack.txt`, `wedge2_fuse_waiting.txt`).

## Reproduction (dev @ b571950)

`~/tmp/sqfsx/soak_persist.sh` — the 075.2-shaped fsx soak WITHOUT the
per-run remount: format + mount ONCE, then consecutive fsx warm-up
(1 000 op) + 10 000-op runs on the same mount (seeds 100…). The wedge
reported at ~6 accumulated runs (seed 105) reproduced even faster here:

```
run 1 seed 100: PASS
run 2 seed 101: PASS
WEDGE: run 3 seed 102, fsx op 1048 "copy 0x821a6f → 0x90efa (0xedcb)"
       fsx log frozen 90 s; daemon alive; kernel-side wait only in fsx
```

- fsx kernel stack: `__fuse_simple_request ← fuse_copy_file_range` (waiting
  for the daemon's reply — kernel exonerated).
- fusectl: `waiting=3` and **every** subsequent op on the mount hangs —
  `stat .stats`, `cat .stats`, `ls`, `touch` — the whole daemon is wedged,
  not one inode.
- Daemon: **all 69 threads idle** (`futex_wait` / `io_cqring_wait` /
  `epoll_wait`) — a pure userspace await wedge, no kernel-side daemon wait.

## The two defects (gdb evidence)

### 1. The deadlock cycle (staging-shard lock)

parking_lot `RwLock` is **writer-preferring**: once a writer is queued, a
plain `read()` **parks** the calling thread. The §5.5 zero-copy flush holds
a shard READ guard across its DMA await. Wedged-daemon backtraces:

**Thread 4** — the fuse3 TPC handler thread, parked mid-poll of the CFR
handler in a **synchronous shard read**:

```
#7  parking_lot::raw_rwlock::RawRwLock::lock_shared_slow
#10 squeezefs::tiering::nvme::NvmeCache::get_static
#11 squeezefs::cache::nvme::NvmeStaging::read_staged_zero_copy
#12 squeezefs::fuse_client::flush_one_active_block           (existence probe)
#19 squeezefs::fuse_client::flush_due_active_blocks_for_inode (buffer_unordered(8))
#20 flush_active_blocks_with_retry
#21 squeezefs::fuse_client::copy_file_range                  (step-0 source flush)
#43 tokio::task::local::LocalSet::tick                       (the TPC executor itself)
```

**Thread 2** — a blocking-pool thread, the queued shard **writer**:

```
#6  parking_lot::raw_rwlock::RawRwLock::wait_for_readers
#10 squeezefs::tiering::nvme::NvmeShard::remove
#12 squeezefs::cache::nvme::NvmeStaging::remove_active_block
#13 squeezefs::fuse_client::flush_one_active_block::{closure#1}   (spawn_blocking)
```

The cycle: **C** (thread 4's sync probe) parks behind **W** (thread 2's
queued writer), which `wait_for_readers` on **A** — a §5.5 DMA read guard
held across an await by a sibling `buffer_unordered` future of the *same*
CFR task — whose wake can only be polled by **C's parked executor thread**.
C → W → A → C. Deadlock; every later shard read parks behind W too.

Churn dependency: the cycle needs same-shard coincidence of (guard-holder,
queued writer, sync probe) under flush pressure — probability grows with
accumulated staging population, hence "only after ~6 consecutive 10k-op
runs; fresh mount passes".

### 2. The blast radius: TPC pool collapse (affinity-poisoned lazy sizing)

Why did one parked probe freeze the *whole* daemon (even `.stats`)?
`main.rs` pins every tokio runtime worker to one core
(`on_thread_start` → `core_affinity::set_for_current`). fuse3's
`TPC_SCHEDULER` (the per-core handler-thread pool running **all** FUSE
handler futures) is a `Lazy` first touched from such a pinned worker —
`core_affinity::get_core_ids()` returned the caller's 1-CPU mask →
`remove(0)` → empty → `available_parallelism()` (also caller-affinity) = 1
→ **one** handler LocalSet thread for the entire mount. Observed in the
wedged daemon: exactly one thread ticking a fuse3 LocalSet (thread 4,
inherited cpu-9 affinity), and the stats inode reporting
`striped_block_concurrency = 4` (= clamp(1×2, 4, 64)) on a 16-core
`taskset` mount — the same poison hit every cores-based pool
(BG_TASK_SEM, stripe_write_semaphore, reclaim/upload sizing).

## Fix (invariants, not timeouts)

`tiering/nvme.rs` (`NvmeShard` doc) — the shard-lock acquisition invariant:

1. **Reads never park behind a QUEUED writer**: every shared acquisition is
   `read_recursive()`. An executor thread waits only for an ACTIVE writer's
   bounded, executor-independent critical section (sync index/memcpy op on
   a blocking-pool thread) — cycle impossible by construction.
2. **Writers never run on async executor threads**: `NvmeStaging` gained
   blocking-pool `*_async` wrappers; all inline async-context writers
   converted (stage_write's ring reserve, punch_hole_range,
   upload_full_block's invalidate, insert_active_block_buffer's spill,
   promote_staged_file, release_superseded_staged, delete_file sweep).
   `stage_write` now takes `Bytes` (refcounted — no payload copy).

`src/cpu.rs` — `process_parallelism()` from
`sched_getaffinity(getpid())` (the never-pinned main thread's mask; cached,
calling-thread independent). Adopted by bg_admit, DataRouter stripe
permits, reclaim/upload sizing, staging shard count, and the vendored
fuse3 `TpcScheduler` (same enumeration for its core-id list).

Regression pins (red on b571950): `tests/staging_shard_deadlock_tests.rs`
(deterministic C→W→A reconstruction on one LocalSet, 20 s deadline — timed
out on the parent commit, milliseconds on the fix),
`tests/cpu_parallelism_tests.rs`, `tests/tpc_scheduler_tests.rs`.

## Acceptance

- Persistent-mount churn soak, fixed binary, seeds 100–114, ONE mount, no
  remount: **15/15 consecutive 10k-op runs PASS** (incl. seed 105 at run 6
  and seed 102 at run 3 — both wedge points on dev) — see
  `~/tmp/sqfsx/results/fixedp15/summary.txt`. Baseline wedged at run 3.
- Hang 2 (loop-on-FUSE mkfs / LTP writev03): see the Hang-2 section below.
- fstests generic/616, generic/075, generic/091: green post-fix (one run
  each; per-PR data-path tier).

## Hang 2 — loop-on-FUSE mkfs (writev03)

Same-or-different verdict and evidence recorded after the Hang-1
acceptance runs; see the fix branch's final commit message and the
Task report. (Filled in the same investigation: reproduction on dev
baseline, then the fixed binary.)
