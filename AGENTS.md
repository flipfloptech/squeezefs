# AGENTS.md — Squeezefs One Source of Truth

Squeezefs is a high-performance distributed POSIX FUSE filesystem (Rust + tokio + io_uring) with a decoupled Microsoft Garnet (RESP) metadata backend and an NVMe / NVMe-oF block data backend. Linux-only.

**This combined document is the single authoritative reference.** It merges architectural rules, non-negotiables, development workflow, build/test gates, profiling, and repo conventions. Read it before touching core logic.

---

## Non-Negotiables

### Always use io_uring when we can

**Policy for agents and humans:** prefer and require **io_uring** for every I/O path where the Linux kernel can do it. Do **not** “temporarily” fall back to classical `read`/`write`/`pread`/`pwrite`/`/dev/fuse` polling as a way to unblock a bug. Fix the uring path, or fail loud.

| Path                              | Expectation |
|-----------------------------------|-------------|
| **FUSE request hot path**         | **FUSE-over-io_uring only** after arm (`REGISTER` / `COMMIT_AND_FETCH`). No userspace opt-out. Mount fails if setup fails. |
| **FUSE_INIT only**                | Classical `/dev/fuse` once — kernel requires `fch->initialized` before REGISTER. Then over-uring. |
| **NVMe / block data**             | `NvmeBlockDev` io_uring workers (fixed files when available). |
| **Ad-hoc local files**            | `crate::uring_fs` (not std file APIs) where practical. |
| **Not uring**                     | Garnet/Redis TCP, TLS peers (network stacks). Staging **mmap** segments stay mmap by design. |

If over-uring or block uring misbehaves: **debug and fix uring** — never reintroduce a classical escape hatch “just to make tests pass.”

**FUSE uring knobs (env / mount)**

| Variable | Effect |
|----------|--------|
| `SQUEEZEFS_FUSE_IO_URING_SQPOLL_IDLE_MS` | Enable SQPOLL with idle timeout (ms) |
| `SQUEEZEFS_FUSE_IO_URING_SQPOLL_CPU` | Pin SQPOLL kernel thread |
| `SQUEEZEFS_FUSE_IO_URING_ENTRIES` | SQ depth for classical FUSE rings (INIT/notify) |
| `SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH` | Entries per FUSE-over-io_uring queue (default 4) |
| `SQUEEZEFS_FUSE_OVER_IO_URING_QUEUES` | Number of queues (default `min(nproc, 8)`, max 32) |

Mount always enables FUSE-over-io_uring after INIT (required). Expects: "FUSE-over-io_uring ready..." and "transport enabled for this session".

**io_uring coverage**

| Path | Mechanism |
|------|-----------|
| Primary block device R/W | `NvmeBlockDev` worker + fixed-file registration when supported |
| Ad-hoc file R/W / fdatasync | `crate::uring_fs` process worker |
| Mmap page hint | `IoUringPrefetcher` (`MADV_WILLNEED`) |
| FUSE transport (default) | `fuse3` `BlockFuseConnection` (classical rings during INIT); **FUSE-over-io_uring** (`IORING_OP_URING_CMD` + REGISTER/COMMIT_AND_FETCH) after arm |
| Staging / read-segment hot path | **mmap** (by design — zero syscall) |
| Garnet/Redis, TLS peers | Not uring (network) |

Hardening: pool not marked ready until all queues submit REGISTER; per-qid commit channels; shared inbound queue; eventfd wake; full-size payload buffers.

### No dead code

**Do not leave unused code in the tree.** Agents and humans must remove it, not silence it.

- **Delete** unused functions, methods, fields, imports, constants, modules, and feature-gated stubs that nothing calls.
- **Do not** paper over dead code with `#[allow(dead_code)]`, `#[allow(unused)]`, or broad `allow` attributes “for later.” If it is not used now, delete it; restore from git when needed.
- **Clippy/gate:** `cargo clippy --all-targets --all-features -- -D warnings` must stay clean — that includes unused items. Fix by **removing** dead code, not by allowing warnings.
- **Exceptions only** when the item is part of a public API surface that must stay stable (`pub` for crates/downstream) or is required for `#[cfg]` / trait impl completeness and truly cannot be omitted — document why in a one-line comment on that item. Prefer not exporting unused symbols.

---

## High-Performance Distributed Filesystem Architecture

### Target Scale & Layout
* **Scale:** 15,000+ Concurrent Nodes.
* **Architecture Type:** Decoupled metadata (Garnet) + **block data** (local NVMe / NVMe-oF), exposed via POSIX FUSE.

### Technology Stack
* **Client Daemon (FUSE Engine):** Built in **Rust** using the asynchronous `tokio` runtime and `io_uring` polling over `/dev/fuse`.
* **Metadata & Distributed Lock Manager (DLM):** Microsoft Research **Garnet** (RESP-compatible). Primary meta store for attrs, layout maps, leases, and volume format.
* **Data Backend (primary):** **NVMe / NVMe-oF block devices** via `NvmeBlockDev` (io_uring workers). Progressive layouts (inline / staged / striped) live on this path.

### Progressive Data Layout & I/O Routing

Logical file growth uses three layouts (thresholds are implementation-defined; current code uses ~4 KiB inline, up to ~4 MiB staged when staging dirs exist, else striped):

1. **Inline (tiny):** Payload in Garnet (`inline_data:…`) with type `inline`.
2. **Staged (small):** Local NVMe staging (`file_id` + optional `mapping:…`); writeback/flush promotes to durable blocks.
3. **Striped (large):** 4 MiB (configurable) blocks on the active block backend with `block_map:…` and refcounts.

Writes that grow past thresholds promote layouts **durably** (block I/O before meta type flip — see P0 layout atomicity).

### Distributed Lock Manager (DLM) & Consistency

POSIX FUSE locks map to cluster leases on Garnet:
* **Acquisition:** `SET NX` + fencing token `INCR` (no Lua required for lock grant).
* **Granularity:** File-level or byte-range; never directory-wide for data.
* **Leases & Heartbeats:** TTL + background renewal; local caches must re-validate after lock key loss.
* **Fencing Tokens:** Monotonic per-file tokens; writers present tokens; stale tokens → `FencingTokenExpired` / reject.

### Tiered Caching & Paths

* **Tier 1 (optional GDS):** GPU Direct path when `gds` feature is enabled.
* **Tier 2 (RAM LRU):** Sharded Clock/LRU read & write caches.
* **Tier 3 (Local NVMe staging / read cache):** Staging segments + optional read block cache; dehydrate on eviction.

### FUSE Client & Asynchronous I/O

* Work-stealing / multi-thread tokio; core pinning where configured.
* **Block path io_uring:** `NvmeBlockDev` worker (bounded request queue, backpressure, fixed-file register when available).
* **Path file I/O io_uring:** `crate::uring_fs` for ad-hoc local files (e.g. GDS cache materialize). Staging **mmap** segments stay mmap for zero-syscall get/put.
* **FUSE transport:** vendored **fuse3** — classical `/dev/fuse` only for `FUSE_INIT` (kernel requires initialized connection before REGISTER); **FUSE-over-io_uring is required** for the request hot path after arm (`flags2` `FUSE_OVER_IO_URING`, `REGISTER` / `COMMIT_AND_FETCH`). No userspace opt-out; mount fails if setup fails. Auto-enables `fuse.enable_uring=Y` when possible. One queue per possible CPU; session arms only after all queues REGISTERed.
* Always use io_uring when we can (non-negotiable) — see Non-Negotiables section.
* No dead code (non-negotiable) — see Non-Negotiables section.
* Not uring: Garnet/Redis TCP, TLS peer paths (network). Staging mmap segments stay mmap by design.

### Metadata Cluster Topology

* Optional multi-shard Garnet URLs; keys for volume control use `fs_prefix` / `fs_key!`.
* **Layout keys** (`metadata:…`, `inline_data:…`, `block_map:…`, `mapping:…`, `active_block:…`) are **unprefixed historical** forms — use `crate::keys::*` helpers; do not migrate under `FS_PREFIX` without an on-disk format change.

### Error Handling & Crash Recovery

* FUSE op timeouts; staging recovery on remount (`recover_staging`) with fence + layout checks.
* Stale fencing tokens discard staged work; missing inode meta discards orphan active blocks.
* Write verification is **opt-in** (`--write-verification`, optional sample rate).

### Lock order & connection scope (must not)

Always acquire in this order; **never invert** (P1-9):

1. `active_inode_locks` (per-inode `RwLock`) — FUSE op serialization
2. `lease_locks` (per-inode) — only while acquiring/refreshing DLM lease
3. `BLOCK_FLUSH_LOCKS` (per block) — active-block mutation
4. DLM/Redis — network meta work

**Must not:**
* Hold inode **write** guard across long backend I/O when block locks suffice (striped data path = meta-prep only under write lock — P1-8).
* Hold a **pooled Garnet/Redis connection** across durable NVMe / staging I/O (P1-10: open → short meta → drop → I/O → re-acquire for commit).
* Acquire (1) while holding (3).
* Burn fencing tokens on failed lock `SET NX` (SET then INCR only on success).

### OS / fabric notes

* Dirty ratios and fabric tuning remain operator concerns for large clusters.
* Primary data path uses the NVMe/NVMe-oF block storage backend.

---

## TDD Development Workflow

A disciplined, test-first development workflow for building industrial-grade, high-performance, highly scalable, asynchronous Rust systems. Tests define the contract. Code fulfills it.

### Core Principle: Tests First, Code Second

Every feature, bugfix, or refactor follows the same cycle:

1. **Define** — What should the correct behavior be?
2. **Test** — Write tests that assert that behavior (they will fail).
3. **Implement** — Write the minimum code to make the tests pass.
4. **Refine** — Refactor for clarity and performance while tests stay green.
5. **Commit** — Each logical step gets its own git commit.
6. **Merge** — Bring the code base back into the primary branch.

### Phase 0: Branch from `dev`

All work happens on feature/fix branches off `dev`. Never commit directly to `dev` or `main`.

```bash
git checkout dev
git pull origin dev
git checkout -b feat/short-description   # or fix/short-description
```

- **Branch naming**: `feat/`, `fix/`, `refactor/`, `perf/`, `docs/` prefixes matching conventional commit types.
- **One branch per logical unit of work**: a feature, a bugfix, or a hardening pass.

#### SqueezeFS I/O constraint (always io_uring when we can)

This repo is Linux + **io_uring**-first. When planning or implementing:

- **Default to io_uring** for FUSE request traffic (FUSE-over-io_uring after arm), NVMe/block I/O (`NvmeBlockDev`), and local file I/O (`crate::uring_fs`) whenever the kernel can support it.
- **Do not** introduce or re-enable classical `/dev/fuse` or POSIX file I/O fallbacks to “make it work.” That is an anti-pattern here. Fix the uring path or fail loud.
- The **only** intentional classical FUSE use is the one-shot **`FUSE_INIT`** exchange (kernel requires it before REGISTER). After arm, requests/replies are over-uring only.
- If a failing test tempts you to disable over-uring or switch to blocking `read`/`write`, stop — write a failing test that encodes correct uring behavior, then fix uring.

#### No dead code

- Remove unused functions, fields, imports, and modules as part of the same change that made them unused.
- **Never** add `#[allow(dead_code)]` / `#[allow(unused_*)]` to park unused code. Delete it; git has history.
- Clippy with `-D warnings` failing on unused items means **delete**, not allow.

### Phase 1: Understand & Plan

Before touching any code:

- State the goal in one sentence.
- Identify the crate(s) and module(s) that will be affected.
- List the behaviors that need to be correct when you're done.
- Identify edge cases, error conditions, and concurrency concerns upfront.
- Determine `Send`/`Sync` requirements for all shared state.
- Identify async boundaries — which types must implement `Future`, which tasks cross `.await` points.
- **I/O path:** does this touch FUSE, NVMe, or local files? If yes, plan the **io_uring** design — not a classical shortcut.

Produce a brief execution plan:

```
Goal: [one sentence]
Affected: [crates/modules]

Behaviors:
1. [Expected behavior] → verify: [how]
2. [Expected behavior] → verify: [how]
3. [Edge case] → verify: [how]

Concurrency:
- Shared state: [Arc<Mutex<T>>, Arc<RwLock<T>>, lock-free, actor]
- Async runtime: [tokio multi-thread, current-thread, custom]
- Cancellation: [CancellationToken, drop-based, deadline]
```

### Phase 2: Write Tests

Write the test file(s) BEFORE any implementation:

- **Happy path**: The expected, normal-operation case.
- **Error paths**: What happens when inputs are invalid, connections drop, `CancellationToken` fires, channels close?
- **Boundary conditions**: Empty inputs, maximum values, zero-length streams, single-node mesh.
- **Concurrency**: Race conditions under parallel access. Multiple tokio tasks hitting the same state. Use `#[tokio::test(flavor = "multi_thread", worker_threads = N)]` to stress shared state.
- **Timeouts & Cancellation**: `tokio::time::timeout`, `CancellationToken` propagation, slow peers, hung connections.
- **Backpressure**: Channel saturation, bounded queue overflow, consumer lag.
- **Shutdown**: Graceful drain under in-flight work, partial completion, resource cleanup on abort.

#### Test Quality Checklist

- [ ] Each test has a clear, descriptive name (`test_broadcast_storm_uuid_dedup`)
- [ ] Tests are independent — no shared mutable state between test cases
- [ ] Parameterized tests via `rstest` (`#[case]`, `#[values]`) for combinatorial scenarios
- [ ] Async tests use `#[tokio::test(flavor = "multi_thread")]` to expose data races
- [ ] Assertions include meaningful failure messages (`.expect("msg")`, `assert!(cond, "msg")`, or `pretty_assertions`)
- [ ] No `tokio::time::sleep` for synchronization — use channels, `tokio::sync::Barrier`, `watch` channels, or `CancellationToken` for coordination
- [ ] Tests that spawn tasks verify cleanup (no leaked tasks, no dangling `JoinHandle`s)
- [ ] Property-based tests via `proptest` or `quickcheck` for complex invariants

#### Commit: Tests

```
test(crate): define behavior for [feature]

- Happy path: [describe]
- Error cases: [describe]  
- Edge cases: [describe]
- All tests currently FAIL (no implementation yet)
```

### Phase 3: Implement

Write the minimum code to make all tests pass:

- Don't add features beyond what the tests require.
- Don't add abstractions for hypothetical future needs.
- Don't optimize prematurely — correctness first.
- Run `cargo test --all-features` after every meaningful change.
- Run `cargo test --all-features -- --test-threads=1` if order-dependent failures are suspected.

#### Implementation Checklist

- [ ] All tests pass (`cargo test --all-features`)
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] All async functions and shared types are `Send + Sync` where required
- [ ] Error types use `thiserror` (library) or `anyhow` (application) with `.context()` for operation context
- [ ] Resources cleaned up via RAII — `Drop` impls, `scopeguard`, or `_drop_guard` patterns
- [ ] No `unsafe` without a documented safety invariant and a safety comment
- [ ] No `unwrap()`/`expect()` in library code — propagate errors
- [ ] All `JoinHandle`s are awaited or explicitly detached with documented rationale
- [ ] No TODO/FIXME left without a tracking issue
- [ ] **I/O uses io_uring where applicable** — no new classical FUSE/file fallbacks; FUSE request path stays over-uring after arm
- [ ] **No dead code** — unused items deleted; no new `#[allow(dead_code)]` / unused allows to hide them

#### Commit: Implementation

```
feat(crate): implement [feature]

- All tests passing
- Clippy clean, fmt clean
- [Brief note on approach taken]
```

### Phase 4: Refine, Harden, & Benchmark

With green tests as your safety net:

- Refactor for clarity if the implementation is messy.
- **Mandatory Benchmarking**: EVERY public function must have a corresponding Criterion benchmark. Performance is a first-class feature.
- Profile allocations for data-plane code via Criterion's allocation measurements.
- Run `cargo bench` to record historical performance; commit results to `.benchmarks/` or use `cargo-criterion` with `--save-baseline`.
- Run `cargo loom` on lock-free or highly concurrent data structures to exhaustively check memory-ordering correctness.
- Add any additional edge case tests discovered during implementation.

#### Commit: Refinements

```
refactor(crate): [what changed and why]
```
or
```
perf(crate): optimize [hot path] - [result]
```

### Phase 5: Integration Verification

Before considering work complete:

1. `cargo build --all-targets --all-features` — full project compiles.
2. `cargo test --all-features` — all tests pass project-wide.
3. `cargo clippy --all-targets --all-features -- -D warnings` — zero warnings.
4. `cargo fmt --check` — formatting is clean.
5. `cargo doc --no-deps` — documentation builds without warnings.
6. `cargo audit` — no known vulnerabilities in dependencies.
7. Review the diff: every changed line traces to the original goal.

If a `task check` or `cargo xtask check` target exists, run it — it is the authoritative quality gate.

**Required verification gate (must pass before commit):**

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
cargo test --all-features -- --test-threads=1
cargo doc --no-deps
```

### Phase 6: Merge to `dev` & Cleanup

After all checks pass, merge the feature branch back into `dev` and delete it:

```bash
git checkout dev
git merge --ff-only feat/short-description
git branch -d feat/short-description
```

- **Always `--ff-only`**: If `dev` has diverged, rebase the feature branch first (`git rebase dev`). No merge commits for linear history.
- **Delete the branch**: Once merged, the branch serves no purpose. Delete it immediately.
- **Push**: `git push origin dev` to sync remote.

### Git Discipline

- **`dev` is the integration branch**: All feature/fix branches start from and merge back into `dev`. `main` is reserved for releases.
- **Atomic commits**: One logical change per commit. Tests and implementation can be separate commits.
- **Conventional format**: `type(scope): description` — types: `feat`, `fix`, `test`, `refactor`, `perf`, `docs`, `chore`.
- **No direct commits to `dev` or `main`**: Always use a branch.
- **Commit messages explain WHY**, not just what. The diff shows what changed; the message explains the reasoning.
- **Delete branches after merge**: Stale branches are clutter. Merge → delete → move on.

### Anti-Patterns to Avoid

| Anti-Pattern | Correct Approach |
|---|---|
| Writing code first, tests after | Write tests first — they define the contract |
| Testing only the happy path | Cover errors, boundaries, concurrency, cancellation, backpressure, shutdown |
| Giant commits with tests + code + refactor | Separate commits for tests, implementation, refinement |
| "Improving" unrelated code while fixing a bug | Touch only what the goal requires |
| Skipping concurrency tests because "it's simple" | Always use `multi_thread` flavor. Simple async code has races too |
| Using `tokio::time::sleep` for test synchronization | Use channels, `Barrier`, `watch`, or `CancellationToken` |
| Speculative abstractions | Build what's needed now. Refactor when a real pattern emerges |
| `unwrap()` in library code | Propagate errors with `?` and `thiserror` |
| `unsafe` without safety comments | Document invariants; prefer safe alternatives |
| Blocking the async runtime | Use `tokio::task::spawn_blocking` for CPU-bound or blocking I/O |
| Unbounded channels in production code | Use `mpsc::channel(bound)` with explicit backpressure |
| Leaking tasks (fire-and-forget `spawn` without tracking) | Use `JoinSet` or structured concurrency patterns |
| Ignoring `Send + Sync` bounds | Design types to be `Send + Sync` from the start; document why if not |
| Falling back to classical `/dev/fuse` or POSIX file I/O to dodge an uring bug | **Always use io_uring when we can.** Fix the uring path or fail the mount/op — never reintroduce classical escape hatches |
| Leaving unused code with `#[allow(dead_code)]` “for later” | **Delete dead code.** Restore from git when needed; keep clippy `-D warnings` clean by removal |

---

## Build

System deps (Ubuntu/Debian): `build-essential pkg-config libfuse3-dev fuse3 clang libclang-dev`. The build links FUSE 3 and uses `io-uring` + `/dev/fuse` — it will not compile on non-Linux.

```bash
cargo build --release
```

### Optional Cargo features (off by default)
- `gds` — GPU Direct Storage RDMA path (pulls `libloading`).
- `dhat-on` — heap profiling (`dhat`); a static `dhat::Alloc` replaces the global allocator in `src/main.rs`.
- `coz-on` — causal profiling; the `coz_progress!` macro (defined in `src/lib.rs`) becomes active.

`[profile.release]` keeps `debug = true` so release binaries carry symbols for profiling. `tikv-jemallocator` is the global allocator on Linux.

### Patched `fuse3` dependency

`Cargo.toml` has `[patch.crates-io] fuse3 = { path = "third_party/fuse3" }`. The vendored copy in `third_party/fuse3/` is patched locally — do not replace it with the crates.io version or "restore" `Cargo.toml.orig`. If FUSE behavior changes, edit the vendored source.

### Build features for profiling

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

---

## Testing

Most integration tests talk to a live **Garnet/Redis** instance. Behavior splits two ways — check before you run:

- **Graceful skip** when Redis is down (safe): `dlm_tests.rs`, `defrag_tests.rs`, `recovery_tests.rs`, `failover_tests.rs`, both `benches/*.rs`.
- **Will panic / fail** without Redis: `format_tests.rs`, `block_allocator_tests.rs`, `jobs_tests.rs`, `lock_contention_tests.rs`, `metadata_sharding_tests.rs`, `checkpoint_debug.rs`. These unwrap the Redis connection or assert on results — start Garnet first.

Quickest way to get Garnet running:
```bash
podman run -d --rm --replace --name squeezefs-garnet -p 6379:6379 ghcr.io/microsoft/garnet:latest
```
`redis-server` works too.

### Redis-DB isolation across tests (do not "fix" these by changing DBs)

- `format_tests.rs` → db `9`
- `metadata_sharding_tests.rs` → dbs `1`, `2`, `3`
- `block_allocator_tests.rs`, `jobs_tests.rs`, `lock_contention_tests.rs` → **default db 0** (these will clobber live mount metadata; do not run against a Garnet instance backing a mounted volume).

`GARNET_URL` env var overrides the default `redis://127.0.0.1:6379` in `dlm_tests.rs`, `recovery_tests.rs`, and `benches/*.rs`. Other tests hardcode the URL and ignore it.

Run everything:
```bash
cargo test --all-features -- --test-threads=1
```
`--test-threads=1` is used because several suites share Garnet state and are order-sensitive; use it when an unknown test starts failing locally.

Run a single test:
```bash
cargo test --all-features --test dlm_tests -- test_lock_acquisition_and_release --exact
```

### Required verification gate (every commit)

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
cargo test --all-features -- --test-threads=1
cargo doc --no-deps
```

### External POSIX / IO suites (require root + a mounted FS)

These are **not** part of `cargo test`. Run as **root** on Linux/WSL after material write-path, layout, or FUSE lock changes:

```bash
# POSIX compliance (mounted squeezefs)
sudo tests/run_pjdfstest.sh

# Mount-level IO benchmark
sudo tests/run_elbencho_mount.sh
```

Prerequisites: Garnet up, volume formatted/mounted per `QUICKSTART.md`. Failures here can pass pure unit tests and still indicate mount regressions.

---

## Benchmarks & Profiling

### Criterion benches

Two Criterion benches, both `harness = false`:

- `cargo bench --bench high_concurrency_bench` — in-memory lock / pool / cache contention; no Redis required.
- `cargo bench --bench squeezefs_bench` — exercises the full FS stack; requires Garnet (skips gracefully if absent). Also contains the `CryptoCompressState` compression/encryption micro-benches.

**CI note:** Criterion is optional in PR CI (long / noisy). Prefer **nightly** or manual baseline save:

```bash
cargo bench --bench squeezefs_bench -- --save-baseline main
# later: --baseline main
```

### Profiling command set (release)

#### CPU (perf)

```bash
# One-shot sample while a mount + workload runs
perf record -g --call-graph dwarf -o perf.data -- \
  target/release/squeezefs mount …   # or attach: -p $(pidof squeezefs)

perf report -i perf.data
# or: perf script | inferno-collapse-perf | inferno-flamegraph > flame.svg
```

#### Heap (dhat)

```bash
cargo build --release --features dhat-on
# Run workload; on exit dhat prints / writes heap profile per its config
DHAT_OUT=dhat-heap.json target/release/squeezefs mount …
```

#### Causal (coz)

```bash
cargo build --release --features coz-on
coz run --- target/release/squeezefs mount …
```

Use coz/dhat **after** a known-good cargo test gate, against a representative mount (Garnet + backing file or NVMe).

---

## Module Map (non-obvious wiring)

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
- The architecture uses the **NVMe / NVMe-oF block** backend as the sole primary data path.

## Stats surface

Mounted volumes expose process metrics under the virtual **stats** inode (JSON), including layout mix, bg admission, uring queue-full, and lease acquire outcomes. Prefer these for live regression signals over ad-hoc logging.

---

## Branch & Commit Workflow

The integration branch is **`dev`** (not `main`; `main` is reserved for releases). `origin/HEAD` points at `origin/dev`.

- Branch from `dev`: `feat/`, `fix/`, `refactor/`, `perf/`, `docs/` prefixes. Never commit directly to `dev`.
- Tests-first cycle: write failing tests → implement → refine → commit each logical step separately.
- Merge with `--ff-only`; rebase feature branches if `dev` has diverged. Delete branches after merge.
- Conventional commits: `type(scope): description`. Messages explain WHY.

See the full TDD Development Workflow section above for the detailed phased process.

---

## Authoritative Supporting Documents

While this file is the combined one source of truth for agents and contributors:

- `README.md` / `QUICKSTART.md` — user-facing CLI, NVMe-oF, and bare-metal setup.
- `.agents/walkthrough.md` — notes on the transparent compression / encryption implementation.
- `docs/PROFILING_AND_GATES.md` (historical) — content now consolidated here.

---

**End of combined AGENTS.md.** Follow these instructions exactly. When working in subdirectories, check for additional project instruction files (AGENTS.md, Claude.md, etc.) but prefer this root document.
