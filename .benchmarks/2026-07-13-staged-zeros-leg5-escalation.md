# Staged zeros-LOSS leg 5 — root-caused on tape; candidate fix NOT merged (stop-rule escalation)

Date: 2026-07-13 · Branch `fix/staged-zeros-leg5` (PRESERVED, unmerged, tip `84630a9`) off
dev @ `2303a7a` · Sandbox `~/tmp/leg5` (artifacts kept: `fail1..4/` recops/model/live
images + zero-count content tapes `tape.log..tape4.log`).

## What the new instrument proved (zero-count content tape)

Tape v5 added **zero-byte counts to every staged lifecycle event** (RMW seed,
stage-commit payload, promotion image, truncate clip, CFR source), turning content
corruption into a visible discontinuity between consecutive events. It caught leg 5
end-to-end, twice:

1. **The sole-copy free** (fail1/fail2): an RMW's own high-water-crossing `stage_write`
   enqueues a promotion, which consumes EXACTLY that staged image inside the RMW's
   stage→commit-lock window — commits (generation check passes against this stage's
   gen), publishes `block_map[0]`, and REMOVES the ring entry. The RMW's commit then
   published "ring is authoritative, map=None" (a lie — the entry is gone) and freed
   the promotion's mapping: the file's only copy. Tape: bare `router_free` of the live
   mapping; the offset reallocated to inode_23801/23802 and punched; the next durable
   clip read `zclip=370069/370069` (100 % zeros) and published zeros as truth.
2. **The stale-size promotion** (fail3): `promote_staged_file` persisted
   `{size=306494, image len=425111}` — the commit clones a `current` snapshot whose
   size lags the very image it promotes; a later TTL refill of that pair rolls an
   acked extend back to an implicit-zero tail.

## The candidate fix (tip `84630a9`, two commits)

- `a22ac2e` tests: `touch_staged_generation` unit contract (a pre-touch generation can
  neither release the entry nor pass a commit re-check; a consumed entry reports
  `None`) + an 8×350 promote-race hammer as the regression net.
- `84630a9` fix: staged-arm commit revalidates ring residency under `INODE_META_LOCKS`
  via the touch (Some ⇒ ring-authoritative publish + release is safe by construction;
  None ⇒ ADOPT the promotion's mapping, free nothing, stay `layout_dirty`);
  promotion persists `max(current.size, image_len)`.

## Why it is NOT merged — the honest verdicts

| Build | Aged protocol | QUICK |
|---|---|---|
| dev 2303a7a (base) | 3/3 red | {003,213} ×3 |
| candidate + tape (logging ON) | **3/3 CLEAN** (first ever) | — |
| candidate, clean build (official) | 3/3 red | R1 {003,**074**,213,**616**}, R2 {003,**112**,213}, R3 {003,213} |
| candidate minus size-floor (probe) | 2/2 red | — |

The tape build's 3/3-clean is **logging-serialization masking** (stderr writes act as
sync points in exactly the racing paths) — a heisenbug residual remains in the same
promote/RMW/truncate cluster. Worse, the clean candidate **re-flakes the historical
074/112/616 QUICK classes** that every prior tip passed ×3 — an attributable
regression signature (likely the adopt path or the touch's promotion-invalidations
shifting identity-visibility timing). Per the multi-run discipline (a first-run
deterministic/attributable failure aborts the roll) and the stop-rule (3+ distinct
attempts on this leg without clean-build convergence: the core fix alone, +D1/D2, and
the D1-revert probe), the candidate is **withheld**.

## State of the family after legs 1–4 (merged) + this investigation

- Legs 1–4 remain merged and hold: QUICK {003,213}-only on dev, all seeded suites
  green, `staged_payload_lost_reads=0`, allocator-lifecycle invariants clean on tape.
- Leg 5 is **fully characterized** (mechanism, window, artifacts) with a candidate fix
  that provably closes the taped interleave (the masked run's 3/3-clean is consistent
  with the mechanism being real) but ships a timing regression elsewhere.

## Recommended next iteration (for the owner of the follow-up)

1. Reproduce the QUICK 074/616 re-flake on the candidate and tape the identity
   revalidation paths — the adopt path's `layout_dirty=true` interacts with
   `persist_dirty_layout_if_needed`/fsync timing; the touch's mass promotion
   invalidation changes merge-worker cadence.
2. Consider the narrower alternative fix shape: instead of touch+adopt in the RMW
   commit, make the PROMOTION's ring-removal conditional on the mapping still being
   published (re-check under `INODE_META_LOCKS` *at removal time*, not just commit
   time) — removal is the destructive step; gating it may need no RMW-side changes.
3. The masking observation is itself a tool: bisect by adding a single `log::error!`
   at candidate call sites to find which serialization point hides the residual.

Branch `fix/staged-zeros-leg5` is preserved with the full history for that work.
