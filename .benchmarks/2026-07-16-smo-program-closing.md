# SMO replay-currency program — closing acceptance (PR 5, 2026-07-16)

**Branch** `docs/smo-program-closure` off dev `9f4bf2c`. **Charter**: the PR 5
row of `docs/design-smo-replay-currency.md` (now **Status: Implemented**) —
the program's acceptance gate, i.e. the FIND-VS-A charter's STOP-condition
inverse widened by the §1b signature: the storm shape green **×10
consecutive on the final program binary**, asserting **JOINTLY per round**
`acked-loss == 0 AND mount-refusals == 0 AND replay_dropped_torn == 0`
(never the counter alone — the §1b newest-entry edge can read 0 over a
loss). Program branches under gate: C′ two-phase replay
(`0b6555d..89ea158`), FIND-SMO-TAIL entry-start floors (`5a82ff7..f41a0eb`),
Option A coverage-gated pending-free (`0377431..9f4bf2c`). Closure commits:
`4d2ddef` (acceptance-soak harness,
`.agents/findvsa/smo_acceptance_soak.sh`), `5176399` (normative doc deltas),
this note.

Rails: unique sandboxes `~/tmp/smo_accept_{full,t016}_*` (fresh mountpoint
per round — armor against the Branch-3 stale-mountpoint harness hiccup);
kills by PID; systemd-run --user scope cages (`MemoryMax=16G`);
`/mnt/squeezefs`, `/mnt/juicefs`, `~/tmp/nvme`, redis untouched; per-round
thermal gate (Tctl < 88 °C required; observed peak ≈ 59 °C, R3 rows).

---

## THE acceptance soak — ×10 joint, both masks, zero failures

Binary: release @ dev `9f4bf2c` (all three fix branches in; the closure
branch adds no code). Shape per round: `recapture.sh` round body — 16-worker
acked-create storm (`storm_creator.py`, ack after `close()` returns),
`kill -9` at peak (`KILL_AFTER=4`), page-cache-coherent pre-remount image
capture, in-place remount, audit. 5 rounds on the **full CPU mask** + 5
rounds under **`taskset -c 0-15`** (FIND-VS-B precedent). Joint assertion
per round; multi-run discipline armed (any failure ⇒ STOP + preserve + new
finding) — **never fired: 10/10 first count, no restarts**.

| # | mask | acked creates | missing (acked-loss) | refusals | dropped_torn | replay entries /vol | replay ms /vol | pending_free post-remount |
|---|---|---:|---:|---:|---|---|---|---|
| 1 | full | 68,192 | **0** | 0 | **[0,0,0,0]** | 2371/5366/1573/1833 | 13/22/9/11 | **[0,0,0,0]** |
| 2 | full | 67,629 | **0** | 0 | **[0,0,0,0]** | 3576/2024/2169/3509 | 19/12/11/17 | **[0,0,0,0]** |
| 3 | full | 67,594 | **0** | 0 | **[0,0,0,0]** | 5147/2437/4580/5509 | 25/15/20/22 | **[0,0,0,0]** |
| 4 | full | 70,524 | **0** | 0 | **[0,0,0,0]** | 5177/7033/2885/5623 | 26/26/13/21 | **[0,0,0,0]** |
| 5 | full | 66,145 | **0** | 0 | **[0,0,0,0]** | 3705/2009/4508/6372 | 19/12/19/24 | **[0,0,0,0]** |
| 6 | 0-15 | 61,247 | **0** | 0 | **[0,0,0,0]** | 1489/2285/5643/5130 | 9/9/21/19 | **[0,0,0,0]** |
| 7 | 0-15 | 62,796 | **0** | 0 | **[0,0,0,0]** | 2824/5494/2902/6401 | 15/20/13/22 | **[0,0,0,0]** |
| 8 | 0-15 | 61,321 | **0** | 0 | **[0,0,0,0]** | 3322/1183/3228/2115 | 16/7/13/10 | **[0,0,0,0]** |
| 9 | 0-15 | 62,869 | **0** | 0 | **[0,0,0,0]** | 6257/3751/2986/5647 | 27/16/14/21 | **[0,0,0,0]** |
| 10 | 0-15 | 63,346 | **0** | 0 | **[0,0,0,0]** | 7224/4594/4162/4491 | 31/17/17/16 | **[0,0,0,0]** |

**651,663 acked creates across the ten kills — zero lost, zero remount
refusals, zero torn-drops.** Windows were non-trivial every round
(1,183–7,224 replayed entries per volume; 9,848–20,718 aggregate per
round), i.e. every remount exercised the two-phase replay over real SMO
traffic, not drained tails. `meta_kv_pending_free` read **[0,0,0,0]** at
the first post-remount sample every round (parked window frees drained by
the first post-mount durable checkpoints — the Branch 3 mount-gate
contract; round-1 counters for scale: `pending_free_parked` 260 /
`released` 188, the delta being preflight-open parks per the Branch 3
gauge-vs-counter note). Baseline for contrast: dev `44d14d6` hit
**311/67,887 lost (0.46 %)** on round 1 of its capture loop.

## Companion soaks

- **`churn_unmount_soak.sh` 10** (the FIND-M11-A shape: mount → 1,500-file
  create/write/unlink churn → clean unmount): **10/10 clean** — no SIGBUS,
  no daemon residue, no coredumps.
- **At-cap liveness + wedged-tail loud-fail** re-confirmed on the tip via
  the gate's test leg (they are cargo tests):
  `pending_free_at_cap_forced_cycle_completes_and_conserves_extents ... ok`,
  `pending_free_wedged_tail_fails_volume_loud_never_livelocks ... ok` —
  alongside the rest of the program's contract set, all green:
  `stranded_build_window_commits_survive_crash`,
  `replay_twice_digest_stable_across_smo_windows`,
  `recaptured_window_pins_the_stranding_signature`,
  `mid_entry_tail_multi_leaf_tx_partial_flush_survives_crash`,
  `root_swap_freed_extent_stays_parked_until_tail_covers_free`,
  `mount_side_replayed_free_parks_until_post_mount_checkpoint`,
  `recaptured_window_pins_generation_gate_release`.

## fstests QUICK tier

`FSTESTS_QUICK=1 sudo tests/run_fstests.sh` — 19 curated cases, result
**matches the expected table exactly**:

| Class | Cases | Expected? |
|---|---|---|
| PASS | 001, 008, 013, 069, 074, 075, 091, 112, 127, 263, 285, 469, 616, 617, 618 (15) | ✓ |
| FAIL (platform, documented in `run_fstests.sh`) | **003** (FUSE attr-cache atime semantics), **213** (thin-provisioning fallocate) | ✓ — the standing platform rows |
| NOTRUN (platform) | **009**, **316** (`xfs_io fiemap` unsupported) | ✓ |

Nothing outside the expected table — no investigation owed.

## R3 scoreboard partial (rows stay W)

`SQUEEZEFS_VS_REGIMES=R3
SQUEEZEFS_VS_ALLOW_LOSS="R3.seq_write_1m,R3.rand_write_4k"
tests/run_vs_juicefs.sh` @ `5176399` (artifacts
`/var/tmp/squeezefs_vs_juicefs/artifacts/20260716T153337Z/`): **GATE:
GREEN — no loss rows**.

| Row | JFS | SQZ | SQZ/JFS | Verdict |
|---|---:|---:|---:|:--:|
| R3.seq_write_1m (MiB/s) | 11,735 | 4,182 | 0.36× | L (allowed — scoreboard Loss 1, page-cache-ACK class) |
| R3.seq_read_1m (MiB/s) | 6,566 | 6,583 | 1.00× | TIE |
| R3.rand_read_4k (IOPS) | 122,759 | 254,810 | 2.08× | **W** |
| R3.rand_write_4k (IOPS) | 4,319 | 373 | 0.09× | L (allowed — scoreboard Loss 2, standing rand-write family) |
| R3.stat_storm (files/s) | 119,704 | 352,059 | **2.94×** | **W** |
| R3.del_storm (files/s) | 3,424 | 37,234 | **10.87×** | **W** |

The two formerly-INVALID rows (the FIND-VS-A casualties, scoreboard Loss 3)
**stay W** at the same magnitudes as the `e8a90f4` re-run (2.91×/10.65×);
the cold protocol's unmount → drop → remount chain crossed the program's
machinery ten more times without incident.

## The program, before → after

| Failure class (kill -9 at create-storm peak) | Before (dev `44d14d6`) | After (dev `9f4bf2c`, this gate) |
|---|---|---|
| (i) build-window stranding — acked creates lost across a **clean** replay (`dropped_torn == 0`) | **0.4–0.9 %** of acked (captured: 311/67,887 = 0.46 %; 192/311 direct flip signature) | **0** in 651,663 acked ×10 kills (C′ two-phase replay) |
| (ii) recycled-extent stale routes — `child_node_seq` mount **refusal** (+ silent 31 % early frees: 71/231 window frees released by the old generation gate, round-8 images) | reproduced on unmodified dev | **0** refusals; mount gate parks **100 %** of window frees until post-mount coverage (Option A) |
| §1b mid-entry tails — `dropped_torn ≥ 1` + collateral acked drops | live at ~1-in-3 storm rounds post-C′ (Branch 1 round 3: `[1,1,0,0]`) | **[0,0,0,0]** ×10 (entry-start floors, PR 3) |

The child-seq refusal class is now a **true corruption detector** (kept
loud, zero tolerance); `dropped_torn > 0` after a *clean* unmount stays the
corruption alert it always was.

## Cargo gate (tip, all three closure commits in the tree)

- `cargo clippy --all-targets --all-features -- -D warnings` — clean.
- `cargo fmt --check` — clean.
- `cargo test --all-features -- --test-threads=1` — **856 passed / 0
  failed** (exit 0; same count as the Branch 3 merge — the closure branch
  adds no code).
- `cargo doc --no-deps` — clean. `cargo bench --benches -- --test` — smoke
  green.
- loom: untouched by this branch (last run 25/25 at Branch 3's
  `3cf096d`, incl. the re-clocked `alloc_ext_core` coverage-gate models).

## Doc deltas landed with this closure (`5176399`)

- `docs/design-cow-kv-metadata.md` §4.6 pt 2: floors round DOWN to
  journal-entry starts — "tail is an entry boundary" is an enforced
  invariant; §4.6 SMO protocol: the two-phase unclean-shutdown replay
  bullet (routing before content, root-swap carve-out → §4.7 gate);
  §4.7 CoW reuse rule: coverage-gated pending-free (release on durable tail
  > free-record seq; mount-side LWW-first parking; at-cap force-cycle with
  bounded-retry-then-loud) + the `meta_kv_pending_free_{parked,released}`
  counters; §4.10: the SMO-window closure note.
- `docs/design-smo-replay-currency.md`: Status Approved → **Implemented**,
  with the per-branch SHA table.
- `AGENTS.md`: `meta_kv_pending_free_{parked,released}` added to the v3
  stats list (the only counters the program added).

## Residuals

- None chartered by this program remain open. Standing adjacent items,
  unchanged and tracked elsewhere: the two allowed R3 scoreboard losses
  (seq-write page-cache-ACK semantics; the rand-write-4k family — both
  pre-date the program, `.benchmarks/2026-07-15-vs-juicefs-scoreboard.md`);
  the fstests {003, 213} platform rows; C′'s accepted cache-litter note
  (stranded predecessor *objects* stay mapped-but-unrouted until clock
  eviction — bytes only, bounded by the window's SMO count, stated in the
  backend.rs 5b comment block); §4.10's un-acked-hole caveat (by design).
- Artifacts: per-round logs/stats preserved under
  `~/tmp/smo_accept_full_1784215156/` + `~/tmp/smo_accept_t016_1784215290/`
  (passing rounds' image captures deleted by harness design; regeneration
  is one `smo_acceptance_soak.sh` run). Branch 1/3 raw capture images
  remain at `~/tmp/smo_b1_1051210/capture_round1/` +
  `~/tmp/smo_b3_1493772/capture_round8/`.

**Verdict: the SMO crash-contract program is CLOSED** — an acked tx is
reachable after replay, period (§4.10 law), now held across SMO windows,
mid-entry tails, and extent reuse, at storm scale, on both masks, ×10.
