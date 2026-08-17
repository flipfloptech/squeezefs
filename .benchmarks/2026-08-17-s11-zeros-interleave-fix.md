# 2026-08-17 — S11 rung 18 opener: the zeros-rewrite interleave C8 mint ATTRIBUTED and FIXED — the `s11-range` composition gate FLIPPED (×3 from zero)

Branch `fix/s11-zeros-interleave-c8` (off dev `f609f984`); the finding is
rung 17's OPEN residual #4 (`.benchmarks/2026-08-17-s11-authority-assembler.md`
§Findings — "the reason the gate has not flipped"). Venue: the mw fleet
rig (tcp devsub, nvmet-tcp on localhost), 1 authority + 2 co-located
co-writers, `SQZ_MWFLEET_RANGE_CUSTODY=1`, `SQZ_MWFLEET_MW_PORT=54193`.
Suite: `tests/mw_authority_assembler_tests.rs` (17/17; the two new pins
red-first). Evidence tier: measured-simulated (one box, co-located
members).

## HEADLINE

* **The mint is attributed and dead**: finding #3's custody-scoped full
  Put was **structurally disarmed on every production mount** —
  `arm_multi_writer` never installed a `RangeGeometry` source
  (`install_range_geometry` had exactly two callers, both test
  fixtures), so `custody_scoped_layout`'s no-geometry arm applied every
  range holder's full Put **VERBATIM** ("peer entries at risk", its own
  warn text, ~60×/cell in the authority log). Finding #3 was fixed in
  the test venue and unfixed in the product.
* **The zeros-dependence was FALSIFIED**: under a uniform barriered
  harness the mint is CONTENT-BLIND (extended matrix below). Zeros was
  the original instrument's timing accident — and its read-back MASK.
* **The `s11-range` composition gate is GREEN ×3 from zero** (byte half
  AND fsck/C8 half), post-fix binary, fresh fleet per run — the
  counted-restart discipline (nothing pre-fix credited). The gate text
  is restored from the narrowed standing-red to the full green gate.

## The conviction (live-first, exactly as ordered)

### 1. The live repro and the actual C8 records

Fresh armed fleet, one 128 MiB striped file (32 × 4 MiB blocks),
3 barriered concurrent passes over disjoint 64 MiB halves — m50 (lane 2)
zeros over [0,64M), m51 (lane 1) urandom over [64M,128M),
`conv=fsync,notrunc` (the note's iso2 shape, scripted:
`isocell.sh`, a scratch instrument). First run reproduced exactly:
**fsck findings 48**, all in **lane 2 = m50's allocation lane**, two
shapes:

| shape | count | reading |
|---|---|---|
| `1 durable vs 0 layout references` | 32 | EVERY pass-2/3 mint of m50's half: take landed, the map never (or no longer) references it |
| `0 durable vs 1 layout references` | 16 | EVERY pass-1 key of m50's half: still in the FINAL durable map, its take RELEASED |

So m50's half of the durable map **ended at its pass-1 keys** while the
ref stream advanced through passes 2–3 — the map was REVERTED while the
ledger moved. Beside it: `block_untracked_free_refusals = 16` on the
authority ("begin_free REFUSED untracked offset … double-release
lineage"), in PAIRS on m50's pass-1 offsets, and the free over-shipment
(`free_shipped_blocks` 58 + 62 vs the legitimate 32 + 32; served-Freed
104 + 16 refused = the 120 shipped, closing the ledger).

### 2. The smoking gun in the authority log

Every served full Put of the ino logged:

```
WARN squeezefs::meta_ship::publish] S11: a range holder's full Put for ino 2
cannot be custody-scoped — no geometry source installed; applying verbatim
(peer entries at risk — arm the geometry source with the range plane)
```

`install_range_geometry` grep: **two callers, both test fixtures**
(`tests/mw_authority_assembler_tests.rs:199`,
`tests/dlm_range_custody_tests.rs:1083`). The production arm
(`multi_writer::arm_multi_writer`) installs the custody owner, the
free/harvest executors, the publish client and the extent assembler —
and no geometry. Consequences, all from the one missing install:

1. **`custody_scoped_layout` stands down** → every range holder's full
   Put (the epoch-close save class: `save_metadata_to_backend` with
   `publish_entries = None` is always delta-INELIGIBLE, so every
   rewrite-epoch close ships a full `SetLayoutAndSize`) applies
   verbatim — finding #3's clobber, live.
2. **`block_size = None` at every production ranged acquire** → the
   §9.3 demotion barrier is silently disarmed ("no geometry, no
   barrier", `dlm.rs`'s own words) and the §9.2 span cap never engages.

### 3. The mint mechanics (all observed, no inference left)

Per rewrite pass a co-writer's epoch close ships ONE full Put computed
from its RAM view: its own half fresh + the PEER's half as of its LAST
FETCH (a dirty/cached entry never refetches — by design). Applied
verbatim, the LAST Put to land reverts the peer's half to that stale
view while both sides' ref ops (which ride the same
`set_layout_and_size`, and are correct per-half) land:

* peer's superseded mints → **dangling takes** (32);
* peer's pass-1 keys back in the map with their takes released → the
  **0-vs-1** shape (16);
* the reverted co-writer's next pass refetches the REVERTED head,
  re-displaces already-freed pass-1 keys → **paired double-release free
  refusals** (16) + the free over-shipment;
* the reverted half's map references **freed/discarded offsets** —
  **silent data loss**, masked in the zeros cell because a discarded
  devsub block reads back zeros and zeros were the expected content.
  (In the leg's kill arm the byte half stayed green for exactly this
  reason: the zeros mask. The C8 half was the only honest witness.)

### 4. The discriminator matrix EXTENDED (fresh fleet per cell, uniform
### 3-barriered-pass harness, pre-fix binary `f609f984`)

| cell | m50 (low, lane 2) | m51 (high, lane 1) | findings | lanes hit |
|---|---|---|---|---|
| iso2 | zeros | urandom | **48** | lane 2: 32 danglers + 16 missing-takes |
| iso3 | urandom | urandom | **48** | lane 1: 16 danglers; lane 2: 16+16 |
| iso4 | urandom | zeros | **48** | lane 1: 32 danglers + 16 missing-takes |
| iso5 | zeros (solo) | — | 0 | — |
| iso6 | zeros | zeros | **48** | lane 2: 16 danglers; lane 1: 16+16 |

The mint is **content-blind**: every concurrent cell mints, only solo is
clean, and the damaged lane follows Put-landing ORDER (the last-landing
writer's stale view picks the victim), not content. Rung 17's iso3/4/6
"clean" cells were a timing artifact of that session's ad-hoc harness —
the zeros clue was real evidence of the MASK, not of the mechanism.
(Dev-control: the same pre-fix binary, same harness, solo cell clean —
the harness itself mints nothing.)

## The fix (scoped to the conviction — no rung-17 redesign)

Commits (branch `fix/s11-zeros-interleave-c8`):

1. `test(mw): red pins — the zeros-interleave C8 mint is the UN-ARMED
   custody scoping (rung 18)` — red at `f609f984`:
   * `a_range_holders_put_without_geometry_refuses_rather_than_reverting_a_peer`
     (behavior-red: the Put applied verbatim and erased the peer);
   * `the_production_range_geometry_arms_the_scoped_put` (compile-red —
     the production source did not exist; the a4768339 precedent).
2. `fix(mw): arm the production range-geometry source; a range holder's
   Put is scoped or REFUSED`:
   * **`multi_writer::router_range_geometry(meta, backend)`** — the
     production §9.2 source: size = the ino's durable layout head's
     (0 when absent — the span cap floors at 16; the scoping arm
     consumes only `block`), block = the data plane's live block size.
     Never a constant (Issue-19). Installed by `arm_multi_writer`
     beside `install_custody_owner` for any arm with a data-plane
     router (the solo arm included — a later enrollment never runs a
     grants-without-geometry window).
   * **`custody_scoped_layout`: scoped or not at all** — the
     no-geometry and re-encode-failure arms now REFUSE loud (error
     reply through the witness window; the client's save error refills
     its deferred refs and the writeback ladder re-publishes —
     never-lossy) instead of applying verbatim. On a correctly-armed
     authority both arms are unreachable.

   Untouched: the era gate, the dedup window, chain-onto-head,
   batch-prior compaction, the custody-scoped apply itself, every
   C8/C10 detector. The three rung-17 composition pins stay green
   (suite 17/17).

## Acceptance

* **Fixed-binary discriminator sanity** (fresh fleets): `fix-iso2`
  (zeros/urandom) findings 0, drift 0; `fix-iso3` (urandom/urandom —
  the strongest pre-fix minting control) findings 0, drift 0.
* **`s11-range` composition gate ×3 from zero** (teardown → create →
  leg, post-fix binary `e174b03b`): runs 1, 2, 3 all
  `s11-range GREEN — the first sub-file multi-writer rows, composition
  included` — custody half (engagement exact, Issue-19 column 0, zero
  conflicts, clause ledgers silent on aligned custody), byte half
  (own-mount + cold-authority whole-file verify), kill arm (victim
  swept, survivor green, `range_custody_active` → 0, re-admission),
  fsck findings 0, `meta_kv_block_refs_drift` 0, zero residue.
* **Gate text restored** (`tests/run_mw_matrix.sh`): the narrowed
  standing-red adjudication is retired; a composition failure is now a
  plain REGRESSION naming this note. The `SQUEEZEFS_RANGE_CUSTODY`
  registry justification amended the same way (default stays OFF; the
  default-ON revisit is rung 18's, behind this gate staying green).
* Touched + adjacent suites serial: green (list in the gates section).
* **`s9-fanout`**: green on a fresh promptly-run fleet (engagement
  exact, amp columns present, tripwires flat, fsck 0 / drift 0). A
  first attempt on a fleet that had sat idle ~10 min failed with fsync
  EINVAL — the authority's cluster-wire 60 s idle-session reaper closed
  the publish session and the next lane-raise verb refused
  (`alloc_lane_raise_refusals`, "the coordinator closed the session")
  instead of reconnecting; recorded as a venue residual below, not a
  composition fault (fresh-run green ×1; the leg has always run
  promptly after create).
* **`s10-intents-tarx`**: green on a fresh `SQZ_MWFLEET_OSS_GB=32`
  fleet with the REAL linux-src `fs/` tree (2384 entries; A-B-B-A rows:
  intents ON 14.54/14.65 verbs/entry vs OFF 17.41/17.55). Two venue
  notes: (a) an aged fleet (post-s9-fanout churn) refuses with meta
  ENOSPC on utime — fresh fleet per measured leg is the standing
  discipline; (b) the default 4 GiB×2 data set is STRUCTURALLY too
  small for a real-tree tarx on a co-writer (no staging ⇒ every
  non-inline file costs a whole 4 MiB striped block; 2384 entries ≈
  9.3 GiB of block accounting) — the 32 GiB thin-zram venue costs only
  the written pages.

## The `s11-subblock` priced leg (acceptance item 4)

Run once on the fixed binary (fresh fleet,
`--membership --lease-ttl-ms=6000` — finding 5b's venue; note the rig
nuance: `--lease-ttl-ms` requires an explicit `--membership`). The leg
now runs **further than either of rung 17's attempts**: the §9.3
demotion barrier fired LIVE for the first time (it had never fired on a
production mount — "no geometry, no barrier" — until this fix armed it):
`range_custody_demotions 1 ≡ demotion_acks 1`, extents shipped and
served through the assembler (`extent_served 6`, `extent_replays 4` —
the witness engaging under resends), `extent_parks 2` /
`extent_escalations 1` on the authority's W2 overlay.

**Blocked before the price table by a NEW venue finding** (reachable
only now that the barrier arms): under the two-holder same-block extent
churn, the AUTHORITY-side assembler fold path exhausts the
NON-ESCALATING settle ladder — m51's `FlushExtents` returned
`EIO "block 0 of inode_2 did not settle after 24 binding rebinds"`
(authority `stale_binding_rebinds` = 48; the read path's
rebind-starvation fix escalates to the serialized settle arm, the fold
venue does not). This is the sub-block plane's own machinery — OUT OF
SCOPE for the zeros-interleave conviction (fixing it means extending
the 2026-08-04 rebind-starvation escalation into the assembler fold, a
red-first ladder of its own). The price table stays rung 18's, now with
THREE named venue findings: 5a (POSIX-5 ladder on the ranged acquire),
5b (barrier bound vs DLM_LEASE_WAIT), and this fold-settle exhaustion.

## Gates

* Touched + adjacent suites, serial (19 suites, all green):
  mw_authority_assembler **17** (15 rails + the 2 new pins),
  mw_publish_era_gate 5, dlm_range_custody 33, dlm_multi_writer 16,
  mw_layout_version 11, mw_cowriter_free 13, mw_cowriter_lane 23,
  dlm_cowriter 18, meta_ship 15, publish_coalesce 6,
  publish_drain_economy 7, write_commit_economy 2, write_commit_crash 2,
  layout_delta_fold 9, durable_block_refs 15, mw_truncate_lease_strand
  1, overlay_overwrite 32, env_knob_convention 21, skip_ledger 11.
* `cargo clippy --all-targets --all-features -- -D warnings` +
  `cargo clippy --all-targets -- -D warnings` (shipped config): clean.
* `cargo fmt --check`: clean. shellcheck: clean (`run_mw_matrix.sh`).
* markdown link check: PASS (265 files, 0 broken).
* No new env knobs (ENG-10: one existing registry text amended); no
  loom (no lock-free core touched — the fix is an install + a refusal
  arm under the existing serve serialization).
* Zero-residue teardown asserted after every fleet (final:
  `teardown complete — zero residue`, no state dirs, no stray
  /dev/nvme* plain files, no mounts).
* Full `task check`: DEFERRED per the rung charter.

## Residuals for rung 18 (updated from rung 17's ledger)

1. ~~The zeros-interleave C8 mint~~ — CLOSED (this note).
2. The ranged acquire's POSIX-5 retry ladder + the barrier-bound vs
   DLM_LEASE_WAIT reconciliation (rung 17 findings 5a/5b) — still open;
   now MORE load-bearing because the demotion barrier is armed in
   production for the first time (it had never fired on a live mount
   before this fix).
3. **The assembler fold-settle exhaustion** (NEW, found by the subblock
   run above): the authority's fold path under two-holder same-block
   extent churn exits the 24-rebind ladder as EIO on `FlushExtents`
   instead of escalating to the serialized settle arm (the read path's
   2026-08-04 fix). Blocks the `s11-subblock` price table.
4. §9.5 MPI-IO / adversarial / block-cyclic rows (rung 18 proper).
5. free_grace ack cadence under rewrite churn (rung 17 finding 5c).
6. A RANGE holder whose custody was RELEASED before its straggler Put
   serves still applies verbatim (the `ClientCustodyShape::None` arm is
   the pre-custody class by rung 17's law). Unreachable in the leg
   (saves ship synchronously under held custody); a post-release
   straggler-publish class wants its own adjudication at rung 18.
7. The cluster-wire idle-session reaper vs the publish client: a
   lane-raise (and any witnessed verb) whose session was idle-reaped
   refuses instead of reconnecting — surfaced as fsync EINVAL on an
   idle-then-driven co-writer (the s9-fanout first-attempt shape).
   The witnessed-resend machinery makes a reconnect-and-resend safe by
   construction; wire it.
8. Rung 17 residuals 5 (whole-block re-acquire corner, spill-sink
   recovery, zc-prefilled read-overlay corner, retention-overlay vs
   concurrent truncate) and rung 15's 4–6 — unchanged.
