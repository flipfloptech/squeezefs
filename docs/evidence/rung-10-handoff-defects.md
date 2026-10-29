# Rung 10 — the handoff's owed defects (2026-10-29; decision 0003 §CI)

The owed items of the 10-20/21 notes, each red-first on `fix/handoff-defects` (off `7693bab`).
Rigs: loop images under `/build/tmp/hd-rig*-<pid>` (`~/tmp/hd/rig1*.{sh,py}`, `rig4.sh`, `loop4.sh`).

## 1. A full metadata volume refuses its mount (§CI.1)

**Scenario.** A 48 MiB metadata volume filled with xattr'd files (15,653 until ENOSPC), every 30th
unlinked, unmounted: the next mount refused, `the corpse sweep at mount failed (NoSpaceLeft)`
(rig1c). The live mount had logged `reclaim cadence: drain failed: NoSpaceLeft` per wake and
destroyed nothing. A 16/24 MiB volume refused at the data planes' arm (`Io`) with 0 extents free.
**Causes.** (a) Every clean mount of a symmetric set leaked one heap extent per data volume: the
holder's KVDA bitmap pages, claimed at the arm, whose lease record the clean stop deleted without
freeing them — 152 → 150 → 148 … per remount of an EMPTY 48 MiB set (1 per mount with one data
volume, none on `--single-writer`); a full volume crossed its compaction floor in four remounts.
(b) A destroy batch of scattered corpses is ONE member needing a compaction extent per leaf, never
admitted near the floor; the sweep returned the error (the mount failed) and the drain dropped the
batch. (c) Once destroys were retried corpse by corpse they took the room a user delete's
return-room cycles gave back: `rm` answered ENOSPC.
**Reds.** Smoke `A CLEAN REMOUNT GIVES BACK ITS BITMAP PAGES`: free extents per mount 987, 985,
983, 981, 979. Unit `a FULL metadata heap never fails the corpse sweep at mount` (the sweep returned
NoSpaceLeft), `a corpse batch the full heap refuses whole…` (the drain errored and lost its batch;
`MemStore.space_room` models the whole-member refusal). Smoke `A FULL METADATA VOLUME MOUNTS AND
RECOVERS BY DELETION`: nothing reclaimed after `rm -rf` (df -i 14,982 vs 1).
**Fix.** `releaseOwnPages` (a cycle, then the kind-4 frees); `destroyCorpses` defers what the heap
refuses (per corpse after a whole-entry refusal; the first refused alone and the rest stay
gathered; frees run for every destroy that landed); the sweep's `Swept{destroyed, deferred}`; the
reclaim commit's yield to a waiting user retry. **Green:** 987 ×5; 15,131 removed → 15,132
destroys, 128 deferrals, 4 yields, the heap left its full posture (20 free, 69 pending), 2,000 new
files fit, fsck clean, no corpse left.
**Found, not fixed.** A full volume killed −9 right after the scattered unlinks can refuse its next
open: `bring-up journal residue did not cover within 64 barriered cycles … the bring-up checkpoint
of the replayed window failed (Corrupt)`, the flush pass at 0 free (`rig1d.py FSCK_AFTER_KILL=1`;
before (a)'s fix the same kill left 4/158 free and deletes wedged at the floor). df -i after a
remount over-reports by design (`inos.liveInodes`: minted − destroyed THIS mount).

## 2. `clean unmount (healthy)` after a failed final checkpoint (§CI.2)

**Cause.** `close()` logged the clean line after `dev.barrier() catch {}` whatever the final tick
did; a checkpoint write never latches. **Red.** Unit `a dismount whose final checkpoint fails is no
clean unmount` (seam `device_loader.test_seams.fail_ledger_writes`): `clean_unmounts` +1.
**Fix.** `finalTickFailed` + the barrier's answer → `closeJudged`: `UNCLEAN unmount — the final
checkpoint FAILED, the final barrier landed (… tail N)`, `unclean_unmounts`, exit 1. generic/753's
row now expects exit 1 for its never-latched cycle (`1 unclean without a latch`).

## 3. fsck has no directory-nlink arm (§CI.3, 0002 row 564)

**Red.** RamSet: `dir` 7 vs 4, `sub` 2 vs 3 → no finding offline or online. **Fix.**
`Census.dir_nlink`, `DentryPass.subdirs` (per parent's global ino, before the shard filter, stripe
markers excluded), `c10_dir_nlink`, the witness re-check, `set-dir-nlink-to-counted-subdirs`.
**Green:** both found in both modes, repaired to 4 and 3; the striped-directory, RENAME_EXCHANGE,
ONLINE FSCK and guest-cut rows clean.

## 4. The pending-free wedge under HARD BACKSTOP — not reproduced (§CI.8 a)

mdtest -np 64 (10,000–20,000 files per rank) in a `systemd-run` cage of 900 MiB–1.1 GiB (budget
700–950 MiB), 12–24 `yes` hogs beside other agents' gates: 22 runs, `mem_budget_hard_backstops` 1
in every run, `volume_failed` false, no `wedged` line. The audit counts barriered CYCLES, not time
(`SpaceAudit`, 8 stalled cycles ⇒ FAILED); a tail pinned by a slow in-flight window across 8
rapid cycles is the suspect, not shown.

## 5. The overlay's serve borrows its record (§CI.4)

**Red.** Unit `A SERVE OWNS THE RECORD IT READS` (seam `serve_hold`): block 0's serve parked after
its snapshot, block 0 drained, block 1 stored — the LIFO slot went to block 1 and the serve
answered `served` with block 1's bytes. **Fix.** The reader hold under the registry lock;
`overlay_retires_behind_readers`. **Green:** `settle`, distinct slots; device_overlay 66/66.

## 6. `block_claim_anomalies` — not reproduced (§CI.8 b)

Its one firing (10-19, `vol 1 block 192 found refcount 1`) postdates the give-back fix `2118f55`.
`abandonUnpublished`'s refused give-back restores refcount 1 only when the window does NOT hold the
block; if the window already held it ("already unconsumed"), a claim from the window could meet
that count — a double-source shape no path is shown to produce. No seam red yet.

## 7. The stripe migration nlink and `PromoteSink.onEvict` (§CI.5, §CI.6)

**Reds.** mkdir hot/sub, flip, rename sub across stripes → D 3, old stripe 2, new 3, the fold 4.
Key 1 appended and key 2 overwritten in place, `evictUntil(0)` → 0 promotions. **Fixes.**
`MigrateCounts` (D −1, the stripe +1, no ctime, under the guards); the sink judges presence.
**Green:** 2 / 2 / 3 / 3; 2 promotions, both gone from ring and cache.

## 8. The in-place drains' lines (§CI.7)

`afd1649` had put the class under the set's one-line law, but every line logged while the set was
healthy, and generic/753's row on this branch logged two after the latch in cycle 7. **Red.** 40
in-place DMA failures on a healthy set → 40 lines. **Fix.** `LogClass.oncePerMount`. **Green:** 1.
