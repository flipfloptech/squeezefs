# 2026-08-06 — Round 4: the BDP-equilibrium hypothesis audited NEGATIVE; the wall named at the device plane (single-submitter ⇒ one fabric queue ⇒ one RX core per device) and the read-lane fan-out shipped

Branch `perf/read-queue-wall` (worktree off `integrate/zcrx-wave` tip
`cbc82b2a`, **unmerged — the orchestrator merges**). Charter: the field
falsified the cadence thesis as the whole story (pair `f12e9a03`:
D4 21.64 / D16 22.67 / governed 20.04 GB/s — still flat, mechanics
perfect). Audit every lane bound for a self-fulfilling measured-BW
term FIRST; close the ~52 arithmetic; fix by derivation.

Commits: red (contracts in `tests/nvme_dev_tests.rs`) · green (the
read-lane fan-out) — SHAs in the report. Local artifacts `/tmp/rbw4/`.

## 1. THE PRIME HYPOTHESIS, AUDITED — NEGATIVE (every bound enumerated)

Complete inventory of lane in-flight/issue bounds at `cbc82b2a`:

| bound | value at the field shape | measured-BW-derived? |
|---|---|---|
| `rl_inflight < depth` | pin (4/8/16); governed cap = `budget/8 ÷ bs ÷ streams` ≈ 176 | NO (pin/probe; the probe explores past delivery by design) |
| aggregate `read_lane_inflight ≤ budget/8` | 22 GiB ≈ 5,600 blocks | NO (static R5 share) |
| `examined ≥ examine_cap` (per-CALL walk) | `max(derived §5.5 cap, depth)` ≥ 16 | NO (hot-budget-derived) |
| `next > walk_horizon` = `edge+1+examine_cap` | ≥ edge+17 at D16 ⇒ ≥ 16/file ⇒ **≥ 256 offered** | NO |
| R5 Red ⇒ 0 | rows green | — |
| probe governor | DORMANT under the pin | — |

**No term derives from measured bandwidth or converged delivery** (the
retired BDP depth derivation died in the 2026-08-01 falsification and
never returned), and the horizon admits ≥ 16/file at D16 — the
"edge+3 × 16 ≈ 52" arithmetic does not hold in the code (examine_cap =
`max(derived, depth)`, so a D16 pin always widens the horizon with it).
The client-side caps admit ~256 in flight at D16; the row measured
~52-equivalent delivery. Little's identity then forces one of two
worlds: offered stalls at ~52 anyway, or offered rose and `fetch_dma`
rose proportionally (256 × 4.19 MB ÷ 22 GB/s ≈ 48 ms) because the
device plane WALLS at ~22 GB/s for the daemon's read shape. **The
"implied ~52" in the round-4 charter assumed the old 9.6 ms RTT — the
deployed instrument fix (whole-row `read_fill_phase_ns`) already
discriminates this in the existing D16 row artifacts: fetch_dma
≈ 9–10 ms ⇒ client stall; ≈ 40–48 ms ⇒ the device-plane wall.**

## 2. THE WALL, NAMED AND LOCALLY PROVEN — single-submitter-per-device

Mechanism: `NvmeBlockDev` runs ONE submitting worker thread per device;
blk-mq maps submissions to the per-CPU software queue, so one thread =
one hardware/fabric queue = **one nvme-tcp TCP connection = one RX
softirq core per device** — and nvme-tcp RX pays the per-byte kernel
copy on that core (the near-zero-copy census's standing posture line:
"RX pays one irreducible kernel copy per read byte"). ~2.7 GB/s/core ×
8 field namespaces ≈ **22 GB/s — the wall, invariant to client depth**
(added depth queues on the same connection: dev_service 4.4 → 7.8 ms
across rounds while throughput sat). The write plane escapes it (TX is
zero-copy spliced — 34 GB/s through the SAME single workers), and the
raw row escapes it (10 jobs = 10 submitting CPUs spread queues → 41.8).

**Local discriminator (tcp devsub, WRITTEN region only — the thin-read
pollution excluded; same 4 × zram devices):**

| shape (bs=4M, written 4 GiB/dev) | GB/s |
|---|---|
| 1 submitter/dev, qd16 (the daemon's shape) | **11.55** |
| 1 submitter/dev, qd32 (2× depth, same queue) | **12.08** (+4.6 % — the flat-ladder signature reproduced RAW, no FS involved) |
| 4 submitters/dev, qd4 (SAME per-dev in-flight as qd16) | **16.66** (+44 %) |
| 8 submitters/dev, qd2 | **17.66** |

Depth through one queue buys ~nothing; submitter spread at the same
in-flight buys +44 % — the wall is the submission topology, not the
devices and not the client pipeline.

## 3. THE FIX — read-lane fan-out, derived (never a constant)

* `NvmeBlockDev` gains a READ submission pool: reads round-robin
  `read_lanes` full workers (own thread/ring/O_DIRECT fd → distinct
  per-CPU queues); **writes, barriers and probes stay on lane 0** — the
  ordering and DUR-2 barrier reasoning untouched (a device flush is
  device-wide regardless of submitting queue; callers barrier after
  their writes complete). Latch-free (ArcSwap pool + relaxed rr;
  in-flight requests keep their lane alive by Arc). Monotone growth
  only; default 1 = exact prior posture (tests/offline tools
  unchanged).
* **Derivation**: `read_lanes_for(cpus, devices) = clamp(cpus /
  max(devices,1), 1, cpus)` — the term is "submitting CPUs per device";
  self-bounded by the machine (devices=1 ⇒ the raw row's own shape).
  Field: 32/8 = **4 lanes/device**; local: 32/4 = 8. Armed at mount
  after backend registration (`BackendRouter::arm_read_lanes`) and
  re-armed by the online `volume add-data` (count changed). Explicit
  `SQUEEZEFS_NVME_READ_LANES` wins verbatim (registry entry; `1` = the
  A/B lever). Gauge: `data_read_lanes` (stats inode).
* Red-first: `read_lanes_derivation_table` +
  `read_lane_pool_serves_exact_bytes_and_leaves_writes_on_lane_zero`
  (compile-red at base; byte-parity across a 4-lane pool under
  concurrency, monotone-growth pin, writes-on-lane-0).

## 4. Local A/B (the venue's honest limits)

Engagement exact (`data_read_lanes` 8 vs 1 in the row samples). The
FS-row bracket on this box is thermal-drift-dominated (Tctl ≈ 76–80 °C
mid-session; rows degrade monotonically within a session) — same-phase
pairs show the direction (fan 9.19 vs one 8.33 = **+10 %** early-phase;
bracket table in the report) while the raw §2 table carries the
mechanism's magnitude. The FS ceiling locally also has the handler-CPU
term above the device plane, so the full +44 % is not reachable
in-process here. **The 22 → 35.5 adjudication belongs to the field
row**, where the wall's 8 × RX-core arithmetic and the 41.8 raw
headroom both live.

## 5. Expected field gauges (fan-out pair, the same D-ladder + governed row)

* `data_read_lanes = 4` (32 cpus / 8 namespaces) on the stats inode;
  `ss -ti` on the daemon shows ~4 established nvme-tcp connections per
  namespace carrying read traffic (was 1 hot).
* The D-ladder finally slopes; `dev_service` stays ON the raw row's
  latency-throughput curve as in-flight grows (off-curve growth at flat
  GB/s = the old single-connection serialization signature).
* The governed row's probe adopts (`ups > backoffs`,
  `depth_target` 2–4+): with the wall lifted, +16 fills/epoch responds.
* Cold 128 GB row: 22 → toward the 35.5 bar; per-CPU: softirq spread
  across ~4 cores/namespace instead of one saturated core per device.
