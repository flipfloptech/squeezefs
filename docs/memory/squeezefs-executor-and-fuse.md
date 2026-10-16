# SqueezeFS-Zig executor and FUSE library (rung 3)
The own std.Io executor (SqzIo, lanes, fibers), the uring128/ring_park ring layer, the FUSE transport and buffer-mode lattice, kernel facts on 7.2.6-sqz, and measured rung-3 numbers.

## Executor (src/exec/, decision 0001)
- `SqzIo` (`src/exec/sqz_io.zig`) is our own std.Io implementation: embeds `Io.Threaded` as first field and composes the vtable at comptime, owning async/concurrent/await/cancel/group*/futex*/sleep/now/operate.
- Per-lane event loops reuse std.Io.fiber context switching, park on a per-lane io_uring (ParkedRing), with own scheduling policy: lane homing, no cross-NUMA stealing, ticked backstop, named threads. `Io.Threaded` was rejected (thread per concurrent task, signal-based cancel).
- Lane classes: per-core lanes, sqz-meta pool, blocking pool, timer thread, fd-watch. The journal lane and the conveyor pass run on ONE exec.Lane (cutover 124fa47).
- Fibers (`src/exec/fiber.zig`): `switchIn`/`switchToLane` are `noinline` (the call boundary is the ABI fence for the x18 clobber bug); the trampoline zeroes LR so stack walks end at the fiber's first frame.
- Thread names: class prefix `sqz-fuse-ln` (worst case `sqz-fuse-ln9999`) in `src/core/thread_name.zig`; uring threads `sqz-fuse-ur<qid>`; ONE `sqz-timer` thread for non-lane contexts.
- `conveyor.PARK_ESCALATION_NS = 30 s` replaces the old `SQUEEZEFS_TIMEOUT` knob.

## Rings
- `src/io/uring128.zig`: first-party SQE128/CQE32 binding (`Ring128`, `ParkedRing128`); the FUSE uring command needs the 80-byte cmd area at SQE offset 48.
- `src/io/ring_park.zig`: ONE park/wake/dead protocol, `ring_park.Parked(comptime Face)` shared by both ring widths: eventfd wake through a SeqCst coalescer, standing READ at u64::MAX, submit-first EXT_ARG park, `registerFile`, `prepFdatasync`, `initProbing`, `quiesce`. uring_fs, journal_lane and nvme_dev all ride it.
- Park protocol must submit first, then do the EXT_ARG timed wait, and treat ETIME as a verdict (`Entered{submitted, timed_out}`), not an error.
- Transport's two SeqCst fence points are expressed as SeqCst accesses (no `@fence` in Zig 0.17; see slot.zig module doc).

## Buffer-mode lattice (rung 3 wave 4, 2b21a6e)
- `src/fuse/transport/{probes,kmbuf,zc}.zig` probe-detect UserEnts|BufRing|ZeroCopy. On 7.2.6-sqz kmbuf (register pair 38/39), zero-copy, payload retention and FUSE_TIME_LIMITS are all Present; `auto` picks ZeroCopy; zc READs show 0 daemon copies (~3.98 enters/req vs 1.00 for user/bufring).
- Knobs: `SQUEEZEFS_FUSE_BUFFER_MODE` (auto|user|bufring|zc; an explicit mode the probe refuses fails the mount loud), `SQUEEZEFS_READ_ZC_SERVE` default ON, `SQUEEZEFS_FUSE_READ_FUSION` default OFF.
- `unmountDetached` writes fusectl `abort` (minor from `/proc/self/mountinfo`) to avoid D-state processes.
- Kernel exposes no per-queue bg counters; only `max_background` is published.

## Kernel facts (7.2.6-sqz, pinned red-first)
- `io_cqring_wait` returns 0 instead of `-ETIME` whenever ANY CQE sits unreaped in the ring at expiry; a submitting `io_uring_enter` also discards ETIME.
- Flag rung 0 (`SINGLE_ISSUER|DEFER_TASKRUN|TASKRUN_FLAG`) is buildable; a foreign-thread eventfd write raises `IORING_SQ_TASKRUN` before any enter.
- Kmbuf raw register opcode 38 is live (register ok → EEXIST → region mmap at `0x88000000|(bgid<<16)`).
- `fuse.enable_uring` defaults to N; the daemon writes Y before the INIT reply. `/proc/sys/fs/fuse/max_pages_limit` is 256.
- Geometry the venue produces: max_write 1 MiB, depth 32, max_background 288, congestion 216, arena 288 MiB, drain width 2, sq_entries 128; possible CPUs `0-8` (9 queues).

## Measurements (venue, Debug, scoping rows)
- Journal lane + conveyor on one lane: io_uring_enter per commit 2.98–3.01 → 1.58–1.66; parks per window 2.00 → ~1.2–1.3; 0 tick rescues.
- FUSE transport storm (EchoFs, 4 threads × 10 s): ~200k ring requests at ~2.77 enters/request, wake writes ~5% elided, 0 abandoned, 0 cq_overflows, 0 tick rescues.
- 2,000 lane spawns from 4 foreign threads ≈ 205 ms, 0 tick rescues.
- Op-guard storm (8 threads × 2000 ops, one ino): Shared peak holders 8 at ~440–630 ns/op; EntireOp ~5–6 µs/op (macOS) / 12.7 µs/op (Linux).
- Fiber canary scan ≈ 10 µs per recycle vectorized.
- POSIX smoke: 377 fuse ops = 377 ring requests, ~2.95–2.98 enters/request.
- Explorer models: task_state (3), futex_table (174 states), group (17), wake_core (53), LeaseGate(1) 19 / LeaseGate(2) 81 / LeaseGateShutdown 11 / WakeCoalescer 161 / WakeCoalescerLeaseDrop 288, each with a recorded failing weakening.

## Reference repo material for FUSE/executor
- `~/Source/squeezefs/crates/fuse3/` (~34k lines): `fuse_over_uring.rs` (transport heart), `fast_dispatch.rs`, `kmbuf.rs`, `session.rs`; `crates/squeezefs-ipc/src/sqz_exec.rs`, `exec_core.rs`, `lane_core.rs`, `sqz_time.rs`, `sqz_fdwatch.rs`, `sqz_blocking.rs`, `sqz_task.rs`.
- Daemon-side FUSE handler layer lives entirely in `src/fuse_client.rs`. Kernel contract in `docker/kernel-sqz/{README,SERIES}.md` plus patches. `docs/design-transaction-lock-removal.md` is self-declared superseded.