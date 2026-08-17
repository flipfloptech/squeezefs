# MW rung 10c — fleet-parallel maintenance (KD-MW-16)

Branch `feat/mw-fleet-jobs` (off dev 43719451). Design (the B4-pattern
pass, written BEFORE the code): `docs/design-mw-fleet-jobs.md`, cross-
linked from `docs/design-full-multi-writer.md` row 10c. Contracts:
`tests/mw_fleet_jobs_tests.rs` (6 pins). Rig legs:
`tests/run_mw_matrix.sh s10c-fsck-scale` / `s10c-kill-shard`.

The user ruling this implements (2026-08-15, verbatim): *"to avoid
things like fsck and defragmentation from taking too long those need to
be able to work in multi-writer parallel mode as a job across the
filesystem that all clients participate in."*

## 1. What landed

* **The fleet read-shard plane on the §5.1.6 wire** (`src/job_wire.rs`,
  `WIRE_SCHEMA` 3→4): `Enroll.caps` (worker capability mask,
  `CAP_FLEET_READ` — routing, never authentication),
  `ShardDescriptor.fleet = (k, n)`, the `ReadShardResult` proposal
  (fencing-checked exactly like a mutating submission) and
  `ShardAbandon` (the PROMPT form of the lease-expiry law), per-
  `(job, shard)` lease state, and the read-shard expiry SPLIT: fencing
  bump + re-lease notify ONLY — no destination quarantine (nothing was
  pre-allocated) and **no PR preempt** (a read worker DMAs nothing;
  preempting a live host's registrant key over a lost READ shard would
  fence a co-located co-writer's data plane).
* **The fleet fsck engine** (`fsck::run_fleet`): census partitioned
  `n = 1 + enrolled read workers` ways over the PINNED ino-residue law,
  worker proposals merged through the existing `merge_reports` union,
  then the coordinator FINALIZE runs the allocator classes (C2 tracked /
  C3 / C6, finding-#5 lane exemptions verbatim) + C8 over the
  fleet-merged full reference census with the existing
  verify-before-report ladder (`PartialCensus` grew the mapping
  identities, serde-defaulted; an oversize shard degrades loudly to the
  walk fallback). **Zero capacity IS the shipped local run** (pinned).
  Staging classes shard by LOCALITY (each member scans its own dirs
  FULL — strictly more C4/C5 coverage than the pre-fleet run, which
  never saw a member's staging at all).
* **Membership-roster workers** (`src/fleet_worker.rs`, wired in the
  mount): a reader/co-writer that JOINED the S6 plane spawns the fleet
  worker (`installed_member_session()` is the eligibility gate);
  authentication stays the `job:enroll` storage-trust HMAC (the access a
  member already holds IS the credential — neither law weakens);
  `worker_id` = the KD-MW-2 durable enrollment id. The manual
  `squeezefs job worker` verb became a REAL fleet participant
  (read-only probe opens per shard). `SQUEEZEFS_FLEET_JOBS` (registry
  entry, default on) disarms either half per mount.
* **The R5 composition**: a Red member refuses at admission and cancels
  mid-walk (`sqz-fleet-r5` watcher), then ABANDONS — never a partial
  proposal (a partial census would under-count). The coordinator
  re-leases promptly (abandon) or at the TTL (kill-9).
* **Gate 3**: the §5.7 mover quiescence probe gained the custody arm —
  an ino with a live S9 custody grant is NOT quiescent
  (`WriteCustodyOwner::ino_granted`, `job_mover_custody_defers`).
* **Map hygiene**: `retire_fleet_shards` drops a pass's fleet shard
  state at end; a zombie's late proposal still refuses stale through
  the unknown-shard arm (pinned).

## 2. Design adjudications (the B4 pass — full text in the design doc)

1. **Cross-writer lane reconcile (C2/C3/C6 over peer lanes) DEFERRED**:
   rung 10's own lane-blind free law + the lane-harvest handout ledger
   make a lane owner's exported tracked map non-authoritative at any
   instant — adjudicating peer-lane RAM state from an export is the
   category error finding #5's exemption exists to prevent. **C8 stays
   the multi-writer block-plane oracle** (and the fleet pass runs it).
2. **Mover/repair shard distribution DEFERRED**: a mover's publish is a
   metadata act only the authority may commit — the same
   `wire_executable` gap VL4 recorded; one gap, one place.
3. **Slot-aligned meta assignment** degenerates to the residue partition
   while the ownership plane has ONE owner (every shipped fleet);
   upgrades with S8 multi-owner.

## 3. Cargo pins (tests/mw_fleet_jobs_tests.rs — red-first, all green)

| Pin | Law |
|---|---|
| `zero_capacity_fleet_run_is_the_local_run_verbatim` | fleet(None) ≡ fleet(0 workers) ≡ `run()`; zero fleet metrics move |
| `fleet_census_covers_exactly_once_with_partition_accounting` | gate 1: merged `inodes_scanned` == unsharded baseline at N=3; findings 0; dispatched == completed == 2, relocal 0 |
| `worker_shard_census_feeds_the_coordinator_allocator_classes` | gate 1 teeth: 3 seeded in-capacity referenced-untracked phantoms (one per residue) all convict through the fleet pass — worker census reaches the finalize's C2 tracked arm |
| `lost_read_shard_re_leases_and_the_late_proposal_refuses_stale` | gate 2: TTL expiry → relocal; late proposal refused (`job_remote_refused_stale`); census exact; **PR preempts 0, quarantine 0** (the read-shard split) |
| `red_member_abandons_promptly_and_the_shard_re_leases` | R5: abandon (not TTL) re-leases — completes in <10 s under a 30 s TTL; `refuse_on_red` Red-refuses, Green/Yellow admit |
| `custody_grant_defers_the_mover_probe` | gate 3: granted ino not quiescent (counted); ungranted ino unaffected; uninstall lifts |

Touched sibling suites green serially (job_wire, fsck, fsck_c9/c10,
fsck_repair, defrag, interaction, job_fabric, job_worker_panic,
dlm_multi_writer, durable_block_refs, env_knob_convention, skip_ledger).

## 4. The scaling rows (rig leg `s10c-fsck-scale`)

<!-- FILLED BY THE RUN -->

## 5. Kill-9 mid-shard (rig leg `s10c-kill-shard`)

<!-- FILLED BY THE RUN -->

## 6. New observability (stats inode; registered in the JSON)

Coordinator: `job_fleet_shards_dispatched` / `job_fleet_shards_completed`
(the engagement pair — a fleet row is INVALID unless completed accounts
for dispatched) / `job_fleet_shards_relocal` (the re-lease ledger's
local arm). Member: `job_fleet_worker_shards`,
`job_fleet_worker_red_aborts`. Authority mover:
`job_mover_custody_defers`. The existing
`job_remote_{lease_expiries,reassignments,refused_stale,shards,submissions}`
fire for fleet shards under the same laws. Knob: `SQUEEZEFS_FLEET_JOBS`
(bool, default on — ENG-10 registry entry).

## 7. Residuals

* Deferrals 1–3 of §2 (recorded in the design doc with reasons).
* The scale rows are **measured-simulated** (one box: co-located
  members share the device and the CPUs; the fleet win is real
  parallelism across daemon processes, but a multi-host row would also
  buy independent page/CPU budgets). The 15k extrapolation stays
  arithmetic-on-measured-constants per `docs/rc-manifest.md`.
* The fleet worker reconnect cadence rides `HEARTBEAT_INTERVAL / 2`
  (the wire's own grain); a coordinator restart therefore re-enrolls
  members within ~5 s — visible as a `job_remote_workers` dip in the
  scale rows' remount ceremony, never inside a timed window.
* `DefragMeta`/`DefragFold` fleet distribution rides deferral 2 (each
  member's fold targets its own parked extents, but a co-writer's fold
  publishes metadata — the S8 shipping surface).
