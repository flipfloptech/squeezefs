# Rung 10 — the pass claims every member's entry right after the first turn (2026-10-26)

Decision 0003 §BX.11. Branch `perf/handout-claims` (revised) on `perf/smo-tail` (`968e1dc`). **No benchmark was
run**. These are correctness runs only: 2 MDT + 2 OST, `mdtest -n 10000 -u`, phases settled, a `.stats` delta and
the `sqz-jrnl*` threads' on-CPU time per phase, fsck clean after every run. The owner's A-B-B-A judges the rates.

## Why the handout cost directory removal at -np 32

The owner's bisect put the handout alone (every entry claimed before the first turn; the unlanded variant, kept locally as `perf/handout-at-pass`) at −5.2 % on
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
committer's roll-out against the pass's claim holds, and fails without the CAS. **Red** `370fd4e`: a member
held at its turn never got its entry into the tail, and B parked behind A held at its turn.

## §BX.12 — claim only while the turns run late (`SQUEEZEFS_META_PASS_CLAIMS=auto`)

The owner's A-B-B-A of `3216d71` (vs dev `d0cf1f3`): every -np 64 row was up (+6.6 / +7.0 / +7.1 / +9.6 %),
and every -np 32 row was down (−7.2 / −5.6 / −3.0 / −2.7 %). The claims pay only where the profile's
expensive parks live: owners descheduled inside their claim, because a turn was delivered to a thread busy
with other committers.

The signal is the late turn itself. For each window, it records how long after the handout the last member's
turn started (`meta_turn_lag`), and an EWMA over 1024 windows tracks the share of windows late past 8 µs.
Claims switch on above 62 % and off below 50 %. Before the host's reboot, the shares were 17–37 % at -np 32
(23–46 % with claims) and 69–84 % at -np 64 (90–96 % with claims).

Correctness runs on `d60162b` give these counters for never / auto / always, per phase fc / fr / dc / dr:

| | never: parks, wait/answer | auto: windows claimed, parks | always: parks |
|---|---|---|---|
| -np 16 | 3.2–5.5 %, 0.7–1.0 µs | 0 / 0 / 0 / 0 %, 3.9–5.7 % | 0.3–0.8 % |
| -np 32 | 12.7–22.4 %, 3.2–5.4 µs | 0 / 0 / 0 / 1.1 %, 11.4–17.0 % (2 switches) | 0.17–0.51 % |
| -np 64 | 22.0–29.5 %, 11.5–24.0 µs | 99.0 / 94.8 / 98.7 / 100 %, 0.04–0.40 % (≤ 3 switches a phase) | 0.04–0.15 % |

With auto, the pass's handout histogram at -np 32 is about dev's (0–1.1 % of windows). At -np 64 auto claims
94.8–100 % of windows and parks stay at or under 0.4 %.

The host was rebooted mid-round and came back on the `powersave` governor (platform profile `balanced`).
-np 32 creates fell from 247k to 136k ops/s. In some runs the -np 32 late share rose to 53–79 %, and `auto`
then claimed at -np 32 as well (that run's never → auto rates, hints only, rose 136 → 150k, 143 → 175k,
91 → 122k and 133 → 142k). The decision follows the turns, not the rank count. The rates are the owner's to
measure on a quiet host.

**Models.** Model 14 holds with the pass as a claimer beside the owners in every switch pattern: none, both,
on for M0 then off, and off for M0 then on. The residency weakenings fail in each pattern. Model 16 holds with
the pass's switch on and off. Unit tests cover the hysteresis and the four mode arms. Smoke and xfstests pass:
generic/001, 013, 074 and 753.

## The A-B-B-A (2026-10-27, quiet host on AC power, the RAM-NVMe perf mount, dist builds)

`dev` `d0cf1f3` against the adaptive branch (`SQUEEZEFS_META_PASS_CLAIMS=auto`, the code of `d60162b`), two
A-B-B-A rounds (four legs per arm), `mpirun mdtest -n 10000 -u -i 1`, gate lock and every build slot held,
tctl ≤ 64 °C, fsck clean after all 16 runs; k ops/s, the mean of the four legs:

| | -np 32 dev | -np 32 auto | Δ | -np 64 dev | -np 64 auto | Δ |
|---|---|---|---|---|---|---|
| file creation | 184 | 189 | +2.3 % | 176 | 190 | +8.3 % |
| file removal | 230 | 231 | +0.7 % | 220 | 256 | +16.1 % |
| directory creation | 203 | 202 | −0.7 % | 198 | 193 | −2.6 % |
| directory removal | 210 | 207 | −1.2 % | 209 | 219 | +4.4 % |

The three minus signs sit inside the spread of `dev`'s own legs (191–222 k on directory removal at -np 64),
so -np 32 is neutral and -np 64 gains on the file rows. For the record, the arms that did NOT land: the
unconditional pass claims (`always`) read −7.2 / −5.6 / −3.0 / −2.7 % at -np 32 and +6.6 / +7.0 / +7.1 / +9.6 %
at -np 64; the SMO entry in the tail alone (`perf/smo-tail`) read noise (−7.4 … +5.3 %); the handout before
the first turn alone read −3.0 / +2.9 / +3.2 / −5.2 % and +0.2 / +9.4 / −7.8 / +3.6 %. Landed (gate 154 green,
75/75). Sessions: `/build/perf/hc4-abba/`, `/build/perf/hc5-abba/` (auto), `hc-abba/`, `hc2-abba/`, `hc3-abba/`,
`hc-bisect/`, `st-abba/` (the arms that did not land).
