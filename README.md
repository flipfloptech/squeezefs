# SqueezeFS

![SqueezeFS Header](github.jpeg)

SqueezeFS is a high-performance distributed POSIX filesystem for Linux, built in Rust on `tokio` and `io_uring`. It decouples metadata from data: file metadata lives on dedicated block-based metadata volumes (**MetaLV**, a copy-on-write key/value store), while file contents are striped across local NVMe or NVMe-oF block devices. A FUSE client daemon exposes the filesystem — with no external database or coordination service — and the design targets clusters of 15,000+ concurrent nodes at bare-metal throughput.

## Key properties

- **FUSE-over-io_uring transport.** Every mount arms the kernel's FUSE-over-io_uring request path after the INIT handshake — required, not optional; the mount fails loudly if the kernel cannot arm it.
- **Crash-safe metadata by construction.** The v3 metadata format is a copy-on-write, checksummed key/value btree: whole-transaction atomicity and torn-write immunity on any substrate, including plain files. Scales to ≥ 100 M inodes and 1 M+ entries per directory per volume.
- **Zero-copy write path.** Large writes travel kernel → transport payload lease → one merge copy → io_uring DMA; fully written blocks upload directly to the block backend, skipping staging entirely.
- **Efficient random small writes.** Aligned small overwrites of exclusively owned blocks become one in-place sub-block DMA — no read-modify-write, no metadata commit; other shapes ride a byte-budgeted extent overlay with batched folds.
- **Progressive data layouts.** Files grow from inline (< 4 KiB, metadata-resident) through locally staged (≤ 4 MiB, NVMe staging) to striped 4 MiB blocks, promoted durably as they cross thresholds.
- **Tiered caching with hybrid O_DIRECT.** Optional GPU Direct Storage, RAM clock/LRU tiers, and NVMe staging/read cache. O_DIRECT reads bypass the kernel page cache but still serve from SqueezeFS's own tiers; a mount option restores strictly device-true O_DIRECT for measurement.
- **Distributed lock manager.** POSIX locks map to cluster leases with heartbeat renewal and monotonic fencing tokens; stale writers are fenced, never trusted.
- **Single-writer mount guard.** Write mounts claim their metadata volumes via `flock`, NVMe Persistent Reservations where the device supports them, and heartbeat claim records — a second concurrent mount is refused loudly, naming the holder.
- **Transparent compression & encryption.** Per-volume `lz4`/`zstd` compression and RSA-wrapped AES-256-GCM/ChaCha20 encryption, declared at format; compression is best-effort per block, so incompressible data is stored raw instead of expanding.
- **Instant copy-on-write clones.** `squeezefs clone` duplicates a file's metadata without copying block data; block reference counts track the sharing.
- **Managed NVMe-oF targets, dual-stack.** `squeezefs nvmeof` shares, restores, and adopts target subsystems on SPDK (default) or kernel nvmet, backed by a write-ahead share ledger; refusals are loud and name the exact remedy.
- **Real observability.** Live daemon metrics as JSON on the virtual `.stats` inode; `status`, `clients`, and `df` verbs answer from the volumes themselves — no live mount required.
- **Host auto-tuning.** `squeezefs tune` applies the recommended kernel posture in one step: virtual-memory dirty ratios, socket buffer maxima, and live FUSE connection limits.
- **Proven crash contract.** kill-9 soak suites measure zero acked-durability loss, and every performance or durability claim traces to a committed measurement record in `.benchmarks/`.

## Architecture

```
   +-------------------------------------------------+
   |                  FUSE Client                    |
   |  (Rust, tokio, FUSE-over-io_uring transport)    |
   +--------+-------------------------------+--------+
            |                               |
   (locking & metadata)              (block data I/O)
            |                               |
            v                               v
   +-------------------+           +-------------------+
   |  Metadata Volumes |           |  NVMe / NVMe-oF   |
   |  (MetaLV CoW KV:  |           |  block devices    |
   |  inodes, layouts, |           |  (striped blocks, |
   |  leases, claims)  |           |  io_uring workers)|
   +-------------------+           +-------------------+
```

The client daemon routes each write by size: tiny payloads inline into the metadata volume, small files stage on local NVMe and promote asynchronously, and large files stripe directly across the block backend. Metadata volumes double as the coordination plane — leases, fencing tokens, mount claims, and client heartbeats are ordinary records on them, so no separate lock service exists. All block and file I/O rides io_uring; hot staged segments are memory-mapped for zero-syscall access.

## Performance

Measured with the built-in benchmark and elbencho on the reference substrate; every number links to a committed `.benchmarks/` record that states its box, substrate, and method. Selected headline classes:

| Workload | Measured | Record |
|---|---|---|
| Random 4 KiB O_DIRECT read, default mount (device-true) | **300–320 k IOPS**, no tuning | [transport concurrency](.benchmarks/2026-07-15-l1-transport-concurrency.md), [decomposition](.benchmarks/2026-07-15-iops-parity-decomposition.md) |
| Random 4 KiB O_DIRECT read, cache-tier resident | **~536–558 k IOPS**, zero device traffic | [hybrid I/O](.benchmarks/2026-07-15-hybrid-io.md) |
| Random 4 KiB write (in-place patch shape) | **59–67 k IOPS**, device cost 4 KiB-class per op | [random-write closing](.benchmarks/2026-07-17-rand-write-program-closing.md) |
| Large sequential write | **~1.8 GB/s** write-through; **4.4–4.6 GiB/s** device-true during sequential scoreboard rows | [zero-copy closing](.benchmarks/2026-07-08-zero-copy-write-path-closing.md), [random-write closing](.benchmarks/2026-07-17-rand-write-program-closing.md) |
| Metadata | many-dirs creates 32.7 k/s; rename/unlink ≈ 1.0 journal entries/op; 100 M-inode volume cold-mounts in ~22 ms | [metadata closing](.benchmarks/2026-07-15-metadata-throughput-closing.md), [v3 gates](.benchmarks/2026-07-09-kv-v3-gates.md) |
| vs. JuiceFS, matched-conditions A/B (3 regimes × 6 workloads) | **13 wins / 3 ties** across 18 rows; 2 rows allowlisted as attributed measurement artifacts | [scoreboard standing](.benchmarks/2026-07-17-rand-write-program-closing.md), [baseline](.benchmarks/2026-07-15-vs-juicefs-scoreboard.md) |

The full record set, the vs-JuiceFS scoreboard harness, and the built-in benchmark reference live in [docs/operations.md → Performance records](docs/operations.md#performance-records).

## Building

Linux-only: the build links FUSE 3 and uses `io_uring` end to end; mounting requires a kernel with FUSE-over-io_uring support (the mount enables `fuse.enable_uring` automatically where it can, and fails loudly if the transport cannot arm). System dependencies (Ubuntu/Debian):

```bash
sudo apt install -y build-essential pkg-config libfuse3-dev fuse3 clang libclang-dev
cargo build --release
```

Optional Cargo features (off by default): `gds` (GPU Direct Storage path), `dhat-on` (heap profiling), `coz-on` (causal profiling). Release builds keep debug symbols for profiling.

To verify a build, run the standard gate — clippy (`-D warnings`), `cargo fmt --check`, `cargo test --all-features -- --test-threads=1`, `cargo doc --no-deps`, and the criterion bench smoke. Root-only external suites (LTP, fstests, elbencho, the NVMe-oF fidelity tier) live under `tests/` and are tiered in [AGENTS.md](AGENTS.md).

## Quick example

A file-backed sandbox needs no root and no spare disks (full walkthrough: [QUICKSTART.md](QUICKSTART.md)):

```bash
mkdir -p ~/sqfs/staging ~/sqfs/mnt
truncate -s 256M ~/sqfs/meta.bin && truncate -s 8G ~/sqfs/data.bin

squeezefs format sqmeta://$HOME/sqfs/meta.bin sqdata://$HOME/sqfs/data.bin \
    --disk-cache-paths ~/sqfs/staging
squeezefs mount sqmeta://$HOME/sqfs/meta.bin ~/sqfs/mnt --daemon --log-file ~/sqfs/mount.log

echo hello > ~/sqfs/mnt/hello.txt
head -40 ~/sqfs/mnt/.stats                      # live daemon metrics (JSON)
squeezefs status sqmeta://$HOME/sqfs/meta.bin   # volume summary + client registrations
squeezefs umount ~/sqfs/mnt                     # drains staging before teardown
```

For measured work, skip file-backed volumes: `sudo tests/dev_substrate.sh create` builds a RAM-backed virtual NVMe substrate with real fabric namespaces ([QUICKSTART §2](QUICKSTART.md#2-dev-box-virtual-nvme-substrate-ram-backed-nvme-of-loop)).

The CLI surface: `format`, `mount`, `umount`, `status`, `clients`, `df`, `bench`, `clone`, `tune`, `config`, `claim`, `nvmeof`, and `storage` — each documented in [docs/operations.md](docs/operations.md).

## Documentation

| Document | Contents |
|---|---|
| [QUICKSTART.md](QUICKSTART.md) | Hands-on walkthrough: local sandbox, virtual NVMe dev substrate, bare metal, NVMe-oF fabrics, kernel tuning, durability knobs |
| [docs/operations.md](docs/operations.md) | Operator reference: durability & crash contract, mount-guard guarantee classes, breaking changes & removed verbs, configuration knobs, observability, NVMe-oF operations, performance records |
| `docs/design-*.md` | Normative design records for each subsystem — metadata format, write/read paths, metadata throughput, NVMe-oF target management |
| [`.benchmarks/`](.benchmarks/) | Committed measurement records: baselines, attributions, fix verifications, and program-closing adjudications |
| [AGENTS.md](AGENTS.md) | Contributor and agent rules: architecture invariants, TDD workflow, verification gates |
