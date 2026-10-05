# Rung 1 — io_uring file layer, O_DIRECT metadata posture, NVMe block workers, memory budget

Charter §13 rung 1. Landed 2026-10-05 on `dev`.

## What landed

| Module | Lines | Tests | Contract |
|---|---|---|---|
| `src/core/*` (knobs, counters, derive, checksum, clock, log, thread_name) | ~1,400 | 29 | The ONE knob parser + registry + startup refusal report; per-thread sharded counters, exact-sum histograms, must-stay-0 tripwires; derivation primitives; xxh3-64 field-zeroed checksums; thread name → CPU class |
| `src/mem/budget.zig`, `src/mem/sizing.zig` | 1,760 | 44 | R5 budget: flag → `SQUEEZEFS_MEM_BUDGET_MB` → cgroup `memory.max × 0.8` → `70 % RAM` (fleet-share divided roots); components `{gauge, floor, weight, shed}`; Σ floors ≤ 0.9 budget proportional clamp; Green/Yellow/Red with 80/76 + 95/91 hysteresis; weight-proportional shed with remainder-to-last; 3-tick hard backstop; `explicit > pct > derived` resolvers for the transport and IPC caps |
| `src/io/align_probe.zig` | 398 | 13 | Grain ladder `statx DIOALIGN → sysfs logical_block_size (partition → parent) → 4096`; admit pow2 ≤ 4096, refuse never clamp; write-shape and pointer alignment; widen-and-slice arithmetic |
| `src/io/uring_fs.zig` | 2,739 | 26 | Worker pool `clamp(cpus/4, 4, 64)` (`sqz-uring-fs<i>`), one plain ring per worker (no fixed files/buffers here — the reference has none either: ad-hoc local files, not the data path), Vyukov MPMC + futex oneshot, SQ-full ⇒ submit then retry once, ADMIT_CAP 256; the metadata device registry (`Direct{grain}` / `Buffered`), `registerMetaDevice` trial `O_RDWR|O_CLOEXEC|O_DIRECT`, the ONE fallback (EINVAL on a REGULAR FILE ⇒ buffered, loud, `meta_io_buffered_fallback`), `prepareDirectWrite` (misaligned product write REFUSED → `meta_io_unaligned_refusals`; misaligned pointer bounced → `meta_io_bounce_bytes`), `readSpan` widened + sliced (`meta_io_read_widened`), `padToGrain`; `OwnedRing` for the journal lane (eventfd READ armed at `u64::MAX`, EXT_ARG timed park) |
| `src/io/nvme_derive.zig` | 973 | 20 | `ioLanesFor(cpus, devices) = clamp(ceil(cpus/devices), 1, cpus)`; lane overrides; pool caps; DMA alignment; write-cache class codec; two-sided errno table (EBADE = the fence class); SeqCst `WakeCoalescer`; bounded MPMC; stats + tripwires |
| `src/io/nvme_dev.zig` | 2,302 | 24 | Lanes `sqz-nvme<N>`, 1024-entry ring via the probe ladder (`SINGLE_ISSUER|DEFER_TASKRUN → +COOP_TASKRUN → SINGLE_ISSUER → plain`), `register_files([fd])` with raw-fd fallback, 64 KiB ranged slab as fixed buffer 0 (the 4 MiB pool deliberately unregistered), eventfd wake through the coalescer, ONE `submit_and_wait(1)` per pass, 3 SQ-full retries; `Device.open` with `probeDirectIo` loud refusal, capacity by seek End, write-cache class from sysfs, `readBlock`/`writeBlock` (aligned zero-copy vs 4 KiB-padded bounce), zero-length guards, `flush` = SeqCst watermark drain + `FSYNC|DATASYNC` on lane 0, EBADE fence latch + custody poison hook, read timeout `SQUEEZEFS_TEST_NVME_READ_TIMEOUT_MS` |

Static rails (`tests/rails.zig`): every `SQUEEZEFS_*` literal in `src/` + `tests/` is a registry row; every non-retired row is read outside the registry.

## Evidence

Instrument: `zig build gate` (fmt + unit + rails), Debug test profile. Venue: the `sqz-zig`
macOS container — Ubuntu 24.04, kernel **7.2.6-sqz**, aarch64, 8 CPUs / 16 GiB, root with
`CAP_IPC_LOCK` + unlimited memlock; Zig 0.17.0. Substrate for device tests: regular files on
ext4 (`/build`, O_DIRECT accepted) and a real loop device via `LOOP_CTL_GET_FREE`/`LOOP_SET_FD`.

| Venue | Result |
|---|---|
| Linux (`scripts/venue zig build gate`) | 10/10 steps; **158/158** tests (156 unit + 2 rails); 13 s; exit 0; lane 0 `fixed_file=true fixed_buf=true` |
| macOS (`zig build gate`, portable modules) | 10/10 steps; 104 pass + 2 Linux-only skips + 2 rails; exit 0 |

Binary gate check (Linux): `squeezefs --version` → `squeezefs 0.1.0 (<commit>, gate profile, zig 0.17.0)`;
`SQUEEZEFS_OP_PROFILE=yess SQUEEZEFS_NOT_A_KNOB=1 squeezefs --version` → the typo warning on
stderr, then `Error: refusing to start: invalid SqueezeFS environment knob(s)` naming the
offender, exit 1.

Bugs found red-first during the rung (both would have been load-dependent hangs):
- `FUTEX_WAKE` takes a C `int`; `maxInt(u32)` woke exactly one waiter, so pool `close` hung `join`.
- `io_uring_enter` discards the wait's `ETIME` when the same call submits SQEs; `OwnedRing.park`
  now submits first and waits with `to_submit = 0`.

## Deviations from the reference (deliberate)

- No blocking `pread/pwrite` fallback in `uring_fs` (the reference has one): charter §1.2 forbids
  classical I/O on a product path. A box without io_uring refuses at `Pool.init`; a ring that
  breaks at runtime fails its ops with `RingFailed`.
- The fault-injection shim, `patch_at`, and the linked-timeout seam exist only for the reference's
  harness; they land with the journal rung's harness if needed.
- `SyncCoalescer` (group commit around the barrier) belongs to the metadata plane and lands with
  rung 2; `Device.flush` is uncoalesced today (`sync_requests == syncs`).
- Discard lives in block reclaim (rung 4), not here.
- The R5 allocator purge has no std equivalent (no release-to-OS call); the Red tick notes it.

## Owed

- `daemon_cpu_ns_by_class` sampling (`/proc/self/task/*/schedstat`) — with the `.stats` inode (rung 5).
- `SQUEEZEFS_OP_PROFILE`, `SQUEEZEFS_STATS_KEY_CENSUS` rows — with the `.stats` inode (rows nothing
  reads are dead code; the rail refuses them).
- `thread_name.commName` truncates a tagged base to 13 chars, so `sqz-uring-fs1x` loses a digit for
  workers ≥ 10; the reference has the same budget. Revisit if a class ever needs the index.
- The reference's fixed-file/fixed-buffer measurements are not re-measured here; this rung is
  correctness. Rung-2's journal lane and rung-4's data path carry the first A/B rows.
