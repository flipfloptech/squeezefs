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
| 1 | ONE key layout — the charter's, with the kind byte. Delete the kindless "legacy" layout, `KeyFraming`, `splitForestKey`, the builder re-key, the `extra = 1 if forest` size term. `--single-writer` becomes a one-member forest (control tree + native slot tree; no appender directory) | §5.1 Keys list the kind byte; §5.5 "one mixed-kind KV tree per routing slot" |
| 3–4 | Fixed-width `extern struct` inode value with explicit `rdev` and `flags2`; drop the LEB128 version tag (verified non-canonical: `0x81 0x00` decodes) | §5.1 "packed inode {…} ≈ 96 B"; §12 canonical total codecs |
| 8 | One magic convention (ASCII tags) | §12 |
| 9–10 | ONE stamped bset frame (drop v1); check `reserved` on read | §5.5 frame screen "v2"; §12 total decode |
| 11 | Page header: `appender_id u32` (not `writer_id u16` from `MAX_APPENDERS = 16`); pads validated | §5.5 capacity `heap/16 ÷ ring` |
| 16–18, 36–37 | Ledger record: ONE fixed layout with an explicit version; drop decode-by-length, the frozen `routing_width`, the bit-8 suffix, `tree_id = 0` as a root identity, `stride == 0` singletons | §12; §5.4 W is derived |
| 19, 21, 22 | Superblock re-laid: explicit-endian fields, checksum LAST, `volume_len` so the DUR-5 backup is self-located, `hash_seed` an independent RANDOM seed (today `xxh3(uuid)`) | §5.1 "random `hash_seed`"; §12 |
| 20 | Feature word: `version = 4` absorbs every always-on bit; four real capability bits remain (`SYMMETRIC_PLANE`, `LAYOUT_DELTAS`, `BLOCK_MAP_TREE`, `VOLUME_LIFECYCLE`); DELETE the bit-8 partitioned-append machinery (~135 refs, the pre-forest multi-writer model) | §1.7 "Capability bits mean one complete thing each"; §1.9 no dead code |
| 26 | Validator enforces `heap_start % node_size == 0` | §5.1 |
| 32 | Stop writing the `format_config` JSON xattr on ino 1 | 0002 row 8 |
| 29, 31 | Comment fixes (16 PiB; `MAX_ENTRY_LEN` WHY) | §1.5 |

Net size ≈ −600 lines. KEEP (charter-mandated or good): kind-byte placement asymmetry (refs by
block = the refcount range probe), `seq ≡ position`, PAD-by-payload, `0xFFFF`, the node header,
`KVAB` 32,576 bits (derived), the ledger/journal offsets, `K = 38`, u32 block index, u16 slot.

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
| 12, 13, 14, 15 | ONE ring-park primitive (`io/ring_park.zig`: eventfd wake + armed READ + EXT_ARG timed park + the coalescer), ONE futex wrapper, ONE `Oneshot` (`sync`), ONE bounded MPMC — used by `uring_fs`, `nvme_dev`, `journal_lane`, and rung 3's `f3-ur` workers; `OwnedRing` either becomes that primitive or goes | §1.9; §9 |
| 23 | ONE errno table / ONE typed error set across io + meta; backpressure is `EAGAIN`-class, never `EINVAL` | §12 |
| 4–8 | Knob layer: readers return a diagnostic (no threadlocal `last_error`); readers enforce the row's range (no duplicated defaults); delete `sizing.zig`'s second lenient parser (it clamps `>100` — banned); reclassify the three `derived_override` rows that are A/B levers; `SQUEEZEFS_TEST_NVME_READ_TIMEOUT_MS` is not a seam (see Q5) | §1.6 |
| 3 | Delete the unused `m<x>` mount tag (ends the 13-char truncation) | §1.9 |
| 10 | `Facts.probe()` is the ONE source of cpus/page size (three modules bypass it); fleet share divides the CPU root too | §1.5 |
| 18 | cgroup `memory.max`/`memory.high` read along the ANCESTOR chain (a systemd slice cap today falls silently to 70 % RAM) | §1.8 |
| 24 | `build_options` imported only by `main`, not the library | hygiene |
| 27 | Rails: the inverse census counts only product readers; the quote scan handles `'"'` | §11 |

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
