# 2026-09-10 — Small-file packing: the PK7 rows, LOCAL SCOPING pass (steps 1 + 2)

**Venue: the dev laptop (strixhalo, 32 CPUs, `7.2.3-cachyos-lto`), tcp dev
substrate (`tests/dev_substrate.sh`, nvmet-tcp on localhost: 4 meta + 4
data namespaces, zram-backed), `target/release/squeezefs` @ `5848073a`
(profile `release`). SCOPING ONLY per the venue rule — the space, closure,
and correctness rows below are deterministic and venue-independent; the
throughput/latency numbers are the laptop's and are NOT acceptance. The
acceptance bracket runs on squeeze-test (step 3).**

Rig: `.benchmarks/rigs/2026-09-10-packing-rows-local.sh` (rows `dismount
→ compact → fsyncrow`, arms A = `SQUEEZEFS_SMALL_FILE_PACKING=0` — the
shipped one-block-per-file promotion — and B = `1`; fresh format WITH a
staging dir per arm; every row ends with a remount at a DIFFERENT mount
point with the C8 oracle armed, a byte-exact read of every file, and
online fsck). Reducer: `2026-09-10-packing-rows-reduce.py`. Raw:
`target/packing-rows/` (rows.jsonl, per-row `.stats0/1`, daemon logs,
fio JSON, defrag report JSON). Run wall: 48 s for all six rows.

---

## 1. The rows

### 1.1 dismount — 2,000 small files (8/16/32/64 KiB round-robin, 58.7 MiB, `write`+`close`, no fsync), `syncfs`, `squeezefs umount`

| arm | umount wall | blocks after | daemon summary | second mount point: byte-exact / drift / fsck |
|---|---|---|---|---|
| A | 0.14 s | **2,000** (8 GiB) | `2000 staged-layout file(s) … 0 inline, 0 packed, 2000 to blocks` | 2,000 / 2,000 · 0 · 0 |
| B | 0.14 s | **15** (60 MiB) | `… 0 inline, 2000 packed, 0 to blocks` | 2,000 / 2,000 · 0 · 0 |

The 64× law (design §1.2, `.benchmarks/2026-09-09-fsync-promote-staged-ab.md`)
is gone: 58.7 MiB of files cost 60 MiB of blocks (15 packs, ≈ 133 tenants
each) instead of 8 GiB. Every file reads byte-exact from a mount point
with a different staging scope — i.e. from the data plane, which is the
cross-client visibility the residue note asked for.

### 1.2 compact — `defrag --pack` on the dismount population (lever ON on both)

| arm | population | report-only | `defrag --pack` | after | byte-exact / fsck / drift (remount) |
|---|---|---|---|---|---|
| A | the LEGACY one-block-per-file volume (2,000 blocks) | **"reply too large"** — FIND-PK-6 (§3) | 0.5 s, 2,000 tenants moved, 2,000 blocks freed | **15 blocks** | 0 mismatches · 0 · 0 |
| B | 15 packs, then every other tenant deleted | 15 blocks, 15 below half, mean occupancy 0.33, reclaimable 40.5 MiB | 0.5 s, 1,000 tenants moved, 15 freed | **5 blocks** | 0 mismatches (1,000 survivors) · 0 · 0 |

Row A is the operator story for EXISTING volumes: a volume the shipped
dismount pass filled at one block per file recovers the space with one
`defrag --pack`, no reformat. Row B is the steady-state story: deletes
leave half-empty packs, the D1 face names them, compaction re-packs the
survivors into a third of the blocks.

### 1.3 fsyncrow — the rejected fsync-promotion lever's row, re-run under packing

`SQUEEZEFS_FSYNC_PROMOTE_STAGED=1` on BOTH arms (promotion at fsync); fio
psync, create + fsync-on-close, 16 KiB, 8 jobs × 250 files = 2,000 files
(bounded by count — the A arm's space law fills the volume otherwise).

| arm | files/s | clat p50 / p99.9 µs | fsync total (staged_promote) µs | promoted (packed) | **blocks** | device/user bytes | wareq-sz | daemon CPU µs/file |
|---|---|---|---|---|---|---|---|---|
| A | 828 | 38 / 204 | 200 (110) | 2,000 (0) | **2,000** | 1.00× | 16 KiB | 695 |
| B | 942 | 33 / 245 | 192 (99) | 2,000 (2,000) | **8** | 1.00× | 16 KiB | 589 |

Oracle drift 0 on both remounts; fsck 0; `invariant_tripwires` 0. The
write-amplification columns are identical (a 16 KiB tenant is one 16 KiB
DMA on either arm — packing changes WHERE the DMA lands, not how many
bytes move). The throughput/latency deltas (+14 % files/s, −15 % CPU/file,
p99.9 +20 %) are a single laptop pair and mean nothing yet — the box
bracket decides them.

## 2. Step 2 — fstests QUICK, both postures

`sudo env SQUEEZEFS_SMALL_FILE_PACKING={1,0} FSTESTS_QUICK=1 bash
tests/run_fstests.sh` (the runner bakes the knob into its mount helper;
the live TEST-device daemon's `/proc/<pid>/environ` carried the lever).

| posture | ran | clean | expected-shape | unexpected | wall |
|---|---|---|---|---|---|
| lever ON | 45 | 42 | 3 (003 noatime, 213 thin, 798 write-through — the pinned diffs) | **0** | 18 min |
| lever OFF | 45 | 42 | 3 (same) | **0** | 15 min |

The QUICK set is the standing regression set (every fstests case that has
ever caught a real SqueezeFS bug + the fsx/fsstress soakers + the
hole/punch/seek family + mount cycles) — the external suite that reaches
the paths PK1–PK6 changed live (the read funnel, the truncate clip, the
clone pin, the C8 fixes, the dismount promotion). pjdfstests/LTP do not
reach them and stay the 1.2.3 release gate.

## 3. FIND-PK-6 — the report was unbounded on the wire (fixed `4fc4aa59`)

`defrag --report-only --json` on the legacy 2,000-block volume answered
`reply too large; use the offline probe` — the admin lane refuses any
body past `ADMIN_BODY_MAX` (60 KiB) and the PK6 pack face carried one row
per pack block, which on a one-block-per-file volume is one row per FILE
(≈ 300 KiB). The compaction itself ran; the MEASUREMENT face went dark on
exactly the volume compaction exists for. Same class as the PR 8 fsck
"152-finding wart", same remedy: `DefragReport::to_bounded_json` serves
the aggregates exact, the longest fitting WORST-occupancy row prefix (the
constructor now orders rows worst-first — the order an operator acts in)
and `rows_elided` (`rows.len() + rows_elided ≡ blocks`); the durable
per-block table stays the offline probe's. Contract 14 of
`tests/pack_compaction_tests.rs` (red → green), contract 12 pins the
order. The rig's `compact-A` row will read the aggregates on the next run.

## 4. What this pass establishes, and what it does not

Established (deterministic, venue-independent): the space law (2,000
files → 15 blocks at dismount, 8 at fsync-promotion), cross-client
byte-exactness through the data plane, the oracle/fsck closure across
every row, the legacy-volume recovery path, the compaction of half-empty
packs, and no regression in the standing fstests regression set under
either posture. Not established (the box's): the fsync-row files/s and
latency deltas, which are the one thing step 3's A-B-B-A bracket adds.

Board: the `sparse_write_bounded_tests::cacheless_far_write_is_bounded_and_omap`
flake seen on the PK6 gate is pre-existing (2/20 on the pre-PK6 tree,
0/20 on PK6's tip; an exact ~2 GiB VmHWM jump inside a 4 KiB write's
window = a lazily-committed process-wide pool's first touch the test's
warm-up does not reach) — attribute the region (smaps diff at failure)
and warm it in `warm_io_pools`.
