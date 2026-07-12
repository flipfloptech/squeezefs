# AGENTS.md — Squeezefs One Source of Truth

Squeezefs is a high-performance distributed POSIX FUSE filesystem (Rust + tokio + io_uring) with a decoupled block-based logical volume metadata store backend (MetaLV) and an NVMe / NVMe-oF block data backend. Linux-only.

**This combined document is the single authoritative reference.** It merges architectural rules, non-negotiables, development workflow, build/test gates, profiling, and repo conventions. Read it before touching core logic.

---

## Non-Negotiables

### Always use io_uring when we can

**Policy for agents and humans:** prefer and require **io_uring** for every I/O path where the Linux kernel can do it. Do **not** “temporarily” fall back to classical `read`/`write`/`pread`/`pwrite`/`/dev/fuse` polling as a way to unblock a bug. Fix the uring path, or fail loud.

| Path                              | Expectation |
|-----------------------------------|-------------|
| **FUSE request hot path**         | **FUSE-over-io_uring only** after arm (`REGISTER` / `COMMIT_AND_FETCH`). No userspace opt-out. Mount fails if setup fails. |
| **FUSE_INIT + classical sideband** | Classical `/dev/fuse` for `FUSE_INIT` (kernel requires `fch->initialized` before REGISTER), then the **kernel-mandated classical sideband** only: the kernel keeps FORGET/BATCH_FORGET + INTERRUPT + `fuse_resend` resends + `fiq->ops` switchover stragglers on the classical queue even when armed (fs/fuse/dev_uring.c). A dedicated sideband session services them — **via io_uring `Readv`** — or they strand forever (`waiting ≥ 1`, umount EBUSY). All other requests stay over-uring. |
| **NVMe / block data**             | `NvmeBlockDev` io_uring workers (fixed files when available). |
| **Ad-hoc local files**            | `crate::uring_fs` (not std file APIs) where practical. |
| **Not uring**                     | TLS peers, network TCP/TLS stacks. Staging **mmap** segments stay mmap by design. |

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
| FUSE transport (default) | `fuse3` `BlockFuseConnection` (classical rings during INIT); **FUSE-over-io_uring** (`IORING_OP_URING_CMD` + REGISTER/COMMIT_AND_FETCH) after arm; post-arm **classical sideband session** (io_uring `Readv` on `/dev/fuse`) for kernel-mandated FORGET/INTERRUPT/resend traffic + switchover stragglers |
| FUSE_WRITE payload delivery | Zero-copy **payload lease** (`Bytes::from_owner` over the registered uring payload buffer) with deferred COMMIT_AND_FETCH re-arm; the lease-severance boundary bounds every lease to one handler invocation — `docs/design-zero-copy-write-path.md` §5.4 |
| Staging / read-segment hot path | **mmap** (by design — zero syscall) |
| TLS peers | Not uring (network) |

Hardening: pool not marked ready until all queues submit REGISTER; per-qid commit channels; shared inbound queue; eventfd wake; full-size payload buffers.

### No dead code

**Do not leave unused code in the tree.** Agents and humans must remove it, not silence it.

- **Delete** unused functions, methods, fields, imports, constants, modules, and feature-gated stubs that nothing calls.
- **Do not** paper over dead code with `#[allow(dead_code)]`, `#[allow(unused)]`, or broad `allow` attributes “for later.” If it is not used now, delete it; restore from git when needed.
- **Clippy/gate:** `cargo clippy --all-targets --all-features -- -D warnings` must stay clean — that includes unused items. Fix by **removing** dead code, not by allowing warnings.
- **Exceptions only** when the item is part of a public API surface that must stay stable (`pub` for crates/downstream) or is required for `#[cfg]` / trait impl completeness and truly cannot be omitted — document why in a one-line comment on that item. Prefer not exporting unused symbols.

### Zero-copy and latch-free data paths

**Policy for agents and humans:** maintain and enforce a zero-copy, latch-free hot-path design. Do not introduce traditional blocking locks (like standard `Mutex` or `RwLock`) or unnecessary data copy operations on the primary read/write data path.

- **Zero-Copy Hot Path:** Hot staged files are mapped directly using memory mapping (`mmap`). Buffer segments must be returned or updated in-place via pointer/slice references. Avoid allocating new buffers or cloning vectors during standard read/write execution.
- **Latch-Free/Lock-Free Caching & Indexes:** Hot metadata tables, directory entry indices, and block routing tables must use lock-free or latch-free data structures (e.g., `scc::HashMap`, sharded atomic clock rings, atomic reference counts). Traditional read/write synchronization locks are only permitted for FUSE operations and metadata/lease transactions (see Lock Order constraints).
- **Zero-Copy GPU Direct (GDS):** When `gds` feature is active, bypass host RAM completely. Copy block data directly between NVMe/NVMe-oF and GPU memory via RDMA.
- **Zero-copy write path (normative design):** the large-write hot path is **1 userspace copy + 1 DMA** — transport payload leases (no per-request copy/alloc), one merge copy into the exclusive-owner `ActiveBlockBuf`, complete-block write-through past staging, guard-backed DMA for residual staged flushes. Design + acceptance evidence: `docs/design-zero-copy-write-path.md` and `.benchmarks/2026-07-08-zero-copy-write-path-closing.md`. Do not reintroduce copies or staging detours on this path.

---

## High-Performance Distributed Filesystem Architecture

### Target Scale & Layout
* **Scale:** 15,000+ Concurrent Nodes.
* **Architecture Type:** Decoupled metadata (MetaLV) + **block data** (local NVMe / NVMe-oF), exposed via POSIX FUSE.

### Technology Stack
* **Client Daemon (FUSE Engine):** Built in **Rust** using the asynchronous `tokio` runtime and `io_uring` polling over `/dev/fuse`.
* **Metadata & Distributed Lock Manager (DLM):** Local or distributed logical volume backend (**MetaLV**). Primary meta store for attrs, layout maps, leases, and volume format.
* **Data Backend (primary):** **NVMe / NVMe-oF block devices** via `NvmeBlockDev` (io_uring workers). Progressive layouts (inline / staged / striped) live on this path.

### Progressive Data Layout & I/O Routing

Logical file growth uses three layouts (thresholds are implementation-defined; current code uses ~4 KiB inline, up to ~4 MiB staged when staging dirs exist, else striped):

1. **Inline (tiny):** Payload in Metadata Volume (`inline_data:…` key/attribute) with type `inline`.
2. **Staged (small):** Local NVMe staging (`file_id` + optional `mapping:…`); writeback/flush promotes to durable blocks.
3. **Striped (large):** 4 MiB (configurable) blocks on the active block backend with `block_map:…` and refcounts.

Writes that grow past thresholds promote layouts **durably** (block I/O before meta type flip — see P0 layout atomicity).

### Distributed Lock Manager (DLM) & Consistency

POSIX FUSE locks map to cluster leases on Metadata Volumes:
* **Acquisition:** Lease locking + fencing token `INCR` (no external distributed database required).
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
* Not uring: TLS peer paths (network). Staging mmap segments stay mmap by design.

### Metadata Cluster Topology

* Keys for volume control use `fs_prefix` / `fs_key!` helpers.
* **Layout keys** (`metadata:…`, `inline_data:…`, `block_map:…`, `mapping:…`, `active_block:…`) are **unprefixed historical** forms — use `crate::keys::*` helpers; do not migrate under `FS_PREFIX` without an on-disk format change.

### Metadata format: v3 CoW KV (the only format)

Format v3 is the **only** on-disk metadata format. Legacy v2 support was removed entirely (user directive: always forward — no backwards compatibility): a v2 superblock refuses to mount loud ("no longer supported; reformat required"), `squeezefs format --force` reformats it to v3, and the offline `squeezefs migrate` converter was deleted with the v2 reader. Normative spec `docs/design-cow-kv-metadata.md`, measured gates `.benchmarks/2026-07-09-kv-v3-gates.md`.

* **v3:** a bcachefs-style **copy-on-write typed-KV btree** (`src/meta_backend/kv/`). One node layer, three logical trees per volume (`TREE_INODES`/`TREE_DENTRIES`/`TREE_XATTRS`), memcmp-ordered keys. **256 KiB log-structured CoW nodes** (format knob `--meta-node-kib`, 64 KiB–1 MiB) with internal sorted-bset appends; a **lock-free reservation journal** (ring `clamp(volume/64, 8 MiB, 32 MiB)`, `--meta-journal-mb` override, **read at every mount**) carrying one checksummed entry per transaction; **checkpoints** flip per-tree roots in an A/B root ledger on the background flush cadence, **never on the commit path**; **monotonic ino allocation** (no reuse, no seeding scan); an **A/B extent-bitmap allocator** (1 bit / 256 KiB extent) with a journaled pending-free protocol. Superblock: magic `METALV01`, version 3, `features_incompat` bit 0 = `KV_V3` (`src/meta_backend/kv/superblock.rs`). Caps: ≥ 100 M inodes, 1 M+ entries/dir, unlimited xattrs (value ≤ `min(65536, node_size/4)` B). Reads stay RAM-authoritative and latch-free (scc node index + arc-swap snapshots, demand-paged, clock eviction). Every on-disk unit is checksummed and copy-on-write: whole-transaction atomicity (one tx = one checksummed journal entry) + torn-write immunity (torn writes detected-and-ignored) — strictly stronger than the retired v2 D0/D1/D2 sector contract (§4.10).
* **Env knobs:** `SQUEEZEFS_META_NODE_CACHE_MB` (node-cache budget, default 512), `SQUEEZEFS_META_CHECKPOINT_MAX_DIRTY_NODES` (dirty-node checkpoint cap = mount-replay working-set bound, default 4096); `SQUEEZEFS_META_FLUSH_INTERVAL_MS` is the journal/checkpoint cadence (`0` = strict per-commit).
* **Stats fields (stats inode):** `meta_format_version` (constant `"3"` per volume — kept because operators key on it); `meta_volume_atomicity` (contract class — constant `cow-checksummed`) alongside `meta_volume_atomicity_physical` (the informational hardware probe); and the `meta_kv_*` family — `node_cache_{hits,misses,evictions}`, `node_appends`, `node_{append,rewrite}_bytes`, `node_{compactions,splits}`, `journal_{bytes,entries,full_stalls}`, `checkpoints`, `commit_smo_retries`, `node_dropped_tail_bsets`, `dentry_collision_overflows`, `delta_orphans`, `replay_{entries,dropped_torn,ms}`, `free_extents`, `pending_free`. The retired v2-only counters (`meta_commit_sectors`, `meta_sector_lock_*`, `meta_inode_alloc_*`, `meta_tx_concurrency*`, `meta_quarantined_inodes`) no longer exist.
* **Filesystem generation identity:** per-volume the v3 superblock `uuid` (random per `format` invocation), joined in volume order (`meta_backend::volume_set_generation`) — local staging is bound to it. (The v2-era `FormatConfig.fs_uuid` config stamp was deleted with v2; the superblock uuid is the sole generation identity.)

### Error Handling & Crash Recovery

* FUSE op timeouts; staging recovery on remount (`recover_staging`) with fence + layout checks.
* Stale fencing tokens discard staged work; missing inode meta discards orphan active blocks.
* Write verification is **opt-in** (`--write-verification`, optional sample rate).
* **Metadata crash contract**: whole-transaction atomicity + torn-write immunity **by construction** (v3 CoW KV — see Metadata format section + `docs/design-cow-kv-metadata.md` §4.10; the historical D0/D1/D2 ladder it replaced is `docs/design-wal-crash-consistency.md` §3).
* Mount probes each meta volume's sector atomicity (sysfs) — purely informational, surfaced as `meta_volume_atomicity_physical` while the contract field `meta_volume_atomicity` reads `cow-checksummed`. (The `--strict-meta-atomicity` flag only ever gated v2 volumes and was deleted with them.)

### Lock order & connection scope (must not)

Always acquire in this order; **never invert** (P1-9):

1. `active_inode_locks` (per-inode `RwLock`) — FUSE op serialization
2. `lease_locks` (per-inode) — only while acquiring/refreshing DLM lease
3. `BLOCK_FLUSH_LOCKS` (per block) — active-block mutation
4. MetaLV metadata-transaction locks, in sub-order:
   - **4a.** DLM `I{ino}` / `D{parent:name}` (per-object; `DlmLockManager`).
   - **4b.** (design-cow-kv-metadata §4.9 4b): **per-node write locks — the commit path takes leaf locks only, in ascending NodeId order, deduped, lock-then-revalidate-then-retry against SMOs; interior-node locks belong exclusively to the serialized per-volume checkpoint/SMO task (parent-then-child), which is what keeps the two lock populations acyclic. Node locks are never held across device I/O (commit apply is RAM-only; the journal entry write happens after unlock; writeback freezes under the lock and appends outside it; SMOs reserve in-window and write after release) and never held while waiting on ring space (ring admission happens before any node lock — §4.4 pt 5; the checkpoint task's own admissions never park, they drain-and-retry).**
   - **4c.** The journal reservation — a wait-free atomic, not a lock; ordered inside 4b by protocol, it imposes no ordering edges.

**Must not:**
* Hold inode **write** guard across long backend I/O when block locks suffice (striped data path = meta-prep only under write lock — P1-8).
* Hold a **pooled MetaLV connection/handle** across durable NVMe / staging I/O (P1-10: open → short meta → drop → I/O → re-acquire for commit).
* Acquire (1) while holding (3).
* Burn fencing tokens on failed lock acquisition.

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
- The intentional classical FUSE uses are the one-shot **`FUSE_INIT`** exchange (kernel requires it before REGISTER) and the post-arm **classical sideband session** for the traffic the kernel refuses to put on the ring (FORGET/INTERRUPT/resends + `fiq->ops` switchover stragglers) — serviced via io_uring `Readv`, never as a hot-path fallback. All other requests/replies are over-uring only after arm.
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
cargo bench --benches -- --test   # criterion smoke: one iteration per bench, no measurement
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

Most integration tests execute against local file-backed MetaLV sandboxes.

### Required verification gate (every commit)

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
cargo test --all-features -- --test-threads=1
cargo doc --no-deps
cargo bench --benches -- --test   # criterion smoke: one iteration per bench, no measurement
```

### External POSIX / IO suites (require root + a mounted FS)

These are **not** part of `cargo test`. Run as **root** on Linux/WSL after material write-path, layout, or FUSE lock changes:

```bash
# LTP filesystem syscall tests
sudo tests/run_ltp_syscalls.sh

# fstests filesystem checks
sudo tests/run_fstests.sh

# Mount-level IO benchmark
sudo tests/run_elbencho_mount.sh
```

Prerequisites: Volume formatted/mounted per `QUICKSTART.md`. Failures here can pass pure unit tests and still indicate mount regressions.

### Test tiering (do not run acceptance suites at per-commit cadence)

fstests/LTP are **wall-clock-bound** (fixed-duration fsx/fsstress soaks, mount-cycle overhead) — a full `-g auto` is ~5 h regardless of CPU speed. Running them per fix wastes hours re-failing tests already known to fail. Use three tiers:

| Tier | What | When | Cost |
|------|------|------|------|
| **Per-commit** | the cargo gate (clippy/fmt/`test --test-threads=1`/doc/bench-smoke; loom when a lock-free core changes) | every commit | ~5 min |
| **Per-PR (data-path)** | `FSTESTS_QUICK=1 sudo tests/run_fstests.sh` — the curated `SQUEEZEFS_FSTESTS_QUICK` regression set + `sudo tests/run_ltp_syscalls.sh` | PRs touching the write/read/layout/FUSE paths | ~15–20 min |
| **Nightly / release-gate** | full `sudo tests/run_fstests.sh` (`-g auto`), full LTP, `sudo tests/run_elbencho_mount.sh`, `long_validation.py` scale mounts | nightly + closing gates (e.g. the K7 §8 gate) | hours, unattended |

**`SQUEEZEFS_FSTESTS_QUICK`** (defined in `tests/run_fstests.sh`) is the **standing regression set**: every fstests case that has ever caught a real SqueezeFS bug, plus core fsx/fsstress soak, hole/punch/seek coverage, and mount basics. **Grow it whenever a new test surfaces a bug** — that is the point of the tier. When an external suite catches a bug, also **port the scenario into a fast `cargo` test** (the `tests/*_tests.rs` layer) so the per-commit tier gains the coverage permanently.

**Fix-loop discipline (inventory once, then targeted):**
1. **Inventory once** — one full `-g auto` produces the complete failure list. Do **not** re-run the full suite between fixes.
2. **Targeted fix loop** — per failure *family* (cluster related failures; one root cause often spans several tests): tests-first fix → verify the single case with `sudo tests/run_fstests.sh generic/NNN` (minutes) → merge.
3. **One final sweep** — a single full `-g auto` after the last fix (and nightly thereafter) to catch fix interactions.

---

## Benchmarks & Profiling

### Criterion benches

Two Criterion benches, both `harness = false`:

- `cargo bench --bench high_concurrency_bench` — in-memory lock / pool / cache contention.
- `cargo bench --bench squeezefs_bench` — exercises the full FS stack. Also contains the `CryptoCompressState` compression/encryption micro-benches.

**Bench smoke is part of the required gate:** `cargo bench --benches -- --test` runs one iteration of every bench without measurement (seconds, not minutes). It exists because `meta_lv_bench` sat silently broken from PR 8 until a perf investigation tripped over it — a panicking bench must fail the gate, not wait for the next baseline run.

**CI note:** Full Criterion *measurement* is optional in PR CI (long / noisy). Prefer **nightly** or manual baseline save:

```bash
cargo bench --bench squeezefs_bench -- --save-baseline main
```

### Profiling command set (release)

#### CPU (perf)

```bash
# One-shot sample while a mount + workload runs
perf record -g --call-graph dwarf -o perf.data -- \
  target/release/squeezefs mount …   # or attach: -p $(pidof squeezefs)

perf report -i perf.data
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

Use coz/dhat **after** a known-good cargo test gate, against a representative mount.

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

- **Cache-path policy (mount can never override):** staging/cache directories (`--disk-cache-paths`) are declared at **format** and recorded in the format config — the single source of truth. `mount` reads them from the config and **rejects** the flag with a loud error; format without the flag ⇒ a permanently **cache-less** filesystem (RAM tiers + direct block I/O; beyond-inline writes route striped — no staged layout, no conjured default staging dir). Changing paths is the admin op `squeezefs config set-cache-paths <sqmeta-uri> <paths...>` (format-grade live-client refusal; wipes the new dirs so generation stamping starts clean) with `get-cache-paths` for reads. Contracts pinned in `tests/cache_path_policy_tests.rs`.
- Metadata keys are namespaced via the `fs_key!("suffix")` macro and the global `FS_PREFIX` (`src/lib.rs`). Code that touches metadata keys must go through the macro, not hardcode prefixes.
- `WRITE_VERIFICATION` is a process-global `AtomicBool` toggled by `--write-verification` on mount; read-after-write checksum verification uses it. Library code should call `write_verification_enabled()` rather than reading CLI args.
- The architecture uses the **NVMe / NVMe-oF block** backend as the sole primary data path.

## Stats surface

Mounted volumes expose process metrics under the virtual **stats** inode (JSON), including layout mix, bg admission, uring queue-full, and lease acquire outcomes. Prefer these for live regression signals over ad-hoc logging. Per-volume metadata format + durability fields (`meta_format_version`, `meta_volume_atomicity[_physical]`) and the `meta_kv_*` family are listed under **Metadata format: v3 CoW KV (the only format)**.

**Read-path program families** (`docs/design-read-path.md` §Observability — semantics + regression thresholds there): `singleflight_waiter_result_serves` (R1a cohort serves); `hot_block_{hits,misses,evictions,probation_drops,dehydrate_skips,current_bytes}` (R4 RAM tier); `read_fill_publishes_skipped` / `read_tier_admissions` / `read_tier_admission_ghost_hits` / `read_tier_admission_mode` (R1b second-touch admission — skipped ≈ streamed cold blocks); `prefetch_{issued,completed,wasted,inflight_bytes,window_hwm,foreground_waits,evicted_unconsumed,active_streams}` (R2 pipeline — `evicted_unconsumed` is the refetch-spiral detector); `ranged_{reads,read_bytes,read_unaligned_bounces,read_rebinds}` (R3 — `ranged_read_bytes` vs user bytes is the rand-4k amplification bound; `get_obj` counts ranged ops by design); `mem_budget_{bytes,pressure_bytes,gauge_sum_bytes,level,yellow_events,red_events,floors_clamped,dehydrate_paused,components{…}}` (R5 authority — red_events with no OOM is the designed outcome under pressure).

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
