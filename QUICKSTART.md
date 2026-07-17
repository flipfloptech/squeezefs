# Squeezefs Quick Start Guide

This guide describes how to get Squeezefs up and running, verify a mount, execute its built-in benchmarks, and configure it on bare-metal systems and over NVMe-oF fabrics. It is the operator runbook; concepts, guarantee classes, and tuning-knob reference live in [README.md](README.md).

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
The whole sandbox runs **unprivileged** — FUSE mounts need no root, and keeping everything under your own `$HOME` avoids the modern-kernel `fs.protected_regular` trap (root cannot open another user's files in sticky `/tmp`, so a `sudo mount` over user-created `/tmp` volumes fails with `Permission denied`).

Create sparse files to serve as your metadata and data block devices (sparse — they only consume disk as blocks are written):
```bash
mkdir -p ~/squeezefs-sandbox/staging ~/squeezefs-sandbox/mnt

# Allocate 256MB for Metadata Volume
truncate -s 256M ~/squeezefs-sandbox/meta.bin

# Allocate 8GB for Data Volume
truncate -s 8G ~/squeezefs-sandbox/data.bin
```
> File-backed volumes are fine for a functional sandbox, but **do not benchmark barrier-bound metadata work on them** (especially on btrfs/CoW hosts — the measured distortion is 165× on the journal barrier). For anything measured, build the virtual NVMe substrate in [section 2](#2-dev-box-virtual-nvme-substrate-ram-backed-nvme-of-loop) instead.

### Step 3: Format the Filesystem
Format the backing files using SqueezeFS URIs (cache/staging paths are **declared at format** and recorded in the format config — omit `--disk-cache-paths` for a permanently cache-less filesystem):
```bash
./target/release/squeezefs format \
  sqmeta://$HOME/squeezefs-sandbox/meta.bin \
  sqdata://$HOME/squeezefs-sandbox/data.bin \
  --disk-cache-paths ~/squeezefs-sandbox/staging
```
> Metadata volumes format as **v3** (CoW KV metadata) — the only supported metadata format (legacy v2 volumes refuse to mount: reformat required). Optional format knobs (`--meta-node-kib`, `--meta-journal-mb`) and the v3 durability contract are covered in section 6.

### Step 4: Mount Squeezefs
The mount reads its cache/staging paths from the format config (passing `--disk-cache-paths` at mount is refused — change paths with `squeezefs config set-cache-paths`):
```bash
./target/release/squeezefs mount \
  sqmeta://$HOME/squeezefs-sandbox/meta.bin \
  ~/squeezefs-sandbox/mnt \
  --daemon \
  --log-file ~/squeezefs-sandbox/mount.log
```
The mount log must contain `FUSE-over-io_uring transport armed for this session` — the high-performance transport is required, not optional (the mount fails loudly if the kernel cannot arm it; the mount auto-enables the kernel's `fuse.enable_uring` when it can). Add `--allow-other` (root or `user_allow_other` in `/etc/fuse.conf`) if other users — including root — must access the mount, and `--uid`/`--gid` to change the presented file ownership.

### Step 5: Verify the Mount
Exercise the filesystem, then read the live daemon metrics from the virtual `.stats` inode and the volume summary:
```bash
echo hello > ~/squeezefs-sandbox/mnt/hello.txt && cat ~/squeezefs-sandbox/mnt/hello.txt

# Live daemon metrics (JSON): transport geometry, cache tiers, layout mix, …
head -40 ~/squeezefs-sandbox/mnt/.stats

# Volume config / health summary (JSON; "Clients" lists the mount registrations),
# and honest statfs numbers
./target/release/squeezefs status sqmeta://$HOME/squeezefs-sandbox/meta.bin
df -h ~/squeezefs-sandbox/mnt

# Who has this filesystem mounted? (heartbeat records; safe beside the live mount)
./target/release/squeezefs clients sqmeta://$HOME/squeezefs-sandbox/meta.bin
#   KIND    ID                                     PID      STATE  AGE   VOLUME
#   client  <uuid>                                 <pid>    live   3s    …/meta.bin
#   writer  <uuid>                                 <pid>    live   3s    …/meta.bin

# Space/inode accounting straight from the volumes (offline verb — also works
# with no mount running; reports the durable state)
./target/release/squeezefs df -g sqmeta://$HOME/squeezefs-sandbox/meta.bin
#   Data:   capacity 8.00 GiB   used 0 B (0.0%)   free 8.00 GiB
#   Inodes: quota 1000000   used 2   free 999998
```

### Step 6: Run the Benchmark
A bare `squeezefs bench <mountpoint>` invocation runs the **full saturation suite** over one auto-sized dataset (threads = `min(CPUs, 16)`; total = `max(16 GiB, 2 GiB × threads)` capped at 25% of free space): write seq 1m → read seq 1m → read rand 4k (30 s) → write rand 4k (30 s) → stat → del, all I/O passes O_DIRECT, mount left clean. **Auto-sizing refuses loudly when even its 4 GiB minimum dataset does not fit under the 25%-of-free cap** — so the bare suite wants ≥ 16 GiB free (run it against the [section 2 substrate](#2-dev-box-virtual-nvme-substrate-ram-backed-nvme-of-loop) or real hardware). On this small sandbox, pass an explicit shape instead:
```bash
# Sandbox-sized: write then read back 256 MiB per thread at 1 MiB ops across 4 threads
./target/release/squeezefs bench ~/squeezefs-sandbox/mnt -t 4 -w -r -s 256m -b 1m

# Re-read the SAME dataset at a different block size (no rewrite), then clean up
./target/release/squeezefs bench ~/squeezefs-sandbox/mnt -t 4 -r -s 256m -b 128k
./target/release/squeezefs bench ~/squeezefs-sandbox/mnt -t 4 --del -s 256m
```
Explicit phases inherit the same auto defaults (comparable numbers) and reuse the persistent dataset at `<mountpoint>/squeezefs-bench/`; every run prints its computed shape with `(auto)`/`(explicit)` provenance in the header.

Committed reference numbers live in `.benchmarks/` (each note states its box/substrate/method) — e.g. large-seq ~1.8 GB/s via the zero-copy write path (`.benchmarks/2026-07-08-zero-copy-write-path-closing.md`) and the default-mount 300–320 k device-true rand-4k IOPS class (`.benchmarks/2026-07-15-l1-transport-concurrency.md`). Compare your rows against those when validating a setup — file-backed sandbox rows will be substantially lower than substrate/hardware rows by design.

### Step 7: Unmount Safely
Use SqueezeFS unmount to drain staging writes and cleanly shut down:
```bash
./target/release/squeezefs umount ~/squeezefs-sandbox/mnt
```
*(Or `fusermount3 -u ~/squeezefs-sandbox/mnt`; standard root `/bin/umount` works on `--allow-other` mounts.)*

---

## 2. Dev Box: Virtual NVMe Substrate (RAM-backed NVMe-oF Loop)

If your dev box has no spare raw NVMe, do **not** settle for file-backed volumes on a CoW filesystem — build the virtual substrate instead. One command creates real `/dev/nvmeXnY` namespaces out of RAM block devices, driven through the kernel's NVMe-oF **loop** target (the same class of device the SqueezeFS harnesses use, and the closest local analog to the NVMe-oF production path):

```
metadata (mds):  memory-backed null_blk ──┐
                                          ├── nvmet loop subsystem ── nvme connect -t loop ── /dev/nvmeXnY
data     (oss):  zram (compressed RAM) ───┘
```

```bash
sudo tests/dev_substrate.sh create     # 4 mds (null_blk) + 4 oss (zram) namespaces
sudo tests/dev_substrate.sh status     # device table + a ready-to-paste format/mount hint
sudo tests/dev_substrate.sh teardown   # removes ONLY what it created (nothing foreign)
```

`create` prints the exact `squeezefs format` / `mount` lines against the namespaces it just made. `status` shows which controller/namespace backs which role and what is in use. `teardown` refuses (loudly, with a list) if filesystems are still mounted from the namespaces — `SQZ_DEVSUB_FORCE=1` unmounts *its own* devices' mountpoints and proceeds. Everything is namespaced (`nqn.2026-07.io.squeezefs:devsub-*`, `sqzdevsub_*` null_blk items, a state manifest under `/run/squeezefs-devsub/`), so foreign nvmet subsystems, zram devices (e.g. zram swap), and null_blk instances are never touched; kernel modules are loaded on demand and deliberately never unloaded on teardown.

### Why not file-backed volumes on btrfs/CoW?

Measured on this repo's reference dev box (`.benchmarks/2026-07-14-metadata-throughput-baseline.md`): the metadata journal's barrier primitive (4 KiB write + fdatasync) costs **~495 µs p50 on a btrfs CoW file vs ~3 µs on memory-backed null_blk — 165×** (`chattr +C` does not fix it: ~465 µs), and under strict commit cadence file-backed-on-btrfs collapses metadata throughput **3.8–8.9×** (rename 6.3 k → 0.7 k ops/s). The nvmet-loop stack adds only ~6 µs over raw null_blk while exercising the **full kernel NVMe target/host stack**. The virtual substrate also gives you what a file never can:

- real FLUSH/FUA semantics on the metadata path (`fua=1`, `write_cache=write back`),
- **NVMe Persistent Reservations** — the single-writer mount guard runs **enforcement-grade** (`writer_guard_mode` reads `flock+pr` on the `.stats` inode, vs `flock`-only on files),
- `meta_volume_atomicity_physical` classifying as a real block device (`atomic4k`) instead of `file-backed`.

### Sizing knobs & RAM math

All knobs are env vars documented in the script header (`tests/dev_substrate.sh --help`). Defaults: `SQZ_DEVSUB_MDS_COUNT=4` × `SQZ_DEVSUB_MDS_GB=1` GiB memory-backed null_blk (+ `SQZ_DEVSUB_MDS_CACHE_MB=256` write-back cache each) and `SQZ_DEVSUB_OSS_COUNT=4` × `SQZ_DEVSUB_OSS_GB=8` GiB zram (`SQZ_DEVSUB_OSS_ALGO=zstd`). RAM cost: mds ≤ ~5 GiB worst case (allocated on write); oss disksize is **virtual** — resident RAM ≈ the *compressed* working set (ceiling 32 GiB only if you fill every byte with incompressible data; typical dev/bench sets are a few GiB). `SQZ_DEVSUB_OSS_MEM_LIMIT_GB` hard-caps zram RAM if you need a guarantee (writes past the cap fail with EIO). Comfortable on a ≥ 64 GiB box at defaults.

### Migrating off `~/tmp/nvme/*.nvme` file-backed volumes

There is nothing to convert: format **fresh** volumes on the substrate namespaces (the `create` output hands you the lines) and stop pointing mounts at the old `.nvme` files. SqueezeFS treats the namespaces as ordinary block devices; the old file volumes keep working if you ever need to mount them for archaeology, but don't benchmark against them.

What migrating changes, measured end-to-end (same binary/box/shapes/default mount knobs, only the substrate differs — full method, all ten rows, and the honest anomalies in [`.benchmarks/2026-07-17-substrate-migration-ab.md`](.benchmarks/2026-07-17-substrate-migration-ab.md); medians of 3, `squeezefs bench`):

| headline row | file-backed btrfs (before) | virtual NVMe substrate (after) | after/before |
|---|---|---|---|
| create 25 k × 4 KiB files (fsync/file), **strict** cadence | 677 files/s | 7,058 files/s | **10.4×** |
| delete 25 k, **strict** cadence | 3,303 ops/s | 24,606 ops/s | **7.5×** |
| create 25 k × 4 KiB files (fsync/file), default cadence | 2,550 files/s | 8,295 files/s | **3.3×** |
| rand write 4 KiB O_DIRECT (30 s) | 7,155 IOPS (spread **4.8 k–24.4 k** — btrfs CoW churn) | 35,971 IOPS (±3 %) | **5.0×** |
| seq read 1 MiB O_DIRECT, cold mount | 3,284 MiB/s | 7,718 MiB/s | **2.4×** |

Two rows go the other way and are reported honestly in the note: seq write 1 MiB is 0.86× (host page cache over btrfs draining to a physical SSD beats zram-zstd on incompressible fill), and default-cadence delete is a wash (0.99×, ranges overlap). Stat is substrate-independent (~1× — the control row). The same session also reproduces the strict-cadence bracket the earlier microbenchmarks predicted: on btrfs files strict create collapses to 27 % of default (inside the md-baseline 3.8–8.9× band), on the substrate it holds ≥ 85 % (the OQ-5 constant) — and the substrate runs the guard enforcement-grade (`flock+pr`, `atomic4k`) where files run `flock+claim`, `file-backed`.

> **⚠️ Durability: dev/test only.** Every byte lives in RAM. The volumes (and the devices themselves) vanish on reboot — by design. Never put production data on this substrate. Reformat after every reboot, or have the substrate recreated at boot and reformat on top of it:
>
> ```bash
> tests/dev_substrate.sh systemd-unit | sudo tee /etc/systemd/system/squeezefs-devsub.service
> sudo systemctl daemon-reload && sudo systemctl enable --now squeezefs-devsub.service
> ```
>
> (The script only *emits* the unit — installing it is your choice.)

> **Scripted format→mount flows:** after `squeezefs format` closes a block device, udevd's change-event probe briefly holds an exclusive `flock` on the node, which the single-writer mount guard correctly refuses ("another squeezefs process holds the writer lock" names the wrong holder — the lock is udev's). Interactive use never notices; back-to-back scripts should run `udevadm settle` between format and mount.

---

## 3. Bare-Metal Execution (Real Hardware Setup)

*(Requires dedicated physical NVMe drives — nothing in this section runs on the sandbox/substrate above.)*

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
Format the logical volumes. Declare staging/cache directories on a **fast local NVMe filesystem** (they are recorded at format — the single source of truth; omit for a cache-less filesystem). Pass `--full` if you want a complete block-aligned zero-wipe of the devices:
```bash
sudo mkdir -p /srv/squeezefs_staging   # on local NVMe, not tmpfs
./target/release/squeezefs format \
  sqmeta:///dev/main-pool/meta-vol \
  sqdata:///dev/main-pool/data-vol \
  --disk-cache-paths /srv/squeezefs_staging \
  --full
```

### Step 3: Mount and Run
Cache/staging paths come from the format config (mount rejects `--disk-cache-paths`):
```bash
sudo ./target/release/squeezefs mount \
  sqmeta:///dev/main-pool/meta-vol \
  /mnt/squeezefs \
  --daemon \
  --allow-other
```
For unattended hosts add `--supervise`: the parent stays alive as an external watchdog that probes `<mountpoint>/.stats` and (as root) aborts a wedged FUSE connection to release blocked callers — see README → *External mount supervisor*.

---

## 4. NVMe-oF Fabric Setup (Remote Block Storage)

*(Requires dedicated hardware: spare PCIe NVMe controllers for SPDK user-space binding and a real fabric. Fabric multipath/failover across multiple NICs is the kernel NVMe initiator's native multipath domain — configure it with `nvme connect` policies at the host level.)*

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
sudo ./target/release/squeezefs mount sqmeta:///dev/main-pool/meta-vol /mnt/squeezefs --daemon --allow-other
```
- **Single-writer guard on fabric namespaces:** where the namespace advertises NVMe Persistent Reservation support, the mount guard runs **enforcement-grade** (the device itself fences stale writers) — check `writer_guard_mode` on `.stats`. Guarantee classes per substrate: README → *Single-writer mount guard*.
- **Multipath/HA:** path redundancy across NICs/ports is native NVMe multipath, configured at `nvme connect` time (kernel initiator), transparent to SqueezeFS.

---

## 5. Kernel Tuning for Bare Metal (Auto-Tune)

For maximum HPC file throughput, Squeezefs includes an auto-tuning command. This script adjusts FUSE congestion thresholds, virtual memory dirty page ratios, and network socket maximum buffer sizes.

Run the built-in tune command:
```bash
sudo ./target/release/squeezefs tune
```
- **`vm.dirty_ratio = 40`** & **`vm.dirty_background_ratio = 10`**: Aggressively buffers writes in memory before flushing.
- **`net.core.rmem_max`** & **`net.core.wmem_max` to `67108864` (64MB)**: Expands TCP socket buffers for massive parallel streams.
- **FUSE Connection Limits**: Raises live connections to the L1 policy ceiling — `max_background = 256` and `congestion_threshold = 192` (mounts negotiate these at INIT by default since the IOPS-parity program; `tune` only matters for connections mounted by older binaries, and never lowers a new mount).

> **Runtime trick**: `max_background`/`congestion_threshold` are writable per live FUSE connection without a remount — `echo 256 | sudo tee /sys/fs/fuse/connections/<minor>/max_background` (find `<minor>` via `stat -c %d <mountpoint>` minor number). The IOPS-parity investigation used exactly this to prove `max_background` gates FUSE-over-io_uring traffic too.

> **Benchmarking O_DIRECT (hybrid I/O)**: by default O_DIRECT reads serve from and warm SqueezeFS's own read tiers (kernel page cache stays bypassed) — repeated `--direct` read rows measure the *hybrid* posture and converge to RAM speed. For **device-path / amplification measurement** (the `.benchmarks` methodology rows), mount with `-o direct_device_true`: O_DIRECT reads then hit the device every time, admit nothing, and `squeezefs bench --direct` prints `direct-I/O posture: device-true` in its header. Buffered I/O is unaffected either way.

---

## 6. Metadata Durability Knobs

The metadata crash contract is documented in `README.md` → *Metadata Durability*: v3 (CoW KV metadata) holds it **by construction** — every on-disk unit is checksummed, torn writes are detected-and-ignored (never applied), and each transaction commits atomically as one checksummed journal entry (`docs/design-cow-kv-metadata.md` §4.10). Operationally:

```bash
# Strict sync-on-commit metadata durability (default is a 50 ms deferred window):
SQUEEZEFS_META_FLUSH_INTERVAL_MS=0 ./target/release/squeezefs mount …

# v3 node-cache RAM budget (default 512 MiB) and dirty-node checkpoint cap
# (default 4096; bounds the mount-replay working set):
SQUEEZEFS_META_NODE_CACHE_MB=1024 \
SQUEEZEFS_META_CHECKPOINT_MAX_DIRTY_NODES=8192 \
  ./target/release/squeezefs mount …

# Inode-reclaim group-commit batch size (default 64):
SQUEEZEFS_RECLAIM_BATCH=128 ./target/release/squeezefs mount …

# Read-path knobs (defaults are the measured sweet spot — see README →
# "Read-path tuning" and docs/design-read-path.md). Examples:
#   pin a memory budget instead of the cgroup-derived default:
./target/release/squeezefs mount … --mem-budget 6G
#   disable the sequential prefetch pipeline / sub-block ranged reads (A/B):
SQUEEZEFS_READ_PREFETCH_WINDOW=0 SQUEEZEFS_READ_RANGED_THRESHOLD=0 \
  ./target/release/squeezefs mount …
#   restore unconditional first-touch tier publishes (pre-program behavior):
SQUEEZEFS_READ_TIER_ADMISSION=always ./target/release/squeezefs mount …

# (Root mounts inherit env through sudo -E.)
```

v3 **format-time** knobs (`README.md` → *Format Squeezefs Volume*): `--meta-node-kib <64|128|256|512|1024>` (node size, default `256`; below 256 the per-volume record-value cap drops to `node_size/4`) and `--meta-journal-mb <MiB>` (journal ring, default `clamp(volume/64, 8 MiB, 32 MiB)`).

File-backed sandbox volumes (section 1) classify **physically** as `file-backed` (reported as `meta_volume_atomicity_physical` on the `.stats` inode) — purely informational: metadata integrity does not depend on it; the contract field `meta_volume_atomicity` reads `cow-checksummed`.

### Single-writer mount guard

Every write mount exclusively claims its metadata volume(s): a dedicated `flock` (same-host), an NVMe Persistent Reservation where the namespace supports it (cross-host enforcement), and a `writer_claim` heartbeat record. A second concurrent mount is **refused loudly, naming the holder** (`claim: id=…, pid=…, boot=…, age=…`) — there is no bypass flag. Same-host crashes (even `kill -9`) reclaim instantly and automatically at the next mount; after a cross-host crash on a volume **without** reservation support, clear the stale claim by operator attestation once you have verified the named holder is dead:

```bash
./target/release/squeezefs claim clear sqmeta://$HOME/squeezefs-sandbox/meta.bin
```

The verb re-verifies staleness under its own probe: on a healthy volume it answers `no writer claim present — nothing to clear`, and it refuses fresh claims and live-mounted volumes. Guarantee classes per substrate (and the full recovery runbook): `README.md` → *Single-writer mount guard*.

> **Legacy format v2**: support was removed entirely. A v2 superblock refuses to mount ("no longer supported; reformat required"); reformat it to v3 with `squeezefs format --force` (destroys the old contents). The offline `squeezefs migrate` converter was deleted along with v2 support.
>
> The full catalog of loud refusal classes (pre-watermark v3 volumes, pre-FIND-RW4-A compressed/encrypted volumes, staging generation binding, cache-path policy) lives in `README.md` → *Breaking changes & migration notes* — every message names its cause and remedy.
