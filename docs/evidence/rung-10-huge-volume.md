# rung 10 — a 17 TB set (fstests generic/620): the harness takes any block device, the product is O(structures)

**Finding** (2026-10-22, the release re-run of `-g auto` on dev `d848fb5`, kernel
`7.3.0-1.7-mainline-sqz`): generic/620 builds a **17 TiB dm-hugedisk** (`common/dmhugedisk`: a
dm-zero origin and a TRANSIENT dm-snapshot over it, `0 36507222016 snapshot <zero> $SCRATCH_DEV N
128` — every written 64 KiB chunk lands in the COW, the scratch loop), runs `_mkfs_dev` on it,
mounts it BARE (`_mount $DMHUGEDISK_DEV $SCRATCH_MNT`, no `-t`), `pwrite 1m` + `fsync`, tears it
down. The run stopped fail-early there, in the SUITE HARNESS before the product ran:
`mkfs.fuse.squeezefs: /dev/dm-4 is a block device with no backing file — the suites key a set by
its metadata image's directory`. Owner: "we shouldn't have a disk size limit"; the exclusion
(`d9e79f2`) was reverted (`91e7400`): the test judges the product.

## 1. The harness: any raw block device is a set device (the production shape, charter §1.2)

`scripts/suites/setpaths.sh` keyed a set by its loop device's backing image and refused every
other block device; `suite_slices_up` carved a FIXED layout (1 GiB + 2 × 8 GiB) whatever the
device, so the 17 TiB would never have been exercised even past the refusal. Now:

- `suite_set_of`: a device-mapper target that passes through to ONE device (`linear`, `flakey`,
  `delay`, `log-writes`, `dust`, `snapshot-origin`) is followed level by level (≤ 8 hops) to the
  set's device, as before; `error` names none and the slices standing over it name the set, as
  before; any other target (`snapshot` — the hugedisk —, `thin`) and any block device that is no
  loop over a file (a loop over a block device — generic/563's shape —, a scsi_debug disk, a real
  one) IS a set device: `SET_ID` = its kernel name (`dmsetup info -c --noheadings -o name`, else
  the basename), `SET_DIR` = `$SUITE_SETS_DIR/<id>` — `fstests.sh` writes the run's `<run>/sets`
  into the helpers' env file per run — never `/dev`. The 620 hugedisk: set `huge-test.620`, logs
  under `run1/sets/huge-test.620/`, censused with the two loop sets (daemon logs, alerts,
  tripwires, `daemon-died`), its slices and `.dir` taken down with the run's.
- `suite_slice_layout <sectors>` (pure): meta = `SUITE_META_MIB` (1 GiB); each data slice =
  `max(SUITE_DATA_GIB, (device − meta) / 2)` rounded down to 1 MiB. The loop sets are exactly the
  floor's size, so their tables are byte-identical to before (`0 2097152`, `0 16777216 … 2097152`,
  `… 18874368`); the 17 TiB hugedisk gets `meta 0 2097152 linear <dev> 0`, `d0 0 18252562432 linear
  <dev> 2097152`, `d1 … 18254659584` — 1 GiB + 2 × 9,345,311,965,184 B = the device, 18,691,697,672,192 B.
  Dry run over a 17 TiB dm-zero of our own: both paths (`/dev/mapper/<name>` and `/dev/dm-N`) key
  the same set; the slices sum to the device.
- `scripts/suites/mount.typed`, `check`'s `mount` (first on its PATH; xfstests takes
  `MOUNT_PROG=$(type -P mount)`): a BARE mount of a block device libblkid cannot type (exit 2,
  nothing printed) gets `-t fuse.squeezefs`; anything typed, bound, moved, remounted, `-o loop`
  or not a block device goes to mount(8) unchanged. Without it the first run past mkfs failed
  in mount(8) itself — no helper, libblkid knows no FUSE superblock, dmesg showed exFAT and
  ntfs3 probing dm-5 — and the alternative, `/etc/filesystems`, is a HOST setting.
- The end-of-run scratch fsck skips a scratch that opens as no set (`cannot open metadata
  set`): the transient snapshot's COW chunks — copies of the hugedisk's OWN meta slice — sit on
  the scratch after 620, so `status` reads a superblock whose ledgers are another set's. Every
  test that checks its scratch had it fscked by `_check_scratch_fs`; the leftover of a
  `_require_scratch_nocheck` test is the test's to leave.

Runs: `FSTESTS_TESTS="generic/620 generic/604 generic/001"` → `Passed all 3 tests`, 12 s wall;
`generic/001 013 034 604` → `Passed all 4 tests`; `generic/039` (dm-flakey, the walk arm) passed;
test and scratch `fsck --offline --scrub` 0 findings; 0 alert lines on every set. `bash -n` on
every script (no shellcheck on the venue); `zig build rails` green; `zig fmt --check` clean.

## 2. The product at 17 TiB (our own hugedisk, 620's tables, COW on a 17 GiB sparse loop)

| step | wall | bytes to the device | RSS / notes |
|---|---|---|---|
| `format --single-writer` (meta 1 GiB + 2 × 8.5 TiB) | 0.29 s | 33,865,728 B (32.3 MiB; COW 66,432 sectors used) | per data volume: chunk 0 + the last chunk zeroed + 2 sectors = 8 MiB + 8 KiB; the meta fixed region 16.3 MiB (16 MiB journal) |
| mount (both volumes bound 18 ms after the set config) | 0.34 s | — | VmRSS 3,292,428 kB vs 3,185,316 kB for the 17 GiB loop set: **+107 MB = `block_alloc_table` 108,062,608 B** (2 × (2,228,095 cells × 24 B + 2 × 34,814 bitmap words × 8 B)) |
| `df` | — | — | 18,690,607,153,152 B (17T), 0 used |
| `pwrite 1m` + `fsync` | 23 ms | 1,225,728 B | sha equal |
| `pwrite 256m` + `fsync` | 1.22 s | 269,171,712 B | sha equal; `df` 260M used; RSS +370 MB (the write cache's residents) |
| orderly umount | 0.37 s | COW 593,152 sectors (289.6 MiB) in all | clean unmount, checkpoint seq 3 |
| `fsck --offline --scrub` | 0.60 s (loop set: 0.65 s) | — | maxrss 1,913,476 KiB vs 1,773,524 KiB (+140 MB: the tables + C6's bitmap words); 65 blocks checked, scrub 65 blocks = 269,484,032 B — exactly the used blocks; 706 entries, 26 nodes; 0 findings |

No O(capacity) walk: the format plan writes two chunks and two sectors per data volume
(`data_superblock.planFormat`), the meta format its fixed region; the mount's allocator seeds from
the durable references (O(refs)); fsck's C2/C6 walk `highest` (the fresh-mint cursor), the scrub
the referenced mappings; `df` and `.stats` are arithmetic. The one size-proportional cost is the
allocator's table — RAM, not device: **24 B + 2 bits per block, non-sheddable** (block_alloc.zig's
module doc: 25 M blocks of a 100 TB volume = 600 MB). Pinned: `data_superblock` test "the format
plan is O(structures), never O(volume)" — a 17 TiB passthrough volume's plan writes exactly
`2 × chunk + 2 × 4096` B, every step inside the first or the last chunk, the tail copy at
`volume_len − 4 KiB`, capacity `chunks − 2` as u64.

generic/620's own daemon log (the harness run): set config → volumes bound 18 ms → ready 258 ms
after the claim → stop requested → `dismount promotion: 0 staged` → clean unmount; 0 alerts.
**Verdict: generic/620 PASSES on the product; the failure was the harness's keying of a set.**

## 3. The format's limits ("what is the disk size limitation")

| quantity | width / law | where | at the default 4 MiB block |
|---|---|---|---|
| data volume length | u64 `volume_len` | `data_superblock.zig` wire [24..32); `set_config.DataVolume` | 16 EiB |
| capacity | u64 `capacity_blocks = volume_len / chunk − 2` (chunk 0 and the last are the identity sectors') | `transform.capacityBlocks`; `set_config`; `block_alloc.capacity_blocks` | 2^64 blocks |
| per-file size | block index u32: `EFBIG` when `(offset + len) / block_size ≥ 2^32 − 1` | `write_admit.zig` `write`/`zeroExtension`; `key.BlockMapKey.block_index` u32 | 16 PiB per file (16 TiB at a 4 KiB block) |
| inode numbers | u64 keys; `INO_SPACE` 2^56 (statfs `f_files`); 2^40 per forest slot × 65,537 slots | `meta/key.zig`, `kv_fs.zig` | — |
| block size | a power of two in [4 KiB, 4 MiB]; the default 4 MiB is a constant, never derived from the volume | `transform.MIN/MAX/DEFAULT_BLOCK_SIZE` | — |
| allocator table | 24 B cell + 2 bits per block, RAM, non-sheddable (`tableBytes`) | `block_alloc.zig` | **6.0 MB per TiB** (54 MB per 8.5 TiB measured); 600 MB per 100 TiB; 6 GB per PiB — ×1024 at a 4 KiB block: the practical bound is `volume_len / block_size × 24 B` against the host's RAM |
| fresh mints per era | 40-bit stamp sequence | `block_alloc.STAMP_SEQ_BITS` | 2^40 blocks = 4 EiB per mount era |
| data volumes per set | 4096 | `set_config.MAX_DATA_VOLUMES` | 4096 × 16 EiB |
| metadata members per set | 64 | `uri.MAX_MEMBERS` | — |
| metadata journal | `clamp(volume_len / 64, 8 MiB, 32 MiB)` | `geometry.journalRingLen` | — |
| admin reply `capacity_blocks` | u32, clamped — reporting only (`volume add-data`'s reply) | `ops/jobs/admin.zig`, `coordinator.zig` | saturates at 2^32 blocks = 16 EiB |
| statfs | u64 blocks of 4 KiB | `kv_fs.zig` | — |

Nothing on the device side caps a volume below u64; the host's RAM for the table is the one
proportional cost, by design.

## 4. Seen, not done

- The venue exclusions `generic/108, 349–351, 704, 730, 731, 767, 803, 804` (scsi_debug) and
  `generic/563` (a loop over the scratch loop) cite the harness's old refusal as their reason —
  stale now: the raw-device arm keys them. Not re-run in this window (they widen the release
  run; the owner's call).
- `/run/squeezefs-suites.env` and the `/sbin/*.fuse.squeezefs` symlinks are venue-global
  (pre-existing): two fstests harnesses on one venue would fight over them.
- The idle daemon's RSS (3.2 GB on a 17 GiB set) is the RAM-percentage caches' (write cache
  7,027 MiB memfd, read cache 7,027 MiB), independent of the volume; not this task's.
