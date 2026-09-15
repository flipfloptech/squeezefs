# Symmetric metadata program — PR 5: GPFS-strict read tokens (2026-09-15)

| | |
|---|---|
| **Design** | [`docs/design-symmetric-metadata.md`](../docs/design-symmetric-metadata.md) §5.7 (5.7.1 the read classes — own slot RAM-authoritative, a foreign object under a READ TOKEN whose grant CARRIES the records, the recall before the conflicting commit riding the 4a guard and batched per conveyor pass; 5.7.2 `-o ro` = member-reader + token client, `reader_staleness_bound_ms` = 0; 5.7.3 free-grace recall-driven, the ring as the timeout path; 5.7.4 the broadcast shape's price; 5.7.5 write locality), §5.8.2 (bset frame v2 `(appender_id, g)` + the three-rule screen), §5.8.6 (`dlm_token_recall_timeouts_live` must-stay-0), §5.1.6 (`SlotHolderCache` — the token server is the slot holder), §6.3 (the verbs), §11 the Token family + the free-grace faces, §1.6 rows "Token grant" / "Recall" / "Broadcast write", §8 gate 5; R-SYM-4 / KD-SYM-19 (GPFS-strict is the ONLY foreign-read method); PR plan row **5** |
| **Branch / base** | `feat/sym-tokens` off `dev` @ `6f8d44e6` (PR 1 forest, PR 2 appender region, PR 3 manager lease, PR 4 slot leases, PR 11 conversion, PR 16 SPDK retirement) — `ac5dd3a2`..HEAD, every contract red-first; one of four PRs built in parallel off this tip (PR 6 cross-owner tx, PR 7 pack + refs, PR 8 alloc lease) |
| **Contracts** | [`tests/sym_coherence_tests.rs`](../tests/sym_coherence_tests.rs) — 16 (part A: the six frame pins; part B: the ten token contracts); [`tests/readonly_mount_tests.rs`](../tests/readonly_mount_tests.rs) RE-SCOPED (+5: the token bound and TTLs, the knob-off arm, the flat and leaseless refusals, the predicted-slot ledger read); [`tests/reader_free_grace_tests.rs`](../tests/reader_free_grace_tests.rs) RE-SCOPED (+3: the recall-gated free, the ring as the timeout path, the unarmed gate); the frame codec mirrors in [`tests/decoder_property_tests.rs`](../tests/decoder_property_tests.rs) (`bset_frame_v2_walk_never_panics`, `bset_frame_v2_screen_matches_the_rule_function`) and the fuzz target `bset_frame_v2` |
| **Knobs** | **none new** (the design's row). Two EXISTING knobs gained a reader face: `SQUEEZEFS_SYMMETRIC_META=1` on a `-o ro` mount arms the token client; `SQUEEZEFS_MW_AUTHORITY` is the holder endpoint the reader dials (the co-writer's declaration, the S8 listener the token verbs ride). |
| **Venue** | dev box for every number here (**scoping** — debug build, tmpfs-backed temp files, both arms in-process, three sibling PRs building on the same laptop); no squeeze-test row: the 2026-09-14 venue directive puts this rung on the LOCAL re-gate, the box bracket belongs to gate 5 / PR 13–14 (§7). |

## 1. What landed

Under bit 17 **and** `SQUEEZEFS_SYMMETRIC_META=1` a foreign object is read under a READ TOKEN from its slot holder (an armed writer, PR 4's lessee), whose grant CARRIES the records; the holder recalls every reader's token BEFORE the conflicting commit lands — the recall rides the conveyor pass under the batch's 4a guards, once per pass over the batch's union, and the pass proceeds on every ack or the reader's membership lease expiry; a reader acks only after its in-flight serves drained and its block-key census was purged, and its next resolve re-fetches — **a foreign create is visible at the reader's NEXT resolve, exact, never bounded**. `-o ro` under the knob is a member-reader + token client with `reader_staleness_bound_ms` = 0; free-grace is recall-driven with the ring as the timeout path; under bit 17 every bset frame carries `(appender_id, g)` and both loaders screen foreign frames by the three rules; the control-plane poll reads the predicted 4 KiB ledger slot first. **`=0` and a bit-17-absent mount are the shipped posture EXACTLY** (§2).

### 1.1 The plane (`src/meta_ship/token_plane.rs`, NEW; `src/meta_ship/tokens.rs` extended)

- **The wire**: `TokenCall::{Grant { object, wants }, Recall { wait_ms }, RecallAck { frame_id }, Release { objects }}` — its OWN enum on verb block `0x0600` (`VERB_TOKEN_BASE`), `TOKEN_SCHEMA` 1 under `CLUSTER_WIRE_SCHEMA` 5 (unchanged — the program's unreleased wire; `ManagerCall` untouched, the level-4 rule). Replies `Granted { records, already } / NotHolder { holder } / Gone / Recall { frame_id, objects } / Acked / Released { count } / Refused`; frames bincode-encoded under the CONTROL cap. `TokenService` (one volume) / `TokenSetService` (dispatch by the frame's volume ordinal) ride the S8 owner listener through `AsyncVerbRouter::with_tokens` when any volume of the set holds tokens (`multi_writer.rs`, one appended block).
- **The grant carries the records** (`KvMetaBackend::token_records_for`): the inode record (`WireAttrs`), the carried xattrs (`token_carried_xattr` = `xattr_name_allowed` ∪ `layout` ∪ `system.symlink` — the control-plane names a grant never carries), a directory's dentry set paged by `grant_dentry_budget()` (the reply cap ÷ the max dentry record) with a cookie continuation — the S8 frame law. A re-grant to a holder answers `already` (the S10 `RecallLane::holds` witness).
- **The holder** (`TokenHolderPlane`): the S10 `RecallLane` in a new `ConfigMode::LiveTokens` whose recall DEADLINE is the reader's lease TTL by derivation (`RecallConfig::derived_with_evidence(None, false)` — never a measured p99: a process-wide evidence read made one suite's fast acks the next suite's 1 ms deadline, found by the serialized run); pending frames per client waiting for the reader's standing poll (`serve_poll` parks up to `poll_park_bound(deadline)` = `clamp(deadline / 4, RECALL_POLL_PARK_FLOOR, 1 s)`); the `inflight` set — a grant on an object whose recalled commit has not APPLIED parks (`dlm_token_grant_parks`), never hands out pre-commit records; the lease oracle defaults to `MembershipOwner::lease_deadline_ms(client)` (the contracts install their own).
- **The commit-path recall** (`recall_and_wait`): recall every outstanding token on the batch's objects ONCE, hand the frames to the readers' polls, wait on acks with the expiry sweep at a quarter-deadline tick; a frame past its deadline is classified by the reader's lease — `expired_with_lease` (the token died with the lease) or `timeouts_live` (a LIVE member did not answer: must-stay-0, `invariant_tripwires` label `dlm_token_recall_timeout_live`, `free_grace::note_recall_unacked_live()` opens the ring's window). `dlm_token_recall_rtt_ns` is exact-sum PER BATCH: `send` (the pass's call → the last frame handed to a poll), `drain` (→ the last ack's arrival), `ack` (→ the pass observing it), `total` — cuts clamped monotone into the batch window, pinned to the nanosecond by the broadcast contract.
- **The reader** (`TokenReaderPlane`): an scc token cache (`TokenEntry` — attrs, xattrs, the paged dentry set, a state word LIVE/REVOKED) bounded by the S10 R5 component with eviction = `Release`; single-flight fetch per object (`fetching`), paged dentries to completion, a fetch retried when a recall of the object landed mid-fetch (`dlm_token_fetch_retries`); a standing recall channel on its OWN session (`run_recall_channel` — a parked poll must never block a grant), whose ack runs the data-plane sink FIRST (`RecallDataSink::drain_and_purge`: the mount's `MountRecallSink` = `ro_coherence::drain_in_flight_serves()` + `purge_reader_block_keys`); `serve_gate` refuses every serve when the membership lease is past `T_self` or the channel is not fresh (`dlm_token_serve_refusals` — a reader that cannot be recalled serves NOTHING). `stop()` RELEASES every held token at the holder before stopping (§6 finding 1); `test_die()` is the dead-reader seam.

### 1.2 The hook, the diversions, the mount path (`kv/backend.rs`, `ro_coherence.rs`, `fuse_client.rs`)

- `KvMetaBackend::recall_tokens_for_batch(&[QueuedTx]) -> Vec<u64>` — ONE fn; ONE call at the top of `run_batch_group` before `ring_of_region` (under the batch's 4a guards, before ring admission and any node lock — §4.4 pt 5 + the lock law); `plane.settle(&recalled)` after `run_batch_pipeline`. The union is taken over `record_object_ino(kind, key, forest)` (the ino sits first in every ino-major key; refs by their owner). `KvTx.token_quiet` (set by `setxattr_locked` / `removexattr_locked` for a name a grant never carries) exempts control-plane writes — `writer_claim`, jobs, the membership records — so an idle writer's heartbeat recalls nobody.
- The read verbs divert to the cache on a token reader: `find_dentry` → `token_find_dentry`, `getattr` → the token's attrs, `readdir_page` → the token's dentries filtered by cookie, `getxattr` (carried names) / `listxattr` → the token's xattrs; `filters_unpublished_children()` is false on a token reader (the holder's dentry set is complete by construction).
- **`-o ro` = member-reader + token client** (`ro_coherence::arm_token_readers`, ONE call from `fuse_client::init` beside the S5 arms): under `token_reader_requested()` (the read-only latch AND the knob) every read-only volume arms against the DECLARED set authority (`cowriter::declared_authority()` = `SQUEEZEFS_MW_AUTHORITY`) with the membership member id (`installed_member().id()`) as the grant key and the cluster secret (`job:enroll`); refused LOUD — the mount fails — on a bit-17-absent volume (naming `enable-symmetric`), without a membership lease (naming `SQUEEZEFS_MEMBERSHIP_BIND`), without the endpoint, without the secret. `=0` returns `Ok(0)` and arms nothing.
- **Every reader TTL derives to 0 under tokens**: `ro_coherence::metadata_staleness_bound()` (ZERO when requested, else the S5 bound) feeds `KernelCacheTtls::read_only_defaults` and `reader_daemon_cache_ttl` — a cache may hold an entry exactly as long as freshness is proven, and under a token that is "until the recall", which no TTL expresses (moka's zero TTL expires at insert; the attr cache's `now − inserted ≥ 0` never admits) — so every resolve reaches the token cache. `metadata_staleness_bound_ms(volumes)` = 0 by request or by an armed plane — the stats face `reader_staleness_bound_ms`.
- A token reader's `shutdown()` (the reader arm) calls `tokens.stop()` — the clean leave releases its grants.

### 1.3 Recall-driven free-grace (`free_grace.rs`'s recall-gating arm; `block_allocator.rs` ONE call)

`RECALL_GATE` + `RECALL_GATED_FREES` + `TIMEOUT_DEFERRALS` + `RECALL_UNACKED_UNTIL_MS`; `RecallGate::{Off, Gated, Deferred}`; `arm_recall_gate()` (the holder's arm — `arm_token_holder` calls it) / `disarm_recall_gate()` / `recall_gate_verdict()` / `note_recall_unacked_live()` (window = now + `T_owner` on the owner's clock) / the two accessors / the seam `test_open_recall_window_ms`. `BlockAllocator::finish_free` asks the verdict FIRST (after S7's quarantine): `Gated` → `publish_free_list` directly; `Deferred` or `Off` → the ring exactly as before. The epoch fan-in stays live code (§6 deviation 1). PR 8 owns the ring's per-volume split in the same file; nothing in the ring's data structures was touched.

### 1.4 bset frame v2 + the screen (`kv/node.rs`, `kv/node_cache.rs`, `kv/tree.rs`; commit `ac5dd3a2`)

`BSET_FRAME_VERSION_V2` = 2, `BSET_FRAME_V2_LEN` = 40 (`magic ‖ version ‖ reserved ‖ node_seq ‖ padded_len ‖ bset_len ‖ appender_id: u32 ‖ g: u32 ‖ checksum`); `FrameStamp { appender_id, g }` (the manager's = `(0, 0)`); the version is the LAYOUT's (`NodeLayout::new_symmetric`, `frame_len()`, `symmetric_frames()`; `probe_frame` admits only the layout's version — a v1 frame is foreign on a v2 layout and vice versa), the stamp a per-write copy (`NodeLayout::stamped`, identity on v1). `FrameScreen { g_current, recorded_tail: Option<(g, tail)>, pr_fenced }::foreign_rule(stamp, pos, prev_g) -> Option<u8>` is the pure three-rule function; `load_node_screened` / `verify_node_extent_screened` apply it on the load walk (rule 3 alone without a screen), `probe_tail_page(page, node_seq, layout) -> TailPage::{Clear, Ours, Foreign(stamp)}` on `append_frozen`'s destination page; a screened stop counts `foreign_frames_screened` (or `appender_fence_breach` + the tripwire for rule 1 under `pr_fenced`) and the walk scans past it for a CURRENT-generation frame → `foreign_frame_overwrite_detected` + `Err(Corrupt)`. `NodeCache::FrameFenceSource` is the ONE source (`stamp_for(slot)`, `screen_for(slot, addr)`); the backend's `SlotFrameFence` answers off the lease table's `g` and tree 0's `slot_tails(slot)` through a per-slot per-generation cache (`plane.frame_tails`), `pr_fenced` = `meta_pr_wero`; installed in `arm_slot_leases` before the gate arms. Trees load through `load_for_slot`; SMO successors / merges / fresh roots write under `write_layout_for{,_slot}`; the builder and PR 11's conversion write v2 (the plan prices the layout it builds). `LoadedNode::foreign_frames_screened` is the per-load count; `encode_header_page(layout, params)` is the forger's door (the fuzz target and part A).

### 1.5 The predicted-slot-first ledger read (`kv/revalidate.rs`)

`read_newest_ledger_from(path, base, adopted_seq)`: the writer places checkpoint `seq` in slot `seq % 32`, so the record after the one the reader adopted sits in ONE slot — read it, walk successors while each holds the next seq (a writer that raced ahead by more than the ring lands a higher seq of the same residue there, adopted the same way), stop at an older record or an all-zero (never-written) slot; a slot that decodes as neither (TORN — the writer mid-write, or damage) or a bit-8 partitioned record takes the whole-ledger fallback (`full_ledger_read`, counted). `read_root_epoch` uses it from `NodeCache::reader_adopted_seq()` (the epoch IS the adopted seq); nothing newer restates the adopted epoch (`RootEpoch::synthetic(seq, 0, &[])` — a no-op for `revalidate_trees`). Gauges `meta_kv_revalidate_ledger_read_bytes` / `meta_kv_revalidate_ledger_full_reads`.

## 2. The negative contract — `SQUEEZEFS_SYMMETRIC_META=0` and a bit-17-absent volume are untouched

`symmetric_meta_off_carries_no_token_plane`: a stamped volume opened without the knob and a flat volume each hold no token holder / reader, the recall gate reads `Off`, `foreign_frames_screened` / `recall_gated_frees` / `timeout_deferrals` do not move, a read-only open serves the S5 bound; `frame_v1_stays_byte_identical_and_the_two_versions_are_foreign_to_each_other` pins the shipped 32 B header field by field; `the_mount_path_token_arm_is_the_s5_reader_verbatim_when_the_knob_is_off` pins the mount arm's `Ok(0)`; `an_unarmed_token_gate_leaves_the_ring_in_charge_and_moves_no_face` pins the free path. The flat leg of the matrix (§4) is the whole pre-PR-5 KV contract set on the shipped path.

## 3. Measurements (dev box — SCOPING; both arms in-process; debug build)

| Row | Value | Instrument |
|---|---|---|
| **Broadcast shape** — 1 writer × R = 64 in-process readers of one file, one `setattr` publish | `dlm_token_recall_batches` +1, `dlm_token_recalls` +64, `dlm_token_recall_acks` +64, `dlm_token_recall_timeouts_live` 0, **`dlm_token_recall_fanout` p99 = 64** (≡ the reader count); every reader reads the new mode at its next resolve | `the_broadcast_shape_recalls_every_reader_once_per_publish` |
| **Recall RTT (the batch above)** — `dlm_token_recall_rtt_ns` exact-sum | `send` 1,384,804 ns + `drain` 401,135 ns + `ack` 514,468 ns = **`total` 2,300,407 ns (2.30 ms)** for 64 readers; the sum is pinned exact | same, `--nocapture` (the `SCOPING` line) |
| **`free_grace_hold_ms` under tokens** (§5.7.3: the free's hold IS the recall RTT) | **≈ 2.3 ms** (the batch above; `free_grace_recall_gated_frees` counts the free, no ring residence) vs the **2,724 ms** epoch-fan-in composite of [`.benchmarks/2026-09-06-free-grace-hold-time.md`](2026-09-06-free-grace-hold-time.md) (fleet-cadence model after the ladder re-derivation) — three orders of magnitude, in-process loopback, dev box | `a_free_never_ships_before_every_recall_is_acked_or_expired` + the broadcast row |
| **The control-plane poll's ledger read** | idle poll **4 KiB** (one predicted slot; was 128 KiB — 32× fewer bytes and one I/O instead of one 128 KiB I/O); after `k` = 2 writer checkpoints **12 KiB** (3 slots); a torn predicted slot **4 KiB + 128 KiB** (the fallback, `meta_kv_revalidate_ledger_full_reads` +1) and the next checkpoint is found through it | `the_poll_reads_the_predicted_slot_first_and_falls_back_on_a_torn_one` (bytes asserted exactly) |
| **Recall storm on one object** — 16 concurrent conflicting commits co-queued behind a held pass, 1 reader holding the object | ONE recall batch, the reader recalled once, every commit lands after the ack | `a_recall_storm_on_one_object_is_one_batch_per_pass` |
| **Lease expiry** — a dead reader (`test_die`) holding 64 objects, a commit on each | every recall completes at the lease's expiry: `dlm_token_recall_expired_with_lease` +64, `timeouts_live` 0, the commits land, no ring deferral | `an_unacked_recall_completes_at_the_readers_lease_expiry` |
| **The `-o ro` mount-path arm** | the arm returns 1 volume armed, the first `lookup` is two grants (the root's dentry set, the child's record), `reader_staleness_bound_ms` 0, the volume image byte-identical across the reader's session | `a_ro_mount_under_the_knob_arms_the_token_client_and_writes_nothing` |

Not measured here (owed to gate 5 / the box bracket): grant RTT p99 and recall-ack p99 at N = 32 wire members, the `sym-readers` rig row, `free_grace_hold_ms` on a fleet cadence.

## 4. Verification

- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo clippy --all-targets --all-features -- -D warnings`: clean at every commit.
- `cargo test --test sym_coherence_tests -- --test-threads=1`: 16/16 green flat; **×10 stamped from zero: 10/10 green**, 25.3–27.2 s each (`/tmp/grok-justin/pr5-x10-stamped.log`).
- Touched-neighbour suites green (serialized): `readonly_mount_tests` 33, `reader_free_grace_tests` 64, `mw_recall_valve_tests` 12, `sym_slot_transfer_tests` 45, `dlm_membership_tests` 51, `dlm_cowriter_tests` 18, `kvmap_read_tests` 10, `kv_node_cache_coherence_tests` 21, `mw_delegation_tests` 14, `mw_intent_batch_tests` 20, `mw_slot_placement_tests` 9, `sym_forest_tests` 31, `env_knob_convention_tests` 22, `docs_parity_tests` 5, `derivation_sweep_tests` 54; the frame commit's leg (`sym_forest` / `sym_appender` / `sym_convert` / `sym_manager` / `crash_contract` / `kv_node` / `kv_finding_a` / `decoder_property`) green both ways.
- The matrix `tests/run_sym_forest_suites.sh` (29 suites, `sym_coherence_tests` appended) both legs: **PASS / PASS** (§4a). The live-FUSE trio under `SQUEEZEFS_TEST_REQUIRE_MOUNT=1` both ways: §4a.
- `cd fuzz && cargo check --bins && cargo fmt --check`: clean (the `bset_frame_v2` target).
- The fidelity tier was NOT run: `reservation.rs` / `data_custody.rs` / `nvmeof` are untouched.

### 4a. Matrix + trio results

Both legs **PASS** (29 suites, `sym_coherence_tests` appended; first run from zero red at `kv_node_cache_coherence_tests` stamped — §6 finding 8, fixture fixed, the whole matrix re-run from zero), no ratio NOTE (`SQZ_SYM_RATIO_NOTE` 2.0; the highest, 1.46 on the 4 s `meta_slot_migration_tests`, is scheduler noise on a box three sibling PRs were building on; `sym_coherence_tests` 24.9 s flat / 24.8 s stamped — 1.00). Log: `/tmp/grok-justin/pr5-matrix.log`.

| suite | flat (s) | stamped (s) | ratio |
|---|---|---|---|
| `kv_tree_tests` | 53.5 | 45.7 | 0.85 |
| `kv_node_tests` | 0.2 | 0.1 | 0.94 |
| `kv_backend_tests` | 135.3 | 149.2 | 1.10 |
| `kv_journal_tests` | 0.1 | 0.1 | 1.18 |
| `kv_partitioned_append_tests` | 0.1 | 0.1 | 1.01 |
| `kv_leaf_merge_tests` | 40.2 | 43.1 | 1.07 |
| `kv_node_cache_coherence_tests` | 0.4 | 0.5 | 1.27 |
| `kvmap_tree_tests` | 0.4 | 0.4 | 1.08 |
| `kv_scale_tests` | 31.8 | 36.6 | 1.15 |
| `durable_block_refs_tests` | 10.0 | 11.1 | 1.11 |
| `fsck_tests` | 12.5 | 13.9 | 1.12 |
| `fsck_c9_tests` | 2.6 | 2.8 | 1.08 |
| `fsck_c10_tests` | 3.9 | 4.1 | 1.05 |
| `fsck_c12_tests` | 4.1 | 3.7 | 0.91 |
| `fsck_repair_tests` | 17.3 | 12.4 | 0.72 |
| `crash_contract_tests` | 0.4 | 0.5 | 1.24 |
| `crash_kill_tests` | 4.5 | 5.0 | 1.12 |
| `writer_scoped_staging_tests` | 2.8 | 3.0 | 1.05 |
| `readonly_mount_tests` | 4.5 | 4.7 | 1.05 |
| `meta_slot_migration_tests` | 4.0 | 5.8 | 1.46 |
| `pv_coordinator_tests` | 4.0 | 4.8 | 1.21 |
| `kv_smo_crash_completeness_tests` | 30.7 | 19.0 | 0.62 |
| `sym_appender_tests` | 13.6 | 13.6 | 1.00 |
| `sym_manager_tests` | 6.5 | 6.5 | 0.99 |
| `sym_fence_tests` | 0.4 | 0.4 | 0.94 |
| `sym_slot_transfer_tests` | 34.5 | 33.8 | 0.98 |
| `rename_lock_set_tests` | 0.8 | 0.8 | 0.98 |
| `sym_convert_tests` | 21.6 | 20.9 | 0.96 |
| `sym_coherence_tests` | 24.9 | 24.8 | 1.00 |

**The live-FUSE trio under `SQUEEZEFS_TEST_REQUIRE_MOUNT=1`, both ways:**

| suite | flat | stamped |
|---|---|---|
| `posix_mount_semantics_tests` | 3 passed, 39.2 s | 3 passed, 39.5 s |
| `corpse_sweep_tests` | 4 passed, 172.9 s | 4 passed, 155.8 s |
| `inline_raise_tests` | 7 passed, 112.0 s | 7 passed, 101.6 s |

`/dev/fuse` present, `fusermount3` on PATH, `fuse.enable_uring=Y`; every mount-class contract RAN (`SQUEEZEFS_TEST_REQUIRE_MOUNT=1` turns a self-skip into a failure). Log: `/tmp/grok-justin/pr5-trio.log`.

## 5. Where a contract needed PR 6/7/8/10/12 machinery — stopped and stated

- **The live-FUSE `-o ro` token mount.** The token verbs ride the S8 owner listener, which the mount path builds only under the multi-writer arm (`SQUEEZEFS_MULTI_WRITER=1` — refused on a non-PR substrate); on this laptop no product listener exists for a reader to dial, so the mount-path arm is proven IN-PROCESS (`a_ro_mount_under_the_knob_arms_the_token_client_and_writes_nothing`: an armed writer, its `TokenService` on a listener, a member session installed, `ro_coherence::arm_token_readers` — the one call `fuse_client::init` makes — on a routed read-only open of the same file). N daemon processes on one volume and the appender-side dial are PR 12's.
- **The holder → endpoint binding.** §5.1.6 resolves the token server through tree 0 + `SlotHolderCache`; the cache answers the appender id and `g`, and the id → member identity → endpoint binding is PR 12's join ladder (the cache's own doc says so). The smallest declared seam: the reader dials the DECLARED set authority (`SQUEEZEFS_MW_AUTHORITY`) as every volume's holder — correct under the one-writer-per-volume shape every mount today has; `NotHolder { holder }` is on the wire for PR 12's redirect.
- **Per-stripe tokens** (§5.7.1 "per STRIPE for a striped directory") are PR 7b's — a stripe ino is just another object here.
- **The death path's tail recorder** (§5.8.2's "on the death path the recoverer records EVERY leaf's tail") is PR 10's driver; the screen and `KvMetaBackend::frame_screen_for` (the ONE accessor) are built for it.

## 6. Issues found on the way (attributed before any harness bound moved)

1. **The writer's unmount waited a whole recall deadline for a DEAD reader's root token** (dark path): the `writer_claim` leave on ino 1 recalled the reader's token on the root; the reader's channel had stopped, so the pass parked to the deadline. Two fixes, both pinned: control-plane writes are `token_quiet` (a grant never carries them, so they recall nobody), and `TokenReaderPlane::stop()` RELEASES its held tokens at the holder before stopping (the mount's `shutdown` reader arm calls it).
2. **The lease-expiry contract read `recall_acks` = 877 for 64 acks**: `RecallLane::ack_frame` returned nothing, so the plane recomputed acks from a stats diff; it now returns the acked count.
3. **A process-wide recall-RTT evidence read** made the deadline p99-derived across suites — a second test's fast acks turned the next test's deadline into ≈ 1 ms and every lease-expiry contract red in the serialized run. `ConfigMode::LiveTokens` derives the deadline from the lease TTL alone; the `TOKEN_RECALL_RTT` evidence static is deleted.
4. **`dlm_token_recall_rtt_ns` was not exact-sum**: the first build recorded `send` / `drain` per FRAME (64 samples) and `total` per batch — Σ(send + drain + ack) read 90.8 ms against a 2.7 ms total. The phases are now per-batch cuts on one clock, clamped monotone, pinned to the nanosecond.
5. **`record_object_ino`** carried two `.expect("8-byte slice")` in library code — replaced by a fallible slice → array conversion.
6. **A `-o ro` zero-writes pin over a LIVE writer must settle the writer first**: a checkpoint's tail releases the pending frees it covers AFTER its bitmap pages landed, so the next cadence cycle writes them (the PR-11 shipped-bug fix closed the clean-UNMOUNT face; the live face is legitimate). The contract samples the image one checkpoint landing ceiling apart until two samples agree, then measures the reader alone — the first 500 ms settle window sat inside a quiet gap between cycles (1 in 3 red).
7. **The predicted-slot read on a young volume**: a never-written slot decodes as neither a record nor a torn frame; treating it as torn would have paid the 128 KiB fallback on every idle poll of a volume with fewer than 32 checkpoints — an all-zero slot is the stop, not the fallback.
8. **Found by the matrix's stamped leg (first run red at `kv_node_cache_coherence_tests`)**: a harness that reads a PRODUCT-written volume through a hand-built `NodeCache` with `NodeLayout::new(sb.node_size)` reads every v2 frame as foreign under the seam (the frame version is the LAYOUT's) — the bare-cache reader served nothing. The fixture now asks `KvMetaBackend::node_layout_for(&sb)`, the product's own resolver, and the matrix re-ran from zero. The same class exists in `kv_finding_a_tests` / `kv_leaf_merge_tests` (superblock-derived `NodeLayout::new`), which pass both legs today because their walks tolerate it; a sibling PR or PR 14 touching those fixtures should switch them to the resolver too.

## 7. Owed

- **PR 12**: the per-object holder → endpoint binding (tree 0 → appender id → member identity → S8 endpoint; the `NotHolder` redirect), N daemon processes on one volume, the live-FUSE `-o ro` token mount row.
- **PR 7b**: per-stripe tokens for a striped directory.
- **PR 6/12**: the metanode ship for a foreign slot's mutation (a token reader today mutates nothing).
- **PR 10**: the death path's tail recorder through `frame_screen_for`; `Recovering` / `Recovered`.
- **PR 14 (the flip)**: delete the S5 bounded-staleness projection of user-visible metadata and the epoch fan-in (`min_acked_free_epoch`, the shard fan-in) — retired here as the QUALIFICATION under the plane, kept as the unarmed posture's mechanism and the ring's release law (§1.3); delete the `=0` posture.
- **Economy**: kernel-side dentry / attr invalidation on recall (`notify_inval_entry` / `notify_inval_inode`) so a token reader's kernel TTLs can be non-zero without a stale window — today the TTL-0 posture is exact and pays a daemon round trip per kernel lookup.
- **Gate 5 (the box bracket)**: grant p99 / recall-ack p99 at N = 32 members, the `sym-readers` rig, `free_grace_hold_ms` on a fleet cadence — SCOPING rows here only.
- **The instrumented fuzz run** of `bset_frame_v2` (nightly tier; type-checked on stable here).

## 8. Review round 2 (2026-09-15) — the round's 20 issues (7 bugs), every one closed red-first

Review: `/tmp/grok-justin/grok-exec-review-dadee1dd-pr-5.md` (OPEN 20 / BUGS 7 at HEAD `18847944` — NOT LANDABLE). Twenty-two commits `df1744e0`..`935a8e9d` on `feat/sym-tokens`; each bug's pin is its own `test(...)` commit before its `fix(...)`.

### 8.1 What changed

| # | Class | The fix (the pin) |
|---|---|---|
| 1 | bug | The rustdoc gate: the `///` outer doc on `pub mod token_plane;` deleted (the `//!` doc carries it), the private link spelled in backticks — `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps` green, a line of every round's ledger. |
| 2 | bug | **The central law.** The grant is served UNDER the pass's exclusion: `src/token_grant_core.rs` (`GrantPassGate`, `HolderTable`; `#[path]`-shared into `loom-models/`) — the token REGISTERS before it reads, a pass marks its WHOLE union in flight before it consults holders (the `outstanding == 0` short-circuit is gone), a grant on an in-flight object PARKS to the settle (`dlm_token_grant_parks`), a read a pass straddled repeats after the settle. Two loom models (`token_grant_models`); weakening: the grant's two steps swapped → red ("holders 0, Proceed" / "holders 1, parked 0"); the `SeqCst` fences changed nothing (the mutex chains order the steps) and were deleted rather than claimed. Pin `a_first_touch_grant_inside_the_pass_window_serves_the_committed_records` (the reviewer's schedule under `TEST_CONVEYOR_HOLD_POST_RECALL`). |
| 3 | bug | **Per READER CLASS**: `recall_gate_verdict` → `Deferred` while any LIVE `Reader` member is not a token client of this holder (`MembershipOwner::live_reader_ids` × the plane's client registry, `free_grace::install_token_client_probe`); `free_grace_s5_reader_deferrals`. Pin `an_s5_reader_on_an_armed_set_keeps_its_epoch_protection` (armed writer + token reader + `=0` S5 reader: the free rides the ring, released by the readers' acks — the MIN law — direct after the S5 reader's eviction). |
| 4 | bug | ONE release path, two triggers: `release_retired` runs the sink's drain + R-6 purge THEN `Release`; eviction and the clean leave ride it; channel loss → `drop_all_and_purge`. Pin `a_voluntary_release_drains_and_purges_before_the_holder_is_told`. |
| 5 | bug | `ack ∨ the S6 owner's lease EXPIRED`, INSIDE the wait loop: an upfront sweep, the wait cut at the earliest live lease's remaining, `sweep_expired` → `RecallLane::retire_client` (every grant of the dead member — `dlm_token_lease_swept_grants`); `LeaseVerdict::Unknown` waited like live. Pin re-scoped: 64 files on a dead reader, the oracle flips 300 ms in → the commit completes in [280, 900) ms, 64 swept, the next pass on a swept file < 200 ms with no recall; the LIVE shape still waits the deadline (`timeouts_live` 1). |
| 6 | bug | The token arm REFUSES `SQUEEZEFS_FREE_GRACE_DRAIN_{OBSERVED,EPOCH_STAMP}=0` loud and force-arms the serve ledger. Pin `readonly_mount_tests::the_mount_path_token_arm_refuses_a_drain_lever_at_zero`. |
| 7 | bug | BYTES on the R5 component `dlm_token_records_bytes` (floor 0, weight 1, trim = shed): `dlm_token_cached_bytes` live (charged at install, credited at recall / eviction / shed), `evict_to_budget` by bytes (second-chance), one oversize entry refused (`dlm_token_oversize_refusals`), the derivation 1/256 of the R5 budget floored at one control frame (tie-tested). Pin `the_token_cache_is_bounded_by_bytes_and_refuses_an_oversize_entry`. |
| 8 | sugg | The recall above the growth Dekker, `if token_holder().is_some()`, its wall `meta_txpass_phase_ns.pass_token_recall` (the eleventh phase). The apply stage's service time includes one recall RTT whenever a batch touches a tokened object — stated. |
| 9 | sugg | `fuzz/fuzz_targets/token_call_frame.rs` (raw bytes + constructive round trips) + the proptest mirror (`token_frame_decoders_never_panic`, `token_{request,reply}_frame_round_trips`). |
| 10 | sugg | The recall purge SCOPED per ino: every recall / release / drop hands the sink the entry (`RecalledObject`), `MountRecallSink` (one per volume) decodes its `layout`, `discard_layout_cache(global)` + `purge_block_key` on exactly the keys it names (stored values + free-able bases); the census the counted fallback (no entry held, an off-record map). `dlm_token_recall_{purged_keys,scoped_purges,census_purges}`. The layout-cache STEP stays global (the drain's generation + the stamp gate's race closer; under tokens the miss re-decodes off the token cache). Pin `a_recall_purges_the_recalled_objects_block_keys_and_leaves_the_rest`. |
| 11 | sugg | `TokenEntry` name index (`find` = one probe) + `page_after` (`partition_point`). |
| 12 | sugg | `KvMetaBackend::foreign_slot_holder` — one lease-table read before anything is registered; `NotHolder { holder }` (`dlm_token_not_holder_redirects`). Pin with a PR-4 in-process wire joiner on routing slot 100. |
| 13 | sugg | `refuse_explicit_ttls_under_tokens` at `init`: an explicit non-zero kernel TTL on a token reader refuses loud naming the knob and R-SYM-4. Pin `an_explicit_kernel_ttl_on_a_token_reader_is_refused_loud`. |
| 14 | sugg | `rollback_failed_tx` = recall the undo keys' objects → the two phases → settle. Pin `a_failed_windows_rollback_recalls_the_readers_holding_its_records` (ring-head sector fault + `TEST_CONVEYOR_HOLD_PRE_ROLLBACK`: the phantom served inside the hold, gone after). |
| 15 | sugg | The entry's `inflight`/`drain` Dekker DELETED (immutable records; the `ServeStamp` drain is the DMA hazard's) — stated in the module doc. |
| 16 | sugg | (a) a grant session POOL (`publish_ship_depth_from`'s derivation, `[2, 8]`; `dlm_token_grant_sessions{,_dialed}`) — pin `a_parked_grant_does_not_serialize_the_volumes_other_grants`; (b) records PAGED under one byte budget: `TokenWants.records`, `Grant.xattr_after`, `TokenRecords.xattrs_complete` — xattrs by name first, dentries by cookie after, a continuation re-carries nothing; pin `an_objects_xattrs_are_paged_under_the_grant_budget_and_never_resent` (66 × 16 KiB xattrs + 200 names, 3 pages). (c) the first `readdir` page off the first grant page: **owed** (a partial entry against the immutability law). `TOKEN_SCHEMA` stays 1 (the wire is unreleased). |
| 17 | sugg | `TokenReaderPlane::probe` at the arm (one `Recall { wait_ms: 0 }`); a holder that serves no tokens refuses the mount naming the writer's knobs. |
| 18 | sugg | Screen **rule 4**: `FrameScreen::appender_current` (tree 0's lessee; `None` while UNLEASED), `g == g_current ∧ appender_id ≠ lessee` → the breach class under a device fence (`is_breach_rule`); the overwrite probe looks for the lessee's OWN stamp. **Found by the final-tree ledger's `sym_slot_transfer_tests` leg (both layouts): the first build set `appender_current = 0` for an unleased slot — but a release KEEPS `g`, so the former lessee's frames at `g` read as rule-4 foreign beside the manager's `(0, g)` and a released tree read EMPTY after its remount (`a_handover_of_a_tree_with_pending_structure_runs_the_holders_own_smos`, `getxattr → None`). Rule 4 is inert on an unleased slot; pinned in the rule-4 contract's unleased arm; the ledger restarted from zero.** Pin + fuzz/proptest inputs. |
| 19 | sugg | `find_parent_of_child` refuses `ESTALE` on a token reader (the pin's first run answered "no dentry names ino 4" off the stale projection). |
| 20 | nit | `DELEG_PARK_DEFAULT_MS` reused (+ derived backoff), `iter_sync`, `QueueDepthHistogram::{upper_bound, percentile}` (the one bucket law), the two docs fixed, the recall gate COUNTS holders and the writer's `shutdown` disarms it (pinned). |

### 8.2 Run ledger (the final tree)

Tree `15267a3c` (the code tree; the ledger's own docs commit follows). Logs `/tmp/grok-justin/pr5-logs/round2/`.

| Run | Result |
|---|---|
| `cargo fmt --check` (root, `fuzz/`, `loom-models/`) | clean |
| `cargo clippy --all-targets --all-features -- -D warnings` / `cargo clippy --all-targets -- -D warnings` | clean / clean |
| `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps` | **green** (red at round 1 — Issue 1) |
| `cd fuzz && cargo check --bins && cargo fmt --check` | clean (`token_call_frame` + the grown `bset_frame_v2`) |
| `task check:loom`-equivalent (`RUSTFLAGS="--cfg loom -D warnings" cargo build --release --tests` + fmt) / `tests/run_loom.sh` | clean / **109 passed** (the two `token_grant_models` included) |
| `sym_coherence_tests` ×10 stamped (`SQUEEZEFS_TEST_STAMP_SYMMETRIC=1`), `--test-threads=1`, from zero | 10/10 — **26 passed** each, 43.6–45.7 s |
| `sym_coherence_tests` ×3 flat | 3/3 — 26 passed each, 43.6–44.0 s |
| `readonly_mount_tests` / `reader_free_grace_tests` / `dlm_membership_tests` / `kv_node_tests` / `kv_smo_crash_completeness_tests` / `sym_slot_transfer_tests` / `sym_appender_tests` / `kernel_op_economy_tests` / `docs_parity_tests` / `env_knob_convention_tests` / `derivation_sweep_tests` / `decoder_property_tests` / `audit_instruments_tests`, flat AND stamped | 35 / 65 / 51 / 13 / 13 / 45 / 34 / 3 / 5 / 22 / 55 / 49 / 27 — green on both legs (26/26) |
| `tests/run_sym_forest_suites.sh` (29 suites, both legs, once) | **PASS / PASS**, `matrix exit 0`, no ratio NOTE; `sym_coherence_tests` 43.8 / 43.6 s (1.00), `sym_slot_transfer_tests` 33.5 / 33.8 s (1.01). **Attempt 1 (kept: `matrix-attempt1-flatFAIL-at-slot-transfer-under-load.log`) FAILED its flat leg at `sym_slot_transfer_tests` (3 of 45: the renewal carriage, the two-volume owner table, the alternating pair — PR-4 contracts whose `N_floor` / offer state derives from MEASURED handover and ship walls) at 79.8 s wall against 33–34 s on every other roll, while a sibling PR's `cargo test --all-features --test fsck_tests` ran beside it; the same binary standalone under `--all-features` and the two ledger legs read 45/45 — attributed to box load, the re-run from zero above is the row.** |
| The live-FUSE trio, both legs, `SQUEEZEFS_TEST_REQUIRE_MOUNT=1` | flat: `posix_mount_semantics_tests` 3 (9.8 s), `corpse_sweep_tests` 4 (33.5 s), `inline_raise_tests` 7 (7.9 s); stamped: 3 (6.7 s), 4 (21.7 s), 7 (7.3 s) — every mount-class contract RAN (`/dev/fuse`, `fuse.enable_uring=Y`, `fusermount3`; a self-skip panics under the flag) |
| `tests/check_markdown_links.sh` | PASS (377 files, 632 local links, 0 broken) |

**Found by the ledger itself** (§8.1 row 18): the first build of screen rule 4 read a RELEASED tree empty after its remount — `sym_slot_transfer_tests::a_handover_of_a_tree_with_pending_structure_runs_the_holders_own_smos`, both layouts — because a release keeps `g` and the rule took the manager's 0 as the lessee; fixed in `15267a3c` (rule 4 inert on an unleased slot), the unleased arm pinned, the whole ledger restarted from zero on the fixed tree (the counted-restart discipline).

## 9. Review round 3 (2026-09-15) — round 2's six open issues (2 bugs), closed red-first

| # | Class | The fix (the pin) |
|---|---|---|
| 5 (residual) | bug | The lease-expiry completion held only while the owner still LISTED the member past its deadline; the owner's cadence sweep (`expire_due`) EVICTS it every beat, after which the verdict read `Unknown` — waited like live to the 45 s deadline (the pin read 44.8 s). Two halves: a token client the installed owner no longer lists reads `Expired` (`Unknown` stays for the no-owner case and a client that never reached the service), and `membership.rs` gained an additive DEPARTURE SINK (`install_departure_sink`; `evict` and `leave` call it) — every armed holder registers (`register_holder`) and `TokenHolderPlane::sweep_departed` runs at the departure instant: pending recalls complete as `expired_with_lease` and WAKE the parked pass, the rest are `lease_swept_grants`. Pin `an_evicted_readers_grants_are_swept_at_the_owners_eviction_not_at_the_deadline` drives the REAL owner sweep on a manual clock (a `Reader` member, 64 tokens, `test_kill` — death: no ack, no release; the pass parks; `expire_due` evicts; the commit completes < 2 s at the eviction; 63 swept, 1 expired-with-lease; a second pass recalls nobody). |
| 21 | bug | `fetch_once` re-checked the recall generation BEFORE `evict_to_budget`'s await and installed after it — a recall landing inside the eviction (the grant was registered) installed PRE-COMMIT records LIVE, unrecalled (the pin served 0o644 after the commit to 0o600). The check is repeated ATOMICALLY with the install under the cache entry (the recall handler bumps before it removes, so a bump the check sees aborts the install and a bump after it finds the entry to remove). Pin `a_recall_landing_inside_the_fetchs_eviction_never_installs_pre_commit_records` (a parked `ProbeSink` inside the eviction + a concurrent commit on the fetched object). |
| 22 | sugg | The reader-class verdict caches in one word keyed by `(membership census generation, token-client registry generation)` — both new monotone words (`membership::census_generation`, `token_plane::token_clients_generation`) — so the O(members) scan runs per census change, never per free (`free_grace_s5_class_scans`; pin: 16 frees = 1 scan, a join +1, a token-client registration +1). `TokenClientProbe` is a struct of the two closures. |
| 23 | sugg | `GrantPassGate::inflight` is a REFCOUNT map — the plane's two users (the pass task, the durability lane's rollback) overlap by design, and a SET let the first `settle` clear the other's mark. Pin (the two-user schedule) + loom model `two_overlapping_users_keep_the_object_in_flight_until_the_last_settles`; weakening = the set's `settle` → red (verified). The two-user shape is stated in the plane's doc. |
| 24 | nit | `disarm_recall_gate` is one `fetch_update`; `retire_recall`'s doc comment restored (it had attached to the `HolderTable` impl); the two drain levers' registry text (`env_knobs.rs` + both `operations.md` tables) and the tokens row name the round-2 refusals (the levers at `0`, an explicit non-zero kernel TTL, the holder probe). |

### 9.1 Run ledger (the final tree)

Tree `6695fa45` (the code tree; the ledger's own docs commit follows). `CARGO_INCREMENTAL=0` throughout (disk 83 %). Logs `/tmp/grok-justin/pr5-logs/round3/`.

| Run | Result |
|---|---|
| `cargo fmt --check` (root, `fuzz/`, `loom-models/`) | clean |
| `cargo clippy --all-targets --all-features -- -D warnings` / `cargo clippy --all-targets -- -D warnings` | clean / clean |
| `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps` | green |
| `cd fuzz && cargo check --bins` | clean |
| loom check (`RUSTFLAGS="--cfg loom -D warnings" cargo build --release --tests`) / `tests/run_loom.sh` | clean / **110 passed** (the two-user model added) |
| `sym_coherence_tests` ×5 stamped / ×2 flat, `--test-threads=1`, from zero | 7/7 — **29 passed** each (26 → 29 contracts), 47.8–48.8 s |
| `readonly_mount_tests` (35) / `reader_free_grace_tests` (66) / `dlm_membership_tests` (51) / `docs_parity_tests` (5) / `env_knob_convention_tests` (22), flat AND stamped | green on both legs (10/10) |
| `tests/run_sym_forest_suites.sh` (29 suites, both legs, once) | **PASS / PASS**, no ratio NOTE; `sym_coherence_tests` 47.9 / 47.7 s (1.00), `sym_slot_transfer_tests` 33.7 / 33.8 s (1.00) |
