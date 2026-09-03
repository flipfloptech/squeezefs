# 2026-09-03 — zc-ahead-yield: the ahead lane on FUSE-zc streams — a WASH at the field shape (governed), a −35 % LOSS pinned, an +84 % / +53 % WIN on a single low-qd stream; not merged

**Branch:** `perf/zc-ahead-yield` — two commits from 2026-09-01
(`ea0cd3ce` test, `4c0ebf07` fix), rebased clean onto dev `eb699483` as
`a3c1920a` / `d6c06c10`; rigs `aaee7fae` / `2de47a8c`. **The lever:**
`src/routing.rs`'s single-block read arm passes `pipeline_touch` its
yield operand as `dest_leaseable && !zc_geometry` instead of
`dest_leaseable || zc_geometry` — a 1 MiB FUSE-zc serve no longer stamps
`StreamLanes::dest_lease_ms`, the 2 s stream-wide yield that makes BOTH
speculative issue arms (the R2 pipeline's top-up and the read-lane's
ahead fetch) decline. Dest-lease-only traffic (a kernel ent dest with no
zc handle) keeps the yield: dest-lease contract 2
([`.benchmarks/2026-08-06-read-dest-lease.md`](2026-08-06-read-dest-lease.md)
§2, `tests/read_dest_lease_tests.rs`) is unchanged. **Its test:**
`tests/fuse_zc_serve_tests.rs` contract 5 — the zc-only shape and the
field composition (dest-lease hint AND a zc handle, which is what the
handler mints on every zc-armed ring slot: `src/fuse_client.rs`
`read_hint.dest_lease = dest.is_some() && !dest_arena && lease_enabled`
beside `zc_serve = conn.zc_armed() && slot.is_ring()`) must both issue
ahead/R2 fills for blocks past the demand window. **Premise on dev
re-verified:** under the R-2 ⊕ R-3 dispatch law a cold 1 MiB READ never
fuses (the fusion ceiling is `payload/8` = 128 KiB) and takes arm (c) —
the full handler → this router site, which is still the ONLY site that
stamps the yield (the il ring path and the multi-block arm pass `false`);
the windows the lever makes warm are then served by R-2's inline probe
(`read_fast_probe` → `ring_read_lane_touch(missed = false)` continues
the lane without stamping). **Binaries:** A = the box's own
`/scratch/tmp/squeezefs` + `libsqueezefs_il.so` (**`c985fa8c`**, `release`,
clean — the `.kvmap`-suffixed copies the brief named do not exist on the
box); B = `task build:rocky8` of **`d6c06c10`** (`release`, clean, glibc
≤ 2.28) + its own shim (KD-7). Both arms thin-LTO `release`. Every mean
below is an exact `Δsum_ns / Δcount` from pre/post `.stats` snapshots.

**Verdict up front.** The fix was a correct diagnosis of the field's
1 MiB kern row — on the control `prefetch_issued = read_lane_fetches = 0`
because zc geometry stamps the dest-lease yield — and a wrong prediction
of what un-yielding buys at that shape. **At the field shape (24 × qd16
1 MiB, cluster_reset_v4 nvme-tcp) the lever is a WASH governed and a LOSS
pinned**: kern **35.56 / 36.12 vs 35.31 / 36.19 GiB/s** (both orders,
+0.7 % / −0.2 %), il-mode **40.50 / 41.21 vs 40.90 / 40.70** (−1.0 % /
+1.3 %), sustained 60 s kern **37.79 vs 38.27** (−1.3 %) / il-mode
**38.17 vs 37.76** (+1.1 %), clat par to the µs, kern rand-4k **512.9 k /
511.7 k vs 510.0 k / 510.6 k** — for **+25…+49 % daemon CPU per op on
the rows where it engaged** (167 / 140 vs 112 µs/op; sustained 122 vs
95). Engagement was exact and self-limiting: the 2026-08-05 engage-
governor probed and RETREATED on every row (`read_lane_depth_probe_ups`
/ `_backoffs` 4/4, 2/2, 5/6, 5/5, 3/3), and pinning the depth to prove
it timid did the opposite — **`SQUEEZEFS_READ_LANE_DEPTH=2/4/8` →
23.8 / 22.7 / 22.8 GiB/s vs 36.6 governed on the same binary (−35 %)**,
clat 15.7–16.4 ms, `read_lane_hold_evicted_unconsumed` 1,116–1,978 (the
spiral detector firing), the demand windows that joined an ahead fill
waiting `sf_wait` 13.4 ms against a 5.4 ms zc fetch. The fabric is the
wall: the demand zc fetch's `device_cq` is 5.2–5.5 ms and the ahead
fill's `dev_service` 4.2–5.2 ms on both arms — the same saturated queue
seen from two legs, and every pooled 4 MiB fill is bandwidth the 384
in-flight demand reads already had. **But the premise itself is real,
and large, where the client's own concurrency does not saturate the
fabric**: one job at qd1 **2.45 / 2.50 → 4.57 / 4.55 GiB/s (+84 %, clat
384 → 201 µs)**, one job at qd4 **5.05 / 4.71 → 7.53 / 7.38 GiB/s
(+53 %, clat 771 / 826 → 515 / 525 µs)**, both orders, via the R2
pipeline (`prefetch_issued` 26.9 k / 47.7 k per row, 95 % of the windows
served inline by R-2's fast dispatch) — at 3.6–4.6× the daemon CPU per
op. **Not merged** (AGENTS.md: a wash is not displaced, and the
acceptance was "GiB/s up on both modes with clat down" at the field
shape). The correct design is not "zc yields" or "zc does not yield" but
a yield that reads the stream's own in-flight depth — filed in §7. The
fix and its test stay on the branch as the record; the rigs and this
note are what should reach `dev`. **Tier: measured-real.**

---

## 1. Instruments, venue, tier

| | |
|---|---|
| **Instrument** | fio 3.36 (`/usr/bin/fio` on the box), the FIELD job files verbatim: `read_BW.job` (libaio `direct=1`, 24 jobs × `size=8g`, 1 MiB seq read, iodepth 16, 30 s + 10 s ramp), `randread_iops.job` (24 × 4 KiB qd8 on each file's first 1 GiB), `write_BW.job` (the layout pass). The 60 s rows are `sed`'ed copies with only `runtime=` rewritten (a job-section `runtime=` beats `--runtime`, R-3's lesson). The §5 pinned rows and §6 low-qd rows are the same job with `numjobs`/`iodepth`/`runtime` rewritten (stated per row). Per-job 1 s bw logs for the first/last-third flatness column. |
| **Rigs** | [`rigs/2026-09-03-zc-ahead-yield-field-abba.sh`](rigs/2026-09-03-zc-ahead-yield-field-abba.sh) (the A-B-B-A driver), [`-pinned.sh`](rigs/2026-09-03-zc-ahead-yield-pinned.sh) (§5), [`-lowqd.sh`](rigs/2026-09-03-zc-ahead-yield-lowqd.sh) (§6), [`-row-delta.py`](rigs/2026-09-03-zc-ahead-yield-row-delta.py) (per-row ledger) and [`-table.py`](rigs/2026-09-03-zc-ahead-yield-table.py) (the summary tables below). |
| **Substrate** | squeeze-test (client 32-core Xeon 6426Y, 251 GB, 2×200 GbE, kernel **6.19.14-sqz**) → 5 storage nodes over **nvme-tcp**, memory-backed nullblk targets. **Every A-B-B-A leg on a fresh `cluster_reset_v4.sh`** (5 meta + 10 data namespaces, cache-less format) + its own `write_BW.job` layout pass (24 × 8 GiB), then **umount + fresh remount before the read rows** (§8 finding 1). FUSE-over-io_uring 32 queues × depth 32, `fuse3_zc_negotiated = 1`, `fuse3_kmbuf_negotiated = 1`, `transport_max_write` 1 MiB, `data_read_lanes` 4, `--interception --allow-other`. Box otherwise idle (loadavg 0.00 at start; the user's idle daemon on the box was torn down with their approval — it was `c985fa8c`, the A arm). Window 21:51–22:32 UTC. |
| **Modes** | **kern** = plain fio; **il-mode** = `LD_PRELOAD=` the arm's own shim. **A 1 MiB il-mode row is a KERNEL-LANE row by design** — the shim's hybrid lane gate (D14 corollary, 2026-08-07, `crates/squeezefs-preload/src/lane_gate.rs`) routes ops above its derived threshold to the real syscall: `ipc_lane_gate_kernel_routes` ≡ `fuse3_zc_replies` on every il row (1,653,565 / 1,653,567 on A1), `ipc_ops_read` = 0, `ipc_binds` = 24. So the lever governs both modes through the same router site, and the rows are engagement-valid on the lane-gate ledger (§8 finding 2). |
| **Tier** | measured-real. Rows 30 s + 10 s ramp unless labeled. Amplification columns: `dev/user` = (zc + pooled-fill + dest DMA bytes) ÷ user bytes scaled to include the ramp — ≈ 1.0 on both arms (no double-fetch class; the B rows' 0.94–0.97 is the ramp-scaling approximation on rows that ramped up, not a byte the lever saved). |
| **Artifacts** | `~/sqz-field-artifacts/2026-09-03/zc-ahead-yield-artifacts.tgz` (run1 = the aborted first pass, §8 finding 1; run2 = the A-B-B-A of record; run3 = the pinned legs; run4 = the low-qd legs + the comm sample; every `.stats` pre/post, fio JSON, bw logs, `.row` tables, mount + reset logs, driver logs). `/scratch/tmp/sqz-agent/` removed at the end; the box left unmounted. |

## 2. Suites, gate

Rebase onto dev `eb699483`: clean (no conflicts; the R-2/R-3 rework
moved the arm but not the site). `fuse_zc_serve_tests` 3/3 (an injected
fetch primitive — not capability-class, runs unprivileged),
`read_lane_tests` 18, `read_serve_phase_tests` 5, `read_dest_lease_tests`
1, `read_fast_dispatch_tests` 6, `kernel_op_economy_tests` 2,
`ipc_op_economy_tests` 5 — 40/40 green (`--all-features`,
`--test-threads=1`); `cargo fmt --check` clean; `cargo clippy
--all-targets -- -D warnings` clean in both the all-features and the
shipped configuration.

## 3. The A-B-B-A of record (run2; order A1 → B1 → B2 → A2, fresh cluster per leg)

### 3.1 The 1 MiB rows (`read_BW.job` 24 × qd16, 30 s)

| Row | Arm | GiB/s | clat mean / p50 / p99 µs | CPU s / µs-op | `f3-ur` / `tpc` / `other` s | lane fetch / serves | pf issued / evict-unc | probe up/back | sf waiters | zc GB / warm-copy GB / fill-DMA GB | fd serves / demotes | flat (⅓→⅓) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| A1 kern | c985fa8c | **35.31** | 10,525 / 8,454 / 39,059 | 122.3 / 112.1 | 77.1 / 44.9 / 0.1 | 0 / 0 | 0 / 0 | 0/0 | 0 | 1,515 / 0 / 0 | 0 / 1.445 M | −1.9 % |
| B1 kern | d6c06c10 | **35.56** | 10,510 / 8,585 / 35,914 | 182.5 / **167.1** | 92.4 / 42.6 / **44.6** | 26,435 / 94,018 | 98 / 38 | **4/4** | 11,858 | 1,330 / 2.7 / 111.3 | 92,275 / 1.283 M | +11.0 % |
| B2 kern | d6c06c10 | **36.12** | 10,360 / 8,356 / 38,535 | 155.8 / **140.5** | 81.5 / 49.8 / **21.5** | 12,891 / 35,434 | 130 / 47 | **2/2** | 16,339 | 1,447 / 5.0 / 54.7 | 31,532 / 1.401 M | +0.4 % |
| A2 kern | c985fa8c | **36.19** | 10,327 / 8,094 / 41,157 | 124.7 / 112.1 | 79.2 / 45.2 / 0.1 | 0 / 0 | 0 / 0 | 0/0 | 0 | 1,546 / 0 / 0 | 0 / 1.475 M | +10.9 % |
| A1 il-mode | c985fa8c | **40.90** | 9,145 / 6,652 / 40,108 | 143.7 / 114.4 | 88.6 / 51.6 / 1.8 | 0 / 0 | 0 / 0 | 0/0 | 0 | 1,734 / 0 / 0 | 0 / 1.654 M | +8.1 % |
| B1 il-mode | d6c06c10 | **40.50** | 9,210 / 6,980 / 36,438 | 142.2 / 113.9 | 88.2 / 50.2 / 2.1 | 0 / 0 | 0 / 0 | 0/0 | 0 | 1,730 / 0 / 0 | 1,132 / 1.650 M | +2.4 % |
| B2 il-mode | d6c06c10 | **41.21** | 9,074 / 6,783 / 35,914 | 142.2 / 112.4 | 89.7 / 48.6 / 2.2 | 0 / 0 | 0 / 0 | 0/0 | 0 | 1,753 / 0 / 0 | 1,108 / 1.671 M | +9.6 % |
| A2 il-mode | c985fa8c | **40.70** | 9,184 / 6,914 / 37,487 | 141.7 / 113.3 | 87.9 / 50.4 / 1.8 | 0 / 0 | 0 / 0 | 0/0 | 0 | 1,730 / 0 / 0 | 0 / 1.650 M | +10.5 % |

**Kern: A 35.31 / 36.19 vs B 35.56 / 36.12 — par (+0.7 %, −0.2 %); il-mode:
A 40.90 / 40.70 vs B 40.50 / 41.21 — par (−1.0 %, +1.3 %); clat par to the
µs on both.** The lever engaged on the kern rows exactly as designed —
`prefetch_issued` / `read_lane_fetches` moved from 0 to 26.4 k / 12.9 k
fills (111 / 55 GB pooled into the hold), 94 k / 35 k hold serves, 92 k /
32 k of them inline on the reap thread (`transport_fast_dispatch_serves`,
R-2's arm (a) finally non-zero on this row) — and the engage-governor
probed up and retreated 4/4 and 2/2 times, ending each row at
`read_lane_depth_target = 0`. The il-mode B rows did NOT engage
(`read_lane_fetches` = 0; only the residual hot-block hits the kern row
left, 1.1 k): the il-mode row ran SECOND on each B mount, after the kern
row's retreats had parked the governor in its duty-cycle cool-down — so
the il-mode B numbers are the un-engaged binary and read as the tree
control (par, as expected). The cost where it engaged: **+49 % / +25 %
daemon CPU per op**, almost all of it in the `other` class (44.6 / 21.5 s
≈ 1.7 ms per 4 MiB pooled fill) — §8 finding 3 names it.

### 3.2 The sustained legs (60 s, B1 and A2; kern then il-mode)

| Row | GiB/s | clat mean / p50 / p99 µs | CPU µs/op (`other` s) | lane fetch / serves / hold-evicted-unconsumed (ahead) | probe up/back | first/last third |
|---|---|---|---|---|---|---|
| **B1 kern 60 s** | **37.79** | 9,895 / 7,963 / 35,914 | **121.9** (42.7) | 26,321 / 88,278 / **401 (401)** | 5/6 | 35.91 / 37.90 (+5.6 %) |
| **A2 kern 60 s** | **38.27** | 9,767 / 7,569 / 39,059 | 95.2 (0.1) | 0 / 0 / 0 | 0/0 | 36.05 / 38.40 (+6.5 %) |
| **B1 il-mode 60 s** | **38.17** | 9,790 / 7,766 / 35,914 | **121.1** (39.8) | 23,752 / 77,977 / 0 | 5/5 | 36.09 / 38.14 (+5.7 %) |
| **A2 il-mode 60 s** | **37.76** | 9,876 / 7,700 / 38,535 | 97.2 (1.8) | 0 / 0 / 0 | 0/0 | 35.90 / 36.13 (+0.6 %) |

**Par both ways (−1.3 % kern, +1.1 % il-mode), flat on both arms, +28 %
CPU/op where engaged.** On the 60 s legs the governor got out of its
cool-down and engaged the il-mode row too (the same numbers as kern — the
same kernel lane), and the ahead-class spiral detector moved for the
first time on a governed row (`read_lane_hold_ahead_evictions` 401 on the
kern leg: landing-zone pressure, the 2026-08-05 hold-churn signature).
Both arms drift +5–6 % first→last third on this row — the row's own
warm-up shape (also on R-2/R-3's rows), not a lever term.

### 3.3 The no-regression row (`randread_iops.job` kern 24 × qd8, 30 s)

| Row | IOPS | clat mean / p50 / p99 / p99.9 µs | CPU µs/op | lane fetch / serves | pf issued | fd serves / demotes |
|---|---|---|---|---|---|---|
| A1 | 510,015 | 371.6 / 185 / 4,293 / 14,746 | 39.1 | 0 / 0 | 0 | 0 / 20.19 M |
| B1 | **512,957** | 369.6 / 189 / 4,293 / 13,697 | 39.3 | 82 / 5,371 | 158 | 112,150 / 20.24 M |
| B2 | **511,699** | 370.2 / 196 / 4,358 / 11,993 | 39.8 | 121 / 123,194 | 166 | 220,168 / 20.04 M |
| A2 | 510,584 | 371.2 / 183 / 4,358 / 14,615 | 39.2 | 0 / 0 | 0 | 0 / 20.26 M |

**Par (+0.4 %).** The lever's only footprint on random 4 KiB is the ~100
whole-block fills a randread's occasional stride-run classification
issues (the lane's `covered_skips` arm keeps it there); `dev/user` 0.99–1.00.

## 4. Where the lever's cost went (kern 1 MiB, B1 vs A1, exact means)

| Term | A1 (control) | B1 (lever, engaged) | reading |
|---|---|---|---|
| zc demand fetch `device_cq` | 5,480 µs | 5,380 µs | the fabric queue — unchanged |
| zc `msg_hop` / `wake_hop` | 403 / 692 | 421 / 674 | the two worker hops — unchanged |
| pooled ahead fill `dev_service` / `fetch_dma` | (no fills) | 4,291 / 5,470 µs (n = 26.7 k) | the SAME saturated queue, as a 4 MiB request |
| demand window joining an in-flight ahead fill (`sf_wait`) | — | **5,502 µs** (n = 11.9 k) | a demand read that waits on the lane's fill pays MORE than its own zc fetch would (5.4 ms + hops) — the fill was issued only a block ahead of a 4-block-deep reader |
| hold serve `slice_out` (the 1 MiB copy into the ent payload, NT stores) | — | 357 µs (n = 14.5 k) | cheap in latency, but a payload-scale memcpy on the reap thread: `transport_reap_gap_ns.blind_cqe` 90 → 170 µs, `blind` 18 → 34 µs |
| `read_transport_phase_ns.transport_total` | 7,461 µs | 6,847 µs | −8 % per op; fio clat unchanged — the kernel-side residue absorbed it (the row is fabric-bound end to end) |
| daemon CPU / op | 112.1 µs | 167.1 µs | +49 %: `other` +44.5 s (§8 f3 — the io-wq punt of the pooled fill), `f3-ur` +15 s (the inline hold copies) |

Little's law reads the same on both arms: 384 in-flight × 1 MiB ÷ 10.5 ms
≈ 36.6 GiB/s. The lever adds depth on the daemon side of a queue that is
already the bottleneck; the added depth becomes queueing (`sf_wait`),
not bandwidth — which is exactly what the engage-governor's
"adopt only when total fill delivery responds" law measured, five times.

## 5. The falsification leg: is the governor timid? (run3, B binary, kern 1 MiB, same file set, fresh mount per pin)

| Row | GiB/s | clat mean / p50 / p99 µs | CPU µs/op (`other` s) | lane fetch / serves | hold-evicted-unconsumed | sf waiters / `sf_wait` | zc GB / fill GB | `dev/user` |
|---|---|---|---|---|---|---|---|---|
| `SQUEEZEFS_READ_LANE_DEPTH=2` | **23.82** | 15,681 / 10,027 / 83,362 | 617.6 (226.6) | 135,833 / 482,638 | **1,116** | 53,462 / — | 492 / 571 | 1.039 |
| `=4` | **22.71** | 16,442 / 10,551 / 90,702 | 702.1 (254.2) | 155,976 / 583,720 | **1,978** | 28,152 / 13,417 µs | 355 / 655 | 1.036 |
| `=8` | **22.78** | 16,370 / 10,289 / 86,508 | 699.6 (255.9) | 161,338 / 619,864 | 0 | 22,854 / — | 333 / 677 | 1.032 |
| governed (unset) | **36.61** | 10,206 / 8,094 / 38,535 | 155.5 (33.3) | 20,446 / 67,690 | 0 | 14,261 / 6,777 µs | 1,407 / 86 | 0.949 |

**No — the governor is right.** Holding ANY fixed ahead depth on this
shape loses 35–38 % (with `dev/user` 1.03–1.04: real double-fetch
appears — the demand cohort and the lane race for the same blocks, and
`read_lane_hold_evicted_unconsumed` fires 1–2 k times), clat +55 %,
p99 2.2×, daemon CPU 4.5× per op with 255 s in the io-wq class. The
pinned rows are the read-lane campaign's falsified open-loop venue
(`.benchmarks/2026-08-01-read-lane.md`: "ahead-fetches died FIFO-
unconsumed racing the demand cohort") reproduced at the field — the
2026-08-05 governor exists precisely so this shape cannot happen by
default, and it worked: every governed row above ended at depth 0.

## 6. The premise leg: a single low-qd stream (run4, A B B A, one job, 1 MiB, 20 s + 5 s ramp, fresh mount per arm)

| Row | Arm | GiB/s | clat mean / p50 / p99 µs | CPU s / µs-op (`other` s) | pf issued / evict-unc | fd serves / demotes | zc GB / fill GB | flat |
|---|---|---|---|---|---|---|---|---|
| A1 qd1 | c985fa8c | **2.45** | 384 / 367 / 561 | 3.3 / 66.4 (0) | 0 / 0 | 0 / 62,686 | 66 / 0 | +0.6 % |
| B1 qd1 | d6c06c10 | **4.57** | **201** / 198 / 379 | 22.4 / 239.6 (7.4) | 26,948 / 29 | **107,618** / 5,270 | 5 / 113 | −0.8 % |
| B2 qd1 | d6c06c10 | **4.55** | **201** / 198 / 428 | 22.3 / 239.9 (7.4) | 26,762 / 28 | 106,873 / 5,149 | 5 / 112 | −2.0 % |
| A2 qd1 | c985fa8c | **2.50** | 378 / 362 / 578 | 3.4 / 66.2 (0) | 0 / 0 | 0 / 63,623 | 67 / 0 | −3.6 % |
| A1 qd4 | c985fa8c | **5.05** | 771 / 807 / 1,237 | 6.4 / 61.5 (0) | 0 / 0 | 0 / 129,789 | 136 / 0 | −1.0 % |
| B1 qd4 | d6c06c10 | **7.53** | **515** / 477 / 1,286 | 43.5 / 281.9 (17.5) | 47,748 / 738 | 185,611 / 6,786 | 5 / 200 | +15.8 % |
| B2 qd4 | d6c06c10 | **7.38** | **525** / 481 / 1,286 | 42.0 / 278.0 (16.7) | 45,971 / 673 | 178,804 / 10,979 | 9 / 193 | +25.2 % |
| A2 qd4 | c985fa8c | **4.71** | 826 / 831 / 1,221 | 5.6 / 58.3 (0) | 0 / 0 | 0 / 119,979 | 126 / 0 | +17.3 % |

**qd1 +84 % (2.45 / 2.50 → 4.57 / 4.55 GiB/s, clat 384 → 201 µs); qd4
+53 % (5.05 / 4.71 → 7.53 / 7.38, clat 771 / 826 → 515 / 525 µs) — both
orders, engagement exact.** This is the lever the branch's commit message
described, and on the shape it was designed for (a stream whose own
depth leaves fabric RTT exposed) it delivers: the R2 pipeline (not the
read-lane's probe-governed ahead arm — `read_lane_fetches` = 0,
`prefetch_issued` 26.9 k / 47.7 k) runs 4–8 blocks ahead, 95 % of the
demand windows find their block resident and serve INLINE on the reap
thread (R-2's arm (a): 107.6 k serves vs 5.3 k demotes at qd1 — the first
row on which the serve arm dominates), the zc direct leg goes quiet
(zc bytes 66 → 5 GB), and the stream runs at the pooled fill's
`dev_service` (1.37 ms per 4 MiB ≈ 2.9 GiB/s per stream ceiling from one
fill in flight, so the 4.6 GiB/s at qd1 is the pipeline's depth showing).
The price is the one the 2026-08-06 CPU-wall ruling weighs: **3.6× (qd1)
/ 4.6× (qd4) daemon CPU per op** — 240–282 vs 58–66 µs — split between
the inline 1 MiB copies on `f3-ur` (13.0 / 22.5 s) and the io-wq punt of
the pooled fills (7.4 / 17.5 s in `other`, §8 f3). At the field shape
that same price bought nothing (§3); here it buys +53–84 %.

## 7. Verdict and what it leaves on the board

1. **Not merged.** At the field shape (the acceptance venue named in the
   brief) the lever is par on throughput and clat in both modes,
   sustained, with rand-4k par — a wash by AGENTS.md's law — and it
   costs +25–49 % daemon CPU per op where it engages. Pinned, it loses
   35 %. The governed default never loses because the 2026-08-05
   governor retreats, but it pays the probes' CPU on every stream
   forever. The user's 14-hour `4c0ebf07-dirty` daemon and its EXA
   reading of **43.9 GB/s** is reproduced here as the il-mode row — on
   BOTH arms (A 43.9 / 43.7 GB/s, B 43.5 / 44.2): the number was the
   lane-gate kernel row through the shim (§8 f2), not the lever.
2. **The branch's diagnosis stands; its remedy is the wrong axis.** The
   field row's `prefetch_issued = 0` IS the dest-lease yield stamped by
   zc geometry — true. But whether the ahead machinery should issue is
   a property of the STREAM's exposed RTT (its in-flight depth against
   the fabric's service time), not of the transport class (zc vs dest-
   lease vs pooled). A yield keyed on transport class is right at 24 ×
   qd16 and wrong at 1 × qd1 for the same reason in both directions.
   **Board item (read board): a concurrency-aware yield** — the R2 top-
   up and the read-lane ahead arm decline when the stream's own
   in-flight demand depth (the singleflight population on its blocks,
   or `active_streams × qd` per file from the classifier's run state)
   already covers the fabric's BDP, and issue when it does not; the zc/
   dest-lease class then only says HOW a demand window fetches, never
   WHETHER the lane runs. The §6 rows (+84 % / +53 %) are the row this
   item is measured against; §3 (par) and §5 (−35 %) are its guards.
   The 2026-08-06 CPU-wall ruling prices it: on this fleet a 3.6–4.6×
   CPU/op for a low-qd stream is a trade the operator may want or not —
   it should be a lever with a default that a counted fleet row sets.
3. **The test does not land alone.** `zc_streaming_reads_still_issue_
   ahead_fetches` pins "zc-geometry streams issue ahead/R2 fills" — the
   lever's mechanism, not an independent law; without the fix it is
   red, and with item 2 the law it should pin is "a stream whose depth
   leaves RTT exposed issues ahead fills regardless of transport class",
   which needs the concurrency-aware yield to be true. It is the right
   red-first contract for that item and stays on the branch as its
   input.
4. **Design-doc line to keep true**: `docs/design-read-path.md`'s
   `read_dest_lease_bytes` row states the yield law ("`prefetch_issued`
   / `read_lane_fetches` ≈ 0 — the lane YIELDS to dest-leaseable
   traffic"); the zc composition inherits it on dev today (zc geometry
   stamps the same yield), and this note is the record that the
   composition was measured, not assumed.

## 8. Instrument findings

1. **A read row on the layout pass's mount pays the layout's reclaim.**
   `write_BW.job` is time-based: 24 × 8 GiB land in ~6 s at 33 GiB/s
   and the remaining ~24 s OVERWRITE, displacing ~58 k blocks
   (`rewrite_blocks` 270,920, `block_free_discards` 58,167) whose
   discards drain to the targets for a while after the row — run1's
   first read row on that mount read **35.4 GiB/s with 22.5 k reclaim
   commands in flight** against 41.1 for the next row on the same
   binary. R-2/R-3 avoided it by remounting after the layout (their
   rows ran after a umount); the driver now does the same (`2de47a8c`).
   A protocol lesson for every rig that lays out and reads on one
   mount: umount (the reclaim queue drains at teardown) or wait for
   `block_free_reclaim_queued` to return to 0.
2. **The il-mode 1 MiB row is a kernel-lane row, and it runs +15 % over
   plain kern on BOTH arms.** The shim's hybrid lane gate routes the
   1 MiB iocbs to the real `io_submit` verbatim (pure-kernel batches
   forward untouched), so the daemon sees identical FUSE-zc traffic —
   yet A 40.90 / 40.70 vs kern 35.31 / 36.19 GiB/s, and B the same
   split, run after run (also the R-2/R-3 era's "il seq read 40.35
   GiB/s" vs kern 36). The one shim-side difference on the I/O path is
   the reap: with kernel-lane iocbs tracked as `kernel_pending`, the
   `io_getevents` interposer runs its own bounded merge loop (≤ 50 ms
   passes) around the real call instead of fio's single blocking
   `io_getevents(min_nr, nr, timeout)` — a completion-cadence change on
   the CLIENT side that fio's `iodepth_batch_complete_*` should be able
   to reproduce without the shim. Unexplained here; filed as an
   instrument note: "il seq read" numbers on this fleet are kernel-
   lane numbers with a different reaper, and a kern-vs-il comparison
   at 1 MiB compares reap cadences, not data paths. Row validity for
   il 1 MiB rows is `ipc_lane_gate_kernel_routes` ≡ `fuse3_zc_replies`
   with `ipc_binds` > 0, never `ipc_ops_read` (the driver reads it so).
3. **`daemon_cpu_ns_by_class.other` on a read row is the kernel's
   `iou-wrk-*` threads** — a per-comm sample on the lever daemon during
   a 1-job qd4 row: `f3-ur*` 11.2 s, **`iou-wrk-*` 7.9 s across 29
   threads**, `sqz-nvme` 0.7 s, everything else ≈ 0. The pooled 4 MiB
   fills (`NvmeBlockDev` reads into a hold buffer) take the io-wq punt
   a buffered block-device read takes when it cannot complete inline,
   and those kernel worker threads bill to the daemon's process while
   matching no comm class in `src/daemon_cpu.rs`. On a 1 MiB seq row
   that class costs **≈ 1.7 ms of CPU per 4 MiB fill** (44.6 s / 26,435
   on B1; 226–256 s on the pinned rows) — more than the fuse3 workers'
   whole per-op cost. Two items: (a) add an `iou-wrk` class so the
   instrument names it (the R-1 precedent that pulled `f3-ur` out of
   `other`); (b) the punt itself is a fill-issue economy item for the
   hold path (the R-3 funnel work made the ISSUE side inline; the
   completion of a pooled buffered read is still io-wq).
4. The `.kvmap` control pair the brief named does not exist on the box;
   the control was the box's `/scratch/tmp/squeezefs` (= `c985fa8c`,
   the same commit, `release`, clean). Both arms are the same profile,
   both trees clean, both shims same-commit with their daemons (KD-7);
   no `SQUEEZEFS_IPC_ALLOW_DEV`.

**Landing-law checklist:** A-B-B-A — field, arm = binary, both orders,
fresh cluster + layout per leg (§3); sustained — 60 s kern + il-mode on
both arms, flat (§3.2); regression — rand-4k both arms (§3.3);
falsification — the pinned-depth legs (§5) and the premise legs (§6),
both orders; engagement — exact on every row (`read_lane_fetches` /
`prefetch_issued` / `read_lane_serves` / `transport_fast_dispatch_serves`
moving on the lever arm, 0 on the control; il rows on the lane-gate
ledger); amplification — `dev/user` per row (≈ 1.0 governed, 1.03–1.04
pinned = the double-fetch class named); spiral detectors read
(`read_lane_hold_evicted_unconsumed` 0 / 401 governed, 1–2 k pinned;
`prefetch_wasted` 0 throughout, `prefetch_evicted_unconsumed` 28–80);
tripwires 0 on every row (`invariant_tripwires`, `transport_lease_
overlong`, `fuse_op_watchdog_overdue`, `transport_cq_overflows`,
`read_dest_overruns`, `transport_requests_abandoned`, `fuse3_zc_bridge_
cancels`, `detached_task_panics`, `data_dma_fence_refusals`); R5 level 0,
no red/yellow events; instrument + substrate + binary + profile + kernel
+ tier stated; the box returned unmounted with `/scratch/tmp/sqz-agent/`
removed.
