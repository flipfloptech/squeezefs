# 2026-08-05 — il durable-write decomposition: the fleet-launch session-establishment convoy, named

**Branch:** `perf/il-durable-write` (worktree off `integrate/zcrx-wave`
`f6d0ddd7`). **Parent evidence:** `.benchmarks/2026-08-05-fleet-parity-writes.md`
(+ addendum — the stable field verdict this note decomposes: durable
il/kernel ≈ **0.70–0.75** at 256-process psync qd1 bs=1M, runs 10–12,
both THP modes; durable reads il **+14..+25 %** at the same width).
**Venue (stated per the two-substrate rule):** nvmet-tcp devsub,
localhost, 25-CPU box, 4×8 GiB zram oss + 4×1 GiB null_blk mds.
**Instrument (stated):** fio 3.42 psync process-fleet qd1 bs=1M
`--end_fsync=1` `--zero_buffers`, w256 (the local knee), per-job
wall-clock rate (`user_bytes / max per-job job_runtime` — the three
fio-JSON traps from `tests/fio/fleet_parity_row.sh` honored), K-I/I-K
alternation, engagement exact (`ipc_ops_write` ≡ fio ops on every cited
il row; two earlier partial-engagement runs — KD-7 dirty-stamp
self-refusal, then arena-budget admission refusals at the derived
120 MiB arena — are named and NOT cited). `SQZ_FW_DURABLE=1` landed in
`tests/fleet_width_bracket.sh`; `tests/phase_delta.py` is the
phase-histogram delta analyzer.

## 1. The custody-timeline map (both arms, this shape)

Both arms converge on the SAME `fs.write` handler and the SAME fsync:

* **kernel:** write(2) → page cache → synchronous FUSE_WRITE
  (KD-11: `-o interception` forces `write_back = false` for the WHOLE
  mount, so the "kernel batches in page cache" half of the suspect-1
  hypothesis cannot apply — both arms are write-through) → over-uring
  delivery, payload lease → handler.
* **il:** write(2) interposed → ring op → §5.5.2 sever at dequeue
  (placed-sever adopts whole-block chunks: `placed_severs` =
  `placed_adoptions` = `merge_elides` = 4096/row, exact) → deferred
  handoff to the fuse3 tpc lanes → the SAME handler; ACK after handler
  return; per-growing-write W1 attrs-only inval (sync enqueue since
  `04e044bd`).
* **Common tail:** `record_write` coverage union → complete block →
  `write_pipeline.admit` (pre-ACK backpressure) → detached
  `tpc_spawn_guarded` upload (crypto → allocate → DMA → publish →
  displaced-free) with custody PARKED until durable publish.
* **fsync (identical machinery both arms — the shim does NOT interpose
  fsync; it rides kernel FUSE_FSYNC):** latched-error check →
  `flush_inode_to_backend` = FsyncDurable memory-buffer flush
  (complete → one durable upload; partial → seed + durable escalation)
  → staged-block flush → staging `sync_key` → **DUR-2 data barrier**
  (`flush_data_devices`, coalesced: 1024 requests → ~600–730 syncs/row)
  → `close_rewrite_epoch` → layout persist → meta barrier.

Per-row ledgers are IDENTICAL across arms at this shape (16 GiB row:
`write_through_blocks` 3840, `durable_upload_bytes_escalation` 1 GiB,
`flush_seed_read_bytes` 256 MiB, `nt_copy_bytes` 16.9 GB, journal
entries ~5.0–5.2 k, data write-amp **1.060–1.067 both arms**, data
reads 0) — the durable ladder does the same work for il-written and
kernel-written data. KD-11 is NOT a per-arm differential here (suspect
1 falsified for this shape); the fsync path is NOT arm-split (suspect 2
falsified — same handler, same barriers, coalescer engaged both arms).

## 2. Local repro + the phase evidence (suspect 3)

Durable w256 rows, engagement exact (medians of 3, binary `4619982e`):
il **7,667/7,761/7,758** vs kern **8,163/8,233/8,313** MiB/s →
**il/kernel 0.94** (an earlier 8 GiB-row run at ~82–91 % engagement
read 0.884). Same DIRECTION as the field, smaller magnitude — this box
is ceiling-bound at ~8.5 GB/s and the rows are 2 s.

The ALWAYS-ON histograms (per-arm deltas, same row):

* `write_pipeline_phase_ns`: dma mean 13.1 ms (kern) vs 14.1 ms (il);
  admit_wait 82 vs 62 ms; publish 1.5 vs 2.3 ms — per-block pipeline ≈
  par (il slightly better upstream, slightly worse publish; totals
  favor il: 98.3 vs 79.9 ms residence).
* `fuse_op_phase_ns` (OP_PROFILE): **write.total mean 20.0 ms (kern) vs
  14.6 ms (il)** — the il in-handler path is FASTER per op;
  **fsync.total 45.0 ms (kern) vs 45.6 ms (il)** — the fsync stall is
  EQUAL. The gap is NOT in the daemon's op or fsync machinery.
* fio decomposition: end_fsync stall ≈ 0 both arms locally; the entire
  delta lives in the WRITE PHASE, and per-op clat means are EQUAL
  (26.5 vs 26.3 ms). The delta is *unaccounted per-job time*: med 59 ms
  (kern) vs 192 ms (il) per job.

## 3. The NAMED term: fleet-launch session establishment

Per-job first-op decomposition (`--write_lat_log`, ts − lat = issue):

| run | first write ISSUED after job start (med/p90/max) | first-op latency med |
|---|---|---|
| kern | **7–13 ms** / 21–37 / 55–118 | 76–115 ms |
| il | **140–189 ms** / 204–338 / 219–393 | 24–37 ms |

Every il fleet process pays ~150–190 ms BEFORE its first write issues —
the shim session establishment under the launch storm — and the il
first WRITE is then 3–4× faster than the kernel arm's. strace on a
probe client injected into the storm attributes the client-side wait:
**connect() 85 ms + HELLO→SessionOk recvmsg 82 ms** (one ~51 ms
first-op completion wait follows — cheaper than the kernel arm's
76–115 ms first-op). Discriminators, all counted:

* **Width-scaling:** w32 first-issue med 3 ms (gap gone: il 0.99×);
  w256 med 140–189 ms.
* **Arena-size independence:** 16 MiB vs 64 MiB arenas — identical
  first-issue (falsifies arena/memfd population as the term; consistent
  with the field's THP on/off invariance, since establishment is
  memfd+mmap+admission, not population).
* **Convoy-elimination control (decisive):** the same durable w256 rows
  with `--thread` (one process, 256 threads, ~6 fd-sharded sessions):
  **il 8.86/8.91 vs kern 8.58/8.56 GB/s — il WINS** with the entire il
  data path engaged. The write-phase machinery is at par-or-better; the
  launch convoy IS the local residual.
* Daemon-side share: the new `ipc_session_admission_ns` reads ~3.1 ms
  mean per admission (257/row) — the 140–190 ms client wait is
  CPU-contention scheduling convoy at fleet launch (256 admissions ×
  ctl-thread service racing 256 fio spawns + the row's own I/O), not
  one serialized daemon stage.

**Why the field sees it only on durable rows:** the term is a
per-process constant at row START; the relaxed rows' absorption-race
variance (±6 GB/s, 14–26) swamps it, while fsync pins the tail and
makes the row wall tight — the constant surfaces as a stable ratio. It
also RETRODICTS the field facts the parent note listed: (a) il-only,
(b) THP-invariant, (c) the one 23.2 GB/s il outlier in run 11 (a row
whose convoy landed lightly), (d) prep/inval/R5 falsifications.

## 4. What landed (contained; the parity laws untouched)

* **`ctl_listen_backlog` (derivation law):** the ctl socket listened at
  a hardcoded `64` — exactly ¼ of the field's fleet width, and
  connect(2) on a full SEQPACKET backlog BLOCKS, so the launch burst
  convoyed through 64-slot accept windows. Now derived from the VAL-5d
  ctl connection cap: `clamp(ctl_cap, 64, 4096)` (floor = shipped
  posture; rail = the ctl-thread rail; `somaxconn` stays kernel-owned).
  Local A/B: no regression, no local win (the local convoy is
  scheduling-bound, not backlog-bound — the strace predicted this; the
  fix stands on the law and on removing the kernel-imposed
  serialization stage for hosts where accept keeps up).
* **`ipc_session_admission_ns` (stats inode, ALWAYS-ON):** HELLO
  receipt → SessionOk sent, per ADMITTED session (refusals record
  nothing). One `Instant` + one bucket add per SESSION. This is the
  field adjudication instrument: a fleet row's delta counts the row's
  sessions and splits the launch term into its daemon-served vs
  convoy-scheduling halves without a remount.
* **Rig:** `tests/fleet_width_bracket.sh` gained `SQZ_FW_DURABLE=1`
  (end_fsync, no group_reporting, per-job wall-clock rate, per-row
  diskstats write-amp columns, statvfs-gated capacity settle);
  `tests/phase_delta.py` analyzes any two stats snapshots into
  per-phase bucket-delta tables.
* Pins: `tests/ipc_admission_convoy_tests.rs` (RED `c762f46b` → GREEN
  ×10, `--test-threads=1`): backlog derivation tie test, one-sample-
  per-admission + refusals-record-nothing, and a simultaneous
  establishment burst admitting completely.

## 5. Honest verdict + the requested field rows (orchestrator)

The local term is NAMED and its elimination control is counted; the
field magnitude (0.70–0.75 on 2.7–3.7 s rows ⇒ a ~1 s per-row launch
term at the field's 32-CPU/120 MiB-arena/NUMA shape) is PREDICTED, not
yet field-proven. The launch convoy is a benchmark-row artifact for
long-running fleets (real training jobs establish once) but an honest
cost for process-churning workloads. Field rows to adjudicate, binary
this branch:

1. `tests/fio/fleet_parity_row.sh` durable write ×256, reading
   **`ipc_session_admission_ns`** deltas beside each il row and fio's
   per-job first-issue (`--write_lat_log`, ts−lat med/p90) — the two
   halves of the term, measured live.
2. **The amortization law:** same width, `--size 128m` vs `512m` — a
   launch-constant term moves the ratio toward 1 with size; a per-byte
   term does not. (The parent's runs were all 256m.)
3. **The elimination control:** one `--thread` durable pair — il ≥ kern
   confirms the write path itself is at par in the field too.
4. Per the sustained-state rule: one ≥ 60 s durable-class row
   (`--time_based` overwrite + fsync cadence) — the 2.7 s create rows
   are below the standing rule's horizon and structurally maximize the
   launch term's share.

**Also observed (report-only, filed with the parent note's statfs
item):** after two 16 GiB write+rm cycles the live statfs `avail` stuck
~15.5 GB low with the reclaim queue drained (row 6 of both bracket runs
ENOSPC'd; remount converges) — the parent addendum's
ENOSPC-then-delete drift shape reproduces locally without ENOSPC.
Red-repro candidate remains open.

> **CLOSED (2026-08-05, `fix/wave-closeout`)** — see the parent note's
> closure addendum. Red repro + fix:
> `tests/statfs_live_accounting_tests.rs` (the ENOSPC leg is what parks
> flush units in the allocator valve; the cancel face needs only a
> failing fsync flush fan-out, which is why the local shape fired
> "without ENOSPC" — row 6's ENOSPC had already seeded the unwind).

## SHAs

| commit | what |
|---|---|
| `4619982e` | rig: durable rows + phase-delta analyzer |
| `c762f46b` | RED: the admission-convoy contract |
| `7128aa29` | fix: derived ctl backlog + `ipc_session_admission_ns` |
| `f9d64897` | rig: statvfs-gated capacity settle |
| `3553935c` | refactor: spawn arithmetic dedupe + fmt |
| `8b488d97` | docs: §8 table rows |

Artifacts: `/tmp/fw_dur_z1b`, `/tmp/fw_dur_z2`, `/tmp/fw_dur_B`
(bracket rows + per-row stats snapshots), `/tmp/latlog*` (lat logs,
ramps, probe straces). Verification: convoy/preload-session/
op-economy/inval-venue/derivation-sweep/env-knob suites green
(`--test-threads=1`), clippy both configs `-D warnings` clean, fmt
clean, markdown links PASS, preload gate leg 1 PASSED, new async tests
×10 green.
