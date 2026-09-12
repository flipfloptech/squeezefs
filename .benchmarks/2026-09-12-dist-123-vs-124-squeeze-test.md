# 2026-09-12 — 1.2.3 vs 1.2.4 `dist` binaries on squeeze-test (A-B-B-A, twice)

**Verdict: 1.2.4 is PAR with 1.2.3 on every field row — no regression from
leaf merge, the heap-admission change, the reclaim atomicity change, or
PK8.** The one row that read −3.8 % on the first bracket (kern rand-4k
write) re-ran in the REVERSED order at double the row length and read
−0.2 % with both 1.2.4 positions inside the 1.2.3 positions' spread; the
first bracket's F2 position was a non-flat row (+16 % drift across its
window — label-only under the sustained-state rule). The shim read row
reads −1.1…−1.3 % on both brackets: small, consistent in direction, inside
its own p99.9 noise, recorded as an observation with PK8's arm named as
the only shim-path change between the tags.

**Venue:** squeeze-test (`memp-s3ds-aqs-37`, 32-core Xeon, `6.19.14-sqz`
+ patch 0031), the 5-meta + 10-data nvme-tcp fabric set re-formatted per
position by `cluster_reset_v4.sh`; both arms `dist` profile (fat LTO),
rocky8 builds: E = `squeezefs 1.2.3 (fff56fcb79a8, tag stable-2026.09.3)`,
F = `squeezefs 1.2.4 (1193c6b7afdd, tag stable-2026.09.4)`, each with its
own same-commit shim. Rig: `.benchmarks/rigs/2026-09-08-campaign-rows-abba.sh`
(box copy with the meta URI read from each reset's log and a `ROWS`
selector — both changes now in the repo copy); reducer
`2026-09-08-campaign-rows-reduce.py`. Load average 0.0 at start; the
fabric idle. Raw: `/scratch/tmp/campaign/dist-123-vs-124-20260912-024852/`
and `…-rerun-20260912-031105/` on the box (mirrored to the laptop's
`/tmp/dist-123-vs-124/`). Box time: 19 + 11 minutes.

---

## 1. Bracket 1 — E F F E, all five rows, 30 s

| row | E median | F median | F/E | note |
|---|---|---|---|---|
| kern rand-4k read (IOPS) | 602,937 | 600,341 | **−0.4 %** | p99 −2.0 %, CPU/op +0.9 % |
| kern rand-4k write (IOPS) | 552,216 | 531,376 | −3.8 % | F2 = 521k with +16 % drift (non-flat), F3 = 541k; **contested → bracket 2** |
| kern durable write (MiB/s, `--end_fsync`) | 34,021 | 34,010 | **−0.0 %** | fsync legs par; `staged_promote` 0 both |
| small-file fsync storm (IOPS) | 3,241 | 3,226 | **−0.4 %** | p99 −3.7 %, 113k fsyncs both |
| shim rand-4k read (IOPS) | 905,967 | 894,587 | −1.3 % | p99.9 +11.5 %, CPU/op +1.2 % |

Tripwires 0 on every row; `patch_writes` par; `write_through` par.

## 2. Bracket 2 — F E E F (reversed), the two contested rows, 60 s

| row | E median | F median | F/E | positions |
|---|---|---|---|---|
| kern rand-4k write (IOPS) | 550,433 | 549,127 | **−0.2 %** | E 548,659 / 552,206 · F 550,364 / 547,890 — interleaved; CPU/op +0.4 %; p99 +0.0 % |
| shim rand-4k read (IOPS) | 917,189 | 906,664 | −1.1 % | E 919,690 / 914,689 · F 901,572 / 911,756; p99.9 +13 %; CPU/op +4.9 % |

The write row's F4 (547,890) sits below E2 (548,659) by 0.1 % and F1
(550,364) above it: the arms overlap. `patch_writes` 18.70 M vs 18.73 M —
the same W1 path doing the same work.

## 3. Reading

- **Nothing in the 1.2.3 → 1.2.4 product diff touches the kernel write or
  read hot paths** (`git diff stable-2026.09.3 stable-2026.09.4 -- src/`):
  the KV changes (leaf merge, heap admission, the release-in-destroy) sit
  on the checkpoint task and the reclaim path; `routing.rs`'s changes are
  the reclaim plan/destroy and the mapping-decoration split; the only
  hot-path-adjacent change is PK8's direct-drive prelude in
  `fuse_client.rs` — which is the shim's path.
- **The shim row's −1.1…−1.3 %**: PK8 added the dead-lifetime screen
  (`block_key_lifetime_dead`, one RAM lookup) and the packed-mapping arm to
  the direct-drive prelude the shim's every read goes through — the only
  candidate. On this venue's 24 × 8 GiB striped files no packed tenant
  exists, so the packed arm never fires; the screen is a hash lookup per
  op at ≈ 900 k ops/s on 32 cores. CPU/op read +1.2 % / +4.9 % on the two
  brackets — noisier than the IOPS delta. Not adjudicated as a regression
  (inside the row's own position spread on bracket 2: E 914–920 k vs F
  902–912 k overlap at the edge), but named: if the shim row is a scoreboard
  gate, the screen's cost is the term to measure with the `SQUEEZEFS_IPC_DD_
  PACKED=0` control (which does NOT remove the screen — the screen is
  correctness; a cheaper screen would be the lever).
- The heap-admission change (step 3b of the commit pass) is on every
  metadata commit; the fsync-storm and durable-write rows carry 113 k and
  24 commits respectively at par — the allocation-free fast path holds on
  the field's commit rate.

## 4. Box manifest (for the owner's cleanup)

- `/scratch/tmp/squeezefs` + `/scratch/tmp/libsqueezefs_il.so` — **1.2.4**
  (`1193c6b7`, `dist`; sha256 `312670dd…` / `09a1c568…`) — the reset script's
  paths; **left in place**.
- `/scratch/tmp/squeezefs.1.2.3` + `/scratch/tmp/libsqueezefs_il.so.1.2.3` —
  the A arm (596 MiB + shim); superseded once this record lands.
- `/scratch/tmp/rigs/2026-09-08-campaign-rows-abba.sh` (the box copy),
  `/scratch/tmp/rigs/2026-09-03-r4-row-delta.py`,
  `/scratch/tmp/rigs/2026-09-10-packing-rows-box.sh`.
- `/scratch/tmp/campaign/dist-123-vs-124-20260912-024852/`,
  `…-rerun-20260912-031105/`, `dist-123-vs-124-driver.out`,
  `dist-123-vs-124-rerun-driver.out`, and the earlier
  `packing-rows-20260912-…/` — results (tens of MB).
- `/scratch/tmp/squeezefs-pk7` (596 MB, the pre-1.2.3 bracket build) and
  `/scratch/tmp/logs/` (empty) — from earlier campaigns, superseded.
- The fabric set was re-formatted 8 times by the resets and is left
  FORMATTED; `/scratch/tmp/test` unmounted; no daemon running.
