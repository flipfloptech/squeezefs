# rung 10 — udevreader: the readers took udev's re-read for a live writer

**Left open by nvmetleg.** `rung-10-nvmetleg.md` taught the writer's own door
(`guard.proc.flockExclusive`) that systemd-udevd takes `flock(LOCK_EX|LOCK_NB)` + `BLKRRPART`
for ~10 ms after every write-close of a whole disk. The readers' probe
(`guard.proc.writerFlockHeld`, which takes no lock) still counted ANY exclusive line in
`/proc/locks` as a live writer. Its callers are `status`'s "MOUNTED by a live daemon",
`format`'s preflight ("a live daemon holds its single-writer guard"), `appender clear`
("live-mounted on this host"), and the reader plane's writer faces. So any of these right
after a clean umount, or after an offline verb's close, could read live for ~10 ms. Offline
`fsck`/`scrub`/`config` open through `MetaSet.open`, and so through `flockExclusive`; nvmetleg
had already fixed them. A foreign locker that outlasted the 5 s bound was still refused as
"another squeezefs process holds the writer lock".

**Red (`2e43ff5`, deterministic, kamrui).** A `flock(1)` holder is released once the reader
has retried (the `waits` counter moves, no sleep), and the reader must then read "no writer".
A second `flock(1)` holder never lets go: `flockExclusive` must refuse it and the refusal must
name `pid N (flock)`. This process's own holder must stay a writer with zero waits. Pre-fix:
`meta.guard.proc` 44/47, with all 3 new assertions failing (`!held`, `locker != null` ×2).

**Fix (`a676581`).** All writer-lock judgements now go through one `/proc/locks` read,
`lockerOf`. It returns the exclusive locker's pid and whether that locker is squeezefs (this
process, or `/proc/<pid>/exe` is our executable). `writerFlockHeld(io, path, waits)` uses
`flockExclusive`'s rule: a squeezefs locker is a writer at once, and any other locker is polled
every 5 ms for ≤ 5 s and then counts as held. `flockExclusive` hands back the locker it last
saw. `writeLocker` names it in the mount refusal (`backend`) and in enable-symmetric's
`mounted_here`. A foreign locker now reads `pid 1234 (systemd-udevd), not a squeezefs process,
held the writer lock past the 5 s wait`. A squeezefs locker reads `another squeezefs process,
pid 1234 (squeezefs), holds the writer lock (claim: …)`, so the suite helper's regex still
matches. The staging roots' owner probe stays a single scan (`exclusiveLocker`): it checks a
directory, and udev never locks one. Decision 0002 row 542 and `guard_claims.md` §2.6 now
cover the readers.

**Numbers (kamrui, x86_64, systemd 259).**
- `meta.guard.proc`: 47/47. Filters guard, fsck, claim, enable_symmetric, appender, cli.status
  and reader_plane: 446/446 (36 skipped).
- `umount` → `fsck --offline` → `status` on a `losetup -P` whole-disk meta volume, with a
  mount + one file write per iteration, under 12 × `yes`: 120/120. There were 0 fsck refusals,
  0 spurious "MOUNTED" and 0 other failures.
- Full fresh gate (`-Dfresh=true`, on `e36aa06` + this branch): 55/55 steps, 2997/3080 tests (83 skipped), green.
