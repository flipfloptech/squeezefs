# A member joins a set whose manager has remounted (decision 0003 §BM; BL.6)

## What failed

Format a symmetric set; mount A (the manager), write, `umount` A, mount A again; mount B (a second
mount of the set, which must JOIN): `cannot open metadata set … device I/O failed (IO, terminal)`.
Every remount of the manager (an upgrade, a reboot, a supervisor's successor after `kill -9`) broke
every later join; a join into a fresh set worked. Reproduced by hand on file-backed volumes on
`c79ecb1`, with both daemons' logs kept (`squeezefs format`, two foreground mounts, `SQUEEZEFS_MW_BIND=127.0.0.1:0`).

## Mechanism

Not a device read. Instrumented (probes since removed), B's open:

    JR joiner dials 127.0.0.1:44759 off the projection at checkpoint seq 7; page0 term 2 writer 0x199cfc79bd4c8c68 state live
    JR dial 127.0.0.1:44759: Unreachable          (connect: SO_ERROR = ECONNREFUSED)
    JR joinAppender wire error HolderUnreachable

A's first incarnation listened at `:44759`, its second at `:43785` (`SQUEEZEFS_MW_BIND` `auto` and
`127.0.0.1:0` bind an ephemeral port per incarnation). The joiner reads the manager's listener as
`endpoint:{0}` off tree 0's PROJECTION — the newest checkpoint; it never replays ring 0. The
manager publishes a moved listener as one ring-0 control entry (`publishEndpoints`), which only
its next cycle (≤ 1.1 s) carries into a checkpoint. Appender 0's page was already rewritten at
the bind (term 2, the new writer id) and said "live", so B dialled the dead port. The error chain
mapped the refused TCP connect to a device error: `dialer` `Unreachable` → `HolderUnreachable` →
`ManagerWire.verbError` → `Io` → `join.join` → `JoinedSet.open` → `openJoined` → the mount's
"device I/O failed". With a 4 s pause before B's mount the join succeeded. A fresh set has no
record yet, so its joiner polls (`awaitManagerEndpoint`) and finds the live one.

## The law and the fix (`4a356ff`)

Appender 0's page says WHO manages, never WHERE it listens (§BM.1). Two fixes:

- **Manager** (`wire_plane.publishEndpoints`): after putting a MOVED listener, one checkpoint cycle,
  so the newest record names the live listener from the manager's `ready` on (a remount at the
  same endpoint still writes nothing). The `-o ro` reader (`reader_plane`) and `squeezefs job`,
  which read the same record, get the same guarantee.
- **Joiner** (`JoinedSet.awaitManagerListener`): the projected endpoint is a hint until a dial
  proves it. A dial nobody answers (`HolderUnreachable`, `ListenerRefused`) re-resolves (the
  `ManagerResolver` re-reads `endpoint:{0}` at every newer checkpoint) and redials every 100 ms,
  bounded by the endpoint poll (three cadences, counted `joined_endpoint_polls`). Past the bound
  the joiner refuses `ManagerUnreachable` and names the endpoint and the page's term. It is no
  longer reported as a device error.

## Red, then green

| Test | dev `c79ecb1` | branch |
|---|---|---|
| smoke "A MEMBER JOINS A SET WHOSE MANAGER HAS REMOUNTED" / "… TWICE" / "… WAS KILLED" (kill -9 of a `--daemon --supervise` A, the supervisor's replay, a remount) | 3/3 `device I/O failed (IO, terminal)` | green: 40/40 each before the rebase (20 rounds × 2 parallel copies, run beside another tree's full fresh smoke and the 29-filter joiner set), then 20/20 each on the rebased head (load average ≈ 20) |
| unit wire_plane "THE JOIN AFTER A MANAGER REMOUNT" (the cadence held at 600 s) | `Io` at `verbError` ← `HolderUnreachable` ← SO_ERROR | green |
| unit wire_plane "A JOIN RACING THE MANAGER'S BRING-UP" (incarnation 2 open, its plane armed only after the joiner's first refused dial) | without the joiner half: `try race.result` fails | green |
| smoke "A BIND OF THE MOUNT…" (d): the reformat that dodged this defect is removed; B joins A's fourth incarnation | — | green |

On the head rebased onto `ac7986e`, which landed "a joined member never writes the manager's
tree 0": the three rows are still 3/3 red with this commit's rows alone (`c49ce65`), so this is
not that defect. Here no joiner had mounted before the failing join and the failure was the
refused connect, not a tree-0 read. Every two-mount/joiner smoke row (29 filters: the matrices,
the kill rows, the rejoin, the reader, the fleet, generic/604's race, …) passed 72/72. Unit
filters `sym`, `membership`, `joined`, `page`, `JOIN`, `wire_plane`: 558/606 passed, 48 skipped
(smoke-tier). `zig build rails` 32/32; `zig fmt --check .` clean.

xfstests subset `generic/409 589 732 741` on the final head (`57ccadc`): 409, 589 and 732 pass;
fsck after the run reports 0 findings on both sets. 741 fails with "mount of busy device
succeeded"; the same run on `01bbf38`, before the rebase, gave the same result. That failure is
not from this change, see below.

## Seen, not fixed

- During the manager's bring-up (after its bind, before its publish lands) the `-o ro` reader
  (`reader_plane.zig` reads `endpoint:{0}` at its open) and `squeezefs job` (`cli/job.zig`) still
  read the previous incarnation's listener. The reader's link re-resolves at the next record. The
  job CLI was not traced.
- generic/741 ("mount of busy device succeeded") fails the same way on `9d9e3ce`: the full run of
  2026-10-23 21:23 stopped at it. Its second mount, of the device under the dm-flakey stack, is a
  normal JOIN of the live manager on a fresh set: term 1, `mounted JOINED as appender 1`. That join
  path worked before this change. With flakey unmounted, the same mount would become the manager.
  The device's dm holder is never consulted (no exclusive bdev claim). Owed to its owner.
