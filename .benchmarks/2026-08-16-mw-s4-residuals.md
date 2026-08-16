# MW §6.3 S4 residuals — mdstorm + scoreboard smoke + external QUICK set at the STAMPED-SOLO posture

**Date:** 2026-08-16. **Branch:** `test/stamped-solo-residuals` (binary
`c84c405b`, release, default features). **What this closes:** the three
residuals the S4 re-gate note (`.benchmarks/2026-08-15-mw-s4-regate.md`)
left owed "before the ARM rungs complete" — the ARM rungs (7–10) are now
complete, so this note is due. Every leg formats with the PRODUCTION
`format --multi-writer` stamp (all nine bits), never a test seam, via the
format levers this branch adds (`SQUEEZEFS_FSTESTS_FORMAT_ARGS` /
`SQUEEZEFS_SB_FORMAT_ARGS`, and the committed mdstorm rig's
`--format-args`).

## 1. The stamped-solo external QUICK set — GREEN from zero (and its live catch)

`sudo env FSTESTS_QUICK=1 SQUEEZEFS_FSTESTS_FORMAT_ARGS=--multi-writer
tests/run_fstests.sh` — **44 ran, 42 clean, 2 expected-shape (the
adjudicated generic/003+192 noatime class), 0 unexpected**, fail-fast,
one complete from-zero pass on the final binary. generic/795 (the
visibility campaign's test) passed at the stamped posture (191 s).

**Live catch (the gate doing its job):** the first from-zero run ABORTED
at generic/003 with a mount failure — the **staging-root liveness flock
teardown race**. `umount(8)` returns when the kernel FUSE connection
closes, but the predecessor daemon's flock (`.squeezefs_owner.lock`)
releases only at PROCESS EXIT, so fstests' zero-dwell umount→mount cycle
met its OWN staging root still held for a few more milliseconds and the
prelude refused loud. Only the STAMPED posture runs the writer-scoped
prelude, which is why 44-test QUICK sweeps at the unstamped posture never
saw it — exactly the class this residual gate exists to catch. Fix
(red-first, the `await_same_process_teardown_flock` precedent applied to
the staging plane): `hold_staging_root_lock_waiting` — the prelude's
own-dirs arm polls the flock for a bounded 2 s window; a dying holder
frees within one pass, a live co-located collision pays the bound once
and refuses with the verbatim message; probes and adoption arms stay
one-shot; unstamped mounts untouched. Pins:
`own_root_flock_race_with_dying_predecessor_is_absorbed` +
`own_root_flock_held_by_live_holder_still_refuses_loud`
(`tests/writer_scoped_staging_tests.rs`, suite 32/32 serial).

## 2. Scoreboard smoke at stamped-solo — GREEN (and its posture-independent catch)

`sudo env SQUEEZEFS_SB_SMOKE=1 SQUEEZEFS_SB_FORMAT_ARGS=--multi-writer
SQUEEZEFS_SB_SYSTEMS=sqz tests/run_scoreboard.sh` — exit 0, all three
regimes mounted and ran the micro-grid, engagement clean, no INVALID
cells. (Smoke = plumbing check by definition; no perf claims, and the box
carried background load — irrelevant to a non-gating micro-grid.)

**Catch (posture-independent):** the first attempt died at the FIRST sqz
mount — under `sudo`, the daemon mounts with `user_id=$SUDO_UID` (the
2026-07-13 ownership contract) and a non-allow-other FUSE mount denies
every other uid **including root**, so the runner's own `mountpoint -q`
readiness probe (and its root elbencho workloads) could never see the
mount. Any sudo-invoked scoreboard run was dead, stamped or not. Fix:
`--allow-other` on the scoreboard's sqz mounts (the `run_fstests.sh`
mount wrapper's precedent). A stale `user_id=1000` mount squatting the
mountpoint from a prior aborted run was part of the confusion and was
cleaned; the runner's lazy-unmount prelude handles that shape.

## 3. The mdstorm A-B-B-A — stamped never regresses beyond leg noise

The 2026-07-14 baseline's harness lived in `~/tmp` and is gone; this
branch COMMITS the instrument (`tests/mdstorm.c` — the K7 storm driver —
and `tests/run_mdstorm.sh`: canonical 20 k/100 k phase sequence, fresh
format per leg, comm-exact foreign-work quiet gate with bounded
wait-for-quiet, `.stats` snapshots per leg, and the `abba` verb running
stamped → unstamped → unstamped → stamped).

**Venue:** /dev/shm file-backed meta (2 GiB) + data (8 GiB), 8 threads,
100 % scale, quiet box (load1 0.90 at start; `saurond` ~32 % of one core
present throughout — the standing foreign-daemon honesty line; all four
legs flagged `clean` by the comm-exact gate). ops/s, one leg per cell:

| phase | stamped_1 | unstamped_1 | unstamped_2 | stamped_2 | stamped Δ (medians) |
|---|---|---|---|---|---|
| mkdir 20k | 6,165 | 6,455 | 6,193 | 6,562 | +0.6 % |
| create 100k | 5,146 | 5,354 | 5,110 | 5,225 | −0.9 % |
| stat 100k | 175,279 | 175,041 | 171,341 | 174,590 | +1.0 % |
| rename 100k | 4,064 | 4,285 | 4,113 | 4,061 | −3.3 % |
| unlink 100k | 4,526 | 4,499 | 4,480 | 4,549 | +1.1 % |
| manydirs 100k | 9,194 | 9,336 | 9,386 | 9,310 | −1.2 % |
| rmdir 20k | 5,131 | 5,199 | 5,030 | 5,351 | +2.5 % |

**Verdict: within noise, both orders.** The largest per-phase median
delta (rename −3.3 %) sits inside the venue's own same-posture leg
spread (the two unstamped rename legs differ by 4.2 % from each other),
and no phase moves in a consistent direction across the bracket. Solo
invariants from the per-leg `.stats` snapshots: **`dlm_rpcs = 0`,
`meta_kv_block_refs_drift = 0`, `invariant_tripwires = 0`,
`writer_guard_mode = flock+claim` on all four legs.**

## Verdict

All three §6.3 residuals CLOSED at the stamped-solo posture; the S4 gate
("stamped-solo within noise, `dlm_rpcs == 0`") now stands on the full
row set the design table names: B4 seq-write + rand-4k (the re-gate
note), mdstorm, scoreboard smoke, and the external QUICK set. The two
live catches (the staging-flock teardown race; the scoreboard
sudo-mount blindness) each landed with their red-first pins/fix in this
branch. Rung 10b's default-format flip still additionally gates on
rung 10's S9 acceptance (finding #6 there is OPEN — see
`.benchmarks/2026-08-16-mw-s9-arm.md`).
