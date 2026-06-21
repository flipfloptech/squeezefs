# Squeezefs Quick Start Guide

This guide describes how to get Squeezefs up and running, execute its built-in micro-benchmarks, and configure it on bare-metal systems—including multi-rail setups using physical Mellanox NICs.

---

## 1. Quick Start via Docker (Sandbox/Testing)

Docker is the easiest way to spin up the metadata service (Microsoft Garnet), object storage (MinIO), and mount the FUSE daemon to execute automated tests.

> [!NOTE]
> Running FUSE inside a container requires FUSE privileges on the host system. You must have `/dev/fuse` accessible and run with elevated capabilities.

### Step 1: Run Integration Tests & Build Sandbox
To compile the client daemon and execute the integration suite in the mock container sandbox:
```bash
docker compose up --build squeezefs-test
```

### Step 2: Interactive Benchmarking inside Docker
To run benchmarks interactively within the Docker environment:
1. Spin up the Garnet and MinIO backend services:
   ```bash
   docker compose up -d garnet rustfs-mock
   ```
2. Run a shell in a container configured with FUSE access:
   ```bash
   docker compose run --rm --entrypoint bash squeezefs-test
   ```
3. Inside the container, mount the filesystem and run the benchmark:
   ```bash
   # Create directories
   mkdir -p /mnt/squeezefs /tmp/squeezefs_staging

   # Start squeezefs in the background
   cargo run --release -- mount /mnt/squeezefs --disk-cache-paths /tmp/squeezefs_staging &

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

### Step 1: Start Backend Services
For performance evaluation, you can run the backend services in Docker on the target hosts, or deploy them directly to your cluster:

* **Microsoft Garnet (Metadata):**
  ```bash
  docker run -d --name squeezefs-garnet --network host ghcr.io/microsoft/garnet:latest --lua
  ```
* **RustFS / S3 Storage (Data Blocks):**
  ```bash
  docker run -d --name squeezefs-s3 \
    -p 9000:9000 -p 9001:9001 \
    -e MINIO_ROOT_USER=admin \
    -e MINIO_ROOT_PASSWORD=password \
    minio/minio server /data --console-address ":9001"
  ```

### Step 2: Build Squeezefs Client
Clone and compile the repository with optimizations:
```bash
cargo build --release
```

### Step 3: Run Mount and Benchmarks
1. Set up connection environment variables:
   ```bash
   export GARNET_URL="redis://127.0.0.1:6379"
   export RUSTFS_ENDPOINT="http://127.0.0.1:9000"
   export RUSTFS_ACCESS_KEY="admin"
   export RUSTFS_SECRET_KEY="password"
   export RUSTFS_BUCKET="squeezefs-data"
   ```
2. Create filesystem mount and local staging directories:
   ```bash
   mkdir -p /mnt/squeezefs
   mkdir -p /tmp/squeezefs_staging
   ```
3. Run the FUSE daemon:
   ```bash
   sudo ./target/release/squeezefs mount /mnt/squeezefs --disk-cache-paths /tmp/squeezefs_staging
   ```
4. Run the benchmark tool in a separate terminal:
   ```bash
   ./target/release/squeezefs bench --path /mnt/squeezefs --threads 8 --size 128
   ```

---

## 3. High-Performance Multi-Rail Configuration (6-Node Mellanox Setup)

When deploying on a multi-node cluster where hosts are equipped with multiple physical NICs (e.g. 2 Mellanox NICs per host), configure Multi-Rail bonding to balance network packets at the application socket layer.

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
                     | (Garnet/RustFS) |
                     +-----------------+
```

### Configure Interface IP Binding
To force Squeezefs to pin outbound traffic to specific physical interfaces (e.g., source IPs `10.10.10.1` and `10.10.20.1`), pass the `--local-ips` flag:
```bash
sudo ./target/release/squeezefs mount /mnt/squeezefs \
  --disk-cache-paths /tmp/squeezefs_staging \
  --local-ips 10.10.10.1,10.10.20.1
```

* **Load Balancing:** All backend S3 calls and metadata locks will automatically cycle round-robin between the two local IPs.
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
