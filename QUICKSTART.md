# SqueezeFS Quick Start Guide

This guide gets SqueezeFS running: a local sandbox, a RAM-backed virtual NVMe substrate for a dev box, bare metal, NVMe-oF fabrics, the interception shim, kernel tuning, durability knobs and the maintenance verbs. The project overview is [README.md](README.md); the full operator reference (guarantee classes, every knob, every metric) is [docs/operations.md](docs/operations.md).

---

## 1. Quick Local Sandbox (File-backed Testing)

You can run and test SqueezeFS on any Linux machine using pre-allocated files as volumes. No raw disks, no root, no database servers.

### Prerequisites (Ubuntu/Debian)
```bash
sudo apt update && sudo apt install -y \
    build-essential pkg-config libfuse3-dev fuse3 clang libclang-dev
```
> **The memlock limit.** On a kernel with the kmbuf surface (the sqz kernel series) FUSE-over-io_uring pins `queues × entries × ~1 MiB` of kernel-managed payload buffers at mount — one queue per possible CPU, 32 entries by default: 1 GiB on a 32-CPU box — and an unprivileged mount is bounded by `RLIMIT_MEMLOCK` (8 MiB on most distributions), so a fresh install's first mount can refuse `ENOMEM` at its first queue. Raise it before mounting — `ulimit -l unlimited` in the mounting shell, or persistently through `/etc/security/limits.d/` / `DefaultLimitMEMLOCK=infinity` (new sessions only); root and `CAP_IPC_LOCK` are exempt. The derivation table and the three places are in [docs/operations.md → Prerequisites](docs/operations.md#prerequisites--the-memlock-limit-rlimit_memlock).

### Step 1: Build
```bash
cargo build --release
./target/release/squeezefs --version
#   squeezefs 1.2.4 (<commit> / <full commit>) built <timestamp> profile release
```

The packaged build path is [go-task](https://taskfile.dev) (`Taskfile.yml`; plain cargo stays valid). Install it into `./bin` without root if it is absent:
```bash
command -v task go-task >/dev/null || \
  sh -c "$(curl -fsSL https://taskfile.dev/install.sh)" -- -d -b ./bin
export PATH="$PWD/bin:$PATH"

task build              # host build → dist/host/{squeezefs, libsqueezefs_il.so}
task build:rocky8       # distro container builds (needs docker or podman):
task build:all          #   rocky8 / rocky9 / ubuntu2404 / ubuntu2604 → dist/<target>/
task dist:rocky8        # tagged-release build (full LTO) → dist/rocky8-dist/
```
Each build folder pairs the daemon with the interception shim from the same build; deploy a folder as a unit — the two refuse to pair across builds.

### Step 2: Prepare Sandbox Backing Files
Keep everything under your own `$HOME` (a `sudo mount` over user-created `/tmp` files fails with `Permission denied` on modern kernels). The files are sparse — they consume disk only as blocks are written:
```bash
mkdir -p ~/squeezefs-sandbox/staging ~/squeezefs-sandbox/mnt
truncate -s 256M ~/squeezefs-sandbox/meta.bin    # metadata volume
truncate -s 8G   ~/squeezefs-sandbox/data.bin    # data volume
```
> File-backed volumes are fine for a functional sandbox, but do not benchmark on them — especially on btrfs or another copy-on-write host filesystem. For anything measured, build the virtual NVMe substrate in [section 2](#2-dev-box-virtual-nvme-substrate-ram-backed-nvme-of-loop).

### Step 3: Format the Filesystem
Cache/staging paths are declared **at format** and recorded in the volume; omit `--disk-cache-paths` for a permanently cache-less filesystem:
```bash
./target/release/squeezefs format \
  sqmeta://$HOME/squeezefs-sandbox/meta.bin \
  sqdata://$HOME/squeezefs-sandbox/data.bin \
  --disk-cache-paths ~/squeezefs-sandbox/staging
```
> Fresh formats stamp the symmetric slot-tree forest by default (incompat bit 17 beside the multi-writer bits): a plain mount is one armed writer, every later RW mount of the set joins it as a full writer, and `-o ro` mounts are read-token clients. Pass `--single-writer` only for a volume that will never see a second host (the flat one-writer posture; the ONLY flat writable class). `--symmetric` is the default's spelling (accepted, no effect). A set formatted between 2026-08-16 and this release without bit 17 refuses a writable mount naming `squeezefs volume enable-symmetric` — the offline conversion (`docs/operations.md → Converting an existing set to the symmetric forest`). Optional format knobs (`--meta-node-kib`, `--meta-journal-mb`, compression, encryption) are listed in [docs/operations.md → Format](docs/operations.md#format-squeezefs-format).

### Step 4: Mount
The mount reads its cache/staging paths from the volume (passing `--disk-cache-paths` at mount is refused; change them with `squeezefs config set-cache-paths`):
```bash
./target/release/squeezefs mount \
  sqmeta://$HOME/squeezefs-sandbox/meta.bin \
  ~/squeezefs-sandbox/mnt \
  --daemon \
  --log-file ~/squeezefs-sandbox/mount.log
```
The mount log must contain `FUSE-over-io_uring transport armed for this session` — the transport is required, and the mount fails loudly if the kernel cannot provide it (the mount enables `fuse.enable_uring` itself where it can). Add `--allow-other` (root, or `user_allow_other` in `/etc/fuse.conf`) if other users — including root — must access the mount, and `--uid`/`--gid` to change the presented file ownership.

### Step 5: Verify the Mount
```bash
echo hello > ~/squeezefs-sandbox/mnt/hello.txt && cat ~/squeezefs-sandbox/mnt/hello.txt

# Live daemon metrics (JSON)
head -40 ~/squeezefs-sandbox/mnt/.stats

# Volume config / health summary (JSON; "Clients" lists the mount registrations)
./target/release/squeezefs status sqmeta://$HOME/squeezefs-sandbox/meta.bin
df -h ~/squeezefs-sandbox/mnt

# Who has this filesystem mounted? (safe beside the live mount)
./target/release/squeezefs clients sqmeta://$HOME/squeezefs-sandbox/meta.bin
#   KIND    ID                                     PID      STATE  AGE   VOLUME
#   client  <uuid>                                 <pid>    live   3s    …/meta.bin
#   writer  <uuid>                                 <pid>    live   3s    …/meta.bin

# Space/inode accounting straight from the volumes (works with no mount running)
./target/release/squeezefs df -g sqmeta://$HOME/squeezefs-sandbox/meta.bin
#   Data:   capacity 8.00 GiB   used 0 B (0.0%)   free 8.00 GiB
#   Inodes: quota 1000000   used 2   free 999998
```

> **Large files.** Files above roughly 6–8 GiB (at the default 4 MiB block size) are handled automatically — their block map moves into the metadata tree and nothing needs configuring. If you are curious, `grep kvmap ~/squeezefs-sandbox/mount.log` shows it happening (`kvmap crossing: ino N entered the block-map tree …`).
>
> **Small files.** A file up to one page lives inside its metadata record. On a filesystem formatted with staging paths (this sandbox), a file up to one block (4 MiB) stages on this host's local NVMe and is promoted to the shared devices — many files packed into one block — under staging pressure and at the mount's clean unmount; until then other clients of the volume set read it as zeros. `SQUEEZEFS_FSYNC_PROMOTE_STAGED=1` (default off) makes `fsync(2)` promote it too. Details: [docs/operations.md → Breaking changes & migration notes](docs/operations.md#breaking-changes--migration-notes).

### Step 6: Run the Benchmark
A bare `squeezefs bench <mountpoint>` runs the full suite over one auto-sized dataset (threads = `min(CPUs, 16)`; total = `max(16 GiB, 2 GiB × threads)`, capped at 25 % of free space): sequential write → sequential read → random 4k read (30 s) → random 4k write (30 s) → stat → delete, all O_DIRECT, mount left clean. The bare suite refuses loudly when even its minimum dataset does not fit, so it wants ≥ 16 GiB free — run it against the [section 2 substrate](#2-dev-box-virtual-nvme-substrate-ram-backed-nvme-of-loop) or real hardware. On this small sandbox, pass an explicit shape:
```bash
# Sandbox-sized: write then read back 256 MiB per thread at 1 MiB ops across 4 threads
./target/release/squeezefs bench ~/squeezefs-sandbox/mnt -t 4 -w -r -s 256m -b 1m

# Re-read the SAME dataset at a different block size (no rewrite), then clean up
./target/release/squeezefs bench ~/squeezefs-sandbox/mnt -t 4 -r -s 256m -b 128k
./target/release/squeezefs bench ~/squeezefs-sandbox/mnt -t 4 --del -s 256m
```
Explicit phases reuse the persistent dataset at `<mountpoint>/squeezefs-bench/`; every run prints its computed shape in the header. File-backed sandbox rows are far below hardware rows by design — the measured records are in [docs/operations.md → Performance records](docs/operations.md#performance-records).

### Step 7: Unmount Safely
```bash
./target/release/squeezefs umount ~/squeezefs-sandbox/mnt
```
A clean unmount is a durability boundary: the daemon's teardown promotes every staged-layout file still resident in this host's local staging to the shared devices (packed into shared blocks — the log line is `dismount promoted N staged-layout file(s)`), so other clients read them afterwards. On a TTY the verb warns about unflushed **active write blocks** and offers to wait for their writeback (`-f` skips the prompt). *(Or `fusermount3 -u ~/squeezefs-sandbox/mnt`; root `/bin/umount` works on `--allow-other` mounts — both run the same daemon teardown.)*

---

## 2. Dev Box: Virtual NVMe Substrate (RAM-backed NVMe-oF Loop)

If your dev box has no spare raw NVMe, build the virtual substrate instead of using file-backed volumes. One command creates real `/dev/nvmeXnY` namespaces out of RAM block devices, served through the kernel's NVMe-oF **loop** target — the closest local analog to the NVMe-oF production path:

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

`create` prints the exact `squeezefs format` / `mount` lines for the namespaces it just made. `teardown` refuses (with a list) while filesystems are still mounted from the namespaces — `SQZ_DEVSUB_FORCE=1` unmounts its own devices' mountpoints and proceeds. Foreign nvmet subsystems, zram devices (e.g. zram swap) and null_blk instances are never touched.

Set `SQZ_DEVSUB_TRANSPORT=tcp` to build the same shape over **NVMe/TCP on localhost** instead of loop — the venue for anything bandwidth- or network-sensitive (writes, multi-connection workloads); the two substrates coexist on one box:
```bash
sudo SQZ_DEVSUB_TRANSPORT=tcp tests/dev_substrate.sh create
```

Why not files? On a copy-on-write host filesystem the metadata journal's flush barrier is two orders of magnitude slower than on a block device, and files offer neither real FLUSH semantics nor NVMe Persistent Reservations (so the mount guard runs detection-grade instead of enforcement-grade). Details: [docs/operations.md → Single-writer mount guard](docs/operations.md#single-writer-mount-guard-guarantee-classes).

**Sizing.** Every knob is an env var documented in `tests/dev_substrate.sh --help`. Defaults: `SQZ_DEVSUB_MDS_COUNT=4` × `SQZ_DEVSUB_MDS_GB=1` GiB null_blk and `SQZ_DEVSUB_OSS_COUNT=4` × `SQZ_DEVSUB_OSS_GB=8` GiB zram (`SQZ_DEVSUB_OSS_ALGO=zstd`). zram disk size is virtual — resident RAM is roughly the compressed working set; `SQZ_DEVSUB_OSS_MEM_LIMIT_GB` hard-caps it (writes past the cap fail with EIO). Comfortable on a ≥ 64 GiB box at defaults.

> **⚠️ Durability: dev/test only.** Every byte lives in RAM; the volumes and the devices vanish on reboot. Reformat after every reboot, or install the emitted unit so the substrate is recreated at boot (then reformat on top of it):
>
> ```bash
> tests/dev_substrate.sh systemd-unit | sudo tee /etc/systemd/system/squeezefs-devsub.service
> sudo systemctl daemon-reload && sudo systemctl enable --now squeezefs-devsub.service
> ```

> **Scripted format→mount flows:** right after `format`, udev briefly re-probes the device and holds a transient lock on it. The mount waits such anonymous holders out (about 2 s) instead of refusing, so `udevadm settle` between format and mount is not required.

---

## 3. Bare-Metal Execution (Real Hardware Setup)

*(Requires dedicated physical NVMe drives — nothing in this section runs on the sandbox/substrate above.)*

### Step 1: Create Storage Pool and Volume
Assume `/dev/nvme0n1` and `/dev/nvme1n1` are dedicated NVMe drives:
```bash
./target/release/squeezefs storage pool create main-pool /dev/nvme0n1 /dev/nvme1n1
./target/release/squeezefs storage volume create main-pool meta-vol --size 128G
./target/release/squeezefs storage volume create main-pool data-vol --size 1P
```

### Step 2: Format
Declare staging/cache directories on a **fast local NVMe filesystem** (not tmpfs); omit them for a cache-less filesystem. `--full` zero-wipes the devices instead of quick-formatting:
```bash
sudo mkdir -p /srv/squeezefs_staging
./target/release/squeezefs format \
  sqmeta:///dev/main-pool/meta-vol \
  sqdata:///dev/main-pool/data-vol \
  --disk-cache-paths /srv/squeezefs_staging \
  --full
```

### Step 3: Mount and Run
```bash
sudo ./target/release/squeezefs mount \
  sqmeta:///dev/main-pool/meta-vol \
  /mnt/squeezefs \
  --daemon \
  --allow-other
```
For unattended hosts add `--supervise`: the parent stays alive as an external watchdog that probes the mount and, as root, aborts a wedged FUSE connection to release blocked callers — see [docs/operations.md → External mount supervisor](docs/operations.md#external-mount-supervisor-mount---daemon---supervise). Read-only hosts mount with `--read-only` (or `-o ro`); the consistency they get is stated in [docs/operations.md → Read-only coherent mounts](docs/operations.md#read-only-coherent-mounts--o-ro--one-writer-plus-n-readers).

---

## 4. NVMe-oF Fabric Setup (Remote Block Storage)

Target sharing and client connections live under **`squeezefs nvmeof`**. The kernel `nvmet` target is the one supported target stack (`--target-stack nvmet` / `SQUEEZEFS_NVMEOF_TARGET_STACK=nvmet` is the default and the only admissible value). **SPDK was retired as a target on 2026-09-12** (owner ruling R-SYM-8 — its 16-registrant cap was the only hard registrant ceiling SqueezeFS shipped): `--target-stack spdk` and `nvmeof target install` refuse loud naming nvmet and the re-share sequence, and an SPDK share still in your ledger is listed by `nvmeof list` for you to re-share — the notice and the sequence: [docs/operations.md → NVMe-oF operations](docs/operations.md#nvme-of-operations).

### Prepare the Kernel nvmet Target
```bash
# Load nvmet/nvmet-tcp and check the configfs mount (idempotent):
sudo ./target/release/squeezefs nvmeof target setup

# "Start" = the same readiness check plus a replay of the share ledger;
# configfs IS the running target — nothing is a process, so `target stop` refuses.
sudo ./target/release/squeezefs nvmeof target start

# Health: module presence, configfs, subsystem/namespace/port counts (+ resv_enable):
sudo ./target/release/squeezefs nvmeof target status --json

# Production: emit the oneshot restore unit (squeezefs never installs units — you do):
sudo ./target/release/squeezefs nvmeof target systemd-unit > squeezefs-nvmet-restore.service
```

### Share a Target
```bash
# Share a backing disk as an NVMe-oF subsystem. resv_enable is stamped before
# enable, so the writer guard runs enforcement-grade (Write-Exclusive PR).
sudo ./target/release/squeezefs nvmeof share /dev/nvme1n1 --ip 10.10.10.50

# A regular file as backing (missing paths refuse; --create-size opts into creating one):
sudo ./target/release/squeezefs nvmeof share /srv/backing.img --create-size 100G --ip 10.10.10.50

# Restrict who may connect (default is allow-any — the trusted-fabric posture):
sudo ./target/release/squeezefs nvmeof share /dev/nvme1n1 --ip 10.10.10.50 \
    --allow-host nqn.2014-08.org.nvmexpress:uuid:<client-host-id>
```
Every share is recorded in the share ledger (`/var/lib/squeezefs/nvmeof/shares.json`) so `restore` re-presents it under the same identity (`device_uuid` = the recorded `ns_uuid`) after a target restart. Listener port ids come from the reserved range 53000–53999 (`SQUEEZEFS_NVMET_PORT_ID_BASE` relocates it); foreign configfs ports are never touched. A backing already served — live or by any ledger record, including one the retired SPDK stack left — refuses, naming the live holder and the remedy.

### Inspect, Restore, Unshare, Adopt
```bash
sudo ./target/release/squeezefs nvmeof list              # managed / down / pending / foreign / retired-spdk
sudo ./target/release/squeezefs nvmeof restore           # replay the ledger (idempotent) — the boot-time step
sudo ./target/release/squeezefs nvmeof unshare <subnqn>  # unmount → disconnect → unshare is the sequence
sudo ./target/release/squeezefs nvmeof adopt <subnqn>    # take a live foreign/unledgered share under management
```
`adopt` writes only the ledger — the live target object keeps serving with zero interruption. It is an explicit operator action and refuses unsupported shapes loudly; every refusal message names the remedy. Full semantics: [docs/operations.md → NVMe-oF operations](docs/operations.md#nvme-of-operations).

### Connect to Remote NVMe-oF Storage
```bash
sudo ./target/release/squeezefs nvmeof connect --ip 10.10.10.50 --subnqn nqn.2026-07.io.squeezefs:share-<uuid>

# Pool + volume on the fabric-attached device, then format and mount as usual
sudo ./target/release/squeezefs storage pool create fabric-pool /dev/nvme1n1
sudo ./target/release/squeezefs storage volume create fabric-pool my-fabric-vol --size 1P
sudo ./target/release/squeezefs format sqmeta:///dev/main-pool/meta-vol sqdata:///dev/fabric-pool/my-fabric-vol
sudo ./target/release/squeezefs mount sqmeta:///dev/main-pool/meta-vol /mnt/squeezefs --daemon --allow-other

# Done with a share on the client side:
sudo ./target/release/squeezefs nvmeof disconnect <subnqn>
```
- **Mount guard on fabric namespaces:** where the namespace supports NVMe Persistent Reservations, the guard is enforced by the device itself — check `writer_guard_mode` in `.stats`. Guarantee classes per substrate: [docs/operations.md → Single-writer mount guard](docs/operations.md#single-writer-mount-guard-guarantee-classes).
- **Queue-constrained targets:** if a connect fails mid-queue-setup, bound the request instead: `connect ... --nr-io-queues 8`.

#### Multi-NIC clients: a second fabric path
Share on both target addresses, connect once per path, and let native NVMe multipath merge them into one block device:
```bash
# Target: one share, listeners on both fabric addresses
sudo ./target/release/squeezefs nvmeof share /dev/nvme1n1 --ip 10.10.10.50,10.10.20.50

# Client: one connect per path (NICs on distinct subnets route themselves)
sudo ./target/release/squeezefs nvmeof connect --ip 10.10.10.50 --subnqn <subnqn>
sudo ./target/release/squeezefs nvmeof connect --ip 10.10.20.50 --subnqn <subnqn>

# NICs on the SAME subnet: pin the second path's source explicitly
sudo ./target/release/squeezefs nvmeof connect --ip 10.10.10.51 --subnqn <subnqn> \
    --host-traddr 10.10.10.22 --host-iface eth2

# Spread I/O across the paths
echo round-robin | sudo tee /sys/class/nvme-subsystem/nvme-subsys*/iopolicy
```
`nvmeof list` shows one controller row per path; `disconnect <subnqn>` tears down every path at once.

---

## 5. Benchmarking the LD_PRELOAD Interception Path (Manual)

The interception shim lets unmodified applications bypass kernel FUSE for data operations. Four steps:

### Step 1 — build both ends from the SAME commit
```bash
cargo build --release
cargo build -p squeezefs-preload --profile preload-release --features interposers
# the shim: target/preload-release/libsqueezefs_il.so
```
(`task build` does both and lands the pair side by side in `dist/host/`.) A plain `--release` build of the shim refuses at compile time — `--profile preload-release` is the only supported build. On a dirty tree (uncommitted changes) both ends need `SQUEEZEFS_IPC_ALLOW_DEV=1`.

### Step 2 — mount with interception armed
```bash
export SQUEEZEFS_IPC_ALLOW_DEV=1        # dev/dirty trees only
./target/release/squeezefs mount sqmeta:///dev/nvme1n1 /mnt/squeezefs \
    --daemon --interception --allow-other --log-file /tmp/sqz.log
# device-true measurement (no cache-tier serves): add -o direct_device_true
```
`--interception` also turns off the kernel writeback cache on this mount, so buffered small writes that do **not** go through the shim get slower by design.

### Step 3 — run your tool under the shim
The benchmark binary **must be dynamically linked** (`ldd $(command -v fio)`); a static binary silently ignores `LD_PRELOAD` and measures kernel FUSE.
```bash
export SQUEEZEFS_IPC_ALLOW_DEV=1        # match the mount
SO=$PWD/target/preload-release/libsqueezefs_il.so

# fio, libaio (iodepth concurrency rides the ring)
LD_PRELOAD=$SO fio --name=il --directory=/mnt/squeezefs --filesize=2g \
    --rw=randread --bs=4k --ioengine=libaio --iodepth=32 --direct=1 \
    --thread --numjobs=16 --group_reporting --runtime=30 --time_based

# fio, psync (positional read/write)
LD_PRELOAD=$SO fio --name=il --filename=/mnt/squeezefs/f.bin \
    --rw=randread --bs=4k --size=2g --ioengine=psync --direct=1 \
    --thread --numjobs=16 --group_reporting --runtime=30 --time_based

# elbencho (dynamically linked build)
LD_PRELOAD=$SO elbencho -w -t 16 -s 128m -b 1m --direct /mnt/squeezefs/f{1..16}

# anything else works the same way:
LD_PRELOAD=$SO cp big.bin /mnt/squeezefs/
```

### Step 4 — VERIFY the shim actually served the run
Fall-throughs to kernel FUSE are silent by design, so never publish a number without this check. Snapshot before and after your run:
```bash
grep -o '"ipc_ops_read": *[0-9]*'  /mnt/squeezefs/.stats
grep -o '"ipc_ops_write": *[0-9]*' /mnt/squeezefs/.stats
```
The delta must account for your run's operation count (ring ops are capped at 1 MiB of payload each, so runs with larger application blocks show more ring ops than application ops). If the deltas are ~0: check the binary is dynamic, both builds match, and the mount has `--interception`. Client and daemon tuning knobs are listed in [docs/operations.md → LD_PRELOAD interception](docs/operations.md#ld_preload-interception--o-interception--security-posture--unsupported-mixes).

---

## 6. Kernel Tuning for Bare Metal (Auto-Tune)

`squeezefs tune` applies the recommended host posture in one step (root required):
```bash
sudo ./target/release/squeezefs tune
```
- **`vm.dirty_ratio = 40`** and **`vm.dirty_background_ratio = 10`**: buffer more writes in memory before flushing.
- **`net.core.rmem_max`** / **`net.core.wmem_max` = 64 MiB**: larger TCP socket buffers for many parallel streams.
- **FUSE connection limits**: raises `max_background`/`congestion_threshold` on live connections mounted by older binaries (new mounts negotiate them at mount time; `tune` never lowers one), and disables `read_ahead_kb` on the FUSE bdi.

> **O_DIRECT reads** serve from and warm SqueezeFS's own cache tiers by default (the kernel page cache stays bypassed), so repeated O_DIRECT read runs converge to RAM speed. For device-path measurement mount with `-o direct_device_true`; `squeezefs bench --direct` prints which posture the mount carries. Details: [docs/operations.md → Hybrid I/O](docs/operations.md#hybrid-io-for-o_direct-reads-default-and-the-device-true-escape).

---

## 7. Metadata Durability Knobs

Metadata is crash-safe by construction — every change is atomic and checksummed, torn writes are detected and ignored — on any device, including plain files ([docs/operations.md → Metadata Durability](docs/operations.md#metadata-durability-crash-contract)). The operational knobs:

```bash
# Strict sync-on-commit metadata durability (default is a 50 ms deferred window):
SQUEEZEFS_META_FLUSH_INTERVAL_MS=0 ./target/release/squeezefs mount …

# Metadata node-cache RAM budget (default derived; _PCT is the percentage spelling)
# and the dirty-node checkpoint cap (bounds the mount-time replay working set):
SQUEEZEFS_META_NODE_CACHE_MB=1024 \
SQUEEZEFS_META_CHECKPOINT_MAX_DIRTY_NODES=8192 \
  ./target/release/squeezefs mount …

# Inode-reclaim batch size (default 64). The old SQUEEZEFS_RECLAIM_BATCH /
# _BATCH_WINDOW_MS / SQUEEZEFS_RECLAIM_CONCURRENCY spellings refuse the mount,
# naming these successors:
SQUEEZEFS_INODE_RECLAIM_BATCH=128 ./target/release/squeezefs mount …

# Pin the daemon's memory budget instead of the cgroup-derived default:
./target/release/squeezefs mount … --mem-budget 6G

# (Root mounts inherit env through sudo -E.)
```

Format-time knobs: `--meta-node-kib <64|128|256|512|1024>` (metadata node size, default `256`) and `--meta-journal-mb <MiB>` (journal ring size) — [docs/operations.md → Format](docs/operations.md#format-squeezefs-format). The complete knob reference, with every default, is [docs/operations.md → Environment knobs](docs/operations.md#environment-knobs--the-parsing-convention).

### Single-writer mount guard

Every write mount exclusively claims its metadata volumes: a `flock` (same host), an NVMe Persistent Reservation where the device supports one (cross-host, enforced by the device), and a heartbeat claim record. A second concurrent write mount is **refused loudly, naming the holder** — there is no bypass flag. Same-host crashes (even `kill -9`) reclaim automatically at the next mount. After a cross-host crash on a volume **without** reservation support, clear the stale claim once you have verified the named holder is dead:

```bash
./target/release/squeezefs claim clear sqmeta://$HOME/squeezefs-sandbox/meta.bin
```

The verb re-verifies staleness itself: on a healthy volume it answers `no writer claim present — nothing to clear`, and it refuses fresh claims and live-mounted volumes. Guarantee classes per substrate and the recovery runbook: [docs/operations.md → Single-writer mount guard](docs/operations.md#single-writer-mount-guard-guarantee-classes).

> **Legacy format v2** is no longer supported: a v2 volume refuses to mount; reformat it with `squeezefs format --force` (destroys the old contents). Every other refusal an operator can hit — and its remedy — is catalogued in [docs/operations.md → Breaking changes & migration notes](docs/operations.md#breaking-changes--migration-notes).

---

## 8. Volume Lifecycle & Online Maintenance (Taste)

Volume membership, fsck and defragmentation are first-class verbs — try them against the section-1 sandbox. Long-running work executes as durable, pausable, throttled background jobs (`squeezefs job list <mountpoint>` shows them, live or offline):

```bash
# Grow the data side ONLINE: the new volume joins placement immediately and a
# rebalance pass is scheduled automatically (--no-rebalance opts out)
truncate -s 8G ~/squeezefs-sandbox/data2.bin
./target/release/squeezefs volume add-data ~/squeezefs-sandbox/mnt ~/squeezefs-sandbox/data2.bin
./target/release/squeezefs volume list ~/squeezefs-sandbox/mnt

# Shrink it again: preflight-checked drain (refused with the numbers printed
# if the survivors cannot hold the data), copy-on-write evacuation, retire
./target/release/squeezefs volume remove-data ~/squeezefs-sandbox/mnt <vol-id-from-list>

# Online filesystem check — verified findings only, exit != 0 when any exist;
# add --scrub for the full data scrub, --repair [--apply] for quarantine-first repair
./target/release/squeezefs fsck ~/squeezefs-sandbox/mnt

# Measure fragmentation on its four axes (moves nothing), then defragment what needs
# it — one axis per invocation: --data (free-space contiguity + file locality),
# --pack (re-pack half-empty small-file pack blocks, including the one-block-per-file
# population older releases wrote), --meta (merge underfull metadata nodes so a
# filled metadata volume returns space), --fold, --rebalance
./target/release/squeezefs defrag ~/squeezefs-sandbox/mnt --report-only
./target/release/squeezefs defrag ~/squeezefs-sandbox/mnt --data --throttle 25
./target/release/squeezefs defrag ~/squeezefs-sandbox/mnt --pack
./target/release/squeezefs defrag ~/squeezefs-sandbox/mnt --meta
```

A full metadata volume answers `ENOSPC` to growth (reads, deletes and overwrites keep working) and gives extents back as deletes empty its nodes. Metadata volumes grow and shrink too (`volume add-meta` / `remove-meta` are offline verbs; `volume migrate-meta-slot` moves a routing slot on the live mount), and a multi-node fleet can split metadata ownership per volume with the offline `volume set-owners`. The full runbook — capacity preflight, distributed workers, per-volume owners, the job guarantee table — is [docs/operations.md → Volume lifecycle & online maintenance](docs/operations.md#volume-lifecycle--online-maintenance).
