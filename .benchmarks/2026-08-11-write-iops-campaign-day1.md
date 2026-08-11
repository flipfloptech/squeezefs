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

## Addendum — the worker op-cost leg landed the jump (same day)

The clean worker profile (PAIR posture, F=599 dwarf, resolved locally
against the build-id'd rocky8 binary + rig kallsyms) named the ~51 µs
worker-CPU budget's top line: **scc bucket-writer lock spin, 16.6 % SELF**
(`_mm_pause`; ICF-merged symbol — stacks under
`publish_attr → attr_cache HashIndex insert`, 32 workers × 32 hot inos),
plus the same path's `SystemTime` mint behind `__vdso_gettimeofday`
(~16 % combined clock reads).

**Fix (`09c679f4`): identical-WriteTimes publish elision** — the postlude
stamps on the coarse clock, so per hot ino every publish inside a ~4 ms
tick merges byte-identically; a lock-free peek pre-computes the merge and
equality skips the stripe mutex + insert (value-idempotent; the skipped
TTL-stamp refresh degrades to one refetch per daemon TTL, self-healing).

| Row (E1 binary) | Shared ON | Shared OFF |
|---|---|---|
| kern rand-4k | **525,259** (1.95 ms) / **525,509 sustained 120 s, flat** | 489,874 |
| il rand-4k | **531,344** | 491,769 |
| il seq-1m | 38,832 MiB/s ✓ | — |
| kern rand-4k qd64 | 368k (deeper offered depth still buys latency only) | — |

Three verdicts:
1. **391k → 525k/531k sustained (+34/36 %)** — the campaign's first jump,
   from the convoy+funnel+elision STACK (each was individually a wash;
   the pair law made the stack visible).
2. **The il→kern parity violation is RESOLVED** (il ≥ kern for the first
   time): the shared attr-bucket spin was the parity gap, not the ipc
   handoff — PR 4's question closes without a PR 4.
3. **Shared admission is now a counted +7–8 % win on BOTH paths** — the
   default-OFF ruling should be revisited with a fresh A-B-B-A after the
   next lever (single-order pair today).

Next named terms (from the same profile, in order): the remaining
bucket-writer venues (patch-purge present-key removes; `note_last_write_end`
swap word), the ~32 µs worker wait residue (enter/park economy), and the
qd-scaling knee (525k at qd32 vs 369k at qd64 — offered depth past the knee
still degrades; the probe-governor treatment applies once per-op cost
stops moving).

## Addendum 2 — CORRECTION: every day-1 "il" row was a KD-7 passthrough

The op-registry bracket's engagement audit (same day, later) found that
every "il" row above moved **zero `ipc_ops_write`** — the on-disk shim
was still the `4362ea74` build while the daemon had been redeployed
through `86e47549`/`3c7cf95a`/`09c679f4`, so KD-7 refused every session
("build mismatch … mount passthrough", printed on every row's stderr)
and LD_PRELOAD rows rode the kernel ring. KD-7 worked exactly as
designed; the harness never checked engagement. `row_diag.sh` now
refuses any row whose output announces passthrough (exit 9), and the
deploy step is PAIRED by law (daemon + shim from one `dist/<target>/`).

What survives, what falls:
- **Falls**: verdict 2 ("il→kern parity RESOLVED", "il 531,344") — that
  cell was the kernel path measured twice. Parity is REOPENED: the first
  ENGAGED il row of the day (B1 of the op-registry bracket, ring ledger
  exact: `ipc_ops_write` +16.39 M ≡ row ops) reads **273.2 k = 0.53×
  kern** on the same mount.
- **Survives**: verdict 1 (kern 391 k → 525 k) — kernel-venue rows,
  engagement N/A. Verdict 3 (Shared ON +7–8 %) — BOTH brackets were
  genuinely kernel-path (the "il" bracket was an accidental second
  kernel bracket, order-independent at 530/491 vs 523/489), so the
  default-ON ruling stands on kernel evidence; the engaged-il posture
  bracket is owed.
- Engaged il DID move with the campaign: 205 k (diagnosis note, PAIR
  posture) → 273 k engaged (+33 %) — the same ratio as kern's +34 %.
