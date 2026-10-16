# rung 10 — nvmetleg: udevd's partition re-read was taken for a squeezefs writer

**Symptom.** The kamrui train run on `c68c18e` failed the nvmet smoke row `daemon-connect-loop`
(`.gate-train/kamrui-c68c18ebd339-20261016T052417Z.log`). The mount `dc-a` died before
`ready:` with `meta volume /dev/nvme1n1: another squeezefs process holds the writer lock`. The
refusal named no writer claim. This was the first sighting in the train's history.

**Timeline (kamrui `dmesg -T`, the test's own subsystems, pid 2572771).** At 05:25:18 the
loop leg connected `dcmeta` as nvme1 and `dcdata` as nvme2. It then ran `format`, the refused
`dc-norec` mount, `config show` and `config set-fabric-endpoints` on `/dev/nvme1n1`, reaping
each one (`waitpid`). It disconnected nvme2 and spawned `dc-a`, which was refused. The brief's
05:26:17 `smeta` lines are the later `sigstop-preempt` row, whose second identity reconnects
its own subsystem. No umount preceded the mount, and every squeezefs process that had opened
the inode was reaped. So the exclusive holder was not squeezefs.

**Verdict: PRODUCT (a), but not the umount.** systemd-udevd watches every whole disk
(`60-block.rules` `OPTIONS+="watch"`, `nvme*` included). After each write-close of the node its
worker takes `flock(LOCK_EX|LOCK_NB)` and issues `BLKRRPART` to re-read the partition table.
strace of udevd on kamrui (systemd 259, a `losetup -P` disk) shows this once per `O_RDWR`
close: `flock(5</dev/loop5>, LOCK_EX|LOCK_NB) = 0` at 38.337362, then
`ioctl(6</dev/loop5>, BLKRRPART) = 0` at 38.346222. That is ~10 ms idle, longer through nvmet
under load. `guard.proc.flockExclusive` (from `29029fa`) already waited
out udev's SHARED probe. It refused at once on ANY exclusive line in `/proc/locks`. So any
writer verb or umount followed at once by a mount could meet EBUSY. The umount contract is
unchanged and right: the verb returns 0 only after the daemon exits (0003 §S), so the daemon
releases its flock first. udevd's lock comes AFTER that release. The endpoint question does not
apply here: the meta URI is the rig's own connect.

**Red (`2c397ff`, deterministic).** A `flock(1)` process (not this binary) holds `LOCK_EX`
until the waiting opener has retried once, and only then lets go. It is released from the
test's thread when the waits counter moves, so there is no sleep. Pre-fix, on kamrui:
`expected .SUCCESS, found .AGAIN`. The same test keeps this process's own exclusive holder
refused with zero waits.

**Fix (`5669563`).** The `/proc/locks` scan returns the locker's pid (`flockHolderIn`). The
refusal stands at once only when the locker is this process, or a process whose
`/proc/<pid>/exe` is our own executable. Any other locker is polled every 5 ms for at most
5 s, the same bound as the shared probe: udev's `LOCK_SH` rules, its `LOCK_EX` re-read, or a
locker whose exe cannot be read. A squeezefs holder of a different executable (an upgraded
binary, a test binary against a `./squeezefs` daemon) is still refused, but only after the
bound. A join-door mount behind such a manager likewise joins 5 s late. This
diverges from the reference's claim-based absorption (decision 0002 row 542): a claim names
the last writer, not the locker. The symmetric door (`enable-symmetric`) shares the helper.

**Numbers (kamrui).**
- Pre-fix, with a debug build that dumps the `/proc/locks` holders on refusal: the nvmet
  `daemon-connect` legs went 47/47 under 12 × `yes`. The window is narrow, and the train met it
  once. Back-to-back `config set-fabric-endpoints` on a partscan loop disk went 200/200 idle,
  but 1 of 100 met EBUSY under 12 × `yes`, and a run cut short by its timeout met 3. Every
  locker was a `WRITE` flock held by a `(udev-synth)` child of systemd-udevd (pid 679), in state
  D inside the re-read. The refusals came 8–16 ms after the attempt.
- Post-fix: the same verb loop 0 EBUSY in 100 under 12 × `yes`; the nvmet `daemon-connect` legs
  51/51 under 12 × `yes`; the meta guard unit rows 168/168; the guard, enable-symmetric and
  claim smoke rows 311/311.
- Full fresh gate (on `d7c4121` + this branch): 52/55 steps, 2991/3075 tests (83 skipped). The
  1 failure was the literal rail on the new pid parse (its radix undocumented). The fix
  (an end-of-line comment) is folded into `5669563`, and the rerun is green: rails 29/29, the
  guard unit rows 43/43, 21/21 steps.

**Not changed.** `writerFlockHeld`, the readers' "a writer is here" probe, still counts any
exclusive line, so udev's re-read reads as a writer for ~10 ms. The refusal text still says
"another squeezefs process" even when a foreign locker outlasts 5 s.
