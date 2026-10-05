# AGENTS.md — SqueezeFS-Zig working conventions

**`docs/charter.md` is the specification.** Read it before touching core logic. This file
is only the how-we-work layer on top of it.

The Rust reference implementation lives at `~/Source/squeezefs` (read-only; mounted
read-only in the build container). `docs/porting/*.md` are compact porting specs
extracted from it (derivations, knob rows, gauge names, test contracts, measured
numbers with their WHY). Port from the spec; open the Rust only to settle a detail the
spec leaves open, and then add the detail to the spec.

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

## Gate (per commit)

`zig build gate` = `zig fmt --check` + unit tests + the static rails (`tests/rails.zig`:
every `SQUEEZEFS_*` literal is a registry row; every non-retired row is read by some
module — a row nothing reads is dead code and goes red). Keep it green on both venues.

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
