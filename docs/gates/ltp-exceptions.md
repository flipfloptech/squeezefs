# LTP — adjudicated exceptions

The ledger `scripts/suites/common.sh`'s `suite_judge` reads for `scripts/suites/ltp.sh`
(charter §11 "Gates": the release gate runs the full LTP with adjudicated by-design exceptions
ONLY). A failing test whose id is not a row here fails the gate; a row whose id did not fail in
the current run of its runtest file is reported stale and fails the gate (exit 3).

Ids are `<runtest-file>/<tag>` (`fs/fs_fill`, `syscalls/utimensat01`). Only failures that PASS
with their TMPDIR on the venue's ext4 reach this ledger: a test that fails on ext4 too — on the
SAME assertions, run from kirk's cwd (`ltp.sh` `ltp_control_cmd`) — is the venue's (the
kernel's, the host's) and is reported per run in `ltp-<file>-run<n>.venue`, never adjudicated
here; a test that never touches the mount and endangers the venue's co-tenants is listed in
`scripts/suites/ltp-venue-skip.txt` with its evidence. `reason-class` ∈ {`noatime`,
`thin-provisioning`, `i64-timestamp-range`, `by-design-<name>`}; a `by-design-<name>` row
cites the charter section or decision-0002 row that RULES the behaviour — a row without one
is an OWNER-CALL and says so (audit 2026-10-22, `docs/evidence/rung-10-adjudication-audit.md`;
owner ruling 2026-10-22: no adjudication without certainty that the behaviour is not a bug, a
defect or a missing capability).

| test-id | reason-class | evidence |
|---|---|---|
| syscalls/epoll_ctl06 | by-design-fuse-poll | KEEP (the kernel's FUSE module, outside the daemon). `epoll_ctl(EPOLL_CTL_ADD)` on a regular file must answer EPERM; the kernel refuses only a file whose `f_op->poll` is NULL (`fs/eventpoll.c` `file_can_poll`), and FUSE gives EVERY regular file `fuse_file_operations.poll = fuse_file_poll` (`fs/fuse/file.c`; a daemon's ENOSYS to FUSE_POLL only latches `no_poll` and makes it answer DEFAULT_POLLMASK) — so the ADD succeeds on any FUSE filesystem, as it does for libfuse's. Directories have no poll op and pass (24 of 25 pass; by hand 2026-10-22 on this host: `TFAIL: epoll_ctl() on file succeeded`, ext4 25 pass). No INIT flag withdraws poll; nothing daemon-side changes it. No charter line records it: the owner may add one to §3 (the platform) |
| syscalls/fallocate03 | by-design-no-fallocate | OWNER-CALL — a feature gap, no ruling. Linux: `fallocate(fd, 0, off, len)` allocates (zero-reading) space and extends `st_size` past `off+len`; `FALLOC_FL_KEEP_SIZE` allocates without extending (`fallocate(2)`). We answer ENOSYS to FUSE_FALLOCATE (`filesystem.zig`'s default; the kernel reports EOPNOTSUPP): by hand 2026-10-22 all 8 cases `EOPNOTSUPP`, ext4 8 pass. The charter rules nothing about fallocate; `docs/porting/fuse_abi_session.md` §6 lists `fallocate` in "What the daemon MUST implement" (the reference served it, `fuse_handlers_lanes.md` R4 row), so the earlier citation "outside the must-implement set (decision 0003)" was wrong (decision 0003 has no fallocate ruling). Also hides 31 xfstests behind `_require_xfs_io_command falloc` (`_notrun`). Fix item (mechanism in the audit note): modes 0 and KEEP_SIZE under the inode's exclusive guard with a size-epoch bump, PUNCH_HOLE as a block free + zero coverage, the rest EOPNOTSUPP |
| syscalls/mremap06 | by-design-no-fallocate | OWNER-CALL — the test's setup is `fallocate()` (`mremap06.c:110 TBROK: fallocate() failed`); as fallocate03 |
| syscalls/ioctl_loop05 | by-design-ltp-pseudo-fs-policy | KEEP (the test's precondition, not a filesystem semantic of ours). The test attaches a loop device to `test.img` on TMPDIR and then asks LTP's `tst_find_backing_dev` for the BLOCK DEVICE under that filesystem to read its `BLKSSZGET`; a filesystem whose `st_dev` has major 0 is refused "resides on an unsupported pseudo-file system" (`lib/tst_device.c:857`; btrfs and overlay are special-cased, nothing else). Every FUSE superblock is anonymous (`fs/fuse/inode.c` `sget_fc(…, set_anon_super_fc)` / `get_tree_nodev`) — as tmpfs and NFS are — so the test never reaches a filesystem op. By hand 2026-10-22: `TBROK: …/test.img resides on an unsupported pseudo-file system`, ext4 8 pass. The loop device over a squeezefs file itself works (LTP's `.all_filesystems` tests format and mount one on every run) |
