# Squeezefs Architectural & Behavioral Rules

## 1. High-Performance Distributed Filesystem Architecture

### Target Scale & Layout
* **Scale:** 15,000+ Concurrent Nodes.
* **Architecture Type:** Decoupled metadata (Garnet) + **block data** (local NVMe / NVMe-oF), exposed via POSIX FUSE.

### Technology Stack
* **Client Daemon (FUSE Engine):** Built in **Rust** using the asynchronous `tokio` runtime and `io_uring` polling over `/dev/fuse`.
* **Metadata & Distributed Lock Manager (DLM):** Microsoft Research **Garnet** (RESP-compatible). Primary meta store for attrs, layout maps, leases, and volume format.
* **Data Backend (primary):** **NVMe / NVMe-oF block devices** via `NvmeBlockDev` (io_uring workers). Progressive layouts (inline / staged / striped) live on this path.
* **Data Backend (optional / secondary):** S3-compatible paths may exist in the tree for historical or multi-backend experiments; **do not assume S3 is primary** when changing the data plane. Prefer the NVMe path that `format` / `mount --volume` and integration tests exercise.

---

## 2. Progressive Data Layout & I/O Routing

Logical file growth uses three layouts (thresholds are implementation-defined; current code uses ~4 KiB inline, up to ~4 MiB staged when staging dirs exist, else striped):

1. **Inline (tiny):** Payload in Garnet (`inline_data:…`) with type `inline`.
2. **Staged (small):** Local NVMe staging (`file_id` + optional `mapping:…`); writeback/flush promotes to durable blocks.
3. **Striped (large):** 4 MiB (configurable) blocks on the active block backend with `block_map:…` and refcounts.

Writes that grow past thresholds promote layouts **durably** (block I/O before meta type flip — see P0 layout atomicity).

---

## 3. Distributed Lock Manager (DLM) & Consistency

POSIX FUSE locks map to cluster leases on Garnet:
* **Acquisition:** `SET NX` + fencing token `INCR` (no Lua required for lock grant).
* **Granularity:** File-level or byte-range; never directory-wide for data.
* **Leases & Heartbeats:** TTL + background renewal; local caches must re-validate after lock key loss.
* **Fencing Tokens:** Monotonic per-file tokens; writers present tokens; stale tokens → `FencingTokenExpired` / reject.

---

## 4. Tiered Caching & Paths

* **Tier 1 (optional GDS):** GPU Direct path when `gds` feature is enabled.
* **Tier 2 (RAM LRU):** Sharded Clock/LRU read & write caches.
* **Tier 3 (Local NVMe staging / read cache):** Staging segments + optional read block cache; dehydrate on eviction.

---

## 5. FUSE Client & Asynchronous I/O

* Work-stealing / multi-thread tokio; core pinning where configured.
* **Block path io_uring:** `NvmeBlockDev` worker (bounded request queue, backpressure, fixed-file register when available).
* **Path file I/O io_uring:** `crate::uring_fs` for ad-hoc local files (e.g. GDS cache materialize). Staging **mmap** segments stay mmap for zero-syscall get/put.
* **FUSE transport:** vendored **fuse3** — classical `/dev/fuse` framing only for `FUSE_INIT` and notifications; **FUSE-over-io_uring is required** for the request hot path after INIT (`flags2` `FUSE_OVER_IO_URING`, `REGISTER` / `COMMIT_AND_FETCH`, Linux 6.14+ / 7.x). No opt-out and no classical fallback if setup fails. Tune with `SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH` / `_QUEUES` only.
* **Not uring:** Garnet/Redis TCP, TLS peer paths.

---

## 6. Metadata Cluster Topology

* Optional multi-shard Garnet URLs; keys for volume control use `fs_prefix` / `fs_key!`.
* **Layout keys** (`metadata:…`, `inline_data:…`, `block_map:…`, `mapping:…`, `active_block:…`) are **unprefixed historical** forms — use `crate::keys::*` helpers; do not migrate under `FS_PREFIX` without an on-disk format change.

---

## 7. Error Handling & Crash Recovery

* FUSE op timeouts; staging recovery on remount (`recover_staging`) with fence + layout checks.
* Stale fencing tokens discard staged work; missing inode meta discards orphan active blocks.
* Write verification is **opt-in** (`--write-verification`, optional sample rate).

---

## 8. Lock order & connection scope (must not)

Always acquire in this order; **never invert** (P1-9):

1. `active_inode_locks` (per-inode `RwLock`) — FUSE op serialization  
2. `lease_locks` (per-inode) — only while acquiring/refreshing DLM lease  
3. `BLOCK_FLUSH_LOCKS` (per block) — active-block mutation  
4. DLM/Redis — network meta work  

**Must not:**
* Hold inode **write** guard across long backend I/O when block locks suffice (striped data path = meta-prep only under write lock — P1-8).
* Hold a **pooled Garnet/Redis connection** across durable NVMe / staging I/O (P1-10: open → short meta → drop → I/O → re-acquire for commit).
* Acquire (1) while holding (3).
* Burn fencing tokens on failed lock `SET NX` (SET then INCR only on success).

---

## 9. OS / fabric notes

* Dirty ratios and fabric tuning remain operator concerns for large clusters.
* Primary data path does **not** require S3; NVMe-oF is for remote block exposure of the same block backend model.
