# Design: The Device-Backed Visible Overlay (Approach B) — Rev 2

| | |
|---|---|
| **Title** | Device-backed visible overlay: slot→device direct stores as the write path's strategic end state |
| **Author** | _(placeholder — assign on review)_ |
| **Date** | 2026-08-08 (Rev 2 same day — engineering review folded) |
| **Status** | **Rev 2 — design only, review round 1 folded.** Formalizes Approach B of the write-bandwidth program adjudication (`docs/rc-manifest.md` §3f, third-party read-only investigation 2026-08-08, ACCEPTED). All seven Rev-1 Open Questions are RESOLVED (engineering review, 2026-08-08 — the §Resolved Questions record) and the review's three mandatory corrections (A mint-rollback ownership, B rewrite-shadow coexistence at B4, C recovery attribution) plus the law-6 concurrency clause and the tier-precedence law are folded into the body. No implementation has started; the sequencing gates in §3f (counter confirmation on the existing binary, then Approach A as bounded falsification) run first. |
| **Charter (rc-manifest §3f, condensed)** | The armed 1 MiB streaming path pays an **extraction destination before its real accumulation destination** (slot → memfd bounce [copy] → NT merge into `ActiveBlockBuf` [copy] → block DMA); raw 1 MiB writes reach the **49.5 GB/s class**, so granularity is not the device limit. Approach B is *shape (b) made concrete*: the app's pages, held in the transport's sparse slot, `WRITE_FIXED` **directly to an unpublished device offset**, with a volatile **DeviceOverlay** registry making the stored bytes read-visible before the map publishes. Strategic end state: **≥ 0.85× same-day raw, sustained.** |
| **Repo / branch** | `docs/design-device-overlay` off `dev` (`71f2e967`) |
| **Intended home** | `docs/design-device-overlay.md` |
| **Related** | `docs/rc-manifest.md` §3f (the adjudication — the normative source; rulings D13/D14/D16/D17 context), `docs/design-rewrite-program.md` (shadow dual-map, supersession, discard elision — the consolidation target), `docs/design-zero-copy-write-path.md` (§5.3 write-through + coverage law, §5.4 lease severance), `docs/design-random-small-writes.md` (W1 in-place patch — the existing slot→device consumer), `src/placed_sever.rs` (the assembly-adoption precedent), `src/cache/active_block.rs` (`record_write` coverage union), `src/nvme_dev.rs` (`zc_write_fd`, `authorize_zc_store`), `tests/write_visibility_tests.rs` (the generic/209 contract, `:847-949`), `docker/kernel-sqz/` patch 0024 (the kernel half: WRITE payload pages registered `ITER_SOURCE` in the sparse slot) |
| **Inviolable contracts** | AGENTS.md non-negotiables (io_uring-first, zero-copy + latch-free hot paths, no dead code, portable-by-default incl. ruling D13's kernel-frontier scope, TDD); lock order P1-9/P1-10 + RES-1; the D0 single-writer guard + RES-6/S7 DMA authorization; the v3 whole-tx metadata contract (one tx = one checksummed journal entry); the FIND-M11-A supersession law; the generic/209 read-freshness contract |

**Revision history**

| Rev | Date | Change |
|---|---|---|
| 1 | 2026-08-08 | Initial design: the nine laws, the reuse map, submission topology, the generic/209 read protocol, durability/fsync sequence, v1 hard gates, the B1–B9 PR ladder, stats surface. Seven Open Questions flagged for review. |
| 2 | 2026-08-08 | **Engineering review round 1 folded (all seven OQs resolved; three additional mandatory corrections).** OQ1/OQ2/OQ5/OQ7 **confirmed as written** (OQ7 with a critical addition); OQ3/OQ4/OQ6 **amended**. The four review corrections: **(1)** law 6 gains the mandatory in-flight overlap-exclusion clause — *two overlapping stores to the same overlay destination may never be in flight concurrently* — with the four-step stale-DMA counterexample and the v1 range-claim protocol (`placed_core::PlacedClaims` precedent; claims release only at the store CQE; whole-block-lock-across-DMA rejected — it serializes the 4×1 MiB cohort) (§2.2, §2.3); **(A)** mint-rollback ownership — `InflightAllocGuard` is fsck visibility ONLY, every destination also carries a `MintedBlockGuard`-class rollback owner, law 9 restated: the mint guard disarms only when durable map/ref publication transfers ownership (§2.1, §2.2, §3); **(B)** rewrite-shadow coexistence moves to **B4/B5** — shadow is default-ON, the five dual-authority hazards enumerated, exactly one coexistence arm must hold before overwrite overlays arm, B8 becomes cleanup-only (§8); **(C)** recovery attribution — `overlay_recovered_offsets` replaced by the honest generic `unpublished_offsets_recovered`; overlay-specific attribution is a fault-injection test assertion only (§9.1); **(4 = OQ4's amendment)** tier-precedence law — old-binding tier entries are PRESERVED while an overlay is open (they are the gap-composition source), purged only at durable displacement; overlay presence outranks every direct tier serve; a fast path that cannot run the precedence check DEMOTES rather than purging base data (§5.4). OQ3's comparator became the four-case table (§1.4); OQ6's eligibility narrowed to already-authoritatively-striped files with the promotion transition excluded (§7.1). The reviewer's five mandatory-before-implementation items appended to the Key Decisions ledger (KD-OV-10..14); Open Questions converted to the Resolved Questions record (the design-nvmeof-target-management rev-4 pattern). |

---

## 1. Charter and the path

### 1.1 What the bytes do today (the armed streaming path)

On a zc-armed session (kernel patch 0024 + the fork's negotiation face),
a FUSE WRITE's payload pages are registered `ITER_SOURCE` into the queue
ring's sparse fixed-buffer table at delivery and **held** there
(`crates/fuse3/src/raw/connection/zc.rs` — dispatch-before-extraction;
the daemon sees a `WritePayload::Slot(Arc<ZcWriteSlot>)`,
`src/routing.rs`). Only ONE consumer stores directly today — the W1
sole-owner patch class (`ZcWriteSlot::store` →
`NvmeBlockDev::zc_write_fd`). Every streaming byte instead pays:

```
slot ── WRITE_FIXED(slot → memfd) ──►  extraction bounce      [device-loop copy #1]
memfd mmap ── NT merge ────────────►  ActiveBlockBuf          [CPU copy #2, nt_copy]
ActiveBlockBuf ── write_block ─────►  device                  [the real DMA, later]
```

Two full passes over every byte plus a deferred whole-block DMA — an
extraction **destination before the real accumulation destination**. The
2026-08-08 investigation confirmed the shape on counters (extract bytes
≈ user bytes, direct ≈ 0, nt_copy ≈ user bytes) and priced the ceiling:
**raw 1 MiB writes reach the 49.5 GB/s class** on the same substrate, so
neither request granularity nor the device is the limit — the loop
through userspace is.

### 1.2 The Approach B path

```
app pages (sparse slot, ITER_SOURCE)
  ── install DeviceOverlay record (visible to readers)          [law 1]
  ── reserve unpublished device offset (allocator + guard)      [law 2]
  ── WRITE_FIXED(device fd ← slot) on the FUSE queue ring       [§4]
  ── CQE ⇒ coverage publication (range joins completed set)     [law 3]
  ── … more segments accumulate coverage on the same overlay …
  ── eventual map publication: ONE tx flips the layout + refs   [law 7]
  ── free the displaced old binding                             [law 8]
```

The store IS the accumulation: segments land at their final device
address as they arrive, the overlay registry makes them read-visible
immediately (laws 4/5), and the *durable* name arrives later — exactly
the rewrite-shadow economy (RAM-recorded bindings, one swap), extended
one level down so that even the RAM buffer disappears.

**What it deletes, per streaming byte:** the extraction
(`WRITE_FIXED(slot → memfd)` + the memfd arena), the NT merge into
`ActiveBlockBuf` (the `nt_copy_bytes` site), and the later full-block
`write_block` DMA. Daemon userspace copies on the eligible shape: **0**
(the §5.4 severance law is satisfied by *consumption*, not by copy — the
slot is consumed by the device before the handler returns or the
retention accelerator's lease rules hold it, §8).

### 1.3 Raw-ceiling arithmetic and gates

Per the standing pricing discipline (targets above the substrate's own
ceiling are re-negotiated with numbers, never chased):

| Quantity | Value | Source |
|---|---|---|
| Raw ceiling, 1 MiB writes | **49.5 GB/s class** | §3f adjudication (same-day raw row; re-measured per bracket — the *fraction* is the bar, never the absolute) |
| Strategic end state (B, converged) | **≥ 0.85× same-day raw, sustained** | the write-side twin of the 2026-08-06 read bar ("85 % of the raw ceiling"); sustained per the ≥ 60 s flatness law |
| **B-initial gate** (first landed increment, PR B5 closes it) | **materially above the §1.4 comparator AND ≥ 0.80× same-day raw** on the armed 1 MiB streaming shape | §3f as amended by the Rev-2 review (Resolved Questions #3 — the four-case comparator table, §1.4); "materially" = outside the venue's A-B-B-A noise band, both bracket orders |
| Row validity | engagement exact: `overlay_store_bytes` ≈ user bytes, extraction bytes ≈ 0, `nt_copy_bytes` ≈ 0 on the eligible shape | charter-rule-4 posture (§9) |

### 1.4 Relation to Approach A (the decision seam)

Approach A — the fd-backed **placed-merge assembly** (port
`src/placed_sever.rs`'s adoption design to FUSE delivery: since
`WRITE_FIXED` cannot copy fixed→fixed, the assembly is a memfd whose
mmap view is adopted as the `ActiveBlockBuf` backing) — runs **in
parallel, as bounded falsification**, with its own gates (placement ≥
90–95 % of stream bytes — first-chunk-only ≈ 25 % = FAILURE; armed-A ≥
0.99× unarmed; nt_copy/extract bytes fall commensurately; no W2
full-buffer inflation). A deletes one copy and keeps the accumulation
buffer; B deletes the buffer itself. **The seam:** A's falsification
evidence redirects *here* — if A fails its placement or parity gates,
the finding is that the accumulation-destination architecture cannot be
patched to the ceiling and B's build starts immediately with A's
bracket rows as its baseline; if A lands, B proceeds only if the
remaining gap to 0.85× still prices in. Either way A's instrument work
(the counter-confirmation battery, the per-leg reset discipline)
transfers verbatim.

**The B-initial comparator (Rev 2 — Resolved Questions #3, the
four-case table):**

| Case | A's outcome | B's comparator |
|---|---|---|
| 1 | A **lands** | B beats **A's best valid sustained row** (the engagement-exact, ≥ 60 s-flat one) |
| 2 | A fails **performance** but is correct | compare vs the **shipped extraction control** (A's mechanism is dead evidence, not a bar) |
| 3 | A fails **correctness/engagement** | A's treatment rows are INVALID by the row-validity law — compare vs **A's reset-matched CONTROL legs** (the unarmed halves of its own A-B-B-A brackets, the freshest venue-matched baseline that exists) |
| 4 | **all cases** | B additionally holds **≥ 0.80× same-day raw** — the fraction floor is unconditional |

The custom **fixed-copy opcode** (a kernel opcode copying fixed→fixed,
which would let A's assembly skip the memfd round-trip) stays
**PARKED** with its condition stated in §8.

## 2. The DeviceOverlay state machine

### 2.1 The registry record

One latch-free registry per mount (`scc::HashMap` keyed `(ino, block)`
— the `placed_sever`/`rewrite_epochs` precedent), holding at most one
live record per block:

| Field | Type / content | Why it exists |
|---|---|---|
| `ino`, `block` | the key | one overlay per `(ino, block)`; a second opener joins it |
| `old_binding_or_hole` | the block's **prior durable mapping key**, or `Hole` | law 5's gap source; law 8's deferred free target; captured under `INODE_META_LOCKS` at install so it can never be a stale snapshot |
| `dest` | unpublished backend id + device offset | where the stores land; `vol-` id per KD-5, never a path |
| `fsck_guard` | `InflightAllocGuard` | the fsck C2/C3 in-flight exemption (the `RewriteEpoch::guards` role, verbatim). **Visibility ONLY — dropping it frees nothing** (Rev 2, correction A) |
| `mint_owner` | a `MintedBlockGuard`-class rollback owner (`src/assembly_tasks.rs`, the RES-9 precedent) | law 9's carrier: the unwind that runs the terminal `free_block()` on every runtime failure / cancel / panic / fence / supersession path; **disarms only when durable map/ref publication transfers ownership** (Rev 2, correction A) |
| `completed` | the coverage union (shared pure core, §3) | law 3's ledger; the read protocol's serve authority |
| `inflight` | ranges submitted, CQE not yet seen — each stamped with the `generation` at submission, and each holding its **range claim** (§2.3) | law 3's boundary; the fsync freeze's await set; law 6's logical ordering; the claim is law 6's in-flight overlap exclusion — it releases only at the store CQE |
| `generation` | per-record monotone counter, bumped by **every** accepted segment (including re-writes of already-covered ranges) | law 6 — newest-wins; the read protocol's revalidation word (§5) |
| `fence_epoch` | the ino's DLM fencing token + the S7 custody epoch captured at install | the FIND-M11-A face: a superseded era's overlay publishes nothing; DMA authorization per submission stays `authorize_zc_store` |
| `prune_epoch` | the ino's `LAYOUT_PRUNE_EPOCHS` value at install | truncate/punch supersession detection (§7's drain-or-supersede) |
| `state` | `Open → Frozen → Published \| Superseded \| FenceDropped` | the fsync sequence (§6) and the shape-change ops (§7) drive the transitions; terminal states unwind per §6.3 |

The record is **volatile** — RAM only, no on-disk representation, no
incompat bit (§6). Mutations serialize under the block's
`BLOCK_FLUSH_LOCKS(ino, block)` (the same lock every coverage mutation
already takes); readers probe lock-free (the §5 protocol is
snapshot-and-revalidate, not lock-and-read).

### 2.2 The nine laws

Normative. Every PR in §8 lands red-first against the law(s) it
implements; the "violation instrument" column is what a broken law
looks like in production.

| # | Law | Statement | Enforcement point | Violation instrument |
|---|---|---|---|---|
| 1 | **install-before-ack** | The overlay record (with its `old_binding_or_hole` and destination) is installed and reader-visible **before** the WRITE is ACKed to the kernel. An ACKed byte is always resolvable by a reader — through the overlay or through the store it names. | write handler, before reply | generic/209 suite (`tests/write_visibility_tests.rs:847-949`): an acked-then-stale read is exactly the failure it pins |
| 2 | **reserve-before-DMA** | The destination offset is claimed from `BlockAllocator::allocate_block` (incarnation marked unstable) and its `InflightAllocGuard` held **before** any `WRITE_FIXED` names it. No store ever targets an offset the allocator could concurrently hand to another owner. | overlay install / dest mint | fsck C2/C3 on a live mount (`fsck_findings` must stay 0 — the guard is the exemption); `invariant_tripwires` |
| 3 | **publish-coverage-only-after-full-CQE** | A range joins `completed` only after its store's CQE reports success for the **full** submitted length. Short writes and errors re-arm or fall back (§4.3); a range in `inflight` is never served. | CQE reap on the queue ring | `overlay_short_stores` (§9); a serve of an in-flight range is a generic/209 torn read |
| 4 | **reads-prefer-overlay** | A read of a range ⊆ `completed` serves the overlay's device bytes — they are the newest data, RYW authority, exactly as a dirty `ActiveBlockBuf` is today. | read composition (§5) | `overlay_read_serves` engagement; RYW suites |
| 5 | **gaps-from-old-map-or-zeros** | An uncovered range serves from `old_binding_or_hole`: the old binding's bytes, or zeros for a hole. Never the overlay destination's uncovered bytes (they are recycled device content — the §5.3 uncovered-range contract, device edition). | read composition (§5) | sparse-hole read tests (the recycled-content leak pinned red-first, PR B3) |
| 6 | **generations-for-overlap** (newest-wins) | Overlapping segments are ordered by `generation`; a completing store publishes coverage only if no **newer** generation has claimed an overlapping range — otherwise the overlapped part resolves as superseded (publishes nothing for that part). **A bitmap alone cannot express this**: coverage says *whether* a range is stored, never *which* of two racing stores to the same range is newest — the newest-wins verdict needs the ordering word, which is why `generation` is a first-class field and why every `inflight` entry carries its stamp. The intra-mount CQE-supersession law (design-rewrite-program §4.3) verbatim, one level down. **Mandatory clause (Rev 2, correction 1): two overlapping stores to the same overlay destination may never be in flight concurrently** — generation filtering orders *publication*, but it cannot undo *physical writes* (the §2.3 stale-DMA counterexample); overlap exclusion is enforced by range claims held across the DMA, released only at the store CQE (§2.3). | coverage publication, under the block lock; overlap exclusion at submission (the claim grant) | `overlay_supersessions` (growth = the law engaging); the planted-stale-CQE test shape (`SQUEEZEFS_TEST_UPLOAD_STALL_MS` seam) |
| 7 | **one-transaction map+ref publish** | The map flip (block → overlay dest) and its durable block-ref deltas ride **ONE KvTx** — the layout-merge transaction the write path already uses (`set_layout_and_size` / `merge_layout_and_size` / the Lever-B pass; `pending_block_refs` for deferring sites). Never re-split; the ledger can never disagree with the layout that justifies it. | publication (§6.2) | journal-entry equality pins (the `accounting_rides_the_publish_transaction` precedent); `meta_kv_block_refs_drift` must stay 0 |
| 8 | **free-old-only-after-durable-publish** | The displaced `old_binding` is freed only after a durable save no longer references it — the rewrite-shadow §5.2 deferred-free law verbatim. Crash recovery owns every other case; there is no window in which a durable map references a freed block. | post-publish epilogue | fsck C2Lost/C2Leaked; the rewrite-shadow crash-window table transfers (§6.3) |
| 9 | **never-return-unpublished-to-allocator-while-DMA-in-flight** *(restated Rev 2, correction A)* | An unpublished overlay offset returns to the allocator only through its **mint rollback owner**, and only when the record can prove no `WRITE_FIXED` naming it can still land: `inflight` empty (all CQEs reaped, success or failure) **and** the record terminal. The `InflightAllocGuard` is **fsck visibility only — dropping it frees nothing**; every destination carries BOTH the fsck guard AND a `MintedBlockGuard`-class rollback owner (or an explicit terminal `free_block()`) covering **every** runtime failure / cancel / panic / fence / supersession path, and **the mint guard disarms only when durable map/ref publication transfers ownership.** Abandoned handlers, superseded overlays and fence drops all wait out their in-flight set before rollback runs. The MEM-1 class (a re-armed buffer under an in-flight SQE) applied to device offsets: a reused offset with a straggler DMA in flight is silent cross-file corruption. | record teardown (rollback) + publication (disarm) | `overlay_teardown_waits` (§9); `data_dma_epoch_refusals` for the custody face; an allocated-but-ownerless offset is fsck C2 — the leak the mint owner exists to make unrepresentable |

Laws 1–3 are the write half, 4–6 the read half, 7–9 the
durability/unwind half. Law 6 composes with law 3: coverage publication
is where both the CQE check and the generation check run, under the one
lock that orders them.

### 2.3 Law 6's concurrency clause: in-flight overlap exclusion (Rev 2, correction 1)

Generations order **publication**; they cannot undo **physical
writes**. The four-step stale-DMA counterexample that mandates the
clause:

```
1. segment A (generation g)   submits WRITE_FIXED over range R
2. segment B (generation g+1) overlapping R submits concurrently
3. B's DMA completes FIRST → B publishes coverage for R (newest, per law 6)
4. A's DMA lands LAST → the device now holds A's OLDER bytes under R,
   while coverage and generation both say B
```

No amount of generation filtering at the coverage/read layer can repair
step 4 — the stale bytes are *on the device* at the address the record
serves and will eventually publish. Therefore:

> **Two overlapping stores to the same overlay destination may never be
> in flight concurrently.**

**The v1 protocol:**

* **Disjoint ranges run concurrently** via page/range claims on the
  record — `placed_core::PlacedClaims` is the named precedent (the
  page-claim overlap exclusion the placed-sever assemblies already run,
  with its loom-verified seal-vs-claim discipline).
* **Overlapping ranges queue/serialize**: a segment whose claim is
  refused waits for the holder's CQE (or takes the ordinary
  accumulation fallback if the record froze meanwhile).
* **The claim releases only after the store CQE** — success or failure;
  the release is what re-admits the range, so step 4 is
  unrepresentable.
* **Generations remain** for read invalidation (§5.2's revalidation
  word) and logical ordering (which segment's bytes the coverage
  publication credits) — the claim and the generation answer different
  questions and both stay.

**Rejected simpler alternative:** holding the block's
`BLOCK_FLUSH_LOCKS` across the DMA. It trivially provides exclusion but
serializes the common cohort — a 4 MiB block legitimately receives four
1 MiB segments concurrently (the kernel-split shape RW3b exists for),
and a lock-across-fabric-RTT per segment is exactly the serialization
the write pipeline's detached-task restructure deleted
(design-rewrite-program §4.2). Range claims keep the disjoint cohort
parallel and tax only true overlaps, which are the supersession shape
anyway.

## 3. Reuse map

Nothing in §2 mints a new durable structure or a second publish path.
The table is normative — a PR that builds a private replacement for a
row below is wrong by construction:

| Existing machinery | Where | What the overlay reuses it for |
|---|---|---|
| `BlockAllocator::allocate_block` + `InflightAllocGuard` | `src/block_allocator.rs` | law 2's reservation; the fsck C2/C3 in-flight-registry exemption (the `RewriteEpoch` precedent — a crashed daemon drops the guards and shields nothing). **Rev 2, correction A: the guard is fsck visibility ONLY — it is not the rollback owner and dropping it frees nothing** |
| `MintedBlockGuard` (+ the `OwnedTaskSet` salvage discipline) | `src/assembly_tasks.rs` (RES-9) | law 9's **mint rollback owner**: any exit between mint and durable publication — error, panic, cancel, fence, supersession — frees the offset instead of leaking an allocated-but-unpublished block; disarms only at ownership transfer (the durable map/ref publish). The overlay destination is exactly the RES-9 window, held longer |
| `placed_core::PlacedClaims` | `src/placed_core.rs` (via `src/placed_sever.rs`) | §2.3's range-claim overlap exclusion — the page-claim grant/refuse/release protocol (and its loom-verified fence discipline) applied to overlay destination ranges; the claim releases at the store CQE instead of the merge |
| `publish_block` incarnation stability | `src/block_allocator.rs`, `src/incarnation_core.rs` | the dest offset stays **unstable** from reservation until the record's coverage is fsync-complete — racing validated read-tier fills of a reused key fail their seqlock re-check instead of caching pre-DMA bytes, exactly as on the write-through path |
| `RewriteEpoch` / `rewrite_shadow_record` | `src/routing.rs` (design-rewrite-program §5) | **pending publication — ONE pending-binding authority, decided at B4, never two** (Rev 2, correction B): rewrite shadow is default-ON, so before overwrite overlays arm, exactly one coexistence arm holds — completed overlays feed `rewrite_shadow_record()` immediately, or overlay installation force-closes/disables the shadow for that inode (§8's B4 entry enumerates the five dual-authority hazards). The epoch's swap, deferred-free law, fencing law (KD-1.8), refetch-compose belt (KD-1.9) and supersession coherence (KD-1.11) all transfer verbatim; **B8 is the cleanup/refactor that deletes the transitional plumbing, not the first integration point** |
| The layout-merge transaction | `merge_block_mappings[_if_epoch]` / `set_layout_and_size` / `merge_layout_and_size` + `pending_block_refs` (`src/routing.rs`, `src/meta_ship/publish.rs`) | law 7: one KvTx carries map flip + ref deltas, O(batch); the publish conveyor (Lever B) and delta records compose unchanged |
| The read rebind ladder | `stale_binding_rebinds` / `stale_binding_escalations` (`src/routing.rs`) | the §5 protocol's retry/fallback arm: a read that keeps losing revalidation rides the SAME bounded-rebind-then-serialized-settle ladder the displacement churn path uses — no new EIO class, no new retry machinery |
| The read-window escalation | `overlay_window_escalations` (`src/fuse_client.rs`, 2026-08-15 round 3 — the generic/795 window class) | a read window whose post-read validation failed (a live record inside the window, or custody-fingerprint churn past the bounded lock-free retries) re-serves per block under `BLOCK_FLUSH_LOCKS` (3) + `INODE_META_LOCKS` (3.5) — the rebind-starvation serialized-settle law applied to the whole window (`read_window_settled`): the serve is correct by lock order, not probe completeness. Growth is a latency signal under overlay/publish churn, never a correctness one; the pre-round-3 "serve the last compose" exhaustion arm is retired |
| `free_block` + S7 quarantine + free-grace | `src/block_allocator.rs`, `src/free_grace.rs` | law 8's terminal free of the displaced binding rides the full ladder (begin_free → tier purge → reclaim/elision → finish_free), composing with the dead-epoch quarantine and the §6.8-item-3 grace ring untouched |
| `record_write`'s coverage union | `src/cache/active_block.rs` | **extracted into a shared pure core** (`src/coverage_core.rs`, `#[path]`-included by `active_block.rs` and the overlay record — the `incarnation_core`/`numa_core`/`thp.rs` convention): primary run + sorted disjoint extras + completion transition, overlap-safe, order-blind. One coverage law for RAM accumulation and device accumulation; drift between them becomes unrepresentable |
| `zc_write_fd` + `authorize_zc_store` | `src/nvme_dev.rs` | the store fd and the RES-6/S7 authorization gate, already built and already run by the W1 direct leg — every overlay submission passes the same single authorization point |
| The generic/209 contract | `tests/write_visibility_tests.rs:847-949` | the acceptance oracle for §5 — the storm test and its serialized discriminator run against every read-composition PR |

## 4. Submission topology

### 4.1 The ring is the FUSE queue ring — not the NvmeBlockDev lanes

The slot's registered buffer exists **in the queue ring's sparse
fixed-buffer table** (patch 0024 installs the client's pages at the
ent's slot index at delivery). A `WRITE_FIXED` naming that buffer index
can only be submitted on the ring that owns the table — the per-CPU
FUSE queue ring the request arrived on. Therefore:

* The direct store rides `zc_write_fd()` (the `O_DIRECT | O_WRONLY`
  device fd, `src/nvme_dev.rs`) submitted from the queue worker's ring —
  the exact vehicle the W1 patch leg already uses.
* **`SQUEEZEFS_NVME_WRITE_LANES` and the `NvmeBlockDev` worker pool are
  not this path's fan-out** and must not be reached for: the lanes fan
  out `write_block` submissions from pooled buffers on dedicated worker
  rings; an overlay store has no pooled buffer and no worker — routing
  it there would mean extracting first, which is the path being deleted.

### 4.2 Spread: ring-affinity is the remedy

The FUSE transport already runs **one queue per possible CPU**; the
kernel dispatches requests to the submitting CPU's queue, so a
multi-stream writer's stores are spread across rings — and per-CPU rings
map to different blk-mq software queues and therefore different
NVMe/NVMe-oF fabric queues. The spread instrument is therefore
**per-qid**, and the remedy for an unspread profile is ring-affinity
(where the writing threads run), never a lane-count knob. Precedent to
respect from the counter-confirmation gate (§3f sequence step 1): an
unspread `data_write_lane_submits` on the extraction path is a
regression to fix *before* B is built — B inherits the same law with
its own census.

### 4.3 Accounting + failure ladder

* **Per-qid/device direct-store accounting is B's engagement
  instrument**: `overlay_store_submits` as a per-device, per-qid vector
  (the `data_write_lane_submits` shape) — a row whose stores all sit on
  one qid is a fabric-queue collapse to fix, and the stats inode carries
  the **fabric-queue census** (distinct qids engaged per device per row)
  so the spread verdict needs no external tooling.
* Failure ladder, per submission: refusal/short-CQE → the range never
  joins `completed` (law 3) → the segment falls back to the extraction
  vehicle (`ZcWriteSlot::materialize`, memoized) and the ordinary
  accumulation path, **loudly counted** (`overlay_store_fallbacks`).
  Fallback is correctness, engagement is the instrument — the placed-
  sever posture. A fence/custody refusal (`authorize_zc_store`) is NOT
  a fallback: it fails the write loud through the common exit (a fenced
  holder must not reach a second submission path — the W1 leg's rule,
  verbatim).

## 5. The generic/209 surface (highest risk)

### 5.1 Why this is the risk

Every read-visibility mechanism shipped so far serves **immutable
snapshots**: an `ActiveBlockBuf::snapshot()` is immutable forever (CoW),
a published block's bytes never change under its incarnation word.
**Device offsets under an open overlay have no immutable snapshots**: a
newer-generation segment can be DMA-ing over the very range a reader is
fetching, and the device gives no CoW. The generic/209 contract
(`tests/write_visibility_tests.rs:847-949` — a byte whose write
COMPLETED before the read began must never read the previous pass) is
exactly the contract a naive overlay read breaks, and its convicted
windows (the remove→mutate→reinsert checkout, the uncomposed multi-block
read) show how transient one-write-behind serves happen. This section
is therefore its own PR (B3) with the storm test as its oracle.

### 5.2 The read protocol

The protocol runs for any read touching a block with a live overlay
record. The probe that decides "live" (Rev 2 — Resolved Questions #5,
confirmed): a **global `overlay_open == 0` fast path** first — one
relaxed gauge load, so every mount with no live overlay pays nothing
(the parked-overlay pattern) — then the exact `(ino, block)` registry
lookup. A per-ino count/epoch gate is built only if B3's measurement
shows the exact lookup in `read_serve_phase_ns`; a Bloom filter is
**explicitly rejected pre-evidence** (a probabilistic screen in front
of a correctness-bearing precedence check earns its complexity only
with a profile). With a live record:

```
1. SNAPSHOT   g ← record.generation; C ← record.completed;
              dest ← record.dest              (one consistent probe)
2. FETCH      covered = range ∩ C  → read dest's device ranges
              gaps    = range \ C  → old_binding bytes, or zeros  [law 5]
3. REVALIDATE g' ← record.generation; C' ← record.completed;
              dest' ← record.dest
              valid ⇔ g' == g  ∧  C' ⊇ C over the fetched ranges
                     ∧ dest' == dest (destination identity)
4a. valid     → serve
4b. invalid   → RETRY from 1 (bounded), then the rebind ladder's
                serialized-settle arm (the §3 rebind-ladder row) —
                never EIO for legal writer churn, never a stale serve
```

Notes, each load-bearing:

* **Generation equality, not ≥**: `g' > g` means a same-block write
  landed mid-fetch — the fetched bytes may straddle old and new (the
  torn read). Coverage-only revalidation cannot catch an *overwrite* of
  an already-covered range; the generation can. This is law 6's read
  face.
* **Destination identity**: `dest' == dest` guards the
  publish/teardown race — a record that published and was rebuilt (or
  superseded and re-opened) between snapshot and revalidate would pass
  a generation check reset to 0. Identity is (backend, offset,
  guard-birth) — never offset alone (offsets are reused).
* **Record-gone at revalidate** = published: re-resolve through the
  (now durable) map — the ordinary read path; the just-published bytes
  are the same bytes at the same offset, so the retry is one map read,
  not a refetch.
* The uncovered-gap fetch through `old_binding` runs the EXISTING read
  ladder (tiers, incarnation validation, rebind) — the overlay adds
  composition, not a second fetch engine.

### 5.3 The failed-direct-read law

> **A failed revalidation must overwrite the ENTIRE destination before
> the reply commits.** Any read that lands bytes in a caller-visible
> destination — a zc read-dest (registered kernel window), an IPC arena
> window, a reply buffer — and then fails step 3 must not reply until
> the retry (or the gap/fallback serve) has rewritten **every byte** of
> the destination window. A partially-rewritten destination is a torn
> serve wearing a valid header: the revalidated retry may legitimately
> serve *different* byte ranges from *different* sources (coverage
> moved), so byte-wise "only fix what changed" reasoning is
> unsound — the whole window is re-served or the read fails loud.

This is the overlay's edition of the `read_dest_overruns` hygiene rule
(a dest is either fully honest or refused), and it is why **composed
reads run before zc read-dests** in v1 (§7): the dest-lease direct-DMA
arm (design read-dest-lease) may only serve ranges the registry proves
overlay-free at lease time, until PR B7 teaches the lease the full
protocol.

### 5.4 Tier precedence under an open overlay (Rev 2, correction 4)

Overlay **bytes** are never admitted to the read tiers while the
record is open (their incarnation word is unstable by §3 — admission
would fight the seqlock by design). But the **old binding's** tier
entries are a different population, and the Rev-1 purge-on-install
posture was wrong about them:

* **Old-binding cache entries are PRESERVED while the overlay is
  open.** They are immutable (the old key's content never changes —
  CoW), and they are the **gap-composition source**: law 5's uncovered
  serves resolve through the ordinary read ladder, and evicting warm
  old-binding entries at install would turn every gap serve into a
  device read for no correctness gain.
* **They are purged only at durable displacement** — the publication
  epilogue, where the displaced-key purge law already runs (the same
  point every displacing merge purges today).

**The precedence law:** *overlay presence outranks every direct tier
serve.* On a block with a live record: covered ranges serve from the
overlay destination (law 4); gaps may serve from **valid** old-binding
tier entries (incarnation-checked, as any tier serve); no tier may
serve the block directly — i.e. without having run the §5.2 protocol —
while the record lives. **A fast path that cannot perform the
precedence check DEMOTES rather than purging base data**: the hot-block
fast hit, the read-lane hold, the IPC §5.5.1 sync serve and any future
short-circuit either run the probe or hand the op to the composed path
— they never "solve" the coherence problem by evicting the old-binding
entries they cannot compose (that trades a correctness check for a
warmth loss AND still serves stale on the covered ranges).

### 6.1 Posture: volatile overlay, no new on-disk format

The overlay is RAM state over ordinary allocated-but-unpublished blocks
— the exact durability class of a mid-epoch rewrite shadow or an
in-flight write-through: **the old durable map stays authoritative
until publication.** No incompat bit, no journal record, no sidecar
file. A crash at any point recovers by the existing arithmetic: the
recovery census (the mount walk that seeds refcounts from durable maps
only) finds the overlay's destination offsets referenced by no durable
map ⇒ **free-listed** (unpublished offsets reclaimed); the file reads
its pre-overlay image; un-fsynced ACKed writes are lost — the
writeback-class contract, unchanged and stated. Torn publication is
impossible by v3 §4.10 (whole-tx, torn-write-immune).

### 6.2 The fsync sequence

fsync (and every durability boundary: flush legs, RELEASE-last-close,
unmount drain) runs, per ino, in order:

```
1. FREEZE     every open overlay on the ino: state Open → Frozen —
              no new segment may install; arriving writes queue behind
              the freeze (or open a successor record AFTER step 6)
2. COMPLETE   await the inflight set: every submitted WRITE_FIXED
              reaps its CQE (success → coverage; failure → §4.3
              fallback bytes staged the ordinary way)
3. SEED GAPS  uncovered ranges of each frozen overlay are made whole:
              read old_binding_or_hole bytes (or zeros) and store them
              to the overlay dest's gap ranges (pooled DMA — this is
              the one copy a PARTIAL overlay pays; a fully-covered
              overlay pays nothing)                     [law 5's dual]
4. FLUSH      data device barrier(s): NvmeBlockDev::flush per touched
              backend (DUR-2) — the stores are durable before anything
              names them                                 (DUR-1 order)
5. PUBLISH    layout + refs in ONE tx per the existing publish
              primitive (law 7); coalescing/delta economy unchanged
6. META SYNC  the metadata journal barrier (the existing fsync tail);
              then law 8's deferred frees of displaced bindings run
```

**A successful fsync may never leave an overlay unpublished** on the
ino it covered: post-fsync, the registry holds no record for the ino
(or only successor records opened after the freeze). This is the
invariant that keeps the overlay invisible to the durability contract —
fsync means the same thing it meant yesterday, and the DUR-1 ordering
(data barrier strictly precedes the metadata barrier) is preserved by
construction because step 4 precedes step 5.

Gap-seeding rationale (Rev 2 — Resolved Questions #2, **confirmed**):
seeding into the unpublished destination makes every published overlay
a **whole block** — one map op, no extent records, no partial-block
layout format. The two W2-shaped alternatives were rejected on their
defeat conditions, stated so they are not re-proposed:

* **Publish partial coverage as extent records** — defeated by
  **read-back into extent payloads**: the overlay's covered bytes sit
  on the DEVICE, so minting `active_block_ext:` records for them means
  reading back at fsync what was just direct-stored, paying a device
  read per durability boundary to re-materialize bytes the whole design
  exists to never touch again.
* **A durable partial-coverage map form** (block bound to old key +
  overlay dest + a coverage descriptor) — defeated by **a durable
  two-source block meaning**: it mints a NEW on-disk semantics in which
  one block's content is defined by composing two sources, which every
  reader, fsck class, mover, clone and recovery walk would have to
  learn — an on-disk format change smuggled in as an optimization,
  against the volatile-overlay posture (§6.1).

### 6.3 Crash windows

The rewrite-shadow crash-window table (design-rewrite-program §5.6)
transfers with the overlay's dest playing B and `old_binding` playing A:
mid-overlay crash ⇒ W1 (map = old entire, dest offsets unclaimed ⇒
free-listed); torn publish ⇒ W2 (≡ W1); publish durable, frees not run
⇒ W3 (recovery frees the unreferenced old keys); fenced pre-publish ⇒
W5 (publish refused, nothing freed, successor recovery owns all
accounting — `overlay_fence_drops` loud, must-stay-0 healthy). The one
NEW window is **mid-fsync between steps 4 and 5**: stores durable at an
offset no map names — identical to W1 by the recovery census (durably
written ≠ durably *named*; the offsets free-list). No window leaks and
no window double-frees, because the terminal free's durable effect
remains the Delete that rides the publish (the durable-block-refcounts
law).

## 7. v1 hard gates (scope fence)

All structural refusals at the overlay install point — an ineligible
shape never installs a record and rides the extraction/accumulation
path unchanged (fallback-is-correctness):

| Gate | Rule | Why |
|---|---|---|
| **Passthrough only** | transformed (compression/AEAD) volumes never install overlays | the impossibility argument below |
| **Single-writer** | the D0 write mount only; **no co-writer custody** — S9 co-writers keep the extraction path (a co-writer's direct store would need the custody-epoch composition of PR B9) | custody epochs + lane grants are a separate campaign (PR B9, explicitly out of v1) |
| **One-block, 4 KiB-aligned segments** | a segment must lie within one block and be LBA-aligned (offset and length 4 KiB multiples — the O_DIRECT contract `zc_write_fd` carries) | unaligned edges need RMW, which needs a read, which is the accumulation path's job; kernel-split segments are per-block anyway (the RW3b lesson) |
| **No write-verification** | `--write-verification` mounts decline the direct leg | the pooled path's window-exact read-back covers only `write_block` — the W1 leg's rule, verbatim |
| **Composed reads before zc read-dests** | the dest-lease direct-DMA read arm serves only ranges the registry proves overlay-free, until PR B7 | §5.3's law — a lease cannot revalidate mid-DMA |
| **Clone / truncate / punch / delete drain-or-supersede** | shape-change ops on an overlaid ino first freeze-complete-publish the overlay (drain) or mark it `Superseded` (their own primitive owns durable state; the record unwinds per law 9) — matching the rewrite-shadow close-trigger table + KD-1.11's supersession coherence | one authority per family; a clone of a half-overlaid block must see either the published whole or the old binding, never the registry |
| **Already-authoritatively-striped files only** (Rev 2 — Resolved Questions #6, amended) | overlays install only on inos whose layout authority IS striped — never during a promotion; §7.1 | promotion custody + staged records + unpublished overlays never compose in v1 |

**The compression/AEAD impossibility argument (stated, not assumed):**
a transformed volume's stored image is a whole-block frame whose length
differs from the plaintext's and is unknowable until the transform has
run over the **complete** plaintext (codec output size + AEAD tag +
header; the incompressible store-raw escape decides raw-vs-compressed
per block, after the fact). An incremental device-visible segment store
writes plaintext-shaped bytes at plaintext-shaped offsets; there is no
address inside a transformed frame where "bytes 128 K–256 K of the
plaintext" can land independently, no way to seal an AEAD tag over a
block that is still accumulating, and a partially-stored frame is
undecodable and unverifiable by every reader including recovery. The
transform is a whole-block barrier **by math, not by policy** — so
transformed volumes structurally keep the accumulation path, and no
future PR should attempt to lift this gate without changing the frame
format itself.

### 7.1 Layout eligibility: striped authority only (Rev 2 — Resolved Questions #6)

The Rev-1 phrase "striping-eligible fresh" is sharpened: it means
**AFTER promotion has fully drained AND published — never during**. The
fresh-file flow, stated so the boundary is unambiguous:

```
inline/staged prefix        (ordinary machinery — no overlays)
  ── growth crosses the striped threshold
  ── PROMOTION: staging drains, the striped layout PUBLISHES
     (the existing promotion path, untouched)
  ── the ino's layout authority is now striped
  ── SUBSEQUENT blocks may overlay                    (install admits)
```

**The excluded transition, named:** promotion custody (staged segments
being drained), durable staged records (`file_id`/`mapping:`/
`active_block_ext:` custody), and unpublished overlay destinations are
three pending-authority populations on one ino — **they never compose
in v1**. An overlay may not install while any promotion is in flight
or any staged-family custody is live on the block's ino-range; the
install predicate checks layout authority (striped) and staged-custody
absence, and an ineligible write simply rides the accumulation path
(fallback-is-correctness, as everywhere in §7).

## 8. The PR ladder

Nine PRs, each independently mergeable off `dev`, tests-first, full
required gate. **Falsification-first discipline on every rung**: each
PR states, before implementation, the measurement or contract test that
would prove its mechanism wrong (the Approach-A posture — a bounded
experiment, not a commitment), and a falsified rung stops the ladder
with its evidence note rather than being patched into place.

| PR | Scope | Correctness gate | Falsifier |
|---|---|---|---|
| **B1 — pure state core** | `src/overlay_core.rs` (dependency-free): the record state machine, generation/coverage/inflight words, **the §2.3 range-claim overlap exclusion** (grant/refuse/release-at-CQE as pure transitions — the `PlacedClaims` shape), laws 1–3/6/9 as pure transitions; `record_write`'s union extracted into the shared `coverage_core` (`#[path]`, both consumers); property tests (proptest: arbitrary segment schedules never violate the laws — incl. that no schedule ever grants two overlapping in-flight claims); loom model for the snapshot/revalidate word protocol (§5.2 is lock-free on the reader side — the house extracted-core convention) | property + loom suites green; `active_block.rs` byte-identical behavior through the extracted core (the existing coverage suites re-run unmodified) | a law that cannot be expressed as a pure transition (would mean the state machine is under-specified — redesign before any I/O lands) |
| **B2 — sync reservation + fresh/hole stores** | install/reserve/store/CQE wiring for the SAFEST shape: blocks with **no old binding** (fresh allocations and holes — `old_binding_or_hole = Hole`, so law 5's gaps are zeros and no displaced free exists); fsync = freeze/complete/seed-zeros/flush/publish; per-qid accounting (§4.3) | engagement on a fresh-file 1 MiB stream (stores ≈ user bytes, extraction ≈ 0); `rewrite_amp ≤ 1.05`; generic/209 storm green (fresh-file shape); recovery census reclaims unpublished offsets after kill-9 (red-first) | the fresh-shape bracket shows no material win over extraction — the loop was not the cost and the program stops here with the profile to prove it |
| **B3 — read composition** | the §5.2 protocol + the §5.3 failed-dest law; registry probe on the read path; old-binding gap serves; rebind-ladder integration | `tests/write_visibility_tests.rs` generic/209 storm + serialized discriminator green **with overlays engaged**; torn-read test (planted mid-fetch overwrite via the stall seam) red-first; recycled-content leak test (law 5) red-first | read-side revalidation retries measurably tax the read rows (> noise on the read program's standing brackets) — the protocol is redesigned before overwrite shapes arm |
| **B4 — gap seeding + rewrite-shadow coexistence** *(amended Rev 2, correction B)* — **DONE 2026-08-15** (the B4a–B4e ladder of `docs/design-overlay-overwrite.md`; adjudication `.benchmarks/2026-08-15-overlay-b4-overwrite.md` — coexistence arm (a) shipped, the five hazards pinned red-first, default ON field-adjudicated) | partial-overlay completion: seed uncovered ranges from `old_binding` bytes at fsync (§6.2 step 3); overwrite shapes (old binding present) arm HERE — law 8's deferred free of the displaced key. **Rewrite shadow is default-ON, so B4's overwrite overlays create a second pending-binding authority on the same inos. The five dual-authority hazards, enumerated: (1) two displaced-old queues** (the epoch parks displaced A keys AND the overlay records `old_binding` — one displaced key parked by two owners); **(2) duplicate deferred frees** (both authorities free the displaced key after "their" publish — a double free); **(3) stale refetch composition** (KD-1.9's compose can resurrect a shadow binding an overlay publish already displaced-and-freed — the KD-1.11 poison class); **(4) competing fsync publication** (two authorities each owning "the" binding save for one block at one durability boundary); **(5) duplicate/missing ref ops** (block-ref take/release staged by both paths, or by neither, either way `meta_kv_block_refs_drift`). Before overwrite overlays ARM, exactly ONE coexistence arm holds: (a) completed overlays feed `rewrite_shadow_record()` immediately (the overlay dest becomes the epoch's B key, the epoch owns publication), or (b) overlay installation force-closes/disables the shadow for that inode (the overlay owns publication). The chosen arm is B4's first commit, red-first against each hazard | A-B-B-A overwrite bracket: amp ≤ 1.05, zero mid-row discards (elision composes); the seed pays only on partial overlays (`overlay_gap_seed_bytes` ≈ 0 on full-coverage streams); **the five hazards each pinned red-first (double-free, resurrection, drift, competing-save, duplicate-park) with shadow default-ON** | seed traffic on real streams is material (kernel-split coverage rarely completes) — the eligibility predicate narrows or the ladder stops |
| **B5 — fsync/publication hardening** | the full §6.2 sequence under storms — **with rewrite shadow default-ON and the B4 coexistence arm engaged** (correction B: coexistence is proven BEFORE the gate row, not deferred to B8); freeze/successor-record semantics; unmount drain; fence drops (W5); **closes the B-initial gate** (§1.3) | fsync-storm suites; `overlay_fence_drops` 0 healthy; **the B-initial measurement row: materially above the §1.4 four-case comparator and ≥ 0.80× same-day raw, sustained, reset-per-leg A-B-B-A** | the gate row fails ⇒ the residual is profiled and named before any further rung (the internal-time program's discipline) |
| **B6 — truncate/punch/clone/delete/mover** | drain-or-supersede for every shape-change op (§7 row 6); mover `MergeExpected` composition; KD-1.11-class supersession coherence tests | the rewrite-shadow supersede suite's shapes re-pinned against overlays; fsck composition (mid-overlay scan: `fsck_findings` 0 via the guard exemption) | — (pure correctness rung; its falsifier is a red test) |
| **B7 — zc read-dests** | teach the read dest-lease arm the §5.2 protocol (revalidate before lease commit; §5.3 full-overwrite on failure) so overlay-covered ranges regain the direct-DMA read leg | dest-lease suites + generic/209 with leases armed; `read_dest_lease_bytes` engagement returns on overlaid files | the lease/revalidate composition costs more than the lease wins on overlaid shapes — the v1 gate (composed-reads-first) simply remains |
| **B8 — coexistence cleanup** *(amended Rev 2, correction B: no longer the first integration point)* | the integration decision landed at B4; B8 **deletes the transitional plumbing** the chosen arm left behind — if B4 chose arm (a), the overlay's own publish path goes (the epoch is the one authority) and the feed becomes the only exit; if B4 chose arm (b), the per-ino disable seam and its counters go once the feed is built and proven. Either way: one durable-publish authority, zero transitional branches, gauges reconciled | journal-entry-count equality vs B5 (cleanup adds no commits); the shadow suites green with overlay-fed epochs; the no-dead-code rule discharged on the deleted arm | — (refactor rung; falsifier is the equality pin) |
| **B9 — multi-writer** | **separately, after S9 arms**: custody-epoch stamping of overlay records, co-writer store authorization, shipped publication composition | out of scope here — its design note extends `docs/design-mw-layout-versions.md` when scheduled | — |

**Accelerator (parallel, not a rung): the payload-retention kernel
patch** — the 0029 draft charter (`docker/kernel-sqz/`; the same-day
kernel-patch charter §3f records as stopped pre-work, re-queued as B's
accelerator). It lets the transport ACK the WRITE while the slot's
pages remain registered (**ACK-early**): law 1 still holds (the record
installs first), law 3 unchanged, but the handler no longer waits for
the device CQE before replying — the store completes asynchronously
against the retained pages, and the ent re-arm defers exactly like a
§5.4 payload lease. It is an **accelerator, explicitly not a
prerequisite**: every rung above is correct and measurable with
ACK-after-CQE (Rev 2 — Resolved Questions #1, confirmed: v1 keeps the
W1 `ZcWriteSlot::store()`-await discipline, `src/fuse_client.rs`
`:10848-10860`); the accelerator moves ACK latency, not bytes. It lands
(if it lands) with its own kernel-series review and a negotiation
probe, per ruling D13's custom-kernel sanction — stock kernels keep
ACK-after-CQE.

**O_DIRECT class (daemon, 2026-08-09 live-smoke):** 0-copy retain is
sound only for page-cache folios. O_DIRECT/GUP pages are legally
reusable the instant write(2) returns; DMA-from-retained-GUP after ACK
aliased later-chunk bytes onto earlier dests (dd `oflag=direct`,
6553/8192 pages, first striped block exact because it still rode
promotion ACK-after-CQE). The NFS UNSTABLE analogy in kernel §3.4 is
the *sampling* contract (bytes frozen at ACK), not "DMA whenever".
The daemon's O_DIRECT opt-in (`SQUEEZEFS_ZC_ACK_EARLY_ODIRECT`)
therefore SNAPSHOTS (extract) before the reply and DMAs the snapshot —
still ACK-before-device-CQE (the depth-cap release), one local copy,
safe under buffer reuse. Page-cache retains stay 0-copy.

**The accelerator's future law (Rev 2 addition — recorded now because
ACK-early changes read synchronization and fsync ownership, not just
payload lifetime):**

> *A read intersecting an ACKed-but-incomplete store must wait for that
> store or serve from its retained source; it may never treat the range
> as an uncovered old-map gap.*

Under ACK-after-CQE this situation is unrepresentable (an ACKed range
is a completed range); under ACK-early an ACKed-but-in-flight range is
a new read-protocol state — RYW demands the NEW bytes, coverage says
"not stored yet", and law 5 naively read would serve the OLD binding.
The retained slot pages are the legitimate serve source (or the read
parks on the CQE). The same shift moves fsync ownership: §6.2 step 2's
COMPLETE arm must await stores the *kernel* believes are already done —
the accelerator PR owns restating both, and this paragraph is the
contract it restates against.

**Parked: the custom fixed-copy opcode** (a kernel opcode copying
fixed→fixed buffers, which would let Approach A's assembly skip the
memfd round-trip). Parked because it inherits A's ceiling (still one
copy per byte) for more kernel maintenance; **its un-park condition,
verbatim from the adjudication: only if A shows the memfd arena itself
is the limit.**

## 9. Stats surface + measurement gates

### 9.1 New counters (stats inode)

| Family | Fields | Semantics |
|---|---|---|
| Engagement | `overlay_installs`, `overlay_stores`, `overlay_store_bytes`, `overlay_store_submits` (per-device per-qid vector — the fabric-queue census, §4.3) | the extraction-delete instrument: on an eligible row `store_bytes` ≈ user bytes while `zc_write_extract_bytes` and `nt_copy_bytes` ≈ 0; the qid vector is the spread verdict |
| Coverage / overlap | `overlay_supersessions` (law 6 newest-wins), `overlay_short_stores` (law 3 refusals), `overlay_store_fallbacks` (§4.3 ladder) | supersessions grow on hot-overlap loops by design; fallbacks are loud engagement-loss, ≈ 0 on eligible shapes |
| Read protocol | `overlay_read_serves`, `overlay_read_gap_serves`, `overlay_read_retries`, `overlay_read_settles` (the serialized arm) | a §5 row is INVALID unless serves account for its overlay-covered reads; retries growing without settles is the protocol working, settles growing is churn outrunning it (pair with `stale_binding_escalations`) |
| Publication | `overlay_publishes`, `overlay_published_bytes`, `overlay_gap_seeds`, `overlay_gap_seed_bytes` | seed bytes ≈ 0 on full-coverage streams (the B4 falsifier's instrument) |
| Lifecycle (gauges) | `overlay_open` (records), `overlay_inflight_bytes` (R5 component — non-sheddable in-flight custody, the `write_pipeline_inflight` pattern: Red clamps admission, converges by completion) | `open` → 0 at quiesce (the `rewrite_shadow_open_epochs` law); inflight rides the budget |
| Tripwires (must stay 0) | `overlay_fence_drops`, `overlay_teardown_waits` timing out (law 9), `overlay_unpublished_at_fsync` (the §6.2 invariant, counted rather than assumed) | investigate alongside `writer_guard_fenced` / `invariant_tripwires` |
| Recovery | `unpublished_offsets_recovered` (mount census reclaims) *(renamed Rev 2, correction C — was `overlay_recovered_offsets`)* | the overlay is volatile with **no durable provenance, by design** (§6.1) — after a crash the census cannot tell an overlay's destination from any other allocated-but-unpublished offset (an in-flight write-through, a shadow B block), so an overlay-named counter would be a lie wearing a stat name; the honest generic name counts what the census actually knows. Nonzero after a crash is the design working; growth on clean remounts is a leak. **Overlay-specific recovery attribution lives ONLY as a fault-injection test assertion** (the harness knows which offsets it minted; production never does — stated as such in the B2 recovery tests) |

### 9.2 Measurement gates (standing rules, restated as this program's law)

* **Ratios, not absolutes**: every gate is a fraction of the SAME-DAY
  raw ceiling or an A-vs-B ratio on the same venue — the 49.5 GB/s
  class is a class, not a constant; the raw row re-runs in every
  bracket.
* **Reset-per-leg A-B-B-A**: the venue AGES (free-list shape, thin
  state, staging temperature) — every comparison leg starts from a
  reformatted/reset store, both orders, both brackets cited. A
  single-order delta is an ordering artifact until reversed.
* **Amplification ≤ 1.05** on every write row, with the standing
  columns: device bytes ÷ user bytes on the data namespace, `wareq-sz`
  vs block size (no request-size collapse), `block_free_*` deltas, and
  **tripwires 0** (`overlay_*` must-stay-0 set, `data_dma_fence_refusals`,
  `write_pipeline_fence_drops`) on every row.
* **Sustained rows govern** (≥ 60 s, flat); substrate law: nvmet-tcp or
  the field fabric for acceptance, loop devsub for scoping only; every
  row states instrument + substrate.
* **Row validity is engagement closure**: a B row whose
  `overlay_store_bytes` + extraction bytes + accumulation-path bytes do
  not account for its user bytes is INVALID (charter rule 4).

## 10. Lock order & invariants (P1-9 audit)

* Overlay install/record: (3) `BLOCK_FLUSH_LOCKS(ino, b)` for the
  record mutation; the `old_binding` capture takes (3.5)
  `INODE_META_LOCKS` inside it — the existing merge-under-block-lock
  order. No 4a/4b (no commit at install).
* Publication: the fsync path's existing order — (1)/(2) as today,
  publish under `INODE_META_LOCKS` → 4a/4b inside the commit conveyor;
  displaced frees strictly after the guard drops (RES-1: never a
  terminal `free_block` under 3.5).
* Reader protocol: **no locks** — snapshot/revalidate on the record's
  atomic words (the B1 loom model's subject).
* Device commands: every store passes `authorize_zc_store` (the D0
  latch + S7 authorization point); the reclaimer/trim fence-halt and
  the `failed` latch remain the only destructive-I/O gates.

## Key Decisions

| # | Decision | Rationale |
|---|---|---|
| KD-OV-1 | **The overlay is visible, not shadow-buffered**: reads compose over live device offsets (laws 4–6) instead of retaining a RAM mirror of stored bytes. | Retaining a mirror IS the accumulation buffer — the thing being deleted. The price is §5's revalidation protocol; the generic/209 suite is the oracle that the price was paid correctly. |
| KD-OV-2 | **One coverage law**: `record_write`'s union extracted into `coverage_core`, shared by RAM and device accumulation. | The RW3b campaign already paid for the order-blind union once (FIND-L1-A); a second implementation would re-earn its bugs. Drift becomes a red build, not a review item. |
| KD-OV-3 | **Generation word over bitmap-only state** (law 6). | A bitmap answers "stored?", never "which of two racing stores is newest" — the newest-wins verdict is an ordering fact, and the CQE-supersession law (design-rewrite-program §4.3) needs the ordering word on both the write and read sides. |
| KD-OV-4 | **Stores ride the FUSE queue rings, spread by ring-affinity** — never the `NvmeBlockDev` lane pool, never a lane-count knob. | The sparse-slot buffer index only exists on the ring that registered it (§4.1); a lane detour would re-introduce the extraction. Per-qid accounting makes the spread a measured fact. |
| KD-OV-5 | **Volatile overlay, publication through the existing primitive, consolidation INTO rewrite-shadow** (PR B8) — never a second durable-publish path or a new on-disk format. | The deferred-free law, crash windows, fencing law and refcount transaction are already proven machinery; the overlay's only novel durable act is *when* it calls them. Zero format change keeps every crash window in the recovered-by-existing-arithmetic class (§6.3). |
| KD-OV-6 | **Fsync seeds gaps to whole blocks** rather than publishing partial coverage via extent records. | One map op per block on the durability boundary; the seed DMA is bounded and pays only on partial overlays (`overlay_gap_seed_bytes` is the falsifier's instrument). **Confirmed (Rev 2, Resolved Questions #2)** — the two rejected alternatives' defeat conditions recorded in §6.2. |
| KD-OV-7 | **v1 acks after the CQE**; ACK-early is the retention accelerator's charter, not a rung. | Every ladder rung stays correct and measurable on stock-shaped timing; the accelerator moves latency, not bytes, and lands with its own kernel-series review (ruling D13). **Confirmed (Rev 2, Resolved Questions #1)** — the accelerator's future read-synchronization law recorded in §8. |
| KD-OV-8 | **The failed-direct-read law rewrites the whole destination** (§5.3), never a byte-diff repair. | The revalidated retry may serve different ranges from different sources; partial repair is torn-serve reasoning. Mirrors the dest hygiene rule (`read_dest_overruns` class). |
| KD-OV-9 | **Transformed volumes are excluded by impossibility, not policy** (§7). | The frame math (unknowable stored length, whole-plaintext transform, sealed tag) makes incremental device-visible segment stores unrepresentable; stating the argument prevents a future "just lift the gate" patch. |

The reviewer's five-item **mandatory-before-implementation** list
(engineering review, 2026-08-08 — no B-PR starts before all five are in
the tree as stated):

| # | Decision (Rev 2, mandatory) | Rationale |
|---|---|---|
| KD-OV-10 | **In-flight overlap exclusion is a law, not an optimization** (law 6's mandatory clause + §2.3): range claims held across the DMA, released only at the store CQE; overlapping stores serialize; whole-block-lock-across-DMA rejected. | Generation filtering orders publication but cannot undo physical writes — the §2.3 stale-DMA counterexample is silent newest-loses corruption at the device. The claim protocol keeps the 4×1 MiB disjoint cohort parallel. |
| KD-OV-11 | **Mint rollback ownership** (correction A, law 9 restated): `InflightAllocGuard` = fsck visibility only; every destination carries a `MintedBlockGuard`-class rollback owner (or explicit terminal `free_block()`) on every failure/cancel/panic/fence/supersession path, disarming only at durable map/ref publication. | Dropping the fsck guard frees nothing — without an owning unwind, every abandoned overlay is an allocated-but-unpublished leak that only fsck can see and nothing can safely reclaim while the daemon lives (the RES-9 window, held longer). |
| KD-OV-12 | **Rewrite-shadow coexistence is decided and proven at B4/B5, not B8** (correction B): exactly one pending-binding authority before overwrite overlays arm — feed `rewrite_shadow_record()` immediately, or force-close/disable the shadow per inode; B8 is cleanup only. | Shadow is default-ON; the five enumerated dual-authority hazards (§8 B4) include a double free and the KD-1.11 resurrection class — landing B4 without the coexistence arm ships the run17 poison shape behind a default. |
| KD-OV-13 | **The tier-precedence law** (correction 4, §5.4): old-binding tier entries preserved while an overlay is open (the gap-composition source), purged only at durable displacement; overlay presence outranks every direct tier serve; a fast path that cannot run the precedence check DEMOTES rather than purging base data. | Purge-on-install (Rev 1's posture) destroyed the warm gap source for zero correctness gain; purging-to-avoid-composing would ALSO still serve stale covered ranges — eviction is never a coherence mechanism. |
| KD-OV-14 | **Honest recovery attribution** (correction C, §9.1): the census counter is the generic `unpublished_offsets_recovered`; overlay-specific attribution exists only as a fault-injection test assertion. | No durable provenance is the design (§6.1) — a counter that claims post-crash knowledge the system structurally lacks would train operators on fiction. |

## Open Questions — All Resolved (engineering review, 2026-08-08)

**All seven were resolved by the engineering review of 2026-08-08
(Rev 2; binding).** Kept in place with their resolutions, per the house
annotation pattern (the design-nvmeof-target-management rev-4
precedent), so every cross-reference in this document keeps resolving.
The changes are folded into §1.4, §2.1–§2.3, §5.2/§5.4, §6.2, §7/§7.1,
§8 and the Key Decisions ledger (KD-OV-10..14 — the review's three
additional mandatory corrections A/B/C ride the same revision).

1. **ACK timing in v1** — **RESOLVED, CONFIRMED as written**:
   ACK-after-CQE (the W1 `ZcWriteSlot::store()`-await discipline,
   `src/fuse_client.rs:10848-10860`). The review added the
   accelerator's **future law** to §8's retention entry — *"a read
   intersecting an ACKed-but-incomplete store must wait for that store
   or serve from its retained source; it may never treat the range as
   an uncovered old-map gap"* — because ACK-early changes read
   synchronization and fsync ownership, not just payload lifetime.
2. **Gap-seeding venue** — **RESOLVED, CONFIRMED as written**:
   whole-block seeding into the unpublished destination. §6.2 now
   states the two rejected W2 alternatives' **defeat conditions**
   (read-back into extent payloads; a durable two-source block
   meaning) so they are not re-proposed.
3. **The B-initial comparator when A is falsified** — **RESOLVED,
   AMENDED**: the four-case table (§1.4). A lands → beat A's best
   valid sustained row; A fails performance-but-correct → the shipped
   extraction control; A fails correctness/engagement → A's treatment
   rows are invalid, compare vs A's reset-matched CONTROL legs; every
   case keeps B ≥ 0.80× same-day raw.
4. **Read-tier posture under an open overlay** — **RESOLVED, AMENDED
   (the review's correction 4)**: no overlay-byte admission while
   open, **but old-binding tier entries are PRESERVED** (immutable;
   the gap-composition source) and purged only at durable
   displacement. The new **tier-precedence law** (§5.4): overlay
   presence outranks every direct tier serve; covered ranges from the
   overlay destination; gaps may serve from valid old-binding tiers; a
   fast path that cannot perform the precedence check **DEMOTES rather
   than purging base data**. Rev 1's purge-on-install posture is
   retired.
5. **Registry probe scope** — **RESOLVED, CONFIRMED as written, made
   concrete**: global `overlay_open == 0` fast path → exact
   `(ino, block)` lookup (the parked-overlay pattern); a per-ino
   count/epoch gate only on B3 measurement evidence; a Bloom filter
   **explicitly rejected pre-evidence** (§5.2).
6. **Layout eligibility** — **RESOLVED, AMENDED**:
   **already-authoritatively-striped files only**; "striping-eligible
   fresh" means AFTER promotion has fully drained AND published, never
   during. §7.1 shows the fresh-file flow (inline/staged prefix →
   promotion publishes striped → subsequent blocks may overlay) and
   names the excluded transition: promotion custody + staged records +
   unpublished overlays never compose in v1.
7. **Generation granularity** — **RESOLVED, CONFIRMED as written, with
   a critical addition (the review's correction 1)**: per-record
   generation for read revalidation stands, **plus** the mandatory
   law-6 clause — two overlapping stores to the same overlay
   destination may never be in flight concurrently — with the
   four-step stale-DMA counterexample, the range-claim protocol
   (`placed_core::PlacedClaims` precedent, claim releases only at the
   store CQE), and the rejected whole-block-lock alternative recorded
   in §2.3.

## References

- `docs/rc-manifest.md` §3f — the write-bandwidth program adjudication
  (the normative source), rulings D13/D14/D16/D17, the counter-
  confirmation sequence, Approach A's gates.
- `docs/design-rewrite-program.md` — shadow dual-map (§5), supersession
  (§4), discard elision (§3), the crash-window table (§5.6), KD-1.11.
- `docs/design-zero-copy-write-path.md` — §5.3 coverage/write-through
  law, §5.4 lease severance, the one-merge discipline.
- `docs/design-random-small-writes.md` — the W1 patch class (the
  existing slot→device consumer and its custody rules).
- `docs/design-mw-layout-versions.md` — the multi-writer composition
  PR B9 defers to.
- Code anchors: `src/nvme_dev.rs` (`zc_write_fd`, `authorize_zc_store`),
  `src/routing.rs` (`ZcWriteSlot`, `RewriteEpoch`,
  `merge_block_mappings*`, `pending_block_refs`, rebind ladder),
  `src/cache/active_block.rs` (`record_write`),
  `src/block_allocator.rs` (`allocate_block`, `InflightAllocGuard`,
  `publish_block`, `begin_free`/`finish_free`),
  `src/assembly_tasks.rs` (`MintedBlockGuard` + the `OwnedTaskSet`
  salvage discipline — KD-OV-11's precedent), `src/placed_sever.rs` +
  `src/placed_core.rs` (`PlacedClaims` — KD-OV-10's precedent),
  `src/free_grace.rs`, `src/fuse_client.rs:10848-10860` (the W1
  `ZcWriteSlot::store()`-await discipline — Resolved Questions #1),
  `crates/fuse3/src/raw/connection/zc.rs`,
  `docker/kernel-sqz/patches/0024-fuse-add-zero-copy-over-io-uring.patch`,
  `tests/write_visibility_tests.rs:847-949`.
