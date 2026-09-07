# Pause state — 2026-09-06 (reboot into the patched kernel)

`dev` = `origin/dev` = `08483c51`, clean, no unmerged branches; the gate
worktree `~/Source/squeezefs-gate` exists (pinned, detached) and is the ONLY
place `task check` runs from now (the 11:45 red was a rebase in the main
checkout moving HEAD under a running gate). `squeeze-test` is idle with no
mount (the A-arm rig reset the cluster; remount is part of every arm).

## Landed since the last pause (2026-09-05), all fleet- or loom-proven

| item | commit | outcome |
|---|---|---|
| Co-writer ENOSPC wedge | `ac717c7f` | fixed; fleet: watchdog 37k → 2–4, all mounts answer |
| Finding-15 supply leak | `380ea732` | fixed; refusals 3,449 → 148; the shortage moved INTO the free-grace loop |
| Hold-time levers (b) ack-on-renewal, (d) harvest-on-ack | `c1450c66` | bound age 8,994 → 7,936 ms (−12 %, as priced) |
| F15 term 2 — lane-visible push | `8326bb81` | `min_acked→released` 2,500 → 15 ms; hold 10.9 → 7.4 s |
| F15 term 3 — **fenced-close data loss** (a rotated fencing token at epoch close took the genuine-fence discard arm) | `8326bb81` | 0 fenced closes, 0 fsync failures on the fleet; anomalies −94 %; the 154 refused frees attributed (≡ Σ `own_lane_untracked` 156) |
| CQE doorbell lost wake (v3 latch: a reaped-prior completion paid a stale mark) | `08483c51` | loom-red → fixed (mark-valued latch, no parker clear); IPC_ABI stays 6 |
| Kernel COMMIT-lock split | `9edcf624`, `a05998c5` | patches on all THREE tracks (7.2 = 0026, 6.19.14 = 0031, 7.1 = 0031), compile-proven incl. lockdep |
| Kernel A/B, **A arm recorded** | `e37d1932` | un-patched 6.19.14-sqz on squeeze-test, two same-state runs: rand-4k 505–511k IOPS, p50 194 µs, `fuse3-ur` ≈ 23 µs/op, lock ledger 12.6–13.1 % slowpath + 3.3 % raw_spin_lock |
| Bench smoke race (`timeout_cycle`) | `df938b0d` | observes the first-poll race instead of asserting it |

## Open

1. **Batch `task check` on `08483c51`** — NOT yet green on this exact tip:
   the last full launch (11:50, pinned worktree) went red only on the
   doorbell test now fixed; 1,477 tests had passed before it. Run from the
   gate worktree: `bash /tmp/five/gate/gate.sh 08483c51` (the script
   checks out the commit in `~/Source/squeezefs-gate` and writes
   `/tmp/five/gate/taskcheck.{log,exit}`) — or, if `/tmp` did not survive
   the reboot, `cd ~/Source/squeezefs-gate && git checkout --detach
   08483c51 && task check`. ~50 min on a quiet box.
   **DONE — GREEN on `6cbbd848`** (16:14–17:05, pinned worktree, from
   zero): 357 suites / **4,721 tests**, every stage (root + fuse3 clippy
   both configs, fmt, test, doc, bench smoke, loom build, fuzz check,
   docs, audit — the one allowed warning is the adjudicated
   RUSTSEC-2025-0141 `bincode 1.3.3`, rc-manifest §5). The intermediate
   run on `7af1a0d2` went red ONLY in the bench-harness smoke: three
   `record.rs` unit tests raced on the process-global
   `META_KV_DELTA_ORPHANS` (serial in the test stage, parallel in the
   bench harness) — fixed `66a71e35` (a test-module mutex on exactly
   those three; 1/40 → 0/200), product path untouched. Item CLOSED.
1b. **Finding 15 term 1 — DONE on the mechanism, the row still FAILS
   (`.benchmarks/2026-09-06-free-grace-term1-fleet.md`).** User decision
   2026-09-06: all four KD-FG-11 re-derivations landed (`ee49cf04`, items
   1–3 `7ff374ea`, item 4 + the landing-ceiling composition). Fleet, same
   boot, baseline `6cbbd848` vs after: bound age 9,382 → 1,836 ms, member
   ack lag 6.9 → 0.7 s, held offsets 537 → 68, zero fences — and the
   s11-mpiio gate still NOT SUSTAINED (1,649 → 649; lane ENOSPC 50k → 35k).
   The next binding term is UPSTREAM of the ring: displaced blocks park in
   the co-writer's open rewrite epoch until it closes (KD-1.6 — no routine
   trigger fires mid-iteration on a 1.25 GiB slice of a 10 GiB shared
   file), so supply reaches the lanes in bursts with 9–31 s gaps. Owed:
   (1) a per-second time-series sampler on the fleet row (rig change), (2)
   the lever as a USER DECISION — a supply-coupled epoch close on
   co-writers (KD-1.7's early-close made ahead of the StorageFull). Also
   filed: `read_settle_lost_serialized` fires on the authority in both
   rows (a standing must-stay-0 violation on this venue).
   **Gate on the term-1 tip — GREEN on `c60746fb`** (21:20–22:13, pinned
   worktree, from zero): 358 suites / **4,740 tests**, all 18 stages, both
   audits (the one allowed warning = RUSTSEC-2025-0141 `bincode`). The tip
   is releasable as 1.2.2 whenever the user calls it.
1c. **Finding 15, day 2 (2026-09-07) — the S11 gate PASSES; the next
   term is named** (`.benchmarks/2026-09-07-f15-day2-fleet-pair.md`).
   User decision "2 and 3": landed the supply-coupled epoch close
   (`2c0a90c1`), finding 51 (`d3cb44ac` — the authority's
   `read_settle_lost_serialized` storm was its own retired word for a
   recycled co-writer block; 101 fsync EIOs/row), lane-aware placement +
   failover (`149821c7`), the refill-hint gate (`ac56ceb6`, composed
   `b119ef78`), and the per-second fleet sampler rig (`73a133c8`). Fleet
   A/B same boot: phase A1 (the S11 shared file) **steady 1,684 MiB/s over
   89 s — first PASS ever**, lane-ENOSPC −96 %, tripwires 0. Phase B1 (the
   file-per-proc reference, never reached before) FAILS 770 → 148: the
   authority's renewal processing head-of-line blocks behind ~240 mostly
   empty harvest RPCs/s (members' acks fresh, authority's view 10–11 s).
   Next: single-flight harvest per allocator + renewal serve isolation;
   the claim-anomaly lineage (1,620 in B1). Gate on `b119ef78` running.
1d. **squeeze-test A-B-B-A (the deciding row, 2026-09-07 15:09–15:41Z —
   `.benchmarks/2026-09-07-f15-day2-squeeze-test-abba.md`):** A
   `c60746fb` FAILS A1 in positions 1 and 4 (2,087 → 1,062; 2,162 → 919);
   B `b119ef78` PASSES A1 in positions 2 and 3 (steady 3,193 MiB/s / 72 s;
   3,417 / 102 s), tripwires 0, anomalies 0, ENOSPC +0/+656 on m50 vs 116k–
   130k on A. Thermally flat 35 → 39 °C. **The S11 gate is MET on
   b119ef78.** B1 fails on B (6,964 / 11,564 STALE refusals = the
   finding-51 regression + the harvest storm) — three fixes in flight
   (`fix/finding-51-adopt-key-incarnation`, `perf/lane-harvest-single-flight`,
   `fix/membership-renewal-isolation`); the A-B-B-A re-runs on their tip,
   with the per-lever legs.
   **Gate on the day-2 code tip — GREEN on `b119ef78`** (10:47–11:44, pinned
   worktree, from zero): 361 suites / **4,763 tests**, all 18 stages, both
   audits (the two allowed warnings = the rc-manifest §5 adjudicated
   unmaintained notices, `bincode` + `number_prefix`). The docs commits
   since (`5187da15` … `5ef89fcc`) are docs-only.
1e. **B1 term LANDED (`886d4e31`) — gate GREEN** (13:12–14:17, pinned
   worktree, from zero: 362 suites / **4,781 tests**, 18 stages, both
   audits). The three B1 fixes: single-flight lane harvest (`7cf8433b`),
   the finding-51 phase-B1 containment (`23d243a4` — the storm was the
   authority's OWN open overlay records displaced by served publishes, not
   the witness), the renewal cadence's caught-up relax + `sqz-lease-io`
   venue + renew instruments (`61e45918`, `7d5c1958`). Box row 1C
   (`886d4e31`): **the first full s11-mpiio matrix PASS** — A1 3,497 /
   B1 2,324 / B2 2,301 / A2 2,842 MiB/s all sustained, stale refusals 0,
   tripwires 0, fsck clean; `overlay_superseded_by_served_publish` 1,263 /
   1,259 on the fpp phases. Residues: fpp-phase ENOSPC refusals still
   8–15k per phase (sustained anyway), `block_claim_anomalies` 1,156–1,348
   per fpp phase (the own-lane-untracked lineage). Sequence 2 (C-B-B-C +
   six lever legs) running on the box; the overlay ENOSPC-convergence
   flake (pre-existing, ~3–7 % order-dependent) under a subagent.
1f. **squeeze-test sequence 2 (`.benchmarks/2026-09-07-f15-b1-squeeze-test-seq2.md`):**
   C `886d4e31` passes the FULL four-phase matrix in both C positions
   (1C: 3,497/2,324/2,301/2,842; 4C: 3,538/2,361/2,236/3,003 MiB/s), B
   `b119ef78` passes A1 and fails B1 in both B positions; stale refusals
   0, tripwires 0 everywhere. Lever legs: LANE_PLACEMENT load-bearing (A1
   FAILS without it); SUPPLY_CLOSE and CAUGHT_UP_RELAX pay (−19…−24 % /
   −21 % on the affected phases, 5–10× the refusals without them);
   REFILL_HINT / SINGLE_FLIGHT / RENEW_LANE throughput-par (refusal / RPC
   economy). Row 8C degenerate (probe on a busy box — the wrapper now
   settles first). Residues: `block_claim_anomalies` 1.2–1.4k per fpp
   phase; fpp lane-ENOSPC 7–16k per phase. Overlay ENOSPC-convergence
   flake FIXED (`2a456a62`, a real trim-claim-window race); gate on
   `2a456a62` running.
2. **Kernel A/B, B arm** — after `squeeze-test` boots the 6.19.14 series
   WITH 0031 (or whichever box carries the patched kernel): on the box,
   `cd /scratch/tmp/sqz-agent/k26 && sudo env ARM=B KERNEL_TAG=<uname -r
   substring> BIN=/scratch/tmp/squeezefs IL=/scratch/tmp/libsqueezefs_il.so
   bash 2026-09-06-kernel-bg-per-queue-ab.sh` twice (A A B B; the rig
   refuses a wrong-arm kernel via `fuse_uring_bg_wait` in the module
   symbols — `SYMCHECK=advisory` on an LTO kernel). Same 1.2.1 dist
   binary as the A arm. Verdict columns: rand-4k IOPS vs 505–511k, p50 vs
   194 µs, `fuse3-ur` µs/op vs 23, `lock.txt` slowpath share vs 12.6–13.1 %.
   If the patched kernel is the LAPTOP's (7.1/7.2 track), the laptop needs
   its own A arm first — it was not recorded (the rig's venue is the box).
   **Update (same day, after the laptop reboot):** the laptop is on
   `7.2.3-cachyos-lto` WITH the 7.2 patch — the patch's first runtime is
   recorded as `docs/design-kernel-bg-per-queue.md` §5a (zc gate 149/149
   both postures, a 779k-IOPS rand-4k load row, every tripwire 0, zero
   fuse/uring kernel-log lines). The laptop cannot be an A/B venue (its
   kernel moved 7.1.8 → 7.2.3 in the same boot); the B arm stays the box's.
   **DONE (same day, 19:51–20:05 UTC):** both B boots on squeeze-test —
   `.benchmarks/2026-09-06-kernel-bg-per-queue-ab.md`, **B ships**
   (worker −18 % µs/op, slowpath 12.8 → 0.11 %, kern rand-4k +9 %, p99
   −13…−17 %, controls par, no WARN). Item CLOSED.
3. **Finding 15 term 1 — the user's decision**: the remaining 7.0 s of
   hold are the reader ack ladder's two derived coherence windows (qualify
   ≈ 2 s, drain ≈ 4 s, × min over 8 members). Four KD-FG-11 items
   (`.benchmarks/2026-09-06-free-grace-hold-time.md` §7) would cut
   ≈ 3–3.5 s at the same safety and put 4 GiB lanes at ≈ 50 % headroom;
   until then the s11-mpiio sustained gate stays FAIL by design.
4. **1.2.2** — five multi-writer data-path correctness fixes since 1.2.1
   (wedge, leak, fenced-close data loss, doorbell lost wake, plus the
   levers); strongly warranted once (1) is green: release gate from zero in
   a pinned worktree (`/tmp/release-1.2.1/driver.sh` pattern + the zc leg
   both postures), tag `stable-2026.09.2`, `dist:all` in the pinned
   worktree, ship.
5. Smaller named items: the `own_lane_untracked` refused-free lineage
   (156/row; a stale layout refetch on the non-recomputed paths); the ~38
   no-fence anomaly residue; the co-writer fsync wait behind the 1 s park
   wall on cache-less mounts (term 2 note §7); the two-parker cross-pending
   doorbell strand (pre-existing, needs an ABI-bumping second word).

## Standing hazards

- Check `nproc` = 32 and `/sys/devices/system/cpu/offline` empty after
  the reboot (the 09-04 latch).
- Gates and `dist` builds ONLY in pinned worktrees; measurement rows never
  concurrent with a gate; assert the TEST's exit status when chaining a
  landing; `skip_ledger_tests` needs `--test-threads=1`.
