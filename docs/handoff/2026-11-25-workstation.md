# Handoff — 2026-11-25

Supersedes `docs/handoff/2026-11-20-workstation.md` for next action.
PR-5 remains on `dev` and `origin/dev` at `1c060920`. This work is PR-6
(decision 0004 §8.3). Do not start PR-7. Never push `feat/pr6` unless the
owner asks.

## Current state

**PR-6 landed on `dev` at `04c8990f` (2026-11-25), then the gate train
went red twice on it, both on F6; the fix `fix/succession-peer-key` is
fast-forwarded on top.** Load, eviction rows, the §DM churn unit, both
20-succession soaks and the landing gate (gate 6, 75/75 steps, 31 min)
are in `docs/evidence/pr6-load-and-soak.md`. Raw logs: `~/tmp/pr6-soak/`,
`~/tmp/pr6-gate/gate{1..6}.log`, `~/tmp/pr6-gate/f7a-*.log`.

**The F6 deadlock the train found** (`.gate-train/ledger-v2.tsv`
2026-11-25T21:56Z and 22:24Z; F6 passes alone and passed gates 5–6):
`parkForSuccession` marked every peer volume `reregister_pending`, and
only `successionReregister` clears it, which never runs for a registered
host. S preempts v0, J re-registers on v0, S preempts v1, v1's park marks
v0 again — forever. Decision 0004 §4.10 releases "once every joined volume
where the host was unregistered has re-registered", so a volume that
already did owes nothing: `JoinedSet.post_preempt_registered`, set by a
successful re-registration, cleared when the volume observes itself
unregistered; such a peer is still parked (the cross-volume hold) but not
marked. Red unit in `joined.zig`, F6 ×3, one full gate with `-Dfresh=true`
(75/75, `~/tmp/pr6-gate/gate8-f6fix.log`). The train re-gates `dev` from
here; it is running (`scripts/gate-train`, started this session).

**Residual hazard, not fixed:** `SESSION_IDLE_TIMEOUT_MS` (60 s) reaps a
quiet membership session; the dialer's EOF fires `on_lost`, and
`onManagerLinkLost` parks the self volume with `reregister_pending`,
which a registered host can never clear. Both F6 failures showed that
line ~65–85 s into the stuck park (a parked member after a granted
reclaim sends nothing). Renewals every `min(T_self/3, SHIPPED_BEAT_MS)`
keep a running member's link warm, so it needs a long post-grant park,
which the fix removes; it stays reachable. Directions: the reader treats
an EOF with no cell armed as a retire, not a loss; or the parked member
keeps the link warm. Owner's call before PR-7.

Eight product fixes landed with PR-6 (`3f9f1dec`, `7bb598ca`,
`8c7c15bb`, `e06771bb` on the fence beat and the retired mux, found by the
gates; `511498a0`, `fa4caabe`, `d11e0a96`, `18d6ee54` for the clean
unmount soak), and the ninth above after it. They are the full tier (the
coherence and fencing laws), so each had its full gate before landing;
the gate train runs the three post-landing greens.

The official 16-loop `cpu+smoke` load was not run (thermal derating;
`load.sh` used `CPU_LOAD=4`).

## Next action, in this order

1. Done 2026-11-25 on the owner's word: decision 0003 §DO.34–§DO.43 record
   PR-4, PR-5 and PR-6 as built (the table stopped at DO.32; DO.33 is
   PR-4's premise resolution), and `feat/pr6` is fast-forwarded onto `dev`
   (the local hook publishes `origin/dev`). The gate train runs the
   post-landing greens; a red on these commits is fixed within the hour
   or reverted.
2. **The conformance suites on the current venue and kernel.** Every gate
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
3. Optional: official 16-loop `cpu+smoke` load, on a cooler box or after a
   long cool-down.

## What F7a taught (for the other nvmet rows)

A `python3 -c` one-liner lists its cwd at startup, and a relative path is a
lookup in the root; ino 1 is the manager's, so with the manager frozen
either op parks at `meta_backend_start` until the successor admits the
reclaim. A row that reads during a succession opens its fd while the
manager is alive and uses the fd (F7a, F7b). A re-mount under a dead
sibling's identity while its lease is live also parked a root readdirplus
with the manager alive — not pursued (not PR-6), noted here.
