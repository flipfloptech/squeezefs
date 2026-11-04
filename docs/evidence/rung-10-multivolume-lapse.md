# Rung 10 — a joiner's lapse beat on every joined metadata volume (decision 0003 §DN)

The safety review of §DL rated a pre-existing gap (§DL.8(b)) MAJOR. After an nvmet target restart
(nvmet keeps no PTPL), a joiner's lapse beat (`DataFence.metaLapseBeat`) re-registered its host
only on VOLUME 0's metadata namespace. On a multi-volume set, a restart of another metadata volume's
target played out like this:

1. That volume's own guard never saw its joiners come back, so its grace never ended early.
2. After the grace, its hold returned with the joiners still unregistered.
3. Every joiner's next metadata write on that volume was refused, and every joiner fail-stopped.

Nothing stale landed, but every multi-volume nvmet deployment hits this on routine target
maintenance.

## What changed

- **The beat covers every joined volume.** The data fence holds a client of its own for each joined
  metadata volume that has a device fence (`JoinedMeta` → `MetaVol`; volume 0's comes first). For
  each volume where the namespace is unreserved and the host is unregistered, the beat re-registers
  through the stale-sibling rule. The rule is judged by that volume's own pages and registry lines
  (`vol_index`), with the currency checked before and after. A fresh registration is counted per
  volume in `.stats` `data_fence_meta_volumes`. Only volume 0's lapse marks the data side's lapse as
  pending.
- **The currency is a page Live on EVERY joined volume.** Recovery runs per volume. A death
  recovered on volume 1 first finishes its preempt, its page Recovering, its data fence and its
  release while volume 0 still reads the member as Live and registered. Judged on volume 0 alone,
  that member could re-register on data during a data lapse. The manager's reconcile keeps it,
  because its key is still on volume 0's namespace, so its write lands on a reallocated block. The
  model's `currency_scope = .volume0` weakening shows this.
- **The data checks read every joined volume's registration.** This covers every fresh data
  registration of a joiner (admission, beat, re-admission, re-join), checked before and after. An
  adoption mutates nothing and is not checked.
- **The re-join re-points every volume.** `RejoinHost.readmitData` now takes every successor.
- **Unchanged:** each volume's guard already ends its grace by its OWN namespace's report. The
  manager's fence still judges volume 0 alone.

## The red row ("a restart of ANOTHER metadata volume's target…", tcp, two metadata volumes, 60 s grace seam)

| Run | Result |
|---|---|
| Before the fix (`0d2159e`; `9344bff` before the rebase onto §DM) | `fidelity.zig:2899` `JoinerFailStopped`: J's volume-1 node image write got `ReservationConflict` and the volume latched FAILED after the grace |
| After the fix, ×5 | Green. M: `writer_guard_lapse_beats` 2 (once 1), `writer_guard_lapse_early_ends` 1, `data_pr_lapses` 0; J: `meta_pr_lapse_reregistrations` ≥ 1 |

The unit red (`b1e8f5f`) was compile-red: the fence knew no other volume. It pins the re-registration
on volume 1, and it pins that a member recovered on volume 1 first is refused on volume 1 and on
data.

## Model (`guard/data_fence_model.zig` `Lapse`, `meta_volumes = 2`)

As built, the model holds:

| Restart | Volume 0 recovered first | Volume 1 recovered first | Depth |
|---|---|---|---|
| Volume 1's target only | 109,848 | 107,544 | 31 |
| Both targets | 1,793,960 | 1,756,872 | 56 |
| Volume 0's target only | 839,912 | 812,696 | 51 |

Every weakening fails:

| Weakening | Fails with |
|---|---|
| Beat on volume 0 only (the code before), volume 1 restarted alone or both restarted | `LiveFenced` |
| Currency and data checks on volume 0 only (volume 1 recovered first) | `StaleDataWrite` |
| Volume 1's grace ended by volume 0's registrations | `LiveFenced` |

Trims, with two volumes only: J's post-checks are skipped (always no-ops for a live member), host A's
re-registration on volume 1 is not modelled (nothing reads it), and a guard with no lapse ends at
once. The one-volume state counts are unchanged.

## The re-join (§DN.9; the safety review's MAJOR on §DN.7(b))

The bug predates this branch; this branch extended it to every volume. During a re-join in place,
`readmitData` re-pointed the data fence at the successors and freed the predecessor's clients. If
the membership join was then refused, the successors closed. From then on, every beat — and any
refused data write's re-admission — issued PR commands through the successors' freed
`pr_linux.Device`s and read their freed backends.

The red row (`97392ea`) evicts a joiner of a two-volume set while it is live, using the
member-beat hold seam, with `SQUEEZEFS_TEST_REJOIN_MEMBER_REFUSED` set. The joiner is re-admitted
on data and then refused. On the code before the fix it crashed 3 runs of 3:

```
General protection exception
uring128.zig:271 getSqe ← pr.zig:167 issueRaw ← report ← data_fence.zig:1025 metaVolumeLapseBeat
```

The fix makes the re-admission two-phase:

- `readmit` keeps the predecessor's inputs and clients pending instead of freeing them.
- The wire plane settles the re-admission through `RejoinHost.settleData`:
  - It commits once the successors are published; the predecessor's clients are freed then.
  - It aborts on both refusal arms (the membership join refused, `publishSuccessors` out of memory)
    BEFORE the successors close. The fence goes back to the predecessor, which stays open until the
    unmount.

The row now sees the joiner poisoned but alive. The fence says it was re-pointed at the
predecessor, and its next beat says it ran there.

| Check, at `f8f1a1e` | Result |
|---|---|
| The red | Green ×5 |
| The multi-volume lapse row | ×3 green |
| Focused units | 650 / 75 skip / 0 fail |
| nvmet tier ×1 | 72 / 1 skipped / 0 |
| The 27 rows ×1 | 71/71 |
| FULL `zig build test` | 23/23 steps, 3,323 / 3,518 (195 skipped), 0 failed |
| Rails, `zig fmt --check` | 34/34, clean |

## Checks (kernel.org 7.3.1, `nvme_core.multipath=N`; the full list in decision 0003 §DN.8)

| Check | Before the rebase onto §DM | At the final head |
|---|---|---|
| The fidelity red | Green ×5 | Failing again on `0d2159e`; green ×5 |
| Models | Hold; every weakening fails | (in the units) |
| Focused units | 645 / 74 skip / 0 fail | 649 / 74 / 0 |
| nvmet tier | ×3 idle and ×1 under cpu+smoke load (≈ 47): 71 / 1 skipped (multipath-tcp) / 0 each | ×1 idle: the same |
| 27 eviction / re-join / KILL rows | ×3: 71/71 each | ×1: 71/71 |
| Data-plane rows | ×2: 48 / 1 skipped (FUSE_SYNCFS) / 0 each | — |
| Full smoke | 186 / 199 (13 skipped) | — |
| FULL `zig build test` | 23/23 steps; 3,318 / 3,512 (194 skipped), 0 failed | 23/23; 3,322 / 3,516 (194 skipped), 0 failed |
| Rails, `zig fmt --check` | Green, clean | 34/34, clean |

No hot path changes: the extra work runs at the beat cadence, as one report per joined volume, and
pages are read only during a lapse or around a fresh registration.
