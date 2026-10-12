# Porting spec — Dynamic metadata routing: the slot→volume map, mint spread, online `migrate-meta-slot`, offline `add-meta` / `remove-meta` / `repair-set`

> **Checksum amendment (2026-10-11, decision 0003 §M; 0002 row 480).** Every `xxh3` / `xxh3-64` /
> `xxh3_64` below names the hash the reference used and this port used until format v5. The port's
> ONE hash is now **sqzhash128** (`src/core/sqzhash.zig`, the pinned definition): every 8-byte checksum
> and identity field is its LOW 64 bits under the fixed public format key (`checksum.hash64`,
> `hash64FieldZeroed`, `stamp`/`verify`, the streaming `checksum.Hasher`); the seeded name hashes
> (`hash54`/`hash56`) are `hash64Keyed` under the per-mount name key (`key.NameKey`: the volume's
> `hash_seed` zero-extended, expanded once at mount). Field names on the wire read `csum`. Byte
> layouts, offsets and widths below are unchanged; only the function behind the 8 bytes is.

Source of truth (read-only): `/Users/joberdorf/Source/squeezefs` (abbrev. `R/`).
Target: `/Users/joberdorf/Source/squeezefs-zig` (abbrev. `Z/`), charter §5.4, §5.5, §8.
Law: decision 0002 (the charter's mechanisms are requirements; the reference's incidental
choices are not; no compatibility owed) and 0003 (format v4: one key layout, one-member forest).

Reference files: `R/docs/design-dynamic-meta-routing.md` (515, the W derivation / stamp / mint
spread), `R/docs/design-volume-lifecycle.md` §5.5 (:336–404, the discovery protocol, gate, flip,
staging rebind), `R/src/meta_backend/mod.rs` (6752: `RoutedMetaBackend`, routing arithmetic, the
gate, `pick_mint_volume`, discovery, the format plan), `R/src/meta_backend/slot_gate_core.rs`
(109), `R/src/meta_backend/slot_migration.rs` (954, the engine), `R/src/meta_backend/kv/slot_set.rs`
(274), `R/src/meta_backend/kv/slot_cursor_core.rs` (77), `R/src/meta_backend/kv/checkpoint.rs`
(:186–355 the stamp), `R/src/meta_backend/kv/backend.rs` (:963–1086 the tee, :12040–12248 the
cursor/stamp face, :23762–23816 the tee hookup), `R/src/config_ops.rs` (:1508–3042 the offline
verbs), `R/src/main.rs` (:1355–1498, :4804–5020 the `volume` verbs), `R/src/jobs.rs` (:2290–2334),
`R/src/fuse_client.rs` (:7464–7485, :12835–12867 gauges), tests named in §6.

---------------------------------------------------------------------------
## 0. Where the port stands (what exists, what this spec adds)
---------------------------------------------------------------------------

Exists in `Z/` today — do not re-port, bind to it:

| Z/ item | What it is | Reference twin |
|---|---|---|
| `src/meta/inos.zig:292–317` `ROUTING_WIDTH = geometry.routing_width` (65 536), `routeIno(ino, W)`, `makeGlobalIno(local, slot, W)` incl. the `W ≤ 1` identity arm and the ino-1 root pin | the lane arithmetic | `mod.rs:789–822` |
| `src/meta/key.zig:490–541` `GUEST_NS_SHIFT = 40`, `guestLocalIno(slot, local) = (slot+1) << 40 \| local`, `splitGuestLocal`, `guestForestSlot(slot) = slot + 1`, `NATIVE_FOREST_SLOT = 0`, `FOREST_SLOT_MAX = 65 536`, `forestSlotOfIno = key_ino >> 40`, `checkSlotTreeKey`, `slotTreeKeySlot` | guest keyspace + the record→slot law | `mod.rs:37–81`; forest.rs:305–313 |
| `src/meta/tree.zig:299–342` `TreeIdentity{control, slot}`; `:866–885` `route(trees, tree_id, level, key)` = THE routing law (content by `key_ino >> 40`, interior by `slot ‖ separator`) | one tree per slot | forest.rs:1–27 |
| `src/meta/ledger.zig:17–36, 81–91, 157–283` the v4 stamp codec: `SlotRun{start, stride ≥ 1, count ≥ 1}`, `SlotSet.fromRuns` (bitset overlap check), `memberRun(position, count)`, `MembershipStamp{set_uuid, set_epoch, member_position, member_count, slots_hosted, native_slot: ?u16, slot_cursors}`, `STAMP_MAX_RUNS = 128`, `STAMP_MAX_CURSORS = 256`, worst case 3 767 B | the stamp | checkpoint.rs:186–355; slot_set.rs |
| `src/meta/format.zig:57–58, 188` `member_position / member_count` → `MembershipStamp.member` (one stride run, native = position) | the format plan per member | `mod.rs:6556–6609` |
| `src/data/router.zig:360–446` `volumeHealth`, `pickMintVolume(parent, healths, rr)` (9/10 band, per-router rotor), `MINT_SPREAD = STAMP_MAX_CURSORS / 4 = 64`, `MintRotor` (first 64 hosted ascending, `fetch_add % len`), `mintGlobalIno`, `slotOf` | mint spread | `mod.rs:2585–2632, 3266–3297, 1167–1174` |
| `src/meta/inos.zig:108–186` `RecoveryFold{native_slot, native_max, guest_max}`, `guestCursor(slot, stamped)` | the DUR-8c per-keyspace fold | `backend.rs:12289–` |
| `src/data/staging.zig:920–1054` `Marker{set_uuid, set_epoch, prev_set_epoch (DUAL)}`, `bindVerdict` | the KD-8 generation rebind's marker | `config_ops.rs:1780–1895` |
| `src/meta/backend.zig` ONE writable volume; the stamp passed by VALUE into the checkpointer (`:545`), the fold seeded with `mounted.membership_stamp.native_slot` (`:492`) | — | — |

Missing (this spec): the per-set `RouteTable` (slot→volume) and the multi-volume backend over it;
per-volume `native_slot` at runtime; guest-cursor CELLS folded into the checkpoint stamp;
order-independent discovery with refusals; the cutover gate; the conveyor tee; the migration
engine; `add-meta` / `remove-meta` / `repair-set`; the staging drain + rebind driver; the gauges.
`Z/src/cli/format.zig:13–14` says it outright: "A metadata set of more than one member is rung 5's
(`W = 2^16` stamps) and refused here".

---------------------------------------------------------------------------
## 1. The slot→volume map
---------------------------------------------------------------------------

### 1.1 W is derived, frozen, and baked into every ino

- `DERIVED_ROUTING_WIDTH: u32 = u16::MAX + 1 = 65 536` — "the FULL slot-id namespace the
  guest-keyspace wire encoding already reserves (slot ids are u16 in `guest_local_ino`, the
  membership stamp, and the cutover gate map)" (`mod.rs:41–50`; design §5.1 :167–197).
- Routing: `slot = (ino−2) % W`, `local = (ino−2)/W + 2`; ino 1 → `(0, 1)`; `W ≤ 1` → identity
  `(0, ino)` (`mod.rs:789–797`). Inverse `ino = (local−2)·W + slot + 2`, `(1, 0) → 1`
  (`mod.rs:814–822`). Z/ has both verbatim (`inos.zig:303–317`).
- WHY frozen: "W is baked into every global ino ever surfaced to the kernel (st_ino, NFS handles,
  dentry child references stored in dentry records, `block_map:` keys, DLM stripe names). Changing W
  re-derives every ino" (design §2.1 :49–57). "Dynamic" = only the slot→volume indirection moves.
- Headroom: globals reach `(2^40 − 2)·2^16 + 65537 < 2^56` (design :182–184); Z/ pins the same bound
  at `key.zig:508–513` (every routing ino below 2^57 ⇒ an ino-major key's first byte ≤ 0x01, never the
  refs kind 0x06).
- The reference STORES W in every stamp (`routing_width: u32`, checkpoint.rs:312–317, :529) "so a
  future derivation change can never re-route an existing set". Z/ does NOT (0002 row 41; ledger.zig
  :33–36 "a stored copy could only disagree with the law"). Keep Z/'s choice: W is a comptime tie
  (`geometry.zig:169`), and the superblock `version` is what a future derivation change would bump.
- Measured (design §5.9 :367–395): `route_ino` 1.0–1.1 ns/op (arith + table); route table RAM
  512 KiB as `Vec<usize>` / 128 KiB as u16 per mount; format plan expansion 165–186 µs, discovery
  claim resolution 100–131 µs, validate + native derivation 24–31 µs (all O(W) passes, rustc -O);
  end to end on a file-backed debug sandbox: format 7.0–7.6 ms, discover 6.7–7.8 ms, open 20.9–27.9 ms;
  at-rest stamp 48 B fresh / ≤ 674–676 B with a full 63-cursor mint set vs the 3 953 B budget.

### 1.2 The RAM structure

```
RouteTable { slot_to_volume: Vec<usize> /* W entries */, mint_slots: Vec<Vec<u16>> /* per volume ≤ MINT_SPREAD */ }
```
(`mod.rs:1031–1040`), held in `RoutedMetaBackend.route: ArcSwap<RouteTable>` (`:901`) and swapped
whole by `publish_slot_map` (`:1459–1467`: validate → derive mint sets → `store`; "the
slot_gate_core protocol guarantees no admitted mutation straddles a swap"). Beside it, FIXED for the
mount's lifetime: `legacy_slot: Vec<Option<u16>>` — per volume "the slot whose records live in its
LEGACY (un-namespaced) keyspace … `None` = a fresh `add-meta` member whose every hosted slot is a
guest" (`:902–906`), and `routing_width: u64` (`:895`).

`validate_slot_map(volume_count, W, map)` (`mod.rs:830–872`) refuses, each prefixed
`"metadata slot map invalid: "`:
- `"no metadata volumes"`;
- `"routing width {W} below the volume count {n}"`;
- `"map covers {len} slots but the frozen routing width is {W}"`;
- `"slot {s} maps to volume index {v} of {n}"`;
- `"volume index {v} hosts no slot — every member must host at least one (its native mint slot)"`.
It also derives each volume's SMALLEST hosted slot (`native[vol]`), which `with_slot_map`
(`:1256–1264`) uses as the native slot — "correct for identity-distributed (never-migrated) sets;
migrated sets come through `with_slot_map_and_natives`" (`:1265–1283`) with the stamps'
`resolved_native_slot()` (`:6503–6512`).

Z/ shape: `[W]u16` (128 KiB — the design's own u16 figure) published through
`sync/arc_swap.ArcSwap` like `router.PlacementTable` (`router.zig:205–291`); `mint_slots` as
`[V]MintRotor` (Z/ already has `MintRotor.init(hosted_ascending)`, `router.zig:406–434`). The port
has no "legacy" naming: the per-volume field is `native_slot: ?u16` (the stamp's, `ledger.zig:238–241`)
and the records of slot `s` on volume `v` live in forest slot `0` iff `native_slot == s` else
`guestForestSlot(s) = s + 1` (`key.zig:494–506`).

### 1.3 Fresh-format derivation: one arithmetic progression per member

`plan_meta_slot_set(volume_count)` → `plan_meta_slot_set_with_width(n, DERIVED_ROUTING_WIDTH)`
(`mod.rs:6547–6609`): mints ONE `set_uuid`, epoch 1, and per member `pos`:
`count = ceil((W − pos) / n)`, run `(start = pos, stride = n /* 1 iff count == 1 */, count)`,
`member_count = n`, `native_slot = Some(pos)`, no cursors (`:6580–6603`). Refusals:
`"at least one metadata volume is required"`, `"routing width {w} below the volume count {n}: every
metadata volume must host at least one slot"`, `"routing width {w} exceeds the u16 slot-id namespace
(65536)"` (`:6562–6577`). The width-parametric core stays "the geometry-test surface
(`tests/meta_slot_tests.rs` exercises flip/discovery/refusal shapes at W = 8)" (`:6551–6555`).

Z/ has `ledger.memberRun(position, count) = {start: position, stride: count, count: ceil((W −
position)/count)}` (`ledger.zig:221–225`) and `MembershipStamp.member` (`:249–260`) — the same
derivation; `format.zig:423–434` pins member 2 of 4 → `{2, 4, 16 384}`. One difference: the
reference canonicalizes a singleton to `stride = 1` (`:6588`); Z/'s `stride = count` on a singleton
(a run of `count == 1` arises only when `position ≥ W − member_count`, i.e. sets of more than 32 768
members) is a legal ≥ 1 stride — `SlotSet`
accepts both; nothing to change, but the ONE encoding law (one spelling per set) argues for Z/
canonicalizing singletons to stride 1 too (`SlotRun.last` and `contains` are stride-blind at
`count == 1`, `ledger.zig:164–172`).

The identity distribution `slot s → member s mod V` IS one run per member, and "stride-doubling
growth (`V → 2V` takes every second slot of each donor's run) keeps it O(1) runs per volume at any
power-of-two growth depth" (slot_set.rs:1–22; design §5.5 :303–316). Scattered hand-picked
migrations consume the run budget and are refused loud at preflight past it.

### 1.4 The stamp codec — reference vs `ledger.zig`

Reference wire (checkpoint.rs:186–217), an OPTIONAL suffix told apart by residual length:
```
set_uuid[16] ‖ set_epoch u64 ‖ member_position u16 ‖ member_count u16 ‖ routing_width u32 ‖
n_runs u16 ‖ n_runs × {start u16 ‖ stride u16 ‖ count u32} ‖ native_slot_plus1 u32 ‖
n_cursors u16 ‖ n_cursors × {slot u16 ‖ next_local_ino u64}
```
`native_slot_plus1` is u32 because "a u16 plus1 overflows at slot 65535" (:209–210, design rev 2).
Caps `STAMP_MAX_RUNS = 128`, `STAMP_MAX_CURSORS = 256`, worst case `24 + 34 + 5×17 + (34 + 128×8 +
4 + 256×10) = 3 765 < 4 096` (:266–281). Encode refuses past either cap loud (:455–474):
`"membership stamp needs {n} hosted-slot runs — at most 128 …"` / `"… {n} cursors … at most 256 (the
same §5.3 encoding budget; migrate …"`; decode refuses `n_runs > 128` / `n_cursors > 256` (:662–703)
and runs through `SlotSet::from_runs` (count 0, `stride 0 ∧ count > 1`, escape past u16, overlap —
slot_set.rs:96–130).

Z/ v4 (ledger.zig:27–31): `set_uuid ‖ set_epoch ‖ member_position ‖ member_count ‖
native_slot_present u8 ‖ native_slot u16 ‖ n_runs ‖ runs ‖ n_cursors ‖ cursors`, ALWAYS present,
read by position; worst case 3 767 B with 18 spare roots (`:98–108`). Reconciliation:

| Reference field | Z/ | Verdict |
|---|---|---|
| `routing_width u32` | absent | Z/ by design (0002 row 41) — derived |
| `native_slot_plus1 u32` | `present u8 ‖ slot u16` | same information, no overflow trick |
| runs / cursors / caps | identical | — |
| optional-by-length suffix | always present | Z/ by design (0002 row 41) |
| `stride == 0` singleton (pre-dynamic) | refused (`stride ≥ 1`) | Z/ by design |

**Z/ lacks no stamp FIELD.** What Z/ lacks is the stamp's RUNTIME half (§1.6): the live guest-cursor
cells folded in at every checkpoint, `set_membership_stamp` (install cursors as floors), and the
flip's stamp mutations.

A per-SLOT epoch does not exist in the reference. "Per-slot highest-epoch-wins" compares the two
claimants' VOLUME `set_epoch`s (`mod.rs:6456–6490`); a slot's epoch is implicitly the epoch of the
stamp that claims it. See §7 Q1 for the hazard this leaves and the port's remedy.

### 1.5 Per-volume stamps, order-independent discovery, every refusal

`observe_meta_set(paths)` (`mod.rs:6263–6291`) reads per URI: sector 0 (classify) + the newest
valid ledger record's stamp (`read_newest_ledger`), WITHOUT mounting — refusals `"Metadata volume
{p} is not formatted (zeroed superblock) — run `squeezefs format` first"` and the v2 error; a ledger
with no valid record observes as stampless.

`discover_meta_set(paths)` (`mod.rs:6330–6527`), in order:
1. `stamped == 0` ⇒ `"metadata volumes {listed:?} carry NO §5.5.1a membership stamps — every
   dynamic-routing format stamps its members at format time, so these ledgers are torn or foreign;
   reformat required (`squeezefs format --force`, destroys the old contents)"` (:6334–6350).
2. `0 < stamped < n` ⇒ `"metadata set mixes stamped and stampless members: {k} of {n} volumes carry a
   §5.5.1a membership stamp but {stampless:?} carry none — a slot-mapped set's members are all
   stamped; if a repair was interrupted, run `squeezefs volume repair-set <sqmeta-uri>`"` (:6351–6365).
3. Any `member_count == 0` ⇒ `"metadata volume {p} is a RETIRED member (remove-meta tombstone) —
   remove it from the URI; it carries no live slots"` (:6376–6385).
4. Pairwise vs the first: `set_uuid` differs ⇒ `"metadata volumes belong to DIFFERENT sets: {a} and
   {b} carry different set uuids ({..4} vs {..4}) — one of them is from another filesystem"`;
   `routing_width` differs ⇒ `"metadata set stamps disagree on the frozen routing width: {a} declares
   {w} while {b} declares {w'}"`; `member_count` differs ⇒ `"metadata set stamps disagree on membership
   (an interrupted add-meta/remove-meta): {a} declares {m} members while {b} declares {m'} — re-run the
   interrupted `squeezefs volume add-meta`/`remove-meta` with the same arguments (idempotent), or
   `squeezefs volume repair-set`"` (:6386–6421).
5. `member_count != n` ⇒ `"metadata set stamps declare {m} members but the URI lists {n} ({listed:?})
   — a member is {missing from the URI | listed that the stamps do not name}; every member of the set
   must be listed"` (:6422–6435).
6. Sort by `member_position` (positions may be SPARSE after a remove — only uniqueness is
   structural); duplicates ⇒ `"metadata volumes {a} and {b} BOTH stamp member position {p} — duplicate
   membership; one of them is stale or foreign"` (:6437–6454).
7. Slot claims, per-slot highest-epoch-wins (:6456–6490): a slot ≥ W ⇒ `"metadata volume {p} hosts
   slot {s} outside the frozen routing width {W}"`; two claims at the SAME epoch ⇒ `"metadata volumes
   {a} and {b} BOTH host routing slot {s} at the SAME epoch {e} — impossible by construction (one
   coordinator, one flip per slot); treat as corruption"`; a higher epoch replaces the claim.
8. An unclaimed slot ⇒ `"routing slot {s} of width {W} is hosted by NO listed volume — the durable slot
   map is incomplete; run `squeezefs volume repair-set`"` (:6492–6502).
9. Output `MetaSetDiscovery{ordered_paths, uuids, routing_width, slot_to_volume, native_slots,
   set_epoch = max}` (:6297–6315, :6503–6526).

Then `open_routed_meta_set` (:398–475): `validate_slot_map` BEFORE any guard ("a bad map can never
leave claims behind"), `open_meta_volume_set` in CANONICAL order (each D0-guarded open; a failure
releases every prior guard, :296–317), `with_slot_map_and_natives`, then the cross-volume intent
roll-forward and bring-up residue cover per volume. The reader twin `open_routed_meta_set_read_only`
(:485–506) runs the same discovery with no guards.

Order independence: `volume_set_generation = generation_from_uuids(disc.uuids)` = `"v3:" ‖ hex(uuid)`
joined by `"|"` in canonical order (:6691–6713), so "a STAMPED set's canonical order rides
`member_position`, making the generation identical under any operator URI ordering" (:6624–6628).
Pinned by `test_discovery_is_uri_order_independent_at_derived_width`
(dynamic_meta_routing_tests.rs:626–648) and `test_reordered_uris_discover_same_set_and_same_generation`
(meta_slot_tests.rs:651).

Z/ additions to discovery (the port's own facts): every member's superblock `hash_seed` must be
EQUAL (Z/ mints one set-wide random seed, `geometry.mintSetIdentity`, 0002 row 42; the reference's
`check_hash_seed_uniform` refusal at slot_migration.rs:696–707 still names the retired `format
--meta-slots`) — refuse at discovery `"metadata volumes {a} and {b} carry different hash seeds — not
one set"`; every member's `set_uuid` must equal the `set_config`'s owner (`set_config.zig:1–7`,
`cli/mount.zig:431`). The Z/ generation identity for staging is `set_uuid` + `set_epoch`
(`staging.zig:931–936`), not a uuid join — see §4.4.

### 1.6 The epoch and the cursors at runtime

- `set_epoch`: format mints 1 (:6593); a slot flip writes `E = max(src.e, dst.e) + 1` on BOTH
  participants only (slot_migration.rs:805–809); `add-meta` / `remove-meta` write `E = max over
  members + 1` on EVERY member (config_ops.rs:2510, :2861). Epochs "may legitimately DIFFER at rest
  (a slot flip bumps only its participants)" (`mod.rs:6367–6372`).
- Guest cursors: `KvMetaBackend.guest_cursors: scc::HashMap<u16, Arc<SlotCursor>>` (backend.rs
  :1230); `SlotCursor{next: AtomicU64}` with `new(next.max(2))`, `mint = fetch_add(1, AcqRel)`,
  `mint_range(n)`, `snapshot = load(Acquire)`, `install_floor = fetch_max(floor.max(2), AcqRel)`
  (slot_cursor_core.rs:38–77) — the publication edge: "a mint whose RECORD reached the RAM tree before
  the checkpoint's flush pass synchronizes-with the snapshot load … `snapshot > minted_ino`"
  (:11–19), loom-modelled. Z/ `InoWatermark` (`inos.zig:66–95`) is the same cell with the same
  orderings — reuse it as the per-slot cell.
- `allocate_guest_ino(slot)` (backend.rs:12114–12133): an existing cell mints; a VIRGIN hosted slot
  lazily creates a cell at 2 — "its keyspace is empty by construction (only travelling cursors or
  replay maxima make records; both install cursors)". `reserve_guest_ino_range` (:12138–12151) is the
  intent-supply twin.
- `membership_stamp_for_ledger` (:12093–12109): the stored stamp with the LIVE cursors folded in,
  sorted by slot — "the stamp image a checkpoint's ledger record carries". On a forest with the
  appender plane the cursors are CLEARED from the stamp (:12080–12092) because "a manager that
  first-touched three dead joiners' released slots in one burst held 281 and every durable act on
  the volume refused `corrupt KV encoding`… until the ring filled and the volume failed" — the
  cursor then lives with the LEASE (page / tree 0). For Z/'s one-member forest (no appender plane)
  the stamp carries them: ≤ 64 rotor + travelled ≤ 256 (§7 Q5).
- `set_membership_stamp(stamp)` (:12058–12073): every cursor named by the stamp becomes a live cell
  (`install_floor`, idempotent, never regresses); the `repair-set` re-stamp surface too.
- Mount: `RecoveryFold.guestCursor(slot, stamped) = max(recoverNextIno(stamped, seen), 2)`
  (`inos.zig:182–185`) is Z/'s fold; the reference's `max_local_ino_watermark` folds every cursor for
  fsck C9 (backend.rs:12289–).

Z/ binding: `checkpoint_cycle.RecordDeps.membership_stamp` (`checkpoint_cycle.zig:128`) becomes a
`stampForLedger(ctx)` hook (stored stamp + the cells' snapshots), the mount installs
`cursorFor(slot)` floors from `RecoveryFold.guestCursor`, and the native watermark stays
`InoWatermark` (`backend.zig:302`).

---------------------------------------------------------------------------
## 2. Ino routing on every op; guest vs native keyspaces; mint placement
---------------------------------------------------------------------------

### 2.1 `route_ino` → `(volume, effective local key ino)`

```rust
// mod.rs:3174–3187
let (slot, local) = route_ino_width(ino, W);
let v = route.load().slot_to_volume[slot];
let eff = if legacy_slot[v] == Some(slot) { local } else { guest_local_ino(slot, local) };
(v, eff)
```
`slot_of_ino` (:3190–3195) is the gate/tee attribution; `slot_volume(slot)` (:3200–3206) the live
home; `make_global_ino(eff, v)` (:3211–3227) inverts (a raw local on a volume with no legacy
keyspace is a routing bug → `expect`); `try_make_global_ino` (:3238–3258) is the tree-walk form that
answers `None` for raw local 1 outside slot 0 and locals < 2 ("a per-volume/per-slot CONTROL record
with no global encoding").

On a FOREST volume the backend then routes `(kind, legacy key)` through `SlotTrees::route_or_mint`
by `forest_key_slot = key_ino >> 40` (forest.rs:1–27, :305–313) — so a guest slot's records live in
the host's slot tree `s + 1` under keys `(s+1) << 40 | local ‖ kind ‖ rest`, the native slot's in
slot tree 0 under `local ‖ kind ‖ rest`; refs route by their OWNER ino at offset 17
(design-symmetric-metadata.md:344–358). The reference still applies this as a TRANSFORM at the tree
boundary (`forest_key` / `split_forest_key`; `legacy_recs` at backend.rs:13590–13608 strips the kind
byte for the tee) — Z/ has ONE key layout (0002 row 35) and `tree.route` (`tree.zig:866–885`) IS the
law; a Z/ op computes `routeIno` → `slot_to_volume[slot]` → `volume.native_slot == slot ? 0 : slot+1`
→ `TreeIdentity{.slot}` → `backend.treeOf` (`backend.zig:929`) and builds the key with
`key.guestLocalIno` iff guest. `inos.RecoveryFold.noteRecord` (`inos.zig:130–152`) already splits
the keyspaces this way at replay.

Every mutating op computes its TOUCHED SLOT SET from inos it knows BEFORE any lock (the gate, §3.4):
`link_body [ino, new_parent]` (:1716), `destroy_inode_chunked [ino]` (:3132), `create [parent]` +
the mint slot (:4034, :4058–4075), `rename_body [old_parent, new_parent]` + a RENAME_WHITEOUT mint
(:4443–4468), `setattr/setxattr/removexattr/destroy_inode [ino]` (:5229–5351), `unlink_at [parent]`
(:5379), `set_layout_and_size`, `commit_block_refs*`, `mark_block_refs_shared`,
`migrate_block_map_train`, `sweep_block_map`, `kvmap_*`, `merge_layout_and_size*`,
`park_write_times` all `[ino]` (:5668–6234), `destroy_inodes` the whole batch (:2640–2642). Routes
are derived AFTER admission ("a park can span a flip, and held entries pin the map", :4030–4034).

### 2.2 Health-weighted mint volume: `pick_mint_volume`

`pick_mint_volume(parent_v)` (`mod.rs:2585–2632`): one volume ⇒ the parent's; else over every
non-disabled volume (and, under the retired multi-owner plane, only OWNED ones) collect
`get_volume_health(i)`; empty ⇒ the parent's; sort descending; keep `health ≥ max × 9 / 10`;
round-robin with a `static META_COUNTER` (relaxed `fetch_add % len`). Applies to directories AND
regular files since 2026-07-30: "a workload whose files live under one directory — the field's 32
root-dir files — drove 100 % of journal traffic into one volume's journal + conveyor (~21–25 k meta
device-writes/s ceiling) while its sibling idled at 0.00" (:2563–2584); a cross-volume create costs
two whole-tx entries on DIFFERENT volumes, "so per-volume entries/op stays ≈ 1".

`get_volume_health(idx)` (:2531–2561): disabled ⇒ 0; a process-`static` `HEALTH_CACHE` with a 500 ms
`Instant` TTL ("a short cache avoids scanning allocator occupancy on every mkdir"); score =
`min(1000, free_extents / total_extents × 1000)` in `f64` — "Estimated remaining-capacity FRACTION:
free extents / total (resolved OQ 5, design §4.9)".

Z/ already corrected the statics (0002 row 102): `router.volumeHealth(free, total, enabled) =
min(1000, free·1000/total)` integer (`router.zig:360–363`) and `pickMintVolume(parent_v, healths,
rr)` with the rotor a `Router` field (`:368–393`, `mint_volume_picks` ticks). Owed: the healths
vector must come from the META volumes' allocators (`alloc_ext.ExtentAllocator` free/total extents,
`backend.space()`, `backend.zig:992–999`) refreshed on the placement cadence, never a per-mkdir probe;
`disabled` = the volume's FAILED latch (`backend.isFailed`, `:1007`).

### 2.3 The mint rotor and `MINT_SPREAD` × the lane arithmetic

- `MINT_SPREAD = STAMP_MAX_CURSORS / 4 = 64` (`mod.rs:52–61`): "what makes the derived width REAL
  migration granularity (1/64 of a volume's load per slot) instead of a cosmetic constant — a single
  mint slot per volume would put every record in V of 65536 slots"; ¼ of the cursor budget "leaving
  ¾ for cursors travelling in with migrated slots". Z/ `router.zig:394–402` identical, comptime tie.
- `derive_mint_slots(n, map)` = per volume the first `min(64, hosted)` hosted slots ASCENDING, derived
  "on every publish (no new durable state; cursors are already the durable part and already travel
  at cutover)" (`:1164–1174`; design §5.4 :283–287). Z/ `MintRotor.init(hosted_ascending)`.
- `pick_mint_slot(v)` (`:3266–3297`): `W ≤ 1 ⇒ 0`; (rung-6 lease arm; retired client-slot arm);
  else `mints[mint_rr[v].fetch_add(1, Relaxed) % mints.len()]` — "Relaxed is sufficient — the rotor
  feeds a pure distribution choice, never an ordering edge" (:914–918). Z/ `MintRotor.pickSlot`.
- `allocate_local_ino_in_slot(v, mint)` (`:3445–3467`): `mint == legacy_slot[v]` ⇒ the volume
  watermark (refuses `"volume {v} exhausted its native local-ino namespace ({raw:#x} ≥ 2^40) — the
  monotonic watermark crossed into the guest partition"`) ⇒ `(raw, make_global(raw, mint))`; else
  `allocate_guest_ino(mint)` ⇒ `(guest_local_ino(mint, raw), make_global(raw, mint))`. Callers on
  gated paths "pick FIRST, gate the picked slot, then mint into it" (:3263–3265, :4053–4075).
- `reserve_intent_supply(parent, count)` (`:3490–3542`): one `(volume, slot)` cursor advance of
  `count` under the gate, returns `(first_global, stride = W, count)` and ASSERTS affinity
  (`g_last == g0 + (count−1)·stride`, else `"intent-supply reservation: the global ino encoding is not
  affine over the reserved range (routing bug)"`). Z/ `InoWatermark.reserveRange` exists.

What the lane arithmetic yields with the rotor: member `v` of `V` mints in slots `{v + kV | k < 64}`;
every rotor slot's cursor starts at local 2, so the first 64 mints of a fresh V = 1 set are inos
`2, 3, …, 65` (slot 0 from the watermark, slots 1..63 from cursors), the 65th is slot 0 local 3 =
`65 538` — inos advance by W every 64 mints (sparse, forever stable; ≈ 2^30 after 1 M files, under
the 2^56 ceiling). Pinned: `test_mint_spread_rotates_across_the_mint_set_and_survives_remount`
(dynamic_meta_routing_tests.rs:517–551: 256 mints land in exactly 64 slots; after a remount every
new global is fresh — "cursor lost" otherwise).

---------------------------------------------------------------------------
## 3. Online `migrate-meta-slot`, step by step
---------------------------------------------------------------------------

Entry: CLI `volume migrate-meta-slot <mountpoint> <slot> <target_volume>` (main.rs:1447–1462) is
refused off a live mount (`"volume migrate-meta-slot runs the ONLINE engine on a live mount — pass
the mountpoint (offline set changes are add-meta/remove-meta)"`, :4990–4994), rides the admin
roundtrip as a `job:` (jobs.rs:165–170, :2290–2334 — `migrate_slot` with default options, the
report logged `"slot {s} → volume {v} ({n} records, {k} delta keys, {o} overflows, cutover {ms} ms)"`).

`migrate_slot(routed, slot, target_idx, opts, hooks)` (slot_migration.rs:477–690):

### 3.1 Preflight (before anything is written)
- `migration_lock` — "one slot migration at a time per routed set (the coordinator's one-flip-per-slot
  law, made structural)" (`mod.rs:911–913`; :484).
- `slot ≥ W` ⇒ `"slot {s} outside the frozen routing width {W}"`; `target ≥ volumes` ⇒ `"target volume
  index {t} out of range"`; `W ≤ 1` ⇒ `"a W=1 filesystem has one slot — it can only be replaced
  wholesale (volume add-meta/remove-meta), not spread"` (:485–502).
- `legacy_slot_of(target) == slot` ⇒ `"migrating slot {s} back to its origin volume is not supported in
  v1 (its legacy keyspace may carry residue) — pick another target"` (:503–508). §7 Q6 lifts this.
- (Armed multi-owner plane only — rung 6: slot 0 refuses, cross-owner refuses, :520–553.)
- `src == target` ⇒ already flipped (a window-1/2/3 re-run): `finish_flip` and return (:555–561).
- The source must keep ≥ 1 other slot: `"migrating slot {s} would leave its host volume {v} hosting
  nothing — use `squeezefs volume remove-meta` to retire a whole member"` (:564–573).
- `check_hash_seed_uniform(src, dst)` (:577, :696–707).
- Keyspaces: `src_ks = SlotKeyspace::of(slot, legacy_slot_of(src))`, `dst_ks = … (target)` (:578–579):
  `of(slot, legacy)` = `{lo: 0, hi: 2^40, legacy: true}` iff `legacy == Some(slot)` else `{lo: (slot+1)
  << 40, hi: lo + 2^40}` (:190–205); `translate_ino` masks the top bits and ORs the destination base
  (:218–229); `translate_key` rewrites the first 8 BE bytes of every key (:233–240).
- Incompat bit 4 set + `sync_device` on BOTH participants (:583–586) — gone in v4 (0002 row 43).

### 3.2 Arm: wipe target residue, gate, tee
- `teardown_slot_keyspace(dst, dst_ks)` — "Wipe any prior aborted attempt's target residue
  ('torn-down by re-run' — §5.5.2b write-0 note)" (:588–590).
- `gate = routed.arm_slot_gate(slot)` (:591; `mod.rs:3554–3572`: idempotent per slot; `gates.armed
  += 1`).
- `tee = src.arm_migration_tee(src_ks.lo.max(1), src_ks.hi, opts.delta_log_cap)` +
  `exclude_writer_claim(root_ino, hash56(writer_claim))` (:592–599) — the source's own heartbeat
  xattr "must neither travel with the slot nor keep the cutover's tee-empty recheck from settling"
  (backend.rs:986–990).
- `ArmedMigration` RAII: every early exit disarms tee + gate (:456–471).

### 3.3 Phase A — bulk copy from read snapshots, then delta rounds (:608–640)

**Bulk copy** `bulk_copy_slot(src, src_ks, dst, dst_ks)` (:330–365): `scan_slot_keyspace` (:298–326)
walks the THREE user kinds (`USER_KINDS = [INODES, DENTRIES, XATTRS]`, backend.rs:13230) over
`tree_bounds` (:245–260 — inode `[lo,hi]`, dentry `[dentry_key(lo,0,0), dentry_key(hi, HASH54_MAX,
255)]`, xattr likewise with `HASH56_MAX`) in pages of 512 via `range_kind` (latch-free arc-swap
snapshots — "read-side latch-free, no user-visible pause", design-volume-lifecycle :364), resuming
at `key_successor(last)`; `is_pinned_control_record` (:262–294) skips the volume's own
`writer_claim`, `writer_term`, `membership_owner`, `claim_set` xattrs at the keyspace root
(`client:` heartbeats DO travel). Every record is key-translated and committed to the target in
batches of `COPY_BATCH_RECORDS = 128` through `migration_apply` (backend.rs:12212–12227: ONE
ordinary conveyor tx of puts + deletes, guard set EMPTY — "the migrating keyspace has no other
writer — bulk-copy targets an unpublished guest keyspace"). `meta_slot_records_copied += copied`.

WHICH snapshot: not one frozen point — each page is the tree's current arc-swapped snapshot; the
copy is a moving walk, and the tee (armed BEFORE the walk) catches everything that changes under it.
HOW bounded: by the slot's record count (one pass) and the per-tx batch of 128; nothing else.

**Delta rounds** (:613–640): `tee.drain_round()` → `(keys, overflowed)`. Overflow ⇒
`consecutive_overflows += 1`, `report.overflows += 1`; 3 consecutive ⇒ `"slot {s} migration aborted:
the delta side log overflowed 3 consecutive rounds (cap {cap} keys) — the mutation rate outruns the
copy; retry at a quieter time or raise the cap (§5.5.2 overflow rule)"`; else `continue 'snapshot`
(a FRESH full snapshot pass — "correct by construction — the snapshot supersedes the lost keys").
No overflow ⇒ `apply_delta` (:388–412: for every teed `(kind, key)` RE-READ the current value from
the source (`migration_read_record`, a latch-free point read, backend.rs:12231–12233) — `Some ⇒ Put`,
`None ⇒ Delete` on the translated key, batched 128), `meta_slot_delta_keys += n`; `keys.len() >
cutover_threshold` ⇒ chase again, else enter the cutover window.

**The tee** `MigrationTee{lo, hi, cap, log: Mutex<HashSet<(u8, Vec<u8>)>>, overflowed: AtomicBool,
exclude_xattr}` (backend.rs:976–991). `note_committed(recs)` (:1018–1064) runs on the LANE at each
tx's terminal success (:23762–23816: "the lane IS the key tee — every user commit on the volume
passes through here, so one armed-tee load per group captures every migrating-keyspace key with
zero cost when no migration runs"): skips kinds `ALLOC`, `BLOCK_REFS`, `BLOCK_MAP` ("Neither … is
ino-keyed, so `key_owner` would read a foreign field as an ino … The block-map tree IS ino-keyed, but
its migration story is PR 4/5's"), takes the first 8 BE bytes as the owner, filters `[lo, hi)`,
drops the excluded xattr, then under the mutex: `len ≥ cap` ⇒ latch `overflowed`, `clear()`,
`meta_slot_delta_overflows += 1`; else `insert` (dedup: "per-key LWW means one re-read covers any
number of commits"). WHY keys only: "records do not carry journal sequence numbers and the journal
ring (8–32 MiB) wraps many times during a large-slot copy, so 'replay since seq X' is unavailable …
keys only, not values — values are re-read at delta-apply time from the authoritative tree, so the
log is ≈ 40 B/key; cap 1 M keys ≈ 40 MiB, charged to the node-cache budget" (design-volume-lifecycle
:365). Node-seq diffing was rejected: compaction "churns node identity without changing record
contents". Cross-volume legs: "the tee sits in the per-volume pass task, so a cross-volume op's
slot-`s` leg is captured on whichever volume commits it" (:376).

### 3.4 Phase B — the cutover window (`cutover_window`, :717–870)

The gate (`slot_gate_core.rs`): `SlotGate{closed: AtomicBool, inflight: AtomicU64}`;
`try_enter`: `inflight.fetch_add(1, AcqRel)`; `fence(SeqCst)`; `closed.load(Acquire)` ⇒ back out
(`fetch_sub`) and `false` (:64–74); `exit`: `fetch_sub` (:77–80); `close`: `store(true, Release)`;
`fence(SeqCst)` (:85–88); `reopen`: `store(false, Release)` (:91–93); `drained`: `fence(SeqCst)`;
`inflight.load(Acquire) == 0` (:104–108). "The load-bearing race is the classic store-load window …
Both sides pair their write with an explicit `fence(SeqCst)` before their read (the house Dekker
idiom) … There is no third interleaving in which an admitted op escapes the drain" (:6–22);
loom-checked with weakening.

Op side (`mod.rs:3595–3646`): `slot_gate_enter(inos)` — `armed == 0 ⇒ return` (one relaxed load);
per slot not already entered, loop `{ notified = gates.notify.notified(); if gate.try_enter() break;
parked = true; meta_slot_gate_parked_commits += 1; notified.await }` (register-then-check — no lost
wake); `SlotGatePass` exits every entered gate on drop = "the op's terminal outcome … what the
cutover's drain waits on" (:1197–1211). Position: "checked after lattice 1–3 … before 4a; never
re-checked while holding 4a or 4b — so a parked op holds NO DLM guards and no node locks … A
cross-slot rename spanning `s` and `t` parks whole before acquiring either leg's guards"
(design-volume-lifecycle :374). Deadlock argument: "the cutover task never acquires lattice-1–3 locks
and never waits on parked ops; parked ops hold only lattice-1–3 locks, which the cutover task never
needs" (:375). Non-escalation carve-out: gate parks are "planned, bounded" — tagged
`meta_slot_gate_parked_commits`, NEVER `disabled_volumes`; the escalation is
abort-the-cutover-and-retry (:377).

The window:
1. `t0 = Instant::now(); gate.close()` (:731–732); the `GateClosed` test hold (:742).
2. DRAIN: loop `{ if gate.drained() break; if t0.elapsed() > deadline ⇒ reopen + wake, Retry;
   yield_now().await }` (:746–756) — "they complete through the conveyor as normal — the cutover task
   holds nothing they need".
3. `src.drain_pending_times_now()` — parked Δtime refinements ride ordinary commits "so the tee-empty
   recheck below can settle" (:757–758).
4. FINAL DELTA: `tee.drain_round()`; overflow ⇒ Retry; `apply_delta` through the TARGET's conveyor —
   "this task takes no leaf locks; the 4b taker population stays closed" (:760–772).
5. RECHECK `tee.pending_len() != 0 || tee.has_overflowed()` ⇒ `"late commits landed after the final
   delta"` Retry — "never flip over a hole" (:773–779); deadline ⇒ Retry (:780–783).
6. **Past this point the window never aborts** (:785–787). Write 0: `dst.checkpoint_now()` — the
   final delta durable on the target (:789–791). Crash seam `crash_after_write == Some(0)`.
7. Travelling cursor: `src.next_ino()` if the source keyspace is legacy, else
   `src.guest_cursor_snapshot(slot).unwrap_or(2)` (:798–803). `epoch = max(src.e, dst.e) + 1` (:805–809).
8. Write 1 — TARGET-FIRST: `dst` stamp: `set_epoch = E`, `slots_hosted.insert(slot)`, cursors
   `retain(≠ slot)` + `push((slot, cursor))` sorted; `dst.install_guest_cursor(slot, cursor)`;
   `dst.set_membership_stamp`; `dst.checkpoint_now()` (:812–827) — a ledger write = one durable,
   torn-safe A/B slot (design-volume-lifecycle :353). Seam `Some(1)` ("dual claim — highest epoch wins").
9. Write 2: `src` stamp: `set_epoch = E`, `slots_hosted.remove(slot)`, cursor dropped,
   `src.remove_guest_cursor`; `src.checkpoint_now()` (:835–848). Seam `Some(2)`.
10. RAM swap: `new_map[slot] = target; routed.publish_slot_map(new_map)`; `gate.reopen()`;
    `wake_slot_gate_waiters()` (:858–862) — "parked ops re-route through the new map when the gate
    reopens". `report.cutover_ms = max(window_ms)`; `meta_slot_cutover_ms_max.fetch_max` (:864–869).

A `Retry` outcome: `cutover_retries_this_round += 1`; `≥ max_cutover_retries (16)` ⇒ `"slot {s}
migration aborted: 16 consecutive cutover windows missed the {ms}ms deadline — abort-and-retry
exhausted (§5.5.2a; never disabled_volumes)"` (:660–672); else more delta rounds.

WHY target-first: "source-first would create a crash window where NO volume claims `s` (orphaned
slot ⇒ refuse-loud territory for user data that is perfectly intact). Target-first's worst case is a
dual claim, which the per-slot highest-epoch-wins rule resolves deterministically; a same-epoch dual
claim is impossible by construction (only the coordinator writes stamps, one flip at a time per
slot) and is treated as corruption" (design-volume-lifecycle :394).

### 3.5 Phase C — post-flip (:677–689) and crash resolution
- `armed.defused = true; src.disarm_migration_tee(); routed.disarm_slot_gate(slot)` (wakes every
  parker, `mod.rs:3576–3584`).
- `teardown_slot_keyspace(src, src_ks)` — "strictly after the flip is complete … so an aborted/torn
  flip never faces missing source data" (design-volume-lifecycle :368); batched deletes of 128 through
  the conveyor; node frees ride the SMO pending-free protocol.
- Write 3: `update_slot_map_mirror` (:912–954) — the `FormatConfig` JSON xattr's `meta_slot_runs`
  (best-effort, warn on failure). Seam `Some(3)`. `meta_slot_migrations += 1`.

Crash table (design-volume-lifecycle :383–398): after write 0 ⇒ old map, B carries a complete
unclaimed copy ("harmless — torn-down by re-run"); after write 1 ⇒ dual claim, B@E vs A@E−1 ⇒ B
hosts `s` ("safe, because write 0 guarantees B's copy is complete before B may ever claim"); after
write 2 ⇒ new map fully expressed in stamps; after write 3 ⇒ complete. HOW both stamps are read at
mount: `observe_meta_set` reads EVERY member's newest valid ledger record (A/B newest-valid-wins —
a torn claim slot falls back to the pre-claim record, `test_flip_torn_claim_slot_falls_back…`,
meta_slot_migration_tests.rs:935–939), and `discover_meta_set` step 7 keeps the higher-epoch claimant
per slot. The re-run converges: `src == target` ⇒ `finish_flip` (:875–908) — for every OTHER volume
whose stamp still claims the slot, "the idempotent re-do of flip write 2" at the target's epoch, then
`teardown_slot_keyspace` of that volume's keyspace for the slot, then the mirror.

### 3.6 Counters (reference) and the exact-sum phases Z/ should publish

Reference (fuse_client.rs:7464–7485): `meta_slot_gate_parked_commits` (one per park EVENT),
`meta_slot_migrations`, `meta_slot_records_copied`, `meta_slot_delta_keys`,
`meta_slot_delta_overflows`, `meta_slot_cutover_ms_max` (max gauge — "the G-VL-4 p99-window
instrument"), `staging_drain_barriers`; stats-inode (:12839–12867): `meta_routing_width`,
`meta_slot_mint_spread`, `meta_slot_stamp_runs_max`, `meta_slot_stamp_cursors_max` ("the
encoding-budget pressure gauges — approaching `STAMP_MAX_RUNS` / `STAMP_MAX_CURSORS` means
consolidate before a migration preflight refuses"). Targets: "< 250 ms per slot (measured gate)";
abort at 1 s (design-volume-lifecycle :366, :377).

---------------------------------------------------------------------------
## 4. Offline `add-meta` / `remove-meta` / `repair-set`; the staging drain + rebind
---------------------------------------------------------------------------

All three are OFFLINE verbs taking the `sqmeta://` URI (main.rs:1355–1383, :1480–1497); a live
mount refuses (`"volume add-meta is an OFFLINE verb (design-volume-lifecycle §5.5.2): unmount first
and pass the sqmeta:// URI"`, :4832; remove likewise :5013). Each begins with `format_preflight(path,
live_gate = true)` on EVERY member — a live writer refuses even with `--force` (config_ops.rs
:2362–2368, :2828–2834, :1516–1522).

### 4.1 `add-meta <uri> <device> --take-slots <k | s1,s2,…>` (config_ops.rs:2343–2816)
1. Device already a member ⇒ `"device {d} is already a member of the metadata set"` (:2369–2373).
2. `observe_stamped_members` (:1936–1962): all stamped, one `set_uuid`/width, else `"metadata volume
   {p} carries no §5.5.1a membership stamp — … run `squeezefs volume repair-set` (inferable states) or
   reformat"` / `"metadata volumes {a} and {b} disagree on set identity/width — not one set"`.
3. RESUME detection (:2378–2400): the device's own newest stamp with THIS `set_uuid`; `resume_evidence
   = member_count == n + 1` ("writes 1..n of the crashed run landed").
4. (Rung-6 bit-11 uniformity checks, :2402–2452.)
5. `cfg` read (falls to the EXTENDED URI under resume evidence — "a stamped-ahead crash leaves the old
   URI refusing discovery", :2454–2476); `staging_dirs = staging_isolated_roots(cfg)` (:1902–1924);
   `old_gen` (recomputed from the old members' uuids in position order under resume evidence,
   :2481–2505).
6. `(epoch, position, resume)` = the device's stamp if present, else `(max + 1, max_position + 1,
   false)` (:2507–2519).
7. Slot selection (:2521–2601): the device's prior claim; or the explicit list (each `< W` else `"slot
   {s} outside the frozen routing width {W}"`; a member left hostless ⇒ `"taking {slots:?} would leave
   {p} hosting nothing — every member keeps at least one slot (use `volume remove-meta` to retire a
   member)"`); or `Count(k)`: `k == 0` ⇒ `"--take-slots 0 takes nothing — an added meta volume is only
   useful WITH slot migration"`; census = per member with ≥ 2 slots, record counts of the native slot
   and every CURSOR-BEARING slot via `slot_record_count` (an `open_probe` + keyspace scan,
   :1966–1976) — "virgin hosted slots are empty BY CONSTRUCTION … census as zero without a probe and
   only ever pad the ranking's tail" (design §5.6 :320–325); each host keeps its least-loaded slot;
   top k; too few ⇒ `"--take-slots {k}: only {n} slot(s) are takeable (every member keeps at least one)
   — the W-granularity limit"`.
8. Format the new member (fresh runs only) with a HOSTLESS transient stamp `{set_uuid, epoch E,
   position, member_count = n+1, slots_hosted = ∅, native_slot = None, cursors = ∅}` — "the claim
   (slots + cursors) lands after the copy, per the §5.5.2b write-0-before-claim order" (:2603–2667).
9. KD-8 phase 1: `new_gen` from the extended URI; `staging_rebind_prepare(dirs, old_gen, new_gen)`
   (:2669–2684; §4.4).
10. (Bit 4 on every participant, :2687–2689 — gone in v4.)
11. D0-guarded opens: target + every source (:2692–2697). Body: `check_hash_seed_uniform` per source;
    per taken slot the source = the member whose stamp claims it at the HIGHEST epoch (a released
    source ⇒ resume path: take the target's cursor); `teardown(dst)` (re-run wipe) → `bulk_copy_slot`
    → cursor = `next_ino()` if legacy else `guest_cursor_snapshot.unwrap_or(2)` →
    `target.install_guest_cursor` (:2699–2734).
12. Write 1 (target-first): the new member's claim `@E`, count `n+1`, `slots_hosted = from_slots(taken)`,
    `native_slot = None`, cursors; `checkpoint_now` (:2736–2750). Seam `AddMetaCrash::NewMemberClaim`.
13. Writes 2..n: every old member `@E`, count `n+1`, `retain(∉ taken)`, cursors dropped,
    `remove_guest_cursor`; `checkpoint_now` each (:2757–2774). Seam `SurvivorStamps`.
14. Teardown of every source's keyspace for every taken slot (:2782–2790).
15. Release every guard (:2795–2803); KD-8 phase 2 `staging_rebind_finalize(dirs, new_gen)`; the
    config mirror (:2806–2814). Returns the taken slots.

Pinned: `test_add_meta_rerun_converges_after_new_member_claim_crash` /
`…_after_survivor_stamps_crash` (meta_slot_migration_tests.rs:1266–1333).

### 4.2 `remove-meta <uri> <victim>` (config_ops.rs:2823–3017)
1. `members.len() < 2` ⇒ `"cannot remove the last metadata volume"`; victim not listed ⇒ `"{v} is not a
   member of the metadata set"` (:2835–2848).
2. `epoch = max + 1`; `victim_slots`, `victim_native` (:2861–2863).
3. §5.2 META CAPACITY PREFLIGHT (:2865–2903): `victim_used = total − free` extents; `avail = Σ
   survivors' free`; `headroom = max over survivors of min(2 × max_dirty_nodes, total/4)` using THE
   SAME `resolve_max_dirty_nodes` the checkpoint task uses ("the preflight headroom must price the cap
   the survivors will actually run"); `avail < used + headroom` ⇒ `"volume remove-meta preflight refused
   (§5.2 meta side): survivors' free extents {a} < victim used {u} + checkpoint headroom {h} (extents of
   {node_size} B) — grow the survivors first; nothing was changed"`.
4. KD-8 phase 1 with the SURVIVOR set's generation (:2905–2913); (bit 4, :2915–2918).
5. Opens (:2920–2925). Body: every victim slot → the survivor with the MOST free extents that is not
   the slot's origin (`"no survivor can host slot {s} (v1 refuses migrating a slot back onto its origin
   volume)"`); `check_hash_seed_uniform`; wipe → copy → cursor → `install_guest_cursor` (:2927–2960).
6. SURVIVORS-FIRST stamps `@E`, count `n−1`, positions KEPT, `+ gained slots + cursors`,
   `checkpoint_now` (:2962–2984) — "the victim's disappearance is only ever expressed after the surviving
   set is fully self-describing".
7. VICTIM-LAST tombstone: `set_epoch = E`, `member_count = 0`, `slots_hosted = ∅`, cursors ∅, native kept
   (:2986–2998) — discovery refuses it by name if listed again (§1.5 step 3).
8. Release guards; phase 2 finalize on the survivor set; the mirror (:3001–3016).

Epoch completeness for MEMBERSHIP changes (design-volume-lifecycle :395): `E` is complete iff every
member of `E`'s set carries `set_epoch ≥ E`; an incomplete highest epoch refuses loud naming
`repair-set`. In the code this surfaces as the `member_count` disagreement refusal (§1.5 step 4),
whose text names the same-arguments re-run first.

### 4.3 `repair-set <uri>` (config_ops.rs:1508–1725)
Prints the observed state per volume (`"  {p}: set {..4}.. epoch {e} position {pos}/{count} width
{w} slots {runs:?}"` or `"NO membership stamp"`). Refusals prefixed `"volume repair-set refused: {msg}
— VL5a re-stamps only a coherent observed state …"`: stamps disagree `"(uuid/count/geometry)"`;
`"stamps declare {m} members but the URI lists {n}"`; out-of-range / duplicate position; same-epoch
dual claim `"— corruption"`. Then by stampless count:
- 0 ⇒ every slot must be hosted (`"slot {s} of width {W} is hosted by no member"`); re-stamp every
  member at `max_epoch` with `slots_hosted` RESTRICTED to the slots it WON and cursors restricted to
  kept slots — "converges §5.5.2b writes 1/2 for interrupted flips — keyspace residue is cleaned by
  re-running the migration" (:1629–1650).
- 1 ⇒ the kill-9 window: exactly one free position and the unhosted complement, coalesced to runs,
  ≤ `STAMP_MAX_RUNS` (`"the unhosted slot complement ({n} slots in {r} runs) cannot belong to one
  member"`) ⇒ an INFERRED stamp `{first.epoch, pos, complement, native = pos, no cursors}`
  (:1651–1700).
- n ⇒ `"{n} stampless members beside a stamped set — positions are not inferable"`.
Execution: bit 2 barriered first on every volume to be stamped (gone in v4), then each stamp rides a
guarded open's shutdown checkpoint (:1709–1724).

### 4.4 The staging drain barrier + generation rebind (KD-8)
Generation = `volume_set_generation` (§1.5) decorated with the node's writer scope
(`staging_generation_for_set`, :1757–1770). `staging_rebind_prepare(dirs, old, new)` (:1823–1869):
scan each isolated root's live staged custody; any `active_block:` / `active_block_ext:` key ⇒
`"staging drain barrier refused (KD-8): {n} pending write-custody unit(s) remain — mount the
filesystem, let writeback drain (sync + clean unmount), then re-run; nothing was changed. Units:
{..}"`; else write the TWO-PHASE dual marker `(old, new)` on every root bound to `old` (or `new`).
`staging_rebind_finalize(dirs, new)` (:1873–1895): roots whose bindings include a rebindable
generation get the single `new` marker; `staging_drain_barriers += 1`. `staging_drain_barrier`
(:1780–1813) is the prepare + restamp pair for a live barrier. Roots bound to a FOREIGN generation
are left alone ("their content was condemned before this barrier"). The design's argument: "Because
staging is provably empty at the flip, the discard-on-mismatch law is a no-op even if the restamp is
lost to a crash" (design-volume-lifecycle :402).

Z/: the marker is `StagingMarkerWire{set_uuid, set_epoch, prev_set_epoch (DUAL), …}` with the
identity `xxh3(set_uuid ‖ "staging")` (0002 row 99; `staging.zig:920–1054`). A membership change
changes `set_epoch`, never `set_uuid`, so: phase 1 = refuse pending custody, write `{set_epoch = E,
prev_set_epoch = E_old, DUAL}`; the flip; phase 2 = `{set_epoch = E, prev = null}`. `bindVerdict`
already treats `prev_set_epoch == ours` as same-epoch (`staging.zig:1050`). The reference's
"v3:hex|hex" string join and the scope decoration string are not carried.

---------------------------------------------------------------------------
## 5. Knobs, gauges, constants
---------------------------------------------------------------------------

### 5.1 Knobs
The reference has NO registry knob for routing or migration; `MigrationOptions` is a struct of
literals (slot_migration.rs:137–163) and the symmetric rows (`SQUEEZEFS_SYM_MINT_SLOTS` derived
`clamp(W/(2×writers), 1, 64)`, `SQUEEZEFS_SYM_AFFINITY_MAX_MB`, `SQUEEZEFS_SYM_DIR_STRIPES`,
env_knobs.rs:422–427) and `SQUEEZEFS_SLOT_PLACEMENT` (:349, the retired mw plane) are rung 6's.
Z/ rows to add:

| Row | Kind / range | Default | Class | Doc (the arm) |
|---|---|---|---|---|
| `SQUEEZEFS_TEST_MIGRATE_HOLD` | enum `none\|after_bulk_copy\|gate_closed` | `none` | `test_seam` | The engine parks at the named phase until the test releases it (the reference's `PhaseHold`; the e2e CLI leg's seam — in-process tests pass the hold struct directly) |
| `SQUEEZEFS_TEST_MIGRATE_CRASH_AFTER_WRITE` | int 0..=2 | absent | `test_seam` | Abort the flip after durable write N (the §5.5.2b crash matrix through the binary) |

No `derived_override` for the delta cap / threshold / deadline: each is a derivation (§5.3) and a
counted A/B only if a measurement asks.

### 5.2 Gauges / counters (reference → Z/ name, kind)
| Reference | Z/ | Kind | Note |
|---|---|---|---|
| `meta_slot_gate_parked_commits` | `routing_gate_parks` | `ShardedCounter` | one per park event |
| — | `routing_gate_armed` | `Gauge` | 0/1 per set (the fast-path word) |
| `meta_slot_migrations` | `migrate_slots_completed` | `ShardedCounter` | flip + teardown |
| — | `migrate_slots_aborted` | `ShardedCounter` | overflow ×3 / retries exhausted |
| `meta_slot_records_copied` | `migrate_records_copied` | `ShardedCounter` | bulk + delta puts |
| `meta_slot_delta_keys` | `migrate_delta_keys` | `ShardedCounter` | teed keys applied |
| `meta_slot_delta_overflows` | `migrate_delta_overflows` | `ShardedCounter` | each ⇒ fresh snapshot |
| — | `migrate_cutover_retries` | `ShardedCounter` | deadline / late-commit aborts |
| `meta_slot_cutover_ms_max` | `migrate_cutover_phase_ns` | `ShardedLatencyHistogram` exact-sum | `{drain, times_drain, final_delta, write0, write1, write2, swap, total}` |
| `meta_slot_mint_spread` | `MINT_SPREAD` is a constant | — | Z/ already publishes `slot_rotor` (`router.zig:55`) |
| `meta_slot_stamp_runs_max` / `_cursors_max` | `routing_stamp_runs_max` / `routing_stamp_cursors_max` | `Gauge` | the encoding-budget pressure pair |
| `meta_routing_width` | — | — | a comptime constant; `.stats` prints it as a fact, not a gauge |
| `staging_drain_barriers` | `staging_rebind_barriers` | `ShardedCounter` | prepare + finalize |
| `mint_volume_picks`, `rotor_mints`, `slot_rotor` | (exist, `router.zig:55–60`) | | |
| — | `routing_dual_claims_converged` | `ShardedCounter` | §7 Q1's mount-time write-2 |
| — | `routing_same_epoch_dual_claim` | `Tripwire` | discovery's "impossible by construction" |

### 5.3 Constants with WHY, and measured numbers
| Constant | Value | WHY (file:line) |
|---|---|---|
| `W` | 65 536 | the u16 slot id's value space (`mod.rs:41–50`); ≥ 100× any contemplated member count |
| `GUEST_NS_SHIFT` | 40 | ≥ 2^40 ≈ 1.1×10¹² locals per keyspace, 10× past the 100 M-inode cap (`mod.rs:35–39`) |
| `MINT_SPREAD` | 64 = 256/4 | ¼ of the cursor budget, ¾ for travelled cursors; ≥ 64 movable slices from birth (`mod.rs:52–61`) |
| `STAMP_MAX_RUNS` / `_CURSORS` | 128 / 256 | 3 765 B (ref) / 3 767 B (Z/) worst case < 4 096 (checkpoint.rs:266–281; `ledger.zig:86–108`) |
| `COPY_BATCH_RECORDS` | 128 | "well under the conveyor's byte caps for 64 KiB values" (slot_migration.rs:45–47) — a literal; Z/: derive from `journal.entryPayloadCap()` as `inos.packDestroyEntries` does (`inos.zig:256–269`) |
| `delta_log_cap` | 1 000 000 | ≈ 40 B/key ≈ 40 MiB "charged to the node-cache budget" (:138–140); Z/: `clamp(node_cache_budget / 32 ÷ KEY_SLOT, 64 Ki, 1 Mi)` with the key slot = `KEY_BUF_CAP` (36 B) — the budget-charged form the design asked for |
| `cutover_threshold` | 1 000 keys | "below which the cutover window opens" (:141–142); Z/: the delta one target-conveyor batch applies = `deriveBatchTxs(cpus) × copy_batch` (`conveyor.zig:152`) |
| `cutover_deadline` | 1 s | "a window that cannot drain + apply inside it aborts-and-retries" (:143–145); = `CHECKPOINT_MAX_AGE_MS` — the window holds three ledger writes (0/1/2), each ≤ one cadence barrier; target < 250 ms |
| `max_cutover_retries` | 16 | consecutive missed windows before loud failure (:146–147) |
| consecutive overflows | 3 | "three consecutive overflows abort the migration loudly" (design :365) |
| health cache TTL | 500 ms | per-mkdir allocator scan avoidance (`mod.rs:2538–2545`); Z/: the placement cadence |
| band | 9/10 | `health ≥ max × 9/10` (`mod.rs:2624`; `router.zig:165–166`) |
| scan page | 512 | `range_kind(…, 512)` (slot_migration.rs:310) |

Measured: §1.1's table; the one-volume journal ceiling 21–25 k meta device-writes/s with the sibling
at 0.00 (`mod.rs:2572–2574`); the 281-cursor stamp overflow that failed a volume (backend.rs
:12088–12092); the 8 KiB dense list at W = 4 096 that the ledger slot "physically cannot hold"
(design §2.2 :70); `--take-slots` census "65536 probe opens — hours" before the cursor-bearing
economy (design :72).

---------------------------------------------------------------------------
## 6. Tests (name — one sentence)
---------------------------------------------------------------------------

`tests/dynamic_meta_routing_tests.rs`:
- `test_dynamic_routing_bit_is_bit6_and_does_not_intersect_pre_campaign_mask` (:129) — bit 6 ∉ every prior mask (v4: version absorbs; not ported).
- `test_fresh_formats_carry_dynamic_routing_bit` (:154) — every format stamps bit 6 (not ported).
- `test_bit6_absent_v3_volume_refuses_loud_with_reformat_guidance` (:185) — a cleared bit 6 refuses naming reformat (v4: `LegacyFormat`).
- `test_derived_width_is_the_slot_id_namespace` (:233) — `W == u16::MAX + 1`.
- `test_plan_derives_width_one_run_per_member_and_fits_budget` (:242) — one run per member at V ∈ {1, 2, 8}, stamp ≤ budget.
- `test_routing_round_trip_at_derived_width` (:277) — `make_global ∘ route = id` over the ino space at W = 2^16.
- `test_slot_set_coalesces_and_mutates` (:295) — insert/remove re-coalesce; structural equality.
- `test_slot_set_from_runs_validation_refuses_malformed` (:328) — count 0, stride 0, escape, overlap refuse.
- `test_stamp_round_trips_through_the_ledger_slot` (:414) — encode/decode identity with runs + cursors.
- `test_encoding_budget_caps_encode_at_cap_refuse_past_it` (:430) — exactly 128 runs / 256 cursors encode; +1 refuses loud.
- `test_mint_spread_rotates_across_the_mint_set_and_survives_remount` (:517) — 256 mints in exactly 64 slots; post-remount mints never collide.
- `test_created_inos_stable_across_slot_migration_and_remount` (:558) — 96 creates, migrate a loaded non-root slot, every st_ino identical live and after remount.
- `test_discovery_is_uri_order_independent_at_derived_width` (:626) — forward vs reversed URIs: same order, map, generation.
- `idle_cost_probe` (:661, ignored) — the §5.9 instrument.
- `test_meta_slots_flag_refuses_naming_successor` (:707) — the retired CLI flag's refusal.

`tests/meta_slot_tests.rs`:
- `test_w1_routing_is_byte_identical_identity` (:323) — W = 1 routes as identity incl. ino 1 and the ino-2 edge.
- `test_route_make_global_round_trip_and_distribution_over_width` (:345) — round trip + uniform slot distribution.
- `test_in_ram_constructor_routes_identity_for_one_volume` (:379) — the W ≤ 1 constructor.
- `test_torn_newest_stamped_slot_falls_back_to_predecessor` (:437) — a torn newest ledger slot yields the previous stamp.
- `test_plan_width_bounds_and_the_retired_64x_cap` (:473) — `W < n` and `W > 65536` refuse; the 64× cap is gone.
- `test_plan_meta_slot_set_identity_distribution_and_hosted_runs` (:507) — `slot k → k mod V`, one run each.
- `test_default_format_is_a_single_member_dynamic_set` (:543) — a plain format is a stamped 1-member set.
- `test_reordered_uris_discover_same_set_and_same_generation` (:651) / `test_reordered_uri_mount_resolves_same_inos` (:701) — order independence through to inos.
- `test_discovery_disagreements_refuse_loud_naming_volumes` (:744) — duplicate position, missing member (counts named), epoch spread MOUNTS, same-epoch dual claim refuses naming both, foreign uuid, mixed stamped/stampless.
- `test_repair_set_restamps_coherent_state_and_refuses_incoherent` (:918) — the three stampless arms.
- `test_stamp_survives_mount_write_checkpoint_remount` (:1026) — the stamp rides every checkpoint.

`tests/meta_slot_migration_tests.rs`:
- `test_guest_ino_namespace_partition_disjoint_and_round_trips` (:294) — `guest_local_ino` / `split_guest_local` disjoint from natives.
- `test_stamp_round_trip_with_and_without_cursors` (:324).
- `test_slot_migration_equivalence_isolation_and_ino_stability` (:378) — slot and whole-set logical digests ∅-diff post-cutover and post-remount; post-migration mints never reuse; a non-participant's state untouched.
- `test_delta_tee_captures_concurrent_writes_values_reread` (:483) — a create, two xattr rewrites (the SECOND value wins) and an unlink during the copy all hold post-cutover.
- `test_delta_overflow_fresh_snapshot_fallback_and_triple_abort` (:583) — a tiny cap overflows into a fresh snapshot; three consecutive abort loud.
- `test_cutover_gate_parks_cross_slot_rename_with_zero_guards` (:710) — a rename across the migrating slot parks (counter moves), both parents' exclusive guards are acquirable (zero guards held), `disabled_volumes` stays empty, the rename lands post-flip.
- `test_cutover_deadline_aborts_and_retries_never_escalates` (:826) — a 50 ms deadline forces ≥ 1 abort-and-retry; converges; no fail-stop.
- `test_flip_crash_windows_resolve_and_rerun_converges` (:873) — crash after writes 0/1/2/3 ×3: discovery lands in the table (window 0 old host, 1–3 new host), re-run converges to the pre-migration digest.
- `test_flip_torn_claim_slot_falls_back_and_rerun_converges` (:939) — a corrupted claim-carrying ledger slot falls back to the pre-claim record; old map serves; re-run converges.
- `test_staging_drain_barrier_refuses_custody_and_restamps` (:1038) — pending `active_block:` custody refuses by name; a clean root is restamped.
- `test_add_meta_and_remove_meta_end_to_end_ino_stable` (:1102) — grow then shrink; every ino stable.
- `test_remove_meta_capacity_preflight_refuses_honestly` (:1199) — the §5.2 numbers in the refusal.
- `test_add_meta_rerun_converges_after_survivor_stamps_crash` (:1271) / `…_after_new_member_claim_crash` (:1333) — the two named add-meta windows re-run with the same arguments.
- (`test_slot_migration_bit_is_bit4…` :234, `test_bit4_gates_extended_stamps…` :257 — bit-era tests, not ported.)

`tests/meta_plane_distribution_tests.rs`: `test_per_volume_journal_write_counters_count` (:116),
`test_single_dir_file_storm_spreads_inode_placement` (:159), `test_single_dir_workload_journal_distribution_balanced`
(:202), `test_dir_placement_stripes_across_volumes_pin` (:268) — files AND dirs of one directory
spread over the healthy band so per-volume journal entries balance.

Loom models (slot_gate_core.rs:22–26, slot_cursor_core.rs:21–23): the gate's enter/close/drain
interleavings with the fences weakened; the cursor's mint/publish/install edges. Z/: `src/model/
explorer.zig` models with `expectHolds` + `expectFails` on the dropped fence (AGENTS "Model checks").

---------------------------------------------------------------------------
## 7. QUIRKS (decision 0002) and the better designs
---------------------------------------------------------------------------

**Q1 — A volume-wide `set_epoch` carries per-slot claims, so an UNRELATED flip can resurrect a
stale claim.** No per-slot epoch exists (§1.4). Scenario: flip `s: A→B` crashes after write 1 (B@E
claims `s`, A@E−1 still claims `s` — teardown never ran). Mount resolves `s → B`; writes land on B.
Then flip `t: A→C` completes: A's stamp goes to `E+1` with `s` STILL in `slots_hosted`
(`slots_hosted.remove(t)` only, slot_migration.rs:839–841). Next discovery: `s` claimed A@E+1 vs B@E
⇒ A wins; the writes on B are now residue the next re-run WIPES (:588–590). `finish_flip` runs only
for the SAME slot's re-run (:555–561) and `repair-set` only if invoked. Better design (Z/): **a
writable open converges every dual claim before serving** — after the D0 ladder, for every slot with
two claimants the mount writes the loser's release (the idempotent write 2 at the winner's epoch,
exactly `finish_flip`'s stamp step) and tears down the loser's slot tree; `routing_dual_claims_converged`
counts it; a `-o ro` mount only resolves. The encoding stays O(runs); no per-slot epoch is stored.

**Q2 — The `FormatConfig` JSON mirror as flip write 3** (`update_slot_map_mirror`, :912–954;
`lib.rs:1409–1420`). Already ruled: not carried (0002 rows 46, 121, 123). The Z/ flip is TWO durable
writes (B then A) + the RAM `RouteTable` swap; charter §5.4's "config mirror" is satisfied by the
stamps being the ONE truth. Record this as the charter clause's reading in the evidence note.

**Q3 — Record-at-a-time bulk copy / teardown of a slot that IS a tree.** The engine scans three
per-kind key ranges in 512-record pages and commits 128-record conveyor txs (:298–365), tears down
by batched deletes (:370–383), skips refs / block-map / alloc kinds in the tee "until the walkers
land" (backend.rs:1020–1034) and copies ONLY kinds 1–3 (`USER_KINDS`) — a slot's block map and
block refs do NOT migrate. In Z/ a slot is ONE mixed-kind tree (`TreeIdentity{.slot}`): **bulk copy =
tree copy** — walk the source slot tree's leaf snapshots (`KvTree.range` over `[0, KEY_SPACE_MAX)`,
`tree.zig:807`), build the target slot tree's leaves with `builder.zig` into fresh extents on B
(images barriered; interior built bottom-up; root published into B's tree 0 `slot_state:{s}` —
charter §5.5 "a guest slot tree's root lives in tree 0"; the port already names this home,
`tree.zig:327–334`), ZERO journal traffic per record, every kind travels; the delta applies through
B's conveyor into the new tree as ordinary txs. **Teardown = drop the tree**: one journaled
`Delete(slot_state:{s})` + the tree's extents into pending-free gated on coverage (charter §5.3) —
O(nodes), not O(records). The tee keys by `key.slotTreeKeySlot` (refs by owner, `key.zig:645–649`) —
no kind skip-list, no `legacy_recs` strip.

**Q4 — Per-volume control records inside the slot keyspace** (`writer_claim`, `writer_term`,
`membership_owner`, `claim_set` xattrs at local ino 1; `is_pinned_control_record` :262–294;
`exclude_writer_claim` :592–599; `try_make_global_ino`'s "control record with no global encoding"
:3238–3258; `slot0_root_ino` backend.rs:12241–12248). Better design: every per-VOLUME control record
lives in tree 0 (`TREE_CONTROL`), never in a slot tree — Z/ already puts `set_config` there
(`set_config.zig:1–7`); the B2 `writer_claim` (charter §5.7, not yet landed) goes there too. Then
NOTHING in a slot tree is pinned, Q3's tree copy is exact, and the tee needs no exclusion.

**Q5 — Cursors in the stamp vs with the lease.** The reference's forest arm clears stamp cursors
(backend.rs:12080–12092) after the 281-cursor incident and homes them on the lease. Z/ keeps
`slot_cursors` in the v4 stamp (`ledger.zig:243`) with `MINT_SPREAD = STAMP_MAX_CURSORS / 4`
(`router.zig:398`). For the one-member forest this is bounded (≤ 64 rotor + travelled ≤ 256; a
migration preflight refuses past the cap naming consolidation, design §5.3 :266–269). Charter §5.5
also names `slot_state:{s} → Unleased{root, cursor, …}` — rung 6 must treat both as FLOORS (max
wins at mount) and the stamp copy becomes redundant on an armed set. Decide at rung 6; record here
so neither wave re-imports the other's shape.

**Q6 — "migrating a slot back to its origin volume is not supported in v1"** (:503–508,
config_ops.rs:2933–2947) because the native keyspace "may carry residue" and `native_slot` is kept
forever (`st.native_slot = st.resolved_native_slot()` on every re-stamp, :817, :839). Better: when a
volume's native slot migrates away and its tree is torn down (Q3), stamp `native_slot = null` (the
v4 stamp supports it, `ledger.zig:238–241`); a return lands as guest slot tree `s + 1`. The
refusal and the remove-meta "no survivor can host" arm vanish; `/`'s dentries move with slot 0 like
any slot (volume 0's native tree is not special once control records are in tree 0, Q4).

**Q7 — Literals in `MigrationOptions`** (1 M, 1 000, 1 s, 16, 128; :45–47, :154–162) — charter §1.5.
Derivations in §5.3's table; each with a tie test.

**Q8 — The drain loop spins on `yield_now()`** (:746–756) and the deadline uses `std::time::Instant`
(:731). Z/: the gate's last `exit()` notifies a `sync.Notify` the cutover parks on
(register-then-check); `clock.monotonicNs` for the deadline; the drain is a wait, never a poll.

**Q9 — The tee's side log is a `HashSet<(u8, Vec<u8>)>` under a `parking_lot::Mutex` on the pass
task** (backend.rs:983–984, :1050–1062) — an allocation + a lock per committed record while armed,
on the commit path. Z/: a preallocated open-addressing set of fixed-width `KeyBuf` slots
(`key.zig:119–121`) owned by the pass fiber (the sole producer), the drain swaps generations through
an atomic pointer; charged to the budget as a non-sheddable component; zero per-record allocation.

**Q10 — `SlotGates` as `scc::HashMap<u64, Arc<SlotGate>>` + an `armed` counter + one process-wide
`Notify`** (`mod.rs:1181–1195`). With `migration_lock` making migrations serial (:911–913), at most
one gate is armed per set: Z/ holds ONE `std.atomic.Value(?*SlotGate)` per set (`.acquire` load is
the fast path, null = unarmed) and the gate carries its own `Notify`.

**Q11 — `static META_COUNTER`, `static HEALTH_CACHE` with a 500 ms `Instant` TTL, `f64` health,
an allocator probe per call** (`mod.rs:2531–2561, 2627–2631`). Ruled (0002 row 102): `Router` fields,
integer ppm, the placement snapshot refreshed on the cadence — `router.zig:360–393` is the landed
shape; bind the META volumes' free/total extents into it.

**Q12 — `routing_width` stored, `native_slot_plus1 u32`, the stamp as an optional suffix, bits 2/4/6
as presence-required format bits, `set_slot_migration_bit` + `sync_device` at every migration
start** (checkpoint.rs:186–217; superblock.rs:125, 147, 178; :583–586). Ruled (0002 rows 41, 43):
v4 has none; discovery's `routing_width` disagreement refusal does not exist.

**Q13 — `check_hash_seed_uniform` names the retired `format --meta-slots`** (:696–707). Z/: the seed
is set-wide from birth (0002 row 42); equality is a DISCOVERY refusal naming both volumes (§1.5).

**Q14 — `finish_flip` scans EVERY volume's keyspace for the slot on each re-run** (:880–906). Z/:
tear down only the volumes whose stamp claimed the slot plus the record's source; with Q3 a teardown
is one `slot_state` delete.

**Q15 — `--take-slots k` census = an `open_probe` + a full keyspace scan per cursor-bearing slot**
(config_ops.rs:1966–1976, :2573–2585). Z/: a slot tree's load is its EXTENT COUNT / leaf bytes, which
tree 0's `slot_state:{s}` records (charter §5.5 `slot_tree_extents`, the `A_max` input) — no scan.

**Q16 — `MetaSetDiscovery.slot_to_volume: Vec<usize>` (512 KiB per mount, measured)** (design §5.9
:378). Z/: `[W]u16` (128 KiB), the design's own u16 figure.

**Q17 — The `W ≤ 1` identity arm on the product path** (`mod.rs:3175–3177, 3191–3193, 3201–3203,
3212–3214, 3267–3269, 3446–3449`). W is a constant 65 536 in every product set; the arm exists for the
in-RAM test constructor (`RoutedMetaBackend::new`, :1218–1248). Z/ keeps `routeIno(ino, width)`
width-parametric for the geometry tests (`inos.zig:303–317`) but the product router calls it with
`ROUTING_WIDTH` only — no runtime `W ≤ 1` branch in the multi-volume backend.

**Q18 — Two code paths for one keyspace law (`SlotKeyspace.legacy`, `translate_ino`, `root_ino`
:180–241).** In Z/ the forest slot of slot `s` on volume `v` is ONE function (`native_slot == s ? 0 :
s + 1`) and a tree copy (Q3) rewrites the key ino's top 24 bits once per record only on a native↔guest
move (`key.guestLocalIno` / `splitGuestLocal`); there is no "legacy" class.

**Q19 — The `migrate-meta-slot` CLI replies with a job id and points at `meta_slot_cutover_ms_max`
on `.stats`** (main.rs:4995–5008). The job fabric is rung 8 (charter §8); until then Z/'s verb runs
the engine inline over the admin lane and prints the report; the phase histogram (§5.2) replaces the
max gauge.

---------------------------------------------------------------------------
## 8. Z/ landing shape (what each module owes)
---------------------------------------------------------------------------

- `src/meta/route_table.zig` (portable): `RouteTable{slot_to_volume: [W]u16, native_slot: [V]?u16,
  rotors: [V]MintRotor}` behind `ArcSwap`; `fromDiscovery`, `publish(slot, volume)`; `validate` with
  §1.2's refusals; `volumeOf(slot)`, `forestSlotOn(volume, slot)`.
- `src/meta/discovery.zig` (Linux, device reads): `observe(paths) → [Observation{path, uuid,
  hash_seed, stamp}]`, `discover(obs) → Discovery` with §1.5's refusals as a typed `Refusal` union
  with `write(*Writer)` (the `set_config.DiscoveryRefusal` shape, `set_config.zig:714–757`), the
  dual-claim convergence list for Q1.
- `src/meta/slot_gate.zig` (portable): the Dekker gate + `Notify`; `src/model/slot_gate_model.zig`
  with both fences as weakenings.
- `src/meta/migration_tee.zig` (portable): the bounded key set; `conveyor.Window.finish` calls
  `noteCommitted` per successful tx when armed (one `.acquire` load otherwise).
- `src/meta/slot_migration.zig` (Linux): the engine over `KvBackend`s — tree copy via `builder`,
  delta via the target's conveyor, the window, the two stamp writes through `checkpointNow`
  (`backend.zig:968`), the swap, the drop-tree teardown; `MigrationOptions` derived.
- `src/meta/set_backend.zig` (Linux): the multi-volume facade routing every op through the table +
  the gate; `pickMintVolume` over the volumes' allocator healths; `allocateInoInSlot`.
- `src/cli/volume.zig`: `add-meta`, `remove-meta`, `repair-set`, `migrate-meta-slot` (§4, §3) with
  the staging prepare/finalize over `staging.Marker`.
- `ledger.zig`: no wire change; `checkpoint_cycle` takes a stamp HOOK (§1.6).
- Evidence note `docs/evidence/rung-05-dynamic-routing.md`: the crash matrix (windows 0/1/2 ×
  torn), the gate model's weakenings, the measured cutover phases, the Q1 convergence test.
