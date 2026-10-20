# Handoff — 2026-10-20: the parity wave (ext4/xfs in memory), the interposer clears every mark

**Checkpoint:** `dev` at the commit that adds this note (on `3f9a3ed`); everything pushed. The
2026-10-18 and 10-19 notes hold the release sequence, the earlier waves, the rulings and the venue
plan. Kernel `7.2.8-9.30-omarchy-sqz` (series 0001–0030). The owner paused here for system updates.

## The bar (owner, 2026-10-19)

On an in-memory substrate SqueezeFS is the fastest of {ext4, xfs, SqueezeFS} or on par, on every
row. The LD_PRELOAD interposer (the client bypass, Intel DAOS's libioil as the model) is the
"fastest" gate — THE path that must clear every mark; the FUSE rows get as close as the kernel
round trip allows. Reference numbers: `docs/evidence/rung-10-localfs-comparison.md` (ext4/xfs on
the same brd → nvme-loop namespace, cooled).

## Where the rows stand (this box; `rung-10-*` notes hold each A/B with `tctl_c`)

| row | ext4 | FUSE path now (from) | interposer, direct arm |
|---|---|---|---|
| 4k randread ×32 / QD1 | 2.0 M / 268 k | **1.74 M** (1.19 M) | **3.08 M / 258 k = 101 % / 96 %** |
| 4k randwrite ×32 / QD1 | 1.96 M / 218 k | **1.04–1.12 M** (415 k) | **3.03–3.07 M / 215 k = 100 % / 99 %** |
| 64k randread / randwrite ×32 | 79 / 76 GB/s | 58 GB/s (103 % of ext4's io_uring row) / 31 | **80.4 / 76.1 = 102 % / 100 %** |
| 1 MiB seq read / write ×32 | 98 / 85 GB/s (psync) | 73 / 50 (io_uring ×4: 89 % / 70 %) | **98.0 / 84.2 = 100 % / 99 %** |
| mixed 4k randrw 50/50 | 3.04 M | — | **3.01 M = 99 %** |
| mdtest -u creates / removes | 711 k / 573 k | **58–84 k / 59–91 k** (41 k / 37 k) | FUSE rows (the shim serves data) |
| staged small-file 4k writes | ≈ 2 M | **41–73 k** (1.3 k) | — |
| stat | 9.8 M | 10.9 M (on par) | — |

**The interposer clears every data row it serves** (reads `22bcf12`, writes `0437d26`): a trusted
client (`-o interception=direct`; its uid must open the data device nodes) reads and writes the
device itself under a layout lease the daemon invalidates by epoch, with a recallable WRITE lease
(Dekker over a client-writable ledger, model-checked, a loud forced arm) that makes the inode
device-true while it stands. Kill rows and a dm-log-writes cut byte-exact. The trust boundary is
in charter §7 and decision 0003 §AX/§AY.

## Landed today (every commit through its own full gate; train 3/3 on `74e5402`)

| commit | what |
|---|---|
| `b2363b3` `3f9a3ed` | the create path, waves 1 and 2: the parent stamp parked (§AV), the conveyor reserving before it applies with the applier venue ON at `clamp(cpus/8, 2, 4)` (§AW), the overlay as a persistent chunked arena run, the stripe verdict's RwLock (negative LOOKUP 86 → 7 µs); and **the §AG.2 kernel pin at the holder** (§AZ): a name-less recall had retired a reader's grant while its kernel still served the cached dentries — a pre-existing coherence hole the gate caught |
| `a1ebd3a` | 4k writes: the Shared-class write off the admission's shape snapshot (the per-ino layout-lock convoy removed; the shape row a model-checked seqlock) |
| `83d5435` `7ae262b` | reads: the FUSE boundary measured (the round trip costs LESS host CPU than ext4's whole read — the barrier was ours); the per-ino read view (`ino_witness` the one choke point; model + 3 weakenings); **`umount --lazy`'s verdict under mount-id churn** (the verb named another mount's holders and called the daemon dead — fixed, deterministic row) |
| `74e5402` | sequential bandwidth: the latency split across the boundary, NOWAIT-sized bridge pieces (io-wq punts 495 k → 1.9 k / 10 s, daemon CPU −8–10 %), word-wise overlay bitmaps, the stream-lanes lever |
| `0e3249b` `22bcf12` `0437d26` | the client bypass as a matrix mode; the direct-drive READ, the ring park; **the direct arm** (reads), **the direct writes** |
| `91e0e09` | the staged arm: **the promotion-side durability hole** (DUR-1 before the publish through a coalesced per-volume barrier; synced records DEPART until a cover round — dm-log-writes 415/415 lost → 1000/1000 intact) and the arm's serialization (grain bitmap, synced-only holds, no-tx size-neutral overwrite, lazy punching: 34–92×) |
| `223ac1b` | **fsync makes staged bytes power-cut durable** (owner ruling B): one ranged FSYNC on the lane ring, coalesced; the same-key re-stage hole; charter item 10 amended; the reference HAD this leg (0002 row 549) |
| `81e9f21` | **fence tripwires** (owner ruling B): 7 sites reclassified through `seams.classifyFence` (FAILED/fenced ⇒ counter; healthy ⇒ tripwire); the harness's EIO exception deleted; a compensation write failure now latches FAILED instead of tripping |
| `f1b8570` | two gate flakes fixed at their seams (the lane hold held only a pass that pumped; the wake-coalescing baselines) |
| `96659b4` `944f5e2` `c5e95b4` | kernel series 0030 built and booted; two rings per queue the default with every red pinned (`rung-10-workers-per-queue.md`, `rung-10-dismount-corpse.md`) |

## Owed / next (in order), when work resumes

1. **The release window** (owner's sequence: ONE LTP, ONE xfstests with every tripwire judged,
   fixes as we go; a final full gate with no fixes; the tag). The two rulings are in; the flakes
   found this week are fixed at their causes; `scripts/venue zig build` first (the suites run the
   installed binary).
2. The FUSE rows still short of the bar, each with its binder named: creates (`rung-10-create-path.md`
   wave 2: the wake chain from ack-after-landing on one journal lane; gather xattr → an inode flag;
   a per-lane ino window; the heap admission's resolve), 4k writes through FUSE (the worker's 7.8 µs:
   more rings per queue, the series' per-op cost), the staged arm (`rung-10-staged-arm.md`: in-place
   overwrite of an unsynced record, put-from-lease without the heap image, the record write off the
   mapping via the lane ring), sequential writes through FUSE (`rung-10-seq-parity.md`: the serialized
   prelude, the zc path's per-page bookkeeping, fio's submit cost).
3. Kernel patches drafted and unbooted in `scripts/kernel/proposed/`: 0031 (FUSE uring per-request
   bookkeeping, ≈ 4–5 % of a read), 0032 (shmem NOWAIT reads: the warm `READ_FIXED`'s io-wq punt).
   Build them with the next kernel cycle.
4. Smaller owed items: `MetaSet.fsync` barriers every member (2.1 per fsync); the dismount pass pays
   one flush per resident; the pool ledger does not count held copies; `serve` borrows an overlay
   record across device reads with no hold; the allocator's give-back belt fired once; a FAILED
   volume's in-place drains log ≈ 115 error lines per writer ino; the direct arm's forced recall end
   to end and its uid-refusal arm with a non-root client; the lease epoch bumping on custody only.
5. The 200 G venue (owner's inventory pending).

## Method notes

- One benchmark on the host at a time, never queued; `scripts/perf/matrix.sh` cools per row and
  records `tctl_c`; the host throttled thermally through 10-19 (the owner disabled the throttle;
  the box still heats). Agents work on NAMED branches (a clone on `dev` published six commits on
  10-20 — gated green after the fact; the hooks now push only from the main tree).
- Reading a gate red: the rails (literal, stats inverse census, fuzz census, cost-table staleness)
  are mechanical; a smoke "daemon died" or a coherence row red was, every time this week, a real
  pre-existing defect the gate's parallel load exposed — chase it, never ledger it.
