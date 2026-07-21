# AGENTS.md — Squeezefs One Source of Truth

Squeezefs is a high-performance distributed POSIX FUSE filesystem (Rust + tokio + io_uring) with a decoupled block-based logical volume metadata store backend (MetaLV) and an NVMe / NVMe-oF block data backend. Linux-only.

**This combined document is the single authoritative reference.** It merges architectural rules, non-negotiables, development workflow, build/test gates, profiling, and repo conventions. Read it before touching core logic.

---

## Non-Negotiables

### Always use io_uring when we can

**Policy for agents and humans:** prefer and require **io_uring** for every I/O path where the Linux kernel can do it. Do **not** “temporarily” fall back to classical `read`/`write`/`pread`/`pwrite`/`/dev/fuse` polling as a way to unblock a bug. Fix the uring path, or fail loud.

| Path                              | Expectation |
|-----------------------------------|-------------|
| **FUSE request hot path**         | **FUSE-over-io_uring only** after arm (`REGISTER` / `COMMIT_AND_FETCH`). No userspace opt-out. Mount fails if setup fails. |
| **FUSE_INIT + classical sideband** | Classical `/dev/fuse` for `FUSE_INIT` (kernel requires `fch->initialized` before REGISTER), then the **kernel-mandated classical sideband** only: the kernel keeps FORGET/BATCH_FORGET + INTERRUPT + `fuse_resend` resends + `fiq->ops` switchover stragglers on the classical queue even when armed (fs/fuse/dev_uring.c). A dedicated sideband session services them — **via io_uring `Readv`** — or they strand forever (`waiting ≥ 1`, umount EBUSY). All other requests stay over-uring. |
| **NVMe / block data**             | `NvmeBlockDev` io_uring workers (fixed files when available). |
| **Ad-hoc local files**            | `crate::uring_fs` (not std file APIs) where practical. |
| **Not uring**                     | TLS peers, network TCP/TLS stacks. Staging **mmap** segments stay mmap by design. |

If over-uring or block uring misbehaves: **debug and fix uring** — never reintroduce a classical escape hatch “just to make tests pass.”

**FUSE uring knobs (env / mount)**

| Variable | Effect |
|----------|--------|
| `SQUEEZEFS_FUSE_IO_URING_SQPOLL_IDLE_MS` | Enable SQPOLL with idle timeout (ms) |
| `SQUEEZEFS_FUSE_IO_URING_SQPOLL_CPU` | Pin SQPOLL kernel thread |
| `SQUEEZEFS_FUSE_IO_URING_ENTRIES` | SQ depth for classical FUSE rings (INIT/notify) |
| `SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH` | Entries per FUSE-over-io_uring queue — explicit value wins verbatim (clamp 1..32). **Default = L1 policy** (2026-07-15 IOPS-parity): desired **32**, degraded to fit the payload-buffer cap `min(mem_budget/8, 2 GiB)`, floor 4 (= pre-L1 posture) |
| `SQUEEZEFS_FUSE_OVER_IO_URING_QUEUES` | Number of queues (**testing only**; default = kernel **possible CPUs**, clamp 1..512 — fewer than possible CPUs never becomes ready) |
| `-o max_background=N` / `-o congestion_threshold=N` | Mount-level INIT-reply overrides. Defaults: `clamp(queues × depth, 64, 256)` and ¾ of it (pre-L1 the INIT reply hardcoded 12/9 — one of the two multiplicative in-flight gates; runtime-writable per connection via fusectl) |

Mount always enables FUSE-over-io_uring after INIT (required). Expects: "FUSE-over-io_uring ready..." and "transport enabled for this session". Transport geometry + INIT limits are gauged on the stats inode (`transport_{queues,q_depth,payload_buffer_bytes,max_background}`), and the payload arenas ride the R5 memory budget as the non-sheddable `transport_payload_buffers` component. Evidence: `.benchmarks/2026-07-15-iops-parity-decomposition.md` (44k → 316k measured) + `.benchmarks/2026-07-15-l1-transport-concurrency.md` (defaults acceptance).

**io_uring coverage**

| Path | Mechanism |
|------|-----------|
| Primary block device R/W | `NvmeBlockDev` worker + fixed-file registration when supported |
| Ad-hoc file R/W / fdatasync | `crate::uring_fs` process worker |
| Mmap page hint | `IoUringPrefetcher` (`MADV_WILLNEED`) |
| FUSE transport (default) | `fuse3` `BlockFuseConnection` (classical rings during INIT); **FUSE-over-io_uring** (`IORING_OP_URING_CMD` + REGISTER/COMMIT_AND_FETCH) after arm; post-arm **classical sideband session** (io_uring `Readv` on `/dev/fuse`) for kernel-mandated FORGET/INTERRUPT/resend traffic + switchover stragglers |
| FUSE_WRITE payload delivery | Zero-copy **payload lease** (`Bytes::from_owner` over the registered uring payload buffer) with deferred COMMIT_AND_FETCH re-arm; the lease-severance boundary bounds every lease to one handler invocation — `docs/design-zero-copy-write-path.md` §5.4 |
| Staging / read-segment hot path | **mmap** (by design — zero syscall) |
| TLS peers | Not uring (network) |

Hardening: pool not marked ready until all queues submit REGISTER; per-qid commit channels; shared inbound queue; eventfd wake; full-size payload buffers.

### No dead code

**Do not leave unused code in the tree.** Agents and humans must remove it, not silence it.

- **Delete** unused functions, methods, fields, imports, constants, modules, and feature-gated stubs that nothing calls.
- **Do not** paper over dead code with `#[allow(dead_code)]`, `#[allow(unused)]`, or broad `allow` attributes “for later.” If it is not used now, delete it; restore from git when needed.
- **Clippy/gate:** `cargo clippy --all-targets --all-features -- -D warnings` must stay clean — that includes unused items. Fix by **removing** dead code, not by allowing warnings.
- **Exceptions only** when the item is part of a public API surface that must stay stable (`pub` for crates/downstream) or is required for `#[cfg]` / trait impl completeness and truly cannot be omitted — document why in a one-line comment on that item. Prefer not exporting unused symbols.

### Zero-copy and latch-free data paths

**Policy for agents and humans:** maintain and enforce a zero-copy, latch-free hot-path design. Do not introduce traditional blocking locks (like standard `Mutex` or `RwLock`) or unnecessary data copy operations on the primary read/write data path.

- **Zero-Copy Hot Path:** Hot staged files are mapped directly using memory mapping (`mmap`). Buffer segments must be returned or updated in-place via pointer/slice references. Avoid allocating new buffers or cloning vectors during standard read/write execution.
- **Latch-Free/Lock-Free Caching & Indexes:** Hot metadata tables, directory entry indices, and block routing tables must use lock-free or latch-free data structures (e.g., `scc::HashMap`, sharded atomic clock rings, atomic reference counts). Traditional read/write synchronization locks are only permitted for FUSE operations and metadata/lease transactions (see Lock Order constraints).
- **Zero-Copy GPU Direct (GDS):** When `gds` feature is active, bypass host RAM completely. Copy block data directly between NVMe/NVMe-oF and GPU memory via RDMA.
- **Zero-copy write path (normative design):** the large-write hot path is **1 userspace copy + 1 DMA** — transport payload leases (no per-request copy/alloc), one merge copy into the exclusive-owner `ActiveBlockBuf`, complete-block write-through past staging, guard-backed DMA for residual staged flushes. Design + acceptance evidence: `docs/design-zero-copy-write-path.md` and `.benchmarks/2026-07-08-zero-copy-write-path-closing.md`. Do not reintroduce copies or staging detours on this path.

---

## High-Performance Distributed Filesystem Architecture

### Target Scale & Layout
* **Scale:** 15,000+ Concurrent Nodes.
* **Architecture Type:** Decoupled metadata (MetaLV) + **block data** (local NVMe / NVMe-oF), exposed via POSIX FUSE.

### Technology Stack
* **Client Daemon (FUSE Engine):** Built in **Rust** using the asynchronous `tokio` runtime and `io_uring` polling over `/dev/fuse`.
* **Metadata & Distributed Lock Manager (DLM):** Local or distributed logical volume backend (**MetaLV**). Primary meta store for attrs, layout maps, leases, and volume format.
* **Data Backend (primary):** **NVMe / NVMe-oF block devices** via `NvmeBlockDev` (io_uring workers). Progressive layouts (inline / staged / striped) live on this path.

### Progressive Data Layout & I/O Routing

Logical file growth uses three layouts (thresholds are implementation-defined; current code uses ~4 KiB inline, up to ~4 MiB staged when staging dirs exist, else striped):

1. **Inline (tiny):** Payload in Metadata Volume (`inline_data:…` key/attribute) with type `inline`.
2. **Staged (small):** Local NVMe staging (`file_id` + optional `mapping:…`); writeback/flush promotes to durable blocks.
3. **Striped (large):** 4 MiB (configurable) blocks on the active block backend with `block_map:…` and refcounts.

Writes that grow past thresholds promote layouts **durably** (block I/O before meta type flip — see P0 layout atomicity).

### Distributed Lock Manager (DLM) & Consistency

POSIX FUSE locks map to cluster leases on Metadata Volumes:
* **Acquisition:** Lease locking + fencing token `INCR` (no external distributed database required).
* **Granularity:** File-level or byte-range; never directory-wide for data.
* **Leases & Heartbeats:** TTL + background renewal; local caches must re-validate after lock key loss.
* **Fencing Tokens:** Monotonic per-file tokens; writers present tokens; stale tokens → `FencingTokenExpired` / reject.

### Tiered Caching & Paths

* **Tier 1 (optional GDS):** GPU Direct path when `gds` feature is enabled.
* **Tier 2 (RAM LRU):** Sharded Clock/LRU read & write caches.
* **Tier 3 (Local NVMe staging / read cache):** Staging segments + optional read block cache; dehydrate on eviction.

### FUSE Client & Asynchronous I/O

* Work-stealing / multi-thread tokio; core pinning where configured.
* **Block path io_uring:** `NvmeBlockDev` worker (bounded request queue, backpressure, fixed-file register when available).
* **Path file I/O io_uring:** `crate::uring_fs` for ad-hoc local files (e.g. GDS cache materialize). Staging **mmap** segments stay mmap for zero-syscall get/put.
* **FUSE transport:** the first-party **fuse3** fork (`crates/fuse3`) — classical `/dev/fuse` only for `FUSE_INIT` (kernel requires initialized connection before REGISTER); **FUSE-over-io_uring is required** for the request hot path after arm (`flags2` `FUSE_OVER_IO_URING`, `REGISTER` / `COMMIT_AND_FETCH`). No userspace opt-out; mount fails if setup fails. Auto-enables `fuse.enable_uring=Y` when possible. One queue per possible CPU; session arms only after all queues REGISTERed.
* Always use io_uring when we can (non-negotiable) — see Non-Negotiables section.
* No dead code (non-negotiable) — see Non-Negotiables section.
* Not uring: TLS peer paths (network). Staging mmap segments stay mmap by design.

### Reference fast-FUSE clients (user directive, 2026-07-14)

The **DAOS client** (`github.com/daos-stack/daos`, `src/client/` — dfuse / libdfs / libioil / libpil4dfs) and **JuiceFS** (`github.com/juicedata/juicefs`) are the designated reference fast-FUSE clients. **Consult them for perf/recovery patterns before designing new client-side machinery** — both independently converged on the levers the metadata-throughput program validated (negative-dentry TTLs, clean-handle FLUSH elision, submit batching, per-class kernel TTLs), and their recovery/ops machinery (external supervisor + FUSE-connection abort, fsck/dump/backup, cache-disk health FSM, seamless-upgrade fd handover) is the standing gap board. The full survey — findings classified HAVE / IN-PROGRAM / GAP / N/A with ranked P1/P2/P3 adoption boards and code anchors — lives at **`docs/reference-clients-survey.md`**; extend it (same classification) when surveying them again.

### Metadata Cluster Topology

* Keys for volume control use `fs_prefix` / `fs_key!` helpers.
* **Layout keys** (`metadata:…`, `inline_data:…`, `block_map:…`, `mapping:…`, `active_block:…`) are **unprefixed historical** forms — use `crate::keys::*` helpers; do not migrate under `FS_PREFIX` without an on-disk format change.

### Metadata format: v3 CoW KV (the only format)

Format v3 is the **only** on-disk metadata format. Legacy v2 support was removed entirely (user directive: always forward — no backwards compatibility): a v2 superblock refuses to mount loud ("no longer supported; reformat required"), `squeezefs format --force` reformats it to v3, and the offline `squeezefs migrate` converter was deleted with the v2 reader. Normative spec `docs/design-cow-kv-metadata.md`, measured gates `.benchmarks/2026-07-09-kv-v3-gates.md`.

* **v3:** a bcachefs-style **copy-on-write typed-KV btree** (`src/meta_backend/kv/`). One node layer, three logical trees per volume (`TREE_INODES`/`TREE_DENTRIES`/`TREE_XATTRS`), memcmp-ordered keys. **256 KiB log-structured CoW nodes** (format knob `--meta-node-kib`, 64 KiB–1 MiB) with internal sorted-bset appends; a **lock-free reservation journal** (ring `clamp(volume/64, 8 MiB, 32 MiB)`, `--meta-journal-mb` override, **read at every mount**) carrying one checksummed entry per transaction; **checkpoints** flip per-tree roots in an A/B root ledger on the background flush cadence, **never on the commit path**; **monotonic ino allocation** (no reuse, no seeding scan); an **A/B extent-bitmap allocator** (1 bit / 256 KiB extent) with a journaled pending-free protocol. Superblock: magic `METALV01`, version 3, `features_incompat` bit 0 = `KV_V3` (`src/meta_backend/kv/superblock.rs`). Caps: ≥ 100 M inodes, 1 M+ entries/dir, unlimited xattrs (value ≤ `min(65536, node_size/4)` B). Reads stay RAM-authoritative and latch-free (scc node index + arc-swap snapshots, demand-paged, clock eviction). Every on-disk unit is checksummed and copy-on-write: whole-transaction atomicity (one tx = one checksummed journal entry) + torn-write immunity (torn writes detected-and-ignored) — strictly stronger than the retired v2 D0/D1/D2 sector contract (§4.10).
* **Env knobs:** `SQUEEZEFS_META_NODE_CACHE_MB` (node-cache budget, default 512), `SQUEEZEFS_META_CHECKPOINT_MAX_DIRTY_NODES` (dirty-node checkpoint cap = mount-replay working-set bound, default 4096); `SQUEEZEFS_META_FLUSH_INTERVAL_MS` is the journal/checkpoint cadence (`0` = strict per-commit).
* **Stats fields (stats inode):** `meta_format_version` (constant `"3"` per volume — kept because operators key on it); `meta_volume_atomicity` (contract class — constant `cow-checksummed`) alongside `meta_volume_atomicity_physical` (the informational hardware probe); and the `meta_kv_*` family — `node_cache_{hits,misses,evictions}`, `node_appends`, `node_{append,rewrite}_bytes`, `node_{compactions,splits}`, `journal_{bytes,entries,full_stalls}`, `checkpoints`, `commit_smo_retries`, `node_dropped_tail_bsets`, `dentry_collision_overflows`, `delta_orphans`, `replay_{entries,dropped_torn,ms}`, `free_extents`, `pending_free`, `pending_free_{parked,released}` (SMO frees parked on / released by durable-tail coverage — design-smo-replay-currency Option A; `parked − released` tracks the `pending_free` gauge, the wedge detector). The retired v2-only counters (`meta_commit_sectors`, `meta_sector_lock_*`, `meta_inode_alloc_*`, `meta_tx_concurrency*`, `meta_quarantined_inodes`) no longer exist.
* **Filesystem generation identity:** per-volume the v3 superblock `uuid` (random per `format` invocation), joined in volume order (`meta_backend::volume_set_generation`) — local staging is bound to it. (The v2-era `FormatConfig.fs_uuid` config stamp was deleted with v2; the superblock uuid is the sole generation identity.)

### Metadata-throughput program (Implemented 2026-07)

The 4.4× FUSE-layer metadata multiple was closed by the M1–M12 program — design + landed-SHA record `docs/design-metadata-throughput.md`, gate adjudication `.benchmarks/2026-07-15-metadata-throughput-closing.md`, beta-blocker fix `.benchmarks/2026-07-15-find-m11a-fix.md`. What is now load-bearing machinery (do not regress):

* **Single-writer mount guard (D0)** — every write mount claims each meta volume: a **dedicated daemon-lifetime `flock`** (same-host, kernel-arbitrated, instant crash reclaim), **NVMe Persistent Reservations** Write-Exclusive where `RESCAP` advertises support (cross-host *enforcement* — a fenced holder fail-stops at its first post-fence journal barrier), and a **`writer_claim`** heartbeat record (identity + detection everywhere). **No bypass knob**; automatic cross-host takeover is disabled on non-PR substrates — recovery is dead-pid proof → PR preempt → TTL → the attested **`squeezefs claim clear <sqmeta-uri>`** verb, in that order of automation. The guarantee-class table ships in docs/operations.md verbatim; the barrier-failure escalation governs journal-durability barriers only (the data-path writeback ladder stays retry-forever — see below).
* **Commit conveyor (group commit v2, D5)** — all user commits on a volume enqueue `{staged records, Arc<[DlmGuard]>, oneshot}`; a detached, panic-guarded per-volume **pass task** drains a batch and runs one admission → union-leaf-lock pass → **N ordinary checksummed entries** in one contiguous reservation → one write → one barrier. **One tx = one checksummed journal entry is unchanged** (zero on-disk format change; whole-tx atomicity + torn-write immunity transfer verbatim). Queue entries co-own their tx's DLM guards until terminal outcome — a dropped committer future cannot strand isolation.
* **Journal-entry economy (D4)** — rename is one whole-tx entry (incl. parent Δtimes); the kernel's SETATTR ctime/mtime echo is absorbed, not committed; unlink destroys ride batched `destroy_inodes`. Measured **≈ 1.0 entries/op rename/unlink** (gate G4 ≤ 1.05) — regressions show in `meta_kv_journal_entries` per-op ratios.
* **Fold overlay + snapshot memo (D7)** — record folds serve from fold-forward overlay heads and immutable-snapshot memos (fold algebra untouched, property-tested equivalence; replay rides the same fold). `InodeDelta::decode` fell off the profile (26.3 % → < 0.5 % of daemon CPU); memo/overlay bytes are charged to the node-cache budget.
* **Watchdog, not per-op timeouts (D1.b)** — handlers no longer wrap in `tokio::time::timeout` (that expiry *dropped* futures mid-`commit_tx` — the ring-budget/reservation-wedge vector). A deadline watchdog logs overdue ops loudly (`fuse_op_watchdog_overdue`); ring-admission parking past the threshold escalates via the `disabled_volumes` fail-stop lattice; barrier waits keep bounded errors. Do not reintroduce per-op timeout wrappers.
* **Round-trip + transport economy (D2/D3)** — clean-handle FLUSH/RELEASE fast paths, `FOPEN_NOFLUSH`, kernel-side negative-entry caching, refresh-instead-of-invalidate parent attrs (fuse_ops/create 5.18 → ~4.0); over-uring COMMIT_AND_FETCH submits batch per drain (`io_uring_enter`/create 31 → ~9). SQPOLL on the queue rings exists as a knob and measured **not recommended** (M10; README posture note).
* **Supersession-aware never-lossy writeback (FIND-M11-A fix)** — flush units re-validate staged ownership per attempt and present the ino's *current* DLM generation; fencing-stale units resolve as contractual no-ops, verified-NotFound (reclaimed-ino) units discard their orphans, and everything genuinely transient still retries forever. "Stale fencing tokens discard staged work" is the **remount** contract, not a license to drop live acked custody.
* **Env/mount knobs added:** `SQUEEZEFS_META_COMMIT_BATCH_TXS` (default 64) / `SQUEEZEFS_META_COMMIT_BATCH_BYTES` (default 256 KiB, clamped to the ring's admissible capacity) — conveyor batch caps; `SQUEEZEFS_OP_PROFILE=1` — per-op phase histograms + the under-`i_rwsem` estimator (zero cost off); per-class kernel TTLs `-o attr_timeout/entry_timeout/dir_entry_timeout/negative_timeout` / `SQUEEZEFS_FUSE_{ATTR,ENTRY,DIR_ENTRY,NEGATIVE}_TTL_MS` (default 1 s each); `mount --daemon --supervise` external watchdog (+ `SQUEEZEFS_SUPERVISE_{INTERVAL,UNRESPONSIVE}_SECS`) with FUSE-connection abort.
* **Stats families added (stats inode):** `writer_guard_{mode,fenced,pr_reacquires}`; `meta_commit_group_{size,bytes}`, `meta_conveyor_{leader_passes,pass_panics}`; `meta_kv_fold_{head_serves,memo_hits,memo_misses,memo_bytes}`; `meta_kv_times_echo_{absorbed,pending,drained,drain_commits}`; `fuse_op_phase_ns` / `fuse_create_under_lock_ns` / `fuse_op_watchdog_overdue`; `fuse_{flush,release}_clean_fastpath`, `fuse_lookup_negative_replies`, `fuse_attr_cache_refreshes`; `transport_commit_batch*`; `meta_reclaim_gather_*`; `writeback_{superseded_noops,stale_token_retries,orphan_discards}`.

### Random-small-write program (Implemented 2026-07)

The ~2,500× whole-block RMW amplification on small random writes (the vs-JuiceFS scoreboard's only genuine product loss: 354–397 IOPS) was closed by the RW1–RW5 program — design + landed-SHA record `docs/design-random-small-writes.md`, closing adjudication `.benchmarks/2026-07-17-rand-write-program-closing.md`. Load-bearing machinery (do not regress):

* **Sole-owner extent patch (W1)** — isolated LBA-aligned small overwrites of exclusively-owned, passthrough, whole-block-mapped striped blocks are **one in-place sub-block DMA** (zero reads / meta / staging; 61–67 k IOPS, W all scoreboard regimes). The predicate lives in ONE place (`is_whole_block_mapping()` + the 6-clause decision ledger); the clone/patch race is closed by the §5.1 `fence(SeqCst)` protocol on both sides (composed two-word loom model — the fence is load-bearing, verified by weakening). v1 is aligned-only **by contract**: only app-written sectors are ever rewritten (crash blast radius), and `patch_edge_rmw_reads` must stay 0.
* **Coverage-union write-through trigger (RW3b / FIND-L1-A fix)** — block completeness is the ACCUMULATED written-coverage union (`record_write`'s completion transition), never one write's end offset; kernel-split out-of-order O_DIRECT WRITE segments are the normal case (`FOPEN_PARALLEL_DIRECT_WRITES` stays on). Inline write-path seed fetches are deleted; `write_path_seed_read_bytes` is a must-stay-0 tripwire; a fully-covered buffer can never pay a flush-seed read (`seed_deferred ⇒ union partial`, structural).
* **Extent overlay + staged extent records + batched fold (W2)** — patch-ineligible shapes park extents (byte-budgeted, R5-gauged), spill as versioned+checksummed `active_block_ext:` records (**no seed read at spill, ever**), fold seed-once with measured amortization (`fold_fill` median ≥ 16; compressed rand-4k ~2,500× → 15–26×). Recovery: generation-bound + fencing-stamped like staged blocks; future-version records/dirs refuse loud; clean unmount drains records to fold (a clean staging dir carries none); below-RW4 downgrade is declared-unsupported with loud orphan forward-detection.
* **Incompressible store-raw escape (FIND-RW4-A fix)** — compression is best-effort per block (frame bit 31 = raw, below the AEAD layer); transformed volumes reserve chunk headroom at format and mounts refuse geometries that cannot hold `max_stored_image_len(block_size)`; every transformed upload site refuses oversize images loud (never overflow, never truncate).
* **Instrument-alignment lesson (standing)**: elbencho page-aligns its O_DIRECT buffers; `squeezefs bench` (tokio-copied) does not — an unaligned 1 MiB buffer spans max_pages+1 and the kernel **splits it into 2 concurrent out-of-order FUSE WRITEs**. FIND-L1-A hid from one instrument and reproduced under the other for three sessions. **Every measurement must state its instrument**, and write-path changes must stay correct under split/reordered WRITEs (that is now pinned by `tests/write_through_coverage_tests.rs`).
* **Env knobs added:** `SQUEEZEFS_PATCH_MAX_BYTES` (default 512 KiB; `0` = the acceptance A/B lever, not an operational escape), `SQUEEZEFS_FOLD_MAX_EXTENTS` / `SQUEEZEFS_FOLD_MAX_BYTES` (fold triggers, default 64 / 1 MiB).
* **Stats families added (stats inode):** `patch_{writes,write_bytes,dma_errors}`, `patch_edge_rmw_reads` (**0 by definition in v1**), `patch_ineligible_{unmapped,decorated,unaligned,overlay,shared,transform,adjacent,oversize}` (the decision ledger — growth on a shape that should patch = predicate rot); `active_block_ooo_runs`, `write_path_seed_read_bytes` (must stay 0), `write_through_blocks`, `overwrite_seed_{materialized,skipped}`; `parked_extent_bytes` / `parked_full_buffer_bytes` (R5 components), `extent_{parks,escalations,implicit_escalations,spills,spill_bytes,record_absorbs}`, `fold_{passes,seed_reads,extents_folded,fill}`, `staged_rider_{extent_writes,folds}`, `extent_records_{recovered,stale_discarded,torn_discarded,future_refused}`; `compress_stored_raw`.

### LD_PRELOAD interception data plane (Implemented 2026-07)

The L4 program (`docs/design-preload-interception.md`; closing record `.benchmarks/2026-07-19-l4-interception-closing.md`) shipped the DAOS-libioil-class client bypass: `-o interception` arms a per-mount session host (abstract AF_UNIX + sealed-memfd shm sessions; the §5.2 daemon fd screen is THE security boundary), and `LD_PRELOAD=libsqueezefs_il.so` (built ONLY via `--profile preload-release --features interposers` — the Issue-4 `panic="abort"` compile guard makes wrong-profile builds unrepresentable) routes data ops on bound fds over a lock-free ring. Load-bearing machinery (do not regress): pinned single-consumer service threads (default `clamp(cpus/4,2,8)` — warm serves execute ON them), the §5.5.1 sync fast path (per-inode `try_read()` + the drop-guard-before-enqueue demote rule + the sync tier serve mirroring the handler's staging/hot/read-cache legs), sever-at-dequeue write custody (§5.5.2), KD-7 build-commit equality (dev override forgives `-dirty` degeneracy, never inequality), KD-11 forced write-through (~3× tax on unintercepted buffered small writes, priced in the closing report), the fork-child `close(2)`-not-`shutdown(2)` poison law, idle reap, and the W1 `notify_inval_inode` handoff riding the classical sideband post-arm. Client levers: `SQUEEZEFS_IL_SESSIONS` (fd-sharded sessions, default 4), adaptive spin (`SQUEEZEFS_IL_SPINS` pins), `SQUEEZEFS_IL_OP_TIMEOUT_MS`. G-L4-2 adjudicated (devsub, elbencho sync drivers, medians of 3): **warm 1,017,548 IOPS** (kernel-FUSE warm floor 644,726; target 1.0 M met at median) and **device-true 622,112** (floor 600 k; beats the kernel-FUSE 604,313 reference) — engagement exact on every run. The scoreboard's `SQUEEZEFS_SB_MODES=il` pass emits separately-labeled, never-W/L-gating rows whose engagement check exits nonzero on any silent-passthrough cell; its instrument must be a DYNAMIC elbencho (the pinned static one cannot load the shim). OQ-1 (libaio interposers) resolved: v1.1 economics item, no floor depends on it. stat/del storms and buffered-il semantics are N/S by design.

### Error Handling & Crash Recovery

* FUSE op deadline **watchdog** (per-op `tokio::time::timeout` wrappers were retired in M4 — overdue ops are logged loudly, ring-admission parking escalates to `disabled_volumes`); staging recovery on remount (`recover_staging`) with fence + layout checks.
* Stale fencing tokens discard staged work (the **remount** contract); missing inode meta discards orphan active blocks. Live writeback units apply the same law supersession-aware (FIND-M11-A fix): fencing-stale ⇒ verified no-op, reclaimed-ino NotFound ⇒ verified orphan-discard, transient ⇒ retry forever (never-lossy).
* Write verification is **opt-in** (`--write-verification`, optional sample rate).
* **Metadata crash contract**: whole-transaction atomicity + torn-write immunity **by construction** (v3 CoW KV — see Metadata format section + `docs/design-cow-kv-metadata.md` §4.10; the historical D0/D1/D2 ladder it replaced is `docs/design-wal-crash-consistency.md` §3).
* Mount probes each meta volume's sector atomicity (sysfs) — purely informational, surfaced as `meta_volume_atomicity_physical` while the contract field `meta_volume_atomicity` reads `cow-checksummed`. (The `--strict-meta-atomicity` flag only ever gated v2 volumes and was deleted with them.)

### Lock order & connection scope (must not)

Always acquire in this order; **never invert** (P1-9):

1. `active_inode_locks` (per-inode `RwLock`) — FUSE op serialization
2. `lease_locks` (per-inode) — only while acquiring/refreshing DLM lease
3. `BLOCK_FLUSH_LOCKS` (per block) — active-block mutation
4. MetaLV metadata-transaction locks, in sub-order:
   - **4a.** DLM `I{ino}` / `D{parent:name}` (per-object; `DlmLockManager`).
   - **4b.** (design-cow-kv-metadata §4.9 4b): **per-node write locks — the commit path takes leaf locks only, in ascending NodeId order, deduped, lock-then-revalidate-then-retry against SMOs; interior-node locks belong exclusively to the serialized per-volume checkpoint/SMO task (parent-then-child), which is what keeps the two lock populations acyclic. Node locks are never held across device I/O (commit apply is RAM-only; the journal entry write happens after unlock; writeback freezes under the lock and appends outside it; SMOs reserve in-window and write after release) and never held while waiting on ring space (ring admission happens before any node lock — §4.4 pt 5; the checkpoint task's own admissions never park, they drain-and-retry).** Since the M7 commit conveyor (design-metadata-throughput §5.5), the leaf-lock **taker** population is exactly {the per-volume conveyor **pass task**, the checkpoint/SMO task}: user committers take no node locks — they enqueue and park on oneshots, each queue entry co-owning its tx's 4a DLM guards until terminal outcome, and the pass takes the batch's **union** leaf set under the same 4b discipline while it *holds-but-never-acquires* DLM guards (no new wait-for edges; see `src/stripe_locks.rs`).
   - **4c.** The journal reservation — a wait-free atomic, not a lock; ordered inside 4b by protocol, it imposes no ordering edges.

**Must not:**
* Hold inode **write** guard across long backend I/O when block locks suffice (striped data path = meta-prep only under write lock — P1-8).
* Hold a **pooled MetaLV connection/handle** across durable NVMe / staging I/O (P1-10: open → short meta → drop → I/O → re-acquire for commit).
* Acquire (1) while holding (3).
* Burn fencing tokens on failed lock acquisition.

### OS / fabric notes

* Dirty ratios and fabric tuning remain operator concerns for large clusters.
* Primary data path uses the NVMe/NVMe-oF block storage backend.

---

## TDD Development Workflow

A disciplined, test-first development workflow for building industrial-grade, high-performance, highly scalable, asynchronous Rust systems. Tests define the contract. Code fulfills it.

### Core Principle: Tests First, Code Second

Every feature, bugfix, or refactor follows the same cycle:

1. **Define** — What should the correct behavior be?
2. **Test** — Write tests that assert that behavior (they will fail).
3. **Implement** — Write the minimum code to make the tests pass.
4. **Refine** — Refactor for clarity and performance while tests stay green.
5. **Commit** — Each logical step gets its own git commit.
6. **Merge** — Bring the code base back into the primary branch.

### Phase 0: Branch from `dev`

All work happens on feature/fix branches off `dev`. Never commit directly to `dev` or `main`.

```bash
git checkout dev
git pull origin dev
git checkout -b feat/short-description   # or fix/short-description
```

- **Branch naming**: `feat/`, `fix/`, `refactor/`, `perf/`, `docs/` prefixes matching conventional commit types.
- **One branch per logical unit of work**: a feature, a bugfix, or a hardening pass.

#### SqueezeFS I/O constraint (always io_uring when we can)

This repo is Linux + **io_uring**-first. When planning or implementing:

- **Default to io_uring** for FUSE request traffic (FUSE-over-io_uring after arm), NVMe/block I/O (`NvmeBlockDev`), and local file I/O (`crate::uring_fs`) whenever the kernel can support it.
- **Do not** introduce or re-enable classical `/dev/fuse` or POSIX file I/O fallbacks to “make it work.” That is an anti-pattern here. Fix the uring path or fail loud.
- The intentional classical FUSE uses are the one-shot **`FUSE_INIT`** exchange (kernel requires it before REGISTER) and the post-arm **classical sideband session** for the traffic the kernel refuses to put on the ring (FORGET/INTERRUPT/resends + `fiq->ops` switchover stragglers) — serviced via io_uring `Readv`, never as a hot-path fallback. All other requests/replies are over-uring only after arm.
- If a failing test tempts you to disable over-uring or switch to blocking `read`/`write`, stop — write a failing test that encodes correct uring behavior, then fix uring.

#### No dead code

- Remove unused functions, fields, imports, and modules as part of the same change that made them unused.
- **Never** add `#[allow(dead_code)]` / `#[allow(unused_*)]` to park unused code. Delete it; git has history.
- Clippy with `-D warnings` failing on unused items means **delete**, not allow.

### Phase 1: Understand & Plan

Before touching any code:

- State the goal in one sentence.
- Identify the crate(s) and module(s) that will be affected.
- List the behaviors that need to be correct when you're done.
- Identify edge cases, error conditions, and concurrency concerns upfront.
- Determine `Send`/`Sync` requirements for all shared state.
- Identify async boundaries — which types must implement `Future`, which tasks cross `.await` points.
- **I/O path:** does this touch FUSE, NVMe, or local files? If yes, plan the **io_uring** design — not a classical shortcut.
- **New client-side machinery?** Check the reference fast-FUSE clients first (DAOS client, JuiceFS — `docs/reference-clients-survey.md`) for a proven pattern before inventing one.

Produce a brief execution plan:

```
Goal: [one sentence]
Affected: [crates/modules]

Behaviors:
1. [Expected behavior] → verify: [how]
2. [Expected behavior] → verify: [how]
3. [Edge case] → verify: [how]

Concurrency:
- Shared state: [Arc<Mutex<T>>, Arc<RwLock<T>>, lock-free, actor]
- Async runtime: [tokio multi-thread, current-thread, custom]
- Cancellation: [CancellationToken, drop-based, deadline]
```

### Phase 2: Write Tests

Write the test file(s) BEFORE any implementation:

- **Happy path**: The expected, normal-operation case.
- **Error paths**: What happens when inputs are invalid, connections drop, `CancellationToken` fires, channels close?
- **Boundary conditions**: Empty inputs, maximum values, zero-length streams, single-node mesh.
- **Concurrency**: Race conditions under parallel access. Multiple tokio tasks hitting the same state. Use `#[tokio::test(flavor = "multi_thread", worker_threads = N)]` to stress shared state.
- **Timeouts & Cancellation**: `tokio::time::timeout`, `CancellationToken` propagation, slow peers, hung connections.
- **Backpressure**: Channel saturation, bounded queue overflow, consumer lag.
- **Shutdown**: Graceful drain under in-flight work, partial completion, resource cleanup on abort.

#### Test Quality Checklist

- [ ] Each test has a clear, descriptive name (`test_broadcast_storm_uuid_dedup`)
- [ ] Tests are independent — no shared mutable state between test cases
- [ ] Parameterized tests via `rstest` (`#[case]`, `#[values]`) for combinatorial scenarios
- [ ] Async tests use `#[tokio::test(flavor = "multi_thread")]` to expose data races
- [ ] Assertions include meaningful failure messages (`.expect("msg")`, `assert!(cond, "msg")`, or `pretty_assertions`)
- [ ] No `tokio::time::sleep` for synchronization — use channels, `tokio::sync::Barrier`, `watch` channels, or `CancellationToken` for coordination
- [ ] Tests that spawn tasks verify cleanup (no leaked tasks, no dangling `JoinHandle`s)
- [ ] Property-based tests via `proptest` or `quickcheck` for complex invariants

#### Commit: Tests

```
test(crate): define behavior for [feature]

- Happy path: [describe]
- Error cases: [describe]  
- Edge cases: [describe]
- All tests currently FAIL (no implementation yet)
```

### Phase 3: Implement

Write the minimum code to make all tests pass:

- Don't add features beyond what the tests require.
- Don't add abstractions for hypothetical future needs.
- Don't optimize prematurely — correctness first.
- Run `cargo test --all-features` after every meaningful change.
- Run `cargo test --all-features -- --test-threads=1` if order-dependent failures are suspected.

#### Implementation Checklist

- [ ] All tests pass (`cargo test --all-features`)
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] All async functions and shared types are `Send + Sync` where required
- [ ] Error types use `thiserror` (library) or `anyhow` (application) with `.context()` for operation context
- [ ] Resources cleaned up via RAII — `Drop` impls, `scopeguard`, or `_drop_guard` patterns
- [ ] No `unsafe` without a documented safety invariant and a safety comment
- [ ] No `unwrap()`/`expect()` in library code — propagate errors
- [ ] All `JoinHandle`s are awaited or explicitly detached with documented rationale
- [ ] No TODO/FIXME left without a tracking issue
- [ ] **I/O uses io_uring where applicable** — no new classical FUSE/file fallbacks; FUSE request path stays over-uring after arm
- [ ] **No dead code** — unused items deleted; no new `#[allow(dead_code)]` / unused allows to hide them

#### Commit: Implementation

```
feat(crate): implement [feature]

- All tests passing
- Clippy clean, fmt clean
- [Brief note on approach taken]
```

### Phase 4: Refine, Harden, & Benchmark

With green tests as your safety net:

- Refactor for clarity if the implementation is messy.
- **Mandatory Benchmarking**: EVERY public function must have a corresponding Criterion benchmark. Performance is a first-class feature.
- Profile allocations for data-plane code via Criterion's allocation measurements.
- Run `cargo bench` to record historical performance; commit results to `.benchmarks/` or use `cargo-criterion` with `--save-baseline`.
- Run `cargo loom` on lock-free or highly concurrent data structures to exhaustively check memory-ordering correctness.
- Add any additional edge case tests discovered during implementation.

#### Commit: Refinements

```
refactor(crate): [what changed and why]
```
or
```
perf(crate): optimize [hot path] - [result]
```

### Phase 5: Integration Verification

Before considering work complete:

1. `cargo build --all-targets --all-features` — full project compiles.
2. `cargo test --all-features` — all tests pass project-wide.
3. `cargo clippy --all-targets --all-features -- -D warnings` — zero warnings.
4. `cargo fmt --check` — formatting is clean.
5. `cargo doc --no-deps` — documentation builds without warnings.
6. `cargo audit` — no known vulnerabilities in dependencies.
7. Review the diff: every changed line traces to the original goal.

If a `task check` or `cargo xtask check` target exists, run it — it is the authoritative quality gate.

**Required verification gate (must pass before commit — tiered by change class; user directive 2026-07-18: stop running full test suites on doc-only changes):**

| Change class | Required gate |
|---|---|
| **Docs/markdown only** (no `.rs` / `.toml` / tests / scripts touched) | markdown link/anchor check only — **no cargo gate** |
| **Manifest metadata only** (`Cargo.toml` non-dependency keys) | `cargo check` + `cargo clippy --all-targets --all-features -- -D warnings` + `cargo fmt --check` |
| **Code, dependencies, tests, or harness scripts touched** | the **full gate** below (plus loom when a lock-free core changes) |

The **full gate**:

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
cargo test --all-features -- --test-threads=1
cargo doc --no-deps
cargo bench --benches -- --test   # criterion smoke: one iteration per bench, no measurement
```

### Phase 6: Merge to `dev` & Cleanup

After all checks pass, merge the feature branch back into `dev` and delete it:

```bash
git checkout dev
git merge --ff-only feat/short-description
git branch -d feat/short-description
```

- **Always `--ff-only`**: If `dev` has diverged, rebase the feature branch first (`git rebase dev`). No merge commits for linear history.
- **Delete the branch**: Once merged, the branch serves no purpose. Delete it immediately.
- **Push**: `git push origin dev` to sync remote.

### Git Discipline

- **`dev` is the integration branch**: All feature/fix branches start from and merge back into `dev`. `main` is reserved for releases.
- **Atomic commits**: One logical change per commit. Tests and implementation can be separate commits.
- **Conventional format**: `type(scope): description` — types: `feat`, `fix`, `test`, `refactor`, `perf`, `docs`, `chore`.
- **No direct commits to `dev` or `main`**: Always use a branch.
- **Commit messages explain WHY**, not just what. The diff shows what changed; the message explains the reasoning.
- **Delete branches after merge**: Stale branches are clutter. Merge → delete → move on.

### Anti-Patterns to Avoid

| Anti-Pattern | Correct Approach |
|---|---|
| Writing code first, tests after | Write tests first — they define the contract |
| Testing only the happy path | Cover errors, boundaries, concurrency, cancellation, backpressure, shutdown |
| Giant commits with tests + code + refactor | Separate commits for tests, implementation, refinement |
| "Improving" unrelated code while fixing a bug | Touch only what the goal requires |
| Skipping concurrency tests because "it's simple" | Always use `multi_thread` flavor. Simple async code has races too |
| Using `tokio::time::sleep` for test synchronization | Use channels, `Barrier`, `watch`, or `CancellationToken` |
| Speculative abstractions | Build what's needed now. Refactor when a real pattern emerges |
| `unwrap()` in library code | Propagate errors with `?` and `thiserror` |
| `unsafe` without safety comments | Document invariants; prefer safe alternatives |
| Blocking the async runtime | Use `tokio::task::spawn_blocking` for CPU-bound or blocking I/O |
| Unbounded channels in production code | Use `mpsc::channel(bound)` with explicit backpressure |
| Leaking tasks (fire-and-forget `spawn` without tracking) | Use `JoinSet` or structured concurrency patterns |
| Ignoring `Send + Sync` bounds | Design types to be `Send + Sync` from the start; document why if not |
| Falling back to classical `/dev/fuse` or POSIX file I/O to dodge an uring bug | **Always use io_uring when we can.** Fix the uring path or fail the mount/op — never reintroduce classical escape hatches |
| Leaving unused code with `#[allow(dead_code)]` “for later” | **Delete dead code.** Restore from git when needed; keep clippy `-D warnings` clean by removal |

---

## Build

System deps (Ubuntu/Debian): `build-essential pkg-config libfuse3-dev fuse3 clang libclang-dev`. The build links FUSE 3 and uses `io-uring` + `/dev/fuse` — it will not compile on non-Linux.

```bash
cargo build --release
```

### Optional Cargo features (off by default)
- `gds` — GPU Direct Storage RDMA path (pulls `libloading`).
- `dhat-on` — heap profiling (`dhat`); a static `dhat::Alloc` replaces the global allocator in `src/main.rs`.
- `coz-on` — causal profiling; the `coz_progress!` macro (defined in `src/lib.rs`) becomes active.

`[profile.release]` keeps `debug = true` so release binaries carry symbols for profiling. `tikv-jemallocator` is the global allocator on Linux.

### First-party `fuse3` fork (`crates/fuse3`)

`crates/fuse3` is a maintained **fork** of the upstream crates.io `fuse3` crate (v0.7.3, Sherlock Holo, MIT — upstream attribution retained in `THIRD_PARTY_NOTICES.md` and `crates/fuse3/LICENSE`). It has substantially diverged from upstream: the FUSE-over-io_uring transport, payload leases, and flag sweeps live here. Treat it as first-party code — edit it directly when FUSE behavior changes; **never** replace it with the crates.io version or "restore" `Cargo.toml.orig`. `Cargo.toml` has `[patch.crates-io] fuse3 = { path = "crates/fuse3" }`, which redirects every `fuse3` dependency edge in the build graph to this in-tree fork.

### Build features for profiling

| Feature | Purpose |
|---------|---------|
| *(default)* | Production binary; `tikv-jemallocator` on Linux |
| `dhat-on` | Heap profiling via `dhat` (replaces global allocator in `main`) |
| `coz-on` | Causal profiling; `coz_progress!` active |
| `gds` | GPU Direct Storage path |

```bash
cargo build --release
cargo build --release --features dhat-on
cargo build --release --features coz-on
```

Release profile keeps `debug = true` for symbolicated profiles.

---

## Testing

Most integration tests execute against local file-backed MetaLV sandboxes.

### Required verification gate (tiered by change class)

Match the gate to what the commit touches (user directive 2026-07-18: stop running full test suites on doc-only changes). Same table as the TDD Phase 5 gate — keep the two in sync:

| Change class | Required gate |
|---|---|
| **Docs/markdown only** (no `.rs` / `.toml` / tests / scripts touched) | markdown link/anchor check only — **no cargo gate** |
| **Manifest metadata only** (`Cargo.toml` non-dependency keys) | `cargo check` + `cargo clippy --all-targets --all-features -- -D warnings` + `cargo fmt --check` |
| **Code, dependencies, tests, or harness scripts touched** | the **full gate** below (plus loom when a lock-free core changes) |

The **full gate**:

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
cargo test --all-features -- --test-threads=1
cargo doc --no-deps
cargo bench --benches -- --test   # criterion smoke: one iteration per bench, no measurement
```

### External POSIX / IO suites (require root + a mounted FS)

These are **not** part of `cargo test`. Run as **root** on Linux/WSL after material write-path, layout, or FUSE lock changes:

```bash
# LTP filesystem syscall tests
sudo tests/run_ltp_syscalls.sh

# fstests filesystem checks
sudo tests/run_fstests.sh

# Mount-level IO benchmark
sudo tests/run_elbencho_mount.sh
```

Prerequisites: Volume formatted/mounted per `QUICKSTART.md`. Failures here can pass pure unit tests and still indicate mount regressions.

Dev boxes without spare raw NVMe: `sudo tests/dev_substrate.sh create` builds the preferred pseudo-everything substrate — memory-backed null_blk (mds) + zram (oss) exposed as real `/dev/nvmeXnY` namespaces via nvmet-loop (QUICKSTART → *Dev Box: Virtual NVMe Substrate*). Do not measure barrier-bound work on file-backed-on-btrfs volumes: the substrate bracket is 165× on the journal barrier (`.benchmarks/2026-07-14-metadata-throughput-baseline.md`).

### Test tiering (do not run acceptance suites at per-commit cadence)

fstests/LTP are **wall-clock-bound** (fixed-duration fsx/fsstress soaks, mount-cycle overhead) — a full `-g auto` is ~5 h regardless of CPU speed. Running them per fix wastes hours re-failing tests already known to fail. Use three tiers:

| Tier | What | When | Cost |
|------|------|------|------|
| **Per-commit** | the change-class gate above (docs-only: markdown link/anchor check, no cargo; manifest-metadata: check+clippy+fmt; code-class: the full cargo gate — clippy/fmt/`test --test-threads=1`/doc/bench-smoke; loom when a lock-free core changes) | every commit, sized to its class | seconds–~5 min by class |
| **Per-PR (data-path)** | the full cargo gate + the relevant house rigs (`tests/run_preload_gate.sh` for ipc/preload surfaces, `tests/run_volume_lifecycle.sh` for volume-lifecycle surfaces) — **external POSIX suites (fstests/LTP/pjdfstests) do NOT run per-PR** (user ruling 2026-07-20: they added hours per code commit re-confirming known failures; they are the release gate below) | PRs touching the write/read/layout/FUSE paths | minutes |
| **Per-PR (preload/interception)** | `tests/run_preload_gate.sh` (leg 1, unprivileged: sanctioned cdylib build + Issue-4 guard proof + passthrough battery) / `sudo tests/run_preload_gate.sh` (leg 2: mount parity + §3 rule-4 engagement + dup/close_range/lseek rows + kill-9 and fork-kill-parent soaks) | PRs touching `crates/squeezefs-preload/`, `src/ipc_host.rs`, `src/ipc_service.rs`, or the fuse3 notify surface | leg 1 ~2 min; leg 2 ~5 min |
| **Per-PR (nvmeof/guard fidelity)** | `sudo tests/run_nvmeof_fidelity.sh quick` — both-stack (SPDK + kernel nvmet) product-verb round-trips + 1 guard kill-9 cycle per stack, zero-residue snapshot assert; real kernels, zero mocks (design-nvmeof-target-management §6.8) | PRs touching `src/nvmeof/`, `src/meta_backend/reservation.rs`, or the writer-guard gate | ≤10 min (2m20s measured, zram/localhost dev box) |
| **Release gate (MANDATORY before any release tag)** | **pjdfstests + full LTP (`sudo tests/run_ltp_syscalls.sh`) + full fstests (`sudo tests/run_fstests.sh`, `-g auto`) must ALL pass** (user ruling 2026-07-20: every release passes all three; known-failure exceptions require a documented adjudication in `.benchmarks/`; **every failure triggers the repro-port mandate below** — the fix lands with a cargo test reproducing it), plus `sudo tests/run_elbencho_mount.sh` and `long_validation.py` scale mounts | before EVERY release tag; also nightly unattended where a box is available | hours |
| **Nightly (nvmeof fidelity)** | `sudo tests/run_nvmeof_fidelity.sh full` — guard kill-9 ×10 per stack (restart-from-zero) + SPDK PTPL power-cycle leg, PR/PTPL matrix, loud-fail matrix (G3), crash-window injection (§6.4 law 6), adopt legs (§6.10), target-restart persistence (G2, both stacks), soft-RoCE plumbing leg (rdma_rxe — plumbing only, no guard/perf claims), A/B smoke rows (recorded), teardown-to-zero-residue proof | nightly + nvmeof program/release gates | ≤70 min budget (5m46s measured, zram/localhost dev box) |
| **Per-release (competitive)** | `tests/run_scoreboard.sh` — the standing **multi-reference scoreboard**, the top-3 proof surface (PERFORMANCE IS PRIMARY, 2026-07-18; absorbed `run_vs_juicefs.sh` forward-only): SqueezeFS vs **JuiceFS**, **SeaweedFS native** (weed server + weed mount), **geesefs** + **mountpoint-s3** over one shared local RustFS store — pinned releases w/ checksums, matched substrate/budgets, 3 regimes × 6 elbencho workloads, **RW6 durability-leveled write rows** + **RW6-del durable delete rows** (relaxed published labeled; fsync/syncfs-inclusive *durable* rows govern write **and delete** verdicts — the old seq-write ACK allowlist retired to ∅, and the inaugural run's three adjudicated entries were deleted the day their follow-ups landed: **standing allowlist ∅, bare run**), declarative + empirically-verified **N/S capability matrix** (neutral cells, recorded refusals), per-row-family **top-3 rank adjudication**; exits nonzero on any unattributed LOSS vs any reference or INVALID sqz cell; `SQUEEZEFS_SB_ALLOW_LOSS` tracks attributed losses (legacy `SQUEEZEFS_VS_*` spellings honored). `SQUEEZEFS_SB_SMOKE=1` micro-grid is the per-commit-tier plumbing check | every release + after perf-relevant landings; baseline `.benchmarks/2026-07-18-multi-reference-scoreboard.md` incl. the 2026-07-18 allowlist-∅ addendum (JuiceFS-only lineage: `.benchmarks/2026-07-15-vs-juicefs-scoreboard.md`) | ~2–4 h (smoke ~10 min) |

**NVMe-oF fidelity tier scripts** (design-nvmeof-target-management §6.8, PR 5/N5): `tests/nvmeof_target_substrate.sh` (create/teardown/status/mkzram/snapshot — product-verb-driven dual-stack fabric, `:fideli-` ownership marker, test port slice 54000–54099, hugepage record/restore, built-in zero-residue snapshot diff), `tests/run_nvmeof_fidelity.sh` (the quick/full orchestrator above), `tests/guard_smoke.sh` (writer-guard matrix on product-shared namespaces, `--stack spdk|nvmet --loops N [--ptpl]`). They supersede the ad-hoc root gates that lived under `.agents/spdk-scoping/` (rig scripts removed from the tree 2026-07-18 — git history at `c615e3a`; the scoping record itself lives on as `.benchmarks/2026-07-17-spdk-target-scoping.md`). The multi-run discipline below applies verbatim to the ×10 guard matrices.

**The repro-port MANDATE (user ruling 2026-07-20 — non-negotiable):** for **every test we fail in pjdfstests, LTP, or fstests**, a **real cargo test that reproduces the found issue** MUST be written (the `tests/*_tests.rs` layer, red against the bug, green with the fix) and land WITH the fix. This is what makes the release-cadence external gate safe: a bug found once can never be reintroduced, because its repro runs on every commit forever. No fix for an externally-found failure merges without its cargo repro; where a scenario is genuinely not reproducible in-process (kernel-interface-only behavior), the exception is documented in the fix commit and the scenario joins `SQUEEZEFS_FSTESTS_QUICK` instead.

**`SQUEEZEFS_FSTESTS_QUICK`** (defined in `tests/run_fstests.sh`) is the **standing regression set**: every fstests case that has ever caught a real SqueezeFS bug, plus core fsx/fsstress soak, hole/punch/seek coverage, and mount basics. **Grow it whenever a new test surfaces a bug.**

**Fix-loop discipline (inventory once, then targeted):**
1. **Inventory once** — one full `-g auto` produces the complete failure list. Do **not** re-run the full suite between fixes.
2. **Targeted fix loop** — per failure *family* (cluster related failures; one root cause often spans several tests): tests-first fix → verify the single case with `sudo tests/run_fstests.sh generic/NNN` (minutes) → merge.
3. **One final sweep** — a single full `-g auto` after the last fix (and nightly thereafter) to catch fix interactions.

**Multi-run discipline (counted runs: soaks, ×N repro suites, acceptance medians):**
1. **A deterministic or attributable failure on any early run aborts the count** — stop, fix, then **restart the count from zero**. Runs completed before the fix verified the old binary; they are not creditable toward the fixed one's acceptance.
2. **Completing the remaining rolls is legitimate only as *declared* rate/signature gathering** — measuring how often a flake fires or capturing its tape for attribution — and must be labeled as such in the evidence note, never counted as acceptance.
3. **Acceptance counts always restart post-fix** (e.g. "green ×10" means ten consecutive greens of the final binary, not eight-before plus two-after).

---

## Benchmarks & Profiling

### Criterion benches

Two Criterion benches, both `harness = false`:

- `cargo bench --bench high_concurrency_bench` — in-memory lock / pool / cache contention.
- `cargo bench --bench squeezefs_bench` — exercises the full FS stack. Also contains the `CryptoCompressState` compression/encryption micro-benches.

**Bench smoke is part of the required gate:** `cargo bench --benches -- --test` runs one iteration of every bench without measurement (seconds, not minutes). It exists because `meta_lv_bench` sat silently broken from PR 8 until a perf investigation tripped over it — a panicking bench must fail the gate, not wait for the next baseline run.

**CI note:** Full Criterion *measurement* is optional in PR CI (long / noisy). Prefer **nightly** or manual baseline save:

```bash
cargo bench --bench squeezefs_bench -- --save-baseline main
```

### Profiling command set (release)

#### CPU (perf)

```bash
# One-shot sample while a mount + workload runs
perf record -g --call-graph dwarf -o perf.data -- \
  target/release/squeezefs mount …   # or attach: -p $(pidof squeezefs)

perf report -i perf.data
```

#### Heap (dhat)

```bash
cargo build --release --features dhat-on
# Run workload; on exit dhat prints / writes heap profile per its config
DHAT_OUT=dhat-heap.json target/release/squeezefs mount …
```

#### Causal (coz)

```bash
cargo build --release --features coz-on
coz run --- target/release/squeezefs mount …
```

Use coz/dhat **after** a known-good cargo test gate, against a representative mount.

---

## Module Map (non-obvious wiring)

- `src/main.rs` is the entire CLI: `format`, `status`, `clients`, `claim`, `mount`, `umount`, `bench`, `clone`, `tune`, `config`, `volume` (PR VL3: `add-data`/`list` — durable `vol-` identity, KD-5; VL4/VL5b added drain/remove + meta add/remove/migrate), `fsck`/`scrub` (PR VL6a/VL6b), `defrag` (PR VL7 — §5.7 four-axis model, `--report-only`/`--data`/`--meta`/`--fold`/`--rebalance`, live via the admin lane or offline via the D0-guarded coordinator), `job`, `df`, `storage`, `nvmeof`. `src/lib.rs` is the library surface (incl. `DataVolumeRecord` + `FormatConfig::resolved_data_volumes` legacy grandfathering). (The former `config` fake admin verbs were deleted in the volume-lifecycle program's PR VL1 — `docs/design-volume-lifecycle.md` — and refuse loudly naming their successors.)
- `src/fuse_client.rs` — FUSE daemon + `format_volume_ext` / `SqueezefsFilesystem`.
- `src/defrag.rs` — the PR VL7 online-defrag measurement engine (KD-11 four-axis model: D1 free-space contiguity, D2 file locality, D3 staged-extent pressure, D4 meta node occupancy; `frag_*` gauges + `--report-only`; the §5.8 offline report probe and D0-guarded offline mover harness). Movers live on the job fabric (`src/jobs.rs` `JobType::Defrag{Data,Meta,Fold}`) and REUSE existing machinery: the VL4 `move_one` engine with contiguity-aware destination picks, the W2 fold, the KV SMO compactor.
- `src/routing.rs` — `DataRouter` (progressive layout: inline / staged / striped) plus the `OnceCell`-held `CryptoCompressState`.
- `src/dlm.rs` — distributed lock manager (`DlmClient`, `acquire_lock`, fencing tokens, heartbeat renewal).
- `src/block_allocator.rs`, `src/nvme_dev.rs`, `src/storage.rs` — block allocation + NVMe/LVM pool plumbing.
- `src/cache/`, `src/tiering/` — tiered cache (GDS / RAM LRU / NVMe staging) and tier selection.
- `src/crypto_compress.rs` — compression (lz4/zstd) + RSA-wrapped symmetric encryption, applied across all three write paths via `process_write` / `process_read`.
- `src/jobs.rs`, `src/recovery.rs`, `src/nvmeof.rs`, `src/p2p.rs`, `src/config_ops.rs` — maintenance-job worker skeleton (throttle law + queue; the volume-lifecycle fabric extends it), crash recovery, NVMe-oF control, peer-to-peer, and the offline **guarded** admin verbs over durable on-volume state (cache-path policy, `volume add-data`/state — PR VL3). The historical `/dev/shm` runtime-config file is DELETED (VL3): live `enable`/`disable` health overrides ride the admin lane (`BackendRouter::set_health_override`); offline enable/disable is durable `DataVolumeRecord.state`.

## Repo-specific conventions

- **Cache-path policy (mount can never override):** staging/cache directories (`--disk-cache-paths`) are declared at **format** and recorded in the format config — the single source of truth. `mount` reads them from the config and **rejects** the flag with a loud error; format without the flag ⇒ a permanently **cache-less** filesystem (RAM tiers + direct block I/O; beyond-inline writes route striped — no staged layout, no conjured default staging dir). Changing paths is the admin op `squeezefs config set-cache-paths <sqmeta-uri> <paths...>` (format-grade live-client refusal; wipes the new dirs so generation stamping starts clean) with `get-cache-paths` for reads. Contracts pinned in `tests/cache_path_policy_tests.rs`.
- Metadata keys are namespaced via the `fs_key!("suffix")` macro and the global `FS_PREFIX` (`src/lib.rs`). Code that touches metadata keys must go through the macro, not hardcode prefixes.
- `WRITE_VERIFICATION` is a process-global `AtomicBool` toggled by `--write-verification` on mount; read-after-write checksum verification uses it. Library code should call `write_verification_enabled()` rather than reading CLI args.
- The architecture uses the **NVMe / NVMe-oF block** backend as the sole primary data path.

## Stats surface

Mounted volumes expose process metrics under the virtual **stats** inode (JSON), including layout mix, bg admission, uring queue-full, and lease acquire outcomes. Prefer these for live regression signals over ad-hoc logging. Per-volume metadata format + durability fields (`meta_format_version`, `meta_volume_atomicity[_physical]`) and the `meta_kv_*` family are listed under **Metadata format: v3 CoW KV (the only format)**; the metadata-throughput program's families (`writer_guard_*`, `meta_commit_group_*` / `meta_conveyor_*`, `meta_kv_fold_*`, `meta_kv_times_echo_*`, `fuse_op_phase_ns` / `fuse_create_under_lock_ns` / `fuse_op_watchdog_overdue`, `transport_commit_batch*`, `writeback_{superseded_noops,stale_token_retries,orphan_discards}`) are listed under **Metadata-throughput program (Implemented 2026-07)**; the L4 interception program (2026-07-19, `docs/design-preload-interception.md` §8, closing `.benchmarks/2026-07-19-l4-interception-closing.md`) adds the session-host families `ipc_sessions_{active,total,poisoned,reaped}`, `ipc_binds[_dev_override]`, `ipc_bind_refused_{version,nonce,flags,mode,budget,peercred}`, `ipc_admission_refusals`, `ipc_arena_bytes`, `ipc_descriptor_rejects` (with `ipc_descriptor_rejects`/`ipc_sessions_poisoned` as must-stay-0 tripwires), the data-plane families `ipc_ops_{read,write}` / `ipc_bytes_{in,out}` (**the charter-rule-4 engagement instrument** — an il benchmark row is INVALID unless their deltas account for the row's ops), `ipc_fast_path_serves` vs `ipc_async_handoffs` + the `ipc_fast_path_{lock,miss}_demotions` split (fast-path health), `ipc_service_threads`, and the W1 invalidation pair `ipc_inval_{notifies,suppressed}`; the L3 transport-economy program (2026-07-18) adds `transport_wake_{writes,elided}` — queue-eventfd wake coalescing (lever B): `writes/(writes+elided) ≈ 1` under saturated load means the coalescer stopped eliding (evidence `.benchmarks/2026-07-18-l3-transport-economy.md`); the random-small-write program's families (`patch_*` incl. the `patch_ineligible_*` decision ledger and the `patch_edge_rmw_reads`/`write_path_seed_read_bytes` must-stay-0 tripwires, `active_block_ooo_runs`, `write_through_blocks`, `overwrite_seed_*`, `parked_extent_bytes`/`parked_full_buffer_bytes`, `extent_*`, `fold_*`, `staged_rider_*`, `extent_records_*`, `compress_stored_raw`) are listed under **Random-small-write program (Implemented 2026-07)**.

**Read-path program families** (`docs/design-read-path.md` §Observability — semantics + regression thresholds there): `singleflight_waiter_result_serves` (R1a cohort serves); `hot_block_{hits,misses,evictions,probation_drops,dehydrate_skips,current_bytes}` (R4 RAM tier); `read_fill_publishes_skipped` / `read_tier_admissions` / `read_tier_admission_ghost_hits` / `read_tier_admission_mode` (R1b second-touch admission — skipped ≈ streamed cold blocks); `prefetch_{issued,completed,wasted,inflight_bytes,window_hwm,foreground_waits,evicted_unconsumed,active_streams}` (R2 pipeline — `evicted_unconsumed` is the refetch-spiral detector); `ranged_{reads,read_bytes,read_unaligned_bounces,read_rebinds,read_ghost_escalations}` (R3 — `ranged_read_bytes` vs user bytes is the rand-4k amplification bound; `get_obj` counts ranged ops by design; `ghost_escalations` = hybrid-I/O second-touch → whole-block admissions); `read_odirect_{tier_serves,ghost_admits}` + `read_device_true_reads` + `direct_device_true` (hybrid I/O, 2026-07-15 user directive: O_DIRECT serves/admits like buffered by default — the mount-scoped `-o direct_device_true` / `SQUEEZEFS_DIRECT_DEVICE_TRUE=1` escape restores strict device-true O_DIRECT for `.benchmarks` amplification measurement); `mem_budget_{bytes,pressure_bytes,gauge_sum_bytes,unreclaimable_bytes,level,yellow_events,red_events,hard_backstops,backstop_active,tier_publish_paused,floors_clamped,dehydrate_paused,components{…}}` + `read_tier_publishes_paused` + `parked_gate_{waits,self_flushes,timeouts}` (R5 authority — red_events with no OOM is the designed outcome under pressure; `hard_backstops`/`parked_gate_timeouts` growing on quiet workloads means a convergence regression, §5.7 Red semantics).

---

## Branch & Commit Workflow

The integration branch is **`dev`** (not `main`; `main` is reserved for releases). `origin/HEAD` points at `origin/dev`.

- Branch from `dev`: `feat/`, `fix/`, `refactor/`, `perf/`, `docs/` prefixes. Never commit directly to `dev`.
- Tests-first cycle: write failing tests → implement → refine → commit each logical step separately.
- Merge with `--ff-only`; rebase feature branches if `dev` has diverged. Delete branches after merge.
- Conventional commits: `type(scope): description`. Messages explain WHY.
- **Versioning is git commits only** (`docs/operations.md` §Versioning & releases): release tags (`stable-*`/`lts-*`, created manually as a release act) are the only release names — never introduce semver bumps (`Cargo.toml`'s `version` is a cargo-internal placeholder).

See the full TDD Development Workflow section above for the detailed phased process.

---

## Authoritative Supporting Documents

While this file is the combined one source of truth for agents and contributors:

- `README.md` / `QUICKSTART.md` — user-facing CLI, NVMe-oF, and bare-metal setup.
- `docs/reference-clients-survey.md` — the DAOS-client + JuiceFS reference fast-FUSE client survey (see **Reference fast-FUSE clients** above).
- `docs/design-preload-interception.md` — the L4 LD_PRELOAD POSIX-interception data path (`libsqueezefs-il`): charter labeling discipline, IPC protocol/gates, the survey's P3-A adoption item; since v1.1 also the **libaio interposers** (`io_setup`/`io_submit`/`io_getevents` lane-split — libaio drivers are valid, engagement-verified il instruments; Rev 4 + `.benchmarks/2026-07-19-v1.1-libaio-interposers.md`).
- `docs/PROFILING_AND_GATES.md` (historical) — content now consolidated here.

---

**End of combined AGENTS.md.** Follow these instructions exactly. When working in subdirectories, check for additional project instruction files (AGENTS.md, Claude.md, etc.) but prefer this root document.
