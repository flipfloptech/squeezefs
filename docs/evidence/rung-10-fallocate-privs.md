# Rung 10 — fallocate strips the setid bits; the shipped setattr carries the kill bit (2026-10-23; decision 0003 §BK)

**The symptom.** The full xfstests `-g auto` run on `d5a8e45` ran 673 tests and stopped at generic/683
(`perms prealloc`): `xfs_io -c 'falloc 64k 64k'` by `qa_user` over root's 192 KiB file with mode
6666 / 6676 / 6766 / 6777 / 2676 / 2777 must leave 666 / 676 / 766 / 777 / 676 / 777 (root keeps every
bit). Ours left every bit in place. Create 7b's report (handoff 2026-10-21) had the sibling: the SHIPPED
setattr carried no `kill_suidgid`, so a foreign-slot truncate did not strip S_ISUID/S_ISGID at the holder.

**What the kernel sends** (`/build/kernel/linux-mainline-sqz`, 7.3). `fs/fuse/file.c`
`fuse_file_fallocate`: `inode_lock` → writeback of the range (PUNCH/ZERO) → `inode_newsize_ok` →
`file_modified(file)` → `fuse_simple_request(FUSE_FALLOCATE {fh, offset, length, mode})`. `file_modified`
→ `file_remove_privs` → `__remove_privs` → `notify_change(ATTR_FORCE | ATTR_KILL_SUID | ATTR_KILL_SGID)`
→ `fs/fuse/dir.c` `fuse_setattr`, which DROPS `ATTR_KILL_SUID | ATTR_KILL_SGID | ATTR_MODE` whenever the
daemon handles killpriv (v1 or v2: "the only sane way to reliably kill suid/sgid is to do it in the
userspace filesystem"). What reaches the daemon: a SETATTR whose `valid` names no mode and no kill bit,
then the FALLOCATE. `FUSE_HANDLE_KILLPRIV_V2` sets `FATTR_KILL_SUIDGID` only on a chown and on a size
setattr without CAP_FSETID (`fuse_do_setattr`), `FUSE_WRITE_KILL_SUIDGID` on WRITE, `FUSE_OPEN_KILL_SUIDGID`
on an O_TRUNC open. The capability xattr the kernel drops itself: `ATTR_KILL_PRIV` → `security_inode_killpriv`
→ REMOVEXATTR `security.capability` (generic/688's law, root included).

**The law.** `fs/attr.c` `setattr_should_drop_suidgid` on a regular file: kill S_ISUID when set; kill
S_ISGID when set and (the file is group-exec OR the caller is outside its group — `setattr_should_drop_sgid`,
6.2+); nothing when the caller has CAP_FSETID. `setattr_prepare`'s group rule plays no part (skipped under
ATTR_FORCE). `economy.fallocateStripMode(mode, Modifier{cap_fsetid, in_group})` restates it; the caller's
standing comes from `/proc/<pid>/status` of the header's pid (`CapEff` bit 4, the effective `Gid`, `Groups`),
falling back to uid 0 / the header's gid when the pid is gone. The handler applies the strip after the
fallocate's own commit as a mode setattr (`setattrJudged`; a foreign slot ships it), counted
`fuse_fallocate_setid_strips`. The reference strips both bits for every uid ≠ 0 — a quirk; on every local
filesystem a member's non-exec sgid survives.

**The shipped bit.** `verbs.SETATTR_KILL_SUIDGID` (1 << 8; `SETATTR_KNOWN` 0x1FF), `record_ship.Setattr.
kill_suidgid` through `toWire`/`fromWire`, the holder's `.setattr` arm applies `killSuidgidMode` (suid always,
sgid iff group-exec — FATTR_KILL_SUIDGID's documented meaning), deletes `security.capability`, counts
`record_killpriv_strips`. `kv_fs.setattr`'s ship passes `a.attr.kill_suidgid`.

**Red.**
- smoke "FALLOCATE STRIPS THE SETID BITS" over dev's product files: the matrix came back
  `6666 196608 / 6676 / 6766 / 6777 / 2676 / 2777 / 2666` for `666 / 676 / 766 / 777 / 676 / 777 / 666`.
- kv_fs "fallocate strips the setid bits through the dispatcher" without the strip call: `expected 438,
  found 3510` (0o666 / 0o6666).
- two_holders "a shipped setattr CARRIES the kernel's kill_suidgid" with the holder arm disabled:
  `expected 34230, found 36278` (0o2666 / 0o6666).
- The first draft of the law ("sgid iff group-exec" + a group rule on the result) kept a non-member's 2666;
  the smoke row's matrix against the kernel source corrected it to `setattr_should_drop_suidgid` (2666 → 666
  for a non-member, kept for a member).

**Counts** (the `local` venue, 7.3.0-1.7-mainline-sqz, 2026-10-23).

| What | Result |
|---|---|
| kv_fs "the fallocate setid strip" / "fallocate strips the setid bits through the dispatcher" | 100/100 each (`-Dfresh=true`) |
| two_holders "a shipped setattr CARRIES the kernel's kill_suidgid" | 100/100 |
| smoke "FALLOCATE STRIPS THE SETID BITS" | 10/10 |
| filters `fuse.handler.kv_fs` / `fallocate` / `setattr` / `sym.xv.record_ship` | 111/111, 48/50 (2 smoke skips), 46/49 (3 smoke skips), 40/40 |
| `zig build rails`, `zig fmt --check` | green (the `/proc` radixes and CAP_FSETID documented on their lines) |
| xfstests `generic/683 684 685 686 687 688 193 355`, SUITE_RUNS=1 | `check rc=0, ran 8, not run 2, failed 0, Passed all 8 tests`; 0 alert lines on set-test (8 daemon logs) and set-scratch; 686/687 notrun (`xfs_io finsert`/`fcollapse` failed: FUSE answers EOPNOTSUPP to INSERT/COLLAPSE_RANGE, as on dev); the "did NOT pass … ledger rc 3" line is the subset's stale-ledger rule (7 stale rows) |

**Seen, not fixed.** The WRITE/SETATTR kill law ignores the caller's group: a non-member's truncate over
6766 leaves 2766 where ext4 leaves 766 (`!in_group`). The daemon has the writer's standing at that site but
the shipped verb would have to carry the computed mode or the standing; no fstests row names it (§BK.5).
