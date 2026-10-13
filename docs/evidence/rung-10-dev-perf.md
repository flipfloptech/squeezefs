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
