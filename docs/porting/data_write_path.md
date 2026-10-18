# Porting spec — The data write path: ONE admission, five arms, fsync

> **Checksum amendment (2026-10-11, decision 0003 §M; 0002 row 480).** Every `xxh3` / `xxh3-64` /
> `xxh3_64` below names the hash the reference used and this port used until format v5. The port's
> ONE hash is now **sqzhash128** (`src/core/sqzhash.zig`, the pinned definition): every 8-byte checksum
> and identity field is its LOW 64 bits under the fixed public format key (`checksum.hash64`,
> `hash64FieldZeroed`, `stamp`/`verify`, the streaming `checksum.Hasher`); the seeded name hashes
> (`hash54`/`hash56`) are `hash64Keyed` under the per-mount name key (`key.NameKey`: the volume's
> `hash_seed` zero-extended, expanded once at mount). Field names on the wire read `csum`. Byte
> layouts, offsets and widths below are unchanged; only the function behind the 8 bytes is.

Scope: charter §6.2 ("ONE write admission — the cheapest legal I/O, judged per
page-aligned single-block PIECE"), §6.4 (fsync), §6.5 (`process_write` at every
write path), §1.3 (1 copy + 1 DMA; 0 + 1 zero-copy), §2 lock levels (2)/(3)/(3.5),
§13 rung 4 ("the write path in admission order (accumulation + coverage union → W1
→ overlay → rewrite epoch → extent overlay)"). What the port already has and this
spec sits ABOVE: `src/fuse/handler/op_guard.zig` (level 1 + shared-mode admission —
reference only here), `src/fuse/transport/slot.zig` (`PayloadLease`, the held zc
slot), `src/io/nvme_dev.zig` (`writeBlock`/ranged/`flush`/`classifyDmaOutcome`),
`src/meta/{backend,economy,tx}.zig` (one tx = one journal entry).

Reference (read-only, `~/Source/squeezefs`): `src/fuse_client.rs` (the WRITE handler
`:27674-28333`, `dispatch_striped_write :20760`, `write_file_staged :20818-21647`,
`try_sole_owner_patch :18340-18624`, `try_extent_park :18641-18765`,
`try_device_overlay_store :18867-19461`, `settle_overlay_block_locked :19862-20269`,
`try_serve_overlay_read :20550-20713`, `write_through_complete_block :21699-21828`,
`pipeline_upload_parked_block :21845-22039`, `flush_inode_to_backend_prof
:23251-23483`, `fsync :30064-30184`, `flush_one_active_block :33084-33351`,
`run_constant_writeback_worker :32542-32763`), `src/device_overlay.rs` (790),
`src/overlay_core.rs` (606), `src/coverage_core.rs` (207), `src/write_pipeline.rs`
(736), `src/write_pipeline_core.rs` (574), `src/patch_clone_core.rs` (59),
`src/cow_core.rs` (94), `src/cache/active_block.rs` (1203), `src/cache/nvme.rs`
(`StagedDmaSource :854-976`, `ExtentRecord :727-753`), `src/routing.rs`
(`RewriteEpoch :1592`, `rewrite_shadow_record :16407-16537`, `close_rewrite_epoch_with
:16726-16884`), `src/block_allocator.rs` (`begin_patch_sole_owner :1889`,
`pin_block_validated :2047`, `park_for_reclaimable_supply :2199`), `src/free_grace.rs`
(`pressure_park_wall_ms :3033`), `src/fsync_economy.rs` (413), `src/crypto_compress.rs`
(`FRAME_RAW_FLAG :44`, `TRANSFORM_BLOCK_HEADROOM :91`, `init_scratch_pool :440`),
`loom-models/src/lib.rs` (`:667`, `:749`, `:824`, `:4298-4494`, `:4669-4817`),
`docs/design-device-overlay.md`, `docs/design-overlay-overwrite.md`,
`docs/design-random-small-writes.md`, `docs/design-rewrite-program.md`,
`docs/design-zero-copy-write-path.md`, `src/env_knobs.rs:176-255,396-409`.

Conventions: `R:` = reference quirk flagged under decision 0002 (ledger candidate,
§13); `PORT:` = what the Zig port does instead. Every number carries its source
line. Lock levels are the charter's: (1) op guard, (2) lease, (3) `BLOCK_FLUSH_LOCKS
(ino, b)`, (3.5) `INODE_META_LOCKS(ino)`, (4a/4b) inside the commit.

---

## 1. The admission decision — ONE ordered predicate list per PIECE

### 1.1 Prelude (whole write, before any piece)

The WRITE handler, in order (`fuse_client.rs:27674-28245`):

1. `ro_gate` (EROFS on a read-only mount), op count (`:27684-27685`).
2. `is_virtual_ino(ino)` ⇒ `EACCES` (`:27715-27717`).
3. **Own the payload before ACK**: a zc-armed session delivered the payload HELD
   in the sparse slot — `data` is the empty placeholder and
   `ZcWriteSlot::from_connection(len, sound, conn, slot)` mints the slot source
   (`:27726-27755`); the §3.4 stability class `sound = !O_DIRECT ||
   FUSE_WRITE_CACHE` is stamped at mint (`:27741-27742`). Non-zc deliveries are
   `WritePayload::Bytes(data)` (the transport lease — `slot.zig PayloadLease`).
4. `wlen == 0` ⇒ `written: 0`, no lease, no dirty mark, no killpriv (`:27774-27776`).
5. `offset.checked_add(wlen) ≤ max_file_size()` else `EFBIG` (`:27785-27788`;
   `u32::MAX` blocks — charter §5.6).
6. `FUSE_WRITE_KILL_SUIDGID` ⇒ `apply_killpriv` BEFORE data lands (`:27795-27797`).
7. `dio_coherence = O_DIRECT && !FUSE_WRITE_CACHE` (generic/451, `:27804-27805`);
   non-dio ⇒ `note_page_instantiation` (`:27806-27812`); `mark_handle_dirty`
   (`:27815`); writeback queue-depth sample (`:27818-27819`).
8. **Level (1)** — the shared-mode admission (`:27838-28075`): already ported as
   `op_guard.zig` (`admitWrite`); reference only. Verified-NotFound orphan probe on
   the EXCLUSIVE arm only (`:27916-27938`, counted `writeback_orphan_discards`).
9. **Level (2)** — `acquire_write_lease_for_span(ino, offset, wlen)` → fencing
   token (`:27947-27950`).
10. Layout class from caches (`metadata_cache` → `attr_cache` (< 1 s) →
    `fetch_metadata`), `old_size`, `file_type` (`:27970-28004`).
11. Route: `fits_inline || fits_staged || inline || staged ⇒ router write`
    (materialize + `sever_payload` = one private copy, `:28120-28125`; the
    §5.4 lease-severance law); else STRIPED ⇒ `drop(guard)` BEFORE data I/O
    (KD-1, `:28232`) → `dispatch_striped_write` (`:28234-28244`).
12. A router route whose layout flipped striped under it (`LayoutStriped`)
    re-dispatches onto the ONE striped path (`:28190-28213` — the 2026-09-10
    data-loss fix: the retired router-side RMW ran guard-less and displaced the
    overlay's acked bytes).
13. Postlude STRICTLY after the data landed: RAM size floor bump (only
    `landed_striped && publish_expected > effective_old_size`, `:28271-28275`),
    `publish_attr(WriteTimes{size_claim})` (`:28283-28290`), `park_write_times`
    (`:28299-28309`), DIO coherence at the reply edge (`:28316-28320`), reply.
    SIZE NEVER LEADS DATA (generic/795, `:28085-28091`).
14. `Err(ENOSPC)` at the reply ⇒ `write_enospc_refusals` (`:28328-28332`).

### 1.2 `write_file_staged` — the whole-write screens (`:20818-20975`)

1. Fencing check: `fencing_token < current && !range_token_live ⇒
   FencingTokenExpired` (`:20828-20838`); the caller retries ONCE with a fresh
   lease and re-resolves `effective_old_size` (`:20789-20814`).
2. `start_block = offset / bs`, `end_block = (offset + len − 1) / bs` (`:20843-20844`).
3. **ENOSPC terminal law** (§8): `fresh_supply_exhausted() &&
   write_would_create_fresh_custody(ino, start..=end)` ⇒
   `write_fresh_block_enospc_refusals` + `StorageFull` BEFORE any byte is recorded,
   decided for the WHOLE write (`:20867-20881`).
4. `prev_write_end = note_last_write_end(ino, offset, len)` — the per-ino word is
   swapped UNCONDITIONALLY (predicate 6 needs the true previous end even when the
   knob is 0, `:20886-20891`; `:18287-18295`); `stream_adjacent = prev == offset`.
5. **W1 request-shape half** — `try_patch`, FIRST failing predicate wins, exactly
   one bucket per striped write (`:20896-20927`):
   - `patch_cap == 0 || payload.is_empty()` ⇒ false, ledger silent (the A/B lever);
   - predicate 6: `prev_write_end == Some(offset)` ⇒ `patch_ineligible_adjacent`;
   - predicate 5 size/window: `start_block != end_block || len > patch_cap ||
     offset + len > existing_size` ⇒ `patch_ineligible_oversize` (one bucket by
     design: spans blocks / exceeds the cap / EXTENDS — a grown i_size owes a meta
     commit);
   - predicate 5 alignment: `offset % 4096 != 0 || len % 4096 != 0` ⇒
     `patch_ineligible_unaligned`;
   - else `try_patch = true`.
6. **Overlay write-wide shape** (`:20942-20955`): `overlay_shape =
   device_overlay_enabled() && !empty && (slot || bytes_vehicle_armed()) &&
   !write_verification_enabled() && crypto.is_passthrough()`;
   `overlay_single_block = overlay_shape && start == end && offset % 4096 == 0 &&
   len % 4096 == 0 && overlay_length_eligible(len)`.
7. **Deferred slot**: `(try_patch || overlay_single_block) && slot` ⇒ the slot stays
   UNMATERIALIZED (the direct slot→device DMA candidates); every other shape
   materializes HERE once (memoized, `:20965-20971`).
8. One block future per piece; the single-block shape awaits inline (no `Vec`,
   no `try_join_all` — W-6, `:20973-20979`).

### 1.3 Per PIECE, under level (3) `BLOCK_FLUSH_LOCKS(ino, b)` — the ordered ladder

Acquire (3) first (`block_lock_acquire_timed(.., WriteCheckout)`, `:21030-21032`),
then, in THIS order (every exit drops the guard):

| # | Arm | Predicate (verbatim) | Counters | Exit |
|---|---|---|---|---|
| A | **W1 patch** | `try_patch` (above) ∧ `try_sole_owner_patch` state half (§4.1) | `patch_writes`, `patch_write_bytes` / one `patch_ineligible_*` bucket / `patch_dma_errors` | `Ok(true)` ACK; `Ok(false)` fall through; `Err` fails THIS write (`:21043-21067`) |
| B | **Device overlay** | `piece_overlay = overlay_shape ∧ write_start % 4096 == 0 ∧ slice_len % 4096 == 0 ∧ (start == end ∨ slice_len < bs)` (`:21082-21085`) ∧ `try_device_overlay_store` (§3.4) | `overlay_installs`, `overlay_stores`, `overlay_store_bytes`, `overlay_ineligible_*`, `overlay_sub_cap_joins`, `overlay_enospc_declines`, `overlay_unreachable_declines`, `overlay_claim_conflicts` | `Ok(true)` ACK (ACK-after-CQE or ACK-early); `Ok(false)` fall through; `Err` loud (`:21086-21111`) |
| C | **One-authority screen** | `any_open_fast()` ⇒ `settle_overlay_block_write_arm(ino, b)` (retry forever on transients; fence immediate); `Ok(true)` ⇒ the close-owed hint acted on DETACHED (`:21118-21165`) | `overlay_*` settle families | continue |
| D | **Live existing-bytes re-derivation** | `(live_mapped, live_size)` from the RAM map under the guard AFTER the settle; `needs_existing_data ||= live_mapped || block_write_needs_existing_data(live_existing_size…)` (`:21186-21204`; generic/551 — the stale request-entry size zeroed a sibling's acked bytes) | — | continue |
| E | **Deferred-slot materialize** | `deferred_slot` ⇒ `payload.materialize()` under the guard (`:21215-21230`) | — | continue |
| F | **W2 extent park** | `try_extent_park(ino, b, rel, slice, needs_existing_data, key, stream_adjacent, token)` (§6) | `extent_parks`, `extent_escalations`, `extent_record_absorbs` | `true` ⇒ ACK (`:21236-21258`) |
| G | **Accumulation checkout** | entry present ⇒ `write_block_revisits`; else seed class by `block_has_existing_bytes = live_mapped ∨ min(live_existing_size, b_end) > b_start`: `read_staged` ⇒ `seeded`; placed adoption ⇒ `adopted`; `!existing` ⇒ `fresh` (memset elided); else `deferred` (`overwrite_seed_deferred`) → `park_overlay_entry` PUBLISHED BEFORE this write's coverage is recorded (never invisible, `:21276-21360`) | `write_block_revisits`, `placed_adoptions`, `overwrite_seed_deferred` | continue |
| H | **Absorb older extent record** | `has_staged_extent_record(ext_key)` ⇒ `absorb_older_extent` into gaps + remove (`:21379-21402`) | `extent_record_absorbs` | continue |
| I | **Staged sibling supersede** | `has_staged_active_block(key)` (latch-free index) ⇒ `remove_active_block` on the blocking pool (`:21424-21471`) | `staging_sibling_probes`, `staging_sibling_hops_elided`, `restage_churn_{removes,bytes}` | continue |
| J | **Merge** | under a SHORT map guard: `record_write(rel, rel+len)` → `completed`; pointer-proof placed elision else `dma_copy(make_mut()[..], slice)` — THE one userspace copy (`:21483-21529`) | `placed_merge_elides`, `nt_copy_bytes`, `active_block_ooo_runs`, `active_block_cow_copies` | continue |
| K | **Write-through** | `coverage_completed` ⇒ `sync_inline()` arm (awaited) or `drop(guard)` → `write_pipeline.admit(bs)` → detached `pipeline_upload_parked_block` (`:21539-21618`) | `write_through_{blocks,bytes}`, `write_pipeline_*` | ACK |
| L | **Park** | partial ⇒ `admit_parked_active_block` (R5 pass) then drop (`:21619-21628`) | `parked_*` | ACK |

**Write admission ledger (charter identity).** The charter names
`admit_write_{overlay,inplace,accumulate,inline}_bytes` with
`Σ ≡ user bytes at ACK`. **R:** the reference has NO ledger under that name
(`grep admit_write_ src/` finds only `uring_fs::admit_write_batch`); its engagement is
split across `overlay_store_bytes` (CQE-counted, `:19420-19422`), `patch_write_bytes`
(`:18611-18613`), `nt_copy_bytes` (the merge copy, `:21522-21524`) +
`placed_merge_elides`, extent slab bytes (`parked_extent_bytes` deltas), and the
router's inline/staged commits — so the closure is testable only by assembling five
families. **PORT:** ONE `ShardedCounter` family `admit_write_{overlay,inplace,
accumulate,inline}_bytes` incremented at the ACK site of each arm (B/A/{F,G–L}/
router) with the identity `Σ ≡ user bytes at ACK` as a closure test; the reference's
per-mechanism engagement counters are kept as the arm-internal faces.

---

## 2. Each arm's protocol, step by step (lock levels, atomics, failure arms, numbers)

### 2.1 Arm A — W1 sole-owner extent patch (`try_sole_owner_patch :18340-18624`)

Under (3), after the request-shape half passed. State predicates in THIS order,
each a `patch_ineligible_*` bucket (the decision ledger reconciles against
invocations, `:18303-18317`):

1. clause 7: `patch_range_shared(ino, block_start, block_end, token)` ⇒ `Ok(false)`
   (`patch_ineligible_range_shared`, `block_allocator.rs:2026-2029`; inert on
   every shipped mount — whole-file lease).
2. predicate 2: `active_block_buffers.contains_key || has_staged_active_block ||
   has_staged_extent_record` ⇒ `patch_ineligible_overlay` (`:18369-18384`;
   lock-free probes — never the blocking-pool hop).
3. clause 8: `any_open_fast() && device_overlays.get(ino, b).is_some()` ⇒
   `patch_ineligible_device_overlay` (`:18398-18403`; KD-B4-4 — same guard, no new
   fence).
4. predicate 3: `!is_passthrough()` ⇒ `patch_ineligible_transform` (`:18406-18411`).
5. predicate 1: mapping from the AUTHORITATIVE cached map under the held lock
   (`:18423-18438`); hole/indirect ⇒ `patch_ineligible_unmapped`;
   `!is_whole_block_mapping(mapping)` ⇒ `patch_ineligible_decorated`
   (`:18447-18454`); `split_block_key` / `get_backend` failures ⇒ `unmapped`.
6. predicate 4 durable half FIRST on an armed set: `sole_owner_verdict(ino, alloc,
   off)` — `Shared ⇒ patch_ineligible_shared`, `ForeignCustody ⇒
   patch_ineligible_foreign_custody`, `NonHolder ⇒ patch_ineligible_posture`
   (`:18481-18510`; asked BEFORE the word is retired so a non-resident leaf read
   never sits inside the unstable window, `:18474-18480`).
7. **The fence pair, patch side**: `begin_patch_sole_owner(off)` =
   `mark_incarnation_unstable(off)` → `cross_word_fence()` (`fence(SeqCst)`) →
   `refcount(off) == Some(1) && !is_shared(off)` (`block_allocator.rs:1915-1922`);
   `false` ⇒ `publish_block(off)` re-stabilizes (content unchanged) +
   `patch_ineligible_shared` + CoW fallback (`:18511-18518`).
8. **ONE DMA** at `dev_offset + rel`, two vehicles one contract (`:18520-18590`):
   - D14 direct leg (slot ∧ !write_verification ∧ `zc_write_fd()`):
     `authorize_zc_store()` BEFORE submission (a fence refusal fails THIS write
     through the common exit — never a second submission path, `:18541-18549`);
     `z.store(fd, off+rel)`; `n == len` ⇒ `note_zc_write_direct`; else warn + fall to
     the pooled vehicle (a partial direct store is overwritten by the same
     window's pooled DMA — self-healing, `:18550-18565`).
   - pooled vehicle: `materialize()` → ONE copy into a 4 KiB-aligned `BUFFER_POOL`
     buffer → `device.write_block(off+rel, bytes)` (`:18570-18588`).
9. **Both exits**: `publish_block(off)`; `purge_block_key(mapping)` (the 4-tier law);
   `write_lru.remove`, `read_lru.remove` (`:18595-18599`).
10. Success: `note_fsync_touched_device(ino, &device)` (an in-place DMA changes no
    map key, so the publish-path stamp never sees it — stamp HERE or the next fsync
    skips its barrier, `:18602-18609`), `patch_writes`, `patch_write_bytes`, `Ok(true)`.
    Failure: `patch_dma_errors`, `Err(e)` — EIO for exactly this write, nothing
    acked/parked/lost (`:18616-18622`).

Zero allocation / free / map merge / journal entry / staging (`design-random-small-
writes.md:148`). `patch_edge_rmw_reads` exists and is incremented by NOTHING in v1
(`:198` — "must be 0 in v1"; the G-RW2 gate clause). Measured: rand-4k **354–397 →
61.5–66.7 k IOPS** (13.26×/15.04×/14.57× JFS; `design-random-small-writes.md:75,308`);
amplification 1.0× W / 0.0× R cargo tier, ≤ 3.2× W / ≤ 0.03× R live with
`patch_writes ≈ ops`, `patch_edge_rmw_reads = 0` (`:76`). W1 is un-disableable on a
production mount in the charter's sense: `SQUEEZEFS_PATCH_MAX_BYTES=0` is the A/B
lever only (`env_knobs.rs:176`).

### 2.2 Arm B — the device overlay store (`try_device_overlay_store :18867-19461`)

Under (3). Fully specified in §3.4; the protocol skeleton:

```
length floor (join an Open record below the cap, else decline)
→ §7 state screen (striped authority; RAM map present; overwrite ⇒ lever;
   no shadow binding for b; no RAM/staged/extent custody; range clause)
→ registry: join Open | steal-settle Frozen/terminal then decline | INSTALL:
   [port: reserve a record first — none free ⇒ decline, nothing minted;
   records = max(parked_cap, ⌈pipe ÷ B⌉), 0003 §AP]
   allocate_placed_block (reserve-before-DMA) → fsck guard + mint owner →
   [overwrite: ONE (3.5) section: capture old_binding ∧ re-check shadow ∧ install]
→ depth-governor permit (admit_segment(len, bs))
→ begin_store(first_page, pages) (claim + generation)  → Overlap ⇒ settle + decline
→ store: slot direct leg (authorize_zc_store → ACK-early | store(fd, dest))
         else pooled: materialize → overlay_owned_dma_bytes → write_block(dest, owned)
→ CQE: overlay_store_complete (permit release + lane sample) → complete_store_and_wake
→ Covered{coverage_complete} ⇒ freeze + detached drain_device_overlay_block
```

Measured: 4 MiB aligned overwrite ingest **31.0 → 32.1 GiB/s** (both A-B-B-A orders:
32.1 vs 31.0 / 31.6 vs 30.9) at HALF the daemon CPU (26.5–27.3 vs 52.7–53.1
jiffies/GiB), `nt_copy` merge share **1.000 → 0.001**, `rewrite_amp 1.0000`
(`env_knobs.rs:183`; `design-overlay-overwrite.md:1216`). Venue split recorded:
device-bound local zram-tcp prefers `=0` (1149–1161 vs 783–940 MiB/s at qd4).

### 2.3 Arm F — W2 extent park (`try_extent_park :18641-18765`) — §6.

### 2.4 Arms G–L — accumulation (`:21260-21628`) — §5.

### 2.5 The shared-mode convoy (reference only — ported)

The exclusive guard convoy measured **≈ 1.12 ms/op** of `write_lock_wait` at 32 ×
qd32 while data devices idled at qd ≈ 1–1.5 (`design-write-inode-convoy.md:17`;
`op_guard.zig` header). Shared ON vs OFF +7–8 % order-independent (`env_knobs.rs:220`).

---

## 3. The device overlay: record, state machine, read composition, fsync sequence

### 3.1 The record (`device_overlay.rs:315-343`; pure core `overlay_core.rs:183-216`)

| Field | Where | Role |
|---|---|---|
| `(ino, block)` | registry key, `scc::HashMap` (`:423`) | one live record per block; a second opener JOINS (the port: a fixed registry, 0003 §AP) |
| `core.old_binding: Option<String>` | `overlay_core.rs:215`, getter `:263` | `None` ⇔ fresh/hole (gaps zeros — law 5); `Some(mapping)` ⇔ OVERWRITE; captured in the SAME (3.5) section as the install (§5.1 one-section rule, `fuse_client.rs:19067-19084`); IMMUTABLE — no mutator exists |
| `be_id`, `dest_offset`, `device`, `allocator` | `:320-326` | the unpublished destination (durable `vol-` id, never a path) |
| `fence_token` | `:329` | the ino's DLM token at install; publication re-presents the CURRENT generation |
| `fsck_guard: Mutex<Option<InflightAllocGuard>>` | `:331` | fsck C2/C3 VISIBILITY ONLY — dropping it frees nothing (Rev 2 correction A) |
| `mint_owner: Mutex<Option<MintedBlockGuard>>` | `:333` | law 9's rollback owner; disposition per terminal state (§3.2) |
| `core.state: AtomicU8` | `overlay_core.rs:192` | `Open=0 → Frozen=1 → Published=2 \| Superseded=3 \| FenceDropped=4 \| Fed=5` (`:79-103`); unknown words decode to `FenceDropped` (free-NOTHING, `:112-116`) |
| `core.generation: AtomicU64` | `:196` | law 6: bumped by EVERY accepted segment (re-writes included); readers revalidate on EQUALITY |
| `core.inflight: AtomicUsize` | `:198` | stores submitted, CQE unseen — law 9's wait set |
| `core.claims: PlacedClaims` | `:201` | §2.3 range claims, one bit per `OVERLAY_PAGE = 4096` (`:73`), granted at submit, released ONLY at the CQE |
| `core.covered: Box<[AtomicU64]>` | `:204` | the lock-free published coverage face — set only by a full-success CQE |
| `core.union: Mutex<CoverageUnion>` | `:208` | the coverage LAW (shared with RAM accumulation, KD-OV-2); mutated under (3) |
| `core.birth` | `:190` | registry-monotone identity for `dest' == dest` (offsets are reused; births are not) |
| `retired`, `inflight_change: Notify` | `:336-342` | waiters on retire / on the in-flight set changing (the settle's event wait — the retired `yield_now` spin self-woke forever on the queue worker's lane, 2026-08-13) |

Volatile: RAM only, no on-disk form, no incompat bit, no journal record
(`device_overlay.rs:36-41`). Gauges `overlay_open` (`+1` at install `:509-511`, `−1` at
retire `:525`), `overlay_installs`.

### 3.2 The pure state core (`overlay_core.rs`) — transitions and the Dekker pair

- `begin_store(first_page, pages) → Result<StoreTicket, StoreRefusal>` (`:288-335`):
  `OutOfRange` / `NotOpen` / `Overlap` (`claims.begin_claim` refused); then
  `claims.end_write()` (the CPU-memcpy window closes immediately — the overlay's
  window is submit→CQE carried by the claim BITS), `inflight.fetch_add(1, SeqCst)`,
  **`fence(SeqCst)`**, re-check `state == Open` else back out (`fetch_sub`,
  `claims.release`, `NotOpen`), `generation.fetch_add(1, SeqCst) + 1` → ticket.
- `complete_store(ticket, success) → CompleteVerdict` (`:342-373`): terminal ⇒
  `Superseded` (publish nothing); success ⇒ `union.record(start, end, len)` under
  the lock, then `set_covered_bits` (SeqCst `fetch_or`) ⇒ `Covered{coverage_complete}`;
  failure ⇒ `NotCovered`. The claim releases ONLY here (success or failure);
  `inflight.fetch_sub(1, SeqCst)`.
- `covered_probe(first, pages)` (`:397-425`): claims first (an in-flight range is
  never served, even a re-write of a covered range), then every covered bit.
- `read_begin() = generation.load(SeqCst)`; `read_valid(snap) = generation == snap`
  (EQUALITY, not ≥, `:461-470`); `range_inflight`, `covered_runs` (`:428-452`).
- `freeze(): Open → Frozen` CAS (`:473-482`); `supersede()` / `fence_drop()`:
  `Open|Frozen → Superseded|FenceDropped` (`:485-506`); `mark_published(): Frozen →
  Published` (`:510-519`); `mark_fed(): Frozen → Fed` (`:527-536`).
- `inflight_empty()` = **`fence(SeqCst)`** then `inflight.load(SeqCst) == 0` (`:546-549`).
- `rollback_admissible() = state.is_terminal() && inflight_empty()` (`:584-586`).

**The Dekker pair (store buffering), named** (`:307-321`, `:538-545`): the writer
publishes `inflight += 1` THEN checks `state`; the freezer publishes `state =
Frozen` THEN checks `inflight`. Each side's `fence(SeqCst)` between its store and
its cross-word load is LOAD-BEARING: remove either and the SB interleaving admits a
store that neither backed out on the freeze nor was visible to the freezer's drain.
Model: `overlay_freeze_drain_observes_final_coverage` (`loom-models/src/lib.rs:4725-
4817`) — asserts `stored == covered_at_drain` whenever the freezer observed
`inflight_empty()`; weakening-verified at landing (`:4650-4653`). The read-side
model `overlay_validated_serve_is_never_torn` (`:4669-4712`): a reader passing
`covered_probe` ∧ generation-equality revalidation never observes a mix of two
passes' bytes (the "device" is two Relaxed words — the protocol words carry ALL
the exclusion).

**Teardown disposition** (`device_overlay.rs:367-418`, exhaustive match, no
catch-all): `Published` ⇒ `mint.disarm()`; `Fed` ⇒ disarm WITHOUT freeing (the dest
is the epoch's pending B key — freeing it is the KD-1.11 corruption); `FenceDropped`
⇒ disarm without freeing (W5 — successor recovery owns the accounting);
`Superseded` ⇒ the armed guard's drop FREES; `Open|Frozen` ⇒ tripwire
`overlay_teardown_nonterminal` + disarm. Then `mint = None`, `fsck_guard = None`.

### 3.3 Law 6's mandatory clause — in-flight overlap exclusion

The four-step stale-DMA counterexample (`design-device-overlay.md:179-186`): A (gen
g) over R in flight; B (gen g+1) overlapping R submits; B's DMA lands first and
publishes; A's DMA lands LAST ⇒ the device holds A's OLDER bytes under R while
coverage and generation both say B. Generation filtering orders PUBLICATION; it
cannot undo physical writes. Therefore two overlapping stores may never be in
flight concurrently: range claims held across the DMA, released only at the CQE;
disjoint ranges run concurrently (the 4×1 MiB kernel-split cohort stays parallel);
whole-block-lock-across-DMA REJECTED (it serializes the cohort and a lock across a
fabric RTT per segment is the serialization the pipeline restructure deleted,
`:212-220`).

### 3.4 Install / store / CQE wiring (`try_device_overlay_store :18867-19461`)

1. **Length floor** (finding 47; `overlay_length_eligible(len) = cap == 0 || len >
   patch_max_bytes()`, `fuse_client.rs:623-626`): below the floor the piece
   DECLINES (`overlay_ineligible_sub_cap`) UNLESS it joins a record already `Open`
   on the block (`overlay_sub_cap_joins`, `:18896-18911`) — the tail piece of a
   boundary-straddling write on a misaligned writeback grid must not settle the
   record half-filled and start a buffer whose coverage never completes (§4.4ci).
   WHY the floor: six 4 KiB writes minted six CoW dests and paid
   `overlay_gap_seed_old_bytes = 6 × 4 MiB − 24,576` (`:613-616`).
2. **State screen** (`:18918-18987`): `metadata_cache.get(ino)` present;
   `file_type == "striped"` (§7.1: AFTER promotion drained AND published —
   `file_type` IS the durable flip witness); `block_map` present (indirect ⇒
   decline); `overwrite_shape = bm.contains_key(b)` and the overwrite lever;
   `rewrite_epoch_binds_block(ino, b)` ⇒ decline (`overlay_ineligible_shadow_bound`
   on the overwrite shape — hazard 1); no RAM/staged/extent custody;
   `overlay_range_shared(…)` ⇒ decline (`overlay_ineligible_range_shared`,
   `device_overlay.rs:189-204`).
3. **Registry** (`:18990-19164`): `Some(Open)` ⇒ join; `Some(Frozen|terminal)` under
   OUR guard ⇒ steal the detached publisher's work (`settle_overlay_block_write_arm`)
   then decline; `None` ⇒ INSTALL: `allocate_placed_block()` (law 2 reserve-before-
   DMA; `StorageFull` ⇒ `overlay_enospc_declines` + decline to accumulation — KD-B4-8
   both shapes; `Retryable` ⇒ `overlay_unreachable_declines` + decline);
   `inflight_register(dest)` (fsck guard) + `MintedBlockGuard::new` (mint owner);
   slot payload on a device without `zc_write_fd()` ⇒ decline (the mint guard's drop
   gives the offset back); OVERWRITE: `meta_lock_acquire(ino)` ((3.5) under (3) —
   the extended order), re-derive `old_binding` from the RAM map under the section
   (vanished ⇒ decline), re-check `rewrite_epoch_binds_block`, test seam
   `SQUEEZEFS_TEST_OVERLAY_CAPTURE_STALL_MS`, `install(…, Some(old_binding), …)`
   (`overlay_overwrite_installs`); fresh ⇒ `install(…, None, …)` lock-free; an
   install race under the guard ⇒ tripwire `overlay_install_race_under_guard`.
4. **Governor permit** (W-3, `:19180-19197`): `depth_governor_enabled()` ⇒
   `write_pipeline.admit_segment(len, bs)`, `AdmitWait` phase sample,
   `overlay_governed_stores`. Taken AFTER the registry (no 3.5 held) and BEFORE the
   claim (a parked writer holds no claim a settle would await).
5. **Claim + accept** (`:19200-19231`): `begin_store(rel / 4096, len.div_ceil(4096))`;
   `Err` (an overlapping claim still in flight — the ACK-early rewrite-while-in-
   flight shape) ⇒ `overlay_claim_conflicts`, `drop(admission)` (return governor
   custody before parking), settle, decline. `overlay_inflight_bytes += len`.
6. **Store** (`:19237-19411`): slot ∧ `zc_write_fd` ⇒ `authorize_zc_store()` (refusal
   ⇒ `complete_store_and_wake(ticket, false)`, gauge release, `Err` loud) →
   ACK-early arms (§3.6) → else `z.store(fd, dest)`; `n == len` ⇒
   `note_zc_write_direct`; else `overlay_short_stores` + fallback. Pooled vehicle:
   `materialize()` → `overlay_owned_dma_bytes(bytes)` (4 KiB-aligned ptr ∧ len ⇒
   pass through, `overlay_dma_passthrough_bytes`; else ONE pool copy,
   `overlay_dma_pool_copy_bytes`, `device_overlay.rs:539-557`) → ACK-early-bytes arm
   or `device.write_block(dest, owned)`; failure ⇒ `complete_store_and_wake(false)`
   + `Err` (law 3: never joins `completed`); `was_fallback ⇒ overlay_store_fallbacks`.
7. **CQE** (`:19413-19460`): `overlay_store_complete(admission, &rec, len, true)`
   FIRST (permit release + `record_completion_sized(be_id, len, bs, dur)` — never
   waits on a block guard); `complete_store_and_wake(ticket, true)`; gauge release;
   `overlay_stores`, `overlay_store_bytes`, overwrite subset `overlay_overwrite_bytes`;
   `Covered{coverage_complete: true}` ⇒ `rec.core.freeze()` under THIS guard +
   detached `drain_device_overlay_block(ino, b, false)`; `Superseded` under the
   guard ⇒ tripwire `overlay_store_superseded_under_guard`.

### 3.5 Read composition (`try_serve_overlay_read :20550-20713`) — the §5.2 protocol

Probe: `size == 0 || !any_open_fast()` ⇒ none (one relaxed gauge load — every mount
with no live overlay pays nothing); single-block only; `device_overlays.get(ino, b)`
in `Open|Frozen` (`:20556-20573`). EOF clamp from the max of both size caches —
they publish at ACK so they lag truth and never lead it; no size authority ⇒ decline
to the drain path (`:20574-20590`). Then the loop (`:20600-20712`):

```
1. snap ← read_begin()
2. await_overlay_pages(rec, first_page, pages)   (spin-yield on range_inflight;
                                                  overlay_teardown_waits at 1000 spins)
   terminal ⇒ None
3. for (ps, pc) in covered_runs(first_page, pages):  device.read_block(dest + from, n)
   gaps collected between runs
4. gaps: old_binding Some ⇒ read_nvme_block_old_image(old_key) (short-tolerant: the
   missing tail stays zeros); fetch failure ⇒ None (decline to drain, never EIO);
   fresh ⇒ zeros (law 5)
5. REVALIDATE — terminal re-check AFTER every device fetch (load-bearing: the feed
   retires the record and the close FREES the displaced old key whose offset
   recycles; every free is ordered strictly after the SeqCst terminal CAS, so a
   serve that observes ¬terminal here cannot have fetched post-free bytes)
   ∧ read_valid(snap) ∧ !range_inflight ⇒ serve
   (overlay_read_serves, overlay_read_serve_bytes, overlay_read_gap_{serves,bytes})
   else loop
```

Fallback: `drain_device_overlays_range` (freeze/complete/seed/publish per touched
block, `overlay_read_drains`, `:20716-20735`). A read window whose post-read
validation fails past the bounded lock-free retries re-serves per block under (3) +
(3.5) — `overlay_window_escalations` (`:23709`, `:34353`). **The failed-direct-read
law** (`design-device-overlay.md:361-370`): any read that landed bytes in a caller-
visible dest and fails revalidation rewrites the WHOLE window — never a byte-diff
repair. Tier precedence (KD-OV-13, `:380-408`): old-binding tier entries are
PRESERVED while open (the gap source), purged only at durable displacement; a fast
path that cannot run the precedence check DEMOTES.

### 3.6 ACK-early (`:19269-19341`, continuations `:19501-19741`)

`ack_early_enabled()` ∧ `z.ack_early_sound() ∧ z.commit_retain()` ⇒ detached
`finish_ack_early_store` (page-cache folios: 0-copy, needs kernel 0029) and
`Ok(true)` immediately; `!sound ∧ ack_early_odirect()` ⇒ SNAPSHOT (`materialize`)
then ACK then DMA the snapshot (NFS UNSTABLE samples at ACK — the 2026-08-09
dd `oflag=direct` aliasing); pooled bytes ⇒ `finish_ack_early_bytes` (daemon-owned
bytes are always sound). The continuation: retry-forever with backoff
`min(10 ms << min(attempt, 7), 1 s)` (`:19604-19607`), `overlay_ack_early_retries`,
warn at power-of-two attempts; the fence latch (`authorize_zc_store().is_err()`) is
the ONE legal drop ⇒ `overlay_fence_drops`; a dead ring (`Unsupported`) falls back
to `write_block` with the ACK-time bytes, else `overlay_ack_early_lost` loud
(`:19539-19578`); coverage publishes at the REAL CQE; `z.release_payload()` exactly
once, last (`:19655`). **The future law** stated now (`design-device-overlay.md:
624-636`): a read intersecting an ACKed-but-incomplete store waits for the store
(`await_overlay_pages`) — never an uncovered old-map gap.

### 3.7 The settle (`settle_overlay_block_locked :19862-20269`) — FREEZE → COMPLETE → SEED → FLUSH → PUBLISH

Under (3). `Open ⇒ freeze()`; `Frozen` ⇒ continue; terminal ⇒ teardown + `Ok(false)`
(`:19869-19880`). Test seam `set_test_settle_transient_failures` after the freeze,
before device work (`:19882-19901`). Then:

1. **COMPLETE**: `await_overlay_inflight(&rec)` — register-then-recheck on
   `inflight_change` (`:3314-3354`; `overlay_teardown_waits` once per wait; a 30 s
   bark naming the leaked ticket's claim words — never a hang).
2. **SEED GAPS** (`:19911-20028`): `gaps = union.gaps()`; `app_complete = gaps.is_empty()`
   (a fully-covered overlay pays nothing). Overwrite: the DEAD-CAPTURE belt —
   `!block_key_incarnation_ok(old_key)` ⇒ `overlay_superseded_dead_old_binding`,
   `supersede()`, teardown, `Ok(false)` (finding 51: retrying a dead key ran every
   50 ms for nine minutes, 28,039 refusals). `seed_ranged = old_binding ∧ gaps ∧
   gap_seed_ranged_enabled() ∧ old_image_ranged_eligible(old_key) ∧ Σ gaps <
   device_block_window()`: per gap ONE ranged read of exactly its bytes
   (`read_nvme_block_old_image_range`, `overlay_gap_seed_ranged_bytes`); else the
   whole old image once (`overlay_gap_seed_read_bytes`); fresh ⇒ zeros. Each gap:
   pooled buffer, `write_block(dest + gs, buf)`, `overlay_gap_seeds`,
   `overlay_gap_seed_bytes` (+ `overlay_gap_seed_old_bytes` on the old arm).
3. **FLUSH**: `barrier ⇒ rec.device.flush()` (`:20031-20033`; fsync class only).
4. **PUBLISH**: `rec.allocator.publish_block(dest)`; `new_key = persist_block_key`;
   `purge_block_key(new_key)` (recycled-key hygiene); `min_size = app_complete ?
   (b+1)·bs : 0` (seeded zeros were never app-written — SIZE NEVER LEADS DATA,
   `:20046-20053`); `covered_app_bytes = union covered bytes` (§8.2 user-byte term).
   - **Arm (a) FEED** (`old_binding ∧ rewrite_shadow_enabled()`, `:20059-20137`):
     take the `fsck_guard` OUT of the record (continuous C2/C3 visibility — record →
     epoch), `rewrite_shadow_record(ino, b, new_key, min_size, guard)`;
     `Shadowed{displaced_prev, coverage_complete}` ⇒ SLO attribution
     (`rewrite_blocks`, `rewrite_user_bytes += covered_app_bytes`,
     `rewrite_device_write_bytes += bs`), `mark_fed()` (a guard-less discarding belt
     superseding the record post-freeze ⇒ `mint.disarm()` — never free the epoch's
     live B key), `overlay_epoch_feeds`, teardown, `Ok(coverage_complete)` — the
     close-owed HINT; `NotShadowed(guard)` ⇒ put the guard back, fall through.
   - **Durable-merge arm** (fresh records + the shadow-off degenerate, `:20148-
     20268`): `OverlaySelfPublish{block, dest_offset, birth}` provenance marker
     (KD-B4-11 — the record is still Frozen+registered while its own merge runs;
     retiring it first opens the record-gone-⇒-published generic/209 window);
     `token = current.max(rec.fence_token)`; `merge_block_mappings_coalesced_marked
     (ino, [(b, new_key)], min_size, ToStripedKeepStagedIdentity, token, marker)` —
     `FencingTokenExpired ⇒ fencing_retry_token` convergence; `WriterGuardFenced ⇒
     overlay_fence_drops, fence_drop(), teardown, Err` (W5: publish nothing, free
     nothing); verified-NotFound ⇒ `writeback_orphan_discards`, `supersede()`,
     teardown, `Ok(false)` (the ino never comes back — monotone inos); other ⇒ `Err`,
     record stays FROZEN never-lossy. `!mark_published() ⇒ mint.disarm()`;
     teardown; `overlay_publishes`, `overlay_published_bytes += bs`;
     `old_binding ⇒ overlay_feed_fallbacks` (growth with the shadow lever ON is a
     bug); displaced ⇒ rewrite SLO; **free displaced keys only AFTER** the publish
     and the retire (`:20261-20267`).

The write-arm form retries forever on transients with backoff `min(1 ms <<
min(attempt, 6), 50 ms)`, fence immediate (`settle_overlay_block_write_arm
:19834-19860`). Ours polls the volume's FAILED latch (the plane's fence probe:
the meta set's `is_failed` or a fenced device) on every transient and takes
the fence arm when it is set — a transient on a FAILED volume never clears
before the remount, and the daemon's teardown joins the settler (xfstests
generic/475 with dm-error under the meta AND data slices: the gap seed's EIO
retried past attempt 1024, the umount waited forever). The read/drain-venue form `SETTLE_RETRIES = 24` with `1 ms << min
(attempt−1, 3)` backoff (`settle_overlay_block_converged :19786-19817`; the round-4
budget of 4 "was a constant with no lineage"). **Epoch-close venue law**: the settle
NEVER calls `close_rewrite_epoch` under the block guard — the guard-DROPPING caller
acts on the hint (`drain_device_overlay_block :20298-20337`: settle →
`drop(block_guard)` → `close_rewrite_epoch`); the hint is latency/economy, never
correctness (the 5 s idle sweeper + the next fsync are the backstops).

### 3.8 The fsync sequence and the two non-fsync boundaries

`flush_inode_to_backend_prof` (`:23251-23269`): `any_open_fast()` ⇒
`drain_device_overlays_for_ino(ino, u64::MAX, barrier = true)` BEFORE the ordinary
flush machinery; any record left ⇒ `overlay_unpublished_at_fsync` (counted, not
assumed). `drain_device_overlays_for_ino` (`:20344-20380`): per block, under (3),
blocks at or past `keep_below` SUPERSEDE (shape-change ops own the discarded data)
or drain a Frozen one (never strand a frozen record); below ⇒ drain with the barrier.
RELEASE of a dirty handle runs FREEZE/COMPLETE/SEED + PUBLISH without the data
barrier (write-back class); the unmount drain is followed by a data barrier and
`close_open_rewrite_epochs` (`design-device-overlay.md:458-475`; `routing.rs:16972`).
Must-stay-0: `overlay_fence_drops`, `overlay_unpublished_at_fsync`,
`overlay_teardown_waits` timing out (`design-device-overlay.md:657`).

### 3.9 Crash windows (transfer of design-rewrite-program §5.6)

OW-1 ACK, DMA in flight ⇒ dest free-listed by the census, old image intact; OW-2
fed, no save ⇒ ≡ OW-1; OW-3 intermediate save ⇒ ≡ W6; OW-4 torn publish ⇒ ≡ OW-1;
OW-5 swap durable, frees not run ⇒ recovery free-lists; OW-6 fenced pre-publish ⇒
W5; OW-7 mid-fsync between barrier and meta publish ⇒ ≡ OW-1 (durably written ≠
durably named); OW-8 POWER LOSS after an UNBARRIERED non-fsync save on a write-back
namespace ⇒ dest residue — the inherited DUR-2 class; `SQUEEZEFS_OVERLAY_CLOSE_
BARRIER` (default OFF) is the stronger posture (`design-overlay-overwrite.md:
391-402`; pinned `ow8_window_is_exactly_as_disclosed`).

---

## 4. The W1 patch/clone fence pair, its model and weakening

### 4.1 The two sides (`patch_clone_core.rs:1-59`)

- **patch**: `incarnation_core::retire(word)` (store W: `mark_incarnation_unstable`)
  → `cross_word_fence()` = `fence(SeqCst)` → `refcount_core::peek(rc)` (load R);
  proceed in place only on `refcount == 1`; else `publish` re-stabilizes (content
  unchanged) and the write falls back to CoW (`:9-14`; `block_allocator.rs:1915-1922`).
- **clone**: `refcount_core::try_acquire(rc)` (store R, the pin CAS) →
  `cross_word_fence()` → `incarnation_core::snapshot(word)` (load W); unstable ⇒
  unpin, refetch the map under (3.5), retry — bounded by the existing `attempt >= 3`
  loud refusal (`:15-19`; `pin_block_validated :2047-2063`, the fence BEFORE the word
  lookup so the absent-word arm is fenced too).

### 4.2 Why SeqCst (`:21-33`)

Each side is a store on one word then a load of the OTHER word — store buffering.
Release/Acquire (including `retire`'s trailing Release fence and `snapshot`'s Acquire
load) does not forbid both sides reading the old value (patch sees refcount 1 AND
clone sees stable) on ARM/POWER; x86/TSO masks it. With both SeqCst fences the fence
total order guarantees at least one side observes the other: the patch backs off
(refcount 2) or the clone retries (unstable) — never both proceeding.

### 4.3 The composed model (`loom-models/src/lib.rs:667-740`)

`patch_clone_composed_never_mutates_a_validated_pin`: word = `STABLE_FIRST`, rc = 1;
patcher thread `retire → cross_word_fence → peek == 1 → publish` (both exits);
main thread `try_acquire → cross_word_fence → snapshot`. THE invariant at generation
precision: `¬(pinned ∧ snap == Some(STABLE_FIRST) ∧ patched)` (`:715-720`) — a pin
validated against the POST-publish word (gen 1) is the legitimate "a clone racing a
write may see old or new" arm. Progress: an unvalidated pin's retry snapshot sees
stability post-join (`:725-731`); refcount conserved (`:733-738`). **Weakening**:
either fence to Release/Acquire fails the model (`:664-665`, "verified during
development"). Per-protocol models structurally cannot see SB across protocols
(`:658-663`) — the composition is what gets the model. Companion:
`incarnation_validated_fill_never_serves_mid_patch_bytes` (`:749-783`) — a validated
fill racing retire → mutate → publish never publishes mid-patch bytes.

**PORT:** `src/model/` gets `patch_clone_model.zig` as a two-thread value model with
explicit per-thread store buffers (the fence step drains them); `expectHolds` on the
composed invariant; `expectFails` with the drain step removed. Zig 0.17 has no
standalone fence (`slot.zig:29-39`): the four accesses are themselves SeqCst (C11's
single total order over seq_cst operations is the Dekker guarantee; ARMv8 `stlr`/
`ldar` RCsc; x86 locked `xchg`), the same discipline the lease gate landed with.

---

## 5. Accumulation: CoW pool, coverage union, write-through, rewrite epoch, transforms

### 5.1 The exclusive-owner CoW 4 MiB buffer (`cache/active_block.rs`, `cow_core.rs`)

- Backing: `AlignedBlock` from `ALIGNED_BUF_POOL` (`:45`, `:81-99`) — block-sized,
  4096-aligned (`POOLED_BUF_ALIGN = 4096`, `pool.rs:28`), pool capacity
  `max(cores × 16, 64)` (`pool.rs:319-321`, the fleet-share-DIVIDED root — a pinned
  first toucher once saw ONE cpu and collapsed the pool to its floor); oversized
  blocks fall to a dedicated `alloc_zeroed` layout (`:88-98`). Pool memory is zeroed
  at OS-allocation birth and only written through safe slices, so an uncovered buffer
  holds STALE BUT INITIALIZED bytes — sound to view, private by coverage (`:40-43`).
- `CowCell<T>` (`cow_core.rs:43-93`): `share()` = `Arc::clone` (immutable forever);
  `owned_mut(duplicate)` = `Arc::get_mut` (Acquire, pairs with `Arc::drop`'s Release)
  or copy-on-write into a fresh never-shared Arc (`active_block_cow_copies`);
  `adopt(shared)` for placed-sever assemblies (starts non-unique). Models
  `cow_snapshot_never_changes_under_racing_writer` (`loom:824-857`, with
  `UnsafeCell` access tracking) and `cow_uniqueness_restored_after_remote_snapshot_
  drop` (`:864-880`).
- Birth classes (`:372-456`): `fresh(bs)` — uncovered, memset ELIDED (the complement
  is zeros by definition, served as zeros by `covered_runs_in`, zeroed lazily by
  `zero_complete`); `deferred(bs)` — item B: existing device data, seed read
  postponed (full coverage in ANY order skips it forever — `overwrite_seed_skipped`;
  exits materialize via `fill_complement_from` — `overwrite_seed_materialized`);
  `adopted(shared, deferred)`; `seeded(existing, bs)` — content-valid at birth,
  written union EMPTY (a lone partial overwrite parks, `:675-699`); `extent(bs,
  deferred)` (§6).
- Invariants (`:291-322`): `content_valid` (every byte correct); `deferred_seed ⇒
  !content_valid`; `write_epoch` — a PROCESS-GLOBALLY-UNIQUE stamp advanced by every
  `record_write` under (3) (`next_write_epoch`, Relaxed `fetch_add` on one shared
  counter — uniqueness is the only contract; ordering rides the block lock; global
  uniqueness closes the retire→re-park ABA, `:325-332`).
- `zero_complete()` (`:823-859`): on a deferred-seed buffer ⇒ tripwire
  `zero_complete_on_deferred_seed` (report, never panic — the exit still establishes
  validity); zero EVERY gap, `coverage.set_full`, `content_valid = true`,
  `active_block_memset_elided_bytes += len − zeroed`. Every stage/upload exit calls
  it under (3) (`:31-35`).
- `snapshot()` = `Bytes::from_owner(SnapshotOwner(cell.share()))` (`:898-900`) —
  readers pair it with `covered_runs_in` under the same entry guard (gap bytes are
  served as ZEROS, never buffer bytes).
- Gauges: `parked_full_buffer_bytes` (Full), `parked_extent_bytes` (Extent) — RAII at
  construction/drop/escalation (`:334-357`); the W2 parked-write BYTE budget.

### 5.2 The coverage union core (`coverage_core.rs`) — ONE law for RAM and device

`CoverageUnion { primary: (u32, u32), extra: Vec<(u32, u32)> }` (`:50-56`): the
primary run is the only state an in-order stream touches (no allocation); extras are
sorted, pairwise disjoint and non-abutting, disjoint from the primary.
`record(start, end, len) → RecordVerdict{completed, out_of_order, completed_with_
extras}` (`:72-114`): empty range ⇒ nothing; already full ⇒ NO transition (a re-write
must not double-fire); first touch ⇒ primary; overlaps/abuts ⇒ extend + `coalesce_
extras_into_primary` (one retain pass suffices — extras are pairwise non-abutting,
`:177-192`); disjoint ⇒ `insert_extra_run` (`:194-205`), `out_of_order`. `is_full(len)
= primary == (0, len)` (`:118-120`). `set_full` (`:135-138`) consumes any pending
transition. `gaps`, `runs_sorted`, `contains`, `run_count` (`:142-175`).

The write-through trigger is `union == whole block` (RW3b, FIND-L1-A): the end-offset
proxy `write_end == b_end_offset` fired on partial coverage and missed true
completion under kernel-split `FOPEN_PARALLEL_DIRECT_WRITES` segments — measured
**−34 % at t16** (`design-zero-copy-write-path.md:205`); `FOPEN_PARALLEL_DIRECT_WRITES`
stays on. Tripwires: `complete_union_with_extra_runs` (`active_block.rs:868-873`),
`extent_overlay_completed_coverage`, `small_first_write_completed_block`
(`fuse_client.rs:18671-18676`, `:18740-18744`). Dependency-free so the loom crate
`#[path]`-includes it (`:24-26`); **PORT:** `src/core/coverage.zig` as a plain value
type with the exact `RecordVerdict` contract and a tie test per verdict; the overlay
record and the accumulation buffer both import it.

### 5.3 Write-through with `write_epoch` revalidate and latest-wins (`pipeline_upload_parked_block :21845-22039`)

```
loop:
  token ← current DLM generation (read BEFORE the lock — the fence test's hold point)
  (3) lock: snap ← entry present ∧ !extent ∧ content_valid ∧ !seed_deferred
            ? (snapshot(), write_epoch()) : return (retired / not ready)   ; unlock
  [seam SQUEEZEFS_TEST_UPLOAD_STALL_MS]
  UNLOCKED device phase: process_write_async(snapshot) (crypto; failure ⇒ serialized path)
    → upload_block_dma_phase(processed, permit.auth()):
        ensure_stored_block_image_fits; allocate_placed_block (incarnation UNSTABLE
        at allocate — racing validated fills fail their seqlock re-check);
        inflight_register; write_block_authorized(offset, processed, auth) (the S7
        custody epoch captured at ADMISSION — the permit is the carrier);
        publish_block(offset); purge_block_key(new_key)              (:22911-22983)
    StorageFull ⇒ close_rewrite_epoch(ino, token) succeeded ⇒ rewrite_shadow_fallbacks
                 + continue (KD-1.7 ENOSPC early-close); else serialized path
  (3) lock: current_epoch ≠ snapshot epoch ⇒ SUPERSEDED. The reference abandoned the
            offset and re-drove (:21946-21974); the port (decision 0003 §AJ.7) PUBLISHES
            the snapshot (one consistent image at its stamp), keeps the entry parked with
            the pages written since over the published key, and re-drives only for writes
            its caller owes (`write_pipeline_superseded_publishes`)
  current ⇒ upload_block_publish_phase(…) under the held guard (merge / shadow record),
            retire_parked_overlay, write_through_{blocks,bytes}, upload_invalidation_tail
            UNDER the guard (the 2026-08-04 crash: the tail's second retire raced a new
            same-block writer's fresh entry), drop guard, epoch_coverage ⇒
            close_rewrite_epoch OUTSIDE the guard (RES-1)                (:21976-22029)
```

Stamp and revalidation both run under (3) ⇒ lock-serialized by design, no fence
protocol, no model owed (`design-rewrite-program.md:280-287`). The in-place lever
(`SQUEEZEFS_INPLACE_OVERWRITE`, default OFF — measured −20 % on zram-lz4,
`fuse_client.rs:628-644`) keeps the serialized under-lock upload (KD-2.3).
`write_through_complete_block` (`:21699-21828`) is the serialized unit: fencing
convergence via `fencing_retry_token` (monotone — a failed merge proves the generation
advanced, `:21672-21680`); never-lossy fallback `put_active_block` + `enqueue_writeback`
(`write_through_fallbacks`, `staging_put_bytes_wt_fallback`,
`writeback_enqueued_wt_fallback`); staging refused ⇒ the block STAYS PARKED and rides
the R5 pass (`:21812-21823`). Dispositions (`write_pipeline.rs:220-252`):
`FencingTokenExpired ⇒ StayParked` (a process-local rotation converges inside);
`WriterGuardFenced ⇒ FenceDrop` (`write_pipeline_fence_drops`); other ⇒ `StayParked`.

**Port (decision 0003 §AK.1, 0002 row 534): the in-place drain.** The flush of a PARTIAL
parked buffer (`flushParkedBlock`: RELEASE / the no-open FLUSH, fsync's DUR-1, the gate's
cold flush) whose complement is still the device's (`seed == deferred`) over a block W1's
state half passes for (`beginInPlace`) lands the covered runs widened to pages in place —
each partial page's other bytes read back from the mapping into the cell (≤ 2 per run),
ONE DMA per contiguous page run, the W1 postlude, the entry retired; no seed of the whole
block, no fresh block, no merge, no epoch. `drain_inplace_{drains,pages,dmas,write_bytes,
seed_bytes,declines,dma_errors}`; `drain_inplace_write_bytes` is a device-write byte arm.
An extending append's drain costs `pages(u, 1) × P` where the reference's whole-block
escalation wrote `B` and read `B`. A CONTENT-VALID buffer over the binding its clean pages
equal (`clean_base`) drains its dirty pages the same way (§AJ.7, `drain_inplace_dirty_drains`
inside `drain_inplace_drains`): `pages(dirty) × P` per pass. `write_through_bytes` counts the
DMA'd (stored) length — a superseded snapshot's publish included (§AK.6, §AJ.7).

### 5.4 The rewrite epoch (`routing.rs:1592-1608`, `:16407-16537`, `:16726-16884`)

Per ino, all mutation under (3.5): `shadow: scc::HashMap<u32, String>` (b → newest
B key — the refetch-compose source, KD-1.9), `displaced: SegQueue<String>` (parked A
keys), `guards: SegQueue<InflightAllocGuard>` (B offsets' live-owner registrations —
THE fsck C2/C3 exemption hook), `recorded_bytes`, `parked_bytes`, `last_record_ms`.
Gauges ride construction/Drop (`rewrite_shadow_open_epochs`, `rewrite_shadow_parked_
bytes`, `:1611-1638`).

`rewrite_shadow_record(ino, b, new_key, min_size, guard)` (`:16407-16537`): under
(3.5): RMW base = dirty RAM entry else backend fetch (failure ⇒ `NotShadowed(guard)`);
PARTIAL entries decline; open trigger (KD-1.1) = `striped ∧ prev ≠ new_key`
(`:16442-16446`); CoW map mutation (`Arc::make_mut`); displaced prev ⇒
`purge_block_key(p)`, `displaced.push(p)`, `parked_bytes += bs`; size floor; `layout_dirty
= true`; `note_block_ref_ops(ino, [−prev, +new])` so the save that persists the map
drains the ref delta into the SAME tx (`:16498-16515`); `shadow[b] = new_key`;
`guards.push(guard)`; `coverage_complete = shadow.len() × bs ≥ size`.

`close_rewrite_epoch_with(ino, token, Park|Try)` (`:16726-16884`): absent ⇒ `Absent`;
(3.5) Park (every trigger site) or Try (the pressure close under a block guard ⇒
`Busy`, `rewrite_shadow_pressure_close_busy`); resolve the entry WHILE registered
(a miss refetch-composes the shadow); remove the epoch; dirty ⇒ ONE
`save_metadata_to_backend_ext` with `FencingTokenExpired` convergence
(`rewrite_shadow_close_retries` — a process-local rotation, NEVER W5, `:16782-16799`);
clean ⇒ a covering save already ran. Success: COLLECT displaced keys under the lock,
pop every B guard, `rewrite_shadow_swaps`, `rewrite_shadow_bytes`, `drop(map_guard)`,
THEN free (RES-1 — "the worst park amplifier in the tree", `:16821-16829`).
`WriterGuardFenced` ⇒ drop parked keys + guards, `discard_layout_cache`,
`rewrite_shadow_fence_drops`, loud: publish nothing, free NOTHING (`:16857-16870`).
Transient ⇒ RE-REGISTER the epoch (never-lossy, `:16871-16883`).

Close triggers (KD-1.6, `design-rewrite-program.md:393-404`): full coverage; fsync /
`flush_inode_to_backend` (before the meta barrier); RELEASE; shape-change ops
(truncate/punch prune, spill/promotion, extent-record commit, clone, setattr size);
ENOSPC early-close (KD-1.7) generalized to the ONE allocation act
(`close_all_rewrite_epochs_for_space :16668-16712`, `rewrite_shadow_pressure_closes`);
idle `EPOCH_IDLE_HORIZON_MS = METADATA_CACHE_TTI_SECS × 1000 / 10 = 30 s`
(`routing.rs:1557-1564`); fence / unmount drain. Supersession coherence (KD-1.11):
every durable publish of a shadowed index evicts `shadow[b]` under the same (3.5)
section (`supersede_shadow_bindings :16555`, `rewrite_shadow_superseded`) — the
2026-08-04 C2Lost/C2Leaked/C3 resurrection fix. Deferred-free law (`:382-385`): a
parked A key is freed only after a durable save no longer references it.

The save's VISIBILITY (settled 2026-10-07 against the reference, 0002 rows 178–179):
in the reference the close's `remove` of the epoch and its save both run inside the
ino's meta-lock hold (`:16743-16749`), and the W1/overlay predicates and every base
read serialize behind that same lock, so the epoch's bindings never go out of sight
while a save is unlanded and the base an overwrite is seeded from IS the shadow (the
cache's dirty layout). The port holds no ino-wide lock across the save, so it keeps
the law explicitly: the saving epoch stays visible (`Epochs.closing`) until its save
landed or dropped; a record landing meanwhile opens the next epoch over the closing
shadow; closes of one ino serialize in epoch order; `Epochs.baseBinding` (the newest
shadow else the map) is the base at every use. Taking the epoch out of sight before
the merge, or basing on the durable map under a shadow, loses an acked overwrite
(`write_epoch_model.CloseVisibilityModel`).

### 5.5 Transformed volumes (`crypto_compress.rs`)

Scratch pool `worst_case_scratch_len(bs).next_multiple_of(4096)` ≈ `block + 128 KiB`
for 4 MiB blocks (`:434-459`; deliberately not `ALIGNED_BUF_POOL`, whose exactly-
`block_size` buffers cannot hold expanded compressor output), capacity
`(cores / 4).clamp(4, 16)` (`:21-32`; above the concurrent-transform count every
handout rotates through a cache-cold buffer, +4..10 %). Store-raw escape:
`FRAME_RAW_FLAG = 1 << 31` of the frame word (`:34-44`; incompressible data expands
under lz4/zstd — the raw payload is stored, bounding every image by
`max_stored_image_len = frame(4) + AEAD envelope(3 + 512 + 12 + 16) + payload`,
`:391-401`); `TRANSFORM_BLOCK_HEADROOM = 4096` per chunk, the geometry gate refuses a
mount whose chunk cannot hold a raw full block (`:80-103`, `:411-432`). `process_write`
runs at EVERY write path: the pipeline (`:21899`), the staged flush
(`write_block_from_staging`, `nvme.rs:962-976`, the transform consumes the guard-backed
bytes BEFORE the DMA), the fold, the overlay never (impossible by math —
`design-device-overlay.md:529-543`).

**Port (decision 0003 §AK.4–5, 0002 row 536).** A transformed block's upload transforms its
LIVE bytes only (`transform.liveLen`: through its last non-zero byte), so a tail block's
drain is `≤ storedImageBound(r)`; a short image
reads zeros past its plaintext (`frameSpan` ends at `plain_len`, the whole-block fill
zero-completes the decode). The write side's seeds read a mapped transformed block through
`readMappingPlain`: the chunk-long window into one pooled scratch, decoded straight into the
block-sized buffer (the codec's destination law is the image's plaintext, not the scratch).

---

## 6. W2 extent overlay (`try_extent_park :18641-18765`, `active_block.rs:213-259, 437-587`)

Representation: `Repr::Extent(ExtentOverlay{slabs: Vec<(u32, Vec<u8>)>})` mirroring
the coverage runs EXACTLY (debug-asserted `assert_slabs_mirror_runs`, `:602-616`);
`merge` coalesces overlapping/abutting slabs newest-wins (`:231-258`); payload bytes
charge `parked_extent_bytes` (`:494-500`). A 4 KiB write parks ~4 KiB, not 4 MiB.

Protocol under (3), after W1 and the overlay declined:

1. `small = len × 4 < bs` (`:18653`). Existing RAM entry: `!extent_repr ⇒ false`;
   `!small || adjacent ⇒ escalate_to_full() + false` (the ordinary checkout owns
   it; **port, 0003 §AK.2:** a stream-adjacent small piece meeting an overlay deferred
   over a mapped base MERGES — a held-open appender's appends never seed the block); `merge_extent(rel, data)`; `completed ⇒` tripwire; `extent_parks`;
   `completed || bytes × 4 ≥ bs ⇒ escalate_to_full()` (the **25 % escalation**);
   else fold thresholds `count ≥ fold_max_extents() || bytes ≥ fold_max_bytes()` ⇒
   `fold_tx.try_send((ino, b))`; `spill_parked_toward_cap(token)`; `true` (`:18658-18694`).
2. No RAM entry: `!small || adjacent || has_staged_active_block ⇒ false`
   (`:18698-18701`). Create `ActiveBlockBuf::extent(bs, needs_existing_data)`;
   rehydrate a staged record (`absorb_older_extent` — into GAPS only, `:509-532`;
   `rec.base_deferred ⇒` the deferred form — the safe direction); the record alone
   crossing 25 % ⇒ park escalated, remove the record, `false`; else `merge_extent`,
   `extent_parks`, PARK FIRST then retire the record (at every instant one copy
   serves reads, `:18749-18756`), fold hints, spill, `true`.
3. Escalation (`escalate_to_full :569-587`): allocate the block backing, lay the
   slabs, keep the union and the deferral verbatim; pure RAM, `extent_escalations`;
   `extent_implicit_escalations` counts legacy whole-image routes (designed routes
   keep it 0, `:588-599`).
4. **Spill** = a versioned + checksummed staging `active_block_ext:` record
   (`nvme.rs:727-753`: magic 8 B · version u32 (`EXTENT_RECORD_VERSION = 2`) ·
   fencing_token u64 · block_idx u32 · flags u32 (bit0 `base_deferred`) ·
   extent_count u32 · xxh3-64 over table+payloads · table (start, len) · payloads);
   NO seed read at spill (`design-random-small-writes.md:174`); `extent_spills`,
   `extent_spill_bytes`; torn/future/stale records are detected-and-ignored loudly
   (`extent_records_{torn_discarded,future_refused,stale_discarded,recovered}`).
5. **Fold** (`fold_extent_block :17616+`): under (3) `BlockLockSite::Fold`; seed base
   in authority order: staged FULL sibling > the durable block via `fetch_seed_image`
   (only when the complement is owed, `fold_seed_reads`, `flush_seed_read_bytes`) >
   zeros (hole — NO seed read, the G-RW6 clause, `:17668-17699`); apply record extents
   (older) then RAM slabs (newest); upload; `fold_passes`, `fold_fill` (k),
   `fold_extents_folded`; the merge presents the CURRENT generation per attempt
   (FIND-M11-A). Amortization `amp ≈ 2048/k + 2` (`design-random-small-writes.md:124`):
   k = 32 ⇒ ≈ 66×; k = 16 ⇒ ≈ 130× ≤ the 150× gate; measured lz4 rand-4k 15–17×
   combined, `fold_fill` mean 80.2, 411–501 → 17.7–18.3 k IOPS (`:81`).

Constants: `SQUEEZEFS_FOLD_MAX_EXTENTS` default **64** (`fuse_client.rs:1143-1146`,
"the measured amortization trigger — class C, filed, not converted", `:1197-1198`);
`SQUEEZEFS_FOLD_MAX_BYTES` derived **block_size / 4** (`derived_fold_max_bytes :1199-
1201`; env wins verbatim); the parked budget `max(256, budget/16 ÷ block_size)`
buffers' worth (`resolve_parked_cap_buffers :1213-1227`; floor 256 = the shipped
`MAX_ACTIVE_BLOCK_BUFFERS :8762`, never-regress; Red halves it,
`mem_budget::effective_parked_cap :1103`). The port's table holds the parked
cap's buffers plus as many W2 extents beside them (0003 §AP.3): a first small
piece past the extent cap takes a full buffer.

---

## 7. Staging flush units + never-lossy writeback + the fencing generation

### 7.1 The flush unit (`flush_one_active_block :33084-33351`)

Up to 8 attempts (the layout-prune epoch moving, `:33093`, `:33348-33350`), each under
(3) `WritebackFlush`:

1. `capture_epoch = layout_prune_epoch(ino)`; `staged_token = get_staged_fencing_token
   (key)` — gone ⇒ `Ok(())` (nothing to flush); `owner_token ≠ staged_token ⇒
   writeback_superseded_noops, Ok(())` (a newer write re-staged under a newer stamp —
   the contractual no-op, `:33110-33130`).
2. `allocate_placed_block()` (no shard guard across the allocate — the parking_lot
   queued-writer wedge, `:33101-33109`); `MintedBlockGuard` (RES-9 — the fsync fan-out's
   first-error unwind leaked claimed offsets); `inflight_register`.
3. `staged_dma_source(key)` — the §5.5 write-only, consume-by-value, non-`Clone`
   guard over the staging mmap (`nvme.rs:854-933`; retention past the DMA is a
   `compile_fail`); gone ⇒ `abandon_unpublished_offset`, `Ok(())`.
4. `write_block_from_staging(crypto, dev, offset, chunk, source)` — passthrough DMAs
   straight off the mmap (`WriteData::Aligned` by construction: 4 KiB packing,
   whole-block length); transform severs the guard pre-DMA (`nvme.rs:936-976`);
   `ensure_stored_block_image_fits`. Failure: dismounted ⇒ left staged for the next
   mount (info); else error; `abandon_unpublished_offset`, `Err`.
5. `publish_block`; `durable_upload_bytes_writeback`; `merge_token = CURRENT DLM
   generation` (read at the last responsible moment — the staged stamp is ONLY the
   supersession test, never the merge credential — the incident_013 livelock,
   `:33224-33228`); `merge_block_mappings_if_epoch(ino, Merge, 0,
   ToStripedKeepStagedIdentity, merge_token, Some(capture_epoch))`.
6. Merge error ⇒ `abandon_unpublished_offset`; verified-NotFound (authoritative
   `getattr` ENOENT) ⇒ `writeback_orphan_discards` + remove the staged entry +
   `Ok(())` (never resurrect a dead inode); else `Err`. `None` (a prune invalidated the
   capture) ⇒ abandon + `continue`.
7. `minted.disarm()`; `purge_block_key(new_key)` (+ `read_lru.put` of a detached
   copy on the non-striped promotion path only — guard-backed bytes never enter a
   cache); free displaced; remove the staged entry only if `current_token ==
   staged_token` (blocking pool, `:33330-33344`).

### 7.2 The writeback worker (`run_constant_writeback_worker :32542-32763`)

`WRITEBACK_MAX_ATTEMPTS = 4` (`:8755`) fast retries with backoff `50 ms << min
(attempts, 6)` (cap 3.2 s), then the counter WRAPS (`writeback_retry_exhaustions`) and
the unit re-enqueues forever — never a sticky per-ino error (the multi-volume
bench-suite EIO cascade, `:32675-32700`). Dispositions: `writeback_fence_resolution(e)`
= `WriterGuardFenced ∧ data_custody::poisoned()` ⇒ a VERIFIED fencing-stale no-op
(`writeback_fence_noops`, bytes stay staged for the remount, `:32711-32719`);
`data_plane_dismounted()` ⇒ left staged for the successor; `FencingTokenExpired` ⇒
`writeback_stale_token_retries` (a transient acquire race post-FIND-M11-A) + requeue;
other ⇒ requeue. A full requeue queue `try_send` + `is_closed` polling escapes to
teardown (`:32742-32763`). `enqueue_writeback` on a full queue flushes synchronously
(P1-2, `:23485-23499`). `SQUEEZEFS_WRITEBACK_QUEUE_CAP` default 4096 (`env_knobs.rs:187`).

### 7.3 The fencing generation, one sentence

Every durable publish presents the ino's CURRENT DLM generation and converges on
`FencingTokenExpired` by re-presenting it (`fencing_retry_token`, the staged ladder's
own contract); the staged/record stamp is the supersession and remount test only;
the GENUINE fence is `WriterGuardFenced` from the D0 latch (`write_pipeline.rs:
236-249`; `design-rewrite-program.md:447-462`).

---

## 8. ENOSPC terminal law + the wall-bounded park

- **The verdict** is the allocators' own: `allocate_block` sets the process-wide
  exhaustion latch on a `StorageFull` that survived the valve, the top-up asks and
  the pressure harvest, and clears it on the next landed allocation
  (`block_allocator.rs:2225-2238`); `fresh_supply_exhausted()` is one relaxed load
  (`fuse_client.rs:20853-20858`).
- **The custody probe** `write_would_create_fresh_custody` (`:17361-17440`): RAM-only,
  under the latch only; a metadata-cache MISS answers "held" (the refusal must never
  fire on a block whose mapping it could not see); a MAPPED block is held only where
  a write can land IN PLACE — passthrough ∧ undecorated whole-block ∧ Active volume ∧
  `sole_owner_posture_verdict == None` ∧ `refcount == Some(1)` ∧ `!is_shared` —
  else refused like growth (a transformed volume, a packed tenant, a clone-shared
  block CoWs); a hole below EOF holds nothing; parked/staged/extent/overlay custody
  holds. Refusal BEFORE the ACK, decided for the WHOLE write
  (`write_fresh_block_enospc_refusals`, `:20867-20881`). WHY: generic/751 on the
  1.3.0 chain acked every post-fill write into a parked buffer — a **74 GiB daemon on
  a 24 GiB volume** (`:20850-20853`).
- Custody already held keeps the ladder (an open block's last segment, a brim
  rewrite of a mapped block). An unreachable holder is `Retryable` — never latches
  (`overlay_unreachable_declines`, `:19046-19051`).
- **The wall-bounded park** (`allocate_block_grace_bounded :2174-2184`,
  `park_for_reclaimable_supply :2199-2223`): `StorageFull` with offsets in THIS
  mount's grace ring ⇒ park `pressure_park_slice_ms = (pressure_bound / 16).clamp
  (10, 250)` (`free_grace.rs:3014-3020`) and retry; empty ring ⇒ terminal at once;
  wall `pressure_park_wall_ms = fence_bound_base_ms × 2, floor 1_000 ms`
  (`free_grace.rs:3033-3035`) measured in WALL time (a slice-sum under-read the park
  by the RTT per pass); every caller holds (3) across the park, so past the wall the
  refusal is TERMINAL for that write.
- The fsync ladder answers a `StorageFull` its data-flush step met by a data barrier
  → the parking close of THIS ino's epoch → ONE retry (`fsync_flush_enospc_retries`,
  `fuse_client.rs:23276-23303`); a second wall is the honest ENOSPC.

---

## 9. The write-pipeline depth governor (`write_pipeline.rs`, `write_pipeline_core.rs`)

### 9.1 The admission core (loom-checked, `write_pipeline_core.rs:46-114`)

`AdmissionCore{inflight_bytes, inflight_blocks}`. `try_admit_once(bytes, target)`
(`:83-98`): `cur = bytes.load(Relaxed)`; `cur != 0 ∧ cur + bytes > target ⇒ Full`;
CAS `bytes: cur → cur + bytes` (AcqRel) ⇒ `blocks.fetch_add` + `Admitted`; else
`Raced` (re-evaluate — the caller recomputes the target). The empty-pipe bypass
predicate is `cur == 0` on the BYTES word (the same word the CAS charges) — the blocks
counter LAGS and admitted two racing oversized bypassers (`:75-82`). `release(bytes)`
exactly once per permit (RAII `PipelinePermit::drop`, `write_pipeline.rs:730-736`).
Models (`loom:4298-4447`): `write_pipeline_admission_never_exceeds_target`,
`write_pipeline_empty_pipe_bypass_is_single` (red against the blocks-counter shape),
`write_pipeline_gauges_settle_to_zero`. Wake-liveness is NOT modelled: `notify_waiters`
stores no permit; the 5 ms tick is the liveness backstop by design (`:10-19`).

### 9.2 The BDP target (`write_pipeline.rs:38-57`, `:188-215`, `:449-545`)

Per backend `Lane{ewma_lat_ns, lat_floor_ns, bw_peak_bps, win_start_ms, win_bytes}`
(`:257-319`): `record(bytes, dur, now)` — EWMA `prev − prev/8 + dur/8`; floor = running
min; window roll (`WINDOW_MS = 250`, single-winner CAS on `win_start_ms`):
`bw_peak = max(win_bytes × 1000 / elapsed, prev − prev/8)` (fast up, slow down,
`rolled_bw_peak :203-206`); `lat_floor = min(floor + floor/8, ewma).max(1)` (decay
UPWARD by ⅛, never past the EWMA — queueing inflation must not feed the BDP: at
saturation inflight ≡ bw × latency by Little's law, `rolled_lat_floor :211-215`).
`lane_target = max(FLOOR_BLOCKS_PER_LANE × bs, bdp × HEADROOM)` with
`FLOOR_BLOCKS_PER_LANE = 8` (cold posture only), `HEADROOM = 3` (absorbs
service-time variance and the non-device legs so the DEVICE leg stays ≥ 1×BDP,
`:99-109`). `depth_target_bytes(bs)` (`:449-481`): pinned override `n × bs`; else
`Σ lane targets × mul_q6 / 64` floored; **Red ⇒ `min(raw, red_drain_sum.max(floor))`**
where `red_drain_sum = Σ raw BDP` (finding 39: the fixed 32 MiB clamp starved a
10-lane ~12 ms fabric to ⅛ of its drain — 8× collapse, `parked_gate_waits +19,375`);
cap `budget / BUDGET_CAP_DIVISOR (4)`, never below one block (`:539-545`).
`write_pipeline_inflight` is a non-sheddable R5 component (`:25-33`).

### 9.3 The BBR-flavoured probe (`write_pipeline_core.rs:119-315`)

State `{mul_q6, prev_mul_q6, state ∈ {HOLD, PROBING}, baseline_bps, adopted_bps,
cooldown, epoch_start_ms, epoch_bytes, ups, backoffs}`; constants `PROBE_EPOCH_MS =
500` (two governor windows — a full floor→32× ramp at +¼ per adopted epoch ≈ 15
epochs completes in seconds, `:120-124`), `PROBE_MUL_ONE = 64` (Q6), `PROBE_MUL_MAX =
64 × 32` (runaway guard only — the R5 cap is the operative bound, `:129-132`),
`PROBE_COOLDOWN_EPOCHS = 8` (bounds the dead-gain tax to ~1/(N+1) of epochs, `:134-137`).
`roll(now, saturated, headroom)` (`:213-298`), single-winner CAS on `epoch_start_ms`:

| State | Condition | Transition |
|---|---|---|
| PROBING | `saturated ∧ bps ≥ baseline + max(baseline/16, 1)` | ADOPT: `adopted = bps`, HOLD (re-arm immediately — discovery compounds) |
| PROBING | otherwise | RETREAT: `mul = prev_mul`, HOLD; `saturated ⇒ cooldown = 8`; `backoffs++` |
| HOLD | `!saturated` | bleed: `mul = max(mul − mul/8, ONE)` (the latency guard) |
| HOLD | `collapsed = mul > ONE ∧ adopted > 0 ∧ bps < adopted − adopted/8` | step back ×0.8: `mul = max(mul − mul/5, ONE)`, `adopted = bps`, `backoffs++` (senior to launching) |
| HOLD | `cooldown > 0` | `cooldown−−` |
| HOLD | `headroom ∧ mul < MAX ∧ bps > 0` | LAUNCH: `prev = mul`, `baseline = bps`, `mul = min(mul + mul/4, MAX)`, PROBING, `ups++` |

Inputs (`write_pipeline.rs:421-440`): `saturated = admission_waits grew this epoch ∨
inflight ≥ target`; `headroom = !red ∧ target < cap`. Model
`write_pipeline_probe_epoch_roll_is_single_winner` (`loom:4460-4494`): exactly one
roller, one `probe_ups`, `mul = ONE + ONE/4`; **weakening**: a check-then-store roll
⇒ both roll, `ups == 2`, red (`:4457-4459`). Field basis: pure-BDP sustained 11.6
GB/s at its own BDP; forced depth 64 = 13.7 GB/s (+18 %) against 16.6 GB/s raw
(`:62-66`).

### 9.4 The admission loop (`admit_segment :594-645`)

Fast attempt first (no wait-list traffic); then `park = completions.notified_raw();
park.enable()` BEFORE the re-check (PERF-13 — a completion between a Full re-check and
the park's first poll was lost outright, pinning p99 `admit_wait` at the tick); re-check;
`admission_waits` once per park; seam `TEST_PREPARK_STALL_US`; `race2(park, sleep
(ADMIT_TICK = 5 ms))` — tick wins ⇒ `admission_tick_wakes` (the PERF-13 tripwire:
growth while completions flow = a lost wake). `quiesce(timeout)` (`:650-667`) same
order. The overlay takes the SAME permit (`admit_segment(len, bs)` — one gauge, one
target, one gate for both vehicles; `env_knobs.rs:250`). The permit carries the S7
custody epoch (`auth: current_epoch()`, `:563-569`).

### 9.5 The phase histograms (`fuse_client.rs:3643-3773`) — always-on

`write_pipeline_phase_ns`: `admit_wait, detach_lag, lock_wait, crypto, allocate, dma,
publish, displaced_free, inval_tail, total` — the charter's ten — plus the funnel
pair `dev_queue, dev_service` (`dma ⊇ dev_queue + dev_service`, `:3696-3703`); 12
histograms in the reference. NOT exact-sum: `total` = admission start → task end
(≈ permit release — Little's law's numerator, `:3693-3695`); the field's ~13 ms/block
UNATTRIBUTED residence motivated it (`:3649-3654`). Cost: one clock read + one relaxed
`fetch_add` per phase per block (10 × 4,000 blocks/s ≈ 40 k clock reads/s, `:3656-3661`).
Each record also stamps the op-trace stage (`:3728-3741`). `SQUEEZEFS_WRITE_PIPELINE_
DEPTH_BLOCKS`: unset = adaptive; `0` = synchronous inline (the pre-campaign A/B); `N` =
pinned (`:83-89`).

---

## 10. fsync: exact-sum phases, the per-ino stripe word, leg joining

### 10.1 Phases (`fsync_economy.rs:277-379`)

`FsyncPhase { IntentBarrier=0, DataFlush=1, StagedPromote=2, DataBarrier=3,
MetaPublish=4, MetaBarrier=5, ExtentBarrier=6, Total=7 }`, names
`intent_barrier/data_flush/staged_promote/data_barrier/meta_publish/meta_barrier/
extent_barrier/total` (`:314-323`). `FsyncProf::mark(phase)` records the span since
the previous boundary and moves it; `Drop` records 0 ns for every unmarked leg and
`total = last − t0` ⇒ `Σ phases ≡ total` to the nanosecond on every exit, equal
counts per phase (`:340-369`).

### 10.2 The ladder (`fsync :30064-30184`, `flush_inode_to_backend_prof :23251-23483`)

```
latched writeback error ⇒ report + consume (POSIX-16)
INTENT BARRIER (meta_ship::intents::fsync_ino_barrier — sequential, first; its errno
   short-circuits)                                             → mark IntentBarrier
acquire_write_lease(ino)                      [(2); fsync takes NO inode write guard]
overlay drain barrier=true (any_open_fast) ⇒ overlay_unpublished_at_fsync if any left
DATA FLUSH: flush_memory_buffers_driven(FsyncDurable) + flush_active_blocks_with_retry
   (a partial block's staging leg ESCALATES to one durable upload — DUR-1);
   StorageFull ⇒ fsync_flush_enospc_retries: flush_data_devices → close_rewrite_epoch
   → the step once more                                       → mark DataFlush
STAGED PROMOTE (lever OFF by default; promote_staged_file dispatches on size)
                                                               → mark StagedPromote
DATA BARRIER: step = fsync_barrier_step(ino, touched_lever); legs = staging sync_key
   (file_id) + one dev.flush() per target; legs ≥ 2 ∧ parallel ⇒ join_all, every leg
   to completion, first error wins (never cancel a sibling — a dropped barrier future
   fails the coalescer's queued waiters); barrier_out?; fsync_barrier_done(observed)
                                                               → mark DataBarrier
META PUBLISH: close_rewrite_epoch(ino, token)?; persist_dirty_layout_if_needed?
                                                               → mark MetaPublish
META BARRIER: backend.sync_device_for_ino(ino)?                → mark MetaBarrier
EXTENT BARRIER: the S11 FlushExtents force (overlaps the local ladder under the lever;
   both legs run to completion, local answered first)         → mark ExtentBarrier
```

DUR-1: the WHOLE barrier step completes — every leg Ok — strictly before the meta legs
that name the blocks; a failed leg fails the fsync with the touched bits left set
(`:23246-23250`). Counters: `fsync_calls`, `fsync_data_namespaces_{touched,flushed}`,
`fsync_write_through_skips`, `fsync_noop_clean` (no legs, nothing touched),
`fsync_parallel_joins`, `fsync_promoted_{files,inline_files,bytes}`,
`fsync_promote_{failures,noops}`, `fsync_flush_enospc_retries`.

### 10.3 The per-ino stripe word (`TouchedTable`, `fsync_economy.rs:130-219`)

One `AtomicU64` per stripe over the `stripe_locks` splitmix64 mix at the DLM width
(D-3): low 32 bits = per-device ordinal bits (`bit_for_ordinal`, `MAX_ORDINAL = 30`,
bit 31 = `ALL_BIT` — the unresolvable fallback); high 32 = a stamp generation.
`stamp(ino, bits)` = CAS-loop `(cur | bits) + GEN_STEP` (AcqRel) — ORs the bits AND
bumps the generation in ONE CAS (`:176-189`); `observe(ino)` (Acquire, read BEFORE the
barriers start); `clear_observed(ino, observed)` = CAS `observed → observed &
!BITS_MASK` — succeeds only if NO stamp landed since (conservative: a failure leaves
the bits for the next fsync, `:200-210`). Stripes are SHARED by construction and that
is CORRECT: a device barrier covers every write completed before it started, whoever
issued it — a false share costs one extra coalesced barrier, never a missed one
(`:24-30`). Stamp sites: every layout publish (`block_ref_ops` — the same closure the
C8 oracle proves complete) + the two in-place DMA shapes (W1 `:18606-18609`, in-place
overwrite). `plan_barriers(bits, ordinals, volatile)` (`:242-272`): a device barriers
iff touched (or ALL) AND its acknowledged writes are volatile; write-through
namespaces are `write_through_skips`.

---

## 11. Knobs, gauges/counters/histograms, constants

### 11.1 Knobs (class per the charter's three; rows at `env_knobs.rs`)

| Knob | Kind / default | Class | Doc (condensed) |
|---|---|---|---|
| `SQUEEZEFS_PATCH_MAX_BYTES` | int [0, BYTES_MAX], derived `block_size/8` (`:176`) | A/B lever (`0` = patch OFF) | W1 ceiling; also the overlay length floor |
| `SQUEEZEFS_FOLD_MAX_EXTENTS` | int, 64 (`:177`) | **R:** a free-floating constant with a knob (ledger 17 class) | W2 fold count trigger |
| `SQUEEZEFS_FOLD_MAX_BYTES` | int, derived `block_size/4` (`:178`) | A/B lever | W2 fold byte trigger |
| `SQUEEZEFS_PARKED_BUFFERS` | int, derived `max(256, budget/16 ÷ bs)` (`:179`) | A/B lever | parked-write byte budget |
| `SQUEEZEFS_INPLACE_OVERWRITE` | bool, off (`:181`) | A/B lever (substrate-measured) | in-place full-block overwrite |
| `SQUEEZEFS_DEVICE_OVERLAY` | bool, on (`:182`) | A/B lever (`0` = accumulation control) | the overlay arm |
| `SQUEEZEFS_OVERLAY_OVERWRITE` | bool, on (`:183`) | A/B lever | the overwrite arm; venue split recorded |
| `SQUEEZEFS_OVERLAY_CLOSE_BARRIER` | bool, off (`:184`) | A/B lever (OQ-5) | barrier before a coverage close |
| `SQUEEZEFS_OVERLAY_DEPTH_GOVERNOR` | bool, on (`:250`) | A/B lever | overlay stores take the pipeline permit |
| `SQUEEZEFS_GAP_SEED_RANGED` | bool, on (`:251`) | A/B lever | ranged gap seeds |
| `SQUEEZEFS_ZC_ACK_EARLY` | bool, on (`:249`) | A/B lever | ACK-early |
| `SQUEEZEFS_ZC_ACK_EARLY_ODIRECT` | bool, off (`:255`) | opt-in posture | held-GUP snapshot ACK-early |
| `SQUEEZEFS_REWRITE_SHADOW` | bool, on (`:185`) | A/B lever | the rewrite epoch |
| `SQUEEZEFS_REWRITE_SUPPLY_CLOSE` | Retired (`:186`) | delete | — |
| `SQUEEZEFS_WRITEBACK_QUEUE_CAP` | int [1, 2^24], 4096 (`:187`) | **R:** literal default | flush-unit queue |
| `SQUEEZEFS_WRITE_PIPELINE_DEPTH_BLOCKS` | int [0, 2^20], derived BDP (`:188`) | A/B lever (`0` = sync inline, `N` = pinned) | depth override |
| `SQUEEZEFS_RECLAIM_CAP_PARK_MS` | int, derived clamp(4 × batch ÷ drain, 50, 1000) (`:195`) | A/B lever | reclaim park bound |
| `SQUEEZEFS_NVME_WRITE_LANES` | int [1, 1024], derived cpus/devices (`:217`) | A/B lever | write fan-out (NOT the overlay's — KD-OV-4) |
| `SQUEEZEFS_WRITE_SHARED` | bool, on (`:220`) | A/B lever | ported (op_guard) |
| `SQUEEZEFS_WRITE_GUARD_NARROW` | bool, on (`:221`) | **R:** second shared-mode lever (ledger 85) | not carried |
| `SQUEEZEFS_FSYNC_TOUCHED_NAMESPACES` | bool, on (`:252`) | A/B lever | barrier only touched namespaces |
| `SQUEEZEFS_FSYNC_PARALLEL_LEGS` | bool, on (`:253`) | A/B lever | joined legs |
| `SQUEEZEFS_FSYNC_PROMOTE_STAGED` | bool, off (`:254`) | A/B lever (measured +26 % per fsync unconditional) | fsync promotes staged |
| `SQUEEZEFS_TEST_UPLOAD_STALL_MS` / `_WRITE_STALL_MS` / `_INVAL_TAIL_STALL_MS` / `_CHECKOUT_STALL_MS` / `_ROUTER_DISPATCH_STALL_MS` / `_ATTR_PUBLISH_STALL_MS` | int ms, 0 (`:396-404`) | `SQUEEZEFS_TEST_*` seams | the pinned windows |
| `SQUEEZEFS_TEST_OVERLAY_BYTES` | bool, 0 (`:409`) | seam | Bytes vehicle on with overlay off |
| `SQUEEZEFS_TEST_OVERLAY_CAPTURE_STALL_MS`, `set_test_settle_transient_failures` | cell / seam | seams | §5.1 interleave pin; injected settle transients |

**R:** every write-path knob is read through a per-module tri-state/`OnceLock` cell
(`device_overlay.rs:56-62, 82-92`, `fuse_client.rs:581-590`, `write_pipeline.rs:
157-167` parses with `.trim().parse()` and `.filter(|v| *v >= 0)` — a second
lenient parser, ledger 14/16). **PORT:** every row read once at mount through
`knobs.boolKnob/intKnob(row)`; A/B levers logged when non-default; the per-mount
`WritePath.config` struct carries the resolved values.

### 11.2 Gauges / counters / histograms (names are the stats-inode keys)

Overlay: `overlay_open` (gauge, 0 at quiesce), `overlay_inflight_bytes` (gauge, R5
non-sheddable), `overlay_installs`, `overlay_overwrite_installs`, `overlay_stores`,
`overlay_store_bytes` (CQE-counted, the closure quantity), `overlay_overwrite_bytes`
(subset), `overlay_ack_early_{stores,bytes,retries}`, `overlay_short_stores`,
`overlay_store_fallbacks`, `overlay_claim_conflicts`, `overlay_governed_stores`,
`overlay_dma_{passthrough,pool_copy}_bytes`, `overlay_ineligible_{sub_cap,shadow_bound,
range_shared}`, `overlay_sub_cap_joins`, `overlay_enospc_declines`,
`overlay_unreachable_declines`, `overlay_read_{serves,serve_bytes,gap_serves,gap_bytes,
drains}`, `overlay_window_escalations`, `overlay_gap_seeds`, `overlay_gap_seed_{bytes,
old_bytes,ranged_bytes,read_bytes}`, `overlay_publishes`, `overlay_published_bytes`,
`overlay_epoch_feeds`, `overlay_feed_fallbacks` (growth with shadow ON = bug),
`overlay_supersessions`, `overlay_superseded_{by_merge,by_served_publish,
dead_old_binding}`, `overlay_mover_skips`. **Must-stay-0:** `overlay_fence_drops`,
`overlay_unpublished_at_fsync`, `overlay_ack_early_lost`, `overlay_teardown_waits`
timing out; tripwires `overlay_teardown_nonterminal`, `overlay_install_race_under_
guard`, `overlay_store_superseded_under_guard`. Closure: `overlay_store_bytes ≈ user
bytes` on the eligible row; `overlay_ack_early_bytes ≈ overlay_store_bytes` is the
separate ACK-early check, NEVER a closure term (counts at ACK vs CQE — summing double-
counts, `design-overlay-overwrite.md:1051`).

W1: `patch_writes`, `patch_write_bytes`, `patch_dma_errors`, `patch_ineligible_
{adjacent,oversize,unaligned,range_shared,overlay,device_overlay,transform,unmapped,
decorated,shared,foreign_custody,posture}`; **must-stay-0:** `patch_edge_rmw_reads`.
Ledger identity: every striped write resolves to exactly one of {patched, one
bucket} (`:18303-18307`).

Accumulation: `write_through_{blocks,bytes,fallbacks,inplace_overwrites,inplace_
rewrites}`, `nt_copy_bytes`, `placed_{adoptions,merge_elides,adoption_refusals,
assembly_bytes}`, `active_block_{cow_copies,memset_elided_bytes,ooo_runs}`,
`overwrite_seed_{deferred,materialized,skipped}`, `write_block_revisits`,
`staging_sibling_{probes,hops_elided}`, `restage_churn_{removes,bytes}`,
`parked_full_buffer_bytes` (gauge), `parked_gate_{waits,self_flushes,timeouts}`,
`durable_upload_bytes_{writeback,escalation,self_flush}`, `staging_put_bytes_
{wt_fallback,drain,flush,teardown}`; tripwires `coverage_complete_buffer_not_content_
valid`, `complete_union_with_extra_runs`, `zero_complete_on_deferred_seed`.

W2: `parked_extent_bytes` (gauge, sheddable via fold), `extent_parks`,
`extent_escalations`, `extent_implicit_escalations` (0 on designed routes),
`extent_record_absorbs`, `extent_spills`, `extent_spill_bytes`, `extent_records_
{recovered,stale_discarded,torn_discarded,future_refused,foreign_scope}`,
`fold_passes`, `fold_fill` (histogram), `fold_seed_reads` (≈ `fold_passes`),
`fold_extents_folded`; tripwires `extent_overlay_completed_coverage`,
`small_first_write_completed_block`.

Pipeline: `write_pipeline_inflight_{bytes,blocks}` (gauges), `write_pipeline_
admission_waits`, `write_pipeline_admission_tick_wakes` (tripwire-class),
`write_pipeline_depth_target[_base]`, `write_pipeline_depth_probe_{ups,backoffs}`,
`write_pipeline_superseded_publishes`,
`write_pipeline_phase_ns` (12 histograms); **must-stay-0:** `write_pipeline_fence_drops`
on a healthy mount.

Rewrite epoch: `rewrite_shadow_open_epochs`, `rewrite_shadow_parked_bytes` (gauges),
`rewrite_shadow_{swaps,bytes,fallbacks,close_retries,superseded,pressure_closes,
pressure_close_busy}`, `rewrite_{blocks,user_bytes,device_write_bytes}` (the
`rewrite_amp` SLO); **must-stay-0:** `rewrite_shadow_fence_drops`.

Writeback: `writeback_queue_{cap,depth}`, `writeback_enqueued_{wt_fallback,drain,
flush,teardown}`, `writeback_{superseded_noops,stale_token_retries,retry_exhaustions,
orphan_discards,fence_noops,errors_latched,errors_reported}`.

ENOSPC: `write_enospc_refusals`, `write_fresh_block_enospc_refusals`,
`fsync_flush_enospc_retries`.

fsync: `fsync_phase_ns` (8 histograms, exact-sum), `fsync_calls`, `fsync_data_
namespaces_{touched,flushed}`, `fsync_write_through_skips`, `fsync_noop_clean`,
`fsync_parallel_joins`, `fsync_promoted_*`, `fsync_touched_unresolved` (should stay 0).

Level-3 attribution: `block_lock_wait` (always-on) with per-site labels
`WriteCheckout / PipelineUpload / WritebackFlush / Fold / OverlayPrune`.

### 11.3 Constants with WHY

| Constant | Value | WHY (source) |
|---|---|---|
| `OVERLAY_PAGE` | 4096 | the v1 gate: LBA-aligned segments; one claim/coverage bit per page (`overlay_core.rs:71-73`) |
| `PATCH_MAX` | `block/8` = 512 KiB | the natural sub-block denominator; field-identical to the pre-sweep 512 KiB (`fuse_client.rs:1186-1193`) |
| `FOLD_MAX_BYTES` | `block/4` = 1 MiB | the former flat 1 MiB (`:1195-1201`) |
| `FOLD_MAX_EXTENTS` | 64 | measured amortization trigger (class C, `:1197`) |
| escalation edge | coverage ≥ 25 % (`bytes × 4 ≥ bs`) | design §5.2 (`:18682`, `:18727`) |
| `small` | `len × 4 < bs` | the extent-eligible class (`:18653`) |
| `MAX_ACTIVE_BLOCK_BUFFERS` | 256 | the shipped floor, never-regress (`:8762`, `:1209-1210`) |
| parked budget | `max(256, budget/16 ÷ bs)` | R5 already gauges/sheds parked RAM (`:1203-1227`) |
| `POOLED_BUF_ALIGN` | 4096 | O_DIRECT / `WriteData::Aligned` (`pool.rs:28`) |
| block pool capacity | `max(cores × 16, 64)` | fleet-share-divided root (`pool.rs:319-321`) |
| scratch pool | `worst_case(bs).next_multiple_of(4096)` ≈ block + 128 KiB; cap `(cores/4).clamp(4, 16)` | expanded compressor output; FIFO rotation cache-cold above the hot set (`crypto_compress.rs:21-32, 434-459`) |
| `FRAME_RAW_FLAG` | `1 << 31` | image lengths ≤ 4 MiB class ⇒ bit 31 was always 0 (`:34-44`) |
| `TRANSFORM_BLOCK_HEADROOM` | 4096 | frame + AEAD envelope (547 B) rounded to one LBA (`:80-91`) |
| `FLOOR_BLOCKS_PER_LANE` | 8 | cold posture only; Red's LOWER bound (`write_pipeline.rs:99-105`) |
| `HEADROOM` | 3 | the device leg stays ≥ 1×BDP (`:107-109`) |
| `BUDGET_CAP_DIVISOR` | 4 | the payload-buffer cap shape (`:111-113`) |
| `WINDOW_MS` | 250 | two orders above service time (`:115-117`) |
| `ADMIT_TICK` | 5 ms | liveness backstop for target changes with no completion (`:119-124`) |
| `PROBE_EPOCH_MS` | 500 | a full ramp completes in seconds (`write_pipeline_core.rs:120-124`) |
| `PROBE_MUL_MAX` | 32× | runaway guard; R5 cap is operative (`:129-132`) |
| `PROBE_COOLDOWN_EPOCHS` | 8 | dead-gain tax ~1/(N+1) (`:134-137`) |
| `WRITEBACK_MAX_ATTEMPTS` | 4, backoff `50 ms << min(a, 6)` | fast retries then wrap (`fuse_client.rs:8755`, `:32740`) |
| `SETTLE_RETRIES` | 24, `1 ms << min(a−1, 3)` | the rebind ladder's own `MAX_REBINDS` (`:19800-19802`) |
| write-arm settle backoff | `min(1 ms << min(a, 6), 50 ms)` | retry forever, bark at powers of two (`:19853-19856`) |
| ACK-early retry backoff | `min(10 ms << min(a, 7), 1 s)` | acked custody retries until it lands (`:19604-19607`) |
| settle bark | 30 s | a leaked ticket named, never a hang (`:3319`) |
| `EPOCH_IDLE_HORIZON_MS` | TTI 300 s / 10 = 30 s | the eviction horizon the close defends (`routing.rs:1557-1564`) |
| park slice / wall | `(pressure_bound/16).clamp(10, 250)` / `2 × fence_bound, floor 1 s` | responsive, never a busy-spin; a frozen plane refuses at the wall (`free_grace.rs:3014-3035`) |
| `TouchedTable::MAX_ORDINAL` | 30 (bit 31 = ALL) | 32 ordinal bits + 32 generation bits in one u64 (`fsync_economy.rs:134-146`) |

---

## 12. Tests (name + sentence; venue: device = a block-device/file-backed data volume in-process; mount = a real FUSE mount)

`tests/overlay_core_tests.rs` (pure, portable): `coverage_union_is_overlap_safe_and_
order_blind`, `_tracks_out_of_order_runs_and_gaps`, `_set_full_claims_everything`,
`coverage_publishes_only_on_successful_complete` (law 3), `completion_transition_
fires_exactly_once_at_full_union`, `overlapping_inflight_stores_are_unrepresentable`
(§2.3), `generations_are_strictly_monotone_per_accepted_segment`, `read_snapshot_
revalidation_catches_mid_fetch_accepts`, `freeze_refuses_new_stores_and_inflight_drains_
to_publishable`, `law9_rollback_requires_terminal_state_and_empty_inflight`,
`fence_drop_is_terminal_and_never_publishable`, `fed_is_a_distinct_terminal_reachable_
only_from_the_freeze`; proptest schedules never violate the laws.

`tests/device_overlay_tests.rs` (device): `overlay_defaults_on_when_unset`;
`fresh_block_segments_ride_the_overlay_and_publish_at_completion` (stores ≈ user
bytes, extraction ≈ 0); `partial_overlay_fsync_seeds_zeros_and_publishes_whole_block`;
`open_overlay_reads_serve_acked_bytes_and_zero_gaps` (law 5); `partial_overlay_read_
does_not_seed_or_publish`; `fresh_stream_storm_never_serves_stale_bytes` (generic/209);
`fresh_overlay_never_creates_a_shadow_epoch_interaction`; `crash_without_drain_
reclaims_unpublished_offsets` (kill-9 census); `ineligible_shapes_decline_and_stay_
byte_exact`; `truncate_drains_open_overlays`.

`tests/overlay_overwrite_tests.rs` (device): `hazard1_single_displaced_park`,
`hazard2_no_double_free`, `hazard3_no_kd111_resurrection`, `hazard4_one_fsync_
authority`, `hazard5_ref_drift_zero` (the five dual-authority hazards, shadow ON);
`shadow_off_degenerate_frees_displaced_only_after_durable_publish`; `close_never_
runs_under_block_guard`; `enospc_overwrite_loop_converges_via_epoch_close`;
`fsync_drains_feeds_before_the_one_close`; `settle_publish_never_trips_the_foreign_
merge_hook` (KD-B4-11); `recycled_content_never_served_on_mapped_shape`;
`partial_overwrite_settle_seeds_old_binding_bytes`; `warm_tier_never_serves_stale_
under_open_overlay` (KD-OV-13); `patch_bytes_never_lost_to_overlay_publish`
(clause 8); `mover_merge_skips_open_overlay`; `fsck_damaged_flip_refuses_under_live_
overlay`; `discarding_op_undrained_supersedes_and_retires_detached`; `compose_vs_
close_storm_with_hot_reclaim`; `live_mapped_overwrite_rides_the_overlay_end_to_end`;
`overwrite_install_declines_shadow_bound_and_enospc`; `capture_and_install_share_one_
meta_section` (the §5.1 interleave pin via the capture stall seam); `kill9_crash_
matrix_ow1_to_ow7`; `ow8_window_is_exactly_as_disclosed` (power-cut harness).

`tests/overlay_ack_early_tests.rs` (device, slot seam): `ack_early_write_returns_
before_store_cqe`; `fsync_awaits_acked_inflight_store`; `read_waits_for_acked_inflight_
store` (the future law); `failed_acked_store_retries_until_landed`; `odirect_class_
needs_the_unstable_write_opt_in`; `odirect_ack_early_must_not_observe_post_ack_reuse`;
`odirect_overlay_is_not_hold_eligible`; `bytes_overlay_ack_early_returns_before_device_
cqe`; `aligned_overlay_bytes_skip_the_pool_copy`; `dead_ring_store_falls_back_to_device_
write`; `dead_ring_and_dead_extract_terminate_loud_never_wedge`; `stale_size_seed_class_
never_zeros_a_published_overlay_sibling` (generic/551); `overlay_store_burst_parks_in_
pipeline_admission_beyond_depth_target`; `overlay_completions_feed_the_lane_bdp` (W-3).

`tests/overlay_length_floor_tests.rs` (device): `sub_cap_hole_write_rides_w2_never_the_
overlay`; `sub_cap_write_to_a_clone_shared_block_rides_w2_never_the_overlay`; `above_
cap_aligned_overwrite_still_takes_the_overlay`; `the_floor_tracks_the_block_size`;
`patch_cap_zero_makes_every_length_overlay_eligible`; `rand_4k_burst_into_holes_rides_
w2_amortized`; `small_bs_sequential_segments_accumulate_into_one_write_through`.
`tests/overlay_gap_seed_ranged_tests.rs`: `ranged_gap_seed_reads_only_the_uncovered_
span_on_passthrough` (+ the whole-image control). `tests/overlay_settle_wait_tests.rs`:
`settle_wait_parks_and_wakes_on_complete_store`, `settle_wait_belt_survives_a_lost_
wake`. `tests/overlay_growth_merge_tests.rs` (mount, O_DIRECT, the router-dispatch
stall seam): the 2026-09-10 staged→striped promotion racing an overlay — every
segment byte-exact. `tests/write_grid_straddle_tests.rs`: `a_skewed_grid_delivered_
{out_of_order,in_order}_lands_every_block_once`, `an_aligned_grid_lands_every_block_
once` (§4.4ci — judge PIECES).

`tests/extent_patch_tests.rs` (device): `is_whole_block_mapping_truth_table`;
`patch_byte_exactness_on_fresh_striped_file` (the Issue-19 polarity tripwire);
`exclusion_{unaligned_offset_or_len, extending_write, oversize_len_and_knob_disable,
stream_adjacent, overlay_present_ram_and_staged, range_shared_custody, shared_
refcount_after_clone, transform_compressed_volume, hole_unmapped_block, decorated_
mapping_never_scribbles_outside_window}`; `clone_cfr_vs_patch_storm` (both orders,
byte-audit both files post-race and post-remount; bounded loud refusal or success);
`read_mid_patch_coherence_no_torn_or_stale_serves`; `patch_dma_error_returns_eio_and_
restabilizes`; `write_verification_covers_patched_window`; `crash_after_acked_patch_
foreign_bytes_intact_window_old_or_new`; `crash_mid_patch_dma_failure_leaves_old_block_
intact`; `cli_clone_refuses_live_writer`; `read_survives_perpetual_patch_storm_no_
rebind_eio`. `tests/rand_write_amp_tests.rs`: the device-byte ledger per 4 KiB
overwrite ≤ thresholds; `patch_writes ≈ ops`, `patch_edge_rmw_reads = 0`.

`tests/extent_overlay_tests.rs` (device): `small_nonadjacent_write_parks_compactly`;
`escalation_at_quarter_coverage_both_edges`; `spill_writes_extent_records_without_
seed_reads`; `fold_amortizes_k_extents_over_one_seed`; `hole_writes_fold_without_seed_
storms`; `overlap_rewrite_within_overlay_newest_wins`; `byte_budget_replaces_the_count_
cap`; `never_lossy_staging_refusal_and_fold_failure_repark`; `fold_seeds_decorated_
promoted_mapping`; `reads_mid_fold_serve_exact_bytes`; `fold_revalidates_fencing_per_
attempt_and_never_livelocks`; `staged_rider_subimage_overwrite_uses_extent_records`;
`rider_truncate_clips_record_extents`; `stale_binding_decode_failure_rebinds_not_
errors`. `tests/extent_record_recovery_tests.rs`: generation/fencing/torn-record
recovery, future-version loud unit refusal, orphan forward detection.

`tests/write_through_coverage_tests.rs` (device, RW3b): `ooo_end_first_segments_
write_through_without_seed_reads`; `in_order_segments_write_through_without_seed_
reads`; `ooo_three_segment_shuffles_single_write_through`; `partial_end_aligned_
segment_defers_then_flush_seeds` (tail-first `[2M,4M)` PARKS); `overlapping_rewrites_
inside_accumulation_stay_exact`; `ooo_fresh_create_segments_no_rmw_pipeline`; `reads_
during_ooo_window_serve_exact_bytes`; `crash_with_ooo_parked_segments_leaves_old_
blocks_intact`. `tests/striped_overwrite_lazy_seed_tests.rs`: `full_coverage_overwrite_
reads_nothing_and_is_byte_exact`, `partial_coverage_seeds_old_bytes_at_flush`,
`{truncate,punch,cfr}_inside_deferral_window`. `tests/write_supersession_tests.rs`
(the upload stall seam): `stale_inflight_upload_never_publishes_and_newest_bytes_win`
(the port's law differs, §AJ.7: the snapshot publishes, the newest bytes still win); `flush_that_wins_makes_the_
stalled_task_a_counted_noop`; `inplace_lever_keeps_the_serialized_upload`.
`tests/pipeline_inval_tail_race_tests.rs`: `detached_inval_tail_cannot_retire_a_
concurrent_writers_parked_entry`. `tests/inplace_overwrite_tests.rs`: `eligible_full_
block_rewrite_lands_in_place_with_zero_displacement`, `clone_shared_blocks_keep_cow_
displacement`, `lever_off_restores_cow_always`.

`tests/rewrite_shadow_tests.rs` (device): `epoch_records_are_ram_only_until_the_one_
save_swap`; `full_coverage_auto_closes_the_epoch`; `crash_pre_swap_leaves_a_intact_and_
reclaims_b`; `fenced_close_publishes_nothing_and_frees_nothing`; `a_process_local_
lease_rotation_converges_the_close`; `enospc_early_close_frees_supply_and_converges`;
`a_writeback_flush_that_meets_storage_full_closes_the_epoch_and_lands` (5b); `the_
pressure_close_never_parks_on_a_held_meta_stripe` (5c); `an_fsync_whose_flush_met_a_
busy_skip_closes_its_own_epoch_and_lands` (5d); `one_epochs_per_file_fence_never_fails_
the_pressure_sweep`; `mid_epoch_b_offsets_are_inflight_registered` (fsck 0 findings);
`lever_off_restores_per_block_durable_publishes`. `tests/rewrite_shadow_supersede_
tests.rs`: `durable_merge_supersedes_stale_shadow_binding`; `resurrection_sequence_
cannot_double_free_or_leak`; `truncate_supersedes_shadowed_indexes_past_the_cut`.

`tests/write_pipeline_tests.rs`: pure `depth_override_lever_resolves_sync_pinned_
adaptive`, `lane_target_is_floor_while_unlearned_and_bdp_headroom_once_measured`,
`bw_peak_learns_fast_up_and_decays_slow_down`, `lat_floor_decays_upward_but_never_past_
the_ewma`, `disposition_mapping_is_the_task_counting_contract`, `probe_core_{probes_up_
on_responsive_saturated_epochs, backs_off_when_gain_dies_and_cools_down, latency_
guard_never_inflates_without_saturation_and_decays, hold_revalidation_retreats_when_
delivery_collapses, is_bounded_and_never_probes_without_headroom}`; device
`depth_target_grows_with_measured_bandwidth_and_stays_budget_bounded`, `red_clamps_
target_to_measured_drain_and_pinned_override_wins_verbatim`, `red_clamp_derives_from_
measured_drain_never_a_fixed_aggregate_floor` (finding 39), `pipeline_probe_grows_
depth_target_beyond_bdp_and_retreats`, `pipeline_probe_respects_budget_cap_and_red_
clamp`, `admission_parks_at_target_and_wakes_on_completion`, `empty_pipe_always_
admits_and_quiesce_waits_for_drain`, `t1_completing_write_acks_before_upload_and_
drains_durably`, `t2_stale_token_write_through_converges_and_publishes`, `t3_fsync_
drains_pending_pipeline_uploads_durably`, `t4_teardown_drains_pipeline_custody`,
`t5_fsync_steal_rides_write_through_not_staging`, `admission_wake_is_not_lost_to_the_
park_window` (PERF-13, the prepark stall seam). `tests/write_pipeline_phase_tests.rs`:
`phase_family_is_always_on_with_exact_keys`, `pipeline_write_through_records_every_
residence_phase`, `recording_lands_in_exactly_the_named_phase`.

`tests/write_enospc_terminal_tests.rs` (device): `the_park_wall_is_a_duration_never_
the_reallocation_label`; `a_full_store_with_nothing_reclaimable_refuses_at_once`;
`full_store_writes_terminate_and_the_mount_keeps_answering`; `a_parked_pipeline_upload_
returns_its_permit_within_the_bound`; `an_enospc_write_does_not_poison_its_sibling_
block`; `freeing_space_after_enospc_lets_new_writes_land`; `the_write_enospc_refusals_
gauge_counts_exactly_the_refused_writes`; `fresh_block_writes_against_an_exhausted_set_
are_refused_before_the_ack`; `fresh_block_writes_are_refused_on_the_synchronous_shape_
too`; `acked_custody_of_an_open_block_keeps_the_never_lossy_ladder`; `a_brim_rewrite_of_
a_mapped_block_is_admitted_while_the_set_is_exhausted`; `the_exhaustion_latch_clears_
when_supply_returns`; `concurrent_fresh_block_writers_against_a_latched_store_are_all_
refused`; `a_write_into_a_hole_below_eof_of_a_pre_sized_file_is_refused_when_exhausted`;
`a_rewrite_of_a_mapped_block_on_a_transformed_volume_is_refused_when_exhausted`;
`a_dropped_latched_allocator_leaves_the_process_count`; `a_retryable_class_is_never_a_
terminal_writeback_error`.

`tests/fsync_economy_tests.rs` (device, multi-volume): `fsync_phase_family_is_exact_sum_
and_rides_the_stats_inode`; `fsync_barriers_only_the_namespaces_the_ino_wrote`; `clean_
fsync_is_a_counted_data_noop_and_keeps_the_meta_barrier`; `in_place_dma_shapes_stamp_
their_namespace`; `write_through_namespaces_skip_the_barrier`; `lever_off_barriers_every_
namespace`; `parallel_legs_pay_max_not_sum`; `an_fsync_storm_coalesces_its_meta_
barriers`; `a_failed_data_barrier_still_fails_the_fsync_before_the_meta_barrier`;
`staged_promotion_lever_promotes_exactly_once_on_fsync`; `touched_table_word_law`;
`barrier_plan_law`. `tests/fsync_durability_contract_tests.rs`: `test_fsync_publishes_a_
partially_covered_striped_block`; `test_fsync_publishes_a_fresh_partially_written_block`;
`test_fsync_issues_a_data_device_barrier`; `test_data_barrier_precedes_the_metadata_
barrier` (DUR-1). `tests/fsync_writeback_tail_loss_tests.rs`: `release_captured_token_
flush_converges_and_never_drops_custody`; `stale_token_write_through_unit_converges_and_
publishes`; `release_then_fsync_never_zeroes_the_tail_block`.

`tests/writeback_tests.rs`: `test_writeback_queue_full_deadlock`; `test_staged_dma_
source_is_guard_backed_and_flush_then_remove_sequences`; `test_write_block_from_staging_
transform_leg_drops_guard_before_dma`; `test_striped_flush_dma_zero_copy_aligned_no_lru_
retention`; `test_promotion_flush_lru_entry_is_real_copy_not_guard_backed`; `test_flush_
write_verification_readback_runs_within_guard_hold`. `tests/writeback_fencing_livelock_
tests.rs`: `stale_token_unit_still_owner_flushes_to_durability`; `superseded_unit_
resolves_as_noop_newest_bytes_win`; `teardown_force_flush_drains_stale_token_entries_
bounded`; `reclaimed_inode_unit_discards_orphan_staging_no_spin`; `fsstress_shaped_
epoch_churn_converges_after_quiesce`. In-module: `writeback_requeue_on_full_queue_never_
goes_sticky`, `writeback_exhausted_retries_requeue_forever_not_sticky`
(`fuse_client.rs:33518, 33555`).

`tests/write_visibility_tests.rs` (generic/209 class): `completed_overwrites_never_
serve_the_previous_pass`; `serialized_overwrite_never_reverts_neighbors`; `patched_
block_never_serves_a_held_flight_snapshot`; `patched_block_never_serves_a_mid_publish_
tier_deposit`; `sequential_recopy_readers_never_see_foreign_bytes`; `overlay_compose_
read_clamps_to_eof` (generic/795); `overlay_installed_inside_read_window_never_hides_
acked_bytes`; `settle_inside_the_validate_gap_never_hides_acked_bytes`; `copy_file_
range_sees_parked_overlays_v3`; `prune_epoch_refuses_stale_delayed_merge_v3`;
`whole_file_clone_carries_parked_source_custody`.

Loom (→ `src/model/*_model.zig`): `patch_clone_composed_never_mutates_a_validated_
pin`, `incarnation_validated_fill_never_serves_mid_patch_bytes`, `cow_snapshot_never_
changes_under_racing_writer`, `cow_uniqueness_restored_after_remote_snapshot_drop`,
`write_pipeline_admission_never_exceeds_target`, `write_pipeline_empty_pipe_bypass_is_
single`, `write_pipeline_gauges_settle_to_zero`, `write_pipeline_probe_epoch_roll_is_
single_winner`, `overlay_validated_serve_is_never_torn`, `overlay_freeze_drain_
observes_final_coverage` — each with its weakening proven to fail.

---

## 13. QUIRKS per decision 0002 (ledger candidates) with the better design

| # | Reference quirk | Why it is poor | PORT |
|---|---|---|---|
| Q1 | The write admission ledger `admit_write_{overlay,inplace,accumulate,inline}_bytes` the charter names does not exist; engagement is five unrelated families (`overlay_store_bytes`, `patch_write_bytes`, `nt_copy_bytes`+`placed_merge_elides`, extent slab deltas, router commits) | The one closure identity the charter states (`Σ ≡ user bytes at ACK`) is untestable without assembling families whose counting points differ (ACK vs CQE) | ONE family incremented at each arm's ACK site; the per-mechanism counters stay as internal faces; a closure test per write shape |
| Q2 | Every write-path knob read through a per-module tri-state `AtomicU8`/`OnceLock` cell with its own `std::env::var(...).trim().parse()` (`write_pipeline.rs:157-167`, `fuse_client.rs:581-590`, `device_overlay.rs:56-72`) and `cfg!(test)`-dependent defaults (`device_overlay.rs:87, 148`) | Ledger 14/16: a second lenient parser per knob; a default that differs between the test binary and the product | Rows read once at mount through `knobs.*(row)`; one `WritePath.Config`; tests pass the config explicitly — no `cfg(test)` default anywhere |
| Q3 | `write_pipeline_phase_ns` is NOT exact-sum (`total` = admission → task end; `dma ⊇ dev_queue + dev_service`; 12 names for "ten phases") | Charter §10: phase histograms are exact-sum; two overlapping decompositions in one family | Two exact-sum families: the ten residence phases partitioning `total` (`admit_wait … inval_tail`, with an explicit `other` leg so `Σ ≡ total`), and the device funnel pair as its own `nvme_write_phase_ns` |
| Q4 | `await_overlay_pages` is a `yield_now` spin on `range_inflight` (`fuse_client.rs:20534-20543`), counting `overlay_teardown_waits` at 1,000 spins — while the settle wait moved to `inflight_change: Notify` after the 2026-08-13 fused-lane wedge | A spin on the reader path; the same class the settle already fixed | One `sync/completion`-class event per record for both waits (a per-page claim release wakes it); the 1,000-spin tripwire becomes a wait-age tripwire |
| Q5 | `ZcWriteSlot::materialize` memoizes under an async `SqzMutex` INSIDE the block stripe's critical section (`routing.rs:7108-7128`) | A lock taken under level (3) whose holder parks on the transport | The port's `PayloadLease` is the slot by value; extraction is a lane-resident op with a plain once-cell on the request frame (no mutex, no lock under (3)) |
| Q6 | The in-place full-block overwrite lever (`SQUEEZEFS_INPLACE_OVERWRITE`, default OFF — measured −20 % on the field substrate) and its serialized upload venue (`pipeline_upload_serialized`) | A measured-loser control arm kept as a default-off posture knob (charter §1.6: the user never chooses a data path) | Not carried in rung 4; W1's whole-block face stays CoW; if a substrate A/B ever wins it lands with its own row |
| Q7 | `SQUEEZEFS_FOLD_MAX_EXTENTS = 64` and `SQUEEZEFS_WRITEBACK_QUEUE_CAP = 4096` are literal defaults with a knob on top | Ledger 17 class (charter §1.5) | `FOLD_MAX_EXTENTS` derived as `block_size / (4 × OVERLAY_PAGE)`'s measured-amortization fraction with the WHY on the line (the 2048/k + 2 formula and the 150× gate); the writeback queue cap derived from the parked budget (`parked_cap_buffers`), no knob |
| Q8 | The B2 "Bytes vehicle" test seam (`SQUEEZEFS_TEST_OVERLAY_BYTES`) and `set_*_for_tests` setters beside every lever (`device_overlay.rs:134-137, 157-160, 213-215, 294-303`) | Test posture through process globals (ledger 85's shape) | The overlay store takes a `Vehicle` vtable (slot / bytes) from the request; tests construct the bytes vehicle directly |
| Q9 | `last_write_end` is an `scc::HashMap<u64, AtomicU64>` grown per ino forever (`fuse_client.rs:18287-18295`) | An unbounded per-ino table for one word | A striped `[N]atomic u64` keyed by `splitmix64(ino)` (a collision costs one misrouted adjacency verdict — the accumulation path, never corruption); N = the op-guard stripe width |
| Q10 | Panic-guarded detached tasks (`tpc_spawn_guarded`) as the error boundary for the upload, publish and ACK-early continuations | Ledger 9 | Error unions + tripwires on the lane; a true bug aborts loud |
| Q11 | `Arc<dyn Fn() -> bool>` Red probe and `scc::HashMap<String, Arc<Lane>>` keyed by backend-id STRING in `WritePipeline` (`write_pipeline.rs:339, 351`) | A heap string hash per completion on the hot path; a boxed closure for one level read | Lanes indexed by the router's dense volume ordinal (the same `u32` the `TouchedTable` uses); the Red level read directly from `mem.Budget` |
| Q12 | The overlay's `old_binding: Option<String>` and `new_key: String` mapping strings parsed at every use (`persist_block_key`/`split_block_key`) | A string as the block binding on the data hot path | The port's binding is the rung-4 `extern struct` mapping value (vol ordinal, offset, decoration); strings exist only at the CLI |
| Q13 | `SETTLE_RETRIES = 24` borrowed from `MAX_REBINDS` "rather than a fresh number" and the write-arm's `min(1 ms << a, 50 ms)` cap | Two retry ladders with two shapes for one condition | ONE bounded-exponential `retry.Ladder` value type (base, cap, bark-at-powers-of-two) used by the settle, the ACK-early continuation and the writeback requeue, each instance naming its reason |
| Q14 | `WRITEBACK_MAX_ATTEMPTS = 4` "fast retries then wrap the counter" with `writeback_retry_exhaustions` counting wraps | A counter that wraps as control flow | Retry forever with the ladder's cap; `writeback_retry_exhaustions` counts units past the ladder's cap (same observability, no wrap) |
| Q15 | `overlay_core::OverlayState::from_u8` decodes an unknown word to `FenceDropped` (`overlay_core.rs:106-117`) | A RAM enum that tolerates impossible values | A Zig `enum(u8)` — an impossible value is a tripwire, never a disposition |

When a wave meets a new quirk it adds a row to decision 0002 before diverging.

---

## 14. Port shape (where this lands in rung 4)

- `src/data/write_path.zig` — the handler-side ladder of §1 (`admitPiece`), the ONE
  ledger family, the per-piece future on the lane; takes `io: std.Io` only for the
  depth-governor park and the settle waits (hot DMA submission never does).
- `src/data/overlay_record.zig` (+ `overlay_record_model.zig`) — §3.2 as a plain
  value type over `core/coverage.zig`; the Dekker pair with SeqCst accesses and the
  freeze/drain + torn-serve models with their weakenings.
- `src/data/coverage.zig` — §5.2, dependency-free, shared by the buffer and the record.
- `src/data/block_buf.zig` — §5.1 over a 4 KiB-aligned pool from `mem/`; CoW by
  explicit refcount (`acquire`/`release`/`uniqueOrCopy`) with the CoW model.
- `src/data/patch_clone.zig` (+ model) — §4; the incarnation word and the refcount
  word live in the rung-4 allocator; `crossWordFence` is a SeqCst RMW on each word.
- `src/data/write_pipeline.zig` (+ `admission_model.zig`, `probe_model.zig`) — §9.
- `src/data/rewrite_epoch.zig` — §5.4 under the (3.5) stripe from `meta/dlm.zig`'s mix.
- `src/data/extent_overlay.zig` + `staging/extent_record.zig` (`extern struct`, xxh3) — §6.
- `src/data/writeback.zig` — §7 on the `sqz-meta` pool; `StagedDmaSource` as a
  non-copyable struct consumed by value (`writeBlockFromStaging(source)` takes it by
  value; Zig's move semantics are the compile-fail).
- `src/data/fsync.zig` — §10; `FsyncProf` as a value on the handler frame;
  `TouchedTable` over `dlm.splitmix64` at the DLM width.
- Every must-stay-0 above is a `counters.Tripwire`; every closure identity a test in
  `tests/rails.zig`'s style (`Σ phases ≡ total`, `patched + Σ buckets ≡ striped writes`,
  `admit_write_* ≡ user bytes`, `overlay_store_bytes ≈ user bytes` on the eligible row).
