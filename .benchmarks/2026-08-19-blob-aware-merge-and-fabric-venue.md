# 2026-08-19 — The blob-aware owner-side merge (rung 20 residual 1) + the first real-fabric MW venue

**Branch record:** `feat/s11-blob-aware-merge` (4 commits, ff-merged to dev):
`b810ce30` (red contracts) → `5ed65ebe` (the compose arm) → `bd0f234c`
(clippy-1.97 toolchain drift, pre-existing) → `4ef87de1` (rig sizes back
into the 10 GiB indirect domain). Follow-on rig/venue commits this session:
`3f55f21b` (PLACEMENT_STRATEGY), `4bc02b2b` (connect-capture), `094b092a`
(disconnect verb form), `e86bff6f` (rank-dispatch launcher), `69195bf7`
(MARKET knob, on-demand default — user ruling). Box note: the dev box moved
to NixOS this session (`shell.nix` carries the toolchain + rig deps;
scripts must be invoked as `bash script.sh` — `#!/bin/bash` shebangs do
not resolve).

## 1. What landed (residual board item 1 — CLOSED as an implementation, acceptance below)

The three owner-side refusal sites (`layout_merge_pass` member,
`merge_layout_and_size_direct`, `custody_scoped_layout`) now COMPOSE onto
an indirect head when the multi-writer authority is armed, through the new
`indirect_map` hook (`src/meta_backend/kv/indirect_map.rs` — the
block-refs-resolver pattern: installed at `arm_multi_writer` beside the
refs resolver, uninstalled at disarm, one relaxed load unarmed):
rehydrate the blob → compose the delta / scoped Put onto the FULL map →
recompute refs against the full head (+ the MAP_BLOB transfer pair) →
write a fresh CoW blob (flushed BEFORE the naming commit — DUR-6 §3;
RES-9 mint guards free it on failed commits) → stage a full Put naming
it → free the displaced blob strictly post-commit. Site (a) composes
same-ino batch mates on a pass-local memo (the batch-prior law's blob
face). Unarmed mounts keep the refusal byte-identical (pinned).
Engagement gauge: `publish_blob_composes` (0 on unarmed mounts by
construction). Contracts: `tests/mw_widthn_refs_tests.rs` §1d (5 new,
10/10 with the preserved pins); full cargo gate green (one pre-existing
~50 % dev flake convicted: `write_visibility_tests::
completed_overwrites_never_serve_the_previous_pass`, 5/10 on the
un-merged dev tip — owed its own red-first branch).

## 2. Venue A — the laptop (strixhalo, NixOS, 7.1.8 + sqz patch series, tcp devsub, 1 authority + 8 co-writers × 4 ranks = 32)

- Probe **1,582 MiB/s** → self-sized to the FULL **10,240 MiB** shared
  file (~2,560 map entries ≈ 100 KiB serialized — past the ~60 KiB
  inline cap): **the indirect domain, live**.
- **12 iterations completed with ZERO fsync EIO** — on pre-branch code
  this row was structurally impossible (the rung-19 refusal wall).
  Qualitatively, the compose arm survived its acceptance shape.
- **Flatness FAILED**: 1,435 → 938 → 255 → 955 → 241 → 101 MiB/s — a
  thermal sawtooth on a thermally-capped laptop (zram + 32 ranks + 9
  daemons on one throttling package). Only `_p0` snapshots persisted
  (the leg stopped after A1), so the `publish_blob_composes` delta for
  the indirect phase was not read. **Not acceptance** (the sustained-state
  law); the venue CAN reach the domain and is the only one measured that
  does — re-run with thermal mitigation is the open acceptance path.

## 3. Venue B — AWS (PRESET=mw, 4 × i4i.2xlarge, us-east-1b, nvmet-tcp over the real network, 1 authority + 2 co-writers × 4 ranks = 8)

Session shape: spot session reclaimed 20 min in (3/4 nodes terminated
mid-row — the abort-and-teardown machinery worked; count aborted per the
multi-run discipline; ~$0.30). On-demand session ran the row end-to-end
(~40 min, ~$1.60). Four rig fixes were convicted and landed getting
there (see the commit list above), including the Ubuntu 26.04 OpenMPI
5.0.10 packaging segfaulting on EVERY MPMD spelling — `run_ior` now
launches single-context with a rank-dispatch wrapper (same global-rank →
mount assignment, any-MPI-safe).

**The row: INVALID by the leg's own engagement gate — and the gate is
the finding.** Probe **42.58 MiB/s** → 928 MiB file (inline domain;
`publish_blob_composes` correctly 0). A-B-B-A ratio gate itself passed
(shared/disjoint 0.811 and 1.377, both ≥ 0.8). Artifacts:
`.benchmarks/cloud/2026-08-19-171824/` (incl. the harvested per-phase
stats snapshots under `rows/`).

Deltas across the row (authority, p0→p4):

| Counter | Δ | Reading |
|---|---|---|
| `dlm_custody_conflicts` | **+822** | on a 4 MiB-ALIGNED row with zero true sharing |
| `range_custody_desired_trims` | **+2,367** | the desire machinery keeps requesting overlapping hulls; the authority keeps trimming |
| `range_custody_demotions` | +9 (`prs`+2, `ors`+1) | fabricated block sharing — the engagement gate's refusal is correct |
| `range_custody_demotion_wait_ns` | buckets to ≤4s | pull-based revocation waits for the holder's next renewal — seconds per demotion at fabric RTT |
| `free_grace_offsets` | 0 → **825**, never draining | residual item 6's predicted signature, first live capture |

Both shared AND file-per-proc phases showed ~380 s outlier iterations
(vs ~29 s steady) — the stall is not purely custody-side; the free-grace
climb and the demotion-wait tail are the named suspects, unattributed.

## 4. Verdicts

1. **Item 1 implementation: LANDED and correct by contract + the live
   indirect-domain run.** The specified acceptance row (≥ 750 MiB/s
   probe, gates green) remains OPEN — the laptop reaches the domain but
   not flatness (thermal); the cheap fabric venue reaches neither the
   bandwidth nor a valid engagement.
2. **Residual item 7 is now a MEASURED product finding**, upgraded from
   a refinement note: at real fabric RTT the ranged desire/extend
   machinery fabricates custody contention on fully-aligned disjoint
   writes (822 conflicts / 2,367 trims / 9 demotions in one 8-rank row).
   Any fabric-venue s11 row is INVALID until this is fixed — it gates
   fabric acceptance AND deserves its own red-first campaign.
3. **Residual item 6 has its first live capture** (`free_grace_offsets`
   monotone under a write storm, releases never keeping up).
4. **Item 2 (`SQUEEZEFS_RANGE_CUSTODY` default flip)**: preconditions are
   local-venue gates and remain formally reachable, but flipping while
   the first real-fabric venue shows fabricated contention would be
   dishonest without an explicit adjudication — the flip now ALSO reads
   finding 2's fix (or a recorded ruling that localhost-gate green
   suffices) as an input.

## 5. Cost ledger (the cloud arm)

Failed spot launches ×3: $0. Spot session (interrupted): ~$0.30.
On-demand session (row + teardown): ~$1.60. **Total: ~$2.** Teardown
sweeps verified clean after both sessions; nothing billing.
