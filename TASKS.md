# SqueezeFS — Remaining Audit Task List

Tracked follow-ups from the post-HPC_AUDIT codebase audit. **Excludes work already landed:**

- Unaligned write buffer leak fix (`src/nvme_dev.rs`)
- FUSE write connection scoping (removed `drop(con) // PREVENT DEADLOCK` hack)
- Non-fatal cleanup logging (silent `unwrap_or(())` → `log::debug` on errors)
- Unaligned write regression test (`tests/nvme_dev_tests.rs`)
- Removal of `HPC_AUDIT.md`

**Priorities:**

| Priority | Meaning |
|----------|---------|
| **P0** | Correctness / silent FS failure risk |
| **P1** | Reliability under load (exhaustion, deadlock risk) |
| **P2** | Scalability / performance |
| **P3** | Polish / observability / debt |

**Definition of done** for any task (project gates):

- Tests first when behavior changes
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo fmt --check`
- `cargo test --all-features -- --test-threads=1` (Garnet up for integration suites)
- `cargo doc --no-deps`
- Small reviewable commits; branch from `dev`; conventional commits; no drive-by refactors

---

## Suggested implementation order (DAG-friendly)

1. **P0-5** (tests first — layout/RMW) — *blocks safe refactors*
2. **P0-3**, **P0-2**, **P0-6** — write success = durable success
3. **P0-1**, **P0-4** — lifecycle / crash
4. **P1-1 … P1-6** — stop OOM / hang under load
5. **P1-8 … P1-11** — concurrency hygiene
6. **P2-1 … P2-10** — hot path (after tests exist)
7. **P2-11 … P2-15** — ops subsystems
8. **P3-*** — docs, metrics, gates

---

## P0 — Correctness & regression safety

### P0-1 — Worker-exit free of in-flight unaligned write buffers

| Field | Detail |
|-------|--------|
| **Status** | **done** (2026-07-02) |
| **Location** | `src/nvme_dev.rs` (`UringWorker` Drop + `worker_thread_loop` exit drain) |
| **Why** | On disconnect/shutdown, `ActiveReq.free_ptr` can still leak (drain was dropped during the leak fix for compile/Drop reasons). |
| **Acceptance** | Drain or RAII on worker exit; no leak under kill-during-write stress; existing nvme tests + new shutdown stress still pass. |
| **Notes** | `UringWorker::Drop` closes the channel then **joins** the thread (was detaching via JoinHandle drop). On loop exit: drain residual channel requests (free unaligned ptrs, fail oneshots) and free any remaining in-flight `free_ptr`s. Tests: drop during concurrent unaligned writes; drop after unaligned burst. |

### P0-2 — Transactional / atomic layout transitions

| Field | Detail |
|-------|--------|
| **Status** | **done** (2026-07-02) |
| **Location** | `src/routing.rs` (`durable_write_stripe_payload`, `commit_striped_layout_meta`, `write_file` / `write_striped`); fault inject `nvme_dev::FAIL_NEXT_WRITES` |
| **Why** | Partial Garnet updates + background block writes can leave inconsistent meta vs data on failure. |
| **Acceptance** | Failure leaves consistent state (retry-safe or rolled back); tests for failed allocate / failed write mid-transition. |
| **Notes** | Stripe creation now **awaits** block I/O and only then flips meta; allocated blocks freed on failure. `write_striped` frees successful blocks if any task fails before meta commit. Tests: preserve inline/staged on fail, no striped meta on failed first write, retry succeeds. |

### P0-3 — Background stripe write failure visibility + recovery

| Field | Detail |
|-------|--------|
| **Status** | **done** (2026-07-02) |
| **Location** | `src/fuse_client.rs` writeback worker, `fsync`/`flush`, `flush_inode_to_backend`; tests `tests/writeback_durability_tests.rs` (router create paths fixed in P0-2) |
| **Why** | Client can think write succeeded while block never landed. |
| **Acceptance** | Fail the op or track inflight with join/retry/fsck path; test forced `write_block` failure. |
| **Notes** | fsync no longer masks backend flush errors (returns EIO). Background writeback re-queues with backoff (max 4 attempts) then sticky `WRITEBACK_HARD_FAILURES` for fsync. Successful sync flush clears sticky state. Staged data retained on failed upload for retry. |

### P0-4 — Crash consistency: FUSE lease + DLM + fencing after kill

| Field | Detail |
|-------|--------|
| **Status** | **done** (2026-07-02) |
| **Location** | `src/dlm.rs` (`LockLease::is_held`), `src/fuse_client.rs` (lease re-validation), `src/recovery.rs` + `cache/nvme` binary meta; tests `crash_consistency_tests.rs` |
| **Why** | Two lock layers can diverge after crash. |
| **Acceptance** | Documented model + recovery/fsck tests for stale lease, expired fence, staged-but-uncommitted. |
| **Notes** | Documented DLM/fence/staging model in rustdoc. Live leases re-check Redis ownership before reuse; fencing errors invalidate local lease. Recovery parses **binary** `StagedMetadata` (was broken JSON). Tests: lock loss → re-acquire higher fence; stale write rejected; recover discards stale fence; recover commits matching staged. |

### P0-5 — Direct router layout transition tests

| Field | Detail |
|-------|--------|
| **Status** | **done** (2026-07-02) |
| **Location** | `tests/routing_layout_tests.rs` (+ `src/tiering/memory.rs`, `src/cache/lru.rs`, `src/routing.rs` fix for stale full-file LRU) |
| **Why** | No cargo-level tests for inline→staged→striped / RMW; external pjdfstest only. |
| **Acceptance** | Automated tests (Garnet + temp block file) for transitions, RMW, truncate, concurrent same-inode writes. |
| **Notes** | 10 tests cover inline/staged/striped, inline→striped, staged→striped, RMW (mid + cross-block), delete+rewrite (truncate-to-zero data path), concurrent same-inode under FUSE-like mutex, stale fencing. Tests exposed a real bug: oversized cache `put` left smaller stale entries, so growing staged→striped could read the old size from LRU. |

### P0-6 — Metadata write atomicity (MULTI/EXEC or Lua)

| Field | Detail |
|-------|--------|
| **Status** | **done** (2026-07-02) |
| **Location** | `src/routing.rs` (`atomic_meta_pipe`, commit/striped/inline/staged paths); `src/dlm.rs` lock acquire; tests `atomic_meta_tests.rs` |
| **Why** | Pipelines are not full transactions; partial apply under failure/timeout. |
| **Acceptance** | Critical meta updates atomic; tests simulate mid-pipeline failure if feasible. |
| **Notes** | Critical layout commits use MULTI/EXEC via `atomic_meta_pipe()`. `commit_striped_layout_meta` and `write_striped` map+size+fence are one transaction. Lock acquire is SET NX then INCR only on success (no token burn on fail; Garnet often has Lua disabled). Tests assert co-existence of related meta keys after writes. |

---

## P1 — Resource exhaustion & backpressure

### P1-1 — Bounded staging merge channel + backpressure

| Field | Detail |
|-------|--------|
| **Status** | **done** (2026-07-02) |
| **Location** | `src/cache/nvme.rs` (cap 1024; `try_send` → StorageFull + rollback cache put) |
| **Why** | Unbounded queue → RAM blow-up under write storm. |
| **Acceptance** | Bounded channel; full path either blocks with timeout, returns `ENOSPC`/`EBUSY`, or falls back (existing StorageFull path); load test. |

### P1-2 — Bounded writeback channel + handling

| Field | Detail |
|-------|--------|
| **Status** | **done** (2026-07-02) |
| **Location** | `src/fuse_client.rs` (`WRITEBACK_QUEUE_CAP=4096`; `enqueue_writeback` sync-flush on full) |
| **Why** | Same as above for striped/active-block path. |
| **Acceptance** | Bounded + defined full behavior; no silent drop of work. |

### P1-3 — Bound / limit `active_block_buffers`

| Field | Detail |
|-------|--------|
| **Status** | **done** (2026-07-02) |
| **Location** | `insert_active_block_buffer` spills oldest partials to NVMe staging at cap 256 |
| **Why** | Unbounded per-partial-block RAM. |
| **Acceptance** | Cap by bytes or count; flush or reject under pressure; unit test. |

### P1-4 — Cap growth of local DashMaps / caches

| Field | Detail |
|-------|--------|
| **Status** | **done** (2026-07-02) |
| **Location** | `attr_cache` → moka max_capacity + TTL; meta/block_map already moka |
| **Why** | Unbounded growth with inode/file churn. |
| **Acceptance** | TTL, max_capacity (moka already used in places), or LRU; metrics for size. |

### P1-5 — Admission control for concurrent background `tokio::spawn`s

| Field | Detail |
|-------|--------|
| **Status** | **done** (2026-07-02) |
| **Location** | `src/bg_admit.rs`; prefetch via `spawn_bg` + `buffer_unordered`; striped reads via `STRIPED_IO_SEM` / ordered `buffer_unordered`; DHT/peer publish via `spawn_bg` |
| **Why** | Untracked tasks → task explosion. |
| **Acceptance** | Semaphore / JoinSet / global inflight limit; no unbounded spawn on hot path. |
| **Notes** | Best-effort work is **dropped** when `BG_TASK_SEM` is full (bounded memory). Critical striped reads wait on a separate `STRIPED_IO_SEM` (cap 16). Write_striped already used per-mount stripe semaphore (P0-2). |

### P1-6 — Uring request channel pressure

| Field | Detail |
|-------|--------|
| **Status** | **done** (2026-07-02) |
| **Location** | `src/nvme_dev.rs` bounded 4096 + `try_send` backpressure to callers |
| **Why** | Under overload, only errors after queue growth. |
| **Acceptance** | Bounded queue or backpressure to callers; test under concurrent stress (extend existing 256-task test). |

### P1-7 — Client-side block-alloc rate / global reservation limits

| Field | Detail |
|-------|--------|
| **Status** | **done** (2026-07-02) — documented `LOCAL_BATCH=256` reservoir (existing amortisation) |
| **Location** | `src/block_allocator.rs` + multi-mount |
| **Why** | 15k clients can still hammer Garnet. |
| **Acceptance** | Shared quotas or larger batch policy documented + tested under multi-client sim if possible. |

---

## P1 — Concurrency / deadlock risk (remaining)

### P1-8 — Reduce inode write-lock hold time

| Field | Detail |
|-------|--------|
| **Status** | **done** (2026-07-02) — striped writes: meta-prep under inode write lock, data under `BLOCK_FLUSH_LOCKS`; inline/layout transition keep `EntireOp` |
| **Location** | `src/fuse_client.rs` `write` + `inode_write_lock_scope`; tests `tests/inode_write_lock_tests.rs` |
| **Why** | Serializes all writes per stripe; long tail under Redis/IO stalls. |
| **Acceptance** | Split meta-prep vs data; or finer locks; stress test same-inode concurrent writes for correctness + latency. |

### P1-9 — Document & enforce lock order

| Field | Detail |
|-------|--------|
| **Status** | **done** (2026-07-02) — hierarchy documented on `StripeLocks` in `fuse_client.rs` |
| **Location** | FUSE inode lock → lease lock → `BLOCK_FLUSH_LOCKS` → DLM/Redis |
| **Why** | Nested locks across awaits historically fragile. |
| **Acceptance** | Written lock hierarchy; static review checklist; no new cross-await lock pairs without review. |

### P1-10 — Audit remaining connection lifetime across awaits

| Field | Detail |
|-------|--------|
| **Status** | **done** (2026-07-03) — phased meta cons on `write_file` / `write_striped` / `write_file_staged` + jobs worker; defrag is redis-only SCAN (no change) |
| **Location** | `routing.rs`, `fuse_client::write_file_staged`, `jobs.rs`; tests `tests/connection_lifetime_tests.rs` |
| **Why** | Prep con fixed in FUSE write; other paths may still hold/reuse poorly. |
| **Acceptance** | Grep-driven pass; no connection held across long IO without need. |

### P1-11 — `active_write_backend` contention

| Field | Detail |
|-------|--------|
| **Status** | **done** (2026-07-02) — `arc_swap::ArcSwap<String>` |
| **Location** | `BackendRouter.active_write_backend` |
| **Why** | Read on every active-backend resolve. |
| **Acceptance** | `ArcSwap` / atomic id; microbench no regression. |

---

## P2 — Performance bottlenecks (hot path)

### P2-1 — Hoist / cache Garnet key strings

| Field | Detail |
|-------|--------|
| **Status** | **done** (2026-07-03) — `crate::keys::*` helpers (`FsKey` / `CompactString`); hot paths in routing/fuse/recovery/jobs/defrag/config_ops |
| **Location** | `src/lib.rs` `keys` mod; call sites in fuse/routing/… |
| **Why** | Alloc on every FUSE op. |
| **Acceptance** | Helpers (e.g. `meta_key(ino)`, reuse `FsKey`/`compact_str`); no wrong prefixes; layout tests still pass. |

### P2-2 — Consistent key namespace (`fs_key!` vs raw `metadata:`)

| Field | Detail |
|-------|--------|
| **Status** | **done** (2026-07-03) — dual namespace documented on `keys` mod: volume keys under `fs_prefix` / `fs_key!`; layout keys unprefixed historical (`metadata:`, `inline_data:`, …) — no on-disk migration |
| **Location** | `src/lib.rs` `keys` module docs + helpers |
| **Why** | Risk of wrong keys / hard-to-debug misses. |
| **Acceptance** | Single convention documented + enforced; migration if needed. |

### P2-3 — Crypto path amortization

| Field | Detail |
|-------|--------|
| **Status** | open |
| **Location** | `src/crypto_compress.rs`, per-block `process_write` |
| **Why** | Key schedule / wrap cost per block. |
| **Acceptance** | Session/key reuse where safe; benches for write_none / lz4 / aes paths unchanged or better. |

### P2-4 — Unaligned write path cost

| Field | Detail |
|-------|--------|
| **Status** | open |
| **Location** | `nvme_dev` memalign+memcpy |
| **Why** | Still expensive even if leak-fixed. |
| **Acceptance** | Prefer pool of aligned buffers; avoid copy when possible; keep unaligned test green. |

### P2-5 — Reduce Redis RTTs on write path

| Field | Detail |
|-------|--------|
| **Status** | open |
| **Location** | Meta size/type, fencing, block map, refcounts |
| **Why** | Still dominant under high QPS. |
| **Acceptance** | Pipeline more; cache meta with invalidation rules; measure with coz/dhat. |

### P2-6 — Striped write concurrency policy

| Field | Detail |
|-------|--------|
| **Status** | **done** (2026-07-03) — `bg_admit::striped_block_concurrency()` = `clamp(cores*2, 4, 64)` + runtime override; used by `write_striped`, staged flush, `STRIPED_IO_SEM` |
| **Location** | `src/bg_admit.rs`, `routing::write_striped`, `fuse_client` flush |
| **Why** | May under/over-subscribe cores. |
| **Acceptance** | Tunable (cores-based); criterion/`squeezefs_bench` comparison. |

### P2-7 — LRU put eviction path

| Field | Detail |
|-------|--------|
| **Status** | open |
| **Location** | `cache/lru.rs` `try_send` on every put |
| **Why** | Noise + work even when empty. |
| **Acceptance** | Batch or only on actual eviction; no drop of must-dehydrate blocks. |

### P2-8 — Expand real io_uring usage

| Field | Detail |
|-------|--------|
| **Status** | open |
| **Location** | Block path only; FUSE/network still elsewhere |
| **Why** | Spec vs implementation gap. |
| **Acceptance** | Clear plan: what moves to uring; no regression on existing block tests. |

### P2-9 — Write-verification mode cost

| Field | Detail |
|-------|--------|
| **Status** | open |
| **Location** | Full read-after-write when enabled |
| **Why** | Correct for debug, harsh for default. |
| **Acceptance** | Keep opt-in; optional sample rate; document impact. |

### P2-10 — DataRouter / Arc clone churn

| Field | Detail |
|-------|--------|
| **Status** | open |
| **Location** | Clone-heavy structures on tasks |
| **Why** | Extra atomics under concurrency. |
| **Acceptance** | Pass refs/`Arc` more carefully; profile with `high_concurrency_bench`. |

---

## P2 — Subsystems (defrag, recovery, NVMe-oF, jobs)

### P2-11 — Defrag without full SCAN / all free_set load

| Field | Detail |
|-------|--------|
| **Status** | open |
| **Location** | `src/defrag.rs` SCAN + SMEMBERS |
| **Why** | Won’t scale on large volumes. |
| **Acceptance** | Incremental cursor, sampling, or secondary indexes; `defrag_tests` still pass. |

### P2-12 — Defrag + live write interaction

| Field | Detail |
|-------|--------|
| **Status** | open |
| **Location** | Defrag `BlockMove` vs FUSE writes |
| **Why** | Move under active writers needs strong fencing. |
| **Acceptance** | Explicit lease/fence tests (extend `defrag_tests` “under lock”). |

### P2-13 — Recovery completeness matrix

| Field | Detail |
|-------|--------|
| **Status** | open |
| **Location** | `src/recovery.rs`, `tests/recovery_tests.rs` |
| **Why** | Partial scenarios covered. |
| **Acceptance** | Matrix: partial stage, partial flush, corrupt meta, missing mapping, fencing mismatch. |

### P2-14 — NVMe-oF path is CLI-only — document & fail-fast

| Field | Detail |
|-------|--------|
| **Status** | open |
| **Location** | `src/nvmeof.rs`, mount `--volume` |
| **Why** | Failures look like “FS broken” when target down. |
| **Acceptance** | Clear errors on missing device; optional health check at mount. |

### P2-15 — Jobs worker CPU limit correctness

| Field | Detail |
|-------|--------|
| **Status** | open |
| **Location** | `src/jobs.rs` |
| **Why** | Background jobs can starve FUSE. |
| **Acceptance** | Test under load that FUSE still progresses. |

---

## P3 — Observability, metrics, docs, debt

### P3-1 — Metrics: lock wait, channel depth, layout mix, spawn inflight

| Field | Detail |
|-------|--------|
| **Status** | open |
| **Location** | `METRICS` in fuse/routing |
| **Why** | Can’t debug regressions without signals. |
| **Acceptance** | Exposed in virtual stats inode / logs; no hot-path contention on metrics. |

### P3-2 — Replace remaining silent failures

| Field | Detail |
|-------|--------|
| **Status** | open |
| **Location** | `main.rs` and other `unwrap_or(())` sites |
| **Why** | Same class as routing cleanups. |
| **Acceptance** | At least `log::debug` / `warn` on non-fatal; fail loud on fatal. |

### P3-3 — Align `.agents/AGENTS.md` with block backend

| Field | Detail |
|-------|--------|
| **Status** | open |
| **Location** | Spec still mentions RustFS/S3-heavy design |
| **Why** | Agents/humans implement wrong backend. |
| **Acceptance** | Spec update: Garnet meta + NVMe block primary; progressive layout as implemented. |

### P3-4 — Lock-order & connection-scope guide in AGENTS

| Field | Detail |
|-------|--------|
| **Status** | open |
| **Location** | After P1-9 |
| **Why** | Prevent repeat regressions. |
| **Acceptance** | Short “must not” list for async+locks+Redis. |

### P3-5 — Profiling baseline

| Field | Detail |
|-------|--------|
| **Status** | open |
| **Location** | coz / dhat features |
| **Why** | Need before/after for P2 work. |
| **Acceptance** | Document one release-mode profile command set. |

### P3-6 — Criterion gates for crypto + concurrent locks

| Field | Detail |
|-------|--------|
| **Status** | open |
| **Location** | `benches/*` |
| **Why** | Catch perf regressions early. |
| **Acceptance** | Optional CI or documented nightly. |

### P3-7 — External suite smoke in CI notes

| Field | Detail |
|-------|--------|
| **Status** | open |
| **Location** | `tests/run_pjdfstest.sh`, `tests/run_elbencho_mount.sh` |
| **Why** | cargo tests miss full POSIX mount. |
| **Acceptance** | Document required root run after write-path changes. |

---

## Explicitly deferred (not “do now”)

- Wholesale rewrite of progressive layout state machine
- Changing default channel capacities **without** backpressure design (see **P1-1** / **P1-2**)
- Replacing Garnet with something else
- Full multi-NUMA runtime redesign

---

## Progress log

| Date | Notes |
|------|--------|
| 2026-07-02 | Task list created from post-audit findings. Completed earlier in session: nvme unaligned leak, FUSE write con scoping, cleanup logging, unaligned test, HPC_AUDIT removal. |
| 2026-07-02 | **P0-5 done**: `tests/routing_layout_tests.rs` + fix stale full-file LRU on oversized put / layout transition. |
| 2026-07-02 | **P0-2 done**: durable await + rollback before meta flip; P0-2 failure/retry tests; `FAIL_NEXT_WRITES` inject. |
| 2026-07-02 | **P0-3 done**: fsync propagates flush errors; writeback retry + sticky hard failures; writeback_durability_tests. |
| 2026-07-02 | **P0-1 done**: uring worker join-on-drop + exit drain of unaligned free_ptrs; shutdown stress tests. |
| 2026-07-02 | **P0-4 done**: lease re-validation, recovery binary meta fix, crash_consistency_tests. |
| 2026-07-02 | **P0-6 done**: MULTI/EXEC meta commits; lock SET-then-INCR; atomic_meta_tests. |
| 2026-07-02 | **P1 resource/backpressure**: P1-1..4,6,7,9,11 done; P1-5/8/10 partial/open. |
| 2026-07-02 | **P1-5 done**: bg_admit pool + striped read concurrency limits; bg_admit_tests. |
