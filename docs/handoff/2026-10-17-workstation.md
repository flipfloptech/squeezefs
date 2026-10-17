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
- **xfstests `-g auto`** (xfstests master 22348af, 85 min): 779 ran, 539 not run, **70 failed**, both sets
  fsck-clean. The pass predates the last two fixes, so most of it is explained: **49** are dm-flakey remounts
  refused on the zombie holder (`6141de3`); 5 are the ledger's by-design single-writer rows (409–411, 589, 732);
  192 is the ledger's `noatime` row; 650 and the scsi_debug tests (704, 731, 803) are now venue-excluded.
  **Left to triage (next session, after the reboot; re-run the whole pass):** 294, 306 (`mount -o remount`
  through util-linux 2.42's fsconfig: "fuse: No changes allowed in reconfigure" — the helper's remount
  branch), 338 (util-linux hands `strictatime` to the helper as an option; the daemon refuses it), 441, 452,
  475, 484, 651, 743, 753, 798 (unread). The first two passes of the day were void: a `check` that survived a
  stop fought the next run for the loop devices (fixed: `592fba4`), and udiskie auto-mounted every loop
  device (`0787032`).
- **The host needs a reboot before anything else runs here**: xfstests `generic/650` (CPU hotplug stress)
  offlined 8 of 32 CPUs at 17:28 and they refuse to boot again (`CPUN failed to report alive state`);
  `nproc` reads 24 and every nvme fabrics connect answers EXDEV (gate 12's two nvmet reds). Omarchy's
  Limine menu still has the stock kernel as a fallback; the sqz kernel is the default entry.

## Flakes (`docs/gates/flakes.md`)

Fixed this week and waiting on the train's three greens: KILL ROWS (b), ONLINE FSCK, THE VIRTUAL INODES,
`ops.fsck.scale`, THE INTERCEPTION HOST + row 4b, THE SHARED HOT DIRECTORY, the park ladder, kmbuf 0029,
the storm/routing-loop rows. Still open, none reproducing here (60 six-wide runs each):
- `-o ro` READER (C's teardown / ESTALE fail-stop) and THE TWO-DAEMON MATRIX wedge (VM-only so far): both
  now get a full capture (kernel AND user stacks of every thread) at the next sighting.
- A DEAD MANAGER HOLDING A STRIPED HOT DIRECTORY (`xv_cross_owner_steps_rejected` after a failover; one
  kamrui sighting).
- THE MOVE MATRIX `HolderUnresolved` (one loaded gate; its deadline is T_park_max ≈ 90 s, so it needs a
  stalled host).

## Open items

1. The clock cuts (`57d9090`, `fa3fe60`) stay reverted until the two-daemon wedge is named (now capturable).
2. aarch64 coverage needs a second venue (the Mac VM or an arm64 box): `GATE_TRAIN_VENUES`.
3. Owner rulings pending from `rung-10-seqwrite.md` (fiber-aware device waits; zero-copy big writes).
4. The attrs-only INVAL_INODE the write-back timer's landings still send to the local kernel (76 per
   linkat02 run) are by design (the kernel did not ask); worth a look only if GETATTR counts matter.

## Resuming here

1. Reboot (the offline CPUs); `nproc` must read 32. `scripts/dev/provision-local-venue` (`ready`); `scripts/venue status`.
2. `scripts/gate-train` for the three consecutive greens (`.gate-train/ledger-v2.tsv`); it retires the
   flakes rows marked fixed.
3. `SQZ_VENUE_SLOT=1 scripts/venue 'SUITE_RUNS=1 scripts/suites/fstests.sh'` (~1.5 h) and triage what is left
   (above); when the three suites pass once, the release gate wants three clean runs each (`SUITE_RUNS=3`).
   To stop a suite: `sudo pkill -TERM -f scripts/suites/<suite>.sh` — its trap takes `check`, the daemons,
   the loops and the mounts down; verify with `pgrep -af '\./check'`, `losetup -a`, `grep fuse.squeezefs /proc/mounts`.
4. A single row: `scripts/venue 'zig build --cache-dir "$SQZ_BUILD_ROOT"/zig-cache-v2 --prefix zig-out-linux smoke "-Dtest-filter=<row>"'`;
   a hunt is six of those in parallel in a loop (keep the first failing log).
5. Kernel-side FUSE diagnosis works on this host: tracefs tracepoints `fuse:fuse_request_send/end` and
   kprobes on `fuse_notify` (code=%si), `d_invalidate`, `__detach_mounts`, `r:fuse_dentry_revalidate`.
