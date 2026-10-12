# Porting spec — the job fabric, the volume lifecycle, defrag (rung 8)

> **Checksum amendment (2026-10-11, decision 0003 §M; 0002 row 480).** Every `xxh3` / `xxh3-64` /
> `xxh3_64` below names the hash the reference used and this port used until format v5. The port's
> ONE hash is now **sqzhash128** (`src/core/sqzhash.zig`, the pinned definition): every 8-byte checksum
> and identity field is its LOW 64 bits under the fixed public format key (`checksum.hash64`,
> `hash64FieldZeroed`, `stamp`/`verify`, the streaming `checksum.Hasher`); the seeded name hashes
> (`hash54`/`hash56`) are `hash64Keyed` under the per-mount name key (`key.NameKey`: the volume's
> `hash_seed` zero-extended, expanded once at mount). Field names on the wire read `csum`. Byte
> layouts, offsets and widths below are unchanged; only the function behind the 8 bytes is.

Scope: charter §8 "Job fabric" + "Volume lifecycle" + the `defrag` axes; §6.1 packing compaction
(the `--pack` arm); §5.3 D4 leaf merge via the defrag arm; §5.4 (`add-meta`/`remove-meta` already
ported); §6.2 memory Red. Reference (`R/` = `~/Source/squeezefs`): `docs/design-volume-lifecycle.md`
(`D:`), `docs/design-mw-fleet-jobs.md` (`F:`), `src/jobs.rs` (`J:`), `src/job_wire.rs` (`W:`),
`src/config_ops.rs` (`C:`), `src/defrag.rs` (`DF:`), `src/routing.rs` (`RT:`), `src/fuse_client.rs`
(`FC:`), `src/lib.rs` (`L:`). Port (`Z/` = `~/Source/squeezefs-zig/src`). Every claim carries a
`file:line`. Charter text is the requirement; the reference is evidence (decision 0002).

Conventions in this file: "the law" = a charter sentence restated; "quirk" = a reference choice the
charter does not require (§6 lists the better design and the 0002 row to add).

---

## 0. What the port already has (the reuse ledger)

The fabric lands on top of these; nothing here is re-implemented:

| Piece | Port home | What rung 8 reuses |
|---|---|---|
| The storage-trust ladder, `job:enroll` record `JobEnrollV1` (48 B, minted at FORMAT, never re-minted) | `Z/wire/auth.zig:1-45, 72-115` | enrollment of remote workers verbatim — the job wire is a verb BLOCK on the one listener (§1.8) |
| ONE listener, `SQUEEZEFS_MW_BIND`, cap `clamp(raw_cpus × 16, 64, nofile/8)`, accept backoff 5→1000 ms | `Z/wire/listener.zig:12, 57-77`, `Z/core/knobs.zig:284-285` | the job verbs ride it; no second listener, no `job_endpoint` field |
| Frame classes `handshake` 8 KiB / `control` 1 MiB; the 16 MiB bulk class "is the job fabric's (rung 8) and lands with it" | `Z/wire/frame.zig:56-80` | add `Class.bulk` |
| Verb blocks `wire/membership/xv/token/manager` with explicit u16 numbers | `Z/wire/verbs.zig:40-66, 73-156` | add `Block.job` = `0x06xx` |
| Tree 0 control records: binary keys `kind 8 ‖ tag ‖ id BE`, `extern struct` values `version u16 ‖ flags u16`, one fixed length per kind, `KEY_MAX_LEN = 16` | `Z/meta/sym/control_records.zig:1-30, 42-83` | the `job:` records are two new tags (§1.2) |
| `set_config` on tree 0 of volume 0: `DataVolume{vol_tag, state, device_uuid, uri, capacity_blocks}`, `State{active, disabled, draining, retired}`, pure `addDataVolume`/`setState` mutations, `transitionAllowed`, `lifecycle_bit_needed`, order-independent discovery | `Z/data/set_config.zig:1-40, 60-70, 594-703` | the lifecycle verbs are callers of these pure mutations (§2.3–§2.6) |
| `VOLUME_LIFECYCLE` incompat bit 3, `stamped_later` | `Z/meta/superblock.zig:140-159` | stamped bit-before-record by every verb (§2.2) |
| Router: `VolumeId.mint(random)` = `vol-{16 hex}`, `VolumeState.canTransition`, `placementEligible = healthy ∧ (recordless ∨ active)`, health math `penalty = clamp(1000 × (fill − mean), 0, 300)`, `PlacementTable` ArcSwap pick | `Z/data/router.zig:7-29, 85-131, 162-171, 202-360` | `Draining` weight 0 is already a consequence of `placementEligible`; the mover's fill-destination pick is a new query over `volumesLive()` (§2.5.4) |
| Block allocator: `allocateBlock`, `allocateBlockBelow` (the defrag mover's mint), `allocateBlockAtOrAbove`, `pinBlockValidated → PinVerdict{pinned, pinned_unstable, refused}`, `unpin`, `incrementRefcount`, `refcount`, `abandonUnpublished`, `quarantineBlock(idx, dead_epoch)`, `releaseQuarantine`, `InflightAllocGuard`, `fsckBeginScan` | `Z/data/block_alloc.zig:487, 826-887, 1002-1046, 1186, 1471-1544, 1600-1610` | the mover's pin, picks, destination undo and the wire's expired-lease quarantine |
| Dead epochs + `DrainProof{preempt_landed, proven_dead}` + `BlockQuarantine.admit/release` | `Z/data/sym_planes/custody.zig:186-232` | the expired-lease cohort (§1.6) |
| Durable refs: `refcount = population of (vol_tag, block_idx)`, `stageRefs` rides the layout tx, `RefsSeam` | `Z/data/block_refs.zig:1-40, 259-298` | the republish tx carries `+dst −src` per referencer (§2.5.3) |
| The terminal-free ladder `beginFree → tier purge → elide/enqueue → finishFree`, `reclaim_free.seam().free(io, mapping)` | `Z/data/reclaim.zig:10-18`, `Z/data/plane.zig:906-913` | the displaced source's free |
| `plane.ownBlock` = placement + mint + transform + DMA + `DmaProof`, `abandonOwnBlock` on a failed publish; `LayoutLocks` (3.7) `lockLayout`; `readMappingImage`; `layoutRecord` | `Z/data/plane.zig:328-345, 841, 868-906, 1017, 1435`, `Z/data/layout.zig:1198-1245` | the mover's destination IS an own-block-shaped unpublished mint |
| Packing: `packMaxSlot = CHUNK/2`, `isPackVictim(live, max_slot)`, `Packer.sealBlock` (`SealKind.drain` "the mover found the open pack on its victim block"), `census` | `Z/data/pack.zig:7-14, 128-139, 584-593, 760-763` | `--pack` compaction (§3.5) |
| Memory budget: `Component{gauge, floor, weight, shed}`, Red sheds ∝ weight | `Z/mem/budget.zig:5-9, 38-48, 389-429` | `job_copy_buffers` sheddable component (§1.7) |
| Reserved-xattr screen is an ALLOWLIST (`user.*`/`security.*`/`trusted.*`), `fuse_reserved_xattr_refusals` | `Z/fuse/handler/kv_fs.zig:234-238, 698-736` | `job:` is invisible by construction — nothing to add |
| IPC admin lane frames `AdminHello/AdminOk/AdminReq{verb ≤ 64, arg ≤ 60 KiB}/AdminReply` — declared, NO consumer yet | `Z/ipc/wire.zig:291-360` | rung 8 writes the dispatcher (§1.10) |
| `volume {list, add-meta, remove-meta, repair-set, migrate-meta-slot}`; migrate on a live mount "rides rung 8's admin channel" | `Z/cli/volume.zig:1-16, 62-66, 210`, `Z/meta/routing/offline.zig:218-597` | add `add-data/remove-data/undrain/enable-symmetric`; route migrate through the fabric |
| The OFFLINE set (`withSet`: writer door, set config, optional plane, teardown in mount order) | `Z/cli/offline.zig:1-11, 102` | the offline coordinator body |
| PR client: WE rtype 1 `acquire`, `preempt`, `preemptAndAbort`, `release`; `RTYPE_WERO = 3` "rung 6's manager" | `Z/meta/guard/pr.zig:5-13, 110, 262-292`, `Z/meta/guard/nvme_cmd.zig:125-128` | the coordinator's WERO on DATA namespaces (§1.6) |
| kvmap sweep = a plane fiber, cursor-resumed (`drainKvmapSweep`, `spawnSweep`) | `Z/data/plane.zig:1902-1960` | NOT a job type here (quirk §6 #8) |

---

## 1. THE JOB FABRIC

### 1.1 Shape and authority

The law (charter §8): every maintenance op is a job TYPE on one fabric; the coordinator is VOLUME 0's
MANAGER; workers execute data-plane shards; results are PROPOSALS the coordinator verifies and
commits. Reference: coordinator = the D0 writer-claim holder (`D:169-175`, KD-1 `D:636`); under the
symmetric plane the predicate is `symmetric_coordinator_refusal` — volume 0's `manager_lease.word()
== "held"` else refuse naming the manager (`R/src/meta_backend/kv/alloc_lease.rs:1289-1303`,
`J:1330-1355`). Minting a job record IS the coordinator-class act, so `submit` is where a second
coordinator refuses (`J:1505-1510`); a non-manager still SERVES shards (participation, not a second
coordinator, `J:1350-1354`).

Two worker populations, one protocol (`D:173`): a LOCAL pool of `clamp(cpus/4, 2, 8)` tasks
(`fabric_workers_default`, `J:1203-1205`: the L4 service posture's slope, floored so a small box runs a
coordinator pair, ceilinged so a big one does not spawn a worker farm for a duty-cycled plane; `cpus`
is the fleet-share-divided root — the raw `available_parallelism()` read was the pinned-first-toucher
class, decision 0002 row 22) and REMOTE workers over the wire. `workers == 0` is the remote-only
posture (tests, `J:1478-1480`).

The mount-time ADMISSION HOLD (`J:1366-1373, 1404-1415, 1493-1502`): a fabric started held adopts
and re-queues its durable jobs but CLAIMS none until the mount opens admission after the allocation
lease arms over a quiescent allocator (an adopted mover allocates and frees the moment a worker claims
it). Offline verbs and tests start open.

Port: `Z/jobs/fabric.zig` (portable core: records, control blocks, claim, state machine, throttle,
checkpoint cadence; every wait on `io: std.Io`), `Z/jobs/coordinator.zig` (Linux: the manager
predicate = `sym/manager.zig`'s lease word, the mount hold, the worker fibers on the `sqz-meta`
lanes — `exec.Lane{.class = .meta}`; thread names `sqz-job<N>` for the local pool are NOT needed:
workers are fibers, CPU attributes to the meta class).

### 1.2 The durable `job:` records — byte-exact

Reference: JSON xattrs on ino 1, `job:{uuid}` → `JobRecord{schema 1, job_id, job_type, state,
throttle_pct, created_by "host:pid", created_ts, tasks_done, tasks_total, error: Option<String>}`
(`J:362-376`, `D:179-186`), `job:{uuid}:shard:{k}` JSON (`W:2721-2743`), `job:{uuid}:progress`
(designed `D:183`, never written — the record itself carries `tasks_done`). Cadence: checkpoint every
256 tasks or 5 s, whichever first (`J:56-57`, `D:190`: ≈ 1 GiB of moved data per checkpoint at 4 MiB
blocks — bounded re-work on crash). The record is ADVISORY; correctness is plan regeneration
(`J:370-372`).

Port: two control-record tags on tree 0 of VOLUME 0 beside `set_config` and `job:enroll`
(`Z/meta/sym/control_records.zig:42-78`; the precedent for "a JSON xattr on ino 1 becomes a canonical
control record" is decision 0002 rows 121–124). Keys `kind 8 ‖ tag ‖ id BE` — one prefix walk lists
every job in id order; `KEY_MAX_LEN` 16 holds both (`control_records.zig:80-83`):

```
Tag.job       = 13   id = job_id u64 BE                   (idLen 8)
Tag.job_shard = 14   id = job_id u64 BE ‖ shard u32 BE    (idLen 12)
```

`job_id` = a random u64 from `Io.randomSecure` (the `VolumeId.mint` discipline, `Z/data/router.zig:91`;
0 is never a job id). Printed `job-{16 hex}`.

```zig
pub const JobType = enum(u8) { noop = 1, evacuate_volume = 2, rebalance = 3, migrate_meta_slot = 4,
    fsck = 5, defrag_data = 6, defrag_pack = 7, defrag_meta = 8, defrag_fold = 9 };
pub const JobState = enum(u8) { queued = 1, running = 2, paused = 3, paused_capacity = 4,
    cancelled = 5, completed = 6, failed = 7 };   // terminal = cancelled | completed | failed

pub const JobRecordV1 = extern struct {
    version_le: [2]u8,          // 1
    flags_le: [2]u8,            // bit0 PAUSE_INTENT (operator pause survives a crash)
                                // bit1 CAPACITY_PAUSE (the §5.2 self-pause), bit2 MOVER_CLASS
    job_type: u8,               // JobType
    state: u8,                  // JobState
    throttle_pct: u8,           // 0 = unthrottled, 1..=100
    error_class: u8,            // 0 none; a TYPED failure class (never a string — 0002 row 9/10 law)
    job_id_le: [8]u8,
    created_realtime_s_le: [8]u8,
    created_node_token_le: [8]u8,   // StableIdentity (control_records.zig:124-130), not "host:pid"
    created_mount_slot_le: [4]u8,
    created_pid_le: [4]u8,
    tasks_done_le: [8]u8,
    tasks_total_le: [8]u8,
    arg0_le: [8]u8,             // evacuate/defrag_data/defrag_pack: vol_tag (0 = every eligible volume)
                                // migrate_meta_slot: slot u16 ‖ target_volume u16 ‖ 0; fsck: option bits
                                // noop: tasks
    arg1_le: [8]u8,             // noop: task_ms; fsck: quarantine-dir hash; else 0
    updated_realtime_s_le: [8]u8,
    reserved: [8]u8,            // checked zero on decode
    xxh3_le: [8]u8,
};
comptime { assert(@sizeOf(JobRecordV1) == 96); }
```

Decode is total: wrong length / version / checksum / unknown enum byte / reserved ≠ 0 ⇒ `Corrupt`,
counted and SKIPPED by the adopting scan with an error log naming the key (the reference logs and
skips undecodable records, `J:1700-1708`); an unknown `job_type` byte is `Corrupt` (the reference's
`serde` would refuse the whole record too).

```zig
pub const ShardState = enum(u8) { assigned = 1, reclaimed = 2, completed = 3 };
pub const DestTupleWire = extern struct { vol_tag_le: [8]u8, block_idx_le: [8]u8 };   // 16 B
pub const JobShardV1Head = extern struct {
    version_le: [2]u8, flags_le: [2]u8,
    state: u8, reserved0: [3]u8,
    job_id_le: [8]u8, shard_le: [4]u8, n_dest_le: [4]u8,     // n_dest ≤ 64
    holder_node_token_le: [8]u8, holder_mount_slot_le: [4]u8, lease_ttl_ms_le: [4]u8,
    shard_fencing_le: [8]u8,
    updated_realtime_s_le: [8]u8,
};  // 56 B; then n_dest × DestTupleWire; then xxh3_le [8]u8 over everything before
comptime { assert(@sizeOf(JobShardV1Head) == 56); }
```

`n_dest ≤ 64` because the shard's destination window IS the per-worker in-flight window (§1.5);
max record 56 + 1024 + 8 = 1088 B — far under the control-tree value cap. The count is checked against
the byte length before any tuple is read.

Durable-then-visible (`J:1805-1822, 2657-2662`): every terminal transition persists the record
BEFORE the live state flips, so a waiter woken by the terminal notify finds the terminal record on
disk. Terminal states never regress: the first terminal state wins under the state lock
(`J:1151-1166`).

### 1.3 The coordinator's state machine

```
                 submit (coordinator-class act; durable Put first)
                   │
   Queued ◄────────┴──────────────┐ requeue (remote lease expiry J:1975-1982; resume J:1595-1604)
     │ claim_next (J:1888-1940)   │
   Running ──────┬── park (paused intent) ──► Paused | PausedCapacity   (J:1832-1854)
                 ├── cancel ──────────────► Cancelled (terminal)       (J:1607-1618)
                 ├── census empty / pass done ► Completed (terminal)
                 └── error ───────────────► Failed (terminal, error_class) (J:2339-2348)
```

`claim_next` (`J:1888-1940`): admission held ⇒ none; `for_wire` restricts to wire-executable types;
a candidate must be `Queued`, not paused, and — the MOVER-SCOPE law (VL9 pin (a), `D:756`,
`J:259-288, 309-325`) — its scope must not intersect a CLAIMED mover's: `Volume(v)` conflicts with the
same volume, `WholeSet` with every mover. One mover-class job per volume at a time; the later one stays
Queued LOUD (`job_serialized_waits` counts the episode once, `J:1917-1927`). `claimed` is a CAS word
covering the whole worker-held window, released by an RAII guard on every exit (`J:1138-1146,
2008-2026`: a leaked claim wedged a volume scope forever — RES-7). KD-6 idempotence is what makes
queueing safe (a queued mover re-plans from whatever the earlier one left).

Adoption at start (`J:1440-1476`): every non-terminal durable record is adopted; Running/Queued
re-queue, Paused/PausedCapacity stay paused (operator intent survives the crash). Noop's `tasks_done`
is the advisory cursor; movers re-census.

The paused-park ceremony (`J:1824-1854`, the 2026-08-09 lost-resume strand): durable checkpoint
`Paused`, then `settle_park` under the state lock re-checks the pause intent — a `resume()` that landed
inside the checkpoint window wins (`Queued`, re-asserted durably, pool woken) instead of being
overwritten by the stale `Paused` flip (which left `paused = false` with a `Paused` state —
unclaimable forever). `JOB_PARK_ENTRIES` is the test's only honest observable of "inside the park
window" (`J:82-100`). Port: a `TEST_JOB_PARK_DELAY_MS` seam is a `SQUEEZEFS_TEST_JOB_PARK_DELAY_MS`
registry row (class `test_seam`); the park window is a `sync.Notify` deadline wait, never a sleep.

Panic containment (`J:2016-2036`, RES-7) is a Rust artifact: Zig has no unwinding (0002 row 9) — a
worker's job body returns an error union; `Failed` + `job_worker_panics` is retired, the typed
`error_class` replaces the string.

### 1.4 Job types and their plans (idempotent regeneration)

| Type | Plan source (regenerated EVERY pass from durable state) | Terminal rule | Scope |
|---|---|---|---|
| `noop{tasks, task_ms}` | the record's `tasks_done` cursor (advisory) | `done ≥ total` (`J:2655-2663`) | none |
| `evacuate_volume{vol_tag}` | the CENSUS over the victim: every live inode's layout entries whose mapping names the victim, grouped by distinct base block (`census_for`, `J:862-1027`), refs sorted `(ino, block_idx)` ascending (`J:1016-1020`) | census empty AND `victim used_blocks == 0` ⇒ retire (`J:2889-2912`); else re-plan after 200 ms (`REPLAN_BACKOFF`, `J:138, 3076-3081`) | `Volume(v)` |
| `rebalance` | `plan_rebalance` legs (§2.4), census over the sources clipped to the legs' block budgets (`bound_rebalance_census`, `J:4213-4238`) | ONE bounded pass then Completed — deferred/superseded tasks are NOT retried (`J:3025-3038`) | `WholeSet` |
| `migrate_meta_slot{slot, target}` | the online engine (`Z/meta/routing/migrate.zig`, already ported: bulk copy, tee, gate, target-first flip); idempotent across every crash window (`J:165-174`) | engine returns | none |
| `fsck{...}` | rung 8's fsck spec (separate) | report persisted as `job_shard`-class record `job:{id}:report` (`J:2091-2095`) | none |
| `defrag_data{vol_tag?}` | `plan_defrag` (§3.4): D2 locality tasks then D1 tail-compaction tasks | first no-progress pass, or `DEFRAG_MAX_PASSES = 16` (`J:144, 3049-3074`) | `Volume(v)` / `WholeSet` |
| `defrag_pack{vol_tag?}` | `plan_pack_compaction` (§3.5) | same | same |
| `defrag_meta` | per meta volume: `dead_bset_census` candidates + the §4.6a merge sweep lap (`J:2357-2483`) | one lap per volume | none |
| `defrag_fold` | the mount's fold targets (`FoldHook.targets`, `J:555-560`) | every target kicked once | none |

`wire_executable` (`J:290-306`): only `noop` ships whole jobs over the wire — a mover's publish is a
per-ino meta act only the coordinator commits; fleet READ shards (fsck residues) are the second
wire-shape (`F:85-119`). The port keeps exactly this division (§1.8).

Movers never fight custody (`F:183-190`): the quiescence probe refuses an ino with a LIVE custody
grant (`job_mover_custody_defers`), a live device-overlay record (`overlay_mover_skips`), a resident
staging ring entry for a staged tenant (`pack_mover_resident_defers`), a RAM active buffer, a staged
`active_block:` or spilled extent record (`FC:10611-10650`). Port: `plane.probe()`'s op-guard
`Probe` (`Z/data/plane.zig:2408`) grows one `moverQuiescent(ino, block)` arm over the same tables.

### 1.5 The ONE throttle (KD-3) and the copy window

`job_throttle_sleep(elapsed, pct)` (`J:4442-4454`): `pct == 0 ∨ pct ≥ 100 ⇒ none`; else
`sleep = elapsed × (100 − pct) / pct`, `none` below 1 ms. Task-active time ≈ `pct` of wall time.
Served in 50 ms SLICES re-reading the LIVE throttle and the control flags per slice (`duty_park`,
`J:1864-1881`): a live rethrottle / pause / cancel reaches a parked worker within one slice (at 1 % a
2 s task priced a 198 s un-interruptible park); the TOTAL park for a fixed pct is unchanged, so the
G-VL-7 ±10 % duty gate holds. `throttle(id, pct)` stores the atomic and checkpoints (`J:1621-1626`);
a remote shard in flight keeps its granted duty, the retune reaches new assignments (`F:179-181`).
The duty cycle is per WORKER: an unthrottled drain/rebalance overlaps `MOVER_PIPELINE_WIDTH = 4`
independent block moves (`J:102-109, 2914-2947`: each move pays read → write → verify-read → publish
serially and the serial loop measured exactly the ½-of-raw-copy floor); throttled passes and defrag
(order-dependent D1/D2 floors) stay width 1.

Port: `jobs/throttle.zig` — `dutySleepNs(elapsed_ns, pct) ?u64` as integer arithmetic (`elapsed ×
(100 − pct) / pct`, `null` under 1 ms) with the tie test restating the formula at 25/50/75 (`sleep ∈
{3t, t, t/3}`); `dutyPark(io, ctl, elapsed)` = `sync.Notify` deadline waits in 50 ms slices.

The copy window: ≤ 64 in-flight blocks per worker = 256 MiB at 4 MiB blocks, ≤ 2 GiB at the
8-worker cap (`D:217-218`); charged to the budget as the SHEDDABLE component `job_copy_buffers`
sized from the ACTUAL worker count (`D:218`, `CopyCharge` RAII `J:1083-1106` also feeds
`evacuate_inflight_bytes`). The same 64 is the preflight's `transient` window (§2.4).

### 1.6 Shards, leases, fencing, proposals, quarantine, WERO

Reference wire (`W:1-56`): per shard `{holder, lease_expiry (TTL 30 s), shard_fencing}`
(`LEASE_TTL`, `HEARTBEAT_INTERVAL = 10 s`, `W:181-183`); heartbeats renew (`renew_lease`,
`W:2371-2384`); the sweeper ticks `max(TTL/4, 25 ms)` in ≤ 250 ms slices (`W:2388-2425`); expiry
(`expire_shard`, `W:2432-2574`) = fencing `+1` → `job_remote_lease_expiries` + `job_remote_
reassignments` → the expired lease's destinations enter the do-not-publish QUARANTINE under a fresh
dead epoch (`W:2488-2513`: the live-zombie half of the law — a paused worker resuming DMA lands only on
offsets nothing will reference or reuse) → the holder session is marked expired-but-open so a late
submit is REFUSED, not dropped (`W:1140-1143, 2515-2525`) → PR-preempt the host's registration under
the standing WERO (`W:2530-2568`; a landed preempt IS the drain proof that releases the cohort; on a
detection-grade substrate the cohort stays quarantined — the documented deferred-reclaim class) →
requeue for any population. A departed session's held shards expire NOW (`W:2006-2024`).

`assign` (`W:2298-2365`): reuse the shard state (bumped fencing) or start at fencing 0; EVERY
assignment allocates FRESH destinations (`seam.allocate`), and an allocator that returns a quarantined
tuple is an invariant breach refused loud (`W:2323-2335`). The descriptor carries `{job_id, shard,
shard_fencing, job_type, source_keys, destinations, block_len, throttle_pct, lease_ttl_ms, fleet,
inode_plane}` (`W:237-268`).

Proposals (`handle_submit`, `W:2579-2678`): unknown shard ⇒ refused stale; `shard_fencing ≠ current ∨
done` ⇒ refused stale (`job_remote_refused_stale`); then VERIFY-READ the destinations against the
submitted `{dest, len, xxh3}` checksums BEFORE any publish (`verify_destinations`, `W:2682-2716`) —
count mismatch, a missing checksum, a length or xxh3 mismatch refuse; a failed verification is treated
as an expired lease (fencing bump + quarantine, `W:2644-2651`). Verification STRENGTH keys on the
channel (`W:1346-1362, 2702-2704`): 100 % on plaintext (authenticated by the session MAC but not
confidential — sampling trades reads for trust in what crossed the wire); the configured permille only
under CA-pinned mTLS (`verify_sampling_admissible`). A worker may: read anywhere on shared storage,
write ONLY the pre-allocated unpublished tuples, propose. A worker may NOT: allocate, publish, free,
touch metadata — the wire carries no meta-write verb (`W:9-10`, KD-15 `D:651`).

The worker side (`W:3146-3505`): a local lease clock starts a full TTL at assignment and every
`HeartbeatAck` extends it (`W:3258-3262`); DMA proceeds in `REVALIDATE_BATCH = 16` destinations per
re-validation (`W:210, 3352-3390`); `revalidate_lease` (`W:3536-3567`): expired ⇒ abort the shard
(no submission); past half-TTL ⇒ one heartbeat round-trip bounded by `TTL/4` before the next batch
(rung 1 of the ladder — exposure ≤ one batch). Threads `sqz-jw-rd` (reads) and `sqz-jw-hb`
(heartbeats, 100 ms slices for the stop latch); teardown nudges the socket (`shutdown(Both)`) and
joins (`W:3514-3530`).

WERO (rung 2, `W:2039-2102`): at FIRST remote enrollment the coordinator acquires Write Exclusive –
Registrants Only (rtype 3) on every configured DATA namespace — `pr` mode iff ALL are PR-capable, else
the deferred-reclaim class, loud, no bypass knob; released at LAST departure (zero residue). The hold
is the process's ONE data-plane reservation (an S7-armed mount shares it). Worker hosts register their
OWN keys and present `pr_key` at enrollment (`W:313-317`); the coordinator can only preempt. Side
effect (`D:235`): an unregistered third host is write-blocked on the data namespaces for the job's
duration. Meta WERO (rung 6's manager) and data WERO are different namespaces — no interaction.

Port: `Z/jobs/wire.zig` (Linux) — the shard table `(job_id, shard) → ShardState{ctl, fencing: Atomic
(u64), holder: ?{session, identity, expiry_ns}, destinations: bounded [64]DestTuple, done, fleet}`,
the sweeper as a `sync` deadline fiber, quarantine through `custody.BlockQuarantine.admit(idx, epoch)`
with `DeadEpochSequence.mint` and `release(epoch, DrainProof.preemptLanded(n))` (`Z/data/sym_planes/
custody.zig:186-232`); WERO through `guard/pr.zig` with `rtype = RTYPE_WERO` on the data devices'
`nvme_dev` fds. Sessions ride `wire/session.zig`; the `expired` bit is a session flag.

### 1.7 Pause / resume / cancel / Memory Red

- `pause` (`J:1584-1591`): set the intent, flip `Paused` unless terminal, checkpoint. Workers stop
  AFTER the in-flight task (the control flags are read per task and per duty slice).
- `resume` (`J:1595-1604`): clear the intent; `Paused | PausedCapacity → Queued`; checkpoint; wake.
- `cancel` (`J:1607-1618`): terminal, durable; also clears the intent so a paused job terminates.
- Memory Red (`J:1746-1769`, VL9 pin (e) `D:760`, charter §6.2 "jobs pause loudly"): the `job_copy_
  buffers` component's shed hook pauses every RUNNING job (`job_paused_mem_pressure`, a `warn` line
  naming gauge > target), durable through the workers' park checkpoints, and is deliberately NOT
  self-resuming — `job resume` is the operator's. Fires only when real copy buffers exist (gauge >
  target); a shed at/under target is a no-op. On a REMOTE member Red aborts the shard loudly and sends
  `ShardAbandon` (`F:173-178`, `job_fleet_worker_red_aborts`) — the prompt form of lease expiry.
- `PausedCapacity` (`J:327-331, 2851-2864`): the drain's checkpoint re-verification found the slack
  consumed; `resume` after freeing space is the §5.2 recovery.

### 1.8 Remote workers: enrollment, discovery, the verb block, `rotate-enroll`

Enrollment is the port's existing ladder (`Z/wire/auth.zig:16-28`): `Challenge` first (server nonce,
30 s freshness, single use), `Prove{peer_id, server_nonce, peer_nonce, mac = HMAC-SHA256(secret,
peer_id ‖ server_nonce ‖ peer_nonce ‖ "hello")}`, verify order schema → bounds → constant-time MAC →
consume nonce, HKDF session key, per-frame MAC. The reference's `enroll_hmac` IS `cluster_wire::
proof_mac` (`W:413-430`). What the reference's `Enroll` adds (`W:304-321`): `pr_key: Option<u64>` (the
worker HOST's registration on the data namespaces) and `caps: u32` (`CAP_FLEET_READ = 1 << 0`,
`W:173`: routing, never security — a lying capability buys a shard the worker can only abandon,
`F:73-80`). Port: the `Prove` frame is unchanged; `pr_key` and `caps` travel in a `JobEnroll` verb
sent FIRST after admission on the job block (a peer that never sends it is a cluster peer, not a
worker). Eligibility by MEMBERSHIP, authentication by storage trust (`F:55-71`): a mounted member
reads `job:enroll` through its own backend (`sym/joined.zig:369`), its `worker_id` is its stable
identity.

Discovery: the reference extends the mount-registration heartbeat with `job_endpoint` and workers
dial it (`D:227`, `W:781-793`). Port: the manager's published `Tag.endpoint` record on tree 0
(`control_records.zig:57-61`) IS the coordinator's endpoint — the job verbs ride THE one listener
(`SQUEEZEFS_MW_BIND`), so there is no second port, no second field, no second discovery (quirk §6 #2).

The job verb block (`Z/wire/verbs.zig` `Block.job = 0x06xx`, schema `JOB_SCHEMA = 1` — ours, not the
reference's 4; frames are canonical `extern struct`s, not bincode):

```
job_enroll        0x0600  {pr_key u64 (0 = none), caps u32}            → job_enroll_ok {heartbeat_ms, lease_ttl_ms}
shard_assign      0x0601  ShardDescriptor (coordinator → worker)
heartbeat         0x0602  {}                                            → heartbeat_ack {lease_ttl_ms}
result_submit     0x0603  {job_id, shard, shard_fencing, n, [BlockChecksum]} → result_ack | result_refused {reason class}
read_shard_result 0x0604  {job_id, shard, shard_fencing, payload ≤ bulk cap} (fleet read shards)
shard_abandon     0x0605  {job_id, shard, shard_fencing, reason class}
```

`shard_assign`/`result_submit`/`read_shard_result` are the BULK class (16 MiB: a 64-checksum shard is
~1.6 KiB, a fleet shard report can be MiBs; `Z/wire/frame.zig:69-71`); the rest are control class.
Every word is SCREENED (bounded codec = bounded execution): `n ≤ 64`, every `DestTuple` must be in
the shard's own destination set, `shard_fencing` compared before anything is read off the device.

Transport trust: the port's `wire/` already classes channels `plaintext | mutual_tls` (rung 8 adds
TLS without a wire change, `auth.zig:36-40`); the verify-permille knob honoured only under mTLS, 1000
pinned otherwise (`W:1356-1362`); a CA cert without its key REFUSES the listener (`W:954-961`; "a
cert alone cannot make an authenticated channel").

`job rotate-enroll <sqmeta-uri>` (`C:611-635`): OFFLINE on a quiesced set — the live-client gate on
EVERY volume, then the `job:enroll` record REMOVED on the config home volume under the writer door;
the next coordinator mints a fresh secret and every member re-enrolls at its next arm. Returns whether
a record existed. WHY offline-only: the secret is minted once per set and reused by every coordinator
— a per-mount re-mint broke every member that outlives a coordinator's incarnation (`W:1286-1298`:
`mac invalid` for the member's life after a manager failover). Port: the port mints at FORMAT
(`auth.zig:7-8`), so the "absent ⇒ mint" arm at coordinator start exists only for a set whose record
was rotated away: ABSENT ⇒ mint; PRESENT-but-undecodable ⇒ refuse loud; I/O error ⇒ propagate
(`W:1299-1305` — never conflated).

### 1.9 Crash-resume

KD-6 (`D:645`): jobs are idempotent by PLAN REGENERATION — the adopted record names the type and its
argument; the pass re-censuses from current durable state; the progress record is advisory. The
pre-allocated-unpublished-destination law in BOTH halves (KD-15 `D:651`): (a) a worker writes only
coordinator-pre-allocated UNPUBLISHED offsets, publication is coordinator-side behind fencing; (b) a
reassigned shard ALWAYS gets fresh destinations — an expired lease's tuples are quarantined. A kill-9
mid-copy leaks only unreferenced destination blocks within the current mount's allocator; the next
mount's refs rebuild (`seedFromDurableRefs`, `Z/data/block_alloc.zig:946`) never accounts them
(`D:318`). The offline coordinator (§5.8 duality `D:476-478`) is the same fabric in a short-lived
writer-door process hosting the same endpoint.

### 1.9a Fleet READ shards (the second wire shape, KD-MW-16)

`F:85-156`: a fleet fsck partitions the census into `n = 1 + enrolled read-capable workers` ino-residue
shards (`ino % n`); shard 0 stays on the coordinator (online — its suspect/settle machinery armed);
shards `1..n` ship as READ shards. Zero capacity ⇒ `run()` verbatim (a single-writer mount's fsck is
literally the shipped path — pinned, `F:123-126`). Each shard descriptor carries `fleet = (k, n)`,
`inode_plane` (KD-PV-16: the owner's volumes only), the throttle; the worker runs the engine's EXISTING
sharded walk on the blocking lane while heartbeats renew underneath and proposes `read_shard_result
{payload}` (`W:3264-3345`). The coordinator fencing-checks it exactly like a mutating proposal; a LOST
shard (TTL, `shard_abandon`, malformed payload) is RE-LEASED — another idle capable session, else run
locally (`job_fleet_shards_relocal`); every residue lands exactly once (`F:133-138`). Expiry of a read
shard takes the fencing/notify arm ONLY: no destinations to quarantine, NO PR preempt — a read worker
DMAs nothing, and preempting a live co-writer's registrant key over a lost READ shard would fence its
data plane (`W:2446-2477`, `F:164-172`). The collect loop's PROGRESS deadline derives from the lease
TTL (`FleetDispatch.shard_lease_ttl`, `J:1290-1297`: a worker whose heartbeats keep beating while its
shard never proposes hung the fleet fsck 31 min). `retire_fleet_shards(job)` drops the per-`(job,
shard)` state at pass end (`J:1283-1289`: without it every pass grew the map by `n − 1` forever); a
zombie's LATE proposal after retirement is the unknown-shard refusal. Shard numbers: census residues
`1..n`, inode-plane shards from `INODE_PLANE_SHARD_BASE = 1 << 20` (`J:1299-1306`) — one number space,
so the shard map, lease law, fencing identity and durable records are the same ones. Port: `jobs/
wire.zig` exposes `FleetDispatch` as a vtable the fsck spec's `runFleet` consumes; the member worker is
spawned at mount when `SQUEEZEFS_FLEET_JOBS` is on and the mount has joined the membership plane
(`F:55-66`; `Z/meta/sym/membership.zig`).

### 1.10 The verb table

| Verb | Live mount (admin lane) | `sqmeta://` (offline) | Reference |
|---|---|---|---|
| `job list TARGET` | live map, then durable records | probe read of the `Tag.job` range | `J:1680-1712`, `R/main.rs:1501-1507` |
| `job status TARGET ID` | live ctl, else record | record | `J:1550-1569` |
| `job pause/resume/cancel TARGET ID` | §1.7 | refused: live mount only | `R/main.rs:1509-1514` |
| `job throttle TARGET ID PCT` | §1.5 live retune | refused | `R/main.rs:1516-1520` |
| `job worker <sqmeta-uri>` | — | dial the manager's endpoint, enroll, serve shards until the coordinator goes away | `R/main.rs:1522-1529` |
| `job rotate-enroll <sqmeta-uri>` | refused under a live mount | §1.8 | `C:611-635` |

The admin lane: `Z/ipc/wire.zig:291-360` frames; the host admits ADMIN hellos from uid 0 or the mount
owner only (`D:206-208`, `ipc_bind_refused_flags`), peercred is THE boundary; the dispatcher is a
`(verb string ≤ 64, arg ≤ 60 KiB) → reply` table in `Z/ipc/host/admin.zig` serving `job *`, `volume
add-data/remove-data/undrain/list`, `defrag *`, `fsck`, `migrate-meta-slot`. Replies are bounded: an
oversize report serves a prefix with `rows_elided` (`DF:171-209`, `R/tests/pack_compaction_tests.rs:
2334`), never a refusal.

---

## 2. THE VOLUME LIFECYCLE

### 2.1 Identity: `vol-{hex}` minted once, never reused

`new_data_volume_id() = "vol-{:016x}"` random (`L:1044-1049`); a `Retired` record is kept FOREVER with
its path cleared so a stale key fails loud instead of resolving to a re-added device (KD-5 `D:643`,
R8 `D:607`). Port: `VolumeId.mint(random)` (`Z/data/router.zig:85-110`); `set_config.addDataVolume`
refuses a tag, device uuid or uri already present — "a retired record's tag stays taken forever"
(`Z/data/set_config.zig:659-666`); a retired record carries an EMPTY uri and the decoder refuses a
retired record with a uri or a live one without (`set_config.zig:515-516`).

### 2.2 The state lattice and every transition's durability order

```
Active ──remove-data (preflight OK)──► Draining ──census 0 ∧ used 0──► Retired (terminal; id forever)
  ▲                                       │
  └──────────── undrain ──────────────────┘
Active ◄──enable/disable──► Disabled      (the fail-stop health override; not an evacuation)
```
(`D:300-308`, `L:1008-1020`; port `transitionAllowed`, `set_config.zig:668-676`).

Durability order of EVERY lifecycle commit (port `set_config.zig:21-37`; reference `C:1415-1474`,
`FC:10344-10380`):

1. the live-writer gate on EVERY meta volume (offline) / the manager predicate (online);
2. read + decode `set_config` (undecodable ⇒ refuse by name);
3. preflight (device identity, capacity closed form, denylist);
4. stamp `VOLUME_LIFECYCLE` (bit 3) on every member superblock, barriered, iff `lifecycle_bit_needed`
   — BEFORE the record tx (`D:520`: a crash after the bit and before the record leaves a set old
   binaries refuse and this binary mounts unchanged; sector 0 is never rewritten by the live backend —
   checkpoints flip the ledger — so the stamp is race-free under the held claims, `C:1425-1428`);
5. ONE conveyor tx: `Put(set_config)` at `config_generation + 1`;
6. `add-data` only: the data device's superblock was written BEFORE this tx (device-before-record) and
   placement is enabled strictly AFTER the ack (`D:290-292`: "runtime registration is completed only
   after the durable commit acks — a crash between build and commit must not leave blocks persisted
   with an id the next mount does not know").
7. the retire: `state = Retired`, uri cleared, in ONE tx; runtime deregistration AFTER the durable
   commit (`J:3088-3132`, `RT:3256-3273`: dropping the last device handle joins its io_uring worker —
   off the serving lanes).

`Draining` semantics (`D:308`, `L:1015-1017`): excluded from write placement and NEW allocations
(weight 0 — a consequence of `placementEligible`, `Z/data/router.zig:149`), but SERVES READS and
refcount ops normally until retired (the EIO foot-gun retired; G-VL-3(f)). A mover pass that finds the
victim no longer `Draining` ends the evacuation as cancelled (`J:2762-2777`: covers offline/adopted
races with `undrain`).

### 2.3 Online `add-data` (+ the automatic rebalance)

`admin_add_data_volume` (`FC:10290-10420`): (2) preflight — `validate_new_data_volume` (`C:818-848`:
a usable backing device, sized, not already a member by CANONICAL path — symlinks must not smuggle a
duplicate, not a meta volume) and `probe_data_volume_rw` (`C:856-895`: write + readback of a random
4 KiB pattern at offset 0 through the SAME io_uring device worker the mount will use, restored to
zeros — a read-only or vanished device fails HERE, before any durable record names it; refusals count
`volume_preflight_refusals`); (3) build the runtime backend, placement NOT enabled; (4a) bit 3 on
every member; (4b) the record commit through the live conveyor; (5) publish the backend LAST, re-derive
the per-device I/O lane fan-out (monotone, never shrinks); (6) auto-submit `rebalance` at
`REBALANCE_DEFAULT_THROTTLE_PCT = 25` unless `--no-rebalance` (`J:127-129`, KD-12 `D:647`); a
submission failure is an error log naming `defrag --rebalance`, never a refusal of the add.

Port: the data superblock is `Z/data/data_superblock.zig`'s (device-before-record: the probe IS writing
the superblock with the minted `vol_tag`/`set_uuid`, so a crash leaves a formatted member no record
names — discovery's `not_formatted`/`renumbered` arms already judge it, `set_config.zig:742-760`);
the online path runs on the manager through the admin lane; the offline path is `cli/offline.withSet`
with `with_plane = false` + a durable `JobRecordV1{queued, rebalance, throttle 25}` written in the SAME
tx as `set_config` (`C:951-959`, `durable_queued_job_xattr` `J:401-417`) so the next mount's fabric
adopts it.

### 2.4 The auto-rebalance algorithm (within 10 pp of the mean fill)

`plan_rebalance` (`J:4135-4207`): rows = placement-eligible volumes with a capacity bound, `(id, used,
cap)`; `< 2` rows or `Σcap = 0` ⇒ empty. `mean = Σused / Σcap`. UNDER = rows with `fill < mean −
0.10` (`REBALANCE_BAND`, `J:133`), each with `deficit_blocks = ((mean − 0.10) × cap − used) / block`,
largest deficit first; OVER = rows with `fill > mean`, `surplus_blocks = (used − mean × cap) / block`,
fullest first. Legs = greedy: for each under-filled dest, take `min(want, have)` from the current
over-filled source, advancing sources as they exhaust (`J:4184-4206`) — never pushing a source below the
mean. `bound_rebalance_census` (`J:4213-4238`) clips the sources' census to the legs' budgets and tags
each task with its `dest_hint`; indirect blobs are never relocated by rebalance (they follow their
maps). The pass is BOUNDED: one plan, one execution, Completed (`J:3025-3038`). Continuous drift
prevention is the router's balance penalty (§5.9 `D:480-492`, already ported `Z/data/router.zig:14-24`).

Port: `jobs/rebalance.zig` pure `planLegs(rows, block_size) []Leg` in integer ppm arithmetic
(`fillPpm`, `router.zig:174`) — the band is `100_000 ppm`; tie test: a 70/40/10 % three-volume set at
equal capacities yields legs from the 70 % volume to the 10 % one of exactly `(mean − 0.10) × cap −
used` blocks and none to the 40 % one (within the band).

### 2.5 `remove-data` = drain via THE ONE MOVER

#### 2.5.1 Capacity preflight — each term

```
needed    = distinct_used_blocks(V) × block          census, DEDUPED per block (a clone-shared block once)
avail     = Σ_{b ≠ V, placement-eligible, cap > 0} (cap_b − used_b)     cap 0 (unbounded) contributes 0 (conservative)
transient = 64 × ACTUAL_workers × block             the in-flight double-count window
headroom  = max(write_rate_est × drain_eta, 1 GiB)  foreground growth during the drain
REQUIRE   avail ≥ needed + transient + headroom     else refuse printing every number
```
(`D:247-259`, `J:424-479, 1032-1078`). `write_rate_est` = the trailing-≤ 60 s mean of foreground block
consumption `Δ(write_through_blocks) + Δ(extent_spills)` × block / s, sampled at checkpoint cadence,
0 until two samples (`WriteRateEstimator`, `J:488-533`; the designed staged-promotion term is covered by
the floor — stated honestly `J:484-487`). `drain_eta`: the INITIAL estimate uses the G-VL-3(b) floor rate
`DRAIN_FLOOR_RATE = 512 MiB/s` (½ of a ~1 GiB/s devsub-class raw copy, `J:121-125`); from the first
checkpoint on, `eta = needed / measured_job_rate` with `job_rate = moved_bytes / elapsed_s` (`J:2823-
2835`). The `transient` term is conservative for a pure drain (the victim is excluded from `avail`) but
load-bearing for rebalance/defrag between survivors — it stays so one preflight covers every mover
(`D:261`). Re-verified at EVERY checkpoint; failure ⇒ `PausedCapacity`, loud, with the refusal text
(`J:2818-2865`).

Port: `jobs/preflight.zig` pure `Preflight{needed, avail, transient, headroom}.admits()`,
`transientBytes(workers, block)`, `headroomBytes(rate, eta_s)` with `HEADROOM_FLOOR = 1 GiB`, the
property test "refuse iff `avail < needed + transient + headroom`" over fuzzed censuses
(`R/tests/volume_drain_tests.rs:357`). The rate estimator reads `write_admit` stats
(`admit_write_*_bytes`, charter §6.2) — bytes directly, no block conversion.

#### 2.5.2 The census planner

`census_for(sources)` (`J:862-1027`): walk every meta volume's live inode tree in 512-key pages; per
inode with `nlink > 0` decode the layout; entries from the inline map, the indirect blob (whose own
block is a `blob_relocation` if it lives on a source), or the kvmap tree via the SHARED extraction
(a census that saw zero kvmap blocks would retire a volume with live data on it, `J:966-978`); group
by distinct base block → `MoveTask{base, src, src_offset, refs: [MoveRef{ino, block_idx, mapping
verbatim, staged}], dest_hint, dest_pick}`; refs sorted `(ino, block_idx)`; `distinct_blocks = tasks +
blobs`. Guest-only members' raw control records are skipped (`J:936-940`). Port: the walk is the
inode-plane walk fsck C8 already needs (`Z/data/block_refs.zig` `verifyRefs`), so `jobs/census.zig`
takes a `LayoutWalk` seam from the plane (`plane.layoutRecord`, `mapSeam`), yields `Task`s keyed by
`(vol_tag, block_idx)`; mappings are `layout.Mapping` values, never strings.

#### 2.5.3 `move_one` — the ONE mover primitive (copy-then-republish)

`J:3139-3489`, step by step; the port's law words in brackets:

1. OPEN-PACK gate: a base in the pack-open ledger or with an in-flight registration DEFERS
   (`pack_mover_open_defers`, `evacuate_deferred_staged_blocks`); when the source is the DRAIN's
   victim the pack is SEALED here so the next re-plan moves it (`J:3160-3182`; port `Packer.sealBlock`
   with `SealKind.drain`, `Z/data/pack.zig:584-593`).
2. QUIESCENT-FIRST: every referencer passes the probe (§1.4) or the task defers (`J:3186-3193`).
3. PIN the source (`pin_block_validated`, `J:3204-3241`): `Pinned` ⇒ proceed; `PinnedUnstable` ⇒ undo
   + defer (a patch mid-DMA); `Refused` with a tracked refcount ⇒ Superseded (freed concurrently —
   census stale); `Refused` untracked ⇒ move PIN-LESS (the staged-promoted class has no patch/free
   exposure). The pin makes the copy window patch-free (W1 needs refcount 1) and rides the MOVER
   PRE-PUBLISH LEDGER for the whole window (`J:3226-3233`, VL9 pin (b): an overlapping online fsck
   read refcount = refs + 1 as a C3 mismatch — a false positive on live mover state).
4. COPY: read the STORED image verbatim (transform-opaque — no plaintext materialization, `D:555`),
   `src_hash = xxh3`; charge the copy window (`CopyCharge`).
5. DESTINATION per `dest_pick`: `Balance` = `dest_hint` if still eligible else the LOWEST-fill
   placement-eligible survivor excluding the source (`pick_fill_destination`, `RT:3613-3657`: spreads a
   drain and drives rebalance toward the mean), `allocate_block`; `CompactLow` = `allocate_block_below
   (src_idx)` on the SAME backend, no gap ⇒ Superseded this pass — never a fresh tail mint (`J:3285-
   3304`); `BackendAscending{be, floor}` = `allocate_block_at_or_above(floor)` then `floor = idx + 1`
   (`J:3306-3335`). Register the destination in the in-flight allocation registry for the copy→publish
   window (fsck C2 exemption, `J:3337-3341`).
6. WRITE the image at the destination; VERIFY by a device read-back against `src_hash`
   (`J:3349-3373`): any failure ⇒ `abandon_unpublished_offset` (a never-published cleanup) + unpin +
   Deferred; a MISMATCH is an error log ("destination freed, source untouched"). Then `publish_block`
   (the incarnation word stable).
7. PRE-PUBLISH REFCOUNT TRANSFER (`J:3376-3392`, charter §8 "destination refcount raised before
   publish"): raise the destination to the FULL referencer count (`increment_refcount × (refs − 1)`;
   the mint's own +1 is the first) BEFORE any referencer publishes — no window where `dst refcount <
   published references`; the raised base enters the pre-publish ledger. A refused raise frees the
   destination and defers.
8. PUBLISH per referencer, ASCENDING ino (`J:3399-3462`): fire the test hook; take the block's flush
   lock (lattice 3 — the final check-and-move); re-probe quiescence (went live ⇒ superseded, release
   one destination ref); `merge_block_mappings(ino, MergeExpected[(idx, old_mapping, new_mapping =
   dst_base ‖ decoration_suffix(old))], KeepLayout, CURRENT token)` — tokens are READ, never
   incremented, by movers (`J:30-31`); `Ok(displaced ∋ old)` ⇒ `free_block(old)` (clone-aware
   decrement; terminal decrement punches) and `published += 1`; expected-mismatch (a foreground write
   replaced the mapping or a truncate pruned it) or `FencingTokenExpired` ⇒ the contractual no-op
   (`evacuate_stale_token_noops`) — release one destination ref; any other error ⇒ deferred, release
   one ref. The DECORATION SUFFIX (`:rel_off:len`) is preserved verbatim so a whole-block mapping stays
   whole-block and a packed tenant stays packed (`J:4033-4040`, the §5.4 "same mapping shape" law —
   W1 eligibility never shrinks across a drain, G-VL-3(d)).
9. UNPIN the source (`J:3464-3468`): every published referencer already freed one source reference;
   the pin is the LAST one ⇒ the terminal free punches through the ordinary ladder — never a direct
   free. Outcome: `published > 0` ⇒ Moved (+ `evacuate_blocks_moved`, `_bytes_moved`, `_shared_blocks_
   moved` iff `refs > 1`); else Superseded (every destination reference was released — the terminal
   free punched the destination).

A SHARED BLOCK MOVES ONCE (charter §8; `D:313-314`): the task is the distinct source block, its refs
are every referencer; `evacuate_shared_blocks_moved` is the instrument (G-VL-3(c)). The source is freed
only after the durable publish + tier purge (`J:3426-3431` through `free_block`'s `begin_free → tier
purge → punch → finish_free`, `D:325`). Indirect blob relocations are ONE empty merge under the ino's
current token (`J:2992-3020`: the save path reallocates a blob off a non-active volume).

Port — `Z/data/mover.zig` (Linux, the ONE primitive every mover job calls):

- `moveOne(plane, io, task, dest_pick, drain_victim: ?u64) Outcome{moved, deferred, superseded}`.
- The pin = `alloc.pinBlockValidated(idx)` + the in-flight registry (`InflightAllocGuard`) — the
  reference's RAM "mover ledger" (`J:4047-4077`) and "pack-open ledger" (`J:4086-4120`) are process
  globals under `parking_lot::Mutex<Vec<String>>`; the port's fsck exemption reads the allocator's
  in-flight registry and the packer's open-pack census (`Packer.openPack`), so neither global exists
  (quirk §6 #5).
- The destination = an own-block-shaped mint: `allocateBlock`/`allocateBlockBelow`/`allocate
  BlockAtOrAbove` (`Z/data/block_alloc.zig:826-887`) + `deviceWrite` of the STORED image (no
  `transformWrite` — the image is already stored form) + verify read-back + `publishBlock`; failure ⇒
  `abandonUnpublished` (`plane.abandonOwnBlock`'s law, `Z/data/plane.zig:899-903`).
- "Destination refcount raised before publish" in the port: the RAM refcount cell is raised to the
  referencer count exactly as the reference (`incrementRefcount`, `block_alloc.zig:1002`); the DURABLE
  side needs no pre-raise because each referencer's republish tx carries `+dst_ref(owner_i)
  −src_ref(owner_i)` atomically through `RefsSeam.stageRefs` (`Z/data/block_refs.zig:259-298`) — at
  every instant `population(dst) ≥ published refs` and `population(src) ≥ unpublished refs + 0`; the
  source's RAM pin is what keeps it alive across the window, and its terminal free waits for the
  last RAM release (the ladder, `Z/data/reclaim.zig:10-18`).
- The republish = ONE new layout primitive `plane.republishBinding(io, ino, block, expected: Mapping,
  new: Mapping) RepublishError!void` under `lockLayout` (3.7) + the block's flush stripe: read the
  layout record, require `lookup(block) == expected` (sameRef incl. the decoration — `Mapping.sameRef`
  and the tenant `rel_off/len`), stage `publishLayout` with the entry swapped and `stageRefs(old →
  new)`, commit; `error.Superseded` on mismatch. Fencing: the port's W1/overlay path keys on the
  incarnation word, not a DLM token (charter §6.2); the mover's "current token" is the layout lock +
  the expected-binding compare, which is strictly stronger (an expected-mismatch IS the supersession
  witness).
- The displaced source frees through `plane.freeBindings` AFTER the tx landed (RES-1, `plane.zig:906`).

#### 2.5.4 Convergence, retire, the kill matrix

Per pass (`J:2739-3083`): drain the reclaim queue first (planning against queued-not-finished frees
defers spuriously and never converges a defrag — mover cadence, never the write path, `J:2750-2757`);
re-plan; publish `tasks_total = done + remaining`; capacity re-verify (drains); if the census is
empty: drain reclaim again (space RETURNED, not queued), then rebalance/defrag ⇒ Completed, drain ⇒
`victim.used_blocks == 0` ⇒ `retire_volume` (`J:3088-3132`: record → Retired, path cleared, `data_lv`
mirror filtered, ONE tx; then the runtime snapshot + `retire_backend`) ⇒ Completed, else "census empty
but the victim still holds N allocated blocks (in-flight writes or leaks pending remount reclaim) —
re-planning" (`J:2905-2911`). Deferred work or an empty census ⇒ `REPLAN_BACKOFF` 200 ms. Termination
is guaranteed because the victim admits no new allocations (`D:316`).

Gauges published only for the ACTIVE drain and zeroed at exit (`J:2717-2720`): `evacuate_needed_bytes`,
`_avail_bytes`, `_transient_bytes`. Kill-9 anywhere ⇒ a consistent fs; `tests/run_volume_lifecycle.sh`
injects at mid-copy, mid-publish, mid-retire ×10 (G-VL-3(a) `D:589`).

#### 2.5.5 The interaction ledger (every touchpoint, `D:321-332`)

| Interacts with | Handling (reference) | Port home |
|---|---|---|
| CoW clone refcounts | move-once + pre-publish dst raise; frees through the refcount terminal protocol | `block_alloc.incrementRefcount/unpin`, `block_refs.stageRefs`, the reclaim ladder |
| RAM hot tier + NVMe read cache | the publish purges tiers; the source's free runs `begin_free → purge → punch → finish_free`; no reader observes stale bytes | `purge.zig` (ONE helper over four tiers), `reclaim_free.seam().free` |
| W1 patch eligibility | the mover republishes the SAME mapping shape (decoration verbatim) — a whole-block mapping stays whole-block; `patch_ineligible_*` must not grow across a drain (G-VL-3(d)) | `Mapping.sameRef` + the preserved `rel_off/len`; the pin makes the window patch-free |
| Staged extents / fold | quiescent-first; the mover never folds — it kicks the existing machinery | `probe.moverQuiescent`; `defrag_fold` through the plane's `FoldSeam` |
| Never-lossy writeback | a flush unit re-validates ownership per attempt and presents the current generation; a concurrent move makes the unit a verified no-op — the mover's expected-mismatch is the symmetric outcome | `writeback.zig`'s re-validation; `republishBinding`'s `error.Superseded` |
| Prefetch / single-flight | the old key stays readable until the post-publish free; the publish purge invalidates; `prefetch_evicted_unconsumed` watched in soaks | `single_flight_fill.zig`, `prefetch.zig` |
| Device overlay (one-path §6.2) | a live overlay record ⇒ NOT quiescent; defer + re-plan (`overlay_mover_skips`) | `device_overlay.zig` record table |
| S9 custody grants | a live grant ⇒ NOT quiescent (`job_mover_custody_defers`) | `sym_planes/custody.zig` owner table |
| Memory budget | `job_copy_buffers` sheddable; Red pauses jobs loudly | `budget.Component` |
| Conveyor / ring admission | mover commits are ordinary USER-class traffic; parking escalation applies unchanged | `conveyor.zig` |
| Reclaim queue | drained BEFORE planning (planning against queued frees never converges) and AFTER convergence (space RETURNED before `Completed`); the offline coordinator drains before releasing the claims (a queued punch outliving the claims lands on a successor's reallocation, `C:1303-1309`) | `Reclaimer.drain` |
| Open pack blocks | an OPEN pack is never a victim/source; on the drain's victim it is SEALED (`SealKind.drain`) | `Packer.sealBlock` |

### 2.6 `undrain`, `list`, `enable-symmetric`

- `undrain` (`FC:10570-10601`, `C:1007-1084`): refuse unless `Draining` ("retired volumes never come
  back: ids are permanent"); the DURABLE flip to Active FIRST (a mover pass observing non-draining
  self-cancels even if the explicit cancel races it), then cancel every non-terminal
  `evacuate_volume{v}` job; offline: the records are rewritten `Cancelled` in the same writer-door
  open so no future mount adopts them.
- `list` (`RT:3280-3317`, `D:267`): durable records joined with live allocator accounting — `{id,
  backing_dev, state, healthy, capacity, used, free}`; recordless registrations synthesize `active`
  rows; when a drain is active the preflight numbers ride the row. Port: `set_config.writeJson/
  writeSummary` already print the record half (`set_config.zig:263-294`); the live half joins
  `router.volumesLive()` fills through the admin lane.
- `enable-symmetric` (`C:4072-4178`): on the reference, the OFFLINE flat → forest re-lay of every
  volume (six durable steps with a marker, hybrid ledger, bit 17 stamp, free, marker removal; crash
  seams `EnableSymCrash` `C:3948-3970`) because the reference's `--single-writer` is the flat three-tree
  layout. In the port EVERY format is already the one-member FOREST (`Z/cli/format.zig:11`, `Z/meta/
  format.zig:16`: `--single-writer` = "the flat class, no appender region" where "flat" means NO
  `SYMMETRIC_PLANE` bit, not the three-tree layout; `Z/meta/superblock.zig:147-150`: "the appender
  directory and appender-0 page exist iff this bit is set. Absent on the one-member forest
  `--single-writer` builds"). So the port's verb is SMALL: offline, writer door on every member; refuse
  if already stamped (idempotent no-op reported), if a live client exists, if an in-flight job record is
  non-terminal; per volume: carve + ZERO the appender-directory extent (charter §5.5 "every carved ring
  extent is ZEROED before a page names it"), write appender 0's `Free` page into the fixed ring's first
  4 page slots, write tree 0's `slot_state:{s} = Unleased{root, cursor, g: 0}` for every hosted slot
  (`control_records.zig` `SlotState`), then ONE sector write: `features_incompat |= SYMMETRIC_PLANE`
  + `appender_dir` (`superblock.zig:177-178`) — bit-last, so a crash before it leaves a `--single-writer`
  volume with an unreferenced zeroed extent the next writer door's orphan census returns (the reference's
  order "stamp after the build" `C:4150-4153`). No marker, no hybrid ledger, no re-lay: the forest is
  already there. Report rows `{path, outcome ∈ {converted, already_symmetric, planned}, extents_written}`.

### 2.7 `add-meta` / `remove-meta` / `migrate-meta-slot` / `repair-set`

Already ported (`Z/meta/routing/offline.zig:252, 462, 597`; `Z/meta/routing/migrate.zig`). Rung 8's
only change: `migrate-meta-slot` on a LIVE mount becomes `JobType.migrate_meta_slot` submitted through
the admin lane and run by the fabric on the manager (`Z/cli/volume.zig:14, 210`: "routing it into a
serving daemon is the admin channel of rung 8's job fabric"); the engine is untouched. The staging
drain barrier + generation restamp (`D:400-405`, `C:1780-1873`) is in `offline.zig:183-197`
(`stagingRebindPrepare/Finalize`).

---

## 3. DEFRAG — the four axes

### 3.1 The fragmentation model (KD-11, `D:457-467`; `DF:1-32`)

At 4 MiB blocks and 256 KiB meta extents classical intra-file fragmentation does not exist;
"fragmentation" IS four measured axes, each a gauge + an independently invocable mover reusing
existing machinery:

| Axis | Metric | Mover |
|---|---|---|
| D1 free-space contiguity | per volume over the ALLOCATED offset space `[0, highest)`: `contiguity = largest_free_run / total_free`, `reclaimable_tail = trailing_free_run / total_free` (both 1.0 when nothing is free) — the virgin tail past `highest` deliberately does NOT count (already contiguous and reclaimable; counting it masks real fragmentation on any volume not nearly full, `DF:16-20`) | `defrag_data`: referenced blocks at/above the compaction frontier (`frontier = used_blocks` — `used` blocks fit `[0, used)` exactly) into the lowest same-backend gap strictly below the source (`DestPick::CompactLow`) |
| D1 pack face | per PACK block (every referencer a staged tenant's size-carrying mapping at `block_map[0]`): `occupancy = Σ_{distinct off} pack_slot_len(max len at off) / CHUNK`; same-`off` windows (the clone share, the nested prefix share) counted ONCE; VICTIM iff `live ≤ pack_max_slot` (`DF:276-293`: the one law's other face — copying `live` reclaims `CHUNK − live`, which pays iff `live ≤ CHUNK/2`) | `defrag_pack` (§3.5) |
| D2 file locality | over logical-order striped entries, a pair counts when `idx₂ = idx₁ + 1`; LOCAL when same backend AND physically ascending; `locality = local / pairs` (1.0 when no pairs) (`locality_pairs`, `DF:588-600`) | `defrag_data`: refcount-1 quiescent rewrites of a file onto its plurality-eligible backend, ascending (`DestPick::BackendAscending`); clones never de-shared |
| D3 staged-extent pressure | `parked_extent_bytes + Σ spilled extent-record bytes` (`DF:635-651`) | `defrag_fold`: kick every parked/spilled block through the EXISTING W2 fold (`J:2490-2557`) — never reimplemented |
| D4 meta node occupancy | per meta volume after paging every leaf resident: `dead_bset_ratio = 1 − live/indexed` over the serialized bset logs; `mergeable_leaves` = underfull leaves (fold ≤ ¼ capacity) (`DF:661-704`) | `defrag_meta`: leaf compaction through the SMO serialization in chunks of 8, then ONE whole lap of the §4.6a merge sweep under the checkpoint's `merge_sweep_budget_ms` per chunk, duty-parked between chunks (`J:2385-2469`) |

Ratios are gauges encoded permille + 1 (`encode_ratio`, `DF:44-51`: raw 0 = never measured ⇒ the
stats JSON emits `null`), worst-volume semantics for per-volume axes; `frag_d4_mergeable_leaves` is a
COUNT summed over the set (`DF:736-740`). D1/D3 are cheap (allocator snapshot + record lens) and refresh
every 5 s on the gauge worker and after every mover pass (`DF:764-788`); D2/D4 and the pack face are
walk-priced and refresh when `measure` runs (`--report-only`, the verbs) and after every compaction pass
— stale-until-measured by design, never guessed (`DF:26-32`).

### 3.2 The report and `--report-only`

`DefragReport{d1: [D1Volume], d2: D2Report, d3: D3Report, d4: [D4Volume], pack: PackReport}`
(`DF:54-168`); `to_bounded_json(max_bytes)` serves the longest fitting worst-occupancy prefix of the
pack rows with `rows_elided` (`DF:171-209`; found by the PK7 rig: one row per legacy block refused the
whole admin reply). `--report-only` computes and publishes, moves nothing; offline = read-only probes
(refused under a live writer), a probe-shaped router with the allocator census rebuilt, D3 reads 0 by
construction (staging is mount-owned, `DF:794-828`). G-VL-6 pins `--report-only` numbers against an
independent census (`R/tests/defrag_tests.rs:373, 451`).

### 3.3 The verb

`squeezefs defrag TARGET [--data] [--pack] [--volume ID] [--meta] [--fold] [--rebalance]
[--report-only] [--throttle PCT (default 100)] [--json]` (`R/main.rs:376-421`). `--fold` is live-mount
only (fold custody is the mount's; the offline coordinator fails loud `J:2491-2499`); `--rebalance` is
the same pass `add-data` schedules; offline mutating arms run the D0-guarded coordinator and seal the
open packs before the guard goes (`DF:836-899`: a compaction's destination is this coordinator's open
pack — the dismount discipline). Interaction pins (`D:755-759`): a `defrag_data` NAMING a drain's
victim queues behind it (intersecting scope) then fails LOUD at plan time — not placement-eligible
(`J:3944-3949, 4287-4292`); different volumes run concurrently.

### 3.4 `plan_defrag` (D2 then D1, `J:4271-4424`)

Scope = the named volume (must exist and be placement-eligible) or every eligible one, sorted. D2:
`walk_striped_files` (`DF:482-582`); per file with `pairs > 0 ∧ local < pairs` touching the scope: target
= the PLURALITY backend among eligible canonical owners (ties by id); ONE ascending `floor` per file
(the convergence invariant: each publish bumps it past its destination, so one pass leaves the file
same-backend ascending and the next plan finds nothing, `J:4346-4348`); a task per refcount-1 block not
already planned. D1: `census_for(scope)`; skip planned bases and bases below the frontier; the rest
become `CompactLow` tasks sorted `(ino, logical idx)` so ascending gap consumption preserves per-file
physical order — compaction must not mint D2 work (`J:4406-4415`). Shared blocks compact fine (the
move-once machinery); D2 skips them. Indirect blobs are the drain's (`J:4268-4270`).

### 3.5 `--pack` compaction (charter §6.1; `J:3503-3901, 3930-4013`; `DF:271-464`)

Gate: the packing lever ON — a lever-OFF mount refuses the WHOLE arm loud, an adopted lever-ON-era
record included (`J:3513-3523`; port `knobs` `SQUEEZEFS_SMALL_FILE_PACKING`, `Z/core/knobs.zig:205`).

Plan (`plan_pack_compaction`): drain reclaim; `packer.begin_promotion_batch()` (a compaction pass IS a
promotion batch: an earlier `StorageFull` stop re-arms); census over the scope; per distinct block
`pack_windows` (`DF:321-349`: `None` unless EVERY referencer is a staged tenant at `block_idx 0` with
an exact size-carrying mapping — a striped block, a bare legacy `bk`, a `damaged:` marker or an
undecodable decoration is never a victim; windows grouped by `off` at the max `len`, the longest
referencer indexed, refs in census order); `live = Σ slot` clamped to CHUNK (`DF:355-361`); victim iff
`is_pack_victim(live)`; an OPEN pack is never a victim (deferred at plan, re-checked at move);
victims ascending `(live, base)`. A plan executes only if it frees ≥ 1 block: two victims always net
one (each ≤ half a chunk); a LONE victim frees one iff the open pack already has room (`open_room_bytes
≥ live`), else `skipped_lone_victim` and nothing moves (`J:3920-3929, 4008-4011`).

`compact_one` (`J:3670-3901`) — `move_one`'s discipline with three stated deviations: (1) the
destination is the OPEN pack through `router.repack_window(longest.ino, longest.mapping)` (ONE ranged
read of the window at `max_len`, re-packed; `Ok(None)` = the arm is `StorageFull`-stopped ⇒ the rest of
the victim waits); (2) the destination carries one reference per REFERENCER before any publish — the
handle's own + `refs − 1` raised; a structural refusal of the raise is a TRIPWIRE
`pack_compaction_raise_refused` (`J:3745-3775`); (3) every referencer republishes `MergeExpected(old →
dst:off':len_i)` with its OWN len at the shared slot, under its flush lock + quiesce re-check + current
token, ascending ino; each refused referencer releases one destination reference OUTSIDE the lock
(RES-1); the handle's own reference is released LAST (a window nobody took is an abandoned slot,
`router.abandon_repacked_window`); `note_committed` + `pack_compaction_windows_copied/_bytes_copied`
per landed window. The pin's release is the victim's LAST reference iff every tenant left — the
terminal free through the ordinary ladder (`free_block_verdict`, `pack_compaction_blocks_freed`,
`_bytes_reclaimed = CHUNK − live`). Each distinct `off` group is copied ONCE at max len (the charter's
sentence verbatim: `J:3722-3744`).

Port: `jobs/defrag_pack.zig` over `pack.Packer` — the window read is `plane.readMappingImage(longest)`
(`Z/data/plane.zig:1017`), the re-pack is `Packer.prepare` (ONE DMA of exactly `slot` bytes into the
open pack, `pack.zig:615`), the per-referencer publish is `plane.republishBinding` (§2.5.3) with the
tenant mapping `Mapping.tenant(vol_tag, base, incarnation, off', len_i)`; the refcount arithmetic
rides `Packer`'s pin law (`pack.zig:17-22`: `refcount = committed + in-flight + 1`), so "raise before
publish" = `incrementRefcount × (refs − 1)` on the open block's cell before the first republish.
After the pass: `measure_pack` + `refresh_d1_d3_gauges` (walk-priced re-gauge from the moved state).

---

## 4. Knobs, gauges, constants

### 4.1 Knobs (registry rows; class per `Z/core/knobs.zig:57-70`)

| Row | Kind / range | Default | Class | Reader | Why |
|---|---|---|---|---|---|
| `SQUEEZEFS_JOB_CPU_LIMIT` (mount flag `--job-cpu-limit` in the reference, `D:510`) | int 1..=100 | 100 | `ab_lever` (100 = unthrottled control arm) | `jobs/fabric.zig` as the default throttle for a submit without one (`J:1512-1516`) | KD-3 |
| `SQUEEZEFS_JOB_WORKERS` | int 1..=64 | derived `clamp(cpus/4, 2, 8)` | `derived_override` | `jobs/coordinator.zig` | `J:1203-1205` |
| `SQUEEZEFS_JOB_WIRE_VERIFY_PERMILLE` | int 1..=1000 | 1000 | `ab_lever` (1000 = the plaintext law) | `jobs/wire.zig` — honoured ONLY under mTLS | `W:918, 977-990`, Issue-30 |
| `SQUEEZEFS_FLEET_JOBS` | bool | 1 | `switch_` | member worker at mount + the fan-out (`F:81-83`) | opt-out of both halves |
| `SQUEEZEFS_TEST_JOB_PARK_DELAY_MS` | int 0..=60000 | 0 | `test_seam` | the park window (`J:82-96`) | the lost-resume repro |
| `SQUEEZEFS_TEST_JOB_PANIC_AFTER` | — | — | DELETED | — | a panic seam has no Zig analogue; the claim-release-on-error path is pinned by an error-returning noop task instead |
| `SQUEEZEFS_TEST_JOB_HOLD_SUBMISSION` | bool | 0 | `test_seam` | the worker parks before `result_submit` (`W:3459-3463`) | the zombie window |
| `SQUEEZEFS_TEST_MOVER_PRE_PUBLISH_HOLD` | bool | 0 | `test_seam` | `mover.zig` parks after the copy, before the flush lock (`J:618-642`) | the supersession / crash-injection window |
| `SQUEEZEFS_JOB_WIRE_BIND/_CA_CERT/_CA_KEY/_MAX_CONNS/_ENROLL_FRESHNESS_MS` | — | — | NOT ADDED | the job verbs ride the ONE listener: `SQUEEZEFS_MW_BIND`, `SQUEEZEFS_CLUSTER_WIRE_MAX_CONNS` (`knobs.zig:284-285`), and the wire's TLS rows `SQUEEZEFS_WIRE_CHANNEL` + `SQUEEZEFS_WIRE_TLS_{CA,CERT,KEY}` (rung 10, 0003 §O; the verify-permille knob is honoured once the posture is `mutual_tls`) | one listener, one bind (quirk §6 #2) |
| `SQUEEZEFS_SMALL_FILE_PACKING`, `SQUEEZEFS_PACK_MAX_SLOT_BYTES` | existing | | | the `--pack` gate + victim line (`knobs.zig:205-206`) | |

### 4.2 Gauges (`counters.ShardedCounter` unless noted; `Gauge` for levels)

- Fabric: `job_submitted, job_completed, job_cancelled, job_failed, job_tasks_done, job_checkpoint_
  writes, job_serialized_waits, job_paused_mem_pressure, job_paused_capacity` (`FC:6282-6319`);
  gauges `job_active/queued/paused` (`D:560`), `job_copy_buffer_bytes` (the budget component's gauge),
  `job_throttle_pct` per active job in a sub-object. `job_worker_panics` retired (§1.3).
- Mover: `evacuate_blocks_moved, evacuate_bytes_moved` (THE drain engagement instrument — a drain row
  is INVALID unless it accounts for the victim's used bytes, `D:114, 566`), `evacuate_shared_blocks_
  moved, evacuate_inflight_bytes, evacuate_stale_token_noops, evacuate_deferred_staged_blocks,
  evacuate_replans`; gauges `evacuate_needed_bytes, evacuate_avail_bytes, evacuate_transient_bytes`
  (active drain only); `job_mover_custody_defers, overlay_mover_skips, pack_mover_open_defers,
  pack_mover_resident_defers`.
- Lifecycle: gauge object `volume_states[{id, state, healthy, capacity, used, free}]`,
  `volume_preflight_refusals`, `placement_table_refreshes` (exists), `fuse_reserved_xattr_refusals`
  (exists).
- Remote wire: `job_remote_workers` (gauge), `job_remote_enrollments, job_remote_enroll_refused,
  job_remote_shards, job_remote_submissions, job_remote_refused_stale, job_remote_lease_expiries,
  job_remote_reassignments, job_remote_bytes_moved, job_remote_verify_read_bytes, job_remote_
  quarantined_destinations, job_remote_pr_preempts`; gauge `job_remote_fence_mode ∈ {0 deferred-
  reclaim, 1 pr}` (`FC:6589-6622`, `D:561`). `job_remote_shards/_bytes_moved` are the remote-engagement
  instrument; `_refused_stale` and `_enroll_refused` the security/fencing tripwires.
- Fleet: `job_fleet_shards_dispatched, _completed` (engagement pair), `_relocal` (closure `dispatched
  ≡ completed + relocal + refused-stale-pending`), member `job_fleet_worker_shards, job_fleet_worker_
  red_aborts` (`F:222-234`).
- Defrag: `defrag_blocks_moved, defrag_bytes_moved, defrag_folds_kicked, defrag_meta_compactions_
  kicked, defrag_meta_merges`; gauges `frag_d1_contiguity, frag_d1_reclaimable_tail, frag_d2_locality,
  frag_d3_pressure_bytes, frag_d4_dead_bset_ratio` (permille + 1), `frag_d4_mergeable_leaves` (count),
  `frag_d1_pack_occupancy, frag_d1_pack_occupancy_mean, frag_d1_pack_blocks, pack_reclaimable_bytes,
  pack_blocks_below_half`; counters `pack_compactions, pack_compaction_tenants_moved, _windows_copied,
  _bytes_copied, _bytes_reclaimed, _blocks_freed, _deferred` (`FC:5944-5969, 6560-6586`).
- Tripwires (`counters.Tripwire`, must-stay-0): `pack_compaction_raise_refused` (`J:3757`),
  `job_quarantined_destination_allocated` (the allocator returned a quarantined tuple, `W:2329-2334`),
  `job_terminal_regressed` (a terminal state asked to change).

Closure identities (stated and tested, charter §10): `job_submitted ≡ job_completed + job_cancelled +
job_failed + live non-terminal`; `job_remote_shards ≡ job_remote_submissions + job_remote_lease_
expiries + in-flight` (a submission after an expiry is refused stale, not counted twice); `job_fleet_
shards_dispatched ≡ completed + relocal + refused-stale-pending`; `evacuate_inflight_bytes` returns
to 0 at every job exit (the RAII charge); per drain `evacuate_blocks_moved + superseded + remaining ≡
census.distinct_blocks` at each re-plan; `job_copy_buffer_bytes ≤ 64 × workers × block` always.
Engagement: a drain row is INVALID unless `Δevacuate_bytes_moved` accounts for the victim's used bytes;
a distributed row unless `Δjob_remote_shards` accounts for the remote share; a fleet fsck row unless
`completed` accounts for `dispatched`.

### 4.3 Constants (with WHY)

| Constant | Value | WHY / measured | Ref |
|---|---|---|---|
| `CHECKPOINT_TASKS / CHECKPOINT_SECS` | 256 / 5 s | ≈ 1 GiB moved per checkpoint at 4 MiB blocks — bounded re-work on crash | `J:56-57`, `D:190` |
| `EVACUATE_INFLIGHT_WINDOW_BLOCKS` | 64 | one write-pipeline BDP window per worker; 256 MiB at 4 MiB; ≤ 2 GiB at the 8-worker cap | `J:115`, `D:217` |
| `DRAIN_HEADROOM_FLOOR` | 1 GiB | slack reserved even at a zero rate estimate | `J:119` |
| `DRAIN_FLOOR_RATE` | 512 MiB/s | ½ of a ~1 GiB/s devsub-class raw copy (G-VL-3(b)); replaced by the measured job rate from the first checkpoint | `J:125` |
| `REBALANCE_DEFAULT_THROTTLE_PCT` | 25 | the conservative auto-pass | `J:129` |
| `REBALANCE_BAND` | 10 pp | the charter's "within 10 pp of mean fill" | `J:133` |
| `REPLAN_BACKOFF` | 200 ms | a no-progress pass waits for quiescence / in-flight publishes | `J:138` |
| `DEFRAG_MAX_PASSES` | 16 | termination belt under churn — a maintenance job completes best-effort, never wedges | `J:144` |
| `MOVER_PIPELINE_WIDTH` | 4 | overlaps the 3 serialized device round trips per move; the serial loop measured exactly the ½-of-raw floor | `J:102-109` |
| duty slice | 50 ms | a retune/pause reaches a parked worker within one slice; the total park is unchanged | `J:1865` |
| worker park (no work) | 500 ms bound | a lost notify cannot strand the pool | `J:1997-2003` |
| workers default | `clamp(cpus/4, 2, 8)` | the L4 slope, floored for a coordinator pair, ceilinged against a worker farm | `J:1203` |
| `LEASE_TTL / HEARTBEAT` | 30 s / 10 s | charter §8 | `W:181-183` |
| sweeper tick | `max(TTL/4, 25 ms)` in ≤ 250 ms slices | observes shutdown promptly | `W:2389-2404` |
| `REVALIDATE_BATCH` | 16 destinations | rung 1 of the zombie ladder: exposure ≤ one batch | `W:210` |
| half-TTL round trip bound | `TTL/4` | the ack must move the deadline before the next batch | `W:3563` |
| `ENROLL_DIAL_TIMEOUT` / handshake / body / freshness | 10 s / 10 s / 30 s / 30 s | VAL-6 bounds | `W:206, 883-887` |
| session idle | `max(4 × heartbeat, 60 s)` | a silent session is closed (a capped resource) | `W:1417-1419` |
| nonce cap | `max(4 × max_conns, 64)` | already `auth.nonceCap` | `W:1422`, `Z/wire/auth.zig:67-69` |
| D4 nudge chunk | 8 leaves | duty-parked between chunks | `J:2386` |
| gauge worker cadence | 5 s | the health-worker precedent | `DF:775-781` |
| offline mover bound | 7 days | a belt; kill-9 resumes by re-invocation | `DF:885` |
| `pack_max_slot` = victim line | `CHUNK/2` | the one law's two faces | `DF:276-288`, `Z/data/pack.zig:7-14` |

Scale math (`D:334`): 8 TiB victim at ~1 GiB/s net ≈ 2.3 h at 100 %, 4.5 h at 50 %, 9 h at 25 %;
2 M moves ≈ 8 k conveyor batches; ~8 k progress commits, negligible.

---

## 5. Tests (name — sentence; needs)

Portable unless marked `[L]` (Linux venue). Reference anchors in `R/tests/`.

Fabric (`job_fabric_tests.rs`):
- `throttle law is exact at every duty target` — `dutySleepNs(t, pct) = t × (100 − pct)/pct`, null at 0/≥ 100/< 1 ms (`:343`).
- `duty cycle adheres within ten percent at 25/50/75` — task-active/wall over a noop run (`:369`); needs `std.testing.io` clock control.
- `noop job runs to completion and records are durable` — the record flips terminal on disk BEFORE the waiter sees it (`:239`).
- `a held fabric claims no job until admission opens` (`:289`).
- `pause / resume / cancel / live rethrottle` (`:431`).
- `resume inside the park window never strands the job` — `TEST_JOB_PARK_DELAY_MS` seam (`:954`).
- `live rethrottle reaches a parked worker` within one slice (`:1024`); `cancel inside the park window stays terminal` (`:1076`).
- `crash resume reconstructs from durable records` — abrupt shutdown, restart adopts, Paused stays paused (`:509`).
- `job list is offline-probe shaped` — the `Tag.job` range walk (`:581`).
- `the enroll record is a control record, not an undecodable job` (`:1297`) — trivially true with binary tags; keep as the walk's filter pin.
- `reserved names are invisible and EPERM through FUSE` (`:124, 171, 206`) — already pinned by the allowlist tests `Z/fuse/handler/kv_fs.zig:3404-3469`.
- `an error-returning job releases its claim and keeps the worker` — the RES-7 contract without a panic (`job_worker_panic_tests.rs:120, 196`).
- `JobRecordV1 / JobShardV1 round-trip; a tampered byte, a wrong version, an unknown type byte, a reserved bit and a wrong length are Corrupt` + fuzz "never panic on arbitrary bytes".

Wire (`job_wire_tests.rs`, `[L]` for TCP legs, the loopback `Z/wire/loopback.zig` for the rest):
- `enrollment HMAC gate and schema refusal` (`:269`); `challenge nonce is single use` (`:1022`); `expired challenge is refused` (`:1086`) — already the ladder's tests; add `a job_enroll with a lying cap buys a shard the worker abandons`.
- `plaintext transport forces mandatory verify-reads` — permille pinned 1000 (`:356`); `TLS round trip keeps sampling` (`:421`) `[L]`.
- `remote worker executes a noop shard end to end` (`:505`).
- `lease expiry reassigns to the local pool and refuses the late submit` — `job_remote_refused_stale ≥ 1` (`:574`).
- `a reassigned shard gets fresh destinations and the expired ones are quarantined` — the allocator never hands a quarantined idx back (`:697`).
- `WERO fence acquires, preempts and releases over the wire` — the fake PR client: a preempted key's writes reject while registrants proceed; an unregistered host is write-blocked while the reservation stands (`:186, 835`; `Z/meta/guard/fake_nvme.zig`).
- `a coordinator's restart keeps the set's enroll secret` (`:1372`); `CA-less TLS is refused` (`:935`); `CA cert without key refuses the listener` (`:987`).
- `pre-enrollment frames ride the handshake class cap`; `a lying length prefix allocates one chunk` — exist for the wire; add the bulk class row.
- `SIGSTOP zombie: paused past TTL, reassigned, resumed DMA lands only on quarantined offsets; late submit refused` `[L]` (G-VL-7 `D:593`) — in-process: a worker fiber parked by `TEST_JOB_HOLD_SUBMISSION`.

Lifecycle (`volume_lifecycle_tests.rs`, `volume_drain_tests.rs`):
- `new volume ids are vol-hex16 and unique` (`:330`); `lifecycle bit is bit 3 and intersects no earlier bit` (`:343`).
- `an old mask refuses bit 3; the stamp is durable and idempotent` (`:364`) `[L]`.
- `offline add refuses under a live client and bad devices` (`:440`) `[L]`.
- `add → write → remount → read; the new backend receives allocations` (`:498`) `[L]`.
- `preflight property: refuse iff the closed form` — fuzzed censuses (`drain:357`).
- `a draining volume serves reads and takes no placements` (`drain:411`).
- `drain evacuates, retires and preserves bytes; the W1 ledger delta is zero` (`drain:465`) `[L]`.
- `clone-shared blocks move once, both clones intact` — `evacuate_shared_blocks_moved == distinct shared blocks` (`drain:581`).
- `stale mapping supersession is a contractual no-op` — a write between copy and publish wins (`drain:674`; the pre-publish hold seam).
- `quiescent-first defers live buffers then converges` (`drain:786`).
- `remove-data refuses without survivor capacity` printing every term (`drain:892`).
- `undrain restores active and cancels the job` (`drain:950`).
- `auto-rebalance is submitted on add and suppressed by --no-rebalance` (`drain:1023`); `the offline add writes a durable queued rebalance record` (`drain:1155`).
- `rebalance moves toward the set mean` — a 70/40/10 set ends within 10 pp (`drain:1084`).
- `a crash mid-drain resumes by re-plan and preserves bytes` (`drain:1207`) `[L]`; `offline remove-data drains to retired` (`drain:1295`) `[L]`.
- `an unthrottled drain overlaps block moves` — width 4 engaged (`drain:1353`).
- `close returns queued reclaims before custody release`; `a late reclaim after close cannot zero republished blocks`; `a crash analog ceases device reclaims` (`drain:1473-1606`).
- `plan_rebalance legs tie test` (new): exact deficit/surplus arithmetic in ppm.
- `enable-symmetric on a --single-writer set stamps bit 0 + the appender directory; idempotent; refuses a live client / a non-terminal job` `[L]`.

Interactions (`interaction_tests.rs`):
- `intersecting-scope movers serialize, queued loud, then run` (`:408`); `disjoint scopes run concurrently` (`:543`).
- `defrag naming a draining volume fails loud` (`:636`).
- `Red pressure pauses jobs loudly and operator resume recovers` — the shed hook, no self-resume (`:690`).
- `fsck completes clean while a drain is parked mid-move` — the in-flight registry exemption (`:827`, rung 8's fsck).
- `slot migration and data drain converge concurrently` (`:957`).

Defrag (`defrag_tests.rs`, `pack_compaction_tests.rs`):
- `ratio codec round-trips; 0 is never-measured` (`:353`); `D1 measure matches an independent census` (`:373`); `report-only gauges match` (`:451`).
- `defrag --data: contiguity ≤ 0.3 → ≥ 0.9 and reclaimable tail ≥ 90 % under churn, zero corruption` (`:582`) `[L]`.
- `D2 streaming fixture strictly improves, bytes identical` (`:760`); `D3 fold kick drains parked bytes` (`:823`); `D4 nudge increments compactions` (`:922`); `the merge pass honours the throttle` (`:1208`).
- `defrag throttle live retune and pause` (`:1317`); `kill-9 mid-defrag; the adopted job converges, manifest intact` (`:1402`) `[L]`; `offline report-only probe` (`:1508`).
- Pack: `occupancy is exact on a planted population and counts both share classes once` (`:661`); `victims below half re-pack into one fresh pack and free, a block above half untouched` (`:748`); `the legacy one-block-per-file population is compacted` (`:878`); `a clone share is copied once, both referencers on one window` (`:962`); `a nested prefix share is copied once at the longer len` (`:1023`); `a tenant re-staged mid-plan is deferred, not copied` (`:1088`); `an overwritten tenant re-promoted elsewhere releases its old reference` (`:1173`); `a refused publish releases exactly one destination reference` (`:1231`); `a corpse whose release outlived its record is never a referencer` (`:1337`); `kill-9 mid-pass: no leak, no double free, the rerun converges` (`:1435`); `compaction is throttled, pause/resume/cancel-able` (`:1508`); `compaction serializes loud behind a running drain on the same volume` (`:1621`); `lever off gates the whole arm; report-only moves nothing` (`:1717`); `defrag report serves a bounded view under the admin body cap` (`:2334`).

Models (`Z/model/explorer.zig`): the CLAIM word (`claimed` CAS + the RAII release vs a concurrent
`claim_next` over an intersecting scope: never two claimed movers on one volume; weakening = drop the
scope check); the PARK/RESUME Dekker pair (`settle_park` under the state lock vs `resume`: never
`paused = false ∧ state = Paused`; weakening = flip before the re-check); the shard FENCING pair
(expiry bump + holder take vs a late submit: a stale fencing never publishes; weakening = compare
before the bump lands).

---

## 6. Quirks per decision 0002 (add a row before diverging)

1. **JSON xattrs on ino 1 as job records** (`J:362-376`, `W:2721-2743`; `serde_json`, additive,
   unchecksummed, string `error`/`created_by`) → canonical `JobRecordV1`/`JobShardV1` control records
   on tree 0 with explicit enum bytes, a typed `error_class`, the stable identity, xxh3 (rows 121–124's
   law applied to the job plane).
2. **A second TCP listener (`sqz-jw-accept/dispatch/sweep` OS threads, `bind 0.0.0.0:0`) and a
   `job_endpoint` heartbeat field** (`W:1465-1500`, `D:227`) → the job verbs are a BLOCK on the one
   cluster listener found through the manager's `Tag.endpoint` record; one bind knob, one connection
   cap, one TLS posture.
3. **`WIRE_SCHEMA` bumps 1→4 because bincode is positional** (`W:149-165`) → `extern struct` frames
   with reserved words; the schema is the block's and starts at 1.
4. **`KvmapSweep` as a fabric job type with a mount-time adoption scan** (`J:226-239, 707-771`) because
   the router predated the fabric at mount → the port's sweep is a plane fiber resumed from its
   durable `;sweep:K` cursor (`Z/data/plane.zig:1936-1960`, `block_map_tree.md` §4); not a maintenance
   op, not a job (charter §5.6 names it a write-path tail).
5. **Process-global `MOVER_PREPUBLISH_LEDGER` / `PACK_OPEN_LEDGER` `Mutex<Vec<String>>`** (`J:4047-
   4120`, with a `test_pack_ledger_clear` seam because an in-process crash analog leaked entries) → the
   allocator's in-flight registry (`InflightAllocGuard`) and the packer's open-pack census are the
   fsck exemption sources; no string ledger exists.
6. **Panic containment (`catch_unwind` + `job_worker_panics` + a string `Failed`)** (`J:2016-2036`) →
   error unions + `error_class`; row 9's law.
7. **`created_by = "hostname:pid"` read from `/proc/sys/kernel/hostname`** (`J:4433-4437`) → the
   `StableIdentity{node_token, mount_slot}` + pid.
8. **The quiescence probe and the fold hook as `Arc<dyn Fn>` closures installed at mount
   (`QuiesceProbe`, `FoldHook`, `EVAC_PRE_PUBLISH_HOOK` RwLock'd globals)** (`J:548-642`) → vtables on
   `plane.probe()` and a `FoldSeam` the plane exports; the pre-publish hold is a registered test seam.
9. **`enable-symmetric` as the six-step flat→forest re-lay with a marker on every volume**
   (`C:4072-4178`) → the port's `--single-writer` is already a one-member forest; the verb stamps
   `SYMMETRIC_PLANE` + writes the appender directory and appender 0's page (§2.6). The reference's
   crash seams `EnableSymCrash` do not apply; ONE seam (crash before the sector write) does.
10. **`DRAIN_FLOOR_RATE_BYTES_PER_SEC` as a constant devsub number** (`J:125`) → keep the number but
    name it a MEASURED floor (`// measured: ½ of ~1 GiB/s raw copy, devsub nvmet-loop`), replaced by the
    live rate from the first checkpoint; the tie test restates `eta = needed / 512 MiB/s + 1` for the
    first pass only.
11. **Shard fencing starts at 0 per `(job, shard)` and lives only in RAM between checkpoints**
    (`W:2305-2321`) → the port persists the fencing in `JobShardV1` at every bump (it already persists
    the record on `reclaimed`) and seeds the live word from it at adoption, so a coordinator restart
    cannot re-issue a fencing a zombie still holds.
12. **`volume_states` rows synthesized `active` for recordless registrations** (`RT:3307-3316`) → the
    port has no recordless volumes (`set_config` is the ONE source, `set_config.zig:3-6`); no synthesis
    arm.

---

## 7. PORT PLAN (sizes: S ≤ 1 day, M ≤ 3 days, L ≤ 1.5 weeks)

| # | Module | Size | Content | Depends on |
|---|---|---|---|---|
| 1 | `Z/meta/sym/control_records.zig` (+ `Tag.job`, `Tag.job_shard`, `JobRecordV1`, `JobShardV1`, codecs, fuzz) | S | §1.2 | — |
| 2 | `Z/jobs/throttle.zig`, `Z/jobs/preflight.zig`, `Z/jobs/rebalance.zig` (pure; tie tests) | S | §1.5, §2.5.1, §2.4 | — |
| 3 | `Z/jobs/fabric.zig` (portable core: `JobCtl`, `claimNext` with mover scopes, state machine, park ceremony, checkpoint cadence, adoption, `shedTo`) + `Z/model/job_claim_model.zig`, `job_park_model.zig` | M | §1.3, §1.7 | 1, 2 |
| 4 | `Z/jobs/coordinator.zig` (Linux: the manager predicate, the mount hold, worker fibers on meta lanes, `job_copy_buffers` component, `noop`) | S | §1.1 | 3 |
| 5 | `Z/data/mover.zig` + `plane.republishBinding` + `probe.moverQuiescent` + `jobs/census.zig` | L | §2.5.2–§2.5.4 (drain, rebalance, retire) | 3, 4 |
| 6 | `Z/cli/volume.zig` `add-data/remove-data/undrain/list` (offline via `cli/offline.withSet`; online via the admin lane), the data-superblock probe | M | §2.2, §2.3, §2.6 | 5 |
| 7 | `Z/ipc/host/admin.zig` (the dispatcher: `job *`, `volume *`, `defrag *`, `migrate-meta-slot`) + `Z/cli/job.zig` | M | §1.10 | 4 |
| 8 | `Z/jobs/defrag.zig` (D1–D4 measure, report, bounded JSON, gauge worker) + `jobs/defrag_data.zig` (`plan_defrag`, `CompactLow`/`BackendAscending`) + `jobs/defrag_meta.zig` (nudge + merge lap over `leaf_merge.zig`) + `jobs/defrag_fold.zig` | M | §3.1–§3.4 | 5 |
| 9 | `Z/jobs/defrag_pack.zig` (`plan_pack_compaction`, `compact_one` over `Packer`) | M | §3.5 | 5, 8 |
| 10 | `Z/wire/verbs.zig` `Block.job` + `frame.Class.bulk`; `Z/jobs/wire.zig` (shards, leases, sweeper, proposals, verify-reads, quarantine, WERO on data namespaces, the worker loop `job worker`) + `job_shard_fencing_model.zig` | L | §1.6, §1.8 | 3, 4; rung 8's TLS rows for mTLS sampling |
| 11 | `Z/cli/volume.zig` `enable-symmetric`; `job rotate-enroll` | S | §2.6, §1.8 | 6 |
| 12 | Evidence note `docs/evidence/rung-08-jobs-lifecycle.md` + the 0002 rows of §6 + knob rows §4.1 + rails (every `job_*`/`evacuate_*`/`frag_*` literal a registry row, every row read) | S | | all |

Order: 1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 9 → 10 → 11 → 12. Fsck (C1–C17, the fleet read shards'
consumer `run_fleet`) is its own spec; items 3/4/10 expose `FleetDispatch{readCapacity,
dispatchReadShard, retireFleetShards, shardLeaseTtl}` (`J:1247-1297`) so it plugs in without touching
the fabric.

Gate rows per item: fmt + unit + rails on both venues; the `[L]` rows on the venue (`scripts/venue zig
build gate`); the kill matrix (drain mid-copy/mid-publish/mid-retire, defrag mid-pass, coordinator
kill mid-job → remount → the adopted job converges ×10) in `tests/smoke` with per-test pid files and
the external-kill skip law (0002 row 354); every measured number (the ½-raw-copy floor, the width-4
overlap, duty adherence ±10 % at 25/50/75) stated with instrument and substrate (nvmet-loop for the
throttle rows; nvmet-tcp for the drain throughput row).
