# Fleet-parallel maintenance jobs (KD-MW-16)

MW ladder rung **10c** (`docs/design-full-multi-writer.md` row 10c), the
user ruling of 2026-08-15 verbatim:

> *"to avoid things like fsck and defragmentation from taking too long those
> need to be able to work in multi-writer parallel mode as a job across the
> filesystem that all clients participate in."*

Landed in `src/fsck.rs` (`run_fleet` — the fan-out/merge/finalize engine),
`src/job_wire.rs` (fleet read shards on the §5.1.6 wire: `WIRE_SCHEMA` 4,
worker capability flags, `ReadShardResult`/`ShardAbandon`, per-`(job, shard)`
lease state), `src/jobs.rs` (the `FleetDispatch` seam between the fabric and
the wire host), `src/fleet_worker.rs` (the mount-side member worker + its
`MountFleetSeam`), and `src/fuse_client.rs` (the §5.7 mover custody-defer
probe arm). Contracts: `tests/mw_fleet_jobs_tests.rs`. Rig legs:
`tests/run_mw_matrix.sh s10c-fsck-scale` / `s10c-kill-shard`. Evidence:
`.benchmarks/2026-08-17-mw-fleet-jobs.md`.

This is the **B4-pattern short design pass** the rung-table row demands:
the adjudications below were made BEFORE the code, and two of them are
**deferrals with reasons** (§7) — the honest outcome of composing KD-MW-16
with laws rungs 9–10 themselves landed.

---

## 1. What already existed, and what this rung adds

The job fabric (VL2/VL2b) already had every safety law this rung needs:
durable `job:`/shard records on ino 1 (KD-2), the coordinator = the D0
writer-claim holder, remote workers over the §5.1.6 wire with storage-trust
HMAC enrollment, shard leases (TTL + heartbeat), **fencing-checked result
proposals**, and the KD-3 duty-cycle throttle. The offline fsck already had
zero-coordination ino-residue sharding (`--shards k/N`) whose union law
(`merge_reports`) is exactly-once by construction: every shard walks the
whole dentry tree but marks only its residue, and every inode is judged by
exactly one shard.

What did NOT exist:

1. The wire dispatched **whole jobs only**, and only `JobType::Noop`
   (`wire_executable`) — "shipping scrub sub-shards over the wire's
   read-shard seam is a stated follow-up". This rung is that follow-up.
2. Workers were **manual** (`squeezefs job worker <sqmeta-uri>`), never
   members. The S6 membership plane (rung 7) supplies the roster; nothing
   consumed it for maintenance.
3. Under an engaged allocation partition, fsck C2/C6 **decline foreign
   lanes** (rung-10 finding #5, `fsck_foreign_lane_exempted`) — correct per
   mount, but nothing restored whole-set coverage.
4. The §5.7 mover quiescence probe predates S9 custody: a defrag mover on
   the authority could pick a block whose owning ino a co-writer holds
   **write custody** over, and fight it.

## 2. Worker identity and enrollment: membership names, storage trust proves

**Law: eligibility is BY MEMBERSHIP; authentication stays storage-trust.**
A mounted member (reader or co-writer) that has joined the S6 plane spawns a
fleet worker at mount (`src/fleet_worker.rs`): it reads the `job:enroll`
secret through its own meta backend — the access it already holds IS the
credential (ruling D2's law verbatim: possession of volume access is cluster
membership) — discovers the coordinator endpoint from the mount
registrations, and enrolls over the SAME challenge/HMAC handshake every
worker uses. Its `worker_id` is the mount's durable KD-MW-2 enrollment id
(`node_{16 hex}.m{8 hex}`) so the coordinator's shard records name roster
identities.

Neither law weakens: the membership plane never mints a job credential (a
member that cannot read `job:enroll` cannot work), and the job wire never
trusts a self-declared roster claim (the HMAC is the admission, exactly as
before). The **manual `job worker` verb survives unchanged** for non-mount
storage-trust workers — same wire, same proof, same shard vocabulary.

**Capability classing is routing, never security.** The `Enroll` frame
carries a capability mask (`CAP_FLEET_READ` — this rung's only shipped
capability); the coordinator assigns fleet read shards only to sessions
advertising it. A lying capability buys a shard the worker cannot execute,
which it must `ShardAbandon` (or lose by TTL) — re-leased, never trusted.
Mutating capability classes (repair/mover shards under S9 custody) are §7
deferrals; **repair stays coordinator-only** in this rung, so the read mask
is the whole shipped surface.

**Opt-out**: `SQUEEZEFS_FLEET_JOBS=0` (ENG-10 registry entry) disarms both
halves — the member worker at mount and the coordinator's fan-out.

## 3. Shard alignment

**Meta/census classes (C1, C4/C5, C9/C10, C7 scrub): ino-residue shards.**
The fleet fsck partitions the census into `n = 1 + enrolled read workers`
residue shards (`ino % n`), keeping shard 0 on the coordinator (online mode
— its suspects/settle machinery stays armed) and shipping shards `1..n` as
fleet read shards. Each member runs the engine's EXISTING sharded walk
(offline posture on its own coherent view) and proposes its shard report;
the coordinator merges via the EXISTING `merge_reports` union law.

*Slot alignment, adjudicated:* KD-MW-16 says meta shards align with S8 slot
ownership — "owners verify their own slots, locality for free". On every
shipped fleet the ownership plane has exactly ONE owner (`meta_ship.armed`
is false everywhere; the authority owns every slot), so slot-aligned
assignment **degenerates to any partition of the ino space**, and the
ino-residue partition is the one whose exactly-once union law is already
pinned. When S8 arms with >1 owner, the assignment upgrades to
slots-follow-owners; that refinement is deferred WITH the multi-authority
rung that makes it meaningful (§7.3), not silently skipped.

**Staging classes (C4/C5) shard by LOCALITY, not residue.** Staging dirs
are per-mount; the coordinator's fsck never could see a member's staging.
Fleet shards therefore scan the executing member's OWN staging dirs
**unfiltered** (`FsckOptions.staging_full` — set on every fleet shard and on
the coordinator's local shard 0): each member covers its own custody
records completely, dirs are disjoint across members, so the union is
exactly-once AND strictly more coverage than the pre-fleet run.

**Block classes: see §7.1** — the lane-aligned allocator reconcile is a
deferral with a reason this rung's own predecessors created. What DOES ship:
the coordinator's finalize pass (§4) runs the allocator classes (C2 tracked
/ C3 / C6) against the **fleet-merged full reference census** with its own
allocator ground truth, foreign-lane exemptions exactly as rung-10 finding
#5 shipped them, and C8 (the durable ledger — the stated multi-writer
block-plane oracle) still runs on the coordinator over the whole set.

## 4. The fleet fsck engine (`fsck::run_fleet`)

1. **Capacity**: `FleetDispatch::read_capacity()` (idle, non-expired,
   `CAP_FLEET_READ` sessions). Zero capacity ⇒ **delegate to `run()`
   verbatim** — a single-writer mount's fsck is not merely equivalent to
   the shipped path but literally it (pinned).
2. **Arm**: the coordinator arms the allocator scan latch for the whole
   fleet window (the C2/C3 epoch side map spans every shard walk); the
   local shard-0 run is told the latch is held (`assume_latched`) so its
   release cannot drain the map mid-fleet.
3. **Dispatch** shards `1..n`; run shard 0 locally (online). Each shard
   descriptor carries `(k, n)`, the job's throttle (KD-3 per worker), and
   the lease law unchanged.
4. **Collect**: a worker's `ReadShardResult` is fencing-checked exactly
   like a mutating proposal (stale ⇒ `job_remote_refused_stale`, refused).
   A lost shard (TTL expiry, `ShardAbandon`, malformed/mismatched payload)
   is **re-leased**: re-dispatched to another idle capable worker, else run
   locally (`job_fleet_shards_relocal`). Every residue lands exactly once.
5. **Merge**: `merge_reports` over all shard reports. `PartialCensus` now
   also carries the shard's mapping list (`#[serde(default)]` — old
   reports stay decodable) so the finalize has referencer identities; an
   oversize mapping list degrades loudly (`mappings_complete = false`) and
   the coordinator falls back to walking its own census for the allocator
   classes (a stated Amdahl term, never silent).
6. **Finalize** (coordinator): allocator classes C2/C3/C6 against the
   merged full refs + its own tracked map (finding-#5 lane exemptions
   verbatim), C8 when engaged, then the EXISTING verify-before-report
   ladder (`recheck_suspects`: settle, epoch bump, registry escalation,
   lease re-checks). The reference re-verification arm is what makes
   member-view staleness safe: a suspect born from a member's slightly
   older coherent view re-reads CURRENT meta before any verdict.
7. **Publish**: the coordinator publishes the worker-merged + finalize
   counters to its stats inode (its own shard published itself inside
   `run()`), so the coordinator's `fsck_*` deltas account for the WHOLE
   fleet pass exactly once; each member's stats carry its own share — the
   gate-1 partition accounting instrument.

**Repair composes unchanged**: `--repair` consumes the MERGED verified
findings on the coordinator, under per-object leases, exactly once — a
re-leased shard cannot double-repair because a stale proposal never merges
(fencing) and findings dedupe by identity at merge.

## 5. Failure and pressure laws

* **Kill-9 mid-shard** (gate 2): the lease TTL expires, fencing bumps, the
  shard re-leases (another worker or local), and the dead holder's late
  proposal — should it resurrect — refuses stale. Read shards have **no
  destinations**, so expiry takes the fencing/notify arm only: no
  quarantine (nothing was pre-allocated) and **no PR preempt** (a read
  worker DMAs nothing; preempting a live co-writer's registrant key over a
  lost READ shard would fence its data plane — the split is deliberate and
  pinned).
* **R5 Red on a worker** (the KD-3/R5 composition): the member's shard
  execution watches its own budget level; Red aborts the shard loudly and
  sends `ShardAbandon` (`job_fleet_worker_red_aborts`) — the prompt form of
  the lease-expiry law — and the coordinator re-leases. A Red COORDINATOR
  keeps the VL9 pin (e) posture: jobs pause loudly, `job resume` is the
  operator's.
* **Throttle**: KD-3 duty cycle applies per worker via the descriptor's
  `throttle_pct`, exactly as remote Noop shards already did; live retune
  reaches new assignments (a shard in flight keeps its granted duty).

## 6. Movers never fight custody (gate 3)

The §5.7 quiescence probe (`fuse_client::mover_quiesce_probe`) gains a
custody arm ahead of its existing layers: an ino with a **live S9 custody
grant** (`data_grant::custody_owner()` table) is NOT quiescent — the mover
defers and re-plans (`job_mover_custody_defers`), the same shape as the
device-overlay layer it sits beside. This is the safety half of KD-MW-16's
mover story; the distribution half is §7.2.

## 7. Deferred, with reasons

1. **Lane-aligned cross-writer allocator reconcile (C2/C3/C6 over peer
   lanes).** Rung 10's OWN laws make a lane owner's exported tracked map
   non-authoritative: **frees are lane-blind** (any writer frees a peer's
   block with no message — design-mw-data-alloc-partition §2) and the
   authority **harvests lane free lists into handout ledgers**
   (`alloc_lane_grant`), so per-lane block state is legitimately SPLIT
   across writers at any instant. A shard built from one writer's export
   would adjudicate state that a peer may have changed without telling it —
   the exact category error finding #5's exemption exists to prevent. The
   cross-writer block-plane oracle therefore REMAINS **C8** (the durable
   reference ledger, one record per reference, walk-vs-ledger), which the
   fleet pass still runs; per-lane RAM-state reconcile waits for durable
   per-writer allocator state (the same dependency §6.2 item 8 already
   names). `fsck_foreign_lane_exempted` stays the honest gauge.
2. **Fleet-distributed mover/repair shards under S9 custody.** A mover
   shard's publish step is a metadata act (`merge_block_mappings` per
   referencing ino) that only the authority may commit — the SAME gap
   `JobType::wire_executable` has recorded since VL4, now one rung wider:
   a co-writer COULD ship the publish (S8), but the wire's
   verify-then-publish shard shape does not carry the per-ino merge, and
   faking it would violate the pre-allocated-unpublished-destination law.
   One gap, recorded in one place (`wire_executable`), unchanged.
3. **Slot-aligned assignment with >1 metadata owner** — meaningful only
   when S8 arms multi-owner; the assignment seam is in place (§3).
4. **DefragMeta/DefragFold fleet distribution** — each mount's fold targets
   its OWN parked extents, but a co-writer's fold publishes metadata (the
   S8 shipping surface); rides deferral 2.

## 8. Observability

Coordinator: `job_fleet_shards_dispatched` / `job_fleet_shards_completed`
(the engagement pair — a fleet fsck row is INVALID unless completed accounts
for the dispatched shards) / `job_fleet_shards_relocal` (lost/refused/
no-capacity residues run locally — the re-lease ledger's closure:
`dispatched ≡ completed + relocal + refused-stale-pending`), plus the
existing `job_remote_{lease_expiries,reassignments,refused_stale}` firing
for fleet shards under the same laws. Member: `job_fleet_worker_shards`
(shards executed as a member) / `job_fleet_worker_red_aborts` (the R5
composition's gauge). Authority mover: `job_mover_custody_defers`. Every
per-member `fsck_*` family keeps its meaning on the member's own stats
inode — the partition accounting instrument (gate 1).

## 9. Gates (the rung-table row, restated)

Red-first cargo pins (`tests/mw_fleet_jobs_tests.rs`): (1) an N-member
detect pass covers the census exactly once — merged `inodes_scanned` equals
the unsharded baseline, findings 0, a seeded foreign-residue violation is
found through a WORKER's shard; (2) a worker that goes silent mid-shard
re-leases with its late proposal refused and zero double-count; (3) the
custody probe defers a granted ino (`job_mover_custody_defers`); plus the
zero-capacity identity pin and the R5-Red abandon pin. Rig:
`s10c-fsck-scale` (N=1/2/4 wall-clock, ≥0.6×-linear at N=4 on the tcp
devsub, findings 0 at every N, evidence-tier labeled) and `s10c-kill-shard`
(kill-9 mid-shard, zero double-coverage against the fleet's own baseline).
