# 2026-07-27 — Write-pipeline depth: ACK-parked custody + runtime-BDP admission

Branch `perf/write-pipeline-depth` (off dev `78b9498`). Design +
implementation: `src/write_pipeline.rs`, the detached complete-block
write-through in `src/fuse_client.rs`, the flush write-through leg
(fsync/teardown steal), contract suite `tests/write_pipeline_tests.rs`.

## 1. The field conviction (motivating capture, 4-node cluster)

2×200GbE nvme-tcp cluster, memory-backed null_blk data targets, elbencho
64t × 4 MiB O_DIRECT (shim path):

* Sequential large writes walled at **7.5–8.3 GB/s** while raw fio put
  **6.7 GB/s into a SINGLE namespace at QD128** (two data namespaces;
  198 Gbit/s per NIC measured on the wire).
* Client iostat, per data-namespace head: ~933 w/s × 4 MiB, util ~99 %,
  w_await ~1.9 ms, **aqu-sz ~1.75** — the daemon kept < 2 block writes
  in flight per device. Little's law: 1.8 × 4 MiB / 1.9 ms ≈ 3.8 GB/s
  per device = the observed wall.
* Threads 64→128: throughput 7.5→~8 GB/s only, average latency doubled —
  concurrency above the funnel queued. Same wall with 16 and 64 files.
* NOT flow-concentrated: ~80 nvme-tcp connections active, both NICs and
  paths balanced (hypothesis measured dead — do not revisit).
* Client mpstat: CPU0 ~85 % %sys (single hot submission/transmit
  context), everything else mostly iowait. CPU0 is a candidate NEXT wall
  after depth — **not reproduced on the localhost rig** (§6: hottest
  core 47 %, mean 23 % at the daemon's ~1.7 GiB/s ceiling), so it stays
  a field-venue observation.
* **Filed, not fixed (mds-journal economy):** the field capture showed
  mds0 absorbing ~50k × 4 KiB journal writes/s for ~1,870 block
  commits/s (~27 meta device-writes per data block). On this campaign's
  rig the ratio measured **0.17 meta-writes per data block** (§6) — the
  field shape did not reproduce here; candidate next campaign on the
  field venue.

**The convicted mechanism:** `upload_full_block` (crypto → allocate →
DMA → merge) was awaited INLINE in the WRITE handler — every writer a
closed loop, aggregate throughput = threads ÷ per-block pipeline
latency, device queue depth bounded by writer count net of every other
serialization, not by the device.

## 2. The design law (USER DIRECTIVE — no fixed pipeline depth)

Deployments move 2×200GbE → 400GbE → 800GbE; **no constant depth is
correct across that range**. The depth target derives at runtime, per
backend lane, from measured service time × achieved bandwidth
(Little's law × `HEADROOM`), with two bounds ONLY:

1. **The R5 memory budget** — in-flight write-block bytes are the
   non-sheddable `write_pipeline_inflight` component; the target is
   hard-capped at budget ÷ 4; **Red clamps the target to its floor** so
   custody converges by completion (honest backpressure, never OOM).
2. **Existing admission semantics** — `WritePipeline::admit` is awaited
   in the WRITE handler *before* the ACK.

The governor's runaway guard: the latency-floor estimate decays upward
bounded by the latency EWMA, so congested queueing latency never feeds
the BDP arithmetic (at saturation inflight ≡ bw × latency — a
congested-latency BDP would chase its own tail). BDP arithmetic uses
u128 intermediates (800GbE-class products pinned in tests).

ACK/durability semantics are unchanged: the completing WRITE ACKs with
custody PARKED (readable overlay — the exact posture of
partial-coverage and staging-fallback writes; durability owed at
fsync/close per the writeback-cache contract), and fsync/teardown
draining a pipe-parked complete block rides the **flush write-through
leg** (one durable upload, never the staging+writeback detour — T5).
Fencing mid-flight drops custody loudly (`write_pipeline_fence_drops`,
must-stay-0; the remount law, FIND-M11-A); every other failure stays on
the never-lossy ladder.

`SQUEEZEFS_WRITE_PIPELINE_DEPTH_BLOCKS` is the A/B lever only
(0 = pre-campaign sync-inline; N = pinned; unset = the adaptive
governor — the shipped default).

## 3. Substrates + instruments (per the two-substrate rule)

All measurement on **nvmet-tcp** (fabric-sensitive rows; the loop rig is
not cited here). Two venues, both created this session:

* **`wpd` tcp devsub instance** — `SQZ_DEVSUB_TRANSPORT=tcp
  SQZ_DEVSUB_INSTANCE=wpd tests/dev_substrate.sh create` (the new
  instance-suffix knob: the default tcp substrate was owned by a sibling
  campaign's live mount). Meta = 4 × 1 GiB null_blk (nvme17–20n1), data
  = 4 × 8 GiB zram zstd (nvme21–24n1), localhost NVMe/TCP :54131-slice.
* **Dialed-latency data namespace** — configfs null_blk `sqzwpdlat0`
  (24 GiB, `memory_backed=1`, **`completion_nsec=20000000` (20 ms)**,
  `irqmode=2`, `max_sectors=8192` — the stock 124 KiB `max_sectors_kb`
  splits 4 MiB writes 33× and must be raised for a block-grain dial),
  exposed
  via nvmet-tcp `127.0.0.1:54131`. This is the **mechanism-isolation
  venue**: a device whose throughput is depth-proportional
  (qd1 = 186 MiB/s, qd32 = 3.0 GiB/s single-job fio), playing the role
  the 1.9 ms × 40 GbE-class BDP plays in the field. 20 ms (not the
  field's 1.9 ms) because a localhost rig has none of the field's other
  loop latency — the dial scales the device term so the 4–16-thread
  inline BDP sits below the device ceiling, same regime as the field.
* **Instruments:** elbencho 3.1-10 (dynamic; `--direct`, sync drivers)
  for all FUSE rows and the matched-content raw ceiling; fio 3.x libaio
  for raw device ceilings; `fio psync` inside `tests/write_matrix.sh`.
  Content matters on zram: fio-content raw ceiling 1.35 GiB/s vs
  elbencho-content 2.35 GiB/s on the same namespaces — every row states
  its instrument.
* **Contention label:** a sibling campaign held a live mount + one debug
  test loop on this box until ~22:10; rows before that (k16t*/k64t* zram
  cells) carry wider rep spread (single-rep outliers 786–1317 within one
  cell). The dialed rows and the matrix ran after the sibling exited.

Harness: `tests/wpd_bracket.sh` (new) — fresh `format --force` per run,
A-B-B-A + B-A order (3 reps/binary/cell, medians), per-run
`/proc/diskstats` deltas on the data namespaces (aqu-sz = weighted-ms Δ
÷ elapsed; wareq = bytes ÷ writes; **amp = device bytes ÷ user bytes**),
stats-inode deltas (`write_pipeline_*`, `write_through_*`,
`ipc_ops_write` for shim engagement). KD-7: each binary runs its own
same-commit shim (a mismatched pair HELLO-refuses into silent
passthrough — caught live when the first shim bracket ran dev-tip
against the branch shim: `ipc_write_delta = 0` rows, discarded and
rerun paired).

## 4. Raw ceilings (the finish line)

| Row | Instrument | Result |
|---|---|---|
| 4-ns zram spread, 4M qd32×4 | fio libaio | 1349 MiB/s |
| 4-ns zram spread, 4M qd16×16 | fio libaio | 1353 MiB/s |
| single zram ns, 4M qd32 | fio libaio | 309–592 MiB/s (1–4 jobs) |
| 4-ns zram spread, 4M 16t iodepth4 | **elbencho** (matched content) | **2352 MiB/s** last-done |
| dialed ns, 4M qd1 | fio libaio | 186 MiB/s (clat 21.3 ms — the dial) |
| dialed ns, 4M qd32 | fio libaio | 3040 MiB/s |
| dialed ns, 4M qd32×4 | fio libaio | 4528 MiB/s |

## 5. A-B-B-A brackets (A = branch `2797979`-era binary, B = dev `78b9498`; medians of 3)

### 5.1 zram tcp substrate (16 GiB user bytes/run, fresh format each)

| Cell | A MiB/s | B MiB/s | A aqu-sz | B aqu-sz | amp A/B |
|---|---|---|---|---|---|
| kernel 16t × 1M | 1280 | 1273 | **8.0** | 5.6 | 1.001 / 1.001 |
| kernel 64t × 1M | 1426 | 1408 | **13.1** | 11.7 | 1.004 / 1.004 |
| kernel 16t × 4M | 1281 | 1225 | **9.6** | 5.0 | 1.002 / 1.002 |
| kernel 64t × 4M | 1336 | 1385 | 12.7 | 13.0 | 1.006 / 1.007 |
| shim 16t × 1M | 1353 | 924 | **12.1** | 6.2 | 0.997 / 0.998 |
| shim 16t × 4M | 1184 | 1163 | **11.8** | 5.7 | 0.998 / 0.998 |
| kernel 16t × 1M `--sync` (durable-labeled) | 1236 | 1246 | 7.6 | 4.9 | 1.001 |
| kernel 16t × 4M `--sync` (durable-labeled) | 1180 | 1251 | 11.3 | 5.4 | 1.002 |

Reading: **on this venue both binaries sit at the daemon-CPU ceiling
(~1.2–1.4 GiB/s ≈ 0.55–0.60× the 2.35 GiB/s matched-content device
ceiling), so throughput is a wash within the rig's noise band** (single
reps scatter ±20 %; the shim 16t×1M A-vs-B delta is inside that band on
the B side and is not claimed as a win). What separates cleanly and
consistently is the **device queue depth: the pipeline holds aqu-sz
8–13 where dev-tip holds ~5** at 16t — the charter's aqu-sz ≥ 8 bar is
met on every 16t cell (dev-tip meets it only by brute thread count at
64t). Amplification ~1.0 everywhere; `wareq-sz` ≈ 4 MiB (no request
collapse); `write_pipeline_fence_drops = 0` on every run; shim rows
engagement-exact (`ipc_ops_write` Δ = 16384 = user ops).

### 5.2 Dialed-latency venue (the mechanism row)

Mid-campaign binary (`2797979`-era), medians of 3:

| Cell | A MiB/s | B MiB/s | A aqu-sz | B aqu-sz | A depth_target |
|---|---|---|---|---|---|
| kernel 4t × 4M | 1652 | 361 | 47.4 | **2.0** | 136–195 MB (runtime-derived) |
| kernel 16t × 4M | 1667 | 1577 | 50.3 | 13.4 | 216–259 MB |

**FINAL branch binary (`1a6a147`, all four §8 fixes in), medians of 3,
quiet box:**

| Cell | A MiB/s | B MiB/s | ratio | A aqu-sz | B aqu-sz | amp |
|---|---|---|---|---|---|---|
| kernel 4t × 4M (dialed) | **3939** | **374** | **10.5×** | 32–58 | **2.0** | 1.000 |
| kernel 16t × 4M (zram) | 1284 | 1254 | 1.02× | 9.8 | 4.7 | 1.002 |
| kernel 16t × 4M `--sync` (zram, durable-labeled) | 1195 | 1204 | 0.99× | 9.1 | 4.7 | 1.002 |

**The 4t dialed row is the field shape reproduced end-to-end: dev-tip
walls at 374 MiB/s with aqu-sz 2.0 — the same < 2 per-device depth the
field iostat showed — while the depth-governed pipeline runs
3,939 MiB/s (10.5×) at aqu-sz 32–58, its target grown by the governor
from measured bandwidth × uncongested latency, not a constant.**
Against the dialed venue's fio qd32×4 ceiling (4,528 MiB/s) the final
branch achieves **0.87×**; against single-job qd32 (3,040 MiB/s),
**1.30×** (elbencho spreads 4 files). At 16t the zram venue's daemon-CPU
ceiling (~1.3–1.7 GiB/s) re-binds both binaries; the pipeline still
holds ~2× the queue depth. Stated honestly: **on localhost rigs the
post-depth wall is daemon data-path CPU, spread across cores (§6), not
device depth.**

### 5.3 write_matrix (charter (c): 40-row sweep, REPS=1 RUNTIME=6 — reduced-rep

scoping labeled as such; the headline seq cells carry the full §5.1
brackets). Venue: wpd meta + a dedicated 48 GiB zram nvmet-tcp namespace
(the stock 8 GiB namespaces ENOSPC the seq grid). Branch pass and
dev-tip pass both completed with engagement checks green (exit 0, every
shim row `ring/op=1.00`, zero tripwire movement).

Adjudication: at REPS=1 the grid scatters ±2–3× in BOTH directions
(e.g. shim-rand-64k 3.08× branch-ahead, shim-seq-4k-odirect 0.62×
branch-behind in the same pass). Every row below 0.9× was re-run at
REPS=3:

| Row (REPS=3 medians) | branch | dev | ratio |
|---|---|---|---|
| armed-shim-rand-256k-buffered | 961 | 979 | 0.98 |
| armed-kernel-seq-1m-buffered | 290 | 293 | 0.99 |
| armed-shim-seq-4k-odirect | 52,682 | 50,181 | 1.05 |
| armed-shim-seq-64k-odirect | 4,242 | 4,185 | 1.01 |
| unarmed section (8 rows) | branch ahead 1.34–1.64× except rand-4k | | |

The one residual (~0.90× on the 4k-rand family, consistent across two
branch-first passes) **reversed under the reversed run order**
(dev-first 41,801 → branch-second 47,002 = 1.12× branch-ahead on
`armed-kernel-rand-4k-odirect`) — an ordering/box-state artifact per the
standing A-B-B-A rule, not a binary effect. **Verdict: no matrix row
regresses beyond noise.**

## 6. Auxiliary instrumented run (branch, dialed venue, 16t × 4M, 16 GiB)

* Throughput 1779 MiB/s; elapsed 9.7 s.
* **Write amplification 1.002** (data-ns diskstats: 4112 writes ×
  4087 KiB avg = 17.208 GB device for 17.18 GB user).
* **Meta economy: 0.17 meta device-writes per data block** (714 writes /
  36 MB on the busiest meta ns for 4096 block commits) — the field's ~27
  ratio did not reproduce here (filed in §1).
* **CPU: hottest core 47 % busy, mean 23 %** — no CPU0-class
  single-context wall on localhost; the ~1.7 GiB/s venue ceiling is
  distributed data-path CPU (memcpy/checksum/uring submission spread).

## 7. Gauges (stats inode; AGENTS.md listing updated)

`write_pipeline_inflight_blocks` / `write_pipeline_inflight_bytes`
(gauged R5 component `write_pipeline_inflight`),
`write_pipeline_depth_target` (the runtime-BDP instrument — observed
growing 134 MB → 259 MB with measured bandwidth on the dialed venue,
floor 32 blocks cold), `write_pipeline_admission_waits` (writer
backpressure — 84–3,943 per bracket run, scaling with thread count),
`write_pipeline_fence_drops` (must-stay-0 — 0 on all ~90 measured runs).

## 8. Fixes convicted by the campaign's own suites (landed on the branch)

1. **Flush write-through leg** (`fix(write)` ab06172): fsync/teardown
   stealing a pipe-parked complete block previously took the
   staging+writeback detour (the RW3b parked-straggler pipeline) —
   pinned RED by T5, healed the fsck/defrag/drain/coverage suites.
2. **Indirect-blob pointer coherence** (`fix(routing)` 9ac783b): the
   RAM cache republish kept a stale `block_map_id`, leaking one
   spilled-map blob incarnation per merge (~130 orphans / 700-block
   burst measured; masked by the old sequential inline uploads).
3. **`read_tier_purge` ↔ `backend_router` Arc cycle** (`fix(cache)`
   8be19e6): every dropped mount graph immortal (~65 leaked segment fds
   per fixture; EMFILE at suite scale — pre-existing on dev, reproduced
   at `78b9498` under `ulimit -n 1800`). The staging host now holds the
   router weak; pinned by `tests/fd_release_tests.rs` (leak==0, red 65).
   Residual: ~8 fds/fixture from a different holder — open question.
4. **Flush-leg size floor** (`fix(write)` 9e4bf2f): the flush
   write-through leg inherited `upload_full_block`'s block-end
   `min_size` — correct for coverage-union-complete ACK-path blocks,
   WRONG for zero-completed/seeded parked custody, where it published a
   size acked data cannot back (the generic/795 SIZE-NEVER-LEADS-DATA
   law; its repro-port `sequential_recopy_readers_never_see_foreign_
   bytes` went deterministic-red the day the leg landed and bisected to
   it exactly). Flush legs now merge with `min_size = 0` (the RAM acked
   floor stays the honest bound); the ACK path keeps block-end verbatim.

## 9. Open questions

* **OQ-1**: the field CPU0 %sys wall (nvme-tcp transmit context) —
  next wall after depth on the 4-node cluster; needs field re-measure
  with the depth branch (`write_pipeline_depth_target` and per-device
  aqu-sz are the instruments).
* **OQ-2**: the field's ~27 meta-writes/data-block journal ratio (0.17
  here) — reproduce on the field venue before spending a campaign on it.
* **OQ-3**: localhost daemon data-path CPU ceiling (~1.7 GiB/s at 4 MiB
  blocks) — the venue's post-depth binding constraint; profile before
  the 400GbE window.
* **OQ-4**: residual ~8 leaked fds per dropped fixture graph (§8.3's
  remainder) — holder not yet identified.
