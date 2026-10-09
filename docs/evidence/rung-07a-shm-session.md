# Rung 7 wave A — the shared-memory session: the sealed memfd layout, the slot/ring/doorbell cores with their models, the `il0` rendezvous and the fd screen, the heartbeat, the ONE client library

Spec: `docs/porting/il_session.md` (§1 layout, §2 rendezvous, §3–§5 cores + models, §6 sizing,
§7–§9 knobs/gauges/quirks); `il_shim.md` §3 and `il_sdk.md` §3 (what the client library owes each
consumer). Charter §7, §9 (shm + futex is the sanctioned hop), §10, §12. Decision 0003 §F; decision
0002 rows 300–311. Modules: `src/ipc/{ipc, layout, slot_core, slot_core_model, ring_core,
ring_core_model, doorbell, doorbell_model, wire, sizing, rendezvous, rendezvous_linux, shm,
client}.zig`; `src/sync/futex.zig` (the cross-process face + `waitMany`); the `il0` face in
`src/fuse/handler/kv_fs.zig`; the `ipc` stats family; 13 knob rows. The daemon host (listeners,
the `sqz-ipc-ctl` thread, the service loop, poison/teardown, the data-plane serve) is wave B; the
shim and the SDK are waves C/D.

## What landed

- **The cross-process futex face** (`sync/futex.zig`): `waitSharedTimeout` / `wakeShared` /
  `wakeSharedAll` (never `FUTEX_PRIVATE_FLAG`: a private wait on a MAP_SHARED word is a wait nobody
  else can wake) and `waitMany` over `futex_waitv(2)` (≤ 128 entries, absolute `CLOCK_MONOTONIC`
  deadline, `unsupported` on ENOSYS — the client fails loud at establish, row 307). Red-first: a
  forked child parked on a memfd word is woken by its parent; a wake on ANY of three shared words
  ends a `waitMany`.
- **The layout** (`layout.zig`): `Geometry` (24 B), `SessionHeader` (three cache lines: identity +
  `generation` + `heartbeat` @0, `doorbell`/`daemon_parked` @64, `cqe` @128, `geometry` @192 — the
  PERF-18 co-location kept), `SlotCore{state, _pad, generation}` first in the 128 B `IpcSlot`,
  `RingCell` (8 B), `ClientStatsPage` (4 KiB); every offset asserted at comptime.
  `SessionLayout.compute` with the spec's offsets as ties (`0x1000 / 0x1080 / 0x4000 / 0x24000 /
  0x25000`, total `0x25000 + 64 MiB`), the 11 geometry refusals each on its own member, the ONE
  slab law (64 KiB default; the 90 MiB bounce shape floors to 22 pages), 4 MiB arena alignment,
  `IPC_MAGIC = "SQZIPC02"`, `IPC_ABI = 1` (row 300), the op table `{READ, WRITE}` (row 305). The
  `Mapping` view bounds-checks every slot index and arena window (checked add). `PmdPlan` (the
  over-reserve + trim arithmetic in the kernel's page size), `pickOwner` (balance-first
  `(load, distance, index)`), `nodePartition` (largest-remainder weighted round-robin), the ingress
  delta law (`≥ 1 s` discards: the u32-ns wrap). Fuzz: `SessionHeader.decode` total over 2000
  random images + bit flips.
- **The slot core** (`slot_core.zig`): every transition with its ordering and paired fence on the
  line — claim (Acquire CAS, pairs with `release`'s Release), `publishSubmitted` (Release, pairs
  with `tryBeginServe`'s Acquire), `tryBeginServe` (CAS loop preserving WAITER, the edge licensing
  the ONE `snapshot`), `complete` (`swap(DONE, .acq_rel)`, clears WAITER, reports it for the
  INT_MAX-breadth shared wake), `parkPrepare` (one `fetchOr(WAITER, .acq_rel)` = publish-then-
  recheck), `isDoneFor` (state Acquire FIRST, then the generation), `releaseClaimed` (the
  arena-extension hold). `SessionMap` — the lease-refcounted unmap (`.acq_rel` on the drop; the
  last release unmaps). Cycle stress: 10 k lives, a server racing the consumer, exactly once.
- **The ring** (`ring_core.zig`): a borrowed `View{tail, cells, mask}` over client-writable shm with
  the `Consumer{head}` cursor daemon-private; Vyukov push/pop; `seedForSharing` (a zeroed memfd is
  permanently full for every cell but 0 — pinned); `MIN_RING_ENTRIES = 2`, `MAX = 2^16`; 4 × 4096
  through capacity 64 exactly once; the lap arithmetic across the u32 wrap.
- **The doorbells** (`doorbell.zig`): the completion doorbell's completer (`seq` bump → `parked` →
  `wake_at` → the mark-valued latch CAS; all `seq_cst`, the house Dekker idiom since 0.17 has no
  fence builtin) and parker (REGISTER → SNAPSHOT → compose the mark; the at-snapshot-unpaid keep /
  spent replace / earlier-governs rules as a pure `composeMark`), `NO_PAY_RECORDED`,
  `reapBatchWakeThreshold = clamp(pending/4, 2, pending)` (floor division; the liveness ceiling
  dominates), the submit side `ringSubmit` (SeqCst bump, wake iff parked — row 302),
  `ServicePark` (parked := 1 on every owned header → snapshot every doorbell → the caller's rescan →
  `futex_waitv` on all, bounded `SERVICE_PARK_MAX_NS = 5 ms`), the spin governor (`windowUs`,
  `busyCeilingPct = 100 − ceil(100·lanes/cores)`, `foldParkNs` α = 1/8; 100 µs MEASURED), the
  three knob faces. A Linux test: a parked owner is woken by a submit on ANY owned session.
- **The wire** (`wire.zig`): the 312 B `Il0Blob` (magic `SQZIL0\0\0`, abi, flags, 48 B commit so
  `-dirty` is seeable, two 108 B socket fields, 32 B nonce) and the eleven frames as `extern
  struct`s — `Hello` 112 B (kind, sdk_abi, geometry request; no pid/uid — row 303), `SessionOk`
  16 B (tag + features + the fd; the sealed header is the geometry authority), `Refuse` 16 B (class +
  the SDK window), `Bind` 8 B (durability), `BindOk` 32 B (`rights {read, write, odirect}`,
  binding, ino, durability — row 304), `BindRefused`, `Unbind`, `AdminHello`, `AdminOk`, the two
  length-prefixed admin frames (a lying length is never a slice authority). `RefuseClass` 1–9
  (`mount` = 4, `rights` = 9). `decodeFixed` total; fuzz 4000 rounds over every decoder.
- **Sizing** (`sizing.zig`): `ilSessionsDefault = clamp(ceil(cpus/4), 2, 16)`,
  `ilDrainLanesDefault = clamp(ceil(3·cpus/8), 2, 64)` with the W8→W24 sweep beside it and the
  pointwise dominance sweep (1..=4096), `kernelLaneMinDefault = clamp(round_down(memBW × 2.3 µs /
  2, 4 KiB), slab, max_op)` (15 GB/s → 64 KiB, 100 → 112 KiB, 125–147 → the charter's 140–164 KiB),
  the 64 MiB `probeMemBandwidth` timed once (row 311), `resolveMaxOpBytes` (the transport's
  `max_write`, row 306), the arena family composed from `mem/sizing.zig`, `ctlConnCap` /
  `listenBacklog`.
- **The rendezvous** (`rendezvous.zig`, portable): `NonceState` (60 s TTL, lazy rotation,
  current ∨ previous ⇒ a blob read at `t` is valid 60–120 s), `buildBlob`, `socketName`
  (`sqz-il0-<pid>-<rand>`), `socketDir` (knob / `/run/squeezefs` / XDG / `/tmp/squeezefs-il-<uid>`),
  `dirTrusted`, `peerTrusted` (no "or our own uid"), THE FD SCREEN as pure rules over `FdFacts`
  (`O_PATH` before the device — load-bearing; `S_ISREG` (a directory only for the SDK's HELLO
  credential); `st_dev`; append/sync/dsync/tmpfile/`nlink == 0`; access mode → rights, O_DIRECT
  travels), `admitHello` (steps 2–8 in order: disabled → version (abi, kind, commit / SDK window) →
  dirty (the seam counts) → nonce → peercred → no fd / the screen's class → budget (shed, cap,
  per-uid)), `grantGeometry` (the SDK's request clamped; below `max_op` degrades to the default),
  `preCheck`, `HeartbeatWatch` (1 s = 20 × 50 ms slices unchanged ⇒ probe; row 301), the
  constants with WHY (`IDLE_REAP_NS`, `CTL_*`, `OP_TIMEOUT_NS`).
  `rendezvous_linux.zig`: `sendCtl`/`recvCtl` with the `SCM_RIGHTS` discipline (own EVERY fd from
  `cmsg_len`; `MSG_CTRUNC` or > 1 fd ⇒ all closed, `Protocol`; an fd census pins "nothing leaks"),
  `peerCred`, `fdFacts` (`F_GETFL` + `statx`), `deviceOf` (row 309), `listenAbstract`, `listenPath`
  (dir screen, dirfd-relative unlink/chmod, bind through `/proc/self/fd/<dirfd>`, 0666, unlinked at
  close), `connectLadder` (abstract first; the path rung only after rung 1), `authenticatePeer`
  (rung 2), `hello`, `acceptHello` (the daemon's one-connection half: HELLO → facts → the pure ladder
  → `shm.createSealed` → SESSION_OK + fd, or REFUSE counted per class), `socketAlive` (the
  heartbeat probe), `fillNonce` (`getrandom`).
- **The memfd** (`shm.zig`): `createSealed` (`MFD_ALLOW_SEALING`, the header written once, the ring
  seeded, `F_SEAL_SEAL|SHRINK|GROW`), `checkSeals` (rung 3), `fileSize`, `mapSharedPmdAligned`
  (over-reserve → `MAP_FIXED` → trim; plain fallback), `advise` / `populateCollapse`
  (`MADV_COLLAPSE = 25` pinned) / `bindArenaToNode` (`mbind(MPOL_PREFERRED)`).
- **THE ONE CLIENT LIBRARY** (`client.zig`, raw `std.os.linux` only): `Session.establish` (pre-check
  → connect → rung 2 → HELLO → rung 3 → read the sealed header's geometry off the first page →
  `total_bytes == st_size` → map PMD-aligned → validate → `my_generation` → require `futex_waitv` →
  the lane gate published to the stats page), `bind`/`unbind` (a futex mutex over the ctl socket),
  the one-op protocol (`claimSlot` from a rotating hint → fill → publish → push → `ringSubmit` →
  spin 4096/256 → park in 50 ms slices re-checking DONE, poison, the heartbeat ladder, the deadline →
  consume → release; `-EINVAL` ⇒ fallthrough, else kernel-identical errno; abandoned slots never
  reused), slot runs (`claimRun`: aligned pass then greedy; the `SQUEEZEFS_TEST_IL_MAX_RUN_SLOTS`
  seam), pipelined flights (`MAX_FLIGHTS = 64` per batch under ONE doorbell, offset-order reap,
  POSIX prefix semantics), no-wait tickets (`submitNoWait` / `ticketDone` / `pollTicket` —
  verbatim `-errno`, no fallthrough post-acceptance), the completion-doorbell park
  (`cqeParkBeginBatch` / `cqeParkEnd` / `waitAny` with the userspace admission pre-check),
  `routesKernel` / `noteKernelRoute`, `poison` (shutdown, never close) vs `poisonChild` (close,
  never shutdown), `poisoned()` (the flag or a generation bump).
- **The `il0` face** (`kv_fs.zig`): `Il0Face` + `bindIl0`; GETXATTR of `user.squeezefs.il0`
  answers the armed host's 312 B blob (size probe, ERANGE), ENODATA unarmed; the reserved-family
  screen already filters LIST and refuses SET/REMOVE with EPERM; never stored.
- **Observability**: the `ipc` stats family (`ipc_sessions_total`, `ipc_session_admission_ns`,
  `ipc_admission_refusals`, the nine `ipc_bind_refused_*`, `ipc_binds_dev_override`,
  `ipc_cqe_wake_{writes, collapsed, elided}`, `ipc_service_parks`); the stats-page words the
  client writes (`lane_gate_*`, `il_park_eras`, `il_slot_reroutes`).

## Models and the weakening that fails each (`model/explorer.zig`, SC + explicit store buffers)

| Model | Holds (states) | Weakening | Fails |
|---|---|---|---|
| `slot_core_model.Completion` — server begin/result/DONE/wake, client spin/WAITER/admit/consume over an explicit futex bucket | 64 | the result store lands AFTER the DONE swap (the Relaxed-swap shape) | FinalViolated, 13 states: a ready/woken consumer reads result 0 |
| `slot_core_model.TwoLives` — two lives on one slot + a stale gen-1 probe | 675 | the generation bump moved after `publishSubmitted` | InvariantViolated, 122: the server completes life 2 before the bump; the probe attributes its DONE to gen 1 |
| `slot_core_model.Snapshot` — the one post-begin_serve read with a hostile sibling | 32 | `publishSubmitted` Release → Relaxed (the descriptor store stays buffered) | InvariantViolated, 14: the snapshot reads the pre-submit seed |
| `ring_core_model.Ring` — 2 producers + 1 consumer, capacity 2, across the lap boundary | 1540 | the value store after the publishing seq store | InvariantViolated, 40: the consumer pops a phantom (the seeded 0 on lap 0, the previous lap's value after) |
| `doorbell_model.Cqe` — D1 (a reaped prior op's late doorbell), D2 (DONE + doorbell), P (k = 1 parker), a futex bucket | 6032 | drop the completer's drain | InvariantViolated, 116: the bump is still buffered at P's admission — a wake to nobody, P sleeps |
| | | drop the parker's drain | InvariantViolated, 74: D reads `parked == 0` while P's registration sits in its buffer |
| | | snapshot before the register's drain (the Dekker load-before-store shape) | InvariantViolated, 80 |
| | | the 0/1 flag latch with a parker-side clear (the 2026-09-06 class) | InvariantViolated, 1123: D1's pay consumes the era's flag toward a parker not yet asleep; D2's real completion collapses |
| `doorbell_model.Submit` — producer push/bump/drain/parked-load, consumer parked/drain/snapshot/rescan/admit | 58 | snapshot AFTER the rescan (the pre-fix order) | InvariantViolated, 46: the push + bump racing the rescan's tail fold into the expected value |
| | | drop the producer's drain | InvariantViolated, 45 |
| | | drop the consumer's drain (the reference's production shape had no producer fence) | InvariantViolated, 55 |

The shipped `Cqe` era pays ≤ 1 wake FOR THE PARKER'S MARK in every schedule; a pre-snapshot
completer's pay for a STALE mark is the benign over-wake the latch's `NO_PAY_RECORDED` initial record
exists for, and the model's final check distinguishes the two.

## End to end (Linux, in-process; the `StubHost` in `client.zig` accepts one session over a real abstract `SOCK_SEQPACKET` listener, `SCM_RIGHTS` and a real sealed memfd, and serves its ring on a thread with the heartbeat stamped every pass)

- The ladder admits a shim; BIND serves reads (an `offset + i` pattern) and writes; a 0-byte read is
  the liveness ping; the lane gate is published; every slot returns to FREE.
- A 40-page + 77 B op pipelines 11 flights over slot runs (10 of the 4-page `max_op` window), reaps
  in offset order, releases every slot; capped at 1 slot per run it serves the same bytes via 41
  flights.
- The screen at BIND: a read-only binding's write completes `-EBADF`; `O_PATH`, `O_APPEND`, a
  directory ⇒ `flags`; a memfd (a regular file on a foreign device) ⇒ `mount`.
- Every HELLO refusal, each counted on its class and reported through the client's `Refusal`: a
  stale nonce (two rotations), a wrong commit (the pre-check skips the round trip; the daemon refuses
  authoritatively), `-dirty` on either side (admitted only with the seam on BOTH sides, counted
  `ipc_binds_dev_override`), an `O_PATH` / `O_APPEND` / directory credential, a foreign device, a full
  budget.
- The client refuses the daemon: an unsealed memfd (rung 3, nothing mapped), a world-writable path
  rendezvous (rung 1, no `connect`), a foreign peer uid (rung 2, no HELLO — observable only as a
  non-root tester; root is trusted for every owner).
- 100 k no-wait tickets at depth 32 with a 0–20 µs randomised server delay and the completion-
  doorbell park (`k = clamp(pending/4, 2, pending)`): nothing lost, no abandoned slot, ≤ 1 wake
  paid per park era (asserted per era against `ipc_cqe_wake_writes`), `il_park_eras` exact, every
  slot FREE; 8 s on the venue.
- Heartbeat death: a daemon that stops stamping and severs its socket is detected in ≈ 1 s (≥ the
  1 s stale bound, ≪ the 20 s seam deadline); the session poisons, every later op falls through,
  BIND answers `Poisoned`, UNBIND emits nothing.
- A generation bump by the daemon poisons the client lazily; `poison` shuts the socket down and the
  fd stays valid; the SCM_RIGHTS discipline leaks no fd (an `/proc/self/fd` census).
- Not in this wave: the `il0` xattr on a REAL mount needs the armed host (`-o interception`), which
  is wave B's; the FUSE-dispatch test (`kv_fs.zig`) covers the exact GETXATTR/LISTXATTR/SETXATTR
  path through the loopback dispatcher.

## Gates

`zig build gate` green on macOS (portable cores, models, wire, sizing, the pure rendezvous, the
kv_fs face) and on the venue (the Linux half: the futex face, the memfd, the sockets, the ladder end
to end).

## Wave B2 — the LD_PRELOAD shim: `libsqueezefs_il.so`

### What landed (`src/shim/`, `src/shim_so.zig`, `tests/shim_census.zig`; spec `docs/porting/il_shim.md`)

- **The artifact**: `zig build shim` runs an explicit `zig build-lib -dynamic -lc -z nodelete
  -fsoname=libsqueezefs_il.so -O ReleaseSafe` (0.17's `Step.Compile` has neither knob) with the
  daemon's `build_options` as the identity; installed to `lib/`. The census (`tests/shim_census.zig`,
  a rails step on Linux) parses the ELF: `SONAME libsqueezefs_il.so`, `DT_FLAGS_1 = NOW|NODELETE`,
  `libc.so.6` the only `DT_NEEDED`, and the defined dynamic symbols EQUAL the ONE table — the 59 of
  the spec + `openat2` + `signalfd4` + `squeezefs_il_bind` (62), nothing of Zig's or the client
  library's. 2.0 MB on aarch64 (ReleaseSafe with debug info).
- **`interpose.zig`**: the comptime `table` (name, libaio default version, API flag) generates the
  exports (`@export` only when the root carries the build identity: the daemon's test binary links
  the same bodies in linked mode and interposes nothing of itself), the real-symbol cache
  (`dlsym(RTLD_NEXT)`, `dlvsym` for `io_*`, a missing symbol cached as missing), and one wrapper per
  symbol obeying resolve-eagerly/call-lazily: no body ever calls a real function. Synthesized
  answers where libc has no symbol: raw syscalls (`lseek`, `close_range`, `sendfile`,
  `copy_file_range`, `splice`, `openat2`, `signalfd4`, the creators, and `io_*` — libaio's functions
  are the kernel's). Variadics: true C-variadic exports on x86_64 (`@cVaStart`; `mode` read only
  under `O_CREAT|O_TMPFILE`, the `fcntl` arg only for commands that take one); on aarch64 the LLVM
  `VaList` is disabled by std ("miscompilations"), so the fixed-register AAPCS64 form reads the same
  register under the same predicates (0002 row 341).
- **`fd_table.zig`**: two-level slots (4096 × 1024) naming cells by index; cells in lazily mapped
  chunks, recycled through a tagged-index Treiber free list (0002 row 344); a lookup pins (acquire
  refusing 0) and re-validates the slot; `onClose`/`onDup`/`onCloseRange` (O(1) segment skips)/
  `unbindIno` (per-cell flush + per-last-ref drop)/`forkFlushAll` (epoch bump, one flush per cell
  armed under the previous epoch under a 1024-spin `tryLock`)/`clearAllInChild`; the offset mirror
  with the authority predicate and the Dekker pair (`publish` = SeqCst store + SeqCst load of the
  arm; `disarmIfCurrent` = SeqCst swap + SeqCst load, once, never for a stale or never-armed cell);
  the lane gate's sticky `kernel_latch`; `DevCache` (1024 × 4-way, `dev + 1`, a deadline per entry,
  the S1 backoff 60 s → 15 min) and `MappedInoSet` (8192 fingerprints, never evicts, overflow poisons
  classification).
- **`classify.zig`**: the 8-step ladder over injectable probes; the 32-slot registry keyed
  `(st_dev, shard)` with `shard = fd % clamp(ceil(cpus/4), 2, 16)`; `Session.establish` with the
  three rungs; the refusal code table (1–8, `0x1000|errno`, `0x2000|errno`, `0x4000|class`,
  `0x8000|class`), the once set (128 pairs; past capacity it stops remembering, never printing), the
  permanent-vs-backoff cache verdict per code; `Reason` (21 bail-out rows) counted process-wide
  and on the session's stats page (`preload_passthrough_ops[24]`, `preload_bind_refused[10]` —
  additive layout fields at offsets 48 and 240); `release` sends the Unbind through
  `Session.tryUnbind` and defers on a busy ctl lock (0002 row 350).
- **`ops.zig`**: positional (shape rows → pin → rights → gate → ring → outcome map; `real_poisoned`
  demotes + releases), offsetful (the latch, the gate trigger under the lock, mirror vs raw
  `SEEK_CUR` start, `publish` or write-through, `Errno` moves neither offset, the disarm-once on a
  fallthrough), vector (total-bytes gate, segment by segment, partial counts, the short-and-dying
  park of `f_pos`), `lseek` (SET/CUR arithmetic exactly `vfs_setpos`; END/DATA/HOLE re-seed and keep
  the arm), the `f_pos`-consumer demotes, `close`/`dup`/`close_range`/the creator sweep/`fcntl`
  (locks, `O_APPEND`, an `O_DIRECT` toggle)/`mmap` (the per-inode unbind; `MAP_SHARED` poisons the
  inode)/`posix_spawn` (the fork flush).
- **`aio.zig`**: the userspace ABI mirrors (64 B iocb, 32 B event, pinned), a 256-slot context
  registry with collision retro-neutralization, the mixed-batch core generic over the two lanes
  (kernel runs flush before each ring op; a partial run ends the prefix; a ring refusal reroutes at
  its position; a first-op kernel errno propagates), the reap (ring first into the caller's array,
  the kernel lane non-blocking while ring ops are pending, the batch-mark park in the deep regime, 4
  sweeps then k = 1 in the sparse one, 1 ms / 5 ms bounds), `io_cancel` → `-EINPROGRESS` for a ring
  op, destroy abandons; the knobs `SQUEEZEFS_IL_REAP_PARK_MAX` (24), `_REAP_QUANTUM_US` (50),
  `_SPARSE_BATCH_MARKS` (on) read once per process.
- **`lifecycle.zig`**: ctor-free lazy state (uninit → initializing → ready | failed) built by the
  `open` family / `squeezefs_il_bind`; the thread-local guard; `pthread_atfork(prepare, null, child)`
  registered once at the first state build (PREPARE = the epoch bump + flush; CHILD = flag-poison +
  `close(2)` of every ctl copy, every slot cleared, the registry emptied so the child establishes its
  own sessions); `shimPanic` (poison all, one raw line, abort); linked-mode detection by
  `AT_SECURE` + `dl_iterate_phdr` (0002 row 348). Async-signal posture: no table operation takes a
  lock; the per-description offset lock is held only across a serve window; an `open` from a
  handler over a non-interposed frame pays the control plane (as the reference did).
- **Knobs**: three A/B-lever rows for the reap; the shim reads `SQUEEZEFS_IL_SESSIONS`, `_SPINS`,
  `_KERNEL_LANE_MIN`, `SQUEEZEFS_IPC_ARENA_THP`, the test seams, through the client library.

### Models and the weakening that fails each (`shim/fd_table_model.zig`)

| Model | Holds | Weakening | Fails |
|---|---|---|---|
| `MirrorDekker` — op `store offset → fence → load armed` vs demote `swap armed → fence → load offset`, a per-thread store buffer | yes | the op side's fence removed (the Release-store shape: the offset stays buffered while the arm is loaded) | FinalViolated: the final offset reaches neither consumer |
| `RefcountDrop` — two closers of dup siblings + a pinning reader on one cell | yes | the acquire's zero refusal removed (plain `fetch_add`) | InvariantViolated: a dead cell resurrects / `last` reported twice |
| `ForkEpochBind` — a bind racing the fork PREPARE's epoch bump | yes | the snapshot taken at BIND time instead of before the `open` | FinalViolated: a description the child shares is under an authoritative parent mirror |

### End to end (Linux, in-process linked mode against the client library's `StubHost`; `shim/e2e.zig`)

- The ladder (FUSE magic + the blob injected, `fstat`/`F_GETFL`/`lseek` real) establishes one
  session per shard and binds; positional reads are byte-exact against the host's pattern, writes
  acked; `served == ring ops`; the shape rows and the wrong direction are uncounted real calls; the
  lane gate's over-threshold op is counted on both ledgers.
- Offsetful ops ride the mirror (kernel `f_pos` still 0 after two writes), `SEEK_CUR`/`SEEK_SET` are
  arithmetic, `-1` is EINVAL, `SEEK_END` re-seeds and keeps the arm, a dup shares the mirror, the
  LAST close sends exactly one Unbind (observed on the host).
- Vector ops serve segment by segment with one `f_pos` advance; `RWF_NOWAIT` demotes (the kernel
  catches up) and takes the real call; a raw-closed number reused by `socket()` is swept;
  `close_range` sweeps and `CLOSE_RANGE_CLOEXEC` does not.
- `F_SETLK` and `F_SETFL+O_APPEND` unbind after flushing; `F_SETFL` without a class change keeps the
  binding; `MAP_SHARED` unbinds both fds of the inode and poisons it (the next open is a counted
  `mapped_inode` passthrough); `MAP_PRIVATE` unbinds without poisoning; the fork flush bumps the
  epoch, writes the mirror through, demotes; the child clear drops every slot.
- The daemon dying mid-op (frozen + severed): the op returns `.real` inside the heartbeat ladder
  (≪ 5 s), the session is poisoned, the fd released with no ctl traffic, the real call answers, a
  later open on the mount is a silent `poisoned` passthrough.
- libaio: 8 iocbs in one `io_submit` (6 ring, 2 kernel-class to `/dev/null` through the raw
  syscall lane) accepted in order; `io_getevents` merges them with exact cookies and `obj`
  identity; 2 reads back pattern-exact; `io_cancel` of a ring op answers `-EINPROGRESS` (or the
  real call once reaped); `served == 8`.
- The table/interpose unit rows: the linked-mode wrappers pass a plain file's I/O through byte-exact
  (`pread64`/`pwrite64`/`lseek`/`read`/`dup`/`close`, EBADF verbatim) and open no session.

### The shim smoke (venue; `zig build shim-smoke`, after the bench smoke; `shim/smoke.zig`)

- Row 1 (ext4 under `LD_PRELOAD`): dd 64 MiB → sha256sum, cp, cat, truncate, tar byte parity, exit
  codes; the shim silent; the C program (`pread`/`pwrite`/`lseek`/`fstat`/`pwritev`/`dup`/`mmap`/
  `fork`+`exec`/`F_SETLK`) and the aio harness identical WITH and WITHOUT the shim (stdout + exit
  status) — the harness found the shim's `io_*` through `RTLD_DEFAULT` (`lane=shim-or-libaio
  libaio=absent`: the venue has no libaio; the raw-syscall arm served it).
- Row 2 (a SqueezeFS mount WITHOUT `-o interception`): the same battery, pure passthrough,
  `ipc_sessions_total 0`, the shim silent (ENODATA ⇒ the negative cache). Finding: the kernel's own
  aio PWRITE on the plain mount completes `-EIO` with and without the shim (0002 row 351; the host
  wave's).
- Rows 4a–4c (the armed mount: the `-dirty` pairing refusal counted once per mount, the engagement
  law over `ipc_ops_{read,write}`, the C bail-outs served, kill -9 mid-op ⇒ the passthrough result)
  are written and **PENDING**: on this tree the daemon answers `-o interception: not a squeezefs
  mount option` (the host wave lands it); the smoke prints the pending line and passes on rows 1–3.
  The venue's IOPS, when the armed rows run, are scoping only — the 1,017,548 warm / 622,112
  device-true rows remain the reference-hardware bar.

### Gates

Linux (clean worktree at HEAD + this wave): `zig build test -Dtest-filter=shim.` 66 pass / 1 skip
(the smoke's skip off its staged cwd), `rails` 5/5 + the census, `shim-smoke` rows 1–2 green with
rows 4+ pending. macOS: the shim module is Linux-only (`squeezefs.zig` gates it) and the gate stays
green. Two runs were `terminated with signal KILL` while another agent's gate ran its `pkill
squeezefs` cleanup on the shared container (the test binaries are `squeezefs-*`); re-runs passed.

## Wave B3 — the native SDK: `libsqueezefs_sdk`, the second client kind

Spec: `docs/porting/il_sdk.md` (§1 the surface, §2 the lease law, §3 the second kind, §4 the window,
§5 durability, §10 Zig notes); decision 0003 §F "The SDK"; decision 0002 rows 330–336 (the better
designs taken: ONE error convention, the two-part ABI word, one session per handle, base+offset
addressing with an explicit `-ENOMEM`, no silent POSIX fallback, the directory credential, records
outside the arena). Modules: `src/sdk/{sdk, version, buffer_lease, buffer_lease_model, session,
abi}.zig`; `include/squeezefs_sdk.h`; `tests/sdk/{c_smoke.c, c_smoke_main.zig}`; the header census in
`tests/rails.zig`; `build.zig` steps `sdk` and `sdk-smoke` (the latter in the gate on Linux); one knob
row (`SQUEEZEFS_TEST_SDK_IL0_BLOB`, a test seam). Additive to `ipc/client.zig`: `submitArena` /
`kickDoorbell` / `waitTicket` / `ticketResult` / `releaseTicket` / `geometry` / `arena` (arena-native
tickets, no slab copy), `pub CtlLock`, `pub StubHost` with a byte store and the BIND durability echo.

### What landed

- **The surface** (`abi.zig`, 28 `export fn sqz_*`, `callconv(.c)`, nothing else exported — pinned by
  `nm -D` on the venue): `sqz_version/abi`; `sqz_last_error/last_error_text/strerror`;
  `sqz_session_open/close` + the granted geometry (`arena_bytes/slots/max_op_bytes`);
  `sqz_open/close/fsync/fstat/fsize/fd/durability`; `sqz_buf_alloc/free/addr/len`;
  `sqz_pread/pwrite/preadv/pwritev` (positional only, base + offset, in place); `sqz_submit/poll/wait`.
  ONE convention: every fallible call returns a negative errno and never writes `errno(3)`;
  constructors return NULL with the cause thread-local. The records are `extern struct`s with
  comptime pins (`sqz_sqe_t` 48 B, `user_data` at 40; `sqz_cqe_t` 16; `sqz_iovec_t` 24;
  `sqz_session_opts_t` 24; `sqz_open_opts_t` 8; `sqz_stat_t` 96 — statx-derived, so no arch-specific
  `struct stat`) and the header `_Static_assert`s the same numbers.
- **The buffer-lease law** (`buffer_lease.zig`): ONE packed word `{state u8 | inflight u24}`;
  `submit` (CAS +1, FREED/POISONED refuse), `complete` (CAS −1 while APP_OWNED; a POISONED record
  keeps its count), `free` (FREED ⇒ `-EINVAL`; POISONED ⇒ FREED + quarantine; APP_OWNED ⇒ one
  strong CAS `{app_owned,0} → {freed,0}`, any inflight ⇒ `-EBUSY` with the word unchanged — never
  deferred), `poison` (SUBMITTED ⇒ POISONED). The buddy allocator at the 4 KiB granule over the whole
  arena (natural alignment ⇒ every buffer 4 KiB-aligned whatever its size; a non-pow2 arena is seeded
  as its binary decomposition and merges only inside the arena); the record pool outside the arena,
  one record per granule; the census `allocs ≡ frees + live + poisoned_live` and the quarantine that
  only grows. Property test: 3000 random alloc/free rounds on 64-, 96-, 3- and 1000-granule arenas —
  aligned, inside, non-overlapping, free bytes exact, coalesced back to the seed.
- **The second kind** (`session.zig`): `sqz_session_open` opens the mount root
  `O_RDONLY|O_DIRECTORY|O_CLOEXEC` as the HELLO credential (the kind-SDK screen admits a directory),
  `fstatfs` (`FUSE_SUPER_MAGIC`) + `fgetxattr(user.squeezefs.il0)` (the seam reads a file instead, so
  the tests and the C smoke run against the in-process stub host), the coarse `ipc_abi` pre-check,
  then `client.Session.establish(kind = .sdk, sdk_abi = WIRE part, arena/slot request)`; the arena is
  bound at establish so `sqz_buf_alloc` precedes any `sqz_open`. Files: the real `open(2)` with
  `O_CLOEXEC` forced, `O_APPEND/O_PATH/O_TMPFILE` refused up front, `O_SYNC/O_DSYNC` ⇒ class SYNC,
  BIND with the durability word, a grant below the request is `-EPROTO` (strengthen-only), `flags/
  rights` ⇒ `-EINVAL`, `mount` ⇒ `-EXDEV`; `sqz_fsync` is `fsync(2)` in every class. Sync ops stage
  ≤ 64 flights per doorbell over `submitArena` (descriptor flag `F_ARENA_NATIVE`, `arena_off =
  buf.off + buf_off + cursor`), reap in offset order with POSIX prefix semantics; the batch API keeps
  one op record per SQE and one flight per slot (the ticket IS the slot), folds chunked ops into ONE
  CQE, drains a ready ring first, parks on the completion doorbell with `k = clamp(pending/4, 2,
  pending)` in 50 ms slices re-checking the heartbeat ladder and the op deadline; `sqz_submit` returns
  the prefix under slot exhaustion, `-EINVAL` on the first bad SQE with nothing submitted.
- **Poison** = `-ENOTCONN` for every later call: the heartbeat watch (≈ 1 s), the op deadline (the
  last rung), a generation bump, the impossible ring push, and the fork child (`pthread_atfork`,
  AS-safe: `poisonChild` + the lock-free `poisonAll`). Once: every SUBMITTED buffer → POISONED, every
  live batch op → a `-EIO` CQE on the ready ring with its unfinished flights abandoned (slots never
  recycled), every file's inflight count dropped so `sqz_close` works; `sqz_session_close` refuses
  `-EBUSY` only while files are open or buffers are SUBMITTED. The unload pin (`pinSelf`:
  `dladdr` + `dlopen(RTLD_NOLOAD|RTLD_NODELETE)`) runs at the first open — an atfork handler can never
  be unregistered.
- **The window** (`version.zig`): `sqz_abi() = wire << 16 | sdk`; HELLO carries the wire part alone;
  the daemon's `{cur, cur−1}` set is judged by `rendezvous.admitHello`; the refusal texts name both
  values ("sdk abi outside the daemon's window (app 1, daemon accepts 2..3)", "ipc abi mismatch (sdk
  1, daemon 2)") once per `(mount device, reason)` on stderr with the `squeezefs-sdk:` prefix and every
  time in the thread-local last error. The SDK never reads a commit: a `-dirty` daemon admits it while
  it refuses a clean shim (row G).
- **The header + the C smoke**: `squeezefs_sdk.h` hand-written (0.17's `-femit-h` emits nothing);
  the rails census parses `export fn sqz_*(…) callconv(.c) R` out of `abi.zig` and `R sqz_*(…);` out of
  the header, maps every Zig type to its C spelling through ONE table and requires equal arity and
  types both ways (plus "nothing but `sqz_*` is exported"; the parsers have their own red test).
  `tests/sdk/c_smoke.c` (C11, `-Wall -Wextra -Werror`) calls every symbol against the in-process stub
  host hosted by `c_smoke_main.zig`: NULL handles, a bad `struct_size`, the geometry request granted
  (32 MiB over a 16 MiB default), a buffer before any open, the durability classes and `O_APPEND`,
  a `pwrite/pread` round trip byte-exact in place, base + offset, `-EINVAL` past the buffer, `fsync/
  fstat/fsize`, 8 tickets under one doorbell with `-EBUSY` on every free while SUBMITTED, a chunked
  op into one CQE, free after the CQE / `-EINVAL` on the double free / `-EINVAL` on a batch naming a
  freed buffer, `preadv/pwritev`, `-EBADF` on the wrong direction, the close order; then the death
  scenario: kill → the op in flight `-EIO`, every later call `-ENOTCONN`, the poisoned buffer still
  readable and freed into the quarantine, close succeeds. Linked twice: against `libsqueezefs_sdk.a`
  (`bundle_compiler_rt`) and against `libsqueezefs_sdk.so.1` (SONAME verified with `readelf -d`).

### Models and the weakening that fails

| Model | Holds (states) | Weakening | Fails |
|---|---|---|---|
| `buffer_lease_model.Lease` — submitter (CAS +1 → publish, or the undo on a poisoned session), daemon (completes, or never after a poison), reaper (CAS −1), freer (one free at any time), poisoner (flag, then the lease) over ONE packed word | holds | the freer's one CAS split into `load; if inflight == 0: store(FREED)` | InvariantViolated: the submitter's CAS lands between the two and publishes a range the freer then recycles — a FREED range under daemon custody |

### End to end on the venue (in-process stub host, no daemon, no mount)

- Kind SDK admitted with a directory credential; a 2× arena request granted and 1000 slots clamped
  to the ring; a buffer before any file; a 3-page + 17 B round trip byte-exact against the stub's
  store; base + offset windows; `-EINVAL` past the buffer and for a negative offset; SYNC granted,
  value 2 refused, `O_APPEND` refused; `-EBADF` on a read-only handle's write; `sqz_session_close`
  `-EBUSY` until the files close; the census identity holds; `sdk_descriptor_rejects` 0.
- **64 MiB**: the whole default arena as ONE buffer, written and read back through 64 flights of the
  1 MiB window under one doorbell each, byte-exact (xxh3 of the buffer == xxh3 of the stub's 64 MiB
  store); every slot FREE after; a three-window vector folds with the prefix law at the store's end.
- **The batch API**: 8 tickets, one doorbell, `-EBUSY` ×8 while SUBMITTED, every `user_data` back,
  bytes in the store; 70 one-page SQEs over 64 slots ⇒ a 64-SQE prefix; a 70-page op ⇒ 18 flights,
  ONE CQE.
- **Death**: a frozen + severed stub poisons the parked `sqz_wait` in ≥ 1 s and ≪ 5 s with the op's
  `-EIO` CQE, cause `heartbeat`; the buffer POISONED, its free quarantines 1 granule; every later call
  `-ENOTCONN`; one abandoned slot. A frozen stub with a live socket poisons a sync op at the 300 ms
  seam deadline (`-EIO`, cause `deadline`).
- **The window**: a `-dirty` daemon admits the SDK and refuses a clean shim; `sdk_abi = 2` against
  `{1}` is refused `version` with min = max = 1; the `ipc_abi` mismatch text names both values,
  prints once per mount and is in the last error every time; a missing blob is `-ENODATA`; a file
  as the mount path is `-ENOTDIR`.
- **Fork**: the child finds the session poisoned (cause `fork`), the SUBMITTED buffer POISONED, data
  calls `-ENOTCONN`; the parent's lease is untouched and its session serves on.
- Not in this wave: the real-mount run (`-o interception`) — the host wave's armed mount was not up
  when this landed; the SDK's path to it is the same `fstatfs` + `fgetxattr` the shim takes, exercised
  here through the seam. The product daemon's barrier-before-DONE for a SYNC binding and the
  kind-SDK admission of `O_SYNC` fds at BIND are daemon-side (the host wave); the stub echoes the
  durability word and the SDK enforces the grant ≥ request law on its side.

### Scoping numbers (venue; not performance claims)

In-process stub host (serves by `memcpy` into/out of a flat store — the daemon's data plane is not in
the loop), `--release=safe`, 8 vCPUs, one app thread:

| Row | Number |
|---|---|
| 64 MiB `sqz_pwrite` (64 flights of the 1 MiB window, one doorbell) | ≈ 7.3 ms/op (≈ 8.8 GB/s) |
| 64 MiB `sqz_pread`, in place | ≈ 4.6–5.3 ms/op (≈ 12–14 GB/s) |
| 4 KiB sync `sqz_pread` RTT (claim → publish → doorbell → spin → consume) | ≈ 1.7 µs (≈ 580 k ops/s) |
| batch depth 32, 100 k 4 KiB reads (`sqz_submit` + `sqz_wait`) | ≈ 49 ms = ≈ 2.0 M ops/s, 0 park eras (the stub completes inside the spin) |

These bound the SDK's own overhead (lease CAS, slot claim, descriptor publish, fold) — nothing here
is an il row (charter engagement law: `ipc_ops_*` deltas on a real daemon), and nothing is claimed
before G-S2's counted A-B-B-A against `_raw`/the shim.

### Gates

`zig build gate` green on macOS for the portable half (the lease law, the buddy allocator, the model
and its weakening, the version words, the header census rail; the SDK steps refuse with their reason
off Linux); `scripts/venue zig build gate` green on the venue — unit (the SDK's 7 e2e tests against
the stub host), rails (7/7 incl. the header census), smoke, bench-smoke, and `sdk-smoke` (the C
program against the `.a` and against the `.so.1`).

## Wave B1 — the daemon host: session admission, the service threads with the sync read fast path, sever-at-dequeue and placed sever, the direct-drive lanes

Spec: `docs/porting/il_daemon.md` (§1 the service-thread model, §2 the sync read fast path, §3
writes, §4 the direct-drive lane, §5 lease/heartbeat/reap/shed/poison/teardown, §6–§8
knobs/gauges/constants/tests/quirks). Charter §7 (the daemon's duties, KD-11, the engagement law),
§4 (handler lanes for the demotes), §6.2–§6.3, §9, §10. Decision 0003 §F; decision 0002 rows
312–319. Modules: `src/ipc/host.zig` + `src/ipc/host/{stats, session, service, direct,
severed_pool, placed, placed_claims_model, inval, e2e_linux}.zig`; the `ipc_host` stats family;
6 knob rows (`SQUEEZEFS_IPC_DD_SHARDS`, `_INVAL_WINDOW_MS`, `_DD_COOP_TASKRUN`, `_DD_LANE_FLUSH`,
`_DD_INLINE_REAP`, `SQUEEZEFS_IL_DIRECT_WRITE`); `-o interception` in `fuse/mount_options.zig`
and `cli/mount.zig`; the `.placed` payload vehicle through `data/{write_admit, accumulate,
patch_w1}.zig` and `fuse/handler/{filesystem, kv_fs}.zig`.

### What landed

- **The host** (`host.zig`): `IpcHost.arm` builds the two listeners (the abstract name and the
  path under the socket-dir ladder), the nonce rotation behind the `il0` blob (`kv_fs.bindIl0`,
  unarmed at teardown), the admission over wave A's `admitHello` with the budget `budget/8`, the
  per-uid session count, the balance-first owner pick over the NUMA partition
  (`layout.pickOwner`/`nodePartition`), the sealed memfd + `SessionOk`; spawn-on-admission of the
  service threads up to `clamp(3 × cpus/8, 2, 64)` (`sqz-ipc-svc<N>`), the `sqz-ipc-ctl` poller
  (BIND/UNBIND/EOF/malformed ⇒ poison) with the idle reap on its tick, the R5 shed hook (a bound
  session is never shed; an unbound one is), and the teardown ORDER: every live session poisoned
  and torn down → service threads kicked and joined → dd reapers drained → listeners
  `shutdown(2)`+joined+closed → poller → the xattr unarmed. A session is destroyed by its LAST
  share (`SessionMap`): the registry's, the poller's, the taker's at disarm/reap/shed — the e2e
  now asserts the daemon's own allocator audit prints nothing at a graceful exit (the first
  version leaked the taker's share for a session live at disarm: 728 B + the 64 KiB bindings
  table per session; red through the daemon log, then fixed).
- **KD-11**: `-o interception` arms the host AND forces kernel write-through
  (`resolveInterceptionPosture`: `interception` + an explicit `writeback` is a usage error; the
  FUSE INIT reply drops writeback-cache); every shipped ring write runs the invalidator ladder
  (`host/inval.zig`: ONE striped per-ino word `{hwm: u48, last_fire_s: u16}` — bind and the first
  write of a window notify the whole inode, a growing write inside the window notifies attrs
  only, a non-growing one is suppressed; row 316).
- **The service thread** (`service.zig`): one session per thread for life; the dequeue loop is the
  spec's — epoch-cached owned set → budgeted round-robin → `tryBeginServe` → the in-flight gate
  (`slots`, VAL-5c) → the ONE `snapshot` → validate (binding live, direction, len ≤ max op,
  arena window) → dispatch → sweep-end flush; the completion order ledger → result → DONE+WAITER
  wake → doorbell; the park ladder over `doorbell.ServicePark` (spin governor, snapshot-BEFORE-
  rescan, `futex_waitv` on all owned doorbells, 5 ms bound, the heartbeat stamped per pass);
  every reject through the ONE errno table into the descriptor's result, each a counted, logged
  tripwire. The SYNC READ FAST PATH: `WarmLadder.probe` into the arena window under the
  try-locked shared guard (ONE copy); a miss/lock/policy demote drops the guard FIRST and hands a
  handler frame to the home lane, which completes through the same order; the copy ledger
  `ipc_read_{arena_copy, arena_dma, demote}_bytes`. WRITES: sever-at-dequeue — the arena bytes
  move into a `SeveredPool` slab (`slots = clamp(arena_cap / max_op, 1, 65536)`, derived; a miss
  allocates aligned, a put recycles; the bound frees past the free list) so the client's slot is
  released at completion while the data path owns its own pages; placed sever — a whole-block
  stream chunk claims pages in a shared `Assembly` keyed `(ino, block)` (the page-claim bitmap +
  the landed bitmap); the first merge ADOPTS the backing with the landed runs as coverage
  (`accumulate.checkoutAdopting`, pointer-proof: a foreign pointer never adopts) and seals new
  claims out; a chunk the seal beat falls back to the plain sever; the cohort reaps at the last
  drop. `WritePath.write` takes the `.severed` / `.placed` payload vehicles.
- **The direct-drive lane** (`direct.zig`): shards = the ONE width function; per shard a
  512-entry ring with `COOP_TASKRUN` (two issuers — the owning service thread and the reaper —
  so `SINGLE_ISSUER|DEFER_TASKRUN`, the FUSE rings' posture, is out), lane-scoped flush (the
  owner enters ONLY its own shard), reaper/drain fusion (the owner's inline reap under the
  try-locked CQ gate; the reaper's 100 ms `EXT_ARG` wait), the MEM-7c leak law (`leakInflight`:
  shares stay out, guards released, clients `-EIO`, counted; row 315). The W1-shaped write serves
  INLINE: `patch_w1` eligibility → `beginDirectWrite` (the sole-owner try-lock on the block
  stripe) → the fence pair → ONE SQE straight from the arena → the postlude on the CQE
  (`finishDirectWrite`, the inval ladder, the completion order). libaio arrives decomposed: the
  shim submits one ring op per iocb, the daemon serves each as an ordinary op and knows no
  batch; `io_getevents` is the client's fold of the per-slot DONE states (the events-array
  order law lives there).
- **Stats**: the `ipc_host` family (sessions live/refused/reaped/closed/poisoned, owners and
  service threads, arena bytes, binds/unbinds, ops and bytes by direction, fast-path serves and
  the three demote classes, handoffs, the copy ledger, drain/flush/ingress histograms, parks and
  wakes, descriptor rejects, severs and the pool, placed severs/adoptions/refusals/reaps, the dd
  rows — inline serves, fallbacks by reason, lane flushes, inline/reaper reaps, stalls, leaked
  in-flight — the inval rows) with three closures (`reads`: fast-path serves + read handoffs ≡
  read ops; `writes`: dd inline serves + write handoffs ≡ write ops; `dd`: inline serves +
  fallbacks ≡ dd submits attempted).

### Models and the weakening that fails each (`host/placed_claims_model.zig`)

| Model | Invariant | Weakening that fails |
|---|---|---|
| The placed-claims Dekker (writer in window ↔ seal + adopt; store buffers explicit) | an adoption never lands over a writer inside its window; a claim after the seal never lands | drop the claimer's fence (`no_claimer_fence`), drop the adopter's fence (`no_adopter_fence`), or check the seal before publishing the claim (`seal_check_before_publish`) — each `InvariantViolated` |

### Unit tests (portable where pure; the rest Linux)

The dequeue loop over a RAM session with a stub client (a session-less owner parks on its private
word, admission wakes it, reads and writes complete in the order, every slot returns); the five
validation rejects each a counted tripwire through the ONE table; the three poison reasons stop the
drain with the generation bump while an honest full window never poisons; the park ladder's
lost-wake freedom (20 k ops with randomised gaps over a parking owner, none stranded); the pool's
slot law, hit/miss ledger and bound; the placed registry (shared assembly, overlapping claims fall
back, the cap, adoption with landed runs, the Dekker refusal, the reap at zero); the inval ladder's
four verdicts and the second wrap; the bindings table and the in-flight ledger; the dd lane's
inline W1 serve against a scratch device (one SQE, the lane flush, the inline reap, the bytes at
the mapping, the word re-stabilized); the reap tick, the width tie, the kill-priv class; the stats
family's render and closures.

### End to end on the venue (`host/e2e_linux.zig`; `smoke:`-tagged, `zig build smoke`)

A real `squeezefs mount -o interception` (256 MiB meta + two 2 GiB sparse data images under
`/build/tmp`) and wave A's `client.zig` IN-PROCESS over the real `il0` xattr:

- a session established over the rendezvous, a bind on a real fd (`rights.read and .write`); the
  mount's own flags read `interception: true`, writeback off;
- 4 MiB through the ring as 4 flights of 1 MiB → 4 severs, read back byte-exact through the
  KERNEL path; one 4 MiB `pwrite` of block 1 → `ipc_placed_severs ≥ 4`, `ipc_placed_adoptions
  == 1`, no refusals or fallbacks, byte-exact through the kernel;
- an aligned cold 512 KiB read (the dest lease: `read_dest_dma_bytes` grows, never published),
  an unaligned cold read (the whole-block walk fills the hot tier) — two demotes — then the warm
  aligned read: `ipc_fast_path_serves + 1`, `ipc_read_arena_copy_bytes + 512 KiB`;
- a W1-shaped 4 KiB overwrite: `ipc_dd_inline_serves + 1`, `patch_edge_rmw_reads` unchanged,
  byte-exact through the kernel;
- 8 no-wait 32 KiB tickets (one ring op per iocb) reaped in offset order, every result its length;
- the fd screen through the real session: a read-only binding's write is `-EBADF` (the one
  descriptor-reject tripwire of the run), `O_PATH`, a directory and `O_APPEND` refuse as `flags`,
  a memfd as `mount`;
- kernel `fsync` after the ring writes; `ipc_sessions_poisoned == 0`, `ipc_inval_notifies ≥ 1`;
- `umount` with the LIVE session: the daemon exits 0, no mount is left, the client sees the
  generation bump (`poisoned()`, a `pread` falls through), the log reads "torn down: host
  shutdown", exactly one tripwire line, and NO allocator leak line.
- Death: `kill -9` the daemon under a bound session ⇒ the client poisons inside the ladder
  (< 5 s bound, ≈ 1 s observed), far inside the 20 s seam deadline; the mount is detached.

### What the first armed mount surfaced (fixed here, red-first)

- **The inline → striped promotion refused every sub-block image** (`plane.ownBlock`): the
  own-block proof was minted over the image's grain (`ceilGrain(stored.len)`) while
  `publishLayout` checks `covers` over the mapping's SLOT (the whole block for a whole-block
  mapping; the `DmaProof` doc's own law) — so on a set without a cache path (staging off) the
  SECOND write of any small file answered `ProofMissing` → EIO, on the kernel path too
  (`dd bs=4k count=1; dd bs=4k seek=1` ⇒ "Input/output error", reproduced at `dev` HEAD in a
  clean worktree). The proof now covers the slot (the tail of a fresh mint is unnamed: every
  read and existing-bytes seed is bounded by the layout's `size`); the e2e's libaio-shaped row
  (8 × 4 KiB writes into a fresh file, kernel path then ring) is the repro.
- **The dd lane's guard crossed threads without the ledger knowing** (`write_admit.beginDirectWrite`):
  the (3) block guard is try-acquired on the service thread and released at the CQE on
  whichever thread reaps; the acquiring thread's lock-order ledger kept a stale (3), so its next
  op-guard acquire fired `lock_order_inversions` — on the unit runner's main thread, against a
  later unrelated test. `lock_order.handedOff(.block)` on `.begun` (the `GuardSet` precedent).
- **The shim smoke's rows 4a–4c ran for the first time** (they were `PENDING` without an armed
  mount): row 4a's once-rule is per process (`dd` and `sha256sum` each print the identity
  refusal once); `statsTail` must return THIS dump (a `numberAfter` over the whole log re-read
  the mount's first dump). With both corrected the battery engages: `sessions +14,
  ipc_ops_* sum 47074` over dd 64 MiB + sha256sum + cat/cp/tar + the C program + the libaio
  harness, and the kill -9 row's app call returns the passthrough result.

### Scoping numbers (venue; not performance claims)

The venue is a 9-vCPU aarch64 VM on the 7.2.6-sqz kernel with 15 GiB, data on two sparse ext4
images — nothing here approaches the reference hardware's 1,017,548 / 622,112 IOPS rows, which
remain the bar for G-S2's counted A-B-B-A on real devices. Observed on one e2e run (the
`ipc_host` family before the umount): `ipc_ops_read 11` (1 fast-path serve, 10 miss demotes
= 2 cold + 8 tickets), `ipc_ops_write 9` (8 severs of 1 MiB — 4 of them placed into one
assembly adopted once, 3 slabs pooled = 3 MiB — + 1 dd inline serve of 4 KiB), `ipc_bytes_in
8,392,704`, `ipc_bytes_out 1,835,108`, `ipc_read_arena_copy_bytes 524,288`,
`ipc_read_demote_bytes 1,310,820`, `ipc_dd_shards 4`, `ipc_service_threads 1` (one session ⇒
one lane spawned), `ipc_arena_bytes 67,260,416` (64 MiB + the 0x25000 header/slot/ring pages),
drain pass mean ≈ 1.2 ms over 8 passes (a 4 MiB sever per pass), ingress mean ≈ 0.97 ms over 21
ops, 8 wakes / 25 timed parks, `ipc_inval_notifies 2`, `attrs_only 8`, `suppressed 1`. Both e2e
tests run in ≈ 34 s wall including format, mount and teardown.

### Gates

`zig build gate` green on macOS (1696 pass) for the portable cores (the pool, the placed
registry and its model, the inval ladder, the stats family — referenced from `ipc/ipc.zig`'s
test root; the host, service and dd modules compile on Linux only). On the venue: unit
(1991 pass, 0 error logs), rails 7/7, the shim census, bench-smoke, shim-smoke (rows 1–4c
above) and the smoke's interception rows green in every run. The smoke's cluster KILL ROWS
(a)/(b) (rung 6's two-daemon kill rows) flaked in 3 of 4 full-gate runs AND in 1 of 3
standalone `-Dtest-filter='KILL ROWS'` runs with the interception host unarmed (a torn tree
file; `dead_appender_orphans` 72; the storm child exiting before B's park) — a timing row of
that rung, outside this wave, recorded here so the next full run is read right.
