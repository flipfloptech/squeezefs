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

## 3a. The C9 on the two-daemon set (the row red on `dev`, 2/2) — a census scoping gap

**Root cause.** A's forest scope at fsck time read `64 leased, 65 foreign-live`: B's 64 rotor
slots and **slot 1**. Slot 1 was one of A's rotor slots before A's remount (the row's stage 4
remounts A so its dismount promotes every resident); A's dismount released it, B's join
leased it. A slot is the container: its tree carries every record ever minted in it — here
A's own `d` (ino 3), `a-dir`, `f`, `g` and the NAMES `d` holds (`d → e`). At the grant the
tree left A's forest (`forgetTree`), while `e`'s record (ino 4, slot 2) stayed A's: the
census saw `e` live and prior-era with no name in view → `[C9] ino 4 — named by no dentry`.
The filesystem itself is consistent (both mounts list `d/e`); the census's referenced set
is incomplete BY CONSTRUCTION whenever any slot is a live lessee's — a judged inode may be
named only from a directory in that slot. Class: **scoping gap** (the charter allows the
lessee to hold a slot carrying another member's records; the reverse difference needs every
name).

**Law.** `DentryPass.names_complete` = no slot foreign-live and no dentry scoped; the
name-dependent arms — C9's reverse difference and C10's count arms — take no verdict without
it (`inode_plane_names_declined`); C10's dangling arm over a judged parent and child stays.
Red first: `C9 and the C10 counts under a live lessee: …` (`RamSet.mkdirGuest`: a directory
in a lessee's slot naming a file whose record is slot 0's — C9 before the law, 0 findings
and `names_declined ≥ 1` after; the slot nobody's again → the name is in view and the file's
`nlink 2` against one name is C10's); `forest scoping …` re-pinned to the law. The row's
set stage asserts 0 findings on the UNQUIESCED set again (the members' `sync -f` is gone;
`names_declined == 1`); 10/10 venue runs green, 0 findings each (foreign-dentry scoped 10 or
104 by which slot B's leases land on, slots covered 65,472).

The row's strict law is back (`9c59671`'s tolerance of the coordinator's `fsck_findings`
tripwire reverted): both daemons' `invariant_tripwires` 0, 10/10 venue runs green with it.

**Also.** `data.accumulate`'s "two flushes of one parked block (a RELEASE racing an fsync)"
flaked once in a venue gate (`.discarded` for `.joined`): 200 runs of the row under 8-way
`yes` load passed 200/200 — no reproduction. By inspection the row's rendezvous was a
schedule assumption: the joiner registers on `upload_settled` (`Notified.enable`) BEFORE it
takes the lock and reads `upload_inflight`, so a hold released on `waiters() != 0` lets the
first upload publish and retire the entry ahead of the joiner's first look — `.discarded`,
the correct word for an entry gone (every caller treats `.discarded` and `.joined` alike).
The rendezvous is now the joiner's OBSERVATION of the in-flight upload
(`write_pipeline_join_waits`, counted before the wait; in `.stats`).

## 3b. The merged block census (the data-loss class `1f802d6` fixed; fsck must REPORT it)

**Why.** A joiner shipped a NONTERMINAL free (a pack seal's pin drop, one tenant's delete) to
the manager, whose untracked arm seeded one reference and freed the block under the joiner's
live tenants — and fsck read clean throughout: the coordinator cannot see a live lessee's
layouts, so no arm had the reference. The coordinator's declined arms (§2) were honest and
blind; the law is the members' censuses merged at the coordinator.

**Design.** A member's custody proposal carries its derived census — `Report.owners`:
`[[ino,[[vol_tag,block_idx,block_index],…]],…]` through the ONE decoder (the fuzz row and a
round-trip row; `merge` drops it — an input, never an output). The coordinator's local run
ships its own owners too; `classes.crossMemberBlockPlane` runs the LOST arm (a block a
member's layout binds that the holder tracks no more) and C3 (the holder's count against the
merged references) over every member-referenced block, with the holder's bitmap bit (a
lessee's fresh carve) and the in-flight registry as exemptions. The holder's bitmaps reach
the online binding at every pass (`BitmapFace` over the wire plane's holder plane — the
Linux binding had `bitmap = null`, the flat arm, since rung 8a). A suspect is CONFIRMED
against a SECOND census round from the members (`queueCustodies(base = shards)`; the ladder's
re-check with the member's own fresh evidence — a reference deleted between the rounds or a
block the holder tracks again is cleared and counted); a member silent between the rounds
leaves its suspects unconfirmed (cleared, counted, logged). The leaked arm and C8 stay the
whole-view run's (a leak is space; C8 compares durable refs the holder cannot read).

**Red first.** `the fleet's MERGED block census …` (RAM: the member's census binding a block
the holder tracks no more → C2 lost naming the member's ino; a tracked block at refcount 1
against two references → C3; the second census without the C3's reference clears it, the
lost stands). The venue row `smoke: THE FLEET'S MERGED BLOCK CENSUS` reverts `1f802d6`
behind `SQUEEZEFS_TEST_SHIP_EVERY_FREE` (B ships every release): B promotes 8 × 64 KiB
staged files into a pack by the fsync lever, `rm`s one tenant → A's
`block_free_file_punches` 0 → 1 → `fsck --online <A>` while B is live: **1 C2 lost**
(`vol-… block 1 ← ino 74766790688770 index 0 — lost: referenced but untracked by the
allocator`, the punched pack and B's first live tenant), 14 member bindings merged, 0
suspects cleared, exit 1 → B remounted (its RAM caches gone) reads its 7 remaining tenants
off the device: `seam/f1..f7: FAILED` (the loss, made visible) → the seam off: the same `rm`
stays local (punches unchanged), `fsck --online` 0 findings with 8 bindings merged, B
remounted reads its tenants sha-equal. Found on the way and left for its owner: the manager
reads a live joiner's PACKED tenants with EIO whether or not the block is intact (the row
reads the device truth on B after a remount instead).

## 4. Owed (stated)

- **The block plane over a live set.** The lost arm and C3 run over the merged census
  (§3b); the leaked arm and C8 still decline while a lessee is live (a carve between two
  censuses would read as a leak; the durable refs of a member's blocks live in its tree), and
  so do the name arms (§3a). The storm row's 0-false-positive law holds on a single daemon;
  the set stage holds 0 findings on the unquiesced set with the arms declined and counted.
- **A manager's read of a live joiner's packed tenant answers EIO** (intact or not): the
  data plane's owner's row (seen by the merged-census row).
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
