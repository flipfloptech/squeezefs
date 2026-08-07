# fio engine policy — libaio matched-instrument law across the perf rigs (2026-08-07)

**Ruling (user, 2026-08-07):** performance rows use **libaio**, never psync —
psync survives only as explicitly-labeled sync-lane coverage rows. This note
is the standing record the rig headers cite; branch `fix/fio-engine-policy`.

## The policy

1. **Every throughput/IOPS row uses `ioengine=libaio` with `direct=1` and a
   stated iodepth — BOTH lanes** (kernel and il/shim). The il lane's libaio
   rows ride the v1.1 aio interposers (valid, engagement-verified
   instruments — `.benchmarks/2026-07-19-v1.1-libaio-interposers.md`); their
   engagement counters may differ from the sync lane (the direct-drive
   `ipc_direct_drive_*` families beside the `ipc_ops_*`/`ipc_bytes_*`
   ledger) — **each row's engagement gate checks the counters ITS lane
   actually moves**.
2. **Any A/B comparison (kernel-vs-shim, armed-vs-control) uses the SAME
   engine both sides** — the matched-instrument law. No more kernel-libaio
   vs shim-psync tables.
3. **psync rows survive only where the §5.5.1 sync fast path is the
   measurand** — explicitly labeled (`SYNC-LANE COVERAGE ROW — psync by
   design, measures the §5.5.1 sync fast path; NOT a headline number`),
   never in a cross-lane comparison.
4. **libaio requires O_DIRECT for true async** — a buffered row must not
   silently use libaio (it degrades to sync); a buffered row states its
   engine choice with a comment.
5. **`ioengine=io_uring`** is admissible only as a labeled **kernel-lane**
   extra — the shim cannot interpose io_uring, so it never appears on il
   rows.

## The structural fact the policy has to respect: the v1.1 single-slot aio law

The aio interposers are **single-slot** (`aio_glue::screen_iocb`,
`crates/squeezefs-preload/src/aio_glue.rs` — "no chunking in v1.1: a
multi-slot op would hold slots hostage across an async completion"): an iocb
with `nbytes >` the session **slot slab** (`Geometry::slot_slab` =
`min(arena_bytes / slots, max_op_bytes)`, slots = 1024, `max_op_bytes`
default 1 MiB) rides the **kernel lane by design** (allow-list
fallback-is-correctness). At the derived arena floor (64 MiB) the slab is
64 KiB — which is exactly why the 2026-08-02 probe
(`.benchmarks/2026-08-02-il-anomalies.md` §3) measured **engagement 0.000**
on a bs=1M shim libaio pass and the old `exa_client_perf.sh` battery ran
psync on its bs=1M shim passes (the banned mixed-engine table).

**The honest lever, measured this session:** `SQUEEZEFS_IPC_ARENA_MB=1024`
on the daemon gives a 1 MiB slab (1 GiB / 1024 slots), making bs=1M aio ops
ring-eligible. Probe on the local venue (below), 2 jobs × 256 MiB bs=1M qd8
libaio direct through the shim: **`ipc_ops_write Δ512` (= exactly the 512
fio ops), `ipc_bytes_in Δ536,870,912` (byte-exact)**. The same probe at the
derived floor reads Δ0 — the engagement gate is what makes the geometry
requirement visible, and `exa_client_perf.sh`/`run_fio_row.sh` now print a
loud geometry hint on any big-bs INVALID(passthrough) shim pass.

Corollaries the reclassification rests on:
* bs > `max_op_bytes` (1 MiB) — e.g. the 4m rows — **cannot** engage the
  ring via aio at any arena size; the ring's 4 MiB path is the sync lane's
  multi-slab `claim_run` chunking. Those rows are sync-lane coverage or
  kernel-lane-only.
* A process FLEET at bs=1M (256 processes × ≥2 sessions × 1 GiB arenas)
  is an admission-budget impossibility — the fleet rigs stay psync as
  labeled sync-lane coverage (their measurand IS the session fleet).

## Rigs touched — row reclassification ledger

| Rig | Before | After |
|---|---|---|
| `tests/fio/exa_client_perf.sh` | kernel-libaio vs shim-psync on bs=1M rows (mixed-engine A/B) | **all four battery rows libaio BOTH passes** (matched); psync survives only as the opt-in, shim-only, table-excluded `sync_lane` row; geometry hint on big-bs passthrough |
| `tests/write_matrix.sh` | psync qd1 everywhere | O_DIRECT rows → **libaio qd=`SQZ_WM_IODEPTH` (8)** both lanes (armed mount gains `SQZ_WM_ARENA_MB=1024` so 256k/1m il aio rows are ring-eligible); buffered rows psync **explicitly** (rule 4); 4m seq rows labeled **SYNC-LANE COVERAGE** (aio cannot express >max_op); prealloc prep → libaio |
| `tests/shim_parity_bracket.sh` | psync everywhere | rand4k rows → **libaio qd8** matched; t16-b4m + 1m-prep rows labeled **SYNC-LANE COVERAGE** (placed-sever ring streaming is the campaign's measurand) |
| `tests/pug_bracket.sh` | psync q1 rows | `q1_4k_od` → **libaio iodepth=1**; buffered `q1_4k` psync explicit (rule 4); engines matched per cell (A/B is binary-vs-binary) |
| `tests/fio/fleet_parity_row.sh` | psync process fleet | **KEPT psync — labeled SYNC-LANE COVERAGE RIG** (adjudication below) |
| `tests/fleet_width_bracket.sh` | psync process fleet | **KEPT psync — labeled SYNC-LANE COVERAGE RIG** (same adjudication) |
| `tests/copy_census_rig.sh` | psync rows | **KEPT psync — labeled** (the census's measurand IS the sync-lane copy sites: §5.5.2 sever + §5.5.1 arena serves; instrument rows, never headline) |
| `tests/run_preload_gate.sh` | psync verify row (unlabeled) | labeled sync-lane CORRECTNESS coverage (the sync interposers are its measurand); 2g-aio row unchanged (libaio interposer coverage) |
| `tests/run_nvmeof_fidelity.sh` | io_uring raw rows (unlabeled) | labeled **kernel-lane raw-ceiling** rows per rule 5 |
| `tests/fio/run_fio_row.sh` + jobs + README | libaio defaults (compliant) | policy header + geometry note; README gains the policy section |
| `tests/fio/{perf_session,d12_session,width_sweep_and_read_decomp,transport_ingress_sweep,zcrx_field_rows}.sh`, `tests/repro_rewrite_refdrift.sh` | already libaio direct + stated qd | policy header (compliance recorded), no row changes |
| `.benchmarks/rigs/2026-08-06-zc-{abba,write-side,write-bracket}-rig.sh` | already libaio | one-line policy-compliance note in the header (historical instruments otherwise untouched) |
| `docs/operations.md` §Performance records | — | one-line policy record |

**Kept-psync adjudications (rule 3's "where the sync fast path is the
measurand"):**
* **Fleet rigs** (`fleet_parity_row.sh`, `fleet_width_bracket.sh`): the
  measurand is the §5.5.1 sync-fast-path **session fleet** (one shim session
  per client process — the field's 256-session shape, session
  establishment + the arena-prep ledger included). The single-slot aio law
  makes a bs=1M libaio fleet structurally a kernel-lane row on the il arm
  (silent-passthrough — exactly what rule 2 bans), and the arena geometry
  that would fix it cannot exist at fleet widths. Both arms run the SAME
  engine (rule 2 holds); the rows are labeled NOT-headline. The write rows
  are buffered+`end_fsync` (the field ingest shape) — psync is the explicit
  rule-4 engine there regardless.
* **Copy census** (`copy_census_rig.sh`): the copy ledger's subject is the
  sync-lane sever/serve copy sites themselves; both A/B arms matched.
* **4m rows** (`write_matrix.sh`, `shim_parity_bracket.sh`): a 4 MiB op
  exceeds `max_op_bytes` — only the sync lane's multi-slab `claim_run` can
  express it as ring traffic; the ring streaming path (placed sever) is
  those rows' measurand.
* **Buffered grids** (`write_matrix.sh` buffered half, `pug_bracket.sh`
  `q1_4k`): rule 4 — libaio without O_DIRECT silently degrades to sync;
  psync is the stated, honest buffered instrument.

## Local matched-engine demonstration (the new reference shape)

**Venue (stated per the two-substrate + instrument rules):** local dev box
(strixhalo, 32 CPUs, 117 GiB), **nvmet-tcp devsub on localhost** (meta
nullb ×4, data zram ×4 — the fabric-sensitive venue), fio-3.42, binaries
daemon+shim both `0bb32c73` (KD-7 verified by the rig), fresh format, mount
`--interception` + `SQUEEZEFS_IPC_ARENA_MB=1024 SQUEEZEFS_IPC_MEM_MAX=49152`
(the slab-covers-bs geometry). **SMOKE mode: 15 s + 10 s ramp, njobs=8 ×
qd=8 (in-flight 64 both lanes), size 512m/job — plumbing/shape proof,
RATIOS-ONLY vs any field number, not a quotable baseline.** Read rows are
WARM (label-only — printed by the rig; no remount, fileset < 2× budget).

`tests/fio/exa_client_perf.sh --rows write_bw,read_bw,randwrite,randread,sync_lane`
(exit 0; every row engagement-gated):

| row | bs | kernel (libaio) | shim (libaio) | delta | shim engagement |
|---|---|---|---|---|---|
| write_bw | 1M | 0.78 GB/s | 0.89 GB/s | +14.4 % | 1.646 (≥0.90 OK) |
| read_bw | 1M | 10.80 GB/s | 6.70 GB/s | −38.0 % | 1.772 (≥0.90 OK) — WARM label-only both |
| randwrite | 4k | 107.2 kIOPS | 105.2 kIOPS | −1.9 % | 2.020 (≥0.90 OK) |
| randread | 4k | 260.6 kIOPS | 354.8 kIOPS | +36.2 % | 1.714 (≥0.90 OK) — WARM label-only both |
| sync_lane (psync 1M, shim-only, labeled) | 1M | — | 0.77 GB/s | n/a (never compared) | 1.317 (≥0.90 OK) |

Engagement ratios > 1 are the 10 s ramp window (stats deltas span the ramp;
fio's reported ios exclude it) — the **byte-exact** proof is the dedicated
no-ramp probe above (`ipc_ops_write Δ512/512`, `ipc_bytes_in` byte-exact).
Amplification columns (per-row `/proc/diskstats` on the data namespaces)
printed per row by the runner; artifacts `/tmp/fioeng_demo/` (local only).

The headline read_bw/randread deltas are WARM-labeled venue shapes on a
half-formatted zram store — direction-informative for the demo only; the
point of the table is that **both lanes now run the same instrument and
every shim cell carries a live engagement verdict**.

## Smoke ledger (D15 local-first; short runtimes, engagement gates live)

| Rig | Result |
|---|---|
| `exa_client_perf.sh` (all 4 rows + sync_lane) | PASS exit 0 — the table above; geometry hint path exercised earlier in the session (default-floor probe read Δ0 → INVALID class) |
| `run_fio_row.sh` + `exa_*.job` | exercised by the battery (10 rows, NUMA fan-out single-node, engagement verdicts live) |
| `transport_ingress_sweep.sh` (4x8, 8x8, 8 s) | PASS — 264k/303k IOPS points, phase tables live |
| `fleet_parity_row.sh` (njobs 16, 64m) | PASS end-to-end — 6 write rows (3 il ENGAGED ipc_ops=1024 + prep ledger closed, 3 kern leak-free) + 6 read rows (3 il ENGAGED); SYNC-LANE label printed on the banner |
| `write_matrix.sh` (filtered smoke, reps 1, 5 s) | PASS exit 0 — il libaio rows ring-engaged **ring/op=1.00** at 4k *and* 1m (the 1 GiB-arena geometry working), 4m rows print `psync SYNC-LANE` with the claim_run **ring/op=4.00** chunking signature, buffered rows psync-explicit; parity verdict pairs=4 win=1 loss=0 par=3 |
| `shim_parity_bracket.sh` (rand4k filter, reps 1, 4 passes) | PASS — libaio qd8 rand4k rows engagement-EXACT (`ipc_bytes_in ≡ user bytes`) on all 4 passes; il ~410k vs kern ~277k IOPS on this venue |
| `copy_census_rig.sh` (wr rows, reps 1, 6 s) | PASS — labeled psync sync-lane rows, engagement exact, NT/THP instruments live (`nt_bytes` ≈ user bytes, ShmemPmdMapped 256 MiB during the il row) |
| `fleet_width_bracket.sh` (width 8, 1 GiB) | PASS exit 0 — il rows ENGAGED (ipc_ops_write=1024), kernel rows leak-free, SYNC-LANE label in the instrument line |
| `pug_bracket.sh` | `bash -n` only — its dialed pug substrate (nvme17–25 null_blk/zram slice) does not exist on this box; change is engine-select + labels on the q1 cells |
| `d12_session.sh`, `width_sweep_and_read_decomp.sh`, `zcrx_field_rows.sh`, `perf_session.sh` | `bash -n` only — field-host-hardcoded paths (`/scratch/tmp`); header-only changes (already libaio-compliant) |
| `.benchmarks/rigs/2026-08-06-*` | `bash -n` only — field-host instruments; one-line header note only |
| `repro_rewrite_refdrift.sh`, `run_nvmeof_fidelity.sh`, `run_preload_gate.sh` | `bash -n` only — comment/label-only changes |

Session note: mid-verification a foreign agent's `run_preload_gate.sh`
(another worktree; 10:50 and 10:55 local) swept the box's daemons twice and
took the demo mount down — after the battery had completed and printed, and
mid `fleet_parity_row` read-twin on the first pass (whose FATAL
require-mount gate fired exactly as designed instead of fabricating rows).
The venue was re-checked free each time, remounted, and the interrupted rig
re-run to completion; no shared state was torn down from here (own
mountpoint `/mnt/sqz-fioeng`, devsub left up).

## What this does NOT change

Historical evidence notes stand as written (their instruments are stated
per-row per the standing instrument-alignment lesson). The three 2026-08-06
zc rigs remain their campaigns' instruments byte-for-byte except the
one-line header note. elbencho-based rigs (scoreboard, pug dial cells,
lifecycle soaks) are out of this policy's scope — fio rows only.
