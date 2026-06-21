# Squeezefs Architectural & Behavioral Rules

## 1. High-Performance Distributed Filesystem Architecture

### Target Scale & Layout
* **Scale:** 15,000+ Concurrent Nodes.
* **Architecture Type:** Decoupled Metadata / Object Backend with Local POSIX FUSE Mount.

### Technology Stack
* **Client Daemon (FUSE Engine):** Built in **Rust** using the asynchronous `tokio` runtime and `io_uring` polling over `/dev/fuse`.
* **Metadata & Distributed Lock Manager (DLM):** Microsoft Research **Garnet** (RESP-compliant, latch-free, epoch-based multithreaded architecture).
* **Data Backend:** **RustFS** (S3-compatible, decentralized object store optimized for massive parallel throughput).

---

## 2. Progressive Data Layout & I/O Routing

Data is logically chunked (64MB), sliced into mutations, and physically stored in blocks (max 4MB). Write routing is determined dynamically by file size:

1. **Micro-Files (< 64KB): KV Inlining**
   - Bypasses the object store completely. Raw byte payloads are written directly into the Garnet key-value store alongside file metadata.
2. **Small Files (64KB - 4MB): Asynchronous Batching**
   - Staged locally on a hidden NVMe staging directory. The write is instantly acknowledged to the OS.
   - A background thread merges small files into a single 4MB physical block, uploads it to RustFS, and updates Garnet with byte offset mappings.
3. **Large Files (> 4MB): Parallel Striping**
   - The data stream is sliced into 4MB blocks. A thread pool executes concurrent S3 `PUT` requests to stripe blocks across RustFS volumes.

---

## 3. Distributed Lock Manager (DLM) & Consistency

POSIX FUSE locks are translated to global cluster locks:
* **Acquisition:** Executed via Garnet `SETNX` commands.
* **Granularity:** Strictly file-level or byte-range level (never at the directory level).
* **Leases & Heartbeats:** Lock leases are issued with strict TTLs (e.g., 5 seconds). A background thread continuously renews open locks at 1/3 of the TTL (every ~1.6s).
* **Fencing Tokens:** Garnet issues a monotonic integer upon lock acquisition. This fencing token is sent to RustFS. Writes with older/expired fencing tokens are rejected by RustFS to prevent split-brain write conflicts.

---

## 4. Tiered Caching & Zero-Copy Paths

* **Tier 1 (GPU Direct Storage):** Intercepts GPU reads and routes RDMA transfers directly from RustFS to VRAM, bypassing the host kernel and system RAM.
* **Tier 2 (Unified System RAM):** Least Recently Used (LRU) cache dynamically sized to 20% of system RAM by default.
* **Tier 3 (Local NVMe Staging):** Hidden `.staging` directory on local NVMe handles async writes and cache overflow reads.

---

## 5. FUSE Client Implementation & Asynchronous I/O

* **Thread Pool:** Work-stealing pool bound strictly to physical CPU cores (reserving at least 1 core for OS kernel tasks).
* **`io_uring`:** Main polling loop harvests FUSE requests from `/dev/fuse` without blocking. Network and disk IO are delegated strictly to background workers.
* **FUSE parameters:**
  - `max_read` & `max_write` set to absolute kernel maximums (e.g., `1048576`).
  - `writeback_cache` enabled to group sequential micro-writes.
  - `async_dio` enabled to allow overlapping asynchronous direct I/O.

---

## 6. Metadata Cluster Topology & High Availability

* **Sharding:** 16,384 slots distributed across primary Garnet instances.
* **Log-Shipping:** Append-Only-Files (AOF) enabled on primaries and shipped to replicas.
* **Read Scaling:** Read-heavy metadata lookups route to replicas using the `READWRITE` protocol flag.

---

## 7. Error Handling & Crash Recovery

* **FUSE Timeout Protection:** Task operations must fail-fast within 2 seconds on network stalls, falling back to NVMe disk staging. Hangs are strictly prohibited.
* **Stale Write Discard:** Writes rejected by RustFS due to expired fencing tokens are discarded, and `EIO` is propagated to the OS.
* **Daemon Crash Recovery:** Upon reboot, the daemon scans `.staging` for pending writes, verifies state with Garnet, uploads completed blocks to RustFS, and patches metadata before opening the mount.

---

## 8. OS Kernel & Network Fabric Tuning Requirements

* Elevate `vm.dirty_ratio` and `vm.dirty_background_ratio` on client nodes to buffer write sequences in RAM.
* Use a non-blocking Clos network topology with RoCE (RDMA over Converged Ethernet) to bypass host network stack overhead.
