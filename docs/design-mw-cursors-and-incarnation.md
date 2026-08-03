# Per-writer ino cursors and `offset ‖ incarnation` block keys

Pre-RC engineering spec §6.2 **items 5 and 6**, execution-plan rulings
**D8** (N coherent writers + readers, including concurrent writers to
different regions of one large file) and **D9** (the format work joins the
batched reformat window — *build the bit, do not stamp it*).

Landed behind incompat bits **12** (`KV_INO_LANES`) and **13**
(`KV_BLOCK_KEY_INCARNATION`), neither of which anything stamps. Contracts:
`tests/mw_ino_lane_tests.rs`, `tests/mw_block_key_incarnation_tests.rs`.
Siblings this composes with: `docs/design-durable-block-refcounts.md`
(item 1, bit 9) and `.benchmarks/2026-08-05-mw-partitioned-append.md`
(items 2/3/4, bit 8).

**Bits 12/13, not 10/11.** Bit **10** is writer-scoped staging keys (§6.2
item 8) and bit **11** the S7 data-plane fence; both were authored on
branches parallel to this one, so this wave moved up rather than alias
them — the third occurrence of the bit-8 collision class, which is silent
on-disk ALIASING rather than a merge inconvenience. The reservation is
pinned as a mask (not as constants this branch cannot see) in
`the_multi_writer_format_bits_are_disjoint_and_unstamped`.

**No numbers in this document — ruling D11.** *"No cargo test, benches or
release gate yet until we are done implementing the DLM and can have N
Readers and Writers."* Every cost statement below is therefore a
**prediction with a falsification criterion**, and the bench coverage
(`benches/meta_lv_bench.rs::kv_ino_cursors`,
`benches/write_path_bench.rs::write_block_key`) exists un-run. There is no
evidence note for this branch and none may be cited until the D11 window
opens; §5 is the list of claims the first measured pass has to refute.

**Scope discipline, stated first.** This lands **formats**, not
arbitration. Who may mint an ino, who may allocate a block, and the
partitioning that keeps writers disjoint are §6.9 **S4/S8**. The line this
work had to reach is: *the on-disk names no longer assume exactly one
writer, and a violation is either structurally impossible or refused loud —
never silent.*

---

## 1. The two assumptions, in source

| # | Assumption | Anchor | Failure under a second writer |
|---|---|---|---|
| 5 | `next_ino` is a per-mount atomic over a shared namespace | `kv/backend.rs` (`next_ino`, `allocate_ino`) | Two writers mint the **same ino**. Two files alias immediately; worse, the daemon's IPC binding table rests on the monotonic never-reused ino law (spec §8), so the alias reaches **fd bindings** |
| 6 | Block keys are bare, reusable device offsets | `routing.rs` (`persist_block_key` / `parse_block_key`) | A freed-and-reissued offset makes a stale map binding **serve another file's bytes with no error and no counter** (§6.3) |

§6.3 states item 6's hazard precisely, and it is worth restating because
the design follows from it. The read path's serve proof is: *bytes for key
K serve for block b iff (a) the fetch was incarnation-valid and (b) the
current map still binds b → K.* **Both premises are process-local.** (a)
reads a per-process seqlock that returns `UNKNOWN_STABLE` for any offset
this node did not itself allocate (`src/incarnation_core.rs`); (b) is
deliberately consulted *without* a TTL gate, on the argument that local
merges always republish before freeing. So when node A overwrites a block
(CoW), frees the offset, and the allocator reissues it to a different file,
node B — whose cached map still binds b → K and whose incarnation word is
untouched — serves the other file's bytes. On a passthrough volume (the
default) that is **silent**; on a transformed volume the AEAD tag fails,
which is the one honest degradation.

---

## 2. Item 5 — per-writer ino lanes

### 2.1 The law: a residue class, not a range hand-out

With `writers = W`, a local ino `i` belongs to **lane** `(i − 2) % W`, and
appender `w` mints only lane-`w` locals: `2 + w`, `2 + w + W`, … The cursor
is an ordinary monotone counter with **stride W** instead of 1
(`src/lane_core.rs`).

The alternative — each writer leasing chunks `[start, start + N)` from a
durable distribution watermark — was rejected for one reason: it needs a
**hand-out protocol**, i.e. arbitration, which this work is explicitly not
allowed to build (and which S4 owns). A residue class needs nothing but the
appender's own id:

* **disjointness** — two lanes cannot produce the same value, so "never
  reused" survives N concurrent appenders *without any agreement between
  them*;
* **attribution** — `ino_lane_of(local, W)` names the appender that minted
  any ino, so recovery and fsck can classify a foreign writer's inos
  instead of guessing;
* **defence in depth** — it stays correct even when the arbitration above
  it is wrong. That matters concretely: slots migrate **online**
  (`migrate-meta-slot`), and a membership disagreement — two writers each
  believing they host slot `s` — is exactly the state where duplicate inos
  would appear. Lanes make that survivable rather than trusting S4 to be
  perfect.

The sibling reached the same conclusion for the extent bitmap (`page %
writers`, *interleaved, not contiguous*) for the analogous reason: an
existing volume's allocation is dense at the low end, and interleaving
spreads it instead of handing writer 0 everything.

**Solo (`W = 1`) is lane 0 = every ino, stride 1** — today's dense
`fetch_add(1)`, arithmetically unchanged. That equivalence is a **tie
test**, not a comment: `solo_lane_cursor_ties_the_shipped_cursor` asserts a
`LaneCursor` at `AppendPartition::SOLO` produces the same sequence as the
VL5b `SlotCursor` and as a dense counter, from five different floors. If
the lane arithmetic ever drifts from the shipped counter, that test is red.

### 2.2 Both ino spaces are laned

A create mints a **local** ino in one of two spaces and then encodes it
globally through the frozen routing width:

* the volume's **native** watermark space (`next_ino`) — used when the
  picked mint slot is the volume's legacy keyspace;
* a hosted slot's **guest** space (VL5b `slot_cursors`) — the other ~63
  slots of the `MINT_SPREAD` rotor, i.e. ~63 of every 64 real creates.

Both are laned, for the migration reason above. The mount holds one lane
cursor per `(writer id, space)` it actually mints in
(`KvMetaBackend::lane_cursors`), created on first use; on every mount today
that map is **empty** and the shipped paths (`allocate_ino`,
`allocate_guest_ino`) are untouched.

### 2.3 The three invariants that had to survive

**Monotonic, never-reused inos.** Recovery keeps §4.8's rule
(`max(ledger watermark, replayed + 1)`) and *rounds the result up into the
lane* (`recover_ino_floor` → `next_in_lane_at_or_above`). The rounded-over
values are **burned** — the same law §4.8 already states ("a failed create
burns the ino; crash-skipped ranges waste nothing that matters"), now
bounded by `W` per crash instead of 1. Rounding is idempotent, so
re-seeding never advances a cursor, and `install_floor` is a `fetch_max`,
so a stale floor can never regress a fresher mint.

**Global-ino stability.** Laned locals are *sparse*, and sparseness is the
only change: `route_ino_width` / `make_global_ino_width` are untouched, the
frozen `routing_width` is untouched, and ino 1 stays the root pin.
`laned_locals_keep_global_ino_stability` round-trips every lane's locals
across four slots (including 65,535) and asserts the encoding stays
injective.

**`statfs`'s live count (POSIX-1).** This is where the design drew blood: a
lane cursor is seeded from the space's recovered **dense** watermark, which
already counts every value below it in *every* lane. Counting the lane's
progression as `cursor − 2` therefore charges the seeding gap as mints and
over-reports `IUsed` — the same class of error, one level down, that made
`df -i` misreport by ~64× before the `MINT_SPREAD` fix. The exact count is
`(cursor − seed) / writers`, which is why `LaneCursor` carries its seed and
**re-bases it on `install_floor`** (recovery is not minting). The contract
`live_inode_count_is_exact_under_lanes` caught this as a red test, and the
loom model `lane_install_floor_never_regresses_or_inflates` catches it as a
concurrency property.

### 2.4 Durability: no new durable field

The per-writer watermark is **each appender's own root-ledger record** —
`LedgerRecord.next_ino`, in the appender's own slot range, which incompat
bit 8's partitioned ledger already provides. Two consequences:

* item 5 adds **nothing** to any on-disk structure. Its only durable
  footprint is that the ino *values* in keys are strided;
* `KvMetaBackend::next_ino()` now returns a watermark that **dominates
  every lane** (the dense atomic folded with each native lane cursor). A
  successor recovering from a dominating watermark rounds up into its own
  lane, so it can never re-mint — pinned by
  `appenders_on_one_volume_mint_disjoint_inos`. Solo mounts pay one relaxed
  latch load and return the atomic verbatim.

The crash contract is pinned end to end by
`a_crash_and_remount_never_re_mints_a_lane_ino`: mint in a lane, commit
records that *mention* those inos, drop the backend **without shutdown** (no
final checkpoint), remount, and the replay-fold watermark must round up
strictly above every committed ino.

### 2.5 Why it still needs an incompat bit

Nothing about a laned volume's *layout* is new, but its ino-space
**semantics** are: a lane-unaware writer mints DENSE inos across every
lane, and a peer resuming from its own durable watermark would then re-mint
an ino that writer already used. So a mount that does not understand lanes
must not write to a laned volume — which is exactly what an incompat bit
says. Correspondingly, `allocate_ino_in(non-solo)` on a volume **without**
bit 12 is refused loud, naming the bit.

---

## 3. Item 6 — `offset ‖ incarnation` block keys

### 3.1 The wire form

```text
[be://]offset@<base36 incarnation>[:rel_off:packed_len]
   e.g.  4194304@3f2a          vol-00aa11bb://4194304@3f2a:0:4194304
```

Four properties, each deliberate:

* **the suffix is on the OFFSET component**, so it survives
  `clean_block_key` — the lifetime is part of the key's *identity*, which is
  what makes the map-binding check and all five block-key cache stores
  compare lifetimes instead of offsets;
* **`@`, not `:`** — `:` is already the per-block decoration separator
  (`bk:rel:len`). A distinct separator means the decoration still parses
  after the lifetime, `block_mapping_form` still classifies
  `undecorated-2part`, and — critically — the W1 predicate
  `is_whole_block_mapping` (`!rest.contains(':')`) is untouched, so a
  stamped whole-block mapping stays W1-eligible. Predicate-polarity rot here
  is the Issue-19 class the RW1 program pinned permanently;
* **canonical lowercase base-36** — one lifetime has exactly one key string
  (two spellings would break the binding equality the whole design rests
  on), and the composed 64-bit stamp renders in ≤ 13 characters instead of
  ~20 decimal digits. Key bytes are paid **per map entry in every layout
  publish**, i.e. against the journal-byte term the write-commit-economy
  campaign collapsed, so the encoding is not cosmetic. A non-canonical
  suffix (leading zero, empty, non-digit, overflowing) is **refused**, never
  rounded to a lifetime;
* **`incarnation == 0` is the absent form**, which is byte-for-byte today's
  bare key. An un-stamped volume's keys are unchanged, and a legacy key
  parses as "names no lifetime".

`parse_block_key` is **not forked**: it and `parse_block_key_parts` run one
shared extraction core (`BackendRouter::split_key`, which returns
`(&be_id, offset, incarnation)` borrowed), and the historical entry point
simply drops the lifetime — so every existing consumer (free, refcount, the
durable block-reference resolution, fsck, the movers, the reclaim queue)
keeps resolving the same `(backend, offset)`. The durable-refcount work's
`block_ref_for` therefore continues to key on
`(vol_tag, block_idx, owner_ino, block_index)` — a reference is to a
**block**, not to a lifetime — and needs no change.

Hot-path discipline: the bare form parses through the same `u64::parse` it
always did; the `@` split runs only when that parse *fails*. An un-stamped
volume pays nothing (predicted; see §5).

### 3.1a Composition with writer-scoped staging keys (§6.2 item 8)

Item 8 (bit 10, a parallel branch) scopes the **staging metadata key
names**; item 6 stamps the **block-key values** the layout persists. They
live in different string namespaces and must stay that way. The composed
grammar, stated so both parsers can be checked against one text:

```text
block-key VALUES (layout maps, block_map: records, cache keys)
    [damaged:] [be_id://] offset [@<base36 incarnation>] [:rel_off:packed_len]
        components after `://` are ':'-separated: 1 = whole-block (W1
        eligible), 3 = decorated; the lifetime is INSIDE component 1

staging/mapping META KEY NAMES (item 8's surface)
    active_block:inode_{ino}:block_{n}[:w_{16 hex}]
    active_block_ext:inode_{ino}:block_{n}[:w_{16 hex}]
    mapping:{file_id}[:w_{16 hex}]
```

Two invariants the composition rests on, both checkable by reading the two
parsers side by side:

1. **the writer scope never rides a block-key value.** `block_mapping_form`
   and `is_whole_block_mapping` classify a block key by counting the
   ':'-separated components after `://` (1 or 3). A trailing `:w_…` on a
   block key would make a whole-block mapping read as `unknown` and turn
   the W1 predicate false — the Issue-19 predicate-rot class. Scope belongs
   to the key NAME, which is why item 8 put it there;
2. **the lifetime never rides a meta key name.** `@` appears in no key
   family in the tree (verified: the only `@` in formatted strings is
   `zcrx_lane`'s log text), and the staging key parsers split on ':' with
   fixed indices, so an `@` inside a component would silently become part
   of an ino/block token. Lifetimes belong to block-key values, which is
   where `persist_block_key` puts them.

Concretely, the two decorations are simultaneously present only in a record
whose *name* is scoped and whose *value* is stamped, e.g. meta key
`active_block:inode_7:block_3:w_00000000000000a1` holding the value
`vol-00aa11bb://4194304@3f2a:0:4194304`. Nothing parses those two together,
and nothing should learn to.

### 3.2 Where a lifetime comes from, and why it survives a remount

```text
incarnation = (writer_term << 40) | lane_seq
```

This is the **same composition the DLM's S2 fencing token uses** (spec §6.7
decision 4: `token = (term << 40) | grant_seq`), deliberately, because it is
the same problem: make a per-mount counter unrepeatable across remounts by
riding the one durable, barriered-before-arm era word the volume already
carries. `WRITER_TERM_XATTR` (incompat bit 7) is bumped past every
predecessor's at claim acquisition and barriered before the guard arms, so:

* **across mounts** — every stamp a successor mints dominates every stamp
  its predecessor could have written. No new durable record, no lease
  protocol, no extra commit, nothing on the hot path;
* **across appenders** — `lane_seq` is a lane cursor (item 5's core, reused
  verbatim), so two appenders of one mount-era cannot mint the same stamp.

Bit 11 therefore **requires bit 7** and `set_block_key_incarnation_bit`
refuses loud without it: with no durable era the stamps would restart at
every mount, and a stale key would then *match* the offset's new lifetime.
A detection that lies is worse than no detection, because the read path
would serve on the match. (Fresh formats carry bit 7, so this composes
naturally; the refusal exists for pre-S2 volumes.)

**Sequence exhaustion** (2^40 allocations in one mount — 4 EiB of fresh
blocks at the shipped 4 MiB block) degrades to *unstamped*, loudly and
counted (`block_key_incarnation_exhausted`), never to a wrapped stamp:
losing detection is recoverable by a remount, which advances the era, while
a wrapped stamp would alias a live lifetime — the exact failure the
structure exists to prevent.

### 3.3 One minting site; everyone else reads

`claim_block_idx` — the funnel *every* allocation path runs through
(free-list, fresh tail, `allocate_block_below`, `allocate_block_at_or_above`)
— mints the lifetime, because **an allocation is the only event that starts
a new lifetime of an offset**. Every other site *reads* the live stamp:
`persist_block_key` composes a key from it, so fsck's reconciliation and the
movers' census reproduce an offset's current key instead of inventing a new
lifetime for a block they did not allocate. A second minting site would let
a live map disagree with the live stamp and turn honest reads into refusals.

The live stamp lives beside the loom-verified seqlock word in one
`IncarnationCell` (`{ word, stamp }`) — a second word, never packed *into*
the seqlock, for the same reason PR VL6a kept the fsck allocation epoch in a
separate side map: the seqlock's bit layout is load-bearing and
model-checked. Cost: +8 B on `block_allocator`'s already-recorded RES-13
ceiling (one entry per distinct offset ever allocated, bounded by volume
geometry, no removal path by design).

**Seeding.** Recovery's key walk (`owned_offset`) already parses every
persisted key, so it seeds the offset's lifetime for free — which is what
lets a block laid down by a *previous* mount present its real era instead of
"unknown". This is a forensics/observability win, not a correctness
dependency: the dangerous case is a **reallocation**, and every reallocation
mints through `claim_block_idx` in this process. Seeding never overwrites a
minted stamp (`compare_exchange` from `NONE` only) — a walked key must not
be able to talk the mount out of the lifetime it just minted.

### 3.4 Validation: three verdicts

`BackendRouter::incarnation_ok` runs on both device read legs
(`read_block_with_dest`, `read_block_range`) and on `free_block`:

| Case | Verdict | Why |
|---|---|---|
| the key names **no** lifetime (every field key today) | serve | pre-item-6 behavior, verbatim |
| the offset has **no recorded** live lifetime | serve, count `block_key_incarnation_unknown` | §6.3's honest degradation: an offset this node never allocated has no local answer, and inventing one would be a lie. This counter is the **size of the gap** a shared authority (S9) must close |
| they **disagree** | **refuse**, count `block_key_incarnation_refusals` (must stay 0), log loud | the case that is silent today: the offset was freed and reissued, and the binding would serve — or free — another file's block |

The **free** leg matters as much as the read leg: it is the destructive
face. A free under a dead lifetime would release an offset the allocator has
already reissued *and* queue a discard over the new owner's bytes. Refusing
is the leak-safe direction, exactly like the existing untracked-free
refusal.

Wired but deliberately **not** extended to the W1 in-place patch predicate:
a patch's safety already rests on a process-local refcount (§6.3's second
paragraph), so an incarnation check there would refuse nothing a local
mount can get wrong and would not fix what a remote mount gets wrong. That
is S9's shared-custody problem, listed in §7.

### 3.5 Interaction with the reclaim queue

"Offsets stay non-reallocatable until reclaimed" is a **device-level**
invariant about shared hardware (a queued `BLKDISCARD` must not land on a
new owner's bytes), not a naming invariant, so naming lifetimes neither
weakens nor replaces it. Concretely, and pinned by
`the_reclaim_window_invariant_survives_lifetimes`:

* the `begin_free → reclaim → finish_free` window is unchanged: an offset in
  the window is never handed out, so a new lifetime cannot even exist yet;
* the reclaim queue, the elided-discard debt ledger and `claim_cancels_debt`
  all key on the **offset**, which the stamped key resolves to identically;
* the displacement purge still purges the **old** key string it read from
  the map, which is the string whose cache entries exist;
* what the lifetime *adds* is that the reclaimed lifetime's key stops
  validating the moment the offset is reissued — so a straggler that escaped
  the purge is refused rather than served.

### 3.6 The rejected alternative worth recording

**"Stamp only reused offsets"** — mint a lifetime only when an offset comes
off the free list, leaving the (vastly more common) fresh-tail allocations
byte-identical. Rejected: after a crash the recovered allocation frontier is
derived from *live references*, so an offset that was allocated and published
but whose layout commit was lost sits **above** the recovered watermark and
is handed out again as "fresh" — i.e. unstamped — while a pre-crash key for
its previous lifetime is also unstamped. The two aliases would be
indistinguishable, precisely in the crash window the design has to cover. So
when engaged, **every** allocation is stamped; the cost is paid in key bytes
and is measured rather than assumed.

---

## 4. Compatibility matrix (ruling D9)

`FEATURE_INCOMPAT_KV_INO_LANES = 1 << 12`,
`FEATURE_INCOMPAT_KV_BLOCK_KEY_INCARNATION = 1 << 13`; presence **OPTIONAL**
(the bit-7/8/9 pattern, not bit 6's presence-required one).

| Volume | This binary | An older binary |
|---|---|---|
| **fresh format** (neither bit — `SuperblockV3::plan` sets neither, ruling D9) | dense inos, bare keys: pre-item behavior, byte for byte | mounts exactly as before |
| **un-stamped** (formatted before the bits existed — same on-disk shape) | as above; **sector 0 untouched by mount, and by a publish** | mounts exactly as before |
| **bit 12 stamped** (Phase 8, `set_ino_lanes_bit`) | solo mounts mint lane-0 inos (a subset of the dense space); a non-solo appender mints its own lane | refuses loud (the bit intersects no prior mask) |
| **bit 13 stamped** (Phase 8, `set_block_key_incarnation_bit`; **requires bit 7**) | keys name lifetimes; stale bindings are refused and counted | refuses loud |
| **bit 13 without bit 7** | the stamp is refused at stamping time, and a term-less mount never engages | — |
| **read-only / probe mount** | `writer_term() == 0` ⇒ never engages, never stamps a key | unchanged |

Engagement is per-**set**, not per-volume: `DataRouter::set_meta_backend`
engages incarnation keys only when EVERY mounted meta volume carries bit 13,
and the era is the **max** of their durable terms. A mixed set would have one
volume's layouts naming lifetimes while another's do not, and the era must
dominate every volume's predecessor.

**Nothing stamps either bit today** — not mount, not `plan`. Pinned by
`the_multi_writer_format_bits_are_disjoint_and_unstamped` (which also pins
the four multi-writer bits as pairwise disjoint and all understood: a
sibling wave had two agents independently claim bit 8, which is silent
on-disk aliasing, not a merge inconvenience),
`an_unstamped_volume_mints_dense_and_refuses_a_lane` and
`an_unstamped_volume_is_unchanged_by_mount_and_a_publish` (sector-0 byte
comparison across a real mount **and** a real publish).

The same wave fixed a bit-ledger casualty already in the tree:
`kv_backend_tests::v3_unknown_incompat_bit_refuses_naming_it_and_unknown_ro_does_not`
asserted on `1 << 9`, which the durable-block-refcount landing then claimed
— so the "unknown" bit was known and the case failed on dev. It now
**derives** the lowest unknown bit from `FEATURES_INCOMPAT_KNOWN`, so the
next agent to take a bit cannot re-break it.

---

## 5. Cost — predicted, NOT measured (ruling D11)

There is no evidence note and no bracket for this branch: ruling D11 bars
`cargo bench` in every form (measured or `-- --test`), brackets and
baselines until the DLM admits N readers and writers. What exists is the
bench COVERAGE — `benches/meta_lv_bench.rs::kv_ino_cursors` (solo
`mint_native` / `mint_guest` / `live_inodes_64_cursors`) and
`benches/write_path_bench.rs::write_block_key` (bare *and* stamped mint,
parse, validate, clean rows) — each carrying its field-derived input shape
and the prediction below in-file.

The claims the first post-D11 pass must refute, and what refutation means:

| Claim (all on the **solo / un-stamped** path — ruling D9 makes it the only shipped one) | Structural reason | Falsified by |
|---|---|---|
| ino minting is unchanged | `allocate_ino` is the same `fetch_add`; lane cursors are created on first use and no mount creates one | `mint_native` / `mint_guest` past their group threshold |
| `next_ino()` / `live_inodes()` add one relaxed load | the `lanes_live` latch is read before any `scc` walk, and is `false` forever on an un-stamped volume | `live_inodes_64_cursors` past threshold, or scaling with lanes that do not exist |
| key minting is unchanged | `persist_block_key` gates on one relaxed load of a router-owned word and then runs the pre-item-6 body verbatim | `persist_bare_*` past threshold ⇒ hoist the gate to the publish batch, never drop the lifetime |
| key parsing is unchanged | the bare `u64::parse` succeeds first; the `@` split is reached only when it fails | `parse_bare_*` past threshold |
| the stamped forms cost a CONSTANT more, not a length-scaled amount | one `scc` read, one ≤ 13-char base-36 render, one `split_once` | a `*_stamped*` row growing with key length ⇒ the codec allocates or re-scans per digit |
| the stamped key is ≤ 14 B/entry longer | base-36 of a 64-bit stamp is ≤ 13 chars plus the `@` | the structural byte line the bench prints (deterministic, so it is reportable under D11) |

Two cost facts are *structural* rather than measured and hold regardless:
`IncarnationCell` grows by 8 B per distinct offset ever allocated (the
already-recorded RES-13 ceiling), and a stamped key adds ≤ 14 B per layout
map entry — paid against the journal-byte term the write-commit-economy
campaign collapsed.

---

## 6. Observability

| Counter (stats inode) | Meaning |
|---|---|
| `block_key_incarnation_refusals` | **must stay 0** — reads/frees refused because the key named a dead lifetime of its offset. On a single-writer mount every republish precedes its free, so a live map can never name a dead lifetime; growth means a binding outlived its block |
| `block_key_incarnation_unknown` | keys served whose lifetime could not be checked (§6.3's honest degradation) — the size of the gap S9 must close; structurally 0 on volumes without bit 13 |
| `block_key_incarnation_exhausted` | **must stay 0** — the per-mount lifetime sequence ran out and keys degraded to unstamped |

Item 5 adds no counter by design: the lane machinery is either inert (every
mount today) or its engagement is visible in the ino values themselves —
`ino_lane_of` attributes any ino to its minter, and `next_ino()` /
`live_inodes()` already gauge the cursors.

---

## 7. What S9 still owes on top of this

1. **The shared lifetime authority.** The incarnation makes a stale binding
   *detectable*; on a foreign offset it still reads `unknown` and serves,
   because the truth about "which lifetime does the SET believe this offset
   is in" is not on this node. That is the same shape §6.3's freed-offset
   grace period (spec §6.8 item 3) needs, and the durable block-reference
   ledger (item 1) is what makes "is this block still referenced?" answerable
   without a walk. Until then, `block_key_incarnation_unknown` is the honest
   size of the gap.
2. **W1 and in-place overwrite.** `begin_patch_sole_owner` reads a
   process-local refcount map; the incarnation check is not wired there
   because a local mount cannot get it wrong and a remote one is not fixed by
   it. §6.7 already names the required change (a seventh
   `patch_ineligible_range_shared` clause under S11).
3. **Node-cache revalidation (spec §6.8 item 2)** and **mount-level
   partitioned open**. Both items' recovery folds are pure functions today;
   wiring a mount to open *as appender w of W* — partitioned ledger read,
   merged replay window, per-lane seeding — is S4's admission work. Nothing
   here assumes it. (Node-cache revalidation was landing on a parallel
   branch as this one closed; it neither reads nor writes lane cursors or
   lifetime stamps, so the two meet only at the rebase.)
4. **Per-writer claim terms (item 7).** The incarnation era is the mount's
   max durable writer term, and the lane distinguishes appenders within one
   era. A claim-**set** record with per-writer terms (item 7) would let two
   concurrently-mounted writers carry distinct eras, which is strictly
   stronger than the lane alone.
5. **The remaining §6.2 assumptions (9, 10).** Item 6 stamps the keys the
   *layout* persists; the layout-delta base token is still process-local.
   Writer-scoped staging keys (item 8 — the `active_block:` /
   `active_block_ext:` / `mapping:` names) were authored on a parallel
   branch and claim bit 10; §3.1a states the grammar the two compose into
   and the two invariants that keep them from colliding. Neither item reads
   the other's decoration, so the composition is a rebase-time review of
   those two parsers, not a redesign.
