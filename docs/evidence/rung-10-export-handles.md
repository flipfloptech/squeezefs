# Rung 10 — export handles: `FUSE_EXPORT_SUPPORT`, the durable `..`, the one generation

**The gap** (`rung-10-adjudication-audit.md` §2.2, item F3). xfstests generic/426, 467, 477, 756, 777
— `name_to_handle_at` + `open_by_handle_at` after `echo 3 > drop_caches` and after a cycle-mount —
failed on every file and directory with ESTALE. The handle was minted (`fuse_encode_fh` packs
`nodeid ‖ i_generation`, no daemon involvement) but the daemon never advertised
`FUSE_EXPORT_SUPPORT`, so once the inode left the icache `fuse_get_dentry` (`fs/fuse/inode.c:1054`)
answered ESTALE without asking. Owner ruling 2026-10-23: export handles are in scope — "Build it."

**What the kernel asks, what we answer** (7.3, `fuse_export_operations`):

| the kernel | when | the daemon (`kv_fs.lookup`) |
|---|---|---|
| `fh_to_dentry` → `fuse_get_dentry` → `ilookup5`, else `LOOKUP(nodeid, ".")`; ENOENT ⇒ ESTALE; nodeid mismatch ⇒ EIO; `i_generation ≠ fh` ⇒ ESTALE | every `open_by_handle_at` whose inode is not cached (after a drop, after a remount) | `lookupSelf`: the inode by nodeid alone, ANY kind (file, symlink, directory, a virtual generation ino); a record at `nlink 0` (unlinked, destroy pending) ⇒ ENOENT — a handle dies with its last name, never reopens a file nobody can name; a striped directory answers its fold |
| `get_parent` → `LOOKUP(dir, "..")`, walked by `reconnect_path` until a connected dentry | a DIRECTORY handle (`d_obtain_alias` yields a disconnected dentry; a file's stays disconnected and is accepted), and the parent of a connectable file handle (`fh_to_parent`) | `lookupParent`: the root's `..` is itself; else the directory's DURABLE parent record (below), through the token divert for a foreign one; a non-directory ⇒ ENOTDIR; a corpse or a vanished parent ⇒ ENOENT |
| generic `get_name`: `iterate_dir` on the parent, matching the child's `i_ino`; then `lookup_one` | after each `get_parent` hop | READDIR/READDIRPLUS as before (dirents carry the global ino) |

**The durable parent.** The format stored no parent: `..` was a RAM memo filled at lookup and emptied
by FORGET and by the remount, so every reconnect after a cache drop answered ESTALE. Now every
directory carries ONE record under its own xattr keys — `system.squeezefs.parent` → the LOGICAL
parent's global ino (u64 LE; `economy.PARENT_XATTR`, charter §5.1): written by mkdir in its own
transaction (`stageParentRecord`, slot 0 of a fresh ino's chain, no probe: one more record on the leaf
the inode already takes), moved in the local rename's own entry for a directory whose logical parent
changes (an exchange moves both), by a trailing record (a local commit under the directory's guard, or
ONE `setxattr` record ship to its holder) after a cross-owner rename's intent landed, destroyed with the
directory's other xattrs. The logical parent is the striped directory, never its stripe (`createChildAt`
and `renameKeyed` carry it beside the key parent). Reserved by the allowlist: never listed, read, set or
removed through FUSE. The memo stays (the handover's subtree credit and the ancestry walk read it) as a
cache of the same fact: `memoDurableParent` fills it on a miss, and readdir's `..` entry uses it too (it
showed the directory's own ino after a remount). A directory created before this record exists answers
`..` from the memo alone — unknown after a remount until its next rename.

Why not the reference's way (decision 0002 row 552): it answered `..` by a reverse scan of the WHOLE
dentries tree per hop and refused on token readers; under the symmetric default the scan is not even
expressible across holders. Why not a field in the 68 B inode record: a format bump during the release
window, for a record only directories need; the xattr kind is the format's established way to hang an
optional per-inode datum (`system.symlink` is one). Owed: the cross-owner rename's parent record rides
AFTER the intent, not in it — a crash between leaves the record at the old parent, and the kernel's
`reconnect_path` then finds no name for the directory there and treats the handle as detached (a path
of "/", never a wrong one); a `SetParent` step kind in the intent plan would close the window.

**The generation law** (`KvFs.entry_generation`). The kernel compares the generation a LOOKUP answers
with the handle's, and `fuse_iget` marks a cached inode BAD when a later entry reply carries another
generation — so the value must be ONE per ino for the set's life, on EVERY entry reply (lookup,
create, mkdir, link, readdirplus: `entryOf` is the one constructor). Inos are never reused (charter §5.3;
§5.5's cursors are floors), so one value per SET suffices: the set uuid hashed under the format key and
folded to the kernel's u32 `i_generation` (`entryGenerationOf`; `fuse_entry_out.generation` is u64 but
`i_generation` truncates it — a value above 2³² would mark every re-looked-up inode bad), never 0; the
root's 0 (`fuse_lookup_name` forces it and warns once); a store without an identity (the tests' MemStore)
1. A handle minted on a PREVIOUS format of the same device therefore answers ESTALE, never another
file at the same ino.

**The advertisement is the handler's, not the session's.** `KvFs` declares `pub const export_support =
true`; `filesystem.Filesystem.of` reads the declaration into the bound interface, the reader and move
gates forward their inner's, `ring.Binding.filesystem()` carries it to the transport's `Filesystem`, and
`Session.mount` sets `InitWants.export_support` from it. A filesystem that never said so (the session
tests' EchoFs, the lifecycle's doubles) is never advertised.

**Red first.**
- `smoke: FILE HANDLES SURVIVE A CACHE DROP AND A REMOUNT (FUSE_EXPORT_SUPPORT)` (`src/cli/smoke.zig`):
  handles for a file, its hard link, a symlink, a directory and a nested directory; `echo 2 >
  drop_caches`; each opens (fstat ino equal, the bytes read back, the directory fd's `/proc/self/fd`
  path its CONNECTED path, `faccessat` through it); a renamed file and a renamed directory open (the
  directory at its new path); a file with every name gone and a removed directory ESTALE; after
  `umount`/`mount` every survivor opens, the nested directory resolves through its new parent, the
  removed stay stale. On `origin/dev` + the test: `open_by_handle_at(f after drop): STALE`.
- `kv_fs` unit: `LOOKUP(nodeid, ".")` on a file, a symlink, a directory; a corpse ENOENT; one
  generation, the root's 0; `LOOKUP(dir, "..")` on a FRESH `KvFs` over the same store (no memo) after
  mkdir, a rename across parents, an exchange with a file and with a directory; a rmdir'd directory
  ENOENT; a file's `..` ENOTDIR. On `origin/dev`: `lookup(f, ".")` answered ENOTDIR.

**The numbers** (this host, `7.3.0-1.7-mainline-sqz`, through `scripts/venue`):
- The row: ~1 s a run. 20× beside the full smoke (16 shards): 19 pass, 1 iteration that compiled a
  half-edited tree (a refactor landed mid-loop — a build error, not a row outcome); 20× again beside
  two fstests runs: **20/20**. 39 row runs green in all.
- Unit filters `kv_fs` (102), `lookup`, `dentry`, `mkdir`, `rename`, `economy`, `xv`, `stripe`, `fsck`,
  `symlink`: green. One expectation moved: §AV's mkdir counts 4 records (inode, dentry, the parent's
  Put, the child's parent record), the +1 carried through the test's later sums. The parent record's
  decoder has its fuzz test (the fuzz census rail).
- `zig build rails` 32/32, `zig fmt --check`: green.
- xfstests, `SUITE_RUNS=1 FSTESTS_TESTS="generic/426 generic/467 generic/477 generic/756 generic/777"`:
  **`SUITE_SINGLE_WRITER=0` (the symmetric default): Passed all 5** (426 5 s, 467 3 s, 477 6 s, 756 3 s,
  777 6 s; 30 s wall; 0 daemon alert lines; fsck --offline --scrub 0 findings on both sets).
  **`SUITE_SINGLE_WRITER=1`: Passed all 5** (3, 3, 6, 2, 5 s; 24 s wall; 0 alerts; fsck 0 findings).
  The harness's exit is 1 on a subset (the ledger's "11 stale rows" verdict: rows for tests the subset
  did not run) — the per-test verdict is `check`'s `Passed all 5 tests`.

**Seen, not fixed.**
- Under the first loop's load the full smoke lost two rows outside this change's paths — `A FULL
  DATA STORE ANSWERS ENOSPC` (`sync -f` on the full store answered `synced`, not `refused`) and `THE
  INTERCEPTION HOST` (`ipc_placed_severs` 4 > 3); both pass alone on this tree (load artifacts of 20
  concurrent rows beside 16 shards; neither row makes a directory or a handle).
- A sibling fstests run (the suites' symmetric-posture work, 409/410/411/589/732) left a DEAD
  `fuse.squeezefs` mount of `/dev/loop0` under its results dir ("Transport endpoint is not connected");
  the next `check` on the venue — theirs and mine — aborted with `TEST_DEV=/dev/loop0 is mounted but
  not on TEST_DIR`. Cleared by hand (`umount -l`); the harness's venue-wide wait (F6) sees a running
  `check`, not a dead mount it left.
- The cross-owner rename's parent record rides after the intent (above): a `SetParent` step kind
  would make it one intent.
- A directory of a volume formatted before this commit has no parent record: its handle reconnect
  answers ESTALE after a remount until its next rename (the memo serves it within a mount).
