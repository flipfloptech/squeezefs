# SqueezeFS-Zig client bypass (rung 7): shm session, IPC host, LD_PRELOAD shim, SDK
The interception path under `src/ipc/`, `src/shim/` and `src/sdk/`: modules, build targets, protocol numbers, design calls (decision 0003 §F), shim implementation rules, and what the Rust reference actually shipped.

## Modules and build targets
- Session + client (wave A, 0373895): `src/ipc/{layout,slot_core,ring_core,doorbell,wire,sizing,rendezvous,rendezvous_linux,shm,client}.zig`; 5 explorer models with 10 weakenings failing red.
- `src/ipc/client.zig` is the ONE shared client library used by both the shim and the SDK (charter §7: 'the same wire as a second client kind').
- Daemon IPC host (1a171ba): `src/ipc/host.zig` + `src/ipc/host/*`; `-o interception` arms it and forces kernel write-through (KD-11).
- Shim (22d4912): `src/shim/*`; `zig build shim` → `lib/libsqueezefs_il.so`; 62 exports pinned by `tests/shim_census.zig`; DT_FLAGS_1 NODELETE.
- SDK (a789779): `src/sdk/*`, hand-written `include/squeezefs_sdk.h`, `tests/sdk/c_smoke.c` (pins the header via a C program in the gate); `zig build sdk` / `sdk-smoke`.
- Anchors consumed in `src/squeezefs.zig` under `pub const ipc = struct {...}`: `@anchor:ipc_session_import`, `@anchor:ipc_host_import`, `@anchor:ipc_client_import` (+ matching `_test`).
- `src/sync/futex.zig` has a public cross-process face plus `waitMany` over `futex_waitv`; the shm words live in a MAP_SHARED memfd so FUTEX_PRIVATE is wrong.
- Test seam: `SQUEEZEFS_TEST_SDK_IL0_BLOB`.
- Evidence note: `docs/evidence/rung-07a-shm-session.md`; specs `docs/porting/il_{session,daemon,shim,sdk}.md`; 0002 rows 300–355.

## Protocol numbers (taken from the reference)
- il0 rendezvous xattr blob is 312 B: magic `SQZIL0`, 48 B commit, two 108 B socket fields, 32 B nonce.
- Default layout offsets: header 0x0 / ring 0x1000 / slots 0x4000 / stats 0x24000 / arena 0x25000; inside the header doorbell @64, cqe @128, geometry @192.
- The reference shim interposes 59 symbols (the Zig shim exports 62).
- svc/dd thread count = clamp(ceil(3·cpus/8), 2, 64), from a W8→W24 sweep (622k→700k→690k→626k IOPS); lane-scoped flush gave +39.7%/+27.9%.
- Reference-hardware IOPS bar: 1,017,548 warm / 622,112 device-true (rung 9 box brackets); venue IOPS are scoping only.

## Design calls (decision 0003 §F)
- The header heartbeat IS implemented: the daemon stamps it; the client detects death via 1 s escalation → socket probe → poison (charter §7 names it; a dead daemon must be detected in O(1 s), not 30 s).
- Submit-side doorbell uses SeqCst fetch_add + fence (the reference's loom model checked a stronger program than its shipped Release+SeqCst-load code).
- Shim gets `-z nodelete` via an explicit `zig build-lib` Run step (Zig 0.17's std.Build.Step.Compile has no such knob); fallback would be a runtime `dlopen(RTLD_NOLOAD|RTLD_NODELETE)` self-pin.
- Shim uses raw `std.os.linux` syscalls only (std.posix inside the shim would resolve to the shim's own exported symbols).
- 'No panics escape' (charter §12) = interposer bodies total by construction + a panic handler that poisons sessions before aborting.
- SDK is built from the design doc (no reference crate exists); static .a uses `bundle_compiler_rt = true`.
- On aarch64 Zig 0.17 disables VaList under LLVM, so the shim's variadics use the fixed-register AAPCS64 form there.

## Reference implementation facts (Rust squeezefs)
- The shm header `heartbeat` word has zero production writers; the shim poisons on a 30 s deadline instead.
- The SDK is design-only: no `crates/squeezefs-sdk`, `docs/design-sdk.md` is 'Draft — rev 1'.
- A refused establish is retried on every open of a non-interception mount (no negative cache); the design's `preload_*` client counters and `SQUEEZEFS_IL_PARANOID_FSTAT` never landed.
- Source map: `crates/squeezefs-ipc/src/{layout,ring_core,slot_core,cqe_core,comm_core,spin_governor_core,thp,wire,sizing,numa_core}.rs`; `src/ipc_host.rs` (3,871 lines), `src/ipc_service.rs` (1,836), `src/ipc_direct.rs` (2,578); `crates/squeezefs-preload/src/{interpose(3,086),session(2,052),fd_table,fd_table_core,aio_core,aio_glue}.rs` + build.rs; docs `design-preload-interception.md` (650), `design-sdk.md` (362), `design-il-direct-write.md` (151), `design-one-path.md` (850).