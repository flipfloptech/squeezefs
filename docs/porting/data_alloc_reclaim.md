# PORTING SPEC — data block allocator, durable refs, reclaim + discard elision, ENOSPC

Reference: `/Users/joberdorf/Source/squeezefs` — `src/block_allocator.rs` (3799 lines, cited
`BA:L<n>`), `src/incarnation_core.rs` (83, `IC:L`), `src/refcount_core.rs` (`RC:L`),
`src/patch_clone_core.rs` (`PC:L`), `loom-models/src/lib.rs` (`LOOM:L`), `src/block_reclaim.rs`
(1898, `BR:L`), `src/meta_backend/kv/block_refs.rs` (765, `REFS:L`), `src/routing.rs` (23961,
`RT:L`), `src/fuse_client.rs` (`FC:L`), `src/free_grace.rs` (`FG:L`), `src/data_custody.rs`
(`DC:L`), `src/block_grant.rs` (`BG:L`), `src/meta_backend/kv/alloc_lease.rs` (`AL:L`),
`src/fsck.rs` (`FSCK:L`), `src/env_knobs.rs` (`ENV:L`), `src/assembly_tasks.rs` (`AT:L`),
`docs/design-durable-block-refcounts.md` (`DREF:L`), `docs/design-device-overlay.md` (`DOV:L`),
`.benchmarks/*` (`BM-<date>`). Charter cites are `charter.md:L<n>` in the port.

Port state this spec builds on: the block-ref key `06 ‖ vol_tag ‖ block_idx ‖ owner_ino ‖
block_index` with the 17 B refcount prefix (`src/meta/key.zig:160-170, 323-368, 435-453`), the
4 B value `{version=1, flags(bit0 map blob, bit1 SHARED), reserved=0}` (`src/meta/record.zig:
320-365`), block-map POINT/STRING/RUN/POINT2/RUN2 values (`record.zig:370-521`), `volumeTag`
(`key.zig:455ff`), and the device face `Device.{writeBlock, readBlock, probeReadBlock,
readBlockOutcome, flush, overdueCensus, setFenceSignal, fenced, closeAtDismount, dismounted,
writeCache}` (`src/io/nvme_dev.zig:1012-1470`) with its explicit "discard/trim is NOT here —
it lives in the reclaim module" (`nvme_dev.zig:19-21`). Rung 4 (charter.md:308) lands the
allocator + durable refs + reclaim + elision; rung 6 (charter.md:311) lands the allocation
lease, the data bitmap and ranged grants — §11 names the seams this module must leave open.

Everything string-shaped in the reference (block keys `oss1://123@xyz:rel:len`, `clean_block_key`,
`parse_block_key_parts`, `"backend_0"`/`"squeezefs"` aliases, `file_type == "striped"`) is NOT
carried: the port's block map is binary POINT/RUN with `vol_tag` + `offset` (+ stamp), so every
"resolve a key string to `(allocator, offset)`" step below becomes a table lookup by `vol_tag`.

---

## 1. The allocator — data structures and API

### 1.1 State (one `BlockAllocator` per DATA volume, `BA:L197-378`)

| Field | Type (reference) | Role | Lines |
|---|---|---|---|
| `chunk_size` | `u64` = `CHUNK_SIZE` 4 MiB | physical stride: offsets are minted as `block_idx × CHUNK_SIZE`; an image longer than it tramples the next chunk (FIND-RW4-A) | BA:L106-141 |
| `free_blocks` | `ReachableFreeSet { set: DashSet<u64>, reachable: AtomicU64, trim_windowed: DashSet<u64> }` | the free list by block idx + a REACHABLE population maintained inside insert/remove (free-listed OR inside an open trim claim window) | BA:L434-530 |
| `highest_block` | `AtomicU64` | the dense fresh-mint cursor; never shrinks | BA:L201 |
| `capacity_blocks` | `AtomicU64` | device capacity in whole chunks; 0 = unbounded (offline tools/tests) | BA:L202-209 |
| `refcounts` | `scc::HashMap<u64 offset, AtomicU32>` | RAM refcount per tracked offset; absent = untracked | BA:L210 |
| `shared` | `scc::HashMap<u64, AtomicU32>` | the SHARED mark (clone protocol, rung 6); empty on every unarmed mount | BA:L211-219 |
| `fsck_scan_epoch`, `fsck_scan_active`, `fsck_epoch_map` | `AtomicU64`, `AtomicBool`, `scc::HashMap<u64,u64>` | the fsck allocation-epoch side map: while a scan is latched, every mint records `offset → epoch`; a SEPARATE map, never packed into the incarnation word | BA:L220-233 |
| `inflight` | `scc::HashMap<u64, u32>` | in-flight allocation registry — live owners of allocated-but-unpublished offsets (RAII `InflightAllocGuard`) | BA:L234-244, L3736-3761 |
| `space_valve`, `space_pending` | `OnceLock<SpaceValve>`, `OnceLock<SpacePending>` | the ENOSPC pressure valve (async drain of the reclaim queue) + "reclaim still owed" predicate, wired by the router | BA:L245-260 |
| `elided_debt`, `elided_debt_bytes` | `scc::HashMap<u64 offset, u64 bytes>`, `AtomicU64` | discard-elision debt: RAM-only, strictly ⊆ the free list | BA:L262-272 |
| `trim_claimed`, `trim_returned` | `AtomicU64`, `Notify` | offsets inside a trim CLAIM WINDOW + the return edge a parked allocation waits on | BA:L273-283 |
| `incarnations` | `scc::HashMap<u64 offset, IncarnationCell{word: AtomicU64, stamp: AtomicU64}>` | the seqlock word `gen<<1 \| stable` + the lifetime stamp; NO removal path (bounded by geometry: capacity/chunk entries) | BA:L284-321, L534-547 |
| `stamps_present` | `AtomicBool` | "any lifetime stamp exists" latch so `live_incarnation` skips the probe when none can exist | BA:L322-340 |
| `incarnation_minter` | `OnceLock<{era, seq cursor}>` | the lifetime-stamp minter (`era << 40 \| seq`) | BA:L341-348, L549-556 |
| `quarantine` | `data_custody::BlockQuarantine` | dead-epoch do-not-reallocate set (rung 6) | BA:L349-354; DC:L365-431 |
| `grace` | `free_grace::GraceRing` | freed-offset grace ring (rung 6) | BA:L355-360 |
| `block_grant` | `OnceLock<BlockGrantArm>` | ranged-grant arm (rung 6) | BA:L361-366, L417-432 |
| `fresh_supply_exhausted_since_ns` | `AtomicU64` (0 = clear) | the latched `StorageFull` word | BA:L367-373 |
| `topup_rekick_at_ns` | `AtomicU64` | rate limit on the latched armed writer's proactive re-ask | BA:L374-377 |

Process-wide: `FRESH_SUPPLY_LATCHED: AtomicUsize` = number of allocators whose latch is set; the
write hot path does ONE relaxed load and runs no census while it reads 0 (`BA:L163-176`).
`Drop` decrements it if latched (`BA:L558-567`).

### 1.2 The incarnation word (`IC:L1-83`) — layout and every atomic

Word = `gen << 1 | stable` per offset. Constants: `UNSTABLE_FIRST = 1<<1` (first observed by a
writer: gen 1, unstable), `STABLE_FIRST = 1` (first observed by a publish: gen 0, stable),
`UNKNOWN_STABLE = u64::MAX` (snapshot sentinel for an offset with no cell — treated stable)
(`IC:L27-33`).

| Op | Code | Ordering | Paired with |
|---|---|---|---|
| `retire(word)` | `fetch_update(AcqRel, Acquire, cur → ((cur>>1)+1)<<1)`; then `fence(Release)` | the trailing Release fence is the WRITER half of the seqlock: orders the word write before the caller's later data writes, so a reader whose data read saw post-retire bytes and then runs `still`'s Acquire fence must see the retired word | IC:L35-51 |
| `publish(word)` | `fetch_or(1, AcqRel)` | — | IC:L53-56 |
| `snapshot(word) -> ?u64` | `load(Acquire)`; `None` iff bit0 == 0 | — | IC:L58-67 |
| `still(word, before)` | `fence(Acquire); load(Relaxed) == before` | the READER half: an Acquire LOAD only orders later accesses; the fence orders the prior payload read before the validation load (Boehm seqlock paper; x86/TSO masks it, ARM/POWER do not) | IC:L69-83 |

Allocator wrappers: `mark_incarnation_unstable(offset)` = `retire` on an existing cell or insert
`{UNSTABLE_FIRST, INCARNATION_NONE}` (`BA:L1555-1567`); `publish_block(offset)` = `publish` or
insert `{STABLE_FIRST, NONE}` (`BA:L1571-1583`); `fill_incarnation(offset)` = `snapshot`, absent
cell ⇒ `Some(UNKNOWN_STABLE)` (`BA:L1718-1726`); `fill_incarnation_still(offset, before)` =
`still`, absent cell ⇒ `before == UNKNOWN_STABLE` (`BA:L1730-1738`).

Refcount cell protocol (`RC:L1-62`): `peek` = `load(Acquire)` (`RC:L25-32`); `try_acquire` = CAS
loop from NONZERO only (`AcqRel`/`Acquire`), `false` at 0 (`RC:L34-46`); `release` = CAS loop,
`true` exactly on the 1→0 transition, saturates at 0 (`RC:L48-62`). `cross_word_fence()` =
`fence(SeqCst)` (`PC:L52-54`).

**Ordering law inside `claim_block_idx` (`BA:L2451-2512`):** cancel elided debt → `retire` word →
mint + store the lifetime stamp → `refcounts.insert(1)` LAST. The word and stamp move BEFORE the
refcount entry exists: a pin landing between the two reads "untracked" and takes nothing;
the reverse order let it raise the new count against the OLD stamp (`BA:L2456-2461`). A lingering
refcount entry at claim ⇒ `block_claim_anomalies` + ERROR (`BA:L2484-2500`).

### 1.3 What the loom models assert, and which weakening fails

| Model | Threads | Invariant | Lines |
|---|---|---|---|
| `incarnation_validated_fill_never_serves_dead_bytes` | writer: retire → payload=1 → publish; reader: snapshot → load payload → still | a validated fill's bytes stamp == snapshotted gen | LOOM:L517-554 |
| `incarnation_two_reuse_cycles_still_exact` | two reuse cycles vs one fill | same at generation precision | LOOM:L556-585 |
| `refcount_acquire_never_resurrects_freed_block` | cloner `try_acquire` vs owner `release` from 1 | ¬(freed ∧ acquired), freed ∨ acquired, the clone's release is terminal, a second release is `false` | LOOM:L587-622 |
| `refcount_exactly_one_terminal_release` | two releases from 2 | exactly one observes 1→0 | LOOM:L624-642 |
| `patch_clone_composed_never_mutates_a_validated_pin` | patch: retire → `cross_word_fence` → peek==1; clone: try_acquire → `cross_word_fence` → snapshot | ¬(pinned ∧ snap == STABLE_FIRST ∧ patched); post-join the patch re-stabilized; refcount conserved | LOOM:L644-740 |
| `incarnation_validated_fill_never_serves_mid_patch_bytes` | in-place patch (retire → fence → payload=1 → publish) vs validated fill | no mid-patch bytes published | LOOM:L742-783 |

Weakenings recorded: removing `retire`'s trailing Release fence or `still`'s Acquire fence lets a
fill read mid-flight bytes yet validate against the pre-retire word (`IC:L39-45, L72-79`);
weakening EITHER `cross_word_fence` to Release/Acquire admits the store-buffering outcome (both
sides read old: patch sees refcount 1 AND clone sees stable) — "verified during development"
(`LOOM:L657-665`; `PC:L19-33`). Port: `src/model/` gets `incarnation_model.zig` (retire/publish/
snapshot/still with an explicit per-thread store buffer the fence drains) and
`patch_clone_model.zig` (the two-word composition); each `expectHolds` + the named weakening
`expectFails` per AGENTS.md "Model checks".

### 1.4 Allocation funnel

`allocate_block()` (`BA:L2228-2241`) = `allocate_block_inner` then latch bookkeeping: `Ok` ⇒
`note_fresh_supply_available` (swap latch to 0, decrement the process count, `BA:L669-677`);
`StorageFull` ⇒ `note_fresh_supply_exhausted` (CAS 0→now, increment, INFO log only if no supply is
visible, `BA:L645-666`).

`allocate_block_inner` (`BA:L2243-2382`), in order:
1. `alloc_plane_gate` (read-only mount refuses, `BA:L1106-1113`).
2. `try_allocate_block` (`BA:L2400-2446`): `harvest_grace()` first (one relaxed load unarmed);
   grant-armed ⇒ the window is the ONE source (`next_fresh_block`, `alloc_fresh_mints`); else
   free-list claim loop — `iter().next()` then `remove(idx)`; a LOST race rescans (never falls to
   the fresh mint: a lost race on a cursor-at-cap store fired the valve per allocation and cost
   −33 % rewrite, contract 7, `BA:L2387-2399`); `alloc_from_freelist`; list empty ⇒
   `next_fresh_block()` (`alloc_fresh_mints`) ⇒ `claim_block_idx`.
3. `StorageFull` ∧ grace ring non-empty ⇒ `harvest_grace_pressure()`; retry; else
   `note_alloc_stall` (rung 6, `BA:L2251-2270`).
4. `StorageFull` ∧ grant-armed ⇒ inline top-up ask, retry, join a proactive ask, retry, classify
   the answer (rung 6, `BA:L2271-2291`).
5. Non-`StorageFull` errors return as-is (`BA:L2292-2294`).
6. The ENOSPC pressure valve, ≤ `ENOSPC_VALVE_MAX_ATTEMPTS = 32` passes (`BA:L2328-2371`): retry
   the mint; `await_trim_return()` parks on an open claim window's return edge (bounded by
   `block_reclaim::PARK_BOUND_CEILING_MS`, counted `alloc_trim_window_parks`, `BA:L1350-1363`) and
   `continue`s; no valve wired ⇒ `Err(e)`; `pending = space_pending()`; `valve().await` (the
   off-thread queue drain); grant-armed ∧ pending ⇒ re-ask the holder; `!pending` ⇒ one last try
   — refuse unless a claim window opened during the drain. Past 32: ERROR log naming
   `block_free_reclaim_queue_bytes`, `alloc_trim_window_parks`, `block_free_reclaim_fence_halts`
   and one last try (`BA:L2372-2381`). WHY 32: each pass awaits a FULL drain; the pre-fix loop had
   no bound and spun while concurrent reclaim traffic kept `pending` true (RES-15, `BA:L154-161`).

`next_fresh_block` (`BA:L2099-2144`): grant-armed ⇒ `window.mint()` (+ `highest_block.fetch_max`,
`kick_block_grant_topup`) or `StorageFull` "window empty"; else CAS loop on `highest_block`
(`Relaxed`/`Relaxed`), `StorageFull` iff `cap != 0 ∧ idx ≥ cap` ("full: N of M blocks
allocated") — a refused racer never bumps the cursor.

`allocate_block_grace_bounded()` (`BA:L2174-2185`) — the WRITE PATH's form: loop `allocate_block`;
on `StorageFull` ⇒ `park_for_reclaimable_supply(e, started, grace non-empty)` (`BA:L2201-2226`):
`!reclaimable` ⇒ `Err(e)` at once; `waited ≥ pressure_park_wall_ms()` ⇒ ERROR + `Err(e)`; else
sleep `pressure_park_slice_ms()` (`note_pressure_park`) and retry. On an unarmed mount the ring is
structurally empty ⇒ byte-identical to `allocate_block` (one branch).

Other mints: `allocate_block_below(below_idx) -> ?offset` (lowest free idx < bound, sorted
candidates, CAS-claim, NEVER a fresh mint — the defrag mover, `BA:L2531-2551`);
`allocate_block_at_or_above(min_idx)` (lowest free ≥ min else fresh — ascending convergence,
`BA:L2559-2577`); `allocate_specific_block(idx)` (gap-fills `cur_highest..idx` into the free list,
counts `unpublished_offsets_recovered += idx − cur_highest` when `idx > cur_highest`, else removes
from the free list or refuses "not free"; refcount 1; cancels debt — the recovery census arm,
`BA:L3067-3104`); `recover_block(idx)` (the seed protocol: gap-fill + cursor, or remove from the
free list + count 1, or `fetch_add(1)` an existing count, `BA:L3119-3144`);
`seed_from_durable_refs(&[idx])` (sort ascending, `recover_block` each, `META_KV_BLOCK_REFS_RECOVERED
+= n`, `BA:L3547-3556`).

### 1.5 Free, pin, patch

`begin_free(offset) -> bool` (`BA:L2601-2651`): `plane_gate` (rung 6: lease) → `refcount_core::
release` on the cell; terminal ⇒ remove the entry, `clear_shared`, `true`; non-terminal ⇒ `false`;
UNTRACKED ⇒ REFUSED `false` + ERROR + `block_untracked_free_refusals` (the second half of a
double-release; leak-safe direction — fsck C6 reconciles a genuine limbo, `BA:L2586-2600`);
terminal ⇒ `mark_incarnation_unstable` (retire BEFORE the offset becomes reallocatable so an
in-flight fill of the stale map fails `still`).

`finish_free(offset)` (`BA:L2655-2705`): quarantine non-empty ∧ `defer_free` ⇒ WARN, return
(rung 6); `recall_gate_verdict() == Gated` ⇒ publish directly + `del_obj` (rung 6); `grace.defer`
⇒ `del_obj`, `harvest_grace`, return (rung 6); else `publish_free_list` + `del_obj`.
`publish_free_list(offset)` (`BA:L2711-2773`): holder arm (rung 6: `alloc_lease::note_finish_free`
clears the bit; on an armed allocator the bitmap IS the free list — return without inserting;
`false` ⇒ DOUBLE FREE) → forensics tape (env-gated) → `free_blocks.insert(idx)`; `false` ⇒ ERROR
"DOUBLE FREE" + `block_double_frees`.

`free_block(offset)` = gate + `begin_free` + `finish_free` with nothing between (`BA:L2916-2926`).
`abandon_unpublished_offset(offset)` (`BA:L2935-2986`): a joined non-holder gives it back to its
grant window (rung 6); else `free_block`.

`increment_refcount(offset) -> bool` = `try_acquire`; `false` + WARN on 0 or untracked (never
fabricate a count for a possibly free-listed offset, `BA:L1745-1769`). `refcount(offset) -> ?u32`
= `peek` (`BA:L1776-1779`). `pin_block_validated(offset) -> {Pinned, PinnedUnstable, Refused}`
(`BA:L2047-2063`): increment → `cross_word_fence` → snapshot; the fence sits BEFORE the word
lookup so the absent-word arm is fenced too. `begin_patch_sole_owner(offset) -> bool`
(`BA:L1889-1923`): plane gate → `mark_incarnation_unstable` → `cross_word_fence` →
`refcount == Some(1) ∧ !is_shared`; on `false` the caller MUST `publish_block` (re-stabilize
under a NEW generation, content unchanged) and fall back to CoW.
`patch_range_shared(ino, start, end, holder)` (clause 7: the range-grant table, rung 6,
`BA:L2008-2030`).

### 1.6 Census, fsck seams, accounting faces

`derived_allocation_snapshot()` → `{highest, set_words (1 bit/block below the cursor, SET = not
reallocatable now), free}` (`BA:L945-981`); `drain_free_list_into_grants()` (`BA:L986-996`);
`fsck_begin_scan/fsck_bump_epoch/fsck_end_scan/allocation_epoch_of` (`BA:L1380-1403`);
`inflight_register(offset) -> InflightAllocGuard` (counted per offset; drop decrements and removes
at 0, `BA:L1411-1422, L3747-3761`); `inflight_contains/inflight_offsets/tracked_offsets/
free_blocks_count` (`BA:L1425-1458`); `fsck_set_refcount(offset, count≠0)` (`BA:L1491-1505`);
`fsck_reconcile_accounting() -> (frees_completed, free_list_evictions)` — untracked ∧ not
free-listed ∧ no in-flight owner ∧ not graced ⇒ complete the free; tracked ∧ free-listed ⇒ evict
(`BA:L1517-1544`); `highest_block_index`, `get_used_blocks = highest − free` (holder arm: bitmap
population − own window, `BA:L3040-3052`); `calculate_fragmentation` (`BA:L3054-3065`);
`free_block_indices()` sorted (`BA:L3113-3117`); `virgin_bytes() = (cap − min(cursor,cap)) × chunk`,
`u64::MAX` unbounded (`BA:L1279-1288`); `reachable_free_blocks() = virgin/chunk + reachable`
(`BA:L1473-1482`); `free_supply_blocks() = virgin/chunk + free_list_len` (`BA:L2835-2844`).
`derived_block_census(kv, router) -> BTreeMap<idx, u32>` and `recover_active_blocks_v3` share
`walk_live_layouts` — corpses (`nlink == 0`) INCLUDED (`BA:L3162-3267, L3612-3712`).
`layout_names_block_at(...) -> {Named, NotNamed, Unknown(why)}` — three-valued; an unreadable
justification is never `NotNamed` (`BA:L3353-3466, L3643-3655`).

---

## 2. Durable refs — the ledger that rides the layout tx

### 2.1 One record per REFERENCE (REFS:L22-94; DREF:L88-144)

Key (port form, `key.zig:323`): `06 ‖ vol_tag u64 ‖ block_idx u64 ‖ owner_ino u64 ‖ block_index
u32` BE (29 B; the reference is 28 B without the kind byte, `REFS:L99`). Value 4 B
`{version 1, flags bit0 MAP_BLOB bit1 SHARED, reserved u16 = 0}` (`REFS:L149-172`;
`record.zig:320-365` already refuses unknown version/flags/reserved). `block_index ==
u32::MAX` = the indirect-map blob reference (`REFS:L174-179`; `key.zig:176`).
`refcount(block)` = population of the `(vol_tag, block_idx)` prefix — `block_range` / the port's
`key.blockRange` (`REFS:L565-578`; `key.zig:446-453`); the per-volume scan unit is
`volume_range` / `key.volumeRange` with the all-0xFF 30 B end for `vol_tag == MAX`
(`REFS:L552-562`; `key.zig:435-442`).

WHY not a count (RMW across two inos with no common lock loses increments) and not a delta (a
Δ with no base folds to absent; "first reference?" is not knowable race-free) — `DREF:L117-131`.
Put at a key nobody else writes is idempotent; a release is a Delete (`REFS:L50-53`).

Ops: `BlockRefOp { reference, take: bool, shared: bool }`; `taken`, `taken_shared`, `released`
(`REFS:L253-296`). `cancel_same_reference_pairs(&mut ops)` drops every `(released r, taken r)`
pair over ONE reference (a passthrough `bk:off:len → bk:off:len'` clip): the record survives
unchanged and a from-scratch re-Put would strip a durable SHARED bit; a `taken_shared` is never
cancelled (`REFS:L298-343`). `frame_ram_only_candidates(caller, recomputed)` — the RAM-only
lifetimes of a publish frame (rung 6 owner-side recompute, `REFS:L413-469`).

### 2.2 The delta RIDES the layout transaction (same tx, ONE entry)

Every striped/staged block-map mutation goes through the ONE merge discipline, which already
computes inserted and displaced keys under the ino's meta stripe; the accounting is one vector
push per changed entry (`DREF:L148-173`). Sites and contributions (`DREF:L154-161, L531-540`):

| Site | Contribution |
|---|---|
| `BlockMapOp::Merge/MergeExpected` | `+new`, `−prev` per index whose binding changed; idempotent re-bind = nothing |
| `TruncateFrom/RemoveBlocks` | `−key` per pruned index |
| `publish_pass` (coalesced) | same per APPLIED op; fencing-stale ops contribute nothing |
| `clone_file` | `+key` per dest entry (the durable half of `pin_block_validated`) |
| `save_metadata_to_backend_ext` | `+`/`−` the indirect blob under `BLOCK_INDEX_MAP_BLOB`, same tx |
| staged→striped promotion, StorageFull spill, staged whole-image promotion, staged truncate prune | whole-map swaps through `block_ref_ops_for_map_swap(old, new)` — per-index diff (`RT:L9224-9251`) |
| `prepare_reclaim → reclaim_destroy` | `−` every entry and the blob, riding the destroy entry (§2.5) |
| `rewrite_shadow_record` | mutates RAM, persists later ⇒ the deferred-op accumulator (§2.3) |
| pack `commit` (charter.md:203) | `+ref` for the tenant and any displaced `−ref` in the SAME tx; the open pack is PINNED at `refcount = committed + mid-flight + 1` (`pack.rs:16-27, L283`); the seal releases the pin through `release_pack_reference` (`BA:L3004-3026`) |

`DataRouter::block_ref_ops(ino, &[(block_index, key, take)])` resolves each key to a `BlockRef`
(unresolvable ⇒ `META_KV_BLOCK_REFS_UNRESOLVED`), runs `cancel_same_reference_pairs`, and stamps
the fsync touched-namespace word for every taken key (`RT:L9263-9303`). The port resolves by
`vol_tag` — the only "unresolved" class left is a tag naming no mounted volume.

Measured (`BM-2026-08-04 §2`, Criterion `benches/meta_lv_bench.rs` group `block_refs`, dev box):
`delta_apply/1` (+1 ref) **40.1 ns**; `delta_apply/2` (+1/−1) **51.8 ns** = 26 ns/ref;
`delta_apply/64` 3.94 µs = 62 ns/ref; `key_build` 5.3 ns; **46 B of journal per reference**
(28 B key + 4 B value + record framing; 5888 B over 128 records); entry encode + xxh3 +11 ns per
record; at 2,600 publishes/s ≈ 0.013 % of a core, ≈ 239 KB/s journal, **+0 journal entries**
(pinned by `accounting_rides_the_publish_transaction_and_adds_no_commit`). Recovery scan decode
2.9 ns/ref; oracle fold+compare ≈ 90 ns/ref (`BM-2026-08-04 §3-4`). The port's v4 record header
is 16 B (decision 0002 row 37) so its per-reference journal cost is 29 + 4 + 16 = **49 B**; the
tie test restates that arithmetic, not the 46.

### 2.3 The per-ino deferred-op accumulator (`RT:L5745-5763, L9362-9419`)

`pending_block_refs: HashMap<ino, Vec<BlockRefOp>>`. A site that mutates the RAM map WITHOUT
persisting NOTES its delta inside the same meta-stripe section (`note_block_ref_ops`,
`RT:L9362-9376`); whichever save persists the map DRAINS it (`take_block_ref_ops`, order preserved
so take-then-release on one index keeps its order, `RT:L9414-9419`) into the same tx; a failed
commit re-notes them (the dirty-layout refill). `retract_block_ref_ops` removes a FAILED
promotion's own ops (a promotion never published the RAM entry, so re-noting would land as C8 drift,
`RT:L9378-9409`). **The drain law:** notes drain only into saves that persist the WHOLE map — their
presence forces a publish-class save off the layout-delta path (`has_deferred_refs`), because a
delta-class commit persists only its own entries and would durably claim releases for shadow
bindings its folded map does not reflect (`DREF:L571-588`; `RT:L5751-5758`). "Any future deferring
site is correct by construction rather than by remembering" (`DREF:L562-569`).
`ino_publishes_quiescent(ino)` reads it (no dirty layout, no pending ops, no open epoch shadow,
stripe try-lockable, `RT:L9330-9360`).

### 2.4 The release law (`REFS:L344-374`; `DREF:L241-296`)

"On a ledger-bearing volume a release may decrement the RAM refcount ONLY if THIS owner's durable
record existed at release time." A `Delete` of an absent key is a no-op in the tree and must be a
no-op in RAM; the RAM count was seeded from the records that exist, so a decrement with no record
behind it lands on whichever LIVE owner holds the offset now (generic/749, 2026-09-11: the retry's
`delete_file` re-read a surviving layout, `begin_free`'d its blocks a SECOND time, and the
decrement hit the pack block every promotion lands in). `ReleaseWitness = Derived | Shipped |
Ledger(Vec<BlockRef>)`: the releases whose record EXISTED at commit, probed under the ino's
exclusive 4a guard inside the destroy entry's commit; the rest count `block_release_skipped_no_record`.
In the port there is no `Derived` posture (the ledger is always engaged, §2.7).

### 2.5 The reclaimed-ino destroy rider (RECLAIM-ATOMIC, `DREF:L241-325`; `backend.rs:L24754-24775`)

`destroy_inodes_releasing(&[(ino, &[BlockRefOp])]) -> Vec<DestroyVerdict{Destroyed{held},
Skipped}>`: a corpse's reference releases ride the SAME journal entry as its inode + xattr Deletes;
a live (`nlink > 0`) or missing ino stages NOTHING and answers `Skipped`. `prepare_reclaim` reads
the layout and enumerates the release set (map, indirect blob, torn-down rewrite epoch's displaced
keys, drained notes) without touching the ledger; `reclaim_destroy` packs corpses per home volume to
the journal's whole-entry cap (`destroy_entry_bytes + release_records_bytes`), one entry per group,
bisects a failed group to the failing ino and REFUSES it (`reclaim_destroy_refused_release_failed`,
one WARN); `finish_reclaim` frees what the witness admits. Two half-states become UNREPRESENTABLE:
release-landed/destroy-not (the RAM gate makes a retry a leak-free no-op) and destroy-landed/
release-not (the inode gone, references on the ledger forever — the old residual A). A single ino
past the cap: `destroy_inode_chunked` under ONE held exclusive guard, order releases → other xattrs
→ `layout` xattr + inode record LAST; every committed prefix is a corpse the next sweep converges on
(`ChunkedDestroy{skipped, completed, held, entries, stopped}`, `REFS:L389-411`;
`reclaim_single_ino_chunked_destroys`). Engagement: `reclaim_release_destroy_joint_commits` (N
corpses → 1 entry, pinned N=8→1).

### 2.6 Mount recovery and the C8 oracle

`recover_durable_block_refs` (`RT:L4575-4627`): for each data volume's `vol_tag`, scan
`volume_range` on EVERY mounted meta volume (paged 512/page) and SUM the populations (records live
on the volume hosting the referencing inode, so a set-wide refcount is additive); feed indices
through `seed_from_durable_refs`; hand the same scan to the allocation arm as one bitset
(`alloc_lease::note_mount_seed`, rung 6, `AL:L1152`). **An EMPTY total population DECLINES** (`None`,
INFO log) so the caller runs the derived walk + `backfill_durable_block_refs` (one idempotent Put per
reference, one tx per inode, `RT:L4668ff`; `DREF:L327-354`). Port: the v4 format carries the ledger
from birth (§2.7), so the backfill arm exists only for the empty-volume case and the walk is the
fsck oracle alone.

`verify_durable_block_refs` (`RT:L4796-4892`): per data volume, durable `BTreeMap<idx, count>` from
the scan vs derived from `derived_block_census` over every meta volume; drift rows `(vol, idx,
durable, derived)`; the first 8 offenders logged WITH owners (`ino + map index` names the publishing
site); the raw comparison OBSERVES and convicts nothing — `META_KV_BLOCK_REFS_DRIFT` is fed by the
VERDICT sites only (fsck's confirmed C8 and the quiesced mount-init post-sweep verify, `RT:L4871-4882`).
fsck C8 (`FSCK:L5948-6014`): verify-before-report — re-run ONCE per confirm pass (memoized), keep
the finding iff THIS block still disagrees; an OPEN pack block or a block with a live in-flight owner
is excused and counted (`pack_ledger_exempted`, `inflight_exempted`). Repair (`FSCK:L8492-8600ff`):
ONE direction, OFFLINE only (online refuses — the verification read and the release must not be
interleaved by a live publish), the open-pack belt, every record of the block judged before ANY is
released, `Unknown` justification refuses the whole finding, stale records released
(`release-stale-block-refs`, `fsck_repair_classC8`); the other direction (a layout naming a block with
no record) is NEVER fabricated (`DREF:L200-214`).

### 2.7 Decision for the port: the ledger is always on

The reference built the ledger behind an un-stamped incompat bit ("D9: built, NOT stamped",
`DREF:L359-397`) because its write-path wiring could be incomplete and a PARTIAL ledger is the
dangerous state (non-empty, so the empty-declines rule never fires). The port's format v4 is
forward-only and the charter (charter.md:187) states the ledger as THE representation: there is no
derived-accounting posture, no `block_refs_engaged()` predicate, no `SQUEEZEFS_TEST_STAMP_BLOCK_REFS`
seam. The derived walk survives ONLY as the fsck C8 oracle and the mount-time fallback for a volume
whose total population is 0 (the "genuinely holds no data" case — the walk finds no layouts).
Add a row to decision 0002 (§10 Q1).

---

## 3. Reclaim — every stage

### 3.1 The terminal-free ladder (`RT:L5208-5394`; charter.md:222)

`free_block_verdict(key) -> bool terminal`:
1. (rung 6) a non-holder SHIPS the free to the holder and answers `false` (`RT:L5216-5221`).
2. incarnation check: a key naming a DEAD lifetime refuses `err_stale_incarnation` + `block_key_
   incarnation_refusals` (must-stay-0; the §6.3 hazard's destructive face, `RT:L5227-5235`).
3. (rung 6) SHARED mark ⇒ ask the index home; `Held{remaining}` ⇒ `release_shared_held`, `false`;
   `Freed|NotShared` ⇒ `clear_shared` and fall through; home unreachable ⇒ keep the reference,
   `SHARED_RELEASE_FAILURES`, `false` (`RT:L5250-5302`).
4. (rung 6) untracked at a grant-armed holder ⇒ adjudicate through the owner ladder
   (`block_untracked_free_adjudicated`, `RT:L5319-5335`).
5. `begin_free(offset)` → tenant ledger (`pack_terminal_frees` / `pack_partial_frees` for decorated
   keys, `RT:L5337-5348`).
6. terminal ⇒ **tier purge** (`read_tier_purge(key)` — ONE helper over all four read tiers, after the
   retire, before reallocatable; a straggler validated fill either landed before the purge or fails
   its own `still` after it, `RT:L5349-5361`; charter.md:228).
7. terminal ∧ `elide_reclaim_for(device_path)` ⇒ `record_elided_debt(offset, block_size)` →
   `finish_free(offset)` → `block_free_reclaim_elided` → `debt.record(allocator, device)`; return
   `true` — ZERO device commands (`RT:L5363-5379`).
8. terminal otherwise ⇒ `inflight_register(offset)` and `reclaim.enqueue(ReclaimEntry{allocator,
   inflight, device_path, offset, size: block_size})` — the queue owns `finish_free`
   (`RT:L5380-5392`).

**Discard elision predicate** `elide_reclaim_for(path)` (`BR:L403-411`): lever ON ∧ (`BdevDiscard`
class ∨ the test seam `set_elision_class_all`). Classes `free_reclaim_op(st_mode)`: `S_IFREG ⇒
FilePunch`, `S_IFBLK ⇒ BdevDiscard`, else `Skip` (`RT:L2012-2030`); memoized once per device path
(`BR:L386-399`). WHY bdev-only: on a raw block device `fallocate(PUNCH_HOLE)` was `blkdev_issue_
zeroout` — a full block of Write-Zeroes per freed block (field 1.85× write amplification at 98 %
util; rig **2.000×** exactly, `RT:L1997-2001`; `BM-2026-07-27-shim-write-amplification`); regular-file
backings keep the punch because the host-FS space return is a REAL ENOSPC vector on overcommitted
hosts (KD-4.1, `BR:L339-342`). Correctness never depended on deallocation: unmapped blocks read
zeros, reuse is guarded by write-before-publish + the incarnation word (`RT:L2007-2011`).

**Watermark** `debt_within_watermark(debt, virgin) = debt ≤ virgin` (KD-4.6, no constants): while
the virgin tail ≥ debt the thin exposure is bounded by what fresh-minting would have consumed
anyway (`BR:L413-421`). **Claim-cancels-debt**: `claim_block_idx` and `allocate_specific_block` call
`cancel_elided_debt` (`BA:L1232-1239, L2453-2455, L3099-3101`).

### 3.2 The reclaim queue (`BR:L783-887`)

`ReclaimEntry { allocator, inflight: InflightAllocGuard, device_path, offset, size }` (`BR:L762-770`).
`ReclaimQueue`: `q: SegQueue`, `len` (bumped BEFORE push — an upper bound a pop can never underflow),
`processing` (reserved BEFORE the pop), `supply: HashMap<device, SupplyEntry{alloc, queued}>`,
`pressure_latched`, `notify`, `worker_armed`, `fence_signal: OnceLock<Fn()->bool>`, `halted`,
`dismounted`, `batch_blocks`, `batch_ms`, `cap_blocks` + `cap_pinned`, `lanes_per_dev`,
`park_bound_ms` + `park_pinned`, `room: Notify`, `cap_wake: Notify`, `arrivals`, `arr_sample`,
`arr_sample_ms`, `arrival_rate`, `drain_rate`, `drain_win_start_ms`, `drain_win_blocks`,
`displacement_seed`, `fg_signal`, `fg_last`, `test_stall_ms`.

`population() = len + processing` — the cap governs queued + in-flight (a pop opens no room; a
reclaimed range does, `BR:L964-970`; the W-4 fix — before it `take_batch` moved 2048 entries into
`processing` at pass start and every later producer parked the whole pass wall, `BM-2026-09-05 §1`).

**enqueue** (`BR:L1171-1210`): read-only mount ⇒ ERROR, `halt_device_reclaims`, drop (a reader's
discard is the one hazard that destroys data); `block_free_reclaim_queued += 1`; `arrivals += 1`;
`population ≥ cap ∧ !fence_halted` ⇒ `park_at_cap()`; `queue_bytes += size`; `note_supply_enqueue`;
`len += 1`; push; `ensure_worker`; `notify_one`.

**park_at_cap** (`BR:L1225-1269`): `cap_parks += 1`; `ensure_worker`, `notify_one`, `cap_wake.
notify_one` (pull a manners-deferred worker out of its tick NOW); `start = now`; loop: register
`room.notified_raw().enable()` BEFORE the re-check (registration-before-recheck, PERF-13);
`population < cap ∨ fence_halted` ⇒ return; `deadline = start + park_bound_ms (re-read LIVE each
wake — a cold 1 s park collapses to the derived bound the moment the first window rolls)`;
`now ≥ deadline` ⇒ `cap_overflow += 1`, return (soft overflow: the entry goes in past the cap,
RAM-bounded, conservation by the valve/unmount/idle drains); `race(edge, sleep(min(remaining,
MANNERS_TICK_MS)))`; a tick wake before the deadline counts `park_tick_wakes` (the lost-wake
tripwire ≈ 0). The bound survives re-parks (measured from the park's START).

**take_batch(max)** (`BR:L1464-1481`): `processing += 1` BEFORE `pop`; `Some` ⇒ `len −= 1`,
`note_supply_dequeue`; `None` ⇒ `processing −= 1`, break. Pop-ownership is exactly-once: whoever pops
processes (worker, valve drain, unmount drain); nothing is ever re-queued (`BR:L23-26`).

**The worker** (`ensure_worker`, `BR:L1364-1459`; a `Weak` so a dropped router's queue exits): wait
`notify`; sleep `batch_ms` (accumulation window: displaced blocks of one stream arrive back-to-back,
coalesce adjacency); inner loop: `sample_arrivals()`; `at_cap = population ≥ cap`;
`foreground_active() ∧ !at_cap` ⇒ (`pressure_latched ∨ supply_pressure()` ⇒ latch, `supply_drains
+= 1`, fall through to drain) else (`len == 0` ⇒ break/park; else DEFER: `race(cap_wake.notified(),
sleep(MANNERS_TICK_MS))`, continue); `take = batch_blocks × lanes_per_dev`; `take_batch(take)` empty
⇒ clear `pressure_latched`, break; `dispatch_lanes(batch)`.

**The manners law inputs** (`BR:L69-96, L738-754, L1289-1296, L1344-1359`): `device_activity_signal
= write_through_bytes + patch_write_bytes + staging_put_bytes_{drain,flush,wt_fallback} +
ranged_read_bytes + read_device_true_reads + hot_block_misses` (wrapping sum, relaxed);
`foreground_active` = the signal moved since the previous worker decision (one swap per pass, never
on the enqueue path); DEVICE-byte movement, never op counts (a 1 Hz stats poller or RAM-served reads
hold nothing deferred). `supply_pressure` (finding 40): any device with `queued × 2 ≥
free_supply_blocks` (capacity-unbounded allocators never read pressure) — thin supply outranks
manners so headroom regenerates AHEAD of allocation; `pressure_latched` hysteresis runs the drain
to EMPTY (the ratio is a trigger, never a per-pass gate; without it 17–29 of 32 drained then a
forever-deferred tail, `BR:L791-798`). Verdict table: foreground + below cap ⇒ DEFER (zero commands);
at cap ⇒ drain regardless (parked enqueues are foreground writers too); idle ⇒ full width to empty
(the field's 128 GiB backlog clears in ≈ 12 s). WHY: the target-side deallocate service is the drain
ceiling (idle ≈ 2,700 cmd/s at width 32, ≈ 5,900 at 128; under foreground write load ≈ 1,700 cmd/s at
ANY width), so aggressiveness under load buys nothing and steals target CPU; the fresh row after a
128 GiB `rm` paid −17 % (`BR:L69-78`).

**dispatch_lanes** (`BR:L1495-1531`): group by device; sort by offset; per device `n_lanes =
clamp(ceil(group.len ÷ batch_blocks), 1, lanes_per_dev)`, `chunk = ceil(group.len ÷ n_lanes)`
— CONSECUTIVE slices of the sorted order (adjacency runs survive into each lane's coalescer); width
= Σ_devices n_lanes (the "backends × demand" law: a shallow queue stays one lane, a storm overlaps up
to `lanes_per_dev` fabric round trips per device, `BR:L47-67`); `block_free_reclaim_batches += 1` per
lane; each lane `run_blocking(process_entries)`; await every lane (the worker never outruns its
submissions). Field conviction: queue pinned at cap 17.4 GB ≈ 4,153 × 4 MiB, 196,747 queued vs 589
batches, serial drain ≈ 500 blocks/s vs ≈ 1,650/s displacement (`BR:L49-55`).

**process_entries** (`BR:L1617-1720`): test stall seam; `BatchGuard` reconciles `processing` and
`queue_bytes` for anything unprocessed on unwind and fires the room edge; **fence check ONCE per
batch before any device command** — halted ⇒ `fence_halts += n`, drop WITHOUT `finish_free` (the
successor's recovery owns the accounting, the kill-9 posture; the in-flight guards deregister on
drop); `dismounted` ⇒ `dismount_halts += n`, drop likewise (a discard after the teardown's own drain
would land on an offset the next mount at this mount point re-carved, record §4.4bx); group by
device, sort, `coalesce_entry_ranges` → `(start, len, k)` (adjacent iff `start + len == offset`,
`BR:L1726-1738`); `open_reclaim_target(device)` once per group (`None` = Skip class ⇒
`reclaim_skipped += k`, never a zeroing write); PER RANGE: `issue_range` → `finish_free` each of the
k entries (reallocatable only NOW — a new owner's DMA can never race the discard) → `processing −= k`,
`queue_bytes −= bytes` → `note_drained(k)` → `room_made()` (room flows at the drain rate, not in
pass-sized bursts).

**issue_range** (`BR:L1799-1865`): `reclaim_commands += 1`; `FilePunch` ⇒ `fallocate(fd,
FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, start, len)` ⇒ `file_punches += k`, `punch_bytes += len`;
`BdevDiscard` ⇒ `ioctl(fd, BLKDISCARD = 0x1277 (_IO(0x12,119)), [start, len])` (NVMe DSM Deallocate,
no payload, not write-bandwidth-accounted) ⇒ `discards += k`, `discard_bytes += len`; Trim venue
additionally `trim_discards += k`, `trim_bytes += len`; refused ⇒ `reclaim_skipped += k`, NEVER a
zeroing-write fallback. Counters stay PER-BLOCK when ranges merge (`block_free_discards ≡ displaced
blocks`, `BR:L1611-1613`); `blocks ÷ commands` is the live coalesce factor (contract 11).

**drain_sync** (`BR:L1542-1568`): loop `take_batch(batch_blocks)` → `process_entries`; empty ∧
`processing == 0 ∧ len == 0` ⇒ return processed; 30 s deadline ⇒ ERROR "proceeding; allocation may
refuse StorageFull honestly"; `sleep(1 ms)` poll. `drain_off_thread()` = `run_blocking(drain_sync)` —
the valve's body: the allocating task awaits, the ioctls run on the blocking pool, racing
engagements drain CONCURRENTLY (the single-flight shape measured −15 % on the 4-wide storm bracket;
inline drains froze whole handler lanes, `BR:L1570-1590`). `pending() = !fence_halted ∧ (len > 0 ∨
processing > 0)` (`BR:L1597-1600`). `sync_drains` counts ONLY passes that processed ≥ 1 entry
(contract 8).

### 3.3 The derivations (`BR:L186-305`; `BM-2026-09-05 §3`)

```
room_ms(batch, drain_rate)   = drain_rate == 0 ? 1000 : max(1, batch × 1000 ÷ drain_rate)   BR:L241-246
park_bound_ms(room)          = clamp(4 × room, 50, 1000)                                      BR:L250-254
cap_ceiling(budget)          = clamp(budget ÷ 1024 ÷ entry_ram, 4096, 2^20)                 BR:L259-262
queue_cap(arrival, room, b)  = clamp(arrival × room ÷ 1000 [u128], 4096, cap_ceiling(b))     BR:L268-279
entry_ram                    = size_of(ReclaimEntry) + 32 (device-path heap) + size_of(usize) ≈ 104 B  BR:L230-232
```

| Constant | Value | WHY (on the line) | Lines |
|---|---|---|---|
| `MANNERS_TICK_MS` | 50 | the worker's deferred-drain re-evaluation cadence; the park bound's floor | BR:L191-193 |
| `QUEUE_CAP_FLOOR_BLOCKS` | 4096 | the shipped posture; never regress below | BR:L195-196 |
| `QUEUE_CAP_REGISTRY_MAX_BLOCKS` | 2^20 | the derived ceiling never exceeds what an operator may set | BR:L198-200 |
| `QUEUE_RAM_SHARE_DIVISOR` | 1024 | entries are bookkeeping only (bytes live on the device); 0.1 % of the budget is negligible by construction, never an R5 component | BR:L202-207 |
| `PARK_BOUND_FLOOR_MS` | = tick (50) | a bound below the worker's guaranteed cadence could trip on a healthy worker whose wake sat behind a saturated blocking pool | BR:L209-213 |
| `PARK_BOUND_CEILING_MS` | 1000 | the shipped constant: never park LONGER than shipped; the cold (no drain measured) value | BR:L215-218 |
| `PARK_BOUND_BATCHES` | 4 | a producer that saw no room edge in FOUR batch-times waits on a STALLED drain, not a slow one (room is made per range, so a live drain at any rate frees a batch within one batch-time) | BR:L220-225 |
| `DRAIN_RATE_WINDOW_MS` | 250 | the write pipeline's `WINDOW_MS` scale: two orders above a device command, re-learns a slowed target in a few windows; the FIRST window is one manners tick (a cold queue must learn inside the worker's cadence — p99.9 285 ms at a 250 ms first window vs an 80 ms derived bound) | BR:L317-320, L1006-1016 |
| EWMA α | ¼ | `prev − prev/4 + rate/4` on each window roll | BR:L1023-1033 |
| arrival peak-hold | `rolled_bw_peak` | windowed rate or the previous peak decayed by ⅛: a storm registers on its first window, a lull relaxes over ≈ 8 | BR:L862-865, L1049-1054 |
| displacement seed | write pipeline BDP peak ÷ block | an UPPER bound before the first sample (fresh writes displace nothing) | BR:L871-874 |
| `batch_blocks` / `batch_ms` | 64 / 2 | measured coalesce shape of the async-reclaim campaign | BR:L924-930 |
| `lanes_per_dev` | 32 | idle drain 2,700 → 5,900 cmd/s from 8 → 32; no sensitivity under load | BR:L933-937 |
| `IDLE_CONFIRM_TICKS` | 20 (= 1 s) | a single flat 50 ms tick is NOT idle — rows have sub-second lulls (fsync barriers, per-file closes) the zero-mid-row-discards gate counts as "during the row" | BR:L455-468 |
| `ENOSPC_VALVE_MAX_ATTEMPTS` | 32 | each pass awaits a FULL drain; the unbounded loop spun under concurrent reclaim traffic | BA:L154-161 |
| `drain_sync` deadline | 30 s | NO reason on the line (§10 Q6) | BR:L1543 |
| `FREE_FORENSICS_TAPE_CAP` | 4096 | a double-release investigation needs only the recent frees | BA:L5-12 |

Rederive (`BR:L1061-1085`): `room` from the live drain rate; unpinned bound ⇒ swap, a SHRINK is a
room edge; unpinned cap ⇒ from `arrival_rate` (or the seed when 0), a RAISE is a room edge; publish
the four gauges. Fleet under load (≈ 1,800 blocks/s, batch 64): room 35 ms, bound **140 ms** (was
1,000); idle catch-up (≈ 5,900/s) room 10 ms; at 4 MiB blocks the cap derives to the FLOOR
(4,750 × 35 ms ≈ 166 ≪ 4096) — the cap was never the fleet's lever, the BOUND + the room edge were
(`BM-2026-09-05 §3`). In-process A-B-B-A: pinned 1000 ⇒ p99.9 = max = 1,000 ms; derived ⇒ 80.6 ms
(`4 × 8 ÷ 400`), drain-rate gauge 399–405 vs the seam's 400 (`BM-2026-09-05 §4`).

### 3.4 The debt drainer (`BR:L434-646`) — idle / pressure venues

Per router; `targets: HashMap<device, allocator>` of every target that ever elided; armed lazily on
the first elided free; `must_defer()` = signal moved since last tick (reset `quiet_ticks`) ∨
`quiet_ticks < IDLE_CONFIRM_TICKS` (`BR:L540-548`). Pass (`BR:L556-641`): `fence_halted` ⇒ exit
permanently; `debt_drain_passes += 1`; per target: `debt == 0` ⇒ skip; `trim_venue_held()` ⇒ defer
(the mount-time hold until the allocation arm has judged its seed, `BR:L470-497`); `fg ∧ within
watermark` ⇒ defer (the row pays zero discards); `fg ∧ !within` ⇒ `debt_pressure_drains += 1`;
run ONE paced batch `drain_debt_sync(alloc, dev, batch_blocks, full=false)` on the blocking pool.
Verdict keys on what was RECLAIMED (finding 12: a fully grace-held backlog made every candidate
filter out and a "ran a batch" verdict looped hot at ≈ 5,000 jobs/s on an idle daemon at 88 °C):
`outstanding == 0` ⇒ park on notify; progressed ⇒ continue at once; deferred ⇒ sleep 50; else
`fruitless += 1`, sleep `min(50 << min(fruitless, 5), 1000)`.

`drain_debt_sync(alloc, dev, max, full) -> (blocks, bytes)` (`BR:L670-720`): candidates = `full ?
whole free list × chunk : take_debt_batch(max)`; per window of `max`: (full ⇒ `cancel_elided_debt`)
`claim_free_for_trim(offset)` — losers owe nothing (reused or a racing trim); sort; `coalesce_ranges`;
`issue_device_ranges(dev, ranges, Trim)`; `return_from_trim` each (guards drop after).
`take_debt_batch(max)` skips offsets the grace ring holds (`BA:L1250-1276`).

**The trim claim window** (KD-4.4, `BA:L1290-1340`): `claim_free_for_trim`: `trim_claimed += 1`
(SeqCst) BEFORE `remove_for_trim` (membership out, `reachable` untouched, the idx recorded in
`trim_windowed`) ⇒ `Some(inflight_register(offset))`; lost ⇒ `end_trim_claim`. `return_from_trim`:
`insert_from_trim` (membership back; a plain insert if no claim preceded — the seeding shape; a
re-list mid-window drops the window's share) ⇒ if a window closed, `end_trim_claim` = `trim_claimed
−= 1`, `trim_returned.notify_waiters()`. The insert precedes the count's release so a parked
allocation that reads the window closed finds the offset on its rescan. WHY: a full store under
rewrite (virgin 0 ⇒ the pressure venue drains on every pass) puts its ENTIRE free list inside a
window at times and the empty scan was a spurious, TERMINAL ENOSPC (R7, `BM-2026-09-07-overlay-
enospc-convergence-flake`); a window is pending supply, never a deficit (`BA:L440-448`).

**`trim --full`** (`RT:L5418-5477`): refuse if `fence_halted` (a fenced holder issues NO destructive
command); targets = every backend (full) or the debt targets; per target on the blocking pool:
`drain_debt_sync(…, 64, full)` once (full) or until `outstanding == 0` (debt). The free list is the
durable truth and the debt tracker only its incremental record, so a full trim restores substrate
hygiene after debt was lost to a crash/unmount (KD-4.9).

### 3.5 Fence latch, dismount, crash posture

`fence_halted()` (`BR:L1113-1137`): sticky `halted`; else probe `fence_signal()` (wired from the
meta set's `is_failed` latch — the SAME signal the journal-barrier escalation sets, `BR:L805-812`);
first observation ⇒ ERROR, latch, `room_made()` (parked producers break on the fence).
`halt_device_reclaims()` latches directly (the teardown/crash-analog arm, `BR:L1156-1159`);
`close_at_dismount()` sets `dismounted` (`BR:L1140-1143`). Crash (kill -9 with queued entries): the
queue is RAM-only space-return work; the mount recovery seeds refcounts/free list from the durable
records; the only loss is the discard — un-returned thin space, re-covered on reuse or by `trim
--full` (`BR:L32-39`; `DREF:L227-239`). Port: the reclaimer takes the port's `nvme_dev.FenceSignal`
(`nvme_derive.zig:421-424`) AND the meta `failed` latch as its probe; the `custody_poisoned` latch
(`nvme_derive.zig:436-450`) is the process-wide twin.

### 3.6 Ledger identities (stated and tested)

`block_free_reclaim_queued + block_free_reclaim_elided ≡ terminal frees` (`FC:L7230-7232`);
`block_free_reclaim_queued ≡ displaced blocks` (`BR:L1168-1170`); `block_free_discards (+ punches +
skipped) ≡ queued blocks processed` per-block regardless of coalescing (`BR:L1611-1613`);
`blocks ÷ block_free_reclaim_commands` = coalesce factor; `pack_partial_frees + pack_terminal_frees
≡ tenant releases` (`RT:L5202-5207`); per grace ring `deferrals ≡ releases + len` (`FG:L1313-1316`,
rung 6); `block_free_reclaim_queue_bytes` returns to baseline at quiesce (`FC:L7156-7158`).

---

## 4. ENOSPC — terminal for a write that would CREATE custody

### 4.1 The latched `StorageFull` per allocator (`BA:L620-732`)

The never-lossy law covers ACKED custody; a write that would create custody of a block the set
cannot allocate is refused BEFORE the ack. The verdict is the allocator's own: its TERMINAL
`StorageFull` (every valve pass, top-up ask and grace park exhausted) sets `fresh_supply_exhausted_
since_ns`; its next landed allocation clears it. `fresh_supply_visible()` = `free_supply_blocks() > 0
∨ grace non-empty ∨ trim_claimed > 0 ∨ space_pending()` — the conjuncts ARE the mint sources, and a
source added later must join them (`BA:L683-688`). `fresh_supply_latched()` (PURE: latched ∧
!visible, `BA:L703-707`); `fresh_supply_exhausted()` = latched, and on a grant-armed writer kicks the
proactive re-ask at most once per `membership::renewal_beat_ms` (`BA:L714-732`). There is
deliberately NO time-bounded re-probe that admits a write: an admitted write on a still-full volume
is acked custody that can never land (`BA:L631-636`). The latch is INERT whenever supply is visible.

### 4.2 The placement-candidate walk (`RT:L3553-3605`)

`BackendRouter::fresh_supply_exhausted()`: `FRESH_SUPPLY_LATCHED == 0` ⇒ `false` (one relaxed
load, no census); else `fresh_supply_census(|a| a.fresh_supply_exhausted())` over the placement
table's ELIGIBLE rows (healthy ∧ active at the last refresh; one ArcSwap load; N relaxed loads; a
volume that lost health after the refresh stays a candidate one cadence — the conservative
direction) ⇒ `true` iff `exhausted == candidates`; an empty candidate set ⇒ `false` (the allocation
path refuses "no healthy backends" itself). `fresh_supply_exhausted_volumes() -> ?(latched,
candidates)` is the `.stats` face (`alloc_fresh_supply_exhausted`).

Placement itself (`RT:L1374-1399, L2860-2885, L3396-3449`): `balance_penalty = clamp(1000 × (fill −
set_mean), 0, 300)`; `health_effective = device_health − penalty` saturating (device_health =
`(1 − fill) × 1000`; the cap 300 keeps balance from ever outvoting health — a degraded backend still
loses to a merely-full one); an immutable `PlacementTable{rows, band (eligible within 90 % of the max
health_effective), rr cursor}` refreshed by the health worker and on registration/state changes;
`pick` = one load + one atomic round-robin inside the band; stale table ⇒ one rebuild. This matches
charter.md:204 ("health- and fill-weighted placement … a published immutable choice table").
`allocate_placed_block()` = pick + `allocate_block_grace_bounded` on the ONE picked volume; a
`StorageFull` runs the pressure-supply hook (close the mount's open rewrite epochs, free their parked
displaced keys) and retries the mint ONCE with a fresh pick (`RT:L3486-3511`); the `_under_guard`
form skips the hook for callers holding a level-3.5 stripe or a 4a guard (`RT:L3528-3539`; lock law
pinned by `tests/allocation_act_lock_law_tests.rs`).

### 4.3 The write-path refusal (`FC:L20845-20881`)

In `write_file_staged`, decided for the WHOLE write ahead of every block future: `fresh_supply_
exhausted() ∧ write_would_create_fresh_custody(ino, start_block, end_block)` ⇒ `write_fresh_block_
enospc_refusals += 1`, `StorageFull` ("refused before the ack; acked custody is unaffected"). Custody
the daemon already holds (a mapping, a parked buffer, a staged copy or extent record, an open overlay
record) keeps the ladder — a passthrough whole-block rewrite of a mapped block lands in place,
space-neutral; a hole below EOF holds nothing and is refused like growth. The 74 GiB story: on the
1.3.0 chain (generic/751) every post-fill write acked into a parked buffer, staging filled, the parked
set grew past the memory cap to a **74 GiB daemon on a 24 GiB volume**, and the unmount lost it all
(`FC:L20849-20853`; charter.md:218).

### 4.4 The wall-bounded park (`FG:L3009-3035`)

`pressure_park_wall_ms() = max(2 × fence_bound_base_ms(), 1000)` — a DURATION derived from the plane's
ROUTINE fence bound (the most patient duration the plane publishes), floored at one second; past it
the refusal stands, loud: the plane is broken, not slow. Never `bound()` — that is the reallocation
LABEL (an owner-clock instant whose "nothing owed" sentinel is `u64::MAX` on every non-owner mount),
and reading it as a wait made the park unbounded on every co-writer and reader (the s11 fleet
wedge: 100 writes in flight for 30 min, 37k watchdog lines, `.stats` in D-state —
`BM-2026-09-06-cowriter-enospc-wedge`; `write_enospc_terminal_tests.rs:1-39`).
`pressure_park_slice_ms() = pressure_bound == 0 ? 50 : clamp(pressure_bound ÷ 16, 10, 250)`.
`fence_bound = 2 × ack_cycle`, `ack_cycle = 3 × renew_interval + 3 × staleness + skew_max + D_purge`;
an explicit `SQUEEZEFS_FREE_GRACE_MAX_MS` below one cycle REFUSES (`FG:L1104-1141`). Rung 4 has no
reader plane: `fence_bound_base_ms() == 0`, so the wall is exactly **1 s** and the ring is empty, so
`allocate_block_grace_bounded` never parks at all — the law is in place for rung 6 to fill.

---

## 5. The unpublished-destination law and the fsck visibility guard

Law 2 (reserve-before-DMA): the destination is claimed from `allocate_block` (incarnation unstable)
and its `InflightAllocGuard` held BEFORE any `WRITE_FIXED` names it (`DOV:L160`). Law 8: the
displaced binding is freed only after a durable save no longer references it (`DOV:L166`). Law 9
(Rev 2 correction A): an unpublished offset returns to the allocator ONLY through its mint ROLLBACK
owner (`MintedBlockGuard` — disarmed solely when durable map/ref publication transfers ownership;
drop ⇒ a detached single `free_block` on the meta pool, `AT:L353-385`) or an explicit terminal free,
and only when no DMA naming it can still land; **the `InflightAllocGuard` is fsck VISIBILITY ONLY —
dropping it frees nothing** (`DOV:L167, L230-231`, KD-OV-11 `DOV:L718`). Every owner path
(writeback/flush units, active-block uploads, parked writes, mover and wire-shard destinations, pack
tenants from reserve to commit) registers; the guard drops only after the publish is durable and
visible to the tree/refcount reads fsck performs; a crashed owner drops it on unwind so a genuinely
leaked offset is never shielded (`BA:L234-244, L1405-1410`). fsck C2/C3 exempt offsets in the
allocation-epoch side map (minted while a scan was latched — younger than the scan epoch) and in the
in-flight registry (`BA:L1398-1403, L1429-1434`); C6's limbo reconciliation exempts the trim claim
window's registration and the grace ring (`BA:L1292-1295, L1521-1536`). Crash: the destination is
referenced by no durable map ⇒ recovered FREE (`unpublished_offsets_recovered` — the honest generic
census count, never an overlay-named one, KD-OV-14 `DOV:L657, L721`; `BA:L3071-3085`). The charter's
pack compaction and job fabric apply the same law ("pre-allocated unpublished destination … refcount
raised to the referencer count before any publish", charter.md:203, 251).

---

## 6. Knobs (registry rows to add to `src/core/knobs.zig`)

| Name | Kind / range | Default | Purpose | Class | Ref |
|---|---|---|---|---|---|
| `SQUEEZEFS_RECLAIM_BATCH_BLOCKS` | int 1..=1024 | 64 | worker batch (the coalesce quantum; also the grace `HARVEST_BATCH`) | A/B lever | ENV:L190; BR:L924-930 |
| `SQUEEZEFS_RECLAIM_BATCH_MS` | int 0..=MS_MAX | 2 | accumulation window after the first wake (large values park the worker — tests) | A/B lever | ENV:L191 |
| `SQUEEZEFS_RECLAIM_QUEUE_MAX_BLOCKS` | int 1..=2^20 | derived (floor 4096) | deferred-space cap (queued + in-flight); explicit wins verbatim, 4096 = the A/B lever | A/B lever | ENV:L192; BR:L285-293 |
| `SQUEEZEFS_RECLAIM_LANES_PER_DEV` | int 1..=64 | 32 | per-device parallel drain lane cap | A/B lever | ENV:L193 |
| `SQUEEZEFS_RECLAIM_CAP_PARK_MS` | int 0..=60000 | derived (50..1000) | at-cap park bound; 1000 = the A/B lever, 0 = never park (immediate soft overflow) | A/B lever | ENV:L195; BR:L297-305 |
| `SQUEEZEFS_DISCARD_ELISION` | bool | on | elide bdev-class discards into debt; 0 = the queued-reclaim control arm AND the operational escape for substrates needing eager space return | A/B lever | ENV:L196; BR:L344-365 |
| `SQUEEZEFS_FREE_FORENSICS` | bool | off | capture a free's provenance to pair a double free (reference: a backtrace string per free; §10 Q7) | A/B lever (diagnostic) | ENV:L312; BA:L96-104 |
| `SQUEEZEFS_BLOCK_REFS_VERIFY` | bool | off | run the durable-vs-derived oracle at MOUNT (pays the walk the ledger exists to delete; fsck C8 is unconditional) | A/B lever | ENV:L163 |
| `SQUEEZEFS_TEST_RECLAIM_STALL_MS` | int 0..=MS_MAX | 0 | stall each reclaim batch (deterministic slow-device schedules for the valve-liveness and room-edge contracts) | test seam | ENV:L398; BR:L881-886 |
| `SQUEEZEFS_TEST_CORPSE_SWEEP_FAIL_DESTROY` | bool | 0 | the sweep refuses every chunk's destroy after `delete_file` (the released-but-undestroyed state the release witness must make harmless) | test seam | ENV:L410 |
| `SQUEEZEFS_TEST_DESTROY_CHUNK_STOP_AFTER` | int 0..=u32 | 0 | stop a chunked single-ino destroy after N entries (the crash-between-chunks state) | test seam | ENV:L411 |
| `SQUEEZEFS_FREE_GRACE_MAX_MS`, `SQUEEZEFS_FREE_GRACE_MAX_OFFSETS` | int ≥1000 / int 1..=2^26 | derived | the grace ring's fence bound and cap — **rung 6**; listed because `pressure_park_wall_ms` reads the bound | A/B lever | ENV:L359, L377 |
| `SQUEEZEFS_TEST_STAMP_BLOCK_REFS` | — | — | NOT carried (§2.7) | — | ENV:L413 |

The reference's reclaim knobs are read through a private `env_u64(key, default, lo, hi)` that
`.clamp`s out-of-range values (`BR:L178-184`) — the port reads every row through `knobs.intKnob /
boolKnob` on the registry row (refuse, never clamp; decision 0002 rows 14/16). `resolve_queue_cap_
blocks(explicit, derived)` / `resolve_park_bound_ms` are the pure `explicit > derived` resolvers
(`BR:L291-305`) and keep that shape.

---

## 7. Gauges, counters, histograms

Must-stay-0 tripwires (`counters.Tripwire`, per-site label): `block_double_frees` (FC:L5978-5981),
`block_untracked_free_refusals` (growth = a double-release lineage upstream; leak-safe, FC:L5982-5990),
`block_claim_anomalies` (FC:L6116-6123), `block_key_incarnation_refusals` (a live map named a dead
lifetime, FC:L6145-6153), `block_key_incarnation_exhausted` (FC:L6168-6172), `block_live_free_
refusals` (FC:L6104-6115, rung 6), `block_free_reclaim_fence_halts` (0 on healthy mounts — always
beside `writer_guard_fenced`, FC:L7168-7176), `meta_kv_block_refs_drift` (fsck C8 confirmed +
quiesced mount verify ONLY, `kv/mod.rs:L519`), `pack_release_untracked_noops` (FC:L5906),
`unpublished_mint_abandons` (FC:L6821), `reclaim_destroy_refused_release_failed` (growth = a failing
meta volume, never a leak, FC:L6015-6021), `block_free_reclaim_park_tick_wakes` (≈ 0 while room
flows — the lost-wake tripwire, FC:L7221-7223), `alloc_trim_window_parks` (≈ 0 except a full store
under rewrite with elision, FC:L7253-7261), `block_free_reclaim_dismount_halts` (moves only between
teardown and exit, FC:L7177-7184).

Per-op counters (`counters.ShardedCounter`): `alloc_from_freelist`, `alloc_fresh_mints` (which
stream is recycle-bound, FC:L6783-6785), `del_obj` (FC:L5397), `block_free_discards`,
`block_free_discard_bytes`, `block_free_file_punches`, `block_free_punch_bytes`, `block_free_reclaim_
skipped` (sustained growth on thin provisioning = space not returned, FC:L7134-7147),
`block_free_reclaim_queued`, `block_free_reclaim_batches`, `block_free_reclaim_sync_drains` (0 except
under real space pressure, FC:L7163-7167), `block_free_reclaim_cap_parks`, `block_free_reclaim_cap_
overflow` (≈ 0 while the drain keeps pace; the NORM in the pinned regime, each costing exactly one
bound, FC:L7185-7204), `block_free_reclaim_commands` (FC:L7224-7229), `block_free_reclaim_elided`
(FC:L7233-7237), `block_free_trim_discards`, `block_free_trim_bytes` (growth DURING a measured row =
elision regressed, FC:L7243-7248), `block_free_debt_pressure_drains` (FC:L7249-7252), `block_free_
reclaim_supply_drains` (steady growth at high fill is the arm working, FC:L7262-7269), `block_free_
debt_drain_passes` (bounded growth at idle is the law, FC:L7270-7275), `write_fresh_block_enospc_
refusals` (FC:L6265), `block_release_skipped_no_record` (FC:L5998-6010), `reclaim_release_destroy_
joint_commits`, `reclaim_single_ino_chunked_destroys` (FC:L6091-6103), `pack_partial_frees`,
`pack_terminal_frees` (FC:L5899-5900), `block_pin_stale_keys`, `block_key_incarnation_unknown`
(FC:L6154-6167), `unpublished_offsets_recovered` (nonzero after a crash is the design working; growth
on clean remounts is a leak, FC:L8237; DOV:L657), `rewrite_shadow_pressure_closes` (FC:L6921),
`meta_kv_block_refs_staged` / `_released` / `_recovered` / `_unresolved` (`kv/mod.rs:L496-527`;
DREF:L436-444: staged flat under a striped storm ⇒ wiring regressed; released flat under overwrite
churn ⇒ displaced blocks leak their accounting).

Gauges (`counters.Gauge`): `block_free_reclaim_queue_bytes` (queued + in-flight bytes, FC:L7156-7158),
`block_free_elided_debt_bytes` (FC:L7238-7242), `block_free_reclaim_drain_rate`, `block_free_reclaim_
arrival_rate`, `block_free_reclaim_queue_cap`, `block_free_reclaim_park_bound_ms` (FC:L7205-7220),
`alloc_fresh_supply_exhausted` as `(latched, candidates)` (RT:L3561-3572), `alloc_fresh_supply_
reask_ms` (BA:L690-698, rung 6), per-volume `used/free/capacity` (`VolumeStateRow`, RT:L1360-1372),
`dlm_quarantined_offsets` (rung 6, FC:L6757).

Closures: §3.6, plus `fsck_findings == 0` on a healthy volume with the in-flight registry + epoch map
as the exemptions (charter.md:255).

---

## 8. Constants and measured numbers (beside the derivation they justify)

| Item | Value | Instrument / venue | Ref |
|---|---|---|---|
| `CHUNK_SIZE` | 4 MiB | the allocation stride; a longer image tramples the next chunk (FIND-RW4-A) | BA:L106-112 |
| discard RTT on nvme-tcp | ≈ 235 µs (fio psync QD1 randread clat 248 µs) | 4-node NVMe-oF/TCP; the synchronous per-displaced-block discard stream cost ≈ 2 GB/s of a 4 GiB/s overwrite | BR:L5-12; BM-2026-07-27-async-block-reclaim L6-20 |
| PUNCH_HOLE on a bdev = Write-Zeroes | write amplification 2.000× (rig), 1.85× at 98 % util (field) | elbencho overwrite legs | RT:L1997-2001; BM-2026-07-27-shim-write-amplification L13, L64-74 |
| inline at-cap arm | 12–22 ms synchronous fabric round trip on the handler lane | field, write-wall iteration 1 | BR:L100-103 |
| target deallocate service | idle ≈ 2,700 cmd/s @ width 32, ≈ 5,900 @ 128; ≈ 1,700 under foreground load at ANY width | 4-node cluster width experiments | BR:L73-77 |
| backlog drain tax | fresh row −17 % minutes after a 128 GiB `rm`; idle catch-up clears 128 GiB in ≈ 12 s | field verdict v2 | BR:L71-72, L89 |
| queue pinned at cap | 17.4 GB ≈ 4,153 × 4 MiB; 196,747 queued vs 589 batches; ≈ 500 drained/s vs ≈ 1,650 displaced/s | 4-node, mid-rewrite 6.3 GB/s | BR:L49-55 |
| field rewrite tail | p99.9 893 ms / max 3.08 s = multiples of the 1 s park | 2026-09-01 rewrite rows, 4 MiB @ ≈ 19 GB/s | BM-2026-09-05 §1 |
| derived bound on the fleet | 140 ms (room 35 ms @ 1,800 blocks/s, batch 64) | arithmetic on the measured drain rate | BM-2026-09-05 §3 |
| in-process A-B-B-A | p99.9 1,000 ms (pinned) vs 80.6 ms (derived = 4 × 8 ÷ 400); 12.4× | `reclaim_park_rows`, release, dev box | BM-2026-09-05 §4 |
| lost free-list race | −33 % rewrite at fill 0.62; `sync_drains` 67,376 ≈ per allocation at fill 1.0 | three-session field ledger | BA:L2387-2399; async_block_reclaim_tests.rs:70-88 |
| ENOSPC valve single-flight vs concurrent | −15 % (single-flight) / −9 %, −12 % (alternatives) on the 4-wide storm bracket | counted A/B | BR:L1580-1584; BA:L2300-2305 |
| ref delta CPU | 40.1 ns (+1), 51.8 ns (+1/−1), 62 ns/ref at 64 | Criterion `block_refs` | BM-2026-08-04 §2 |
| ref journal bytes | 46 B/ref (28+4+framing; port: 49 B with the v4 16 B header) | in-situ entry size | BM-2026-08-04 §2 |
| ref scan decode / oracle | 2.9 ns/ref; ≈ 90 ns/ref fold+diff | Criterion | BM-2026-08-04 §3-4 |
| incarnation map ceiling | capacity/chunk entries ≈ 40 B each; 100 TB @ 4 MiB ≈ 25 M ≈ 1 GB at full lifetime coverage | RES-13 | BA:L299-311 |
| incarnation stamp | `era << 40 \| seq`, seq 1..=2^40−1 (0 = NONE); 4 EiB of fresh blocks per mount | `GRANT_SEQ_BITS = 40`, `TERM_MAX = 2^24−1` | RT:L2110-2137; dlm.rs:L923-933 |
| 74 GiB daemon | 24 GiB volume, generic/751 | 1.3.0 chain | FC:L20849-20853 |
| debt drainer hot loop | ≈ 5,000 jobs/s, ≈ 1 core, 88 °C on an idle set authority | finding 12 | BR:L609-619 |
| grace ring floor | 131,072 entries = 12.7 GB/s × 38 s ÷ 4 MiB (rung 6) | BM-2026-07-28-ingest-economy | FG:L1054-1060 |

---

## 9. Tests (name — one sentence; venue needs)

Unit (portable unless noted):
- `allocate_block_respects_device_capacity` — three mints then `StorageFull` at cap; a free reopens exactly one slot (BA:L3767-3798).
- `block_refs` codec tests: `vol_hex_ids_decode_to_themselves`, `keys_are_memcmp_ordered_by_dimension`, `key_roundtrip_and_length_refusal`, `value_roundtrip_and_refusals`, `shared_flag_rides_the_value_and_a_plain_take_is_unchanged`, `same_reference_pairs_cancel_and_nothing_else_moves`, `block_range_bounds_exactly_one_block`, `volume_range_bounds_exactly_one_volume` (REFS:L580-765; the key/value halves already exist in `key.zig`/`record.zig` tests — port the pair-cancel law).
- `derivation_sweep_tests`: `reclaim_room_latency_is_batch_over_measured_drain_rate`, `reclaim_park_bound_derives_from_the_drains_room_latency`, `reclaim_queue_cap_derives_from_displacement_rate_and_room_latency`, `reclaim_knobs_explicit_wins_verbatim_over_derived` (L2464-2636) — the W-4 tie tests.
- `block_free_reclaim_tests`: `classify_block_device_discards_never_zero_writes`, `classify_regular_file_punches`, `classify_anything_else_skips`; `terminal_free_on_file_backing_is_one_counted_punch`, `nonterminal_free_reclaims_and_counts_nothing`, `batched_frees_count_one_reclaim_per_key` (file-backed router).

`async_block_reclaim_tests` (file-backed router + a real striped FS fixture; Linux for the punch):
- `terminal_free_queues_and_background_worker_reclaims` (contract 1), `nonterminal_free_queues_nothing`, `enospc_valve_never_blocks_executor_threads` (stall seam; contract 2b), `enospc_pressure_valve_drains_queued_reclaims_before_refusing` (2), `clean_unmount_drains_queued_reclaims` (3), `concurrent_frees_and_drains_reclaim_exactly_once` (4), `fenced_guard_halts_worker_batch_reclaims`, `fenced_guard_halts_sync_drain_and_enospc_valve` (6; fences a real meta volume), `kill9_with_queued_discards_leaves_recoverable_volume` (5; child process ×5), `field_rewrite_free_list_regime_runs_off_the_write_path` (7), `valve_counts_only_genuine_nonempty_drains` (8), `brim_rewrite_converges_in_place_with_honest_ledger` (9; W1 whole-block face), `displacement_storm_never_caps_queue_or_spills_inline` (10), `adjacent_displaced_ranges_coalesce_into_fewer_device_commands` (11), `drain_defers_under_foreground_and_catches_up_idle` (12; injected signal), `thin_supply_drains_despite_moving_foreground` (finding 40), `at_cap_enqueue_parks_until_drain_relieves_and_never_overflows` (13), `at_cap_park_ends_on_the_room_made_edge_not_the_quantum` (14; 60 s quantum, 1 s stalled lane ⇒ ≈ 1 s), `derived_park_bound_never_overflows_while_the_drain_keeps_pace` (15), `derived_queue_cap_never_regresses_below_the_shipped_floor` (16), `reclaim_park_rows` (#[ignore] A-B-B-A rig).

`discard_elision_tests` (file-backed with the class-all seam): `elided_terminal_free_is_reallocatable_with_zero_device_commands`, `trim_drains_debt_with_counted_commands_and_returns_offsets`, `stale_debt_never_discards_an_owned_offset` (claim loses ⇒ nothing issues), `elision_lever_off_restores_queued_reclaim_verbatim`, `file_backings_keep_the_queued_reclaim_without_the_seam` (contract 5: the bdev-only fence), `watermark_is_debt_versus_virgin_tail`, `fenced_daemon_never_trims`, `a_fully_grace_held_backlog_paces_the_drainer` (finding 12), `allocation_racing_a_trim_claim_window_parks_and_lands_on_the_returned_offset` (contract 9).

`durable_block_refs_tests` (format + mount + data file; Linux): `a_clone_records_durable_refcount_two_and_survives_remount`, `a_crash_in_the_free_window_neither_leaks_nor_double_frees`, `durable_matches_derived_across_a_mixed_workload`, `accounting_rides_the_publish_transaction_and_adds_no_commit` (journal entry deltas EQUAL, staged +8), `a_data_device_power_cut_leaves_ledger_and_layout_agreeing`, `the_indirect_map_blob_carries_its_own_durable_reference`, `fsck_reports_no_durable_reference_drift_on_a_healthy_volume` (the permanent C8 pin: publishes, a displacement, a clone, a punch, a truncate), `fsck_repair_releases_a_stale_durable_reference_offline_and_never_fabricates_one`, `a_malformed_reference_record_refuses_the_population_on_either_layout`, `mid_epoch_delta_publish_cannot_strand_pending_shadow_notes_across_a_crash` + `pending_shadow_notes_land_durably_by_the_epoch_close` (Vector B: the drain law), `reclaim_of_an_ino_with_an_open_rewrite_epoch_orphans_no_durable_reference`, `a_prior_eras_unforgotten_corpse_is_reclaimed_at_mount`, `a_corpse_whose_reference_was_already_released_cannot_free_a_live_owners_block` (generic/749), `an_overcap_corpse_population_is_reclaimed_across_chunked_destroys`, `an_overcap_ref_load_publishes_chunked_and_converges`, `a_failed_release_commit_never_lets_the_destroy_orphan_the_ledger`, `a_reclaim_batch_releases_and_destroys_in_one_journal_entry` (N=8 → 1), `a_single_corpse_whose_destroy_exceeds_the_cap_is_reclaimed_across_entries`, `a_corpse_whose_release_alone_exceeds_the_cap_orphans_no_record`, `a_chunked_destroy_stopped_between_entries_is_finished_by_the_next_mount`. NOT carried: `a_fresh_format_does_not_carry_bit8_and_mounts_derived`, `unstamped_volume_is_unchanged_by_mount_and_stays_derived`, `stamping_the_bit_engages_accounting_on_the_next_mount`, `stamping_a_non_empty_volume_backfills_instead_of_freeing_live_blocks` (the D9 stamp dance, §2.7) — keep only "an empty population declines and the walk seeds".

`write_enospc_terminal_tests` (mount-level rig, BS 64 KiB, 10 s bound): `the_park_wall_is_a_duration_never_the_reallocation_label`, `a_full_store_with_nothing_reclaimable_refuses_at_once`, `full_store_writes_terminate_and_the_mount_keeps_answering`, `a_parked_pipeline_upload_returns_its_permit_within_the_bound`, `an_enospc_write_does_not_poison_its_sibling_block`, `freeing_space_after_enospc_lets_new_writes_land`, `the_write_enospc_refusals_gauge_counts_exactly_the_refused_writes`, `fresh_block_writes_against_an_exhausted_set_are_refused_before_the_ack`, `fresh_block_writes_are_refused_on_the_synchronous_shape_too`, `acked_custody_of_an_open_block_keeps_the_never_lossy_ladder`, `a_brim_rewrite_of_a_mapped_block_is_admitted_while_the_set_is_exhausted`, `the_exhaustion_latch_clears_when_supply_returns`, `concurrent_fresh_block_writers_against_a_latched_store_are_all_refused`, `a_write_into_a_hole_below_eof_of_a_pre_sized_file_is_refused_when_exhausted`, `a_rewrite_of_a_mapped_block_on_a_transformed_volume_is_refused_when_exhausted`, `a_dropped_latched_allocator_leaves_the_process_count`, `a_retryable_class_is_never_a_terminal_writeback_error`.

`reused_key_stale_fill_tests` (striped FS fixture): `parked_read_never_serves_reused_or_freed_key`, `size_bump_never_reverts_merged_map`, `stress_recycled_keys`, `test_terminal_free_purges_read_tiers`, `test_validated_read_cache_publish_incarnation_gate`, `test_put_ring_never_clobbers_live_entries`.
`refcount_clone_tests`: `test_increment_reports_refusal`, `test_clone_fails_loud_when_blocks_unpinnable`, `test_clone_retries_via_authoritative_map` (bounded `attempt ≥ 3`), `test_nonterminal_free_must_not_punch_shared_block`, `test_terminal_free_punches_before_offset_is_reallocatable`, `test_clone_of_large_inline_v3_file_pins_every_block`.
`mw_block_key_incarnation_tests`: `the_incarnation_codec_is_canonical_and_refuses_junk`, `a_reallocated_offset_names_a_new_lifetime`, `the_reclaim_window_invariant_survives_lifetimes`, `a_stale_keys_pin_takes_no_reference_on_the_offsets_new_lifetime`, `a_stale_binding_is_refused_instead_of_serving_another_files_bytes`, `an_offset_with_no_recorded_lifetime_serves_and_is_counted`, `the_durable_era_makes_lifetimes_unrepeatable_across_remounts`, `a_power_cut_and_remount_refuse_the_pre_cut_lifetime` (the base-36 `@` wire form tests are NOT carried — the port's POINT2/RUN2 carry the stamp as a u64 field).
`hole_read_zeros_tests`: `punch_reads_zeros`, `punch_whole_block_striped`, `punch_then_reuse_reads_zeros`, `truncate_down_stale_size_reads_zeros` (the "freed ranges are never read" half of the elision argument).
`corpse_sweep_tests` (binary-driven mounts): `a_released_corpses_stale_free_never_touches_a_live_files_block`, `an_overcap_corpse_population_is_fully_reclaimed_in_one_sweep`, `the_generic_749_shape_survives_a_released_corpse_at_the_pack_offset`, `a_single_overcap_corpse_stopped_between_entries_is_finished_by_the_next_mount`.
`allocation_act_lock_law_tests::every_hooked_allocation_act_caller_holds_no_meta_stripe_and_no_dlm_guard` — a static rail over call sites (port: `tests/rails.zig`).
`reader_free_grace_tests` — rung 6.

Loom → `src/model/`: the six models of §1.3 with their weakenings.

---

## 10. QUIRKS (decision 0002 rows to add) and the better design

| Q | Reference quirk | Why poor | Port |
|---|---|---|---|
| 1 | The ledger behind an un-stamped incompat bit with a derived-walk posture, `block_refs_engaged()` predicates on every site, `ReleaseWitness::Derived`, the backfill-on-stamp dance and `SQUEEZEFS_TEST_STAMP_BLOCK_REFS` (DREF:L359-397, L616-640) | Two accounting postures for one fact; the dangerous PARTIAL ledger exists only because wiring could lag the stamp | Always on from format (§2.7); the walk is the fsck oracle + the empty-population seed; no engaged predicate, no stamp seam |
| 2 | Block keys as decorated STRINGS (`oss1://off@inc:rel:len`) resolved by `clean_block_key`/`parse_block_key_parts` with `backend_0`/`squeezefs` aliases on every free/pin/census (RT:L5222-5247, BA:L3507-3530) | Parsing on hot paths; alias rules duplicated in three walkers; `meta_kv_block_refs_unresolved` exists to count parse misses | Binary POINT/RUN/POINT2/RUN2 (`record.zig`) carry `vol_tag` + offset + stamp; resolution is a `vol_tag → allocator` table; `unresolved` counts only an unknown tag |
| 3 | `incarnations: scc::HashMap<offset, cell>`, `refcounts: scc::HashMap`, `free_blocks: DashSet` + `trim_windowed: DashSet` + `elided_debt: scc::HashMap` + `inflight: scc::HashMap` — six hashed maps keyed by offset, ≈ 40 B/entry, the incarnation map "deliberately has NO removal path" (BA:L299-321) | Hashing on every pin/fill/free; six consistency surfaces for one per-block state; the reference's own ceiling is ≈ 1 GB at full coverage | ONE dense per-block table sized at mount from `capacity/chunk` (`[]BlockCell{word u64, stamp u64, refcount u32, state u8(virgin/free/allocated/limbo/trim_windowed/graced/quarantined), inflight u16, debt_bytes…}` ≈ 24–32 B/block — 25 M blocks ≈ 600–800 MB worst case, still below the hash maps; charged to the budget as a non-sheddable component) + a free BITMAP with 64-word summaries for `lowest free ≥ / < idx` picks — which IS rung 6's "the bitmap is the free list" (§11) |
| 4 | Reclaim knobs read through a private `env_u64(...).clamp(lo, hi)` (BR:L178-184) and `explicit_*` filters that silently fall to the derivation in-process (BR:L285-305) | A second lenient parser that CLAMPS (charter §1.6 bans it; 0002 row 14) | Registry rows read through `knobs.*`; the pure `resolve*` keeps `explicit > derived` and never refuses (the gate did) |
| 5 | `ReclaimEntry.device_path: String` per entry, `BTreeMap<String, Vec>` grouping per pass, `open_reclaim_target` re-opening a NEW fd per device group per batch (BR:L762-770, L1496-1499, L1769-1781), `backing_class` memo keyed by path with a `stat` (BR:L386-399) | Heap strings on the free path; an open(2) per batch; a classification the Device already knows at open | Entry carries `vol: u16` (the allocator index); the reclaim module opens ONE write fd per device at registration and classifies from the Device's `st_mode` once (`Device.reclaimOp()` seam on `nvme_dev.zig`) |
| 6 | `drain_sync`'s `std::thread::sleep(1 ms)` poll with a free-floating 30 s deadline (BR:L1543-1567); the debt drainer's `sleep(50)`/escalating sleeps as the cadence (BR:L635-641) | Sleep-as-synchronization (AGENTS.md "No sleeps for synchronization"); a 30 s constant with no reason on the line | The drain waits on the room `Notify` (the in-flight set's completion IS the edge; `BatchGuard` fires it); the drainer's deferred cadence is the per-lane timer heap at `MANNERS_TICK_MS` with the WHY; no 30 s |
| 7 | `SQUEEZEFS_FREE_FORENSICS` captures `Backtrace::force_capture().to_string()` per free into a global `Mutex<HashMap<u64,String>>` ring (BA:L23-86, L2744-2759) | Kilobytes and a global mutex per free when armed; Zig has no cheap symbolized backtrace | A bounded ring of `(offset, site_label, thread_id, monotonic_ns)` records (no strings) — the double-free pairing names the two SITES |
| 8 | `recover_block` bumps an existing count with `fetch_add(1, SeqCst)` and `fsck_set_refcount` stores with SeqCst (BA:L3133-3139, L1498) | Bypasses the loom-verified `try_acquire`; SeqCst with no pairing named | Mount-time seeding runs quiescent on a dense table with plain stores; the one accessor family is `refcount_core`'s |
| 9 | `allocate_specific_block`/`recover_block` GAP-FILL `cur_highest..idx` into the free list (BA:L3086-3089, L3124-3127) — the "zombie window" the shipped-free seeding refuses to use (BA:L1801-1805) | Declares a live peer's minted-but-unpublished tail free; only sound on a quiescent single-writer mount | Gap fill exists ONLY in the mount-time seed (quiescent by construction); the runtime specific-claim refuses an index above the cursor on a grant-armed allocator (rung 6 carves instead) |
| 10 | Two `Notify` pairs + a `cap_wake` + `notified_raw().enable()` registration idiom copied from the write pipeline (BR:L844-855, L1237-1239) | A second copy of the registration-before-recheck protocol | ONE `sync.Notify` ("notify with permit + epoch and synchronous enable-then-recheck", charter.md:266) for `room`, `cap_wake`, `trim_returned` |
| 11 | `IncarnationMinter.seq` is a `lane_core::LaneCursor` striding by `writers` (BA:L1595-1612) | The lane partition is deleted (0002 row 45) | A plain per-mount `AtomicU64` from 1; the stamp is `era << 40 \| seq` with the era = the appender's durable term (rung 6; rung 4 engages with term 1) |
| 12 | `derived_block_census`/`walk_live_layouts` decode layouts as JSON-or-bincode `LayoutMetadata` with `file_type` strings (BA:L3697-3706, L3309) | 0002 rows 2/4 | The walk reads the port's typed layout record and the block-map tree (`key.indexRangeFrom`) |
| 13 | `MintedBlockGuard::drop` spawns a DETACHED async free on the meta pool because `Drop` cannot await (AT:L373-385) | A fire-and-forget free with no completion the caller can observe | Zig has no `Drop`: every mint owner returns its offset through an explicit `abandon` on its error path (`errdefer`), synchronous — `begin_free`/`finish_free` are not async |

---

## 11. Seams rung 6 needs (interfaces only)

1. **Allocation-plane gate** (`plane_gate(what)` / `alloc_plane_gate` / `holds_ownership_plane()`,
   BA:L1057-1113): every ownership-accounting mutation (mint, terminal free, specific claim, the
   seed's free completion, the W1 retire) routes through ONE gate `fn planeGate(self, what:
   Site) Refusal!void`; rung 4 implements the read-only arm (`EROFS`-class refusal) and leaves the
   `vol_tag → lease held?` arm as a `?*const fn` hook defaulting to "held".
2. **Free-list publish hook** (`publish_free_list` → `alloc_lease::note_finish_free(vol_tag, idx)
   -> cleared: bool`, BA:L2713-2742; AL:L867-873): `FinishFreeSink{ctx, noteFinishFree}`; when
   installed, a terminal free clears the bitmap bit and journals the kind-5 CLEAR delta
   (`key.TREE_DATA_ALLOC_RESERVED`, 0002 row 44) INSTEAD of inserting into the local list; `false`
   ⇒ `block_double_frees`. With Q3's dense bitmap the local list and the holder's bitmap are one
   structure — the hook becomes "journal the delta".
3. **Fresh-mint source** (`next_fresh_block`'s grant arm, BA:L2103-2121): `MintSource = union {
   cursor, window: *GrantWindow }`; `GrantWindow{install(BlockGrant) bool, mint() ?idx,
   give_back(idx) bool, remaining(), unconsumed(), drain(), wants_topup(), note_exhausted()}`
   (BG:L403-540); `BlockGrant{start u64, len u32}` (BG:L58-61); `BLOCK_GRANT_FLOOR = 64` = one
   write-pipeline BDP window (BG:L47-51); `TopupOutcome{NotAsked, Landed, Full, Unreachable,
   Deferred, Refused}` and `classify_topup_answer` (Unreachable/Deferred = retryable, NEVER a
   latched `StorageFull`; Refused = loud, BA:L865-913).
4. **Mount seed to the allocation arm** (`alloc_lease::note_mount_seed(vol_tag, blocks, idxs)`,
   AL:L1150-1158): `recover_durable_block_refs` hands the SAME scan over as one bitset so the arm
   never walks the ledger twice.
5. **Derived allocation snapshot** (`derived_allocation_snapshot() -> {highest, set_words, free}`
   + `drain_free_list_into_grants()`, BA:L945-996): the bitmap's FIRST hold; with Q3 it is a copy of
   the dense bitmap's complement below the cursor.
6. **Supply arithmetic hooks** (`grant_supply_blocks()`, `get_used_blocks` holder arm,
   BA:L2855-2861, L3040-3048): `free_supply_blocks`/`reachable_free_blocks`/`get_used_blocks` take
   an optional `HolderView{bitmap_blocks, population, window_remaining}`.
7. **Shipped frees** (`frees_ship_to_holder(key)`, `shipped_free::ship_displaced_frees`,
   `authority_accounting_scope_active()`, `seed_shipped_free_reference`, `retire_shipped_free_
   tracking`, `release_shipped_free_tracking`, RT:L5060, L5216-5221; BA:L1786-1864): the terminal
   ladder's step 1 is a `?FreeTarget` resolved by `vol_tag`; the three allocator seams exist as
   named functions with rung 4 bodies that are unreachable (no target installed).
8. **Shared mark + index home** (`mark_shared/clear_shared/is_shared/release_shared_held`, the
   `shared_free_gate`, RT:L5250-5302): one `state` bit in Q3's cell + `?SharedFreeGate` hook.
9. **Untracked-free adjudication** (`untracked_free_gate`, RT:L5319-5335): `?UntrackedFreeGate`.
10. **Quarantine** (`quarantine_offset/is_quarantined/release_quarantine`, `BlockQuarantine
    {admit, contains, defer_free, release}`, DC:L365-431; `finish_free`'s first gate BA:L2656-2670):
    one `state` value in Q3's cell + a `DeadEpoch` tag; rung 4 ships the two-line gate inert.
11. **Grace ring** (`GraceRing{defer(offset,size) bool, harvest_with_supply(max, supply,
    lane_reachable) []offset, harvest_pressure(max), holds(offset), len, bytes, oldest_label}`,
    FG:L1306-1650; `harvest_grace` at the funnel head and every terminal free BA:L2791-2810;
    `recall_gate_verdict()` FG:L3508): rung 4 ships `armed() == false` so `defer` returns `false`
    (one relaxed load) and the funnel's `harvest_grace()` is a no-op; `pressure_park_wall_ms` reads
    `fence_bound_base_ms() == 0` ⇒ 1 s.
12. **fsck visibility** (`inflight_register`, `fsck_begin_scan/bump/end`, `allocation_epoch_of`,
    `tracked_offsets`, `inflight_offsets`, `fsck_reconcile_accounting`, `fsck_set_refcount`,
    `layout_names_block_at`): rung 8 consumes; rung 4 ships them.
13. **Pressure-supply hook** (`pressure_supply_hook` on `allocate_placed_block`, RT:L3486-3511):
    `?*const fn() bool` closing the mount's open rewrite epochs — the rewrite epoch is rung 4's own
    (charter.md:213) so this is intra-rung, but it stays a hook so the allocator never imports the
    write path.
14. **Reclaimer probes** (`set_fence_signal`, `set_foreground_signal`, `set_displacement_seed`,
    BR:L1104-1107, L1274-1276, L958-962): `FenceSignal` (reuse `nvme_dev.FenceSignal`), a
    `ForegroundSignal{ctx, read}` defaulting to the device-byte counter sum, a `DisplacementSeed`
    from the write-pipeline governor's BDP peak.

On a `--single-writer` (rung 4) mount every hook reads 0/absent and every seam is one relaxed load or
one `?ptr` test — the reference's "byte-identical unarmed path" discipline (BA:L735-739, L2406-2418).
