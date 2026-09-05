# 2026-09-05 — W-2 `perf/write-stream-guard`: the order-1 guard on the fresh/append stream

**Program:** [`docs/design-e2e-perf-audit.md`](../docs/design-e2e-perf-audit.md)
ladder row 10 / §3.3 row 11 / Appendix C fat-board item **Write #6**:
"exclusive inode guard on fresh/append streams — the order-1 write guard
serializes a stream's blocks even when block locks suffice (P1-8); narrow
to meta-prep only on the fresh/append shape". Term column on the board:
**"—"** (no measured term).
**Prior program:** [`docs/design-write-inode-convoy.md`](../docs/design-write-inode-convoy.md)
(the Shared-mode write admission, 2026-08-10/11) — §4.4 row 15 left the
extend/hole shapes out of the Shared class "(c) — out of class in v1
(KD-3); the widening campaign owes the (a) proof". This is that campaign.
**Branch / SHAs:** `perf/write-stream-guard` off `dev` `27a396e1` —
`022f3647` (instrument), `72ff4582` (red contracts + seam), `15d5e7b7`
(lever), + the docs/rows commit.
**Venue (in-process rows):** the shared 26-CPU dev box (`7.1.8-cachyos-lto`),
FOUR sibling campaigns running concurrently — walls swing ±30 % run to
run and are quoted as such; the exact `sum_ns`/`count` ledgers are the
instrument. Release profile (`thin` LTO, the dev/A-B profile), BS 64 KiB
(the suites' downscale), file-backed `NvmeBlockDev` on btrfs `/tmp`,
`--interception`-free in-process `SqueezefsFilesystem` driven through
`Filesystem::write`. **Tier: measured-real, in-process only.** No field
row was run here (the box is shared; field rows are the parent's — §
"Field rows — OWED").

---

## 1. Finding — the premise, put to an instrument

The board's claim had two halves and the code could only be read one
way for each:

1. **The HOLD half** ("serializes a stream's blocks … when block locks
   suffice", P1-8): every striped WRITE — the fresh/append stream past
   its first block — classifies `MetaPrepOnly` (or `Shared`) and drops
   the order-1 guard at the KD-1 pre-dispatch point (`src/fuse_client.rs`,
   the `drop(guard)` before `write_file_staged`); the data path runs
   under `BLOCK_FLUSH_LOCKS` (3) + `INODE_META_LOCKS` (3.5) only. The
   only `EntireOp` a fresh file pays is its FIRST write's layout
   promotion — the f46 field row shows exactly that
   (`write_lock_scope_entire` = 24 on 24 files,
   `.benchmarks/2026-09-02-f46-kvmap-stream-publish.md`).
2. **The MODE half**: that pre-dispatch hold is taken **exclusive** for
   every extending write (the convoy design's KD-3 admitted only the
   fully-mapped within-EOF overwrite to the read guard), so a qd-N
   stream's N in-flight writes on ONE ino hand a single exclusive guard
   down a FIFO wake chain — one cross-lane wake per handoff — to
   serialize a snapshot that is RAM-only: a lease-cache hit, two cache
   peeks, the classifier, two counters.

Nothing metered (1). The convoy ledgers meter the WAIT per mode
(`write_lock_wait_{shared,exclusive}`) and the FINAL scope
(`write_lock_scope_*`), never how long the guard is held once taken —
so P1-8 on the stream shape was unfalsifiable in either direction.

## 2. Instrument — `write_lock_hold_{shared,metaprep,entire}` (`022f3647`)

Per-FINAL-scope hold histograms (acquisition → drop; exact `sum_ns` +
`count` per the A1 law). `HeldWriteGuard` became a struct carrying its
acquisition instant — the wait's own end clock read, so no extra read at
entry — and a histogram bound at classification; `Drop` records the
hold, so every exit after classification records truthfully and an
unclassified exit (the orphan-discard ack, a lease/fetch error) records
nothing. **Closure law: Σ hold counts ≡ Σ `write_lock_scope_*`, class by
class** (pinned). Cost: one `Instant::now()` per WRITE at the drop.
Exported on the stats inode; operations.md stats table.

The DMA is made observable with the existing checkout-stall seam
(`set_test_checkout_stall_ms` / `SQUEEZEFS_TEST_CHECKOUT_STALL_MS`):
every write parks `stall` inside its `BLOCK_FLUSH_LOCKS`-held window
(post-checkout, pre-merge — the data path proper). A hold that reads
stall-length = the order-1 guard held across block I/O.

## 3. Conviction

### 3.1 The HOLD half — ACQUITTED (the guard already drops before block I/O)

Contract rows (`tests/write_stream_guard_tests.rs`, GREEN on the
pre-lever tree `022f3647`, counted ×20 below):

| contract | shape | assertion | reading |
|---|---|---|---|
| `stream_write_hold_excludes_the_block_io_window` | one extending write, 300 ms block-window stall | hold < stall/4; wall ≥ stall; Σ hold ≡ Σ scope; never `EntireOp` | the hold excludes the data path |
| `concurrent_stream_writers_to_one_file_overlap_their_block_windows` | K = 8 extends into 8 fresh blocks of ONE ino, 200 ms stall | wall < 3 × stall (vs the serialized 8 × = 1,600 ms); Σ hold over the 8 < ONE stall; blocks byte-exact after a cold read | block locks suffice; the guard fences nothing on the data path |
| `n_fresh_files_qd_k_streams_are_parallel_and_byte_exact` | 4 files × qd-4 cursor-driven streams × 12 blocks, 100 ms stall | wall < 3 × the ideal pipeline (BLOCKS/K rounds); Σ hold over 48 writes < ONE stall; `sha256` of every file's cold read-back == its image; size exact | parallel across AND within files |

Release rows (the row printer `rows_in_process_release`, contract shape,
A-B-B-A across the posture in one process; run 1 of 3 — the quietest):

| contract row | posture | wall | K × stall | Σ hold (8 writes) | wait |
|---|---|---|---|---|---|
| k8_stall200_0 | `narrow=0` (pre-campaign) | **214.3 ms** | 1,600 ms | 22 µs | exclusive n=8 mean 9.8 µs |
| k8_stall200_1 | `narrow=1` | 203.6 ms | 1,600 ms | 16 µs | shared n=8 mean 337 ns |
| k8_stall200_2 | `narrow=1` | 205.1 ms | 1,600 ms | 14 µs | shared n=8 mean 349 ns |
| k8_stall200_3 | `narrow=0` | 210.1 ms | 1,600 ms | 14 µs | exclusive n=8 mean 209 ns |

Both postures: wall ≈ one stall, Σ hold ≈ 2–5 µs per write. **The
board's P1-8 premise is false for the stream shape on the pre-campaign
tree** — the hold was already meta-prep only. Runs 2–3 (box loaded by
the siblings): walls 209–216 ms, Σ hold 26–38 µs — same verdict.

### 3.2 The MODE half — the measured term

Stream rows (release, `stream_row`: `files` striped inos × `qd`
cursor-driven tasks, `blocks` × 4 sub-block segments each — the field's
1 MiB-into-4 MiB shape at BS/4; A-B-B-A per shape in one process; run 1
of 3, the quietest):

| row | posture | shape | writes | wall | wait shared (n, mean) | wait exclusive (n, mean) | hold shared (mean, max bucket) | hold metaprep (mean, max bucket) | hold entire |
|---|---|---|---|---|---|---|---|---|---|
| one_ino_qd16_0 | `narrow=0` | 1 × qd16, 512 blk × 4 | 2,048 | 173.3 ms | — | 2,048 / **3,430 ns** | — | 780 ns / ≤ 16 µs | 0 |
| one_ino_qd16_1 | `narrow=1` | " | 2,048 | 163.3 ms | 2,048 / **79 ns** | — | 499 ns / ≤ 8 µs | — | 0 |
| one_ino_qd16_2 | `narrow=1` | " | 2,048 | 169.0 ms | 2,048 / 78 ns | — | 503 ns / ≤ 16 µs | — | 0 |
| one_ino_qd16_3 | `narrow=0` | " | 2,048 | 163.5 ms | — | 2,048 / 3,284 ns | — | 730 ns / ≤ 16 µs | 0 |
| 24_inos_qd16_0 | `narrow=0` | 24 × qd16, 32 blk × 4 | 3,072 | 267.4 ms | — | 3,072 / **124,336 ns** | — | 1,721 ns / ≤ 1,024 µs | 0 |
| 24_inos_qd16_1 | `narrow=1` | " | 3,072 | 259.4 ms | 3,067 / **432 ns** | 5 / 360 ns | 985 ns / ≤ 16 µs | 2,304 ns (n=5) | 0 |
| 24_inos_qd16_2 | `narrow=1` | " | 3,072 | 244.3 ms | 3,071 / 390 ns | 1 / 541 ns | 1,002 ns / ≤ 512 µs | 2,325 ns (n=1) | 0 |
| 24_inos_qd16_3 | `narrow=0` | " | 3,072 | 238.8 ms | — | 3,072 / 60,358 ns | — | 1,155 ns / ≤ 1,024 µs | 0 |
| one_ino_qd64_0 | `narrow=0` | 1 × qd64, 512 blk × 4 | 2,048 | 252.9 ms | — | 2,048 / **30,815 ns** | — | 1,097 ns / ≤ 1,024 µs | 0 |
| one_ino_qd64_1 | `narrow=1` | " | 2,048 | 249.8 ms | 2,048 / **106 ns** | — | 702 ns / ≤ 512 µs | — | 0 |
| one_ino_qd64_2 | `narrow=1` | " | 2,048 | 220.7 ms | 2,048 / 94 ns | — | 619 ns / ≤ 512 µs | — | 0 |
| one_ino_qd64_3 | `narrow=0` | " | 2,048 | 222.2 ms | — | 2,048 / 13,424 ns | — | 911 ns / ≤ 1,024 µs | 0 |

(The 5 + 1 exclusive dispatches on the `narrow=1` 24-ino rows are the
first post-promotion write of a few files racing its own attr-cache
publish — a snapshot miss routing exclusive at admission, KD-2 working;
max-bucket columns in run 1 are process-cumulative — runs 2–3 used the
per-row delta and read ≤ 8–256 µs on the one-ino rows, ≤ 128–1,024 µs on
the 24-ino rows: scheduler preemption inside a µs hold on a box running
four campaigns, never stall-length.)

Readings:

* **Hold, both postures: 0.5–2.3 µs mean.** That IS the meta-prep
  (lease hit + two peeks + classify + two counters). The P1-8 slot is
  empty; the "narrow the hold" lever has nothing to narrow.
* **Exclusive wait, `narrow=0`: 3.3–4.4 µs/op at one ino qd16, 13–31 µs
  at qd64, 33–124 µs at 24 inos × qd16** (384 tasks on 16 workers — the
  oversubscription the field's 32 lanes vs 384 in-flight WRITEs also
  has). This is the FIFO wake chain: N writers per ino serialize through
  one exclusive guard, one cross-lane wake per handoff, to protect a
  RAM-only snapshot. **Shared wait, `narrow=1`: 78–903 ns/op** — the
  term is deleted (−97 % … −99.9 %), and the hold itself shortens
  (no `is_exclusive` orphan-probe arm, the Shared snapshot IS the
  resolution).
* **Walls: par within the shared box's noise** (run-to-run swings of
  ±30 % on identical rows dwarf any posture delta; runs 2–3 are in the
  artifact log). The in-process venue drives `Filesystem::write` from
  tokio tasks at ~12 k writes/s per row — the wait term is 0.2–10 % of
  the in-process per-op latency, never the wall's binder here. **No
  throughput claim is made from these rows.** Against the field's
  `w_fresh` (post-f46: 32.7 GiB/s, clat 11.3 ms at 24 × qd16 × 1 MiB —
  `.benchmarks/2026-09-02-f46-kvmap-stream-publish.md`) a 60–124 µs/op
  exclusive wait is 0.5–1.1 % of clat at equal depth: the honest
  expectation for the owed field row is **a small latency win or par**,
  not a headline — the field's `write_lock_wait_exclusive` mean on the
  pre-campaign leg is the number that decides it.

## 4. Mechanism (`15d5e7b7`)

The Shared class widens from the convoy design's v1 predicate
(striped ∧ fully mapped ∧ within the conservative EOF floor) to
**every cache-resident striped write** (striped ∧ `shared_size_floor`
resident — both RAM caches, KD-7's witness): the extending fresh/append
stream and within-EOF hole-fills join the mapped overwrite. ONE classifier
(`inode_write_lock_scope`, now carrying the `narrow` posture), the same
§4.2 acquire protocol (classify-from-cache → acquire-as-classified →
revalidate-under-guard → at most ONE upgrade), the same KD-1 drop point,
the same KD-6 admission record and postlude. Knob
`SQUEEZEFS_WRITE_GUARD_NARROW` (registered, default **on**; `0` = the v1
class verbatim — the same-binary A/B lever; inert under
`SQUEEZEFS_WRITE_SHARED=0`). Engagement: `write_lock_scope_shared` counts
the stream's writes; `write_lock_hold_shared` is their hold;
`write_guard_narrow_enabled` on the stats inode.

**Why the widening is sound — the §4.4 row-15 "(a) proof" the convoy
design left owed.** KD-3 kept extends and holes exclusive because
"a within-EOF hole-fill allocates and `merge_block_mappings` under
`INODE_META_LOCKS` — an inode-plane mutation" and XFS's overwrite-only
DIO refuses holes for the same reason. But in SqueezeFS all three of
those mutations — hole-fill allocation, `merge_block_mappings`, the size
publish (`update_metadata_cache_size` + `publish_attr`) — run in the
per-block future or the postlude, **after the KD-1 drop point, on BOTH
modes**, under (3) + (3.5). The exclusive `MetaPrepOnly` class already
ran them concurrently across a stream's siblings (writer A's data path
overlaps writer B's meta-prep and data path) — which is precisely why the
generic/551 fix re-derives existing-bytes LIVE under the block guard and
why `write_through_coverage_tests` pins order-blind coverage. XFS's reason
does not transfer: its i_size and extent-tree mutations sit under
`i_rwsem`; ours never sat under the inode guard. What the exclusive
meta-prep serialized on the extend shape was the snapshot read itself:
`acquire_write_lease_for_span` (a lease-cache hit; a miss is serialized
by `lease_locks`, order 2, double-checked — convoy §4.4 row 1), two cache
peeks, the classifier, two counters. No RMW. Two extend writers resolving
their `old_size` concurrently under read guards see exactly what they saw
serially (neither publishes until after its drop), and a stale-low
`old_size` is the designed world on both modes (the max-based size
publish absorbs it; the block guard's live re-derivation owns the seed
class).

## 5. Invariants — what the exclusive meta-prep protected, and where each lives now

| invariant | held by (unchanged) | pinned |
|---|---|---|
| kernel-split out-of-order O_DIRECT segments (`FOPEN_PARALLEL_DIRECT_WRITES` on) | `BLOCK_FLUSH_LOCKS` (3) + the coverage-union `record_write` — never the inode guard | `tests/write_through_coverage_tests.rs` (green) |
| coverage-union completion transition / write-through trigger | (3), inside the per-block future | same + `concurrent_stream_writers_to_one_file_overlap_their_block_windows` |
| supersession / CoW `§5.1 fence(SeqCst)` protocol | (3) + the fence — **not touched** (the classifier never reaches the data path) | `tests/write_supersession_tests.rs`, loom models (unchanged) |
| truncate / fallocate / `copy_file_range` vs a write | those take EXCLUSIVE and are excluded by every Shared holder for the hold; after the drop the world is today's MetaPrepOnly world (`drop_active_block_overlays_beyond` + `truncate_layout` + block locks; KD-6's growth-only size claim) | `tests/posix_semantics_tests.rs`, `tests/attr_publish_tests.rs`, `truncate_between_probe_and_guard_upgrades_exactly_once` (v1 leg) |
| §5.4 lease-severance boundary | the sever/merge sites are in the data path — untouched | `tests/transport_lease_overlong_tests.rs` (self-skips unprivileged) |
| KD-2 any-doubt-exclusive / KD-7 lifecycle order | a snapshot miss (either cache) routes exclusive AT ADMISSION; the orphan probe stays exclusive-only and needs a meta miss, which Shared cannot have | `cold_cache_extend_still_routes_exclusive` |
| KD-2's ONE upgrade under the widened class | revalidation under the read guard fails on snapshot EVICTION (a truncate no longer moves an extend's verdict) → drop, exclusive once, the fetch-capable path | `evicted_snapshot_between_probe_and_guard_upgrades_exactly_once` — deterministic via the new `SqzRwLock::waiters` observable (the writer is seen PARKED before the eviction lands; no sleep-as-sync) |
| lease mint under N concurrent first-touch Shared writers | `lease_locks` (order 2) double-check | convoy §4.4 row 1 (unchanged) |
| the read guard is compatible with readers, exclusive with mutators | `SqzRwLock` semantics | `extending_write_completes_under_a_held_read_guard` (the mode witness) |

No new lock class, no order change: (1) → (2) → (3) → (3.5) → (4) as
before; only the MODE at layer (1) for a wider class, and the §4.2
must-not (a Shared holder never re-enters `active_inode_locks` on the
same ino) still holds — the Shared arm's entire hold is the RAM snapshot.
AGENTS.md's lock-order paragraph is unchanged (P1-8's wording — "striped
data path = meta-prep only under write lock" — describes the must-not,
which stands).

## 6. Gate (this side — targeted, four sibling campaigns share the box)

Recorded verbatim in the campaign report: `cargo fmt --check`; clippy
both configs (`--all-targets --all-features` and the shipped default
config) `-D warnings`; suites `write_stream_guard_tests` (×20 counted),
`write_lock_scope_tests`, `write_shared_scope_tests`,
`write_through_coverage_tests`, `posix_semantics_tests`,
`overlay_length_floor_tests`, `attr_publish_tests`,
`rebind_starvation_tests`, `transport_lease_overlong_tests` +
the zc write suites (self-skip unprivileged), `env_knob_convention_tests`,
`no_tokio_convention_tests`. `task check`, root rigs, fstests: NOT run
here (the parent's batch gate). Loom: not re-run — `sqz_sync_core.rs`
(the modeled core) is untouched; `SqzRwLock::waiters` is a pass-through
on the shipped face.

## 7. Field rows — OWED (the parent's)

Same binary, `w_fresh` (`/scratch/tmp/fio_jobs/write_BW.job`: 24 × 8 GiB,
sequential 1 MiB libaio `direct=1`, iodepth 16, 30 s + 10 s ramp; the
sustained-60 s form beside it) on the **tcp devsub** (root), **A-B-B-A**
via `SQUEEZEFS_WRITE_GUARD_NARROW=1` (A) / `=0` (B), fresh reset per leg,
post-f46 binary:

| leg | GiB/s | clat mean / p99 | `write_lock_wait_exclusive` (n, mean, ≥ 64 µs population) | `write_lock_wait_shared` (n, mean) | `write_lock_hold_shared` / `_metaprep` (n, mean, max bucket) | `write_lock_scope_{shared,metaprep,entire}` | `write_pipeline_phase_ns.lock_wait` (n, mean) | `fuse_op_phase_ns` write `entry_to_backend` (mean) |
|---|---|---|---|---|---|---|---|---|
| A `narrow=1` | | | | | | expect `shared` ≈ writes − 24, `entire` = 24 | | |
| B `narrow=0` | | | | | | expect `metaprep` ≈ writes − 24, `entire` = 24 | | |
| B | | | | | | | | |
| A | | | | | | | | |

Validity: the scope columns must account for the row's writes (the f46
`entire = 24` law); Σ hold ≡ Σ scope; `fuse_op_watchdog_overdue` = 0,
`invariant_tripwires` = 0, `meta_kv_block_refs_drift` = 0;
`write_lock_scope_shared_upgrades` ≈ 0 (nonzero = eviction/truncate
racing the stream). Verdict rule (§0 landing law): the knob's default
stays `on` if A ≥ B within noise on both brackets with the exclusive-wait
population gone; a beyond-noise loss on either bracket flips the default
to `0` and the note records why (the write ledger's own row 16 — the
il-vs-kernel parity verdict — rides the same row).

## 8. Boarded (not done here)

1. The stream's remaining per-inode serialization is now the lease-cache
   read + the two cache peeks under a READ guard — i.e. nothing is
   serialized; the next write-plane term on this shape is W #7 (per-WRITE
   allocations, the op-economy campaign) and the pipeline admission wait
   (`write_pipeline_phase_ns.admit_wait`), not the inode guard.
2. `write_lock_wait_exclusive`'s remaining population on a stream row is
   the fresh file's promotion write (one per file) and any snapshot-miss
   exclusive dispatch (`cold_cache_extend_still_routes_exclusive`'s
   class); a field row showing more than ≈ 1 per file names an attr/meta
   cache eviction under load.
3. The convoy design doc's status header still reads "DESIGN — awaiting
   sign-off" from 2026-08-10 though PRs 1–3 landed; not this campaign's
   to rewrite — only KD-3 and §4.4 row 15 carry the dated W-2 addenda.
