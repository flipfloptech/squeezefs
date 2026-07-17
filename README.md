# Squeezefs

![SqueezeFS Header](github.jpeg)

Squeezefs is a slimmed-down, high-performance distributed POSIX FUSE filesystem (Rust + tokio + io_uring) featuring a decoupled, block-based logical volume metadata store backend (**MetaLV**) and a local or NVMe-oF block device client. Linux-only.

Designed to operate at scale (15,000+ concurrent nodes), it delivers bare-metal file throughput by leveraging asynchronous file architectures over FUSE-over-io_uring, tiered client-side caching, and direct NVMe / NVMe-oF block I/O, with zero external database dependencies.

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
Built in **Rust** using the asynchronous `tokio` runtime and `io_uring` polling over `/dev/fuse` to process OS requests efficiently. Every mount switches to high-performance **FUSE-over-io_uring** after the INIT handshake (required — the mount fails loudly if the kernel cannot arm it). Default transport geometry is the measured IOPS-parity policy: a default mount sustains the **300–320 k device-true rand-4k IOPS class with zero knobs** on the reference substrate (`.benchmarks/2026-07-15-l1-transport-concurrency.md`).

### 2. Progressive Data Layout & I/O Routing
Writes are dynamically routed based on file sizes to optimize storage overhead and network latency:
- **Inline Files (< 4KB):** Inlined directly in the Metadata Volume's inodes/attributes.
- **Staged Files (4KB - 4MB):** Staged locally on NVMe cache and asynchronously merged into physical blocks flushed to the main NVMe device.
- **Striped Files (> 4MB):** Sliced into 4MB blocks and written directly to the target NVMe block devices.

### 3. Distributed Lock Manager (DLM) & Consistency
Translates POSIX FUSE locks to cluster-wide leases on the metadata backend, protected by heartbeat limits and monotonic fencing tokens to prevent split-brain write conflicts. Every write mount additionally claims its metadata volumes under the [single-writer mount guard](#single-writer-mount-guard-guarantee-classes) (flock + NVMe Persistent Reservations where supported).

### 4. Tiered Caching & Zero-Copy Paths
- **Tier 1 (GPU Direct Storage - GDS):** Routes RDMA transfers directly from NVMe to VRAM, bypassing the host CPU/RAM.
- **Tier 2 (Unified System RAM):** Clock/LRU caches dynamically sizing to system memory limits.
- **Tier 3 (Local NVMe Staging):** Staging directory (`.staging`) for async writes and local caching of read blocks to avoid RTT latency.
- **Zero-copy write path:** large sequential writes travel kernel → transport payload lease → one merge copy → io_uring DMA. Blocks whose accumulated written coverage is complete upload directly (**write-through**), skipping the staging round-trip entirely — the trigger is the coverage *union*, so kernel-split out-of-order parallel O_DIRECT writes stay on the fast path; FUSE_WRITE payloads ride zero-copy leases over the registered FUSE-over-io_uring buffers. Measured on the committed reference profile: large-seq writes went from 430–512 MiB/s to ~1.8 GB/s (**≥ 3.5×**) with small-write, read, and metadata rows at-or-better — see `docs/design-zero-copy-write-path.md` and `.benchmarks/2026-07-08-zero-copy-write-path-closing.md`.
- **Random-small-write path:** aligned small overwrites of exclusively-owned striped blocks are **one in-place sub-block DMA** (no read-modify-write, no metadata commit); everything else rides a byte-budgeted extent overlay with batched folds — see [Random-small-write path](#random-small-write-path-sole-owner-patch--extent-overlay-design-docsdesign-random-small-writesmd).

### 5. Transparent Compression & Encryption
Optional per-volume transforms declared at format: `--compression lz4|zstd` and `--encrypt-algo aes256gcm-rsa|chacha20-rsa` (RSA-wrapped symmetric keys via `--encrypt-key`), applied across all three write layouts. Compression is **best-effort per block**: an incompressible block is stored raw (frame-flagged, counted as `compress_stored_raw` in `.stats`) instead of expanding — and transformed volumes reserve per-chunk headroom at format so worst-case images always fit (see [Breaking changes](#breaking-changes--migration-notes)).

### 6. Built-in HPC Auto-Tuning
Includes built-in host auto-tuning (`squeezefs tune`) to optimize virtual memory dirty page ratios (40/10), network socket buffer maxima (64 MiB), and live FUSE connection limits (`max_background`/`congestion_threshold` to the 256/192 policy ceiling, `read_ahead_kb` to 0). See [Kernel Tuning](QUICKSTART.md#5-kernel-tuning-for-bare-metal-auto-tune).

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
  - `--block-size <bytes>`: Block size in bytes (e.g. `4M`, `1M`, default: `4M`). On compressed/encrypted volumes the effective block size is clamped so a worst-case (incompressible) stored image plus headroom fits its allocator chunk — the clamp prints loudly.
  - `--capacity <bytes>`: Formatted capacity (default: the summed physical size of the data volumes). May be **lower** than physical (useful for testing); values above physical are refused — thin-provision underneath via LVM/fabric instead.
  - `--inodes <count>`: Hard quota limit for number of inodes (default: `1000000`).
  - `--disk-cache-paths <paths>`: Comma-separated paths to NVMe cache staging directories. **Declared here, at format** — recorded in the format config as the single source of truth. Omit it and the filesystem is **permanently cache-less**: mounts run with RAM tiers + direct block I/O only (no NVMe staging/read-cache tier). Change later with `squeezefs config set-cache-paths`.
  - `--compression <lz4|zstd|none>` / `--encrypt-algo <aes256gcm-rsa|chacha20-rsa|none>` / `--encrypt-key <pem>`: transparent per-volume compression / client-side encryption (see [Key Features §5](#5-transparent-compression--encryption)).
  - `--mem-cache-size` / `--disk-cache-size` / `--{read,write}-cache-size` / `--{read,write}-mem-cache-size`: cache budget defaults recorded in the format config (overridable per mount).
  - `-f, --force`: Force formatting even if a squeezefs volume is already detected (this is also the reformat path for refused legacy volumes — destroys old contents).
  - `--full`: Performs full block-aligned zero-wiping of the backing device capacity with a progress bar (default is quick-format).
  - `--meta-node-kib <64|128|256|512|1024>`: v3 metadata btree node size in KiB (default `256`). Below `256` prints a warning — the per-volume record-value cap drops to `node_size/4`, so large xattrs / layout maps spill to the indirect mechanism sooner.
  - `--meta-journal-mb <MiB>`: v3 metadata journal ring size, overriding the default `clamp(volume/64, 8 MiB, 32 MiB)`.

* **Mount Squeezefs:**
  ```bash
  squeezefs mount sqmeta://<meta_dev> [sqmeta://...] <mountpoint> [options]
  ```
  Cache/staging paths come from the format config; passing `--disk-cache-paths` at mount is a loud error (use `squeezefs config set-cache-paths` to change them).
  *Options (operator-relevant subset; `squeezefs mount --help` is authoritative):*
  - `--daemon`: Run FUSE daemon in the background (changes its working directory to `/` to avoid locking paths).
  - `--supervise` (requires `--daemon`): keep the parent alive as an external mount watchdog — see [External mount supervisor](#external-mount-supervisor-mount---daemon---supervise).
  - `--allow-other` (alias `--allow-others`): Allow other users/root to access the mount (required for `sudo umount`).
  - `--log-file <path>`: Path to write daemon logs to when running in background.
  - `--mem-budget <size>`: the daemon's joint memory budget (shed-don't-OOM authority) — see [Hybrid I/O](#hybrid-io-for-o_direct-reads-default-and-the-device-true-escape).
  - `--mem-cache-size <size>`: System RAM cache size (e.g. `16GB` or `20%`); the other cache-size family flags override the format-config defaults the same way.
  - `--uid <uid>` / `--gid <gid>`: presented owner of files in the mount (presentation-only; staging I/O runs as the mounting user).
  - `-o <opts>`: FUSE options, including the per-class kernel TTLs (`attr_timeout`, `entry_timeout`, `dir_entry_timeout`, `negative_timeout`), `max_background` / `congestion_threshold` INIT overrides, and `direct_device_true` — each documented in its section below.
  - `--no-writeback`: disable the FUSE writeback cache (enabled by default).
  - `--write-verification` (+ `--write-verification-sample <N>`): opt-in read-after-write checksum verification.
  - `--dismount-wait <secs>` / `--upload-delay <dur>`: staging drain window on dismount / background upload cadence.

* **Change cache/staging directories (admin op):**
  Guarded like `format` (refused while any client has the volume mounted); rewrites the format config and wipes the new directories so the next mount stamps a fresh staging generation.
  ```bash
  squeezefs config set-cache-paths sqmeta://<meta_dev> <path> [<path>...]
  squeezefs config get-cache-paths sqmeta://<meta_dev>
  ```

* **Show filesystem Status:**
  ```bash
  squeezefs status [sqmeta://<meta_dev> | <mountpoint>]   # config + volume summary (JSON)
  ```
  The report's `"Clients"` array carries the volume's real mount registrations (same records and classification as `squeezefs clients` below).
  A mounted filesystem also exposes live daemon metrics as JSON on the virtual **`.stats`** inode at the mount root (`cat <mountpoint>/.stats`) — the preferred live regression signal (layout mix, cache/tier counters, `meta_kv_*`, `writer_guard_*`, transport geometry, patch/fold ledgers, memory-budget level).

* **List client mount registrations:**
  Serves the `client:{id}` heartbeat records and the single-writer `writer_claim` recorded on the volume set's root inos — the same records the format preflight and the mount guard consume, under the same staleness law. Read-only probe: works beside a live mount and never perturbs it. States: `live` (fresh heartbeat), `stale` (heartbeat older than the 45 s TTL — crashed or partitioned holder), `dead` (writer claim whose same-host pid is provably gone — reclaimable immediately, no TTL wait).
  ```bash
  squeezefs clients sqmeta://<meta_dev> [--json]
  ```
  ```text
  KIND    ID                                     PID      STATE  AGE   VOLUME
  client  0d3179c8-6a02-4f45-9c11-0c8ad6a0a1b2   731022   live   4s    /dev/xai-meta/mds01
  writer  9c41c2e6-6a4e-4bfb-b41c-2fb1b1f2b7aa   731022   live   4s    /dev/xai-meta/mds01
  2 registration(s): 2 live, 0 stale, 0 dead (reclaimable).
  ```

* **Show space/inode usage (offline/URI query):**
  Answers from the same authoritative sources as the mounted daemon's statfs — formatted capacity/quotas from the format config, allocator-tracked striped-block usage (rebuilt by the same live-inode-tree walk a mount runs), and the v3 monotonic inode watermark — via read-only probes: **no live mount required**, and beside one it reports the durable point-in-time state. Aggregate plus per-volume rows (data volumes: size/allocated; meta volumes: KV heap size/free, next-ino).
  ```bash
  squeezefs df -g sqmeta://<meta_dev> [--json]
  ```
  ```text
  SqueezeFS 'squeezefs' — offline query over 1 meta / 1 data volume(s), durable state
  Data:   capacity 8.00 GiB   used 64.00 MiB (0.8%)   free 7.94 GiB
  Inodes: quota 1000000   used 2   free 999998
  ```
  A **mounted** filesystem also answers plain `df -h <mountpoint>` from the OS (see [`df` / statfs semantics](#df--statfs-semantics)).

* **Clear a stale writer claim (recovery verb):**
  Operator-attested removal of a stale single-writer claim after a cross-host crash on a volume without NVMe Persistent Reservations — see the [recovery runbook](#single-writer-mount-guard-guarantee-classes). Refuses fresh claims and live-mounted volumes.
  ```bash
  squeezefs claim clear sqmeta://<meta_dev>
  ```

* **Unmount Squeezefs:**
  Safely unmounts SqueezeFS by waiting for staging caches to flush before tearing down FUSE.
  ```bash
  squeezefs umount <mountpoint> [--force]
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
  Share and dismantle NVMe-oF targets, and install/configure user-space SPDK via `squeezefs storage nvmeof` (SPDK verbs: `spdk-install`, `spdk-setup`, `spdk-bind`, `spdk-unbind`, `spdk-start` — see [QUICKSTART §4](QUICKSTART.md#4-nvme-of-fabric-setup-remote-block-storage)).
  ```bash
  squeezefs storage nvmeof share <path> [--spdk] [--port <port>] [--ip <ip>]
  squeezefs storage nvmeof connect --ip <ip> --subnqn <nqn> [--port <port>]
  squeezefs storage nvmeof disconnect <nqn>
  squeezefs storage nvmeof unshare <nqn> [--spdk]
  squeezefs storage nvmeof list
  squeezefs storage nvmeof restore-shares   # re-register persistent target shares
  ```

* **Storage pools & volumes (LVM):**
  ```bash
  squeezefs storage pool create <name> <disks...>     # + add/remove/delete/list
  squeezefs storage volume create <pool> <name> --size <sz>   # + extend/delete/list
  ```

---

## Quick Start & Verification

To get up and running quickly or deploy directly onto physical bare-metal hardware over NVMe-oF, see the [QUICKSTART.md](QUICKSTART.md) guide. Dev boxes without spare raw NVMe should use the one-command virtual NVMe substrate (`sudo tests/dev_substrate.sh create` — QUICKSTART §2).

## Performance snapshot (measured, citations)

Every number traces to a committed `.benchmarks/` note (box/substrate/method inside each). Headline classes on the reference box:

| Axis | Measured class | Evidence |
|---|---|---|
| rand-4k O_DIRECT read IOPS, **default mount, device-true** | **300–320 k** (zero knobs; 11.4× the pre-L1 stock posture) | `.benchmarks/2026-07-15-iops-parity-decomposition.md` (44 k → 316 k), `.benchmarks/2026-07-15-l1-transport-concurrency.md` |
| rand-4k O_DIRECT read IOPS, tier-resident (hybrid warm) | **~536–558 k** steady-state, zero device traffic | `.benchmarks/2026-07-15-hybrid-io.md`, reconfirmed `.benchmarks/2026-07-17-rand-write-program-closing.md` §3b |
| rand-4k write IOPS (sole-owner patch shape) | **59–67 k** (was 354–397 pre-program; device cost 4 KiB-class/op vs ~12 MiB/op) | `.benchmarks/2026-07-17-rand-write-program-closing.md` |
| Large sequential write | ~1.8 GB/s zero-copy write-through (≥ 3.5× pre-program); **4.4–4.6 GiB/s device-true** during scoreboard seq rows | `.benchmarks/2026-07-08-zero-copy-write-path-closing.md`, `2026-07-17-rand-write-program-closing.md` §2 |
| Metadata: create / entries-per-op | one-dir creates +44–62 % (110 µs/op serial wall), many-dirs 32.7 k/s; rename/unlink ≈ **1.0 journal entries/op** | `.benchmarks/2026-07-15-metadata-throughput-closing.md` |
| Mount time at scale | 100 M-inode volume cold-mounts in ~22 ms | `.benchmarks/2026-07-09-kv-v3-gates.md` |
| Crash contract soaks | kill-9 acked-loss **0** across 10/10 SMO soak rounds + 100/100 journal kill soaks; torn-write drops 0 | `.benchmarks/2026-07-17-rand-write-program-closing.md` §7, `2026-07-15-metadata-throughput-closing.md` G6 |

### The vs-JuiceFS scoreboard (release gate)

`tests/run_vs_juicefs.sh` is the standing **"beat-JuiceFS" scoreboard**: a re-runnable, matched-conditions A/B harness that measures SqueezeFS against JuiceFS on the same substrate (both durable stores in one non-tmpfs directory), matched cache budgets (one `SQUEEZEFS_VS_CACHE_MB` knob drives our `--mem-budget` and their `--cache-size`/`--buffer-size`), identical elbencho drivers, across **three regimes** — R1 as-deployed (all cache layers live, dataset 2–4× cache), R2 device-true (their `--cache-size 0` + tight cage, our `-o direct_device_true`, both **verified by counters per row**), R3 cold-cache (full drops, first pass) — × the 6-shape workload grid (seq write/read 1 MiB, rand read/write 4k at `t16 iodepth16 --direct`, stat storm, del storm).

```bash
tests/run_vs_juicefs.sh                      # full scoreboard (~30–60 min, quiet-gated)
SQUEEZEFS_VS_SMOKE=1 tests/run_vs_juicefs.sh # 30s-class micro-grid plumbing proof (per-commit tier)
```

**The gate:** the run emits a win/loss table (+ machine TSV + per-row raw logs, counter snapshots, and diskstats evidence) and **exits nonzero if SqueezeFS loses any row** (loss = < 0.95× JuiceFS; INVALID/unverified rows count as losses). `SQUEEZEFS_VS_ALLOW_LOSS="R1.foo,..."` exempts named rows for known-loss tracking — every allowed loss must have an attribution + follow-up in the current `.benchmarks` scoreboard report. Cadence: **per-release** (with the acceptance suites) and after any perf-relevant landing.

**Current standing (closing run, 2026-07-17, `.benchmarks/2026-07-17-rand-write-program-closing.md` §2): 13 W / 3 TIE across the 18 rows, gate GREEN** with the allowlist shrunk to exactly two rows (`R1.seq_write_1m,R3.seq_write_1m`). Highlights: rand-write 12.6–15.9× JuiceFS (the scoreboard's former only genuine loss, closed by the random-small-write program), rand-read 1.9–3.5×, stat 2.8×, del 9.2–11.2×, seq-read TIE. The two allowed rows are an **ACK-semantics measurement artifact, not a product loss**: on those page-cache-drain seq-write rows JuiceFS acks from RAM (its device drains 2.1–3.2 GiB/s) while SqueezeFS puts 4.4–4.6 GiB/s on the device during the row — the honest device-true comparison is regime R2's seq-write, which SqueezeFS **wins 2.11×**. Inaugural baseline (8 W / 3 TIE / 5 L / 2 INVALID): `.benchmarks/2026-07-15-vs-juicefs-scoreboard.md`.

### `df` / statfs semantics

A mounted SqueezeFS reports honest, cheap numbers to `statfs(2)` (`df`): **total** is the formatted capacity — the summed data-backend size, or the lower explicit `--capacity` quota chosen at format (the effective limit you experience); **used/free** track the bytes currently allocated on the striped block backends, maintained by the block allocators at alloc/free time (no metadata transactions or device I/O on the statfs path). Tiny inline payloads live in the metadata volume and staged-but-unpromoted small writes in the local NVMe staging dirs, so those transient bytes appear in `df` as their blocks promote via writeback rather than instantaneously; deletes return space after background reclaim completes. Inode columns (`df -i`) report the format inode quota against the v3 monotonic, no-reuse inode watermark — `IFree` is remaining create headroom, and deleting files does not raise it.

The **`squeezefs df`** verb answers the same accounting **offline** — read-only probes over the volume set, no mount required (`squeezefs df -g sqmeta://<meta_dev> [--json]`; see the command list above). It reports the durable point-in-time state: beside a live mount, bytes still in flight through staging/journal deferral appear once durable.

## Breaking changes & migration notes

SqueezeFS moves **always forward** — no backwards compatibility. Refusals are loud, name their cause, and state the remedy. Current refusal classes an operator can hit:

> **⚠️ Legacy metadata format v2 — removed.** A v2 superblock refuses to mount with *"no longer supported; reformat required"*. Reformat to v3 with `squeezefs format --force` (destroys the old contents). The offline `squeezefs migrate` v2→v3 converter was deleted along with v2 support.

> **⚠️ Pre-watermark v3 volumes — refused (REFORMAT REQUIRED).** v3 volumes formatted before the node-seq mint watermark (the Finding-A KV-corruption fix era) fail the superblock feature gate: *"pre-watermark v3 volume: formatted before the node-seq mint watermark (Finding A) and no longer supported; reformat required"*. Volumes carrying **unknown** incompat bits (formatted by a newer binary) also refuse, naming the bits — upgrade squeezefs instead.

> **⚠️ Pre-fix compressed/encrypted volumes — refused (REFORMAT REQUIRED).** Volumes formatted with `--compression`/`--encrypt-algo` before the FIND-RW4-A incompressible-block fix cannot hold worst-case stored images; mounts refuse with *"compressed/encrypted volume geometry cannot hold incompressible blocks (FIND-RW4-A) … refusing to mount"* (full-size incompressible blocks on such volumes were never readable — the refusal names the fix). Reformat with a current binary: `format` now reserves per-chunk headroom on transformed volumes (clamping the block size loudly when needed), and compression became **best-effort per block** — incompressible blocks are stored raw (`compress_stored_raw` counts them in `.stats`).

> **⚠️ Staging directories are generation-bound.** Staging/cache dirs are stamped with the filesystem generation (the v3 superblock uuid set). A mount that finds staged content from a **dead generation** (e.g. after a reformat over live staging dirs) wipes it with one loud `STAGING GENERATION MISMATCH` line and counts `staging_generation_discards` in `.stats` — staged writes stamped by the old generation are gone **by design** (reformat discards data).

> **⚠️ Cache/staging paths are format-declared.** `mount --disk-cache-paths` is refused loudly (never silently ignored). Change paths with the admin op `squeezefs config set-cache-paths <sqmeta-uri> <paths...>` (guarded like `format`: refused while any client has the volume mounted; the new dirs are wiped so the next mount stamps a fresh staging generation). Read them back with `config get-cache-paths`. A filesystem formatted without `--disk-cache-paths` is **permanently cache-less**.

> **Removed flags/verbs** (kept here so stale scripts fail comprehensibly): `--strict-meta-atomicity` (only ever gated v2 volumes; deleted with them), `squeezefs migrate` (deleted with v2), `mount --local-ips` (the socket-level multi-rail bonding was removed in the 2026-07-04 connection simplification — fabric multipath is the kernel NVMe initiator's domain), `mount --disk-cache-paths` (see above), `squeezefs defrag` (removed 2026-07-17: the verb's engine was an unimplemented no-op that reported fake success — no fake surfaces; the jobs-layer `BlockMove` merge machinery it would drive remains, test-pinned, awaiting a real defrag program).

## Metadata Durability (crash contract)

SqueezeFS metadata is **format v3** (CoW KV) — the only supported metadata format (v2 support was removed; v2 volumes refuse to mount with "no longer supported; reformat required"). Its crash contract holds **by construction** (design: `docs/design-cow-kv-metadata.md`; the historical D0/D1/D2 ladder it strictly strengthens is `docs/design-wal-crash-consistency.md` §3):

- **Every on-disk unit is checksummed** — superblock, journal pages and entries, btree nodes, bsets, the allocator bitmap, and root-ledger slots.
- **Torn writes are detected and ignored, never applied.** A torn journal entry, node append, or ledger slot fails its checksum and the last consistent state serves (the old copy-on-write node / the predecessor ledger record). Nothing overwrites live data in place.
- **Whole-transaction atomicity**: one transaction = one checksummed journal entry, replayed all-or-nothing at mount. A transaction is never visible half-applied.
- **No hardware-atomicity dependency**: a file-backed volume gets the same integrity guarantee as an atomic-4KiB device. The sector-atomicity probe still runs, purely informationally, and reports as `meta_volume_atomicity_physical` on the `.stats` inode (`atomic4k` / `likely` / `unknown` / `file-backed`); the contract field `meta_volume_atomicity` reads `cow-checksummed`. (The old `--strict-meta-atomicity` mount gate only ever gated v2 volumes and was deleted with them.)

**Acked durability** (`fsync`/`fsyncdir` returning success) is carried solely by post-apply coalesced `fdatasync` barriers — exactly one physical barrier per fsync.

- `SQUEEZEFS_META_FLUSH_INTERVAL_MS`: deferred metadata durability window in ms (default `50`); `0` = strict sync-on-commit — every metadata commit returns only after a post-apply device barrier. Legacy alias `SQUEEZEFS_JOURNAL_FLUSH_INTERVAL_MS` is honored; the new name wins if both are set.
- `SQUEEZEFS_RECLAIM_BATCH`: inode-reclaim group-commit batch size (default `64`, clamp 1–1024).
- `SQUEEZEFS_META_COMMIT_BATCH_TXS` / `SQUEEZEFS_META_COMMIT_BATCH_BYTES`: per-volume commit-conveyor batch caps (defaults `64` transactions / `256 KiB`; bytes are clamped to the journal ring's admissible capacity). Group commit batches admission, locking, the journal write, and the barrier across concurrent transactions — **never the atomicity unit**: one transaction stays one checksummed journal entry (design `docs/design-metadata-throughput.md` §5.5). Watch `meta_commit_group_size` on the `.stats` inode; a strict-mode median ≈ 1 under concurrent writers means batching regressed.
- `SQUEEZEFS_OP_PROFILE=1`: per-op FUSE phase histograms (`fuse_op_phase_ns`, `fuse_create_under_lock_ns`) on the `.stats` inode — diagnostics for metadata-latency attribution. Off by default; zero per-op cost when off.

### Single-writer mount guard (guarantee classes)

The v3 metadata engine is single-writer by construction, and the mount enforces it: every **write** mount claims each metadata volume with (a) a dedicated daemon-lifetime `flock` (same-host exclusivity; the kernel releases it instantly on process death), (b) an **NVMe Persistent Reservation** (Write Exclusive) where the namespace advertises reservation support — cross-host *enforcement*: the device itself rejects a fenced or stale holder's writes — and (c) a `writer_claim` heartbeat record (identity + detection on every substrate). A second concurrent mount is **refused loudly, naming the holder**. There is **no bypass flag**; read-only probes (`status`, format preflight) are never blocked. Design: `docs/design-metadata-throughput.md` §5.0. What the guard guarantees depends on the substrate:

| Substrate | Guarantee |
|---|---|
| Same host, any volume | **Refusal-grade** (flock on a dedicated fd; kernel-enforced; instant crash reclaim; SIGSTOP-safe) |
| NVMe / NVMe-oF namespace with `RESCAP` PR support | **Enforcement-grade** (Write-Exclusive reservation: the device rejects a fenced/stale holder's writes; acquire arbitrates simultaneous mounts; automatic TTL-stale preemption is safe). **Fencing detection latency ≤ one flush cadence + one barrier** (50 ms default; immediate in strict/fsync — Issue 14); PTPL-lapse residual ≤ 10 s (heartbeat report re-check, §5.0 B1 pt 6) |
| — SPDK-served namespace (`storage nvmeof share --spdk`) | PR support exists in SPDK's nvmf target — **probe decides the row above vs below**; validated in OQ 4's scope (SPDK differs from kernel-nvmet) |
| — loop-device-backed nvmet namespace (the repo's own file-backed share path, `losetup` wrap) | loop devices expose no PR ⇒ lands in the **"block without PR"** row below — named explicitly because the repo's own tooling creates this shape |
| Block volume **without** PR support | **Detection-grade**: mounts separated by > ~1 heartbeat are refused; near-simultaneous mounts can both arm; a paused holder cannot detect usurpation — therefore automatic cross-host takeover is disabled (operator-attested `claim clear` only) |
| File-backed volume shared cross-host (NFS et al.), or containers with private `/dev` nodes | **Unsupported for concurrent-mount protection** — single-host operation of such volumes remains fully guarded by flock (former) / PR-if-available (latter) |

**Recovery runbook**, in order of automation — the refusal message always names the holder (`{id, pid, boot, age}`) and the exact remedy:

1. **Same-host crash**: nothing to do — the flock died with the process, and a dead-pid-proven claim (same boot, `kill(pid,0)` = ESRCH) is reclaimed automatically and instantly.
2. **PR-capable volumes**: a TTL-stale holder (> 45 s without heartbeat) is **preempted automatically** at the device; a fresh holder refuses loudly.
3. **Non-PR volumes after a cross-host crash**: automatic takeover is deliberately disabled (a paused holder cannot detect usurpation). Verify the named holder is truly dead, then clear the stale claim by operator attestation:

   ```bash
   squeezefs claim clear sqmeta://<meta_dev>
   ```

   The verb probe-mounts read-only, re-verifies staleness (refusing a fresh claim), and removes the record — the same live-check style as the format preflight.

Live signals on the `.stats` inode: `writer_guard_mode` per volume (`flock+pr` = enforcement-grade | `flock+claim` = detection-grade | `flock` = read-only mount) — alert on fleet drift; `writer_guard_fenced` (a fenced/usurped holder fail-stopped — working as designed, always investigate); `writer_guard_pr_reacquires` (the target dropped reservations, e.g. a PTPL-less power cycle — audit the fabric).

### Read-path tuning (mount env; design `docs/design-read-path.md`)

Defaults are the measured sweet spot — override only with a live-counter reason (the `.stats` inode exposes every family):

- `SQUEEZEFS_READ_TIER_ADMISSION` (`second-touch` default | `always` | `never`): NVMe read-tier admission for >256 KiB fills. `second-touch` kills the streaming publish tax (a cold 16 GiB pass writes ~0 instead of ~16.9 GiB to the tier) while re-read heat still converges to the tier; `always` restores unconditional first-touch publishes (A/B escape hatch).
- `SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB`: RAM hot-block tier budget for >256 KiB blocks (default derived from the read-mem cache; `0` disables the tier and admission auto-degrades to `always`).
- `SQUEEZEFS_READ_PREFETCH_WINDOW` (default `16`, `0` disables): per-stream prefetch pipeline depth cap in blocks. The window is adaptive (2→cap, AIMD) and contention-scaled; the cap is a ceiling, not a target.
- `SQUEEZEFS_READ_PREFETCH_SHARE_PCT` (default `50`): the prefetch pipeline's share of the hot-tier budget in the contention-scaling formula — lower it if concurrent stream count routinely exceeds hot-tier capacity.
- `SQUEEZEFS_READ_RANGED_THRESHOLD` (default `262144`, `0` disables): reads at or under this size on passthrough (uncompressed/unencrypted) volumes fetch only their 4 KiB-aligned device window instead of the whole block — the rand-4k amplification kill (≈1000× → ~1.0×). Compressed/encrypted volumes always fetch whole blocks (decode requirement).

### Hybrid I/O for O_DIRECT reads (default) and the device-true escape

**Hybrid I/O (default, user directive 2026-07-15):** O_DIRECT reads get the best of both worlds — they keep bypassing the *kernel page cache* (the kernel's side of O_DIRECT, unchanged) while serving from and admitting into *SqueezeFS's own read tiers* exactly like buffered reads. Tier hits serve from RAM (binding-validated — the ~536–558 k IOPS class on tier-resident data, `.benchmarks/2026-07-15-hybrid-io.md` + the RW5 close §3b); misses use **evidence-based admission** — first touch of a block reads the device (device-true, nothing admitted: streaming/scan pollution protection), a **second touch within the ghost window** admits the block (one whole-block fetch → RAM hot tier + NVMe read tier), so re-read-heavy O_DIRECT workloads (rand-4k databases, repeated scans) converge to RAM speed after one warm-up pass. Admission pauses under memory-budget Red. Watch `read_odirect_tier_serves` / `ranged_read_ghost_escalations` in `.stats`.

- **`-o direct_device_true`** (mount option) / **`SQUEEZEFS_DIRECT_DEVICE_TRUE=1`** (daemon env): the **measurement/diagnostic escape** — O_DIRECT reads become strictly device-true (no tier serve, no admission, no ghost recording, no prefetch classification; every O_DIRECT read is a validated device read of exactly its aligned window). This is the posture for device-path benchmarking and the `.benchmarks` amplification methodology (`squeezefs bench --direct` prints which posture the mount carries by sniffing `.stats`). Buffered traffic on the same mount keeps full hybrid behavior. Mode visible as `"direct_device_true"` in `.stats`; adoption counted by `read_device_true_reads`.
- `--mem-budget <size>` (mount flag) / `SQUEEZEFS_MEM_BUDGET_MB`: the daemon's joint memory budget. Unset, the budget follows cgroup v2 `memory.max` × 0.8 (re-read every second — a runtime-lowered cage tightens the budget live), else 70 % of RAM. Under pressure the daemon sheds (early flushes, cache clamps, prefetch pause) instead of OOMing; watch `mem_budget_level`/`mem_budget_red_events` in `.stats`.

### Random-small-write path (sole-owner patch + extent overlay; design `docs/design-random-small-writes.md`)

Small random overwrites of striped files no longer pay a whole-block read-modify-write. Two levers, both default-on (program Implemented 2026-07; closing evidence `.benchmarks/2026-07-17-rand-write-program-closing.md`):

- **Sole-owner extent patch (W1)**: an isolated, LBA-aligned, non-extending small write to an exclusively-owned, passthrough, whole-block-mapped striped block becomes **one in-place sub-block DMA** — zero reads, zero metadata commits, zero staging (354–397 → 61–67 k IOPS on the 4 KiB random-write shape; device amplification ~1× writes). Sequential streams are predicate-excluded (adjacency guard) and keep the whole-block write-through economy.
- **Extent overlay + batched fold (W2)**: patch-ineligible shapes (compressed/encrypted volumes, refcount-shared blocks post-clone, holes, unaligned) park 4 KiB-class extents instead of 4 MiB buffers, spill as checksummed staging *extent records* (never a seed read at spill), and fold into blocks lazily — compressed-volume rand-write amplification ~2,500× → 15–26×.
- **Torn-extent durability note (the v1 aligned-only contract)**: a patch rewrites **only device sectors wholly inside the application's own write range** — bytes the application never wrote are never rewritten, so a crash can never perturb foreign data. The residual exposure is a per-sector old/new mix *strictly inside an un-fsynced in-flight write* (POSIX-legal; fsync acks only after DMA completion — the write ACK on this shape is *stronger* than before, since data reaches the device before ACK instead of a parked buffer).
- Knobs (acceptance/diagnostic, not operational escape hatches): `SQUEEZEFS_PATCH_MAX_BYTES` (default 512 KiB; `0` disables the patch path — A/B lever), `SQUEEZEFS_FOLD_MAX_EXTENTS` / `SQUEEZEFS_FOLD_MAX_BYTES` (fold triggers, default 64 / 1 MiB).
- Watch in `.stats`: `patch_writes` ≈ ops on the patch shape (`patch_ineligible_*` growing there = predicate rot), `patch_edge_rmw_reads` **must stay 0**, `fold_fill` median ≥ 16, `extent_records_{recovered,torn_discarded,future_refused}` on recovery.

### FUSE transport in-flight concurrency (defaults are the L1 policy; knobs are overrides)

Random-4k iodepth workloads are gated by two multiplicative kernel-side limits: the FUSE-over-io_uring per-queue ring depth and the INIT-negotiated `max_background`. Opening both measured **44k → 316k IOPS (7.2×, device-true)** on `elbencho --rand -t 16 -b 4k --iodepth 16 --direct` (`.benchmarks/2026-07-15-iops-parity-decomposition.md`); since L1 that class is the **default** — no knobs required:

- **Per-queue depth** (`SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH`, clamp 1..32): default is **32 degraded to the payload-buffer cap** `min(mem-budget/8, 2 GiB)` with floor 4 (the pre-L1 posture — small-RAM boxes keep yesterday's footprint). An explicit value wins verbatim over the cap. Payload arenas cost `queues × depth × ~1 MiB` of registered anon memory — gauged as `transport_payload_buffer_bytes` in `.stats` and attributed to the memory budget as the `transport_payload_buffers` component.
- **Queues** (`SQUEEZEFS_FUSE_OVER_IO_URING_QUEUES`, testing only): pinned to kernel **possible CPUs** — registering fewer never becomes ready (kernel readiness requirement).
- **`-o max_background=N` / `-o congestion_threshold=N`**: INIT-reply overrides; defaults `clamp(queues × depth, 64, 256)` and ¾ of it. Also runtime-writable per live connection via fusectl: `echo 256 | sudo tee /sys/fs/fuse/connections/<minor>/max_background`.
- **FIND-L1-A (≥ 13-writer O_DIRECT convoy): FIXED 2026-07-17.** The convoy was a write-path completion-trigger defect (one write's end as a proxy for block completeness — kernel-split out-of-order WRITE segments misfired it), not a transport trade; the coverage-union trigger cured it (`t16` default/mb12 = 1.006–1.026, t16 ≥ 1.10× t8, both cells *rose*). No `max_background` throttle is needed or recommended anymore. Forensics + fix: `.benchmarks/2026-07-17-rw3-find-l1a-forensics.md`, `.benchmarks/2026-07-17-rw3b-write-through-coverage-fix.md`.

### FUSE io_uring SQPOLL (mount env; measured — leave unset)

- `SQUEEZEFS_FUSE_IO_URING_SQPOLL_IDLE_MS` (default unset = **off**): opts every FUSE-side io_uring into kernel submission-queue polling with the given idle timeout — the classical `/dev/fuse` INIT/notify/sideband rings (one poller each) **and** the FUSE-over-io_uring queue rings, which share **one** poller for all queues (qid 0 creates it, the rest attach via `IORING_SETUP_ATTACH_WQ`; a kernel that declines SQPOLL degrades loudly to plain rings, never failing the mount).
- `SQUEEZEFS_FUSE_IO_URING_SQPOLL_CPU`: pin that one queue-ring poller (`IORING_SETUP_SQ_AFF`; the leader's pin governs the shared group — per-queue pins do not exist by design).
- **Measured posture (2026-07-15, `.benchmarks/2026-07-15-m10-sqpoll.md`, resolves design OQ 3): not recommended — including for dedicated metadata-heavy nodes.** On the post-M3 transport (one `io_uring_enter` already carries commit+wait), SQPOLL-on measured **+25 % enters/create** (wake-cycle fragmentation; 9.10 → 11.40), **one full core burned by the poller under storm** (idle mounts burn 0.0 % — the idle timeout parks it), and **flat-to-worse paired mdstorm rows** (−1.5 % create … −12.9 % many-dirs unlink) at byte-identical op shape. Consider only on boxes with uncontended spare cores, and only if a live profile of *your* workload (strace `io_uring_enter` counts + `iou-sqp` thread CPU, the M10 method) proves it out.

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
