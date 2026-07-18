# L3 transport-economy program — closing report (2026-07-18)

**Charter**: `.benchmarks/2026-07-15-iops-parity-decomposition.md` lever L3 —
eliminate the ~8.3 syscalls/op residual transport fat measured post-L1
(splice-bounce block on payload replies, 1.67 eventfd writes/op, the 200 ms
`pop_timeout` timer park, statx 0.34/op). Expected payoff +10–20 % at the
post-L1 transport ceiling. Branch `perf/l3-transport-economy` off dev
`3974cd9`.

## Provenance

| | |
|---|---|
| Box | AMD RYZEN AI MAX+ PRO 395, 32 possible CPUs, **capped 3.5 GHz** (performance governor), kernel `7.1.3-2-cachyos`, 109 GiB RAM |
| Substrate | `tests/dev_substrate.sh` virtual NVMe (nvmet-loop): 4 mds = memory null_blk 1 GiB (+256 MiB WB cache), 4 oss = zram-zstd 8 GiB → `/dev/nvme{1..8}n1`. Raw-substrate rand-4k ceiling on the oss namespaces: **1.98 M IOPS** (same elbencho line, devices read directly) |
| Binaries | BEFORE dev `3974cd9` md5 `d33c8bcb6c61666d605a0e9d63875ca7`; AFTER branch `4a25d85` md5 `50111143a7375ffc1977c3fef201d2d7` — both `cargo build --release`, same day, same substrate/session |
| Instrument | **elbencho 3.1-9** (page-aligns its O_DIRECT buffers — AGENTS.md instrument-alignment lesson; every row below is elbencho). Timed rows `taskset -c 0-15`; daemon unpinned, no memory cage |
| Mount | `squeezefs mount <4 mds> /tmp/l3mnt --daemon --allow-other --read-mem-cache-size 1G --write-mem-cache-size 1G` (+ `-o direct_device_true` on the device-true family only). Format: house shape, staging declared at format on tmpfs (`/tmp/l3staging`). Transport geometry (default L1 policy, verified on `.stats`): queues 32, Q_DEPTH 32, max_background 256, payload arena 1 GiB |
| Workload | charter line: `elbencho -r --rand -t 16 -b 4k --iodepth 16 --direct <16 files>` over 16×1 GiB (`-w -t 16 -s 1g -b 1m --direct`); warm family over 16×48 MiB (768 MiB, inside the 1 G RAM tier, two warm passes) |
| Divisor method | straced windows state ops = Δ`fuse_over_uring_requests` (`.stats` sampled at window edges); IOPS rows are elbencho "LAST DONE" steady state; amplification = daemon `/proc/<pid>/io` read_bytes ÷ (ops × 4096) |
| Rails | `/mnt/squeezefs`, `~/tmp/nvme`, the user's juicefs/redis containers and zram0 untouched; only devsub objects + `/tmp/l3*` used; substrate torn down at session end |

## What landed (levers → commits)

| Lever | Change | Commits |
|---|---|---|
| **A — splice block** | `splice_reply()`/`ThreadPipe`/thread-local pipe **deleted**; `reply_fuse` routes replies through `write_vectored` only (over-uring COMMIT_AND_FETCH for ring uniques, classical vectored for INIT/sideband/handoff); `FuseConnection::{splice_read,splice_write}` atomics deleted; INIT never echoes `FUSE_SPLICE_{READ,WRITE,MOVE}` (capability nothing implements); abi constants deleted with their last users. Contract pinned by `init_negotiation_tests` (negotiation extracted pure) | `74cd123` (RED) → `2711779` (+ `87cca13` EOL-only normalization of tokio.rs, isolated on purpose) |
| **C — session pull park** | `InboundQueue::pop_timeout` (200 ms `tokio::time::timeout` per pull — per-pull timer registration + time-driver park; shutdown relied on a no-op notify) → pure event-driven `pop` parking on the channel + a pool-level `tokio::sync::Notify`; `shutdown()` stores `active=false` then `notify_waiters()` (enable-then-check closes the race); session caller fails loud on `None`, the `qid ≥ nqueues` 5 Hz sleep branch deleted | `5202ee6` (RED — shutdown-to-None measured 180 ms) → `02eeb88` |
| **B — eventfd wake coalescing** | `wake_core::WakeCoalescer` per queue: producers publish → `arm()` → write the eventfd only on `true`; worker pass = drain-to-EAGAIN → `disarm()` → scans. Wired into `submit_reply` and `EntPayloadLease::drop` (shared via the arena); shutdown wakes stay unconditional. Loom-verified (see below). Stats: `transport_wake_{writes,elided}` on the `.stats` metrics surface (test-pinned) | `55f952b` (core + models + RED surface test) → `6b1c265` (wiring) |
| **statx residual (attributed + fixed)** | `strace -k`: the statx/op is `BackendRouter::is_backend_healthy` → `Path::exists()` **on the data device node per ranged read** (`routing.rs:404`). Node probe now TTL-cached on the device (`NODE_PROBE_TTL_MS` = 1 s, shared across clones); the explicit `unhealthy_backends` mark stays authoritative/instant; I/O path still fails loud on a vanished node inside the window | `625f877` (RED) → `4a25d85` |

## Splice-bounce proof (baseline, live trace)

Raw strace during the charter row (armed over-uring session): **every** splice
reply attempt bounces and pays the teardown —

```
vmsplice(538, [{iov_len=4096}], 1, SPLICE_F_NONBLOCK|SPLICE_F_GIFT) = 4096
splice(535, NULL, 427, NULL, 4112, SPLICE_F_MOVE|SPLICE_F_NONBLOCK) = -1 ENOENT
pipe2([533, 537], O_NONBLOCK|O_CLOEXEC) = 0        ← pipe recreated per reply
```

`strace -c` over the same window: splice **31,630 calls, 31,630 errors —
100 % error rate** (the kernel holds ring uniques in the uring ent, not
`fpq->processing`; a classical reply write for them is ENOENT by
construction). Post-fix trace: **0** splice/vmsplice/pipe2 calls in a 4 s
saturated window.

## Before/after — straced per-op syscall table

`strace -c -f -p <daemon>`, 6 s window inside a saturated charter row,
device-true mount. Divisors: **42,264 ops** (before), **138,768 ops** (after)
— the ptrace tax itself shrank with the syscall count, hence the larger
after-window divisor. Same instrument, same substrate, same day.

| syscall | before /op | after /op | note |
|---|---:|---:|---|
| splice | 0.748 (100 % err) | **0** | lever A (deleted) |
| vmsplice | 0.748 | **0** | lever A |
| pipe2 | 0.748 | **0** | lever A |
| fcntl | 0.748 | **0** | lever A (F_SETPIPE_SZ) |
| close | 1.499 | 0.001 | lever A (2×close per pipe teardown) |
| statx | 0.757 | 0.003 | node-probe TTL cache |
| write | 3.045 | 1.872 | lever B (+A: pipe header writes gone); counter-true elision below |
| read | 2.180 | 0.566 | fewer wake passes ⇒ fewer eventfd drains (B/C) |
| futex | 0.361 | 0.051 | fewer cross-thread wakeups |
| sched_yield | 0.806 | 0.002 | — |
| io_uring_enter | 2.490 | 0.490 | fewer wakes ⇒ deeper per-pass batching (M3 machinery untouched) |
| epoll_wait | 1.537 | 1.513 | tokio worker park — the dominant residual (see below) |
| **total** | **≈ 15.7/op** | **≈ 4.5/op** | **−71 %** |

Counter-true wake economy (not strace-distorted): after a full charter run,
`transport_wake_writes` 5,639,149 vs `transport_wakes_elided` 6,945,070 over
12,584,230 replies — **0.45 eventfd writes per reply, 55 % elided**
(structurally 1.0/reply before; charter target ≲ 0.5 under saturated load —
met).

## Before/after — IOPS rows (no strace)

n=3 per family, medians bold. Amplification 1.000× on every device-true row
(daemon `/proc/io` ÷ ops×4096).

| Row family | before (3974cd9) | after (4a25d85) | Δ |
|---|---|---|---|
| **Device-true** (`-o direct_device_true`, 16 GiB set) | 447,867 / **445,657** / 442,531 | 603,989 / **604,313** / 600,280 | **+35.6 %** |
| **Warm/hybrid** (default posture, 768 MiB tier-resident set) | 469,340* / **466,594** / 474,717 | 633,873 / **644,726** / 649,958 | **+38.2 %** |

*first-listed numbers are FIRST-DONE elbencho column; medians use LAST DONE
(steady state) for every row.

Charter G-L3-2 target was +10–20 % at the ceiling: **exceeded** (+35.6 %
device-true, warm row *improved* not regressed — lever A touches exactly the
tier-serve payload replies). Context: this substrate's device latency is
µs-class (zram), so per-op transport cost dominates harder than on the
decomposition's real NVMe; the same levers on that box should land inside or
above the predicted band but were not re-measured there.

## Gates

| Gate | Verdict |
|---|---|
| G-L3-1 syscall economy | **PASS** — splice/pipe2/vmsplice/fcntl block = 0 post-arm (strace, saturated window); eventfd writes 1.0/reply → 0.45/reply counter-true (elision 55 %); per-pull timer park gone (`pop` is pure event-driven; futex 0.36→0.05, sched_yield 0.81→0.002, eventfd `read` 2.18→0.57). Honest note: `epoll_wait`/op is ≈ flat (1.54→1.51) — it is tokio worker parking per request batch, not the retired time-driver churn; recorded as the dominant residual |
| G-L3-2 charter IOPS | **PASS** — +35.6 % device-true (445.7k → 604.3k median), warm/hybrid +38.2 % (466.6k → 644.7k), amp 1.000× everywhere |
| G-L3-3 verification | **PASS** — full cargo gate green; loom 29/29 (incl. the two new `wake_coalescer_*` models); LTP 174/0; FSTESTS_QUICK 17/20 with the 3 failures **A/B-proven pre-existing on dev `3974cd9`** (identical set on the baseline binary, same box/day — details below) |
| G-L3-4 zero-copy invariants | **PASS** — zero-copy/write-through/lease suites green in the full run; `write_path_seed_read_bytes` and `patch_edge_rmw_reads` pins unchanged (their tests are part of the gate); the over-uring reply dest (`UringBufOwner`) and §5.4 lease-severance boundary untouched by review + tests |

### Loom (lever B protocol)

`tests/run_loom.sh` (LOOM_MAX_PREEMPTIONS=3): `wake_coalescer_publication_never_stranded`
+ `wake_coalescer_lease_drop_parked_commit_never_stranded` (composed with
`lease_core` exactly as the drop site ships) — both green. **Weakening
evidence** (both verified failing during development): worker pass permuted
to scan-before-disarm strands a publication ("consumed 1 of 2" park);
`disarm` as a plain SeqCst store (no RMW happens-before carrier) strands both
models. The shipped drain→disarm→scan order + RMW disarm passes.

### Full cargo gate (branch `4a25d85`)

`cargo clippy --all-targets --all-features -- -D warnings` clean;
`cargo fmt --check` clean; `cargo test --all-features -- --test-threads=1`
**all green** (946 s, includes the zero-copy / write-through / lease /
writeback suites — G-L3-4); `cargo doc --no-deps` (the 2 known pre-existing
fabric.rs link warnings only); `cargo bench --benches -- --test` green.
`tests/run_loom.sh`: **29/29 models** (27 existing + 2 new `wake_coalescer_*`).
The fork's own suite (`crates/fuse3`, production features): 33/33.

### External suites (per-PR data-path tier)

- `FSTESTS_QUICK=1 sudo tests/run_fstests.sh`: 20 ran, 2 notrun
  (generic/009, generic/316 — env), **17 pass / 3 fail: generic/003
  (atime/ctime semantics), generic/213 (fallocate ENOSPC), generic/464
  (open/close-race EIO)**. Attribution A/B: the **identical** 3-failure set
  reproduces on a clean dev `3974cd9` worktree build
  (md5 `c2aa83c940b7e71385ced8633e69505e`) on the same box/day —
  **pre-existing on the base commit, zero L3 delta**. Not fixed here
  (out of charter scope); left for their own fix loop.
- `sudo tests/run_ltp_syscalls.sh`: **PASS 174 / FAIL 0 / BROKEN 0 /
  SKIPPED 9** on the branch binary.

## statx attribution (charter question)

`strace -f -e trace=statx -k`: **daemon-side**, `statx(AT_FDCWD,
"/dev/nvmeXnY", …)` from `std::fs → Path::exists ←
BackendRouter::is_backend_healthy ← read_block_range ←
get_block_range_for_index ← read_file_range_zero_copy` — one statx of the
data device node per ranged read (0.76/op at this workload's read mix).
Fixed with the TTL-cached node probe (above); 0.003/op after (residual =
`get_backend_health` status paths + mount-time probes).

## Honest anomalies & residuals

1. **`epoll_wait` ≈ 1.5/op remains** — tokio multi-thread runtime parking
   (session/reply/handler task wakeups), not the retired per-pull timer. A
   deeper fix means restructuring task-per-request wake topology (L4
   territory), out of L3 scope.
2. **`write` 1.87/op after** vs 0.45 counter-true wake writes: the remainder
   is tokio cross-thread unpark writes (runtime-internal eventfds), not the
   queue wake path.
3. **Straced windows distort**: the ptrace tax squeezes IOPS ~20–60× and
   shifts batching; the table is for per-op *ratios* under identical
   instrumentation, the IOPS rows are unstraced.
4. **Baseline splice rate here was 0.75/op** (≈ every READ reply) vs the
   decomposition's "~⅓ of replies" — that row was measured at stock QD4
   posture and a different op mix; both are the same 100 %-error bounce.
5. **This substrate is zram-fast** (raw ceiling 1.98 M): transport economy
   is a larger share of per-op cost than on production NVMe; gains on real
   devices will skew toward the latency floor, not above it.
6. Baseline mount also served a first (non-headline) hybrid row at 2.7×
   amplification before the device-true remount — kept in the raw results
   dir, not a headline row (posture mismatch, documented for completeness).
7. **FSTESTS_QUICK carries 3 pre-existing failures on this box**
   (generic/003 atime semantics, generic/213 fallocate-ENOSPC, generic/464
   open/close-race EIO — daemon-side "did not settle after 8 binding
   rebinds" on the 464 leg). A/B-identical on dev `3974cd9`; they predate
   this branch and need their own tests-first fix loop.

## Files

Raw rows/straces/io snapshots: `/tmp/l3bench/results/` (session-ephemeral;
all headline numbers reproduced in this report). Daemon logs:
`/tmp/l3bench/daemon*.log`.
