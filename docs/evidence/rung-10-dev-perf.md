# Rung 10 — the dev container's performance gap: decomposition, bounds, fixes

The owner's measurement (2026-10-13, `scripts/dev_mount.sh`): the same 4-CPU / 10 GiB container on the
venue kernel, a 2-MDT + 2-OST set on RAW virtio-blk partitions, put SqueezeFS "worlds apart" from ext4
on the same virtual disks — 3.2 k creates/s against 21.8 k, 16.7 k random 4 KiB O_DIRECT reads/s
against 291 k. Decision 0003's performance ruling makes each gap a defect. This note decomposes every
row from the daemon's own instruments and the kernel's FUSE tracepoints before fixing anything, states
the bound each op should meet (decision 0003 §AA), fixes the largest gaps red-first, re-measures the
whole table and names what remains and why.

## 1. The venue and the instruments

- **Container** `sqz-perf` (`SQZ_DEV_NAME=sqz-perf SQZ_DEV_VOLUME=squeezefs-zig-perf
  scripts/dev_mount.sh`): the owner's recipe in a separate container, kernel `7.2.6-sqz` (d42a24dd),
  `nproc` 5, 10 GiB. Targets: `mdt0/mdt1` = `/dev/vdd1`, `/dev/vde1` (368 MiB), `ost0/ost1` =
  `/dev/vdf1`, `/dev/vdg1` (1.1 GiB), each a raw partition of its own virtio-blk disk. References in
  the same container: ext4 on `/build` (`/dev/vdc`, virtio-blk), tmpfs on `/dev/shm`.
- **Commands** (`scripts/perf/table.sh <dir> <label>`, the owner's shapes; `scripts/perf/reset.sh
  sym2|sym1|single` reformats the raw targets into a posture and mounts it): `mpirun -np 4 mdtest -n
  2000 -d <dir>/md`; `fio --rw=randread --bs=4k --size=256m --direct=1 --ioengine=io_uring
  --iodepth=32` (one job, 15 s time-based, after a 1 MiB sequential lay-down); the same with
  `--rw=randwrite`; `mpirun -np 4 ior -a POSIX -t 1m -b 64m -F`.
- **The device's own capability** (fio on the raw OST partition, read-only, while mounted): random
  4 KiB O_DIRECT reads 19.7 k IOPS at QD1 (49.8 µs), 207 k IOPS at QD32 (154 µs); ext4's file at QD1
  18.1 k (54.2 µs). The data set spans two such devices.
- **Daemon instruments**: `.stats` snapshots around every phase (`fuse_op_latency_ns[class]`,
  `SQUEEZEFS_OP_PROFILE` phases, `meta_conveyor_*`, `journal_lane_*`, `xv_cross_owner_phase_*`,
  `read_*`, `transport_*`, `daemon_cpu_ns_by_class`); `perf record -g` (linux-tools 6.8, frame
  pointers intact in the ReleaseSafe binary); the kernel's `fuse:fuse_request_send/end` tracepoints for
  requests per user op and their kernel-side round trips (`scripts/perf/opcount.sh`, per mdtest phase
  `scripts/perf/phases.sh`); `.stats` deltas by `scripts/perf/sdiff.py`, inclusive profiles from
  `perf script` by `scripts/perf/incl.py`.
- **The FUSE floor on this kernel**: a negative LOOKUP from one thread — 13–14 µs kernel send → end,
  of which the daemon's handler is 5.3 µs: ≈ 8 µs of transport (kernel queueing, the ring's command
  completion, the worker → handler-lane handoff and back, the commit).

## 2. The decomposition (before any fix; binary `b652702`)

| op (owner's row) | requests per user op (tracepoints) | where the time goes |
|---|---|---|
| file create (mdtest, 4 ranks, ONE shared directory) | `CREATE` + an async `RELEASE` (`FLUSH` elided by `FOPEN_NOFLUSH`); a `LOOKUP` first for `O_CREAT` without `O_EXCL` | The kernel serializes creates in one directory on its `i_rwsem`, so the rate is `1 / CREATE round trip`: mean 254 µs (p50 355 µs). Daemon create latency 266 µs mean. **Half the creates were cross-member intents**: `xv_cross_owner_intents_minted` 20,003 over 40 k namespace ops, each `intent_barrier` 191 µs + `local_steps` 56 µs + `retire` 166 µs (the second barrier) = 424 µs — two device flushes per create. A single-member create: 82 µs in the daemon (`handler_to_backend` 11.4, `backend` 66.8 = queue wait 10.6 + pass 13.0 (leaf locks 8.3) + the journal write landing ≈ 38 + the ack hop; `backend_to_reply` 4.1) + ≈ 28 µs of transport. Daemon CPU 112 µs per create. |
| directory create / rename / remove | `MKDIR` / `RENAME` / `UNLINK` (+ `LOOKUP` when the dentry expired) | The same intent split: mkdir 298 µs, unlink 241 µs mean in the daemon. |
| 1-MDT controls, same disks | — | `--single-writer` 1 MDT: 5,973 creates/s; symmetric 1 MDT: 5,628 — the symmetric plane's own cost is ≈ 6 %; the 2-MDT set's 3.0 k is the intents. |
| file stat | one `LOOKUP` per stat whose dentry expired | Entry and attr TTLs are 1 s (charter §4 default); the create phase lasts longer than 1 s, so most dentries had expired: one round trip each, 55 µs under 4 ranks. A cached dentry costs no request. |
| file read (mdtest: open, 0 bytes, close) | `LOOKUP` (expired dentry) + `OPEN` + async `RELEASE` | ≈ 60 µs per request under 4 ranks. |
| file remove | `LOOKUP` + `UNLINK` | `UNLINK` = one commit (intent split as above). |
| random 4 KiB O_DIRECT read, QD32 | one `READ` | **The mount ran `buf_ring`, not zero-copy** (§3.1): every read was handed to a handler lane and DMA'd through the nvme lane, 101 µs daemon serve, `transport_buffer_mode` 1. On a zero-copy mount (58 k IOPS): ONE drain worker (`sqz-fuse-ur1`, the fio thread's CPU — the kernel queues by submitting CPU) at 87 % of a core, 17 µs of worker CPU per read: `SafeAllocator.alloc` 38 % (bucket `munmap` 7 %), the layout record resolved FIVE times per read (`layoutRecord` 72 %: probe file + binding, direct-leg file + binding, post-DMA recheck), each a chain RANGE with copies, + two inode lookups — 7 node-cache probes per read; `compiler_rt.fastMemset` 10 % (ReleaseSafe's fill of `undefined` scratch); the flight table's 1024-slot locked scan 2.6 %. Kernel round trip QD1 91 µs against the device's 50 µs. |
| random 4 KiB O_DIRECT write, QD32 | one `WRITE` | 29 k IOPS on the `buf_ring` mount, 49 k on zero-copy (W1 patches). |
| IOR 1 MiB write / read | — | 2.97 GB/s / 26.6 GB/s (reads from the page cache). |

## 3. The fixes

Each lands with a red test that fails at the old value (verified on the old code before the fix).

### 3.1 The first mount after every boot lost zero-copy (`48074d0`)

**Root cause.** `Session.resolveMode` gated the zc verdict on the INIT request's `FUSE_OVER_IO_URING`
offer — but the kernel makes that offer only when `fuse.enable_uring` already reads Y, and the
parameter defaults to N after every boot; the daemon writes Y itself (charter §3) AFTER the probes.
So the first mount after each boot armed its ring (since 7.2.4 the ring is created at reply time) on
the slower `buf_ring` rung with retention absent — the owner's dev container, the venue after every
recreate — while every later mount got zero-copy (reproduced: container restart → first mount
`buf_ring`, second `zero_copy`). Fix: write the parameter before the probes; the verdict is
`probes.ringAtReply(offered, enable_uring after the write)`.
**Red test**: `probes` "the ring exists at reply time iff enable_uring reads Y after the daemon's
write…" (`ringAtReply(0, true)` was false). **Verified live**: restart → first mount `zero_copy
(retention present)`.
**Effect**: rand-4k 17.4 k → 58 k IOPS, rand-4k write 29 k → 49 k on that mount.

### 3.2 The product ran on std's SafeAllocator (`d4403b9`)

**Root cause.** Zig 0.17's `start.zig` hands `main` a `SafeAllocator` in Debug AND ReleaseSafe
(`use_safe_allocator`); ReleaseSafe is the product's default build (efdc88b). Every allocation wrote a
footer with a captured stack trace and buckets were mapped and unmapped as they filled and drained.
Fix: `mem/process_allocator.select` — every release mode takes `std.heap.smp_allocator`, Debug keeps
std's choice (its leak report). **Red test**: `process_allocator` "a release product never runs on
std's SafeAllocator…". **Effect**: the 38 % `SafeAllocator` share of the worker vanishes from the
profile, and with it every op's allocations get cheaper (drain-worker CPU per zero-copy rand-4k read 17.2 → 10.4 µs
  from this fix alone).

### 3.3 Cross-member mints (`0b75664`; decision 0003 §AA.1, 0002 row 518)

**Root cause.** `MetaSet.allocateIno` picked the member first by the round-robin (§5.4 "mint spread")
and only then applied the slot plane's parent affinity (§5.5) — which the plane grants only to a
parent on the SAME member. On a 2-member set half of every directory's children landed on the other
member, and every create, mkdir, unlink and rename of such a child was a cross-owner intent paying
two device barriers. The reference does the same (`pick_mint_volume` before `lease_mint_slot`, "no
affinity across volumes"). Fix: `Plane.homesChildren` — a child of a directory this mount leases
outside the native slot mints on the directory's member (affinity, then that member's rotor); the
root's children (the native slot) and a foreign directory's children keep the round-robin, so a set
still spreads by subtree. **Red test**: `meta_set` "a child of a directory this mount leases outside
the native slot mints on the directory's member…" (100 creates under such directories: 0 intents;
was: children on the other member). **Effect**: 0 intents over mdtest (was 20,003); daemon create
266 → 76 µs, mkdir 298 → 79 µs, unlink 241 → 83 µs; creates 3.0 k → 6.1 k/s.

### 3.4 The read path resolved its layout five times per read (`635f71a`, `f4b662a`, `e931327`)

**Root cause.** The transport calls `probe_read` then `direct_leg` for the same request; the leg
re-read the file info and the binding the probe had just read, and the post-DMA recheck read the
binding a third time — 5 layout-record resolutions and 2 inode lookups per cold read, each layout
read a chain RANGE (a list, a copy of every key and value, then the record copied again). Fixes:
(a) the probe hands its resolution of the request to the leg (`warm_ladder.Resolved`, taken by the
leg, cleared by every probe; the post-DMA recheck still re-reads the binding — charter §6.3's
validation after the bytes are in hand); (b) `DataPlane.layoutRecord` answers from ONE point lookup
of the chain's first slot, the range only on a 56-bit hash collision; (c) a snapshot lookup whose
newest record is a Put or a Delete folds without the gather scratch (13 % of the worker: ReleaseSafe
fills its 64 `undefined` refs on every from-scratch fold); (d) the flight table's empty-table miss
answers from an open count, not the locked 1024-slot scan. **Red test**: `warm_ladder` "a cold
direct-leg read resolves its file ONCE and its binding TWICE…" (old code: 2 file reads, 3 binding
reads per read; also pins that a binding moved between the probe and the bytes still fails the
recheck). **Effect**: rand-4k 96 k → 158–166 k IOPS (fio mean latency 332 → 193 µs).

### 3.5 Format's summary named the wrong version (`7f3e2fc`)

`squeezefs format` printed "SqueezeFS format v4" and the too-small refusals said "format v4" since the
v5 act (0003 §M): both now print `superblock.version_v5`. Red test: `format` "the summary and the
too-small refusal name the format version the superblock writes…".

## 4. The table, before and after (same container, same commands)

Before = `b652702` (the owner's build) on its first mount after the container's boot (`buf_ring`, the
owner's posture); after = `7f51e85`. Two runs of each where the host was shared with other agents'
gates: the spread is shown (the host is 11 cores shared by the venue, the owner's container and this
one; `uptime` 3.6–4.6 during the after runs).

| workload | owner (`sqz-dev`) | before (`sqz-perf`) | after | ext4 (`/build`) | tmpfs |
|---|---|---|---|---|---|
| mdtest file create (4 ranks × 2000, one dir) | 3,163/s | 2,994/s | 5,958–6,084/s | 26,682/s | 545,663/s |
| mdtest dir create | 3,175/s | 2,759/s | 6,016–6,119/s | 7,312/s | 331,484/s |
| mdtest file stat | 128,349/s | 114,966/s | 188,595–245,302/s | 5,041,231/s | 5,034,423/s |
| mdtest file read (open/close) | 76,350/s | 71,717/s | 57,755–77,876/s | 5,895,016/s | 4,725,311/s |
| mdtest file remove | 3,126/s | 3,401/s | 6,173–6,287/s | 134,673/s | 1,296,990/s |
| fio randread 4k O_DIRECT QD32 io_uring | 16.7 k | 17.4 k | 157.6–168.4 k | 334.0 k | — |
| fio randwrite 4k O_DIRECT QD32 | 94.5 k | 29.0 k | 98.8–102.3 k | 257.4 k | — |
| IOR 4 × 1 MiB write / read | 2.04 / 47.8 GB/s | 2.97 / 26.6 GB/s | 2.46–2.86 / 18.2–32.3 GB/s | — | — |

Daemon-side, the same mdtest: cross-owner intents 20,003 → 0; daemon create latency 266 → 76 µs,
mkdir 298 → 79 µs, unlink 241 → 83 µs, rename 273 → 78 µs. The random read: node-cache probes per
direct-leg read 7.00 → 4.00, drain-worker CPU per read 17.2 → 5.4 µs, kernel READ round trip at
QD32 424 → ≈ 190 µs.

Postures on the same disks (before → after): 1-MDT `--single-writer` 5,973 creates/s, 1-MDT
symmetric 5,628 → the 2-MDT set now matches them (6.0–6.1 k): the plane's own cost is inside the
noise and the cross-member price is gone from a directory's children.

## 5. The remaining gap per op, and its cause

| op | after | ext4 | gap | cause (measured) |
|---|---|---|---|---|
| file create, one shared directory | 6.0 k/s | 26.7 k/s | 4.4× | The kernel serializes the 4 ranks on the directory's `i_rwsem`: the rate is `1 / CREATE round trip` ≈ 1 / 110 µs (kernel send → end; daemon 76 µs). §AA.2's bound `R + W` ≈ 52 µs (≤ 19 k/s): the journal write landing the charter owes (≈ 38 µs) plus one bare round trip. The rest is software: four cross-thread wakes per create (worker → handler lane → journal lane → lane → worker; `wakeFromForeign` is 12–28 % of each of the three threads' cycles) and ≈ 100 µs of daemon CPU per create across the threads (the journal lane's per-tx undo capture and leaf locks, the handler's gather opt-in xattr read, the mint's held-slot census). ext4 journals in RAM and acks before any device write. |
| dir create | 6.0 k/s | 7.3 k/s | 1.2× | As above; ext4's own mkdir is slower than its create. |
| file stat | 0.19–0.25 M/s | 5.0 M/s | 20–27× | A stat of a dentry under 1 s old is served by the kernel (no request); the create phase outlives the 1 s entry TTL, so the older dentries cost one LOOKUP each (13 µs at QD1, ≈ 55 µs under 4 ranks: the rank, the drain worker and the handler lane share the rank's CPU — qid = CPU). §AA.6: the TTL is the staleness bound for remote namespace changes no recall names; raising it needs entry-grained invalidation. A faster create phase also shrinks the expired share. |
| file read (open + close) | 58–78 k/s | 5.9 M/s | 75–100× | `LOOKUP` (expired dentry) + `OPEN` per file, two round trips (`RELEASE` is asynchronous, `FLUSH` elided). ext4 and tmpfs open in the dcache. |
| file remove | 6.2 k/s | 135 k/s | 22× | `LOOKUP` + `UNLINK`; the unlink is a commit (the journal write landing) and the shared directory serializes it like the create. |
| random 4 KiB read, QD32, one job | 158–168 k | 334 k | 2.0–2.1× | ONE drain worker carries the job (the kernel picks the queue by the submitting CPU): at 5.4 µs of worker CPU per read it is ≈ 90 % busy; the remaining worker time is the kernel's `io_uring_enter` work (35 %) and the four node-cache resolutions (≈ 35 %). The devices themselves give 207 k each at QD32. |
| random 4 KiB write, QD32 | 99–102 k | 257 k | 2.5× | Not decomposed in this pass (W1 patches; the same single-worker shape). |

## 6. Gates

At `7f51e85` before landing: Linux venue gate 43/46 steps — every step green except the literal
rail (two allowlist rows shifted by the read path's and the probes' edits; rebased by
`scripts/rails-literal-rebase`, then `zig build rails` 16/16 on both venues): unit 2,570 pass / 69
skip, smoke 87 pass / 2 skip, bench-smoke 41, shim-smoke 40, nvmet-smoke 53, suites-smoke (pjdfstest
141 files) PASS; macOS gate unit 2,086 pass / 27 skip, the same rail fixed.
