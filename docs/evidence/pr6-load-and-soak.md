# PR-6 load and soak (decision 0004 §8.3) — 2026-11-18 to 2026-11-25

Venue: local Linux ≥ 7.3 (strixhalo). Load and soaks ran under the thermal
governor, with cool-down to Tctl ≤ 70 °C and level 0 before each heavy
leg. Official `cpu+smoke` with 16 busy loops was not run; `load.sh` used
`CPU_LOAD=4`. Branch `feat/pr6` from `dev` at `1c060920`; the soaks ran at
`de6edaa0`, the full gate at `52252916`. Logs under `~/tmp/pr6-soak/` and
`~/tmp/pr6-gate/`.

## Load and eviction rows

`tests/soak/load.sh` with `CPU_LOAD=4`: F1, F2, F4, F7a, F7b each ×3, every
leg 41/41. F7a `preempt_all_ns` 59.832 / 59.325 / 59.270 ms, one drain
barrier, one retry, read ok. Tctl peaked at 88 °C, governor level 1.
Log: `~/tmp/pr6-soak/load.log`.

`tests/soak/rows.sh`: smoke 67 pass, 1 skip (`other_step` libsqueezefs_il.so)
×3, different seeds, no `HUNG TEST`. §DM owner-churn unit (one live pass in
the first attempt): 4000 ticks, 15849 joins, 130727 renewals, 12008 reader
passes, 132 evictions (132 flaky, 0 steady), 0 wrongful. Tctl peaked at
78 °C. Log: `~/tmp/pr6-soak/rows.log`.

The park-extension weakening unit in `park_reclaim.zig` is green: without
the bind extension, `expireIfDue` fires in the large-ring window.

## 20-succession soak

`zig build succession-soak` via `tests/soak/succession.sh`. Two joiners plus
one data writer; manager cycles SIGKILL, SIGSTOP past the claim TTL, and a
clean unmount. Assertions held on both rings: 0 fail-stops,
`appender_park_expiries` 0, settles = 20, early = 20 (≥ 18), fsynced bytes
back, fsck 0, `meta_pr_preempt_kept` 0, `data_pr_reconcile_in_window` 0, no
park reached its deadline.

| ring | commit | settles | early | extensions | window_ms | fsynced j1 / j2 / data | log |
|---|---|---:|---:|---:|---|---|---|
| derived | `18d6ee54` | 20 | 20 | 420 | 5556–5855 | 77698 / 63962 / 77707 | `~/tmp/pr6-soak/derived20-green-18d6ee54/` |
| `--meta-journal-mb 1024` | `de6edaa0` | 20 | 20 | 420 | 6757–7370 | 72062 / 59674 / 72071 | `~/tmp/pr6-soak/journal1024-green-de6edaa0/` |

A 3-cycle derived proof on `18d6ee54` (kill / stop / umount, all early,
5498 / 5621 / 5649 ms) is under `~/tmp/pr6-soak/cycles3-green-18d6ee54/`.

The 1 GiB ring formats on a 20 GiB metadata volume. A 2 GiB volume
formats but refuses the first join: appender 0's 1 GiB ring is charged
against the heap/16 budget.

## What made the clean-unmount cycle hold

On a clean manager unmount the joiner's previous host key stays visible
until the successor's preempt-all. Without a park that lasts until
re-registration, an already-acked journal landing can be fenced and
`rollbackHook` latches the volume FAILED.

- Successor grace is `T_self + grace` (`511498a0`), so a member who parks
  at `T_self` is not reaped when the successor arms.
- `followReclaim` releases a `T_self` park (`fa4caabe`).
- The mux `on_lost` hook parks to hold acks (`d11e0a96`).
- The succession park, and any in-flight landing, stay held until
  `successionReregister` adopts or registers a key for this park
  (`18d6ee54`). A still-standing key from the previous manager does not
  clear the park or classify a write EIO as a retry.

## The full gate (six runs, 2026-11-21 to 2026-11-25)

Gates 1–5 were each red on one thing, fixed red-first on this branch:

| gate | red | fix |
|---|---|---|
| 1 `e97a1d4e` | rails: soak knobs unregistered / not harness-kind | `8506a601` |
| 2 `8506a601` | re-join smoke: the fence beat latched FAILED on a Recovered page (decision 0003 §BS says re-join eligibility) | `3f9f1dec` |
| 3 `3f9f1dec` | F4/F9: no `reservation-conflict` log once `retireIncarnation` fail-stopped first | `7bb598ca` |
| 4 `7bb598ca` | F6: the beat returned early on Recovered and never repointed; then J's umount hung because the retired mux's `on_lost` re-parked it | `8c7c15bb`, `e06771bb` |
| 5 `e06771bb` | F7a `barriers 0, retries 0` with the read ok (one run in three) | `52252916` |

F7a's two shapes, both in the test's path to the device: mine/t's clean
leaf survived the budget sweep (a clock with second chances) so the
getxattr never left the node cache; and the `python3 -c` path getxattr
listed its cwd and looked `mine` up in the root — the frozen manager's
directory — and parked at `meta_backend_start` until the successor admitted
the reclaim (~35 s; every cold-remount failure in `~/tmp/pr6-gate/f7a-*.log`
shows the stall, no pass does). The fix is a getxattr-only seam
(`SQUEEZEFS_TEST_EVICT_CLEAN_NODES_ON_GETXATTR`, the budget sweep with a
target of 0) and F7b's shape for the read (the fd opened while M is alive,
`getxattr(fd)` at the drain). Isolated F7a 5/5 at 59.4–59.8 ms.

Gate 6 at `52252916`: 75/75 steps, 3863/4095 tests (232 skipped), nvmet 91
pass / 3 skip, F7a `59.4 ms, barriers 1, retries 1`, pjdfstest ledger
verdict 0/0/0, 31 min. `~/tmp/pr6-gate/gate6.log`.

## Not in this note

The official 16-loop `cpu+smoke` load. Decision 0003 §DO rows (owed for
PR-4, PR-5 and PR-6; the owner's call, see the handoff). Landing `feat/pr6`
onto `dev`.
