# 2026-07-31 — fio gap accounting: the canon, the ladder, THE GAP NAMED

> **SUPERSESSION (same day):** every "26.1 GB/s raw write ceiling"
> reference below is RETIRED — the approved re-sweep measured the true
> ceiling at **49.7 GB/s (dual-200GbE line rate)**, re-verdicting §8.1
> to FS-write = 0.63× raw (~18 GB/s headroom). See
> `.benchmarks/2026-07-31-raw-write-ceiling-resweep.md` (source of
> record). The read-side verdict (§6.2/§8.2) is unchanged.

Branch `perf/fio-gap-accounting` (off dev tip `3ea474b`, **unmerged —
do not merge/push without orchestrator review**). Governing user
directives (verbatim): *"Finding the gap is the most important thing we
can do. once we know what the gap is we can target it accordingly"*;
*"The lustre benchmarks are ran with fio so fio makes the most sense to
do all testing with from here out"* (**fio is the house instrument**;
elbencho = informal only); *"Whatever fio incantations we come up with
should be stored in the repo somewhere maybe in a benchmarks section
for reproducibility."*

Commits: `f981462` (the fio canon), `1f347aa` (amp columns +
--emit-only), `9bbf001` (fresh_write_pass shape), plus this note.

## 1. Deliverable 1 — the fio canon (`tests/fio/`)

Shapes reproduced from the DDN EXAScaler client-validation kit
(shapes only — no vendor content committed):

| Job | Shape | Purpose |
|---|---|---|
| `exa_write_bw.job` / `exa_read_bw.job` | libaio, bs=1M, qd=8, direct, time_based 30s+10 ramp, size=1g, njobs=nproc NUMA-split | The Lustre-comparable validation BW shape |
| `exa_randwrite_iops.job` / `exa_randread_iops.job` | same, bs=4k fixed | The validation IOPS shapes |
| `gap_probe_write.job` / `gap_probe_read.job` | all dims parametrized | The discriminating sweep probes |
| `fresh_write_pass.job` | one pass, volume-bound | Fresh-ingest face (raw-ceiling-comparable) |
| `raw_ceiling_write.job` / `raw_ceiling_read.job` | bs=4m, qd=16, 8 jobs/device, per-device sections | Device-sweep raw ceilings; write is DESTRUCTIVE — runner refuses without `--i-know-this-destroys-data` |

`tests/fio/run_fio_row.sh` is the runner: **topology-general NUMA
fan-out** (discovers nodes at runtime, one section per node with
`numa_cpu_nodes`/`numa_mem_policy=bind`; works on N domains; single
node ⇒ one section, no numa lines — verified on 1, 2 and 3-domain
emission), raw per-device sections, shim rows engagement-VERIFIED from
the stats inode (`ipc_ops_*` deltas ≥ 0.90 of the row's ios or exit
nonzero), `--data-devs` diskstats amplification columns, `--emit-only`
dry-run seam, venue labels (instrument/shape/substrate/fill/order)
printed + persisted per row. `bash -n` + shellcheck clean.

## 2. Venue (labeled once, applies to every row below)

**Client** squeeze-test: 32 CPU / 2 NUMA nodes, dual-200GbE.
**Substrate:** the 4-node cluster (2 mds + 2 oss, `cluster_reset` v3),
**nullblk 4-wide** data plane (8 × 48 GiB memory-backed null_blk
namespaces over nvme-tcp: `nvme{4,6,8,10,12,14,16,18}n1`), meta
`/dev/nvme{0,2}n1`, cache-less format, 4 MiB blocks. **Binary:** the
`.devtip` pair = dev tip `3ea474b` (this branch's parent; branch adds
no daemon changes). **Instrument:** fio-3.36 (distro dynamic build,
numa-enabled), driven by `tests/fio/run_fio_row.sh`; all rows journaled
to `/scratch/tmp/agent_runs.log`; artifacts (fio JSON, generated jobs,
stats-inode before/after/delta, ss -tin + iostat samples) under
`/scratch/tmp/fio_gap/`. **References on the same store, same binary**
(sibling numa-affinity verdict, elbencho 3.1-10, t32×4m over **16
shared files**, 60 s infloop): fresh 22.3, wrkern 16.7, wril 17.9,
rdkern 16.9, rdil 17.0 GiB/s. **Raw ceiling reference:** the recorded
**26.1 GB/s** 4-head fio WRITE ceiling (journal 2026-07-31T08:12:53Z;
destructive — NOT re-run, per orchestrator rule).

Store hygiene: reclaim-queue settle (`block_free_reclaim_queue_bytes`
= 0 ×3) before every write row; loop-rewrite rows labeled with live
store-fill %; write rows carry diskstats amp columns (ramp-inclusive,
so ≈ 1.17× overstatement at 60s/10s — labeled).

## 3. Rung 1 — shape parity (the big one)

All rows 60 s sustained + 10 s ramp, `$M/fiogap`, A-B-B-A across the
aging store (pass1 then reversed pass2). "il" = LD_PRELOAD shim;
engagement per row printed by the runner.

### 3.1 Write side (GB/s = decimal; GiB/s in artifacts)

| Row (shape) | pass1 | pass2 | engagement |
|---|---|---|---|
| **EXA shape, kernel** (libaio 1M qd8 nj32, 32 files) | **31.33** | 30.89 | n/a |
| **EXA shape, il** | 31.31 | 31.53 | 0.000 — **kernel-lane by v1.1 design** (see §3.3) |
| psync 4M nj32 (elbencho-t32b4m analog, 32 files), kernel | 25.26 | 25.18 | n/a |
| psync 4M nj32, il | **30.01** | 30.48 | 3.50 OK (ring 1M chunking) |
| psync 1M nj16 (elbencho-t16b1m analog), il | 32.69 | **33.91** | OK |
| fresh pass 32×8g (256 GiB), kernel, pass-bound | 28.61 | 29.36 (a2_prefill) | n/a |

(The `r1_fresh_il` row is excluded — INVALID-venue, see §7 hygiene
finding.)

Cross-check vs references: identical direction in both bracket orders;
write amp 1.11–1.16 (ramp-inclusive) with `wareq-sz` 4 MiB-class on
every row.

**Verdict (write):** the EXA validation shape through the FUSE kernel
path sustains **~31 GB/s — 1.20× the recorded 26.1 GB/s raw fio
ceiling and ~1.7× the elbencho rows that defined "FS ~20 GB/s"**.
There is **no FS write-bandwidth term** on this venue at validation
shapes: the "gap" was instrument shape. Decomposition of the
instrument-shape term:

1. **Per-stream sync depth (the dominant term):** psync 4M qd1 ×32
   through the kernel path = 25.2 GB/s, Little-bound end-to-end
   (clat 5.08 ms ⇒ 32 × 4 MiB / 5.08 ms = 26.4 GB/s predicted — the
   row IS its latency chain). The same offered load with qd8 async
   depth = 31.3 GB/s. elbencho `-t 32 -b 4m --direct` is exactly the
   psync shape.
2. **File sharing (the second term):** the elbencho reference rows run
   32 threads over **16 shared files**; the fio canon runs one file
   per job. §6 addendum prices this per-inode concentration term
   directly (fio psync 4M ×32 over 16 shared files, offset-split).
3. **The il ACK-before-DMA detach erases term 1 for sync writers:**
   psync 4M il rides the ring (4×1M chunks, ACK at sever) — clat
   4.14 ms → **30.0–30.5 GB/s**, and psync 1M nj16 il hits **32.7–33.9
   GB/s at clat 0.49 ms** — the shim-parity design working as
   specified.

### 3.2 Rung 3 — phase residence during the shapes (write_pipeline_phase_ns deltas, bucket-midpoint means, ms/block)

| phase | EXA kern (31.3) | psync4M kern (25.3) | psync1M il (32.7) | fresh kern (28.6) |
|---|---|---|---|---|
| admit_wait | 21.84 | 0.02 | 0.18 | 0.00 |
| detach_lag | 0.68 | 0.08 | 0.08 | 0.06 |
| lock_wait | 0.11 | 0.01 | 0.00 | 0.06 |
| crypto+allocate | 0.01 | 0.01 | 0.00 | 0.01 |
| dma | 4.20 | 2.80 | 3.01 | 1.68 |
| publish | 4.30 | 0.64 | 0.66 | 0.88 |
| displaced_free | 0.77 | 0.70 | 1.26 | 0.00 |
| inval_tail | 0.10 | 0.05 | 0.03 | 0.02 |
| **total** | **32.13** | **4.22** | **5.09** | **2.97** |

Reading: at 31 GB/s the governor holds offered load at admission
(admit_wait 21.8 ms = honest backpressure, the designed shape); in-pipe
residence ≈ dma + publish + displaced_free — fully attributed, no
hidden term. The psync rows' totals (4.2/5.1 ms) ≈ their clat: the
pipeline is NOT the limiter there; the per-op round trip is.

### 3.3 The libaio-shim lane finding (engagement instrument caught it)

fio libaio 1M/4M rows under the shim show `ipc_ops_write = 0` with
sessions bound: `screen_iocb` (crates/squeezefs-preload/src/aio_glue.rs)
sends any iocb with `nbytes > slab` to the kernel lane — **one async
ring op = one slot, no chunking in v1.1 by design** (a multi-slot op
would hold slots hostage across an async completion). So libaio ≥1M
rows "through the shim" are kernel-path rows (labeled `path=shim`,
engagement INVALID — the runner's charter-rule-4 gate did exactly its
job). Consequence for il benchmarking: **large-bs libaio rows do not
measure the ring**; use psync/sync engines (which chunk) or bs ≤ slab.
Priced here: EXA-shape il ≡ kernel (31.3 ≈ 31.3 — zero shim tax on the
passthrough), and the sync engines show the ring winning (§3.1).

### 3.4 Read side (single pass; over the 32×8g prefilled set)

| Row | GB/s | clat mean |
|---|---|---|
| EXA read shape, kernel (libaio 1M qd8 nj32) | 21.04 | 12.7 ms |
| EXA read shape, il | 21.08 | 12.7 ms |
| psync 4M nj32, kernel | 20.85 | 6.4 ms |
| psync 4M nj32, il | 20.09 | 6.7 ms |

**Reads pin at ~21 GB/s on every shape and both paths** while writes
reach 31+ on the same venue — the read plateau is the remaining true
term. §6.2 adjudicates it against the raw READ ceiling (44–45 GB/s)
and names the mechanism from the read-path counters.

## 4. Rung 2 — dimension sweep (qd × bs × njobs)

gap_probe_write.job, 20 s + 5 ramp per cell, `$M/gap{8,16,32}`,
loop-rewrite fill labeled per row (fill % grows across the sweep —
sweep = classifier, not headline; best/worst re-confirmed at 60 s
sustained). libaio ≥1M ⇒ kernel-lane effective (§3.3). The full grid
(GB/s):

| bs, qd → | qd1 | qd4 | qd8 | qd32 |
|---|---|---|---|---|
| **nj8** 1M | 19.85 | 26.03 | 25.76 | 21.54 |
| **nj8** 4M | 19.68 | 23.74 | 21.73 | 24.90 |
| **nj16** 1M | 26.60 | 30.03 | 29.57 | 29.55 |
| **nj16** 4M | 25.56 | 28.44 | 29.25 | 29.31 |
| **nj32** 1M | **33.80** | 30.14 | 30.24 | 30.11 |
| **nj32** 4M | 29.49 | 29.76 | 29.66* | 30.09 |

(*qd8_bs4M_nj32 from the run log.) 60 s sustained confirms: best
`qd1_bs1M_nj32` = **31.34 GB/s** (clat 0.96 ms); worst `qd1_bs4M_nj8`
= **19.68 GB/s** (clat 1.55 ms — identical to its 20 s cell: stable,
not noise).

**Classifier verdict:** throughput is a pure function of OFFERED
CONCURRENCY (streams × depth) against the per-op ACK chain — every
losing cell is Little-closed (e.g. worst cell: 8 × 4 MiB / 1.55 ms =
20.6 GB/s predicted vs 19.68 measured; qd4 recovers nj8 to 26; nj32
saturates even at qd1). bs is not a classifier at ≥ 1M; njobs and qd
are interchangeable routes to the same inflight-bytes product; the
plateau ≈ 30 GB/s once inflight ≳ 32 MiB. **Bottleneck class: per-op
latency chain, NOT an FS bandwidth term.** Phase residence agrees: the
worst sustained cell's pipeline is FASTER per block than the best's
(total 2.66 ms vs 5.97 ms) — the pipeline starves, it does not clog.

## 5. Rung 4 — queue-spread audit (ss -ti deltas)

Per-connection nvme-tcp byte deltas across the 60 s EXA rows
(~600 established connections to the 4 nodes, `nvme connect` multi-queue):

* write row: n=645 sending connections, mean 3.19 GB, max 7.82 GB,
  **CV 0.61** — spread across both NICs and all 4 targets, no
  single-connection concentration.
* read row: n=587 receiving connections, mean 2.29 GB, max 4.49 GB,
  **CV 0.46** — same shape.

Submission concentration is NOT the gap: the daemon fans both
directions wide (the ss audit closes rung 4 with "healthy").

## 6. Addenda — the two residues, adjudicated

**6.1 File-sharing discriminator — REFUTED.** elbencho geometry (32
workers over 16 SHARED files, offset-split halves; fio psync 4M ×32,
60 s + 10 ramp): kernel **25.49 GB/s** (clat 4.97 ms) vs the 32-file
rows' 25.26/25.18; il **29.58** vs 30.01/30.48. **Per-inode sharing
costs ≈ 0 at 2 threads/file** — the elbencho reference rows'
additional shortfall (17.8 GiB/s on nominally this geometry, same
store, same day) is instrument-attributed (not reproducible with fio
at matched geometry; mechanism not decomposed here — candidates:
elbencho's worker loop/accounting; its rows remain informal per the
instrument law).

**6.2 Read plateau probes + raw READ ceiling — THE TRUE GAP.**
Kernel-path probes over a fresh 32×8g prefill, then the
non-destructive raw read rows (FS idle):

| Row | GB/s | clat | read_amp (dev÷user) |
|---|---|---|---|
| FS libaio 1M qd8 nj32 (plateau repro) | 21.47 | 12.5 ms | **1.048** |
| FS libaio 1M qd32 nj32 | 18.13 | 57.4 ms | **1.476** |
| FS libaio 4M qd16 nj32 | 18.16 | 111 ms | 1.478 |
| FS libaio 1M qd32 nj16 | 18.88 | 27.6 ms | 1.361 |
| **RAW read ceiling** (4m qd16, 8 jobs/dev × 8 devs) | **45.04** | 93.6 ms | — |
| **RAW read, FS-matched shape** (1M qd8, 4 jobs/dev × 8 devs = nj32) | **44.02** | 5.98 ms | — |

The FS read path delivers **0.49× the raw ceiling at the identical
shape** (21.5 vs 44.0), and MORE client depth makes it WORSE, not
better (qd32: 18.1 GB/s at amp 1.48). Mechanism, from the row deltas
(`a2_rd_qd8_1m` / `a2_rd_qd32_1m` stats_delta.json):

* **Whole-block device-true serves with no pipeline:** 240k hot-block
  misses ≈ 5,000 block-fetches/s × 4 MiB = **20.9 GB/s = the
  plateau**. `prefetch_issued` = 128 TOTAL against 1.15 M O_DIRECT
  requests (`prefetch_foreground_waits` 2.1k) — the R2 pipeline never
  engages on this shape: 256 GiB ≫ the 128 MiB hot-tier budget, the
  scan-resistant governor correctly refuses admissions
  (`read_admission_wasted_bytes` 586 GB of tier churn at qd8), and
  governor-denied O_DIRECT misses ride direct-drive — each stream's
  next block waits a full ~4 MiB fabric round trip. Read concurrency
  is therefore ≈ streams-with-distinct-blocks, NOT client qd. (The
  4k face of this same composition was the 2026-07-29 read-saturation
  campaign's field signature; this is its 1M/4M face, quantified
  against a same-shape raw reference.)
* **Deep qd breaks the block cohort:** at qd8, 668k of ~960k ops are
  singleflight waiter serves (amp 1.048 — the 4-ops-per-block cohort
  amortizes); at qd32 waiter serves drop to 517k and **amp rises to
  1.476** — concurrent same-block fetches escape the cohort window
  and the device reads half the set twice. Deeper client depth
  converts amortization into re-fetch.

`nvme-tcp` RX also pays the kernel's one-copy-per-read-byte (the
near-zero-copy census posture) — but the raw rows pay the same copy
and still do 44 GB/s, so the copy is NOT the plateau term; the
fetch-concurrency composition above is.

**Pass-bound amp caveat (a2_prefill):** the prefill row printed
write_amp 0.506 — a pass-bound row's diskstats window closes at fio
exit while the ACK-before-DMA pipeline is still draining the tail; amp
on pass-bound rows must be read after settle (time_based sustained
rows measured amp 1.11–1.16 with the device keeping pace at
~30 GB/s). Canon README updated implication: quote amp from sustained
rows only.

## 7. Artifacts & reproduction

* Rows: `/scratch/tmp/fio_gap/<label>/` (generated .job, fio JSON,
  stats before/after/delta, meta.json) + `<label>.{ss0,ss1,iostat}`;
  addenda logs `/scratch/tmp/fio_gap_addendum{,2}.out`; ladder log
  `/scratch/tmp/fio_gap_ladder.out`.
* Ladder: `.agents/fio-gap/fio_gap_ladder.sh` + `fio_gap_addendum.sh`
  (shared-file discriminator) + `fio_gap_addendum2.sh` (read probes +
  raw read ceiling), analyzers `phase_residence.py` / `ss_spread.py` —
  local campaign artifacts (`.agents/` is gitignored); every
  incantation reproducible from `tests/fio/` + this note (client
  copies at `/scratch/tmp/fio_canon/`, `/scratch/tmp/fio_gap_*.sh`).
* Journal: `/scratch/tmp/agent_runs.log` (`fio-gap agent:` +
  `fio-row:` lines, START/DONE per row with numbers). Standing dev
  pair (`b4edafc`) restored and store settled after every leg
  (verified 19:38:43Z).

### Venue-hygiene finding (standing, from the r1_fresh_il artifact)

A 256 GiB `rm` immediately before a fresh row collapsed it to 2.5 GB/s
with a CLEAN pipeline (total 3.0 ms/block — same as the healthy fresh
row): the rm's frees ride `meta_kv_pending_free` parking (14,025
parked) and release on checkpoint cadence AFTER the reclaim-queue
settle reads 0 — the row then eats the digestion (30.6k reclaim
commands, R5 Red ×4, 9.1k parked-gate self-flushes mid-row).
**Settle discipline must outlast pending-free release, not just
`block_free_reclaim_queue_bytes = 0`** (watch `meta_kv_pending_free`
and `mem_budget_level` too). The row is labeled INVALID-venue and
excluded from verdicts; kernel-path fresh (valid) = 28.6 GB/s.

## 8. THE GAP, NAMED

1. **Write side: there is no FS gap — it was instrument shape.** The
   EXA/Lustre validation shape (fio libaio 1M qd8 nproc-wide) drives
   the FUSE kernel path to **~31 GB/s sustained, 1.20× the recorded
   26.1 GB/s raw write ceiling**, and the il ring turns sync 4M
   writers from 25 GB/s (latency-chain bound, clat ≈ 5 ms) into 30+
   GB/s (ACK at sever). Every sub-30 write cell in the sweep is
   Little-closed by offered concurrency (§4); per-inode sharing was
   priced at ≈ 0 (§6.1). The historical "FS ~20 GB/s" write rows were
   an instrument artifact (elbencho psync shapes + elbencho-specific
   shortfall — §6.1).
2. **Read side: the true gap — FS reads are 0.49× the raw ceiling at
   the identical shape (21.5 vs 44.0 GB/s), evidence chain §3.4 +
   §6.2:** beyond-budget O_DIRECT streams ride governor-denied
   direct-drive with the R2 pipeline never engaging (prefetch_issued
   128 vs 1.15 M ops), so read concurrency collapses to
   streams-with-distinct-blocks and each stream waits a whole-block
   fabric RTT; deeper client qd makes it WORSE by breaking
   singleflight cohorts (re-fetch: read_amp 1.05 → 1.48 from qd8 →
   qd32). This is the 1M/4M face of the 2026-07-29 read-saturation
   composition, now quantified against a same-shape raw reference.
3. **Instrument law going forward:** field rows are fio via
   `tests/fio/` (this campaign's directive); elbencho rows remain
   informal. il rows must use sync engines or bs ≤ slab until a
   libaio ring-chunking decision is taken (v1.1 keeps one-op-one-slot
   by design, §3.3).

## 9. Targeting recommendation

* **The kill campaign is the read-side cold-stream fetch pipeline:**
  governor-denied/beyond-budget sequential streams need a
  ledger-invisible READ-AHEAD lane (the transient-stream window
  admission exists since 2026-07-29 — the missing piece is issuing
  pipelined whole-block fetches on classified streams WITHOUT tier
  publication, direct-drive-fed, so a stream's next block is in
  flight while the current one serves; target ≥ 2 blocks in flight
  per stream ⇒ ~2× the plateau on this venue) + a cohort-stability
  fix for deep-qd same-block concurrency (the qd32 amp-1.48
  re-fetch). Acceptance instrument: §6.2's matched-shape raw read
  row (44.0 GB/s) and `tests/fio/gap_probe_read.job`; success = FS
  read ≥ 0.8× raw at the EXA shape, read_amp ≤ 1.05 at qd32.
* **Adopt the sibling numa-affinity candidate** (its +23.7% wril rides
  the same physics this campaign measured on the il sync lane).
* **Optional il economics item:** libaio ring chunking (multi-slot
  async ops) only if large-bs libaio il workloads matter in the
  field; otherwise the lane split stays documented (§3.3).
