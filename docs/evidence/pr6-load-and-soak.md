# PR-6 load and soak (decision 0004 §8.3) — 2026-11-18 to 2026-11-20

Venue: local Linux ≥ 7.3 (strixhalo). Load and soaks ran under the thermal
governor, with cool-down to Tctl ≤ 70 °C and level 0 before each heavy
leg. Official `cpu+smoke` with 16 busy loops was not run; `load.sh` used
`CPU_LOAD=4`. Branch `feat/pr6` at `de6edaa0`, from `dev` at `1c060920`.
Logs under `~/tmp/pr6-soak/`.

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

## Not in this note

The official 16-loop `cpu+smoke` load. Decision 0003 §DO check rows.
Landing `feat/pr6` onto `dev`.
