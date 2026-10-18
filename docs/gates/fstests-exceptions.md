# fstests (xfstests) — adjudicated exceptions

The ledger `scripts/suites/common.sh`'s `suite_judge` reads for `scripts/suites/fstests.sh`
(charter §11 "Gates": the release gate runs the full fstests `-g auto` with adjudicated by-design
exceptions ONLY; `_notrun` rows are not failures). A failing test whose id is not a row here
fails the gate; a row whose id did not fail in the current run is reported stale and fails the
gate (exit 3).

Ids are xfstests' own (`generic/003`). Tests excluded in the container venue for reasons that
are not a filesystem semantic (no loop devices, no dm, …) live in
`scripts/suites/fstests-venue-exclude.txt` with their reasons, never here. `reason-class` ∈
{`noatime`, `thin-provisioning`, `i64-timestamp-range`, `by-design-<name>`}; a `by-design-<name>`
row cites its decision 0002 row.

| test-id | reason-class | evidence |
|---|---|---|
| generic/003 | noatime | relatime semantics: "access time has not been updated after accessing file1/2/3" — as generic/192, atime is never stamped by a READ. The test ran for the first time under `LIBMOUNT_FORCE_MOUNT2=always` (its `_require_relatime` mounts with `-o relatime`, which the new mount API refused on a FUSE reconfigure and the venues before 2026-10-17 reported `_notrun`) |
| generic/192 | noatime | atime is never stamped by a READ: the data plane's read ladder (charter §4 try-only warm ladder, zero-copy serves) performs no inode write per read, and the kernel sends no SETATTR for atime on a FUSE mount — the charter's adjudicated `noatime` class; the test measures `delta1 == 0` where it expects 5–7 s |
| generic/441 | by-design-fail-stop-metadata-eio | the test loads a dm-error table under SCRATCH_DEV — our METADATA volume (the data volumes are files beside it) — then writes again on every fd expecting the buffered writes to succeed and only the fsyncs to answer EIO, once each. A metadata I/O error is terminal by the charter (§1.8 fail-stop: the volume latches FAILED, every mutation answers EIO until remount) and a write-through FUSE write surfaces it at write(2): "Second write on fd[0] failed: Input/output error" |
| generic/484 | by-design-fail-stop-metadata-eio | as generic/441 for syncfs: the test expects ONE syncfs EIO after the dm-error table and clean syncfs calls after the working table is reloaded; the fail-stopped volume answers EIO to both (EIO until remount) |
| generic/798 | by-design-write-through | cachestat(2) over the page cache: the test's own header says "any mount option that writes data back synchronously should be rejected, or it will screw up the dirtied page reporting"; a FUSE daemon without the kernel's writeback cache is exactly that — a write reaches the daemon synchronously and the kernel holds no dirty page ("Dirty: 0" where the test expects 1 and 2) |
| generic/409 | by-design-single-writer-guard | the `mount` group mounts SCRATCH_DEV a second time under TEST_DIR (then binds it): a second `mount` of one squeezefs metadata volume is a second DAEMON, refused by the single-writer guard (charter §5.5; decision 0002 row 7-class) — ext4 shares one superblock across mountpoints, we never do; the stderr line is `another writer holds the single-writer guard` |
| generic/410 | by-design-single-writer-guard | the `mount` group mounts SCRATCH_DEV a second time under TEST_DIR (then binds it): a second `mount` of one squeezefs metadata volume is a second DAEMON, refused by the single-writer guard (charter §5.5; decision 0002 row 7-class) — ext4 shares one superblock across mountpoints, we never do; the stderr line is `another writer holds the single-writer guard` |
| generic/411 | by-design-single-writer-guard | the `mount` group mounts SCRATCH_DEV a second time under TEST_DIR (then binds it): a second `mount` of one squeezefs metadata volume is a second DAEMON, refused by the single-writer guard (charter §5.5; decision 0002 row 7-class) — ext4 shares one superblock across mountpoints, we never do; the stderr line is `another writer holds the single-writer guard` |
| generic/589 | by-design-single-writer-guard | the `mount` group mounts SCRATCH_DEV a second time (then moves/binds the mountpoints): one squeezefs metadata volume has one writer daemon (charter §5.5); `another writer holds the single-writer guard` |
| generic/732 | by-design-single-writer-guard | mounts SCRATCH_DEV at two mountpoints at once (`SCRATCH_MNT=$testdir1 _scratch_mount; SCRATCH_MNT=$testdir2 _scratch_mount`): a second mount of one volume is a second daemon, refused by the guard |
