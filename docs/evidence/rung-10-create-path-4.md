# Rung 10 — the create path, wave 4: the ack after the apply, the journal tail (2026-10-20)

Continues `rung-10-create-path-3.md`. **The owner's ruling (2026-10-20):** the Lustre/ext4 posture for
metadata — acked once applied in RAM, durable at the next commit, `fsync`/`fsyncdir`/`syncfs` durable
on return — under two conditions: a daemon crash stays lossless; group commit. Venue: this workstation
(32 threads, `7.2.8-9.30-omarchy-sqz`), the RAM NVMe set (512-B grain); A = dev `561bb90` (mdtest) /
`a43bda8` (data rows), B = `perf/create-path-4`. Scratch `~/tmp/createpath2/` (`w2-{A,B}-w4r*`),
`~/tmp/w4cp/`.

## The law (charter §1 item 10, §5.2 "The ack point", §5.3; decision 0003 §BB; the ledger spec §3.4)

A member is acked once its window's RAM apply is published, its image is in the volume's **journal
tail** and the window is in the lane's hands; the landing and the barrier follow on the flush cadence
(50 ms derived; `0` = strict acks after the barrier; `SQUEEZEFS_META_ACK_POINT=landing` is the control
arm). A daemon crash is lossless — the supervisor replays the tail before it releases the writer claim;
a power cut loses at most the flush window of un-fsync'd metadata (ext4's posture, its 5 s against our
50 ms); a mount without a supervisor has no replay (said once at mount). A write that fails after an
ack rolls nothing back: FAILED at once (ext4's journal abort); an fsync parked under it answers
`VolumeFailed`. The §AG.2 recall is the pass's A1½, before the reservation, so before the new ack
point as before the old (INVALIDATION COHERENCE ×5 green).

## The tail (`meta/journal_tail.zig`) and the replay's proof

Per writable volume a size-sealed memfd: a header (device path, ring segments, grain), a slot table of
2× the lane's cells (48 B: state, seq_base, two covering runs), a MIRROR of the ring at its own
offsets. The applier copies the window's ops into the mirror and publishes the slot before the lane
handoff; the lane's verdict frees it. The bytes the lane writes ARE the replay's: idempotent, a landed
slot skipped. The daemon sends each tail's fd and the writer guard's fd over a `SOCK_SEQPACKET`
socketpair the intermediate made before the second fork; the supervisor parks in `poll` on it (a death
is a `POLLHUP`, seen at once) and at the death writes every written slot to the device in seq order,
`fsync`s, then closes the guard fds — the `flock`'s open file description is shared, so the dead claim
holds until the device is complete; a successor waits it out in `guard.proc`'s 5 s bound.

**THE REPLAY PROOF (smoke):** a `--daemon --supervise` mount under the landing-hold seam
(`SQUEEZEFS_TEST_JOURNAL_HOLD_LANDING`), three creates acked, `kill -9` → "7 un-landed window(s),
28672 bytes replayed and barriered", then "writer guard released", fsck clean, the remount finds all
three. The foreground control arm loses its create ("lost"), fsck clean: the window. The coalesced
`kill -9 mid-tar -x` row runs supervised now (the second proof); THE KILL MATRIX stands as written.
Unit: the tail's codec, claim/replay in seq order with landed slots skipped and a second replay
byte-identical, the wrap's two runs, a segmented ring's split, the memfd's seals and `replayFd`; the
lane's §BB.4 (an fsync behind an un-landed window waits for its landing); the backend's ack at the
apply (the commit returns under the hold, fsync parks, the landing frees the slot; the landing and
strict arms keep the commit parked) and the acked write failure (the record stands, FAILED at once).

## Group commit (§BB.6) — a measure, not built

Entries per lane write (`-np 32 -u`): fc 3.26 → 3.70, fr 3.32 → 4.07, dc 4.11 → 4.64, dr 1.99 → 2.22
(A → B): the early ack lets committers re-enter sooner, so the pass drains wider. Byte-granular
reservations need the shared-grain write serialization and the tail's per-entry rebuild — next wave.

## A/B (`mdtest -n 10000`, the quietest legs; loads at the phases' ends ≤ 6 unless noted)

`-np 32 -u`, four legs a side over four interleaved rounds, means: file creation A 104.9 k → B 97.8 k
(0.93×; legs 98.5–112.3 against 80.4–105.7), removal 103.1 → 108.9 k (1.06×), directory creation
62.5 → 62.2 k, removal 57.0 → 56.6 k. `-np 64 -u` (one pair, L6–14): creation 92.7 → **129.4 k**
(1.40×), removal 77.6 → 106.4 k (1.37×), directories 53.4 → 61.7 k, 51.0 → 52.3 k. `-np 8 -u`
(L7–11): 94.7 → 102.9 k, 83.5 → 97.7 k, 68.6 → 73.4 k, 49.8 → 62.5 k. Stat 10.08 → 10.34 M/s. Eight
appliers at 32 ranks: 108.9 k, the queue wait 26 → 12 µs. The shared directory met loads of 10–26 on
every leg (other tenants); B led all eight phases of two pairs without a quiet pair to stand on.

**Neutral at 32 ranks on this substrate, +37–40 % at 64.** The 32-rank create went 250 → 263 µs: the
queue wait 21 → 26 µs, `leaf_lock_hold` 68 → 95 µs per window (3.3 → 3.8 txs), the §AW pacing holds
33 k → 50 k. 100 % of windows ack at the apply (`tail_slot_misses` 0). What left the critical path —
the lane's round trip, `device` 12.6 µs, `meta_window_total` 54 µs — is small on RAM; what came back —
committers one round trip sooner — lands on the appliers' serialization over the hot inode leaf (§BA).
At 64 ranks that concurrency was what the loop lacked. On an NVMe (a 30–100 µs write plus a barrier)
the round trip the law removes is the create's largest term.

**Data rows unchanged** (A `a43bda8`, both at busy < 0.5 cores): rr4k-q32j4 1.739 → 1.748 M IOPS,
rw4k-q32j4 1.140 → 1.145 M, sw1m-q16j4 52.1 → 52.6 k. fsck clean after every session.

Bars: ext4 711 k / 573 k (file creation / removal), 246 k / 456 k (directories); xfs 471 k / 183 k.

## Tests

Unit `fmt test rails`: 2896/2986 passed, 90 skipped, rails 29/29; after the rebase onto `a43bda8` the
families 181/186. Smoke battery (coherence, shared hot directory, dead manager, the kill rows, the
kill matrix, kill -9 mid-tar ×2, the replay proof, the supervise rows, busy/lazy umounts, `sync -f`,
the durability-points row, the block-map tree) 58/58; INVALIDATION COHERENCE ×5.

## The first gate's two defects (`~/tmp/sqz-gate-58/59-create4.log`)

1. **A DEAD MANAGER HOLDING A STRIPED HOT DIRECTORY wedged.** B's log (`/build/tmp/smoke-2136151/daemon-b.log`):
   the reclaim against the remounted manager answered `UnknownLease` ("the prior lease is not
   custody") — the one verdict that poisons a joined mount (`appender_park_expiries` fired, every
   volume FAILED); the storm's creates parked at the move gate 48 s, the cross-owner intents stuck,
   the storm child unkillable in a request B never answered. The cause: A had acked B's slot leases at
   the apply, died unsupervised within the flush window, and came back without them. **The fix:** the
   apply ack is this mount's kernel's alone (`tx.kernel_origin`); a wire-served step, a control record
   (a lease, a grant, the claim, a membership page), a job's or a tool's commit is acked at the landing —
   every word of the symmetric protocol means durable (charter §5.5), so a peer never holds a lease or
   an attested step its holder's device lacks. The row then 10/10 under load (8 × `yes` + a parallel
   unit run; `~/tmp/w4cp/dm/`), the backend's §BB test proves a served verb's commit still parks.
2. **Two `squeezefs-unit` shards became zombies and the gates ran to their caps.** Shard 12's "10 s
   random-op storm" (an in-process FUSE session) passed its hang limit under the gate's load; the hang
   watch's `exit_group` could not end a process whose own threads were parked in requests on the
   mount it served (D state in `fuse_open` / `fuse_direct_read_iter`; the fd table lives while they
   do, so the connection never ends and the build runner never sees the shard end). The hang watch
   now aborts the FUSE connections of this process's pid-tagged squeezefs mounts before it exits
   (`/sys/fs/fuse/connections/<minor>/abort`), so a hung shard is reported in minutes, never at the
   gate's cap — the smoke rig's dead-daemon mounts included (the storm child's `wait4` of defect 1
   would have returned the same way). Why the storm test itself passed its limit twice on this branch
   and on none of the eight gates before it is not established.

## Risks and what was not verified

- The symmetric death ladder does not WAIT for a dead peer's replay: the order is time's (`T_owner`
  45 s against a replay of milliseconds on RAM, under a second on NVMe) and the PR registration's
  (the host's; it outlives the daemon until the ladder preempts it). A supervisor dead with its daemon
  leaves no replay (the window applies — documented).
- Not run: the NVMe-substrate A/B (the lever's gain), a quiet shared-directory pair, dm-log-writes
  rows for the power cut (the foreground arm of THE REPLAY PROOF is the window's witness).
