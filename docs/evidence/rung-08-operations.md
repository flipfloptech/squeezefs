# Rung 8 — operations: the job fabric in the daemon, the admin lane behind the live verbs, fsck/scrub, the data-volume lifecycle, defrag, nvmet, the zcrx lane

The consolidation of the rung-8 waves: A1 (`rung-08a1-fsck.md`), A2 (`rung-08a2-jobs-lifecycle-defrag.md`),
A3 (`rung-08a3-nvmet-fidelity.md`), B1 (`rung-08b1-zcrx-lane.md`, the other agent's) and THIS integration
pass. Charter §8 in full, §5.4 (`enable-symmetric`), §6.1 (promotion at dismount), §1.8/§1.10. Decision
0003 §G; decision 0002 rows 380–441 (this pass: 434–441). The wave notes stay as the per-wave record; this
note is the rung's verdict.

## What rung 8 landed (across the waves)

| Area | Modules | Verdict |
|---|---|---|
| fsck C1–C17 + the scrub (A1) | `src/ops/fsck/*`, `cli/fsck.zig` | ONE finding union; the zero-FP ladder; verify-before-repair; `--offline` through the writer door; **`--online <mountpoint>` now SUBMITS through the admin lane and prints the daemon's report (this pass)** |
| The job fabric, the lifecycle, defrag D1–D4 (A2) | `src/ops/{jobs,lifecycle,defrag}/*`, `cli/{job,volume,defrag}.zig` | the records, the throttle law, shard leases, the `0x06xx` block, the ONE mover, the lattice, the planners; **the coordinator runs in `mount`, every verb acts on the LIVE mount (this pass)** |
| nvmet over configfs, the initiator, the PR legs, the fidelity tier, keys (A3) | `src/ops/nvmet/*`, `ops/keys.zig` | kernel nvmet as THE target; the fidelity tier through the binary with zero mocks (`zig build nvmet-smoke`) |
| The zcrx read lane (B1) | `src/ops/zcrx/*` | cores + the userspace NVMe/TCP association; the probe-decided lane with a loud `Absent` posture on this venue |
| **This pass** | below | the dismount defect fixed; the coordinator in `mount`; the admin lane (`0x07xx`); the live verbs; online `add-data`; the kvmap and packed republish arms; the TCP `job worker`; the Linux jobs matrix |

## This pass

### 1. The dismount defect (rung-08a1's recorded defect) — cause and fix

Reproduced exactly at HEAD `a0ac3c3` on the venue: 20 × 64 KiB staged files + 3 × 20 MiB `dd conv=fsync`
striped files → `umount` → `thread panic: reached unreachable code` at `block_alloc.nextFreshBlock`'s
`assert(took) // virgin ⇒ clear`, via `staging.promoteAllAtDismount → promotePacked → pack.prepare →
allocateBlock`; the daemon exited 134 on a CLEAN unmount.

**Cause:** an ordering defect in `mount`'s teardown. The wire plane stopped (`WirePlane.stop →
HolderPlane.destroy → disarm`: `alloc.grant_source = null`) BEFORE `DataPlane.close` ran the promotion pass.
The holder's own grant window had carved 64 bits at the frontier for the striped writes (the cursor stood at
15); with the mint source gone, the pack allocator fell to the plain cursor CAS and met a SET bit the window
owned — the invariant "every bit at/above the cursor is clear" held only while the arm was live. A pure
staged fill never minted a window; a pure striped fill had nothing to promote: both halves were needed.

**Fix (commit `d5c780b`):** `DataPlane.dismount` is its own phase (sweeps stop, tasks drain, the write
pipeline quiesces, every staged file promotes, the packs seal DISMOUNT, the reclaimer drains), called by
`mount` before the wire plane stops; `close` runs it once if nobody did (the bring-up unwind). The assert is
gone: a SET bit at the cursor puts the cursor back, fires `block_claim_anomalies` with its site and refuses
`PlaneRefused` (retryable; never the `StorageFull` latch, never a walk past the holder's carve) — the staged
resident stays staged and the pass logs the failures at error. Red first: the `sym_planes` two-writer fixture
(carve → disarm → mint ⇒ typed refusal, cursor unmoved, population unchanged; re-armed, block 15 mints) and
the `FSCK AND SCRUB` smoke row refilled in ONE mount pinning `dismount promotion: 200 staged file(s) …
promoted, 0 failed` with no anomaly line. Verified by hand: the same sequence → exit 0, 23/23 files byte-exact
on the remount, `fsck --offline --scrub` clean.

### 2. The coordinator in `mount`; the admin lane

- Every writer mount runs a `coordinator.Node` after the data planes arm: the durable records adopted, the
  bodies registered over the plane (`noop`, `evacuate_volume`, `rebalance`, `defrag_data`, `defrag_pack`,
  `fsck`), the `job_copy_buffers` budget component, the local pool (`clamp(cpus/4, 2, 8)` workers), the
  shard-lease sweeper; admission opens once the grant plane armed; volume 0's manager is the coordinator
  (`is_coordinator`); the node stops BEFORE the dismount pass (a mover mid-step releases its claim at its
  pass boundary; the record stays non-terminal for the next adoption).
- **The admin lane** is the block `0x07xx` on the ONE listener (`admin_frames.zig`, `admin.zig`):
  `admin_job_list` (the live table, ≤ 256 × 96 B rows), `admin_job_control` (pause/resume/cancel/throttle →
  a typed verdict: `ok | unknown_job | terminal | not_coordinator | table_full | refused | io |
  no_report`), `admin_job_submit`, `admin_fsck_report` (the bounded JSON, ≤ 60 KiB), `admin_volume`
  (add-data / remove-data / undrain with the preflight's four terms in the reply). The CLI
  (`cli/admin_lane.zig`) resolves the mount through its `.stats` (`sqmeta_uri`, `wire_endpoint`), proves
  the storage-trust ladder with the `job:enroll` secret it reads through a read-only open of the set, dials
  under its own identity (`mount_slot = 0xFFFF_0000 | pid16`: never a member's lane index), and runs the
  verb's typed calls. The `.stats` inode stays read-only (charter §8).
- The live verbs: `job list/status/pause/resume/cancel/throttle <mountpoint>`, `fsck|scrub --online
  <mountpoint>` (submit → wait for the fabric's typed terminal state → read the report → print; `--repair`
  online refused by name), `defrag --data|--pack|--meta|--fold|--rebalance <mountpoint>` (one submission per
  arm), `volume add-data|remove-data|undrain <mountpoint>`.
- **Online `volume add-data`** (charter §8 "online data add"): the lattice's seven steps over the online
  world with the attach folded into step 7 — the device formatted and recorded, `DataPlane.attachVolume`
  (the row published to every lock-free reader by `Volumes.add`'s release store; the router row DISABLED),
  `WirePlane.armDataVolume` (the allocation lease on tree 0 + the holder's pre-sized grant-plane row), THEN
  `Active` and the rebalance SUBMITTED at 25 %. `Volumes.count` became an atomic (add/n release/acquire
  pair); `VolumeInfo.active` an atomic the lifecycle flips (`DataPlane.setVolumeState` is the ONE runtime
  flip; the router mirrors the DURABLE state at mount — a Draining volume read Active to a resumed drain
  before).
- The TCP `job worker sqmeta://…`: the coordinator's endpoint off the `endpoint:` record of appender 0, the
  secret off the same tree, ONE dialed session, `job_enroll`, the pull loop until SIGTERM; the listener's new
  departure hook (`Listener.bindDeparture`) expires a departed worker's shard leases
  (`Coordinator.sessionDeparted`).

### 3. Found and fixed on the way (each a 0002 row)

| Defect | Row |
|---|---|
| The online fsck's binding frozen at mount: watermarks and the volume list ⇒ C2/C8 false positives on a healthy live mount (`1 inode(s)` scanned of 6) | 438 |
| `republishBinding` read a packed tenant through the inline map (always null) ⇒ 5,110 stale-token no-ops over a 20-tenant drain; kvmap heads refused outright | 437 |
| The drain's convergence counted the reserved superblock chunk ⇒ 7,050 re-plans in 25 min and a 9.6 GB RSS; a superseded-only pass counted as progress ⇒ 17,240 re-plans in 2 min; a forever-deferring body never read the fabric's stop ⇒ the daemon could not be unmounted | 436, 439 |
| The mover's `unpin`/`releaseRef` seams were bare refcount decrements ⇒ `block_live_free_refusals` fired 41× on a drain (the pin's release IS the terminal free) | 436 |
| The router's fill census refreshed only at open | 440 |

### 4. The Linux jobs matrix (`smoke: THE JOBS MATRIX`, venue, debug build, 4 GiB sparse data volumes, ≈ 1.8 GiB written)

| Row | Measured |
|---|---|
| format 1 meta + 2 data (1 MiB blocks, a cache path) → fill: 20 packed 64 KiB, 3 × 20 MiB fsync'd striped, a 1.5 GiB block-map file → umount, `clone` (a shared block), remount | 17 inodes, 1,598 blocks |
| `volume add-data <mp> d2` ONLINE | `vol-…` live at config generation 2; the rebalance job completed; `backend_fill_spread` 165,852 ppm on the 21 %/21 %/0 % shape (every volume inside the 10 pp band of the 14 % mean; row 441) |
| `job list/status` live; `job throttle` on a terminal job | refused by verdict `terminal`, exit 1 |
| `volume remove-data <mp> d0` → `job throttle 25` → `job pause` (observed `paused`) → `resume` → `throttle 100` → retire | 895 blocks moved (packed, striped, shared and kvmap bindings), `volume_retired 1`; `df` used 1,636,352 → 1,636,352 KiB (the baseline law exact) |
| `undrain` after retire | refused (exit 1, the lattice's reason) |
| `defrag --pack <mp>` after unlinking 10 of 20 packed files | 2 pack compactions; the kept files byte-exact |
| `remove-data d1` at 25 % → `kill -9` mid-drain → remount | `1 job(s) adopted`; the job resumed from its regenerated plan and completed; `volume_retired 1` |
| `fsck --online <mp>` | 17 inodes, 1,598 blocks, 0 findings |
| every file after two drains and a compaction | `big`, `big0..2`, the clone and the kept packed files byte-exact (sha256) |
| a second process `job worker sqmeta://…` | enrolled over TCP (`job_remote_enrollments 1`, `job_remote_workers 1`), a submitted job completed, SIGTERM → exit 0, `job_remote_workers 0` |
| `fsck --offline --scrub` after the clean umount | 0 findings (1,598 blocks) |

Also by hand on the venue: the first drain shape at the smoke's 2 GiB volumes was refused by the capacity
closed form (`avail 1.13 GiB < needed + transient 128 MiB + headroom 1 GiB`) and parked `PausedCapacity`
exactly as §8 says — the matrix's 4 GiB sparse volumes are the venue's answer to the 1 GiB headroom floor.

## Gates

- macOS (portable modules): `zig build gate` 22/22 steps, 1,934/1,958 tests passed (24 skipped).
- Venue (7.2.6-sqz): `scripts/venue zig build gate` — see the totals in the pass's report; the jobs matrix
  row runs inside `zig build smoke`.

## Deviations / not delivered (stated, not hidden)

- **`volume enable-symmetric`** is NOT built (the verb is a usage refusal). What it needs is now exact: a
  heap extent claimed for the appender directory link (the format claims extent 0 before any tree; a flat
  volume's extent 0 holds a tree, so the link lives at any free extent the superblock names), the fixed
  ring's first 16 KiB re-based under appender 0's four page slots on an EMPTY replay window (the ledger's
  tail = head after a clean unmount), the superblock's `symmetric_plane` bit + `appender_dir` stamped by one
  A/B write, then the ordinary symmetric mount. Deferred to the next operations pass with this row.
- **The daemon-owned nvmet data-plane connect at `mount` + `disconnect --identity-only` at clean `umount`**
  (A3's owed row) and **key rotation as a defrag-class mover** (`defrag --rekey`): not wired. The connect
  needs the set config's `FabricEndpoint` records bound into `mount`'s device discovery (the ladder's
  `daemon_connect` arm is decided and tested pure); the rotation needs a mover variant that re-encrypts
  through the transform with the config's `{scheme, salt, key_id}` updated LAST. Both deferred with this row.
- `defrag --meta` / `--fold` submit on a live mount and fail typed (`body`): the D3/D4 plane seams
  (`FoldSeam`: the write side's parked/spilled inos; `MetaSeam`: the checkpoint's compaction nudge and the
  merge-sweep lap) are not exposed by the plane/backend yet; the bodies are portable and tested over RAM.
- A JOINER learns an online-added volume at its next mount (the manager's `armDataVolume` is the only arm);
  the config generation bump is not pushed over the wire.
- The remote worker runs the wire-executable class (`noop` whole jobs); a scrub's READ shard rides the fsck
  engine's fleet runner, which is not bound on the worker — the matrix proves the enrollment, the pull loop,
  the heartbeat clock and the departure, not a sharded scrub.
- `mTLS` on the wire: not written (the session key is bound to a channel exporter the TLS arm fills).
- The `df` baseline law was exact in every hand run and in the passing matrix; one matrix run read `0` used
  immediately after the retire (the row now polls for the reclaimer's park bound and dumps the holder rows
  on a miss).
