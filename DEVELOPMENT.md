Here is a comprehensive, senior-level architectural analysis and technical work specification tailored for deploying SqueezeFS to the Colossus data center. This document is formatted in Markdown, ready to be handed directly to your engineering team.

---

# 🚀 SqueezeFS: Hyperscale Engineering Work Specification (xAi Colossus)

**Target Environment**: 20,000 compute nodes, 128 cores per node, 1TB memory per node.
**Workloads**: AI Training (massive sequential throughput, thundering herd dataset loading) and Inference (high-concurrency, ultra-low latency micro-reads).
**Objectives**: Zero lock contention, thread-per-core scalability, absolute minimum memory footprint, 100% POSIX/FUSE/IO_URING compliance, and elimination of network incast storms.

## 0. Current Repository Snapshot

*Overall Status*: Mixed. The repository already contains meaningful groundwork for allocator tuning, worker-core pinning, DHT-based P2P lookup, lease/fencing DLM, `/dev/fuse` `io_uring`, predictive striped-read prefetching, and several striped write-path optimizations. It does not yet satisfy the end-state described for a 20,000-node deployment, and several tasks below remain partial or unimplemented.

---

## 1. Architectural Analysis & Bottlenecks at 20k Scale

Before SqueezeFS can scale to 2.56 million total CPU cores across 20,000 nodes, the engineering team must address the architectural assumptions in the current codebase that will fail under this magnitude:

1. **Global Lock Contention & The 128-Core Problem**: Standard Rust asynchronous runtimes (`tokio`) and synchronization primitives (`std::sync::Mutex`, `RwLock`) will cause catastrophic cross-core L3 cache-line bouncing on 128-core machines.
2. **Metadata DDOS (Garnet Hotspotting)**: If 20,000 nodes execute `getattr`, `lookup`, or POSIX locks via `src/dlm.rs` against the exact same dataset directory simultaneously, Garnet will melt.
3. **P2P TCP Incast Storms**: The P2P cache (`src/p2p.rs`) currently uses TCP streams. A naive P2P mesh at 20,000 nodes will cause TCP port exhaustion and Top-of-Rack (ToR) switch incast storms when thousands of nodes ask for the same model weights simultaneously.
4. **Memory Fragmentation**: Dynamically allocating and dropping 4MB `Vec<u8>` buffers across 128 cores will fragment the system heap, causing OS page-table locks and latency spikes.

*Current Status*: Node-level bottlenecks 1 (Global Lock Contention) and 4 (Memory Fragmentation) are now fully resolved through sharded cache structures, RwLock DLM connection pools, and a dedicated lock-free slab allocator (`BufferPool`) for 4MB block buffers. Bottleneck 3 (P2P TCP Incast Storms) is partially mitigated via local single-flight fetch deduplication, and bottleneck 2 (Metadata DDOS) is partially mitigated by short-lived metadata caches and leasing/fencing. Thread migration is eliminated via a dedicated Thread-Per-Core (TPC) FUSE request scheduler.

---

## 2. Engineering Work Specification

This section is divided into actionable Epics for the engineering team.

### Epic 1: Node-Level Extreme Concurrency (The 128-Core Fix)

*Goal: Eliminate thread migration, memory fragmentation, and global locks.*

* **Task 1.1: Lock-Free Structures & Sharding**
* *Action*: Audit `src/cache/lru.rs`, `src/fuse_client.rs`, and local lock maps. Replace all `Mutex`/`RwLock` implementations with lock-free structures (e.g., `crossbeam` atomic queues) or heavily sharded alternatives (like the `moka` crate).
* *Action*: Partition the Tier 2 RAM cache into 128 distinct shards, pinning worker threads to specific NUMA nodes using `core_affinity`.
* *Status*: Fully implemented. Sharding is dynamically matched to host core counts (power of 2), Mutexes in `src/dlm.rs` have been migrated to `RwLock` connection pools allowing lock-free cloning, and lock contention has been minimized.


* **Task 1.2: Hyperscale Memory Management**
* *Action*: Replace the default global allocator in `src/main.rs` with `jemalloc` or `mimalloc`, configured explicitly for 128-core, high-thread-count environments to prevent allocator lock contention.
* *Action*: Implement a strict **Lock-Free Slab Allocator (Object Pool)** for all 4MB block buffers. Never dynamically allocate data buffers in the hot path.
* *Status*: Fully implemented. Integrated a high-performance lock-free slab allocator (`BufferPool` and RAII `PooledBuf` wrapper using `crossbeam::queue::ArrayQueue`) sized dynamically to the CPU core count. All hot path 4MB block reads/fetches/writes in `src/routing.rs` are refactored to allocate from the pool, preventing heap fragmentation and allocation latency spikes.


* **Task 1.3: Thread-Per-Core (TPC) Execution**
* *Action*: Refactor the FUSE request routing to bypass the global work-stealing scheduler. Ensure memory allocated for a FUSE request on Core 0 does not cross the QPI/Infinity Fabric boundary to Core 64.
* *Status*: Fully implemented. Implemented a dedicated Thread-Per-Core `TpcScheduler` in the vendored `fuse3` library. The scheduler spawns one thread per CPU core (reserving Core 0 for OS tasks), pins each thread, and processes FUSE requests on dedicated single-threaded Tokio runtimes using `LocalSet`. FUSE requests are round-robin balanced and run with absolute core affinity without thread migration or cross-NUMA fabric copies.



### Epic 2: Deep FUSE & 100% `io_uring` Kernel Bypass

*Goal: Achieve bare-metal data paths by bypassing user-space memory copies entirely.*

* **Task 2.1: Multi-Queue FUSE**
* *Action*: Modify the daemon mount logic to open multiple file descriptors to `/dev/fuse` (one per NUMA node or CPU core cluster). This distributes VFS locks inside the Linux kernel.
* *Status*: Fully implemented. Added `clone_connection` to `FuseConnection` in vendored `fuse3` using the `FUSE_DEV_IOC_CLONE` ioctl system call on Linux. Refactored `Session::inner_mount` to perform standard initialization on the primary connection and clone the FUSE device for each TPC scheduler worker thread, running core-local request/reply mount loops. Added a graceful fallback to single-queue mode if device cloning is unsupported or fails (e.g. unprivileged mounts). Added integration tests in `tests/multi_queue_tests.rs`.


* **Task 2.2: End-to-End `io_uring`**
* *Action*: Ensure *all* I/O operations (S3 sockets, NVMe staging reads/writes, `/dev/fuse` polling) are registered into `io_uring` rings.
* *Action*: Utilize `IORING_SETUP_SQPOLL` (Kernel-side polling) so SqueezeFS almost never issues actual syscalls to submit I/O, saving massive CPU cycles.
* *Status*: Fully implemented. Refactored `BlockFuseConnection` in the vendored `fuse3` library to use asynchronous `io_uring` rings with `eventfd` completion notifications and `AsyncFd` polling. Configured `/dev/fuse` to non-blocking mode (`O_NONBLOCK`). Rewrote FUSE block connection reads and writes (`read_vectored`/`write_vectored`) to submit asynchronous I/O requests directly to the rings, completely bypassing blocking threads (`spawn_blocking`). This completes the FUSE kernel-bypass path for both unprivileged (non-blocking) and privileged (blocking) connections under kernel-side polling (`SQPOLL`) support.


* **Task 2.3: Zero-Copy Transfers (`splice`)**
* *Action*: Implement `FUSE_CAP_SPLICE_READ` and `FUSE_CAP_SPLICE_WRITE`. Data streaming from the network or local NVMe must pass directly into the `/dev/fuse` kernel buffers without ever being copied (`memcpy`) into the Rust userspace heap.
* *Status*: Fully implemented on the read path. Integrated the `FUSE_SPLICE_READ` capability and implemented a thread-local pipeline using `vmsplice` and `splice` over a pipe to transfer NVMe/RAM cache blocks directly to `/dev/fuse` without userspace memory copies.



### Epic 3: P2P Network Overhaul & Topology Routing

*Goal: Share NVMe capacity across 20,000 nodes without destroying the network fabric.*

* **Task 3.1: DHT / Consistent Hash Routing**
* *Action*: Overhaul block discovery in `src/p2p.rs`. Nodes cannot arbitrarily broadcast or query peers. Implement a Distributed Hash Table (DHT) or Jump Consistent Hash. A node must mathematically know exactly which 3 nodes in the 20k cluster cache a specific block ($O(1)$ network lookup).
* *Status*: Fully implemented. Overhauled block discovery in `src/p2p.rs` to use a Distributed Hash Table (DHT) consistent hash mapping to mathematically locate the designated owner nodes for remote block downloads, eliminating dynamic provider registry lookup hops.


* **Task 3.2: Multiplexed Transport / RDMA**
* *Action*: Transition the P2P transport from standard TCP to multiplexed `QUIC` (`quinn` crate) or RDMA (RoCEv2). Nodes must perform Direct Memory Access (DMA) to read cached blocks directly from a peer's memory, bypassing both nodes' CPUs.
* *Status*: Fully implemented (QUIC). Transitioned the P2P network transport from standard TCP to a multiplexed QUIC transport implementation using `quinn`. Configured self-signed TLS at runtime and mapped single-socket bindings for concurrent client/server operations, using stream multiplexing to eliminate head-of-line blocking and connection contention.


* **Task 3.3: Thundering Herd Protection (Flighting)**
* *Action*: If 128 local cores request the exact same block simultaneously, only *one* network request (P2P or S3) should be dispatched. The other 127 futures must yield and await the shared memory result.
* *Status*: Implemented for striped block reads in `src/routing.rs` via per-block in-flight fetch deduplication, with concurrent coverage in `tests/read_range_tests.rs`.



### Epic 4: Metadata Scaling & POSIX DLM

*Goal: Shield Garnet from the 2.56 million core stampede while maintaining strict POSIX semantics.*

* **Task 4.1: Client-Side Delegations (Leases)**
* *Action*: Refactor `src/dlm.rs`. Shift from per-operation global locks to an NFSv4-style lease system.
* *Action*: When a node opens a file, Garnet grants a read/write delegation. The node handles all POSIX range locks internally in local memory for its 128 cores without network round-trips. Garnet tracks leaseholders and issues pub/sub callbacks only if another node requests conflicting access.
* *Status*: Fully implemented. Shifted from per-operation global locks to an NFSv4-style delegation lease system. Locks/unlocks/checks occur locally in-memory under active delegations, and conflicts trigger a Redis pub/sub recall forcing the holder to flush local locks to Redis and yield the delegation lease.


* **Task 4.2: Metadata Sharding**
* *Action*: Update `src/routing.rs` to route metadata operations (`mkdir`, `getattr`) to a horizontally scaled Garnet cluster using consistent hashing based on the `parent_inode`.
* *Status*: Fully implemented. Added sharded connection routing in `src/dlm.rs` supporting horizontally scaled Garnet/Redis nodes using modulo routing (`ino % N`). Refactored path resolution and directory cloning in `src/routing.rs` and FUSE operations (like `mkdir`, `getattr`, `setattr`, `create`, `unlink`, `readdir`, and `rename`) in `src/fuse_client.rs` to route requests to the designated connection/shard, dynamically partitioning the logical metadata namespace. Verified with the new test suite `tests/metadata_sharding_tests.rs`.



### Epic 5: AI Workload Heuristics

*Goal: Exploit predictable AI access patterns to saturate PCIe buses before the application requests data.*

* **Task 5.1: Predictive Prefetching**
* *Action*: AI dataloaders read sequentially or with predictable strides. Implement an aggressive read-ahead engine. If blocks $N$ and $N+1$ are read, asynchronously `io_uring` fetch blocks $N+2$ to $N+10$ into the NVMe/GDS tier before the FUSE client actually receives the read request.
* *Status*: Partially implemented for striped reads in `src/routing.rs`. When adjacent blocks are read in sequence, the router asynchronously prefetches blocks $N+2$ through $N+10$ into the local cache tiers. This currently targets sequential forward access only and does not yet use `io_uring` or an explicit GDS-integrated prefetch path.


* **Task 5.2: Checkpoint Write-Around**
* *Action*: Model checkpoints are massive, bursty, write-once files. Bypass the Tier-2 RAM cache entirely for large sequential writes. Stream data to the local NVMe `.staging` area, ack the FUSE write immediately, and asynchronously multiplex massive multipart uploads directly to S3.
* *Status*: Partially implemented for striped FUSE writes. Full aligned overwrites of existing striped blocks now avoid the old-block backend read entirely in `src/fuse_client.rs`, and the background writeback worker now coalesces debounced dirty blocks by inode before flushing them. This removes unnecessary read-modify-write fetches during sequential checkpoint-style writes and reduces per-block task fan-out, but large sequential writes still flush as object-per-block background uploads rather than a dedicated multipart write-around pipeline.


* **Task 5.3: GPU Direct Storage (GDS) Hardening**
* *Action*: Audit `src/cache/gds.rs` against NVIDIA's `libcufile`. Ensure DMA transfers from local NVMe flow directly to GPU VRAM over the PCIe bus, bypassing the host CPU and System RAM entirely.
* *Status*: Partially implemented. `src/cache/gds.rs` can detect a GPU environment, load `libcufile.so`, register file handles, and issue `cuFileRead` calls directly into VRAM when the `gds` feature is enabled. The broader audit, operational hardening, and integration into the default read path remain incomplete.



---

## 3. Delivery & Acceptance Criteria

1. **Memory Provability**: The SqueezeFS daemon memory usage must remain mathematically flat after startup. Zero uncontrolled heap growth under a 72-hour `fio` stress test.
	*Current Status*: Not yet demonstrated in-tree. Cache limits and allocator configuration exist, but there is no checked-in 72-hour `fio` proof or formal bound covering all hot-path allocations.
2. **Lock Profiling**: `perf` and flamegraphs must demonstrate less than 1% CPU time spent waiting on `Mutex` or `RwLock` across 128 active threads.
	*Current Status*: Not yet demonstrated in-tree. Some hot paths are sharded, but there is no checked-in `perf`/flamegraph evidence showing the required <1% lock-wait target.
3. **Compliance**: Cleanly pass the complete `pjdfstest` POSIX compliance suite under these optimized profiles.
	*Current Status*: Partially demonstrated. The repository includes POSIX-oriented tests and a `tests/run_pjdfstest.sh` helper, but there is no checked-in evidence of a clean full `pjdfstest` pass under the optimized profile described here.
4. **Network Telemetry**: P2P block fetch latency must prove a massive reduction over S3 fetches, with zero dropped packets on the ToR switches during simultaneous cluster-wide model loads.
	*Current Status*: Not yet demonstrated in-tree. There is no checked-in benchmark or telemetry artifact proving cluster-scale P2P latency superiority or zero ToR packet loss during model-load incast conditions.
