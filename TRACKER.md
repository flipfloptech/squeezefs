# Squeezefs Feature Completeness & Roadmap Tracker

This document tracks the current implementation status of Squeezefs, analyzes gaps to reach feature completeness relative to JuiceFS, and lists concrete Todo items for the next stages of development.

---

## 1. Architectural Alignment

Squeezefs is designed as a high-performance, decoupled distributed filesystem for HPC workloads:
- **Metadata & Lock Engine:** Microsoft Research Garnet (latch-free, epoch-based, Redis/RESP compatible).
- **Data/Object Storage:** RustFS (high-throughput S3-compatible backend).
- **Client Daemon:** Rust FUSE client utilizing `tokio` multi-threaded runtime and `io_uring` polling loop.

---

## 2. Feature Implementation Status

| Feature Area | Sub-Feature | Status | Notes |
|---|---|---|---|
| **FUSE / POSIX API** | Core File Ops | **Complete** | `lookup`, `getattr`, `setattr`, `readdir`, `symlink`, `readlink`, `link`, `unlink`, `rename`. |
| | POSIX Hardening | **Complete** | Link/unlink restrictions, directory loop checks, parent directory timestamp updates. |
| | Permission Check | **Complete** | Kernel enforces standard user/group and mode checks via `default_permissions`. |
| | **Random-Access Writes** | **Complete** | Offset-aware Read-Modify-Write (RMW) implemented for all layouts with size transitions. |
| **Progressive Layout** | Micro-Files (<64KB) | **Complete** | Inlined directly into Garnet (`inline_data:<path>`). Zero S3 calls. |
| | Small Files (64K-4M) | **Complete** | NVMe staged, immediately ACKed to OS, merged asynchronously by worker. |
| | Large Files (>4MB) | **Complete** | Sliced into 4MB blocks, uploaded via parallel thread pool. |
| **Tiered Caching** | Tier 1 (GPU Direct) | **Complete** | Detects CUDA/Nvidia devices; performs dynamic loading of libcufile.so and fallback GDS RDMA transfers. |
| | Tier 2 (System RAM) | **Complete** | Thread-safe LRU cache. Configurable capacity via size (e.g. 128GB) or percentage (e.g. 50%). |
| | Tier 3 (NVMe Staging) | **Complete** | Configurable size/percentage capacity check. Supports multiple directories/disks with hash-based routing. |
| **Operational Tools** | CLI Subcommands | **Complete** | Clap-based interface supporting `mount` and `bench` subcommands. |
| | Benchmark Suite | **Complete** | Modeled after `juicefs bench`, reporting rates, latencies, and backend diffs. |
| | Crash Recovery | **Complete** | Scans staging dir on boot, cross-references Garnet, recovers incomplete uploads. |

---

## 3. JuiceFS Gap Analysis & Feature Completeness

JuiceFS is the gold standard for POSIX distributed filesystems. To achieve absolute parity and HPC readiness, Squeezefs must address the following gaps:

### A. POSIX Random-Access Writes (Highest Priority)
* **JuiceFS:** Handles arbitrary write offsets, block overlaps, and file truncations using a local buffer pool and slice-offset mappings in Redis.
* **Squeezefs Current State:** **Complete.** Full offset-aware RMW and layout transitions implemented.

### B. High Availability & Sharding
* **JuiceFS:** Shards metadata slots across multiple database instances and uses replication for read scaling.
* **Squeezefs Current State:** Connects to a single Garnet instance.
* **Solution:** Configure connection to cluster topology using Garnet's cluster slots (16,384 slots) and read-replicas.

### C. GPU Direct Storage (GDS) Linkage
* **JuiceFS:** Does not natively support bypass to VRAM. Squeezefs can differentiate itself here.
* **Squeezefs Current State:** Simulated.
* **Solution:** Integrate `libcufile.so` user-space APIs to coordinate RDMA bypass transfers from RoCE NICs directly to VRAM.

### D. FUSE io_uring Engine
* **JuiceFS:** Uses standard libfuse/FUSE channels.
* **Squeezefs Current State:** Spawns an `io_uring` polling loop but dispatches requests using standard `fuse3` event-loop wrappers.
* **Solution:** Fully integrate the `io_uring` loop to bypass the tokio channel overhead on FUSE event reaping.

---

## 4. Checklist & Todos

### Phase 1: POSIX Write Correctness (Next Step)
- [x] Modify `DataRouter::write_file` to accept `offset` parameter.
- [x] Implement Read-Modify-Write (RMW) for staged files:
  - If a write is at an offset, retrieve the staged block, patch the bytes at the offset, and rewrite the block.
- [x] Implement RMW for striped files:
  - Identify which 4MB blocks are affected.
  - Download the affected blocks, modify the bytes, and re-upload.
- [x] Add integration tests in `tests/metadata_tests.rs` for partial writes and random access offsets.

### Phase 2: Metadata Scale & HA
- [x] Implement cluster slots mapping for Garnet to scale to 15,000+ nodes.
- [x] Implement read scaling: redirect read-only metadata lookups to Garnet replicas.
- [x] Add heartbeat lease renewal logging in `dlm.rs`.

### Phase 3: GPU Direct Storage (GDS) integration
- [x] Add conditional compilation features for CUDA / GDS.
- [x] Use `dlopen` to load `libcufile.so` dynamically if present.
- [x] Implement direct block transfer mapping to GPU virtual addresses.

### Phase 4: io_uring Performance Tuning
- [x] Hook the low-level `io_uring` polling loop in `src/fuse_client.rs` directly to the session message dispatcher.
- [x] Benchmark FUSE latency using `squeezefs bench` before and after direct loop integration.

### Phase 5: Copy-on-Write (COW) Writes & Snapshot Cloning
- [x] Refactor `write_striped` to upload blocks to unique S3 keys and update `block_map:<block_map_id>` in Garnet.
- [x] Refactor `read_file` (striped layout arm) to lookup the S3 keys from `block_map:<block_map_id>`.
- [x] Implement `clone_file` in `DataRouter` to support zero-copy metadata-level cloning of inline, staged, and striped files.
- [x] Add `squeezefs clone` CLI subcommand in `src/main.rs`.
- [x] Add integration tests in `tests/clone_tests.rs` verifying COW and cloning behavior.
