# Rung 10 — the pass claims every member's entry right after the first turn (2026-10-26)

Decision 0003 §BX.11. Branch `perf/handout-claims` (revised) on `perf/smo-tail` (`5d12737`). **No benchmark was
run**. These are correctness runs only: 2 MDT + 2 OST, `mdtest -n 10000 -u`, phases settled, a `.stats` delta and
the `sqz-jrnl*` threads' on-CPU time per phase, fsck clean after every run. The owner's A-B-B-A judges the rates.

## Why the handout cost directory removal at -np 32

The owner's bisect put the handout alone (`d230542`: every entry claimed before the first turn) at −5.2 % on
directory removal at -np 32. Counters from the rmdir phase at -np 32, without and with the handout (two runs
each):

- The handout encoded every member before the first turn was sent. Members' turn spins fell into parks more
  often: 61 → 69 % in rmdir, 35 → 43 % in file creation. The pass's own time per window rose by the claims
  (+2.4 µs).
- `perf record -g` of the rmdir phase: the claims are the pass's added cost (`claimMemberOnce` 1.6 % of the
  daemon's samples, most of it `partOps`/`entryOps`). The journal lane's thread also runs the reclaim's destroys
  (`destroyCorpses`, about 7 % of the daemon's samples in rmdir), so every µs the pass adds there sits on every
  turn's path.
- What the handout removed: at -np 32 the parks were mostly help-and-go. Two thirds of them were covered by the
  answer's own help before sleeping, and the wait per answer was 3–4.5 µs. There was little to gain there and a
  turn's latency to lose.
- A diagnostic build classified the -np 64 parks. About a third truly slept, almost all behind an owner
  descheduled inside its own claim (the CAS taken, the encode not done), which no helper may take over.

## The change

The pass sends member 0 its turn first. It then claims every member still unclaimed, with the claim's CAS, as a
helper does. Members keep claiming at their turns, and parked answers keep helping. The claims overlap the
members' wake and apply.

Rejected: encoding before the CAS, so that a helper could take over an owner mid-encode. A helper still reading a
member's records after the owner won, applied and answered would read a freed transaction.

| per phase fc / fr / dc / dr | `perf/smo-tail` | handout before the turn | the pass after the first turn |
|---|---|---|---|
| -np 32 parks | 13.3 / 15.2 / 11.0 / 17.8 % | 0.30 / 0.11 / 0.49 / 0.04 % | 0.47 / 0.23 / 0.55 / 0.14 % |
| -np 32 turn-spin falls | 35.1 / 43.2 / 30.4 / 61.1 % | 43.3 / 52.1 / 43.1 / 69.2 % | 35.3 / 48.3 / 36.8 / 61.9 % |
| -np 32 queue wait | 21.1 / 19.7 / 67.7 / 37.8 µs | 25.1 / 18.3 / 56.7 / 36.5 µs | 21.7 / 16.6 / 58.8 / 34.7 µs |
| -np 64 parks | 28.7 / 30.4 / 25.5 / 27.9 % | 0.09 / 0.01 / 0.09 / 0.01 % | 0.13 / 0.07 / 0.18 / 0.06 % |
| -np 64 wait per answer | 21.5 / 25.9 / 14.0 / 19.1 µs | 0.12 / 0.01 / 0.11 / 0.01 µs | 0.11 / 0.03 / 0.56 / 0.04 µs |
| -np 64 queue wait | 76 / 68 / 152 / 118 µs | 85 / 60 / 139 / 92 µs | 87 / 59 / 124 / 85 µs |

The pass's claims take 2.2–2.7 µs per window at -np 32 and 3.4–6.0 µs at -np 64. The `sqz-jrnl*` CPU per op is
2.6–3.6 µs in all three arms. `meta_conveyor_tail_slot_misses` was 0.

**Models.** Model 15 now runs with two helpers (the pass and a parked answer). Model 16 (GoneRollOut): a gone
committer's roll-out against the pass's claim holds, and fails without the CAS. **Red** `0679695`: a member
held at its turn never got its entry into the tail, and B parked behind A held at its turn.
