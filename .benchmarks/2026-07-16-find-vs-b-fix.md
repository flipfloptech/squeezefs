# FIND-VS-B fix — staging shard plan vs same-key replace headroom (the pooled-seed 100/200 storm)

**Branch:** `fix/staged-rmw-pool-regression` off dev `15cc394`.
**Finding:** `.benchmarks/2026-07-15-vs-juicefs-scoreboard.md` §FIND-VS-B —
`staged_rmw_storm_is_pool_backed_recycling_and_byte_exact`
(`tests/staged_rmw_alloc_tests.rs`) failing deterministically,
`staged_rmw_pooled_seeds` Δ = 100 of 200, reproduced bit-identically on
pristine `abfde00`; sole failure in a full `--no-fail-fast` inventory.

## Reproduction — the trigger is CPU-affinity width, not commit lineage

On dev tip `15cc394` the test is **green ×25 under `taskset -c 0-15`**
(the house build rail every historical gate ran under) and **fails
deterministically with the exact 100/200 signature under the box's full
25-CPU mask** (the scoreboard agent's gate shell). The knife edge:

```
crate::cpu::process_parallelism() = 25  → default_shards = next_pow2(25) = 32
taskset -c 0-15                   = 16  → default_shards = 16
```

## Mechanism (probe tape, per-op)

The storm fixture: 2 MiB staged file, 128 MiB staging budget
(`TieredCache::new(..., Some("128MB"))`), 200 × 4 KiB scattered overwrites,
each RMW-rebuilding the whole staged image through
`read_staged_into` (ring-hit pooled seed) + `stage_write` (same-key
re-stage).

`NvmeShard::reserve_and_write` is **never-destroy + crash-safe replace**:
a same-key re-stage keeps the existing copy live (in the `live` extent set)
until the replacement is fully written — torn-write immunity for the sole
copy of dirty data. A shard therefore needs **2 × entry + framing** headroom
to re-stage an entry in place.

Pre-fix sizing (`src/cache/nvme.rs`, both cfg arms):
`shards = max(next_pow2(cores), 16)`, per-shard floor **one** 4 MiB entry
(`min_required = shards × 4 MiB`, capacity silently inflated to reach it).

| mask | shards | shard capacity | 2 MiB same-key replace |
|------|--------|----------------|------------------------|
| 16 CPUs | 16 | 128 MiB/16 = **8 MiB** | fits (needs ~4.2 MiB) — 200/200 pooled |
| 25 CPUs | 32 | 128 MiB/32 = **4 MiB** | **structurally refused** — the shard can never hold two 2 MiB copies |

Under refusal the write path takes its designed degraded leg
(`routing.rs` staged arm, `Err(StorageFull)` → durable **spill** with a
fresh `file_id`, superseded ring entry released), and the per-op tape locks
into a deterministic 2-cycle (probe, full mask):

```
op N   (even): read_staged_into HIT  (pooled +1) → stage_write REFUSED → spill:
               fresh uuid, block_map={0}, ring entry released
op N+1 (odd) : read_staged_into MISS (ring empty) → seed from the spilled
               durable block (promoted-mapping leg, pooled +0) → stage_write
               ADMITTED into the now-empty shard → ring entry back
```

Even ops count, odd ops don't → **exactly half**: pooled Δ = 100/200,
`bit-for-bit` stable, both feature sets — precisely the scoreboard
signature. (Byte-exactness held on every probe run — the miss leg seeds
from the spill block through the same pooled buffer; the defect is the
flood-shaped I/O economy, not correctness: each 4 KiB write paid a 2 MiB
durable block write + a 2 MiB device read-back + an allocator
allocate/free.)

## Bisect verdict

Not a recent regression — **latent from the machinery's birth** and
invisible only because every prior gate ran under the 16-CPU rail:

| commit | full-mask signature |
|--------|---------------------|
| `e4a8434` (pooled-seed fix + test born, `0ca1e9a`) | FAIL, Δ=1/200 (pre-stage-generation spill shape) |
| `c74ba8e` (stage generations), `9804335` (leg-5 v2), `e73d0df` (lazy-RMW item B) | FAIL, Δ=100/200 (the settled 2-cycle) |
| `2027f75`, `abfde00`, `15cc394` (dev tip) | FAIL, Δ=100/200 |
| any of the above under `taskset -c 0-15` | PASS 200/200 |

Hybrid-io `b8d7e4a..abfde00`, L1 `3335507`, FIND-M11-A `e3342eb`, and the
M-program PRs are all **exonerated** — none changed the outcome in either
geometry.

## Judgment: code defect (the test stays verbatim)

The test correctly pins the follow-up-C contract (pooled, recycling,
byte-exact staged RMW). The code's shard plan made that contract
**structurally unsatisfiable** on ≥ 17-core boxes at ≤ 256 MiB staging
budgets: shard capacity of exactly one block-size entry means *zero*
same-key replace headroom, so near-block-size staged files degrade to
per-op durable spill + device-RMW alternation — the 21 GB-churn flood's
I/O-shaped twin (~600× write amplification on a shape the staging tier
exists to absorb). Two adjacent latent defects in the same arm: the read
cache (same one-entry floor) could not admit a maximal 4 MiB block at all
on those geometries (`block > shard capacity` refusal), and the sizing
silently **inflated the operator's configured disk budget** to
`shards × 4 MiB`.

## Fix

`src/cache/nvme.rs` — one pure planner replaces both cfg-split sizing arms
(the `#[cfg(test)]` arm never applied to integration tests anyway — the
gate ran production sizing all along):

- `shard_plan(per_device_capacity, default_shards, max_entry_bytes)`:
  per-shard floor = **`2 × max_entry_bytes + 64 KiB` replace headroom**;
  the shard count halves (power-of-two preserved) until the floor holds,
  floor 1 shard. Pools < 10 MiB stay one whole-pool shard (the
  pre-existing tiny-pool rule, pinned by
  `tests/staging_budget_tests.rs::test_stage_write_shard_full_is_loud_never_lossy`).
- `max_entry_bytes` = `crate::routing::default_block_size()` (factored
  from `DataRouter::new`; env `SQUEEZEFS_DEFAULT_BLOCK_SIZE`, default
  4 MiB) — the staging plan and the router agree on the block-size class
  by construction.
- **Configured budgets are authoritative** — capacity inflation deleted.
- Construction-time loud `warn!` when even a whole device ring is below
  replace headroom (the per-op spill warn downstream is rate-limited and
  reads as transient pressure).
- Unit pins (`src/cache/nvme.rs::tests`): the replace-headroom invariant
  across box widths {16..512} × budgets {10 MiB..64 GiB} (machine-
  independent — simulates any core count), the exact FIND-VS-B geometry
  (128 MiB @ 32 shards → 8 shards × 16 MiB), large-budget fan-out
  preservation (500 MiB keeps 32), tiny-budget floors.

Post-fix geometry for the storm fixture on the 25-CPU box: 8 shards ×
16 MiB — the 2 MiB image re-stages in place forever; pooled Δ = 200/200.

## Acceptance

- `staged_rmw_storm_is_pool_backed_recycling_and_byte_exact`:
  **green ×10 full 25-CPU mask + green ×10 `taskset -c 0-15`** (post-fix
  binary, counts restarted after the final planner revision).
- Related suites (full mask, serial): `writeback_fencing_livelock_tests`
  5/5, `staging_budget_tests` 7/7, `staging_generation_tests` 6/6,
  `staging_shard_deadlock_tests` 2/2, `writeback_tests` 10/10,
  `mem_budget_tests` 16/16, planner pins 6/6.
- Full gate: `cargo clippy --all-targets --all-features -- -D warnings`
  clean; `cargo fmt --check` clean; `cargo doc --no-deps` clean;
  `cargo test --all-features -- --test-threads=1` (full 25-CPU mask,
  `ulimit -n 65536`) **0 failures project-wide** — the clean-gate
  invariant is restored on the geometry that broke it;
  `cargo bench --benches -- --test` smoke green.

## generic/074 fingerprint check

**Not shared.** The fstests harness mounts with `--disk-cache-size 500MB`
→ 500 MiB / 32 shards = 15.6 MiB shards ≥ the 8.06 MiB replace floor:
074's geometry is identical pre/post fix and never structurally refuses.
074's documented class (`.benchmarks/2026-07-15-find-m11a-fix.md` row 7b:
fstest.2 512 B-block zeros signature, standalone-harness/environment,
fail-fail A/B on dev with error-free daemon logs) is a different family —
not run, per the "don't chase if unrelated" charter.

## Adjacent gaps (noted, not chased)

- **Cross-geometry staging recovery**: shard count derives from
  `process_parallelism()` at construction, so a crash-dirty staging dir
  recovered by a mount with a different affinity mask (or, post-fix, a
  pre-fix→post-fix upgrade on a wide box) opens fewer/more
  `segment_j.bin` files than the writer created; entries in unopened
  segments are not indexed (and a shrunken per-shard `set_len` can
  truncate). Pre-existing class — geometry was already
  affinity-dependent — now one step more likely to surface across the
  upgrade boundary exactly once. Candidate charter: derive shard count
  from the segment files present at recovery, or persist the plan.
- The offline `status` path (`main.rs`) opens staging with hardcoded
  16 shards / 100 MB — already mismatched with mount geometry pre-fix;
  unchanged.
