# Rung 5 evidence — dynamic routing: the slot→volume map, multi-volume mounts, online `migrate-meta-slot`, the offline set verbs

Spec: `docs/porting/dynamic_routing.md`; charter §5.4, §5.5, §8, §1.5, §1.7, §2. Decision 0002
rows 211–220. Venue: `sqz-zig` (7.2.6-sqz, Zig 0.17.0, aarch64); dev box macOS for the portable
suite.

## What landed

- **`src/meta/routing/route_table.zig`** — the frozen `W = 2¹⁶` map (128 KiB, u16 volume
  index per slot) behind `sync/arc_swap`; `validate` with the reference's refusal texts plus the
  port's own `native_not_slot_zero_host`; `fromRuns`/`coalesceRuns` (one run per member, singletons
  coalesced). **The effective-ino law is table-free**: slot 0's records are the native keyspace of
  slot 0's host, every other slot's records are `guestLocalIno(slot, local)` wherever the slot
  lives — so a key alone names its home, a parked op's keys stay valid across a flip, and the
  reference's per-volume `legacy_slot` test (Q18) does not exist (row 211).
- **`routing/discovery.zig`** — order-independent assembly over N stamps: set uuid, hash seed,
  node size, member count, URI count (both directions), duplicate positions, tombstones,
  stampless members — the nine refusals verbatim; the canonical order by position; coverage and
  disjointness of W; **per-slot highest-epoch-wins** over dual claims. **Q1 decision**: no per-slot
  epoch is stored; a WRITABLE open converges every dual claim before serving
  (`MetaSet.convergeDualClaims`: the loser's idempotent write 2 at the winner's epoch + its tree
  dropped; `routing_dual_claims_converged`); `-o ro` only resolves (row 214).
- **`routing/stamp_cell.zig`**, `ledger.zig` — the runtime stamp: runs + per-slot guest-cursor
  cells (`inos.InoWatermark`, created at first touch) folded into every ledger record
  (`slot_cursors`, a v4 field: `STAMP_MAX_RUNS = 128`, `STAMP_MAX_CURSORS = 256`, worst case
  pinned ≤ one 4 KiB ledger slot). `MembershipStamp.member()` stamps `native_slot = 0` on
  position 0 only.
- **`routing/slot_state.zig`**, `backend.zig`, `tree.TreeTable` — a slot IS a tree: guest slot
  `s` is forest tree `s + 1`, minted lazily on first touch (`mintGuestTree`), its root published as
  the tree-0 control record `slot_state:{s}` at the checkpoint's C1b (`publishGuestRoots`), reopened
  from tree 0 at mount, and re-minted empty for a slot the replay window names without a root.
  The live tree set is an atomically published list (`TreeTable`: superseded lists and dropped
  trees are retired with the table — no grace protocol). Teardown = `dropTree` (one `slot_state`
  delete + every node's extent into pending-free gated on it).
- **`src/meta/meta_set.zig`** — the `MetaSet` facade kv_fs and the plane bind: opens N
  `KvBackend`s (each with its D0 gate, lane, checkpointer), `route(ino)`, lookups/ranges through
  the key's home tree (a multi-slot range merges every member's slot trees in key order),
  `allocateIno` over healthy members (`router.pickMintVolume` + a per-member rotor; the slot gate
  entered before the mint), `checkpointNow`/`fsync`/`close` across members. **Cross-member
  transactions**: `tx.KvTx.cross = .refuse | .independent | .child_first | .parent_first`;
  `commitSplit` groups the staged records by member and commits two ordered entries (create/link:
  the child's member first — a crash leaves an unreferenced inode, fsck C9; unlink/rename: the
  parent's first — a stale nlink, fsck C10; the destroy batch independent); an undeclared spread is
  `error.CrossVolume` (EXDEV, `routing_cross_volume_refused`). The symmetric plane's cross-owner
  intents are rung 6.
- **`routing/slot_gate.zig` + `slot_gate_model.zig`** — the cutover gate: one in-flight word
  per slot, one closed-slot word, the Dekker pair's four accesses SeqCst; entered at op entry
  BEFORE any 4a lock (`kv_fs.lockPlan`; parked ops hold nothing); the drain and the parkers wait on
  `sync.Notify` with deadlines. The model proves the pair holds and that removing EITHER fence
  fails (`expectHolds` + two `expectFails`).
- **`routing/migration_tee.zig`** — the delta tee: a per-pass hook over a committed window's
  record KEYS (`conveyor.CommitTee`, one acquire load per window; zero cost unarmed), only the
  migrating slot's keys (refs by owner), deduped in preallocated open-addressing tables of
  `key.KeyBuf`, `drainRound` swaps tables, overflow latches at the derived cap
  (`deriveDeltaCap(budget) = budget/32 ÷ KEY_BUF_CAP ∈ [64 Ki, 1 Mi]`).
- **`routing/migrate.zig`** — the online engine: preflight (slot 0 pinned, target in the set,
  stamp budgets), ARM (residue wipe, gate installed, tee armed), BULK COPY as a tree copy
  (`builder.StreamWriter` packs the target tree bottom-up from the source's leaf snapshots into
  fresh extents — every kind, zero journal bytes per record, images barriered before the tree
  joins the live set), delta rounds (Put the live value / Delete an absent one through the
  target's conveyor, packed under `journal.entryPayloadCap`; overflow ⇒ a fresh round,
  `MAX_CONSECUTIVE_OVERFLOWS = 3` ⇒ abort), the WINDOW (gate closed → drain → final delta →
  recheck → write 0 (B's checkpoint) → write 1 (B stamps `E, +s`, barrier) → write 2 (A stamps
  `E, −s`, barrier) → RAM swap → gate open; `CUTOVER_DEADLINE_NS = CHECKPOINT_MAX_AGE`,
  `MAX_CUTOVER_RETRIES = 16`, `deriveCutoverThreshold(cpus)` = one conveyor batch of inode
  frames), `finishFlip` and the crash table resolved at mount by highest-epoch-wins. Counters
  `migrate_*`; `migrate_cutover_phase_ns` is an exact-sum family (drain, final_delta, write0,
  write1, write2, swap ≡ total). Seams `SQUEEZEFS_TEST_MIGRATE_HOLD` (`off | after_bulk_copy |
  gate_closed`) and `SQUEEZEFS_TEST_MIGRATE_CRASH_AFTER_WRITE` (0..2).
- **`routing/offline.zig` + `cli/volume.zig`** — `volume list`, `add-meta` (the 15 steps with
  resume detection from the joiner's own stamp; `--take-slots k | s,s,…`; the hostless transient
  stamp through `format`'s `stamp_override`), `remove-meta` (member 0 refuses; capacity preflight
  on the live allocators; a treed slot is copied and its cursor travels, a virgin slot moves in
  the stamp alone; survivors-first stamps; the victim's tombstone LAST), `repair-set` (0 stampless
  ⇒ re-stamp from discovery; 1 ⇒ the complement inferred and the member re-created as the
  bootstrap, said loud; > 1 ⇒ refused), `migrate-meta-slot <slot> <from> <to>` (the engine inline
  over an exclusively opened set; a live mount is told so by name), the staging drain + KD-8
  generation rebind on every set change (`set_epoch`/`prev_set_epoch`). Every mutating verb
  refuses a live-mounted set.
- **Wiring**: `kv_fs` routes every op's nodeid first (`localOf = route_table.effectiveIno`,
  `globalOf` its inverse) and declares each tx's cross-member order; `format` stamps N members
  (`sqmeta://a,b`); `mount`/`offline`/`status` open the set; `stats` exports the `routing` and
  `set` families; the plane's queue-thread arms (the read probe, the direct leg, the W1 hold gate)
  receive the routed KEY ino from the handler binding — a nodeid never reaches the plane.

## Measured on the venue

- **Two-member smoke stage** (`format sqmeta://a,b` → mount → 8 dirs × 250 files → remount in
  the other URI order → `volume list` → `remove-meta` → one-member mount): guest trees minted
  **127** (63 + 64), cross-member mints **1,004** of 2,008, split commits **1,004**, gate parks
  **0**, `routing_same_epoch_dual_claim` **0**; every file byte-exact live, after the reversed
  remount and on the survivor. `remove-meta` of a member hosting 32,768 slots (64 with trees):
  **3,004 records** copied in **4.5 s** wall; `volume list` shows the survivor at
  `[0+1×65536]`, 127 cursors, the victim RETIRED.
- **Migrate under a writer storm** (in-process, two members, the most loaded slot, 4 writer
  threads hitting it through the gate): every acked create present after the flip and after a
  remount, the tee's deltas applied, parked ops resumed, `migrate_cutover_phase_ns` exact-sum.
- **Crash table**: a flip torn after write 0 leaves the old map (B's residue wiped by the
  re-run); after write 1 the next mount hosts the slot on B and converges the dual claim
  (`routing_dual_claims_converged` +1); after write 2 only the RAM swap was lost and the re-run
  `finishFlip`s; the slot then migrates BACK to its origin and mints there (row 212).
- **Three places the smoke found the routing law missing** (each red through the binary,
  then pinned): (1) the plane's queue-thread arms (read probe, direct leg, W1 hold gate) keyed
  their size authority by the RAW kernel nodeid — with the mint spread, nodeid 3 (slot 1's first
  mint) equals KEY ino 3 (slot 0's second), and the probe served the wrong file's inline bytes
  without a FUSE READ ever reaching kv_fs; the binding now routes the nodeid (`handler.zig
  keyIno`, `route_table.isRoutable` declines the virtual control inos `bench` reads). (2) A refs
  range (`volumeRange`, `blockRange`) has both bounds at owner 0 → slot 0, so the "same slot ⇒
  one tree" shortcut read only the native tree: the C8 oracle reported `durable 0` for every
  guest-owned block after a kill -9 and the pack census undercounted; a refs range now always
  merges every slot tree (`meta_set` test). (3) `status`'s offline ref census read the native
  tree alone; it walks the forest (native + every `slot_state` guest root). `remove-meta` copied
  and cursored EVERY hosted slot (32,768 empty trees → extents and the 256-cursor stamp budget
  exhausted); a virgin slot moves in the stamp alone.

## Gates

- macOS `zig build gate`: fmt + the portable suite (route table, discovery, gate + model, tee,
  stamp cell, slot_state, stats family, CLI grammar) + rails green; the unit binary looped 8×
  clean. One early run of the gate test "an open gate admits…" raced its deterministic
  observable (the parker's REGISTRATION preceded its first try, so a reopen could admit it
  before it parked); it now waits for the park COUNT — 60 loops clean.
- venue `scripts/venue zig build gate`: full unit suite (incl. `meta_set`, `migrate`, `offline`,
  the two-member e2e) + rails + the smoke (every stage incl. the two-member one, the kill
  matrix with C8 0 drift) + bench-smoke green (exit 0); no mounts or daemons left. One full
  smoke run before the refs-range fix saw the 8-way storm's writes wedge in `commit` for > 75 s
  (the watchdog's census); it did not recur in five later runs of the same stages — recorded
  here as unexplained, with the daemon's wedge census the next sighting's first evidence.

## Deviations stated

- `migrate-meta-slot` runs the engine inline over an exclusively opened set; a live-mounted set
  is refused by name (the engine beside a serving daemon is the admin channel of rung 8). The
  live leg — parked ops, the writer storm, the crash matrix — is the engine's in-process contract
  and the binary's crash seam (row 219).
- A rename or link whose records span two members with no declared order answers EXDEV loud
  (`routing_cross_volume_refused`); the declared orders cover create/mkdir/symlink/link (child
  first), unlink/rmdir/rename-whiteout (parent first) and the destroy batch (independent).
- `--take-slots` is bounded by `STAMP_MAX_CURSORS = 256` slots per join (the joiner's stamp
  must encode every travelled cursor); the default join takes the `k` most loaded takeable slots.
- `src/squeezefs.zig`'s anchors gate `meta_set` on `builtin.os.tag == .linux` (it binds the
  Linux-only backend); `routing` is portable and ungated.
