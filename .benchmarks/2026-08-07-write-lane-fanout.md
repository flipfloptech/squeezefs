# 2026-08-07 — DATA-WRITE submission fan-out across derived per-device lanes (the read-queue-wall fix's write half)

Branch `perf/write-lane-fanout` (worktree off dev tip `e0d0575c`).
Commits: red contracts (`tests/nvme_dev_tests.rs` + `tests/dma_fence_gate_tests.rs`) ·
green (the fan-out) · this note. Local artifacts `/tmp/wlanes-rows2/`.

## 1. The evidence — writes hit their own per-connection wall

Field decomposition (2026-08-07, live rows on squeeze-test): kernel seq
writes **35.31 GB/s = 7.06 GB/s × 5 data namespaces** — a per-device
constant, the read wall's signature shape one octave up.
`write_pipeline_phase_ns` shows per-block residence dominated by **`dma`
(mode 2–8 ms)** with the interior µs-clean (admit/lock/crypto/allocate/
publish all fine); the depth governor sits **pinned at base with
`probe_ups` 20 = `backoffs` 20** — depth probes into a per-connection
wall convert nothing, exactly the read campaign's flat-D-ladder
signature (`.benchmarks/2026-08-06-read-queue-wall.md` §1). **Raw fio
reaches 49.7 GB/s on the SAME namespaces** by spreading queues.

The read half already shipped (`a17f625d`): `read_lanes_for(cpus,
devices) = clamp(cpus/max(devices,1), 1, cpus)` in `src/nvme_dev.rs`,
+44 % raw local, with the explicit scope note that *"writes escape it
(TX is zero-copy spliced — 34 GB/s through the same single workers)"*.
That statement was true and is now obsolete: the TX splice's
per-connection ceiling is HIGHER than RX's (~7 vs ~2.7 GB/s/connection
class) but it exists, and the field found it. One submitting worker
thread per device = one blk-mq software queue = one nvme-tcp connection
for the write direction too.

## 2. The ordering adjudication (verdict: conservatism, not a blocker)

The read campaign's exclusion of writes was **evidence-based
conservatism** ("writes escape it"), not a structural constraint. Every
ordering obligation was walked at `e0d0575c`:

1. **Per-offset ordering.** The single worker never guaranteed device
   COMPLETION order — the worker ring uses no `IO_LINK`/`IO_DRAIN`, up
   to 1024 SQEs are in flight concurrently, and NVMe completes same-queue
   commands out of order. Per-offset serialization lives ABOVE the device
   layer: `BLOCK_FLUSH_LOCKS` (lock order 3), awaited write oneshots
   before dependent publishes, the W1 §5.1 clone/patch fence. What the
   single lane DID provide structurally is same-FIFO SUBMISSION order.
   The fix preserves exactly that: the write-lane pick is the pure
   function `write_lane_index(offset, grain, lanes) = (offset / grain) %
   lanes` with grain = the volume block size — same block → same lane →
   same channel FIFO → same connection. Two DMAs naming one device
   offset (supersession, in-place overwrite, W1 sub-block patch + the
   whole-block write of the same block) can never split across lanes.
   No new reordering surface; no caller-discipline dependency added.
2. **Barriers (`NvmeBlockDev::flush`, DUR-2).** A device flush command
   covers only commands the device completed before processing it — and
   an io_uring `Fsync` never drained even its OWN ring's in-flight SQEs
   (no `IO_DRAIN`), so the shipped single-lane contract was honest only
   because callers barrier after awaiting their DMAs. With lanes, the
   barrier now **drains every lane's write watermark first**: each lane
   carries `writes_submitted`/`writes_completed` counters (worker-side
   completion accounting — every terminal outcome counts: CQE, SQ-full
   refusal, teardown drain; a dead worker fails the barrier loud), and
   `flush` snapshots every lane's submitted watermark at barrier start,
   parks until each lane's completions reach its snapshot (Dekker
   SeqCst waiter-gate + `tokio::sync::Notify` — the completer pays a
   wake only toward a parked barrier, the `transport_wake_*`
   discipline), and only then issues the ONE device Fsync on lane 0.
   Strictly STRONGER than the shipped posture. The alternative
   (per-lane `IO_DRAIN` Fsync fan-out + join) was rejected: `IO_DRAIN`
   orders behind ALL prior SQEs *including in-flight reads* (a wedged
   read's 30 s timeout class would hold every fsync hostage — lanes are
   a shared read/write pool), and it pays N device flushes per barrier
   instead of one. Journal barriers are untouched — the metadata plane
   rides `crate::uring_fs`, not `NvmeBlockDev`.
3. **D0 fence law.** `fence_gate` (RES-6 + S7 `authorize_dma`) runs per
   submission in `write_block`/`write_block_authorized` BEFORE the lane
   pick — lane-independent by construction. Pinned:
   `fenced_device_refuses_writes_on_every_lane` (every offset class
   refuses `WriterGuardFenced`, `data_dma_fence_refusals` counts each,
   and a refused write never reaches any lane's channel).
4. **Reclaim (discard/punch).** Never rode the uring workers at all —
   `BLKDISCARD`/`fallocate(PUNCH_HOLE)` are blocking-pool ioctls
   (`src/block_reclaim.rs`, "io_uring posture" note). No lane class to
   assign; the freed-offset non-reallocatable-until-reclaimed law is
   orthogonal to submission lanes. The D14 zc write leg (`zc_write_fd`)
   bypasses the workers on its own fd and keeps its own gate
   (`authorize_zc_store`), unchanged.

## 3. The fix — same derivation, affinity not round-robin

* `io_lanes_for(cpus, data_devices)` — `read_lanes_for` generalized
  verbatim (`clamp(cpus / max(devices,1), 1, cpus)`, the "submitting
  CPUs per device" term; no new constants). Field write shape: 32/5 =
  **6 lanes/device**; local devsub: 32/4 = 8.
* `NvmeBlockDev` now holds ONE shared lane pool (full `UringWorker`s —
  own thread/ring/O_DIRECT fd → distinct per-CPU blk-mq queues →
  distinct fabric connections): reads round-robin the first
  `read_lanes`; writes offset-affinity over the first `write_lanes`;
  **barriers and probes stay on lane 0**. Default `write_lanes = 1` =
  byte-for-byte the pre-change posture (offline tools/tests unchanged;
  `write_lane_pick` short-circuits to the construction worker).
* Affinity grain = the volume block size, handed at arm
  (`BackendRouter::arm_io_lanes`) and re-stamped by
  `DataRouter::set_block_size` at FUSE init (the mount arms before the
  config read; affinity is a structural-preservation property, not a
  correctness dependency, so the re-stamp is safe).
* Armed at mount after backend registration and re-armed by the online
  `volume add-data` (monotone — lane counts only grow). Explicit
  **`SQUEEZEFS_NVME_WRITE_LANES`** wins verbatim (registry entry;
  `1` = the pre-change posture, the A/B lever), mirroring
  `SQUEEZEFS_NVME_READ_LANES`.
* Gauges: **`data_write_lanes`** (armed count, beside
  `data_read_lanes`) and **`data_write_lane_submits`** (per-device
  per-lane write submit counters, `<dev>=<lane0>,<lane1>,…` — the
  engagement instrument: a fan-out row is INVALID unless more than one
  lane's counter moved per device).

## 4. Contracts (red-first) + weakening verification

`tests/nvme_dev_tests.rs`: `io_lanes_derivation_table` (32/5 = 6 is the
write field shape) · `write_lane_affinity_is_pure_and_block_stable`
(same offset → same lane; same block → same lane incl. W1 sub-block
shapes; consecutive blocks cover every lane; degenerate inputs floor at
lane 0) · `write_lane_pool_spreads_by_offset_and_stays_byte_exact`
(per-lane submit deltas exactly the residue classes; same-block repeat
writes move ONE lane; byte parity across the span; monotone growth;
default = 1) · **`flush_completes_no_earlier_than_writes_on_every_lane`**
— THE red DUR-2 contract: a write stalled in flight on lane 3 (the
`set_test_write_stall` seam, the read-stall seam's twin) must be
complete when a concurrent `flush` returns. **Weakening-verified**: with
`drain_write_lanes()` disabled the test fails exactly at the "barrier
returned while a write submitted before it started was still in flight
on another lane" assertion; restored, green.
`tests/dma_fence_gate_tests.rs`: `fenced_device_refuses_writes_on_every_lane`.

Gate at final HEAD: root clippy `-D warnings` (both feature configs) +
fmt clean; fuse3 workspace clippy/fmt/suite/bench-smoke clean; root
bench smoke clean; `cargo audit` both lockfiles (allowed-warning residue
only); knob-convention + derivation-sweep suites green; blast-radius
suites (`write_pipeline_tests`, `write_through_coverage_tests`,
`fsync_writeback_tail_loss_tests`, `rw5a_never_lossy_tests` + the two
contract suites) **green ×10 consecutive** (60/60 suite runs).
Full-suite note: `data_path_correctness_tests::
test_aligned_striped_overwrite_equivalence_64k_blocks` flaked once
(line-856 post-cache-invalidation arm) — adjudicated **pre-existing at
the dev tip**: counted ×40 brackets fail base `e0d0575c` 2/40 and this
branch 1/40 with the identical assertion signature (base ≥ branch;
fixture never arms lanes, so the write routing there is byte-identical
by the `write_lanes == 1` short-circuit). Also fixed in passing: the
pre-existing parallel-observation flake in
`read_bounce_pool_routing_and_home_recycle` (process-global pool gauges
sampled without the file's serial discipline — ~40 % failure rate under
the parallel runner even with the new tests skipped; now serialized).

## 5. Local rows (D15 — the venue's honest limits)

Substrate: the **tcp devsub** (nvmet-tcp on 127.0.0.1, zram-backed data
namespaces /dev/nvme{5..8}n1 — the fabric-sensitive venue) · instrument:
fio libaio `direct=1 rw=write bs=1M iodepth=8 numjobs=8 size=2g
end_fsync=1` over the kernel mount at `/mnt/sqz-wlanes`, fresh format
per leg, A-B-B-A (A = derived lanes 8/device, B =
`SQUEEZEFS_NVME_WRITE_LANES=1`) · rig
`.benchmarks/rigs/2026-08-07-write-lane-fanout-rig.sh`.

The charter expected a flat local delta (zram device capping first, the
read fan-out's local story); the venue instead SHOWED the wall — this
box's nvmet-tcp loopback single write connection ceilings below the FS's
offered load — so the A-B-B-A bracket carries direction here too.
These are engagement/correctness rows (the task's acceptance
instruments), not scoreboard rows (no amplification columns; the field
row spec below carries that discipline):

| leg | mode | GB/s (fio, 17.2 GB/leg) | P0 smoke (cp && sync ×3, md5) | engagement (per-lane submit deltas) |
|---|---|---|---|---|
| 1A | derived (8 lanes/dev) | **1.493** | clean | all 8 lanes ≈128–136 per device ×4 devices |
| 2B | =1 (pre-change) | 0.836 | clean | lane 0 only (~1039), lanes 1–7 = 0 — pinned |
| 3B | =1 | 0.772 | clean | lane 0 only (~1035) |
| 4A | derived | **1.249** | clean | all 8 lanes ≈128–136 ×4 |

Both brackets agree (A₁/B₂ = **+79 %**, A₄/B₃ = **+62 %** — order-
independent per the A-B-B-A rule), `data_write_lanes` gauges 8 vs 1
exactly, the residue-class distribution is textbook (lane 0 carries its
class + the barrier fsyncs), and the fence tripwire stayed 0.

Raw discriminator (fio straight at the 4 data namespaces, bs=4M
time_based 30 s, same in-flight per device — the venue's own wall):

| shape | GB/s |
|---|---|
| 1 submitter/dev, qd16 | 1.00 |
| 4 submitters/dev, qd4 (same in-flight) | **1.85** (+85 %) |

Depth through one connection buys nothing this venue can't already do;
submitter spread at the same in-flight buys +85 % — the write-direction
twin of the read campaign's §2 table.

## 6. The FIELD row spec (runs LATER — squeeze-test is the user's right now)

The 22→35.5 read adjudication's write twin. On squeeze-test (32 CPUs, 5
data namespaces → `data_write_lanes = 6`), same venue/fileset as the
35.31 GB/s row:

```bash
# A/B pair, fresh mount each; B leg prepends SQUEEZEFS_NVME_WRITE_LANES=1
squeezefs mount <sqmeta-uri> /scratch/tmp/test --daemon --allow-other \
  --log-file /scratch/tmp/logs/sqz-wlanes.log
cat /scratch/tmp/test/.stats > before.json
fio --name=wl --directory=/scratch/tmp/test/exa_perf --ioengine=libaio \
    --direct=1 --rw=write --bs=1M --iodepth=8 --numjobs=16 --nrfiles=8 \
    --size=8g --time_based --runtime=60 --ramp_time=10 --group_reporting \
    --end_fsync=1 --output-format=json --output=wl-fio.json
cat /scratch/tmp/test/.stats > after.json
```

Expected on the armed leg:
* `data_write_lanes = 6`; `data_write_lane_submits` deltas move **all 6
  lanes on every namespace** (engagement — an armed row with one moving
  lane is INVALID); `ss -ti` shows ~6 established nvme-tcp connections
  per namespace carrying write traffic (was 1 hot).
* Seq write moves **off the 35.3 = 7.06 × 5 shape toward the raw 49.7
  class** (the per-connection term is the attributed wall; the residual
  gap is the daemon's own CPU terms).
* `write_pipeline_phase_ns.dma` mode shifts DOWN out of the 2–8 ms class
  (the queue-wait share of "dma" is what the spread deletes), and the
  depth governor's probes start CONVERTING (`probe_ups > backoffs`,
  `depth_target > base`) — with the wall lifted, added depth responds.
* The control leg (`SQUEEZEFS_NVME_WRITE_LANES=1`) reproduces the 35.3
  class with lane-0-only submits — the A/B attribution.
* Both legs: `data_dma_fence_refusals` flat 0, amplification columns
  per the standing write-row requirement (`tests/write_amp_rig.sh`
  discipline), md5 + `cp && sync` P0 smoke clean.

Sustained-state rule applies: the headline field row is the 60 s
`time_based` window (flat across thirds), not a burst.

## 7. Field confirmation (2026-08-07, perf/field-confirmations-0807 — RUN)

**Venue/instrument** (stated per the standing rules): squeeze-test
(32 CPUs, EL8, 6.19.14-sqz), real NVMe-oF TCP fabric — reset-v5 converged
cluster (5 storage nodes × [1 nullb meta + 2 nullb 48 GiB data namespaces],
2 fabric paths/subsystem, iopolicy round-robin), task format = 5 meta +
**5 data namespaces** (aqr37-d0/d1, aqr38-d0/d1, aqr39-d0); binary+shim
`cc5e4ab1` (md5-verified deploy); fio 3.36 libaio `direct=1 rw=write bs=1M
iodepth=8 numjobs=16 nrfiles=8 size=8g time_based 60 s + 10 s ramp,
end_fsync=1`, fresh dir per leg. Rig
`.benchmarks/rigs/2026-08-07-fieldconf-set1-wlanes.sh` (+ its analyzer /
raw-calibration sibling); artifacts `/scratch/tmp/fieldconf-0807/set1*`.

**Venue-stationarity lesson (first run = the aging capture).** The
memory-backed null_blk data plane is a CONSUMABLE venue: a first pass
without target resets decayed 27.3 → 18.7 GB/s monotonically across legs
regardless of arm, and the two A-B-B-A brackets DISAGREED (1.30× forward,
0.96× reversed) — the ordering-artifact signature, row set INVALID for the
bracket (kept as `set1-run1-agingcapture/`). The creditable run resets the
cluster (`cluster_reset_v4.sh`) before EVERY leg, so each leg starts from
the reference state; device names are re-resolved from NQNs per reset.
**Same-day venue grade**: the reference epoch's raw class did NOT
reproduce — raw fio on the same 5 namespaces reads **24.22 GB/s**
(1 submitter/dev qd16) and **38.05 GB/s** (6 submitters/dev qd3, same
in-flight) vs the reference epoch's 49.7; the venue sits at ~77 % of the
epoch the 35.31 GB/s wall was measured in. Absolute-GB/s comparisons to
§6's targets are therefore ratio-graded against the same-day raw rows.

**The A/B (reset per leg, 8 legs = A-B-B-A + B-A-A-B, medians of 4/arm):**

| leg | arm | GB/s (60 s sustained) | engagement | governor (target vs base; ups/backoffs Δ) |
|---|---|---|---|---|
| 1A | derived (6 lanes/dev) | 24.38 | all 6 lanes ×5 devs | **462 M > 296 M; +17/+14 (converting)** |
| 2B | lanes=1 | 21.71 | lane 0 only, pinned | 381 M = base; +19/+19 |
| 3B | lanes=1 | 23.85 | lane 0 only | 266 M = base; +19/+19 |
| 4A | derived | **31.07** | all 6 ×5 | **185 M > 169 M; +22/+21** |
| 6B | lanes=1 | 22.40 | lane 0 only | 270 M = base; +20/+20 |
| 7A | derived | **30.83** | all 6 ×5 | **178 M > 168 M; +24/+22** |
| 8A | derived | 24.67 | all 6 ×5 | 224 M = base; +16/+16 |
| 9B | lanes=1 | 22.58 | lane 0 only | 284 M = base; +19/+19 |

* **Medians: A 27.75 vs B 22.49 GB/s = +23.4 %**, and every adjacent
  A/B pair agrees in direction across both orders (1.12× / 1.30× /
  1.38× / 1.09×). Same-day-raw-normalized: the fan-out arm delivers
  **0.73× raw-spread** vs the control's **0.59×** — the control's
  FS/raw matches the reference epoch's 35.31/49.7 = 0.71 shape only
  through the fan-out arm; the single-lane posture has fallen further
  behind the venue than it was at the reference epoch.
* **Governor conversion — the §6 signature CONFIRMED**: probes convert
  only on the armed arm (target > base with ups > backoffs on 3 of 4 A
  legs; every B leg pinned target ≡ base, ups ≡ backoffs). `dma` phase
  mode moves down one octave (B mode `<=8ms`, A mode `<=4ms`) — down,
  though not out of the 2–8 ms class on this venue.
* **Amplification columns**: dev/user 1.007–1.011, wareq-sz ≈ 2.5–3.0 MiB
  on every leg (no request-size collapse); `data_dma_fence_refusals`
  flat 0; P0 smoke (`cp` 32 MiB `&& sync f`, md5 ×3) clean on all 9 legs.
* **Connection census (ss -ti, >100 MB TX in a 15 s mid-row window)**: A
  legs saturate the data-queue population UNIFORMLY — 320 hot = exactly
  (2+2+1 data subsystems) × 2 paths × 32 queues; B legs read 224–300 with
  per-target asymmetry. The §6 "was 1 hot" expectation is NOT what ss
  shows on this kernel: io-wq punting + round-robin multipath spread even
  the single lane across many queues — the delivered difference is
  submitting-thread width (the product's own `data_write_lane_submits`
  ledger is the engagement instrument; ss corroborates uniform-vs-partial).
* **Armed compose variant (labeled, 1 leg)**: `SQUEEZEFS_FUSE_ZC=1` +
  derived lanes = **25.22 GB/s** — inside the A population's spread
  (24.4–31.1); no separable zc term at this venue's variance.
* **Within-leg thirds** varied leg-to-leg (flat 1.02 on the best A legs —
  which were also the fastest — up to 1.6 on decaying ones): the null_blk
  first-pass/rewrite transition is visible INSIDE a 60 s row; the quoted
  figures are the full measured-window means per the sustained-state law.

**Verdict:** the fan-out leaves the single-lane wall on the real fabric —
**+23 % at medians, direction unanimous across four order-alternating
brackets, engagement exact, governor converting only when the wall is
lifted** — but the venue's absolute class has degraded ~24 % below the
reference epoch (raw-graded), so "35.31 → raw-49.7-class" could not be
re-tested as an absolute; on today's venue the control arm's wall sits at
≈ 22.5 GB/s and the fan-out clears it to ≈ 27.8–31.1.
