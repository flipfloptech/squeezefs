# 2026-08-16 — MW rung 10b: the Phase-B DEFAULT FLIP (`feat/mw-default-format`)

**What landed:** `squeezefs format` stamps the nine multi-writer incompat bits
(7,8,9,10,11,12,13,14,15 — `MULTI_WRITER_FORMAT_BITS`) **by default**, in the
one planned superblock write (KD-MW-1, one act, never piecemeal);
**`--single-writer`** is the explicit opt-out that formats the pre-flip
unstamped class byte-identically; **`--multi-writer`** survives accepted and
announced-inert; the contradictory pair refuses loud (clap `conflicts_with` —
the refusal precedes any write). User ruling 2026-08-15, verbatim intent:
"when multi-writer is supported we shouldn't need '--multi-writer' — that
should just be the default; if anything '--single-writer' would be preferred
in some instances (maybe fixing the filesystem or recovery efforts)."
Design: `docs/design-full-multi-writer.md` §6.2 pt 1 Phase B + PR-plan
row 10b. **No runtime code path changed** — the flip is the format default
and its pins/docs only (the solo-perf non-negotiable held by construction).

## Gate inputs (all green at dev `9b959343`, cited per the charter)

| Input | Evidence |
|---|---|
| §6.3 S4 re-gate (stamped-solo perf/behavior parity) | `.benchmarks/2026-08-15-mw-s4-regate.md` |
| S4 residuals closed (mdstorm / scoreboard-smoke / QUICK set within-noise/green stamped) | `.benchmarks/2026-08-16-mw-s4-residuals.md` |
| Rung 10 S9 acceptance (fan-out, failover, co-located fencing) | `.benchmarks/2026-08-16-mw-s9-arm.md` |
| S9 finding #6 landed (shipped-publish era gate + idempotence witness) | `.benchmarks/2026-08-16-mw-publish-era-gate.md` |

## The flip's contents (commit list, branch `feat/mw-default-format` off dev `9b959343`)

| Commit | What |
|---|---|
| `b3aac379` | test(mw): the five red pins (all red against dev) |
| `377c76cb` | feat(mw): the flip — library default + `format_v3{,_stamped}_single_writer`, CLI `--single-writer`/inert `--multi-writer`/conflict refusal, add-meta uniformity both directions, first drift-pin wave (`_multi_writer` builder variants deleted — no dead code) |
| `1f2c5cb1` | test(mw): rigs — `mw_fleet.sh` formats bare, mdstorm A-B-B-A legs re-keyed (`--single-writer` = the unstamped legs), format-args lever docs |
| `c73b650d` | docs(mw): operations.md format section + stale "nothing stamps (D9)" claims, QUICKSTART note, design row 10b LANDED annotation |
| `e104121c` | test(mw): `data_path_correctness` reused-key purge pin → single-writer class (bit 13 retires bare-offset key reuse) |
| `a10dd06d` | test(mw): audit wave 2 — kill-9 matrix single volume identity (C8 oracle now ENGAGED across the matrix), delta-economy suite pins the raw form, mdstorm exec bit |
| `b663cd16` | test(mw): unused-import clippy fix |
| (this note + final annotations) | docs(mw): evidence note |

## Pin list (red-first where the charter names it)

- `tests/mw_stamping_tests.rs`
  - `format_default_stamps_all_nine_and_single_writer_stays_the_preflip_posture`
    — the library equality pin: default = single-writer word | the nine-bit
    mask, EXACTLY (single-writer word `0xd7` = bits 0/1/2/4/6/7; default
    `0xffd7`); the 7..=15 mask identity re-pinned.
  - `phase_b_cli::cli_default_format_stamps_all_nine_and_announces_the_class` (RED first)
  - `phase_b_cli::cli_single_writer_formats_the_unstamped_class` (RED first)
  - `phase_b_cli::cli_single_writer_plus_multi_writer_refuses_loud` (RED first —
    nonzero exit, stderr names BOTH flags, sector 0 untouched)
  - `phase_b_cli::cli_multi_writer_is_accepted_and_announced_inert` (RED first)
  - the enable-verb/crash-window/interaction-rule suites re-based onto
    `format_v3_stamped_single_writer` (the class the verb exists to upgrade).
- `tests/mw_ino_lane_tests.rs::the_multi_writer_format_bits_are_disjoint_and_class_scoped`
  — the bit ledger now asserts BOTH classes (default stamps 12/13,
  single-writer stamps neither).
- Intent-audited suites moved to the explicit single-writer API (they exercise
  one bit IN ISOLATION or the unstamped class itself):
  `mw_ino_lane_tests`, `mw_block_key_incarnation_tests`,
  `mw_layout_version_tests`, `kv_partitioned_append_tests` (sector-0
  byte-identity), `writer_scoped_staging_tests` (unscoped/unanimity arms),
  `dlm_membership_tests` (claim-set projection arms),
  `dlm_multi_writer_tests` (the unstamped-refusal repro; `sandbox` base),
  `data_path_correctness_tests` (ONE test: bare-offset key-reuse purge),
  `write_commit_economy_tests` (raw version-less delta form).
  Every other formatting suite **inherits the flip** (runs the stamped class).
  No refusal pin was weakened; every refusal arm still runs, now against the
  explicitly-formatted unstamped class.

## Audit catches worth recording

1. **`data_path_correctness_tests::test_write_through_reused_key_purges_stale_read_tiers`**
   — precondition "the allocator must reuse the freed offset('s key string)"
   is exactly what bit 13 (incarnation keys) retires on the stamped class:
   the pin rides the single-writer class; the stamped class's no-reuse
   protection is `mw_block_key_incarnation_tests`.
2. **`overlay_overwrite_tests::kill9_crash_matrix_ow1_to_ow7`** — the
   per-session allocator ids (`…_s1`/`…_s2`) changed the volume identity
   across the simulated remount (impossible in production — KD-5), which made
   the C8 oracle diverge by construction once bit 9 stamped by default. Fixed
   to ONE identity per case; the matrix now runs the C8 durable-ref oracle
   ENGAGED across every kill window — coverage the unstamped matrix never had.
3. **`write_commit_economy_tests`** — drives `merge_layout_and_size` directly
   with hand-built version-LESS deltas; on a bit-15 volume the first-touch
   version re-base shifts the pinned delta cadence. Pinned to the raw
   (single-writer) form; the composed stamped economy is
   `mw_layout_version_tests::versioning_adds_no_entries_and_exactly_16_bytes_per_delta`.

## Live sanity (release build, default features, this box)

- **(a) bare `format` + solo mount** (file-backed meta/data + staging):
  superblock word `0xffd7` (all nine stamped); announce line names the class
  and the opt-out; mount serves I/O; stats: `dlm_mode=solo`, **`dlm_rpcs=0`**,
  `mount_posture=writer`, `writer_guard_mode=flock+claim`, `dlm_term=1`,
  `alloc_lane_writers=0` (solo installs no partition); bit-9 ledger LIVE
  (`meta_kv_block_refs_staged=3` after an 8 MiB O_DIRECT write,
  `meta_kv_block_refs_drift=0`); clean unmount.
- **(b) `--single-writer` format + mount**: superblock word `0xd7` (the
  pre-flip posture, none of the nine beyond bit 7); announce line names the
  class + the upgrade verb; mounts and serves; `dlm_rpcs=0`.
- **(c) fleet rig WITHOUT the flag**:
  `sudo SQZ_BIN=$PWD/target/release/squeezefs tests/mw_fleet.sh create 2
  --membership --cowriters=1` — the rig's format line is now bare (default
  class); `--cowriters` implies the ARM as before. Result: writer
  (`data_plane_fence_mode=1`, WERO held, membership owner) + reader + a
  CO-WRITER admitted (`mount_posture=co-writer`); a co-writer write read back
  through the writer's mount; `teardown` → "zero residue".

## Gates

- Red pins: all five RED against dev 9b959343, GREEN after the flip.
- `cargo clippy --all-targets --all-features -- -D warnings` — clean.
- `cargo clippy --all-targets -- -D warnings` (shipped config) — clean.
- `cargo fmt --check` — clean.
- shellcheck on touched scripts — touched lines clean (the standing
  SC2318/SC2034 pair pre-exists on dev, verified against dev's copies).
- markdown link/anchor check — PASS (251 files, 0 broken).
- **Full serial cargo suite** (`cargo test --all-features --no-fail-fast --
  --test-threads=1`, from zero on the final tree): all suites green except
  the ONE documented pre-existing failure below. (Full `task check` stays
  DEFERRED per the rung charter.)

## Residuals

1. **Pre-existing (NOT flip-caused), owed a fix on its own branch:**
   `fsync_durability_contract_tests::test_data_barrier_precedes_the_metadata_barrier`
   fails deterministically **on dev `9b959343` itself** on this box (verified
   by detached checkout: same failure shape, `meta_device_syncs` advances by 2
   despite the faulted data barrier; the suite formats via raw `ImageBuilder`,
   untouched by the flip). Needs its own red-first investigation.
2. AGENTS.md still carries pre-flip "built and never stamped (D9)" phrasings
   (bit 9/11/14 sections) plus the standing bit-numbering staleness — rung 19's
   charter (`docs/design-full-multi-writer.md` row 19) owns those corrections;
   no AGENTS.md format row names the flag, so rung 10b's docs act deliberately
   did not touch it.
3. The env-knob purpose strings for `SQUEEZEFS_TEST_STAMP_{BLOCK_REFS,WRITER_SCOPE}`
   were corrected (bit numbering + the flip); the seams themselves remain
   useful for engaging one bit in isolation on a `--single-writer` base.
4. Instrument note: live sanity ran while the serial suite loaded the box; the
   `umount` in leg (a) needed the direct-unmount fallback (SIGTERM timeout
   under load) — the unmount completed and leg (b) + the fleet teardown were
   clean; not attributed to the flip (format-class-only change), worth an eye
   on the next quiet-box run.
