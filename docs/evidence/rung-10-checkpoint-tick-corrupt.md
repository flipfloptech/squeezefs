# Rung 10 — the checkpoint tick's `Corrupt` under a device that drops its writes (xfstests generic/311)

Decision 0003 §CL, 0002 row 566; commits `7398bd0` (red), `e763ed0` (fix).

## Scenario

generic/311 runs `fsync-tester` 80 times over dm-flakey: write + fsync, `_load_flakey_table
$FLAKEY_DROP_WRITES` (every write acknowledged and dropped — a power cut under the live mount),
unmount, the working table, remount, md5, unmount, fsck. Every run since at least 2026-10-24 logged
`error(squeezefs): kv checkpoint tick failed on …-meta: Corrupt` 2–4 times among its 162 scratch
daemons (one- and two-member sets, dev and branches); the test passed and `fsck --offline --scrub`
found 0. Each line was the daemon whose unmount ran under the drop: its shutdown tick (right after the
data teardown) and, when the cut spanned a cadence tick, that tick too. The volume was never latched
(`clean unmount (… healthy)`), and no ledger landed from a failed tick: the next mount read the last
pre-cut seq and replayed 14–17 entries — the consistent state fsck saw.

## Cause, with proof

A diagnostic build (error tracing on; the tick's catch and the SMO seam's catch logging the trace;
never committed) ran generic/311 on `0778bb4` and logged 4 failures, each the same path:

```
checkpointCycle → CacheFlusher.flush → SmoSeam.maintain → smoReplace (node 0x1fc0000, level 0)
  → writeSuccessor → tree.loadFresh → NodeCache.buildNode → node.verifyNodeExtentScreened
  → decodeHeaderPage refuses → smoReplace's catch: `.failed = .corrupt` → failureError → tick Corrupt
```

The SMO wrote its successor's image (acknowledged, dropped) and loaded it back OFF THE DEVICE: the
extent's old bytes do not verify. The frozen node failed every later tick the same way. The source had
the same flaw: `Source.load` folded the node's DEVICE log, and `foldSourceExtra` refused a disk walk
shorter than the object's append cursor whenever an append was dropped before the SMO (the merge red
below: `disk walk ends at 36864, the live object's append cursor is at 49152`); `KvTree.create`
loaded a fresh tree's first root back the same way. The tick did not misread a latched volume: the
device failed nothing; the mount read back what it had just been told was written.

## Fix

Charter §5.3 taken literally — successors build from a frozen snapshot, RAM is authoritative:

- `smo.Source` is the node's snapshot: every bset image its base indexes, in append order, the frozen
  delta's frame the newest (`FrozenDelta`'s image joins the base at the freeze; the base changes only
  at a load and a freeze, both on the serialized task). `smo.foldSources` folds the logs — the K1
  algebra, the elision rule at the durable tail, the cut's apply floor — for `smoReplace` and the leaf
  merge. `Source.load`, `foldSourceExtra` and its device-vs-RAM guards are deleted.
- `writeSuccessor` and `KvTree.create` build the node off the image they wrote (`tree.writtenNode`);
  `loadFresh` is deleted. `create`'s residue read (the extent's OLD bytes, the seq ceiling) stays.

Nothing in the structural path reads the device back; a dropped write is lost the way a power cut
loses it, and the next mount replays from the last durable ledger. Two 256 KiB reads per SMO go too.

## Red → green

| test | `0778bb4` + the tests (`7398bd0`) | `e763ed0` |
|---|---|---|
| unit smo `AN SMO NEVER READS BACK WHAT THE DEVICE ACKNOWLEDGED` (`RamLoader.drop_writes`; the cut between the last landed append and the SMO) | `expected .replaced, found .failed` — the load-back | pass |
| unit leaf_merge `A MERGE UNDER A DEVICE THAT DROPS ITS WRITES BUILDS OFF RAM` | `SMO fold-source log-view mismatch … Corrupt` | pass |
| smoke `A CHECKPOINT UNDER A DEVICE THAT DROPS ITS WRITES NEVER READS ITS OWN WRITES BACK` (400 synced names, the cut, 1,600 more) | 5 tick failures, 0 SMOs completed in the cut | 11–12 SMOs in the cut, 0 failures, fsck clean, the remount serves the 400 |

## Checks

| check | `0778bb4` | fix on `0778bb4` | fix rebased on `578d73c` |
|---|---|---|---|
| generic/311, one-member set | 2–4 `tick failed … Corrupt` a run | ×5: 0 tick failures, 0 error lines, fsck 0 both sets | ×1: 0, 0, fsck 0 |
| generic/311, two-member set (`SUITE_META_MEMBERS=2`) | 4 a run | ×5: 0, 0, fsck 0 | ×1: 0, 0, fsck 0 |
| generic/001 013 | — | pass, fsck 0 | — |

The alerts left in the 311 runs are `reclaim: vol N's device does not support bdev_discard`, one per
mount, as on the base. Smoke rows green: the cut row ×3, both POWER CUT rows ×2, THE FREED BLOCK ×2,
THE KILL MATRIX ×2, A FAILED VOLUME'S DAEMON, A FAILED REPLAY'S JOURNAL TAIL, THE REPLAY PROOF, kill -9
mid-tar ×2, KILL -9 BETWEEN A PROMOTION, THE KILL ROWS (a)(b)(c), THE ORDERED LAW'S KILL ROW. Units
`meta.` 1101/1104 ×4 (1112/1115 rebased), `ops.` 316/331, `fuse.` 367/386, `data.` 618/618; `zig build
fmt rails` green. No lock-free core and no durability law changed. The fix as first built was renamed
before landing (`tree.fromImage` → `writtenNode`: the fuzz census reads `from…([]const u8)` as an
untrusted decoder).

## Seen, not fixed (decision 0003 §CL.6)

- The SMO seam maps every non-device, non-space error to `.failed = .corrupt`, and two guest-root
  publish paths map an unknown class to `Corrupt`: the flattening that disguised this defect.
- `THE MOVE MATRIX — step 0 drained` failed once (`create answered PathAlreadyExists`) in 5 full
  `meta.` runs under the venue's concurrent gates; 0 in 30 runs of the row alone.
- The rebased two-member 311 pass: two unmounts waited out the 30 s residency backstop on all 96
  transport groups after the mount detached; 0 in the ten earlier passes.
- Two agents' fstests harnesses can race on the shared `local.config` (one void pass here).
