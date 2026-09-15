# Symmetric metadata — PR 6: cross-owner transactions (`crossvol_tx` over shipped steps; create-in-foreign-dir; the set-wide directory-rename lock; D18 reversed)

| | |
|---|---|
| **Date** | 2026-09-15 |
| **Branch** | `feat/sym-cross-owner-tx` (cut from `dev` @ `6f8d44e6` — PR 1, 2, 3, 4, 11 and 16 in) |
| **Design** | [`docs/design-symmetric-metadata.md`](../docs/design-symmetric-metadata.md) §5.6 (D18 reversed), §5.6.4 (the set-wide lock), §5.3.5 (idempotent verbs), §5.4.2, §11 (the Cross-owner family); PR-plan row 6; KD-SYM-14 |
| **Contracts** | [`tests/sym_cross_owner_tests.rs`](../tests/sym_cross_owner_tests.rs) — 15 contracts + the `#[ignore]`d scoping instrument, appended to `tests/run_sym_forest_suites.sh`'s list |
| **Venue** | the dev laptop — **SCOPING only** (the venue rule: no acceptance row is this rung's; the `tar -x` / shared-directory brackets are PR 13/14's on squeeze-test). Four implementers shared the box during every number below. |
| **Instrument** | in-process file-backed volumes (64 KiB nodes, a 1 MiB ring, one 64 MiB volume, one file-backed data volume for the offline fsck), the two-appender declared-partition model over a real `cluster_wire` loopback session; `cargo test --release` for the rate row, debug for the contracts |
| **Status** | landed dark: bit 17 AND `SQUEEZEFS_SYMMETRIC_META=1`; `=0` and every bit-17-absent mount take the S3.5 paths verbatim (pinned on both layouts) |

## 1. What landed

Under the armed plane a namespace mutation whose objects live in slots OTHER appenders lease is an ordinary POSIX op — ONE S3.5 intent whose foreign steps SHIP:

1. **Shipped steps.** Every `crossvol_tx` step homed on a foreign holder's slot travels as the S8 verb `MetaCall::XvStep` (`MetaVerb::XvStep = 0x60`, the PR-6 block) to the holder resolved through tree 0's lessee (`SlotLeaseTable::resolve` vs `KvMetaBackend::own_appender_id()`) and the appender→endpoint table (`SlotHolderCache::{set_endpoint,endpoint}`); **`xv_apply_step` stays the ONE applier** — the served side (`RoutedMetaBackend::xv_serve_step`) is the same function under the holder's lease and ring, idempotent under the recorded `(pre, post)` witness, its reply after its durability lane, its outcome counted on the S3.5 step ledger AT THE HOLDER.
2. **The arms.** `create`/`mkdir` under a foreign directory (`create_in_foreign_directory`, `XvOp::Create`, the new step kind `CreateInode` — the child minted in the CREATOR's rotor slot, affinity never following a foreign parent), `unlink`/`rmdir` of its entries, `link` into it, `rename` and `RENAME_EXCHANGE` across holders — all through the transaction path, plan under the op's 4a guards (**which travel**, §3), `tx0` = the first LOCAL step's records + the intent in ONE entry of the initiator's ring (the intent keyed on local 0 of that step's slot namespace; a plan whose every step is foreign writes the intent alone), the ring barriered, steps in plan order, retire.
3. **The set-wide `dir_rename` lease** (§5.6.4): every `rename` whose SOURCE is a directory takes it — same-slot ones too — OUTERMOST (a `Metadata::rename` wrapper decides on an unguarded read of the source's type, `rename_body` re-decides under the guards and hands back `Ok(false)` when the source became a directory), a tree-0 record on volume 0 (`slot_state::DirRenameRecord`) served by its manager (`ManagerCall::DirRenameLock/DirRenameUnlock`, idempotent — `already` / `busy { holder }`); the ancestor check on EXACT data (`refuse_rename_into_own_subtree`: the target's parent chain walked through `parent_link_of`, every link confirmed at its holder through the read verb `LookupExact` (0x61); a chain reaching the moved directory `EINVAL`); file renames never take it. Expiry law: the lock dies with the initiator's membership lease; PR 10's recovery releases a dead holder's record (`manager_dir_rename_release_dead` is the entry).
4. **Roll-forward.** A ship that fails leaves the intent OPEN (never a fail-stop — the S3.5 lattice latch guards a LOCAL mid-plan device error's violation of the witnesses' premise, which a holder that is down does not violate); the set's own **roll-forward cadence** (`spawn_roll_forward_cadence`, at the checkpoint landing ceiling, the TWO-TICK rule) or the next mount's `recover_open_intents` completes it once the holder serves; `xv_cross_owner_intents_stuck` (open past the grace window `CLIENT_STALE_TTL_SECS`) is the must-stay-0 face.
5. **The Cross-owner family** (§4) on the stats inode, 0 on every unarmed mount by construction.
6. **The contracts** (§2) and the docs (`docs/operations.md`, `AGENTS.md`, design row 6).

## 2. Contracts (15 + 1 `#[ignore]`d; every one red before its implementation, green ×10 stamped from zero — §5)

| # | Contract | Red on | Pins |
|---|---|---|---|
| 1 | `a_create_in_a_foreign_directory_ships_one_insert_dentry_and_mints_in_the_creators_rotor` | the unbuilt arm (EXDEV) | ONE shipped step, the child in the creator's rotor, `dlm_rpcs` 0, closure |
| 2 | `unlink_rmdir_link_rename_and_exchange_across_two_holders_leave_a_byte_exact_tree` | every arm | the verb set across two holders; ≥ 9 intents minted ≡ retired; offline fsck clean |
| 3 | `a_rename_across_three_holders_ships_a_step_to_each_foreign_side` | the third holder's routing | one step to each foreign side |
| 4 | `every_initiator_crash_window_of_a_foreign_create_rolls_forward_with_no_acked_loss` | the roll-forward over shipped steps | `TEST_XV_SEAM_AFTER_STEPS` 1..=3, reopen, `roll_forward_open_intents` — no acked loss |
| 5 | `a_holder_dying_after_commit_before_reply_is_recognized_exactly_once_by_its_successor` | the successor's witness | `TEST_XV_SERVE_MISDELIVER_ONCE` + a sequenced second service (an empty dedup window) |
| 6 | `a_holder_dying_before_commit_leaves_an_open_intent_the_cadence_rolls_forward` | the cadence | `TEST_XV_SERVE_REFUSE`; `intents_stuck` 1 then 0 |
| 7 | `all_parties_down_after_a_foreign_unlink_the_next_mount_rolls_forward` | the mount's recovery over a foreign step | reopen + holders + roll-forward |
| 8 | `two_nodes_cannot_rename_directories_into_a_cycle` | the lock | exactly one of two concurrent directory renames completes, the loser `EINVAL`, every chain reaches the root, +2 lock takes |
| 9 | `a_file_rename_never_takes_the_dir_rename_lock_and_every_directory_rename_does` | the lock's scope | file renames 0, directory renames +1 each (same-slot too) |
| 10 | `the_dir_rename_lock_is_durable_in_tree0_and_a_dead_holders_lock_is_released_by_recovery` | the record | the wire verbs, `already`/`busy`, `dir_rename_lock_held` parks, `manager_dir_rename_release_dead` |
| 11 | `the_unarmed_and_flat_paths_ship_nothing_and_lock_nothing` | — (the byte-identity pin) | intents at ino 0, no ship, no lock, every gauge 0 on both layouts |
| 12 | `the_cross_owner_family_is_exported_under_its_published_names` | the export | every §11 key, the phase table |
| 13 | `a_served_step_never_parks_behind_the_initiators_guards_even_on_a_stripe_collision` | **the first build (§3)** | the collision forced by NAME; the rename completes inside 5 s |
| 14 | `a_remote_initiators_guards_park_at_the_holder_until_release_and_expire_with_its_lease` | the wire pair | park (a local acquirer waits), a scoped step applies without a guard, release (idempotent), expiry with the lease (`guard_expiries` 1) |
| 15 | `an_initiator_acquires_its_foreign_guards_at_the_holder_and_releases_them_at_the_end` | the initiator's remote arm | `TEST_XV_GUARDS_FORCE_REMOTE`: one `XvGuards` per holder table, released at the terminal outcome |
| — | `scoping_row_per_verb_wire_and_barrier_cost` (`#[ignore]`) | — | the §4 instrument; **found §3** |

## 3. Finding — the served step parked behind the initiator's guards (the first build; fixed before landing)

The first build kept the initiator's 4a guards LOCAL (`lock_many_leased` dropped a foreign slot's keys) and had the holder take the step's guards around the served apply — the as-built deviation "no foreign 4a guard travels", argued from the lack of a common order between a held remote guard and the stripe-canonical `lock_many`. **The scoping instrument falsified it on its first run**: 200 creates into a foreign directory passed, then the **27th** of 200 directory renames across holders stalled — the holder's `insert_dentry` (needing `I{shared}` + `D{shared,e26}`) waited behind a 4a stripe the initiator held across the ship (`D{mine,d26}` — a dentry-class stripe collision in the ONE table the two appenders share in-process), the initiator waited on the reply, and the op ran out the wire's two 10 s call timeouts (the cadence then completed the intent: the step's second sight answered `AlreadyApplied`). Across two nodes the same edge is the two-mutual-initiator cycle (`A: local X → ship Y`, `B: local Y → ship X`) — reachable by two concurrent inverse renames on a shared directory pair; a stripe collision was just its earliest in-process face.

**The fix is design §5.6 line 1, built**: "plan under the op's 4a guards (foreign-home guards travel)".

- `crossvol_tx::acquire_guards_leased` is the arms' ONE acquisition: every key whose lock lives in THIS process's table (the initiator's own slots and — in the contracts' declared-partition model — its other regions', `KvMetaBackend::is_own_region`) rides the one stripe-canonical `lock_many`; a foreign holder's keys ride one `MetaCall::XvGuards` (0x62) to it, where the same `lock_many` runs in ITS table and the guards park under the op's scope (`mint_guard_scope`); **tables are acquired in ascending appender-id order** (the initiator's own at its own id), so a waiter for table `T` holds only tables `< T` and the wait-for graph over tables is acyclic — the hierarchical argument (in the shipped one-appender-per-process shape a holder's id IS its table's rank; the multi-region-per-process seam ranks them all at region 0's id, stated on the line).
- A step ships under the scope (`MetaCall::XvStep { scope }`); the served side's verdict (`serve_guards_for`) is `Covered` for an in-process initiator's marker (`LOCAL_SCOPES`, matched on the frame's `client_id` = this process's shipper identity) or a scope parked here, and the apply takes NOTHING; `Take` (a roll-forward that travelled no scope, an expired scope) takes the step's keys for the apply's duration. **A served step never parks on a 4a lock.**
- The release rides the drop of the op's guard set: `DlmGuard::external(scope, on_drop)` (a new form of the 4a guard, `Sync`) carries each remote hold, its drop shipping `XvRelease` (0x63) fire-and-forget — the terminal-outcome release law by construction, through every existing `Arc<[DlmGuard]>` signature. A dead initiator's scope expires with its lease: the cadence sweeps parked scopes older than the grace window (`sweep_expired_guards`; `xv_cross_owner_guard_expiries`, must-stay-0).
- `xv_serve_guards` skips a stripe the scope already parked (a second call under one scope — the same holder serving two of the op's volumes — or a resend past the dedup window must never wait behind itself); the guard verbs are `mutating()` so a resend inside the window is answered from the winner's outcome; a key whose slot the holder does not lease refuses `EAGAIN` (the stale-holder-view class, exactly `xv_serve_step`'s).
- The mount-time recovery acquires the whole plan's guards the same way; a holder it cannot reach for them is the ship-failure class (the intent stays open for the cadence), never a failed mount — found by the matrix's serial run, where the leaked register entry of one contract's failed recovery red-flagged three others' closure pins.

Cost: a same-process holder costs no round trip (the contracts' model — `guard_rpcs` 0 there); a real foreign holder costs one `XvGuards` + one `XvRelease` per holder table per op, priced as `xv_cross_owner_phase_ns.guard_rtt` beside the transaction (the `+ 1–3 remote 4a guards` term of design §6.3's cost table). Pins: #13–#15 above.

## 4. The scoping row (SCOPING — dev laptop, `cargo test --release`, one file-backed 64 MiB volume, 64 KiB nodes, a 1 MiB ring, loopback wire, box shared by four implementers)

`scoping_row_per_verb_wire_and_barrier_cost --ignored --nocapture`: 200 creates into a foreign directory (one shipped step each), then 200 directory renames from a native directory into the foreign one (two shipped steps each — the parent's `SetNlink` and the `InsertDentry` — under the set-wide lock).

| row | ops | µs/op | intents | steps shipped | `guard_rpcs` | lock takes |
|---|---|---|---|---|---|---|
| create in a foreign directory | 200 | **863** | 200 | 200 | 0 (same-process holder) | 0 |
| directory rename across holders | 200 | **2 659** | 200 | 400 | 0 | 200 |

`xv_cross_owner_phase_ns` over the 400 transactions (mean): `plan` 38 µs · `intent_barrier` **625 µs** · `ship_rtt` 83 µs per step (600 steps) · `retire` 21 µs · `total` 837 µs. The S8 wire beneath the ship (`meta_ship_phase_ns`, 600 frames): `rtt` 72 µs, `queue_wait` 5 µs, encode/decode < 1 µs; the owner's side (`meta_ship_owner_phase_ns`): `execute` 30 µs (the holder's apply + commit on its ring), `dispatch` 42 µs, `total` 44 µs. `dir_rename_lock_wait_ns` mean **537 µs** (200 takes, uncontended — the take IS one ring-0 control entry + its barrier on this substrate; the cycle contract's contended take is the same write behind a park).

Reading: the create's 863 µs is **72 % the initiator ring's barrier** on a file on a CoW host filesystem (the substrate bracket the metadata-throughput baseline measured at 165× on the journal barrier) and 10 % the shipped round trip — on the box the barrier term collapses and the per-verb wire cost (≈ 80 µs loopback here, ≈ 100 µs at the design's fabric RTT) is the row. The directory rename adds the lock's own control entry + barrier (537 µs), a second shipped step and the exact ancestor walk (local links here). Closure exact: 400 minted ≡ 400 retired, 0 open, 0 stuck, `steps_shipped ≡ steps_served` = 600. **Scoping evidence only** — no acceptance row is this rung's; the row's job was to run the arms at count, and it found §3.

## 5. Verification (the venue rule — everything local)

- `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings` and the shipped-config clippy clean; `fuzz/` type-checks and is fmt-clean on stable.
- `tests/sym_cross_owner_tests.rs`: 15/15 green serially and parallel-threaded; ×10 stamped from zero (§5a).
- The timed matrix `tests/run_sym_forest_suites.sh` (29 suites, flat THEN stamped) with the suite appended — both legs green (§5a; the pre-fix tree's run also passed both legs, with `fsck_tests` 2.46 and `sym_convert_tests` 2.15 stamped/flat ratio NOTEs under a box running four implementers' builds and a concurrent release link).
- `crossvol_tx_tests` (14), `pv_partial_open_tests` (22), `rename_lock_set_tests` (2) green on both legs; `meta_ship_tests` (15 — the census pins re-scoped to the 17-verb vocabulary), `decoder_property_tests` (44), `docs_parity_tests` (54), `env_knob_convention_tests` (5), `derivation_sweep_tests` (22) green.
- The live-FUSE trio under `SQUEEZEFS_TEST_REQUIRE_MOUNT=1`, both legs (§5a).

### 5a. Run ledger (the fixed tree, `2894e309`+)

| Run | Result |
|---|---|
| `sym_cross_owner_tests`, serial | 15 passed, 1 ignored, 7.8 s |
| `sym_cross_owner_tests`, default threads | 15 passed, 1 ignored, 8.6 s |
| ×10 stamped from zero (`--test-threads=1`) | 10/10 green — 6.41 / 6.78 / 6.90 / 6.90 / 7.29 / 6.91 / 6.72 / 6.78 / 6.68 / 6.69 s |
| `tests/run_sym_forest_suites.sh` (29 suites) | flat leg PASS, stamped leg PASS, **0 ratio NOTEs**; `sym_cross_owner_tests` 7.3 s / 7.3 s (0.99); 16 min wall |
| the pre-fix tree's matrix (for the record) | both legs PASS; NOTEs `fsck_tests` 2.46 and `sym_convert_tests` 2.15 — the box ran four implementers' builds plus this branch's concurrent release link; on the fixed tree's quieter run the same suites read 0.82 and 1.01 |
| `crossvol_tx_tests` / `pv_partial_open_tests` / `rename_lock_set_tests`, flat and stamped | 14 / 22 / 2 green on both legs |
| `meta_ship_tests`, `decoder_property_tests`, `docs_parity_tests`, `env_knob_convention_tests`, `derivation_sweep_tests` | 15 / 44 / 54 / 5 / 22 green |
| the live-FUSE trio, both legs, `SQUEEZEFS_TEST_REQUIRE_MOUNT=1` | flat: `posix_mount_semantics_tests` 3 (37 s), `corpse_sweep_tests` 4 (143 s), `inline_raise_tests` 7 (97 s); stamped: 3 (39 s), 4 (145 s), 7 (110 s) — all green, no mount-class skip |

## 6. Deviations from the PR-plan row / the brief

1. **The D19/D20 arms (`cross_owner_error`, M1/M2/M3) are NOT deleted.** They are the per-volume-owner recipe's (`SQUEEZEFS_MULTI_WRITER` + `set-owners`, flat volumes) and are unreachable under the symmetric plane by construction (its ownership is the slot lease, not the S8 `OwnerMap`), so `meta_ship.cross_owner_refusals` reads 0 on every symmetric mount — the brief's "0 by construction" — while deleting them here would change a flat armed mount's behaviour and red the PV suites (rule 5). They retire with the recipe (PR 12/14).
2. **The intent's home** is local 0 of the first LOCAL step's slot namespace (`intent_ino_for_slot`) — ino 0 verbatim on the flat/native path, a guest slot's own 0 otherwise (guest cursors mint from 2) — so `tx0` is ONE entry in ONE region; the design's "the initiator's ring" is honoured with the region chosen by the step.
3. **`dlm_rpcs += 1` per travelling guard** is the family's own `xv_cross_owner_guard_rpcs`: the S4 word lives in `src/dlm_slot.rs`, PR 5's file; the one-line fold is the rebase's.
4. **`LookupExact` is a plain read verb** (the brief's own instruction); PR 5's tokens may collapse it onto a token grant at the rebase.
5. The plan's foreign READS (the EEXIST probe, the `(pre, post)` witnesses' inputs) are exact in-process and the S5 projection on the wire until PR 5's tokens.
6. **The multi-holder model** is PR 2–4's declared partition (`SQUEEZEFS_TEST_SYM_APPENDER_SLOTS`) made two holders in one process — a declared region's slot is FOREIGN to appender 0 for every cross-owner decision and its step ships over a real `cluster_wire` session — with the consequence that a declared region shares the initiator's 4a table (so its guards ride the one canonical `lock_many` and no guard RPC is paid); the remote arm is exercised through `TEST_XV_GUARDS_FORCE_REMOTE`. N daemon processes on one volume is PR 12's join ladder.
7. **The first build's "no foreign 4a guard travels" was unsound** (§3) and is reversed: the landed shape is the design's own.

## 7. Owed

- Tokens over the plan's foreign reads (PR 5); the endpoint binding + the wire initiator's lock take through the join ladder (PR 12); the death ledger driving `manager_dir_rename_release_dead`, a dead holder's slot recovery, and the dead initiator's parked-guard release at the eviction instead of the grace sweep (PR 10); the striping steps (PR 7b — `InsertDentry`/`RemoveDentry` take the KEY ino); the `tar -x` / shared-directory rows on squeeze-test (PR 13/14).
- `dlm_rpcs`'s fold of `xv_cross_owner_guard_rpcs` (one line in `dlm_slot.rs`, at the rebase).
- The parked-guard registry is RAM (a holder restart releases every parked scope — an initiator's next step then takes the `Take` verdict under its own witness; PR 10's `Recovering` law names the holder's successor explicitly).
- The instrumented fuzz run of the grown `cluster_wire_frame` / `manager_call_frame` targets (the nightly tier).
