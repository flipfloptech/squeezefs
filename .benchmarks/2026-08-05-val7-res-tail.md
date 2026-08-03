# VAL-7 + RES-remainder — the §3 and §7 tails

**Branch:** `fix/val7-and-res-tail` (base: `dev` @ `4302b22f`, 162 commits past `402ca77`)
**Scope:** pre-RC engineering spec §3 **VAL-7a–7i** and §7 **RES-12, RES-17, RES-18, RES-19, RES-20**.
**Not in scope:** RES-16's remaining half (deferred to DLM S3 — a seam redesign), and the §7 positive result (the `await_holding_lock` class and the traced lock ordering are CLEAN and were left untouched).

Box: the standing dev box (thermally capped — relative truth only). Instrument: Criterion (`harness = false`), `cargo bench --all-features`. Every row states its own shape in-file.

---

## 1. What landed

| Item | Disposition |
|---|---|
| VAL-7a | **Landed.** `.stats`/`.config` are `0400` owned by the mount uid (`fuse_client::VIRTUAL_INODE_MODE`); the key census is opt-in behind `SQUEEZEFS_STATS_KEY_CENSUS=1`; census-free count gauges always export. |
| VAL-7b | **Landed.** Staging/read-cache dirs `0700`, segment rings `0600`, one policy point (`config_ops::create_private_dir_all`); the format-grade stamp applies mode + ownership through one `O_DIRECTORY\|O_NOFOLLOW` fd. |
| VAL-7c | **Landed.** The ADMIN lane runs the data plane's version → nonce → peercred ladder (`AdminHello` gained `abi`/`build_commit`/`nonce`; `IPC_ABI` 2 → 3), and `--admin-uid` / `-o admin_uid=N` states the administering identity explicitly. |
| VAL-7d | **Closed by explicit posture statement** (the item's sanctioned alternative) — `docs/operations.md` §"Multi-user mounts — the single-tenant resource posture". |
| VAL-7e | **Landed.** `copy_probe_block_range` bounds the staged-sibling probe to the copied extent. |
| VAL-7f | **Landed.** `read_caller_struct` — pid-liveness + pid-namespace checked, `/proc/<pid>` **dirfd-pinned**. |
| VAL-7g | **Landed.** `validate_nqn_component` refuses `.`, `..` and any leading-dot component. |
| VAL-7h | **Landed (remaining half).** `--log-file` opens are `0600` + `O_NOFOLLOW` at all three sites. `zeroize`, `PR_SET_DUMPABLE(0)` and `RLIMIT_CORE=0` were **verified already delivered** by the VAL-3/KW-1 wave (`src/keyfile.rs:268 harden_process_memory`, called from both key-load paths; `src/crypto_compress.rs` + `src/keyfile.rs` `Zeroizing`) — skipped as instructed. |
| VAL-7i | **Landed (remaining half).** Root subprocess `PATH` sanitized once at startup. The worktree-state check the item asks for **already existed** (`git status --porcelain --ignore-submodules=dirty` in `verify_checkout`, landed `a2ada701` as the FIND-N3-B fix) — the spec's anchor text is stale on that half. |
| RES-12 | **Landed** at the teardown sites (trace below). |
| RES-17 | **Landed** — the redundant queue DELETED (consumer decision below). |
| RES-18 | **Closed by recorded reasoning** (the item's sanctioned alternative) — on `TpcScheduler`'s doc comment. |
| RES-19 | **Landed.** Bounded ring (`FREE_FORENSICS_TAPE_CAP = 4096`). |
| RES-20 | **Landed.** `try_send` enqueue; one shared drainer under backpressure. |

---

## 2. RES-12 — the teardown trace

The prior agent left this saying the fix is at the teardown SITES and that it could not establish which paths are reachable from an async context without tracing dismount + session-reap ordering. That trace:

### `Drop for UringWorker` (`src/nvme_dev.rs`)

Owner chain: `NvmeBlockDev { worker: Arc<UringWorker> }`. Every `Arc<NvmeBlockDev>` holder — `DataRouter`, `TieredCache`, each `StorageBackend` in `BackendRouter.backends` — keeps it alive, so the join fires wherever the LAST clone drops. Enumerated:

| Site | Async context? | Live daemon? | Disposition |
|---|---|---|---|
| `config_ops::probe_data_volume_rw` (`src/config_ops.rs`) | **yes** — constructs + drops inside one `async fn` | **yes** — reached from `SqueezefsFilesystem::admin_add_data_volume`, i.e. the admin-lane `volume-add-data` verb on a serving mount | **Hopped + awaited.** The outcome is captured first so every exit path (including the three error arms) hands the device to `drop_off_runtime`; awaited because the caller publishes a durable record naming the device right after. |
| `BackendRouter::retire_backend` (`src/routing.rs`) | **yes** — a sync fn called from the VL4 retire path inside async job/admin code | **yes** — retirement is an online verb | **Hopped, not awaited.** `remove` now yields the `Arc<StorageBackend>` and it is handed to `drop_off_runtime`; retirement is complete once routing no longer names the volume, and the worker's exit cleanup is private. |
| the daemon's `default_device` + per-volume backends | at process exit, after `mount_squeezefs` returns | — | **Left inline, deliberately.** Nothing else needs that worker thread at that point; there is no victim. |
| offline verbs — `fsck`, `defrag`, `config_ops::add_data_volume`, `main.rs`'s one-shot device | end of an `async fn` in a one-shot process | no | **Left inline, deliberately.** Same reasoning: the process is about to exit. |

Join cost bound: an idle worker parks in `rx.recv()` and wakes on channel disconnect immediately; a worker with in-flight SQEs completes them first, bounded by device latency (the same bound MEM-1's 30 s timeout guards). So the hazard is real but bounded — which is why the fix is placement, not restructuring.

**Why `Drop` keeps its synchronous join.** Deferring it would make teardown ordering non-deterministic, and P0-1's contract is exactly that exit cleanup (freeing unaligned bounce buffers) has run when `Drop` returns. `tests/dismount_teardown_tests.rs` depends on the drain being finished, so the impl is unchanged and the trace is recorded on it.

### `Drop for DataPlaneSink` (`src/ipc_service.rs`)

Owner chain: `Arc<DataPlaneSink>` → held by `IpcHost.sink` → `IpcHost` held in `fs.ipc_host` (an `ArcSwap` shared across every `SqueezefsFilesystem` clone). So the *only* reachable drop is when the last filesystem clone drops — i.e. process exit, on whichever tokio worker gets there. `IpcHost::shutdown()` does **not** drop the sink (it is a field), so the join was guaranteed to land off the existing hop.

The daemon already calls `host.shutdown()` inside `tokio::task::spawn_blocking` (`src/fuse_client.rs`, the teardown block). So: a new `SessionSink::shutdown_threads()` (default no-op) is invoked at the end of `IpcHost::shutdown`, `DataPlaneSink` implements it with the same `shutdown_engine()` the `Drop` calls, and `DirectDriveEngine::shutdown`'s existing `shutting_down.swap(true)` gate makes the pair idempotent (the second call returns before touching the ring; the reaper handle is already taken). Net effect: the reaper join moves onto a blocking-pool thread on every real teardown, and `Drop` remains a correct backstop for paths that never run a host shutdown (tests, error unwinds) — so `run_preload_gate.sh` leg 2's kill-9 / fork-kill-parent soak semantics are unchanged.

`detached::drop_off_runtime` is the shared primitive: inside a runtime it returns the `JoinHandle` of a blocking task that performs the drop; outside one it drops inline and returns `None` (offline verbs and `Drop` backstops must not require a reactor).

---

## 3. RES-17 — the consumer decision

The prior agent's note was that the fix must preserve what `active_keys()`'s three consumers rely on for ordering, and that the field doc ("the map is the authority; the queue is bookkeeping") suggests changing the CONSUMERS. The call: **neither index-based removal nor a consumer change — delete the queue.**

Evidence:

1. **No consumer depends on order.**
   * `NvmeCache::offline_device` — iterates the keys and moves *all* of them off the device. Order-irrelevant.
   * `NvmeCache::online_device` rebalance — takes `i % share_fraction == 0`, i.e. an arbitrary 1/N sample. Any stable enumeration works.
   * `NvmeCache::list_keys` → `NvmeStaging::list_{staged_files,cached_blocks}` → the `.stats` census. Already a cross-shard concatenation (no meaningful global order), and its only real consumer (`squeezefs umount`) takes `.len()` — which after VAL-7a reads a count gauge instead.
2. **Nothing ever popped the front.** `grep pop_front` over `src/tiering/nvme.rs` is empty. The front-run eviction walk that once needed FIFO order was deleted by the geometry-complete eviction fix — whose own comment is the field doc quoted above, and whose bug (a same-key replace / out-of-order placement desynchronising queue order from ring order, so an overlapping live entry sat deeper in the queue and got its bytes clobbered) was *caused* by treating the queue as authoritative.
3. **So the queue was pure cost plus a live bug class.** Keeping two containers agreeing across replace / evict / recover is exactly the invariant that produced the generic/074 fstest.3 stale-fill corruption.

`active_keys()` now derives from `self.map.keys()`; `entry_count()` was added for the count-only callers. Removals (`evict_overlapping`, `put`'s same-key replace, `reserve_and_write`'s flip, `remove`, `recover_index`'s dedupe) lost their `retain` entirely — the O(n) scan **and** the O(n) shift, both under the shard WRITE lock. Contract pinned by `tests/res_tail_tests.rs::staging_shard_active_keys_track_the_map_exactly` (census tracks the map exactly; a removed key never lingers; a replace never duplicates or grows the census).

---

## 4. RES-18 — the recorded reasoning

Recorded on `TpcScheduler`'s doc comment (`crates/fuse3/src/raw/session.rs`), in short:

* **The ring lane's incidental bound IS a transport-geometry bound.** A request delivered over the ring holds its ent slot from delivery until its reply commits, and the handler future's lifetime is contained in that window ⇒ concurrently resident over-uring handler futures ≤ `queues × q_depth`, the transport's own registered slot count (the number `max_background` derives from). Backpressure is applied where it belongs: the kernel stops delivering when every slot is out, so nothing queues in the channel.
* **Bounding the sideband would be actively harmful.** It carries FORGET/BATCH_FORGET **and INTERRUPT** on ONE serialized reader. A full bounded channel blocks the dispatch loop, stalls the reader, and delays exactly the INTERRUPT deliveries that exist to unstick requests — converting a memory-pressure event into a liveness failure. Its real bound is the reader: one request in flight per `Readv`, kernel-side coalescing into BATCH_FORGET (one future for many inos), and — since RES-20 — a daemon-side reclaim enqueue that spawns nothing per FORGET.
* **The instrument that would catch a real problem already exists**: `transport_requests_abandoned` (must stay 0) plus `TpcScheduler::dispatch`'s dead-lane re-dispatch. If a measurement ever shows lane residency mattering, the bound to add is `queues × q_depth` on the SIDEBAND dispatch alone.

---

## 5. Microbenches (mandatory where a hot path changed)

### RES-20 — the FORGET enqueue (`high_concurrency_bench` / `reclaim_enqueue`)

Shape: a 1 024-ino burst (the BATCH_FORGET / `drop_caches`-storm class), measured inside a multi-thread runtime — the daemon's venue. `spawn_per_ino_burst_1024` is the pre-fix A0 control (one `tokio::spawn` per ino whose whole body is a channel send).

| Row | time (1 024 inos) | per ino | thrpt |
|---|---|---|---|
| `try_send_burst_1024` (**shipped**) | **47.56 µs** | **46.4 ns** | 21.53 Melem/s |
| `spawn_per_ino_burst_1024` (pre-fix A0) | 994.88 µs | 971 ns | 1.03 Melem/s |

**≈ 20.9× faster on the enqueue**, and — the point of the item — zero tasks land on the current fuse3 handler lane's `LocalSet` instead of one per forgotten inode.

### RES-17 — staging-shard drain (`write_path_bench` / `staging_shard_removal`)

Shape: one anonymous shard holding `N` × 4 KiB-class staged extents (the W2 parked-extent size — the shape that actually produces high per-shard key counts), **drained** and reported per key. That is the honest shape for the claim: with the queue, draining N keys from an N-occupancy shard was O(N²) (each `retain` scans *and shifts* the remaining queue); map-only it is O(N), so the **per-key** figure must be flat.

| Occupancy | drain time | **per key** | thrpt |
|---|---|---|---|
| 64 | 75.02 µs | **1.17 µs** | 853.1 Kelem/s |
| 1 024 | 1.5624 ms | **1.53 µs** | 655.4 Kelem/s |
| 4 096 | 6.6134 ms | **1.61 µs** | 619.3 Kelem/s |

**Per-key cost is flat** (853 → 655 → 619 Kelem/s across a 64× occupancy range — the mild slope is page-fault/locality over a larger mapping, not algorithmic). Total is linear in N. The pre-fix shape would have paid ~N²/2 `Bytes` comparisons *plus* N²/2 element shifts on the 4 096 drain (≈ 8.4 M of each), all under the shard write lock.

Two measurement traps this row hit and now documents in-file: the shard must be **returned** from the routine (`iter_batched` drops outputs outside the measured region — dropping it inline timed the occupancy-sized mmap teardown and produced a fake super-linear curve: 20.9 µs → 632 µs → 2.53 ms), and the drain must be **per batch** rather than one key per iteration (a per-iteration batch holds many live mappings and charges their page-fault cost to the removal: 1.5 → 15.2 → 35.6 µs, also fake).

### VAL-7e — the copy probe range (`write_path_bench` / `copy_probe_range`)

Shape: a 1 TiB source at the shipped 4 MiB block (262 144 blocks). `loop_chunk` is the fix's scan for a 1 MiB `copy_file_range`; `loop_whole_file` is what the pre-fix code did for **every** call, capped at 4 096 iterations so the bench terminates.

| Row | time | note |
|---|---|---|
| `range_1mib_chunk` (the arithmetic) | **12.22 ns** | the bound itself is free |
| `loop_chunk` (**shipped**: 1 block) | **83.26 ns** | one `active_block:` key + probe |
| `loop_whole_file` (pre-fix, 4 096 blocks) | **372.44 µs** | ≈ 11.0 Melem/s |

The shipped scan is **≈ 4 470× cheaper** than the same call's pre-fix scan at the bench's 4 096-block cap — and the real pre-fix scan on this file is 262 144 blocks (64× the cap) **× 4 passes per call**, i.e. ≈ 95 ms of pure probe work per 1 MiB `copy_file_range`, plus a `Vec<u32>` proportional to the file rather than the request.

### Bench smoke

`cargo bench --all-features --bench write_path_bench -- --test` and `--bench high_concurrency_bench -- --test`: every new row `Success`.

---

## 6. Measured rows (raw)

Dev box, `--all-features`, Criterion 100-sample estimates `[lower median upper]`. Same-box relative truth only (the box is thermally capped).

```
reclaim_enqueue/try_send_burst_1024        [ 47.429 µs   47.559 µs   47.734 µs ]
reclaim_enqueue/spawn_per_ino_burst_1024   [917.34  µs  994.88  µs    1.0725 ms ]

staging_shard_removal/drain_64_live_keys    [ 71.809 µs   75.021 µs   79.513 µs ]
staging_shard_removal/drain_1024_live_keys  [  1.4847 ms   1.5624 ms   1.6450 ms ]
staging_shard_removal/drain_4096_live_keys  [  6.4167 ms   6.6134 ms   6.8169 ms ]

copy_probe_range/range_1mib_chunk           [ 12.186 ns   12.219 ns   12.255 ns ]
copy_probe_range/loop_chunk                 [ 83.035 ns   83.264 ns   83.478 ns ]
copy_probe_range/loop_whole_file            [357.44  µs  372.44  µs  388.80  µs ]
```

---

## 7. Items closed by a written posture (both sanctioned by the spec item)

### VAL-7d — the multi-tenant posture call

**Call: state the single-tenant posture explicitly.** `docs/operations.md` gained §"Multi-user mounts — the single-tenant resource posture", and AGENTS.md's Stats-surface section points at it.

Why not per-uid accounting: the design target (ruling D1, 15 000+ nodes of AI-training mixed workloads) scales by NODES — each node runs its own daemon over its own mount — so cluster multi-tenancy is a *scheduler* property (one mount per job/container), and per-uid R5 sub-budgets plus a per-uid admission gate would add a per-op charge to the hot path for a shape the target does not have. Building it speculatively would also violate the derivation/measurement law: it would ship without a bracket.

What the statement says, precisely: every R5 component is process-global, so one workload driving the budget to Red pauses maintenance, tier publishes and dehydration for all users of that mount (visible in `mem_budget_{level,red_events}`, `job_paused_mem_pressure`, `read_tier_publishes_paused`); nothing rate-limits a uid's op stream (the bounds that exist — transport slots, the BDP write admission target, the writeback and reclaim queues — are global and structural); the only per-uid things are the interception control plane's `per_uid_session_cap` and the `ipc_session_arenas` shed, neither of which bounds the FUSE path; and — stated separately, because it is the part that IS enforced — confidentiality/integrity are multi-user-safe (`default_permissions`, kernel-arbitrated POSIX locks, the reserved-xattr screen, and now VAL-7a/7b/7c).

### RES-18 — see §4.

---

## 8. Drifted spec anchors

Located by SYMBOL. Every §3 VAL-7 and §7 RES anchor in the spec table, with its stated line and the line the symbol actually sits on in `dev` @ `4302b22f` (pre-change):

| Spec anchor | Stated | Actual | Drift |
|---|---|---|---|
| VAL-7a `src/fuse_client.rs:5185` (`get_stats_attr`) | 5185 | **5618** | +433 |
| VAL-7a `:6322` (`get_config_attr`) | 6322 | **6845** | +523 |
| VAL-7a `:5359` (`generate_stats_json`) | 5359 | **5642** | +283 |
| VAL-7b `src/tiering/nvme.rs:397` (`NvmeShard::new`) | 397 | **396** | −1 |
| VAL-7b `src/cache/nvme.rs:95` (`write_staging_generation_marker`'s `create_dir_all`) | 95 | **95** | 0 |
| VAL-7b `src/cache/nvme.rs:938` (the segment-dir creates) | 938 | **1014** (`create_dir_all(&rc_dir)`) | +76 |
| VAL-7b `src/config_ops.rs:115-121` (the stamp's `chown`) | 115–121 | **326** (`std::os::unix::fs::chown(dir, …)`) | +205..+211 |
| VAL-7c `src/ipc_host.rs:1380-1402` (`admin_loop`) | 1380–1402 | **1694** | +292..+314 |
| VAL-7c `src/config_ops.rs:81` (`invoking_owner`) | 81 | **81** | 0 |
| VAL-7e `src/fuse_client.rs:13946-13975` (the probe loop's `Vec<u32>`) | 13946–13975 | **15164** | +1189..+1218 |
| VAL-7f `src/fuse_client.rs:14654-14675` (`/proc/<pid>/mem`) | 14654–14675 | **16051** | +1376..+1397 |
| VAL-7g `src/nvmeof/mod.rs:295` (`validate_nqn_component`) | 295 | **295** | 0 |
| VAL-7g `src/nvmeof/nvmet.rs:190` (`remove_configfs_object`) | 190 | **190** | 0 |
| VAL-7h `src/main.rs:2576` / `:2829` (the log-file opens) | 2576, 2829 | **2336, 2615, 2926** (THREE sites, not two) | +/−; and the item under-counts the sites |
| VAL-7h `src/crypto_compress.rs:133-137` (key material) | 133–137 | already `Zeroizing` + `harden_process_memory` (`src/keyfile.rs:268`) | **stale — delivered** |
| VAL-7i `src/nvmeof/spdk/lifecycle.rs:796-812` (`verify_checkout`) | 796–812 | **796** | 0 — but the item's premise is **stale**: the worktree check it asks for is at `:821` (`git status --porcelain --ignore-submodules=dirty`, landed `a2ada701`). Only the `$PATH` half was open. |
| RES-12 `nvme_dev.rs:196` (`Drop for UringWorker`) | 196 | **333** | +137 |
| RES-12 `ipc_service.rs:341` (`Drop for DataPlaneSink`) | 341 | **433** | +92 |
| RES-17 `tiering/nvme.rs:237` (the field) | 237 | **180** (field) / 237 (`evict_overlapping`'s `retain` — the stated line is one of the USE sites, not the declaration) | −57 for the field |
| RES-17 `tiering/nvme.rs:578` | 578 | **578** | 0 |
| RES-17 `tiering/nvme.rs:837` | 837 | **837** | 0 |
| RES-18 `crates/fuse3/src/raw/session.rs:4668` | 4668 | **4967** (the lane `unbounded_channel`) | +299 |
| RES-18 `:4729` | 4729 | **5028** (`spawn_local`) | +299 |
| RES-19 `block_allocator.rs:11` (`free_forensics_tape`) | 11 | **10** | −1 |
| RES-19 `block_allocator.rs:888` | 888 | **890** (`begin_free`'s arm) / **923** (`finish_free`'s) | +2 / +35 |
| RES-20 `fuse_client.rs:4578-4589` (`queue_reclaim_inode`) | 4578–4589 | **5030** | +452..+463 |

Pattern worth recording for the next agent: `src/fuse_client.rs` anchors have drifted by **+280 to +1400 lines** (the file grew past 18 500), `src/ipc_host.rs` by ~+300, and `crates/fuse3/src/raw/session.rs` by a uniform **+299**. `src/tiering/nvme.rs`, `src/nvmeof/*` and `src/config_ops.rs`'s early lines are essentially stable. **Two anchors are not drift but staleness** — VAL-7h's zeroize/dumpable clause and VAL-7i's worktree clause describe work already in the tree.

---

## 9. Gates run

* `cargo build --all-features --all-targets` — clean.
* `cargo clippy --all-targets --all-features -- -D warnings` — clean (root + `crates/fuse3`).
* `cargo fmt --check` — clean (root + `crates/fuse3`).
* `cargo doc --no-deps --all-features` — no NEW warnings (the two private-intra-doc-link warnings this branch introduced were fixed by de-linking; the pre-existing ones in `assembly_tasks` / `cache/pool` are untouched).
* `cargo bench --benches -- --test` (smoke) — every new row `Success`.
* Targeted suites, `--test-threads=1`, all green: `val7_access_control_tests`, `val7_admin_lane_tests`, `val7_path_validation_tests`, `res_tail_tests`, `gds_ioctl_range_tests`, `dismount_teardown_tests`, `placement_tests`, `forget_sweep_tests`, `job_fabric_tests`, `ipc_host_tests`, `preload_authn_tests`, `preload_session_tests`, `copy_file_range_tests`, `nvmeof_grammar_tests`, `daemon_logging_tests`, `cache_path_policy_tests`, `staging_wipe_guard_tests`, `mount_owner_override_tests`, `staging_generation_tests`, `staging_budget_tests`, `staging_shard_deadlock_tests`, `memory_shard_tombstone_tests`, `metrics_tests`, `status_shape_tests`.

Per the assignment: **no full suite, no merge, no push, `squeeze-test` never touched.**

### One bug found and fixed inside this pass

The first cut of `create_private_dir_all` used `O_DIRECTORY | O_NOFOLLOW`. The mount layout deliberately makes `<isolated>/cache_segment` a **symlink** to `../cache_segment` (the read cache is shared across mounts of one staging root), and `O_DIRECTORY|O_NOFOLLOW` on a symlink is `ENOTDIR` — so **every mount with a staging root failed** with `Io(Os { code: 20, kind: NotADirectory })`. Caught by `tests/cache_path_policy_tests.rs` (2 of 6 red), fixed by scoping `O_NOFOLLOW` to the format-grade stamp (where the path is operator-supplied and the chown runs as root) and pinned forever by `tests/val7_access_control_tests.rs::private_dir_helper_traverses_the_shared_cache_segment_symlink`.
