# Rung 10 — fallocate: every mode's law (2026-10-23; charter §6.6; decision 0003 §BH; 0002 row 554)

**What changed.** `fallocate(2)` was the adjudication audit's largest gap (F1): every mode answered
EOPNOTSUPP, LTP fallocate03 and mremap06 sat in the ledger for it, 31 xfstests of the auto group
`_notrun` behind `_require_xfs_io_command falloc`. The owner's ruling: implement it; the bar: a
reservation is real (a later write inside it cannot ENOSPC) and a 1 GiB fallocate must not write
1 GiB. Branch `feat/fallocate` off `316460d`.

## The law per mode (`kv_fs.fallocate`, under the lifecycle's EXCLUSIVE level-1 guard)

| mode | law |
|---|---|
| 0 / `KEEP_SIZE` / `UNSHARE_RANGE` | every striped block of `[off, end)` the map does not bind is minted and bound **UNWRITTEN** (`WritePath.reserveRange`: a durable reference, `+ref` in the publish, charged to the volume at once, no device byte; one merge per `max_displaced` blocks). An inline range reserves nothing (the record IS the storage); a staged/packed file promotes to striped first. Without `KEEP_SIZE` the size rises to `end` through `setattr`'s truncate-up path (§AL's zeros on a mapped EOF block, the size epoch, mtime/ctime). Idempotent: a re-reservation changes and stamps nothing |
| `PUNCH_HOLE \| KEEP_SIZE` | `[off, min(end, size))`: the custody settled, the whole blocks leave the map (`MapSeam.punch`: the inline map republished without them, one tx per `max_displaced`; a tree-mapped file's `kvmap.stagePunch` re-canonicalizes the straddling runs into head and tail) and are freed after the commit; a partial edge inside a WRITTEN block is zeroed through the write admission (W1 in place when the piece admits); an edge in a hole or a reservation writes nothing; inline → the record rewritten, staged/packed → the image re-staged. The size never moves |
| `ZERO_RANGE` (± `KEEP_SIZE`) | the edges as the punch's; every whole block flipped to its unwritten twin (same reference, no I/O, purged from every tier) or reserved fresh; the size rises as allocate's without `KEEP_SIZE` |
| `COLLAPSE_RANGE` / `INSERT_RANGE` | at the BLOCK grain only (EINVAL below it — the stripe block is the filesystem's block for fallocate(2)'s alignment law): the inline map re-indexed in ONE tx, the data unmoved (`MapSeam.shift`, `Publish.shift`'s proof exemption, `shiftFits`), a collapse's blocks freed, the size committed last. A tree-mapped head or a shift past one journal entry: EOPNOTSUPP, counted. **The kernel's FUSE client forwards neither** (below) |
| screens | the u32 block ceiling for every mode naming blocks up to `end` (EFBIG); a volume-less set past its inline bound (EFBIG); a directory EISDIR; zero length EINVAL; the VFS's combination law (PUNCH needs KEEP_SIZE, never with ZERO; COLLAPSE/INSERT alone) |

## The on-disk choice and why

`M_UNWRITTEN` (bit 2 of the 40 B mapping wire; whole-block only, never DAMAGED; the tree carries it as
a kind-2 record). A reservation is a REFERENCE the allocator, `df`, fsck's census and the mount's
recovery seed already know; it needs no DMA proof (the proof law guards a type flip's bytes; an
unwritten binding names none) and its flip to WRITTEN is the same reference (no `±ref`). The two
designs refused: zero-filled blocks written at fallocate (1 GiB of DMA for 1 GiB — the owner's bar)
and ext4's reservation COUNT (durable only through a per-inode range list every publish splits,
re-derived by a full layout scan at mount, invisible to the allocator and fsck).

**One hide, two consumers** (§BH.2): `MapSeam.lookup` and the read seam answer an unwritten binding as
a HOLE; `MapSeam.reserved` answers it to the admission's custody probe (a sole-owned reservation is
custody — admitted on an exhausted store) and to the landing's `accumulate.pickDest`, which writes the
whole image INTO the reserved block under W1's fence pair and publishes the written twin durably at
once (never through the rewrite epoch: a shadowed takeover would be freed as "the epoch's mint" on a
destroy's `NotFound` while the layout names it). A clone's share, an epoch's binding or an open
overlay record decline the takeover: the fresh-mint path displaces the reservation (`−ref`, freed
after the commit) and the epoch parks it as the displaced A key, so the drift tripwire stays silent.
The read choke point refuses an unwritten binding that reaches it (`unwritten_mapping_served`,
must-stay-0). The reference reserved nothing (0002 row 554).

## The kernel's law (OWNER-CALL)

`fs/fuse/file.c` `fuse_file_fallocate` (mainline 7.3 and the 9.33 series alike): `mode &
~(FALLOC_FL_KEEP_SIZE | FALLOC_FL_PUNCH_HOLE | FALLOC_FL_ZERO_RANGE)` is `-EOPNOTSUPP` before any
request is formed. **COLLAPSE_RANGE, INSERT_RANGE and UNSHARE_RANGE never reach a FUSE daemon.** The
daemon's arms are implemented and tested through the dispatcher (the harness) and are reachable by
nothing else until a kernel patch forwards them — a UAPI-semantic change (the kernel must truncate
the page cache from `offset` and move `i_size` for a shift) that belongs upstream (`scripts/kernel/
upstream/`), not in the series alone. xfstests' `_require_xfs_io_command fcollapse` probe meets the
kernel's EOPNOTSUPP and `_notrun`s the fcollapse/finsert rows exactly as before; the smoke row pins
the kernel's verdict and that the daemon's shift counters stay 0. Owner's call: a kernel patch (and
whether the block-grain law is the one to forward), or the arms stay dispatcher-only.

## Red tests (shown red on `origin/dev`: every xfs_io command "Operation not supported"; the kv_fs
test "the rung-4 ops default to ENOSYS" pinned `!@hasDecl(MemFs, "fallocate")`)

- `data.layout`: the wire pin, the law, the choke point's tripwire, the kvmap kind-2 round trip, the
  publish's proof exemptions (a reservation; a same-reference flip) and the shift arm's.
- `data.write_admit`: "the fallocate reservation" (3 holes reserved, the written block untouched, no
  device byte, charged, idempotent; the write inside lands in place — same reference, no mint, no free,
  W1 takes the next rewrite; ZERO_RANGE flips with no I/O; a clone-shared reservation declines, is
  displaced, no drift) and "under a FULL store" (a hole's write ENOSPC before any byte; the
  reservation's admitted and landed in place; a refused reservation returns its mints).
- `data.kvmap`: `stagePunch` — head/tail of a run, a superseding point's own binding, a reservation in
  the range, idempotent, the save's width per call.
- `fuse.handler.kv_fs`: the mode law and the inline arms on a volume-less set; every striped arm through
  the recording Data face (what the handler asks IS the contract).
- smoke row **FALLOCATE — EVERY MODE'S LAW**: `falloc -k 0 12m` → size 0, `df` +12 MiB, `own_block_dmas`
  0, `write_through_blocks` 0; `falloc 0 12m` → size 12 MiB, every byte zero; `pwrite 4m 1m` + fsync →
  `df` unchanged, `reserved_landings` ≥ 1, the pattern exact, the block's rest zeros; `fpunch 4m 4m` →
  `df` −4 MiB, zeros; `fpunch 100 200` inside a written block → `cd cd cd cd 00 … 00 cd cd cd cd`;
  `fzero -k 8m 4m` → `df` unchanged, zeros, `fallocate_blocks_zeroed` ≥ 1; `fcollapse`/`finsert`/
  `funshare` → the kernel's EOPNOTSUPP, the file whole, the daemon's shift counters 0; the full store:
  20 × 16 MiB files to ENOSPC, a hole's `dd` refused, the reservation's 8 MiB `pwrite` admitted and
  landed; kill -9 → `fsck --offline --scrub` clean → remount → every law re-read → clean unmount →
  fsck clean; no error line, no tripwire.

## The suites

- LTP by hand (`/build/ltp/testcases/bin`, `TMPDIR` on a fresh mount of a 2 × 2 GiB set):
  `fallocate03` **8/8 pass** (was 8 × EOPNOTSUPP), `mremap06` **3/3 pass** (was TBROK at its
  `fallocate()` setup); no daemon alert, fsck clean. Their ledger rows are deleted.
- xfstests: the `auto`-group members gated on `falloc`/`fpunch`/`fzero`/`fcollapse`/`finsert` (171 tests;
  `grep -l '_require_xfs_io_command "f…"' tests/generic/*` ∩ `auto`). Pass 1 (through `scripts/suites/
  fstests.sh`, fail-early): 157 ran before the stop — **29 PASSED** (generic/008 033 059 071 086 103 213
  214 228 286 299 311 315 347 391 422 436 439 445 446 469 512 539 567 568 586 590 610 627 — every one
  `_notrun` on `origin/dev`), 127 `_notrun` for another capability (62 + 13 reflink, 21 FIEMAP, 10
  `_scratch_mkfs_sized`, 5 + 2 fcollapse/finsert at the kernel, 4 swapfiles, 3 shutdown, 2 dax, FITRIM,
  quotas, defrag, geometry), **1 FAILED: generic/703** — the finding below. Pass 2 (generic/703 and the
  14 tests after it): RESULTS BELOW.

## The finding: the sync law (generic/703; pre-existing, fixed here)

generic/703: `falloc 0 256M`, fio `ioengine=io_uring direct=1 bs=64K sync=1 rw=randwrite` for 10 s, a
dm-flakey power cut, the digest must stand. It differed in EVERY 64 KiB slot — and the by-hand repro
(a kill -9 instead of flakey) differed the same way on a file WRITTEN with zeros instead of fallocated:
not a fallocate defect. `.stats` after the fio run: `fsync_calls 1` for 137 k O_DSYNC writes,
`patch_ineligible_overlay 112 k`, `parked_buffers 16`, `writeback_timer_drains 18`. The mechanism:
the kernel's FUSE DIRECT-I/O path sends no FUSE_FSYNC behind a dsync write — `fs/fuse/file.c`
`fuse_write_flags` ORs the iocb's `O_DSYNC`/`O_SYNC` into `fuse_write_in.flags` and leaves the sync
to the daemon (only the buffered path's `generic_write_sync` sends an FSYNC). The daemon ignored the
flags: every O_DSYNC write was acked into RAM custody and landed by the write-back timer within
`T_wb`; a kill inside that window lost it, while the application had been told it was durable.
**The sync law** (`kv_fs.write`): a WRITE whose open flags carry `O_DSYNC` (or `O_SYNC`) runs the
fsync ladder before its ack (`datasync` for DSYNC, the stamps too for SYNC; `fuse_write_sync_fsyncs`).
By hand after the fix: 0 differing bytes on both files; the write pattern turns honest — the first
landing takes the reservation over, every later 64 KiB write is a W1 patch in place (`patch_writes`
30,682 of 31 k) at the fsync rate (10.4 k IOPS / 650 MiB/s, from the lying 44 k).

## Counts

- unit filters green: `data.layout`, `data.kvmap`, `data.write_admit`, `data.accumulate`,
  `data.device_overlay`, `data.write_epoch`, `data.writeback`, `data.fsync`, `data.plane`,
  `data.read_path`, `ops.fsck`, `fuse.handler.kv_fs`, `fuse.handler.lifecycle`,
  `fuse.handler.filesystem`, `stats`; `zig fmt --check` clean.
- loops under the venue's load (two other agents' suites and a full gate beside): the new unit tests
  (`fallocate`, `UNWRITTEN`, `punch of a tree-mapped`) **100/100**; the smoke row **20/20**.

## Seen, not fixed

- **FIEMAP** (`FS_IOC_FIEMAP`): 21 of the subset `_notrun` "xfs_io fiemap failed" — the ioctl is not
  served (a capability gap of the same family as fallocate was; the reservation would report
  `FIEMAP_EXTENT_UNWRITTEN`).

- `st_blocks` is `ceil(size / 512)` as before: a `KEEP_SIZE` reservation of an empty file reports 0
  blocks (ext4 reports the allocated ones). xfstests rows that judge `du` after `falloc -k` would see
  it; none of the auto-group rows run so far do.
- A `KEEP_SIZE` reservation past EOF is clipped by a later truncate down exactly as data blocks are
  (ext4's law too); a truncate UP over a reservation exposes zeros (the hide), never the block's bytes.
- A punch's or zero's partial edge inside a written block on a TRANSFORMED volume cannot land in place
  (no W1 there): it CoWs through accumulation and can ENOSPC on a full store, as a write there would.
- The writeback worker (`writeback.flushOne`, the file-backed ring's spill path) still mints fresh for
  every unit: no product plane wires it (`fsync.Ctx.wb_deps` is null everywhere), so no takeover there.
