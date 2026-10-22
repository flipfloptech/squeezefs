# fstests (xfstests) — adjudicated exceptions

The ledger `scripts/suites/common.sh`'s `suite_judge` reads for `scripts/suites/fstests.sh`
(charter §11 "Gates": the release gate runs the full fstests `-g auto` with adjudicated by-design
exceptions ONLY; `_notrun` rows are not failures). A failing test whose id is not a row here
fails the gate; a row whose id did not fail in the current run is reported stale and fails the
gate (exit 3).

Ids are xfstests' own (`generic/003`). Tests excluded on the venue for reasons that are not a
filesystem semantic (a host hazard) live in `scripts/suites/fstests-venue-exclude.txt` with
their evidence, never here. `reason-class` ∈ {`noatime`, `thin-provisioning`,
`i64-timestamp-range`, `by-design-<name>`}; a `by-design-<name>` row cites the charter section
or decision-0002 row that RULES the behaviour (audit 2026-10-22,
`docs/evidence/rung-10-adjudication-audit.md`; owner ruling 2026-10-22: no adjudication without
certainty that the behaviour is not a bug, a defect or a missing capability).

| test-id | reason-class | evidence |
|---|---|---|
| generic/003 | noatime | relatime semantics: "access time has not been updated after accessing file1/2/3" — as generic/192, atime is never stamped by a READ (charter §11's `noatime` class). The test ran for the first time under `LIBMOUNT_FORCE_MOUNT2=always` (its `_require_relatime` mounts with `-o relatime`, which the new mount API refused on a FUSE reconfigure and the venues before 2026-10-17 reported `_notrun`) |
| generic/192 | noatime | atime is never stamped by a READ: the data plane's read ladder (charter §4 try-only warm ladder, zero-copy serves) performs no inode write per read, and the kernel's time writeback to a FUSE daemon carries `FATTR_MTIME` (+ `FATTR_CTIME`), never atime (`fs/fuse/dir.c` `fuse_flush_times`) — the charter's adjudicated `noatime` class (§11); the test measures `delta1 == 0` where it expects 5–7 s |
| generic/441 | by-design-fail-stop-metadata-eio | the test loads a dm-error table under SCRATCH_DEV — the set's one device — then writes again on every fd expecting the buffered writes to succeed and only the fsyncs to answer EIO, once each. A metadata I/O error is terminal by the charter (§1.8 fail-stop lattice: repeated journal-barrier failure latches the volume FAILED, EIO until remount) and a write-through FUSE write surfaces it at write(2): "Second write on fd[0] failed: Input/output error" |
| generic/484 | by-design-fail-stop-metadata-eio | as generic/441 for syncfs: the test expects ONE syncfs EIO after the dm-error table and clean syncfs calls after the working table is reloaded; the fail-stopped volume answers EIO to both (charter §1.8: EIO until remount) |
| generic/798 | by-design-write-through | cachestat(2) over the page cache: the test's own header says "any mount option that writes data back synchronously should be rejected, or it will screw up the dirtied page reporting"; a FUSE daemon without the kernel's writeback cache is exactly that — the release posture does not advertise `FUSE_WRITEBACK_CACHE` (`-o writeback` is the opt-in; charter §6.1 / §1 item 10: the daemon's RAM ring IS the write cache), so a write reaches the daemon synchronously and the kernel holds no dirty page ("Dirty: 0" where the test expects 1 and 2). The charter has no sentence naming the kernel-cache posture itself (audit 2026-10-22: the owner may add one to §4) |
| generic/409 | by-design-single-writer-guard | the `mount` group mounts SCRATCH_DEV a second time under TEST_DIR (then binds it): under the `--single-writer` class the suites format (`common.sh` `SUITE_SINGLE_WRITER=1`) a second `mount` of one metadata volume is a second DAEMON, refused by the guard (charter §5.7) — ext4 shares one superblock across mountpoints, a flat squeezefs volume has one writer; the stderr line is `another writer holds the single-writer guard`. Under the DEFAULT symmetric format the second mount JOINS (§5.7: "a later RW mount JOINS it"), so this row is the suites' posture, not a law of the product — audit 2026-10-22, the owner's call (`SUITE_SINGLE_WRITER=0` runs the suites on the default) |
| generic/410 | by-design-single-writer-guard | as generic/409 (the `mount` group's second mount of SCRATCH_DEV under TEST_DIR, then the bind propagation) |
| generic/411 | by-design-single-writer-guard | as generic/409 (the `mount` group's second mount of SCRATCH_DEV under TEST_DIR, then the bind propagation) |
| generic/589 | by-design-single-writer-guard | the `mount` group mounts SCRATCH_DEV a second time (then moves/binds the mountpoints): one flat volume has one writer daemon (charter §5.7); `another writer holds the single-writer guard`. As generic/409: the default symmetric format would join |
| generic/732 | by-design-single-writer-guard | mounts SCRATCH_DEV at two mountpoints at once (`SCRATCH_MNT=$testdir1 _scratch_mount; SCRATCH_MNT=$testdir2 _scratch_mount`) and renames through one while reading through the other: under `--single-writer` the second mount is a second daemon, refused by the guard (charter §5.7). As generic/409: the default symmetric format would join, and the test would judge cross-mount coherence (§5.5) |
