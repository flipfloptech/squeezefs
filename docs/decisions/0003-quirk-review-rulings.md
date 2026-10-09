# 0003 — Quirk review before rung 3: findings, work packages, rulings

Status: ACCEPTED 2026-10-05 with the owner's rulings: Q1 yes (aligned headers), Q2+Q3 yes (one key
layout; `--single-writer` is a one-member forest; the bit-8 partition machinery is deleted), Q4
derive, Q5 the fail-stop lattice (the per-read timeout and its knob go), Q6 rename the thread classes
to `sqz-fuse-lane<N>` / `sqz-fuse-ur<qid>` (charter §9 amended), Q7 every `.tuning` row becomes an
A/B lever or is deleted, Q8 `dist` = ReleaseSafe + fat LTO, Q9 deferred to rung 3 as recorded.
Inputs: `docs/audits/2026-10-05-quirks/{format,runtime,foundation}.md` (105 findings over
everything landed in rungs 1–2). Law: decision 0002 — the charter's mechanisms, invariants and
derivations are requirements; the reference's incidental choices are not; no compatibility with
the reference is owed; we have no release yet, so a format change today costs nothing later.

## A. Work packages (clear-cut: a charter clause decides them)

### WP-F — Format v4: one format act (format audit rows 1, 3–4, 8–11, 16–22, 26, 32, 36–37)

`superblock.version = 4`. The Rust binary then refuses every Zig volume loud with its own message,
and no half-changed image can be misread by it; its `fsck` oracle is retired for the trees and OUR
invariants (replay ≡ RAM, SMO completeness, the kill matrix) stand on their own.

| Row | Change | Charter clause |
|---|---|---|
| 1 | ONE key layout — the charter's, with the kind byte. Delete the kindless "legacy" layout, `KeyFraming`, `splitForestKey`, the builder re-key, the `extra = 1 if forest` size term. `--single-writer` becomes a one-member forest (control tree + native slot tree; no appender directory) | §5.1 Keys list the kind byte; §5.5 "one mixed-kind KV tree per routing slot" — **landed 2026-10-05** (0002 row 35; `key.checkSlotTreeKey`, `tree.TreeIdentity`/`tree.route` = the ONE routing law; the backend mounts the forest) |
| 3–4 | Fixed-width `extern struct` inode value with explicit `rdev` and `flags2`; drop the LEB128 version tag (verified non-canonical: `0x81 0x00` decodes) | §5.1 "packed inode {…} ≈ 96 B"; §12 canonical total codecs — **landed 2026-10-05** (0002 row 36; 68 B `InodeValueWire`, the Δ field table derived at comptime) |
| 8 | One magic convention (ASCII tags) | §12 — **landed 2026-10-05** (0002 row 38) |
| 9–10 | ONE stamped bset frame (drop v1); check `reserved` on read | §5.5 frame screen "v2"; §12 total decode — **landed 2026-10-05** (0002 row 39) |
| 11 | Page header: `appender_id u32` (not `writer_id u16` from `MAX_APPENDERS = 16`); pads validated | §5.5 capacity `heap/16 ÷ ring` — **landed 2026-10-05** (0002 rows 37, 40; PAD = entry kind 1, charter §5.1 amended) |
| 16–18, 36–37 | Ledger record: ONE fixed layout with an explicit version; drop decode-by-length, the frozen `routing_width`, the bit-8 suffix, `tree_id = 0` as a root identity, `stride == 0` singletons | §12; §5.4 W is derived — **landed 2026-10-05** (0002 row 41) |
| 19, 21, 22 | Superblock re-laid: explicit-endian fields, checksum LAST, `volume_len` so the DUR-5 backup is self-located, `hash_seed` an independent RANDOM seed (today `xxh3(uuid)`) | §5.1 "random `hash_seed`"; §12 — **landed 2026-10-05** (0002 rows 42, 43; `geometry.mintSetIdentity` draws from `Io.randomSecure`) |
| 20 | Feature word: `version = 4` absorbs every always-on bit; four real capability bits remain (`SYMMETRIC_PLANE`, `LAYOUT_DELTAS`, `BLOCK_MAP_TREE`, `VOLUME_LIFECYCLE`); DELETE the bit-8 partitioned-append machinery (~135 refs, the pre-forest multi-writer model) | §1.7 "Capability bits mean one complete thing each"; §1.9 no dead code — **landed 2026-10-05** (0002 rows 43, 45; the ino lanes and the RAM appender gate went with it) |
| 26 | Validator enforces `heap_start % node_size == 0` | §5.1 — **landed 2026-10-05** (0002 row 43) |
| 32 | Stop writing the `format_config` JSON xattr on ino 1 | 0002 row 8 — **landed 2026-10-05** (0002 row 46) |
| 29, 31 | Comment fixes (16 PiB; `MAX_ENTRY_LEN` WHY) | §1.5 — **landed 2026-10-05** (0002 row 47) |
| 25, 34, 38 | Rung-6 layouts (alloc deltas vs DATA-alloc deltas as distinct kinds; the appender page and directory) written into the porting specs as v4 decisions so rung 6 never re-imports the reference's layout | §5.5 — **spec amended 2026-10-05** (0002 row 44; `docs/porting/kv_alloc_superblock.md` §"v4 decisions", `kv_journal_ledger.md` §"v4 decisions") |
| — | The reference oracle (`fsck --offline`, the byte-identity cross-checks, the reference FIND-VS-A fixture) | 0002 §3 — **retired 2026-10-05** (0002 row 50; the Zig invariants are the oracle; `tests/fixtures/` holds a Zig-generated FIND-VS-A fixture) |
| — | `core/errors.zig` adopted across `src/meta/*` (WP-C row 23's second half) | §12 — **landed 2026-10-05** (0002 row 48) |

Landed as one branch of commits on `dev` (2026-10-05); net ≈ −1,500 lines. KEEP (charter-mandated
or good): kind-byte placement asymmetry (refs by block = the refcount range probe), `seq ≡
position`, `0xFFFF`, the node header, `KVAB` 32,576 bits (derived), the ledger/journal offsets,
`K = 38`, u32 block index, u16 slot. PAD-by-payload became PAD-by-kind with the aligned entry header
(Q1).

### WP-R — Runtime correctness (runtime audit rows 3, 4, 6, 9, 20, 21, 1–2, 5, 15, 27, 31)

| Row | Change | Why |
|---|---|---|
| 4 | Connect threshold writeback: the checkpoint task drains the tree's `MaintenanceQueue` (today two queues never meet; the "one bset page" writeback never runs on a mounted volume) | §5.3 Writeback — a bug — **landed 2026-10-05** (`checkpoint_cycle.MaintenanceSource` bound round-robin over the trees' queues; the task-private ArrayList deleted; red first: a Linux backend e2e over the threshold) |
| 6 | SMO pre-lock barrier failure must fail the SMO (today `catch {}`; pointer records could publish non-durable images) | §1.8 fail loud — a bug — **landed 2026-10-05** (`SmoJournal.barrier` → `BarrierFailed`, counted, on the latch's barrier class; the build abandoned, extents released; red first on `smo.TestVolume`) |
| 3 | Lane barriers advance DUR-3 `reusable_upto` (today only the checkpointer's barrier does; `KickMark.flush` dead) | §5.3 ring reclamation — a bug — **landed 2026-10-05** (`Checkpointer.barrierStarted`/`barrierLanded` + two lane hooks; `KickMark.flush`, `requestFlush`, the tick's deferred barrier, `meta_flush_deferred`, `fsync_pending`/`deferred_barrier` and `coveringBarriers` deleted; red first: a backend cadence cycle + `fsync`) |
| 9 | One space-wake notify (today split in two; a 10 ms tick is load-bearing) | §9 no timer-dependent liveness — **landed 2026-10-05** (`Checkpointer.space_wake`; the task-loop bridge deleted; `meta_conveyor_park_tick_rescues` + the `hold_at_park` seam pin the wake window; red first: a direct `checkpointNow` waking a registered parker) |
| 20, 21 | Rollback/compensation OOM exits are loud + latch; a dead lane ring latches FAILED | §1.8 — **landed 2026-10-05** (`divergedFromReplay` + `conveyor_divergence_*` tripwires, red first with a `FailingAllocator` at the rollback's first allocation; `failAll` fail-stops + `journal_lane_rings_dead`, red first with the `kill_ring` seam) |
| 1–2 | Keep the coalesced posture (§5.2/§5.3 name it; §1.10 `kill -9` holds because a completed O_DIRECT write survives the process); DERIVE the interval once (`CHECKPOINT_MAX_AGE/20` = 50 ms with its reason) in one source | §1.5 — **landed 2026-10-05** (`checkpoint.FLUSHES_PER_CHECKPOINT_WINDOW = 20`, `DEFAULT_FLUSH_INTERVAL_MS` derived, `resolveFlushIntervalMs` explicit-over-derived; the lane's constant deleted; the tie test parses the registry row's default against the derivation) |
| 5 | Collapse the `heap_full` twins (`SpaceAudit.heap_full` dead shadow; `HeapPromises`/`recordCycle` test-only) | §1.9 — **landed 2026-10-05** (`SpaceAudit` = the wedge rung alone; `HeapPromises`/`recordCycle` deleted; the assertions live on `HeapFullPosture` + `SpaceAudit` in `heap_law`) |
| 15 | Never allocate under `inflight_lock` | §1.3 — **landed 2026-10-05** (`inflight` sized at init to `queueCapacity(pages) + 1`, `putAssumeCapacity`; red first with a `FailingAllocator` armed after init) |
| 27, 31 | Rename `sqz-uring-fs<N>`; assert the close ordering `waitHeadStable` relies on | §9 — **27 resolved by WP-C's mount-tag deletion (0002 row 20)**: with the tag gone `sqz-uring-fs63` is 14 chars with both digits intact (the pool cap 64 bounds the index), the literal lives in `uring_fs.zig`, so no rename; the class-row question (pool workers attributed to `other`) stays with the §9 class list. **31 landed 2026-10-05** (`checkHeadStable`: the must-stay-0 `checkpoint_final_tick_open_reservations`; the 64-round wait deleted; red first: an open reservation at the final tick — a wedge before) |

KEEP: SMO entries through the pool (one reservation core orders both writers; lane-native at rung 3),
spin locks where they are (miss paths), `std.Io.Mutex` uses, `GuardPlan` as data, parent-guard class
from the record, `destroy_inodes` batching, `RETRY_BUDGET`/`READER_LOAD_RETRIES`, the FAILED lattice,
32 ledger slots; `ArcSwap.load`'s shared-line RMW gets a measured A/B at rung 4.

### WP-C — Foundation consolidation (foundation audit rows 3–8, 10, 12–15, 18, 23–25, 27)

| Row | Change | Why |
|---|---|---|
| 12, 13, 14, 15 | ONE ring-park primitive (`io/ring_park.zig`: eventfd wake + armed READ + EXT_ARG timed park + the coalescer), ONE futex wrapper, ONE `Oneshot` (`sync`), ONE bounded MPMC — used by `uring_fs`, `nvme_dev`, `journal_lane`, and rung 3's `sqz-fuse-ur` workers; `OwnedRing` either becomes that primitive or goes | §1.9; §9 — **landed 2026-10-05** (`ring_park.ParkedRing`, `sync/futex.zig`, `sync/completion.zig` — the lane-class cell; `sync.Oneshot` stays the `Io` task channel — `sync/mpmc.zig`; `OwnedRing` deleted; `uring_fs` pools its request cells; the journal lane lives on the primitive; `nvme_dev` — outside WP-C's files — embeds `ParkedRing` and the completion cell in a follow-up) |
| 23 | ONE errno table / ONE typed error set across io + meta; backpressure is `EAGAIN`-class, never `EINVAL` | §12 — **landed 2026-10-05** (`core/errors.zig`; `uring_fs` adopted; `nvme_derive`/`nvme_dev` and meta adopt by import in the WP-F pass) |
| 4–8 | Knob layer: readers return a diagnostic (no threadlocal `last_error`); readers enforce the row's range (no duplicated defaults); delete `sizing.zig`'s second lenient parser (it clamps `>100` — banned); reclassify the three `derived_override` rows that are A/B levers; `SQUEEZEFS_TEST_NVME_READ_TIMEOUT_MS` is not a seam (see Q5) | §1.6 — **landed 2026-10-05** (`Parsed(T)`/`Diagnostic`; `comptime key` readers with `intDefault`/`boolDefault`; `Class.tuning` deleted per Q7: `SQUEEZEFS_TIMEOUT` and `SQUEEZEFS_PARKED_GATE_ASSIST_MS` deleted, `CHECKPOINT_MAX_AGE_MS`/`META_FLUSH_INTERVAL_MS`/`NVME_*_LANES`/`DLM_STRIPES` → `ab_lever` naming the arm; the registry test `class == test_seam ⇔ SQUEEZEFS_TEST_ ∧ inert default` is green with Q5's deletion of the timeout row) |
| 3 | Delete the unused `m<x>` mount tag (ends the 13-char truncation) | §1.9 — **landed 2026-10-05** |
| 10 | `Facts.probe()` is the ONE source of cpus/page size (three modules bypass it); fleet share divides the CPU root too | §1.5 — **landed 2026-10-05** (`derive.facts()`, `Facts.withFleetShare`; `counters`, `uring_fs`, `budget` route through it; `nvme_dev`'s two bypasses — outside WP-C's files — follow) |
| 18 | cgroup `memory.max`/`memory.high` read along the ANCESTOR chain (a systemd slice cap today falls silently to 70 % RAM) | §1.8 — **landed 2026-10-05** (`budget.walkCgroupCage`, synthetic-tree tests) |
| 24 | `build_options` imported only by `main`, not the library | hygiene — **landed 2026-10-05** (+ Q8: `dist` = ReleaseSafe + full LTO) |
| 27 | Rails: the inverse census counts only product readers; the quote scan handles `'"'` | §11 — **landed 2026-10-05** (`std.zig.Tokenizer` scan; `test`-block depth tracking; `tests/` excluded from product scope) |

KEEP: Tripwire rate limit, the lane reactor shape, fixed buffers only where they pay, the watermark
barrier, the ring-flag probe ladder, the O_DIRECT posture law and grain ladder, cgroup v2-only, the
pure `tickInner`, `clamp`-floor-wins derivations with ties. Deferred with a rung: lazily allocated
stripe arenas (R5), the metadata device onto a fixed-file lane class (R4), acquire/release modelling
in the explorer (R3, before the payload-lease and doorbell models), seam split by `builtin.is_test`
(R3), a non-blocking logger (R5), the "no runtime literal" rail (owed, §11).

## B. Questions for the owner (each with the recommendation)

| Q | Question | Recommendation |
|---|---|---|
| Q1 | Take the naturally aligned headers with ONE reserved must-be-zero byte: 16 B record header `{key_len u16, kind u8, flags u8, val_len u32, seq u64}` and 24 B entry header `{pos u64, len u32, kind u8, reserved[3], xxh3 u64}` (PAD becomes `kind = 1`; the charter's "payload tag 0xFF" wording gets a one-clause amendment)? Cost: interior record 36 → 37 B (fanout −2.7 %), refs 46 → 47 B | **Yes** — a reserved byte is the only cheap insurance a forward-only format has, and it is free today |
| Q2 | `--single-writer` = a one-member forest (control tree + native slot tree, no appender directory), so ONE key layout exists? | **Yes** (WP-F row 1 depends on it) |
| Q3 | Delete the bit-8 partitioned-append machinery (ring/bitmap/ledger split by `writer_id`, `MAX_APPENDERS = 16`)? Rung 6's appender model (per-writer rings + extent grants) has no page-interleaved ownership | **Yes** |
| Q4 | `SQUEEZEFS_FLEET_SHARE` (co-located daemons dividing the budget): derive it from a `cgroup.procs` census of squeezefs daemons, with the knob kept only as a test seam? | **Derive** |
| Q5 | A stuck device READ: per-op `EIO` after 30 s (the reference) or the fail-stop lattice via the watchdog (§4 bans per-op timeout wrappers; §1.8 names the lattice)? | **Lattice**: the watchdog logs overdue ops loudly; a device that never answers fences the volume; delete the per-read timeout and its knob |
| Q6 | Thread class names: the charter §9 lists `fuse3-tpcN`, `f3-ur<qid>` — Rust-crate names. Keep them as the contract, or amend the charter to `sqz-fuse-lane<N>` / `sqz-fuse-ur<qid>` before rung 3 bakes them into dashboards? | **Amend** (one line in the canonical charter) — but it is your charter |
| Q7 | Knob class `.tuning` (a literal shipped default) is not one of the charter's three classes (delete / `SQUEEZEFS_TEST_*` seam / A/B lever). Reclassify every `.tuning` row as an A/B lever (logged when non-default) or delete it? | **Reclassify**; delete the ones no measurement needs |
| Q8 | `dist` profile optimize mode: `ReleaseFast` (UB on a true bug) or `ReleaseSafe` (a true bug aborts loud; the charter's §1.8 posture) with fat LTO? | **ReleaseSafe** for dist; measure the cost once at rung 9 |
| Q9 | The SETATTR-echo absorber is correct iff INIT advertises `FUSE_WRITEBACK_CACHE`; the charter's §4 INIT list does not mention writeback cache. Decide at rung 3 whether the echo arm exists at all | **Defer to rung 3** with the question recorded |

## C. Order

WP-R's bugs (rows 4, 6, 3, 9, 20, 21) first — they are independent of every ruling. Then WP-C (the
primitives rung 3 composes). Then WP-F as one branch, once Q1–Q3 are ruled. Every change lands with
its red test first and a row in 0002's ledger.

## D. Rung-3 design calls (2026-10-06, from `docs/porting/exec.md` §3.15 and the fuse specs' quirk ledgers)

| Call | Decision | Why |
|---|---|---|
| Q-A lane thread name | `sqz-fuse-ln<N>` (11 chars + up to 4 digits) replaces `sqz-fuse-lane<N>`; `sqz-fuse-ur<qid>` stays | the 15-char comm budget; arbitrary core counts (charter §1.4) |
| Q-B timers / fd-watch | Per-lane timer heaps and `POLL_ADD` on the lane's own ring for lane-resident tasks; ONE `sqz-timer` thread remains only for non-lane contexts (main, the blocking pool). The epoll fd-watch thread is not built | 0.38 timer arms per kernel READ each paying a global lock + a cross-thread hop; the hop measured 59–62 % of a journal write round trip; a lane that parks in its ring must be woken by its ring |
| Q-C core-0 reservation | Derived from `isolcpus`/`nohz_full`/the affinity mask, never "drop the lowest CPU by index"; the `core` pin scope is the A/B control arm | charter §1.4–§1.5 |
| Q-D `Io.Batch` on a lane | Fails loud in v1 (`error.Unsupported` at the vtable); implemented over the lane ring only when a measured caller needs it | charter §1.8: unsupported fails loud, never degrades |
| SQE128 | Zig 0.17's `std.os.linux.IoUring` indexes SQEs at a 64 B stride; the FUSE uring command needs the 80 B command area at offset 48. A first-party SQE128/CQE32 binding `src/io/uring128.zig` carries the `ParkedRing` park/wake/dead protocol for the transport workers; the sideband/INIT ring stays a plain `ParkedRing` | charter §12 "io_uring through Zig's own liburing-equivalent bindings (SQE128/CQE32 …)" |

Charter §9 amended accordingly (lane names; the timer/fd-watch bullet).

## E. Rung-6 design calls (2026-10-08, from `docs/porting/sym_*.md`)

| Call | Decision | Why |
|---|---|---|
| Membership rows | Member/client rows are `WriterClaimV1`-shaped records (`kind = 1`) in tree 0 of the home volume; the rendezvous rides the manager's claim; the appender page keeps the charter's `SLOT_PAGE_BUDGET = 108` — no `claim_set` fold into the page | charter §5.5 names 108 at the fixed part's natural widths; a page carrying membership would shrink it |
| Wire thread classes | `sqz-wire-acc` (the accept thread) and `sqz-wire-conn` (thread-per-connection) join the §9 naming contract (charter amended) | charter §9 thread-per-connection for wires; `daemon_cpu_ns_by_class` needs a class |
| Wire authentication | Rung 6: HMAC-SHA256 trailer over the `job:enroll` storage-trust secret with a server nonce + session key (the reference's ladder); mTLS on the cluster wire is rung 8 (Zig 0.17 std has a TLS client only; a server needs vendoring) | charter §8 names TCP/TLS for remote workers; the storage-trust ladder already binds identity to the volume |
| Cross-volume stopgap | Rung 5's `KvTx.cross` / `commitSplit` / EXDEV refusal are retired by the intents | charter §5.5 cross-owner intents |
| Grant sizing | The writer MEASURES its own block rate and names `want`; `G`'s EWMA term is live (the reference never fed it — every grant was the floor 64) | charter §5.5 `G = clamp(2 × ewma_blocks/s × T_renewal, 64, cap/(2 × writers))` |
| Un-share witness | A share intent registered before `MarkShared` step 1 and retired at `ShareBlock`'s ack gives `Held{1}` the witness the charter calls owed; `UnshareBlock` lands guarded on it | charter §5.5 "the un-share protocol needs a share-in-flight witness — owed" |

## F. Rung-7 design calls (2026-10-09, from `docs/porting/il_*.md`)

| Call | Decision | Why |
|---|---|---|
| Header `heartbeat` | IMPLEMENTED as the charter describes: the daemon's service pass stamps it (coarse ns) at least every tick; the shim/SDK read it for death detection with the 1 s escalation → socket probe → poison ladder. The reference never wrote it (the shim poisoned on a 30 s deadline) | charter §7 names it; a dead daemon must be detected by the client in O(1 s), not 30 s |
| Submit-side doorbell | `fetch_add(seq_cst)` + the SeqCst fence — the program the model checks IS the program that ships | the reference's loom model checked a stronger program than its code |
| Shim `-z nodelete` | Set at link time via an explicit `zig build-lib … -z nodelete` Run step in `build.zig` (0.17's `Compile` has no knob); verified by reading `DT_FLAGS_1` in a test | charter §12 |
| Shim syscalls | Raw `std.os.linux.*` only inside the shim (never `std.posix`, which would resolve to the shim's own exports); local declarations for `fstatfs`/`FUSE_SUPER_MAGIC`/`RTLD_NEXT`/`CMSG_*` | the interposition law |
| No-panic law | Interposer bodies total by construction + a `panic` handler that poisons every session and passthroughs the in-flight call before aborting the LIBRARY's own machinery (never the host process when avoidable) | charter §12 "no panics escape" |
| The SDK | Built from the charter and `design-sdk.md` (the reference shipped only the design): a second client kind on the same wire, positional-only ops on arena-backed 4 KiB-aligned buffers, the buffer-lease law, `{cur, cur−1}` ABI window, strengthen-only durability; a hand-written `squeezefs_sdk.h` pinned by a rails census + a C program compiled in the gate | charter §7, §12 |
| ONE client library | `src/ipc/client.zig` is the session client (HELLO, map, ring/slot/doorbell client, park, heartbeat watch, teardown) consumed by both the shim and the SDK; the shim adds interposition + the fd table + the offset mirror; the SDK adds buffer leases | charter §7 "the same wire as a second client kind" |
| Shared-process futex face | `sync/futex.zig` gains a public cross-process face (`waitShared/wakeShared` without `FUTEX_PRIVATE`) + `waitMany` over `futex_waitv` for the service thread's park on every session's doorbell | the shm words live in a MAP_SHARED memfd |
