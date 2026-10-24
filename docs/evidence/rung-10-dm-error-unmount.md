# rung 10 — dm-error: a failed metadata read freed its buffer twice; a failed replay left dirty pages behind

**Finding (xfstests generic/753, 2026-10-23 run 21:50).** Two of 48 soak cycles printed
`squeezefs umount: … no squeezefs daemon serves it, or the daemon died`. The daemon had not
exited at its unmount. It died of SIGSEGV while its mount was still in the table. The
supervisor logged `daemon … exited (wait status 0x8b)` about 0.1 s after the volume latched
FAILED, and coredumpctl recorded the faulting threads as `sqz-fuse-ur27` / `sqz-fuse-ur0`. The
other 46 daemons exited by themselves (`exit 1`, `latched FAILED during this mount`) after
the kernel umount. That exit is legitimate: charter §1.8 keeps the mount and answers EIO until
the unmount.

**Defect 1 — `Pool.readAt` double free (`src/io/uring_fs.zig`).** `readAt`'s `errdefer
buf.deinit()` still covered the wait after the send. The reactor also frees a sent request's
`AlignedBuf` in every failure arm: the open refusal, a CQE error, a dead ring, and the queue's
close. So every failed node read freed its 256 KiB page-backed buffer twice. The second
`munmap` unmapped whatever had been mapped there in the meantime. In a reproduction under the
753 mix (fsstress over dm-error, crash at cycle 20) that was a FUSE worker's `listxattr` key
array: `kvs.items` at `0x7f51ae45c000`, the fault at `+0x14000`, and the next entry read as
`0x0707…`, which is fsstress xattr bytes. Fix: the caller frees the buffer only when the send
is refused.

**Defect 2 — the supervisor's failed replay (`src/meta/journal_tail.zig`).** The tail replay
writes to the device buffered. Under the error table a write is refused (`replay FAILED
(DeviceWriteFailed)`). The pages that already sat in the cache stayed dirty. The flusher
then failed their writeback with nobody watching. The successor mount's first barrier,
`fdatasync` after it reclaims the dead holder's claim, received that unseen error and
answered `device I/O failed` on a healthy table. A later writeback success could also have
landed old windows under the successor's O_DIRECT ring. Fix: a refused replay still `fsync`s
on the supervisor's descriptor, and every replay drops its page cache
(`POSIX_FADV_DONTNEED`).

**Red tests.**
- Unit test `a failed positional read frees its buffer ONCE`: a free census on the pool's
  allocator. On `7a6e224` it reports `expected 1, found 2` on the open-refusal arm.
- Smoke row `A FAILED VOLUME'S DAEMON SERVES EIO UNTIL ITS UNMOUNT AND NEVER DIES OF IT`: 8
  dm-error cycles under an xattr storm, then the working table, a mount, and fsck. On the
  unfixed tree it failed 2 of 12 runs, both with defect 2 (`cannot open metadata set … device
  I/O failed`). With defect 1 fixed alone it failed 3 of 20 runs, all with the same signature.
  The SIGSEGV is rare in this storm (0 of 96 cycles). Its deterministic red is the unit test.
- `squeezefs umount` of a path with no mount entry and no daemon now answers `not mounted`.
  Before, it took the dead-daemon fallback (the second `_dmerror_unmount` of the test's
  `_cleanup`).

| run | result |
|---|---|
| `7a6e224`, the 753 loop by hand (the test's fsstress mix, 4 procs, over a set stacked on one switchable dm device; error table without lockfs, kill, umount, working table, mount) | SIGSEGV at cycle 20 (core: `listxattr` → `XattrValue.decode`) |
| `7a6e224`, the unit test | red, `expected 1, found 2` |
| both fixes, the unit filters `umount uring_fs FAILED fail-stop latch supervisor` | 163 pass, 47 skip, 0 fail |
| both fixes, the smoke row × 20 under load (a second loop and the repro beside it) | 20/20; the second loop beside it 10/10 |
| both fixes, the 753 loop by hand × 100 cycles (beside both loops) | 0 deaths; every daemon alive at its umount and exited `0x100` by itself |
| both fixes, xfstests `generic/753 475 482 388` (`2dceace`) | Passed all 4 (753 and 475 ran; 388 not run: no shutdown; 482 not run: no `LOGWRITES_DEV`); 103 scratch daemons exited by themselves (100 × exit 1, 3 × exit 0), 0 signals, no `no squeezefs daemon serves it` |
| both fixes, the umount, replay and fail-stop smoke rows (supervise, raced unmount, replay proof, busy ×2, stop's sender, MOUNT-ID churn, bind, own connection, supervised kill, late refusal) | 55 pass, 1 skip (the late refusal: no kmbuf on mainline) |
| `zig build rails`, `zig fmt --check` | 18/18 steps, 32/32 tests; clean |

**Seen, not fixed.** (a) The dismount logs `clean unmount (… healthy)` and exits 0 even when
its final checkpoint and barrier failed under the error table: `backend.zig` logs it
unconditionally after `be.dev.barrier() catch {}`. Only reads and checkpoint writes failed,
and those never latch FAILED. (b) A metadata volume filled with xattrs refuses the mount:
`the corpse sweep at mount failed (NoSpaceLeft)`. (c) When the device is still failing at the
daemon's death, the acked windows in the tail are lost. That is §5.2's power-cut window, and
the supervisor line's wording matches it. Holding the tail for the successor, as the write
cache is held, would close it. That needs an owner ruling.
