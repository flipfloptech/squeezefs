# Rung 10 — the owed follow-ups batch (2026-10-29; decision 0003 §CS)

Branch `fix/followups-batch` off `5b86c9f`, `7.3.0-1.7-mainline-sqz`. Every load ran inside one
`SQZ_VENUE_SLOT=1 scripts/venue` command.

**1. An error is reported as itself** (§CL.6 (a); `22ba6bf` red, `42e5788` fix). *What:* the SMO seam
answered `.failed = .corrupt` for every class but the device's, the space's and the allocator's;
`publishGuestRoots`, `writeSlotStateRecord` and ~60 other non-decode `else => error.Corrupt` arms did the
same — a device that dropped writes read as corruption. *Fix:* the flush step carries its error
(`checkpoint_cycle.FlushError`, `PublishError`), the meta core's sets carry what they meet
(`conveyor.Error ⊇ tree.Error ∪ journal.Error`, `OpenError ⊇ Error ∪ CycleError`, `FormatError`, `ProbeError`,
`journal_lane.SubmitError`); a closed sym/routing vocabulary maps an unnamed class through
`errors.narrow` (itself, a verification failure `Corrupt`, else `Io` with a line naming it). Replies are
unchanged: every newly carried class answers EIO as `Corrupt` did. *Proof:* smo unit `THE SMO SEAM REPORTS
ITS FAILURE AS ITSELF` — red on `5b86c9f`: `expected error.Canceled, found error.Corrupt`; green after.
`errors.narrow`'s unit. Units 3170/3343 (173 skipped) on the head; smoke rows: the drop-writes checkpoint
row, A FAILED VOLUME'S DAEMON, THE REPLAY PROOF, A FAILED REPLAY'S JOURNAL TAIL, A FULL METADATA VOLUME,
a TWO-MEMBER set, A HARD LINK … FRESH GUEST SLOTS — green; `zig build fmt rails` green.

**2. One fstests run per host** (§CL.6 (d); `b1dcb28`). *Fix:* `suite_host_lock fstests` holds a flock
from before the first host write to after the helpers' and env file's restore; the waiter logs
`waiting for suite run by pid N (owner <tree>, results …)`; `check` never inherits the fd; an older
harness's `check` is still waited for. *Proof:* `scripts/suites/test-host-lock.sh` — two harnesses
started together: 3/3 PASS, hold windows disjoint (e.g. `…7281888018` / `…7358920109`), the second named
the first's pid, `/sbin/mount.fuse.squeezefs` back on its prior target.

**3. The perf record's `.stats`** (§CN.6 (d); `f57915e`). `cat`, not `cp` (cp trusted the zero size).
*Proof:* 0 of 769 earlier records under `/var/lib/sqz-perf/runs` had `stats-at-exit.json`; one bounded
`ramnvme.sh up` → `perf_mount.sh --ram --fresh -- true` → `down` wrote 145,663 B that parses, fsck 0.

**4. The supervisor's recovered miss** (§CO.25 (a); `3dd5ae1` red, `5409129` fix). The needle
`after a missed probe` joins `missed a probe`. *Proof:* the unit over the line, red on the old needle
(`expected error.SupervisorFired, found void`). The three rows using it (the `.stats` herd, umount of a BUSY
mount, `umount --lazy`) ×3 at load 66–76, and one full smoke run (168/175, 7 skipped): no miss.

**5. The slow unmount** (§CL.6 (c)) — not reproduced, nothing changed.

| tried | cycles | umount → daemon exit | `residency backstop lapsed` |
|---|---|---|---|
| two-member set, format, mount (`--supervise`), 16 MiB + 200 files, sync, `squeezefs umount` | 20 | 151–173 ms | 0 |
| the same on one set, 20 mounts | 20 | 138–160 ms | 0 |
| two such loops side by side | 2 × 10 | ≤ 143 ms | 0 |
| xfstests generic/311, `SUITE_META_MEMBERS=2`, through the locked harness | 6 passes × 164 daemons | — | 0 (fsck 0, 0 alerts each) |

**Seen, not fixed** (§CS.6). The `set_bind` "two-member set behind the filesystem" unit took 6–130 s alone
(base 36–98 s; cost row 415 s) and once passed the 900 s hang limit in a full run at load ≈ 60 (its
checkpoint thread ~38 % in the debug allocator's stack capture inside the SMO fold). `publishTree0`'s
explicit `Io`/`Corrupt` arms, `node.Refusal.err`, enable-symmetric's encoder `.corrupt` kept; `InvalidKey`
answers EIO; `perf_mount.sh`/`ramnvme.sh` pass `SQZ_BUILD_ROOT` to every verb (a knob warning each).
