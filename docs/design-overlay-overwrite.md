# Design: Device-Overlay B4 — the OVERWRITE arm (zero-copy CoW for mapped-block writes)

| | |
|---|---|
| **Title** | B4 — aligned overwrites of mapped striped blocks ride the zero-copy slot→device overlay: fresh-dest CoW displacement, epoch-fed publication, manners-law reclaim of the displaced binding |
| **Status** | **Draft** — design only; extends `docs/design-device-overlay.md` Rev 2 (the parent design; its §8 ladder names this rung B4 and its Rev-2 correction B mandates the coexistence decision land here) |
| **Date** | 2026-08-14 |
| **Author** | _(assign on review)_ |
| **Branch** | `docs/design-overlay-b4` off `dev` (`f9f72fe1`) |
| **Parent design** | `docs/design-device-overlay.md` Rev 2 — the nine laws, KD-OV-1..14, the B1–B9 ladder. B1/B2 landed (`.benchmarks/2026-08-09-device-overlay-b1-b2.md`); the ACK-early accelerator (kernel 0029, `docs/design-zc-write-kernel-v2.md` §3) landed and flipped `SQUEEZEFS_DEVICE_OVERLAY` **default ON** (`src/env_knobs.rs:145` — "the 35 GB/s A-leg, not an opt-in"); B3 read composition is live (`try_serve_overlay_read`, `src/fuse_client.rs:13623`) |
| **Governing conviction** | 2026-08-14 field row (byte-exact closure, §2 below): the overlay serves **only first touches**; 96.6 % of a sustained overwrite ingest row rides the full-copy merge path |
| **Inviolable contracts** | AGENTS.md non-negotiables (io_uring-first; zero-copy + latch-free hot paths; no dead code; resource-derived tunables, allocations **round UP** per the 2026-08-14 doctrine); lock order P1-9/P1-10 + RES-1; the D0 guard + RES-6/S7 DMA authorization; v3 whole-tx atomicity (one publish = one checksummed journal entry); FIND-M11-A supersession (stale fencing tokens discard staged work — the remount contract; acked live custody is never dropped except by the fence); never-lossy acked data; the generic/209 read-freshness contract; the parent design's nine laws verbatim |

---

## 1. Overview

The device-backed visible overlay (Approach B) stores an eligible FUSE
WRITE's payload **directly from the transport's sparse slot to an
unpublished device offset** — zero daemon copies — and ACKs early
against retained pages (kernel 0029). Today that path is gated to
**fresh/hole blocks only**: `try_device_overlay_store` declines the
moment the target block has a durable mapping
(`src/fuse_client.rs:12845-12848` — `if bm.contains_key(&b) { // An old
binding exists — the overwrite shape is PR B4. return Ok(false); }`),
and the HOLD gate declines identically at delivery
(`src/fuse_client.rs:12781`), so every overwrite of a mapped block falls
back to the accumulation path: slot → extraction bounce → NT merge into
`ActiveBlockBuf` (**one full CPU pass per byte**, `nt_copy_bytes`) →
write-through DMA.

B4 removes that gate for qualifying shapes. An aligned overwrite of a
mapped striped block:

1. mints a **fresh** destination block (never in-place — §5.1),
2. rides the identical slot→device `WRITE_FIXED` + ACK-early machinery,
3. **publishes the remap by feeding the rewrite epoch**
   (`DataRouter::rewrite_shadow_record`, `src/routing.rs:9755`) — the
   coexistence decision the parent design's Rev-2 correction B requires:
   arm (a), ONE pending-binding authority (§5.4),
4. and sends the displaced old binding through the epoch's deferred-free
   law → `free_block` → discard-elision / background reclaim queue
   (`src/block_reclaim.rs`) under the manners law — exactly the
   displacement dance the rewrite program and the CoW write-through
   already perform.

No new on-disk format, no new copies, no new blocking locks, one new
bool knob. The acceptance instrument is the field row's **96.6 % merge
share collapsing to ≈ 0** on the eligible shape.

## 2. Background & Motivation

### 2.1 The field conviction (2026-08-14, byte-exact closure)

60 s sustained 4 MiB sequential-overwrite ingest on the production field
client (32-CPU, 2×200GbE, nvme-tcp; hybrid IL — large ops on the kernel
lane): fio `io=1864 GiB` at **31.1 GiB/s**. Stats-inode deltas close
byte-exactly:

| Counter | Delta | Meaning |
|---|---|---|
| `overlay_ack_early_bytes` | **64 GiB** = 16,376 stores × 4 MiB | EXACTLY one pass over the 8 × 8 GiB fileset — the overlay served only the FIRST touch of every block |
| `nt_copy_bytes` | **1800 GiB** | every overwrite byte paid the lease→`ActiveBlockBuf` NT merge — a ~30 GB/s memcpy stream burning whole cores |
| `write_through_bytes` | **1800 GiB** | the same bytes then rode the pooled write-through DMA |

64 / 1864 = 3.4 % overlay share; **96.6 % of the row's bytes declined
into the merge path at the mapped-block gate**. The zero-copy conversion
target is therefore not a micro-optimization: it deletes one full
daemon CPU pass (plus the extraction bounce on non-retained shapes) over
~97 % of a sustained overwrite ingest row. Per the efficiency doctrine
(the `a7029111` spin-governor precedent), even a throughput **wash**
ships ON when CPU/byte drops this much — here the memcpy stream alone is
the price of several cores at 31 GiB/s.

### 2.2 Why the gate exists today (what B4 must not break)

The fresh/hole gate was PR B2's deliberate scope fence (parent §8):
overwrites introduce every hard problem at once —

* a **displaced old binding** whose free must be ordered against the
  remap publish (parent law 8) and against ACK-early acked custody;
* a **second pending-binding authority**: the rewrite shadow epoch is
  default ON (`SQUEEZEFS_REWRITE_SHADOW`, `src/env_knobs.rs:146`) and
  already owns rewrite displacement on the merge path
  (`upload_block_publish_phase`, `src/fuse_client.rs:15655-15701`) — the
  parent's five dual-authority hazards (double park, double free,
  KD-1.11 resurrection, competing fsync save, ref-delta drift);
* **warm read tiers holding the OLD bytes** (a fresh block has no tier
  population; a mapped block has hot-block entries, read-lane holds,
  NVMe read-cache entries and IL hold-probe serves that all become
  stale-capable the moment covered ranges live at the dest);
* the **W1 sole-owner patch** (`try_sole_owner_patch`,
  `src/fuse_client.rs:12378`) mutating the OLD offset in place while an
  overlay displaces it — a lost-update shape that is unreachable today
  only because fresh blocks are patch-ineligible (`unmapped`).

B4's job is to graft the overwrite shape onto the overlay's ACK-early
continuation while resolving each of these with existing, proven
machinery — never a second discipline.

## 3. Goals & Non-Goals

### Goals

1. Aligned single-block overwrites (and the aligned interior segments of
   big sequential overwrite streams) of mapped, authoritatively-striped,
   passthrough blocks ride the zero-copy slot→device overlay with
   ACK-early — daemon copies on the eligible shape: **0**.
2. Publication reuses the rewrite epoch (arm (a)) — one pending-binding
   authority, one durable-publish primitive, the §5.2 deferred-free law
   and crash-window table transferring verbatim.
3. The displaced binding's reclaim rides `free_block` → discard elision /
   background reclaim queue under the manners law — zero mid-row
   discards on the acceptance row, `rewrite_amp ≤ 1.05`.
4. Durable block refs stay one-transaction (parent law 7): the displaced
   ref Delete + fresh ref Put ride the SAME publish KvTx via the
   existing `pending_block_refs` deferred-op accumulator
   (`src/routing.rs:3526`, noted by `rewrite_shadow_record` at
   `:9848-9856`).
5. Every fast read arm either composes correctly over an open overwrite
   overlay or demotes (KD-OV-13 made live for the mapped population).

### Non-Goals (explicit, with owners)

* **In-place overwrite.** Never. Sub-block in-place is W1's separately
  fenced territory (design-random-small-writes §5.1, refcount-1 +
  `fence(SeqCst)` protocol); whole-block in-place is Idea 7's
  substrate-probed program (design-rewrite-program §9.2) and the
  `SQUEEZEFS_INPLACE_OVERWRITE` lever's — the CoW/crash posture forbids
  overwriting live mapped bytes under ACK-early (a crash mid-DMA would
  tear durable data the map still names: a never-lossy violation, not a
  perf trade).
* **Multi-block request spans.** `try_overlay` keeps
  `start_block == end_block` (`src/fuse_client.rs:13818`); a slot cannot
  be sliced across per-block futures (`deferred_slot`, `:13832`).
  Per-block bytes-vehicle spans are a named P2 follow-on (§14 OQ-1).
* **Transformed volumes** — excluded by the parent §7 impossibility
  argument (frame math), unchanged.
* **Co-writer / multi-writer stores** — PR B9's, after S9 arms. The
  install keeps the D0 single-writer gate.
* **Unaligned edges** — they ride accumulation (the one-authority settle
  screen publishes the overlay first, §5.9); the phase-2 torn-edge
  contract is not this design's.
* **Write-verification mounts** — decline, verbatim from B2 (the pooled
  path's window-exact read-back is the verifier).

## 4. Current state (code anchors the design builds on)

| Machinery | Anchor | Role in B4 |
|---|---|---|
| Mapped-block declines (the gates B4 relaxes) | `src/fuse_client.rs:12845` (store), `:12781` (hold), `zc_write_hold_eligible` `:17912-17924` | become the OLD-BINDING arm |
| Install/reserve/claim/store/CQE | `try_device_overlay_store` `:12814-13153`; claims `overlay_core::begin_store` (`src/overlay_core.rs:235`), release-at-CQE (KD-OV-10) | unchanged, vehicle-blind |
| ACK-early continuations (retry-forever, fence-only drop, dead-ring fallback) | `finish_ack_early_store` `:13164`, `finish_ack_early_bytes` `:13318` | unchanged |
| Settle: freeze → await inflight → seed gaps (zeros) → optional barrier → `publish_block` → coalesced merge → teardown | `settle_overlay_block_locked` `:13398-13506` | gains the old-binding seed source + the epoch-feed publish arm |
| Read compose (snapshot/revalidate; gaps = zeros) | `try_serve_overlay_read` `:13623-13712`; `read_begin`/`read_valid` (`src/overlay_core.rs:408/415`) | gains old-binding gap serves |
| The rewrite epoch (park displaced, ref-delta note, coverage close, §5.2 deferred free, W1–W6 crash windows, KD-1.8 fence, KD-1.11 supersession) | `rewrite_shadow_record` `src/routing.rs:9755-9877`; `close_rewrite_epoch` `:9957` (collect-under-lock, free-outside — the RES-1 pattern); `supersede_shadow_bindings` `:9896` | **the publication authority** (arm (a)) |
| The merge-path rewrite arm (the template B4 mirrors) | `upload_block_publish_phase` `src/fuse_client.rs:15633-15759` | shadow-feed vs durable-merge split, SLO attribution, displaced frees after publish |
| fsync ordering (overlay drain barrier=true BEFORE epoch close) | `flush_inode_to_backend` `:15839` — drain at `:15849`, `close_rewrite_epoch` at `:15900` | already correct for arm (a) |
| Displaced-free ladder | `BlockAllocator::{begin_free:1990, finish_free:2041, free_block:2179}`; `src/block_reclaim.rs` (manners law, park-don't-spill, fence halts); discard-elision debt (design-rewrite-program §3) | law 8's vehicle |
| One-authority accumulation screen | `src/fuse_client.rs:13962-13969` (settle-before-accumulate) + the generic/551 live re-derivation `:13971+` | unchanged; the settle's publish arm changes |
| Teardown dispositions (Published/FenceDropped disarm; Superseded frees) | `src/device_overlay.rs:221-261` | gains the fed-to-epoch disposition |

## 5. Proposed Design

### 5.1 Eligibility — the overwrite arm of the install screen

The §7 state screen in `try_device_overlay_store` splits its mapped
verdict instead of declining:

```
mapped(b) ∧ striped-authority ∧ passthrough ∧ ¬write_verification
        ∧ aligned single-block segment (4 KiB offset+len, one block)
        ∧ len > patch_max_bytes()              — the LENGTH FLOOR (finding 47), BOTH shapes
        ∧ no RAM accumulation (active_block_buffers)         [existing]
        ∧ no staged custody (active_block / active_block_ext) [existing]
        ∧ no live W2 extent overlay on the block              [existing]
        ∧ ¬rewrite_epoch_binds_block(ino, b)   — see below
        ∧ ¬range_shared(block span)            — S11 rung 16, see below
  ⇒ install an OVERWRITE overlay record (old_binding = the mapping string)
```

Load-bearing points, each with its measured or structural reason:

* **The length floor (finding 47, 2026-09-02 — `overlay_length_eligible`,
  `src/fuse_client.rs`; contracts `tests/overlay_length_floor_tests.rs`).**
  A segment is overlay-eligible by length iff `len > patch_max_bytes()`
  — literally the W1 predicate-5 oversize verdict, so the two fast
  paths tile the sub-block population at the derived cap (block_size/8,
  512 KiB on the shipped block): `≤ cap` is the Random-small-write
  program's (W1 in place when eligible, else the W2 byte-budgeted extent
  park + amortized fold), `> cap` is the overlay/accumulation class —
  the 1 MiB+ segment A-leg this design was built for. It applies to
  BOTH shapes (a fresh/hole sub-cap write is W2's too) and to the hold
  gates (a sub-cap slot follows the patch ladder's hold rules or
  extracts at delivery). Why: the shape screen had no minimum and the
  overlay arm runs BEFORE the W2 park, so a sub-cap write the patch
  declined at STATE time (hole / clone-shared / decorated /
  co-writer-refused / stream-adjacent) minted a whole CoW dest per
  touched block, held it Open across the drain interval, fed the
  rewrite epoch per record, and at settle read the whole old image and
  seeded the whole complement as gap runs — the §5.8 falsifier firing
  exactly as written: `.benchmarks/2026-08-17-mw-shipped-free-c8-fix.md`
  (six refused patches ⇒ `overlay_gap_seed_old_bytes` = 6 × 4 MiB −
  24,576, one 4 KiB write per record). Composition with the
  `SQUEEZEFS_PATCH_MAX_BYTES=0` A/B lever: cap 0 empties the W1 class,
  so every length is overlay-eligible — exactly as it makes none
  patch-eligible; the lever keeps meaning "no W1", never "no overlay".
  Counted `overlay_ineligible_sub_cap` (§11). Small-bs SEQUENTIAL
  segments (4–256 KiB, `patch_ineligible_adjacent`) are the same
  population's other face: below the floor they accumulate in the
  `ActiveBlockBuf` into ONE whole-block write-through (request size =
  the block) instead of one overlay store per op (`wareq-sz` collapsing
  to bs).

* **Shared blocks are eligible.** Unlike W1 (refcount == 1 mandatory —
  in-place mutation of a pinned block is corruption), B4 is CoW: the old
  key is displaced, and the epoch close's free is `free_block`'s
  refcount-decrement law (`close_rewrite_epoch` already handles
  clone-shared parked keys decrement-only). No refcount screen.
* **Decorated `bk:off:len` mappings are eligible.** The old binding is a
  gap-composition SOURCE, read through the block-fetch funnel that has
  decoded decorated forms since FIND-RW2-A — never an arithmetic base
  for in-place writes (that was W1's hazard, not ours).
* **`rewrite_epoch_binds_block` declines** (the existing probe at
  `:12849`, kept): a block whose CURRENT binding is a RAM-only shadow
  B key must not overlay — the captured `old_binding` would be an
  unpublished key whose park/free the epoch already owns (hazard 1,
  double park). The write instead rides accumulation, whose shadow
  record supersedes the prior B key inside the epoch (the proven
  same-epoch re-rewrite arm, `src/routing.rs:9816-9819`). This is
  deliberate: overlaying an epoch-bound block is a **re-overwrite within
  one un-fsynced window** — the accumulation path's latest-wins coalesce
  already owns that shape, and the field row (one pass per block per
  window) never hits it. Counted `overlay_ineligible_shadow_bound`; if a
  workload shows it hot, OQ-3 owns the relaxation.
* **The RANGE clause screens BOTH shapes** (DLM S11 rung 16 — KD-MW-12;
  `design-full-multi-writer.md` §9.3 item 3): *a block any live range
  grant does not solely cover is overlay-ineligible*, counted
  `overlay_ineligible_range_shared` — the W1 clause-7 twin
  (`device_overlay::overlay_range_shared`, the same custody core
  `dlm::span_range_shared`, its own ledger bucket). The overlay's
  eventual publish covers EVERY byte of the block (old-binding gap
  composition on the overwrite shape, gap seeding on the fresh shape),
  so a foreign sub-block writer's bytes would be composed away by the
  settle→rewrite-epoch feed; both fast paths closed, the write rides
  the CoW-rewrite + shipped-publish path — the only vehicle whose
  custody/publish laws handle sharing (§9.3's demotion/extent
  machinery, PR row 17). ONE probe at the screen, ahead of the
  registry (guards install AND join): grants are not serialized by the
  3.5 meta section, so an under-lock re-check buys no atomicity — the
  issuance-time closure is rung 17's demotion barrier, and the bit-15
  layout-version gate backstops the crash/late windows. Structurally
  inert on every shipped mount (no live ranges ⇒ one O(1) empty-table
  probe — the KD-MW-12 fast-path tax row's proof); the hold gates stay
  deliberately RANGE-BLIND (a stale TRUE costs one late extraction,
  the documented stale-verdict price).
* **`StorageFull` at the dest mint DECLINES (Ok(false)), never errors.**
  On an overwrite row the parked-A population is exactly the free supply
  the epoch's ENOSPC early-close ladder (KD-1.7) reclaims — the
  accumulation fallback triggers it; a propagated error would fail a
  write that the ladder can serve. Counted `overlay_enospc_declines`.
  Every other mint error stays loud.
* **The hold gates** (`overlay_hold_eligible:12754`,
  `zc_write_hold_eligible:17912`) relax identically — mapped blocks
  become hold-eligible under the same conjuncts — so the slot is
  retained at delivery instead of extracted (the whole point; a stale
  TRUE still only costs one late extraction, the documented posture at
  `:17900-17907`).

**Capture→install atomicity (normative).** The mapping capture and the
registry `install` execute inside **one `INODE_META_LOCKS(ino)` critical
section**, taken under the caller's held `BLOCK_FLUSH_LOCKS(ino, b)` —
the (3) → (3.5) extended order (`src/stripe_locks.rs`), no new edges.
This is the parent §2.1 mandate ("captured under `INODE_META_LOCKS` at
install so it can never be a stale snapshot",
`docs/design-device-overlay.md:134`) and the working precedent is
`rewrite_shadow_record` itself, which does capture-prev + park + binding
insert inside one 3.5 section (`src/routing.rs:9763-9877`). The block
guard alone closes **nothing** against foreign durable merges — the
merge **primitive** never takes (1)/(3) (its lock-order contract,
`src/routing.rs:9722-9727`), and not every caller holds them either:
the mover DOES hold the block guard around its `MergeExpected`
(`src/jobs.rs:2720-2721` — which is why its §5.7 layer-1 screen is the
guard-held quiesce probe), but fsck's `flip_mapping_damaged`
(`src/fsck.rs:3915-3935`) and the conveyor pass do not — and a capture
that dropped 3.5
before installing would let a foreign merge run its entire 3.5 section
(including the §5.7 hook, which can only supersede records it can SEE)
in the gap: the record would then be born with `old_binding` naming a
displaced key whose free is already scheduled — the KD-1.11 resurrection
class at birth. The generic/551 live re-derivation below the install
site is the **RAM-authority-source** precedent (which map to read), not
a locking precedent — it is a lock-free `metadata_cache.peek_with`
(`src/fuse_client.rs:13990-13998`); the capture must not imitate its
lock posture. Red-first (B4c-ii): the install-vs-foreign-merge
interleave, driven through a stall seam, asserts a record can never be
born stale.

### 5.2 The store path — unchanged, vehicle-blind

Install → law-2 reserve (`allocate_block` + `InflightAllocGuard` +
`MintedBlockGuard`) → §2.3 range claim → slot `WRITE_FIXED` /
COMMIT_RETAIN ACK-early / O_DIRECT snapshot / pooled Bytes vehicle →
CQE-anchored coverage — all verbatim (`:12879-13153`). The overwrite arm
adds **zero** submission-side mechanics. `authorize_zc_store` remains
the single RES-6/S7 authorization point per submission;
`fence_epoch`/fencing capture at install unchanged.

### 5.3 ACK-early semantics vs displacement ordering — the crash-window enumeration

The parent design's question 1, answered by construction. The ordering
laws, then the windows (the design-cow-kv §5.5.2b discipline):

**Ordering laws (normative):**

| # | Law | Enforcement |
|---|---|---|
| O1 | **ACK before DMA-complete never implies publication.** ACK-early replies at install+claim (record reader-visible first — law 1); coverage publishes per-range only at the real CQE (law 3, `complete_store_and_wake`); the REMAP is recorded only at **coverage completion** (all CQEs in) — so no map, RAM or durable, ever names a dest range whose bytes have not landed. | `overlay_core` coverage gate; the feed runs from the coverage-complete verdict only |
| O2 | **The remap publishes only DMA-complete bytes; the data BARRIER is guaranteed on the fsync-class boundaries only.** The RAM-only epoch record is not durability. On the fsync leg the full §6.2 order runs: COMPLETE (await CQEs) → SEED → **FLUSH (data barrier, `flush_data_devices` at `src/fuse_client.rs:15893`)** → PUBLISH → META SYNC (`flush_inode_to_backend:15849` drains overlays barrier=true strictly before `close_rewrite_epoch:15900` — DUR-1 by construction). **Three other real save triggers run NO data barrier** — the KD-1.6 coverage-triggered close (`src/fuse_client.rs:14798-14804`, `:15526-15535`), the 5 s idle epoch sweeper (`src/routing.rs:10069-10095`), and any mid-epoch conveyor save of the ino's dirty layout (the feed leaves `layout_dirty = true`, `src/routing.rs:9837`) — the DUR-2 acknowledged-volatility class **inherited verbatim from the shipped rewrite program** (B4 changes nothing here; the shipped shadow swap has the identical window). See OW-8. | fsync leg ordered by construction; non-fsync saves disclosed via `data_volume_write_cache` (DUR-2) |
| O3 | **The displaced old binding is freed only after a durable save that no longer references it** — the §5.2 deferred-free law verbatim; the free rides `free_block` → refcount decrement → `begin_free` → reclaim enqueue → `finish_free`, offsets non-reallocatable until reclaimed, discard elision composing (zero mid-row discards). | `close_rewrite_epoch:9998+` (collect under lock, free outside — RES-1); `src/block_reclaim.rs` pop-ownership |
| O4 | **A fence drops acked custody loudly and frees NOTHING** (`overlay_fence_drops` + KD-1.8's epoch fence: publish nothing, free nothing, successor recovery owns all accounting). | `finish_ack_early_*` per-retry fence check `:13246`; epoch §5.4 |

**Crash windows** (dest plays the epoch's B, `old_binding` plays A —
the design-rewrite-program §5.6 table transfers; the two genuinely new
windows are OW-1/OW-2):

| # | Crash point | Durable state | Recovery outcome |
|---|---|---|---|
| OW-1 | after ACK, store DMA in flight (ACK-early) | map = old, entire; dest bytes partial/garbage | recovery census: dest referenced by no durable map ⇒ free-listed (`unpublished_offsets_recovered`); file reads the **intact old image**. Acked-un-fsynced bytes lost — the writeback-class contract, unchanged and stated. Old binding never torn (nothing ever wrote it). |
| OW-2 | coverage complete, epoch fed, no durable save yet | ≡ OW-1 (the feed is RAM-only) | ≡ W1: dest free-listed, old intact |
| OW-3 | intermediate flush-leg save committed mid-epoch | map = old ⊕ new-partial (whole-tx per save) | ≡ W6: persisted new bindings are real (DMA-**complete** before any feed — O1; **barriered too only on the fsync class** — O2); their displaced old keys are durably unreferenced ⇒ recovery frees; unpersisted blocks ≡ OW-1. Kill-9 semantics exact; power-loss on a write-back namespace ⇒ OW-8 |
| OW-4 | torn publish entry | detected-and-ignored (v3 §4.10) | ≡ OW-1 |
| OW-5 | swap durable, displaced frees not run | map = new; old keys allocated-unreferenced | ≡ W3: recovery free-lists them — never a leak, never a double free (the terminal free's durable effect IS the ref Delete that rode the publish) |
| OW-6 | fenced pre-publish | map = old (save refused) | ≡ W5: zombie freed nothing (fence-halt latch covers the reclaimer too), published nothing; successor ≡ OW-1 |
| OW-7 | mid-fsync between data barrier and meta publish | dest durable, unnamed | ≡ OW-1 (durably written ≠ durably named) |
| OW-8 | **POWER LOSS** (not kill-9) after an **unbarriered** non-fsync save (coverage close / idle sweeper / conveyor side-save) on a `write-back`-class data namespace | map = new durably; dest bytes possibly still in the device's volatile cache; old key durably unreferenced (recovery frees it per OW-5) | the range can read dest residue — neither the new bytes nor the old image. **Inherited verbatim from the shipped rewrite program** (the shadow swap's non-fsync closes have the identical window); it is the DUR-2 acknowledged class: un-fsynced acked writes carry no power-loss guarantee, and `data_volume_write_cache` is the operator disclosure. fsync remains the contract point and is fully ordered (O2). The stronger posture — a `flush()` before any coverage/idle close of an overlay-fed epoch — is OQ-5, priceable in B4d, deliberately NOT taken by default (it would change the shipped durability class of the whole rewrite path, not just B4's) |

The two FORBIDDEN orderings the task names are unrepresentable by
construction: *publish-before-DMA-complete* (would make a crash resolve
the map to garbage dest bytes — corruption, not just loss) is excluded
by O1 — the feed runs only from the coverage-complete verdict, so no
save, on any trigger, can ever persist a binding for un-landed dest
bytes; *free-before-publish* (a durable map naming a freed, possibly
reallocated block — cross-file corruption) is excluded by O3, and there
is no intermediate durable free state to get wrong (the durable-block-
refcounts law: the terminal free's effect is the Delete riding the
publish tx). What is NOT excluded by construction is device-cache
volatility behind a non-fsync save — OW-8, the inherited DUR-2 class.
The B4c-ii crash matrix is therefore stated in two halves: the **kill-9**
matrix pins OW-1..OW-7 exactly (process death cannot un-land a CQE'd
DMA); the **power-loss** claim is OW-8's, and it is **pinnable in cargo
today** — the TEST-1 data-device power-cut harness exists in-tree
(`src/dev_power_cut.rs`: the `NvmeBlockDev`-worker fault seam that
journals uncovered writes and reverts everything a `flush` barrier did
not cover; consumers `tests/data_device_power_cut_tests.rs`). B4c-ii
therefore carries `ow8_window_is_exactly_as_disclosed` — arm the cut,
drive a coverage-triggered (unbarriered) close, `power_cut()`, remount:
the range reads dest residue, exactly as OW-8 discloses — a red
*documentation pin* that catches any future accidental barrier-order
change; its **OQ-5 green twin** (the same test behind the
barrier-before-close lever goes green) lands with it so B4d's pricing
leg arrives with its correctness half already written. Kill-9 greens
still never adjudicate OW-8.

### 5.4 Publication: coexistence arm (a) — the overlay FEEDS the rewrite epoch

**The Rev-2 correction-B decision, made:** completed overwrite overlays
feed `rewrite_shadow_record()` — the overlay dest key becomes the
epoch's B key and **the epoch owns publication, the displaced park, the
deferred free, the fencing law and every crash window**. Arm (b)
(overlay-owned per-block durable publication + inline displaced free)
is rejected as the default because it forks a second pending-binding
authority (all five hazards live), loses the swap economy (one durable
save per epoch vs one per block), and would re-derive crash windows the
epoch already proved — but it survives structurally as the
`SQUEEZEFS_REWRITE_SHADOW=0` degenerate (below), so the A/B lever is
free.

The settle path splits exactly where `upload_block_publish_phase` does
(`:15655` — the template, mirrored verbatim):

```
settle_overlay_block_locked(ino, b, barrier):
  freeze → await inflight → seed gaps (§5.9) → [barrier: device flush]
  → rec.allocator.publish_block(rec.dest_offset)      (incarnation stable)
  → purge new_key read tiers                          (recycled-key hygiene)
  → IF overwrite-record ∧ rewrite_shadow_enabled():
        take fsck_guard out of the record
        match rewrite_shadow_record(ino, b, new_key, min_size, fsck_guard):
          Shadowed { displaced_prev, coverage_complete }:
             SLO attribution (§8.2); mint_owner.disarm()   [ownership → epoch]
             rec.core.mark_fed(); retire record            [the Fed terminal, §5.4a]
             return CloseOwed(coverage_complete)           [venue: the CALLER,
                                                            after the guard drops]
          NotShadowed(guard):    [lever off / not displacing — degenerate arm]
             put guard back; fall through ↓
    ELSE (fresh records, and the shadow-off degenerate):
        merge_block_mappings_coalesced(…,
          overlay_self_publish=(b, record identity))    [today's path, :13466;
                                                          the §5.7 Merge-class
                                                          self-publish exemption
                                                          rides the op]
        displaced := returned keys
        mark_published; teardown; THEN free displaced   [after publish + after
                                                          the meta guard — the
                                                          upload path's order,
                                                          :15750-15758]
```

**Epoch-close venue (normative — the tree's two precedents disagree and
this design picks the pipeline's):** `settle_overlay_block_locked` NEVER
calls `close_rewrite_epoch` under the caller's held block guard; it
returns a close-owed verdict and the guard-dropping caller runs the
close — the detached publisher (`drain_device_overlay_block`, `:13532` —
settle, `drop(block_guard)`, then close), the handler-ladder settle
screen (close spawned detached after its guard drop), and the fsync leg
(which already closes outside any block guard, `:15900`).
**`CloseOwed` is a latency/economy hint, never a correctness
obligation — any venue may drop it.** The fed epoch stays registered
and never-lossy regardless; the 5 s idle sweeper
(`src/routing.rs:10069-10095`) and the next fsync are the backstops
(the epoch's re-register law). The DELIBERATE droppers, enumerated so
the signature change needs no re-derivation at review: the two
in-store settles inside `try_device_overlay_store` (the steal-work arm
`:12870-12876` and the claim-conflict arm `:12936-12941` — both decline
into other paths whose own durability boundaries follow) and the four
shape-change entry drains through `drain_device_overlays_for_ino`
(truncate `:20388`, unlink `:20789`, clone `:21243`, punch `:22067`).
The entry drains' safety rationale is the **hint law itself + the
idle-sweeper/fsync backstops** — NOT a per-op close trigger: only clone
is a KD-1.6 close trigger in its own right; truncate/punch PRUNE the
epoch's bindings rather than close it (`supersede_shadow_bindings_from`
/ the `RemoveBlocks` arm, `src/routing.rs:10273/:10290`), and unlink
has no close trigger at all. Only the three venues above ACT on the
hint, because they are the latency-relevant ones. This is the
write-pipeline posture, verbatim: "the epoch close below stays OUTSIDE
the guard (it takes (3.5) and frees — the RES-1 posture)"
(`src/fuse_client.rs:14789-14801`). The contrary precedent —
`upload_full_block_sized` closing under the guard with a defending
comment (`:15526-15533`) — is deliberately NOT followed: the close is
the self-described "worst park amplifier in the tree"
(`src/routing.rs:10001-10008` — one displaced-key free per rewritten
block of the WHOLE ino's epoch, each free able to cap-park up to
`SQUEEZEFS_RECLAIM_CAP_PARK_MS` = 1000 ms at the reclaim cap), and on
the governing acceptance row coverage closes fire constantly with the
reclaim queue hot (R6's own scenario) — holding the block guard every
reader's serialized-settle arm and every same-block writer needs across
that is a designed-in stall, legal under 3→3.5 ascending order but
against the RES-1 intent the pipeline comment states.

### 5.4a The Fed terminal state

The fed record transitions to a **distinct `OverlayState::Fed` terminal
state** (not a reuse of `Published`): `overlay_core`'s transition set,
the loom models and `run_teardown_disposition`'s match each grow one
arm, and B4a's exhaustive-table pin binds to it. Its disposition is
disarm-without-free (the `Published` arm's body — ownership transferred
to the epoch, KD-B4-3), but the state stays distinct because (a) the
`overlay_publishes` (durable) vs `overlay_epoch_feeds` (RAM-only)
accounting split must be readable off the record, and (b) a fed record
observed after a fenced epoch is a debugging fact `Published` would
erase. Ordering sentence the disposition depends on: the feed takes the
`fsck_guard` OUT of the record BEFORE retire, so the teardown's
`fsck_guard = None` clear (`src/device_overlay.rs:257-260`) clears an
already-empty slot — C2/C3 visibility is continuous (record → epoch)
with no uncovered instant.

Why the guard/ownership transfer is sound (parent law 9 restated for the
feed): `rewrite_shadow_record` takes the `InflightAllocGuard` and parks
it in `epoch.guards` (`src/routing.rs:9865`) — fsck C2/C3 visibility is
continuous. The `MintedBlockGuard` disarms at the feed because
**ownership transfers to the epoch**, whose dispositions are exactly the
mint owner's: swap-durable ⇒ B published (disarm-equivalent);
fence/close-fail ⇒ free NOTHING, successor recovery owns (the W5
posture the record's own `FenceDropped` disposition already implements,
`src/device_overlay.rs:234-241`). The fed record's terminal is the
distinct `OverlayState::Fed` state (§5.4a) with a disarm-without-free
disposition — so `run_teardown_disposition`'s
`overlay_teardown_nonterminal` tripwire never fires on a feed, and the
fed-vs-durably-published distinction stays readable.

The five dual-authority hazards, discharged one line each (each still
gets its red-first pin, §PR Plan):

| Hazard | Why arm (a) closes it |
|---|---|
| 1 — two displaced-old queues | only the epoch ever parks (the overlay never records a park; `old_binding` is a read-composition source, not custody) |
| 2 — duplicate deferred frees | only the epoch close frees; the overlay's terminal dispositions free only the UNPUBLISHED dest (Superseded), never the old key |
| 3 — KD-1.11 resurrection (the poison class the KD-1.9 compose can carry) | the feed IS a shadow record — the KD-1.9 compose belt and the KD-1.11 `supersede_shadow_bindings` coherence see it natively; no second map to resurrect from |
| 4 — competing fsync publication | one authority: fsync drains overlays INTO the epoch (`:15849`) then closes it (`:15900`) — one save |
| 5 — duplicate/missing ref ops | `rewrite_shadow_record` notes (Delete old, Put new) into `pending_block_refs` exactly once (`:9848-9856`); the note drains into the SAME KvTx as whichever save publishes the map — parent law 7 holds with **zero added commits** (the journal-entry-equality pin transfers) |

**The degenerate arm** (`SQUEEZEFS_REWRITE_SHADOW=0`): `NotShadowed`
falls to the coalesced durable merge, which returns the displaced keys;
frees run strictly after the publish and outside the 3.5 guard —
byte-for-byte the upload path's non-shadow arm (`:15723-15758`). This
keeps the shadow A/B lever meaningful on overlay rows and is the only
place the overlay itself ever frees an old binding.

### 5.5 Interaction with W1 patches and the §5.1 fence

Threat: a sub-block W1 patch DMAs into the **old** offset in place
while a B4 overlay holds covered ranges at the dest; the eventual
remap publish then discards the patch bytes — a silent lost update.
Reachable only in B4 (fresh blocks are `patch_ineligible_unmapped`).

Resolution — **serialization plus one predicate clause, no new fence**:

* Both `try_sole_owner_patch` and `try_device_overlay_store` already run
  under the same `BLOCK_FLUSH_LOCKS(ino, b)` guard (the handler ladder,
  `:13884-13956`), so patch-vs-install/settle cannot interleave — the
  hazard is only ever a patch arriving while a record from a PRIOR write
  is still Open/Frozen (ACK-early keeps records alive past their ACK).
* The patch predicate gains **clause 8** — clause 7 is already taken by
  the DLM S11 byte-range-custody screen (`patch_ineligible_range_shared`,
  declared at `src/fuse_client.rs:5559-5561`, enforced `:12388-12403`):
  **a live `device_overlays.get(ino, b)` record ⇒ decline**, counted in
  a new `patch_ineligible_device_overlay` bucket (the existing
  `patch_ineligible_overlay` at `src/fuse_client.rs:5556` stays the
  W2-extent bucket — the ledgers must not merge, predicate-rot detection
  depends on the split). The declined write then falls through the
  handler ladder: if overlay-eligible it joins/settles the record; else
  the one-authority screen settles first (`:13962`) and accumulation
  proceeds on the published new binding. No ordering in which the patch
  bytes can be silently displaced remains.
* The reverse direction (overlay install while a patch is mid-DMA) is
  excluded by the shared guard. The §5.1 clone/patch `fence(SeqCst)`
  protocol is untouched: B4 never mutates a mapped offset, so the
  post-map-immutability assumption W1 violated (and fenced) is not
  re-violated here — **no new lock-free protocol ⇒ no new loom model
  beyond the overlay-core transition re-runs** (§PR Plan B4a). Clone
  (CFR) composes as today: clone is an epoch close trigger (KD-1.6) and
  a drain-or-supersede shape-change op for overlays
  (`drain_device_overlays_for_ino` at `:21243-21246`), so a clone never
  observes the registry.

### 5.6 Interaction with concurrent reads — composition, tiers, and freed-offset reuse

**(1) Compose gap serves come from the old binding, not zeros.**
`try_serve_overlay_read`'s gap fill (`:13679` — currently `vec![0u8]`
background) becomes, for overwrite records: fetch the uncovered ranges
through the block-fetch funnel against the captured `old_binding` key —
tier serves valid (incarnation-checked, as any serve), decorated forms
decoded, the parent §5.2 snapshot/revalidate loop unchanged (generation
equality + destination identity + `range_inflight` re-check). The
captured key is identity-stable for the record's life (the §5.1
one-section capture rule makes it true at birth; the §5.7 class-split
hook keeps it true for life), so it needs no revalidation word of its
own. A
gap-fetch failure rides the existing rebind ladder
(`stale_binding_rebinds` → serialized settle), never EIO for legal
churn. The §5.3 failed-direct-read law (whole destination rewritten on
revalidation failure) applies verbatim — and matters MORE here, since a
retried serve may legitimately re-source a range from dest instead of
old.

**(2) Tier precedence (KD-OV-13) goes live.** Old-binding tier entries
are PRESERVED while the record is open (they are the gap source — they
are immutable CoW content) and purged at displacement: for arm (a) that
is the feed instant, where `rewrite_shadow_record` already purges the
displaced key's tiers under `INODE_META_LOCKS` (`:9819`). Every fast
serve arm that cannot run the precedence check **demotes**: the
`overlay_open == 0` relaxed-gauge fast path (`any_open_fast`,
`src/device_overlay.rs:166`) keeps the no-overlay fleet at literally one
load; with a live record, the hot-block fast hit, the read-lane hold
serve, the IPC §5.5.1 sync legs (staging/hot/hold/read-cache) and the
read dest-lease arm each run the exact `(ino, block)` probe or hand off
to the composed path. The B2 campaign already screened the IPC
direct-drive probe (`.benchmarks/2026-08-09-device-overlay-b1-b2.md:110`);
B4's red suite must prove the remaining arms per-arm on a **warm mapped
fixture** (write → read until tier-hot → overlay-overwrite → read: the
stale bytes are RIGHT THERE — this is B4's highest-severity risk, R1).
The dest-lease arm keeps the parent §5.3 v1 gate (serves only
registry-proven overlay-free ranges) until B7.

**(3) Freed-offset reuse timing — verified sufficient, one composition
note.** A reader still resolving the OLD key when the epoch close frees
it is today's displacement race, not a new one: (i) `free_block` retires
the incarnation word, so validated tier fills fail their seqlock
re-check; (ii) the read path's bounded-rebind → serialized-settle ladder
re-resolves through the published map; (iii) the offset is
non-reallocatable until the background reclaimer's `finish_free`
(pop-ownership, `src/block_reclaim.rs`), so a straggler device read of a
freed-not-yet-reclaimed offset returns stale-but-owned bytes that the
incarnation check then refuses — never another file's; (iv) armed
membership planes add the §6.8-item-3 free-grace ring downstream of
`finish_free`, untouched. The one B4-specific obligation: the compose
gap serve must complete its snapshot/revalidate cycle BEFORE the feed
can free old — guaranteed because the free is downstream of a durable
save that happens at epoch close, while the compose runs against a live
record; a compose racing the feed fails destination-identity/record-gone
revalidation and re-resolves through the (now updated) map
(`:13704-13712`, the record-gone = published rule). Red pin: a
compose-vs-close storm with the reclaim queue forced hot.

### 5.7 Old-binding staleness under foreign durable merges — the KD-1.11 extension, split by merge class

A durable map mutation of an overlaid index by a path that does not run
the settle screen would strand `old_binding` pointing at a key the
durable path displaced and will free: gap serves would then read a freed
offset (the KD-1.11 resurrection class, one level down). The rewrite
program solved this with `supersede_shadow_bindings` invoked from the
durable primitive's op arms under the same `INODE_META_LOCKS` section
that mutates the map (`src/routing.rs:9896`; sites at
`:10209/:10258/:10273/:10290` + the conveyor pass). B4 extends the SAME
hook sites with `overlay_foreign_merge_hook(ino, op-class, touched)` —
**but the disposition splits by merge class, because the classes carry
opposite custody semantics**:

* **`MergeExpected` (content-preserving — VL4 `move_one`, VL5b
  migration, defrag D2): SKIP-APPLY, never supersede — in TWO layers.**
  A mover copies the OLD content and republishes; it carries **no new
  user bytes** — superseding the overlay here would drop ACK-early acked
  dest custody (the `Superseded` teardown disposition frees the dest,
  `src/device_overlay.rs:242-244/:256`) and durably replace it with a
  copy of the old image, with no fence: a **never-lossy violation**
  (only the D0 fence may drop acked custody — FIND-M11-A). Note the
  asymmetry that makes skip the RIGHT disposition: the shadow is
  structurally immune to this exact shape because
  `rewrite_shadow_record` flips the RAM map at record time, so a mover's
  `expected` no longer matches and the apply **skips** ("a skipped entry
  means the MOVER's view is stale, not the epoch's" —
  `src/routing.rs:10236-10247`); the overlay's Open→feed window has no
  map flip, so `cur == expected == old` MATCHES and would apply.
  **Layer 1 (primary — the quiesce probe):** `move_one`'s final
  check-and-move already takes `BLOCK_FLUSH_LOCKS(ino, block)` around
  its `MergeExpected` (`src/jobs.rs:2720-2721`, "lattice 3") and gates
  it on the **mover quiesce probe** (`(ctx.quiesce)(ino, b)`,
  `:2722-2728`), whose definition is exactly the live-custody screen
  (`fuse_client.rs:7889-7903`: RAM `ActiveBlockBuf` / staged
  `active_block:` / spilled `active_block_ext:` — the overlay registry
  is the one custody form it predates). The overlay arm **joins the
  probe**: `device_overlays.get(ino, b)` non-terminal ⇒ not quiescent —
  and because both installs and the probe run under the same held block
  guard, the check is race-free, and the EXISTING deferral machinery
  does everything this section needs verbatim: the entry defers
  (`evacuate_deferred_staged_blocks`), the raised dst reference is
  released (`src/jobs.rs:2777-2780`), the mover's own source pin unwinds
  last (`:2787`), and the re-plan revisits (by which time the record has
  fed and the ordinary expected-mismatch skip takes over). Zero new
  machinery in the primitive for the mover class; `overlay_mover_skips`
  counts at the probe.
  **Layer 2 (belt — the primitive-level skip):** for `MergeExpected`
  callers that do NOT hold the block guard, the primitive's
  `MergeExpected` arm skips entries whose index carries an open/frozen
  overlay record (counted `overlay_mover_skips` too). Exactly one such
  caller exists today and is named: **fsck's `flip_mapping_damaged`**
  (the C7/C2 `damaged:` quarantine flip, `src/fsck.rs:3915-3935`), which
  rides `MergeExpected` guard-less. The skip composes correctly with its
  contract by construction, not luck-left-unexamined: a skipped entry
  returns no displaced match ⇒ `Ok(false)` ⇒ **the repair refuses the
  action** — its own documented supersession-safety arm ("`false` =
  superseded, the caller refuses") — and a block under a live overlay is
  about to be displaced anyway, so refusing the quarantine flip is the
  correct verdict, pinned red-first (B4c-i).
  Rejected alternative — apply and re-capture `old_binding` to the moved
  key — is correct but buys nothing over the skip (the mover revisits
  anyway) at the price of a mutable capture, which §5.6's
  no-revalidation-word argument depends on being immutable.
* **`TruncateFrom` / `RemoveBlocks` (genuinely discarding): mark
  `Superseded`.** Here the durable op discards the range's data by
  definition — a truncate/punch IS a newer write of nothing — so
  dropping the record's custody is the correct newest-wins outcome.
  These ops also drain-or-supersede at their entries already
  (`:20388/:20789/:21243/:22067` — verified they drain FIRST); the hook
  is the belt for any discarding path that reaches the primitive without
  the entry drain.
* **`Merge` (a foreground durable publish of new bytes): SELF-PUBLISH
  EXEMPT, foreign = tripwire + containment.** The `Merge` class has one
  entirely legitimate, always-firing member the hook MUST exempt: **the
  settle's own publish**. `settle_overlay_block_locked` runs freeze →
  seed → `merge_block_mappings_coalesced` → `mark_published`/`mark_fed`
  → teardown (`src/fuse_client.rs:13398-13500`) — the record is still
  **Frozen and registered** while its own merge runs (retiring it first
  is not an option: the record-gone-⇒-published compose rule,
  `:13704-13712`, would then let readers resolve the still-old map — a
  generic/209 violation), and the coalesced conveyor's apply pass is one
  of the exact hook sites (`src/routing.rs:10628`). An unexempted hook
  would therefore fire on **every healthy overlay publish** — B2
  fresh-arm and the shadow-off degenerate alike — polluting
  `invariant_tripwires`, and its containment `supersede()` would SUCCEED
  from Frozen (`Open | Frozen → Superseded`,
  `src/overlay_core.rs:431-434`), making the settle's teardown run the
  Superseded disposition and **free a destination the merge just durably
  published into the map** — the belt manufacturing the exact KD-1.11
  freed-key-in-a-durable-map corruption it exists to prevent.
  **The exemption is explicit PROVENANCE, never state-based**: the
  settle's merge op (and its `QueuedPublish` conveyor entry) carries an
  `overlay_self_publish: (b, record identity)` marker — record identity
  = the §5.2 destination-identity tuple (backend, offset, guard-birth),
  the same word the read protocol already revalidates on — and the hook
  exempts exactly that `(ino, b)` for exactly that op (per-entry in a
  coalesced batch: sibling entries in the same pass stay screened).
  **The layer split that makes the tuple's uniqueness structural, not
  incidental:** the marker's scope is the **per-ino apply pass** — the
  publish conveyor is per-ino by construction
  (`publish_conveyors: scc::HashMap<ino, ConveyorCore<QueuedPublish>>`,
  `src/routing.rs:3494-3498`; `publish_pass(&self, ino, batch)` applies
  one ino's batch under one `INODE_META_LOCKS(ino)` section,
  `:10508-10527`), so a multi-ino batch never reaches the hook — and
  the tree's ONE multi-ino aggregation, the Lever-B
  `publish_commit_group` grouping (rewrite-publish-drain campaign),
  lives **downstream at the SAVE layer**, after the map-apply where the
  hook and the marker live: it is **marker-blind by construction**. The
  marker is consumed at apply and never serialized into the save/group
  path — a future conveyor-widening refactor re-asks this question
  against a stated invariant, not an incidental one.
  Provenance is race-free because the settle holds the block guard
  across the merge await, and installs require the guard — no second
  record can appear on the block mid-publish; the identity word makes
  the exemption robust even so. Two cheaper fixes are REJECTED with
  reasons: exempting Frozen records generally (a genuinely foreign
  discarding merge racing a Frozen record must still be contained), and
  retiring the record before the merge (the generic/209 window above).
  A `Merge` on an open-overlay index WITHOUT the marker remains what
  rev 2 said: unreachable past the one-authority screen
  (`:13962-13969`) ⇒ `note_invariant_tripwire("overlay_foreign_merge")`
  + mark `Superseded` as containment (the durable map is the authority
  the moment the foreign merge commits; keeping the record alive would
  serve dest bytes the durable authority displaced).

**MARK-only rule (normative).** The hook runs inside the primitive's
`INODE_META_LOCKS` section: it may only CAS the record state
(`supersede()`, `src/overlay_core.rs:432`) and collect the touched
`(ino, b)` set — **never** run `run_teardown_disposition` or await the
in-flight set inline (teardown frees the dest via the mint-guard drop
and waits out stragglers: holding 3.5 across a terminal free is RES-1's
letter, and across an inflight wait is a designed-in stall). Store
continuations observe the terminal verdict at their CQE and release
claims (the existing `CompleteVerdict::Superseded` arm,
`src/fuse_client.rs:13296-13301`). **Retire venue:** the hook returns
the touched set and the primitive's caller, after its 3.5 section exits,
schedules a detached `drain_device_overlay_block` per touched block
(`tpc_spawn_guarded`, the existing detached-publish venue — it takes the
block guard and retires terminal records). Without that, a hook-marked
record would retire only when some later path touched its block, holding
`overlay_open` nonzero and keeping `any_open_fast()`
(`src/device_overlay.rs:166`) true mount-wide indefinitely — the
"no-overlay fleet pays one relaxed load" property would silently degrade
after any truncate-class hook hit.

Red-first (B4c-i):
`settle_publish_never_trips_the_foreign_merge_hook` — fresh arm AND the
shadow-off degenerate publish with the hook armed: `invariant_tripwires`
delta 0, the dest never freed, the map resolves to the dest post-settle,
**and the marker is consumed at the apply layer — never observable in
the save/`publish_commit_group` path** (the layer-split invariant above,
asserted so a conveyor-widening refactor fails this pin, not the field);
`mover_merge_skips_open_overlay` — evacuate/defrag a volume holding a
block with an open **ACK-early** overlay; assert the acked bytes survive
publication (the dest key wins the map), the mover converges by re-plan
through the probe deferral, and no gap serve ever reads a freed key
(the prior draft's `mover_merge_supersedes_overlay` obligation pinned
the WRONG outcome and is retired);
`fsck_damaged_flip_refuses_under_live_overlay` — the layer-2 belt's one
real client: `flip_mapping_damaged` on an overlaid index returns
`Ok(false)` and the repair REFUSES rather than reporting success. Plus:
the truncate-belt shape (a discarding op reaching the primitive
un-drained marks Superseded and the detached retire converges
`overlay_open` to 0).

### 5.8 Gap seeding at settle — old-binding bytes, priced

§6.2 step 3's B4 half: a PARTIAL overwrite overlay at a durability
boundary seeds its uncovered ranges by reading them from `old_binding`
(through the same funnel as §5.6(1) — one pooled read per gap run) and
DMA-ing them to the dest's gap ranges, making every published overlay a
whole block (KD-OV-6 confirmed — the two W2-shaped alternatives stay
rejected on their recorded defeat conditions). This is the one copy a
partial overwrite pays, off the ACK path, priced by
`overlay_gap_seed_bytes` with a new source split
(`overlay_gap_seed_old_bytes` vs the zeros face): ≈ 0 on the field's
sequential shape (kernel 4 MiB segments cover whole blocks), material
growth is the B4 falsifier firing (eligibility narrows — e.g. minimum
covered fraction before install — rather than the seed becoming a
steady-state RMW engine). `min_size` on the fed record: `(b+1) ×
block_size` only when app coverage completed the block, else 0 — the
generic/795 size-never-leads-data floor, verbatim from the fresh arm
(`:13457-13464`).

**The seed-bytes law (W-6, e2e perf audit write board #10, 2026-09-08 —
`.benchmarks/2026-09-08-w6-write-handler-economy.md`).** "One pooled read
per gap run" above described the seed WRITES; the seed's SOURCE was one
whole-image read of `old_binding` per settle whatever the gaps summed to
— a 4 MiB block with one 64 KiB hole read 4 MiB to seed 64 KiB, and
`overlay_gap_seed_old_bytes` (seeded bytes) never showed it. The law now
in force: on a passthrough volume with an UNDECORATED old binding, the K
bytes a gap needs are sourced by ONE ranged device read of exactly K
bytes (`DataRouter::read_nvme_block_old_image_range` over the read
path's `read_block_range`; gaps are OVERLAY_PAGE-aligned, so the window
is already LBA-aligned), with the whole funnel's short-read tolerance (a
tail past the backing is holes ⇒ zeros). The whole-image read survives
exactly where it is cheaper (Σ gaps at or past the block window) or the
only correct form (a decorated `bk:off:len` binding, whose short-image
prefix discipline the whole funnel owns; transformed volumes never reach
the overlay by the §5.1 shape screen, and the eligibility probe's
passthrough clause is the belt). `SQUEEZEFS_GAP_SEED_RANGED=0` is the A/B
control (the whole read, byte-identical seeds). Instruments:
`overlay_gap_seed_read_bytes` — device bytes READ per seed, the
amplification numerator (= the block window per old-sourced settle on
the control arm) — and `overlay_gap_seed_ranged_bytes` ⊆
`overlay_gap_seed_old_bytes`, the lever's engagement. Contracts:
`tests/overlay_gap_seed_ranged_tests.rs`.

### 5.9 Sequence (the common case: 4 MiB aligned overwrite, ACK-early)

```mermaid
sequenceDiagram
    participant K as kernel (fuse-over-uring, 0024/0029)
    participant H as write handler (BLOCK_FLUSH_LOCKS held)
    participant OV as DeviceOverlay record
    participant A as BlockAllocator
    participant D as NVMe (zc_write_fd)
    participant E as RewriteEpoch
    participant R as reclaim queue

    K->>H: WRITE (slot held, ITER_SOURCE)
    H->>H: W1 patch probe: declines (live-overlay clause / oversize)
    H->>A: allocate_block() -> fresh dest (law 2; guards armed)
    H->>OV: install {old_binding = map[b], dest, fence_epoch} (law 1)
    H->>OV: begin_store claim (KD-OV-10)
    H-->>K: ACK-early (COMMIT_RETAIN; bytes sampled at ACK)
    H->>D: WRITE_FIXED slot -> dest (detached continuation)
    D-->>OV: CQE -> coverage publish (law 3)
    Note over OV: reads: covered from dest, gaps from old_binding (law 5-B4)
    OV->>OV: coverage complete -> freeze
    OV->>A: publish_block(dest) (incarnation stable)
    OV->>E: rewrite_shadow_record(ino,b,new_key,guard)  [arm (a)]
    Note over E: parks displaced old key; notes ref Delete+Put (law 7)
    E->>E: fsync / coverage close: ONE durable save (data barrier first)
    E->>R: free_block(old) -> begin_free -> elision/reclaim -> finish_free (law 8, manners)
```

## 6. API / Interface Changes

* **Env knob (ENG-10 registry entry mandatory):**
  `SQUEEZEFS_OVERLAY_OVERWRITE` — Bool, **default ON** (the efficiency
  doctrine: the conversion deletes a full CPU pass over ~97 % of
  overwrite ingest bytes; a throughput wash still ships ON on CPU/byte).
  `0` = the B2 fresh-only gate restored verbatim (the A/B control leg —
  never an operational escape). `SQUEEZEFS_DEVICE_OVERLAY=0` continues
  to disable the whole overlay including this arm (existing semantics
  preserved); `SQUEEZEFS_ZC_ACK_EARLY`/`_ODIRECT` semantics unchanged
  and vehicle-blind. **No new numeric tunables** — the arm derives
  everything from existing geometry (block size, the R5-ridden
  `overlay_inflight_bytes` component), which discharges the
  derive-from-resources law with nothing to derive; the one allocation
  (dest block) is inherently whole-block (rounds up by definition).
* **`try_device_overlay_store`** — the mapped decline becomes the
  overwrite install arm (§5.1); signature unchanged.
* **`settle_overlay_block_locked`** — gains the epoch-feed publish arm +
  old-binding gap seeding (§5.4/§5.8); returns a close-owed verdict
  (the epoch close moves to the guard-dropping caller — §5.4's venue
  law).
* **`try_serve_overlay_read`** — gap composition source becomes
  old-binding-aware (§5.6).
* **`try_sole_owner_patch`** — predicate **clause 8** (live
  device-overlay record ⇒ decline; new `patch_ineligible_device_overlay`
  bucket — clause 7 is the S11 range-custody screen).
* **`DataRouter::overlay_foreign_merge_hook`** — new, called from the
  exact `supersede_shadow_bindings` sites (§5.7), class-split:
  `MergeExpected` ⇒ skip-apply entries on open-overlay indices (the
  belt for guard-less callers; the mover's primary screen is the
  quiesce probe); `TruncateFrom`/`RemoveBlocks` ⇒ mark `Superseded`;
  `Merge` ⇒ self-publish-provenance exempt, foreign = tripwire +
  containment. MARK-only under 3.5; returns the touched set for the
  caller's post-3.5 detached retire. **Wiring**: the registry lives on
  `SqueezefsFilesystem` (`src/fuse_client.rs:6998`) while every hook
  site is router-side — a probe/mark closure pair (or the registry
  `Arc`) is **injected into `DataRouter` at mount construction**, the
  exact `QuiesceProbe` precedent (`fuse_client.rs:7893` →
  `jobs.rs:534`); the injected surface stays a pure probe + `supersede()`
  CAS so the router gains NO teardown authority (the MARK-only rule's
  structural form). The mover-probe arm needs no new wiring at all —
  `mover_quiesce_probe` is already the fs-side closure the mover ctx
  carries.
* **`DeviceOverlayRecord` / `overlay_core`** — `old_binding:
  Option<String>` (None ⇔ the fresh/hole shape; immutable post-install)
  + the distinct **`OverlayState::Fed`** terminal (§5.4a) with its
  disarm-without-free disposition. `DeviceOverlayRegistry::install`
  grows the parameter.
* No CLI changes; no wire changes; no fuse3-fork changes (the transport
  hold decision already routes through `zc_write_hold_eligible`).

## 7. Data Model Changes

**None on disk.** The overlay stays volatile (parent §6.1 — no incompat
bit, no journal record); the remap publishes through the existing
layout-merge transaction; `TREE_BLOCK_REFS` deltas ride it via
`pending_block_refs` (one tx = one checksummed journal entry — the
`accounting_rides_the_publish_transaction` equality pin re-runs on B4's
suites). RAM: one `Option<String>` per record + the epoch entries the
merge path would have created anyway. R5: `overlay_inflight_bytes` keeps
riding the budget (non-sheddable, Red clamps admission — unchanged);
parked displaced bytes are already gauged
(`rewrite_shadow_parked_bytes`).

## 8. Accounting

### 8.1 Durable block refs
Exactly parent law 7 via arm (a): the feed notes `(b, old, Delete)` +
`(b, new, Put)` once, inside the ino's meta-lock section
(`:9848-9856`); the save drains them into its own KvTx. The C8 oracle
(`SQUEEZEFS_TEST_STAMP_BLOCK_REFS=1` + `verify_durable_block_refs`) runs
over every B4 suite; `meta_kv_block_refs_drift` stays a must-stay-0 gate
column.

### 8.2 `rewrite_amp` SLO attribution (vehicle-blind, design-rewrite-program §2)
The feed's `Shadowed { displaced_prev: true }` arm attributes:
`rewrite_blocks += 1`, `rewrite_user_bytes += covered app bytes`,
`rewrite_device_write_bytes += block_size` (stores + gap seeds — the
dest receives exactly one block). Mirrors `upload_block_publish_phase`
`:15678-15687` so the SLO cannot tell vehicles apart, which is its
charter.

### 8.3 Write-amp instrument columns (standing row requirement)
Every B4 row carries: device bytes ÷ user bytes on the DATA namespace
(≤ ~1.05; the displaced block's discard is ELIDED or deferred —
`block_free_reclaim_*` and the elision debt ledger must account for
displacement without mid-row device discards), `wareq-sz` vs block size
(no request-size collapse — the 4 MiB stores must not fragment), and the
`rewrite_amp` gates (≤ 1.05 + zero mid-row discards).

## 9. Alternatives Considered

| Alternative | Trade-off | Verdict |
|---|---|---|
| **Arm (b): overlay-owned per-block durable publish + inline displaced free** | No epoch dependency; but forks a second pending-binding authority (all five hazards live against a default-ON shadow), one durable save per block instead of one per epoch (the write-commit-economy campaign exists because per-block commits were the tax), and re-derives the crash-window table | **Rejected as default; retained as the `SQUEEZEFS_REWRITE_SHADOW=0` degenerate** (§5.4) — the A/B lever comes free |
| **In-place overwrite of the mapped offset** (skip displacement entirely) | Zero displaced-free traffic; but a crash mid-DMA tears bytes the durable map names (never-lossy violation under ACK-early — acked+fsynced OLD data destroyed), collides with W1's fence territory, and the substrate evidence is against it (zram slot-replace ≈ 2× a fresh write — the `SQUEEZEFS_INPLACE_OVERWRITE` doctrine) | **Rejected**; Idea 7 (§9.2 rewrite program) owns any future intent-derived in-place arm |
| **Eager RMW at install** (seed gaps from old immediately, record always whole) | Simpler reads (no gap composition); but pays a device read per overlay ON the ACK path for gaps sequential streams will cover anyway — re-importing the seed-read class RW3b spent a campaign deleting | **Rejected**; seed lazily at settle only (§5.8) |
| **Publish partial coverage as extent records / durable two-source map** | — | **Rejected in the parent** (§6.2 defeat conditions recorded); not re-opened |
| **Overlay the epoch-bound (shadow-B) blocks too** | Captures re-overwrites within one fsync window | **Deferred** (OQ-3): needs the overlay to displace a RAM-only key whose park the epoch owns — hazard-1 territory; the field shape never hits it |

## 10. Security & Privacy

No new surfaces. The dest is minted by the daemon's own allocator and
DMA'd through `zc_write_fd` behind `authorize_zc_store` (D0 latch + S7
epoch) — unchanged. Law 5 already forbids serving recycled dest content;
the B4 twist (gaps from old binding) serves only bytes the file already
durably owned. The stats additions carry no keys/paths (counts and bytes
only — no VAL-7a census exposure). Passthrough-only means no interaction
with the AEAD envelope. Fenced zombies: the reclaim fence-halt +
`authorize_zc_store` per-retry check + KD-1.8 epoch fence keep every
destructive command gated on the same `failed` latch.

## 11. Observability

New counters (stats inode), all `Align64<AtomicU64>` in the overlay
family (`src/fuse_client.rs:5854+`):

| Field | Semantics |
|---|---|
| `overlay_overwrite_installs` / `overlay_overwrite_bytes` | the B4 engagement face. **`overlay_overwrite_bytes` counts at the store CQE** — it is the overwrite-arm SUBSET of `overlay_store_bytes` (which is counted exactly once per landed segment on both the inline and ACK-early arms, `src/fuse_client.rs:13120-13122/:13280-13282/:13364-13367`), so the closure equation is well-formed: on the governing row `overlay_store_bytes ≈ user bytes` (= overwrite subset + fresh-arm subset — the 96.6 % collapse instrument). `overlay_ack_early_bytes` is **not** a term of the closure (an ACK-early store increments it at ACK AND `overlay_store_bytes` at CQE — summing them double-counts); it is the separate ACK-early-share check, `overlay_ack_early_bytes ≈ overlay_store_bytes` on an ACK-early-armed row |
| `overlay_epoch_feeds` / `overlay_feed_fallbacks` | arm (a) vs the shadow-off degenerate; `fallbacks > 0` with the lever ON is a bug |
| `overlay_gap_seed_old_bytes` | the §5.8 falsifier instrument (subset of `overlay_gap_seed_bytes`); ≈ 0 on sequential shapes |
| `overlay_gap_seed_read_bytes` / `overlay_gap_seed_ranged_bytes` | the §5.8 seed-bytes law (W-6): device bytes READ to source gap seeds (the amplification numerator — `read ÷ old` ≈ 1 with the ranged seed engaged, = block window ÷ Σ gaps on the `SQUEEZEFS_GAP_SEED_RANGED=0` control) / the old-sourced seed bytes that rode the ranged funnel (⊆ `overlay_gap_seed_old_bytes`; 0 with the lever off, on decorated bindings and where Σ gaps ≥ the window) |
| `overlay_ineligible_shadow_bound`, `overlay_enospc_declines` | the two new decline ledgers (§5.1) |
| `overlay_ineligible_range_shared` | the S11 rung-16 range clause (§5.1) — the W1 clause-7 twin, kept apart from `patch_ineligible_range_shared` and from `overlay_ineligible_shadow_bound` (the ledgers must not merge). **0 on every shipped mount** (whole-file leases ARE whole-inode custody; no verb issues ranges without the mw arm) and **0 on block-aligned ranged rows** (`design-full-multi-writer.md` §9.5's MPI-IO gate: nothing should share a block) — growth means range custody engaged on sub-block-shared blocks (rung 17's demotion territory) or the predicate rotted |
| `overlay_ineligible_sub_cap` | the §5.1 **length floor** (finding 47): aligned single-block passthrough segments every other shape conjunct admitted but whose length is ≤ the W1 cap (`patch_max_bytes()`, block_size/8) — sent down the W1/W2 ladder instead, BOTH shapes. Grows ≈ per sub-cap aligned write on overlay-armed mounts by design; `overlay_gap_seed_old_bytes` growing on a rand-4k row while this stays flat is the predicate rotting. 0 under `SQUEEZEFS_PATCH_MAX_BYTES=0` (cap 0 empties the sub-cap class) |
| `patch_ineligible_device_overlay` | W1 clause 8 (kept apart from both the W2 `patch_ineligible_overlay` bucket and the S11 clause-7 `patch_ineligible_range_shared`) |
| `overlay_mover_skips` | the §5.7 `MergeExpected` skip engaging — counted at BOTH layers (the mover quiesce-probe deferral and the primitive belt for guard-less callers); acked custody preserved, mover/repair re-plans or refuses — growth under mover passes is the hook working |
| `overlay_superseded_by_merge` | the §5.7 DISCARDING-class belt engaging (`TruncateFrom`/`RemoveBlocks` reaching the primitive un-drained) — pair with `rewrite_shadow_superseded`; a FOREIGN un-marked `Merge` additionally trips `invariant_tripwires` (`overlay_foreign_merge`) — **must stay 0 on healthy mounts** now that the settle's own publish is provenance-exempted (§5.7): any growth is a real one-authority-screen escape, never publish noise |
| `overlay_read_gap_serves` / `overlay_read_gap_bytes` | old-binding gap composition engagement |

Must-stay-0 set additions: none new — `overlay_fence_drops`,
`overlay_unpublished_at_fsync`, `meta_kv_block_refs_drift`,
`fsck_findings`, `write_path_seed_read_bytes` (the compose/seed reads
are counted in their own families, NEVER in the write-path seed
tripwire) all keep their existing laws and now also gate B4 rows.
Existing gauges (`overlay_open`, `overlay_inflight_bytes` R5 component,
`rewrite_shadow_parked_bytes`) cover the new population unchanged.

## 12. Risks

| # | Risk | Severity | Mitigation |
|---|---|---|---|
| R1 | Warm-tier stale serve on a mapped overlaid block (hot-block hit, read-lane hold, IPC sync legs, dest-lease) — the failure generic/209 exists to catch, now with a WARM population | **Critical** | per-arm probe-or-demote (KD-OV-13 live), red-first per arm on a tier-hot fixture; generic/209 storm re-run with overwrite overlays engaged (per-arm red tests = PR B4c-i; the storm re-run = B4c-ii's gate); dest-lease keeps the v1 overlay-free gate |
| R2 | Dual-authority double-free / resurrection / ref drift (hazards 1–5) | **Critical** | arm (a) single authority (§5.4); each hazard pinned red-first with shadow default-ON; the C8 drift oracle + journal-entry-equality pin on every suite |
| R3 | W1 patch lost-update against a live overlay | **High** | predicate clause 8 + shared block-lock serialization (§5.5); red: patch-during-open-overlay asserts the patch bytes survive publication or the patch declined |
| R4 | `old_binding` staleness under mover/foreign durable merges → gap serves read a freed key — AND the two converses: a supersede that drops acked custody under a content-preserving merge, and a hook that cannot tell the settle's OWN publish from a foreign one (containment would free a durably-published dest — the belt manufacturing KD-1.11) | **High** | the class-split `overlay_foreign_merge_hook` at the KD-1.11 sites (§5.7): `MergeExpected` skip in two layers (mover quiesce probe primary, primitive belt for the guard-less `flip_mapping_damaged`), discarding ops mark-Superseded, `Merge` **self-publish-provenance-exempt** then tripwire+containment for foreign; capture→install in one 3.5 section (§5.1); red: `settle_publish_never_trips_the_foreign_merge_hook` + `mover_merge_skips_open_overlay` + `fsck_damaged_flip_refuses_under_live_overlay` + the truncate-belt shape + the install-vs-foreign-merge interleave |
| R5 | Partial-coverage seed traffic material on real streams (RMW re-import) | **Medium** | `overlay_gap_seed_old_bytes` falsifier; eligibility narrows (min-coverage predicate) before the ladder proceeds — the parent's B4 falsifier, kept verbatim |
| R6 | Displaced-free pressure mid-row (reclaim cap parks on the write path) | **Medium** | discard elision (Idea 4) makes steady-state displacement discard-free; manners law + park-don't-spill already shipped; acceptance row carries `block_free_reclaim_*` + `cap_parks` columns |
| R7 | ENOSPC interplay (dest mints racing parked-A supply) | **Low** | StorageFull ⇒ decline-to-accumulation ⇒ the epoch's KD-1.7 early-close ladder (§5.1); red: near-full volume overwrite loop converges without spurious ENOSPC |
| R8 | ACK-early acked-custody loss shapes (dead ring at unmount, fence) | **Low** (existing class) | the `finish_ack_early_*` ladders transfer unchanged (`overlay_ack_early_lost` / `overlay_fence_drops` loud); B4 adds no new drop arm |

## 13. Rollout Plan

1. Land per the PR plan below (each rung independently mergeable off
   `dev`, tests-first, full `task check` gate; loom re-runs on
   `overlay_core` transitions in B4a).
2. `SQUEEZEFS_OVERLAY_OVERWRITE` ships default **ON** with B4d's
   acceptance evidence (the efficiency doctrine); the lever + the shadow
   lever give a 2×2 A/B grid, all four cells correctness-green.
3. Local acceptance on the two-substrate rule: loop devsub for
   decomposition, **nvmet-tcp devsub mandatory** for the write rows;
   field venue re-run for the governing row.
4. Release-gate composition: pjdfstests/LTP/fstests full runs (the
   generic/209, /551, /795 shapes now exercise the overwrite arm);
   `run_bench_baseline.sh` pre-merge (perf PR tier);
   the fresh-arm brackets re-run as regression sentinels (B4 must not
   move the first-touch rows).
5. Rollback = the knob (mount-time); no durable state to migrate in
   either direction (volatile overlay).

## 14. Open Questions

1. **OQ-1 — multi-block bytes-vehicle spans** (P2): relax
   `start_block == end_block` for materialized payloads so each
   per-block future can overlay its slice? Needs per-future eligibility
   + slot-vs-bytes split at delivery. Deferred until a counted row shows
   multi-block requests material on an overwrite venue.
2. **OQ-2 — gap-serve venue**: this design serves gaps from the captured
   `old_binding` key through the fetch funnel (identity-stable, no
   recursion). The alternative — re-enter the ordinary read path with an
   overlay-suppressed flag — composes tiers more naturally but risks
   probe recursion. B4c-i measures both if `read_serve_phase_ns` shows the
   captured-key path off the tier fast path on warm gap workloads.
3. **OQ-3 — overlaying epoch-bound blocks** (re-overwrite within one
   durability window): requires the overlay to displace a RAM-only B key
   under the epoch's custody rules. Deferred with its decline counter as
   the demand instrument (§5.1).
4. **OQ-4 — IL ring-write parity**: the placed-sever IPC write path keeps
   merge semantics; whether severed whole-block Bytes should install
   overwrite overlays via the bytes vehicle (they already may on the
   fresh shape when `bytes_vehicle_armed`) is a shim-parity follow-on
   measured against the write_matrix verdict.
5. **OQ-5 — barrier-before-close for overlay-fed epochs** (the OW-8
   strengthening): require `flush_data_devices()` before any
   coverage-triggered / idle-sweeper close of an epoch holding
   overlay-fed bindings, upgrading those saves out of the DUR-2
   volatility class. Deliberately NOT taken by default — the identical
   window ships today in the rewrite program's own non-fsync closes, so
   taking it for B4 alone would be an inconsistent durability posture
   and taking it globally is a product durability-class change beyond
   this design's charter. B4d prices the barrier (one `flush()` per
   coverage close on the governing row) so the decision, if wanted, is
   counted, not guessed — and its correctness half is already written:
   the TEST-1 green twin of `ow8_window_is_exactly_as_disclosed`
   (§5.3) proves the lever closes the window before anyone prices it.

## 15. References

* `docs/design-device-overlay.md` Rev 2 — the parent (nine laws,
  KD-OV-1..14, §2.3 claims, §5 read protocol, §6.2 fsync sequence,
  §7 gates, §8 ladder).
* `docs/design-rewrite-program.md` — §5 shadow dual-map (KD-1.1..1.11),
  §5.2 deferred-free law, §5.6 crash windows W1–W6, §3 discard elision,
  §2 `rewrite_amp` SLO, §9.2 Idea 7 (the in-place non-goal's owner).
* `docs/design-random-small-writes.md` §5.1 — W1 predicate ledger + the
  clone/patch `fence(SeqCst)` protocol (composed two-word loom model).
* `docs/design-zc-write-kernel-v2.md` §3 — patch 0029 retention /
  ACK-early sampling law; `docs/design-one-path.md` §overlay counters.
* `docs/design-cow-kv-metadata.md` §4.10 (whole-tx), §5.5.2b (the
  crash-window enumeration discipline this doc mirrors);
  `docs/design-durable-block-refcounts.md` (law-7 substrate).
* Evidence: `.benchmarks/2026-08-09-device-overlay-b1-b2.md`,
  `.benchmarks/2026-08-09-kernel-zc-write-v2.md`,
  `.benchmarks/2026-07-27-async-block-reclaim.md`,
  `.benchmarks/2026-07-31-write-wall.md`,
  `.benchmarks/2026-08-01-rewrite-publish-drain.md`.
* Code anchors: `src/fuse_client.rs`
  `:12378` (`try_sole_owner_patch`), `:12754/:12781` (hold gate),
  `:12814-13153` (store), `:12845` (the B4 gate), `:13164/:13318`
  (ACK-early continuations), `:13398-13506` (settle), `:13552` (ino
  drain), `:13623-13712` (read compose), `:13815-13969` (handler
  ladder + one-authority screen), `:15633-15759`
  (`upload_block_publish_phase` — the template), `:15839-15900` (fsync
  ordering), `:17912` (`zc_write_hold_eligible`); `src/routing.rs`
  `:9748-9877` (`rewrite_shadow_record`), `:9896`
  (`supersede_shadow_bindings`), `:9957` (`close_rewrite_epoch`),
  `:10365` (`merge_block_mappings_coalesced`), `:3526`
  (`pending_block_refs`); `src/device_overlay.rs` `:166/:221-261/:310`;
  `src/overlay_core.rs` `:235/:289/:408-420/:520-532`;
  `src/block_allocator.rs` `:1990/:2041/:2179`; `src/block_reclaim.rs`;
  `src/env_knobs.rs` `:145/:146/:205/:206`.

---

## Key Decisions

| # | Decision | Rationale |
|---|---|---|
| KD-B4-1 | **Coexistence arm (a): completed overwrite overlays feed `rewrite_shadow_record()`; the epoch is the ONE pending-binding authority** (publication, displaced park, deferred free, fencing, crash windows). Arm (b) survives only as the `SQUEEZEFS_REWRITE_SHADOW=0` degenerate. | Discharges all five Rev-2 dual-authority hazards structurally (§5.4 table) instead of by coordination; inherits the proven §5.2/W1–W6/KD-1.8/KD-1.11 machinery verbatim; keeps the swap economy (one durable save per epoch); the ref deltas ride the publish tx with zero added commits (law 7's equality pin transfers). This is the decision Rev-2 correction B mandated before overwrite overlays may arm; B8 stays cleanup-only. |
| KD-B4-2 | **Always a fresh destination — never in-place.** The mapped offset is never written; displacement is CoW, and the old image stays byte-intact through every crash window. | ACK-early + in-place = a crash can tear durably-named bytes (never-lossy violation, not a perf trade). In-place remains W1's (sub-block, §5.1-fenced) and Idea 7's (substrate-probed) — three regimes, three owners, no overlap. |
| KD-B4-3 | **Guard-ownership transfer at the feed**: the `InflightAllocGuard` moves into `epoch.guards` (continuous fsck C2/C3 visibility); the `MintedBlockGuard` disarms because the epoch's dispositions (swap-publish / fence-free-nothing) ARE the rollback contract from that point. | Law 9 restated for a RAM-only ownership transfer: the epoch's W5 posture (free nothing when fenced; successor recovery owns) is byte-identical to the record's own `FenceDropped` disposition — no window where the dest has zero owners and no window where two owners can free it. |
| KD-B4-4 | **W1 declines on a live overlay record (predicate clause 8, own ledger bucket); no new fence.** | The lost-update shape is closed by serialization (shared `BLOCK_FLUSH_LOCKS`) + the predicate — B4 never violates post-map immutability, so the §5.1 `fence(SeqCst)` protocol needs no extension and no new loom model beyond the core-transition re-runs. Cheapest correct fix; keeps W1's hot path one extra latch-free probe. |
| KD-B4-5 | **Gaps compose and seed from the captured `old_binding`; the capture and the registry install share ONE `INODE_META_LOCKS` section (§5.1); the capture is immutable for the record's life, and the class-split `overlay_foreign_merge_hook` (the KD-1.11 sites, §5.7) is what makes that immutability true — with `MergeExpected` handled by SKIP-APPLY, never supersession.** | An immutable capture needs no reader revalidation word (the lock-free compose stays two-word: generation + destination identity), but immutability must hold from BIRTH (the one-section rule — the block guard closes nothing against foreign merges) and for LIFE (the hook). Content-preserving movers carry no new bytes, so superseding for them would drop ACK-early acked custody without a fence — the never-lossy law decides the arm, and the shadow's own map-flip immunity (`MergeExpected` mismatch-skip) is the precedent the skip extends. |
| KD-B4-6 | **Old-binding tier entries preserved while open, purged at the feed** (KD-OV-13 live); every fast serve arm probes or demotes. | Eviction is never a coherence mechanism (the Rev-2 ruling); the old entries are the warm gap source; the purge point (the feed's `rewrite_shadow_record` tier purge under `INODE_META_LOCKS`) is exactly where readers stop resolving old through the map. |
| KD-B4-7 | **Shared blocks and decorated mappings are eligible; refcount is not a screen.** | CoW displacement + `free_block`'s decrement law make clone-shared displacement already-correct (the epoch handles it today); decorated forms are read-composition sources decoded by the existing funnel — B4 has no in-place arithmetic to protect, which was W1's reason for both screens. |
| KD-B4-8 | **`StorageFull` at the dest mint declines to accumulation** (never errors), engaging the epoch's KD-1.7 ENOSPC early-close. | On overwrite rows the free supply IS the parked displaced set; only the epoch ladder can recycle it. A loud failure here would be a self-inflicted ENOSPC. |
| KD-B4-9 | **`SQUEEZEFS_OVERLAY_OVERWRITE` default ON; no new numeric tunables.** | Efficiency doctrine: the conversion deletes a full daemon CPU pass over 96.6 % of the governing row's bytes (~30 GB/s of memcpy at 31 GiB/s ingest) — ships ON even at throughput wash. Everything else derives from existing geometry; the registry entry + drift tests per ENG-10. |
| KD-B4-10 | **Acceptance is the field row's merge-share collapse, counted A-B-B-A with the pre-fill rule.** The closing quantity is **`overlay_store_bytes`** (CQE-anchored, counted once per landed segment on both arms): its delta ≈ the row's user bytes, with `overlay_overwrite_bytes` (its CQE-counted overwrite subset) accounting for the overwrite share. `overlay_ack_early_bytes ≈ overlay_store_bytes` is the SEPARATE ACK-early-engagement check — never a closure term (it counts at ACK while store bytes count at CQE; summing them double-counts every ACK-early byte and self-invalidates the row). `nt_copy_bytes` and `write_through_bytes` ≈ 0 on the eligible shape; `rewrite_amp ≤ 1.05` with zero mid-row discards; sustained ≥ 60 s flat, both bracket orders, reset-per-leg. | The conviction was byte-exact; the acceptance must be too. A row whose engagement deltas do not close is INVALID (charter rule 4), whatever its GB/s says — which is exactly why the equation itself must be well-formed. |
| KD-B4-11 | **The `Merge`-class hook exempts the settle's OWN publish by explicit PROVENANCE** (`overlay_self_publish = (b, record identity)` on the op / `QueuedPublish` entry) — never by Frozen-state exemption, never by retiring the record pre-merge. | The settle publishes while its record is still Frozen+registered (`:13398-13500`) and the conveyor apply is a hook site (`routing.rs:10628`) — an unexempted hook fires on every healthy publish AND its containment `supersede()` succeeds from Frozen (`overlay_core.rs:431-434`), freeing a dest the merge just durably published (the belt manufacturing KD-1.11). Frozen-exemption would blind the belt to genuinely foreign discarding merges racing a Frozen record; pre-merge retire opens the record-gone-⇒-published generic/209 window (`:13704-13712`). Provenance is race-free (the settle holds the block guard across the merge; installs need the guard) and the identity word makes it robust regardless. Uniqueness is STRUCTURAL by the layer split: the marker's scope is the per-ino apply pass (`publish_conveyors` is keyed by ino, `routing.rs:3494-3498`); the one multi-ino aggregation in the tree — Lever-B `publish_commit_group` — is downstream at the save layer and marker-blind by construction, so the marker is consumed at apply and never serialized onward. |

## PR Plan

Ordered; each independently reviewable and mergeable off `dev`,
tests-first, full `task check` gate. Falsification-first per the parent
ladder discipline: a fired falsifier stops the ladder with its evidence
note. The **governing acceptance row** for every perf rung is the
field-shaped 60 s sustained 4 MiB sequential-overwrite ingest (8 × 8 GiB
fileset, **pre-fill rule**: the fileset is fully written, published,
epoch-closed and reclaim-drained before any measured leg; reset-per-leg
A-B-B-A) on nvmet-tcp devsub locally + the field venue for the closing
row.

| PR | Branch | Scope | Red-first test obligations (named) | Acceptance / falsifier |
|---|---|---|---|---|
| **B4a — core: the old-binding record** | `feat/overlay-b4-core` | `overlay_core`/`device_overlay`: `old_binding` field (immutable capture), the **distinct `OverlayState::Fed` terminal** (§5.4a) + its disarm-without-free disposition, gap enumeration vs a non-zeros source as pure transitions; registry `install` signature; loom re-run of the snapshot/revalidate + freeze/begin_store models over the grown transition set (no new fence — KD-B4-4); proptest schedules extended to overwrite records | `tests/overlay_core_tests.rs`: law pins for the `Fed` disposition (disarm-without-free; distinct from `Published`), old-binding immutability, no schedule grants overlapping in-flight claims on an overwrite record; teardown-disposition table exhaustive over the grown state set (the `overlay_teardown_nonterminal` tripwire never fires on a feed) | pure-transition expressibility (the B1 falsifier verbatim): a law that cannot be a pure transition ⇒ redesign before I/O |
| **B4b — the coexistence arm (a)** | `feat/overlay-b4-feed` | `settle_overlay_block_locked` publish split (feed vs durable-merge degenerate, §5.4) returning the close-owed verdict; **epoch close moved to the guard-dropping venues** (§5.4's venue law — the `:14798` pattern at the detached publisher / handler screen / fsync leg); guard transfer; SLO attribution; `min_size` floor; fsync ordering pin (drain-feeds-before-close). **Test seam, named**: the mapped-decline gates are still closed until B4c-ii, so every suite installs overwrite records **directly through `DeviceOverlayRegistry::install`** (pub(crate); B4a's grown signature) under a held block guard — the in-process suites' pattern; this seam is what makes B4b independently mergeable | **the five hazards, each red-first with shadow default-ON** (`tests/overlay_overwrite_tests.rs`): `hazard1_single_displaced_park`, `hazard2_no_double_free` (kill the free twice — fsck C2/C3 clean), `hazard3_no_kd111_resurrection` (KD-1.9 refetch-compose after feed — the KD-1.11 poison class), `hazard4_one_fsync_authority` (journal-entry-count equality vs the un-fed control — the law-7 pin), `hazard5_ref_drift_zero` (C8 oracle, `SQUEEZEFS_TEST_STAMP_BLOCK_REFS=1`); the shadow-off degenerate's displaced-free-after-publish pin; `close_never_runs_under_block_guard` (the venue pin); ENOSPC decline→early-close convergence (R7) | correctness rung; falsifier = any hazard pin needing a coordination patch instead of the structural discharge (would mean arm (a) is not actually one authority — stop and redesign) |
| **B4c-i — read composition, the hook, and the W1 clause (gates still closed)** | `feat/overlay-b4-read` | read-compose old-binding gap serves + per-arm tier probe/demote audit (KD-OV-13 live); gap seeding from old at settle; the class-split `overlay_foreign_merge_hook` (MARK-only + caller's detached retire + the **self-publish provenance marker** on the settle op / `QueuedPublish`, §5.7) with its `QuiesceProbe`-pattern injection (§6) + the mover-probe overlay arm; W1 clause 8 (`patch_ineligible_device_overlay`); the §5.7/§11 counters. All exercisable through the B4b registry seam — R1 (the highest-severity risk) lands and proves out BEFORE any product path can mint an overwrite record | red-first: **`settle_publish_never_trips_the_foreign_merge_hook`** (fresh arm AND shadow-off degenerate: `invariant_tripwires` delta 0, dest never freed, map resolves to dest post-settle — the Issue-1 pin); `recycled_content_never_served_on_mapped_shape` (law 5, overwrite edition); `warm_tier_never_serves_stale_under_open_overlay` — **one case per fast arm** (hot-block, read-lane hold, IPC sync legs, dest-lease gate) on a tier-hot fixture; `patch_bytes_never_lost_to_overlay_publish` (R3, both orders); **`mover_merge_skips_open_overlay`** (R4 — ACK-early acked bytes survive an evacuate/defrag publication; the mover converges via the probe deferral + re-plan) + **`fsck_damaged_flip_refuses_under_live_overlay`** (the layer-2 belt's one real client refuses, never reports success) + the truncate-belt shape (discarding op un-drained ⇒ Superseded ⇒ detached retire converges `overlay_open` → 0); `compose_vs_close_storm_with_hot_reclaim` (§5.6(3)) | correctness rung; falsifier = a fast arm that can neither probe nor demote without measurable read-row tax (the parent B3 falsifier re-applied to the warm population) |
| **B4c-ii — the overwrite arm live** | `feat/overlay-b4-overwrite` | remove the `:12845`/`:12781`/`:17922` mapped declines behind `SQUEEZEFS_OVERLAY_OVERWRITE`; the ONE-3.5-section capture+install (§5.1); `StorageFull` decline; knob registry entry + drift test | red-first: the **install-vs-foreign-merge interleave** (stall seam — a record can never be born with a displaced `old_binding`, §5.1); generic/209 storm + serialized discriminator with overwrite overlays engaged; generic/551 sibling-AIO re-run; **kill-9** crash matrix pinning OW-1..OW-7 (recovery census reclaims dest, old image intact, no leak/double-free); **`ow8_window_is_exactly_as_disclosed`** on the in-tree TEST-1 harness (`src/dev_power_cut.rs` — arm, unbarriered coverage close, `power_cut()`, remount ⇒ dest residue, exactly the OW-8 disclosure) **+ its OQ-5 green twin** behind the barrier-before-close lever (kill-9 greens still never adjudicate OW-8 — §5.3); `SQUEEZEFS_REQUIRE_MOUNT` skip-ledger routing for every mount-class case | correctness gate: all red suites green, `fsck_findings`/drift/tripwires 0 across the matrix; fresh-arm suites (`tests/overlay_ack_early_tests.rs`) unmodified-green |
| **B4d — perf acceptance (the gate row)** — **DONE 2026-08-15** (`.benchmarks/2026-08-15-overlay-b4-overwrite.md`: local above-control gate FAILED both orders on the device-bound tcp devsub — falsifier fired, residual named as the BDP-depth term, discard term priced ≈ 0; engagement/amp/sentinel gates all passed; the deciding row moved to B4e per the venue split) | `perf/overlay-b4-acceptance` | measurement only + any counted retunes; refresh `tests/run_bench_baseline.sh` reference if hot-path benches moved; price the OQ-5 barrier-before-close option (one counted leg; its correctness twin already landed green-able in B4c-ii) | rig extension: the write-amp rig gains the overlay-overwrite engagement columns | **counted A-B-B-A, the governing row**: `SQUEEZEFS_OVERLAY_OVERWRITE` ON vs OFF (the B2-gate control), pre-fill rule, ≥ 60 s flat sustained, both orders, reset-per-leg, nvmet-tcp substrate stated. **Engagement (row-invalid otherwise)**: `overlay_store_bytes` delta ≈ the row's user bytes with `overlay_overwrite_bytes` (its CQE-counted overwrite subset) accounting for the overwrite share (the 96.6 % → ≈ 0 merge-share collapse — THE instrument); `overlay_ack_early_bytes ≈ overlay_store_bytes` (the separate ACK-early-share check — never summed with it, KD-B4-10); `nt_copy_bytes ≈ 0`, `write_through_bytes ≈ 0`, `overlay_feed_fallbacks = 0`, `overlay_gap_seed_old_bytes ≈ 0` on the sequential shape. **Gates**: materially above the control in both orders (outside the venue's noise band); `rewrite_amp ≤ 1.05` with zero mid-row discards; device÷user ≤ ~1.05 with `block_free_reclaim_*`/`cap_parks` columns; `wareq-sz` no collapse; fresh-ingest-vs-overwrite within the rewrite charter's ±5 %; the fresh-arm first-touch row unmoved (regression sentinel). **Falsifier**: overwrite rows fail to approach fresh-ingest parity with engagement exact ⇒ the residual is profiled and named (`write_pipeline_phase_ns` / `publish_phase_ns` / reclaim columns) before any further rung — the B2 honest-failure discipline |
| **B4e — field closure** — **DONE 2026-08-15** (`.benchmarks/2026-08-15-overlay-b4-overwrite.md`: ON wins both orders 32.1/31.6 vs 31.0/30.9 GiB/s at HALF the daemon CPU, engagement exact, nt_copy share 1.000 → 0.001; default ON adjudicated) | (evidence note) | re-run the 2026-08-14 field row on the field client; `.benchmarks/2026-08-15-overlay-b4-overwrite.md` records both venues, both brackets, the collapse arithmetic, and flips the parent design's B4 ladder row to DONE | — | field `overlay_store_bytes` ≈ io size on the 60 s row (with `overlay_ack_early_bytes ≈ overlay_store_bytes` as the ACK-early check); the merge-share table from §2.1 re-published with the post-B4 deltas |

Follow-ons (not this ladder): OQ-1 multi-block spans, OQ-3 epoch-bound
overlays, OQ-4 IL bytes-vehicle parity, B7 dest-lease overlay
composition (parent ladder, unchanged).
