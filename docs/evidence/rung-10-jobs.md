# Rung 10 — the job fabric's three owed items (2026-10-12)

The charter's §8 fabric landed through rungs 8/8a/10; this note closes the three rows
`rung-08-operations.md` and `rung-08a1-fsck.md` left owed: `defrag --meta/--fold` on a live
mount, `fsck --online` through the admin lane with `--shards k/N` + `merge-reports`, and the
remote worker beyond `noop` (the fleet READ shard). Design calls: `0003` §R; reference
departures: `0002` rows 496–498. Venue: `sqz-zig` (Ubuntu 24.04, kernel 7.2.6-sqz, 9 vCPU /
16 GiB), the matrix rig of `src/cli/smoke.zig`.

## 1. `defrag --fold` / `defrag --meta` on a live mount (landed as `cbb51ff`)

**Design.** The D3 seam is the write side's own: `accumulate.Table.extentKeys` lists the inos
with PARKED W2 extents, `WritePath.foldParked` runs the EXISTING fold on every parked extent
block of one ino (in place at the page grain for a sole-owner passthrough block, the seeded
escalation otherwise), `plane.foldTargets/foldKick` expose both to the fabric's `PlaneFold`.
The D4 seam is the meta set's: `smo.compactForDefrag` (freeze → `smoReplace` with the FIFO
valve kept, admitted at `dead ≥ 500 ‰` over `leaf_merge.leafOccupancy`) and
`KvBackend.defragNudgeCompactions` / `defragMergeLap` under `smo_lock` (one §4.6a lap per tree
in tick-bounded calls, the lock dropped between them), exposed as `PlaneMeta`. Both bodies
run under the duty-cycle throttle and `job pause/resume/cancel`; a `kill -9` mid-job resumes
by plan regeneration at the next mount's adoption. The "spilled" half of D3 is 0 by
construction in this port (0003 §R row 1).

**Red first.** The D3 fold over the RAM write path (targets deduped ascending; 3 kicks = 3
in-place passes; device ÷ user = 8× ≤ the §J bound; bytes exact); the D4 nudge over
`TestVolume` (a mostly-dead leaf compacts into ONE frame, 0 dead after, a superseded node the
no-op); the device-backed D4 faces (nudge ≥ 1 then 0; a forest emptied by deletes merges and
converges).

**Venue** (`smoke: DEFRAG --fold AND --meta ON A LIVE MOUNT`): 12 parked extents across 3 open
files fold as 12 page DMAs — 49,152 B of device writes for 6,144 B of user data (×8),
`parked_extent_bytes` 0 after, every byte exact; 8,000 files cut to 800 and rewritten 4× →
`--meta --throttle 1` cancelled mid-run → paused → resumed → `kill -9` mid-job → the remount
adopts and completes by plan regeneration: 49 compactions, 8 merges, leaves 73 → 66; `fsck
--online` 0 findings.

## 2. `fsck --online` through the admin lane; the joined posture's census

**Design.** `admin_fsck` (0x0706, `AdminFsckReq{flags, throttle_pct, shard_k, shard_n}` with
`check()` at the codec; the generic `admin_job_submit` refuses `JobType.fsck` as it refuses
`.rekey`) mints a `fsck` fabric job whose record carries the option bits in `arg0` and the
shard word `N ‖ k` in `arg1` (`fsck.jobShardArg/jobShardOf`); the CLI's `fsck|scrub --online
[--shards k/N] <mountpoint>` rides it and polls the table to the terminal state, then reads
the report through `admin_fsck_report`. The engine's online face (`runner.linux.JobBody`)
REBINDS at every pass: `refreshLiveScope` classes every routing slot off tree 0 and the
appender pages — leased here (the LIVE lease table is the authority: a joiner's tree 0 is a
projection of the manager's pen), foreign-live (a lessee whose page is Live/Recovering),
unleased; every live foreign ring's window is scanned for the inos it names. An online
`--shards k/N` judges its residue exactly as the offline shard does; `merge-reports` over the
N reports ≡ the whole run's inode plane.

The two-daemon set taught the census four scoping laws it lacked (each red first over the RAM
fixture, then the venue): C4 scopes a staging custody naming an unjudged slot's ino or a
current-era ino (`c4_custody_scoped`; the staging dir is the data plane's, a joiner stages
through it too — 103 of B's inos read as orphans); C10 scopes a dentry whose CHILD sits in an
unjudged slot and gates the names its bitmap could not index (lessee scope, ONLINE the era
floor, then the record) — 50 of B's files under A's directory read as dangling; the OFFLINE
open classes the slots of an appender LIVE on the directory with its ring unrecovered as
foreign-live (the open refuses them until the recovery ladder runs); the coordinator's
reference-count arms (C3, the leaked arm, C8) take NO verdict while any slot is foreign-live
(`alloc_census_declined`) and a block a visible foreign record references is scoped from every
allocator arm (`block_plane_foreign_scoped`, `block_refs.Exemptions.isForeignRef`) — a live
lessee's slot trees leave the coordinator's forest at the grant with every record they carry,
so its derived counts are incomplete by construction (`refcount 64 vs 63` on packs the manager
itself promoted). C17's `missing_stripe` answers "present" for a stripe in an unjudged slot or
past the start watermark. Two pre-existing defects fell out of the row: `admin_job_list`'s
row count narrowed to u9 by `@min` against the 256-row cap (`6 × 96` overflowed on the sixth
online fsck of a mount — the daemon panicked in the wire thread) and `promotePacked`'s failed
arm abandoning a tx the backend had already consumed (`CommitOutcome.failed` now names the
tx's owner; the bus error at `0xaaaa…` on a joiner's dismount).

**Red rows.** `admin.zig`: "admin_fsck mints the online run …", "the job list carries a table
past five records …"; `fsck.zig`: "the online job's shard word"; `admin_frames.zig`: the
`AdminFsckReq` round trip / malformed rows and the payload fuzz; `fsck/tests.zig`: "C4/C5
through the staging face …" (the scoped custodies), "C10's cross-owner names …", "the fleet
member's residue is its custody …", "the block plane under a live lessee …", "sharding k/N
…", "forest scoping …"; `pack.zig`: the failed-commit row asserts `.failed == .commit` and
the fixture's seam consumes the tx.

**Venue** (`smoke: ONLINE FSCK THROUGH THE ADMIN LANE`, 40/40 smoke rows): the fill (dirs, 200
× 64 KiB packed files, hard links, three fsync'd 20 MiB files) → `fsck --online` whole: 208
inodes, 209 dentry refs, 15 blocks, 0 findings → `--shards 0/3, 1/3, 2/3` online: the three
residues sum to 208 inodes and `merge-reports` of the three ≡ the whole run (0 findings, the
census exact) → three online runs INSIDE an 8-way `cp` storm of 50 MiB files: 0 findings each
(shield engagement 4–12 exemptions over the runs) → A remounted (its dismount promotes every
resident), B JOINED creates 2,000 files and a 100-file tree under A's root, both members'
barriers → `scrub --online` from A over the live set: 215 blocks (76 MB) read, 0 failures,
0 findings, the fleet dispatched B's custody → `fsck --online` from A over the live set: 2,313
inodes, 2,210 dentry refs, foreign-dentry scoped 104, slots covered 65,472 of 65,536, the
reference-count arms declined (1), 0 findings on the quiesced set → on B's mount the lane
refuses by verdict (`not the coordinator`) → B's clean dismount promotes 0 residents with 0
failed, invariant tripwires 0 on both → the offline `fsck --scrub` after the row: 2,313
inodes, 19 blocks, 0 findings.

## 3. The remote worker beyond `noop` — the fleet READ shard

**Design.** A JOINED mount with `SQUEEZEFS_FLEET_JOBS` on runs ONE worker fiber
(`coordinator.Node.fleetMain`) enrolled `CAP_FLEET_READ` at the coordinator over its own
manager link (`joined.SharedLink`), re-enrolling at the heartbeat cadence while the block is
unserved and after a dead link. The coordinator's `FleetDispatch` QUEUES the residues (no
destinations — the lease's fencing arm alone), runs its own view online, then collects: a
read-capable pull leases a queued residue (`lease.ShardTable.assign`, 30 s TTL / 10 s heartbeat
/ fencing), the worker runs the engine over ITS binding — its plane's read path, its posture's
scope — with a concurrent heartbeat fiber (a non-live verdict or a dead wire raises the
engine's cancel; the walk ends at its next step and the residue is ABANDONED, never proposed),
and PROPOSES the report as `read_shard_result` under the lease's fencing; the coordinator
fencing-checks it (`complete`; a stale or unknown proposal is `Rejected`), parses it with the
ONE decoder, merges it and publishes the merged report once. On Plaintext the engine's reads
are verified 100 % (`verify 1000 ‰ (plaintext pins 1000)` — `SessionAuthn.confidential()`
through the existing ladder). The residue is a CUSTODY (0003 §R): the member's engine covers
the inos of the slots it leases (`Host.custody_only`) — the scrub and C1/C4 of its slots; the
name-dependent and allocator arms are the coordinator's. A lost residue counts
`job_fleet_shards_relocal`, and the coordinator re-runs its own view once.

**Red rows.** `wire.zig`: "a fleet READ shard end to end over the loopback …" (the RAM engine
proposes, the coordinator merges, `job_remote_shards`/`job_fleet_shards_*` close), "a fleet
READ shard whose worker dies mid-walk …" (the lease expires, fencing +1, the stale proposal
refused, relocal); `fsck/tests.zig`: "the fleet member's residue is its custody …".

**Venue** (`smoke: THE FLEET READ SHARD`): B enrolls at A (`job_remote_workers` 1) → 1 GiB of
fsync'd files, 4 × 128 MiB on each side → `scrub --online <A>`: A's view (128 blocks) + B's
custody (128 blocks read off B's own plane, 536,870,912 B; `job_fleet_worker_shards` 1 on B)
→ merged 256 blocks (1,073,741,824 B), 0 failures, 0 findings; A: dispatched 1 = completed 1,
relocal 0 → `scrub --online --throttle 5` again, `kill -9` B inside its residue (its shard
counter moved at the shard's start) → A: lease expiries 0 → 1, relocal 1, dispatched 2,
completed 1, remote workers 0, the job completes whole with exit 0 (0 findings), invariant
tripwires 0 → the offline `fsck --scrub` after the row: 256 blocks checked, 0 findings (B's
unrecovered slots scoped foreign-live by the offline open).

## 4. Owed (stated)

- **The block plane over a live set.** The coordinator's C3/leaked/C8 arms decline while a
  lessee is live; the complete law is the members' derived censuses merged at the coordinator
  (the custody report carrying per-block references). The inode plane's verdict over a
  joiner's cross-owner mints under churn (a C9 of a cross-owner-minted directory appeared on
  one unquiesced run and not on the quiesced one) is read for its scoping counters on the
  set stage; the storm row's 0-false-positive law holds on a single daemon.
- **C13's live-appender arm and C16 on a real set** stay as `rung-08a1-fsck.md` left them
  (0003 §R): C13 needs a member-side census verb (the lessee's own SMO serialization), C16 a
  product index home.
- **The staging marker's per-mount scope** (data plane): `openStaging` writes `node_token = 0`
  and binds with `peer_live = false`, so two mounts of one set on one host share one live ring
  (B recovered A's 200 residents and promoted them at ITS dismount through slots it did not
  hold: 197 refused, `pack_pin_refcount_drift`). The rig's two-daemon stage remounts A first
  and asserts B recovers nothing.
- **A joiner's clean unmount + rejoin while the manager stays up** refuses the mount with
  `corruption outside the replay window` (reproduced with no fsck in play: mount A, mount B,
  write on B, `umount` B, mount B again); the symmetric plane's owner's row. The fleet row
  kills B inside a throttled residue instead of remounting it.
- **A non-worker live member's blocks** are scanned by nobody while it is live (its custody
  is its own); every member with the fleet on is a worker.
