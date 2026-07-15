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
- **Zero-copy write path:** large sequential writes travel kernel → transport payload lease → one merge copy → io_uring DMA. Content-complete blocks upload directly (**write-through**), skipping the staging round-trip entirely; FUSE_WRITE payloads ride zero-copy leases over the registered FUSE-over-io_uring buffers. Measured on the committed reference profile: large-seq writes went from 430–512 MiB/s to ~1.8 GB/s (**≥ 3.5×**) with small-write, read, and metadata rows at-or-better — see `docs/design-zero-copy-write-path.md` and `.benchmarks/2026-07-08-zero-copy-write-path-closing.md`.

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
  Initialize physical block maps and metadata. New metadata volumes are formatted as **v3** (CoW KV metadata — see [Metadata Durability](#metadata-durability-crash-contract)). Executes concurrently across all target devices.
  ```bash
  squeezefs format sqmeta://<meta_dev> [sqmeta://...] sqdata://<data_dev> [sqdata://...] [options]
  ```
  *Options:*
  - `--block-size <bytes>`: Block size in bytes (e.g. `4M`, `1M`, default: `4M`).
  - `--capacity <bytes>`: Maximum capacity of the volume (default: auto-detected or 1PB).
  - `--inodes <count>`: Hard quota limit for number of inodes (default: `1000000`).
  - `--disk-cache-paths <paths>`: Comma-separated paths to NVMe cache staging directories. **Declared here, at format** — recorded in the format config as the single source of truth. Omit it and the filesystem is **permanently cache-less**: mounts run with RAM tiers + direct block I/O only (no NVMe staging/read-cache tier). Change later with `squeezefs config set-cache-paths`.
  - `--full`: Performs full block-aligned zero-wiping of the backing device capacity with a progress bar (default is quick-format).
  - `--meta-node-kib <64|128|256|512|1024>`: v3 metadata btree node size in KiB (default `256`). Below `256` prints a warning — the per-volume record-value cap drops to `node_size/4`, so large xattrs / layout maps spill to the indirect mechanism sooner.
  - `--meta-journal-mb <MiB>`: v3 metadata journal ring size, overriding the default `clamp(volume/64, 8 MiB, 32 MiB)`.

* **Mount Squeezefs:**
  ```bash
  squeezefs mount sqmeta://<meta_dev> [sqmeta://...] <mountpoint> [options]
  ```
  Cache/staging paths come from the format config; passing `--disk-cache-paths` at mount is a loud error (use `squeezefs config set-cache-paths` to change them).
  *Options:*
  - `--local-ips <ips>`: Comma-separated list of local source IP interfaces for multi-rail load balancing.
  - `--mem-cache-size <size>`: System RAM cache size (e.g. `16GB` or `20%`).
  - `--daemon`: Run FUSE daemon in the background (changes its working directory to `/` to avoid locking paths).
  - `--allow-others` (or `--allow-other`): Allow other users/root to access the mount (required for `sudo umount`).
  - `--log-file <path>`: Path to write daemon logs to when running in background.

* **Change cache/staging directories (admin op):**
  Guarded like `format` (refused while any client has the volume mounted); rewrites the format config and wipes the new directories so the next mount stamps a fresh staging generation.
  ```bash
  squeezefs config set-cache-paths sqmeta://<meta_dev> <path> [<path>...]
  squeezefs config get-cache-paths sqmeta://<meta_dev>
  ```

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
  A **bare invocation is the full saturation suite**: over one auto-sized dataset it runs write seq `1m` `--direct` → read seq `1m` `--direct` → read rand `4k` `--direct` (30 s box) → write rand `4k` `--direct` (30 s box) → stat → del (timed; leaves the mount clean), and prints one table with a row per pass (THROUGHPUT / IOPS / coverage / latency min/avg/p99/max) plus the daemon's `.stats` metrics delta.
  ```bash
  squeezefs bench /mnt/squeezefs
  ```
  Auto-sizing (the default for `-t`/`-n`/`-s` everywhere; explicit flags always override): threads = `min(CPUs, 16)`, 1 file per thread, total = `max(16 GiB, 2 GiB × threads)` capped at 25% of the mountpoint's free space (loud error if even 4 GiB does not fit), per-file rounded down to 1 MiB. The computed shape — with `(auto)`/`(explicit)` provenance per value — is printed loudly in the header of **every** run.

  *Explicit phases* run over the same **persistent, reusable dataset** at `<mountpoint>/squeezefs-bench/t{tid}/f{fid}.bin` and inherit the identical auto defaults, so single-phase numbers are directly comparable to the matching suite pass:
  ```bash
  # reproduce the suite's rand-4k read pass against an existing dataset:
  squeezefs bench /mnt/squeezefs -r --rand -b 4k --direct
  # write then read 1 GiB/file across 4 threads at 1 MiB ops:
  squeezefs bench /mnt/squeezefs -t 4 -w -r -s 1g -b 1m
  # re-read the SAME dataset later at a different I/O size (no rewrite):
  squeezefs bench /mnt/squeezefs -t 4 -r -s 1g -b 128k
  ```
  *Phases* (any combination, always executed in this fixed order; none given ⇒ the full suite):
  - `-w, --write`: create/overwrite the dataset, timed (includes create/open; each file is fsync'd before the clock stops — durable write numbers).
  - `-r, --read`: read it back, timed (reuses the dataset from an earlier `-w`; loud shape-mismatch error otherwise — never silently creates files).
  - `--stat`: stat every file, timed.
  - `--del`: delete the dataset, timed (doubles as cleanup).

  *Shape* (applies to all phases; `-t`/`-n`/`-s` auto-size when omitted):
  - `-t, --threads <N>`: workers (default: auto = `min(CPUs, 16)`).
  - `-n, --files <N>`: files per thread (default: auto = 1).
  - `-s, --size <SZ>`: file size, human units `4k`/`128k`/`4m`/`10g` or plain bytes (default: auto-sized from free space, see above).
  - `-b, --block <SZ>`: I/O size per operation, same units (default: `1m`; explicit phase runs only — the suite fixes `1m` seq / `4k` rand).
  - `--rand`: random offsets (shuffled full-coverage block list — every block exactly once; explicit phase runs only).
  - `--direct`: O_DIRECT (`-b` must be a multiple of 4096 and `-s` a multiple of `-b`); the suite's I/O passes are always O_DIRECT.
  - `--time <SECS>`: wall-clock box for rand read/write passes (default: 30 for `--rand`, unlimited for sequential; `0` forces full coverage). Partial coverage is honest — reported over actual elapsed/bytes and stated in the row.
  - `-i, --iterations <N>`: repeat the selected pass set (default: 1).

  Write phases fill blocks with a deterministic non-zero pattern seeded per `(thread, file, block)`, so transparent compression cannot fake throughput numbers.

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

### `df` / statfs semantics

A mounted SqueezeFS reports honest, cheap numbers to `statfs(2)` (`df`): **total** is the formatted capacity — the summed data-backend size, or the lower explicit `--capacity` quota chosen at format (the effective limit you experience); **used/free** track the bytes currently allocated on the striped block backends, maintained by the block allocators at alloc/free time (no metadata transactions or device I/O on the statfs path). Tiny inline payloads live in the metadata volume and staged-but-unpromoted small writes in the local NVMe staging dirs, so those transient bytes appear in `df` as their blocks promote via writeback rather than instantaneously; deletes return space after background reclaim completes. Inode columns (`df -i`) report the format inode quota against the v3 monotonic, no-reuse inode watermark — `IFree` is remaining create headroom, and deleting files does not raise it.

## Metadata Durability (crash contract)

SqueezeFS metadata is **format v3** (CoW KV) — the only supported metadata format (v2 support was removed; v2 volumes refuse to mount with "no longer supported; reformat required"). Its crash contract holds **by construction** (design: `docs/design-cow-kv-metadata.md`; the historical D0/D1/D2 ladder it strictly strengthens is `docs/design-wal-crash-consistency.md` §3):

- **Every on-disk unit is checksummed** — superblock, journal pages and entries, btree nodes, bsets, the allocator bitmap, and root-ledger slots.
- **Torn writes are detected and ignored, never applied.** A torn journal entry, node append, or ledger slot fails its checksum and the last consistent state serves (the old copy-on-write node / the predecessor ledger record). Nothing overwrites live data in place.
- **Whole-transaction atomicity**: one transaction = one checksummed journal entry, replayed all-or-nothing at mount. A transaction is never visible half-applied.
- **No hardware-atomicity dependency**: a file-backed volume gets the same integrity guarantee as an atomic-4KiB device. The sector-atomicity probe still runs, purely informationally, and reports as `meta_volume_atomicity_physical` on the `.stats` inode (`atomic4k` / `likely` / `unknown` / `file-backed`); the contract field `meta_volume_atomicity` reads `cow-checksummed`. (The old `--strict-meta-atomicity` mount gate only ever gated v2 volumes and was deleted with them.)

**Acked durability** (`fsync`/`fsyncdir` returning success) is carried solely by post-apply coalesced `fdatasync` barriers — exactly one physical barrier per fsync.

- `SQUEEZEFS_META_FLUSH_INTERVAL_MS`: deferred metadata durability window in ms (default `50`); `0` = strict sync-on-commit — every metadata commit returns only after a post-apply device barrier. Legacy alias `SQUEEZEFS_JOURNAL_FLUSH_INTERVAL_MS` is honored; the new name wins if both are set.
- `SQUEEZEFS_RECLAIM_BATCH`: inode-reclaim group-commit batch size (default `64`, clamp 1–1024).

### Read-path tuning (mount env; design `docs/design-read-path.md`)

Defaults are the measured sweet spot — override only with a live-counter reason (the `.stats` inode exposes every family):

- `SQUEEZEFS_READ_TIER_ADMISSION` (`second-touch` default | `always` | `never`): NVMe read-tier admission for >256 KiB fills. `second-touch` kills the streaming publish tax (a cold 16 GiB pass writes ~0 instead of ~16.9 GiB to the tier) while re-read heat still converges to the tier; `always` restores unconditional first-touch publishes (A/B escape hatch).
- `SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB`: RAM hot-block tier budget for >256 KiB blocks (default derived from the read-mem cache; `0` disables the tier and admission auto-degrades to `always`).
- `SQUEEZEFS_READ_PREFETCH_WINDOW` (default `16`, `0` disables): per-stream prefetch pipeline depth cap in blocks. The window is adaptive (2→cap, AIMD) and contention-scaled; the cap is a ceiling, not a target.
- `SQUEEZEFS_READ_PREFETCH_SHARE_PCT` (default `50`): the prefetch pipeline's share of the hot-tier budget in the contention-scaling formula — lower it if concurrent stream count routinely exceeds hot-tier capacity.
- `SQUEEZEFS_READ_RANGED_THRESHOLD` (default `262144`, `0` disables): reads at or under this size on passthrough (uncompressed/unencrypted) volumes fetch only their 4 KiB-aligned device window instead of the whole block — the rand-4k amplification kill (≈1000× → ~1.0×). Compressed/encrypted volumes always fetch whole blocks (decode requirement).
- `--mem-budget <size>` (mount flag) / `SQUEEZEFS_MEM_BUDGET_MB`: the daemon's joint memory budget. Unset, the budget follows cgroup v2 `memory.max` × 0.8 (re-read every second — a runtime-lowered cage tightens the budget live), else 70 % of RAM. Under pressure the daemon sheds (early flushes, cache clamps, prefetch pause) instead of OOMing; watch `mem_budget_level`/`mem_budget_red_events` in `.stats`.

### Kernel cache TTLs (mount options / env; per-class)

Four kernel-cache TTL classes, each defaulting to the historical 1 s (the DAOS per-class split: directory dentries invalidate whole subtrees, so they get their own knob). Mount options are libfuse-style float seconds (`-o attr_timeout=2.5`) and win over the env knobs (milliseconds); both are per-mount. Longer TTLs widen the staleness window a single mount can observe of its own metadata — safe under the single-writer mount guard; revisit before any multi-writer future.

- `-o attr_timeout=<s>` / `SQUEEZEFS_FUSE_ATTR_TTL_MS`: GETATTR/SETATTR reply TTL + the daemon attr-cache freshness window.
- `-o entry_timeout=<s>` / `SQUEEZEFS_FUSE_ENTRY_TTL_MS`: dentry TTL for non-directory lookup/create results.
- `-o dir_entry_timeout=<s>` / `SQUEEZEFS_FUSE_DIR_ENTRY_TTL_MS`: dentry TTL for directory results.
- `-o negative_timeout=<s>` / `SQUEEZEFS_FUSE_NEGATIVE_TTL_MS`: TTL for cacheable negative lookup replies (kernel-side negative dentries — repeated misses of the same name stop paying a round trip). `0` disables negative caching (misses reply bare ENOENT).

### External mount supervisor (`mount --daemon --supervise`)

With `--supervise` the `mount --daemon` parent stays alive as an external watchdog (JuiceFS-supervisor precedent): it probes `<mountpoint>/.stats` every 5 s (`SQUEEZEFS_SUPERVISE_INTERVAL_SECS`) and, after 30 s of sustained unresponsiveness (`SQUEEZEFS_SUPERVISE_UNRESPONSIVE_SECS`), logs loudly, dumps the daemon's `/proc` state (per-task wchan + kernel stacks when root), and — when kernel callers are blocked (`waiting > 0`) and the supervisor runs as root — writes `/sys/fs/fuse/connections/<id>/abort` to release them with `ECONNABORTED`. The abort kills the mount by design; the escalation message prints the daemon PID and the exact manual recovery commands (kill-by-PID → `squeezefs umount` → remount). This complements the in-daemon op watchdog, which can log a wedge but cannot clear one.

### Format v3 (CoW KV metadata)

Metadata volumes format as **v3**: a copy-on-write, typed key/value btree (bcachefs-style 256 KiB CoW nodes + a logical reservation journal + background checkpoints). Full design: `docs/design-cow-kv-metadata.md`; measured gates: `.benchmarks/2026-07-09-kv-v3-gates.md`.

Capacity/scale: ≥ 100 M inodes per volume, 1 M+ entries per directory, unlimited xattrs (values up to `min(64 KiB, node_size/4)`), and O(active-set) mount time (a 100 M-inode volume cold-mounts in ~22 ms on the reference box).

**v3 tuning knobs** (format-time and mount-env):

- `--meta-node-kib <64|128|256|512|1024>` (format): btree node size, default `256`. Below 256 the per-volume record-value cap becomes `node_size/4` and a warning prints, spilling large xattrs / layout maps to the indirect mechanism sooner — leave at 256 unless cold-read latency on tiny-record workloads dominates.
- `--meta-journal-mb <MiB>` (format): journal ring size; default `clamp(volume/64, 8 MiB, 32 MiB)`.
- `SQUEEZEFS_META_NODE_CACHE_MB` (mount env): RAM budget for the demand-paged node cache (default `512`).
- `SQUEEZEFS_META_CHECKPOINT_MAX_DIRTY_NODES` (mount env): dirty-node checkpoint cap; bounds the mount-replay working set (default `4096`).
- `SQUEEZEFS_META_FLUSH_INTERVAL_MS` (mount env): the journal/checkpoint cadence — `0` = strict per-commit durability.

> **Legacy format v2**: support was removed entirely (always forward — no backwards compatibility). A v2 superblock refuses to mount with a precise "no longer supported; reformat required" error; `squeezefs format --force` reformats such a volume to v3 (destroying the old contents). The offline `squeezefs migrate` v2→v3 converter was deleted along with v2 support.
