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
   **Gate on the trim-window tip — GREEN on `2a456a62`** (14:53–15:43,
   pinned worktree, from zero, laptop idle: 362 suites / **4,782 tests**,
   18 stages, both audits). Its first launch went red on the C-2
   structural timing contract because my overlay-suite loop ran beside it
   — the standing "no measurement beside a gate" hazard, not the code.
   Every code landing of 2026-09-06/07 is now gated; the tip is
   releasable as 1.2.2.
1g. **Residues (user pick) — landed, box-adjudicated, NOT closed**
   (`.benchmarks/2026-09-07-f15-residues-squeeze-test-seq3.md`): D
   `0bd03455` = claim-anomaly lineage fix (reply carries the freed
   offsets, publish schema 15, `89cb60b6`) + per-volume fpp supply (close
   per volume, per-volume lane hint on the grant at `CLUSTER_WIRE_SCHEMA`
   3, `lane_allocators` dedup, `0bd03455`). D-C-C-D: every row passes the
   FULL matrix (tripwires 0, fsck clean); the mechanisms engage
   (`recomputed_retires` 5.5–21k/phase, `volume_hint_skips`, 15–26k
   per-volume close blocks) but `block_claim_anomalies` (1,108 / 1,117 on
   D's fpp phases vs 1,035–1,478 on C) and the park slices (20k / 9k /
   26k vs 12k / 11–14k / 13–18k) did NOT move. Both investigations RESUMED
   against the per-mount snapshots (`fix/cowriter-claim-anomaly-population`,
   `perf/cowriter-fpp-supply-reattribution`). Row 4D degenerate (probe
   235 MiB/s — a 4.4 s cold start); the matrix probe now runs 2 iterations
   and sizes from the warm one. Gate on `0bd03455` running.
1h. **Residues, round 2 — CLOSED on the box** (`.benchmarks/2026-09-08-f15-residues-squeeze-test-seq4.md`):
   E `96f8d873` = the lane-free notice channel (every reply frame carries
   the authority's frees of the client's blocks — the population was the
   AUTHORITY's OWN publishes: a co-writer's first-iteration kernel-split
   segment ships as an extent, the S11 assembler folds it, the recompute
   frees the displaced block with no reply naming it; publish schema 16;
   `f64f448a`/`265d69df`/`96f8d873`) + the per-volume epoch close RETIRED
   (`f230eb3a`: stock is fungible across a co-writer's volumes; the plan
   stranded 9–14 % of parked keys) + the fpp park slices written as the
   capacity law (`-k` keeps 640/1,024 live, 3.4 s transit, ~10 % under).
   E-C-C-E, two-iteration probe (no degenerate row): all four rows pass
   the full matrix; **`block_claim_anomalies` 0 on every phase of both E
   rows** (C: 842–1,246 per fpp phase); notices queued ≡ shipped ≡
   received ≡ the authority's `fold_passes` ±2; reminted 0; throughput
   par. Board item opened: the range grant's coverage rule (why a
   whole-block writer ships an extent). Gate on `96f8d873` running.
   **Gate on the round-2 tip — GREEN on `96f8d873`** (20:42–21:37, pinned
   worktree, from zero, laptop idle: 362 suites / **4,792 tests**, 18
   stages, both audits). Every code landing of 2026-09-06 → 09-08 is
   gated; `dev` = `1cb1d122` (docs over it). Releasable as 1.2.2.
1i. **Campaign board (user pick 2026-09-08) — four mechanisms landed on
   `dev` (W-6 `c88bdf7f`, R-5 `1ca9ab67`, W-5 `ab669857`, D-5 `fce9aa5f`
   + the trim-window gauge fix `e0bdd35f`); the field rows for THREE are
   MET** (`.benchmarks/2026-09-08-campaign-rows-squeeze-test.md`, E F F E
   binaries `96f8d873` vs `e0bdd35f` on squeeze-test, kernel 0031, the
   5-node nvme-tcp set): `rr_4k` kern +6.8 % IOPS / CPU −9.6 % (R-5),
   `rw_4k` kern +13.2 % / CPU −12.2 % (W-6), fsync storm +23.8 % fsyncs/s,
   p50 −22.9 %, data sync requests 10 → 1 per fsync (W-5), `w_durable`
   par by construction, tripwires 0. **Still owed: D-5's fleet A-B-B-A**
   (`SQUEEZEFS_META_SHIP_INLINE_SERVE` on the mw rig). New write board
   item 11: fsync of a partial active block escalates the whole block
   (16× per fsync, 80 % of the field fsync — pre-existing, both arms).
   **The gate on `e0bdd35f` is RED** (exit 201, 2 of 21 in
   `mw_authority_assembler_tests`): bisected to D-5's accept-tick commit
   `863a4304`, which made the finding-27 standing custody notice poll
   LIVE in the in-process fixture (it was dead there before: `polls` 0 →
   5) — the poll hears the demotion, the client quiesces + acks, and the
   grant releases before the contracts' 10 ms sampler sees the pending
   mark. The field rows (48–51 poll rounds per row) say the poll always
   worked; fixture artifact, not a product regression. Fix in flight on
   `fix/assembler-contracts-notice-poll` (a `test_set_notice_poll` seam
   held off in exactly the pre-ack-state contracts + a real finding-27
   coverage contract); the gate on the resulting tip is the next act.
1j. **D-5's fleet row RUN — the lever SHIPS OFF** (`af6f49cb`,
   `.benchmarks/2026-09-08-d5-fleet-squeeze-test.md`): two same-binary
   A-B-B-A brackets on the squeeze-test 8-co-writer fleet, both orders.
   `SQUEEZEFS_META_SHIP_INLINE_SERVE=1` deletes the two lane hops exactly
   (0.5–0.7 ms) but the served work runs 0.6–0.9 ms SLOWER on the
   connection thread (in-work wakes become OS unparks, `sqz-jrnl`
   +17–25 %): co-writer publish latency +15–49 %, ingest −1.5…−7 %,
   verbs/s par. The in-process win needed an artificial lane hog; the
   fleet's lanes run at ρ ≈ 0.2. Audit row 18 + DLM #7 closed; the
   remaining owner-dispatch term is `run` itself (the verb-plane
   grouping rung). All four campaign mechanisms are now field-rowed.
   **Gate GREEN on `9e785b6d`** (06:14–07:06, pinned worktree, from
   zero, laptop idle: 365 suites / **4,835 tests**, 18 stages, both
   audits — 1 allowed warning = the adjudicated bincode advisory).
   It took SIX runs; every red was a distinct timing-shaped contract
   fixed test-side, plus one fmt slip and one exec bit (logs
   `/tmp/five/gate/taskcheck.<sha>-red.log`): `e0bdd35f` the assembler
   contracts (the accept tick unmasked the finding-27 notice poll —
   `69a120dd`, with the poll's ask derived inside the reply bound +
   `dlm_custody_notice_poll_failures`); `69a120dd`
   `il_direct_write_tests` (the batch contract had been reading the
   warm-up's late deferred dispatch as its 1 — `ce879a57`, + the fixture
   shuts down on drop); `ce879a57` fmt (`d39775c4`); `d39775c4` the
   delegation red half (the channel's re-assert re-stamped the grant
   after the conflict — `64b4df5d`); `64b4df5d` exec bit (`4e91c45a`);
   `4e91c45a` the free-grace checkpoint lever-off arm (an in-flight
   elastic cycle landing after the sample — `3e6bf333`); `3e6bf333` the
   delegation own-grant contract (same re-assert class — `9e785b6d`).
   The laptop was thermally throttling (98–100 °C, down to 3.67 GHz)
   through these gates; the contracts that broke are the ones that
   sample a counter whose increment the product DEFERS past the
   observable the test waits on. Every code landing of 2026-09-08 is
   gated; `dev` = `origin/dev` = `9e785b6d`.
1k. **1.2.2 TAGGED — `stable-2026.09.2` = `e3635557`** (docs over the
   tested `d08959f2`, whose product tree == `6ca29ea4`; record
   `.benchmarks/2026-09-08-1.2.2-release-gate.md`). The gate ran TWICE:
   run 1 on the bump `49964d7a` was green by the runners' rules and its
   fstests leg exposed the **`generic/795` FUSE wedge** (two delivered
   LOOKUPs unanswered 23 min, every daemon thread idle, cleared only by
   the harness abort, scored "clean") — the user held the tag for
   attribution + a red-first fix. Attribution: the class (a queue worker
   parked UNBOUNDED in cq-wait with one lost wake — the 2026-08-07 zc
   bounded-outcome law, which covered only zc pends) but NOT the wake's
   loser (kernel task-work wake vs daemon; 48 fresh-mount storms with the
   live capture armed did not recur — rigs under
   `/tmp/release-1.2.2/wedge795/`, capture under
   `/var/tmp/squeezefs_forensics/`). Fix (`8efc7e1b`→`1834bed4`): the park
   is bounded (100 ms) while the drain group owes any reply; rescue ledger
   `transport_park_tick_{commit,cqe}_rescues` (≈ 0 healthy = the lost-wake
   tripwire) + fdinfo attribution snapshot in the first-rescue WARN and the
   5 s overdue-slot WARN; seam `SQUEEZEFS_TEST_DROP_COMMIT_WAKES`; suite
   `commit_wake_loss_tests` (RED strand / GREEN one tick — weakening check
   reproduced independently). Runner: wedge verdict (`5a28c592`, replayed
   RED on the tape) + `SQUEEZEFS_FSTESTS_EXCLUDE` (`6ca29ea4` —
   generic/650 hard-hangs this laptop twice: platform hazard). Run 2 from
   zero on the fix: task check 366/4,841; fstests 787/783/4/0 ONE pass;
   pjdfstests 8,798; LTP 174/0/0; require-mount 13/82; zc 187 ledger EMPTY
   (one test-only red first: the `wake_hop` sample law — `d08959f2`); fuzz
   424.0 M execs / 0 crashes. Box zc leg NOT rerun on the final commit
   (squeeze-test's toolchain/checkout were removed in a cleanup, no network
   to rustup/crates.io) — owed when a rocky8 build can be shipped.
   **Next: `task dist:all` (running), ship the rocky8 pair to the box +
   `cluster_reset_v4.sh` remount (rollback `squeezefs.1.2.1` already
   there), remove the release worktree, bench baseline compare + save on a
   cool box, scoreboard on 1.2.2.** Open P1s: the wake's loser; dismounts
   with unflushed staged files (4 of 407 at 200+, the fstests test device
   2,193 every cycle); the NOTE line in the runner is noisy (per-test) —
   quiet it to once per distinct value.
1l. **Post-tag (2026-09-09).** 1.2.1-vs-1.2.2 `dist` E-F-F-E on squeeze-test
   (`.benchmarks/2026-09-09-dist-121-vs-122-squeeze-test.md`): the campaign
   rows reproduce on the shipped profile (rr4k +6.3 %/CPU −9.3 %, rw4k
   +13.3 %/CPU −11.9 %, fsync storm +24.4 %, wdur par, il control +1.7 %)
   and the bounded-park fix's owed field row is MET (ticks 7–339 per row,
   rescues 0/0 everywhere, closure exact). Box back on 1.2.2 `dist`, fresh
   set. Staged-residue investigation (dismounts reporting unflushed staged
   files) in flight — subagent + laptop live probe.
1m. **Dismount staged residue — steps (1)+(2) LANDED and GATED** (`9ac570ab`;
   gate 09:46–10:41, 367 suites / 4,843 tests, 18 stages). (1) the drain
   wait polls `active_block_custody_count()` (the custody the teardown
   retires; notify on its zero transition) — census 10.4 s → 348 ms; the
   teardown flush returns its summary, `failed > 0` → ERROR; the census
   names two classes (staged-layout INFO / active custody WARN); the
   lost-payload message no longer blames a crash; `umount [w]` only with
   active blocks; ops.md paragraph. (2) `promote_all_staged_files_at_dismount`
   drains the staged ledger through `promote_staged_file` under
   `striped_block_concurrency` with the ino's current token — "Dismount
   clean" now means every other client reads the files (the red: a
   different-mount-point remount read 200 files as zeros); gauges
   `dismount_promoted_{files,bytes}`, `dismount_promote_failures`.
   Suite `tests/dismount_staged_residue_tests.rs` (require-mount gate).
   **Step (3) — should `fsync` promote — is the OWNER's decision**
   (`.benchmarks/2026-09-09-dismount-staged-residue.md` §7 item 3): a
   counted A/B on squeeze-test. Left for it: the daemon's own SIGTERM TTY
   prompt (`fuse_client.rs` ≈ 28913) still offers "[w] Wait for staged
   files" with the stale wording; AGENTS.md's "writeback/flush promotes"
   sentence.
1n. **Step (3) priced and REJECTED; the inline raise built and priced OUT;
   packing is the answer (2026-09-09).** (a) The fsync-promotion lever
   (`SQUEEZEFS_FSYNC_PROMOTE_STAGED`, registered, default OFF) on
   squeeze-test A B B A (`.benchmarks/2026-09-09-fsync-promote-staged-ab.md`):
   smallf-fsync −16.7 % files/s, fsync +26 %, p99.9 +122 % — and the
   finding is the SPACE LAW: `promote_staged_file`'s block arm takes one 4 MiB
   block per file (140,183 promotions filled the 480 GiB set in 8 s; the
   landed dismount pass pays the same 64× on its 2,193 files). Options
   A/B/C all share the primitive → rejected; owner: keep step (2), fix via
   the raise. (b) The inline raise (`aec1241f`): derived ceiling
   `inline_max_bytes_ceiling` (format bound = `value_cap − headroom` =
   61,440 on the 256 KiB node), `SQUEEZEFS_INLINE_MAX_BYTES` (4096..=bound,
   refusal outside), size-dispatching `promote_staged_file →
   Result<Option<PromotedInto>>` (≤ ceiling → INLINE in one commit), the
   READ fast-probe inline arm (a pre-existing hole), 7 red-first contracts
   (`tests/inline_raise_tests.rs`). The local sweep
   (`.benchmarks/2026-09-09-inline-raise-sweep-local.md`, tcp devsub,
   scoping) priced it OUT as a default: inline payload rides the meta plane
   twice — 16 KiB = −34 % files/s and 59× meta bytes/file, 32 KiB = −68 %,
   117×, and a 1 GiB meta volume heap-exhausted and FAIL-STOPPED
   (`Metadata volume 1 is disabled`, EIO) → **default flipped back to one
   page** (`00942fd2`, `INLINE_MAX_FLOOR` = the derived default; the format
   bound is the override's maximum). **New P1: a FULL metadata volume
   fail-stops (EIO) instead of answering ENOSPC** — the sweep's T=32768 leg.
   (c) **PACKING design in progress**: `docs/design-small-file-packing.md`
   (design-skill loop, writer/reviewer rounds; the deliverable is the
   design + PR plan, PK1–PK7, lever `SQUEEZEFS_SMALL_FILE_PACKING` OFF
   until the squeeze-test rows). Its round-1 review found **FIND-PK-2, a
   P0**: the block arm of `promote_staged_file` published `bk:0:len` with NO
   durable block reference (the 2026-08-02 wiring `4db827c6` named "the
   staged whole-image promotion" and never wired it) — on a ledger-seeded
   remount every promoted block recovers FREE and the next striped write
   overwrites promoted files (red repro: drift 200/200, 8 of 200 files
   clobbered by four fresh writes). **FIXED `bfcf1e57`** (the map-swap diff
   rides the promotion's commit; contract 3 of
   `dismount_staged_residue_tests`; note
   `.benchmarks/2026-09-09-promotion-durable-ref-hole.md`). Shipped 1.2.2
   carries the pressure-driven half (staging-dir volumes under the 75 %
   high-water arm); the field fleet is cache-less and unaffected; 1.2.3
   carries the fix with the dismount pass. Also fixed test-side today, both
   the timing/parallel-harness class: the mux one-connection contract
   (`2836fa8b`, the documented cold-dial race) and the fork's `op_trace`
   unit tests under the parallel bench harness (`cf4ba9e1`). Gate on
   `bfcf1e57` = the day's landing gate.
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
