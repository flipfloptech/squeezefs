# Design: Durable Per-Ino Layout Versions (spec §6.2 item 9)

**Status:** landed (branch `feat/mw-durable-layout-version`, 2026-08-03).
**Format face:** incompat **bit 15**, `KV_LAYOUT_VERSIONS` — built, **never
stamped** by a production format (ruling D9); `set_layout_versions_bit` is the
offline Phase-8 upgrade verb.
**Contracts:** `tests/mw_layout_version_tests.rs` (+ the extended
`tests/decoder_property_tests.rs` round-trip and the untouched
`tests/layout_delta_fold_tests.rs` algebra pins).
**Sibling record:** the layout delta wire itself is the write-commit-economy
campaign's (`.benchmarks/2026-07-30-write-commit-economy.md`, incompat bit 5);
the RMW base-cache serve it composes with is rewrite-publish-drain Lever A
(`.benchmarks/2026-08-01-rewrite-publish-drain.md`).

## 1. The hole being closed

A layout delta chain's base was named by a **process-local token**:
`CachedMetadata::layout_base_token` (Lever A) proves the RAM entry is coherent
with the ino's *current fencing era*, and the chain itself is positional — the
fold applies whatever deltas sit above the newest `Put`. Under the single
writer that is sound: `INODE_META_LOCKS` + the 4a I-guard make the RAM belief
and the durable chain agree, and the era check catches lease loss.

It stops being sound the moment the base's *identity* must survive the
process: an authority failover folds chains whose base was named in a dead
process's RAM, and an S8/S9 shipped publish is a delta computed against a
**co-writer's** belief of the base that the **authority's** durable chain must
actually match. A delta folded onto a base other than the one it was computed
against produces a layout **neither writer ever computed** — "divergent chains
fold to divergent layouts" is silent data corruption, not a leak.

## 2. The representation: one version pair per LINK, riding the existing record

Each delta record gains an optional `(base_version, version)` pair
(`src/layout_wire.rs`, wire flag bit 4, fixed offsets 3..19 — peekable without
a decode):

* `version` — this link's own durable name, minted by
  `crate::dlm::mint_layout_version()` as **`(term << 40) | seq`** — the exact
  fencing-token composition the tree uses everywhere. It draws from the **same
  `GRANT_SEQ` sequencer as the S1 fencing mint** on purpose: `(term, seq)`
  uniqueness stays ONE invariant with one owner, gap-carrying sequences are
  already the law, and the S2 durable term makes links unique **across writer
  eras** — which is precisely what makes the base nameable after failover.
  `0` is reserved: "unversioned record" (the entire pre-item-9 wire).
* `base_version` — the `version` of the link this delta was computed against;
  `0` for the first link above a full `Put` (a bare `Put` carries no stamp —
  see §5 for why that is not a hole) and for a writer with unknown provenance.

Rejected shapes, and why:

* **A per-ino version record/xattr** — a second staged record per publish:
  journal bytes and fold work for something the link itself can carry in 16
  bytes. The pair rides the SAME staged record in the SAME tx, so the D4
  economy is untouched *structurally* (pinned by journal-entry-count equality
  in the suite, the way S3.5 pinned its economy).
* **A version field inside `LayoutMetadata`** — bincode is not
  self-describing; every historically stored layout value would stop decoding.
* **Naming the base by record seq** — the seq is assigned inside `commit_tx`,
  after the writer has already built the delta; plumbing it back out would
  re-serialize the publish path for no gain over the mint.

## 3. The commit gate (the enforcement line)

`KvMetaBackend::admit_versioned_delta` — both merge arms (the Lever B
aggregated pass and the `SQUEEZEFS_PUBLISH_COMMIT_GROUP_MAX=1` direct path) —
runs the pure verdict `layout_wire::layout_version_gate` against the durable
chain head (`delta_chain_probe`: DUR-8b's depth probe extended to also peek
the newest link's pair; one gather, no decode). Under the 4a I-guard the probe
cannot race the commit; within one aggregated pass, an in-batch head map keeps
a second same-ino member honest against the head the pass just staged.

| durable head | claim == head | claim == 0 | claim ≠ 0, ≠ head |
|---|---|---|---|
| bare `Put` (depth 0) | — | **Stage** (first link) | **Re-base** (full `Put`) |
| versioned link `Vh` | **Stage** | **Re-base** | **REFUSED loud** |
| unversioned link (pre-stamp chain) | — | **Re-base** (first-touch) | **REFUSED loud** |
| any (delta itself unversioned) | **Re-base** | **Re-base** | **Re-base** |

The two dispositions are deliberate and asymmetric:

* **Re-base, never error**, wherever the shape is locally legitimate:
  a refetched writer (claim 0 — `fetch_metadata_from_backend` cannot see the
  head's version through the fold, so refetched provenance is honestly
  unknown), **a chain the SMO compactor collapsed underneath a live writer's
  RAM provenance** (nonzero claim against a bare `Put` — the claim may name
  the very state the `Put` folded from, so refusing would wedge a healthy
  writer), and the pre-stamp first touch. The full `Put` is convergent by
  construction — it IS the writer's absolute state, computed from the base it
  served under the lock chain.
* **Refuse loud, never silently full-`Put`**, when a *nonzero* claim
  contradicts a *versioned or pre-stamp* head: two writers genuinely disagree
  about the chain tip. Staging would fold a layout nobody computed; falling
  back to the full `Put` would let the stale-based writer **clobber** a head
  it never saw. The refusal (`InvalidOperation`, naming §6.2 item 9, both
  versions, and the volume) stages nothing and crosses the publish wire back
  to a shipped caller, whose correct move is refetch-and-recompute.

**Wedge-vs-clobber adjudication:** a deterministic refusal reaching the
never-lossy writeback ladder would retry forever. That is acceptable because
the refusal is **unreachable from the local publish path by construction** —
every local layout persist funnels through `save_metadata_to_backend_ext`
(verified: the only `set_layout_and_size` caller in `routing.rs` is that
function's own full arm), whose republish stamps `CachedMetadata::layout_version`
to exactly the link it persisted (or 0 on a full save), under
`INODE_META_LOCKS`; the suite pins the local shapes (rebase arms) as
non-errors. The refusal therefore only ever fires against a *foreign* writer,
where refusing loudly and retaining staged custody is exactly the never-lossy
posture — data stays in staging, operator-visible, never folded divergently
and never clobbered.

## 4. The fold law (the backstop)

`fold_deltas_onto_put` (every from-scratch fold: reads, compaction, journal
replay reads) enforces, per base segment:

1. **Homogeneity** — all-versioned or all-unversioned (the gate re-bases
   before a versioned link can join a pre-stamp chain, so a mix is a gate
   bypass);
2. **First versioned link claims 0** (the only claim the gate stages onto a
   bare `Put`);
3. **Every later link's `base_version` IS the previous link's `version`.**

Violations are `KvError::Corrupt` naming the divergence. Tie-duplicate
records (same seq across sources — the bset/replay-window overlap the fold
input law already allows for identical-effect records) re-apply idempotently
and skip the link check. `fold_forward` (D7 overlay heads) stays
**version-blind by design**: a materialized head carries no version memory.
The gate is the enforcement line; a gate-bypassing chain refuses loud at the
latest by its next cold fold or compaction pass. That residual window (reads
served from an overlay head built by in-process applies) is named, not hidden
— in-process applies passed the gate, so only a rogue/foreign appender could
populate it, and item 9's runtime sibling (the node-cache partitioning gate)
is the layer that refuses foreign structural mutation.

## 5. Compatibility (ruling D9) and the old-format rule

Nothing stamps bit 15 today: `SuperblockV3::plan` omits it (pinned), mount
never ratchets it (unlike bit 5 — version emission keys on the **open-time**
superblock snapshot, so the bit is durable strictly before the first versioned
record can be; there is no crash window to order). Consequences:

* **Un-stamped volumes are byte-identical**: the backend strips the pair at
  encode (`encode_unversioned`), pinned by exact journal-byte equality against
  the shipped wire. A pre-item-9 binary keeps reading them forever.
* **A stamped volume refuses pre-item-9 binaries at mount** (unknown incompat
  bit — KD-14), which is exactly right: their decode would otherwise die
  mid-read on wire flag 4, and their gate-less publishes could stage
  unverifiable links.
* **Mixed chains exist only after a Phase-8 stamp**, and only in one shape: a
  pre-stamp unversioned chain below the stamp point. The rule is
  **fold-then-rebase on first touch**: the chain folds normally (reads
  unaffected), and the first post-stamp publish re-bases it with a full `Put`
  — terminating the unversioned segment before any versioned link can join
  it. No converter, no scan, no second format change.
* The first link above any `Put` is **unverifiable by construction** (the
  `Put` is unstamped). This is not a hole for divergence: uniqueness of
  `version` closes every fork from the second link on — two writers staging
  "first links" onto one `Put` collide at the gate, because the second one's
  head is now the first one's versioned link and its claim (0 or stale) can
  only Re-base or be refused. What the unverifiable first link *does* concede
  is that a claim-0 re-base trusts the custody plane exactly as far as the
  pre-existing shipped full-`Put` verb (`SetLayoutAndSize`) already does —
  under S9, custody serializes writers per ino, and the version chain is the
  check that custody actually did its job.

## 6. What S9 still owes on this surface

* **Shipped-strictness for claim-0 deltas**: the owner-side publish handler
  could refuse claim-0 shipped deltas outright (a co-writer under custody can
  always learn the head version) instead of re-basing with the co-writer's
  full layout. Deliberately not built here — it belongs to the S9 custody
  lease surface (`meta_ship/publish.rs`, owned by the parallel co-writer
  branch), and today `owner_of` never routes, so no shipped publish exists to
  strengthen.
* **Returning the staged link's version on the publish reply** so a co-writer
  can chain without a refetch (`PublishReply::DeltaUsed` → a versioned reply).
* The fold_forward residual named in §4.

## 7. Verification posture (ruling D11)

No cargo test suites beyond the targeted files, no benches, no rigs, no
measured claims. Run for this change, one at a time:
`mw_layout_version_tests` (11 contracts: wire round-trip/strip/refusal, bit-15
disjointness + D9, fold divergence/mixing/tie-duplicates, failover
restart+term-bump fold-identity across bset AND replay faces, commit-gate
refusal with untouched durable state, compaction-tolerance re-base, pre-stamp
first-touch, journal entry-count equality + exact 16 B/delta byte delta,
multi-thread concurrent chains, Lever A ledger closure on a stamped volume),
`layout_delta_fold_tests`, `write_commit_economy_tests`,
`publish_coalesce_tests`, `publish_drain_economy_tests`,
`write_commit_crash_tests`, `decoder_property_tests`,
`dur_metadata_integrity_tests`, the four bit pins
(`writer_scoped_staging_tests`, `dlm_data_fence_tests`,
`dlm_membership_tests`, `meta_ship_tests`), and
`dlm_multi_writer_tests::the_daemon_publish_surface_ships_to_the_owner`.
Clippy clean under both configs; the encode-path bench addition
(`benches/write_path_bench.rs`, `write_layout_publish` group) is **written,
not run**, with its prediction and falsification bounds in-file.
