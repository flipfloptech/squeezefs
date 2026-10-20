# Rung 10 — the client bypass as a measured row: `libsqueezefs_il.so` against ext4, xfs and FUSE on the same in-memory substrate (2026-10-20)

The owner's bar: in memory, the fastest of {ext4, xfs, SqueezeFS} or on par on every row, and the
LD_PRELOAD interposer (charter §7; the DAOS libioil shape — data I/O from the client's address space
to the daemon over shared memory, never through kernel FUSE) is the path meant to clear it. Same
RAM-backed NVMe namespaces (brd through nvme-loop), the workstation (32 threads, `7.2.8-9.30-omarchy-sqz`),
the venue held, every row started at Tctl ≤ 70 °C, 20 s rows. fio's io_uring engine never enters
libc, so the bypass runs on **psync** — `pread`/`pwrite`, depth 1, `min(qd × nj, 32)` jobs, one
384 MiB file / one fd / one shim session per job — and ext4, xfs and the FUSE mount on the SAME shapes
(`matrix.sh MX_ENGINE=psync`, `MX_PRELOAD`; `baseline.sh ram-{ext4,xfs,sqz}` and `ram-sqz-il` =
`-o interception` + the preload). Dist build `28779da` (this series before its rebase onto `dev`; the run records under `/var/lib/sqz-perf/runs` carry that identity); rows under `/build/perf/mx/{ram-ext4-rpsync,
ram-xfs-rpsync,ram-sqz-rpsync,ram-sqz-il-rfinal2}`. Every bypass row printed its engagement line:
100.0 % of the fio ops through the ring, FUSE READ/WRITE flat (3–10 ops), 0 rejects, 0 poisons.

| row (psync; 32 jobs unless QD1) | ext4 | xfs | FUSE | **bypass** | ÷ ext4 | ÷ FUSE |
|---|---|---|---|---|---|---|
| 4k randread QD1 (IOPS, p50 µs) | 268,178 (3.8) | 236,330 (4.0) | 115,878 (8.2) | **144,230** (7.0) | 54 % | 1.24× |
| 4k randread ×32 | 3,053,210 (8.8) | 2,847,022 (9.4) | 1,188,485 (20.9) | **1,224,870** (23.2) | 40 % | 1.03× |
| 4k randwrite QD1 | 217,747 (4.6) | 215,840 (4.0) | 84,502 (11.1) | **110,633** (8.2) | 51 % | 1.31× |
| 4k randwrite ×32 | 3,027,972 (9.0) | 2,791,974 (9.7) | 789,400 (33.0) | **1,168,926** (24.7) | 39 % | 1.48× |
| 64k randread ×32 (MiB/s) | 79,223 | 78,724 | 43,400 | **36,902** | 47 % | 0.85× |
| 64k randwrite ×32 | 76,207 | 76,203 | 30,076 | **31,201** | 41 % | 1.04× |
| 1 MiB seq read ×32 | 98,338 | 97,153 | 88,571 | 88,406 † | 90 % | — |
| 1 MiB seq write ×32 | 85,076 | 84,865 | 53,165 | 51,287 † | 60 % | — |

† kernel-routed by design: the hybrid lane gate (`SQUEEZEFS_IL_KERNEL_LANE_MIN`, derived to the 64 KiB
slab here) sends larger ops to FUSE — FUSE rows under the shim. Host CPU per 4k op at ×32: ext4 7.8 µs
(all inside the caller's syscall), FUSE 18.0 (daemon 13.0), bypass 18.4 (daemon 8.5 on 12 `sqz-ipc-svc`
at 90 % busy; client 9.9 — the shim's spin and park). Tctl start→end over the rows: ext4 49–62→57–87,
xfs 55–65→56–92, FUSE 51–62→59–85, bypass 53–69→58–83.

**What the shim serves**: data only (`il_shim.md` §1) — `pread/pwrite`, `read/write` (the offset
mirror), the vector forms, `lseek`, libaio, on BOUND regular fds. `open/close/stat/fsync` and every
namespace call go to the kernel: mdtest-class rows are FUSE rows and stay with the create-path work.

## Levers (A/B on the 32-job 4k rows; the control arm is the knob's 0 on the final build)

| lever | control → shipped |
|---|---|
| **Direct-drive READ** (`SQUEEZEFS_IL_DIRECT_READ`, `cd36fcf`): a cold ring read the direct device leg admits (`WarmLadder.resolveDirectLeg`: passthrough, 4 KiB-aligned, single block, no custody) is ONE READ SQE on the owner's dd shard DMA'd into the arena, the leg's recheck at the CQE. Rung 7 handed every miss to a FUSE lane (a fiber spawn, a lane wake, the full handler) | reads ×32 381 k → 1.22 M, QD1 42 k → 144 k (control: `sqz-fuse-ln` 7.9 + `sqz-nvme` 1.6 cores, p99 522 µs) |
| **The ring park** (`SQUEEZEFS_IPC_DD_RING_PARK`, `17e0ca1`): an owner with ops in flight waits for its CQEs AND its doorbells in one `io_uring_enter` (a `FUTEX_WAITV` SQE that persists until it fires; the reaper backs off to a half-CQ/100 ms backstop) and never spins in userspace with ops in flight. Rung 7 left every CQE to the reaper thread (a wake + a context switch per op) and, under `COOP_TASKRUN`, stranded the owner's own CQEs behind its spin (578 k, p99 144 µs; the IPI arm `COOP=0` gave 1.03 M — the lever stays at 1 now that the stranding is unreachable) | reads 1.05 → 1.22 M, writes 896 k → 1.17 M, QD1 reads 112 → 144 k; `sqz-ipc-dd` 2.9 → 0.01 cores (with the spin half below) |
| **The spin half** (`5b2a139`, its window `93b762c`): the ring park polls CQ ∪ TASKRUN flag ∪ doorbells for 2 × the lane's park EWMA inside a 50 µs RAM-class rail (`SpinGovernor.ringWindowUs`) — the doorbell governor's busy ceiling disengaged spinning exactly in the loaded regime (an explicit 20 µs measured +5 %/+9 % there) | over the ring park alone: QD1 reads 122 → 144 k; ×32 reads 1.19 → 1.22 M, writes 1.14 → 1.17 M |
| Measured, not taken: `SQUEEZEFS_IL_SPINS=2048/16384` (the client spins through the op) QD1 +30 % but ×32 −13 %/−34 %, p99 346/667 µs — 32 spinning clients starve 12 svc threads on 32 cores; `SQUEEZEFS_IPC_SERVICE_THREADS=20` ×32 −6 %/−12 % — W12 stands | — |

## Where it caps — the verdict

The bypass is **ahead of FUSE on every data row it serves** (+3 % to +48 %; 64k reads the exception:
the FUSE worker's zero-copy leg lands in the app's pages, the ring pays a 64 KiB arena copy) and **at
39–54 % of ext4/xfs** on 4k random I/O, 41–47 % at 64k. Parity was not reached, and the gap is
structural: an in-kernel filesystem serves a 4 KiB O_DIRECT read in ONE syscall on the caller's core
(ext4 7.8 µs CPU, 8.8 µs latency at ×32, 3.8 at QD1); the bypass's op is client → svc thread (a wake
or a spin) → the svc's own `io_uring_enter` (the same nvme-loop submit ext4 pays, ≈ 2 µs) → the CQE
back to the svc → `futex_wake` of the client (≈ 1 µs on the svc, 3–5 µs of scheduler latency on a
parked client) → a 4 KiB arena copy. Per op at ×32 (profile `ram-sqz-il-ril-prof`): 8.5 µs of svc CPU
— the probe's and recheck's KV lookups (`metaFileAt`/`metaBinding`/`inodeSizeLocal`) ≈ 3, enter+submit
≈ 2, the client wake ≈ 1, pass/complete ≈ 1.5 — on 12 lanes is a ceiling near 1.4 M ops/s, and the
23 µs latency chain's floor is two wakes above ext4's one; at QD1 a cross-thread hop never reaches a
3.8 µs syscall.

For parity a next wave needs: (1) **the client issuing the device I/O itself** — a layout lease
(ino, block → volume, offset, incarnation) resolved once per session, revalidated by a generation word,
the device fd over `SCM_RIGHTS`; it deletes both hops and the daemon's syscall but hands the client
whole-device rights, so a trusted-client mode, not the default boundary (DAOS's libioil does not do it:
its engine does the I/O on dedicated cores); (2) the metadata engine's per-op cost — ≈ 3 µs of KV
lookups per read here and in the FUSE worker's 13 µs — an inode-layout cache with a per-inode generation
for the recheck, owned by the data/create-path work; (3) dedicated svc cores so the svc spin never
competes with the client fleet. Tests: `ipc`/`warm_ladder`/`knobs` 171 + `shim`/`sdk` 101 unit, the dd
lane's three rows (DMA into the slot, the recheck with bytes in hand, the fallback and the EIO law; the
ring park's doorbell arm on a stuck read, the owner-side reap, the epoch re-arm, both spin absorbs, the
reaper's backstop), the interception e2e (the cold aligned ring read is a dd read), `shim-smoke` rows
1–4c, `sdk-smoke`, rails 29/29 with the census 62/62; the reads closure gained the dd term.
