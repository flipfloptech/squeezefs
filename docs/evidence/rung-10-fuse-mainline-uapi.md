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

## 6. Live validation owed (the owner boots `7.3.0-1.7-mainline-sqz`; items 1 and 2's gate rows ran on 2026-10-22 — §9; item 3's unknowns and §7 stay owed)

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

## 9. Live on 7.3 (2026-10-22, the workstation on `7.3.0-1.7-mainline-sqz`; branch `fix/mainline-gate-7.3`)

**What the probe reported.** Every root mount's INIT line reads `buffer mode mainline_pool (bufpool
present, kmbuf null, zero-copy true, retention absent, rings per queue absent)` — the offer carries
bit 43, the kmbuf ladder finds no opcode, the zc surface is up (CAP_SYS_ADMIN + the sparse table), no
legacy probe is sent, the derived 3 rings per queue. The daemon's mount: 32 queues × depth 21, payload
1 MiB, 96 workers, `MainlinePool zero-copy serves OPEN (SQUEEZEFS_FUSE_ZC_OPEN=1)`; the data plane arms
every zc leg (`held-slot direct store ON, direct device leg ON, fd-source probe serve ON`). The bring-up's
success arm ran for real (§8's first risk): "e2e mainline pools" passes (ADD_QUEUE + the registered
ADD_BUFPOOL on every queue, /big and /scratch byte-exact through the pool, the zc'd READ/WRITE, the 1 MiB
held slot store, the pooled hold under the lever at 0, every law 0), and the WITH-cache data-plane row
counts 243 held stores and 1,496 direct legs over its pass.

**The first gate on it** (`.gate-train/local-7b8bb0f1c330-20261022T015655Z.log`, dev `7b8bb0f`; the
second run identical): 62/75 steps, 3,135/3,257 tests, **112 skipped, 10 failed**. The last 9.33 gate
(`d0fcaf8`): 75/75, 3,166/3,261, 95 skipped.

*What skips, and why* (every one `testkit.skipWhy` naming the kernel feature, by design): the 16
`session` e2e rows of the legacy rungs and the 2 `kmbuf` rows, `uring128`'s kmbuf row and the smoke's
late-bring-up refusal — "the kmbuf register opcode (BufRing; zero-copy and retention ride it) absent on
7.3.0-1.7-mainline-sqz" (the kmbuf UAPI is not in the mainline series); "e2e mainline pools" and the
mainline stratum's live arm no longer skip. The rest of the 112 are the gate's usual `other_step` rows
(the smoke under the unit runner, the nvmet tier, the shim).

*The ten reds — one cause, in two shapes.* Nine were the posture: on this kernel `auto` is MainlinePool
with zero-copy queues, so `SQUEEZEFS_FUSE_ZC_OPEN=1` (§BE.3's default) serves opens whatever
`SQUEEZEFS_FUSE_NO_OPEN` read — every row judging zero-message opens saw served opens and no ENOSYS
latch: `session` "a no-open session" (INIT kept ATOMIC_O_TRUNC), the smoke's seven §AF/§AJ rows (no
`fuse_open_enosys`, FLUSH drains on served handles, `.stats` WRONLY opens refused at OPEN, the close
burst's drains over its bound). The tenth shape was the stratum: "the stock stratum" seam hid kmbuf
alone and the offer kept the pool (`.mainline_pool`, wanted `.user_ents`); the fused poll row judged
`mode == .zero_copy`; `dataPlanePass` judged the zc legs by the kmbuf ladder alone.

*What changed* (`91904da`): every no-open row pins the posture it tests on both strata —
`SQUEEZEFS_FUSE_ZC_OPEN=0` beside `SQUEEZEFS_FUSE_NO_OPEN=1` (`no_open_pin` in `session.zig`, `pinNoOpen`
in `smoke.zig`; inert on the legacy rungs, so 9.33 runs the same mount) and the e2e rows assert
`s.noOpen()`; `EchoFs` mirrors the lifecycle's flip (`InitCaps.zc_open` clears its `no_open`);
`SQUEEZEFS_TEST_KMBUF_ABSENT` hides the bufpool offer too (the stock stratum is a pre-7.3 kernel with
neither UAPI; the row asserts the bufpool verdict/gauge and refuses `pool`); the fused poll judges
`zeroCopyArmed()` (16 fusions of 16 reads on MainlinePool); `dataPlanePass` reads the daemon's INIT line
(`bufpool present`, `zero-copy true`) beside its own kmbuf probe — a kernel with either carrier must arm
the legs. Under the pin the no-open rows read exactly their 9.33 numbers here: the close drain's ONE
ENOSYS and 2 FLUSH drains, the burst's 1 drain (bound 2) + 401 deferred, the write-back timer's 1,079 ms
landing (10 residencies, mean 1,034 ms), the idle drain's 1, 200 whole `.stats` documents over 199
generations, O_TRUNC's ONE ENOSYS on each daemon.

*A product defect the posture uncovered* (`efaaf1d`; not a posture question): under served opens
(mainline's default — and the control arm `SQUEEZEFS_FUSE_NO_OPEN=0` on every kernel) B's `: > d/a` over
A's file wedged 48 s in `open` and answered EIO — "move gate: open met a slot move that did not resolve
in 48000 ms (last answer SlotMoved)". `KvFs.open`'s ATOMIC_O_TRUNC arm ran the LOCAL size path on a slot
another appender holds; `setattr` had the foreign-slot ship (§AL.6) and the OPEN arm never reached it,
because under zero-message opens no OPEN carries O_TRUNC. The arm now delegates to `setattr(size 0)` on a
foreign slot. Red first: the new smoke row "O_TRUNC UNDER SERVED OPENS TRUNCATES" (the O_TRUNC body under
`SQUEEZEFS_FUSE_NO_OPEN=0`: red without the fix — the 48 s EIO —, green with it; `slot_move_unresolved` 0
on both daemons, asserted on the no-open row too). Known and unchanged: the shipped setattr carries no
`kill_suidgid`, so a foreign-slot truncate (SETATTR or O_TRUNC) does not strip S_ISUID/S_ISGID.

*The mount's default posture on mainline* stays §BE.3's (a): zero-copy per open file by serving opens.
Nothing found here says it is wrong — the one defect was in the control arm's code, exposed by the
posture, and is fixed; the trade (one 13 µs OPEN per open against 2× on every large write) is the
measured one and the 7.3 A/B (series 2 of §7) is still owed before the default is called shipped. Nothing
was measured in this pass (the venue was shared with the gate train and another agent).

*Counts.* The ten rows + the new row + the control-arm close drain: **5/5 each** (the eight smoke rows as
one filtered process, 48/48 × 5, 52–56 s a run; the unit e2e rows 45/46 × 5 with the fused poll's BufRing
arm skipping by name). The whole unit suite once, `-Dfresh=true`: **2,870 / 2,982 passed, 112 skipped,
0 failed** (the skips: 17 `session` kmbuf rows — the fused poll's BufRing arm now among them, after its
`auto` arm passes —, 2 `kmbuf`, 1 `uring128`, the `other_step` rows); `rails` 29/29; `zig fmt --check`
clean. The full gate is the orchestrator's.

## 10. The 7.3 matrix (2026-10-22, `dev` `4efb8ec`, dist build, the RAM-NVMe set; one session at a time, the host cooled ≤ 70 °C between, `tctl_c` in `results.tsv`)

Three sessions of §7's plan (series 1, 2 and the cross-kernel row of 3), one leg each, 20 s rows,
the venue held by `scripts/perf/quiet.sh`. The daemon ran `transport_buffer_mode` 3 (MainlinePool)
and `fuse_uring_rings_per_queue` 3 in every 7.3 session; the 9.33 column is the last `dev` session
on the sqz kernel (`ram-sqz-rw3-striped-A`, 2026-10-21, kmbuf zero-copy, 3 rings).

| row | 9.33 `auto` (kmbuf zc) | 7.3 `auto` (pool, zc, opens served) | 7.3 pool, `ZC_OPEN=0` (zero-message opens, copies) | 7.3 `user` (stock rung) |
|---|---|---|---|---|
| rr4k-q1j1 (IOPS / p50 µs) | — | 154 k / 5.3 | 101 k / 4.1 | 66 k / 7.8 |
| rr4k-q32j4 | **1.88 M / 64** | **1.30 M / 90** | 1.33 M / 82 | 0.73 M / 173 |
| rw4k-q1j1 | — | 102 k / 8.5 | 107 k / 8.3 | 51 k / 17.8 |
| rw4k-q32j4 | **1.33 M / 86** | **1.17 M / 91** | 1.15 M / 91 | 0.78 M / 142 |
| rr64k-q16j4 (GB/s) | 58 | **58.2** | 39.1 | 9.2 |
| sr1m-q16j4 (GB/s) | 73–81 | **74.6** | 30.2 | 16.7 |
| sw1m-q16j4 (GB/s) | 50.6–52.2 | **52.7** | 25.6 | 15.1 |

**What it says.**

- **§BE.3 holds: zero-copy per open file is what carries bandwidth.** Taking the opens away
  (`ZC_OPEN=0`) leaves the 4k rows where they are (1.33 / 1.15 M — the same within a leg's
  spread) and halves every bandwidth row (64k 58 → 39 GB/s, seq read 75 → 30, seq write 53 → 26:
  the copies). The default stays (a); the RFC (`scripts/kernel/upstream/fuse-zc-no-open`) is worth
  exactly the zero-message opens' metadata gain, still unmeasured (the open/close loops of §7).
- **The pool rung's own worth on 7.3 is 1.8× on 4k IOPS and 4.5–6× on bandwidth over the stock
  `user` rung** (series 1): the port earns its keep on a mainline kernel.
- **The cross-kernel gap is in the 4k round trip, not the data path:** bandwidth rows are at
  parity with 9.33 (58.2 / 74.6 / 52.7 against 58 / 73–81 / 50.6–52.2); 4k randread is −31 %
  (1.30 vs 1.88 M, p50 64 → 90 µs at the same host cores ≈ 18 and daemon cores ≈ 11.5), 4k randwrite
  −12 % (1.17 vs 1.33 M). The 9.33 series carries 0031 (one header copy per direction, per-entry
  zero-copy scratch, one lock section per completion — "4–5 % of the read's cost" measured there)
  and 0030; mainline's `dev_uring.c` has neither in that form (`rung-10-kernel-mainline.md`: 0031
  part 4 is upstream as `fc60668db855`, the rest is written against kmbuf). The next lever on 7.3
  is therefore the kernel's: 0031 re-expressed on mainline's request path and posted — and, on the
  daemon's side, a per-request profile of the pool rung's completion path (`MX_PROFILE=1` on
  `rr4k-q32j4`) to see what of the 26 µs the daemon owns.
- QD1 4k reads: 154 k on `auto` against 101 k with `ZC_OPEN=0` — one leg each; the open posture
  should not touch a QD1 read loop on an open file, so this is a spread question until A-B-B-A says
  otherwise.

Sessions: `/build/perf/mx/ram-sqz-rml73-{default,zcopen0,user}-r1/`; fsck clean after each.
