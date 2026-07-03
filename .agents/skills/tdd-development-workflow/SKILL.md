---
name: tdd-development-workflow
description: Test-Driven Development workflow for high-performance, highly parallel, asynchronous Rust. Tests are written first to define correct behavior, then code is written to satisfy them. Every logical unit of work is committed. Edge cases are not optional.
---

# TDD Development Workflow

A disciplined, test-first development workflow for building industrial-grade, high-performance, highly scalable, asynchronous Rust systems. Tests define the contract. Code fulfills it.

## Core Principle: Tests First, Code Second

Every feature, bugfix, or refactor follows the same cycle:

1. **Define** — What should the correct behavior be?
2. **Test** — Write tests that assert that behavior (they will fail).
3. **Implement** — Write the minimum code to make the tests pass.
4. **Refine** — Refactor for clarity and performance while tests stay green.
5. **Commit** — Each logical step gets its own git commit.
6. **Merge** - Bring the code base back into the primary branch.

## Phase 0: Branch from `dev`

All work happens on feature/fix branches off `dev`. Never commit directly to `dev` or `main`.

```bash
git checkout dev
git pull origin dev
git checkout -b feat/short-description   # or fix/short-description
```

- **Branch naming**: `feat/`, `fix/`, `refactor/`, `perf/`, `docs/` prefixes matching conventional commit types.
- **One branch per logical unit of work**: a feature, a bugfix, or a hardening pass.

## SqueezeFS I/O constraint (always io_uring when we can)

This repo is Linux + **io_uring**-first. When planning or implementing:

- **Default to io_uring** for FUSE request traffic (FUSE-over-io_uring after arm), NVMe/block I/O (`NvmeBlockDev`), and local file I/O (`crate::uring_fs`) whenever the kernel can support it.
- **Do not** introduce or re-enable classical `/dev/fuse` or POSIX file I/O fallbacks to “make it work.” That is an anti-pattern here. Fix the uring path or fail loud.
- The **only** intentional classical FUSE use is the one-shot **`FUSE_INIT`** exchange (kernel requires it before REGISTER). After arm, requests/replies are over-uring only.
- If a failing test tempts you to disable over-uring or switch to blocking `read`/`write`, stop — write a failing test that encodes correct uring behavior, then fix uring.

Authoritative detail: root `AGENTS.md` (“Non-negotiable: always use io_uring” / “no dead code”) and `.agents/AGENTS.md`.

### No dead code

- Remove unused functions, fields, imports, and modules as part of the same change that made them unused.
- **Never** add `#[allow(dead_code)]` / `#[allow(unused_*)]` to park unused code. Delete it; git has history.
- Clippy with `-D warnings` failing on unused items means **delete**, not allow.

## Phase 1: Understand & Plan

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

## Phase 2: Write Tests

Write the test file(s) BEFORE any implementation:

- **Happy path**: The expected, normal-operation case.
- **Error paths**: What happens when inputs are invalid, connections drop, `CancellationToken` fires, channels close?
- **Boundary conditions**: Empty inputs, maximum values, zero-length streams, single-node mesh.
- **Concurrency**: Race conditions under parallel access. Multiple tokio tasks hitting the same state. Use `#[tokio::test(flavor = "multi_thread", worker_threads = N)]` to stress shared state.
- **Timeouts & Cancellation**: `tokio::time::timeout`, `CancellationToken` propagation, slow peers, hung connections.
- **Backpressure**: Channel saturation, bounded queue overflow, consumer lag.
- **Shutdown**: Graceful drain under in-flight work, partial completion, resource cleanup on abort.

### Test Quality Checklist

- [ ] Each test has a clear, descriptive name (`test_broadcast_storm_uuid_dedup`)
- [ ] Tests are independent — no shared mutable state between test cases
- [ ] Parameterized tests via `rstest` (`#[case]`, `#[values]`) for combinatorial scenarios
- [ ] Async tests use `#[tokio::test(flavor = "multi_thread")]` to expose data races
- [ ] Assertions include meaningful failure messages (`.expect("msg")`, `assert!(cond, "msg")`, or `pretty_assertions`)
- [ ] No `tokio::time::sleep` for synchronization — use channels, `tokio::sync::Barrier`, `watch` channels, or `CancellationToken` for coordination
- [ ] Tests that spawn tasks verify cleanup (no leaked tasks, no dangling `JoinHandle`s)
- [ ] Property-based tests via `proptest` or `quickcheck` for complex invariants

### Commit: Tests

```
test(crate): define behavior for [feature]

- Happy path: [describe]
- Error cases: [describe]  
- Edge cases: [describe]
- All tests currently FAIL (no implementation yet)
```

## Phase 3: Implement

Write the minimum code to make all tests pass:

- Don't add features beyond what the tests require.
- Don't add abstractions for hypothetical future needs.
- Don't optimize prematurely — correctness first.
- Run `cargo test --all-features` after every meaningful change.
- Run `cargo test --all-features -- --test-threads=1` if order-dependent failures are suspected.

### Implementation Checklist

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

### Commit: Implementation

```
feat(crate): implement [feature]

- All tests passing
- Clippy clean, fmt clean
- [Brief note on approach taken]
```

## Phase 4: Refine, Harden, & Benchmark

With green tests as your safety net:

- Refactor for clarity if the implementation is messy.
- **Mandatory Benchmarking**: EVERY public function must have a corresponding Criterion benchmark. Performance is a first-class feature.
- Profile allocations for data-plane code via Criterion's allocation measurements.
- Run `cargo bench` to record historical performance; commit results to `.benchmarks/` or use `cargo-criterion` with `--save-baseline`.
- Run `cargo loom` on lock-free or highly concurrent data structures to exhaustively check memory-ordering correctness.
- Add any additional edge case tests discovered during implementation.

### Commit: Refinements

```
refactor(crate): [what changed and why]
```
or
```
perf(crate): optimize [hot path] - [result]
```

## Phase 5: Integration Verification

Before considering work complete:

1. `cargo build --all-targets --all-features` — full project compiles.
2. `cargo test --all-features` — all tests pass project-wide.
3. `cargo clippy --all-targets --all-features -- -D warnings` — zero warnings.
4. `cargo fmt --check` — formatting is clean.
5. `cargo doc --no-deps` — documentation builds without warnings.
6. `cargo audit` — no known vulnerabilities in dependencies.
7. Review the diff: every changed line traces to the original goal.

If a `task check` or `cargo xtask check` target exists, run it — it is the authoritative quality gate.

## Phase 6: Merge to `dev` & Cleanup

After all checks pass, merge the feature branch back into `dev` and delete it:

```bash
git checkout dev
git merge --ff-only feat/short-description
git branch -d feat/short-description
```

- **Always `--ff-only`**: If `dev` has diverged, rebase the feature branch first (`git rebase dev`). No merge commits for linear history.
- **Delete the branch**: Once merged, the branch serves no purpose. Delete it immediately.
- **Push**: `git push origin dev` to sync remote.

## Git Discipline

- **`dev` is the integration branch**: All feature/fix branches start from and merge back into `dev`. `main` is reserved for releases.
- **Atomic commits**: One logical change per commit. Tests and implementation can be separate commits.
- **Conventional format**: `type(scope): description` — types: `feat`, `fix`, `test`, `refactor`, `perf`, `docs`, `chore`.
- **No direct commits to `dev` or `main`**: Always use a branch.
- **Commit messages explain WHY**, not just what. The diff shows what changed; the message explains the reasoning.
- **Delete branches after merge**: Stale branches are clutter. Merge → delete → move on.

## Anti-Patterns to Avoid

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