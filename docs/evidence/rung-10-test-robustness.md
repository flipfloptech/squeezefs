# Rung 10 — rows that assert their law, not the host's pace (2026-10-29; decision 0003 §CO)

A row may judge only what holds under any host load and may count the kernel's caches only while it holds
what it counts. Branch `test/host-noise-robustness` off `74d419c`, `7.3.0-1.7-mainline-sqz`. Every proof
ran its noise inside one `SQZ_VENUE_SLOT=1 scripts/venue` command (`~/tmp/tn/loop.sh`, trap EXIT).

**1. The `.stats` herd row (§CC.4).** It bounded the slowest read by 2 s — a fresh render (§CC.6) plus a
`cat | grep | cut` pipeline, paced by the host: 3 of 10 full-smoke runs at load ≈ 56 failed. Now: 32 readers
each complete ≥ 8 whole documents beside 8 creators for ≥ 8 s, the probe never misses, no drain worker is
inside one pass for the probe's answer window, renders are shared, the watchdog is silent; reads are bash
builtins; walls and renders per read are printed only (the ratio is load-paced: a descheduled reader
revalidates against every render that ran while it slept — 2.0 idle, 5.0 at load 115). **20/20** beside the
full smoke binary's 16 shards and 16 busy loops (load 88–156): slowest read 3,967 ms (14 runs past the old
bound), lane hold ≤ 24 ms, 0 misses (those 20 also judged renders ≤ shared lookups and < readers / 4 a read; the shipped `shares > 0` ran 5/5 more at load 111–140). **Red on `bc569d4`** (+ the row and the gauge): 171 of 256 reads failed
when the supervisor SIGABRTed the daemon (1 probe miss), walls p50 9.8 s / max 35.7 s; the lane hold read
58 ms — the reads and the probe tell the shapes apart (the failing assertion is the shipped row's).

**2. Counting the kernel's caches (§CE.10's class).** An evicting FORGET empties the daemon's read view
and directory memo; `echo 2 > drop_caches` (the export-handles row) or reclaim evicts every unreferenced
inode. Each red below came from a drop placed in the window; each fix ran 20/20 beside a drop every 2 s.

| row | exposure | fix | red → fixed |
|---|---|---|---|
| THE READ VIEW | passes open and close the file: an eviction between them reads as a miss | `O_PATH` pin for the row | misses 1 → 0 (all 20) |
| a TWO-MEMBER metadata set | `depth_reads == 1`: evicting `r$d` before `mkdir r$d/s` re-reads it; no pin can cover that window | mkdirs a phase between SIGUSR1 dumps; `hits + reads == 16`, `1 ≤ reads ≤ 1 + evictions` | hits 12/15 → 7 reads ≤ 1 + 20; the eviction arm taken in 2 of 20 |
| THE TWO-DAEMON MATRIX | A's second read must take no grant: evicted, it is a daemon read under A's token entry, a grant if the budget shed it | `b1/f7` pinned to the count | a drop alone was absorbed (equal grants) |

Audited, not exposed: INTERCEPTION TTL (§CE.10's pins), FILE HANDLES (the dropper; asserts hold either
way), UNLINK STORM, the EchoFs e2e rows, ATIME (relatime judged on every read that reaches the daemon),
the access-stamp, busy-writer and kvmap-peer rows (O_DIRECT), VIRTUAL INODES WITHOUT AN OPEN.

**3. Wall bounds that were no product law,** each replaced by the mechanism it stood for (walls printed):
the bind row's exits (no `residency backstop lapsed`), busy/lazy `umount` (the host's fd census →
`transport_umount_busy_refusals`; a waiting verb never returns), the rename matrix and sideband create (the
move gate's `T_park_max` EIO), the stalled plane's second writer (no second wall), four notifier waits
(half the 60 s TTL), the atime and interception-death rows; units — `dutyPark` in served time, try-only
probes whose lock this thread holds, the ring park and lease recall by their verdicts, the pool's burst by
rendezvous, the lane's drop-wake by `tick_rescues`, the poison rows by cause or op deadline, the mint's
lower bound only. `direct_lease`'s one test never compiled into the unit binary: referenced, 20/20 beside the busy loops.
Units **20/20** beside 16 busy loops; the ten smoke rows **20/20** beside the full smoke's shards and 16 busy loops (load 55–185). Kept as product laws: tick-, `T_owner`-, quantum-,
recovery-, request-deadline-, `DIAL_TIMEOUT`- and exiting-holder-derived bounds.

**Seen, not fixed.** `expectSupervisorSilent` matches `missed a probe` but the supervisor logs `answers
again after a missed probe` (a recovered miss is invisible to its rows); the retention teardown's
`close < 2 s` and the seam-armed looped stage row's ≤ 1 s op latency; `enospc`'s freer racing the 1 s wall
floor; the nvme-dev close ceilings and two 30 s poll caps (anti-hang).
