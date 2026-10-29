# SqueezeFS

SqueezeFS is a shared-disk parallel filesystem for Linux, written in Zig. A client daemon on every
node mounts the same **volume set** — one or more **metadata volumes** and one or more **data
volumes**, local NVMe or NVMe-oF namespaces — and every read-write mount is an equal metadata
writer. There is no lock server, directory service or database: the volumes are the coordination
plane. The design target is thousands of simultaneously mounted clients reading and writing, rarely
the same files.

If you know Lustre: a metadata volume plays the MDT, a data volume the OST, and there is no MDS/OSS
tier in between — every client talks to the devices directly. Reading and writing follow the GPFS
shared-disk model.

- **Fast by construction.** FUSE-over-io_uring on the request path, io_uring with fixed files for
  block I/O, zero-copy and latch-free hot paths, a per-core executor. Every sizing default is derived
  from the machine (cores, RAM, device geometry), never a hand-picked constant.
- **Simple to run.** One binary (`squeezefs`), two URIs (`sqmeta://`, `sqdata://`), one block size.
  Files place themselves: tiny files live inline in their metadata record, small files pack many to a
  block, large files stripe in 4 MiB blocks over every healthy data volume with fill- and
  health-weighted placement. There are no per-file layout knobs to get wrong.
- **Scales metadata too.** Directories spread over the metadata volumes by depth (default: the first
  three levels round-robin; everything below follows its parent), so a single project tree uses every
  metadata volume. The depth is one set-level setting.
- **Durable the way you expect.** A metadata operation is acknowledged once applied and durable at the
  next commit (50 ms); `fsync`, `fdatasync` and `syncfs` are durable on return, including small writes
  that live in the write cache. On a supervised mount a `kill -9` of the daemon loses nothing synced
  or acknowledged: the supervisor keeps the journal tail and the write cache across the crash and the
  next daemon replays them. A power cut can lose only the unsynced work of the last commit interval.
- **Operable.** Online `fsck`, `scrub` and `defrag`; `status`, `df`, `clients`, `config show`; volume
  add/remove/migrate with background jobs; NVMe-oF sharing and fencing with persistent reservations;
  optional lz4/zstd compression and AES-256-GCM or ChaCha20-Poly1305 encryption, set at format.
- **A bypass for the fast lane.** An `LD_PRELOAD` interposer (`libsqueezefs_il.so`) and a C SDK
  (`libsqueezefs_sdk`, `include/squeezefs_sdk.h`) move data operations over a shared-memory ring to the
  daemon, skipping the kernel FUSE round trip. FIEMAP (`filefrag`, `xfs_io fiemap`) is answered for
  preloaded programs.

## Status

Release train `0.1.0` (tag `v0.1.0`), developed on Linux 7.3. The on-disk format and the wire protocol
are forward-only: an older binary refuses a newer volume, and a mount refuses an unknown format bit
loudly. Treat it as pre-production software: every release runs xfstests (`-g auto`), LTP, pjdfstest,
mdtest and the project's own smoke and crash-consistency rows (the few adjudicated exceptions are
listed with their reasons in `docs/gates/`), but it has not seen a production fleet yet.

Indicative metadata rates on the development workstation (32 threads, RAM-backed NVMe namespaces,
2 metadata + 2 data volumes, `mdtest -n 10000 -u`, 32 ranks): ≈ 257 k file creates/s, 328 k
removes/s, 284 k mkdir/s, 283 k rmdir/s. The measurement notes behind every number live in
`docs/evidence/`; the comparison against ext4 and xfs on the same substrate is
`docs/evidence/rung-10-localfs-comparison.md`.

## Requirements

- **Linux ≥ 7.3** with FUSE-over-io_uring buffer pools (`CONFIG_FUSE_IO_URING`; the daemon
  switches `fuse.enable_uring` on itself). A kernel without them refuses the mount, naming what is
  missing. Linux 7.3 runs the filesystem unpatched. `scripts/kernel/patches-mainline/` carries a
  few optional kernel patches — two fixes, `syncfs(2)` forwarded to the daemon,
  `FUSE_TIME_LIMITS`, host-scoped NVMe-oF subsystems for two fenced identities on one host — and
  `scripts/kernel/build-host-kernel` builds a kernel with them.
  Every kernel feature is probed at runtime; a missing one is reported by name, never worked around
  silently.
- **Root.** The daemon mounts with FUSE-over-io_uring and opens devices `O_DIRECT`.
- **Block devices** for anything you measure. Image files work for trying it out (see QUICKSTART),
  but then you are measuring the host filesystem.
- **Zig 0.17.0** to build. No other build dependency; zstd is vendored.
- Optional: `nvme-cli` and the `nvmet` modules for NVMe-oF; OpenMPI + `mdtest`/`ior` for the metadata
  and bandwidth rows; `fio` for the bandwidth rows.

## Build

```sh
zig build                          # the gate profile (ReleaseSafe + thin LTO) → zig-out/bin/squeezefs
zig build -Dprofile=dist dist      # the release artefact + split debug info
zig build shim                     # libsqueezefs_il.so, the LD_PRELOAD interposer
zig build sdk                      # libsqueezefs_sdk.{a,so} + include/squeezefs_sdk.h
zig build test                     # unit tests (the portable modules run on any host)
zig build gate                     # fmt + unit tests + static rails + the smoke rows (Linux, root)
```

`squeezefs --version` names the train, the commit and the profile. The daemon and the shim refuse to
pair across different builds.

Then read [QUICKSTART.md](QUICKSTART.md) to format a set and mount it.

## How it is put together

```
                 ┌──────────────────────────────────────────────────────┐
  applications ─►│ squeezefs daemon (one per mount)                     │◄─ LD_PRELOAD shim / SDK
                 │  FUSE-over-io_uring · per-core handler lanes         │   (shared-memory ring)
                 │  routing by layout · write cache · read cache        │
                 └──────────┬───────────────────────────┬───────────────┘
                 metadata, leases, tokens           block data I/O (io_uring, O_DIRECT)
                            ▼                                   ▼
            ┌──────────────────────────────┐    ┌──────────────────────────────┐
            │ Metadata volumes             │    │ Data volumes (NVMe / NVMe-oF)│
            │ copy-on-write KV trees       │    │ 4 MiB striped blocks         │
            │ per-writer journals          │    │ packed small-file blocks     │
            │ leases, membership, jobs     │    │ PR-fenced, per-volume bitmap │
            └──────────────────────────────┘    └──────────────────────────────┘
```

One law holds everywhere: a node appends only to metadata trees it leases, journals only into its own
log, allocates only from its own grants, and runs every structural change of its trees itself. Everyone
else reads under tokens the lessee grants and recalls.

| Directory | What it holds |
|---|---|
| `src/fuse/` | the FUSE-over-io_uring transport and the request handlers |
| `src/meta/` | the metadata engine: KV trees, the commit conveyor, journals, leases, placement |
| `src/data/` | the data plane: layouts, allocation, the write and read caches, routing |
| `src/exec/`, `src/sync/` | the executor (an `std.Io` implementation) and its primitives |
| `src/ipc/`, `src/sdk/` | the client bypass: the shared-memory ring, the shim, the SDK |
| `src/ops/` | fsck, scrub, defrag, jobs, keys |
| `src/cli/` | every `squeezefs` verb, the supervisor, the smoke rows |
| `src/model/` | the exhaustive model checker every lock-free core is proven with |
| `src/core/` | knobs registry, counters, logging, clocks, checksums |
| `tests/` | the test runner, the static rails, cost tables, the fuzz targets |
| `scripts/` | `venue` (run things on a Linux venue), `perf_mount.sh`, `suites/`, `kernel/`, `release.sh` |
| `docs/` | see below |

## Documentation

- `docs/charter.md` — **the specification.** Mechanisms, invariants, derivations, the delivery plan.
  Read it before touching core logic.
- `docs/decisions/` — why things are the way they are, including every deliberate departure from the
  Rust reference implementation (`0002`, `0003`).
- `docs/evidence/` — one note per landed rung or lever: what changed, why, the red test, the numbers.
- `docs/porting/` — compact specs extracted from the reference implementation.
- `docs/gates/` — the test ledgers: known flakes, suite exceptions with their reasons.
- `docs/handoff/` — the latest state of the work and its open items.
- `AGENTS.md` — how we work: venues, the gate, landing tiers, conventions.

## Contributing

Work lands through `AGENTS.md`'s process: a named branch off `dev`, a red test first, the targeted
runs on a Linux venue, the full gate for anything that touches the format, the wire, a lock-free core
or a durability law, then a fast-forward onto `dev`. Every `SQUEEZEFS_*` knob is a registry row in
`src/core/knobs.zig`; every "cannot happen" is a tripwire counter; no dead code.
