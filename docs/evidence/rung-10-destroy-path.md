# Rung 10 — the destroy path under self-apply, and self-apply ships (2026-10-26)

Continues `rung-10-create-path-7.md`. Decision 0003 §BV. Venue: this workstation, `7.3.0-1.7-mainline-sqz`,
branch `perf/destroy-path-self-apply` off `380866f`. **No benchmark was run**: another agent shared the host
(load 7–17), so the rates below are correctness runs, not measurements — the counters judge the mechanism and
the owner's quiet A-B-B-A judges the rates. Rig `~/tmp/destroy-self/mdrig.sh`: 2 MDT + 2 OST on 512-B loop
devices over `/build/tmp` (the RAM set's grain: the §L pad hold derives off), `--daemon --supervise`,
`mpirun -np {32,64} mdtest -n 10000 -u`, one phase at a time, a `.stats` delta per phase, fsck after.

## What held the removal back

Wave 7 put the 64-rank collapse on "the reclaim's grouped destroy windows take the venue". On `380866f` the
destroy is not a group: it is ONE self-applied member of 64 corpses that locked its ~47 leaves ascending and
held them across every wait (the red test: the earlier of its two leaves still write-locked while it waits for
the second). Making it leafwise (§BV.1) cut the members' leaf hold 34 → 19 µs and the leaf-take wait 6 → 1 µs,
but **not the collapse**: the removal's window (handoff → verdict) stayed at 264 ms. The profile named the
rest — 26 % of the daemon's samples in `GrantPassGate.passBegin`'s hash probes on `sqz-jrnl0`:

- **The pass starved its own lane.** Under `self` nothing parks the pass (no applier pacing hold, no pad hold
  at 512 B), and 64 committers never let the queue run dry, so the pass ran batch after batch on `sqz-jrnl`
  while the windows it handed off sat un-issued in the lane's inbox until the window gate parked it
  (`sqz-jrnl` 19.5 s of CPU in a 24 s phase, against 1.5 s for the `venue` arm).
- **The maps it churns filled with tombstones** (every object, every window a new key; std's map rehashes
  only when it grows): each new key probed the whole table.
- **The journal tail's slots ran out.** Under `self` every member acked at the apply holds a slot until its
  window lands; the table was sized for one per window. 2,225 `meta_conveyor_tail_slot_misses` tripwires in
  one removal phase.

## The fix (five commits after the red ones)

Leafwise destroy (§BV.1, model 12); the pass yields the lane between batches (§BV.2); the maps compact
(§BV.3); the tail gate (§BV.4, model 13); the tail holds a window whole (§BV.5); `self` ships (§BV.6). Laws:
a destroy never waits for a leaf while holding one; stage B runs between any two batches of one pass; no member
ever meets the tail's table full; a window's PAD and every member's entry are in the tail before any member
answers.

**Found on the way: THE REPLAY PROOF fails under `self` on `380866f`.** The smoke row (a supervised mount,
landing held, kill -9, remount) replayed ONE window of the tail and lost `d/x`, `d/y`, `d/z`: each member claimed
a slot for its own entry, and the window's trailing PAD — the link to the next window — was only in the image
written at the landing, so the judged death replay (§BR.11 (c)) stopped at the first window's end. Red: backend
"§BV the death replay of self-applied windows" (`past_hole` 2 for 0). Fixed by §BV.5; the row passes.

| `-np 64` file removal, `self` (counters per phase) | `380866f` | leafwise only | all four |
|---|---|---|---|
| window, handoff → verdict (mean) | 227 ms | 264 ms | 1.2–2.0 ms |
| the pass's A1½ recall | 606 µs | 693 µs | ≈ 1 µs |
| `meta_conveyor_tail_slot_misses` | 2,225 | 2,169 | 0 |
| `sqz-jrnl` CPU (the pass venue's occupancy; `venue` arm 1.5 s) | 19.5 s | 22.6 s | 1.9 s |
| member leaf hold / leaf-take wait | 34 / 6 µs | 19 / 1 µs | 14 / 2 µs |
| a destroy: apply, leaves, per-leaf hold | (union) | 350 µs, 47, 7 µs | 249–443 µs, 38–48, 3–10 µs |

At 32 ranks the window went 3.6 ms → 0.3–0.5 ms, the recall 65 → 0 µs. `meta_conveyor_tail_gate_parks` stayed
0 on these runs (the lane lands promptly once it runs); the gate is the bound for a slow device or a floor ring.

**Correctness-run rates** (O-series, interleaved, loaded host — indicative only): file creation `self` 139–144 k
vs `venue` 103–105 k at 64, 100–112 k vs 106 k at 32; file removal 200–209 k vs 89–100 k at 64, 186–217 k vs
98 k at 32 — no collapse; directory creation 50–66 k vs 54–65 k; **directory removal 43–45 k vs 52–58 k**
(0.76–0.86×, §BV.6 (a)). Every run: fsck clean, 0 tripwires, 0 pass panics.

## Tests

Red first: `d70b6ad` (the destroy holds its earlier leaf), `709d0d2` (no lane turn; both maps' tombstones),
`0f10a59` (a member meets the tail full), `44b4538` (the tail lacks the window's PAD). Unit `test rails` with `self` the default: 3082/3246 passed, 164
skipped; filters `conveyor`, `reclaim`, `destroy`, `self-apply`, `backend`, `token`, `§BV` green; models 12
and 13 hold with every weakening failing. Smoke and xfstests: the branch's last section.

## Seen, not changed

§BV.7: directory removal under `self` (the daemon's guarded section is the shorter one — 41 vs 315 µs — so the
gap is outside the commit); `mkdir`'s parent-inode leaf convoy at 64 ranks (143–332 µs leaf waits); the turn
spin's 560–637 k fall-throughs per 640 k ops on driven FUSE lanes; the reclaim's ≈ 22 k corpses/s in both arms;
`self` windows' journal pad (67–95 B/op against 28–30); and §BB's crash law has a narrow hole in BOTH arms — a
member (`self`) or a window (`venue`, §AW.4) answered while an earlier one has not yet claimed its tail slot lies
past a hole if the daemon dies in that span, and the judged death replay drops it though it was acked.
