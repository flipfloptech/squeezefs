# Squeezefs

![SqueezeFS Header](github.jpeg)

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
Binds outbound client connections to multiple configured physical interfaces (source IPs). Distributes traffic round-robin across NICs and automatically fails over on interface drops. Fully compatible with all standard-compliant NVMe-oF targets, including user-space storage engines like SPDK (Storage Performance Development Kit).

### 6. Built-in HPC Auto-Tuning
Includes built-in host auto-tuning (`squeezefs tune`) to optimize virtual memory dirty page ratios, TCP socket buffers, and FUSE connection thresholds.

---

## Subcommands & CLI Usage

Squeezefs exposes a clean CLI to manage formats, mounts, status, performance benchmarks, and optimize systems. 

### SqueezeFS URI Scheme
To centralize connections, SqueezeFS utilizes a single connection URI:
`squeeze://<ip>:<port>/<fs_name>` (e.g. `squeeze://127.0.0.1:6379/myvol`).
* **Mount & Format**: Require this URI as a primary positional parameter.
* **Other Subcommands**: Can dynamically resolve connection details from FUSE mount `.config` metadata files, system mount tables, environment variables (`GARNET_URL` / `SQUEEZE_URI`), or parent paths, making the URI completely optional.

---

* **Format Squeezefs Volume:**
  ```bash
  squeezefs format squeeze://<ip>:<port>/<fs_name> [options]
  ```
  *Options:*
  - `--block-size <bytes>`: Block size in bytes (default: 4MB).
  - `--capacity <bytes>`: Maximum capacity of the volume in bytes (default: 1PB).
  - `--mem-cache-size <size>`: Memory cache limit (default: 20%).
  - `--disk-cache-size <size>`: Staging disk cache capacity (default: 50G).
  - `--nvme-target-path <path>`: Required NVMe device target path for format (e.g. /dev/nvme0n1).

* **Mount Squeezefs:**
  ```bash
  squeezefs mount squeeze://<ip>:<port>/<fs_name> <mountpoint> [options]
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
  - `--nvme-path <path>`: Local NVMe path or NVMe-oF connected target path.
  - `--job-cpu-limit <percentage>`: Cap background job worker CPU utilization percentage (1 to 100, default: 50).
  - `--write-verification`: Enable read-after-write checksum verification on all writes to cache and disk.

* **Show filesystem Status:**
  ```bash
  squeezefs status squeeze://<ip>:<port>/<fs_name>
  ```

* **Defragment Squeezefs Volume (Cluster-Distributed Job):**
  ```bash
  squeezefs defrag --nvme-path <path> [--squeeze-uri <uri>]
  ```
  Calculates block fragmentation on the NVMe device, generates block migration tasks, and submits them as a cluster-distributed job. All active FUSE client mount nodes poll and execute these block moves in parallel (subject to their configured `--job-cpu-limit`).

* **Benchmark Mountpoint:**
  ```bash
  squeezefs bench <mountpoint> [options]
  ```
  *Options:*
  - `--threads <num>`: Parallel workload threads (default: 4).
  - `--iterations <num>`: Run loop count for benchmark runs.
  - `--large-size <MB>`: Large file workload size.
  - `--small-size <KB>`: Small file workload size.
  - `--small-count <count>`: Small file writes count.
  - `--only <filters>`: Comma-separated list of workloads to run (e.g. `large-seq,small-rand`).
  - `--skip <filters>`: Skip specific workloads.
  - `--direct`: Enable Direct I/O (O_DIRECT) path validation.

* **Instant Metadata Clone (CoW):**
  ```bash
  squeezefs clone <src> <dest> [--squeeze-uri <uri>]
  ```

* **Tune Kernel Parameters (requires root):**
  ```bash
  squeezefs tune
  ```

* **NVMe-oF Utilities:**
  Share and dismantle NVMe-oF targets, and install/configure user-space SPDK via `squeezefs nvmeof`.
  ```bash
  squeezefs nvmeof share <path> [--spdk] [--port <port>] [--ip <ip>]
  squeezefs nvmeof connect --ip <ip> --subnqn <nqn> [--port <port>]
  squeezefs nvmeof disconnect <nqn>
  squeezefs nvmeof unshare <nqn> [--spdk]
  squeezefs nvmeof list
  squeezefs nvmeof spdk-install
  squeezefs nvmeof spdk-setup [--hugepages <2GB/4GB>]
  squeezefs nvmeof spdk-bind --pci <pci_addr>
  squeezefs nvmeof unbind --pci <pci_addr>
  squeezefs nvmeof spdk-start
  ```

* **Storage Pool & Volume Management:**
  Abstracts underlying LVM operations for seamless scale-out and multi-tenancy.
  ```bash
  squeezefs storage pool create <pool> <disks...>
  squeezefs storage pool add <pool> <disks...>
  squeezefs storage pool remove <pool> <disks...>
  squeezefs storage volume create <pool> <volume> --size <size>
  squeezefs storage volume extend <pool> <volume> --add-size <size>
  squeezefs storage volume delete <pool> <volume>
  ```

* **Runtime Configuration Management:**
  Configure limits and caches at runtime:
  ```bash
  squeezefs config <action> [--squeeze-uri <uri>]
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
