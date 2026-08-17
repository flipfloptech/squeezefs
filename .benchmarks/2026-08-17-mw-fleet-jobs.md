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

## 4. The scaling rows (rig leg `s10c-fsck-scale`) — GATE MET

Venue: the tcp devsub fleet (`tests/mw_fleet.sh create 4 --membership`,
nvmet-tcp on 127.0.0.1, 2 mds null_blk + 2 oss zram, one box), binary
`9d629c49dedb` release (default features), **quiet** (no foreign cargo,
loadavg ≈ 3.8 / 32 cpus). Instrument: `time`d `squeezefs fsck <mnt0>
--scrub --throttle 10` (the CLI's 500 ms status poll is inside every
row equally); corpus = 96 × 32 MiB urandom files (3 GiB), written+
fsync'd once; EVERY member remounted per run (cold rows — the R1b
second-touch law keeps a single scrub pass from warming the disk cache,
and the remount clears the RAM ghosts; run-to-run spread came out
±3 ms). **The KD-3 throttle is the stretch instrument**: unthrottled,
this box's zram scrubs the whole corpus inside the CLI poll quantum;
the duty cycle applies PER MEMBER, so linearity — and the ratio the
gate is defined on — is preserved.

| Width | runs (ms) | median (ms) | speedup vs N=1 |
|---|---|---|---|
| N=1 | 12529, 12528, 12525 | **12528** | 1.00× |
| N=2 | 6518, 6516, 6516 | **6516** | 1.92× |
| N=4 | 5013, 5014, 5016 | **5014** | **2.50×** |

* **Gate: MET** — 2.50× ≥ 2.4 (= 0.6×-linear at N=4). Evidence tier:
  **measured-simulated** (one box; co-located daemons share the device
  and CPUs — the parallelism across daemon PROCESSES is real, the
  device is one).
* **Coverage law held exactly**: the coordinator's
  `fsck_inodes_scanned` delta was **98 at every width and every run**
  (96 files + the corpus dir + root) — the residue partition covers the
  census exactly once, and the coordinator's published counters account
  for the whole fleet pass. `scrub_bytes_scanned` delta =
  3,221,225,472 B (the corpus, byte-exact) at every width.
* **Engagement exact on every run**: `job_fleet_shards_dispatched` ==
  `job_fleet_shards_completed` == N−1, `job_fleet_shards_relocal` == 0,
  each reader's `job_fleet_worker_shards` == 1, findings: 0 at every N.
* Attribution of the N=2→4 sub-linearity (1.30× step): the per-member
  scrub share falls to ~0.75 GiB (~4 s throttled) while the row keeps
  ~1 s of width-independent terms — the coordinator-only C8
  walk+ledger scan, the census/dentry/C1 walks each member repeats, the
  job submit/poll ceremony — so the fixed floor shows exactly where
  design-mw-fleet-jobs §4 predicts (the finalize is the stated Amdahl
  term). N=1→2 is 1.92× (near-perfect halving of the dominant term).

## 5. Kill-9 mid-shard (rig leg `s10c-kill-shard`) — GREEN

Same fleet, corpus 2 GiB, `--throttle 5` (the kill window's stretch).
A no-kill baseline learned the fleet census total (66 inodes, 7.0–7.5 s
wall); the kill run then killed **reader 1 with SIGKILL while both
worker shards were IN FLIGHT** (dispatched delta == 2, completed 0):

* the victim's lease expired (`job_remote_lease_expiries` +1);
* the residue **re-leased by re-dispatch to the surviving reader**
  (dispatched +3 total, relocal 0, completed 2) — the fencing-checked
  re-lease across workers, the charter's exact shape;
* the pass completed **findings: 0** with the census total IDENTICAL
  to the baseline (66) — **zero double-coverage** (a stale proposal
  cannot merge: fencing; the in-process pin also proves the late-
  proposal refusal, which a SIGKILLed daemon cannot exercise live);
* **`job_remote_pr_preempts` delta 0 and destination-quarantine delta
  0** — the design-§5 read-shard expiry split held live (a read worker
  DMAs nothing; its host's registrant key is never preempted);
* the victim remounted at leg end (stale-FUSE endpoint reaped),
  posture `reader` — zero residue; fleet teardown afterwards asserted
  zero residue.

First attempt of this leg (counted-restart discipline: it aborted the
count) found the **report-persist bug** (§7 → fixed + repro-ported):
the acceptance pass above is a from-zero green of the fixed binary.

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

## 7. Rig-found bug (fixed + repro-ported)

The kill leg's FIRST run failed at the baseline: the fleet shard
reports' mapping identities made the 512-block corpus's `job:{id}:report`
**66,499 B against the 64 KiB xattr cap** — the persist failed, so the
CLI read "no report". Fix: the partial census is MERGE-INPUT data and
never persists (`to_store.partial = None` in `run_fsck_job`);
repro-ported as
`fsck_job_report_persists_without_the_partial_census` (the full
fabric-job path). The same run sized the venue (zram scrubs the corpus
inside the CLI poll quantum), which is why the legs ride the KD-3
throttle as their stretch instrument.

## 8. Residuals

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
