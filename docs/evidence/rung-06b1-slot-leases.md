# Rung 6 wave B1 — slot leases: the dense lease table, the ONE commit door, acquisition with parent affinity, holder-decided handover, the page-budget law, slot → holder resolution

Spec: `docs/porting/sym_slot_leases.md`; charter §5.5 ("Slot acquisition and minting", "Handover",
"Slot → holder resolution", the commit door and the structural door, the page-budget overflow law),
§5.2 (the door before queueing), §2 lock order. Decision 0002 rows 248–256. Modules:
`src/meta/sym/slots.zig` + `src/meta/sym/slots/{lease_table, slot_door_model, door, acquire,
handover, page_budget, resolve, plane}.zig`; the bindings in `meta/{tx, conveyor, backend,
meta_set, tree, smo, leaf_merge}.zig`, `meta/sym/{manager, bind}.zig`, `meta/routing/migrate.zig`,
`fuse/handler/kv_fs.zig`; `routing/slot_gate.zig` + `slot_gate_model.zig` deleted.

## What landed

- **The lease table** (`lease_table.zig`): ONE dense table of 40 B hot cells per slot (`holder ‖
  state ‖ dead ‖ rotor` as the Dekker word — SeqCst on both sides —, `g`, `inflight`, the record
  frontier, the two half-window `HolderOps` buckets as one CAS word) + a cold map for slots that
  ever held a lease (the offer, `last_written`, the four words + `seq_floor`) + per-holder held
  lists (O(held) walks). The state machine `unleased ↔ leased ↔ offered ↔ releasing` (+ `migrating`,
  rung 5's cutover fold); `g` strictly monotone (+1 per grant, kept across a rollback); `release`
  exact on `g` (stale/future refused, same-`g` Unleased = already); `prefer: unleased-then-idle`
  (never-written first, then LRU by `last_written`, ties by index); `settleEntry` — rows 5/6, the
  stale-entry drop, C14's conflict — and `liveOnTwoPages`. `foreign` is derived (`holder ≠ me`).
- **The model** (`slot_door_model.zig`): committer (`inflight += 1 ; fence ; load state`, backing
  out on `releasing`), closer (`state = releasing ; fence ; load inflight`, then the snapshot under
  the node lock), and the belt's token-less apply arm reading its verdict under the lock. Holds;
  each fence removed fails (a passed commit escapes the drain); check-then-lock fails (applied,
  not in the snapshot). `committers = 1` is rung 5's two-thread gate, both weakenings RED.
- **The door** (`door.zig`): `enter(slots, me) → Tokens([6]u16)` — the verdict table of spec §3.1
  (own lease ⇒ token + the holder-op note; own `releasing` ⇒ PARK, register-recheck-await; another
  appender's ⇒ `SlotBusy` = `errors.SlotMoved` (EAGAIN) with every earlier token returned and the
  requester's touch noted on the window; an offer standing for `me` ⇒ the accept through the
  first-touch seam; `unleased` on an armed volume ⇒ first touch (`Deferred` ⇒ EAGAIN); not hosted
  ⇒ `SlotMoved`; `migrating` ⇒ park (`routing_gate_parks`); unarmed ⇒ only the cutover parks).
  `beginRelease / drain / endRelease`, `closeForCutover / reopenCutover`. Counters
  `slot_door_{parks, refusals, draining_admits, resumed, park_refused, moved}`, `slot_acquires`;
  closure `parked ≡ resumed + refused` pinned.
- **Acquisition** (`acquire.zig`): `M = clamp(W/(2·writers), 1, 64)` (ties 1→64, 512→64,
  1 024→32, 12 500→2, 2^20→1), `2M` the rotor cap, `A_max = max(leaf_bytes/64, node)` with the
  static knob REFUSING outside `[node, heap]` (Q13), `cursorFloor`, `mintChoice` (affinity →
  strictly-over by ≥ 1 extent (equality is not over) → overflow under `2M` / smallest at it →
  signed headroom, rr ties) with the counter closure, `gather` (`user.squeezefs.gather`, the
  bool-knob vocabulary, admitted BY NAME in the xattr allowlist), the cadence's decisions (forced
  shrink smallest-first to `M`, LRU past the budget, region release). The five verbs over the
  manager's volume seam: `acquireSlots` (screen → PARK admission holding nothing → the mutex →
  explicit ∪ the rotor pick → the rotor cap → the structural door for a non-manager grantee →
  RAM `g+1` → ONE barriered control entry → the install; a failed write rolls back keeping `g`),
  `acquireSlot` (the in-process ACCEPT: transfer then grant under the handover mutex),
  `offerSlot`, `releaseSlot` (every word screened against tree 0's record — `seq_floor` strictly
  above the grant's, the cursor, the root, the tails — a stale `g` is the `manager_verb_refusals`
  tripwire; ONE entry `Unleased` + `slot_tails`; ring 0's offset raised; the RAM drop last),
  `resolveSlot`; the handlers bound into the manager's `HandlerTable` through the wire codecs and
  `wire/screen.zig`.
- **Handover** (`handover.zig`): the EWMA (seeds, then eighths), `N_floor = max(2, ceil(h/s))`
  (19 at the fleet's 5.5 ms ÷ 0.3 ms; the cold start 4 barriers + 3 round trips), the cooldown
  `8 × T_idle`, the rule without sustain/immunity, the bounded `[8]` dominance window with the LRU
  victim, the directory-parent memo walk (a holder live below a directory is never idle at it;
  another volume's ancestor credited there; a miss ends the walk), and `transfer` — the five steps
  with the seam (`drainPendingTimes` → gate → drain → cycles until the LESSEE's ring tail passes the
  frontier and the root is published, the moved/unmoved split at `COVER_CYCLES_MAX` → page
  `Releasing{root, cursor, g}` + barrier → `ReleaseSlot` → page drop), `slot_handover_phase_ns`
  exact-sum, `Hold{after_page, after_tree0, park_after_page}` as a plane field.
- **The page-budget law** (`page_budget.zig`): `Publication{root, cursor, home}`, the ONE page
  writer's rank (mid-handover, page-homed, unpublished, tree-0-named), the cut at 108 with
  publish-before-drop (`demote`), `tree0Publications` for the overflow and the cursor law, and the
  pre-barrier `cursorRefresh` (`meta_kv_cursor_page_writes`).
- **Resolution** (`resolve.zig`): `SlotHolderCache` as a view of the table + the endpoint map
  (`learn` ignores a lower `g`, `forget`), `stepHome{local, foreign, unreachable}`,
  `resolveViaManager` (ONE `ResolveSlot`, counted on the caller), `followRedirect` once, a dead
  holder never dialled (`slot_resolve_dead_redirects`).
- **The plane** (`slots/plane.zig`, Linux): one per backend, created before its conveyor (the
  door is step 0 from the first commit; a flat volume's plane stays unarmed), armed by the appender
  set — tree 0's population loaded, appender 0's page settled (row 5 completed from the entry's
  words with `seq_floor = max(frontier, recorded + 1)`, C14 refuses the mount), re-adoption, the
  native slot, the rotor to `M`, `N_floor` seeded. The cycle hooks: C4b `publishBeforeTail`
  (overflow publications into tree 0, the pre-barrier page refresh), C11 `planPage` +
  `pageLanded`, the cadence's own flight after the tick (never inline). The mint decision behind
  `MetaSet.allocateIno`; an in-process region's `appendPut` enters the door as itself.

## Measured / counted (the venue, `scripts/venue test "sym.bind"`)

- The in-process handover: region 1 hammering region 0's slot 3 takes `touches ≥ N_floor`
  refusals (17 at the cold seed) before the dominated OFFER, then the accept transfers and grants
  at `g + 1`; `slot_handovers` +1, `slot_offers` +1, no acked record lost, `slot_lease_conflicts`
  0, the phases exact-sum; region 0's next write of slot 3 is `SlotBusy`.
- The kill matrix: after the page named `Releasing` (parked at the seam, image copied) the remount
  completes the release from the page's words; after tree 0 went `Unleased` the remount drops the
  entry; both homes agree (table unleased, tree 0 Unleased at the same `g`, no Live entry), the
  first touch re-leases at `g + 1`.
- The page-budget law: 109 leases ⇒ 108 named on the page, the 109th published into tree 0 at its
  live root (`meta_kv_forest_page_publications_demoted` +1); the cadence's LRU returns the region
  to the budget; a remount opens every tree from the home that names it.
- Cursors as floors: a death after the pre-barrier refresh remounts above every minted ino; the
  ledger stamp carries no cursors.
- The migration storm (`routing/migrate.zig`) runs through the folded gate: parked commits resume
  (`routing_gate_parks` moved), every acked record present after the flip and the remount.
- The smoke's KILL MATRIX found a recovery hazard the slot plane made reachable on a solo mount:
  every mint now lands in a first-touch guest tree, and a tree ROOT is the one node no interior
  Put names. A kill before its first durable home replayed nothing that raised the node-seq mint,
  so the next incarnation re-minted the SAME seq into the SAME recycled extent (the dead
  incarnation's allocation was never durable either) and, when the window-mentioned slots were
  re-minted in another order, one slot's fresh root sat on another slot's dead frames — which
  verified as its own (equal seq) and resurrected a stale inode + its block refs (the `rm` then
  left `terminal_frees < bindings_freed` and `df` never returned). `KvTree.create` now floors
  the mint above the claimed extent's residue ceiling (`node.residueSeqCeiling`, one node read
  per tree create); the red test `create floors the root's seq above the extent's residue`
  re-creates into the recycled extent with a reseeded handle and sees no stale record. The same
  pass stopped the native slot's lease root from being opened as a second guest tree over the
  native nodes (the lease's cursor floors the native watermark instead). Three consecutive KILL
  MATRIX runs and the whole smoke are green after it.

## Not in this wave (stated)

- The wire holder side (a joiner's first touch over the wire, `PublishRoots` of a wire lessee,
  `RecallForOffer` as a push verb, the renewal carriage's recall/offer words) — wave C/D with the
  cross-owner intents and membership; the handlers are served, the client of a solo mount never
  calls them.
- The record-level ship behind `SlotBusy` ("ship to the holder") — wave C; here the refusal is the
  typed EAGAIN and, in one process, the requester's touch on the window.
- A two-WRITER smoke needs a second mount joining through the wire: the smoke's two-member stage
  asserts the plane's gauges in the binary (`slot_leases_held ≥ 129`, the mint closure,
  `slot_door_refusals == 0`, the conflict tripwire inside `invariant_tripwires`); the two-region
  handover e2e is the bind test above.
- `slot_tails` past the inline cap spill (the recovery/leave rung); the leave releases every held
  slot through its own transfer first.
