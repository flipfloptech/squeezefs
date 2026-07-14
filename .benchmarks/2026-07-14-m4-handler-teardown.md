# PR M4 acceptance — handler teardown: watchdog timeouts, alloc & cache-maintenance slimming

| | |
|---|---|
| **Program** | metadata-throughput (`docs/design-metadata-throughput.md`), PR M4 (§5.1 D1.b + D1.c) |
| **Branch** | `perf/fuse-handler-teardown` @ `6f54a89` (off dev `3c9eda4`) |
| **Box / rails** | same 3.5 GHz-capped box as the baseline/M2; storms quiet-gated (3-poll 45 s streak, Tctl < 80 °C, post-phase DIRTY re-check — **zero DIRTY rows in the accepted session**), daemons caged (`systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=0`), binaries named `sqm4`/`sqm4dev` (kill-pattern immunity), kills by PID only |
| **Substrate** | file-backed sandbox on btrfs-CoW home fs (`~/tmp/m4_accept_31095/`) ≈ baseline **A cow** class, default cadence — the sanctioned substrate for non-barrier throughput work |
| **Shape** | mdstorm 8 threads × 100 k per phase, one dir (`create → stat → rename → unlink`) + many-dirs (`mfcreate`/`mfunlink`, 8 × t\<w\> dirs); fresh volume per set; dev/M4 interleaved; median of 3 paired runs; rig OFF for headline rows; one rig-ON leg per side; separate untimed perf legs |

## Incident note: power-loss restart (multi-run discipline)

The first acceptance session (sandbox `~/tmp/m4_teardown_3081068/`) was cut off
mid-run by a **laptop battery death / host reboot (2026-07-14 13:43)**. Its
partial A/B rows are **VOID** per the multi-run discipline (attributable
interruption ⇒ restart): they read 4.5–5.1 k creates/s on *both* sides with
DIRTY markers — battery-throttled garbage well below the M2 band. This report's
session was restarted from scratch post-reboot (CPU cap re-verified 3.5 GHz,
Tctl 48 °C at gate, no stale mounts/daemons) with **freshly rebuilt** binaries
for both sides (dev control rebuilt in a clean worktree @ `3c9eda4`).

## Verdicts up front

1. **Timed gates hold.** One-dir create **+0.7 %** with clean separation (every
   M4 run ≥ every dev run); many-dirs create **+2.6 %** and the ≥ 27 k/s context
   floor holds (all M4 runs ≥ 27.9 k/s). No row regresses beyond overlapping
   noise (rename −1.2 % median with fully overlapping spreads — M4 holds both
   the session min *and* max).
2. **The vdso share did NOT collapse to ~0 — and the attribution says why.**
   `__vdso_clock_gettime`: dev 2.41 % → M4 **2.21 %** (−0.2 pp). The per-op
   `timeout()` wrappers are gone (all 16, code-verified), but the profile shows
   the clock/timer population was never majority-owned by them: the **fuse3
   transport's per-pull `pop_timeout`** (`recv_inbound_timeout` →
   `InboundQueue::pop_timeout`, `fuse_over_uring.rs:285/643` — arms a tokio
   timer per inbound request pull) plus the tokio **time-driver park path**
   (`process_at_time` 0.68 %/0.72 %, driver total 1.31 %/1.30 % — equal on both
   sides) survive M4 untouched, because the transport is **M3/M10 scope** and
   M4's Files row never touches `third_party/fuse3`. The design's own word was
   "2.75 % … is the measured **ceiling** of the win", and the measured handler
   share of it is ~0.2 pp. D1.b's direct proof therefore rests on (a) the
   deleted wrappers + zero-clock-read registry path (code + pinned contract
   tests), (b) the phase move below, (c) the timed rows, and (d) the
   correctness half (the no-drop commit pin — the M4→M7 load-bearing edge).
   **Hand-off**: the residual per-op timer belongs to the transport PRs; noted
   for M10's session.
3. **Correctness/wedge net clean.** `fuse_op_watchdog_overdue = 0` across every
   session; 10× mount→storm→unmount soak: all daemons exited bounded, no
   leftover mounts, no watchdog wedge signatures; fstests `generic/013` +
   `generic/001` pass.

## mdstorm rows (rig OFF, median of 3 paired runs, zero DIRTY)

| phase | dev @3c9eda4 | M4 @6f54a89 | Δ | runs dev → M4 (sorted) |
|---|---:|---:|---:|---|
| create (one-dir) | 6,669 | **6,717** | **+0.7 %** | 6632/6669/6680 → 6700/6717/6782 |
| stat | 237,900 | 250,057 | +5.1 % | 232633/237900/267818 → 233679/250057/271666 |
| rename | 7,034 | 6,948 | −1.2 % | 6997/7034/7038 → 6868/6948/7185 (overlap: M4 holds min+max) |
| unlink | 4,221 | 4,289 | +1.6 % | 4073/4221/4231 → 4070/4289/4343 |
| **mfcreate (many-dirs)** | 27,961 | **28,683** | **+2.6 %** | 27704/27961/28321 → 27948/28683/28965 — **≥ 27 k floor holds** |
| mfunlink | 21,227 | 21,619 | +1.8 % | 20374/21227/21582 → 21401/21619/21732 |

Dev rows sit at/above the M2 band (5,868–6,490 A-cow) — the box is healthy;
the voided pre-reboot rows (~4.7–5.1 k) were throttle artifacts as suspected.

Counter shapes unchanged by design (M4 is teardown, not round-trip/entry
economy): `fuse_ops/create` 5.165 both sides, `meta_kv_journal_entries/create`
1.0054/1.0055, `meta_updates/op` 1.000 — the M5/M6 targets are untouched.

## Perf attribution (untimed legs, 150 k-create storm, 8 s window, F997 dwarf)

| symbol family | dev | M4 |
|---|---:|---:|
| `__vdso_clock_gettime` | 2.41 % | **2.21 %** |
| `_rjem_malloc` (top jemalloc symbol) | 2.31 % | 2.35 % |
| jemalloc total (`_rjem_*`) | 5.06 % | 5.42 % (flat within sampling noise; D1.c's alloc cuts are per-record staging + key-alloc removals — the storm's alloc body is engine-side node/bset churn, D7 territory) |
| tokio time driver total | 1.31 % | 1.30 % (owner: transport `pop_timeout` per-pull + driver park — see verdict 2) |
| `Timespec::now` | 0.30 % | 0.20 % |
| total cycles in window | 33.76 G | 33.55 G (−0.6 % at equal ops rate: 6,375 vs 6,343 ops/s untimed) |

## Phase-histogram move (rig-ON leg, `SQUEEZEFS_OP_PROFILE=1`, n = 100 k/side)

Bucket-geometric medians are stable at bucket resolution (power-of-2 buckets;
a ~1 µs/op cut cannot move a 64–128 µs median): create total 90.5 µs median
both sides, under-`i_rwsem` estimator median 64–128 µs bucket both sides —
create's wall stays ~entirely inside `backend.create` exactly as M2 measured
(the body belongs to D5/D7). The mean-sensitive phase that D1.b/D1.c touch did
move: **create `backend_to_reply` mean 3.0 → 2.4 µs** (reply-side teardown:
timeout scope exit + metadata-cache seed + dir-entry invalidate all died).
getattr/flush/release/lookup phases byte-identical medians. Rig-ON throughput
6,519 (dev) vs 6,522 (M4) — the rig costs the same on both sides.

## Correctness / wedge regression net

- **fstests (targeted, per the M4 verify row)**: `generic/013` **pass** (4 s),
  `generic/001` **pass** (2 s) — fsstress metadata churn + create/rename chains.
- **Unmount soak** (watchdog-semantics wedge net): 10 × (mount → 8-thread
  create/rename/unlink storm 20 k each → immediate unmount). All 10: unmount
  completed bounded, daemon PID exited < 60 s, no leftover fuse mounts, no
  `FUSE op watchdog:` overdue lines, `fuse_op_watchdog_overdue = 0`.
- **Cargo gate at tip**: clippy `-D warnings` clean, fmt clean, full
  `--all-features --test-threads=1` suite green (80 binaries, run twice), doc
  `--no-deps` **zero warnings** (three `private_intra_doc_links` warnings found
  during takeover verification were fixed in `6f54a89` — comment-only), bench
  smoke green.

## Scope-item disposition (takeover audit)

All §5.1 D1.b + D1.c + M4-row items verified present: await-disposition audit
(in-code table, `fuse_client.rs` module header), watchdog registry + 5 s scan
task replacing all 16 per-op `timeout()` wrappers (deleted, not vestigial — the
only surviving `tokio::time::timeout` uses in `fuse_client.rs` are the audit's
KEPT bounded waits: destroy drain + reclaim gather window), bounded-barrier
class (`SyncCoalescer::barrier_bounded`), ring-admission park escalation
(`backend.rs` commit_tx step 2: bounded parks, cumulative ≥ `SQUEEZEFS_TIMEOUT`
→ `note_journal_failure` → latch → `disabled_volumes`), no-drop commit pin
(`slow_commit_completes_and_watchdog_logs_instead_of_timeout_drop`), ino-keyed
router metadata cache (`Cache<u64, CachedMetadata>`), parent generation
counters (`dir_gen` scc map; per-op moka `invalidate` calls gone from all
mutate paths — readdir snapshots key on `(parent, gen)`), single-copy record
staging (`encode_parts` + `Vec::from(Bytes)` reclaim). **Metrics-stamp
consolidation: intentionally not done** — the design conditions it on "where
the profile shows them", and M2's phase split showed handler glue (where the
stamps live) at 0.7–1.4 µs against a 90 µs backend body; there was no measured
population to collapse. Judged unnecessary by measurement.

## Artifacts

Session sandbox `~/tmp/m4_accept_31095/` (`results.tsv` — every row reproduced
verbatim in the tables above — `rig_on.tsv`, per-phase `.stats` snapshots,
`attrib_{dev,m4}/perf.data`, daemon + soak logs) and the harness scripts
(`session_lib.sh` inherited from the voided session, `m4_soak.sh`,
`m4_rig_on.sh`) were session-local, not repo material, and were removed after
this report was recorded — as were the voided pre-reboot sandbox
`~/tmp/m4_teardown_3081068/` and the dev-control worktree
`~/tmp/m4_devtip_worktree/`. The shared mdstorm harness
(`~/tmp/mdbase_20260714/`) is untouched.
