# 2026-09-06 — finding 15 term 3: the co-writers' residual `CLAIM ANOMALY` lineage is a fenced epoch close

| | |
|---|---|
| **Branch** | `fix/cowriter-free-residual-lineage` off `dev` c1450c66 |
| **Commits** | `b5b0b112` (red contracts + instruments) · `3ad983bb` (fix) · `38265fef` (the OW-6 crash case on the genuine fence class) · docs commit |
| **Evidence** | `.benchmarks/rows-holdtime-s11-20260906/` (m0 + m50–m57 `.log` / `.stats.json`, `rows/*/A1.out`) and the previous row `rows-leakfix-s11-20260906/`; the notes `2026-09-06-cowriter-free-refcount-leak.md` §6–7 and `2026-09-06-free-grace-hold-time.md` §6 (term 3) |
| **Class** | data-path correctness — a LIVE co-writer's rewrite epoch fenced by its own lease rotation: the parked keys' local hygiene dropped (the `CLAIM ANOMALY` lineage), the uncovered acked bindings discarded, the fsync an EIO |
| **Fleet row** | **OWED (parent)** — see §7 |

## 1. What the evidence says (read before any code)

The hold-time note left three terms; this is the third: six of eight
co-writers still logged `CLAIM ANOMALY … refcount entry lingers` after the
leak fix (0 / 7 / 323 / 205 / 52 / 3 / 323 / 0 on m50…m57), the authority
refused 163 shipped frees (`block_untracked_free_refusals`), and the two
clean co-writers' lane ENOSPC fell 60–80 % under the hold-time levers
while the six carriers did not move. The task's working hypothesis tied
the anomalies to the refusals and to two custody holders straddling one
block. The row's own numbers say otherwise:

| question | answer | source |
|---|---|---|
| can two ranks straddle one block? | **No.** ior `-a POSIX -b 4m -t 4m -s 80`, 32 tasks, rank `r` → mount `m5(r/4)`: transfer = block = 4 MiB, block `b` is written by exactly rank `b % 32`, every write block-aligned | `rows/*/A1.out`, `A1.dispatch.sh` |
| whose lane are the anomaly offsets in? | **the logging co-writer's own** — 100 % (m51: 7/7 lane 5; m52: 235/235 lane 2; m53: 169/169 lane 3; m54: 46/46 lane 7; m55: 3/3 lane 4; m56: 303/303 lane 1) | `CLAIM ANOMALY: offset N`, `N/4 MiB mod 16` vs `lane K of 16` |
| do the anomaly offsets match the refused offsets? | **barely** — 0 / 5 / 6 / 2 / 0 / 11 of the 160 distinct refused offsets (random overlap on a 512-block lane) | m0 `begin_free REFUSED untracked offset` ∩ each co-writer's anomaly set |
| whose lane are the REFUSED offsets in? | the shipper's own, and evenly: lane 8 (m50) 30, 5 (m51) 17, 2 (m52) 15, 3 (m53) 21, 7 (m54) 21, 4 (m55) 22, 1 (m56) 18, 6 (m57) 19 — identical to each co-writer's "the authority refused N shipped free(s)" count | m0.log by lane |
| do refusals produce anomalies? | **No.** m50 has the MOST refusals (30, spread 13:57 → 14:02, so every refused offset had time to be re-harvested) and ZERO anomalies. A refused shipped free names an offset this mount had already released locally (no lingering entry) | m50.log |
| refusal reason classes | 156 "held in the freed-offset grace ring", 7 "already on the free list" — every one a block the authority freed by RECOMPUTE seconds earlier | m0.log |
| **what does correlate with the anomalies?** | **`rewrite_shadow_fence_drops`.** m52 / m53 / m56: ONE `rewrite epoch for ino 2 FENCED at close` each → 322 / 204 / 323 anomalies AFTER it (1 / 1 / 0 before), the first burst 1–26 s after the fence; m50 / m57: no fence, 0 anomalies. The previous row says the same: m51 (4 fence drops) 319, m53 (1) 183, m57 (1) 33; m50/m52/m54/m56 (0) 1–2 | m5x.log, `rewrite_shadow_fence_drops` in m5x.stats |
| what precedes each fence? | a FUSE op refused `Lock expired or invalid fencing token: T (expected >= T+8…T+11)` 11–15 s earlier — a token superseded by a handful of newer grants | m52.log 13:57:47 → fence 13:57:58 |
| the residue without a fence | m54 52 (two bursts, each 10 s after a `Lock expired` EIO), m51 7, m55 3, m52/m53 1 each pre-fence — 64 of 913 (7 %) — a second, smaller shape (§6) | |

So the anomalies are NOT the refused frees' mirror and NOT a straddle.
They are the co-writer's local view going stale at ONE event: an epoch
close the mount fenced on its own superseded token.

## 2. The mechanism (`src/routing.rs` `close_rewrite_epoch`, `src/fuse_client.rs` `flush_inode_to_backend`)

Four ranks per co-writer write one ino. Each new stripe grant advances
the ino's local fencing generation (`dlm.get_fencing_token_ino`); a
rank's fsync acquires a lease whose token sibling grants then supersede,
and by the time the fsync reaches the swap that token is neither current
nor a live range grant. Inside that one `flush_inode_to_backend`:

1. the **flush leg** (`flush_memory_buffers_driven`, the fsync-durable
   partial-block upload — on the fleet the ENOSPC "falling back to
   staging" write-throughs, `displaced_free.count` 15–83 per co-writer)
   reads the ino's CURRENT generation *"never the caller's captured token
   (the 2026-08-06 tail-loss fix)"* and publishes. That durable merge
   carries the WHOLE dirty RAM map (a dirty map with deferred notes is
   full-save class by Vector B), i.e. every rewrite-epoch binding fed so
   far. The authority's custody-scoped compose adopts them, recomputes
   the head→composed diff, and frees the epoch's parked predecessors —
   the round-before mints A — through its own ladder (`free_recomputed_
   blocks` 38,775 on m0). The reply says `recomputed`; the co-writer
   retires only THIS merge's own displaced key. The A keys' local hygiene
   is deferred to the epoch close, as designed (`epoch.displaced`).
2. **`close_rewrite_epoch(ino, fencing_token)`** presents the fsync's
   STALE token verbatim; `save_metadata_to_backend_body` refuses
   `FencingTokenExpired` (nothing ships); the close takes the W5 arm —
   *"publish nothing, free nothing … acked un-fsynced rewrite bytes
   discard with the fenced era"* — pops `epoch.displaced` on the floor,
   `discard_layout_cache`, counts the fence drop, and the fsync returns
   EIO (ior: `WARNING: fsync(15) failed`).

Result, per parked A key the flush leg had covered: the authority holds
it free (grace → free list), the co-writer's `refcounts` map still says
`Some(1)`. The lane harvest hands it back to the same lane 8–26 s later
and `claim_block_idx`'s `insert_sync` fails: `CLAIM ANOMALY … count=Some(1)`,
once per re-claim (each offset re-claimed 1–3× on the row — the repeats
histogram). The stale entry happens to read 1, so the count self-heals
on the next displacement; the cost is the log storm plus what the SAME
arm did to the bindings the flush leg had NOT covered: dropped from RAM
while the durable map still names the previous iteration's blocks —
**acked bytes lost on a live mount, and their blocks gone from the lane
until a remount**, which is the headroom the six carriers never regained.
(The previous row's m55 shows the pure loss face: 50 fence drops, 3
anomalies — 50 epochs discarded with nothing covered.)

Why the two halves disagreed inside one fsync: the 2026-08-06 tail-loss
campaign ruled that within one process `FencingTokenExpired` from a
publish can only mean this daemon re-acquired the ino's lease between
token capture and revalidation — a ROTATION, never the cross-mount fence
(that is the D0 guard latch, `WriterGuardFenced`) — and made every
publish site re-present the current generation (`flush_one_active_block`,
`write_through_complete_block`, the overlay settle, `upload_active_block_
bytes`). The epoch close (built 2026-08-02/04 with W5 keyed on the stale
token as the stand-in for the D0 fence) was the one site the law never
reached. The design's own §5.4 says *"the swap's save presents the ino's
CURRENT DLM token"*; the fsync-driven close did not.

## 3. The in-process repro (`tests/mw_cowriter_free_leak_tests.rs` §1b, red against c1450c66)

The leak campaign's two-node harness (authority + one co-writer driving a
real `SqueezefsFilesystem` under range custody), the write-through
pipeline as the epoch's vehicle (quiesce makes the shadow records
deterministic; the overlay feeds the same epoch on the fleet):

* round 0: the co-writer rewrites blocks 2..6 of an 8-block shared file,
  fsync — its mints are now the durable predecessors;
* round 1: rewrite 2..6 (fed to the epoch, round-0 keys parked) plus a
  4 KiB partial write of block 6 (the flush leg's parked buffer);
* `flush_inode_to_backend(ino, stale)` with a token below the current
  generation that no live range grant carries (the fleet's dead-grant
  fsync token — found by walking down from the generation).

On c1450c66: `Err(FencingTokenExpired { token: …552, expected: …554 })`,
`rewrite_shadow_fence_drops` +1, `free_recomputed_blocks` +5 (the four
predecessors + block 6's seed — the authority freed them all), the
durable map names round 1 (the flush leg covered everything here), and
the co-writer's `refcount` for every round-0 mint reads `Some(1)` while
the authority lists it free at population 0. Draining the lane's fresh
supply until the funnel harvests those four offsets back re-claims them:
`block_claim_anomalies` +4 — the fleet's line, in-process.

The solo-harness twin (`tests/rewrite_shadow_tests.rs`): contract 4b — a
stale token with a newer generation in the process must CONVERGE (Ok,
swap persisted, A freed, the bytes read back); contract 4 re-pinned on the
GENUINE fence (the D0 custody poison → `WriterGuardFenced`, nothing
published, nothing freed, `rewrite_shadow_fence_drops` +1).

## 4. The fix (`3ad983bb`)

`close_rewrite_epoch`:

* **A `FencingTokenExpired` on the swap's save re-presents the ino's
  current generation and retries** (the tail-loss law; counted
  `rewrite_shadow_close_retries`). The read is monotone past every
  failed presentation, so an unconvergeable fence (fresh ≤ presented) is
  structurally unreachable and falls to the never-lossy transient arm
  (the epoch re-registers, nothing drops) — the idiom of every sibling
  site.
* **The W5 arm is the genuine fence class only**: the process-wide D0
  custody poison (`data_custody::poisoned`, probed before the save) or a
  `WriterGuardFenced` publish refusal (a dead custody era — which before
  this fell to the transient arm and re-registered the epoch forever on a
  dead mount). Publish nothing, free nothing, discard, count, loud —
  verbatim.

Kept: one save = one tx (the retry is a new presentation of the same
save); the parked frees run strictly after the durable save under the
recomputed/latched verdict; exactly-once frees; the untracked-free
tripwire's meaning (a genuine double release is still refused —
`a_genuine_double_release_is_refused_counted_and_named`); the block-refs
delta rides the publish tx; W5 verbatim for a fenced holder. Nothing in
`src/free_grace.rs`, `alloc_lane_grant.rs` or the custody renewal reply
was touched (the sibling branch's files); the shared-file hunks are
`src/fuse_client.rs` (three METRICS fields + JSON), `src/block_allocator.rs`
(the anomaly counter, `lane_is_ours` visibility), `src/cowriter.rs` (the
§6 instrument + JSON).

## 5. Contracts and gate (this side)

New / re-pinned (`--all-features -- --test-threads=1`):

| contract | shape |
|---|---|
| `mw_cowriter_free_leak_tests::a_rotated_fsync_token_converges_the_close_and_orphans_no_local_hygiene` | §3 end to end through `flush_inode_to_backend`: Ok, fence drops +0, close retries > 0, epoch closed, durable map = round 1, every predecessor freed once (`population` 0, authority free-listed) with its local tracking gone, the exact re-claim venue driven (`allocate_block` until all four harvest back) with `block_claim_anomalies` +0, six more rewrite rounds, untracked refusals +0, lane ENOSPC +0, `supply_after + live == supply_before`, every live block at population 1 — RED on dev |
| `mw_cowriter_free_leak_tests::a_genuine_double_release_is_refused_counted_and_named` | extended: the duplicate through the ROUTER seam counts `cowriter_free_ship_own_lane_untracked` +1 and the wire refuses it (+1) — the two faces of the §6 residue |
| `rewrite_shadow_tests::a_process_local_lease_rotation_converges_the_close` | contract 4b — RED on dev |
| `rewrite_shadow_tests::fenced_close_publishes_nothing_and_frees_nothing` | contract 4 on the genuine fence class (`data_custody::poison`) — RED on dev (the close answered `FencingTokenExpired`, not the fence class) |

Instruments landed (stats inode): **`block_claim_anomalies`** (the
`CLAIM ANOMALY` log line's counter — must stay 0; the fleet row reads it
instead of grepping), **`rewrite_shadow_close_retries`** (the rotation
shape's engagement — the fleet's ~1 per fenced-carrier becomes ~1 retry),
**`cowriter.free_ship_own_lane_untracked`** (§6).

* `cargo fmt --check` clean.
* `cargo clippy --all-targets --all-features -- -D warnings` exit 0;
  `cargo clippy --all-targets -- -D warnings` exit 0.
* Suites, `--all-features -- --test-threads=1`: see §5a.
* No `task check`, no root rigs (the parent's).

### 5a. Suites and the ×20

The four new/re-pinned contracts **20/20 consecutive** (the two leak-suite
contracts 3.4 s per run, the two shadow contracts 0.75 s).

Suites, `--all-features -- --test-threads=1`, all on `38265fef`:
`mw_cowriter_free_tests` 50/50 · `mw_cowriter_free_leak_tests` 9/9 (8 + §1b)
· `mw_cowriter_lane_tests` 26/26 · `dlm_cowriter_tests` 18/18 ·
`cowriter_enospc_wedge_tests` 9/9 · `durable_block_refs_tests` 17/17 (the
fsck C8 contracts live here — there is no `fsck_c8_tests` binary) ·
`mw_data_alloc_lane_tests` 23/23 · `rewrite_shadow_tests` 8/8 ·
`rewrite_shadow_supersede_tests` 3/3 · `fsync_writeback_tail_loss_tests`
3/3 · `overlay_overwrite_tests` 32/32 (the kill-9 matrix's OW-6 case
re-pinned on the genuine fence class, `38265fef`) · `overlay_ack_early_tests`
14/14 · `dlm_range_custody_tests` 41/41 · `mw_ranged_lease_ladder_tests`
15/15 · `fsck_tests` 22/22 · `write_pipeline_tests` 23/23 ·
`write_through_tests` 26/26 · `pv_shipped_free_ledger_tests` 2/2 ·
`mw_widthn_refs_tests` 15/15 · `reader_free_grace_tests` 48/48 ·
`attr_publish_tests` 7/7 · `fencing_remount_tests` 2/2 (+1 ignored) ·
`extent_overlay_tests` 14/14 · `mw_arbiter_fold_tests` 3/3 ·
`durability_matrix_tests` 12/12 (+2 ignored) · `meta_lock_free_hoist_tests`
2/2. `tests/check_markdown_links.sh`: 318 files, 0 broken.

## 6. What this does NOT close — the 163 refusals (the refused-free residue)

The refusals are a SEPARATE lineage from the anomalies (§1: no
co-occurrence, no offset overlap, and the most-refused co-writer has no
anomaly). Their shape: a co-writer ships a free for an offset in its OWN
lane that the authority freed by recompute seconds earlier (156 of 163
still in grace), and its own local tracking of that offset is already
gone (otherwise m50's 30 would have fired anomalies on re-harvest). That
is a SECOND displacement of a key this mount already released once — the
skewed-frame shape the recompute already filters when the publish is
recomputed (*"a skewed frame can re-take a durable block whose release
the diff already carries"*), reaching the WIRE only on the paths that
ship the caller's frame verbatim: a write-through merge answered
`recomputed = false` (`displaced_free.count` 15–83 per co-writer) or the
epoch close's clean arm when the ino's `publish_recomputed` latch —
membership-only, the LAST save's verdict — was flipped off by a later
non-recomputed save. The resurrection source is the co-writer's layout
refetch after a release-hook `discard_layout_cache` (finding 33) reading
a head up to the reader staleness bound old (`read_settle_stale_head_
refetches` 210 on m0). Not reproduced here; the instrument that confirms
it on the next row is landed: **`cowriter.free_ship_own_lane_untracked`
should ≈ each co-writer's refused count** (a foreign-lane predecessor is
untracked by construction and is not counted). If it does, the fix is
per-key provenance for the clean arm (stamp the covering save's verdict
on the EPOCH, not the ino) plus a stale-refetch guard; if it does not, the
lineage is elsewhere and the counter says so.

The 7 % anomaly residue without a fence (m54's two bursts 10 s after a
`Lock expired` EIO on a non-fsync op, m51's 7, m55's 3) is the same
clean-arm-latch shape's other face (a refused free leaves the entry
lingering) and rides the same instrument.

## 7. Fleet acceptance — RUN 2026-09-06 12:10: the fenced close is GONE; the residual lineage is now attributed

Same row as the lane-visible note §6 (`rows-t2t3-s11-20260906/`).

| gauge | pre (hold-time row) | this row | predicted |
|---|---|---|---|
| `rewrite_shadow_fence_drops` (m0) / `FENCED at close` lines (all 8) | 3 fences on m52/m53/m56 | **0 / 0** | 0 ✓ |
| fence-class fsync failures (ior `WARNING: fsync`) | — | **none** | none ✓ |
| `rewrite_shadow_close_retries` per co-writer | — | 8 / 0 / 0 / 1 / 0 / 0 / 12 / 1 — the rotation CONVERGED 22 times where it used to fence | |
| `CLAIM ANOMALY` / `block_claim_anomalies` (m50…m57) | 0 / 7 / 323 / 205 / 52 / 3 / 323 / 0 | **0 / 13 / 0 / 2 / 1 / 0 / 11 / 11** (−94 %; the ~7 % no-fence residue §6 named) | 0 ✗ (38 left) |
| `block_untracked_free_refusals` (m0) vs Σ `cowriter.free_ship_own_lane_untracked` | 163 vs — | **154 vs 156** (19+17+19+24+19+15+22+21) — the attribution closes | attribution ✓ |
| lane ENOSPC on the formerly affected six | 99–133 | 72–126 (`alloc_lane_enospc_refusals` 3,985–10,405) — not to the clean pair's level | ✗ |

**Verdict — LANDS.** The live-mount data-loss face is closed on the
fleet: no epoch is fenced at close by a rotated token, no acked bytes are
discarded, no fsync fails; the anomaly lineage fell 94 % and its residue
is the no-fence latch shape §6 named. The refused frees are now
ATTRIBUTED by the per-co-writer counter (154 ≡ 156) — the next item is
that lineage (an own-lane free of an offset this mount already released,
resurrected by a stale layout refetch, reaching the wire on the
non-recomputed paths). Lane ENOSPC did not fall to the clean pair's level
because — as the lane-visible note §6 shows — exhaustion is governed by
the coherence windows (term 1), not by these lineages.
