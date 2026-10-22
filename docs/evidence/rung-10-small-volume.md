# rung 10 — a 64 MB set (fstests generic/563): the layout scales down to the product's floor; the cgroup charge is the daemon's

**Finding** (2026-10-22, the `-g auto` run on dev `7e0792a`, 546 tests in): generic/563 builds a
**64 MiB loop device** over the scratch (`_small_fs_size_mb $((4 × 16 MiB))`,
`_create_loop_device_like_bdev`), `_mkfs_dev`s and mounts it, writes a 16 MiB file, cycles the
mount, then from cgroups `563-cg`/`563-cg-2` does `pread`/`pwrite -b 4096`/`fsync` and asserts the
loop device's `io.stat` rbytes/wbytes in those cgroups within 5 %. The harness refused first:
`mkfs.fuse.squeezefs: /dev/loop2 holds 67108864 B; the set needs 18253611008 B` — `suite_slice_layout`
(08a9aaa) scaled UP from the fixed 1 GiB + 2 × 8 GiB layout and never down. Owner's ruling: a
harness limit is ours to fix; never skip unless 100 % certain.

## 1. The product's smallest set (measured, `squeezefs format`'s refusals on 7e0792a)

| volume | floor | why |
|---|---|---|
| metadata, default node 256 KiB, `--single-writer` | **11,800,576 B** (11.25 MiB) | `geometry.plan`: ledger at 4 KiB (128 KiB) → journal `clamp(volume/64, 8 MiB, 32 MiB)` = the **8 MiB floor** (`journal_ring_min`) → bitmap 8 KiB → heap aligned to the node at 8,650,752 B; the plan requires `compactionReserveExtents(total) + 4` = **12 heap extents** (3,145,728 B); the last 4 KiB is the DUR-5 tail copy. Binary search over sparse files lands exactly on the formula |
| metadata at `--meta-node-kib 64` | 9,375,744 B | the same with 64 KiB extents |
| metadata with `--meta-journal-mb` | down to ~1 MiB | the override floor is 384 KiB (reserve 256 KiB + one 128 KiB entry) |
| data, default 4 MiB block | **12,582,912 B** (3 chunks) | `transform.capacityBlocks = chunks − 2`: chunk 0 and the last chunk carry the identity sectors; 1 usable block. 8 MiB refuses ("holds fewer than 3 chunks") |
| the set | **36,966,400 B** (12 + 12 + 12 MiB rounded: **36 MiB**) | holds 8 MiB of file data |

The block size is NOT derived from the volume: the default is the constant `transform.DEFAULT_BLOCK_SIZE`
(4 MiB, charter §6.2's accumulation stride), `--block-size` 4 KiB–4 MiB is the operator's knob. On a
24 MiB data volume that is 4 usable blocks (16 MiB); `--block-size 1m` gives 22 (`df` 44M). mkfs.ext4
derives its block size by size class; a `blockSizeFor(smallest data volume)` derivation (say
`clamp(pow2 ≤ volume/64, 4 KiB, 4 MiB)`) would give small sets a finer grain — a set-wide, on-disk
format choice (Full tier) for the owner to rule on; it blocks nothing: 563's shape passes at 4 MiB.

Trials (sparse files, the 563 sequence: 16 MiB write → cycle → `pread` + `pwrite -b 4096` 16 MiB +
fsync → two more 16 MiB overwrites + fsync → umount → fsck): the floor set (1 block per volume) takes
the first 16 MiB (staged in the write cache — the dismount promotion then fits 8 MiB: the second
write answers ENOSPC, fsck clean); **16 MiB + 2 × 24 MiB (the 64 MiB layout) passes every step**,
`df` 32M, fsck 0 findings; 12 MiB + 2 × 26 MiB the same (6 chunks either way).

## 2. The harness: the layout scales down (`scripts/suites/setpaths.sh`)

`suite_slice_layout <sectors>`: at or above the fixed layout (17 GiB — the loop sets) meta =
`SUITE_META_MIB`, data = the rest / 2 (unchanged tables); below it **meta = clamp(device / 4,
`SUITE_META_MIN_MIB` 12, `SUITE_META_MIB`)**, **data = (device − meta) / 2**, every slice a 1 MiB
multiple, refused only when a data slice is below `SUITE_DATA_MIN_MIB` 12 — with the numbers
(`holds N B; the smallest set is 37748736 B — a 12 MiB metadata volume (the 8 MiB journal floor + 12
heap extents) and two 12 MiB data volumes (three 4 MiB chunks each, two reserved)`). Dry run:
17G → 1 GiB + 2 × 8 GiB; **64M → 16 + 2 × 24 MiB**; 36M → 12 + 2 × 12; 35M refused; 100M → 25 +
2 × 37; 1G → 256 + 2 × 384; 10G → 1 GiB + 2 × 4.5 GiB; 17T → 1 GiB + 2 × 8.5 TiB.
`fstests.sh` exports xfstests' own hook `MIN_FSSIZE` = the smallest set (36 MiB; xfs sets 600 MB,
btrfs 512 MB): `_small_fs_size_mb` rounds a smaller request up, and nothing is skipped by it (our
FSTYP has no `_scratch_mkfs_sized` arm, the one path that `_notrun`s on it).

Beside it, two venue-sharing defects met in the same hour:
- **Two harnesses on one venue shared dm slice names.** Every run's scratch set was `set-scratch`,
  so its slices were `sqz-set-scratch-{meta,d0,d1}` for every run: this work's mkfs at 19:28:42
  re-pointed the adjudication audit's scratch slices under its generic/795 post-test fsck, which
  then read `sqz-set-scratch-d0 carries a data superblock this binary refuses — not a member of this
  set` (run `suites-fstests-20261022-192638-2987726`: **that generic/795 failure is this collision,
  not the product's**). Now a loop set's id carries 8 hex of its directory's md5
  (`set-scratch-1a2b3c4d`), each run writes its env (`SQZ_BIN`, `SUITE_SETS_DIR`) beside its sets
  (`<run>/suites.env`, found from a set's path by `suite_run_env`, a loop over a loop set's device
  included), and `fstests_build` waits (≤ 900 s) for any other `check` before it re-points the
  venue-wide `/sbin` helper symlinks. The symlinks themselves stay venue-wide (util-linux looks up
  `/sbin/mount.<type>`): a harness of an OLDER version started under a newer one still crosses.
- **The end-of-run scratch fsck is the record, never the verdict.** The scratch at a run's end is
  the last test's to have left: a foreign mkfs (740), a snapshot's COW chunks (620), another set's
  volumes over its first 64 MiB (563 — `status` reads that set's superblock, the fsck then reads
  `sqz-loop4-d0 … not a member of this set`). Every test that checks its scratch had
  `_check_scratch_fs`; a finding here was that test's (already red) or no test's. Logged, rc 0.
  The TEST set's end-of-run fsck stays a verdict.

## 3. The product under generic/563

(a) The small set: formatted (16 MiB + 2 × 24 MiB: 4 usable blocks each, `capacity 33554432 B`),
mounted and cycled 4 times, every one of the 11 `xfs_io` commands whole (16 MiB, 4096 ops each; no
ENOSPC, no alert line, clean unmounts), 4 daemon logs under `run1/sets/loop4/`.

(b) The charge. `563.out.bad`: every expectation of 16 MiB charged to `563-cg` reads
`read has value of 0` / `write has value of 0`; every "nothing charged to the second group" line
passes vacuously. By hand (a 64 MiB loop, the daemon mounted from the root cgroup as 563's
`reset()` does, 16 MiB `pwrite -b 4096` + fsync, `drop_caches`, 16 MiB `pread`, issued from
`/sys/fs/cgroup/sqz563-cg`), `io.stat` for the loop's `7:0`:

| cgroup | row |
|---|---|
| the issuing cgroup `sqz563-cg` | **no row** (0 rbytes, 0 wbytes) |
| the daemon's cgroup (= the root `/`) | rbytes **+16,777,216**, wbytes **+17,464,320** (16 MiB + 688 KiB of metadata), rios +134, wios +1,097 |

The kernel: a bio is charged to the SUBMITTING task's blkcg — `bio_associate_blkg()` →
`blkcg_css()` = `task_css(current, io_cgrp_id)` (`block/blk-cgroup.c`), and `blk_cgroup_bio_start()`
adds its bytes/ios to `bio->bi_blkg`, which `io.stat` prints — or, for a KERNEL filesystem's
writeback, to the dirtying inode's `wb->blkcg_css` through `wbc_init_bio()`
(`include/linux/writeback.h`; `Documentation/admin-guide/cgroup-v2.rst` "Writeback": "cgroup
writeback requires explicit support from the underlying filesystem. Currently, cgroup writeback is
implemented on ext2, ext4, btrfs, f2fs, and xfs. On other filesystems, all writeback IOs are
attributed to the root cgroup"). Under FUSE the test's `pread`/`pwrite` reach the daemon as
FUSE_READ/FUSE_WRITE requests; the DEVICE bios are the daemon's own io_uring/`pwrite` on
`/dev/mapper/sqz-…`, submitted in the daemon's context — its blkcg. No interface lets a userspace
process attribute a bio to another cgroup: `kthread_associate_blkcg()` (how `drivers/block/loop.c`
and dm-crypt charge their worker I/O to the original submitter — the reason 563 holds for kernel
filesystems on a loop device) and `REQ_CGROUP_PUNT` (btrfs) are kernel-internal; io_uring has no
per-SQE cgroup; `cgroup.procs`/`CLONE_INTO_CGROUP` move whole tasks; FUSE passthrough
(`FUSE_PASSTHROUGH`, 6.9: `fuse_passthrough_read_iter` → `vfs_iter_read` in the caller's context)
maps a FUSE file 1:1 onto a backing REGULAR file of a kernel filesystem — not a raw device with a
layout of its own. The sqz kernel series (kmbuf, the io_uring FUSE transport) moves request
payloads between kernel and daemon; it does not change who submits device bios. **563's assertion
is unsatisfiable for any FUSE filesystem whose daemon issues the device I/O**; satisfying it would
take a kernel interface (a capability-gated per-request blkcg attribution for userspace servers —
none upstream), not a product change.

The venue: cgroup v2 with `io` in `cgroup.controllers` and `+io` in the root's `subtree_control`
(`_require_cgroup2 io` passes; on a host without it the test is `_notrun`).

### Proposed ledger row (NOT added — the owner decides)

```
| generic/563 | by-design-fuse-io-charge | cgroup v2 `io.stat` charges a bio to the SUBMITTING task's blkcg (`bio_associate_blkg` → `blkcg_css()` = `task_css(current, io_cgrp_id)`; `blk_cgroup_bio_start` adds the bytes to `bio->bi_blkg`), or for a kernel filesystem's writeback to the dirtying inode's `wb->blkcg_css` (`wbc_init_bio`; cgroup-v2.rst "Writeback": ext2/ext4/btrfs/f2fs/xfs, "on other filesystems, all writeback IOs are attributed to the root cgroup"). Under FUSE the device I/O is the DAEMON's: the test's pread/pwrite reach it as FUSE requests and its own io_uring/pwrite on `/dev/mapper/sqz-…` submits the bios in ITS cgroup (the root: 563's `reset()` mounts from `$cgdir`); no userspace interface attributes a bio to another cgroup (`kthread_associate_blkcg`, `REQ_CGROUP_PUNT` kernel-internal; io_uring no per-SQE cgroup; FUSE passthrough = a backing regular file). Measured (rung-10-small-volume.md): the issuing cgroup's io.stat has no row for the loop device; the daemon's grew rbytes +16,777,216 / wbytes +17,464,320 for the 16 MiB read and write. The 64 MB set itself formats, mounts, cycles and takes every 16 MiB write and fsync (11/11 xfs_io commands whole); every `read/write has value of 0` line is the charge. Unsatisfiable for any FUSE filesystem whose daemon issues the device I/O — decision 0002 row to be assigned |
```

## 4. Runs

- `generic/563` alone: past the harness, to its assertions (the charge lines above); the 64 MB
  set clean (4 mounts, 0 alerts).
- `generic/001 013 034 563 604 620`: `check` runs them in numeric order and the harness stops
  fail-early at the first unadjudicated failure — **563** (its diff: `read has value of 0` …);
  001, 013, 034 passed before it. Then `generic/001 013 034 604 620`: **Passed all 5** (the loop
  sets' tables unchanged at 1 GiB + 2 × 8 GiB; 620's hugedisk at 1 GiB + 2 × 8.5 TiB), test-set
  fsck 0 findings, 0 alert lines on every set, the scratch's leftover recorded.
- `bash -n` clean on every script (no shellcheck on the venue); no Zig changed.
