# Handoff — 2026-11-20

Supersedes `docs/handoff/2026-11-17-workstation.md` for next action.
PR-5 remains on `dev` and `origin/dev` at `1c060920`. This work is PR-6
(decision 0004 §8.3). Do not start PR-7. Never push `feat/pr6` unless the
owner asks.

## Current state

**Branch `feat/pr6`, HEAD `de6edaa0`, from `dev` at `1c060920`.** Load,
eviction rows, the §DM churn unit, and both 20-succession soaks are green.
Numbers are in `docs/evidence/pr6-load-and-soak.md`. Raw logs:
`~/tmp/pr6-soak/`, with the green 20-cycle rings copied to
`derived20-green-18d6ee54/` and `journal1024-green-de6edaa0/`.

The official 16-loop `cpu+smoke` load was not run (thermal derating;
`load.sh` used `CPU_LOAD=4`).

## Next action, in this order

1. Decision 0003 §DO check rows for the PR-6 evidence (do not edit
   decision 0004 to claim completeness until those rows are in).
2. Land `feat/pr6` onto `dev` when the owner asks (fast-forward only; the
   local hook publishes `origin/dev`).
3. Optional: official 16-loop `cpu+smoke` load if the owner wants it, on a
   cooler box or after a long cool-down.

## Product fixes on this branch that the umount soak needed

`511498a0` successor grace is `T_self + grace`. `fa4caabe` releases a
`T_self` park when the successor admits the reclaim. `d11e0a96` parks on
manager link loss. `18d6ee54` keeps that park until this host
re-registers. `de6edaa0` sizes the 1 GiB-journal soak's metadata volume
at 20 GiB so two joiners fit under the heap/16 budget.
