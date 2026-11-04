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
| fidelity: a member's death fences its host on the DATA namespace before its extents are reallocated (X's write held past its own gates by `SQUEEZEFS_TEST_DATA_WRITE_HOLD`, `ba5c477`; X SIGSTOPped, evicted, recovered; M unmounts, M2 fills the 32 MiB / 64 KiB-block volume to ENOSPC; X resumed) | `expected 0, found 65536` — X's whole block landed over one of M2's files | `57fb5ee` |
| unit: a member's death is recovered only once its host is fenced on every DATA namespace | `expected .failed, found .acted` — the data preempt's outcome ignored | `51baaaa` |
| unit: the data admission registers this host on every DATA namespace before its first data write | does not compile: no admission exists | `4f5f7b3` |

## Models (`guard/data_fence_model.zig`, `8bd0fc7`)

`Death` 15,364 states (co-located: 95,277), `Succession` 2,973 — HOLD. Weakenings, each failing: no data
PR (today) → `ReallocatedUnfenced`; the release before the fence → `ReallocatedUnfenced`; WERO with the
holder's own preempt on nvmet → the recovery stuck (`final`), proceeding unproven → `ReallocatedUnfenced`;
the fence before Recovering → `StaleDataWrite`; a plain preempt → `ReallocatedUnfenced`; no data rule, no
drain → fail; no kept law → `StaleDataWrite`; no co-located kill → `ReallocatedUnfenced`; the victim-only
or no reconcile, no metadata re-check → fail.

## Checks (first round, `f45de01`'s tree; the final head's below)

The fix `a005a98`, the row's counters `b9d9998`: the three reds green; FULL test 3,281 / 3,470 (189 skipped);
the nvmet tier ×3 idle and ×1 under load 67 / 1 skipped / 0; the 27 rows ×3 71/71; the data-plane rows (writes
with and without the write cache, the freed block, ENOSPC, the recall, a deleted file's parked bytes, the
reservation's room, the orderly unmount) ×2 48 / 1 skipped (FUSE_SYNCFS); rails 32/32. Hot paths touched:
`nvme_dev.Device.writeBlock` (one armed-count load for the seam, a call split) and `classifyDmaOutcome`'s
EBADE arm (a counter); registration per mount and data namespace, the fence per death.

## Review round (the safety review of `f45de01`: no blocker, two majors, four minors)

Fact the reviewer found: nvmet NEVER supports PTPL (`pr.c` reports `ptpls = 0`; `nvmet_pr_init_ns` starts
every enabled namespace empty) — a target restart forgets every registration and reservation, metadata too.

| item | fix | red (alone, failing first) |
|---|---|---|
| MAJOR 1 — a transient error left a fenceable namespace unfenced | only stable facts mean absent (`pr_linux.classifyNode`); a transient node, Identify or FID 0x81 refuses the admission (§DJ.14) | `0d53bec`: `expected error.Refused, found void` |
| MAJOR 1 — step 2c fenced only the table | the set config read at each fence: retired rows leave at once, an untabled non-retired volume holds (gauge `data_pr_fence_held_volumes` + a loud log); escape `squeezefs volume waive-data-fence <mountpoint> vol-<id>` (§DJ.15) | `4e20765`: compile-red (no set-config source) |
| MAJOR 2 — after a target restart the data namespace stayed unreserved; joiners fail-stopped on metadata | the lapse beat (10 s): members re-register through the rule (metadata registration and currency, before and after); the manager re-reserves once every metadata registrant is back or after `LAPSE_GRACE_S` = 20 s, then reconciles; the guard takes the metadata hold after the same grace; a joiner re-registers on metadata (§DJ.16, §DJ.17) | `926a086` (tcp, both subsystems wiped and restored): J's write after M's re-acquire fails (fail-stopped) |
| minor 1 — a refused reconcile preempt refused the mount | the data report read again: the key absent is success | `b0bda7c`: `error.Refused` |
| minor 2 — a proven-dead co-located member got no drain | every data node drained always (requeue-blind residual kept, §DF.13(g)) | `e2c4bbc`: `TestUnexpectedResult` |
| minor 3 — registry dirs of test rows under the real `/run` | `SQUEEZEFS_TEST_HOST_REGISTRY_DIR`; the smoke and nvmet rigs use their run dir | — |
| minor 4 — model gaps | `Death.transient_d1` (32,145 states), `Succession.leaver` (25,890), `Lapse` (63,282 / 70,683): hold; each weakening fails; the lapse's windows reachable | model `d0a3b43` |

The fix `558d850`; at the final head on kernel.org 7.3.1, multipath=N:

| check | result |
|---|---|
| the five reds | green (the target-restart row 41/41 alone, 51 s; the units in the full test) |
| models | 21 tests: every as-built HOLDS (states above and in §Models), every weakening fails |
| FULL `zig build test` | 23/23 steps, 3,303 / 3,494 passed, 191 skipped, 0 failed |
| nvmet tier ×3 idle; ×1 under cpu+smoke load (≈ 31) | 68 passed / 1 skipped (multipath-tcp) / 0 failed, each |
| 27 eviction / re-join / KILL rows ×3; data-plane rows ×2 | 71/71 each; 48 / 1 skipped (FUSE_SYNCFS) / 0 each |
| full smoke; rails; `zig fmt --check` | 186 / 199 (13 skipped); 32/32; clean |

Hot paths: none added — the lapse beat is one report per data namespace (a joiner's plus one metadata report)
per 10 s on its own fiber; the guard's beat changes only inside a lapse.

## Open

The lapse window — a target restart until the re-reservation (§DJ.12 (a)); multipath=Y unexercised on this
venue; a re-format from another host over a kept data reservation (§DJ.12 (c)); no forced removal of a data
volume that can never drain.
