# LTP — adjudicated exceptions

The ledger `scripts/suites/common.sh`'s `suite_judge` reads for `scripts/suites/ltp.sh`
(charter §11 "Gates": the release gate runs the full LTP with adjudicated by-design exceptions
ONLY). A failing test whose id is not a row here fails the gate; a row whose id did not fail in
the current run of its runtest file is reported stale and fails the gate (exit 3).

Ids are `<runtest-file>/<tag>` (`fs/fs_fill`, `syscalls/utimensat01`). Only failures that PASS
with their TMPDIR on the venue's ext4 reach this ledger: a test that fails on ext4 too is the
venue's (container limits) and is listed in `scripts/suites/ltp-venue-skip.txt` with its control
run, never adjudicated here. `reason-class` ∈ {`noatime`, `thin-provisioning`,
`i64-timestamp-range`, `by-design-<name>`}; a `by-design-<name>` row cites its decision 0002 row.

| test-id | reason-class | evidence |
|---|---|---|
| syscalls/epoll_ctl06 | by-design-fuse-poll | `epoll_ctl(ADD)` on a regular file must EPERM; the kernel's FUSE module gives EVERY file `fuse_file_poll` (no_poll ⇒ DEFAULT_POLLMASK), so it succeeds on any FUSE filesystem — the kernel module's shape, outside the daemon |
| syscalls/fallocate03 | by-design-no-fallocate | `fallocate(2)` is outside the must-implement set (decision 0003; the rung-4 ops answer ENOSYS → EOPNOTSUPP); xfstests `_notrun`s 31 tests on the same absence. Named in the evidence note as a feature gap, not a defect of implemented behaviour |
| syscalls/mremap06 | by-design-no-fallocate | the test's setup is `fallocate()`; as fallocate03 |
| syscalls/ioctl_loop05 | by-design-ltp-pseudo-fs-policy | LTP's `tst_device` refuses to back a loop device with a file "on an unsupported pseudo-file system" — its classification of every FUSE mount; the test never reaches a filesystem op |
| syscalls/setxattr03 | by-design-no-ioctl | the setup sets the IMMUTABLE inode flag through `ioctl(FS_IOC_SETFLAGS)` (chattr +i) and breaks on ENOTTY: `ioctl` is a rung-4 op outside the must-implement set (`filesystem.zig`: its ENOSYS default is the kernel's ENOTTY), as fallocate03's fallocate; the test never reaches its `setxattr` on an immutable file. New in LTP 20260930 (the Omarchy workstation's first pass; the VM's and kamrui's LTP clones predate it) |
