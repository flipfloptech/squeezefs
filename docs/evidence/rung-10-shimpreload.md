# Rung 10 — the shim smoke's silent battery failure: a truncate reverted by the times drain

**The red.** The gate train's VM run on `e540b77` failed `shim.smoke` "the LD_PRELOAD battery" at
row 2 (a plain mount, the shim passthrough): the battery script exited 1 with empty stdout and
stderr (`.gate-train/vm-e540b776a1b3-20261016T184139Z.log`). Kamrui passed the same commit.

**Diagnostics (test-only).** `preloadOk` now prints the cwd, the script, the last 4 KiB of its
stdout and stderr, and what the daemon logged while the script ran. The battery's two silent
`test`s name themselves: the sha check prints f/g/h hashes, sizes and `cmp`; the truncate check
prints the size `stat` read.

**Repro.** `.shimloop.sh` (the `shim-smoke` step in a loop) and a single-mount rig running the
battery on one mount (or a fresh format + mount per iteration, the gate's shape), under `yes`:

| Venue | Shape | Load | Runs | Fails |
|---|---|---|---|---|
| kamrui | `shim-smoke` step | 12 × yes | 30 | 1: row 4b, a second defect, see below |
| kamrui | rig, one mount / fresh mount | 12 × yes | 120 / 60 | 0 / 0 |
| VM | `shim-smoke` step | 4 × yes | 25 | 0 |
| VM | rig, one mount | 4 × yes | 299 | 0 |
| VM | rig, fresh mount | 8 × yes | 129 | **1**: `FAIL truncate -s 3M: stat says 67108864; again 67108864; after 2s 67108864` |

The size stayed at 64 MiB after 2 s, so the daemon's durable size was wrong. The kernel's attr
cache was not the cause.

**Cause (product).** `setattr(size)` folds the parked size claim (64 MiB, from `cat g > h`'s
extending writes) and commits the new size. The commit hands its 4a guard to the conveyor, which
drops it at the terminal outcome. Only after that did setattr call `pending.retireAll`. A times
drain (the pending-times drain, e.g. the write-back timer's) that took the guard in that window
re-read the still-parked claim and journaled it: `drainDelta` stages size whenever
`claim > stored.size`, so the truncate was reverted. The window is the setattr thread's wakeup
after its commit, so it opens under load: the VM inside a gate. The O_TRUNC open had the same
shape. The unconditional `retireAll` also had a twin bug: it dropped a claim that a write had
parked after the commit landed.

**Red** (`f0c077e`, `kv_fs`): the fake's (3.7) layout-lock release runs the times drain inside
the window. Result: `expected 100, found 16384`.

**Fix** (`6740df8`): `PendingTimes.clipClaim(ino, size)` clips the parked claim to the new size
under the 4a guard, before the commit. After the commit, `retireIfStill(after)` retires the claim
only if it is still as it was left, so a newer write's claim survives. A failed commit re-parks
`before`. The fix covers setattr and the O_TRUNC open; `retireAll` is deleted. Green: the window
pin; kv_fs + economy 112/112 (kamrui).

**After.** VM fresh-mount rig (8 × yes): 232 of 232 (before: 1 of 129). Full VM gate (`-Dfresh=true`): 3,000 of 3,084 passed, 83 skipped, 1 failed. The failure was smoke "`volume enable-symmetric`": after B joined, `umount A` reported "daemon … has not exited 32 s after the signal". That is the open two-daemon teardown wedge (flakes row "THE TWO-DAEMON MATRIX", owner matrixwedge), here in a sibling row, and it is unrelated to this change.

**Second defect (not fixed here; flakes row).** Kamrui, 1 of 30 runs: on the interception mount,
the C program's `fstat` right after ring `pwrite`s read the pre-write size (`shim_c_program.c:29`).
Probable cause: `service.zig` publishes the ring write's completion (`c.complete`) BEFORE
`inval.onWrite` enqueues the attrs-only `FUSE_NOTIFY_INVAL_INODE`, and the sideband writes it
fire-and-forget (`il_daemon.md` §3.3). A kernel-routed `fstat` / `lseek(SEEK_END)` inside the
attr TTL therefore reads the cached size, which breaks POSIX-8's stated purpose. Fix direction:
an attrs-only notify that is written (it touches no page) before the completion of a growing
ring write.
