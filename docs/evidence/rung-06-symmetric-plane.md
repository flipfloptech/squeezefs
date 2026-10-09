# Rung 6 — the symmetric metadata plane (waves A–D): the forest, appenders and the manager, slot leases, tokens, cross-owner intents and striping, the data planes, membership and recovery, the join ladder, the N-daemon posture's manager half, the default flip

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

- **The JOINED backend (the N-daemon posture's joiner half).** A second `squeezefs mount` of a
  symmetric set is still refused by layer A (the flock). The spec's joined door
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
- **The recovery driver over the real backend.** `recovery.Driver` has only the RAM `Volume`
  seam; the ledger poll runs in the daemon with NO volume hooks (it drains parked death records
  and retires expired ones; `dead_members_acted` stays 0). A dead joiner's region stays `Live`
  (the smoke shows C's) until the 25-function device `Volume` (the dead ring's window read,
  the structural-class replay under stamps, the writer-legal root install, leaf tails, orphan
  images) is bound.
- **The frame screen through the loaders — BOTH halves together.** Every product frame is still
  written with the layout's `(0, 0)` stamp (`node_cache.freezeLocked` takes the cache's one
  `NodeLayout`), and `node_cache.loadLeader` / `buildNode` / `smo.Source.load` call
  `verifyNodeExtentScreened(…, null, …)`. The load path DOES carry the slot
  (`KvTree.loadForSlot(addr, forest_slot)` → `loadLeader(addr, slot)`), so the screen half is a
  `FrameScreenSeam` on the cache the slot plane installs at its arm (`ScreenSource.screenFor(slot,
  recorded_tail)` with a RAM index of tree 0's `slot_tails`); the stamp half is a per-slot
  `layout.stamped(FrameStamps.stampFor(slot))` at every freeze (the flusher's and the SMO's).
  They must land in ONE change under the priming law (`fencing.FrameStamps.prime` at the first
  frame writer): a screen armed over `(0, 0)` frames is inert until the first release records a
  tail, after which rule 2 would end a leased leaf's log at the stale stamp — the acked-writes loss
  class the law exists for. Deliberately not half-done here.
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
- **`appender clear` end to end** on a dead joiner's region (the verb exists; its smoke leg needs
  the recovery driver's device binding).
- From earlier waves, unchanged: the paged-resident dentry set, the in-place join of a
  fail-stopped quiescent reader, `slot_tails` past the inline cap, the dead recoverer's stashed
  interior records, the un-share protocol; mTLS, the nvmet PR legs and fsck C13–C17 as full checks
  are rung 8's.
