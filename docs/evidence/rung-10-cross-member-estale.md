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

**The creates' regression** (§DH.14). The orchestrator's A-B-B-A (dev `10f6182` vs `e4c0193`, mdtest -n 10000 -u,
fresh RAM perf mount per run): File creation −2.6 to −4.3 % in all four shapes, plain np64 Directory creation
−5.5 %, removal flat. The perf mount's own stats: `dlm_token_grants_served` 0 in both arms (one member, wire up), so
the registry's lock never ran on a create; `fuse_delete_registry` 1,145,729 at exit (the gather's backlog — dev's
§DC marks 1,148,865). Cause: the new `KvFs` fields re-ordered the auto layout, putting `next_fh` (a RMW per create)
at 848 beside `xv` 840 and `io` 856 (read per op) and the `..` memo's header (written per mkdir) at 872.
**Fix** (`8fde307`): `next_fh`, the memos, the destroy tally and the gather's count in `OwnLine` (own cache lines,
pinned by a unit); the registry a 4,096-cell counting filter over 64 own-line shards — a clear cell answers a
grant or shipped verb in one SeqCst load; the delete raises its cell THEN reads the holder's watch, the grant's
registration raises the watch THEN reads the cell (both SeqCst). **Model** `tokens/delete_watch_model.zig`:
holds over 2,704 states (4,576 with another deleted ino in the cell); the delete's raise un-fenced fails after
17 states, the grant's after 1,773. **Counters** (a joined pair, temporary row): empty registry — B's 10,000
stats of A's files: A's grants +10,016–10,021, `fuse_delete_registry_looks` +0; B's 10,000 creates in A's
directory: grants +12,286–15,092, looks +0. 256 deletes registered: A's own 10,000 creates 0 grants, 0 looks;
B's 10,000 stats of fresh files +10,010–10,012 grants, +512 looks (5.1 %); B's creates in A's directory +14,383
grants, +0 looks. The old check locked once per grant. Seen on dev too, not changed (§DH.13 (h)): after B filled
A's directory, A's unlink there answered EIO (B's `cluster_wire_service_refusals` tripwire), and one dev run's
fsck found 169 live inodes named by no dentry.

**Checks** (`8fde307`): the red ×3 (3/3), the §DH units ×3 (49/49 each), the 27 related rows ×1 (68/68), the full
unit step 3,248 pass / 185 skip / 0 fail (one earlier run lost stats' eight-thread render test to load: 7/7
alone), `zig fmt --check`, rails 32/32.
