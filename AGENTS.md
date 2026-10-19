# AGENTS.md — SqueezeFS-Zig working conventions

**`docs/charter.md` is the specification.** Read it before touching core logic. This file
is only the how-we-work layer on top of it.

When resuming work, read the newest note in `docs/handoff/` first (state, open items in order,
owed lists, resume steps); `docs/decisions/` and `docs/evidence/` hold the why and the numbers.

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

Linux only (charter §1.2). Every Linux venue is reached through `scripts/venue`, runs on
the **sqz kernel series** (`scripts/kernel/patches-7.2/`, 0001–0030 on linux 7.2.y, plus
`scripts/kernel/config-fragment`; `uname -r` carries `sqz`) with Zig 0.17.0 and the repo
at the same absolute path, and runs commands as root:
- `local` (`SQZ_VENUE=local`; the default when the controller is Linux): THIS host —
  commands run through `sudo` in this tree, no sync, no VM. Set it up once with
  `scripts/dev/provision-local-venue` (checks the distro — Debian/Ubuntu with apt, or Arch
  and its derivatives such as Omarchy with pacman —, zig on root's PATH, the sqz kernel
  and `fuse.enable_uring`, the nvmet/brd/null_blk modules, the packages, the build root,
  passwordless sudo; prints each fix; `--apply` does the packages, modules, the boot
  parameter in GRUB or Limine, IOR/mdtest and directories — never a kernel install or a
  reboot; with ≥ 64 GiB of RAM, `/build/tmp` and the tree's `.zig-cache/tmp` live in RAM:
  ext4 on a zram device via zram-generator, so every test image stays off the disk). The kernel: `scripts/kernel/build-host-kernel --local` — on Debian/Ubuntu
  linux 7.2.8 + the series as .debs (`7.2.8-sqz`); on Arch/Omarchy Omarchy's CURRENT
  `linux-omarchy-bore` PKGBUILD (its base, all its patches, its config) + the series, built
  with makepkg, installed with pacman (`7.2.x-N-omarchy-sqz`; owner ruling 2026-10-16).
  After an Omarchy bump run `--prepare-only` first: it applies everything at `--fuzz=0` in
  minutes and a conflict means refreshing the patch in `scripts/kernel/patches-7.2/`.
  Reboot into it yourself. Build slots: one per 4 threads and per 8 GiB
  (`SQZ_VENUE_SLOTS` overrides; every controller of one build root must agree).
- `vm` (the default on the Mac): the macOS `container` `sqz-zig` — aarch64, Ubuntu 24.04,
  this repo mounted rw at the same path.
- `kamrui` (`SQZ_VENUE=kamrui`): the x86_64 box `kamrui-dev` over ssh — 12 threads,
  61 GiB, local NVMe; the repo rsynced to the same path before every command.

One venue covers one architecture: a local x86_64 workstation needs a second venue for
aarch64 (the Mac's `vm`, or an arm64 box over ssh), and the gate train runs on every venue
in `GATE_TRAIN_VENUES` (default `local` on Linux, `vm kamrui` on the Mac).

```
scripts/venue up                 # vm: create the container; local: the provisioning check
scripts/venue status             # kernel / zig / fuse / nvmet modules / build-root space
scripts/venue zig build gate     # the full gate, in Linux (the gate train runs it)
scripts/venue test "filter"      # unit tests matching a filter, in Linux
zig build gate                   # optional local check on macOS: portable modules only
```

Portable modules (codecs, derivations, knobs, counters, pure state machines) run on
every host. Linux-only modules (`src/io/*`, anything touching io_uring, /proc, /sys,
cgroups) compile their tests only on Linux — gate them in `src/squeezefs.zig` with
`builtin.os.tag == .linux`, never with a stub.

## Test data and disk

The build root `SQZ_BUILD_ROOT` (default `/build`; ext4 on the cache volume inside the VM,
an ext4 LV on kamrui, an ext4 filesystem of its own on a local workstation) holds the venue's
gate lock, build slots and zig caches, and is where the gate's smoke runs: real block
semantics, sparse files, fast. The product's tests write under `/build/tmp`, so a root
elsewhere needs `/build` linked to it (the provisioning check says so). An external capacity disk is mounted at `/build/ext` when
`SQZ_EXT_DIR` (default `/Volumes/512GB/sqz-tmp`) exists at `scripts/venue up`: HFS+ over USB
through virtiofs — **no sparse files** (`truncate -s 8G` costs 8 GB), ~40 MB/s writes, reads
served from the Mac's cache, so use it for capacity-class experiments by hand (large volumes,
many files, io500/ior later), never for crash-consistency rows. `SQUEEZEFS_TEST_DATA_DIR`
moves the smoke's data volumes + cache dir there. Run `scripts/venue trim` after big runs.

## Gate (per commit)

`zig build gate` = `zig fmt --check` + unit tests + the static rails (`tests/rails.zig`:
every `SQUEEZEFS_*` literal is a registry row; every non-retired row is read by some
module — a row nothing reads is dead code and goes red). Keep it green on the Linux
venues; the macOS run is an optional local check (owner ruling 2026-10-14: the product is
Linux-only, so no macOS gate is required).

Its test steps run side by side (`docs/evidence/rung-10-gate-speed.md`): the unit tests
and the smoke are shard sets of one compiled binary under `tests/test_runner.zig`, so a
test must not depend on what ran before it in the process (reset process-wide state it
asserts — a tripwire's log budget, a histogram-derived threshold) nor on an idle host
(wait for the observable it asserts, never a real-time bound between two statements).
Rigs stay run-scoped (pid-tagged paths, `127.0.0.1:0` ports); a test that cannot share
the venue says `[venue-exclusive]` in its name. When the runner refuses a stale cost
table, `scripts/venue zig build costs` regenerates `tests/costs/` — commit it. A test past
its hang limit (20 × its cost row, clamped to 120–900 s; no row = 900 s) ends its shard with a
`HUNG TEST:` line, every thread's kernel and user stack, and exit 124
(`docs/evidence/rung-10-hangwatch.md`): a test that legitimately got that much slower needs
its row in `tests/costs/` updated, not a bigger limit.

## Landing (the gate train owns the full gate)

`scripts/gate-train` gates `dev`'s head on the Linux venues in `GATE_TRAIN_VENUES`,
re-gates a green head until it has three consecutive greens on all of them (charter §11's
release criterion), and reports one line per run (`.gate-train/ledger-v2.tsv`). So a change
lands like this:

- **Light tier** (tests, accounting, small defects, docs): the red test, the fix, the
  targeted runs on a Linux venue (`scripts/venue test "<filter>"`, the affected smoke
  rows); rebase, `--ff-only`, done. No full gate of your own.
- **Full tier** (on-disk format, wire protocol, a lock-free core or its model, the FUSE
  contract, the durability or coherence laws): the light tier plus ONE full venue gate
  before landing (`scripts/venue 'timeout 3900 zig build --cache-dir "$SQZ_BUILD_ROOT"/zig-cache-v2
  gate --summary all'` — serialized by `flock $SQZ_BUILD_ROOT/.gate.lock`; never launched from a
  script inside the container, never under extra load). No post-landing gates: the train
  runs them.
- **Landing publishes**: a fast-forward of `dev` (and any commit on `dev`) is pushed to
  `origin` by the main repo's local hooks (owner ruling 2026-10-14). Never push a branch of
  your own, never force-push.
- **The train goes red on your commit**: fix it within the hour or revert it — `dev` stays
  green. A row in `docs/gates/flakes.md` failing is reported, not yours to chase: its owner
  has it.
- Evidence notes stay under a page: what changed, why, the red test, the numbers.
- **Venue resources**: `scripts/venue` admits ONE full gate at a time (it holds every build
  slot) and at most two other builds (three on kamrui; `nproc`/4 and GiB/8 locally). A heavy command that is not a
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
