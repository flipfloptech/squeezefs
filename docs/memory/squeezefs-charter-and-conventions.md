# SqueezeFS-Zig charter and code conventions
What SqueezeFS is, the charter's non-negotiables, delivery order, release gate, performance bars, and the repository's coding/testing conventions (modules, naming, knobs, rails, counters, sync primitives, logging, model checking, flaky-test hygiene).

## System shape
- From-scratch Zig implementation of a high-performance distributed POSIX filesystem: FUSE-over-io_uring client daemon, shared CoW typed-KV metadata volumes ("MetaLV"), 4 MiB striped data blocks, every RW mount an equal metadata writer.
- Charter: `docs/charter.md` (canonical `~/Source/squeezefs/docs/zig-rewrite-charter.md`), sections 0–13: mission, non-negotiables, system shape, platform/custom kernel, FUSE-over-io_uring, metadata plane incl. MetaLV, commit pipeline, checkpoints, routing, symmetric metadata, refcounts, single-writer guard, data plane, client bypass, operations, Zig concurrency model, observability, engineering discipline, Zig conventions, delivery order.
- Pending user action: the canonical charter in the read-only reference repo still lacks amendments our `docs/charter.md` carries — §4/§9 thread-class names (`sqz-fuse-ln<N>`/`sqz-fuse-ur<qid>`), the §5.1 PAD clause ('entry kind PAD'), the §9 timers/fd-watch bullet, §5.1 sqzhash128, §5.2 parked write stamps. The reference repo was deliberately left untouched; apply only when the user says so.

## Non-negotiables
- Performance is the terminal requirement; Linux-only, io_uring everywhere; zero-copy latch-free hot paths.
- Owner ruling 2026-10-11: "our credence is performance first; amplifications are not performant." A measured amplification — write, read, or metadata journal padding — is a product defect to fix red-first with a derived bound (formula in 0003 §J/§K/§L), a `.stats` closure identity (device bytes ≤ bound × user bytes), and a bracket-harness verdict — never a "known, not fixed" reporting line. When a subagent reports one instead of fixing it, assign the fix at once.
- Read side: every device-read arm needs a bytes counter, `device_read ÷ user_read` is a ledger identity per access shape with a derived bound; the charter blesses only the ranged 4 KiB-outward DMA on a random ≤ 256 KiB passthrough miss. Metadata node bytes per op are bounded too. Format changes are allowed pre-release (decision 0002).
- Size-neutral write law (charter §5.2): a size-neutral write(2) produces ZERO journal entries — its (size,mtime,ctime) stamp is parked (`park_write_times`) and drained in batches; a write's stamp never regresses a future utimes mtime (signed-max fold).
- Every constant derived, with drift tests. `SQUEEZEFS_*` knob registry refuses out-of-range values (never clamps).
- Forward-only on-disk format among Zig releases (§1.7). Fail-loud must-stay-0 tripwires. No dead code. No third-party async runtime.
- Zero acked-data loss on kill -9 — the promise is at the write(2) grain, so kill-row oracles must judge at the write(2) grain and know the writer's syscall pattern. Red-first TDD.
- No classical pread/pwrite fallback (uring_fs has none).
- No compatibility obligation to the Rust reference (decision 0002); charter mechanisms are law, reference choices are not.
- Substrate gaps (owner 2026-10-12): when the venue lacks a capability the design needs (e.g. the kernel's tls ULP), rebuild/fix the substrate rather than add a silent userspace fallback; missing-capability paths fail loud by name.

## Delivery order (charter §13, each rung red-first, gated)
1. io_uring file layer (O_DIRECT metadata posture, aligned pad law) + NVMe block workers + memory budget authority.
2. MetaLV: codecs + ONE fold algebra → node format → journal ring/reservation (model-checked) + ledger → extent allocator → btree/node cache/SMO → superblock/format → commit conveyor + durability lane + journal lane + checkpoint → leaf merge/ENOSPC → monotonic inos.
3. Own FUSE library (executor, INIT, geometry, ring transport, slot state machine, leases, drain groups, sideband, notify).
4. Routing + layouts + write/read paths (data plane).
5. Dynamic routing W=2^16, D0 guard, stats inode, CLI.
6. Symmetric metadata plane.
7. LD_PRELOAD shim + SDK.
8. Lifecycle/jobs/fsck/defrag/nvmet/keys/zcrx.
9. Release gates, fuzz campaign, baselines, the box brackets.

## Release gate (charter ~line 285, §11)
- pjdfstest + full LTP + full fstests `-g auto` all passing (by-design exceptions: noatime, thin provisioning, ±292-year i64 timestamps), require-mount/capability-gated suites as hard failures, the nvmeof fidelity tier, nightly fuzz, and a committed microbench baseline with per-group regression thresholds.
- Multi-run discipline: xfstests `-g auto` ×3 consecutive clean and LTP syscalls ×3 on the FINAL binary; a failure on any early run aborts the count and acceptance counts restart from zero on the fixed binary.
- Box brackets need reference hardware; zcrx against a real HDS NIC and PR preempt on real substrate are hardware-owed.

## Performance bars (32-core client, 5 nvme-tcp targets)
- Read 43.9 GB/s, write 36.4 GB/s, 942k read IOPS, 727k write IOPS; kernel-FUSE rand-4k ≥ 555k IOPS; creates ≥ 32k/s.

## Zig conventions (charter)
- Explicit allocators; extern structs with endian fields + comptime size asserts + a checksum on every unit (sqzhash128 via `core/checksum.zig` since format v5 — see the sqzhash128 topic); typed error classes; io_uring via own bindings; the metadata-transaction class is the only place a blocking mutex is allowed (`std.Io.Mutex` in Zig 0.17); in node_cache.zig mutexes are miss-path only, hot reads via ArcSwap.
- dist profile = ReleaseSafe + fat LTO (decision 0003 Q8). Product binary + SDK libs default to ReleaseSafe since efdc88b; tests stay Debug; `-Doptimize` wins.
- Error handling: error unions + tripwires; true bugs abort loud (no panic-guarded tasks).

## Naming conventions
- In `src/data`, write-path arm modules are named by mechanism (`extent_overlay`, `device_overlay`, `accumulate`, `sole_owner_patch`), never by the charter's W-number shorthand (renamed from `patch_w1.zig` in afbbb22). W1/W2 labels belong in doc comments, not filenames.
- `src/stats/stats.zig` holds a census table of module paths that must be updated whenever a src file is renamed; also check `tests/rails/literal-allowlist.txt` rows keyed to the old path and living docs under `docs/porting/` and `docs/decisions/` (evidence notes stay historical).

## Repository modules
- `src/core/{knobs,counters,derive,checksum,sqzhash,clock,log,thread_name,errors}.zig`, `src/mem/{budget,sizing}.zig`, `src/io/{align_probe,uring_fs,nvme_derive,nvme_dev,ring_park,uring128}.zig`, `src/sync/{futex,completion,mpmc}.zig`, `src/exec/*` (executor), `src/meta/*`, `src/model/explorer.zig`, `tests/rails.zig`, root `src/squeezefs.zig`.
- ONE of each primitive (WP-C): ring park `src/io/ring_park.zig`; futex wrapper `src/sync/futex.zig`; lane completion cell `src/sync/completion.zig` (`Completion(T)` — NOT `channel.Oneshot`, which needs an Io on both sides); bounded MPMC `src/sync/mpmc.zig`; errno table `src/core/errors.zig` (classes Retryable/Terminal/Refusal; backpressure is EAGAIN never EINVAL; EBADE round-trips; closing volume → EROFS, refused device → EINVAL).
- `derive.facts()` is the ONE source of cpus/page size; cgroup `memory.max` AND `memory.high` are read along the ancestor chain. Mount tag (`setMountTag`) is deleted. The library does not import `build_options`.
- New modules are wired by replacing `// @anchor:<name>_import` / `_test` lines in `src/squeezefs.zig`; no anchors may remain after a wave.
- Linux-only modules are gated in `src/squeezefs.zig` by `builtin.os.tag == .linux`, never stubbed. Futex is Linux-only, so portable spin locks are used where portability is needed.

## Knobs and rails
- Every `SQUEEZEFS_*` knob is a row in `src/core/knobs.zig` and must be read by some module; `tests/rails.zig` fails the gate on unregistered literals and on registered rows nothing reads (inverse census).
- Knob layer: `Parsed(T) = union(enum){absent, value, invalid: Diagnostic}`; readers `boolKnob/intKnob/optIntKnob/enumKnob` take a comptime key, enforce the row's range in-process (never clamp), take the default FROM the row (`intDefault`/`boolDefault`). The `.tuning` knob class is deleted.
- Deleted knobs: `SQUEEZEFS_PARKED_GATE_ASSIST_MS`, `SQUEEZEFS_TIMEOUT` (→ `conveyor.PARK_ESCALATION_NS = 30 s`), `SQUEEZEFS_TEST_NVME_READ_TIMEOUT_MS`; `SQUEEZEFS_FLEET_SHARE` → `SQUEEZEFS_TEST_FLEET_SHARE` (test seam; real value derived from a cgroup.procs census).
- `SQUEEZEFS_META_FLUSH_INTERVAL_MS` is the flush A/B lever (default 50, 0 = strict). There is no `-o strict` mount option.
- `zig build gate` = `zig fmt --check` + unit tests + rails. Literal rail allowlist `tests/rails/literal-allowlist.txt` (~609 OWED rows still to resolve).

## Counters and tripwires
- `ShardedCounter`, `Gauge`, `ShardedLatencyHistogram`, `Tripwire` in `core/counters.zig`. Clocks in `core/clock.zig` (no std.Io on hot paths). Thread names via `thread_name.nameCurrentThread` (prctl).
- Tripwire registry (67952ae): a static 512-slot table keyed by the site NAME; each instance caches its slot; instances are NOT linked into a global list, so embedding Tripwires per-instance in stats structs is safe. `tripwireCount(site)` sums every instance; `forEachTripwireSite` visits every site for the `.stats` fold; `tripwireSitesOverflow()` is itself must-stay-0; `Tripwire.resetLogBudgetForTest()` is the test seam for the per-site log budget.

## Logging and tests
- Zig 0.17's test runner fails any test that logs at error level; tests expecting error logs use `core/log.zig`'s `log.testing.expectErrorLogs(n)` seam.
- The build runner prints `failed command: …` whenever a test writes to stderr even with exit 0.

## Flaky-test hygiene (rung 9 lessons)
- Never assert a relation between two real timing measurements in a unit test (the hash microbench's chained≥throughput assertion flipped 67/150 under 8× `yes`): pin the arithmetic on fixed counts in the unit test and REPORT (never assert) the measured relation as a bench-smoke row (22e764b).
- Publish accounting before the wake: the shim stub host bumped `served` after `completeAndWake`, so a woken reaper could read a stale count; bump before the DONE swap whose Release publishes it (9042514).
- `testkit.zig` has no load-factor knob; verify fixes with hundreds of runs under synthetic load. Recorded in `docs/evidence/rung-09-gates.md` under 'Gates'.
- `mem/budget.zig` `censusFleetShare` counts same-`comm` pids in the ONE container cgroup, so two concurrent `squeezefs-unit` runs halve each other's derived tier (spurious `mem.budget` fleet-share failures) — a shared-venue artifact, not a regression; a deterministic seam is owed.

## Model checking convention (recorded in AGENTS.md)
- Lock-free cores are written as standalone value-type MODELS (per-thread pcs, one atomic action per `step`) and explored exhaustively with `src/model/explorer.zig` via `expectHolds`.
- Every model ships with its weakening proven to fail via `expectFails`.
- Explorer contract: `thread_count`, `init`, `done`, `step`, `invariant`, `final`, optional `key`. DFS over the state graph memoised by content hash; defaults max_states 2M, max_depth 256; sequential consistency.
- Store buffering (TSO) must be modelled as a separately schedulable drain step; draining in the same step as the load hides the Dekker reordering.