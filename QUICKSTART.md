# Squeezefs Quick Start Guide

This guide describes how to get Squeezefs up and running, execute its built-in micro-benchmarks, and configure it on bare-metal systems—including multi-rail setups using physical Mellanox NICs over NVMe-oF.

---

## 1. Bare-Metal Execution (Real Hardware Setup)

To avoid containerization network bridges or WSL virtualization overheads and measure true hardware capacity, run Squeezefs directly on the host system.

### Prerequisites (Ubuntu/Debian)
Ensure compilation tools, clang, and the FUSE 3 user-space library are installed:
```bash
sudo apt update && sudo apt install -y \
    build-essential \
    pkg-config \
    libfuse3-dev \
    fuse3 \
    clang \
    libclang-dev
```

### Step 1: Start Metadata Service
For performance evaluation, you can run Garnet on the target hosts, or deploy it directly to your cluster:

* **Option A: Custom SqueezeFS-Garnet Container (Recommended for Max Performance):**
  Builds the custom C# transaction extensions automatically:
  ```bash
  podman build -t squeezefs-garnet -f docker/Dockerfile.garnet docker/
  podman run -d --rm --replace --name squeezefs-garnet -p 6379:6379 squeezefs-garnet
  ```

* **Option B: Standard Microsoft Garnet Container (Fallback):**
  ```bash
  podman run -d --rm --replace --name squeezefs-garnet -p 6379:6379 ghcr.io/microsoft/garnet:latest
  ```
  *(Note: Standard Garnet does not support C# extensions out-of-the-box. SqueezeFS will automatically fall back to standard pipelined metadata deletion).*

### Step 2: Build Squeezefs Client
Clone and compile the repository with optimizations:
```bash
cargo build --release
```

### Step 3: Format and Mount Squeezefs

1. **Create a storage pool and volume for Squeezefs**:
   ```bash
   # Assuming /dev/nvme0n1 is a local fast NVMe drive dedicated to Squeezefs
   ./target/release/squeezefs storage pool create main-pool /dev/nvme0n1
   ./target/release/squeezefs storage volume create main-pool my-vol --size 1P
   ```

2. **Format the filesystem volume** (this sets up block allocation maps for your NVMe device in Garnet using SqueezeFS URI):
   ```bash
   ./target/release/squeezefs format squeeze://127.0.0.1:6379/squeezefs-volume \
     --nvme-target-path /dev/main-pool/my-vol
   ```

3. **Create the mount point and local staging directories**:
   ```bash
   mkdir -p /mnt/squeezefs
   mkdir -p /tmp/squeezefs_staging
   ```

4. **Mount the FUSE daemon** (run in background with daemon mode, specify log file destination, and pass your volume path):
   ```bash
   ./target/release/squeezefs mount squeeze://127.0.0.1:6379/squeezefs-volume /mnt/squeezefs \
     --disk-cache-paths /tmp/squeezefs_staging \
     --nvme-path /dev/main-pool/my-vol \
     --daemon \
     --log-file /tmp/squeezefs.log \
     --uid 1000 \
     --gid 1000
   ```

5. **(Optional) Configure quotas at runtime** (Note: connection and volume settings are auto-resolved from the active FUSE mount!):
   To dynamically adjust size or inode quotas:
   ```bash
   # Change filesystem capacity quota at runtime
   ./target/release/squeezefs config set capacity 10T
   ```

6. **Run the benchmark tool** as the mounting user (Note: running with 'sudo' will be blocked by FUSE unless mounted with 'allow_other'):
   ```bash
   ./target/release/squeezefs bench /mnt/squeezefs --threads 8 --large-size 128
   ```

---

## 2. High-Performance Multi-Rail Configuration (NVMe-oF & 6-Node Mellanox Setup)

When deploying on a multi-node cluster where hosts are equipped with multiple physical NICs (e.g. 2 Mellanox NICs per host), configure Multi-Rail bonding to balance network packets over NVMe-oF at the application socket layer.

```
       +---------------------------------------------+
       |             Squeezefs Client Node           |
       |  (Binds sockets to local interface IPs)     |
       +----------+-----------------------+----------+
                  |                       |
        Interface 1 (10.10.10.1)  Interface 2 (10.10.20.1)
                  |                       |
       +----------+-----------+ +---------+----------+
       | Mellanox Fab-A (10G) | | Mellanox Fab-B (10G) |
       +----------+-----------+ +---------+----------+
                  |                       |
                  +-----------+-----------+
                              |
                     +--------+--------+
                     |  Storage Rack   |
                     |  (NVMe Target)  |
                     +-----------------+
```

### Automatic SPDK Compilation, Setup, and Execution
To compile SPDK from source, set up local hugepages, selectively bind target NVMe SSDs to user-space, and launch the user-space target daemon (`nvmf_tgt` listener) in the background:
```bash
# 1. Compile SPDK from source and install system dependencies to /opt/spdk
sudo ./target/release/squeezefs nvmeof spdk-install

# 2. Configure hugepages safely (defaults to 2GB, supports 4GB) without unbinding system disks
sudo ./target/release/squeezefs nvmeof spdk-setup --hugepages 2GB

# 3. Selectively bind only a specific secondary NVMe SSD PCIe controller to SPDK
sudo ./target/release/squeezefs nvmeof spdk-bind --pci 0000:02:00.0

# 4. Start the background SPDK target daemon (nvmf_tgt)
sudo ./target/release/squeezefs nvmeof spdk-start
```
This maps only the selected data NVMe drives to SPDK polled user-space drivers while keeping your system OS disk safe under kernel control.

### Share a Target via user-space SPDK
To share a local backing file or NVMe block device using the high-performance user-space SPDK target (`nvmf_tgt` listener):
```bash
# Share a backing disk as SPDK NVMe-oF subsystem target
sudo ./target/release/squeezefs nvmeof share /dev/nvme0n1 --spdk --port 4420 --ip 10.10.10.50
```
This sends JSON-RPC requests directly to the SPDK daemon listening at `/var/tmp/spdk.sock` to construct bdevs, subsystems, namespaces, and bind TCP listeners, achieving bare-metal polling throughput.

### Connect to Remote NVMe-oF Storage
To connect to an NVMe over Fabrics target device (fully compatible with standard Linux targets and user-space SPDK targets) before mounting:
```bash
# Connect to the remote storage cluster
sudo ./target/release/squeezefs nvmeof connect --ip 10.10.10.50 --subnqn nqn.2026-06.org.squeezefs:data

# Create pool and volume spanning the fabric-attached block device
sudo ./target/release/squeezefs storage pool create fabric-pool /dev/nvme1n1
sudo ./target/release/squeezefs storage volume create fabric-pool my-fabric-vol --size 1P

# Format and mount the fabric-attached volume
sudo ./target/release/squeezefs format default --nvme-target-path /dev/fabric-pool/my-fabric-vol
sudo ./target/release/squeezefs mount /mnt/squeezefs --nvme-path /dev/fabric-pool/my-fabric-vol --local-ips 10.10.10.1,10.10.20.1
```

* **Load Balancing:** All IO operations will cycle and balance round-robin between the two local IPs traversing the fabric.
* **Failover HA:** If a Mellanox NIC link drops or returns errors, Squeezefs catches the error and instantly retries the operation on the remaining healthy NIC.

---

## 3. Kernel Tuning for Bare Metal (Auto-Tune)

For maximum HPC file throughput, Squeezefs includes an auto-tuning command. This script adjusts FUSE congestion thresholds, virtual memory dirty page ratios, and network socket maximum buffer sizes to matches the requirements of high-speed fabrics.

Run the built-in tune command:
```bash
sudo ./target/release/squeezefs tune
```

This applies the following optimizations:
- **`vm.dirty_ratio = 40`** & **`vm.dirty_background_ratio = 10`**: Aggressively buffers writes in memory before flushing.
- **`net.core.rmem_max`** & **`net.core.wmem_max` to `67108864` (64MB)**: Expands TCP socket buffers for massive parallel streams.
- **FUSE Connection Limits**: Increases `max_background` to `64` and `congestion_threshold` to `48` to prevent FUSE queue starvation.

---

## 4. Developer Micro-Benchmarks & Disk-less Mode

### Running in Disk-less (Memory-only) Staging Mode
If you do not have a dedicated local NVMe staging drive or want to evaluate SqueezeFS core logic bypassing all physical drive write amplification, you can configure staging to run entirely in system RAM:
```bash
./target/release/squeezefs mount squeeze://127.0.0.1:6379/squeezefs-volume /mnt/squeezefs \
  --disk-cache-paths memory \
  --nvme-path /dev/main-pool/my-vol \
  --daemon
```
* **Memory Staging:** Specifying `memory` (or `none`) directs SqueezeFS to spin up virtual `MmapMut::map_anon` segments in RAM, completely bypassing local disk operations.

### Running the High-Concurrency Micro-Benchmark Suite
SqueezeFS includes a Criterion-based micro-benchmark suite to stress-test locks, writes, reads, and memory allocations under high parallel task loads:
```bash
# Run the concurrent micro-benchmarks
cargo bench --bench high_concurrency_bench
```
This suite profiles:
- **`concurrent_writes_16_tasks`**: Measures concurrent chunk writes to distinct files.
- **`concurrent_reads_16_tasks_same_file`**: Measures concurrent reads from a shared file.
- **`concurrent_locks_16_tasks`**: Measures DLM lock acquisition/release contention.
- **`concurrent_pool_alloc_16_tasks`**: Measures allocation/deallocation concurrency in the unified buffer pool.

---

## 5. Deploying Garnet C# Custom Command Extensions

To simplify deployment and avoid forcing administrators to manually compile C# projects or manage DLL paths, SqueezeFS automates the C# transaction deployment.

### 1. Build and Run the Custom Image
The multi-stage `docker/Dockerfile.garnet` extracts the exact assembly binaries from the base Garnet image, references them in C#, builds the project under .NET 10 SDK, and places the compiled `SqueezeExtensions.dll` inside the `/app/extensions` folder. It starts the server with `--enable-module-command yes` and `--extension-allow-unsigned`:
```bash
podman build -t squeezefs-garnet -f docker/Dockerfile.garnet docker/
podman run -d --rm --replace --name squeezefs-garnet -p 6379:6379 squeezefs-garnet
```

### 2. Automatic Registration
Upon the first file deletion request, the SqueezeFS client daemon automatically sends a `REGISTERCS` command to the Garnet server:
```text
REGISTERCS TXN SqueezeUnlink 6 SqueezeUnlink SRC /app/extensions/SqueezeExtensions.dll
```
This registers the transaction on the fly. No manual registration CLI calls or configuration file edits are required from system administrators. If registration fails or standard Garnet is used, the client seamlessly falls back to standard pipelined metadata deletion.
