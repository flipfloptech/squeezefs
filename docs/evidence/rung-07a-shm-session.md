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
