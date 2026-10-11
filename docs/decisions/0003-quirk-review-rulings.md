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
per write (the Put stays for KILLPRIV's mode strip), one entry per op, concurrent appenders
coalesced by the conveyor's window.

**Laws:** `staging_moved_bytes ÷ appended bytes ≤ 11` on an append stream (tie test over the
geometric chain and the pinned 069 shape); `journal entries ÷ appends ≤ 1.05` (the rename gate).
