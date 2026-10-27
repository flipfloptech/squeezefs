# Rung 10 — an SMO's entry in the journal tail (2026-10-26)

Decision 0003 §BX.10. Branch `perf/smo-tail` off `fix/bx-error-paths` (`d0cf1f3`). The turn-time claims and the
help are dev's, unchanged. **No benchmark was run**. These are correctness runs only: 2 MDT + 2 OST on 512-B loop
devices, `mdtest -n 10000 -u`, phases settled, a `.stats` delta per phase, fsck clean after every run. The rates
are the owner's A-B-B-A to judge.

An SMO reserves its entry inside its lock window. The entry used to become resident only when its structural
write landed through the lane, so every answer ahead of the landing above it waited for the build and the write.
Now `commitResident` claims the entry's image into the tail right after its build. One structural slot is
reserved for it, and the tail gate counts the rest of the table.

| per phase fc / fr / dc / dr | `d0cf1f3` (+ the counter) | `perf/smo-tail` |
|---|---|---|
| -np 32: answers parked at an SMO's entry | 0.87 / 0.65 / 1.04 / 0.67 % | 0.33 / 0.09 / 0.52 / 0.05 % |
| -np 32: all parked answers | 14.9 / 16.2 / 12.4 / 18.0 % | 13.3 / 15.2 / 11.0 / 17.8 % |
| -np 32: wait per answer | 4.7 / 4.9 / 4.3 / 4.9 µs | 3.2 / 4.5 / 3.2 / 3.7 µs |
| -np 64: answers parked at an SMO's entry | 0.92 / 0.98 / 1.34 / 1.22 % | 0.07 / 0.01 / 0.13 / 0.00 % |
| -np 64: all parked answers | 29.5 / 30.8 / 26.7 / 28.9 % | 28.7 / 30.4 / 25.5 / 27.9 % |
| -np 64: wait per answer | 24.9 / 27.2 / 20.5 / 25.1 µs | 21.5 / 25.9 / 14.0 / 19.1 µs |

The rest of the parks are the turn-time claims' own: a sibling's turn not yet run. The handout claims address
those (§BX's next rows). `meta_conveyor_tail_slot_misses` was 0 in every run. Red `29a630a` (backend "§BX an
SMO's entry rides the journal tail from its build to its landing"): B was not answered while a threshold drain's
entry was held before its write. With the fix, B is answered without parking, and a death replays through the
SMO's entry to B with `past_hole` 0. Model 13 holds with the structural slot, and a gate that counts the whole
table fails.
