# Handoff — 2026-11-04

`dev` = `origin/dev` = `b43bd14` (updated 2026-11-04 afternoon). Venue: `7.3.1-1-omarchy-mainline` (kernel.org 7.3.1, Omarchy-flavoured, none of our
patches), `nvme_core.multipath=N`. Every landing below had its own green full gate on the local venue; the
safety-critical ones (§DG, §DJ) also passed an independent read-only safety review loop, and the perf-sensitive ones (§DH,
§DJ) a governor-controlled A-B-B-A. Decision ledger 0003 runs through §DN.

## What landed since the 11-03 note (red first; evidence in `docs/evidence/`)

- **§DG the fencing registrant**: a write-free second NVMe host identity of the manager's host, under the HOST's key,
  issues the member-death Preempt — device-grade member fencing on METADATA under nvmet 7.3 (whose holder Preempt
  ignores PRKEY). Refused by name under multipath=Y. Three review rounds closed: a late-registered fencer preempting,
  the unlocked PR char-node ring (pre-existing), the fencer's block node reachable from userspace (udev rule
  `scripts/udev/60-squeezefs-fencer.rules`, installed by provisioning — NOT yet on this venue), the lapse claim window.
- **§DH cross-member ESTALE**: a file another member deleted answers ESTALE on a member's descriptors (a buffered read
  inside the cached size: EIO; mmap: SIGBUS — stock 7.3 limits). A sharded delete registry with a lock-free filter on
  the grant path, one cache line per delete (two A-B-B-A rounds found false sharing and a global gauge; fixed).
- **§DI cross-member directory storms**: a holder's grant page overflowed past ~16k short names (EIO); a slot release
  ran with a moved leaf's parent dirty (a member's unmount lost its newest names → fsck C9 orphans); a crash-only
  dirty-marker loss.
- **§DJ data-namespace PR fencing**: every data namespace WEAR, every member host registered before its first data
  write, member death preempts-and-aborts the dead host on every data namespace and proves it before any extent is
  reallocated; a lapse beat re-registers after a target restart (nvmet has no PTPL) for data AND metadata joiners;
  an operator escape `squeezefs volume waive-data-fence`. A-B-B-A neutral within noise (DJ.20).
- **§DK** a loop share never joins a loop port it did not make (two gates went red on the RAM perf rig's port).

## Landed later on 11-04 (each with a green full gate and an independent safety review)

- **§DL** after a target restart the manager ends its grace as soon as every live member's host is registered again
  (both restart windows ≤ two beats from the target's return with every host back); a slow member re-registers on a
  recent-lapse refusal instead of fail-stopping.
- **§DM** the membership Owner's shared state under one mutex (pre-existing blocker-class race: a lost renewal evicted
  live members; a torn death screen accepted a death record naming a live key). 775 s loaded churn, 0 wrongful evictions.
- **§DN** a joiner's lapse beat re-registers on EVERY joined metadata volume (multi-volume sets no longer fail-stop their
  joiners after another volume's target restart); a joiner is current only with a Live page on every volume; the
  pre-existing re-join use-after-free (a refused join closed devices the data fence still used) fixed by a two-phase
  re-admission.

## In flight

Nothing.

## Follow-ups (no owner yet)

- No real-device row covers a SUCCESSFUL re-join with a data fence (fakes only) — §DN.10.
- §DM minors: a re-joined incarnation's re-written row can race a later leave (a stale row; delayed fresh-join window at
  one succession); census paging above 1,025 members skips (no client yet); the renewal lookup is a linear scan under
  the lock (index it before the 15k-member design point).
- §DL minor: the slow-member re-admission runs the stale-sibling rule on the data writer's own thread (tens of seconds
  bounded); hand it to the beat.

## Owner decisions / actions owed

- Install the fencer's udev rule on this venue (`scripts/dev/provision-local-venue --apply` writes `/etc/udev`)?
- The nvmet PR-preempt patch (`~/tmp/nvmet-pr-preempt-bug.md`): linux-nvme, To hch/sagi/kch, Cc linux-nvme + LKML +
  Guixin Liu, `Fixes: 5a47c2080a73`; the owner sends. Offered: a `git format-patch` and a test with a temporarily
  loaded patched `nvmet.ko`.
- Production multipath=Y: metadata member-death fencing stays detection-only there (the per-controller ioctl form is
  owed, or the upstream nvmet fix); data fencing has no fencer and should work under Y (unexercised on this venue).
- ESTALE semantics: a non-holder that unlinks a file it holds open gets ESTALE once the holder's kernel forgets it
  (NFS-style, no silly rename); a never-cached cross-owner unlink is destroyed only at the holder's next mount.
- Owner checks: s2idle resume on 7.3.1; the ported display fixes (external HDMI 2.1 VRR / OLED).

## Benchmarks (read before any A-B-B-A)

- Use `~/tmp/abba-gov.sh` (waits for the thermal governor at level 0 before EVERY mdtest run; the governor's 3.2 GHz
  cap made legs bimodal). AC power only; pause every agent; `sudo bash scripts/perf/ramnvme.sh down` afterwards.

## Open defects (no owner yet)

- Each NVMe writer open replays the ring privately three times (§DF.13(l)); a lighter claim probe.
- The handoff-11-03 list: the other ino-keyed maps with removals, reclaim-tick starvation in removal storms,
  transport_lease_overlong at load ~50, §CI.8; `SQUEEZEFS_META_FLUSH_INTERVAL_MS=0` fails the no-open close row (§CO.36).
