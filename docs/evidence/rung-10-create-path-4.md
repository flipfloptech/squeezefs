# Rung 10 — the create path, wave 4: the ack after the apply, the journal tail (2026-10-20)

Continues `rung-10-create-path-3.md`. **The owner's ruling (2026-10-20):** SqueezeFS takes the
Lustre/ext4 posture for metadata — an op is acked once applied in RAM and durable at the next commit;
`fsync`/`fsyncdir`/`syncfs` durable on return — under two conditions: a DAEMON crash stays lossless,
and group commit. Venue: this workstation (32 threads, `7.2.8-9.30-omarchy-sqz`), the RAM NVMe set
(512-B grain), A = dev `561bb90` (the main tree's dist build), B = `perf/create-path-4`. Scratch
`~/tmp/createpath2/` (legs `w2-{A,B}-w4r*`), `~/tmp/w4cp/`.

## The law (charter §1 item 10, §5.2 "The ack point", §5.3 fsync; decision 0003 §BB; the ledger spec §3.4)

Under the coalesced cadence a member is acked once its window's RAM apply is published, its encoded
image is in the volume's **journal tail** and the window is in the lane's hands; the landing and the
barrier follow on the flush cadence (50 ms derived; `0` = strict keeps the ack after the barrier;
`SQUEEZEFS_META_ACK_POINT=landing` is the A/B control arm). The crash matrix: a daemon crash is
lossless (the supervisor replays the tail before it releases the writer claim); a power cut loses at
most the flush window of un-fsync'd metadata — ext4's posture, its 5 s commit interval against our
50 ms; a mount without a supervisor has no replay (said once at mount). A write that fails after an
ack rolls nothing back: the volume latches FAILED at once (ext4's journal abort); an fsync parked under
it answers `VolumeFailed`. The §AG.2 recall is the pass's A1½, before the reservation, so before the
new ack point as before the old (INVALIDATION COHERENCE ×5 below).

## The tail (`meta/journal_tail.zig`) and the replay

Per writable volume a size-sealed memfd: a 4288-B header (device path, ring segments, grain), a slot
table of 2× the lane's cells (48 B each: state, seq_base, two covering runs), a MIRROR of the ring at
its own offsets. The applier copies the window's aligned ops into the mirror and publishes the slot
(release) before the lane handoff; the lane's verdict frees it. The bytes the lane writes ARE the
replay's: no second codec, idempotent by construction, a landed slot skipped. The daemon sends each
tail's fd and the writer guard's fd over a `SOCK_SEQPACKET` socketpair the intermediate created before
the second fork; the supervisor parks in `poll` on it (the daemon's death is a `POLLHUP`, seen at once
rather than a 5 s probe later), and at the death writes every written slot to the device in seq order,
`fsync`s, logs it, and only then closes the guard fds — the `flock`'s open file description is
shared, so the dead claim holds until the device is complete, and a successor's `flock` waits it out
inside `guard.proc`'s 5 s bound (the dead locker is not this binary's pid, so the retry runs).

**The replay's proof (smoke THE REPLAY PROOF):** a `--daemon --supervise` mount under the landing-hold
seam (`SQUEEZEFS_TEST_JOURNAL_HOLD_LANDING`: every window admitted after ready stays un-issued), `mkdir
d && touch d/x d/y && echo hello > d/z` acked, `kill -9` the daemon → the supervisor's line "journal
tail of volume 0: 7 un-landed window(s), 28672 bytes replayed and barriered", then "writer guard
released", then "done"; `fsck --offline --scrub` clean; the remount finds `d/x`, `d/y`, `d/z = hello`.
The control arm — the same on a FOREGROUND mount — loses `e/w` ("lost"), fsck clean: the window.
The coalesced `kill -9 mid-tar -x` row now runs supervised and is the replay's second proof (every
entry tar was acked on present whole); the strict arm stays foreground. THE KILL MATRIX stands as
written (fsync'd bytes exact; the unsynced whole or a hole).

Unit: the tail's codec round-trip and refusals; claim + replay in seq order, a released slot skipped,
a second replay byte-identical, a filling slot never replayed, the wrap's two runs, a segmented ring's
split at the extent boundary; the memfd's seals and `replayFd` against a file device. The lane's
§BB.4 (an fsync behind an un-landed window waits for its landing; no flush tick while held). The
backend's ack at the apply (the commit returns under the hold, fsync parks, the landing frees the slot;
the landing arm and strict mode keep the commit parked) and the acked write failure (the record
stands, FAILED at once, fsync `VolumeFailed`, the tail keeps the window). Three durable-at-ack rows
of the guard gate pinned to the landing arm.

## Group commit (§BB.6) — not built; the measure

The pass's batch is the group. Entries per lane write (`meta_kv_journal_entries / lane_handoffs`,
`-np 32 -u`): fc 3.26 → 3.70, fr 3.32 → 4.07, dc 4.11 → 4.64, dr 1.99 → 2.22 (A → B): the early ack
lets committers re-enter sooner, so the pass drains wider. Byte-granular per-committer reservations
need two laws this wave had no room for (two writes to one grain serialized on the lane; the tail's
replay rebuilding a shared grain from entries).

## A/B (`mdtest -np 32 -n 10000 -u`, the quietest legs; load at each phase's end in brackets)

| leg | file creation | file removal | dir creation | dir removal |
|---|---|---|---|---|
| A r1/1 (L1) | 112.3 k | 111.1 k | 64.2 k | 58.5 k |
| B r1/3 (L1–2) | 105.7 k | 114.5 k | 69.6 k | 57.6 k |
| A r2/4 (L5–6) | 110.1 k | 109.1 k | 63.7 k | 57.6 k |
| B r2/3 (L6–8) | 104.8 k | 109.8 k | 56.6 k | 56.2 k |
| B, 8 appliers (L1) | 108.9 k | — | — | — |

Other agents' builds and benchmarks shared the host through the rounds (loads 5–20 on half the legs;
those are in the logs, not the table). **The ack point is throughput-neutral on this substrate**: the
create's mean 250 → 263 µs, inside the commit the queue wait 21 → 26 µs and `leaf_lock_hold` 68 → 95
µs per window (3.3 → 3.8 txs). 100 % of windows ack at the apply (`meta_conveyor_acks_at_apply`;
`_at_landing` 10 per phase: the mount's own; `tail_slot_misses` 0). What the lever removed from the
critical path — the lane's round trip, `device` 12.6 µs, `meta_window_total` 54 µs — is small on a
RAM namespace, and what it added — committers back in the queue one round trip sooner — lands on the
appliers' serialization over the hot inode leaf (§BA: the appliers' waits for each other's leaves are
most of the hold; the §AW pacing holds rose 33 k → 50 k per phase). The gain the law buys is the
device's: on an NVMe whose write is 30–100 µs plus a barrier, one round trip leaves every create.
Eight appliers (`SQUEEZEFS_META_APPLIERS=8`): the queue wait 26 → 12 µs, the create 244 µs, the rate
inside A's spread — the venue widened, the leaf the same.

The reference bars: ext4 711 k / 573 k (file creation / removal), 246 k / 456 k (directories); xfs
471 k / 183 k, 369 k / 158 k.

## Risks and what was not verified

- The symmetric death ladder does not WAIT for a dead peer's replay; it is ordered by time alone
  (`T_owner` 45 s against a replay of milliseconds on RAM, under a second on NVMe) and by the PR
  registration being the host's (it outlives the daemon until the ladder preempts it). A supervisor
  that itself died with its daemon leaves no replay (documented; the power-cut window applies).
- Group commit (§BB.6) is a measure, not a mechanism, this wave.
- The NVMe-substrate A/B (where the lever's gain lives) was not run; the data rows and the `-np 8/64`
  and shared-directory shapes under a quiet host are owed if the host's other tenants permit.
