# Read-path PR 3 — hot-block RAM tier + unified four-tier purge (R4): gate evidence

**Design:** `docs/design-read-path.md` §5.4 / PR 3 entry. **Commits:** `0afbd89` (red
contracts) + `ee7f1d1` (implementation). **Base:** dev @ `77cfb4f`. Admission is untouched
(every >256 KiB validated fill still publishes to the NVMe tier — PR 4); the hot tier adds
a `Bytes`-refcount RAM landing zone beside it, plus the structural purge unification.

## Mechanism landed (summary)

Clock shard grows the sticky `protected` bit beside the consumable `referenced` bit;
`put_probationary` (scan-resistant insert); evictions carry `(key, value, EvictClass)`;
dehydration channel typed **behavior-neutral** (gate flip = PR 4; ≤256 KiB `read_lru`
dehydration bit-identical, worker + key-filter untouched). `TieredCache.hot_block`
(`SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB`, default `max(8 MiB, read_mem_limit/4)`, 0
short-circuits). `purge_block_key` = the only legal block-key purge over ALL FOUR tiers
(read_lru, hot_block, NVMe, GDS `.gds_cache`) — census converted (routing displaced/prune/
free sites, `upload_full_block`, `flush_one_active_block`, the `read_tier_purge` free-side
callback), grep-guard test enforces it; publisher-local single-tier undos stay local by
design (documented in the guard's whitelist). GDS filename construction unified through
`get_gds_path` (`read_direct`'s divergent inline sanitizer deleted — prefixed
`be_id://offset` keys previously produced two names and the purge missed the served copy);
no migration, mount-time wipe suffix-matches both schemes (forward-only directive). Probe
order: overlay → hot → read_lru → NVMe → device; hot fast-path hits carry the same binding
recheck as NVMe-tier hits; fills are device-validated only; NVMe hits never re-promote.

## Red-test list (tests/hot_block_tier_tests.rs, 11 green post-impl; + churn/stale suites)

probation-before-protected eviction order · sticky promotion classes the eventual victim
Protected · typed-channel behavior-neutral pin (probation victims visible AND still
dehydrate-eligible) · validated-fills-land-hot + zero-device re-read + no-repromote ·
displaced-key hot purge + new-bytes serve · adversarial stale hot entry under a dead key
unreachable · four-tier GDS purge incl. prefixed keys · census grep-guard · GDS
construction unification guard · cache-less volume gains the tier · budget-0 short-circuit.

## Perf (same-session A/B, base 77cfb4f vs PR 3, attribution protocol, 8 GiB cage)

| Row | PR 1 lineage | Base (tonight) | PR 3 | Verdict |
|---|---|---|---|---|
| warm-re-read (slice A 2 GiB) | 16,588–16,915 MiB/s | 16,406 / 16,978 | **17,628 / 17,406** | **improves (+4–6%)**, `hot_block_hits` +368 ✓ |
| **interleaved-scan variant** (3 iterations vs concurrent 16 GiB cold scan) | 9,098→9,341→**758** (collapse) | 10,283→**1,565**→8,490 | **12,084 → 9,957 → 11,687 — no collapse; device reads 0 vs 496 MiB (base)** | recorded (PR 4 lineage); the hot tier absorbs scan pollution outright |
| rand-4k warm (2 GiB set, 8 G cage) | — | 124,104 / 124,493 IOPS | 122,497 / 121,094 IOPS | holds (−1.9%, within noise): at zero memory pressure the tier mmap is RAM-resident and there is nothing to win — the doc's honest-magnitude clause |
| row 1 fresh create | 3817–4282 | 4099 | 3557 † / 3760 † / **4214** (quiet) | flat ✓ († = loadavg-residue samples, 4.2–6.0 at start; quiet sample in-band) |
| row 4 overwrite | 800–869 | 686 | 712 / 649 | flat vs same-session base (±4%); both binaries below the PR 1 band tonight equally (substrate-state drift) |
| row 5 rand-4k write | 63–108 | 137 | 49 / 100 | state-noisy in every session to date; same-session spreads overlap |

Counters: warmhit `hot_block_hits` +368; `hot_block_current_bytes` ≤ budget throughout;
`probation_drops` = 0 (gate not flipped, as designed); churn suite `get_obj` assertions
byte-identical green.

## Extra-diligence probe (beyond the doc gate): 3 GiB cage oversubscription — **pre-R5 regression, recorded loudly**

Synthetic squeeze (NOT a doc gate row): `MemoryMax=3G` against 1G+1G RAM LRUs + 5 GiB tier
mmap + 256 MiB hot budget, warm rand-4k over a 2 GiB set:

| | Base | PR 3 |
|---|---|---|
| warm-re-read | 4,963 MiB/s (1.9 GiB re-faults) | 5,064 MiB/s (2.1 GiB) — parity |
| rand-4k warm | 36,427 IOPS (83 GiB faults) | **4,485 IOPS (105 GiB)** — **8× regression** |

Reading: in an already-thrashing cage, the hot tier's anon footprint (+ probation churn on
random misses) displaces tier-mmap residency past the thrash knee. This is exactly the
memory-oversubscription regime the design assigns to **PR 7 (R5 joint budget)** — no
per-component cap can see the sum. Mitigations today: `SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB=0`
(restores pre-PR 3 behavior verbatim, pinned by the budget-0 test). **PR 7 must include
this 3 G-cage rand-4k-warm row in its gate** (this note is the lineage).

## Gates

- Cargo gate: clippy `-D warnings` clean, fmt clean, doc 0 warnings, bench smoke ok; full
  serial test run **61/61 suites green** on re-run — one iteration hit
  `crash_kill_tests::test_kill9_remount_soak_v3` ("clean shutdown must leave an empty
  replay window", KV checkpoint-cadence timing in an untouched subsystem); 8×
  standalone + full-run repeat all green — timing one-off, recorded.
- 074-family: `hot_block_tier_tests` 11/11, `reused_key_stale_fill_tests` 6/6,
  `staged_identity_visibility_tests` 7/7, churn suite 2/2 (A–H byte-identical).
- fstests QUICK: failures {003, 213} (documented platform expected-fail) **+ 618 as a
  cascade of the pre-existing 616-soak cage-OOM** — decisively attributed: pristine
  dev@77cfb4f fails the identical {003, 213, 618} set the same night with the same OOM
  signature, and the OOM fires equally with the hot tier disabled
  (`SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB=0` run); generic/618 standalone on PR 3: 3/3 pass.
  Trio + 074 (generic/074, 075, 091, 616): pass.
- LTP: 174 PASS / 0 FAIL / 9 SKIPPED.
- loom: not required — the sticky bit is a single-word Relaxed atomic beside the existing
  clock bit, no cross-word invariant (documented on `EntryState`); the loom-models scope
  (allocator bitmap, incarnation seqlock, budget gauge, CoW cell) is untouched.

## Rails

Quiet-gate per timed run (loadavg-residue samples annotated, quiet re-samples taken);
Tctl 48–70 °C; 3.5 GHz cap untouched; daemons caged; builds taskset 0-15 / JOBS=12;
`/mnt/squeezefs` untouched; nothing pushed.
