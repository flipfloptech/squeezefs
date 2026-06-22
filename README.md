# Squeezefs

Squeezefs is a slimmed-down, high-performance distributed filesystem featuring a decoupled metadata/object store backend and a local POSIX FUSE client daemon.

Designed to operate at scale (15,000+ concurrent nodes), it delivers bare-metal file throughput by leveraging asynchronous network and file architectures, client-side caching, and multi-rail network load balancing.

---

## Key Features & Architecture

```
   +-------------------------------------------------+
   |                  FUSE Client                    |
   |   (Rust, Tokio Event Loop, io_uring Polling)    |
   +--------+-------------------------------+---------+
            |                               |
  (Locking & Metadata)              (Block I/O Data)
            |                               |
            v                               v
  +-------------------+           +-------------------+
  | Microsoft Garnet  |           |      RustFS       |
  |  (RESP Metadata)  |           | (S3-Compatible Object)
  +-------------------+           +-------------------+
```

### 1. Asynchronous POSIX FUSE Daemon
Built in **Rust** using the asynchronous `tokio` runtime and `io_uring` polling over `/dev/fuse` to process OS requests efficiently.

### 2. Progressive Data Layout & I/O Routing
Writes are dynamically routed based on file sizes to optimize storage overhead and network latency:
- **Micro-Files (< 64KB):** Inlined directly in the Microsoft Garnet key-value store alongside metadata.
- **Small Files (64KB - 4MB):** Staged locally on NVMe and asynchronously merged into 4MB physical blocks uploaded to S3.
- **Large Files (> 4MB):** Sliced into 4MB blocks and striped concurrently across RustFS volumes.

### 3. Distributed Lock Manager (DLM) & Consistency
Translates POSIX FUSE locks to cluster-wide locks in Garnet using `SETNX` commands, protected by heartbeats and monotonic fencing tokens to prevent split-brain write conflicts.

### 4. Tiered Caching & Zero-Copy Paths
- **Tier 1 (GPU Direct Storage - GDS):** Routes RDMA transfers directly from S3/RustFS to VRAM, bypassing the host CPU/RAM.
- **Tier 2 (Unified System RAM):** LRU cache dynamically sizing to 20% of system RAM.
- **Tier 3 (Local NVMe Staging):** Staging directory (`.staging`) for async writes and local caching of read blocks to avoid RTT latency.

### 5. Multi-NIC (Multi-Rail) Network Load Balancing & HA
Binds outbound client connections to multiple configured physical interfaces (source IPs). Distributes traffic round-robin across NICs and automatically fails over on interface drops.

### 6. Built-in HPC Auto-Tuning
Includes built-in host auto-tuning (`squeezefs tune`) to optimize virtual memory dirty page ratios, TCP socket buffers, and FUSE connection thresholds.

---

## Subcommands & CLI Usage

Squeezefs exposes a clean CLI to manage formats, mounts, status, performance benchmarks, and optimize systems:

* **Format squeezefs Volume:**
  ```bash
  squeezefs format <name> [options]
  ```
  *Options:*
  - `--block-size <bytes>`: Block size in bytes (default: 4MB).
  - `--capacity <bytes>`: Maximum capacity of the volume in bytes (default: 1PB).
  - `--s3-endpoint <url>`: S3 compatible object store endpoint URL.
  - `--s3-access-key <key>`: S3 compatible access key.
  - `--s3-secret-key <key>`: S3 compatible secret key.
  - `--s3-bucket <bucket>`: S3 compatible bucket name (automatically initialized/created on format).

* **Mount Squeezefs:**
  ```bash
  squeezefs mount <mountpoint> [options]
  ```
  *Options:*
  - `--disk-cache-paths <paths>`: Comma-separated paths to NVMe cache staging directories.
  - `--local-ips <ips>`: Comma-separated list of local source IP interfaces for multi-rail network load balancing.
  - `--mem-cache-size <size>`: System RAM cache size (e.g. `16GB` or `20%`).
  - `--disk-cache-size <size>`: NVMe cache capacity threshold.
  - `--s3-endpoint <url>`: Overrides S3 endpoint URL stored in Garnet metadata.
  - `--s3-access-key <key>`: Overrides S3 access key stored in Garnet metadata.
  - `--s3-secret-key <key>`: Overrides S3 secret key stored in Garnet metadata.
  - `--s3-bucket <bucket>`: Overrides S3 bucket name stored in Garnet metadata.

* **Show filesystem Status:**
  ```bash
  squeezefs status
  ```

* **Benchmark Mountpoint:**
  ```bash
  squeezefs bench --path <mountpoint> --threads <num> --size <mb>
  ```

* **Instant Metadata Clone (CoW):**
  ```bash
  squeezefs clone <src> <dest>
  ```

* **Tune Kernel Parameters (requires root):**
  ```bash
  squeezefs tune
  ```

---

## Quick Start & Verification

To get up and running quickly with local mock backends in Docker, or deploy directly onto physical bare-metal hardware, see the [QUICKSTART.md](QUICKSTART.md) guide.
