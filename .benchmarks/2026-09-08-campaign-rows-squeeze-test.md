# 2026-09-08 — campaign board field rows on squeeze-test (W-6, R-5, W-5)

The 2026-09-08 campaign board landed four mechanisms on in-process
evidence (`.benchmarks/2026-09-08-{w6-write-handler-economy,
r5-read-handler-economy,w5-fsync-economy,d5-owner-hop-and-depth}.md`), each
with its FIELD row owed under the e2e perf audit's landing law
(`docs/design-e2e-perf-audit.md` §0). This note is the field row for three
of them — the read/write handler economies and the fsync ladder — measured
as ONE A-B-B-A whose arms are binaries on one boot. D-5's row is a fleet
shape (owner dispatch under co-writers) and stays owed; see §6.

## 1. Venue and instrument

| | |
|---|---|
| box | squeeze-test (`memp-s3ds-aqs-37`), 32-core Xeon, Rocky 8, kernel `6.19.14-sqz` **with patch 0031** (per-queue FUSE bg accounting — the B arm of `.benchmarks/2026-09-06-kernel-bg-per-queue-ab.md`, shipping since 2026-09-06) |
| fabric | the 5-node nvme-tcp set `/dev/nvme{0,2,4,6,8}n1` (meta + data namespaces; real fabric, not devsub) |
| arms | **E** = `96f8d873` (the pre-campaign tip, 1.2.1 + finding-15 residues), **F** = `e0bdd35f` (E + W-6 `c88bdf7f`, R-5 `1ca9ab67`, W-5 `ab669857`, D-5 `fce9aa5f`, the trim-window gauge fix `e0bdd35f`); both `profile release`, same-commit shims (`libsqueezefs_il-{E,F}.so`) |
| order | **E F F E**, one boot, the same formatted set re-prepped per arm (`write_BW` 40 s prep after each mount); mount/reset logs in the artifact dir |
| rows per arm | `rr4k-kern` (randread 4 KiB O_DIRECT, 24 jobs × qd 8, 30 s + 10 s ramp — R-5's row), `rw4k-kern` (randwrite, same shape — W-6's row), `wdur-kern` (`write_BW` 1 MiB × 24 × qd 16, 30 s, `--end_fsync=1` — the W-5 `w_durable` row), `fsync-storm` (24 jobs × 256 KiB buffered writes, `fsync=1` per write, 30 s + 5 s ramp — W-5's own shape), `rr4k-il` (the shim path — takes neither handler; attribution control) |
| snapshots | `.stats` + `/proc/stat` + hwmon around every fio run; every mean below is EXACT (Δsum ÷ Δcount) |
| rig / reducer | `.benchmarks/rigs/2026-09-08-campaign-rows-abba.sh` → `.benchmarks/rigs/2026-09-08-campaign-rows-reduce.py` (the R-4 per-row analyzer only knows read jobs, which is why the rig's own `.row` files are empty for the write rows — the reducer reads the fio JSON + stats pairs directly) |
| artifacts | `squeeze-test:/scratch/tmp/sqz-agent/campaign/rows-efFE/`, log `efFE.log`; started 05:33Z, done 05:51Z; box `hwmon2/temp2` 51 °C flat across the run |

Tripwires (`invariant_tripwires`, `fuse_op_watchdog_overdue`,
`transport_cq_overflows`, `transport_lease_overlong`,
`write_pipeline_fence_drops`, `data_dma_fence_refusals`,
`writeback_errors_latched`, `detached_task_panics`) were 0 on every row of
both arms; dmesg carried nothing after boot.

## 2. R-5 — `rr4k-kern` (the read handler economy)

| arm | pos | IOPS | p50 µs | p99 µs | p99.9 µs | daemon µs/op | `fuse3-ur` µs/op | `fuse3-tpc` µs/op | box busy % |
|---|---|---|---|---|---|---|---|---|---|
| E | 1 | 562,460 | 179.2 | 3,523 | 10,420 | 35.36 | 19.18 | 16.13 | 74.5 |
| E | 4 | 559,826 | 179.2 | 3,391 | 11,338 | 35.78 | 19.32 | 16.40 | 74.8 |
| F | 2 | 600,267 | 173.1 | 3,293 | 9,634 | 32.16 | 18.79 | 13.33 | 74.9 |
| F | 3 | 598,724 | 171.0 | 3,326 | 10,027 | 32.17 | 18.81 | 13.32 | 74.6 |
| **F/E** | Δ median | **+6.8 %** | −4.0 % | −4.3 % | −9.6 % | **−9.6 %** | −2.3 % | **−18 %** | +0.1 % |

The mechanism lands where it was aimed: the handler lanes (`fuse3-tpc`,
where the READ handler + the router's single-block serve run) shed
2.8 µs/op — 18 % — and the reap worker (`fuse3-ur`, R-4's diet, untouched
by R-5) moves 0.4 µs. Both orders agree to 0.3 % on IOPS and 0.01 µs on
CPU/op. The il control row (§5) moves +3.1 % IOPS at −4.9 % daemon CPU/op
with the shim's service-thread classes, which take no READ handler — the
control bounds the row's free-standing term (box/thermal/set state) at a
third of the kern delta, so the kern row's remainder is the handler.

## 3. W-6 — `rw4k-kern` (the write handler economy)

| arm | pos | IOPS | p50 µs | p99 µs | p99.9 µs | daemon µs/op | `fuse3-ur` µs/op | `fuse3-tpc` µs/op | `patch_writes` | `write_through_blocks` | wp total µs | box busy % |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| E | 1 | 480,593 | 284.7 | 1,532 | 3,162 | 46.32 | 38.48 | 7.09 | 16,633,261 | 1,796 | 2,081 | 72.9 |
| E | 4 | 467,156 | 288.8 | 1,663 | 3,129 | 47.18 | 39.53 | 6.90 | 16,225,379 | 1,722 | 2,186 | 72.1 |
| F | 2 | 534,232 | 250.9 | 1,417 | 3,523 | 41.23 | 33.65 | 6.81 | 18,434,305 | 2,294 | 2,059 | 74.8 |
| F | 3 | 538,804 | 248.8 | 1,434 | 3,686 | 40.84 | 33.62 | 6.44 | 18,483,211 | 2,296 | 2,039 | 75.1 |
| **F/E** | Δ median | **+13.2 %** | **−12.9 %** | −10.8 % | +14.6 % | **−12.2 %** | **−15 %** | −6 % | +12.4 % | +30 % | −4.0 % | +3.3 % |

Every write on this shape is the W1 in-place patch (`patch_writes` ≡ fio
ops + ramp on both arms; `write_through_blocks` is the prep's tail), so the
row is the patch handler's per-op cost end to end. It fell 5.7 µs/op, and
5.6 of those are on the `fuse3-ur` class — on this zc-armed session every
WRITE ran on the fused write lane, i.e. ON the queue worker
(`fuse3_zc_write_fusions` ≡ `patch_writes` on both arms, 16.64 M / 18.44 M,
`fuse3_zc_write_fusion_demotions` 0), which is where the 14.6 → 3.06 allocs
and the 17 striped RMWs were paid. IOPS +13.2 % at p50 −12.9 %; the p99.9 tail is +15 % on a
row whose p99.9 is the device's (the nvme-tcp write completion at qd 192),
and the two F rows disagree with each other by 4.6 % on it — it is noise at
this sample, not a shape. Both orders agree to 0.9 % on IOPS.

## 4. W-5 — `fsync-storm` and `wdur-kern` (the fsync ladder)

### 4.1 The fsync storm (24 × 256 KiB buffered writes, fsync per write)

| arm | pos | fsyncs/s | MiB/s | sync mean µs | sync p50 µs | sync p99 µs | daemon µs/op | data sync REQUESTS / fsync | physical data syncs / fsync | meta syncs / fsync | box busy % |
|---|---|---|---|---|---|---|---|---|---|---|---|
| E | 1 | 2,615 | 654 | 8,875 | 8,716 | 10,945 | 1,939 | 10.00 | 1.87 | 0.47 | 31.4 |
| E | 4 | 2,611 | 653 | 8,892 | 8,716 | 10,813 | 1,980 | 10.00 | 1.88 | 0.47 | 31.5 |
| F | 2 | 3,216 | 804 | 7,066 | 6,717 | 11,338 | 1,822 | 1.00 | 1.00 | 0.92 | 34.4 |
| F | 3 | 3,251 | 813 | 6,996 | 6,717 | 11,207 | 1,816 | 1.00 | 1.00 | 0.92 | 34.8 |
| **F/E** | Δ median | **+23.8 %** | +23.8 % | **−20.8 %** | **−22.9 %** | +3.6 % | −7.2 % | **10 → 1** | 1.88 → 1.00 | 0.47 → 0.92 | +3.1 pt |

The touched-namespace lever engages exactly as designed on a real fabric:
the shipped ladder requested a barrier on EVERY data namespace per fsync
(10 requests per fsync = the five-node set's data namespaces on both
mounts of the coalescer's pair), and the field coalescer collapsed those to
1.88 physical `NvmeBlockDev::flush` commands per fsync; the F arm requests
one — the namespace the ino's block landed on — and issues one. The meta
sync count per fsync RISES 0.47 → 0.92 because the F arm's meta barrier
runs last, after the data legs, and no longer shares its coalescer window
with nine data barriers' worth of neighbours (the barrier the DUR-1 order
requires is still exactly one per fsync — `fsync_phase_ns.meta_barrier` is
0.56 ms of the 6.9 ms).

fsyncs/s +23.8 %, fsync latency p50 −22.9 %, daemon CPU per fsync −7.2 %,
both orders within 1.1 %. The in-process rows said +47–58 %; the field is
smaller because the field's ladder has a term the in-process fixture does
not — see the decomposition:

| F arm `fsync_phase_ns` (exact means, n = 113 k) | µs | share |
|---|---|---|
| `data_flush` (writeback of the ino's dirty active block) | 5,503 | 80 % |
| `data_barrier` (the one touched namespace) | 775 | 11 % |
| `meta_barrier` | 565 | 8 % |
| `meta_publish` | 31 | 0.5 % |
| `intent_barrier` / `extent_barrier` | 0 | — |
| `total` | 6,875 | |

80 % of a field fsync on this shape is `data_flush`, and the ledger says
why: every 256 KiB write + fsync ESCALATES the whole 4 MiB active block —
`durable_upload_bytes_escalation` = 4.19 MiB per fsync, `flush_seed_read_bytes`
= **18.4× the user bytes** (both arms identically: 359,970 MiB seed reads
for 19,618 MiB written on E; 445,586 for 24,126 on F), `patch_ineligible_adjacent`
= 1.00 per fsync (the write is stream-adjacent to the active block, so the
W1 patch declines it and it rides the active buffer), `overwrite_seed_materialized`
= 0.98 per fsync. The write pipeline's `dev_service` for that upload is
1.73–1.78 ms per fsync on both arms, so the remaining ≈ 3.7 ms of
`data_flush` is the seed read + the wait for the drain. **This is the
pre-existing fsync-of-a-partial-block amplification that the W-5
instrument now names in the field, not a W-5 regression** (E pays it
identically — its fsync is 8.9 ms because it pays the same flush PLUS nine
extra barrier requests). It is filed as a board item in §6.

### 4.2 `wdur-kern` (`write_BW` 1 MiB × 24 × qd 16 + `--end_fsync=1`)

| arm | pos | MiB/s | p50 µs | p99 µs | daemon µs/op | `write_through_blocks` | fsyncs | data sync req / fsync | physical data syncs / fsync | box busy % |
|---|---|---|---|---|---|---|---|---|---|---|
| E | 1 | 32,954 | 8,290 | 48,497 | 641 | 5,695 | 24 | 10.75 | 4.75 | 75.4 |
| E | 4 | 33,384 | 7,700 | 51,118 | 672 | 6,487 | 24 | 10.71 | 6.04 | 75.2 |
| F | 2 | 33,864 | 7,832 | 47,972 | 616 | 6,841 | 24 | 10.75 | 8.29 | 76.1 |
| F | 3 | 33,206 | 7,635 | 51,642 | 641 | 6,746 | 24 | 10.71 | 5.96 | 74.0 |
| **F/E** | Δ median | +1.1 % | −3.3 % | +0.0 % | −4.2 % | +12 % | = | = | — | −0.3 % |

**PAR by construction.** This row is 30 s of streaming write-through at
33 GB/s with ONE fsync per job at the end (24 fsyncs total); the fsync
ladder's cost is invisible against the stream, and the W-5 lever has
nothing to skip here — a 24-file stream touches every namespace, so
"touched" = "all" (10.7 requests per fsync on both arms, the extra 0.7
being the job's file-per-namespace spread). Bandwidth +1.1 % and daemon
CPU/op −4.2 % are within this row's run-to-run band (the two E rows differ
by 1.3 %). The F-only `fsync_phase_ns` on the 24 end-fsyncs reads
`meta_publish` 136–152 ms / `total` 169–195 ms — the end-of-stream fsync
waits for the whole in-flight publish conveyor, which is the row's
intended shape, and n = 24 is too few to compare across positions. The
`w_durable` verdict for W-5 is therefore: no regression, no claim.

## 5. The attribution control — `rr4k-il`

| arm | pos | IOPS | p50 µs | p99.9 µs | daemon µs/op | box busy % |
|---|---|---|---|---|---|---|
| E | 1 | 865,631 | 191.5 | 2,212 | 24.40 | 81.0 |
| E | 4 | 862,180 | 189.4 | 2,343 | 24.43 | 80.7 |
| F | 2 | 886,667 | 183.3 | 2,040 | 23.43 | 81.1 |
| F | 3 | 894,227 | 185.3 | 2,146 | 23.03 | 80.9 |
| **F/E** | Δ median | +3.1 % | −3.2 % | −8.1 % | −4.9 % | +0.1 % |

The shim's direct-drive path takes neither the READ nor the WRITE handler,
so its delta is the row's free-standing term — everything the campaign
tip changed OUTSIDE the two handlers (the striped stats words the service
threads also record into, the per-thread `ShardedAtomic` counters the
direct-drive ledger shares with the handlers, plus set state). It bounds
the kern rows' unattributed share: R-5's kern delta is 2.2× the control on
IOPS and 2× on CPU/op; W-6's is 4.3× / 2.5×. Engagement: `ipc_ops_read` ≡
fio ops + ramp, `ipc_direct_reap_stalls` = 0, on every row of both arms.

## 6. Verdicts and the board

| row | campaign | verdict | lands / owes |
|---|---|---|---|
| `rr4k-kern` | R-5 | **+6.8 % IOPS, −9.6 % daemon CPU/op (`fuse3-tpc` −18 %)**, both orders agree | the field row is MET — audit row 16 closes |
| `rw4k-kern` | W-6 | **+13.2 % IOPS, p50 −12.9 %, −12.2 % daemon CPU/op (`fuse3-ur` −15 %)**, both orders agree | the field row is MET — audit row 17 closes |
| `fsync-storm` | W-5 | **+23.8 % fsyncs/s, p50 −22.9 %**, data sync requests 10 → 1 per fsync, meta barrier still exactly one, tripwires 0 | the field row is MET — audit row 15 closes; the in-process +47–58 % was the barrier-only fixture's number, the field's is bounded by the partial-block flush below |
| `wdur-kern` | W-5 | PAR (+1.1 % bw, −4.2 % CPU/op, within band) | no regression on the `w_durable` shape; no claim |
| `rr4k-il` | control | +3.1 % / −4.9 % — the shared-word term | bounds the kern rows' attribution |
| fleet owner-dispatch A-B-B-A (`SQUEEZEFS_META_SHIP_INLINE_SERVE`) | D-5 | not run in this bracket (a co-writer fleet shape on the mw rig) | **still owed** — audit row 18 stays open on its field row |

**New board item (write board, from §4.1):** *fsync of a partial active
block escalates the whole block* — on the 256 KiB-write + fsync shape every
fsync reads 4 MiB of seed (`flush_seed_read_bytes` 18.4× user bytes) and
uploads 4 MiB (`durable_upload_bytes_escalation`), `data_flush` = 80 % of
the 6.9 ms fsync, the `patch_ineligible_adjacent` verdict (stream-adjacent
to the active block) is what routes the write onto the active buffer
instead of the W1 in-place arm. The candidate shape is an fsync-time
partial upload (the sub-block DMA the W1 patch already has, driven by the
coverage union rather than a whole-block seed) — a counted campaign, not a
patch: the seed read is what makes the later whole-block write-through
correct, so the row must prove the coverage-driven form against the
crash-consistency contract before it can skip it. Pre-existing on both
arms; W-5's instrument is what made it visible.

**Not in this bracket, unchanged:** the D-5 fleet row; the owed field rows
D-3 (mdstorm), W-2 (`w_fresh`), W-4 (`w_rewrite`); the unattributed
2026-09-01 rewrite tail.
