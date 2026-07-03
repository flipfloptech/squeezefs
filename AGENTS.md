# AGENTS.md

Squeezefs is a high-performance distributed POSIX FUSE filesystem (Rust + tokio + io_uring) with a decoupled Microsoft Garnet (RESP) metadata backend and an NVMe / NVMe-oF block data backend. Linux-only.

## Non-negotiable: always use io_uring when we can

**Policy for agents and humans:** prefer and require **io_uring** for every I/O path where the Linux kernel can do it. Do **not** “temporarily” fall back to classical `read`/`write`/`pread`/`pwrite`/`/dev/fuse` polling as a way to unblock a bug. Fix the uring path, or fail loud.

| Path | Expectation |
|------|-------------|
| **FUSE request hot path** | **FUSE-over-io_uring only** after arm (`REGISTER` / `COMMIT_AND_FETCH`). No userspace opt-out. Mount fails if setup fails. |
| **FUSE_INIT only** | Classical `/dev/fuse` once — kernel requires `fch->initialized` before REGISTER. Then over-uring. |
| **NVMe / block data** | `NvmeBlockDev` io_uring workers (fixed files when available). |
| **Ad-hoc local files** | `crate::uring_fs` (not std file APIs) where practical. |
| **Not uring** | Garnet/Redis TCP, TLS peers (network stacks). Staging **mmap** segments stay mmap by design. |

If over-uring or block uring misbehaves: **debug and fix uring** — never reintroduce a classical escape hatch “just to make tests pass.”

See `.agents/AGENTS.md` (architecture) and `.agents/skills/tdd-development-workflow/SKILL.md` (dev workflow).

## Authoritative docs already in this repo

- `.agents/AGENTS.md` — full architectural & behavioral spec (data layout tiers, DLM, caching, recovery). Read this before touching core logic; it is the source of truth for intended behavior.
- `.agents/skills/tdd-development-workflow/SKILL.md` — mandatory dev workflow. Summarized below.
- `.agents/walkthrough.md` — notes on the transparent compression / encryption implementation.
- `README.md` / `QUICKSTART.md` — user-facing CLI, NVMe-oF, and bare-metal setup.

## Branch & commit workflow

The integration branch is **`dev`** (not `main`; `main` is reserved for releases). `origin/HEAD` points at `origin/dev`.
- Branch from `dev`: `feat/`, `fix/`, `refactor/`, `perf/`, `docs/` prefixes. Never commit directly to `dev`.
- Tests-first cycle: write failing tests → implement → refine → commit each logical step separately.
- Merge with `--ff-only`; rebase feature branches if `dev` has diverged. Delete branches after merge.
- Conventional commits: `type(scope): description`. Messages explain WHY.

## Build

System deps (Ubuntu/Debian): `build-essential pkg-config libfuse3-dev fuse3 clang libclang-dev`. The build links FUSE 3 and uses `io-uring` + `/dev/fuse` — it will not compile on non-Linux.

```bash
cargo build --release
```

Optional Cargo features (off by default):
- `gds` — GPU Direct Storage RDMA path (pulls `libloading`).
- `dhat-on` — heap profiling (`dhat`); a static `dhat::Alloc` replaces the global allocator in `src/main.rs`.
- `coz-on` — causal profiling; the `coz_progress!` macro (defined in `src/lib.rs`) becomes active.

`[profile.release]` keeps `debug = true` so release binaries carry symbols for profiling. `tikv-jemallocator` is the global allocator on Linux.

## Patched `fuse3` dependency

`Cargo.toml` has `[patch.crates-io] fuse3 = { path = "third_party/fuse3" }`. The vendored copy in `third_party/fuse3/` is patched locally — do not replace it with the crates.io version or "restore" `Cargo.toml.orig`. If FUSE behavior changes, edit the vendored source.

## Testing

Most integration tests talk to a live **Garnet/Redis** instance. Behavior splits two ways — check before you run:

- **Graceful skip** when Redis is down (safe): `dlm_tests.rs`, `defrag_tests.rs`, `recovery_tests.rs`, `failover_tests.rs`, both `benches/*.rs`.
- **Will panic / fail** without Redis: `format_tests.rs`, `block_allocator_tests.rs`, `jobs_tests.rs`, `lock_contention_tests.rs`, `metadata_sharding_tests.rs`, `checkpoint_debug.rs`. These unwrap the Redis connection or assert on results — start Garnet first.

Quickest way to get Garnet running:
```bash
podman run -d --rm --replace --name squeezefs-garnet -p 6379:6379 ghcr.io/microsoft/garnet:latest
```
`redis-server` works too.

Redis-DB isolation across tests (do not "fix" these by changing DBs — they are chosen to avoid collisions):
- `format_tests.rs` → db `9`
- `metadata_sharding_tests.rs` → dbs `1`, `2`, `3`
- `block_allocator_tests.rs`, `jobs_tests.rs`, `lock_contention_tests.rs` → **default db 0** (these will clobber live mount metadata; do not run against a Garnet instance backing a mounted volume).

`GARNET_URL` env var overrides the default `redis://127.0.0.1:6379` in `dlm_tests.rs`, `recovery_tests.rs`, and `benches/*.rs`. Other tests hardcode the URL and ignore it.

Run everything:
```bash
cargo test --all-features -- --test-threads=1
```
`--test-threads=1` is used in this repo because several suites share Garnet state and are order-sensitive; use it when an unknown test starts failing locally. Run a single test:
```bash
cargo test --all-features --test dlm_tests -- test_lock_acquisition_and_release --exact
```

Required verification gate (must pass before commit):
```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
cargo test --all-features
cargo doc --no-deps
```

External POSIX/IO integration suites (require root + a mounted FS, not part of `cargo test`):
- `tests/run_pjdfstest.sh` — pjdfstest POSIX compliance, must run as root inside WSL/Linux.
- `tests/run_elbencho_mount.sh` — elbencho mount benchmark.

## Benchmarks

Two Criterion benches, both `harness = false`:
- `cargo bench --bench high_concurrency_bench` — in-memory lock / pool / cache contention; no Redis required.
- `cargo bench --bench squeezefs_bench` — exercises the full FS stack; requires Garnet (skips gracefully if absent). Also contains the `CryptoCompressState` compression/encryption micro-benches.

## Module map (non-obvious wiring)

- `src/main.rs` (~5.3k lines) is the entire CLI: `format`, `mount`, `status`, `defrag`, `bench`, `clone`, `tune`, `nvmeof`, `storage`, `config`. `src/lib.rs` is the library surface.
- `src/fuse_client.rs` — FUSE daemon + `format_volume_ext` / `SqueezefsFilesystem`.
- `src/routing.rs` — `DataRouter` (progressive layout: inline / staged / striped) plus the `OnceCell`-held `CryptoCompressState`.
- `src/dlm.rs` — distributed lock manager (`DlmClient`, `acquire_lock`, fencing tokens, heartbeat renewal).
- `src/block_allocator.rs`, `src/nvme_dev.rs`, `src/storage.rs` — block allocation + NVMe/LVM pool plumbing.
- `src/cache/`, `src/tiering/` — tiered cache (GDS / RAM LRU / NVMe staging) and tier selection.
- `src/crypto_compress.rs` — compression (lz4/zstd) + RSA-wrapped symmetric encryption, applied across all three write paths via `process_write` / `process_read`.
- `src/jobs.rs`, `src/defrag.rs`, `src/recovery.rs`, `src/nvmeof.rs`, `src/p2p.rs`, `src/config_ops.rs` — cluster jobs, defrag, crash recovery, NVMe-oF control, peer-to-peer, runtime config.

## Repo-specific conventions

- Garnet keys are namespaced via the `fs_key!("suffix")` macro and the global `FS_PREFIX` (`src/lib.rs`). Code that touches Garnet keys must go through the macro, not hardcode prefixes.
- `WRITE_VERIFICATION` is a process-global `AtomicBool` toggled by `--write-verification` on mount; read-after-write checksum verification uses it. Library code should call `write_verification_enabled()` rather than reading CLI args.
- The `.agents/AGENTS.md` spec names **RustFS** (S3) as the data backend, but `README.md` / `QUICKSTART.md` describe the **NVMe / NVMe-oF block** backend as primary. Both code paths exist (`aws-sdk-s3` and NVMe block device); when changing data-path code, check which backend the relevant test/CLI actually exercises rather than assuming.