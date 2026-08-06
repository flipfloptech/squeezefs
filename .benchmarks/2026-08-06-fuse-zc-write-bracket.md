# 2026-08-06 — FUSE-zc WRITE-side pricing bracket: default stays OFF

Branch `perf/fuse-zc-write-bracket` (off wave tip `90f55604`). Charter:
the fuse-zc-serve campaign's §6 deferred follow-up — price the armed
mounts' WRITE vehicle (every FUSE_WRITE payload arrives via a
`WRITE_FIXED slot→memfd` bounce extraction: one extra ring round trip,
kernel shmem copy replacing the delivery-time folio copy) and flip
`SQUEEZEFS_FUSE_ZC` default-ON iff armed writes hold ≥ 0.97× control on
all three write-row medians with correctness clean and the read row
confirming ≈ 39.5+ GB/s (the dest-lease precedent: counted field
acceptance before any default flip).

Commits: `a9ce917c` (red: the extraction must carry an engagement
counter pair) · `975452ef` (feat: `fuse3_zc_write_extractions/_bytes` —
counted at the WriteExtract CQE success point, exported through the
stats inode; failed extractions stay on `fuse3_zc_fallbacks`) ·
`ae19864b` (the A-B-B-A rig) — the counter existed BEFORE any row ran,
because an engagement-invisible write row is invalid by repo law.

**Verdict up front: DO NOT FLIP — the default stays OFF.** Sequential
1M writes are at par (0.998× median), but **rand-4k write loses 6.4 %
at median with BOTH brackets losing (−7.3 % / −5.5 % — order-
independent)** and the **durable (fsync_on_close) row loses 9.1 % at
median (armed never reached the ~24 GB/s mode control found in 3 of 5
control legs)**. Two of three write rows breach the > 3 % loss line.
The read win is confirmed intact on this binary (armed 40.09 GB/s).
`SQUEEZEFS_FUSE_ZC=1` remains the counted, engagement-exact read-side
lever for read-dominated mounts; write-heavy mounts should not arm it.
This is the decision point the charter reserved for the orchestrator:
default-ON now requires either a cheaper WRITE vehicle (see §6) or a
ruling that the read win prices the write tax for the fleet posture.

## 1. Venue / instrument / deviation

* Host `squeeze-test`: EL8, **6.19.14-sqz** (kmbuf+zc series booted),
  32 CPUs, 2×200 GbE, nvme-tcp fabric — meta on 5×8.6 GB namespaces
  (`nvme{0,2,4,6,8}n1`), data on 5×51.5 GB (`nvme{10,12,14,16,18}n1`).
* Binary `ae19864b` (rocky8 pair, md5-verified both ends;
  `--version` carries the checkout commit). Includes the P0
  fsync-writeback tail-loss fix `ebf3c3f8`.
* Instrument: fio 3.36 libaio `direct=1`, `time_based` 60 s + 10 s
  ramp, group_reporting, JSON. Substrate/instrument stated per the
  standing rule. Rig: `.benchmarks/rigs/2026-08-06-zc-write-bracket-rig.sh`
  (FATAL require-mount, arm-proof, correctness and engagement gates on
  EVERY leg); analysis `.benchmarks/rigs/2026-08-06-zc-write-bracket-table.py`.
* **Stated deviation from the charter shape**: seq/durable rows run
  `size=4g` × 16 jobs × nrfiles=8 (**64 GiB fileset**, 512 MiB/file),
  not `size=8g` (128 GiB): the 240 GiB volume holds 112 GiB free beside
  the pinned 128 GiB `exa_perf` read set. At ~21–24 GB/s × 70 s the row
  makes ~23 passes over the fileset either way — the steady state is
  the CoW overwrite/displaced-free regime in both shapes.
* Fresh mount per leg; fileset `rm -rf` + reclaim/space settle
  (`block_free_reclaim_queue_bytes == 0`, df recovered) between rows.
* Absolute-level note: the control seq-write level here (~21.6 GB/s)
  sits below the loose "writes ≈ 34 GB/s" reference class from earlier
  campaigns (different fileset regime, 54 %-full volume, kernel path).
  The bracket is A/B-relative on one fixed venue; the absolute is not
  this campaign's claim.

## 2. Correctness (every leg, FATAL — all green)

Per write leg on its own mount posture: O_DIRECT+fsync 32 MiB
write→readback md5 (O_DIRECT AND buffered) + the **P0 shape**
`cp 32MiB f && sync f` → md5 (buffered AND O_DIRECT) ×3. Result:
**4/4 legs clean, 12/12 P0 trials clean** — the `ebf3c3f8` fix holds
under both postures (the pre-fix field rate was 5–7/10 corrupt).
`fuse3_zc_fallbacks` and `fuse3_zc_slot_payload_skips` were **0 on
every leg**; control legs' zc ledger identically 0.

## 3. The write rows (A-B-B-A, 60 s sustained, engagement exact)

Engagement: armed rows' `fuse3_zc_write_extract_bytes` ≥ 95 % of the
fio window (in fact ≈ the ramp-inclusive volume: e.g. W1/seqwr 1586 GB
vs 1288 GB window × 7/6 + 64 GiB layout), extractions ≈ the row's WRITE
count; control rows identically 0. `amp` = data-namespace device bytes
written ÷ ramp-inclusive user bytes (`/proc/diskstats` deltas).

### Row 1 — seq write 1M (16 jobs × qd8): **PAR (0.998×)**

| leg | zc | GB/s | p50 ms | p99 ms | box busy % | daemon CPU s | ms/GB | extractions | extract GB | amp |
|----:|---:|-----:|-----:|-----:|----:|----:|----:|----:|----:|----:|
| W1 | 1 | 21.448 | 0.85 | 76.0 | 51.0 | 825.6 | 549.5 | 1,512,952 | 1586.4 | 1.06 |
| W2 | 0 | 21.563 | 0.95 | 73.9 | 51.8 | 812.9 | 538.4 | 0 | 0 | 1.07 |
| W3 | 0 | 21.626 | 0.91 | 73.9 | 50.4 | 794.7 | 524.6 | 0 | 0 | 1.06 |
| W4 | 1 | 21.672 | 0.86 | 73.9 | 51.8 | 831.5 | 547.8 | 1,524,648 | 1598.7 | 1.06 |

Medians: armed **21.560** vs control **21.595** → **0.998×**; brackets
0.995× (W1/W2) and 1.002× (W4/W3). Daemon CPU/GB +3.4 % armed (the
extraction round trip priced per 1 MiB op — small against the DMA
path). p50 slightly BETTER armed (0.85 vs 0.91–0.95 ms).

### Row 2 — durable seq write (fsync_on_close=1): **LOSES 9.1 %**

Includes the declared adjudication pair D7/D8 (see below).

| leg | zc | GB/s | p50 ms | p99 ms | box busy % | daemon CPU s | ms/GB | extractions | extract GB | amp |
|----:|---:|-----:|-----:|-----:|----:|----:|----:|----:|----:|----:|
| W1 | 1 | 22.153 | 0.77 | 66.8 | 46.0 | 754.3 | 485.3 | 1,453,249 | 1523.8 | 0.98 |
| W2 | 0 | 24.371 | 1.35 | 66.3 | 56.4 | 872.4 | 510.2 | 0 | 0 | 1.05 |
| W3 | 0 | 21.685 | 0.82 | 70.8 | 45.9 | 731.0 | 480.5 | 0 | 0 | 0.98 |
| W4 | 1 | 21.801 | 0.79 | 70.8 | 46.4 | 756.6 | 494.9 | 1,430,658 | 1500.2 | 0.98 |
| D7 | 0 | 24.109 | 1.17 | 70.8 | 55.0 | 852.5 | 504.5 | 0 | 0 | 1.05 |
| D8 | 1 | 21.914 | 0.77 | 67.6 | 50.0 | 800.8 | 521.0 | 1,514,190 | 1587.7 | 0.98 |

The original bracket DISAGREED (0.909× W1/W2 vs 1.005× W4/W3 — W2 read
as a possible single-order excursion), so one extra B-then-A pair ran
as **declared adjudication** (multi-run discipline rule 2: signature
gathering, labeled, never silently folded): D7 control reproduced the
24-mode (24.109) and D8 armed did not (21.914). Combined —
**control {21.685, 24.109, 24.371} median 24.109; armed
{21.801, 21.914, 22.153} median 21.914 → 0.909×**. The 24-mode is
bimodal on the control side (2 of 3 legs) and was **never reached by
any of the 3 armed legs** — the honest reading is a mode armed mounts
cannot enter, not noise (its signature: box busy 55–56 % vs 46–50,
p50 1.17–1.35 ms vs 0.77–0.79, per-op `write_transport_phase_ns`
queue_wait 165 µs vs 100–111 — a deeper delivery pipeline).

### Row 3 — rand-4k write (32 jobs × qd8): **LOSES 6.4 %, both brackets**

| leg | zc | IOPS | GB/s | p50 ms | p99 ms | box busy % | daemon CPU s | ms/GB | extractions | extract GB |
|----:|---:|-----:|-----:|-----:|-----:|----:|----:|----:|----:|----:|
| W1 | 1 | 349,198 | 1.430 | 0.34 | 5.1 | 76.9 | 1361.0 | 13,593 | 24,173,694 | 99.0 |
| W2 | 0 | 376,802 | 1.543 | 0.33 | 4.6 | 75.3 | 1324.5 | 12,259 | 0 | 0 |
| W3 | 0 | 370,792 | 1.519 | 0.33 | 4.8 | 75.3 | 1328.0 | 12,490 | 0 | 0 |
| W4 | 1 | 350,408 | 1.435 | 0.34 | 5.1 | 76.9 | 1360.3 | 13,539 | 24,212,031 | 99.2 |

Medians: armed **349,803 IOPS** vs control **373,797** → **0.936×**;
brackets **0.927×** (W1/W2) and **0.945×** (W4/W3) — the loss is
order-independent. Per-op daemon CPU **+9.2 %** (13.57 vs 12.37 s/GB
median); p99 **+9 %** (5.1 vs 4.6–4.8 ms). Device-write ratio ~0.45–0.51
of user bytes on both sides (extent-overlay/compression absorbing the
4k stream — unchanged by the vehicle). mpstat: armed runs **+3 pts
%sys** (33.8–34.0 vs 30.8–31.0) at LOWER delivered IOPS — kernel-side
ring work, not daemon userspace.

## 4. Loss attribution (the phase ledger)

* **The per-op vehicle term is the extraction round trip.** On an armed
  queue a FUSE_WRITE delivery is not dispatchable until its
  `WRITE_FIXED(slot → memfd)` CQE lands: the §5.4 lease then rides the
  bounce mapping. Copy COUNT is parity (kernel shmem copy replaces the
  delivery folio copy) but the copy+ring hop is now **serialized ahead
  of dispatch** on the queue worker instead of riding the kernel's
  COMMIT path. At 1 MiB the added per-op cost vanishes into the DMA
  pipeline (row 1 par); at 4 KiB it is the dominant per-op term —
  −6.4 % IOPS, +9 % daemon CPU/op, +3 pts %sys, p99 +9 %.
* **Daemon-visible phases stay ≈ par** (`write_transport_phase_ns`
  dispatch_lag 225 vs 223–225 µs, queue_wait 165–170 µs both sides;
  per-op transport_total +2–3.5 % armed), because the extraction wait
  sits BEFORE the delivery timestamps — the cost lands in the box %sys
  column and the delivered-op rate, which is exactly where the table
  shows it.
* **The durable row's 24-mode**: `fsync_on_close` makes throughput
  sensitive to delivery-side concurrency (close-time barriers batch
  against in-flight deliveries). Control legs intermittently hold a
  deeper pipeline (the queue_wait-165 µs / busy-56 % signature);
  armed legs' extraction serialization never entered it in 3 tries.
  `write_pipeline_phase_ns` (block path) shows no armed-side dma/publish
  regression on seqwr — the term is transport-side, not pipeline-side.
* rand-4k writes ride the extent-overlay path (block
  `write_pipeline_phase_ns` deltas are zero on that row by design), so
  the pipeline instrument cannot name the term there either — the
  attribution rests on the CPU columns + delivered rate + the
  extraction ledger, all of which move together and only on armed legs.

## 5. Read sentinel (regression gate — the win holds)

Standing cold recipe over `exa_perf` (read bs=1M ×16 jobs qd8, fresh
mount, 60 s): armed **40.085 GB/s** (`read_zc_serve_bytes` 2807.7 GB ≈
ramp-inclusive volume, 2,677,585 zc replies, 0 fallbacks; box busy
65.8 %, daemon 235.7 s) vs control **27.842 GB/s** (zc ledger 0, busy
80.2 %, daemon 843.5 s) — matches the serve campaign's 39.84/39.92 vs
27.95/27.33 within run noise, and clears the ≥ 39.5 flip-gate leg on
this binary. The read side is NOT the blocker.

## 6. Decision + follow-ups

* **Default stays `off`** (`src/env_knobs.rs` untouched). The decision
  rule was ≥ 0.97× on all three write-row medians; measured 0.998×
  (seqwr) / **0.909×** (dur) / **0.936×** (rand-4k). No contract test
  changes needed — nothing pinned a default flip.
* What a future flip needs (either): (a) a cheaper armed WRITE vehicle —
  e.g. dispatch-before-extraction for shapes whose handler consumes the
  payload asynchronously, extraction batching across ents per drain
  pass, or a kernel-side change letting WRITE payloads ride the kmbuf
  on zc queues (the series' `can_zero_copy_req` has no per-request
  opt-out today — the §1 central finding); or (b) an explicit fleet
  ruling that read-dominated mounts arm it operationally (the knob is
  live, loud, and engagement-gauged for exactly that).
* The engagement pair `fuse3_zc_write_extractions/_bytes` ships
  regardless — armed write rows are now ledger-visible (this bracket
  would have been invalid without it).
* Artifacts: `squeeze-test:/scratch/tmp/zcw-bracket-1/` (per-row fio
  JSON, stats before/after, /proc/stat + daemon CPU + diskstats
  snapshots, mpstat/pidstat tapes, mount logs), run log
  `/scratch/tmp/zcw-bracket-1.log` + `/scratch/tmp/zcw_dur_extra.log`.
* Field host left mounted on binary `ae19864b` in the resulting
  **default posture (zc off)**: `fuse3_zc_negotiated=0`,
  `fuse3_zc_write_extractions=0` verified post-remount.
