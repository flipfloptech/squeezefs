# DLM S5 — the read-only coherent mount: cost of the reader gate, cost of the purge, and what item 3 still needs

**Branch** `feat/dlm-s5-readonly-mount` · **Base** `dev` @ `a703ce3f`
**Scope** pre-RC engineering spec §6.8 items **1, 4, 5, 6** (the "days of work" set). Item 2 (node-cache revalidation) is `feat/mw-node-cache-coherence`; item 3 (the freed-offset grace period) is assessed below and **not taken**.
**Instrument** Criterion (`benches/write_path_bench.rs`, groups `ro_gate` / `ro_revalidate_purge`), release profile, default features. **Substrate** dev box, file-backed volumes for the correctness legs; the numbers below are CPU-only micro-rows and are substrate-independent by construction (no device I/O on either path).
**Contracts** `tests/readonly_mount_tests.rs` (22 legs, green, `--test-threads=1`).

---

## 1. The number that matters: what a READER feature charges WRITERS

A reader mount must not tax the writer it reads behind. The reader gate is one
process-global relaxed atomic load feeding a never-taken, perfectly-predicted
branch, placed at each write-path ownership transition (four allocation
entries, `begin_free`/`free_block`, `begin_patch_sole_owner`, the
in-place-overwrite lever, `ReclaimQueue::enqueue`, the recovery walk).

| Row | Median | Meaning |
|---|---|---|
| `ro_gate/latch_probe` | **391 ps** (CI 386–399 ps) | the gate in isolation — the whole cost added per gated site |
| `ro_gate/w1_patch_predicate_write_mount` | **80.7 ns** (CI 79.2–82.2 ns) | the hottest gated site, gate included: `begin_patch_sole_owner` + `publish_block` |
| `ro_gate/w1_patch_predicate_reader` | 25.1 ns | the reader's refusal path (short-circuits before the incarnation word is touched — a refusing gate is *cheaper*, which is the honest shape) |

**Verdict: free on the writer path.** W1 is the right row to judge on — it is the
gated site that runs at per-4-KiB-write frequency (61–67 k IOPS,
`.benchmarks/2026-07-17-rand-write-program-closing.md`), two orders of
magnitude more often than `allocate_block` (once per 4 MiB block). 391 ps is
**0.48 %** of that call's 80.7 ns and sits inside the row's own run-to-run CI
(±1.5 ns ≈ 1.9 %), i.e. the gate is not separable from noise at the site it
matters at. No writer-path row was restructured, no lock was added, no
allocation was added, and the latch is never written after mount.

## 2. The item-5 purge pass

`purge_reader_block_keys` enumerates the three enumerable block-key stores,
purges each key through `TieredCache::purge_block_key` (the ONE legal purge —
all five stores, grep-guarded) and drops the ledger-invisible read-lane hold
whole (`trim_to(0)`).

| Census | Median | Per key | Throughput |
|---|---|---|---|
| 1,024 keys | 420 µs | 410 ns | 2.44 M keys/s |
| 8,192 keys | 3.69 ms | 450 ns | 2.22 M keys/s |

It runs **at most once per revalidation epoch that observed the writer's roots
advance** — never on an idle writer, never per op. At the default 50 ms cadence
an 8,192-key warm census costs 3.69 ms of one core per epoch (≈ 7 % of a core
while the writer is continuously checkpointing, 0 % when it is not). That is the
price of bounding data staleness to one interval without a layout walk, and it
is the term to revisit first if a reader's cold-read rate is ever the
constraint (the scoping answer would be scoped attribution — see item 3).

## 3. D0 is unweakened — the argument, and how it is pinned

The reader's shared lock is a **released probe**, not a retained `LOCK_SH`.
This is the one design decision in the increment that departs from the spec
bullet's literal wording (`flock(LOCK_SH)`), and it is deliberate:

* `flock` `LOCK_SH` conflicts with `LOCK_EX`. A *retained* shared lock would
  make an attached reader refuse a legitimate write mount on the same host —
  and, order-reversed, make a live writer refuse every local reader. That is a
  reader feature taxing (indeed denying) writers, and it would also make the
  1-writer-plus-N-readers shape untestable in-process and undemonstrable on a
  single dev box.
* A reader needs no exclusion, because it mutates no plane. It therefore takes
  none and grants none. What the probe still buys is the one local question a
  reader can answer for free: whether an exclusive holder (write mount, or an
  offline guarded verb) exists on this host — logged beside the guarantee class.

Pinned by `tests/readonly_mount_tests.rs`:

| Leg | Pins |
|---|---|
| `read_only_mount_admitted_while_a_writer_holds_the_volume` | the §6.4 refusal is bypassed for RO; the writer keeps serving |
| `a_reader_never_refuses_a_writer_mount` | reader-then-writer succeeds (no tax) |
| `second_writer_still_refused_with_readers_attached` | **write exclusion intact** with 2 readers attached |
| `read_only_mount_leaves_a_foreign_writer_claim_untouched` | a fresh FOREIGN claim still refuses a write mount, and the reader neither reclaims, preempts nor heartbeats it |
| `read_only_mount_writes_nothing_to_the_volume` | device image byte-identical (xxh3) across an RO session including a mutation attempt |
| `read_only_mount_reports_the_reader_guarantee_class` | `writer_guard_mode == "reader"` — its own row, never `unguarded` (which means "not a mount") |

The existing D0 contracts (`tests/mount_writer_guard_tests.rs`,
`tests/mount_registration_tests.rs`) pass unchanged — no assertion was
weakened, relaxed, or re-scoped.

## 4. Item 3 — the freed-offset grace period: assessed, not taken

§6.8 calls it "the highest-value single item in the coherence analysis, because
it converts §6.3's cross-file staleness into *bounded* staleness". With item 5
landed the staleness is already bounded by one revalidation interval; item 3 is
what **eliminates** the window. It was not taken, for one structural reason and
two scope reasons.

**The structural blocker (new, and worth recording).** The spec's mechanism is
"refuse to reallocate an offset until every registered reader has acknowledged
passing that epoch, **riding the existing `client:` heartbeat**". A reader
cannot ride that heartbeat: `client:{uuid}` is an xattr commit on ino 1 under
an exclusive `I{1}` guard, i.e. a metadata **write** — precisely what item 1
(and this branch's `read_only_mount_writes_nothing_to_the_volume` contract)
refuses. So item 3 needs a reader→writer acknowledgement channel that is not a
metadata write. The two candidates:

1. **S3 `cluster_wire`** (spec §6.7 transport; a sibling's file) — the right
   home: reader epoch acks are exactly the "client-initiated backchannel whose
   health is observable" pattern §6.6 takes from NFSv4.1, and S6 moves
   membership off the journal anyway. This makes item 3 depend on S3/S6, not
   on the `client:` records.
2. **Permitting exactly one reader write** (its own registration). Rejected on
   two counts: it breaks the writes-nothing contract that makes a reader
   admissible beside a *fenced or foreign* writer at all, and §6.5 pt 3 already
   measures that plane saturating at ~4,550 clients (455 beats/s available vs
   the 1,500/s that 15 k clients need) because ino 1 routes to one volume.

**What the writer half needs** (expressible in the files this branch owns,
which is why it is worth stating precisely):

* an **epoch** the reader already observes — the checkpoint/ledger seq is it
  (`ReaderEpoch::ledger_seq`, shipped here);
* a **quarantine** between `begin_free` and `finish_free`: today a terminal
  free's offset becomes reallocatable when `finish_free` runs (after the
  background reclaim). Item 3 adds "…and no registered reader is still below
  the epoch in which the offset was freed", i.e. an epoch-keyed hold-back list
  in `block_reclaim.rs` plus a release condition in
  `BlockAllocator::allocate_block`. Counters `dlm_grace_{reclaims,conflicts}`
  and `dlm_quarantined_offsets` are already named by §6.9 for this;
* **fencing, not waiting**: a reader whose ack ages past a TTL is declared dead
  and stops holding offsets. Needs the membership/TTL plane above.

**One prerequisite did improve this wave**, and it should be recorded: durable
block refcounts (spec §6.2 item 1, incompat bit 8 — built, not stamped) make
"is this block still referenced?" answerable from the `TREE_BLOCK_REFS` census
**without the inode-tree walk**. That removes the objection that a grace
period's release condition would need an O(tree) query per offset; the release
condition is now a prefix population count. It does not remove the
acknowledgement-channel blocker.

**Estimate**, with the channel available: writer-side quarantine + release
condition + counters ≈ days; the reader ack half is S3/S6-shaped ≈ weeks, which
matches the spec's own "2 and 3 are weeks" classification.

## 5. What remains before N readers can be demonstrated on a real cluster

In dependency order — nothing below is a change of plan, all of it is stated in
§6.8/§6.9:

1. **Item 2, node-cache revalidation** (`feat/mw-node-cache-coherence`): until
   its arm is installed through the `NodeCacheRevalidate` seam, a reader's
   METADATA view is a mount-time snapshot. A cluster demo of "N readers see the
   writer's new files" is impossible without it; a demo of "N readers stream
   existing files while a writer works" is possible today. The mount logs the
   difference loudly and `ro_node_cache_nodes_dropped == 0` is its signature.
2. **A capability row** — §6.9's S5 gate is "N readers × cached stat/s". It
   needs the tcp substrate (`SQZ_DEVSUB_TRANSPORT=tcp`, the two-substrate rule)
   or a real fabric, one writer host and N reader hosts sharing the namespaces,
   and a sustained ≥ 60 s row per the sustained-state rule. Not runnable from
   this branch: the cluster venue is held by a live mount.
3. **Item 3** for the silent-window elimination (above), which is also the
   multi-writer prerequisite §6.3 names.
4. **Reader visibility** (`squeezefs clients` lists no readers today) — S6.

## 6. Honesty ledger

* Every number here is a Criterion CPU micro-row on one box; they are
  **same-box relative** by the standing baseline rule, and none of them is a
  throughput claim.
* No cluster, mount-level or fabric row is claimed by this note. No scoreboard
  row was run. No `.benchmarks` sustained row exists for the reader path yet —
  item 2 gates the honest version of it.
* The one measured *product* claim made here is negative and it is the
  important one: **the reader gate costs the writer 391 ps per gated site**,
  which is inside the noise of the site it rides.
