# Symmetric metadata — PR 6: cross-owner transactions (`crossvol_tx` over shipped steps; create-in-foreign-dir; the set-wide directory-rename lock; D18 reversed)

| | |
|---|---|
| **Date** | 2026-09-15 |
| **Branch** | `feat/sym-cross-owner-tx` (cut from `dev` @ `6f8d44e6` — PR 1, 2, 3, 4, 11 and 16 in) |
| **Design** | [`docs/design-symmetric-metadata.md`](../docs/design-symmetric-metadata.md) §5.6 (D18 reversed), §5.6.4 (the set-wide lock), §5.3.5 (idempotent verbs), §5.4.2, §11 (the Cross-owner family); PR-plan row 6; KD-SYM-14 |
| **Contracts** | [`tests/sym_cross_owner_tests.rs`](../tests/sym_cross_owner_tests.rs) — 28 contracts + the `#[ignore]`d scoping instrument (15 at round 1, 8 added by review round 2 — §8, 5 by round 3 — §9), appended to `tests/run_sym_forest_suites.sh`'s list |
| **Venue** | the dev laptop — **SCOPING only** (the venue rule: no acceptance row is this rung's; the `tar -x` / shared-directory brackets are PR 13/14's on squeeze-test). Four implementers shared the box during every number below. |
| **Instrument** | in-process file-backed volumes (64 KiB nodes, a 1 MiB ring, one 64 MiB volume, one file-backed data volume for the offline fsck), the two-appender declared-partition model over a real `cluster_wire` loopback session; `cargo test --release` for the rate row, debug for the contracts |
| **Status** | landed dark: bit 17 AND `SQUEEZEFS_SYMMETRIC_META=1`; `=0` and every bit-17-absent mount take the S3.5 paths verbatim (pinned on both layouts) |

## 1. What landed

Under the armed plane a namespace mutation whose objects live in slots OTHER appenders lease is an ordinary POSIX op — ONE S3.5 intent whose foreign steps SHIP:

1. **Shipped steps.** Every `crossvol_tx` step homed on a foreign holder's slot travels as the S8 verb `MetaCall::XvStep` (`MetaVerb::XvStep = 0x60`, the PR-6 block) to the holder resolved through tree 0's lessee (`SlotLeaseTable::resolve` vs `KvMetaBackend::own_appender_id()`) and the appender→endpoint table (`SlotHolderCache::{set_endpoint,endpoint}`); **`xv_apply_step` stays the ONE applier** — the served side (`RoutedMetaBackend::xv_serve_step`) is the same function under the holder's lease and ring, idempotent under the recorded `(pre, post)` witness, its reply after its durability lane, its outcome counted on the S3.5 step ledger AT THE HOLDER.
2. **The arms.** `create`/`mkdir` under a foreign directory (`create_in_foreign_directory`, `XvOp::Create`, the new step kind `CreateInode` — the child minted in the CREATOR's rotor slot, affinity never following a foreign parent), `unlink`/`rmdir` of its entries, `link` into it, `rename` and `RENAME_EXCHANGE` across holders — all through the transaction path, plan under the op's 4a guards (**which travel**, §3), `tx0` = step 0's records + the intent in ONE entry of the initiator's ring when step 0 is local (the intent keyed on local 0 of that step's slot namespace; the intent alone, its own entry, when step 0 is another appender's), the ring barriered, steps in plan order, retire.
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
2. **The intent's home** is local 0 of STEP 0's slot namespace when step 0 is local (`intent_ino_for_slot`) — ino 0 verbatim on the flat/native path, a guest slot's own 0 otherwise (guest cursors mint from 2) — so `tx0` is ONE entry in ONE region; when step 0 is another appender's (a rename whose OLD parent is foreign) the intent is written ALONE first — its own entry + barrier (review round 1, Issue 14b: the earlier text said "the first LOCAL step", which the code never did). The design's "the initiator's ring" is honoured with the region chosen by step 0.
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

## 8. Review round 2 (2026-09-15) — five bugs, fifteen suggestions; what the fixes changed

The round-1 review (`/tmp/grok-justin/grok-exec-review-dadee1dd-pr-6.md`) found the tree NOT LANDABLE on five bugs. Each was fixed red-first; the eight new contracts and the changed laws:

| Issue | Class | The defect as built | The fix (and its pin) |
|---|---|---|---|
| 1 | bug | `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps` red on three private intra-doc links | unlinked; the doc gate is in every round's table |
| 2 | bug | the ancestor walk ran after the rename's 4a guards were held and confirmed each hop through `lookup_dentry`, whose SHARED `D{}` stripe guard parked the initiator behind its own EXCLUSIVE one on a collision — no timeout, the set-wide lease held, every directory rename in the set wedged | both arms read guard-free (`lookup_dentry_exact_unguarded`; the lease IS the read's consistency); pin `a_directory_rename_whose_ancestor_link_collides_with_its_own_guard_stripe_completes` (the collision forced by name — red at 5 s pre-fix) |
| 3 | bug | `DirRenameLock/Unlock` acted on the wire's id verbatim, on any volume | `screen_dir_rename_words` (pure, fuzzed + proptest-mirrored): volume 0 only (`ManagerService` learns its ordinal from the set service), a `Live` non-own id, the holder for an unlock — `Rejected` + `manager_verb_rejected`; pin `the_lock_verbs_reject_a_foreign_unlock_a_dead_id_and_any_volume_but_zero` (5 refusals counted); the durable-lock contract runs on wire joiners |
| 4 | bug | the cadence's two-tick rule was a TIMER and `recover_one` applied the SCANNED plan; a live op spanning ticks was adopted and replayed after its retirement against witnesses the new state satisfied (a link resurrected after an unlink — C10's leak; a compensated child re-minted); a retirement between scan and register left a RAM ghost tripping `intents_stuck` | the register carries STATE (`InFlight` — registered BEFORE the record is durable — / `Abandoned`), the cadence adopts abandoned intents only and only while one exists, `recover_one` RE-READS the record under the acquired guards (`xv_read_intent`) and applies that image (gone ⇒ no-op, forgotten); pins `a_live_op_spanning_cadence_passes_is_never_replayed_by_the_cadence` (a held served step, ≥ 2 passes, then the unlink), `a_retired_intent_met_by_a_stale_scan_is_a_no_op_and_leaves_no_ghost` (pass A parked after its scan, pass B retires, the user unlinks, pass A resumes: nothing re-applied, `intents_open` 0, `intents_stuck` 0) |
| 5 | bug | the cycle pin was hollow: one identity (`already`), renames sharing `I{b}` | two DISTINCT identities (`TEST_DIR_RENAME_IDENTITY_ONCE` + a wire joiner's Live id), guard sets DISJOINT by construction (inode stripes re-minted, dentry names chosen off both sets), the loser WAITS on `Busy` (`dir_rename_lock_wait_ns` ≥ 200 ms) then `EINVAL`; with the lease made a no-op the pin FAILS at "the loser WAITED" (verified, reverted). One process's concurrent directory renames are TICKETS under one record (per `(volume, identity)`), released with the last |
| 6 | sugg. | the lease leaked on a dropped future | `DirRenameLease` is RAII (Drop spawns the release) |
| 7 | sugg. | a wire-timed-out `XvGuards` left the holder's late-parked scope until the sweep | the initiator ships a fire-and-forget `XvRelease` behind every failed acquisition |
| 8 | sugg. | the wire's `child` unvalidated; `Covered` trusted the scope; the local applier unchecked | `screen_insert_child` (a record on its volume, or a slot tree 0 says some appender leases — `EINVAL` + `xv_cross_owner_steps_rejected`, must-stay-0); `Covered` iff the scope's stripes ⊇ the step's; `invariant_tripwires` `xv_local_step_unguarded`; pin `a_served_insert_naming_an_unmintable_child_is_refused_and_plants_nothing` |
| 9 | sugg. | the parked-scope expiry was a timer | with the S6 authority armed a scope lives exactly while its client is a live member (`MembershipOwner::epoch_of`) and is released the sweep after its eviction; the grace window is the belt only without a plane; pins `a_parked_scope_expires_with_its_initiators_membership_lease_when_a_plane_is_armed` + the renamed `…_expire_at_the_grace_belt` |
| 10 | sugg. | each hop a whole-set reverse dentry scan under the lease | the routed backend's directory-parent memo `dir_parents` (dentry-cache derivation; fed by every directory mint / rename / confirmed hop; a hint confirmed exact per hop); the scan is the miss path, `dir_rename_parent_scans` |
| 11 | sugg. | an unconditional per-op CAS on the unarmed path | the scope is a per-op `Option<u64>` minted on the first ARMED acquisition; `kernel_op_economy_tests` green |
| 12 | sugg. | dead / test-only faces | `clear_endpoint` deleted; `roll_forward_open_intents` is the cadence's actual call; `parent_of_directory`, `install/uninstall_xv_shipper`, `manager_dir_rename_release_dead` carry their product-caller line |
| 13 | sugg. | unlink acquired the foreign parent twice (two scopes, an async release between) | the discovery phase acquires no foreign table; pin `every_cross_owner_verb_pays_one_guard_round_trip_per_foreign_holder_table` (create/unlink/link/rename 1, a two-holder rename 2) |
| 14 | sugg. | the stale first-build paragraph; "the first LOCAL step"; a phase table short of exact-sum | the paragraph is gone; every doc says "step 0 when local, the intent alone otherwise"; `local_steps` phase added |
| 15 | sugg. | a live refusal compensated only a create | the plan STOPS at the refusal and undoes every applied half in reverse under the op's guards (a count's inverse CAS, a removed dentry re-inserted, an insert removed, a mint destroyed; a foreign inverse ships under the same scope); pin `a_live_refusal_compensates_a_links_count_and_a_renames_removed_source`. **Stated residue**: a kill between an applied step and the LAST inverse (which carries the retirement when local — round 2, Issue 26; until the separate retirement when the last inverse is foreign) leaves the intent open and the roll-forward applies the FORWARD plan — the C9/C10 census classes; the recovery-side abort marker is owed (§8a) |
| 16 | sugg. | the summary listed `kv/backend.rs`'s three line edits as additive | named as edits in the summary's rebase section |
| 17 | nit | "D18 arms retired" overclaimed | title / heading / design row / note say "D18 reversed (the D19/D20 arms stay for the flat recipe until PR 12/14)"; `cc6d416e`'s commit subject predates the reword and is not rewritten (unpushed history the review already cites) |
| 18 | nit | `since_ns` monotonic; `term` claimed a check nobody made | `since_ns` is CLOCK_REALTIME; `term` is the serving manager's era, provenance the release-dead log names |
| 19 | nit | an absent link broke out of the retry; the served `LookupExact` skipped the lease check | both fixed |
| 20 | nit | a process-global misdelivery flag | a per-request op-id marker |

Every gauge added by the round is on the stats inode and in `docs/operations.md` (`docs_parity_tests` green): `xv_cross_owner_steps_rejected`, `dir_rename_parent_scans`, the `local_steps` phase. New seams: `TEST_XV_SERVE_HOLD_MS`, `TEST_XV_CADENCE_HOLD_AFTER_SCAN_MS`, `TEST_XV_SERVE_SKIP_ONCE`, `TEST_DIR_RENAME_IDENTITY_ONCE`. No knob.

### 8b. Run ledger (round 2, the final tree `cbfdef6d`)

| Run | Result |
|---|---|
| `cargo fmt --check` (root, `fuzz/`) | clean / clean |
| `cargo clippy --all-targets --all-features -- -D warnings` / shipped config | clean / clean |
| `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps` | **green** (red at round 1 on three links; two more caught and fixed inside this round) |
| `fuzz/` `cargo check` | clean |
| `sym_cross_owner_tests` ×10 stamped from zero, `--test-threads=1` | 10/10 — 23 passed each, 11.6–13.2 s |
| `sym_cross_owner_tests` ×3 flat | 3/3 — 13.0 / 20.4 / 22.9 s |
| `sym_cross_owner_tests` `--test-threads=4`, flat / stamped | 23/23 (22.1 s) / 23/23 (15.6 s) |
| `crossvol_tx_tests` / `pv_partial_open_tests` / `rename_lock_set_tests` / `meta_ship_tests` / `posix_semantics_tests` / `sym_slot_transfer_tests` / `kernel_op_economy_tests` / `docs_parity_tests` / `env_knob_convention_tests` / `derivation_sweep_tests` / `decoder_property_tests`, flat AND stamped | 14 / 22 / 2 / 15 / 13 / 45 / 3 / 5 / 22 / 54 / 45 — green on both legs |
| `tests/run_sym_forest_suites.sh` (29 suites) | flat PASS, stamped PASS; `sym_cross_owner_tests` 11.3 / 17.6 s (1.56); ONE ratio NOTE `sym_convert_tests` 20.8 / 101.4 s (4.89) — **attributed to box load** (load average 7.4 during the stamped leg, four implementers' builds): re-run alone right after, 20.39 s flat / 20.33 s stamped = 1.00 |
| the live-FUSE trio, both legs, `SQUEEZEFS_TEST_REQUIRE_MOUNT=1` | flat: `posix_mount_semantics_tests` 3 (43 s), `corpse_sweep_tests` 4 (154 s), `inline_raise_tests` 7 (98 s); stamped: 3 (35 s), 4 (147 s), 7 (92 s) — all green, no mount-class skip |
| `tests/check_markdown_links.sh` | PASS |

### 8a. Owed after round 2

- **PR 12 — the intent scan's cross-process face** (review round 2, Issue 21): the scan is scoped to intent homes whose slot this mount's step-home is `Local` for (a peer slot's intent is never adopted here — pinned), and the register is process-local; the design's "roll-forward by whoever recovers the initiator's ring" — a dead initiator's intents adopted by the mount that recovers its ring, an intent INHERITED with a re-leased slot (PR 10's re-lease, an LRU release + first-touch) scanned at the lease install so it registers `Abandoned` — is the join ladder's to build. On this tree one process holds every slot tree, so every intent is its own.

- The recovery-side ABORT marker: a plan a live refusal stopped is compensated under the op's guards, and since review round 2 (Issue 26) the retirement RIDES the last inverse's own entry when that inverse is local to the intent's volume (the rider pattern — compensation + retirement are one commit, pinned: a link's live refusal is `tx0` + ONE entry). The window that remains is a kill between an applied step and that last inverse — and, when the last inverse is FOREIGN (a plan whose step 0 shipped), until the separate retirement lands — after which the roll-forward completes the plan FORWARD (re-meeting the refusal; the applied halves stand for the C9/C10 census, the just-undone half returning because the compensation restored exactly the forward witness). Recording the abort on the intent record and recovering it as an undo is the answer; not built here.
- The lock verbs' caller identity: `appender_id` is screened against the directory (a `Live` non-own page) and the record (the holder), not against the SESSION — the session→appender binding is PR 12's join ladder (PR 4's slot verbs carry the same limitation).

## 9. Review round 3 (2026-09-15) — the seven open items closed

| Issue | The change | Pin |
|---|---|---|
| 21 | the intent scan is SCOPED to homes whose slot this mount's step-home is `Local` for — a peer slot's intent is never adopted here; the cross-process face (a dead initiator's intents adopted by the mount that recovers its ring; an intent inherited with a re-leased slot scanned at its lease install) is PR 12's obligation (§8a, the design row) | `an_intent_homed_in_a_slot_another_appender_leases_is_never_adopted_here` (a record planted in the declared region's slot home: the raw scan sees it, the roll-forward adopts nothing) |
| 22 | the lease-gated sweep keeps a client the installed owner does NOT know for the grace window (the doc's law; the code released it at the next tick) — the safety argument on the line: an early release costs isolation, never correctness | the plane-armed contract gained the unknown-client leg |
| 23 | the ticket JOIN is gone: one permit per `(volume uuid, identity)` — the lease serializes OPS; the second directory rename of one identity waits for the first's release; no dependency on the kernel's `s_vfs_rename_mutex` (an S8-served `Rename` and the offline `RoutedMetaBackend` callers never pass through it) | `two_directory_renames_of_one_identity_serialize_under_the_lease` (disjoint 4a sets; red pre-fix: r2 joined and completed beside r1) |
| 24 | keyed by the volume's durable uuid, never an `Arc` address; the permit is RAII on the lease and drops in the release's own scope (also on the `Drop` path) | — (the ticket counter no longer exists) |
| 25 | `b`'s name chosen off `D{root,shared1}`'s stripe and the fixed pair asserted disjoint; pins for the coverage-checked `Covered` (a step outside its scope's parked keys PARKS behind a local holder — verified to fail with the coverage test removed, reverted) and the `xv_local_step_unguarded` tripwire (a plan on B under guards acquired over A trips once; the covered shape never) | `a_step_outside_its_scopes_parked_keys_takes_its_own_guards_and_parks`, `a_local_step_outside_its_scopes_keys_trips_the_invariant_tripwire` |
| 26 | the retirement RIDES the last local inverse's own entry (compensation + retirement = ONE commit, `xv_destroy_unnamed` takes the rider too); the remaining window stated to its true end — before that inverse, or before the separate retirement when the last inverse is foreign | the compensation pin asserts a link's live refusal is `tx0` + ONE journal entry |
| 27 | `mem_budget::dir_entry_capacity` is the ONE derivation the FUSE dentry cache, its `..` memo and the directory-parent memo size by (the `fuse_client.rs` site is a one-line call — noted for the rebase); tie test `dir_entry_capacity_is_one_derivation_with_the_shipped_floor`; the AGENTS count says 28 | `derivation_sweep_tests` |

### 9a. Run ledger (round 3, the final tree)

Filled in the round-3 implementation summary in the review file; the same table.

