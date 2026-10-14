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
| file create (mdtest, 4 ranks, ONE shared directory) | a negative `LOOKUP` + `CREATE` + an async `RELEASE` (`FLUSH` elided by `FOPEN_NOFLUSH`) | The kernel serializes creates in one directory on its `i_rwsem` and runs the `LOOKUP` and the `CREATE` both under it (`atomic_open`), so the rate is `1 / (LOOKUP + CREATE round trips)`; the `CREATE` round trip: mean 254 µs (p50 355 µs). Daemon create latency 266 µs mean. **Half the creates were cross-member intents**: `xv_cross_owner_intents_minted` 20,003 over 40 k namespace ops, each `intent_barrier` 191 µs + `local_steps` 56 µs + `retire` 166 µs (the second barrier) = 424 µs — two device flushes per create. A single-member create: 82 µs in the daemon (`handler_to_backend` 11.4, `backend` 66.8 = queue wait 10.6 + pass 13.0 (leaf locks 8.3) + the journal write landing ≈ 38 + the ack hop; `backend_to_reply` 4.1) + ≈ 28 µs of transport. Daemon CPU 112 µs per create. |
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

### 3.6 The create and lookup paths' CPU (`7e1eff7`, `81ff635`, `2826904`, `3261ea4`)

Profiled under the mdtest create phase (`perf record -g`, all daemon threads): the create handler's
cycles went to the commit's park and resume (32 %), the mint (15 %: `Plane.mintSlot` built the
held-slot list — an allocation and a walk of every lease — and every rotor tree's byte count before
its first step, the parent's affinity, decided alone), the gather opt-in xattr read (13.5 %), the
parent's inode read (12 %), the guard plan (7 %) and the dentry chain scan (7 %); ReleaseSafe's fill
of `undefined` arrays was 5–6 % of the handler lane and of the journal lane: the 256-slot dentry
`ChainBuf` (4 KiB per lookup and per create) and `std.sort.block`'s 512-element scratch, which it
declares before its small-array return (4–16 KiB per lock-set, guard-plan, write-op and range-page
sort). Fixes: the mint asks `acquire.affinityHolds` before the census (`mintChoice`'s step 1, one
predicate for both); `ChainBuf` keeps four keys inline and spills past them; `core/sort.stable`
(insertion up to 32 elements, block above) on the conveyor's lock sets, the guard plan, the
journal's write ops and spans and the set's range merges; and the warm ladder's probe reads the file
facts and the request's binding from ONE layout read (`read_path.Meta.file_at`; node-cache probes
per cold direct-leg read 4 → 3). Behaviour-preserving (the mint's tie test `acquire` "mint_choice",
the sort's stability test `core.sort`, the ladder's resolution test above). **Effect**: the QD1 local
create's handler time before the commit 11.4 → 6.6 µs, its daemon total 82 → 65 µs; the shared
directory's rate moved only to 6.0–6.2 k/s — the create is bounded by its thread handoffs, not its
CPU (§5).

### 3.7 Where a create's 90 µs go now (QD1, one bash loop, `SQUEEZEFS_OP_PROFILE=1`, 3,000 creates)

| span | µs | source |
|---|---|---|
| kernel → drain worker → handler lane (dispatch) + lane → worker → kernel (reply) | ≈ 25 | `CREATE` round trip 90.1 µs (tracepoints) − daemon total 64.8 µs |
| handler before the commit (mint, guards, parent read, chain scan, stage) | 6.6 | `fuse_op_phase_ns.create.handler_to_backend` |
| enqueue → the journal lane's pass begins (its wake) | 10.3 | `meta_tx_queue_wait` |
| the pass (admission, leaf locks 5.8, apply, reservation) | 7.5 | `meta_pass_total` |
| the journal write landing | ≈ 19 | `meta_window_total` 26.4 − the pass |
| the ack → the committer fiber resumed on its lane | ≈ 17.5 | `backend` 54.2 − the three above |
| the reply built | 4.0 | `backend_to_reply` |

Three cross-thread handoff pairs (worker ↔ handler lane, lane → journal lane, journal lane → lane)
cost ≈ 52 µs of the 90; the device write the charter owes is ≈ 19 µs. A `LOOKUP` is the same shape
without the journal: 27 µs round trip against a 5.0 µs handler.

## 4. The table, before and after (same container, same commands)

"Before" is the owner's build `b652702` as the owner ran it — its first mount after the container's
boot, so `buf_ring` (§3.1) — and, in the A-B-B-A column, the same build on a later mount
(`zero_copy`), interleaved with the final build `e837153` on ONE freshly formatted 2-MDT + 2-OST set
per leg (`scripts/perf/reset.sh sym2 <bin>; scripts/perf/table.sh … mdtest,randread`, 21:44–21:47
UTC): the A-B-B-A pair is the code's own effect under the same host load. The host (11 cores) is
shared with the venue, the owner's container and this one; a single after run moved by up to 20 %
with the other agents' load (two final runs shown).

| workload | owner (`sqz-dev`) | before, first mount (`buf_ring`) | A-B-B-A old (`zero_copy`) | A-B-B-A new | final runs (new) | ext4 (`/build`) | tmpfs |
|---|---|---|---|---|---|---|---|
| mdtest file create (4 ranks × 2000, one dir) | 3,163/s | 2,994/s | 2,919 / 3,134 | 5,625 / 5,844 | 5,602–5,740 | 26,682/s | 545,663/s |
| mdtest dir create | 3,175/s | 2,759/s | 2,939 / 2,724 | 5,887 / 6,254 | 5,927–6,304 | 7,312/s | 331,484/s |
| mdtest file stat | 128,349/s | 114,966/s | 76,944 / 106,613 | 186,298 / 209,572 | 170,238–181,327 | 5,041,231/s | 5,034,423/s |
| mdtest file read (open/close) | 76,350/s | 71,717/s | 51,540 / 65,623 | 63,720 / 63,776 | 61,715–65,487 | 5,895,016/s | 4,725,311/s |
| mdtest file remove | 3,126/s | 3,401/s | 2,982 / 3,104 | 6,300 / 6,439 | 6,033–6,272 | 134,673/s | 1,296,990/s |
| fio randread 4k O_DIRECT QD32 io_uring | 16.7 k | 17.4 k | 46.5 k / 51.7 k | 172.5 k / 170.8 k | 172.7–174.2 k | 334.0 k | — |
| fio randwrite 4k O_DIRECT QD32 | 94.5 k | 29.0 k | — | — | 106.8–109.1 k | 257.4 k | — |
| IOR 4 × 1 MiB write / read (GB/s) | 2.04 / 47.8 | 2.97 / 26.6 | — | — | 2.48–3.70 / 16.5–26.9 | — | — |

Ratios, new over old on the same mounts (A-B-B-A): creates 1.9×, dir creates 2.2×, stats 2.2×,
reads 1.1×, removes 2.1×, random reads 3.4× (9.9× over the owner's first-mount `buf_ring` posture).
The daemon's side of the same mdtest: cross-owner intents 20,003 → 0; daemon latency create 266 →
76 µs, mkdir 298 → 79 µs, unlink 241 → 83 µs, rename 273 → 78 µs. The random read: node-cache
probes per direct-leg read 7.00 → 3.00 (`Δmeta_kv_node_cache_hits ÷ Δtransport_zc_direct_legs`),
drain-worker CPU per read 17.2 → 5.5 µs, kernel READ round trip at QD32 424 → ≈ 185 µs.

Requests per user op on the final build (`scripts/perf/phases.sh`, mdtest's file phases one at a
time, kernel tracepoints): create = `LOOKUP` 38 µs + `CREATE` 108 µs + async `RELEASE`; stat =
`LOOKUP` 62 µs when the dentry expired (the phases run seconds apart); read = `LOOKUP` 46 µs +
`OPEN` 45 µs + async `RELEASE`; remove = `LOOKUP` 42 µs + `UNLINK` 103 µs. Postures on the same
disks before the fixes: 1-MDT `--single-writer` 5,973 creates/s, 1-MDT symmetric 5,628 — the 2-MDT
set's creates now match them: the cross-member price is gone from a directory's children.

## 5. The remaining gap per op, and its cause

| op | final | ext4 | gap | cause (measured) |
|---|---|---|---|---|
| file create, one shared directory | 5.6–5.8 k/s | 26.7 k/s | 4.6× | The kernel serializes the 4 ranks on the directory's `i_rwsem` across BOTH round trips (`LOOKUP` 38 µs + `CREATE` 108 µs under 4 ranks ⇒ ≤ 6.8 k/s). §AA.2's bound `2R + W ≈ 46 µs` (≤ 21 k/s): two bare round trips and the journal write landing the charter owes. The rest is the thread handoffs (§3.7: three handoff pairs ≈ 52 µs of a QD1 create's 90; a QD1 `LOOKUP` 27 µs against a 5 µs handler) — on this VM a cross-thread wake of an idle vCPU costs 10–20 µs. ext4 journals in RAM and never waits on the device inside a create. |
| dir create | 5.9–6.3 k/s | 7.3 k/s | 1.2× | As above; ext4's own mkdir is slower than its create. |
| file stat | 0.17–0.21 M/s | 5.0 M/s | 24–30× | The stats whose dentry expired (entry TTL 1 s; the create phase outlasts it) pay one `LOOKUP` round trip each (62 µs under 4 ranks: the rank, its drain worker and its handler lane share the rank's CPU — qid = CPU — so each request is a chain of context switches there). A dentry younger than 1 s costs no request. §AA.6: the TTL stays until entry-grained invalidation exists. |
| file read (open + close) | 62–65 k/s | 5.9 M/s | 90–95× | `LOOKUP` (expired) + `OPEN`, two round trips per file (`RELEASE` asynchronous, `FLUSH` elided); ext4 and tmpfs open in the dcache. |
| file remove | 6.0–6.4 k/s | 135 k/s | 21× | `LOOKUP` + `UNLINK` under the shared directory's lock; the unlink is a commit (one journal write landing) and three handoff pairs like the create. |
| random 4 KiB read, QD32, one job | 171–174 k | 334 k | 1.9–2.0× | ONE drain worker carries the job (the kernel picks the queue by the submitting CPU): 5.5 µs of worker time per read, ≈ 95 % busy — the kernel's `io_uring_enter` work for the FUSE commit and the device read (≈ 35 %) and the three node-cache resolutions (≈ 30 %). The devices give 207 k each at QD32. |
| random 4 KiB write, QD32 | 107–109 k | 257 k | 2.4× | Not decomposed in this pass (W1 patches through the same single drain worker). |

## 6. The levers this pass did not pull (each needs a design call; measured price stated)

1. **A metadata warm ladder** — LOOKUP and GETATTR answered on the drain worker by a try-only
   resolution (a node-cache hit, a local slot, no token, no stripe map; anything else demotes to the
   lane exactly as the READ law's arm (a) does). It removes the worker ↔ lane handoff pair from the
   most frequent requests: a `LOOKUP` is 27 µs at QD1 and 38–62 µs under 4 ranks against a 5 µs
   handler, and every create, stat, open and unlink of mdtest pays one. Needs: a thread-local
   try-only mode in `KvTree.fetch` (a miss is a demotion, never a demand load on the worker), the
   lookup census (`OpenTable.noteLookup`) on the worker, a partition law
   (`inline + demotes ≡ LOOKUP + GETATTR`) and its model of the try-only guard.
2. **Entry-grained invalidation, then a longer entry TTL** (§AA.6): `FUSE_NOTIFY_INVAL_ENTRY` from
   the holder's served namespace steps and from a reader's recall of a directory's dentry set
   (the reader knows the names its kernel looked up); the TTL then derives from the recall's
   bound instead of the charter's fixed 1 s. On mdtest's shape every stat, open and unlink would
   meet a cached dentry — the stat row is the kernel's rate (5 M/s) when it does.
3. **The drain worker as the handler lane** — the lane's fiber scheduler parked IN the FUSE ring,
   as the journal lane and the conveyor pass already share one thread and one ring (charter §5.2's
   "a task wake and a device CQE are one `io_uring_enter` wait"). It removes the handoff pair from
   EVERY request, the create's included (≈ 25 µs of its 90); the cost is the transport's slot law
   and the arm (b)/(c) dispatch law rewritten around it.
4. **The conveyor's two hops** (enqueue → the journal lane 10 µs, the ack → the committer's lane
   17.5 µs): a committer whose lane is idle could spin on its completion for one predicted device
   round trip before parking (the IL doorbell's spin-then-park law), trading CPU for the wake.

## 7. The venue's bracket rows

`scripts/bracket/bracket.sh --scoping --substrate file --budget-mb 7168 <row> <A> <B>`,
`BRK_FILE_SIZE=512m`, A = `e837153` (this pass), B = `b652702` (before), A → B → B → A, 60 s legs
(+10 s warm-up), numjobs 4 × iodepth 32 for the fio row; records under
`bench/brackets/7.2.6-sqz_aarch64_apple-vm_9c_virtio-blk/{rand-4k-read-20261013T214836Z,
creates-mdtest-20261013T215344Z,stat-mdtest-20261013T221451Z}`. SCOPING; ONE metadata volume
(the harness's set), so §3.3's member placement is not in these rows.

| row | A legs | B legs | A ÷ B | §3.1 ReleaseSafe row (nvmet-loop, `585152f`) |
|---|---|---|---|---|
| rand-4k-read | 272,815 / 271,111 IOPS | 53,391 / 46,138 IOPS | 5.1–5.9× | 39,630 / 47,130 |
| creates-mdtest | 14,755 / 12,014 creates/s | 12,425 / 11,494 creates/s | 1.0–1.3× | 7,556 / 9,722 |
| stat-mdtest | 40,998 / 46,004 stats/s | 45,696 / 45,460 stats/s | 0.9–1.0× | 50,703 / 52,582 |

The random read is the read path's resolution (§3.4) and the allocator (§3.2) — 4 jobs on 4
queues, so 4 drain workers carry it. The create row (many directories per task, one volume) moves
only by the CPU fixes: its rate is handoff-bound like the dev container's. The stat row is its
`LOOKUP` round trips (§6's first lever), unchanged by this pass. Every leg the harness marked
INVALID carries `meta_device_div_user` alone: on the FILE substrate the META device is the venue's
shared `/dev/vdc`, whose diskstats counted the other agents' traffic beside the leg's (leg 1 of the
read row: stats 125,952 B against diskstats 124 MB) — a substrate artifact, not a ledger break.

The nvmet-loop substrate the §3.1 rows used does not mount at `dev` HEAD, for either binary: `mount:
fabric: data volume …: refused: a data volume under an explicit identity needs a fabric_endpoint
record (squeezefs config set-fabric-endpoints)` — the harness predates the daemon-owned connect
(0003 §O.1) and still pre-connects the data namespaces. Owed to the harness: format the set with
`config set-fabric-endpoints` and let the daemon connect.

## 8. Gates

- Landing 1 (`93fdfae`, fixes §3.1–§3.5): Linux venue gate 46 steps — every step green but the
  literal rail (two allowlist rows shifted; rebased by `scripts/rails-literal-rebase`, `zig build
  rails` then 16/16 on both venues): unit 2,570 pass / 69 skip, smoke 87 / 2 skip, bench-smoke 41,
  shim-smoke 40, nvmet-smoke 53, suites-smoke PASS; macOS unit 2,086 / 27 skip, the same rail fixed.
- Landing 2 (`e837153`, §3.6): Linux venue gate 46/46 steps, 2,811 / 2,882 (71 skipped) before the
  rebase onto the sync-posture landing and 46/46, 2,816 / 2,888 (72 skipped) after it (unit 2,573 /
  70 skip, rails 18, smoke 88 / 2 skip, bench-smoke 41, shim-smoke 40, nvmet-smoke 53, suites-smoke
  PASS); macOS 28/28, 2,197 / 2,224 (27 skipped).

## 9. The four levers (0003 §AB)

The owner approved §6's four levers. Every row below is a same-mount A-B-B-A on this agent's own
container (`sqz-perf2`, 4 vCPU, 12 GiB, the venue kernel; `scripts/perf_mount.sh` with
`SQZ_PERF_NAME=sqz-perf2`, the dist profile): `scripts/perf/abba.sh <label> <A> <B> <phases>` runs
legs A B B A, each `scripts/perf/reset.sh sym2 <bin>` (a freshly formatted 2-MDT + 2-OST set on the
raw partitions) then `scripts/perf/table.sh` (mdtest 4 ranks × 2,000 in one directory; fio 4 KiB
O_DIRECT randread QD32). The host (11 cores) is shared with the venue gate and the other agents'
containers: a leg that ran beside a gate reads low across EVERY row (the A-B-B-A's order is what
separates the code's effect from that drift).

### 9.1 AB.1 — LOOKUP and GETATTR on the drain worker

A = `71f6767` (the lever), B = `d520a8a` (dev before it). Two runs; leg 1 of the second ran beside
another agent's build (every row low, A and B alike).

| op | run 1 A (leg 1 / 4) | run 1 B (leg 2 / 3) | run 2 A (leg 1 / 4) | run 2 B (leg 2 / 3) |
|---|---|---|---|---|
| file stat | 283.8 k / 219.6 k | 194.4 k / 36.9 k | 127.3 k / 426.6 k | 104.5 k / 132.7 k |
| dir stat | — | — | 106.8 k / 377.0 k | 62.4 k / 87.7 k |
| file create | 5,881 / 3,891 | 5,905 / 1,825 | 2,603 / 6,297 | 3,955 / 5,342 |
| file read (open/close) | 57.8 k / 46.7 k | 68.4 k / 35.7 k | 32.7 k / 62.7 k | 50.1 k / 50.8 k |
| file remove | 5,798 / 5,181 | 5,979 / 1,921 | 2,912 / 6,466 | 5,141 / 5,226 |
| randread 4 KiB QD32 | 164 k / 136 k | 145 k / 80.6 k | 121.7 k / 166.7 k | 137.4 k / 140.8 k |

The stat rows carry the lever: an expired dentry's `LOOKUP` (and a GETATTR) is answered on the
worker — `Δtransport_meta_inline_serves ≡ ΔLOOKUP + ΔGETATTR − Δdemotes`, demotes only for the
virtual names and the first touch of a cold node — so a stat is ONE round trip with no handoff (the
undisturbed legs: 2.2–3.2× B). Creates, opens and removes still pay a `CREATE` / `OPEN` / `UNLINK`
on the lane (AB.3's lever) and are within the host's drift.

### 9.2 AB.4 — the committer's bounded ack spin

A = the lever (`b70fbc0`), B = AB.1 (`f246494`). The rule's inputs, read from the A legs' `.stats`:

| A leg | `w` (wake, µs) | ack mean (µs) | bound | hits | fall-throughs | skips | busy |
|---|---|---|---|---|---|---|---|
| leg 1 | 8.5 | 147.8 | 0 | 674 | 12 | 39,318 | 251 |
| leg 4 | 8.6 | 308.5 | 0 | 0 | 0 | 40,003 | 251 |

| op | A leg 1 | B leg 2 | B leg 3 | A leg 4 |
|---|---|---|---|---|
| file create | 4,610 | 5,957 | 6,230 | 6,334 |
| dir create | 4,022 | 4,584 | 6,767 | 6,811 |
| file remove | 3,985 | 6,547 | 6,455 | 6,033 |
| file stat | 409.4 k | 408.6 k | 461.9 k | 461.0 k |

The ack's mean at this posture is the §L window-fill hold plus the write landing — 17–36 × the wake
— so the derived bound is 0 and the committer parks (98 % skips; the 674 hits are a phase start,
while the estimator still read short acks). Leg 1 ran beside a gate (every row low); legs 3 and 4
are equal within 2 %. The lever is neutral here BY ITS DERIVATION: it would engage where the ack is
within two wakes (a RAM or nvme-fast journal), and it never spins where it cannot win. An earlier cut
sampled the wake on every idle-lane park and read `w` = 252 µs — other fibers had run in the gap —
which set a 503 µs bound and burned CPU on 9,259 spins; the poll-count rule (`lanePolls` one more at
the resume) is that measurement's fix.

### 9.3 AB.2 — entry-grained invalidation and the isolated TTL

A = the lever (`e37ee89`, run 1; `d65a300` with the two follow-up fixes, run 2), B = AB.4's tip
(`121c5d4`). The perf set is ONE mount of a symmetric 2-MDT set — the ISOLATED posture: every class
derives to `T_owner` (45 s).

| op | run 1 A (leg 1 / 4) | run 1 B (leg 2 / 3) | A ÷ B | run 2 A (leg 1 / 4) | run 2 B (leg 2 / 3) |
|---|---|---|---|---|---|
| file stat | 5.02 M / 5.91 M | 0.91 M / 0.78 M | 6.4× | 5.60 M / 3.34 M | 0.22 M / 0.23 M |
| dir stat | 4.82 M / 6.06 M | 1.00 M / 1.17 M | 5.0× | 2.87 M / 3.86 M | 0.09 M / 0.27 M |
| file remove | 7,354 / 7,925 | 6,470 / 6,639 | 1.17× | 7,361 / 4,993 | 4,778 / 5,434 |
| dir rename | 8,018 / 7,977 | 7,000 / 6,901 | 1.15× | 5,575 / 5,630 | 2,996 / 4,921 |
| dir remove | 7,444 / 7,843 | 6,128 / 6,820 | 1.18× | 6,869 / 4,581 | 3,233 / 5,378 |
| file create | 6,593 / 6,557 | 6,594 / 6,695 | 0.99× | 6,457 / 4,494 | 4,394 / 4,751 |
| file read (open/close) | 59.2 k / 78.5 k | 66.8 k / 73.9 k | 0.98× | 64.6 k / 42.3 k | 52.3 k / 46.0 k |
| randread 4 KiB QD32 | — | — | — | 167.1 k / 139.1 k | 158.3 k / 158.8 k |

The stat rows are the kernel's rate now — ext4 on the same disks measured 5.04 M/s (§5): a stat of a
dentry younger than 45 s costs no request. Removes and renames gain the `LOOKUP` their path walk no
longer sends. Creates and opens are unchanged: a create's name is new (one negative `LOOKUP`, then
`CREATE`), an open is an `OPEN` round trip whatever the dentry. Run 2's B legs ran beside another
agent's build (every B row low); its A/B order still separates the lever. The `creates-mdtest`
shape (−C −F −u −z 2 −b 8, the bracket's) on the same container: A 11,211 / 10,705, B 11,097 / 11,858
(0.96×, inside the run's spread); the venue's brackets (§9.5): 1.075× (file) and 1.034× (nvmet-loop).

The coherence it costs nothing on the isolated mount: the A legs' `transport_notify_*` deltas are 0 —
every create, unlink and rename is the kernel's own and names nothing to it. The first cut DID name
the corpse destroys (16,000 `ENOENT` notifies per mdtest: the inode the last FORGET evicted); a
corpse is no kernel's (`kernel_origin` on the destroy) — and it WEDGED the first A-B-B-A: the
journal lane waited for a reserved sideband cell while the sideband thread itself waited on a
commit (a FORGET's destroy) — an `rmdir` parked 75 s. The producer now only queues and the mount's
notifier thread writes (0003 §AB.2 (2)). The cross-daemon row (`smoke: INVALIDATION COHERENCE
ACROSS DAEMONS`, every class at 60 s on both daemons): B's recalled names 5, epochs 0; A's one
epoch is the join's whole-view flush. The shared hot directory row found the second defect: a
stripe flip's NUL-led marker names rode the recall and the reader's screen refused the frame (the
live recall deadline tripped) — markers are never noted.

### 9.4 AB.3 — the drain worker drives a handler lane of its own

**The mechanism** (0003 §AB.3). Each drain worker builds a DRIVEN `exec.Lane` (`Options.driven`: no
thread; its 64 B `ParkedRing` and coalescer unchanged) and attaches it on its own thread
(`attachDriven`: a ring without `DEFER_TASKRUN`, `IORING_REGISTER_EVENTFD` on the lane's wake
eventfd). The worker's SQE128 ring holds a standing READ of that eventfd beside its own wake READ, so
its ONE `io_uring_enter` wait covers a foreign enqueue on the lane and the lane ring's own
completions; the lane's next timer and in-flight backstop fold into the wait's bound. The pass pumps
the lane after the commit cells and the bridge pumps (step 4a: disarm → inbox → timers → the fibers
ready at entry) and puts what those fibers published through the gate in the same pass (4a': the
reply cells, the bridge requests, the releases — their SQEs ride the pass's enter). A publication
from the worker's own thread elides the eventfd write (`wakeSelf`, `self_kick`); a runnable fiber or
an elided wake is work in hand (no park). Dispatch spawns the NAMESPACE and ATTRIBUTE class there
(`worker.onWorkerLane`); the DATA class stays on the CPU's handler lane (below).
`SQUEEZEFS_FUSE_HANDLERS_ON_WORKER` (default 1; 0 = every handler on the CPU's lane, the control arm).

**Why the data class stays on the lane — the all-on-worker cut.** The first cut ran every handler on
the worker. A-B-B-A, 3 rounds on `sqz-perf3` (§9's recipe; `scripts/perf/abba.sh … mdtest,randread,randwrite,ior`; A = dev
`d3a7845`, B = every handler on the worker):

| op | A legs 1/4, rounds 1–3 | B legs 2/3, rounds 1–3 | A mean | B mean | B ÷ A |
|---|---|---|---|---|---|
| file create | 6,738 / 6,568; 6,540 / 6,693; 6,782 / 5,798 | 8,720 / 8,051; 7,935 / 8,208; 6,840 / 6,750 | 6,520 | 7,751 | 1.19 |
| file remove | 7,779 / 7,600; 7,587 / 7,593; 7,472 / 6,424 | 9,273 / 8,916; 8,899 / 9,139; 7,923 / 7,748 | 7,409 | 8,650 | 1.17 |
| dir create | 7,223 / 6,961; 7,115 / 7,176; 7,037 / 6,588 | 8,010 / 8,528; 8,396 / 8,441; 6,855 / 7,750 | 7,017 | 7,997 | 1.14 |
| dir rename | 8,256 / 7,582; 7,992 / 7,898; 8,051 / 7,181 | 8,967 / 9,257; 9,014 / 9,267; 7,296 / 7,528 | 7,827 | 8,555 | 1.09 |
| dir remove | 7,976 / 7,092; 7,636 / 7,802; 8,244 / 6,517 | 9,511 / 8,051; 9,361 / 9,394; 7,774 / 7,496 | 7,545 | 8,598 | 1.14 |
| file read (open/close) | 81.8 k / 79.6 k; 80.1 k / 80.6 k; 81.1 k / 53.8 k | 293.9 k / 153.8 k; 151.9 k / 349.2 k; 137.1 k / 128.8 k | 76.2 k | 202.4 k | 2.66 |
| randread 4 KiB QD32 | 169.4 k / 179.4 k; 183.3 k / 178.2 k; 145.5 k / 156.1 k | 177.7 k / 179.0 k; 182.2 k / 167.2 k; 162.0 k / 160.8 k | 168.6 k | 171.5 k | 1.02 |
| randwrite 4 KiB QD32 | 110.1 k / 109.7 k; 110.2 k / 108.3 k; 100.2 k / 100.2 k | 63.3 k / 62.6 k; 63.5 k / 63.4 k; 65.2 k / 64.9 k | 106.5 k | 63.8 k | **0.60** |
| IOR write (MiB/s) | 2,905 / 3,063; 2,896 / 3,546; 2,334 / 1,866 | 3,694 / 3,239; 3,332 / 3,756; 2,269 / 2,258 | 2,768 | 3,091 | 1.12 |
| IOR read (MiB/s) | 7,854 / 5,322; 6,550 / 6,554; 2,925 / 2,847 | 6,392 / 7,017; 6,196 / 6,611; 4,108 / 3,196 | 5,342 | 5,587 | 1.05 |

The random writes lost 40 % on every B leg. The data plane's device completions park the CALLING
THREAD (`sync/completion.zig` — "the waiter a caller thread parked on the raw futex", 0003 WP-C row
14; `nvme_dev.Lane.wait`, `WriteWatermark.waitCompleted`, `uring_fs`): on a handler lane that blocks
the lane, on the worker it blocks the queue's whole ring — no delivery, commit, extraction or
release while a patch write is on the device. The `.stats` of one randwrite phase show the cascade:
in-place patch writes 121 k → 207 k (the accumulation behind the blocked ring drains before the next
write finds its overlay), `dlm_inode_key_waits` 53 k → 822 k. The worker must never wait on a device
(AB.1's try-only rule says the same for its inline serves), so the data class — READ, WRITE, the
syncs, RELEASE (a close drains the file's write-back), a truncating SETATTR / OPEN / CREATE,
FALLOCATE, COPY_FILE_RANGE, LSEEK, IOCTL, POLL, the locks, BMAP, the forgets — keeps its lane.

**The table** (A-B-B-A, 3 rounds; A = dev `d3a7845`, B = the lever with the dispatch law; these legs
ran beside other agents' gates and load storms — A's legs 14, 21, 31 and B's 32/33 read low on every
row, so the quiet series above is the cleaner ratio for the namespace rows, which the two cuts
dispatch identically but for RELEASE):

| op | A legs 1/4, rounds 1–3 | B legs 2/3, rounds 1–3 | A mean | B mean | B ÷ A |
|---|---|---|---|---|---|
| file create | 5,419 / 3,188; 3,246 / 6,086; 3,475 / 5,915 | 7,280 / 4,483; 7,615 / 7,039; 3,749 / 3,991 | 4,555 | 5,693 | 1.25 |
| file remove | 6,119 / 3,752; 3,951 / 6,936; 4,637 / 6,822 | 8,682 / 4,308; 8,242 / 8,251; 4,132 / 4,924 | 5,370 | 6,423 | 1.20 |
| dir create | 6,673 / 3,235; 3,853 / 6,534; 4,159 / 6,739 | 7,399 / 8,017; 7,867 / 8,292; 4,388 / 4,349 | 5,199 | 6,719 | 1.29 |
| dir rename | 8,093 / 3,380; 3,618 / 6,762; 3,187 / 7,273 | 8,566 / 8,789; 8,934 / 8,000; 4,947 / 4,150 | 5,385 | 7,231 | 1.34 |
| dir remove | 7,376 / 4,272; 3,840 / 7,131; 3,700 / 6,778 | 8,749 / 9,112; 9,276 / 7,971; 4,817 / 4,790 | 5,516 | 7,452 | 1.35 |
| file stat | 4.75 M / 2.92 M; 2.99 M / 2.19 M; 3.22 M / 5.73 M | 5.94 M / 2.72 M; 5.83 M / 5.37 M; 3.07 M / 3.14 M | 3.63 M | 4.35 M | 1.20 |
| dir stat | 5.33 M / 2.87 M; 3.23 M / 3.24 M; 3.11 M / 5.99 M | 4.91 M / 5.59 M; 5.72 M / 5.03 M; 4.22 M / 3.17 M | 3.96 M | 4.77 M | 1.20 |
| file read (open/close) | 52.5 k / 42.3 k; 42.1 k / 52.9 k; 40.5 k / 64.9 k | 254.5 k / 114.5 k; 262.4 k / 207.5 k; 155.5 k / 102.4 k | 49.2 k | 182.8 k | 3.72 |
| randread 4 KiB QD32 | 163.3 k / 113.1 k; 120.3 k / 170.2 k; 107.0 k / 181.7 k | 168.1 k / 105.3 k; 178.0 k / 166.4 k; 114.6 k / 134.0 k | 142.6 k | 144.4 k | 1.01 |
| randwrite 4 KiB QD32 | 102.5 k / 75.7 k; 105.2 k / 108.6 k; 68.8 k / 110.1 k | 104.8 k / 79.1 k; 105.9 k / 106.9 k; 69.8 k / 110.2 k | 95.2 k | 96.1 k | 1.01 |
| IOR write (MiB/s) | 1,864 / 1,879; 2,803 / 2,873; 2,003 / 2,938 | 2,806 / 1,896; 2,297 / 2,787; 1,865 / 3,121 | 2,393 | 2,462 | 1.03 |
| IOR read (MiB/s) | 3,271 / 4,049; 3,840 / 3,918; 3,289 / 5,327 | 4,270 / 2,418; 3,793 / 4,141; 3,102 / 4,370 | 3,949 | 3,682 | 0.93 |

Stats are the kernel's caches in both arms (AB.2); their ratio is the host's drift. The data rows
are the control arm's dispatch and read as such (randwrite 1.01, randread 1.01; IOR inside its
spread).

**The request's price, kernel-side** (`scripts/perf/opcount.sh`: `fuse_request_send → end`, the
same clock as §1's floor; A-B-B-A-A-B legs, `sqzmicro` from one thread, 20,000 calls each):

| request | A (legs 1 / 4 / 5) | B (legs 2 / 3 / 6) | what B is |
|---|---|---|---|
| GETXATTR (one round trip, the kernel caches no xattr) | 44.7 / 31.2 / 30.4 µs | 26.7 (p50 15.0) / 12.0 / 12.3 µs | the bare round trip `R` (13–14 µs, §1) |
| OPEN (QD1 open/close loop) | 44.9 / 32.4 / 34.6 µs | 19.6 / 19.5 / 18.9 µs | `R` + the open's handler |
| RELEASE (data class: the lane in both) | 49.7 / 36.2 / 38.2 µs | 47.0 / 46.8 / 45.2 µs | unchanged |

In mdtest (4 ranks, the census: LOOKUP 24,004, RELEASE 16,000, MKDIR 8,002, RMDIR 8,002, RENAME
8,000, CREATE 8,000, OPEN 8,000, UNLINK 8,000 — 88,013 requests) every namespace request lost the
pair: MKDIR 108.5 → 88.8 µs, RMDIR 136.5 → 103.0, RENAME 109.5 → 86.6, CREATE 113.1 → 92.3, UNLINK
125.0 → 100.0, OPEN 52.9 → 8.7 (p50 6.0); LOOKUP 17.8 → 16.6 (AB.1 already answered it on the
worker). The new floor for a request whose handler runs on the worker is `R` itself — 12 µs
send → end for a GETXATTR against §1's 13–14 µs for a negative LOOKUP — where the control arm paid
`R` + 18–33 µs for the two handoffs. User-visible (the syscall included, `sqzmicro`, 8 legs each):
GETXATTR from one thread 18–41 µs (median 35) against 49–87 µs (median 75); from four threads
233 k/s against 51 k/s; an open/close pair 31–49 µs against 60–81 µs.

The pass economy (mdtest `.stats`): passes per request 2.21 → 1.62, enters per request 2.24 → 1.75,
worker eventfd writes 57,710 → 13,833 (the RELEASEs' replies, from the lane), `handler_dispatches_own`
48,018 / `_foreign` 16,003 (the RELEASEs).

**The model** (`src/model/worker_pass_model.zig`): two foreign producers and the worker; the pass in
the shipped order — reap the lane-wake READ → the cells → lane DISARM → its inbox → the fibers ready
at entry (a same-lane wake; a reply cell published with no eventfd write) → the cells they published
→ arm the lane READ → park only with no runnable fiber and no unpopped cell. Holds over every
schedule (< 20,000 states). Each weakening fails: the park ignoring the ready FIFO (a fiber made
runnable during the run, no eventfd for it — the lost fiber); neither the post-run pump nor the park
check taking a same-thread cell (the lost reply); the lane disarmed after its scan (two producers: the
lost foreign wake); no READ of the lane's eventfd on the worker's ring (the wake lost between the
lane's queue and the worker's CQ wait).

**The red rows.** `session` e2e "a namespace request's handler runs on the thread that delivered it":
64 directory listings (OPENDIR / READDIR / RELEASEDIR) — every dispatch `own`, none foreign, the
handler's thread id one of the workers', zero worker eventfd writes; 16 file reads — each RELEASE
foreign, each OPEN own; the control arm's handlers on another thread with eventfd writes. On the
cross-thread dispatch (today's, the knob's default flipped) it fails: "expected 256, found 0". `lane`
"a DRIVEN lane": an owner thread's one wait on its own `DEFER_TASKRUN` ring wakes for a foreign
enqueue and for a 20 ms timeout armed on the lane's ring (a completion that is task work, never the
1 s bound); a same-thread spawn writes no eventfd.

**The laws that did not move, and their rows** (green in every gate below):

| law | how the merge keeps it | rows |
|---|---|---|
| teardown releases every retained slot | the worker's FUSE side drains and releases exactly as before; `transport_done` ends the session's `join`, then the thread runs its lane until every fiber has finished (late replies dropped), detaches it, quiesces the ring | `e2e retention teardown: N slots RETAINED at close …`, `e2e: teardown with an in-flight request …` |
| the kmbuf region outlives every handler (`f326cbc`) | `Group.deinit` joins the thread (its lane drained), frees the lane, THEN tears the kmbuf region down — after `Session.deinit` awaited the handler group | `e2e: teardown with a LOOKUP parked in its handler — the request's bytes … stay mapped until every handler has finished` |
| the request-timeout fail-stop (§Z, kernel 0028) | the worker's ring and its abort handling are unchanged; a handler that could park on a device runs on a lane, so a frozen device never wedges the worker | smoke `A FROZEN DAEMON NEVER HANGS THE HOST'S sync` [venue-exclusive] |
| zero-copy retention | WRITE (the only retaining request) is the data class: its lease, RETAIN commit and RELEASE run as before | `retain commit parks the slot until RELEASE re-arms it …`, `e2e retention: a held page-cache WRITE replies RETAIN …`, `e2e: a FUSE_WRITE payload is a zero-copy lease …` |
| the watchdog / supervisor probes | the op slots and the probe cadence are untouched; the merged lane's fibers are watched like any handler's | `watchdog tick: …`, `the watchdog never cancels: …`, `the reclaim cadence is one watchdog tick; the supervisor's probe interval is that tick …`, smoke `--daemon --supervise --log-file: …` |
| the move gate's parks | a parked namespace op parks its FIBER on the worker's lane (the epoch wake enqueues it there) | `the move gate re-issues an op refused by a move class …`, `the move park: observe → attempt → register → recheck → park …`, the handover MOVE MATRIX rows |
| the journal-ack spin's accounting (§AB.4) | `laneIdleBesidesCaller` is false on a driven lane (its owner serves a ring): a committer there counts `busy` and parks; the ledger stays exact (every park one class) | `conveyor` "the ack spin's ledger"; mdtest A: hits 1 / falls 1 / skips 39,997 / busy 257, B: skips 1 / busy 40,255 (the bound was 0 at this posture in both) |
| the per-name invalidation sideband (§AB.2) | the notifier thread stays its own thread and writes from its own cells; nothing on the merged lane waits on it | smoke `INVALIDATION COHERENCE ACROSS DAEMONS`, `conveyor` "the kernel view", `tokens` "invalidation coherence" |

**The host's disk filled during this pass.** Between 06:00 and 07:20 the Mac had 116 MiB free; the
venue's sparse images returned EIO (`I/O error, dev vdc`, `EXT4-fs (vdb): Remounting filesystem
read-only` at 40,497 s). Two venue gates in that window failed in rows the lever does not reach —
the handover MOVE MATRIX panicking `incorrect alignment` in the allocator under a node write's EIO,
the cache-path data smoke's FUSE_WRITE leases living 2–3.5 s behind EIO'd staging writes, `ring_park`
fdatasync answering −5 — and the same binaries passed 12/12 (move matrix) and the full gate on
healthy disks. An EIO under the move matrix panicking instead of failing loud is a defect of its
own (owed, not this lever's).

**Gates.** Pre-landing, on the rebased tree (`8762523`): venue 49/49 steps, 2,851 / 2,924 (73
skipped), 6 m 40 s; macOS 29/29, 2,216 / 2,245 (29 skipped), 3 m 04 s. The post-landing gates and
the four rows they made robust: §9.8.

### 9.5 The bracket harness's two substrates

Two defects the dev-perf pass found (§7) are fixed: the `file` substrate's metadata volume is a
dedicated loop device per set (`losetup --direct-io=on` over its image), so `meta_device_div_user`
reads this daemon's bytes alone — stats ≡ diskstats ×1.0000 on every leg, where every `file` leg was
INVALID before; the `nvmet` substrates record each data volume's `fabric_endpoint` after the format
(`config set-fabric-endpoints`) and release the format-time association, so each leg's daemon
connects its own data volumes (0003 §O.1) and the harness reads their names back from
`/sys/class/nvme` after the mount (the leg's diskstats name `nvme1n1` beside the metadata
`nvme0n1`). One VALID row on each (`creates-mdtest`, SCOPING, 4 × 20 s + 5 s warm-up, A =
`d65a300` (AB.2), B = AB.4's code; `--budget-mb 12288` — at 7,168 the venue's shared cgroup pushed
two legs to Yellow, `mem_budget_green` INVALID; records under
`bench/brackets/7.2.6-sqz_aarch64_apple-vm_9c_virtio-blk/creates-mdtest-20261014T{025426Z,030117Z}`):

| substrate | A legs | B legs | A ÷ B | spread | META stats ≡ diskstats | verdict |
|---|---|---|---|---|---|---|
| file (meta on `/dev/loop0`) | 14,877 / 15,481 | 14,115 / 14,133 | 1.075 | 9.3 % | ×1.0000 every leg | SCOPING-VALID, 4/4 legs |
| nvmet-loop (data by the daemon) | 16,110 / 15,659 | 14,694 / 16,037 | 1.034 | 9.1 % | ×1.0000 every leg | SCOPING-VALID, 4/4 legs |

### 9.6 The remaining gap per op (after AB.1–AB.4)

| op | now | ext4 | gap | cause |
|---|---|---|---|---|
| file stat | 5.0–6.2 M/s | 5.04 M/s | parity | served by the kernel's caches (AB.2) |
| file create | 7.3–8.7 k/s | 26.7 k/s | 3.1–3.7× | the kernel serializes `LOOKUP` + `CREATE` under the directory's `i_rwsem` (§AA.2's `2R + W`); a CREATE is 92 µs send → end, the journal write landing most of it — the handoff pair is gone (AB.3) |
| file remove | 8.2–9.3 k/s | 135 k/s | 15–16× | `UNLINK` is a commit (the write landing, 100 µs send → end) |
| file read (open/close) | 103–349 k/s (median ≈ 180 k) | 5.9 M/s | 17–57× | an `OPEN` round trip per open — 8.7 µs send → end on the worker now — and a `RELEASE` per close on the lane; `FUSE_NO_OPEN_SUPPORT` removes both (§9.7 (a), the owner's call) |
| random 4 KiB write, QD32, one job | 96–110 k | — | — | the data class on the CPU's lane (its device waits park the thread); unchanged by AB.3 |
| random 4 KiB read, QD32, one job | 143–172 k | 334 k | 1.9–2.3× | io-wq spreads one io_uring job over every queue; the 5-vCPU container is the ceiling, not one worker (§9.7 (b)) |

### 9.7 Two more levers, measured — the owner's call (0003 §AB.5, §AB.6)

**(a) `FUSE_NO_OPEN_SUPPORT` / `FUSE_NO_OPENDIR_SUPPORT`.** The kernel (7.2.6 `fs/fuse/file.c`
`fuse_file_open`) enters no-open mode the first time an OPEN answers `ENOSYS` (`fc->no_open = 1`,
connection-wide, never undone); from then every open is local: `fh = 0`, `open_flags =
FOPEN_KEEP_CACHE` (a directory: `| FOPEN_CACHE_DIR`), and `fuse_file_put` ends the release locally —
no RELEASE is ever sent, for a CREATE-opened file too. What the daemon keeps per open today, and what
the mode does to each:

| state today | where | under no-open |
|---|---|---|
| the open count per ino (`addOpen` at OPEN / OPENDIR / CREATE, `removeOpen` at RELEASE) and the `open_count_stranded` tripwire | `open_table.zig`, `lifecycle.zig` | gone; the unlink-while-open custody needs only the kernel's lookup count (it cannot evict an inode a file holds, so the last FORGET already certifies no open file) — the tripwire loses its meaning |
| the write-back drain at last close ("flush at release", charter §6.2) | `kv_fs.release` → `data.releaseIno` | loses its hook: no RELEASE, and a FLUSH arrives on every close only while `FOPEN_NOFLUSH` is unset and the clean-FLUSH fast path does NOT answer `ENOSYS` (an `ENOSYS` sets `fc->no_flush` forever) — the drain moves to a synchronous FLUSH per close or to eviction / a timer: a durability-posture change |
| `ATOMIC_O_TRUNC`: the truncate rides the OPEN under the size setter's exclusive guard | `kv_fs.open`, `lifecycle.zig` | **lost**: with `atomic_o_trunc` set, `fuse_setattr` skips the SETATTR for `ATTR_OPEN` (dir.c "actual truncation has already been done by OPEN") and no OPEN is sent — the mode requires dropping `FUSE_ATOMIC_O_TRUNC` from INIT (an O_TRUNC open then costs a SETATTR(size = 0)) |
| the virtual inodes (`.stats`, `.config`, `.trace`): OPEN pins the frozen generation (`fh` = the generation ino) and answers `FOPEN_DIRECT_IO` (a short read is EOF, no cached `i_size`) | `kv_fs.open`, `virtual_inodes.zig` | **lost** for every file of the connection: no pin, no `DIRECT_IO` — the stats file would need its exact size at LOOKUP/GETATTR and an `INVAL_INODE` per generation |
| `FOPEN_KEEP_CACHE | FOPEN_NOFLUSH | FOPEN_PARALLEL_DIRECT_WRITES` on every regular open | `regularOpenFlags` | KEEP_CACHE stays (the mode's default), NOFLUSH goes (see the drain), PARALLEL_DIRECT_WRITES is inert today (the kernel reads it only with `FOPEN_DIRECT_IO`: `fuse_dio_wr_exclusive_lock`; a user `O_DIRECT` write takes `fuse_cache_write_iter`'s exclusive `inode_lock` either way) |
| OPENDIR's existence check and `open_flags = 0` (no readdir cache) | `kv_fs.opendir` | the kernel CACHES readdir pages (`FOPEN_CACHE_DIR`); a peer's create/unlink in a directory reaches a cached listing only through an `INVAL_INODE` of the directory — AB.2's `INVAL_ENTRY` per name drops the dentry, not the page — so no-opendir needs the directory's own invalidation on every remote namespace change (owed), or OPENDIR stays |

Unchanged by the mode: page-cache coherence of regular files (KEEP_CACHE is already every open's
flag; coherence rides the explicit `INVAL_INODE` / `INVAL_ENTRY` of AB.2); `O_DIRECT` / `O_SYNC` (the
file's flags ride every READ and WRITE — `fuse_read_in.flags`, the write flags — and `O_SYNC` is the
kernel's FSYNC after the write); the shim's session handoff (the bind travels the control socket
with `SCM_RIGHTS` and the daemon screens the fd by `fstat`/`F_GETFL`; the bootstrap is a GETXATTR —
neither touches OPEN or `fh`); the token leases (granted per object by the namespace and data ops;
`kv_fs.open` takes none).

The estimate from the opcode census (§9.4's mdtest, 88,013 requests): OPEN 8,000 and RELEASE 16,000
— 27 % of the requests — disappear. The file-read phase sends 2 requests per file today and none
under the mode: its per-file cost falls from ≈ 23 µs per rank (180 k/s on 4 ranks, the OPEN 8.7 µs
send → end on the worker plus the RELEASE's load on the queue) to the kernel's local open + close
(≈ 1–2 µs: a stat is 0.3 µs from one thread) — 1–2 M opens/s, 5–10× today (ext4 5.9 M/s). A create
drops its asynchronous RELEASE (≈ 47 µs of lane time each, off the caller's path): ≤ 5–10 %. The
price is the four "lost / loses" rows above.

**(b) One drain worker per kernel queue.** The kernel (7.2.6 `fs/fuse/dev_uring.c`) builds one
queue per possible CPU (`nr_queues = num_possible_cpus()`) and picks it by the SUBMITTING task's CPU
(`fuse_uring_task_to_queue`: `qid = task_cpu(current)`), with no fallback: a request waits in that
queue's `fuse_req_queue` when none of its entries is free. The daemon drains each queue with one
worker (width 1 under kmbuf / zero-copy). Measured (`threadcpu`: the workers' CPU over 10 s; fio 4 KiB
O_DIRECT randread):

| fio | IOPS (A medians, 4 legs) | the workers it lands on |
|---|---|---|
| io_uring QD32, 1 job | 167 k | all five (24 / 19 / 14 / 12 / 11 % — or 61 / 24 % when io-wq bunches): a FUSE O_DIRECT read cannot issue NOWAIT, so io-wq workers submit it and their CPUs pick the queues |
| io_uring QD32, 2 jobs | 228 k | all five, 17–45 % |
| io_uring QD32, 4 jobs | 244 k | all five, 29–50 % — none saturated; fio, io-wq, the kernel's FUSE work and the workers share 5 vCPUs |
| psync, 1 job (QD1) | 16 k (62 µs) | the job's CPU (14–20 %) and the CPU it migrates to |
| psync, 4 jobs | 43 k (92 µs) | all five, ≈ 20 % each |

So numjobs > 1 already scales, and one io_uring job already spreads: the "one job → one queue → one
worker at 5.5 µs per read" ceiling binds only for a job whose submissions stay on one CPU (a
synchronous thread, which is latency-bound at QD1, or a job pinned by `taskset`). The two designs and
their price: (1) **several workers per queue** — the 7.2.6 code admits it without a patch (an
entry belongs to the io_uring whose command registered it; `fuse_uring_queue_fuse_req` takes the
first free entry), untested here: N workers each registering `depth / N` entries share a queue; the
price is N rings contending on `queue->lock` at every commit (0026's per-queue background ledger
lives under that lock), N kmbuf regions where the zero-copy posture keeps one per group today
(pinned memory × N), and N − 1 workers off the queue's CPU; (2) **work stealing** between workers'
lanes re-introduces the cross-thread handoff (≈ 18–33 µs, §9.4) for every stolen request — worth it
only for a saturated worker, which no row here shows. A kernel-side alternative (a queue choice that
falls back to an idle CPU's queue) is not in 7.2.6.

### 9.8 AB.3's post-landing gates

The first post-landing venue gate on `8762523` failed one row, a second run hung 28 min in another,
a third failed two more — each a row that asserted a timing or a process-wide counter the merge or
the new shard plan (four new tests moved the partition) exposed; each now waits for the observable
it asserts (AGENTS.md's gate rules), landed with both gates green before it:

| row | what it asserted | what failed | the row now | commit |
|---|---|---|---|---|
| smoke `INVALIDATION COHERENCE ACROSS DAEMONS` | B's served create visible on A at once | A saw its cached negative: A's own kernel is invalidated by its notifier thread, which the served step's ack does not wait for (0003 §AB.2 (2)) | polls A's view for 2 s (a lost invalidation keeps the negative for the 60 s TTL) | `3243f1a` |
| `session` e2e "a namespace request's handler runs on the thread that delivered it" | own dispatches ≡ the handlers EchoFs ran, right after the helper's last close | 257 against 256: the last RELEASEDIR is asynchronous | waits, bounded, until every counted dispatch ran (and for the sixteenth RELEASE) | `40f5d08` |
| `nvme_dev` "wake coalescing: N publishes … ONE eventfd write" | the process-wide `dev_wake_writes` delta == 1 | one write too many (a lane an earlier test left) — and the failed check returned with the lane held before its park, so `deinit` waited forever | sums its own lanes' `ParkedRing.written`; a `defer` releases the hold and joins every reader | `98def1b` |
| `session` e2e "EchoFs mounts over the ring" | requests ≡ replies after waiting for them to meet, read from a later snapshot | 13 against 12: the last close's RELEASE landed between the wait and the snapshot (a worker answering on its own thread meets sooner) | the snapshot itself must show the equality (bounded) | `f344862` |

Three failures the gates also met are not this lever's and passed on rerun: the macOS online fsck
"zero false positives under a live writer storm" ([C8] durable refs 0 vs derived 1, one shard of one
run), the smoke `-o ro` reader of a live set (C's remount refused by the home shard's join), and the
smoke joiner-side grant ledger (daemon A's mount refused loud on `KmbufRingEnomem` — no order-8
pages free in the venue after four hours of gates). Owed: the coherence row's SECOND step (A creates
`d/y`, B's kernel held the negative — the recall path, whose reader writes `INVAL_ENTRY` before it
acks) read `no` once, in a cached gate after the three below; it passed in every other run. Whether
B always holds the token on `d` when A's create runs (no token, no recall, the negative stands for
the TTL) is AB.2's open question, not yet reproduced standalone.

Three consecutive full venue gates on landed `dev` (`f344862`), each with a fresh local cache (every
binary rebuilt, every row run): **49/49 steps, 2,851 / 2,924 passed (73 skipped)** each — 6 m 32 s,
7 m 18 s, 7 m 57 s.

## 10. Zero-message opens (0003 §AF)

§9.7 (a) measured the mode and left it open; this pass implemented the orchestrator's posture —
no-open for regular files, OPENDIR stays, every durability law kept — with the knob
`SQUEEZEFS_FUSE_NO_OPEN` (default 1; 0 = the OPEN/RELEASE control arm, tested beside it).

### 10.1 The kernel contract, read off 7.2.6 (`fs/fuse/{file,dir,inode}.c`)

| fact | where | what the posture does with it |
|---|---|---|
| the first OPEN answered ENOSYS sets `fc->no_open`, never undone | `fuse_file_open` | the lifecycle answers OPEN ENOSYS; the daemon opens its own `.stats` before it reports ready (`latchNoOpen`) |
| with `no_open` set, `fuse_file_put` ends every `FUSE_RELEASE` locally — a CREATE-opened file's and one opened before the latch alike | `fuse_file_put` | until the latch a CREATE-opened file still sends RELEASE (served: the control arm's drain); after it none |
| FLUSH is sent on every `filp_close` — every close of every copy of an fd (a dup, a forked child's at exit, its exec under `O_CLOEXEC` too) — unless `FOPEN_NOFLUSH` (no writeback cache) or `fc->no_flush`; an ENOSYS sets `no_flush` for good | `fuse_flush` | a CREATE's reply drops `FOPEN_NOFLUSH`; no FLUSH is ever ENOSYS; FLUSH carries the drain; bursts coalesce (§10.4) |
| the only FORGET of a live inode is at eviction, after every `struct file` let go of it | `fuse_evict_inode` | the unlinked-open custody keys on the lookup count; the open count is OPENDIR's |
| with `atomic_o_trunc`, `fuse_do_setattr` skips the ATTR_OPEN truncate ("done by OPEN") | `fuse_do_setattr` | INIT drops `FUSE_ATOMIC_O_TRUNC`: the O_TRUNC open is a SETATTR(size 0) under the size setter's guard |
| a shared mapping is written back at munmap, after its fd's close | `fuse_vma_close` | a write no FLUSH follows is owed and drained once idle (§10.2 item 2) |

### 10.2 The posture, item by item

1. **No-open for regular files, OPENDIR stays.** `lifecycle` answers OPEN `ENOSYS` before any
   guard (`fuse_open_enosys`: 1 per mount — the daemon's own latch); CREATE counts no open. A
   readdir stays OPENDIR's: no-opendir would let the kernel cache listing pages that only a
   directory's own `INVAL_INODE` reaches (owed).
2. **The drain rides FLUSH.** The worker probes every FLUSH (`worker.dispatchFlush` →
   `probe_meta` → `probe_flush` → `DataPlane.hasWriteCustody`: the parked table, the overlay
   registry, the rewrite epochs, each O(1) on an empty set and one spin-locked scan otherwise) and
   answers it when nothing is held; custody demotes it to the CPU's lane (the data class), which
   drains — `data.releaseIno`, the RELEASE drain — before the reply. A write the FLUSH could not see
   is owed: every WRITE notes its ino after its custody is recorded, a FLUSH covers the notes made
   before it began (the stripe sequence orders the two under the stripe lock), and the reclaim
   cadence drains an owed ino once idle for TTI/10 (30 s). The drain is complete: a close drains
   at once (synchronously — the control arm's RELEASE was asynchronous); a held-open writer, a
   mapping written back after its fd closed, an async write landing after its close drain within
   TTI/10 + one tick; fsync, syncfs, truncate and the unmount drain as before.
3. **The open count / unlink-while-open custody.** The custody was already the kernel's lookup
   count (the corpse destroy keys on the last FORGET's `evicted`, never on the count);
   `open_count_stranded` is re-defined, not retired — a lost RELEASE in the control arm, a lost
   RELEASEDIR under no-open (0002 row 530).
4. **`FUSE_ATOMIC_O_TRUNC` dropped** under no-open (0002 row 529): the O_TRUNC open is the
   kernel's SETATTR(size 0), whose guard is the size setter's exclusive one, shipped to a foreign
   holder like any truncate.
5. **The virtual inodes** keep their per-lookup generations: the kernel's lookup reference pins a
   generation while a file holds it, its exact size at LOOKUP / GETATTR and TTL 0 bound every
   page-cache read to one document, and a new generation is a new ino — so no `INVAL_INODE` is owed
   (the reference itself calls `FOPEN_DIRECT_IO` "a belt" for generation inos). With no OPEN to
   refuse a write-intent open, a write or truncating open by root answers EACCES at the WRITE /
   SETATTR (0002 row 531).
6. **Shim / IPC.** The session handoff touches neither OPEN nor `fh` (the bind over the control
   socket, the `fstat` / `F_GETFL` screen, a GETXATTR bootstrap); an IPC write notes its ino owed
   like a FUSE write; the gate's shim smoke ran on the no-open default.
7. **The knob** `SQUEEZEFS_FUSE_NO_OPEN` (default 1); the control arm keeps its rows (the
   lifecycle and kv_fs rows' control halves, the smoke "THE CONTROL ARM'S CLOSE DRAIN").

### 10.3 The red rows

| row | pins | red when |
|---|---|---|
| `codec` "INIT reply carries FUSE_ATOMIC_O_TRUNC iff offered and wanted" | the INIT word | the flag echoed unconditionally (dev) |
| `session` e2e "a no-open session" | on the real kernel: ONE OPEN for 16 opens, no RELEASE, 16 FLUSHes answered on the worker, a custody FLUSH on the CPU's lane (off every worker), the O_TRUNC open's SETATTR(size 0) | with the flag kept: zero size-setting SETATTRs — the truncate lost |
| `open_table` "no-open … OWED", "the idle harvest", "close bursts coalesce" | the owed set's order law, harvest, deferral | — (new structure) |
| `lifecycle` "no-open", "no-open close bursts"; `kv_fs` "no-open through the POSIX layer"; `write_admit` "the no-open FLUSH's try-only probe" | OPEN ENOSYS with no guard, CREATE uncounted, the inline / lane / deferred FLUSH arms, CREATE without NOFLUSH, the custody predicate per class | — |
| smoke "A NO-OPEN CLOSE IS THE WRITE-BACK DRAIN" | write, close, kill -9, remount: both files whole; nothing parked, nothing owed, no RELEASE, ONE OPEN | a CREATE reply keeping NOFLUSH: the block stays parked (owed 1, parked 1); without the latch at ready: one RELEASE |
| smoke "CUSTODY NO CLOSE COVERS IS DRAINED ONCE IDLE" | a held-open writer's parked block drained by the cadence, then kill -9: bytes whole | the idle drain off: still parked (owed 1, parked 1) |
| smoke "A CLOSE BURST COALESCES ITS DRAINS" | 200 forked appenders on one fd: 1 drain, 601 deferred, the log whole after kill -9 | no window: 201 drains |
| smoke "O_TRUNC UNDER NO-OPEN TRUNCATES" | `: >` empties a striped and an inline file; across daemons both ways | (the e2e row above is the flag's red) |
| smoke "THE VIRTUAL INODES WITHOUT AN OPEN" | 200 reads under a create storm, each one parsed document of exactly the fstat size; two opens, two generations; EACCES for a truncate and a write | green from its first run: the generation design already carried it |
| smoke "THE CONTROL ARM'S CLOSE DRAIN" | the control arm's asynchronous RELEASE drains (parked 0 observed) before kill -9: both files whole | — |

Two existing rows held a dirty fd in the test process while they spawned probes; under no-open
each child's exec closes its copy — a FLUSH, the drain — so the `sync -f` row now writes from the
test process and spawns nothing until syncfs(2) (on an fd of the mount) returned, and the defrag
`--fold` row holds its fds in a forked child that never forks or execs (`FoldHolder`).

### 10.4 The close burst — found by the A-B-B-A, fixed before landing

The first cut drained every FLUSH with custody. The perf container's fork appender (a shell
holding one fd, 2,000 `/bin/echo` children appending through it; each child's exit a FLUSH) on a
CACHE-LESS set:

| arm | drains | device writes (OST diskstats) | for | elapsed |
|---|---|---|---|---|
| no-open, every close drained | 2,384 | 8,141 MB (57,000×) | 143 KB | 16.2–21.5 s |
| no-open, coalesced (one FLUSH-drain per ino per reclaim tick) | 2 + 2 idle | 4.2–8.4 MB | 143 KB | 0.2–3.7 s |
| control | 0 (one RELEASE at the shell's close) | 4.2 MB | 143 KB | 0.5–2.2 s |

Each drain re-uploaded the extending append's whole 4 MiB block. The same cost exists in the
CONTROL arm for an open-append-close loop (500 × `echo … >> f`, each `>>` an open and a RELEASE):
1,602 MB for 36 KB, 1.5 s — where the coalesced no-open arm writes 4.2 MB in 176 ms. That cost is
the data plane's (a drained extending append on a cache-less set re-uploads its block; a cache
path stages it — the fork appender wrote 0 B to the OSTs on the perf set's cache-path posture in
both arms): owed, below.

### 10.5 The A-B-B-A and the opcode census

Same-mount A-B-B-A on `sqz-noopen` (4 vCPU / 12 GiB, the venue kernel, the dist build of this
branch, a fresh 2-MDT + 2-OST set with a cache path per leg; A = no-open, B = the control arm — one
binary, the knob the arm). The host ran other agents' gates throughout (loads 3–15): a leg beside a
gate reads low on every row. Two rounds after the coalescing fix:

| op | A legs 1/4, 5/8 | B legs 2/3, 6/7 | A mean | B mean | A ÷ B |
|---|---|---|---|---|---|
| directory creation | 5,009 / 3,814; 3,434 / 6,560 | 2,927 / 3,833; 6,344 / 6,632 | 4,704 | 4,934 | 0.95 |
| directory stat | 3.26 M / 2.48 M; 2.94 M / 4.99 M | 2.35 M / 2.80 M; 4.18 M / 4.81 M | 3.42 M | 3.54 M | 0.97 |
| directory rename | 4,557 / 2,952; 3,102 / 7,632 | 3,041 / 3,915; 4,658 / 6,295 | 4,561 | 4,477 | 1.02 |
| directory removal | 4,931 / 2,938; 3,849 / 7,050 | 3,025 / 4,388; 6,437 / 6,462 | 4,692 | 5,078 | 0.92 |
| file creation | 5,954 / 2,551; 3,425 / 7,255 | 2,316 / 3,590; 4,987 / 5,396 | 4,796 | 4,072 | 1.18 |
| file stat | 3.04 M / 2.80 M; 3.11 M / 5.87 M | 3.88 M / 3.10 M; 3.20 M / 4.79 M | 3.70 M | 3.74 M | 0.99 |
| file read (open/close) | 108.9 k / 91.4 k; 157.7 k / 161.2 k | 91.2 k / 160.5 k; 181.7 k / 179.7 k | 129.8 k | 153.3 k | 0.85 |
| file removal | 6,451 / 3,285; 3,682 / 8,758 | 3,773 / 4,337; 6,299 / 7,262 | 5,544 | 5,418 | 1.02 |
| randread 4 KiB QD32 | 92.8 k / 97.1 k; 136.2 k / 142.8 k | 98.7 k / 101.2 k; 159.0 k / 167.4 k | 117.2 k | 131.6 k | 0.89 |
| randwrite 4 KiB QD32 | 43.2 k / 92.4 k; 101.4 k / 65.7 k | 92.8 k / 53.7 k; 103.2 k / 69.2 k | 75.7 k | 79.7 k | 0.95 |
| IOR write (MiB/s) | 892 / 1,584; 2,170 / 1,121 | 1,889 / 1,381; 1,334 / 1,256 | 1,442 | 1,465 | 0.98 |
| IOR read (MiB/s) | 2,357 / 2,926; 3,681 / 1,912 | 2,924 / 2,522; 1,551 / 1,890 | 2,719 | 2,222 | 1.22 |
| open/close, 1 thread | 34.1 k / 40.0 k; 46.3 k / 24.7 k | 32.8 k / 32.4 k; 22.3 k / 17.9 k | 36.3 k | 26.4 k | 1.38 |
| open + 4 KiB read + close, 1 thread | 32.8 k / 43.8 k; 29.0 k / 18.6 k | 35.3 k / 28.2 k; 16.0 k / 20.8 k | 31.0 k | 25.1 k | 1.24 |
| open/close, 4 threads | 503.8 k / 647.2 k; 586.6 k / 211.2 k | 55.8 k / 101.8 k; 68.8 k / 70.8 k | 487.2 k | 74.3 k | 6.56 |
| open + 4 KiB read + close, 4 threads | 459.9 k / 477.7 k; 542.2 k / 267.0 k | 71.0 k / 82.0 k; 57.2 k / 99.8 k | 436.7 k | 77.5 k | 5.64 |

The first two rounds (the first cut, before §10.4's fix — identical on these rows, where no close
finds custody twice): open/close 4 threads 543 k vs 68 k (8.0×), with a read 427 k vs 70 k (6.1×),
1 thread 1.44× / 1.41×; mdtest file read 1.19×, file creation 0.92×, the namespace rows 0.74–0.85
(two A legs ran beside a gate); randread 1.11, randwrite 1.05, IOR 0.96 / 0.86. Over the 16 legs
the mdtest rows are within the host's drift (file read A 147 k against B 145 k), the open/close
loops 5.6–8× from four threads and 1.2–1.4× from one, the data rows unchanged.

The census (`scripts/perf/opcount.sh`: `fuse_request_send → end`; the latch at ready in place):

| workload | A (no-open) requests | B (control) requests |
|---|---|---|
| mdtest 4 × 2,000 | LOOKUP 24,015, **FLUSH 16,001**, MKDIR 8,002, RMDIR 8,002, RENAME 8,000, CREATE 8,000, UNLINK 8,000, GETATTR 10, READ 2, STATFS 1 — **80,033**; OPEN 0, RELEASE 0 | LOOKUP 24,004, **RELEASE 16,000**, MKDIR 8,002, RMDIR 8,002, RENAME 8,000, CREATE 8,000, **OPEN 8,000**, UNLINK 8,000, READ 6, GETATTR 2, FLUSH 1, STATFS 1 — **88,018** |
| open + 4 KiB read + close, 1 thread, 5 s | 122,825 ops: **1 FLUSH per op** (122,827) | 98,580 ops: **1 OPEN + 1 RELEASE per op** (98,580 / 98,581) |

Per open/close pair the requests go 2 → 1, not 2 → 0: the drain law keeps a synchronous FLUSH
where the control arm's open paid a synchronous OPEN, and what disappears is the RELEASE (≈ 60 µs
of lane time each, asynchronous) and the OPEN's handler. That is why §9.7's 5–10× estimate holds
for concurrent open/close loops (the lane's RELEASE work was their bottleneck: 74 k/s against
487 k/s) and not for mdtest's file-read phase, whose per-file cost stays one synchronous round
trip — FLUSH (6.7 µs send → end in the first census, against the control arm's OPEN 13.2 µs) —
plus the rank's own path walk. A census's round trips are not an A/B (each arm's ran minutes apart
under a moving load; the second census's A ran beside a gate); its counts are exact.

### 10.6 Gates

Pre-landing on the first commit: venue 49/49 steps, 2,864 / 2,942 (78 skipped), 6 m 53 s (an
earlier run of the same tree: 2 failed — the defrag `--fold` row, §10.3's held fds, and one
"data plane WITH a cache path" run that passed 2/2 alone after); macOS 25/29 steps with one
failure, `ops.jobs.fabric` "duty cycle adheres within ten percent" (a wall-clock duty cycle under
host load; passed alone).

### Owed

- The data plane's drain of an extending append on a cache-less set re-uploads the whole block
  (§10.4): 1.6 GB for 36 KB in the control arm's open-append-close loop. The coalescing bounds the
  no-open arm's drains; the per-drain cost is the data plane's (§J).
- No-opendir (directory listings in the kernel's page cache) needs the directory's own
  invalidation on every remote namespace change.
- `.stats` repeats a few keys across families (a JSON parser in strict mode refuses the document:
  `DuplicateField`); the smoke parses with `use_first`.
