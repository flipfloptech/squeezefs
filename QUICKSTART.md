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
   mkdir -p /mnt/squeezefs /tmp/squeezefs_staging

   # Create a mock NVMe loopback file for testing
   truncate -s 10G /tmp/mock_nvme.img

   # Format the volume
   cargo run --release -- format default --nvme-target-path /tmp/mock_nvme.img

   # Start squeezefs in the background
   cargo run --release -- mount /mnt/squeezefs --disk-cache-paths /tmp/squeezefs_staging --nvme-path /tmp/mock_nvme.img &

   # Wait a moment for mount to initialize, then run benchmark
   cargo run --release -- bench --path /mnt/squeezefs --threads 4 --size 64
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

1. **Set up connection environment variables** (so the CLI/daemon knows where Microsoft Garnet is running):
   ```bash
   export GARNET_URL="redis://127.0.0.1:6379"
   ```

2. **Format the filesystem volume** (this sets up block allocation maps for your NVMe device in Garnet):
   ```bash
   # Assuming /dev/nvme0n1 is a local fast NVMe drive dedicated to Squeezefs
   ./target/release/squeezefs format squeezefs-volume \
     --nvme-target-path /dev/nvme0n1
   ```

3. **Create the mount point and local staging directories**:
   ```bash
   mkdir -p /mnt/squeezefs
   mkdir -p /tmp/squeezefs_staging
   ```

4. **Mount the FUSE daemon** (run in background with daemon mode, specify log file destination, and pass your NVMe path):
   ```bash
   sudo ./target/release/squeezefs mount /mnt/squeezefs \
     --disk-cache-paths /tmp/squeezefs_staging \
     --nvme-path /dev/nvme0n1 \
     --daemon \
     --log-file /tmp/squeezefs.log \
     --uid 1000 \
     --gid 1000
   ```

5. **(Optional) Configure quotas at runtime**:
   To dynamically adjust size or inode quotas:
   ```bash
   # Change filesystem capacity quota at runtime
   ./target/release/squeezefs config "redis://127.0.0.1:6379" squeezefs-volume set capacity 10T
   ```

6. **Run the benchmark tool** in a separate terminal:
   ```bash
   ./target/release/squeezefs bench --path /mnt/squeezefs --threads 8 --size 128
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
# Format and mount the fabric-attached block device
sudo ./target/release/squeezefs format default --nvme-target-path /dev/nvme1n1
sudo ./target/release/squeezefs mount /mnt/squeezefs --nvme-path /dev/nvme1n1 --local-ips 10.10.10.1,10.10.20.1
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
