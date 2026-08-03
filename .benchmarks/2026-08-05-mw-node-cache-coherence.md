# 2026-08-05 — The KV node cache under a second process: a lagging reader, and partitioned appenders

Branch `feat/mw-node-cache-coherence`, off dev `a703ce3f` (221 commits past
`402ca77`). Closes the **runtime** item pre-RC engineering spec §6.2 calls
*"arguably harder than any of the ten"* durable-format single-writer
assumptions: *"the KV node cache is load-once RAM-authoritative … with no
revalidation path, because in a single-writer design there cannot be another
appender. Two writers on one volume produce divergent trees and mutually
destructive checkpoints. The tractable answer is to ensure two writers never
cache the same node — **partitioning, not cache coherence.**"*

Scope discipline, stated first: **no cross-node coherence protocol was
built**, because the spec's verdict is partitioning and a coherence protocol
would be the wrong answer. What landed is (1) the §6.8 item-2 reader
revalidation path — the piece "one writer + N readers" needs and the S5
prerequisite — and (2) the partitioning argument for writers, *enforced* at
the one choke point every node mutation passes, with a runtime detector for
the case the partitioned-append formats could only catch at replay.

| Commit | What |
|---|---|
| `02f1aa03` | `bench(meta)` — price the node-cache hit path FIRST (the baseline this work must not move) |
| `28aa33c2` | `test(meta)` — 31 contracts (20 integration + 11 in-module), all red |
| `a1f6a72a` | `feat(meta)` — `epoch_core.rs`, the cache machinery, `revalidate.rs`, the partition gates |
| `1c52244f` | `test(loom)` — 4 models against the shipped core, with weakening evidence |
| `6837041f` | `bench(meta)` — the epoch gate, the inert poll, the drop pass |
| (this) | `docs` — the consistency model (operations.md), design §4.9a, this note |

---

## 1. The API the RO-mount agent builds against

Stated first because a sibling consumes it. Two levels; both are additive and
inert until armed.

**Backend level** (`impl KvMetaBackend`, `kv/revalidate.rs`):

```rust
// Once, at open, AFTER the mount decided this is a read-only mount.
be.arm_reader_revalidation(Some(sink))?;     // sink: Option<Arc<dyn EpochPurgeSink>>

// On the derived cadence (the poller owns "is it due", clock injected).
let poller = RevalidationPoller::derived();          // or ::new(interval_ms)
if let Some(out) = poller.poll_at(&be, Instant::now()).await? { … }

// Or drive it directly:
let out: RevalidateOutcome = be.revalidate_reader().await?;   // poll + adopt + drop
let epoch: u64 = be.reader_epoch();                  // 0 = not a reader
let bound: Duration = poller.staleness_bound();      // interval + 1 s
```

**Cache/tree level** — for wiring that owns a `NodeCache` and its `KvTree`s
rather than a backend:

```rust
let epoch = RootEpoch::from_ledger(&record);   // or ::synthetic(seq, tail, roots)
cache.arm_revalidation(&epoch, sink)?;         // seeds epoch+tail, drops nothing
let out = revalidate_trees(&cache, &trees, &epoch);   // roots THEN the drop pass
cache.revalidation_epoch();  cache.is_revalidating();
tree.adopt_root(RootPtr { addr, seq })?;       // refused on a non-reader
```

`RevalidateOutcome { advanced, from_epoch, epoch, tail, dropped,
bytes_credited, retained, skipped_dirty, keys_purged }` is the per-poll
ledger — `advanced == false` means the poll was inert (nothing dropped,
nothing purged, not even a cached `Arc` replaced).

Four contracts the consumer should rely on:

1. **Arming is a declaration with teeth.** After it, **every** node mutation
   on that cache is refused loud (`meta_kv_node_partition_refusals`), because
   a cache that is a projection of another process's tree cannot also be
   authoritative. `revalidate_reader()` refuses on a mount that never armed,
   and `KvTree::adopt_root` refuses on a non-reader — adopting a ledger's
   roots on a write mount whose SMOs moved past them would be time travel.
2. **Arming never drops anything**, including the roots `KvTree::open` had to
   load; they are re-stamped into the armed epoch instead.
3. **Order inside a poll is fixed and load-bearing**: roots are adopted
   before the epoch step, and the epoch is published (Release) before the
   sweep walks. A thread seeing the new epoch sees the new roots; a thread
   seeing the old epoch gets a stale-stamped node, i.e. a miss.
4. **Nothing is required of the caller on the hit path.** The staleness gate
   lives inside `try_get`/`load`.

**The R-6 trigger** is `TieredEpochPurge::new(tiers)` + `note_suspect(key)`
from wherever the reader's data path learns a block-key binding; on each
epoch step the sink drains its suspect set through
`TieredCache::purge_block_key` and nothing else (the R-6 law). If the data
plane never registers, `meta_kv_revalidate_keys_purged` stays 0 while
`meta_kv_revalidate_epochs` grows — the honest, visible statement that half
the wiring is missing rather than a silent hole.

---

## 2. The stated consistency model

Operator wording: `docs/operations.md` § *Read-only coherent mounts*.
Normative wording: `docs/design-cow-kv-metadata.md` §4.9a. In one line: **a
coherent reader serves the metadata state of the most recent checkpoint it
has polled.**

| Property | Statement | Why it holds |
|---|---|---|
| Bounded staleness | ≤ **poll interval + 1 s** (≈ 2 s default) | a write mount publishes a ledger record at the end of every cycle that had work, ≤ 1 s apart under load (§4.6 pt 2); worst case a record lands just after a poll |
| Idle-inert | an idle writer costs a reader **nothing but the ledger read** | `checkpoint.rs::tick` runs a cycle only when `final_cycle ‖ dirty_nodes > 0 ‖ distance > 0`, so no record is minted and `publish` finds nothing newer |
| Monotone | never backwards, never loses a served record | the epoch advances by CAS only |
| Per-operation atomic | an operation starting after a poll sees exactly one epoch's nodes | epoch published before the sweep + the lazy hit-path gate |
| Multi-key caveat | a `readdir` spanning a poll may mix two adjacent checkpoints | stated, with `reader_epoch()` as the seqlock-style handle for callers that care |
| Not durability | committed-but-uncheckpointed work is invisible | deliberate: no journal replay on the read side is what makes it cheap |
| Metadata only | data-block bindings are **not** coherent | §6.3's block-key hazard needs the §6.8 item-3 freed-offset grace period, which is not built; the epoch step fires the purge trigger for registered suspects only |

The number is machine-readable (`RevalidationPoller::staleness_bound()`) and
pinned by a test, so the doc cannot drift from the code.

**Why the cadence is derived, not a constant** (`resolve_revalidate_interval_ms`,
tie-tested): `max(effective writer cadence, CHECKPOINT_MAX_AGE_MS)`, where the
effective writer cadence is the flush knob with strict mode reading as the
checkpoint task's own 100 ms tick — the identical derivation
`spawn_checkpoint_task` performs, so the two cannot drift. Polling faster than
the writer mints records cannot reduce staleness (the records do not exist yet)
and pays a drop pass for nothing; a slower operator-chosen flush cadence moves
the derivation with it. `SQUEEZEFS_META_REVALIDATE_MS` wins verbatim
(registry entry added; the startup gate owns the loud refusal).

---

## 3. The measured cost

Venue: house dev box, `taskset -c 8-15`, `nice 19`, **sibling agents
compiling in other worktrees throughout** (Tdie pinned ~100 °C, load 5–11).
That is the noise floor this box could offer, so the estimators are per-side
minima plus the same-side spread, and both are reported.

**The hit-path gate is free.** Cleanest instrument first — *within one
binary*, so the only difference is whether the epoch word is nonzero
(un-armed `0 == 0` vs armed `N == N`); 4 runs × 8 s:

| row | medians (ns) | mean | min |
|---|---|---|---|
| `try_get_hit` (un-armed) | 36.37 / 36.26 / 34.66 / 37.49 | 36.20 | 34.66 |
| `try_get_hit_armed` | 35.36 / 33.80 / 39.77 / 34.72 | **35.91** | 33.80 |

−0.8 %, with the armed **minimum below** the un-armed one: two relaxed loads
and a compare are not measurable against the scc map read and `Arc` clone the
hit already pays. The cross-binary A-B-B-A-A-B bracket on the same row (A =
saved pre-change binary, B = this branch, 10 s windows) agrees:

| side | runs (ns) | mean | same-side spread |
|---|---|---|---|
| A | 39.38 / 34.95 / 37.15 | 37.16 | 12.7 % |
| B | 37.07 / 38.10 / 35.93 | 37.03 | 6.0 % |

**−0.3 %, inside the noise.** (The first, quieter baseline run recorded
31.17 / 31.80 / 20.22 ns for hit / rotating16 / absent — the box got hotter
and busier as siblings built, which is exactly why the verdict rests on the
within-binary pair and the same-side spreads.)

**The cadence and the drop pass:**

| row | min | reading |
|---|---|---|
| `revalidate_inert_poll` | **6.32 ns** | what a poll costs in RAM when the writer minted nothing; the real per-poll cost is the 128 KiB ledger read |
| `revalidate_drop_pass/64` (64 KiB nodes) | **36.81 µs** | ≈ 575 ns/node, dominated by freeing each node's extent buffer — the same deallocation eviction pays |

Extrapolated to a 512 MiB reader budget of 256 KiB nodes (2,048 mappings): a
full drop pass is ≈ 1.2 ms of CPU per epoch step ⇒ **≈ 0.1 % of one core** at
the derived 1 s cadence. The reload is demand-paged and therefore proportional
to what the reader actually touches, not to what it dropped.

Methodology note worth keeping: the drop-pass row uses `iter_custom`, not
`iter_batched`. Criterion runs a batch's setups **before** its timed
routines, so with batching only the first pass of each batch met a populated
map and the row reported the cost of sweeping an *empty* cache (~1 ns/node —
the tell that caught it).

---

## 4. The partitioning argument, and the answer to the partitioned-append agent

`.benchmarks/2026-08-05-mw-partitioned-append.md` §6 item 4 stated what its
formats assume about this item:

> *(a) the authority is the only appender mutating structure (so interior
> nodes have one cacher); (b) peer content folds in at replay, not
> concurrently; and (c) nothing here revalidates a leaf a peer wrote — that
> case is a `PartitionViolation::Key`, detected at replay, not prevented at
> runtime.*

**(a) holds, and is now enforced rather than argued.** Two independent
structural facts already made it true: lock order **4b** gives interior-node
mutation exclusively to the serialized per-volume checkpoint/SMO task (a
commit-path writer takes leaf locks only, and that split is what keeps the two
lock populations acyclic), and `read_partitioned_ledger` **refuses loud** a
non-authority record that names tree roots, so a peer cannot publish
structural state even if it computed some. What was missing was a RAM-side
gate: `CachedNode::apply_locked` — the ONE place every record application to
every node passes — now refuses a non-authority mutation of a `level > 0`
node loud and counts it (`meta_kv_node_partition_refusals`, must stay 0).
Cost: `level > 0` short-circuits the leaf commit path before the gate word is
even read, and on a solo volume that word is 0.

**(b) holds unchanged** — nothing here folds peer records concurrently, and
the reader path deliberately does *not* replay the journal (that is what
bounds its cost and its model).

**(c) was right, and is now half-wrong in the useful direction.** A runtime
detector exists for the case that actually destroys data: `append_frozen`
probes its destination page on a partitioned volume and refuses loud if it
already holds a checksum-verified frame of this incarnation. Without it, our
append writes at the tail offset we remember and validates only the node
incarnation — a peer's records at that offset are **silently overwritten**,
acked and lost with no counter. That is now `foreign append detected …`
(counted, non-solo only, so solo pays no extra read).

**And the hole the item exists to prevent, stated plainly.** A leaf a peer
wrote in an **earlier** window, which the authority cached before that window
closed, is still undetected: the append probe fires only if the authority
appends to that node again, and replay's `PartitionViolation::Key` fires only
while both writers' records are inside the replay window. Afterwards there is
no evidence anywhere. Two things close it, neither of them this branch's:

1. **Arm the reader path on every non-authority appender.** With respect to
   *structure* a peer **is** a reader — it routes through interior nodes it
   does not own — so it must drop its cached interior nodes (and, since
   coverage is nearly empty, its leaves) on each authority epoch step. That
   machinery now exists and is exactly what §6.8 item 2 built; what does not
   exist is the S4/S8 wiring that hands a peer an appender identity and a
   cadence. Note the corollary the peer must respect: a non-authority
   appender cannot arm the *current* reader mode as-is, because arming
   refuses all mutation — a peer needs "reader for structure, appender for its
   own leaves", which is a two-line generalization of the gate word (a third
   state), not a new mechanism.
2. **The slot map's leaf partition** (§6.2 items 4/8): a peer must only ever
   write keys assigned to it. The node cache deliberately does not arbitrate
   leaves, and a gate that pretended to would be mistaken for cross-writer
   custody.

So: the partitioned-append formats' assumption is **confirmed for (a) and
(b)**, and **(c) is upgraded** — the runtime prevention it said did not exist
now exists for the overwrite case, with the earlier-window case named,
pinned, and left to S4/S8 with the mechanism it needs already in the tree.

---

## 5. Gauge integrity

The charge accounting is the piece whose failure mode is silent: a lost credit
reads as "fuller than it is" and evicts forever; an over-credit **wraps a u64
through zero** and reads as "full forever". The drop pass is a *mass* credit,
the shape most likely to lose or duplicate one, so:

* every released mapping credits exactly one extent through the same
  `remove_if_sync(ptr_eq)`-gated `fetch_sub` that eviction and retire use —
  scc arbitrates, so **racing sweepers cannot double-credit** (pinned by
  `racing_revalidations_credit_each_mapping_exactly_once`, and by the loom
  model proving exactly one poller ever observes a step);
* `bytes_credited == dropped × node_size` is asserted per pass;
* memo bytes leave with the dropped snapshot — the §5.7 Drop-owned law,
  re-asserted across the new path against the process-global gauge
  (`a_dropped_snapshot_memo_leaves_the_global_gauge_at_its_baseline`);
* **dirty nodes are never dropped**: a node with an un-durable floor (or a
  write lock held) is kept and counted on the must-stay-0
  `meta_kv_revalidate_dirty_skips` tripwire. Dropping one would lose RAM
  records no disk image holds;
* the clock ring is drained of dropped addresses and re-seeded with the
  survivors: without that, a reader's drop-and-reload cycle would push one
  clock entry per reload forever (pops only happen while over budget).

TEST-9's five original charge-conservation contracts are unchanged and green.

---

## 6. Verification

`--test-threads=1`, on this branch:

| Gate | Result |
|---|---|
| `tests/kv_node_cache_coherence_tests.rs` (20 cases) | **green** |
| `node_cache.rs` in-module (11: TEST-9's 5 + 6 new) | **green** |
| `kv_node_tests` (14), `kv_tree_tests` (12), `kv_journal_tests` (21), `kv_alloc_tests` (14) | green |
| `kv_partitioned_append_tests` (27), `kv_finding_a_tests` (10), `kv_fold_slimming_tests` (3) | green |
| `kv_smo_crash_completeness_tests` (12), `crash_contract_tests` (25) | green — **both of the pre-existing reds the partitioned-append note flagged are gone on current dev** |
| `env_knob_convention_tests` (20) | green (the new knob has its registry entry) |
| `cargo clippy --all-targets --all-features -- -D warnings` | clean |
| `cargo clippy --all-targets -- -D warnings` (shipped config) | clean |
| `cargo fmt --check` | clean |
| `cargo bench --benches -- --test` (smoke) | green |
| `tests/run_loom.sh` | **69/69** (65 pre-existing + 4 new) |

### Loom, with weakening evidence

Four models against the shipped `kv/epoch_core.rs` (`#[path]`-included,
`LOOM_MAX_PREEMPTIONS=3`). Each weakening was applied to the shipped file and
reverted:

| Weakening | Outcome |
|---|---|
| publish the epoch BEFORE the tail | FAILS — *"a node stamped epoch 2 would be classified against tail 10"* |
| loader reads the tail BEFORE the epoch | FAILS — same assertion, from the other side |
| `Relaxed` on both sides of the pair | FAILS — same assertion (the other three models still pass: the pair is the ordering-sensitive one) |
| `publish` stores instead of CAS-ing | FAILS — two sweepers for one step, i.e. the double credit that wraps the budget gauge |
| `set_appender` clobbers the reader bit | FAILS — the reader declaration is lost |

### One pre-existing dev red, NOT this branch's

`kv_backend_tests::v3_unknown_incompat_bit_refuses_naming_it_and_unknown_ro_does_not`
fails on dev `a703ce3f`: the test probes `1 << 9` as an *unknown* incompat
bit, but bit 9 is now `FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS` and is in
`FEATURES_INCOMPAT_KNOWN`, so the mount legitimately succeeds. Neither
`superblock.rs` nor `kv_backend_tests.rs` is touched by this branch (empty
diffstat vs the base). The fix belongs with the durable-block-refcounts
landing: move the probe to the lowest bit that is still free. Flagged for the
orchestrator.

---

## 7. What S8 (and S5) still owe

1. **The RO mount mode itself (S5, §6.8 items 1/4/6).** `flock(LOCK_SH)`, the
   `FreshForeign` bypass for RO, the write gate extended past metadata (block
   allocator, reclaim queue, W1, in-place overwrite,
   `recover_active_blocks_v3`), kernel TTLs cut to the checkpoint interval,
   `dir_entry_cache_v3`'s 300 s TTL likewise, writeback cache off. The
   revalidation API above is what item 2 owed them.
2. **The freed-offset grace period (§6.8 item 3)** — the highest-value item in
   the coherence analysis and the one that makes the data half of a reader's
   promise bounded. Until it lands, the model's *metadata only* row is load-
   bearing, and `meta_kv_revalidate_keys_purged` is the gauge that says how
   much of the trigger is wired.
3. **The peer-appender third gate state** (§4 item 1 above): "reader for
   structure, appender for my own leaves". Two lines in the gate word plus the
   S4 appender-identity plumbing; the epoch machinery is already there.
4. **The leaf partition** (§6.2 items 4/8): writer-scoped keys and the slot
   map's assignment. Until it exists, cross-appender leaf disjointness is
   *detected* (append probe, replay violation) rather than *prevented*.
5. **Cross-appender tail interlock** (the partitioned-append note's item 5):
   the authority's checkpoint must not advance structure past records a peer
   has not had folded. A protocol (S8), not a format, and not a cache.
6. **Stats-surface consumption.** The eight counters are rendered on the
   `.stats` inode already; what is missing is the RO mount's own row in the
   guarantee table and an alerting recipe for the two must-stay-0 tripwires
   (`meta_kv_revalidate_dirty_skips`, `meta_kv_node_partition_refusals`).
