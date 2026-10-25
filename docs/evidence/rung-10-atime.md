# Rung 10 — access time: relatime, strictatime, noatime are the kernel mount's

**Owner ruling (2026-10-23):** "never skip or adjudicate unless 100 % certain it is not a
bug/defect/unsupported." xfstests generic/003 and generic/192 sat in
`docs/gates/fstests-exceptions.md` as `noatime` ("the kernel never flushes atime for FUSE") —
true, incomplete: the kernel never TELLS a FUSE daemon about atime at all, so a daemon that
stamps nothing makes `-o relatime`/`strictatime` a lie the mount table tells. The two rows left
the ledger on `dev` (`f25fe2b`, the owner) ahead of this fix. Venue: the Omarchy workstation
(`local`), kernel `7.3.0-1.7-mainline-sqz`, branch `feat/atime-posture` off `f25fe2b`.

## What the kernel sends and keeps (linux 7.3, `/build/kernel/linux-mainline-sqz`, read not remembered)

- `fs/inode.c` `touch_atime(path)` → `atime_needs_update`: `S_NOATIME`, an unmapped id,
  `IS_NOATIME(inode)`, `SB_NODIRATIME` on a directory, the vfsmount's `MNT_NOATIME`,
  `MNT_NODIRATIME` on a directory, then `relatime_need_update` (`MNT_RELATIME`: update iff
  mtime ≥ atime, ctime ≥ atime, or `now.tv_sec − atime.tv_sec ≥ 86400`), then "equal to now"
  = no change. Then `mnt_get_write_access` (fails on `MNT_READONLY`) and, FUSE having no
  `update_time` op in 7.3, `generic_update_time` — the IN-CORE inode alone.
- Nothing reaches the daemon: `fuse_read_in` is `{fh, offset, size, read_flags
  (FUSE_READ_LOCKOWNER), lock_owner, flags (the FILE's open flags), padding}` — no atime word;
  `fuse_write_inode` → `fuse_flush_times` (`fs/fuse/dir.c:2110`) sends `FATTR_MTIME` (+
  `FATTR_CTIME` at minor ≥ 23) with the in-core values and no `FATTR_ATIME` — the "atime echo"
  the economy already absorbs; `fuse_reconfigure` (`fs/fuse/inode.c:187`) does `sync_filesystem`
  and nothing else, and a flag remount (`-o remount,strictatime`) is `fs/namespace.c`
  `do_reconfigure_mnt`/`do_remount` → `set_mount_attributes` → `touch_mnt_namespace`: the
  vfsmount's `mnt_flags` move, the superblock is not asked.
- The daemon's atime WINS: every read-class request ends in `fuse_invalidate_atime` (`dir.c:335`;
  `file.c:1022/1056/1797`, `readdir.c:429`, `passthrough.c:18`) which sets `STATX_ATIME` in the
  inode's `inval_mask` unless `IS_RDONLY`, so the next `stat` is a sync `GETATTR`
  (`fuse_update_get_attr`: `request_mask & inval_mask & ~cache_mask`), and
  `fuse_change_attributes_common` sets atime from the reply unconditionally
  (`fuse_get_cache_mask` is `MTIME|CTIME|SIZE` under the writeback cache, else 0 — never
  ATIME). After a read, `stat` shows OUR atime.
- The one signal a mount-table change gives: `fs/proc_namespace.c` `mounts_poll` reports
  `EPOLLERR | EPOLLPRI` once per `ns->event` move (always `EPOLLIN`: a POLLIN wait on mountinfo
  never blocks), and `touch_mnt_namespace` bumps the event and wakes `ns->poll`.

## The law (charter §4 "Access time"; `src/fuse/atime.zig` is the pure rule)

- **The posture is the kernel mount's.** `cli/mount.AtimeWatch` reads the daemon's own entry in
  `/proc/self/mountinfo` (by the mount id `mount(2)` gave, else the path; `core/mountinfo.zig`
  now carries the per-mount and the superblock options) once before `ready:` and again at
  every mount-table change: a meta-lane fiber parked on `POLL_ADD(POLLPRI)` of an open mountinfo
  fd (`exec/fdwatch.exceptional`), one table read per change on the host, nothing per op,
  nothing per tick. `lawOf(options, super_options)`: `relatime` → relatime, `noatime` → noatime,
  neither → strictatime (the kernel prints no word for `MS_STRICTATIME`), `nodiratime`, `ro` in
  either field. `SQUEEZEFS_ATIME` (`auto|relatime|strictatime|noatime`, registry row) replaces
  the posture word alone. The daemon's own `-o` parser now takes mount(8)'s vocabulary
  (`relatime`, `strictatime`/`nostrictatime`, `lazytime`/`nolazytime`, `atime`, `diratime`) and
  the suites' mount helper passes the words through to `mount(2)` instead of stripping them.
- **The rule** `needsUpdate(law, is_dir, {atime, mtime, ctime}, now)` restates
  `atime_needs_update` exactly (tests pin every clause, the whole-second day included).
- **The apply** is §5.2's ack posture: a due stamp PARKS `now` as the ino's pending refinement
  (`economy.PendingTimes`, `Times.atime` + `AtimeRule`) — fold-visible to the GETATTR the kernel
  asks next, journaled by the times drain as a `DELTA_ATIME` (alone, or beside a Δtimes/Δsize
  when both advance), never a ctime, never a barrier, never an entry per read; retire/retireIfStill
  judge it with the rest; strict mode (no cadence) journals it at fsync/fsyncdir/the dismount.
- **Every leg is a read.** `KvFs.touchAtime` in `read`, `listDir` (READDIR/READDIRPLUS) and
  `readlink` over the record already in hand (one byte load + three compares when nothing is
  due). Paths that hold no record note the read through the new `note_read` vtable entries
  (`handler/filesystem.VTable.note_read`, `transport/filesystem_vtable.VTable.note_read`): the
  transport worker's arm (a) (`probeRead` served — the staged/write-cache serve the first
  hand probe found: `fuse_atime_touches` stayed 0 until this hook) and the direct device leg,
  the interception host's ring fast path (`service.serveRead` served/eof) and its direct arm's
  lease grant (the only read the daemon sees of that arm). On those threads no record is
  read: under strictatime the stamp parks judged; under relatime it parks as a CANDIDATE
  (`AtimeRule.relatime`) that `foldTimes`/`drainDelta` apply iff `relatime_need_update` holds
  over the record — exact, lazy; `parkAtimeIfAbsent` parks one per drain tick per hot file.
- **Foreign-slot inodes** (§5.5): the stamp parks locally (folded over the token's record for
  this mount's GETATTRs) and the drain SHIPS it — `drainPendingTimes` routes foreign inos to
  `shipAtimeTouch`: one `record_ship.setattr{atime, atime_touch}` carrying the new wire bit
  `SETATTR_ATIME_TOUCH` (1 << 9; `SETATTR_KNOWN` 0x3FF — a wire-protocol change, full tier); the
  holder stages a `Δatime` iff it advances, no ctime, no other field (`record_atime_touches`);
  whatever the holder answers the stamp leaves the map (`fuse_atime_touches_shipped` /
  `_dropped`: an access stamp is lazy by law, never a failed read).

## Red first

- `smoke: ATIME — RELATIME, STRICTATIME, NOATIME ARE THE KERNEL MOUNT'S` against the `dev`
  binary `9d9e3ce`: `FAIL relatime first read atime: 1792808466.212106753 is not later than
  1792808466.212106753` — generic/003's first assertion (`~/tmp/atime-red-dev.log`).
- Unit: `fuse.atime` (the rule's clauses, `lawOf`, the override, `Live`), `economy` (a judged
  stamp folds/drains as a Δatime alone, beside a Δtimes, never when stored; a relatime candidate
  applies only where the rule holds; a standing stamp takes no second candidate; retire/claims),
  `kv_fs` (the handler on READ/READDIR/READLINK under relatime/nodiratime/strictatime/noatime/ro
  with the drain's stored record; `note_read`'s candidates), `mountinfo` (the two options
  fields), `mount_options` (the atime vocabulary and `MS_STRICTATIME|MS_LAZYTIME`),
  `verbs` (the wire bit). None compile on `dev`: the law did not exist.

## The row (`src/cli/smoke.zig`)

format → mount (relatime) → `echo aaa > dir1/file1`, `cat` after 1.1 s: atime later, mtime and
ctime unmoved; a second `cat`: unmoved; `ls dir1` twice: the first moves, the second not;
`readlink`: moves → `mount -i -o remount,relatime,nodiratime` (the watcher's line
`atime: relatime,nodiratime (the kernel mount's flags)`): a fresh `dir2`'s listing unmoved, its
file's read moves → `remount,strictatime,diratime`: three reads of file1 all move (the second
and third O_DIRECT — see the gap) → `remount,noatime,diratime`: file3's read unmoved, `echo xyz >
file1` moves mtime and ctime alone → `remount,ro,strictatime,diratime`: nothing → `remount,rw,
relatime,diratime`: file1's read moves once → `.stats` `fuse_atime_touches` 8, `_dropped` 0,
must-stay-0 clean → umount, mount: `stat %.9X %.9Y %.9Z` of six paths byte-identical to the
previous mount's → `SQUEEZEFS_ATIME=noatime`: a new file's read unmoved; `=strictatime` with the
mount `relatime,nodiratime`: two reads move, the listing not → fsck --offline --scrub 0 findings.
(`LIBMOUNT_FORCE_MOUNT2=always` for the flag remounts: libmount's new-API `fsconfig` reconfigure
of a FUSE superblock is refused by `fuse_parse_param` — "No changes allowed in reconfigure" —
as xfstests already knows; mount(8) merges the standing flags into a remount, so every remount
names its whole atime set.)

## The gap that stays (a kernel one), and the strictatime open

A read the kernel serves from its own page cache reaches no FUSE daemon: the kernel's in-core
`touch_atime` stands until the next GETATTR replaces it with the daemon's (`fuse_invalidate_atime`
is not called for a cache hit, so the replacement waits for the attr TTL or the next real read).
The first subset run showed it as generic/003's one failure — "access time has not been updated
after accessing file3 third time": the strictatime arm's second `cat` of a 4-byte file on one
mount was a page-cache hit (`transport_fast_dispatch_serves` flat, `fuse_atime_touches` flat,
`stat` after the 1 s TTL showed the daemon's older stamp). Every FUSE filesystem shares it. So
under **strictatime** a regular file's open carries no `FOPEN_KEEP_CACHE` (`regularOpenFlags`):
the open drops the cached pages and every open-read cycle reads through the daemon and is stamped
(the three `cat`s of the row and generic/003 all move); the posture's own cost, as ext4's journal
write per read is. **relatime** keeps the cache: a partial-page write marks no folio uptodate
(`fuse_fill_write_pages`), so the first read after `echo aaa > f` is a daemon read (generic/003,
generic/192), and a full-page write followed by a `cat` while the pages stay cached is the shape
that misses. What stays: repeated reads on ONE open descriptor under strictatime, and that
relatime shape — the fix is the kernel's (`fuse_flush_times` carrying `FATTR_ATIME`, taken by the
daemon as a touch, not a utimes; or a FUSE `update_time` op): an owner-call for
`scripts/kernel/patches-mainline/`. Under zero-message opens (the 9.33 kernel's no-open posture)
the kernel keeps the cache on its own (`fuse_file_open`: `FOPEN_KEEP_CACHE` is the no-open
default) and the strictatime open cannot drop it — the same gap there.

## Counts (this box, 7.3.0-1.7-mainline-sqz, under the venue's other agents' load)

- The ATIME row: red on `dev` `9d9e3ce` (`FAIL relatime first read atime: … is not later than …`);
  10/10 on the branch at `3840749` (`~/tmp/atime-loop.out`), then 1/1 after each later
  amendment (`7c9af55`, `ff150a3`, `85e4520`).
- Unit filters, all green: `atime` 53, `Access time` 44, `relatime CANDIDATE` 41, `access stamp
  alone` 41 (the backend test of the two exemptions), `kv_fs` 114, `read_view` 46, `times` 85,
  `pending` 73, `mountinfo` 48, `mount option` 39, `flags and names` 40, `SetattrWire` 39,
  `record_ship` 40, `two_holders` 62, `economy` 71, `verbs` 66, `conveyor` 93, `node_cache` 76,
  `witness` 53, `lease` 365, `service` 54, `worker` 106, `fdwatch` 40, `readable` 58,
  `journal_core` 64; the whole unit set once: `23/23 steps succeeded; 2945/3076 tests passed
  (131 skipped)`, 0 failed. `zig build rails` 18/18; `zig fmt --check` clean.
- The read rows: THE READ VIEW, THE DIRECT ARM, THE INTERCEPTION HOST, THE STAGED READ'S
  IDENTITY WINDOW, KERNEL AIO, O_TRUNC UNDER SERVED OPENS — pass. THE READ VIEW and THE DIRECT
  ARM each caught a defect of the first cut (a landed Δatime staled the view and bumped the
  layout lease epoch: `hits 1023 < 1024`, `il_lease_asks` 3 ≠ 2) — the two exemptions above.
- xfstests subset `generic/003 generic/192 generic/001 generic/013 generic/127`: run 1 on
  `3840749` stopped at generic/003's one line (`access time has not been updated after accessing
  file3 third time` — the strictatime open, above); run 2 on `ff150a3`+the hint:
  `check rc=0, ran 5, not run 0, failed 0, 72 s wall (Passed all 5 tests)`
  (`/build/tmp/suites-fstests-20261023-233447-690333`; the `ledger rc 3` after it is the
  subset's stale-row artifact, every ledger row being absent from a 5-test run). The two
  ledger rows had left `docs/gates/fstests-exceptions.md` on `dev` already (`f25fe2b`).
- `rr4k-q32j4` before/after: NOT measured — the venue carried a full gate, three agents' smoke
  loops and the harness (load 28, Tctl 82 °C) throughout; owed on a cooled, quiet host:
  `sudo MX_RUNTIME=20 MX_ROWS='rr4k-q32j4' scripts/perf/quiet.sh ram-sqz atime-a` on `dev`,
  then on this branch, A-B-B-A. The per-op addition on the hot read path: in `kv_fs.read` one
  byte load and three compares beside the record the handler already read; on the worker's
  served probe and direct leg, and the IPC fast path, one `localOf` and one relaxed load of a
  per-stripe read-mostly word (the hint) — the lock and its cache line only once per drain tick
  per file.

## Throughput A-B-B-A (2026-10-24, quiet host, the RAM-NVMe set, dist builds, 20 s rows, `tctl_c` 49–74 °C)

Base `c79ecb1` (before atime) against `dev` `9744d22` (atime `bbced72` + idle RSS `e2e3c94` + the
conveyor window gate `9744d22`), legs A B B A:

| row | A (base) | B (dev) | B (dev) | A (base) | Δ of the means |
|---|---|---|---|---|---|
| rr4k-q32j4 IOPS (p50 µs) | 1,299,998 (88.6) | 1,327,760 (87.6) | 1,316,173 (88.6) | 1,331,279 (87.6) | +0.5 % |
| sw1m-q16j4 MB/s | 51,600 | 49,661 | 50,867 | 49,586 | −0.6 % |

Inside the legs' own spread: the read cache's stripe spin lock, atime's per-read hint load and
the window gate cost nothing measurable. Sessions `/build/perf/mx/ram-sqz-rabba-{base,dev}-{1,2}`.

## O_NOATIME — the file's law before the mount's (2026-10-25, LTP open12)

**The red.** The LTP verdict on `bd97cda` failed `open12`'s O_NOATIME case on all eight
iterations (`open12.c:99: TFAIL: File access time changed`; ext4's control passes;
`/build/tmp/suites-ltp-20261025-000716-494138`). The test's `.all_filesystems` loop mounts
nothing at `mntpoint` (no `.mount_device`; strace shows only the `/dev/zero` support probes), so
every iteration ran on the TMPDIR's filesystem — ours, `relatime`: a fresh file, `open(O_RDONLY |
O_NOATIME)`, one byte read, and the daemon stamped it by the mount's law. Reproduced by hand on
the `bd97cda` binary (`passed 24, failed 8`).

**The kernel (7.3, read).** `file_accessed` (`include/linux/fs.h:2278`) skips `touch_atime` for
`f_flags & O_NOATIME` in-core alone; `fuse_read_args_fill` (`fs/fuse/file.c:599`) sends
`in.flags = file->f_flags` on READ and on READDIR/READDIRPLUS (`fs/fuse/readdir.c:395/397`), so an
`F_SETFL` toggle shows in the next request; `may_open` (`fs/namei.c:4283`) and `setfl`
(`fs/fcntl.c:51`) admit the flag for the owner or `CAP_FOWNER` alone — the daemon honours what it
is handed. READLINK has no description (`touch_atime` on the path).

**The fix (every leg).** `atime.fileAccessed(open_flags)` is the rule. The handler's READ and
READDIR/READDIRPLUS judge `fuse_read_in.flags` (`ReaddirArgs` now carries them) before the law
(`KvFs.fileAccessed`); `note_read` takes the flags (`NoteReadArgs.flags`): the worker's arm (a)
serve and direct leg hand the READ's own (`fsNoteRead` — never the OPEN's, which zero-message
opens never send); the interception host captures `O_NOATIME` off the bound fd at BIND
(`fdFacts` → `ScreenOk.noatime` → `Binding.noatime`, host-side, no wire bit) and every arm reads
`Binding.openFlags()` — the ring fast path's note, the handoff's READ, the direct arm's lease
grant; the shim unbinds on an `F_SETFL` toggle of `O_NOATIME`, as of `O_DIRECT`. Cost on the hot
read path: one AND and a branch on a flag word already in hand (the READ's args, the request the
worker holds, the binding row the op carries); the IPC builds that word from two bools of the
row it copied, replacing the `O_DIRECT` select it had.

**Red first** (commit `test(atime): red`, against the `dev` daemon): unit `O_NOATIME (LTP
open12)` — `expected 1000000000, found 1792903914` (the READ's GETATTR); smoke ATIME row —
`FAIL relatime O_NOATIME listing atime: 1792905626.151510384 != 1792905629.479501241` and, in the
row's first cut, `FAIL strictatime O_NOATIME read atime: 1792904783.884218276 !=
1792904784.998219384`. The row's arms: the O_NOATIME listing sits in the relatime phase (no
kernel readdir cache: every listing reaches the daemon; a strictatime listing is a READDIR per
`getdents`, the EOF page stamped iff the coarse clock ticked — `iterate_dir`'s `file_accessed`
per call, ext4's too — so its touch count is not exact there), the O_NOATIME read in the
strictatime phase (under relatime the kernel keeps the pages the first open filled). The helper
runs `python3 -I`: a plain `-c` lists its cwd, the mount's root, for its imports.

**Counts** (`local`, 7.3.0-1.7-mainline-sqz, quiet host): the ATIME row 10/10; THE READ VIEW,
THE DIRECT ARM, THE INTERCEPTION HOST (and its death row) pass; unit filters `atime` 59, `kv_fs`
120, `read_view` 46, `O_NOATIME` 46, `fcntl F_SETLK` 40, `ring binding` 42, `FD SCREEN` 40, `fd
facts` 40, `bindings: ids` 40 — 0 failed; `zig build rails` 18/18; `zig fmt --check` clean.
