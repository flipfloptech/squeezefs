# Rung 6 — the symmetric metadata plane (waves A–F): the forest, appenders and the manager, slot leases, tokens, cross-owner intents and striping, the data planes, membership and recovery, the join ladder, the N-daemon posture (the manager half, then the JOINED backend and the two-daemon matrix), the default flip, the kill rows and the `-o ro` reader (the close)

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
| the kill rows (wave F) | `kill -9 B` mid-`tar` → eviction → recovery → the same-node successor's first op; `kill -9 A` → B PARKS-AND-RECLAIMS into the D0 successor's grace window (the manager link re-resolved off the epoch poll, the allocation leases taken over); `appender clear` end to end — as smoke stages | `cli/smoke.zig` "THE KILL ROWS", `sym/joined.zig` `ManagerResolver`, `sym/wire_plane.zig` `PeerLinks` |
| `-o ro` of a live set (wave F) | the READER posture: the projection backend (no flock, no claim, no ring), the reader plane (a member-reader + token client with every slot foreign; the quiescent reader with the epoch poll's `ESTALE` fail-stop), the FUSE gate | `backend.openReader`, `sym/reader_plane.zig`, `fuse/handler/reader_gate.zig` |

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

The rung's closing totals are in the wave-F section at the end; the manager-half wave's were:

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

The rung is CLOSED (wave F, 2026-10-09; the section "Landed after this note (2026-10-09, wave F)"
at the end). What remains, each stated where it stops:

- **The shared-dir handover/striping matrix row**: the holder-decided handover under `N_floor`
  and the K = 64 striping engage on a hot directory two writers share; the cores and the
  `.stats` words exist (`slots/handover.zig`, `xv/striping`), the two-daemon row that drives a
  shared directory past `N_floor` and asserts the offer, the move and the stripe flip was not
  written.
- **kv_fs over a FLIPPED directory** (lookup/create/readdir through the stripe map, the explicit
  `setfattr -n user.squeezefs.stripes` flip): the stripe map and its verbs exist; the handler's
  routing through it is not bound (`setfattr`/`getfattr` are also absent from the venue image).
- **The foreign-slot LAYOUT publish riding `record_ship`** at the data plane's publish site, and
  **`SupplyStripeInos` over the wire**: the verbs exist and are served; the publish site still
  publishes locally and the striped-create's ino supply is in-process.
- **The KVDA page sink** (0002 row 292): the holder's bitmap is RAM; a holder's death re-learns
  the granted ranges from the live writers' declared windows and the refs (the manager's D0
  successor takes the leases over with no page fold — the wave-F row).
- **mTLS on the cluster wire** — rung 8.
- **fsck C13–C17 as full checks** — rung 8 (C13's orphan class is the recovery's return; C14 is
  the mount-path refusal; C15 is the mount-path recovery; C16/C17 report nothing yet).
- **The nvmet PR legs** (the preempt on a PR substrate, the registrant cap learner against a
  real target) — rung 8.
- **A joiner's `.stats` under a dead manager**: the kernel revalidates the root's attributes
  before the virtual lookup, and the root is the manager's object — on a parked joiner the walk
  answers EAGAIN until the successor admits the reclaim (the smoke observes the park through the
  daemon's log). The `-o ro` fail-stop gate exempts `getattr(1)` for exactly this reason; the
  parked joiner wants the same exemption served off its token cache's last root entry (a stale
  root attribute under a dead holder can mislead no reader), owed.
- From earlier waves, unchanged: the paged-resident dentry set, `slot_tails` past the inline cap
  (a spilled set screens by the lease alone), the dead recoverer's stashed interior records, the
  un-share protocol; the in-place join of a fail-stopped quiescent reader (the reader fail-stops
  and the remount joins: the cheaper stopgap the spec names).

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

**The kill rows, by hand** (wave E, four runs): A evicted B at `T_owner` and recovered its region, but the same-node successor's first cross-owner `mkdir` answered EAGAIN / parked 15 s, A's `rm -rf` of the recovered tree parked past `meta_backend_start`, and one run of four aborted the manager in `bset.buildBset` under the recovery's `cycleUntilClean`. Both defects are root-caused and fixed in wave F below; the rows are smoke stages now.

**Gate totals (this wave).** Linux venue (`scripts/venue zig build gate` under `timeout 3600`,
uncached, 473 s): 18/18 steps; unit 1,822 pass / 15 skipped, rails 5/5, smoke 39 pass / 1 skipped
(the matrix stage inside two minutes), bench-smoke 28/28 — 1,894 / 1,910 in all; `dmesg | grep -c
retained` flat (0 → 0 on the freshly started venue); no `squeezefs` process and no fuse mount left;
`scripts/venue trim` after. macOS (`zig build gate`): 18/18 steps, unit 1,538 pass / 16 skipped
(the Linux-only modules skip). The rails' inverse census carries `joined.zig` and `plane_arm.zig`
under their families.

**Owed from wave E** — every item below landed in wave F (the close, further down) except where the close's own owed list says otherwise.

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

## Landed after this note (2026-10-09, wave F — the close): the kill rows as smoke stages, the two defects, the `-o ro` reader of a live set

**The two defects the by-hand kill rows exposed** (0002 rows 296–299, 293–295, 297–298):

| Defect | Cause | Fix (red test first) |
|---|---|---|
| the manager's abort in `bset.buildBset` under the recovery's `cycleUntilClean` (1 run of 4) | `volume_bind.readRingFn` / `replayFn` mapped every non-`put` frame of the dead ring to `delete` (the driver's `Kind` had no `delta`): an inode DELTA — every `write`'s size, every `setattr` — replayed as a TOMBSTONE carrying the delta's bytes; the flush's record encoder asserts `kind != delete ∨ value.len == 0` → the abort (the stack's "unreachable" was `std.debug.assert`'s panic, the ERT pointing at `buildBset`); had the assert not fired, the recovered inode would have been deleted — a silent loss class | `recovery.Kind.delta` as a content kind; `tree.kindOf` (pub, 1:1, asserted against the journal's enum at comptime) in both the mount replay and the recovery; a tombstone with a value is refused typed at the ONE apply choke point (`tree.Error.InvalidRecord`) so a mislabelled replay fails the recovery LOUD (`appender_recovery_rollbacks` +1, the page stays `Recovering`) instead of aborting the daemon; the frame encoder's `RecordsNotAscending`/`EmptyBset` at the freeze is a tripwire (`kv_freeze_order_refusals`) + `Corrupt`, never `unreachable`. Red: `volume_bind`'s dead region now carries a delta (`size = 4096`) and a tombstone in its window — the old mapping fails the recovery (typed), the fix folds the delta (`InodeValue.size == 4096`) and drops the tombstoned key |
| the same-node successor's first cross-owner `mkdir` EAGAIN / parked, A's `rm -rf` of the recovered tree parked past `meta_backend_start` | `wire_plane.PeerLinks` bound ONE `SharedLink` per token client id and per holder appender id and never re-resolved it; the death fan-out swept the dead member's grants but left its sessions; the successor (the same client id and appender id — `(node, mount_slot)` — at a NEW ephemeral port) was pushed to the dead incarnation's port: every recall `unreachable_` until the lease TTL (`dlm_token_recall_timeouts_live` fired, the `unlink` parked 25 s), every xv step to holder 1 EAGAIN | every peer link carries an `EndpointResolver` consulted before every (re)dial — the owner's roster by client id, the RAM endpoint table / `ResolveEndpoint` by holder — and `DeathFanout.noteDeath` retires the dead member's sessions at once (`peer_links_invalidated`; `peer_links_repointed` on a move). Red: `wire_plane` "THE SAME-NODE SUCCESSOR IS REACHED" — a member at a dead port, its eviction, the successor under the same ids at the plane's own listener: both links' next call lands. A second cause in the same row: the recovery released the slot in the lease TABLE but the holder cache's LEARNT view (primed off tree 0 at the arm: "leased by appender 1") outlived the release — every read of a recovered slot `unreachable_` on a mount that armed with the dead lease in tree 0 (the `appender clear` leg's next mount); `noteUnleasedFn` drops the view (red in `volume_bind`: `stepHome(DEAD_SLOT)` `unreachable_{1}` before, `.local` after) |

**The kill rows as smoke stages** (`cli/smoke.zig` "THE KILL ROWS (a)/(b)/(c)"; A = the rig's manager, B a second process under its own UTS hostname `sqz-nodeb` — another node to the identity law, so a remount under the same name is the same-node successor and `appender clear` judges its page foreign residue; the venue as above, `T_owner` 3 s by the seam):

| Row | Observed |
|---|---|
| (a) `kill -9 B` mid-`tar -x` of 1,000 files into A's directory (201–203 visible) | A: `membership_evictions` 1 → `dead_members_recorded` 1 → `dead_members_acted` 1 (≡ recorded × 1 region) → `appender_recoveries` 1, `appender_recovery_rollbacks` 0, `dead_appender_orphans` 0, every must-stay-0 gauge 0; the page `recovered → free` (`recovered_regions_released` 1) **8.5–8.7 s after the kill** (`T_owner` 3 s + the 5 s reclaim cadence); 202–204 files whole on A (≥ the count seen at the kill; every body names its file) |
| remount B under the same node (the same-node successor) | `mounted JOINED as appender 1` a second time; B lists and reads the same 202–204 files whole |
| the successor's FIRST cross-owner `mkdir` under A's root | **5–8 ms**; `xv_cross_owner_intents_minted` 1, retired 1, stuck 0 |
| A's `rm -rf` of the recovered tree | **76–87 ms** for 202–204 unlinks; `dlm_token_recalls` 206–208 at A, acked at B 205–207, `dlm_token_recall_timeouts_live` 0, `invariant_tripwires` 0 on both |
| (b) `kill -9 A` (the manager) mid-storm on B (202–213 of 2,000 creates visible) | B **PARKS 413–644 ms after the kill** (`T_self` 798 ms from the kill's last renewal; the daemon's own log line — its `.stats` is behind the kernel's revalidation of the root, a foreign object whose holder is dead, until the successor stands: a finding, below), the storm child alive with its acks held at the pre-admission gate; after the reclaim `membership_self_fences` 1, `appender_park_expiries` 0 |
| remount A | the D0 ladder's same-host dead-pid proof: `appender 0 Live as the manager (term 2`, `membership owner term 2 … grace open`; the allocation leases TAKEN OVER from the dead incarnation (`alloc_lease_same_node_takeovers` 2, `alloc_lease_successions` 2, the EARLY records retired by the grant: `dead_members_recorded` 2, `appender_recoveries` 0 — B's region is never recovered) |
| B's reclaim | the manager link re-points at A's new port off the epoch poll's newest checkpoint (`manager_repoints` 1, `the manager's listener moved to`), the reclaim admitted in the grace window (`admitted the reclaim`), the token plane re-pointed (`dlm_token_holder_repoints` 2), this listener re-published; the held writes drain: **the storm's 2,000 files whole on B AND on A** (read under tokens from B's listener dialled afresh); `appender_park_expiries` 0, `invariant_tripwires` 0, `joined_wire_words_rejected` 0, `membership_members` 1 at A. (B's held write outlives the 5 s op watchdog by design — the park holds acks up to `T_park_max`; the stage names the must-stay-0s one by one) |
| (c) `appender clear <uri> 1` with A and B live | refused: `live-mounted` |
| A's clean umount (B parks at `T_self`), `kill -9 B` parked (its page Live, unrecovered, no manager to record it) → `appender clear <uri> 1` | `appender 1 cleared (was live)`: the four writes — the page `Recovering{term 0}` (the next `appenders` row), the writer bring-up, the EARLY record on volume 0, the leave |
| `appender clear <uri> 0` | refused (`refusing — the page is this node's OWN residue`: appender 0's page is the smoke host's manager; the `appender_zero` refusal is the fourth gate behind it) |
| the next `mount` | its ledger poll acts on the EARLY record: `appender_recoveries` 1, `dead_members_acted` 1, rollbacks 0, `slot_custody_quarantines_installed` 64 (the EARLY record quarantines the recovered slots' custody); `appenders` shows the page `free`; B's 8 files whole from A |

**The `-o ro` reader of a LIVE set** (`backend.KvBackend.openReader`, `meta/sym/reader_plane.zig`, `fuse/handler/reader_gate.zig`; 0002 rows 294–295): a `-o ro` mount of a symmetric set is a third `KvBackend` posture — the device read-only, no flock, no claim ("a reader claims nothing"), no ring, no lane, no conveyor, no checkpointer; every tree a PROJECTION at the newest checkpoint (the ledger's roots + tree 0's `slot_state` roots), every mutation entry `PartitionRefused` (EROFS). The writer faces at the mount decide the arm: an exclusive writer `flock` on the device (read off `/proc/locks` — no lock of the reader's own) or a heartbeat-fresh writer claim ⇒ TOKENS: a member of the home shard as a READER (a random 16 B id, no region, no park gate), the manager link off `endpoint:{0}` (the same epoch-poll resolver as a joiner's), ONE listener for the pushed recalls, and the divert with EVERY slot foreign (tree 0's lessee, or the manager for an unleased slot) — every TTL derives to 0 (`tokenReaderTtls`; an explicit `-o *_timeout` refuses), `reader_staleness_bound_ms` 0. No writer ⇒ QUIESCENT: the projection served exactly, the S5 epoch poll + the flock face every checkpoint landing ceiling (1.1 s) on the reader's beat; a writer seen fail-stops the mount ONCE — `ReaderGate` wraps the filesystem's vtable so every op answers `ReaderFailStopped` (ESTALE), `getattr(1)`, the virtual lookups and the virtual inodes excepted (`.stats` reads WHY). A token reader whose lease lapses re-evaluates the faces at its beat: no writer ⇒ the quiescent posture (every volume adopts the writers' last checkpoint RCU-style — the old trees retired, never freed under a reader), a writer ⇒ a rejoin with the planes re-pointed, every serve the typed EAGAIN meanwhile. A joiner's holder resolves a reader's listener for its recall push through the owner's CENSUS over the wire (row 294). The smoke row (`THE -o ro READER OF A LIVE SET`):

| Row | Observed |
|---|---|
| `mount -o ro` C beside A (manager) and B (joined) | `mounted as a READER — the checkpointed projection`, `a member-reader of the home shard … every object read under a token`, `TTLs attr 0 ms entry 0 ms dir_entry 0 ms negative 0 ms`; no `writer claim committed`; `appenders` still lists A and B alone (a reader holds no region); `mount_posture = reader`, `reader_arm = tokens` |
| C reads B's 2,000 files and A's tree | `dlm_token_grants` 2,016, hits 111, **`dlm_token_local_reads` 0**, `dlm_token_reader_holder_planes` 2 (A and B), `reader_staleness_bound_ms` 0; a write on C answers `Read-only file system` |
| B writes `b1/f7` | C's next read is the fresh content (`dlm_token_recalls_received` ≥ 1 at C, `recall_timeouts_live` 0): the recall — not a TTL, not a poll — is the proof |
| A and B leave | C's lease lapses, no face stands: `reader_arm = quiescent` (`reader_quiescent_transitions` 1), every read local and fresh off the writers' last checkpoint (`fresh`, 500, `a2`) |
| A mounts again | C's poll sees the ledger advance: `a writer appeared (ledger_advance) under the quiescent reader — every metadata op now answers ESTALE`; `ls` and `cat` on C answer `Stale file handle`, `.stats` reads (`reader_fail_stopped: true`) |
| remount C | `reader_arm = tokens` again; `fresh`/`a2` read under tokens; every must-stay-0 gauge 0 on A and C, `membership_members` 1 at A |

**Also in this wave**: a fail-closed token channel (a failed round) is re-probed ONCE inside the same op by the divert (row 293 — before it, one transient failure left every read of that holder EAGAIN for ever); the wire reply buffer holds a full grant page (`frame.CONTROL_CAP`, asserted ≥ 2 × `GRANT_PAGE_BUDGET`; a 4,000-entry directory read from a peer answered EIO at the 64 KiB cap — row 299); the manager's D0 successor takes its predecessor's allocation leases over (`acquireAndHold` with page 0's identity at the open as the proof, incarnations told apart by `writer_id` — row 298); a holder bitmap with no journal sink queues no deltas (the ring filled after 192 carves once the successor armed its grant plane — the RAM posture of row 292 made explicit); `status`'s guard probe reads `/proc/locks` like the reader's.

**Gate totals (the close).** Linux venue (`scripts/venue zig build gate` under `timeout 3600`,
uncached, 7 m 53 s): 18/18 steps; unit 1,827 pass / 19 skipped, rails 5/5, smoke 43 pass / 1 skipped
(the three kill-row stages ≈ 90 s together, the `-o ro` row ≈ 40 s), bench-smoke 28/28 — 1,903 /
1,923 in all; `dmesg | grep -c retained` 0 → 0; no `squeezefs` process and no fuse mount left;
`scripts/venue trim` after. macOS (`zig build gate`, uncached, 6 m 17 s): 18/18 steps, unit 1,540
pass / 16 skipped (the Linux-only modules skip), rails 5/5.
