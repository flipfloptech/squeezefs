# 2026-09-11 — KV leaf merge: the underfull-sibling SMO (§4.6a; landed `fac25656`)

**What changed: the v3 CoW KV tree can now SHRINK.** Since format v3
landed (`docs/design-cow-kv-metadata.md` §4.6), "sibling merge of underfull
nodes is deliberately out of v1" — deletes returned record space inside a
leaf, never the leaf's extent, so a metadata volume filled once could never
give its extents back no matter how much was deleted. The 2026-09-11
metadata-full fix (`.benchmarks/2026-09-11-post-123-board-items-2-4.md` §1)
made a full heap answer ENOSPC instead of fail-stopping and stated this as
its limitation: after a fill, creates resumed into the room the deletes
freed and hit ENOSPC again the moment a new leaf was needed. The owner
picked it as the next item ("tackle 3, that seems important").

Design amendment FIRST (`docs/design-cow-kv-metadata.md` §4.6a, the §4.6
and §4.7 "never shrinks" sentences struck with pointers, a §10 row), then
nine red-first contracts, then the SMO. Nine commits on `dev` `a5b2d5fc` →
`fac25656`; batch gate on `fac25656` = the landing gate. Dev-box evidence
only (scoping) — no field row is owed: the change is a correctness /
space-recovery mechanism, not a lever.

---

## 1. The motivating contract (red → green)

`tests/kv_leaf_merge_tests.rs` (1): a small volume (64 KiB nodes) filled to
ENOSPC — 1,045 files — then 940 deleted SPREAD across leaves (no leaf
empties on its own), then checkpoint cycles, then creates.

| | before (`a5b2d5fc`) | after (`fac25656`) |
|---|---|---|
| creates that landed after the deletes | **4** | **938** of 940 |
| `meta_kv_node_merges` | 0 | 313 |
| free extents at full → peak after the recovery cycles | 11 → 11 | 11 → **325** |
| `meta_kv_merge_sweeps` | — | 14 |

Red text: `after deleting 940 of 1045 files … creates resumed for only 4
files … free extents at full = 11, peak after the recovery cycles = 11`.

## 2. The design as built (§4.6a)

- **Candidate law, derived from `fold_capacity` C alone — no new constant.**
  Two ADJACENT leaves under one parent merge iff `f(L) + f(R) ≤ ¾C`
  (`merge_pair_capacity` ≡ `split_part_capacity`: a successor is exactly as
  full as a fresh split part — the split's fill target read backwards) **and**
  `min(f) ≤ ¼C` (`merge_candidate_capacity` = ¾C − ½C: the fill target less
  `split_node`'s byte-midpoint balance point). Hysteresis is structural: a
  fresh split half (> ½C) never qualifies as a candidate and never completes
  a pair, so a split is never undone by the next merge. `f` is
  `fold_bytes_upper_with`, which gained a `durable_tail` argument so covered
  tombstones project as elided (the 862b8077 heap admission keeps passing 0
  — its posture unchanged). Drift-is-red tie in `derivation_sweep_tests`.
- **Protocol = §4.6's three steps over two frozen sources.** Step 1 builds
  the ONE successor spanning `[left.min, right.max]` (gap-free by K2) from
  both frozen snapshots + both on-disk logs with the K1 fold, NO locks;
  writes it and BARRIERS it, and admits the entry from the checkpoint-task
  reserve, all before any lock. Step 2 takes the parent FIRST, then both
  children in **ascending NodeId order** (the commit path's leaf order —
  never key order, a coincidence of allocation — so a multi-leaf transaction
  holding one sibling and waiting on the other cannot cycle with the SMO),
  reserves INSIDE the window, moves both deltas into the successor (the
  bounded second merge), swaps both cache mappings (both old nodes
  superseded, snapshots intact for in-flight readers), releases. Step 3
  writes the entry bytes after release.
- **Interior record shape:** `Put(right.max → succ)` + `Delete(left.max)` —
  the existing separator vocabulary, no new record kind: the Put REPLACES the
  right's separator by per-key LWW, the tombstone retires the left's, and
  `next_live` routes every key of the union to the successor. Entry order
  `[Put, Delete, alloc(succ), free(left), free(right)]`: flips first so the
  parent's floor pins at `res.start`; the LAST free is the entry's highest
  seq = the §4.7 coverage gate for BOTH retirements.
- **Root collapse** (a root with one child → the child becomes the root) is
  a root-swap SMO: `[free(old_root)]`, no pointer record, dying floor at
  `res.start`, replays through the parked old root under the coverage gate —
  the only way the tree's height DEcreases; the traversal's `level < target
  ⇒ restart` arm (whose comment said "v1 trees never shrink") is exactly
  right for it and is kept, re-documented. Interior merge is the same code
  path (level-agnostic).
- **Heap arithmetic:** a merge claims one extent and frees two — net −1 —
  admitted at the **compaction floor** (`claimable − 1 ≥ reserve/2`), never
  the flush pass's own half; the freed extents return through pending-free
  1–2 barriered cycles later, so a heap-full recovery is a WAVE, not an
  instant (`merge_backlog` keeps the sweep alive past `heap_full` clearing).
- **Three derived triggers:** the flush pass checks an under-¼ dirty leaf's
  right sibling (cheap, per visit); the heap-full space class runs a bounded
  merge sweep — **level by level, leaves up, then the collapse chain** — the
  recovery the 862b8077 posture was missing; and the D4 defrag arm
  (`JobType::DefragMeta`'s second pass, `defrag_merge_leaves`, the census
  face `mergeable_leaves`).
- **Crash windows** (as split's, enumerated in §4.6a): before the successor
  is durable / before the reserve → nothing changed; entry reserved, bytes
  unwritten → the §4.4 pt 4 hole, identical to a split (old extents routed
  AND allocated); entry written, before the checkpoint → phase 1 applies Put
  then Delete (level DESC, seq), phase 2 by key, both frees park until the
  first post-mount durable checkpoint; root-collapse hole → identical to root
  growth's; collapse entry written on the old ledger → replays through the
  parked old root. Contract (4) exercises the first three via copied images
  (parked in the build window, post-flip, post-collapse): replay-twice
  digests equal, survivors resolve, deletes gone.

**A design defect the contracts caught:** the height-3 collapse contract
first read `2 → 2` — interior tombstones minted by a leaf merge sit above
the durable tail until the next cycle, so a parent projected underfull only
on a LATER sweep that ran no leaf merge and never climbed. Fix (`fed7d053`):
the sweep walks every level, leaves up, then the collapse chain.

## 3. Contracts (9, all red first)

(1) the motivating fill/delete/resume; (2) fold equivalence against a shadow
map through a merge storm; (3) K2 gap-freeness + separator consistency after
merges and collapses; (4) replay-twice digest equality with merges in the
crash window (three shapes); (5) the SMO-vs-commit storm (writers retry on
stale resolution, `meta_kv_commit_smo_retries` moves, every acked key
survives); (6) root collapse height 3 → 1 with a mid-walk reader restarting;
(7) the heap arithmetic — a merge admitted at the compaction floor where a
split is refused, `free_extents` +1 per merge after the covering cycle; (8)
the D4 face (`mergeable_leaves`, `defrag --meta` merges them, in-process job
drive); (9) the derivation tie. Contracts (2)–(9) were compile-red (46
errors naming exactly the specified API). `meta_volume_full_tests` (d) was
RE-STATED, not weakened: it cycles until `pending_free` drains (≥ 3, bounded
64; a wedge still fails loud) instead of asserting 0 after exactly three.

## 4. Verification

fmt, clippy both configs `-D warnings`, rustdoc `-D warnings`, markdown
links, `task check:loom` compiles, `task check:fuzz` clean. Green with
`--test-threads=1`: lib `meta_backend::kv` 69; `kv_alloc` 14, `kv_backend`
36, `kv_finding_a` 3, `kv_fold_slimming` 10, `kv_freeze_wedge` 5,
`kv_journal` 21, `kv_node_cache_coherence` 21, `kv_node` 13,
`kv_partitioned_append` 27, `kv_smo_crash_completeness` 12 (×2), `kv_tree`
14, `kv_scale` 15, `kv_leaf_merge` 9 (×4); `meta_volume_full` 4,
`crash_contract` 25, `conveyor` 16, `conveyor_two_stage` 10, `posix_errno`
13, `derivation_sweep` 51, `defrag` 12 (×4), `durable_block_refs` 24,
`inline_raise` 7, `dismount_staged_residue` 3. Orchestrator re-run on the
fetched branch: 163 tests / 8 suites green.

Gauges: `meta_kv_node_merges`, `meta_kv_root_collapses`,
`meta_kv_merge_candidates` (per volume, live), `meta_kv_merge_sweeps` (per
volume), `frag_d4_mergeable_leaves`, `defrag_meta_merges`. No knob, no
incompat bit (the on-disk vocabulary is the existing separator records).

## 5. Stated, not done

- Merges never cross a parent boundary (adjacent leaves under different
  parents shrink through interior merges instead) — by design, §4.6a (d).
- `sweep.candidates` counts underfull leaves seen before the first floor
  refusal, so it under-reads during a wave; the D4 census is the complete
  count.
- The D4 sweep's cost is O(resident nodes) fold walks per pass, unmeasured
  on a field-sized node cache; the heap-full sweep pays it only while the
  posture stands.
- No field row: the change is a space-recovery mechanism; its engagement on
  a real volume is the `meta_kv_node_merges` / `meta_kv_merge_sweeps` pair
  under a delete-heavy workload.
