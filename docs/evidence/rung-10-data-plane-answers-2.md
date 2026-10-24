# Rung 10 — the data plane answers honestly on the paths §BQ left (2026-10-24)

**The report** (`rung-10-data-plane-answers.md`, "Seen, not fixed"): the interception host answered EAGAIN when
every lane refused its handoff; a failed truncate or punch commit left its re-stage under the old layout (reads served
zeros); fsync's parked leg answered ENOSPC for a device EIO; a write answered at the gate after earlier pieces landed
returned the error; 6,349 overlong-lease lines on a filled scratch; two functions read only by their tests.

**The laws (0003 §BU; charter §7 amended).**

- **The interposer's handoff waits.** Every lane refusing parks the op in its owner's queue (any thread pushes;
  the owner re-offers at each flush, its park bounded at 5 ms while sessions live); EIO only past the lattice's
  bound (the write path's stall bound) or for a torn-down session; never EAGAIN. Completions post straight to the
  slot, so a waiting op holds nothing the lanes need.
- **A failed re-stage is undone.** A staged truncate's clip and a staged punch's zeros re-stage the record the
  committed layout names before the commit; every path that leaves without the commit puts the pre-op image back
  under (3.7) (`fuse_restage_restores`).
- **fsync's wall speaks the device's class:** ENOSPC iff a landing since the leg began met the store's
  `StorageFull` or the device's own ENOSPC (now noted on every parked-landing DMA), else EIO.
- **The short count.** A later piece's error after earlier pieces landed returns the landed prefix's count; the
  error is the next call's. The kernel honours it: `fs/fuse/file.c` `fuse_perform_write` adds the reply's
  `out.size`, ends at a short one and returns the bytes so far (`fuse_direct_io` likewise). Caveat on 7.3: the
  buffered path marks fully copied folios uptodate before the request and no longer clears them on a short or
  failed reply, so the page cache can show an unwritten tail until evicted — as it already did for a failed write.
- **An overlong lease is not an escape** (the constant's own law: admission backpressure exceeds 1 s; the gate holds
  a payload it has not yet admitted): every one counts, one line per episode (closed after 10 s, two op-watchdog
  ticks, without one).
- **Dead code deleted:** `block_alloc.coreError`, `staging.resolveStagedRead` (and `StagedRead`).

| red test | origin/dev `eeec3e8` | branch |
|---|---|---|
| service "a handoff every lane refuses WAITS" (one meta lane, every fiber slot held) | -EAGAIN | runs when a slot frees, byte-exact; past the bound EIO |
| kv_fs "a staged truncate's and a staged punch's failed commit restore the record" | the 8 KiB clip under the 16 KiB layout; punch first: zeros across [4, 12) KiB | the old image |
| write_admit "fsync's parked leg answers the device's class" | ENOSPC for a device EIO | EIO; ENOSPC for a device ENOSPC |
| write_admit "returns the SHORT count" (2 pieces, the second answered at the gate) | EIO for the whole write | half a block; the next call EIO |
| slot "overlong payload leases" (8 leases held 2 s) | 8 error lines | 1 line, 8 counted |

**Numbers (local, 7.3.0-1.7-mainline-sqz).** The five red tests 40/40 (20 × 2 side by side). Unit filters
(write_admit, accumulate, enospc, lane, read_path, kv_fs, staging, nvme, fsync, geometry, plane, block_alloc, ipc,
transport, fallocate, placed): 946/1,018, 72 skipped. `rails` 32/32, `fmt --check` clean. Smoke (full store,
fallocate ×2, FAILED volume, THE INTERCEPTION HOST ×2, THE DIRECT ARM, the stalled plane, the 1,100-op storm, the
staged identity window, the write-back timer's fsync): 50/50. `zig build shim-smoke`: 41/41. xfstests `generic/347 299 750 475`: passed 4/4 (binary `0a8963a`, and again on the rebased head), fsck
0 findings on both sets, 0 `TRIPWIRE` lines, no kernel abort, 0 `serving N zero bytes` lines (475's FAILED cycles),
ONE overlong-lease line on the filled scratch (6,349 on `eeec3e8`'s binary, 5,980 on `a4d5d24`'s), the wall's 844
terminal ENOSPC lines unchanged in kind; ledger rc 3 is the subset's stale-row check.

**Seen, not fixed.** The 7.3 buffered-write caveat above (a kernel behaviour: the daemon could send
`FUSE_NOTIFY_INVAL_INODE` for an answered write's unwritten range). The wall's terminal ENOSPC is still one error
line per refused write (844 here) — the next candidate for the one-line-per-episode law.
