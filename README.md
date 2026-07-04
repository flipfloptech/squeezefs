# Squeezefs

![SqueezeFS Header](github.jpeg)

Squeezefs is a slimmed-down, high-performance distributed POSIX FUSE filesystem (Rust + tokio + io_uring) featuring a decoupled, block-based logical volume metadata store backend (**MetaLV**) and a local or NVMe-oF block device client. Linux-only.

Designed to operate at scale (15,000+ concurrent nodes), it delivers bare-metal file throughput by leveraging asynchronous network and file architectures, client-side caching, and multi-rail network load balancing over NVMe-oF fabrics, with zero external database dependencies.

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
   |  Metadata Volume  |           |     NVMe / NVMe-oF|
   | (MetaLV Superblock|           |   (Local Block Dev)
   |    Inode Tables)  |           |   (Striped Blocks)
   +-------------------+           +-------------------+
```

### 1. Asynchronous POSIX FUSE Daemon
Built in **Rust** using the asynchronous `tokio` runtime and `io_uring` polling over `/dev/fuse` to process OS requests efficiently. Standard mounts automatically switch to high-performance FUSE-over-io_uring after the INIT handshake.

### 2. Progressive Data Layout & I/O Routing
Writes are dynamically routed based on file sizes to optimize storage overhead and network latency:
- **Inline Files (< 4KB):** Inlined directly in the Metadata Volume's inodes/attributes.
- **Staged Files (4KB - 4MB):** Staged locally on NVMe cache and asynchronously merged into physical blocks flushed to the main NVMe device.
- **Striped Files (> 4MB):** Sliced into 4MB blocks and written directly to the target NVMe block devices.

### 3. Distributed Lock Manager (DLM) & Consistency
Translates POSIX FUSE locks to cluster-wide leases on the metadata backend, protected by heartbeat limits and monotonic fencing tokens to prevent split-brain write conflicts.

### 4. Tiered Caching & Zero-Copy Paths
- **Tier 1 (GPU Direct Storage - GDS):** Routes RDMA transfers directly from NVMe to VRAM, bypassing the host CPU/RAM.
- **Tier 2 (Unified System RAM):** Clock/LRU caches dynamically sizing to system memory limits.
- **Tier 3 (Local NVMe Staging):** Staging directory (`.staging`) for async writes and local caching of read blocks to avoid RTT latency.

### 5. Multi-NIC (Multi-Rail) Network Load Balancing & HA
Binds outbound client connections to multiple configured physical interfaces (source IPs). Distributes traffic round-robin across NICs and automatically fails over on interface drops. Fully compatible with user-space storage engines like SPDK (Storage Performance Development Kit).

### 6. Built-in HPC Auto-Tuning
Includes built-in host auto-tuning (`squeezefs tune`) to optimize virtual memory dirty page ratios, TCP socket buffers, and FUSE connection thresholds.

---

## Subcommands & CLI Usage

Squeezefs exposes a clean CLI to manage formats, mounts, status, performance benchmarks, and optimize systems.

### SqueezeFS URI Scheme
To centralize block storage connectivity, SqueezeFS utilizes two connection URIs:
* **Metadata Volumes**: `sqmeta://<path_to_block_device_or_file>` (e.g. `sqmeta://dev/xai-meta/mds01`).
* **Data Volumes**: `sqdata://<path_to_block_device_or_file>` (e.g. `sqdata://dev/xai-data/oss01`).

---

* **Format Squeezefs Volume:**
  Initialize physical block maps and metadata tables. Executes concurrently across all target devices.
  ```bash
  squeezefs format sqmeta://<meta_dev> [sqmeta://...] sqdata://<data_dev> [sqdata://...] [options]
  ```
  *Options:*
  - `--block-size <bytes>`: Block size in bytes (e.g. `4M`, `1M`, default: `4M`).
  - `--capacity <bytes>`: Maximum capacity of the volume (default: auto-detected or 1PB).
  - `--inodes <count>`: Hard quota limit for number of inodes (default: `1000000`).
  - `--full`: Performs full block-aligned zero-wiping of the backing device capacity with a progress bar (default is quick-format).

* **Mount Squeezefs:**
  ```bash
  squeezefs mount sqmeta://<meta_dev> [sqmeta://...] <mountpoint> [options]
  ```
  *Options:*
  - `--disk-cache-paths <paths>`: Comma-separated paths to NVMe cache staging directories.
  - `--local-ips <ips>`: Comma-separated list of local source IP interfaces for multi-rail load balancing.
  - `--mem-cache-size <size>`: System RAM cache size (e.g. `16GB` or `20%`).
  - `--daemon`: Run FUSE daemon in the background (changes its working directory to `/` to avoid locking paths).
  - `--allow-others` (or `--allow-other`): Allow other users/root to access the mount (required for `sudo umount`).
  - `--log-file <path>`: Path to write daemon logs to when running in background.

* **Show filesystem Status:**
  Prints a detailed formatted configuration and volume health status summary:
  ```bash
  squeezefs status sqmeta://<meta_dev>
  ```

* **Unmount Squeezefs:**
  Safely unmounts SqueezeFS by waiting for staging caches to flush before tearing down FUSE.
  ```bash
  squeezefs umount <mountpoint> [--force]
  ```

* **Defragment Squeezefs Volume:**
  ```bash
  squeezefs defrag --nvme-path <path>
  ```

* **Benchmark Mountpoint:**
  Loads layout details from the FUSE mount and performs parallel read/write benchmarks, auditing metrics against the local `.stats` file.
  ```bash
  squeezefs bench <mountpoint> [options]
  ```
  *Options:*
  - `--threads <num>`: Parallel workload threads (default: 4).
  - `--iterations <num>`: Run loop count for benchmark runs.
  - `--large-size <MB>`: Large file workload size.
  - `--small-size <KB>`: Small file workload size.
  - `--small-count <count>`: Small file writes count.
  - `--direct`: Enable Direct I/O (O_DIRECT) path validation.

* **Instant Metadata Clone (CoW):**
  ```bash
  squeezefs clone <src> <dest>
  ```

* **Tune Kernel Parameters (requires root):**
  ```bash
  squeezefs tune
  ```

* **NVMe-oF Utilities:**
  Share and dismantle NVMe-oF targets, and install/configure user-space SPDK via `squeezefs storage nvmeof`.
  ```bash
  squeezefs storage nvmeof share <path> [--spdk] [--port <port>] [--ip <ip>]
  squeezefs storage nvmeof connect --ip <ip> --subnqn <nqn> [--port <port>]
  squeezefs storage nvmeof disconnect <nqn>
  squeezefs storage nvmeof unshare <nqn> [--spdk]
  squeezefs storage nvmeof list
  ```

---

## Quick Start & Verification

To get up and running quickly or deploy directly onto physical bare-metal hardware over NVMe-oF, see the [QUICKSTART.md](QUICKSTART.md) guide.
