# Rung 10 — generic/589's wedge under joins: the sideband law, and what the wedge really was

`rung-10-suites-symmetric-posture.md` "generic/589's wedge" read the joiner's dump — its `sqz-fuse-sb`
thread in the kernel at `fuse_reverse_inval_entry` under a create holding the root's `i_rwsem` — as a
cycle op → recall → notify → the op's directory lock → op, the recall ack waiting on the kernel-view
invalidation. Run `suites-fstests-20261023-093140-74130` (`run1/set-scratch/`) says otherwise.

## What the run shows

1. **The ack never waited.** `token_cache.handleRecall` acks at the push (no serve stamp is ever in flight
   in the product) and the names ride the sideband fire-and-forget (`session.notifyEntryOrEpoch`, whose
   comment already names the hazard). The joiner's recall was acked at once; its `INVAL_ENTRY(root, name)`
   then parked in the kernel behind its own create — harmlessly for the ack.
2. **The 39 s / 45 s were a dead reader's lease.** `daemon-20261023-095343-174575.log`: the sub-case's
   previous `mpA/dir` member PANICKED at its orderly unmount — `free_ship.zig:436 for loop over objects
   with non-equal lengths` in `Shipper.flush` (wait status 0x86), BEFORE its membership leave and its token
   releases. It kept its root token in the manager's grant table. The next change to the root — the 5th
   member's first create, 630.34 — recalled it: the push found no listener (`unreachable_`), and by the
   token law (spec §1.12, charter §5.5/§5.9) the frame retires only at the reader's lease expiry: the step
   was served at 669.232 = the dead member's last contact (624.2) + `T_owner` 45 s; the manager's tick
   evicted it at 670.741 ("1 evicted at T_owner, 1 death record"). Meanwhile every `getattr(1)` on every
   joiner parked on a root grant behind the in-flight recall (`token_grant served in 45000 ms` ×2), the
   supervisors' probes (a LOOKUP under that root) got no answer, and two healthy daemons were killed at
   30 s (173639 at 657, 174971 at 660). The same panic hit a second member at 887 and killed two more.
3. **The panic's cause** (`src/data/sym_planes/free_ship.zig`, `wire_bind.serveFrees`): the holder's free
   dedup window was keyed `(lease_epoch, correlation)` with the frame's correlation — a per-SESSION counter,
   so two joiners' first batches collided and one read the other's verdicts (three for its one block).
4. **What the parked sideband DID cost** (the real defect in the recall/sideband area): the killed daemon
   could not exit for 108 s (its sideband thread in an uninterruptible `down_write`; `is dead` only at
   768.6 when the harness killed fsstress and the create aborted), and for the op's whole life every FORGET,
   INTERRUPT, classical reply and awaited attrs notify sat behind it — a classical straggler reply of the very
   op would deadlock.

## The kernel's lock facts (7.3, `fs/fuse/dir.c` ~1600–1660, `inode.c` ~545, `notify.c` 367)

`fuse_reverse_inval_entry`: `inode_lock_nested(parent, I_MUTEX_PARENT)` UNCONDITIONALLY before `d_lookup`;
`FUSE_EXPIRE_ONLY` only skips `d_invalidate`. `lookup_open` (O_CREAT), mkdir, unlink, rename hold the
parent's `i_rwsem` exclusively across the request; a lookup or readdir shared — `down_write` parks behind
either. `fuse_reverse_inval_inode`: `fi->lock` + `fuse_invalidate_attr` + `invalidate_inode_pages2_range`,
no `i_rwsem`. `fuse_notify_inc_epoch`: `atomic_inc(&fc->epoch)`, no lock; `fuse_dentry_revalidate` (dir.c:402)
`fd->epoch < fc->epoch ⇒ invalid` — every dentry and readdir cache revalidates at its next walk, and a lookup
in flight stores the epoch it read BEFORE its request (dir.c:630/653), so a bump during it lands too.

## The laws

- **(a) the ack law, stated and modelled:** a recall ack never waits on a kernel-view invalidation.
  `src/meta/sym/tokens/recall_notify_model.zig` — manager, the requester's op, the requester's sideband;
  `expectHolds` with the ack at the push; the WEAKENING (the ack after the notify's write, libfuse's
  synchronous posture) `expectFails` with `error.InvariantViolated` (the three actors deadlocked behind the
  op's directory lock).
- **(c') the sideband law, new:** the sideband never `inode_lock`s a directory this daemon owes a reply on.
  Right before the device write it judges an `INVAL_ENTRY`'s parent against the handler's in-flight
  registry (`watchdog.Registry.hasOpOn`: the request's nodeid, or a rename's new parent — `OpSlot.ino2`) and
  rewrites the frame in place as an `INC_EPOCH` (`Sideband.writeEntryOrEpoch`; counted
  `transport_notify_entry_epochs_substituted`). Coverage ⊇ the name's; cost one LOOKUP per cached dentry
  at its next walk, paid only when a recall names a directory with an op in flight. An op arriving between
  the check and the kernel's lock parks the write for that op's duration alone — nothing of the op's depends
  on this thread.
- **The trigger (out of area, isolated commit `3f77cfb`):** the dedup keyed `(lease_epoch, sender,
  request_id)` with the wire peer's identity hashed as `sender`; the shipper treats a reply of another length
  as a failed attempt (counted, the batch stays tracked) — never a panic.

## Red → green

- Smoke "THE SIDEBAND NEVER inode_locks A DIRECTORY ITS DAEMON OWES A REPLY ON": A + B JOINED; B lists the
  root and `touch`es into it; A's pass holds after its recall (`SQUEEZEFS_TEST_TOKEN_RECALL_HOLD`, a FIFO seam
  the daemon's holder now reads); B's `sqz-fuse-sb` wchan sampled every 25 ms across the hold. **Red with the
  law off: 58 of 58 samples in `fuse_reverse_inval_entry`.** Green: 0 of 59, substituted 1, the create
  completes 2 ms after the release (1.5 s wall = the hold), no slow grant, fsck clean; **20/20** under the
  suite's load.
- Unit: session e2e (a parent the probe names → substituted +1, epochs_sent +1, the next stat drops and
  re-looks-up the dentry; a free parent or no registry → the name itself); `watchdog` `hasOpOn`;
  `free_ship` (two senders, one correlation; the foreign-length reply fails the flush, nothing moved).
- Unit filters `sym` 433/433, `tokens` 98/98, `recall` 75/75, `sideband` 47/47, `kv_fs` 109/109; `zig build
  rails` green; `zig fmt --check` clean. Rows INVALIDATION COHERENCE, F8 rename matrix (slowest 5 ms), THE
  TWO-DAEMON MATRIX, THE WRITE CACHE'S RECALL, JOINER'S CLEAN UNMOUNT AND REJOIN, SHARED HOT DIRECTORY: green.
- `SUITE_RUNS=1 FSTESTS_TESTS="generic/589 generic/409 generic/411 generic/732"` on `3f77cfb`: three runs,
  `Passed all 4 tests` each (589 in 370 s; 715/710/… s wall); 0 panics, 0 kills in the daemon logs.

## Seen, not fixed

- A member that dies without its leave stalls every recall of what it held for `T_owner` (45 s) — the
  spec's law (§1.12) — while the supervisors kill healthy members at 30 s: a crash anywhere in a set turns
  into a kill cascade on the root. A death notice from the dying member's supervisor (it already replays
  the tail and releases the guard) to the manager (`noteMemberDeparted`) would retire the grants at once;
  the membership plane's owner's call.
- The holder's other `Seams` (`SQUEEZEFS_TEST_TOKEN_RECALL_DEADLINE_MS`, `_CONVEYOR_HOLD_POST_RECALL`,
  `_GRANT_HOLD_AFTER_CHECK`) are read by `Seams.fromEnv` in the in-process contracts only; the daemon's holder
  never calls it (the rails see them read). Only the new FIFO seam is wired into the daemon.
- The supervisor's probe (`stat` of `.stats` under the root) is blocked by any long op holding the root's
  `i_rwsem`; a probe that needs no parent lock (`statx` of the root itself) would judge the daemon, not its
  callers' directory locks.
- The costs tables carry no rows for the new tests (under the rail's tenth); regenerate at the next
  `zig build costs`.
