# 2026-09-02 — E2E performance audit: the baseline of record (squeeze-test)

**Program:** [`docs/design-e2e-perf-audit.md`](../docs/design-e2e-perf-audit.md)
(the finality program — §2 is this table's home; §3 the merged campaign
order; §4 the findings below in full).
**Venue:** squeeze-test — 5 storage nodes over **nvme-tcp**, memory-backed
(nullblk) NVMe targets; client 32-core Xeon 6426Y, 251 GB RAM, 2×200 GbE;
**cacheless mount with `--interception`**.
**Binary:** `aecf1561` (dev tip at the time; `dafa82ca` at writing — one
test-only commit apart).
**Window:** 2026-09-02 14:53–15:10.
**Instrument:** fio, **24 jobs, 30 s + 10 s ramp** per row, the field job
files `/scratch/tmp/fio_jobs/{read_BW,write_BW,randread_iops,randwrite_iops,mixed_workload_BW,mixed_workload_rand_iops,file_creation}.job`;
two modes per row — `kern` (kernel FUSE-over-io_uring) and `il`
(`LD_PRELOAD=libsqueezefs_il.so`, same-commit pairing per KD-7).
**Artifacts:** `/scratch/tmp/e2e-baseline-20260902-145345/` — pre/post
`.stats` snapshot per row + fio bw logs (per job).
**Tier:** measured-real.
**Row class: baseline-30 s — NOT sustained.** No row here is a ≥ 60 s flat
window; none may be cited as a sustained claim (the sustained re-run is
item 2 below). Amplification columns (device ÷ user bytes, `wareq-sz`,
`block_free_*`) were **not captured** for these rows — the write rows are
therefore scoping evidence, not acceptance rows, until the rig
(design §6.2) re-runs them.

---

## The table

Floors (sources in design §2.1): wire payload **41.6 GiB/s** (= 44.7 GB/s,
`.benchmarks/2026-08-15-overlay-b4-overwrite.md` line 119); raw 4 KiB
**2.486–3.36 M IOPS** (`.benchmarks/2026-08-08-shim-reap-fanin.md` line 272;
`.benchmarks/2026-08-05-ingress-queue-spread.md` line 59). "% of floor"
divides GiB/s by 41.6 GiB/s and IOPS by the raw range; mixed rows get no
single floor (duplex directions + mix ratio). Latency has NO floor column:
the 235 µs RTT constant is a 2026-07-25 posture and the il `rr_4k` mean
clat is already below it — the qd1 raw row (item 1 below) is owed first.

| Row | kern | il | % of floor (kern / il) | Reading |
|---|---|---|---|---|
| **w_fresh** (24 × 8 GiB fresh seq write) | **0.36 GiB/s**, clat 986 ms, p99 2.4 s | **0.36 GiB/s**, clat 988 ms | 0.9 % / 0.9 % | **FINDING 46** (below) |
| **r_cold** | 36.88 GiB/s, clat 10.13 ms, p99 38.0 ms | 40.35 GiB/s, 9.21 ms, p99 43.8 ms | **88.6 % / 96.9 %** | il within 3 % of the wire payload |
| **r_repeat** | 36.15 GiB/s, 10.34 ms | 40.34 GiB/s, 9.25 ms | 86.8 % / 96.9 % | cacheless mount: repeat ≡ cold |
| **rr_4k** | 441.5 k IOPS, 0.43 ms, p99 4.95 ms | 873.8 k IOPS, 0.22 ms, p99 0.54 ms | 13.1–17.8 % / 26.0–35.1 % | the plane farthest from its floor |
| **w_rewrite** (same kvmap files) | 32.26 GiB/s, 11.52 ms, p99 49.6 ms | 30.48 GiB/s, 12.08 ms, p99 50.6 ms | 77.5 % / 73.2 % | the files that collapsed fresh rewrite at 32 GiB/s |
| **rw_4k** | 476.2 k IOPS, 0.40 ms, p99 1.55 ms | 704.0 k IOPS, 0.27 ms, p99 1.32 ms | 14.2–19.2 % / 21.0–28.3 % | W1 patch path |
| **mix_bw** | r 23.45 + w 10.05 GiB/s @ 5.0 ms | **see artifacts** (not transcribed) | n/c | per direction kern: r 56.3 % / w 24.1 % of one-direction wire |
| **mix_4k** | r 212.7 k + w 91.1 k IOPS | r 536.0 k + w 229.6 k IOPS | 9.0–12.2 % / 22.8–30.8 % | aggregate vs the raw range |
| **w_durable** (`end_fsync`) | 31.00 GiB/s, 11.88 ms | 31.01 GiB/s | 74.5 % / 74.5 % | kern ≡ il |

**Little's-law reading** (arithmetic-on-measured-constants; the block size
and depth are inferred and must be confirmed against the job files): every
row runs closed-loop at ≈ 190 ops (the 4 KiB rows: 441.5 k × 0.43 ms ≈ 190,
873.8 k × 0.22 ≈ 192, 476.2 k × 0.40 ≈ 190, 704.0 k × 0.27 ≈ 190) or
≈ 380 MiB (36.88 GiB/s × 10.13 ms ≈ 382 MiB; 32.26 × 11.52 ≈ 380 MiB) in
flight — **so every il-vs-kern delta in this table is a latency delta at
equal depth**, and every lever in the program is a latency term.

---

## Per-row engagement / tripwire summary

Per-row stats-inode deltas live in the artifacts (pre/post `.stats` per
row); only the **end-of-run cumulative gauges** were carried into the
session, so per-row engagement accounting (charter rule 4: the row's
`ipc_ops_*` / `patch_writes` / `write_through_blocks` deltas vs fio's op
counts) is **owed from the artifacts**, not asserted here.

| Gauge (end of run) | Value | Reading |
|---|---|---|
| `map_migrate_inos` | 144 | 6 × 24 — six 24-file sets crossed into the kvmap tree; WHICH six rows is a per-row-delta question (matters for which rows ran on kvmap heads) |
| `kvmap_partial_inos` | 0 | no partial crossing left behind |
| `meta_kv_block_refs_drift` | 0 | C8 must-stay-0 held |
| `invariant_tripwires` | 0 | held |
| **`transport_lease_overlong`** | **1** | **fired once — on the f46 row**: a write-handler invocation exceeded the §5.4 lease watchdog's 1 s under the collapse; loud-never-fatal, attributed |
| `fuse_op_watchdog_overdue` | 0 | held |
| `*_fence_drops` (write-pipeline / rewrite-shadow) | 0 | held |
| `ipc_ops_read` / `ipc_ops_write` | 57.4 M / 38.6 M | the il rows' engagement instrument, aggregate |
| `overlay_ack_early_stores` | 6.98 M | the B4 overwrite arm engaged on the rewrite / mixed rows |
| `patch_writes` | 55.3 M | W1 sole-owner patch — `rw_4k` + `mix_4k` writes (the baseline's rand-write files are fresh sole-owner files: NOT the f47 shape) |
| `write_through_blocks` | 201.8 k | complete-block write-through on the streams |

**Tripwire verdict: clean, except `transport_lease_overlong = 1`, which is
attributed to finding 46.**

---

## FINDING 46 — the fresh-stream collapse at the kvmap crossing (fix in flight: `fix/f46-kvmap-stream-publish`)

**Row:** `w_fresh`, both modes. **Evidence (bw logs + per-row deltas +
the `publish_phase_ns` dump):**

- The write ran **28.7 GB/s for ≈ 7 s**, then **all 24 files crossed into
  the kvmap tree at 14:54:22** (`map_migrate_inos` +24) and per-job
  bandwidth fell to **8–12 MB/s for the remaining 33 s**. The 7 s burst sat
  inside the 10 s ramp, so the reported row (0.36 GiB/s, clat 986 ms,
  p99 2.4 s) IS the post-collapse regime.
- **38,587 publishes = one per 4 MiB block**; `publish_phase_ns.total`
  ≈ 23 ms, `meta_commit` ≈ 17.8 ms **of which ≈ 16 ms is unattributed
  inside the commit** — midpoint estimates over a bimodal population (most
  of the 38,587 happened pre-crossing at burst rate), which is the §1
  apparatus caveat in one row.
- `layout_publish_batched_blocks / layout_publish_batches = 1.0`;
  `publish_commit_groups` ≈ all size 1 — the lever-1/lever-B coalescers
  are structurally disengaged on this shape.
- **The REWRITE of the same kvmap files runs 32.26 / 30.48 GiB/s**
  (`w_rewrite`) — the collapse is specific to the post-crossing EXTEND
  publish, not to kvmap resolution.
- `transport_lease_overlong` +1 on this row.

**Arithmetic (flagged):** 8–12 MB/s per job ≈ **0.3–0.5 s per 4 MiB block
per stream**; × 24 ≈ 0.2–0.3 GiB/s aggregate, consistent with the 0.36
GiB/s mean carrying a tail of the burst. 38,587 × 4 MiB ≈ 151 GiB ≈ 5.6 s
at the burst rate — the post-crossing trickle contributed almost nothing.

**Hypothesis under test:** post-crossing extend publishes run the
whole-map diff train **O(map) per block**, against the kvmap design's §3
promise of "a 64-block window ≈ 4 KiB of journal"
(`docs/design-kvmap-block-map-tree.md` §3 Publish).

**Closing evidence required:** `w_fresh` both modes ≥ the same files'
`w_rewrite`, **sustained 60 s across the crossing**; `publish_phase_ns`
with exact sums (post-A1) attributing `meta_commit`; `batched_blocks /
batches ≫ 1` post-crossing; the red-first cargo contract pinning O(batch)
per-block publish cost after the crossing.

## FINDING 47 — the overlay B4 arm reopens the small-write amplification regime (fix in flight: `fix/f47-overlay-length-floor`)

**Source:** the 2026-09-02 write ledger, fat #1 (HIGH) — NOT a baseline
row (the baseline's rand-write files are fresh sole-owner files and ride
W1: `patch_writes` 55.3 M). **Mechanism:** the device-overlay B4 overwrite
arm (`src/fuse_client.rs:14704`, gated only by
`overlay_overwrite_enabled()`) runs BEFORE the W2 extent park
(`try_extent_park`, called at `src/fuse_client.rs:16659`) with **no length
floor**, so a **patch-INELIGIBLE sub-cap write** (hole / clone-shared /
decorated — the shapes W1's 6-clause ledger refuses) mints a fresh 4 MiB
CoW dest and reads the 4 MiB old image **per 4 KiB write: ≈ 1,024× each
way**. **Field-observed:** `.benchmarks/2026-08-17-mw-shipped-free-c8-fix.md`
line 30 — `overlay_gap_seed_old_bytes +25,141,248 = 6 × 4 MiB − 24,576 user
bytes` for six sub-block writes. The ~2,500× regime the RW program closed
(`.benchmarks/2026-07-17-rand-write-program-closing.md`) is reopened for
every shape W1 does not take; write ledger #2 (small-bs seq O_DIRECT
riding the overlay per-op — the `wareq-sz` collapse) is the same hole.

**Fix in flight:** a derived length floor on the overlay arm,
`len ≥ patch_max_bytes` (= `block_size/8`, `SQUEEZEFS_PATCH_MAX_BYTES`'s
derivation, `src/env_knobs.rs:146`), so sub-cap shapes fall through to the
W2 park. **Closing evidence:** a deliberately built f47 venue (rand-4k on
hole / clone-shared / decorated files), both substrates, A-B-B-A:
`overlay_gap_seed_old_bytes` ≈ 0; `extent_parks` accounts for the row;
amplification columns back in the RW program's 15–26× fold regime; the
red-first contract in the write-through / overlay suites.

## Finding candidate 48 — the per-read timer + channel mutex class (MEASURE FIRST)

Not a finding yet (a mechanism and an arithmetic estimate, no number).
Since the 2026-08-13 rip-tokio-TOTAL sweep, every device read passes
`sqz_time::timeout` (`src/nvme_dev.rs:2426`; 30 s default at `:91-97`):
process-global `Mutex<Registry>` (`crates/squeezefs-ipc/src/sqz_time.rs:69`)
over a `BinaryHeap` (`:55`), tombstone on normal completion (`:192`),
`Box::pin` of the future (`:240`); plus per-channel `Mutex` on the fill's
lane mpsc (`nvme_dev.rs:2404`) and oneshot (`:2395`), and the same
`timeout` per 50 ms cohort-wait slice (`src/routing.rs:9299`). Ledger
arithmetic: ≈ 1.2 M process-global mutex ops/s + ≈ 190 MB tombstones at
field rand-4k rates; **unpriced by any read row** (every read number
predates the sweep, and today's `rr_4k` 441 k / 874 k is ABOVE the
pre-sweep record of 364–393 k / 438–531 k, so the cost is hidden inside a
net gain). The measurement: A1's `dev_queue` exact sums +
`daemon_cpu_ns_by_class`, plus `perf record` on the `fuse3-tpc*` /
`sqz-ipc-svc*` lanes during `rr_4k` with the `sqz_time`/`sqz_channel`
symbol share — design §4.3.

---

## What to run next (in order)

1. **B-10 raw fabric rows on today's target posture** — fio direct on the
   namespaces: rand 4 KiB **qd1** (the RTT floor — retires the stale
   235 µs), qd8, qd32 (one number for the 2.486–3.36 M range), seq 1 MiB
   read/write. Nothing latency-graded is computable before this.
2. **The sustained re-run of this table** — every row ≥ 60 s, first-third
   vs last-third flatness stated, **amplification columns on every write
   row** (diskstats deltas on the DATA namespaces, `wareq-sz`,
   `block_free_*`), per-row engagement accounting from the `.stats`
   deltas, the `mix_bw` il cell transcribed. This is the row set the
   campaign PRs bracket against.
3. **f46 closing row** on the fix branch: `w_fresh` both modes across the
   crossing, sustained 60 s, `publish_phase_ns` exact sums (post-A1),
   `batched_blocks / batches` post-crossing.
4. **f47 venue build + closing row**: prepared hole / clone-shared /
   decorated file sets; rand-4k A-B-B-A both substrates with the
   amplification columns and `overlay_gap_seed_old_bytes` ≈ 0.
5. **Candidate 48 measurement**: `rr_4k` both modes on the A1 binary with
   `daemon_cpu_ns_by_class` + perf on the svc/tpc lanes; promote to
   finding 48 or adjudicate load-bearing-at-cost.
6. **`tests/run_bench_baseline.sh save`** on the A1 binary (the month-stale
   `reference.json` at `66bb4775` retired) — then the nightly compare
   means something again.
7. **The D-1 baseline row** for F-A: the K-writer fan-out fleet
   (`tests/mw_fleet.sh`, K = 1..8) on tcp devsub — verbs/s per authority
   and aggregate ingest vs the S9-a ≈ 2.6 GiB/s wall, `owner_phase_ns`
   exact sums, engagement `shipped ≡ served` — the row the first campaign
   PR brackets against.
8. **The per-row-delta transcription** from the artifacts: which six
   24-file sets crossed (`map_migrate_inos` 144), per-row `ipc_ops_*` vs
   fio op counts, per-row `overlay_ack_early_stores` attribution.

## Addendum — same-day raw-device controls (2026-09-02 15:40, the denominators)

Raw `fio` libaio direct against the ten data namespaces on the same
client, FS mounted but idle (artifacts `raw.*.json` beside the rows).
These replace the ledger-era constants for every "% of floor" on this
venue:

| Control | Shape | Result |
|---|---|---|
| qd1 fabric RTT (4k randread) | 1×1 | **24.8 µs** mean, p50 24.4, p99 32.4 (the 235 µs constant was another fleet's) |
| 4k randread IOPS ceiling | 32×8 / 32×32 | **2.03 M** (116 µs mean) / **2.73 M** (365 µs mean) |
| 1 MiB seq read ceiling | 24×16 (= the FS read job's shape) | **38.59 GiB/s** (41.4 GB/s ≈ 93 % of the 44.7 GB/s payload wire), clat 9.5 ms |

Re-derived distances (same shape, same day):

| FS row | kern | il | verdict |
|---|---|---|---|
| seq read 24×16 | 36.88 GiB/s = 89 % of wire | 40.35 GiB/s = **97 % of wire — at the floor** | kern owes ~8 % (transport ingress, read board #2); il has nothing left |
| seq rewrite | 32.3 = 78 % | 30.5 = 73 % | write board #3/#5 (overlay depth, conveyor) — ~22 % to the wire |
| rand-4k read 24×8 | 441 k = **22 %** of 2.03 M | 874 k = **43 %** | THE headroom: FS adds ~310 µs (kern) / ~100 µs (il) per op over raw at matching depth — read board #1/#2/#6 |
| rand-4k write | 476 k / 704 k | raw randwrite control not yet run (nullblk writes are discards — run it before adjudicating) | |
| seq fresh write | 0.36 GiB/s | | finding 46 |

## Addendum — the user-run EXA client-validation row (binary `c985fa8c`)

Recorded beside the baseline rows because it is the row the README
quotes (the four "hero numbers"); it is **the user's instrument, not
this rig's**, and is not a re-run of the table above.

**Instrument:** the user-run **EXA client-validation script v1.2.1**,
**il mode** (`LD_PRELOAD=libsqueezefs_il.so`, same-commit pairing),
**40 s rows**, memory-backed targets on the squeeze-test fabric (the
venue at the top of this note). **Binary:** `c985fa8c` (dev tip
2026-09-03 — carries the R-2/R-3/R-4 read-side campaign, the W-3 overlay
depth governor, the f46/f47 fixes, D-1b/D-2/C-2 on the metadata side,
and the finding-40 reclaim hysteresis that is the commit itself).

| Row | Result |
|---|---|
| Read BW | **43.9 GB/s** (= 40.9 GiB/s; the 24×16 il `r_cold` row above read 40.35 GiB/s on `aecf1561`) |
| Write BW | **36.4 GB/s** (= 33.9 GiB/s; the `w_rewrite` / `w_durable` rows above read 30.5–32.3 GiB/s on `aecf1561`) |
| Read IOPS (4 KiB) | **942 k** (the il `rr_4k` row above read 874 k on `aecf1561`; the R-2/R-3 notes measured the read-side levers' kern gain at +14.8 %/+15.5 %) |
| Write IOPS (4 KiB) | **727 k** (the il `rw_4k` row above read 704 k on `aecf1561`) |

What this row does NOT carry, stated so it is not over-read: the
script's job shapes (block size, queue depth, job count, file set) are
the script's own and were not transcribed here — the parenthetical
comparisons above are shape-approximate, not same-shape brackets; no
`.stats` engagement deltas (charter rule 4: `ipc_ops_*` vs the row's op
count) and no amplification columns were captured for it; 40 s is
below the ≥ 60 s sustained-row bar, so it is a **burst-class** row like
the 30 s baseline rows, not a sustained claim. **The script's summary
printed an `actions ⚠` flag; what that flag means is unexplained** — the
script's author owns its semantics, and this note records the flag
rather than interpreting it. Tier: measured-real (the user's own
fabric), instrument external to the repo.
