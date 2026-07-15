# PR M9 acceptance — KV record-fold slimming: fold-forward overlay + snapshot memo (§5.7 D7)

| | |
|---|---|
| **Program** | metadata-throughput (`docs/design-metadata-throughput.md`), PR M9 — §5.7 D7.a (fold-forward overlay head) + D7.b (snapshot fold memo) + the equivalence guard + the revision-1 memory accounting. Companion law: `docs/design-cow-kv-metadata.md` §4.2 — the fold FUNCTION is untouched; D7 changes *when* it runs, never *what* it computes |
| **Branch** | `perf/kv-fold-slimming` (off dev `ed28ad6`) |
| **Box / rails** | same 3.5 GHz-capped box as baseline/M3/M6/M7; daemons caged (`systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=0`); storms + gates `taskset -c 0-15` (builds/benches on 16-31); binaries `sqm9` / `sqm9pre` (kill-pattern immunity); kills by PID only; unique sandbox `~/tmp/m9_fold_936854/` (artifacts preserved); Tctl 52-56 °C throughout (≤ 88 °C rail never approached) |
| **Substrate** | **B1 null_blk** (`/dev/mdb_m9fast`, baseline configfs recipe: 3 GiB, `memory_backed=1`, `cache_size=1024`, `completion_nsec=0`, `irqmode=0` — `fua=1`, write-back). M9 has no barrier-shape surface, so the single fast substrate carries the CPU-attribution acceptance; counters and profile shares are load/substrate-invariant per the M3/M6/M7 precedent |
| **Shape** | mdstorm one-dir `create` (8 threads × 100 k) + `stat` pass; per-phase `.stats` snapshots; **pre tip (`sqm9pre` = dev @ `ed28ad6`) vs M9 tip (`sqm9`) fresh release builds**, paired same-session |
| **Session hygiene** | **every timed row is DIRTY-flagged**: the steady juicefs co-tenant persists (5 root daemons on `/mnt/juicefs`, load flat 19.6-22.3 for the whole window, Tctl 52-56 °C). M6/M7 house fallback applies: same-session pairs under the steady load, rows flagged, the load-invariant counter/profile-share gates stay authoritative |

## Verdicts up front

1. **The acceptance artifact is delivered: `InodeDelta::decode` is off the top
   table.** Pre-tip create-storm profile: `NodeSnapshot::lookup` **19.95 %** of
   daemon cycles with `InodeDelta::decode` **6.47 %** as the #2 symbol
   (+ `Reader::u64` 2.40 %). M9 tip, same storm: `InodeDelta::decode`
   **0.03 %**, `Reader::u64` 0.01 %, `NodeSnapshot::lookup` **0.40 %** — the
   per-read record-fold re-decode tax is structurally gone (the 0.03 % residue
   is the *write-side* fold-forward decoding its own incoming Δ once per
   apply, exactly the designed cost model).
2. **mdstorm one-dir create: 7,100 → 10,169 ops/s (+43 %, DIRTY)** — median of
   2 pre runs (7,171 / 7,030) vs 3 tip runs (10,169 / 10,217 / 10,116). Far
   above §5.7's +5-8 % expectation because on *this* dev tip (post-M3/M6/M7)
   the fold machinery had grown to ~34 % of daemon cycles (the transport and
   entry-economy levers already landed), and every reclaimed µs lands 1:1
   inside the kernel-`i_rwsem` serial chain (§5.8). `stat` row flat (250-266 k
   both tips — moka-served above the engine, as expected).
3. **The fold algebra is provably untouched** (risk R7): the K1 extension
   proptest pins `fold_forward ≡ fold_newest_first` over random histories at
   the record layer, and the end-to-end proptest drives random
   put/Δ/delete/freeze/append/mid-range-rollback histories through a real
   node asserting first *and* repeat (memo) reads byte-equal a from-scratch
   fold over the survivors. The §4.4 pt 4 stale-head hazard (rollback removal
   *under* a newer concurrent Δtime) has a deterministic pin; heads are
   invalidated, never guessed at. Full suite green: **824 passed / 0 failed**
   serial, incl. crash-contract + replay-twice digest suites (fold results
   feed replay; equivalence makes divergence impossible — proven green, not
   assumed).
4. **§5.7 memory accounting holds under a deliberately tiny budget** — and the
   acceptance row **caught a real liveness bug** (§Finding below, fixed
   in-branch, RED-first): at `SQUEEZEFS_META_NODE_CACHE_MB=8` (32 nodes) the
   storm now completes at **full speed** (10,099-10,234 ops/s ≈ the 512 MiB
   rows), `meta_kv_fold_memo_bytes` bounded (every 200 ms sample = 0; the
   gauge lives sub-cadence and finals at **exactly 0** — Drop-owned, leak-free),
   `meta_kv_node_cache_evictions` 842, RSS peak 523 MiB inside the 8 G cage,
   zero daemon errors.
5. **Loom 24/24**; no new model. Disposition (⚙ judged, as the mission asks):
   D7 adds **no new lock-free protocol** — the overlay head is mutated only
   under the existing per-node write lock (no second synchronization regime,
   the R7 mitigation verbatim); the memo is populate-once `std::sync::OnceLock`
   cells reached only through an **immutable** snapshot (every populate of a
   key computes the same deterministic fold, so all interleavings serve
   byte-equal results — pinned by an 8-thread stress test instead: loom cannot
   model std's OnceLock internals and re-implementing the cell just to model
   it would verify the mock, not the code); the budget gauge is Drop-paired
   counters with no compound invariant. `node_state_core` is untouched, so
   the existing models remain the authority for the lifecycle word.
   The one lifecycle-adjacent change (eviction's strong-count gate) *narrows*
   an existing transition's trigger without adding states or orderings.
6. **Criterion: existing meta benches non-regressing; the new `kv_fold` rows
   quantify the win in-tip**: overlay-head serve **90.5 ns**, memo serve
   **37.9 ns**, from-scratch fold of the same 16-Δ chain **360.3 ns** — the
   head is 4.0× and the memo 9.5× cheaper than what every read paid pre-M9.
   `lookup_file` **1.558 µs → 463 ns (−70 %)**.

## Perf diff — the lever-8 kill (8×100 k one-dir create storm, B1, dwarf callgraph, 8 s sample)

Pre (`sqm9pre` = dev @ `ed28ad6`, 8 K samples / 19.48 G cycles):

| % | symbol |
|---:|---|
| **19.95** | `kv::node_cache::NodeSnapshot::lookup` (fold walk + delta apply inlined) |
| **6.47** | `kv::record::InodeDelta::decode` |
| 4.00 | libc memcmp |
| 3.38 | `arc_swap::debt::Debt::pay_all` |
| **2.40** | `kv::record::Reader::u64` |
| 2.10 | `kv::bset::MergeIter::run_end` |
| 1.98 | `kv::node_cache::NodeSnapshot::next_live` |
| 1.29 | `kv::node_cache::RecordIndex::group_bounds` |

M9 tip (`sqm9` @ `90e2b63`, 9 K samples / 17.98 G cycles, higher throughput):

| % | symbol |
|---:|---|
| 5.20 | `arc_swap::debt::Debt::pay_all` |
| 4.27 | libc memcmp |
| 2.99 | `NodeSnapshot::next_live` (cursor arithmetic — its folds now head/memo-served) |
| 2.65 | `MergeIter::run_end` (freeze/compaction write side, not the read fold) |
| … | *(transport / runtime symbols)* |
| **0.40** | `NodeSnapshot::lookup` |
| **0.03** | `InodeDelta::decode` (write-side fold-forward of its own incoming Δ) |
| **0.01-0.04** | `Reader::u64` / `Reader::u32` / `Reader::finish` |

Reading: the read-path fold machinery collapses 19.95+6.47+2.40 ≈ **29 %
of daemon cycles → < 0.5 %**. `next_live`/`run_end` *shares* tick up because
the denominator shrank ~43 % — their per-op cycles are flat (they were never
decode work: `next_live`'s residue is partition-point walking, `run_end` is
bset merge on the writeback side).

## mdstorm rows (B1, default flush, 8 × 100 k, one dir, all DIRTY)

| run | bin | create ops/s | stat ops/s |
|---|---|---:|---:|
| pre_r1 | sqm9pre | 7,171 | 266,133 |
| pre_r2 | sqm9pre | 7,030 | 250,015 |
| tip_r1 | sqm9 | 10,169 | 262,902 |
| tip_r2 | sqm9 | 10,217 | 263,394 |
| tip_r3 (final bin) | sqm9 | 10,116 | 252,218 |
| **median** | | **7,100 → 10,169 (+43.2 %)** | flat |

## `meta_kv_fold_*` counter table (tip_r3, per-phase deltas)

| counter | create (100 k) | stat (100 k) | reading |
|---|---:|---:|---|
| `meta_kv_fold_head_serves` | **759,632** | 47 | ≈ 7.6/create — every LOOKUP/CREATE chain probe of the hot parent + dentry keys serves from the D7.a head |
| `meta_kv_fold_memo_hits` | 1,067 | 1 | the post-freeze window (D7.b's target) — ~50 % hit rate on the memo-eligible residue; the write storm swaps snapshots too fast for more, **by design** (D7.a owns write phases, D7.b owns read-mostly ones — the 37.9 ns bench row and `lookup_file` −70 % show it working where it applies) |
| `meta_kv_fold_memo_misses` | 1,082 | 7 | misses count exactly where a populate follows (delta-materialized folds), so hits/(hits+misses) is the honest D7.b rate |
| `meta_kv_fold_memo_bytes` | 0 at phase end | 0 | gauge — populate-once cells die with their snapshot (Drop-exact) |
| `meta_kv_delta_orphans` | 0 | 0 | semantics preserved (now counted per-materialization, not per-read) |
| `meta_kv_journal_entries` /create | **1.00543** | — | G4 entry economy byte-class unchanged (M6/M7 preserved) |

`stat` barely touches the engine (moka attr-cache above it) — its rows are
the null-check they should be.

## Criterion (paired same-session, cores 16-31, DIRTY box)

| bench | pre @ ed28ad6 | M9 tip | Δ |
|---|---:|---:|---|
| `kv_meta_metadata/lookup_file` | 1.5575 µs | **463.3 ns** | **−70 %** (the trait-path point lookup is now memo/head-served) |
| `kv_meta_metadata/create_unlink_file` | 86.68 µs | 84.52 µs | −2.5 % |
| `kv_tree/point_lookup_hot_100k` | 921.8 ns | 941.4 ns | +2.1 % (repeat-noise class; distinct-key scan — memo deliberately does not populate plain-Put folds, see Deviations) |
| `kv_tree/insert_48b` | 3.678 µs | 3.796 µs | +3.2 % (inside the M7-documented ±5-9 % repeat-noise band; the apply now materializes one head — the designed write-side cost) |
| `kv_bset/fold_lookup_4src_delta_chain` | 730.7 ns | 724.6 ns | −0.8 % (raw algebra untouched) |
| `kv_fold/hot_parent_key_probe_overlay_head` (new) | — | **90.5 ns** | D7.a serve, zero decodes |
| `kv_fold/hot_parent_key_probe_bset_resident_memo` (new) | — | **37.9 ns** | D7.b serve, zero decodes |
| `kv_fold/hot_parent_key_probe_from_scratch_fold` (new) | — | 360.3 ns | the pre-M9 per-read price, kept as the in-tip comparator |

## Tiny-budget storm (the §5.7 memory-accounting gate)

`SQUEEZEFS_META_NODE_CACHE_MB=8` (32 × 256 KiB nodes), same 8×100 k create
storm + stat pass, 200 ms gauge sampler (47 samples):

| | |
|---|---|
| create | **10,099-10,234 ops/s** across runs — indistinguishable from the 512 MiB-budget rows |
| `meta_kv_fold_memo_bytes` | 0 at every sample; **final 0 exactly** (Drop-owned: cells die with their snapshots; peak lives sub-cadence). The in-suite churn test (`tiny_budget_eviction_bounds_memo_gauge`) pins the bound deterministically: 3 × 48-node laps through a 4-node budget, peak ≤ capacity bound, final == baseline |
| `meta_kv_node_cache_evictions` | 842 (sweep active the whole run) |
| daemon RSS peak | 523 MiB (8 G cage; no OOM-class growth) |
| daemon log | zero errors |

### Finding (caught by this row, fixed in-branch, RED-first): eviction reload-thrash on externally-held nodes

The first tiny-budget run **failed at ~86 k creates** with a loud
`EINVAL` — `commit retry budget exhausted (revalidation never passed)`. The
pre-M9 binary passes the same storm, but only by accident: 32 mapped nodes sat
exactly *at* the 32-node budget, so the sweep never ran. M9's overlay/memo
charge keeps the cache persistently a few KiB over budget, and the sweep
would evict nodes the conveyor pass was **holding** between resolve and lock.
Evicting a held node frees no memory (the holder's Arc keeps node + snapshot
alive) but severs the mapping: the holder revalidates `Stale`, re-resolves,
demand-reloads, and the next sweep severs it again — reload-thrash to the
bounded-retry EINVAL. Fix: the sweep skips candidates with external strong
references (`strong_count > map + probe`) — it may only reclaim what dropping
the map reference would actually free. Pinned by
`eviction_skips_externally_held_nodes` (hold across sweeps → mapping
survives; release → reclaimed). This is a pre-existing hazard class that
default budgets never reached; M9's accounting exposed it, exactly what the
acceptance row exists for.

## Verification

- **TDD red-first**: `tests/kv_fold_slimming_tests.rs` landed RED (5 failing
  behavioral contracts: head serve zero-decode via the
  `META_KV_FOLD_RECORD_DECODES` pin, memo serve + cross-thread race, budget
  charge growth, gauge populate/bound) with the equivalence pins green
  against the from-scratch fold, as the theorem demands. GREEN across the
  branch; the eviction finding got its own RED→GREEN cycle.
- **Full cargo gate per commit** (taskset 0-15, `CARGO_BUILD_JOBS=12`):
  clippy `-D warnings`, fmt, `--test-threads=1` (final: **824/0**), doc,
  bench smoke (42 benches; smoke caught the cold-bench budget interaction
  before any measurement run did — the gate working as designed).
- **Loom**: `tests/run_loom.sh` **24/24**; no new model (disposition in
  Verdict 5).
- **Crash contract**: `crash_contract_tests` + `crash_kill_tests` +
  `conveyor_tests` + `meta_entry_economy_tests` + `kv_scale_tests` green
  (replay-twice digest equality rides the same fold the overlay/memo serve —
  the equivalence guard is what makes that non-negotiable hold).
- **No fstests row** (per the M9 verify row: no FUSE/data-path surface).

## Deviations from the §5.7 letter, with rationale

1. **Memo populate scope**: only *delta-materialized* folds (`Cow::Owned`)
   populate cells — the §5.7 D7.b text targets "bset-resident **deltas**",
   and populating plain-`Put`/tombstone/no-record folds (already zero-decode)
   measurably taxed the distinct-key hot-lookup bench (+12.6 %) for zero
   possible win while churning the 8 cells (tombstone deserts, ENOENT
   probes). Misses count only where a populate follows, so the §9 hit rate
   reads as designed.
2. **Memo horizon seq**: stored per the design tuple and enforced as a
   `debug_assert` immutability tripwire on every hit (with snapshot-scoped
   memos it is vacuously current; the assert is what keeps that assumption
   loud if snapshots ever mutate in place).
3. **`META_KV_FOLD_RECORD_DECODES`**: an extra crate-public counter (not on
   the stats JSON — §9 names exactly four fold fields) counting fold-executed
   record decodes; it is the RED-first zero-decode pin and the profiler's
   live twin.
4. **Orphan-delta counting** moved from per-read to per-materialization
   (same records counted, no longer multiplied by read count).
5. **Eviction strong-count gate**: not in the design — the tiny-budget
   acceptance row surfaced it (§Finding); fixed in-branch rather than shipped
   as a known wedge.
6. **AGENTS.md untouched**: M12 owns the stats-surface documentation closure.

## Artifacts

- Sandbox `~/tmp/m9_fold_936854/` (preserved): `results.tsv`, per-phase
  `.stats` snapshots (`stats/{pre,tip}_r*/`), perf data + top tables
  (`stats/{pre,tip,tipfinal}.perf.*`), tiny-budget sample series
  (`stats/tiny_budget.samples.tsv`, `.final.json`), harness
  (`lib.sh`, `run_storm.sh`, `run_perf.sh`, `run_tiny_budget.sh`,
  `setup_substrate.sh`, `mdstorm.c`).
- Substrate `/dev/mdb_m9fast` (session-scoped null_blk; torn down post-merge).
