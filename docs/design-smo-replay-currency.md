# Design decision: SMO routing currency vs the checkpoint tail — closing the FIND-VS-A acked-loss residual

**Status: Implemented** (2026-07-16; approved at writer/reviewer consensus, 2 review rounds, 11 issues
resolved). All three fix branches + the PR 5 acceptance gate landed on `dev`:

| Program row | Branch | Commits (dev) | Evidence |
|---|---|---|---|
| PR 1 scaffolding + 1(a)/(c) + PR 2 (C′ two-phase replay) | `fix/kv-smo-two-phase-replay` | `0b6555d..89ea158` | `.benchmarks/2026-07-16-smo-two-phase-replay.md` |
| PR 1(d) + PR 3 (FIND-SMO-TAIL entry-start floors) | `fix/kv-mid-entry-tail-floors` | `5a82ff7..f41a0eb` | `.benchmarks/2026-07-16-mid-entry-tail-fix.md` |
| PR 1(b) + PR 4 (Option A coverage-gated pending-free) | `fix/kv-pending-free-coverage` | `0377431..9f4bf2c` | `.benchmarks/2026-07-16-pending-free-coverage-fix.md` |
| PR 5 (acceptance gate + closure) | `docs/smo-program-closure` (this branch) | follows `9f4bf2c` | `.benchmarks/2026-07-16-smo-program-closing.md` — the ×10 joint storm soak (acked-loss == 0 AND refusals == 0 AND `dropped_torn == 0`, 5 full-mask + 5 `taskset 0-15`), suites, doc deltas |

Normative deltas folded into `docs/design-cow-kv-metadata.md`: §4.6 (two-phase replay bullet; pt 2
entry-start floor rounding), §4.7 (coverage-gated pending-free + at-cap force-cycle), §4.10 (SMO-window
closure note).

**Revision 2** (folds the round-1 review, `/tmp/grok-design-smo/review.md` — all 8 issues addressed;
the C′+A recommendation is unchanged and the reviewer concurs).

**Scope**: the FIND-VS-A part-2 residual (`.benchmarks/2026-07-16-find-vs-a-fix.md:157-183`): kill-9 at
create-storm peak loses 0.4–0.9 % of ACKED creates across a clean-replay remount (`dropped_torn = 0`),
or refuses the remount loud (`traversal retry budget exhausted … child-seq`, also reproduced on unmodified
dev). Clean unmounts are unaffected (shutdown drains to `tail == head`, `checkpoint.rs:592-624`).
**Law**: `docs/design-cow-kv-metadata.md` §4.4/§4.5/§4.6/§4.7/§4.10 — an acked tx must be reachable after
replay, period. **Baseline**: dev `44d14d6` (dying-floor clamp, SMO fold-source guards, `checkpoint_past`
already landed). Review round 1 additionally surfaced an **adjacent latent hole** (mid-entry tails —
chartered below as FIND-SMO-TAIL) that neither C′ nor A reaches; it gets its own PR and acceptance row.

---

## 1. The mechanism, dissected (what any option must close)

Replay input after kill-9 = newest **written** ledger record `L(seq N, tail T)` (page cache survives a
process kill, so no torn-slot fallback is in play) + node images **as last written** (including post-`L`
bset appends) + the journal window `[T, head]`. Completeness requires: durable images reachable from `L`'s
roots ⊕ window records, per-key-LWW-folded through the routing, ≡ the acked keyspace (§4.10). Today's
replay is **one pass in seq order** (`KvMetaBackend::open` step 5b, `backend.rs:674-723`): content records
descend to their leaf through the *routing as it stands at that entry* (`tree.rs:516-538` → `descend`,
`tree.rs:342-413`); interior pointer records ("flips", level-tagged, `untag` at `backend.rs:684`) apply to
the interior currently covering their separator (`tree.rs:556-581`). Two sub-mechanisms break it:

**(i) Stranding: routing evolves *during* replay, and earlier-seq records land on doomed lineages.**
An SMO's successor images are built + written in step 1 with **no locks held** (`tree.rs:874-1014`); user
commits racing that build window reserve *before* the SMO's in-lock reservation, so their seqs are **lower**
than the flip's. Their records reach the successors only as the "bounded second merge" leftovers —
`take_overlay()` partitioned into successor **RAM overlays** (`tree.rs:1143-1166`); the successor *images*
on disk predate them. At replay, such a record `C (T ≤ c < p_flip)` descends via the **pre-flip** route,
applies to the predecessor object's overlay (`apply_at_seq`, `tree.rs:461-507`), and then the flip
`p_flip > c` re-routes the key range to the successor — whose durable image never had `C` and whose RAM
overlay died with the process. `C` is acked, in the window, replayed "cleanly" — and the post-replay walk
routes around it. The measured SMO cadence (≈ 939/s aggregate, §2-D below) exposes a multi-ms build window
over the hottest leaves hundreds of times per second; the 0.4–0.9 % loss magnitude, the leaf-granular
contiguous `missing.A` key runs in the surviving artifacts (review.md:27-29), and the "clean unmount
unaffected" bound (drains write successor overlays back before `tail == head`) all match.

**(ii) Recycled-extent stale routes: the pending-free gate certifies the wrong thing.** An SMO frees the
old extent tagged `retire_seq` = the *next* checkpoint seq (`tree.rs:1096-1097`; bumped at ledger write,
`checkpoint.rs:752`), and the extent is released once that ledger record is durable
(`after_durable_barrier`, `backend.rs:1227-1237`; mount-side `retire_seq ≤ mounted_seq ⇒ release`,
`alloc_ext.rs:404-441`). That gate certifies **durability of the freeing checkpoint record — not coverage
of the freeing flip**. The two decouple: the checkpoint flush pass may *skip* the flip-carrying interior
(SMO-reserve exhaustion, `checkpoint.rs:658-666`) or the flip may otherwise stay behind the written tail
(`T ≤ p_flip`) while the free's tag `N+1 ≤ mounted` still passes. Replay then legitimately walks pre-flip
routing (the flip is *in the window*, not yet applied when a lower-seq content record descends) into an
extent whose current tenant is a different lineage → `child_node_seq` mismatch (`tree.rs:397-402`) →
restart loop → **the child-seq mount refusal** (`tree.rs:407-412`), verbatim in the surviving remount logs
(review.md:39-41). The same decoupling with a seq-*colliding* tenant is excluded only by the node-seq
watermark (`checkpoint.rs:146-155`, `backend.rs:633-638`) — the refusal is the *detected* face of a loss
mechanism. Stated at full strength (review.md:41-44): with the flip unflushed, the current code **violates
the letter of §4.7's reuse rule** ("any state replay can select references only never-overwritten
extents") — Option A restores a law invariant, not just an edge.

Why the landed hardening cannot reach these: the dying-floor clamp (`node_cache.rs:1496-1560`,
`checkpoint.rs:686-698`) keeps un-covered records **inside the window** — both (i) and (ii) already are
inside the window; the failure is in how the single-pass walk *routes* them. Widening the clamp fed the
same broken walk longer windows and was measured worse (evidence note:168-170).

## 1b. FIND-SMO-TAIL — new chartered finding (review round 1, HIGH): mid-entry tails

**Not the chartered residual** (its signature is `dropped_torn ≥ 1`; the residual's is `== 0`) and **not
closed by C′ or A** — it is upstream of `recovery.entries`. Chartered here so this program's acceptance
soak cannot go green over it. Mechanism: floors are raw **record** seqs (`dirty_floor.fetch_min(rec.seq)`,
`node_cache.rs:1141`) while the conveyor stamps per-record seqs `entry_start + i` across a multi-leaf tx
(`backend.rs:2800-2806`; rename/exchange span up to 6 leaves). Flush leaf `rec[0]` while leaf `rec[j>0]`
stays dirty — or is reserve-exhaustion-skipped with its floor restored (`checkpoint.rs:658-666`) — and the
§4.6 pt 2 tail computes to `entry_start + j`, **strictly inside the entry**. The replay chain walk starts
parsing *at the tail* (`cursor = Some(tail)`, `journal.rs:734-737`), trusting the module-doc claim "the
tail is always an entry boundary" (`checkpoint.rs:34-41`) — which the floor discipline does not enforce
for user entries. The mid-entry parse fails, resyncs at the next page's `first_entry_off`, and drops the
entry's own ≥-tail records (acked; their only copy was the un-flushed leaf's delta) **plus collateral
entries up to the resync point**, counted `dropped_torn ≥ 1` — with one signature edge: drops are
confirmed only once a *later* entry parses, so a mid-entry tail on the **newest** entry can read
`dropped_torn == 0`; covered because PR 1(d) and PR 5 assert no-loss AND `== 0` **jointly**, never the
counter alone. Fix direction: **floors round DOWN to entry
starts** — the conveyor apply passes each member's `entry_start` for floor purposes (it computes it
already, `backend.rs:2800-2806`), replay applies pass their entry's start; SMO floors need no change (they
already pin at `res.start`: flips are record index 0, `tree.rs:1137-1139`; root-swap dying floor is
`res.start`, `tree.rs:1209-1210`). This restores the `checkpoint.rs:34-41` claim globally. **Judged its
own PR** (PR 3 below), not folded into C′: distinct mechanism, distinct RED (multi-leaf tx + partial
flush + kill-9), distinct signature and acceptance row (`dropped_torn == 0` under the storm soak) — fusing
it with the replay-order change would blur bisection between two independently-arguable fixes.

---

## 2. Options

### Option A — freed-extent quarantine (release gated on flip coverage)

Extents freed by SMO retirement stay unreusable until the durable **tail** passes the freeing SMO's
records, i.e. until checkpoint coverage of the flip — not merely durability of a ledger record whose tail
may sit below the flip. Implementable with **zero format change**: the SMO's free record rides the *same
journal entry* as its flips, with the entry's **highest** seq (`tree.rs:1080-1097`; seqs stamped
`res.start + i`, `tree.rs:1137-1139`). So: *live gate* — release a pending free only when the durable tail
(already plumbed: `cache.set_durable_tail`/`advance_reusable_upto`, `backend.rs:1227-1237`) exceeds the
free record's own seq; *mount gate* — a replayed free (all replayed frees are in-window by construction)
parks pending until the first post-mount durable checkpoint. The on-disk `Freed` value keeps its
`retire_seq` byte-for-byte; gating keys off `rec.seq`, which every record already carries.

- **Why "free covered ⇒ flips covered" holds — the per-SMO-entry floor-pinning argument** (replacing
  rev-1's general "tail is always an entry boundary" claim, which FIND-SMO-TAIL shows is *not* enforced
  for multi-leaf user entries): the only floor contributions from an SMO's **own** entry pin at exactly
  `res.start` — the parent's flips apply with record index 0 first (`tree.rs:1137-1139` →
  `fetch_min(rec.seq)`, `node_cache.rs:1141`, min = `res.start`), a root swap's dying floor is `res.start`
  (`tree.rs:1209-1210`), and alloc/free records touch no node floors. Every *other* floor source is a
  record seq of a **different** entry — and seqs are byte-domain positions offset by record index inside
  disjoint reservations (`backend.rs:2800-2806`), so they lie outside `[res.start, res.end)`. Hence no
  tail can sit strictly between an SMO's flip and its free: `tail > free.seq ⇒ tail ≥ res.end ⇒` every
  flip of that entry is covered. (After PR 3 rounds all floors to entry starts, the general boundary claim
  becomes true again — belt on top of this argument, which stands on its own.)
- **Soundness vs (ii)**: closes it. Any route replay can reach — mounted image or window flip — never
  references a recycled extent; conversely a quarantined extent's predecessor image stays intact, so
  pre-flip walks resolve the correct lineage and `child_node_seq` always matches. The refusal class
  becomes structurally impossible inside the window, and §4.7's reuse rule is restored to the letter.
- **Soundness vs (i)**: **does not close it** — the charter's suspicion is confirmed. Quarantine keeps old
  images *readable*, and the pre-flip walk is then complete **for records below the tail** (folded in the
  predecessor image). But stranding is about **in-window** records: replay *does* route `C` to the intact
  predecessor — then the higher-seq flip abandons that lineage. Extent liveness was never the gap; routing
  evolution during replay is. Measured residual would stay > 0.
- **Crash windows**: kill-9 anywhere is safe — the gate only ever *delays* reuse; a kill before release
  re-parks the free at mount (in-window) or has it already covered (below tail). Free-record-unwritten
  (kill between swap and `commit_entry`, `tree.rs:1264-1282`) ⇒ the whole SMO entry is a §4.4 pt 4 hole ⇒
  mount never sees claim or free ⇒ predecessor stays routed *and* allocated. Sound.
- **Bitmap-generation soundness under the gate's new clock domain** (required analysis): the A/B pages are
  written *before* the ledger record naming the same `alloc_bitmap_generation` (`checkpoint.rs:670-679`,
  `:722-727`), and mount takes the newest **valid** slot per page (`alloc_ext.rs:368-401`) — so a page
  generation can run ahead of the mounted ledger. That asymmetry is safe because **sets are eager, clears
  are strictly post-durable**: claims/pending frees keep bits set; a bit clears only in
  `advance_durable → release → mark_dirty` (`alloc_ext_core.rs:358-400`, `alloc_ext.rs:547-553`), which
  runs post-barrier of the justifying ledger record (`backend.rs:1227-1237`). An ahead-generation page
  observable by any selectable (newest or fallback) ledger record is therefore only *conservatively newer*
  (extra allocated bits = bounded leak, never a routed-extent clear). A moves release strictly **later**
  (tail coverage ≥ record durability), never earlier — clears still lag at least one durable record, so
  the fallback-safety asymmetry is undisturbed.
- **Perf**: commit path untouched (frees are SMO-task-only). Backlog at the **measured** SMO rate
  (§2-D): ≈ 939 extents/s ≈ 235 MiB/s of quarantined extents against a ≤ 1 s checkpoint cadence ⇒
  steady-state ≈ 939 extents ≈ **1.4 %** of `PENDING_FREE_CAP = 65,536` (`backend.rs:74`; 16 GiB at
  256 KiB extents).
- **At-cap truth and the chosen posture**: today the law's "pressure forces a checkpoint rather than
  unsafe reuse" (§4.7) is intent, not mechanism — at cap `free_pending` returns the typed
  `KvError::PendingFreeFull` (`alloc_ext.rs:530-541`, core `alloc_ext_core.rs:305-343`) *after* the swap
  and entry write (`tree.rs:1283-1284` `?`), which aborts the maintenance tick loudly; only
  `JournalReserveExhausted` forces an inline cycle (`checkpoint.rs:500-508`, `:548-557`). The extent then
  leaks from the live FIFO and recovery rides the cadence. **PR 4 makes the code match the law**, in
  three clauses (the third-arm and terminal-placement clauses per review round 2, Issue 9):
  a pending-headroom check at SMO admission (beside the `try_admit`, `tree.rs:1099-1110`); handling
  `PendingFreeFull` like reserve exhaustion in the two `run_maintenance` arms (`checkpoint.rs:500-508`,
  `:548-557`) — force one `checkpoint_cycle(…, true)` and retry; **and the same skip-and-defer in the
  flush-pass match inside `checkpoint_cycle` itself** (`checkpoint.rs:658-666` — today only
  `JournalReserveExhausted` skips; anything else aborts the whole cycle at `:666`), identical
  restore-floor semantics, so a *forced* cycle under a cap-saturated storm (many full-log leaves — the
  precise FIFO-filling regime) still completes its non-SMO flushes, writes the ledger, barriers, and
  drains the FIFO via `after_durable_barrier` (`backend.rs:1191`, `:1227-1237`) — without this clause the
  forced cycle itself returns `PendingFreeFull` before its barrier and the remedy livelocks the
  checkpoint task while ring reclamation wedges behind it. **The bounded-retry-then-loud terminal lives
  in the arms** (the `checkpoint_past` precedent: bounded cycles then fail loud, `backend.rs:1259-1272`):
  N forced cycles without `pending_count` decreasing ⇒ fail the volume loud — the genuinely-wedged-tail
  defect presents there, not as a retry loop. The post-swap error at `tree.rs:1283-1284` is
  **unreachable under admission headroom** (the serialized SMO task is `free_pending`'s only producer, so
  headroom at admission holds at step 3); it stays as defense-in-depth, not as the terminal path. Why
  forced-cycle over documented-posture: A **widens** parking lifetimes (tail coverage, not record
  durability), so the cadence-drain posture degrades exactly when A lands; and the wedged-tail bound is
  short — a tail pinned by a skip loop fills the cap in ≈ `65,536 / 939` ≈ **70 s** of sustained storm,
  too fast to leave to "the cadence will get to it".
- **Format**: none (see above). **Size**: S (~60-100 lines: `alloc_ext_core.rs` gate clock-domain +
  headroom, call sites, mount gating `alloc_ext.rs:404-441`, checkpoint arms) + loom-model update.
  **Residuals**: sub-mechanism (i) in full; FIND-SMO-TAIL untouched.

### Option B — pointer-currency epochs in the ledger

Interior pointer values gain an epoch; the ledger records the volume's current epoch; replay validates
each pointer's currency before trusting it, falling back to — and this is where B collapses — either
(a) refuse loud, or (b) reconstruct the route from the journal, which *is* Option C.
`(child_addr, child_seq)` **already is a per-pointer epoch** (§4.2 stale-pointer detection,
`tree.rs:390-402`), and the ledger already carries `node_seq_watermark` (`checkpoint.rs:107-155`); the
refusal class is precisely this detection firing. B adds a global generation — pure *detection*
refinement. It repairs nothing: fallback (a) converts a 0.4–0.9 % loss into a 100 % mount refusal on the
affected rounds (an availability regression `squeezefs claim clear` cannot fix — the volume is *healthy*,
the replay is not); fallback (b) makes B a strictly-worse C. And B is blind to (i): a stranded record's
route was *current* at its apply time — epochs validate it happily.
- **Format**: interior value encoding + ledger field ⇒ `features_incompat` bit (precedent
  `FEATURE_INCOMPAT_NODE_SEQ_WATERMARK`, `superblock.rs:85-100`) ⇒ reformat of every volume, for detection
  we already have. **Size**: M-L. **Verdict: reject.**

### Option C′ — replay-time routing reconstruction from journaled SMO records (two-phase replay)

The chartered "replay-time interior reconstruction", refined to what the journal already guarantees. On
unclean shutdown, do not interleave routing changes with content application. Phase 1: apply **all** window
interior-pointer records first — ordered by (level **descending**, then seq) so upper flips route lower
ones — plus the allocator records (already consumed pre-tree, `alloc_ext.rs:407-441`). Phase 2: apply
content records (original seqs, unchanged per-key LWW gate `tree.rs:475-494`) through the now-**final**
routing. `recovery.entries` is fully materialized before apply (`backend.rs:682`); the change is iterating
it twice with a level filter — the existing `apply_replayed_interior` / `apply_replayed` machinery is
reused verbatim. Level-desc (not pure seq) is *necessary*, not stylistic: a leaf-SMO flip is itself
"content" to the interior it applies to, and pure-seq phase 1 would strand it on a doomed interior lineage
exactly as (i) strands leaf content (review.md:47-53).

- **Correctness of re-ordering**: content-fold is per-key LWW by seq (§4.2's one fold theorem) — *where* a
  record folds is irrelevant as long as it folds into the node covering its key in the final structure;
  successor images barriered before their flips can exist (`tree.rs:1076-1079`) make every final-routed
  target durable. Flip-vs-flip: a flip folded into a successor interior's image re-applies onto it and is
  LWW-skipped (idempotent); a leftover flip lands on the final interior. Replay-twice digest equality holds
  by construction: replay is read-only into the cache and the phased order is a deterministic total order
  over the same materialized entries.
- **Soundness vs (i)**: closes it. `C (c < p_flip)` is phase-2-routed by the final structure into the
  live successor — served. No content record is ever applied to a node a later window flip will unroute.
- **Soundness vs (ii) — with the root-swap carve-out stated**: phase ordering closes the *walks* for every
  SMO that journals flips: phase-1 descents stop at their target level (never load leaves); phase-2
  descents follow final routing, which never references an extent whose free is in the window (its flips
  ride the same entry ⇒ applied in phase 1 ⇒ the route already moved on). **Root-swap SMOs journal no
  pointer records** (`tree.rs:1081` `if !is_root_swap`; `:1196-1215`) while their free still rides the
  entry (`tree.rs:1093-1097`) — phase 1 cannot move that route. A mount whose ledger names the **old**
  root replays through the old structure *by design* (sound: the swap's dying floor at `res.start` holds
  the tail, so every post-swap record is in-window), and the old extents stay readable only because **A
  parks the in-window frees**. For root-swap windows, (ii)-closure is A's, not C′'s.
- **The charter's leaf-scan variant, evaluated**: rebuilding routing from leaf-level reachability
  (header-scan of allocated extents) is O(allocated extents) random 4 KiB reads at mount (a 1 TiB heap =
  4 M reads — minutes, vs the §3 mount budget) and **unsound alone**: predecessor and successor lineages
  are both checksum-valid, seq-admitted leaves; arbitrating which is *live* requires the SMO/free records
  — i.e., it degenerates into the journal-driven variant with a scan bolted on. The bounded framing
  (`SQUEEZEFS_META_CHECKPOINT_MAX_DIRTY_NODES` + window) is exactly the set the window's alloc/flip
  records already *name*, for free. Rejected in favor of C′.
- **Crash windows**: kill-9 at any point re-runs the same deterministic two-phase replay. Flip-entry-
  unwritten holes: route stays pre-flip, predecessor intact (whole-entry atomicity: flips+allocs+free ride
  one checksummed entry, one `try_admit`, one `commit_entry` — `tree.rs:1080-1097`, `:1267`). Power loss
  (D1/D2): §4.1 drop-and-resync feeds the same entry list; torn entries drop whole, so phases never see
  half an SMO.
- **Perf**: mount-only; commit path and checkpoint task untouched (checkpoints stay off the commit path).
  Same per-record work in a different order; window size unchanged. This is also what makes any future
  tail-conservatism *safe* — the "clamp widening measured worse" failure was the single-pass walk
  misrouting bigger windows, i.e., this defect.
- **Format**: none. Ring entries already carry level tags. **Size**: ~60-120 lines in
  `KvMetaBackend::open` step 5b + ordering helper; no new state machines. **Residuals**: (ii)'s
  below-tail-flip edge **and root-swap windows** (→ A); FIND-SMO-TAIL (→ PR 3); stranded predecessor
  *objects* remain mapped-but-unrouted in the cache until clock eviction (bytes only, bounded by the
  window's SMO count — acceptable, note in code).

### Option D — close the window at the SMO site

Two chartered shapes, costed at the **measured** SMO rate: the surviving FIND-VS-A sampler
(`art/1784175925_forensic`, 250 ms cadence, samples 2→14 = 3.0 s) shows `meta_kv_node_compactions` +1,548
and `meta_kv_node_splits` +1,268 ⇒ **≈ 939 SMOs/s aggregate** (4 volumes, ≈ 235/vol/s; ≈ 1 SMO per ~55
creates at the measured ≈ 52.5 k journal entries/s — not the 1-per-~1 k L1-era heuristic rev-1 cited; the
measured number is ~3× worse for D and still ≪ cap for A). **D1 (journal-carried SMO barrier)**: write +
barrier the SMO's entry *before* the route flip ⇒ flips never unwritten. Closes only the flip-unwritten
hole — already sound (§4.4 pt 4) — and *neither* (i) nor (ii); marginal cost is a **second** barrier per
SMO (`tree.rs:1076-1079` already pays one pre-images) ≈ 939 extra barriers/s at peak on the very workload
under protection. **D2 (checkpoint-before-retire)**: a full cycle inside every SMO ⇒ tail provably covers
the flip before the extent can free. Sound for (ii), blind to (i) (stranding needs no reuse), and turns
the ≤ 1 s checkpoint cadence into a ≈ 1 ms one (ledger write + barrier ≈ 939/s) on the checkpoint task —
the same territory as the clamp-widening experiment (measured worse) with an added device barrier. Both
keep the commit path clean but tax the maintenance path that keeps ring reclamation live (§4.4 pt 5 R10).
**Verdict: reject** — strictly dominated by A (same (ii)-closure, zero barriers) + C′ ((i)-closure D never
reaches).

---

## 3. Decision matrix

| | A quarantine | B epochs | C′ two-phase replay | D SMO barriers/ckpt |
|---|---|---|---|---|
| Closes (i) stranding loss | ✗ | ✗ | **✓** | ✗ |
| Closes (ii) reuse loss | ✓ (incl. root swaps + below-tail-flip edge) | detect-only | ✓ flip-journaled SMOs only; ✗ root-swap windows, ✗ below-tail-flip edge (A load-bearing) | ✓ (D2) |
| Closes child-seq refusals | ✓ | ✗ (is the refusal) | ✓ where flips exist (carve-outs → A) | ✓ (D2) |
| Closes FIND-SMO-TAIL (§1b) | ✗ | ✗ | ✗ | ✗ — own fix, PR 3 |
| Commit-path cost | 0 | 0 | 0 | 0 |
| SMO/checkpoint-task cost | ~0 (backlog ≈ 1.4 % of cap; forced cycle at watermark) | ~0 | 0 | barrier/ckpt **per SMO** ≈ 939/s |
| Mount cost | ~0 (frees park 1 ckpt) | ~0 | 2nd pass over same entries | ~0 |
| Format impact | **none** (gates on `rec.seq`) | incompat bit + reformat | **none** | none |
| Size | S (~60-100 loc + loom) | M-L | S-M (~60-120 loc) | M |
| Residual after it alone | (i); §1b | both; §1b | (ii) root-swap + below-tail edge; §1b; cache litter | (i); §1b; storm perf |

No single option satisfies the contract alone; **C′ + A** jointly close (i) and (ii) — each independently
correct, both format-free, both off the commit path — and **FIND-SMO-TAIL requires its own fix** (PR 3)
regardless of the option chosen here.

## 4. Recommendation

**Land C′ (two-phase replay) as the load-bearing fix for (i), with A (coverage-gated pending-free) as the
load-bearing fix for (ii)** — not fix-plus-belt: A is *required* wherever no flip exists to re-apply
(root-swap windows) and for the below-tail-flip edge, and it restores §4.7's reuse rule to the letter; C′
is *required* for stranding, which no reuse discipline can reach. B is detection without repair; D buys a
subset of A at ≈ 939 barriers/s. Neither C′ nor A touches the commit path, the on-disk format, or the
checkpoint cadence; both are exercisable at cargo scale with crash-equivalent reopens. **FIND-SMO-TAIL
(§1b) ships in the same program as its own PR** so the acceptance soak's `dropped_torn == 0` row holds by
fix, not by luck.

## 5. The child-seq mount-refusal class, under the recommendation

Under A, a within-window stale route always resolves the intact predecessor lineage (extent quarantined ⇒
tenant unchanged ⇒ `child_node_seq` matches); under C′, phase ordering prevents replay from walking
pre-flip routes wherever flips exist. The refusal ceases to be a legitimate crash artifact and becomes a
**true corruption detector**: keep it loud (no tolerance/retry), keep the reason tallies
(`tree.rs:343-412`). Mount-time occurrences after C′+A are new defects by definition — the acceptance soak
asserts **zero** refusals alongside zero acked loss. Regression fixtures: the three surviving in-repo fold
fixtures (`tests/fixtures/findvsa_{pred_bset.bin,extra_records.jsonl,lost_keys.jsonl}`) plus **freshly
re-captured** post-kill images (the raw FIND-VS-A images no longer exist — `/var/tmp/sqz_findvsa_copy/` is
empty; PR 1 re-captures as its first act, §6).

## 6. PR sketch

**Branch stacking (AGENTS.md Phase 6 — tests-first *commits*, never tests-only *merges*)**: PR 1's cases
are RED on dev by design, so they never merge alone. The rows below land as test-commit + fix-commit
pairs on one feature branch each: **1(a) + PR 2** (`fix/kv-smo-two-phase-replay`), **1(d) + PR 3**
(`fix/kv-mid-entry-tail-floors`), **1(b)/(c) + PR 4** (`fix/kv-pending-free-coverage`) — each branch's
first commit is its RED tests, its merge to `dev` is `--ff-only` with the full gate green; the shared
PR-1 scaffolding (image re-capture, seam, fixtures) rides the first branch and the others rebase on it.

| PR | Content | Verification (red-first) |
|---|---|---|
| 1 `test(kv)` | **First act: re-capture crash images** — the FIND-VS-A repro is scripted and on record (`repro4.sh:79-84` copies page-cache-coherent `meta{1..4}.img` pre-remount; storm scale reproduces the shapes deterministically-enough); store fresh images + `kvparse.py`-derived expectations under `.agents/findvsa/` (harness since removed from the tree — git history at `c615e3a`) with a regeneration script, committing only a truncated fixture subset if size-appropriate (1 GiB sparse volumes are not committable as-is). **Test seam**: a build-window pause point in `smo_replace` between successor load-back and the lock window (`tree.rs:1014-1123`), test-static-gated per the `TEST_CONVEYOR_POISON_APPLY_INO` precedent (`backend.rs:138`, `:2815`) — chosen over a declared-rate probabilistic RED because a per-commit contract test must be deterministic (red-first discipline; unarmed cost = one relaxed load per SMO ≈ 939/s, negligible). Contract tests: (a) stranding — commits injected in the paused build window → crash-equivalent reopen must serve the leftovers; (b) reserve-exhaustion-skipped flush + free-gate pass + reuse → reopen must neither refuse nor lose (tiny `--meta-journal-mb` + 64 KiB nodes reach the skip path; no new hooks); (c) fold fixtures + re-captured-image replay expectations; (d) **FIND-SMO-TAIL**: multi-leaf tx + partial flush (reserve-skip restoring a `rec[j>0]` floor) + kill-equivalent reopen ⇒ asserts no acked loss and `dropped_torn == 0`. Extends `tests/kv_smo_crash_completeness_tests.rs`. | (a)–(d) **RED on dev `44d14d6`** ((d) RED with `dropped_torn ≥ 1`); existing 847-test gate stays green. |
| 2 `fix(kv)` | **C′**: split `KvMetaBackend::open` replay step 5b (`backend.rs:674-723`) into phase 1 (interior records, level-desc then seq; allocator untouched) and phase 2 (content); replay-twice digest assertion extended over the phased order. | PR 1(a) green; crash-contract / crash-kill / conveyor suites green; full cargo gate. |
| 3 `fix(kv)` | **FIND-SMO-TAIL**: floors round down to entry starts — conveyor apply passes each member's `entry_start` for floor purposes (`backend.rs:2800-2806` already computes it), replay applies pass their entry start; `node_cache.rs:1141` folds the rounded value; SMO/root-swap floors already pin at `res.start` (no change). Restores the `checkpoint.rs:34-41` tail-boundary claim globally. | PR 1(d) green; `fsync`/conveyor/crash suites green; loom unaffected (floor domain only). |
| 4 `fix(kv)` | **A**: pending-free release gated on durable tail > free-record seq (live: `free_pending` carries the reservation seq; mount: replayed frees park until first post-mount durable checkpoint); **at-cap force-cycle, three clauses (Issue 9)**: pending-headroom check at SMO admission (`tree.rs:1099-1110`); `PendingFreeFull` handled like `JournalReserveExhausted` in the two `run_maintenance` arms (`checkpoint.rs:500-508`, `:548-557`) — force `checkpoint_cycle(…, true)` and retry; **and skip-and-defer on `PendingFreeFull` in the flush-pass match inside `checkpoint_cycle`** (`checkpoint.rs:658-666`, restore-floor semantics identical to the reserve skip) so a forced cycle completes its non-SMO flushes and drains via `after_durable_barrier` (`backend.rs:1191`, `:1227-1237`). **Bounded-retry-then-loud lives in the arms** (`checkpoint_past` precedent, `backend.rs:1259-1272`): N forced cycles without `pending_count` decreasing ⇒ fail the volume loud; the post-swap error (`tree.rs:1283-1284`) is unreachable under admission headroom and stays defense-in-depth only. Loom model updated (`alloc_ext_core`); stats `meta_kv_pending_free_parked/_released`. | PR 1(b)/(c) green; `tests/run_loom.sh` green; backlog gauge ≈ 1.4 % of cap under storm bench; **at-cap liveness test**: saturated FIFO + full-log dirty nodes ⇒ the forced cycle completes (ledger written, barrier, FIFO drains — no livelock); wedged-tail variant ⇒ loud volume failure after N cycles, never a retry loop. |
| 5 `bench/docs` | Acceptance: the FIND-VS-A storm shape (`repro3.sh`/`repro4.sh`: 16-worker acked-create storm, kill -9 at peak, full CPU mask + `taskset 0-15` per FIND-VS-B precedent) — **acked-loss == 0 AND mount-refusals == 0 AND `dropped_torn == 0`, ×10 consecutive on the final binary** (multi-run discipline: any failure restarts the count post-fix). Plus: `churn_unmount_soak.sh` 10/10; `FSTESTS_QUICK=1` tier; `SQUEEZEFS_VS_REGIMES=R3` scoreboard re-run (rows stay W); evidence note `.benchmarks/…-smo-currency-closing.md`; §4.6/§4.7 + FIND-SMO-TAIL deltas in `docs/design-cow-kv-metadata.md` + AGENTS stats rows. | The gate row is the charter's STOP-condition inverse, widened by the §1b signature. |
