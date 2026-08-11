# 2026-08-11 — Write-IOPS Campaign, Day 1: rand-4k decomposition to the worker budget

| | |
|---|---|
| **Binaries** | branch `perf/write-inode-convoy-pr3`, `0d5ac72b` → `c0f4071a` (rocky8 `task build:rocky8`, measurement-valid) |
| **Substrate** | squeeze-test (`memp-s3ds-aqs-37`): 32 CPUs, 5 meta + 10 data NVMe-oF namespaces, cache-less format, 4 MiB blocks |
| **Instrument** | elbencho 3.1-11, `row_diag.sh` stats+diskstats deltas; raw comparator fio libaio |
| **Row shape** | rand-4k overwrite `-w --rand -t 32 --iodepth 32 --direct --lat --infloop --timelimit 60`, 32 × 2 GiB preconditioned |
| **Engagement** | exact on every row (scope ledger, reap venues, zc directs) |

## The raw comparator (measured this session — first raw rand-WRITE row on this rig)

fio libaio 32×32 across the 10 data namespaces: **2.49 M IOPS / 30 s, 2.62 M
/ 60 s sustained, clat ~400 µs**. The substrate holds 6× headroom over every
FS row below; the wall is software-side by construction.

## Row ledger (kern rand-4k unless noted)

| Row | Binary + posture | IOPS | clat | note |
|---|---|---|---|---|
| A1/A2 | pre-funnel, Shared ON | 403k / 361k | 2.54–2.83 ms | bracket spans B — wash |
| B1/B2 | pre-funnel, Shared OFF | 382k / 383k | 2.67 ms | tight |
| F1 | interleave (no GETEVENTS), ON | 394k | 2.59 ms | in-flight 19.5→26.3 |
| G1/G2 | + purge economy, OFF/ON | 389k / 392k | 2.6 ms | |
| T1 | + timeline instrument, **OFF (default)** | 381k | 2.69 ms | **midpass_reaps +0 / 22.85 M** — the instrument convicted the funnel fix (DEFER_TASKRUN: `submit()` runs no task-work) |
| T2 | + GETEVENTS flush, OFF | 357k | 2.87 ms | engagement 95 %, bridge RTT ~1 ms → **64 µs** |
| T3 | + burst-drain, OFF | 370k | 2.77 ms | |
| T4 | + work-conserving pass, OFF | 358k | 2.86 ms | |
| P2 | T4 binary, OFF, OP_PROFILE row | 355k | 2.88 ms | **`write_lock_wait` mean 1,818 µs — the convoy was BACK** (posture OFF): the T-series never ran the pair |
| **PAIR** | **T4 binary, Shared ON** | **386k** | 2.65 ms | guard 2 µs + bridge 80 µs + midpass 97.5 % + Shared 99.7 % — every named stage dead SIMULTANEOUSLY |
| pair qd64 / qd128 | ON | 346k / 347k | 5.9 / 11.8 ms | flat ⇒ pure RATE limit |
| UNFUSED | ON + `SQUEEZEFS_FUSE_ZC_WRITE_FUSION=0` | **57k** | 17.8 ms | fusion is **6.7× load-bearing** — the architecture is right |
| busy probe | ON + pidstat | 305k (instrumented) | — | fused workers **61 % CPU each** |

## The verdict

With guard wait (1,120 µs → 2 µs), bridge DMA RTT (~1 ms → 64–80 µs,
device-true), pass funnel (97.5 % mid-pass), and purge ceremony all dead and
proven engaged, the row is a pure worker-rate limit:

**32 fused workers × ~83 µs serial/op ≈ 386k**, split ≈ 51 µs worker CPU +
≈ 32 µs worker wait per op. qd-flatness pins it (offered depth buys latency,
not throughput); the unfused control pins the venue (classic dispatch is
6.7× worse — the fused design stays).

The 1 M budget in these units: **~32 µs total/op/worker**. Next leg = the
worker-path op-cost campaign: perf capture of ONE fused worker on this
binary; named targets from the (instrument-tax-corrected) SELF profile —
scc hash traffic (~16 %), handler body, `publish_attr`, OpProf registry
claim, phase-record clock reads — plus the ~32 µs wait residue (enter/park
economy).

## Standing cautions

- The T-series fuse3 commits (`3c7cf95a`, `17738193`, `c0f4071a`) merge
  only WITH the Shared pairing evidence: the PAIR row is their first
  same-band showing; a pre-merge A-B-B-A of PAIR vs G2-equivalent posture
  is still owed (single-order today).
- Posture pairing is now a MEASUREMENT LAW for this campaign: every future
  row states BOTH `write_shared_enabled` and the reap-venue deltas — the
  T-series measured four binaries behind a restored convoy because the
  default flipped mid-campaign.
- il rand-4k parity (best 297k OFF / 276k ON vs kern 386k) remains open —
  PR 4's question.
- Parallel-run test flakes (zc suites, read_lane_tests) reproduce on
  pre-campaign trees under default `cargo test` parallelism; the gate's
  `--test-threads=1` never sees them. Backlog item, not campaign debt.
