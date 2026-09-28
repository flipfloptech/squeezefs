# Staging-root lock handover — the fill → unmount → remount A/B (record §4.4bx)

**Date:** 2026-09-28. **Venue:** the dev laptop (`strixhalo`, kernel
`7.2.5-7-omarchy`) — a MECHANISM A/B in the "it works" class; no number here
is a verdict (the venue law, AGENTS.md § Benchmark VENUE). **Instrument:**
`~/sym-run-state/full-vol-handover.sh` (mirrored from
`/tmp/grok-justin/full-vol-handover.sh`): a 2 GiB metadata + 2 GiB data
volume on `/dev/shm`, formatted by the arm's own binary with a staging root
under `/dev/shm/sqfs-handover/staging`, mounted `--daemon --disk-cache-size
200MB`; `fio --rw=write --bs=4M --direct=0 --nrfiles=2 --filesize=512M
--numjobs=8 --ignore_error=ENOSPC --fallocate=none --runtime=20 --time_based`
(the generic/751 shape at small scale: buffered writes past the volume's
capacity), then `fusermount3 -u`, then — the harness's own next step — a
second `mount --daemon` at the same mount point AT ONCE. The script polls the
predecessor's `.squeezefs_owner.lock` with `flock -xn` and its pid with
`kill -0` every 50 ms and reports both instants relative to the `umount`'s
return, plus the successor's exit status and wall.

The chain's red (`/tmp/release-1.3.0/attempt6-23ee8e03/`) is the field face
of the same race — there the successor met the lock during the predecessor's
13-second process EXIT (see the record); here, at small scale, the lock is
held through the dismount teardown's 10-second writeback-retire wait on
custody the full volume can never land. Both faces hold the lock past the
successor's 2-second exit-grade wait, and law 2 covers either.

## Arm A — the tested tree `23ee8e03` (`~/Source/squeezefs-release-1.3.0/target/release/squeezefs`, profile `release`)

```
predecessor: lock freed at +9.91 s, exited at +9.91 s after umount returned
successor mount returned rc=1 at +2.10 s
Error: Invalid operation: cannot own this mount's staging root: Invalid operation: staging root /dev/shm/sqfs-handover/staging/squeezefs/dev_shm_sqfs_handover_mnt is HELD by a live process (its .squeezefs_owner.lock flock is taken) — another mount owns this root. If a previous mount at this mount point is still running, unmount it first
refusals during fill: 8320
```

RED: the lock is released only at the process exit (+9.91 s — the teardown's
10-s retire wait, then the exit), and the successor refuses at +2.10 s with
the chain's exact message. (An earlier run of the same arm read 10.9 GiB RSS
at the unmount and 6,341 refusals; the RSS is the parked acked custody the
full volume cannot land — §7 item 24.)

## Arm B — the fix (`fix/staging-lock-dismount-release`, debug build of the landing tree)

```
predecessor: lock freed at +-1.00 s, exited at +10.45 s after umount returned
successor mount returned rc=0 at +10.45 s
[2026-09-28T10:48:47Z INFO  squeezefs::config_ops] staging root /dev/shm/sqfs-handover/staging/squeezefs/dev_shm_sqfs_handover_mnt is still held by the previous mount's daemon at this mount point (its mount is gone; its dismount teardown is running) — waiting up to 80s for the handover
fill.0.0 fill.0.1 fill.1.0
refusals during fill: 6336
```

GREEN: the successor classifies the holder as dismounting (no FUSE mount at
the mount point), announces the wait once and lands at +10.45 s — the
instant the predecessor's teardown completed and released the lock (law 1),
which is also when the predecessor exits here (no exit hold at small
scale, so the script's `flock -xn` probe never sees the lock free: the
successor takes it in the same 50-ms poll grain — `+-1.00` is the probe's
"never observed" sentinel). The predecessor's files (`fill.0.0 fill.0.1
fill.1.0 …`) read back through the successor.

## What the A/B does and does not show

- It shows the race and its closure in the fstests shape on a real kernel
  unmount and a real second daemon — the mechanism.
- It does not price anything: the laptop's numbers are the retire wait's
  10 s (`dismount_wait`), a constant, and the 2-s exit-grade bound.
- The two law-1 observables that need the predecessor's EXIT to outlive its
  teardown (the field's 13 s) are pinned with the `SQUEEZEFS_TEST_EXIT_HOLD_MS`
  seam in `tests/staging_lock_handover_tests.rs`; law 2 in the fstests
  shape, composed with the `--daemon` parent's readiness deadline, with the
  `SQUEEZEFS_TEST_DISMOUNT_HOLD_MS` seam.
