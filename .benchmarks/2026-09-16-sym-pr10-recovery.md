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
  suites); the both-ways run and the ×10 stamped count are recorded in
  the summary (`/tmp/grok-justin/grok-exec-summary-dadee1dd-pr-10.md`).

## 6. Fleet legs (LOCAL, the tcp devsub)

`sudo tests/mw_fleet.sh create N=2 --symmetric --lease-ttl-ms=15000` then
`sudo tests/run_mw_matrix.sh sym-crash --rounds=10`: the s7-kill-matrix
body on the symmetric fleet with per-round successor asserts (own-residue
recovery ≥ 1, `manager_lease` `held` on every volume, `symmetric_meta` 1,
every symmetric must-stay-0 gauge 0, `appender_recoveries` 0 — one
appender, nothing foreign dies) + the online fsck with the C8 oracle
(C14/C15 riding it). The run's counts are in the summary; a FOREIGN
appender's recovery over the wire needs N daemons on one volume — PR 12's
venue.

## 7. Stats

Appended at the END of the symmetric block: `appender_recoveries`,
`appender_recovery_phase_ns` (exact-sum), `appender_recovery_bound_ms`
(per volume), `recovery_full_tail_scan_bytes`, `appender_recovery_preempts`,
`recovered_regions_released`, `appender_clear_runs`, `recovery_ledger_polls`,
`recovery_intents_rolled_forward`, `fsck_slot_custody_conflicts`
(must-stay-0), `fsck_unrecovered_appenders`, `fsck_repair_classC15`. PR 8's
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
