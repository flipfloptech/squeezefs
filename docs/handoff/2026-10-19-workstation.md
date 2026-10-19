# Handoff — 2026-10-19: the first local performance wave

**Checkpoint:** `dev` at the commit that adds this note (on `8c1aa86`); everything pushed. The
2026-10-18 note holds the release sequence, the owed lists, the rulings and the venue plan; this one
adds the day's performance work. Venue: this host (`SQZ_VENUE=local`).

## Landed today (each with its own full gate, A/B and evidence note)

| commit | lever | this box |
|---|---|---|
| `7ab7e68` | (b) zero-copy big writes: the overlay class HELD in its slot, `WRITE_FIXED(device ← slot)`; `SQUEEZEFS_ZC_HOLD_OVERLAY` | seq write ×4 14.7 → 27.5 GB/s, io-wq 2.5 → 0.35 cores |
| `f410930` `3b14b6d` | (a) fiber-aware device waits (decision 0001 amended): a DMA parks only its fiber | 1M ×1 1.80×; in-flight DMAs 3.5 → 9.6 |
| `451bab6` | the `data_alloc_population_drift` check race (count, popcount and trim window read apart) | 4005/4069 racing checks fired → 0 |
| `b8d3b61` `1f98985` `b1d6333` | (c) the publish off the heap: `merge_scratch` slab pool (the budget's), in-place staging, merge-walk diff; the save 64 → 213 blocks | seq write ×4 27.4 → 47.1 GB/s; publishes/block 0.585 → 0.086; meta bytes/block 17.2 → 2.3 KiB |
| `f17a364`…`8c1aa86` | lever 4: a HELD write dispatches to the worker's own lane; `SQUEEZEFS_FUSE_WRITE_OWN_DEPTH` = 8 (ruling: all-own regressed QD32×1 0.76×; 0 once lever 5 lands) | 4k randwrite QD1 2.06× (p50 18.8 → 9.8 µs); QD32×4 1.03× |
| `2f7878a` `c28babc` `bdfb316` | FAILED volume: the drain warns once; `umount` finds a live daemon by process; the busy census reads `/proc/<pid>/maps` to EOF (mmap holders were invisible) | — |

Final head over the full matrix (`wave1-final-r1`, in `rung-10-perf-baseline-workstation.md`):
**seq write 1M ×4 46.6 GB/s (3.2×, 79 % of raw), ×1 24.5 GB/s (4.1×); 4k randwrite QD1 84.9 k
(1.48×), QD32×4 427 k (1.08×); 4k randread QD32×4 853 k (1.08×); reads unchanged.** Write and read
bandwidth bars cleared on the RAM substrate; write IOPS at 59 % of its bar is the open gap.

## Rulings (orchestrator, under the owner's "performance first; a regression is a defect")

- `SQUEEZEFS_FUSE_WRITE_OWN_DEPTH` ships at 8; all-own (0) when lever 5 removes the one-queue
  saturation. `SQUEEZEFS_FABRIC_IOPOLICY` round-robin (10-18). Pools register with the memory
  budget (the publish and transform scratch pools do now; the zstd codec pool is owed).

## Next levers (in order; the profiles are in the evidence notes)

1. Lever 5: N workers per kernel queue (unlocks depth 0; the 4k write IOPS gap).
2. The journal lane's `Ticket.wait` from a fiber on another lane (handler-lane fsync) through the
   fiber-aware route (`waitIo`/`completeIo`, the `JournalLane` given the route).
3. Fresh-file writes on the rewrite epoch / §5.6 publish coalescing and layout deltas (the lay-down's
   12–13 GB/s); the 7.8 % save-in-flight fallback.
4. The read side's per-request transport cost (4.8 µs per 4k read, `sqz-fuse-ur` alone).

## Method notes for the next wave

- `scripts/perf/quiet.sh <session> <label>` holds the whole venue for a session; A legs from the
  main tree at `dev`, B legs from the worktree; A B B A; the 4k rows need ≥ 4 legs per side here.
- Owed from the wave: the zstd `CodecPool` unregistered with the budget; extracted/gate-declined
  writes still run on the CPU's lane (a follow-up A/B now that device waits are fiber-aware); the
  three per-publish copies on the journal lane and the `KvTx` box are still heap-per-op (9× rarer).
- Owner's inventory for the 200 G venue is still pending (see the 10-18 note).
