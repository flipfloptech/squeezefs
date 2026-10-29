# Rung 10 — rows that assert their law, not the host's pace (2026-10-29; decision 0003 §CO)

The gate runs every shard side by side (`AGENTS.md`): a row may assert only what holds under any host
load, and may count the kernel's caches only while it holds what it counts. Branch
`test/host-noise-robustness` off `74d419c`; this workstation on `7.3.0-1.7-mainline-sqz`. Every proof ran
its noise inside one `SQZ_VENUE_SLOT=1 scripts/venue` command (`~/tmp/tn/loop.sh`, trap EXIT).

## 1. The `.stats` herd row (§CC): its slowest read was bounded by 2 s

**Exposure.** A read's wall is a fresh render's (§CC.6) plus a `cat | grep | cut` pipeline and two
`date` forks, all paced by the host: 3 of 10 full-smoke runs at load ≈ 56 read 2,196–4,085 ms.
**Fix.** The row asserts the law's observables: every one of 32 readers completes ≥ 8 reads (and the
herd runs ≥ 8 s beside 8 creators), every read a whole document (`{…}` with the lane-hold gauge; each
reader's last one strictly parsed), the supervisor's probe never missed, no drain worker inside one pass
for the probe's answer window (`transport_pass_stalest_ms`), renders collapse (renders ≤ shared lookups,
renders per read < readers / 4), the op watchdog silent. Reads are bash builtins (`$(< f)`, `[[ =~ ]]`,
`EPOCHREALTIME`): no tool forks, the walls printed, never judged.
**Proof.** RESULTS_HERD. **Red on the pre-§CC shape** (`bc569d4` + the row + the gauge line,
`~/tmp/wt-tn-herd-red`): 256 reads attempted, 171 failed (the supervisor SIGABRTed the daemon: probe
misses 1), walls p50 9.8 s / max 35.7 s, longest worker pass 58 ms — the lane hold does not tell the shapes
apart; the reads, the probe and the collapse do (idle: 1.92 renders a read, 5.7 shared lookups a render).

## 2. Rows that counted the kernel's caches without holding them (§CE.10's class)

The daemon keeps per-inode state only while the kernel holds the inode: an evicting FORGET drops the read
view (`DataPlane.forgetShape`), the directory memo (`forgetParent`), the shape. A host-wide
`echo 2 > drop_caches` (the export-handles row, any shard) or reclaim evicts every unreferenced inode.

| row | exposure | fix | red (a drop placed in the window) / proof |
|---|---|---|---|
| THE READ VIEW | each read pass opens and closes the file: an eviction between passes empties the view — `misses == 0` fails, a stale view after the truncate/overwrite reads as a miss | the file PINNED by an `O_PATH` fd for the row | unpinned: misses 1; pinned: 0. RESULTS_AUDIT |
| a TWO-MEMBER metadata set | `fuse_dir_depth_reads == 1`, `memo_hits == 15`: an eviction of `r$d` between its mkdir and `mkdir r$d/s` re-reads the record; no pin can close that window (the directory is unreferenced from its own mkdir's return) | the mkdirs are their own phase, bracketed by SIGUSR1 dumps (no `.stats` lookup of its own): `hits + reads == 16`, `1 ≤ reads ≤ 1 + evictions` — with none, `/` alone was read | unfixed: memo hits 12 of 15; fixed: reads 7 ≤ 1 + 20 evicted. RESULTS_AUDIT |
| THE TWO-DAEMON MATRIX | A's second `cat b1/f7` must take no grant on B: unpinned, an eviction makes it a daemon read served under A's token entry — a grant whenever the budget shed that entry | `b1/f7` (and so `b1`) pinned from the first read to the count | a drop alone was absorbed by A's token cache (grants equal unpinned); the pin removes the eviction leg. RESULTS_AUDIT |

Audited, not exposed: THE INTERCEPTION TTL (pinned by §CE.10), FILE HANDLES (the dropper; every assert
holds cached or evicted), THE UNLINK STORM (a FORGET comes at the unlink either way), the EchoFs e2e rows
(one stat each; FORGETs ride the sideband, counted apart), ATIME (the daemon judges relatime on every read
that reaches it: the touch count is the same evicted or cached), the access-stamp, busy-writer and kvmap
peer rows (O_DIRECT; counts are recalls and purges), THE VIRTUAL INODES WITHOUT AN OPEN (fd held per read).

## 3. Real-time bounds between two statements that were no product law

Replaced by the mechanism that bound stood for (each red path stays red): the bind row's exits (≤ 2 ticks
→ no `residency backstop lapsed`, the rig's exit ceiling), busy/lazy `umount` (≤ 5 s / 20 s, the host's fd
census → `transport_umount_busy_refusals` reads 1 after the bare SIGTERM; a verb that waited never
returns), the rename matrix and the sideband create (`timeout 2 mv` / ≤ 2 s → the move gate's own
`T_park_max` answers an unresolved op EIO), the stalled plane's second writer (≤ 2 ticks + 2 s → no second
wall, `parked_gate_wall_plane_stalls` unchanged), the notifier waits of four rows (2–4 s → half the 60 s
TTL a lost invalidation would hold), the atime row's log waits (5 s → the rig's ready bound). Units:
`dutyPark` in served time (`served_ns`, each slice credited ≤ its deadline), the warm ladder / write-side
try-only probes (the test thread holds the lock: a park never returns), the dd ring park and the write
lease recall (the verdict `returned` / `drained`, bounds that outlast a thread's start), the pool's burst
(a rendezvous of all four jobs), the idle reap (awaited), the lane's drop-wake (`tick_rescues` stays 1),
the poison rows (the cause, or the session's own op deadline). RESULTS_TIMING

Kept as product laws (each bound is the product's own deadline, the alternative path its length): the
tick-derived waits (`write_admit` ×3, lane stop, rwlock, gate close, backend's rescue), `T_owner`/recall
deadlines in `tokens`, the reclaim quantum, the recovery bound in force, the kernel's request deadline,
`DIAL_TIMEOUT`, the exiting-holder ceiling.

## Seen, not fixed

(a) `expectSupervisorSilent` matches `missed a probe`, but the supervisor logs `answers again after a
missed probe`: a recovered miss is invisible to every row that uses it. (b) The retention teardown's
`close < 2 s` (`fuse/transport/session.zig`) and the looped stage row's op-latency ≤ 1 s (seam-armed, not
in the gate) are wall bounds still. (c) `enospc` "supply that ARRIVES during the park" races its freer's
start against the 1 s wall floor. (d) The nvme-dev close ceilings (10 s, 30 s) and two 30 s poll caps are
anti-hang ceilings, not laws.
