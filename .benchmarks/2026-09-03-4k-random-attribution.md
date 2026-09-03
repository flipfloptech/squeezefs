# 2026-09-03 — the 4 KiB-random attribution pass (squeeze-test): where the FS overhead per read goes, by the A2 trace ring

**Program:** [`docs/design-e2e-perf-audit.md`](../docs/design-e2e-perf-audit.md)
§2 (the baseline), §3.3 read #2/#3, Appendix B (the read ledger), §5 order
("attribute the 4k-random gap with the A2 trace ring BEFORE R-2/R-3" — the
pause-state item 5). **Baseline of record:**
[`.benchmarks/2026-09-02-e2e-audit-baseline.md`](2026-09-02-e2e-audit-baseline.md)
(`rr_4k` 441.5 k kern / 873.8 k il at 24×8; addendum: 24.8 µs qd1 RTT,
2.03 M raw at 32×8 — "FS adds ~310 µs (kern) / ~100 µs (il) per op").
**R-1's verdict stands as the input:**
[`.benchmarks/2026-09-02-r1-device-read-executor.md`](2026-09-02-r1-device-read-executor.md)
— the executor mutex class is NOT the gap (≤ 3 % CPU, < 0.2 % of clat).
**Instrument:** the **A2 per-op trace ring** (`SQUEEZEFS_OP_TRACE=1`,
`cat <mnt>/.trace`, [`tests/op_trace_stitch.py`](../tests/op_trace_stitch.py);
module docs [`src/op_trace.rs`](../src/op_trace.rs),
[`crates/fuse3/src/raw/op_trace.rs`](../crates/fuse3/src/raw/op_trace.rs)) —
**every stage number below is a join on ONE op**, never a difference of two
histograms — plus the **A1 exact-sum histograms** (`sum_ns`/`count`, the
whole-row means), the **kernel `fuse:fuse_request_{send,end}` tracepoints
joined by `unique`** (perf, `-k CLOCK_MONOTONIC`, `--ns`; bpftrace is not
installed on the box), the **`nvme:nvme_{setup_cmd,complete_rq}`
tracepoints** for the device term (population-level, joined by
`(ctrl_id, qid, cid)`), and **fio 3.36 JSON** (libaio, `direct=1`, the
field job `/scratch/tmp/fio_jobs/randread_iops.job` verbatim: 24 jobs ×
`size=1g`, 4 KiB randread, iodepth 8, 30 s + 10 s ramp).
**Substrate:** squeeze-test (client 32-core Xeon 6426Y, 251 GB, 2×200 GbE,
kernel **6.19.14-sqz**) → 5 storage nodes over **nvme-tcp**, memory-backed
**nullblk** targets (`cluster_reset_v4.sh` fresh: 5 meta + 10 data
namespaces, cache-less format), FUSE-over-io_uring 32 queues × depth 32,
`fuse3_zc_negotiated = 1`, `fuse3_kmbuf_negotiated = 1`, `data_read_lanes = 4`.
**Binary:** `/scratch/tmp/squeezefs.kvmap` + `libsqueezefs_il.so.kvmap` =
dev `7fe9fde2` (`7fe9fde2ea1c-dirty`, `release` = thin-LTO profile; daemon
and shim one build — KD-7 pairing under `SQUEEZEFS_IPC_ALLOW_DEV=1` on both
ends because the shipped box build carries `-dirty`, `ipc_binds_dev_override
= ipc_binds = 24`, `ipc_bind_refused_version = 0`). **Mount:**
`--daemon --interception --allow-other` + `SQUEEZEFS_OP_TRACE=1` (ring armed:
256 rings × 16384 slots = 96 MiB, sampling **1 in 10** ops). **File set:**
24 × 8 GiB laid out fresh by `write_BW.job` at **32.4 GiB/s** (f46 fixed —
no crossing collapse; `map_migrate_inos = 24`: every file is a kvmap-tree
ino); the randread rows use each file's first 1 GiB. **Window:**
2026-09-03 00:14–01:05 UTC, box otherwise idle. **Tier: measured-real.**
**Row class: 30 s + 10 s ramp — NOT sustained.** Nothing here is a
sustained claim; the R-1 field 60 s rows (flat −4.3 % / −1.5 %) are the
standing evidence that this shape does not decay, and the sustained re-run
of the table stays owed (baseline item 2). **Amplification columns: N/A
(reads).** Artifacts: `~/sqz-field-artifacts/2026-09-03/4k-attribution-artifacts.tgz`
(pre/post `.stats` per row, every `.trace` drain, fio JSON, the tracepoint
CSVs, the stitch outputs, the row and analysis scripts); the box's
`/scratch/tmp/sqz-agent/` was removed at the end.

**Verdict up front.** At 24×8 the FS's 4 KiB random read is
**software-bound in both modes; the device is 9 % (kern) / 24 % (il) of
the per-op latency.** Kern (438.8 µs clat, 434 k IOPS = 22.9 % of the
shape-matched raw 1.898 M): the **zc device-fetch bridge** (`keys_resolved →
block_fetched`, 166 µs, of which only **40 µs is the device**) and the
**transport ingress** (`queue_wait` 69 + `dispatch_lag` 89 = 158 µs) are
the two ~36 % terms; the **kernel-side residue** measured by the join is
**≈ 78 µs** (send → daemon reap ≈ 56 µs mean at a **p50 of 8 µs** — a
stage NOT on the read board that also **carries the whole tail**: it is the
dominant stage of 60 % of every op slower than 3 ms, p99.9 = 6–13 ms). Il
(217.8 µs, 858–878 k = 45–46 % of raw): **ring ingress** (client publish →
svc dequeue, 86 µs = 40 %) and **`device_cq`** (96 µs = 45 %, of which the
device is 51 µs) are the two terms; the direct-drive family contains the
trace to **1.00–1.02** on all six phases. **Containment PASSED** in both
modes (kern 0.94–0.98 on the transport family, the 4–6 % low bias
attributed below). **The zc bridge's software half and the kernel reap
residue are not on the read board by name — the campaign order for
R-2/R-3 is re-stated in §7 with expected µs per op.** Two
instrument bugs found and worked around (§8): the `.trace` drain-on-LOOKUP
loses its payload to the kernel's dentry revalidation, and the
dispatcher's ring fills in ≈ 0.2 s.

---

## 1. The rows and the same-day controls (item 2 — the cross-check)

fio JSON, 24 jobs × qd 8, `direct=1`, libaio, 30 s + 10 s ramp (`clean` =
no drains/no perf; `traced` = the mid-row 0.5 s drains + the 3 s
tracepoint window; `single` = one discard + one capture drain; `nvme` = a
2 s `nvme:*` perf window at t = 20 s). Every row's engagement is exact:
kern `fuse3_zc_replies` ≡ `fuse3_read_inplace_replies` ≡ ops (row + ramp:
17.32 M vs fio 13.02 M + 10 s × 434 k), `ranged_reads` = 24 (one per file
— the kvmap window loads), `read_zc_serve_bytes` = every byte; il
`ipc_ops_read` 34.49 M vs 25.75 M + 8.58 M ramp, direct-drive
`ranged_reads` = 87.3 % of ops, `ipc_fast_path_serves` 12.7 % (the RAM
hold, §6). Tripwires 0 throughout (`invariant_tripwires`,
`transport_lease_overlong`, `fuse_op_watchdog_overdue`,
`meta_kv_block_refs_drift`, both `*_fence_drops`, `transport_cq_overflows`,
`read_dest_overruns`, `ipc_direct_reap_stalls`).

| Row | IOPS | clat mean / p50 / p90 / p99 / p99.9 µs | daemon CPU µs/op (class split) | vs baseline |
|---|---|---|---|---|
| **kern clean** | **433.8 k** | **438.8** / 244.7 / 799 / 3,359 / 13,042 | **34.9** (fuse3-tpc 53 %, fuse3-ur 46 %, sqz-timer 1.1 %) | 441.5 k / 0.43 ms → **−1.7 %** |
| kern traced | 430.1 k | 442.7 / 240.6 / 782 / 3,523 / 15,139 | 34.8 | the instrument's cost: −0.9 % |
| kern single / nvme | 426.8 k / 427.7 k | 446.1 / 238.6 / — / 3,850 / 13,959 · 445.3 / 240.6 / — / 3,752 / 14,484 | 35.8 / 35.7 | |
| **il clean** | **858.4 k** | **222.8** / 179.2 / 375 / 733 / 2,089 | **18.2** (sqz-ipc-svc 50 %, sqz-ipc-dd 29 %, fuse3-tpc 13 %, sqz-blk 6 %) | 873.8 k / 0.22 ms → **−1.8 %** |
| il traced | 877.6 k | 217.8 / 189.4 / 334 / 553 / 2,343 | 18.0 | +0.4 % |
| il single / nvme | 879.0 k / 877.5 k | 217.5 / 187.4 / — / 561 / 2,277 · 217.9 / 187.4 / — / 569 / 2,277 | 17.9 / 18.0 | |

Row-to-row spread is ≤ 2.3 % in both modes; **there is no same-day drift
against the baseline** (−1.7 % / −1.8 % on the clean rows).

**Raw fabric controls** (fio libaio direct on the ten data namespaces,
`/dev/nvme{10..28 even}n1` colon-joined, FS mounted but idle):

| Control | Shape | IOPS | clat mean / p50 / p99 / p99.9 µs | Reading |
|---|---|---|---|---|
| **shape-matched floor** | **24×8** (the FS job's shape) | **1,897.7 k** | **94.1** / 69.1 / 465 / 815 | the denominator for every % below |
| baseline's shape | 32×8 | 2,160.9 k | 109.6 / 79.4 / 537 / 3,719 | baseline 2.03 M → +6 % (raw drift, one day) |
| fabric RTT | 1×1 | 36.0 k | **24.6** / 24.4 / 31.9 / 89.6 | baseline 24.8 µs ✓ |

Re-derived distances at the SAME shape: **kern 433.8 k = 22.9 % of
1.898 M; il 858.4 k = 45.2 % (877.6 k traced = 46.2 %)**. The subtraction
the baseline addendum used gives "FS adds" **344.7 µs (kern) / 128.7 µs
(il)** per op at matched depth — but that subtraction compares against a
device carrying 192 in flight at 1.9 M IOPS (94 µs). The **device term
INSIDE the FS path is smaller** (§3: 40 µs / 51 µs — the FS pushes the
device at 22–46 % of its rate), so the software actually costs **≈ 400 µs
(kern) / ≈ 160 µs (il direct-drive)** per op. Both figures are stated
because the second is what the levers can remove.

## 2. How the rows were traced (and what the ring's own limits did to them)

Per mode: `pre_row .stats` → fio → at t = 15 s `pre_win .stats` + one
DISCARD drain (rings emptied) → ten capture drains 0.5 s apart
(`trace.<mode>.N.json`, merged by op id — a chain split across two drains
re-joins because the stitch sorts each op's stamps by `mono_ns`) → at
t = 20 s `post_win .stats` → fio end → `post_row .stats`. Kern: `perf record
-e fuse:fuse_request_send -e fuse:fuse_request_end -a -k CLOCK_MONOTONIC
-m 4096 -- sleep 3` inside the window (2.49 M events, **0 lost**),
`perf script --ns` → `send|end,unique,ns` CSV. The device term: a separate
row per mode with `perf record -e nvme:nvme_setup_cmd -e
nvme:nvme_complete_rq` for 2 s at t = 20 s (1.69 M / 2.89 M events, 0 lost).

Three facts about the ring shaped the analysis and are recorded in §8:
**(a)** a `.trace` read after the first returns almost nothing unless the
dentry is dropped first (`echo 2 > /proc/sys/vm/drop_caches` precedes every
drain here); **(b)** the dispatcher thread's ring (2 stamps/op on ONE
thread) fills in ≈ 0.2 s, so per 0.5 s drain only the first ≈ 40 % of
chains carry `transport_recv`/`dispatch` — the complete-chain population
is time-sliced, not speed-biased, except at interval edges; **(c)** each
drain (a 6–12 MB JSON built on a handler lane + `drop_caches`) perturbs
the kern transport for ≈ 250 ms — `send → transport_recv` runs 2–3× its
undisturbed mean in those windows, so **the kernel-residue numbers below
are taken from the undisturbed sub-population** (ops whose `send` falls
outside `[drain − 50 ms, drain + 250 ms]`, n = 51.7 k), and the
daemon-stage means are taken from the **whole-row exact histograms** of the
clean row (4 kern rows agree to ≤ 1 %). The il chains are unaffected
(all-vs-undisturbed identical to 0.3 µs); the il numbers are the trace's.

## 3. The device term (nvme tracepoints — the physical floor at the row's own load)

`nvme_setup_cmd → nvme_complete_rq`, opcode 2 (READ), client-side, per
command (the population IS the row's device reads — nothing else ran):

| Mode | cmds/s in window | n | p50 | p90 | p99 | p99.9 | **mean** | in flight at the device (Little) |
|---|---|---|---|---|---|---|---|---|
| kern | 420.7 k (≡ IOPS) | 842,810 | 28.0 | 66.5 | 155 | 300 | **40.0 µs** | **≈ 17 of 192** |
| il | 722 k (= the 87 % direct-drive share of 878 k, window-edged) | 1,446,981 | 31.2 | 98.4 | 226 | 1,122 | **50.8 µs** | ≈ 37 of 192 |
| (raw 24×8, fio clat) | 1,898 k | — | 69.1 | 173 | 465 | 815 | 94.1 | 192 |
| (raw 1×1, fio clat) | 36 k | — | 24.4 | 27.3 | 31.9 | 89.6 | 24.6 | 1 |

**Only ≈ 17 of the kern row's 192 in-flight ops are at the device at any
instant** (≈ 37 in il); the other 175 (155) are in software. The device at
the FS's rate sits 1.6–2× above its unloaded RTT — the floor for this row
is 40–51 µs, not the 94 µs of a saturated raw control.

## 4. Kern attribution (item 1 — the per-stage table)

Population: the `traced` row's 0.5 s-drain capture (199,412 traced ops of
2.10 M in the window; 119,651 with a complete daemon chain AND a kernel
join). Percentiles from the trace; means as stated per column. `share` is
of the row's fio clat (438.8 µs). Stage names are the A2 `Stage` vocabulary
(each stage = the END of the phase beside it).

| # | Stage (kern, 4 KiB randread 24×8) | p50 | p99 | p99.9 | **mean** | share | Term / board item |
|---|---|---|---|---|---|---|---|
| K1 | **`send → transport_recv`** (kernel `fuse_request_send` → the fuse3-ur worker reaps the ring CQE) | **8.5** | 1,097 | 6,453 | **≈ 56** (undisturbed; 96 in the drain-perturbed window; 53 in the fast-biased single-drain sample) | **12.8 %** | **NOT on the board.** The queue worker's `io_uring_enter` cadence: `SINGLE_ISSUER + DEFER_TASKRUN` rings surface the kernel's request-delivery task-work only at the worker's next enter, and the worker's loop is shared with zc-fetch submission/reaping + commits. **The tail lives here** (§5). |
| K2 | `transport_recv → dispatch` (`queue_wait`: inbound push → session dispatch pop) | 28.4 | 379 | 617 | **69.3** (exact, clean row) | 15.8 % | **read #2 transport ingress → R-2** |
| K3 | `dispatch → handler_entry` (`dispatch_lag`: dispatch → the handler future's first poll on a fuse3-tpc lane) | 47.9 | 474 | 663 | **89.2** (exact) | 20.3 % | **read #2 → R-2** (+ the lane run-queue / CPU-scheduling half — R-5's CPU term) |
| K4 | `handler_entry → keys_resolved` (prelude + meta_resolve + key_resolve — the serve prelude) | 2.8 | ~18 | ~160 | **3.6** (exact: 1.2 + 2.1 + 0.3) | 0.8 % | read #6 → R-5 (allocs/atomics; kvmap head resolve is 2.1 µs) |
| K5 | **`keys_resolved → block_fetched`** (`block_fetch`: the zc direct leg — tier probe, `zc_device_fetch` message to the queue worker, batched SQE, device, CQE reaped by the worker, oneshot → handler lane wake) | **108.8** | 623 | 4,597 | **165.7** (exact) | **37.8 %** | = **device 40.0** (9.1 %, §3) **+ zc-bridge software ≈ 126 µs (28.7 %)** — **read #3 fill-issue economy in its zc-leg form → R-3**; the board's `dev_queue`/`wake` split does not exist on this leg (§8 gap 2) |
| K6 | `block_fetched → reply_commit` (post_validate + read_return + handler glue between phase boundaries + the in-place COMMIT enqueue) | 2.8 | ~16 | ~44 | **≈ 8.5** (exact by difference inside `transport_total`; the trace's consecutive stamps carry 3.6 of it) | 1.9 % | glue |
| K7 | **`reply_commit → end`** (COMMIT_AND_FETCH SQE → kernel `fuse_request_end`) | **10.2** | 123 | 537 | **≈ 22** (undisturbed) | 5.0 % | kernel-side post residue (the commit's ring flush + the kernel copy-from-ring on the kmbuf arm) |
| K8 | unattributed by subtraction (fio clat − `transport_total` − K1 − K7: VFS + io_uring submit + fuse pre-`send` request formation + post-`end` wake) | — | — | — | ≈ 25 | 5.6 % | labeled subtraction |
| | **`transport_recv → reply_commit` (daemon-visible)** | 211.7 | 1,415 | 4,813 | **336.3** (exact `transport_total`) | 76.6 % | |
| | **`send → end` (kernel-visible total, joined)** | 245.4 | 2,658 | 8,317 | 381 (undisturbed subset) / 438 (full window) | | fio clat 438.8 |

Reading the columns: **Σ stage p50s = 209 µs vs a p50 total of 245 µs**,
and every stage's mean is 1.5–7× its p50 — **the per-op latency is
tail-driven at every stage**, not a fixed cost. The undisturbed kernel
join sums to 336.3 + 56 + 22 = 414 µs against 438.8 µs of clat (94 %
attributed by measurement; K8 is the 6 % residue).

**Where the 192 in-flight ops sit (Little's law, kern):** transport
ingress (K2 + K3) **≈ 69**, zc-bridge software **≈ 55**, kernel pre-daemon
reap **≈ 24**, device **≈ 17**, unattributed ≈ 11, kernel post ≈ 10,
prelude/glue ≈ 5.

## 5. The kern tail — one stage owns it

Ops with a kernel-visible total > 3 ms (1.1 % of ops; fio p99 = 3.4 ms):

| Stage | p50 | mean | share of the slow op | dominant stage of… |
|---|---|---|---|---|
| **`send → transport_recv`** | **2,831** | **5,688** | **52 %** | **832 of 1,370 slow ops (61 %)** |
| `transport_recv → dispatch` | 111 | 2,177 | 20 % | 176 |
| `keys_resolved → block_fetched` | 318 | 1,799 | 17 % | 264 |
| `dispatch → handler_entry` | 188 | 776 | 7 % | 28 |
| `reply_commit → end` | 21 | 283 | 3 % | 59 |

In the FAST half (ops ≤ the p50 total, mean 142 µs) the same stage is
10 µs. **The kern tail (p99 3.4 ms, p99.9 13 ms) is a queue-worker reap
stall of 3–10 ms, not a device or a lock term** — a stall in which the
kernel has queued the request, the ring CQE is posted, and the fuse3-ur
thread does not enter the kernel to surface it. The device's p99.9 is
0.3 ms; the 3.5 stripe lock (`read_routed → stripe_lock_acquired`, 24
kern ops at 9.5 ms mean — the per-file kvmap window loads) is 0.01 % of
ops and 0.1 µs of the mean. The cause of the reap stall is **not
attributed here** (candidates: CPU oversubscription — 32 fuse3-ur + 32
fuse3-tpc + 24 fio + 12 svc threads on 32 cores with the daemon at
≈ 15 cores; a loop iteration stuck behind a batch of zc fetches/commits;
the ticked `InboundQueue::pop` park R-1 named) — a per-worker enter-gap
histogram is the instrument that would (§8).

## 6. Il attribution

Population: 426,968 traced il ops; **375,721 direct-drive ops with the
complete `ipc_ingress → … → ipc_complete` chain (87.3 %)** and 50,937 ops
(12.7 %) that stamp only `ipc_ingress`/`ipc_dequeue` — the §5.5.1 sync
fast path serving from RAM (`ipc_hold_probe_serves` 4.21 M ≈
`ipc_fast_path_serves` 4.44 M over the row: the read-lane HOLD; there is
no completion stamp on that path — §8 gap 3). Percentiles and means from
the trace (drain-insensitive here); the whole-row exact histogram means
agree to ≤ 1 % (`ipc_ingress_ns` 85.7, `ipc_direct_phase_ns` admit 9.2 /
sq_wait 18.3 / device_cq 94.3 / finish 3.9 / total 125.7). `share` is of
the direct-drive op's daemon-visible mean (215.2 µs).

| # | Stage (il direct-drive, 4 KiB randread 24×8) | p50 | p90 | p99 | p99.9 | **mean** | share | Term / board item |
|---|---|---|---|---|---|---|---|---|
| I1 | **`ipc_ingress → ipc_dequeue`** (client publish stamp → the svc thread's dequeue: the RING INGRESS) | **71.3** | 172 | 316 | 798 | **86.8** | **40.3 %** | `ipc_ingress_ns` — the shim drain-funnel line ([r3](2026-08-08-shim-drain-funnel-r3.md): lane-scoped flush cut it 67–75 %; what remains is the svc pass period at 12 lanes ≈ 66 % busy). **Not on the read fat board** (il-specific). |
| I2 | `ipc_dequeue → ipc_admitted` (`admit`: direct-drive slab insert) | 6.2 | 16.7 | 47 | 116 | 9.4 | 4.4 % | shim-iops cost ledger ([r1](2026-08-07-shim-iops.md)) |
| I3 | `ipc_admitted → ipc_sq_enter` (`sq_wait`: SQE queued until the lane's carrying `io_uring_enter`) | 13.0 | 41 | 99 | 163 | 18.9 | 8.8 % | the flush-batch cadence (`SQUEEZEFS_IPC_DD_EAGER_FLUSH` is the registered lever, +2.3 % counted) |
| I4 | **`ipc_sq_enter → ipc_cqe`** (`device_cq`: enter → CQE popped) | **72.5** | 162 | 318 | 1,508 | **96.1** | **44.6 %** | = **device 50.8** (23.6 %, §3) **+ reap software ≈ 45 µs (21 %)** — the direct-drive reaper's cadence (inline reaps engaged on 8 % of ops in the window, the bounded-wait reaper on the rest) — **read #3's il face** |
| I5 | `ipc_cqe → ipc_complete` (`finish`: completion posted + doorbell) | 2.2 | 7.7 | 33 | 95 | 4.0 | 1.9 % | |
| | **`ipc_ingress → ipc_complete` (daemon-visible)** | 188.8 | 332 | 567 | 2,157 | **215.2** | | Σ stage p50s = 165 |
| I6 | client residue by subtraction: fio clat 217.8 − (0.873 × 211.4 + 0.127 × (85.7 + fast-path serve)) | — | — | — | — | **≈ 22** | ≈ 10 % of clat | claim/publish before the ingress stamp + `ipc_complete` → doorbell wake → reap → `io_getevents`; **the A2 item-7 client completion stamp is not landed** — labeled subtraction |

Il tail (daemon-visible > 1 ms, 0.3 % of ops): `device_cq` dominates (65 %,
p50 1.0 ms — the device's own p99.9 is 1.1 ms at this load, so the il tail
is largely the device's), ingress 31 %. The 84 il ops that fell to the
handler path (`ipc_async_handoffs` 0.02 %) sat 20–60 ms under the 3.5
stripe lock (`read_routed → stripe_lock_acquired` p50 20.6 ms) — the kvmap
window fill; 3 µs of the mean, but the p99.99 (50 ms) is theirs.

## 7. Top-3 per mode, the board mapping, and the recommended R-2/R-3 order (item 3 + item 4)

### 7.1 Top-3 by µs and share

| Mode | Rank | Stage | mean µs | share of clat | Board item | Campaign |
|---|---|---|---|---|---|---|
| **kern** | 1 | `keys_resolved → block_fetched` — the zc fetch bridge (device 40 + software ≈ 126) | **165.7** | 37.8 % | read #3 (fill-issue economy) — zc-leg form; **the software half is NOT named on the board** | **R-3** `perf/fill-poll-cohort` |
| | 2 | `queue_wait` + `dispatch_lag` — transport ingress | **158.5** | 36.1 % | read #2 | **R-2** `perf/read-fast-dispatch` |
| | 3 | `send → transport_recv` + `reply_commit → end` — the kernel-side residues (the queue worker's enter cadence; **the whole tail**) | **≈ 78** | 17.8 % | **NOT on the board** (the ledger had it only as a subtraction, ≈ 220 µs — R-1 §4.3; measured here at 78) | joins R-2/R-3 (same thread), plus a new instrument (§8) |
| **il** | 1 | `device_cq` (device 50.8 + reap software ≈ 45) | **96.1** | 44 % | read #3's il face (the direct-drive reaper) | the shim reap line (r1–r3), R-3's il rows |
| | 2 | `ipc_ingress → ipc_dequeue` — ring ingress | **86.8** | 40 % | not on the read board (il-specific; the drain-funnel campaign's term) | a drain-funnel r4 |
| | 3 | `sq_wait` | **18.9** | 9 % | the lane flush cadence | `SQUEEZEFS_IPC_DD_EAGER_FLUSH` re-grade |

**Said loudly:** in kern, **two of the three biggest terms are not on the
read board by name** — the zc bridge's software half (≈ 126 µs: the board's
#3 names `dev_queue`/`wake` on the `NvmeBlockDev` funnel, a leg the kern
rand-4k row never takes on the sqz kernel) and the queue worker's reap
cadence (≈ 56 µs mean, p50 8, the p99.9 = 13 ms tail). Both are the SAME
thread — the per-queue fuse3-ur worker, which reaps the request CQE (K1),
pushes to the inbound queue (K2's start), receives the handler's
`zc_device_fetch` message, submits it at its loop bottom, reaps the device
CQE and completes the handler's oneshot (K5's software), and carries the
COMMIT_AND_FETCH (K7). **Read #6 (the serve prelude: allocs/atomics) is
≈ 4 µs of wall time — 1 % — and is a CPU term, not a latency term**; R-1's
executor class is confirmed absent from the timeline (the `sqz-timer`
class is 1.1 % of CPU; no stage carries it).

### 7.2 Recommended order and expected µs per op (kern)

The row is closed-loop at 192 in flight, so every µs removed is IOPS
gained: IOPS = 192 / clat.

| Order | Campaign | Attacks | Mechanism to land | Expected Δ clat | Acceptance row |
|---|---|---|---|---|---|
| **1** | **R-2 `perf/read-fast-dispatch`** | K2 + K3 (158 µs) and part of K1 | READ handlers dispatched from the reaping fuse3-ur worker straight onto a lane (or the ≈ 4 µs prelude run inline on the worker), bypassing the shared inbound queue + session dispatcher; the lane hop stays (dispatch_lag's p50 half) | **−80 … −130 µs** (queue_wait gone, dispatch_lag halved) ⇒ 439 → ≈ 310–360 µs ⇒ **≈ 530–620 k IOPS** | `rr_4k` 24×8 kern A-B-B-A, loop + tcp devsub then field; `queue_wait`+`dispatch_lag` exact sums; the K1 join p50 must not rise; `fuse3_read_inplace_replies` ≡ ops |
| **2** | **R-3 `perf/fill-poll-cohort`** (zc-leg form) | K5's software half (≈ 126 µs) and K1 | The zc READ issued and reaped without two worker hops: a per-lane (or per-worker-owned) submission ring the handler lane drives itself, completions waking the handler directly (no oneshot through the worker); or the fused variant — the worker runs prelude + fetch inline and commits from the CQE — as the A/B alternative. The `dev_submit`/`dev_complete` stamps land on this leg FIRST (§8 gap 2) so the bracket reads device vs software per op | **−60 … −100 µs** ⇒ with R-2: ≈ 200–250 µs ⇒ **≈ 750–950 k IOPS** (kern reaching today's il) | same rows; `block_fetch` exact sum vs the nvme device mean (target: `block_fetch − device` ≤ 30 µs); K1's tail (p99.9) must fall with the worker's loop load |
| 3 | the K1 tail instrument (new) | the 3–10 ms reap stalls (61 % of > 3 ms ops) | per-worker enter-gap histogram (`transport_enter_gap_ns`) + `perf sched` on `fuse3-ur*` during the row; then whichever of CPU oversubscription / loop-batch head-of-line / the ticked pop it convicts | p99 3.4 ms → the device's 0.3 ms class if convicted | `rr_4k` p99/p99.9 |
| 4 | R-5 `perf/read-handler-economy` | CPU/op (34.9 µs = 15 cores at 434 k) | the board's #6/#7/#8/#9 batch | CPU, not clat — but the K1/K3 tails are CPU-contention-shaped, so it composes | `daemon_cpu_ns` per op |

### 7.3 Il

| Order | Term | Lever | Expected Δ clat |
|---|---|---|---|
| 1 | I1 ring ingress (87 µs) | the svc pass economy — pass period at 12 lanes × 66 % busy; re-grade lane width now that r3 removed the funnel contention (its width row was a no-op UNDER the contention), doorbell-driven dequeue | −40 … −60 µs |
| 2 | I4's software half (≈ 45 µs) | the direct-drive reap cadence (inline-reap share 8 % → higher; the bounded-wait reaper's quantum) | −20 … −30 µs |
| 3 | I6 client residue (≈ 22 µs, subtraction) | land the A2 item-7 client completion stamp first — no lever before the number | — |
| 4 | I3 sq_wait (19 µs) | `SQUEEZEFS_IPC_DD_EAGER_FLUSH` re-grade at 24×8 | −5 … −10 µs |

## 8. Instrument findings and gaps (report items)

1. **`.trace` drain-on-LOOKUP loses its payload to the kernel's dentry
   revalidation (BUG).** The `.trace` LOOKUP drains the rings into a fresh
   generation ino (`src/fuse_client.rs`, the `.trace` arm beside `.stats`,
   `ttl 0`). With a cached dentry the kernel's `fuse_dentry_revalidate`
   sends a LOOKUP, sees a DIFFERENT nodeid, invalidates, and the path walk
   sends a SECOND LOOKUP — the first drained everything into a payload the
   kernel discards, the second returns whatever was pushed in the
   ≈ 50–100 ms the first spent serializing. Measured: 20,000 direct reads →
   20,014 samples pushed → `cat .trace` returned **0**; with `echo 2 >
   /proc/sys/vm/drop_caches` first → **20,024** (exactly 1-in-10, 2,005
   ops). Every drain in this note is preceded by the dentry drop. Fix
   options (not applied — read-only pass): drain at OPEN/first-READ of the
   minted ino instead of at LOOKUP, or answer `.trace` lookups with a
   stable nodeid so the revalidate passes.
2. **The zc direct leg has no `dev_submit`/`dev_complete` stamps.** The
   `READ_FILL_STAGE` funnel pair lives on the `NvmeBlockDev` worker; kern
   rand-4k rides `zc_device_fetch` on the fuse3 queue worker's ring, so
   `keys_resolved → block_fetched` is one opaque 166 µs. The device/software
   split in §4 is population-level (nvme tracepoints), not a per-op join.
   R-3's first commit should add the pair on the zc bridge (submit at the
   worker's SQE push, complete at its CQE pop).
3. **The il sync fast path stamps only ingress/dequeue** (12.7 % of il ops
   here — the read-lane hold serves); no completion stamp, so its serve
   span is unmeasured (assumed ≈ 0 in I6). **The client completion stamp
   (A2 item 7) is not landed** — the il client residue is a subtraction.
4. **Ring geometry vs the stamping population.** The derivation sizes the
   POOL for one drain interval at the machine's op ceiling assuming an even
   spread over 256 rings; the session dispatcher stamps `transport_recv` +
   `dispatch` for EVERY traced op on ONE thread (86 k samples/s at 43 k
   traced ops/s) and fills its 16,384-slot ring in ≈ 0.19 s, dropping the
   rest (`op_trace_dropped` 85 k over the 6 s kern window). Role-aware ring
   depth (or a larger ring for the dispatcher class) is the fix; until then
   drain at ≥ 5 Hz for complete kern chains.
5. **The drain perturbs what it measures.** A 200 k-sample JSON built on a
   fuse3-tpc lane + a 6–12 MB FUSE READ + `drop_caches` inflates kern
   `send → transport_recv` 2–3× for ≈ 250 ms after each drain (§2); the il
   path is unaffected. A compact/binary export built off-lane would remove
   the term; the note's kernel residues use the undisturbed sub-population.
6. **Stitch tool:** the containment table applies read-family spans to
   every chain regardless of class — the il row's 97 kernel metadata ops
   FAILED `read_transport_phase_ns.transport_total` at 18.9× (7 multi-ms
   `handler_entry → reply_commit` ops that the read histogram never
   counts). `FAMILY_CLASS` exists but the span loop does not filter by it;
   the verdict on the read population is unaffected (kern 0.96 OK).
   `read_transport_phase_ns.reply_commit` is not recorded on the zc
   in-place arm (count Δ 140 of 2.1 M) — the `read_return → reply_commit`
   transition (1.6 µs) is the trace's answer.
7. **Kernel join availability:** bpftrace is absent on squeeze-test; perf
   4.18 (RHEL 8 userspace) on the 6.19 kernel records the fuse and nvme
   tracepoints with `-k CLOCK_MONOTONIC` and prints `--ns`; `perf script`
   says `[FAILED TO PARSE]` on the nvme events but emits every field
   (`ctrl_id= qid= cid= opcode=`), which is all the join needs.

## 9. Containment verdict (the trace vs the A1 exact sums, same window)

| Mode | Phase | trace n | hist Δn | trace mean | hist mean | ratio | verdict |
|---|---|---|---|---|---|---|---|
| kern | `read_transport_phase_ns.queue_wait` | 199,379 | 2,104,534 | 69.7 | 71.4 | 0.98 | OK |
| kern | `read_transport_phase_ns.transport_total` | 199,328 | 2,104,570 | 314.9 | 328.0 | 0.96 | OK |
| kern | `read_transport_phase_ns.dispatch_lag` (~) | 199,363 | 2,104,532 | 77.9 | 82.5 | 0.94 | ~approx (start = nearest earlier stamp) |
| kern | `read_serve_phase_ns.total` (~) | 199,264 | 2,104,017 | 165.4 | 171.6 | 0.96 | ~approx |
| il | `ipc_direct_phase_ns.{admit, sq_wait, device_cq, inflight, finish, total}` | 375.7 k each | 3,887 k each | 9.4 / 18.9 / 96.1 / 115.0 / 4.0 / 128.4 | 9.3 / 18.8 / 95.7 / 114.5 / 4.0 / 127.8 | **1.00–1.02** | OK ×6 |

Kern `n × divisor` = 1.99 M vs Δn 2.10 M (95 %; the dispatcher-ring drops
+ window edges); the 4–6 % low bias on the means is the truncation class:
a chain cut at a drain boundary or a full ring is excluded from the span,
and the slowest ops are the likeliest to straddle. Il is exact. **The
trace is a faithful 1-in-10 sample of the histograms' populations; the
tables above are its per-op decomposition.**

## Landing-law checklist

A-B-B-A — N/A (attribution pass, no lever); substrates — field only (the
attribution's venue; the loop/tcp decomposition rows belong to R-2/R-3's
brackets); sustained — NOT (30 s rows, labeled; R-1's 60 s field rows are
the flatness evidence for this shape); amplification — N/A (reads);
engagement — exact on every row (§1); `read_copy_*` closure — kern: every
byte in `read_zc_serve_bytes` (zero daemon passes; `read_copy_dest_bytes`
0), il (traced window): `read_dest_dma_bytes` 15.92 GB (= `ranged_read_bytes`,
the direct-drive DMA) + `ipc_arena_copy_bytes` 2.17 GB (the hold serves) =
18.10 GB = `ipc_ops_read` 4.418 M × 4 KiB, `read_copy_bounce_bytes` 0;
instrument + substrate + binary + kernel + tier stated; tripwires 0; the
box returned unmounted with `/scratch/tmp/sqz-agent/` removed.
