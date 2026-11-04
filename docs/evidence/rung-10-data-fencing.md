# Rung 10 — data-namespace PR fencing (2026-11-03, decision 0003 §DJ)

**The ruling** (the owner): build data-namespace fencing next. DATA namespaces had no registrant since
rung 6: a frozen-then-resumed dead member could overwrite extents reallocated after its death, silently —
no device fence, no screen on data.

## The shape

| | change |
|---|---|
| reservation | every data namespace **Write Exclusive – All Registrants**; every member host registers under its host key; no single holder, nothing moves at a succession, no fencer: nvmet 7.3's All-Registrants preempt arm precedes the holder re-type (§DF.11's quirk), so the manager's own client fences — no second identity, so nothing refuses multipath=Y (unexercised here; §DJ.1) |
| admission | every writer, after its metadata admission and the fabric bind, before `DataPlane.open`: adopt, or the rule judged by the METADATA facts + the data node's drain + the register + the re-judge, only while the metadata namespace registers the host; unreserved ⇒ WEAR (§DJ.2) |
| succession | volume 0's writer preempts every data registrant the metadata namespace no longer registers (data report first) before it allocates (§DJ.3) |
| death | recovery step 2c after Recovering: preempt-and-abort on every data namespace, the report proving it; **unproven ⇒ the recovery holds** — no data screen to fall back on (§DJ.4) |
| co-located | the own-key case: the recoverer on the same host kills the dead member's process and drains the data nodes (§DJ.6) |
| re-join, online add, offline verbs | re-admitted after the successor's metadata admission; a latched data device refuses the re-join; online adds admitted before they take placement; `clone`/`staging promote` admit too (§DJ.7) |
| release | the last registrant keeps its registration where a preempt landed (§DJ.8) |

## Reds (each committed alone, shown failing first)

| row | on the code before the fix | commit |
|---|---|---|
| fidelity: a member's death fences its host on the DATA namespace before its extents are reallocated (X's write held past its own gates by `SQUEEZEFS_TEST_DATA_WRITE_HOLD`, `b09212a`; X SIGSTOPped, evicted, recovered; M unmounts, M2 fills the 32 MiB / 64 KiB-block volume to ENOSPC; X resumed) | `expected 0, found 65536` — X's whole block landed over one of M2's files | `ea921cf` |
| unit: a member's death is recovered only once its host is fenced on every DATA namespace | `expected .failed, found .acted` — the data preempt's outcome ignored | `27c4d9b` |
| unit: the data admission registers this host on every DATA namespace before its first data write | does not compile: no admission exists | `2cd5a8a` |

## Models (`guard/data_fence_model.zig`, `fce248b`)

`Death` 15,364 states (co-located: 95,277), `Succession` 2,973 — HOLD. Weakenings, each failing: no data
PR (today) → `ReallocatedUnfenced`; the release before the fence → `ReallocatedUnfenced`; WERO with the
holder's own preempt on nvmet → the recovery stuck (`final`), proceeding unproven → `ReallocatedUnfenced`;
the fence before Recovering → `StaleDataWrite`; a plain preempt → `ReallocatedUnfenced`; no data rule, no
drain → fail; no kept law → `StaleDataWrite`; no co-located kill → `ReallocatedUnfenced`; the victim-only
or no reconcile, no metadata re-check → fail.

## Checks

The fix `e9bf827`, the row's counters `402fd90`; on kernel.org 7.3.1, multipath=N, at the final head:

| check | result |
|---|---|
| the three reds | green (the fidelity row in every tier run below; the two units in the full test) |
| models | as built HOLD (states above); 9 weakening tests, every weakening fails |
| FULL `zig build test` | 23/23 steps, 3,281 / 3,470 passed, 189 skipped, 0 failed |
| nvmet tier ×3 idle | 67 passed / 1 skipped (multipath-tcp: multipath=N) / 0 failed, each |
| nvmet tier ×1 under cpu+smoke load (load ≈ 47) | 67 / 1 / 0 |
| 27 eviction / re-join / KILL rows ×3 | 71/71, each |
| data-plane rows ×2 (writes with and without the write cache, freed block not reused, ENOSPC, the write cache's recall, a deleted file's parked bytes, the reservation's room, the orderly unmount's landing) | 48 passed / 1 skipped (FUSE_SYNCFS: kernel < 7.4) / 0 failed, each |
| full smoke | 28/28 steps, 185 / 198 passed, 13 skipped (kernel features, other steps' artifacts) |
| rails, `zig fmt --check` | 32/32; clean |

Hot paths touched (nothing measured here; for the A-B-B-A): `nvme_dev.Device.writeBlock` (one armed-count
load for the seam, a call split) and `classifyDmaOutcome`'s EBADE arm (a counter). Registration is per mount
and data namespace; the death's fence and the admission are per death and per mount.

## Open

PTPL-less data-target power cycles (§DJ.12 (a)); multipath=Y unexercised on this venue; a re-format
from another host over a kept data reservation (§DJ.12 (c)).
