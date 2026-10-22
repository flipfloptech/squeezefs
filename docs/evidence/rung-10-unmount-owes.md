# rung 10 — an orderly unmount lands every acked write (fstests generic/029)

**Finding** (2026-10-22, `-g auto` run 1 on dev `3492203`, kernel `7.3.0-1.7-mainline-sqz`):
generic/029 — 5,120 B written, `truncate 2048`, `truncate 5120`, the tail `[2048, 5120)`
rewritten through a mapping, close, `_scratch_cycle_mount` — read the tail as ZEROS after the
remount; both daemons' logs 0 alert lines, fsck 0 findings. It passed on the sqz kernels on
2026-10-17/18.

## The three-way split (by hand, the exact xfs_io sequence and 17 variants)

| posture | 029 case 1 | the same without mmap (pwrite) | overwrite inside a landed file, unmount at once |
|---|---|---|---|
| default (write cache, `ZC_OPEN=1`) | LOST | LOST | whole (≤ block) / **LOST (striped, > block)** |
| `SQUEEZEFS_FUSE_ZC_OPEN=0` | LOST | LOST | — |
| `SQUEEZEFS_WRITE_CACHE_PCT=0` | LOST | LOST | **LOST** |

Neither the mainline open posture nor the mapping is the cause; `fsync` before the close
lost the tail PRE-remount with the cache on and kept it with the cache off — two mechanisms.

## Mechanisms

1. **The staged arm's §L.6 shortcut** (`kv_fs.writeStaged`): a size-neutral write "inside the
   image" parked its stamp and published no layout — judged by the INODE's size. A truncate-up
   over a staged file raises the inode's size alone (the record stays at the truncate-down's
   length, `stageTruncate .staged`), so the tail write re-staged a 5,120 B image under a record
   saying 2,048. `promoteLocked` dispatches on the record's size and `promoteInline` clips the
   image to it: the tail left the ring at fsync, LRU, recall or the dismount pass. Silent until
   then: reads were served from the resident.
2. **The dismount sweep landed the size claimants' custody alone** (`landClaims`,
   `PendingTimes.claimants`). Under the write-back timer close is no durability point; an
   overwrite inside a file parks custody and NO claim, so an orderly unmount within `T_wb`
   left its bytes in RAM — any posture, any file past inline that is not a ring resident
   (no cache: every one; with the cache: every striped file). Charter §6.2 (the write-back drain within `T_wb`): the unmount
   "drains synchronously as before" — it did not.

## The fix (`2eaf980`)

- The shortcut requires the record to NAME the image it leaves: an in-place overwrite, or a
  record whose size is the rebuilt image's; else the tx publishes `Layout.staged(size)`. Law on
  the line: the record's size is the image's length (the promotion dispatches on it).
- `destroy` → `landCustody` → `DataPlane.flushAllCustody` → `WritePath.flushAll`: every ino the
  syncfs census finds (parked blocks, overlay records, epochs) through `flushIno` — the RELEASE
  drain — before the times drain journals the claims. `claimants` had no other reader; gone.

## Red first

- Smoke rows `AN ORDERLY UNMOUNT LANDS EVERY ACKED WRITE` WITH / WITHOUT the write cache: `t`
  (the dance), `s` (a striped overwrite, custody parked), `v` (the dance + fdatasync); the
  orderly unmount the moment the last write returned → remount → whole; then `v` killed -9 →
  fsck clean → remount → whole. Red on `3492203`: with the cache `v-LOST` on the writing mount;
  without it `t-LOST s-LOST` after the remount. Green 20/20 (both rows, 40 runs).
- Unit `the dismount sweep lands EVERY write custody` (kv_fs, SettlingData): red at
  `hasWriteCustody` after `destroy`; green.

## Counts

Unit filters kv_fs 98/98, write_admit 78/78, fsync 92/92, economy 66/66, staging 66/66, plane
174/174, open_table 52/52; rails 32/32 (the rig sync rail caught `sync <file>` — the row uses
`sync -d`); smoke §AF 52/52, §AJ 51/51, WRITE-BACK TIMER 42/42, the write-cache / TWO MOUNTS /
RECALL / promotion-kill / `sync -f` / fsync-fdatasync-syncfs / KILL MATRIX / ORDERED LAW rows
green. Suite subset `generic/029 003 001 074 075 127`: 029, 001, 074, 075, 127 pass, 003 the
ledgered noatime row; 0 alert lines, fsck 0 findings on both sets. The hand matrix (17 cases ×
3 postures) whole.

## Seen, not fixed

- No `tests/costs` rows for the two new smoke rows (hang limit 900 s; ≈ 10 s measured) — the
  next `zig build costs` adds them.
- The durability law is full tier: ONE full venue gate before landing (not run here by the
  lead's instruction).
