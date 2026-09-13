# 2026-09-13 — Symmetric metadata PR 1: the solo re-gate (design gate 1) on squeeze-test

PR 1 of the symmetric shared-disk metadata program
([`docs/design-symmetric-metadata.md`](../docs/design-symmetric-metadata.md))
landed the slot-tree FOREST DARK ([`.benchmarks/2026-09-13-sym-forest.md`](2026-09-13-sym-forest.md)):
incompat bit 17 is stamped by nothing but the format-time test seam
`SQUEEZEFS_TEST_STAMP_SYMMETRIC=1`, every default-format volume stays FLAT
(the three per-kind trees), and the flat code path now runs through the
kind-routed helpers with `TreeSet::Flat` borrowing its per-kind tree. Gate 1
(§8) is the program's "evidence this program must NOT move": **a single
mount stays within noise of `dev` tip with `dlm_rpcs == 0`** — within
noise, not byte-identical (§2 Non-Goals: the forest changes the on-disk
layout, so the gate is a measured bracket, never a sector diff). This note
is the gate-1 row at PR 1: the A-B-B-A on the field box, the stamped
SCOPING leg beside it, and the engagement/tripwire reads per arm. Prior
shape: the solo re-gate law and its first row
([`.benchmarks/2026-08-15-mw-s4-regate.md`](2026-08-15-mw-s4-regate.md)),
the binary-arm A-B-B-A rig this one derives from
([`.benchmarks/2026-09-08-campaign-rows-squeeze-test.md`](2026-09-08-campaign-rows-squeeze-test.md)),
and the venue's most recent same-shape rows
([`.benchmarks/2026-09-12-dist-123-vs-124-squeeze-test.md`](2026-09-12-dist-123-vs-124-squeeze-test.md)
— 34.0 GB/s durable write, the same ceiling this note reads).

**Verdict: gate 1 MET at PR 1.** Every acceptance row (`w_fresh`, kern
rand-4k read, kern rand-4k write, mount time, the seven mdstorm phases)
reads **within noise** of the pre-PR tip across the A-B-B-A; `dlm_rpcs`
read **0 on every one of the 27 mount legs** (both binaries, every posture,
including the stamped forest); `invariant_tripwires`, `fsck_findings`,
`meta_kv_block_refs_drift` and the five new forest gauges stayed at 0 on
every flat mount. The two rows that read DELTA in bracket 1 (`remount`
and `mdstorm mkdir`, both in B's favour) were re-run once per the
single-bracket rule and do not reproduce (§4). Details, the honest
residual (`rw4k` −1.5 % in both orders, inside the 3 % floor) and what
this row does NOT cover are below.

## 1. Venue

| | |
|---|---|
| box | `squeeze-test` (memp-s3ds-aqs-37): 32-core Xeon, 251 GiB, Rocky 8.10, kernel **`6.19.14-sqz`** (the sqz series incl. patch 0031), idle at the first leg (load 0.00), no heat soak (the hottest hwmon sensor read 47–51 °C at every row start AND end — the dev-box heat-soak law is why the row runs here) |
| fabric / substrate (fio rows) | the reset-v5 CONVERGED epoch (`/scratch/tmp/cluster_reset_v4.sh`, user decision 2026-08-02): **5 storage nodes × (1 meta + 2 data) namespaces over nvme-tcp**, two paths each, round-robin iopolicy — meta = memory-backed null_blk 8 GiB/node (`sqmeta:///dev/nvme{0,2,4,6,8}n1`), data = memory-backed null_blk 48 GiB × 10 (`sqdata:///dev/nvme{10..28 even}n1`, the THROUGHPUT-CEILING venue: no compression, discard spotty), **cache-less format** (CACHE_DIR empty — beyond-inline writes route striped, no staging). Storage nodes aqr37/38/39, aqs38, oss2 (`4.18.0-553.123.1.el8_lustre.ddn17`), targets kernel **nvmet** (R-SYM-8; SPDK retired). Every arm gets a FRESH reset: teardown + rebuild backings + re-share + reconnect + `format` — the device-thin-state / free-list aging the A-B-B-A rule exists for is reset per arm, and the reversed order still runs |
| substrate (mdstorm rows) | **the instrument's own file-backed substrate**: `tests/run_mdstorm.sh leg` formats a 2 GiB meta image + 8 GiB data image + a staging dir under `/dev/shm/sqz_mdstorm` (tmpfs, 126 GiB) per leg and mounts it with the arm's binary — NOT the fabric set. Barrier-bound (`fdatasync` on tmpfs), so its rows are RELATIVE, both arms on the identical substrate; the leg runs with NO fabric daemon up, and its quiet gate waited for load1 < 2.0 before every leg (the previous arm's fio echo takes ~3 min to decay — the `leg wall` line in the log is mostly that wait) |
| binaries | **A** = dev `3228fcb8` (the tip BEFORE PR 1), **B** = dev `a9827378` (PR 1 landed — code-identical to `73d3e74e`, the run-log commit on top); both `squeezefs 1.2.4 … profile release` (the dev/A-B profile per the two-profile LTO law — both legs the same profile), rocky8 builds from the laptop's `task build:rocky8`, sha256 `fde667aaf11dffb3…` (A) / `4794c0364e13c57d…` (B) verified against `SHA256SUMS.local` on the box. Placed at `/scratch/tmp/sym-pr1/squeezefs-{A,B}` by the orchestrator |
| instrument | `fio-3.36`, the box's standing job files `/scratch/tmp/fio_jobs/{write_BW,randread_iops,randwrite_iops}.job` (libaio, `direct=1`, 24 jobs, `directory=/scratch/tmp/test/client_validation/`, `filename_format=test.$jobnum.$filenum.root`, `time_based`, `ramp_time=10`): `write_BW` 1 MiB × qd 16 × `size=8g`; `rand{read,write}_iops` 4 KiB × qd 8 × `size=1g`. **`RT=60`** on every fio row (the sustained-state rule: 60 s + 10 s ramp; the bw-log first-vs-last-third flatness is a column). Kernel FUSE path only (`--interception` armed as in the campaign rig, no shim rows — the shim takes neither handler) |
| rows per arm (in order) | `mdstorm` leg (mkdir 20k → create 100k → stat → rename → unlink → manydirs 100k → rmdir 20k, 8 threads, 100 % scale) → the TIMED first `mount` (wall clock from `exec` to `--daemon`'s return = the child's "ready" handshake, and to the first successful `.stats` read) → **`wfresh-kern`** (`write_BW` on the NEVER-written file set — no prep before it) → the campaign prep (`write_BW` 40 s, ramp 0 — the rand rows' precondition, identical to the 2026-09-08 rig) → `rr4k-kern` → `rw4k-kern` → a TIMED clean unmount (`fusermount3 -u`, daemon exit awaited) + TIMED `remount` of the populated set |
| order | **bracket 1: A B B A** (17:31 → 17:57 UTC, one pass, `rows-20260913-173118`); **bracket 2 (the re-run of the two DELTA rows): A B B A** over `mdstorm wfresh-kern remount` (18:00 → 18:17 UTC, `rows-20260913-173118-rerun`); **S** (stamped SCOPING, one leg, 18:17 → 18:24 UTC, after both brackets) |
| rig / reducer | [`.benchmarks/rigs/2026-09-13-sym-pr1-solo-regate.sh`](rigs/2026-09-13-sym-pr1-solo-regate.sh) (the 2026-09-08 campaign-rows rig's shape: `reset_arm` → `mdstorm_leg` → `mount_timed` → `row` → `umount_timed`; prints the gate reads per row and exits 3 if `dlm_rpcs` ever moves or a forest key violation is counted) → [`.benchmarks/rigs/2026-09-13-sym-pr1-solo-regate-reduce.py`](rigs/2026-09-13-sym-pr1-solo-regate-reduce.py) (medians per arm, B/A of the medians, the noise band from the two same-arm positions, the verdict rule below; runs on the box's Python 3.6) |
| verdict rule | per row: `within noise` iff \|B/A − 1\| ≤ max(noise band, 3 %), where noise band = max(\|A1−A4\|/mean(A), \|B2−B3\|/mean(B)); otherwise `DELTA`, and a DELTA row is re-run once before any verdict is written (a single bracket never convicts) |

### 1.1 What the gate CAN and CANNOT read at PR 1

* The forest is **dark**: A and B both mount FLAT volumes (`features_incompat = 0xffd7` on every meta volume of every A/B arm, bit 17 = 0 — read off sector 0 per arm by the rig). The acceptance row is therefore **flat-vs-flat**: what it measures is the cost of PR 1's relayout of the flat path (the kind-routed helpers, `TreeSet::Flat`'s borrowed trees, the staged-key planners, the forest-aware checkpoint/replay branches that a flat volume must skip) on a solo mount — exactly the R13 risk row.
* The stamped leg **S** (§5) is the forest FOR REAL on the same box (bit 17 via the seam at format), but it is ONE leg against B's two positions and the default format never produces it at PR 1 — **SCOPING** evidence of the forest's cost, never the gate.
* The gate-1 engagement gauges that name later rungs' machinery — `appender_joins`, `manager_role`, `slot_leases_held`, `affinity_ceiling_overflows`, `manager_verbs`, `slot_tree_bytes` / `meta_kv_slot_tree_leaves` — **do not exist yet** (PR 2 appenders, PR 3 manager, PR 4 slot leases); the packing-row re-run and the divisibility-law assertion likewise wait for their machinery. What exists and was read per arm: `dlm_rpcs` (+ `dlm_mode`, `mount_posture`), `invariant_tripwires` and the rest of the must-stay-0 set, `fsck_findings`, `meta_kv_block_refs_drift`, `meta_kv_{journal_entries,journal_bytes,checkpoints,node_appends,node_append_bytes}`, and PR 1's five forest gauges `meta_kv_forest_{slot_trees_minted,root_publishes,key_violations,reader_window_skips,reader_unpublished_children}`.

## 2. Gate 1 — bracket 1 (A B B A, every row)

Primary metric per row; the four positions, the medians, B/A, the band, the verdict. Every position's full detail (clat p50/p99, daemon µs/op, box busy %, flatness, thermal, tripwires, the meta_kv deltas, the forest gauges) is in §7.

| row | primary | A1 | B2 | B3 | A4 | median A | median B | **B/A** | noise band | verdict |
|---|---|---|---|---|---|---|---|---|---|---|
| `wfresh-kern` (write_BW on the fresh set — §7's honesty note on the window) | MiB/s | 34,640 | 34,190 | 34,116 | 33,837 | 34,239 | 34,153 | **0.997** | 2.3 % | **within noise** |
| `rr4k-kern` (randread 4 KiB) | IOPS | 598,582 | 599,501 | 601,789 | 601,959 | 600,271 | 600,645 | **1.001** | 0.6 % | **within noise** |
| `rw4k-kern` (randwrite 4 KiB) | IOPS | 536,122 | 527,220 | 528,675 | 535,475 | 535,798 | 527,947 | **0.985** | 0.3 % | **within noise** (3 % floor — see §4.3) |
| `mount` (fresh format → ready) | s | 0.451 | 0.472 | 0.509 | 0.549 | 0.500 | 0.490 | 0.981 | 19.6 % | within noise |
| `mount` → first `.stats` | s | 0.464 | 0.484 | 0.522 | 0.562 | 0.513 | 0.503 | 0.981 | 19.1 % | within noise |
| `remount` (populated set → ready) | s | 0.655 | 0.601 | 0.530 | 0.660 | 0.657 | 0.566 | 0.860 | 12.6 % | DELTA (B faster) → re-run §4.1 |
| `umount` (clean, after `rw4k`) | s | 8.462 | 8.436 | 7.869 | 8.827 | 8.645 | 8.152 | 0.943 | 7.0 % | within noise |
| `mdstorm mkdir` 20k | ops/s | 6,501 | 6,669 | 6,758 | 6,496 | 6,498 | 6,714 | 1.033 | 1.3 % | DELTA (B faster) → re-run §4.2 |
| `mdstorm create` 100k | ops/s | 5,732 | 5,678 | 5,902 | 5,829 | 5,780 | 5,790 | **1.002** | 3.9 % | within noise |
| `mdstorm stat` 100k | ops/s | 188,854 | 188,053 | 192,007 | 191,736 | 190,295 | 190,030 | **0.999** | 2.1 % | within noise |
| `mdstorm rename` 100k | ops/s | 4,406 | 4,275 | 4,531 | 4,511 | 4,458 | 4,403 | **0.988** | 5.8 % | within noise |
| `mdstorm unlink` 100k | ops/s | 5,154 | 5,089 | 5,185 | 5,220 | 5,187 | 5,137 | **0.990** | 1.9 % | within noise |
| `mdstorm manydirs` 100k | ops/s | 10,972 | 11,162 | 11,119 | 11,494 | 11,233 | 11,140 | **0.992** | 4.6 % | within noise |
| `mdstorm rmdir` 20k | ops/s | 5,679 | 5,722 | 5,888 | 6,088 | 5,884 | 5,805 | **0.987** | 7.0 % | within noise |

Reading the table: the three fio acceptance rows and six of seven mdstorm
phases land at 0.985–1.002 with same-arm bands of 0.3–7 %; the mount legs
are 0.4–0.7 s events whose same-arm spread is 12–20 % (a 50 ms difference is
10 %), so their DELTA is a noise-floor artefact on this scale — bracket 2
below re-reads it. Every row's B position moved the same metadata economy
as its A position: `Δjournal_entries` 55.0–55.1k on every `wfresh` row,
48–51 on every `rr4k`, 4,051–4,098 on every `rw4k`, 547.6–547.7k on every
mdstorm leg; checkpoints 200 / 64–68 / 203–205 / 71–73 respectively; node
appends within ±6 % arm-to-arm — the PR-1 flat path writes the same
records, the same number of times (§7 has the per-position values).

## 3. Engagement and tripwire reads — every mount leg

27 mount legs — 8 arms across the two brackets × {the mdstorm leg's mount,
the first fabric mount, the populated remount} + the stamped leg's 3 — each
read from its `.stats` inode at mount and before/after every row:

| read | A (4 arms, 12 mount legs) | B (4 arms, 12 mount legs) | S (1 arm, 3 legs, §5) |
|---|---|---|---|
| `dlm_mode` / `dlm_rpcs` | `solo` / **0** everywhere | `solo` / **0** everywhere | `solo` / **0** |
| `mount_posture` | `writer` | `writer` | `writer` |
| `Δinvariant_tripwires` (+ `fuse_op_watchdog_overdue`, `transport_cq_overflows`, `transport_lease_overlong`, `write_pipeline_fence_drops`, `data_dma_fence_refusals`, `writeback_errors_latched`, `detached_task_panics`, `job_worker_panics`) | 0 on every row | 0 on every row | 0 on every row |
| `Δfsck_findings` | 0 | 0 | 0 |
| `Δmeta_kv_block_refs_drift` (the C8 tripwire) | 0 | 0 | 0 |
| `meta_kv_forest_slot_trees_minted` | (key absent) | **0** on every flat mount | > 0 (the forest engaging — §5) |
| `meta_kv_forest_root_publishes` | (absent) | **0** | > 0 |
| `meta_kv_forest_key_violations` (must-stay-0) | (absent) | **0** | **0** |
| `meta_kv_forest_reader_window_skips` / `_reader_unpublished_children` (0 on every write mount) | (absent) | **0** / **0** | **0** / **0** |
| sector-0 `features_incompat` (every meta volume) | `0xffd7`, bit 17 = 0 | `0xffd7`, bit 17 = 0 | `0x2ffd7`, **bit 17 = 1** |

The rig's exit code carries the `dlm_rpcs` law: it fails loud (exit 3) if any
leg reads nonzero. Both brackets and the stamped leg exited `gate_failed=0`.

### 3.1 `.stats` field census — B vs A

Top-level `metrics` keys plus one nesting level, from the first fabric mount
of each position: **A 1538 keys, B 1543**. In B and not in A: the five
forest gauges above (all 0 on every flat mount, every position) and two
`meta_kv_commit_sites.src/meta_backend/kv/backend.rs:<line>` entries; in A
and not in B: two `meta_kv_commit_sites…backend.rs:<line>` entries. The
commit-site census is keyed by SOURCE LINE, so PR 1's edits to `backend.rs`
relocated the same two sites (`:6586 → :7231` with count 5, `:13179 → :14445`
with count 2) — a rename, not a new or lost gauge. Nothing else was added or
removed.

## 4. Bracket 2 — the re-run of the DELTA rows (and the residual)

Per the rule (a single bracket does not convict, and a DELTA row is re-run
once — `ROWS="mdstorm wfresh-kern remount"`, one A-B-B-A pass; `wfresh`
rides along because `remount` of the POPULATED set needs the written file
set, and this keeps the shape identical to bracket 1's remount minus the
rand rows):

| row (bracket 2) | primary | A1 | B2 | B3 | A4 | median A | median B | **B/A** | noise band | verdict |
|---|---|---|---|---|---|---|---|---|---|---|
| `remount` (populated set → ready) | s | 0.644 | 0.580 | 0.642 | 0.592 | 0.618 | 0.611 | **0.989** | 10.1 % | **within noise** |
| `remount` → first `.stats` | s | 0.656 | 0.593 | 0.655 | 0.607 | 0.631 | 0.624 | 0.988 | 9.9 % | within noise |
| `mdstorm mkdir` 20k | ops/s | 6,594 | 6,698 | 6,562 | 6,363 | 6,478 | 6,630 | **1.023** | 3.6 % | **within noise** |
| `mdstorm create` | ops/s | 5,872 | 5,869 | 5,774 | 5,808 | 5,840 | 5,822 | 0.997 | 1.6 % | within noise |
| `mdstorm stat` | ops/s | 189,267 | 189,966 | 189,603 | 188,405 | 188,836 | 189,784 | 1.005 | 0.5 % | within noise |
| `mdstorm rename` | ops/s | 4,436 | 4,522 | 4,436 | 4,446 | 4,441 | 4,479 | 1.009 | 1.9 % | within noise |
| `mdstorm unlink` | ops/s | 5,236 | 5,233 | 5,115 | 5,173 | 5,204 | 5,174 | 0.994 | 2.3 % | within noise |
| `mdstorm manydirs` | ops/s | 11,027 | 11,039 | 10,934 | 10,834 | 10,930 | 10,986 | 1.005 | 1.8 % | within noise |
| `mdstorm rmdir` | ops/s | 6,019 | 5,822 | 5,960 | 5,740 | 5,880 | 5,891 | 1.002 | 4.7 % | within noise |
| `wfresh-kern` (rode along) | MiB/s | 33,772 | 34,786 | 34,418 | 34,793 | 34,282 | 34,602 | 1.009 | 3.0 % | within noise |
| `mount` (fresh) | s | 0.423 | 0.504 | 0.399 | 0.394 | 0.408 | 0.452 | 1.105 | 23.3 % | within noise |
| `umount` (clean, after `wfresh` only) | s | 0.944 | 0.934 | 0.954 | 0.939 | 0.942 | 0.944 | 1.003 | 2.1 % | within noise |

Every bracket-2 row is within noise; `dlm_rpcs` 0 on all 12 leg-reads,
tripwires / `fsck_findings` 0, `features_incompat = 0xffd7` (bit 17 = 0)
on every meta volume of every arm, the mdstorm metadata economy identical
(`Δjournal_entries` 547,657–547,691, checkpoints 71–72 on all four legs).

### 4.1 `remount`

Bracket 1 read B/A = 0.860 on a 12.6 % band — B FASTER by 90 ms on a
0.53–0.66 s event. Bracket 2 reads **0.989 on a 10.1 % band**, the four
positions 0.58–0.64 s interleaved by arm (A 0.644 / 0.592, B 0.580 /
0.642). The bracket-1 delta does not reproduce: a populated-set remount is
a ~0.6 s event (open 5 meta volumes' ledgers + journals, replay, the
ownership-recovery walk of a 24-file set, arm 32 FUSE queues, the fabric's
10 data namespaces) whose leg-to-leg spread is 50–130 ms on either binary.
**Within noise**, both brackets cited.

### 4.2 `mdstorm mkdir`

Bracket 1 read B/A = 1.033 on a 1.3 % band — B faster by 3.3 % on the
20k-mkdir phase alone while the other six phases sat at 0.987–1.002.
Bracket 2 reads **1.023 on a 3.6 % band**: the same sign (B 6,698 / 6,562
vs A 6,594 / 6,363), now inside its own band. Both brackets have PR 1
ahead on `mkdir` by 2–3 % and nowhere else; that is a repeatable small
favourable difference on the one phase that runs first on a cold daemon,
not a regression, and the gate's question ("does the solo mount move?")
is answered within noise on both. Recorded, not claimed.

### 4.3 The honest residual: `rw4k-kern` −1.5 % in both orders

`rw4k-kern` reads B/A = 0.985 with a same-arm band of 0.3 % — the two A
positions agree to 0.1 % (536.1k / 535.5k) and the two B positions to 0.3 %
(527.2k / 528.7k), so the −1.5 % is REPRODUCIBLE across both orders on this
bracket and sits below the rule's 3 % floor, not below the measured band.
The verdict rule says `within noise` and the gate's own words ("within
noise") are met; the number is recorded rather than argued away: clat p50
250.9 → 255.0 µs (+1.6 %), daemon µs/op 40.9–41.2 → 41.6–41.7 (+1.4 %),
`patch_writes` proportional to IOPS (18.4M → 18.0M — the row is the W1
in-place patch arm on both binaries, `write_through_blocks` ≈ 2.2–2.3k both),
journal entries / checkpoints identical. The candidate term is the flat
path's kind-routed metadata resolve on the write handler's inode read
(the `TreeSet::Flat` borrow is one relaxed dispatch per record access; the
warm W1 write reads the ino's metadata and the `active_block:` record per
op), and it is at the edge of what one bracket resolves. **Not convicted;
carried to PR 2's re-gate** (the design's R13 row names PRs 1 AND 2 as the
squeeze-test brackets) as the one number to re-read — if it reproduces
there at the same sign with the appender machinery in, the per-op resolve
is the first place to profile (`fuse_op_phase_ns` write + `read_serve_phase_ns.meta_resolve`-class attribution on the write side).

## 5. SCOPING — the forest STAMPED (S = B + `SQUEEZEFS_TEST_STAMP_SYMMETRIC=1` at format)

**Not the gate.** The default format is flat at PR 1; this leg prices the
forest's cost on the same box so PR 2 has a number to compare against. ONE
leg (position 1 after both brackets, box cold) vs B's two positions from
bracket 1 (fio, rr4k/rw4k) and both brackets (mdstorm, mount, wfresh). The
reset's `format` cannot be given the env for that one command (the reset
runs `$SQZ format` on the client with the env inherited by every verb it
runs), so the rig re-issues the reset's format line verbatim under the
seam — `SQUEEZEFS_TEST_STAMP_SYMMETRIC=1 squeezefs-B format --force <sqmeta> <sqdata>`
(cache-less, default bits) — and reads sector 0 of every meta volume:
`features_incompat = 0x2ffd7`, bit 17 = 1 on all five. The mdstorm leg's
own file-backed format is stamped the same way (the env reaches
`run_mdstorm.sh`'s `format`).

| row — **SCOPING** | primary | B2 (flat) | B3 (flat) | **S1 (forest)** | median B | S/B | B band | read |
|---|---|---|---|---|---|---|---|---|
| `wfresh-kern` | MiB/s | 34,190 | 34,116 | 34,361 | 34,153 | **1.006** | 0.2 % | within noise of B |
| `rr4k-kern` | IOPS | 599,501 | 601,789 | 597,204 | 600,645 | **0.994** | 0.4 % | within noise of B |
| `rw4k-kern` | IOPS | 527,220 | 528,675 | 530,336 | 527,947 | **1.005** | 0.3 % | within noise of B |
| `mount` (fresh format → ready) | s | 0.472 | 0.509 | 0.438 | 0.490 | 0.893 | 7.5 % | a 0.4–0.5 s event; noise |
| `remount` (populated set: 24 files in **20 guest slot trees + native**) | s | 0.601 | 0.530 | **0.801** | 0.566 | **1.416** | 12.6 % | **+0.2–0.27 s absolute** — the forest opens tree 0, then 20 guest trees it names, then replays per slot (bracket-2 B remounts read 0.58–0.64, so the delta vs the flat population of four B remounts is +0.16–0.27 s) |
| `umount` (clean, after `rw4k`) | s | 8.436 | 7.869 | 8.248 | 8.152 | 1.012 | 7.0 % | same as flat |
| `mdstorm mkdir` | ops/s | 6,669 | 6,758 | **7,172** | 6,714 | **1.068** | 1.3 % | forest FASTER |
| `mdstorm create` | ops/s | 5,678 | 5,902 | **6,125** | 5,790 | **1.058** | 3.9 % | forest FASTER |
| `mdstorm stat` | ops/s | 188,053 | 192,007 | 189,539 | 190,030 | 0.997 | 2.1 % | par |
| `mdstorm rename` | ops/s | 4,275 | 4,531 | 4,576 | 4,403 | 1.039 | 5.8 % | par (inside band) |
| `mdstorm unlink` | ops/s | 5,089 | 5,185 | 5,258 | 5,137 | 1.024 | 1.9 % | par |
| `mdstorm manydirs` | ops/s | 11,162 | 11,119 | **12,533** | 11,140 | **1.125** | 0.4 % | forest FASTER |
| `mdstorm rmdir` | ops/s | 5,722 | 5,888 | 5,857 | 5,805 | 1.009 | 2.9 % | par |

**Engagement (the forest was real on this leg):** `meta_kv_forest_slot_trees_minted`
= **63** after the mdstorm leg (the 120k-inode storm reached every rotor
slot — 63 guest trees + the native slot = the design's "native + 64 rotor
slot trees" solo shape, §7.3) and **20** after the 24-file fio set;
`meta_kv_forest_root_publishes` 425 on the storm (≤ 68 checkpoints × 63
trees — the checkpoint publishes only MOVED roots), 193 / 319 / 340
cumulative across the three fio rows (200 / 68 / 196 checkpoints);
`meta_kv_forest_key_violations` **0**; the two reader gauges **0** (write
mount); `dlm_rpcs` **0**; tripwires / `fsck_findings` / `block_refs_drift`
**0**; `mount_posture` `writer`. After the remount the mint/publish gauges
read 0 (a fresh process — the remount OPENS the 20 trees tree 0 names and
mints nothing, as designed).

**What the forest costs and saves on a solo mount, priced (one leg — SCOPING):**

* **The fio hot paths do not see it**: `w_fresh` / `rr4k` / `rw4k` at
  1.006 / 0.994 / 1.005 of flat B, daemon µs/op 535 / 31.95 / 41.35 vs
  532–533 / 31.8–31.9 / 41.6–41.7 — the `TreeSet::Forest` dispatch costs
  the data-plane handlers nothing measurable.
* **The metadata storm is FASTER on the forest** in its write-heavy phases
  (mkdir +6.8 %, create +5.8 %, manydirs +12.5 %; rename/unlink +2–4 %
  inside B's band; stat/rmdir par). The plausible mechanism is the one the
  design bought the forest for: 64 slot trees of ≤ 1/64 the leaves each
  instead of one inode tree + one dentry tree, so the conveyor's
  union-leaf-lock pass and the SMOs touch smaller trees with fewer
  colliding leaves — but ONE leg on the file-backed substrate is scoping,
  not a claim; PR 2's bracket (appenders in) is where it gets measured.
* **"Ledger + page writes per checkpoint priced"** (the gate row's item —
  the forest's checkpoint has more trees to flush): on the **storm** the
  journal grew **+1.8 % bytes** (107.49 MB vs 105.60–105.62 MB for 547.2k
  vs 547.6k entries — the slot-prefixed interior keys and 425 root
  publications) and node appends were **par** (24,211 vs 24,428–24,881;
  130.3 MB vs 128.2–129.8 MB); on **`w_fresh`** node appends **+13–20 %**
  (6,961 vs 5,775–6,166; 70.0 MB vs 60.6–63.5 MB) for the same 200
  checkpoints and 54.9k journal entries; on **`rw4k`** node appends
  **3.5–4×** (982 vs 242–284; 4.19 MB vs 1.72–1.97 MB over 60 s) — 20 trees'
  dirty leaves per checkpoint instead of 3 on a scattered-key shape. All
  three are device-idle write bytes on the meta namespaces (the rows'
  throughput did not move); the multiplier on `rw4k` is the number PR 2's
  per-appender checkpoint task with the flush ceiling should re-read.
* **Remount +0.2–0.27 s** with 20 guest trees (0.80 s vs 0.53–0.64 s): the
  per-tree open + per-slot replay. At the design's 65-tree solo shape and
  with a real population this is the mount-time term to watch at PR 2 —
  fresh-format mount (tree 0 + native only) reads no different (0.44 s).
* Sector 0: `0x2ffd7` on all five volumes (bit 17 set beside the fourteen
  default bits); the A binary would refuse such a volume loud (unknown
  incompat) — not exercised here.

## 6. dmesg, thermal, box CPU

* **dmesg**: the rig tails `fuse|WARN|lockdep|BUG|nvme…(error|reset|timeout)` after every row; every tail is the same five BOOT-TIME lines from Sep 6 (`fuse: init (API version 7.45)`, two firmware-bug WMI notices, the PCI host-bridge line, the systemd firewalling notice) — **nothing was logged during any row**, no nvme error/reset/timeout, no fuse warning, on either binary or on the forest.
* **thermal**: the hottest hwmon sensor read 47–51 °C at every row start and end across both brackets and the S leg — the box does not heat-soak, so position effects are venue noise, not clocks (the dev-box law this venue exists for).
* **box CPU** (from `/proc/stat` around each fio row): `wfresh` 69–72 % busy (sys 43–46 %, iowait 26–28 %), `rr4k` 74.2–74.6 % (sys 36 %), `rw4k` 74.4–75.2 % (user 34 %) — the same on both arms to ±1 %, so daemon µs/op (§7) is the finer instrument: `wfresh` 522–535 µs per 1 MiB write both arms (the field's 33–34 GB/s write ceiling is nvme-tcp CPU: ~1 TiB moved per 60 s row), `rr4k` 31.8 µs/op on all four positions, `rw4k` 40.9–41.7 (§4.3).

## 7. Per-position detail

Bracket 1 + the S leg (the reducer's tables verbatim; `flat %` = bw-log
last third vs first third; `°C` = the hottest hwmon sensor; the forest
column drops the three gauges that read 0 on every row —
`key_violations`, `reader_window_skips`, `reader_unpublished_children`).
Every daemon µs/op is `Δdaemon_cpu_ns ÷ fio ios` over the row's
`.stats0/.stats1` pair.

#### fio `wfresh-kern`
| arm | pos | IOPS | MiB/s | GiB moved | clat p50 µs | clat p99 µs | daemon µs/op | box busy % | flat % | °C start | °C end | tripwires Δ | dlm_rpcs | Δfsck_findings | Δjournal entries | Δcheckpoints | Δnode appends | write_through | patch_writes | forest gauges |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| A | 1 | 34,628 | 34,640 | 1025.9 | 7962.6 | 45351 | 522.49 | 69.5 | +3.2 | 47 | 48 | 0 | 0 | 0 | 55,078 | 200 | 6,107 | 2,941 | 0 | — |
| A | 4 | 33,825 | 33,837 | 997.9 | 7962.6 | 47972 | 534.71 | 71.7 | +5.3 | 48 | 49 | 0 | 0 | 0 | 54,962 | 200 | 6,160 | 3,664 | 0 | — |
| B | 2 | 34,178 | 34,190 | 1006.2 | 7700.5 | 49021 | 531.99 | 70.9 | +6.8 | 48 | 49 | 0 | 0 | 0 | 55,086 | 200 | 6,166 | 3,742 | 0 | root_publishes=0 slot_trees_minted=0 |
| B | 3 | 34,104 | 34,116 | 1002.4 | 8224.8 | 46399 | 532.81 | 72.0 | +10.9 | 48 | 48 | 0 | 0 | 0 | 55,044 | 200 | 5,775 | 2,835 | 0 | root_publishes=0 slot_trees_minted=0 |
| S | 1 | 34,348 | 34,361 | 1010.9 | 7700.5 | 48497 | 535.08 | 72.1 | +0.7 | 48 | 49 | 0 | 0 | 0 | 54,853 | 200 | 6,961 | 3,495 | 0 | root_publishes=193 slot_trees_minted=20 |

**What "w_fresh" means at this venue's ceiling (honesty note):** the job
moves ~1 TiB per 60 s at 24 × qd 16 × 1 MiB against a 24 × 8 GiB = 192 GiB
`time_based` file set, so the never-written FIRST pass completes in ≈ 6 s
— inside fio's 10 s ramp — and the measured 60 s window is passes 2–6 over
the same offsets: a steady-state CoW rewrite (displaced-block free + async
reclaim on a device whose discard is spotty) of a FRESHLY FORMATTED set on
both arms alike. What is fresh is the volume, its free list and the
device's thin state (the A-B-B-A aging law's concern — reset per arm), not
every measured byte. A pure first-pass row at 34 GB/s × 70 s would need
> 2.3 TiB of data namespace (the set has 480 GiB) or a one-pass burst that
fails the sustained-state rule; the campaign rig's `wdur-kern` row had the
identical shape, so this row is comparable to it. The bw-log flatness
(+0.7…+10.9 %, last third over first) is rewrite-vs-rewrite, flat on both
binaries.

#### fio `rr4k-kern`
| arm | pos | IOPS | MiB/s | GiB moved | clat p50 µs | clat p99 µs | daemon µs/op | box busy % | flat % | °C start | °C end | tripwires Δ | dlm_rpcs | Δfsck_findings | Δjournal entries | Δcheckpoints | Δnode appends | forest gauges |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| A | 1 | 598,582 | 2,338 | 68.5 | 171.0 | 3359 | 31.80 | 74.2 | +7.6 | 50 | 50 | 0 | 0 | 0 | 51 | 68 | 37 | — |
| A | 4 | 601,959 | 2,351 | 68.9 | 171.0 | 3326 | 31.78 | 74.6 | +3.7 | 50 | 51 | 0 | 0 | 0 | 49 | 64 | 39 | — |
| B | 2 | 599,501 | 2,342 | 68.6 | 169.0 | 3391 | 31.88 | 74.2 | −6.1 | 50 | 51 | 0 | 0 | 0 | 50 | 67 | 38 | root_publishes=0 slot_trees_minted=0 |
| B | 3 | 601,789 | 2,351 | 68.9 | 171.0 | 3293 | 31.80 | 74.4 | −8.0 | 50 | 50 | 0 | 0 | 0 | 48 | 65 | 41 | root_publishes=0 slot_trees_minted=0 |
| S | 1 | 597,204 | 2,333 | 68.3 | 169.0 | 3293 | 31.95 | 74.4 | −6.2 | 50 | 51 | 0 | 0 | 0 | 52 | 68 | 44 | root_publishes=319 slot_trees_minted=20 |

#### fio `rw4k-kern`
| arm | pos | IOPS | MiB/s | GiB moved | clat p50 µs | clat p99 µs | daemon µs/op | box busy % | flat % | °C start | °C end | tripwires Δ | dlm_rpcs | Δfsck_findings | Δjournal entries | Δcheckpoints | Δnode appends | write_through | patch_writes | forest gauges |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| A | 1 | 536,122 | 2,094 | 61.4 | 250.9 | 1434 | 40.87 | 75.2 | +11.3 | 50 | 50 | 0 | 0 | 0 | 4,051 | 204 | 261 | 2,247 | 18,395,504 | — |
| A | 4 | 535,475 | 2,092 | 61.3 | 250.9 | 1434 | 41.20 | 74.4 | +18.7 | 51 | 51 | 0 | 0 | 0 | 4,098 | 205 | 274 | 2,226 | 18,377,617 | — |
| B | 2 | 527,220 | 2,059 | 60.3 | 255.0 | 1466 | 41.68 | 74.9 | +2.5 | 51 | 51 | 0 | 0 | 0 | 4,051 | 203 | 242 | 2,293 | 18,039,937 | root_publishes=0 slot_trees_minted=0 |
| B | 3 | 528,675 | 2,065 | 60.5 | 255.0 | 1434 | 41.63 | 74.7 | +11.3 | 50 | 51 | 0 | 0 | 0 | 4,077 | 205 | 284 | 2,285 | 18,069,755 | root_publishes=0 slot_trees_minted=0 |
| S | 1 | 530,336 | 2,072 | 60.7 | 255.0 | 1417 | 41.35 | 75.1 | +4.4 | 51 | 50 | 0 | 0 | 0 | 4,093 | 196 | 982 | 2,254 | 18,178,177 | root_publishes=340 slot_trees_minted=20 |

`patch_writes` ≈ ios on every position: the row is the W1 sole-owner
in-place patch arm on all three binaries/postures (the 4 KiB O_DIRECT
overwrite of an exclusively owned, passthrough, whole-block-mapped striped
block), `write_through_blocks` 2.2–2.3k = the prep's residue draining, no
overlay engagement — the shape the random-small-write program pinned.

#### mount / remount / umount legs
| arm | pos | fresh mount → return s | → first `.stats` s | populated remount → return s | → first `.stats` s | clean unmount s (after `rw4k`) | rc | dlm_rpcs |
|---|---|---|---|---|---|---|---|---|
| A | 1 | 0.451 | 0.464 | 0.655 | 0.669 | 8.462 | 0 | 0 |
| A | 4 | 0.549 | 0.562 | 0.660 | 0.673 | 8.827 | 0 | 0 |
| B | 2 | 0.472 | 0.484 | 0.601 | 0.612 | 8.436 | 0 | 0 |
| B | 3 | 0.509 | 0.522 | 0.530 | 0.541 | 7.869 | 0 | 0 |
| S | 1 | 0.438 | 0.450 | **0.801** | 0.814 | 8.248 | 0 | 0 |

The daemon log's `FUSE-over-io_uring registered: queues=32 depth=32
payload_sz=1048576 max_write=1048576 max_pages=256
buffers=kmbuf-bufring+zero-copy+retention kmbuf_ops=37/38 (6.19-sqz)
sqpoll=off` line is identical on every leg of every arm — the same
transport geometry, the same zc posture — and `session path armed
(ready=true, queues=32)` follows it on every leg.

#### mdstorm ops/s per phase (file-backed `/dev/shm`, 8 threads, 100 % scale)
| arm | pos | mkdir 20k | create 100k | stat 100k | rename 100k | unlink 100k | manydirs 100k | rmdir 20k | dlm_rpcs | tripwires Δ | Δjournal entries | Δjournal MB | Δcheckpoints | Δnode appends | Δnode-append MB | forest gauges |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| A | 1 | 6,501 | 5,732 | 188,854 | 4,406 | 5,154 | 10,972 | 5,679 | 0 | 0 | 547,641 | 105.60 | 72 | 24,881 | 129.68 | — |
| A | 4 | 6,496 | 5,829 | 191,736 | 4,511 | 5,220 | 11,494 | 6,088 | 0 | 0 | 547,691 | 105.60 | 71 | 24,428 | 128.15 | — |
| B | 2 | 6,669 | 5,678 | 188,053 | 4,275 | 5,089 | 11,162 | 5,722 | 0 | 0 | 547,658 | 105.62 | 73 | 24,754 | 129.41 | root_publishes=0 slot_trees_minted=0 |
| B | 3 | 6,758 | 5,902 | 192,007 | 4,531 | 5,185 | 11,119 | 5,888 | 0 | 0 | 547,642 | 105.60 | 71 | 24,817 | 129.82 | root_publishes=0 slot_trees_minted=0 |
| S | 1 | 7,172 | 6,125 | 189,539 | 4,576 | 5,258 | 12,533 | 5,857 | 0 | 0 | 547,214 | 107.49 | 68 | 24,211 | 130.27 | **root_publishes=425 slot_trees_minted=63** |

Bracket 2's per-position tables (the `mdstorm wfresh-kern remount` re-run)
are in §4's table and in the reducer's output on the box
(`python3 /scratch/tmp/rigs/2026-09-13-sym-pr1-solo-regate-reduce.py /scratch/tmp/sym-pr1/rows-20260913-173118-rerun`).

## 8. Verdict

**Gate 1 is MET at PR 1, in the design's own words:** the solo mount stays
**within noise of `dev` tip** on `w_fresh` (0.997), kern rand-4k read
(1.001) and write (0.985, inside the 3 % floor with a 0.3 % band —
recorded as the residual in §4.3), the seven mdstorm phases (0.987–1.033,
the one out-of-band phase re-run in §4.2), mount time (fresh 0.98 both
brackets; populated remount 0.86 in bracket 1 → 0.99 in the §4.1 re-run —
a 0.6 s event) and clean unmount (0.94 / 1.00);
**`dlm_rpcs == 0`** on every leg by construction and by measurement;
`invariant_tripwires`, `fsck_findings`, `meta_kv_block_refs_drift` and
`meta_kv_forest_key_violations` at 0 throughout; the forest gauges at 0 on
every flat mount (the bit-17-absent volume takes the shipped path — pinned
in the suite, now read live); and the metadata economy per row identical
arm to arm. **Not byte-identical, by design** (§2) — the `.stats` census
gained exactly the five forest gauges and moved two commit-site line
labels. The scoreboard smoke (`SQUEEZEFS_SB_SMOKE=1`) named in the gate
row did not run in this pass — see §10.

The stamped leg is SCOPING only and is reported in §5 for PR 2.

## 9. Issues found

1. **Rig, not product** — the campaign-rows rig's `pkill -x squeezefs` /
   `pgrep -x squeezefs` never matched the arm daemons: a binary named
   `squeezefs-A` has comm `squeezefs-A`. In the first launch the `wfresh`
   row's "daemon up?" check therefore tried a SECOND mount of the same set
   and the D0 single-writer guard refused it loud (`EBUSY … another
   squeezefs process holds the writer lock (claim: id=…, pid=…, age=0s)`) —
   the guard doing exactly what it exists for, and the reason the 17:25
   launch is archived as `rows-20260913-172501-aborted-rigbug` and the
   count restarted from zero at 17:31. The rig now matches the mount argv
   (`squeezefs[-A-Za-z]* moun[t] sqmeta://`), the reset script's own
   pattern. The campaign rig relied on the reset's `pkill -f` for the kill
   and never remounted, so it did not trip on this.
2. **Log-timestamp precision** — the daemon's `FUSE-over-io_uring session
   path armed` line is printed without a timestamp and the INFO lines
   around it carry env_logger's second-precision stamp, so the "armed"
   time in the log resolves to the second only. The wall-clock pair the
   rig records (exec → `--daemon` return, i.e. the child's `ready`
   handshake; exec → first `.stats` read) is the mount-time instrument;
   the log stamp is recorded beside it as a consistency check (it agrees
   to the second on every leg).
3. **Clean-unmount time depends on what ran last** — 7.9–8.8 s after the
   `rw4k` row (bracket 1, and 8.2 s on S) vs 0.93–0.95 s after `wfresh`
   alone (bracket 2), the same on A, B and S. The ~7 s term a rand-write
   row leaves for the teardown is NOT attributed here (it is not a PR-1
   term — both arms carry it); noted because a future mount/unmount-time
   row must state what preceded its unmount, and because 8 s of clean
   teardown after 60 s of 4 KiB patches is a number someone should own.

No product defect surfaced. No load-dependent hang, no watchdog line, no
tripwire.

## 10. Owed

* **The PR-2/PR-3/PR-4 engagement gauges** the gate row names —
  `appender_joins == 1`, `manager_role == held`, `slot_leases_held == 1
  native + 64 rotor`, `affinity_ceiling_overflows == 0`, `manager_verbs ==
  join-time only` — do not exist yet; the gate-1 rows at PRs 2 and 13/14
  add them. Likewise the **packing-row re-run** (blocks ≈ 375,
  `pack_blocks_sealed_dismount ≤ 64`) and the **divisibility-law
  assertion** on mdstorm's `storm`/`manydirs` shape need `slot_tree_bytes`
  / `meta_kv_slot_tree_leaves`, which PR 1 does not export.
* **The scoreboard smoke** (`tests/run_scoreboard.sh` with
  `SQUEEZEFS_SB_SMOKE=1`) named in the gate row: not run — it needs the
  reference clients (JuiceFS / SeaweedFS / geesefs / mountpoint-s3) and a
  RustFS store the box does not have (no internet), and the orchestrator's
  row list scoped this pass to mdstorm + the fio rows + mount time. It is
  the one gate-1 sub-row this note does not cover; the release-gate cadence
  runs it.
* **The `rw4k` −1.5 %** (§4.3) is carried to PR 2's bracket as the one
  number to re-read; a third bracket here would not resolve it below the
  floor the rule already accepts.
* The mdstorm rows here are the file-backed `/dev/shm` substrate the
  instrument ships with; the fabric-substrate mdstorm (a `--format-args`
  lever for a fabric URI does not exist in `run_mdstorm.sh`) is not a gate
  row and was not built for this pass.

## 11. Box footprint — everything created or left on squeeze-test and the storage nodes

Created by this run (all root-owned, none removed — the artifacts are the
evidence, the rigs are reusable at PR 2):

| path | what |
|---|---|
| `squeeze-test:/scratch/tmp/squeezefs` | **a copy of `squeezefs-B`** (`a9827378`, 638,209,296 B, 0755) — the reset script's hardcoded client `SQZ=/scratch/tmp/squeezefs` (its `nvmeof disconnect/connect` + `format` verbs; the 2026-09-12 box clean had removed it). PR 1 does not touch `nvmeof`, so B is the right binary for the reset on both arms |
| `squeeze-test:/scratch/tmp/rigs/2026-09-13-sym-pr1-solo-regate.sh` | the rig (laptop copy `.benchmarks/rigs/…`) |
| `squeeze-test:/scratch/tmp/rigs/2026-09-13-sym-pr1-solo-regate-reduce.py` | the reducer |
| `squeeze-test:/scratch/tmp/rigs/mdstorm/tests/{run_mdstorm.sh,mdstorm.c}` | the packaged mdstorm instrument (verbatim from `tests/`; `run_mdstorm.sh` derives `REPO_DIR` from its location, so the `tests/` nesting is what makes `cc tests/mdstorm.c` resolve) |
| `squeeze-test:/scratch/tmp/sym-pr1/rows-20260913-173118/` (+ `.log`) | **bracket 1** artifacts: per row `.stats0/.stats1/.fio.json/.fio.txt/_bw.*.log/.procstat*/.thermal*/.dmesg`, per arm `reset-*.log`, `features-*.txt`, `*.mount.*` / `*.remount.*` / `*.umount.*` (timed legs + daemon logs + the `.stats` census), `*-mdstorm.{txt,row,pre.json,post.json,daemon.log,thermal*}`, `prep-*.fio.json` |
| `squeeze-test:/scratch/tmp/sym-pr1/rows-20260913-173118/S1*`, `format-S1.log`, `…-S.log` | **the stamped SCOPING leg** (same directory, arm letter S) |
| `squeeze-test:/scratch/tmp/sym-pr1/rows-20260913-173118-rerun/` (+ `.log`) | **bracket 2** (the DELTA-row re-run) |
| `squeeze-test:/scratch/tmp/sym-pr1/rows-20260913-172501-aborted-rigbug/` (+ `.log`) | the aborted first launch (§9 item 1) — A1's mdstorm leg + the refused second mount; kept as the record, not counted |
| `squeeze-test:/scratch/tmp/sym-pr1/{CURRENT_OUT,RERUN_OUT}` | two one-line pointer files the run used |
| `squeeze-test:/scratch/tmp/logs/` | created by the reset script (`mkdir -p "$LOG_DIR"`); empty — its own mount step only echoes |
| `squeeze-test:/dev/shm/sqz_mdstorm/` | the mdstorm substrate — **removed** after every leg and at rig exit (tmpfs; nothing left) |
| `squeeze-test:/scratch/tmp/test` | the mountpoint — left **unmounted**, no daemon running (verified at the end of §12) |
| the 5 storage nodes | **nothing placed**: `/scratch/tmp/squeezefs` was PRESENT on all five (`squeezefs 1.1.0 (19888503)`, 2026-08-31 — the binary every reset since Sep 1 has used for the nvmet `share`/`unshare` verbs); the reset rebuilt their null_blk backings and nvmet shares per arm as it always does, leaving the fabric in the converged shape it found (5 × {m0,d0,d1} shared, client connected on both paths) |

Pre-existing and untouched: `/scratch/tmp/sym-pr1/{squeezefs-A,squeezefs-B,libsqueezefs_il-{A,B}.so,SHA256SUMS*}` (the orchestrator's), `/scratch/tmp/cluster_reset_v4.sh`, `/scratch/tmp/fio_jobs/`, `/scratch/tmp/exa_client_perf_scripts-1.2.1*`.

## 12. Run log (UTC, 2026-09-13)

| time | event |
|---|---|
| 17:13 | orchestrator's box state verified; binaries A/B + shims at `/scratch/tmp/sym-pr1/`, sha256 verified |
| 17:15–17:24 | box inspected: reset script read (`SQZ=/scratch/tmp/squeezefs` missing on the client — `squeezefs-B` copied there; the five storage nodes' `/scratch/tmp/squeezefs` present, `1.1.0 (19888503)`, untouched); fabric connected (30 nvme controllers, 15 subsystems × 2 paths); rig + mdstorm instrument copied; `bash -n` both; mdstorm smoked at 5 % scale with A (`/dev/shm/sqz_mdstorm_smoke`, removed) |
| 17:25:01 | **launch 1** (`rows-20260913-172501`): reset A1 OK (20 s), mdstorm A1 OK, first mount 0.39 s — then the rig's `pgrep -x squeezefs` missed the `squeezefs-A` comm and issued a second mount, refused by the D0 guard (`EBUSY`, §9.1). Daemon killed, run archived as `-aborted-rigbug`, rig fixed (argv match), re-synced |
| 17:31:18 | **launch 2 = bracket 1** (`rows-20260913-173118`), `SEQ="A B B A"`, all rows: A1 17:31:20 → B2 17:35:50 → B3 17:42:59 → A4 17:50:05 → done **17:57:14**, `gate_failed=0` |
| 17:58 | bracket 1 reduced: 3 fio rows + 6 mdstorm phases + fresh mount + umount within noise; `remount` (0.860) and `mdstorm mkdir` (1.033) DELTA — both in B's favour |
| 18:00:04 | **bracket 2** (the DELTA-row re-run, `rows-20260913-173118-rerun`), `ROWS="mdstorm wfresh-kern remount"`, `SEQ="A B B A"`: A1 18:00:06 → B2 18:02:41 → B3 18:07:33 → A4 18:12:16 → done **18:17:09**, `gate_failed=0`; both DELTA rows within noise |
| 18:17:09 | **S leg** (`SEQ=S`, same dir): reset, `format --force` under `SQUEEZEFS_TEST_STAMP_SYMMETRIC=1`, sector 0 `0x2ffd7` on all five meta volumes; mdstorm S1 (63 slot trees minted), mount 0.44 s, wfresh / prep / rr4k / rw4k, umount 8.2 s, remount 0.80 s → done **18:24:10**, `gate_failed=0` |
| 18:25 | box verified: no `squeezefs-*` process, `/scratch/tmp/test` unmounted, `/dev/shm` empty; fabric left connected in the reset-v5 shape; artifacts reduced on the box (Python 3.6) |

Wall: 17:25 → 18:24 UTC of box time for one aborted launch, two A-B-B-A
brackets and the stamped leg (≈ 1 h; the mdstorm quiet-gate waits are
≈ 2–3 min of each arm's ≈ 5–7 min).
