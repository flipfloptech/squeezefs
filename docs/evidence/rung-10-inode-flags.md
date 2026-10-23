# Rung 10 — inode flags: IMMUTABLE and APPEND-only are laws, not hints

**Owner ruling (2026-10-23):** inode flags are in scope — "build it." The gap was
`rung-10-adjudication-audit.md` F2: `FS_IOC_GETFLAGS`/`SETFLAGS` answered ENOTTY, LTP
`setxattr03` sat in the ledger for it, and the record's `flags` word (charter §5.1) held STRIPE
alone. Venue: the Omarchy workstation (`local`), kernel `7.3.0-1.7-mainline-sqz`, branch
`feat/inode-flags` off `316460d`.

## What the kernel sends (linux 7.3, read in the tree, not remembered)

- Every `ioctl(2)` of the four commands is caught by the VFS (`fs/ioctl.c` `do_vfs_ioctl`
  → `fs/file_attr.c` `ioctl_{get,set}flags` / `ioctl_fs{get,set}xattr`) and routed through the
  inode's `fileattr_get`/`fileattr_set` ops — never the file's own ioctl op. FUSE registers both
  (`fs/fuse/dir.c` `fuse_common_inode_operations`), and `fs/fuse/ioctl.c` `fuse_fileattr_get/set`
  → `fuse_priv_ioctl` sends ONE `FUSE_IOCTL` per call: `fh` = a private open's handle (0 under
  zero-message opens; a directory's is an OPENDIR/RELEASEDIR pair around the call), `flags` =
  `FUSE_IOCTL_DIR` on a directory (`FUSE_IOCTL_32BIT` on a 32-bit kernel), `cmd` the NATIVE
  command (a compat caller's `FS_IOC32_*` is translated before the dispatch), `arg` 0,
  `in_size`/`out_size` = exactly the argument's size by the command's direction (4 for the flags
  word, 28 for `struct fsxattr`), the in bytes after the header. The reply is
  `fuse_ioctl_out{result 0, flags 0, 0, 0}` ‖ exactly `out_size` bytes (`fuse_simple_request`
  refuses any other length; a `FUSE_IOCTL_RETRY` is -EIO there); a header-only `-errno` is the
  refusal, returned to the caller as is (ENOSYS becomes ENOTTY — an unknown command).
- `vfs_fileattr_set` ALWAYS reads the old state first through `fuse_fileattr_get` with
  `fsx_valid`: a `chattr +i` arrives as FSGETXATTR then SETFLAGS, an `xfs_io chattr` as
  FSGETXATTR then FSSETXATTR. Both pairs are served and agree. The kernel owns the permission
  checks before the request reaches the daemon: `inode_owner_or_capable` (EPERM), and
  `fileattr_set_prepare`'s `CAP_LINUX_IMMUTABLE` for a change of IMMUTABLE or APPEND.
- The kernel never learns a FUSE inode's flags: `fuse_change_attributes_common` reads only
  `FUSE_ATTR_SUBMOUNT`/`FUSE_ATTR_DAX` from `fuse_attr.flags`, no `S_IMMUTABLE`/`S_APPEND` is ever
  set, so none of `may_open`/`may_setattr`/`may_write_xattr`/`may_delete`/`vfs_link`/
  `vfs_fallocate` fires for it, and `statx` fills no `stx_attributes` (`fuse_fillattr`,
  `fuse_statx_to_attr`). The daemon is the ONE enforcer.

## The law (charter §4 "Inode flags"; `src/fuse/handler/inode_flags.zig` is the one place)

Stored: `FS_IMMUTABLE_FL` → `record.INODE_FLAG_IMMUTABLE` (bit 1), `FS_APPEND_FL` →
`INODE_FLAG_APPEND` (bit 2), beside STRIPE (bit 0, never a user's). Durable, remount-stable;
a SET is one record Put with a new ctime under the inode's exclusive guard (the lifecycle takes
the whole-inode op guard for a SET, as for a truncate), a no-op set moves nothing; a
foreign-slot inode's SET is ONE shipped setattr verb carrying the attribute bits (`verbs.
SETATTR_FLAGS`; the holder replaces the attribute bits alone). Refused EOPNOTSUPP: every other
bit (SYNC, NOATIME, NODUMP, DAX, PROJINHERIT, …), a non-zero project id or extent-size hint —
the record holds no field the kernel would honour for them (a FUSE inode is `S_NOATIME` for
the kernel whatever the daemon says). GET answers the bits alone (`fsx_nextents`, the project
id, the hints: 0). Unknown commands stay ENOSYS → ENOTTY.

Enforced (EPERM), restating the kernel's own checks:

| op | IMMUTABLE | APPEND-only | where |
|---|---|---|---|
| WRITE | refused | `O_APPEND` (the WRITE carries the file's open flags) at the claimed EOF alone | `writeInline`/`writeStaged`/`writeStriped` on the record each reads first; the Shared class and the staged fast path never see a restricted inode (below) |
| OPEN (served opens) | write intent, `O_TRUNC` | write intent without `O_APPEND`, any `O_TRUNC` | `kv_fs.open` (`may_open`); under zero-message opens the first WRITE/SETATTR answers |
| SETATTR | everything (mode, owner, size, every time incl. the `touch` NOW forms) | mode, owner, size, EXPLICIT times; `*_NOW` and a ctime echo pass | `setattr` (`may_setattr`, `vfs_truncate`); `SetAttr.times_set` carries the kernel's `ATTR_TIMES_SET` |
| SETXATTR / REMOVEXATTR (the stripes flip command included) | refused | refused | `may_write_xattr` |
| UNLINK / RMDIR victim; RENAME source and replaced target; LINK source | refused | refused | `may_delete`, `vfs_link`; before the ENOTEMPTY probe, as the kernel orders them |
| an entry MADE inside the directory (create, mkdir, mknod, symlink, link, rename-in fresh) | refused | admitted | `may_create` (`inode_permission(MAY_WRITE)`) |
| an entry REMOVED from the directory (unlink, rmdir, rename-out, a replacing rename-in; an exchange both ways) | refused | refused | `may_delete`'s `IS_APPEND(dir)` |
| FALLOCATE | refused | preallocation alone (`mode & ~KEEP_SIZE` refused) | `kv_fs.refuseFallocate(global, mode)` — the hook the fallocate handler calls first (fallocate lands on a sibling branch) |
| the direct arm's WRITE lease (`-o interception=direct`) | refused (`write` class) | refused | `direct_lease.buildLease` reads the flags through the plane's view; a SET recalls standing leases (the op guard before the hold, `lease_epoch.recallWrites` after the commit) |

Striped directories: the stripe carries no flags of its own — the LOGICAL directory the caller
named rules, read once per op only when the key parent is a stripe (`refuseLogicalParent`).

**Zero cost on the hot path, exact.** The two write arms that read no record — the Shared
class (`writeStripedShared`, rung-10-write-iops: "no record read at all", pinned by its unit
test) and the staged fast path (`stagedPointCached`) — are structurally unreachable for a
restricted inode instead of paying a record read: the data plane's read view (`read_view.zig`)
carries the inode record's `flags` in a spare header word (`W_FLAGS`), filled from the same
record read that fills its size and staled by the ino witness on every record change (a flag
SET is a Put of the record, so the row dies with it); `plane.sharedProbe` declines Shared when
the view's row carries a flag or is missing (the exclusive arm reads the record and answers),
`plane.stagedPoint` fills no staged point for a flagged record, the promotion publishes no
shape row for one. An unflagged inode pays one `acquire` load more on the admission probe
(the view row it already has in L1). The foreign verdict carries the holder's flags too.

## Red first

`2ada529` — the smoke row "INODE FLAGS — IMMUTABLE AND APPEND-ONLY ARE LAWS, NOT HINTS" + the
pure `inode_flags.zig` module, run against the pre-implementation daemon: `chattr: Operation
not supported while reading flags on imm` (the kernel's ENOTTY for the unserved FUSE_IOCTL;
e2fsprogs' wording). Then the implementation.

Two bugs the red-to-green path found: (1) the first IOCTL reply with BOTH an out struct and a
window body aliased the ent payload (`@memcpy arguments alias`, a daemon panic): the reply is
`fuse_ioctl_out ‖ data` and the window IS the payload, so the data now lands at
`Ctx.window[IOCTL_DATA_OFFSET..]` and the commit moves nothing; (2) the open posture: on this
venue mainline's zero-copy arm SERVES regular-file opens, so a write-intent open of an
immutable file is EPERM at the open (the kernel-equivalent answer) — the row reads the posture
off the law and asserts both forms, and runs under both (`INODE FLAGS UNDER NO-OPEN` pins
`SQUEEZEFS_FUSE_NO_OPEN=1`).

## Numbers

- Smoke: both rows 20/20 under load (two concurrent loops of 10, four `yes` burners beside
  them, 28 s), and 41/41 with the two rows alone; the interposer rows (`THE INTERCEPTION HOST`,
  `THE DIRECT ARM`, 42/42) and `zig build shim-smoke` (41/41, the direct write arm's row
  included) green.
- Unit: `inode_flags` 4 tests; `kv_fs` +4 (the four ioctls; IMMUTABLE per handler; APPEND-only
  per handler + the fallocate hook; the served-open arm); `filesystem.SetAttr.times_set`;
  `read_view` flags word; `record_ship` flags verb. Filters kv_fs / ioctl / setattr / xattr /
  read_view / plane / record_ship / verbs / lifecycle / direct_lease / two_holders / wire_plane
  / record: 643 pass, 32 skipped, 0 failed (675). The Shared-class "reads no record" pin holds.
- `zig build rails` 29/29 (one stale allowlist row for the old `SETATTR_KNOWN` deleted);
  `zig fmt --check` clean.
- LTP `syscalls/setxattr03` by hand (kirk's environment, TMPDIR on the mount): **passed 2,
  failed 0, broken 0** — as ext4; its ledger row deleted.
- xfstests through the harness (`SUITE_RUNS=1 FSTESTS_TESTS=…`): **generic/079 PASS**
  (`t_immutable`: the whole immutable + append-only matrix — opens, truncate, utime, link,
  chmod, chown, ACLs, xattrs, rename, unlink, creates inside), **generic/545 PASS**
  (`chattr +ia` with and without CAP_LINUX_IMMUTABLE under capsh), **generic/553 PASS**
  (`copy_range` into an immutable file EPERM), **generic/555 PASS** (`xfs_io chattr +ia`
  under capsh: the FSSETXATTR pair); notrun 159/160 (reflink/dedupe), 277 (`+A`), 507
  (`+ASai`), 573 (fsverity), 596 (accton), 717 (exchangerange); **generic/424 FAIL** — `statx`
  must report `STATX_ATTR_IMMUTABLE`/`APPEND` and the FUSE kernel fills no `stx_attributes`
  (mechanism above; ledger row `by-design-fuse-statx-attributes`, an OWNER-CALL: the sqz series
  could carry the propagation). fsck 0 findings on every set, 0 daemon alert lines.

## Seen, not fixed

- `open(2)` for write of an immutable file SUCCEEDS under zero-message opens (the kernel asks
  no OPEN); the first write answers EPERM. Only a kernel that knows the flag can refuse the
  open itself — the same patch that would fix generic/424 (`FUSE_ATTR_IMMUTABLE`/`APPEND` in
  `fuse_attr.flags` → `S_IMMUTABLE`/`S_APPEND`, `generic_fill_statx_attr`), a series item for
  the owner. The served-open posture (mainline's default) has no such gap.
- A SET moves ctime; the kernel's cached attrs show the old ctime for up to the attr TTL (1 s)
  — `fuse_fileattr_set` invalidates nothing; xfstests checks it only under `+A` (277, notrun).
- The holder's record-ship door does not re-check the flags: a stale writer's shipped verb
  is caught at the writer (every ship is preceded by the writer's own check on the holder's
  folded record); a holder-side re-check (`error.Rejected` → EINVAL at the writer) is defence
  in depth, not done.
- The reference served a GDS ioctl only (decision 0002 row 553).
