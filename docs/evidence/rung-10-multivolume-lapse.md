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
| Before the fix (`9344bff` on the model commit) | `fidelity.zig:2899` `JoinerFailStopped`: J's volume-1 node image write got `ReservationConflict` and the volume latched FAILED after the grace |
| After the fix, ×5 | Green. M: `writer_guard_lapse_beats` 2 (once 1), `writer_guard_lapse_early_ends` 1, `data_pr_lapses` 0; J: `meta_pr_lapse_reregistrations` ≥ 1 |

The unit red (`6d1b55f`) was compile-red: the fence knew no other volume. It pins the re-registration
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

## Checks

See decision 0003 §DN.8 for the full list (kernel.org 7.3.1, `nvme_core.multipath=N`).

No hot path changes: the extra work runs at the beat cadence, as one report per joined volume, and
pages are read only during a lapse or around a fresh registration.
