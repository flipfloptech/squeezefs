# Handoff — 2026-10-20: the parity wave (ext4/xfs in memory), the interposer clears every mark

**Checkpoint:** `dev` at the commit that adds this note (first on `3f9a3ed`; amended 2026-10-21 on
`0e75e13`); everything pushed. The 2026-10-18 and 10-19 notes hold the release sequence, the earlier
waves, the rulings and the venue plan. Kernel **`7.2.8-9.33-omarchy-sqz` (series 0001–0033)** since
2026-10-21 07:47 — patches 0031 (cheaper FUSE uring round trip), 0032 (shmem NOWAIT reads), 0033
(per-folio zero-copy pages) — every landing since gated green on it. **Paused here on the owner's word;
the release window waits for the owner's go and runs on this kernel.**

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

## The third wave (2026-10-20 → 21, "squeeze out the last bit")

| commit | what | this box |
|---|---|---|
| `fd468d3` (write-iops wave 2) | three rings per kmbuf/zc FUSE queue and every held write on its worker's own lane are the derived defaults (the one-submitter regression of depth 0 is gone with three rings); the ring shares PINNED-EXACT (the ceil/rest shares pinned +512 MiB per three-ring mount — the gate's 16 shards each paid it and the venue's memory sweep killed daemons mid-row); two rigs pin one ring by name | 4k randwrite ×4 **1.14 → 1.40 M (71 % of ext4)**; 4k randread **1.88–1.90 M (96 %)**; 64k write 1.19×; seq write 1.20× |
| `815abcb` (seq wave 2) | `SQUEEZEFS_FUSE_STREAM_LANES=own` (−5 % cores); kernel patch 0033 drafted from the trace: the ring worker's per-page zc bookkeeping is 52 % of its read cost, 25 % of its write cost | bandwidth flat at 75–81 % / 77–89 %; the remainder is 0033 (now booted, unmeasured) and fio's own submit cost through FUSE |
| `561bb90` (create wave 3) | the §L pad hold derived from the device grain; the applier's resolve seeded from the committer's leaves; the pass venue as a lever | creates 1.16×, removals 1.22× (106 k / 106 k) |
| `0e75e13` (create wave 4) | **the metadata ack posture (owner ruling 2026-10-20, charter §1 item 10 + §5.2 + §5.3, decision 0003 §BB)**: a window of this mount's own kernel-origin members is acked once its RAM apply is published and its image sits in the volume's **journal tail** — a kernel-held memfd mirror the supervisor replays to the device, barriers and only then releases the writer claim; wire-served steps and control records (leases, grants, the claim) stay acked at the landing so every word of the symmetric protocol still means DURABLE; `SQUEEZEFS_META_ACK_POINT=landing` is the control arm; fsync/fsyncdir/syncfs barrier the lane. Smoke row THE REPLAY PROOF. The test runner's hang watch aborts this process's FUSE connections before exit (a parked thread of ours kept a shard a zombie) | neutral at 32 ranks on RAM (the round trip it removed is 12.6 µs here; the appliers' hot-leaf serialization is the next ceiling), **1.40× at 64 ranks**; the gain lives on real NVMe |
| `11de20a` (staged wave 2) | the in-place grain overwrite of an unsynced record (header into the other slot → the grain via the lane ring → the RAM flip; a seqlock for readers), the read-nothing fast path off a staged-point cache, the times drain draining its tick's population once (it journaled ≈ 95 k records/s in a re-snapshot loop) | small-file 4 KiB rewrites at 16 jobs **48 k → 290 k/s (50 % of ext4)**, p99 1.9 ms → 100 µs; the remainder: the grain write punts to io-wq on btrfs and 16 lanes share one segment file's inode lock |
| `9798762` | the supervisor's first missed probe is silent (an orderly teardown has an instant where the control inode fails while the mount is still listed) | the lazy-umount row 6/6 |

## Owed / next (in order), when work resumes

1. **The release window** (owner's sequence: ONE LTP, ONE xfstests with every tripwire judged,
   fixes as we go; a final full gate with no fixes; the tag) — on this kernel. `scripts/venue zig
   build` first (the suites run the installed binary). The metadata ack law changed what a
   `kill -9` row proves (the replay) and what a power cut may lose (≤ the flush window of un-fsync'd
   metadata): the dm-flakey rows will say so.
2. The FUSE rows still short of the bar, each with its binder named: creates (the appliers' hot-leaf
   serialization, the gather xattr → inode flag, a per-lane ino window; group commit §BB.6 not
   built), 4k writes (the worker's ≈ 8 µs: ≈ 5.5 kernel — 0031 now booted, unmeasured), the staged
   arm (per-lane segment files + a NOWAIT host FS or its own device), sequential bandwidth (0033
   booted, unmeasured; fio's submit cost).
3. Measure the three kernel patches now that they are booted: the read/seq rows A/B against the
   10-20 numbers (same daemon, new kernel).
4. Smaller owed items: `MetaSet.fsync` barriers every member; the dismount pass pays one flush per
   resident; the pool ledger does not count held copies; `serve` borrows an overlay record across
   device reads with no hold; the allocator's give-back belt fired once; a FAILED volume's in-place
   drains log ≈ 115 error lines per writer ino; the direct arm's forced recall end to end and its
   uid-refusal arm with a non-root client; the lease epoch bumping on custody only; a journal write
   failure after an ack latches FAILED at once (a retry from the tail's bytes is the softening); the
   symmetric death ladder orders a dead peer's replay by time (`T_owner` 45 s), not by waiting.
5. The 200 G venue (owner's inventory pending).

## Method notes

- One benchmark on the host at a time, never queued; `scripts/perf/matrix.sh` cools per row and
  records `tctl_c`; the host throttled thermally through 10-19 (the owner disabled the throttle;
  the box still heats). Agents work on NAMED branches (a clone on `dev` published six commits on
  10-20 — gated green after the fact; the hooks now push only from the main tree).
- Reading a gate red: the rails (literal, stats inverse census, fuzz census, cost-table staleness)
  are mechanical; a smoke "daemon died" or a coherence row red was, every time this week, a real
  pre-existing defect the gate's parallel load exposed — chase it, never ledger it. Two gates this
  wave ran to their caps on a HUNG smoke row whose storm child was unkillable in a FUSE wait: read
  the `HUNG TEST` stacks, clear the row's daemons by hand, and expect the cause in the branch.
- The `fsck_findings` / `block_untracked_free_refusals` lines without an "(expected error)" prefix in
  a gate log belong to the merged-census row's seam-ON arm and appear in every green gate.
