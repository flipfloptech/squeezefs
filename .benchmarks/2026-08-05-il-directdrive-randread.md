# 2026-08-05 — il rand-4k residual: direct-drive rings/reapers shard by the derived drain width (D12 randread-shim board item)

**Branch:** `perf/il-directdrive-randread` (worktree off `integrate/zcrx-wave`
`5ae36dd2`). **SHAs:** red tests `87c5d69d`, fix `770a2cc6`.
**Venue:** local tcp devsub (`SQZ_DEVSUB_TRANSPORT=tcp`, nvmet-tcp on
127.0.0.1 — 4× null_blk mds + 4× zram-8G oss), 25-CPU single-node box.
**Instrument:** fio libaio `--direct=1 --rw=randread --bs=4k`, 32 files ×
128 MiB striped prefill, interception + `-o direct_device_true --allow-other`
mounts, per-row `.stats` deltas (`/tmp/sqz-dd-ab/row.sh`). netem is
unavailable on this kernel (no `sch_netem`), so the field's 235 µs RTT was
substituted per the charter by SCALING IN-FLIGHT until the single-reaper
ceiling showed (32×16 → 32×32).

## The residual (standing since the transport-queueing campaign)

Field cluster: fio libaio rand-4k **32×qd8** via the shim (io_setup/io_submit/
io_getevents interposers → the DIALED P1/P1.5 direct-drive engine) ran
**~208k IOPS FLAT** vs the kernel FUSE path's **273–280k** (−26 %) at 235 µs
fabric RTT / 256 in-flight — while the same shape locally ran +44 % il.
Lever 1 (same-lane READ dispatch, `b6d7413b`) improved the KERNEL path +12 %
and never touched this engine.

## Structural map (where the ceiling lives)

Client: fio job → interposed `io_submit` → `screen_iocb`/`classify_iocb` →
session ring (fd-sharded, `il_sessions_default` sessions/process) →
doorbell. The client reap at qd8 > `REAP_EVENT_PARK_MAX`(2) rides the 50 µs
bounded-sleep batch regime, so daemon completion wakes are elided
(`ipc_cqe_wake_elided`) — the reap-economy machinery (event-driven sparse
reap, cqe doorbell) covers the CLIENT loop and the sync lane only. **None of
it covers the daemon's direct-drive completion side.**

Daemon (pre-fix): every governed O_DIRECT miss from EVERY service thread
(ceiling `il_sessions_default(cpus)` — 8 on the 32-CPU field box) submitted
into **ONE shared 512-entry io_uring behind ONE state mutex**, and **ONE
unpinned `sqz-ipc-dd` reaper** serialized every CQE-side serve: slab-take
under the same mutex → `ipc_direct_revalidate` (moka metadata get, block-map
probe, custody-epoch read, overlay/staging screens) → ~12 counter updates →
`SlotCompletion::complete`. `IpcDirectSnapshot`'s own doc already recorded
"the reaper thread is the deep-qd throughput governor — 100 %-CPU saturation
at t32qd32", and the module doc recorded per-thread rings as the fallback
"if SQ contention ever shows on a profile". 208k flat = ~4.8 µs of
serialized reaper work per op; the kernel path spreads the same reply work
over per-CPU queues, which is why lever 1 widened the gap.

## Local repro (the ceiling shown by in-flight scaling)

Quiet-window bracket (base = `5ae36dd2` pair, engagement EXACT on every row:
`dd_serves == ios`, fallbacks 0, `serves+fallbacks == submits`):

| row | in-flight | IOPS | clat | note |
|---|---|---|---|---|
| base il 32×8 | 256 | 357,342 | 716 µs | kern 32×8 ref 323,561 (+10 % il — the "local hides it" shape) |
| base il 32×16 | 512 | 420,019 | 1218 µs | |
| base il 32×32 | 1024 | **433,999 / 433,069** | **2359/2364 µs** | **FLAT vs 512 while clat doubles = the serialization ceiling**; `sqz-ipc-dd` 61 % CPU, 6 svc threads 50 % each |
| raw fabric ceiling | — | 654–699k | — | fio libaio direct on the oss namespaces |
| **fix il 32×32** | 1024 | **653,298** | 1567 µs | **+50.7 %** — at the raw ceiling; 6 `sqz-ipc-ddN` reapers 23–38 % CPU, `ipc_direct_shards` = 6 |

Counted alternating A-B rounds (base↔fix remounts per round so box drift —
Tctl 62–99 °C, foreign builds — hits both sides; medians of 3 INCLUDING the
polluted round 3):

| shape | base median | fix median | Δ | per-round pairs |
|---|---|---|---|---|
| il 32×8 (the field shape) | 246,702 | 304,258 | **+23 %** | R1 +51 %, R2 +35 %, R3 polluted† |
| il 32×32 (ceiling-exposed) | 296,425 | 530,848 | **+79 %** | R1 +81 %, R2 +85 %, R3 polluted† |
| kern 32×8 (untouched control) | 303,066/196,151 | 289,606/267,198/188,123 | box-noise band | |

† Round 3 discarded with attribution (rate-gathering label per the
multi-run discipline): a concurrent agent session externally umounted the
base mount mid-round ("FUSE filesystem was unmounted externally" in the
daemon log) and ran rustc storms (load 16, Tctl 95 °C+). Even counting it,
the medians above stand.

## The fix (770a2cc6) — by derivation, never a constant

- `ipc_direct::dd_shards_from(env, cpus)` == `il_sessions_default(cpus)`
  (the ONE `cpus/4` drain-parallelism slope — the SAME function that
  ceilings the service threads, so lanes:service-threads are 1:1 by
  construction; the ingest-economy paired-derivation law, drift-is-red).
  `SQUEEZEFS_IPC_DD_SHARDS` (registry entry) is the override/measurement
  lever, clamp 1..=64; cpus = `process_parallelism()` (the Hang-1
  pinned-first-toucher discipline — the engine spawns lazily from a
  possibly NUMA-pinned service thread).
- One LANE per service thread: `set_service_lane(owner)` in
  `IpcHost::service_loop`; foreign threads take a dense round-robin
  fallback. Each shard: own 512-entry ring, own in-flight slab + mutex, own
  `sqz-ipc-ddN` reaper spawned on the lane's FIRST governed submit
  (spawn-on-bind) and pinned per `numa_core::owner_nodes(width)` (gated —
  `SQUEEZEFS_NUMA=0`/single-node no-op).
- Unchanged per shard: prelude, 795 revalidation, fallback ladder,
  accounting, submit-batch flush economy (flush walks all shards),
  MEM-7c stall law, shutdown drain-then-join. Severance law untouched
  (write path not in scope); KD-7 untouched; libaio semantics untouched
  (interposer suites green).
- Engagement gauge `ipc_direct_shards` (stats inode): LIVE shard reapers.
  1-forever-under-load = the single-reaper field structure is back.

## Red evidence

`87c5d69d` lands the contract red (E0432/E0603/E0609 — no `dd_shards_from`,
no `ipc_direct_shards`): derivation tie, two-lanes⇒two-shards engagement,
multi-shard shutdown promptness + `serves+fallbacks == submits` closure,
stats export. Green at `770a2cc6`; the two new async tests ran ×10 green.

## Suites (targeted, D12 posture)

- `ipc_direct_drive_tests` 14/14 (×10 on the new async pair);
  `ipc_direct::tests` unit pair (lane fallback/pin).
- ipc/preload root suites green: ipc_host (37), ipc_op_economy,
  ipc_hold_probe, ipc_inval_venue, ipc_admission_convoy, ingest_economy,
  preload_session (24), preload_parity (14), preload_lifecycle,
  preload_authn, read_saturation, unsafe_contract.
- `squeezefs-preload` crate: aio_core (12), aio_glue (17), core, linked_mode,
  offset_mirror, refusal_reason — all green.
- `env_knob_convention_tests` (20) + `derivation_sweep_tests` (21) green
  (the new knob is registered).
- `tests/run_preload_gate.sh` leg 1 PASSED (Issue-4 guard, passthrough
  battery, aio lifecycle ×3 orderings, direct-link battery). Note: the gate
  resolves artifacts at `<tree>/target/`, so a `CARGO_TARGET_DIR` worktree
  needs a `target` symlink.
- clippy `--all-targets --all-features` AND `--all-targets` (shipped
  config): clean, `-D warnings`. `cargo fmt --check` clean.
  Markdown link check: 188 files, 0 broken.

## Field confirmation row (for the cluster)

Same instrument as the ingress ladder, il side vs kernel side, engagement
exact — deploy the wave binary + same-commit shim pair, then:

```
# il (the residual row): expect ipc_ops_read ≈ dd_serves ≈ row ios,
# ipc_direct_shards == 8 (32-CPU box), and IOPS ≥ the kernel row
LD_PRELOAD=<pair>/libsqueezefs_il.so fio --name=r --directory=$MNT \
  --filename_format='sqzfio.$jobnum.0' --rw=randread --bs=4k --size=1g \
  --numjobs=32 --iodepth=8 --ioengine=libaio --direct=1 --time_based \
  --runtime=30 --group_reporting
# kernel reference: the same line without LD_PRELOAD (273–280k standing)
```

Verdict gate (board item): shim ≥ kernel on rand-4k. Local evidence says
the single-reaper term is gone (+51 % at the ceiling-exposed shape, base
flat-at-434k vs fix at-the-raw-ceiling 653k); the 235 µs-RTT field row is
the confirmation this note's venue cannot mint.
