# 2026-09-08 — W-6: the WRITE handler's per-op economy (allocs, striped words, the zc slot vehicle, the ranged gap seed)

**Branch** `perf/write-handler-economy` (worktree off dev `36d517f3`). RED
`33b977a1` → lever A `1eeacaac` (alloc diet) → lever C `40f99e6d` (ranged
gap seed) → lever B `ea9b26b5` (striped per-op words) → lever D `370e981f`
(zc slot connection vehicle) → benches + docs (this note's commit).
Campaign: `docs/design-e2e-perf-audit.md` §3.4 Tier 3 **Write #7** ("per-
WRITE 4 Box + 4 Arc slot closures, per-block `String`s, ~30 atomics") and
**Write #10** ("gap seeding re-reads the whole old block"), §5.3 ladder
**row 17**, Appendix C rows 7 and 10. Contracts:
`tests/kernel_op_economy_tests.rs` (the WRITE budget, 4/op) and
`tests/overlay_gap_seed_ranged_tests.rs` (the seed-bytes law). Target
release: **1.2.1**.

Status: **LANDED in-process (§4); the field `rw_4k` CPU/op row on
squeeze-test is the parent's (§6).** Every number below is from this
dev box unless labeled; the box was carrying sibling `rustc` builds
(load 16–20 on 32 threads) throughout, which is why §4.3's end-to-end
rows are reported as brackets with the noise stated, not as a verdict.

## 1. The baseline ledger (measured first)

### 1.1 Allocations per warm kernel WRITE (the op-economy suite)

Shape: 4 KiB overwrite at a fixed offset of an already-striped 512 KiB
block — the W1 sole-owner patch, the random-small-write population.
Dev profile, 1,000 ops, `tests/kernel_op_economy_tests.rs`
`warm_kernel_write_prelude_allocation_budget`; sites from
`SQZ_ALLOC_TRACE=1` (32-op window, deduped 4-frame table).

The shipped budget was **30.00/op** (PERF-12's line); the shipped tree read
**16.14/op** with the old harness. Two of those were the HARNESS's — a
per-op `Bytes::copy_from_slice` payload mint (its `Vec`) whose first
clone inside the handler promoted the `Vec`-backed `Bytes` to its shared
form (a `Shared` box); the transport lease (`Bytes::from_owner`) never
pays either. The harness now mints the payload once and clones it per
op (a refcount bump — the lease's shape), and the pre-W-6 tree reads
**14.60/op**:

| # | Site (pre-W-6) | allocs/op | Disposition |
|---|---|---|---|
| 1 | `write_phase_begin` → `scc::HashMap::insert_sync` → `get_or_create_array` → `BucketArray::alloc` | **3** | the stage-1b write-phase census map was `scc::HashMap::new()` (minimum capacity 0): `try_resize` DROPS the bucket array when the sampled population is 0, so a mount whose writes do not overlap re-allocated the array (three allocations) on every write's unit key. **Lever A**: `with_capacity(2 × transport_inflight_ceiling)` — never shrunk below |
| 2 | `keys::active_block(ino, b)` (`FsKey`) + `.to_string()` | 2 | the per-block `cache_key: String` → `active_block_stack` (the `StackKey` the op-economy campaign ships); slow arms that need an owned key (`park_overlay_entry`, the staged-sibling handoffs) mint it there |
| 3 | `keys::inode_path(ino)` | 1 | the per-block `file_path: String` → `inode_path_stack` |
| 4 | `Vec<future>::push` | 1 | the single-block shape's one future → awaited inline; only a multi-block span builds the `Vec` |
| 5 | `try_join_all` → `Vec<TryMaybeDone>::with_capacity` | 1 | same |
| 6 | `try_sole_owner_patch`: `keys::active_block_ext` | 1 | → `active_block_ext_stack` (the overlay-store screen's twin probe too) |
| 7 | `try_sole_owner_patch`: `bm.get(&b).cloned()` (`String`) | 1 | the mapping is BORROWED from the map's `Arc` for the patch's life |
| 8 | `try_sole_owner_patch`: `parse_block_key` (owned `be_id`) | 1 | → `split_block_key` (the borrow form) |
| 9 | `PooledBuf::into_bytes` → `Bytes::from_owner` box | 1 | **remains** — the DMA payload's owner box (device layer) |
| 10 | `sqz_channel::oneshot::channel` (`Arc<Shared>`) | 1 | **remains** — the uring worker's completion oneshot |
| 11 | `nvme_dev::worker_thread_loop` per-request record | 1 | **remains** — the worker's request bookkeeping |
| — | background (times-echo drain, KV batch pipeline, timer boxes) | ≈ 0.6 | not per-op; visible as the `[4]`/`[5]`/`[6]` rows of a 32-op window |

### 1.2 Shared-line RMWs per WRITE (static census of the W1 path)

Counted from the handler → `write_file_staged` → `try_sole_owner_patch`
→ `write_block` code (rig off, buffered write). The **METRICS-family
words on process-global lines** every handler lane touches per write:

| Word | RMWs/op | Class |
|---|---|---|
| `write_lock_wait` + `write_lock_wait_{shared,exclusive}` (`LatencyHistogram::record` = bucket + count + sum_ns) | 6 | shared line, tight distribution ⇒ ONE bucket word |
| `write_lock_hold_{shared,metaprep,entire}` (the `HeldWriteGuard` drop) | 3 | shared |
| `block_lock_wait` | 3 | shared |
| `writeback_queue_depth` | 1 | shared |
| `write_lock_candidate_*`, `write_lock_scope_*` | 2 | shared |
| `patch_writes`, `patch_write_bytes` | 2 | shared |
| `fuse_ops` | 1 | already `ShardedAtomic` (core-local) |
| **Σ shared-line METRICS RMWs** | **17** | **→ 0 after lever B** |

The other RMWs on the path are NOT METRICS words and are left as they
are, named: the ino's `SqzRwLock` acquire/release (2 — per-ino by
design), the block stripe `try_lock`/release (2) + the last-holder store,
the `open_inodes` dashmap shard lock (2, `mark_handle_dirty`), the
`page_cache_inos` and `last_write_end` scc updates (≈ 2 + 3), the
write-phase census map's insert/update/remove pairs (≈ 14 bucket-line
RMWs — the next candidate on this list; see §6), the allocator's
`begin_patch_sole_owner`/`publish_block` words (2, per block), the
`active_block_buffers` dashmap shard lock (2 × 2 — handler screen + patch
predicate 2), the `OP_PROFILE` registry claim/release (≈ 3, shard-homed
per thread), and the device channel's push + eventfd wake (≈ 2 + a
syscall). Clock reads (rig off): `Instant::now()` × 2 for the inode-lock
wait, × 2 for the block-lock wait, the hold's `elapsed()`, and one
`coarse_realtime_ns()` — **6 per write**, unchanged by W-6 (≈ 20 ns each
on the vDSO; not attacked).

### 1.3 The zc held-slot mint (sqz-kernel sessions only)

On a zc-armed session every WRITE whose payload the kernel HELD in the
sparse slot built `ZcWriteSlot::new_with_ack_early` from **four boxed
closures** (store / extract / retain / release), each capturing its own
`Arc<FuseConnection>` clone, plus the slot `Arc` — 5 allocations + 4
refcount RMWs per zc WRITE before any handler work — and the store and
extract closures `Box::pin`ned a future per call (one more allocation
per patch store / per extraction). Not measurable by the op-economy
harness (no zc session in-process); counted by construction.

### 1.4 Gap-seed device bytes (the overlay settle, §5.8)

A partial overwrite record's settle sourced its uncovered ranges by ONE
whole-image read of the old binding: `read_nvme_block_old_image` →
`device_block_window()` = the block size on passthrough. The family's
byte face `overlay_gap_seed_old_bytes` counts SEEDED bytes, so the
amplification never showed: on the shipped 4 MiB block a record with one
64 KiB hole read **4 MiB to seed 64 KiB (64×)**; the contract fixture
(16 KiB blocks, one 4 KiB gap) reads **16 KiB to seed 4 KiB (4×)**.

## 2. The levers

| Lever | Commit | What | Knob / gauge |
|---|---|---|---|
| **A — the alloc diet** | `1eeacaac` | census-map capacity floor (`stripe_locks::transport_inflight_ceiling` × 2), stack keys for `active_block:` / `inode_{ino}` / `active_block_ext:`, the single-block future awaited inline, the patch prelude borrow-only (`split_block_key`, the map `Arc` held) | none — structural; the budget `tests/kernel_op_economy_tests.rs` (4/op) |
| **B — striped per-op words** | `ea9b26b5` | `ShardedLatencyHistogram` / `ShardedQueueDepthHistogram` (per-thread stripes on `ShardedAtomic::stripe_index`, folded on read) for the five histograms + the depth sample; `ShardedAtomic` for the four counters | none — every stats key keeps its meaning (`to_json`/`count`/`sum_ns`/`p99_micros`/`reset` fold the stripes) |
| **C — the ranged gap seed** | `40f99e6d` | `DataRouter::read_nvme_block_old_image_range` (the read path's `read_block_range`, short-tolerant) per gap when passthrough ∧ undecorated ∧ Σ gaps < window; whole read otherwise | `SQUEEZEFS_GAP_SEED_RANGED` (registry; default on, `0` = whole read); `overlay_gap_seed_ranged_bytes` ⊆ `overlay_gap_seed_old_bytes`, `overlay_gap_seed_read_bytes` (device bytes read per seed) |
| **D — the zc slot's connection vehicle** | `370e981f` | `ZcSlotVehicle::Connection { conn, slot }` — one `Arc` clone, every verb a direct method call; the `Closures` arm keeps the injected-source constructors the suites drive | none — structural |

## 3. Contracts (red-first)

- `warm_kernel_write_prelude_allocation_budget` — budget **4.00/op**; RED
  on the pre-W-6 tree at 14.60, GREEN at 3.06 (`33b977a1` → `1eeacaac`).
  The READ budget (9.00, reads 8.00) is untouched.
- `tests/overlay_gap_seed_ranged_tests.rs` (4 tests, compile-red until
  the knob/seam/gauges existed): one trailing gap page reads exactly 4 KiB
  (not the 16 KiB block); two gaps around one covered page read Σ 12 KiB
  (< the window, ranged taken); `SQUEEZEFS_GAP_SEED_RANGED=0` reads the
  whole 16 KiB with the ranged face at 0 and the seeded bytes identical;
  an lz4 volume never reaches the overlay (0 seeds, 0 ranged bytes,
  old⊕new exact through the accumulation path). Every case checks the
  durable image byte-exact and `write_path_seed_read_bytes` Δ = 0.
- Unchanged and green: the coverage-union law
  (`write_through_coverage_tests`, kernel-split OOO WRITEs), the §5.8
  seeded-byte arithmetic in `overlay_overwrite_tests` /
  `device_overlay_tests` / `overlay_length_floor_tests`, the W1 ledger
  (`extent_patch_tests`, `patch_edge_rmw_reads` = 0), `fuse_zc_write_tests`
  (the Closures arm), `write_stream_guard_tests` (the striped hold
  histograms' `count`/`sum_ns`/buckets).

## 4. Measurements

### 4.1 Allocations per op (deterministic — the suite, dev profile, 1,000 ops)

| Tree | allocs/op | Δ |
|---|---|---|
| shipped (old harness form) | 16.14 | — |
| pre-W-6 (harness-corrected form) | **14.60** | the baseline |
| post-W-6 | **3.06** | **−79 %**; the three that remain are §1.1 rows 9–11 |

### 4.2 The per-op counter set (release, `benches/write_path_bench.rs` `write_handler_counters_{8,32}t`)

The exact 20-RMW set a W1 write bumps (five histograms × 3, the depth
sample, four counters), `THREADS × 20,000` ops per batch, `global_lines`
= the shipped types on shared lines, `striped` = the landed ones. This
box, under the sibling-build load stated above (the effect is an order
of magnitude, well outside it):

| Threads | global_lines (per batch / per op) | striped (per batch / per op) | Δ per op |
|---|---|---|---|
| 8 | 49.5 ms / **310 ns** | 7.63 ms / **48 ns** | **−85 %** (6.5×) |
| 32 | 82.6 ms / **129 ns** | 10.6 ms / **16.6 ns** | **−87 %** (7.8×) |

(The 8-thread per-op cost is HIGHER than the 32-thread one on the shipped
types because with 8 threads each op's line ping-pong is a longer
cross-core round trip per contender; the striped form is flat in thread
count by construction.)

### 4.3 The end-to-end in-process W1 write (release, `write_handler_e2e`) — brackets under load, no verdict

The `kernel_op_economy_tests` fixture through `Filesystem::write`:
`w1_patch_4k_serial` (one writer) and `w1_patch_4k_8_writers` (eight
inos, eight spawned writers on a 4-worker runtime, per-batch time). A
`dev`-tip worktree with the same bench file was the BEFORE binary; both
pinned `taskset -c 24-31`, 3 s warm-up / 10 s measurement, alternated
A-B-B-A. The box carried sibling `rustc` builds at load 16–20 the whole
time and the legs did not converge:

| Bracket / leg | serial µs/op | 8 writers µs/batch | load avg at start |
|---|---|---|---|
| 1 BEFORE | 96.5 | 537 | 15.8 |
| 1 AFTER | 53.8 | 280 | |
| 1 AFTER | 33.9 | 175 | |
| 1 BEFORE | 55.9 | 176 | 16.3 (end) |
| 2 AFTER | 94.0 | 466 | 24.3 |
| 2 BEFORE | 97.0 | 531 | |
| 2 BEFORE | 86.5 | 670 | |
| 2 AFTER | 64.3 | 484 | 38.7 (end) |
| AFTER, the first (least loaded) run, unpinned | 27.9 | 167 | ≈ 9 |

Same-tree legs differ by up to 1.7× within a bracket and the second
bracket ran at load 24–39 (siblings' clippy/rustc), so this row is **not
a verdict** — it is recorded so the next run has the command and the
shape (`taskset -c <quiet cores> cargo bench --bench write_path_bench --
write_handler_e2e`, a `dev`-tip worktree with the same bench file as the
BEFORE binary). What the legs do say: in bracket 1 every AFTER leg is at
or below every BEFORE leg, and in bracket 2 (heavier load) the pairs
overlap. The alloc-count and counter-set rows (§4.1, §4.2) are the
counted evidence this campaign lands on; the CPU/op verdict is the
parent's `rw_4k` row on squeeze-test.

### 4.4 Gap-seed bytes (deterministic — the contract suite)

| Shape (16 KiB block, 4 KiB pages) | control (`=0`) read bytes | ranged read bytes | seeded old bytes (both) |
|---|---|---|---|
| 3 pages covered, 1 gap page | 16,384 | **4,096** | 4,096 |
| 1 page covered, gaps of 1 + 2 pages | 16,384 | **12,288** (2 reads) | 12,288 |
| lz4 volume, any shape | 0 (no overlay) | 0 | 0 |

At the shipped 4 MiB block and the length floor (segments ≥ 512 KiB), a
record has ≤ 8 covered runs ⇒ ≤ 9 gaps, so the ranged arm issues at most
9 device reads per settle in place of one 4 MiB read; a record with one
64 KiB hole reads 64 KiB (64× fewer device bytes).

## 5. What is NOT claimed

- **No field number.** The `rw_4k` CPU/op row on squeeze-test (the §0
  landing law's field row) is the parent's; §4.3 is brackets under load.
- **The zc connection vehicle is unexercised in-process** — the sqz-kernel
  ring is its only live venue, exactly the coverage the four closures had.
  The `Closures` arm (every in-process slot suite) is unchanged and green.
- **Clock reads are unchanged** (6 per write, named in §1.2).
- **The write-phase census map's ≈ 14 bucket-line RMWs per write** are
  named, not attacked — the map is the exec1b wedge-autopsy instrument
  (a live-phase word per in-flight unit) and a slot-table redesign is a
  separate change.
- **The device layer's three allocations per write** (§1.1 rows 9–11) are
  named as follow-on work: `Bytes::from_owner` boxes its owner by
  construction, and the oneshot/worker-record pair is the NvmeBlockDev
  request protocol's.
- **The compose's gap read** (§5.6(1), the open-overlay READ serve of an
  overwrite record's gaps) still reads the whole old image — the same
  funnel could take the ranged form; not done here.

## 6. Owed

- The parent's field row: `rw_4k` kern on squeeze-test, A-B-B-A same
  profile, `daemon_cpu_ns`/op and `fuse3-tpc` class beside IOPS;
  `overlay_gap_seed_read_bytes ÷ overlay_gap_seed_old_bytes` on any
  overwrite row that settles partial records (≈ 1 engaged).
- `tests/run_bench_baseline.sh save` after the field row lands (the two
  new `write_path_bench` groups have no reference yet).

## 7. Suites run

Dev profile, `--test-threads=1`, this tree at `370e981f` + benches:
45 write-path suites, **508 passed / 0 failed / 2 ignored** —
`async_block_reclaim`, `audit_instruments`, `data_path_correctness`,
`derivation_sweep`, `device_overlay`, `env_knob_convention`,
`extent_overlay`, `extent_patch`, `extent_record_recovery`,
`f48_warm_read_overlay_gap`, `fuse_zc_write_fusion`, `fuse_zc_write`,
`hybrid_io`, `il_direct_write`, `ipc_op_economy`, `kernel_op_economy`,
`mmap_writeback_staleness`, `overlay_ack_early`, `overlay_core`,
`overlay_gap_seed_ranged`, `overlay_length_floor`, `overlay_overwrite`,
`overlay_settle_wait`, `pipeline_inval_tail_race`, `placement`,
`rand_write_amp`, `rewrite_shadow_supersede`,
`rewrite_shadow_supply_close`, `rewrite_shadow`, `rw5a_never_lossy`,
`volume_drain`, `write_commit_crash`, `write_commit_economy`,
`write_in_handler_economy`, `write_in_handler_phase`, `write_lock_scope`,
`write_pipeline_phase`, `write_pipeline`, `write_shared_scope`,
`write_stream_guard`, `write_supersession`, `write_through_coverage`,
`write_through`, `write_times_durability`, `write_visibility`. Plus
`cargo fmt --check`, clippy both feature configs, rustdoc, and the
`write_path_bench` smoke (`-- --test`) — results in the closing commit.
