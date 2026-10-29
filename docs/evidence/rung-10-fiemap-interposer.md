# Rung 10 — FIEMAP through the interposer (2026-10-28; charter §4 "FIEMAP"; decision 0003 §CJ)

**Owner ruling (2026-10-28):** FIEMAP is served THROUGH THE INTERPOSER ONLY, no kernel patch. Linux
answers `FS_IOC_FIEMAP` for a FUSE file in the VFS (`do_vfs_ioctl` → `ioctl_fiemap` → EOPNOTSUPP: FUSE
has no `->fiemap`), so without `LD_PRELOAD=libsqueezefs_il.so` that answer stands. Branch
`feat/fiemap-interposer`, written on `7693bab` and rebased onto `d80407c`; venue `local` (7.3.0 mainline-sqz).

## What and how (the calls and their reasons: decision 0003 §CJ)

- **Law** `src/fuse/handler/fiemap.zig` (from `feat/fiemap` a6bb7ed): the map at the true allocation
  unit (the 4 MiB stripe block; a small file's one image), clipped at 4 KiB; `prep` = `ioctl_fiemap` +
  `fiemap_prep` for a FUSE superblock. **Daemon** `kv_fs.fiemapIoctl` (from 42390ac minus the kernel
  forwarding): custody lands first, SYNC runs the fsync data leg, `DataPlane.walkKvmap` pages a tree map.
- **Verb** `src/ipc/fiemap_verb.zig` (wire tags 19/20): the fd itself rides `FiemapReq`; screen +
  `prep` on the ctl poller, the handler on a FUSE-class lane, the reply on a dup of the ctl socket.
- **Shim**: `ioctl` (68th interposition) serves FS_IOC_FIEMAP on a regular file of an interception
  mount and passes everything else through; copies via `process_vm_readv/writev` (EFAULT, not SIGSEGV).
- **Harness**: `FSTESTS_PRELOAD=<so>|1` preloads `check`, mounts `-o interception`; helpers unset it.

## Red (the test commit, `41f64a6` — before the rebase `7073aff` = `7693bab` + the tests)

- kv_fs "fiemap through the dispatcher": `FS_IOC_FIEMAP: errno NOSYS` (39/40).
- shim smoke "FIEMAP UNDER THE INTERPOSER": the probe under LD_PRELOAD read `rc=-1 errno=95`
  (EOPNOTSUPP) for `t/inline` (43/44 shim-smoke tests, the other rows green).

## Green

- Unit filters shim/ipc/sdk/fiemap/FIEMAP/ioctl green (the law + fuzz, the dispatcher, the verb + fuzz,
  shim e2e "copy law": 3,000 extents in three batches, SYNC once, count-only, EBADR, EINVAL, EFAULT, a
  partial array; linked-mode passthrough); `zig build rails` and `zig fmt --check` clean.
- `zig build shim-smoke` green, the FIEMAP row included; `zig build smoke -Dtest-filter=INTERCEPTION` green.

## xfstests under the preload (`FSTESTS_PRELOAD=1`, one harness run per test; 0 fsck findings on every set)

The 30 tests that `_notrun` "xfs_io fiemap failed" on the venue's kernel, the fiemap group's others
(519, and 409–411 whose fsstress runs a fiemap op), and 001/013:

| verdict | tests |
|---|---|
| PASS | 001, 032, 409, 410, 411, 519, 742 |
| `_notrun` on their next gate | 012 016 017 021 022 (fcollapse: FUSE forwards no COLLAPSE_RANGE), 058 060 061 063 064 (finsert, the same), 425 (`fiemap -a`: XATTR is EBADR), 578 (reflink), 679 (`_scratch_mkfs_sized`), 746 (discard ranges), 791 (fanotify ioerrors), 804 (scsi_debug) |
| FAIL — the allocation unit (proposed by-design rows below) | 009 092 094 177 225 255 316 483 677 695 796 797 |
| FAIL — not FIEMAP | 013 (below) |

**By-design rows for a preloaded run** (owner ruling 2026-10-29, decision 0003 §CJ.9: the allocation
unit is the 4 MiB stripe block by the KISS ruling; `fstests-exceptions.md`'s format but NOT in the ledger:
the release run is unpreloaded, where these tests `_notrun` and a row would be stale; a preloaded run
judges any of them failing on another cause as a defect):

| test-id | reason-class | evidence |
|---|---|---|
| generic/009 | by-design-allocation-unit | ZERO_RANGE over 320 KiB files: a zero range inside a stripe block writes zeros and reserves nothing short of a whole block (a hole stays unmapped, data stays one extent); a fallocate over a small file promotes its image to a written block first (§6.6), so the "unwritten" rows map `[0..8191]: data`; every md5 matches the golden |
| generic/092 | by-design-allocation-unit | `falloc -k 0 10m` + `pwrite 0 5m` + truncate: data rounds to two 4 MiB blocks (`[0..16383]`), the reservation to the third (`[16384..24575]`); `_require_congruent_file_oplen 5M/7M` passes because `stat -f %S` is 4096 |
| generic/094 | by-design-allocation-unit | fiemap-tester with preallocation at FIGETBSZ (4 KiB): a block the map makes a hole or preallocated shares a 4 MiB block with written ones, so it is reported written ("found an allocated extent where a hole should be", "preallocated extent is not marked with FIEMAP_EXTENT_UNWRITTEN" — the seed decides which first) |
| generic/177 | by-design-allocation-unit | punches of 8–32 × 4 KiB inside one stripe block leave it mapped (written zeros) where the golden has holes |
| generic/225 | by-design-allocation-unit | fiemap-tester without preallocation: a small file's holes are bytes of its one image (inline/staged/packed, `[0, ceil_4k(size))`), so FIEMAP maps data where the map has a hole (FIBMAP says hole) |
| generic/255 | by-design-allocation-unit | `_test_generic_punch` over 320 KiB files: a punch inside a small file's image or a stripe block stays mapped; the fallocated variants map the whole 4 MiB block |
| generic/316 | by-design-allocation-unit | as 255 without unwritten extents |
| generic/483 | by-design-allocation-unit | a 1.25 MiB KEEP_SIZE reservation maps its whole 4 MiB block (`[0..8191]`), after the power cut too |
| generic/677 | by-design-allocation-unit | two 1 MiB reservations 3 MiB apart lie in two adjacent 4 MiB blocks: one unwritten run `[32768..49151]` where the golden has two with a hole |
| generic/695 | by-design-allocation-unit | `fpunch 2M 2M` and `fpunch 8M 2M` inside 4 MiB blocks leave blocks 0 and 2 mapped (`rung-10-punch-edge.md`) |
| generic/796 | by-design-allocation-unit | the reservation rounds to the block: `[8192..24575]` where the golden has `[8192..18431]` |
| generic/797 | by-design-allocation-unit | a write into a reservation flips its whole 4 MiB block (§6.6's in-place landing): `[0..8191]: data` where the golden has data then unwritten |

The alternative that turns most of these into honest `_notrun`s is xfstests' own large-allocation-unit
mechanism: `_require_congruent_file_oplen` reads `stat -f -c %S` (statfs `f_frsize`, 4096 today); a
mount reporting the stripe block there would `_notrun` 009 092 255 316 483 677 695 796 797 (and size
177's punches in 4 MiB units) — a posture change for every `statvfs` consumer (`df` counts in
`f_frsize` units). **Not taken** (owner ruling 2026-10-29): `f_frsize` stays 4096 — it would misstate
small files, which allocate 4 KiB packed slots or live inline, and change what `df` and every `statvfs`
caller see. fiemap-tester (094, 225) reads FIGETBSZ and fails either way.

**generic/225's first run** found the one flag defect: a packed tenant carried DATA_TAIL|NOT_ALIGNED
on a 4 KiB-aligned slot ("FIEMAP_EXTENT_NOT_ALIGNED is set but offset and length is blocksize
aligned") — fixed in `3eb5985` (an aligned extent); re-run below.

**On the final code before the rebase (`3eb5985`'s tree on `7693bab`, clean):** 225 fails on the grain alone (the packed slot is `flags 0x001`; fiemap-tester stops at a hole inside a small file's image), 094 likewise (a hole inside a written 4 MiB block); 001 and 742 pass; fsck 0 findings, 0 alerts.

**After the rebase onto `d80407c`:** unit filters 239/248 (9 skipped), rails, fmt, shim-smoke (the
FIEMAP row), the INTERCEPTION rows green; under the preload 001 032 519 742 pass, 225 and 695 fail on
the grain alone; fsck 0 findings, 0 alerts.

## Seen, not fixed

- **generic/013 under the preload wedges the mount — pre-existing, not FIEMAP.** 4 of 4 runs fail: twice on
  this branch, twice on the pre-rebase test commit (`7693bab` + the tests: no `ioctl` hook, no daemon FIEMAP); fsstress issued no
  FIEMAP (`fiemap - no filename` every time). The watchdog names a `syncfs` and an `fsync` parked past
  `write_admitted`, an `fsyncdir` past `meta_backend_start`, a `read` past `read_routed`, 75 s and
  more; then a FUSE_WRITE payload lease outlives the kernel's 135 s request timeout and the connection
  is aborted (`fsstress … Transport endpoint is not connected`; on both dev runs check's fsck was
  refused — the wedged daemon still held the writer guard — and reads "inconsistent"; the harness's
  own fsck after the run: 0 findings). Repro: `SUITE_RUNS=1 FSTESTS_PRELOAD=1
  FSTESTS_TESTS=generic/013 scripts/suites/fstests.sh`. Owed: its own red and fix (the interposer's
  sync/read paths under concurrent ring writers).
- §CJ.10 (b)–(d): SYNC promotes this member's residents only; a first fd that is no HELLO credential
  (`O_APPEND`) and no session yet is EOPNOTSUPP; a 4096 B inline file reports NOT_ALIGNED on an aligned
  length; `fe_physical` renumbers when a volume leaves the set.
- Wire tags 19/20 are this verb's; a parallel IPC change taking the same numbers conflicts at merge.
