# Rung 10 — syncfs, the quarantine's release and timing, the holder's bit-takes, the tick commit ledger, the nvmet ports (2026-10-13)

The items `rung-10-concurrency-defects.md` "Found on the way" and `rung-09-gates.md` §11 left
open. Decision 0003 §Y.1–Y.6; decision 0002 rows 514–516. Loops ran on the venue (9 vCPU, the
handover agent's loops alongside, load 6–10) or on macOS where the row is portable; each red was
run on the red commit before its fix.

## 1. `sync -f` is a durability point for the daemon's write custody

**Root cause** (two layers). The daemon served no FUSE_SYNCFS (the codec decoded opcode 50 as
unsupported: ENOSYS). Underneath, the venue kernel never sends it to a plain `/dev/fuse` server:
7.2.6's `fuse_sync_fs` returns 0 unless `fc->sync_fs`, which only virtiofs and fuseblk set (the
kernel offers INIT minor 7.45; 1,244 INIT lines on `/build/tmp` all read `7.45 → 7.36`). Upstream's
opt-in — `FUSE_HAS_SYNCFS` (uapi 7.47, bit 44; Jimmy Zuber's v3 of 2026-08-20, applied to fuse
for-next) — is offered only to a `/dev/fuse` opener with CAP_SYS_ADMIN in the initial user
namespace (a server can stall `sync`). So `sync -f <mnt>` reached neither the kernel's forwarding
nor the daemon: RAM custody (the accumulation table's parked blocks, the overlays, the open
rewrite epochs, the parked inode stamps) drained only at fsync/fsyncdir/RELEASE.

**Fix** (0003 §Y.1, 0002 row 514).
- Daemon: INIT echoes `FUSE_HAS_SYNCFS` iff offered (`InitReply.syncfs`); FUSE_SYNCFS decodes
  (`fuse_syncfs_in`) and dispatches to a `syncfs` vtable op (ENOSYS default). `KvFs.syncfs`: every
  parked stamp journaled, the plane's mount-wide drain (`DataPlane.syncfs` → `fsync.syncfs`: a
  census of every ino holding custody — the table's parked keys by key, the overlay's open records,
  the epochs open or saving, room reserved outside every spin lock — each overlay drained with its
  barrier, each parked key uploaded with the ENOSPC ladder once, the staged promote by its lever,
  ONE barrier over every volatile namespace with the stripe words cleared after it, every census
  ino's epoch saved), then the store-wide meta barrier. `fuse_syncfs` counts it,
  `fuse_syncfs_phase_ns` is its exact-sum family, `fuse_syncfs_armed` (0/1, and the INIT log line)
  says whether the kernel forwards syncfs at all.
- Kernel: the upstream patch backported as `scripts/kernel/patches-7.2/0027-sqz-fuse-allow-
  FUSE_SYNCFS-for-privileged-userspace-servers.patch` (upstream's bit 44; the in-tree minor stays
  45 — the flag is not minor-gated); `scripts/kernel/build-venue-kernel` rebuilds the venue kernel
  from the reference recipe (squeezefs@44752ce5) plus this repo's deltas (the kTLS/CE fragment rows,
  the patch). Built: `Image-7.2.6-sqz` sha256 `d42a24dd719c94c7…` (config sha `f6dfc7317983…`,
  identical to the running kernel's), ~8 min at 4 CPUs.

**Red → green.**
- `fuse.handler.lifecycle`: `FUSE_SYNCFS (opcode 50) decodes and reaches the filesystem's syncfs
  once …` — red `expected null, found .NOSYS`.
- `fuse.codec`: `INIT reply asks for FUSE_SYNCFS (FUSE_HAS_SYNCFS, flags2 bit 12) iff the kernel
  offered it …` — red on the echo.
- `smoke: \`sync -f\` IS A DURABILITY POINT FOR THE DAEMON'S WRITE CUSTODY` (a holder keeps `f` and
  `g` open so no RELEASE drains them; 1 MiB + 1000 B into the fresh `f`, 8 KiB over the fsync'd
  `g`; `sync -f`; kill -9; fsck --offline clean; remount): red on the venue kernel with the old
  daemon — `f-LOST`, `g-whole`, g's mtime `…11:42:15.683` vs the holder's `…11:42:15.723` (the
  parked stamp lost); red on the PATCHED kernel with the old daemon (`f-LOST`: it never echoes the
  flag). Green on the patched kernel with the new daemon, in a one-off `container run -k <Image>`
  (4 CPU, 4 GiB, the repo mounted, never the shared venue): **20 of 20** (`fuse_syncfs 1, custody
  inos 1, parked blocks flushed 1`; `f-whole`, `g-whole`, the mtime the holder saw). On the venue's
  running kernel the row is the ledger's `no_kernel_feature` skip (3 of 3 build-runs; the release
  gate refuses it until the venue boots the patched Image).
- New: `fsync.zig` `syncfs is fsync's data ladder for EVERY ino holding custody …` (three keys on
  two inos and an overlay record, one barrier after the last write, the stripe words cleared, a
  crash reads every byte off the map, exact-sum), `kv_fs.zig` `FUSE_SYNCFS is a per-file fsync for
  the whole mount …` (the stamps journaled before the plane's drain, the store barrier after it).

**Owed (the orchestrator's):** the venue still runs Image `36a9604b…`. Swapping it is a venue
recreate (`container system kernel set --binary <Image> --force`, `container rm -f sqz-zig &&
scripts/venue up`), which kills every agent's processes — not done here.

## 2. The jobs sink's quarantine release finishes its cohort

**Root cause.** `AllocSink.releaseFn` called `releaseQuarantine(epoch, &.{}, &out)` and dropped
`out` (every OWED publish moved to `limbo`, never `finishFree`d) and named no live admission: a
shard's destinations are pre-allocated UNPUBLISHED mints — live when the lease expires — so the
allocator flags them alone and keeps no epoch; the release could not find them and the flag (and
`quarantine_count`) never fell. The reference leaves expired destinations for "job end / next mount
tree-walk" (0002 row 515). Latent in the product today: `assign` hands every remote shard empty
destinations; the sink's path is the one the pre-allocated-unpublished-destination law takes.

**Fix** (0003 §Y.2). `custody.DestinationCohort`: `admit` quarantines a destination and
relinquishes a minted one (its terminal free deferred, OWED), keeping the live admissions;
`release` (under `DrainProof.preemptLanded`) names them to every volume's `releaseQuarantine` and
`custody.releaseCohort` finishes every owed publish, draining `out` in passes. The coordinator's
`AllocSink` delegates to it (it reads the plane's volume table — the seam a fixture reaches).
`custody_cohort_frees` counts the blocks handed back.

**Red → green.** `ops.jobs.coordinator` (Linux): `the quarantine sink returns an expired lease's
cohort to the free list at the drain proof …` — red: the minted destinations still quarantined
after the release. New portable row: `custody.zig` `the destination cohort hands an expired
lease's destinations back at the drain proof …` (an unreleased cohort stays held; a cohort larger
than one `out` drains whole). The allocator-level reading of fsck's leaked arm: every admitted
block ends `free`, its bit clear, `quarantine_count` 0.

## 3. A quarantine admitted after a finish passed its gate is taken back

**Root cause.** `finishFree` read the flag, then published (state → bit → credit); an admission
between them read `limbo`, took the flag alone, and the publication listed a flagged block —
reallocatable under a dead epoch, the flag outliving the cohort. The grace ring's late publication
had the same window.

**Fix** (0003 §Y.3). A Dekker pair: `publishFreeList` stores the state seq_cst and, after its bit
and credit, RE-CHECKS the flag seq_cst; `quarantineBlock` sets the flag (seq_cst RMW) before its
seq_cst state read. A re-check that finds the flag takes the block back (entitlement, then bit) and
leaves it `limbo` OWED (`block_quarantine_takebacks`). Every OWED mark — the gate's and the
take-back's — is a CAS that requires the flag, against the release's one `fetchAnd` of both; a
take-back that finds the cohort released publishes the block again.

**Model** (`free_list_model.zig` model 3, `a_quarantine_racing_a_finish_never_leaves_a_flagged_
block_on_the_free_list`): the finish, the admission and a claimant — and, in the second variant,
the cohort's release after the admission. Holds (both variants). Weakenings, each proven to fail:
`no_recheck` (the shipped finish — FinalViolated; the hand schedule `{P, Q, Q, P, P, P, C}` ends
`FlaggedBlockOnFreeList`), `recheck_before_state` (FinalViolated), `owe_unconditional` (the
take-back's plain OWED after the release — `StuckOwed`), `gate_two_steps` (the shipped gate's read
then mark — FinalViolated).

**Red → green.** `data.block_alloc`: `a quarantine admitted between a finish's quarantine gate and
its publication leaves no flagged block on the free list …` — `test_finish_station` (null in the
product) admits inside the window; red: the bit clear (free-listed, flagged).

## 4. Every non-carve take on an armed holder runs under its ordering lock

**Root cause.** The carve scans the lowest clear run and asserts every take of it under the
holder's lock (`DataBitmap.mutations`); `claimFreeForTrim` and `quarantineBlock` took bits (and won
entitlements) without it — a take between the carve's scan and its take trips the assert; an
entitlement won ahead of the carve's debit underflows the budget; the trim's return cleared its bit
outside the lock (the carve meets a clear bit before its credit); a quarantine's take moved no
population (the drift check's tripwire).

**Fix** (0003 §Y.4). `FinishFreeSink` carries the holder's `takeOutOfList` and `returnTrim`
(`DataBitmap`): the trim claim, the quarantine's free and virgin arms and the re-check's take-back
run their whole take (state, entitlement, bit) inside the lock; a quarantine's take (and a
take-back) moves the population and queues a SET delta; the trim's return clears and credits
inside it. Unarmed, the same bodies run in place (`holderTakeOutOfList`, `holderReturnTrim`).

**Model** (`free_list_model.zig` model 4, `a_holders_carve_never_meets_a_bit_taken_between_its_
scan_and_its_take`): the carve (lock; scan; take with its assert; debit if `free`) against a
non-carve taker (lock{state, entitlement, bit} → trim → return under the lock). Holds. Weakenings:
`take_outside_lock` (the shipped trim and quarantine — `CarveTookASetBit`), `return_outside_lock`
(an underflow).

**Red → green.** `data.sym_planes.data_bitmap`: `a trim claim and a quarantine on an armed holder
take their bit under the holder's ordering lock …` — `test_carve_station` holds the carve between
its scan and its take, a second thread's trim claim either completes (the defect: the row hands the
bit back so the carve stays whole) or announces it waits for the lock (`test_contended_station`);
the quarantine half reads the population drift. Red **100 of 100** on the red commit, green **300
of 300** (macOS); green on Linux.

## 5. The tick's commit ledger splits

**Root cause.** `transport_park_tick_commit_rescues += pass_msgs` counted every message the pass
after a backstop tick pumped with no wake CQE reaped — including a reply published between the
post-tick reap and the pump, whose producer armed and wrote the wake while its READ completion was
still deferred task work (late, never lost) — and it sat on the smoke's must-stay-0 list.

**Fix** (0003 §Y.5, 0002 row 516), as `bab8784`/`e44bf2e` split the CQE half. Every arm-winning
`Wake.wake` counts its eventfd write after the write returns (`Parked.written`, seq_cst);
`consumeWake` adds each reaped READ's count (`consumed`). The worker reads `wakeOwed` (written >
consumed, the READ standing) before its park; a backstop tick on a `DEFER_TASKRUN` ring whose park
was owed fires the tripwire `transport_park_tick_commit_lost_wakes` — the count sat in the eventfd
(the standing READ completes inside the wait) or in an unreaped READ CQE (the CQ not empty), so the
wait could not time out: exact. Every other post-tick pumped message is the plain
`transport_park_tick_late_commits`. The old counter and its WARN are gone; `brk.py`
`TICK_RESCUES`/`TICK_LATE`, `bench.zig` `tick_rescue_keys`/`tick_late_keys`, the rails' bracket pin
(TICK_LATE now two names), the smoke's must-stay-0 list and teardown line, `mount.zig`'s teardown
line and the porting spec follow.

**Red → green.** `fuse.transport.session`: `e2e: a reply published after a backstop tick's reap and
before the next pass's pump …` — `Shared.tick_await_publish_remaining` (null in the product) holds
the pass after a tick before its pump until a producer arms the coalescer. Red **20 of 20**
(`expected 0, found 1`); green **50 of 50** (late +1, both tripwires 0). New: `ring_park.zig`
`wakeOwed: a completed arm-winning write no reaped READ consumed is owed …`.

## 6. Every nvmet fidelity tcp listener takes a port of its run's own

**Root cause.** The legs' fixed TCP service ports (4433, 4434, 4440, 4450, 4460, 4461, 4470, 4480,
4490); the port-id base (54000) already probes past another run's ports (`allocatePortId`). Red: the
pre-fix nvmet-smoke binary run twice at once on the venue — A **3 failed**, B **2 failed**, every
one a share whose port link was refused (`PortLinkRefused`: nvmet-tcp's bind met the other run's
listener); the failed shares left 4 subsystems and 4 port shells in configfs (removed by hand, mine
alone: `fideli-1165743-*`, `fideli-1165744-*`).

**Fix** (0003 §Y.6). `Rig.tcpPort`: `bind(127.0.0.1:0)` + `getsockname`, the probe socket held bound
(SO_REUSEADDR, never listening) until the rig ends — no concurrent `bind(0)` is handed it, and
nvmet-tcp binds beside it (`nvmet_tcp_add_port` sets SO_REUSEADDR). The adopt row's hand-built port
id is `smallPortId` (pid-keyed below both ranges, probing past held ids). With the ports derived the
two runs still met — 3 rounds, 1–2 legs each: the daemon-connect leg's whole-host
`/sys/class/nvme` census, the loop port's last-out (the host's ONE loop port carries every run's
loop shares), the adopt row's first `foreign_live` entry and host-wide share/port counts, and the
hand-built namespace's fixed uuid (the host's nvme core ignores a namespace whose uuid another
subsystem shows: "no namespace appeared within admin_timeout"). Each scoped to the run
(`Rig.ownsNqn`, `expectPortLastOut`, `foreignLiveNames`; the pid in the hand uuid's last four
bytes). Green: two concurrent runs, **3 of 3 rounds both exit 0**, zero residue after; standalone
53/53.

## Commits

| item | red | fix |
|---|---|---|
| 1 FUSE_SYNCFS | `204b14b` | `f9945c5` (+ `scripts/kernel/`) |
| 2 jobs cohort release | `a4fb0fd` | `32f4aa2` |
| 3 the finish's re-check | `a4fb0fd` (+ model 3) | `32f4aa2` |
| 4 the holder's lock | `a4fb0fd` (+ model 4) | `32f4aa2` |
| 5 the commit ledger | `609a31b` | `cbc2005` |
| 6 nvmet ports | — (the concurrent pair above is the red) | `48a8dc2`, `a9fa84e` |

## Gates

- **macOS** (`zig build gate` on `7e8582d`, the landed tree): **28/28 steps; 2,195 of 2,222
  passed, 27 skipped** (unit 2,071 / 27 skip, rails 16). On `f9945c5` alone: 28/28, 2,190/2,217.
- **Venue gate 1** (`f9945c5`, item 1 alone; load 8–10, the handover agent's loops alongside):
  unit 2,538 pass / 69 skip / **1 fail** — the fsck zero-false-positive storm's `ops > 20` (its two
  writer threads starved under the load); smoke 80 / 3 skip / **6 fail** — daemons ended by a
  SIGKILL the rows never sent, a work dir removed under a running row (`readLog` NOENT), a set left
  mounted by the previous row. None touches the change; every one of those rows passed in gate 2.
- **Venue gate 2** (`48a8dc2`, items 1–6 before the fidelity scoping; load 6–9): 33/46 steps; unit
  2,545 pass / 69 skip / **1 fail, 1 crash** — `sync.mpmc` "a pool free list at its population …"
  (asserts a race occurs: `refused > 0`; on pristine `dev` **14 of 40** fail under an 8-way load) and
  `io.nvme_dev` "wake coalescing …" (an exact wake-write count missed under load, then its `deinit`
  joined a lane held by a seam the failed row never released — a 40-minute wedge, ended with a
  SIGSEGV for its stack; on pristine `dev` **4 of 60** miss under the same load, 2 of 60 here);
  smoke 85 pass / 3 skip / **1 fail** — `ONLINE FSCK THROUGH THE ADMIN LANE`, B's `cannot create
  x48: File exists` (the §10 handover class; on pristine `dev` under the same load **3 of 3** runs
  fail: a `[C9]` prior-era orphan, a `DeviceRefused`, an assertion at :7585). The smoke failure held
  bench-, shim-, nvmet- and suites-smoke back; run on the same tree with the smoke rows filtered out
  (`zig build suites-smoke -Dtest-filter=…`): **22/22 steps** — bench-smoke 41, shim-smoke 40,
  nvmet-smoke 53, smoke's namespace 39, suites-smoke clean.
- **Venue gate 3** (`a9fa84e`; load ~8): 35/46 steps; **2,650 of 2,724 passed, 72 skipped, 2
  failed** — unit 2,547 pass / 69 skip (clean), rails 16, smoke 84 pass / 3 skip / **2 fail**:
  `THE ONLINE VOLUME ROW ACROSS DAEMONS` (after its add-data half passed) and `THE KILL ROWS (a)`,
  both the rig's own spawn of `sh -c 'cat <mnt>/.stats'` (cwd: the run's work dir) failing in
  `processSpawnPosix` before any daemon op. Both rows alone on the same tree at load 1–2: **3 of
  3** runs green.
- **Venue gate 4** (this branch rebased onto the handover branch's unlanded `f3e303d` — 0002 rows
  renumbered 515–517 there — the merge of both): **46/46 steps; 2,802 of 2,874 passed, 72
  skipped** (unit 2,563 / 69 skip, rails 16, shim-census 3, smoke 86 / 3 skip, bench-smoke 41,
  shim-smoke 40, nvmet-smoke 53); macOS on the same tree 28/28, 2,189 of 2,216, 27 skipped.
- **Venue gate 5** (`7e8582d`, the handover agent's 40-run smoke loop alongside, load ~10): smoke
  77 pass / 3 skip / **9 fail** — `readLog` NOENT, "the volume is MOUNTED", `DeviceRefused`,
  `ScriptFailed` across nine rows. The cause: that loop (`.wt-handover/.diag/loop.sh`, diag-only)
  `rm -rf`s (or moves) EVERY `/build/tmp/smoke-*` directory that appeared during each of its
  iterations — this gate's smoke work dir with it (gone after the run). The same sweep explains
  gate 1's `readLog` NOENT and fits gate 3's spawn failures (the child's `chdir` into the removed
  work dir; the loop's `fsck40f` run overlapped that gate until 14:26). The nine rows alone (smoke
  filter), each run in a private mount namespace whose `/build/tmp` is its own directory: this
  tree **50 of 50** (load 3–4); pristine `dev` **49 of 50** — `ONLINE FSCK THROUGH THE ADMIN LANE`
  fails there (the handover class above).
- **Venue gate 6** (`7e8582d`, the landed tree, in a private `/build/tmp` namespace; load 1–2, the
  handover agent's gate alongside): **46/46 steps; 2,767 of 2,839 passed, 72 skipped** (unit 2,547
  / 69 skip, smoke 86 / 3 skip, bench-smoke 41, shim-smoke 40, nvmet-smoke 53; rails and
  shim-census cached from gate 5's run of the same tree: 16 and 3).

## Found on the way, stated, not changed

- **fsync flushes at most 64 parked blocks of one ino** (`fsync.flushParked`, and RELEASE's
  `WritePath.flushIno` and `foldParked`): `accumulate.Table.blocksOf(ino, &[64]u32)` truncates; a
  file with more parked partial blocks (the parked cap is 1,024 buffers) returns from fsync with the
  rest still in RAM. `syncfs` walks the table by key and is not bounded. Owner: the write path.
- **The nvme_dev wake-coalescing row wedges the unit step when it fails**: its expectations run
  while `hold_before_park` holds the lane; a miss returns before the release and `deinit`'s join
  waits forever. Owner: the NVMe lane.
- **`sync.mpmc`'s pool row asserts the race it describes happened** (`refused > 0`), which a loaded
  host does not guarantee (14 of 40 on pristine `dev`). Owner: sync.
- **A failed share's subsystem outlives the fidelity rig** (the `PortLinkRefused` residue above:
  `teardownByWalk` leaves a PENDING record's subsystem). Owner: the nvmet harness.
- **`quarantineBlock`'s virgin arm stores `quarantined` without re-checking a racing mint**, and its
  free arm's OWED mark after `quarantined` races a same-epoch release (benign: a dead writer mints
  nothing, and a release follows its admissions). Owner: the custody plane.
- **The smoke's work dir is a fixed `/build/tmp/smoke-<pid>` and a diag loop sweeps every new
  `smoke-*`** (gate 5 above): two agents' runs on one venue reach each other's dirs. Owner: the
  handover agent's diag loop (scope its sweep to the dirs its own binary made) and the smoke rig.
