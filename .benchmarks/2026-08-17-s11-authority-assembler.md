# 2026-08-17 — S11 rung 17: the AUTHORITY ASSEMBLER (KD-MW-8 revised) — machinery landed, composition gate NARROWED (not yet flipped)

Branch `feat/s11-authority-assembler` (off dev `ea0853a8`); charter
`docs/design-full-multi-writer.md` PR-plan row 17 + §9.3 + KD-MW-8 +
MW-10/11/13; the inherited shape `docs/design-mw-layout-versions.md` §6a
("PR 17 inherits the whole shape verbatim"). Deps: rung 15
(`.benchmarks/2026-08-17-s11-range-wire.md`) + rung 16
(`.benchmarks/2026-08-17-s11-b4-clause.md`). Venue: the mw fleet rig
(tcp devsub, nvmet-tcp on localhost), 1 authority + 2 co-located
co-writers, `SQZ_MWFLEET_RANGE_CUSTODY=1`, `SQZ_MWFLEET_MW_PORT=54193`.
Suite: `tests/mw_authority_assembler_tests.rs` (15/15, every pin
red-first). Evidence tier: measured-simulated (one box, co-located
members).

## HONEST STATUS UP FRONT

The charter's headline acceptance — the `s11-range` composition gate
GREEN ×3 from zero — **was NOT met**. What the rung's own gate runs
proved instead:

* The gate's **BYTE half flipped green**: the cold-authority whole-file
  verify now passes on every from-zero run (pre-17 it failed — the
  rung-15/16 standing red's data-loss face). Three real composition
  bugs were found by the ladder, fixed, and pinned (findings ledger
  below).
* The gate's **fsck/C8 half stays standing-RED on ONE characterized
  residual**: the kill arm's zeros-rewrite interleave still mints
  dangling ledger takes + double-release free refusals. The
  discriminator matrix (§Findings, iso2–iso6) pins the reproducing
  shape exactly; the mint's mechanism is NOT yet attributed. The leg's
  gate text now states this narrowed adjudication verbatim
  (`tests/run_mw_matrix.sh`, the G-RW2 standing-red pattern kept).

Everything else the charter names is BUILT and red-first pinned: the
WriteExtent/FlushExtents wire (schema 6), the demotion barrier with the
closed ledger, retention with all four pull release paths, MW-10/11/13,
and the live client/authority halves.

## Decisions (the charter's named adjudications)

1. **Publish schema = 6, not the design text's "4"** — the wire moved
   since the row was written (rung 9 took 3→4 for `HarvestLaneFree`,
   finding #6 took 4→5); the design's CONTENT governs. Adjudicated in
   the `PUBLISH_SCHEMA` doc.
2. **`WriteExtent`/`FlushExtents` join the RETRIED class — stated
   against `publish.rs`'s no-retry doctrine** (the module docs' opening
   law): exactly the finding-#6/shipped-free shape, inherited verbatim —
   era gate BEFORE the witness window (`extent_stale_refusals` its own
   row), the `(lease_epoch, request_id)` witness through the SAME
   `publish_dedup` window (ids come from one client sequence, so the id
   spaces cannot collide), same-frame bounded epoch-stable resends,
   never re-keyed.
3. **Default posture: `SQUEEZEFS_RANGE_CUSTODY` STAYS default-OFF**
   (rung 15's adjudication upheld against §11's provisional default-on):
   the composition gate has not flipped, so the never-lossy law keeps
   the lever dark; the fleet arms it explicitly. Revisit at rung 18
   only behind a green gate.
4. **The renewal-observation release path is keyed on the WITNESS IDS**
   the wire already carries: the owner's coverage ledger
   (`extent_ship::owner_*`) tracks per-`(client, ino)` merged/covered
   request-id watermarks, advanced by the `FlushExtents` force (and by
   an `ExtentAck{Some}` merge); the renewal reply carries
   `extent_covered: Vec<(ino, upto_id)>`. Same pull-only property as
   the design's "stamped version ≤ observed" without guessing a future
   version. The ack still carries `covering_version: Option<u64>`
   (path 1's design name).
5. **The demotion's coexistence model carves nothing**: an acked
   demotion marks the block-aligned region DEMOTED on the `FileCustody`
   entry — byte-disjoint (and, within the region, even overlapping)
   grants coexist there because NOBODY DMAs a demoted block (the
   authority is the single publisher; extents serialize in its
   overlay), and `span_is_range_shared` answers TRUE for any overlap of
   a demoted region, which is what re-routes every holder to
   extent-ship and is the rung-16 clauses' firing venue. A
   fence-resolved (incumbent-died) barrier marks NOTHING demoted — the
   survivor gets clean custody (pinned).
6. **Reclaim gained RANGE re-assertion** (`ReclaimFrame::ranges`,
   `reclaim_with_ranges`) — MW-13's law verbatim ("A re-asserts its
   ORIGINAL block-aligned grant"); the shipped whole-file-only reclaim
   would have turned the restart into a plain conflict wait.

## What landed (by commit)

1. `test(mw): rung-17 red pins` — the 13 original red-first pins
   (compile-red at the commit, the a4768339 precedent).
2. `feat(mw): the authority assembler …` — the four machines:
   * **publish.rs schema 6**: `WriteExtent` + `FlushExtents` (era-gated,
     witnessed, retried; own ledger rows
     `extent_{shipped,served,replays,stale_refusals,flush_forces,
     spills}` + `extent_retained_bytes`); `PublishReply::DeltaUsed`
     became `{used, version}` (the §6 chain-without-refetch residual
     paid); owner-side assembly = installed executor pair
     (`ExtentMergeExec`/`ExtentFlushExec`, the FreeExecutor precedent).
   * **The composition fix (chain-onto-head)**: a SHIPPED
     `MergeLayoutAndSize` re-stamps its claim onto the durable head
     under the backend's own 4a I-guard (aggregated pass AND direct
     arm), the link version re-mints from the OWNER's sequencer (two
     co-writers adopt one term but run private `GRANT_SEQ`s — client
     mints CAN collide, owner mints cannot), at the chain cap the OWNER
     compacts from its own folded state (never the caller's private
     full layout), and the reply carries the staged version, stamped
     into the co-writer's RAM provenance (`routing.rs`). The authority's
     own publishes on a GRANTED ino chain too. The LOCAL path keeps the
     §6.2-item-9 gate verbatim (pinned). Routing skips the RAM
     chain-cap full save for chained targets.
   * **The demotion barrier** (`range_custody_core.rs` +
     `dlm.rs` + `data_grant.rs` CUSTODY_SCHEMA 4): block-sharing
     acquires mark pendings and PARK; the notice rides the incumbent's
     renewal reply, composed under the SAME `FileCustody` entry
     serialization that parked the waiter (the in-flight-renewal race
     pin); `VERB_CUSTODY_DEMOTE_ACK`; ack-or-incumbent-death resolution
     (the retire sweep = the fence column); closed ledger
     `demotions ≡ acks + fence_resolves` + the
     `demotion_fenced_publishes` ≈0 tripwire + `demotion_wait_ns`; the
     Extend arm barriers too (a widening into a newly-shared block);
     licensed-coexistence admits EXTEND the holder's own record (the
     §9.2 geometry cap would otherwise refuse the interleave the
     demotion exists for).
   * **Retention** (`src/extent_ship.rs`): retain-until-coverage riding
     `parked_extent_bytes`; budget derived R5/128 floor 8 MiB
     (`test_swap_retention_budget` seam); the four pull release paths;
     at-budget spills through the installed sink, never blocks;
     `reship_all` (MW-11); `note_demotion` (mark-local → quiesce → ack
     order); the read-your-writes overlay.
3. `feat(mw): the live extent-ship client half + the production
   assembler` — write-handler striped-branch routing of range-shared
   blocks to extent-ship (§5.4 sever before retention), fsync chaining
   through `FlushExtents`, the read handler moved behind the
   retained-extent overlay wrapper (`read_impl`), the authority's
   production executors (the fs's own write path — W1/B4 decline on the
   shared block, the W2 overlay parks, the fold publishes once, DMA
   authorized under the authority's OWN epoch = the §9.3 custody
   transfer), and the co-writer's quiesce (the §5.3 merge-discipline
   mutex barrier) + coverage-release cache invalidation hooks.
4. `fix(mw): a batch-prior ino never compacts over its own pass mates`
   — finding #2 below.
5. This commit — finding #3's custody-scoped full Put + the owner-side
   per-ino serve serialization, the priced-row leg (`s11-subblock`),
   the narrowed gate text, and this note.

## The pins (15/15 green, each red-first)

| Pin | Law |
|---|---|
| `write_extent_is_era_gated_witnessed_and_served_through_the_assembler` | era gate before the window (journal equality on refusal), witness replay (executor ran ONCE), `extent_{served,replays,stale_refusals}` |
| `the_demotion_barrier_withholds_the_grant_until_the_incumbents_ack` | grant withheld; ledger closes through ACKS; fenced-publishes 0; post-demotion BOTH holders classify range-shared and the rung-16 clause ledgers MOVE (their firing venue); per-block (block 1 untouched) |
| `an_unacked_demotion_resolves_when_the_incumbents_grant_dies` | the fence column closes the ledger; a fence-resolved barrier marks nothing demoted |
| `the_renewal_carries_the_notice_and_the_ack_releases_the_parked_grant` | the in-flight-renewal race pin: pre-mark reply carries nothing, post-mark reply ALWAYS carries; client order mark-local → quiesce → ack; grant issues only after |
| `an_unacked_demotion_resolves_at_lease_expiry_on_the_owners_clock` | the owner-clock expiry arm (manual clock advanced; B's peer lease kept renewed through the sweep) |
| `mw13_authority_death_mid_demotion_a_reasserts_and_the_demotion_restarts` | pending state dies with the authority; A re-asserts its ORIGINAL range span in grace (`reclaim_with_ranges`); the demotion RESTARTS and resolves by ack |
| `release_path_1..4` (four pins) | ack-carried `covering_version`; renewal watermark (ack alone NEVER releases); `FlushExtents` (fsync returns only after release); at-budget spill (never blocks; spilled stays logically retained) |
| `mw11_authority_death_between_ack_and_publish_loses_no_acked_fsynced_bytes` | retention survives the death; re-ship idempotent under the successor; byte-exact assembly; release only on coverage |
| `shipped_merges_chain_onto_the_head_and_never_clobber_a_peer` | claim-0 and stale-nonzero claims CHAIN; versioned replies; final layout carries every writer's blocks |
| `a_batch_prior_ino_never_compacts_over_its_own_pass_mates` | finding #2's repro (two publish clients + the conveyor hold seam + cap 2) |
| `a_shipped_full_put_is_custody_scoped_and_never_erases_a_peer` | finding #3's repro: a range holder's Put is authoritative inside its spans (absence = removal intent), peers' entries survive; whole-file Puts verbatim |
| `the_local_publish_path_keeps_the_version_gate` | solo untouched: local divergent claims still refuse naming item 9 |

Fixture update where the rung is the point (the era-gate suite's own
precedent): `out_of_order_and_cross_era_publishes_refuse` arm (a) now
pins the CHAINED shipped outcome (the §6 residual this rung pays
superseded the shipped-face refusal); the local refusal pin moved to
this suite.

## Findings ledger (the crucible doctrine working — every one found by
## the rung's own gate, on real mounts over real nvmet-tcp)

1. **The divergence-refusal wedge / full-Put clobber (the standing red's
   core)** — pre-17, two co-writers' layout publishes of one ino ran
   the §6.2-item-9 refusal → provenance reset → full-Put re-base from a
   PRIVATE RAM view → peer blocks erased (C8 drift 39k, byte loss).
   FIXED by chain-onto-head (owner-restamped claims, owner-minted link
   versions, owner-side chain-cap compaction, versioned replies). The
   gate's BYTE half flipped green on every subsequent from-zero run.
2. **Batch-prior pass-mate compaction erasure** — first post-fix
   from-zero run: C8 drift 8742 with CLEAN bytes. In ONE aggregated
   layout-merge pass, a chained same-ino member whose batch-head read
   sat at the chain cap compacted against the COMMITTED fold
   (`xattrs.lookup` cannot see a pass mate's staged delta) and its full
   Put erased the mate's entry while the mate's refs landed. Reachable
   only from TWO custody clients (one client's publish lane mutex
   serializes same-ino ships — law 3), which is why the in-process pin
   needed two `PublishClient`s. FIXED: a batch-prior ino stays on the
   chained delta past the cap (the next pass compacts).
3. **Un-scoped shipped full Puts** — the zeros/remove-class save (the
   vehicle of everything the inserts-only delta wire cannot express)
   ships the client's full layout, computed from a base that
   legitimately lags a peer's publishes → verbatim apply erased peer
   entries. FIXED: the owner applies a RANGE holder's Put
   CUSTODY-SCOPED (inside its spans the Put's presence/absence is the
   truth; outside, durable entries are preserved; whole-file and
   pre-custody Puts verbatim), with owner-side per-ino serve
   serialization (`SERVE_INO_LOCKS`) closing the scoping read→commit
   window (law 3's owner half made structural).
4. **THE OPEN RESIDUAL — the zeros-rewrite interleave C8 mint** (the
   reason the gate has not flipped). Fresh-fleet discriminator matrix,
   3 barriered concurrent passes over disjoint 64 MiB halves of one
   file (16 blocks each), post-fix binary:

   | run | m50 (low half) | m51 (high half) | drift |
   |---|---|---|---|
   | iso2 | **zeros** | urandom | **48 findings / 49 lines** |
   | iso3 | urandom | urandom | 0 |
   | iso4 | urandom | **zeros** | 0 |
   | iso5 | zeros (solo) | — | 0 |
   | iso6 | zeros | zeros | 0 |

   Signature: dangling ledger takes on the LOW half's superseded
   incarnations ("durable 1 vs 0 layout references") paired with
   authority-refused shipped frees ("already free — the double-release
   lineage", ~16/run on m50), i.e. one displaced chain forked: some old
   key released+freed TWICE, its successor's take never released. The
   leg's kill arm rewrites zeros (dd if=/dev/zero), so every leg run
   reproduces it. Serialized two-writer and single-writer zeros runs
   are clean — the mint needs the CONCURRENT low-half-zeros +
   high-half-differing pair, which is not yet mechanistically
   attributed (the three fixed arms above were each convicted and
   eliminated on this same matrix). **Rung 18 blocker #1.**
5. **Secondary live findings** (recorded, not fixed here): (a) the
   ranged co-writer's `acquire_lock_range` path lacks the POSIX-5 retry
   ladder — a demotion-barrier wait longer than the 5 s DLM budget
   surfaces EIO instead of retrying (the whole-file path has the
   ladder; rung-15 residual, now load-bearing); (b) at default
   membership cadence (TTL/3 = 10 s) the barrier's renewal-carried
   notice arrives after that budget — the priced row needs
   `--lease-ttl-ms=6000` (and the design's "well inside the bound"
   claim should be re-read against DLM_LEASE_WAIT at rung 18); (c)
   under sustained displaced-free churn with debug logging, free_grace
   evicted a co-writer as a laggard (`free_grace_laggard_fences`) —
   the §6.8-item-3 ack cadence vs rewrite churn wants its own row; (d)
   a per-phase file close RELEASES range grants, so sub-block sharing
   dissolves between opens — the priced leg now keeps the incumbent's
   fd open across the row (the §9.3 "mid-stream" wording made
   mechanical).

## The sub-block exception row (`s11-subblock` — built, NOT yet priced)

`tests/run_mw_matrix.sh s11-subblock`: two co-writers stream 4 KiB
records into the two halves of ONE block (the incumbent holds one open
fd and loops — finding 5d), with engagement gates (extent ledgers
account for both holders, demotion ledger closes, clause ledgers move,
retention → 0), the price columns (per-pass walls pre→post demotion,
extents/s, authority CPU/extent from `/proc/<pid>/stat` deltas), cold
byte verify + fsck. Two attempted runs each convicted a real venue
finding (5b, then a mid-run census eviction under the 6 s TTL) — the
row has NOT produced a publishable price table. It is rung 18's first
run-target once findings 4/5 close.

## Gates

* Touched + adjacent suites, serial (19 suites): all green —
  mw_authority_assembler 15, mw_publish_era_gate 5, dlm_range_custody
  33, dlm_multi_writer 16, mw_layout_version 11, mw_cowriter_free 13,
  mw_cowriter_lane 23, dlm_cowriter 18, meta_ship 15, publish_coalesce
  6, publish_drain_economy 7, write_commit_economy 2, write_commit_crash
  2, layout_delta_fold 9, durable_block_refs 15, mw_truncate_lease_strand
  1, overlay_overwrite 32, env_knob_convention 21, skip_ledger 11.
* `cargo clippy --all-targets --all-features -- -D warnings` +
  `cargo clippy --all-targets -- -D warnings` (shipped config): clean.
* `cargo fmt --check`: clean. shellcheck: clean (`run_mw_matrix.sh`).
* markdown check: this note + the touched script comments.
* No new env knobs (ENG-10: nothing to register); no loom (no new
  lock-free core — the demotion state mutates under the existing
  `FileCustody` entry lock, the lease-transaction mutex class the
  charter predicted; `SERVE_INO_LOCKS` is a plain async mutex stripe).
* `s9-fanout` / `s10-intents` re-runs: NOT run this session (budget) —
  rung 18 owes them alongside the gate re-count.
* Full `task check`: DEFERRED per the rung charter.

## Residuals for rung 18 (ordered)

1. **The zeros-interleave C8 mint** (finding 4) — attribute and fix;
   the `s11-range` gate ×3-from-zero count then restarts (nothing from
   this session is creditable — counted-restart law).
2. The ranged acquire's POSIX-5 retry ladder + the barrier-bound vs
   DLM_LEASE_WAIT reconciliation (finding 5a/5b).
3. Run + publish the `s11-subblock` price table; then the §9.5
   MPI-IO/adversarial/block-cyclic rows (rung 18 proper).
4. free_grace ack cadence under rewrite churn (finding 5c).
5. The whole-block re-acquire corner on a previously-demoted block
   (documented at CUSTODY_SCHEMA 4's doc), the production spill sink's
   recovery semantics (re-ship vs fold of spilled shared-block
   records), the zc-prefilled read-overlay corner (documented in the
   read wrapper), and the retention-store read overlay's composition
   with concurrent truncates.
6. Rung 15's standing residuals 4–6 (unchanged).
