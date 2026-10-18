# Handoff — 2026-10-17: the Omarchy workstation is the `local` venue; the flakes and the first class C pass

**Checkpoint:** `dev` at the commit that adds this note; everything pushed to `origin` (the hook).
The venue is THIS host (`SQZ_VENUE=local`): Omarchy (Arch), 32 threads, 109 GiB, kernel
`7.2.8-7-omarchy-sqz` (Omarchy's `linux-omarchy-bore` + the sqz series 0001–0029, `scripts/kernel/
patches-7.2/`), `/build` a btrfs subvolume, `/build/tmp` ext4 on zram (the test images in RAM).
`scripts/dev/provision-local-venue` reports `local venue: ready`. A full gate takes ~7 minutes here.

## What landed since the move checkpoint (`0aa6527`), in order

| commit | what |
|---|---|
| `7ab95bf` `b7d8d26` | provisioning knows pacman; the kernel series lives in the repo; `build-host-kernel --local` builds the Omarchy PKGBUILD + the series (`--prepare-only` after an Omarchy bump) |
| `bfa89ec` `e3e851e` | the suites harness knows pacman (no lock: rolling release, owner ruling); `/build/tmp` in RAM; the writer guard reads the mount device off mountinfo (btrfs's per-subvolume `st_dev` made every flock invisible) |
| `3fc458a` | a wire joiner's extent pool is ceilinged (KILL ROWS (b): B hoarded 919 of 990 extents) |
| `1248f90` `f8fa6e7` | the SMO double free on a refused admission (BOTH `ops.fsck.scale` and THE VIRTUAL INODES rows) + the C4b guest-root publish livelock |
| `a1ca828` | a reader's projection open re-points at the newest record (THE ONLINE FSCK row) |
| `0c0b267` | a ring write that grows a file announces its size before its ack (row 4b, THE INTERCEPTION HOST) |
| `cc604fa` | the node cache's retired mark names the lagging node's seq (THE SHARED HOT DIRECTORY row: a re-granted extent stayed `RetiredExtent` at the manager for ever); the park-ladder test's real-time bound and teardown order |
| `d4f46fa` | a wedged daemon's capture takes every thread's USER stack (SIGQUIT handler in the daemon; `printWedge` on A's and every peer's umount and exit wait) |
| `0787032` | a desktop host's udisks is kept off the suites' loop devices (udev rule, provisioned; the harness refuses without it) |
| `32e591a` | **a kernel request is kernel-origin by the lifecycle's flag** — a flat mount (no move gate) invalidated its own kernel's every name and inode: `d_invalidate` threw mounts off fresh directories (LTP linkat02/fanotify13/dirtyc0w_shmem), `getcwd` ENOENT (msgget01); 43,000 INVAL_ENTRY + 84,827 INVAL_INODE per linkat02 run → 0 + 76 |
| `c5c3464` `caf1c0d` `19c91b2` `592fba4` `2bf09b8` | suites harness: `suite_log` before the results dir; xfstests builds against glibc 2.43+ (locktest.c's `struct delegation` guard, the maintainer's pending fix applied at build time); the rebuild check sees `src/locktest`; a heredoc's backticks; the venue's `SQZ_*` exports never reach the daemon; a stopped harness takes `check`'s process group down; a dm target over the scratch loop resolves to the loop's set |
| `6141de3` | **a ZOMBIE holder is provably dead** (the supervisor reaps on its 5 s probe; a remount 50 ms after the unmount met "claimed by a live writer": every dm-flakey test); `setpaths.sh` refuses a block device with no backing file; `generic/650` and the scsi_debug tests venue-excluded; the mount helper waits for a holder only when its log says "stop requested" |

## Class C on this venue (`SUITE_RUNS=1`, `SQZ_VENUE_SLOT=1 scripts/venue 'SUITE_RUNS=1 scripts/suites/<suite>.sh'`)

- **pjdfstest**: 3 of 3 runs clean (238 files, 8,798 tests each, 9 suite-declared TODOs, fsck 0 findings).
- **LTP** (`fs` + `syscalls`, LTP 20260930, kirk): `fs` 71 pass / 1 conf; `syscalls` 1,411 pass / 128 conf /
  5 adjudicated (`epoll_ctl06`, `fallocate03`, `ioctl_loop05`, `mremap06`, `setxattr03` — the last is new,
  `docs/gates/ltp-exceptions.md`) / 7 the venue's (`clock_settime03`, `leapsec01`, `swapon*`/`swapoff*`:
  fail on ext4 too). PASSED after `32e591a`.
- **xfstests `-g auto`** (xfstests master 22348af): **PASSED** at 23:42 — 700 ran, 515 not run, 10 failed, all ten
  ledger rows (003, 192, 409–411, 441, 484, 589, 732, 798), both sets fsck-clean, 46 min. With the venue
  exclusions below in force. The road there, in order: a `check` that survived a stop fought the next run
  for the loop devices (`592fba4`); udiskie auto-mounted every loop device (`0787032`); dm targets resolved
  to `/dev/mapper` as the set (`2bf09b8`); the zombie holder (`6141de3`); `LIBMOUNT_FORCE_MOUNT2=always` and
  the atime words (`37662ec`); **the lane's hole checkpoint deadlock** (`09ca33c`, found by generic/753's
  dm-error soak loop); the dm-error tripwire scoping and the leaving-holder waits (same commit).
- **Venue exclusions** (`scripts/suites/fstests-venue-exclude.txt`): the 65 dm-flakey, 5 dm-log-writes and
  dm-error-on-data (743) tests were excluded 2026-10-17 because they cut the METADATA volume alone while the
  data volumes and the staging cache kept their writes. **Re-enabled 2026-10-18** (`c6e0f5b`): each set is
  ONE loop image the helpers carve with dm-linear into meta | d0 | d1 | an ext4 for the cache
  (`scripts/suites/setpaths.sh`), so the test's dm target cuts every byte of the set. Under it generic/475
  exposed the overlay settle ladder retrying a plain-EIO gap seed forever on a FAILED volume (`1fbb62d`:
  the ladder polls the plane's fence probe), and fsx exposed three staged-arm defects (`34ca652`,
  `482300c`, `c917653`). Still excluded: generic/650 (CPU hotplug: this platform's CPUs do not come back)
  and the scsi_debug tests (no backing file).
- **The flakey corpus needs a durability ruling**: with `SQUEEZEFS_FSYNC_PROMOTE_STAGED` off (the default)
  a staged file's fsync'ed bytes live in the staging ring's mmap, never msync'ed, so a dm-flakey cut loses
  them and generic/057 fails BY DESIGN of charter item 10 (kill -9 only); with the lever on (exported into
  the venue command) the whole validated subset passes. Options: run the suites with the lever on; msync the
  segment at fsync; or adjudicate the flakey rows. Until ruled, run xfstests with the lever on.
- **Open from the pass**: generic/112's size race (a `flakes.md` row; 1 of 3 passes).
- **Fail early, fail hard** (owner ruling): every suite stops at the first failing test that is not a ledger
  row (`eea8892`); the three suites take a stop (`sudo pkill -TERM -f scripts/suites/<suite>.sh`) down to
  their runners, daemons, loops and mounts.

## Flakes (`docs/gates/flakes.md`)

Retired 2026-10-17 after the train's three greens on `0d6633b` (`02f1589`): 20 rows. Open:
- `-o ro` READER (C's teardown / ESTALE fail-stop) and THE TWO-DAEMON MATRIX wedge (VM-only so far): both
  now get a full capture (kernel AND user stacks of every thread) at the next sighting; 0 of 60 six-wide here.
- A DEAD MANAGER HOLDING A STRIPED HOT DIRECTORY (one kamrui sighting; 0 of 60 here).
- THE MOVE MATRIX `HolderUnresolved` (one loaded gate; its deadline is T_park_max ≈ 90 s, so it needs a
  stalled host).
- xfstests generic/112's truncate-vs-landing size race (1 of 3 full passes; keep the results dir next time
  and replay `112.2.fsxops`).

## Open items

1. The clock cuts (`57d9090`, `fa3fe60`) stay reverted until the two-daemon wedge is named (now capturable).
2. aarch64 coverage needs a second venue (the Mac VM or an arm64 box): `GATE_TRAIN_VENUES`.
3. Owner rulings pending from `rung-10-seqwrite.md` (fiber-aware device waits; zero-copy big writes).
4. The attrs-only INVAL_INODE the write-back timer's landings still send to the local kernel (76 per
   linkat02 run) are by design (the kernel did not ask); worth a look only if GETATTR counts matter.

## Resuming here

1. `scripts/dev/provision-local-venue` (`ready`); `scripts/venue status`; `nproc` must read 32 (generic/650 is excluded; a CPU it offlines never comes back without a reboot).
2. `scripts/gate-train` (three greens on `0d6633b` were reached 2026-10-17 evening; the 20 fixed flakes rows retired in `02f1589`); re-run it on the current head.
3. The release gate wants three clean runs of each suite (`SUITE_RUNS=3`): pjdfstest has them; LTP and
   xfstests have one each (`SQZ_VENUE_SLOT=1 scripts/venue 'SUITE_RUNS=3 scripts/suites/<suite>.sh'`).
   To stop a suite: `sudo pkill -TERM -f scripts/suites/<suite>.sh` — its trap takes `check`, the daemons,
   the loops and the mounts down; verify with `pgrep -af '\./check'`, `losetup -a`, `grep fuse.squeezefs /proc/mounts`.
4. A single row: `scripts/venue 'zig build --cache-dir "$SQZ_BUILD_ROOT"/zig-cache-v2 --prefix zig-out-linux smoke "-Dtest-filter=<row>"'`;
   a hunt is six of those in parallel in a loop (keep the first failing log).
5. Kernel-side FUSE diagnosis works on this host: tracefs tracepoints `fuse:fuse_request_send/end` and
   kprobes on `fuse_notify` (code=%si), `d_invalidate`, `__detach_mounts`, `r:fuse_dentry_revalidate`.
