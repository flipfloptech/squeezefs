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

**The F7a misses the train then found (2026-11-26, `9348ef6e` and
`724c0368`):** one run in three, `barriers 0, retries 0, read ok`, the
shape of every F7a miss since gate 5. Not timing: instrumented, the
drain reads made 20,077 getxattrs a second against 5 node loads, and
J's 65 mapped nodes were all PINNED with the seam's clock empty. On a
forest every mint slot is its own tree with a pinned root; the rotor
spread the fill's files over ~60 slots, so a file's slot tree was a
single root-leaf, pinned and unevictable. The fill now gives the two
files the drain reads twelve 60 KiB xattrs each so their trees split and
`user.p` lives in an evictable leaf; the reader runs one serial stream
per file from before the preempt-all until 1 s after; the F7a line
prints the node-cache and divert deltas. Alone 5/5 with 36–41 real
loads. **Observation for the owner:** a forest pins one 256 KiB root per
touched mint slot regardless of the node-cache budget — 65 roots, 17 MiB
resident, on a 1 MiB budget after 88 small files. By design (pinned
classes are exempt), but worth a look at production slot counts.

**The train's three greens (2026-11-27T00:03Z, 00:28Z, 00:56Z) stand on
`5b2f0ef7`** (`.gate-train/ledger-v2.tsv`; 1830 / 1476 / 1673 s). On the
way there `dev` moved four times after PR-6's landing: the F6 fix
(`97de559a`), the F7a storm (`9348ef6e`), the F7a pinned-root fix
(`724c0368`), the handoff (`49a77e87`) and F12's diagnostics
(`5b2f0ef7`); each move restarts the count. Across the 14 train runs of
2026-11-26 F6 never recurred and F7a never missed after `724c0368`.

**F12 flaked twice in four runs on `49a77e87`** (2026-11-26T21:43Z,
22:33Z), both at the wait for `did not land this tick (HandoverDeferred)`:
the cadence made no release attempt on the idle rotor slot inside the
20 s after the idle instant. A PR-5 row untouched this week (ten earlier
train runs and every gate passed it; alone it passes), so it is a flake
under a loaded gate, cause unknown — the 40-line tail was all growth-
drain spam. `5b2f0ef7` makes the failure path print the cadence's slot
stats (`slot_rotor`, forced shrinks, LRU releases, handovers, deferrals,
door parks/resumes, ring grows/deferrals, journal stalls) and J's slot-
lease and membership needles. Next miss: read those; the candidates are
the slot never idle (`holderOps` is noted at door entry, two 40 s epochs),
M moved, or the cadence not running. Not in `docs/gates/flakes.md` yet;
the owner's call whether to list it with an owner.

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
   (the local hook publishes `origin/dev`). The gate train ran the
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
