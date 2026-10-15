# Rung 10 — the writer claim of a member ≥ 1 never refreshed and outlived the unmount (umountclaim)

**Seen.** `rung-10-epochperf.md`: on kamrui perf mounts (2 MDT + 2 OST LVs, dist build), mdt1's
writer-claim heartbeat failed `SlotBusy` and the claim "could not be deleted at unmount".

**Reproduced** (kamrui, `perf_mount.sh --fresh`, 4 × 2 GiB layout, then 64 KiB randwrite qd16 nj4
60 s `--end_fsync=1`, the teardown's umount + `fsck --offline`, the gate lock and every build slot
held): **4 of 4 sessions** failed every one of mdt1's 6 beats and its claim delete. The defect is
deterministic, not a race. In those 4 sessions the umount and the fsck were clean.

**Root cause.** The claim was an xattr at local ino 1. `forestSlotOfIno(1)` is forest slot 0 →
routing slot 0, which only member 0 hosts. Before the slot plane arms, the door admits everything,
so the mount's claim committed. After the arm, member 1's door answered `moved` for slot 0, and the
conveyor maps `SlotMoved` to `SlotBusy`. Nothing else held the slot. No conveyor, checkpoint or
journal work raced the claim: the door refused it on every beat. The claim went stale 45 s after
the mount under a live writer. On a PR substrate that makes the live holder preemptable, and on
any substrate a foreign host reads it as stale. It also survived the clean unmount.

**The umount failures of epochperf** (3 of ~13 sessions, `fsck` "live-mounted") are a different
event. The claim failure happened in every session, including the clean ones, and a same-host fsck
reclaims a dead pid's claim. "Live-mounted" means the daemon was still alive. The daemon refuses a
stop while the mount is held (decision 0003 §S), so a holder was present at the umount, most likely
that run's 2 s sampler. Not reproduced in 26 sessions here; the logs naming the holder are gone.

**Fix** (0003 §AS). The claim is a tree-0 control record (`control_records.writerClaimKey`), as
`dynamic_routing.md` Q4 had already ruled. It takes no door token and is never migrated. Gone with
it: the FUSE screen row, the digest exclusion and the ino-1 chain scan. Beside it: the migration's
bulk copy now unretires the extents it writes. Its crash-table test loaded `RetiredExtent` once
the claim's tree-0 writes changed the target's allocation pattern.

**Red → green.** `meta_set` "a two-member set: every member's writer claim refreshes on the beat
and is deleted at the clean dismount". Red: `heartbeat_failures` +1 with the venue's exact two
warnings. Green on kamrui, along with the guard, claim, migrate, control, heartbeat, dismount,
xattr-allowlist and digest filters.

**Loop after the fix** (same session shape): **22 of 22 clean**, with 0 heartbeat failures, 0 delete
failures, `fsck` 0 findings and every umount orderly.

**Seen, not this change.** Every session, before and after the fix, logs the tripwire
`routing_split_refused` about 67 times and "times drain cadence: drain failed (InputOutput)" about
1,100 times. Owner: the times drain / cross-owner intents.
