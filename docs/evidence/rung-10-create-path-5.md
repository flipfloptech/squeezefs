# Rung 10 — the create path, wave 5: the committer applies its own transaction (2026-10-21)

Continues `rung-10-create-path-4.md`. **The owner's ruling:** go for ext4's class — the reference's
engine floor (≈ 300 µs per create per thread, never attacked) is ours to remove; the §BB ack law is
what permits it. Venue: this workstation (32 threads, `7.2.8-9.33-omarchy-sqz`), the RAM NVMe set
(512-B grain); A = dev `12768c8`, B = `perf/create-path-5` (`SQUEEZEFS_META_APPLY=self`). Scratch
`~/tmp/createpath2/` (`w2-{A,B}-w5*`), `~/tmp/w4cp/`.

## Lever 1 — self-apply (decision 0003 §BC; `SQUEEZEFS_META_APPLY=self`, measured; `venue` ships)

The pass drains, recalls (A1½), admits, reserves ONE window and stamps the seqs, then hands every
member its TURN (`Done.Signal.turn`); the committer, on its own thread, resolves (seeded), locks ITS
leaves ascending, revalidates, captures its pre-images, applies, encodes its own entry
(`journal.partOps`), copies it into the journal tail and answers itself (a kernel op, §BB.1); the last
member builds the window's image (`journal.buildWindowFromOps`) and hands it to the lane. A member
that rolls out becomes a PAD entry over its range (no hole, no hole checkpoint). The turn spin (twice
the turn's predicted latency, capped at twice the §AB.4 wake): a send to a spinning receiver wakes
nobody, so the pass's fan-out costs no syscall — three of four turns found by the spin on creates. The
fsync cover reads the conveyor's reservation high-water mark (a self-applied window is reserved and
acked before the lane sees it). Red test: a 4-thread storm of kernel creates (`self_applies ≡ acks at
the apply ≡ commits`, a poisoned member a PAD with its window's survivors standing, nothing rolled
back, fsync drains the tail, the reopen replays every ack). The chain per create: enqueue → the pass
(one election; drain, recall, reserve, stamp: 5.8 µs) → the turn (spun, no wake) → the own apply →
the ack. ONE hop at most (the pass's wake of the lane fiber when it is not already running); the
applier's two hops are gone, the lane's round trip was gone with §BB.

**Measured (`mdtest -n 10000 -u`; loads at the phases' ends in brackets — other tenants shared the
host through most legs):**

| shape | A (dev) | B (self) | ratio |
|---|---|---|---|
| `-np 32` file creation | 93.4 / 93.7 / 96.7 k (L2/L28/L2) | 113.2 / 117.9 / 118.8 k (L2/L16/L0) | **1.21×** |
| `-np 32` file removal | 107.1 / 114.4 / 108.9 k | 80.8 / 153.1 / 123.5 k | 1.08× (0.75–1.40) |
| `-np 32` dir creation | 62.8 / 58.4 / 57.9 k | 54.5 / 50.5 / 51.9 k | 0.87× |
| `-np 32` dir removal | 60.7 / 59.5 / 57.9 k | 53.8 / 45.8 / 46.7 k | 0.82× |
| `-np 8` fc / fr / dc / dr | 118.0 / 106.3 / 78.4 / 59.0 k (L1) | 135.4 / 112.0 / 71.8 / 64.1 k (L26) | 1.15 / 1.05 / 0.92 / 1.09× |
| `-np 64` fc / fr / dc / dr | 115.4 / 98.4 / 57.4 / 54.2 k (L7–13) | 67.7 / 26.2 / 31.2 / 31.7 k (L8–13) | **0.59 / 0.27 / 0.54 / 0.58×** |

Inside the 32-rank create under `self`: the queue wait 17.7 → 7.9 µs (the spin), the pass 5.8 µs, the
create 238 → 233 µs — and `leaf_lock_hold` **81–93 µs per member** (one transaction!), of which the
wait to ACQUIRE each leaf is 41 µs × 3.9 leaves per create (`leaf_wait_other`, the new per-tree phases:
every take is in a slot tree). The leaves are the serializer once the hops are gone: 320 k creates in
2.7 s over one contended leaf set is ≈ 8.5 µs of exclusive time per create — the rate we see. With 64
ranks the per-transaction locks convoy (64 committers behind each other's leaves) and the venue's
union lock, which took every leaf once per 4-tx window, wins by 2–4×. **So `venue` ships** and `self`
is the lever, until the leaf is spread.

## Lever 2 — the lane's inode window (§BC.6; measured, reverted)

Two forms, both worse: (a) the rotor's tie-break by the committer's thread — the headroom law
("the most headroom, ties by the cursor") funnels every lane into the smallest tree: `leaf_wait_other`
41 → 63 µs, file creation 119 → 85 k; (b) the cursor's own tree while under the affinity ceiling —
41 → 65 µs, 85 k again, directory creation 75 k (its best) and removal 37 k (its worst). The contended
leaves are therefore not the rotor trees' edges alone; which tree and which leaf (3.9 takes per
create: the inode, the dentry, and two more) is the next wave's first measurement — the per-tree
wait phases stay in the stats for it. The global cursor is back.

## Lever 3 — atomic open (kernel patch 0034): not drafted

Not reached in this wave's budget; the daemon half and the INIT capability remain to be written
against `/build/kernel/linux-omarchy-sqz/src/linux-7.2.8` + 0001–0033.

## The shared directory (the `i_rwsem` row)

Every leg met loads of 22–26 from other tenants; B 20.9 / 17.6 / 15.7 / 14.8 k against A's earlier
quiet 18–20 k — inside the noise. The kernel holds the parent's `i_rwsem` exclusive across each
create's LOOKUP + CREATE round trips, so one directory is a serial chain of round trips: the daemon can
shorten each (the ack point, the own apply, one hop) and remove one of the two (atomic open); it
cannot overlap them.

## Tests

Unit `fmt test rails`: **2900/2990 passed, 90 skipped**, rails 29/29, fmt clean (the §BC storm, the §BB
rows, the lane's §BB.4 under the conveyor's cover; the §AW venue storm pinned to its arm). Smoke, the
shipped arm: INVALIDATION COHERENCE, SHARED HOT DIRECTORY, DEAD MANAGER, THE KILL ROWS (a)(b), THE
KILL MATRIX, kill -9 mid-tar ×2, THE REPLAY PROOF, the durability-points row — **50/50**; the same
battery under `SQUEEZEFS_META_APPLY=self` — **49/49**; INVALIDATION COHERENCE ×5. fsck clean after
every session.

## Risks and what was not verified

- `self` at 64 ranks: the hot-leaf convoy (0.3–0.6×); it ships off. Under `self` a window's members
  answer themselves before the window is submitted: the fsync cover is the conveyor's high-water mark
  (BC.4) — a barrier waits for every reserved window, including one whose members are still applying.
- Not run: a quiet shared-directory pair; the data rows (the shipped arm is dev's apply path; `self`
  touches no data row); the NVMe substrate; the full gate (the train's).
