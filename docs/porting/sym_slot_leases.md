# Porting spec — Symmetric metadata, part A: SLOT LEASES — the per-slot lease state machine, acquisition and minting, the COMMIT DOOR, handover (flush-then-transfer), the page-budget overflow law, slot → holder resolution

Source of truth (read-only): `/Users/joberdorf/Source/squeezefs` (abbrev. `R/`).
Target: `/Users/joberdorf/Source/squeezefs-zig` (abbrev. `Z/`), charter §5.5 "Slot acquisition and
minting", "Handover", "Slot → holder resolution" (`Z/docs/charter.md:160–164`), §5.2 (`:120`),
§2 lock order (`:51`), §5.5 death/recovery (`:174`), gauges (`:183`).
Law: decision 0002 (the charter's mechanisms, invariants and derivations are requirements; the
reference's incidental choices are not; no compatibility owed) and 0003 (format v4).

Reference files: `R/docs/design-symmetric-metadata.md` §5.1 (`:254–340`), §5.3.2 (`:411–432`),
§5.3.4 crash matrix (`:440–466`), §5.3.5 (`:467–469`), §5.10 (`:744–766`);
`R/src/slot_lease_core.rs` (1674 — the loom-checked core: `LeaseGate`, `SlotLeaseTable`,
`DominanceWindow`, the derivations, `mint_choice`); `R/loom-models/src/lib.rs` `slot_lease_models`
(`:7011–7278`), `park_gate_models` (`:7428–7510`), the `slot_gate_core` / `slot_cursor_core` models
(`:3865–3990`); `R/src/meta_backend/kv/slot_lease.rs` (994 — the plane, knobs, `DoorPass`, gauges);
`R/src/meta_backend/kv/slot_state.rs` (the tree-0 record, `:1–240`); `R/src/meta_backend/kv/
appender.rs` (page entry `:117–124`, `:170–175`, `:259–268`; the release-word screen `:1410–1638`);
`R/src/meta_backend/kv/backend.rs` (the verbs `:5927–7241`, the page plan `:7470–7731`, the handover
`:7739–8074`, the door `:8212–8360`, the mint policy `:8369–8468`, the cadence `:8473–8636`, the
leave `:8695–`, the cursor page `:11652–11729`, `publish_forest_roots` `:21785–`, `commit_tx`
`:22350–22387`, the open-time settle `:5273–5603`, the reader redirect `:4063–4190`);
`R/src/meta_backend/kv/backend/joined.rs` (the wire requester/holder `:1694–1905`, `:3467–3537`);
`R/src/meta_backend/kv/backend/recovery.rs` (`:1–50`, `:189–236`, `:1533–1631`);
`R/src/slot_holder_cache.rs` (189); `R/src/meta_backend/crossvol_tx.rs` `step_home` (`:2230–2297`);
`R/src/meta_backend/kv/forest.rs` (`PublicationHome` `:71–94`, `:452–546`);
`R/src/meta_backend/kv/node_cache.rs` (`slot_record_frontier` `:2886–2924`); `R/src/park_core.rs`
(`:120–210`); `R/src/env_knobs.rs` (`:345`, `:421–425`); `R/src/meta_ship/tokens.rs` (`:789`,
`:868–873`); `R/src/lib.rs` (`GATHER_XATTR` `:1232–1253`); `R/src/meta_backend/mod.rs`
(`gather_mint_slot` `:3357–3400`); tests in §8.

---------------------------------------------------------------------------
## 0. Where the port stands (what exists, what this spec adds)
---------------------------------------------------------------------------

Exists in `Z/` today — bind to it, do not re-port:

| Z/ item | What it is | Reference twin |
|---|---|---|
| `src/meta/routing/slot_state.zig:1–121` the tree-0 `slot_state:{s}` record — decimal key under one prefix (`keyOf`/`slotOfKey` `:23–35`), `SCAN_START/END` `:39–40`, `Value` 28 B `{version, flags(0), root_addr, root_seq, cursor}` (`:42–86`), total decode | the UNLEASED shape only ("Rung 6 grows the shape behind the version", `:9–10`) | `slot_state.rs:101–128` (two variants, 50/54 B) |
| `src/meta/routing/slot_gate.zig:49–166` `SlotGates`: one in-flight word PER SLOT (`inflight[s]`, 256 KiB), ONE `closed_slot` word, the Dekker pair with SeqCst accesses (`tryEnter` `:75–83`, `exit` `:89–99`, `close` `:122–126`, `isDrained` `:129–131`), register-then-check waits on two `sync.Notify`s (`enter` `:104–119`, `waitDrained` `:137–150`), `Pass{slots[6]}` (`:172–202`) | the migration cutover gate — THE door's shape already (§3.6 below makes it the one mechanism) | `slot_gate_core.rs` (109), `LeaseGate::enter/begin_release` (`slot_lease_core.rs:193–233`, `:325–331`) |
| `src/meta/routing/slot_gate_model.zig:19–111` the Dekker pair as a TSO model with explicit store buffers; both weakenings `expectFails` | the model the door's pair re-uses | loom `slot_gate_*` (`lib.rs:3865–3977`), `a_commit_that_passed_the_door_is_drained_by_the_release` (`:7063`) |
| `src/meta/routing/stamp_cell.zig:29–120` per-slot `inos.InoWatermark` cells created on first touch (`cursorFor` `:92–104`), `installFloor` (`:112–115`), folded into the ledger stamp (`STAMP_MAX_CURSORS = 256`) | the per-slot cursor CELL; its ledger home is retired by §2.5 | `slot_cursor_core.rs` (77: `mint`/`snapshot`/`install_floor` `:50–76`) |
| `src/meta/inos.zig:66–97` `InoWatermark{allocate, reserveRange, snapshot, installFloor}` | the cursor primitive (fetch_add / fetch_max) | `SlotCursor` |
| `src/meta/routing/route_table.zig:59–117` `slotOf`, `effectiveIno` (table-free: slot 0 native, else `(s+1) << 40`), `forestSlotOf`, `slotOfKey` | one slot type; the record → slot law | `record::forest_slot_of_ino`, `page_slot_of_forest_slot` (`appender.rs:682–700`) |
| `src/meta/conveyor.zig:29–52` stage A0–A13; "Guards drop at the terminal outcome BEFORE the ack"; `src/meta/tx.zig:55–80` `GuardSet` co-owned by the queue entry, `QueuedTx` `:337–355` | where the door token rides (§3.3) | `QueuedTx._door` (`backend.rs:19580–19585`), `commit_tx` (`:22350–22387`) |
| `src/meta/dlm.zig:1–40` the 4a stripes, `lockMany` canonical order, `tryLockMany` | the door is taken BEFORE these (§3.4) | — |
| `src/meta/checkpoint.zig:112–117` `COVER_CYCLES_MAX = 8 × PENDING_FREE_FORCE_CYCLES = 64` with its WHY; `checkpoint_cycle.zig:465–466, 582–603` `reusable_upto` moved at `barrierLanded` | the handover's cover loop bound and watermark (§4.6) | `checkpoint::COVER_CYCLES_MAX` (`backend.rs:5932`, `:7772`) |
| `src/meta/guard/claim.zig:30, 41` `HEARTBEAT_INTERVAL_S = 10`, `STALE_TTL_S = 45` | `T_owner` = `T_idle`'s derived default; the renewal BEAT | `CLIENT_STALE_TTL_SECS`, `CLIENT_HEARTBEAT_INTERVAL_SECS` |
| `src/model/explorer.zig:24–31, 125–136` the model contract, `expectHolds`/`expectFails` | every lock-free core here ships as a model | loom |

Missing (this spec): the per-slot lease RECORD table (RAM + its two durable homes), the `Leased`
/ `Releasing` shapes of `slot_state`, the appender-page slot ENTRY (the page itself is `kv_journal_
ledger.md`'s v4 decision), `g`, `seq_floor`, the commit door as a verdict of the one gate, first-touch
acquire, the rotor and the mint policy, the dominance window and the offer, flush-then-transfer, the
cadence (LRU / forced shrink / region release), the page plan + publication homes, the holder cache,
`ResolveSlot`, the knobs, gauges and tests.

---------------------------------------------------------------------------
## 1. The slot lease state machine
---------------------------------------------------------------------------

### 1.1 The RAM record (one per slot, the manager's mirror of tree 0)

The reference keeps the lease state in FOUR structures: `SlotLeaseTable` (a `Mutex<BTreeMap<Slot,
SlotLease>>` plus two indexes `held[holder]`, `offered[to]` — `slot_lease_core.rs:590–595, 640–648`),
`LeaseGate` (three dense atomic bitsets `leased` / `releasing` / `foreign` + `inflight[slot]: u32` —
`:138–155`), `SlotHolderCache` (an `ArcSwap<HashMap>` — `slot_holder_cache.rs:42–53`) and the
region's `leases()` set + the `rotor` vector (`slot_lease.rs:234–237`). The port keeps ONE dense
table (§9 Q2) with these words per slot:

| Word | Type | Meaning | Reference |
|---|---|---|---|
| `state` | `enum(u8){unleased, leased, offered, releasing}` | §5.1.4's diagram | `LeaseState` `slot_lease_core.rs:445–453` |
| `holder` | `AppenderId u32` | valid unless `unleased` (0 = the manager / none) | `SlotLease.holder` `:478–479` |
| `g` | `u32` | the lease GENERATION: `+1` at every grant, never at a replay; strictly monotone per slot; the frame screen's word (`(appender_id, g)` on every bset frame — charter §5.5 "Fencing") | `:480–482`, `acquire` `:710` (`wrapping_add(1)`) |
| `offered_to`, `offer_expires_ns` | `u32`, `u64` | the ONE requester an `Offered` slot names and the stand's end (monotonic) | `:483–486` |
| `last_written` | `u64` | the manager's lease seq at the last release — `prefer: unleased-then-idle`'s rank (0 = never written) | `:487–489`, `release` `:889` |
| `words` | `SlotWords{root: (addr, seq), cursor, extents: u32, seq_floor}` | the four durable words a lease carries + the seq-space floor | `:459–472` |
| `rotor` | `bool` (RAM-only) | granted by a ROTOR ask ⇒ counts against `2 × M`; cleared at release; re-marked at the arm's re-adoption (`mark_rotor`) | `:491–498`, `:1007–1016` |
| `dead` | `bool` (RAM-only; the port adds it) | `releasing` under a DEAD holder — the charter's `Releasing{dead}` (§5.5 "RAM lease table marks the dead slots `Releasing{dead}`"); the reference infers it from the holder's page being `Recovering` (`recovering_lessees`, `slot_lease.rs:360–371`) | §9 Q8 |
| `inflight` | `atomic u32` | the DOOR TOKENS out (commits past the door, not yet terminal) | `LeaseGate.inflight` `:150`, `:189–225` |
| `foreign` | the gate's projection: `state ∈ {leased, offered, releasing} ∧ holder ∉ own regions` | derived, not stored — the reference stores a bitset rebuilt whole at every wire grant/release (`install_foreign` `:270–281`) and marks it AHEAD of the durable write (`mark_foreign` `:295–298`, rolled back by `clear_foreign` `:301–304`) | §9 Q2 |

Indexes the manager needs per HOLDER (`held_by` — the renewal carriage, the page plan; `offered_to` —
the carriage's `offered_slots`) are O(held) off a per-appender list, never a scan of 65 k entries
(`:584–589` "KD-FG-4's no-scan-in-renew law").

### 1.2 The two durable homes

| Home | Record | Written by | Reference |
|---|---|---|---|
| **Tree 0** `slot_state:{s}` | `Unleased{root, cursor, g, slot_tree_extents, last_written, seq_floor}` (50 B in R/: `version u8 ‖ variant u8 ‖ root.addr ‖ root.seq ‖ cursor ‖ g u32 ‖ extents u32 ‖ last_written ‖ seq_floor`) or `Leased{appender_id, g, page_addr, root, cursor, slot_tree_extents, seq_floor}` (54 B) — fixed-size, versioned (`SLOT_STATE_VERSION = 3`), a future version refuses loud | the MANAGER: every grant (`Leased`, grant-time words), every release (`Unleased`), every overflow publication (`Leased` rewritten with the CURRENT root — §5), the recovery (`Unleased`) | `slot_state.rs:45–63, 101–128, 132–239` |
| **Tree 0** `slot_tails:{s} → {g, tails}` | the flushed-leaf log tails the last release recorded, stamped with the `g` they attest; INLINE under the value cap, SPILLED to heap extents above it; written in the SAME control entry as the `Unleased` record, left alone by the grant (a `Leased` slot keeps its previous generation's tails readable — the frame screen's rule 2 reads them while the slot is leased at `g + 1`) | the release / the recovery | `slot_state.rs:18–29`; `manager_release_slot` `:6713–6724` |
| **The holder's appender page** entry `(slot u16, state u8 {Live, Releasing}, g u32, slot_tree_extents u32, root (u64, u64), cursor u64)` = 35 B; ≤ `SLOT_PAGE_BUDGET = (4096 − 314) / 35 = 108` entries, DERIVED from the page's fixed part and the entry width | the HOLDER at every checkpoint (one write per cycle), at the handover's steps 3 and 5, at the leave; a WIRE joiner's page by the manager only while `ckpt_seq == 0` (one writer per page) | `appender.rs:117–124, 170–175, 259–268`; `write_wire_joiner_page_slots` `backend.rs:6282–6341` |

**The two-homes invariant** (design `:299`: "Two durable homes at every instant — never zero"): a
slot's root is named by tree 0 OR by its lessee's page at every instant, and during a handover by
BOTH (the page's `Releasing{root, cursor, g}` entry stands until tree 0's `Unleased` is durable, and
only then does the page drop it). The crash matrix (design `:456–457`): page `Releasing` ∧ tree 0
still `Leased{me, g}` ⇒ ROW 5, the next open of that identity COMPLETES the release from the page's
words (`settle_page_entries_against_tree0` `backend.rs:5547–5587`: `manager_release_slot(r.id, slot,
words{root, cursor, extents from the entry; seq_floor = max(ring frontier, recorded)}, se.g, tails)`);
page `Releasing` ∧ tree 0 `Unleased`/`Leased{other}` ⇒ ROW 6, **tree 0 wins**, the entry is dropped
(`:5588–5599`; C14 ignores `Releasing`). A `Live` entry BELOW tree 0's `g` is stale residue, dropped
and counted `slot_lease_stale_entries` (`:5461–5476`); a `Live` entry at tree 0's `g` under ANOTHER
lessee, or two `Live` attestations of one slot, REFUSE the mount (`slot_lease_conflicts`, C14 —
`:5477–5506`).

Which home is a root's CURRENT publication is a RAM fact per slot: `Publication{root, home:
Tree0 | Page, cursor}` (`forest.rs:71–94`). A root named by neither is UNPUBLISHED and its
`root_floor` clamps its ring's checkpoint tail (`unpublished_root_floors` `:452–461`; the
checkpoint's tail law in `kv_journal_ledger.md`). §5 is the law that moves roots between homes.

### 1.3 Transitions, durable step order, barrier count

| # | Transition | Durable steps (in order) | Barriers | Reference |
|---|---|---|---|---|
| T1 | `Unleased → Leased{q}` (grant: a rotor ask, an explicit `AcquireSlot`, a first touch) | RAM table `g += 1, state = leased, holder = q, rotor` and (wire q) the foreign mark → ONE tree-0 control entry: `Leased{q, g, page_addr, grant-time words}` per slot of the call (+ the grantee's `extent_grant` record gaining the trees' live images, same entry) → RAM install (`install_lease`: raise the lessee ring's seq offset above `words.seq_floor`; adopt the root; extents; cursor floor; frontier reset to the receiving ring's head; `region.add_lease`; gate `grant`) → holder cache `learn` → (wire q) the joiner's page while `ckpt_seq == 0`, the S4 owner table, the manager's tokens on the slot recalled BEFORE the reply | the control entry's (the lane's; `Try` for a wire ask, PARKING pre-admission for the manager's own) | `manager_acquire_slots` `backend.rs:6000–6265`; `install_lease` `:5780–5855` |
| T1' | a failed write ⇒ `roll_back_unwritten_grants`: the RAM entry back to `Unleased` keeping the minted `g` (one ahead of tree 0 — harmless, `g` need only be monotone) and `last_written = 0`; foreign marks cleared | — | `:5886–5900`, `:6155–6174` |
| T2 | `Leased → Offered{to: q, expires}` | RAM only (an offer that dies with the manager simply expires — §5.3.5 `:469`) | 0 | `offer` `slot_lease_core.rs:761–787`; `manager_offer_slot` `backend.rs:6578–6616` |
| T3 | `Offered → Leased` (lapse) | RAM only; `offers_expired += 1`; the gate's `end_release` on each lapsed slot; a wire recall withdrawn with it | 0 | `expire_offers` `:794–821`; `lapse_offers` `slot_lease.rs:719–728` |
| T4 | `Offered → Releasing` (the accept) / `Leased → Releasing` (the cadence, the leave) | §4.6's flush-then-transfer: gate `begin_release` (SeqCst) → table `begin_release` → drain the door → cover cycles → page `Releasing{root, cursor, g}` + barrier → tree 0 `Unleased` + `slot_tails` (one entry, barriered) → RAM drop → page without the slot | 1 (page) + 1 (tree 0 entry) + the cover cycles' (≥ 1) + the final page write's (lands with the next cycle) | `transfer_slot_locked` `backend.rs:7863–8074` |
| T4' | `Releasing → Leased` (abort: the manager refused, the flush failed, the custody census deferred) | RAM only: `abort_release` + gate `end_release`; the offer withdrawn | 0 | `abort_release` `:847–860`; `abort` closure `backend.rs:7941–7945` |
| T5 | `Releasing → Unleased` (`ReleaseSlot` landed) | tree 0 written FIRST (`check_release` verdict → `Unleased` + tails + grant-minus-images as ONE entry) → ring 0 `raise_seq_floor(words.seq_floor)`, the slot's frontier reset to ring 0's head, foreign cleared → holder cache `forget` → table `release` (or `release_reserving` when a standing recall is spent: the offer memo kept until `now + beat + handover_bound`) | the entry's | `manager_release_slot` `:6653–6850` |
| T6 | `Leased{dead} → Releasing{dead} → Unleased` (recovery) | RAM `begin_release(slot, dead)` + foreign cleared (structural class = the manager's) → replay → cycles until no dirty slot node → tree 0 `Unleased{post-flush root, cursor per §2.5, g, extents, seq_floor = max(recorded, dead frontier)}` + tails per slot → RAM `Unleased` LAST (an RAII rollback restores every RAM word and the slot's tree on any error) | the recovery's cycles + the tree-0 entries' | `recovery.rs:18–50, 189–236, 1533–1631`; charter `:174` |
| T7 | `Leased{me}` re-adopted at open (crash-remount of this identity) | no write: tree 0 already names us; `install_lease(fresh = false)` re-applies `seq_floor` and the CURSOR as floors; region 0's re-adopted non-native slots fill the rotor up to `M` and are `mark_rotor`ed | 0 | `arm_slot_leases` `:5280–5316` |

**Barrier accounting of one handover** (design `:300`): the idle arm ≈ 4 barriers + 3 RTTs; the
dominated arm adds a flush of ≤ one ceiling's dirty set. The cold-start `N_floor` seed restates it
(`handover_cold_start_ns = 4 × ewma_barrier + 3 × ewma_ship_rtt`, `slot_lease_core.rs:1332–1336`).

### 1.4 The gate's verdicts (what the one RAM word answers)

`CommitVerdict{Unarmed, Allowed, NotLeased, Releasing}` (`slot_lease_core.rs:90–105`). The CONTENT
verdict read UNDER the node's write lock (`verdict` `:371–387`): `!leased ⇒ NotLeased`; `releasing ∧
inflight > 0 ⇒ Allowed` (a token holder's apply during the drain — counted `slot_door_draining_
admits`, `:380–383`); `releasing ∧ inflight == 0 ⇒ Releasing` (the rogue the belt refuses); else
`Allowed`. The STRUCTURAL verdict (`verdict_structural` `:403–415`): a slot this mount leases is
admitted whether or not a handover is in flight (the flush IS the departing holder's); a slot nobody
leases is admitted on the MANAGER (KD-SYM-2/3) and refused elsewhere; a FOREIGN slot is refused — the
belt behind the sweep's `maintainable_trees` filter. `meta_kv_leaf_lease_refusals` (must-stay-0,
charter `:151`) is the belt both verdicts feed.

### 1.5 The models (what loom asserts; which weakening fails each)

| Model | Threads / steps | Invariant | Weakening (verified RED) | Reference |
|---|---|---|---|---|
| **The door's Dekker pair** `a_commit_that_passed_the_door_is_drained_by_the_release` | committer: `inflight += 1 ; fence ; load releasing`; release: `releasing |= bit ; fence ; load inflight` | if the commit PASSED (read no bit) the release saw `tokens == 1`; a refused door leaves `inflight == 0` (a transient 1 then backed out is ALLOWED — the release waits for the count to fall) | WITHOUT the two SeqCst fences (the sides' accesses SeqCst on their own words, loom models them acquire/release) the commit passes AND the release reads 0 — the flush snapshots without it, the commit lands afterwards | `lib.rs:7045–7096`; the pair `slot_lease_core.rs:193–204, 325–331` |
| **Verdict-inside-the-lock** `a_commit_never_lands_on_a_releasing_slot` | committer: `lock(dirty) ; verdict == Allowed ⇒ push`; holder: `begin_release ; lock(dirty) ; snapshot` | if applied, the snapshot CONTAINS the record; else the snapshot is empty and the verdict reads `Releasing` | reading the verdict BEFORE taking the node lock (check-then-lock): `Allowed` observed, `Releasing` raised, an empty snapshot handed over, the record landed after — applied, never flushed, in no ring the successor replays | `:7098–7152` |
| `racing_acquires_yield_one_holder_and_one_g_increment` | two `acquire(SLOT, 1/2)` | exactly one `Granted{g: 1}`, one `Refused{g: 1, holder = the winner}`; `g == 1` | a serializability PIN of the table's one mutex, not a memory-ordering model (`:7016–7024` stated honestly) | `:7158–7191` |
| `an_offer_expiring_under_an_accept_never_yields_two_holders` | `expire_offers(100)` ∥ `acquire(SLOT, 2, 99)` | `(holder, g) == (1, 1)` after; the accept is `Recall{1, 1}` or `Refused{1, 1}` with the offer in the expired list | pin | `:7198–7234` |
| `a_stale_release_is_refused_and_the_live_one_lands_once` | `release(SLOT, 2, 1, default)` ∥ `release(SLOT, 1, 1, words)` | the stale one `Refused`, the live one `Released`, a replay `Already`; `(words, last_written) == (words, 10)`; the next grant carries `words` at `g = 2` | pin (found the `Already`-for-any-`g ≥` defect) | `:7240–7277` |
| **The park gate** `a_commit_entering_against_the_park_is_held_or_parked_never_lost` | committer `try_enter` ∥ parker `park` | `Admitted ⇒ seen == 1 ∧ inflight == 1`; `Parked ⇒ seen ≤ 1 ∧ inflight == 0`; never `Admitted ∧ seen == 0` | the same Dekker shape (`park_core.rs:120–165`); `release_and_expiry_are_exclusive` pins exactly one edge moves a standing park | `lib.rs:7428–7510` |
| The cutover gate (rung 5) | `Z/slot_gate_model.zig` | never `(entered ∧ drained)` | either fence removed | already in `Z/` |
| The slot cursor `slot_cursor_mints_unique_and_snapshot_covers_applied` | minter `mint ; publish record` ∥ publisher `snapshot` | mints unique; a snapshot taken after synchronizing with an applied record is strictly above its ino | `mint` weakened to load-add-store collides | `lib.rs:3979–`; `Z/inos.InoWatermark` carries it |

**Port:** ONE model `slot_door_model.zig` with THREE threads — committer (`inflight += 1 ; fence ;
load state`), closer (`state = releasing ; fence ; load inflight`), and a second committer whose
apply reads the verdict under a modelled node lock — proving (a) never `(passed ∧ release saw 0)`,
(b) never `(applied ∧ not in snapshot)`; weakenings: each fence, and check-then-lock. The rung-5
`slot_gate_model.zig` becomes this model's two-thread special case (§3.6).

---------------------------------------------------------------------------
## 2. Acquisition and minting
---------------------------------------------------------------------------

### 2.1 `M` at join

`M = clamp(W / (2 × writers_known), 1, MINT_SPREAD)` (`mint_slots_derived` `slot_lease_core.rs:
1310–1312`; `writers_known.max(1)`): `2 ×` keeps half the width unleased for inheritance and churn;
`MINT_SPREAD = 64` is the shipped granularity ceiling (design `:266`). Ties: 1 writer → 64; 512 →
64; 1 024 → 32; 12 500 → 2; 2^20 → 1 (`derivation_sweep_tests.rs:3746–3759`). The explicit knob
wins verbatim clamped to `[1, MINT_SPREAD]` (`mint_slots_in_force` `slot_lease.rs:79–84`). The
census is re-read at every cadence tick and `slot_rotor` follows it (`:8553–8563`).

At the arm (`arm_slot_leases` `backend.rs:5273–5406`): load tree 0's population into the table
(`load_slot_leases` `:5124–`) → settle the pages against tree 0 (§1.2) → re-adopt what tree 0 names
as OURS (no write; the first `M` re-adopted non-native slots are the rotor, `mark_rotor`ed) → the
manager's NATIVE slot `AcquireSlots(0, [native], Try)` (KD-SYM-2) → the rotor topped up to `M` with
`AcquireSlots(0, want = M − rotor_now, [], Try)` → owners published, holder cache refreshed →
`seed_n_floor_inputs` → `arm_as_manager` → the frame fence installed → `arm`.

### 2.2 The rotor and `prefer: unleased-then-idle`

A rotor ask picks `want` slots from the HOSTED set minus the native slot (`hosted_rotor_candidates`
`backend.rs:5015–5038` — walked LAZILY, never a 65 536-entry vector per grant), ranked
`pick_unleased` (`slot_lease_core.rs:1083–1094`): never-leased first (`last_written == 0`), then the
least-recently-written, ties by slot index. The ROTOR CAP: `rotor_held_by(q) + want > 2 × M ⇒
RotorAtCap` (`slot_rotor_cap_refusals`, legal — `backend.rs:6086–6099`); the cap counts ROTOR
grants only, never every non-native lease (the page budget governs holdings — `:6087–6089`).

### 2.3 First-touch acquire with the parking admission

At the door (§3.1) a slot that resolves `Unleased` is acquired by the committer itself — "it is a
user commit" (design `:305`): the manager grants ITSELF with `ControlAdmit::Park` (`backend.rs:
8322–8327`); a joined appender asks `AcquireSlot` over the wire (`joined_acquire_slot` `joined.rs:
1798–1856`). The PARKING admission is taken HOLDING NOTHING, before the verb mutex (`:6015–6052`):
the worst case is one `Leased` put per slot the call can grant (`leased_puts_worst_len` `:5860–
5875`), reserved exactly under the mutex, the remainder released (`HeldAdmission`, every early
return hands the budget back — `:6049–6052`). WHY (Issue 24, `a_first_touch_acquire_parked_for_
ring_space_never_holds_the_verb_mutex` `sym_slot_transfer_tests.rs:3815`): a first touch parked at
ring admission while HOLDING `manager_verbs` blocked the checkpoint task's grant verb, the cycle
that would have freed the ring blocked behind it, and the D1.b escalation was the only exit. A
declared appender's `Park` ask degrades to `Try` loud (`:6026–6031`).

### 2.4 Parent affinity, the ceiling, the overflow, `gather`

`A_max(t) = max(used_leaf_bytes(volume, t) / MINT_SPREAD, node_size)` (`affinity_ceiling_bytes`
`slot_lease_core.rs:1317–1319`; design `:268`): no tree holds more than 1/64 of the volume's CURRENT
leaf bytes, never less than one extent. `used_leaf_bytes` = Σ of the per-slot EXTENT LEDGER
(`SlotExtentLedger.total` `slot_lease.rs:156–169`: every slot tree's images, leaves and interior —
the interior share < 1 % at the shipped fan-out — never the heap's rings/directory/pages/tree 0) ×
`node_size`; the static knob is clamped to `[node_size, heap]` (`affinity_ceiling_in_force` `:55–
65`); the ceiling in force is published `affinity_a_max_bytes`. The cap is SOFT and its input is
DURABLE: `slot_tree_extents: u32` rides every page entry and both tree-0 shapes, maintained from the
lessee's own claims/frees at the SMO context (`SlotExtentLedger.claim/free` `:126–137`), seeded at
open from the page entry / record (`:5511–5516`, `install_lease` `:5823–5832`).

**The mint decision** (`mint_choice` `slot_lease_core.rs:1392–1437`), in order:

1. `Affinity(parent.slot)` iff this mount leases the parent's slot ∧ it is not the native slot
   (native holds control records + `/`'s dentries only) ∧ `parent.tree_bytes < a_max` (`:1400–1404`).
2. Every rotor tree STRICTLY over the cap by ≥ 1 extent (`b ≥ a_max + node_size`, `:1408–1410`;
   exact equality is NOT over — `:1667–1672`): `Overflow` iff `rotor.len() < 2 × M` (`:1411–1413`),
   else `Smallest` (the smallest rotor tree, ties by the round-robin cursor — `:1415–1420`).
3. Else `Rotor(best)`: the most SIGNED headroom `a_max − b` (so once every tree is at/over the cap the
   least-over tree takes the mint and the trees equalize), ties broken by the rr cursor (`:1421–
   1436`).

The overflow arm (`lease_mint_overflow` `backend.rs:8430–8468`): `AcquireSlots(0, want = 1, [],
Park)`; granted ⇒ pushed on the rotor under its one-writer mutex (`rotor_update` `slot_lease.rs:
523–528`), `affinity_ceiling_overflows += 1`; `RotorAtCap` ⇒ the smallest rotor tree. Counting
(`lease_mint_choice` `:8410–8422`): `Affinity ⇒ affinity_mints`; `Rotor | Smallest ⇒ rotor_mints`
(+ `affinity_ceiling_spills` when the parent WAS leased and non-native — the cap bound);
`Overflow ⇒ nothing here` (the overflow arm counts). Closure: `affinity_mints + rotor_mints +
dir_gather_mints ≡ mints` (`slot_lease.rs:321–325`).

**The port's additions (decision 0003 §BV, 2026-10-26; not in the reference).** Ties in steps 2–3
go to the first candidate at or after the rr cursor, cyclically. Beside another active minting
stream (the FUSE caller, else the thread), steps 2–3 run once per MINT WINDOW: the pick skips trees
other streams' windows claim (the over-cap verdict reads every tree) and the stream claims the tree
it took for `windowMints(node_size)` mints or one checkpoint age; step 1 yields to another stream's
window on the parent's tree (`slots/mint_window.zig`). Every mint whose decision took a leased slot is KEPT only if, after the
cursor's `fetch_add`, the slot still names this mount at the decision's generation and is not
releasing — else burned (`mint_burns`), so the closure reads `≡ mints + mint_burns`.

**`gather`** (`GATHER_XATTR = "user.squeezefs.gather"` `lib.rs:1232–1243`; `gather_xattr_opts_in`
— the bool-knob vocabulary `1/true/yes/on`, trimmed, case-insensitive — `:1247–1253`): the ONE
user-settable name inside the reserved `user.squeezefs.` family, admitted BY NAME (`xattr_name_
allowed` `backend.rs:731–745`). The mint arm (`gather_mint_slot` `mod.rs:3357–3400`) runs FIRST on
an armed volume (`pick_mint_slot_for` `:3407–`): `None` (the ordinary policy) when the parent is on
another volume, in the native slot, or carries no opt-in; else the parent's slot "whatever the
affinity ceiling says", `dir_gather_mints += 1`. The holder is this mount when it leases the slot;
an unleased slot is acquired first-touch at the door; a foreign one refuses `SlotBusy` there —
exactly as the parent's own dentry record does for every create into that directory. NEVER automatic
(the checkpoint-directory shape would route every creator's publishes through one holder).

Consequences the design derives (`:272`): a SOLO mount is its volume's whole load, so `A_max` = 1/64
of its own leaf bytes at every instant and its load is divisible into ≥ 64 movable slices at SUBTREE
grain; a job at the operating point is ≈ 1/195 of its volume's load (V = 64) and mints whole under
one rotor slot; `mkdir /<top>` is a ship to volume 0's manager (rare).

### 2.5 Cursors travel with the lease as FLOORS

`cursor = max(tree-0 cursor, page cursor (Releasing or Live), max ino in every in-window ring entry
for the slot, high live inode key of the slot tree) + 1` (design §5.1.8 `:330–336`; charter `:160`
"never re-mint"). As built:

- The cell is `SlotCursor` = `Z/inos.InoWatermark` (`fetch_add` mint, `fetch_max` floor).
- Every INSTALL floors it: the grant's word (`install_lease` `backend.rs:5834–5843`), a re-adoption's
  recorded word (`:5292–5299`), OUR surviving page entry's cursor (`:5518–5533`), the recovery's
  `max(recorded, page, window max + 1)` (`recovery.rs:1558–1581` the window's highest local ino per
  slot — kind ≠ refs, `key[..8] & (2^40 − 1)`).
- The durable homes are the LEASE's: the page entry at every checkpoint and tree 0 at every grant /
  release / overflow publication. **A forest's ledger stamp carries NO per-slot cursors** (§4.4bd,
  design `:1131`: the stamp's `STAMP_MAX_CURSORS = 256` is the FLAT budget and a manager at 281
  leases could neither checkpoint nor release). `Z/stamp_cell.zig`'s fold into the stamp is
  therefore retired under the symmetric plane (§9 Q6): the cell stays, its publication moves to the
  page entry (`write_cursor_page_before_barrier`, §5.3) and the tree-0 record (the cursor law
  `cursor_stale_root` `forest.rs:838`: an overflow slot whose tree-0 record named a cursor below the
  live one is republished — "mints move no root").
- Release words carry the snapshot: `slot_words_now` (`backend.rs:5085–5120`) = `{root = the live
  tree's, cursor = the cell's snapshot, extents = the ledger's, seq_floor = max(the slot's ring's
  stamp frontier, the recorded floor)}`.
- Pin: `a_slot_released_reacquired_with_its_top_inos_deleted_never_remints_an_ino`
  (`sym_slot_transfer_tests.rs:4507`).

### 2.6 The seq-space law (why `seq_floor` is a lease word)

Record seqs are per-key LWW across rings; a handover from a busier to a quieter ring would stamp the
new lessee's same-key writes BELOW the stored ones (shadowed in RAM, skipped at replay — acked-data
loss; design `:304`). So every ring stamps `position + seq_offset`; a release records the departing
ring's frontier (`head + offset`) as the slot's `seq_floor` (`max`ed over releases, `slot_words_now`
`:5111–5113`); a grant / re-adoption RAISES the receiving ring's offset above it (`raise_seq_floor`,
a monotone `fetch_max` — `install_lease` `:5795–5797`); a release to the manager raises ring 0's
(`:6778–6783` — the manager's structural custody of the unleased tree stamps above the departing
ring; its OWN release moves nothing). The offset is durable on the page (`seq_offset` at 306 —
`appender.rs:102`), recovered as `max(page, the window's own stamps)`. Pin: `a_same_key_write_
after_a_handover_to_a_quieter_ring_is_the_newest_value` (`:5099`). The port's `kv_journal_ledger.md`
v4 page carries `seq_offset`; this spec adds the floor word to both tree-0 shapes.

---------------------------------------------------------------------------
## 3. THE COMMIT DOOR and the structural door
---------------------------------------------------------------------------

### 3.1 The door, exactly (`ensure_leases_for_tx` `backend.rs:8212–8341`)

Called at step (0) of `commit_tx` (`:22350–22358`) — BEFORE `build_queued_tx`, BEFORE the enqueue,
BEFORE any ring admission or node lock. Per tx:

1. The DISTINCT slots the tx's slot-tree records name (`is_slot_tree_kind`, `legacy_key_slot` —
   `:8222–8234`; the port: `key.slotTreeKeySlot` over `tx.recs`, deduped into `[MAX_SLOTS]u16`, the
   rung-5 `Pass` bound of 6). No slots ⇒ no door (control records).
2. **The holder-op note INSIDE the door, before any token** (`:8235–8276`): for every slot, `note_
   holder_op(slot, now, T_idle)` unless the tx is a SERVED STEP (`tx.served_step` — a foreign
   requester's work, never `ops_h`); then the SUBTREE credit (§4.2). WHY here: noted after a first
   touch's acquire, the slot read IDLE with its commit's token out — the cadence's LRU release began
   and drained against the very commit the belt then refused (Issue 11, `a_token_holding_commit_
   lands_under_the_release_that_drains_it` `:1132`).
3. Per slot, the loop (`:8277–8339`):
   - `is_leased(slot)` (ours, possibly mid-handover) ⇒ `gate.enter(slot)`: `inflight += 1 ; fence ;
     load releasing` — passed ⇒ the token is held; refused (mid-handover) ⇒ register on
     `handover_done`, re-check `is_releasing`, `slot_door_parks += 1`, PARK (register-recheck-await),
     loop.
   - not ours ⇒ `table.resolve(slot)`: `Holder{h, g}` with `h ∉ own regions` ⇒ return every token
     taken so far, `slot_door_refusals += 1`, **`SlotBusy{slot, holder, g}`** = EAGAIN, the "ship to
     the holder" class (`:8297–8308`); `Unleased` (or ours in tree 0 without the gate bit — the
     arm's window) ⇒ FIRST TOUCH (§2.3), then loop to take the token.
4. Returns the `DoorPass` (the tokens) which the queue entry OWNS to the terminal outcome
   (`QueuedTx._door` `:19580–19585`, beside `_guards` `:19573–19579`); its drop returns each token
   (`leave`: a CAS loop that never wraps below 0 — `slot_lease_core.rs:213–225`) and wakes
   `door_drained` when any slot reaches 0 (`DoorPass::drop` `slot_lease.rs:838–850`).

**The states and refusals, as a table:**

| Slot at the door | Verdict | Count | Errno |
|---|---|---|---|
| ours, `Leased`/`Offered` | token taken, proceed | — | — |
| ours, `Releasing` | PARK on `handover_done`, re-read | `slot_door_parks` | none (bounded by one handover) |
| another appender's (`Leased`/`Offered`/`Releasing{…}`/`Releasing{dead}`) | refuse | `slot_door_refusals` | `SlotBusy` → EAGAIN ("ship to the holder") |
| `Unleased` | first touch: `AcquireSlot` with a PARKING admission (manager) / the wire verb (joiner); `Deferred` ⇒ EAGAIN; then token | `slot_acquires`, `slot_grants` | — |
| unarmed mount | no door | — | — |

The re-dispatch (design `:302`, defects 30/31): a LOCALLY planned namespace op whose first touch is
refused `SlotBusy` learns the holder into its projection and re-runs ONCE through the cross-owner
arm (`redispatch_once_on_slot_moved`; `xv_cross_owner_op_slot_moved_redispatches`); a second refusal
is the application's EAGAIN. The door refuses before ring admission and any node lock, so the re-run
is safe.

### 3.2 The release's half: `begin_release` + drain

`begin_release(slot)`: `releasing |= bit` (SeqCst) then a SeqCst fence (`slot_lease_core.rs:325–
331`) — ordered before the release reads `inflight(slot)` (`:230–233`, its own fence first) AND
before the node locks the flush takes, so a committer inside its own node lock observes it (the
belt). `drain_door` (`backend.rs:8348–8360`): register on `door_drained`, `inflight(slot) == 0 ⇒
return`, else await — bounded by the conveyor's own latency, the last token out wakes it. A token
holder's apply under `Releasing` is ADMITTED (`verdict` `:380–383`): the drain waits for exactly it;
after the drain the door admits nobody, so `inflight` can only be a token holder's.

### 3.3 Composition with the conveyor's A0 admission and the 4a guards

- Order per commit: door tokens (0) → `build_queued_tx` (1) → enqueue + leader-elect (2) → park on
  the outcome (3) (`:22350–22387`). The pass's A2 ring admission and A4 leaf locks come AFTER the
  token; the token and the 4a guards are both co-owned by the queue entry and both drop at the
  TERMINAL OUTCOME before the ack (`Z/conveyor.zig:50–52` "GUARDS DROP BEFORE THE ACK"; the
  reference's fan-out drops `_guards` then `_door`).
- The door is taken HOLDING the op's 4a guards (the op acquired its canonical set before staging;
  the door sits between staging and queueing). A park at the door therefore holds 4a guards — which
  is why the handover's step 0 (§4.6) runs BEFORE `begin_release` and why "no path may hold a 4a
  guard and wait on `handover`" (design `:302` Issue 20; `backend.rs:7851–7862`).
- The first touch's PARKING admission inside the door is the ONE place a committer may park for
  ring space before queueing; it holds no node lock and no verb mutex (§2.3).
- The park gate (`park_core`, `T_self` PARK-AND-RECLAIM — charter `:172`) is consulted by the pass
  before ring admission (`pre_admission`) and its pass rides `QueuedTx._park_pass` (`:19586–19592`)
  — a third Dekker pair of the same shape (§3.6).

### 3.4 The structural door (`manager_acquire_slots` `:6052–6079`; `clear_ring0_window_of_unleased`
`:5927–5988`)

A grant to ANY appender but the manager takes the per-volume SMO mutex BEFORE the verb mutex (the
checkpoint task's own order) — a structural pass holds it for its whole pass, so no manager SMO on
the slot is in flight when the lease moves and no pass can span the grant (its `maintainable_trees`
filter and the belt can never disagree). Under it, ring 0's window is CYCLED CLEAR of the unleased
slots the grant hands out: `for cycle in 0..=COVER_CYCLES_MAX`: the pending set = `slot_record_
frontiers_above(reusable_upto)` ∩ explicit ∩ `¬leased ∧ ¬foreign`; empty ⇒ done; at the bound: tail
unmoved since the start ⇒ `Corrupt` ("a stuck tail is a defect, never a longer wait"); tail moved ⇒
`GrantDeferred` (EAGAIN, `slot_grant_deferrals`, nothing written; on the wire the typed
`ManagerReply::Deferred`); else `checkpoint_cycle(smo, barrier_now = true)`. A wire ask is `Try`, so
nothing parks under the mutex; the cadence's tick waits out one grant. The `foreign` mark goes on at
`table.acquire` ahead of the durable write and off with the rollback. Pin: `a_wire_grant_of_an_
unleased_tree_waits_for_the_sweep_and_clears_its_window` (`:3244`), `the_manager_merges_an_unleased_
slot_tree_and_skips_a_foreign_leased_one` (`:3092`).

### 3.5 The record frontier (the quantity the cover loops wait on)

`slot_record_frontier(slot)` (`node_cache.rs:2905–2909`) = the highest journal position a record of
the slot was applied at in ITS LESSEE'S ring (0 = none since the arm); `reset_slot_record_frontier
(slot, head)` at every install (`:2886–2903`: never `max`ed with the departing ring's positions — a
position in another ring is unreachable by this ring's `reusable_upto`, "64 cycles and an aborted
release, every tick"); `slot_record_frontiers_above(tail)` the grant's clearing set (`:2914–2924`).
Port: one `atomic u64` per slot in the dense table, noted by the pass at apply (A9) for every slot
the batch touched — one store per slot per batch, not per record.

### 3.6 ONE mechanism: the rung-5 cutover gate IS the door

The reference has four copies of one protocol: `slot_gate_core` (migration), `LeaseGate::enter/
begin_release` (the lease door), `park_core` (the park gate), `token_grant_core` (the grant ∥ pass).
`Z/slot_gate.zig` already has the right shape — a per-slot in-flight word + a state word + two
`Notify`s. The port makes the door a VERDICT of that gate:

- `closed_slot: u32` (one closed slot) becomes the per-slot `state` word of §1.1 packed with
  `holder` (`u64 = state u8 ‖ dead u1 ‖ holder u32 ‖ g…` or two words, SeqCst where the pair needs
  it). Migration's "closed" is one more state (`migrating`) — a park, like `releasing`.
- `tryEnter(slot)` returns `enum{admitted, park, refuse(holder, g), first_touch}` from one RMW +
  fence + load; `exit(slot)` is unchanged; `Pass` keeps the slots. The committer's loop (§3.1) and
  the migration engine's `enter` are the same code; `waitDrained` serves both the cutover drain and
  `drain_door`.
- The holder-op note (§3.1 step 2) is the ONE extra RMW the door adds per slot per commit (the
  dominance window's `HolderOps`, lock-free — `slot_lease_core.rs:1162–1208`).
- `routing_gate_parks` and `slot_door_parks` stay distinct counters (one per state the park came
  from); `slot_door_refusals` is the `refuse` arm.
- The park gate: a per-VOLUME state word read at the same point with the same pair (its models'
  invariants restated as the three-thread model's fourth arm) — or kept as `park_core`'s own cell if
  the volume-wide word costs the slot table nothing; either way ONE model, ONE idiom.

---------------------------------------------------------------------------
## 4. Handover end to end
---------------------------------------------------------------------------

### 4.1 The window and the counters

`T_idle = T_owner` (`resolve_t_idle_ms` `slot_lease.rs:88–96`: the knob verbatim, else
`SQUEEZEFS_MEMBERSHIP_LEASE_TTL_MS` = `CLIENT_STALE_TTL_SECS × 1000` = 45 000; port: `claim.STALE_
TTL_S`). The window is TWO half-window buckets: `total = cur + prev` counts the ops of the last
`T_idle/2 .. T_idle` — the SAME window for the holder and every requester (a comparison across unequal
windows was the retired sustain-clause bug; `Buckets` `slot_lease_core.rs:1109–1133`); the half-window
epoch is `now_ns / (T_idle/2)` (`:1238–1240`); a rotation by 1 step moves `cur → prev`, by ≥ 2 zeroes
both.

- `HolderOps` (`:1162–1208`): per slot, lock-free — `note` aligns the epoch by CAS then `cur += 1`; a
  lost rotation race UNDER-counts by one (errs toward serving, never a handover). One bump per
  commit on the lessee's hot path.
- `DominanceWindow` (`:1223–1301`): per slot, a bounded table of `REQUESTERS_PER_SLOT_MAX = 8`
  requesters (`:72–83`: a dominator needs ≥ 2× the holder and the offer names ONE identity, so at most
  one can dominate; 8 equal requesters each hold ≤ 1/8 and none reaches 2× unless the holder is idle,
  where `N_floor` decides and the crowd is aggregate by construction; an evicted requester re-enters
  at 0 and pays at most one more window — the dominator is the most active and never the LRU victim).
  LRU victim = `min(touched epoch, ops)` (`:1263–1273`).

### 4.2 `ops_h` on the SUBTREE — the directory-parent memo (design `:303`, PR 13c F-B2)

The box found the law's exact failure: the holder's live job ran under `job-wA/live/r*` — directories
the `A_max` floor had spilled to the ROTOR — while the requester created 64 names INTO `job-wA`; the
per-slot count saw one own op beside 64 foreign creates and recalled a LIVE holder at `N_floor = 2`.
The law as built: at the door every dentry-bearing commit notes the holder's op on the tx's own
slots AND on the slots of the parent directory's ANCESTORS — `liveness_ancestors(parent)` walks the
routed layer's `dir_parents` memo to the root (`install_liveness_ancestors` `backend.rs:8079–8081`;
the walk `:8257–8275`): the tx's own slots skipped, slots this mount does not lease skipped (a foreign
slot's ops are its holder's), an ancestor on ANOTHER volume credited on THAT volume's plane
(`note_subtree_holder_op` `:8087–8099`), a memo MISS ends the walk and credits nothing (the shipped
per-slot count). The memo is a HINT with two error directions: a miss under-credits (safe for "never
recall a live holder"); a STALE hit over-credits (the memo has no TTL, bound `dir_entry_capacity`) —
the wrong direction for "a bursty requester still reclaims from a trickle holder". Root-ward
consequence: a directory near `/` whose holder is live anywhere below it is never handed over by
DOMINANCE (the striping arm is that shape's remedy); the IDLE arm is unchanged. Cost: one memo `get`
per ancestor level per namespace commit. Pins: `a_holder_live_below_a_directory_is_never_recalled_by_
a_burst_into_it` (`:2011`), `…_on_another_volume_keeps_the_directorys_slot` (`:2144`),
`an_unarmed_mount_feeds_the_directory_parent_memo_nothing` (`:2235`).

### 4.3 The offer decision (`note_slot_ship` `backend.rs:8115–8197`; `note_ship` `slot_lease_core.rs:
1247–1295`)

Evaluated AT EACH SERVED SHIP (`xv_serve_step` notes every ship it applies; the requester = the
shipping mount's appender id off its member identity, else the child's slot for an insert — design
`:301`). `Serve` on an unarmed mount or a slot this mount does not hold. `ship_ns` (the served wall)
folds `ewma_ship_ns` (`fold_ship_ns` = `ewma − ewma/8 + sample/8`, the first sample seeds —
`slot_lease.rs:597–605`); the cold-start seed is retried until the always-on tables answer
(`:8140–8142`). Then `ops_h = holder_ops(slot, now)` and the rule:

`ops_q ≥ N_floor ∧ ops_q ≥ 2 × ops_h` ⇒ `OfferIdle{q}` iff `ops_h == 0`, else `OfferDominated{q}`
(`:1286–1293`); else `Serve`. `N_floor = max(2, ceil(ewma_handover_ns / ewma_ship_ns))` (`n_floor`
`:1324–1326`; `ewma_ship.max(1)`). Cold start: `ewma_handover = 4 × ewma_barrier + 3 × ewma_ship_rtt`
with `ewma_ship` = the S8 `meta_ship_phase_ns.rtt` mean when a verb has shipped, else one journal
write's round trip (`seed_n_floor_inputs` `slot_lease.rs:538–572`); published `slot_offer_n_floor`.
There is NO sustain clause and NO immunity clause (design `:298`).

Then (`backend.rs:8163–8194`): in the slot's COOLDOWN ⇒ `Serve` (the requester-side never-thrash
valve: `recall_cooldown_from(explicit, T_idle)` = `T_idle × RECALL_COOLDOWN_WINDOWS (8)` =
360 s shipped — `tokens.rs:789, 868–873`; set at the accept on the NEW holder's plane, `note_
handover` `slot_lease.rs:498–503`); else the OFFER: the manager's `offer(slot, holder, to,
expires)` (`Busy` = a second dominating ship before the accept, `slot_offers_busy`, legal); a JOINED
holder's travels as the wire `OfferSlot` (`joined_offer_slot` `joined.rs:1786–1796`). Counted
`slot_offers_idle` / `slot_offers_dominated`.

The offer's STAND (`offer_expiry_ns`): in-process `offer_stand_ms = clamp(min(beat, T_idle/3), 1)`
(`slot_lease_core.rs:1342–1344`); on the wire `wire_offer_stand_ms = 3 × beat + handover_bound` with
`handover_bound_ms = 2 × ewma_handover_ns` (`:1354–1359`; `slot_lease.rs:734–741`) — the requester
learns on its renewal (≤ 1 beat), its accept becomes a RECALL the holder learns on ITS renewal
(≤ 1 beat), the holder transfers, the requester re-acquires on its next renewal (≤ 1 beat). §9 Q10.

### 4.4 Measured (the fleet, `R/.benchmarks/2026-09-19-sym-acceptance.md`)

| Quantity | Value | Where |
|---|---|---|
| `slot_handover_phase_ns` total (idle arm, `sym-foreign-touch`) | **5.5–7.8 ms** = flush 1.9–4.1 / page 0.15–0.18 / tree 0 3.5 | `:5320`; 7.48 / 6.67 ms = flush 3.99 / 2.04, tree 0 3.36 / 4.49, page 0.13 at `:158`, `:1611–1612`; 17.94 / 17.59 ms (flush 13.8 / 14.1) on another run `:158` |
| the dominated arm's first measured handover | 13.4 ms = flush 9.4 / tree 0 3.8 / page 0.14 | `:1034` |
| `N_floor` | design estimate ≈ 50 (5 ms ÷ 100 µs, `:298`); measured ≈ 18 after the first handover (5.5 ms ÷ ≈ 0.3 ms ships, `:5324`); 2 → 23 after the box's first handover fed the EWMA (`:1034`) | — |
| idle tree moved after | 4–5 bursts of 64 touches at the 10 s beat (238 / 320 ships, 1–2 offers), 0.016–0.020 handovers/s | `:1611–1612` |
| LIVE law | 192 / 384 touches, 0 handovers | `:158`, `:1611` |

### 4.5 The requester protocol

- **In-process** (the manager's own regions): `manager_acquire_slot(q, s)` (`backend.rs:6442–6529`):
  `Offered{to: q, now < expires}` ⇒ THE ACCEPT — under the `handover` mutex `release_slot_handover_
  locked(holder, s)` then `grant_one_slot(q, s)` (the release AND the grant under ONE hold, so a door
  parked on the slot wakes only once the requester holds it and reads `SlotBusy`, never a first-touch
  re-acquire by the departing holder); `handover_done.notify_waiters()`; `slot_handovers += 1`;
  `fold_handover_ns(wall)`; `note_handover(s, now)` (the cooldown). A lapsed offer ⇒ `lapse_offers`
  + `Refused{holder}`.
- **Wire** (`joined.rs`): the offeree learns the offer on its renewal grant's `offered_slots`
  (`carriage_for` `slot_lease.rs:759–808`); `joined_accept_offers` (`joined.rs:3511–3537`) issues
  `AcquireSlot` per offered slot; the manager answers `Refused{holder}` NOW and records a RECALL for
  the wire holder (`note_recall` `slot_lease.rs:688–700`, `slot_recall_notices`) that rides the
  HOLDER's next renewal as `slot_release_notices` (`backend.rs:6471–6489`); the holder's
  `joined_act_on_release_notices` (`joined.rs:3467–3503`) runs `release_slot_handover` for each; the
  release that SPENDS a standing recall is the handover the manager counts (`manager_release_slot`
  `:6791–6819`: `release_reserving` keeps the offer memo until `now + beat + handover_bound` so the
  requester's re-acquire lands at `g + 1` instead of the slot standing `Unleased` until somebody's
  first touch); the requester's next `AcquireSlot` is granted and it sets the cooldown on ITS plane
  (`joined.rs:3519–3529`). Pins: `an_accepted_offers_slot_lands_at_the_wire_requester_off_its_next_
  carriage` (`sym_n_daemon_tests.rs:16312`), `an_offer_that_lapses_under_a_standing_recall_is_
  expired_once_never_also_a_handover` (`:16442`), `a_wire_holder_releases_a_slot_on_the_managers_
  notice_and_accepts_an_offer` (`:7950`).
- **The receiver's install** (`install_wire_grant` `joined.rs:1694–1767`): under ITS SMO mutex the
  tree is re-read at the granted root (`adopt_transferred_slot_tree` — the cross-daemon barrier; the
  cached images dropped, the root installed writer-legal) BEFORE any RAM word names this mount the
  holder (F-R4: a read landing between the word and the adoption was served the un-adopted
  projection's absence — `ENOENT` for a directory that existed) → `table.load(Leased{own, g,
  words})`, `mark_rotor` if a rotor grant → `clear_foreign` → `install_lease(fresh = true)` → the
  tree's live images `transfer_in` to the joiner's RAM grant (the manager's entry moved them into
  its `extent_grant` record) → holder cache `learn`; `slot_grants += 1`.

### 4.6 Flush-then-transfer, step by step (`transfer_slot_locked` `backend.rs:7863–8074`)

Preconditions: the `handover` mutex (one handover at a time per volume; taken with NO 4a guard and
NEVER under the SMO mutex — `:7851–7862`), the slot's custody mark held (`defer_handover_for_custody`
— a writer holding custody of a file in the slot ⇒ `HandoverDeferred`, retryable, the recalled writer
releases within one beat — `:7818–7839`); the manager's native slot is non-transferable while it
holds the role (`:7887–7893`).

| Step | Act | Reference |
|---|---|---|
| 0 | DRAIN the slot's RAM-parked pending-times refinements (the SETATTR-echo / write-times map) while the door still admits them; best-effort; a refinement whose slot another appender leases is dropped and counted (`meta_kv_times_echo_foreign_dropped`) — before it one such ino wedged the manager's every drain | `:7893–7906`; design `:302` defect 31 |
| 1 | `gate.begin_release(slot)` FIRST (no new commit passes the door; SeqCst) → `table.begin_release(slot, me)` → `g` (a refusal ⇒ `end_release`, `Busy`); a slot not `Leased`/`Offered` under me ⇒ `Busy` ("a concurrent release or transfer took it") | `:7907–7940` |
| 1b | `drain_door(slot)`: every admitted commit at its terminal outcome — apply included — before the flush snapshots | `:7947`, `:8348–8360` |
| 2 | `flush_slot_clear_of_region`: `for cycle in 1..=COVER_CYCLES_MAX { checkpoint_now(); tail = region.reusable_upto; done iff tail ≥ slot_record_frontier(slot) ∧ root not in unpublished_root_floors }`; at the bound ⇒ `Corrupt` ("a stuck tail is a defect, never a longer wait"). The cycle's own page write names the slot `Releasing` (the page plan's first class) and notes the root page-published | `:7767–7804` |
| 2b | the TOKEN half: every read token this holder granted on the slot's objects is recalled and waited for (the door is closed, so no commit of ours can mutate them again; the next lessee's commits recall at ITS plane alone) | `:7954–7961` |
| — | `words = slot_words_now(slot)`; `tails = leaf_tails(tree)` (every reachable leaf: resident off RAM, evicted by ONE extent read scanned and dropped — `:7243–7260`) | `:7963–7970` |
| 3 | the PAGE with the slot `Releasing{root, cursor, g}` via `prepare_page_entries(releasing = [slot])` (publish-before-drop, §5.2) → `write_region_page` → barrier → `note_page_entries_published` | `:7971–7994` |
| seams | `TEST_HANDOVER_HOLD_AFTER_PAGE` (die here — row 5), `TEST_HANDOVER_PARK_AFTER_PAGE` (park here — the door contract's window) | `:7995–8015`, `:549–575` |
| 4 | TREE 0: `release_slot_durably(me, slot, words, g, tails)` — the manager's `manager_release_slot` (ONE control entry: `Unleased` + `slot_tails` + the grant record minus the trees' images; barriered by its lane) / a joiner's wire `ReleaseSlot` (screened, §4.7) | `:8016–8026`; `joined.rs:1883–1905` |
| seam | `TEST_HANDOVER_HOLD_AFTER_TREE0` (die here — row 6) | `:8027–8033`, `:622` |
| 5 | RAM first: `region.drop_lease`, `gate.revoke` (clears `leased` and `releasing`), `dominance.forget`, `extents.forget`, the rotor without it (`rotor_update`), the guest cursor cell removed; then the PAGE without the slot (`prepare_page_entries([])`) | `:8034–8056` |
| — | `phases.record(flush, page, tree0, 0)`; `fold_handover_ns(flush + page + tree0)` (the HOLDER's measured cost feeds ITS `N_floor`); one INFO line | `:8056–8073` |

Abort at any step before 4: `abort_release` + `end_release` (the slot accepts commits again, the
parkers wake). `Deferred` on a moving tail: the HANDOVER's loop answers `Corrupt` at the bound (it has
no requester to defer to — the tail-moved vs unmoved split belongs to the GRANT's clearing, §3.4);
the charter's "`Deferred` on a moving tail" is the grant side. §9 Q12 recommends the handover loop
take the same split (abort as `HandoverDeferred`, retried by the cadence) instead of `Corrupt`.

### 4.7 The wire release's screen (`screen_release_words` `appender.rs:1556–1597`)

Every word of a wire `ReleaseSlot` is screened against DURABLE / derived state BEFORE any RAM or
durable effect (design `:307`, "bounded codec = bounded execution"): `seq_floor` STRICTLY above the
grant's recorded floor and ≤ `release_seq_floor_bound = max(page.seq_offset, granted_floor_max + 1)
+ page.head_hint + 2 × ring_len`, capped at `SEQ_FRONTIER_SANE_MAX = u64::MAX / 2` (`:1420–1445`;
a page no checkpoint wrote — `ckpt_seq == 0` — bounds by the cap alone); `cursor ≥ recorded ∧ ≤
GUEST_NS_BASE`; `extents ≤ total_extents`; `root` the recorded one verbatim, or a node-aligned heap
address (`root_extent_of` `:1535–1545`) whose extent the appender's `extent_grant` record holds AND
(I/O, the caller's) whose node decodes with the word's incarnation stamp and names the released slot
(`node_records_slot` `backend.rs:6388–6393`). A word outside its bound ⇒ `STATUS_REJECTED`
(`manager_verb_rejected`), nothing written; `g` stays the witness check's (`STATUS_REFUSED`). The
frame's `appender_id` must name a `Live` directory page and never one of this mount's own regions
(`wire_appender_page` `:6352–6380`); an offer's `to` must be known and not the holder (`:6623–
6647`). Pins: `a_wire_releases_slot_words_are_screened_before_any_effect` (`:3423`), `manager_wire_
bounds_tests::a_release_frames_slot_words_are_screened_against_durable_bounds` (`:153`).

### 4.8 The idle arm, LRU release, forced shrink, region release (`slot_lease_cadence` `backend.rs:
8538–8636`)

The cadence runs on its OWN single-flight task after every checkpoint tick (`spawn_slot_lease_
cadence` `:8500–8530` — "because a release waits on admitted commits and a commit parked at ring
admission needs the TICK, the cadence never runs inline in the tick", design `:305`); a no-op before
the join, read-only, or non-writer.

1. `lapse_offers(now)` (T3).
2. **Forced shrink**: `M` re-derived from the census (`appenders_known`, or the test seam);
   `rotor.len() > M` ⇒ the IDLE rotor slots (`holder_ops == 0` over the window) sorted by extents
   ascending, released one by one (`release_for_cadence` = the handover's own sequence to nobody;
   `Busy`/`HandoverDeferred` = "not this tick") until the rotor is at `M`; `slot_forced_shrinks`.
   The rotor is the mount's OWN region's (a joiner shrinks its own — Issue 10). Pin: `forced_shrink_
   releases_idle_rotor_slots_down_to_the_derived_m` (`:4601`: 64 → 32 past 512 writers).
3. **LRU release**: a region holding more than `SLOT_PAGE_BUDGET` slots releases idle NON-rotor,
   non-native slots (sorted by slot) until at the budget; `slot_lru_releases`.
4. **Region release**: a declared region (never region 0) with no lease left, `head == reusable_upto`,
   no pass inside and no window in flight goes `Free` (`release_region`); `slot_region_releases`. Pin
   `a_declared_region_is_released_when_its_last_slot_goes` (`:4666`).

**The idle arm** of the handover is NOT the cadence: it is the dominance rule at `ops_h = 0`
(§4.3) — an idle tree moves only to a requester that shipped ≥ `N_floor` into it ("cheaper to serve
a touch as a ship ≈ 100 µs than to hand over 5–200 ms", design `:298`).

**The leave** (`release_leases_at_leave` `:8695–`): the page with EVERY held slot `Releasing` +
barrier FIRST (a crash past this leaves attestations the next open drops by row 6, never `Live`
entries a later lessee's page would contradict), then every slot `Unleased` + tails in control
entries CHUNKED by the entry cap (108 complete tails sets do not fit one 128 KiB entry — `:8687–
8693`; each chunk moves ITS trees' images out of the grant record), then the RAM plane. Pins
`a_leave_crash_history_remounts_without_a_custody_conflict` (`:5431`), `a_leave_moves_the_released_
trees_images_out_of_the_regions_grant` (`:5313`).

---------------------------------------------------------------------------
## 5. The page-budget overflow law
---------------------------------------------------------------------------

### 5.1 The law (design `:308`; `publish_forest_roots` `backend.rs:21785–`)

A region holding MORE slots than its page names (a burst of first-touch acquires, none idle yet for
the LRU) has roots its page cannot publish; an unpublished root is a FLOOR, so before the law the
ledger tail pinned for good and the clause-b audit fail-stopped the volume after 8 barriered cycles.
The overflow roots ride TREE 0: the cycle's publication (BEFORE the flush pass and the page write)
rewrites each overflow slot's `Leased` record with its CURRENT root (lessee, `g`, cursor, extents,
`seq_floor` kept) — the record the next open already falls back to when the page names no entry
(`:21796–21809`). The filter (`:21834–21847`): a slot is published as `Unleased` only when the RAM
table says `Unleased` ∧ not leased ∧ not releasing; an OVERFLOW slot is published as `Leased` unless
releasing; a slot leased by anyone (a foreign lessee, `Releasing{dead}`) is NEVER published
`Unleased{g: 0}` over a live generation. The cursor law adds an overflow slot whose tree-0 cursor fell
behind the live one (`cursor_stale_root`, `:21848–21863`). A wire lessee ships its overflow as
`PublishRoots` (`joined_publish_overflow_roots` `joined.rs:2673`; `slot_roots_shipped` / `slot_roots_
published_for_lessees`; the manager screens each word with `screen_publish_root_words` — the release
screen MINUS the seq-floor clauses, `appender.rs:1609–1638`).

### 5.2 Homes and the ONE page writer's ranking (`page_plan` `backend.rs:7479–7588`)

A publication remembers its HOME (`PublicationHome::{Tree0, Page}` `forest.rs:71–79`). The
`sym-storm` loss (§4.4af): the page named the 108 LOWEST slots by page slot, so a later first touch of
a LOWER slot pushed a PAGE-published slot off the page with its mark intact — named nowhere durable,
the lessee's death recovered it at tree 0's grant-time root (15 fsynced files lost). Now every page
write of a leased region takes its entries from `prepare_page_entries` (`:7613–7635`), ranked by what
a slot LOSES if dropped, each class in page-slot order, the budget cutting the tail:

| Rank | Class | Why |
|---|---|---|
| 0 | the entry MID-HANDOVER (the caller's `releasing`, or the table's `Releasing` state) | the two-homes law of the release |
| 1 | `page_homed` — a page-homed publication at ANY root (at the live root the page is the root's only home; at an older root it is the only home of every record flushed UNDER that root, which the live root's floor no longer covers — Issue 12) | dropping it loses acked records |
| 2 | UNPUBLISHED, last home tree 0 (its floor keeps every record since tree 0's root in the window; naming it lifts the floor) | — |
| 3 | tree 0 names its current root; the NATIVE slot (the fixed ledger's root — safe off the page) | — |

Entries past the cut: `off_page`; the page-homed among them (`off_page_homed`) are PUBLISHED INTO
TREE 0 BEFORE THE PAGE DROPS THEM (`publish_roots_before_drop` `:7643–7710`): `demote_page_
publications` (the floor re-armed at `max(root floor, the region ring's reusable_upto)`, counted
`meta_kv_forest_page_publications_demoted` — `forest.rs:525–546`) → ONE control entry rewriting each
`Leased` record with the live root (the manager; `TryCheckpoint` admission inside a cycle, `Try` from
the handover/leave) / `PublishRoots` awaited (a joiner) → `note_published_with_cursor`. A
publication that cannot land FAILS THE PAGE WRITE, never the invariant — the previous image, which
named the slot, stands (`:7607–7609`). A wire joiner's page before its first own checkpoint is cut at
the 108 lowest slots — safe because every word is the TABLE's (tree 0's own `Leased`), so no entry is
a root's only home (`:6324–6334`). Pins: `a_page_published_root_pushed_off_the_page_by_a_lower_first_
touch_rides_tree_zero_before_the_lessee_dies` (`sym_n_daemon_tests.rs:9497`), `releasing_an_overflow_
slot_of_a_page_full_region_publishes_the_slot_it_pushes_off_first` (`:9735`), `a_first_touch_between_
the_cycles_publication_and_its_page_write_evicts_no_page_homed_root` (`:9923`, seam
`TEST_CHECKPOINT_PARK_BEFORE_PAGES`), `a_root_split_in_the_parked_cycles_flush_pass_never_drops_the_
slots_page_home` (`:10102`).

### 5.3 The cursor rides page 0 BEFORE barrier #1 (`write_cursor_page_before_barrier` `backend.rs:
11652–11729`)

The manager's tail rides the ledger record and the cursors of its page-named slots ride page 0,
which the cycle wrote AFTER that record — a death between the two left every page-named slot's
cursor one cycle stale while the record's tail had passed the mints: the successor reopened the slot
at the page's word and MINTED OVER LIVE INOS (§4.4bh: `raw 104 ≤ 115`). Now: a CURSOR-ONLY refresh of
the slots the page ALREADY names (the previous image's entries with each cursor raised to its live
cell — no page plan, no publish-before-drop, no control-entry admission, because a checkpoint-class
refusal in this window would abort the cycle BEFORE its record where the post-record write's refusal
leaves the record landed and the tick retrying — §4.4bj), written before barrier #1, the bitmaps'
own law; a cycle whose page-named cursors did not move writes nothing; `meta_kv_cursor_page_writes`.
A slot the page does not yet name rides the post-record path (its root is unpublished until a page
names it, so its floor keeps its mints in the window, or tree 0 names its root AND cursor). A joiner's
page names its own tail and cursors in ONE write, so the window is region 0's alone. Pins `a_page_
named_slots_cursor_is_durable_before_the_record_whose_tail_passes_its_mints` (`:5626`), `a_page_
publish_refusal_never_reaches_the_cycle_ahead_of_its_ledger_record` (`:5720`,
`TEST_PREPARE_PAGE_ENTRIES_REFUSE_ONCE`).

Port: the checkpoint cycle in `Z/checkpoint_cycle.zig` gains the step between the flush pass and
barrier #1: `writeCursorPageBeforeBarrier` (region 0 only), then after the ledger record the page
plan's full write.

---------------------------------------------------------------------------
## 6. Slot → holder resolution
---------------------------------------------------------------------------

### 6.1 `SlotHolderCache` (`R/src/slot_holder_cache.rs:1–189`)

Shape: three `ArcSwap<HashMap>`s per volume — `map: slot → SlotHolder{appender_id, g}` (`:33–39`,
`:45`), `endpoints: appender_id → Arc<str>` (the wire endpoint its owner service listens on; `:47`,
`set_endpoint`/`endpoint` `:60–71`; `appender_at_endpoint` = the lowest id at an address — one page
identity, one claim-set entry — `:75–85`), `members: appender_id → member id` (the liveness key the S6
owner answers for; `:48–52`, `:89–101`). Read latch-free (one load). Fed: `refresh(entries)` whole
from tree 0's lessee population at the arm and at a reader's epoch poll (`:109–115`); `learn(slot,
holder)` at every grant the manager writes — a LOWER `g` than cached is a stale view and ignored
(`:119–127`); `forget(slot)` at every release (`:130–137`). Invalidation: `g` changes observed in tree
0 (the `learn` rule), the grant's `dead_members`, a release (`forget`), and a `NotHolder` redirect's
answer learnt into the projection.

Endpoint PUBLICATION: each writer's listener is published into its CLAIM-SET entry (charter `:164`
"endpoints published into each writer's claim-set entry"); the join ladder's rung 7 binds every
`Live` appender's endpoint (`sym_join::bind_live_appender_endpoints`); a later joiner's arrives with
the wire `JoinAppender`; `bind_holder_endpoint_on_demand` fills a miss off durable state once
(`step_home_bound` `crossvol_tx.rs:2289–2297`).

### 6.2 `step_home` (`crossvol_tx.rs:2230–2282`)

`StepHome{Local, Foreign{holder, endpoint}, Unreachable{holder}}`. `foreign_holder_of` reads the
LEASE TABLE (not the gate — "the gate says some region of this mount, and a declared region's slot is
another appender's for every cross-owner decision", `:2248–2251`): `Holder{h, g}` with `h ≠ own` ⇒
foreign; then the endpoint from the cache; none ⇒ `Unreachable` (EAGAIN-class: the intent stays open,
the roll-forward cadence retries — `unreachable_error` `:2311–2322`). `spans_foreign_slot(inos)` is
the arms' "this op is cross-owner" predicate (`:2302–2307`).

### 6.3 `ResolveSlot`, `NotHolder` once, `HolderDead`

- `ResolveSlot{s}` to the volume's manager (`manager_resolve_slot` `backend.rs:7209–7225`): the
  table's `Resolved{Unleased{g} | Holder{appender_id, g}}`; counted `slot_resolve_rpcs` on the SERVER
  — never `dlm_rpcs`, which keeps its S4 meaning; a mount whose cache is warm never issues one. The
  ship path's stale-view fallback (`reresolve_slot_holder` `joined.rs:1208` — one wire `ResolveSlot`,
  the answer learnt into the projection, a bounded retry `xv_cross_owner_guard_stale_reresolves`).
- `NotHolder{current}` (`token_plane.rs:289`, `:991–994`): a holder (or the manager) answers it for
  an object whose slot it does not lease; a READER follows it ONCE at the holder the redirect named,
  through its per-holder binding (`backend.rs:4131–4160`, `note_reader_redirect_followed`); a
  JOINER refreshes its control projection and dials the holder the manager named regardless (its
  word is fresher than any ledger record — a grant is a ring-0 entry whose root reaches the ledger
  only at the next cycle; `:4164–4190`). A served step whose "this mount does not lease the slot"
  verdict at the travelling guards IS the re-resolve's trigger (design `:301`).
- `HolderDead{holder}` (`token_plane.rs:336`, `:991`, `:1217`): a redirect to a lessee the membership
  owner lists DEAD is answered `HolderDead`, never a redirect — the retryable class (`slot_resolve_
  dead_redirects`), NEVER dialled (the storm leg's reader parked 24 minutes behind a dead holder's
  address — "ONE bounded dial then tree 0's word is the law", `backend.rs:4063–4071`); the dead
  holder's fallback is the MANAGER's word asked once (`manager_word` `:4073–4093`).

---------------------------------------------------------------------------
## 7. Knobs, gauges, constants
---------------------------------------------------------------------------

### 7.1 Knobs (`R/src/env_knobs.rs`)

| Knob | Kind / range | Default | Class (port) | Reference |
|---|---|---|---|---|
| `SQUEEZEFS_SYMMETRIC_META` | bool | `1`; `=0` on a stamped volume REFUSES the writable open; inert on `--single-writer` | **delete** — under v4 every volume is a forest and the posture is `-o ro` vs writable; the knob names nothing a writer could run under (its own doc says so) | `:422`; charter `:147` keeps the refusal law — the port's open refuses a non-forest superblock, not a knob |
| `SQUEEZEFS_SYM_MINT_SLOTS` | int `1..=MINT_SPREAD (64)` | derived `M` | `ab_lever` ("explicit wins verbatim — the measurement lever"; the control arm is the derivation) | `:423`; `resolve_mint_slots` `slot_lease.rs:42–47` |
| `SQUEEZEFS_SYM_AFFINITY_MAX_MB` | int `1..=2^20` (BYTES_MAX in MiB) | derived dynamic `A_max(t)` | `ab_lever` (the divisibility lever; clamped at open to `[node_size, heap]` — NOTE: an in-process clamp of an admitted value; the port's gate refuses below `node_size`/above the heap instead, §9 Q13) | `:424`; `:55–65` |
| `SQUEEZEFS_SYM_T_IDLE_MS` | int `1000..=MS_MAX` | derived `T_owner` (45 000) | `ab_lever` | `:425`; `:88–96` |
| `SQUEEZEFS_DLM_RECALL_COOLDOWN_MS` | int `1..=MS_MAX` | derived `8 × T_idle` | `ab_lever` (the S10 valve's lever; "the valve itself has no knob") | `:345`; `tokens.rs:868–873` |
| `SQUEEZEFS_TEST_SYM_APPENDER_SLOTS` | harness `<id>:<slot>[,…][;…]` | — | **not carried** (§9 Q11: the in-process declared-region seam) | `:421` |
| `SQUEEZEFS_TEST_HANDOVER_*`, `TEST_CHECKPOINT_PARK_BEFORE_PAGES`, `TEST_PREPARE_PAGE_ENTRIES_REFUSE_ONCE`, `TEST_CONVEYOR_HOLD_POST_DOOR`, `TEST_WIRE_GRANT_PARK_MID_INSTALL` | process statics (`AtomicBool`/`AtomicU64` + a `Notify`) | off | `test_seam` rows or per-instance seam fields (0002 row 157's shape): `SQUEEZEFS_TEST_HANDOVER_HOLD = after_page \| after_tree0 \| park_after_page`, `SQUEEZEFS_TEST_CHECKPOINT_PARK = before_pages`, `SQUEEZEFS_TEST_PAGE_PUBLISH_REFUSE_ONCE`, `SQUEEZEFS_TEST_CONVEYOR_HOLD = post_door`, `SQUEEZEFS_TEST_WIRE_GRANT_PARK = mid_install` | `backend.rs:395–430, 512, 549–575, 622`; `joined.rs:1734` |
| `SQUEEZEFS_MEMBERSHIP_LEASE_TTL_MS` | — | 45 000 | **not carried** (0002 row 188: `T_owner` IS `claim.STALE_TTL_S`) | `slot_lease.rs:91–94` |

### 7.2 Gauges (the Slot-lease family — `SlotLeaseStats` `slot_lease.rs:920–994`; charter `:183`)

| Gauge | Kind | Meaning |
|---|---|---|
| `slot_leases_held` | Gauge | `gate.leased_count()` |
| `slot_rotor` | Gauge | `M` in force |
| `slot_acquires`, `slot_grants` | ShardedCounter | `AcquireSlot(s)` verbs; grants written |
| `slot_offers`, `slot_offers_idle`, `slot_offers_dominated`, `slot_offers_expired`, `slot_offers_busy` | counters | offers made / by arm / lapsed / `Busy` (a second dominating ship before the accept — legal) |
| `slot_handovers` | counter | releases that spent a recall + in-process accepts |
| `slot_handover_phase_ns{flush, page, tree0, grant, total}` | exact-sum histogram | `HandoverPhases` `:172–206`; `total ≡ flush + page + tree0 + grant` |
| `slot_ships` | counter | served ships evaluated |
| `slot_lru_releases`, `slot_forced_shrinks`, `slot_region_releases` | counters | the cadence's three arms |
| `slot_lease_conflicts` | **Tripwire** (must-stay-0) | two live attestations of one slot (C14's live face) |
| `slot_lease_stale_entries` | counter | `Live` entries below tree 0's `g` dropped at the arm |
| `slot_resolve_rpcs` | counter | `ResolveSlot` served (0 on a solo mount) |
| `slot_door_parks`, `slot_door_refusals`, `slot_door_draining_admits` | counters | §3.1's three outcomes |
| `slot_acquire_refusals`, `slot_rotor_cap_refusals`, `slot_grant_deferrals` | counters (legal) | an explicit ask for a held slot; a rotor ask past `2 × M`; a grant deferred behind ring 0's window (0 on a healthy cadence) |
| `slot_recall_notices` | counter | recalls recorded for a wire holder's carriage |
| `slot_roots_shipped`, `slot_roots_published_for_lessees` | counters | the overflow law's wire form |
| `slot_tree_inos_{p50,p99,max}` | Gauges | cursor-derived per held slot (the minted population — an upper bound on the live one; "a census walk per stats read is not a price a gauge pays", `:607–610`) |
| `slot_tree_bytes_{p99,max}` | Gauges | `extents × node_size` per held slot |
| `affinity_a_max_bytes`, `affinity_mints`, `affinity_ceiling_spills`, `affinity_ceiling_overflows` | Gauge, counters | §2.4 |
| `rotor_mints`, `dir_gather_mints` | counters | §2.4; closure `affinity_mints + rotor_mints + dir_gather_mints ≡ mints` |
| `slot_offer_n_floor` | Gauge | `N_floor` in force |
| `merge_sweep_foreign_skips` | counter | structural passes that skipped a foreign tree (0 on a solo mount) |
| `meta_kv_forest_page_publications_demoted`, `meta_kv_cursor_page_writes` | counters | §5.2, §5.3 |
| `meta_kv_leaf_lease_refusals` | **Tripwire** | the belt behind the door (charter `:151`) |
| `manager_verb_replays` / `manager_verb_refusals` (must-stay-0: a verb whose durable witness contradicts the caller) / `manager_verb_rejected` (the screened-word class) | counters / Tripwire / counter | §5.3.5 `:469` |

Every one reads 0/`solo` on a `--single-writer` mount (charter `:183`); the port exports them through
`counters.writeNamespaceJson` from a `slot_lease.stats` namespace (0002 row 197).

### 7.3 Constants with WHY

| Constant | Value | WHY (on the line) | Reference |
|---|---|---|---|
| `W` | 65 536 | the u16 slot-id wire type; frozen | `Z/route_table.zig:34–42` |
| `MINT_SPREAD` | 64 | the shipped granularity: a volume's load divisible into ≥ 64 movable slices from birth | `slot_lease_core.rs:67–71`; `Z/router.zig` `MINT_SPREAD = STAMP_MAX_CURSORS / 4` (re-derive: `MINT_SPREAD = 64` is the base, the cursor budget derived from it or retired — §2.5) |
| `M` | `clamp(W / (2 × writers), 1, 64)` | §2.1 | `:1310–1312` |
| `A_max(t)` | `max(used_leaf_bytes / 64, node_size)` | §2.4; one extent floor | `:1317–1319` |
| `2 × M` | the rotor cap | inside the width headroom the `2 ×` in `M` reserved | design `:270` |
| `SLOT_PAGE_BUDGET` | `(4096 − 314) / 35 = 108` | derived from the page's fixed part and the entry width — never a free constant | `appender.rs:121–124` |
| `REQUESTERS_PER_SLOT_MAX` | 8 | at most one requester can dominate; 8 equals each ≤ 1/8 | `slot_lease_core.rs:72–83` |
| `T_idle` | `T_owner` = 45 s | the membership lease the slot lease lives as long as | `env_knobs.rs:425` |
| `N_floor` | `max(2, ceil(ewma_handover / ewma_ship))` | a handover must pay for itself in ships saved; floor 2 = a single touch never moves anything | `:1324–1326` |
| cold start | `4 × ewma_barrier + 3 × ewma_ship_rtt` | what a handover IS | `:1332–1336` |
| `RECALL_COOLDOWN_WINDOWS` | 8 | bounds the worst-case residual thrash duty cycle at `cycles/(cycles+8) ≈ 27 %` | `tokens.rs:789`; `env_knobs.rs:345` |
| offer stand (in-process) | `clamp(min(beat, T_idle/3), 1 ms)` | the accept runs the handover directly; the requester needs only its beat | `:1342–1344` |
| offer stand (wire) | `3 × beat + 2 × ewma_handover` | the recall's delivery bound over three renewals | `:1354–1359`; `slot_lease.rs:734–741` |
| `COVER_CYCLES_MAX` | 64 = 8 clause-b rounds | a tail that does not move fails loud inside the eighth cycle by the audit itself; a loop reaching the bound has a MOVING tail | `Z/checkpoint.zig:112–117`; design `:306` Issue 30 |
| `SEQ_FRONTIER_SANE_MAX` | `u64::MAX / 2` | seqs are journal bytes + one per grant; 2^63 bytes is provably not a frontier | `appender.rs:1410–1420` |
| release floor bound | `max(page.seq_offset, granted_max + 1) + head_hint + 2 × ring_len` | the newest page is ≤ one checkpoint stale; a head advances ≤ one ring length past the declared tail before the next page write | `:1421–1445` |
| `g` | `u32`, `+1` per grant | strictly monotone per slot; the frame screen's word | `slot_lease_core.rs:480–482` |
| EWMA fold | `ewma − ewma/8 + sample/8` | 1/8 — the first sample seeds | `slot_lease.rs:586–605` |
| handover displacement | idle ≈ 4 barriers + 3 RTTs (5–20 ms); dominated + one ceiling's flush (10–200 ms); measured 5.5–7.8 ms | §4.4 | design `:300` |
| `MAX_SLOTS` per tx | 6 | rename's two parents, two inodes, a whiteout mint, plus a mint | `Z/slot_gate.zig:168–173` |

---------------------------------------------------------------------------
## 8. Tests (name + one sentence; venue)
---------------------------------------------------------------------------

Portable (model / pure): `slot_door_model` (§1.5 — holds; each fence and check-then-lock fail);
`racing_acquires_yield_one_holder_and_one_g_increment`, `an_offer_expiring_under_an_accept_never_
yields_two_holders`, `a_stale_release_is_refused_and_the_live_one_lands_once` (the table's law as
explorer models over its one lock); `table_state_machine`, `pick_prefers_never_written_then_lru`,
`dominance_over_a_common_window`, `derivations` (`slot_lease_core.rs:1507–1673` — the unit
contracts); `door_tokens_pair_with_the_release`, `a_token_holders_apply_is_admitted_while_the_
release_drains` (`:1465–1505`); `sym_slot_lease_rotor_ceiling_floor_and_window_derive_from_the_
width_and_the_lease` (`derivation_sweep_tests.rs:3733–3830`: every tie above + the registry ranges);
`the_cover_cycles_bound_is_eight_clause_b_rounds` (`sym_slot_transfer_tests.rs:3798`); `the_knobs_
derive_and_win_verbatim` (`:411`); `a_release_frames_slot_words_are_screened_against_durable_bounds`
(`manager_wire_bounds_tests.rs:153` — eight poisoned words, the volume byte-identical after each);
`the_page_layout_is_versioned_and_a_foreign_layout_refuses_the_open` (`:4001`); the `slot_state`
codec's round-trip + totality + fuzz (`Z/slot_state.zig:95–121` extended to both shapes).

Linux, one process (a RAM or file volume; `sym_slot_transfer_tests.rs`):
- `a_solo_armed_mount_leases_its_native_slot_and_sixty_four_rotor_slots` (`:264`) — the arm's census.
- `a_solo_mounts_load_spreads_over_sixty_four_rotor_trees_whatever_its_top_level_shape` (`:473`);
  `a_tree_past_a_max_spills_new_children_to_the_rotor_slot_with_the_most_headroom` (`:592`);
  `all_rotors_strictly_over_cap_grant_one_more_up_to_two_m_while_exact_equality_never_does` (`:650`).
- `first_writer_takes_it_and_the_second_is_refused_naming_the_holder` (`:802`); `leaf_lease_refusals_
  fire_on_a_foreign_mutation` (`:891` — the belt reached only by an illegal direct mutation).
- `a_commit_during_a_handover_parks_at_the_door_and_never_fails_einval` (`:976` — parked after the
  page, before tree 0; completes as `SlotBusy` for a wire accept, SUCCEEDS first-touch for the
  cadence; the belt never counts); `a_token_holding_commit_lands_under_the_release_that_drains_it`
  (`:1132`, seam `TEST_CONVEYOR_HOLD_POST_DOOR`).
- `lease_set_and_rotor_rmws_never_lose_an_update` (`:1249`).
- `a_region_past_its_page_budget_publishes_the_overflow_roots_into_tree0` (`:1339`); `a_region_past_
  the_flat_stamps_cursor_budget_keeps_checkpointing` (`:1460` — 281 leases); `an_overflow_slots_
  cursor_survives_a_crash_between_root_moves` (`:1527`); `a_page_named_slots_cursor_survives_a_
  remount_that_dies_before_its_next_page_write` (`:1621`).
- `a_handover_never_leaves_in_window_records_behind` (`:1678` — two rings replayed with `meta_kv_
  replay_key_violations == 0`; phases exact-sum).
- `dominance_over_a_common_window_decides_every_offer` (`:1835` — the six laws of design `:300`);
  `two_nodes_alternating_on_one_directory_converge_on_one_holder` (`:2278` — ≥ 2 handovers against
  the REAL `N_floor`, re-read < 1 000).
- `a_holder_live_below_a_directory_is_never_recalled_by_a_burst_into_it` (`:2011`) and the two-volume
  twin (`:2144`); `an_unarmed_mount_feeds_the_directory_parent_memo_nothing` (`:2235`).
- The seq-space law: `rollbacks_address_stamped_seqs_on_a_ring_with_an_offset` (`:2429`), `a_release_
  from_the_quieter_ring_clears_its_window_in_one_cycle` (`:2553`), `an_smo_retirement_on_an_offset_
  ring_releases_on_its_entrys_coverage` (`:2607`), `a_same_key_write_after_a_handover_to_a_quieter_
  ring_is_the_newest_value` (`:5099`).
- Tails: `a_release_of_a_tree_past_the_inline_cap_spills_its_tails_and_the_mount_survives` (`:2747`),
  `the_leaves_batch_of_108_hundred_leaf_trees_packs_into_entries_under_the_cap` (`:2846`), `a_leave_
  of_a_region_holding_several_trees_records_every_trees_tails` (`:2903`).
- The structural door: `the_manager_merges_an_unleased_slot_tree_and_skips_a_foreign_leased_one`
  (`:3092`), `a_wire_grant_of_an_unleased_tree_waits_for_the_sweep_and_clears_its_window` (`:3244`),
  `a_handover_of_a_tree_with_pending_structure_runs_the_holders_own_smos` (`:5565`).
- `a_first_touch_acquire_parked_for_ring_space_never_holds_the_verb_mutex` (`:3815`); `the_rotor_
  cap_counts_the_re_adopted_rotor_after_a_crash_remount` (`:3939`).
- Crash rows: `a_holder_dying_after_its_page_named_releasing_has_tree0_written_from_the_page` (`:4105`,
  row 5), `tree0_wins_over_a_stale_releasing_page_entry` (`:4214`, row 6), `a_requester_retries_
  against_the_successor_and_a_dead_requesters_lease_survives` (`:4376`), `two_live_pages_attesting_
  one_slot_refuse_the_mount` (`:4738`), `a_stale_release_is_refused_before_tree0_is_written` (`:5210`).
- `a_slot_released_reacquired_with_its_top_inos_deleted_never_remints_an_ino` (`:4507`); `forced_
  shrink_releases_idle_rotor_slots_down_to_the_derived_m` (`:4601`); `a_declared_region_is_released_
  when_its_last_slot_goes` (`:4666`); `the_renewal_grant_carries_the_members_slot_leases_recalls_and_
  offers` (`:4809`).
- `a_page_named_slots_cursor_is_durable_before_the_record_whose_tail_passes_its_mints` (`:5626`);
  `a_page_publish_refusal_never_reaches_the_cycle_ahead_of_its_ledger_record` (`:5720`).

Linux, N daemons over one volume (the port's `scripts/venue`; `sym_n_daemon_tests.rs`):
- `three_daemons_hold_disjoint_slots_and_commit_into_three_rings` (`:4673`); `a_joiner_reads_foreign_
  slots_through_tokens_and_serves_the_managers_shipped_step` (`:4870`).
- `a_dominating_requester_earns_an_idle_joined_holders_tree_through_served_ships` (`:2510`); `a_joined_
  holders_n_floor_is_seeded_at_its_first_served_ship` (`:2691`).
- `a_stale_holder_view_at_the_guards_re_resolves_and_lands_the_create` (`:2867`); `a_locally_
  dispatched_create_whose_slot_the_manager_took_is_redispatched_through_the_cross_owner_arm` (`:2974`);
  `a_local_steps_slot_busy_mid_plan_redispatches_and_never_fail_stops_the_initiator` (`:3084`);
  `a_create_into_a_directory_whose_slot_moves_to_the_creator_mid_plan_never_answers_enoent` (`:11444`).
- `a_released_slots_pending_times_are_drained_first_and_a_foreign_slots_are_dropped_not_wedged` (`:3191`).
- `a_dead_joiners_region_is_recovered_by_the_manager_and_a_third_daemon_takes_its_slot` (`:3610`);
  `a_dead_joiners_recovery_never_returns_an_extent_a_released_trees_root_still_reaches` (`:3504`).
- `a_wire_lessee_past_its_page_budget_keeps_committing_and_its_overflow_roots_ride_tree_zero` (`:3891`);
  `a_wire_lessee_sheds_its_idle_overflow_to_the_page_budget_within_one_beat` (`:4394`).
- `a_joiners_rotor_shrinks_to_the_derived_m_and_its_census_follows_the_set` (`:7839`); `a_wire_holder_
  releases_a_slot_on_the_managers_notice_and_accepts_an_offer` (`:7950`); `the_joined_door_refuses_
  without_the_plane_and_never_writes_a_control_entry` (`:8087`).
- `a_token_reader_follows_a_not_holder_redirect_to_the_lessee` (`:9357`); `a_joined_holder_resolves_
  a_later_joiners_slot_at_a_served_step` (`:1890`); `a_peers_lookup_through_a_dead_holder_is_refused_
  inside_the_bound_and_exact_after_recovery` (`:6582`); `a_foreign_slot_verb_whose_holder_is_
  unreachable_refuses_retryable_then_lands_once_bound` (`:6094`).
- The four page-home pins (§5.2: `:9497`, `:9735`, `:9923`, `:10102`).
- `an_accepted_offers_slot_lands_at_the_wire_requester_off_its_next_carriage` (`:16312`); `an_offer_
  that_lapses_under_a_standing_recall_is_expired_once_never_also_a_handover` (`:16442`).
- The corpse law's three classes (`:15840`, `:15960`, `:16571`, `:16724`) — the reclaim rung's, listed
  for the door's `SlotBusy`-at-the-withhold arm (design `:278`).

Red-first rule for the port: every row above lands as a failing test first; the seams that hold the
exact window (§7.1) are registry `SQUEEZEFS_TEST_*` rows, never process statics.

---------------------------------------------------------------------------
## 9. Quirks (per decision 0002) and better designs
---------------------------------------------------------------------------

| # | Reference quirk | Why it is poor | What the port does |
|---|---|---|---|
| Q1 | FOUR Dekker gates of one shape: `slot_gate_core` (cutover), `LeaseGate::enter/begin_release` (the door), `park_core` (the park), `token_grant_core` (grant ∥ pass) — each with its own fences, its own model, its own `Notify`s (`slot_gate_core.rs`, `slot_lease_core.rs:193–233`, `park_core.rs:120–165`) | Four copies of one protocol; the loom lesson ("the sides' accesses SeqCst on their own words are not enough — the fences are") learnt four times | **ONE gate**: `Z/slot_gate.SlotGates` with a per-slot STATE word; the door is `tryEnter`'s verdict (§3.6); the cutover's `closed` is one more state; the rung-5 model becomes the three-thread door model's special case |
| Q2 | Lease state in four RAM structures (`SlotLeaseTable` BTreeMap + indexes, `LeaseGate` three bitsets + `inflight`, `SlotHolderCache` ArcSwap map, the region's lease set + the `rotor` Vec), the `foreign` bitset REBUILT WHOLE per wire grant/release (`install_foreign` `:270–281`) and marked ahead of the write / rolled back by hand | Four sources of one truth; `foreign` is derivable from `(state, holder)`; an `O(leased)` rebuild per grant | ONE dense per-slot record table (65 537 × ≤ 32 B ≈ 2 MiB): the hot words atomic (`state ‖ holder ‖ g` packed, `inflight`), `foreign` DERIVED (`holder ∉ own`), the holder cache a VIEW of the table (the endpoint table is the only extra map), per-appender held/offered lists for the O(held) walks |
| Q3 | `DoorPass{Arc<SlotLeasePlane>, Vec<ForestSlot>}` per commit (`slot_lease.rs:818–836`); `ensure_leases_for_tx` collects slots into a `Vec` and the first touch is `Box::pin`ned (`backend.rs:8222, 8318–8327`) | Per-commit heap allocation on the commit path (charter §12) | The tokens are `[MAX_SLOTS = 6]u16` inline in the queue entry (the rung-5 `Pass`); no allocation; the first touch runs on the committer's fiber |
| Q4 | `DominanceWindow` = `Mutex<BTreeMap<Slot, SlotOps{Vec<RequesterOps>}>>` locked per served ship (`slot_lease_core.rs:1223–1301`); `holder_ops` = `scc::HashMap<Slot, Arc<HolderOps>>` (`slot_lease.rs:241–243`) | A global mutex + heap cells on the ship path; `HolderOps` per slot behind a hash | The dense table carries `HolderOps{epoch, cur, prev}` (24 B) and a fixed `[8]RequesterOps` per slot (8 × 24 B); the requester table under a per-slot spin lock (a ship is a wire round trip; the lock is µs) |
| Q5 | `slot_state:` key = `prefix ‖ slot u32 BE` (`slot_state.rs:40–42`); the port's rung 5 wrote the charter's DECIMAL spelling (`Z/slot_state.zig:23`) which does not sort in index order (`"slot_state:10" < "slot_state:2"`) | Two spellings; the decimal key defeats the census's index-ordered range walk | Keep the port's decimal key (landed, the charter's spelling, the SCAN range is correct) and state that the walk is NOT index-ordered — the census sorts its result (65 536 u16s, trivial); OR move to `prefix ‖ u16 BE` before the first release. Record the choice in 0002 |
| Q6 | The forest's per-slot cursors ride the LEDGER STAMP (`STAMP_MAX_CURSORS = 256` — a FLAT budget) until §4.4bd fail-stopped a manager at 281 leases; then the stamp carries none and the cursor lives with the lease (design `:1131`) | A budgeted list on the fixed ledger as the home of an unbounded population; the port's `stamp_cell.zig` folds guest cursors into the stamp today | Under the symmetric plane the stamp's `slot_cursors` is EMPTY by law (a tie test); the cell's homes are the page entry (every checkpoint, the cursor refresh before barrier #1 — §5.3) and tree 0 (grant / release / overflow publication / the cursor law); `inos.RecoveryFold.guestCursor` folds the window max per slot as the third floor term |
| Q7 | `SLOT_STATE_VERSION = 3` with a `variant` byte and two fixed shapes (50 / 54 B); the reference tolerates nothing else — fine — but `last_written` (the manager's lease seq) rides the durable record only for `prefer: unleased-then-idle`'s rank | A RAM ranking key persisted per slot; a `Leased` record with a `page_addr` the reader never dereferences (`page_addr` names a directory page the lessee's identity already locates) | v4 record: `{version u16, state u8 {unleased, leased}, dead/flags u8 = 0, appender_id u32, g u32, slot_tree_extents u32, root_addr, root_seq, cursor, seq_floor, last_written}` = 56 B ONE fixed `extern struct` for both states (the lessee words zero when unleased), total decode, size-asserted; `page_addr` dropped (the appender directory maps id → page) |
| Q8 | `Releasing{dead}` is `Releasing` under the dead holder's id with "dead" inferred from the holder's page state (`recovering_lessees` `slot_lease.rs:360–371`, read at open from the directory) | The door's verdict and the frame screen's rule 4 consult a page-state set beside the table | The RAM record carries `dead: bool` (§1.1); the door refuses `SlotBusy{holder: dead}` off the table alone; the screen's rule-4 exemption reads the same bit |
| Q9 | `wire_appender_page` READS THE WHOLE APPENDER DIRECTORY per wire slot verb to screen the frame's `appender_id` (`backend.rs:6352–6380`, `appender_known` `:6396–6411`) | A device read per `AcquireSlot`/`OfferSlot`/`ReleaseSlot` for a fact the manager holds in RAM | The manager's appender table (RAM mirror of the directory, maintained by every page write it makes or observes) answers; a MISS re-reads the one page, never the directory |
| Q10 | The wire handover's latency is THREE renewal beats + the handover (`wire_offer_stand_ms = 3 × beat + …`, `slot_lease_core.rs:1354–1359`) because "a wire holder serves no push channel" — the recall rides the holder's renewal (`backend.rs:6471–6489`) | Up to 30 s + handover for a decision made in µs; every holder DOES serve a listener (its endpoint is in the claim-set entry — §6.1) | `RecallForOffer{s}` is a WIRE VERB the manager dials to the holder's published endpoint (idempotent against the table's `Offered → Releasing` state); the renewal carriage keeps the recall as the BELT for a holder whose listener was unreachable; the stand becomes `beat + handover_bound` |
| Q11 | `SQUEEZEFS_TEST_SYM_APPENDER_SLOTS` — the in-process "declared region" seam (`env_knobs.rs:421`, `declared` `slot_lease.rs:224–228`) standing two appender regions in one process, with `Park` degrading to `Try` for them (`backend.rs:6026–6031`), a special rotor rule (Issue 10), a `test_set_manager` seam on the gate (`:263–265`) | A second holder model inside one process that production never runs, with its own arms everywhere | Not carried: the N-daemon venue (the container) is where two holders exist; in-process tests stand two `KvBackend` instances over one file (the port's crash-table shape) |
| Q12 | The HANDOVER's cover loop answers `Corrupt` at `COVER_CYCLES_MAX` whatever the tail did (`flush_slot_clear_of_region` `:7795–7803`), while the GRANT's clearing splits moved-tail (`Deferred`) from unmoved (`Corrupt`) (`:5959–5983`) | One law, two verdicts; a slow device's long stage-B window fail-stops a cadence release | Both loops take the split: a moving tail at the bound aborts the release as `HandoverDeferred` (retried next cadence; counted `slot_handover_deferrals`), an unmoved tail is `Corrupt` |
| Q13 | `affinity_ceiling_in_force` CLAMPS the knob's static MiB to `[node_size, heap]` in-process (`slot_lease.rs:61–63`); `resolve_mint_slots` clamps the knob to `[1, 64]` (`:44`) after the registry already ranged it | Clamping an admitted value (charter §1.6); two ranges for one row | The registry row's range is the whole law; a static ceiling below `node_size` or above THIS volume's heap REFUSES the mount naming the bound (the `layout.resolveInlineMax` pattern, 0002 row 103) |
| Q14 | `SQUEEZEFS_SYMMETRIC_META` as a product bool whose `=0` "names nothing a writer could run under" and only refuses (`env_knobs.rs:422`) | A knob whose only effect is to break the mount | Deleted; `-o ro` is the reader posture; a non-forest superblock is refused by the format version |
| Q15 | EWMA folds as racy load/store (`fold_handover_ns` `slot_lease.rs:586–594`); the plane's gauges `AtomicU64` with `Relaxed` `fetch_add` on shared lines | A torn fold under two concurrent handovers (benign — one sample lost); shared-line RMWs on the ship path | `counters.ShardedCounter` for the per-op counts, a `Gauge` for levels, the EWMA a CAS fold (or a single-writer word under the `handover` mutex, which it already is) |
| Q16 | `SlotHolderCache` endpoints as `Arc<str>`, members as `Arc<str>` member-id strings parsed per lookup (`slot_holder_cache.rs:47–52`, `parse_node_member_id`) | Strings on the resolve path | Endpoints as fixed sockaddr/URI cells indexed by appender id; the member identity is `(node_token u64, mount_slot u32)` (the page's own words) |
| Q17 | The structural door takes the SMO mutex for a WIRE grant and cycles ring 0 under it with a `Try` ask (`:6069–6079`) — correct — but the manager's OWN first touch takes neither "its SMOs are region 0's whatever the gate bit says" (`:6066–6068`) | Stated correctly; carried as is | Carried verbatim; the port's `verdict_structural` admits an unleased slot on the manager (§1.4) |

When a wave meets a new quirk here it adds a row to `Z/docs/decisions/0002` before diverging.

---------------------------------------------------------------------------
## 10. Delivery order inside the rung
---------------------------------------------------------------------------

1. `slot_state.zig` v4 shape (Q7) + the tails record codec; the dense `slot_lease.zig` table with
   its model (§1.5); `slot_gate.zig` grows the state word and the verdict (§3.6) — the migration
   engine re-pointed, `slot_gate_model.zig` subsumed.
2. The door in `commit_tx` (tokens on the queue entry; the holder-op note; `SlotBusy`; first touch
   with the parking admission) + `meta_kv_leaf_lease_refusals` behind `apply_locked`'s verdict.
3. The manager verbs (`AcquireSlots/AcquireSlot/OfferSlot/ReleaseSlot/ResolveSlot`) over the control
   tree with the structural door and the word screen; the arm (load, settle rows 5/6/C14, re-adopt,
   native, rotor).
4. The mint policy + `gather`; the cursor homes (Q6) + the pre-barrier cursor page.
5. The dominance window, the offer, the in-process accept, flush-then-transfer, the cadence.
6. The page plan + publication homes + publish-before-drop + the overflow publication.
7. The holder cache / `step_home` / `NotHolder` / `HolderDead` (with the tokens rung).
8. The wire faces (joiner's first touch, `PublishRoots`, `RecallForOffer` — Q10) with the cluster
   wire rung; the N-daemon tests in the venue.
