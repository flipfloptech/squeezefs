# Squeezefs

Squeezefs is a slimmed-down, high-performance distributed filesystem featuring a decoupled metadata store backend (Garnet) and a local or NVMe-oF block device client.

Designed to operate at scale (15,000+ concurrent nodes), it delivers bare-metal file throughput by leveraging asynchronous network and file architectures, client-side caching, and multi-rail network load balancing over NVMe-oF fabrics.

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
  | Microsoft Garnet  |           |     NVMe / NVMe-oF|
  |  (RESP Metadata)  |           |   (Local Block Dev)
  +-------------------+           +-------------------+
```

### 1. Asynchronous POSIX FUSE Daemon
Built in **Rust** using the asynchronous `tokio` runtime and `io_uring` polling over `/dev/fuse` to process OS requests efficiently.

### 2. Progressive Data Layout & I/O Routing
Writes are dynamically routed based on file sizes to optimize storage overhead and network latency:
- **Micro-Files (< 64KB):** Inlined directly in the Microsoft Garnet key-value store alongside metadata.
- **Small Files (64KB - 4MB):** Staged locally on NVMe and asynchronously merged into physical blocks flushed to the main NVMe device.
- **Large Files (> 4MB):** Sliced into 4MB blocks and written directly to the target NVMe block device.

### 3. Distributed Lock Manager (DLM) & Consistency
Translates POSIX FUSE locks to cluster-wide locks in Garnet using `SETNX` commands, protected by heartbeats and monotonic fencing tokens to prevent split-brain write conflicts.

### 4. Tiered Caching & Zero-Copy Paths
- **Tier 1 (GPU Direct Storage - GDS):** Routes RDMA transfers directly from NVMe to VRAM, bypassing the host CPU/RAM.
- **Tier 2 (Unified System RAM):** LRU cache dynamically sizing to 20% of system RAM.
- **Tier 3 (Local NVMe Staging):** Staging directory (`.staging`) for async writes and local caching of read blocks to avoid RTT latency.

### 5. Multi-NIC (Multi-Rail) Network Load Balancing & HA
Binds outbound client connections to multiple configured physical interfaces (source IPs). Distributes traffic round-robin across NICs and automatically fails over on interface drops.

### 6. Built-in HPC Auto-Tuning
Includes built-in host auto-tuning (`squeezefs tune`) to optimize virtual memory dirty page ratios, TCP socket buffers, and FUSE connection thresholds.

---

## Subcommands & CLI Usage

Squeezefs exposes a clean CLI to manage formats, mounts, status, performance benchmarks, and optimize systems:

* **Format Squeezefs Volume:**
  ```bash
  squeezefs format <name> [options]
  ```
  *Options:*
  - `--block-size <bytes>`: Block size in bytes (default: 4MB).
  - `--capacity <bytes>`: Maximum capacity of the volume in bytes (default: 1PB).
  - `--mem-cache-size <size>`: Memory cache limit (default: 20%).
  - `--disk-cache-size <size>`: Staging disk cache capacity (default: 50G).
  - `--nvme-target-path <path>`: Required NVMe device target path for format (e.g. /dev/nvme0n1).

* **Mount Squeezefs:**
  ```bash
  squeezefs mount <mountpoint> [options]
  ```
  *Options:*
  - `--disk-cache-paths <paths>`: Comma-separated paths to NVMe cache staging directories.
  - `--local-ips <ips>`: Comma-separated list of local source IP interfaces for multi-rail network load balancing.
  - `--mem-cache-size <size>`: System RAM cache size (e.g. `16GB` or `20%`).
  - `--disk-cache-size <size>`: NVMe cache capacity threshold.
  - `--daemon`: Run FUSE daemon in the background (detach from terminal).
  - `--uid <id>`: Custom UID owner for the mount (default: current user or SUDO_UID).
  - `--gid <id>`: Custom GID owner for the mount (default: current group or SUDO_GID).
  - `--log-file <path>`: Path to write daemon logs to when running in background.
  - `--nvme-path <path>`: Local NVMe path or NVMe-oF connected target path (required).

* **Show filesystem Status:**
  ```bash
  squeezefs status
  ```

* **Defragment Squeezefs Volume:**
  ```bash
  squeezefs defrag --name <name> --nvme-path <path>
  ```
  Calculates block fragmentation on the NVMe device and performs in-place reallocation to compact blocks and fill holes.

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

* **NVMe-oF Utilities:**
  Share and manage NVMe-oF targets via `squeezefs nvmeof`.
  ```bash
  squeezefs nvmeof share <path>
  squeezefs nvmeof connect --ip <ip> --subnqn <nqn>
  squeezefs nvmeof disconnect <nqn>
  squeezefs nvmeof list
  ```

* **Runtime Configuration Management:**
  Configure limits and caches at runtime:
  ```bash
  squeezefs config <garnet_url> <fs_name> <action>
  ```
  *Actions:*
  - `set <key> <value>`: Updates runtime format quotas and cache limits. Supported keys are `capacity` (e.g. "100G", "2T"), `inodes` (e.g. "2000000"), `mem_cache_size`, `read_mem_cache_size`, `write_mem_cache_size`, `disk_cache_size`, `read_cache_size`, and `write_cache_size`.
  - `diskcache <subcommand>` (alias: `diskcaches`): Manages staging disk cache paths.
    * `add <path>`
    * `remove <path> [--force]`
    * `enable <path>`
    * `disable <path>`
    * `list`
  - `list`: Lists the entire unified configuration.
  - `fsck`: Runs consistency checks on metadata and block references.

---

## Quick Start & Verification

To get up and running quickly or deploy directly onto physical bare-metal hardware over NVMe-oF, see the [QUICKSTART.md](QUICKSTART.md) guide.
