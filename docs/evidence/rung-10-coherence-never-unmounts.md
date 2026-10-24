# A daemon never unmounts what stands on its tree (xfstests generic/410; decision 0003 §BL)

## What failed

generic/410 on `9d9e3ce` (run 4, "after make-shared run on private"): after the mount at `mpB/dir`
the listing lacked `mpB` and `mpB/dir`; `umount mpB/dir` answered "no mount point specified"; the
mount at `mpC/dir` answered `NOENT`; the next listing held only `TEST_DIR/410`. No daemon stopped
anything in that window: the mpB/dir daemon logged `the FUSE session ended (unmounted)` (432.221)
and the mpA daemon `mpA is no longer attached` (433.196), neither a `stop requested`; the helper's
`mounts.log` has no `umount mpA` for the run. The kernel detached them.

## Mechanism

`d_invalidate` of a dentry detaches every mount on it. FUSE runs it at once for an
`INVAL_ENTRY` without `FUSE_EXPIRE_ONLY`, and at the next walk for every dentry older than an
`INC_EPOCH` (`fuse_dentry_revalidate`'s epoch arm precedes any LOOKUP). `mpA`/`mpB`/`mpC` sit on
the head daemon's dentries and the `mpX/dir` mounts on the mpA daemon's `dir`: an epoch in either
throws them off one walk at a time — the 410 cascade (`mpB` at fsstress's walk, `mpC` at the
`mpC/dir` mount's, `mpA` last).

Traced outside the harness (run 4 emulated: the head, A at `head/mpA`, binds, a joiner per
`mpX/dir`, fsstress; kprobes on `fuse_notify`, `fuse_reverse_inval_entry`, `__detach_mounts`):
every epoch of every daemon was §AB.2's substitution (`_entry_epochs_substituted` ≡
`_epochs_sent`; 5–27 per fsstress run per joiner, 1 per round on A). Rounds 5 and 8 of 13 caught
it: `lookup_fast → d_invalidate → __detach_mounts name="dir"` after A's extra substitutions (3
that round). Deterministic: a tmpfs on the manager's dentry, then a member's join (the first-join
whole-view epoch) — gone at the next walk.

6b10668's pre-`umount2` verdict was not the cause (it never ran in the window), but it did count
the copies the same `umount2` propagates away and abort under them (~90 warnings per 409/410 run).

## The law

Decision 0003 §BL: `INVAL_ENTRY` expires (`FUSE_EXPIRE_ONLY`); the substitution waits for the
parent's answer (`transport_notify_entries_deferred`); an owed epoch is withheld while a mount
stands on the connection (`transport_notify_epochs_withheld`); the held-elsewhere verdict runs
after our own `umount2` with the superblock held by an `open_tree(OPEN_TREE_CLONE)` clone.

## Second defect on the way

The emulation's after-phase joiner died at `bindFsck` 2 of 2 rounds (General protection exception:
`foreignWindowInos` → `b.host.store.lookup` with `b.host` still `undefined` in `bind`). Fixed by
this branch's `fix(fsck)` (`storeFace`); 13 rounds since, every joiner mounted.

## Red → green

| Row | dev 7a6e224 | branch |
|---|---|---|
| e2e: a bind on `/hello` after INVAL_ENTRY / substituted / epoch | reads EchoFs's `hello` ×3 | "A COHERENCE NOTIFY NEVER UNMOUNTS" green |
| smoke bind row (d): tmpfs on the manager's `sub`, a member joins | `cat: sub/x: No such file or directory` | green, epoch withheld |
| smoke bind row (e): a propagated copy at the orderly stop | "1 other mount(s) … (…-peer)" + abort | no line, exit 0 |
| A DAEMON ENDS ONLY THE CONNECTION IT OWNS | green | green (A's id reissued to B) |

## Counts

- xfstests subset on the branch (binary `134b0f5`, before its rebase onto `9f4ee8d`; `FSTESTS_TESTS="generic/409 generic/410 generic/411 generic/589
  generic/732"`): `check rc=0, ran 5, failed 0` — 409 308 s, 410 696 s, 411 22 s, 589 373 s,
  732 10 s; the scratch set's alerts carry 0 `other mount(s) of this superblock` (dev: ~90 per
  409/410 run) and 0 withheld epochs (the substitutions are deferrals now).
- The emulation on the branch: 8 rounds, 0 detaches, 0 epochs on the joiners (dev: 5–27 each).
- Unit: `session` + `mountinfo` filters 127 pass, 20 venue skips (kmbuf on mainline, shim
  binaries); rails green; `zig fmt --check` clean.
- Smoke under load: the bind row, "A DAEMON ENDS ONLY THE CONNECTION IT OWNS" and the sideband
  row **20/20** beside five `-Dfresh=true` full smokes (plus 30/30 earlier, beside another
  agent's xfstests run); A's id reissued to B in 28 of 50 (the rest: another mount took it
  first); the sideband row: 0 of 59 samples in `fuse_reverse_inval_entry`, the name deferred 1.
  Full smokes 4 of 5 at 127/131 (4 venue skips: kmbuf, a looped-stage seam, two shim rows) with
  the umount (BUSY, the stop's own sender, `--lazy`, mount-id churn) and kill rows; the fifth
  red on THE TWO-DAEMON MATRIX's read-cache hit arm (0 hits in 5 re-reads at budget level 1
  under that load; green 2/2 alone, hit at the first re-read, level 0) — not this law.
