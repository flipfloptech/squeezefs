# Rung 6 wave A2 — the appender region, tree-0 control records, the manager lease, extent grants

Spec: `docs/porting/sym_appenders_manager.md`; charter §5.5 ("The forest", "Partition law",
"Appenders", "The record-seq space is monotone across rings", "Node seqs are
incarnation-scoped", "The manager lease", "Extent grants"), §5.1, §5.7, §1.7. Decision 0002
rows 230–235. Modules: `src/meta/sym.zig` + `src/meta/sym/{control_records, appender_page,
appender_dir, region, extent_grant, manager, join, bind}.zig`.

## What landed

- **Tree 0's control records** as v4 `extern struct`s under BINARY keys `kind 8 ‖ tag ‖ BE id`
  (≤ 16 B, index order, below every ASCII-keyed record): `slot_state` v2 (68 B, one shape for
  Unleased/Leased/Releasing), `slot_tails` (inline/spilled by a flag bit), `extent_grant`
  (normalized runs, `maxRuns(value_cap)` = 4,111 at 256 KiB), `appender_hint` (52 B, the pending
  GrowRing witness), `custody_quarantine`, `dir_rename`, `alloc_lease` + bitmap refs,
  `dead_member` (32 B, the early flag), `recovered`, `node_seq_incarnations` (the 9 B value
  codec kept). Every decoder total and canonical; fuzzed + a 25 k-case PRNG property.
  `routing/slot_state.zig` is a face over the ONE codec (backend + status migrated).
- **The appender page** v4: fixed 296 B, xxh3 LAST; `SLOT_PAGE_BUDGET = (4088 − 296) / 35 = 108`
  by comptime assert (the charter's number restated); `classify` ∈ {valid, blank, corrupt,
  foreign_layout} with ForeignLayout refusing the whole four-slot read; `pageSlotFor(g) = g % 4`;
  `dirNamedMask` + `pageWriteSlots(mask, g)` = the directory-first law, tied over every cell.
- **The directory**: `KVAD` header FIRST, `pairs = (node/4096 − 1)/2` (31 / 7), dense ids
  `1 + chain_index × pairs + pair`, the bounded chain walk (index enforced, a cycle refuses), the
  torn-growth-safe grow plan (new header + barrier, then the relink + barrier).
- **The region** (`region.zig`): `RING_FLOOR_BYTES` = 512 KiB (comptime), the ceiling clamp, the
  `heap/16` budget, `appendersCapacity`, `joinRingUnderBudget` (one law, Q-10), `growRoom`,
  `growSegmentFloor`, `growTarget` (one step ≥ a doubling), `ringSegmentsThatFit` (all ≤ 8 / the
  largest half at the floor / the largest 8), `zeroPlan` (4 MiB chunks), `seqContinuation`,
  `SEQ_FRONTIER_SANE_MAX`, the incarnation mint, `RegionCore` over a segmented
  `journal.RingLayout` (the fixed ring's four pages / a joiner's two ring-side pages carved off),
  the per-cycle page plan, the fixpoint tail law. **Model** `SeqOffsetModel`: a stamp reserved
  after a raise exceeds the floor and stamp order is reservation order; the weakening (the offset
  read outside the registry lock) fails `FinalViolated`.
- **Extent grants**: `grantDerived` (floor 8, cap `free/(4 × appenders)`, 2× headroom),
  `failoverBoundMs` (45 s + ladder + replay), `grantWireCap`, `clampGrantWant` (an over-cap want
  is REJECTED), the pool target/refill laws, the return screens (`validateReturnRuns`,
  `coalesceRuns` bounded by the frame — 250 k copies of one run coalesce to ONE —
  `intersectWithRecord` bounded by the record), `pageRunsOf` (the largest ≤ 4), the RAM
  `RegionGrant` with `granted ≡ held + unclaimed + returned` asserted on every mutation and a
  4 k-step random walk. **Model** `ClaimReturnModel`: never claimed-and-returned under the grant
  lock; the lockless return fails `InvariantViolated`.
- **The manager** (`manager.zig`) over a `Volume` seam: `joinAppender` (the whole §3.1 order:
  directory read → durable idempotency → the remainder law → the lowest Free pair / chain growth →
  the pending witness settled → carve + zero + ONE control entry → the incarnation minted and
  barriered → the page Live into BOTH directory slots + barrier → residue returned → the pool
  floor `8 + M` granted), `leaveAppender` (screens; `Deferred` while anything is leased; Free into
  both slots keeping id/term/head_hint; bits clear AFTER the barrier), `extentGrant` (verbatim
  replay when the page word ∩ the record covers `want`; a top-up to the pool size; the run cap;
  `extent_grant_conflicts`), `returnExtents` (`cleared + already ≡ named`; the own-region RAM
  screen is the `manager_verb_refusals` tripwire), `growRing` (the pending witness replayed
  verbatim; the longest run; the segment floor; eight segments decline), `publishRoots` (the root
  word screen minus the seq-floor clauses), `publishEndpoint`/`resolveEndpoint`; the ONE reply
  classifier (`Rejected → rejected`, `Deferred → deferred`, a full USER window → the checkpoint
  kick + `deferred`); the `HandlerTable` with the 8 served verbs and typed empty slots for the
  slot/token/dir-rename/data-plane/recovery verbs (`rejected{verb_unserved}`); the structural
  door bounded by `COVER_CYCLES_MAX` (= 64; a moving tail defers, a stuck tail is `Corrupt`).
- **The join ladder** (`join.zig`): `nodeToken = xxh3(boot_id ‖ hostname)`, the `Door` seam, the
  reply SCREENED before anything is installed (`joined_wire_words_rejected`), the region core +
  RAM grant from the reply's word, `NodeSeqHandle.incarnation(base)`; rejoin = `already` with the
  page's head resumed; the clean leave.
- **The binding** (`bind.zig`, Linux): the backend accepts `SYMMETRIC_PLANE`
  (`SQUEEZEFS_SYMMETRIC_META=0` on a stamped volume refuses), the ring proper sits behind
  appender 0's four pages (`geometry.fixedRingExtent`; the gate's probe reads the same), the
  directory census at open (a Live page of another appender is unrecovered + loud + counted),
  region 0 Live with `is_manager` and `term + 1` (a foreign Live page 0 = the D0 ladder won over
  its holder: the role passes), the lease held, ONE barriered cycle; the checkpoint's C11 writes
  every region's page (`RecordSource.appenderPages`); control entries are USER-class ring-0
  entries through the structural writer and BARRIERED; `joinInProcess` (the N-daemon harness shape)
  spawns a lane over the region's segment table; `appendPut` reserves on the region's registry
  inside the leaf lock window and floors the shared cache's node at ring 0's head; the pool
  advances/recycles at the cadence; `leaveAll` at close. `format` writes the symmetric class by
  default (the directory link at heap extent 0 zeroed, `KVAD` first, appender 0's Free page at
  slot 1); `--single-writer` is the flat opt-out; both classes mount.

## Measured on the venue (Ubuntu 24.04 / 7.2.6-sqz, 64 MiB file volumes, 64 KiB nodes, 1 MiB ring)

- The 108 budget tie: `(4088 − 296) / 35 = 108` (comptime).
- `appendersCapacity(heap/16)` on the venue's 62 MiB heap = 7 at the 512 KiB floor; on a 1 TiB
  heap 131,072 (the spec's "128 k at the floor").
- E2E: format (symmetric) → mount A (manager, term 1) → in-process region 1 joins (512 KiB ring,
  zeroed; a 12-extent grant = `8 + M(4)`; incarnation 1) → 6 records through the conveyor + 6
  through region 1's own lane → two cycles (the second names region 1's tail at its head) →
  close (region 1 leaves: page Free, term kept, 8 + 12 extents back) → remount: every record
  present, a clean unmount, term 2, `manager_verb_refusals = extent_grant_conflicts =
  joined_wire_words_rejected = 0` (deltas).
- kill -9 (the image copied with two regions live): the remount recovers ring 0 (every acked
  record), region 1's page reads Live under mount slot `0x5001`, `appender_live_pages_at_mount`
  +1, the unrecovered set names it, the mount serves and logs loud.
- The whole backend suite (every `TestVol` now formats the symmetric class) and the smoke's
  mounts run on the stamped class.

## Known boundaries (the next waves)

- A region ≥ 1's ring is NOT replayed at remount (the recovery wave): its page stays Live and
  the mount refuses its slots until recovered.
- The region tail law is the fixpoint form (0002 row 235): the per-ring floor word arrives with
  the slot leases, which make a node's ring unique.
- `ring0WindowClearOf` answers "the window is empty" until the slot wave tracks per-slot record
  frontiers; the structural door therefore cycles to the fixpoint.
- The wire binding of the `HandlerTable` is the wire agent's (`src/wire/`): the table's
  `dispatch(verb, payload)` is the seam.
