# Rung 10 — a file another member deleted answers ESTALE on a member's descriptors (2026-11-03; decision 0003 §DH)

**The ruling** (the owner, 2026-11-03): when member A deletes a file member B still has open, B's later reads and
writes answer ESTALE — never zeros, never stale or garbage bytes; same-member unlinked-open files stay POSIX.
§DC.9 (b) had left it: from the deleting holder's FORGET, B read the discarded write-cache resident as zeros.

**The model.** Every member reaches a file only through its HOLDER (the slot lessee): token grants, shipped record
verbs, publishes. The delete is §DC's — the holder's final FORGET at nlink 0; until it the file lives for every
member. Inos are never reused (one watermark, cursors are floors), so a stale nodeid never names another file.

**Red** (`317fc1e`, run on its own tree, Linux 7.3.1): A and B supervised; A writes `d/s` (a 20,000 B resident)
and `d/g` (4 MiB + 4 KiB, fsync'd); a python descriptor holder on each mount; A `rm`s both (still open on A):
B reads both byte-exact, B's write to `g` is read back through A's descriptor (POSIX across members, green
throughout). A writes through its descriptors and closes them — the delete, the destroy held by the new seam
`SQUEEZEFS_TEST_CORPSE_DESTROY_HOLD`. Then B's 12 ops, the destroy, the 12 ops, 64 fresh files on A, the 12 ops:
- deleted, destroy held: `s` read **20,000 zero bytes** (`staged_payload_lost_reads` 1) and mmap'd the same;
  `g` read its bytes; fstat ok; write, fsync, ftruncate **succeeded** into the corpse the destroy then dropped;
- destroyed: fstat/write ENOENT, fsync ok, ftruncate EIO with **8 tripwires** on A (`record_refusals`,
  `cluster_wire_service_refusals`), a read past the size ENOENT, inside it EIO, mmap SIGBUS;
- after 64 fresh files (none took a deleted ino): the same, and an in-size read returned **B's own refused
  write's 4 KiB** (7.3 keeps a fully copied folio uptodate when the WRITE fails).

**Fix** (`0db35d7`; §DH.3–DH.7). The holder registers the delete (`KvFs.deleted`) before §DC's discard; a grant of
a registered object answers `gone` (`PrepareSeam.deleted`, after the registration — register-then-check, a
Dekker pair since `8fde307`); a shipped verb on a registered or absent object answers the new reply kind `gone`
(never a tripwire); a delete a member may hold a grant on kicks its destroy, whose pass recalls that grant. On B
every nodeid op on a gone record answers ESTALE (`readHeld`), counted by cause, and drops B's kernel view of the
inode; a publish answered `gone` is the merge's `NotFound`.

**Kernel limits** (stock 7.3, no patch; §DH.8): a buffered read inside the cached size answers EIO (iomap's read
path reports any failed folio as EIO; a read past the size asks GETATTR first and carries ESTALE); mmap SIGBUS;
a refused buffered write's copied pages stay uptodate until the ESTALE's invalidation lands (notify speed).

**Checks** (`0db35d7`, Linux 7.3.1): the red ×10 (10/10; `staged_payload_lost_reads` 0, B's ESTALE answers
`record_gone` 61–78 / `ship_gone` 6, A's `dlm_token_grants_deleted` 52–76, `record_ships_gone` 6, kicks 2, fsck
clean); 27 related rows ×2 (68/68 both: same-member unlinked-open, §DC/§CW/§CR, unlink/rename/hard-link, the
cross-daemon coherence rows); units `fuse.handler.kv_fs` + `data.` + `meta.` + `wire.` 1953 pass / 3 skip / 0
fail; xfstests generic/001 013 074 3/3, fsck 0 (alerts: §DB.3's syncfs line only); `zig fmt --check`; rails 32/32.
Not changed (§DH.13): a full heap delays the kicked destroy; a non-holder's own unlink of an open file answers
ESTALE once the holder forgets it; a served unlink of a file the holder's kernel never held waits for the next
mount's sweep; a wired mount pays the registry at every delete (removal rows flat in the A-B-B-A below).

**The creates' regression** (§DH.14). A-B-B-A (dev `10f6182` vs `e4c0193`, mdtest -n 10000 -u): File creation
−2.6 to −4.3 %, plain np64 Directory creation −5.5 %. The perf mount serves no grant (`dlm_token_grants_served` 0
in both arms), so the registry's lock never ran on a create; the cause was the layout — `next_fh` (a RMW per
create) moved beside `xv` and `io` (read per op), the `..` memo's header beside them. **Fix** `8fde307`: those
fields in `OwnLine`; the registry a 4,096-cell counting filter over 64 own-line shards, a Dekker pair with the
grant's registration (both raises SeqCst). **Model** `tokens/delete_watch_model.zig`: holds over 2,704 / 4,576
states; the delete's raise not SeqCst fails after 17 states, the grant's after 1,773. **Counters** (a joined
pair): 10,000 grants of live files take 0 registry locks with an empty registry, 512 (5.1 %) with 256 deletes
registered. Checks on `8fde307`: red ×3, §DH units ×3 (49/49), 27 rows ×1 (68/68), full units, fmt, rails 32/32.
Seen on dev too (§DH.13 (h)): after B filled A's directory, A's unlink there answered EIO; one dev run's fsck
found 169 live inodes named by no dentry.

**The mkdirs' regression** (§DH.16). Four order-balanced rounds: File creation fixed, Directory creation −7 % at
plain np32 and real at rw4k np64. A mkdir takes the memo's spin lock four times; `8fde307`'s `Memo` put the
`parents` header at +8, on the lock word's line, where every waiter spins. perf c2c (IBS, `mdtest -D -C` np32,
RAM perf mount; cache-line counts only): memo HITMs per run old head 28/20/35, dev 14/11/18/14, the old head's
header HITMs on the lock's line (7/6/15). Fix `38ecaac`: the word and both headers each in a block of their own —
19/12/24, the lock's line lock-only, the headers' HITMs on their own lines; the unit `THE MEMO'S LAYOUT` (red on
`054d5a9`). **The row's flake** (§DH.17, 1 run in ~170): B's relatime drain serves a token after shipping an
access stamp; that background grant, landing between A's last recall and A's delete, is never recalled while the
seam holds the destroy, and B answered from its cache. The row mounts B noatime and asserts no stamp shipped.

**Checks** (`6b39633`): the red ×13 (13/13), the §DH units ×3 (50/50 each), the full unit step 3,249 pass /
185 skip / 0 fail, `zig fmt --check`, rails 32/32.
