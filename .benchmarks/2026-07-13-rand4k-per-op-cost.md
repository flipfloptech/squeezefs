# Item A — rand-4k per-op cost investigation (profiling-first)

**Branch:** `perf/rand4k-per-op` off dev@e73d0df.
**Commits:** `e1de2c0` (snapshot-independence pins) → `add13a5` (fix 1: Arc'd block map, CoW publishes) → `23e8c9f` (red: launch-time timeout pin) → `5de7347` (fix 2: memoized FUSE op timeout).
**Substrate note (honesty):** this session's sandbox is the baseline protocol's
file-backed volumes on the /home NVMe (4×8 GiB sqdata + 1 GiB sqmeta, caged 8G,
taskset 0-15, CPU capped 3.5 GHz — untouched). Raw artifacts: `~/tmp/rand4k/`
(`results_base/`, `results_fix1/`, `results/`, `perf_*.data`, ladder logs).

## 1. Same-session rows (tip = dev@e73d0df content at baseline)

| Row | Baseline (e73d0df) | Final tip (5de7347) |
|---|---|---|
| Cold rand-4k qd16×8t 30 s (`-r --rand -t 8 -b 4k --iodepth 16 --direct`) | 72,156 IOPS | 90,959 IOPS † |
| **Cold-row daemon CPU per op** | **75.7 µs/op** (163.9 s CPU / 2.16 M ops; 5.5 cores busy) | **30.9 µs/op** (−59%; user CPU −76%) |
| Warm hot-tier control (R-10 shape; device ledger 0) | 365,481 IOPS / 20.5 µs/op | 336–372 k IOPS (×4 runs) / 19.9 µs/op |
| Raw control (same substrate, no FUSE) | 391,103 IOPS | 371,888 IOPS |
| Cold-row amplification | 1.00× (8,457 MiB dev / ~8.3 GiB user) | 1.00× |

† **IOPS honesty:** the cold-row IOPS on this sandbox is *drive-state-noise
bound*, not daemon-bound. Interleaved remount ladder (same binary, alternating
Q_DEPTH): qd4 = {75.5 k, 42.0 k, 50.6 k}, qd16 = {52.5 k, 99.3 k, 60.7 k} —
±2× swings uncorrelated with the knob or the binary. The transferable win is
the **CPU column** (device-time-independent): the daemon now does the same row
on 2.25 cores instead of 5.5. On hardware where the daemon CPU was the binding
constraint (the closing report's 59.5 k row at 20% of the 294 k transport
ceiling), per-op CPU is the lever this row needed; on this sandbox the row
stays device-latency-bound after the fix, as the controls predict.

The early "Q_DEPTH=16 degrades the row" observation (73.5 k → 39 k) did NOT
survive the ladder — drive-state drift, recorded as noise, no causal effect.
Warm-row Q_DEPTH=16 pair: 343 k → 372 k (+8%, within band).

## 2. Attribution — cold profile (baseline binary, perf dwarf 997 Hz, 30 s row)

Daemon on-CPU self-cost (155 k samples):

| % | Symbol | Category |
|---|---|---|
| **15.70** | `HashMap<u32,String>::clone` | block-map deep clone |
| **7.97** | `drop_in_place<Option<HashMap<u32,String>>>` | block-map drop |
| 5.14 / 4.61 / 4.14 / 3.27 / 2.87 (**≈20**) | `_rjem_je_tcache_bin_flush_small` / `_rjem_malloc` / `_rjem_sdallocx` / `tcache_bin_flush_edatas_lookup` / `arena_cache_bin_fill_small` | jemalloc traffic serving the clones |
| 1.73 | `nvme_dev::worker_thread_loop` | block uring worker |
| 1.16 | `__vdso_clock_gettime` | timers/metrics |
| rest | kernel (scheduler, syscall entry, io_uring), long tail < 1% each | |

**≈ 44% of daemon CPU was the per-op deep clone of the 512-entry block map.**
Mechanism: `metadata_cache` is `moka::sync::Cache<String, CachedMetadata>` —
`get` returns the value **by clone**; `fetch_metadata` then cloned the owned
value AGAIN; `current_block_binding` (the R3/074-family binding recheck each
ranged serve) takes another moka get. 2–3 deep clones × 512 `String`s per
4 KiB op at ~72 k IOPS ≈ 75 M+ string allocs+frees/s.

## 3. What shipped (2 bounded fixes, red/pin-first)

**Fix 1 — share the block map (`add13a5`).**
`CachedMetadata.block_map: Option<Arc<HashMap<u32,String>>>` — clone is a
refcount bump; writers publish **copy-on-write** (`Arc::make_mut` / fresh map:
`merge_block_mappings`, promotion, spill, staged clip). `fetch_metadata`'s
redundant second deep clone deleted. `LayoutMetadata` (persisted form)
untouched — the deep copy survives only at the cold persist/rehydrate
boundary. Pinned first (`e1de2c0`, green through both implementations):
`test_block_map_snapshot_independent_of_{merge_publish,truncate_prune}` — a
held snapshot keeps exactly the map it was taken with across concurrent
publishes; the next fetch sees them. Sharing contract
(`test_block_map_clone_is_shared_until_publish`): `Arc::ptr_eq` shared until a
publish, CoW after.
**Measured:** cold-row daemon CPU **75.7 → 30.7 µs/op**; post-fix profile flat
(top symbol 2.9%; clone/drop/jemalloc block gone).

**Fix 2 — memoize the FUSE op timeout (`5de7347`).**
Warm-row profile showed `getenv` at 0.73% cycles: `get_fuse_timeout()` did
`std::env::var` (process-global env lock + alloc + parse) on **every** FUSE
op. Now `OnceLock`-memoized; `SQUEEZEFS_TIMEOUT` is a launch-time knob (was
never contracted otherwise). RED first (`23e8c9f`): a post-launch env mutation
changed the op timeout, proving the per-op read.

## 4. Warm (transport-ceiling) attribution — lever 2, investigation only

Post-fix warm profile (267 k IOPS under perf, 56 k samples) is **flat** — no
symbol ≥ 3%:
`__vdso_clock_gettime` 2.95 · syscall entry/exit ~1.7+1.0+0.99 ·
scheduler dequeue/pick ~2.4 aggregate · `reply_fuse` 1.14 ·
`queue_worker` 1.02 · `native_queued_spin_lock_slowpath` 0.85+0.82 ·
`getenv` 0.73 (fix 2 removes) · sip `Hasher::write` 0.73 ·
`Submitter::submit_and_wait` 0.71.
The 294–372 k ceiling is **distributed transport cost** (one uring commit
submit + wakeup + scheduler round trip per op across 8 queue workers + tokio),
not a single bounded hotspot. Nothing here qualifies as "clearly bounded" —
lever 2 goes to the structural menu.

## 5. Structural menu (NOT implemented — for sign-off)

| # | Proposal | Effort | Risk | Expected win |
|---|---|---|---|---|
| S1 | **Per-file binding snapshot cache keyed on map generation** (the closing report's named lever): one generation-validated snapshot per serve instead of fetch_metadata + current_block_binding double resolution | M | M — carries the 074-family reused-key discipline; needs its own red suite | ~⅓–½ of the remaining 30 µs/op cold CPU (moka get + sip hash + CachedMetadata field clones per op ×2) |
| S2 | **Commit batching in fuse3 over-uring queue_worker**: drain `commit_rx` fully, push all COMMIT_AND_FETCH SQEs, ONE `ring.submit()` per drain (today: submit per commit message) + same for un-parked commits | S–M | M — lease re-arm gate ordering must hold per ent; vendored-fuse3 change | fewer `io_uring_enter` syscalls/op on the reply path; syscall+sched ≈ 15–20% of warm profile; plausibly +10–20% on the 294–372 k ceiling |
| S3 | **SQPOLL on the FUSE rings** (`SQUEEZEFS_FUSE_IO_URING_SQPOLL_IDLE_MS/_CPU` knobs already exist): kernel-side SQ polling removes the submit syscall per commit | S (knob) | L — burns up to a core; needs a measured session | same target as S2; one quiet session to measure |
| S4 | **Coarse-clock metrics**: `Instant::now`/vdso clock ≈ 3% of both profiles (tokio timeout wrappers ×2/op + metrics latency records + moka TTI) | S | L | ~2–3% CPU/op |
| S5 | **FUSE passthrough mode** (kernel ≥ 6.9) for O_DIRECT striped reads: kernel serves reads from the backing fd directly after an open-time map handshake | L | H — architecture + safety model (leases, binding revalidation) change | approaches raw-control IOPS for uncached reads; eliminates the transport round trip entirely |

## 6. Acceptance (final tip)

| Gate | Result |
|---|---|
| Pins + sharing + memoization tests | green (independence pins green through BOTH implementations; sharing/timeout red-first) |
| Full serial cargo gate | **690 passed / 0 failed** (first roll hit the documented `test_kill9_remount_soak_v3` shutdown-drain flake at round 11 — 5/5 standalone green, test is KV-only vs a routing-only diff, lineage `91e9883`; count restarted per the multi-run directive, second roll fully green) |
| clippy -D warnings / fmt | clean |
| cargo doc --no-deps | 0 warnings |
| bench smoke | 118 ok |
| loom | 19/19 (no new cross-word atomics — Arc refcount only) |
| QUICK ×3 | **{003,213}-only ×3** (trio 616/075/091 + 074 green every run) |
| aged fsx ×3 (caged) | **3/3 CLEAN** |
| kill9 deep churn | SQUEEZEFS_CRASH_ROUNDS=60: ok |
| unmount kill soak | PASS — 30 cycles, 0 coredumps / 0 SIGABRT / 0 panics |
| LTP | **174 PASS / 0 FAIL / 0 BROKEN** (9 conf-skips) |
