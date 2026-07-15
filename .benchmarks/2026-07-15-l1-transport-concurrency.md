# L1 — transport in-flight concurrency defaults (2026-07-15)

**Charter**: IOPS-parity lever board **L1** (`.benchmarks/2026-07-15-iops-parity-decomposition.md`):
make the measured 316k class the DEFAULT mount behavior — no env knobs — with a
payload-buffer sizing policy, without regressing small-box/metadata posture.
Branch `perf/transport-inflight-defaults` off `c56ec6a`.

## The shipped default policy

Resolved ONCE per session (`TransportGeometry::resolve`, vendored fuse3), before the
INIT reply serializes — the limits the kernel learns always describe the rings that
register:

| Parameter | Default | Override |
|---|---|---|
| Queues | kernel **possible CPUs** (unchanged; readiness requires all queues) | `SQUEEZEFS_FUSE_OVER_IO_URING_QUEUES` (testing only, clamp 1..512) |
| Per-queue depth | **desired 32** (the measured-best), degraded to the buffer cap: `clamp(cap / (queues × payload_sz), 4, 32)` — floor 4 = the pre-L1 shipped posture | `SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH` wins **verbatim** (clamp 1..32, bypasses the cap) |
| Payload-buffer cap | **`min(mem_budget / 8, 2 GiB)`** — budget resolved eagerly at mount with the sampler's §5.7 order (flag → env → cgroup×0.8 → 70 % RAM); fuse3-local fallback `min(RAM/8, 2 GiB)` | `--mem-budget` / `SQUEEZEFS_MEM_BUDGET_MB` (via the budget) |
| INIT `max_background` | **`clamp(queues × depth, 64, 256)`** — floor 64 (the old dead-letter option's intent), ceiling 256 (the measured class) | `-o max_background=N` (now LIVE — pre-L1 it was filtered and the INIT reply hardcoded **12**) |
| INIT `congestion_threshold` | ¾ of `max_background` (kernel's own ratio) | `-o congestion_threshold=N` (live) |

Worked geometries (payload_sz = 1 MiB at max_write 1 MiB): 32-CPU big-RAM box →
depth 32, arena 1 GiB, mb 256/192 (the 316k config); 8 G cage (budget 6.4 G) →
cap 819 MiB → depth 25, mb 256; 1 GiB budget → depth 4 (floor) = yesterday's
128 MiB arena, mb 128; 256-CPU box → cap 2 GiB → depth 8, mb 256.

Observability: arm line prints the geometry; stats inode gains
`transport_{queues,q_depth,payload_buffer_bytes,max_background}`; the arena rides the
R5 budget as the non-sheddable `transport_payload_buffers` component (anon,
pressure-carrying, weight 0). `squeezefs tune` now writes 256/192 (never lowers a new
mount back to 64/48).

## Provenance

| | |
|---|---|
| Tree | `perf/transport-inflight-defaults` @ `77210d2` (tests `618f40f`, impl `98d0673`), release build |
| Box | the phase-1 box (AMD RYZEN AI MAX+ PRO 395, 32 possible / 25 online CPUs @ 3.5 GHz cap, 109 GiB RAM, PC SN8000S 2TB, kernel 7.1.3-2-cachyos) |
| Rails | `taskset -c 0-15` rows; daemons `systemd-run --user --scope` cages (`MemoryMax` per row); quiet-gated (no rustc/cargo; Tctl 44–49 °C all rows); kills by PID; `/mnt/squeezefs` + `~/tmp/nvme` untouched |
| Harness | phase-1 sandbox `~/tmp/iops_parity_3456308/` (same volumes + 16×1 GiB dataset) + its `row.sh`; new `l1_remount.sh` (parameterized cage/env/`-o`); fresh-volume A/B sandbox for the write-seq isolation (removed at session end) |
| Baseline control | `c56ec6a` release built in a throwaway worktree (md5 differs from L1 binary) for fstests + teardown attribution |

## TDD evidence

RED (`618f40f`): 4 mount-level contract tests (`tests/transport_concurrency_tests.rs`)
failed on dev — `transport_*` geometry fields absent, INIT stuck at 12/9, depth flat 4.
GREEN (`98d0673`): all 4 pass (real unprivileged mounts asserting stats inode AND
fusectl); +6 fuse3 pure-core `TransportGeometry::plan` unit tests (ample budget ⇒
32/256/192; exact degradation incl. floor-at-4, cap-0, huge-CPU; env verbatim; queue
clamps; mb/ct overrides incl. 0-ignored; payload floor) and the
`transport_buffer_cap` formula test in `tests/mem_budget_tests.rs`. Full cargo gate
green at every commit (clippy `-D warnings`, fmt, `test --all-features
--test-threads=1`, doc, bench smoke); zero_copy + multi_queue + write_through +
data_path_correctness + writeback(+fencing) suites green by name.

## Acceptance rows (user's exact line: `elbencho -r --rand -t 16 -b 4k --iodepth 16 --direct` on the 16×1 GiB dataset, +`--timelimit`)

| Row | Config (all same binary) | Geometry / INIT | IOPS | device | daemon |
|---|---|---|---:|---|---|
| **A. Default, 8 G cage** (phase-1 shape) | **no env, no `-o`** | q32 **d25** (819 MiB cap) mb **256**/192 | **300,374** / 305,922 (×2) | 1.00× amp, 298k r/s | 7.1 cores |
| **B. Default, 16 G cage** | no env | q32 **d32** (1 GiB arena) mb 256/192 | **320,852** | 1.00×, 312k r/s | 7.5 cores |
| C. Stock-equivalent | `Q_DEPTH=4` env + `-o max_background=12,congestion_threshold=9` | q32 d4 mb 12/9 | 26,418 | 1.00× | 0.9 cores |
| D. Small-box sim | `SQUEEZEFS_MEM_BUDGET_MB=1024` | q32 **d4 (floor)** 128 MiB arena, mb 128/96 | 48,334 (functional row) | 1.00× | 1.4 cores |

**Default lands the 300k class with zero knobs** (A/B); the same binary reproduces the
stock ceiling when forced to the old limits (C — also proves the `-o` overrides land in
the kernel: fusectl 12/9); the small-RAM policy degrades to exactly the pre-L1
footprint and stays functional (D — floor beats stock because mb floors at 64+).
Default vs stock-equivalent on identical binary/box/dataset: **11.4×**.

**RSS / gauge**: gauge = component = arena bytes exactly (A: 838,860,800 B; B:
1,073,741,824 B; D: 134,217,728 B). Daemon RSS: A 5.51 GiB post-mount → 5.94 GiB
post-row; B 7.35 GiB post-row (16 G cage); C (stock arena) 5.63 GiB post-row ⇒ the L1
default's RSS delta in the 8 G cage ≈ the arena delta (+672 MiB), inside the cage. The
~5.5 GiB base is the sandbox's staged-dataset recovery mmap (pre-existing, config-
independent). In the 8 G cage the budget authority sat **Red by design** (base RSS ≈
92 % of the 6.4 G budget) with zero OOM/backstops across all rows; 16 G cage: Green,
`red_events=0`.

## Seq / 6-pass regression check (`squeezefs bench -t 16 -s 256m`, suite = write seq 1m / read seq 1m / read rand 4k / write rand 4k / stat / del, all O_DIRECT)

Default vs stock-equivalent, interleaved (8 G and 16 G cages, plus a fresh-volume
sandbox for isolation; MiB/s or ops/s):

| Pass | Default (mb256) | Stock-equiv (mb12) | Verdict |
|---|---|---|---|
| Read seq 1m | 2022 / 1797 / 3091 | 1733 / 1537 / 2818 | **default wins every pair (+10–17 %)** |
| Read rand 4k | 183.0k / 171.6k / 151.8k | 171.5k / 157.2k / 147.8k | default ≥ stock |
| Write rand 4k | 362 ops/s | 346 ops/s | equal (device-latency-bound) |
| Stat / Del | 119.7k / 7.5k–19.2k | 123.0k / 18.8k–30.8k | 16-op samples — noise (del varies 2.6× within one config) |
| **Write seq 1m** | fresh-volume: **1202–1304** | fresh-volume: **1609–1652** | **−25 % at this exact shape — see finding** |

### FIND-L1-A — 16-writer O_DIRECT streaming convoy (daemon-side, exposed not caused)

The only regressing row: **≥13 concurrent O_DIRECT sequential writers** (t16 × 1 MiB).
Isolation (fresh volume, interleaved, n≥3 each): the 2×2 mb/ct matrix pins the lever —
`{mb12: 1623/1624/1652/1631, mb256: 1217/1222/1212/1243}` with **ct 9 vs 192
indistinguishable**; Q_DEPTH exonerated (QD4+mb256 = 1680 ≈ QD32+mb256); mb128 ≈ mb256
(both admit all 16 writers — the boundary is `mb < writers`); at t8 (< 12 writers) the
gap collapses into noise. Mechanism: pre-L1, the kernel's mb=12 *throttled* the
product's own write-path convoying out of sight — at 16 admitted writers,
`block_lock_wait` shows a 241-sample ≤16 ms tail per pass while `uring_queue_full=0`,
`bg_spawn_rejected=0`, conveyor healthy (write-through 1024/1024 blocks). It is a
daemon concurrency defect of the striped write path, not a transport law, and it is
**not depth-related**. Trade accepted for the default: −25 % on that one shape vs
+7–12× on the charter workload, seq reads +10–15 %, buffered/metadata rows unchanged;
per-mount escape documented (`-o max_background=12` restores the old throttle,
runtime-writable via fusectl). **Follow-up recorded on the lever board as L3-adjacent:
fix the >12-writer convoy (block-lock shard audit + striped-flush admission), then
re-run this table — the mb256 write row should join the mb12 band.**

### Pre-existing defects surfaced during acceptance (attributed, NOT L1)

1. **Teardown SIGBUS**: every daemon that ran the bench suite then unmounted dumped
   SIGBUS (5× L1 binary, both geometries incl. stock-equivalent). Reproduced
   byte-for-byte with the **baseline `c56ec6a` binary** (same bench + `squeezefs
   umount`, coredump 15:54:54, binary md5 distinct). Pre-existing teardown defect
   (mmap-region access after staged-segment teardown class); rand-only daemons exit
   clean. Needs its own charter.
2. **generic/074**: fails identically (fstest.2 transient-zeros, 512 B blocks ×3
   children) on the L1 binary AND the baseline `c56ec6a` binary — a dev-baseline
   deviation from the QUICK table's deterministic-PASS row, unrelated to transport
   (26 s vs 40 s run shapes both fail; signature = the historical staged-identity
   family). Filed as a standing dev regression to re-bisect; **not** an L1 effect.

## fstests (root, singles)

- `generic/013`: **pass** (5 s) on the L1 binary.
- `generic/074`: fail — **identical failure on baseline `c56ec6a`** (see above);
  deviation pre-dates this branch.

## Files / knobs / stats

Vendored fuse3: `TransportGeometry` (+6 unit tests), `try_start(fd, geom)`,
`MountOptions::{max_background, congestion_threshold, transport_buffer_cap_bytes}`,
INIT reply wired to the plan, `over_uring_geometry()`, module-doc knob text fixed
(L5b: queues default was documented "min(nproc, 8)" vs actual possible-CPUs),
`DEFAULT_MAX_BACKGROUND`/`DEFAULT_CONGESTION_THRESHOLD` constants deleted (dead).
SqueezeFS: `mem_budget::{resolve_budget_now, transport_buffer_cap,
TRANSPORT_BUFFER_CAP_CEILING}`, `-o` override parsing, cap plumbed into MountOptions,
4 stats fields, `transport_payload_buffers` budget component, tune 256/192, dead
`max_background=64,congestion_threshold=48` tokens removed from the default option
string. Docs: AGENTS knob table (+ stale-queue-text fix), README transport section,
QUICKSTART tune text + fusectl runtime trick, design-read-path OQ E supersession note.
**L5a (`FOPEN_KEEP_CACHE`) deliberately not taken**: semantics change (page-cache
retention across opens vs attr-TTL staleness windows), own test sweep — stays on the
lever board.

## Artifacts

`~/tmp/iops_parity_3456308/` (preserved): `l1_remount.sh`, `results/l1[a-n]_*` rows
(elbencho outputs, `.stats` before/after, `/proc` io/cpu snaps, disk deltas, env
honesty lines), `logs/l1*.log` arm lines per geometry. Baseline worktree and the
fresh-volume A/B sandbox were removed after attribution (coredumps remain in
`coredumpctl` under the paths recorded above).
