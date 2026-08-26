# Finding 17 — the read-tier deposit window serves a dead incarnation (2026-08-26)

**Class:** read-after-ACK wrong-data, load-selected (the "load selects such
schedules; it never causes them" law — first-class product bug).
**Found by:** the finding-16 carrier fix's full `task check` gate —
`test_aligned_overwrite_supersedes_dirty_active_block`
(`tests/data_path_correctness_tests.rs`) failed with the read returning the
PRE-overwrite image after the overwrite's ACK. **Pre-existing on dev**
(reproduced at `0afabcff` without the carrier commits, run 1/20), measured
~5 %/run on the whole binary at capped clocks.

## The conviction (counter + tier census, instrumented worktree loop)

Per-step ledger of the failing schedule (every failing capture identical):

| Window | Deltas | Reading |
|---|---|---|
| partial write | `patch_ineligible_unaligned +1`, `extent_parks +1`, `parked_extent_bytes +1024` | the 1 KiB patch parks a W2 extent |
| read-back | `parked_extent_bytes −1024`, `fold_{passes,extents_folded,seed_reads} +1`, `read_tier_admissions +1`, `cache_hits +4`, `cache_misses +1` | the read triggers the fold (extent drained, base+patch composed and CoW-published) and its block-2 fill is ghost-admitted → the **deferred tier publish** is queued |
| aligned overwrite | `patch_writes +1` | W1 in-place patch: retire → DMA → publish → purge (predicate 2 correctly finds no overlay — the fold drained it) |
| failing read | `cache_hits +4`, everything else **0** | all four blocks served from tier-hit arms; block 2 = the stale base+patch image |
| post-mortem probes | read_lru/hot/hold/staging/nvme-read-cache ALL none; `block_fill_inflight` false; device@mapping = the overwrite | the evidence erased itself |

**The mechanism:** the PERF-11 deferred tier publish (and every fill
deposit) was **put-then-revalidate-then-undo** — correct for the FINAL
state, but the pre-check→put→undo window is **reader-visible**. A
blocking-pool backlog deschedules the closure between its pre-check and its
put; the W1 patch's whole retire→DMA→publish→purge lands inside that gap
(the purge finds nothing — the put has not happened); the put then deposits
the DEAD incarnation's bytes into the NVMe read cache; a read that begins
strictly after the patch's ACK serves them as a trusted tier hit ("entries
always hold current-incarnation bytes" — the invariant the window breaks
transiently); the closure's own undo check then removes the entry — which
is why the flake healed on the very next read and every post-mortem probe
came back empty. The same put-then-undo shape existed at every fill deposit
site: the demand fill's hot/read-lru/hold RAM puts, the small-block inline
nvme put, the hold-serve admission's deferred nvme put and hot landing, the
lane fill's hold deposit, and dehydration's `cache_read_block_validated`.

## The fix — validation at the insert's visibility point

Every fill deposit now validates **atomically at insert**, under the tier's
own insert lock (the entry's visibility point), so an entry that would fail
the incarnation still-check can never be observed by a reader:

- `NvmeShard::put_impl` runs the validation in phase 2 (the index insert)
  under the shard write lock — the same lock every purge/remove takes, so
  an insert that validated against the pre-retire word completes before any
  post-retire purge can run, and one that runs after the retire refuses.
  A refusal drops the reservation and zeroes the header magic.
- `MemoryCacheShard::put` validates under the scc bucket writer lock
  (occupied-and-invalid also REMOVES the existing entry — absence is always
  correctness-safe for a read cache); exposed as
  `LruCache::put_class_validated` (PutClass made pub — one entry point
  instead of four validated wrappers; the dead bare wrappers deleted).
- `ReadLaneHold::insert{,_demand}_validated` — same bucket-lock validation.
- The deferred publish closure and the hold-serve admission use
  `cache_read_block_if`; their undo legs are deleted (post-insert movement
  is owned by the mover's purge, which every retire path already runs after
  publish). The fill's final still-check stays — it is the serve-validity
  verdict's source — and its failure-path unified purge stays.

**Deterministic repro (the repro-port mandate):**
`patched_block_never_serves_a_mid_publish_tier_deposit`
(`tests/write_visibility_tests.rs`) — two new seams
(`TEST_TIER_PUBLISH_MID_WINDOW_STALL_MS`, `TEST_TIER_PUBLISH_POST_PUT_STALL_MS`)
hold the closure's window open across a W1 patch; RED pre-fix ("OLD BYTE
0x22 after the patch's ACK"), GREEN post-fix with the generic/209 sibling
(`patched_block_never_serves_a_held_flight_snapshot`) untouched.

## Acceptance

- Deterministic pin: RED pre-fix, GREEN post-fix (above).
- Directly-affected tier suites green (hot_block_tier, memory_shard_tombstone,
  read_stream_transient, read_tier_refetch_churn, read_admission_governor,
  hybrid_io, read_lane).
- Statistical: the whole `data_path_correctness_tests` binary, serial,
  **100 consecutive clean runs from zero post-fix** (pre-fix rate ~5 %/run,
  so P(0/100 unfixed) ≈ 0.006; counted-run law — the pre-fix captures were
  declared rate/signature gathering only, restarted from zero after the
  fix).
- Full `task check` gate green (clock-capped, thermal tripwire).
