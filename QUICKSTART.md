# Squeezefs Quick Start Guide

This guide describes how to get Squeezefs up and running, execute its built-in micro-benchmarks, and configure it on bare-metal systems—including multi-rail setups using physical Mellanox NICs over NVMe-oF.

---

## 1. Quick Local Sandbox (File-backed Testing)

You can run and test SqueezeFS on any Linux machine using simple pre-allocated loopback files. No raw disk partitions or database servers are required.

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

### Step 1: Build the Squeezefs Client
Clone and compile the repository with release optimizations:
```bash
cargo build --release
```

### Step 2: Prepare Sandbox Backing Files
Create blank files to serve as your metadata and data block devices:
```bash
# Allocate 64MB for Metadata Volume
truncate -s 64M /tmp/squeezefs_meta.bin

# Allocate 1GB for Data Volume
truncate -s 1G /tmp/squeezefs_data.bin
```

### Step 3: Format the Filesystem
Format the backing files using SqueezeFS URIs:
```bash
./target/release/squeezefs format \
  sqmeta:///tmp/squeezefs_meta.bin \
  sqdata:///tmp/squeezefs_data.bin
```

### Step 4: Mount Squeezefs
Create the mount point and staging cache directories:
```bash
sudo mkdir -p /mnt/squeezefs
sudo mkdir -p /tmp/squeezefs_staging

# Mount Squeezefs in the background
sudo ./target/release/squeezefs mount \
  sqmeta:///tmp/squeezefs_meta.bin \
  /mnt/squeezefs \
  --disk-cache-paths /tmp/squeezefs_staging \
  --daemon \
  --log-file /tmp/squeezefs.log \
  --allow-others \
  --uid $(id -u) \
  --gid $(id -g)
```

### Step 5: Run the Benchmark
Run SqueezeFS parallel benchmarks to stress metadata and raw data operations:
```bash
./target/release/squeezefs bench /mnt/squeezefs --threads 4 --large-size 64
```

Committed reference numbers for this bench (large-seq writes ~1.8 GB/s on the reference box via the zero-copy write path) live in `.benchmarks/2026-07-08-zero-copy-write-path-closing.md`; compare your rows against that table when validating a setup.

### Step 6: Unmount Safely
Use SqueezeFS unmount to drain staging writes and cleanly shut down:
```bash
sudo ./target/release/squeezefs umount /mnt/squeezefs
```
*(Or use standard `/bin/umount /mnt/squeezefs`, enabled by `--allow-others` and daemon CWD setsid root isolation).*

---

## 2. Bare-Metal Execution (Real Hardware Setup)

To avoid containerization network bridges or WSL virtualization overheads and measure true hardware capacity, run Squeezefs directly on the host using physical block devices.

### Step 1: Create Storage Pool and Volume
Assume `/dev/nvme0n1` and `/dev/nvme1n1` are dedicated fast NVMe drives:
```bash
# Initialize storage pool
./target/release/squeezefs storage pool create main-pool /dev/nvme0n1 /dev/nvme1n1

# Construct metadata and data block volumes
./target/release/squeezefs storage volume create main-pool meta-vol --size 128G
./target/release/squeezefs storage volume create main-pool data-vol --size 1P
```

### Step 2: Format Volumes Concurrently
Format the logical volumes. Pass `--full` if you want a complete block-aligned zero-wipe of the devices:
```bash
./target/release/squeezefs format \
  sqmeta:///dev/main-pool/meta-vol \
  sqdata:///dev/main-pool/data-vol \
  --full
```

### Step 3: Mount and Run
```bash
sudo ./target/release/squeezefs mount \
  sqmeta:///dev/main-pool/meta-vol \
  /mnt/squeezefs \
  --disk-cache-paths /tmp/squeezefs_staging \
  --daemon \
  --allow-others
```

---

## 3. High-Performance Multi-Rail Configuration (NVMe-oF Mellanox Setup)

When deploying on a multi-node cluster where hosts are equipped with multiple physical NICs (e.g. 2 Mellanox NICs per host), configure Multi-Rail bonding to balance network packets over NVMe-oF at the application socket layer.

### Automatic SPDK Compilation, Setup, and Execution
To compile SPDK from source, set up local hugepages, selectively bind target NVMe SSDs to user-space, and launch the user-space target daemon (`nvmf_tgt` listener) in the background:
```bash
# 1. Compile SPDK from source and install system dependencies to /opt/spdk
sudo ./target/release/squeezefs storage nvmeof spdk-install

# 2. Configure hugepages safely (defaults to 2GB, supports 4GB) without unbinding system disks
sudo ./target/release/squeezefs storage nvmeof spdk-setup --hugepages 2GB

# 3. Selectively bind only a specific secondary NVMe SSD PCIe controller to SPDK
sudo ./target/release/squeezefs storage nvmeof spdk-bind --pci 0000:02:00.0

# 4. Start the background SPDK target daemon (nvmf_tgt)
sudo ./target/release/squeezefs storage nvmeof spdk-start
```

### Share a Target via user-space SPDK
```bash
# Share a backing disk as SPDK NVMe-oF subsystem target
sudo ./target/release/squeezefs storage nvmeof share /dev/nvme0n1 --spdk --port 4420 --ip 10.10.10.50
```

### Connect to Remote NVMe-oF Storage
To connect to an NVMe over Fabrics target device before mounting:
```bash
# Connect to the remote storage cluster
sudo ./target/release/squeezefs storage nvmeof connect --ip 10.10.10.50 --subnqn nqn.2026-06.org.squeezefs:data

# Create pool and volume spanning the fabric-attached block device
sudo ./target/release/squeezefs storage pool create fabric-pool /dev/nvme1n1
sudo ./target/release/squeezefs storage volume create fabric-pool my-fabric-vol --size 1P

# Format and mount the fabric-attached volume
sudo ./target/release/squeezefs format sqmeta:///dev/main-pool/meta-vol sqdata:///dev/fabric-pool/my-fabric-vol
sudo ./target/release/squeezefs mount sqmeta:///dev/main-pool/meta-vol /mnt/squeezefs --local-ips 10.10.10.1,10.10.20.1
```
- **Load Balancing:** All IO operations will cycle and balance round-robin between the two local IPs traversing the fabric.
- **Failover HA:** If a Mellanox NIC link drops, Squeezefs catches the error and instantly retries the operation on the remaining healthy NIC.

---

## 4. Kernel Tuning for Bare Metal (Auto-Tune)

For maximum HPC file throughput, Squeezefs includes an auto-tuning command. This script adjusts FUSE congestion thresholds, virtual memory dirty page ratios, and network socket maximum buffer sizes.

Run the built-in tune command:
```bash
sudo ./target/release/squeezefs tune
```
- **`vm.dirty_ratio = 40`** & **`vm.dirty_background_ratio = 10`**: Aggressively buffers writes in memory before flushing.
- **`net.core.rmem_max`** & **`net.core.wmem_max` to `67108864` (64MB)**: Expands TCP socket buffers for massive parallel streams.
- **FUSE Connection Limits**: Increases `max_background` to `64` and `congestion_threshold` to `48` to prevent FUSE queue starvation.

---

## 5. Metadata Durability Knobs

The metadata crash contract (levels **D0/D1/D2**) is documented in `README.md` → *Metadata Durability* and `docs/design-wal-crash-consistency.md` §3. Operationally:

```bash
# Refuse to mount on storage that cannot promise 4 KiB atomic sector writes
# (classification below `atomic4k` fails loud; check `.stats` → meta_volume_atomicity):
sudo ./target/release/squeezefs mount sqmeta:///dev/main-pool/meta-vol /mnt/squeezefs --strict-meta-atomicity

# Strict sync-on-commit metadata durability (default is a 50 ms deferred window):
SQUEEZEFS_META_FLUSH_INTERVAL_MS=0 sudo -E ./target/release/squeezefs mount …

# Inode-reclaim group-commit batch size (default 64):
SQUEEZEFS_RECLAIM_BATCH=128 sudo -E ./target/release/squeezefs mount …
```

File-backed sandbox volumes (section 1) classify as `file-backed` — fine for development, but they carry the documented D2 torn-sector exposure on power loss. Use 4 KiB-LBA NVMe (or kernels ≥ 6.11 with an atomic-write unit ≥ 4 KiB) for the `atomic4k` classification in production.
