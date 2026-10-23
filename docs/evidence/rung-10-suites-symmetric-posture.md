# Rung 10 — the suites run the shipped symmetric posture; a bind is the kernel's, never the daemon's to stop

**Ruling (owner, 2026-10-23).** The release suites judge the SHIPPED format: symmetric (charter §5.7 —
the D0 winner is the manager, a later RW mount of the set JOINS). `scripts/suites/common.sh` and
`mkfs.squeezefs` default `SUITE_SINGLE_WRITER=0`; `=1` still formats the `--single-writer` class for a
run that wants it. `suite_mount_pair` (tarx's A-B-B-A row) was symmetric already; LTP, pjdfstest,
mdtest and tarx's parallel phase mount one daemon of a symmetric set — the solo manager (a listener
up, every Symmetric gauge at its solo value) — and nothing else about them changes. Until today every
suite set was formatted `--single-writer` (a 2026-10-10 harness choice), so xfstests' `mount` group
(generic/409, 410, 411, 589, 732) only ever confirmed the guard's BUSY refusal of a second mount and
sat in the ledger as `by-design-single-writer-guard`. Those five rows are deleted: under the default
the second mount JOINS and the tests judge the join.

## What the five found under joins (`rung-10-adjudication-audit.md` §2.4, F8 `4c7cf3e`, and today)

1. **F8** (fixed `4c7cf3e`): a joiner's rename into the manager's directory never resolved its slot move.
2. **F6, the umount persona's bind/move gap** (`scripts/suites/mount.squeezefs`, fixed here): the
   persona ran the orderly `squeezefs umount` for ANY mount whose FSROOT was `/`, so a whole-tree bind
   of a joined mount (409–411: `mount --bind mpA mpB`; 589: `--bind DSTHEAD DSTHEAD`, `--move`)
   signalled the daemon at the FIRST bind's unmount; the daemon unmounted ITS path, then waited out the
   30 s residency backstop (×32 groups) because the other binds kept the superblock alive, and every
   later unmount met a dead connection ("its .stats inode is unreadable … detaching externally",
   "daemon N died without unmounting"); 410's next sub-case mounted into a detached tree. Now the mount
   helper tags its mountpoint with the set AND the kernel's mount id of the mount it made; the umount
   persona runs the orderly stop only when the topmost mount at the target IS that mount, and detaches
   every other mount of our fstype — a bind of a path inside, a whole-tree bind, a bind stacked over
   the daemon's own path, a moved mountpoint, a propagated copy — with the kernel's plain `umount -i`
   (`-l` carried through), the daemon unsignalled. `umount -l` of the daemon's own mount is the daemon's
   lazy stop (`squeezefs umount --lazy`): `fstests_unmount_all`'s fallback after a fail-early kill now
   detaches a TEST_DIR with a killed test's mounts stacked inside it instead of meeting BUSY twice
   (the first five-test run left `/dev/loop0` mounted at a dead TEST_DIR and the next run's `check`
   refused "mounted but not on TEST_DIR").
3. **The verb itself** (`src/cli/umount.zig`): `squeezefs umount <bind>` read `.stats` through the bind,
   found the daemon's pid and SIGTERMed it — the daemon unmounted its own path and the bind was left
   dead. Now `isOwnMount`: the orderly stop only when the `.stats` mount family's `mountpoint` is the
   path AND its new `mount_id` (the id the kernel gave the daemon's `mount(2)`, published by
   `cli/mount.zig` once the session is up; 0 on an older daemon, judged by the path alone) is the
   table's entry at the path; anything else is "a bind or a moved mountpoint of daemon N's mount at
   P" — the kernel's plain detach (`MNT_DETACH` under `--lazy`), nothing signalled, one stderr line,
   exit by the detach's result. The daemon serves the mounts of its superblock that remain and exits
   when the last goes.
4. **The 30 s wait — a product defect, fixed** (`src/fuse/transport/session.zig`
   `endIfHeldElsewhere`). What the daemon waited for: the workers' residency parks until the KERNEL
   ends the connection, which on a type-`fuse` mount is the superblock's destruction inside the
   `umount2` that drops its last reference (`fuse_conn_destroy` → abort; no FUSE_DESTROY is sent for
   non-fuseblk) — so the bound IS the kernel's event, and it comes within the `umount2` call whenever
   the daemon's mount was the superblock's last. When another mount of the superblock stands (a bind the
   operator left; a moved copy; a propagated copy a holder keeps) the event cannot come, the drained
   session cannot serve them, and the daemon answered their requests ENOTCONN for 30 s before exiting
   the same way. Now, BEFORE its own `unmount`, the session reads the mount table once: another mount
   carrying its connection's `major:minor` (`mountinfo.othersOfSuperblock`: every entry but its own
   id at its own path) means the end will not come — it names them (up to four, the rest counted),
   ends the connection itself (fusectl's abort when mounted, else the device fd's close at the end of
   `close`) and raises `Shared.detached`, so the workers' residency exits at once (decision 0003 §S's
   race-case law: a prompt, loud end beats 30 s of ENOTCONN). The backstop lapses only when the end
   cannot be foreseen (a holder inside a lazily detached propagated copy).
   **The first cut read the table AFTER the unmount and aborted another daemon's connection** (gate 94,
   `~/tmp/gate94-symmetric.log`: shard 1/16's mtls daemon logged `1 other mount(s) of this superblock
   stand after the unmount of /build/tmp/mnt-1718474 (/build/tmp/mnt-1718481)` and shard 8/16's
   ORDERED LAW row died at its first mount, `the INIT reply write returned -2` → `InitReplyFailed`).
   The kernel frees a FUSE superblock's anonymous device inside the `umount2` that drops its last
   reference and reissues it to the next `mount(2)` on the host, so after its own unmount the number
   says nothing about the daemon's connection. Decision 0003 §AN.6: the verdict and the abort come
   before the daemon's own `umount2` (its mount holds the superblock, so the number is its own), every
   abort names the minor recorded at `mount(2)`, and `writeAbort` first asks the daemon's own device fd
   whether its connection still stands (`poll` → POLLERR once the kernel ended it). Red first: smoke
   "A DAEMON ENDS ONLY THE CONNECTION IT OWNS" holds A's teardown right after its `umount2`
   (`SQUEEZEFS_TEST_FUSE_UNMOUNT_HOLD`), fills every lower number with a tmpfs so B's fresh mount is
   given A's id (`A's connection id 99 … B's connection id 99 (A's id REISSUED to B)`), and on the
   first cut A aborted B (`the FUSE connection … was aborted under its live mount (fail-stopped) —
   exiting 1`); green after, B serves through A's teardown and both exit 0. The bind row (c) still
   ends the connection in 470 ms.
   The alternative — treating the superblock's other mounts as HOLDERS (BUSY) or auto-taking the lazy
   shape — was rejected here: the kernel's `umount2` of the daemon's mount PROPAGATES to the peers of a
   shared mount (409–411's `mpA/dir` → `mpB/dir`, `mpC/dir`, `mpD/dir` go with it), and a bind is not
   distinguishable from a propagated peer by the table's peer groups, so refusing would break every
   propagation case; the owner may still rule the lazy shape in for a real bind.
5. **A `.stats` render raced another and killed a daemon mid-teardown** (`src/stats/stats.zig`, fixed
   here). generic/410's first run under joins: daemon 9733 (a sub-case's MNTHEAD) panicked
   `reached unreachable code` in `std.HashMap.getOrPut` under `writeAll` of the document, 0.4 s after
   "stop requested … no holder", wait status 0x86; the verb reported "died without unmounting", the
   test's golden output gained the line. The document build MUTATES its own state — the CPU ledger's
   hash map (`cpu_class.sample`), the renderer's size hint — and `VirtualSource.build` ran it on
   whichever handler lane the LOOKUP of `.stats` arrived on, unserialized: `squeezefs umount`'s 100 ms
   `.stats` poll beside the supervisor's probe LOOKUP (and any reader) raced in it. The 10-21 handoff's
   "`cpu_class.zig:101 finishPass` panic, a departed thread's ledger entry with class 170 of 13" is the
   same race. `Sources.render_lock` (the in-tree `sync/mutex.zig`, held with the `Io` bound — fibers
   and the SIGUSR1 dump's plain thread alike) serializes `render`, `renderConfig` and `logFamilies`.
   Red: unit "render: concurrent renders of ONE renderer (eight threads …)" — 1 CRASHED without the
   lock (`39 pass, 1 crash`), 40/40 with it.

## Red → green

- Smoke "A BIND OF THE MOUNT IS THE KERNEL'S, NEVER THE DAEMON'S TO STOP": (a) the kernel's `umount`
  of the original — the daemon unsignalled, the bind keeps serving (`.stats`, the file sha-equal),
  `umount` of the bind drops the last reference and the daemon exits in **131–140 ms** (bound two
  ticks), exit 0, "the FUSE session ended (unmounted)"; (b) `squeezefs umount <bind>` — the plain
  detach naming daemon and mount, nothing signalled, the mount serves on, the orderly stop after it
  exits at once; (c) `squeezefs umount <mount>` with the bind alive — the verb returns 0 after
  **319–328 ms**, the log names the bind, no `residency backstop lapsed`, the bind answers ENOTCONN
  and detaches plainly; fsck 0 findings after each. **Red on dev** (the verb and the session stashed):
  (b) failed at `r.term.success()` after 30 s of `residency backstop lapsed with 0 slot(s) owing` on
  every group.
- Unit `umount.zig` "the daemon's OWN mount is the one at its mountpoint carrying its published mount
  id …"; `stats.zig` render test asserts `"mountpoint":"/mnt/x","mount_id":77,`.

## The five under the default (`SUITE_RUNS=1 FSTESTS_TESTS="generic/409 generic/410 generic/411 generic/589 generic/732"`)

Three runs on this head (the first two stopped early on the defects above, fixed between them):

| test | run 1 (persona fix only) | run 3 (all fixes) | verdict |
|---|---|---|---|
| generic/409 | PASS 304 s | PASS 304 s | the 12 bind matrices, every `find_mnt` block golden, fsstress through the binds |
| generic/410 | FAIL — the `.stats` render race killed one sub-case's MNTHEAD daemon mid-teardown (item 5) | PASS 689 s | 20 make-* matrices |
| generic/411 | (not reached) | PASS 22 s | the shared/slave tree, a 4th daemon under `mpB/mnt1/mnt2` |
| generic/589 | (not reached) | **FAIL — a product wedge under joins (below)** | the 11th of 12 move matrices; 10 passed |
| generic/732 | (not reached; PASS on `f1d0d21`, §2.4) | PASS 18 s (run alone after the fail-early stop) | fsck 0 findings on both sets |

Both sets fsck-clean after every run; no tripwire. The five ledger rows are deleted: four pass on
their merits under the join, and 589's failure is a PRODUCT defect, which is never a by-design row.

**generic/589's wedge (unfixed; the kernel-view coherence owner's — decision 0003 §AB.2/§AG.2).**
Run `suites-fstests-20261023-093140-74130`, sub-case `move private to private`, daemon 174971 (log
`set-scratch/daemon-20261023-095349-174922.log`): the test's 5th member of the scratch set — JOINED as
appender 4 at `mpD/dir` — took fsstress's first `create (ino 1)` into the ROOT (the manager's
directory) and parked it at station `[meta_backend_start]`; the FUSE op watchdog reported it at 10,
15, 20, 25 s; the supervisor's probe (a LOOKUP of `.stats` under that same root) got no answer and
killed the daemon at 30 s (`dumping state, then SIGABRT → SIGKILL`, wait status 0x9). The dump has
the mechanism: the daemon's sideband thread `sqz-fuse-sb` sat in the KERNEL at
`fuse_reverse_inval_entry ← fuse_notify ← fuse_dev_do_write` — a `FUSE_NOTIFY_INVAL_ENTRY` for the
root, which the kernel serializes behind the root's `i_rwsem`, held EXCLUSIVELY by the very `create`
in flight (`lookup_open` holds the parent lock across the request). Meanwhile the manager
(`daemon-20261023-095330-173107.log`) logged `token_grant served in 45000 ms` (= T_owner: a recall
never acked) and `xv_step served in 38892 ms`: the joiner's cross-owner create shipped its step, the
manager recalled the root's token from its holders — the requester itself among them — and the
requester's recall ack waited on its kernel-view invalidation of the root, which its own in-flight
create blocked. A cycle: op → recall → notify → the op's directory lock → op. libfuse documents the
hazard (`fuse_lowlevel_notify_inval_entry` must not be issued for the parent of a request in flight);
here the op's completion DEPENDS on the notify, so running the notify elsewhere does not break it —
the recall's ack must not wait for a kernel-view invalidation of a directory this mount has an op in
flight on (or the manager must not recall the requester's own token for the op's parent: the op's
reply refreshes that view). Intermittent (10 of 11 sub-cases' `mpD/dir`/`mpC/dir` members passed the
same shape): it needs the member to hold a token on the root at its first create. The other members
of the set stayed healthy; fsck clean; 409–411 passed the same shape many times in this run.

## The broader confidence run (head `88945fa`, rebased on `07d0e74`)

`FSTESTS_TESTS` = the `mount` group (050 067 085 491 604 620 632 633 644 645 656 696 697 698 699 766
783 805) + 001 013 034 563 (`suites-fstests-20261023-114143-1198778`, 92 s wall): **8 PASS** — 001
(7 s), 013 (14 s), 034 (6 s), 067 (7 s), 604 (6 s: the `umount &` + `mount` race, now under a
symmetric set), 620 (3 s: the 17 TB hugedisk), 632 (2 s), 696 (5 s); **13 `_notrun`** on the kernel
module's features (shutdown ×2, freeze ×3, idmapped ×5, ACLs ×2, casefold ×1); **1 failed =
generic/563**, adjudicated `by-design-fuse-io-charge` (fails on, as it must). 0 alert lines on every
set (test, scratch, `huge-test.620`, `loop2`); fsck 0 findings on both sets. (A subset run ends
"ledger rc 3" on the rows it did not exercise, by design.)

pjdfstest ×1 (`SUITE_RUNS=1`, the set `SYMMETRIC_PLANE stamped`; `suites-pjdfstest-20261023-114225-1210491`):
**PASSED** — Files=238, Tests=8798, 0 failing subtests, 9 suite-declared TODOs, 205 s wall, 0 alert
lines, fsck 0 findings, 0 ledger rows exercised.

mdtest ×1 (`SUITE_RUNS=1`, -np 32 and 64, -n 10000 -u; `suites-mdtest-20261023-114600-1296402`):
**PASSED** — -np 32 in 34 s (file creation 79,185/s, file stat 7.2 M/s, directory creation 57,118/s),
-np 64 in 68 s (file creation 70,039/s, directory creation 53,944/s), `df` back to its baseline after
3 s / 7 s, the mount healthy after each leg (tripwires 0, watchdog never overdue), 0 alert lines, fsck
0 findings; 7,051,260 FUSE ops and 1,280,065 destroys at -np 64. The class costs nothing there.

generic/732's own cleanup unmounts the MANAGER before the joiner (`_unmount $testdir1; _unmount
$testdir2`): the joiner, parked without a manager, logs `appender 1 could not leave cleanly (Io) —
its page stays Live for the next open's recovery` and one `kv checkpoint tick failed … Io` — the
defined outcome (the next open recovers the page; fsck 0 findings), counted by the suite's alert
census as the run's evidence (6 lines on the scratch set), not a failure.

## Seen, not fixed

- A raw `kill -TERM` of a daemon whose own mount was `mount --move`d: its census and `umount2` key by
  its original path, so nothing is unmounted and `endIfHeldElsewhere` ends the connection loud (the
  moved mount is ENOTCONN until unmounted). Following the mount by id (`mountinfo.entryOf`) would make
  the orderly stop unmount it; `squeezefs umount <moved path>` detaches it plainly today.
- The persona's lazy stop leaves the set's dm slices standing until the run's end or the set's next
  mkfs (the daemon may still serve over them).
