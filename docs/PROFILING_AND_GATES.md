# Profiling, benchmarks, and external gates

Operational notes for release-mode baselines and post-write-path verification (P3-5 … P3-7).

## Build features

| Feature | Purpose |
|---------|---------|
| *(default)* | Production binary; `tikv-jemallocator` on Linux |
| `dhat-on` | Heap profiling via `dhat` (replaces global allocator in `main`) |
| `coz-on` | Causal profiling; `coz_progress!` active |
| `gds` | GPU Direct Storage path |

```bash
cargo build --release
cargo build --release --features dhat-on
cargo build --release --features coz-on
```

Release profile keeps `debug = true` for symbolicated profiles.

## Profiling command set (release)

### CPU (perf)

```bash
# One-shot sample while a mount + workload runs
perf record -g --call-graph dwarf -o perf.data -- \
  target/release/squeezefs mount …   # or attach: -p $(pidof squeezefs)

perf report -i perf.data
# or: perf script | inferno-collapse-perf | inferno-flamegraph > flame.svg
```

### Heap (dhat)

```bash
cargo build --release --features dhat-on
# Run workload; on exit dhat prints / writes heap profile per its config
DHAT_OUT=dhat-heap.json target/release/squeezefs mount …
```

### Causal (coz)

```bash
cargo build --release --features coz-on
coz run --- target/release/squeezefs mount …
```

Use coz/dhat **after** a known-good cargo test gate, against a representative mount (Garnet + backing file or NVMe).

## Criterion benches (P3-6)

```bash
# No Garnet required
cargo bench --bench high_concurrency_bench

# Full stack (skips gracefully if Garnet down)
cargo bench --bench squeezefs_bench
```

Includes `CryptoCompressState` microbenches (`process_write_none` / `lz4` / `aes…`).

**CI note:** Criterion is optional in PR CI (long / noisy). Prefer **nightly** or manual baseline save:

```bash
cargo bench --bench squeezefs_bench -- --save-baseline main
# later: --baseline main
```

## Required verification gate (every commit)

From `AGENTS.md` / project convention:

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
cargo test --all-features -- --test-threads=1
cargo doc --no-deps
```

## External POSIX / IO suites (P3-7)

These are **not** part of `cargo test`. Run as **root** on Linux/WSL after material write-path, layout, or FUSE lock changes:

```bash
# POSIX compliance (mounted squeezefs)
sudo tests/run_pjdfstest.sh

# Mount-level IO benchmark
sudo tests/run_elbencho_mount.sh
```

Prerequisites: Garnet up, volume formatted/mounted per `QUICKSTART.md`. Failures here can pass pure unit tests and still indicate mount regressions.

## Stats surface (P3-1)

Mounted volumes expose process metrics under the virtual **stats** inode (JSON), including layout mix, bg admission, uring queue-full, and lease acquire outcomes. Prefer these for live regression signals over ad-hoc logging.

## io_uring coverage (P2-8)

| Path | Mechanism |
|------|-----------|
| Primary block device R/W | `NvmeBlockDev` worker + fixed-file registration when supported |
| Ad-hoc file R/W / fdatasync | `crate::uring_fs` process worker (GDS cache materialize, etc.) |
| Mmap page hint | `IoUringPrefetcher` (`MADV_WILLNEED`) |
| **FUSE `/dev/fuse` transport** | **fuse3 `BlockFuseConnection`**: readv/writev on dedicated rings, eventfd wakeups, optional SQPOLL, multi-queue clone, **fixed-file** `Fixed(0)` for the fuse fd (P2-8) |
| Staging / read-segment hot path | **mmap** (by design — zero syscall get/put) |
| Garnet/Redis, TLS peers | **Not** uring (network stacks) |

### FUSE uring knobs (env)

| Variable | Effect |
|----------|--------|
| `SQUEEZEFS_FUSE_IO_URING_SQPOLL_IDLE_MS` | Enable SQPOLL with idle timeout (ms); also set via mount/format |
| `SQUEEZEFS_FUSE_IO_URING_SQPOLL_CPU` | Pin SQPOLL kernel thread |
| `SQUEEZEFS_FUSE_IO_URING_ENTRIES` | SQ depth for FUSE rings (default 1024, clamp 64–4096) |

Kernel **FUSE-over-io_uring** (register/cmd protocol, Linux 6.14+) is a separate ABI and is **not** required; we use classical FUSE over userspace `io_uring` syscalls, which works on older kernels.
