# Design: Shared-Mode Write Admission — the Write-Inode-Convoy Campaign

| | |
|---|---|
| **Title** | Shared-mode write admission (the lock-scope half of one-path) |
| **Date** | 2026-08-10 (rev 2 — both review rounds folded) |
| **Status** | DESIGN — review rounds 1+2 addressed; awaiting sign-off |
| **Related** | `docs/design-one-path.md` (parent program — this is the lock-scope half of "one admission, cheapest legal"; NOT a new vehicle), `docs/design-random-small-writes.md` (W1/W2 machinery this admits concurrently), `.benchmarks/2026-08-10-write-inode-convoy-diagnosis.md` (the evidence note — R-M7) |
| **Inviolable** | One admission path (no benchmark fork); any classification doubt routes EXCLUSIVE; lease/DLM layer untouched; block-plane locks untouched; a Shared holder never re-enters `active_inode_locks` on the same ino |
| **Branch (on approval)** | `perf/write-inode-convoy` |

---

## 1. Charter

Random small **writes** are ceilinged by the per-inode `active_inode_locks`
**write** guard — a tokio-wake convoy pricing ≈ 1.12 ms/op at 32×32 depth
while the data devices idle at qd ≈ 1.5 (evidence: the diagnosis note,
measured on dev `e4f756a9`, production fabric rig, engagement exact). Remove
the ceiling by admitting the **fully-mapped within-EOF striped overwrite
class** under the inode **read** (shared) guard, serialized only where bytes
conflict (the block plane) — the `xfs_file_dio_write` /
`IOMAP_DIO_OVERWRITE_ONLY` shape (shared `i_rwsem` for overwrite-only DIO;
`-EAGAIN` → one exclusive retry on anything that would allocate).

**Non-goals (charter rails):**

* **No benchmark fork.** One classifier every write passes through; the
  decision ledger (§7) makes rot visible. The admitted shape is the design
  target's dominant write (D1 mixed AI-training workloads; the scoreboard's
  rand-4k row; checkpoint/DB in-place update traffic; mmap writeback — audit
  row 16).
* **No DLM/lease changes.** The custody layer already runs
  hold-until-conflict (32 lease acquisitions / 23.5M writes on the diagnosis
  row). Not the bottleneck; not touched.
* **No block-lock changes.** `BLOCK_FLUSH_LOCKS` + the W1 §5.1 fence are the
  safety basis and stay verbatim.
* **v1 is overwrite-only.** Hole-fills / overlay-B2 stores / any map insert
  are a *different, larger* campaign (KD-3). Nothing here relaxes them.

## 2. Key decisions

| # | Decision | Rationale |
|---|---|---|
| **KD-1** | **Hold duration: drop-before-data-I/O** (MetaPrepOnly's law, shared mode). The Shared guard covers classification-revalidation, lease, and the size/type/map snapshot — never the DMA window. | Hold-across-I/O would block truncate/fallocate for full device windows (a new convoy) and make any future nested inode-lock acquisition a self-deadlock. Truncate-vs-in-flight-I/O is today's MetaPrepOnly world, already handled by `drop_active_block_overlays_beyond` + `truncate_layout` + block locks (review R-B2 + R2 correction). |
| **KD-2** | **Acquire protocol: classify-from-cache → acquire-as-classified → revalidate-under-guard → at most ONE upgrade** (§4.2). Pre-lock probe is RAM-cache-only; cache miss ⇒ exclusive; no fetch/refill/await while classifying or holding Shared. | Today's order (exclusive first at `fuse_client.rs:18243`, classify at `:18350`) cannot express Shared. The XFS precedent retries exclusive on `-EAGAIN`; ours upgrades once, never loops. Tokio `RwLock` is write-preferring/FIFO, so a waiting truncate excludes newly-arriving Shared writers — no starvation, no upgrade livelock (R-B1). |
| **KD-3** | **v1 predicate is mapped-only**: the written range must be fully mapped in ONE immutable cached `block_map` snapshot. Resident rehydrated indirect maps are eligible (the refill decodes into `CachedMetadata.block_map`; the 2 GiB diagnosis file ≈ 512 mappings stays in class). Miss, `block_map_id`-without-map anomaly, or any hole ⇒ exclusive. **Superseded 2026-09-05 (W-2, §4.4 row 15's delivered proof):** the v1 clauses survive only as the `SQUEEZEFS_WRITE_GUARD_NARROW=0` A/B leg; the default class is every cache-resident striped write. | A within-EOF hole-fill allocates and `merge_block_mappings` under `INODE_META_LOCKS` — an inode-plane mutation; overlay B2 is *defined* as the fresh/hole store. XFS overwrite-only refuses holes for the same reason (R-B3, R2-M1). *(W-2's correction: those mutations run after the KD-1 drop on both modes, so they were never under the inode guard — XFS's reason does not transfer, because XFS's i_size/extent-tree mutations are under `i_rwsem` and ours are under (3)+(3.5).)* |
| **KD-4** | **Predicate 6 (stream adjacency) is adjudicated by a gate row, not redesigned.** PR 3 adds a seq-4k streaming-overwrite gate row; if write-through economy regresses beyond band, the contingency is a small per-ino recent-ends window for the adjacency detector — a follow-up, not a blocker. | Shared admission makes qd>1 sequential writers miss `offset == prev_end` and take W1 — correctness-safe, economy-visible (R-M3). Measure, don't speculate. |
| **KD-5** | **Attr publication becomes one atomic merge domain**: `attr_publish_locks` (4096-way striped sync mutex, the `INODE_META_LOCKS` sizing pattern) wraps get→merge→insert for **every** attr publisher (WRITE postlude, `refresh_attr_cache`, setattr, expiry-reinsert). Merge law: times = signed max per field (the `park_times_refinement` pattern, `kv/backend.rs:3053`); size = explicit-set (setattr/truncate) else max. Policy/listener work runs OUTSIDE the entry guard (the 2026-08-10 `read_mostly_cache` deadlock law). Works identically over both cache backings. | Today's `:18485` get→overwrite-mtime→insert loses updates under concurrent Shared writers; `ReadMostlyCache` exposes only replacement `insert`, and a WRITE-only fix still races refresh/setattr (R-M4, R2-B3). Lands in PR 2 — behavior-preserving under exclusive, load-bearing before PR 3. |
| **KD-6** | **The admission record survives the drop** (R2-B2): dispatch carries `{scope_final, size_floor, map_snapshot: Arc, fencing_token}`. A `FencingTokenExpired` retry re-runs the FULL protocol (classify → acquire → revalidate), never reuses the entry-time snapshot. The Shared postlude publishes **times only** — a size-neutral completion must not resurrect a stale-high size over a newer truncate. Exclusive-class postludes unchanged. | Truncate/punch can land between the Shared drop and a retry/postlude; the entry-time `old_size` republish is the dangerous direction. |
| **KD-7** | **The lifecycle boundary stays exclusive-only** (R2-B1): the verified-NotFound orphan probe, and every cold/no-open write, route exclusive *by construction* — Shared requires cached meta+attr (KD-2's miss⇒exclusive), and the orphan probe fires exactly when no handle/meta/attr is live. Reclaim purging caches under its own protocol (`open_inodes` + `reclaim_inflight` + `INODE_META_LOCKS`) forces the revalidation miss → upgrade → the exclusive path runs the probe before any fetch, exactly as today. Reclaim-wins reply stays the counted orphan-discard ack. | Shared must never fetch or fabricate metadata ahead of the lifecycle probe; making the class unreachable without cached state is the proof, and revalidation is the fence. |
| **KD-8** | **Preview and final scope are separate ledgers** (R2-B4): PR 1 ships `write_lock_candidate_{shared,metaprep,entire}` while still taking today's exclusive guard; PR 3 ships final `write_lock_scope_{shared,metaprep,entire}` + `write_lock_scope_shared_upgrades`, with closure against WRITE ops. `write_lock_wait` extends to per-mode histograms (aggregate kept) so Shared's wait cannot vanish from the convicting instrument by moving to an unmetered `read().await`. | The engagement gate must meter the mode actually HELD after revalidation, and the ceiling instrument must survive the fix. |

## 3. Evidence

`.benchmarks/2026-08-10-write-inode-convoy-diagnosis.md` — the four
engagement-verified rows, phase decomposition, and the Little's-law model.
Summary: kern 391k / il 205k IOPS at 32×32; `write_lock_wait` ≈ 1,120 µs
dominant; devices qd ≈ 1–1.5; lease layer n=32; sessions lever no-op; W1
engagement 99.96 %; write amp ≈ 1.03. Note: 38/39 GB/s are the seq-1m
bandwidth class and are quoted only as the non-regression gate, never as a
4k ceiling claim. Held time "~µs" is cache-hit meta-prep; the 82 µs
per-inode interval is wake + prep — which is why device qd stays ≈ 1.5.

## 4. Design

### 4.1 The classifier

`inode_write_lock_scope` is rebuilt so it can actually decide (R-M2): it
takes `(file_type, size_floor, offset, len, map_probe)` and absorbs the
staged bypass at `:18350` (`staged && expected_new_size <= block_size ⇒
MetaPrepOnly`) — one classifier, no side channel. It returns a **candidate**;
the **final** scope exists only after §4.2's revalidation (KD-8 naming).

```rust
pub enum InodeWriteLockScope {
    /// Inline / staged / layout-transition / RMW: write guard, whole op.
    EntireOp,
    /// Striped shapes that publish size or map: write guard, dropped
    /// before data I/O.
    MetaPrepOnly,
    /// Fully-mapped within-EOF striped overwrite: READ guard, dropped
    /// before data I/O (KD-1) — the same drop point as MetaPrepOnly.
    Shared,
}
```

**Range validation precedes classification** (R2-M2): zero length returns
success with no lease/dirty/time side effects; `offset.checked_add(len)`
overflow or `end > max_file_size()` is EFBIG before any classification;
touched blocks derive from `[offset, end)` (no `len - 1` underflow).
Contract tests pin zero, `u64::MAX`, the last representable byte, and an
end exactly on a block boundary.

**`Shared` candidate predicate** (each clause cache-only; any failure falls
through; never an error, never a second admission):

1. `file_type == "striped"` from the cached metadata snapshot.
2. **Within EOF by the conservative floor**: `end <= shared_size_floor(ino)`
   — a **named new helper**, distinct from `freshest_size` (which takes max
   for the never-lose-acked-bytes direction): it returns the *minimum* of
   the live attr-cache and metadata-cache sizes and **misses ⇒ exclusive**
   (no fetch under or before Shared). Stale-low only routes exclusive;
   truncate publishes the new size into **both** caches before dropping its
   exclusive guard (pinned: `truncate_layout` → `merge_block_mappings`
   updates `metadata_cache`; `setattr` updates `attr_cache` at `:18878` —
   both under the same exclusive hold), so stale-high is unrepresentable
   after the revalidation fence.
3. **Fully mapped** (KD-3): one immutable `Arc` map snapshot taken from the
   same `CachedMetadata` read as (1)–(2); probe exactly the touched block
   indexes, O(touched). Resident rehydrated indirect maps qualify; miss /
   anomaly / any hole ⇒ exclusive.

(The former clause 4 — "no in-flight EntireOp holder" — is deleted per
R-m1: that is the RwLock's own semantics, not a classifier input.)

Deliberately not in the predicate: alignment, W1 eligibility, payload size.
Those choose the data-plane *vehicle*; Shared is about the inode plane, and
with KD-3 the class provably performs no inode-plane mutation.

### 4.2 The acquire protocol (KD-2)

```
validate range (R2-M2)                      [no locks]
probe caches: meta snapshot + attr floor    [no locks, no fetch, no await]
candidate = classify(...)                   [count: write_lock_candidate_*]
acquire: Shared ⇒ read().await; else write().await
revalidate UNDER the guard: re-read floor + re-probe the SAME touched
    indexes on a fresh snapshot
  ├─ still Shared-eligible ⇒ final = Shared  [count: write_lock_scope_shared]
  └─ moved (truncate/punch/reclaim purge/…) ⇒ drop; write().await;
       re-classify ONCE among {MetaPrepOnly, EntireOp}
       [count: write_lock_scope_shared_upgrades]
proceed:
  Shared / MetaPrepOnly ⇒ lease + snapshot + admission record (KD-6);
       DROP GUARD; data I/O; postlude per KD-6
  EntireOp ⇒ today's path verbatim
```

No loop: one upgrade maximum. Tokio `RwLock` write-preference means a
waiting truncate gates new Shared arrivals — a rand-4k storm cannot starve
setattr-size. The exclusive classes keep today's exact order (guard first),
so the lifecycle probe (KD-7) sees an unchanged world.

**Must-not (P1-9 addendum):** a Shared or MetaPrepOnly holder never
re-enters `active_inode_locks` (read or write) on the same ino. For the
record (R2 correction): the queue-full and synchronous-fsync paths reach
`flush_due_active_blocks_for_inode` → `flush_one_active_block`, which takes
**no** inode guard; `flush_single_active_block` (`:22829`) is the
background/teardown **read**-guard wrapper, and the GDS ioctl takes read.
No current chain violates the must-not; the rule keeps it unrepresentable.

### 4.3 What the read guard still excludes — corrected taker list (R-M1)

Exclusive `active_inode_locks.write()` takers at `e4f756a9`: FUSE `write`
(pre-classification today), `setattr` **iff size**, `fallocate` punch/zero,
`fallocate` extend, `copy_file_range` dest. All keep exclusive and are
excluded while a Shared guard is held. **`fsync` does not take the inode
write guard today and its mode does not change** — its flush serializes
against in-flight data via block locks (making it exclusive would put fsync
behind the very convoy this campaign removes).

### 4.4 The writer-writer invariant audit

Every row lands as a red-first test in PR 2 before PR 3 flips admission.
Dispositions: (a) already covered elsewhere, (b) migrate, (c) predicate
grounds.

| # | Site | Disposition |
|---|---|---|
| 1 | Lease mint/refresh | (a) — `get_or_acquire_lease_bounded` double-checks `active_leases` then serializes misses on `lease_locks` (`:9902–9933`). Red test: no double-mint under 32 Shared first-touch writers |
| 2 | `note_last_write_end` (W1 predicate 6) | (a) correctness / **KD-4** economy — seq-4k gate row; detector contingency |
| 3 | Attr-cache publication | **(b) — KD-5**: the striped merge domain + monotone law; red test: two concurrent within-EOF writes cannot regress mtime, across both cache arms and refresh/setattr/expiry publishers |
| 4 | Coverage-union `record_write` / ActiveBlockBuf merge | (a) — block guard; pinned OOO by `write_through_coverage_tests` |
| 5 | W1 patch + §5.1 fence | (a) — loom-verified for concurrent clone/patch |
| 6 | Extent park / escalation | (a) — runs inside the per-block future under the held block guard |
| 7 | Seed/park live re-derivation (generic/551 fix) | (a) — deliberately block-guard-scoped |
| 8 | Overlay one-authority screen | (a) **for mapped overwrites only** — hole stores are out of class (KD-3) |
| 9 | Orphan-discard probe (VL8 item 9) | **(c) — KD-7**: unreachable from Shared (cache miss ⇒ exclusive); probe order unchanged on exclusive |
| 10 | `mark_handle_dirty` / open generation | (a) — atomic |
| 11 | Rewrite-epoch / CQE supersession | (a) — block-lock-serialized, globally unique epochs |
| 12 | `park_write_times` / `park_times_refinement` | (a) — already signed-max monotone |
| 13 | Attr-cache RMW `:18485` | (b) — folded into row 3 / KD-5 |
| 14 | `placed_sever_for` per-(ino, block) assembly | (a) after audit — IL Shared writers to one block (claims Dekker + adoption seal) |
| 15 | Hole-fill / overlay install / `merge_block_mappings` | (c) — out of class in v1 (KD-3); the widening campaign owes the (a) proof. **Delivered 2026-09-05 (W-2 `perf/write-stream-guard`, `.benchmarks/2026-09-05-w2-write-stream-guard.md`): (a)** — all three run in the per-block future AFTER the KD-1 drop point on BOTH modes, under `BLOCK_FLUSH_LOCKS` (3) + `INODE_META_LOCKS` (3.5); the exclusive `MetaPrepOnly` class already ran them concurrently across siblings (generic/551's live re-derivation exists because of exactly that), so the exclusive meta-prep protected nothing the read guard does not. The Shared class is now every cache-resident striped write (`SQUEEZEFS_WRITE_GUARD_NARROW`, default on; `=0` = this table's v1 class). Pinned: `tests/write_stream_guard_tests.rs` |
| 16 | mmap writeback WRITEs after truncate+regrow | **in the Shared class** — gate: `mmap_writeback_staleness_tests` + generic/074 (closes former OQ 3) |
| 17 | `copy_file_range` dest | stays exclusive (write-mode today) |
| 18 | Nested inode-lock from the data path | must-not (§4.2) |
| 19 | `apply_killpriv` | (a) — runs before the inode lock (`:18212`); latch protocol exists |
| 20 | Write-pipeline in-flight depth | not correctness — Shared raises offered depth; the governor absorbs (gate note: watch `write_pipeline_admission_waits`) |
| 21 | Final FORGET / unlink / rename-over / open-count / `reclaim_inflight` / `delete_file` | (a) via KD-7 — these ride the open/reclaim handshake + `INODE_META_LOCKS`, not the inode write guard; pins: `fuse_watchdog_teardown_tests` counted discard + `write_visibility_tests` open/reclaim handshake |
| 22 | Fencing retry + postlude publication | **(b) — KD-6**: admission record; re-run protocol on retry; times-only Shared postlude; deterministic truncate/punch-race tests pinning both legal outcomes |

### 4.5 IL path

Handler-only classification: the sink already calls `Filesystem::write`
after the §5.5.2 sever (`ipc_service.rs:944`) — no second classifier at the
sink. The IL read fast path's `try_read()` (`ipc_service.rs:675`)
legitimately succeeds during a held Shared write (compatible readers) —
expected, not a bug to "fix" (R-m4). If il still trails kernel after PR 3,
that residual is PR 4 (instruments: `ipc_ingress_ns`, `ipc_async_handoffs`,
`placed_*`, per-mode `write_lock_wait` — which should by then be quiet).

## 5. Lock-order statement (P1-9 delta)

Order unchanged: (1) → (2) → (3) → (3.5) → (4). Deltas: the **mode** at
layer (1) for the Shared class, plus two addenda — the §4.2 must-not
(no same-ino re-entry by any layer-(1) holder) and KD-5's
`attr_publish_locks` (a leaf domain: acquired last, holds no other lock,
never held across an await — no new wait-for edges).

## 6. What could go wrong (review seeds, rev 2)

* **Interleaved same-inode writes become observable.** True today across
  writers under MetaPrepOnly; POSIX does not promise write/write atomicity.
  The battery adjudicates (gate 5; generic/074 + 075 named).
* **Truncate vs post-drop in-flight I/O.** Today's MetaPrepOnly world
  (KD-1); handled by `drop_active_block_overlays_beyond` + `truncate_layout`
  + block locks; the *new* protections are KD-6's times-only postlude and
  retry-revalidation.
* **Stale-high size floor.** Unrepresentable after the revalidation fence
  given truncate's both-caches-before-drop publication (pinned in §4.1.2).
* **Hidden writer-writer invariant.** The audit method: every §4.4 row is a
  red test before any admission flips; `invariant_tripwires` is the runtime
  backstop.
* **Convoy relocates to block locks.** Only same-block collisions queue
  there (rand-4k across ~512 blocks/file ⇒ rare); gate 3 measures the
  per-mode wait histograms rather than arguing.

## 7. Observability (KD-8)

* PR 1: `write_lock_candidate_{shared,metaprep,entire}` (preview; exclusive
  still taken).
* PR 3: final `write_lock_scope_{shared,metaprep,entire}`,
  `write_lock_scope_shared_upgrades` (zero on fully-mapped overwrite rows =
  floor healthy; nonzero around truncate storms = the upgrade working), and
  per-mode `write_lock_wait_{shared,exclusive}` histograms (aggregate kept).
  Closure: final-scope counts ≡ WRITE ops per row.
* `SQUEEZEFS_WRITE_SHARED` registered in `env_knobs.rs` (ENG-10) in PR 1;
  default OFF; PR 3 flips default ON and the knob becomes the measurement
  A/B (`=0` disables, the `SQUEEZEFS_NT_COPY` pattern) — never an
  operational escape, never `SQUEEZEFS_TEST_*`.

## 8. Gates

1. **Red-first**: every §4.4 row + the classifier contract tests (three
   classes, any-doubt-exclusive default, R2-M2 range table) before their
   migrations.
2. **Loom** only if a new fence/atomic protocol appears (KD-5 is a plain
   striped mutex; none expected).
3. **PR 3 gate (counted A-B-B-A on squeeze-test, sustained-60 s,
   engagement exact, vs the R-M7 diagnosis note)**:
   a. kernel rand-4k: `write_lock_wait` ms-population gone;
   b. data-device qd rises materially from ≈ 1.5 (the convoy was hiding the
      device);
   c. IOPS ≥ **2×** baseline floor (3× / >1.2M is the stretch iff
      device-service holds);
   d. seq-1m 38/39 GB/s non-regression;
   e. **seq-4k streaming-overwrite row** (KD-4's adjudicator);
   f. preview/final ledger closure on every row.
4. **PR 4 gate**: write_matrix parity (il ≥ kernel at minimum) on the
   rand-4k rows.
5. **The full battery regime** from zero (`task check`, pjdfstests, LTP,
   fstests `-g auto` fail-fast) — a lock-**mode** change (not order), but it
   re-earns the certificate; generic/074 and generic/075 are the named
   truncate/overlay-race sentinels.
6. Bench baseline (`tests/run_bench_baseline.sh`) pre-merge.

## 9. PR plan

| PR | Content | Gate |
|---|---|---|
| 1 | Range validation (R2-M2) + rebuilt classifier signature (staged bypass folded in) + `shared_size_floor` helper + map-probe + **candidate** ledger + knob registry entry. Exclusive guard still taken for every class (R-m5) | full cargo gate |
| 2 | §4.4 audit rows as red-first tests + migrations — including KD-5's attr merge domain (load-bearing pre-PR-3) and KD-6's admission record + retry protocol | full cargo gate (+ loom if any protocol appears) |
| 3 | The §4.2 acquire protocol live: Shared takes the read guard; final-scope + per-mode-wait ledgers; default ON | gates 8.3, 8.5, 8.6 |
| 4 | il parity residual (only if the re-measure still fails) | gate 8.4 |

## 10. Resolved review decisions (former open questions)

1. **fsync**: mode untouched — it is not exclusive today (R-M1) and must not
   become so.
2. **IL classification**: handler-only (already structurally true).
3. **mmap writeback**: not an open question — audit row 16, in the Shared
   class, gated by `mmap_writeback_staleness_tests` + generic/074.
4. Hold duration → **KD-1**. v1 breadth → **KD-3**. Predicate 6 → **KD-4**.
   Write-vs-reclaim → **KD-7**. Retry/postlude → **KD-6**. Attr merge →
   **KD-5**. Ledgers/timing → **KD-8**. Range semantics → §4.1 (R2-M2).
