# Handoff — 2026-10-19: the first local performance wave

**Checkpoint:** `dev` at the commit that adds this note (first on `8c1aa86`, amended on `039ae68`); everything pushed. The
2026-10-18 note holds the release sequence, the owed lists, the rulings and the venue plan; this one
adds the day's performance work. Venue: this host (`SQZ_VENUE=local`).

## Landed today (each with its own full gate, A/B and evidence note)

| commit | lever | this box |
|---|---|---|
| `7ab7e68` | (b) zero-copy big writes: the overlay class HELD in its slot, `WRITE_FIXED(device ← slot)`; `SQUEEZEFS_ZC_HOLD_OVERLAY` | seq write ×4 14.7 → 27.5 GB/s, io-wq 2.5 → 0.35 cores |
| `f410930` `3b14b6d` | (a) fiber-aware device waits (decision 0001 amended): a DMA parks only its fiber | 1M ×1 1.80×; in-flight DMAs 3.5 → 9.6 |
| `451bab6` | the `data_alloc_population_drift` check race (count, popcount and trim window read apart) | 4005/4069 racing checks fired → 0 |
| `b8d3b61` `1f98985` `b1d6333` | (c) the publish off the heap: `merge_scratch` slab pool (the budget's), in-place staging, merge-walk diff; the save 64 → 213 blocks | seq write ×4 27.4 → 47.1 GB/s; publishes/block 0.585 → 0.086; meta bytes/block 17.2 → 2.3 KiB |
| `f17a364`…`8c1aa86` | lever 4: a HELD write dispatches to the worker's own lane; `SQUEEZEFS_FUSE_WRITE_OWN_DEPTH` = 8 (ruling: all-own regressed QD32×1 0.76×; 0 once lever 5 lands) | 4k randwrite QD1 2.06× (p50 18.8 → 9.8 µs); QD32×4 1.03× |
| `2f7878a` `c28babc` `bdfb316` | FAILED volume: the drain warns once; `umount` finds a live daemon by process; the busy census reads `/proc/<pid>/maps` to EOF (mmap holders were invisible) | — |

Final head over the full matrix (`wave1-final-r1`, in `rung-10-perf-baseline-workstation.md`):
**seq write 1M ×4 46.6 GB/s (3.2×, 79 % of raw), ×1 24.5 GB/s (4.1×); 4k randwrite QD1 84.9 k
(1.48×), QD32×4 427 k (1.08×); 4k randread QD32×4 853 k (1.08×); reads unchanged.** Write and read
bandwidth bars cleared on the RAM substrate; write IOPS at 59 % of its bar is the open gap.

## The second wave (same day, after the kernel change)

| commit | what | this box |
|---|---|---|
| `b67ed72` `9fc7ff6` | **the mdtest wedge** (§1.1): a driven lane's owner queued as a raw waiter behind a fiber of its own lane on a FREE lock, and the futex table's count-one wake skipped the kernel wake (model `DrivenOwnerModel`); try-only probes for the worker | `mdtest -np 32` wedged → 42–44 k creates/s, 10.5 M stat/s, every backstop 0 |
| `25fa103` `6e8ab9d` | the read side: per-ino presence stripes on the accumulate table (a read of an empty table scanned its whole index under the lock); one divert ask per probe | 4k randread QD32×4 828 → 897 k, 64k 1.087×; worker µs/read 4.94 → 4.53 |
| `0f6bda4`…`02e942b` | fresh-file publish: fresh blocks ride the epoch save; charter §6.2 items 1/3 amended, 0002 row 546 | overwrite rows +16 % / +9 %; journal bytes/block 9.8 → 0.7 KB; the lay-down is bound by the kernel's exclusive inode lock on extending O_DIRECT writes (≈ 4 GB/s per file) |
| `96659b4` `944f5e2` | **kernel series 0001–0030**: patch 0030 (several io_urings per FUSE queue under kmbuf/zc) built as `7.2.8-9.30-omarchy-sqz`; the sqz pkgrel is `N.S`; the script tracks `linux-omarchy` (Omarchy folded BORE into it 2026-10-19) | booted; gate green on it |
| `ff2d73d`…`941d343` | **the fsync route, re-landed**: the journal lane's off-lane ticket waits through the fiber-aware route — and its wedge (`654fd83`, reverted in `a9f7f5d`): the route gave the inline block-grant carve a park point, so a minter that LOST the top-up latch spun on the lane the winner was homed on; the latch is a futex word now, the loser parks, the winner wakes; smoke row **A STREAMING OVERWRITE NEVER WEDGES THE DAEMON** (red without the fix — the gate had been green with the wedge in it) | 16-job fsync p99 338 → 297 µs, write p99 119 → 84; sw1m ×1 29.9 GB/s |
| `832f962`…`2ea8765` | **the overlay record-lifetime panic**: `completeStore` dropped the record's last hold and THEN notified; a settler's re-check tore the record down and the LIFO pool re-issued its slot under the thread still in the notify. The drop and its announcement are one critical section of the record's notify, the settler quiesces on it once (model 4, weakenings `drop_outside_lock`/`no_quiesce` fail) | the window hit 3× in 6 sessions and absorbed |
| `fde9a1f`…`039ae68` | **lever 5, the daemon half**: the FEATURES probe, two `sqz-fuse-ur<qid>` workers per queue with `ceil(depth/2)` entries each (pinned bytes unchanged), `SQUEEZEFS_FUSE_RINGS_PER_QUEUE` (derived 2 where the probe reads present); the own-lane knee split over the queue's rings | 4k randread QD32×4 871 k → **1.25 M (1.43×, the bar cleared)**, QD32×1 1.73×; **4k randwrite QD32×4 0.85× (+2 cores) — a flagged defect**, an agent on it; depth 0 still regresses QD32×1 (0.83×), so lever 4 stays at its knee |

## Rulings (orchestrator, under the owner's "performance first; a regression is a defect")

- `SQUEEZEFS_FUSE_WRITE_OWN_DEPTH` ships at 8 (split over a queue's rings); all-own regressed
  QD32×1 even with two rings (a half-loaded worker parks per CQE). `SQUEEZEFS_FABRIC_IOPOLICY`
  round-robin (10-18). Pools register with the memory budget (the publish and transform scratch
  pools do now; the zstd codec pool is owed).
- Two rings per queue ship as the derived default although the 4k randwrite QD32×4 row lost 15 %:
  no regression-free setting captures any of the read gain (1.43× / 1.73×, the §1.4 read bar), the
  mechanism is named, and `SQUEEZEFS_FUSE_RINGS_PER_QUEUE=1` is the one-line return. Owner may
  overrule.
- A wedge or panic found by a perf session is fixed or reverted before anything else lands (the
  fsync route was reverted within the hour and re-landed with its gate row; the overlay panic got
  its model).

## Follow-ups recorded by the wave's agents (owed)

- `serve` borrows an overlay record across device reads with no hold and never re-checks `birth`
  (pre-existing; `rung-10-overlay-lifetime.md`).
- The allocator's give-back-vs-next-claim belt (`block_claim_anomalies`) fired once in six seq-write
  sessions, fsck clean (`rung-10-pack-read-tls-teardown.md` describes the race).
- Remaining raw foreign-lane waits: `io/uring_fs.Pool` cells on a node-cache miss; the transport
  sideband cells (`rung-10-fsync-route.md`). `MetaSet.fsync` barriers every member (2.1 per fsync).
- The lay-down's kernel inode-lock serialization on extending O_DIRECT writes; the 4k read's
  remaining 2.9 µs of kernel + three KV lookups; the zstd `CodecPool` unregistered with the budget.
- Perf method: the 4k rows swing 12–43 % between sessions of one binary (20–35 % with two rings);
  ≥ 4 legs per side, judged within a matrix; the venue's level drifted ≈ 25 % during one afternoon.

## Next levers (in order; the profiles are in the evidence notes)

All four of the first list landed in the second wave (lever 5 as kernel patch 0030 + the daemon
half; the fsync route; fresh-file publish; the read side). What follows:

1. The two-ring 4k randwrite QD32×4 regression (0.85×): a bounded spin / multi-CQE reap before the
   half-loaded worker parks; the pass boundary an all-own write crosses twice (an agent is on it).
2. `MetaSet.fsync` barriers every member (2.1 barriers per fsync): barrier the touched volume(s) or
   join the members' barriers.
3. The lay-down: the kernel's exclusive inode lock on extending O_DIRECT writes (≈ 4 GB/s per
   file) — a transport-side answer (FUSE's `FOPEN_PARALLEL_DIRECT_WRITES` posture, or the write's
   size extension decoupled from the inode lock in the sqz series).
4. 4k write IOPS beyond the two-ring fix: the write's three KV lookups and journal entry per op.

## Method notes for the next wave

- `scripts/perf/quiet.sh <session> <label>` holds the whole venue for a session; A legs from the
  main tree at `dev`, B legs from the worktree; A B B A; the 4k rows need ≥ 4 legs per side here.
- Owed from the wave: the zstd `CodecPool` unregistered with the budget; extracted/gate-declined
  writes still run on the CPU's lane (a follow-up A/B now that device waits are fiber-aware); the
  three per-publish copies on the journal lane and the `KvTx` box are still heap-per-op (9× rarer).
- Owner's inventory for the 200 G venue is still pending (see the 10-18 note).
