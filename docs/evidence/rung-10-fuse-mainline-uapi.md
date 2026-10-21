# Rung 10 — the FUSE transport on mainline's buffer-pool UAPI (2026-10-21)

The owner's decision (after `rung-10-kernel-mainline.md`): the daemon's fast buffer modes speak a
kernel UAPI upstream rejected; Linux 7.3 shipped buffer pools and per-file zero-copy instead; we do not
keep the fork — port the transport to mainline's UAPI, the series shrinks to real fixes and proposable
patches. Branch `feat/fuse-mainline-uapi`; venue the workstation (`local`, `7.2.8-9.33-omarchy-sqz`,
the legacy kernel — the mainline rung is developed against the v7.3 sources and the fake-kernel seams;
`linux-mainline-sqz 7.3.0-1.7` is installed below the default Limine entry and the owner boots it).
Decision 0003 §BE; 0002 row 550; charter §3/§4 amended; the spec `docs/porting/fuse_mainline_pools.md`
supersedes `fuse_kmbuf_zc.md` for the shipped contract.

## 1. The protocol, as read off v7.3 (`fs/fuse/dev_uring.c`, `include/uapi/linux/fuse.h`)

A queue is created by `ADD_QUEUE` (opcode 3; `FUSE_URING_ZERO_COPY` is its one flag, CAP_SYS_ADMIN,
EEXIST on a second add) and handed ONE contiguous user region by `ADD_BUFPOOL` (opcode 4: `uaddr`,
`len`; the kernel splits it into `len / max_payload_sz` buffers; with `IORING_URING_CMD_FIXED` +
`sqe.buf_index` the pool is REGISTERED and every later REGISTER/COMMIT on the queue must carry the same
flag and index); both complete inline, before any REGISTER (a REGISTER on a queue nobody added makes it
a per-entry-payload queue — the pre-7.3 shape). A REGISTER carries the header iovec and a `{NULL, 0}`
payload iovec, plus `ent_zero_copy_buf_index` — the entry's own slot in the ISSUING ring's registered-
buffer table. Per request the kernel claims a free pool buffer iff the request has copyable payload,
copies the in-payload there and reports `offset` (into the pool) and `payload_sz` in the 32-byte
`fuse_uring_ent_in_out` (the series' padding word is `offset`); an entry keeps its buffer across
consecutive payload-carrying requests and recycles it at a payload-less fetch. Zero-copy is granted
PER OPEN FILE (`FOPEN_IO_URING_ZERO_COPY` on the OPEN/CREATE reply → `args->zero_copy`) and applied to
READ and WRITE alone: the request's folios are registered as a bvec at the entry's slot (DEST for READ,
SOURCE for WRITE), the header's `FUSE_URING_ENT_ZERO_COPY` says so, the folio copy is skipped both
ways, a zc WRITE's `payload_sz` still counts the data, the slot is unregistered at the COMMIT. The
COMMIT reads `payload_sz` and the out-args from the pool buffer (a zc READ: `payload_sz = n`, nothing
copied, the folio tail zeroed). Several rings per queue work without a patch: the pool is user memory
keyed by offset, each ring registers the same region at the same index (the kernel never checks that a
COMMIT comes from the REGISTERing ring — an `-EXDEV` hardening is proposable). The INIT offer carries
`FUSE_HAS_IO_URING_BUFPOOL` (bit 43) whenever `fuse.enable_uring` read Y when the INIT was sent.

## 2. The mode table (`probes.resolveBufferMode`; `transport_buffer_mode`)

| kernel | INIT bit 43 | kmbuf ladder | `auto` | zero-copy | opens | legacy probes (3/4) sent |
|---|---|---|---|---|---|---|
| mainline ≥ 7.3 | present | absent | **`mainline_pool` (3)** | per queue, iff CapEff bit 21 ∧ the sparse table | served with `FOPEN_IO_URING_ZERO_COPY` (`SQUEEZEFS_FUSE_ZC_OPEN=1`), or zero-message + CREATE-only (`=0`) | never |
| 7.2-sqz (0001–0033) | absent | 38/39 | `zero_copy` (2) / `buf_ring` (1) | the kmbuf slot | `SQUEEZEFS_FUSE_NO_OPEN` as before | yes (retention on zc; FEATURES) |
| stock 6.14–7.2 | absent | absent | `user_ents` (0) | — | as before | never |
| a kernel offering bit 43 AND answering the ladder | present | present | `mainline_pool` | — | — | never (bit 43 ranks above) |

Explicit pins: `pool` refused loud without bit 43 (`BufpoolAbsent`); `bufring`/`zc` refused on a
mainline kernel (kmbuf absent there); `user` always. The daemon writes `fuse.enable_uring=Y` BEFORE
`mount(2)` so bit 43 (and 41) are in the offer on the first mount after boot. The test seam
`SQUEEZEFS_TEST_MAINLINE_POOLS=1` answers the bufpool probe present on any kernel.

## 3. What was built (`6072017`, `da547c8`)

- **ABI** (`abi.zig`): bit 43, opcodes 3/4 in their mainline meaning beside the legacy names (the
  collision pinned by comptime asserts), `FUSE_URING_ENT_ZERO_COPY`, `FOPEN_IO_URING_ZERO_COPY`,
  `IORING_URING_CMD_FIXED`, the 40-byte `fuse_uring_cmd_req_mainline` (layout-pinned; the legacy
  struct's union sits inside mainline's padding), `ent_in_out.offset`.
- **The lattice** (`probes.zig`): `bufpoolVerdict`, `Verdicts.bufpool`, the zc surface over either
  carrier, `poolZeroCopy` / `zeroCopyArmed`, `legacyProbesApply` gating the retention and FEATURES
  probes to the legacy rungs; `geometry.BufferMode.mainline_pool` (width 1; rings per queue the derived
  3 with no FEATURES); the knob `pool` value and the seam row.
- **The transport** (`worker.zig`, `session.zig`): `Pool` (one session mapping, a region per queue,
  accounted as `transport_payload_arena`), `setupMainlinePool` (the sparse table: pool at 0, zc slots
  `1 + ent`; ADD_QUEUE + the registered ADD_BUFPOOL on the queue's first ring, both reaped inline and
  checked — any refusal the typed `MainlinePoolRefused`), `prepRegisterMainline` / `prepCommitMainline`
  (the FIXED flag + index 0), `poolWindow` by the header's `offset` (in-region else the session fails;
  off the `payload_sz` grid a tripwire), `zc.mainlineDirOf` (the flag + opcode; no mirror, no readdir
  latch), `zcSlotOf` shifting every bridge by one on this rung; the reply legs (prefilled, direct
  device, fd-source, bounce), the held zc write and the slab unchanged over the shifted slot.
- **The pooled held write** (`tryHoldPooled`, `PayloadLease.acquireHeldPooled`, `prepPoolStore`): a
  WRITE the kernel copied into the pool, when the shape and the filesystem's gate admit it, is held in
  its pool buffer — the bytes in hand AND the slot coordinates; the direct store is `WRITE_FIXED(device ←
  pool @ user address)` on index 0; a reader pays no bridge. The kernel allows it: the pool is an
  ordinary user-registered buffer (`imu->dir` both ways, no FUSE-side lock; the buffer is the entry's
  until its COMMIT's fetch), and the reply's `fuse_write_out` lands in the buffer's first 16 bytes only
  at the commit, after the handler awaited the DMA — the zc slot's lifetime law, unchanged.
- **Zero-copy per open file** (`session.openPosture`, `lifecycle.initFilesystem`, `InitCaps.zc_open` /
  `zc_create_only`, knob `SQUEEZEFS_FUSE_ZC_OPEN`): on a mainline zero-copy session the lifecycle serves
  regular-file opens (the §AF control arm — `atomic_o_trunc` back, `FOPEN_NOFLUSH` on clean handles, the
  `.stats` latch skipped) and ORs the flag into every OPEN/CREATE reply; with the lever off,
  zero-message opens stay and CREATE alone carries it. `EchoFs` mirrors it for the e2e rows.
- **The RFC** (`scripts/kernel/upstream/fuse-zc-no-open/`): `FUSE_IO_URING_ZERO_COPY_DEFAULT`, an INIT
  flag making a `no_open` local open zero-copy; the sketch patch (24 lines on v7.3) is checkpatch
  `--strict` clean and compiles `W=1`; the text carries the measured motivation and the 7.3 A/B owed.

## 4. The zc-open decision (decision 0003 §BE.3)

(a) serve OPEN with the flag — chosen as the default; (b) keep `no_open`, lose zc — the lever's 0 arm
(with CREATE-only zc, which costs nothing); (c) the INIT flag — the RFC. The numbers are the two
measured sides on the same daemon (7.2.6 + the series): zero-message opens bought open/close ×4 6.6×,
open + read + close ×4 5.6×, one thread 1.2–1.4×, mdtest file creation 1.18× (its OPEN + RELEASE were
27 % of 88,018 requests), the data rows unchanged (`rung-10-dev-perf.md` §10.5); zero-copy bought 1M
seq write 1.9–2.5× (13.9 → 27.5 GB/s ×4; io-wq's share of the daemon 26 % → 2.6 %), 4k randread
1.04–1.08×, 64k randread 1.03× (`rung-10-zero-copy-writes.md`). A data mount pays one 13 µs OPEN per
open against 2× on every large write; the metadata workloads (mdtest's create+close, the open/close
loops) keep the lever. Decided by reasoning from the two measurements — the 7.3 A/B below is owed before
the default is called shipped, and the RFC exists so both can hold at once.

## 5. Red → green on this host (the legacy kernel)

- `probes` "the mainline stratum": the lattice table, the ranking above kmbuf, the gates, the gauges.
- `session` "e2e lattice: the mainline stratum": under the seam `auto` resolves MainlinePool, the pool is
  mapped, no legacy probe is sent (retention and rings-per-queue read 0), and the first ring's ADD_QUEUE
  meets this kernel's `RELEASE_PAYLOAD`: **`ADD_QUEUE(zero_copy=true) → -107` (ENOTCONN),
  `ADD_BUFPOOL → 1`** (this kernel's FEATURES mask) — the mount fails with the typed
  `MainlinePoolRefused`, nothing registered, nothing mounted; an explicit `pool` without the offer is
  refused by the bufpool probe. The collision guard, proven against the real legacy kernel.
- `session` "e2e mainline pools": skips here (`no_kernel_feature … absent on 7.2.8-9.33-omarchy-sqz`);
  runs for real on 7.3 (§6).
- `abi` "mainline uring ABI pins", `worker` "mainline SQE bytes" + "the mainline pool", `zc` "the
  mainline direction", `slot` the pooled hold, `lifecycle` "mainline zero-copy per open file", `session`
  "the open posture table", `geometry` "rings per queue" (mainline 3 without FEATURES).
- No regression on 9.33: the full unit suite **2,876 / 2,967 passed, 91 skipped, 0 failed** (every
  legacy lattice, zc READ/WRITE, retention, rings-per-queue and no-open e2e row among them); `rails`
  and `fmt` green (the literal rail: the worker's `started` word is an enum; two stale rows retired).
  The gate is the orchestrator's.

## 6. Live validation owed (the owner boots `7.3.0-1.7-mainline-sqz`)

1. `scripts/venue test "e2e mainline pools"`: `auto` mounts `mainline_pool`, `pool_queues_added ≡
   queues`, rings per queue 3, the arena gauge = the pool, /big and /scratch byte-exact through the pool
   (`pool_deliveries` moves; `pool_offset_breach`, `_off_boundary`, `_setup_late_cqes`, `_zc_unarmed`
   all 0), with CAP_SYS_ADMIN the opened file's READ/WRITE flagged (`pool_zc_deliveries` moves), a 1 MiB
   aligned write HELD and stored from the slot; under `SQUEEZEFS_FUSE_ZC_OPEN=0` + no_open the same
   write is the POOLED hold (`transport_pool_write_holds` +1, the bytes exact in the device fd, a
   declining filesystem reads them with no bridge); `bufring`/`zc` refused naming kmbuf. Also "e2e
   lattice: the mainline stratum" in its live arm (the seam inert, the mount serves).
2. The smoke's mount/umount/kill rows on `auto` (the real daemon, `kv_fs` over the pool; the
   `.stats` family carries the `transport_pool_*` fields), `SQUEEZEFS_FUSE_RINGS_PER_QUEUE=1` and 3.
3. Unknowns the source could not settle: `ADD_BUFPOOL`'s accounting of a 32 MiB region per queue against
   `RLIMIT_MEMLOCK` through the ring's `IORING_REGISTER_BUFFERS` (the kmbuf ENOMEM class had a typed
   refusal; here the registration's refusal is `RingSetupFailed` naming memlock); the kernel's split
   (`max_payload_sz`) agreeing with `plan.payload_sz` on this box (`_off_boundary` is the witness); the
   `-EXDEV` question with three rings per queue under load (a COMMIT always rides its own ring here).

## 7. The measurement plan (one benchmark at a time, Tctl ≤ 70 °C, the venue held)

`scripts/perf/quiet.sh ram-sqz`, the RAM-NVMe set, dist builds, 20 s rows, legs A B B A, mean and
spread per row, `tctl_c` reported. Rows: `rr4k-q1j1 rr4k-q32j1 rr4k-q32j4 rw4k-q32j4 rr64k-q16j4
sr1m-q16j4 sw1m-q16j1 sw1m-q16j4 sw4m-q4j4`, plus mdtest 4 × 2,000 and the open/close loops ×1/×4 for
the open posture.

| series | kernel | A | B | what it answers |
|---|---|---|---|---|
| 1 | 7.3 | `SQUEEZEFS_FUSE_BUFFER_MODE=user` | `auto` (= `pool`, zc, opens served) | the mainline rung's own worth on 7.3; `transport_pool_zc_deliveries ≡ READ + WRITE` on B |
| 2 | 7.3 | `pool` + `SQUEEZEFS_FUSE_ZC_OPEN=1` | `pool` + `=0` | §BE.3's trade: the data rows (A's zc) against the metadata rows (B's zero-message opens); the gap is the RFC's value |
| 3 | 7.3 vs 9.33 | the same daemon binary on 9.33, `auto` (= `zero_copy`, 3 rings) | the same binary on 7.3, `auto` (= `pool`) | **the only cross-kernel comparison we allow, stated as such**: the legacy rung's measured numbers (`rung-10-write-iops` wave 2: 4k randwrite QD32×4 1.40 M, 4k randread 1.90 M, 1M seq write ×4 27.5 GB/s) against the mainline rung on the same box — the bar the mainline rung must meet before the legacy rungs and the kmbuf patches are deleted; the kernels differ in more than the FUSE path (7.3 lacks 0026/0030/0031; its io_uring moved), so the column is a target, not an attribution |
| 4 | 7.3 | `pool`, `SQUEEZEFS_FUSE_RINGS_PER_QUEUE=1` | 3 | the carried-over derivation re-measured on the pool |

Every session ends with `fsck --offline` clean; THE KILL MATRIX and the mid-tar kill rows on `auto`
before the first leg.

## 8. Risks

The first ADD_QUEUE on a real pool kernel has not run: the seam proves the bring-up's failure arm, not
its success arm — a wrong byte in the 40-byte command (the layout is pinned against the header, not a
kernel) shows only on 7.3. Under `SQUEEZEFS_FUSE_ZC_OPEN=1` a mainline mount serves opens: the §AF
rows' zero-message assumptions (the `.stats` latch, FLUSH on the worker) switch to the control arm's,
which has its own smoke rows but not under this rung. The pooled hold aliases the reply's 16 bytes
over the data's head — safe by the lifetime law, and `kv_fs`'s held-store path was proven only with
the zc slot's source. The `Pool` pins `nqueues × depth × payload_sz` at registration (1 GiB at 32
queues × 32 × 1 MiB): the same as the kmbuf regions, but now charged through `IORING_REGISTER_BUFFERS`
(memlock without `CAP_IPC_LOCK`). The legacy rungs' code paths gained a `mode` switch each; the full
suite says they still hold on 9.33.
