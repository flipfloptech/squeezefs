# 2026-09-10 — Small-file packing: the PK7 acceptance bracket on squeeze-test (A-B-B-A)

**Verdict: the default flip is ADMISSIBLE on the acceptance venue.** Packing
is throughput-, latency-, CPU- and amplification-**par** with the shipped
one-block-per-file promotion on the pricing row (files/s +0.5 %, fsync
−3.7 %, CPU/file −2.2 %, p99.9 +7.5 % — all inside the bracket's own
position-to-position spread), and it changes the SPACE law by the factor
the design promised: **96,000 fsync-promoted 16 KiB files cost 375 blocks
instead of 96,000 (256×); 20,000 dismount-promoted small files cost 148
instead of 20,000 (135×); the legacy one-block-per-file volume recovers
20,000 → 147 blocks with one `defrag --pack` in 5.5 s.** Every row closed:
C8 oracle drift 0, online fsck 0 findings, every file byte-exact from a
second mount point, `invariant_tripwires` 0, transport overdue 0, rescues 0
— on all four positions.

**Venue:** squeeze-test (`memp-s3ds-aqs-37`, 32-core Xeon, `6.19.14-sqz`
with patch 0031), the 5-meta + 10-data nvme-tcp fabric set re-formatted
per position WITH a RAM-backed staging dir (`/dev/shm/sqz-staging`, 64 GB
budget — the fleet is cache-less and its local disk is SATA; the staged
layout has to exist for promotion to have a subject). ONE binary:
`squeezefs 1.2.2 (d914b6730a0d) profile release`, rocky8 build of the gated
`dev` tip. Instrument: fio 3.x psync (`create_on_open`, `fsync_on_close`)
for the pricing row, a deterministic Python populate for the dismount row,
`/proc/diskstats` on the data namespaces for the amplification columns,
statvfs for blocks. Sequence **A B B A** (A = `SQUEEZEFS_SMALL_FILE_PACKING=0`,
B = `1`), 20:06–20:17 UTC, load average 0.00 at start. Rig:
`.benchmarks/rigs/2026-09-10-packing-rows-box.sh`; reducer
`2026-09-10-packing-rows-reduce.py`; raw:
`/scratch/tmp/campaign/packing-rows-20260910-200617/` on the box (mirrored
to the laptop's `/tmp/pk7-box/`).

---

## 1. The pricing row — create + fsync-on-close, 16 KiB × 96,000 (24 jobs × 4,000), promotion at fsync on both arms

`SQUEEZEFS_FSYNC_PROMOTE_STAGED=1` on both arms: every fsync promotes its
staged file (the lever the 2026-09-09 A/B priced and REJECTED on the space
law — this row is that row, re-run under packing). Count-bounded, not
time-bounded: the A arm's 96,000 blocks = 384 GiB of the set.

| pos | arm | files/s | clat p50 / p99.9 µs | fsync total (staged_promote) µs | promoted (packed) | **blocks** | dev/user bytes | wareq-sz | CPU µs/file | tripwires / overdue / rescues |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 4,238 | 43 / 122 | 293 (97) | 96,000 (0) | **96,000** | 1.00× | 16 KiB | 1,025 | 0 / 0 / 0 |
| 2 | B | 4,162 | 43 / 124 | 286 (87) | 96,000 (96,000) | **375** | 1.00× | 16 KiB | 1,023 | 0 / 0 / 0 |
| 3 | B | 4,283 | 42 / 126 | 280 (87) | 96,000 (96,000) | **375** | 1.00× | 16 KiB | 995 | 0 / 0 / 0 |
| 4 | A | 4,163 | 43 / 111 | 296 (97) | 96,000 (0) | **96,000** | 1.00× | 16 KiB | 1,039 | 0 / 0 / 0 |

| per-arm median | A | B | B/A |
|---|---|---|---|
| files/s | 4,200 | 4,222 | **1.005** |
| clat p50 µs | 43.0 | 42.2 | 0.982 |
| clat p99.9 µs | 116.7 | 125.4 | 1.075 |
| fsync total µs | 294 | 283 | **0.963** |
| blocks minted | 96,000 | 375 | **0.0039** |
| device / user bytes | 1.00 | 1.00 | 1.000 |
| daemon CPU µs/file | 1,032 | 1,009 | 0.978 |

Both orders agree (A1 ≈ A4, B2 ≈ B3 within 1.7 % / 2.9 % on files/s), so
nothing here is an ordering artifact. The amplification columns are
identical by construction — a 16 KiB tenant is one 16 KiB DMA on either
arm; packing changes where the DMA lands, not how many bytes move
(`wareq-sz` 16 KiB both). The fsync's `staged_promote` phase is 10 µs
CHEAPER under packing (87 vs 97 µs: a slot reservation is a `fetch_add`,
a block allocation is the allocator's pick + placement + a lane
reservation). The oracle remount read drift 0 on all four positions.

## 2. The dismount row — 20,000 small files (8/16/32/64 KiB, 587 MiB, `write` + `close`, no fsync), `syncfs`, `squeezefs umount`

| pos | arm | populate | unmount wall | daemon summary | **blocks** | second mount point: byte-exact / drift / fsck |
|---|---|---|---|---|---|---|
| 1 | A | 15.3 s | 2.12 s | `20000 … 0 packed, 20000 to blocks` | **20,000** (80 GiB) | 20,000 / 20,000 · 0 · 0 |
| 2 | B | — | 2.22 s | `20000 … 20000 packed, 0 to blocks` | **148** (592 MiB) | 20,000 / 20,000 · 0 · 0 |
| 3 | B | — | 1.93 s | same | **148** | 20,000 / 20,000 · 0 · 0 |
| 4 | A | 15.9 s | 2.12 s | `… 20000 to blocks` | **20,000** | 20,000 / 20,000 · 0 · 0 |

The dismount pass (the fstests TEST device's shape: hundreds to
thousands of never-fsynced small files promoted at unmount) costs 592 MiB
of blocks for 587 MiB of files instead of 80 GiB, in the same unmount wall
(≈ 2 s for 20,000 promotions either way — the pass is metadata-bound), and
every file reads byte-exact from a different staging scope, i.e. from the
data plane: the cross-client visibility the residue note asked for.

## 3. The compaction row — the LEGACY one-block-per-file volume (positions 1 and 4), lever ON

| pos | report-only (aggregates; `rows_elided` = the bounded view working) | `defrag --pack` | after | byte-exact / fsck / drift (remount) |
|---|---|---|---|---|
| 1 | 20,000 blocks, 20,000 below half, occupancy mean 0.0073 (worst 0.0020), reclaimable 77.6 GiB, rows kept 387 / elided 19,613 | **5.5 s**, 20,000 tenants moved, 20,000 blocks freed | **147 blocks** | 0 mismatches · 0 · 0 |
| 4 | same (rows kept 386 / elided 19,614) | 5.5 s, 20,000 / 20,000 | **147** | 0 · 0 · 0 |

This is the operator story for volumes the shipped code has ALREADY
filled at one block per file: one `defrag --pack` (throttled, on the job
fabric) recovers 77.5 GiB of 80 GiB in 5.5 s with every file intact and
the ledger closed. FIND-PK-6's bounded report served the aggregates with
19.6 k rows elided where the unbounded one refused the reply.

## 4. Verdict against the design's acceptance rows (design §7 / PK7)

| row | target | result |
|---|---|---|
| (1) fstests-shape dismount: blocks ≈ data / 4 MiB, not one per file | 20,000 → ≈ 147 | **148** (MET); unmount wall par |
| (2) the rejected fsync-lever row re-run: space ≈ data; latency stays priced | 96,000 → ≈ 375 | **375** (MET); fsync −3.7 %, files/s par |
| (3) create+fsync files/s par; cross-client byte-exact | par; 100 % | **+0.5 %**; 100 % on all positions (MET) |
| (4) write-amplification columns | device/user ≈ 1.0, `wareq-sz` = bs | **1.00× / 16 KiB both arms** (MET) |
| (5) compaction: legacy volume recovers; half-empty packs compact | ≥ 100× on legacy | **20,000 → 147** (MET); the half-delete row was proven locally (15 → 5) |

**Decision input for the owner: flip `SQUEEZEFS_SMALL_FILE_PACKING` to
default ON.** The lever stays as the A/B control (`0` = today's shape,
byte-identical — pinned). What the flip does NOT change: files ≤ 4 KiB stay
inline; files > 2 MiB (the `pack_max_slot_bytes()` law) take their own
block; striped files are untouched; a co-writer packs only where its
authority serves the group frame (PK4) and falls back one-block-per-file
otherwise; every existing one-block-per-file volume keeps working and
recovers its space with `defrag --pack`.

## 5. What this bracket created on squeeze-test (for manual cleanup)

- `/scratch/tmp/squeezefs-pk7` — the rocky8 binary (596 MiB, sha256
  `18acbd34…f41382`)
- `/scratch/tmp/rigs/2026-09-10-packing-rows-box.sh` — the rig
- `/scratch/tmp/campaign/packing-rows-20260910-200617/` — results (103
  files incl. per-row stats, fio JSON, daemon logs, the reset variant
  `reset-staging.sh`); `/scratch/tmp/campaign/packing-rows-driver.out` —
  the driver's stdout
- `/scratch/tmp/logs/` — an EMPTY directory the reset script creates as its
  mount-log home (its standing convention; the rig's mounts log elsewhere)
- Created AND removed by the rig: `/dev/shm/sqz-staging`, `/scratch/tmp/test2`
- The fabric's test set was re-formatted eight times (one reset per row);
  it is left FORMATTED (the last reset's format, lever-independent) with
  `/scratch/tmp/test` unmounted and empty; no daemon is running.
