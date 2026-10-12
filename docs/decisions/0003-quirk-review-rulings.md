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

## G. Rung-8 design calls (2026-10-09, from `docs/porting/ops_*.md`)

| Call | Decision | Why |
|---|---|---|
| fsck shape | ONE tagged finding union (no string classes), the census over lessee shards only (the retired owner plane is not overlaid on the forest), the fleet shard wire = the job fabric's verb block (no second wire inside fsck), the epoch side map = the allocator's per-cell word | 0002; charter §8 zero-FP ladder |
| kvmap sweep | NOT a fabric job type: it stays the plane's cursor-resumed fiber (a write-path tail per charter §5.6) | the charter names it under the block-map tree, not maintenance |
| `job:` records | Tree-0 control records (tags 13/14, v4 `extern struct`s) — never JSON xattrs; the job verbs ride the ONE listener as block `0x06xx`; the enrollment is `wire/auth.zig`'s ladder | charter §8 "durable `job:` records"; 0002 |
| `enable-symmetric` | Stamps the capability bit + lays the appender region on a `--single-writer` set (already a one-member forest) | the port's format v4 |
| nvmet driving | configfs and `/dev/nvme-fabrics` written DIRECTLY (no `nvme-cli`/`modprobe`/`losetup` shell-outs; nvmet serves regular files without loop devices); the share ledger is a v4 `extern struct` file written by atomic rename through `uring_fs`; identity default derived from the machine id, `/etc/nvme/*` read-only; discovery by the `instance=` read-back + the namespace uuid via uevent, never a sleep-poll | charter §8; 0002 |
| zcrx | Cores (codecs + fuzz, parser, span ledger model, sizing ties, governor, steering over a mock, probe verdicts) + the real userspace association against the venue's nvmet-tcp over loopback with `RECV` into granted chunks; the ifq registration + NIC steering + live `RECV_ZC` = hardware-owed with a loud probe and an `Absent` posture here; the 24 B header scratch per the charter (not the reference's 128 B); NO per-op read timeout (0002 row 34 stands) | charter §6.3; §1.4 |
| `Ring128` CQ | gains an explicit `cq_entries` + `CLAMP` variant (the lane's derived CQ exceeds its SQ) | — |

## H. Rung-9 design calls (2026-10-10, from the bracket harness's findings, `docs/evidence/rung-09-brackets.md` §4)

| Call | Decision | Why |
|---|---|---|
| IL admission cap vs a bounded budget (F4) | The cap stays the charter's `budget/8` and the arena the charter's `max(64 MiB, align_down(cap / population_target, 4 MiB))` — but the 64 MiB floor YIELDS to a **population floor** of `2 × cpus` sessions (one client process per CPU at the shim's per-mount session floor of 2): where the cap cannot hold the floor at the charter's arena, the arena is the largest 4 MiB-aligned size at which `2 × cpus` sessions (arena + the layout's 148 KiB overhead each) fit, never below the 4 MiB DMA grain (`mem/sizing.resolveIpcArenaBytes`; the arm line prints the census and warns under the floor). An arena under 64 MiB carries FEWER slots of the shipped 64 KiB slab (`ipcSlotsForArena` = `min(1024, arena / 64 KiB)`), never smaller slabs: the slab is the ring's per-op ceiling for an async iocb and the lane gate's floor, so a bounded budget trades per-session depth (still ≥ 64 slots = 2 × fio's QD32 at the grain), not the per-op shape (the first cut shrank the slab and the interception smoke's 32 KiB no-wait tickets stopped fitting a slot). Rejected: raising the cap above `budget/8` (the R5 fraction is the invariant; 18 × 64 MiB would take 56 % of a 2 GiB budget on top of a 2.7 GiB unreclaimable floor) and per-admission arena negotiation (the client maps whatever geometry the header says, so it is possible, but a fat-first-thin-later fleet is asymmetric and untestable as a law). The client side: an open the refusal's backoff window kernel-serves is attributed `establish_refused` (it was `not_ours`); the daemon's `ipc_admission_refusals` is the `.stats` term and the harness's engagement detail prints it | charter §7 "arena `max(64 MiB, cap/population)`; admission cap `budget/8`" read with §1.5 (every default a derivation of the facts): the floor was a field-box constant (every box ran 64 MiB sessions) that on a 2 GiB venue budget admitted 3 sessions and kernel-served fio's second process (engagement 72 %, INVALID); 2 GiB ⇒ 12 MiB arenas, 21 sessions; 6144 MiB ⇒ 40 MiB, 19; the 176 GiB field shape is byte-identical (88 MiB, 255) |
| libaio's queue face (F3) | `io_queue_init/release/run/wait` are interposed with libaio's own bodies over the shim's `io_*` wrappers, never the real symbols; an unregistered context is a counted passthrough (per bound iocb on the binding's page → `ipc_client_aio_unknown_ctx_ops`; per call for getevents/cancel) | libaio binds its internal calls locally (`-Bsymbolic-functions`), so the real `io_queue_init` creates a context the shim never learns; the engagement law must never lose an op silently |
| The bracket ledgers are ONE registry (F9/F10, §6) | `bench.zig`'s term lists are brk.py's (device-write arms, request arms, admission arms, read-copy arms), pinned equal by a rail; every brk.py name is checked against the product's `.stats` keys by a source census (instrument declarations + the family writers' hand-written keys) | the harness's lists were checked against `/proc/diskstats`; a renamed counter is a red gate, not an `absent_keys` entry (the rail's first run caught `transport_wake_elided` → `transport_wakes_elided` and the client-only `lane_gate_threshold_bytes`) |

## I. Rung-9 sizing calls (2026-10-11, from the memory-footprint profile, `docs/evidence/rung-09-memory-footprint.md`)

| Call | Decision | Why |
|---|---|---|
| The gate binary's optimize mode | The product binary and the SDK libraries are **ReleaseSafe by default** (the module doc's promise, decision Q8's posture); the test artifacts stay Debug; `-Doptimize` / `--release` sets every artifact | `standardOptimizeOption(.{ .preferred_optimize_mode })` returns Debug unless `--release` is passed: every gate, bracket and suite since rung 1 ran a DEBUG daemon whose canary fills and `undefined` memsets touch every pool byte — 5.6 GB RSS after four fsstress rounds, 674 MB of fully resident 1 MiB stacks; the ReleaseSafe build costs 2m21s on the venue against Debug's 1m48s |
| What a pool's budget gauge counts | **Resident bytes the kernel may hold, never logical bytes**: a slab counts its idle-resident slots (the tiers count the checked-out ones), a stack pool its mapped-not-trimmed stacks, the bounce its populated windows, a cache its live cells | Σ gauges vs RSS is the unaccounted part the sampler cannot see; the reference's logical gauges undercounted every pool's retention (its `read_lru` 0 against a fully resident 1.6 GiB slab) and overcounted the virtual (752 MB of stack RESERVE counted as if resident) |
| The idle clock's grain | The sampler's tick count (1 Hz); Yellow trims entries idle ≥ 2 ticks, Red everything idle (0) | One tick cannot distinguish "freed a microsecond ago" from "free for a second": 2 is the least count that proves a full period idle; 0 under Red is the shed's promise that what it evicted returns in the same tick |
| The in-flight fill headroom | Stays uncounted (the one gap left in the fill pool's Σ): `max(cores × 16, 64)` slots × block | A fill in flight is neither a tier entry nor idle; counting it needs the pool to know which checked-out slots are tiers' — the reference's `read_lane_inflight` component is the same hole; bounded by the headroom (576 MiB at 4 MiB blocks on 9 CPUs), named in the evidence note |
| The parked-buffer component's weight | `parked_full_buffers` registers weight 0 (attribution + the idle trim of its cached cells) until the write path binds a drain-target shed | A sheddable weight with a no-op shed dilutes every other component's Red share for nothing; the drain plumbing (`drain_target.fetch_min` + the parked gate) lives in `write_admit.zig`, a concurrently owned file — the registration is here, the shed is theirs to bind (owed) |
| The node snapshot's base index (2026-10-12, `352f7f2`) | A `RecordIndex` charges `indexBytes(entries, sources) = entries × 24 B + sources × 16 B` to the owning cache's `kv_node_cache` gauge at `build`/`extendWith` and credits exactly at `deinit` — it joins `kv_node_cache` (same lifetime law as the memos and heads: deinit-exact through one gauge; eviction returns it with the extent), no sibling component; `meta_kv_node_index_bytes` names the live index bytes process-wide | Measured by the new R5 test (16 nodes × 400 records): 1,220,136 B live against a 1,048,576 B charge — 14 % under-reported, 10,722 B per node of which 9,616 B the index; charged, the residual is 1,106 B per node of fixed objects (the node, the snapshot's 8 memo cells, the Arc headers, the key bounds, a map and a clock slot). An index lives for as long as any snapshot over it (a reader's held snapshot keeps the superseded index charged), which is the footprint note's ≈ 2.6 GB of 64–324 KiB heap at the LTP peak; the cache's own budget now bounds the real bytes and its Red shed acts on them |
| The staging segment's fixed per-cell tables (2026-10-12, `1d91625`) | A budget component `staging_tables` (attribution-only, weight 0, floor 0, heap), gauge `= Segment.tablesBound(capacity) = maxCells × (Cell 80 B + u32 + Extent 40 B) + (maxCells + 1) × Extent + ceilPow2(maxCells / 0.8 + 1) × 33 B`, charged at `Segment.init`, credited at `deinit`, registered beside `staging_cell_state` at `openStaging`; the pending list is sized PRECISELY so every table is the formula | 31,574,220 B at the venue's 546 MiB ring and 3,768,524 B at 64 MiB — 5.5 % of the ring, none of it a component before; weight 0 because the tables live with the segment (nothing short of closing the ring returns them) and the never-lossy lever that empties cells is `staging_mmap`'s promotion kick. The std hash map's capacity law is restated and pinned by the tie test (a std change goes red here, not silent) |
| The fabric sampler's tables (2026-10-12, `8b0df62`) | Pooled for the process's life on the process allocator (beside the retained identities the reconnect law already kept there), sized when the population grows and reused after; the sysfs walk folds controllers straight off `getdents64` into a stack `Controller` — a `.stats` render borrows no allocation from the render | The `.stats` allocation law (ONE allocation per render) read 6 per render with one nvmet-loop controller connected (the names list, its dupe, the controller array, the presence and merge tables — allocated and freed every render); a fixed capacity would have needed a bound no fact gives (controllers per host are the operator's connects, not a derivation) and a refusal class for a cold-path instrument, where a pool that grows with the population and never shrinks costs ≈ 24 B per identity and nothing per render |

## J. Write amplification is a product defect (2026-10-11, the owner's ruling; `docs/evidence/rung-09-external-suites.md` generic/069 + generic/074)

Two measured defects — the W2 fold at **128×** (109 GB of device writes for 855 MB of user
bytes) and the staged arm re-staging a 1–2 MiB image per 4-byte append (≈ 3 ms, ≈ 2–4 MiB
moved per append) — and the bounds that replace the charter's `2048/k + 2` (written for ~2 KiB
extents and a whole-block fold; a COUNT cap with tiny extents is the wrong bound and a whole-block
rewrite the wrong grain). Symbols: `B` the block (4 MiB), `P` the device grain (4 KiB), `e` one
W2-class write's length, `a` the alignment its offsets obey (`a | P`; `a = 1` for arbitrary
offsets), `u` the user bytes a block received between two device landings.

### J.1 The W2 overlay — three landings, three bounds

The overlay is **page-indexed**: a touched page is a 4 KiB buffer that is content-valid at every
byte (zeros on a hole block; the base's page, read ONCE at first touch on a passthrough mapped
block; a transformed base — whose plaintext page costs a whole-frame decode — takes the seeded
full buffer, below). Coverage is therefore page-granular and exact (a 128 B page set), never a
65-run table that silently drops runs; "uncovered bytes never escape" holds by construction.

1. **A hole block lands exactly once.** Write-before-publish (a recycled block holds stale bytes;
   only a block every byte of which was written since its allocation may be published) makes the
   first landing a whole-block write whatever the fold grain; so a hole block is never folded
   mid-stream — at `pages ≥ B/4P` (a quarter block of page RAM) it escalates to a zero-completed
   full buffer and PARKS (a complete, content-valid block absorbs every later write at memcpy
   cost) until its flush (release, fsync, the pool cap's cold flush, the budget).
   `device = B`, `amp_hole = B / u_flush`; for a stream filling a fraction `f` of the block,
   `amp = 1/f` (generic/074's 512 B at a 1 KiB stride: `f = ½ ⇒ 2×`). A lone small write per block
   pays `B/e` — the physics of the law, stated, not a fold defect.
2. **A mapped sole-owner passthrough block folds IN PLACE at the page grain** (W1's state half:
   undecorated whole-block mapping, passthrough, `refcount == 1 ∧ !SHARED` through the fence pair,
   no overlay record, no epoch binding): one DMA per contiguous run of touched pages, zero
   allocation / free / map merge / journal. A write of `e` bytes at offsets `≡ 0 (mod a)` touches
   at most `pages(e, a) = 1 + ⌊(e + P − a − 1) / P⌋` pages (the worst start is `P − a` into a page),
   each page is written once per fold whatever it received, so
   `amp_inplace ≤ pages(e, a) × P / e`: **8× for 512 B at 512-aligned offsets, 16× for 512 B at
   arbitrary offsets, 2× for an unaligned 4 KiB write; 1× for the page-aligned shapes W1 already
   takes.** The seed is the same page set, read once at first touch (`fold_inplace_seed_bytes ≤
   fold_inplace_write_bytes`), never a whole-block image.
3. **A CoW block (transformed volume, a cloned/shared block, an epoch-bound block) lands whole,
   once per parked episode:** it escalates to a content-valid full buffer seeded from the base
   (ONE whole-block read) and parks; `device = B` (+ `B` read) per flush, `amp_cow = B / u_flush`,
   with `u_flush ≥ (B/4P) × e` at the escalation point for a sub-page `e` ⇒ `≤ 4P/e` at the
   worst, `→ 1` as the parked buffer absorbs the stream. The former whole-block fold every 64
   extents (`B / (64 × e)` = 128× at 512 B) is gone: nothing folds a block whole more than once
   per episode.

The trigger is the RAM bound and only the RAM bound: `pages ≥ B/4P` (the former 25 % coverage
edge measured in what the overlay actually holds — a touched page costs a page whatever the
write's length). `FOLD_MAX_EXTENTS` and `FOLD_MAX_BYTES` are retired (0002 rows); `fold_fill`
keeps counting the writes a fold absorbed.

**Laws (tie tests restate each; the `.stats` identity rides every write row):**
`fold_inplace_write_bytes ÷ admit_write_accumulate_bytes ≤ pages(e, a) × P / e` on the in-place
shape; `fold_inplace_seed_bytes ≤ fold_inplace_write_bytes`; a hole block's `device_write_bytes
= blocks × B` exactly; `parked_extent_bytes` charges `pages × P` (+ the page struct).

### J.2 The staged append — bytes moved per byte appended

A staged file's ring record reserves `reserve(V) = align_up(footprint(V) + slack(V), P)` with
`slack(V) = max(P, V/4)` when the write that stages it EXTENDS the image (an O_APPEND / sequential
small write at or past the end — the append distribution: generic/069's 4-byte integers, a log's
lines, `dd`'s 4 KiB; a whole-file `cp` extends nothing and reserves no slack). An extending write
that fits the slack lands **in place** (the appended bytes into the reserved tail, then the record's
header rewritten into the ALTERNATE header slot with the new `val_len`, generation and checksum —
the slot not current; recovery takes the valid slot with the higher generation, so a kill between
any two stores leaves the old record intact); a re-stage runs only when the slack is exhausted.
Bytes moved per appended byte: an in-place append moves `1`; a re-stage at image `V` moves `2V`
(one copy out of the ring, one in) and buys `slack(V) ≥ V/4` of appends before the next, so over
the geometric chain `V, 1.25V, …` the total is `1 + 2(1 + ¼)/¼ = 11` (and `≤ 1 + 2V/slack(V)`
for the last link). WHY a quarter: the ring's own headroom law (`highWater = max − max/4`) already
reserves a quarter for the drain's lag, so the append slack never exceeds what the pool has
promised to keep free; a larger slack (doubling → 5×) would strand half the ring under
stop-growing files until their promotion. The journal: one Δ`(size, mtime, ctime)` inode record
per EXTENDING write (the Put stays for KILLPRIV's mode strip), one entry per append, concurrent
appenders coalesced by the conveyor's window; a size-neutral striped write journals nothing of its
own — its stamp is a parked refinement (§L.6).

The chain is over the image's GROWTH: a write that lands `e` bytes `h` bytes past the end grows
the record by `e + h` (the hole is zero-filled in the ring), so the bound on moved bytes per USER
byte is `11 × (e + h) / e` — `11` for a dense append stream, `22` for generic/074's fstest `-F`
shape (512 B at a 1 KiB stride; measured 17× over its staged life).

**Laws:** `staging_moved_bytes ÷ appended bytes ≤ 11` on a dense append stream (tie test over the
geometric chain and the pinned 069 shape); `journal entries ÷ appends ≤ 1.05` (the rename gate).

## K. Read amplification: the bounds, their derivations, and the rulings (2026-10-11; `docs/evidence/rung-09-read-amplification.md`)

The owner's ruling: performance first — amplification is a product defect. Every read shape below
gets a DERIVED bound `A = device bytes read ÷ user bytes read`, a tie test, and a bracket row whose
`read_amplification_bound` verdict the harness asserts (`scripts/bracket/brk.py`; the ruler is the
exclusive nvmet namespace's `/proc/diskstats` read sectors — verified to count every direct read
issued to the namespace even when its backing file sits in the VM's page cache — else the daemon's
device-read arms). The ledger closes exactly: `data_device_read_bytes ≡ Σ` the plane's nine purpose
arms at its one read choke point (`plane.deviceRead`), asserted by `.stats`'s
`device_read_ledger_closed` and the harness's `device_read_ledger` verdict on every leg.

Notation: `len`/`off` the request; `g` the DMA grain = the device's O_DIRECT grain (`rsc.LBA_GRAIN`
4 KiB — every venue namespace reports `logical_block_size 4096`, and a 512 B direct read is EINVAL
there); `W(off, len) = ceil_g(off + len) − floor_g(off)` the ranged window; `B` the block (4 MiB);
`f = SQUEEZEFS_READ_ADMISSION_FILL_PCT / 100 = 0.05` the governor's share; `S` the bytes a stream
reads before it stops; `F` the transformed frame grain.

| # | Shape (row) | Bound | WHY (the derivation) |
|---|---|---|---|
| K1 | random sub-block read, passthrough, not stream-classified, every tier missed (`rand-4k-read`, `rand-4k-read-unaligned`, `rand-512b-read`, `rand-256k-read`) | `A ≤ W(off,len)/len × (1 + f)` — 1.0 aligned 4 KiB; 2.0 (mean 1.875) for 4 KiB at a 512 B grain; **8.0 at 512 B**; 1.0 at 256 KiB | The DMA cannot be smaller than the device grain, so one window per miss is the floor; a ghost second touch escalates to ONE whole-block fill whose waste the governor bounds to `f` of the foreground device bytes (eviction-side ratio); a fitting set converges once (escalations that pay back are not waste). **Ruling on 512 B: 8× stands on a 4 KiB-LBA device** — the registered reply window, the direct leg, the zc bridges and the kernel's FUSE pages are page-grained, so a 512 B-grain DMA would buy nothing on this venue and is EINVAL on its namespaces; a 512 B-LBA device's grain (OQ #1 of the reference design: a per-volume grain from `align_probe`'s measured `logical_block_size`) is the measured-evidence upgrade, taken when `ranged_read_unaligned_bounces` shows real 512-native waste on such a device, and would make the row 1.0 |
| K2 | random sub-block read ABOVE the reference's 256 KiB ceiling (`rand-1m-read`) | `A ≤ W(off,len)/len × (1 + f)` = 1.05 aligned | A random reader has proven NO contiguity and the ghost alone proves reuse, so the window law is the same at 1 MiB as at 4 KiB; the whole-block fetch (4×) was the 256 KiB read-LRU population bound (`READ_SIZE_CLASS_BOUNDARY_BYTES`) borrowed as a DMA policy. **Ruling: the ranged ceiling is DERIVED, `block − g`** (`read_path.derivedRangedThreshold`: a request longer than that windows to the whole block anyway, where the whole-block fetch is byte-identical and buys tier residency); `SQUEEZEFS_READ_RANGED_THRESHOLD` stays the explicit override (262144 = the reference's arm, 0 = the kill switch). Streams keep whole blocks (the `!streaming_fresh` veto stands: 1.0× + the pipeline) |
| K3 | the ghost escalation under a random storm over a set ≫ the hot tier (every K1/K2 row's `read_dma_block_demand_bytes`) | escalation bytes `≤ f × foreground` once the clamp engages; measured on the venue 13.7× (`rand-512b-read`: 656 whole blocks in 30 s) and 3.0× (`read-after-small-write`) | Two defects in the governor: (a) the clamp read the PREVIOUS epoch alone, so wherever the trickle bought fewer than two blocks per epoch (`CLAMP_FLOOR_BLOCKS`; 5 % of 48 MB/s = 1 block) the throttled epoch's single eviction failed the floor and the next epoch re-admitted everything the cooldown allowed — a tier's worth every two epochs; (b) a released clamp opened in one step. **Ruling: the verdict is re-decided at the ROLL from the finishing epoch's own eviction evidence (an epoch without evictions carries none: the verdict stands), engages at the floor ∧ ratio, and RELEASES by an additive-increase share ladder `f → 2f → … → 100 %` (one step per epoch whose victims were mostly served; any wasteful epoch resets to `f`)**; payback is credited in the DEVICE bytes a hit avoided (`W` of the served read, not its user bytes: a 512 B tier serve saved a 4 KiB DMA). `read_admission_governor_share_pct` is the gauge. **Addendum (the ReleaseSafe A/B)**: read at the roll alone, a storm's ONSET still escalated at device bandwidth for a whole 2 s epoch before the first verdict (1.37 GB of whole blocks in a 60 s `rand-512b-read`, 30 % of the foreground); the clamp now ENGAGES the moment the running epoch's own victims clear the floor ∧ ratio (`Governor.engage` from `onEviction`; tokens stay 0 until the roll mints them), the release stays the ladder at the roll — the onset costs the tier fill (96 blocks) plus the trickle: 512 MiB, 8.50× whole-run with the onset included, 8.24–8.33× in the harness's warm-up-excluded window |
| K4 | sequential read with an early stop (`seq-read-early-stop`: the first `S` bytes of many files) | `A ≤ 1 + min(W_cap, max(R2_WINDOW_START, S/B)) × B / S` — 2.0 at S = 8 MiB | Speculation never exceeds demonstrated consumption: a classified stream's prefetch window starts at 2 blocks and grows only on a foreground wait; the plan stops at the file's last block; a stream that stops after `S` leaves at most its window in flight. (Measured on the venue: see the evidence note — the row's verdict decides whether the window-growth gate needs a consumption term) |
| K5 | sequential full pass (`seq-read-1m-direct`, `seq-read-1m-buffered`, `shim-seq-read-1m`) | `A ≤ 1 × (1 + f)` | A classified stream fetches every whole block once; the plan stops at `total_blocks`; a time-based wrap claims a fresh lane after the 2 s idle law |
| K6 | read after small writes (`read-after-small-write`: 512 B random writes, then 4 KiB random reads) | `A ≤ W(off,len)/len × (1 + f)` on the read arms (2.1 at a 512 B grain), the fold's seed images (`write_side_read_dma_bytes`, the write-amplification agent's) beside | A read composing extent records / an overlay's gaps / a redirect's shadow over its base reads the base RANGED (`plane.fillGap`: one window per gap, `compose_read_dma_bytes`) — never the whole seed block |
| K7 | transformed volumes (`--compress lz4|zstd`, `--encrypt`): random 4 KiB reads | before: `A = chunk / len` = **1025× at 4 KiB** (measured 446.8× zstd / 446.5× AES-GCM / 446.7× lz4 on a 1 GiB set the 384 MiB hot tier a third holds); with the frame grain `F = 64 KiB`: `A ≤ (g + ceil_g(F + tag)) / len × (1 + f)` = **≤ 18× at 4 KiB on an incompressible frame** (`(4 + 68) / 4`), `(g + ceil_g(F/ρ + tag)) / len` on a frame of ratio ρ (9× at ρ = 2), ≤ 1.02× at 1 MiB | One AEAD/compression frame per block means every sub-block read decodes the whole image. **Ruling: the frame grain is the right fix and a FORMAT change we owe no compatibility for** (decision 0002): the block image is `header (96 B) ‖ frame table (16 B head, xxh3-guarded 16 B entries) ‖ frames`, each frame `F = min(64 KiB, block)` of plaintext compressed with the per-frame raw escape and sealed with its own nonce (the block's base counter + index) under AAD = header ‖ entry ‖ index; the table always fits the image's FIRST DMA grain, so a sub-block read DMAs that grain, learns its frames' window, DMAs the window (reusing the grain in hand when frame 0 is in it) and decodes those frames alone (never published, ghost-escalated like a ranged read). F = 64 KiB: lz4's match window is 64 KiB (no ratio cost), zstd −3 loses a few percent at 64 KiB vs 4 MiB, AES-GCM pays 28 B per frame (0.04 %). **The table is COMPACT, never a padded page**: frame 0's body begins at `tableLen(count)` (128 B for one frame, 1,136 B for 64) — a 4 KiB header page per image would have doubled every small packed tenant's slot (`pack_slot_len = ceil_4k(image)`: a 1 B file 4 KiB → 8 KiB) and the DMA that reads it; the headroom stays one grain (`chunk = block + 4 KiB`: table 1,136 + 64 tags ≤ 4,096). The data superblock's `features_incompat` bit 0 (`feature_framed_images`) names the format: `format` stamps it on transformed volumes, an older train refuses the bit loud at mount, this train refuses a transformed volume without it (reformat; pre-release). fsck's C-pass verifies every frame through the same decoder (table xxh3, body coverage, every frame's AEAD tag / decode). Landed: `transform.zig` (codec + `tableFuzz` + the span fuzz), `read_path.zig` arm 8f, `data_superblock.zig`; measured after: see the evidence note |
| K8 | metadata: `stat` storm (`stat-mdtest`), directory walk (`find-readdir`), lookups | cold ≤ tree height nodes per record (`node_reads ÷ records ≤ h`, h = 3 for the venue's trees); warm ≈ 0 device reads per record; readdir ≤ 1 node per stripe per page | A cold lookup walks root→leaf once per record and the node cache holds every node it loaded; a warm storm's working set IS the node cache; a readdir page is one leaf scan per stripe. `node_read_bytes ÷ (lookups + getattrs + dirents)` is the ledger (`META_READ`) |
| K9 | the shim rows (`shim-rand-4k-read`) | the kernel row's bound (K1), `ipc_bytes_out` beside as the user side | The IPC path's reads ride the same ladder (`WarmLadder.probe` then `read`); nothing on the ring changes the device side |
| K10 | transformed volumes: the sub-block read's THROUGHPUT (the K7 rows' headline IOPS: `rand-4k-read-zstd`, `rand-4k-read-lz4`, `rand-4k-read-aes`) | the decode of a frame never waits for another lane's decode: `transform_codec_pool_waits` = 0 by derivation (one codec per executor lane); the row's IOPS is judged against its passthrough twin on the same venue (the honest cost of the transform: the frame window's DMA and one 64 KiB decode per 4 KiB read) | The framed image (K7) brought the amplification in bound, but every lane's frame decode queued on the plane's ONE transform context behind a mutex, and that context decoded zstd through `std.compress.zstd` (601 µs per 64 KiB frame at ρ ≈ 4 in ReleaseSafe): 849 IOPS on the compressible zstd probe, 7.8 k on lz4 — a serialization, which the owner's ruling makes a product defect. **Ruling: the transform is split into its immutable half (`Transform`: modes, key state, raw lever) and a per-LANE mutable half (`Codec`: a static `ZSTD_CCtx` + a static `ZSTD_DCtx` of the vendored library — its one-context-per-thread contract; lz4 and the AEADs carry no state); a mount owns `codecPoolSize(lanes)` codecs in a lock-free free list under a semaphore (a transform never parks holding one, so the pool never runs dry; a stateless volume hands out one shared codec with no atomics) and `scratchPoolCap(cpus) = clamp(cpus/4, 4, 16)` stored-image scratches each held from the transform THROUGH the DMA that reads it** — the ONE shared write scratch the lock guarded was a data race (the next upload's transform overwrote the image the previous upload's DMA was still reading; red test in `accumulate.zig`); the promotion transforms once (the stored image picks the arm and is landed). The decoder's window slack is gone (`scratchLen` = block + 4 KiB). Measured A → B on the venue: the evidence note §4.3. Beside it, the same rounds' `write_shared_class_violation` residual was the plane's map merge publishing the LAYOUT's reach floor (`max(prev.size, min_size)` — the striped shrink's zero tail uploads with the block's end as `min_size`) as the inode's SHAPE; the shape is the filesystem's alone now (`noteShape` at every layout publish and size change), the merges publish none (red: smoke stage C's shrunk-then-extended file; fstests generic/013, 075, 112, 127, 263: 73 → 0 lines) |

Rulings on the LEDGER itself: `READ_COPY_ARMS` is now the exact user-byte partition (`read_copy_dest_bytes
+ read_dest_dma_bytes + read_zc_serve_bytes + transport_zc_direct_leg_serve_bytes`); the first
ledger summed the warm/hot/hold/cache/fill-slice SUBSETS of the dest copy and the transport's
bridge terms with it, which read 2.19× on the first row that served from a tier (its lists had been
pinned on direct-leg rows where every subset is 0). The ahead lane's fills are their own
`FillClass.lane` (ledger-invisible hold deposits per the design; `read_lane_fetch_bytes` counts
them — it had been declared and printed but never incremented). `fuse_readdir_entries` is the
dirent denominator. The transport's direct device leg counts its DMA (`transport_zc_direct_leg_dma_
bytes`, validated or withdrawn) apart from its serves.

## L. The journal's pad economy (2026-10-11, the owner's write-amplification ruling; `docs/evidence/rung-09-brackets.md` F14, `rung-09-external-suites.md` defect 26)

The measured defect: on fstests generic/074's shape (3 writers, 512 B fragmented writes, 4.61 M
journal entries) the metadata device took **1.51 GB of PAD for 2.36 GB of user bytes** — 77 % of
its write bytes pad, ≈ 430 B per op; the brackets' `rand-4k-write` on nvmet-loop landed **0.71 ×
the user bytes on the META namespace** (1.46 entries per 4 KiB window, F14). The pad is the
sector-pad law's price (charter §5.1: every reservation ends aligned behind a PAD entry) paid once
per conveyor window; the conveyor has no timer, so a window carries what arrived during the previous
pass's own latency. Symbols: `s` the entry (24 B header + Σ(1 + 16 + key + value); the 512 B-write
shape's Δ(size, mtime, ctime) inode record is **76 B**, a W1 patch's Δ(mtime, ctime) 68 B, a create
≈ 200 B), `g` the device grain (4096: the O_DIRECT page posture; `g_d = g − 24` data bytes per
page), `λ` the entries' arrival rate, `L_p` the pass latency (drain → handoff; measured 34 µs), `R`
the window round trip (handoff → terminal outcome; 180–300 µs on nvmet-loop under the 4 KiB rows,
70–95 µs under the QD1 rows), `n` the entries per window.

### L.1 The cost of a window

A window of `n` entries occupies `⌈(n·s + PAD_MIN)/g_d⌉` pages and lands as that many grains:
`device = pages × g`, of which `n·s` are entries, `24 × pages` page headers and the rest PAD
(`conveyor.windowCost`, tie-tested). For a sub-page window `device = g` whatever it carries:

    bytes per op = g / n            pad fraction = 1 − n·s / (pages × g)

A 76 B entry alone is 97.6 % pad; 27 of them stop being pad-majority (`pad > entries` — the hold's
predicate); 53 fill the page; 54 spill into a second, nearly whole pad. The exact-sum identity makes
every term measurable: `meta_kv_journal_device_bytes ≡ meta_kv_journal_bytes + _pad_bytes +
_header_bytes + _hole_bytes` (`journal.WriteAccounting.closes`, asserted at the ONE accounting
site; the harness restates it per leg), and the META device's bytes are the sum of its two write
arms, `journal_lane_write_bytes + meta_io_write_bytes ≡ the META namespace's diskstats` (measured
×1.0000 on every nvmet-loop leg).

### L.2 The window population, before: `n = λ·L_p`

With no timer the pass drains whatever the last pass's latency brought: `n = max(1, λ·L_p)`, so
`bytes/op = g / (λ·L_p)` until `λ·L_p ≥ g_d/s` — for the 76 B entry at `L_p = 34 µs` that is
λ ≥ 1.6 M entries/s, past the 727 k IOPS bar. Measured: `n̄ = 1.46` at 12.4 k IOPS (λ·L_p = 0.42:
every window is one page), `n̄ ≈ 12` on generic/074 (the page cache's writeback bursts, not the
rate). The journal could issue up to `1/L_p ≈ 29 k` windows per second — 120 MB/s of pad-majority
pages on a device whose own round trip is 7× the pass.

### L.3 What the bound SHOULD be — the pacing law, its gate and its budget

The pad is the price of a window; a window is worth issuing when the device would otherwise be
idle (pad on an idle device costs nothing the workload can feel) and not when it would queue behind
the journal's own previous windows. So:

**A pad-majority window waits until the flight is empty.** The pass that drained a pad-majority
batch (`pad(head + Σ) > Σ`) parks on the landing notify, per landing, until every window issued
before it reached its terminal outcome — bounded by the round trip's predictor from the hold's
start — then re-drains what arrived meanwhile into the SAME window and issues it
(`conveyor.holdForFill`). Consequences, each a law:

1. **Pad-majority windows under the law number at most one per device round trip**: `windows_held +
   windows_unshadowed − holds_not_worth ≤ span / R + deadline-ended holds + 1` (the harness verdict
   `meta_pad_fraction_bound`, `bench.Row.withinMetaPacingBound`). The journal's pad-majority device
   traffic is bounded by `g / R` bytes per second whatever `λ` — 14–22 MB/s on nvmet-loop — and its
   device utilization by one window per round trip: the journal can never saturate its device with
   pad.
2. **Each paced window carries `n ≥ λ·(R + L_p)` entries**, so `bytes/op ≤ windowCost(n, s)/n`
   (`conveyor.bytesPerOpBound`); an entry-majority window pipelines as before (its device write is
   majority payload — the batching has nothing left to recover) and costs under `2s` per entry.
3. **The latency budget: a commit's latency at most doubles, and a lone committer never waits.**
   A held committer waits for one round trip — the latency the device would have charged its own
   write; nothing in flight ⇒ nothing to shadow ⇒ no hold (QD1 shapes: `tar -x`, a single `fsync`
   job, one mdtest task pay nothing). The deadline that bounds the park is the round trip's own
   predictor, RFC 6298's `srtt + 4·rttvar` (gains 1/8 and 1/4, K = 4: TCP's ruling on where a
   round trip stops being a round trip and starts being a stall; no clamp below — a sub-µs device
   holds sub-µs), capped at the coalesced flush interval (a hold past the barrier cadence would carry
   a window across the barrier due to cover it). The fsync phase ledger (F16: `data_barrier` 824 µs
   + `meta_barrier` 517 µs on nvmet-loop; 152–194 µs + 132–176 µs on the quiet reboot) is the
   ruler: a durable op already pays two barriers; the hold spends at most one write round trip,
   under one barrier, and only when another committer's window is in flight — strict mode
   (`META_FLUSH_INTERVAL_MS=0`) holds nothing: a per-commit-barrier committer declared latency over
   bytes.
4. **The worth gate: a hold is paid only while it pays.** A hold saves one grain iff it collects at
   least one entry that would otherwise have been its own window, so the gate is the MEASURED gain
   — the smoothed count of entries the re-drain collected per hold (`srtt` of `holdWorthIt`, 1024
   fixed point) — not the arrival rate: in a closed loop the committer the hold delays arrives later,
   and the rate the hold needs is the rate it suppresses. Measured ungated on the venue, `fsync-4k`
   (4 QD1 jobs, 5 k commits/s against an 80 µs round trip) held 30 % of its windows for 48–55 µs,
   collected 0.09 per hold and lost 12 % of its fsyncs/s; `frag-512b-write` (4 QD1 jobs) collected
   0.35, wrote 8 % MORE META bytes (the hold broke the jobs' natural coincidence) at −11 % IOPS;
   `rand-4k-write` (QD32 × 4) collected 1.3 and took the META bytes from 0.70 to 0.46 × user. The
   gate starts OFF (never pay without evidence) and PROBES one eligible window in `HOLD_PROBE_PERIOD
   = 2/α = 16` (twice the estimator's memory: the gain it carries is at most two memories stale, at
   a cost of one hold in sixteen); a probe waits for the whole flight, not the first landing — in the
   unpaced regime several windows are in flight and the first landing comes in a fraction of a round
   trip (a first-landing probe measured 0.77–0.9 of the gain `rand-4k-write` pays 2.3 for, and kept
   the gate off).
5. **Liveness** is the landing's wake, never a tick: the pass registers on the notify BEFORE it
   re-reads the in-flight count, on every iteration (`sync/notify.zig`'s register-then-check law);
   conveyor model 8 (`HoldPark`, one and two windows in flight) proves the reversed order parks the
   pass on an empty flight (`hold_check_before_register` fails `InvariantViolated` in both), and
   `meta_conveyor_park_tick_rescues` stays 0. The hold is a parked timer on the pass fiber (the
   lane's own heap), never a sleep.

### L.4 The bound at each shape (nvmet-loop, `g = 4096`; the A/B rows in `rung-09-brackets.md` F14)

| shape | `s` | `λ` | `R` | `λ·R` | the law | before | after |
|---|---|---|---|---|---|---|---|
| `rand-4k-write` (W1 patch, QD32 × 4) | 76 | 9.5–11 k/s | 180–300 µs | 1.9–3 | gate ON: `n ≥ 2.3` ⇒ ≤ 1,760 B/op | 0.70–0.71 × user (n̄ 1.46) | **0.475 × user** (n̄ 2.24, 1,824 B/op), IOPS 11,982/11,989 → 12,088/11,943 (A/B 0.997, spread 1.2 %), p50 8,290 → 8,290 µs, p99 25.0 → 24.8 ms; the gate open 88 % of the windows (gain 0.7–0.9 per hold at the band) |
| `frag-512b-write` (generic/074's 512 B at a 1 KiB stride, 4 QD1 jobs) | 76 | 8.4 k/s | 76–95 µs | 0.65–0.8 | gate OFF (0.35 collected per probe): the pacing law cannot reach it — the entry was not owed (§L.6) | 5.4–5.8 × user (2,800 B/op; n̄ 1.46) | 5.35–5.40 × user (gate off: 5 % of the windows probed, gain 0–0.25), IOPS 9,026/8,735 → 8,726/8,944 (A/B 1.005), p50 247–253 µs both; **§L.6: 0.019 × user at +54 % IOPS** |
| `creates-mdtest` (4 tasks, QD1 each) | ≈ 200 | 9–13 k/s | 66–110 µs | 0.8–1.2 | borderline: the gate decides on the measured gain | ≈ 5.1 KB/create (half of it node images and appends) | 4,753/4,748 B/create (gate mostly off: 12 % held in bursts, gain 0.1–0.4; 1.7 entries/window), creates/s 12,283/9,389 → 12,754/12,235 (A/B 0.867 inside the row's 29 % spread) |
| `fsync-4k` (4 jobs, QD1 each, fsync per write) | 76 + layout | 5–6 k/s | 80 µs | 0.4 | gate OFF (0.09): no hold | 0.96 × user, fsync p50 281–322 µs | 0.960 → 0.960 × user (gate off: 2 % probed, gain 0–0.13), fsyncs/s 6,259/6,089 → 6,285/6,319 (A/B 0.980), fsync mean 339/348 → 337/335 µs, p99 618/651 → 610/602 µs; `data_barrier` 147/150 → 145/144, `meta_barrier` 124/128 → 124/124 |
| `tar -x` (one thread) | ≈ 200 | 1–1.2 k entries/s | — | — | nothing in flight when the next arrives: no hold | 5.21 entries/tar entry | unchanged by construction |
| the 727 k IOPS bar (box) | 76 | 727 k/s | ≈ 100 µs | 73 | entry-majority: pipelined, `2s` = 152 B/op | — | the law never engages: `λ·L_p` already fills pages |

~~The floor the law cannot cross: a 76 B entry is 1.9 % of a 4 KiB page, and the 512 B shape also
lands ≈ 100 B/op of bset appends, so that shape's metadata cost is ≥ ≈ 180 B/op (0.35 × user) at any
pacing.~~ **Superseded by §L.6 (2026-10-12)**: 180 B/op was the entry-and-bset floor WITH one entry
per write, not what the shape paid — it paid 2,738 B/op, 92.6 % of it pad, because a closed loop
of `c` committers caps the window at `n ≤ c` entries and the measured `n̄` was 1.56; and the entry
itself was not owed: a size-neutral write's record is two timestamps. The grain lever stands as a
substrate fact (a 512 B-LBA metadata namespace prices a lone entry at 512 B, not 4,096; nvmet's file
backend defaults to `blksize_shift` 12), but it is no longer the remaining lever.

### L.6 The write-stamp park: a size-neutral write journals nothing (2026-10-12; `rung-09-brackets.md` F14's second resolution, `rung-09-external-suites.md` defect 27)

**The decomposition** (`frag-512b-write`, 4 psync QD1 jobs — NOT QD32 — of 512 B buffered random
writes over pre-sized files, nvmet-loop, the recorded B legs of `frag-512b-write-20261012T001908Z`;
the A legs agree within 2 %): 533,297 writes = 533,349 FUSE ops = 533,297 payload leases — every
write(2) is its own 512 B `FUSE_WRITE` (the mount is write-through; nothing merges). Journal entries
533,522 = **1.0004 per write**, 76.0 B each (the Δ(size, mtime, ctime) of §J.2); windows 342,143 →
**n̄ = 1.56 entries per window**, every window ONE 4 KiB page (342,143 page headers). META device
bytes per write: `journal_lane_write_bytes` **2,628 B/op** = entries 76.0 + page headers 15.4 +
**pad 2,536** (96.5 % of the journal's bytes); `meta_io_write_bytes` **110 B/op** = node appends
100.9 (the leaf's bset flushes: ≈ one Δ record per write, 4 KiB-padded per checkpoint; the leaf fills
and rewrites every ≈ 0.3 s — `node_rewrite` 5.0) + ledger 3.8; staging ring / W2 spill **0** (staging
is off on the set; a spill targets the staging ring, never the metadata device). Total **2,738 B/op =
5.35 × 512** — the arithmetic closes on the measured 5.347. The committers: 4; a commit's journal
phase ≈ pass 27 µs + round trip 92 µs of a 451 µs mean op (p50 253 µs), so ≈ 1.06 committers sit
in the journal at any instant, the pass drains `λ·L_p = 8.7 k/s × 27 µs = 0.24` beyond the first,
and cohort bunching lifts it to 1.56; 55 % of the windows found NOTHING in flight (`unshadowed −
not_worth` = 189 k of 342 k) and the hold's probes collected 0–0.25: the free committers were
`c − n_flight − n_held ≈ 0.9`, each back inside a 92 µs hold with probability `R / T_nj ≈ 92/330`.

**The bound at a closed loop.** With one entry per write and `c` committers each waiting for its
landing (charter §5.2: acks after the landing), a window carries `1 ≤ n ≤ c` entries whatever the
pacing — perfect coalescing is `c` per window and costs every committer the slowest one's cycle —
so on a `g`-grained ring

    journal bytes/op = g / n̄ + 24/n̄ (headers)    with    1 ≤ n̄ ≤ c    ⇒    g/c ≤ bytes/op ≤ g

4 KiB / 4 = 1,024 B/op at the unreachable best, 2,628 measured at n̄ 1.56, 4,096 at a lone
committer: `bytes/op = f(g, s, c)` is dominated by `g/n̄`, the entry `s` enters only as `n̄·s` of
the page (119 B of 4,096) and through the bset's ≈ 101 B/op, and NO pacing of a one-entry-per-write
journal gets this shape under 2 × the user bytes. The component above its derived bound is therefore **the entry count**: a size-neutral
write — the W1 patch, the W2 overwrite, generic/074's whole shape, every `rand*write` row over a
pre-sized file — changes only `mtime`/`ctime`, and the reference's law for exactly that record is
ZERO entries: `park_write_times` ("the ONE durable authority for the write's mtime/ctime — the same
stamp the FUSE handler publishes to the attr cache. RAM-parked (zero hot-path journal entries),
fold-visible on every read, journaled by the batched drain / the fsync/unmount durability points";
the size claim "publishes ONLY when the claim was genuine growth"). The port had the map
(`economy.PendingTimes`, the SETATTR-echo absorber) and the drain's helpers but no drain task, and
its write postlude committed the Δ per write — §J.2's "one entry per op" was derived for APPENDS
(growth) and applied to every write. A port gap, not a reference quirk.

**The ruling** (`kv_fs.writeStriped`, `parkWriteTimes`, `drainPendingTimes`; `cli/mount.zig`
`TimesDrainCadence`):

1. **A size-neutral striped write parks its stamp** under the ino's exclusive guard (`PendingTimes.
   park`, the signed monotone max — the same law `fold` serves it by on every inode read); the tx
   stays empty, nothing commits, the write's latency loses the journal round trip. An EXTENDING write
   commits its Δ(size, mtime, ctime) per op as before (§J.2: size never leads data; the kill matrix's
   "whole or empty" rests on it); a KILLPRIV write commits its Put. Under the coalesced cadence only —
   **strict mode (`META_FLUSH_INTERVAL_MS=0`) commits per write**: that mount declared latency over
   bytes and its ack means durable, and parking there would make it mean "within a drain".
2. **The drain** journals every parked refinement (the write stamps, the absorbed echoes) in batched
   Δ commits — `PENDING_TIMES_DRAIN_BATCH` (128) snapshotted per pass, committed in sub-batches of
   `dlm.max_requests` (16) inos under ONE ordinary canonical plan each (a drain never holds more
   inodes exclusive across a commit than any op's own guard set may; the reference's 128-wide set is
   not owed). Triggers: the daemon's cadence (a meta fiber parked on `times_drain_wake` with the
   flush interval as its deadline — register-then-check, never a sleep), the cap crossing (512
   parked wakes it early), `fsync(ino)` (that ino alone, ONE Δ ahead of the barriers — the exact
   POSIX scope; the reference drains the whole volume), `fsyncdir` and the dismount (`destroy`,
   every teardown path) ahead of their barriers. A drain member whose stamp no longer advances the
   stored record (a committed write superseded it; a destroyed corpse) stages nothing and retires.
3. **The retire is conditional** (`PendingTimes.retireIfStill`): the drain's guards drop at the
   commit's terminal outcome before the retire runs, so a write that parked a NEWER stamp in between
   keeps it for the next drain — an unconditional retire would lose a stream's last stamp.
4. **A moved slot's refinements are dropped, never left to poison the door**: a batch refused
   `SlotMoved` (the slot leased to another appender since the park) retires its members and counts
   `meta_kv_times_echo_foreign_dropped` (the reference's defect 31); any other failure keeps them
   parked for the next trigger.
5. **Durability.** A parked stamp is journaled within one flush interval of its write (50 ms
   shipped) — the same bound the coalesced barrier already gives an ACKED entry against a power cut,
   and tighter than ext4's 5 s commit interval for an inode dirtied by a buffered write; a daemon kill
   inside the interval leaves the file with its new bytes (the W1 patch landed; a W2 park's bytes
   were volatile anyway) and the previous mtime — ext4's own post-crash shape. `fsync` closes it.
   A write's stamp never regresses an mtime set ahead of it by `utimes` (the fold's law; the
   reference's too).

**Laws** (the §L.6 test in `kv_fs.zig` restates each over a RAM store): a size-neutral striped
write commits 0 entries and parks 1 stamp (`fuse_write_times_parked`); the drain commits ⌈parked /
16⌉ entries and the stored record then carries exactly the served stamp; `fsync(ino)` costs ≤ 1
entry; an extending write costs 1 (§J.2's `≤ 1.05` for appends stands); the cap wakes the cadence;
strict mode commits per write. The bound this buys the 512 B shape: journal entries per op ≤
`⌈inos / 16⌉ / (interval × IOPS)` = 4 inos at 50 ms and 8.7 k IOPS → **0.0023 entries/op**, ≈ 9 B/op
of journal + the bset's Δ once per inode per checkpoint; the data device's §J.1 bound is untouched.
On an fsync-per-write shape the bound is one Δ per file per tick of the COARSE stamp clock (a stamp
inside the tick of the last drained one advances nothing and the drain stages nothing): 4 files at
the venue's 4 ms tick → ≤ 1,000 entries/s whatever the fsync rate.

**Measured** (nvmet-loop, 4 × 60 s legs A B B A, A = `585152f`, B = the park on the same tree;
`rung-09-brackets.md` F14, second resolution): `frag-512b-write` META ÷ user **5.885 / 5.661 → 0.019 /
0.017**, journal 2,903 / 2,789 → 7.3 / 6.4 B/op (0.0018 entries per write: one drain per 50 ms carrying
3.9 Δs), IOPS 7,793 / 7,766 → **11,223 / 12,720** (A/B 0.650), p50 276 / 268 → **116 / 111 µs**, p99
2.5 / 2.9 → 0.88 / 0.39 ms, the data device's 7.0× (§J.1) unchanged; `rand-4k-write` META ÷ user
**0.586 / 0.598 → 0.0008**, IOPS 11,011 / 12,379 → **24,432 / 24,534** (A/B 0.478, B spread 0.4 %),
p50 8.7 / 8.0 → 2.6 / 2.4 ms (p99 25.8 → 59–61 ms: the data namespace's own queue at twice the
throughput); `fsync-4k` META ÷ user **0.959 → 0.128**, 0.13 entries per op (the tick law above),
fsyncs/s 6,083 / 6,228 → **7,491 / 7,663** (A/B 0.812), write p50 276 → 151 µs, fsync mean 279 →
252 µs; `creates-mdtest` unchanged by construction (0 parks; A/B 1.068 inside A's 20.5 % spread).
Every leg VALID, 0 tripwires, 0 tick rescues, every ledger closing to ×1.0000.

### L.5 Rejected

- **Tail-page rewrite** (the next window rewrites the previous window's last partial page with the
  same prefix plus new entries): saves ring CAPACITY (the pad no longer occupies the ring), not
  device bytes — every window is still one grain-aligned device write, and the measured defect is
  device bytes per op. It would also make a landed page mutable (a torn rewrite loses an ACKED
  entry's bytes unless the rewrite is atomic at the grain — the one honest RMW the ring allows today
  is the recovery pad at open). Not built.
- **Entry economy** (shrinking the 76 B Δ): the pad is `g − n·s`; at `n = 1.46` shrinking `s` by
  20 B saves 29 B of a 4 KiB write. Irrelevant to this defect (relevant at the box's rates, where
  `2s` per op is the bound).
- **A fixed or rate-derived hold longer than one round trip** (`H = (g − Σ)/(s·λ̂)` would be 2–5 ms
  at the venue's rates): it buys `n ∝ H` but costs every committer `H` — a QD1 loop's throughput
  falls by `H/R_op` (mdtest −25 % at 1 ms) and the owner's ruling is performance first. The round
  trip is the one budget the committer already pays; a longer hold is a measurement lever for the box
  (not built: the knob would be a free-floating constant).
- **An arrival-rate gate** (`mean inter-arrival ≤ srtt`, one companion expected): built, measured,
  replaced — it flip-flopped on the borderline shapes (`frag-512b-write` λ·R ≈ 0.9: half the windows
  held, 1.35 collected, +8 % META bytes, −11 % IOPS) because a closed loop's rate is not independent
  of the hold.

## M. The hash: sqzhash128 replaces xxh3-64 everywhere (2026-10-11, the owner's ruling; `docs/evidence/rung-09-sqzhash.md`)

The owner's decision: adopt **sqzhash128** — the owner's own keyed AES-round 128-bit hash
(`~/Source/zig_hash_bench/src/sqzhash.zig`, ported bit-exact into `src/core/sqzhash.zig`, which
is now the format's pinned definition: any output change is a format change) — as the ONE hash of
the on-disk format and the wire, replacing xxh3-64 in every checksum, identity and name hash.
Two reasons, both measured: it is faster on every format shape (§M.3; the owner re-confirmed the
measurement the same day), and it is SMHasher3-clean in full — all 252 tests with `--extra`, both
as 128 bits and on the low 64 bits alone (verification values `0xAB49D6E0` / `0xD6703A98`) —
where XXH3-128 passes 214 of 250 in SMHasher3's published results. The charter names the
mechanisms (checksums everywhere, verified once at load; a keyed name hash so names cannot be
chosen to collide), not the algorithm; decision 0002 owes no compatibility, so the swap is one
format act (row 480).

### M.1 The projection: 8-byte fields carry the LOW 64 bits

sqzhash128's state is 128 bits wide whatever the output, so the low 64 bits cost the same as the
whole (one register move less) and are the half SMHasher3 verified on its own. Every 8-byte format
field — the superblock, ledger slot, journal page header and entry, node header, bset, bitmap page,
appender page, intent, claim, record frames, the wire frame — is `@truncate(u64, h128)`
(`checksum.low64`). The facade (`src/core/checksum.zig`) is algorithm-neutral: `hash64`, `hash128`,
`hash64Keyed`, `hash64FieldZeroed`, `stamp`/`verify`, the streaming `Hasher` (`final` u128 /
`finalLow64`); the algorithm's name lives in `sqzhash.zig` and in the format version. Wire and page
fields read `csum_le` / `CSUM_OFF` — the algorithm's name leaves the format's field names.

### M.2 Two keys, never more

- **The FORMAT key** (`checksum.FORMAT_KEY = 0x5351_5A46_5320_464F_524D_4154_204B_4559`, the
  ASCII of `SQZFS FORMAT KEY`): fixed and PUBLIC on purpose. A checksum detects corruption and keeps
  no secret, and sector 0 must verify before any per-volume value exists. Expanded at comptime
  (`checksum.format_key`; the engine's software AES round runs under `@inComptime()`, proven equal
  to both hardware paths), so no mount pays the 40-round expansion and no code path can forget it.
- **The NAME key** (`key.NameKey`): the superblock's random `hash_seed` widened to the hash's
  128-bit secret by ZERO-EXTENSION (`checksum.keyFromSeed`; the rule is the format's — the name
  hashes on disk depend on it; `FORMAT_KEY`'s upper half is non-zero so no seed can expand to it),
  expanded ONCE at open/mount/format and handed to every `hash54`/`hash56` by pointer through every
  store (`KvBackend`, `MetaSet`, the fsck and test stores), the image builder, the data plane, the
  fsck host and the stripe map. Never a key expansion per name: a `Key.init` is 40 AES rounds, and
  the per-call seeded form the reference used paid the equivalent on every lookup.

### M.3 The measurement (`squeezefs bench --hash`; ReleaseSafe, the gate profile)

Two modes per (algorithm, shape): THROUGHPUT (independent hashes the core overlaps — a stream of
page checksums) and CHAINED (the previous result written into the input before the next hash — a
probe chain's dependent latency). The venue (`7.2.6-sqz`, 9 vCPU Apple-VM aarch64, two other
agents' gates sharing it) and the dev box (Apple M3 Pro, macOS 27) agree within a few percent:

| shape | xxh3-64 (std) tput | sqzhash64 tput | ratio | xxh3-64 chained | sqzhash64 chained |
|---|---|---|---|---|---|
| 16 B name | 2.17 ns (7.4 GB/s) | **1.35 ns** (11.8 GB/s) | 1.6× | **5.15 ns** | 5.9 ns |
| 100 B record | 8.55 ns (11.7 GB/s) | **3.11 ns** (32.1 GB/s) | 2.7× | 8.69 ns | **6.46 ns** |
| 4 KiB page | 3.67 µs (1.1 GB/s) | **29.4 ns** (139 GB/s) | 125× | 3.67 µs | **29.5 ns** |
| 256 KiB node | 207 µs (1.3 GB/s) | **2.60 µs** (101 GB/s) | 80× | 208 µs | **2.62 µs** |

WHY the long shapes are two orders apart: `std.hash.XxHash3`'s stripe is a `@Vector(8, u64)`
`(lo32 × hi32)` multiply, which AArch64 NEON has no instruction for — LLVM scalarizes it into eight
`MUL`s with extracts and inserts, and the accumulate loop runs at ~1 GB/s on this core (the owner's
hand-written xxh3 in `zig_hash_bench` reaches 46–60 GiB/s with `UMLAL` splits, which the std
implementation does not do). The product paid the std rate on every node header, bset and page.
The one shape xxh3 keeps is the 16 B DEPENDENT chain (5.15 vs 5.9 ns: one multiply-fold against
four dependent AES rounds), which the owner's own table also records; the throughput at 16 B and
everything from 17 B up is sqzhash's. The `sqzhash128` column (the full 128-bit result, both halves
folded) tracks `sqzhash64` within noise on macOS and reads 2× slower at 100 B throughput on the
venue's Linux build (6.9 vs 3.1 ns, reproducible across three runs) — the u128 return leaves the
vector register there; the format pays the 64-bit projection, which is the judged baseline column.

### M.4 The format act

Metadata format **v5**: v4's layout with every checksum re-defined. `classifySector0` names a
`SQZMETLV` sector with version `< 5` LEGACY before any checksum check — a v4 image verifies nowhere
under this binary, and the mount's refusal names the lever ("a pre-v5 squeezefs format (version 4)
… reformat required: run `squeezefs format`"), never a tear. The data superblock moves to v2
(version before checksum, the same lever), the framed block image to `FRAME_VERSION 3`, the staging
segment record to v3 and its marker to v2 (a v2/v1 record or marker is torn to the scan: the ring
is a cache, wiped loud), the NVMe read-cache frame to v2 (a stale frame refills). The FIND-VS-A
fixture is regenerated as `findvsa_pred_bset.v5.bin` (same 254,932 bytes, 3,515 records; every
checksum and every `hash54` differs). The kill matrix and fsck's C-passes verify every checksum
under the new definition (both gates green on both venues, `rung-09-sqzhash.md`).

### M.5 What was NOT changed

- The on-disk layouts: every offset, width and `@offsetOf` assertion holds; only the function
  behind the 8 bytes changed.
- `std.hash.Wyhash` for the model checker's state fingerprints and the fuzz corpus's directory
  names (RAM-only cache paths, never format); `std.hash.XxHash3` survives only as the bench's
  reference column.
- The staged cell's value-first streaming state (`staging.ValueHash = checksum.Hasher`): the law
  (O(appended) per in-place append) holds. The state had grown from ~600 B to 4,736 B per LIVE cell
  (the engine's 16-chunk buffer plus a key copy — up to 116 % of the ring's own bytes, ungauged);
  RESOLVED 2026-10-12 (`rung-09-sqzhash.md` §3.1): the state is the construction's minimum, 1,056 B
  (the key by pointer; the input itself while ≤ 1 KiB, else 16 lanes + the last chunk), pinned, the
  output unchanged, and gauged as the R5 component `staging_cell_state`.

## N. Rung-10 design calls (2026-10-12, the symmetric plane's owed items; `docs/evidence/rung-10-symmetric-completion.md`)

| Call | Decision | Why |
|---|---|---|
| `enable-symmetric`'s order | The claim (bitmap + restated ledger) lands BEFORE the stamp; the stamp is the last write. A crash between them leaves a flat volume with one claimed extent no root reaches, which the re-run finds by CONTENT (`findStrandedLink`) and resumes onto; the alternative — stamp first, claim at the first symmetric mount — would make a mount a repair site and leave a window in which the directory the superblock names is free in the bitmap | charter §5.3 "any ledger slot a mount may select references only never-overwritten extents"; §5.5 "every verb is idempotent against DURABLE state" |
| `enable-symmetric`'s window law | A non-empty replay window REFUSES (typed `WindowNotEmpty`, the remedy named: mount and unmount cleanly); the verb never replays. The ring is zeroed end to end rather than re-based byte for byte: positions are logical and the symmetric geometry maps them to other pages, so kept bytes would be phantom chain links | charter §5.9's recovery belongs to the mount path; §5.5 "every carved ring extent is ZEROED before a page names it" |
