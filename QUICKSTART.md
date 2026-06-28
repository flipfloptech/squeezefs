# Squeezefs Quick Start Guide

This guide describes how to get Squeezefs up and running, execute its built-in micro-benchmarks, and configure it on bare-metal systems—including multi-rail setups using physical Mellanox NICs over NVMe-oF.

---

## 1. Quick Start via Docker (Sandbox/Testing)

Docker is the easiest way to spin up the metadata service (Microsoft Garnet) and mount the FUSE daemon to execute automated tests.

> [!NOTE]
> Running FUSE inside a container requires FUSE privileges on the host system. You must have `/dev/fuse` accessible and run with elevated capabilities.

### Step 1: Run Integration Tests & Build Sandbox
To compile the client daemon and execute the integration suite in the mock container sandbox:
```bash
docker compose up --build squeezefs-test
```

### Step 2: Interactive Benchmarking inside Docker
To run benchmarks interactively within the Docker environment:
1. Spin up the Garnet metadata backend service:
   ```bash
   docker compose up -d garnet
   ```
2. Run a shell in a container configured with FUSE access:
   ```bash
   docker compose run --rm --entrypoint bash squeezefs-test
   ```
3. Inside the container, set up a loopback device and format/mount the filesystem:
   ```bash
   # Create directories
   mkdir -p /mnt/squeezefs /tmp/squeezefs_staging    # Create a mock NVMe loopback file for testing
    truncate -s 10G /tmp/mock_nvme.img

    # Format the volume (uses the squeezefs URI: squeeze://host:port/fs_name)
    cargo run --release -- format squeeze://127.0.0.1:6379/default --nvme-target-path /tmp/mock_nvme.img

    # Start squeezefs in the background
    cargo run --release -- mount squeeze://127.0.0.1:6379/default /mnt/squeezefs --disk-cache-paths /tmp/squeezefs_staging --nvme-path /tmp/mock_nvme.img &

    # Wait a moment for mount to initialize, then run benchmark (auto-resolves config from mount path)
    cargo run --release -- bench /mnt/squeezefs --threads 4 --large-size 64
   ```

---

## 2. Bare-Metal Execution (Real Hardware Setup)

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
For performance evaluation, you can run Garnet in Docker on the target hosts, or deploy it directly to your cluster:

* **Microsoft Garnet (Metadata):**
  ```bash
  docker run -d --name squeezefs-garnet --network host ghcr.io/microsoft/garnet:latest --lua
  ```

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

## 3. High-Performance Multi-Rail Configuration (NVMe-oF & 6-Node Mellanox Setup)

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

### Connect to Remote NVMe-oF Storage
To connect to an NVMe over Fabrics target device before mounting:
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

## 4. Kernel Tuning for Bare Metal (Auto-Tune)

For maximum HPC file throughput, Squeezefs includes an auto-tuning command. This script adjusts FUSE congestion thresholds, virtual memory dirty page ratios, and network socket maximum buffer sizes to matches the requirements of high-speed fabrics.

Run the built-in tune command:
```bash
sudo ./target/release/squeezefs tune
```

This applies the following optimizations:
- **`vm.dirty_ratio = 40`** & **`vm.dirty_background_ratio = 10`**: Aggressively buffers writes in memory before flushing.
- **`net.core.rmem_max`** & **`net.core.wmem_max` to `67108864` (64MB)**: Expands TCP socket buffers for massive parallel streams.
- **FUSE Connection Limits**: Increases `max_background` to `64` and `congestion_threshold` to `48` to prevent FUSE queue starvation.
