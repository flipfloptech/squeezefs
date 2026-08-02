# 2026-08-04 — The derivation sweep: hardcoded resource limits → system-derived values

**Branch:** `refactor/derivation-sweep` off dev `68e8474`.
**Ruling (user, 2026-08-02, verbatim):** "remove any hard coded limitations
in favor of dynamically computed values based on available system
resources. i.e percentage totals of memory and cpu cores. i.e instead of a
2G cache a 10% cache of system memory. or instead of a fixed 4 threads,
50% of the cores available."
**Precedents applied:** the ipc-arena-cap resolve/precedence/tie pattern
(`.benchmarks/2026-08-02-ipc-cap-derivation.md`), the geometry-law tests
(`tests/transport_geometry_tests.rs`), `il_sessions_default` + drift-is-red
tie tests, the R5 budget (`src/mem_budget.rs`) as the derivation root.

**Law line added to AGENTS.md (portable-by-default section):** resource
caps derive from system resources; fixed clamps require a documented
physical/format/kernel/measured reason on the line; floors only as
physical minima or the never-regress-below-shipped posture; env precedence
absolute > percentage > derived; every derivation carries a tie test
(`tests/derivation_sweep_tests.rs`).

**Canonical shapes** (every derivation pinned on both):
- **Field:** 251 GB RAM ⇒ resolved budget ≈ 176 GiB (70 % default), 32
  CPUs, 4 MiB block volumes, 32 MiB journal rings.
- **Floor:** 4 GB / 2-CPU box ⇒ budget ≈ 2.8 GiB.

---

## 1. Phase 1 — the classification table

### Class A — DERIVE-NOW (converted in this sweep)

| # | Item (file:line at base) | Old | Derivation shipped | Field value |
|---|---|---|---|---|
| A1 | `TRANSPORT_BUFFER_CAP_CEILING` — `src/mem_budget.rs:586` + duplicate `crates/fuse3/src/raw/connection/fuse_over_uring.rs:1012` | `min(budget/8, 2 GiB)` | `budget/8`, ceiling **deleted** — the pinned-arena bound is the geometry's STRUCTURAL demand cap `nqueues × Q_DEPTH_DESIRED × payload_sz` (depth ≤ 32 by construction), so the ceiling's only effect was degrading depth on > 64-CPU big-RAM boxes. New envs `SQUEEZEFS_TRANSPORT_MEM_MAX` (MiB abs, A0 lever `2048`) > `SQUEEZEFS_TRANSPORT_MEM_PCT` > derived (`resolve_transport_buffer_cap`). Depth-degradation evidence RE-DERIVED, not discarded: the 32→4 ladder is untouched, now driven by the machine-derived fraction alone | 2 GiB → **22 GiB** (arena unchanged at 1 GiB demand on 32 CPUs; ≥ 96-CPU boxes regain depth 32) |
| A2 | `MAX_BACKGROUND_CEILING = 256` — `fuse_over_uring.rs:1008` | `clamp(q×d, 64, 256)` | `clamp(q×d, 64, u16::MAX)` — the ceiling is the INIT-reply **wire format** (u16, class B), the value is the delivered ring capacity (already machine-derived); floor 64 kept (never-regress, the dead-letter mount-option intent). `-o max_background=256` = the A0 lever. `squeezefs tune` made **raise-only** (it would have LOWERED ring-capacity mounts — sweep finding) | 256 → **1024** (32×32) |
| A3 | `il_sessions_default` ceiling literal 16 — `crates/squeezefs-ipc/src/sizing.rs:41`; shim `MAX_SESSIONS: usize = 32` — `crates/squeezefs-preload/src/interpose.rs:76`; env clamp `1..=16` | free-floating 16/32 | magnitude derived from the structural binder: `SESSION_REGISTRY_SLOTS = 32` (the shim's fixed static session table — alloc-free interposer law, class B with justification comment), ceiling = `SLOTS/2` (two concurrently-bound mounts' headroom); shim `MAX_SESSIONS` ties to the same constant; ties red on drift both sides. The R5-arena-math leg of the old 16 rationale dissolved with the 2 GiB session-shm ceiling (2026-08-02) | values identical (16/32) — the magnitude is now derived; lifting requires growing the registry (filed) |
| A4 | node-cache default 512 MiB — `src/meta_backend/kv/node_cache.rs:55`, resolver `backend.rs:722` | flat 512 MiB | `max(budget/16, 512 MiB)` per volume (`resolve_node_cache_budget`); floor = shipped posture; new `SQUEEZEFS_META_NODE_CACHE_PCT` spelling; absolute > pct > derived | 512 MiB → **11 GiB** |
| A5 | checkpoint dirty-node cap 4096 — `checkpoint.rs:654` (+ duplicate env read `src/config_ops.rs:1785`, now the same resolver) | flat 4096 | `max(4096, budget/32 ÷ node_size)` (`resolve_max_dirty_nodes`); floor = shipped posture (lower ⇒ more checkpoints than any shipped box — overhead, no RAM won); env verbatim | 4096 → **22528** (256 KiB nodes) |
| A6a | commit batch txs 64 — `backend.rs:198` | flat 64 | `max(64, cpus × 2)` (`resolve_commit_batch_txs`) — committer arrivals scale with handler parallelism (queues = possible CPUs); floor = shipped M7 posture | 64 → **64** (32×2 — field-identical) |
| A6b | commit batch bytes 256 KiB — `backend.rs:199` | flat 256 KiB | `max(256 KiB, ring_user_capacity/16)` (`resolve_commit_batch_bytes`) — a fixed fraction of ITS volume's ring so ≥ 16 batch reservations always cycle; senior admissible clamp unchanged | 256 KiB → **~2 MiB** (32 MiB ring) |
| A7 | `SQUEEZEFS_PATCH_MAX_BYTES` default 512 KiB — `src/fuse_client.rs:279` | flat 512 KiB | `block_size/8` (`derived_patch_max_bytes`, applied at mount; env verbatim incl. `0`) — the volume's own geometry is the denominator | 512 KiB → **512 KiB** (4 MiB block — field-identical) |
| A8 | `SQUEEZEFS_FOLD_MAX_BYTES` default 1 MiB — `fuse_client.rs:417` | flat 1 MiB | `block_size/4` (`derived_fold_max_bytes`, applied at mount) | 1 MiB → **1 MiB** (field-identical) |
| A9 | `SQUEEZEFS_IPC_ARENA_MB` default 64 — `fuse_client.rs:15371` (const `DEFAULT_ARENA_BYTES`, `crates/squeezefs-ipc/src/layout.rs:65`, stays as the floor) | flat 64 MiB/session | `max(64 MiB, pmd_align_down(admission_cap/128))` (`mem_budget::resolve_ipc_arena_bytes`) — /128 = per-uid cap (64) × 2 safety (a full per-uid population fits in half the cap); PMD round keeps the THP-collapse law; floor = shipped posture | 64 MiB → **176 MiB**/session |
| A10 | uring_fs worker pool `clamp(nproc, 4, 8)` — `src/uring_fs.rs:145` | ceiling 8, `available_parallelism()` | `clamp(cpus/4, 4, 64)` (`resolve_worker_count`) — cpus/4 = the measured ingest-economy drain slope; floor 4 = shipped floor; ceiling 64 = env-clamp parity. **Sweep finding fixed:** the site read `available_parallelism()` from whatever thread touched the Lazy first — the Hang-1 pinned-first-toucher poison; now `crate::cpu::process_parallelism()` | 8 → **8** (field-identical; 256-CPU boxes 8→64) |
| A11 | parked-write cap `MAX_ACTIVE_BLOCK_BUFFERS = 256` — `fuse_client.rs:3992` | flat 256 buffers | `max(256, budget/16 ÷ block_size)` (`resolve_parked_cap_buffers`, applied at mount; new env `SQUEEZEFS_PARKED_BUFFERS` abs-verbatim = A0 lever) — parked bytes are already an R5 component with shed + Red halving; floor = shipped posture | 256 → **2816** buffers (11 GiB worth) |

### Class B — FORMAT/PROTOCOL/PHYSICAL (keep; justification present or added on the line)

| Item | Why B |
|---|---|
| `SECTOR_SIZE 4096`, `NODE_PAGE 4096`, `MIN/MAX/DEFAULT_NODE_SIZE`, `RECORD_VALUE_CAP_CEILING 65536`, `XATTR_RECORD_ENVELOPE_MAX 256` (`kv/node.rs`, `kv/superblock.rs`) | on-disk format geometry / format-knob bounds |
| `JOURNAL_RING_MIN = 8 MiB` (`superblock.rs:180`) | on-disk liveness floor: must admit the largest whole-entry batch on any mount host |
| `ROOT_LEDGER_SLOTS 32` / `ROOT_LEDGER_SLOT_LEN 4096`, `STAMP_MAX_RUNS 128` / `STAMP_MAX_CURSORS 256`, `PENDING_FREE_CAP 65536` | on-disk ledger encoding budgets / replay-window contract |
| `layout.rs` `PAGE_BYTES/SLOT_BYTES/RING_CELL_BYTES/RING_TAIL_LINE_BYTES`, `MAX_ARENA_BYTES` (1 TiB sanity), `REQ_HEADER_SZ`, FUSE header sizes, kmbuf/zcrx/ethtool kernel ABI consts, `FUSE_MIN_READ_BUFFER 8192`, `PMD_BYTES 2 MiB`, `POOLED_BUF_ALIGN 4096` | wire/ABI/kernel/hardware facts |
| `u16` bound on INIT `max_background`/`max_pages` | INIT-reply wire format (now the ONLY max_background ceiling) |
| `SESSION_REGISTRY_SLOTS = 32` (`squeezefs-ipc/src/sizing.rs`, new home) | structural: the shim's fixed static session table (alloc-free interposer environment); documented on the line; every ceiling derives from it |
| `MAX_INLINE_SIZE 4096`, `CHUNK_SIZE 4 MiB` default block | on-disk layout thresholds / format knobs |
| routing width `2^16`, u16 slot namespace | on-disk namespace (design-dynamic-meta-routing) |

### Class C — MEASURED-CONSTANT (filed with mechanism thesis + validating re-bracket; NOT blind-converted)

| Item | Measured basis | Derivation thesis | Re-bracket that would validate it |
|---|---|---|---|
| `Q_DEPTH_DESIRED 32` / `Q_DEPTH_FLOOR 4` / `PAYLOAD_BASE 1 MiB` (fuse3) | L1 decomposition 44k→316k; floor/base = shipped postures | desired depth = f(completion latency × arrival rate) per queue | depth sweep ≥ 96-CPU box, ample budget (same window as the A1/A2 field A/B) |
| `MAX_BACKGROUND_FLOOR 64` | dead-letter mount-option intent, never-regress | none needed (floor) | — |
| `JOURNAL_RING_MAX 32 MiB` (`superblock.rs:183`) | replay-time bound (mount latency) | ceiling = target replay budget ÷ measured journal-apply bandwidth; **NOT format-host-RAM-derived — on-disk geometry must stay portable to smaller mount hosts** (pushback on the directive's class-A listing, recorded §4) | `meta_kv_replay_ms` vs ring-size sweep, loop + tcp substrates |
| `FOLD_MAX_EXTENTS 64` | G-gate `fold_fill` ≥ 16 amortization | trigger = the point where fold amortization crosses the measured floor | fold_fill histogram vs trigger sweep on the W2 rig |
| `REAP_EVENT_PARK_MAX 2`, `WAIT_SPINS_* 4096/256`, `SQUEEZEFS_IPC_SPIN_US = 0`, `SQUEEZEFS_IL_SESSIONS`-adjacent spin ladder | counted A/Bs (op-economy Phase B; reap-economy) | — | re-run the counted brackets on a new venue only |
| reclaim knobs `RECLAIM_BATCH_BLOCKS 64` / `BATCH_MS 2` / `LANES_PER_DEV 32` / `CAP_PARK_MS 1000` | write-wall width/coalesce experiments (2026-07-31) | `RECLAIM_QUEUE_MAX_BLOCKS 4096` could derive from aggregate data capacity (deferred thin-space debt, not RAM) | devsub-tcp rewrite rows, queue-cap sweep with `block_free_reclaim_cap_parks` as the verdict |
| ino-reclaim batch 64 / window 20 ms (`fuse_client.rs:16778`) | group-commit sector-merge shape | batch = f(inode-table sector clustering) | reclaim-storm bracket |
| `PENDING_TIMES_DRAIN_BATCH 128` / `_CAP 512`, `COMMIT_RETRY_BUDGET 256`, `SCAN_PAGE 512` | M6/M7 entry-economy shapes, protocol loop bounds | — | — |
| kernel TTLs 1 s, moka TTLs 300 s, `SQUEEZEFS_IPC_IDLE_SECS 300`, `INVAL_WINDOW_MS 1000`, heartbeat 10/45 | M-program per-class measurements / liveness protocol horizons (time, not capacity) | — | — |
| `READ_RANGED_THRESHOLD 256 KiB`, `READ_LANE_MIN_FILL_BYTES 256 KiB`, `NT_COPY_MIN`, `NT_READ_SERVE_MIN 256 KiB` | read-path program counted rows | — | re-bracket on the fio-gap venue |
| channel/queue caps: `URING_REQ_QUEUE_CAP 4096`, `URING_FS_QUEUE_CAP 4096`, `RING_ENTRIES 512`, `ADMIT_CAP 256`, `WRITEBACK_QUEUE_CAP 4096`, fold 1024, reclaim 100000, `STAGING_MERGE_QUEUE_CAP 1024`, evict channel 16384 / `EVICT_CHANNEL_BYTE_BOUND 256 MiB` | backpressure bounds (small structs; custody lives elsewhere) | scale with device count/offered concurrency | saturation soak with queue-depth gauges |
| `striped_block_concurrency` ceiling 64, `STRIPED_READ_CONCURRENCY 16`, bg_admit floor 32 | P2-6 fan-out bounds | ceiling = f(device queue depth) | striped fan-out sweep |
| write-pipeline `FLOOR_BLOCKS_PER_LANE 8` / `HEADROOM 3` / `BUDGET_CAP_DIVISOR 4` / `WINDOW_MS 250`, probe consts | probe-up-governor program (the depth itself is fully derived — the house model) | — | — |
| hot-tier min-shard 16 MiB, `SHARD_REPLACE_SLACK 64 KiB` | measured churn fix (refetch spiral) | min-shard = 4 × block_size | hot-tier churn rig |
| `fuse_uring_entries` default 1024 clamp [64, 4096] (`tokio.rs:32`) | classical INIT/notify rings only (not the hot path) | — | — |
| `per_uid_session_cap 64` (`fuse_client.rs`) | DoS posture; rider verified NOT the binder | — | — |
| `DEFAULT_RING_ENTRIES 1024`, `DEFAULT_MAX_OP_BYTES 1 MiB` (L4 slot/chunk geometry) | L4 economics; chunk ≈ kernel-lane max_write parity | max_op could track negotiated max_write | large-op il throughput vs chunk-size bracket |
| `FOLD_MEMO_CAPACITY 8`, `OVERLAY_TAIL_MAX 8` | design-fixed per-node bounds (§5.7 budget accounting relies on them) | — | — |
| nvmeof `DEFAULT_DPDK_MEM_MB 1024` / `DEFAULT_HUGEMEM_MB 2048` | external-stack (SPDK) defaults | — | — |
| stripe/shard geometry `BLOCK_LOCK_STRIPES 4096`, `SHARDED_ATOMIC_STRIPES 64`, fuse3 `SHARDS 64`, op-profile rings 256/512/1024 | contention-sized hash geometry, diagnostic rings (RAM-trivial) | — | — |

### Class D — KERNEL/EXTERNALLY-BOUND or already derived (documented, nothing to do)

R5 budget resolution (flag → env → cgroup×0.8 → 70 % RAM — the root);
read/write RAM LRUs 10 % RAM each; disk tiers 25 % of aggregate staging
capacity; hot tier `max(8 MiB, read_mem/4)`; `ipc_arena_cap` budget/8;
`READ_LANE_BUDGET_DIVISOR` budget/8; prefetch window derived +
`SHARE_PCT 50`; `READ_ADMISSION_FILL_PCT 5` (pct knob); transport queues
= kernel possible CPUs; `max_pages_limit` sysctl; page size sysconf;
fuse3 TPC lanes = process CPUs − 1; il/service `cpus/4` slope; bg_admit
`max(32, cores×8)`; striped `clamp(cores×2, 4, 64)`;
`reclaim_concurrency max(4, parallelism)`; `max_background_uploads
max(16, par×2)`; dir-entry/attr caches `max(k, RAM/c)`; format pool
`cores×2`; write-pipeline BDP/probe governor (NO fixed depth anywhere);
congestion threshold = ¾ (kernel's own ratio); killpriv/geometry
negotiated at INIT.

**Counts: A = 11 converted (12 sites), B ≈ 20 kept-justified, C ≈ 28
filed, D ≈ 20 documented.**

---

## 2. Phase 2 — red-first evidence

New contracts `tests/derivation_sweep_tests.rs` (11 tests, field + floor
shape per derivation, precedence + garbage-hygiene rows):

| Gate | Result |
|---|---|
| Red at base (API) | 13 errors: E0425 ×5 (`derived_patch_max_bytes`, `derived_fold_max_bytes`), E0432 ×8 (`resolve_transport_buffer_cap`, `resolve_ipc_arena_bytes`, `resolve_parked_cap_buffers`, `resolve_node_cache_budget`, `resolve_commit_batch_{txs,bytes}`, `resolve_max_dirty_nodes`, `resolve_worker_count`) |
| Red at base (behavioral) | `mem_budget_tests::transport_buffer_cap_is_budget_fraction` FAILED at `68e8474` (76 GiB budget → 2 GiB ceiling; expected 9.5 GiB) |
| Green post-fix | derivation_sweep_tests 11/11; fuse3 `cargo test --lib` 60/60 (plan-suite rows re-pinned to the ring-capacity law incl. the new 256-CPU/9.5 GiB depth-32 restoration row) |
| Tie tests | `sizing::ceiling_is_registry_derived` (ipc), `registry_ties_to_shared_structural_constant` (preload), the existing `sessions_default_ties_to_*` / `service_ceiling_default_ties_to_*` pairs unchanged-green |

Full gates: see §5.

---

## 3. Field-window A/B manifest (conversions that change behavior on the field shape)

All ship default-on with the old value reachable via the env override
(the A0 lever). Ride the next reformat/deploy window; instrument = stats
inode deltas + the standing rigs; A-B-B-A where the store ages.

| Row | Change on field | A0 lever | Verdict instrument |
|---|---|---|---|
| max_background 256 → 1024 | INIT-negotiated background concurrency ×4 (`transport_max_background`) | `-o max_background=256` | rand-4k iodepth rows (elbencho + fio), `fuse_op_watchdog_overdue` must stay 0 |
| transport cap 2 GiB → 22 GiB | no arena change at 32 CPUs (demand 1 GiB) — a >64-CPU client regains depth 32 | `SQUEEZEFS_TRANSPORT_MEM_MAX=2048` | `transport_{q_depth,payload_buffer_bytes}` + IOPS rows |
| node cache 512 MiB → 11 GiB/volume | meta working set stays RAM-resident | `SQUEEZEFS_META_NODE_CACHE_MB=512` | `meta_kv_node_cache_{hits,misses,evictions}`, find/stat storm rows |
| dirty cap 4096 → 22528 | fewer forced checkpoints on hot meta; replay set up to 5.5 GiB | env `=4096` | `meta_kv_checkpoints`, `replay_ms` on remount |
| commit batch bytes 256 KiB → 2 MiB | bigger conveyor batches on 32 MiB-ring volumes | env `=262144` | `meta_commit_group_{size,bytes}`, `journal_full_stalls` |
| parked cap 256 → 2816 | more parking before staging spill on bursty writes | `SQUEEZEFS_PARKED_BUFFERS=256` | `parked_full_buffer_bytes`, `mem_budget_level`, write_matrix parity |
| ipc arena 64 → 176 MiB/session | deeper il pipelines; session footprint ×2.75 (48 HELLOs ≈ 8.3 GiB of a 22 GiB cap) | `SQUEEZEFS_IPC_ARENA_MB=64` | `ipc_bind_refused_budget` delta 0, il engagement ≥ 0.90, write_matrix il rows |

No cluster work performed in this sweep (isolated worktree; the standing
coordination rule).

## 4. Pushbacks + findings (audit surprises)

1. **Journal ring clamps are NOT class A** (directive listed them to
   confirm): the ring is ON-DISK format geometry — deriving its ceiling
   from format-host RAM would bake host resources into a volume that must
   mount on smaller hosts. Floor 8 MiB reclassified B (liveness);
   ceiling 32 MiB reclassified C with the replay-time thesis (§1 table).
2. **`squeezefs tune` would have LOWERED ring-capacity mounts**: it wrote
   max_background=256 unconditionally to every live fusectl connection —
   correct under the old ceiling, a downgrade under the derived law.
   Fixed raise-only (< 256 lifts to 256/192; ≥ 256 untouched).
3. **`uring_fs::worker_count` carried a live Hang-1-pattern bug**: it
   sized from `available_parallelism()` — the calling thread's mask — and
   the `URING_FS` Lazy is routinely first-touched from a core-pinned
   runtime worker, collapsing the pool to the floor on pinned mounts.
   Now `crate::cpu::process_parallelism()` (the documented law).
4. **`docs/operations.md` said `SQUEEZEFS_IPC_SERVICE_THREADS` "default
   2"** — stale since the ingest-economy campaign (default is the shared
   derivation). Corrected in the same doc pass.
5. `MAX_OP_BYTES`/`IDLE_SECS` (named in the directive's class-A list)
   audit as C: the former is L4 slot/chunk geometry entangled with slab
   math (thesis filed — track negotiated max_write), the latter is a
   time-horizon, not a resource cap (R5 shed already reaps under
   pressure).

## 5. Verification

| Gate | Result |
|---|---|
| `cargo clippy --all-targets --all-features -- -D warnings` (root) | clean |
| `cargo clippy --all-targets --all-features -- -D warnings` (fuse3 root) | clean |
| `cargo fmt --check` (both roots) | clean |
| `cargo test --all-features -- --test-threads=1` (full root suite, from zero) | pass |
| fuse3 standalone suite | pass (lib 60/60 + suites) |
| squeezefs-ipc / squeezefs-preload suites | pass |
| `cargo doc --no-deps` | clean |
| `cargo bench --benches -- --test` (criterion smoke) | pass |
| Field A/B rows | deferred to the reformat window (§3 manifest) |
