# 2026-09-06 — finding 15's root cause: the co-writer free path did not close its supply

| | |
|---|---|
| **Branch** | `fix/cowriter-free-refcount-leak` off `dev` ac717c7f |
| **Commits** | `a0102b5f` (red contracts, `tests/mw_cowriter_free_leak_tests.rs`) · `1bed978d` (fix) · docs commit |
| **Evidence** | `.benchmarks/rows-wedgefix-s11-20260906/` (m0 + m50–m57 `.log` / `.stats.json`) and `rows-d4-s11-20260905/`; the notes `2026-09-06-cowriter-enospc-wedge.md` §6–7 and `2026-09-05-d4-free-grace-sustain.md` §6 |
| **Class** | data-path correctness — a lane-supply LEAK plus a duplicate-free storm on the S9 co-writer FREE path; finding 15 (the s11 lane exhaustion) |
| **Fleet row** | **OWED (parent)** — see §6 |

## 1. What the fleet numbers say (before any code was read)

The wedge note's hypothesis was: a HARVESTED offset takes its reference
through the co-writer's shipped publish, the authority's RAM map never
learns it, the later shipped free finds "no refcount entry", is refused,
and the block leaks — "3,449 refusals ≈ 13.5 GiB per phase". The row's own
ledgers falsify the arithmetic and point elsewhere:

| quantity | value | source |
|---|---|---|
| `block_untracked_free_refusals` (authority m0) | **3,449** | m0.stats |
| Σ co-writers' "the authority refused N shipped free(s)" | **3,448** | m50–m57 logs |
| Σ co-writers' `free_shipped_blocks` | 7,362 | m50–m57 stats |
| `free_served_blocks` (accepted, m0) | 3,914 | m0.stats |
| → shipped ≡ served + refused | 7,362 = 3,914 + 3,448 ✓ | |
| `free_recomputed_blocks` (m0 freed on its own recomputed publishes) | 46,466 | m0.stats |
| `harvest_served_blocks` | 46,386 | m0.stats |
| `del_obj` = `free_grace_deferrals` (every terminal free on m0) | 54,530 | m0.stats |
| Σ allocations (8,192 fresh + 45,092 from-freelist on co-writers; 574 + 3,584 on m0) | 57,442 | all stats |
| → live at capture = allocations − frees | 2,912 vs 2,560 file blocks + 43 graced + ≈2 blobs | ≈ **300 blocks unaccounted** |
| Σ `publish_blob_orphan_reclaims` (co-writers) | 4,119 | m50–m57 |
| Σ `publish_full_save_indirect` (own-mint map blobs minted) | ≈ 600 | m50–m57 |
| refused ÷ orphan_reclaims, per co-writer | 0.84 / 0.82 / 0.84 / 0.86 / 0.84 / 0.83 / 0.82 / 0.84 | m50…m57 |
| distinct refused offsets; max refusals of one offset | 2,089; 8 | m0.log |

So: (a) every refusal is a shipped free the authority answered `Refused`
in its already-free/graced class — the refused offsets **re-enter the
supply** (they are refused again up to 8× after being re-harvested), so the
refusals are not the leak; (b) they are almost exactly the co-writers'
own-mint **map-blob orphan reclaims**, ≈ 7 per own-mint save, one accepted
and the rest refused; (c) the accepted data-block frees close (3,914 ≈
4,119 blob firsts + the non-recomputed publishes' displaced blocks); (d)
the actual leak is ≈ 300 blocks per row — too small to be the whole
shortage on its own, but a supply that shrinks per rewrite on a fleet that
never remounts. The rest of the shortage is the free-grace release latency
× churn product the D-4 note owns (`free_grace_bound_age_ms` 17 s at 560
displaced blocks/s against 5,632 recyclable lane blocks), which this
campaign does not touch.

## 2. The in-process diagnosis (`tests/mw_cowriter_free_leak_tests.rs`, red against ac717c7f)

The harness: one authority (custody + publish + a live data plane with the
free/harvest executors, the rung-19 refs resolver, the §9.2 range geometry
and the rung-20 indirect-map hook — the production arm's installs) and one
co-writer driving a REAL `SqueezefsFilesystem` over its own lane-engaged
data plane under range custody (`TEST_RANGE_CUSTODY_OVERRIDE`), rewriting
blocks 2..6 of a shared striped file 10 rounds with `fsync` per round on a
lane of 9–17 blocks, so every round past the second runs on harvested
offsets.

**The wedge hypothesis does not reproduce.** The router-level cycle harvest
→ publish (full `set_layout_and_size` Put AND the Lever-B merge conveyor) →
displace → shipped free is accepted every round, the block re-enters the
lane and is harvested again (`a_harvested_offset_published_then_displaced_
frees_and_is_reharvested`, 4 cycles); the plain whole-block rewrite loop
closes exactly (`a_range_custody_rewrite_loop_…`: 40 displaced, 40 freed
by the recompute, 0 shipped, 0 refused, supply-after + live == supply-before).

**Three defects did reproduce:**

1. **The LEAK — a RAM-only lifetime under a recomputed publish**
   (`a_same_epoch_rerewrite_loop_…`, each block overwritten twice per
   round). The first overwrite's overlay destination is FED to the rewrite
   epoch (a RAM-only binding, its take NOTED for a later save); the second
   overwrite finds the block shadow-bound (`overlay_ineligible_shadow_bound`),
   rides write-through, and its durable merge displaces the RAM-only dest.
   The authority recomputes the frame as the head→composed diff
   (`recompute_refs_against_map`), which cannot name a block NEITHER map
   ever held; the reply says `recomputed`, the co-writer's frame-derived
   free stands down (`retire_displaced_locally` — local hygiene only), and
   the block is freed by nobody. Red: round 0 mints 8 lane blocks, frees 4
   (`recomputed 4`), leaves 4 off every free list and out of every layout;
   the lane (9 blocks) hits `StorageFull` at round 1's fsync. The field's
   face: `rewrite_shadow_superseded` 2,216 fleet-wide, of which the
   not-yet-persisted subset is the ≈ 300-block residue.

2. **The REFUSAL STORM — an own-mint map blob reclaimed twice, then again**
   (`an_own_mint_blob_lineage_closes_exactly_once`). A range-episode
   co-writer's save that displaces its OWN map blob frees it at the tail
   (`old_indirect_to_free`) AND its republish runs the layout-entry insert
   chokepoint's orphan reclaim (finding 35c: old entry own-mint, new entry
   names a different blob) — two issuers of one free. Worse, a merge that
   cloned the entry before a lock-free release-hook `discard_layout_cache`
   re-publishes the clone (own_mint still set) afterwards, and every later
   discard reclaims the same blob again. Each duplicate ships, the authority
   finds the offset already free/graced and refuses on the untracked
   tripwire — the 3,448. When the offset was re-harvested and re-minted in
   between, the duplicate names a LIVE lifetime (the CLAIM ANOMALY mirror;
   the executor's shields cover only authority-tracked offsets).

3. **The QUIET ABANDON on a live co-writer**
   (`a_never_published_mint_on_a_live_co_writer_recycles_…`). A superseded
   overlay destination / failed-publish upload on a healthy laned co-writer
   went through `abandon_unpublished_offset` → "left to the next
   derivation" (`cowriter_unpublished_abandons`, 92 on the row) — a
   remount-only recovery on a fleet that never remounts.

Plus one venue/partial-authority fault the indirect loop convicted
(`an_indirect_map_rewrite_loop_…`, a 640-block file whose map spills to a
blob): the served compose's `free_after_commit` blob free ran OUTSIDE the
authority-accounting scope, so a process whose posture latch reads
co-writer SHIPPED its own blob's free (refused on the live-free shield,
`block_live_free_refusals` +1 per round). In production the set authority's
posture is `writer`; a PARTIAL authority's latch reads co-writer, so its
own compose blobs were shipping to the set authority.

## 3. The fix (`1bed978d`)

* **RAM-only lifetimes ride the recompute's free set.**
  `block_refs::frame_ram_only_candidates(caller, recomputed)` names the
  frame's net-zero data lifetimes (≥ 1 take, takes == releases, not named
  by the recomputed ops; map-blob custody excluded). Every recompute site —
  the aggregated pass (compose + inline arms), `merge_layout_and_size_direct`
  (both arms), `custody_scoped_layout` (the full-Put compose) and the
  authority-local `compose_episode_save` — filters the candidates against
  the head AND the composed map (a skewed frame can re-take a durable block
  whose release the diff already carries; the head's keys resolve only when
  a candidate exists) and appends the survivors to the released set
  `free_recomputed_releases` runs. They are staged NOWHERE (no record ever
  existed) — one tx = one entry unchanged, the delta still rides the layout
  publish. `retire_displaced_locally` skips a frame key whose lifetime
  stamp is dead (the offset re-minted since the authority freed it).
* **A blob lineage closes exactly once.** `DataRouterInner::reclaimed_own_mints`
  (ino → the closed own-mint key; one slot, since every get→insert pair and
  the successor-minting save run under `INODE_META_LOCKS`): the save tail
  records the predecessor before republishing, `reclaim_own_mint_blob`
  declines a closed lineage (`publish_blob_orphan_reclaim_dedups`), and
  `publish_layout_cache_entry` clears a re-inserted clone's `own_mint` bit
  when it names the closed lineage.
* **A live laned co-writer RECYCLES a never-published mint** into its own
  lane free list (`cowriter_unpublished_recycles`) — the same act
  `adopt_lane_free_grant` performs for a harvested offset; nothing durable
  ever named the block. The abandon stays for poisoned eras and foreign
  lanes.
* The served compose's displaced-blob free runs under
  `with_authority_accounting` (the hook's `free` in
  `multi_writer::indirect_map_io_for`).
* **Instrument:** the executor's already-free refusal names its class on
  the owner's log line (free-listed / graced / quarantined / in-flight);
  `meta_ship_publish.free_refused_blocks` counts `Refused` verdicts so
  `served + refused + non-terminal ≡ the peers' shipped` closes on the wire.

Kept: one tx = one journal entry; the ledger delta rides the layout
publish; exactly-once frees under the dedup window; the grace/quarantine
composition on the authority's ladder; genuine double releases stay refused
(`a_genuine_double_release_is_refused_counted_and_named`).

## 4. Contracts

`tests/mw_cowriter_free_leak_tests.rs` (8):

| contract | shape |
|---|---|
| `a_range_custody_rewrite_loop_frees_every_displaced_block_exactly_once` | FUSE-level, 10 rounds × 4 whole-block overwrites + fsync under range custody on a 17-block lane: recomputed == displaced (40), shipped 0, untracked 0, ENOSPC 0, `supply_after + live == supply_before` |
| `a_same_epoch_rerewrite_loop_…` | the same, every block overwritten twice per round (the epoch-fed dest displaced by write-through): 80 displaced, 80 freed by the recompute, 10 recycles, closure exact — RED on dev (StorageFull at round 1) |
| `an_indirect_map_rewrite_loop_…` | 640-block indirect map: 40 recomputed, 10 shipped = 10 own-mint blob reclaims, all accepted, 0 refused — RED on dev (`block_live_free_refusals` 9) |
| `a_harvested_offset_published_then_displaced_frees_and_is_reharvested` | router-level: harvest → publish (full Put / Lever-B alternating) → displace → freed exactly once → re-harvested, 4 cycles, ledger populations exact |
| `an_own_mint_blob_lineage_closes_exactly_once` | the collapsing save ships ONE free for its own-mint predecessor (dedups +1), a re-inserted stale clone + discard ships none, `free_refused_blocks` +0 — RED on dev |
| `a_genuine_double_release_is_refused_counted_and_named` | a second free of a freed block: `Refused`, `free_refused_blocks` +1, tripwire +1, the block free exactly once |
| `a_never_published_mint_on_a_live_co_writer_recycles_into_its_own_lane` | recycle +1 / abandon +0, the funnel serves it back; a poisoned era abandons — RED on dev |
| `frame_ram_only_candidates_names_exactly_the_net_zero_unnamed_lifetimes` | the pure law |

The two staged-cleanup contracts in `mw_cowriter_free_tests` moved to the
recycle law (recycles +1, abandons +0). Every other contract listed in the
task is unchanged and green.

## 5. Gate (this side)

* `cargo fmt --check` clean.
* `cargo clippy --all-targets --all-features -- -D warnings` exit 0;
  `cargo clippy --all-targets -- -D warnings` exit 0.
* Suites, `--all-features -- --test-threads=1`: `mw_cowriter_free_tests`
  49/49 · `mw_cowriter_lane_tests` 26/26 · `mw_data_alloc_lane_tests` 23/23
  · `dlm_cowriter_tests` 18/18 · `cowriter_enospc_wedge_tests` 9/9 ·
  `durable_block_refs_tests` 17/17 · `reader_free_grace_tests` 39/39 ·
  `pv_shipped_free_ledger_tests` 2/2 · `mw_widthn_refs_tests` 15/15 ·
  `mw_cowriter_free_leak_tests` 8/8 ×20 (see §5a). The fsck C8 contracts
  live in `durable_block_refs_tests` (there is no `fsck_c8_tests` binary).
* No `task check`, no root rigs (the parent's).

### 5a. Adjacent suites and the ×20

* `mw_cowriter_free_leak_tests` **8/8 ×20 consecutive** (15.4–16.1 s per run).
* Adjacent, `--all-features -- --test-threads=1`: `rewrite_shadow_supersede_tests`
  3/3 · `rewrite_shadow_tests` 7/7 · `overlay_overwrite_tests` 32/32 ·
  `indirect_map_backend_keys_tests` 10/10 · `mw_ranged_lease_ladder_tests`
  15/15 · `dlm_range_custody_tests` 41/41 · `mw_layout_version_tests` 11/11 ·
  `fsck_tests` 22/22 · `kvmap_mw_hazard_tests` 7/7 · `kvmap_crossing_tests`
  9/9 · `write_through_coverage_tests` 8/8.

## 6. Fleet row — RUN 2026-09-06 00:15 (dev box, 32 CPUs): the leak and the storm are GONE; the shortage is now the recycle loop's own term

`s11-mpiio` from zero on `34823f0d` (release), same fleet (1 authority +
8 co-writers, range custody, `OSS_GB=32`); artifacts
`.benchmarks/rows-leakfix-s11-20260906/`. Probe 1,606 MiB/s → 10 GiB file,
12 iterations.

| | before the fix (`4ca040aa`, `rows-wedgefix-s11-20260906/`) | after (`34823f0d`) |
|---|---|---|
| `block_untracked_free_refusals` (m0) | 3,449 | **148** (−96 %) — `meta_ship_publish.free_refused_blocks` 148, the same number; `free_served_blocks` 2,686 |
| double-release refusals per co-writer (log) | 367–542 | **15–20** |
| `publish_blob_orphan_reclaims` / `_dedups` per co-writer | one blob re-armed at every discard | 65–85 reclaims ≈ `publish_full_save_indirect` 30–77 (one closure per own-mint save), **256–321 dedups** carrying the re-arms that used to ship |
| `cowriter.unpublished_{recycles,abandons}` | abandons | **recycles 5–10, abandons 0** |
| `CLAIM ANOMALY` per co-writer | ×34 (m57) | 1 / 183 / 33 on m50 / m53 / m57 — NOT 0 (below) |
| lane ENOSPC refusals per co-writer | 114–225 | **76–129** — the shortage PERSISTS |
| watchdog reports | 2–4 | 0–2 |
| ior | all 18 iterations ran; A1 NOT SUSTAINED 920 → 557 MiB/s | all 12 ran; **A1 NOT SUSTAINED 1,270 → 482 MiB/s** (iterations 1,860 / 1,932 / 914 / 965 / 554 / 1,580 / 790 / 250 / 776 / 584 / 263 / 600) |
| free-grace at capture (m0) | deferrals 54,530 / releases 54,487 / **offsets 43** | deferrals 35,594 / releases 32,423 / **offsets 3,171 (≈ 12.4 GiB)**, `alloc_stalls` 0, `bound_tightenings` 185,054, `demand_waits` 49,528 |

**Verdict — the fix does what it claims; finding 15 is not closed by it,
and the row now says exactly why.** The refused-free storm is gone (−96 %,
the residue named as genuine double releases), never-published mints
recycle instead of abandoning, the blob lineages close once. With the
leak plugged the ledger moves: the free-grace ring is now holding
**3,171 offsets (a third of the 36 GiB lane supply) at capture** where it
held 43 before — i.e. the supply is no longer leaking OUT of the loop, it
is sitting IN the loop waiting to be released. That is D-4's rate term
(release latency × churn against `cap/W`), the ack-cadence campaign's
row, not this fix's — with the PR-1 instruments live: `bound_tightenings`
185 k and `demand_waits` 49.5 k over a 3.5-minute phase say the valve is
saturated and still cannot release fast enough for 1.6 GiB/s of CoW
displacement into 4 GiB lanes. `CLAIM ANOMALY` did not reach 0 (183 on
m53) — the residual 148 refusals' co-writer-side face; a smaller,
separate item to name (which lineage still double-ships) before the next
fleet row.

**Disposition:** LANDS (a data-path correctness fix with a fleet-proven
effect on its own ledger). Finding 15 proper moves to the free-grace
release-latency campaign with a sharper premise: the loop's hold time,
not a leak, and not the refused frees.

## 7. Follow-on — the `CLAIM ANOMALY` residue was NOT the refusals' face

`.benchmarks/2026-09-06-cowriter-free-residual-lineage.md` (term 3): the
anomalies correlate one-to-one with `rewrite_shadow_fence_drops` (one
fenced epoch close per carrier → 200–323 anomalies; none → 0) and barely
intersect the refused offsets. The shape: a co-writer's fsync whose token
sibling stripe grants superseded ran its flush leg under the CURRENT
generation (the covering publish the authority recomputed) and then its
epoch close under the STALE one — the W5 arm dropped the parked keys'
local hygiene and the uncovered acked bindings. Fixed by applying the
2026-08-06 tail-loss law to the close (it re-presents the current
generation; W5 is the genuine fence class only). The refused frees
(148 → 163) are a separate residue, instrumented there as
`cowriter.free_ship_own_lane_untracked`.
