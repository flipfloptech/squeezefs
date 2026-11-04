# Rung 10 — the lapse grace's early end and the slow member's re-admission (decision 0003 §DL)

The round-2 safety review of data-namespace fencing (§DJ, landable) left two minors.

1. **The metadata grace had no early end.** After a PTPL-less target restart, the manager's guard
   re-registered its host and took the WERO hold back only `claim.LAPSE_GRACE_S` (20 s) after it saw
   the lapse. The data beat re-reserves only while the guard holds the metadata namespace, so with
   every host back both namespaces stayed unreserved for one beat + 20 s at least.
2. **Slow members fail-stopped.** A member whose data re-registration came after the manager's
   re-reservation met the reservation conflict at its next data write and latched the fence class
   (EIO until remount). This happened although it was registered on the metadata namespace, its page
   was Live and a lapse had just passed.

## What changed

- The guard's lapse beat reads the membership census's live writer keys (`Owner.writerKeys`, bound
  by the wire plane into every managed guard) after its own re-registration. When every key is
  registered, the grace ends at that beat: re-acquire, claim re-read, fencer. A flat volume has no
  member, so its grace ends at the first lapse beat. A symmetric volume with no census, or an
  incomplete one (a successor's re-assertion window), keeps the grace.
- The guard kicks the data fence's beat when it first sees the lapse and again when its hold returns.
  The data re-reservation then follows at once if every metadata registrant is back on data.
- A data write, barrier or discard refused with EBADE asks the data fence once
  (`nvme_derive.Readmit`). The data fence re-admits the host only when all of these hold:
  - a lapse is still pending for the process;
  - the host is registered on volume 0's metadata namespace;
  - the incarnation is current;
  - the currency holds again after a register through the stale-sibling rule.

  Then the command is issued once more. Any other refusal latches the fence as before.
- The census gets a roster lock. The owner shard is served on one thread per connection with no lock
  at all. That race was pre-existing and still open for its other readers (§DL.4); closed by §DM
  (`rung-10-owner-lock.md`).

## The window, in beats (the red row "the lapse grace ends early", tcp, M under a 600 s grace seam)

Every run printed the same counters, ×5:

| Counter | Value | Meaning |
|---|---|---|
| `writer_guard_lapse_beats` | 2 | One beat saw the lapse; the hold returned at the next, after J's beat re-registered J |
| `writer_guard_lapse_early_ends` | 1 | The grace ended early |
| `data_pr_lapse_kicks` | 2 | The guard kicked the data beat twice |
| `data_pr_lapse_beats` | 3 | The re-reservation came at the kicked beat right after the hold |
| `data_pr_lapse_early_rereservations` | 1 | The data namespace was re-reserved before the grace |

Before the fix, the same row never saw `writer_guard_pr_reacquires` reach 1 within 120 s, with J
re-registered.

New bounds with every live host back: both windows are at most two beats from the target's return. A
live member that never comes back still leaves the grace as the bound (one beat + 20 s).

## Reds (`104e2ab`, committed alone, each shown failing on the seams commit `186d48f` — `7e9db3f` before the rebase)

- "the lapse grace ends early once every live host is back": `fidelity.zig:2702`,
  `expected 1, found 0`.
- "a member slow on data … is re-admitted at its refused write": `fidelity.zig:2778`,
  `TestUnexpectedResult`. J's write and sync after the re-reservation failed because the fence
  latched.

## Model (`guard/data_fence_model.zig` `Lapse`)

As built, the model holds:

| Variant | States | Depth |
|---|---|---|
| As built | 1,124,580 | 48 |
| Data only | 734,976 | 44 |
| Slow member | 1,917,758 | 50 |
| Slow member, data only | 1,572,834 | 46 |

Every weakening fails:

| Weakening | Fails with |
|---|---|
| The grace ended by the report's own registrants | `LiveFenced` |
| A slow member that latches (the code before) | `LiveFenced` |
| The re-admission without its checks | `StaleDataWrite` |

The re-admission's window (`EbadeWindow`, the beat window's class) is reachable.

The model had a beat that withdrew its registration whether or not it was fresh. Beside the
re-admission, that withdrawal took back the re-admission's registration without an abort
(`ReallocatedUnfenced`). The code withdraws only a fresh registration, and the model now does too.

## Checks on the final code (kernel.org 7.3.1, `nvme_core.multipath=N`)

| Check | Result |
|---|---|
| The two reds | Green ×5 |
| Models | Hold; every weakening fails |
| Focused units | 614 pass / 65 skip / 0 fail |
| nvmet tier, idle ×3 | 70 passed / 1 skipped (multipath-tcp) / 0 failed each |
| nvmet tier, ×1 under `cpu+smoke` load (load ≈ 43) | The same |
| The 27 eviction / re-join / KILL rows ×3 | 71/71 each |
| Data-plane rows ×2 | 48 passed / 1 skipped (FUSE_SYNCFS) each |
| Full smoke | 186 / 199 (13 skipped) |
| FULL `zig build test` | 23/23 steps; 3,313 / 3,506 passed (193 skipped), 0 failed |
| Rails | 32/32 |
| `zig fmt --check` | Clean |

No hot path is touched. The write path's success arm is unchanged; the re-admission is asked only
on a reservation-conflict error. The census read and the kicks run at the heartbeat cadence, and
only during a lapse.
