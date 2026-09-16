# Symmetric metadata — PR 10: dead-appender recovery (2026-09-16)

Branch `feat/sym-recovery` from `dev` @ `c4cb3730` (PR 1/2/3/4/5/6/7/8/11/16
in). Design `docs/design-symmetric-metadata.md` §5.5.2 / §5.5.3 / §5.8.2 /
§5.8.3 / §5.8.4 / §5.8.5 / §5.9 / §6.2 / §11; PR-plan row 10. Dark behind
incompat bit 17 AND `SQUEEZEFS_SYMMETRIC_META=1`.

**Venue: the dev laptop (AMD Ryzen AI MAX+ PRO 395, 32 CPUs), file-backed
volumes on tmpfs-class storage, the `test` (debug) profile — SCOPING ONLY.**
Every number here is mechanism engagement and shape, never acceptance; the
box bracket is PR 13/14's (the venue rule). A `--release` build of the
scoping instrument on the shared laptop (two other implementers building)
did not finish inside 40 min and was abandoned — the debug numbers are
what this note carries.

## 1. What landed

The rung that completes §5.8.4's zero-acked-loss argument — every earlier
PR's death path lands here:

1. **The death ledger's driver.** The S6 owner's eviction (and the grace
   deadline's non-reclaimers — never a reclaimer) is the production writer:
   `membership::install_death_sink` → `KvMetaBackend::record_death_with_key`
   (`dead_member:{node_token, mount_slot} → { epoch, ts, pr_key }` on tree 0
   of volume 0). PR 8's reserved wire `RecordDeath` is ACTIVATED and
   SCREENED (`screen_record_death`: the manager's own identity or a live
   member of the installed census is `STATUS_REJECTED`). `DeadMemberRecord`
   grew `pr_key` (25 B; v1's 17 B decodes with key 0).
2. **The recovery driver** (`src/meta_backend/kv/backend/recovery.rs`):
   every manager polls volume 0's ledger as a projection at the checkpoint
   landing ceiling and runs §5.9 per dead region — preempt (PR) → page
   `Recovering` → ring read + the violation classes → the RAM lease table's
   release → two-phase replay in the structural lease class (+ PR 8's
   data-bitmap deltas) → flush → tails (EVERY leaf on non-PR, the interior
   walk only — a non-resident leaf costs one extent read) → tree 0
   `Unleased { root, cursor per §5.1.8, g, tails }` → grant + orphan images
   returned → page `Recovered` → `recovered:{X, v}` → the dead manager's
   `dir_rename` released → parked acquires woken → open cross-owner intents
   rolled forward. A `Recovered` region no allocation lease names is
   RELEASED at the next projection (page `Free`, ring back to the heap).
3. **Manager death**: the D0 ladder's successor (PR 3) → grace →
   non-reclaimers only; the vol-0 rule's release stops the claim refresh
   (the ladder re-elects); `membership::observe_successor` reads the home
   volume's rendezvous record at every parked beat (PR 8's seam's
   production writer).
4. **C14 / C15** in `fsck.rs` off `slot_custody_census`; the mount-path
   gate (`recovery::arm`, before `arm_symmetric_allocation`) recovers
   ledgered regions before serving and refuses an unledgered `Recovering`
   page naming the verb.
5. **`squeezefs appender clear <sqmeta-uri> <id> [--volume N]`** — the
   `claim clear` law for an appender page.
6. **Owed items**: PR 8's seam writer re-scoped (the key-less
   `record_death` is the same-node takeover's form); PR 2's foreign-`Live`
   refusal narrowed to the partition-claimed page and an own `Recovering`
   page made own residue; PR 5's screen extended to NON-WRITER opens off
   tree 0; PR 6's intents adopted after a recovery; PR 4's flake signatures
   not hit in this rung's matrix runs (see §5).
7. `tests/sym_crash_matrix_tests.rs` (15 contracts + the scoping
   instrument), `mw_fleet.sh --symmetric`, `run_mw_matrix.sh sym-crash`.
8. Docs: operations.md (the recovery section + the guarantee table's two
   death rows), AGENTS.md, design row 10, this note, the stats JSON.

## 2. Found on the way (product defects, fixed)

| # | Finding | Where | Fix |
|---|---|---|---|
| F1 | **An own-node `Recovering` page got a FRESH ring** — a recoverer that died inside our predecessor's recovery, then our remount: `open_appender_regions` treated only `Live` as own residue, so the declared region's acked window was never replayed (acked loss). | PR 2's `open_appender_regions` | `Live \| Recovering` of our node is own residue; pinned `an_own_recovering_page_is_own_residue_and_the_join_takes_it_back`. |
| F2 | **Offline fsck's raw C1 walk FOLDED a zombie frame every writer screened** and reported it as C1 damage (`key ordering violated`, a refused key) on a volume a writer reads clean: a probe has no lease plane, so `frame_screen_for` answered `None` (rule 3 alone). | PR 5's screen on non-writer postures | `reader_frame_screen_for` off tree 0 (`slot_state`'s `(g, lessee)` + `slot_tails:{s}`, cached per slot against the control root), installed at the four non-writer opens; pinned by the zombie contract's `fsck_clean`. |
| F3 | **The tail scan read 0 bytes**: `reachable_node_addrs` LOADS every leaf, so every leaf was resident when the scan asked and `recovery_full_tail_scan_bytes` never moved — and every leaf was materialized for a tail peek. | PR 10's first build | `leaf_addrs_unloaded` walks the interior population only; a non-resident leaf costs `peek_tail_offset`'s one extent read. |
| F4 | **The inode plane judged a set with an unreplayed foreign window**: the C14 contract's probe read six healthy cross-owner children as C9-unreferenced — their dentries live in the dead holder's ring, their records in the creator's slot. | PR 8's `inode_plane_owns_slot` scoping (by the INO's slot) | `foreign_windows_pending` gates C9/C10 to no verdict while a foreign `Live`/`Recovering` page has a window; C15 names the dead one. |
| F5 | **The C14 census reported one conflict twice** (a second Live page and tree 0's lease name the same pair). | PR 10's first build | One finding per unordered pair per slot. |
| F6 | **A fresh `writer_claim` blocks offline fsck for the TTL after a kill** (by design — the preflight's freshness law) — the crash contracts could not reach the census. | harness | `fsck::run_offline_over` — the body over a caller-held probe (the CLI keeps the preflight). |
| F7 | **A crashed incarnation's block-grant window LEAKED for ever** — the `sym-crash` leg's first round read `C6: used-blocks accounting 386 vs 385`: the grant window is RAM, so a kill left its granted-but-unminted tail and its minted-but-unpublished blocks SET in the allocation bitmap with no reference and no open grant — the bitmap oracle's LEAK half PR 8 stated (`DataAllocBitmap::drift`) and wired nowhere; and C6's `highest − free_list` arithmetic is single-writer's (the bitmap IS the free list on a grant-armed allocator — the drained list makes every bitmap-clear block read "used"). | PR 8's crash path | `arm_symmetric_allocation` clears every such bit at the (re-)hold with journaled deltas (`data_alloc_bitmap_leaks_released`; pinned red-first by `sym_block_grant_tests::a_crashed_incarnations_window_remainder_is_released_at_the_rehold` — 0 released without the arm); fsck's C6 DECLINES on a grant-armed allocator (`fsck_foreign_lane_exempted`, the lane partition's doctrine). On the fleet: 156 blocks released at the round-1 re-hold (77 + 79 over two data volumes), 121 at the round-10 re-hold. |

## 3. Recovery phase ns vs the dead window (SCOPING, debug profile)

`scoping_recovery_phase_ns_vs_window` (`#[ignore]`d): one stamped 64 MiB
volume (64 KiB nodes, 1 MiB fixed ring), a declared region killed with N
shipped creates in its ring, the ledger written, ONE projection. µs per
phase (delta of `appender_recovery_phase_ns`), the derived bound and
`dead_member_propagation_ms` (record `ts` → acted):

| files | entries | preempt | read | replay | flush | tails | tree0 | **total µs** | bound_ms | prop_ms |
|---|---|---|---|---|---|---|---|---|---|---|
| 12 | 12 | 9 | 618 | 337 | 1,938 | 11 | 7,192 | **10,845** | 1,108 | 14 |
| 200 | 200 | 6 | 1,083 | 1,673 | 2,175 | 11 | 7,949 | **13,757** | 1,108 | 18 |
| 600 | 601 | 5 | 2,301 | 9,259 | 10,463 | 10 | 9,897 | **32,669** | 1,108 | 36 |

Reading: the exact-sum law holds (Σ phases ≤ total); `tree0` dominates at
small windows (the control entries' write + barrier — an fdatasync on the
file-backed volume); `replay` and `flush` scale with the window (≈ 15 µs
per entry replayed, debug); `preempt` is a no-op on a file-backed volume
(no PR); `tails` is a single-leaf tree here. The derived bound (1,108 ms:
≈ 1 MiB ÷ 230 B = 4,559 entries → 23 leaves × 120 µs + 4.6 ms fold +
the 1,100 ms landing ceiling) is 34× the measured total at 600 entries —
the bound is the LANDING CEILING's, which the in-process `checkpoint_cycle`
(a direct call, no cadence wait) never pays; on a live mount the flush
phase rides the cadence and the bound is the honest operator number.
Propagation 14–36 ms in-process (the wire adds an RTT per hop — PR 12's
row).

## 4. Multi-death and multi-volume (SCOPING)

- `a_node_holding_regions_on_three_volumes_is_recovered_on_all_three`: ONE
  `dead_member:` record, three volumes each with a Live region of the dead
  node → `recover_dead_appenders_set` recovers all three in one projection,
  `dead_members_acted` +3 (`acted ≡ recorded × regions held`).
- `eight_simultaneous_deaths_are_recovered_by_one_projection`: a 128 MiB
  member, 7 declared regions (slots 101..119) + one wire joiner (routing
  slot 300); eight records, one projection, eight recoveries, 8 acts; every
  acked name of every dead region resolves. Both rows together: **1.85 s
  wall** in the debug profile (the whole suite, 15 contracts: ≈ 10 s).

## 5. The matrix, flat and stamped

- `tests/sym_crash_matrix_tests.rs`: 15/15 green (`--test-threads=1`),
  plus the `#[ignore]`d scoping instrument. Every row a deterministic kill
  through the existing seams: `SQUEEZEFS_TEST_SYM_APPENDER_SLOTS` (the
  declared region), PR 6's wire venue (`HoldersVenue`) for the served
  create whose only home is the dead ring, `TEST_XV_SERVE_REFUSE` /
  `TEST_XV_STUCK_AFTER_MS` (the open intent), `TEST_RECOVERY_HALT_AFTER_
  RECOVERING_PAGE` (the recoverer's death), `test_plant_dir_rename_record`
  (the failover fixture), `restamp_page_identity` / `rewrite_page` (the
  page forgeries).
- Re-scoped: `sym_appender_tests` (two PR 2 refusals → the ledger's driver
  + the verb; own `Recovering` = own residue), `sym_block_grant_tests`
  (the wire serves `RecordDeath`), `decoder_property_tests` + the
  `manager_call_frame` fuzz target (`pr_key`).
- Touched suites re-run green: `sym_appender_tests` 34, `sym_coherence_tests`
  36, `sym_forest_tests` 31, `sym_manager_tests` 29, `sym_slot_transfer_tests`
  45, `readonly_mount_tests` 35, `sym_block_grant_tests` 55,
  `decoder_property_tests` 37, `fsck_c9_tests` 18, `fsck_c10_tests` 12,
  `derivation_sweep_tests` (the new tie).
- PR 4's three flake signatures (`grok-exec-review-dadee1dd-pr-8.md`
  Issue 26) did not fire in this rung's runs of `sym_slot_transfer_tests`
  (45/45 ×1 flat); the attribution stays owed to its owner — this rung's
  fixtures do not depend on the handover cadence's timing.
- `tests/run_sym_forest_suites.sh` grew `sym_crash_matrix_tests` (34
  suites): **flat PASS / stamped PASS**, no ratio NOTE (the largest ratio
  1.31 on the 4 s `meta_slot_migration_tests` — under the runner's own
  "a suite under 5 s is scheduler noise" rule; `sym_crash_matrix_tests`
  10.8 / 10.7 s = 0.99). `sym_crash_matrix_tests` **×10 stamped from
  zero: 10/10** (15 passed each, 10.4–10.7 s). The live-FUSE trio under
  `SQUEEZEFS_TEST_REQUIRE_MOUNT=1`, both legs: stamped
  `posix_mount_semantics_tests` 3 (46 s) / `corpse_sweep_tests` 4 /
  `inline_raise_tests` 7 (127 s); flat `corpse_sweep_tests` 4 (183 s),
  `inline_raise_tests` 7 (116 s), `posix_mount_semantics_tests` 3 (54 s)
  on its re-run — its first flat run (concurrent with the 34-suite
  matrix, the ×10 loop and the fleet legs; load average 18) failed ONE
  contract at the harness's 90 s mount-readiness bound
  (`mount_statvfs_ifree_recovers_after_create_delete_loop`: "mount did
  not become ready in 90s", a debug daemon under the box load) — 2/3 then
  3/3 alone; no mount-class skip on any leg. Fidelity `quick` (root,
  nvmet): **PASS 43 / FAIL 0** in 1m37s; fidelity `full` (the `pr-matrix`
  preempt leg included): **PASS 115 / FAIL 0** in 2m43s.

## 6. Fleet legs (LOCAL, the tcp devsub — nvmet-tcp on 127.0.0.1, a PR substrate)

`sudo SQZ_MWFLEET_OSS_GB=24 tests/mw_fleet.sh create N=2 --symmetric
--lease-ttl-ms=15000` (format `--symmetric`, the manager armed with
`SQUEEZEFS_SYMMETRIC_META=1`, the S6 plane implied, a reader member; the
release binary of `3d71717d`) then `sudo tests/run_mw_matrix.sh sym-crash
--rounds=10`: kill -9 of the manager at a randomized phase (0.5–8.5 s)
into a sustained `dd conv=fsync` load, dead-mount sweep, successor
remount, the per-round successor asserts (own-residue recovery ≥ 1, the
dead incarnation's `Live` page listed, `manager_lease` `held` and
`symmetric_meta` 1 on every volume, `writer_guard_mode` `flock+pr` — the
metadata PR the death path's preempt fences — and every symmetric
must-stay-0 gauge 0: `meta_kv_forest_key_violations`,
`appender_fence_breach`, `foreign_frame_overwrite_detected`,
`manager_verb_refusals`, the three replay violation classes,
`fsck_slot_custody_conflicts`, `fsck_unrecovered_appenders`,
`appender_park_expiries`, `meta_kv_leaf_lease_refusals`,
`dlm_token_recall_timeouts_live`, `appender_flush_ceiling_overruns`;
`appender_recoveries` 0 — one appender, nothing foreign dies) + the FULL
online fsck with the C8 oracle (C14/C15 riding it) + `meta_kv_block_refs_
drift` 0 + `data_dma_fence_refusals` 0 + `invariant_tripwires` 0 + the R5
backstops 0; the reader re-joins at the matrix end.

**GREEN 10/10 from zero** (`/run/squeezefs-mwfleet/rows/symcrash-1789549107`):

| round | kill phase ms | remount s | fsck | C8 drift | fence refusals | tripwires |
|---|---|---|---|---|---|---|
| 1 | 4,611 | 1 | findings: 0 | 0 | 0 | 0 |
| 2 | 766 | 1 | 0 | 0 | 0 | 0 |
| 3 | 6,256 | 1 | 0 | 0 | 0 | 0 |
| 4 | 7,916 | 1 | 0 | 0 | 0 | 0 |
| 5 | 7,310 | 2 | 0 | 0 | 0 | 0 |
| 6 | 1,072 | 1 | 0 | 0 | 0 | 0 |
| 7 | 1,451 | 1 | 0 | 0 | 0 | 0 |
| 8 | 7,853 | 1 | 0 | 0 | 0 | 0 |
| 9 | 6,162 | 1 | 0 | 0 | 0 | 0 |
| 10 | 3,524 | 1 | 0 | 0 | 0 | 0 |

Final successor: `appender_self_recoveries` [1, 1], `appender_live_pages_
at_mount` [1, 1], `data_alloc_bitmap_leaks_released` 121 (the dead dd's
window remainder over two data volumes), `data_alloc_bitmap_drift` 0,
`writer_guard_pr_reacquires` [0, 0], `recovery_ledger_polls` 19,
`appender_recovery_bound_ms` [1,185, 1,185] (a 2 MiB fixed ring at 256 KiB
nodes under the shipped cadence). Teardown: zero residue.

**The counted-restart history (every earlier attempt aborted the count):**
(1) the first build gated the sym leg on `data_plane_fence_mode` at the
mount — on a symmetric-only fleet that gauge is the JOB WIRE's WERO,
re-acquired over the dead incarnation's registration on its own retry
cadence (≈ 40 s measured), not this plane's guarantee → the leg gates on
`writer_guard_mode` = `flock+pr` (the metadata PR); (2) `ENOSPC` inside the
kill phase on the default 2 × 4 GiB OSS (≈ 1.3 GB/s of zeros into zram) →
`SQZ_MWFLEET_OSS_GB=24`; (3) **round 1's fsck read C6 `386 vs 385`** — F7
above, the real defect, fixed red-first; (4) the 30 s bounded wait for the
job wire's WERO expired → the `flock+pr` gate. A FOREIGN appender's recovery
over the wire needs N daemons on one volume — PR 12's venue.

## 7. Stats

Appended at the END of the symmetric block: `appender_recoveries`,
`appender_recovery_phase_ns` (exact-sum), `appender_recovery_bound_ms`
(per volume), `recovery_full_tail_scan_bytes`, `appender_recovery_preempts`,
`recovered_regions_released`, `appender_clear_runs`, `recovery_ledger_polls`,
`recovery_intents_rolled_forward`, `fsck_slot_custody_conflicts`
(must-stay-0), `fsck_unrecovered_appenders`, `fsck_repair_classC15`,
`data_alloc_bitmap_leaks_released`. PR 8's
`dead_members_recorded` / `_acted` / `recovered_records` /
`dead_member_propagation_ms` are driven by the plane now. All 0 on an
unarmed or bit-17-absent mount.

## 8. Deviations (stated in the design row)

- The in-process "another node died" fixture is drop-without-shutdown +
  the page's identity restamped a foreign node's; the killed holder is PR
  6's wire venue in the same process.
- The busy-appender-across-a-manager-failover row and the 8-home-shard-
  members storm are N-daemon shapes (PR 12's venue); their halves are
  pinned (the grace writer records only non-reclaimers; the successor
  observation; PR 8's park-and-reclaim).
- The "device rejection past TTL" row is the fidelity tier's preempt leg
  (root, the nvmet substrate); the in-process preempt is a no-op on a
  file-backed volume and counted 0.
- PR 2's drain-then-grow stays owed.
- `appender_recoveries` is 0 by construction on the one-appender fleet leg.

## 9. Review round 2 (2026-09-16) — the two-backend fixture and the nine bugs

The review's structural finding governed the round: the one-backend fixture
(one process = one RAM tree, one actor) could not reach the driver's real
defects. `tests/common/sym.rs` grew the **region-image fixture**
(`capture_region_image` / `apply_region_image` over
`KvMetaBackend::region_device_ranges` — a region's ring segments, its two
directory page slots, its grant's image extents) and the crash matrix's
`two_backends()`: backend A (the manager + region 1 as PR 6's wire holder)
checkpoints (image 1), keeps writing until its flush pass MOVES the slot's
root, checkpoints again (image 2's page names the newer root), writes a
window past its page's tail and dies; image 1 is re-applied under a foreign
identity, backend B — the recoverer — opens with the slot at the OLDER root,
then image 2 lands under B. B's RAM tree is stale against the device exactly
as another daemon's would be. Every round-2 pin runs on it.

| # | Class | Fix (red-first) | Pin |
|---|---|---|---|
| 1 | bug | the open takes the newest root the node's own pages name for an unleased guest; the page-root pass installs writer-legal; `SlotTrees::new` seeds `published` with TREE 0's root (the opened-root seed kept tree 0 stale for ever — the clean leave then dropped 25 dentries of a create storm) | `a_kill_between_a_page_write_and_the_next_publication_remounts_an_unarmed_forest` (bounded rounds until a page-0 root is ahead; every name through the remount AND a probe after its leave) |
| 2 | bug | `NodeCache::drop_slot_nodes` + `KvTree::install_recovered_root` under the SMO mutex | `a_recoverer_whose_ram_tree_is_stale_installs_the_dead_lessees_newer_root` |
| 3 | bug | the handover order: `Releasing { dead }` first, every durable step, drain-and-retry admission for tree 0 under the SMO mutex, the table `Unleased` LAST; `RecoveryRollback`; a re-run past tree 0 treats the released slots as absorbed; a `Recovered` page without its `recovered:` record is completed before the release law; `publish_forest_roots` never writes `Unleased { g: 0 }` over a leased slot | `a_recovery_that_fails_at_any_step_resumes_from_the_durable_state_without_loss` (steps 3–9), `a_first_touch_acquire_during_a_recovery_is_refused_and_never_regresses_tree_0` |
| 4 | bug | `pr_fenced` iff the metadata preempt of a non-zero key landed | the `pr-matrix` leg (landed) + `appender clear`'s key-0 shape taking the scan (`a_zombie_frame_on_an_untouched_leaf_is_screened`) |
| 5 | bug | the death write pre-admits PARKING; a failed write is parked in RAM and retried by the poll (`dead_member_write_deferrals`) | `a_deferred_death_record_lands_at_the_next_ledger_poll` |
| 6 | bug | `appender clear` refuses a fresh `writer_claim` on the volume and on volume 0, from a probe read before either writer open | `appender_clear_refuses_a_fresh_writer_claim_on_volume_0` + the C14 contract's killed-claim arm |
| 7 | bug | the key word screened against the census's registered key (kept through departure), own keys, live keys; unknown ⇒ 0 | the eviction contract's (d) arm + `screen_death_key`'s table + the fuzz arm |
| 8 | bug | the page re-read under the handover mutex; state/identity/term moved ⇒ skipped | `a_page_that_moves_under_the_polls_snapshot_is_skipped_not_recovered` |
| 9 | bug | `retire_death_record` at the rejoin (the writer's arm), at the poll (a live member), by the sweep past `2 × T_owner` | `a_rejoined_member_is_never_recovered_and_its_record_is_retired` |
| 10 | sugg | the orphan census walks the RELEASED slots' trees without loading a leaf; the bound prices three leaf passes | the derivation tie |
| 11 | sugg | C6 runs the bitmap oracle on a grant-armed allocator; 11a/11b stated in operations.md | — |
| 12 | sugg | C9/C10 scope out the inos foreign windows name (`fsck_inode_plane_window_scoped`) | the C14 contract's probe (six healthy children judged, none reported) |
| 13 | sugg | the unarmed pin | `a_flat_volume_and_an_unarmed_forest_arm_no_recovery_driver` |
| 14 | sugg | the fleet leg's acked-writes oracle (`run_mw_matrix.sh sym-crash`) | the ×10 run below |
| 15 | sugg | the non-writer screen applies rules 2 and 3 only | the zombie contract (probe) |
| 16 | sugg | RACQA 2 — `preempt_and_abort_registrants_only` on the death path | the `pr-matrix` leg |
| 17 | sugg | the dead-initiator half stated (covered by the roll-forward arm; PR 12's venue) | — |
| 18 | sugg | `dead_members_quarantined` split off `dead_members_acted` | — |
| 19 | sugg | one volume-0 resolver (`recovery::vol0_of`) for the driver and fsck | — |
| 20 | nit | `InteriorReplay` (no clippy allow), the doc, `test_clear_death_sinks`, no library `unwrap()` | — |
| 21 | nit | the bound reads the landing ceiling resolved at open; the mount-path census skips the ring reads; the intent contract polls `intents_stuck` | — |

**Found beside the fixes (shipped, every layout)**: `open_probe`'s
`shutdown` took the writer branch and ran `checkpoint_now()` — a probe wrote
a ledger record (and, once `published` was seeded from tree 0, re-published
a leased slot's root as `Unleased { g: 0 }`) over a volume it holds no lock
on. Every non-writer door writes nothing at teardown.
