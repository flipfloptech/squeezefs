# 2026-07-26 — Read-admission scan resistance: the beyond-budget random-read collapse

Branch `perf/read-admission-scan-resistance` (off dev `51c597a`, unmerged
pending review). Commits: red `4c63ce5` (tests + unwired scaffolding),
green `6da77e4` (governor + wiring + shard geometry), red `117cf52` /
green `8f9d024` (herd-proofing: token grant), red+green (declassification,
final commit on the branch). Contract suite:
`tests/read_admission_governor_tests.rs` (5 phases + herd + shard pins).

## 1. The finding (2026-07-25, two independent confirmations)

Cold-dominated random 4k O_DIRECT over a working set ≫ mem budget
collapsed the DEFAULT hybrid mount posture against `-o direct_device_true`
(rig ~20× in the original ipc-miss-path residual, 2.4× reproduced here at
kernel-path saturation; field 45–174k vs 280k with `rareq-sz` ~20 KB at
>90 % util and ~0.1 % hit rate). Mechanism: R1b second-touch ghost
escalation admits by fetching the WHOLE 4 MiB block; under
uniform-random-over-huge-set, admitted blocks evict before reuse, so
admission bandwidth is pure waste competing with foreground reads for
device queue slots. The 2026-07-15 hybrid directive is LAW — the default
stays hybrid; this program made it scan-resistant.

## 2. Substrate (labeled; same rig as `.benchmarks/2026-07-25-ipc-miss-path.md` §2)

23-CPU box, 109 GiB RAM, kernel 7.1.4-1-cachyos. Fabric-latency substrate
(still up from the prior campaign): configfs null_blk `sqzlat_oss0`
(36 GiB, memory-backed, `completion_nsec=235000`, `irqmode=2`, 8 squeues,
QD 128) via nvmet-loop → `/dev/nvme1n1` (data); `sqzlat_mds0` (3 GiB) →
`/dev/nvme2n1` (meta). Filesystem: cache-less format
(`sqmeta:///dev/nvme2n1 sqdata:///dev/nvme1n1`), 4 MiB blocks, 1 GiB mem
cache ⇒ **hot-block tier budget 128 MiB (32 blocks) — "the budget" for
every regime below** (cache-less ⇒ no NVMe read tier; the hot RAM tier is
the only landing zone). Mounts: `--daemon --allow-other`
(± `-o direct_device_true`), fresh mount per row (cold) unless marked
STEADY. **Instrument: elbencho 3.1-10** (`-r --rand -t 16 --iodepth 16
-b 4k --direct`, 20 s rows; fitting sets add `--randamount 4g`, 30 s);
**skew row: fio 3.42** (libaio, qd16 × 16 jobs, `zipf:1.1`, 25 s, files
round-robin); **seq row: elbencho** (`-r -t 16 -b 1M --direct`, 25 s).
Datasets: big 16 × 1.5 GiB (24 GiB ≫ budget), mid 16 × 8 MiB (128 MiB ≈
budget), small 16 × 4 MiB (64 MiB ≪ budget). Baseline binary: dev
`51c597a`. Box quiet for counted rows.

## 3. Baseline (dev `51c597a`) — the collapse reproduced

| Row | default (hybrid) | device-true | default/ddt |
|---|---|---|---|
| big 24 GiB ≫ budget, rand-4k | **131.3k** | **311.7k** | 0.42× |
| mid 128 MiB ≈ budget, rand-4k | **98.6k** | 273.5k | 0.36× |
| small 64 MiB ≪ budget, rand-4k | 296.0k | 264.7k | 1.12× |
| zipf 1.1 over 24 GiB (fio) | **162k** | 301k | 0.54× |
| seq 1 MiB cold (MiB/s) | 6,060 | — | — |

Baseline forensics (default, big row, 20 s): `ranged_read_ghost_escalations`
**+43,199** (≈ 2,160/s × 4 MiB ≈ **8.4 GiB/s of admission fetches** against
a device sustaining ~1.2 GiB/s of useful 4k), hot hit rate 0.5 %
(13.7k serves), evictions ≈ admissions (churn). Three compounding
mechanisms, each with its own fix below:

1. **No aggregate bound on escalations.** The per-key cooldown's intended
   ~192/s bound (6,144 keys / 32 s) ran at **7× (2,160/s)**: the 2¹⁶-slot
   direct-mapped cooldown table ping-pongs colliding keys' records
   (~576 colliding keys on this population re-escalate freely).
2. **≈budget regime, second face:** mid-row misses were steered to
   **whole-block fetches** (23.8k `read_tier_admissions` vs 41
   escalations): elbencho's random offsets over 8 MiB files coincidentally
   land 4-contiguous runs (76 spurious `read_streams_classified`/20 s),
   and one classified lane vetoes the ranged path FILE-WIDE for its 2 s
   freshness window. 241k ops parked as single-flight waiters behind
   4 MiB fetches — mid default (98.6k) was WORSE than big default.
3. **Hot-tier shard geometry:** 32 shards × 128 MiB budget = **one 4 MiB
   block per shard** — hash-colliding keys can never coexist. Even the
   FITTING 64 MiB set churned (6.1k evictions + whole-block refetches in
   20 s, hit rate stuck at 90 %, `hot_block_current_bytes` plateau at
   ~83 MiB of 128 MiB).

## 4. The policy (landed; docs/design-read-path.md §Observability updated)

**Admission governor** (`src/routing.rs::AdmissionGovernor`, owned by
`TieredCache`, single-word relaxed atomics, racy-tolerant — the
GhostTable concurrency class, no loom model required):

- **Waste signal** (the `prefetch_evicted_unconsumed` sibling for
  admissions): every hot-tier eviction of a ghost-admitted (protected)
  entry reports its payback — `served_bytes` credited by real reader
  serves (`get_serving`; probes/residency checks stay non-crediting).
  Shortfall vs the entry's length is windowed waste; never-served victims
  count `read_admission_evicted_unhit`.
- **Clamp**: engages when windowed waste ≥ half of windowed
  admitted-victim bytes (floor 2 blocks). **Eviction-side ratio** — an
  admission burst cannot dilute its own denominator and unclamp itself.
- **Fill budget while clamped**: an epoch **token grant** — minted once
  per 2 s epoch as `SQUEEZEFS_READ_ADMISSION_FILL_PCT` (default **5**) %
  of the foreground ranged device bytes the workload actually paid last
  epoch; spent by single-word `checked_sub` CAS (reservation, not
  check-then-add); leftovers die with the epoch. Two rejected designs are
  part of the record: plain check-then-add overshot **6×** under 256-deep
  herds, and a two-window spend comparison retained a roll-boundary
  snapshot race worth exactly **2×** — both reproduced on the rig, both
  now pinned by the herd phase.
- **Denials record no escalation cooldown**, so hot keys retry and win
  the trickle (skew convergence); heat recording continues (mirrors the
  §5.7 Red publish-pause semantics). Denied reads proceed as device-true
  ranged window reads — always correctness-safe.
- **Random-dominated declassification**: 16 consecutive reads matching no
  stream lane clear every lane's classification (`StreamLanes::
  foreign_since_match`) — a spurious 4-contiguous coincidence can no
  longer veto the ranged path file-wide for 2 s. Honest cost: a real
  small-request stream interleaved ≥ 16:1 with random on ONE file rides
  ranged reads until the flood subsides (re-classification = 4 requests).
- **Hot-tier shard geometry**: `LruCache::with_capacity_min_shard` keeps
  ≥ 16 MiB (4 default blocks) per shard (128 MiB budget: 32 → 8 shards).
- Mem-budget Red still pauses escalations outright (unchanged); fitting
  working sets produce no evictions ⇒ no waste ⇒ the governor never
  clamps and the pre-governor policy is byte-identical.

**New counters** (stats inode; regression thresholds in
docs/design-read-path.md §Observability): `read_admission_evicted_unhit`,
`read_admission_wasted_bytes`, `read_admission_governor_denials`,
`read_admission_governor_clamped` (gauge). **Env:**
`SQUEEZEFS_READ_ADMISSION_FILL_PCT` (0..=100, default 5). **Test seam:**
`routing::TEST_ADMISSION_EPOCH_MS`.

## 5. Fixed (final branch binary) — A/B

| Row | baseline default | **fixed default** | ddt (fixed binary) | fixed/ddt |
|---|---|---|---|---|
| big 24 GiB ≫ budget (medians of 3: 306.3/297.8/290.6) | 131.3k | **297.8k (+127 %)** | 303.0k | **0.98×** |
| big STEADY (2nd 20 s, same mount) | — | **296.4k** | — | 0.98× |
| mid 128 MiB ≈ budget | 98.6k | **563.9k (+472 %)** | 280.9k | **2.01×** |
| small 64 MiB ≪ budget | 296.0k | **604.2k (+104 %)** | 257.5k | **2.35×** |
| zipf 1.1 over 24 GiB (fio) | 162k | **295k (+82 %)** | 301k (baseline) | 0.98× |
| seq 1 MiB cold (MiB/s) | 6,060 | **8,239 (+36 %)** | — | — |

Counter verdicts (fixed binary):

- **Bar (a) — ≫ budget converges to ~device-true, waste bounded:** steady
  interval escalations **282/20 s** (14/s × 4 MiB = 56 MiB/s = **4.7 % of
  the 1.18 GiB/s foreground** — exactly the 5 % knob), denials 5.2M
  (each a device-true ranged read), IOPS 0.95–0.98× of device-true.
  Baseline was 43,199 escalations (waste ≈ 7× the useful traffic).
- **Bar (b) — ≤ budget unchanged-or-better:** zero clamp engagement on
  truly fitting sets by construction (pinned in-process); on the rig the
  fitting rows IMPROVED 2× (shard geometry: hit rate 90 % → 94 %, misses
  6.1k → 77, `hot_block_current_bytes` 50 → 59 MiB on the 64 MiB set).
  This is allowed by the bar (steady-state RAM-tier speed *unchanged*
  means not-regressed; the baseline churn was a defect).
- **Bar (c) — skew keeps earning:** zipf tier serves 21.6k → **62.8k**
  with admissions 44,696 → **386** (the hot 32 blocks are retained —
  evicted-unhit only 61 of 386; clock `referenced` re-arming keeps paid
  blocks resident while the tail stays device-true).
- **Tripwires:** `write_path_seed_read_bytes` = 0 on every row;
  device-true rows byte-exact (`ranged_read_bytes`/user = 1.00×);
  `stale_binding_rebinds` = 0; seq-read row not regressed (improved —
  device fetches for the 24 GiB pass dropped 11,888 → 8,009 ≈ 1.30× of
  the 6,144 blocks, prefetch pipeline healthier under the new shard
  geometry); ddt posture unchanged (303.0k vs baseline 311.7k, run
  variance band on this rig).

## 6. Contract tests (red-first lineage)

`tests/read_admission_governor_tests.rs`, counter-isolated phases in one
serial fn (cache-less fixture, 512 KiB blocks, deterministic patterns):

- **FIT** (≤ budget): every second touch escalates exactly as today, zero
  denials, zero waste, warm reads device-flat (the governor is invisible
  when the cache is earning).
- **CHURN** (16 blocks vs 2 slots): escalations bounded (4 of 16 second
  touches; 12 denials), waste counters fire. Red at `4c63ce5`: 16/16.
- **SKEW**: hot pair converges to RAM under an engaged clamp (no-cooldown
  denials + trickle), then serves device-flat.
- **PAYBACK**: bytes-based waste plumbing (served ≥ len ⇒ not waste).
- **HERD**: 4096 concurrent clamped attempts never overshoot the epoch's
  token grant (red: check-then-add admitted all 4096).
- **DECLASSIFY**: a spurious classification clears after 16 foreign
  reads; the next cold random miss rides the ranged path (red: 2 s veto).
- **SHARD**: ≥ 16 MiB per hot shard; TieredCache fixture pin + pure-API
  constructor pin.

## 7. Residuals (recorded, not chased)

- **Fill-site whole-block admissions are ungoverned** (`read_tier_
  admissions` on the buffered/whole-block path): those fetches serve the
  read anyway (no extra device I/O), but on cached mounts their NVMe
  publish (memcpy + writeback) under churn is the R1b tax in a smaller
  form; the §5.7 Red publish pause is the only current bound. This rig is
  cache-less — not measurable here. If field cached mounts show publish
  churn, extend the governor's clamp to the fill-site publish decision.
- **Partial admission** (admit the ranged extent, not the whole 4 MiB)
  needs sub-block hot-tier residency — the tier's whole-block-entry
  contract (§5.6 "a partial payload must not exist under a whole-block
  tier key") makes this a design change, not a lever. Filed as the larger
  follow-up if 5 % trickle warm-up proves too slow for huge skewed sets.
- **Escalation-cooldown slot collisions** (the 7× ping-pong) are now
  harmless (the governor bounds aggregate bandwidth) but the 2-way tag
  upgrade named in §5.3 OQ #2 would clean the per-key semantics.
- The mount-cycling harness occasionally races `umount` → `mount` on this
  box (row retried; not a product path — `row.sh` remount hygiene).
- elbencho `--rand` on fitting sets exhausts `--randamount` before the
  timelimit at post-fix speeds; rows stay ≥ 15 s of load (counted as
  run-length-honest — same protocol both binaries).

## 8. Gate

Full cargo gate on the branch head: `cargo clippy --all-targets
--all-features -- -D warnings` clean, `cargo fmt --check` clean,
`cargo test --all-features -- --test-threads=1` — **complete from-zero
pass, 137/137 test binaries green, zero failures** (`--no-fail-fast`
sweep; earlier attempts intermittently tripped two **pre-existing flakes
reproduced at identical rates on CLEAN dev `51c597a`**, both carrying the
same one-extra-device-fetch signature — plausibly one class — recorded
here for their own red-first fix loop):

- `hybrid_io_tests::escape_direct_device_true_is_device_true` — `get_obj`
  +2 instead of +1 on the escape-mode tier-resident O_DIRECT read
  (counted ×10 each side on this box: **dev 5/10 fail, branch 5/10 fail**
  — identical; profile-independent).
- `read_tier_refetch_churn_tests::fetched_blocks_are_tier_visible_and_
  never_refetched` phase B — a sub-read of a just-fetched block refetches
  once (counted ×10: dev 1/10, branch 2/10 — same class).

Neither is this branch's regression (dev-reproduced, counted). The
governor suite itself is ×5-consecutive green; `cargo doc --no-deps`
clean; `cargo bench --benches -- --test` smoke green (22/22). Substrate
left up (RAM-backed, reboot-ephemeral) while the branch is under review.
