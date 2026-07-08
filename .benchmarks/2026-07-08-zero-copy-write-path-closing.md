# Zero-copy write path — closing report (PR 7)

Design: `docs/design-zero-copy-write-path.md` (PRs 0–7). Acceptance gate:
**large-seq write ≥ 3× the attribution baseline (≥ ~1.3 GB/s)** on the
committed substrate profile, no regression on small-write ops/s, read
rows, or Metadata rows vs the committed baselines.

**Verdict: GATE MET.** Closing re-run mean **~1827 MiB/s** (five clean
runs, 1764–1901) vs the original **430–512 MiB/s** attribution band ⇒
**3.57×–4.25× (3.88× vs the 471 midpoint)**, absolute ~1.8 GB/s ≥
~1.3 GB/s. Userspace copies per sequential byte: **5 → 1** (+ the
irreducible kernel copy + 1 DMA); the extra per-block staging device
write + msync is gone.

## Provenance

| | |
|---|---|
| Tree | `docs/zero-copy-closing` off dev @ `2c99e85` (PRs 0–6 landed) |
| Date | 2026-07-08 |
| Binary | `cargo build --release` (release keeps `debug = true`) |
| Machine | AMD RYZEN AI MAX+ PRO 395 w/ Radeon 8060S, 32 hw threads, 94 GiB RAM |
| Kernel | 7.1.3-1-cachyos |
| Method | PR 4/5 method: unprivileged local mount, btrfs backing, `chattr +C` on both files, fresh 256 MiB meta + 20 GiB data volume, 4 MiB blocks, `--disk-cache-paths` staging, FUSE-over-io_uring armed on every mount (32 queues × depth 4) |
| Box note | the same production squeezefs daemon as in the PR 4/5 notes shares the machine (~16 GB RSS today); write rows were stable across runs, single-run read/metadata rows stay noisy |

```
squeezefs format sqmeta://…/meta.bin sqdata://…/data.bin
squeezefs mount sqmeta://…/meta.bin ~/tmp/sqfs_p7/mnt \
  --disk-cache-paths ~/tmp/sqfs_p7/staging --daemon --log-file … \
  --uid $(id -u) --gid $(id -g)
squeezefs bench ~/tmp/sqfs_p7/mnt -t 10 --large-size 1024 --only large-seq-write   # gate rows
squeezefs bench ~/tmp/sqfs_p7/mnt -t 10                                            # full table
```

## The throughput story — attribution → per-PR gates → closing

| Stage | Large-seq (t=10 × 1 GiB, 1 MiB chunks) | Ratio | Evidence |
|---|---|---|---|
| Attribution baseline (pre-PR 1) | **430–512 MiB/s** (substrate control 2.1–2.2 GB/s; 45–50 % of daemon cycles in glibc memcpy) | 1× | `.benchmarks/2026-07-07-write-path-attribution.md` |
| Re-baseline before PR 4 (dev @ `b7a2973`, PRs 1–3 landed) | ~921 MiB/s (904–946) | — | `.benchmarks/2026-07-07-pr4-write-through-gate.md` |
| **PR 4** write-through (`ffc5fe0`) | **~1679 MiB/s** (1611–1742) | **1.82×** vs re-baseline (inside the pre-authorized 1.6–1.9× proceed band; shortfall ~0.08× recorded) | same file |
| **PR 5** transport leases (`2580006`) | **~1660.9 MiB/s** sustained (BEFORE @ `ffc5fe0` ~1555.9; +6.7 %) | **cumulative 3.24×–3.86×** vs 430–512 — **≥ 3× gate MET at PR 5** | `.benchmarks/2026-07-07-pr5-transport-lease-gate.md` |
| **Closing re-run** (PRs 1–6 + the two PR 6-surfaced corruption fixes) | **~1827 MiB/s** mean — 1784.32 / 1888.69 / 1901.17 / 1764.44 (fresh volume) / 1794.00 (fresh volume); ack 0.53–0.57 ms | **3.57×–4.25×** | this file |

A sixth closing run with `perf` attached to the daemon sampled 1675.76
MiB/s (excluded from the mean; profiler overhead) — it feeds the
attribution rerun below. Every stage was measured with the same command
shape on the same box; per-PR deltas are attributable because each perf
PR re-ran its own fresh BEFORE (Rollout step 2).

## Final full-table reference numbers (new committed reference)

Full bench, t=10, defaults (large 128 MB/thread, small 128 KiB × 100/thread),
single run on the first closing mount — **these are the new committed
reference rows** for future no-regression gates on this machine/profile:

| Workload | Throughput | IOPS | Avg latency |
|---|---|---|---|
| Write (Large Seq) | 1340.53 MiB/s | 1340.53 ops/s | 0.75 ms |
| Read (Large Seq) | 1072.52 MiB/s | 1072.52 ops/s | 0.93 ms |
| Write (Large Rand) | 587.38 MiB/s | 587.38 ops/s | 1.70 ms |
| Read (Large Rand) | 1246.08 MiB/s | 1246.08 ops/s | 0.80 ms |
| Write (Small Seq) | 260.69 MiB/s | 2085.54 ops/s | 0.48 ms |
| Read (Small Seq) | 3125.26 MiB/s | 25002.05 ops/s | 0.04 ms |
| Write (Small Rand) | 226.06 MiB/s | 1808.51 ops/s | 0.55 ms |
| Read (Small Rand) | 2953.35 MiB/s | 23626.82 ops/s | 0.04 ms |
| Metadata Stat | — | 141469.31 ops/s | 0.01 ms |
| Metadata Mkdir | — | 32452.42 ops/s | 0.03 ms |
| Metadata Readdir | — | 18208.91 ops/s | 0.05 ms |
| Metadata Rmdir | — | 31393.99 ops/s | 0.03 ms |
| Metadata Delete | — | 3553.37 ops/s | 0.28 ms |

No-regression vs the PR 5 committed AFTER table (identical method):
Write Large Seq +11 %, Write Large Rand −1 %, Write Small Seq +4 %,
Write Small Rand +2 %, Read Small Seq −5 %, Read Small Rand −10 %,
Read Large Seq −2 %, Read Large Rand +30 %, Mkdir +8 %, Readdir +2 %,
Rmdir +10 %, Delete ±0. Large-read rows remain instrument-coupled
(±20–30 % run-to-run on the same binary, PR 4 note); the Stat row
(208671 → 141469) sits inside its historical committed spread —
132178 / 171999 / 159722 / 208671 across the PR 4/5 measurement days on
adjacent binaries — and no metadata code changed after PR 5. The
pre-WAL-removal tmpfs baseline (`2026-07-07-pre-wal-removal-mount-bench.md`)
is a different substrate and small-file shape (`--small-size 4096`, sudo,
tmpfs); its rows are not row-comparable here, but every metadata row is
at-or-far-better (e.g. Stat 69490 → 141469, Delete 1516.73 → 3553.37 at
the *easier* 128 KiB shape — the honest 4 MiB-shape Delete story stays as
measured in `2026-07-07-pr5-delete-gate-analysis.md`).

## Mechanism proof (stats inode)

First closing mount after 3 × 10 GiB gate runs + the full table:

| Field | Value | Meaning |
|---|---|---|
| `write_through_blocks` / `bytes` | 8280 / 34728837120 | ≈ striped-seq volume: full adoption |
| `write_through_fallbacks` | 0 | no backpressure degradation |
| `active_block_memset_elided_bytes` | 32696422400 | seed memsets gone (PR 4 note evidenced 41.4 GB after 4 × 10 GiB) |
| `active_block_cow_copies` | 0 | no read/write collisions paid |
| `transport_payload_leases` | 115928 | every FUSE_WRITE rode a lease |
| `transport_parked_commits` / `leases_outstanding` | 0 / 0 | re-arm never waited; severance boundary holds at quiesce |
| `transport_lease_max_age_ms` | 95 | bounded by one handler invocation |
| `uring_queue_full` / `writeback_hard_failures` | 0 / 0 | clean |

Fresh-volume single-gate-run mount: `write_through_blocks` 2551 (10 GiB =
2560 blocks; each file's first staged-layout block promotes via the
router), `nvme_staging_current_bytes` **0** (BEFORE PR 4 the same
workload left ~2.6 GB staged), `nvme_unaligned_write_fallbacks` 476 —
composition-checked: packed staged-value and promotion writes
(`8+meta_len` framing, non-4-KiB-multiple **by design**; risk R6's named
legitimate traffic, and the one leg PR 3 deliberately did not convert).
Pooled full-block sources provably took the aligned DMA branch: 2551
write-throughs vs 476 fallbacks — a pooled-source escape would have
counted ≥ 2551.

## Attribution rerun (perf, daemon-side, mid-storm)

997 Hz cycles sample of the mount daemon during a gate-run storm
(2114 samples, unprivileged `perf record -p`):

| | Attribution doc (430 MiB/s) | Closing (1676 MiB/s sampled run) |
|---|---|---|
| glibc memcpy cluster (anonymous `0x1b1exx`) | **45–50 %** of daemon cycles | **~31.9 %** |
| Other libc | — | ~3.6 % |

Delivered throughput rose ~3.6× while the memcpy share fell ~15+ points:
per-byte copy traffic dropped roughly 5–6× — consistent with the 5 → 1
userspace-copy result, and the remaining share is dominated by the two
copies the design names irreducible on this transport (kernel copy K
into the registered payload, and the one legitimate merge copy into the
accumulation buffer). Alloc-rate evidence is structural rather than a
dhat rerun: the per-request 1 MiB heap allocation (audit #1) cannot
occur on a leased delivery, and 115,928 write deliveries rode leases
with 0 parked commits and 0 outstanding at quiesce.

## Copy inventory — before → after

From `.benchmarks/2026-07-07-write-copy-audit.md` (13 sites). Sequential
striped stream, per byte / per 4 MiB block:

| Audit # | Site (before) | Before | After | Landed |
|---|---|---|---|---|
| K | kernel → registered `ent.payload` | 1 MiB /R | unchanged — irreducible (no FUSE-over-io_uring splice) | — |
| 1 | transport `Bytes::copy_from_slice` → fresh heap Bytes | 1 MiB /R + 1 MiB alloc | **0** — `Bytes::from_owner` payload lease, deferred COMMIT_AND_FETCH re-arm | PR 5 `2580006` |
| 2 | session `data_buf` body copy | 1 MiB /R | **0** — FUSE_WRITE body-copy skip (only `fuse_write_in` copied) | PR 5 `2580006` |
| 4 | 4 MiB zero-fill on first block touch | 4 MiB /B memset | **0** for sequential fills — coverage-tracked elision (41.4 GB evidenced in the PR 4 gate; 32.7 GB re-evidenced here) | PR 4 `ffc5fe0` |
| 6 | merge into active block (was shared-`Bytes` raw-ptr **UB**) | 1 MiB /R | **kept** — the one legitimate userspace copy, now UB-free (`ActiveBlockBuf::make_mut`, CoW on collision) | PR 1 `7f783ff` |
| 7 | staging mmap put + `msync` | 4 MiB /B + **extra staging device write** | **0** for content-complete blocks — write-through (staging demoted to partials/tails/spill/backpressure) | PR 4 `ffc5fe0` |
| 8 | writeback flush `Bytes::copy_from_slice` | 4 MiB /B + 4 MiB alloc | **0** — write-only guard-backed `StagedDmaSource`, DMA straight off the staging mmap | PR 3 `b7a2973` |
| 11 | unaligned bounce (alignment was jemalloc luck) | occasional 4 MiB | contractual 4 KiB alignment for pooled sources + `nvme_unaligned_write_fallbacks` violation counter | PR 2 `ec304cf` |
| 12 | router striped `PooledBuf` copy | 4 MiB /B | **0** on full coverage — `data_slice` reused directly | PR 6 `80b0569` |
| 3 | classical-path session copy | INIT-only | unchanged (INIT-only by policy) | — |
| 5 / 13 | RMW seeds (staged path / router path) | ≤4 MiB /B | kept — genuine partial-block RMW only, by design | — |
| 9 | crypto passthrough / non-passthrough | 0 / transform buffers | passthrough stays 0; non-passthrough scratch-pool consolidation **deferred** (§5.7, severable) | follow-up |

Net per sequential 4 MiB block: ~24 MiB memcpy + 4 MiB staging write +
4 MiB DMA → **8 MiB (4 kernel + 4 merge) + 4 MiB DMA**.

## Bugs found & fixed along the way

1. **P0 shared-`Bytes` mutation UB** (pre-existing): the write merge
   mutated a shared refcounted buffer through a raw pointer while readers
   held zero-copy slices — held read replies observably changed in
   flight. Fixed by construction with exclusive-owner CoW
   `ActiveBlockBuf` (`Arc::get_mut`-gated mutation, immutable snapshots,
   loom-modeled). Tests `ac92350`, fix `7f783ff`.
2. **Promotion overrun seed** (same UB class): promotion seeds could
   merge past a short shared `Bytes`' length; seeds now build full-block
   zero-extended buffers. Fixed inside PR 1 (`7f783ff`).
3. **Cross-file infoleak through recycled `PooledBuf` memory**
   (pre-existing): the Vec backing preserved stale bytes across pool
   recycling — a previous user's content could leak through hole reads /
   short-read tails; zeroing was FIFO luck. Fixed in PR 2 (`ec304cf`:
   handouts logically empty, every newly exposed byte filled) and closed
   structurally under PR 4's memset elision by the uncovered-range
   contract (recycled bytes never escape the covered interval; sparse
   hole reads serve zeros — the durable variant is pinned via
   fsync → staging → writeback → device read-back, `a678890`).
4. **Missed-wake loom counterexample in the lease protocol sketch**
   (caught pre-ship): the loom `ent_lease` model rejected a
   SeqCst-accesses-only draft of the refs/parked protocol with a concrete
   missed-wake interleaving — a parked commit nobody wakes is a
   deterministic mount hang at Q_DEPTH=4 (store-buffer litmus). Shipped
   protocol carries explicit SeqCst fences on both sides
   (`lease_core.rs`, PR 5 `2580006`).
5. **Hole-punch-after-reallocatable corruption** (pre-existing on dev;
   surfaced by PR 6's pinned striped concurrency workload, ~1/4 runs):
   `BackendRouter::free_block` published the offset to the free list and
   *then* punched — a concurrent allocate→DMA could land a new owner's
   acked bytes in the window and the late `FALLOC_FL_PUNCH_HOLE` zeroed
   them (durable acked-write lost update); non-terminal (clone-shared)
   frees punched unconditionally. Fixed with the `begin_free` /
   `finish_free` split: punch only on terminal release, strictly before
   the offset is reallocatable; shared blocks never punch. Tests
   `1a0ea16`, fix `f0ca977`.
6. **Stale-fill key-reuse poisoning** (pre-existing; same PR 6 workload):
   block keys are offset strings, so free + realloc reuses the same key —
   the read-fill's check-then-publish could stick a dead incarnation's
   bytes under the reused key (all-zero reads until remount). Fixed with
   publish–revalidate–undo fills, deletion of unvalidatable NVMe→RAM
   re-promotes, and reused-key tier purges by the no-LRU-put owners.
   Tests `2974416`, fix `8e3995e`.

Items 5 and 6 were invisible before this design because `write_striped`'s
unconditional read-LRU put served every read from RAM; PR 6's
write-through route (no LRU put, by design) made the device bytes
observable — the perf work paid for itself in correctness.

## Open follow-ups (dispositions)

1. **Pre-existing intermittent stuck-request unmount wedge** — one kernel
   request never completes (`/sys/fs/fuse/connections/N/waiting == 1`),
   syncfs blocks, plain umount EBUSY, no userspace holders; the fs keeps
   serving all other requests. **Not introduced by this design**:
   reproduced byte-for-byte on dev @ `ffc5fe0` (pre-lease binary — no
   leases in the binary) and 3/3 on unmodified dev @ `2580006` during the
   PR 6 investigation. During this closing session the class fired on
   **both classifiable bench-mount teardowns** (transport stats clean —
   leases 0 outstanding / 0 parked — and `fusectl` abort recovered
   cleanly every time) and **3/3 in standalone storm-test re-runs**
   (kernel 7.1.3-1-cachyos; see the gate note below) — the box currently
   reproduces it near-deterministically after storm workloads, which
   makes it *easier* to bisect, not scarier. The multi_queue storm test
   detects the class, hard-fails any lease-shaped wedge, and loudly
   reports the inherited class — the test stays as-is. Follow-up:
   dedicated stuck-request investigation (kernel-side FUSE request
   accounting vs the over-uring transport).
2. **`CRYPTO_SCRATCH_POOL` sub-commit (§5.7)** — deferred per design
   ("severable — the gate is passthrough and does not depend on it").
   Non-passthrough write-through keeps bounded transform copies (R6
   accepted). Follow-up if compressed/encrypted large-write throughput
   becomes a target.
3. **Root suites** (`run_ltp_syscalls.sh`, `run_fstests.sh`,
   `run_elbencho_mount.sh`) — require root + a mounted FS; per Rollout
   step 4 they run against dev post-merge (material write-path + FUSE
   transport changes). To be executed as root on this box after this
   branch merges; not part of the cargo gate.
4. **`write_file_staged` RMW-seed unvalidated publish** (noted in
   `8e3995e`): the seed path at `fuse_client.rs:1396-1438` still
   publishes without post-publish revalidation — unreachable for
   full-block writes and predates this design; left as-is, filed for the
   same follow-up as the fill-discipline work.
5. **Defrag source-slot free (design OQ 6)**: `BlockMove` now merges
   through the primitive (serialization + fencing revalidation + tier
   coherency fixed in PR 4), but the displaced source mapping is
   tier-purged and **not** freed — the move reuses the allocation;
   source-slot reclamation stays with the defrag driver. Disposition
   recorded in the PR 4 census; revisit with the defrag ledger work.
6. **OQ 4 health-endpoint warning** — filed with the
   `write_through_fallbacks` stat instead: `health.rs` is probe
   hysteresis, not a stats surface (recorded in the PR 4 commit).
7. **OQ 5 elbencho re-attribution** — the ~230 MiB/s elbencho row predates
   PRs 1–6; re-attribute instrument-vs-FS share when the root suites run
   (item 3).
8. **Alternatives C/D levers** (sub-block direct DMA; detached-upload
   ack) — documented in the design, deliberately not taken; the ≥ 3× gate
   was met without them. Reopen only if a future target demands
   zero-copy-to-DMA.
9. **Transport-notify over-uring** (`fuse_notify_inval_*`) — separate
   design, filed from `2026-07-07-pr5-delete-gate-analysis.md`; unchanged
   by this work.

## Criterion companion baseline

`cargo bench --bench squeezefs_bench -- --save-baseline zero_copy_write_path`
and `--bench high_concurrency_bench` saved at this commit
(`target/criterion`, local artifact per the pre-WAL-removal precedent) —
the reference for future in-process regressions alongside the mount rows
above.

## PR 7 gate note

Full required gate run on this branch (docs-only vs dev @ `2c99e85`):
`clippy --all-targets --all-features -D warnings` clean, `fmt --check`
clean, `doc --no-deps` (one pre-existing warning in
`src/meta_backend/alloc.rs`, untouched — same disposition as the PR 5
gate), bench smoke green, `tests/run_loom.sh` green,
`test --all-features -- --test-threads=1` green across every binary
**except** the real-mount storm test
(`multi_queue_tests::storm::test_single_queue_qdepth4_small_file_storm_no_starvation`),
which fired the **inherited environmental wedge class** on this box
today — **3/3 standalone re-runs**, `[WEDGE] clean unmount EBUSY with
clean transport stats`, then `daemon still alive 30s after a lazy
detach` (the stuck request survives even a lazy detach; only a `fusectl`
abort releases it). This is the follow-up-1 class, byte-for-byte: the
identical 3/3 reproduction was recorded on **unmodified dev** during the
PR 6 session (commit `80b0569`) and on the pre-lease binary dev @
`ffc5fe0` (PR 5 note), and the closing-bench mounts in this session
reproduced the same signature at teardown on the unmodified dev-merged
release binary with clean lease counters. The storm phase itself passes
(no starvation, leases 0 parked / 0 outstanding, byte-exact
round-trips); only the environmental teardown assertion trips, on code
this branch does not touch. Per the design's risk posture the test is
**not deleted or weakened** — it is the loud detector this follow-up
needs, and it hard-fails the lease class it was built to catch. All six
transport sinks-table canary tests in the same binary pass.
