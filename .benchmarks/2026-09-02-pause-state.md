# Pause state — 2026-09-02 17:30 EDT (host reboot)

Written at the pause; resume from here.

## `dev` = `1a327a44` — what it carries

| Landed today | Commit(s) | Gate status |
|---|---|---|
| kvmap ladder complete (PRs 1–6c-i) + f43/f44/f45 | through `38c49b98` | **full `task check` green** |
| f46 (kvmap stream collapse) + suite hardening | `591d6e11`, `80b779b5` | green (the tip gate above) |
| f47 (overlay length floor) | `ed1bde68` | green (the tip gate above) |
| f48 (clean-unmount data loss, RELEASE/teardown overlay drain) | `38c49b98` | green (the tip gate above) |
| e2e audit rig of record | `659e343d` | docs-only |
| **PR A2** per-op trace ring | `98346dab` | contract suites only — **batch gate owed** |
| **Two-profile LTO** (release=thin, dist=fat) | `f9df1f3c` | version/knob suites only — **batch gate owed** |
| **D-1** owner concurrent in-frame dispatch (F-A) | 4 commits | suites green — **batch gate owed**; fleet field row owed |
| **W-3** overlay under the write-pipeline governor | 6 commits | suites green — **batch gate owed**; tcp devsub row owed |
| **R-1** device-read executor: measured, NOT fat, closed | 3 commits | suites green — **batch gate owed** |

Last fully-gated tip: `38c49b98`. Everything after it rides the user's
batch-gate rule (one `task check` per batch).

## Post-reboot checklist

1. `cd ~/Source/squeezefs && git status && git log --oneline -1` (expect
   `1a327a44`, clean).
2. **Batch gate**: `task check` on the tip (~1 h) — covers A2, two-profile
   LTO, D-1, W-3, R-1. On red: fix forward, one more from-zero pass.
3. Recreate the local substrate (does not survive reboot):
   `sudo SQZ_DEVSUB_TRANSPORT=tcp tests/dev_substrate.sh create`.
4. Owed measurement rows (the campaign notes list them):
   - W-3: tcp devsub A-B-B-A (1 MiB seq + rand-4k write) —
     `.benchmarks/2026-09-02-w3-overlay-depth-governor.md`.
   - D-1: the mw fleet ingest row (`tests/mw_fleet.sh` + the s9 ingest
     matrix row) — `.benchmarks/2026-09-02-d1-owner-concurrent-dispatch.md`.
   - Two-profile LTO: quiet-box fat-vs-thin build-time + perf brackets;
     move release-gate/scoreboard rows to `dist`.
   - f47: tcp-substrate acceptance re-run.
   - Raw randwrite control on squeeze-test (nullblk discards — care).
5. Next campaigns (docs/design-e2e-perf-audit.md §5 order): D-2 two-stage
   conveyor, R-2 READ fast-dispatch, R-3 fill-issue economy, D-3 resident
   pass task, D-5 connection multiplexing. Attribute the 4k-random gap
   with the A2 trace ring FIRST (`SQUEEZEFS_OP_TRACE=1`, `cat <mnt>/.trace`,
   `tests/op_trace_stitch.py`) before R-2/R-3.
6. kvmap PR 7 acceptance + release tiers (pjdfstests / LTP / fstests from
   zero) — after the audit's first campaign batch.

## squeeze-test (the field box)

- `/scratch/tmp/squeezefs.kvmap` + `libsqueezefs_il.so.kvmap` = `38c49b98`
  (kvmap + f46/f47/f48; **pre-A2/LTO/D-1/W-3**). Rebuild + reship after
  the batch gate: `task build:rocky8` then scp to the `.kvmap` names.
- `/scratch/tmp/squeezefs` + `libsqueezefs_il.so` = the user's own dirty
  build; `logs/sqz.log` = the user's.
- Agent scratch convention: `/scratch/tmp/sqz-agent/`, removed on exit.
- No daemon mounted at the pause.

## Local

- `/mnt/squeezefs` unmounted; no daemons; no fleet residue.
- Worktrees: none active (the campaign worktrees may be pruned:
  `git worktree prune`).
- Field artifacts: `~/sqz-field-artifacts/2026-09-02/e2e-artifacts.tgz`
  (the baseline rows + the f41 corpse log).

## Boards (unchanged)

f48: promotion-arm partial-image key reads as zeros (should fail loud);
read-triggered feed on an open unfsynced writer leans on the 30 s sweeper.
f46: rewrite epoch close still whole-map (~1 M range records/row); overlay
train floor probe; co-writer whole-map ship. Umount SIGTERM→abort
slowness. f36b blob refusal pairs. Serve-side over-cap crossing. mw+il
row. f29 cliff ladder. Cold-read latency floor (now R-3).
