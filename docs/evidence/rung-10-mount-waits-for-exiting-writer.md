# rung 10 — a mount that races the previous unmount waits for it (fstests generic/604)

**Finding** (2026-10-22, `-g auto` run 1 on dev `27b8ceb`, kernel `7.3.0-1.7-mainline-sqz`):
generic/604 — 501 files on SCRATCH, `_scratch_unmount &`, `sleep 0.01s`, `_scratch_mount` —
failed at once: `another writer holds the single-writer guard … (BUSY, refusal)`. The scratch
daemon's log: `another squeezefs process, pid N, holds the writer lock (claim: …)` with the
holder's own `stop requested` line 25 ms LATER — the new mount met the flock inside the umount
verb's census. A real filesystem's `mount(2)` serializes behind the in-flight `umount(2)` on
`s_umount`; a FUSE daemon's unmount is a process exit, and the guard had no such law.

## The law (`src/meta/guard/proc.zig`, layer A's wait; the backend's ladder step 1)

A mount whose `flock(LOCK_EX | LOCK_NB)` is refused judges the HOLDER each round, on kernel
facts alone — never a request to it: a read of its `.stats` would be an fd on its mount, a
HOLDER to its stop's census and EBUSY to its own `umount2` (the task's "control inode answers
stopping" is not readable harmlessly; checked in `holders.zig`'s exemption rule).

| holder | fact | verdict |
|---|---|---|
| not this binary (udev) | — | waited out 5 s, then refused (unchanged) |
| creator pid gone or a zombie, lock standing | `/proc/<pid>/stat` | **exiting** (`replay`): the supervisor's hold through the journal tail's replay (charter §5.2) |
| alive, `squeezefs mount` argv, no FUSE entry at its mountpoint | `/proc/<pid>/cmdline` + `/proc/self/mountinfo` | **exiting** (`detached`): its `umount2` landed, the dismount pass runs (or a bring-up, which resolves to the row below within its own bound) |
| alive, attached, a `squeezefs umount <its mountpoint>` in the process table | `/proc/*/cmdline` | **exiting** (`umount_in_flight`): the orderly stop's verb, alive from before its census to the daemon's exit — the `s_umount` analogue |
| alive, attached, no verb in flight (or no mount verb at all) | — | **live**: refused AT ONCE (the single-writer law) |

An exiting holder is waited out up to the orderly-exit ceiling `EXITING_HOLDER_WAIT_NS` =
`cli/umount.DAEMON_EXIT_WAIT_NS` (32 s: the session close's residency backstop + one tick) +
two ticks (the verb's pre-signal census, the supervisor's replay) = 36 s, at the umount verb's
exit-poll quantum (20 ms, re-judged every round: a stop the census refused is seen within
one); past it the refusal stands and names the class (`held the writer lock past the 36 s
orderly-exit ceiling while exiting (…)`). The join door of a symmetric set rides the same
wait: a newcomer never joins an exiting manager — it becomes the manager. Engagement gauge:
`guard_exiting_holder_waits` (the backend's stats). Every judgement is one `/proc` read; the
flock itself is retried each round.

Beside it: the mount verb's mountpoint pre-check is `statx(AT_STATX_DONT_SYNC)` — FUSE
serves the cached root attrs, so no request ever reaches the exiting daemon (its drains would
answer an error and the mount would fail "mountpoint: …" instead of waiting).

What this does not see: a holder stopped by a raw `kill -TERM` with its mount still attached
(its census and drains, sub-second) — a mount racing that window refuses as before; once its
mount leaves the table the wait applies. Closing it needs the daemon's own declaration (an OFD
lock on the guard's inode read off the same `/proc/locks` scan is the candidate); not done.

## The red row and the numbers

`smoke: A MOUNT THAT RACES THE PREVIOUS UNMOUNT WAITS, THEN SERVES` — a `--daemon --supervise`
mount, 501 staged 8 KiB files, `squeezefs umount` in the background and a second mount at
once, the first daemon's dismount pass pinned on `SQUEEZEFS_TEST_PROMOTION_HOLD` (its mount
off the table, its lock held) so the race never passes vacuously; two arms, `--single-writer`
(the suites' posture) and the symmetric default. Red on `27b8ceb`: `exited with code 1 …
(BUSY, refusal)` at once (the symmetric arm red differently: the newcomer JOINED the exiting
manager and failed `device I/O failed`). Green: 20/20 beside the full smoke (115/119, 4
skipped), waits 650–1,840 ms, 11–40 rounds, first judgement `umount_in_flight` every run;
files whole, one daemon alive, the newcomer the manager, supervisors silent, fsck 0 findings.
The existing "second mount beside a LIVE mount refuses BUSY" check now also bounds the refusal
to half the ceiling and asserts the live phrasing. Unit: the ceiling tie, the cmdline grammar
(mount/umount verbs, absolute mountpoints), `umountInFlight` over a `/bin/sh` stand-in under
the verb's argv, the replay hold via `fork` (a lock whose creator exited while its description
lives on: judged `replay`, waited out, released). Unit 508/508, rails green, fmt clean.

## The harness (`scripts/suites/mount.squeezefs`)

With the product waiting, generic/604 failed one step later: the umount helper took the set's
dm slices down (`dmsetup remove --retry`) after the daemon's exit while the successor — up
through the same slices — was between the guard and its data-volume discovery; `d0` read as a
foreign superblock. Now the mount helper holds the set's lock (`/run/squeezefs-suites/
<set>.lock`) SHARED for its whole bring-up (fd 9, closed for the daemon), and the umount helper
takes it exclusively (non-blocking) before taking the slices down — a bring-up in flight or a
successor already serving the mountpoint leaves the set up (`stays up` in `mounts.log`); the
successor's own umount takes it down. The helper's bash wait for an exiting holder (the
60 s poll on the holder's log) is superseded by the product's law and now dormant — a prune
for a later pass.

Suite subset: `FSTESTS_TESTS="generic/604 generic/034 generic/039 generic/040 generic/041"`
→ `check rc=0, ran 5, not run 0, failed 0 (Passed all 5 tests)`, fsck 0 findings on both
sets; 604's cycle: `holds it and is exiting (a `squeezefs umount` of its mountpoint is in
flight) … released it after 175 ms — proceeding`. (The ledger's "10 stale rows" verdict is
the subset's: its by-design rows did not run.)
