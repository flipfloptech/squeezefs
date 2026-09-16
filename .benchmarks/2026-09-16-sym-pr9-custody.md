# Symmetric metadata program — PR 9: custody by the slot holder, the custody grant carrying the file's token, the PK4 wire adjudicated, PR 7's owed pair cancellation (2026-09-16)

| | |
|---|---|
| **Design** | [`docs/design-symmetric-metadata.md`](../docs/design-symmetric-metadata.md) §5.5 (the "S9 custody endpoint" row — *the slot holder of the file … 0 own; 1 remote grant for a foreign file*), §5.1.5 (the custody lock class homes on the slot of the file), §5.1.4 (handover — "the departing holder's outstanding custody leases"), §5.4.1 / §5.4.3 (the W1 clause; law 1's pack scope), §5.6 (custody under a cross-owner op), §5.7.1 (the holder's implicit Write; the grant carries the records), §8 gate 1 (`dlm_rpcs == 0`), §11 (the `dlm_custody` family); PR plan row **9** (deps 4, 5) |
| **Branch / base** | `feat/sym-custody` off `dev` @ `c4cb3730` (PRs 1/2/3/4/5/6/7/8/11/16 in; the level-4 batch gate GREEN on `07375bd7`) — `0c187dbd`..HEAD, every contract red-first; built in parallel with PR 7b (directory striping) and PR 10 (dead-appender recovery) under the level-5 shared-file laws (§8) |
| **Contracts** | [`tests/sym_custody_tests.rs`](../tests/sym_custody_tests.rs) — 7 custody contracts + the 2 translator pins (+ 1 `#[ignore]`d scoping instrument); the PR-9 variants in the `token_call_frame` fuzz target + the proptest mirror in [`tests/decoder_property_tests.rs`](../tests/decoder_property_tests.rs); the suite appended to [`tests/run_sym_forest_suites.sh`](../tests/run_sym_forest_suites.sh) (34 suites) |
| **Knobs** | none (the brief's law) |
| **Venue** | dev box for every number here (**scoping** — debug build, tmpfs-backed temp files, both "nodes" in one process, two other implementers building on the same box); no squeeze-test row — the 2026-09-14 venue directive puts this rung on the LOCAL re-gate, the box bracket belongs to gate 1 / PR 13–14 |

## 1. What landed

Under bit 17 **and** `SQUEEZEFS_SYMMETRIC_META=1` (PR 4's plane) the S9 write-custody protocol — JOIN / ACQUIRE / RENEW / RELEASE, the `T_self` self-fence, the custody epoch (`src/data_grant.rs`) — is served, for an inode object, by the appender leasing the object's SLOT instead of the set authority; the acquire rides PR 5's token wire so ONE round trip answers custody AND the file's records; a slot with live custody on it does not hand over until the writer releases; W1 declines a foreign-custody file; own files pay nothing. **Unarmed (`=0`) and bit-17-absent mounts take the S9 paths verbatim** (§2). Beside it: PR 7's owed no-re-Put law at the two BACKEND-side translators (§1.5 — where the kvmap train's turned out to be a LIVE shape), and the §5.4.3 `pack_group` adjudication (§1.6 — the wire stays).

### 1.1 Where the server is (`data_grant.rs`, `dlm_slot.rs`)

- **The resolver is PR 6's** (`crossvol_tx::step_home`: tree 0's lease table compared with `own_appender_id()` — region 0's — and the `SlotHolderCache`'s endpoint table): `data_grant::slot_holder_home(ino)` answers `None` unarmed and for every own-slot / unleased object (the manager maintains an unleased tree — KD-SYM-2/3), `CustodyHome::Holder { holder, endpoint, volume, object }` for a slot another appender leases with a bound endpoint, `CustodyHome::Unbound { holder }` for one without (the join ladder's census binding is PR 12's; a dead holder's slots are PR 10's) — refused loud, naming both. The lease TABLE decides, not the S4 lock table: a declared region's slot is another appender's for every cross-owner decision (PR 6's two-holder model in one process), which is what makes the mechanism pinnable here.
- **The two acquire entry points** (`SlotLockManager::acquire_lock_mode` / `acquire_lock_range`) consult `slot_holder_home_of_path` FIRST — one relaxed `ArcSwapOption` load on every unarmed mount — and a holder's answer is `dlm_rpcs += 1` (S4's LOCK-round-trip meaning, the site's own) + `acquire_at_slot_holder` / `acquire_range_at_slot_holder`; everything else falls through to the S4 table's `is_local_slot` and the shipped S9 `acquire_remote`.
- **One JOINed `WriteCustodyClient` per holder endpoint** (`SlotCustodyArm::holder`, single-flight under an ASYNC mutex held across the dial: a second JOIN at one holder REPLACES the first lease and revokes its grants — the S9 re-join law), its renewal cadence spawned on the `sqz-lease` lane (`cowriter::spawn_custody_renewal`, the co-writer arm's own), plus one `TokenReaderPlane` per `(holder, volume)` (its standing recall channel started, the mount's `MountRecallSink` installed, the holder probed once at the dial — a holder that serves no tokens is found there, never at a later serve). The plane's arm is `arm_slot_custody(routed, node_id, secret, pr_key, sink_for)`; the clean leave `disarm_slot_custody` releases every plane's tokens (the drain + purge before the holder is told) and flushes the queued custody releases; `uninstall_slot_custody` is the teardown after a panic (the death shape).
- **The mount path** arms it as ONE call after PR 8's allocation arm (`main.rs` → `arm_mount_slot_custody`): iff some volume leases slots, with this mount's KD-MW-2 member id (`cowriter::node_member_id_of` over the appender page's identity — the string every plane knows it by), the set's cluster secret (`membership::cluster_secret` — none ⇒ one WARN and no arm: no holder is dialable without a cluster listener, and a foreign file's acquire then meets the S9 refusal naming the multi-writer mount), the per-volume `MountRecallSink`; the registrant key travels as 0 (the S9 `connect` default) until PR 12's join ladder carries the per-namespace key.

### 1.2 The grant carries the token (`meta_ship/token_plane.rs`, `data_grant.rs`)

- **The wire**: `TokenCall::CustodyGrant { object, span, concurrent_write, wait_ms, lease_epoch }` appended to PR 5's enum (a `// PR 9` block; `TOKEN_SCHEMA` 1 unchanged — the program's unreleased wire under `CLUSTER_WIRE_SCHEMA` 5; `ManagerCall` untouched, per the level-5 rule) and two replies at the END of `TokenReply`: `CustodyGranted { grant: GrantRecord, records: TokenRecords, already }` (the S9 grant record verbatim — the same word the custody wire's `AcquireReplyFrame` carries) and `CustodyRefused { status, reason }` (the custody wire's own status word, so the writer runs the S9 refusal ladder verbatim — `CUSTODY_UNKNOWN_LEASE` → `note_lease_lost`). `object` is the frame volume's LOCAL key ino; the holder derives the GLOBAL ino for its arbiter (`global_ino_of`: `split_guest_local` + the native routing slot + `make_global_ino_width` over the published width) — **one wire word, never a second to trust** (the wire-word law); a control record (raw local < 2) refuses.
- **The holder's arm** (`TokenHolderPlane::serve_custody_grant`): the `foreign_slot_holder` redirect first (`NotHolder { holder }`, `dlm_token_not_holder_redirects`), the S9 arbitration on the holder's INSTALLED custody authority (`custody_owner()` — none ⇒ a loud `Refused` naming the owner half) under the caller's lease at it, THEN the records under the grant ∥ pass gate (`serve_grant` — register before read, PR 5's law: a commit on the object recalls the caller's token whether it landed before or after the read). Custody first because the arbiter's `inode_{ino}` lease is what keeps the holder's own writes off the file while the caller holds it; a token that cannot be served (`Gone`, a refused read) RELEASES the custody just taken (`owner.release`), so the writer adopts both words or neither.
- **The writer's arm** (`WriteCustodyClient::acquire_carrying_token`): one `call_once` on the workload session with `VERB_TOKEN_CALL`, the reply's schema + correlation id checked, the grant adopted through `adopt_grant` — the ONE adopt both carriers now share (factored out of `acquire`: the client handle, `adopt_custody_generation`, `adopt_remote_grant`, `GRANTS`) — and the records installed through `TokenReaderPlane::install_carried(object, records, gen0)` under the fetch's own generation law (`gen0` read before the call; a recall that landed inside the round trip installs nothing — the holder's channel already retired it and the next serve re-fetches). Never retried on a transport failure (the S9 acquire's law). A `NotHolder` is re-resolved ONCE through the cache's endpoint table (tree 0 moved under the writer's view); a second refuses loud.
- **Gauges** (`data_grant::stats_json` — the `dlm_custody` object): `dlm_custody_via_slot_holder` (grants acquired from a holder), `dlm_custody_token_carried` (those installed as this writer's token; ≤ the former — the difference is grants whose token a recall retired mid-flight). 0 unarmed and for every own file.

### 1.3 Custody across a handover — DEFERRED, never spanned (`data_grant::slot_custody_live`, ONE call in `release_slot_handover_locked`)

§5.1.4 asks which: *recalled before the flush-then-transfer completes, or carried to the new holder*. As built: **neither moves a live grant — the handover DEFERS.** A slot whose files a writer holds custody of from this holder (one `grants_snapshot` scan routed through `route_ino` — O(live grants) per handover decision, a rare-cadence act) answers `KvError::Busy` at the top of `release_slot_handover_locked`, the class the slot-lease cadence already retries at its next tick and a requester's offer stands under; the moment the writer releases (the S9 release verb lands at the holder's authority) the slot moves, and the file's next custody comes from the NEW holder through tree 0. Why not recall: the S9 channel to the writer is PULL-based by design (no owner→client push; a holder-initiated per-grant recall would be a protocol change, and the brief's law is *"you change WHERE the server is, not the protocol"*). Why not carry: a carried grant would sit in a new holder's arbiter with no lease behind it and a fencing token another authority minted. A live grant is live work — the same spirit as "a live holder is never recalled by a touch". Consequence pinned: a grant never spans a handover, so no write lands under a stale holder's custody and no acked write is lost (the writer's DMA under the grant completed before its release; nothing was in flight across the move).

### 1.4 A dead holder; W1 (`routing.rs`)

- **A dead holder**: the writer's lease at it runs the S9 law unchanged — at its own `T_self` (strictly before the holder's TTL by the S6 formula) the writer POISONS its custody (`MemberSession::self_fence` → PR 8's `FenceClass::RemoteCustody`; the appender PARK is `self_fence_as(SymmetricAppender)`'s alone, the membership renewal tick's), so no DMA lands after the holder may have re-granted; the holder's sweep at its TTL retires the grant. The holder SIDE — a dead holder's slots re-leased, its ring recovered — is PR 10's; nothing here needs it (the brief's stop-and-report clause was not reached: the writer side is complete under the S9 law).
- **W1 declines a foreign-custody file** — `DataRouter::sole_owner_durably`'s first armed clause: a file whose custody this mount holds from its slot holder never patches in place, whatever the probe would read — the block's references live in the HOLDER's tree (another process's RAM in production; the in-process model can read it, which is why the clause is a decision, not a probe failure) and the patch retires a lifetime that tree accounts. The CoW path, counted on `patch_ineligible_shared` at the caller exactly like PR 7's SHARED clause; an own file keeps PR 7's durable probe (sole ⇒ patch; a durable SHARED bit ⇒ CoW — both re-pinned beside it). This is the co-writer posture's W1 law (`patch_ineligible_posture`) stated per FILE under the plane.

### 1.5 PR 7's owed pair cancellation at the two BACKEND-side translators (`kv/backend.rs` — `0c187dbd`)

PR 7 (review Issue 18b) left `cancel_same_reference_pairs` unapplied at `KvMetaBackend::recompute_refs_against_map` (the owner's compose of a co-writer's shipped frame) and the kvmap train's `resolve`, stating both *"unreachable on PR 7's surface … a kvmap map never carries a decorated `bk:off:len` re-description (its entries are whole-block keys)"*. **The train's is a LIVE shape**: a run's start re-adopted as a POINT at the same block (the claims train's `start_adopt` arm — `displaced.push((start, run, 0)); ref_takes.push((start, want))`, both resolving `entry_key` "start") is a legal per-index re-description of an unchanged binding, and on the base it staged `released r ‖ taken r` — a re-Put from scratch (the SHARED bit stripped, C16) AND `r` in the train's `released` set, so the post-commit ladder FREED a block the record still held. Red-first pins: `the_owner_side_translator_stages_no_op_for_a_re_described_reference` (a bare resolver; the clip stages nothing, a move stays two ops) and `the_kvmap_trains_resolve_keeps_the_shared_bit_across_a_re_description` (a FLAT kvmap volume — layout-blind: a run of 4 established, `taken_shared(r0)` committed, the claims train re-adopts index 0 as a point at the same block → the base panicked at *"a re-described reference never enters the free stream: [BlockRef { block_idx: 0 … }]"*; now `released` is empty, the probe reads `(1, shared = true)`, the run dissolved with its tail intact). Both translators run the ONE law after their pairs are built; the train's cancelled pair also leaves `released` / `released_keys`. `recompute_refs_against_map` + `RecomputedFrame` are `pub` for the pin (a contract accessor).

### 1.6 The PK4 `pack_group` wire — adjudicated: it STAYS

§5.4.3 says *"deleted … a shipped pack group has no consumer"*; the brief asks it adjudicated against the byte-identical law. `tests/mw_cowriter_pack_tests.rs` (thirteen contracts, PK4) pins that the UNARMED S9 co-writer packs THROUGH the authority today: `prepare × N → commit_group → seal`, one `pack_group` frame per pack (publish schema 17), `PUBLISH_PACK_GROUP_UNAVAILABLE` / `_SPLIT` refusals, `Grant::pack_group_available`. The wire lives in `routing.rs` (`commit_pack_group`), `publish.rs` (the frame flag, the served arm), `membership.rs` (the grant flag), `meta_backend/mod.rs` — not `pack.rs` as the brief's row says — and nothing of it is dead: "delete only what the unarmed path never reaches" deletes nothing. An ARMED symmetric mount is never a co-writer (the plane arms on the D0 winner; a co-writer opens through `open_co_writer` and never leases slots) and its promotions pack in its own `(meta volume, forest slot)` scope (PR 7), so it never issues a `pack_group` frame — pinned as `pack_cowriter_frames` flat on flat / dark / solo-armed mounts. The wire retires with the co-writer posture at PR 12/14 — PR 6's D19/D20-arms precedent. No `#[allow]`, no stub; the suite is not re-scoped (every contract describes shipped unarmed behaviour).

## 2. The unarmed / flat law

`=0`, a bit-17-absent volume and an armed SOLO mount: `slot_holder_home` answers `None` in one relaxed load (no arm installed / every slot the mount's), the two acquire entry points fall through to the S4 table verbatim, `dlm_rpcs` and the `dlm_custody` family stay flat across a create + a lock + a block publish, the two PR-9 keys read the flat ledger, no `pack_group` frame leaves the mount (`unarmed_and_flat_mounts_arm_nothing_and_count_nothing`, both layouts + solo). The S9 suites — `dlm_multi_writer_tests`, `dlm_data_fence_tests`, `mw_cowriter_pack_tests`, `mw_cowriter_free_tests`, `dlm_slot_lock_tests`, `dlm_cowriter_tests` — run green on BOTH legs at ratio 0.99–1.06 (§4). The mount arm without a cluster secret arms nothing, loud (`the_mount_arm_needs_a_cluster_secret_and_disarms_clean`).

## 3. Contracts (red → green)

| Contract | RED on the base | Landed |
|---|---|---|
| `the_owner_side_translator_stages_no_op_for_a_re_described_reference` | `left: [released 7, taken 7, released 8, taken 9]` — the clip staged a pair | `0c187dbd` |
| `the_kvmap_trains_resolve_keeps_the_shared_bit_across_a_re_description` | *"a re-described reference never enters the free stream: [BlockRef { block_idx: 0, owner_ino: 2, block_index: 0 }]"* — the base FREED the record's block | `0c187dbd` |
| `an_own_file_costs_no_rpc_and_a_foreign_file_exactly_one_grant_with_its_token` | no arm (`slot_holder_home` did not exist; a foreign-slot acquire met `acquire_remote`'s "no client armed" refusal) | `52637fbe` |
| `the_holders_publish_recalls_the_writers_token_and_the_writer_refetches` | same | `52637fbe` |
| `w1_declines_a_foreign_custody_file_and_keeps_the_durable_probe_for_own_files` | `sole_owner_durably(foreign)` read `true` (one unshared reference in a tree this mount can read in-process) | `52637fbe` |
| `custody_defers_a_handover_of_its_slot_and_moves_with_it_once_released` | the handover moved the slot under a live grant | `52637fbe` |
| `a_writer_poisons_at_t_self_when_its_holder_stops_answering_and_never_parks` | no holder client to fence | `52637fbe` |
| `unarmed_and_flat_mounts_arm_nothing_and_count_nothing` | (the law pinned with the mechanism) | `52637fbe` |
| `the_mount_arm_needs_a_cluster_secret_and_disarms_clean` | (same) | `52637fbe` |

The fixture is PR 6's two-holder shape: one stamped volume, a file PRESET into forest slot 4 while the slot is the manager's (`create_with_rdev_preset`), the slot released to `Unleased`, the set reopened under `SQUEEZEFS_TEST_SYM_APPENDER_SLOTS=1:4` so declared region 1 takes it; the "venue" is one `RpcListener` serving `AsyncVerbRouter::with_custody(owner).with_tokens(TokenSetService).with_manager(…)` over the same backend (what PR 12's join ladder stands up on every writer), appender 1's endpoint bound on the plane, the S9 owner installed process-wide, the slot-custody arm installed with a probe sink. Every gauge is read as a DELTA (process-global counters; the suite's tests run on libtest's threads).

## 4. The matrix (both legs) and the ×10

- **The S9 suites + `sym_custody_tests`, flat then stamped** (`/tmp/grok-justin/pr9-logs/s9-both.log`): PASS both legs — `dlm_multi_writer_tests` 0.7/0.7 s (1.06), `dlm_data_fence_tests` 0.4/0.4 (0.99), `mw_cowriter_pack_tests` 8.4/8.4 (1.01), `mw_cowriter_free_tests` 44.0/44.2 (1.01), `dlm_slot_lock_tests` 0.9/0.9 (1.00), `dlm_cowriter_tests` 4.2/4.4 (1.05), `sym_custody_tests` 8.1/8.1 (1.00). No ratio NOTE.
- **`sym_custody_tests` ×10 stamped from zero**: 10/10 green (`/tmp/grok-justin/pr9-logs/x10-clean-*.log`; an earlier ×10 read 9/10 with run 1 a COMPILE race against the in-progress `test_drop_entry` seam — not a test failure, attributed and re-run from zero).
- **The full matrix** (34 suites, flat then stamped; `/tmp/grok-justin/pr9-logs/matrix-both.log`): see §4a below (filled from the run's summary table).

### 4a. Matrix summary

**34 / 34 PASS on both legs, no ratio NOTE** (`rc=0`; the flat leg ran FIRST and paid each test binary's compile, which is why 30 of the 34 ratios read below 1.0 — a build artifact of the leg order, not a layout effect; the four suites that compiled nothing new read 1.09–1.10 (`kv_tree_tests`, `kv_backend_tests`) or ≈ 1). `sym_custody_tests` 14.0 s flat (incl. its compile) / 8.4 s stamped.

| suite | flat | stamped | ratio |
|---|---|---|---|
| kv_tree_tests | 44.5 | 49.1 | 1.10 |
| kv_node_tests | 96.9 | 3.6 | 0.04 |
| kv_backend_tests | 142.0 | 154.1 | 1.09 |
| kv_journal_tests | 4.7 | 0.1 | 0.03 |
| kv_partitioned_append_tests | 4.7 | 0.2 | 0.03 |
| kv_leaf_merge_tests | 54.5 | 42.0 | 0.77 |
| kv_node_cache_coherence_tests | 5.8 | 0.4 | 0.07 |
| kvmap_tree_tests | 5.4 | 0.4 | 0.07 |
| kv_scale_tests | 39.4 | 31.9 | 0.81 |
| durable_block_refs_tests | 17.2 | 9.8 | 0.57 |
| fsck_tests | 20.3 | 14.9 | 0.73 |
| fsck_c9_tests | 9.3 | 2.8 | 0.31 |
| fsck_c10_tests | 10.5 | 4.0 | 0.38 |
| fsck_c12_tests | 10.2 | 3.6 | 0.36 |
| fsck_repair_tests | 17.8 | 12.1 | 0.68 |
| crash_contract_tests | 5.3 | 0.5 | 0.08 |
| crash_kill_tests | 9.1 | 4.6 | 0.51 |
| writer_scoped_staging_tests | 8.0 | 2.9 | 0.37 |
| readonly_mount_tests | 10.0 | 4.6 | 0.46 |
| meta_slot_migration_tests | 9.0 | 5.4 | 0.60 |
| pv_coordinator_tests | 9.5 | 4.5 | 0.48 |
| kv_smo_crash_completeness_tests | 31.4 | 24.3 | 0.77 |
| sym_appender_tests | 19.0 | 13.4 | 0.70 |
| sym_manager_tests | 12.8 | 6.2 | 0.48 |
| sym_fence_tests | 4.8 | 0.4 | 0.08 |
| sym_slot_transfer_tests | 40.8 | 34.0 | 0.83 |
| rename_lock_set_tests | 5.0 | 0.8 | 0.16 |
| sym_convert_tests | 25.7 | 20.5 | 0.80 |
| sym_pack_tests | 5.3 | 0.6 | 0.11 |
| sym_shared_refs_tests | 24.0 | 17.7 | 0.74 |
| sym_cross_owner_tests | 21.5 | 15.0 | 0.70 |
| sym_block_grant_tests | 24.5 | 19.4 | 0.79 |
| sym_coherence_tests | 69.4 | 62.5 | 0.90 |
| **sym_custody_tests** | 14.0 | 8.4 | 0.60 |

## 5. Scoping row (dev box, debug build, loopback — SCOPING, never acceptance)

`scoping_row_grant_rtt_with_and_without_the_carried_token` (`#[ignore]`d): 200 foreign-file acquires from the slot holder —

| Arm | Wall | Per op |
|---|---|---|
| **carried** (custody + records in ONE round trip — `CustodyGrant`) | 57.8 ms | **288.8 µs** |
| **uncarried** (the plain S9 `acquire` on the custody wire, then the token as its own `Grant`) | 82.7 ms | **413.3 µs** |
| handover deferral (a live grant on the slot) | | 36 µs — a refusal |
| release + the handover itself (PR 4's flush-then-transfer) | | 12.9 ms |

The carriage saves one loopback round trip per foreign first touch (≈ 125 µs here, ≈ 30 %); on a fabric it saves one RTT (≈ 250 µs at the fleet's measured RTT). The custody phases read `rtt` mean 131 µs, `arbitrate` 7.8 µs, `adopt` 5.1 µs. The handover's cost is PR 4's (the deferral itself is a microsecond refusal).

## 6. Deviations from the design, stated

1. **The grant rides the TOKEN wire, not the custody wire.** The design says "the custody grant CARRIES the file's read token"; the level-5 rule says PR 9's wire is PR 5's `TokenCall`. So the acquire is a `TokenCall::CustodyGrant` whose reply embeds the S9 `GrantRecord` (the custody wire's `AcquireReplyFrame` and `CUSTODY_SCHEMA` 7 are untouched; the notice carriage — demotions / shrinks — stays on the custody wire's carriers, which the JOINed client still runs).
2. **Custody across a handover DEFERS** (§1.3) rather than recalls or carries.
3. **The S11 ranged acquire at a holder rides the custody wire without records** (`acquire_range_at_slot_holder` → `acquire_range`): the token carriage is the whole-file grant's; a range holder reads under the S8 cache the S11 plane already rides (`SQUEEZEFS_RANGE_CUSTODY` is default-off).
4. **The writer does not yet CONSUME the carried records on its read path**: `KvMetaBackend`'s read verbs divert to a token plane on `-o ro` mounts only (PR 5's `arm_token_reader` refuses a writable open). PR 9 installs the records in the writer's per-holder plane, keeps them under the recall law, and pins the re-fetch through the plane; the divert of a writer's foreign-slot reads to that plane is PR 12's (where every writer is also a token client of the holders it writes to). In-process the writer's backend holds the declared region's tree, so its reads are exact either way.
5. **The holder-side S9 owner** (`WriteCustodyOwner`) exists on an armed writer only under `SQUEEZEFS_MULTI_WRITER=1` today; the join ladder (PR 12) arms it on every writer. The `CustodyGrant` service arm refuses loud without it.
6. **The registrant key on the holder JOIN travels as 0** (the S9 `connect` default) until PR 12 carries the per-namespace key.
7. **The `pack_group` wire is NOT deleted** (§1.6) — the design row's "deleted" is corrected to "adjudicated: stays until the co-writer posture retires".
8. **`kv/backend.rs` was touched** (the level-5 table says PR 9 adds nothing there): two one-block insertions (PR 7's owed law — the brief's explicit deliverable 5) + one call at the handover's entry + two visibility words (`pub`) for the translator pin. Listed in §8 for the rebase.

## 7. Owed

- PR 12: the writer's read-verb divert to its per-holder token planes (deviation 4); the holder-side owner on every writer (5); the registrant key on the JOIN (6); the endpoint binding from the census (today `Unbound` refuses loud); the `pack_group` wire's retirement with the co-writer posture; the un-share of a surviving sole owner (PR 7's).
- PR 10: a dead holder's slots re-leased (the writer side is complete: it poisons at `T_self`).
- Gate 1 / PR 13–14: the box bracket (the carried-vs-uncarried RTT row on a fabric; the solo re-gate with the arm installed).
- The instrumented fuzz run of `token_call_frame` with the PR-9 arms (nightly tier).

## 8. Shared-file touches (the level-5 laws)

- `src/data_grant.rs` (mine): the PR 9 block at the END (`CarriedAcquire`, `CustodyHome`, `SlotCustodyArm`, `arm_slot_custody` / `disarm_slot_custody` / `uninstall_slot_custody` / `arm_mount_slot_custody`, `slot_holder_home[_of_path]`, `acquire_at_slot_holder`, `acquire_range_at_slot_holder`, `slot_custody_live`); `WriteCustodyClient::{adopt_grant, acquire_carrying_token}`; the two statics + `ClientStats` fields + `stats_json` keys; `use std::collections::HashMap`.
- `src/meta_ship/token_plane.rs` (PR 5's wire — PR 9's to extend per the table): `TokenCall::CustodyGrant` + `name()`; `TokenReply::{CustodyGranted, CustodyRefused}`; `CustodyAsk` + `global_ino_of`; `TokenService::serve_frame`'s arm + the `CustodyRefused` status mapping; `TokenHolderPlane::serve_custody_grant`; `TokenReaderPlane::{recall_generation, install_carried, install_records (factored out of fetch_once), test_drop_entry}`.
- `src/dlm_slot.rs`: the PR 9 arm at the top of `acquire_lock_mode` and `acquire_lock_range` (ONE call each).
- `src/routing.rs`: one clause at the top of `sole_owner_durably`'s armed arm.
- `src/meta_backend/kv/backend.rs` (PR 10's — see deviation 8): `cancel_same_reference_pairs` in `recompute_refs_against_map` (1 line + comment) and in the kvmap train's resolve (the block after `ref_takes`, ≈ 15 lines); `slot_custody_live` at the top of `release_slot_handover_locked` (≈ 8 lines); `RecomputedFrame` + `recompute_refs_against_map` made `pub`.
- `src/main.rs`: ONE call (`arm_mount_slot_custody`) after PR 8's allocation arm.
- `fuzz/fuzz_targets/token_call_frame.rs`, `tests/decoder_property_tests.rs`: the PR 9 arms/strategies.
- `tests/run_sym_forest_suites.sh`: a comment paragraph + `sym_custody_tests` appended to `DEFAULT_SUITES`.
- `docs/operations.md`: the S9 section's armed-plane paragraph, the PR 9 section after PR 5's, the `dlm_custody` stats row. `AGENTS.md`: the PR 9 paragraph before "### Metadata-throughput program"; the S9 `dlm_custody` sentence amended. `docs/design-symmetric-metadata.md`: row 9 ONLY.
- `src/env_knobs.rs`, `src/fuse_client.rs`, `src/meta_ship/manager.rs`, `CLUSTER_WIRE_SCHEMA` — untouched.

## 9. The fidelity tier (`quick`, root, nvmet — the release binary at `1ca066e2`)

`sudo -n env … FIDELI_SQZ_BIN=$PWD/target/release/squeezefs bash tests/run_nvmeof_fidelity.sh quick` (`/tmp/grok-justin/pr9-logs/fidelity-quick.log`): **PASS=43 FAIL=0 in 1m37s** — `substrate-up` 1, `roundtrip-nvmet` 15, `pr-registrants` 7, `sym-manager-failover` 18 (the mount path with the PR-9 arm: 196/200 creates acked under the seam with the 4 refusals naming PR 6's cross-owner class — the leg's own expected shape; successor wall 1,408 ms against the 45,011 ms `manager_failover_bound_ms`; acked data byte-intact; WERO re-held by the successor; zero PR residue after the clean unmount), `guard-nvmet-x1` 1, `teardown-zero-residue` 1. The arm reads `Ok(false)` on that leg's mounts (no cluster listener ⇒ no `job:enroll` secret ⇒ the WARN and no holder dialed — §1.1), which is the honest posture: nothing of PR 9 engages without the wire, and the custody plane's device rows are untouched.
