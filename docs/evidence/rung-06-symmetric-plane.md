# Rung 6 — the symmetric metadata plane (waves A–E): the forest, appenders and the manager, slot leases, tokens, cross-owner intents and striping, the data planes, membership and recovery, the join ladder, the N-daemon posture (the manager half, then the JOINED backend and the two-daemon matrix), the default flip

Charter §5.5 in full, §5.7 (the D0 winner IS the manager; a later RW mount JOINS), §4 (teardown),
§8 (`clients`, `appenders`, `appender clear`, `umount`), §13 rung 6's order, §1.10. Decision 0002
rows 230–283; 0003 §E. The per-wave notes stay the detailed record: `rung-06a-appenders-manager.md`,
`rung-06b1-slot-leases.md`, `rung-06b2-tokens.md`, `rung-06c1-intents-ship-striping.md`,
`rung-06c2-membership-recovery.md`; the data planes landed in 776f2c5 without a note of their own
and are covered here. This note is the rung's closing summary: what stands, the models, the
N-daemon numbers, the deviations and the owed list the next rung inherits.

## What the rung delivered, in its order (charter §13)

| Step | Landed | Where |
|---|---|---|
| forest | one mixed-kind KV tree per slot; tree 0's binary control records (`slot_state` v2, `slot_tails`, `extent_grant`, `appender_hint`, `custody_quarantine`, `dir_rename`, `alloc_lease`, `dead_member`, `recovered`, `node_seq_incarnations`, `member`, `endpoint`), fuzzed total + canonical | `meta/sym/control_records.zig` |
| appender region | the 296 B A/B page over four slots under the directory-first law, `SLOT_PAGE_BUDGET = 108` by comptime tie, the directory chain, the region core over a segmented ring, the seq-offset law (model), the incarnation mint | `appender_page`, `appender_dir`, `region` |
| manager lease + grants | the lease, the 8 region verbs + the slot verbs through ONE `HandlerTable`, every word screened before any effect, the structural door bounded by `COVER_CYCLES_MAX`, grants as a POOL (`granted ≡ held + unclaimed + returned`, model) | `manager`, `extent_grant`, `join`, `bind` |
| slot leases + door + handover | the dense lease table, the ONE commit door (model), acquisition with parent affinity under `A_max`, holder-decided handover with `N_floor`, the page-budget overflow law, slot → holder resolution | `slots/*` |
| tokens | GPFS-strict: the holder's grant table with the register-then-check gate (3 models), the recall at A1½, the reader's cache with DMA-drain acks (2 models), the divert, `-o ro` + the epoch poll | `tokens/*` |
| cross-owner intents | ONE intent per POSIX op across holders, the ONE applier under the witness, travelling guards, the `dir_rename` lease, record-level ship, directory striping (K = 64), the one-intent `rmdir` | `xv/*` |
| shared refs + pack scope, allocation lease + bitmap + grants, custody | the data planes: `alloc_lease`, `data_bitmap` (KVDA pages), `block_grant` (`G = clamp(2 × rate × T_renewal, 64, cap/(2 × writers))`), `free_ship` + `free_grace`, `shared_index` + the clone protocol (model), custody (2 models), the wire handlers | `data/sym_planes/*` |
| dead-appender recovery | the §5.9 procedure as ONE driver over a `Volume` seam, the RAII rollback, 13 crash windows re-run to the converged state, `appender clear` | `membership/recovery.zig` |
| join ladder | rungs 1–7, the manager's wire face, the joiner's wire with the redial flag, `Joiner` (rungs 5–7), the same-node successor rule, the flush-ceiling audit; WERO fencing with registrants and the cap learner; the frame screen's inputs | `membership/join_ladder.zig`, `fencing.zig` |
| N-daemon posture (this wave: the MANAGER half) | `mount` arms the cluster wire: ONE listener on `SQUEEZEFS_MW_BIND` serving the manager set (dispatch by the request's volume), the membership shard, the death ledger's verbs, the token holder (its A1½ hook armed on every volume's pass), the cross-owner step service and the `dir_rename` lease; the endpoint published into the RAM table AND the durable `endpoint:{0}` record; the owner shard armed at the manager's term with the predecessor's roster; the cadence runs eviction + grace reap + the ledger poll; the read divert bound into the filesystem with the session's notify seam; `umount` signals the daemon; `clients` lists the durable member rows offline | `meta/sym/wire_plane.zig`, `cli/{mount,umount,clients,status}.zig` |
| record-level ship | `setattr/setxattr/removexattr/publish_layout` as ONE verb each, served under the holder's door, the holder's caches + kernel view invalidated | `xv/record_ship.zig` |
| default flip | `format` stamps `SYMMETRIC_PLANE` by default, `--single-writer` the flat opt-out; `SQUEEZEFS_SYMMETRIC_META` defaults 1, `=0` on a stamped volume refuses the writable open; EVERY symmetric family reads 0/`solo` on a `--single-writer` mount (the smoke walks `meta.sym`, the per-volume `sym` words and `data.sym_planes` after a full POSIX pass) | `sym/bind.zig`, `cli/smoke.zig` |

## Models (explorer, default limits; every weakening proven to fail)

| Model | Holds | Weakening that fails | File |
|---|---|---|---|
| `SeqOffsetModel` | a stamp reserved after a raise exceeds the floor; stamp order = reservation order | the offset read outside the registry lock (`FinalViolated`) | `sym/region.zig` |
| `ClaimReturnModel` | never claimed-and-returned under the grant lock | the lockless return (`InvariantViolated`) | `sym/extent_grant.zig` |
| slot door (`Model`) — committer `inflight += 1; fence; load state`, closer `state = releasing; fence; load inflight` | a passed commit never escapes the drain; the belt's apply reads its verdict under the lock | either fence removed; check-then-lock; the `committers = 1` rung-5 gate with either fence removed (4 weakenings) | `sym/slots/slot_door_model.zig` |
| `FirstTouch` (grant racing the pass) | recalled ∨ parked | the grant's two steps swapped | `sym/tokens/grant_gate_model.zig` |
| `TwoReaders` | recalled + parked ≥ decided-in-window | the same swap | — |
| `TwoUsers` (overlapping passes) | a grant inside user 2's window parks | refcount → set | — |
| `ServeDrain` (TSO) | no pre-step serve escapes the drain | the serve's seq_cst re-read dropped | `sym/tokens/serve_stamp_model.zig` |
| `InstallUnderGen` | pre-commit pages never installed after the removal | remove-then-bump | — |
| `SharedRefModel` (the W1 clone/patch fence pair) | a patch never lands on a block being shared | the patcher's fence removed; both cloner fences removed (either alone holds) | `data/sym_planes/clone_protocol.zig` |
| `CustodyUseModel` | a DMA use never outlives a revoke | the revoke's fence or the use's fence or both removed | `data/sym_planes/custody.zig` |
| `HandoverMarkModel` | the handover mark is seen by the use it must stop | the mark stored after the check (`FinalViolated`) | — |
| park gate `DekkerModel` | `tryEnter` / `park` exclusive | either fence removed | `sym/membership/park_core_model.zig` |
| `ExclusiveExitModel` | one exit of PARKED | a load-then-store exit | — |

## The N-daemon posture, the manager half — numbers (the venue: Ubuntu 24.04 / 7.2.6-sqz, 8 vCPU aarch64, debug build, file-backed 256 MiB meta + 2 × 2 GiB data, cache path; scoping only)

The smoke stage `THE N-DAEMON POSTURE, the manager half` drives the BUILT BINARY as daemon A and a
second process (the smoke) as the joiners B and C over real TCP on loopback:

| Row | Observed |
|---|---|
| `mount` A on the symmetric default | `ready` ≈ 1 s; `.stats` `mount.pid` = A's pid, `mount.wire_endpoint` = `127.0.0.1:<ephemeral>`; `manager_lease = held`, `wire_listener_up = 1`, `membership_shards = 1`; the durable `endpoint:{0}` record at the newest checkpoint decodes to the same endpoint (read offline by the joiner before it dials) |
| B joins over TCP (the secret read off tree 0) | `JoinAppender` → region 1 (512 KiB ring, 1 segment), `AcquireSlots(4)` → 4 rotor slots, a first touch of slot 60 000 → granted at `g = 1`, a first touch of slot 0 (A's native) → `SlotBusy{holder 0}`; membership `join` → granted, a durable `member:` row with B's pid |
| C joins the same way, then dies (connection dropped, no leave) | `appenders`: 3 pages Live (0 `mgr`, 1, 2); `clients`: A's writer claim `live`, B's member row `live` (the same-host pid proof), C's `stale` (another incarnation's pid) |
| B beats across 4 × `T_owner` (3 s by the seam; `T_self` 797 ms, renew 265 ms) | 15–16 renewals, every tick idle/renewed, the poison never fired |
| C evicted at `T_owner` by A's cadence | `membership_evictions = 1`, `membership_members = 1`; the ledger's writer recorded `dead_member:{C}` on volume 0 (`early = false`, epoch ≠ 0) — read offline at the next checkpoint |
| B's clean leave | every lease released with words flooring tree 0's record, the member row deleted, `LeaveAppender` → page 1 `free` (identity cleared, id/term kept) |
| closure | `manager_verbs` 14, `manager_verb_refusals` 0, `joined_wire_words_rejected` 0, `slot_lease_conflicts` 0, `invariant_tripwires` 0, every must-stay-0 gauge 0 |
| `umount` | the verb signals A (SIGTERM by the pid in `.stats`), A logs `stop requested (SIGTERM/SIGINT): unmounting`, its own `close()` detaches, exit 0; the log carries no `the FUSE session ended` (no external detach happened) |

The unit e2e of the same plane (`wire_plane.zig`, `scripts/venue test "wire plane"`) runs the
flow over a `MetaSet` in one process with a dialled TCP joiner, adds the renewal beat
(`idle` before the cadence, `renewed` at it), the holder's lease oracle reading the member
`live`, and the remount (a fresh ephemeral port re-publishes the record; the shard arms with an
empty roster after the clean leave). `MW_BIND=off` refuses a writable mount at rung 3 (counted),
arms nothing on a reader; a malformed bind refuses by name.

## Gate totals

- Linux venue (`scripts/venue zig build gate`, uncached): 18/18 steps; unit 1,812 pass / 14
  skipped, rails 5/5, smoke 38 pass / 1 skipped (the new N-daemon stage ≈ 25 s; the solo-posture
  walk rides the `--single-writer` stage), bench-smoke 28/28 — 1,883 / 1,898 in all;
  `dmesg | grep -c retained` 5 before and after; no `squeezefs` process and no fuse mount left.
- macOS (`zig build gate`): 18/18 steps, 1,596 pass / 16 skipped (the Linux-only modules skip).

## Deviations and findings (this wave)

- **One owner shard per daemon.** With ONE listener per daemon (0002 row 269) and the membership
  verbs carrying no volume, the shards of every volume this daemon manages fold into one
  `shard.Owner` at its home volume (0): a member homed on any of them renews at this listener.
  `membership_shards` reads 1. The manager family alone dispatches by volume (`ManagerSet`).
- **The endpoint's durable home** is a new control record (`Tag.endpoint = 12`, 32 B) rather than
  a field of the 88 B claim shape (0002 row 281).
- **`umount` signals the daemon** (0002 row 280); the external detach is the loud fallback.
- **Member rows and the pid proof** (0002 rows 282–283): a row without an incarnation word is
  proven by the pid's existence; `clients` lists the rows offline.
- **A `--single-writer` volume's `sym` words are PRESENT** (`manager_lease: "solo"`, every count 0,
  the unarmed plane's `n_floor` 0) so an operator keys on the same names on both classes.

## Not done — the owed list the next rung inherits (each stated where it stops)

- ~~**The JOINED backend (the N-daemon posture's joiner half).**~~ **Landed 2026-10-08** (see
  "Landed after this note — the joined backend" below): `KvBackend.openJoined`, the two-daemon
  matrix over real TCP; the kill rows' recovery half pinned by hand, the successor's cross-owner
  ops and the `-o ro` reader still owed (stated there). The paragraph that follows is the pre-wave
  statement, kept as the record of what the gap was. A second `squeezefs mount` of a
  symmetric set WAS refused by layer A (the flock). The spec's joined door
  (`sym_wire_membership_recovery.md` §5.2 — no flock, the fixed ring replayed as a projection, no
  claim, `JoinAppender` + `AcquireSlots` over the wire BEFORE `open_inner`, the JOINED checkpoint
  cycle: its slot trees' flushes into its grant, its page as its ledger, its tail, extent
  refills/returns over the wire, never the ledger/bitmap/tree 0/page 0) is a `KvBackend` posture
  this wave did not build: `core/reg/lane/smo_journal` over the region's ring, an extent
  allocator fed by `RegionGrant`, the trees opened at the acquire reply's words, the token divert
  dialling the holders, the xv plane's wire sessions, the data plane's grants pulled from the
  lease holder. Every component it composes exists and is tested over the wire in-process; the
  composition is the next wave's. Consequently the N-daemon MATRIX rows that need two FUSE
  mounts (B's 2,000 creates under its own slots, A's token reads of them, the cross-owner
  `tar -x`, the hot-dir handover/striping engagement, the 200 MiB `dd` on B's grants, the record
  ship with B's kernel view invalidated, the `-o ro` C reader within `reader_staleness_bound_ms`,
  `kill -9 B` mid-`tar` → recovery → successor remount, `kill -9 A` → B PARKS-AND-RECLAIMS → a
  new manager) were NOT run. What ran is the manager half above.
- ~~**The recovery driver over the real backend.**~~ **Landed 2026-10-08** (see "Landed after
  this note" below): `membership/volume_bind.zig` binds the 27-function `Volume` seam to a
  `KvBackend`; `wire_plane` installs one binding per managed volume as the ledger poll's hooks
  and the set-level roll-forward.
- ~~**The frame screen through the loaders — BOTH halves together.**~~ **Landed 2026-10-08**
  under the priming law (below): `node_cache.FrameSeam` ← `fencing.FramePlane`, installed at
  `slots/plane.Plane.init` — BEFORE the claim gate's ring-recovery preflight — every loader
  (`loadLeader`, `buildNode`/`loadFresh`, `smo.Source.load`, `KvTree.open`, the backend's tree
  walks) screens by slot and every frame writer (`appendFrozen`, `writeSuccessor`) stamps by slot.
- **The planes' device-path arms** (`LeaseArm`/`GrantSource`/`Shipper` into `data/plane.zig`,
  the Checkpointer's `PageWriteSet` for KVDA pages, the journal-lane drainer fiber, the free-ship
  and grace binding): the data plane still allocates from its ledger-seeded allocators; the token
  reader's `discardLayout`/`purgeCensus` answer `no_entry` (no foreign layout entry is ever held
  until the divert can dial).
- **The token divert's dialler** (a holder's reader plane needs the slot → endpoint resolution
  through the published `endpoint:` records and a session per holder), the holder's `Pusher`
  over real links (today `unreachable_`: a recall completes at the reader's lease expiry), the
  `bindDivert` of a foreign-slot read therefore answers the typed EAGAIN (never a stale local
  read) on the manager.
- **kv_fs over a FLIPPED directory** (lookup/create/readdir through the stripe map, the explicit
  `setfattr -n user.squeezefs.stripes` flip), the foreign-slot LAYOUT publish at the data
  plane's publish site, `SupplyStripeInos` over the wire, a foreign holder's wire session in
  `set_bind` (resolves `unreachable_` = EAGAIN).
- **`appender clear` end to end** on a dead joiner's region: the verb's two durable effects (the
  page `Recovering{term 0}`, the EARLY record) are exactly the states the real-backend kill matrix
  recovers (rows `page_recovering` and the early row below); the SMOKE leg over the built binary
  (`appender clear` → the next mount's poll) waits on the joined daemon's `mount B`.
- From earlier waves, unchanged: the paged-resident dentry set, the in-place join of a
  fail-stopped quiescent reader, `slot_tails` past the inline cap (a spilled set screens by the
  lease alone), the dead recoverer's stashed interior records, the un-share protocol; the data
  namespaces' registrants in the death preempt (the data planes' device arm); mTLS, the nvmet PR
  legs and fsck C13–C17 as full checks are rung 8's.

## Landed after this note (2026-10-08, wave E): the JOINED backend — a second mount as an equal writer over the wire; the two-daemon matrix

**The posture** (`meta/sym/joined.zig`, `backend.zig` `openJoined`, `meta_set.zig`; 0002 rows
286–292). `mount` on a `SYMMETRIC_PLANE` set runs volume 0's D0 ladder: the winner is the manager
(the half above); a loser whose volume carries the plane and whose manager is live (the durable
`endpoint:{0}` record, or `--join host:port`) opens EVERY volume through `openJoined` — no flock
(the claim IS `JoinAppender` + the member row), the device O_DIRECT read-write, the ring = the
joined region's segments on its own journal lane with the seq space from the reply's
`seq_offset`/`head_hint`, the node-seq incarnation from the reply, the trees opened from the
`AcquireSlots` words (`tree.openFromRoot`), the control tree a READ-ONLY projection the tokens
refresh (a joiner never writes tree 0), the checkpoint = PAGE-AS-LEDGER (`checkpoint_cycle` with
its ledger and KVDA sinks absent: the appender page carries roots/cursors/`ckpt_seq`/
`ledger_tail_seq` under the directory-first law, `reusable_upto` from its own page's durable tail,
the page-budget overflow through `PublishRoots`), the extent allocator a MIRROR of a wire-fed grant
pool (`ExtentGrant` refills at the quiet cadence, `ReturnExtents` of the surplus, `GrantExhausted`
= EAGAIN under the conveyor's pre-admission gate), the token client + `bindDivert` over ONE
`SharedLink` per peer (the joiner listens too: recalls arrive as pushed frames), the xv plane's
wire session, the commit door's first touch over the wire, the park gate armed at `T_self`, the
member heartbeat; `umount` = `LeaveAppender` + every lease released + page `Free`; a joiner's
`mount_slot` carries bit 31 over its appender id (the same-node successor re-learns the id).

**The data planes' device arms** (`data/sym_planes/plane_arm.zig`): the manager acquires the
allocation lease of every data volume on tree 0 (`alloc_lease.acquire`, term 1 first-come; its
own replay `already`; a dead predecessor's succession once recovered) and stands the HOLDER up —
the `DataBitmap` over the allocator's bits (a carve SETS at the lowest clear run, a shipped free
CLEARS), the grant `Ledger`, the `FreeExecutor`, `BlockGrant`/`ReturnBlocks`/`FreeBlocks` behind
handlers REGISTERED BEFORE the listener and armed by an acquire/release pointer pair (a verb before
the arm meets `Busy`, never a torn handler), the holder's own window over its own carves as its
mint source; a JOINER stands the WRITER up — a `GrantWindow` per volume as the allocator's ONE
mint source, the PULL inline at the mint below the 50 % mark (single-flight), the first pull at
the arm (an empty window IS below the mark; the reply's word is what `df` reads before a byte is
written), `free_ship.Shipper` as the reclaimer's free target flushed at the cadence, the
unconsumed runs RETURNED at the stop. `df` per the rule on both: the holder's word (`blocks`,
`population`, `frontier`) rides every `BlocksGranted` head (manager schema 2); the joiner projects
its acked `Freed` verdicts onto the word until the next reply. A grant is served to a LIVE member
of the home shard only (the owner's roster is the writer directory). The owner's death sink fans
out: the token holder sweeps the dead member's grants FIRST (`noteMemberDeparted`), then the
ledger records the death — a same-node successor under the same identity starts clean.

**The two-daemon matrix** (`cli/smoke.zig` stage `THE TWO-DAEMON MATRIX`: A = the built binary
as the manager, B = a SECOND `squeezefs mount` process JOINED over TCP loopback; the venue: Ubuntu
24.04 / 7.2.6-sqz, 8 vCPU aarch64, debug, file-backed 256 MiB meta + 2 × 2 GiB data, cache path,
`T_owner` 3 s by the seam):

| Row | Observed |
|---|---|
| `mount` A, `mount` B (a second process) | B `ready` ≈ 1.5 s after A; B's log `mounted JOINED as appender 1`, `slot leases armed (JOINED`; B's `.stats`: `mount_posture = joined`, `wire_posture = joined`, `manager_lease = joined`, `member_epoch = 1` |
| `appenders` / `clients` | 2 pages: 0 `mgr` live (8 MiB ring), 1 live (512 KiB ring, 1 segment); `clients`: A's writer claim + B's member row (`member … writer`, B's pid live) |
| B creates 2,000 files in its own 4 directories | `slot_lease_conflicts` 0, `rotor_mints` 2,004 (the files into B's rotor slots), `xv_cross_owner_intents_minted` 4 (the `mkdir`s under A's root), `slot_grants` 0 (B's own slots, no RPC), `extent_grant_refills` 1, `intents_stuck` 0 |
| A reads B's files under tokens | `cat b1/f7` from A: `dlm_token_grants` on B 10 (the dentry set + the records); a second `cat`: 0 new grants (A's cache); a write on B: `dlm_token_recalls` 2, A's third read is the fresh content; A's `dlm_token_reader_holder_resolves` 527 |
| `tar -x` of 200 files on B into a directory A owns | every create ONE cross-owner intent: minted 5, retired 5, open 0 (`minted ≡ retired + open`), `xv_cross_owner_steps_shipped` 5, `intents_stuck` 0; A lists the 200 and reads `t5` |
| `chmod` from A on B's file | `record_ships` ≥ 1 at A; B's `stat -c %a` reads `600` (B's own kernel view invalidated through the notify seam) |
| 200 MiB `dd` on B | 3 `block_grants` / 192 blocks pulled from A in 3 top-ups (two at the arm — one per data volume — one at the 50 % mark), `topup_refused` 0, `topup_unreachable` 0; B's `df` used 204,800 KiB (= the file: the holder's word minus B's window), A's 786,432 KiB (= every carved grant); the sha from A ≡ B's (the boundary-crossing reads included — the fix in 0002 row 290); `rm` on B → 50 frees shipped and `free_ship_freed` 50 at B, `free_served_blocks` 50 at A, A's `df` back by 204,800 KiB, B's 0 |
| closure | A: `manager_verbs` 7, `manager_verb_refusals` 0, `membership_members` 1, token grants 717, xv steps served 6; B: `joined_wire_verbs` 4, `member_renewals` 16, `joined_wire_words_rejected` 0, `appender_park_expiries` 0, `extent_grant_closure_violations` 0; every must-stay-0 gauge 0 and `invariant_tripwires` 0 on both |
| `umount` B | the clean leave: every lease released over the wire, page 1 `free` (identity cleared), the member row gone (`membership_members` 0 at A); every file B wrote readable from A afterwards (`ls b3` 500, `cat b1/f7` = the fresh content, `stat b2/f3` 600) |
| `umount` A | exit 0 |

The stage runs inside two minutes; the whole smoke suite passes with it (the totals below).

**The kill rows, by hand** (the same binary, a bash harness over the venue; four runs):

| Row | Observed |
|---|---|
| `kill -9 B` mid-`tar -x` (≈ 205 of 1,000 files landed) | A evicts at `T_owner` (3 s): `membership_evictions` 1 → `dead_members_recorded` 1 → the ledger poll acts `dead_members_acted` 1 ≡ recorded × regions held (1) → `appender_recoveries` 1, `appender_recovery_rollbacks` 0, `dead_appender_orphans` 0, `invariant_tripwires` 0; the page `recovered` at 4 s, `free` at the next tick (`recovered_regions_released` 1); A lists every file B acked (205 — the one mid-create included, whole) and reads them |
| remount B at the same mount point (the same-node successor) | B joins again as appender 1; lists the 205 and reads `t5` |
| the successor's first cross-owner op (`mkdir` under A's root), A's `rm -rf` of the recovered tree | NOT clean: before the death fan-out the old B's tokens at A stood under the successor's identical client id and every recall waited the TTL (`dlm_token_recall_timeouts_live` fired; B2's `mkdir` parked 15+ s); with the fan-out B2's `mkdir` answers EAGAIN and A's `rm -rf` of the recovered tree still parks (an `unlink` past `meta_backend_start`); one run of four panicked in `bset.buildBset` under the recovery's `cycleUntilClean` (the two-phase replay's flush of the recovered slot — the recovery agent's seam). Stated below as owed |
| `kill -9 A`, C `-o ro`, `appender clear` | not run (below) |

**Gate totals (this wave).** Linux venue (`scripts/venue zig build gate` under `timeout 3600`,
uncached, 473 s): 18/18 steps; unit 1,822 pass / 15 skipped, rails 5/5, smoke 39 pass / 1 skipped
(the matrix stage inside two minutes), bench-smoke 28/28 — 1,894 / 1,910 in all; `dmesg | grep -c
retained` flat (0 → 0 on the freshly started venue); no `squeezefs` process and no fuse mount left;
`scripts/venue trim` after. macOS (`zig build gate`): 18/18 steps, unit 1,538 pass / 16 skipped
(the Linux-only modules skip). The rails' inverse census carries `joined.zig` and `plane_arm.zig`
under their families.

**Owed from this wave** (each stated where it stops):

- **The same-node successor's cross-owner ops over recovered slots** (above): the successor's
  `mkdir` under A's root answers EAGAIN and A's own `unlink` in the recovered tree parks; the
  slot → holder resolution for inos minted in the dead B's rotor (recovered `Unleased`, then
  partly re-leased to the successor at the same appender id) needs the re-resolve the spec's
  "a writer re-resolves ONCE inside the same op" names on the UNLINK/MKDIR arms, and the
  `buildBset` panic under `cycleUntilClean` needs its red repro.
- **`kill -9 A` → B PARKS-AND-RECLAIMS → remount A → B redials**: the park gate, the member's
  poison and the redial exist and are pinned in-process (`wire_plane` e2e, `park_reclaim`); the
  two-daemon row was not run.
- **`mount -o ro` (C)**: a read-only mount of a LIVE set is still refused by layer A — the
  reader posture (no flock, a projection, the token reader over every slot, the epoch poll
  within `reader_staleness_bound_ms`) is a `KvBackend` posture this wave did not build.
- **The hot-dir handover/striping engagement row**, **kv_fs over a FLIPPED directory**, the
  foreign-slot LAYOUT publish riding `record_ship`, `SupplyStripeInos` over the wire: not run /
  not bound (the cores and the verbs exist).
- **The KVDA page sink** (0002 row 292): the holder's bitmap is RAM; a holder's death re-learns
  the granted ranges from the live writers' declarations and the refs.
- **`appender clear` end to end**: waits on the kill rows' harness.

## Landed after this note (2026-10-08): the frame screen with its stamps, and recovery over the real backend

**The frame plane** (`fencing.FramePlane`, `node_cache.FrameSeam`; charter §5.5 "Fencing";
0002 rows 284–285). ONE source for both halves — the lease table: a frame on a slot-tree node
carries `(lessee, g)` for a live lease and the manager's structural `(0, g)` on an unleased slot
or a `Releasing{dead}` one mid-recovery; a slot-tree load screens with the slot's `(g, lessee)`,
the recorded tail of the leaf (tree 0's `slot_tails`, indexed in the table — loaded at the prime,
replaced at every release and at the recovery's tree-0 step) and the Recovering suspension. The
**priming law**: `Plane.init` (step 9 of the mount ladder, before `coverBringUpResidue`) loads
tree 0's population into the table, computes the manager's overrides (a re-adopted `Leased{0, g}`
at `g`, an `Unleased` native slot at the arm's `g + 1`) and installs the seam; the arm retires the
overrides. A stamp asked unprimed is the `appender_unstamped_frame` tripwire. Pinned:

| Test (`volume_bind.zig`, Linux; `fencing.zig`, portable) | Observed on the venue |
|---|---|
| the one-writer forest | slot 3's leaves: every frame `(0, g₃)`, tree 0 and the table agreeing on `g₃`; the native leaf: `format`'s `(0, 0)` base then `(0, g)` — monotone; the remount screens nothing |
| THE PRIMING LAW, red then green | the kill image opened with `fencing.test_unprimed_first_writer`: the tripwire fires, the preflight writes `(0, 0)` over slot 3's `(0, 1)`; the NEXT mount ends the log there by rule 3 (4 acked records of the window gone, `foreign_frames_screened` +1) — the loss class found twice; the same image opened primed keeps all 8 and screens nothing |
| two regions | B's handover frames `(1, g + 1)` behind A's `(0, g)`; A's loader over B's current frames (the slot's nodes discarded, reloaded) screens nothing; after B's release recorded the tail a zombie `(1, g + 1)` frame at the leaf's tail is screened by rule 2 — the zombie's record never served, every earlier one present |
| the frame plane (portable) | overrides until the arm, `(holder, g)` / `(0, g)` by lease state, the suspension nulls rule 4, the tail index per leaf, a later release replaces the slot's set |

**Recovery over the real backend** (`membership/volume_bind.zig`). The driver's `Volume` seam
bound to a `KvBackend`: the dead page over the cached directory chain (four slots, newest-valid),
`writeForeignPage` for `Recovering`/`Recovered`, the ring as ONE logical image over its segment
table scanned by `journal.replayScanImage` ≥ `ledger_tail_seq` (every record a window record by
handle), tree 0 through the control tree, `discardSlotNodes` + the writer-legal install at ring
0's head (the root node's seq verified against the pointer), the two-phase replay floored at ring
0's head (the frames the flush writes stamped `(0, g)` off the dead mark), the data-bitmap fold
onto the dead holder's pages through its refs, `checkpointNow` cycles until no node of the slot is
dirty (`COVER_CYCLES_MAX`), the tails of every leaf (non-resident ones loaded and counted; resident
only under a device fence), the extents census, the returns through the manager's own verb (RAM
bits released), the orphan census flooring the node-seq handle, the RAM custody quarantine, the
released region's ring back to the heap AFTER its page went Free. `wire_plane` binds one
`VolumeBinding` per managed volume as the projection's hooks and `xv.ops.Plane.rollForwardRecovered`
as its roll-forward, so the manager's ledger poll (the reclaim cadence's tick) recovers a dead
member whose region is on its volumes. Pinned (the venue, debug, file-backed 64 MiB volume):

| Test | Observed |
|---|---|
| a dead in-process region recovered by the manager's poll | region 1 (slot 700, 24 records, one unpublished grant claim) killed Live; the copy mounted: `unrecovered = [1]`, a tick before the record acts on nothing; the record + one tick: `appender_recoveries` +1, `dead_members_acted` +1, the page `Recovered{n_slots 0, head_hint = ledger_tail_seq, recovered_by_term = the manager's}`, all 24 records readable, tree 0 `Unleased{root = the tree's, cursor ≥ 1024, g}` + `slot_tails` (the table's index agrees), the orphan returned (`dead_appender_orphans` +1, its bit clear), the grant record's remaining extents all allocated images, `recovered:{1, 0}` standing, the bound gauge = `recoveryBoundMs(512 KiB, 64 KiB)` and the run inside it; the next tick replays nothing and releases the region (page `Free`, term kept, ring to the heap); a remount: nothing unrecovered, every record durable |
| every crash window (13 steps) | each step's failure rolls back (`appender_recovery_rollbacks` +1), the page `Live` before step 2, `Recovering` through the returns, `Recovered` after; the re-run converges with every record readable and the slot `Unleased`; the early record (the `flush` row) quarantines slot 700's custody durably and in RAM; a third poll replays nothing and frees the page |
| the data-bitmap deltas | a dead holder's `alloc_lease:` homed here with pages in its grant; one SET run of three blocks in its ring; the fold writes one page; the pages read back with exactly those bits set; the region stays `Recovered` while the lease names the holder |

Not in this delivery: the smoke's `kill -9 B` row (a second daemon's region recovered by A's
poll) and the `appender clear` smoke leg wait on the joined daemon; the data namespaces' preempt
rides the data planes' device arm; a `Volume` over a JOINED backend is the joined wave's (a joiner
recovers nothing: the ledger is the manager's).
