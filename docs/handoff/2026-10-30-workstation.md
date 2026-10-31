# Handoff — 2026-10-30

`dev` = `origin/dev` = `902137c`. The venue is quiet: no daemon, no mount, no venue slot held. The host
runs `7.3.0-1.7-mainline-sqz`. Every landing below had its own green full gate on the local venue; the
gate train was not run (owner ruling 2026-10-28 (4)). Decision ledger 0003 runs through §DA.

## What landed since the 10-29 note (red first, then the fix; evidence in `docs/evidence/`)

- **The three branches the 10-29 note paused:** the pass-recall wedge (§CM: a futex wake wrote a fiber's
  link after the bucket lock; the landed shape collects the taken fibers under the lock and wakes them
  after it — the first shape, its CAS under the lock, cost −12 % on plain np32 mkdir; the batch shape's
  A-B-B-A over two rounds is −1.4…+2.7 % on every row), the orphan supervisor plus the handover ack
  (§CV), the defrag job hold seam (§CO.26–30).
- **FALLOCATE — EVERY MODE'S LAW** (the 10-29 note's open defect): a deleted file's destroy waited on its
  own corpse as supply; a regression from `7698b38` (§CW).
- **THE KILL ROWS (a)** (3 of 29 red under load): the empty body is charter §5.2's power-cut window on an
  unsupervised B — the row now runs B strict (§CX); the successor's `Corrupt` unmount was a product defect —
  a JoinAppender never retired the predecessor's death record, so the manager recovered the live
  successor's region; it now retires it once that recovery is settled on every volume, else Deferred (§CY).
- **A JoinAppender adopted another incarnation's Live page** (§DA): pages now match the whole identity;
  another incarnation's join is Deferred until its recovery; a predecessor proven alive on this host
  (`/proc` writer-id proof) is refused at once with Busy.
- **generic/103's dismount promotion FAILED** (the 10-29 note's open defect): reservations keep the write
  cache's claim covered and `df` rounds the claim to a block (§CZ).
- **The §BO no-EAGAIN row's `hwm > 64`** held deterministically by `SQUEEZEFS_TEST_BARRIER_HOLD_FSYNCS`
  (§CO.31–33); it had turned a gate red at 54.

## In flight

Nothing. No paused branch, no agent.

## Open defects (no owner yet)

- The other ino-keyed std maps with removals (read cache `InoMap`, pending times, `kv_fs_stripes`).
- The reclaim tick starves its other work during a long removal storm; `transport_lease_overlong` at load
  ~50; the 30 s unmount backstop seen once (§CS); §CI.8 not reproduced.
- After 4 mdtest runs on one mount the dismount promotion takes 29–122 s, past `squeezefs umount`'s 32 s
  wait (§CM.12 (a)).
- §CZ.9: `promoteToStriped` takes a block unjudged; a striped write can take the block the claim counted on;
  the job movers hold one extra block per move.
- §DA.8: `leaveAppender` screens by (node, mount slot); a second mount at the MANAGER's own mount point is
  not refused (it joins as another appender and mounts over the first — no data risk; a mount-table check
  before the open would cover it).

## Owed to the owner

- The 10-29 note's items still stand: the 7.3 patches (option 1 recommended), RDMA, the `~/tmp` sweep, a
  LICENSE.
- §CW.5: a deleted file's parked block outlives its destroy (its dead bytes upload once space returns) —
  discard at the destroy?
- The host: every unplanned end of a 7.3 mainline boot since 10-21 was unclean (10-26 and 10-27 are failed
  s2idle resumes); `/usr/local/sbin/sqz-thermal-governor` builds its thresholds before it sources
  `/etc/default/sqz-thermal-governor`, so the tuned 76/82/88/94 °C ladder is ignored (84/88/92/96 runs).

## Resume

1. `scripts/venue status`; `mount | grep squeezefs` empty.
2. Pick from the open defects; one full gate per landing.
3. Benchmarks: AC power, a quiet host (no agent loops — ask them to pause), `~/tmp/abba-*.sh` under the gate
   lock and every slot; build both arms with `-Dprofile=dist --prefix zig-out-perf dist shim`.
