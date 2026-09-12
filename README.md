# SqueezeFS

SqueezeFS is a high-performance distributed POSIX filesystem for Linux, written in Rust on `io_uring` with its own lock-free executor — the daemon links no async-runtime framework. File metadata lives on dedicated block-based metadata volumes (**MetaLV**, a copy-on-write key/value store) and file contents are striped across local NVMe or NVMe-oF block devices. A FUSE client daemon exposes the filesystem at bare-metal throughput — no external database or coordination service required.

## Key properties

- **FUSE-over-io_uring transport.** Every mount uses the kernel's FUSE-over-io_uring request path. It is required, not optional: a mount fails loudly if the kernel cannot provide it.
- **Crash-safe metadata.** Every metadata change is atomic and checksummed on any device, including plain files — a crash or torn write can never leave metadata half-applied.
- **Files of any size.** The block map scales to petabyte-class files with nothing to configure; large files are handled automatically.
- **Fast writes of every shape.** Large writes ride zero-copy transport leases into whole-block write-through; small random overwrites become one in-place sub-block write instead of a read-modify-write; small files pack many-to-a-block on the shared devices instead of taking a block each.
- **One writer, many readers, opt-in co-writers.** One write mount per volume set, any number of read-only mounts, and co-writer mounts sharing the data devices (opt-in). A mount guard refuses a second plain writer loudly, naming the holder.
- **Transparent compression and encryption.** Per-volume `lz4`/`zstd` compression and AES-256-GCM/ChaCha20 encryption, declared at format; the key lives in a file the operator controls, never on the volume.
- **Instant copy-on-write clones.** `squeezefs clone` duplicates a file's metadata without copying its blocks.
- **Built to operate.** Online data-volume add/remove, online fsck and defrag, managed NVMe-oF targets; live metrics as JSON on the mount (`.stats`), and `status`/`clients`/`df` answered from the volumes directly.

## Architecture

```
   +-------------------------------------------------+
   |                  FUSE Client                    |
   |  (Rust, FUSE-over-io_uring transport)           |
   +--------+-------------------------------+--------+
            |                               |
   (locking & metadata)              (block data I/O)
            |                               |
            v                               v
   +-------------------+           +-------------------+
   |  Metadata Volumes |           |  NVMe / NVMe-oF   |
   |  (MetaLV CoW KV:  |           |  block devices    |
   |  inodes, layouts, |           |  (striped blocks, |
   |  claims, jobs)    |           |  io_uring workers)|
   +-------------------+           +-------------------+
```

The client daemon routes each write by size: payloads up to one page are stored inside the metadata volume, small files (up to one block) stage on local NVMe where the format declared staging paths and are later promoted — packed together into shared blocks — onto the block devices, and large files stripe directly across them. The metadata volumes double as the coordination plane — mount claims, client heartbeats and maintenance-job state are ordinary records on them, so there is no separate lock service to run. All block and file I/O rides io_uring.

## Performance

Measured on a fabric of 5 storage nodes over NVMe/TCP (memory-backed targets) with a 32-core client, using the `LD_PRELOAD` interception shim (the data path that bypasses the kernel for applications that load it), build `c985fa8c`. Instrument: a user-run client-validation script, 40 s rows — burst-class figures, not a sustained window ([the record](.benchmarks/2026-09-02-e2e-audit-baseline.md)):

| Read bandwidth | Write bandwidth | Read IOPS (4 KiB) | Write IOPS (4 KiB) |
|:---:|:---:|:---:|:---:|
| **43.9 GB/s** | **36.4 GB/s** | **942 k** | **727 k** |

Against the reference FUSE field — JuiceFS, SeaweedFS, geesefs, mountpoint-s3 — SqueezeFS ranked first on 13 of 18 primary rows and second on the other five in the standing scoreboard run ([2026-07-18](.benchmarks/2026-07-18-multi-reference-scoreboard.md)). Every measurement, method and record lives in [docs/operations.md → Performance records](docs/operations.md#performance-records) and the notes under [`.benchmarks/`](.benchmarks/).

**Scope, stated plainly.** What ships is one write mount per volume set, plus any number of read-only mounts, plus opt-in co-writer mounts. The very-large-fleet design target has so far been proven on a single-node fleet of many co-located mounts; every scale claim carries its evidence tier in [docs/rc-manifest.md](docs/rc-manifest.md).

## Building

Linux only: the build links FUSE 3 and mounting needs a kernel with FUSE-over-io_uring support (the mount enables `fuse.enable_uring` itself where it can). System dependencies (Ubuntu/Debian):

```bash
sudo apt install -y build-essential pkg-config libfuse3-dev fuse3 clang libclang-dev
cargo build --release
```

Packaged builds use [go-task](https://taskfile.dev) (`Taskfile.yml`); install it into `./bin` without root if it is absent (`sh -c "$(curl -fsSL https://taskfile.dev/install.sh)" -- -d -b ./bin`):

| Task | Result |
|---|---|
| `task build` | Host build → `dist/host/` (daemon + interception shim) |
| `task build:<distro>` — `rocky8`, `rocky9`, `ubuntu2404`, `ubuntu2604` | Container build for that distro → `dist/<distro>/` (needs docker or podman) |
| `task build:all` | All four distro builds |
| `task dist:<distro>` / `task dist:all` | Release builds (full LTO) for tagged releases → `dist/<distro>-dist/` — stripped daemon + shim with their `.debug` sidecars and `SHA256SUMS` |
| `task check` | The full verification gate |

Each build folder holds `squeezefs` and `libsqueezefs_il.so` side by side. Deploy the daemon and the interception shim from the same build folder together — they refuse to pair across builds. `squeezefs --version` prints the release train, the git commit and the build profile on one line, e.g. `squeezefs 1.2.4 (<commit> / <full commit>) built <timestamp> profile release` (a build on a release tag adds `, tag stable-…` after the commit).

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
squeezefs umount ~/sqfs/mnt                     # the daemon's teardown promotes staged files first
```

For measured work, skip file-backed volumes: `sudo tests/dev_substrate.sh create` builds a RAM-backed virtual NVMe substrate with real fabric namespaces ([QUICKSTART §2](QUICKSTART.md#2-dev-box-virtual-nvme-substrate-ram-backed-nvme-of-loop)).

The CLI surface: `format`, `mount`, `umount`, `status`, `clients`, `df`, `bench`, `clone`, `tune`, `config`, `claim`, `staging`, `volume`, `fsck`/`scrub`, `defrag`, `job`, `nvmeof`, and `storage` — `squeezefs <verb> --help` for each; the operator reference is [docs/operations.md](docs/operations.md).

## Documentation

| Document | Contents |
|---|---|
| [RELEASE_NOTES.md](RELEASE_NOTES.md) | What changed in 1.2.4, 1.2.3, 1.2.2, 1.2.1 and 1.2.0: highlights, upgrading from 1.1, fixes, new operator surface, known limitations, the release gate |
| [QUICKSTART.md](QUICKSTART.md) | Hands-on walkthrough: local sandbox, virtual NVMe dev substrate, bare metal, NVMe-oF fabrics, kernel tuning, durability knobs |
| [docs/operations.md](docs/operations.md) | Operator reference: durability & crash contract, mount-guard guarantee classes, breaking changes & removed verbs, every configuration knob, every metric, NVMe-oF operations, performance records |
| `docs/design-*.md` | Design records for each subsystem |
| [`.benchmarks/`](.benchmarks/) | Committed measurement records |
| [AGENTS.md](AGENTS.md) | Contributor rules: architecture invariants, development workflow, verification gates |

## License

SqueezeFS is source-available under the [Business Source License 1.1](LICENSE): use it, modify it, run it in production — including commercially — for anything except offering SqueezeFS itself (or a derivative) to third parties as a competing commercial storage product or managed/hosted storage service. Each released version automatically converts to Apache 2.0 four years after its publication. Third-party components and the dependency license audit are recorded in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
