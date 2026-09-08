# 2026-09-08 — R-5: read-handler economy — the kernel READ handler's per-op ledger (8 → 0 allocs warm, 11 → 1 cold zc; 26 → 0 contended RMWs; 13 → 9 clock reads) and the levers that removed each term

**Program:** [`docs/design-e2e-perf-audit.md`](../docs/design-e2e-perf-audit.md)
§3.4 Read #6 / #7 / #8 / #9 + the R-1 residual (§4.3), §5.3 row 16; the
read ledger Appendix B fat items 6–9. **Inputs:** the 2026-09-02 read
ledger's per-op terms ("8 allocs/op + ~20 global atomics + ~9 clock
reads"), `tests/kernel_op_economy_tests.rs` (the PERF-12 alloc-site
instrument), the 2026-07-28 IPC op-economy precedent
(`.benchmarks/2026-07-28-ipc-op-economy.md`: 12 → ≈ 0 allocs/op bought
+22–29 % il IOPS), R-4's `ShardedCounter`/PERF-3 layout
(`crates/fuse3/src/raw/read_phase.rs`). **Branch:**
`perf/read-handler-economy` from dev `36d517f3`; commits `a6d7a6d8`
(red-first cold zc-leg budget + bench rows), `e0fa407d` (alloc levers),
`cfc8da46` (atomics / clock / kvmap levers), plus this docs commit.
**Tier: in-process measured** (the alloc law on the op-economy fixture,
a release microbench on the dev box) — **the field `rr_4k` CPU/op row on
squeeze-test is the parent's and is NOT claimed here** (§7).

**Verdict up front.** The kernel READ handler's per-op cost had three
faces the ledger named and this campaign priced site by site (§2): on
the WARM ladder serve **8 allocations per op** (two `Vec<u64>` custody
fingerprints, a `Vec` + key clone + one-element sort for a one-block map
probe, a heap W2 overlay key, an owned key copy + an owned recheck resolve
in the fetch ladder, and an `Arc` "backing" the reply minted for a
transmuted-slice era that ended 2026-08-02); on the COLD FUSE-zc leg — the
field kern rand-4k posture — **11 per op**, seven of them key RE-PARSES
(`zc_device_resolve` and the three incarnation probes each minted one or
two `String`s and `Arc`-cloned the allocator); and on every warm serve
**~26 process-global atomic RMWs** on lines every handler lane shares
(seven three-word histogram records + five ledger counters) plus **13
clock reads** where phases abut. **Every term is gone** (§3): the alloc
law now pins **0.00 allocs/op warm** (budget 1, was 9) and **1.00 cold
zc** (the injected fetch's own `Box::pin`; budget 2, new — the live
connection arm carries none), every per-op histogram record and ledger
counter is a stripe-local RMW folded on read (the export byte-identical,
pinned exact to the ns), and a warm serve reads the clock **9 times, not
13**. Release microbench on the dev box (§4, scoping evidence): custody
fingerprint build **29.1 → 20.9 ns**, build-pair + match **63.2 →
44.0 ns**, the cold leg's `fill_incarnation` resolve **142.9 → 60.9 ns**
and `zc_device_resolve` **129.2 → 76.3 ns**, one phase record from 8
threads **1.62 → 0.45 ms per 32k records (−72 %)** — the contention term
the field's 32 lanes pay. R #8 (kvmap partial windows) fills one arena
instead of O(4096) `String`s and its head parse renders nothing; R #9's
residency check clones no `Bytes`. **No knob was added**: every lever is
an economy with the same verdict as before. The R-1 residual (§6) is
adjudicated MOOT for the READ hot path — since R-2 a kernel READ never
traverses `InboundQueue::pop`, whose ticked recv was the arm the residual
named.

---

## 1. Instruments, venue, tier

* **The alloc law** — `tests/kernel_op_economy_tests.rs`: a counting
  global allocator around a quiesced window of 2,000 handler calls, per-op
  bound asserted; `SQZ_ALLOC_TRACE=1` flips it into the deduped
  alloc-site profiler. Fixture: 512 KiB blocks (a small file is genuinely
  striped), prefetch and ranged windows pinned off so no lane's
  allocations enter the window. Two READ rows: **warm** (`h.fs.read` of
  a resident 4 KiB sub-block — the handler + router ladder serve) and
  **cold zc** (new this campaign: the router primitive with an injected
  `ZcReadServe` whose fetch is an immediately-ready `Ok(len)` — the leg
  deposits nothing, so every op is cold; the `fuse_zc_serve_tests` venue).
* **Atomics and clock reads** are counted BY READING the two paths
  (handler `read_impl` → router `read_file_range_zero_copy_with_meta`
  single-block arm → the serving arm), listed per site in §2 — no
  `perf stat` proxy on the dev box.
* **The ns face** — `benches/read_path_bench.rs` group
  `read_handler_economy` (release, `taskset -c 4-11`, `--warm-up-time 1
  --measurement-time 3`, dev box). **Scoping evidence only**: the dev
  laptop heat-soaks across a row sequence (the 2026-09-07 venue ruling);
  the before/after binaries were built from the same worktree in a
  separate target dir and run back to back, both orders not run.
* **Contracts** — the two alloc budgets; `read_serve_phase_sharded_fold_
  is_exact` (audit suite: N threads' records fold to exact count / sum_ns
  / Σ buckets, phase names and JSON shape byte-identical);
  `canonical_decimal_guard_matches_the_render_check` (block_map unit).

## 2. The baseline ledger (dev tip `36d517f3`)

### 2.1 Warm ladder READ — 8.00 allocs/op (2,000 ops: 16,007 allocs)

| # | Site (trace) | Allocs/op | What |
|---|---|---|---|
| 1 | `ReadCustodyFp::build_sync` ×2 | 2 | `epochs: Vec<u64>` for a one-block window, before and after the router |
| 2 | `load_striped_block_keys` (serve arm) | 2 | `Vec<(u32, Option<String>)>` + the key `String` clone (+ a one-element sort) |
| 3 | `staged_extent_runs_in` → `active_block_ext_for_path` | 1 | heap `CompactString` key for the W2 existence probe (absent on every clean block) |
| 4 | `get_block_for_index_class` | 1 | `key: Option<String> = Some(k.to_string())` — the caller's resolved key copied |
| 5 | `current_block_binding` (the recheck) | 1 | the recheck's OWNED resolve, compared and dropped |
| 6 | `Arc::new(downloaded)` (`backing`) | 1 | the reply's keepalive `Arc` — vestigial since E-IL1 (the body refcounts its bytes) |

### 2.2 Cold FUSE-zc leg — 11.00 allocs/op (2,000 ops: 22,008 allocs)

| # | Site | Allocs/op |
|---|---|---|
| 1 | `staged_extent_runs_in` heap key | 1 |
| 2 | `load_striped_block_keys` (`Vec` + clone) | 2 |
| 3 | `zc_device_resolve` → `parse_block_key_parts` (`be_id: String`) | 1 |
| 4 | `key_incarnation_tracked` → `allocator_for_key` (`clean_block_key` `String` + `parse_block_key` `String`) | 2 |
| 5 | `fill_incarnation` → `allocator_for_key` | 2 |
| 6 | `fill_incarnation_still` → `allocator_for_key` | 2 |
| 7 | the injected fetch's `Box::pin` (structural to the venue) | 1 |

Not in the fixture (no armed transport in-process) but on the field path
by reading: the handler's `ZcReadServe::new(Box::new(closure))` — **one
`Box` per READ, warm or cold** (minted before the warm/cold decision) —
and, per cold fetch, the closure's `Arc<FuseConnection>` clone +
`Box::pin` of the async block.

### 2.3 Contended atomics per warm serve (by reading)

| Family | RMWs | Lines |
|---|---|---|
| `read_serve_phase_ns` records: prelude, key_resolve, classify_probe, slice_out, binding_check, post_validate, total — 3 words each (bucket + count + sum_ns) | 21 | one hot bucket word + count + sum per phase, shared by every lane |
| ledger counters on the hot arm: `read_copy_dest_bytes`, `read_copy_warm_serve_bytes`, `read_copy_hot_serve_bytes`, `cache_hits`, `hot_block_hits` | 5 | one line each, shared |
| **Total process-global RMWs on shared lines** | **26** | (+ `nt_read_serve_bytes` on ≥ 256 KiB shapes) |

Cold zc leg: 16 histogram/ledger RMWs (prelude, key_resolve,
block_fetch, post_validate, total records + `read_zc_serve_bytes`) **plus
10 refcount RMWs on shared lines** — the `Arc<FuseConnection>` clone/drop
per READ and per fetch (4) and the three `Arc<BlockAllocator>`
clone/drop pairs inside `allocator_for_key` (6). Already stripe-local
before this campaign: `fuse_ops` (`ShardedAtomic`), the `ServeStamp`
shards, the fuse3 transport tables (R-4).

### 2.4 Clock reads per warm serve (by reading)

`serve_t0`, prelude-end, `key_t0`, key_resolve-end, `probe_t0`,
classify_probe-end, `slice_t0`, slice_out-end, `bind_t0`,
binding_check-end, `router_done_at`, post_validate-end, total-end =
**13** (`Instant::now()` ≈ 20–25 ns each through the vDSO). Cold zc leg:
10 — and the leg recorded NO `classify_probe`, so its probe span sat
unattributed inside `total`.

## 3. The levers (per item) — before → after

| Item | Lever | Before → after |
|---|---|---|
| R #6 alloc — custody fp | `CustodyEpochs`: the covered window's epochs INLINE (≤ 4 words; a kernel READ spans one block, two at a boundary), heap only past that | 2 → 0 allocs/op |
| R #6 alloc — key resolve | the single-block serve arm resolves through `block_key_in` by reference; `load_striped_block_keys` is reached only for the partial-store / anomalous shapes it alone answers | 2 → 0 |
| R #6 alloc — W2 probe | `staged_extent_runs_in` runs the stack-key existence gate first (`has_staged_extent_runs`), heap key only when a record exists | 1 → 0 |
| R #6 alloc — fetch ladder | the first attempt BORROWS the resolved key (`Cow`), the recheck is `block_binding_is` in place; only a LOSS materializes the current binding (it is the next key) | 2 → 0 on the hit path |
| R #6 alloc — reply backing | the router's read range primitive returns `Bytes` alone; the `Option<Arc<dyn Any>>` kept a transmuted `'static` slice alive until E-IL1 made every arm's body refcounted/copied — it kept nothing since. `ReplyData.backing` (the fork's vehicle) stays `None`; the copy-ledger suite's aliasing proof reads the tier's block directly | 1 → 0 |
| R #6 alloc — zc handle | `ZcReadServe<'c>::for_connection(&conn, slot)` borrows the guard the handler holds anyway; `fetch` awaits `conn.zc_device_fetch` unboxed; the boxed arm stays for injected primitives | 1 Box/READ + (`Box::pin` + `Arc` clone)/fetch → 0 (structural — not fixture-measurable) |
| R #7 re-parse | `clean_block_key_ref` (the cleaned key is always a sub-slice), `BlockKeyRef<'a>` + `split_block_key_ref`, `with_allocator_for_key` (borrowed allocator, no `Arc` clone) under `zc_device_resolve`, `read_block_with_dest`, `read_block_range`, `free_block`, `increment_refcount`, `pin_block_validated`, `publish_block`, the three incarnation probes | 7 allocs + 6 refcount RMWs per cold op → 0 |
| R #6 atomics — histograms | `ShardedLatencyHistogram` for `read_serve_phase_ns` / `read_fill_phase_ns`: per-thread stripes (padded to 256 B), fold on read | 21 shared RMWs → 21 stripe-local |
| R #6 atomics — counters | 20 per-op READ counters → `ShardedAtomic` (same `fetch_add`/`load` face) | 5 shared RMWs (warm) → 0 |
| R #6 clock | `read_serve_phase_record` returns its `now`; abutting phases chain (`key_resolve → classify_probe → slice_out → binding_check`; `post_validate` + `total` share one read via `read_serve_phase_record_at`; the cold ladder's probe end is the fetch start; the zc leg now records `classify_probe` off the same read) | warm 13 → 9; cold zc 10 → 8 (and one more phase attributed) |
| R #8 kvmap | `KvmapWindowSpan` keeps its bindings in ONE arena (`(index, start, len)` list + one `String`) filled by `map_entry_block_key_at_into` (no backend-id `String`, no body `String`, no stamped re-`format!`); `parse_canonical_u32/u64` scan for canonical form instead of rendering the value back | O(4096 × 3) allocs per window fill → O(log); 2 `String`s per head parse → 0 |
| R #9 residency | `touch_no_promote` on both RAM tiers: the same clock-bit refresh as `get_no_promote`, no `Bytes` clone | 2 refcount RMW pairs per pipelined block → 0; short-circuit order unchanged |

### 3.1 The alloc law, after

| Row | Before | After | Budget (was) |
|---|---|---|---|
| warm ladder READ (`warm_kernel_read_prelude_allocation_budget`) | 8.00 | **0.00** (0 / 2,000) | **1.00** (9.00) |
| cold zc leg (`cold_kernel_read_zc_leg_allocation_budget`, new) | 11.00 | **1.00** — the injected fetch's `Box::pin` alone | **2.00** (—) |
| warm WRITE (unchanged, the W-6 campaign's) | 16.1 | 16.1 | 30.00 |

Both READ budgets are red on the pre-R-5 tree (8× and 5.5× over).

## 4. The ns face — `read_handler_economy` (release, dev box, scoping)

| Row | Before | After | Δ |
|---|---|---|---|
| `custody_fp_build_1block` | 29.14 ns | 20.88 ns | −28 % |
| `custody_fp_pair_matches` | 63.23 ns | 43.95 ns | −30 % |
| `key_resolve_fill_incarnation` | 142.9 ns | 60.9 ns | −57 % |
| `key_resolve_zc_device` | 129.2 ns | 76.3 ns | −41 % |
| `serve_phase_record_1thread` | 57.3 ns | 54.1 ns | −5 % (uncontended: TLS stripe lookup ≈ the saved shared-line miss) |
| `serve_phase_record_8threads` (8 × 4,096 records) | 1.617 ms | 0.448 ms | **−72 %** — 49 → 14 ns per record aggregate; the contention term the field's 32 lanes pay per record |

Arithmetic on the ledger, per cold zc op at the field posture: the seven
key-parse allocations (≈ 7 × 25 ns jemalloc alloc+free), the ten
refcount RMWs on shared lines, the 16 shared histogram/ledger RMWs and
two clock reads sum to a few hundred ns of daemon CPU per op — at 556k
IOPS ≈ 10–20 % of one core. **This is arithmetic, not a measured field
number**; the parent's `rr_4k` CPU/op row on squeeze-test adjudicates it.

## 5. What did NOT change

* No verdict moved: every recheck compares what it compared; a ladder
  LOSS still resolves and logs the current binding; the zc leg's
  incarnation ladder is unchanged; `clean_block_key_ref` yields the same
  bytes `clean_block_key` did (a prefix/suffix slice by construction).
* No knob: economies only. The one instrument semantics change is
  ADDITIVE — the zc leg now records `classify_probe` (it never did), so
  `total ≈ Σ phases` holds on that leg too.
* The stats JSON: every family's keys, phase names, bucket labels and
  the `count`/`sum_ns`/`mean_ns` words are byte-identical; the sharded
  fold is exact (pinned).
* Public API kept: `parse_block_key`, `parse_block_key_parts`,
  `clean_block_key` (owned) stay for the census/fsck/mover callers;
  `ZcReadServe::new(Box<ZcFetchFn>)` stays for injected primitives (its
  type gained a lifetime parameter — `ZcReadServe<'static>` for the boxed
  arm).

## 6. The R-1 residual — adjudicated moot for READ

R-1 (`.benchmarks/2026-09-02-r1-device-read-executor.md`) named two
residuals: timer-thread wakeup coalescing (≈ 1.1–1.5 % CPU) and the
transport's `InboundQueue::pop` ticked recv arming 0.35–0.45 timers per
kernel READ on the fork's `sqz_time` registry. **Since R-2 (2026-09-03,
`850ce8b0`) a kernel READ on an armed session never reaches
`InboundQueue::pop`**: the reap thread serves it inline (R-2's sync
probe), fuses it onto its own lane (R-3), or hands the minted handler to
the `fuse3-tpc` lane homed on the queue's CPU — the inbound queue and the
session dispatch task are skipped by construction (`fuse_over_uring.rs`,
the READ dispatch law). The ticked recv remains the venue for non-READ
opcodes and unfused WRITE deliveries; making it timer-less would change
the sqz-sync liveness posture ("a lost wake costs one tick, never a
wedge") and belongs to the transport-economy PR, not a READ economy. The
op-economy fixture's own trace shows 4 ticked arms per 32 ops from the
`Notify` parks the fixture's background tasks take — not per-op READ
work.

## 7. Not claimed / owed

* **The field row** — `rr_4k` kern 24×8 on squeeze-test, A-B-B-A, same
  `release` profile both legs: IOPS, `daemon_cpu_ns` per op,
  `daemon_cpu_ns_by_class[fuse3-tpc|fuse3-ur]`, `read_serve_phase_ns`
  sums — the parent runs it; nothing here states a field number.
* The dev-box microbench rows are single-order, heat-soak-exposed
  scoping evidence (§1); the criterion baseline (`reference.json`)
  refresh rides the landing's `tests/run_bench_baseline.sh save` on the
  quiet-core venue, not this session.
* The WRITE handler (W #7, ~16 allocs/op on this fixture) is W-6's.
* The handler's `note_page_instantiation`, `ServeStamp`, moka and
  `RwLock` internals were not re-priced (already stripe-local or
  uncontended on the read path by construction).
* `read_fill_phase_ns` was sharded alongside `read_serve_phase_ns`
  (same table type) but its per-op count is a fill-path term, not a
  warm-serve term — no before/after row.
