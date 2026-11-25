# Handoff — 2026-11-25

Supersedes `docs/handoff/2026-11-20-workstation.md` for next action.
PR-5 remains on `dev` and `origin/dev` at `1c060920`. This work is PR-6
(decision 0004 §8.3). Do not start PR-7. Never push `feat/pr6` unless the
owner asks.

## Current state

**Branch `feat/pr6`, HEAD `52252916`, from `dev` at `1c060920`, tree
clean.** Load, eviction rows, the §DM churn unit, both 20-succession soaks
and ONE full venue gate (gate 6, 75/75 steps, 31 min) are green. Numbers
and the five red gates that preceded it are in
`docs/evidence/pr6-load-and-soak.md`. Raw logs: `~/tmp/pr6-soak/`,
`~/tmp/pr6-gate/gate{1..6}.log`, `~/tmp/pr6-gate/f7a-*.log`.

Eight product fixes landed on this branch (`3f9f1dec`, `7bb598ca`,
`8c7c15bb`, `e06771bb` on the fence beat and the retired mux, found by the
gates; `511498a0`, `fa4caabe`, `d11e0a96`, `18d6ee54` for the clean
unmount soak). They are the full tier (the coherence and fencing laws), so
the one full gate was required before landing; the gate train runs the
three post-landing greens.

The official 16-loop `cpu+smoke` load was not run (thermal derating;
`load.sh` used `CPU_LOAD=4`).

## Next action, in this order

1. **The owner's call on the §DO rows.** Decision 0003 §DO says every PR
   lands with its §DO rows; PR-1 to PR-3 did (DO.1–DO.32), DO.33 is PR-4's
   premise resolution, and no rows exist for PR-4, PR-5 or PR-6. The
   standing instruction to this work is to leave decisions 0003 and 0004
   unedited, so none were written. Either the owner lifts that for one
   docs commit (DO.34+ for PR-4/5/6, drafted from the evidence notes) or
   the debt stays recorded here.
2. Land `feat/pr6` onto `dev` when the owner asks: `git checkout dev &&
   git merge --ff-only feat/pr6` from `/home/justin/Source/squeezefs-zig`
   (the local hook publishes `origin/dev`), then delete the branch.
3. **The conformance suites on the current venue and kernel.** Every gate
   runs the pjdfstest subset (`suites-smoke`, ledger verdict 0/0/0 in
   gate 6). The release gate (`scripts/venue zig build -Drelease-gate=true
   gate-release`: full pjdfstest, LTP fs+syscalls, fstests `-g auto`,
   mdtest, tar-x; 3 clean runs each, hours, venue-exclusive) last ran in
   the rung-9/10 notes on kernel 7.2.x; stock 7.3.1 (no `FUSE_SYNCFS`),
   PR-5 and PR-6 have not been through it. Suggested order: one
   `SUITE_RUNS=1` pass per suite through `scripts/venue` to find the delta,
   red in-process repro-ports and ledger rows (`docs/gates/*-exceptions.md`;
   a syncfs-only failure on 7.3.1 cites decision 0003 §DB), then the 3-run
   count. Never beside a gate.
4. Optional: official 16-loop `cpu+smoke` load, on a cooler box or after a
   long cool-down.

## What F7a taught (for the other nvmet rows)

A `python3 -c` one-liner lists its cwd at startup, and a relative path is a
lookup in the root; ino 1 is the manager's, so with the manager frozen
either op parks at `meta_backend_start` until the successor admits the
reclaim. A row that reads during a succession opens its fd while the
manager is alive and uses the fd (F7a, F7b). A re-mount under a dead
sibling's identity while its lease is live also parked a root readdirplus
with the manager alive — not pursued (not PR-6), noted here.
