# 2026-08-18 — S11 rung 19: the width-N same-ino publish REFS/lineage composition FIXED — the block-cyclic fsck half ×3 GREEN, THE MPI-IO VERDICT ISSUED (MET)

Branch `fix/s11-widthn-refs` (off dev `0e539fe2`); the finding is rung 18's
standing-red residual #1 (`.benchmarks/2026-08-18-s11-mpiio-row.md` — "the
width-N same-ino publish REFS composition … compute displaced/inserted
INSIDE the chained merge"). Venue: the mw fleet rig (tcp devsub, nvmet-tcp
localhost), 1 authority + 8 co-located co-writers,
`SQZ_MWFLEET_RANGE_CUSTODY=1`, `SQZ_MWFLEET_MW_PORT=54193`,
`SQZ_MWFLEET_OSS_GB=32`, fresh fleet per counted run. Instruments: pinned
ior 4.0.0 (sha256 `510b7d…9158`) + Open MPI mpirun 5.0.10, the stats-inode
deltas, the C8 oracle, and a scratch controlled repro
(width-8 `-F` control shape, cold fsck with files KEPT **and** deleted —
the publish-plane / delete-plane discriminator). Evidence tier:
measured-simulated (one box, co-located members).

## HEADLINE

* **The residual was not one bug but a FAMILY OF FOUR**, each convicted
  live on this branch's own from-zero runs, each fixed red-first:
  1. **The fork latch** (rung 18's "retried divergence/compaction churn",
     the MPI-IO stall's named cause): node compaction collapsed a
     versioned layout chain into a bare `Put` UNDERNEATH a later link
     whose base claim was legally gated — every subsequent fold of the
     key refused FOREVER.
  2. **The swapped-pair refs mint** (the residual's own wording):
     caller-frame block-ref deltas rode the authority's composed commits
     while the composition displaced different bindings.
  3. **The GLOBAL-ino identity hole** (introduced by fix 2's first cut,
     convicted by this campaign's own bisect): the recompute keyed
     records by the volume-LOCAL ino — phantom records nothing can
     release.
  4. **The indirect-head hole** (the MPI-IO row's 10 GiB face): the chain
     gate STAGED deltas onto `indirect:` heads (fold-refused forever —
     the poisoned-volume shape), and a range holder's Put onto one fell
     to the verbatim clobber arm.
* **The s11-blockcyclic fsck/C8 half is GREEN ×3 from zero** on the final
  binary (counted-restart; findings 0 / drift 0 every run) — the
  standing-red adjudication text retires.
* **THE MPI-IO VERDICT IS ISSUED: MET.** The unmodified rung-18 leg ran
  end-to-end on the final binary — A-B-B-A both brackets ≥ 0.8×
  (min **1.411×**), engagement exact, read-back-exact zero errors, cold
  fsck findings 0 — quiet-gated, no provisional label. The verdict table
  is below, with its honest boundary named (the leg self-sized into the
  inline-map domain; the indirect-map domain is a fail-safe refusal +
  the recorded rung-20 residual).

## The conviction ladder (live-first, exactly as ordered)

### 1. The fork latch (bc row, from-zero loop, run 5 of 5)

The plain `s11-blockcyclic` row was re-run as declared signature
gathering (runs 1–4 green — the mint is schedule-dependent). Run 5
minted the FULL shape:

```
kv checkpoint tick failed on "/dev/nvme9n1": corrupt KV encoding:
divergent layout-delta chain (spec §6.2 item 9): link seq 3024497 names
base version 0x800000001d6 but folds onto 0x0
```

**5,421 failed checkpoint ticks**, `meta_kv_block_refs_drift` 266+ per
verify pass, fsck reply-too-large, the layout UNREADABLE to the C8 walk
— and, critically, on every re-read: once minted, every publish of the
ino fails forever (the retried divergence class can never converge on a
durably forked chain). This is the MPI-IO row's phase stall, named.

Mechanism (pinned deterministically in
`meta_backend::kv::bset::tests::compaction_preserves_the_lineage_a_live_link_claims`,
RED with the exact live signature): a link's base claim is gated at
COMMIT against the then-durable head under the 4a I-guard — but the
checkpoint/SMO plane takes no 4a, so `compact_fold` collapsing the whole
chain into a bare `Put` invalidates a claim already committed (or gated
and mid-apply across the node swap). The gate's own "compaction
legitimately collapses a chain underneath a live writer" Rebase arm
covered ADMISSION only, never the already-committed suffix.

**Fix 1**: `compact_fold` folds everything BELOW a group's newest
versioned layout link and RETAINS the link, claim restamped to 0 (it is
now its segment's first link — the fold's own bare-`Put` law). Folded
value byte-equal; any live later claim still verifies; unversioned/inode
chains stay single-record byte-identical (solo volumes never pay the
extra record).

### 2. The swapped-pair refs mint (the residual's title face)

Pinned red-first at the wire level (`tests/mw_widthn_refs_tests.rs`):

* `a_chained_merge_recomputes_refs_against_the_live_head` — RED: the
  head's displaced binding dangled beside the new take ("1 durable vs 0
  layout references", the bc row's face), and a stale caller release
  survived to delete live records.
* `a_scoped_puts_refs_follow_the_composition_not_the_callers_frame` —
  RED with the exact swapped pair: an entry the custody scope DROPS
  staged a take that dangled beside a release that deleted the very
  record the composition KEPT (one caller frame = one pair = the rung-18
  drift-6 arithmetic).

**Fix 2**: on every AUTHORITY-COMPOSED layout commit the staged
accounting is computed FROM THE COMPOSITION ITSELF — the chained merge
diffs the delta's entries against the folded durable head (aggregated
conveyor member AND direct path, under the member's own 4a guard against
the just-probed fold), the custody-scoped Put diffs head→composed —
through the new `block_refs` resolver hook (the router's `block_ref_for`
behind `install_block_ref_resolver`, armed at `multi_writer::arm` beside
the range geometry, uninstalled at disarm). O(batch + inline-head
decode): the head decode is bounded by the xattr value cap (a format
constant) and paid only on the chained plane. The delta still rides ONE
publish transaction (never re-split); solo/un-chained/verbatim arms keep
the caller's frame byte-identical; C8 stays report-only.

### 3. The GLOBAL-ino identity hole (this campaign's own regression,
### caught by its own instruments)

Fix 2's first cut turned the bc row RED HARDER (run 1 post-fix: 72
danglers + C2 leaks on the CONTROL files). The controlled repro
(files-KEPT vs files-DELETED cold fsck) proved the mint at PUBLISH time;
the three-arm live bisect attributed it:

| arm | shape | result |
|---|---|---|
| A (both fixes) | full | mints 2/2 |
| B (lineage only, resolver disarmed) | caller frames everywhere | **clean 2/2** |
| C (chained recompute alone) | | mints 2/2 |
| D (tape) | per-entry recompute + owner tape | the phantom owners NAMED |

The armD tape's dangling owners read `0xE754_0000_0000_02`-class values
beside live files whose global inos are small — the **guest-namespaced
volume-LOCAL ino** (`((slot+1) << 40) | local`). The recompute ran
inside the KV volume, whose `ino` is the ROUTED-LOCAL identity; the
block-reference key law (`block_refs.rs`: "owner_ino: the referencing
inode (GLOBAL ino)") is load-bearing — a local-keyed record is a phantom
no release, no delete-path teardown and no oracle walk can ever match.

**Fix 3**: the routed layer threads the GLOBAL ino down the merge chain
as `refs_owner` (`QueuedLayoutMerge` carries it through the Lever-B
conveyor; the direct path takes it as a param). Pinned:
`the_chained_recompute_keys_records_by_the_global_ino` (local ≠ owner at
the KV seam — RED against the un-threaded build), plus the deterministic
chain-durability contract `tests/widthn_chain_checkpoint_probe.rs` (deep
chained chain + forced checkpoints: map complete, ledger mirrors map).
Controlled repro on this commit: **drift 0 ×2 from zero** (kept AND
deleted phases).

### 4. The indirect-head hole (the MPI-IO row's 10 GiB face)

The first post-composition MPI-IO run (self-sized to 10,240 MiB — past
the ≈ 6 GiB inline-map cap at 4 MiB blocks) convicted it live: a
co-writer's spill legally takes the shared head `indirect:`, the chain
gate's bare `{`-peek ADMITTED deltas onto it, and every subsequent
lookup/checkpoint refused forever ("layout delta base unusable: indirect
base" — fsync terminal-EIO, the same poisoned-volume shape as face 1).
The scoped Put's decode-bail verbatim arm was the clobber half.

**Fix 4**: a chained merge onto an indirect head REFUSES with the
retried-class marker and stages NOTHING; a range holder's Put meeting an
indirect map on EITHER side refuses the same way ("scoped or not at
all", rung 18's law). Never-lossy over availability: the refusals make
the missing indirect-domain composition FAIL-SAFE instead of
volume-poisoning. Pinned red-first:
`a_chained_merge_onto_an_indirect_head_refuses_and_stages_nothing`,
`a_range_holders_put_onto_an_indirect_head_refuses_not_clobbers`.

## The ×3 count (counted-restart, final binary, fresh fleet per run)

`s11-blockcyclic`, from zero ×3 — the rung-18 standing-red's named
acceptance:

| run | grants | cap_refusals | demotions | band vs disjoint | fsck | drift |
|---|---|---|---|---|---|---|
| 1 | 512/512 | 0 | 0 | 2.032× | findings 0 | 0 |
| 2 | 512/512 | 0 | 0 | 1.970× | findings 0 | 0 |
| 3 | 512/512 | 0 | 0 | 2.493× | findings 0 | 0 |

Zero-residue teardown after every run. The count RESTARTED from zero
after every fix, per the counted-restart discipline: an earlier ×3
(1.741×/2.394×/2.884×, same gates green) counted against the
pre-indirect-commit binary and was retired when commit 3 landed; the
table above is the FINAL binary's own consecutive count. Nothing
pre-fix is credited — the aborted counts are recorded as convictions.

## THE MPI-IO VERDICT TABLE (the rung-18 leg, unmodified, final binary)

`sudo tests/run_mw_matrix.sh s11-mpiio` — 8 co-writer mounts × 4 procs =
32 ranks, ONE shared file, 4 MiB-aligned block-cyclic segments, pinned
ior 4.0.0 POSIX MPMD, A-B-B-A vs `-F` file-per-proc on the SAME fleet,
quiet-gated (no provisional label — the gate passed). Probe 234 MiB/s →
the leg self-sized to **5,120 MiB (s=40), 5 iterations/phase** (the
sustained-window law: ≥ 60 s, ≥ 3 steady iterations).

| phase | mode | steady MiB/s | wall | iterations |
|---|---|---|---|---|
| A1 | shared | 156.1 | 170 s | 160.55 170.07 160.07 125.41 168.68 |
| B1 | fpp | 110.6 | 236 s | 120.34 122.04 88.40 110.10 121.99 |
| B2 | fpp | 221.9 | 139 s | 169.76 134.78 173.17 345.50 234.00 |
| A2 | shared | 504.3 | 60 s | 353.39 454.87 506.77 471.89 583.73 |

| gate | value | verdict |
|---|---|---|
| A-B-B-A bracket 1 (A1/B1) | **1.411×** | ✅ ≥ 0.8× |
| A-B-B-A bracket 2 (A2/B2) | **2.273×** | ✅ ≥ 0.8× |
| **THE ≥ 0.8× VERDICT** | min bracket 1.411× | **MET** |
| engagement | ranged acquires+extensions 3,200/mount ×8; publish shipped ≈ 80–84 k/mount; authority grants 5,525; cap_refusals **0**; demotions 0; conflicts 0 | ✅ exact |
| read-back-exact (`-r -R -C`, fixed `-G`) | 1,826 MiB/s aggregate, **zero data-check errors** | ✅ |
| cold-authority fsck + C8 | **findings 0 (clean)**, drift 0 | ✅ |
| zero-residue teardown | "teardown complete — zero residue" | ✅ |

**The verdict's honest boundary**: the leg self-sizes by probed
bandwidth, capped at 10 GiB. This run's probe (234 MiB/s) sized it to
5,120 MiB — the inline-map domain the composition now covers. A faster
probe sizes to 10,240 MiB (run 1 did), where a co-writer's spill takes
the head indirect and the row fails loud-and-fail-safe (fsync
EIO, retried-class, volume unpoisoned — fix 4) rather than completing:
the **indirect-map width-N composition** (blob-aware owner-side merge)
is rung 20's first residual below. The conditional rung-18 verdict is
hereby FINAL for the pinned leg at its own sizing law; the indirect
domain is a named, fail-safe, oracle-visible gap — never a silent one.

## No-regression legs (final binary)

* **`s11-range` GREEN** (fresh fleet, from zero): composition gate
  included — ranged engagement exact, Issue-19 column 0, zero conflicts,
  ships accounted, the rung-16 range-clause ledgers exported and silent
  on aligned custody; kill arm converged (`range_custody_active` → 0,
  victim re-admitted); fsck + C8 clean.
* **`s9-fanout` GREEN at its historical width 2** (fresh 1+2 fleet, from
  zero): engagement exact, amp columns present, tripwires flat, fsck
  findings 0 / drift 0. (A first attempt on the width-8 armed fleet
  aged by a prior leg tripped the leg's INVALID-ROW guard — the rung-18
  note's recorded width-8 territory + fleet aging, not a regression:
  the leg's own venue is width 2 and it is green there from zero.)
* Zero-residue teardown after every leg ("teardown complete — zero
  residue").

## The sub-block fourth face (rung-18 residual #2) — FIXED, ×3 from zero

The mission's conditional take: the conviction REACHED it — face 4 was
this same family (the authority fold's publishes carried caller-frame
accounting across the served free/publish window; the recompute closes
exactly that non-atomicity). **`s11-subblock` is GREEN ×3 from zero**
(fresh fleet per run, final binary): engagement GREEN (both holders
shipped, ledger closed, clauses fired, retention quiesced), the price
table published (run 1: 2,632 extents, ≈ 3,674 µs authority CPU/extent
upper bound — the KD-MW-8 priced exception, D1 holds), and the fsck/C8
half **findings 0 (clean) every run** — the adjudicated one-pair-per-row
mint is gone. The leg's standing-red die-text is retired to "ANY drift
here is a REGRESSION" (this note), and the s11-killrange cell-A
expected-shape gate already admits drift 0, so no gate loosened.

## Gates

* Touched + adjacent suites, serial (final binary): mw_widthn_refs 5,
  widthn_chain_checkpoint_probe 1, mw_authority_assembler 21,
  durable_block_refs 15, mw_publish_era_gate 5, mw_layout_version 11,
  layout_delta_fold 9, kv_node 12, mw_cowriter_free 13, mw_cowriter_lane
  24, dlm_multi_writer 16, dur_metadata_integrity 10,
  write_commit_crash 2, dlm_range_custody 33, mw_ranged_lease_ladder 7,
  rebind_starvation 4, write_pipeline 22, mw_truncate_lease_strand 1,
  publish_coalesce 6, `--lib` 305+91 — all green.
* `cargo clippy --all-targets --all-features -- -D warnings` +
  `cargo clippy --all-targets -- -D warnings` (shipped config): clean.
* `cargo fmt --check`: clean. shellcheck: N/A (no repo scripts touched).
  markdown link check: PASS.
* Full `task check`: DEFERRED per the rung charter.

## Residuals for rung 20 (ordered)

1. **The indirect-map width-N composition** (fix 4's named gap): a
   shared file whose composed map exceeds the inline cap (≈ 6 GiB at
   4 MiB blocks) cannot currently be published by concurrent chained
   writers — the head's spill turns every peer's publish into the loud
   retried-class refusal (fsync EIO; fail-safe, never poisoning). The
   composition needs a blob-aware owner-side merge (the authority CAN
   read the blob — the data router is armed; the meta plane cannot).
   The MPI-IO row at ≥ 750 MiB/s probe bandwidth self-sizes into this
   domain and is its acceptance.
2. The sub-block dangling-take face 4 (rung 18's residual #2) — see the
   assessment above.
3. The free-grace ack cadence under rewrite churn (rung-17 finding 5c,
   carried).
4. The kernel-split sequential-frontier extend RTT (carried).
5. `range_custody_grant_census` unexported; rung-15 residuals 4–6
   (carried).
6. The controlled kept-vs-deleted repro (`-F` control shape + cold fsck
   before AND after deletion) earned its keep as the publish-plane /
   delete-plane discriminator — worth packaging into the rig if the
   family recurs.
