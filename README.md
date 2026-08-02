# SqueezeFS

![SqueezeFS Header](github.jpeg)

SqueezeFS is a high-performance distributed POSIX filesystem for Linux, built in Rust on `tokio` and `io_uring`. It decouples metadata from data: file metadata lives on dedicated block-based metadata volumes (**MetaLV**, a copy-on-write key/value store), while file contents are striped across local NVMe or NVMe-oF block devices. A FUSE client daemon exposes the filesystem — with no external database or coordination service — at bare-metal throughput.

> **Concurrency scope, stated plainly.** What ships today is **one write mount per volume set**, enforced (not merely advised) by the single-writer mount guard: a second concurrent mount is refused loudly. The 15,000-node scale target is the design's destination, and the program to get there — read-only coherent mounts first, then multi-writer — is specified in `docs/pre-rc-engineering-spec.md` §6 with its staging plan and gates. Every capability claim in this README describes shipped behavior; roadmap items say so.

## Key properties

- **FUSE-over-io_uring transport.** Every mount arms the kernel's FUSE-over-io_uring request path after the INIT handshake — required, not optional; the mount fails loudly if the kernel cannot arm it.
- **Crash-safe metadata by construction.** The v3 metadata format is a copy-on-write, checksummed key/value btree: whole-transaction atomicity and torn-write immunity on any substrate, including plain files. Scales to ≥ 100 M inodes and 1 M+ entries per directory per volume.
- **Zero-copy write path.** Large writes travel kernel → transport payload lease → one merge copy → io_uring DMA; fully written blocks upload directly to the block backend, skipping staging entirely.
- **Efficient random small writes.** Aligned small overwrites of exclusively owned blocks become one in-place sub-block DMA — no read-modify-write, no metadata commit; other shapes ride a byte-budgeted extent overlay with batched folds.
- **Progressive data layouts.** Files grow from inline (< 4 KiB, metadata-resident) through locally staged (≤ 4 MiB, NVMe staging) to striped 4 MiB blocks, promoted durably as they cross thresholds.
- **Tiered caching with hybrid O_DIRECT.** Optional GPU Direct Storage, RAM clock/LRU tiers, and NVMe staging/read cache. O_DIRECT reads bypass the kernel page cache but still serve from SqueezeFS's own tiers; a mount option restores strictly device-true O_DIRECT for measurement.
- **Write custody and fencing.** File write custody is taken as a per-inode lease with a monotonic fencing token; stale writers are fenced, never trusted. **Scope today: one mount.** The lease manager is process-local — it serializes writers *within* a mount, and cross-mount exclusion is enforced by the single-writer mount guard below, not by a network protocol. Multi-client operation is the active program (`docs/pre-rc-engineering-spec.md` §6): one writer plus N coherent readers first, then multi-writer. POSIX advisory locks are kernel-local per mount — full canonical semantics via the kernel's own arbitration.
- **Single-writer mount guard.** Write mounts claim their metadata volumes via `flock`, NVMe Persistent Reservations where the device supports them, and heartbeat claim records — a second concurrent mount is refused loudly, naming the holder.
- **Transparent compression & encryption.** Per-volume `lz4`/`zstd` compression and RSA-wrapped AES-256-GCM/ChaCha20 encryption, declared at format; compression is best-effort per block, so incompressible data is stored raw instead of expanding.
- **Instant copy-on-write clones.** `squeezefs clone` duplicates a file's metadata without copying block data; block reference counts track the sharing.
- **Managed NVMe-oF targets, dual-stack.** `squeezefs nvmeof` shares, restores, and adopts target subsystems on SPDK (default) or kernel nvmet, backed by a write-ahead share ledger; refusals are loud and name the exact remedy.
- **Online volume lifecycle & maintenance.** Add and remove metadata and data volumes with honest capacity preflight and automatic copy-on-write migration; online fsck with verified findings and per-class quarantine-first repair; a four-axis online defragmenter; all long-running maintenance runs as pausable, percentage-throttled background jobs that survive crashes and can be distributed across mounted clients.
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
| Random 4 KiB read via **LD_PRELOAD interception** (`-o interception` + `libsqueezefs_il.so`) | warm **~1.02 M IOPS** (1.58× the kernel-FUSE warm path); device-true **~622 k**, beating the kernel-FUSE reference — engagement counter-verified | [interception closing](.benchmarks/2026-07-19-l4-interception-closing.md) |
| Metadata | many-dirs creates 32.7 k/s; rename/unlink ≈ 1.0 journal entries/op; 100 M-inode volume cold-mounts in ~22 ms | [metadata closing](.benchmarks/2026-07-15-metadata-throughput-closing.md), [v3 gates](.benchmarks/2026-07-09-kv-v3-gates.md) |
| vs. the reference FUSE field — JuiceFS, SeaweedFS, geesefs, mountpoint-s3 (matched conditions, 3 regimes × 6 workloads, fsync-inclusive **durable** write timing) | **top-3 or better on every row-family; fastest of the field on 13 of 18 rows** (59 W / 2 TIE across 66 comparable cells; 3 attributed-loss cells tracked) | [multi-reference scoreboard](.benchmarks/2026-07-18-multi-reference-scoreboard.md), [JuiceFS-only lineage](.benchmarks/2026-07-15-vs-juicefs-scoreboard.md) |

The full record set, the multi-reference scoreboard harness (`tests/run_scoreboard.sh`), and the built-in benchmark reference live in [docs/operations.md → Performance records](docs/operations.md#performance-records).

## Building

Linux-only: the build links FUSE 3 and uses `io_uring` end to end; mounting requires a kernel with FUSE-over-io_uring support (the mount enables `fuse.enable_uring` automatically where it can, and fails loudly if the transport cannot arm). System dependencies (Ubuntu/Debian):

```bash
sudo apt install -y build-essential pkg-config libfuse3-dev fuse3 clang libclang-dev
cargo build --release
```

Optional Cargo features (off by default): `gds` (GPU Direct Storage path), `dhat-on` (heap profiling), `coz-on` (causal profiling). Release builds keep debug symbols for profiling.

### Packaged builds with go-task

The packaged build path is [go-task](https://taskfile.dev) (`Taskfile.yml`); plain `cargo build` remains valid for dev work. Bootstrap go-task without root — install into `./bin` locally if it is absent (never sudo system-wide; some distros ship the binary as `go-task`, both spellings work):

```bash
command -v task go-task >/dev/null || \
  sh -c "$(curl -fsSL https://taskfile.dev/install.sh)" -- -d -b ./bin
export PATH="$PWD/bin:$PATH"
```

| Task | Result |
|---|---|
| `task build` (default) | Host dev build → `dist/host/` (daemon + interception shim) |
| `task build:rocky8` / `build:rocky9` / `build:ubuntu2404` / `build:ubuntu2604` | Distro-targeted container build → `dist/<target>/` |
| `task build:all` | All four distro targets |
| `task check` | The authoritative full cargo gate (clippy `-D warnings`, fmt, tests, doc, bench smoke) |
| `task clean` | Remove `dist/` |

Every `dist/<target>/` holds `squeezefs` and `libsqueezefs_il.so` **side by side under their default names** — the folder disambiguates, never a filename suffix. The pair in one folder is built from the **same commit**, and that pairing is load-bearing: interception sessions refuse a daemon/shim build-commit mismatch (KD-7), so deploy a dist folder as a unit and never mix artifacts across folders or builds.

Distro builds need docker or podman (docker preferred; `CONTAINER_TOOL=podman task build:rocky8` overrides). They mount the checkout read-only so the git commit identity embeds, cache the cargo registry/target in per-target named volumes for incremental rebuilds, and assert per artifact — inside the container — that `--version` carries the real commit (never `unknown`) and that the maximum referenced `GLIBC_` symbol version stays within the distro's ceiling (Rocky 8 = 2.28, Rocky 9 = 2.34, Ubuntu 24.04 = 2.39, Ubuntu 26.04 probed in-image).

Builds carry two identities: the release-train version (currently the 1.1 train, bumped as a release act) and the git commit they were built from; releases are `stable-*`/`lts-*` git tags on specific commits. Check a build with `squeezefs --version` (both identities on one line; also the `.stats` `build_commit` field on a mounted daemon); policy details in [docs/operations.md §Versioning & releases](docs/operations.md#versioning--releases).

To verify a build, run the standard gate — `task check`: clippy (`-D warnings`), `cargo fmt --check`, `cargo test --all-features -- --test-threads=1`, `cargo doc --no-deps`, and the criterion bench smoke. Root-only external suites (pjdfstest, LTP, fstests, elbencho, the NVMe-oF fidelity tier) live under `tests/` and are tiered in [AGENTS.md](AGENTS.md).

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

The CLI surface: `format`, `mount`, `umount`, `status`, `clients`, `df`, `bench`, `clone`, `tune`, `config`, `claim`, `volume`, `fsck`/`scrub`, `defrag`, `job`, `nvmeof`, and `storage` — each documented in [docs/operations.md](docs/operations.md).

## Documentation

| Document | Contents |
|---|---|
| [QUICKSTART.md](QUICKSTART.md) | Hands-on walkthrough: local sandbox, virtual NVMe dev substrate, bare metal, NVMe-oF fabrics, kernel tuning, durability knobs |
| [docs/operations.md](docs/operations.md) | Operator reference: durability & crash contract, mount-guard guarantee classes, breaking changes & removed verbs, configuration knobs, observability, NVMe-oF operations, performance records |
| `docs/design-*.md` | Normative design records for each subsystem — metadata format, write/read paths, metadata throughput, NVMe-oF target management |
| [`.benchmarks/`](.benchmarks/) | Committed measurement records: baselines, attributions, fix verifications, and program-closing adjudications |
| [AGENTS.md](AGENTS.md) | Contributor and agent rules: architecture invariants, TDD workflow, verification gates |

## License

SqueezeFS is source-available under the [Business Source License 1.1](LICENSE): use it, modify it, run it in production — including commercially — for anything except offering SqueezeFS itself (or a derivative) to third parties as a competing commercial storage product or managed/hosted storage service. Each released version automatically converts to Apache 2.0 four years after its publication. Third-party components and the dependency license audit are recorded in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
