# 2026-08-05 — Partitioning the per-volume single-appender append structures

Branch `feat/mw-partitioned-append`, developed off dev `e7ce625f` and
**rebased onto dev `4f219082`** (30 commits later: byte-range custody /
S11, FUSE-3/4 + MEM-4/7 + POSIX-14, VAL-7 + the RES tail, ENG process/CI
with the new `src/env_knobs.rs` registry — this branch adds **no env
knobs**, so it has no registry entry to make; the rebase was clean, dev
touched only `kv/node_cache.rs` inside the KV layer). Closes **spec §6.2
items 2, 3 and 4** — the three durable-format assumptions that make the
metadata plane a one-appender machine — behind incompat bit 8, **built but
NOT stamped** (execution-plan ruling **D9**).

Scope discipline (stated first, because it is the easiest thing to get
wrong): this lands **formats**, not arbitration. Who may append, and the
partitioning that keeps appenders disjoint, is §6.9 **S4/S8**. The line
this work had to reach is: *the on-disk structures no longer assume
exactly one appender, and a second appender's transactions are either
correctly merged or loudly refused — never silently dropped.*

| Commit (post-rebase) | What |
|---|---|
| `99b291a3` | `bench(meta)` — price the solo journal/ledger/allocator hot paths FIRST (the baseline the partitioned forms must not move) |
| `732f2327` | `test(meta)` — 27 partitioned-append contracts, all red |
| `2e7b5a69` | `feat(meta)` — the three partitioned forms + the not-stamped bit |
| `d7c596e8` | `perf(meta)` — the bracket's measured fix (`part()` indexes instead of dividing), 2 loom models with weakening evidence, the ragged-page case, this note |

## 1. The three assumptions, and what replaced them

### #2 — one journal ring head per volume (`kv/journal.rs`)

The ring hands out ONE monotonic logical position space (`head` + derived
lap). Two committers sharing it write over each other's positions, and
the loser's pages fail the lap/seq identity — so they are classified as
**torn and silently dropped**. Losing acked transactions without a sound
is the dangerous half; a second writer today does not fail loudly.

Partitioned form:

* the journal extent splits into `writers` equal page regions
  (`partition_ring_base` / `partition_ring_pages`); writer `w` owns region
  `w`. Each sub-ring is an ordinary `JournalRing` with its own
  `JournalCore`, laps and logical space — **entry framing, admission and
  reservation are untouched**, and solo is the whole extent, i.e. today's
  ring bit for bit;
* the page header carries the appender id in two of the four **explicit
  zero pad bytes §4.1 already reserved** (`[12..14)`), inside the existing
  checksum. Writer 0 stamps the same zeros the shipped writer does →
  un-stamped page images are byte-identical (asserted against a hand-built
  shipped header);
* `partitioned_ring_geometry_ok` is the format-time sizing law: every
  sub-ring must hold its checkpoint carve-out **plus one `MAX_ENTRY_LEN`
  entry**, or that appender parks on admission forever. The shipped 8 MiB
  floor serves up to 16 appenders.

### #3 — one A/B extent bitmap and one `advance_durable` tail (`kv/alloc_ext{,_core}.rs`)

Partitioned by **page**, because the page is the A/B write unit: one owner
per page is exactly what keeps the alternate-slot discipline
single-appender (two writers on one page put a peer's newest copy in the
slot this write replaces). Ownership is `page % writers` —
**interleaved, not contiguous** — so an existing volume's allocation,
which is dense at the low end, spreads evenly instead of handing writer 0
a full partition and its peers empty ones.

Per partition: free budget, scan hint, compaction reserve floor,
pending-free FIFO + forced-retirement overflow, and **durable-coverage
clock**. The last one is the load-bearing item: a gate seq is a position
in the *freeing* appender's own journal ring, so a shared tail would
release an extent a peer's replay window still routes into — §4.7's reuse
rule (risk R3) in its cross-appender form.

Bits stay ONE shared word array, with every mutation (`release`,
`free_pending`, `mark_allocated`) routed to the **owning** partition, so a
foreign touch lands in the owner's accounting instead of desynchronising
the toucher's. Two things built this wave stayed intact and are built
upon: DUR-4's transactional snapshot-and-clear (`write_dirty_pages`
restores the dirty bits on every fallible step) and RES-14's bounded
claim loop (the partitioned scan lives inside the same
`CLAIM_RESCAN_LIMIT` structure).

### #4 — one A/B root ledger, `slot = seq % 32` (`kv/checkpoint.rs`)

Per-appender **contiguous slot ranges** with round-robin inside
(`ledger_slot_for`), ≥ 2 slots each — which is what caps appenders at 16:
newest-valid-wins with a torn newest slot must fall back to a predecessor
**of the same appender**, and one slot each cannot provide one. Records
carry an optional 4-byte `writer_id ‖ writer_count` suffix (unambiguous by
length against the 34-byte membership-stamp prefix; 4 B against the §5.3
encoding budget's 331 spare bytes). `read_partitioned_ledger` resolves
newest-valid **per appender** and refuses loud on three classes — a record
outside its own slot range, a record whose `writer_count` disagrees with
the mount's, and a **non-authority record carrying tree roots** (two
structural authorities being §6.2 item 4's hazard itself).

## 2. The replay-merge argument

`merge_replay_windows` orders the union of the per-ring windows by
`(seq, writer_id)`. Two questions have to be answered for that to be
sound, because seqs from different rings are **not comparable** (they are
byte positions in different logical spaces).

**Why any order-preserving interleaving is correct.** Appenders are
partitioned: they never write the same object. Every key's records
therefore come from exactly one ring, and the fold that reconstructs state
is per-key LWW-by-seq (§4.2's one theorem). So the fold result depends
only on each ring's *internal* order, which every interleaving preserves.
A merge needs a total order for **determinism** (the §4.10 replay-twice
digest equality), not for correctness — which is precisely what makes N
incomparable seq spaces mergeable at all. `(seq, writer_id)` is
deterministic, independent of the order windows are handed in, preserves
per-ring order (seqs strictly increase within a ring), and for a solo
window is *exactly* today's seq-sorted `JournalRecovery::entries`.

**How a broken partition is detected.** The premise above is checked, not
assumed. `detect_partition_violations` walks the merged window and reports:

| Arm | Evidence | Why it cannot be resolved by guessing |
|---|---|---|
| `Key` | the same `(tree_id, key)` written by two appenders | with an overlap there IS no correct order: the two rings' seqs are incomparable, so LWW cannot pick a winner |
| `Structure` | a `level > 0` (interior/SMO) record from a non-authority appender | the two-phase replay applies flips in `(level DESC, seq)`, which is a sound total order only if every flip comes from one ring |
| `Extent` | an allocator delta naming an extent outside the emitter's bitmap partition | the bit and its coverage gate live in another appender's clock domain |

Plus a *page-level* arm one layer down: a verifying page header naming a
different appender inside my sub-ring is a **foreign page**
(`JournalRecovery::foreign_pages`) — attributed as such, treated as a
discovery hole, and **never folded into the torn census**. That is the
exact inversion of the silent-drop failure: the shared-ring case that used
to look like a tear now has a name.

`replay_merge` is the policy on top: any foreign page or any violation
**refuses loud**. Note how this composes with §4.1's "nothing inside the
replay window ever fails a mount loud": torn bytes are still never loud (a
tear is a legitimate power-loss artifact, and the per-ring scan handles it
unchanged). What refuses is a **checksum-valid structure** proving the
partition broke — the same class §4.5 already fails loud on
(valid-bset-after-tear) and the `child_node_seq` mismatch belongs to.

**Honest scope of detection:** it is *window*-scoped. Two appenders that
touched one object in different windows (one already checkpointed) leave
no evidence in the journal, and bounding that is the S4/S8 admission
problem, not replay's. Stated in the module docs, not hidden.

Also stated: **re-partitioning is not in-place.** Changing the appender
count moves every sub-ring boundary, so pre-existing pages land in another
appender's region and read as foreign. A count change needs every ring
drained to `tail == head` first; the ledger's `writer_count` refusal makes
an attempt to skip that step loud rather than silent.

## 3. Solo-case cost — the number that matters most

Spec §6.9's S4 gate is "solo mode is indistinguishable from today". At the
format layer that gate arrives now, so the bench group landed **before**
the implementation (`d7dbec0e`) and the shipped paths were preserved
deliberately:

* the journal ring's solo geometry, framing, admission and reservation are
  the same code — `journal_core.rs` is **untouched** (per-appender rings
  are separate cores, not a shared one with a writer dimension);
* the page-header build gains one `u16` store of a value that is 0 for
  writer 0;
* `ExtCore::claim` keeps the **shipped flat bit sweep verbatim** behind an
  `is_solo()` branch — the page-walking partitioned scan (which needs a
  division per page) is never entered on a solo volume;
* `ExtCore::new` delegates to `new_partitioned` with a one-partition map,
  so the solo structures are one `Box<[Partition]>` of length 1 (one extra
  indirection on the budget/hint/FIFO words);
* the ledger's solo encode adds one `is_some()` branch and zero bytes; the
  decode adds one length comparison.

### The bracket

**Instrument:** the `kv_append_partition` criterion group, medians, with
the two bench binaries **saved and run alternately** off disk (build each
side once; four measured runs per round) so thermal drift, background load,
and ordering cannot masquerade as a delta. **A = the tree at `d7dbec0e`**
(shipped single-appender src + the pre-change bench), **B/C = this branch**.
Venue: the house dev box, pinned `taskset -c 8-15`, `nice 10`, Tdie ~66 °C
at start; sibling agents were building in other worktrees throughout, so
**the same-side spread (A1-vs-A2) is the noise floor this box could offer**
and a delta inside it is not a signal.

**Round 1 — A-B-B-A, the first partitioned implementation:**

| bench (ns) | A1 | A2 | B1 | B2 | mean A | mean B | delta | spread A / B |
|---|---|---|---|---|---|---|---|---|
| `alloc_claim_release` | 67.55 | 62.10 | 68.84 | 75.37 | 64.83 | 72.10 | **+11.2 %** | 8.4 % / 9.1 % |
| `alloc_claim_park_release` | 141.19 | 141.07 | 143.79 | 132.12 | 141.13 | 137.96 | −2.3 % | 0.1 % / 8.5 % |
| `ledger_encode_slot` | 166.88 | 173.84 | 171.67 | 167.45 | 170.36 | 169.56 | −0.5 % | 4.1 % / 2.5 % |
| `ledger_decode_slot` | 16 499 | 15 044 | 16 390 | 15 012 | 15 771 | 15 701 | −0.4 % | 9.2 % / 8.8 % |
| `ledger_slot_index` | 0.81 | 1.13 | 0.86 | 0.86 | 0.97 | 0.86 | −11.2 % | 32.7 % / 0.6 % |

`alloc_claim_release` was the one delta whose A-range `[62.1, 67.6]` and
B-range `[68.8, 75.4]` did **not overlap** — a real cost, and the bracket
is what found it. Root cause: `ExtCore::part()` selected the partition with
`writer % parts.len()`, and a `%` on a *runtime* length compiles to a 64-bit
division — paid on every claim, release and `free_pending` by a volume with
exactly one partition to choose from. Fixed to an index-with-fallback
(`parts.get(writer)`), i.e. a compare + branch.

**Round 2 — A-C-C-A after the fix** (side A is the same saved binary, so
this round is directly comparable):

| bench (ns) | A3 | A4 | C1 | C2 | mean A | mean C | delta | spread A / C |
|---|---|---|---|---|---|---|---|---|
| `alloc_claim_release` | 67.71 | 67.84 | 63.41 | 63.73 | 67.78 | **63.57** | **−6.2 %** | 0.2 % / 0.5 % |
| `alloc_claim_park_release` | 141.23 | 141.66 | 131.98 | 148.03 | 141.45 | 140.00 | −1.0 % | 0.3 % / 11.5 % |
| `ledger_encode_slot` | 157.18 | 156.97 | 169.82 | 152.85 | 157.07 | 161.34 | +2.7 % | 0.1 % / 10.5 % |
| `ledger_decode_slot` | 14 990 | 16 641 | 16 407 | 15 110 | 15 815 | 15 758 | −0.4 % | 10.4 % / 8.2 % |
| `ledger_slot_index` | 0.74 | 0.84 | 0.94 | 1.39 | 0.79 | 1.16 | +47.7 % | 12.2 % / 38.6 % |

**Verdict: solo mode is free.** Every row is inside its own noise floor
except `alloc_claim_release`, whose tight spreads (0.2 % / 0.5 %) make its
**−6.2 %** the one row with a real signal — and it is *faster* than the
shipped path, because the partitioned claim reads its budget/hint out of one
`Partition` struct (three words adjacent in one cache line) instead of three
separately-placed `ExtCore` fields. `ledger_slot_index` swings ±48 % between
rounds and disagrees in sign (−11.2 % then +47.7 %) on a **sub-nanosecond**
single-modulo bench: noise, reported rather than hidden. `ledger_encode_slot`
and `ledger_decode_slot` sit under 3 % with 8–11 % spreads.

The journal side needs no row of its own: `journal_core.rs` — the wait-free
admission/reservation core the commit path pays per transaction — is
**byte-for-byte untouched** (per-appender rings are separate cores, not one
core with a writer dimension), which the existing
`kv_journal/core_admit_reserve_advance` bench continues to cover.

## 4. Verification

All on the rebased tree (base `4f219082`), `--test-threads=1`:

| Gate | Result |
|---|---|
| `tests/kv_partitioned_append_tests.rs` (27 cases) | **green** |
| `kv_journal_tests` (21), `kv_alloc_tests` (14), `kv_node_tests`, `kv_tree_tests` (12) | green |
| `kv_smo_crash_completeness_tests` | 11 green, 1 **pre-existing** red (below) |
| `crash_contract_tests` | 24 green, 1 **pre-existing** red |
| `kv_backend_tests` | 32 green, 2 **pre-existing** reds |
| `cargo clippy --all-targets --all-features -- -D warnings` | clean |
| `cargo fmt --check` | clean |
| `cargo doc --no-deps` | no new warnings (14 pre-existing, none in the touched files) |
| `cargo bench --benches -- --test` (smoke) | green |
| `tests/run_loom.sh` | **63/63** (61 pre-existing + 2 new) |
| `src/env_knobs.rs` registry | n/a — this branch adds no env knob |

### Pre-existing reds on the base commit (NOT this branch's) — **four, one family**

Verified twice, before and after the rebase, by checking out the base
commit's `src` (and, where the test file itself changed, the base test
file) and re-running:

| Test | Shape |
|---|---|
| `kv_smo_crash_completeness_tests::replay_twice_digest_stable_across_smo_windows` | replay-twice digests diverge |
| `crash_contract_tests::test_kv_v3_torn_newest_ledger_mount_serves_predecessor` | post-fold digest mismatch after the torn-newest-slot fallback |
| `kv_backend_tests::v3_mutations_survive_clean_shutdown_remount` | post-fold digest mismatch across shutdown/remount |
| `kv_backend_tests::v3_ring_full_liveness_storm_drains` | ring-full liveness storm |

All four fail **byte-identically** with this branch's `src` reverted to
base `4f219082` (counts match exactly: crash_contract 24 passed/1 failed;
kv_backend 32 passed/2 failed). Flagged for the orchestrator: three of the
four are the same shape — a replay or remount serving *different post-fold
state* — which smells like ONE defect in the current dev tip's
replay/flush path rather than three, and it is squarely in the area this
branch's formats sit on top of. Worth a red-first repro before S4 wires
the partitioned forms into mount, because a mount-side divergence would be
indistinguishable from a partitioning bug once N appenders exist.

### Loom, with weakening evidence

Two new models against the shipped `alloc_ext_core.rs`
(`#[path]`-included, `LOOM_MAX_PREEMPTIONS=3`):

* `alloc_ext_partitioned_claims_never_cross` — two appenders claiming
  concurrently never get the same extent and never get one from the
  other's pages; budgets settle exactly.
  **Weakened** to the pre-change structure (one shared free budget + one
  flat scan for every appender) ⇒ FAILS: *"writer 1's partition holds
  exactly 2"*.
* `alloc_ext_partitioned_coverage_gates_never_cross` — both appenders park
  an extent at gate seq **100** (the same number in two different ring
  spaces, the confusion a shared clock cannot tell apart); writer 0's tail
  passing 100 must release only its own, leave the peer's bit set, and
  leave the peer's clock at 0.
  **Weakened** to one whole-volume tail (every partition drains on any
  advance) ⇒ FAILS at the `released == [mine]` assertion.

## 5. What S8 (and S4) still owe on top of this

The formats are expressible; the machinery that makes them *usable* is
not, and deliberately so:

1. **Mount wiring (S4).** `KvMetaBackend::open` still builds one solo ring,
   one solo allocator, and reads the ledger with `read_newest_ledger`. The
   partitioned entry points are kept alive by the test file (the K1–K5
   liveness convention) until an appender set is threaded through `open`.
   Once wired, the backend should write partitioned records whenever the
   bit is present (even at `writers == 1`, whose placement is identical),
   so the transition is one-way.
2. **Appender admission and disjointness.** Nothing here says who may
   append. The D0 writer claim is singular (§6.2 item 7 — a claim *set*
   record plus NVMe registrants is that item's fix), and the partition
   that keeps appenders off each other's objects is the slot map's job.
3. **The remaining §6.2 items.** Durable block refcounts (item 1, sibling
   agent), per-writer ino cursors (item 5 — cheapest, VL5b's `slot_cursors`
   already exist), `offset ‖ incarnation` block keys (item 6), the claim
   set (7), writer-scoped `active_block:`/`mapping:` keys (8), durable
   per-ino layout versions (9), node identity in the staging stamp (10).
4. **Node-cache revalidation (the item §6.2 calls harder than any of the
   ten).** Explicitly NOT this branch's, but the formats here *assume* the
   answer is **partitioning, not coherence**: two appenders must never
   cache the same node. Concretely, this branch's structures assume (a) the
   root authority is the only appender that mutates tree structure, so
   interior nodes have one cacher; (b) content records from peers are
   folded into the authority's trees at replay, not concurrently; (c) no
   mechanism here revalidates a leaf that a peer wrote — a peer writing a
   key whose leaf the authority has cached is a `PartitionViolation::Key`,
   detected at replay but **not** prevented at runtime. S8's function
   shipping is what keeps that from being needed; if S8 ever lets two
   processes apply records to one volume's trees, the node cache needs the
   §6.8 item-2 revalidation path first.
5. **Tail interlock across appenders.** Each appender's ledger record names
   only its own ring's tail, and each partition's coverage clock is its
   own — sound in isolation. What is missing is the *cross*-appender rule:
   the root authority's checkpoint must not advance structure past records
   a peer has not yet had folded, and a peer must not advance its own tail
   past records the authority has not folded. That is a protocol
   (function-shipped commits, S8), not a format.
6. **Stats surface.** `foreign_pages`, `foreign_page_writes`, and the
   violation classes are struct-local accessors read by tests. Surfacing
   them on the stats inode (they are exactly the must-stay-0 tripwire
   shape the house uses) belongs with the S4 wiring, which owns
   `fuse_client.rs`.
7. **Detection cost.** `detect_partition_violations` builds a
   `HashMap<(tree_id, key)>` over the replay window — mount-time only and
   bounded by the ring, but it allocates per record. If the merge ever
   moves onto a hot path, that map wants the arena treatment.
