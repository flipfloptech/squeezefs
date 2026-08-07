# 2026-08-07 — The shim's HYBRID LANE GATE: large ops → kernel FUSE lane, small ops → IPC ring

| | |
|---|---|
| **Charter** | Ruling **D14**'s fleet-posture corollary (`docs/rc-manifest.md` §3f: *"with kernel-lane reads AND writes at zc, the interception shim's charter narrows toward the IOPS/rand-small lane, big-sequential traffic riding the kernel lane"*) formalized INSIDE the shim: a derived size gate in the routing funnels routes strictly-larger-than-threshold ops to the kernel FUSE lane via the proven `RingServe::Real` fallthrough; everything else rides the ring untouched. |
| **Branch / SHAs** | `feat/shim-hybrid-lane-gate` off dev `e0d0575c` — tests `873ad664` (red), impl `2c794bdd`, gate-script evolution `6d3860b4`, this note |
| **Field truth motivating it** | kernel lane (zc armed) 49.2 GB/s read / 35.3 GB/s write at 1 MiB; ring lane 594 k rand-4k read IOPS (+135 % vs kernel) but 28 GB/s psync reads (consume copy + qd1). Big ops belong kernel; small ops belong ring. |
| **Local verdict (D15: local-first)** | Engagement **EXACT** on every row (lane bytes ≡ row bytes, ring ops ≡ row ops, sticky routes ≡ op count); A/B (gate ON vs `SQUEEZEFS_IL_KERNEL_LANE_MIN=0`): **psync 1 MiB reads +66 % (both bracket orders)**, psync 1 MiB writes par-to-modestly-ahead (order-sensitive), **rand-4k parity+ (1.03–1.04×)**. Ratios only — local venue law. |

---

## 1. Design ledger (the adjudications)

### 1.1 Gate placement — positional per-op; offsetful STICKY per description (adjudicated)

* **Positional forms** (`pread[64]`/`pwrite[64]`, `preadv*`/`pwritev*` positional shape, libaio iocbs): gated **per op** inside `ring_positional_core` (the single serve-vs-Real funnel) after the session lookup — no `f_pos` state, so routing is free. Vector forms gate on the **total** requested bytes (the kernel serves the whole vector as one FUSE request — the per-op-overhead-vs-per-byte model prices the op, not a segment).
* **Offsetful forms** (`read`/`write`/`readv`/`writev`): **sticky per-description latch**, adjudicated over both alternatives:
  * *per-op routing* was rejected: every ring→Real fallthrough flushes and **permanently demotes** the PERF-7 offset mirror (`disarm_if_current` — the mirror never re-arms), so a mixed-size offsetful stream would flap the fd back to the two-syscall lseek-resync discipline forever, the exact cost PERF-7 exists to delete;
  * *conservatively disabled in v1* was rejected: the offsetful large-write stream (dd bs=1M — the `.benchmarks/2026-08-06-dd-width-slope.md` shape) is precisely the traffic the gate exists to move.
  * The latch (`BindingCell.kernel_lane`, a monotone relaxed `AtomicBool`) engages **exactly once**, under the cell's offset lock, together with the one-time mirror flush; dup siblings share it exactly as they share `f_pos` (one description = one offset = one lane); a fresh bind installs a fresh cell (unlatched). **Documented cost**: small ops on a latched fd ride kernel too — no flapping, counted honestly in the ledger. Offset-consistency edges stay with the mirror's own Dekker protocol; the latch carries none.
* **libaio**: per-iocb reroute at the batch position (the mixed-batch core's existing kernel-lane venue — prefix law preserved). The screen now runs **size-blind** (`slab = u64::MAX`) and the two size laws — the structural one-slot slab law (a multi-slot async op would hold slots hostage) and the gate threshold — resolve in ONE place, so the ledger attributes **every size-based kernel route** (this is what makes a libaio 1 MiB row's `ipc_lane_gate_kernel_bytes` ≈ row bytes). Gate OFF (`=0`) keeps the structural slab reroute (a slot-shape constraint, not a gate decision) but counts nothing — the A/B lever stays clean.
* **O_DIRECT is NOT consulted in v1** (direct+large is the strongest kernel-lane candidate, but the size gate alone already routes it; must-not-be-required per the charter). The hook exists on both sides if a counted row prices the refinement in: the shim sees the flags at bind-classify time, the daemon's `BindingRights` already carries `odirect`.
* **Boundary convention**: route kernel iff `len > threshold` (strictly greater — the threshold is the *largest op the ring keeps*, so the derived floor leaves the single-flight serial path untouched; an exactly-64 KiB pwrite — the linked-harness/elbencho shape — stays ring). `0` is special-cased as **gate OFF** (all eligible ops ring — the A/B lever), never "everything routes kernel".

### 1.2 The threshold formula (derivation law — never a bare constant)

`crates/squeezefs-ipc/src/sizing.rs::kernel_lane_min_default(mem_bw, slab, max_op)`:

```
ring_time(size)   ≈ ring_rtt + copies × size / memBW
kernel_time(size) ≈ fuse_rtt            (zc: ≈ 0 per-byte userspace cost)
crossover: size* = memBW × (fuse_rtt − ring_rtt) / copies
threshold = clamp(round_down_4KiB(size*), [slot_slab, max_op_bytes])
```

* **memBW** = the shim's startup micro-probe (`lane_gate::probe_mem_bw_bytes_per_sec` — best-of-3 copies of a 4 MiB buffer, ~1 ms once per process, memoized; runtime behavior, never a CPU-ID table — the portability law). *Honest caveat*: on big-L3 parts the 4 MiB working set is LLC-resident, so the probe reads L3-class bandwidth (this box: ~125–146 GB/s across runs) rather than DRAM — which is arguably the closer model of the arena copy (the client just produced the payload), and its only effect is a HIGHER threshold, i.e. conservative toward the ring. A failed probe (0) lands on the slab floor.
* **`KERNEL_LANE_RTT_DELTA_NS = 2_300`** — a MEASURED class constant (the W2 fold-amortization-constant pattern), from the D14 field bracket's rand-4k rows (the shape where per-byte cost vanishes and the per-op delta is the whole difference): ring 594 k IOPS vs kernel ≈ 253 k ⇒ 1/253 k − 1/594 k ≈ 2.27 µs. Pinned by the derivation tie test; retuning it is a deliberate act with a new counted citation.
* **`copies = 2`** — the read side's two CPU passes over the payload (daemon serve-into-arena + client consume copy — the read-copy-ledger's E-IL closure). Writes pay one client copy, crossing at 2×; both collapse to the same clamp floor at every measured memBW on target hardware, so ONE threshold (the conservative read side — also the direction with the larger field gap, 28 vs 49.2 GB/s) governs both directions.
* **Rails, each physical** (on the line in the code): 4 KiB grain = the LBA/page grain `slot_slab` floors to; floor = the slot slab (at/below it a ring op is the single-flight allocation-free serial path — the IOPS lane's own regime, protected, never taxed); ceiling = `max_op_bytes` (past it an op must chunk into multiple ring flights — RTT multiplication — so the gate must have engaged at latest there).
* **Env**: `SQUEEZEFS_IL_KERNEL_LANE_MIN` — explicit wins VERBATIM (incl. sub-slab values; an override lever is not clamped), `0` = gate off, malformed announces once and keeps the derived default (the shim's documented ENG-10 asymmetry). Registry row in `src/env_knobs.rs`; the convention census test covers it.
* Default-geometry consequences: 64 MiB arena / 1024 slots ⇒ slab 64 KiB, `max_op` 1 MiB. This box derives **140–164 KiB** (probe variance run-to-run ~15 %, both interior and 4 KiB-grained); a ~15 GB/s DRAM-class client derives the slab floor (64 KiB) exactly — "single-flight stays ring, chunked goes kernel".

### 1.3 Observability (the split-attribution ledger)

* `ipc_lane_gate_kernel_routes` / `ipc_lane_gate_kernel_bytes` — shim-side gate decisions. A kernel-routed op never touches the ring, so the daemon cannot see it: the counters ride the **client stats page** — the session layout's stats region gains its typed form (`ClientStatsPage`, `IPC_ABI` 3 → 4; region arithmetic unchanged, the page always existed all-zeroes; client-writable, **display-only** on the daemon side per the §5.3.1 trust boundary — summed verbatim into the export, never a control input). The daemon sums LIVE sessions + folds each session's totals **exactly once at teardown under the registry lock** (monotone across session churn, and a concurrent snapshot counts every session on exactly one half).
* `ipc_lane_gate_threshold_bytes` — the resolved threshold, published by each session at establish; exported as **max over LIVE sessions** (a gauge — it drops with them; a reaped fleet reads 0 while its routes/bytes persist).
* Bytes are counted at route time (requested bytes; a short real serve is kernel business). All three keys export unconditionally (zeros on non-interception mounts).
* **Row-validity law**: a hybrid il row is attributable iff `ipc_lane_gate_kernel_*` deltas account for its large-op traffic while `ipc_ops_*`/`ipc_bytes_*` account for its small-op traffic.

### 1.4 The decision table (pinned in `crates/squeezefs-preload/tests/lane_gate_tests.rs` + `tests/preload_parity_tests.rs`)

| Shape | Gate arm | Decision |
|---|---|---|
| `pread`/`pwrite` (positional), len ≤ thr | per-op | ring |
| `pread`/`pwrite`, len > thr | per-op | kernel (Real; binding stays, no demote), counted |
| `preadv`/`pwritev` positional, Σiov > thr | per-op on TOTAL | kernel, counted |
| `read`/`write`/`readv`/`writev`, unlatched, len ≤ thr | sticky | ring (unchanged) |
| `read`/`write`, unlatched, len > thr, rights ok | sticky trigger | latch + one-time mirror flush under the offset lock → kernel, counted |
| any offsetful op on a LATCHED description | sticky | kernel, counted (incl. small ops — the documented no-flap cost) |
| wrong-direction op, any size | rights first | Real via the kernel's EBADF — **never** a gate route, never counted |
| libaio iocb, would-be-Ring, nbytes > slab or > thr | per-iocb at batch position | kernel, counted (uncounted when gate is OFF — structural slab law only) |
| `thr = 0` (env `=0`) | all | gate off — every eligible op rings; aio keeps the structural slab reroute uncounted |
| unbound fd / refused session / poisoned | pre-existing valves | untouched (Real), never counted |
| dup sibling of a latched fd | shared cell | kernel (one description = one lane) |
| fresh bind on a reused fd number | fresh cell | unlatched (lane state is per-description, never per-fd residue) |
| exactly-at-threshold op (e.g. 64 KiB at the slab floor) | strictly-greater law | ring (the linked-harness/elbencho 64 KiB shape stays the IOPS lane's) |

### 1.5 Correctness invariants

Routing to Real preserves everything the availability valve preserves — same fallthrough, same errno semantics (the real call's own), binding lifetime untouched, poison law untouched (the gate returns `Real{poisoned:false}` shapes only; poisoned sessions keep their own ladder). Mixed-lane traffic on one fd is coherent by construction (both lanes funnel into one daemon custody) — pinned by `mixed_lane_writes_on_one_file_stay_coherent_and_both_lanes_account` (interleaved ring-small + kernel-large writes on one ino, readback exact through EITHER lane, both lanes' engagement counters moving) and live by the gate script's cross-lane parity row.

### 1.6 The preload gate evolved with the posture (the first run's honest failure)

The first post-gate `sudo tests/run_preload_gate.sh` failed at the legacy §3-rule-4 engagement row — cp's 256 KiB buffers and the bs=1M dd rows now legitimately ride the kernel lane (with byte parity holding THROUGH it, including the notify-delivery settle rows: the posture working, not a regression). The gate script now pins `SQUEEZEFS_IL_KERNEL_LANE_MIN=0` for the legacy ring rows (they keep proving the ring serves what it claims) and adds the **2g-lane** row for the DERIVED posture: bs=1M dd routes kernel with `ipc_lane_gate_kernel_{routes,bytes}` accounting for the row, threshold gauge published from a LIVE derived session, 4k stays ring with gate counters unmoved, cross-lane readback byte-exact. Full sudo run: **both legs PASSED** (threshold=167936 on this box, kernel routes +4 / bytes +4 MiB for the 4 MiB dd).

---

## 2. Verification (local-first, D15)

* Red-first contracts, all green ×10: decision table + resolve law + probe sanity + sticky latch (`lane_gate_tests.rs`, 7 tests); derivation tie test (`derivation_sweep_tests.rs::il_kernel_lane_min_derives_from_membw_and_lane_rtt_delta`); mixed-lane coherence + stats-page export/fold/gauge (`preload_parity_tests.rs`, 2 tests + the 3 keys in `data_plane_stats_fields_export`).
* `cargo clippy --all-targets --all-features -- -D warnings` and `cargo clippy --all-targets -- -D warnings` clean; `cargo fmt --check` clean.
* Full suites green: squeezefs-preload (all), squeezefs-ipc (all), preload_parity (16), preload_authn/session/lifecycle, ipc_host (37), ipc_op_economy, ipc_hold_probe, ipc_direct_drive, shim_parity, ingest_economy, ipc_admission_balance, env_knob_convention (24), derivation_sweep (24), metrics.
* `sudo tests/run_preload_gate.sh`: **both legs PASSED** incl. the new 2g-lane row (§1.6).

## 3. Local live rows (tcp devsub, instance `hyb` — own venue)

**Substrate/instrument**: `tests/dev_substrate.sh` nvmet-**tcp** instance `hyb` (2× null_blk mds `/dev/nvme9n1,/dev/nvme10n1`, 2× zram oss `/dev/nvme11n1,/dev/nvme12n1` — the shared-box main tcp substrate was owned by a foreign agent's live mount and was not touched); cache-less format; mount `/mnt/sqz-hybrid` `--interception --allow-other`, **`SQUEEZEFS_FUSE_ZC=1`** (armed: `buffers=kmbuf-bufring+zero-copy`, `kmbuf_ops=38/39 (7.1-sqz)`; `fuse3_zc_replies` 16,439 by end of rows — the kernel lane IS the zc lane). Instrument: fio 3.42 (dynamic, `LD_PRELOAD=libsqueezefs_il.so`), `direct=1`, group_reporting; daemon+shim same clean commit `6d3860b4` (no dev override). Rows are 2 GiB bursts / 10 s timed — **ratio brackets, not sustained headline claims** (local venue law: ratios only).

Gate ON = derived default (threshold gauge read live: **143,360 B** during these rows; 167,936 B in the gate run — probe variance, both interior); gate OFF = `SQUEEZEFS_IL_KERNEL_LANE_MIN=0`.

### 3.1 Engagement (gate ON — every check EXACT)

| Row | Expectation | Result |
|---|---|---|
| E1 libaio 1M write ×4 GiB (qd8×2) | `ipc_lane_gate_kernel_bytes` Δ ≈ row bytes; ring in ≈ 0 | Δ = **4,294,967,296 ≡ row bytes**; `ipc_bytes_in` Δ = 0 |
| E2 libaio 1M read ×4 GiB | same, read side | Δ = **4,294,967,296 ≡ row bytes**; `ipc_bytes_out` Δ = 0 |
| E3 psync rand-4k read 10 s ×2 jobs | `ipc_ops_read` Δ ≈ row ops; gate routes 0 | row ios 1,092,940; `ipc_ops_read` Δ = **1,092,940 ≡**; lane routes Δ = 0 (109 k IOPS) |
| DD offsetful sticky: `dd bs=1M count=1024` (write(2)) | routes ≈ ops, ring writes 0 | lane routes Δ = **1024 ≡**; `ipc_ops_write` Δ = 0 |

### 3.2 A/B — gate ON (derived) vs OFF (`=0`, all ring), alternating both orders

psync seq 1 MiB, 2 jobs, 2 GiB each, same fileset; forward bracket ON/OFF alternating ×3, then the reversed **B-A-A-B** bracket (the aging-store rule):

| Row family | Forward bracket (on/off ×3, MB-class bw_bytes) | Reversed B-A-A-B | Verdict (ratios) |
|---|---|---|---|
| **seq read 1M** | on 7052/7018/6498 vs off 4040/4114/4194 → medians **7018 vs 4114 = 1.71×** | off 5389 / on 6817 / on 5711 / off 3890 → **on 6264 vs off 4639 = 1.35×** | **Gate ON wins BOTH orders, pointwise in-bracket** — combined medians 6817 vs 4114 ≈ **+66 %** (the ring's consume-copy + serial chunk RTTs vs the zc kernel lane) |
| **seq write 1M** | on 539/600/603 vs off 437/527/584 → medians **600 vs 527 = 1.14×** | off 638 / on 623 / on 625 / off 604 → on 624 vs off 621 = **1.005×** | **Par-to-modestly-ahead, order-sensitive** — the forward bracket's +14 % did not reproduce reversed (store warmer by then); combined medians 603 vs 584 = 1.03×. No row shows a gate LOSS |
| **rand-4k read 10 s** | on 117.9k/110.6k vs off 113.6k/106.9k | — | **Parity+ (1.03–1.04×)** — the gate never touches sub-threshold ops; the small-op lane is untouched |

**Reading**: the D14 field truth reproduces in miniature — the kernel zc lane dominates large reads (the ring's 2-copy consume path is the whole gap), large writes sit at par on this venue (the ring's 1-copy write path with placed severs is already competitive locally; the FIELD's 35.3 GB/s zc write lane vs 28 GB/s-class ring ceilings is where the write half of the gate earns its keep), and the IOPS lane is untouched. The gate's job here is **routing + attribution**, and both are exact.

## 4. What the field confirmation needs (the exact rows)

Venue: squeeze-test (or any fabric client), armed mount (`SQUEEZEFS_FUSE_ZC=1`), `--interception`, daemon+shim same commit. All rows `LD_PRELOAD=libsqueezefs_il.so`, dynamic fio; stats deltas from `.stats` before/after each row; **engagement gates make any silent-passthrough or wrong-lane cell exit nonzero**:

1. **Threshold publish**: with one bound fd held, `ipc_lane_gate_threshold_bytes > 0` (gauge is live-session-scoped).
2. **1 MiB lane row (both engines)**: `fio --ioengine=libaio --bs=1M --iodepth=8 --numjobs=4 --rw=read|write --direct=1` and the psync twin — gate: `Δipc_lane_gate_kernel_bytes ≥ 0.99 × row_bytes` AND `Δipc_bytes_{in,out} ≤ 0.01 × row_bytes`.
3. **rand-4k ring row**: `--ioengine=psync --bs=4k --rw=randread` (and the direct-drive libaio twin ≤ slab) — gate: `Δipc_ops_read ≥ 0.99 × row_ios` AND `Δipc_lane_gate_kernel_routes ≈ 0`.
4. **Offsetful sticky**: `dd bs=1M` — `Δroutes ≡ count`, `Δipc_ops_write = 0`.
5. **A/B at the field shapes** (medians of 3, A-B-B-A): 1 MiB seq read AND write, gate ON vs `SQUEEZEFS_IL_KERNEL_LANE_MIN=0` — the read row should reproduce the 49.2-vs-28 GB/s class gap as a ≥ 1.5× ON ratio; the write row is the one this venue could not decide (expect ON ≈ 35.3 GB/s-class vs the ring's fleet ceiling); rand-4k ON/OFF parity band ±5 %.
6. **Sustained row**: one ≥ 60 s `--time_based` 1 MiB read row ON, throughput flat across the window (the burst rows above are label-only for speed claims).
7. Tripwires along every row: `ipc_sessions_poisoned = 0`, `ipc_descriptor_rejects = 0`, `transport_lease_overlong` flat, and the write rows carry their amplification columns per the standing instrument.

## 5. Standing residuals

* The probe reads LLC-class bandwidth on big-L3 parts (§1.2) — conservative toward the ring; if a field row shows the threshold materially misplaced, the probe's buffer should grow past LLC (a one-line change, re-derivation automatic).
* The sticky latch never un-latches short of a fresh bind — a workload that writes one 1 MiB burst then does rand-4k on the SAME fd via read()/write() pays kernel-lane for the small tail (counted, visible in the ledger). Positional ops on the same fd are unaffected (per-op). If a counted field row prices un-latching in, hysteresis (latch-down after N sub-threshold ops + re-arm cost) is the shaped follow-on.
* aio gate-OFF keeps the structural > slab kernel reroute uncounted (it is not a gate decision); a libaio "all-ring" A/B arm is therefore only meaningful at ≤ slab shapes — stated here so nobody reads a libaio 1 MiB OFF row as a ring row.
