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
`KvMetaBackend::test_region_device_ranges` — a region's ring segments, its two
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
on. Round 2 keyed the no-write arm on `non_writer`; **that key was WRONG**
(a §4.11-degraded WRITE mount shares it — its whole teardown ladder skipped)
and was DROPPED at the round-3 rebase for PR 7b's `KvMetaBackend.probe`
field, set by `open_probe` alone (§10).

**Round 2 verification (the final tree, `CARGO_INCREMENTAL=0`, the shared
laptop)**: fmt / both clippy configs / rustdoc `-D warnings` / the fuzz
workspace's check+fmt / loom 113 models — clean; `sym_crash_matrix_tests`
stamped ×10 + flat ×3 from zero (24 contracts) — green; `corpse_sweep_tests`
stamped ×10 — 10/10 (round 1: 3/4, the routed Issue 1); the nine named
suites both layouts + `docs_parity` / `env_knob_convention` /
`derivation_sweep` / `decoder_property` — green; the 34-suite matrix
flat→stamped AND stamped→flat — PASS; the live-FUSE trio both ways under
`SQUEEZEFS_TEST_REQUIRE_MOUNT=1` — green, no skip; fidelity `quick` 43/0 +
`full` 115/0 (root, nvmet); **`sym-crash` ×10 from zero on the tcp devsub
WITH the acked-writes oracle — 10/10, 426 ledgered files across the ten
kills, every one present with its content on the successor**, every
per-round must-stay-0 gauge 0 (incl. `dead_member_write_deferrals`,
`data_alloc_bitmap_drift`). (Round 2's oracle word was `sync -f` =
`syncfs(2)`, which the FUSE fork does not serve — "create acked + writeback
pushed", exact for a process kill, an overclaim for power loss; round 3's
oracle is a per-file `fsync(2)` that returned — §10.)

## 10. Review round 3 (2026-09-16) — the rebase onto PR 7b and Issues 22–27

**The rebase.** `git rebase --onto 6c80d70f c4cb3730` replayed the 19
round-1/2 commits onto the dev tip carrying PR 7b (directory striping,
`bddea3e3`). Conflicts and their composition, theirs (7b) then ours:

| File | Hunks | Composition |
|---|---|---|
| `src/fsck.rs` | 13 (+1 at `c559f52b`) | 7b's C17 `FindingId` / `SuspectKind` / counters / merge sites / confirm arm / plan text, then PR 10's C14 / C15 — additive; the nomination call order `evaluate_c16 → evaluate_c17 → evaluate_c14_c15`; `foreign_windows_pending`'s header (superseded in round 2) dropped for `foreign_window_inos`; the inode-plane verdict gate now states BOTH laws in one comment — 7b's blanket no-verdict on a NON-WRITER with a `Live` page (`build_referenced_inos` → `None`) fires first, PR 10's per-ino window scoping governs the WRITER's online plane |
| `src/fuse_client.rs` | 1 | 7b's Striping family block, then PR 10's Recovery family block (the stats order `docs_parity_tests` decides — green) |
| `tests/run_sym_forest_suites.sh` | 2 | `sym_dir_stripe_tests` then `sym_crash_matrix_tests` — **35 suites** |
| `docs/operations.md` | 1 | 7b's striping section, then PR 10's recovery section |
| `src/meta_backend/kv/backend.rs` | 1 (at `c559f52b`) | **Issue 22**: PR 10's `self.non_writer \|\|` shutdown predicate DROPPED, 7b's `self.probe \|\|` (the field, its init, `open_probe`'s set, the predicate + comment) kept verbatim; 7b's `manager_extent_grant_class` shutdown-refill gate and PR 10's `pre_admit_control_parking` / `admit_control_drain_and_retry` compose additively |
| `meta_ship/` enums | none | 7b's 0x90 block is `MetaCall`'s (`SupplyStripeIno` / `IsEmpty` / `DestroyStripe`), PR 10's activation is `ManagerCall::RecordDeath` (PR 8's reserved variant) — different enums, no split-variant shape; the fuzz mirror compiles and runs |
| `AGENTS.md`, `docs/design-symmetric-metadata.md`, `loom-models/src/lib.rs` | auto-merged | 7b's paragraph / row then PR 10's; the loom crate untouched by both |

**The 7b seams, checked.** (a) A dead lessee's slot tree may hold a
STRIPED directory — its `K + 2` reserved-name marker dentries and the
stripe inos' dentry sets: the §5.9 steps are kind-blind by construction
(`replay_dead_window` applies every record by `(kind, key)`; the tails
walk leaves; tree 0 writes per slot), confirmed by the new contract
`a_striped_directory_in_a_dead_lessees_slot_recovers_with_its_map_and_c17_clean`
— the declared holder flips its directory into 4 stripes, 24 names land in
the stripes through the cross-owner shipped steps, the holder dies, the
recovery replays the window (`entries ≥ 1`), the map reads `k = 4`, every
name resolves through the stripes, a post-recovery create routes into its
stripe, and offline fsck is clean with **C17 `stripe_findings` 0** beside
C14/C15 and the inode plane covered. (b) 7b's `IsEmpty { scope: 0 }` on a
remote holder under the S8 owner-execute context is stated owed to PR 12
by 7b — no action here.

**Issues 22–27 as built.**

| # | Class | Fix | Pin (red-first where stated) |
|---|---|---|---|
| 22 | bug | resolved BY the rebase — 7b's `probe` field is the shutdown's no-write key; PR 10's `non_writer` key dropped (a §4.11-degraded WRITE mount shares it); every doc reworded to 7b's posture | PR 10's `a_kill_between_a_page_write_and_the_next_publication_remounts_an_unarmed_forest` GREEN against 7b's predicate; 7b's three probe pins GREEN |
| 23 | bug | `RecoveryRollback` restores EVERY RAM word step 4 changed as one RAII: the table's state (`abort_release`), the gate's `foreign` bit as step 4 FOUND it (`begun: Vec<(slot, was_foreign)>`), the door's waiters; the installed root / RAM tree (idempotent) and the extent ledger (max-only) stay — enumerated in the type's doc | `a_failed_recovery_leaves_the_dead_lessees_tree_foreign_to_the_managers_sweep` — fail at 5 / 6 / 7, after each: `is_foreign`, `Holder { dead }`, one `defrag_merge_sweep` skips the tree (`merge_sweep_foreign_skips` moved, `META_KV_NODE_MERGES` and `META_KV_LEAF_LEASE_REFUSALS` not); RED with `mark_foreign` reverted |
| 24 | bug | `KvTree::install_recovered_root` is the ONE async install both the open's page-root pass and the driver run — the root node read and its `node_seq` checked against the pointer FIRST (torn ⇒ `Corrupt`, nothing installed), pinned and slot-stamped, the floor set, `seq.fetch_max(root.seq)`; `open_unpublished_slot_tree` is the fresh-tree arm (open + floor + the same raise); the orphan images' residue stamps floor the handle before their extents return (`node::residue_seq_ceiling`, one extent read per orphan) | `the_recovered_root_install_raises_the_node_seq_handle_and_refuses_a_stale_pointer` (RED with the `fetch_max` reverted: handle stayed 0 under a seq-42 root); `an_empty_window_death_leaves_the_recoverers_node_seqs_above_the_recovered_trees` (the `TwoBackendsWindow::Empty` fixture — green on both sides here, stated: the lessee and the recoverer shared one handle through the ledger's watermark) |
| 25 | sugg | the cross-shard rejoin exposure stated as PR 12's obligation — design row 10 (exact text), operations.md's owed list, AGENTS.md | — |
| 26 | sugg | `foreign_window_inos` resolves a dentry `Delete`'s child through `lookup_kind` in the parent's tree (the window is unreplayed, the pre-delete dentry stands) | `a_dentry_delete_in_a_foreign_window_scopes_its_child_out_of_the_inode_plane` (`TwoBackendsWindow::UnlinkOne`; the recoverer's ONLINE plane through the new `inode_plane_over` helper); RED before with `C10ZeroNlinkNamed { ino: 67 }` "DATA-LOSS RISK" |
| 27 | nit | `DEPARTED_KEYS_MAX` gone — the memo is bounded by AGE, `alloc_lease::death_record_retire_age_ms` = `DEATH_RECORD_RETIRE_TTLS` (2, reason on the constant) × `T_owner`, the sweep's own age (ONE law, two readers); `test_pr_key` / `test_region_device_ranges` / `test_pending_deaths`; `admit_control_drain_and_retry` answers `Busy` when the tail moved, `Corrupt` only when it did not; the fleet oracle's "acked" is a per-file `fsync(2)` that returned (`dd conv=fsync`, never `sync -f` = `syncfs`, unserved by the fork) | `derivation_sweep_tests::sym_death_record_retire_age_ties_the_sweep_and_the_departed_key_memo` (90 s at the shipped TTL; an evicted member's key answers at the age and not one tick past); the `sym-crash ×3` below |

The crash matrix is **29 contracts** (+1 ignored instrument) after this
round; the two-backend fixture grew `TwoBackendsWindow::{Creates(n), Empty,
UnlinkOne}`.

**Round 3 verification (the rebased tree, `CARGO_INCREMENTAL=0`, the dev
laptop alone)**:

| Leg | Result |
|---|---|
| `cargo fmt --check` (root; the fuzz workspace's own) | clean |
| `cargo clippy --all-targets --all-features -- -D warnings` / `cargo clippy --all-targets -- -D warnings` | clean / clean |
| `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps` | clean |
| `fuzz/`: `cargo check` + `cargo fmt --check` | clean |
| `task check:loom` + `tests/run_loom.sh` | clean; **113 / 113** models |
| `sym_crash_matrix_tests` stamped ×5 + flat ×2 from zero (29 contracts + the `#[ignore]`d instrument) | **7 / 7** (20.9–22.8 s) |
| `sym_dir_stripe_tests` (7b's, incl. the striped-slot recovery seam) both layouts | stamped 27/27; flat 27/27 ×3 + the two matrix flat legs — **ONE HANG** in the first flat run (`stripe_dirs_at_mkdir_stripes_every_new_directory`, parked inside its closing `fsck_clean` → `run_offline` for 18 min at 2 % CPU, every thread in a futex/epoll park, killed; the thread dump `/tmp/grok-justin/pr10-r3/hang-bt.txt`) — NOT reproduced in five later flat runs and three stamped; unattributed (7b's suite over the composed tree; the parked future's frames are not in a thread dump) — stated, not adjudicated |
| `sym_block_grant_tests` 38 / `sym_slot_transfer_tests` 45 / `sym_appender_tests` 34 / `fsck_tests` 22 / `fsck_c9_tests` 12 / `fsck_c10_tests` 18 / `docs_parity_tests` 5 / `derivation_sweep_tests` 59 / `env_knob_convention_tests` 22 / `decoder_property_tests` 59 / `dlm_membership_tests` 51 — flat AND stamped | **22 / 22 legs green** |
| `tests/run_sym_forest_suites.sh` (35 suites, flat → stamped) | **PASS** (21.1 min; no ratio NOTE — `sym_crash_matrix_tests` 36.8 / 21.1 s = 0.57) |
| `tests/run_sym_forest_suites.sh stamped` then `flat` (the other way) | **PASS / PASS** (9.8 + 11.4 min) |
| `corpse_sweep_tests` stamped ×4, `SQUEEZEFS_TEST_REQUIRE_MOUNT=1` | **4 / 4** runs (4/4 each, 165–168 s) |
| the live-FUSE trio under `SQUEEZEFS_TEST_REQUIRE_MOUNT=1`, stamped then flat | `posix_mount_semantics_tests` 3/3 + 3/3, `inline_raise_tests` 7/7 + 7/7, `corpse_sweep_tests` 4/4 + 4/4; no mount-class skip |
| `tests/run_nvmeof_fidelity.sh quick` (root, nvmet; the release binary of the rebased tree) | **PASS 43 / FAIL 0** (1m38s; `sym-manager-failover` successor wall 1,407 ms against the 45,011 ms bound) |
| `mw_fleet.sh create N=2 --symmetric --lease-ttl-ms=15000` (`SQZ_MWFLEET_OSS_GB=24`, tcp devsub, the release binary) + `run_mw_matrix.sh sym-crash --rounds=3` WITH the per-file-`fsync` oracle; teardown | **3 / 3 GREEN** — kill phases 6,846 / 844 / 3,267 ms, remount 2 / 1 / 1 s; the oracle: **1,146 / 149 / 460 files whose `fsync(2)` returned before the kill, every one present with its content on the successor**; per round fsck findings 0, C8 drift 0, fence refusals 0, tripwires 0, `self_recoveries` 2, manager `held`; the reader healthy after the matrix; teardown zero residue (rows `/run/squeezefs-mwfleet/rows/symcrash-1789580884`, torn down) |

Not run (by rule): `task check`, squeeze-test, anything on `dev` / `main`;
`.benchmarks/2026-09-12-sym-pr-run.md` untouched.

## 11. Review round 4 (2026-09-16) — Issues 29 and 30

Round 3's verdict attributed the one hang to **PR 7b (Issue 28 — `flip_dir_inner`
takes DLM guards in separate acquisitions on the same striped tables; deterministic
under `SQUEEZEFS_DLM_STRIPES=1`)**; it is fixed on dev by 7b's owner and nothing of
`dir_stripe.rs` is touched here. PR 10's two items:

| # | Class | Fix | Pin |
|---|---|---|---|
| 29 | bug | the rollback restores the slot's RAM TREE too: a failed recovery left the installed page root — published nowhere, unpublishable while tree 0 leases the slot to the dead appender — with its `root_floor` at the run's ring-0 head, so `unpublished_root_floors` clamped ring 0's checkpoint tail until a re-run SUCCEEDED; a recovery that never re-ran successfully (the record retired by the member's rejoin, a permanent failure) pinned the tail for ever — the ring fills, every commit parks (the wedge class). Now `RecoveryRollback` DISCARDS every cached node of the slot, dirty ones included (`NodeCache::discard_slot_nodes` — the failed replay's RAM fold of a window the dead ring still holds; nothing of this mount's own, nothing to flush, and a flush would have met the restored `foreign` bit at the structural gate), then puts the tree back at its pre-install `(root, floor)` (`KvTree::restore_root` — the pointer alone, no read, no pin: the grant-time root the mount held without traversing) or removes a guest the run adopted fresh (`SlotTrees::remove_guest`); the RAII is declared UNDER the SMO mutex so its drop runs under it (no flush pass mid-walk). The re-run installs from the durable state exactly as a first run does | `a_failed_recovery_with_no_re_run_leaves_no_floor_on_ring_0` (two-backend fixture): fail at step 5 and at step 7 (the flush done, the root moved) — after each `unpublished_root_floors()` is EMPTY (RED before: `{4: 155791}`), the tree reads its grant-time root, the page stays `Recovering`; `retire_death_record` (the rejoin), the poll recovers nothing; 40 creates in the manager's own slots then TWO cycles — ring 0's `reusable_upto` ≥ the storm's head, `journal_full_stalls` flat; a new record then recovers every acked file from the durable state; fsck clean |
| 30 | nit | `open_guest_trees` opens a root ahead of tree 0 through `KvTree::open_unpublished_slot_tree` (the ONE law's last inline copy gone); `tests/run_sym_forest_suites.sh` runs every suite under `timeout` with a DERIVED bound — `max(SQZ_SYM_HANG_FLOOR_S = 600 s, SQZ_SYM_HANG_FACTOR = 4 × the suite's flat wall)` (the flat leg, or a single leg, has no measured wall and takes the floor) — kills the whole process tree on expiry (GNU `timeout` signals its process group; verified: no orphaned test binary) and prints `=== HUNG: <leg> <suite> killed by the per-suite watchdog after N s (the last test line: …) ===`, failing the leg; proven to fire (`SQZ_SYM_HANG_FLOOR_S=3` on `sym_appender_tests` → HUNG at 3.0 s, exit 1) and inert on green suites (the matrix run below, 0 HUNG rows) | — |

The crash matrix is **30 contracts** (+1 ignored instrument).

**Round 4 verification (`CARGO_INCREMENTAL=0`, the dev laptop alone)**: fmt /
both clippy configs / rustdoc `-D warnings` — clean; `sym_crash_matrix_tests`
stamped ×3 + flat ×1 (30 contracts) — 4 / 4 green (23.4–23.9 s);
`sym_appender_tests` 34 / `docs_parity_tests` 5 / `derivation_sweep_tests` 59,
flat AND stamped — 6 / 6 legs green; `bash -n tests/run_sym_forest_suites.sh`
clean; the 35-suite matrix flat → stamped with the watchdog ARMED — **PASS**
in 20.8 min with **0 `HUNG` rows** and no ratio NOTE (inert on green suites);
the watchdog proven to FIRE beside it (`SQZ_SYM_HANG_FLOOR_S=3` on
`sym_appender_tests` → `HUNG … after 3 s`, the leg failed, no orphaned test
binary). Not run (by rule): `task check`, squeeze-test, anything on `dev` /
`main`; `.benchmarks/2026-09-12-sym-pr-run.md` untouched.

## 12. Review round 5 (2026-09-16) — Issue 31, the recoverer's death inside its flush

Round 4's finding, adjudicated BUILD: a permanent mount refusal on the death
path. **The shape, corrected by the pin**: a recoverer that dies AFTER step 6
completes leaves no exposed record (the flush loops until every dirty node is
covered, and the dead slot's floor — the manager holds no lease on it — never
clamps ring 0), so the window is the one INSIDE a cycle: between the flush
pass's SMO records and the cycle's covering ledger record. In the two-backend
shape the dead lessee's slot is no region of the recoverer's set, so its
compactions journal their interior records into ring 0; a death there left
them uncovered with tree 0 still `Leased { dead }`, and every later open
refused with `appender partition violated … appender 0's ring carries a record
for slot tree 4 it does not lease` — clearable by nothing.

| Piece | File | Why |
|---|---|---|
| `detect_appender_violations(rings, leases, granted, recovering)` — the manager's INTERIOR records for a slot in `recovering` are legal; nothing else moves | `kv/journal.rs` | the exemption itself; `recovering` = the slots tree 0 leases to an appender whose PAGE is `Recovering` — keyed on the page state of the slot's lessee, never on the record's writer. Pinned: a `Live` lessee's slot stays the `Lease` class, a content record for the slot stays it, a third appender's record in its ring stays it, the lessee's own interior record in ITS ring stays legal (`sym_appender_tests::the_recovering_exemption_admits_the_managers_structure_alone_and_only_for_the_named_slots`) |
| `RecoveringStructure { slots, stash }` / `RecoveringInterior` / `AppenderSet::recovering_structure` | `kv/appender.rs` | the forest replay WITHHOLDS the manager's interior records for those slots (they were journaled against the PAGE root the recoverer installed, not the tree-0 root the open holds — folding them there dirtied a foreign tree, which `drop_slot_nodes` then refused) and hands them to the appender open |
| the forest replay's directory + tree-0 read, the stash filter; the appender open's install of a foreign `Recovering` page's roots BEFORE `park_replayed_frees` (a root swap's retirement of the page root is among the replayed frees — dropped for a mounted root), an OWN `Recovering` page's stash applied at the open, a foreign page's kept on the set; the screen's `appender_current = None` for a lessee mid-recovery | `kv/backend.rs` | the open's half of the law |
| step 4 applies the stash onto the installed root (structural class, level DESC / seq ASC) before the dead window; `SlotRollback.structure` puts it back and discards what it dirtied (an `Untouched` tree discards for this alone); `recovering_lessees` in at step 2, out at step 8 | `kv/backend/recovery.rs` | the re-run's half |
| `SlotLeasePlane::recovering_lessees` | `kv/slot_lease.rs` | the SECOND face of the same law, found by the pin: the recoverer's frames on the dead slot are stamped `(0, g)` and the §5.8.2 screen's rule 4 (`g == g_current ∧ appender ≠ lessee`) read them FOREIGN at every reload while tree 0 named the dead lessee — the recovered leaves loaded TRUNCATED (the base bset itself screened) and the re-run's compaction refused the fold (`SMO fold-source log-view mismatch … disk walk ends at 61440, the live object's append cursor is at 4096`). **The suspension's window and class (round 7, Issue 33)**: rule 4 is inert on the slot from the page's `Recovering` write to the recovery's tree-0 step (the next grant moves `g`); what it admits at `g` is a frame from an appender that is neither the dead lessee nor `0` on a slot it never leased — a corrupted or hostile appender; rules 1–3 stay armed (a former lessee at `g − 1`, a frame past the recorded tail) and on PR the preempt fences the dead lessee's key regardless |
| `TEST_CHECKPOINT_HALT_BEFORE_LEDGER` | `kv/checkpoint.rs` | the kill inside a cycle, deterministic (the sibling of PR 8's `…_AFTER_LEDGER`) |

**The pin** `a_recoverer_dying_after_its_flush_leaves_a_mount_the_next_open_admits`
(two-backend fixture, `TwoBackendsWindow::CreatesUnderInterior(600)` — the
lessee's rounds continue until the slot tree has an INTERIOR root, so a leaf
compaction's parent flip is an interior record; a root-leaf compaction alone is
a root swap, which journals none): the recoverer's first step-6 cycle halts
before its ledger record with ≥ 1 SMO (the premise, asserted; a fixture whose
flush only appended — the dentry keys hash across leaves the lessee's own
maintenance left at random fills, so 1 in ≈ 6 fixtures — is rebuilt, bounded at
6, loud on the bound), the recoverer is dropped without a shutdown, the next
open ADMITS (`meta_kv_replay_lease_violations` flat), the stash is witnessed
(`test_recovering_structure_len ≥ 1`), the page's root is the tree's root at
the open, `mount_path_custody_gate` re-runs the recovery (`recovered 1`, the
stash consumed), every acked record resolves, tree 0 `Unleased`, the page
`Recovered`, `recovered:` written, a create under the directory lands, fsck
clean. **RED before** with the detector's exemption disabled: the reopen
refused with the exact `Lease` text above. The crash matrix is **31
contracts** (+1 ignored instrument).

**Stated, not built (PR 12's)**: a dead lessee's NODE that rejoins over its
own `Recovering` page (own residue) after a FOREIGN recoverer mutated its tree
and died mid-flush reads the tree from its page root without the recoverer's
uncovered flips (they sit in the recoverer's ring 0, which the rejoiner never
reads) — the predecessor images those flips retired are freed once the
recoverer's tail passes their parks. The `Recovering` page should carry the
recoverer's identity/term so a rejoin over a foreign recovery REFUSES (or
waits for the recovery to complete and takes a fresh region) — the
cross-node rejoin's protocol, Issue 25's obligation widened.

**Verification (`CARGO_INCREMENTAL=0`, the dev laptop)**: fmt / both clippy
configs / rustdoc `-D warnings` — clean; `sym_crash_matrix_tests` stamped ×3 +
flat ×1 — 4 / 4 green (31 contracts, 28–31 s); `sym_appender_tests` 35 /
`crash_contract_tests` 25 / `docs_parity_tests` 5, flat AND stamped — 6 / 6
legs green; the Issue-31 pin alone ×6 stamped — 6 / 6. Not run (by rule):
`task check`, squeeze-test, anything on `dev` / `main`;
`.benchmarks/2026-09-12-sym-pr-run.md` untouched; no rebase.

## 13. Review round 6 (2026-09-16) — the rebase onto PR 9 (custody by the slot holder)

`feat/sym-recovery` rebased from `6c80d70f` (PR 7b) onto dev `b7be8177`
(`git rebase --onto b7be8177 6c80d70f HEAD`; dev carries, after the old base,
the 7b flip lock-law fix `fe62cb0e` and PR 9 `3625c5cb`). Every conflict was
ADDITIVE and composed theirs (dev) then ours: `tests/run_sym_forest_suites.sh`
(`sym_custody_tests` then `sym_crash_matrix_tests` — **36 suites**; the
per-suite watchdog is ours and kept), `tests/derivation_sweep_tests.rs` (PR 9's
handover-bounds tie then ours), `docs/operations.md` (PR 9's section then
ours), `docs/design-symmetric-metadata.md` (dev's row 9 LANDED kept, our row
10 and the §5.3.4 row kept — at each of the five commits touching the table);
`AGENTS.md`, `src/fuse_client.rs` (PR 9's custody keys then ours),
`src/env_knobs.rs` (`SQZ_STRIPE_FLIP_LAW_SHAPE` + PR 9's seams, ours after) and
`loom-models/src/lib.rs` (`custody_revoke_core` then ours) auto-merged.

**The five seams PR 9's reviewer named**, each verified to compile AND mean
the same (pinned where the check found a gap):

| Seam | Verdict |
|---|---|
| (a) `leave_appender_regions` — `custody_blocked` folded into `uncovered_ids` BEFORE the one `uncovered` verdict; `recall_custody_at_leave` ahead of `release_leases_at_leave` | PR 10 does not touch the leave (`git diff b7be8177..HEAD -- kv/backend.rs` has no hunk in it); the `Recovered`-until-`alloc_lease:`-stops-naming law lives in the POLL (`release_recovered_regions`), the death-ledger arm in the S6 eviction. A custody-blocked region takes the uncovered posture through PR 9's own fold — page `Live`, roots, tail, ring, grant — and the page carries no "custody" word, so the next open cannot distinguish the two: the same identity recovers it as own residue (PR 2's arm, PR 9's `a_leave_whose_grant_survives_the_bound_keeps_the_region_live_and_the_next_open_recovers_it` — green on the rebased tree), a dead identity's is the ledger's driver (the headline). Neither RECALLS the surviving grant: a grant is the ISSUING owner's process RAM — the successor's owner (a fresh `WriteCustodyOwner` at an own-residue open; the recoverer's, which never issued it, at the driver) holds nothing on the slot's files, and the grant is retired at its WRITER by the S9 law — `CUSTODY_UNKNOWN_LEASE` at its next renewal against the successor, `T_self` when the holder is unreachable (PR 9's `a_dead_holders_t_self_fence_is_scoped_to_its_own_custody_and_driven_by_the_cadence`); the driver's step-1 preempt fences the dead HOLDER's registrant, not the writer's. Pinned on the driver's path by the (b) contract (`slot_custody_live == false` before, during and after; `handover_recalls_pending == 0`). **Stated, PR 12's**: the successor's S9 owner opens no grace window at the production arm (`open_grace` has sim callers only), so between its first fresh grant of a recovered/resumed slot's file and the old writer's next renewal (≤ one beat, ≤ `T_self`) two writers can believe they hold custody of one file — S9's authority-restart posture, not new here; the join ladder's re-arm is where the grace window for a recovered slot's files belongs. |
| (b) `release_slot_handover_locked` = a wrapper holding `HandoverCustodyMark` to the terminal outcome; the recovery's steps 7/7b | The driver never runs `release_slot_handover_locked` and arms no mark: its door is the lease TABLE — `Releasing { dead }` from step 4 to 7b — and BOTH grant paths consult the table before anything is granted: the served `CustodyGrant`'s `foreign_slot_holder` answers `NotHolder { dead }`, the local acquire's `slot_holder_home` → `step_home` resolves the DEAD holder (`Unreachable` once PR 9's holder fence forgot its dial slot; `Foreign` to a dead endpoint otherwise — refused at the dial, never local). So a grant during a recovery cannot land at the recoverer — the mark's job (deferring at the OLD holder) is moot because the old holder is dead. After 7b (`Unleased` on the manager) both consults answer "ours" and the local arbiter grants. **Pinned**: `a_custody_grant_of_a_slot_mid_recovery_is_the_dead_holders_never_the_recoverers` (two-backend fixture with two REGULAR FILES preset into the slot — `common::sym::seed_file_in_slot`; PR 9's owner + slot-custody arm installed on the recoverer; the recovery parked before tree 0): `foreign_slot_holder == Some(dead)`, `step_home == Unreachable { dead }`, `slot_holder_home == Unbound { dead }`, `SlotLockManager::acquire_lock` → `Refused { EAGAIN }` with `via_slot_holder` flat and `owner.held() == 0`, `handover_recalls_pending == 0`, tree 0 untouched; after the terminal outcome every consult answers local and the same acquire is GRANTED by the local arbiter, no mark ever armed. |
| (c) `foreign_slot_holder` / `token_record_mode` — a custody object is a REGULAR FILE | The recovery is kind-blind: `recovery.rs` inspects `KIND_INTERIOR` (interior records route by their journal key's slot) and `TREE_BLOCK_REFS` (the refs family's window) only — never an inode's `mode`; the replay applies `(kind, key)` records, the tails walk leaves, tree 0 is per slot. The (b) pin's objects are regular files of the slot (premise asserted: `S_IFREG`, `forest_slot_of_ino == slot`); the 7b seam pin covers a striped directory, its stripes and the markers. |
| (d) `dlm_slot.rs`'s arms consult under the op's inode + lease lock; `RecoveryRollback` restores the lease table + `foreign` + the door's waiters (+ the slot's RAM tree, round 4) | PR 9's state at the recoverer, enumerated: (1) the mid-handover mark `(uuid, slot)` — armed only by THIS process's handover / leave / deferred tick; the recovery neither arms nor clears it (a mark a deferred handover of ours armed against the now-dead holder expires at `2 × renew`); (2) `SlotHolderCache`'s lessee projection (`learn` / `forget`) — re-derived from the table at 7b (`refresh_holders`); (3) `SlotHolderCache`'s id → endpoint binding — per APPENDER (PR 12's), inert once no slot resolves to the dead id, re-bound at a rejoin; (4) the S9 owner's grant table — per INO at the ISSUING holder; the recoverer issued none on the dead slot's files (a slot it handed to the dead carried no grant across the move — PR 9's law); (5) the writer-side dial slots (`SlotCustodyArm.holders`) — per holder ENDPOINT, forgotten by PR 9's holder fence; (6) the FUSE layer's cached custody words — per ino, fenced at `T_self`. Nothing per slot beyond (1)–(2), so the rollback restores nothing of PR 9's, and (2) is consistent by construction (derived at the terminal outcome only). **FOUND AND FIXED (the (b) pin's second half, red before)**: the S4 lock plane (`dlm_slot::install_lease_foreign_slots` — the per-volume FOREIGN routing-slot set `publish_slot_owners` installs at the arm, at every grant and at every release) was NOT republished by the recovery, so after step 7b every `SlotLockManager::acquire_lock` on a recovered slot's file was refused `S9: lock object … homes on slot 3, which this node's lock authority does not own` until the volume's next grant or release happened to republish it — a write-custody refusal on every recovered file at the recoverer (the create path's 4a guards ride the process-local `DlmLockManager`, which is why the crash matrix's post-recovery creates never saw it). Step 7b now calls `publish_slot_owners(set, &plane)` after `refresh_holders` — the gate's structural belt (`install_foreign`) and the S4 table follow the table, the grant's and the release's own step. |
| (e) the flip fix's `stripe_at_mkdir`-after-guards vs nothing of ours | Compiles clean (`cargo check --all-targets --all-features`); `a_striped_directory_in_a_dead_lessees_slot_recovers_with_its_map_and_c17_clean` green stamped on the rebased tree. |

The crash matrix is **32 contracts** (+1 ignored instrument).

**Verification (`CARGO_INCREMENTAL=0`, the dev laptop; the rebased tree at the
fix)**: fmt (root / `fuzz/` / `loom-models/`), both clippy configs, rustdoc
`-D warnings`, `fuzz` check, `task check:loom`, `tests/run_loom.sh` (117 ok) —
all clean; `sym_crash_matrix_tests` stamped ×5 + flat ×2 — 7 / 7 green (32
contracts, 27–31 s); `sym_custody_tests` 27 / `sym_dir_stripe_tests` 29 /
`sym_block_grant_tests` 38 / `sym_slot_transfer_tests` 45 / `sym_appender_tests`
35 / `fsck_tests` 22 / `fsck_c9_tests` 12 / `docs_parity_tests` 5 /
`env_knob_convention_tests` 22 / `derivation_sweep_tests` 60 — flat AND stamped,
20 / 20 legs green; the matrix `tests/run_sym_forest_suites.sh` (36 suites, flat
→ stamped) PASS in 21.9 min, no ratio NOTE, no `HUNG`; `corpse_sweep_tests`
stamped ×4 under `SQUEEZEFS_TEST_REQUIRE_MOUNT=1` — 4 / 4; the live-FUSE trio
(`posix_mount_semantics_tests` 3, `inline_raise_tests` 7, `corpse_sweep_tests`
4) stamped AND flat under `REQUIRE_MOUNT=1` — 6 / 6; fidelity `quick` PASS=43
FAIL=0 (1m38s) and `full` PASS=115 FAIL=0 (2m44s; `pr-matrix` 11 / 11) as root
on the release binary; `sym-crash` ×3 on the tcp devsub (`mw_fleet.sh create
N=2 --symmetric`) — 3 / 3 GREEN, the acked-writes oracle 729 / 306 / 948
fsynced files all present, `self_recoveries=2`, manager `held`, tripwires 0,
`findings:0` every round. Not run (by rule): `task check`, squeeze-test,
anything on `dev` / `main`; `.benchmarks/2026-09-12-sym-pr-run.md` untouched.

## 14. Review round 7 (2026-09-16) — Issues 32–35

**Issue 32 (bug) — a live foreign lessee's page root ahead of tree 0 floored
ring 0 for the mount's life.** Round 2 seeded `published` with tree 0's root
and round 3 floored every root opened AHEAD of it; the `Leased` arm of the
forest replay took the same law for a slot a FOREIGN appender leases, whose
page root is ahead of tree 0's grant-time record in the wire venue's steady
state — and `publish_forest_roots` never publishes a leased slot by law, so
nothing could lift it. **Building the pin found the in-process masking, and
the masking was itself a hole**: the bring-up's `publish_forest_roots`
(before the plane arms — `plane` = `None`) listed the leased slot as pending,
skipped its record (`Leased => continue`) and then noted EVERY pending root
published — lifting the floor with nothing durable naming the root. That is
how every two-backend fixture opened B at all: with the hole closed alone and
the seeding unfixed, B's open over image 1 (root R1 ahead of tree 0's R0)
REFUSED — `bring-up journal residue did not cover within 64 barriered cycles
(head=155686, reusable_upto=150770)` — the wedge as a mount refusal. The same
hole lifted Issue 31's hold (a foreign `Recovering` page's floor) at the
open, so between the bring-up and the mount path's C15 re-run the dead
recoverer's flips were coverable — a narrow death window the hold existed to
close.

| Piece | File | Law |
|---|---|---|
| the forest replay's `Leased` arm decides `publication_ours` = the lessee's page is this node's OR `Recovering`; a live FOREIGN lessee's slot opens at its page root with `published = root`, no floor | `kv/backend.rs` (`open_forest_and_replay`) | the lessee's page is its publication; its records sit in ITS ring; a ring-0 floor protects nothing |
| `SlotTrees::new` takes each guest's PUBLISHED root (tree 0's where ours, the opened root where the lessee's) | `kv/forest.rs` | the seeding, stated |
| `KvMetaBackend::unpublished_root_floors` — the LEASE FILTER: a slot leased to a live foreign appender (not an in-process region, not mid-recovery) never floors, whatever the forest's map says | `kv/backend.rs` | the belt behind the seeding, armed plane only |
| `publish_forest_roots`: `written` ≠ `pending` — only roots whose record this entry writes are noted; an OWN region's `Leased` record without a table entry (the bring-up window) is REWRITTEN with the moved root (the page-budget overflow arm's exact shape: the lease's words verbatim, only the root moves — durable in tree 0, the record `leased_root_from_directory` falls back to); a FOREIGN lessee's is never rewritten and never noted; an entry with nothing to write returns early | `kv/backend.rs` | closes the skip-yet-note hole; an own region's root moved by the bring-up's own flush is named durably before the tail passes its records |
| `cover_bring_up_residue` covers DOWN TO the recovery hold (`recovery_hold_floor` — the lowest floor of a slot whose lessee is a foreign appender in `recovering_lessees`), never past it | `kv/backend.rs`, `kv/backend/recovery.rs` | Issue 31's hold made real across the open: the re-run's tree-0 step lifts it; the D1.b law holds for everything but the recovery's own window |
| `test_unpublished_root_floors` reads the checkpoint's filtered view | `kv/backend/recovery.rs` | the Issue-29 witness and the Issue-32 pin read what the tail reads |

**The pin** `a_live_foreign_lessees_page_root_ahead_of_tree_0_floors_nothing_at_the_managers_open`:
the two-backend fixture, the manager's clean leave, one publication deferred
across the reopen (`TEST_FOREST_PUBLISH_DEFER` — the shape that keeps a
floor standing), the manager reopens over X's `Live` page at R2 with tree 0
`Leased { 1, root: R1 }`: the slot at R2, tree 0 untouched, **no floor for
the slot**, a 200-create storm in the manager's own slots + the cadence
passes the storm's start within `COVER_CYCLES_MAX`, the lessee's tree
untouched; then X's death recovers the published-at-open tree whole, no
floor left. **RED before** (with the masking hole closed): the fixture's own
open refused at the bring-up. **The hold, witnessed**: the Issue-31 pin now
asserts the slot's floor STANDS after the reopen's bring-up (`reusable_upto ≤
hold`) and is lifted by the re-run — before this round the same assertion
would have failed (the hole lifted it).

**Issue 34 (suggestion, built) — the custody quarantine on the death path.**
`DeadMemberRecord` carries its RECORDER class (a 26th byte; the 17 B PR-8 and
25 B round-1..6 images decode as the plane's own, `early = false`; a class
byte outside 0/1 refuses — the proptest mirror covers the three lengths and
the poison): `record_death_with_key` (the S6 eviction, the wire) writes
`false`; `record_death` (PR 8's same-node takeover) and `appender_clear` write
`true`. At step 7b a recovery from an EARLY record calls
`data_grant::quarantine_slot_custody(uuid, slot, ts_ms + T_self)` for every
released slot (`T_self` from the installed custody authority's clocks, else
the shipped derivation); both grant paths consult
`custody_quarantine_remaining(ino)` after PR 9's mid-handover mark — the
local acquire (plain and ranged, `dlm_slot.rs`) answers `Refused { EAGAIN }`
naming the quarantine, the served `CustodyGrant` `CUSTODY_DEFERRED` with the
same reason — one relaxed load on every mount without a quarantine; gauges
`slot_custody_quarantine_refusals` / `slot_custody_quarantined`. The pin
`an_early_death_record_quarantines_the_recovered_slots_custody_for_t_self`:
two regular files preset into the slot, the lessee killed, `appender clear`
(the early record), the next mount with PR 9's owner (2.2 s `T_self`) + arm,
the gate recovers: the slot is ours (`foreign_slot_holder` = `None`) yet the
acquire is refused `EAGAIN` naming the quarantine inside `T_self` of the
record, then granted no earlier than `ts_ms + T_self`, the quarantine gone;
the plane-record control is the seam-(b) pin's tail (an immediate local grant
after a `record_death_with_key` recovery).

**Issue 33** — §5.8.2 gained rule 4 with its suspension window (page
`Recovering` → the re-run's tree-0 step, when the next grant moves `g`) and
the class it admits (a frame at `g` from an appender that is neither the dead
lessee nor `0` on a slot it never leased — corrupted or hostile; rules 1–3
armed; the PR preempt regardless); the same sentence in operations.md's stats
row and §12's table above. **Issue 35** — `SQZ_SYM_HANG_FACTOR` /
`SQZ_SYM_HANG_FLOOR_S` registered `Kind::Harness` with their operations.md
rows.

The crash matrix is **34 contracts** (+1 ignored instrument).

**Verification (`CARGO_INCREMENTAL=0`, the dev laptop)**: fmt (root / `fuzz/`
/ `loom-models/`), `fuzz` check, both clippy configs, rustdoc `-D warnings`,
the markdown link check — all clean; `sym_crash_matrix_tests` stamped ×3 +
flat ×1 — 4 / 4 green (34 contracts, 33 s); `sym_custody_tests` 27 /
`sym_slot_transfer_tests` 45 / `sym_appender_tests` 35 / `kv_leaf_merge_tests`
14 / `env_knob_convention_tests` 22 / `docs_parity_tests` 5 /
`derivation_sweep_tests` 60 / `decoder_property_tests` 61 / `sym_forest_tests`
31 / `sym_manager_tests` 29 — flat AND stamped, 20 / 20 legs green; before
the commits, with the Issue-32 fix alone: `sym_convert_tests` 38 /
`crash_contract_tests` 25 both layouts green. Not run (by rule): `task check`,
squeeze-test, anything on `dev` / `main`; `.benchmarks/2026-09-12-sym-pr-run.md`
untouched.

## 15. Review round 8 (2026-09-17) — Issue 36, the quarantine's bound and durability

Round 7's quarantine compared the RECORDER's wall clock (`ts_ms`) against the
manager's with the MEMBER's `T_self` as the window, and kept the word in RAM.
Now: the bound is the OWNER's — `T_owner + 2 × skew_max` past `ts_ms`
(`data_grant::custody_quarantine_bound_for`; `T_owner` is the instant past
which a holder may re-grant a lease it issued, `T_self = T_owner − 2·skew_max −
D_purge` the member's stricter self-fence — the wrong side for an owner-side
quarantine; the `2 × skew_max` absorbs the two wall clocks' disagreement; the
bound never outlives the record's retirement age `2 × T_owner`, since
`2·skew_max < T_owner` by the clocks' own admissibility — tie-tested in
`derivation_sweep_tests::sym_custody_quarantine_bound_derives_from_t_owner_and_skew_max`
over three clock sets + the shipped derivation). And it is DURABLE: the
recovery's step-7 tree-0 entry carries `custody_quarantine:{slot} → { version
‖ until_ms }` (a 9 B record under a new tree-0 prefix; `slot_state.rs`) beside
each released slot's `Unleased`; the mount path's C15 gate re-derives the RAM
word from those records before the set serves (`load_custody_quarantines` —
one range read per armed volume; expired records retired in one `Try` control
entry, best effort) — the codec fuzzed by `slot_state_record` + the proptest
mirror. Pins: `a_custody_quarantine_survives_a_manager_restart_inside_the_window`
(the first incarnation recovers, is refused, leaves cleanly; the process word
CLEARED — the restart; the second incarnation's gate recovers nothing and
re-derives ONE quarantine, the acquire refused naming it until the bound past
the ORIGINAL record, then granted; a third load retires the record) — **RED**
without the gate's re-derivation (`the quarantine is re-derived from the
durable record: left 0, right 1`); the Issue-34 pin re-targeted to the bound
and asserting the durable record's deadline + its retirement. The crash matrix
is **36 contracts** (+1 ignored instrument).

**Verification (`CARGO_INCREMENTAL=0`, the dev laptop)**: filled in below.
