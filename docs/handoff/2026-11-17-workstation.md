# Handoff — 2026-11-17

Supersedes `docs/handoff/2026-11-11-workstation.md`. That note still says
PR-5 is unmerged and not to start PR-6. Do not follow it for next action.
PR-5 is on `dev` and on `origin/dev` at `1c060920`. This work is PR-6.

## Current state and next action

**Branch `feat/pr6`, from `dev` at `1c060920`.** Decision 0004 §8.3 is the
spec. PR-6 is the load and soak runs: F1, F2, F4, F7a and F7b ×3 under
`cpu+smoke` load; the 20-succession soak at the derived ring and at
`--meta-journal-mb 1024`; the eviction / re-join / KILL rows ×3; the §DM
churn unit. Any defect found lands red-first in its own fix PR before this
one. Do not start PR-7.

`meta_pr_park_extensions` was specified in §4.5.6 and was not counted. The
park beat now increments it when `extendTo` grants a deadline. A unit in
`park_reclaim.zig` is the seeded weakening: without the bind extension,
`expireIfDue` fires inside the large-ring bring-up window; with it, the
park stays until the extended deadline.

The runners are `tests/soak/load.sh`, `tests/soak/rows.sh`, and
`tests/soak/succession.sh`. The succession driver (two host identities, the
writer storm, the three-way manager cycle) is not wired yet.

**Next action, in this order.**

1. The succession soak driver in `tests/soak/succession.sh`, then a
   `--cycles 1` proof, then the 20-cycle derived-ring and 1 GiB runs.
2. `SQZ_VENUE_SLOT=1 scripts/venue tests/soak/load.sh` and
   `tests/soak/rows.sh`. Record the numbers in an evidence note.
3. §DO check rows once those runs are green.

Never push `feat/pr6` unless the owner asks. Never force-push. Never amend
landed commits. A fast-forward of `dev` is pushed by the local hook.

## What landed on `dev` in PR-5

`1c060920` passed three consecutive full venue gates (12, 13, 14), then
`dev` was fast-forwarded and `origin/dev` was pushed. The last two product
fixes were `79952102` (the slot cadence does not wait out a succession
park) and `1c060920` (F7a always runs its second fill wave).
