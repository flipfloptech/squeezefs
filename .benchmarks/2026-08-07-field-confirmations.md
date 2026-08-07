# 2026-08-07 — Field confirmations: write-lane fan-out + shim hybrid lane gate (squeeze-test)

Branch `perf/field-confirmations-0807` off dev tip `cc5e4ab1` — a
MEASUREMENT campaign (zero product-code changes; rig fixes only). Deployed
pair: `dist/rocky8/{squeezefs, libsqueezefs_il.so}` at `cc5e4ab1`
(md5-verified both ends, KD-7 same-commit). Full row detail lives in the
two campaign notes' new sections:

* `.benchmarks/2026-08-07-write-lane-fanout.md` **§7**
* `.benchmarks/2026-08-07-shim-hybrid-lane-gate.md` **§4b**

Rigs (committed): `.benchmarks/rigs/2026-08-07-fieldconf-set1-wlanes.sh`
(+ `…-set1-analyze.py`, `…-set1-raw.sh`),
`.benchmarks/rigs/2026-08-07-fieldconf-set2-lanegate.sh`
(+ `…-set2-gate.py`). Artifacts on-host: `/scratch/tmp/fieldconf-0807/`
(`set1/`, `set2/`, plus the two INVALID-row captures
`set1-run1-agingcapture/`, `set2-run1-slabclamp/`).

**Venue** (every row): squeeze-test — 32 CPUs, EL8, kernel 6.19.14-sqz,
real NVMe-oF **TCP fabric**, reset-v5 converged cluster (5 storage nodes ×
[1 meta + 2 × 48 GiB data] memory-backed null_blk namespaces, 2 paths per
subsystem, round-robin); task format = 5 meta + **5 data** namespaces.
Instrument: fio 3.36 (dynamic), engines stated per row (libaio matched per
the fio-engine-policy; psync only where stated). Engagement gates FATAL in
every scripted row.

## Campaign lessons (both encoded into the rigs)

1. **Venue stationarity** — the null_blk data plane is consumable: without
   target resets, legs decayed 27.3 → 18.7 GB/s monotonically and the
   A-B-B-A brackets disagreed (INVALID, kept as the aging capture). The
   creditable Set 1 resets the cluster before EVERY leg
   (`cluster_reset_v4.sh`), and the same-day RAW rows grade the venue:
   raw spread 38.05 GB/s vs the reference epoch's 49.7 — the venue sits at
   ~77 % of the class the 35.31/49.7 references were measured in, so
   absolute-GB/s targets are graded as ratios.
2. **Geometry composition** — the fio-engine-policy arena-slab law
   (`SQUEEZEFS_IPC_ARENA_MB=1024` for bs=1M il aio) and the lane gate's
   derived threshold do NOT stack: a 1 MiB slab clamps the threshold to
   exactly 1 MiB and the strictly-greater law keeps bs=1M on the ring
   (measured, kept as the slab-clamp capture; now a §5 residual in the
   gate note). The confirmation runs the gate rows on the DEFAULT arena
   and uses the big arena only as the OFF arm's ring geometry.

## Combined table

### Set 1 — write-lane fan-out A/B (fio libaio seq-write 1M ×16 jobs qd8, 60 s sustained + ramp, fresh reset+format+dir per leg)

| Row | Result | Engagement / gates |
|---|---|---|
| A legs (derived, `data_write_lanes=6`) | 24.38 / 31.07 / 30.83 / 24.67 → **median 27.75 GB/s** | all 6 lanes move on all 5 devices, every leg |
| B legs (`SQUEEZEFS_NVME_WRITE_LANES=1`, shipped pre-fanout) | 21.71 / 23.85 / 22.40 / 22.58 → **median 22.49 GB/s** | lane 0 only, pinned, every leg |
| **A/B** | **+23.4 % at medians; direction unanimous in all 4 order-alternating adjacent pairs (1.09–1.38×)** | brackets A-B-B-A + B-A-A-B |
| Governor | probes CONVERT only on A (target > base, ups > backoffs on 3/4 A legs); every B leg pinned (target ≡ base, ups ≡ backoffs) | the §6 signature confirmed |
| dma phase mode | B `<=8ms` → A `<=4ms` (one octave down) | — |
| Amplification | dev/user 1.007–1.011, wareq-sz 2.5–3.0 MiB, all legs | write-row columns clean |
| Tripwires / P0 | `data_dma_fence_refusals` 0 ×9 legs; P0 smoke (cp 32 MiB && sync, md5 ×3) clean ×9 | — |
| Armed compose variant (1 leg, labeled) | zc+lanes 25.22 GB/s — inside the A spread; no separable zc term at this variance | all 6 lanes ×5 |
| Same-day raw grading | 1 submitter/dev qd16 = 24.22; 6 submitters/dev qd3 = **38.05 GB/s** | venue at ~77 % of reference epoch |

### Set 2 — hybrid lane gate (armed zc mount, LD_PRELOAD shim, engagement EXACT on every row)

| Row | Result | Engagement / gates |
|---|---|---|
| Threshold publish (derived, default arena) | `ipc_lane_gate_threshold_bytes` = 86,016 | > 0 on a live bound fd ✓ |
| libaio 1M write / read il | 12.58 / **24.06 GB/s** | lane bytes ≡ row (1.0000×), ring bytes 0 |
| psync 1M write / read il | 13.21 / 9.15 GB/s (read = qd1 sync twin, label-only) | lane bytes ≡ row, ring 0 |
| rand-4k psync read il | 167,989 IOPS | ring ops ≡ row ios (3,359,949), lane routes 0 |
| dd bs=1M sticky | routes 1024 ≡ count | ring writes 0 |
| Kernel reference (no shim) 1M read | 26.23 GB/s | labeled class row |
| **A/B 1M read, ON vs OFF(ring @ its best geometry), medians of 3 ON-OFF-OFF-ON-ON-OFF** | **ON 26.42 vs OFF 15.55 GB/s = 1.70×**; ON ≡ kernel reference (1.007×) | per-arm lane/ring gates exact, legs spread < 0.2 % |
| Sustained ON read ≥ 60 s | **26.39 GB/s, thirds 26.38/26.46/25.72 (≤ 2.9 %)** | lane bytes ≡ row |
| Tripwires (every row) | `ipc_sessions_poisoned` 0, `ipc_descriptor_rejects` 0, `transport_lease_overlong` 0 | ✓ |

## Verdicts

**Fan-out (Set 1): CONFIRMED in ratio, venue-limited in absolute.** Seq
write leaves the single-lane per-connection wall on the real fabric:
+23.4 % at medians (27.75 vs 22.49 GB/s), all four adjacent A/B pairs
agree across both bracket orders, engagement exact (6 lanes × 5 devices vs
lane-0-pinned), and the depth governor converts probes ONLY once the wall
is lifted — the exact §6 signature. The absolute "35.31 → 49.7-class"
question could not be re-asked: the venue itself graded ~24 % below its
reference epoch (raw 38.05 vs 49.7), and today's control arm walls at
≈ 22.5 GB/s. Raw-normalized, the fan-out arm delivers 0.73× same-day-raw
vs the control's 0.59×.

**Lane gate (Set 2): CONFIRMED.** The il 1 MiB read gap closes all the
way to the kernel lane's class — gate ON 26.42 GB/s ≡ the no-shim kernel
reference 26.23 (the reference-epoch 49.2-vs-28 gap reproduces as a 1.70×
ON/OFF ratio, above the ≥ 1.5× expectation); rand-4k holds (ring
engagement exact, gate routes 0); the offsetful sticky latch counts
exactly; the threshold gauge publishes live; the 60 s sustained ON row is
flat; zero tripwires end-to-end.

**Hand-offs:** none — no product bug surfaced (no wedge, no tripwire, no
corruption; every umount drained clean across ~14 mount cycles). Two
non-product findings for their owners' notes, both recorded: the venue's
raw class degraded vs the reference epoch (tracked in fanout §7 — worth a
storage-node look before the next absolute-GB/s campaign), and the
slab-clamp composition residual (gate note §5).

**Host left state**: default-posture mount (no env overrides) on
`cc5e4ab1` at `/scratch/tmp/test` (`--daemon --interception --allow-other`,
log `/scratch/tmp/logs/sqz.log`), fresh reset + task-spec 5+5 format,
mount gate + P0 smoke clean, `data_write_lanes=6` (the shipped derived
default), tripwires 0.
