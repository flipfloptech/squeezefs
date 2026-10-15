# Architecture survey — KV tree, node cache, commit conveyor, checkpoint/SMO

> **Checksum amendment (2026-10-11, decision 0003 §M; 0002 row 480).** Every `xxh3` / `xxh3-64` /
> `xxh3_64` below names the hash the reference used and this port used until format v5. The port's
> ONE hash is now **sqzhash128** (`src/core/sqzhash.zig`, the pinned definition): every 8-byte checksum
> and identity field is its LOW 64 bits under the fixed public format key (`checksum.hash64`,
> `hash64FieldZeroed`, `stamp`/`verify`, the streaming `checksum.Hasher`); the seeded name hashes
> (`hash54`/`hash56`) are `hash64Keyed` under the per-mount name key (`key.NameKey`: the volume's
> `hash_seed` zero-extended, expanded once at mount). Field names on the wire read `csum`. Byte
> layouts, offsets and widths below are unchanged; only the function behind the 8 bytes is.

Reference: `~/Source/squeezefs/src/meta_backend/kv/` (Rust, read-only). Target: the Zig
port under `~/Source/squeezefs-zig`, charter §5.2 (commit pipeline), §5.3 (checkpoints,
writeback, SMOs, space), §2 "Lock order". This document is architecture-level: every
mechanism, state machine, derivation, lock and gauge the charter names, with `file:line`
anchors into the reference. It is NOT a function-by-function catalogue of `backend.rs`.

Companion surveys already written: `/tmp/sqz-specs/kv_node.md` (node format, bsets, fold)
and `/tmp/sqz-specs/kv_journal_ledger.md` (ring framing, reservation core, ledger). This
survey picks up where those stop: the btree over the cache, the cache itself, the
commit conveyor + durability lane + journal lane, the checkpoint/SMO task, leaf merge, the
space law, inos.

All paths below are relative to `~/Source/squeezefs/` unless absolute.

---

## 1. MODULE MAP

### 1.1 Files surveyed, responsibilities, line ranges

| File | Lines | Responsibility (what lives there) |
|---|---|---|
| `src/meta_backend/kv/conveyor_core.rs` | 279 | **Loom-checked** leader-election / FIFO queue / drain core shared by the tx conveyor, the layout-merge conveyor and the durability lane. `ConveyorCore<T>` = `Mutex<VecDeque<(T,u64)>>` + `AtomicBool leader` + optional queued-gauge (`:95-105`). `enqueue`/`enqueue_many` (`:130-150`), `try_lead` (SeqCst CAS, `:158-162`), `drain(max_txs, max_bytes)` first-entry-progress rule (`:169-186`), `unlead_and_recheck` release-then-recheck theorem (`:193-199`). |
| `src/meta_backend/kv/epoch_core.rs` | 385 | **Loom-checked** two-word revalidation publication (`RevalidationEpoch`: tail-then-epoch Release, epoch-then-tail Acquire, `:88-176`), the packed `AppendGate` word (reader bit 31 / writers 16..31 / writer_id 0..16, `:208-265`), `NodeEnv` (epoch + gate + `StructuralHolds`, `:279-300`), the hold-Σ ledger per `HoldClass` (`:304-385`). |
| `src/meta_backend/kv/node_state_core.rs` | 294 | **Loom-checked** node lifecycle word `DIRTY|FREEZING|SUPERSEDED` (`:60-64`): `mark_dirty` (`:167-181`), `begin_freeze` (`:188-210`), `begin_forced_freeze` (`:220-239`), `end_freeze`/`abort_freeze` (`:246-265`), `supersede` (`:273-282`), `try_evict` (CAS 0→SUPERSEDED, `:289-293`). |
| `src/meta_backend/kv/node_cache.rs` | 4595 | RAM-authoritative node cache. `RecordIndex` k-way merged `(key asc, seq desc)` index over bset images (`:181-400`), `OwnedRec` + `FoldedHead` (`:404-480`), `FoldMemo` 8-cell populate-once memo (`:515-625`), `NodeSnapshot` = base + Arc-shared `stable` run + ≤8-record `tail` + memo (`:638-1146`), `NodeDirty` open delta + frozen delta + promise ledger + charge (`:1148-1420`), `CachedNode` (`:1424-2307`) incl. `apply_locked` choke point (`:1697-1971`) and `freeze_locked` (`:2147-2307`), `RootEpoch`/`EpochPurgeSink`/`RevalidateOutcome` (`:2321-2500`), `NodeCache` struct (`:2522-2653`) with `try_get`/`load_for_slot` single-flight (`:3657-3819`), `publish_stamped`/`retire` (`:3889-3973`), `evict_to_budget` clock (`:3992-4037`), `append_frozen` (`:4045-4146`). |
| `src/meta_backend/kv/tree.rs` | 3869 | The CoW btree over the cache. `SmoJournal`/`SmoContext`/`SmoRegion` (`:183-521`), `KvTree` (`:656-688`), latch-free `descend` with reason tallies (`:1215-1363`), `apply_at_seq_class` lock-revalidate-apply (`:1449-1501`), replay applies (`:1512-1680`), `run_maintenance_until` (`:1827-1859`), `checkpoint_flush_node` (`:1914-1972`), `merge_after_flush` prefilter (`:1990-2023`), `maintain_node` (`:2082-2120`), **`smo_replace`** three-step protocol (`:2177-2804`), §4.6a merge: `fold_upper`/`pair_mergeable`/`is_merge_candidate` (`:2976-3038`), `merge_underfull` bounded cursor sweep (`:3088-3233`), `try_merge_node` (`:3234`), `smo_merge` (`:3381`), `smo_root_collapse` (`:3708`). |
| `src/meta_backend/kv/checkpoint.rs` | 3297 | Ledger record codec + slot placement (`:244-950`, covered by the journal survey) **and** the per-volume checkpoint task: derivations (`:956-1115`), `FlushPassSample`/units/projection (`:1116-1270`), `CycleTape`/`CycleTermWindow` (`:1286-1470`), `PENDING_FREE_FORCE_CYCLES`/`COVER_CYCLES_MAX`/`FIXPOINT_COVER_CYCLES_MAX` (`:1500-1529`), `DrainBudget` (`:1546-1577`), `spawn_checkpoint_task`/`spawn_times_drain_task` (`:1579-1635`), `checkpoint_task` loop (`:1680-1839`), `maintenance_pass` (`:1851-1911`), `decide_checkpoint` (`:1965-2042`), `tick` (`:2053-2334`), test seams (`:2340-2410`), **`checkpoint_cycle_managed`** (`:2549-3297`). |
| `src/meta_backend/kv/backend.rs` | 28730 | `KvMetaBackend`. The commit-pipeline slice this survey is about: conveyor types `QueuedTx`/`ConveyorWindow`/`BatchProduct`/`PassSentinel`/`LaneSentinel` (`:19554-19845`), `KvTx` staging (`:19014-19186`), `note_journal_failure` (`:22312`), `commit_tx`/`commit_tx_group` (`:22350-22526`), `conveyor_pass_task` (`:22632-22718`), `fan_out`/`abandon_window` (`:22732-22758`), `durability_lane_task` (`:22797-22871`), `run_batch`/`run_batch_group`/`run_batch_pipeline` (`:22878-23565`), `run_windows`/`await_window_write` (`:23587-23861`), `admit_user_budget` (`:23869-23953`), `admit_heap_locked` (`:23978-24210`), `enter_heap_full` (`:24215`), `rollback_failed_tx` (`:24315-24427`), `sync_device`/barrier lattice/`after_durable_barrier`/`checkpoint_now`/`checkpoint_past` (`:16071-16349`), `cover_bring_up_residue` (`:16381`), pending-times absorber (`:16436-16530`), `shutdown` (`:16670`), `kick_checkpoint_for_ring_park` (`:16917-16925`), `run_merge_sweep` (`:17012`), `destroy_inodes` (`:24748`), `routed_rename_local` (`:25492`), `setattr_locked` absorb arm (`:27727-27766`). The rest is enumerated in §1.3. |
| `src/meta_backend/kv/revalidate.rs` | 658 | Reader-side revalidation orchestration (S5 coherent `-o ro` mount): cadence derivation (`:109-123`), `RevalidationPoller` (`:129-229`), `revalidate_trees` roots-then-epoch (`:242-275`), `TieredEpochPurge` R-6 sink (`:301-348`), predicted-slot ledger read (`:382-410`), `arm_reader_revalidation`/`read_root_epoch` (`:417-470`). **Not in rung 2 scope** (no writer uses it) but it constrains the cache: every node carries an epoch stamp. |
| `src/meta_backend/kv/ino_lane.rs` | 106 | Per-writer ino lanes over `lane_core` (incompat bit 12, built-not-stamped): `LOCAL_INO_BASE = 2` (`:45`), `ino_lane_of`, `first_ino_in_lane`, `recover_ino_floor` = `max(ledger, replayed+1)` rounded up into the lane (`:80-88`), `minted_in_ino_lane`. |
| `src/meta_backend/kv/indirect_map.rs` | 164 | Process-global `ArcSwapOption<IndirectMapIo>` hook (read/write/free closures for the data-plane indirect block-map blob) + `IndirectBlobGuard` RES-9 mint guard. Symmetric-plane rung; one relaxed load on unarmed mounts. **Out of rung-2 scope.** |
| `src/meta_backend/kv/journal_lane.rs` | 195 | The per-volume OS thread `sqz-jrnl<N>` owning the volume's journal io_uring (`JOURNAL_RING_ENTRIES = 512`, `:53`), `RingPark` (park = `io_uring_enter` with the wake-eventfd READ SQE armed, `:77-102`), `JournalLane::spawn` (`:116-159`), `spawn_task` panic-contained (`:163-168`), `submit_write_at_batch` lane-native only from the lane thread (`:174-188`), ringless degrade (`:118-128`). |
| `src/meta_backend/kv/mod.rs` | 1371 | Module list (`:48-80`); every process-global `META_KV_*` / `META_CONVEYOR_*` / `META_COMMIT_*` static counter with its doc (`:84-780`); `ParkedGaugeGuard` (`:784-797`); `CommitGroupSizeHistogram` 12 buckets (`:800-880`); `META_KV_COMMIT_SITES` per-site attribution (`:895-934`); `KvError` typed classes + exhaustive `Clone` + errno mapping (`:942-1371`). |

Supporting modules referenced but surveyed elsewhere: `journal_core.rs` (641, loom: admission/reservation/`reusable_upto`; `CoreGeometry::smo_keep` `:192-194`), `journal.rs` (2425: `reserve_registered` `:1109`, `complete` `:1163`, `completed_upto`/`min_inflight_start` `:1176-1191`, `wait_completed_upto` `:1197-1215`, `advance_reusable_upto` `:1220`, `space_notified` `:1233`, `submit_entries_batch` `:1589`, `checkpoint_reserve_bytes` `:230`), `alloc_ext.rs` (`compaction_reserve_extents = max(8, total/50)` `:156`, `compaction_floor_extents = reserve/2` `:169`, `free_pending_forced` `:953`, `advance_durable` `:989`), `node.rs` (`NodeLayout::fold_capacity` `:379`, `split_part_capacity` `:387`, `merge_pair_capacity` `:395`, `merge_candidate_capacity` `:407`, `smo_extents_for_parts` `:423`, `split_growth_window` `:436`, `append_frame_len` `:445`), `src/meta_backend/dlm.rs` (4a lock manager), `src/stripe_locks.rs` (lock-order doc `:30-130`, `dlm_stripe_width` `:450`).

### 1.2 Dependency DAG (rung-2 slice)

```
                 record.rs (fold algebra)   bset.rs   node.rs (format, NodeLayout)
                          \                   |          /
                           v                  v         v
   node_state_core.rs ──► node_cache.rs ◄── epoch_core.rs      journal_core.rs (loom)
   (loom)                    │  ▲                                    │
                             │  │ dirty floors / durable tail       v
                             v  │                               journal.rs (ring I/O,
   alloc_ext_core.rs ──► alloc_ext.rs ──► tree.rs ◄─────────── reservation registry,
   (loom)                (extents,        (KvTree, SmoContext,  completed_upto)
                          pending-free)    smo_replace, merge)      │
                                              │                     │
                                              v                     v
                       checkpoint.rs ◄──── backend.rs ◄──── conveyor_core.rs (loom)
                       (task, cycle)        (KvMetaBackend:         ▲
                                             KvTx, conveyor,        │
                                             lane, admission,  journal_lane.rs
                                             heap law, inos)   (sqz-jrnl<N> thread)
                                              │
                                              v
                              dlm.rs / stripe_locks.rs (4a guards, held not acquired by the pass)
```

Edges that matter for the port order: `tree.rs` needs the cache, the allocator and the ring
(it reserves SMO entries); `checkpoint.rs` is `impl KvMetaBackend` for the cycle and
free functions for derivations — it cannot stand without `backend.rs`'s fields; the
conveyor cores (`conveyor_core`, `node_state_core`, `epoch_core`, `journal_core`) are
dependency-free by construction so the model checker can `#[path]`-include them
(`loom-models/src/lib.rs:310-388`).

### 1.3 What else lives in `backend.rs` (skimmed, with line ranges)

| Band | Content |
|---|---|
| `:1-133` | Module doc: mount sequence, readdir cookie contract, **the two-stage conveyor argument** (D-2) incl. the 4b acyclicity proof (`:97-123`) and the tail-rule argument (`:125-132`). |
| `:204-700` | Knobs/constants: `NODE_CACHE_MB_ENV`, `PENDING_FREE_CAP = 65_536` (`:210`), `JOURNAL_FAILURE_LATCH = 3` (`:241`), ~30 `TEST_*` seams (`:249-640`), `COMMIT_RETRY_BUDGET = 256` (`:649`), batch-cap derivations (`:655-686`), `PENDING_TIMES_DRAIN_BATCH = 128` / `_CAP = 512` (`:689-694`). |
| `:731-1200` | `xattr_name_allowed` allowlist, `WriterClaim`/`MountRegistration` (D0 guard records), `ClaimClearOutcome`, `MigrationTee`, `ForestCensus`, `TreeSet` (flat vs forest). |
| `:1213-1811` | **The `KvMetaBackend` struct** (every field documented). |
| `:1843-1889` | `resolve_node_cache_budget` = `max(budget/16, 512 MiB)` with MB/PCT overrides. |
| `:1891-3300` | `open`/`open_inner`: SB → ledger → bitmap → **two-phase replay** (`:2700-2850`) → trees → `next_ino` (`:2935-2936`) → appender regions → park replayed frees (`:2900-2933`). |
| `:3000-5100` | Forest/symmetric plumbing: slot words, token planes, regions, slot trees (`maintainable_trees_rotated` `:5000s`). |
| `:6000-11999` | Manager verbs (acquire/offer/release slot, extent grants, join, ring growth), appender pages, flush-ceiling audit — symmetric plane (rung 6). |
| `:12000-13999` | Ino cursors/lanes (`allocate_ino`, `next_ino` `:12264`, `live_inodes`), checkpoint telemetry faces (`:13000s`), tree routing helpers (`lookup_kind`/`range_kind`/`tree_of_node`), block-refs/block-map engagement. |
| `:14000-15999` | Block-map tree sweeps/migration (PB-class), read side `lookup`/`getattr`/`readdir_page`/`getxattr`/`listxattr`, `allocate_ino`/`reserve_ino_range`. |
| `:16000-17300` | **Durability lattice** (`sync_device`, `note_barrier_failure`, `after_durable_barrier`), `checkpoint_now/past/past_region`, `cover_bring_up_residue`, pending-times, `shutdown`, kicks, `dead_bset_census`, `run_merge_sweep`, `merge_candidates_audit`, `defrag_merge_sweep`. |
| `:17284-18996` | D0 mount guard: flock on a dedicated fd, PR reservations, `writer_claim`, dead-pid proof, `claim_clear`, guard heartbeat. |
| `:18997-19200` | `KvTx`, `UndoKey`. |
| `:19200-19553` | `QueuedLayoutMerge` (Lever B layout conveyor), block-map train/crossing guards, frame fence/screens. |
| `:19554-19893` | **Conveyor types + sentinels**, `OpenPosture`, `RoutedParentUpdate`. |
| `:19894-22311` | Forest root publish, reader resync, control entries, region replay. |
| `:22312-24430` | **The conveyor** (both stages), admission, heap law, rollback, compensation. |
| `:24430-24750` | `enter/leave_heap_full`, `heap_growth_floor_clear`, parent-update staging. |
| `:24748-25490` | `destroy_inodes*` (batched, chunked). |
| `:25492-27700` | Routed arms: create/mint/add_dentry/unlink/link/rename/touch_ctime/dest_replace; layout set/stage/merge (write-commit economy, chained deltas). |
| `:27703-28298` | `setattr_locked` (SETATTR echo absorber), xattr set/remove, cross-volume (xv) steps. |
| `:28299-28730` | `impl Metadata for KvMetaBackend`, `StructuralHold`, `DirtyLeafAge`. |

---

## 2. MECHANISMS — where they live and how they work

### 2.1 Transaction staging: `KvTx` with read-your-own-writes

- **Shape** (`backend.rs:19014-19047`): `staged: Vec<(tree_id u8, key Vec<u8>, RecordKind, Bytes)>` in stage order; `site: &'static Location` (`#[track_caller]`, D4.a attribution); `guards: Arc<[DlmGuard]>` (co-owned with the queue entry, see §2.4); `token_quiet`, `served_step` (symmetric-plane flags).
- **Staging API**: `stage_put/stage_delta/stage_delta_raw/stage_delete` (`:19076-19100`), `stage_block_refs` / `stage_block_map` (same tx as the inode/layout record so accounting can never disagree with the layout — "one tx = one checksummed journal entry", `:19102-19181`).
- **RYOW**: reads inside an op fold the tx overlay over the cache. The dentry chain probe `chain_occupancy` (`:24591-24617`) scans the live range, then walks `tx.staged` and removes slots with a staged `Delete`, inserts slots with a staged `Put`, ignores `Delta`. `dentry_insert_key` (`:24624`) uses it to pick the first free `coll_seq` so a rename EXCHANGE can re-insert into a slot it just deleted. There is no task-local; ops thread one `KvTx` explicitly (`:19001-19003`) — kv ops never nest transactions.
- **Seqs are not assigned at stage time** — `Record.seq = 0` until the pass stamps it from the reservation inside the lock window (`:22554-22559`, `:23370-23377`).
- Pre-queue admission at `build_queued_tx` (`:22534-22619`): forest key framing, debug record audit, the per-volume **record-value cap** `record_value_cap = min(64 KiB, node_size/4)+envelope` enforced at the commit choke point (so an over-cap record fails its own commit, never the checkpoint's freeze — the 2026-08-19 AlreadyFreezing wedge's third leg), `len = entry_len_for(&recs)` (the exact §4.1 entry length, 128 KiB cap), `region = region_of_records`.

### 2.2 The commit conveyor — stage A (apply pass)

**Queue entry shape** (`QueuedTx`, `backend.rs:19554-19596`): `{token_quiet, recs: Vec<(u8, Record)>, region: u32, len: u64 (exact entry length), enqueued_at, trace_id, site, _guards: Arc<[DlmGuard]>, _door: Option<DoorPass>, _park_pass: Option<Arc<AdmissionPass>>, done: oneshot::Sender<Result<(), KvError>>}`. The three charter items are `recs`, `_guards`, `done`.

**Committer protocol** (`commit_tx`, `:22350-22388`): (0) slot door (symmetric; no-op unarmed) → (1) `build_queued_tx` → (2) `conveyor.enqueue(entry, len); lead_pass_if_elected(weak)` with **no await between** → (3) `park_on_outcome(rx)` under `ParkedGaugeGuard(META_COMMIT_PARKED)`. Election winner spawns `conveyor_pass_task` on the journal lane (or `sqz-meta` with the lane off) — `spawn_conveyor_task` (`:22302-22310`). **Group enqueue** (`commit_tx_group`, `:22470-22526`): build every member first, `enqueue_many` under ONE queue lock, ONE election, await fan-outs in order; counts `META_CONVEYOR_GROUP_COMMITS/_TXS`.

**Pass task loop** (`conveyor_pass_task`, `:22632-22718`): `weak.upgrade()` per batch (a dropped backend fails out residue and unleads); `drain(batch_max_txs, batch_max_bytes)`; empty ⇒ `drop(be)` then `unlead_and_recheck` (never park on leadership holding the backend); else `run_batch` → for each window `note_window_inflight(); lane.enqueue(window, n); if lane.try_lead() spawn durability_lane_task` (again no await between enqueue and elect) → `drop(be)` → `fan_out(outcomes)` for pre-reserve failures.

**Batch caps** (`:655-686`): `batch_max_txs = env ≥1 verbatim else max(64, cpus×2)`; `batch_max_bytes = env ≥1 verbatim else max(256 KiB, ring_user_capacity/16)`, then clamped at open to the ring's user-admissible capacity so one Σ-admission can always eventually succeed (`:1464-1472`). No timers anywhere — jbd2's no-wait shape (`conveyor_core.rs:21-25`).

**`run_batch` → `run_batch_group` → `run_batch_pipeline`** (`:22878-23565`), per region group (one reservation in one ring):

1. Counters: `META_CONVEYOR_LEADER_PASSES`, `META_COMMIT_GROUP_SIZE.record(n)`, `META_COMMIT_GROUP_BYTES`; per-tx `TxQueueWait` phase; debug-assert the Issue-13 same-key co-queue exclusion (only `Delta`/`Delta` may share a key across members, `:22939-22966`).
2. Token recall (symmetric; `:22989-22994`), region-growth Dekker (`:23010-23029`), `PassSentinel` armed (`:23031-23043`).
3. Liveness re-check (shutting down / failed ⇒ `fail_batch`) (`:23083-23087`).
4. **ONE ring admission for Σ len BEFORE any node lock**: `admit_user_budget(ring, region, total_len)` (`:23098-23110`) — see §2.7 for the park loop. Returns `Admission` + park-gate pass; park pass clones onto every member.
5. **Union lock window, drop-all-and-relock retry** (`:23141-23500`), bounded by `COMMIT_RETRY_BUDGET = 256`:
   - resolve every record's leaf latch-free (`tree.resolve_leaf`), per entry;
   - `lock_set = flatten; sort_by_key(addr); dedup_by_key(addr)`; take write guards ascending (`:23186-23194`);
   - **revalidate every member** under the locks: `is_superseded || key < min_key || key > max_key` ⇒ record hold, drop ALL guards, `META_KV_COMMIT_SMO_RETRIES++`, `continue` (`:23205-23219`);
   - **heap admission** `admit_heap_locked` (§2.12) — a refused member is removed pre-reserve; one "return room" retry via two `checkpoint_now()` cycles under `begin_space_retry` when `heap_promised > 0 || pending_count > 0` (`:23236-23322`); then re-admit ring budget and re-resolve (`continue`);
   - **pre-images** once per distinct `(tree, key)` across the batch — first touch wins (`:23324-23360`);
   - **ONE contiguous reservation inside the window**: `(res, seq_base) = ring.reserve_registered(adm)`; per-tx seqs stamped back-to-back `r.seq = seq_cursor + i; seq_cursor += q.len` (`:23367-23378`);
   - **apply in queue order**, grouped per node per tx, `node.apply_locked(&mut guards[gi], group, entry_start)` where the floor contribution is the member's **entry start**, never the record seq (FIND-SMO-TAIL §1b, `:23392-23428`); a member whose apply fails is rolled OUT in-window by `remove_overlay_records_locked(.., member_span)` and recorded in `failed` (`:23450-23463`);
   - slot frontier notes; `applied_unrolled = survivors > 0`; threshold check `overlay_bytes ≥ writeback_delta_bytes ⇒ tree.enqueue_maintenance(addr)` and `ckpt_wake.notify_one()` after unlock (`:23477-23498`);
   - record `LeafLockHold`, drop guards, `break`.
6. **Submit** the surviving entries outside every lock: `parts = [(Reservation::unpadded(start, len), &recs)]` skipping failed members (their sub-ranges stay unwritten holes), `ring.submit_entries_batch(&res, &parts, lane)` — one submission, NO wait (`:23514-23532`).
7. **Handoff**: move reservation/entries/undo/write handle into a `ConveyorWindow` (`:23537-23565`); `applied_unrolled = false` (the lane's sentinel covers from here). `region_window_opened` for ring growth accounting.

**Window shape** (`ConveyorWindow`, `:19604-19641`): `{ring, region, entries: Vec<QueuedTx>, failed: Vec<(idx, KvError)>, res: Reservation, res_open: bool, seq_base, undo: Vec<UndoKey>, write: Option<Result<Option<EntriesWriteInFlight>, KvError>>, t_pass, t_handoff, submitted_at, traced}`.

**Panic guard** (`PassSentinel`, `:19675-19782`): on Drop with content — send computed outcomes, `META_CONVEYOR_PASS_PANICS++`, `ring.complete(res)` (abandon, never a `completed_upto` wedge), `core.release(adm)`, fail remaining members EIO, abandon a built-but-unhanded window, latch `failed` if `applied_unrolled` (RAM diverged from replay), drain+fail the queue behind the dead leader with `unlead_and_recheck`, `note_journal_failure()`.

#### 2.2.1 Stage A as a step table (lock state and failure disposition per step)

| Step | What | Node locks | Ring state | On failure |
|---|---|---|---|---|
| A0 drain | `conveyor.drain(txs_cap, bytes_cap)` FIFO prefix | none | — | empty ⇒ `drop(be)`, `unlead_and_recheck` |
| A1 flags | `is_shutting_down || is_failed` | none | — | `fail_batch` every member EIO (pre-reserve outcome) |
| A2 admit | `admit_user_budget(Σ len)` — may PARK | **none** | `admitted += Σ len + slack` | shutdown/failed while parked ⇒ `fail_batch`; parked ≥ 30 s ⇒ `note_journal_failure` per crossing |
| A3 resolve | per record `tree.resolve_leaf(key)` latch-free | none | admission held | device-read class ⇒ release admission, `fail_batch` |
| A4 lock | sort+dedup by `addr`, write-lock ascending | union held | admission held | — (cannot fail; waits only on other node-lock holders) |
| A5 revalidate | every member `!superseded && min ≤ key ≤ max` | union held → dropped on stale | admission held | stale ⇒ `SMO_RETRIES++`, goto A3 (budget 256; exhaustion ⇒ release admission, `fail_batch(Corrupt)`) |
| A6 heap admit | `admit_heap_locked` promises/refuses per member | union held | admission held | refused members removed; one retry via 2 cycles under `begin_space_retry` (no locks held across them); re-admit ring budget; goto A3 |
| A7 pre-images | `snapshot().lookup(key)` first-touch per `(tree,key)` | union held | admission held | lookup error ⇒ release admission, `fail_batch` |
| A8 reserve | `reserve_registered(adm)` → `(res, seq_base)`; stamp `seq = seq_base + Σlen_j + i` | union held | `head += Σ len (+pad)`; registered in `inflight.open` | — (cannot block: budget already admitted) |
| A9 apply | per tx, per node `apply_locked(group, entry_start)` | union held | reservation open | member apply error ⇒ `remove_overlay_records_locked(member_span)`, recorded in `failed`; batch survives |
| A10 threshold | `overlay_bytes ≥ 4096 ⇒ enqueue_maintenance`; `applied_unrolled = survivors > 0` | union held → **dropped** | reservation open | — |
| A11 wake | `ckpt_wake.notify_one()` if any threshold crossed | none | reservation open | — |
| A12 submit | `submit_entries_batch(res, parts without failed, lane)` — NO wait | none | reservation open, bytes in flight | encode refusal ⇒ `write = Err` carried to the lane (a failed write with nothing landed) |
| A13 handoff | build `ConveyorWindow`; `applied_unrolled = false`; `region_window_opened` | none | reservation open (lane completes it) | sentinel Drop with a built window ⇒ `abandon_window` + FAILED |

Panic at A2–A7 ⇒ release admission, fail members, unlead; panic at A8–A11 (`applied_unrolled`) ⇒ additionally latch FAILED (RAM would diverge from replay: `PassSentinel::drop`, `:19708-19782`).

### 2.3 The durability lane — stage B

**Task** (`durability_lane_task`, `:22797-22871`): `carry: VecDeque<ConveyorWindow>`; drain the whole lane queue FIFO; `head = carry.pop_front()`; upgrade backend; `run_windows(head, &mut carry)`; `drop(be)`; `park_gate::hold_acks` (symmetric parks); `fan_out`.

**Group formation + protocol** (`run_windows`, `:23587-23823`):

8. Await the head's write (`await_window_write`: `inflight.finish(ring)`, phases `WindowLaneWait`, `JournalRingWrite`); then pull every `carry` front whose `write_landed()` (non-blocking poll) — the group. Await their writes too.
9. **Complete every window's reservation in journal order** (`ring.complete(&w.res)`, `res_open = false`) — unconditional, lane-owned, so `completed_upto` walks forward through the lane only (law 4 of the two-stage loom model).
10. If any write succeeded: **ONE completed-prefix wait** per ring in the group `wait_completed_upto(max padded_end)` (`:23651-23666`).
11. Per window: `Ok` ⇒ `journal_failures = 0`, record apply-holes `(region, padded_end)` if `failed` non-empty, verdict Ok; `Err` ⇒ `rollback_failed_tx(SeqSpan::stamped(seq_base, res.len), &undo, &ring)`, `note_journal_failure()`, `checkpoint_past_region(region, padded_end)` (the permanent hole must be behind the tail before any later ack), verdict Err.
12. **Hole checkpoints** for acked windows with in-window apply failures: `checkpoint_past_region` BEFORE acking survivors (`:23733-23747`); a failure here fails the survivors loud rather than acking unreachable entries.
13. **ONE barrier** on the strict cadence (`self.strict && any_ok && hole_err.is_none()` ⇒ `sync_device()`), else `needs_flush = true` (`:23751-23761`).
14. **Terminal outcomes in journal order**: per member the failed-at-apply error, else `(verdict, hole_err, barrier_out)` ⇒ outcome; success counts `note_commit_site(q.site)` and the migration tee; `region_window_settled`, `note_window_done()` (`:23768-23821`).

**Fan-out** (`fan_out`, `:22732-22741`): `drop(_guards)` BEFORE `done.send(outcome)` (D-3: an answered committer re-asking for the same key must not park on its own previous tx; measured as 11 % of many-dirs renames at 32 writers self-waiting, `dlm_inode_key_waits` p50 ≤ 16 µs).

**Panic guard** (`LaneSentinel`, `:19793-19845`): send computed outcomes, abandon in-progress windows (`abandon_window`: complete reservation if open, fail members EIO, `note_window_done`), drain+abandon the lane queue, unlead, **latch `failed`** (an applied window's outcome is unknown), `note_journal_failure()`.

**Acks only after the entry landed; strict after the barrier; guards drop before ack; acks in journal order** — all four charter laws are visible in steps 8–14 + `fan_out`.

#### 2.3.1 Stage B as a step table

| Step | What | Node locks | Ring state | On failure |
|---|---|---|---|---|
| B0 drain | `lane.drain(∞)` into `carry`; `head = pop_front` | none | — | empty ⇒ `unlead_and_recheck` |
| B1 upgrade | `weak.upgrade()` | none | — | backend gone ⇒ answer dead receivers, `note_window_done`, unlead |
| B2 await head | `inflight.finish(ring)` (CQE on the lane's own ring) | none | head reservation open | write error ⇒ verdict Err (B6) |
| B3 group | pull every `carry` front whose `write_landed()`; await each | none | all open | — |
| B4 complete | `ring.complete(&res)` per window **in journal order** | none | `completed_upto = min(open) else head` advances | — (unconditional: abandoned ranges complete too) |
| B5 prefix | ONE `wait_completed_upto(max padded_end)` per ring if any write Ok | none | — | parks behind the checkpoint task's own in-flight entries (never behind a node lock) |
| B6 verdicts | Ok ⇒ `journal_failures = 0`, apply-hole noted; Err ⇒ `rollback_failed_tx` + `note_journal_failure` + `checkpoint_past_region(padded_end)` | **rollback arm only**: leaf locks ascending, revalidated, released before compensation | — | hole checkpoint failure ⇒ `note_journal_failure` (volume escalating) |
| B7 hole ckpt | `checkpoint_past_region` for every acked window with in-window failures | none (the cycle takes the SMO mutex and its own node locks) | zero ring bytes | failure ⇒ `hole_err` fails the survivors loud |
| B8 barrier | strict ⇒ ONE `sync_device()`; else `needs_flush = true` | none | — | barrier error ⇒ every survivor Err; `note_barrier_failure` lattice |
| B9 outcomes | per member in journal order: failed-at-apply error / verdict / hole_err / barrier_out; `note_commit_site`; tee | none | — | — |
| B10 settle | `region_window_settled`, `note_window_done` per window | none | — | — |
| B11 fan-out | `drop(_guards)` THEN `done.send(outcome)` (after `hold_acks`) | none | — | dead receiver harmless |

Panic anywhere in B2–B10 ⇒ `LaneSentinel::drop`: abandon in-progress + queued windows (complete reservations, EIO), unlead, **FAILED**, `note_journal_failure` (`:19801-19845`).

#### 2.3.2 Seq-conditional rollback (`rollback_failed_tx`, `backend.rs:24315-24427`)

Phase 1 — re-resolve every `UndoKey`'s leaf, lock ascending/deduped, revalidate (stale ⇒ retry, budget 256), then per key `remove_overlay_records_locked(key, span)` where `span = SeqSpan::stamped(seq_base, res.len)` is the window's STAMPED seq range (the overlay's address domain — Issue 20; `res` stays the position domain for the hole). Probe `snapshot().newest_record_seq(key)`: if the newest is still inside the span, one of this window's records escaped the open overlay (a freeze raced the failed write) ⇒ stage a compensating `Put(pre)` or `Delete`. Phase 2 (rare) — `commit_compensation(tx, ring)` through the checkpoint-class reserve (never parks behind user admissions; skip-if-newer re-checked under the locks) so a crash after the escaped records reach a durable bset still replays to the rolled-back state. A newer committed record on the same key (only a Δtime merge can be one) is left standing — LWW-correct (§4.4 pt 4).

### 2.4 4a object locks and guard co-ownership

- `DlmLockManager` (`dlm.rs:66-70`): two `LockClass`es (inode `I{ino}`, dentry `D{parent,name}`), each `StripeLocks<Arc<SqzRwLock<()>>>` + `StripeCensus`. Width `dlm_stripe_width()` (`stripe_locks.rs:450-460`) = `SQUEEZEFS_DLM_STRIPES` explicit else `resolve_dlm_stripes(shipped=4096, possible_cpus, q_depth)` ⇒ `next_pow2(max(4096, 16 × cpus × q_depth))` (doc `dlm.rs:14-17`); `q_depth` is the transport's sizing depth (explicit knob clamped to `Q_DEPTH_DESIRED`, else `Q_DEPTH_DESIRED`, `:434-445`).
- Dentry hash input `parent ^ xxh3_64(name).rotate_left(1)` (`dlm.rs:103-105`).
- **Canonical multi-key acquisition** (`dlm.rs:24-32`): all `I` before any `D`; within a class dedup by stripe index, ascending; shared+exclusive on one stripe ⇒ exclusive; taken holding no other 4a guard; two-phase discovery (unlink's child) locks, reads, drops, re-locks full set, revalidates. The ONE sanctioned out-of-order addition is `try_lock_many` (try-only, all-or-nothing; `stripe_locks.rs:74-81`).
- `lock_stripe` (`dlm.rs:128-199`): try-acquire fast path (zero clock reads); contended path classifies `stripe_collisions` vs `key_waits` against the stripe's last-acquirer word, records `lock_phase_ns.dlm_guard_wait`, then parks. Exclusive `I` guards are hold-timed (`dlm_guard_hold`).
- **Co-ownership** (design-metadata-throughput §5.5 rev 2, Issue 13): the op's guard set becomes `Arc<[DlmGuard]>` on the `KvTx` (`hold_guards`, `backend.rs:19072-19074`) and travels into `QueuedTx._guards`; the pass **holds but never acquires** 4a (`stripe_locks.rs:103-108`), and releases at the terminal outcome (fan-out; post-rollback on failure). A dropped committer future drops only its frame's refs — exclusion is structural from ownership, modelled as `conveyor_guard_outlives_dropped_committer_until_terminal` (`loom-models/src/lib.rs:2495`).
- Parent Δtime merge records are the one same-key class allowed to co-queue (SHARED parent guard, LWW by seq — §4.4 pt 6).

### 2.5 4b leaf locks (commit) and interior locks (SMO)

- Per-node `SqzRwLock<NodeDirty>` (`node_cache.rs:1451`). Readers never take it (`snapshot()` is an `ArcSwap` load, `:1629`).
- Commit path: leaves only, ascending `addr`, deduped, revalidate-then-retry (`backend.rs:23186-23219`; the single-record form `tree.rs:1457-1463`). **Never held across device I/O or ring waits**: admission precedes the window; the submit follows the unlock; `lock_phase_ns.leaf_lock_hold` is the tripwire whose tail must sit ≪ device latency (`fuse_client.rs:3420-3425`).
- SMO task: parent (interior) then child/children ascending address (`tree.rs:2546-2551`, merge: `tree.rs` §4.6a (b) 2). Interior mutation is exclusively `apply_locked_structural` on the SMO task, enforced at the gate (`node_cache.rs:1766-1773`).
- Acyclicity argument: `backend.rs:97-123` — taker population {stage A, stage B rollback arm, checkpoint/SMO task}; no node-lock holder waits on B; B holds node locks only while waiting on node locks ascending; checkpoint admissions never park (drain-and-retry).

### 2.6 The journal lane (one OS thread per writable volume)

`journal_lane.rs`: `JournalLane::spawn(path, idx)` creates an `OwnedRing(512)` on the spawning thread, moves it into thread `sqz-jrnl{idx}` where `LANE_REACTOR` (thread-local) holds the reactor; the lane executor is a `LaneExec::with_park(RingPark)` whose `park(tick)` is `reactor.park(tick)` = `io_uring_enter` with the wake-eventfd READ SQE armed, `unpark()` bumps the eventfd, `service()` reaps CQEs once per loop iteration (`:77-102`). Both conveyor stages are spawned onto it (`backend.rs:22302-22310`); the lane is created lazily on the volume's FIRST commit (`journal_lane: OnceLock<Option<...>>`, `:22283-22296`), gated by `SQUEEZEFS_JOURNAL_LANE` (default on). `submit_write_at_batch` is lane-native only when called ON the lane thread with a ring, else `None` ⇒ pool path (`:174-188`). Degrade-loud: no io_uring ⇒ condvar-parked lane, pool writes, `JOURNAL_LANES_RINGLESS++` (`:118-128`). Counters `journal_lanes_spawned/ringless`, `journal_ring_lane_writes/pool_writes` (engagement: lane writes ≡ passes on a lane-armed mount).

Why: `.benchmarks/2026-09-03-c2-uring-fs-completion-hop.md:235-271` — on the shared `sqz-meta` lane the oneshot `wake_hop` was 746/664 µs of a 1,209/1,130 µs journal round trip (**62 %/59 %**); the lane took it to 53–107 µs, `journal_ring_write` −34…−57 %.

### 2.7 Ring admission, the park, the kick

`admit_user_budget` (`backend.rs:23869-23953`): `park_gate::pre_admission()` (symmetric; inert otherwise) → loop { `ring.try_admit(len, User)` ⇒ return; `stalls++`; `notified = ring.space_notified()` (register) → re-try (recheck) → liveness flags → **`kick_checkpoint_for_ring_park()`** (sets `ring_park_kick = true` + `ckpt_wake.notify_one()`, `:16917-16920`) → `timeout(until_crossing, notified)` with `until_crossing = threshold − parked_since` floored at 10 ms → if parked ≥ `SQUEEZEFS_TIMEOUT` (default 30 s): `log::error` + `note_journal_failure()` (D1.b audit row 2: a wedged-not-failed drain escalates through the lattice), reset the clock → re-check flags }.

The checkpoint task consumes the mark via `take_ring_park_kick()` (`checkpoint.rs:1750-1754`): a wake carrying it is promoted to a cadence tick whose decision is `due` with `ring_pressure` (⇒ `barrier_now`), because the pressure law `distance > logical_len/2` is unreachable on a ring whose reserve is half of it (the 512 KiB floor), and under the sector-pad law a serial commit occupies a whole 4 KiB page (`:1732-1749`).

### 2.8 The node cache

- **`NodeCache`** (`node_cache.rs:2522-2653`): `map: scc::HashMap<addr, Arc<CachedNode>>`; `inflight: scc::HashMap<addr, Arc<sqz_flight::Sender>>` (single-flight); `clock: scc::Queue<u64>`; `cached_bytes: Arc<AtomicU64>` (Σ mapped extents + overlay + heads + memo); `heap_promised: Arc<AtomicU64>`; `space_retries`/`merge_yields`; `env: Arc<NodeEnv>` (durable tail + epoch + gate); `lease` gate; `retired: scc::HashSet<u64>`; `dying_floors: AtomicU64`; `dying_leaf_floors` per slot; `slot_tails` ArcSwap; `slot_frontiers`; projection-refresh hook.
- **`CachedNode`** (`:1424-1484`): immutable identity `{addr, node_seq, tree_id, level, min_key, max_key}`; `state: NodeState` (lifecycle word); `snapshot: ArcSwap<NodeSnapshot>`; `dirty: SqzRwLock<NodeDirty>`; `ref_bit`, `pinned`; `dirty_floor: AtomicU64` (`u64::MAX` = none); `epoch_stamp`; forest slot stamp.
- **Snapshot** (`:638-654`): `base: Arc<RecordIndex>` (k-way merged `(key asc, seq desc, newer-source-first)` index over bset images, 24 B `IdxRec`s, `:166-262`) + `stable: Arc<Vec<OwnedRec>>` shared across publishes + `tail: Arc<Vec<OwnedRec>>` ≤ `OVERLAY_TAIL_MAX = 8` (`:632`) + `memo: FoldMemo`. Per-apply publish clones only the tail; overflow re-merges into a fresh stable (PR K7: the whole-overlay clone per apply was ~44 % of the serial create path, `:1948-1956`).
- **Dirty half** (`NodeDirty`, `:1148-1201`): `overlay: Vec<OwnedRec>` authoritative `(key asc, seq asc)`; `overlay_bytes`, `head_bytes`; publish mirrors `snap_stable`/`snap_tail`; `max_applied_seq` (run-precedence guard); `frozen: Option<FrozenDelta{records, horizon, frame_len}>`; `tail_offset`; `charged`/`charge`; `promised`/`promise_basis`/`promise_added`/`heap_promised` (§2.12).
- **`apply_locked_class`** (`:1728-1971`), the ONE RAM-mutation choke point: gate checks (reader ⇒ refuse; interior && !authority ⇒ refuse; slot lease verdict) → `state.mark_dirty()` (Err ⇒ "apply on superseded node") → `dirty_floor.fetch_min(floor)` (+ forest age stamps) → per record: stamp `entry_floor = min(entry_floor, floor)`, `partition_point` insert, **D7.a head materialization** (`Put` ⇒ Live, `Delete` ⇒ Tombstone, `Delta` ⇒ `fold_forward(prev_head)`), single-head-per-key, inversion guard invalidates stale heads above, monotonic seq ⇒ insert into `snap_tail` else re-merge stable → `resync_charge` → `snapshot.store(NodeSnapshot{base, stable, tail, memo: fresh})`.
- **Demand paging** (`load_for_slot`, `:3690-3819`): map hit (+ stale-epoch drop) → `retired` check ⇒ `Ok(None)` → `inflight.entry_sync`: Occupied ⇒ subscribe, `wait().await`, `continue`; Vacant ⇒ insert sender + `InflightLoadGuard` → `env.epoch.load_snapshot()` **before** the device read → `load_node_screened(path, layout, addr, snap.tail, screen)` (one `read_at`, K2 verification, positional torn-tail classifier) → re-check `retired` after the read → `CachedNode::from_loaded(.., pinned=false)`; `level>0 ⇒ pin()`; block-map leaves ⇒ probation → `publish_stamped(node, snap.epoch)` → `META_KV_NODE_CACHE_MISSES++`. All scc buckets taken sync, never across an await (`:3695-3705`).
- **Pinning**: interior nodes always (`:3802-3804`; SMO successors `pinned = level > 0 || root`, `tree.rs:2325`); roots pinned at open/create; dirty/serializing pinned by the lifecycle CAS (`try_evict` only from word 0).
- **Clock eviction** (`evict_to_budget`, `:3992-4037`): while `cached_bytes > budget` and attempts ≤ `2×(map.len()+1)`: pop clock; stale ⇒ skip; pinned ⇒ skip (never re-enter); `ref_bit.swap(false)` true ⇒ second chance; `Arc::strong_count > 2` ⇒ held externally, re-enter (evicting frees nothing and causes resolve→lock→revalidate reload thrash — PR M9 tiny-budget finding); `state.try_evict()` ⇒ `note_node_dying_floor`, `remove_if_sync`, `cached_bytes −= node_size`, `EVICTIONS++`; else dirty/serializing ⇒ re-enter.
- **Budget**: `resolve_node_cache_budget(budget, MB, PCT)` = MB×MiB verbatim, else PCT of the R5 budget, else `max(budget/16, 512 MiB)` (`backend.rs:1843-1877`; `DEFAULT_CACHE_BUDGET_BYTES = 512 MiB` is the never-regress floor, `node_cache.rs:118`). Charged: extent bytes at publish/evict + overlay+heads via `resync_charge` + memo bytes via `FoldMemo` Drop-exact.
- **`publish_stamped`** (`:3902-3945`): replacing an Occupied mapping supersedes the old object and folds its floor into `dying_floors` (loud if it was dirty/freezing); Vacant charges one extent; clears `retired`; evictable ⇒ `clock.push`; `evict_to_budget()`.
- **`retire`** (`:3955-3973`): `note_node_dying_floor` → `retired.insert` BEFORE `remove_if_sync` (so a loader that missed the map sees the mark on its post-read re-check) → uncharge.
- **Fold-head memo** (`FoldMemo`, `:536-612`): `[OnceLock<MemoCell{key, look, horizon}>; FOLD_MEMO_CAPACITY = 8]`; `probe` scans ≤ 8 cells (`:560-565`); `populate` claims the first empty cell, loser re-checks whether the winner memoized its key (`:572-602`); charged to `META_KV_FOLD_MEMO_BYTES` and the cache budget, Drop-exact. Property test: memo ≡ from-scratch fold (`tests/kv_fold_slimming_tests.rs`; the `META_KV_FOLD_RECORD_DECODES` zero-decode pin, `mod.rs:657-667`).

### 2.9 Writeback: freeze → release → append

- **Freeze under the lock** (`freeze_locked`, `node_cache.rs:2147-2307`): returns the existing frozen delta if one is parked; RES-22 self-heal tripwire for `FREEZING` latched with no frozen delta (`kv_freeze_latched_recovered`, `:2168-2185`); empty overlay ⇒ `None`; `state.begin_freeze()`; `FreezeRestore` guard (W-A: any error after the word transition `abort_freeze`s); **shadow-fold** one linear pass over per-key runs keeping from the newest `Put`/`Delete` (`META_KV_NODE_FREEZE_SHADOW_DROPPED`, `:2237-2259`); `horizon = max seq`; `encode_bset_frame`; `base.extend_with(bset_image)` incrementally (the k-way rebuild was 37 % of the serial create path, `:2271-2280`); disarm restore; clear overlay/heads/mirrors; `frozen = Some(..)`; publish a snapshot with empty overlay.
- **Append outside the lock** (`append_frozen`, `:4045-4146`): read `(frozen, tail)` under a read guard; `tail + frame_len > node_size ⇒ Ok(false)` (compact/split); foreign-append probe on partitioned volumes only; `append_bset(path, layout, dest, records, horizon)`; on success under a write guard `tail_offset = new_tail; frozen = None; release_promise() if projected fits`; `state.end_freeze()`.
- Threshold trigger: `overlay_bytes ≥ writeback_delta_bytes` (`DEFAULT_WRITEBACK_DELTA_BYTES = 4096`, one bset page, `node_cache.rs:124`) enqueues the node on the tree's `maintenance: scc::Queue<u64>` (`tree.rs:1494-1499`, `backend.rs:23480-23487`) and wakes the checkpoint task.
- `maintain_node` (`tree.rs:2082-2120`): freeze → `append_frozen` (re-enqueue if re-dirtied past threshold) → else `smo_replace(ctx, node, out, forced=false)`.
- `oldest_dirty_seq` is `CachedNode::dirty_floor` (`:1456-1461`); `take_dirty_floor` swaps to MAX, `restore_dirty_floor` `fetch_min`s back (`:2050-2063`).

### 2.10 Checkpoint task, cadence, cycle, tail, `reusable_upto`

- **Task** (`checkpoint_task`, `checkpoint.rs:1680-1839`): FIXED cadence deadline `next_tick` (advances only when it fires); `timeout_at(next_tick, wake.notified())`; `cadence = !woke || now ≥ next_tick` (finding 49: the deadline is read off the CLOCK); elastic ceiling (free-grace) may tighten the period; park kick promotes a wake to a tick; FAILED volume idles until shutdown; `drain_deadline = now + period` (unbounded on the shutdown tick); `cadence || shutting_down ⇒ tick(..)` else `maintenance_pass(be, deadline)`; re-arm the wake if work remains or shutdown landed mid-pass.
- **Period**: `checkpoint_tick_period_ms(flush_ms) = flush_ms, 0 ⇒ 100` (`:996-1001`); default flush interval 50 ms.
- **Decision** (`decide_checkpoint`, `:1965-2042`): `due = final || ring_pressure (distance = head − reusable_upto > logical_len/2 || region pressure) || park_kick || dirty_nodes > dirty_node_cap || age_due`; `covers_something = final || dirty > 0 || distance > 0 || regions_uncovered`; `barrier_now = ring_pressure || park_kick || final`. Flat age law: `elapsed ≥ CHECKPOINT_MAX_AGE_MS` since the last cycle END (`flat_age_due`, `:1949-1951`); forest volumes use the term-anticipating trigger (§4).
- **`tick`** (`:2053-2334`): shutdown ⇒ wait every ring's `completed_upto` to a STABLE head (≤ 64 iterations) BEFORE taking the SMO mutex (the lane's hole checkpoints take it) → `smo.lock()` (wait measured as another holder's hold) → decision BEFORE the drain → (1) threshold drain under `DrainBudget` over `maintainable_trees_rotated()`, `JournalReserveExhausted|PendingFreeFull ⇒ checkpoint_cycle(true)` and retry, `NoSpace ⇒ enter_heap_full`, `GrantExhausted ⇒ reactive refill` → (2) deferred-mode barrier if `take_needs_flush()` (`meta_flush_deferred++`, barrier wall sampled) → (3) decide again → cycle with `barrier_now` → shutdown fixpoint: cycle until `head == reusable_upto && !rings_uncovered && !has_dirty_pages`, ≤ `FIXPOINT_COVER_CYCLES_MAX = 16` (`:2256-2332`).
- **`checkpoint_cycle_managed`** (`:2549-3297`), pinned order: `h = head`; `pending_before/tail_before` (audit inputs) → publish forest roots → **collect dirty set** (`for_each_node` with `dirty_floor != MAX && !superseded`), `note_checkpoint_collected` → `order_flush_pass` (oldest floor first; the ring's oldest node `keep_exempt`) → per node `tree.checkpoint_flush_node(smo, addr)` with the grant refill ladder; outcomes: `JournalReserveExhausted` ⇒ defer; `GrantExhausted` ⇒ `deferred_for_grant++`; `NoSpace` ⇒ `deferred_for_space++` (skip-and-defer, floor restored inside); `PendingFreeFull` unreachable (forced retirement) → heap-full posture update → `note_flush_pass(sample)` → write dirty bitmap pages with `ckpt_seq = checkpoint_seq+1` → data-alloc pages, cursor page → **barrier #1** `sync_device()` → `note_checkpoint_cycle_term`, flush-ceiling audit → **tail** (below) → `write_ledger_slot(rec)` (on error restore dying floors) → `checkpoint_seq = ckpt_seq; last_ledger_tail = tail; retire_seq = ckpt_seq+1` → `pending_reclaim.push((tail, barrier_push_epoch()))` → `META_KV_CHECKPOINTS++` → appender pages → `barrier_now ⇒ sync_device()` + the **wedged-tail audit** (§3.5) → ring growth → merge sweep (`run_merge_sweep`, end of cycle, after the barrier).
- **Tail computation** (`:2898-2975`): `tail = min(h, min_inflight_start, dying_floors)`; `min` over unpublished guest-root floors (ring-0 slots); `min` over dying leaf floors; `min` over every live node's `dirty_floor`. All floor inputs are entry starts (FIND-SMO-TAIL §1b: `apply_locked` folds the member's `entry_start`; SMO flips are record 0 so `res.start`; root-swap dying floor is `res.start`). Debug line names which clamp bound the tail.
- **`reusable_upto`** (`after_durable_barrier`, `backend.rs:16172-16230`): DUR-3 epoch law — `pending_reclaim: Vec<(tail, push_epoch)>`; `barrier_starts` ticket taken BEFORE the `fdatasync`, `barrier_durable = fetch_max(ticket)` on success (`:16088-16100`); release the prefix with `epoch < barrier_durable`; for each `alloc.advance_durable(tail); ring.advance_reusable_upto(tail)` (wakes admission parkers); `cache.set_durable_tail(tail)` (elision + torn-tail classifier floor); per-region equivalents. The head bounds against `reusable_upto`, never the logical tail (`journal_core.rs:61-67`).
- **`checkpoint_past(pos)`** (`:16336-16349`): ≤ 8 barriered cycles until `last_ledger_tail ≥ pos`, else `Corrupt` (replay would walk into the hole). `cover_bring_up_residue` (`:16381-16434`): at open, cycle ≤ `COVER_CYCLES_MAX = 64` until `reusable_upto ≥ head` so the volume serves with zero reclaimable residue (closes the D1.b wedge-crumb class).

#### 2.10.1 One checkpoint cycle as a step table (`checkpoint_cycle_managed`)

| Step | What | SMO mutex | Device I/O | Durable-order obligation |
|---|---|---|---|---|
| C0 | `h = head`; `pending_before`, `tail_before` | held (caller) | — | audit inputs captured before any mutation |
| C1 | `publish_forest_roots()` (forest only; `JournalReserveExhausted` ⇒ defer) | held | ring write (reserve) | unpublished roots clamp the tail (C8) |
| C2 | collect dirty set: `for_each_node(dirty_floor != MAX && !superseded)`; `note_checkpoint_collected` | held | — | every leaf dirtied from here is the NEXT cycle's |
| C3 | `order_flush_pass` (oldest floor first; the ring's oldest node keep-exempt) | held | — | the tail-moving node always admits its SMO entry |
| C4 | per node `checkpoint_flush_node`: freeze + take floor (+ keep `overlay_floor`) under the node lock → `append_frozen` or `smo_replace(forced=true)`; grant refill ladder; classify `JournalReserveExhausted` / `GrantExhausted` / `NoSpace` as deferrals (floor restored); `merge_after_flush` for leaves | held + node locks briefly | appends, images, SMO barrier+entry | successor images barriered BEFORE their pointer records |
| C5 | heap-full posture (`deferred_for_space > 0 ⇒ enter`, else `leave` if floor clear); `note_flush_pass(sample)` | held | — | — |
| C6 | `write_dirty_pages(ckpt_seq = checkpoint_seq+1)`; data-alloc pages; cursor page | held | bitmap page writes | pages on the device before the record that names their generation |
| C7 | **barrier #1** `sync_device()`; `note_checkpoint_barrier`; cycle term + tape; flush-ceiling audit | held | fdatasync | appends + pages + completed journal writes + the PREVIOUS ledger record are durable; `after_durable_barrier` drains `pending_reclaim` |
| C8 | `tail = min(h, min_inflight_start, dying_floors, unpublished root floors, dying leaf floors, every live dirty_floor)` | held | — | every input is an entry start |
| C9 | `write_ledger_slot(rec{seq, roots, tail, next_ino, bitmap gen, node_seq_watermark, stamp})` at slot `seq % 32` | held | 4 KiB write | on error: restore dying floors (`restore_dying_floors`), return Err |
| C10 | `checkpoint_seq = ckpt_seq; last_ledger_tail = tail; retire_seq = ckpt_seq + 1; pending_reclaim.push((tail, barrier_push_epoch()))`; `CHECKPOINTS++` | held | — | the record's durability rides the NEXT barrier unless `barrier_now` |
| C11 | appender pages (forest) | held | page writes | — |
| C12 | `barrier_now ⇒ sync_device()` + **wedged-tail audit** (§3.5) | held | fdatasync | `reusable_upto`, pending-free releases and the cache's durable tail advance before return |
| C13 | ring growth (forest); **merge sweep** (`run_merge_sweep`) while `heap_full || merge_backlog` | held | images, entries | the sweep reads the SAME tail a post-cycle census reads |

The progress theorem the task rests on (`checkpoint.rs:99-109`): a minimal cycle consumes **zero ring bytes** (appends → node extents, ledger/bitmap → their own extents); only SMO records draw the reserve and they never park (`JournalReserveExhausted` ⇒ skip-and-defer). So the drain always completes, and nothing ever blocks on ring space while holding a node lock.

### 2.11 The SMO task (`smo_replace`) and two-phase replay

Serialized by `&mut SmoContext` behind `KvMetaBackend::smo: SqzMutex<SmoContext>` (`backend.rs:1493`); drivers: the checkpoint task, `checkpoint_now`, the merge sweep, the defrag nudge, hole checkpoints.

`smo_replace(ctx, node, out, forced_retirement)` (`tree.rs:2177-2804`):

1. **Build, no locks** (`:2192-2456`): `load_node(path, layout, addr, durable_tail)`; `fold_source_extra` (frozen delta; FIND-VS-A fold-source guards); `compact(views, durable_tail)` with THE K1 algebra; `total ≤ fold_capacity ⇒ 1 part` else `partition_records(folded, split_part_capacity)`; `check_grant_covers(need)`; claim extents (`claim_internal`, internal class) + write images (`compact_node` / `split_node` / `write_node` per part, gap-free bounds via `key_successor`); load back successors (`pinned = level>0 || root&&1 part`); test build-pause seam; new root for a multi-way root replacement; **any error releases every claimed extent** (`release_unpublished`).
2. **Pre-lock journaling** (`:2477-2535`): `j.barrier()` (successor images durable BEFORE any pointer record can exist); FIFO headroom check unless forced (`PendingFreeFull` pre-swap); records `[Put(succ.max → (addr, seq)) per successor (skipped for a root swap), alloc(e) per claimed, free(old, retire_tag)]`; `admit_smo_entry(len)` from the checkpoint class (`keep_exempt ? try_admit(Checkpoint) : try_admit_checkpoint_keeping`; over-keep counted `META_KV_SMO_ENTRIES_OVER_KEEP`, `:402-425`); refusal ⇒ release extents, `JournalReserveExhausted`.
3. **Lock window** (`:2537-2733`): `parent = resolve_parent(node)` (None for root swap); parent write guard then child write guard; **`reserve_registered(adm)` inside the window**, seqs `seq_base + i` stamped; `leftovers = child_guard.take_overlay()`; `node.state().supersede()`; partition leftovers into successors with **exact per-record `entry_floor` stamps** (`floor = min(entry_floor.min(seq))`), re-promise a successor whose inherited delta overflows; **publish successors (and new root) FIRST, then the route flip, then retire LAST** (`:2638-2665`): root swap ⇒ `note_root_swap_floor(res.start)` dying floor + `root.store`; else `parent.apply_locked_structural(pg, flips, flip_floor = recs[0].seq)`; `cache.retire(node)`; drop guards; `node.state().end_freeze()`.
4. **After release** (`:2735-2803`): `gate = res.record_gate(last)`; `ring.commit_entry(&res, &recs)` — a failure is the §4.4 pt 4 crash-equivalent hole, loud not fatal; `ctx.free_pending(old_extent, gate, forced)` (forced ⇒ unbounded overflow); re-enqueue parent if over threshold; count `NODE_COMPACTIONS` (1 part) or `NODE_SPLITS`.

**Pointer record vocabulary**: split/compaction = `Put(succ.max_key → (addr, node_seq))` per part (the old separator is replaced LWW at the same key for the rightmost part); merge = `Put(R.max → succ) + Delete(L.max)` (`design-cow-kv-metadata.md:280`). Root swaps journal no pointer record; the next ledger names the root; dying floor at `res.start`.

**Writer retry**: `apply_at_seq_class` (`tree.rs:1457-1463`) and the pass (`backend.rs:23205-23219`) revalidate `!superseded && min ≤ key ≤ max`; `Stale ⇒ META_KV_COMMIT_SMO_RETRIES++` and re-resolve. `descend` (`tree.rs:1215-1363`) restarts on root-retired / root-seq / routing-hole / child-retired / child-seq (tallies carried into the exhaustion error), yields between attempts, `RETRY_BUDGET = 256`; a `level < target_level` step restarts (root collapse under the walk).

**Two-phase replay** (`backend.rs:2700-2850`): phase 1 interior records ordered `(level DESC, seq)` via `apply_replayed_interior` (raising the node-seq mint above every child seq named); phase 2 content records at original seqs via `apply_replayed` (per-key LWW gate: skip if `newest_seq_of(key) ≥ seq`, `tree.rs:1465-1483`); every ino MENTIONED raises `max_replayed_ino` (DUR-8c). Replayed in-window frees park until the first post-mount durable checkpoint except frees of a mounted root (`park_replayed_frees`, `META_KV_REPLAY_ROOT_FREES_DROPPED`).

### 2.12 Leaf merge (§4.6a), interior recursion, root collapse

- **Law**: `C = NodeLayout::fold_capacity()`; `f(N) = NodeSnapshot::fold_bytes_upper_with(&[], merge_pair_capacity, durable_tail).0` (tombstones below the tail credited as elided, `tree.rs:2976-2987`); mergeable iff `f(L)+f(R) ≤ ¾C (merge_pair_capacity = split_part_capacity) ∧ min(f(L),f(R)) ≤ ¼C (merge_candidate_capacity = ¾C − ½C)` (`pair_mergeable`, `:2991-2994`; `node.rs:395-409`). Hysteresis is structural: a fresh split half holds > ½C and never pairs.
- **Candidate predicate** `is_merge_candidate_at(node, tail)` = `!superseded && owns && !root && f ≤ ¼C` — ONE source of truth for the sweep, the count phase, the D4 census and the bench (`:3001-3015`).
- **Triggers**: (1) **flush-pass prefilter** `merge_after_flush` (`:1990-2023`): after a leaf's visit, O(1) bound `tail_offset − NODE_PAGE + frozen_bytes + overlay_bytes ≤ ¼C` proves `f ≤ ¼C`; yields under a user space retry (`note_merge_yield`); `NoSpace|JournalReserveExhausted` ⇒ deferral; (2) **heap-full sweep** `run_merge_sweep` → `merge_underfull(ctx, forced, deadline)` (`:3088-3233`): walks RESIDENT nodes level by level leaves-up in key order, greedy left-to-right, climbs after each merge, root-collapse chain, then a COUNT phase publishing the exact underfull population; bounded by `merge_sweep_budget_ms = checkpoint_tick_period_ms` (`checkpoint.rs:1015-1017`), cursor `SweepCursor{phase: Merge(level)|Count, next_min, lap_candidates, space_refused}` (`tree.rs:604-633`) resumed next cycle; runs while `heap_full || merge_backlog`; `NoSpace` stops merging, counting continues; `SweepRefusal::{JournalReserve, PendingFreeFull}` parks the cursor at the node; (3) **D4 defrag arm** `defrag_merge_sweep` — the same walk in `merge_sweep_budget_ms` chunks with duty parks.
- **Protocol** (`smo_merge`, `:3381`): freeze both under their own locks (`begin_forced_freeze` for an empty delta via `ForcedFreezeGuard`, `:642-653`), fold both logs + frozen deltas, re-check the pair law on the exact fold, claim ONE extent at the **compaction floor**, write successor `[L.min, R.max]`, barrier, parent-then-both-children ascending, reserve from the reserve, `take_overlay` both, supersede both, publish successor, `Put(R.max→succ)+Delete(L.max)` in ONE `apply_locked_structural`, retire both last; entry `[Put, Delete, alloc, free(L), free(R)]`; both retirements park gated on `free(R).seq`.
- **Interior recursion**: `smo_merge` is level-agnostic; the parent of a merged pair that projects `≤ ¼C` is itself a candidate at its level. **Root collapse** (`smo_root_collapse`, `:3708`): a root with exactly one live separator makes the child the root (`RootPtr = (child.addr, child.node_seq)`; child must span `["", KEY_SPACE_MAX]`), entry `[free(old_root)]`, dying floor at `res.start`, replays through the OLD structure under the coverage gate. The ONLY way height decreases. Counters `META_KV_NODE_MERGES`, `META_KV_INTERIOR_MERGES`, `META_KV_ROOT_COLLAPSES`.
- **Space arithmetic**: merge = −1 extent net, admitted at `compaction_floor = reserve/2`; extents return 1–2 barriered cycles later; recovery is a geometric wave (measured trace `8 → 16 → 32 → 64 → 128` merges/wave, 313 in 14 sweeps, free 11 → 325 on a 24 MiB/64 KiB volume; `design-cow-kv-metadata.md:286`). Convergence bound `passes ≤ 3·h₀ + 1`; full heap `sweeps ≤ 3·h₀ + 1 + 2·waves + 2`, `waves ≤ ⌈log₂(candidates/room₀ + 1)⌉ + 1` (`:288`).

### 2.13 The ENOSPC / promise law and pending-free coverage gating

- **Reserve** `compaction_reserve_extents(total) = max(8, total/50)` (2 %) (`alloc_ext.rs:156-158`); **compaction floor** `= reserve/2` (`:169`). Internal-class claims (SMO successors) are admitted down to the floor; user growth never claims directly.
- **Commit-time heap admission** (`admit_heap_locked`, `backend.rs:23978-24210`), under the union locks before the reservation: FAST PATH — Σ batch bytes per leaf; `projected_log_end(layout, total) ≤ node_size` for every leaf ⇒ nothing to promise (stack buffer of 32, no allocation). Else per member per leaf: in-place fits ⇒ skip; already promised and within the growth window (`promised == 1 ⇒ basis+added ≤ fold_capacity`; split ⇒ `added < split_growth_window`) ⇒ `note_promise_growth`; else exact walk `fold_bytes_upper_with(pending records, split_part_capacity, tail 0)`, `need = smo_extents_for_parts(fold, parts) (+1 for a root leaf's split)`, `delta = need − promised`; leased slot ⇒ grant ledger; else `floor = is_split ? reserve : compaction_floor`, `claimable = free − heap_promised`; `claimable − delta ≥ floor ⇒ promise(delta, fold)` else `NoSpace` — the member's earlier promises are retracted (`retract_promise`, Issue 17). An admitted split clears the heap-full latch.
- Promises are consumed by the SMO (`take_overlay` ⇒ `release_promise`, `node_cache.rs:1257-1268`), released when an append leaves the remainder fitting (`:4135-4137`), Drop-owned (`:1409-1420`). Ledger `NodeCache::heap_promised` is 0 at quiesce.
- **Refusal path**: one retry behind ≤ 2 `checkpoint_now()` cycles with `begin_space_retry` (sweeps yield) when returnable budget exists; a surviving refusal fans out `NoSpace` ⇒ `ENOSPC` (`KvError → SqueezefsError::no_space`, `mod.rs:1319`), `enospc_refusals++`, `enter_heap_full`.
- **Pending-free gate** (Option A): `free_pending(extent, gate_seq = free record's own position-domain seq)`; released only when a **durable tail passes the seq** (`advance_durable(tail)` from `after_durable_barrier`), never on ledger durability alone. Cap `PENDING_FREE_CAP = 65_536` is a pressure valve: threshold SMOs check headroom pre-swap (`pending_has_room_for(1)`) and refuse `PendingFreeFull` (the maintenance arms force a cycle); the flush pass's SMOs run `forced_retirement = true` and park in the unbounded overflow (`free_pending_forced`) — the §4.7 cycle-break whose progress theorem is `tree.rs:2158-2176` (any parked retirement drains within TWO barriered cycles).
- **Wedged-tail audit** (`checkpoint.rs:3119-3182`): per barriered cycle, progress = `pending_after == 0 || pending_after < pending_before || tail > tail_before` ⇒ rung reset; `deferred_for_grant > 0` ⇒ manager dependency; `deferred_for_space > 0` ⇒ SPACE standstill (`heap_full_cycles++`, `heap_full` latched, never terminal); else rung++ and at `PENDING_FREE_FORCE_CYCLES = 8` ⇒ `fail_stop_loud` (FAILED). The two classes are the charter's "counted non-terminal space class" vs "the wedge class alone".

### 2.14 Monotonic inos

`next_ino: AtomicU64` (`backend.rs:1247`), `fetch_add` on create; recovered at open as `ledger.next_ino.max(max_replayed_ino + 1)` (`:2935-2936`) where `max_replayed_ino` is the max ino **mentioned** by any replayed inode key, dentry value `child_ino`, or xattr key (`:2816-2839`); guest slots keep per-slot maxima. Published in every ledger record (`checkpoint.rs:2999`). `destroyed_inodes` turns the watermark into a live-population gauge for `statfs`. `ino_lane.rs` generalizes recovery to per-writer lanes (`recover_ino_floor`, `:80-88`) — built, not stamped. No reuse ⇒ `destroy_inodes` is one batched entry of inode `Delete`s + per-ino xattr `Delete`s with no free ordering rule (`backend.rs:24741-24751`).

### 2.15 fsync and strict mode

`sync_device` (`backend.rs:16071-16109`): `SyncCoalescer::barrier_bounded(threshold, fdatasync)` — the trailing coalesced barrier (leader runs, followers ride); `meta_device_syncs++`; DUR-3 tickets; `note_barrier_success/failure`; `after_durable_barrier()`. Strict mode = `SQUEEZEFS_META_FLUSH_INTERVAL_MS=0` ⇒ `strict = true` (`:1320`): the lane barriers once per group (§2.3 step 13); the checkpoint task still ticks at 100 ms for reclamation/SMOs. Deferred mode: the lane sets `needs_flush`, the tick barriers.

### 2.16 Journal economy

- **Rename = one entry**: `routed_rename_local` (`:25492`) stages dentry surgery + both parents' Δtime merge records + the moved inode's Δctime in one `KvTx` (D4.b; closing report rename 1.003 entries/op).
- **SETATTR echo absorption** (`setattr_locked`, `:27727-27766`): a times-only `FUSE_SETATTR` whose `mtime` is absent-or-equal to the folded stored value and carries a `ctime` is recognized by shape, `META_KV_TIMES_ECHO_ABSORBED++`, and parked in `pending_times: scc::HashMap<Ino, (mtime, ctime)>` by signed monotone max (`park_times_refinement`, `:16476-16491`) with zero journal entries; every inode read folds the map (`fold_pending_times`, `:16449-16465`); a dedicated drain task (`spawn_times_drain_task`, `checkpoint.rs:1621-1678`) commits batches of ≤ 128 inos per `lock_many` set, waking early at 512 pending (`PENDING_TIMES_DRAIN_BATCH/_CAP`) — a separate task because its `commit_tx` may park on admission (parking the checkpoint task would deadlock the drain).
- **Batched `destroy_inodes`** (`:24748`) one entry per batch; the FUSE reclaim gather fixes brought unlink to 1.018 entries/op (gate ≤ 1.05).
- **Parent-time updates** are `Delta` merge records under the SHARED parent guard (`RoutedParentUpdate::SharedTimes`, `:19882-19893`).
- **Fold-head memo** (§2.8) and the per-key materialized head in the dirty delta (`FoldedHead` on `OwnedRec`) — `META_KV_FOLD_HEAD_SERVES` / `FOLD_MEMO_HITS/MISSES`.

### 2.17 Lock order — the rung-2 wait-for table

Charter §2 levels (4a)/(4b)/(4c) plus the SMO mutex, as the reference composes them (`backend.rs:97-123`, `stripe_locks.rs:70-130`, `checkpoint.rs:79-98`):

| Holder | Holds | May wait on | Never waits on |
|---|---|---|---|
| committer (user op) | 4a guards (acquired by `lock_many`, canonical) | the conveyor oneshot | node locks, ring space (it never admits), the SMO mutex |
| conveyor pass (stage A) | 4a guards (HELD, never acquired); union leaf write locks (4b) | ring admission (holding NO node lock); leaf locks ascending (held by A itself, B's rollback arm, or a checkpoint freeze — all RAM-only) | device I/O under a node lock; 4a acquisition; the SMO mutex |
| durability lane (stage B) | window reservations; in the rollback arm leaf locks ascending | uring completions; `completed_upto` (advanced by B itself and by the SMO task's `commit_entry`s); the SMO mutex for hole checkpoints | — (no node-lock holder ever waits on B) |
| checkpoint/SMO task | SMO mutex; parent-then-child write locks briefly; checkpoint-class admissions (never park) | leaf/interior locks (RAM-only holders); device I/O **outside** node locks; the journal device for barriers (process pool, never the lane) | ring admission as a park; 4a; the lane |
| times drain task | its own 4a `lock_many` set | the conveyor oneshot (it is a committer) | the SMO mutex (hence a separate task from the checkpoint task) |
| clock eviction | one node's lifecycle CAS | nothing | — |

Edges: committer → pass (oneshot); pass → ring (admission, holding nothing) → checkpoint task (`advance_reusable_upto`) → device; lane → device, lane → checkpoint task (hole checkpoints); checkpoint task → device. No edge points back at the lane or at a parked committer, so the graph is acyclic. The only liveness obligation left is that the checkpoint task RUNS its cycle — finding 49's two laws (clock-read deadline, period-bounded drain, `checkpoint.rs:53-77`).

### 2.18 Knobs these modules read (charter §1 item 6: registered, admissible range, refuse out-of-range)

| Knob | Read where | Default / derivation | Class |
|---|---|---|---|
| `SQUEEZEFS_META_FLUSH_INTERVAL_MS` | `resolve_flush_interval_ms` → `strict`, tick period | 50 ms; `0` = strict | measurement lever |
| `SQUEEZEFS_META_COMMIT_BATCH_TXS` / `_BYTES` | `resolve_commit_batch_txs/bytes` at open (`backend.rs:668-685`) | `max(64, cpus×2)` / `max(256 KiB, ring/16)` | A/B lever |
| `SQUEEZEFS_META_NODE_CACHE_MB` / `_PCT` | `resolve_node_cache_budget` (`:1843`) | `max(budget/16, 512 MiB)` | sizing |
| `SQUEEZEFS_META_CHECKPOINT_MAX_DIRTY_NODES` | `resolve_max_dirty_nodes` at open (`checkpoint.rs:969`) | `max(4096, budget/32 ÷ node_size)` | sizing |
| `SQUEEZEFS_DLM_STRIPES` | `dlm_stripe_width` once per process (`stripe_locks.rs:450`) | `next_pow2(max(4096, 16×cpus×q_depth))` | sizing |
| `SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH` | the width law's `q_depth` input (`:441-445`) | `Q_DEPTH_DESIRED` | transport |
| `SQUEEZEFS_TIMEOUT` | `timeout_threshold` at open (`backend.rs:640-645`) | 30 s | watchdog/escalation threshold |
| `SQUEEZEFS_JOURNAL_LANE` | `journal_lane()` (`:22286`) | on | A/B lever (same-binary control) |
| `SQUEEZEFS_META_REVALIDATE_MS` | `resolve_revalidate_interval_ms` (`revalidate.rs:90`) | `max(tick, 1000 ms)` | reader cadence (rung 6) |
| `SQUEEZEFS_TEST_*` seams | `TEST_*` statics listed in §6 | off | test seams |

### 2.19 Typed error classes crossing these modules (`KvError`, `mod.rs:942-1194`)

The Zig port's error unions should reproduce these as named classes with their errno in one table (`mod.rs:1310-1370`):

| Class | Retryable? | Who raises | Errno / disposition |
|---|---|---|---|
| `NoSpace{free, reserve}` | after one cycle retry: terminal | heap admission, `claim_internal` | `ENOSPC` |
| `JournalReserveExhausted{needed}` | yes — minimal drain + retry | `admit_smo_entry` | internal; never reaches userspace |
| `PendingFreeFull{pending}` | yes — forced cycle + retry | threshold SMO admission | internal |
| `FreezeRefused{node, refusal}` | protocol state | `freeze_locked` | typed apart from `Corrupt` (2026-08-19 field hunt) |
| `NodeFull{needed, available}` | signal | `append_bset` | downgraded to `Ok(false)` ⇒ compact/split |
| `EntryTooLarge` / `ValueTooLarge` / `NameTooLong` / `DentryChainOverflow` | no — the op fails alone | pre-queue admission | `E2BIG`-class / `EINVAL`-class clean refusals |
| `CheckpointCoveredBsetAfterTear` | no — corruption | node load | loud mount/load failure |
| `GrantExhausted` / `SlotBusy` / `GrantDeferred` / `LeaseDeferred` / `HandoverDeferred` / `WireDeferred` | yes | symmetric plane | `EAGAIN` (rung 6) |
| `Busy` / `ManagerUnreachable` / `RotorAtCap` / `Rejected` | no | D0 guard, manager verbs | `EBUSY` / `EHOSTUNREACH` / `EBUSY` / `EINVAL` |
| `Io(SqueezefsError)` | terminal per lattice | device | passes through |
| `Corrupt(msg)` | no | everywhere | `InvalidOperation` class; mount gates |

`Clone` is exhaustive on purpose so the conveyor can answer one pass failure to every member with its own class (`:1196-1302`) — the retired lossy clone turned a joiner's `GrantExhausted` into `mkdir → EINVAL`.

### 2.20 Test seams the port must reproduce (the exact windows the contracts pin)

| Seam | Holds open | Used by |
|---|---|---|
| `TEST_SMO_BUILD_PAUSE_TREE` / `_SLOT` / `TEST_SMO_BUILD_PAUSED` / `test_smo_build_pause_release` (`tree.rs:78-127`) | `smo_replace` between successor load-back and the lock window (build-window stranding) | `stranded_build_window_commits_survive_crash`, merge crash windows |
| `TEST_CONVEYOR_HOLD_STAGE ∈ {PRE_DRAIN, PRE_FANOUT, EMPTY_DRAIN_TAIL, PRE_ROLLBACK, POST_RECALL, POST_DOOR}` (`backend.rs:335-425`) | the pass before drain / the lane before fan-out / the empty-drain tail with the backend held / stage B between reservation completion and verdicts / post-recall pre-apply / post-door pre-enqueue | `group_forms_under_held_pass`, cancel-stage cases, Issue-23/11 schedules |
| `TEST_CONVEYOR_POISON_APPLY_INO` (`:428`) | fails ONE member's apply after it applied | `poisoned_tx_fails_alone_batch_survives` |
| `TEST_COMMIT_ADMITTED_STALL_MS` (`:249`) | admission held, nothing reserved | D1.b no-drop / watchdog suite |
| `TEST_CHECKPOINT_HALT_BEFORE_LEDGER` / `_AFTER_LEDGER` / `TEST_CHECKPOINT_PARK_BEFORE_PAGES` (`checkpoint.rs:2340-2380`) | the cycle before/after its record, before its pages | crash-window pins (ordering law) |
| `TEST_FREEZE_ENCODE_FAIL` (`node_cache.rs:139`) | fail N freezes right after `begin_freeze` | `an_encode_failure_inside_freeze_leaves_the_node_freezable` |
| `TEST_PENDING_FREE_CAP` (`backend.rs:437`) | a tiny cap | at-cap forced-cycle contracts |
| `TEST_SHUTDOWN_FIXPOINT_CYCLES` (`:493`) | bound the shutdown fixpoint | the ring-0-idle defect's pin |
| `TEST_BRING_UP_COVER_DISABLED` (`:457`) | skip the bring-up cover | wedge-crumb pins |
| `uring_fs::{TORN_WRITE_FAULT, arm_power_cut, power_cut, arm_barrier_error, arm_device_latency}` | torn units, volatile-cache loss, barrier errno, slow device | every crash-contract case; the two-stage saturation rows |

Every seam is one relaxed load when unarmed and uses register-recheck-await on a `Notify` (never a sleep) when parked.

---

## 3. STATE MACHINES (transition tables)

### 3.1 `conveyor_core` — leadership + queue (`conveyor_core.rs`)

State = `(leader: bool, queue: FIFO)`.

| From | Event / op | Guard | To | Notes |
|---|---|---|---|---|
| any | `enqueue(item, len)` / `enqueue_many` | — | queue += items (atomic under the mutex); gauge += n | A drain never sees part of a group. |
| `leader=false` | `try_lead()` | CAS false→true SeqCst succeeds | `leader=true`, caller MUST spawn a pass | Exactly one winner. |
| `leader=true` | `try_lead()` | CAS fails | unchanged, caller parks | The live leader owns the queue (no-lost-wakeup theorem `:26-37`). |
| `leader=true` | `drain(max_txs, max_bytes)` | — | queue −= prefix: first entry always, then while `n < max(max_txs,1)` and `bytes + len ≤ max_bytes` | FIFO; gauge −= n. |
| `leader=true` | `unlead_and_recheck()` | queue empty after the SeqCst store | `leader=false`, return `false` (exit) | Release THEN recheck. |
| `leader=true` | `unlead_and_recheck()` | queue non-empty and re-CAS wins | `leader=true`, return `true` (run another pass) | |
| `leader=true` | `unlead_and_recheck()` | queue non-empty, re-CAS loses | `leader=false` for this task; `true` for the racer | Racer owns the drain. |

Model invariants (`:51-74`): leader uniqueness; no lost wakeups; FIFO; budget conservation across committer drops; guard-lifetime ≥ staged-record lifetime; group atomicity. Weakening SeqCst on `try_lead`/`unlead` breaks invariant 2.

### 3.2 `epoch_core` — revalidation pair and append gate

`RevalidationEpoch {epoch, tail}`:

| From | Op | To | Ordering |
|---|---|---|---|
| `epoch = UNARMED(0)` | `arm(e≠0)` CAS 0→e | armed at `e` | Release |
| armed `cur` | `publish(tail, e > cur)` | `tail = max(tail)` THEN `epoch = e` (CAS loop; an observed `≥ e` aborts) | AcqRel then Release — **tail before epoch** |
| any | `advance_tail(t)` | `tail = max` | AcqRel |
| any | `load_snapshot()` | reads **epoch then tail** | Acquire, Acquire ⇒ `snapshot.tail ≥ tail(snapshot.epoch)` |
| any | `probe()` | epoch | Relaxed (a mismatch is a miss; the miss path re-reads Acquire) |

`AppendGate` word: `set_reader` (`fetch_or` bit 31), `set_appender(writers, id)` CAS loop preserving the reader bit; `load` one Relaxed load. Node consequences at the choke point: reader ⇒ refuse every mutation; `level>0 && !authority` ⇒ refuse (`node_cache.rs:1757-1773`).

### 3.3 `node_state_core` — node lifecycle word

Bits `DIRTY=1, FREEZING=2, SUPERSEDED=4`; decoded `Clean(0) → Dirty → Serializing(FREEZING±DIRTY) → Superseded(terminal)`.

| From word | Op | Result / to | Caller |
|---|---|---|---|
| any without SUPERSEDED | `mark_dirty()` | `w | DIRTY`, Ok | `apply_locked` (under the node write lock) |
| SUPERSEDED set | `mark_dirty()` | Err(Superseded) — nothing accepted; re-resolve | writer revalidation |
| DIRTY, !FREEZING, !SUPERSEDED | `begin_freeze()` | `(w & !DIRTY) | FREEZING` | `freeze_locked` (SMO task, under lock) |
| !DIRTY | `begin_freeze()` | Err(NotDirty) | |
| FREEZING | `begin_freeze()` | Err(AlreadyFreezing) | one freeze at a time |
| SUPERSEDED | `begin_freeze()` | Err(Superseded) | |
| !FREEZING, !SUPERSEDED (any DIRTY) | `begin_forced_freeze()` | `(w & !DIRTY) | FREEZING` with an EMPTY frozen delta | `freeze_for_smo` (merge of a clean sibling, defrag nudge) |
| FREEZING | `end_freeze()` | `w & !FREEZING`; returns `DIRTY` was set (re-enqueue signal) | after append / after SMO swap; legal on SUPERSEDED |
| FREEZING | `abort_freeze()` | `(w & !FREEZING) | DIRTY` | W-A restore, RES-22 self-heal |
| !SUPERSEDED | `supersede()` | `w | SUPERSEDED`; returns `{was_dirty, was_freezing}` | SMO swap / publish-replace |
| SUPERSEDED | `supersede()` | Err — double swap = protocol bug | |
| exactly `0` (Clean) | `try_evict()` | CAS 0→SUPERSEDED true | clock sweep |
| anything else | `try_evict()` | false (pinned: dirty/serializing) | |

Usage composition (cache layer): `apply_locked` = `mark_dirty` + floor + overlay insert + snapshot swap; `freeze_locked` = `begin_freeze` + shadow-fold + base extend + overlay clear + snapshot swap; `append_frozen` = I/O then `end_freeze`; `smo_replace` = `take_overlay` + `supersede` + publish + flip + `retire` + `end_freeze`. Loom models `node_state_*` (`loom-models/src/lib.rs:2058-2224`).

### 3.4 SMO phases (`smo_replace` / `smo_merge`)

| Phase | Locks held | Durable-order obligation | On error |
|---|---|---|---|
| P0 freeze source(s) | node write lock (briefly) | — | `FreezeRefused` typed; `ForcedFreezeGuard` ends a forced freeze on drop |
| P1 build images | none | successor images written to FRESH extents | release every claimed extent (`release_unpublished`); `NoSpace`/`GrantExhausted` surface |
| P2 barrier + admit | none | images durable BEFORE any pointer record exists | `PendingFreeFull` (threshold posture) / `JournalReserveExhausted` ⇒ release extents, caller cycles and retries |
| P3 lock window | parent then child(ren) ascending addr | `reserve_registered` inside; publish → flip → retire order; dying floor for root swaps | `Corrupt` on double supersede |
| P4 write entry | none | `commit_entry` after release; a failed write = crash-equivalent hole (loud, not fatal) | `free_pending` still parks (forced ⇒ overflow) |
| P5 park old extent | none | gate = free record's seq; released by `advance_durable(tail > seq)` | non-forced cap refusal unreachable (headroom checked in P2) |

Test seam: `TEST_SMO_BUILD_PAUSE_TREE` parks between P1 load-back and P3 (`tree.rs:78-127`).

### 3.5 Volume FAILED latch and the fail-stop lattice (`backend.rs`)

| Rung / input | Counter | Transition |
|---|---|---|
| entry-write failure (lane) | `journal_failures++` (reset to 0 on any write success) | `≥ JOURNAL_FAILURE_LATCH (3)` ⇒ `failed = true` (`:22312-22321`) |
| hole checkpoint cannot drain | `note_journal_failure()` | same lattice |
| ring-admission park ≥ `SQUEEZEFS_TIMEOUT` | `note_journal_failure()` per crossing | same lattice (D1.b row 2) |
| pass/lane panic | `META_CONVEYOR_PASS_PANICS++`, `note_journal_failure()`; `applied_unrolled` ⇒ immediate `failed` | |
| barrier failure, generic | `barrier_failures++` (reset on barrier success; separate counter because `journal_failures` resets per write) | `≥ 3` ⇒ `failed` (`:16138-16157`) |
| barrier failure, reservation-conflict errno | `guard_fenced++` | immediate `failed` (`guard_fail_stop`) |
| wedged-tail audit | `pending_free_stalled_cycles` reaches `PENDING_FREE_FORCE_CYCLES (8)` | `fail_stop_loud` ⇒ `failed` |
| `failed = true` | — | mutations EIO (`write_gate`), checkpoint ticks idle until shutdown (`checkpoint.rs:1765-1767`), parked passes bail (`:23911-23915`) |
| `heap_full` (separate posture word) | `heap_full_cycles`, `enospc_refusals` | set by a refused growth commit or a `NoSpace` deferral; cleared by a pass that deferred nothing with `heap_growth_floor_clear()`; **never terminal** |

### 3.6 Ring-admission park (`admit_user_budget`)

| State | Op | To |
|---|---|---|
| trying | `try_admit(len, User)` Ok | admitted (return `(adm, park_pass)`) |
| trying | refused | `stalls++`; register `space_notified()`; re-try (recheck) |
| registered | re-try Ok | admitted |
| registered | shutting_down ‖ failed | Err EIO (queued txs fail out) |
| registered | — | `kick_checkpoint_for_ring_park()`; `parked_since.get_or_insert(now)`; `timeout(min(threshold − elapsed, ≥10 ms), notified)` |
| parked | notified / timeout | if `elapsed ≥ threshold`: `log::error`, `note_journal_failure()`, reset `parked_since`; flags re-check; goto trying |

Woken by `advance_reusable_upto` (`space_notify.notify_waiters`) from `after_durable_barrier`, or by `wake_parked` at shutdown.

### 3.7 Writer resolve → lock → revalidate → retry (single record, `tree.rs:1449-1501`; batch, §2.2.1 A3–A5)

| State | Condition under the leaf write lock | To |
|---|---|---|
| resolved (latch-free `descend`) | `superseded ‖ key < min ‖ key > max` | drop lock, `COMMIT_SMO_RETRIES++`, `Stale` ⇒ re-resolve (budget 256) |
| resolved | valid, replay mode and `newest_seq_of(key) ≥ seq` | `Applied` (idempotent skip — already folded in) |
| resolved | valid, live mode | `seq = next()`; `apply_locked(recs, floor = seq)`; over threshold ⇒ `maintenance.push(addr)`; `Applied` |

`descend` restart reasons (`:1223`, tallies `[root-retired, root-seq, routing-hole, child-retired, child-seq]`): each is a racing SMO's swap window; the walk yields between attempts; projection trees refresh the root every `PROJECTION_REFRESH_EVERY = 8` restarts.

### 3.8 Clock eviction decision (`evict_to_budget`)

| Popped node | Decision |
|---|---|
| not in map (stale clock entry) | skip |
| pinned (interior / root) | skip, never re-enter |
| `ref_bit.swap(false) == true` | second chance: re-push |
| `Arc::strong_count > 2` (held by a resolver/traversal/walk) | re-push (evicting would free nothing and cause reload-thrash) |
| `state.try_evict()` succeeds (word was exactly Clean) | `note_node_dying_floor`; `remove_if_sync(ptr_eq)`; `cached_bytes −= node_size`; `EVICTIONS++` |
| CAS fails (dirty/serializing) | re-push (pinned until writeback) |

Loop bound: `2 × (map.len() + 1)` attempts, so an all-pinned cache cannot spin; runs inline at every `publish_stamped`.

### 3.9 Checkpoint task tick (`checkpoint_task`)

| State | Event | Action | Next |
|---|---|---|---|
| parked | deadline fires | `cadence = true` | tick |
| parked | wake (threshold) | `cadence = now ≥ next_tick` | tick if cadence else maintenance_pass |
| parked | wake with `ring_park_kick` | promote to cadence; `due` + `barrier_now` | tick |
| any | `is_failed && !shutting_down` | skip | parked |
| tick | decision `runs_a_cycle()` | `checkpoint_cycle(barrier_now)` | re-arm if work pending |
| tick | not due | drain within budget, deferred barrier, re-decide | |
| shutdown | flag | unbounded drain, final cycles to the fixpoint, return | task exits |

---

## 4. DERIVED CONSTANTS and MEASURED NUMBERS

### 4.1 Derivations (formula → why → anchor)

| Constant | Formula | Why | Anchor |
|---|---|---|---|
| Conveyor tx cap | `env ≥1 verbatim else max(64, cpus×2)` | arrivals scale with handler parallelism (queues = possible CPUs); 64 = shipped M7 floor (32-CPU box derives exactly 64) | `backend.rs:668-672` |
| Conveyor byte cap | `env ≥1 else max(256 KiB, ring_user_capacity/16)`, clamped to admissible capacity | ≥ 16 batch reservations always cycle (liveness margin); floor = shipped M7; clamp so Σ-admission can succeed | `:681-685`, `:1464-1472` |
| Checkpoint reserve | `max(256 KiB, ring/64)` | SMO records ~50–100 B each ⇒ thousands per cycle; ring-sized so the carve-out never dominates a small ring | `journal.rs:230-232` |
| SMO keep | `max(reserve/4, page_data_len) + max_pad` | the widest SMO entry the tail-pinning node can need; `parts ≤ ⌈(fold_capacity + ring_len)/split_part_capacity⌉` | `journal_core.rs:170-194` |
| Max pad | `grain ≤ 1 ? 0 : PAD_MIN(21) + grain` | a reservation ends at most `PAD_MIN` short of a boundary + one grain | `journal_core.rs:159-168` |
| 4a stripe width | `env else next_pow2(max(4096, 16 × possible_cpus × q_depth))` | a false-sharing wait ≤ 1 acquire in 16 at full delivered concurrency; 4096 = shipped floor | `dlm.rs:14-17`, `stripe_locks.rs:450-460` |
| Node-cache budget | `MB×MiB else PCT×budget else max(budget/16, 512 MiB)` | R5 memory budget share; 512 MiB never-regress floor | `backend.rs:1843-1877` |
| Dirty-node cap | `env else max(4096, budget/32 ÷ node_size)` | bounds RAM pinned by dirty nodes AND the mount-replay working set; 4096 = shipped floor | `checkpoint.rs:969-982` |
| Checkpoint max age | `CHECKPOINT_MAX_AGE_MS = 1000` | §4.6 pt 2 "every ≤ 1 s"; also the reader's staleness term | `:989` |
| Tick period | `flush_ms; 0 ⇒ 100` | strict mode still needs reclamation/SMO service | `:996-1001` |
| Merge sweep budget | `= checkpoint_tick_period_ms` | finding 49's drain law (one period per pass, leftovers re-arm) | `:1015-1017` |
| Landing ceiling | `max_age + 2×tick` | the tick wait + one period of pre-decision work | `:1038-1040` |
| Cadence trigger (forest) | `max_age − anticipated_term` | land inside the ceiling on a stationary term | `:1073-1075` |
| Cycle term | `prebarrier_wall + (min(late, cap) − tick)⁺` | lateness beyond one tick is the tick's own device work | `:1107-1114` |
| Flush unit | `wall/count` per class, horizon MAX over 64 cycles | a bound must bound; mean under-prices | `:1195-1197`, `TERM_HORIZON_CYCLES = COVER_CYCLES_MAX = 64` |
| Flush projection | `dirty×node_unit + promised×max(image_unit, node_unit)` | pending work is known before the cycle (promises) | `:1221-1230` |
| Covering barriers | `strict ? 1 : 2` | deferred mode pays the tick's flush barrier too | `:1241-1247` |
| Wedge rung | `PENDING_FREE_FORCE_CYCLES = 8` | a resolvable pinned floor converges in 2 cycles; 8 is the wedge class | `:1500` |
| Cover cycles | `COVER_CYCLES_MAX = 8 × 8 = 64`; fixpoint `2 × 8 = 16` | the audit fails a non-moving tail inside the 8th cycle, so reaching 64 means a MOVING tail (schedule); two windows (tail + bitmap) for the fixpoint | `:1514`, `:1529` |
| Writeback threshold | `4096` B of open delta | one bset page (§4.6 pt 1) | `node_cache.rs:124` |
| Fold memo | 8 cells | the hot-parent case needs 1; fixed per-node bound for budget accounting | `:129` |
| Overlay tail | 8 records | bounds the per-apply clone | `:632` |
| Fold capacity | `node_size − NODE_PAGE − frame_len − BSET_HEADER_LEN` | one image's record bytes | `node.rs:379-381` |
| Split part / merge pair | `¾ × fold_capacity` | append headroom after a split; merge successor as full as a fresh part | `:387-397` |
| Merge candidate | `¾C − ½C = ¼C` | fresh halves (> ½C) never pair ⇒ structural hysteresis | `:407-409` |
| SMO extents for parts | `fold ≤ C ? 1 : parts + 2` | packer rounding + cascade extent | `:423-429` |
| Split growth window | `max(split_part_capacity − record_value_cap, 1)` | growth below it cannot need a new part | `:436-440` |
| Compaction reserve / floor | `max(8, total/50)` / `reserve/2` | 2 % reserve; half admits deletes/compactions on a full volume, half stays the flush pass's | `alloc_ext.rs:156-169` |
| Pending-free cap | `65_536` | 16 GiB of 256 KiB extents; steady state ≈ 1.4 % under the 939-SMO/s storm | `backend.rs:210`, `mod.rs:457` |
| Journal failure latch | `3` consecutive | §4.4 pt 4 "repeated" | `backend.rs:241` |
| Commit/traversal retry budget | `256` | SMOs are rare and serialized; exhaustion is a protocol bug | `backend.rs:649`, `tree.rs:71` |
| Pending-times batch / cap | `128` inos / `512` pending | the `destroy_inodes` batching shape; bound the crash-loss window by count | `backend.rs:689-694` |
| Journal lane SQ depth | `512` | a window's entries (≤ batch cap, 1–2 SQEs each + headers) + the wake READ | `journal_lane.rs:53` |
| Reader poll cadence | `env else max(tick_period, 1000 ms)` | polling faster than the writer checkpoints buys nothing | `revalidate.rs:109-123` |

### 4.2 Measured numbers found in comments/docs (and what they justify)

| Number | Where | Justifies |
|---|---|---|
| `wake_hop` 746/664 µs of 1,209/1,130 µs journal round trip = **62 %/59 %**; lane ⇒ 53–107 µs; `journal_ring_write` −34…−57 %; single-committer chain 8 µs vs the pool's 19 µs | `.benchmarks/2026-09-03-c2-uring-fs-completion-hop.md:235-288` | the per-volume journal lane (§2.6) |
| quiet round trip 116 µs = queue 11 + device 38 + wake_hop 66; under lane+box load 923 of 1,206 µs; behind a 2 ms serve burst 2.9 ms | `journal_lane.rs:7-17` | same |
| M7 serialized pass 752–776 µs of which `journal_ring_write` 650–672 µs, leaf-lock window 86–91 µs, ρ ≈ 0.97, committers queued 764–801 µs | `backend.rs:45-49` | the two-stage split (D-2) |
| D-2 took the conveyor from ρ 1.0 to 0.13 | c2 note `:262` | |
| `pass_total` 97–147 µs on the lane legs | c2 note `:242` | |
| 11 % of many-dirs renames at 32 writers self-waited on their own tx; `dlm_inode_key_waits` p50 ≤ 16 µs | `backend.rs:22726-22728` | guards drop before the ack (D-3) |
| whole-overlay clone per apply ≈ 44 % of the serial create path | `node_cache.rs:630-631`, `:1949` | stable/tail split |
| k-way index rebuild per freeze 37 % of the serial create path | `node_cache.rs:278`, `:2274` | incremental `extend_with` |
| `InodeDelta::decode` 7.9 %, record fold 26.3 % of daemon CPU; M9 +43 % one-dir creates | design-metadata-throughput §1.1, implementation record | fold-head + memo |
| 16 % of daemon CPU re-decode tax | `mod.rs:663` | `META_KV_FOLD_RECORD_DECODES` pin |
| SMO rate ≈ 939/s aggregate (≈ 1 per ~55 creates at 52.5 k entries/s); backlog ≈ 1.4 % of cap; cap fills in ≈ 70 s of a pinned tail | design-smo-replay-currency §2-A/D | coverage gating + forced cycles |
| acked-loss 0.4–0.9 % across clean replay before C′+A | same §scope | two-phase replay, coverage gate |
| merge waves 8→16→32→64→128, 313 merges/14 sweeps, free 11→325 (24 MiB, 64 KiB nodes, reserve 8, floor 4) | design-cow-kv-metadata §4.6a (d) | geometric recovery |
| 32 MiB ring ≈ 139 K entries ≈ 4.3 s at 32 K creates/s; 8 MiB floor ≈ 35 K ≈ 1.1 s | §4.4 pt 5 | admission stalls = pathology |
| device bytes/create ≈ 0.5–0.7 KiB (11–24× under v2) | §4.4 write-amp table | |
| rename 1.003 / unlink 1.018 entries/op (gate ≤ 1.05); `io_uring_enter`/create 26 → 9.1 | metadata-throughput implementation record | journal economy |
| 1.12 ms write-inode convoy at 32×32 (`write_lock_wait` ≈ 1,120 µs, devices qd ≈ 1.5) | `design-write-inode-convoy.md:16-19, 56-60` | (data-plane; not rung 2) shared-mode write admission |
| 35 k-record same-key Δtime run folded in 44 s through O(run²) `run_end` | `checkpoint.rs:71-74` | memoized `run_end` + bounded drain (finding 49) |
| 62-part split, 2.3 MiB fold, 4.6 KiB entry past the first keep | `journal_core.rs:179-180` | the SMO keep bound |
| cycle term examples: 16–106 ms past 1,100; 38 images priced 2.04 ms paid 3.97; deferred barrier ≈ 2 ms (`meta_barrier` mean 1.3–1.9 ms, device 15 µs) | `checkpoint.rs:1056-1066`, `:1174-1178`, `:1254-1259` | per-item max units, decision-anchored term |

---

## 5. GAUGES / COUNTERS / HISTOGRAMS

Process-global `AtomicU64`s in `mod.rs` unless noted; per-volume faces live on `KvMetaBackend`. Kind: C = counter, G = gauge, H = histogram, P = posture word. ★ = must-stay-0 tripwire. Closure identities listed after each group.

### 5.1 Conveyor (stage A/B)

| Name | Kind | Meaning / alert | Anchor |
|---|---|---|---|
| `meta_commit_group_size` | H (buckets 1–8 exact, ≤16/≤32/≤64/>64) | txs per drained batch; `median_lower_bound ≥ 4` under an 8-writer strict storm is the G3 gate; ≈ 1 = the conveyor regressed | `mod.rs:698-880` |
| `meta_commit_group_bytes` | C | Σ entry bytes drained | `:703` |
| `meta_conveyor_leader_passes` | C | one per drained batch | `:707` |
| `meta_conveyor_pass_panics` | C ★ | a batch failed LOUD (budget released, reservation abandoned, EIO fan-out) | `:713` |
| `meta_conveyor_group_commits` / `_group_txs` | C | `enqueue_many` groups and their members; `txs ÷ commits` = live group size | `:720-726` |
| `meta_conveyor_windows_inflight` / `_hwm` | G / G | windows between handoff and terminal outcome; HWM ≥ 2 under load is the D-2 engagement instrument; bounded by admissible ring capacity | `:739-745` |
| `meta_conveyor_durability_passes` | C | one per lane group; `windows ÷ passes` = coalesce factor; flat while inflight grows = stalled lane | `:753` |
| `meta_conveyor_queued` | G | entries queued on any conveyor and not drained (wedge census) | `:772` |
| `meta_commit_parked` / `meta_publish_parked` | G (RAII) | committers parked on their oneshot right now | `:777-780` |
| `meta_kv_commit_sites` | map | successful `commit_tx` per `file:line` construction site (D4.a attribution) | `:895-934` |
| `meta_txpass_phase_ns{…}` | H×10 | `tx_queue_wait, pass_admission, pass_leaf_locks, pass_journal_write, pass_total, journal_ring_write, journal_prefix_wait, journal_barrier, window_lane_wait, window_total` | `fuse_client.rs:3924-3964` |
| `lock_phase_ns{…}` | H×6 | `stripe_lock_wait/hold, leaf_lock_wait, leaf_lock_hold (tail ≪ device latency = the "never across I/O" tripwire), dlm_guard_hold, dlm_guard_wait` | `:3407-3443` |

Closures: `leader_passes ≡ Σ group_size buckets`; `windows_inflight = 0` at quiesce; `commit_parked = 0` at quiesce; `tx_queue_wait + window_total ≈ commit latency`.

### 5.2 Journal / lane

| Name | Kind | Meaning / alert | Anchor |
|---|---|---|---|
| `meta_kv_journal_entries` / `_bytes` (+ `_per_volume`) | C | one whole tx each / `res.len` incl. headers | `mod.rs:350-358` |
| `meta_kv_journal_pad_entries` / `_bytes` | C | sector-pad entries behind windows (0 on an unpadded ring) | `:359-366` |
| `meta_kv_journal_full_stalls` | C | ring-admission parks; steady growth ⇒ checkpoint lagging | `backend.rs:1358` |
| `journal_lanes_spawned` | C | one per writable volume's first commit | `journal_lane.rs:56` |
| `journal_lanes_ringless` | C ★ (on any io_uring box) | the degrade-loud arm | `:59` |
| `journal_ring_lane_writes` / `_pool_writes` | C | engagement: lane writes ≡ passes on a lane mount | `:64-69` |

### 5.3 4a tables

| Name | Kind | Meaning | Anchor |
|---|---|---|---|
| `dlm_inode_stripes` / `dlm_dentry_stripes` | G | the derived width in force | `stripe_locks.rs` |
| `dlm_{inode,dentry}_stripe_collisions` / `_key_waits` | C | false-sharing vs true same-key waits on a contended acquire | `dlm.rs:18-21` |

Closure: `lock_phase_ns.dlm_guard_wait.count ≡ Σ both classes' (collisions + key_waits)`.

### 5.4 Node cache

| Name | Kind | Meaning / alert | Anchor |
|---|---|---|---|
| `meta_kv_node_cache_hits` / `_misses` / `_evictions` | C | hit rate = the Stat-regression early signal | `mod.rs:111-121` |
| node-cache bytes | G | `NodeCache::cached_bytes` (extents + overlay + heads + memo) | `node_cache.rs:2531-2537` |
| `meta_kv_fold_head_serves` | C | point lookups served from the overlay head (zero decodes) | `mod.rs:633` |
| `meta_kv_fold_memo_hits` / `_misses` | C | snapshot memo; create-storm hit rate is the acceptance number | `:640-646` |
| `meta_kv_fold_memo_bytes` | G | Drop-exact; rides the node-cache budget | `:656` |
| `META_KV_FOLD_RECORD_DECODES` | C (test pin only) | a head/memo serve moves it by exactly 0 | `:667` |
| `meta_kv_node_dropped_tail_bsets` | C | > 0 after a CLEAN unmount = corruption alert | `:105` |
| `meta_kv_node_partition_refusals` | C ★ | an armed reader mount mutated a node (v4: the appender partition and its foreign-append probe are gone — decision 0002 row 45) | `:189` |
| `meta_kv_leaf_lease_refusals` | C ★ | a leaf of a slot this mount does not lease (symmetric) | `:307` |
| `kv_freeze_latched_recovered` | C ★ (invariant tripwire) | FREEZING latched with no frozen delta — a third unknown window | `node_cache.rs:2169-2171` |
| `meta_kv_revalidate_polls/_epochs/_nodes_dropped/_stale_serves/_dirty_skips★/_keys_purged/_ledger_read_bytes/_ledger_full_reads/_gap_scans` | C | reader-side (0 for the life of a write mount) | `mod.rs:134-176`, `:429` |
| `meta_kv_reader_load_retries` | C | a reader's load raced the writer's append | `:182` |
| `meta_kv_projection_root_refreshes` / `_walk_exhaustions` | C | joined-appender projections (rung 6) | `:400-413` |

### 5.5 Writeback / SMO

| Name | Kind | Meaning / alert | Anchor |
|---|---|---|---|
| `meta_kv_node_appends` / `_append_bytes` | C | frozen deltas appended (4 KiB-padded) | `mod.rs:370`, `:434` |
| `meta_kv_node_rewrite_bytes` / `meta_kv_node_images` | C | whole-node CoW rewrites; images = the flush pass's per-image unit | `:439-446` |
| `meta_kv_node_compactions` / `_splits` / `_merges` | C | SMO kinds; compaction:append > ~1:8 ⇒ node_size/cadence mistuned; merges move on delete-heavy workloads | `:193-206` |
| `meta_kv_interior_merges` / `meta_kv_root_collapses` | C | level-≥1 merges; height decrements | `:213-219` |
| `meta_kv_node_freeze_shadow_dropped` | C | overlay records shadowed at freeze (designed high under same-key storms) | `:380` |
| `meta_kv_flush_floor_kept` | C | a flush appended a parked frozen delta and KEPT the open records' floor | `:390` |
| `meta_kv_commit_smo_retries` | C | lock-revalidate failures (SMO raced) | `:348` |
| `meta_kv_smo_entries_over_keep` | C (0 on healthy mounts) | an SMO entry wider than the class keep | `:240` |
| `meta_kv_bitmap_generation_raises` | C (0 across clean cycles) | DUR-4 law 2 raises | `:342` |

### 5.6 Checkpoint

| Name | Kind | Meaning | Anchor |
|---|---|---|---|
| `meta_kv_checkpoints` | C | ledger records written by cycles | `mod.rs:450` |
| `meta_kv_checkpoint_term_ms` / `_late_max_ms` / `_barrier_ms` / `_node_unit_ns` / `_image_unit_ns` / `_trigger_ms` / `_projected_ms` | G | the cadence term machinery (`CycleTermWindow` maxima over 64 cycles) | `backend.rs:1699-1790` |
| `meta_kv_checkpoint_last_cycle` | tape | the last cycle's decision + paid words | `:1746-1750` |
| `meta_flush_deferred` / `meta_device_syncs` / `meta_barrier` | C / C / H | deferred-mode tick barriers; every `sync_device`; barrier wall | `checkpoint.rs:2190`, `backend.rs:16085` |
| `appender_flush_ceiling_overruns` | C | forest flush-ceiling audit (rung 6) | `backend.rs:11000s` |
| `meta_kv_ledger_restatements` / `meta_kv_cursor_page_writes` | C | dense-ledger restatements; pre-barrier cursor pages (forest) | `mod.rs:422`, `:248` |

### 5.7 Space

| Name | Kind | Meaning / alert | Anchor |
|---|---|---|---|
| `meta_kv_free_extents` | G | per volume | `backend.rs:12000s` |
| `meta_kv_pending_free` | G | per volume — the wedge detector | §4.7 |
| `meta_kv_pending_free_parked` / `_released` | C | retirements parked / released by a covering tail | `mod.rs:459-467` |
| `meta_kv_pending_free_overflow` | C (≈ 0 steady) | forced retirements past the FIFO cap — the cycle-break's engagement | `:477` |
| `meta_kv_heap_promised` | G (0 at quiesce) | Σ extents promised to pending SMOs | `node_cache.rs:2546` |
| `meta_kv_heap_full` | P | the SPACE standstill posture (never terminal) | `backend.rs:1667` |
| `meta_kv_heap_full_cycles` / `meta_kv_enospc_refusals` | C | cycles that deferred for `NoSpace`; user commits refused `NoSpace` | `:1669-1673` |
| `meta_kv_merge_candidates` | G (EXACT under its counted tail) | underfull leaves after the last lap / census | `:1678` |
| `meta_kv_merge_sweeps` / `_laps` / `_yields` / `_sweep_ns` / `_sweep_projections` | C | bounded sweep calls; whole laps; yields to a space retry; ns ÷ projections = ns/leaf | `:1675-1806` |
| `frag_d4_mergeable_leaves` / `defrag_meta_merges` | G / C | the D4 census face | §4.6a (h) |
| `meta_kv_replay_root_frees_dropped` | C (0 on clean remount) | replayed frees of a live root dropped at mount | `mod.rs:488` |
| `meta_kv_replay_root_claims_adopted` | C (0 on clean remount) | extents a named root reaches that the recovered allocator held free, adopted at mount (a kill between a guest root's naming and its claim's C6 page) | Z/ `alloc_ext.zig` |

Closures: `parked − released ≈ Σ per-volume pending_free`; `merge_candidates ≡ census_at(gauge_tail)` with no commit since the publish; `census_now ≥ census_at(gauge_tail)`; `frag_d4_mergeable_leaves ≡ merge_candidates` on a quiescent volume; `heap_promised = 0` at quiesce.

### 5.8 Replay, economy, guard

| Name | Kind | Meaning / alert | Anchor |
|---|---|---|---|
| `meta_kv_replay_entries` / `_dropped_torn` / `_ms` | mount-scoped | `dropped_torn > 0` after a CLEAN unmount = corruption alert | `backend.rs:218-237` |
| `content_records` | mount-scoped | the clean-unmount law: 0 | `:227-236` |
| `meta_kv_replay_key_violations` / `_lease_violations` / `_extent_violations` | C ★ | one-ring-per-key / lease / grant violations at replay (symmetric) | `mod.rs:291-300` |
| `meta_kv_delta_orphans` / `meta_kv_dentry_collision_overflows` | C | Δ-without-base folds; 257th same-hash name | `:89-96` |
| `meta_kv_times_echo_absorbed` / `_drain_commits` / `_drained` / `_foreign_dropped` / `_pending` | C/C/C/C/G | the SETATTR echo absorber's economy | `:592-624` |
| `meta_kv_layout_delta_commits` / `_full_commits` / `_delta_bytes` / `_delta_folds` | C | layout delta economy (data plane) | `:675-689` |
| `meta_kv_block_refs_staged` / `_released` / `_recovered` / `_drift` ★ / `_unresolved` | C | durable refs (rung 4) | `:496-527` |
| `writer_guard_fenced` / `writer_guard_pr_reacquires` | C | D0 guard (rung 5) | `backend.rs:1639-1641` |

---

## 6. CRASH CONTRACT and how the harness proves it

**Contract** (`design-cow-kv-metadata.md:370-384`): D0 (process crash) — a returned tx's entry has completed at the device (O_DIRECT) and replay restores every returned tx atomically; D1/D2 (power loss, atomic or not) — identical: every unit checksummed, torn units detected and ignored, CoW + pending-free keep the fallback state intact; un-acked txs may be lost with holes (safe per key by LWW + resync). SMO-window closure (2026-07-16): acked-loss == 0 AND mount-refusals == 0 AND `replay_dropped_torn == 0` asserted **jointly** ×10 (a mid-entry tail on the newest entry can read 0 alone).

### 6.1 Kill-window outcome table (what a kill -9 or power cut at each point must replay to)

| Window | On-disk state at the kill | Mount outcome (what the harness asserts) |
|---|---|---|
| Committer enqueued, pass not yet admitted | nothing reserved, nothing applied | tx absent; guards died with the process; `ENOENT`-class for an un-acked create — never acked, so not a loss |
| Pass between admission and reservation (A2–A7) | nothing on the ring | same; admitted budget is process-local |
| Pass after reservation, before submit (A8–A11) | reserved range unwritten (a hole) | replay's checksum walk drops the hole; same-page successors of a dead chain are lost (un-acked by construction — the lane never acked past an unlanded predecessor); the volume's next ledger must pass the hole (`checkpoint_past`) |
| Entries submitted, not landed (lane B2) | some entries' bytes may be on the device, torn or whole | each whole entry replays (un-acked tx applied — permitted); a torn member drops ALONE (`test_kv_batch_torn_*_member_drops_alone`); a torn member followed by a whole same-page successor: the successor follows the chain rule (`:1844`) |
| Entries landed, prefix not complete, not acked (B4–B5) | entries whole on the device | replayed; un-acked but whole — fine |
| Strict barrier in flight (B8) | entries whole, barrier not durable | deferred-mode class: entries whole in the device's cache; with a REAL volatile cache they may vanish — un-acked, so no loss; `arm_power_cut` reverts them and the harness asserts only ACKED ops |
| Acked (deferred cadence), checkpoint not yet run | entries whole, no ledger covers them | replayed from `[tail, head]`; whole-tx atomic (`test_kill9_remount_soak_v3`) |
| Freeze done, append not landed | node extent tail torn or stale | §4.5 classifier truncates at the first checksum failure; records are > tail (the node's floor pinned the tail) so replay re-supplies them (`test_kv_node_torn_tail_bset_dropped_counted_priors_intact`); a valid bset after the tear with `horizon ≤ tail` fails LOUD |
| SMO P1 (images written), before P2 barrier/admit | successor images durable-or-not, unreferenced; claims RAM-only | nothing changed; old node serves; no leak (bitmap bit not durable) |
| SMO P3 done, P4 entry unwritten | successor durable, flips/alloc/free in a hole | the hole drops whole; predecessor stays routed and allocated; a checkpoint that ran meanwhile (process lived) names roots reaching the successor and its parent bset carries the flip (`design-cow-kv-metadata.md:300-304`) |
| SMO P4 entry written, before a covering ledger | flips + alloc + free in the window; parent's flip maybe unflushed | phase 1 applies flips (level DESC, seq), phase 2 content by key through the final routing; the free parks until the first post-mount durable checkpoint (`mount_side_replayed_free_parks_until_post_mount_checkpoint`) |
| Root swap (grow/collapse) before the next ledger | RAM root swapped, `[free(old)]` maybe in the window | replay through the OLD structure; the old root's extent parked (its free dropped if it IS a mounted root — `an_unnamed_root_swaps_replayed_free_never_releases_the_live_root`); a later ledger naming the child drops the window's unroutable flips |
| Checkpoint between barrier #1 and the ledger write (C7–C9) | pages + appends durable, record absent | the previous ledger record is selected; longer replay window; dying floors were never drained (restored on error) |
| Ledger record written, not yet durable (`barrier_now = false`) | newest slot may be torn | torn newest ⇒ predecessor + longer window (`test_kv_ledger_torn_slot_falls_back_to_predecessor`); `reusable_upto` never advanced past the predecessor's tail so the ring window is intact (`test_kv_ring_reuse_never_overwrites_fallback_window`) |
| Ledger durable, pending frees released, bitmap page torn | A/B pair: one slot torn | newest valid slot + journal deltas reconstruct (`test_kv_bitmap_torn_slot_predecessor_plus_journal_reconstruct`) |
| Clean unmount | `tail == head`, bitmap pages persisted, `reusable_upto == head` | empty replay window; `dropped_torn == 0`, `content_records == 0`; a residue means the shutdown fixpoint missed (WARN) |

### 6.2 Harness mechanisms

- `uring_fs::TORN_WRITE_FAULT` tears arbitrary offsets; `arm_power_cut(path)`/`power_cut(path)` revert every write not covered by an `fdatasync` on the metadata path (`src/uring_fs.rs:1300-1310`, `:1477`); `arm_barrier_error` injects the reservation-conflict errno at the **barrier layer** (`:1312+`).
- `src/dev_power_cut.rs` — the data-device twin for `NvmeBlockDev` (journal prior bytes per write, barrier epochs, `power_cut` restores in reverse admission order; env `SQUEEZEFS_TEST_POWER_CUT_DEVS`). Coverage law: a barrier covers writes journaled before it started; concurrent in-flight writes are not modelled (quiesce first).
- Deterministic seams: `TEST_SMO_BUILD_PAUSE_TREE` (build-window stranding), `TEST_CONVEYOR_HOLD_STAGE` (pre-drain / pre-fanout / empty-drain tail / pre-rollback / post-recall / post-door), `TEST_CONVEYOR_POISON_APPLY_INO` (in-window member failure), `TEST_COMMIT_ADMITTED_STALL_MS`, `TEST_CHECKPOINT_HALT_{BEFORE,AFTER}_LEDGER`, `TEST_CHECKPOINT_PARK_BEFORE_PAGES`, `TEST_FREEZE_ENCODE_FAIL`, `TEST_PENDING_FREE_CAP`, `TEST_SHUTDOWN_FIXPOINT_CYCLES`, `TEST_BRING_UP_COVER_DISABLED`.
- **Kill matrix**: `tests/crash_kill_tests.rs` re-execs the test binary as a child (`SQUEEZEFS_CRASH_CHILD_V3=1`), churns create/setxattr/unlink/destroy appending a side ledger (ONE `write(2)` + `fsync` per line, `:30-47`), SIGKILLs at a random 5–50 ms deadline, remounts and asserts every ledger-ACKED op present and whole unless superseded, plus replay idempotence (remount twice ⇒ identical post-fold digest). `SQUEEZEFS_CRASH_ROUNDS` default 20; nightly 500. Variants: `test_kill9_remount_soak_v3`, `_batched` (3 concurrent lanes), `test_kill9_after_arm_same_host_instant_reclaim`, `test_rollback_race_seq_conditional`, `test_repeated_journal_failures_escalate_to_disabled_volume`. Program-level matrices: FIND-VS-A storm shape (16-worker acked-create storm, kill -9 at peak, full mask + `taskset 0-15`) ×10; `sym-crash` matrix (design-symmetric-metadata §9 row 4); `tests/run_unmount_kill_soak.sh` (mount → I/O → kill/umount shapes, fails on SIGABRT/panic/drain-timeout).
- **fsck-clean**: after every kill `fsck_findings == 0`, `meta_kv_block_refs_drift == 0` (C8 oracle), `data_alloc_bitmap_drift == 0`; fsck has no meta-tree structural class — the tree's structural detectors are the mount's `child_node_seq` check (now a TRUE corruption detector), `KvTree::open` root checks, `verify_node_extent`, and the K2 walk in `kv_leaf_merge_tests` (`design-cow-kv-metadata.md:308`).
- **Torn-unit cases** (`tests/crash_contract_tests.rs`): node torn tail dropped+counted / valid-bset-after-tear with horizon ≤ tail fails loud / torn rewrite unreferenced; journal torn last entry / mid-log drop+resync / torn page header never loud / garbage len never dereferenced / hole then resync / torn middle page of a multipage entry; ledger torn slot falls back; **ring reuse never overwrites the fallback window** (`:921`); bitmap torn slot + journal reconstruct; torn newest root after churn; rename one-entry atomicity under power cut (`:1591`); **batch cases**: torn first/middle/last member drops alone, same-page successor follows the chain rule, batch spanning ring wrap, mid-rollback race RAM ≡ replay, replay-twice digest equal (`:1792-2069`).
- **SMO completeness** (`tests/kv_smo_crash_completeness_tests.rs`): acked creates survive SMO churn; stranded build-window commits survive; replay-twice digest stable; mid-entry tail multi-leaf partial flush; root-swap freed extent parked until the tail covers the free; mount-side replayed free parks until post-mount checkpoint; unnamed root swap's free never releases the live root; pending-free at cap forced cycle completes and conserves; pinned floor at cap `checkpoint_now` / maintenance converge, never fail loud; wedged shape reopen recovers and drains; ring-full crash mount recovers via the pre-claim drain.

---

## 7. PROPOSED ZIG MODULE DECOMPOSITION (implementation order)

Sizing: S ≤ ~400 lines, M ≤ ~1,500, L > 1,500. ⚙ = needs the exhaustive interleaving explorer (model check as a dependency-free module, record the weakening that fails each model). 💾 = needs a real device/file (io_uring, O_DIRECT) for its contract tests. Prerequisites assumed landed from the earlier surveys: record/bset codecs + fold algebra, node format (`NodeLayout`, load/append/compact/split), journal ring + reservation core + ledger, extent allocator + pending-free core.

| # | Zig module | Ports | Size | Flags | Tests to port (name — one sentence) |
|---|---|---|---|---|---|
| 1 | `meta/kv/node_state_core.zig` | `node_state_core.rs` lifecycle word | S | ⚙ | `node_state_supersede_never_loses_a_racing_apply` — an applied record is visible to the successor build or never accepted; `node_state_evict_never_wins_against_accepted_dirt` — CAS 0→SUPERSEDED loses to any dirt; `node_state_freeze_swap_conserves_records` — a concurrent apply lands in the frozen or the fresh delta; `node_state_at_most_one_freeze_in_flight`; `node_state_forced_freeze_conserves_and_reports_freezing`; `kv_tree_tests::node_lifecycle_word_transitions`. |
| 2 | `meta/kv/epoch_core.zig` | `epoch_core.rs` (`RevalidationEpoch`, `AppendGate`, `NodeEnv`, `StructuralHolds`) | S | ⚙ | `epoch_core_loader_never_sees_an_epoch_newer_than_its_tail` — tail-then-epoch / epoch-then-tail orderings; `epoch_core_racing_pollers_step_the_epoch_exactly_once`; `epoch_core_a_node_published_across_an_advance_is_adopted_or_rejected`; `epoch_core_gate_declarations_never_clobber_each_other`. |
| 3 | `meta/kv/conveyor_core.zig` | `conveyor_core.rs` | S | ⚙ | `conveyor_leader_unique_and_loser_entry_drained`; `conveyor_no_lost_wakeup_across_unlead` — release-then-recheck; `conveyor_fifo_apply_order_under_race`; `conveyor_budget_conserved_across_committer_drop`; `conveyor_guard_outlives_dropped_committer_until_terminal`; `conveyor_group_never_drains_partially`; unit: `drain_is_fifo_and_cap_bounded`, `leadership_is_exclusive_and_recheck_reelects`, `enqueue_many_is_fifo_and_counted`. |
| 4 | `meta/kv/node_snapshot.zig` | `RecordIndex`, `IdxRec`, `OwnedRec`/`FoldedHead`, `FoldMemo` (8 cells), `NodeSnapshot` (base + stable + tail ≤ 8 + memo), `lookup`/`next_live`/`prev_live`/`newest_seq_of`/`delta_chain_probe`/`fold_bytes_upper_with` | M | — | `kv_fold_slimming_tests` (zero-decode pins: memo ≡ from-scratch fold property test; head serve moves `FOLD_RECORD_DECODES` by exactly 0); `kv_tree_tests::reads_proceed_while_writer_holds_the_node_lock` — snapshot reads never take the node lock. |
| 5 | `meta/kv/node_cache.zig` | `NodeDirty` (overlay, frozen, promise ledger, charge), `CachedNode` (`apply_locked` choke point with gate checks, D7.a heads, inversion guard, floor stamping; `freeze_locked` with W-A restore and RES-22 tripwire; `take_overlay`; floors), `NodeCache` (map, single-flight load with pre-read epoch snapshot and post-read retired re-check, `publish_stamped`, `retire` mark-before-remove, clock `evict_to_budget` with the `strong_count` gate, `append_frozen`, dying floors, durable tail) | L | 💾 (loads/appends) | `kv_tree_tests::eviction_respects_budget_pins_dirty_and_interior_and_held_snapshots_survive`; `single_flight_collapses_racing_cold_loads`; `writeback_appends_then_compacts_then_splits`; `kv_freeze_wedge_tests::an_encode_failure_inside_freeze_leaves_the_node_freezable` — W-A restore; `a_hand_latched_freeze_recovers_loud_with_one_tripwire` — RES-22; `a_freeze_refusal_is_a_protocol_error_not_encoding_corruption` — typed `FreezeRefused`; `a_flush_of_a_parked_frozen_delta_keeps_the_floor_of_the_records_applied_since` — `overlay_floor` kept; `a_dropped_forced_compaction_leaves_the_node_freezable` — `ForcedFreezeGuard`; `an_over_cap_record_fails_its_own_commit_not_the_checkpoint`; `crash_contract_tests::test_kv_node_torn_tail_bset_dropped_counted_priors_intact`, `test_kv_node_valid_bset_after_tear_horizon_le_tail_fails_loud`. Derivation tie: `max(budget/16, 512 MiB)`. |
| 6 | `meta/kv/tree.zig` (traversal + commit-side apply + replay applies) | `KvTree` root `ArcSwap<RootPtr>`, `descend` with reason tallies + yields + `level < target` restart, `resolve_leaf`, `apply_at_seq_class` (lock → revalidate → seq inside → `apply_locked` → threshold enqueue), `apply_replayed` / `_interior` with the LWW skip, `range`, `maintenance` queue | M | — | `kv_tree_tests::point_ops_insert_overwrite_delete_reinsert`; `range_scan_folds_orders_and_resumes`; `stale_writer_revalidates_and_retries_after_smo`; `interior_value_roundtrip_and_bounds`; `kv_scale_tests::descend_survives_sustained_smo_churn`; `rightmost_separator_pointer_record_replays_clean`; `lookup_p50_immune_to_tombstone_desert`. |
| 7 | `meta/kv/smo.zig` | `SmoJournal`/`SmoContext`/`SmoRegion` (region plumbing may stay a stub), `admit_smo_entry` + keep law, `claim_internal`/`release_unpublished`/`free_pending(forced)`, `maintain_node`, `checkpoint_flush_node` (freeze + take floor + `FLUSH_FLOOR_KEPT` + restore on failure), **`smo_replace`** (build/barrier/admit/lock-window/publish→flip→retire/write/park), `resolve_parent`, `fold_source_extra` guards, build-pause seam | L | 💾 | `kv_tree_tests::split_vs_commit_storm_loses_nothing`; `interior_recursion_grows_root_to_depth_three`; `reopen_from_disk_after_flush_serves_everything`; `kv_smo_crash_completeness_tests::acked_creates_survive_crash_across_smo_churn`, `stranded_build_window_commits_survive_crash` (needs the pause seam), `replay_twice_digest_stable_across_smo_windows`, `root_swap_freed_extent_stays_parked_until_tail_covers_free`, `an_unnamed_root_swaps_replayed_free_never_releases_the_live_root`; `crash_contract_tests::test_kv_node_torn_rewrite_unreferenced_old_node_untouched`; derivation tie for `smo_keep` vs `parts ≤ ⌈(C + ring_len)/¾C⌉` (`derivation_sweep_tests`). |
| 8 | `meta/kv/leaf_merge.zig` | `fold_upper`, `pair_mergeable`, `is_merge_candidate[_at]`, `merge_candidate_census`, `resident_nodes_at`, `merge_underfull` (bounded, cursor-resumed, count phase), `try_merge_node`, `smo_merge`, `smo_root_collapse`, `merge_after_flush` prefilter, `MergeSweep`/`SweepRefusal`/`SweepCursor` | L | 💾 | `kv_leaf_merge_tests::merge_bounds_derive_from_the_split_fill_target` (tie test); `a_mostly_deleted_full_volume_keeps_creating`; `fold_equivalence_holds_across_a_merge_storm`; `height_three_tree_collapses_to_a_root_leaf_gap_free_with_readers_mid_walk`; `stale_writer_revalidates_and_retries_after_a_merge`; `merge_vs_commit_storm_loses_nothing`; `a_merge_is_admitted_at_the_compaction_floor_and_returns_extents`; `replay_converges_across_crash_windows_of_a_merge_and_a_root_collapse`; `cross_parent_underfull_leaves_shrink_through_interior_merges_to_a_fixed_point` (K2 walk + fixed-point law); `merge_sweep_is_bounded_per_call_and_resumes_from_its_cursor`; `merge_sweep_budget_derives_from_the_checkpoint_tick_period`; `merge_candidates_gauge_is_exact_mid_wave_and_at_quiescence`; `a_refused_multi_leaf_member_strands_no_heap_promise`; `d4_census_reports_mergeable_leaves_and_the_defrag_arm_merges_them` (needs the defrag arm — may defer). |
| 9 | `meta/kv/dlm.zig` + `stripe_locks.zig` | 4a `I{ino}`/`D{parent,name}` striped `RwLock` tables, `lock_many` canonical order (I before D, dedup ascending, shared+exclusive ⇒ exclusive), `try_lock_many`, census (collisions vs key waits), `dlm_stripe_width` derivation | M | — | `meta_dlm_stripe_tests` (canonical order, collision widening never under-locks, two-phase discovery), derivation tie `next_pow2(max(4096, 16×cpus×q_depth))`, closure `dlm_guard_wait.count ≡ Σ census`. The sqz async RwLock itself (no ownership assignment to sleepers, 2 s ticked backstop, bounded barging) is charter §9 — `sqz_sync_models` ⚙. |
| 10 | `meta/kv/tx.zig` | `KvTx` staging + RYOW helpers (`chain_occupancy`, `dentry_insert_key`), `build_queued_tx` (value cap, `entry_len_for`, forest key framing), `UndoKey` | S | — | `conveyor_tests::batch_of_one_keeps_todays_entry_shape` (bytes identical to a solo entry); value-cap typed refusal (`kv_tree_tests::value_cap_enforced_typed`); dentry chain RYOW for rename exchange (routed rename tests). |
| 11 | `meta/kv/journal_lane.zig` | `JournalLane` thread `sqz-jrnl<N>`: own io_uring (512), `RingPark` (park in `io_uring_enter` with the eventfd READ armed; `service` reaps per iteration), `spawn_task` panic-contained, lane-native `submit_write_at_batch`, ringless degrade | M | 💾 | `conveyor_completion_hop_tests::completion_delivery_is_isolated_from_the_serve_lanes` — `wake_hop` stays ≪ a serve burst under lane load; `measurement_rows_completion_hop_under_lane_and_box_load` (measurement row, bench-smoke only); engagement: `journal_ring_lane_writes ≡ passes`. Depends on the charter §9 lane executor. |
| 12 | `meta/kv/conveyor.zig` | `QueuedTx`, `ConveyorWindow`, `BatchProduct`, `PassSentinel`, `LaneSentinel`, `commit_tx`/`commit_tx_group`, `conveyor_pass_task`, `run_batch_pipeline` (Σ admission → union locks → revalidate → heap admission → pre-images → ONE reservation → apply in order → unlock → submit → handoff), `durability_lane_task`, `run_windows` (group, complete in order, prefix wait, verdicts, hole checkpoints, ONE barrier, outcomes in order), `fan_out` (guards drop before send), `abandon_window`, `admit_user_budget` (park + kick + D1.b escalation), `rollback_failed_tx` + `commit_compensation`, `note_journal_failure` | L | ⚙ (two-stage composition model) 💾 | Loom: `conveyor_two_stage_models::two_stage_acks_in_journal_order_parked_head_holds_successors_rollback_only_leaf_taker` (laws 1–4; weakenings: lane ack publish Release→Relaxed fails law 1, device landed publish Release→Relaxed fails law 3). Contracts: `conveyor_tests::{group_forms_under_held_pass, batch_tx_cap_bounds_group_size, batch_byte_cap_bounds_group_size, poisoned_tx_fails_alone_batch_survives, cancel_pre_enqueue_is_inert, cancel_post_enqueue_same_key_blocks_until_terminal, cancel_mid_pass_conserves_and_commits, cancel_pre_fanout_is_harmless, strict_batch_pays_one_barrier, a_group_of_distinct_inos_is_one_pass_by_construction, an_inadmissible_member_fails_alone_and_its_siblings_commit, empty_groups_and_empty_members_commit_nothing, two_racing_groups_commit_every_member_exactly_once, an_over_cap_group_splits_at_the_byte_cap_and_commits_all, a_group_replays_identically_to_single_commits}`; `conveyor_two_stage_tests::{windows_overlap_when_the_journal_device_is_slow, throughput_tracks_the_committer_population_when_the_device_is_slow, acks_follow_journal_order_across_overlapped_windows, an_ack_never_precedes_its_barrier_on_the_strict_cadence, barrier_failure_fail_stops_from_the_durability_stage_with_windows_in_flight, acked_commits_survive_kill9_while_windows_overlap}`; `crash_contract_tests::{test_kv_batch_torn_first/middle/last_member_*, test_kv_batch_torn_member_same_page_successor_follows_chain_rule, test_kv_batch_spanning_ring_wrap_replays_whole, test_kv_batch_mid_rollback_race_ram_equals_replay, test_kv_batched_commits_replay_twice_digest_equal}`; `crash_kill_tests::{test_rollback_race_seq_conditional, test_repeated_journal_failures_escalate_to_disabled_volume}`; derivation ties for both batch caps. |
| 13 | `meta/kv/checkpoint.zig` | derivations (`resolve_max_dirty_nodes`, `checkpoint_tick_period_ms`, `merge_sweep_budget_ms`, landing ceiling/trigger/term/units/projection — the forest-only term machinery can land as pure functions with tie tests first), `DrainBudget`, `checkpoint_task` loop (fixed deadline read off the clock, park-kick promotion, FAILED idle), `maintenance_pass`, `decide_checkpoint`, `tick` (shutdown head-stable wait before the SMO mutex, decide-before-drain, deferred barrier, fixpoint), `checkpoint_cycle_managed` (collect → ordered flush with the keep-exempt oldest → bitmap pages → barrier #1 → tail = min(h, inflight, dying, floors) → ledger → `pending_reclaim.push((tail, epoch))` → barrier_now + wedged-tail audit → merge sweep), `sync_device` DUR-3 tickets + barrier lattice, `after_durable_barrier`, `checkpoint_now/past/past_region`, `cover_bring_up_residue`, kicks | L | 💾 | `conveyor_two_stage_tests::checkpoint_cadence_survives_a_sustained_maintenance_storm` and `parked_committers_are_released_by_the_checkpoint_under_a_sustained_storm` (finding 49); `kv_scale_tests::{threshold_writeback_drains_without_cadence, shutdown_signalled_mid_maintenance_pass_completes_within_the_tick}`; `crash_contract_tests::{test_kv_ring_reuse_never_overwrites_fallback_window, test_kv_ledger_torn_slot_falls_back_to_predecessor, test_kv_v3_torn_newest_ledger_mount_serves_predecessor, test_kv_alloc_torn_newest_root_after_churn_predecessor_extents_intact}`; `kv_smo_crash_completeness_tests::{mid_entry_tail_multi_leaf_tx_partial_flush_survives_crash, mount_side_replayed_free_parks_until_post_mount_checkpoint, pending_free_at_cap_forced_cycle_completes_and_conserves_extents, pending_free_pinned_floor_at_cap_checkpoint_now_converges, pending_free_pinned_floor_at_cap_maintenance_converges_never_fails_loud, pending_free_wedged_shape_reopen_recovers_and_drains, ring_full_crash_mount_recovers_via_preclaim_drain}`; DUR-3 reclamation-epoch legs (`checkpoint_cadence` shape); `crash_kill_tests::test_kill9_remount_soak_v3[_batched]` (the kill matrix; 20 rounds gate, 500 nightly); derivation ties for every constant in §4.1. |
| 14 | `meta/kv/heap_law.zig` (or inside `conveyor.zig`) | `admit_heap_locked` fast path + exact walk + promises/retractions, `enter/leave_heap_full`, `heap_growth_floor_clear`, the space-retry hold, `NoSpace → ENOSPC` mapping | M | — | `kv_leaf_merge_tests::a_refused_multi_leaf_member_strands_no_heap_promise`, `a_merge_is_admitted_at_the_compaction_floor_and_returns_extents`; the inline-raise sweep's full-volume contract (acked records always flushable; `heap_promised == 0` at quiesce; the wedge audit never fires on a full heap). |
| 15 | `meta/kv/inos.zig` | `next_ino` watermark, create `fetch_add`, open recovery `max(ledger, max mentioned + 1)` (DUR-8c: inode keys, dentry child inos, xattr keys), `destroyed_inodes`/`live_inodes`, `destroy_inodes` batched entry; `ino_lane.zig` as pure lane arithmetic (`recover_ino_floor`) | S | — | `mw_ino_lane_tests::{lanes_are_disjoint_monotone_and_attributable, solo_lane_cursor_ties_the_shipped_cursor, recovery_rounds_into_the_lane_and_never_re_mints, a_crash_and_remount_never_re_mints_a_lane_ino, live_inode_count_is_exact_under_lanes}`; `crash_kill_tests` acked-op model (no re-mint of a mentioned ino); `kv_scale_tests::million_entry_directory_storm_create_lookup_readdir_rmdir`. |
| 16 | `meta/kv/economy.zig` (routed arms) | rename one-tx (dentry surgery + both parents' Δtime + moved Δctime), SETATTR echo absorber + `pending_times` map + the dedicated drain task (128/512), parent `SharedTimes` Δ records, batched destroys | M | — | `meta_entry_economy_tests` (rename ≤ 1.02, unlink ≤ 1.05 entries/op; `commit_sites` attribution; zero-commit inline teardown pin); `crash_contract_tests::test_rename_one_entry_atomicity_power_cut`; generic/003-class remount divergence (pending-times fold on read). |
| 17 | `meta/kv/revalidate.zig` | reader cadence derivation, `RevalidationPoller`, `revalidate_trees` roots-then-epoch, predicted-slot ledger read, `arm_reader_revalidation` | M | — | **Defer to rung 6 (S5 readers)**; the cache's epoch stamp (module 5) must already exist so arming is a once-per-mount declaration, not a refactor. |
| 18 | `meta/kv/indirect_map.zig` | the data-plane blob hook | S | — | **Defer to rung 4/6** (needs the router). |

### 7.1 Per-module interface notes (what each Zig module must expose; no code)

- **`node_state_core.zig`** — one `std.atomic.Value(u64)`; API exactly the eleven ops of §3.3; every op documents its pairing (`mark_dirty` AcqRel vs `supersede` AcqRel `fetch_or`; `try_evict` CAS from 0). No allocator, no I/O, no dependency — so the explorer can include it standalone.
- **`epoch_core.zig`** — two `u64` atomics + the packed `u32` gate word + the `[2]` hold ledgers; the comment on `publish` must name the two-word law and the model that fails on reversal.
- **`conveyor_core.zig`** — generic over `T`; `std.Thread.Mutex` around a ring/deque (sanctioned metadata-transaction class), `AtomicBool leader` SeqCst; optional gauge pointer. Must expose `enqueue_many` (group atomicity) and `pending()` (stats + conformance probe).
- **`node_snapshot.zig`** — `RecordIndex{sources: []Bytes-like refcounted slices, entries: []IdxRec(24 B)}` built by a k-way cursor merge then per-key `seq desc` normalization; `NodeSnapshot{base: *RecordIndex (refcounted), stable: refcounted []OwnedRec, tail: []OwnedRec ≤ 8, memo: [8]OnceCell}`; `lookup` = binary search + fold newest-first with the head short-circuit; `fold_bytes_upper_with(extra, part_cap, tail)` returns `(bytes, parts)`.
- **`node_cache.zig`** — `CachedNode` with an arc-swap-style `*NodeSnapshot` (atomic pointer + refcount), `SqzRwLock(NodeDirty)`, the lifecycle word, the floor atomics, pin/ref bits; `NodeCache` with a concurrent map keyed by `addr`, an inflight map of single-flight senders, a lock-free clock queue, the budget gauge, the promise ledger, the `retired` set, the dying floors. Loads are single-flight; every map bucket is taken synchronously and never across a suspension point (`node_cache.rs:3695-3705`).
- **`tree.zig`** — `KvTree{tree_id, cache, root: atomic *RootPtr, seq: *NodeSeqHandle, maintenance: lock-free queue, merge_cursor, root_floor}`; `descend` returns a strong node ref; `apply_at_seq_class` is the ONE lock-window primitive both live commits and replay use.
- **`smo.zig`** — `SmoContext` passed `*` (exclusive) through every SMO entry; `SmoJournal{ring, retire_seq, sync, path}`; the `smo_replace` body in the exact P0–P5 order with the publish→flip→retire ordering comment and the leftovers' per-record `entry_floor` floor.
- **`leaf_merge.zig`** — the three capacities from `NodeLayout`; `MergeSweep`, `SweepRefusal`, `SweepCursor`; `merge_underfull(ctx, forced, deadline)` returning `lap_complete`, `candidates`, `space_refused`, `refusal`, `projections`, `elapsed`.
- **`dlm.zig`** — two stripe tables of async RwLocks (owned guards, movable, `'static`-equivalent so a guard set can travel into a queue entry); `lock_many(plan) → []Guard` canonical; `try_lock_many`; the census words.
- **`tx.zig`** — `KvTx{staged: ArrayList(Staged), site, guards: shared []Guard, flags}`; `build_queued_tx` producing `QueuedTx` + a oneshot receiver.
- **`journal_lane.zig`** — one OS thread per writable volume owning an io_uring (512 SQEs) and a `LaneExec` whose park is `io_uring_enter` with the eventfd READ armed; `spawn_task(site, future)` panic-contained; `submit_write_at_batch` lane-native only on the lane thread.
- **`conveyor.zig`** — `QueuedTx`, `ConveyorWindow`, the two sentinels (Zig: `defer`/`errdefer` with explicit "armed" booleans replace `Drop`; a panic in a conveyor task must be caught by the lane's panic containment and run the same cleanup), `commit_tx`, `commit_tx_group`, the pass and lane task bodies, `admit_user_budget`, `rollback_failed_tx`, `commit_compensation`.
- **`checkpoint.zig`** — pure derivation functions with tie tests; `DrainBudget`; `CycleTermWindow`/`CycleTape`/`FlushPassSample`; the task loop; `decide_checkpoint`; `tick`; `checkpoint_cycle_managed`; `sync_device` with the DUR-3 tickets; `after_durable_barrier`; `checkpoint_now/past/past_region`; `cover_bring_up_residue`; the kicks.
- **`heap_law.zig`** — `admit_heap_locked(entries, leaves, lock_set, guards) → []Refused`; `enter/leave_heap_full`; `heap_growth_floor_clear`; the space-retry hold.
- **`inos.zig`** — `next_ino` atomic; open-time recovery fold; `destroyed_inodes`; `destroy_inodes` batching; lane arithmetic.
- **`economy.zig`** — the routed arms' staging shapes (rename, SETATTR absorb, parent Δtime, destroys) and the pending-times map + drain task.

### 7.2 Red-first contract order per module (what to write before the code)

1. Derivation tie tests for every §4.1 formula the module owns (drift is red).
2. The loom-equivalent models for ⚙ modules, each with its recorded failing weakening.
3. The unit contracts listed in the table (ported names, same assertions).
4. The crash-contract cases the module's durable ordering claims (torn units, power cut, kill matrix rows) — on a file-backed `O_DIRECT` volume with the buffered fallback counted.
5. The engagement gauge assertions (a row is invalid unless the gauge's delta accounts for the row's ops) and the closure identities (`parked − released ≈ pending_free`, `lane_writes ≡ passes`, `dlm_guard_wait.count ≡ Σ census`, `leader_passes ≡ Σ group_size buckets`, `windows_inflight = 0` at quiesce, `heap_promised = 0` at quiesce).

**Ordering rationale**: 1–3 are dependency-free cores that the model checker needs first and that nothing else can be written correctly without (the port must record each failing weakening as the Rust models do). 4–5 give a RAM-authoritative cache that already pins floors, charges budget and refuses on the gate. 6–7 make a tree that splits/compacts with the full three-step protocol and reserve-in-window discipline — this is where the SMO phases, dying floors and two-phase replay become testable against the crash harness. 8 is the §4.6a rung and needs 7's `smo_replace` verbatim. 9–11 are the conveyor's prerequisites (guards to co-own, a tx to queue, a thread to run on). 12 is the highest-risk module (the reference's PR M7/D-2 budget was ~3 weeks + the two-stage note); land it with the two-stage loom model and the cancel/poison/torn-batch suites red-first. 13 cannot be unit-tested without 12 (the tail rule reads in-flight reservations the lane owns) and 12 cannot be soaked without 13 (admission parks until a cycle reclaims) — land 13's derivations and `checkpoint_cycle` core first with `checkpoint_now` as the driver, then the task loop. 14–16 close the charter's rung-2 tail (ENOSPC/promise law, monotonic inos, journal economy).

**Model-check set for this rung** (charter §9 list intersected with these modules): journal reservation (admission + reservation jointly — already in the journal survey), node lifecycle (module 1), the conveyor handoff (module 3) and the two-stage composition (module 12), the epoch pair (module 2), the pending-free gate/overflow conservation (`alloc_ext_forced_overflow_gate_and_conservation`, `alloc_ext_pending_free_never_claimable_before_covering_tail` — allocator survey), the async RwLock/Notify primitives the node lock and the admission park ride (`sqz_sync_models`).

**Real-device set**: every module that appends/loads node images, writes ring entries or barriers (5, 7, 8, 11, 12, 13) — the file-backed `O_DIRECT` posture (regular-file `EINVAL` ⇒ loud buffered fallback) is enough for the cargo-tier contracts; the kill matrix and the storm soaks are the acceptance brackets (`.benchmarks` discipline: nvmet-loop for latency-shaped rows).
