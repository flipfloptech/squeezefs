# 2026-08-02 — Post-reset baseline: the reset-v4 canonical table (dev tip `1b936bf`)

**VENUE EPOCH: reset-v4** — USER-APPROVED cluster reset executed
2026-08-01T~15:00Z via the patched `/scratch/tmp/cluster_reset_v3.sh`
(the window's only destructive act). This table is the **canonical
baseline every subsequent campaign brackets against**; the previous
epoch (reset-v3, five campaign windows old) had aged to ±13 %
same-binary swings (`.benchmarks/2026-08-02-read-copy-count.md` §2/§6),
which is what this window retires. Branch `docs/post-reset-baseline`
(docs-only, **unmerged — the orchestrator merges**).

## 1. Venue (labeled once — the epoch record)

* **Client:** squeeze-test / memp-s3ds-aqs-37 (32 CPU, 2× Xeon Gold
  6426Y, 2 NUMA nodes, dual ConnectX 200GbE), out of production.
* **Substrate (reset-v4 = the reset-v3 recipe, freshly rebuilt):**
  nullblk 4-wide over **nvme-tcp** — mds0/mds1 memory-backed null_blk
  meta (8 GiB each → `/dev/nvme0n1`, `/dev/nvme2n1`), DATA_ON_MDS=1 ⇒
  4 nodes × OSS_NAMESPACES=2 × OSS_NULLB_MB=49152 = **8 × 48 GiB data
  namespaces** (`/dev/nvme{4,6,8,10,12,14,16,18}n1`, one subsystem
  each, both fabric paths, round-robin iopolicy). Format: cache-less,
  4 MiB blocks, meta-slots 8, no compression/encryption, 384 GiB.
* **Binary (the NEW standing pair):** dev tip **`1b936bf`** rocky8
  container pair (KD-7 verified, glibc ≤ 2.28) — includes the
  read-copy-count campaign (read copy ledger, E-IL1/E-IL2, NT
  read-serve default-on with arena exemption) and `tests/perf_remote.sh`.
  Retained at `/scratch/tmp/{squeezefs,libsqueezefs_il.so}.tip1b9`;
  deployed at the standard names.
* **Daemon posture (uniform across every row):** `--interception
  --allow-other`, `SQUEEZEFS_IPC_MEM_MAX=8192` (the verified il-session
  posture — never engagement-INVALID), all other knobs default.
* **Instrument:** fio-3.36 via `tests/fio/run_fio_row.sh` + the census
  wrapper (`/scratch/tmp/rcc/rcc_row.sh`: uncore
  `cas_count_{read,write}` DRAM sampler, mpstat, copy-ledger deltas).
  Every row: 60 s + 10 s ramp (headline flat legs 120 s), cold = fresh
  remount, settle hygiene between rows/deploys (reclaim queue 0 AND
  `meta_kv_pending_free` 0 AND mem level 0, ×3 at 1 Hz). NUMA fan-out
  2 nodes; **njobs 32 over 16 files = 2 sequential readers/file** (the
  fill recipe: `fresh_write_pass` nj32 size 8g ⇒ 16 × 8 GiB, written
  at **30.99 GB/s** onto the empty store — first bytes of the epoch).
  Counter totals ramp-inclusive (≈ 1.16–1.19× the fio window).
* **Raw ceilings quoted (source of record):** write **49.7 GB/s**
  (`.benchmarks/2026-07-31-raw-write-ceiling-resweep.md`, NIC-line-rate
  bound), read **44–45 GB/s** (same re-sweep family; client RX kernel
  copy + softirq class).

## 2. THE BASELINE TABLE (reset-v4, `1b936bf`, medians of stated reps)

| row | shape (job / engine / bs / qd / njobs) | result | clat mean | amp (dev/user) | DRAM B/B raw | ×raw ceiling | reps |
|---|---|---|---|---|---|---|---|
| **read kern qd8 cold** | gap_probe_read / libaio / 1M / 8 / 32 | **27.33 GB/s** | 9.79 ms | 0.71 | 10.5 | **0.61** (÷45) | r1 27.32, r2 27.33; flat-120s 27.33 |
| read kern qd8, NT-A0 (`SQUEEZEFS_NT_READ_SERVE=0`) | same | 24.76 GB/s | 10.81 | 0.72 | 12.0 | 0.55 | a0a 24.60, a0b 24.91 — **NT default = +10.4 % attributed on pristine** |
| **read kern qd32 cold** | same, qd 32 | **22.40 GB/s** | 46.45 | 1.13 | 13.0 | 0.50 | r1 (A0 20.73 ⇒ NT +8.1 %) |
| **read il qd8 cold** | gap_probe_read / psync / 1M / 32 jobs, shim | **34.11 GB/s** | 0.98 | 0.58 | 6.6–6.8 | **0.76** | r1 34.18, r2 33.04; flat-120s 34.05; engagement 1.157–1.160 |
| **write kern fresh** | exa_write_bw / libaio / 1M / 8 / 32, fresh dir | **31.51 GB/s** | 8.41 | 1.16 | 8.8 | **0.63** (÷49.7) | A1 31.21, A2 31.80 |
| **write kern rewrite** | same, same dir | **31.43 GB/s** | 8.44 | 1.16 | 9.0 | 0.63 | B1 31.29, B2 31.56; flat-120s 31.78 — **fresh/rewrite A-B-B-A: PARITY both orders** (the rewrite tax stays closed post-`e2efe52`) |
| **write il rewrite** | exa_write_bw / psync / 1M / 32 jobs, shim | **26.34 GB/s** | 1.24 | 1.17 | 7.3 | 0.53 | r1; engagement 1.170 (psync-instrument row — not the libaio shape; matched-instrument parity lives in `write_matrix`) |
| **rand-4k read cold** | exa_randread_iops / libaio / 4k / 16 / 32 | **260,269 IOPS** | 1.85 | 1.30 | 28 | — | r1 259.9k, r2 262.1k, r3 260.3k (±0.9 %; ledger: `dest_dma` ≡ device bytes — zero-daemon-copy ranged serves) |
| **rand-4k write** | exa_randwrite_iops / libaio / 4k / 16 / 32, onto the base set | **286,705 / 285,518 IOPS** | 1.69 | 1.17 | — | — | r1/r2 (overwrite shape, stated) |
| **600 s mixed soak** | read 1M qd8 nj16 + 8-worker mdstorm + syncfs | § 4 | — | — | — | — | 1 |

Flatness (per-10 s device bytes across the 120 s legs, no decay trend):
rd-kern 159–168 GB/10 s; rd-il 167.9–172.3 (after first partial
sample); wr-rewrite 308–319.

## 3. The rand-4k verdict: **VENUE STATE, not a real regression — closed**

The read-copy-count campaign flagged a bounded −2.8 % on rand-4k cold
(campaign binary vs dev tip, aged reset-v3 venue). On pristine
reset-v4, same-state A-B-B-A (T = `.rpd`/`e2efe52`, the pre-campaign
dev tip binary; C = merged tip `1b936bf`; interleaved cold remounts on
the identical post-randwrite fill):

| leg (order) | IOPS | clat |
|---|---|---|
| T1 | 259,301 | 1.854 |
| C1 | 264,541 | 1.817 |
| C2 | 262,027 | 1.835 |
| T2 | 260,088 | 1.849 |

**C ≥ T on both orders (+1.4 % medians, 263.3k vs 259.7k)** — the
aged-venue deficit does not reproduce pristine; additionally the tip's
absolute 260.3k ±0.9 % (×3) sits inside the historical expectation band
(the ~271k anchor was itself a single early-window rep of a row this
note now shows swings with store state). Verdict: the −2.8 % flag was
**venue-state-coupled, not binary** — no attribution work owed. (The
row's amp 1.30 = window bytes + governed escalations; ledger closure
exact on every rep.)

## 4. Soak (the epoch's wedge-health row)

600 s: kernel read plane (1M qd8 nj16 time_based) + 8-worker metadata
storm + syncfs/10 s on the new standing pair: **27.58 GB/s sustained
for the full 600 s** (clat 4.85 ms, p99 12.4 ms), copy-ledger closure
1.017 over the whole window (`bounce` 0), 8/8 storm workers alive at
stop, wedge indicators (`fuse_op_watchdog_overdue`,
`transport_lease_overlong`, `tpc_lane_redispatches`,
`ipc_sessions_poisoned`, `ipc_descriptor_rejects`,
`write_path_seed_read_bytes`, `patch_edge_rmw_reads`, `fsck_findings`,
`write_pipeline_fence_drops`, `block_double_frees`,
`writer_guard_fenced`) sampled ×21 — every sample identical ALL-ZERO;
no non-kernel D-states after quiesce.

## 5. Bracketing rules for future campaigns (what this table is FOR)

1. Bracket against THIS table's numbers on THIS epoch; a row measured
   after further store aging must carry its own same-state control legs
   (the A-B-B-A-on-aging-store rule) — reset-v3's ±13 % drift is the
   cautionary record.
2. Every row cites instrument/shape/substrate/fill/order per the
   standing labeling law; il rows are INVALID without engagement ≥ 0.90
   under `SQUEEZEFS_IPC_MEM_MAX=8192`; read rows carry the copy-ledger
   closure check; write rows carry amp columns.
3. NT read-serve attribution on this epoch: default-on is worth
   +10.4 % (qd8) / +8.1 % (qd32) over A0 — any campaign touching read
   serves re-runs the A0 leg before claiming wins.
4. Headroom ledger at this epoch: read kern 0.61× / il 0.76× of the
   44–45 raw read ceiling; write 0.63× of 49.7 — the remaining gaps are
   interface-class per the read copy ledger (RX + commit copies) and
   the write op-ACK chain (serve-decomposition §4.3).

## 6. Client state

New standing pair = `1b936bf` at the standard names (retained
`.tip1b9`; outgoing `b4edafc` retained `.b4edafc.prev`); mount healthy
+ armed at SESSION END. Filesets left: `base/` (16×8g, post-randwrite),
`w1/`, `w2/` (32×1g each), `mdstorm_rpd/` (normal bench artifacts).
Artifacts: `/scratch/tmp/rcc/rows/` (per-row json/stats/census),
`/tmp/prb_*` (reset log, flat series, soak indicators). No raw-device
writes; no storage-node changes beyond the reset script itself.
