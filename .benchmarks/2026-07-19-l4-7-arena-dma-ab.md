# L4-7 A/B — direct-to-arena DMA on the device-true interception row

**Verdict: NO-GO (self-delete executed).** The design's pre-committed
gate (`docs/design-preload-interception.md` §5.5.3 / PR L4-7 card):
the deepening stays only if it shows **≥ 10 %** on the device-true
interception row. Measured: **+5.2 % best row**. Per the card and the
no-dead-code non-negotiable, the implementation was deleted the same
day; it remains retrievable in full (implementation + its 4-row test
matrix) at commit **`b75a677`** on this branch's history.

## What was measured

The §5.5.3 candidate, staged as two levels:

1. **Dest plumb (built + measured)**: eligible ring-read handoffs plant
   their arena window as the router's `dest_addr` (the kernel
   transport's existing registered-payload slot) — device reads land
   directly in the completion payload region, deleting the pooled-
   buffer→arena copy. Landed-detection by pointer equality; parity
   unconditional; §5.3.1 rule 3 enforced (transform volumes never get
   a client-writable dest).
2. **`IORING_REGISTER_BUFFERS` fixed-buffer registration (not built)**:
   strictly a refinement of (1) — it can only reduce per-op cost
   *below* what (1) already deleted, so (1)'s measured ceiling bounds
   it. With (1) under the gate, (2) cannot clear it.

## Instrument (house law: stated per row)

- **fio psync** (`--direct=1 --time_based --group_reporting`), rand-4k
  `randread` and seq-1m `read`, thread counts stated per row, via the
  `LD_PRELOAD` shim; engagement verified per run (charter §3 rule 4:
  `ipc_ops_read` delta ≥ ~fio ops or the run exits INVALID).
- **elbencho is DISQUALIFIED on this box for il rows**: the installed
  binary is statically linked — `LD_PRELOAD` never loads, and the
  "row" measures kernel FUSE (caught by the engagement check on this
  rig's very first run: 176k "IOPS" with `ring_ops_read_delta: 0`).
  **PR L4-8 must verify its elbencho is dynamic or swap instruments.**
- Rig: `tests/run_ipc_dma_ab.sh` (leg per invocation; interleaved A/B
  pairs; `SQUEEZEFS_IL_ARENA_DMA=0` was the lever — deleted with the
  code).

## Substrates

- **devsub** (`tests/dev_substrate.sh` nvmet-loop: null_blk meta +
  zram data — the house device-true surface), mount
  `--interception -o direct_device_true`.
- /dev/shm file-backed (A/B-relative sanity leg only).

## Results (medians of interleaved runs; 5 pairs for rand-4k t8, 3 for the rest)

| Row | DMA on | Lever off | Delta |
|---|---|---|---|
| devsub rand-4k t8 (IOPS) | 128,159 | 121,802 | **+5.2 %** |
| devsub rand-4k t32 (IOPS) | 114,510 | 116,100 | −1.4 % |
| devsub seq-1m t8 (KB/s) | 8,035,783 | 7,779,098 | +3.3 % |
| /dev/shm rand-4k t8 (IOPS) | 115,987 | 112,698 | +2.9 % |

Run spread was wide (rand-4k t8 individual runs 90k–141k); the first
pair alone read +29.8 % — the multi-run medians are the adjudicating
numbers (multi-run discipline). `dma_reads` == `ring_ops_read_delta`
on every DMA-on run (100 % of eligible ops landed directly), so the
number measures the mechanism at full engagement, not a partial
plumb.

## Why the physics agrees

The deleted copy is ≤ 64 KiB (slab ceiling) against a device op the
rand-4k row prices at ~100 µs end-to-end; a 4 KiB memcpy is single-
digit µs of that at worst, mostly hidden behind the tokio handoff the
op pays anyway. The §5.5.2 ledger's "strictly one better than kernel
FUSE" copy-count claim was true and is simply not where this row's
time goes; the row is Little's-law-bound by delivered concurrency
(§5.8.3), which the plumb does not change (t32 confirming: wash).

## Standing residue

- The **test matrix** (`tests/preload_dma_tests.rs`) and the rig
  (`tests/run_ipc_dma_ab.sh`) were deleted with the implementation
  (they pin a mechanism that no longer exists); both live at
  `b75a677`.
- The **elbencho-static disqualification** and the engagement-check
  pattern carry forward to PR L4-8's scoreboard il mode.
- OQ-3 (sync write fast path) reads this note as its cost prior: the
  candidate's win was copy deletion on the completion path; the write
  path's severance copy is the same magnitude, so a service-thread
  sync write fast path must find its win in the *handoff wake*
  (~1–3 µs, G-L4-1 leg iii), not the copy — measure that directly if
  pursued.
