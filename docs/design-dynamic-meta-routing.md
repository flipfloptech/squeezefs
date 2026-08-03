# Design: Dynamic Meta Routing — the derived virtual width

**Status:** Rev 3 — 2026-08-03, IMPLEMENTED (campaign `feat/dynamic-meta-routing`;
§9 records DLM stage S4 as the slot map's second consumer).
**Charter:** direct user ruling (2026-08-02, verbatim): the frozen, user-chosen
routing width is "a horrible / restrictive design decision... it needs to be
dynamic." The no-fixed-constants law and the forward-only law (no backwards
compatibility — old formats refuse loud, reformat required) both apply.
**Supersedes:** the user-visible width surface of
`docs/design-volume-lifecycle.md` §5.5.1 (KD-7's `format --meta-slots` +
`W ≤ 64 × volumes` bound + the OQ-2 "default W = volume count" ruling). The
§5.5.1a stamp-discovery protocol, the §5.5.2 migration engine, the §5.5.2a
cutover gate, and the §5.5.2b flip protocol are all **unchanged** — this
design changes W's magnitude, derivation, visibility, and at-rest encoding,
nothing about how slots move.

---

## 1. The wart, precisely

VL5a froze metadata routing as durable `routing_width W` = the meta volume
count at format, or the `format --meta-slots <n>` override, bounded
`volumes ≤ W ≤ 64 × volumes`. Consequences:

1. **W is a format-time guess a user must make.** Nobody knows their
   five-year meta-volume count at format time.
2. **Growth beyond W volumes is impossible without reformat** (every volume
   must host ≥ 1 slot).
3. **Migration granularity is 1/W** — a default (W = V) format can never
   spread: every slot is its host's only slot.

The slot machinery itself is already dynamic-capable: durable slot map,
guest keyspaces (incompat bit 2/4), ONLINE `migrate-meta-slot` (conveyor
delta tee, per-slot cutover gate before 4a, target-first ordered flip). The
only restrictive part is W's magnitude and visibility.

## 2. The actual constraints (verified against the code)

### 2.1 What pins global inos — the eternal-stability invariant

The ino ↔ slot binding is pure arithmetic over W
(`src/meta_backend/mod.rs:314-335`):

```
route:   slot = (ino − 2) % W        local = (ino − 2) / W + 2      (ino 1 → slot 0, the root pin)
inverse: ino  = (local − 2) · W + slot + 2
```

**W is baked into every global ino ever surfaced to the kernel** (st_ino,
NFS handles, dentry child references stored in dentry records, `block_map:`
keys, DLM stripe names). Changing W re-derives every ino ⇒ st_ino
instability + on-disk key rewrites — exactly the rejected-alternative class
KD-7 documented (consistent hashing, ino-high-bits). Therefore:

> **W stays frozen per set, forever. "Dynamic" is achieved by making W so
> large — and invisible — that the slot→volume map (the indirection that is
> already online-migratable) is the only thing that ever needs to move.**

Slot identity is additionally embedded durably in **guest local-ino
namespaces**: a guest record's key ino is `(slot + 1) << 40 | raw`
(`GUEST_NS_SHIFT = 40`, slot as **u16** — `mod.rs:34-56`), and in the stamp
wire (`slots_hosted`, `slot_cursors`, u16 slot ids —
`kv/checkpoint.rs:181-225`). The slot-id **type** is the structural ceiling:
`W ≤ 2^16`.

### 2.2 Per-slot costs at rest (what breaks at W = 4096 / 65536)

| Cost site | Shape today | At W = 4096, V = 1 | At W = 65536 |
|---|---|---|---|
| `MembershipStamp.slots_hosted: Vec<u16>` — **dense list, 2 B/slot** in the 4096-B A/B root-ledger slot (`checkpoint.rs:211,258`) | ≤ 64 slots/volume ⇒ ≤ 128 B | 4096 hosted slots = 8 KiB — **`encode_slot` refuses; the ledger slot physically cannot hold it.** This is what `MEMBERSHIP_MAX_HOSTED_SLOTS = 64` and the `W ≤ 64 × V` format bound exist for | 65536 slots = 128 KiB — same, 32× worse |
| `FormatConfig.meta_slot_map: Option<Vec<u16>>` — JSON mirror on the ino-1 xattr (`lib.rs:576`) | W entries | ~12–20 KB JSON — fits but bloats every config read | ~300–400 KB JSON — **exceeds the xattr value cap `min(65536, node_size/4)`** |
| `--take-slots k` census (`config_ops.rs:1288-1298,1490-1505`) | one `open_probe` + full keyspace scan **per hosted slot** | 4096 probe opens | 65536 probe opens — hours |
| `RouteTable.slot_to_volume: Vec<usize>` (RAM, per mount) | 8 B × W | 32 KiB | 512 KiB (128 KiB as u16) — measured, §5.9 |
| `validate_slot_map` / discovery claim resolution / plan expansion — O(W) passes at open | µs | measured 24–31 µs / 100–131 µs / 165–186 µs per pass at W = 65536 (§5.9) | same |
| Per-slot cutover gates, tee side-logs | armed per **migrating** slot only | 0 idle | 0 idle |
| Journal / conveyor / checkpoint / node format | **nothing per-slot** | — | — |

Only the first three rows are real. Everything else is µs- and KiB-scale.

### 2.3 The cosmetic-W trap: minting

Only **mint slots** ever carry records. `allocate_local_ino`
(`mod.rs:912-939`) mints exclusively on `mint_slot[v]` — ONE slot per
volume (the smallest hosted). A huge W without a mint-policy change gives
**zero** real granularity: a fresh V-volume set has exactly V loaded slots,
and "migration granularity 1/W" is a lie — the movable unit is a whole
volume's load, the very W = V pathology this campaign exists to kill. Any
large-W design MUST spread minting across many slots per volume (§5.4).

## 3. Candidate architectures, honestly priced

### 3.A Large derived virtual W (the pre-split pattern) — **CHOSEN**

W derived internally at format from the slot-id wire type:
`W = 2^16 = 65536` (§5.1). Slots park as one arithmetic-progression run per
volume; growth = the existing online migration, forever; minting spreads
across a derived per-volume mint set so load is always divisible.

Price (all changes are encodings and policy, zero mechanism):

| Change | Size |
|---|---|
| Stamp wire v3: `slots_hosted` as **stride runs** `(start u16, stride u16, count u32)` under a new incompat bit (§5.2) | ~200 lines in `checkpoint.rs` + a `SlotSet` type |
| Mint spread M = 64 rotation in `allocate_local_ino` + per-volume mint sets in `RouteTable` (§5.4) | ~80 lines |
| `plan_meta_slot_set` derives W; `--meta-slots` dies (error naming successor); every format is stamped + set-wide hash seed | ~60 lines |
| FormatConfig mirror: `meta_slot_map` (O(W)) → `meta_slot_runs` (O(runs)) | ~30 lines |
| Census economy: probe only cursor-bearing + native slots (§5.6) | ~40 lines |
| Preflight caps: hosted-count cap → encoding-budget caps (§5.3) | ~30 lines |

Idle-cost verdict (measured, §5.9): **O(1) per volume on disk (one run,
42 B fresh / ≤ 676 B with full mint cursors, vs the 3953-B ledger budget);
O(W) RAM = 512 KiB/mount; O(W) open-path passes = ~150 µs aggregate.**
Amortized to irrelevance — requirement met.

### 3.B Prefix-splittable slots (extendible hashing)

No fixed W: a slot = a variable-depth low-bit prefix of `(ino − 2)`; a slot
splits online by extending its prefix. Superficially "no W at all" — but
priced against the actual key encoding it loses decisively:

1. **The local-ino derivation is W-dependent** (`local = (ino−2)/W + 2`).
   Under variable depth there is no stable division — the only split-stable
   key is `local = global ino` (identity). That deletes the entire dense
   keyspace layer: per-volume `next_ino` watermarks, the 2^40 guest
   namespace partition, travelling per-slot cursors, the root pin — all
   rewritten. Every record key on every volume changes: **a reformat-class
   on-disk migration of the whole KV layer**, not an encoding change.
2. **A slot's records stop being a contiguous key range.** Today a guest
   slot is one contiguous ino range (`(s+1)<<40 …`) — bulk copy and
   teardown are range scans/drops (`slot_migration.rs` `SlotKeyspace`).
   Under identity keys a slot is a *strided* subset interleaved with every
   other slot on the volume: bulk copy = full-tree scan + filter, teardown
   = per-record deletes. The migration engine's cost profile regresses
   from O(slot) to O(volume) per slot moved.
3. **Bootstrap stamps become a trie** (per-slot prefix depths), a strictly
   more complex §5.5.1a encoding with new crash-window analysis, for zero
   reachable benefit: 3.A's W = 65536 already exceeds any plausible
   deployment ≥ 100× (§5.1), so B's "unbounded" region is unreachable.
4. The one thing B gets free — within-volume split as a pure map update —
   3.A also gets: taking every second slot of a volume's stride run **is**
   a prefix split expressed in modulo space (stride doubling), §5.5, and
   it keeps the stamp at O(1) runs.

**Counted rationale:** B = rewrite of the keyspace layer + migration
enumeration + stamp trie (≈ the whole VL5b surface re-opened, plus an
on-disk key migration the forward-only law would turn into "reformat
everything"), to lift a ceiling (65536 meta volumes) nothing can reach.
A = ~440 lines of encoding/policy with the migration engine, flip
protocol, cutover gate, tee, and crash windows untouched. **A wins unless
its idle costs fail; they were measured and did not (§5.9).**

## 4. Non-goals

- **No cluster work.** Everything here is local-set machinery; integration
  tests ride local file-backed MetaLV sandboxes per house norm.
- **No online meta membership change.** `add-meta`/`remove-meta` stay
  offline verbs; `migrate-meta-slot` stays the online path. Unchanged.
- **No routing-arithmetic change.** `route_ino_width`/`make_global_ino_width`
  are byte-identical; W = 1 identity short-circuit kept (test surface +
  correct arithmetic coincidence).
- **No journal/node/tree format change.** Guest keyspaces remain ino-
  namespace partitions inside the existing three trees (the VL5b deviation
  note stands).

## 5. The design

### 5.1 Derived width (no knob)

```rust
/// The derived virtual routing width: the FULL slot-id namespace the
/// guest-keyspace wire encoding already reserves (slot ids are u16 in
/// `guest_local_ino`, the membership stamp, and the cutover gate map).
/// Not a knob — derived from the wire type, not chosen by anyone.
pub const DERIVED_ROUTING_WIDTH: u32 = (u16::MAX as u32) + 1; // 65536
```

- **Derivation, stated:** W = the size of the slot-id type's value space.
  The ≥ 100× requirement: a deployment would need > 655 meta volumes for
  W to be < 100× past it; the largest contemplated deployments are two
  orders of magnitude smaller. Growth ceiling = 65536 meta volumes (each
  member must host ≥ 1 slot) — unreachable.
- **Ino-space headroom:** globals reach `(2^40 − 2)·2^16 + 65537 < 2^56`
  — comfortably inside u64; the ≥ 100 M-inode cap holds per volume via the
  2^40 native/guest raw space, per slot via cursors.
- `plan_meta_slot_set(volume_count)` (width parameter **removed** from the
  public planner; a width-explicit `plan_meta_slot_set_with_width` remains
  for the routing-equivalence test surface — the routing code is width-
  parametric and small-W geometry tests remain valid coverage). Fresh
  distribution unchanged in shape: slot `s → member s mod V` — which is
  exactly **one stride run per volume**: `(start = v, stride = V,
  count = ⌈(W − v)/V⌉)`.
- W remains **stored** in every stamp (`routing_width: u32`) and remains
  the authority at mount — a future derivation change must never re-route
  an existing set. Mounts route over the stored W, always.
- `format --meta-slots <n>` **dies loud** (forward-only), naming its
  successor: routing is dynamic, width is derived, growth is
  `volume add-meta --take-slots …` / `migrate-meta-slot`.

### 5.2 Stamp wire v3 — stride runs, under incompat bit 6

New superblock incompat bit (`kv/superblock.rs`):

```rust
/// `features_incompat` bit 6: dynamic meta routing — the volume's
/// membership stamps use the stride-run slot-set encoding over the
/// DERIVED routing width, and minting spreads across the per-volume
/// mint set. Set at format on EVERY volume. A v3 volume WITHOUT this
/// bit was formatted with the frozen user-chosen routing width and is
/// no longer supported: refuse loud, reformat required (forward-only).
pub const FEATURE_INCOMPAT_KV_DYNAMIC_ROUTING: u64 = 1 << 6;
```

- **Presence is required** (the `NODE_SEQ_WATERMARK` precedent,
  `superblock.rs:460-466`): decode refuses bit-6-absent v3 volumes loud
  with reformat guidance. This refuses BOTH legacy identity sets and
  `--meta-slots`-era stamped sets in one message shape, before any ledger
  read. `format --force` remains the remedy (the format preflight degrades
  refused superblock classes to the plain force gate — existing law).
- Old binaries refuse bit-6 volumes via their `FEATURES_INCOMPAT_KNOWN`
  gate (bit 6 intersects no prior mask — pinned test, the VL5a
  non-intersection precedent).
- Fresh formats set bits **0 | 1 | 2 | 4 | 6**: stamps exist from birth
  (bit 2's claim), the wire is always the extended form (bit 4's claim),
  and the encoding is stride-run (bit 6). The lazy ensure-bit sites become
  always-satisfied no-ops and stay (they are the guard for the invariant,
  not dead code); bit 3 (`KV_VOLUME_LIFECYCLE`) keeps its lazy
  first-lifecycle-commit semantics unchanged.
- **Stamp wire (one form — the dense list and the "unextended" variant are
  deleted with the binaries that could read them):**

```
set_uuid(16) | set_epoch(8) | member_position(2) | member_count(2) |
routing_width(4) | n_runs(2) | n_runs × { start(2) | stride(2) | count(4) } |
native_slot_plus1(4) | n_cursors(2) | n_cursors × { slot(2) | next(8) }
```

  (`native_slot_plus1` is u32 on the wire — a u16 `plus1` would overflow
  at slot 65535, which the derived width makes reachable.)

```
```

  `slots_hosted` becomes a `SlotSet` (ordered stride runs; `contains`,
  `len`, `iter`, insert/remove with greedy re-coalescing — mutations expand
  to a W-bitmap and re-coalesce; control-plane only, measured µs). Decode
  refuses overlapping/out-of-range runs, `count = 0`, and cap violations
  loud — same §9 validate-before-trust discipline.

### 5.3 The encoding budget replaces the hosted-slot cap

The 4096-B ledger slot budget, restated for the new wire
(24 hdr + 34 fixed payload + ≤ 5 roots × 17 = 85 ⇒ **3953 B** for the
stamp):

```
STAMP_MAX_RUNS    = 128   // 8 B each = 1024 B
STAMP_MAX_CURSORS = 256   // 10 B each = 2560 B
34 + 1024 + 4 + 2 + 2560 = 3624 ≤ 3953  ✓  (worst-case image re-derived in the consts' docs)
```

- `MEMBERSHIP_MAX_HOSTED_SLOTS` (= 64) and `META_SLOTS_PER_VOLUME_CAP`
  (= 64) **die** — a volume may host 32768 slots as ONE run. What is
  bounded is **encoding complexity**: runs (migration-history
  fragmentation) and cursors (ever-minted slots).
- `encode_slot` enforces the caps loud (load-bearing, as today).
  **Migration/add-meta preflight** refuses any assignment whose
  prospective source/target stamps would exceed the caps, naming the
  remedy (take strided sub-progressions / consolidate). The
  `W ≤ 64 × volumes` format bound **dies** — a fresh identity
  distribution is always 1 run/volume at any V.
- New pressure gauges (§5.8) make cap approach visible before it refuses.

### 5.4 Mint spread — what makes the width real

```rust
/// Per-volume mint-set size: minting rotates across the volume's first
/// MINT_SPREAD hosted slots. Derived from the stamp cursor budget:
/// STAMP_MAX_CURSORS / 4 — a freshly-spread volume consumes ≤ ¼ of its
/// cursor budget, leaving ¾ for cursors that travel in with migrated
/// slots.
pub const MINT_SPREAD: usize = STAMP_MAX_CURSORS / 4; // 64
```

- `RouteTable` gains `mint_slots: Vec<Vec<u16>>` — per volume, the first
  `min(MINT_SPREAD, hosted)` hosted slots ascending, **derived from the
  map on every publish** (no new durable state; cursors are already the
  durable part and already travel at cutover — VL5b machinery verbatim).
- `allocate_local_ino(v)` rotates a per-volume relaxed atomic counter over
  the mint set: the volume's legacy/native slot mints from the watermark
  (unchanged 2^40 overflow check); every other mint slot mints via
  `allocate_guest_ino` (the existing lazy virgin-cursor-at-2 law — sound
  for the same reason it is sound today: a virgin hosted slot's keyspace
  is empty by construction).
- Consequence: a volume's load is divisible into ≥ 64 movable slices from
  birth — granularity 1/64 of a volume, forever, at any V. Root pin
  unchanged (ino 1 → slot 0 → volume 0's native keyspace at format).
- Cursor persistence rides the existing checkpoint stamp path; ≤ 63 extra
  cursors = 630 B per ledger write — noise.
- No loom clause: the rotation counter is a relaxed `fetch_add` feeding a
  modulo (no ordering edge); the route table stays ArcSwap; no lock-free
  core changes.

### 5.5 Growth shapes (the operator story)

- **Format anywhere**: any V ∈ [1, 65536]; nothing to size, no knob.
- **Grow forever**: `volume add-meta --take-slots k` (offline, unchanged
  verb) or online `migrate-meta-slot`, up to 65536 members.
- **The stride-doubling pattern**: V → 2V growth takes every second slot
  of each donor's run (stride V → 2V) — the donor keeps
  `(v, 2V, …)`, the new member gets `(v + V, 2V, …)`: **O(1) runs per
  volume at any power-of-two growth depth** (this is B's prefix split,
  expressed in modulo space, on unchanged machinery). Arbitrary growth
  still encodes in a few runs; scattered hand-picked migrations consume
  the run budget and are refused loud at preflight past it.
- The `--take-slots k` default planner prefers budget-preserving strided
  picks over per-slot scatter where load permits.

### 5.6 Census + mirror economy

- **Census** (`--take-slots k` most-loaded ranking): probe only slots that
  can carry records — the native slot + cursor-bearing slots (≤ 1 + 256
  per volume, vs O(W)); virgin hosted slots are empty **by construction**
  (the same law that makes lazy cursors sound). One `open_probe` per
  member, not per slot.
- **FormatConfig mirror**: `meta_slot_map: Option<Vec<u16>>` (O(W)) is
  **deleted**; `meta_slot_runs: Option<Vec<Vec<(u16, u16, u32)>>>`
  (per member-position, its hosted runs) replaces it. Stamps remain
  authoritative (unchanged law); the mirror stays human-readable and
  O(runs). `meta_routing_width` mirror stays.
- Discovery (`discover_meta_set`): the legacy unstamped arm is
  **unreachable** behind the bit-6 gate and is replaced by a loud
  defense-in-depth refusal (a bit-6 volume with no stamp = torn/foreign);
  the all-stamped path iterates run sets (order-independent bootstrap,
  URI-disagreement refusals, per-slot highest-epoch-wins — all verbatim).
  `repair_meta_set`'s missing-member complement inference coalesces the
  complement to runs and applies the run cap.
- The migration hash-seed law is unchanged in mechanism but universal in
  reach: every format is now a stamped set and mints ONE set-wide
  `hash_seed` (previously only `--meta-slots` formats did) — the
  `assert_matching_hash_seeds` refusal can only trip against foreign
  volumes.

### 5.7 fsck / C-class and VL5b interactions

- No new detection class. C1's ledger validation covers the run-encoded
  stamp via decode's refusals; the zero-FP ladder is untouched. Boundary
  coverage: fsck over a mint-spread volume (64 populated keyspaces) and
  over a post-migration set must report `fsck_findings = 0`.
- Guest-keyspace walking (`try_make_global_ino`) is unchanged — mint
  spread just makes guest keyspaces the common case from birth instead of
  the post-migration case.
- §5.5.2a gate, §5.5.2b flip, delta tee, staging rebind (KD-8), D0 guard
  interactions: byte-identical mechanics; the flip's stamp writes carry
  the new encoding.

### 5.8 Stats (stats inode)

| Field | Meaning |
|---|---|
| `meta_routing_width` | the set's stored W (constant per set — operators key on it like `meta_format_version`) |
| `meta_slot_mint_spread` | the mount's effective per-volume mint-set size |
| `meta_slot_stamp_runs_max` | max hosted-run count across members — the run-budget pressure gauge (approaching `STAMP_MAX_RUNS` ⇒ consolidate before preflight refuses) |
| `meta_slot_stamp_cursors_max` | max cursor count across members — the cursor-budget pressure gauge |

(`meta_slot_migrations`, `meta_slot_gate_parked_commits` unchanged.)

### 5.9 Measured idle costs (the A-adjudication numbers)

Standalone probe (rustc -O, this box, medians of 1000 iterations;
structural mirrors of the exact open-path passes), W = 65536:

| Pass | V=1 | V=2 | V=4 | V=8 |
|---|---|---|---|---|
| format plan expansion (O(W)) | 186 µs | 167 µs | 165 µs | 166 µs |
| discovery claim resolution (O(W)) | 104 µs | 100 µs | 131 µs | 126 µs |
| validate + native derivation (O(W)) | 31 µs | 24 µs | 24 µs | 24 µs |
| `route_ino` (arith + table) | 1.1 ns/op | 1.0 | 1.0 | 1.1 |
| route table RAM | 512 KiB (`Vec<usize>`) / 128 KiB (u16) — per mount | | | |
| fresh stamp bytes (1 run + 63 cursors) | **676 B** vs the 3953-B budget | | | |

Real end-to-end numbers post-implementation (`idle_cost_probe`,
file-backed sandbox, debug build — a conservative upper bound; the
probe stays in `tests/dynamic_meta_routing_tests.rs` as an explicitly
invoked instrument):

| Shape | format | discover | open (guarded, replay) | at-rest stamp | route table |
|---|---|---|---|---|---|
| V=1, W=65536 | 7.0 ms | 7.8 ms | 20.9 ms | **48 B** (fresh; ≤ 674 B with a full mint-cursor set) | 512 KiB RAM |
| V=2, W=65536 | 7.6 ms | 6.7 ms | 27.9 ms | 48 B/member | 512 KiB RAM |

Verdict: per-slot idle cost aggregates to O(1)-per-volume on disk and
sub-ms/sub-MiB per mount — **displaced nothing**; architecture A stands.
The standing regression instrument is the `dynamic_meta_routing`
Criterion group in `benches/high_concurrency_bench.rs`; the campaign
evidence note is `.benchmarks/2026-08-02-dynamic-meta-routing.md`.

## 6. Forward-only surface (the refusal matrix)

| Volume shape | Old binary (≤ bit 5 mask) | This binary |
|---|---|---|
| Fresh dynamic-routing format (bits 0,1,2,4,6; 3/5 lazy) | refuse loud (unknown bit 6) | mounts |
| Legacy identity set (bits 0,1) | mounts | **refuse loud**: "formatted with a frozen routing width (pre-dynamic-routing) — no longer supported; reformat required (`squeezefs format --force`)" |
| `--meta-slots`-era stamped set (bits 0,1,2[,3,4]) | mounts | **refuse loud**, same shape |
| v2 superblock | refuse (v2 gate) | refuse (v2 gate, unchanged) |

No downgrade path; no window (bit 6 is in the format-time superblock —
there is no crash prefix in which a dynamic-routing volume lacks it).
`--meta-slots` on the CLI is a hard error naming its successor.

## 7. Test contracts (red-first battery)

`tests/dynamic_meta_routing_tests.rs` (+ updates to `meta_slot_tests.rs`,
`meta_slot_migration_tests.rs`, `interaction_tests.rs`,
`meta_plane_distribution_tests.rs` helpers):

1. **Derivation**: `plan_meta_slot_set(v).routing_width == 65536` for all
   v; one run per member; fresh stamp `encoded_len` ≤ budget at
   V ∈ {1, 2, 8}; `DERIVED_ROUTING_WIDTH == u16::MAX + 1`.
2. **Routing equivalence under migration**: global-ino round-trip across
   arbitrary `publish_slot_map` flips at W = 65536 (property test);
   logical tree digest invariant across a real slot migration (existing
   G-VL-4 diff tool, re-pointed).
3. **Ino stability**: st_ino of a minted population is invariant across
   arbitrary slot moves + remounts (the KD-7 law at the new W).
4. **Mint spread**: creates on one volume land in `MINT_SPREAD` distinct
   slots; cursors appear in the persisted stamp; remount reseeds cursors
   with no collision (mint-after-remount continues each sequence).
5. **Bootstrap order-independence**: scattered/reversed URI order at
   W = 65536 discovers the identical canonical set (positions, natives,
   map) — the §5.5.1a suite re-run at the derived width.
6. **Refusal shapes**: bit-6-absent v3 volume refuses loud naming
   reformat (superblock crafted by clearing bit 6 + re-checksumming);
   `--meta-slots` CLI errors naming the successor; unstamped bit-6 volume
   refuses as torn/foreign; bit 6 ∉ pre-campaign `FEATURES_INCOMPAT_KNOWN`
   (mask non-intersection pin).
7. **Encoding caps**: stamps at exactly `STAMP_MAX_RUNS`/`STAMP_MAX_CURSORS`
   encode; +1 refuses loud (the `encode_slot` boundary law); preflight
   refuses a cap-violating assignment naming the remedy.
8. **W = 1 identity byte-equivalence** stays pinned
   (`RoutedMetaBackend::new` — the in-RAM/test surface).
9. **Idle-cost bound (structural)**: route-table build + validation at
   W = 65536 under a generous wall bound; stamp size assertions as in 1.
10. **Rig**: `tests/run_volume_lifecycle.sh` legs 9–11 re-pointed at the
    derived width (formats drop `--meta-slots`); a new leg formats ONE
    meta volume, writes a dataset, grows to two volumes by
    `add-meta --take-slots`, asserts byte identity + st_ino stability +
    both volumes carrying live slots — the "format anywhere, grow
    forever" proof shape. `run_lifecycle_soak.sh` / `run_vl9_matrices.sh`
    formats updated the same way.

## 8. Rollout

Single PR (this campaign), no on-disk coexistence to manage (forward-only
refusals carry the transition). Rig + soak evidence, idle-cost numbers,
and refusal-shape transcripts land in
`.benchmarks/2026-08-02-dynamic-meta-routing.md`. AGENTS.md's
`format --meta-slots` references and the VL5 summary rows are updated with
the successor story; `docs/design-volume-lifecycle.md` gains a banner note
at §5.5.1 + a revision row pointing here (history preserved, not
rewritten).

## 9. Second consumer: DLM lock homing (stage S4, landed 2026-08-03)

The slot map now homes **locks** as well as metadata
(`pre-rc-engineering-spec.md` §6.7 decision 2, §6.9 stage S4;
`src/dlm_slot.rs`, contracts `tests/dlm_slot_lock_tests.rs`). Recorded
here because the routing law above is now load-bearing for a second
plane, and any change to it moves both.

* **Homing = `route_ino_width(ino, W).0`, verbatim.** `lock_home_slot`
  parses the lock object's **global** ino out of its `inode_{N}` key form
  (through `dlm::ino_of_path`, the one place that law lives) and routes it
  over the set's frozen `W`. No hash ring exists anywhere in the lock
  path: the whole point is that the lock master and the metadata
  authority are the SAME process by construction, which is what lets a
  metadata RPC and its lock be one round trip when S8 ships function
  shipping. A test pins `slot_of_ino ≡ route_ino_width(..).0` across
  widths {0, 1, 2, 3, 64, 65536, 131072} and the ino-1/ino-2/`ino + k·W`
  edges — divergence between the two planes is a red test, not a
  debugging session.
* **`W ≤ 1` is the identity arm here too** — every object homes to slot 0,
  which is exactly the in-RAM/test constructor's and the legacy
  single-volume shape (§5.1, contract 8 above).
* **A non-inode lock object pins to slot 0.** It has no ino and therefore
  no routed home; slot 0 is the root ino's slot, whose owner is a member
  of every set by construction. Not a scaling concern (no product verb
  locks a non-inode object), but it IS the answer S6/S8 inherit.
* **The width reaches the lock plane by publication, not by plumbing** —
  `RoutedMetaBackend::new` / `with_slot_map_and_natives` call
  `dlm_slot::publish_routing_width` when a routed set is opened, so
  mounts and offline verbs agree and `DlmClient::new()` (which has no
  backend handle) needs no new argument. One process serves one set, so
  one word is honest; **a process that can ever serve two sets at once
  must bind the width to the set handle instead** — the S6/S8 contract,
  also stated on the static.
* **Ownership is a separate, lock-free question** (`is_local_slot`): solo
  mode has no owner table installed and answers "local" for every slot,
  so acquisition is bit-for-bit the S0–S2 `scc` probe and `dlm_rpcs` is
  0 by construction. A remote owner map is later the same call over an
  installed per-slot bitset — a lookup, not a re-plumbing.
* **Slot migration and lock homing.** A slot's move re-homes its locks
  along with its metadata, which is correct and free today (one owner) and
  is precisely why the per-slot cutover gate (§5.5.2b, checked *before* 4a
  acquisition) is the right remastering primitive later: the gate already
  parks operations while a slot changes hands. Nothing about migration
  changes at S4 because there is only ever one owner to hand to.

## 10. Revision history

| Rev | Date | Change |
|---|---|---|
| 3 | 2026-08-03 | §9 records the slot map's second consumer: DLM stage S4 homes lock objects on `route_ino_width` (solo mode — `dlm_rpcs == 0` by construction), publishes the frozen width to the lock plane at routed-set open, and pins the two planes' routing equivalence as a test |
| 2 | 2026-08-02 | Implementation record: `native_slot_plus1` widened to u32 on the wire (u16 plus1 overflows at slot 65535); §5.9 gains the measured end-to-end probe numbers (7–8 ms format / 21–28 ms open / 48 B at-rest stamps on the file-backed sandbox); the idle-cost probe and the `dynamic_meta_routing` Criterion group named as standing instruments |
| 1 | 2026-08-02 | Initial design: constraints verified against code (ino-pin arithmetic, ledger/xattr/census cost sites, the mint-slot cosmetic-W trap); A vs B priced with B's key-encoding rewrite named as the disqualifier; A adjudicated on measured idle costs (µs/KiB-scale at W = 65536); derived W = 2^16 (slot-id namespace), stride-run stamp wire under incompat bit 6, encoding-budget caps replacing the 64-slot cap, MINT_SPREAD = 64 minting rotation, census + mirror economy, forward-only refusal matrix |
