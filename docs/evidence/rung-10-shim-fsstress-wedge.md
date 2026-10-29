# Rung 10 — the shim's fsstress wedge: a notify the kernel sleeps on parked the sideband (2026-10-29; decision 0003 §CK)

Found by the FIEMAP agent (`rung-10-fiemap-interposer.md`, "Seen, not fixed"; decision 0003 §CJ.10 (a)): generic/013 under
`FSTESTS_PRELOAD` wedged the mount 4 of 4 runs. Pre-existing on `dev` `0778bb4`; venue `local`
(`7.3.0-1.7-mainline-sqz`). Rig `~/tmp/shim-wedge/fsstress.sh`: a 2 MDT + 2 OST loop-image set of its own,
`-o interception`, xfstests' `fsstress` with `libsqueezefs_il.so` preloaded, every thread's kernel stack at a
stall, fsck after every run.

## Scenario and cause, with proof

generic/013's second pass, `fsstress -p 20 -r -m 8 -n 1000` (4–9 s when it completes), stalled 7 of 11 runs
on `0778bb4`: one ran on to the kernel's 135 s request timeout (the connection aborted, nine awaited attrs
invalidations failing at the abort), six were captured at 26–30 s with 9–26 requests waiting. In every capture:

- **The sideband loop sleeps in the kernel**: `sqz-fuse-sb` in D, `io_uring_enter → io_submit_sqes → io_write →
  fuse_dev_write → fuse_notify →` `fuse_reverse_inval_entry` (the kernel-view notifier's INVAL_ENTRY, its
  parent's `i_rwsem` held; 3 captures) or `fuse_reverse_inval_inode → invalidate_inode_pages2_range →
  folio_wait_bit_common` (the limiter's whole INVAL_INODE, a folio a write-through WRITE or a READ holds
  locked across its request; 3). io_uring issues the write INLINE: `/dev/fuse` polls EPOLLOUT
  (`io_file_supports_nowait`) and the device ignores IOCB_NOWAIT.
- **The holder is a request the daemon cannot finish**: in r3 fsstress's `mkdir` holds the parent
  (`fuse_mkdir → request_wait_answer`, ten more mkdirs in D at `filename_create`); daemon side it sits past
  `[meta_backend_start]` beside a write past `[write_locked]` and two syncfs past `[write_admitted]`.
- **Lanes wait on the loop with their thread**: 3–10 FUSE lanes in `futex_wait`; r3's user stacks: `ln1` in
  `invalSyncThunk ← Invalidator.onWrite`, `ln3`/`ln4` in `Sideband.takeNbCell ← invalSyncThunk ← onWrite` —
  IPC write handoff fibers owing the awaited attrs-only INVAL_INODE of a grow (§CE.3), all eight reserved
  cells published, none written. A lane thread in a raw futex runs none of its other fibers.

The cycle: loop ← lock ← request ← its fiber (or what it waits on) on a parked lane ← the lane's wait on the
loop. No shim lock, session or slot takes part; only a ring write awaits the sideband from a lane, and fsstress
mixes growing writes with the directory mutations and buffered I/O the notifies sleep on.

## Red (`4591761`) and fix (`eee0b89`)

Two `fuse.transport.session` e2e rows over EchoFs's park seams: an INVAL_ENTRY on root while a LOOKUP parked in
its handler holds root's `i_rwsem`; a whole INVAL_INODE of `/park` while a buffered read parked in its handler
holds the folio (EchoFs's new `cached_opens`). Behind either, an awaited attrs-only INVAL_INODE must return
within 5 s and a reserved one be written while the sleeping frame stays unwritten; released, it lands. Both
fail at `expect(awaited_done)` on `0778bb4` (the second with the fix's one line disabled).

The fix: `SbCell.sleeps` marks a frame the kernel may sleep on — INVAL_ENTRY, a page-invalidating INVAL_INODE
(`off >= 0`), PRUNE — and `submitWrite` issues it `IOSQE_ASYNC`: an io-wq worker sleeps, never the loop; the
shared pool bounds the workers (a cell is held to its CQE). The reserved class and the replies stay inline:
they never sleep, so §AB.2's "a reserved cell's write is one sideband round" holds again. No lock-free core
is touched (`sleeps` is written before the publish, read after the pop: the ready ring's own pair).

## Checks (the fix's binary; every run fsck 0 findings, no watchdog or tripwire line)

- fsstress under the preload, 33 min of load in four chunks: `-p 20 -r -m 8 -n 1000` 20/20 (4–9 s);
  `-p 32 -n 10000` 8/8 (12–36 s); `-p 32 -n 20000` 4/4 on 24 GiB volumes (207–219 s), 7/7 on 12 GiB (92–146 s).
  One `-p 32 -n 10000` run: 24,312 ring writes, 3,684 ring reads, 1,903 binds, 26,570 notifies.
- xfstests under `FSTESTS_PRELOAD=1` (the harness, `b4b0610` on `dev`): generic/013 11/11 (16–20 s, 5–74 IPC
  sessions per mount); generic/001, 070, 117 pass; 083 and 269 `_notrun` (`_scratch_mkfs_sized`).
- The two rows 20/20 (`-Dfresh=true`); unit filters `shim.` 79/82 (3 skips), `ipc.` 154/158 (4), `sdk.` 60/60,
  `fuse.transport.session` 66/83 (17 kernel-feature skips); smoke `INTERCEPTION` 42/42, `DIRECT ARM` 40/40,
  `INVALIDATION` 40/40; `zig build shim-smoke` 43/43; `zig build fmt rails` 32/32.
- Rebased onto `dev` `74d419c` (no change to the sideband, the limiter or the IPC host since `0778bb4`):
  `74d419c` itself wedged 2 of 6 `-p 32 -n 10000` runs (the sideband in `invalidate_inode_pages2_range →
  folio_wait_writeback` and in `fuse_launder_folio → folio_wait_writeback`, 6–11 lanes in futexes) and
  outran 90 s in 2 more; with the fix `-p 32 -n 10000` 6/6 (17–34 s), `-p 20 -r -m 8 -n 1000` 15/15; generic/013
  3/3, 001/070/117 pass; the rows, `shim.` 83/86, `ipc.` 158/162, `sdk.` 61/61, `fuse.transport.session` 68/85;
  smoke `INTERCEPTION` 43/43, `DIRECT ARM` 41/41, `INVALIDATION` 41/41; `shim-smoke` 45/45; `fmt rails` 32/32.
  Then onto `b2820d7` (one teardown change in `cli/mount.zig`): the rows, `fuse.transport.session`, `shim.`,
  `ipc.`, `fmt rails` re-run green.

## Seen, not changed

- A lane fiber that waits on the sideband (the awaited grow, a reserved cell, a classical reply's cell) still
  parks its lane THREAD; `sync/completion.zig`'s routed face would park only the fiber. Bounded by one loop
  turn now; owed as hardening (§CK.6).
- generic/013, one run: a daemon panicked at an orderly unmount, `fsync.zig:734` `error.Canceled =>
  unreachable` — §CG.5 (b), fixed on `fix/meta-owed-followups` (`cc1f99b`), not on `dev`; the test passed.
- **`open_count_stranded` fires on `74d419c`** (a final FORGET with an open count — a lost RELEASEDIR under
  no-open): 231–1,311 per `-p 32 -n 10000` run without the fix, 64–912 with it and no preload at all; never on
  `0778bb4` (0 in 50 runs under the preload, every shape, with and without the fix). A regression in `0778bb4..74d419c`
  (small files through the write cache `74cd347`, the fsync scope `d4275eb`, FIEMAP), not the shim's nor this fix's.
- `-n ≥ 10000` fills these sets: ENOSPC at the sync point, `dismount promotion: … FAILED`, the supervisor
  holding the residents (`rung-10-full-store-enospc.md`, §BR) — by design, not this defect.
