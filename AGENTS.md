# AGENTS.md — SqueezeFS-Zig working conventions

**`docs/charter.md` is the specification.** Read it before touching core logic. This file
is only the how-we-work layer on top of it.

The Rust reference implementation lives at `~/Source/squeezefs` (read-only; mounted
read-only in the build container). `docs/porting/*.md` are compact porting specs
extracted from it (derivations, knob rows, gauge names, test contracts, measured
numbers with their WHY). Port from the spec; open the Rust only to settle a detail the
spec leaves open, and then add the detail to the spec.

**The reference is evidence, not law** (`docs/decisions/0002-no-compatibility-obligation.md`):
the charter's mechanisms, invariants and derivations are requirements; the reference's
incidental choices are not, and no compatibility with its volumes or wire is owed. When a
spec item is a quirk (a serialization-library artifact, a legacy slot reuse, a known race),
take the better design and add a row to the ledger in decision 0002 before diverging.

## Venue

Linux only (charter §1.2). The dev box is macOS; the Linux venue is the macOS
`container` `sqz-zig` (Ubuntu 24.04 on the **7.2.6-sqz** kernel series, Zig 0.17.0,
`fuse.enable_uring` available, this repo mounted rw at the same path).

```
scripts/venue up                 # create the container once
scripts/venue status             # kernel / zig / fuse facts
scripts/venue zig build gate     # THE per-commit gate, in Linux
scripts/venue test "filter"      # unit tests matching a filter, in Linux
zig build gate                   # same gate on macOS: portable modules only
```

Portable modules (codecs, derivations, knobs, counters, pure state machines) run on
every host. Linux-only modules (`src/io/*`, anything touching io_uring, /proc, /sys,
cgroups) compile their tests only on Linux — gate them in `src/squeezefs.zig` with
`builtin.os.tag == .linux`, never with a stub.

## Test data and disk

`/build` (ext4 on the cache volume inside the VM) is where the gate's smoke runs: real block
semantics, sparse files, fast. An external capacity disk is mounted at `/build/ext` when
`SQZ_EXT_DIR` (default `/Volumes/512GB/sqz-tmp`) exists at `scripts/venue up`: HFS+ over USB
through virtiofs — **no sparse files** (`truncate -s 8G` costs 8 GB), ~40 MB/s writes, reads
served from the Mac's cache, so use it for capacity-class experiments by hand (large volumes,
many files, io500/ior later), never for crash-consistency rows. `SQUEEZEFS_TEST_DATA_DIR`
moves the smoke's data volumes + cache dir there. Run `scripts/venue trim` after big runs.

## Gate (per commit)

`zig build gate` = `zig fmt --check` + unit tests + the static rails (`tests/rails.zig`:
every `SQUEEZEFS_*` literal is a registry row; every non-retired row is read by some
module — a row nothing reads is dead code and goes red). Keep it green on both venues.

Its test steps run side by side (`docs/evidence/rung-10-gate-speed.md`): the unit tests
and the smoke are shard sets of one compiled binary under `tests/test_runner.zig`, so a
test must not depend on what ran before it in the process (reset process-wide state it
asserts — a tripwire's log budget, a histogram-derived threshold) nor on an idle host
(wait for the observable it asserts, never a real-time bound between two statements).
Rigs stay run-scoped (pid-tagged paths, `127.0.0.1:0` ports); a test that cannot share
the venue says `[venue-exclusive]` in its name. When the runner refuses a stale cost
table, `scripts/venue zig build costs` regenerates `tests/costs/` — commit it.

## Landing (the gate train owns the full gate)

`scripts/gate-train` gates `dev`'s head on both venues, re-gates a green head until it has
three consecutive greens (charter §11's release criterion), and reports one line per run
(`.gate-train/ledger.tsv`). So a change lands like this:

- **Light tier** (tests, accounting, small defects, docs): the red test, the fix, the
  targeted runs (`scripts/venue test "<filter>"`, the affected smoke rows), `zig build gate`
  on macOS (~3 min); rebase, `--ff-only`, done. No full venue gate of your own.
- **Full tier** (on-disk format, wire protocol, a lock-free core or its model, the FUSE
  contract, the durability or coherence laws): the light tier plus ONE full venue gate
  before landing (`scripts/venue 'timeout 3900 zig build --cache-dir /build/zig-cache-v2
  gate --summary all'` — serialized by `flock /build/.gate.lock`; never launched from a
  script inside the container, never under extra load). No post-landing gates: the train
  runs them.
- **The train goes red on your commit**: fix it within the hour or revert it — `dev` stays
  green. A row in `docs/gates/flakes.md` failing is reported, not yours to chase: its owner
  has it.
- Evidence notes stay under a page: what changed, why, the red test, the numbers.
- **Venue resources**: `scripts/venue` admits ONE full gate at a time (it holds every build
  slot) and at most two other builds (three on kamrui). A heavy command that is not a
  `zig build` — a loop of smoke rows, daemons, `yes` load — runs as
  `SQZ_VENUE_SLOT=1 scripts/venue '…'` so it takes a slot too and never starves a gate.
  `SQZ_VENUE=kamrui` selects the x86_64 box; `scripts/venue pull <path>` brings back what
  a command there wrote into the tree.

## Conventions (Zig 0.17)

- **Allocators explicit**; hot paths own pools and allocate nothing per op.
- **Knobs**: `src/core/knobs.zig` is the registry and the ONE parser. Read a knob only
  through `knobs.boolKnob / intKnob / optIntKnob / enumKnob` with the process
  `Environ.Map`. Adding a knob = adding a registry row (name, kind, range, default,
  class, doc). Precedence `explicit > percentage > derived` lives in a pure `resolve*`
  function per family that never refuses (the startup gate already did).
- **Derivations**: write every sizing default as a pure function of `derive.Facts`
  (cpus, page size, RAM) and the budget; give it a tie test that restates the formula.
  A fixed clamp carries its physical/format/kernel/measured reason ON THE LINE.
- **Observability**: `counters.ShardedCounter` for per-op counts (never a shared-line
  RMW on a hot path), `counters.Gauge` for levels, `counters.ShardedLatencyHistogram`
  for phases (exact-sum), `counters.Tripwire` for every "cannot happen" (must-stay-0,
  per-site label, never a panic in a handler). Thread names via
  `thread_name.nameCurrentThread`; they are the CPU accounting contract.
- **Logging**: `core/log.zig` (`log.err/warn/info/debug`). The test runner fails a test
  that logs an error; a test of a path whose contract IS a loud error log declares it
  with `log.testing.expectErrorLogs(n)` and asserts `expectAllFired()`.
- **Clocks**: `core/clock.zig` (`monotonicNs`, `realtimeCoarseS`) — no `std.Io` handle
  on hot paths.
- **Syscalls**: `std.os.linux.*` raw wrappers + `linux.errno(ret)`; `std.os.linux.IoUring`
  for rings. Never classical `read/write/pread/pwrite` on a product I/O path.
- **On-disk/wire structs**: `extern struct`, explicit endianness, `comptime` size
  assertions, canonical total codecs, xxh3-64 via `core/checksum.zig`.
- **Errors**: error unions with typed classes (retryable / terminal / refusal); errno
  mapping in one table.
- **Atomics**: explicit orderings and a comment naming the paired fence.
- 0.17 specifics: no `**` array repeat (use `@splat`); `std.ArrayList` is unmanaged
  (`.empty`, pass the allocator); format with `std.Io.Writer` (`{f}` calls a type's
  `format(self, *Writer)`); file I/O in tests through `std.testing.io` and
  `std.Io.Dir.cwd()`.
- **Executor & waiting** (`docs/decisions/0001-executor-is-an-io-implementation.md`): code that
  waits, spawns or does non-hot-path I/O takes an `io: std.Io` parameter (tests pass
  `std.testing.io`); the charter's primitives live in `src/sync/` over the `Io` futex ops; hot
  paths never take an `Io`. Our own `Io` implementation (`src/exec/`) lands with rung 3.
- **Model checks**: every lock-free core is written as a standalone, dependency-free MODEL
  (plain value type, explicit per-thread program counters, one atomic action per `step`) and
  explored exhaustively with `src/model/explorer.zig` (`expectHolds`). Every model ships with
  its WEAKENING (the fence or step whose removal breaks it) proven to fail (`expectFails`), and
  the evidence note records which weakening fails each model. Store buffering is modelled
  explicitly (a per-thread pending store the fence step drains) where a core depends on it.
- **Red-first**: tests define the contract; a bug lands with a red repro first. No
  sleeps for synchronization; `SQUEEZEFS_TEST_*` seams hold the exact window a pin
  needs.
- **No dead code.** Delete it; never suppress it.
- Branch from `dev`; conventional commits; `--ff-only` merges; delete branches after
  merge. Each delivery rung lands with an evidence note in `docs/evidence/`.
