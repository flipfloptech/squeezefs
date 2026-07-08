# PR 5 gate: transport zero-copy (payload leases) — before/after + cumulative

Design: `docs/design-zero-copy-write-path.md` §5.4 (PR 5). Gate: large-seq
additive improvement with the **cumulative ≥ 3× (≥ ~1.3 GB/s)** acceptance
target vs the original attribution baseline (430–512 MiB/s,
`.benchmarks/2026-07-07-write-path-attribution.md`); small-write ops/s and
Metadata rows at-or-better; `transport_parked_commits` ≈ 0 steady-state;
`transport_leases_outstanding` = 0 at quiesce.

## Method

Same shape as the PR 4 note (`2026-07-07-pr4-write-through-gate.md`):
unprivileged local mount, fresh volume per run, btrfs backing with
`chattr +C` on both files, runs interleaved BEFORE/AFTER to spread machine
noise:

```
~/tmp/sqfs_p5/meta.bin  256M   ~/tmp/sqfs_p5/data.bin  20G
squeezefs format sqmeta://…/meta.bin sqdata://…/data.bin       # 4 MiB blocks
squeezefs mount sqmeta://…/meta.bin ~/tmp/sqfs_p5/mnt \
  --disk-cache-paths ~/tmp/sqfs_p5/staging --daemon --log-file … \
  --uid $(id -u) --gid $(id -g)
squeezefs bench ~/tmp/sqfs_p5/mnt -t 10 --large-size 1024 --only large-seq-write
```

FUSE-over-io_uring armed on every mount (32 queues × depth 4). BEFORE =
dev @ ffc5fe0 (PRs 1–4 landed); AFTER = dev + PR 5 (payload leases +
session body-copy skip + severance boundary). Box note: the same
production squeezefs daemon (~27 GB RSS) shares the machine as in the PR 4
note; single-run read rows stay noisy, write rows were stable.

## Primary gate — Write Large Seq, t=10 × 1 GiB, 1 MiB chunks

| Run | BEFORE (dev ffc5fe0) | AFTER (PR 5) |
|---|---|---|
| 1 | 1559.92 MiB/s | 1691.42 MiB/s |
| 2 | 1559.09 MiB/s | 1605.22 MiB/s |
| 3 | 1548.58 MiB/s | 1685.95 MiB/s |
| **mean** | **~1555.9 MiB/s** | **~1660.9 MiB/s** |

- **PR 5 delta: +6.7%** (avg ack latency 0.64–0.65 ms → 0.59–0.62 ms).
  Consistent with the audit's expectation: PR 4 already removed the
  staging round-trip, so the transport copies (audit #1/#2: 2 MiB memcpy +
  1 MiB alloc per 1 MiB request) were the remaining fuse-thread share,
  smaller than the audit's +150–300 MiB/s midpoint on this box but firmly
  additive and beyond run-to-run noise (BEFORE spread 11 MiB/s).
- **Cumulative acceptance gate: MET.** ~1660.9 MiB/s vs the original
  430–512 MiB/s attribution band ⇒ **3.24×–3.86× (3.53× vs the 471
  midpoint)** ≥ 3×, and 1.66 GB/s ≥ ~1.3 GB/s absolute.

## Transport lease mechanism proof (stats inode, per AFTER gate run)

| Field | r1 / r2 / r3 | Meaning |
|---|---|---|
| `transport_payload_leases` | 34389 / 35402 / 34418 | every FUSE_WRITE rode a lease (10 GiB ÷ 1 MiB requests ≈ 10.2k of these are bench-file writes; the rest are small/metadata-phase writes + retries) — the 1 MiB copy + alloc per request is gone |
| `transport_parked_commits` | 0 / 0 / 0 | leases drop inside one handler invocation; re-arm never waited |
| `transport_leases_outstanding` | 0 / 0 / 0 | severance boundary holds at quiesce — no lease escaped |
| `transport_lease_max_age_ms` | 28 / 25 / 24 | bounded by one handler invocation (debug builds hard-assert < 1000) |
| `write_through_fallbacks`, `uring_queue_full`, `writeback_hard_failures` | 0 | clean |

## No-regression rows (full bench, t=10, defaults: large 128 MB/thread)

| Row | BEFORE | AFTER | Δ |
|---|---|---|---|
| Write Large Seq | 1112.29 MiB/s | 1202.76 MiB/s | +8% |
| Write Large Rand | 610.42 MiB/s | 594.20 MiB/s | −3% (noise) |
| Write Small Seq | 247.96 MiB/s | 251.71 MiB/s | **+2%** |
| Write Small Rand | 225.61 MiB/s | 221.28 MiB/s | −2% (noise) |
| Read Small Seq | 3111.01 MiB/s | 3281.01 MiB/s | +5% |
| Read Small Rand | 3130.71 MiB/s | 3298.98 MiB/s | +5% |
| Read Large Seq | 1054.76 MiB/s | 1095.36 MiB/s | +4% |
| Read Large Rand | 1214.59 MiB/s | 960.95 MiB/s | see PR 4 note: large-read rows are instrument-coupled and swing ±20% run-to-run on the same binary; no read code changed in PR 5 |
| Metadata Stat | 159722 ops/s | 208671 ops/s | +31% |
| Metadata Mkdir | 29001 ops/s | 30010 ops/s | +3% |
| Metadata Readdir | 17015 ops/s | 17874 ops/s | +5% |
| Metadata Rmdir | 28569 ops/s | 28455 ops/s | ±0 |
| Metadata Delete | 3693 ops/s | 3557 ops/s | −4% (inside the ±17% same-binary variance documented in the PR 4 note) |

Small-write rows at-or-better as the design predicts (§5.4: the sever copy
replaces transport copies #1 *and* #2 — strictly less copying than
before). Full-bench AFTER run took 12674 leases with 0 parked / 0
outstanding / max age 88 ms.

## Storm / teardown suites

- Single-queue starvation test (`tests/multi_queue_tests.rs`): storm of
  1500 small files + 300 hot rewrites + an 8 MiB striped stream pinned to
  one CPU (one uring queue, `Q_DEPTH=4`): no stall,
  `transport_parked_commits == 0`, `transport_leases_outstanding == 0`,
  max age < 1 s, byte-exact round-trips, unmount clean.
- Pre-existing (NOT PR 5): an intermittent dev-baseline wedge where one
  kernel request never completes under small-file storms
  (`/sys/fs/fuse/connections/N/waiting == 1`, syncfs blocks, plain umount
  EBUSY forever). Reproduced on dev @ ffc5fe0 with the identical storm and
  no leases in the binary; PR 5 runs of the same probe unmounted cleanly.
  The storm test hard-fails on any lease-class wedge (parked/outstanding
  ≠ 0) and reports the inherited wedge class loudly without failing the
  PR 5 gate on it. Filed observation for a follow-up.
