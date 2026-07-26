# 2026-07-26 — IPC handoff economy: ring handoffs ride the kernel lane's per-core handler threads

Branch `perf/ipc-handoff-economy` (off dev `4af3d57`). Commits: red
`417efd0` (the adjudicated spin-window-default pin), green `c4f9db5`
(default 0 in code), `b96e176` (the handoff-venue fix — this program's
change). The pre-agreed charter: eliminate the ~130 µs/op
async-handoff term named OPEN in `.benchmarks/2026-07-26-ipc-reap-economy.md`
§7, choosing among (1) batched handoffs, (2) service-thread
direct-drive device reads, (3) a dedicated handoff lane — profile
decides.

## 1. Root cause (profile evidence — the design decision)

perf (dwarf, 12 s window) on the dev-pair daemon under the cold ddt il
t16qd16 row (baseline row for the capture: 290.9k IOPS, avg 879 µs):

- Per-op **CPU** across the tokio workers was only ~14 µs — the
  ~100–130 µs/op gap vs the kernel lane at equal offered load is
  **queueing, not compute**. No symbol family (moka ~7 %,
  `enqueue_read` 2.3 %, `schedule_task` 1.1 %, custody/routing ~4 %)
  came close to accounting for it as cycles.
- The structural asymmetry: the kernel transport's dispatch task
  spawns every handler onto fuse3's **`TPC_SCHEDULER`** — per-core
  `std::thread` + current-thread-runtime + `LocalSet` lanes fed by
  unbounded channels. The IPC service thread is a **foreign OS thread**
  to tokio: `Handle::spawn` from it lands every miss on the
  multi-thread runtime's **global inject queue**, which saturated
  workers poll only between local-queue batches. Every ring miss paid
  that inject-queue wait retail; kernel-lane ops never do.
- Secondary finding, fixed first: `1508ea9` ("spin window defaults to
  0") changed only the DOC comment — the code still carried the sweep
  side's `unwrap_or(30)`. Under the divergent 30 µs ambient default the
  4 active service threads burned ~2.5 % CPU each in
  `clock_gettime` spin (13.6 % of daemon cycles in `__vdso_clock_gettime`
  across the capture). Red pin `spin_window_default_is_zero` →
  default 0 landed in code (`c4f9db5`); explicit settings unchanged.

**Design picked: direction 3 in its cheapest shape** — not a new
executor: route `DataPlaneSink` handoffs (read misses AND writes)
through `handoff_spawn` → `fuse3::raw::tpc_spawn`, the SAME per-core
handler lanes kernel-lane requests run on. This deletes the
foreign-thread inject term (unbounded FIFO submission, no
inject-queue starvation), makes the §5.5 "the handoff path IS the
existing handler" argument exact (same body, same locks, same
executor class), and needs no new machinery — so directions 1
(batching) and 2 (direct-drive) were **not needed**: the term they
would amortize is gone at the venue level. Direct-drive remains the
recorded follow-on only if a future program wants to delete the
remaining ~14 µs/op handler-lane CPU (§7). The now-unused
`DataPlaneSink.runtime` handle was deleted (no dead code).

Pins (weakening-verified — a `Handle::current().spawn` handoff fails
both): `handoff_runs_on_a_current_thread_handler_lane`,
`handoff_spawns_from_a_plain_os_thread_without_ambient_runtime`
(src/ipc_service.rs `handoff_venue_tests`). No park/wake protocol was
touched (the futex ladders are byte-identical), so no new loom model;
the existing 42 ran green in the gate.

## 2. Substrate (labeled; same boot as the reap-economy note §2 rig)

32-CPU box, 109 GiB RAM, kernel 7.1.4-1-cachyos. The fabric-latency
substrate was STILL UP from the reap campaign (verified knob-by-knob):
configfs null_blk `sqzlat_oss0` (36 GiB, memory-backed,
`completion_nsec=235000`, `irqmode=2` timer, bs 4096, 8 squeues, hw QD
128) → nvmet-loop (port 52126, resv_enable=1) → `/dev/nvme1n1` (data);
`sqzlat_mds0` (3 GiB) → `/dev/nvme2n1` (meta). Raw ceiling this
session: fio 3.42 libaio 16×QD16 on /dev/nvme1n1 = **474k** (reap note
same boot: 486k — same-session A/B pairs only below).

Filesystem: cache-less format (`sqmeta:///dev/nvme2n1
sqdata:///dev/nvme1n1`), 4 MiB blocks, 1 GiB mem cache. Mount:
`--daemon --allow-other --interception -o direct_device_true`
(queues=32 depth=32, ipc_service_threads=8). Dataset 16 × 1.5 GiB
(cold-dominated by design; ddt il rows are 100 % handoffs).
Instruments: **elbencho 3.1-10 (dynamic)** threaded rows, **fio 3.42**
16-forked-process libaio fleet rows. Engagement printed per row
(§3 rule 4): `ipc_ops_read` delta == row ops == `ranged_reads` delta —
**exact on every il row of both sides**; kernel rows verified
`ipc_d=0`. Baseline side = `4af3d57` daemon+shim pair (KD-7), ship
side = `b96e176` pair; same volume + dataset, fresh mount per side,
sides back-to-back in one session window (baseline 11:19, ship 11:26
local).

## 3. A/B (medians of 3, 15 s rows unless noted; per-run values in parentheses)

| Row | baseline `4af3d57` | ship `b96e176` | Δ |
|---|---|---|---|
| kernel t16 qd16 (context) | 316.1k (305/316/323) | 312.7k (323/304/313) | — |
| **il t16 qd16, s4 (bar shape 1)** | 275.5k (277/275/253), avg 922 µs | **334.8k** (334/336/335), avg 763 µs | **+21.5 %; 0.87× → 1.07× kernel — bar MET** |
| kernel t32 qd32 (context) | 290.1k (290/289/298) | 307.3k (293/320/307) | — |
| **il t32 qd32, s4 (bar shape 2)** | 303.6k (319/304/297), avg 3.37 ms | **347.7k** (349/343/348), avg 2.94 ms | **+14.5 %; 1.05× → 1.13× kernel — bar MET** |
| 16-process fio libaio qd16 (one session/proc — the multi-process probe) | 273k (279/273/264) vs kernel 307k | **329k** (329/327/331) vs kernel 317k | **+20.5 %; 0.89× → 1.04× kernel** |
| il t1 qd1 RTT (10 s rows) | 3,582 IOPS / avg 278–280 µs | 3,309 IOPS / avg 299–302 µs | −7.6 % — attributed (below); **still beats kernel qd1 (3,139 / 317–319 µs) on every run of every side** |
| il sync t32 qd1 (psync-class protected row, 10 s) | 90.1k (87.7/90.1/90.5) | **102.4k** (102.6/102.4/102.0) | **+13.6 %** (sync-lane serves are handoffs on a ddt mount — they ride the same venue) |
| sessions sweep s1/s2/s4/s8 (10 s, one run each, recorded) | 306/269/261/258k | **333/337/331/328k** | the session-spread degradation is GONE (s8: +27 %) |
| warm fast path (default mount, 4×200 MiB warm, sync t8, ×3) | 20.6k median (25.0/20.5/20.6), mix ~65.7k fast / ~139.4k handoffs per 204.8k | 21.3k median (21.5/21.3/20.5), mix ~65.8k / ~139.0k | unregressed, serve mix identical |

**qd1 attribution (single runs, recorded)**: the −7.6 % il qd1 delta
is idle-lane cpuidle wakeup, not protocol cost — with deep C-states
capped (`cpupower idle-set -D 2`) the ship pair reads **3,598 IOPS /
avg 277 µs** (== baseline's 277–280 µs band) and kernel qd1 also
improves (3,392 / 294 µs). At sparse arrival the round-robin lane pick
lands on a lane sleeping in deep C-state; at any depth ≥ 2 the lanes
stay warm (see the t16/t32 rows). Not chased further: the shim's qd1
per-op still beats the kernel path by ~15–40 µs on every run.

**Tripwires (ship mount, post-side snapshot)**:
`write_path_seed_read_bytes` 0, `patch_edge_rmw_reads` 0,
`ipc_descriptor_rejects` 0, `ipc_sessions_poisoned` 0,
`read_admission_governor_denials` 0, `read_admission_wasted_bytes` 0,
`fsck_findings` 0; `ranged_reads` == `read_device_true_reads` ==
`ipc_ops_read` == 64,019,635 (100 % handoffs by ddt policy —
`ipc_fast_path_serves` 0 on the cold rows, by design).

## 4. What the venue moved (ship-side profile, recorded)

perf on the ship daemon under the same row (327.0k during the
capture): handler bodies (`enqueue_read::{{closure}}`, moka probes,
custody fingerprints, routing) now sample on the TPC lane threads
(they inherit the `tokio-rt-worker` comm — the `TPC_SCHEDULER` Lazy is
first touched from a dispatch task, so its `std::thread::spawn`
children inherit that comm; lane attribution verified by symbol
content, service threads at 2.8 % each). Remaining per-op CPU terms
for a future economy pass, honest sizes: `OpProf::begin` 5.4 % of
worker cycles (clock reads — armed even with `SQUEEZEFS_OP_PROFILE`
off), moka attr/metadata probes ~9 %, hashing ~3.4 %,
`CachedMetadata::clone` 1.6 %.

## 5. Umount promptness (constraint row)

- The reap note's **5 s reaper-join hang stays fixed** (its cargo pin
  `shutdown_is_prompt_with_the_idle_reaper_armed` is in the gate).
- The reap note's separate OPEN item — the **~11 s il-only SIGTERM
  teardown term OUTSIDE the ipc host** — reproduces IDENTICALLY on
  both sides here (baseline 11.06 s; ship 11.06/11.06/11.06 s ×3,
  counted): parity kept, still not this branch's surface, still owed
  its own red-first loop. (One ship umount measured 0.15 s when the
  client exited earlier relative to teardown — the term is
  client-lifecycle-dependent; the 11 s bound is the honest number.)

## 6. Gates

- **Full cargo gate** on `b96e176`: `cargo fmt --check` clean,
  `cargo clippy --all-targets --all-features -- -D warnings` clean,
  `cargo doc --no-deps` clean, bench smoke green, loom 42/42 green
  (`tests/run_loom.sh`), `cargo test --all-features --
  --test-threads=1` green across the suite (the two read-path
  failures the reap note inherited at `26fe3f9` were fixed on dev by
  `6ad1a63` before this branch point — clean slate here).
- **Preload gate**: leg 1 (unprivileged) PASSED; leg 2 (root) PASSED —
  mount parity + engagement, dup/close_range/lseek rows, notify
  delivery, foreign-netns rendezvous, fio/elbencho/libaio verify,
  **kill-9 soak ×5 and fork-kill-parent soak — zero residue**.
- KD-7 lockstep: daemon+shim measured as same-commit pairs both sides;
  no wire change was needed (the venue is daemon-internal).

## 7. Residuals (recorded, not chased)

- **The raw-ceiling gap**: il t16qd16 347.7k (t32qd32) vs the 474k fio
  raw ceiling — the remaining term is per-op handler CPU (~14 µs/op:
  moka probes, custody fingerprints, OpProf clock reads, key
  formatting) plus the 235 µs device floor at these depths. The §7
  reap-note fallback shapes (batched handoff drain, service-thread
  direct-drive into `NvmeBlockDev` with a synchronous policy prelude)
  remain the pre-agreed follow-on if a program wants that CPU back;
  the queueing term they were sized against no longer exists.
- **qd1 idle-lane C-state wakeup** (−7.6 % at qd1 only, attributed in
  §3): a latency-sensitive fleet can cap cpuidle; a code-side lever
  (biasing the lane pick toward recently-active lanes) is a
  micro-optimization left unclaimed.
- The TPC lane threads inherit the `tokio-rt-worker` comm (cosmetic,
  makes per-thread attribution need symbol inspection — a
  `named-thread` nicety for a future fuse3 touch).

## 8. Substrate teardown

As the miss-path note §8: disconnect the two nvmet-loop subsystems,
unlink port 52126, rmdir nvmet objects, power-off + rmdir the
`sqzlat_*` configfs null_blk items. Left up while the branch is under
review (RAM-backed, reboot-ephemeral).
