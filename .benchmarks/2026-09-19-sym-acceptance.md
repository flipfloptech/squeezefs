# Symmetric shared-disk metadata — PR 13, the acceptance rung (gates 1–8b)

**Date:** 2026-09-19. **Branch:** `perf/sym-acceptance` off `dev` @ `8ead0244`
(every rung landed: PR 1/2/3/4/11/16, level 4 (5/6/7/8), level 5 (7b/9/10 +
the flip fix), PR 12, PR 12b). **Design:** `docs/design-symmetric-metadata.md`
§8 (the gate table — the contract), §1.6 (the measured constants), §5.10 (the
D1 arithmetic), §7.3/§7.4 (the flip decision's inputs), PR-plan row 13.
**Status: COMPLETE (review rounds 1–3 addressed; landed by the orchestrator)** — every row states its venue
class (the 2026-09-14 venue law and the owner's 2026-09-20 restatement
`51bf21e1`, quoted in §1: the dev box answers *does the mechanism work*;
`squeeze-test` is the acceptance venue for every number).

## 0. The verdict in one paragraph (updated last)

**NOT YET — and the rung did its job.** Every acceptance leg exists and
ran from zero on a 7-joiner + token-reader fleet; every MECHANISM law
is GREEN on the final binary (`702ab502`): `tar -x` with 0.012 wire
verbs per entry (its RATIO is the box's — §1), one flip and `shipped ≡ served` in the
shared directory, `K + C + 3` tokens for its `ls -l`, a live holder never
recalled and an idle tree moved in 1–2 bursts (its 5–8 ms wall the
box's — §1), readers exact at the next
resolve with the recall RTT as the free-grace hold, the free wall and
the join storm served, the 46-volume format row VALID, SIM-1 at 12,500
× 64 MET, and **`sym-crash` 10/10 GREEN on nine consecutive from-zero
runs (attempts 7–15; §3.8 lists attempt → binary)**. The rung found
**thirty-five product defects and fixed every one red-first** (defects 10
and 13's pins landed in fix round 1, Issue 10; eleven of them P0 — two
appenders under one `node_seq`, a
flush that dropped acked records, a successor reading every joiner dead,
a reader that never followed a failover, a shipped/local/mid-plan
`SlotBusy` each fail-stopping or refusing, a manager wedged out of every
`fsync` by one stale refinement, three joiners aborting on a stack
overflow) and **one it could not build — the record-level metanode arm
(defect 32)**: a colleague's file cannot be `chmod`ed, `touch`ed or
WRITTEN from another mount, and as found an unfsynced append acked bytes
that never landed — design §5.10's priced row, never built, the flip's
first blocker (the interim posture since the fix rounds: the ARMED plane
refuses the OPEN for write and every record mutation of such a file
`EREMOTE` naming PR 13b, §4.4z / §4.4ai). **Every RATE, wall-clock ratio and timing-shaped reading in
this record is venue-attributed pending the box** (`51bf21e1`: "no laptop
number holds merit, enters a record as a verdict, or decides a gate") —
never a MET, never a MISS; the box owes gates 1 / 2 / 3 / 3b / 3c / 5 / 7
on the flip binary. **`sym-storm`'s ×10 count is NOT reached** (§3.8b):
the fix round's two from-zero runs under the harness's venue word ran 7 +
3 rounds GREEN and each stopped on a PRODUCT finding — the flip's
projection walk (fixed red-first) and then an **acked-writes LOSS across a
seven-victim kill with the cross-owner mover into striped destinations
(§4.4af — 25 of 17,376, one writer's moved files, unattributed, a P0
class: the flip's SECOND blocker)**; the widened deleted-stays-deleted arm
then found a joiner's read inside the failover window answering `EIO`
(§4.4ag). The flip waits for defect 32's rung and finding 1's
attribution, then the box.

> **Re-read at PR 13c (the box campaign, `fix/sym-box-campaign`):** PR 13b
> landed the paragraph's two blockers and the box brackets ran (§3.9 —
> gates 2 / 3b / 7@N=8 MET as VERDICTS; gate 1 a flat-path MISS, gate 3's
> N = 8 term, gate 3c a live holder recalled, gates 5 and 7@N=32 BLOCKED
> on the connection cap, `appender_flush_ceiling_overruns` tripping). PR
> 13c fixed F-B1 / F-B2 / F-B3 and three unarmed-path costs red-first,
> attributed gate 3's N = 8 term to the co-located venue, and formed the
> first 32-member fleet; **what still stands between the tree and the
> flip is §9's PR-13c blockquote**: the box re-runs of gates 1 / 3 / 3c /
> 5 / 7@N=32 on PR 13c's binary, the flush-ceiling MARGIN's derivation
> (F-B1 landed an exclusion, not the margin), and the per-NODE gate-3
> law, UNMEASURED on any venue — a multi-node venue (PR 15's cloud row)
> is its instrument.
>
> **Re-read after the third box pass (PR 13e / 13f's binary `b377cbb8`, 2026-09-23 — §3.9.5):** gate 1 MET by the rule (the setattr term FIXED — `rename` PAR, `unlink` B ahead both orders), gate 3c MET on all three laws twice (the PAUSED law's first real run; F-R3 FIXED as a verdict and F-R4 FIXED as far as the leg reaches), gate 7 at N = 32 MET twice with 0 trips on 32 writers; **the ONE box item left before the flip is F-B1** — the manager's cadence overran twice inside `sym-scale` with PR 13e's derivation engaged but its 64-cycle horizon EMPTY (the term 11 / 4 ms at each joiner storm's start, the trip cycle's own 133 / 127 ms in the window only after; the steady state did not trip) — the first storm cycle after a quiet horizon, under the joiners' create-storm `ExtentGrant` burst (F-R5: a wire joiner's grant derives from an EWMA the manager never receives → the floor 8, on a 512 KiB floor ring) — §7 items 3 / 16, §9's blockquote.
>
> **Re-read after the fourth box pass (PR 13g's binary `230e95dd`, 2026-09-24 — gate 3 only, §3.9.6):** **F-R5 FIXED as a verdict** (every joiner's ring above the floor, 1–3 wire grants per row, reactive 0, the manager's N = 8 row 88 verbs / 0.114 s against 892 / 2.93 s) and **the third pass's F-B1 class GONE** — the first `sym-scale` row set ever to reach its oracle on the box read 0 trips through N = 1/2/4/8 with the onset class exercised at N = 2 / 4; **the tripwire is still not 0**: the second row set (a harness re-run with the row's setup outside its clock) read ONE trip on the manager at 1,101 ms — 1 ms past — at a storm's END under one served verb: the LIVE projection UNDER-PRICED the storm's END cycle by ≈ 65 ms (84 ms at the tick's decision against a ≈ 150 ms pre-barrier wall — the dirt the storm's last second adds after the tick decides) with the decision's lateness at 35 ms, against the fixed two-tick margin; the create-end snapshot preceded the trip (overruns 0 there) and no barrier face reads above 2 ms — §7 item 3's last piece is the projection's growth between the decision and the flush plus the lateness term. The launch skew is MEASURED (9.1 s at N = 8 = three fresh joiners' 3.0 s `mkdir`s into the freshly striped root) and taken out of the row's clock: N = 8 reads 4.61× creates on the storms' own clock (the co-located venue's term; per-NODE is PR 15's). One new armed-plane finding, F-R6 (a joiner's FORGET-driven reclaim pricing destroys for the holder's inos through its stale projection — nothing destroyed, a CPU + log storm, defect 18's root-seq loop (defect 34's family) under it), joins PR 14's list — §7 item 17, §9's fourth-pass blockquote.

## 1. Venue block

| Venue | What ran here | Host / kernel / fabric / substrate |
|---|---|---|
| **Dev box** (SCOPING; every local leg, every from-zero count, SIM-1, the fidelity tier) | `tests/run_mw_matrix.sh` `sym-*` legs on `tests/mw_fleet.sh` (`create N=2 --symmetric --writers=7 --token-readers --lease-ttl-ms=15000`, `SQZ_MWFLEET_OSS_GB=16`), `cargo test --release` pins, `membership_sim::run_sharded` | 32 CPUs (throttles to 3.1–3.7 GHz at 95 °C under sustained load — wall-clock rows are scoping only), kernel `7.2.3-cachyos-lto`, **nvmet-tcp on `127.0.0.1` (`resv_enable=1`)** — the fleet's own tcp devsub instance `mwfleet` (memory-backed null_blk metadata namespaces ×2, zram data namespaces ×2 × 16 GiB); the in-process pins on file-backed sandboxes |
| **squeeze-test** (ACCEPTANCE brackets) | gate 1's A-B-B-A (PR 1's rig verbatim, `.benchmarks/rigs/2026-09-13-sym-pr1-solo-regate.sh`) | `memp-s3ds-aqs-37`, 32 cores, the reset-v5 converged fabric (`/scratch/tmp/cluster_reset_v4.sh`: 5 nodes × (1 meta null_blk + 2 data null_blk) over nvme-tcp), mountpoint `/scratch/tmp/test`; the box has no toolchain — arms built on the laptop with `task build:rocky8` (the `release` profile, both arms — the two-profile law), checksummed, copied to `/scratch/tmp/sym-pr13/` |

**The venue ruling** (`dev` @ `51bf21e1`, 2026-09-20 — AGENTS §Benchmark
VENUE's restated paragraph; the worktree predates it, the law is read from
the main tree): **"no laptop number holds merit, enters a record as a
verdict, or decides a gate — a laptop row that reads a MISS on a
timing-shaped or throughput-shaped law (a flush-ceiling overrun, an ingest
multiple, a wall-clock ratio) is *venue-attributed pending the box*, never
a MISS and never a MET, and the record says so in the row."** Every rate,
ratio and timing reading below carries that label; the laptop's verdicts
are MECHANISM verdicts only (legs green, engagement laws met, tripwires 0,
crash/storm counts from zero).

**Binaries.** Arm A (gate 1) = `3228fcb8` (the pre-program dev tip PR 1's
bracket used — comparability), rocky8 `release` profile, built in a throwaway
worktree (`/tmp/pr13-armA`, `task build:rocky8`, artifact checks passed).
Arm B = the FLIP binary (PR 14's default-on tree — §8; no box row ran in
this rung, so no arm-B SHA stands here).
Every local leg ran `target/release/squeezefs` of the commit named in its row.

**Instruments.** `tests/run_mw_matrix.sh` (the `sym-*` legs — every leg prints
its engagement gauges and exits nonzero when a law is violated; a row without
its engagement is INVALID, not a number), `tests/run_mdstorm.sh` (the metadata
storm — `SYM_STORM`), `dd conv=fsync` (the acked-writes oracle and the ingest
rows), `tar -x` (gate 2), `getfattr`/`setfattr` (the stripe flip), the `.stats`
inode (every gauge named below), `squeezefs fsck` (the C1–C17 census after every
kill), `squeezefs appenders`, SIM-1 = `membership_sim::run_sharded` (release).

## 2. Gate table

_(Per row: the MECHANISM verdict with its engagement gauges, and the
RATE's status — under the venue ruling (`51bf21e1`, §1) a laptop rate is
never a MET or a MISS, so every rate row reads OWED to the box or
SCOPING; the only MET words are SIM-1's (tier (ii), the design's own
class). "dev" = scoping venue; "box" = squeeze-test.)_

> **Status (the box-rows rung, `perf/sym-box-rows`, 2026-09-22): the box
> brackets RAN on PR 13b's binary (`7b2ef9e9`'s code) after the owner
> restored the venue at 01:07 UTC (it had been reclaimed by a third party
> on 2026-09-19 — §3.9's history). The "Rate (box)" cells below are
> superseded by §3.9.1 (gate 1) and §3.9.2 (gates 2 / 3 / 3b / 3c / 5 / 7),
> summarized: gate 1 **MISS on mdstorm mkdir / rename / unlink** (−3.4…
> −5.8 % vs the pre-program tip, both brackets, both orders; rand-4k within
> noise with a reproducible −1.8 % / −2.5…−3.8 %; `w_fresh` and mount time
> within noise); gate 2 **MET** (1.04–1.07× of S0); gate 3 **MISS at N = 8**
> (4.27× creates / 5.33× ingest vs ≥ 5.6×; N = 2 / 4 MET) + the
> flush-ceiling tripwire tripped; gate 3b **MET** (3,400–3,581 creates/s
> into one directory, one flip, `K + C + 3` tokens); gate 3c **MISS
> (mechanism)** — a LIVE holder recalled by a touch; gate 5 **BLOCKED** and
> gate 7's N = 32 storm **BLOCKED** — the manager's cluster-wire connection
> cap derives to 64 under `FLEET_SHARE=32` and the 32-member fleet never
> comes up; gate 7 at N = 8 MET on both rows with the tripwire tripped.
> Three product findings (F-B1 flush-ceiling margin, F-B2 the dominance
> rule's per-slot `ops_h`, F-B3 the listener cap) and the gate-1 regression
> go to PR 14 (§9's re-read).**

> **Status (PR 13c, the box campaign — `fix/sym-box-campaign`):** F-B1 /
> F-B2 / F-B3 and three unarmed-path costs fixed red-first; the box cells
> above stay the VERDICTS and the re-runs (gates 1 / 3 / 3c / 5 / 7@N=32)
> are owed on PR 13c's binary. **Gate 3's N = 8 row is attributed to the
> co-located venue** (§3.9.3): the per-daemon-CPU-second face read off
> the box's own `.stats` is 4,085 (N = 1) → 2,586 (N = 8) creates per
> daemon-CPU-s — **0.63× at N = 8, which MISSES the 0.7 law too when
> the ingest phase's CPU is folded in** (the create phase alone is what
> `sym-scale`'s new `C/CPU-S` column reads; the box has not run it) — so
> the C/CPU-S face is the law's VENUE-BOUNDED PROXY, not its replacement:
> **the per-NODE law ("bounded by no node") is UNMEASURED on any venue and
> a multi-node venue (PR 15's cloud row) is its instrument.**

> **Status (the box RE-RUN on PR 13c's binary `77f4da1d`, `perf/sym-box-rerun`, 2026-09-22 11:02 → 13:52 UTC — §3.9.4; the counted-run law: every row set from zero, gates 2 / 3b not re-run):** **gate 1** — `rr4k` PAR (0.999; the −1.8 % residual GONE), `mkdir` within noise (0.976 / 0.984 — the MISS closed), `create` / `stat` / `manydirs` / `rmdir` / `w_fresh` / mount / remount within noise, `rw4k` 0.970 (the 3 % floor, reproducible), **`rename` 0.960 / 0.967 and `unlink` 0.956 / 0.967 DELTA in both orders — MISS, narrowed to two phases and ATTRIBUTED** (the priced PR-4 rename guard + the `handle_setattr` future's move, named by DWARF perf — §3.9.4.1); **gate 3** — B N = 8 **4.27× creates / 5.37× ingest** (MISS on the wall law vs ≥ 5.6×, exactly §3.9.2's read; `C/CPU-S` 0.61× at N = 8), the A arm (the shipped authority + co-writers, measured for the first time) **bounded at 0.07× at every N** (F-R2: the owner's 8k-entry readdir per shipped create); **gate 3c** — **the LIVE law MET as a VERDICT ×2 (F-B2 FIXED)**, IDLE MET ×2 (6.7–7.5 ms), **PAUSED NOT RUN — its job never ran (a harness defect since PR 13, §4.4aj; fixed, proven locally, the box row owed)**; **gate 5** — **MET ×2 (F-B3 FIXED: the 1 × 31 fleet forms at `max 512 connections`)**, 0 misses, 155 ≡ 155, recall RTT 300 µs, hold 0; **gate 7 at N = 32** — **row (a) MET (1,002–1,299 frees/s, `shipped ≡ served ≡ displaced`, the ledger 1.000× submitted/user — diskstats owed), row (b) MET (32 mounts in 3.66–3.68 s, 250–269 verbs, 12.3–14.6 s service)**; **F-B1 NOT FIXED on the box — `appender_flush_ceiling_overruns` moved SIX increments on five writers across the three fleets (the two with a WARN line 16 / 106 ms past the ceiling; the scale fleet's four have no age reading — H-R2; one of the six (m60) established as not under a storm — the three "between the rows" trips fall in each writer's `rm -rf` of its 20k-file tree + the joins) with the exclusion excusing NOTHING (0 ns on every writer) — the row sets of gates 3 / 3c / 7 each stopped at r1 on it; the missing per-cycle pass-wall instrument is named for §7 item 3.** Two NEW product findings on the armed plane (F-R3: every cross-owner unlink of a foreign-minted child orphans the inode — 430 / 512 in one leg, invisible to fsck while the lessee lives; F-R4: a mid-handover create answered `ENOENT`) and one on the shipped path (F-R2). §9's re-read: NOT YET — the exact list is there.**

> **Status (the THIRD pass — PR 13e / 13f's binary `b377cbb8`, `perf/sym-box-13e`, 2026-09-23 01:53 → 03:52 UTC — §3.9.5; from zero, minimum count, gates 2 / 3b / 5 not re-run):** **gate 1 MET by the rule** — `rename` **1.012 / 0.998** PAR and `unlink` **1.054 / 1.042** B AHEAD in both orders (the setattr-future term FIXED on the box), `rr4k` 1.005, `rw4k` 0.984 / 1.015, `wfresh` 0.991, `mkdir` / `stat` / `manydirs` / `rmdir` within noise, `create` 0.981 / 0.972 at the floor, remount 0.95 / 0.93; two sub-second rows stated — `mount` 1.251 (within a 31.5 % band) / 1.326 (DELTA by 0.03 over 29.7 %: B's first mount of a fresh set +0.1 s on 0.4 s) and the post-`rw4k` clean `umount` 1.109 / 1.388 (+0.5–0.9 s — NEW on this binary, the re-run's B read faster there; unattributed, owed — §7 item 16; not a gate law); `dlm_rpcs` 0 ×24; **gate 3** — N = 1 4,928 c/s · 4,133 `C/CPU-S` · 1,327 MiB/s; N = 2 1.85× / 1.81×; N = 4 3.17× / 3.48×; **N = 8 3.53× creates (storms 9.2–14.4 s; an INFERRED ≈ 3.9 s of launch skew, the root's STRIPING at the row's mkdirs the hypothesised cause; ≤ 4.5× the storms' own bound) / 5.50× ingest**, `C/CPU-S` 0.67×; **F-B1 TRIPPED TWICE on the MANAGER with PR 13e's derivation ENGAGED but its horizon EMPTY (1,127 / 1,125 ms; the term 11 / 4 ms at each joiner storm's start, the trip cycle's own 133 / 127 ms only after; 199 quiet cycles between the rows; the steady state did not trip; 0 excused) — the FIRST cycle of a joiner create storm's `ExtentGrant` burst (50–80 verbs/s); the row set stopped at r1; F-R5 found (the manager derives a wire joiner's grant from `ewma = 0` → the floor 8 whatever its SMO rate; the 512 KiB floor rings checkpoint ≈ 8×/s under a storm and refill in ≤ 8-extent grants — PR 13g)**; **gate 3c MET on ALL THREE laws ×2** — LIVE 192 / 0, IDLE 4 bursts (17.9 / 17.6 ms), **PAUSED (its first real run on any venue) 0 handovers / 0 idle offers / 0 dominated offers**; **F-R3 FIXED** (zero dangling names, the offline census exempting 0, findings 0 ×2), **F-R4 FIXED as far as the leg reaches** (0 errnos on 451 / 451 touch creates — 902; the slot-moved retry class read 0, unexercised — the pin is its proof); **gate 7 at N = 32 MET ×2** — 1,269 / 1,344 frees/s with device ÷ user **1.000** (`/proc/diskstats` ≡ the ledger, `wareq-sz` 1.2 MiB), 32 mounts in 3.80 / 4.06 s, 263 / 291 verbs, **F-B1 0 on all 32 writers both launches**. §9's re-read: NOT YET — F-B1 (+ F-R5) is the one remaining flip precondition this pass leaves; the exact list is there.**

> **Status (the FOURTH pass — gate 3 only, on PR 13g's binary `230e95dd`, `perf/sym-box-13g`, 2026-09-24 04:05 → 04:57 UTC — §3.9.6; two `sym-scale` row sets from zero on fresh fleets, the second a harness re-run with the row's setup outside its clock):** **set 1 is the first gate-3 row set to complete to its ORACLE on the box** — `appender_flush_ceiling_overruns` 0 on the manager and every joiner through N = 1/2/4/8 (the third pass's onset class exercised at N = 2 / 4 and not tripped), deleted-stays-deleted 0 / 3,000 ×2, fsck clean; N = 1 4,997 c/s · 4,216 `C/CPU-S` · 1,506 MiB/s, N = 8 3.53× creates / 4.84× ingest with a MEASURED 9.135 s launch skew (three 3.03 s `mkdir`s by the fresh joiners into the freshly STRIPED root); **set 2 (the setup before the clock): N = 8 22,941 c/s = 4.61× creates (Σ per-writer rates 5.25×, the bound; the co-located venue's term), `C/CPU-S` 0.66×, ingest 7,594 MiB/s (the N = 1 base 2,215 — a sub-second dd; the N = 8 absolute stable at 7.3–7.6 GB/s across three passes), and ONE trip — the manager's volume 1 at 1,101 ms (1 ms past) at the N = 4 storm's END with ONE verb served on that volume across the row: the LIVE projection under-priced the storm's END cycle by ≈ 65 ms (84 ms at the decision, the cycle's own term 151 ms and lateness 35 ms entering the horizon after it; the create-end snapshot preceded the trip with overruns 0; no barrier face reads above 2 ms) against the fixed two-tick margin — §7 item 3's next piece is the projection's growth between the decision and the flush plus the lateness term.** **F-R5 FIXED as a verdict** (every joiner's ring 768 KiB–2.3 MiB, returns ≪ compactions, 1–3 wire grants per joiner per row, reactive 0; the manager 88 verbs / 0.114 s of service over the N = 8 row against 892 / 2.93 s; the closure exact set-wide). **F-R6 (new, reported)**: a joiner's FORGET-driven reclaim prices destroys for the HOLDER's inos through its stale projection (6,782 / 7,266 withheld per set, defect 18's root-seq loop (defect 34's family) under it; nothing destroyed). §9's re-read: NOT YET — the tripwire read 1 increment in 8 rows (1 ms past) on this binary; the exact list is there.**

| Gate | Row | Venue | Verdict | Engagement (the law's gauges) |
|---|---|---|---|---|
| 1 | solo re-gate (flat A vs flat B: mdstorm, mount, w_fresh, rr4k, rw4k, remount) | box | **THIRD PASS 2026-09-23 on PR 13e / 13f's binary `b377cbb8` (§3.9.5.1, A B B A at RT = 60 + B A A B on mdstorm / rw4k / remount): `rename` 1.012 / 0.998 PAR, `unlink` 1.054 / 1.042 B AHEAD (both orders), `rr4k` 1.005, `rw4k` 0.984 / 1.015, `wfresh` 0.991, `mkdir` 0.993 / 0.983, `create` 0.981 / 0.972 (the 3 % floor), `stat` / `manydirs` / `rmdir` within noise, remount 0.947 / 0.931; `mount` 1.251 / 1.326 (0.4 s events, 30 % bands — B's first mount +0.1 s, stated), `umount` 1.109 / 1.388 (post-`rw4k`, +0.5–0.9 s — NEW on this binary (the re-run's B read faster), unattributed, owed §7 item 16; not a gate law) — MET by the rule; the setattr term FIXED on the box; `dlm_rpcs` 0 ×24.** **RE-RUN 2026-09-22 on PR 13c's binary `77f4da1d` (§3.9.4.1, A B B A + B A A B at RT = 60): `rr4k` PAR 0.999 (band 0.8 %; the −1.8 % residual GONE), `wfresh` 0.990 (0.9 %), `rw4k` 0.970 (0.8 % — the 3 % floor, a reproducible −3.0 %), mount 1.13 / remount 1.11 / umount 0.71 (0.4–0.7 s events), mdstorm `mkdir` 0.976 / 0.984 (the MISS CLOSED), `create` 0.992 / 0.983, `stat` 0.987 / 1.006, `manydirs` 0.999 / 0.997, `rmdir` 0.971 / 0.985 within noise, `rename` 0.960 (3.0 %) / 0.967 (0.1 %) DELTA, `unlink` 0.956 (1.2 %) / 0.967 (1.6 %) DELTA — MISS on those two, both orders, ATTRIBUTED (the PR-4 rename guard's priced +2.6 µs + the `handle_setattr` future's construction / lane move per op); `dlm_rpcs` 0 ×20.** The first pass (§3.9.1, PR 13b's binary): MISS on mdstorm mkdir/rename/unlink (−3.4…−5.8 %, both brackets), rand-4k within noise with a reproducible residual (rr4k −1.8 %, rw4k −2.5…−3.8 %), w_fresh/mount within noise. Before it: OWED to the flip binary (§8 — the flat path takes ONE behaviour change from PR 13: defect 6's shipped-bug fix (§4.3), pinned red-first flat, plus per-op atomic loads that are behaviour-identical (`KvTree::descend`'s `LeaseGate::is_armed`, `NodeSeqHandle::next`'s ceiling compare); every other change is behind bit 17 + the knob; PR 14's B arm is the default-on binary by definition) | `dlm_rpcs == 0` ✓ (×20), `meta_kv_forest_*` 0 on flat ✓, Δtripwires 0 ✓; the amplification columns (`/proc/diskstats`): `wfresh` 1.116× both arms at `wareq-sz` 1,037 KiB, `rw4k` 1.134–1.146× at 5 KiB, `rr4k` reads 1.17× at 4 KiB |
| 2 | `sym-tarx` (N = 2, netem 250 µs, the extracting node NOT the manager) | dev → box | **Mechanism GREEN on every run** (verbs/entry 0.012, handovers 0, `dlm_rpcs` 0, oracle clean — §3.2). **Rate (box): MET — 1.04–1.07× of S0 (§3.9.2, two positions, both orders each)**; the laptop had read 0.85–1.02× (SCOPING) | `wire_verbs_per_entry` < 0.05 (0.0122 / 0.0000 on the box), `slot_handovers == 0` |
| 3 | `sym-scale` N = 1/2/4/8 | dev → box | **Mechanism GREEN on every final-binary run** (every N completes, `appenders == N`, tripwires flat, fsck clean, deleted stays deleted through the widened arm — §3.1). **FOURTH PASS on PR 13g's `230e95dd` (§3.9.6, two row sets from zero): set 1 — N = 1 4,997 c/s · 4,216 `C/CPU-S` · 1,506 MiB/s; N = 2 1.87× · 0.96× · 1.75×; N = 4 3.33× · 0.85× · 2.98×; N = 8 3.53× · 0.73× · 4.84× with a MEASURED 9.135 s launch skew (3 × 3.03 s fresh-joiner `mkdir`s into the freshly striped root; Σ per-writer rates 5.69×, the bound); F-B1 **0 on every writer through all four rows — the first gate-3 row set to reach its oracle on the box** (deleted-stays-deleted 0 / 3,000 ×2, fsck clean); set 2 (the setup outside the clock) — N = 8 **22,941 c/s = 4.61× creates** (per-writer storms 10.8–13.9 s, skew 12 ms; Σ rates 5.25×), `C/CPU-S` 2,820 = 0.66×, ingest 7,594 MiB/s (3.43× of a 2,215 MiB/s sub-second base), and **ONE trip on the manager at N = 4 — 1,101 ms, 1 ms past, at the storm's END under one served verb** (the LIVE projection under-priced the storm-end cycle by ≈ 65 ms — 84 ms at the decision vs the cycle's ≈ 150 ms wall, lateness 35 ms; the create-end snapshot preceded the trip; §7 item 3); F-R5 FIXED on every joiner (rings 768 KiB–2.3 MiB, 1–3 wire grants per row, reactive 0, returns ≪ compactions; the manager 88 verbs / 0.114 s over the N = 8 row vs 892 / 2.93 s); handovers 0, ships ≤ 4, rpcs 0; ingest device ÷ user 1.09–1.23× at 1.25–1.65 MiB; F-R6 reported.** **THIRD PASS on `b377cbb8` (§3.9.5.2): N = 1 4,928 c/s · 4,133 `C/CPU-S` · 1,327 MiB/s; N = 2 1.85× · 0.93× · 1.81×; N = 4 3.17× · 0.78× · 3.48×; N = 8 3.53× · 0.67× · 5.50× (per-writer storms 9.2–14.4 s; an INFERRED ≈ 3.9 s launch skew — the root's STRIPING at the row's mkdirs hypothesised; ≤ 4.5× the storms' own bound); handovers 0, ships ≤ 4, rpcs 0; ingest device ÷ user 1.24 → 1.13× at `wareq-sz` 1.5 → 1.3 MiB; F-B1 TRIPPED ×2 on the manager (1,127 / 1,125 ms; the derivation ENGAGED with its horizon EMPTY — the term 11 / 4 ms at each storm's start, 133 / 127 ms after; the steady state did not trip) at the FIRST cycle of the joiners' create-storm `ExtentGrant` bursts — the row set stopped at r1; F-R5 (a wire joiner's grant derived from `ewma = 0` = the floor 8; floor-ring claim-and-retire churn at the SMO grain — PR 13g).** **Rate (box, RE-RUN on `77f4da1d` — §3.9.4.2): B N = 8 4.27× creates / 5.37× ingest vs ≥ 5.6× — MISS on the wall law (N = 2 1.88× / 2.12×, N = 4 3.25× / 3.31× MET); `C/CPU-S` 4,281 → 3,944 → 3,314 → 2,627 (0.92× / 0.77× / 0.61×); the A arm (`mw-scale`, the SHIPPED authority + co-writers, measured for the first time) 351 / 343 / 391 creates/s = 0.07× at N = 2 / 4 / 8 (F-R2); F-B1 tripped 4× on the B fleet (0 ns excused) — the row set stopped at r1.** The first pass (§3.9.2): 4.27× / 5.33× at N = 8, F-B1 tripped; the laptop's 2.2–6.0× band was SCOPING | `appenders == N` ✓, `manager_load_pct` 0–3 % ✓, handovers 0 / ships ≤ N / rpcs 0 ✓ (re-run: ships 0 / 1 / 3 / 6); the A arm: shipped + intents ≡ served ✓ ×3, publish refusals 0, `local_commit_refusals` 0 |
| 3b | `sym-shared-dir` (+ `-ls`) | dev → box | **Mechanism GREEN on every run since defect 14** (one flip at the holder, `shipped ≡ served`, handovers 0; `-ls` = `K + C + 3` tokens, 0 data-leaf reads — §3.3). **Rate (box): 3,400–3,581 creates/s (8 × 5,000 into one directory), `ls -l` of 40,000 in 65.3 s — MET on every law, two positions (§3.9.2)**; the design's A arm (authority + co-writers) has no leg | `dir_stripe_flips == 1` ✓, `dir_stripe_ships ≡ foreign creates` ✓ (34,901 / 34,982 shipped ≡ served), `slot_handovers == 0` ✓; ls: `dlm_token_grants` = 40,067 = K + C + 3 ✓ |
| 3c | `sym-foreign-touch` | dev → box | **Mechanism GREEN on every run since defect 10** (LIVE 192 ships / 0 handovers; IDLE moved after 1–2 bursts; PAUSED keeps its tree — §3.4 — **VOID: the PAUSED job never ran, §4.4aj**). **THIRD PASS on `b377cbb8` (§3.9.5.3): MET on ALL THREE laws ×2 — LIVE 192 ships / 0 handovers, IDLE moved after 4 bursts (17.94 / 17.59 ms — flush 13.8 / 14.1, tree 0 4.1 / 3.4, page 0.1), PAUSED (its FIRST real run on any venue — a live STOPPED job, resumed to 40,000 mkdirs) handovers 0 / idle offers 0 / dominated offers 0; F-R3 FIXED (zero `no inode record`, `xv_cross_owner_dangling_names` 0, the post-leave census C9 = C10 = 0 and the OFFLINE census `current_era_exempted` 0 / findings 0 ×2); F-R4 FIXED as far as the leg reaches (0 errnos on 451 / 451 touch creates = 902 — LIVE 192 + IDLE 256 + PAUSED 3 per position; `xv_cross_owner_step_slot_moved_retries` 0 — the slot-moved class not exercised, the pin its proof); F-B1 0; oracle clean ×2.** **Box RE-RUN on `77f4da1d` (§3.9.4.3): LIVE law MET as a VERDICT ×2 (192 ships / 0 handovers per fresh fleet at `N_floor` 2 — F-B2 FIXED); IDLE MET ×2 (moved after 4 / 5 bursts, `slot_handover_phase_ns` 7.48 / 6.67 ms); PAUSED NOT RUN — INVALID both positions (§4.4aj), owed to the next box session; position 2 RED at its oracle on F-B1 (m60, 1,116 ms); F-R3 / F-R4 found.** The first pass (§3.9.2): MISS (mechanism) — the LIVE phase's touches RECALLED the live holder once (`slot_handovers` 1, `slot_offers_dominated` 1 of 64 evaluations, handover 13.4 ms; `N_floor` seeded 2 on the box) — F-B2; the row set stopped | re-run: 0 handovers on 384 LIVE touches ✓; IDLE 0.016–0.020 handovers/s, `slot_handover_phase_ns` 7.48 / 6.67 ms (flush 3.99 / 2.04, tree 0 3.36 / 4.49, page 0.13); a paused live job keeps its tree — NOT EXERCISED (§4.4aj). First pass: 13.4 ms (flush 9.4 / tree 0 3.8 / page 0.14) |
| 4 | `sym-crash` / `sym-storm` (a)–(f) ×10 from zero | dev (LOCAL by the venue law) | **`sym-crash` 10/10 GREEN on nine consecutive from-zero runs (attempts 7–15 — §3.8's attempt → binary list; their deleted arm read EIO as "deleted", §4.4ag); on the fix-round binaries 1/1 GREEN then 0/1 on the WIDENED arm (finding 2). `sym-storm` ×10 NOT REACHED: 7 + 3 GREEN rounds from zero under `--venue=laptop`, stopped by finding 1 (§4.4af — an acked-writes LOSS, open)** | must-stay-0 set (the flush-ceiling gauge venue-attributed on the laptop, Issue 2); `appender_recoveries ≡ regions of the killed nodes`; acked-loss 0 (VIOLATED once — §4.4af); `fsck_findings == 0`; C8/bitmap drift 0; `replay_dropped_torn == 0` |
| 5 | `sym-readers` (exactness; 1 × 31 broadcast; `free_grace_hold_ms`) | dev → box | **Mechanism GREEN on the 1-reader fleet every run** (exact at the next resolve; `recalls ≡ mutations × holders`, `fanout_p99 ≡ readers`, `timeouts_live` 0 — §3.5). **Box RE-RUN on `77f4da1d` (§3.9.4.4): MET ×2 — the 1 × 31 fleet FORMS (`max 512 connections`; F-B3 FIXED), 0 misses over 31 readers, `dlm_token_recalls` 155 ≡ 5 × 31, acks 155, `fanout_p99` 32, `timeouts_live` 0, recall RTT 299.6 / 300.3 µs (p99 ≤ 512 µs), token grant p99 ≤ 512 µs (exactness window) / ≤ 1,024 µs (leg-wide), `free_grace_hold_ms` 0 under tokens, `reader_staleness_bound_ms` 0 ×31.** The first pass (§3.9.2): BLOCKED — the 1 × 31 fleet never came up (the listener cap 64 under `FLEET_SHARE=32` refused the 14th member — F-B3); the laptop's recall RTT 110–126 µs stays SCOPING | re-run at N = 32: `dlm_token_recalls` 155 ≡ 5 × 31 ✓, `fanout_p99` 32 ≡ the readers' bucket edge ✓, `reader_staleness_bound_ms` 0 ×31 ✓, `dlm_token_reader_holder_planes` 1 per volume, `recall_gated_frees` 5 / row ✓ |
| 6 | format cost at N = 8 / 32 (+ the 46-volume width row) | dev (LOCAL) | **RUN — §3.6**: the width row VALID at N = 1/4/16/46 (mount 0.96 s, reopen 1.39 s at 46; the per-slot extent floor 16 MiB/volume = 736 MiB at 46 vols × 20 k files vs flat 47 MB — R9's number; `A_max` inert on a manager, §7 item 8); the appender rows ≈ 4 MiB (N = 8) / 16.5 MiB (N = 32) of region overhead per volume beside the manager's ring | per-slot extent floor, `slot_tree_bytes` p99 vs `A_max`, ring space, page writes |
| 7 | relocated walls (terminal-free rate per holder under `w_rewrite` N = 8; the manager verb rate under a 32-mount join storm) | dev → box | **Mechanism GREEN on every run** (row (a): `shipped ≡ served ≥ displaced` — 2,723 ≥ 1,792; row (b): `/jobs` ships 7/7). **THIRD PASS on `b377cbb8` at N = 32 (§3.9.5.4, two launches on fresh fleets): row (a) MET ×2 — 1,269 / 1,344 frees/s, `1,984 ≡ 1,984 ≡ 1,984`, `/proc/diskstats` device ÷ user 1.000 (byte-exact with the ledger) at `wareq-sz` 1,204 / 1,192 KiB, 0 reads; row (b) MET ×2 — 32 mounts in 3.80 / 4.06 s, 263 / 291 verbs, Σ service 15.1 / 16.0 s, 31 ≡ 31 fleet-wide; F-B1 0 on all 32 writers both launches; oracle clean ×2.** **Box RE-RUN on `77f4da1d` at N = 32 (§3.9.4.5, two launches): row (a) MET ×2 — 1,299 / 1,002 frees/s at the holder, `shipped 1,984 ≡ served 1,984 ≡ displaced`, the daemons' ledger 1.000× device/user (diskstats owed), `free_ship_failures` 0; row (b) MET ×2 — 32 mounts in 3.66 / 3.68 s, 250 / 269 manager verbs, `manager_service_ns` Σ 14.6 / 12.3 s, Σ served ≡ Σ shipped fleet-wide (`/jobs` striped mid-storm); launch 2's row (a) tripped F-B1 (m65, 1,206 ms, 0 excused) — the row set stopped at r1.** The first pass (§3.9.2, N = 7 / 8): row (a) 983 frees/s, `shipped 1,824 ≡ served 1,824 ≥ 1,792 displaced`, `manager_load_pct` 1 — MET with the tripwire tripped (F-B1); row (b) at N = 7: 3.92 s join wall, 55 verbs, 278 ms service, `/jobs` ships 7/7 — MET; the N = 32 storm BLOCKED (F-B3) | re-run at N = 32: `free_shipped_blocks` 1,984 ≡ `free_served_blocks` 1,984 ≡ displaced ✓, `free_ship_failures` 0, `free_refused_blocks` 0, `manager_load_pct` 0, `manager_verbs_per_s` 1; row (b) `manager_verbs` 250 / 269, `manager_service_ns` 14.63 / 12.32 s, `appenders_known` 32. First pass (N = 7): `manager_verbs_per_s` 4, `manager_load_pct` 1, `manager_service_ns` 571 ms / 278 ms |
| 8 | SIM-1 `SimConfig { clients: 12_500, shards: 64 }` | dev (tier (ii)) | **MET** — §5 | beat p99, eviction fan-out, the free-grace V-fan-in, the death ledger's reach |
| 8b | fidelity `full` (nvmet; `pr-registrants` ≥ 1,024 + the emulated cap refusal; `sym-join-ladder` N = 3) | dev (LOCAL) | **PASS = 190 / FAIL = 0 from zero on a quiet box (fix round 1, `fix1-post3`) — 1,024 registrants reported by the REGCTL-sized read (65,600 B), the join ladder N = 3, guard ×10, the residue snapshot clean; the rung's own run read PASS 189 / FAIL 1 with the one FAIL the concurrent fleet's zram teardown (§3.7)** | the tier's own verdicts |

## 3. The local legs — from-zero counts

_(filled per leg: rounds run, greens, the counted-restart events.)_

### 3.1 `sym-scale` (gate 3) — dev box, SCOPING (every rate below is venue-attributed pending the box — `51bf21e1`; the MECHANISM verdicts are the laptop's)

Four full-ladder runs on the fleet (7 joiners + the manager, one token
reader). Per row: N RW mounts each creating 30,000 files (4 threads) in its
OWN directory (`tests/run_mdstorm.sh` `create`), then ingesting 512 MiB each
(`dd bs=4M conv=fsync`); exactly N appenders live per row
(`sym_ensure_joiners`); every mount's `.stats` snapped before/after.

| run (binary) | N=1 | N=2 | N=4 | N=8 | note |
|---|---|---|---|---|---|
| r2 (`c2c5e663`) | 7,227 c/s · 2,065 MiB/s | 19,162 (2.65×) · 4,769 (2.31×) | 27,548 (3.81×) · 7,433 (3.60×) | storm completed (8 × ~5,100 c/s = 40,700 = 5.6×) | must-stay-0 tripped at N=8: `appender_flush_ceiling_overruns=2` on the manager (§4.3) |
| r4 (`c2c5e663`) | 6,661 · 1,015 | 18,626 (2.80×) · 2,199 (2.17×) | 27,314 (4.10×) · 4,855 (4.78×) | a joiner FAIL-STOPPED at its 20,250th create (§4.2, defect 5) | N=4 also read `appender_flush_ceiling_overruns=+2` |

| **r5 (`2a94abbc` + the return belt = `d00db50b`'s tree; defect 5 a+b landed)** | 6,874 · 2,481 | 19,192 (2.79×) · 5,679 (2.29×) | 27,909 (4.06×) · 8,015 (3.23×) | **39,778 (5.79×) · 10,309 (4.16×)** — the row COMPLETES: no fail-stop, no contamination, must-stay-0 set flat | create-rate reading 5.79× at N = 8 and ingest 4.16× — both venue-attributed pending the box (SCOPING; 10.3 GB/s into zram over nvmet-tcp on `127.0.0.1` is the single 32-CPU box's data path — 8 daemons × 4 MiB `dd conv=fsync` + their FUSE queues on the same cores); **deleted-stays-deleted 0 / 3,000** sampled removed names after every joiner's clean unmount, judged at the manager AND at a remounted joiner |

| **r7 (`8d7fd3c0` — the FINAL binary, from zero, `pr13-batch7`; defects 20–26 landed)** | 7,023 · 2,991 | 19,018 (2.71×) · 6,212 (2.08×) | 30,071 (4.28×) · 6,300 (2.11×) | **20,517 (2.92×) · 5,023 (1.68×)** — the row COMPLETES (defect 24 gone: `/` auto-striped under the seven `mkdir`s and every joiner's `stat /` folded through the divert), must-stay-0 flat, `appender_flush_ceiling_overruns` 0, `manager_load_pct` 1 %, 453 stripe tokens served at the manager once | rate readings venue-attributed pending the box (SCOPING: every writer at 2,600–3,100 c/s uniformly at N = 8, the manager included, against 7,600–8,400 at N = 4 — the throttling box after four hours of fleets (86 °C idle), not a serialization: no product change touches the create path between r5 and r7); deleted-stays-deleted 0 / 3,000 at the manager AND at the remounted joiner |

| **r10 (`5b0ec0be`'s code, `pr13-batch10`, from zero, quiet box after the build; defects 27–29 landed)** | 6,589 · 2,628 | 17,559 (2.66×) · 5,106 (1.94×) | 30,143 (4.57×) · 4,236 (1.61×) | **20,896 (3.17×) · 4,877 (1.86×)** — COMPLETES, fsck clean, must-stay-0 flat (`appender_flush_ceiling_overruns` 0), `manager_load_pct` 1 %, `MGR_CPU` 423 % | rate readings venue-attributed pending the box (SCOPING: per-writer 2,634–3,204 c/s at N = 8 against 7,587–8,201 at N = 4, uniform, the manager included; ingest at N ≥ 4 is the single box's data path — 8 × `dd bs=4M conv=fsync` into zram over nvmet-tcp on 32 CPUs beside 8 daemons) |

| **r11 (`8c992af6` — THE FINAL binary, `pr13-batch11`, from zero; defects 30/31 landed)** | 6,083 · 2,191 | 15,953 (2.62×) · 5,572 (2.54×) | 13,379 (2.20×) · 8,992 (4.10×) | **36,465 (5.99×) · 4,487 (2.05×)** — COMPLETES, fsck clean, must-stay-0 flat, `manager_load_pct` 1 % | rate readings venue-attributed pending the box (SCOPING: creates 5.99 × at N = 8 and 2.20 × at N = 4 — the same code read 4.57 × at N = 4 the run before; ingest 4.10 × at N = 4, 2.05 × at N = 8 — the single box's data path) |

`slot_handovers == 0`, `slot_ships` ≤ 5 (the `/` flip's supplies), Σ
`dlm_rpcs == 0`, `manager_load_pct` 0–3 % on every row (the manager's
verbs cost nothing measurable at N ≤ 8 — its CPU is its OWN storm's).
**The create-rate reading on this laptop is NOISE at N ≥ 4**: the same
code read N = 4 at 4.57 × then 2.20 ×, and N = 8 at 3.17 × then 5.99 ×,
on two consecutive from-zero runs an hour apart (r10 → r11; r5's 5.79 ×
and r7–r9's 2.9–3.2 × at N = 8 sit inside the same band) — every row
COMPLETES with every tripwire flat and the deleted law and fsck clean,
so what moves is the 32-CPU box's scheduling of 8–32 storm threads
beside 4–8 daemons' lanes under heat (the venue law's exact case), not a
product term (`manager_load_pct` and the wire gauges are flat in N on
every run). **Neither the 0.7 × N create law nor the ingest law takes a
verdict from this box** (`51bf21e1`): the readings above are
venue-attributed pending the box, and a local A-B-B-A of two binaries on
this box cannot resolve a band this wide — §7's item 0 states the band
as a scoping read and the box row alone decides. **Fix round 1 (Issue
8):** the deleted-stays-deleted arm of this leg was WIDENED — it read an
EIO / EAGAIN / refused `stat` as "deleted"; it now reads the ONE
classifier (`sym_stat_deleted`: only `ENOENT` is deleted, anything else
dies with its errno) — and the leg re-ran once from zero on the fix-round
binary (mechanism only): §3.1a.

### 3.1a `sym-scale` on the fix-round binaries (Issue 8's re-run; mechanism only)

**Run 1 — `16408a2f` (`/tmp/grok-justin/fix1-fleet/sym-scale.log`, from
zero, exit 0, PUBLISHED):** every N completes (N = 1/2/4/8: 6,588 /
17,355 / 28,579 / 24,038 creates/s, ingest 2,668 / 5,538 / 3,517 / 5,492
MiB/s — SCOPING, venue-attributed pending the box), `appenders == N`,
handovers 0, `slot_ships` ≤ 4, Σ `dlm_rpcs` 0, `manager_load_pct` 1–2 %,
the must-stay-0 set flat (the venue word reported nothing), fsck clean;
**the WIDENED deleted-stays-deleted arm (`sym_stat_deleted`, only ENOENT
is deleted): 0 of 3,000 sampled removed names resolve at the manager
after every joiner's clean leave, 0 of 3,000 at the remounted joiner —
every `stat` answered `ENOENT`, none an EIO / EAGAIN the old arm would
have read as "deleted"**. Run 2 on the flip-fix binary (`7c62428b`) is
§3.8b's batch — its row is appended there when it lands.

### 3.2 `sym-tarx` (gate 2) — dev box, SCOPING; mechanism GREEN on every run, the ratio venue-attributed pending the box

2,468 entries (`linux-7.2.3/fs`), A-B-B-A both orders per run, the
joiner under netem 250 µs, the manager local. r4 1.02× / r5 0.85× / r7
0.96× / r9 0.96× / **r10 0.96×** (2.42 s vs 2.53 s) of S0 — a wall-clock
ratio, venue-attributed pending the box (gate 2's bound ≤ 1.10× is the box
row's to judge);
`verbs/entry` 0.0122 on the first sym leg (30 manager verbs = the join +
grant refills) and 0.0000 on the second; `xv` / `ship` / `pub` 0;
`slot_handovers` 0; oracle clean. The joined writer's `tar -x` into its
own tree costs no wire verb per entry — §5.10's first row, exact.

### 3.3 `sym-shared-dir` (+ `-ls`) (gate 3b) — dev box, SCOPING; mechanism GREEN on every run since defect 14, the rates venue-attributed pending the box

8 creators × 2,500 files into ONE directory held by m60: r10 — wall 6.08
s, 3,290 creates/s aggregate, **one flip at the holder (K = 64)**,
`xv_shipped ≡ xv_served` = 17,502, `dir_stripe_ships` 17,394 (the 1/64
own-stripe lands are the difference), `slot_handovers` 0. `-ls` on the
token reader: cold `readdir + stat` of 20,000 children in 20.9 s,
**`dlm_token_grants` 20,067 = K + C + 3** (the directory, its parent, the
root), `readdir_merges` 43, `node_cache_misses` 12 (the poll's dropped
images over 8 epoch steps), `token_hits` 5.4 M — 0 data-leaf reads for
the listing. Oracle clean.

### 3.4 `sym-foreign-touch` (gate 3c) — dev box, SCOPING; mechanism GREEN on every run since defect 10, the handover walls venue-attributed pending the box

Holder m60, requester m61, the manager holder C; beat 10 s, `N_floor(A)`
5, bursts of 64, 3 rounds per phase. **LIVE**: 192 ships, 0 handovers (a
live holder is never recalled by a touch). **IDLE**: handed over after 2
bursts (r10: 21.9 s, 0.046 handovers/s; `slot_handover_phase_ns` total
5.6 ms — flush 2.8, page 0.09, tree 0 2.7). **PAUSED**: 3 single touches
over 3 beats, 0 handovers — **VOID (harness defect §4.4aj, found by the
box re-run's review 2026-09-22): the phase's "paused live job" never
ran on any of these attempts** — its storm was launched into a
directory that did not exist and died at its first `mkdir`, so every
PAUSED green here (and §4.4ac's attempt-12 handover) read the IDLE arm
on a slot nobody wrote, never the paused-job law. The law is exercised
for the first time by the fixed harness on 2026-09-22 (§3.9.4.3: 0
handovers, 0 idle offers, 0 dominated offers with a live STOPPED job)
and its box row is owed. Oracle clean.

### 3.5 `sym-readers` (gate 5) — dev box, SCOPING; mechanism GREEN on the 1-reader fleet every run, the recall RTT venue-attributed pending the box

Writer m60, one token reader: create / rename / setattr **exact at the
reader's next resolve** (0 misses); the broadcast shape over 5 publishes
of one file: `dlm_token_recalls` 5 ≡ mutations × holders, acks 5,
`fanout_p99` 1 ≡ readers, `timeouts_live` 0, `recall_rtt_mean` 110–126
µs; recall-driven free-grace: `recall_gated_frees` 5 (the freeing
publish's recall IS the qualification — the hold under tokens is the
recall RTT against the S5 composite's 2,724 ms), ring `deferrals ≡
releases + offsets` = 0. The 1 × 31 broadcast is the box's (§7).

### 3.6 Gate 6 — format cost (`pv-volume-scaling --symmetric`, the 46-volume width row; the N = 8 / 32 appender rows) — dev box, LOCAL (`pr13-post11/gate6-pv-symmetric.log`, exit 0, "rows VALID")

**The width row** (`tests/pv_volume_set.sh` with `SQZ_PVSET_SYMMETRIC=1`:
`format --symmetric`, ONE armed daemon over N file-backed volumes,
20,000 files × 8 threads into one directory, medians of 3; the flat
baseline is `.benchmarks/2026-08-21-pv-volume-scaling.md`'s table on
the same fixture):

| N vols | mount s (flat) | reopen s (flat) | RSS at mount MB (flat) | create ops/s (flat) | stat warm/cold ops/s | node cache MB (flat) | B/file (flat) | slot trees | tree p99 / max KiB | `A_max` KiB | Σ ring KiB | ckpts over create | free extents | jrnl max/mean (flat) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 0.17 (—) | 0.56 (—) | 1,761 | 6,508 | 33,795 / 34,814 | 18.3 | 957 | 64 | 2,048 / 2,048 | 288 | 8,176 | 3 | 1,939 | 1.0 |
| 4 | 0.23 | 0.58 | 1,792 | 5,656 | 34,486 / 33,884 | 67.5 | 3,539 | 256 | 1,792 / 1,792 | 284 | 32,704 | 16 | 7,781 | 2.3 |
| 16 | 0.45 (0.34) | 0.81 (1.09) | 1,864 (1,758) | 5,200 (4,828) | 33,984 / 34,867 (26,058 / 26,415) | 265.3 (17.0) | 13,907 (891) | 1,024 | 1,536 / 1,536 | 280 | 130,816 | 64 | 31,146 | 8.3 (8.3) |
| 46 | 0.96 (0.60) | 1.39 (1.26) | 2,034 (1,832) | 4,304 (4,934) | 35,411 / 34,498 (25,540 / 26,433) | **760.0 (47.0)** | **39,846 (2,464)** | 2,944 | 1,280 / 1,280 | 276 | 376,096 | 230 | 89,557 | 23.3 (23.3) |

R5 level 0 / yellow 0 / red 0 / backstop 0 at every width; `volsMoved
== N`; node-cache hit 100 % warm and cold; the tripwires flat; the solo
re-gate held (`dlm_rpcs` 0). Readings: (1) **the per-slot extent floor
(R9) is the row's number** — every volume mints its 64 rotor trees at
this population (`slot trees = 64 × N`, lazy minting engaged: a tree
mints on its first record), and a tree is ≥ one 256 KiB node, so the
forest's floor is **16 MiB per volume** (64 × `node_size`) — at N = 46
with 20,000 files that is 736 MiB of node images against the flat
layout's 47 MB, **39.8 KB per file vs 2.5 KB**; it amortizes with the
population (the fleet's N = 8 volumes carried 1,850 inos per tree at
1.5 MiB p99) and `--meta-node-kib 64` quarters it — the design's stated
lever; (2) mount / reopen at N = 46 cost +0.36 s / +0.13 s over flat
(the 2,944 trees' roots read from tree 0 + the directory's page); RSS
+200 MB (the floor's node images); (3) create ops/s −13 % at N = 46 and
+8 % at N = 16 vs the flat baseline — inside the laptop's band (§3.1);
the stat passes +32 % (the kernel-TTL-0 stat sweep serves off the same
cache either way — a different day's thermal state, not a layout term);
(4) `A_max` reads ≈ `node_size` (276–288 KiB) at every width while the
trees sit at 1.3–2 MiB: **the soft cap is inert on a MANAGER** — PR 3
keeps the manager's own images untracked in the slot-extent ledger that
`A_max` reads (`affinity_a_max_bytes`), so a solo mount spills every
mint past 256 KiB per tree to the rotor with the most headroom
(`affinity_ceiling_spills` 34,241 vs `affinity_mints` 5,759 on the
fleet's manager); the OUTCOME is the designed one (p99 = max —
uniform trees), the gauge misleads and each spill pays an O(M) headroom
pick — §7's item 8 (feed the manager's images or derive `A_max` off
`slot_tree_bytes` Σ); (5) `jrnlMax/Mean` 23.3 at N = 46 ≡ the flat
baseline's 23.3 — a single-parent workload lands on one volume on both
layouts (the 2026-08-21 record's own reading); the balance instrument is
unchanged by the layout.

**The N = 8 / 32 appender rows** (per-appender format cost — measured
constants off the fleet's N = 8 `sym-scale` row on `8c992af6`, the N =
32 row arithmetic on them): a joined appender costs its **ring** (the
floor `SYM_RING_FLOOR_BYTES` = 512 KiB — `appender_ring_bytes` 524,288,
2 segments, 0 grows on every joiner), its **pages** (4 × 4 KiB slots —
two in the directory, two ring-side), its **grant** (`extent_grant_
claimed` 65–71 extents per volume per joiner = 16–18 MiB of slot-tree
images — the trees' OWN bytes, returned at the leave; `unclaimed` 3–7 =
the standing remainder, ≤ 2 MiB), and a directory pair; the manager's
ring is the format's (16 MiB here — `clamp(volume/64, 8, 32 MiB)`),
`appenders_capacity` 125 on a 1 GiB volume (heap/16 ÷ ring). So at **N
= 8**: 7 × 512 KiB rings + 8 × 16 KiB pages + one 256 KiB directory
extent ≈ **4 MiB of region overhead per metadata volume** beside the
manager's 16 MiB ring, and 128 minted slot trees × 256 KiB = 32 MiB of
tree floor (16 per appender at this load); at **N = 32**: 31 × 512 KiB
+ 32 × 16 KiB + 2 directory extents (31 pairs per 256 KiB extent) ≈
**16.5 MiB** of region overhead per volume, the tree floor 32 × 16 × 256
KiB = 128 MiB at the same per-appender shape (64 rotors each mint only
when written), `appenders_capacity` 125 unchanged (the ring floor sizes
it) — the 32-appender join itself is the in-process contract
(`sym_manager_tests`: 32 joiners grow the directory chain past its
first extent) and the fleet's join storm (`sym-walls` row (b): 7 joiners
in 2.2–3.2 s, the wall to the last armed). Page writes: one per
appender per checkpoint (`meta_kv_checkpoints` 226 at the manager /
375 at a joiner over the N = 8 ladder — the joiner's own cadence).

### 3.7 Gate 8b — the fidelity tier `full` (`pr13-post15/fidelity-full.log`; binary `702ab502`'s tree)

`sudo tests/run_nvmeof_fidelity.sh full` with `FIDELI_PR_REGISTRANTS=1024`
on kernel nvmet (the tier's own zram/null_blk substrate, 19 m 53 s):
**PASS = 189, FAIL = 1** — the one FAIL is the closing
`teardown-zero-residue` snapshot, whose diff is `-/dev/zram1 -/dev/zram2`:
two zram devices PRESENT at the tier's start (the fleet's data namespaces
— the storm-only rerun's fleet was up when the "before" snapshot was
taken) and torn down by the fleet during the tier — a removal the
residue law reads as a diff, not a product residue (the tier's own
subsystems / controllers read `(none)` both sides). Every product leg
PASSED: `roundtrip-nvmet`; **`pr-registrants` — 1,024 registrants on one
namespace established in 950 s (928 ms each), the product's REGCTL-sized
read REPORTS all 1,024 (`nvme-cli regctl=1024`), the report transfer
`64 + 64 × 1024 = 65,600 B` (`pr_report_bytes`), the registrant cap in
force `unbounded` (nvmet has none — the emulated-cap refusal is
unreachable by construction here, its contract is the in-process one),
drained to `regctl=0`, unshared**; `sym-manager-failover`;
`sym-join-ladder` (N = 3 processes on real namespaces, the token reader
beside them); `loud-fail-matrix` (incl. the R-SYM-8 retirement
refusals); `crash-window-nvmet`; `adopt`; `pr-matrix`; `g2-persistence-
nvmet`; `soft-roce` (plumbing); `ab-smoke`; `guard-nvmet-x10`. Gate 8b
MET on the product legs; the tier's residue snapshot had to be re-taken
on a quiet box for the clean `FAIL = 0` line. **Fix round 1 re-ran it
from zero on a QUIET box (no fleet up; `fix1-post3/fidelity-full.log`,
binary `4523f25e`'s tree, `FIDELI_PR_REGISTRANTS=1024`, 19 m 58 s): PASS
= 190, FAIL = 0** — every leg above again (1,024 registrants reported,
65,600 B; the join ladder N = 3 with the device reporting rtype 3 on the
data namespace; guard ×10) and the residue snapshot clean. Gate 8b's
`FAIL = 0` line stands (§7 item 11 closed).

### 3.8 `sym-crash` / `sym-storm` (gate 4) — dev box, LOCAL by the venue law; the from-zero counts

Every batch runs both ×10 from zero on the binary its row names
(counted-restart law: a red aborts the count, the fix restarts it).
`sym-crash` (7 joiners + a token reader, the manager killed −9 every
round under a sustained write load, the successor remounted): **10/10
GREEN on nine consecutive from-zero attempts (7–15, listed with their
binaries in §3.8b)** — per round the acked-writes oracle
(866–3,820 fsynced files per round, all present with content), the
reader following the failover as a member without fencing
(`self_fences` 0), a member worker re-enrolled at the successor, the
symmetric successor (`self_recoveries` 2, manager `held`, tripwires 0),
every joined writer's window covering its post-failover write and its
free served at the successor, the freed-offset fan-in advancing with 7
members, the dead incarnation's deferred bitmap leaks converging
(`deferred ≡ released + adopted`, pending 0), the member-side census
shard on the reader (0 findings), the reader reading every joiner's
post-failover name, `fsck` clean. `sym-storm --victims=7 --cross-owner
--striped` ×10 (every joiner killed at once per round, its acked files
renamed concurrently into the manager's directory, every storm directory
striped K = 64, the recalled-reader arm, deleted-stays-deleted): attempt
8 rounds 1–3 GREEN then defect 28; attempt 9 round 1 RED (defect 29);
attempt 10 round 1 GREEN (10,608 acked from 7 joiners + the manager, 14
regions recovered in 47 s, the reader arm exact) then round 2 RED
(defects 30 / 31); attempt 11 (`8c992af6`) rounds 1–3 GREEN then round
4 (defect 33); attempt 12 (`7ac89d24`) rounds 1–3 GREEN then round 4
(defect 34); attempt 13 (`87461d56`) rounds 1–3 GREEN then round 4
(defect 35); attempt 14 (`24bbb195`) rounds 1–4 GREEN then round 5
(defect 36); attempt 15 (`702ab502`, the final code) rounds 1–2 GREEN
then round 3 RED on the recalled-reader arm's PREMISE check ("the token
reader's resolve … took no token — `dlm_token_grants` flat") — the
harness asserts the reader's pre-kill `stat` of the victim's acked object
took a fresh inode token, and the reader served it without one (the
object's tokens already held from the two stats' first, failed, source
walk, or the plane's fold — a harness premise, not the arm's law: the
recall-on-successor-setattr it then tests ran GREEN on every round it
reached, attempts 8–15), re-run as a storm-only from-zero attempt on the
same binary (`pr13-batch15-storm`): round 1 GREEN on every law (26,181
acked from 7 joiners + the manager, 14 regions recovered in 17 s, the
reader arm exact) and then `appender_flush_ceiling_overruns=1 on m0` at
1,114 ms — 14 ms past the ceiling at the storm's START (seven explicit
stripe flips + seven `mkdir /` ships + the movers on the manager, with
the fidelity tier's substrate coming up on the same box), the §4.5 /
§4.4aa NON-recovery class (no extension applies — the manager's
grant/ship service under the SMO mutex), the margin PR 14 derives.
**The `sym-storm` ×10 count on the final binary is NOT reached in this
rung**: attempts 8–15 each ran 1–5 rounds GREEN on every law — acked
loss 0, deletes stay deleted, fsck clean after every round, the reader
arm exact — and each ended on a defect fixed the same day (28–36) or,
on the final code, a harness premise and the flush-ceiling margin;
the rounds that ran GREEN on the final code are the count that stands
(2 + 1). **Fix round 1 (Issue 2):** the harness gained the venue word
(`--venue=laptop|box` — the ruling's named timing-shaped gauge is
REPORTED per round on the laptop, must-stay-0 on the box; every other
law fatal on both) and the storm re-ran ×10 from zero on the fix-round
binary under `--venue=laptop` — §3.8b carries the count and the
per-round venue-attributed readings.

### 3.8b Fix round 1's from-zero runs on the fix-round binaries

**Batch 1 — `16408a2f` (Issues 2/7/8/12/13's product + harness landed;
`/tmp/grok-justin/fix1-fleet/`):** `sym-crash --rounds=1` GREEN (1,643
fsynced files all present, the reader followed as a member with
`self_fences` 0, a member worker enrolled at the successor, the
member-side census shard scoped 130 projected trees with 0 findings, the
successor's must-stay-0 set flat, the WIDENED deleted-stays-deleted arm
GREEN through every joiner and the reader, fsck `findings:0`). `sym-storm
--rounds=10 --victims=7 --cross-owner --striped --venue=laptop` from
zero: **rounds 1–7 GREEN on every law** (acked 14,357 / 10,631 / 17,414 /
15,232 / 19,463 / 15,351 / 18,733 fsynced files from 7 joiners + the
manager, all present; 14 regions recovered per round in 13–46 s; the
recalled-reader arm exact; deleted stays deleted through every daemon by
the ONE classifier; fsck clean after every round); **the venue word did
its job** — round 5 read `appender_flush_ceiling_overruns=1 on m0` and
the leg REPORTED it as venue-attributed (ledger `venue-attributed.txt`:
`round 5 m0 appender_flush_ceiling_overruns=1 venue=laptop`; the gauge is
cumulative, so round 7 reported the same word) and continued where the
attempt-15 rerun had died; **round 8 RED — a NEW product defect, fixed
red-first the same hour** (`7c62428b`; not one of the 35): the rejoined
joiner m64's explicit stripe flip of the round directory it had just made
was refused `EINVAL` — the flip's stripe check (`stripe_parent_dir` →
`find_parent_of_child`) ran the reverse dentry scan over EVERY slot tree
of the volume, its PROJECTIONS of slots other appenders lease included,
and slot 10's tree, whose root its lessee had recycled, exhausted the
traversal budget (`tree 0 (slot Some(10), … a PROJECTION here) …
restarts [root-seq] = 256` — a leased slot's tree no refresh heals,
KD-SYM-3; §7 item 1's class reached from the FLIP). A stripe has no
ordinary name and inos are never reused, so the directory-parent memo —
fed at every directory mint now — answers the check for a directory this
mount made without a scan; pinned red-first
(`an_explicit_flip_of_a_joiners_own_fresh_directory_walks_no_projection`:
`meta_parent_scans` +0 across a joiner's mkdir + flip, RED +1 before).
By the counted-restart law the storm count restarts from zero on the fix
binary — batch 2 below.

**Batch 2 — `7c62428b` (the flip fix landed; `/tmp/grok-justin/
fix1-fleet2/`), from zero:** `sym-storm --rounds=10 --victims=7
--cross-owner --striped --venue=laptop`: **rounds 1–3 GREEN on every law**
(acked 16,086 / 17,418 / 18,388 fsynced files from 7 joiners + the
manager, all present; 14 regions recovered per round in 46 s; the
recalled-reader arm exact; deleted stays deleted by the ONE classifier;
fsck clean; no venue-attributed reading); **round 4 RED — an ACKED-WRITES
LOSS, a NEW finding (§4.4af), FOUND and NOT FIXED in this fix round**: 25
of 17,376 fsynced files, every one of writer m60's and every one a file
its mover had renamed into the manager's striped cross-owner directory
`/storm-xo-r4` (`w60-f000086`, `w60-f000216`, `w60-f000344`, … — a
128-name stride, 15 names; 10 of them the mover's ledger says RETURNED),
each "at NEITHER its source nor its destination" at the manager after
the seven-victim recovery. The die that should have named it expanded an
UNBOUND `${survivors[0]}` (every joiner a victim) and the leg exited on
`survivors[0]: unbound variable` — the harness die is fixed (`4523f25e`),
the loss is the finding. **The storm count on the fix-round binaries is
therefore 7 + 3 GREEN rounds from zero, the ×10 NOT reached**, stopped
twice by product findings (the flip's projection walk, fixed; the
acked-loss class, open) and never by the venue-attributed gauge. Then
`sym-crash --rounds=1` on a fresh fleet: **RED on the WIDENED
deleted-stays-deleted arm — a second new finding (§4.4ag)**: joiner m60's
`stat` of the removed `acked-r1` answered `EIO`, not `ENOENT` — the read
went to the SUCCESSOR's token plane one second after its listener came
up and was refused "holds no live membership lease with this set's
owner" (the joiner's reclaim had been refused `Connection refused` a
second earlier and had not yet re-asserted); the old arm read that EIO
as "deleted". Every other law of the round held (the acked-writes oracle
2,281 / 2,281, the reader followed as a member, the member worker
enrolled). Then `sym-scale` once (exit 0, PUBLISHED): every N completes
(7,019 / 18,515 / 24,350 / 19,375 creates/s — SCOPING), handovers 0,
`slot_ships` ≤ 5, Σ `dlm_rpcs` 0, the widened deleted-stays-deleted arm 0
/ 3,000 at the manager AND the remounted joiner, fsck clean. The manager's
log of batch 2 also carried 23,434 `ForeignSlotFileMutation` refusals —
the kernel's SETATTR times ECHO on the oracle's 14 k foreign reads per
round, refused as a mutation by the Issue-12 gate; the gate absorbs the
echo class now (`4523f25e`, `foreign_file_times_echo_absorbed`; the pin
gained the echo and `touch` arms).
**`sym-crash`: 10/10 GREEN on NINE consecutive from-zero runs (attempts
7–15) — the ONE count this record carries** (§0, §2, §9 say the same
number); the attempt → binary pairs, each `target/release/squeezefs` of
the code named (the SHA the binary reports; where a docs-only commit sat
on top the record names the code's SHA beside it): attempt 7 `8d7fd3c0`
(`pr13-batch7`), 8 `f38776a7` (`pr13-batch8`), 9 `649d80f7`
(`pr13-batch9`), 10 `ad91d694-dirty` (= `5b0ec0be`'s code,
`pr13-batch10`), 11 `8c992af6`, 12 `7ac89d24`, 13 `87461d56`, 14
`24bbb195`, 15 `d414ca9c` (= `702ab502`'s code, `pr13-batch15`); the
three exit-2 codes (attempts 12–14) are the harness-edit shifted-tail
class after the 10/10 verdict line, never a round (the harness law, §4.6). Fix round 1 added a
1/1 mechanism round on the fix-round binary (§3.8b).

### 3.9 The box brackets (the box-rows rung, `perf/sym-box-rows`, 2026-09-22) — the venue was reclaimed 2026-09-19 and restored by the owner 2026-09-22 01:07 UTC; the brackets ran after that

> **History (kept as the record of what happened, read 00:20 UTC):** the
> section below down to "The procedure" is the venue finding as written
> before the owner restored the box. **At 01:07 UTC the owner rebooted
> `squeeze-test` into `6.19.14-sqz`** (grub default restored, no Lustre
> mount, docker inactive, no daemon; verified at 01:09 — up 1 min, load 0)
> and the brackets ran as §3.9.1 onward records.

**What this rung was to run** (the brief off §8 / §9): gate 1's solo
re-gate A-B-B-A with PR 1's rig verbatim (arm A `3228fcb8` flat vs arm B
= PR 13b's tip flat), then ONE A-B-B-A per row set for gates 2 / 3 / 3b /
3c / 5 / 7 on the armed plane (`format --symmetric` +
`SQUEEZEFS_SYMMETRIC_META=1` — identical to the post-flip default, so
valid inputs to PR 14), on `squeeze-test`, the box left unmounted after.

**What was found instead (box state read 2026-09-22 00:20 UTC, nothing
changed on it):**

| read | value |
|---|---|
| running kernel | **`4.18.0-553.123.1.el8_lustre.ddn17.x86_64`** — the DDN Lustre client kernel, not `6.19.14-sqz` (which is still installed, grub index 0, no longer the default: `grubby --default-kernel` = the ddn17 kernel) |
| the act (journal boot −1 + `/root/.bash_history`; root from **`10.179.193.136`**, not the program's usual `10.187.129.115`) | 2026-09-19 21:00 UTC: a boot into `6.19.14-sqz` (21 min); 21:04–21:19 `rpm -Uvh lustre-2.14.0_ddn255-1.el8 kmod-lustre-2.14.0_ddn255-1.el8`; **21:21:54 `grubby --set-default=/boot/vmlinuz-4.18.0-553.123.1.el8_lustre.ddn17.x86_64`; 21:22 `reboot`**; 21:50–22:16 `lustre_rmmod; systemctl start lnet; modprobe lustre; mount /s3ds; systemctl start docker; s3ds status; s3ds_cluster status --all` |
| current role | Lustre `10.181.177.101@tcp,…:/memexa01` mounted at **`/s3ds`** (`rw,flock,user_xattr,lazystatfs,encrypt`); `lnet` active; docker active, the **`s3ds` container "Up 2 days"**; `s3ds status` → "Mount points configured correctly ✓ / Docker functioning correctly ✓ / S3ds containers not running"; load 0.00 |
| the fabric, client side | no nvme host module loaded, no `/dev/nvme*` — the reset script's `nvmeof connect` has not run on this boot |
| the fabric, target side (the reset script's five nodes, read over its root ssh from the box) | **INTACT and idle**: `aqr37 / aqr38 / aqr39 / aqs38 / oss2` (all `4.18.0-553.123.1.el8_lustre.ddn17`), nvmet subsystems `nqn.2026-07.io.squeezefs:<node>-{m0,d0,d1}` shared, `/scratch/tmp/squeezefs` (`1.1.0`, Sep 1) present on every node |
| `/scratch/tmp/` on the box | intact: `cluster_reset_v4.sh`, `fio_jobs/`, `rigs/` (PR 1's rig + reducer + mdstorm), `squeezefs` (PR 1's arm-B copy `a9827378`), `sym-pr1/` (PR 1's rows), `logs/`; 251 G free |

**Why no row ran.** FUSE-over-io_uring needs the sqz kernel (the request
hot path has no classical fallback — the mount fails on a 4.18 kernel by
design), so no squeezefs row can run on the box as it stands; and
rebooting it back into `6.19.14-sqz` would take down another party's
deliberately configured Lustre / S3DS service on a shared field machine
— the brief's stop-and-report class ("do not improvise a substrate"),
and a reboot is the owner's call, not a measurement rung's. The box's
root ssh, `sudo -n`, the storage nodes and the reset script all still
work, so the venue is one owner decision away (restore the grub default
to `/boot/vmlinuz-6.19.14-sqz` and reboot, or another sqz-kernel box),
not rebuilt.

**The arms — BUILT, checksummed, staged on the laptop, NOT shipped**
(`/tmp/grok-justin/box-rows/arms/`, `SHA256SUMS.local`):

| arm | code | binary identity (`--version`, read in-container by the artifact check) | sha256 | provenance |
|---|---|---|---|---|
| **A** | `3228fcb8` (the pre-program dev tip PR 1's bracket used — comparability with `.benchmarks/2026-09-13-sym-pr1-solo-regate.md`) | `squeezefs 1.2.4 (3228fcb8…) built 2026-09-19T13:23:09Z profile release` | `993100757bde274d729f4d0fb7956b885af358b4acc3db079dcef90cac4d69b1` | REUSED from PR 13's own arm-A build (`/tmp/pr13-armA/dist/rocky8/`, a linked worktree at `3228fcb8`; §8 of this record named it valid) — no rebuild |
| **B** | `7b2ef9e9`'s code (the worktree HEAD `088e8c4a` is one docs commit on top — the binary embeds `088e8c4a`) | `squeezefs 1.2.4 (088e8c4a…) built 2026-09-22T00:20:14Z profile release` | `ea41d9e57bfe51b26559511a2055775324354ae57cb1508ae4457686662ef692` | `task build:rocky8` from the rung's worktree (3 m 25 s on the cached target volume; artifact checks passed: glibc ≤ 2.28, the shim embeds the same commit) |

Both arms are the `release` profile (the two-profile law — both legs the
same profile). Shims `libsqueezefs_il-{A,B}.so` beside them (no shim row
is planned; they ride along as the KD-7 pairing unit).

**The procedure, ready for the day the venue is back** (every command
below is what this rung would have run; none ran):

1. *Venue*: `ssh squeeze-test`; `uname -r` must read `6.19.14-sqz`
   (`grubby --set-default=/boot/vmlinuz-6.19.14-sqz` + a reboot is the
   owner's act); `pgrep -x squeezefs` empty; `/scratch/tmp/squeezefs`
   replaced by arm B (the reset script's `SQZ` — PR 1 placed `a9827378`
   there); `scp` both arms + `SHA256SUMS.local` to `/scratch/tmp/sym-box/`,
   `sha256sum -c`, `--version` on the box.
2. *Gate 1* — PR 1's rig VERBATIM, one A-B-B-A: `sudo env
   BIN_A=/scratch/tmp/sym-box/squeezefs-A BIN_B=/scratch/tmp/sym-box/squeezefs-B
   RT=60 OUT=/scratch/tmp/sym-box/rows-<ts> bash
   /scratch/tmp/rigs/2026-09-13-sym-pr1-solo-regate.sh` (default `SEQ="A B B A"`,
   rows `mdstorm mount wfresh-kern rr4k-kern rw4k-kern remount`); reduce with
   `python3 /scratch/tmp/rigs/2026-09-13-sym-pr1-solo-regate-reduce.py <OUT>`
   (the verdict rule: within noise iff |B/A − 1| ≤ max(band, 3 %); a DELTA
   row gets ONE reversed re-run). The B arm here is the FLAT shipped path
   on PR 13b's binary — every shipped-bug fix of PRs 1–13b in, so a real
   A/B, and the one number PR 1 carried forward (`rw4k` −1.5 %, §4.3 of
   PR 1's record) is re-read by this bracket.
3. *Gates 2 / 3 / 3b / 3c / 5 / 7* — the fleet legs with `--venue=box`
   (the must-stay-0 gauges are VERDICTS there). Two venue options, the
   owner's call:
   * **(a) the box's own tcp devsub** (`tests/mw_fleet.sh create N=2
     --symmetric --writers=7 --token-readers` on the box — nvmet-tcp on
     `127.0.0.1` with `resv_enable=1`, null_blk metadata, zram data): the
     design's gate-3 venue as written ("on the tcp devsub; squeeze-test
     brackets") and the D-5 fleet precedent on this box
     (`.benchmarks/2026-09-08-d5-fleet-squeeze-test.md`); one box, one
     memory bus, so the ingest column is the box's data path — a 32-core
     Xeon without heat soak, which is what the laptop lacked. The driver
     `.benchmarks/rigs/2026-09-21-sym-box-brackets.sh` runs this shape:
     fleet A (`N=2 --symmetric --writers=7 --token-readers`) for gates 2 /
     3 / 3b / 3c / 7 and fleet B (`N=32 --symmetric --writers=1
     --token-readers`, the 1 × 31 broadcast) for gate 5, `REPEATS` = 2
     positions per leg (the same-arm band; the gate-2 leg is itself an
     A-B-B-A), each leg's `$STATE/rows` + log + thermal + dmesg collected
     under `$OUT/<gate>-r<i>/`, each fleet torn down to zero residue, a
     `SUMMARY.txt` of every verdict and table; its preflight REFUSES a
     non-sqz kernel loud (this rung's finding made a rail) and a busy box.
     **Plumbing smoke on the laptop (`SMOKE=1`, 1 joiner, `sym-scale
     --scale-ns=1,2 --sym-files=2000 --ingest-mb=64`): rc 0 — fleet up,
     the leg PUBLISHED with `--venue=box`, rows collected, teardown to zero
     residue, 1 m 45 s wall; no number from it stands** (the rig labels a
     SMOKE summary so). On the box: `rsync tests/ .benchmarks/rigs/` to
     `/scratch/tmp/sym-box/repo/{tests,.benchmarks/rigs}/`, then `sudo env
     BIN=/scratch/tmp/sym-box/squeezefs-B TAR_SRC=/scratch/tmp/sym-box/linux/fs
     OUT=/scratch/tmp/sym-box/brackets-<ts> bash
     /scratch/tmp/sym-box/repo/.benchmarks/rigs/2026-09-21-sym-box-brackets.sh`
     (the linux `fs/` corpus must be shipped too — gate 2's instrument; the
     box has no internet).
   * **(b) the REAL fabric** (the reset script's 5 × {m0, d0, d1} over
     nvme-tcp): the storage nodes' `4.18.0-553.123.1` nvmet has **no
     `resv_enable`** (probed on `aqr37-d0`'s namespace: the knob is
     absent), so the armed plane's rung 4 needs the loud lab opt-in
     `SQUEEZEFS_SYM_ALLOW_NON_PR=1` (KD-SYM-13 — detection-grade fencing,
     announced at every mount) — the rates are the fabric's but the row
     is not the PR-substrate row gate 4 names. It also needs a harness the
     tree does not have: the fleet's mount recipe over the fabric's
     `/dev/nvme*` heads under the box's default host identity (the
     `tests/cluster_reset_v5_mw.sh` co-located shape, adapted to `format
     --symmetric` + the join ladder) — NOT written in this rung.
   * The **A arm for gates 3 / 3b** the brief names (the SHIPPED
     authority + co-writers at the same N on the same binary): no leg
     runs it — `sym-scale` is self-relative to its N = 1 row and
     `sym-shared-dir` asserts the flip; an `mw-scale` / `mw-shared-dir`
     pair over `create N=1 --multi-writer --cowriters=K` is a harness
     item stated here, not written (the design's gate 3 is stated
     against N = 1 and needs no A arm; gate 3b's "vs today's
     authority+co-writers" does).
4. *After*: `pkill -x squeezefs; umount -l /scratch/tmp/test`, `pgrep`
   empty, the fleet torn down to zero residue, every placed file listed
   in §8.

**What the venue's loss does NOT change.** Every mechanism law is GREEN
on PR 13b's binary from zero (its summary: `sym-foreign-file` ×3,
`sym-crash` 3/3, `sym-storm` 3/3 at seven victims, fidelity `quick`
130/0, the 42-suite matrix both legs); the three product blockers of §9
are closed there. The rate half of gates 1 / 2 / 3 / 3b / 3c / 5 / 7 is
exactly as owed as it was at PR 13's close — now with the arms in hand
and the venue's state on record.

#### 3.9.1 Gate 1 — the solo re-gate on the restored box (2026-09-22 01:12 → 02:07 UTC): **MISS on the mdstorm write phases (mkdir / rename / unlink −3.4…−5.8 %, DELTA in BOTH brackets, both orders); the data-plane rows within noise with a reproducible residual (rr4k −1.8 %, rw4k −2.5…−3.8 % — at the 3 % floor); `w_fresh`'s bracket-1 DELTA did not reproduce**

**Gate 1 verdict (the two brackets cited, the PR 1 rule):**

| row | bracket 1 (A B B A) B/A · band | bracket 2 (B A A B) B/A · band | positions (bracket 1 ; bracket 2) | **verdict** |
|---|---|---|---|---|
| `wfresh-kern` MiB/s | 0.950 · 3.7 % DELTA | **1.000 · 1.2 %** | A 36,298 / 34,993 ; 34,184 / 34,271 — B 33,942 / 33,811 ; 34,453 / 34,025 | **within noise** — the bracket-1 delta is A1's 36,298 (the first row after the reboot, 1,065 GiB moved vs 991–1,026 on the other seven positions); six of eight positions sit at 33.8–34.5 GB/s on either binary |
| `rr4k-kern` IOPS | 0.982 · 0.5 % | 0.983 · 0.7 % | A 603,015 / 605,847 ; 606,347 / 602,252 — B 593,213 / 593,357 ; 594,632 / 593,694 | **within noise (3 % floor) — REPRODUCIBLE −1.7…−1.8 %**: every B position 593.2–594.6k, every A 602.3–606.3k, both orders, both brackets (PR 1's §4.3 carried residual, read again at the same sign) |
| `rw4k-kern` IOPS | **0.962 · 1.1 % DELTA** | 0.975 · 1.4 % | A 542,637 / 536,596 ; 530,267 / 537,977 — B 520,361 / 517,480 ; 520,740 / 520,404 | **at the floor — REPRODUCIBLE −2.5…−3.8 %**: every B position 517.5–520.7k (a 0.6 % spread), every A 530.3–542.6k; DELTA in bracket 1, inside the 3 % floor in bracket 2 — not convicted by the rule, not cleared: PR 1's −1.5 % residual has GROWN |
| `mount` / `remount` / `umount` s | 1.009 / 1.051 / 1.100 | 1.150 / — / — | 0.37–0.57 s / 0.56–0.69 s / 7.6–10.0 s | within noise (0.4–0.7 s events, 17–29 % bands; the 8–10 s clean unmount after `rw4k` is PR 1's §9.3 term, both arms) |
| `mdstorm mkdir` ops/s | **0.965 · 2.4 % DELTA** | **0.942 · 5.2 % DELTA** | A 6,725 / 6,834 ; 6,775 / 6,853 — B 6,466 / 6,625 ; 6,254 / 6,588 | **DELTA — MISS** (B −3.5 % / −5.8 %; every B position below every A position) |
| `mdstorm create` | 0.988 · 2.5 % | 0.976 · 4.1 % | A 5,730 / 5,877 ; 5,925 / 5,796 — B 5,770 / 5,695 ; 5,604 / 5,838 | within noise (−1.2 % / −2.4 %) |
| `mdstorm stat` | 1.015 · 1.2 % | 0.998 · 0.6 % | 187–192k both arms | within noise |
| `mdstorm rename` | **0.966 · 3.3 % DELTA** | **0.948 · 3.8 % DELTA** | A 4,365 / 4,512 ; 4,550 / 4,521 — B 4,312 / 4,265 ; 4,218 / 4,382 | **DELTA — MISS** (B −3.4 % / −5.2 %; every B below every A) |
| `mdstorm unlink` | **0.965 · 1.5 % DELTA** | **0.960 · 2.5 % DELTA** | A 5,163 / 5,242 ; 5,306 / 5,235 — B 5,051 / 4,986 ; 4,995 / 5,123 | **DELTA — MISS** (B −3.5 % / −4.0 %; every B below every A) |
| `mdstorm manydirs` | 0.990 · 2.7 % | 0.981 · 1.1 % | A 10,912 / 11,206 ; 11,035 / 10,910 — B 10,871 / 11,026 ; 10,766 / 10,765 | within noise (−1.0 % / −1.9 %) |
| `mdstorm rmdir` | 1.015 · 9.0 % | 0.978 · 4.6 % | 5,451–5,963 both arms | within noise |

**Gate 1 as the design states it ("within noise of `dev` tip on mdstorm,
rand-4k, `w_fresh`, and mount time; `dlm_rpcs == 0`") reads: `dlm_rpcs`
0 ✓ (every leg), mount time ✓, `w_fresh` ✓, rand-4k ✓ by the rule with a
reproducible 1.8–3.8 % residual at the floor, **mdstorm ✗ — the three
write-heavy phases are 3.4–5.8 % slower on PR 13b's flat path in both
orders of both brackets.** A PRODUCT finding (a flat-path regression
accumulated over PRs 2–13b), attributed below to the extent the captured
`.stats` allow; reported to the orchestrator; nothing in the tree was
changed for it. The verdict is not softened: the same rows read
0.985–1.033 at PR 1 (`.benchmarks/2026-09-13-sym-pr1-solo-regate.md`),
so the regression landed between `a9827378` (PR 1) and `7b2ef9e9`.

##### 3.9.1-details — the two brackets

**Venue (the same as PR 1's row, re-verified 01:09 UTC):** `squeeze-test`
(`memp-s3ds-aqs-37`), 32-core Xeon, 251 GiB, Rocky 8.10, **kernel
`6.19.14-sqz`** (the sqz series incl. patch 0031 — the per-queue bg budget),
up 1 min and idle at the first leg (load 0.02); the reset-v5 converged
fabric (`/scratch/tmp/cluster_reset_v4.sh`, run once by hand to verify —
rc 0, 15 namespaces connected, format complete — then per arm by the
rig): 5 storage nodes × (1 meta + 2 data) memory-backed null_blk
namespaces over nvme-tcp, two paths each, cache-less format; the storage
nodes on `4.18.0-553.123.1.el8_lustre.ddn17` (nvmet targets — fine for
this row, no PR needed on the flat path). Sector 0 `features_incompat =
0xffd7`, bit 17 = 0 on every meta volume of every arm (the rig reads it).
Transport geometry identical on every leg of both arms: `queues=32
depth=32 payload_sz=1048576 max_write=1048576 max_pages=256
buffers=kmbuf-bufring+zero-copy+retention kmbuf_ops=37/38 (6.19-sqz)
sqpoll=off`. **Instrument:** PR 1's rig VERBATIM
(`/scratch/tmp/rigs/2026-09-13-sym-pr1-solo-regate.sh`, diff-identical to
the tree's) + its reducer; `fio-3.36`, the box's standing job files
(libaio, `direct=1`, 24 jobs, `ramp_time=10`): `write_BW` 1 MiB × qd 16,
`rand{read,write}_iops` 4 KiB × qd 8. **The fio window is 30 s measured
+ 10 s ramp per row, NOT 60 s — a rig finding (§3.9.1b): the job files
carry `runtime=30` and a job-section value overrides the CLI's
`--runtime=60`, so PR 1's rows (which state "RT=60") ran the same 30 s
window — the two brackets are comparable to each other and to the
campaign rows, and neither meets the ≥ 60 s sustained-state rule; the
bw-log flatness column stands in.** Arms: **A** = `3228fcb8`, **B** =
`7b2ef9e9`'s code (§3.9's table — both `release`, same `rustc 1.98.0
(88d9e12a 2026-08-18)`; A's binary 627.5 MB, B's 857.0 MB with
debuginfo). Order: **bracket 1 A B B A** (01:12:58 → 01:38:57 UTC, all six
rows, `rows-gate1-20260922-011258`); **bracket 2 B A A B** (the reversed
re-run of the DELTA rows + `rr4k`, 01:40:30 → §3.9.1a). Verdict rule =
PR 1's: `within noise` iff |B/A − 1| ≤ max(band, 3 %), else `DELTA` and
one reversed bracket before any verdict.

**Bracket 1 (A B B A) — every row:**

| row | primary | A1 | B2 | B3 | A4 | median A | median B | **B/A** | band | verdict |
|---|---|---|---|---|---|---|---|---|---|---|
| `wfresh-kern` (write_BW, the fresh set) | MiB/s | 36,298 | 33,942 | 33,811 | 34,993 | 35,645 | 33,877 | **0.950** | 3.7 % | **DELTA** (B −5.0 %) |
| `rr4k-kern` (randread 4 KiB) | IOPS | 603,015 | 593,213 | 593,357 | 605,847 | 604,431 | 593,285 | **0.982** | 0.5 % | within noise (the 3 % floor; B's two positions agree to 0.02 %, A's to 0.5 % — REPRODUCIBLE −1.8 %, PR 1 §4.3's carried residual, re-read in bracket 2) |
| `rw4k-kern` (randwrite 4 KiB, the W1 patch arm) | IOPS | 542,637 | 520,361 | 517,480 | 536,596 | 539,616 | 518,920 | **0.962** | 1.1 % | **DELTA** (B −3.8 %) |
| `mount` (fresh format → ready) | s | 0.372 | 0.420 | 0.458 | 0.498 | 0.435 | 0.439 | 1.009 | 29.0 % | within noise |
| `remount` (populated set → ready) | s | 0.616 | 0.687 | 0.562 | 0.572 | 0.594 | 0.625 | 1.051 | 20.0 % | within noise |
| `umount` (clean, after `rw4k`) | s | 7.589 | 7.844 | 9.972 | 8.612 | 8.101 | 8.908 | 1.100 | 23.9 % | within noise |
| `mdstorm mkdir` 20k | ops/s | 6,725 | 6,466 | 6,625 | 6,834 | 6,780 | 6,546 | **0.965** | 2.4 % | **DELTA** (B −3.5 %) |
| `mdstorm create` 100k | ops/s | 5,730 | 5,770 | 5,695 | 5,877 | 5,804 | 5,732 | 0.988 | 2.5 % | within noise |
| `mdstorm stat` 100k | ops/s | 189,280 | 190,079 | 191,908 | 187,012 | 188,146 | 190,994 | 1.015 | 1.2 % | within noise |
| `mdstorm rename` 100k | ops/s | 4,365 | 4,312 | 4,265 | 4,512 | 4,438 | 4,288 | **0.966** | 3.3 % | **DELTA** (B −3.4 %) |
| `mdstorm unlink` 100k | ops/s | 5,163 | 5,051 | 4,986 | 5,242 | 5,202 | 5,018 | **0.965** | 1.5 % | **DELTA** (B −3.5 %) |
| `mdstorm manydirs` 100k | ops/s | 10,912 | 10,871 | 11,026 | 11,206 | 11,059 | 10,948 | 0.990 | 2.7 % | within noise |
| `mdstorm rmdir` 20k | ops/s | 5,451 | 5,768 | 5,813 | 5,963 | 5,707 | 5,790 | 1.015 | 9.0 % | within noise |

**Engagement / tripwires — every leg of both arms:** `dlm_mode` `solo`,
**`dlm_rpcs` 0** on all 12 mount legs (the rig's exit-3 law: `gate_failed=0`),
`mount_posture` `writer`, Δ`invariant_tripwires` (+ the eight sibling
tripwires) 0 on every row, Δ`fsck_findings` 0, Δ`meta_kv_block_refs_drift`
0, `meta_kv_forest_{slot_trees_minted,root_publishes,key_violations,
reader_window_skips,reader_unpublished_children}` **0** on every B mount
(the bit-17-absent volume takes the shipped path), `write_enospc_refusals`
/ `rewrite_shadow_fence_drops` / `write_pipeline_fence_drops` /
`fuse_op_watchdog_overdue` / `stale_binding_escalations` / the reclaim
`sync_drains` / `fence_halts` / `cap_parks` **0** on every write row.
dmesg: the same five boot-time lines after every row, nothing logged
during any row. Thermal: the hottest hwmon sensor 47–51 °C at every row
start AND end (no heat soak — the venue law's point). The metadata
economy per row is identical arm to arm: `Δjournal_entries` 54.9–55.3k
(`wfresh`), 48–50 (`rr4k`), 4,029–4,072 (`rw4k`), 547.6–547.7k (mdstorm);
checkpoints 200 / 65–67 / 200–205 / 70–73; `layout_publish_batches` 49,128
and `layout_delta_commits` 35,204–35,207 on every `wfresh` position.

**Write-amplification columns (the daemon's device-byte ledger ÷ fio's
user bytes; RAMP-INCLUSIVE — the `.stats` pair brackets fio's 40 s
while `io_bytes` counts the 30 s window, so ≈ 1.04–1.15× reads as 1.0×
device/user):** PR 1's rig snapshots no `/proc/diskstats` and runs no
`iostat`, so the `/proc/diskstats` face and `wareq-sz` are NOT in this
bracket (harness gap, §3.9.1b).

| row / position | user GiB | device write bytes ÷ user (daemon ledger) | `write_through_blocks` | reclaim `commands` / `discards` | discard bytes ÷ user | `block_free_elided_debt_bytes` |
|---|---|---|---|---|---|---|
| `wfresh` A1 / A4 | 1,065 / 1,026 | `rewrite_device_write_bytes` **1.054 / 1.043** | 4,845 / 3,021 | 13,644 / 11,796 ; 13,783 / 11,944 | 0.051 / 0.045 | 42.6 / 43.9 GiB |
| `wfresh` B2 / B3 | 1,001 / 991 | **1.035 / 1.041** | 4,213 / 3,513 | **32,131 / 50,356** ; 32,546 / 51,009 | **0.127 / 0.201** | 68.3 / 81.1 GiB |
| `rw4k` A1 / A4 | 62.1 / 61.4 | `patch_write_bytes` **1.144 / 1.141** (4 KiB in-place DMA per op; `patch_writes` ≈ ios) | 2,224 / 2,275 (the prep's residue) | 0 / 0 | 0 | ≈ 0 |
| `rw4k` B2 / B3 | 59.6 / 59.2 | **1.146 / 1.146** | 2,238 / 2,222 | 0 / 0 | 0 | ≈ 0 |

Device write bytes ≡ user bytes on both arms and both rows (no
amplification; the W1 arm and the CoW-rewrite arm behave identically).
**One behaviour difference on the flat path**: B's reclaimer issued
**2.4–3.7× the discard commands** of A on `wfresh` (32–50k vs 12–14k per
row, `block_free_trim_bytes` 0.13–0.20× user vs 0.05×) for the SAME
`block_free_reclaim_elided` count (243–264k on all four) and a higher
elided-debt drain (`block_free_debt_pressure_drains` 59 on B2, 0 on A) —
the discard-elision debt draining as device commands more often; ≈ 1k
commands/s against 25k+ 1 MiB writes/s, so not the throughput term, but
a flat-path delta to attribute (§3.9.1c).

**Attribution of the DELTA (the captured `.stats` pairs; no extra row
run):** the cost is a **uniform ≈ +1–1.5 µs of daemon CPU per FUSE op
on the data-plane handlers**, read three ways —

* `rw4k` (the W1 in-place patch: NO metadata commit per op, 4,0xx journal
  entries per row both arms): daemon µs/op **40.3 / 41.0 (A) → 42.2 /
  42.7 (B)** (+4 %); by class `fuse3-ur` (the FUSE-over-io_uring queue
  workers) 33.4 / 33.9 → 34.9 / 35.0 (**+1.2 µs/op**), `fuse3-tpc` 6.2 /
  6.3 → 6.5 / 6.9 (+0.4); `write_transport_phase_ns.transport_total`
  197–199 → 208 µs, `queue_wait` 66–67 → 70 µs; clat p50 249–251 → 261 µs
  (+4 %); `write_pipeline_phase_ns` (the 2.2k write-through blocks) par.
* `rr4k`: daemon µs/op **31.6 / 31.7 → 32.8 / 32.75** (+3.5 %);
  `fuse3-tpc` 12.9 → 13.6 (+0.7), `fuse3-ur` 18.7 → 19.1 (+0.4);
  `read_serve_phase_ns.total` 154 → 157 µs = `block_fetch` 148 → 151 =
  `zc_bridge_phase_ns.total` 147.4–147.9 → 150.3–150.9 (`msg_hop` 24.3–24.6
  → 25.0–25.1, `wake_hop` 26.0 → 27.7–28.0, `device_cq` 96 → 97 — the
  hops grow on a 74–75 %-busy box when the handler lanes carry more CPU
  per op); `read_transport_phase_ns.dispatch_lag` 26.0 → 27.6–28.4.
* `wfresh` (CPU-bound at the venue's nvme-tcp ceiling, box 71–72 % busy
  both arms): daemon µs per 1 MiB write 524 / 533 → 536 / 545 (+1–2 %);
  `publish_phase_ns.meta_commit` 434–435 → 456–489 µs, `publish total`
  663 → 683–769; `write_transport_phase_ns.dispatch_lag` 714–791 →
  819–854 µs, `queue_wait` 592–646 → 608–703; GiB moved in the window
  1,026–1,065 → 991–1,001.
* mdstorm: the three write-heavy phases −3.4…−3.5 % with identical
  journal entries / checkpoints / node appends (the same records, the
  same number of times — a per-op CPU cost, not an economy change);
  `stat` +1.5 %, `create` −1.2 %, `manydirs` −1.0 %, `rmdir` +1.5 %.

The shape is a fixed per-op overhead added to every FUSE op on the
UNARMED path (both data handlers AND the metadata write phases), not one
phase's term — the class PR 13's §2 named as "per-op atomic loads that
are behaviour-identical" plus whatever PRs 5–13b added at the unarmed
entry points (`token_reader_for`'s divert check on every metadata resolve,
`record_ship`'s home resolution at every setattr / layout publish, PR 9's
per-write `custody_use_enter` + `cached_lease_token`, the served-mutation
and recall sinks, `refuse_foreign_slot_open` at every write-intent open,
the new sharded phase families' recording). **Naming the site needs a
`perf record` A/B on the box — a follow-up row the orchestrator approves,
not this bracket's; the phase ledger above is the honest attribution this
rung has.**

**Bracket 2 (B A A B — the reversed re-run of the DELTA rows + `rr4k`;
01:40:30 → 02:07:19 UTC, `rows-gate1-20260922-011258-rev`, `gate_failed=0`):**

| row | primary | B1 | A2 | A3 | B4 | median A | median B | **B/A** | band | verdict |
|---|---|---|---|---|---|---|---|---|---|---|
| `wfresh-kern` | MiB/s | 34,453 | 34,184 | 34,271 | 34,025 | 34,227 | 34,239 | **1.000** | 1.2 % | within noise |
| `rr4k-kern` | IOPS | 594,632 | 606,347 | 602,252 | 593,694 | 604,300 | 594,163 | **0.983** | 0.7 % | within noise (3 % floor; −1.7 % reproducible) |
| `rw4k-kern` | IOPS | 520,740 | 530,267 | 537,977 | 520,404 | 534,122 | 520,572 | **0.975** | 1.4 % | within noise (3 % floor; −2.5 % reproducible) |
| `mount` (fresh) | s | 0.569 | 0.529 | 0.445 | 0.551 | 0.487 | 0.560 | 1.150 | 17.2 % | within noise |
| `mdstorm mkdir` | ops/s | 6,254 | 6,775 | 6,853 | 6,588 | 6,814 | 6,421 | **0.942** | 5.2 % | **DELTA** |
| `mdstorm create` | ops/s | 5,604 | 5,925 | 5,796 | 5,838 | 5,860 | 5,721 | 0.976 | 4.1 % | within noise |
| `mdstorm stat` | ops/s | 188,292 | 189,113 | 187,918 | 188,068 | 188,516 | 188,180 | 0.998 | 0.6 % | within noise |
| `mdstorm rename` | ops/s | 4,218 | 4,550 | 4,521 | 4,382 | 4,536 | 4,300 | **0.948** | 3.8 % | **DELTA** |
| `mdstorm unlink` | ops/s | 4,995 | 5,306 | 5,235 | 5,123 | 5,270 | 5,059 | **0.960** | 2.5 % | **DELTA** |
| `mdstorm manydirs` | ops/s | 10,766 | 11,035 | 10,910 | 10,765 | 10,972 | 10,766 | 0.981 | 1.1 % | within noise |
| `mdstorm rmdir` | ops/s | 5,670 | 5,939 | 5,929 | 5,934 | 5,934 | 5,802 | 0.978 | 4.6 % | within noise |

Engagement identical to bracket 1: `dlm_rpcs` 0 on all 8 mount legs,
tripwires / `fsck_findings` / `block_refs_drift` 0, the forest gauges 0
on every B mount, `features_incompat = 0xffd7` everywhere, dmesg the
boot-time lines only, hottest sensor 48–51 °C at every row start and
end; the per-row metadata economy identical arm to arm (`wfresh`
Δjournal 54,975–55,041 / 200 checkpoints; `rw4k` 4,066–4,100 / 205;
mdstorm 547.65–547.82k / 70–75). Daemon µs/op on the fio rows read the
same shift as bracket 1: `rr4k` A 31.7 / 31.7 → B 32.5 / 32.4, `rw4k` A
41.5 / 41.2 → B 42.6 / 42.3, `wfresh` A 520 / 529 → B 535 / 535. Write
amplification: `rewrite_device_write_bytes` ÷ user 1.05–1.06× (A) /
1.05–1.06× (B) on `wfresh`, `patch_write_bytes` ÷ user 1.14× on every
`rw4k` position (ramp-inclusive ≡ 1.0×); B's `wfresh` discard commands
again 1.9–3.9× A's (A2 12,836 / A3 13,077 — B1 24,586 / B4 50,672).

**The mdstorm MISS, attributed from the eight legs' pre/post `.stats`
(both brackets):**

| leg | daemon µs/op (540k ops) | `fuse3-tpc` µs/op (the handler lanes) | `sqz-jrnl` / `sqz-meta` | conveyor `pass_total` / `window_total` µs | **`lock_phase_ns.dlm_guard_hold` count** | `dlm_guard_wait` count | journal entries / checkpoints / node appends |
|---|---|---|---|---|---|---|---|
| A1 / A4 / A2 / A3 | 229.6 / 224.0 / 225.3 / 228.5 | 125.5 / 124.8 / 124.5 / 125.3 | 36–38 / 17–19 | 38–39 / 56–61 | **889–890k** | 4k | 547.6–547.7k / 70–72 / 24.2–24.8k |
| B2 / B3 / B1 / B4 | 232.4 / 232.1 / 234.1 / 230.1 | **130.1 / 128.0 / 129.6 / 127.9** | 37–38 / 18 | 39 / 58–62 | **989–990k** | 3–4k | 547.7–547.8k / 72–75 / 24.5–24.9k |

* The conveyor pass, the journal lane and the meta lanes are IDENTICAL
  arm to arm (`pass_total` 38–39 µs, `window_total` 56–62 µs, `sqz-jrnl`
  36–38 µs/op, the same 547.7k entries and 96 splits) — the regression is
  not in the commit path.
* **The handler lanes (`fuse3-tpc`) carry +3–5 µs per op on B** (124.5–125.5
  → 127.9–130.1), i.e. the whole daemon µs/op shift (+1.5–2.5 %), and it
  lands on the storm's wall as −3.4…−5.8 % on the three phases whose
  handler work is largest.
* **B takes ≈ 100,000 MORE 4a DLM guards per storm** — `dlm_guard_hold`
  889–890k (A) vs 989–990k (B) over the same 540,000 ops, i.e. exactly
  one more guard per op of one 100k-op phase: the shape of PR 4 round 2's
  rename lock-set fix (`RoutedMetaBackend::rename` now locks `I{moved}`
  (+ `I{dest}`) beside the two parents and the two `D{}` keys, in the
  unlink path's two-phase discover → `lock_many` → revalidate law — a
  SHIPPED-BUG fix, the co-queued `Delta`/`Put` hole, pinned by
  `tests/rename_lock_set_tests.rs`), which also adds the second lookup
  per rename — the `rename` phase's −3.4…−5.2 %. `mkdir`'s and `unlink`'s
  terms are not named by a gauge here: candidates are the per-directory-
  mint memo feed (`dir_parents`, PR 6 / PR 13 defect 37), the unarmed
  `record_ship` / `token_reader_for` / stripe-map checks at the routed
  entry points (PRs 5, 7b, 12b, 13b), and the same per-op atomics the
  data rows pay.
* `fold_memo_misses` (14–28k) and `node_compactions` (1,533–1,664) scatter
  the same on both arms; `meta_reclaim_inflight_waits` 25–70 both.

**Naming the mkdir / unlink sites needs `perf record` on the box (one
mdstorm leg per arm under `perf record -g`) — a follow-up row for the
orchestrator to approve; this rung's attribution is the ledger above.**
The finding is a PRODUCT regression on the flat path: the design's gate
1 says "within noise", and −3.4…−5.8 % in both orders of two brackets on
1.5–5.2 % bands is not noise. Reported; nothing changed in the tree for it.

**→ PR 13c (`fix/sym-box-campaign`, the gate-1 commit) — attributed, three
unarmed-path costs deleted, the rename fix's cost priced.** (i) The laptop's
in-process routed+KV microbench (`tests/meta_flat_path_microbench.rs`, 4
threads × 40 k ops, A-B-B-A against `3228fcb8`, SCOPING): create / mkdir /
unlink at par (±0.5 of 23–26 µs/op), rename +0.4–1.6 µs (the PR-4 lock-set
fix's second lookup + `I{moved}` guard), lookup / getattr +30–80 ns. (ii) The
laptop mount A-B-B-A (`run_mdstorm.sh`, scale 25, `SQUEEZEFS_OP_PROFILE=1`,
SCOPING): `dlm_guard_hold` +24.8 k per leg = +1 guard per rename (the box's
+100 k at scale 100); `fuse3-tpc` 23.5 → 24.65 µs per FUSE op (A → B, 459 k
ops per leg); `unlink.backend` 117–123 → 125 µs, `rename.backend` 51–53 →
54–56, `lookup.backend` 5.8–6.0 → 6.2–6.4. (iii) **The ONE approved `perf
record` leg per arm on the box** (the flat mdstorm at scale 50 on `/dev/shm`,
45 s of the daemon at 499 Hz; `fuse3-tpc*` samples 24,801 (A) → 25,384 (B),
+2.4 %): `__memmove_avx512_unaligned_erms` +280 samples (782 → 1,062, +36 %
— HALF the delta; glibc stops the frame-pointer chain, the callers are
unnamed), the `RouteTable` arc-swap loads +78 (`arc_swap::debt::LocalNode::
with<HybridProtection<Arc<RouteTable>>>` — the added `route_ino` calls at
the routed entry points), moka +71 / jemalloc +72 (the memo feed's insert +
`Arc<str>` per mkdir and the attr cache's policy churn), `DlmLockManager::
lock_stripe` +32 (the rename guard), `token_serve::{closure}` +30 (the read
verbs' divert layers); `event_listener` −187 (retired for `sqz_notify` —
a win). Fixed: `stripe_map` / `stripe_unlink_prelude` answer an unarmed set
in one `Option` probe per volume BEFORE any route-table load; the
directory-parent memo is fed on an ARMED set only (no moka insert, no
`Arc<str>` per `mkdir` on a flat mount — pinned:
`an_unarmed_mount_feeds_the_directory_parent_memo_nothing`); `token_serve`
returns a flat volume in two probes. The laptop's C-A-A-C re-read on the
fixed tree (SCOPING): `fuse3-tpc` 24.35 µs/op (C) vs 23.5 (A) vs 24.65 (B),
`unlink.backend` back at par (115–123), `rename.backend` 55–56 (the lock-set
fix's cost, ≈ +2.6 µs — it STAYS: a shipped-bug fix), `lookup.backend`
6.1–6.2. **What remains for the box bracket on PR 13c's binary**: the rename
fix's priced cost and the memmove term (a per-op state-size growth of the
routed mutation futures across PRs 6 / 7b / 13b — nameable only with a
dwarf-unwound leg, not run). The venue law: no laptop number above is a
verdict.

**→ The memmove term: named by the box re-run's DWARF legs (§3.9.4.1) and
FIXED in PR 13f (`perf/setattr-future-economy`, 2026-09-22).** It is the
`handle_setattr` future's `Box::pin` + lane-handoff
move on the kernel's per-op ctime SETATTR echo. `size_of_val` at the FUSE
entry (`tests/meta_op_future_economy_tests.rs`, test profile, rustc
1.98.1): `SqueezefsFilesystem::setattr` **18,960 B (`3228fcb8`) → 26,016 B
(`77f4da1d`) → 896 B (PR 13f)**; `unlink` **7,616 → 11,088 → 280 B**;
`rename` 392 B on all three (never grew). The routed `setattr` / `getattr`
`async_trait` boxes the echo mints per op: 1,536 / 1,088 → 5,552 / 5,552 →
1,720 / 1,192 B (rustc `-Zprint-type-sizes`). The root, named by the type-
size dumps of both trees: NOT the routed setattr entry (a 16-byte box at
the FUSE call site) but `KvMetaBackend::commit_tx` 176 → 4,816 B — PR 4's
door `ensure_leases_for_tx` with its two first-touch acquire arms inline in
EVERY commit's future, so every layout-publish site grew ≈ 3.5–4.6 KiB and
the truncate arm (two publishes — the tail-zeroing staged write + the layout
prune) +7 KiB, the unlink handler's overlay drain (one) +3.5 KiB; the echo
never takes either arm and moved their state twice per op. The fix boxes
each such arm INSIDE its branch (the truncate arm as `setattr_truncate`, the
drain as `drain_unlink_target_overlays`, the door's two acquires, the PR 13b
ship behind the sync `slot_is_foreign`, `note_served`'s tail,
`getattr_local`'s striped fold, `token_serve_armed`): `commit_tx` 408 B,
`open` 26,224 → 2,064 B (its O_TRUNC fold runs the setattr arm), every
publish site within ≈ 300–400 B of pre-program (`merge_layout_and_size`
+272, `commit_block_refs` +416 — PR 13b's `publish_target` pair and PR 7's
forest ref ops), `write` 26,592 → 20,320 B (the striped-write arm's own
growth stays — the `rw4k` row's follow-on, same fix shape). No behaviour
change; pinned at 2× the fixed sizes (test-profile readings — a same-profile
tripwire; the type-size dumps are test-profile too (`cargo rustc --lib --profile test`); no release-profile size was measured). **Where the armed
arms now allocate**, exactly: the door's two acquires (once per slot per
mount, a durable control write already), the PR 13b ship (a wire round
trip), `note_served`'s tail (inside a served verb), `fold_striped_dir_attrs`
(once per DIRECTORY `getattr` on an armed volume — beside the fold's own
`stripe_map` KV read), and `token_serve_armed` ONLY for a read a plane will
serve: an armed solo writer's own-object read verb and every read verb of a
`SQUEEZEFS_SYMMETRIC_META=0` forest take the sync `writer_reads_locally`
exit off the gate's bits and allocate nothing for the divert (review round
1, Issue 1 — the first build boxed before deciding, one allocation per read
verb on the flip's default path; pinned by
`tests/sym_read_divert_economy_tests.rs`: 8,800 → 7,600 allocations over
400 × (`getattr` + `lookup`) on the armed writer AND the `=0` forest alike,
the remaining excess over flat — 9 per round — being PR 1's forest key
framing, stated as owed). The box's gate-1 bracket on the next flip binary
re-reads the row.

##### 3.9.1b Harness findings (PR 1's rig, both brackets)

1. **The fio window is 30 s, not the rig's `RT=60`.** The box's job files
   carry `runtime=30` (+ `ramp_time=10`, `time_based`), and fio lets a
   job-section value override a CLI `--runtime` given after the job file
   (the fio JSON records `global options: runtime=60` beside `job options:
   runtime=30`, `job_runtime` 30,040 ms): every fio row of PR 1's bracket
   AND this one measured 30 s after a 10 s ramp — comparable to each other
   and to the 2026-09-08 campaign rows, and PR 1's record's "RT=60 (60 s +
   10 s ramp)" was wrong. Neither meets the ≥ 60 s sustained-state rule;
   the bw-log flatness column (−20…+12 %, last third vs first) stands in.
   Fix (the tree's rig, after these brackets — the on-box copy stays the
   one that ran): `row()` writes a per-row job file with `runtime=$RT`.
2. **No `/proc/diskstats` snapshot, no `iostat`**: the write rows'
   amplification column is the daemon's own device-byte ledger
   (`rewrite_device_write_bytes`, `patch_write_bytes`, the `block_free_*`
   reclaim counters); the `/proc/diskstats` face and `wareq-sz` the
   AGENTS instrument names are not in either bracket. Fix alongside 1:
   snapshot the data namespaces' `/proc/diskstats` lines per row.
3. **The mdstorm quiet gate dominates the wall**: 26 min for six rows ×
   four arms, 27 min for the four-row reversed bracket — ≈ 3 min per arm
   is the load-1 decay wait after the previous arm's fio echo (PR 1 §1's
   note), not measurement.

#### 3.9.2 Gates 2 / 3 / 3b / 3c / 5 / 7 — the N-writer brackets on the box (02:11 → 02:34 UTC): **gate 2 MET (1.04–1.07× of S0), gate 3b MET on its laws (3,400–3,581 creates/s into ONE directory; `ls -l` 40,000 = K + C + 3 tokens), gate 3 MISS at N = 8 (4.27× creates / 5.33× ingest vs ≥ 5.6×), gate 7 row (a)/(b) MET on their laws at N = 8 — and THREE product findings that stop three row sets: `appender_flush_ceiling_overruns` trips on the box (1–32 ms past the ceiling), a LIVE holder recalled by a touch (3c), and the manager's cluster-wire connection cap (64 under `FLEET_SHARE=32`) that keeps a 32-member fleet from coming up (gates 5 and 7 at N = 32 cannot run on PR 13b)**

**Venue.** The same box and kernel as §3.9.1, the fleet legs on the
box's OWN tcp devsub (option (a) of §3.9's procedure — the design's gate-3
venue and the D-5 precedent; the real fabric's storage-node nvmet has no
`resv_enable` and `require_symmetric` refuses a non-PR guard mode):
`tests/mw_fleet.sh create N=2 --symmetric --writers=7 --token-readers`
(a manager, 7 joined writers = N up to 8, one token reader) — nvmet-tcp on
`127.0.0.1` (`resv_enable=1`), 2 memory-backed null_blk metadata
namespaces, 2 × 16 GiB zram data namespaces (**`lzo-rle`** — the box's
sqz kernel offers `lzo-rle`/`lzo` only; the devsub's default `zstd` refused
loud, the driver names the algorithm now), `SQUEEZEFS_FLEET_SHARE=9` per
daemon; the legs ran with **`--venue=box`** (every must-stay-0 gauge a
VERDICT). Driver: `.benchmarks/rigs/2026-09-21-sym-box-brackets.sh` (the
repo's `tests/` snapshot at `/scratch/tmp/sym-box/repo/`), arm B only
(`squeezefs-B` = `7b2ef9e9`'s code). Two passes: **pass 1**
(`brackets-20260922-021250`, one fleet for gates 2 / 3 / 3b / 3c / 7, then
the 32-member fleet for gate 5) and **pass 2** (`brackets-20260922-022526`,
gates 2 and 3b with a FRESH fleet per leg, after the harness findings
below). Wall: 23 min of box time for both passes. The box was left with
zero fleet residue (no `/run/squeezefs-mwfleet`, no daemon, no netns, no
policy rule), the fabric unmounted.

| Gate | Row / instrument | Position(s) | Number | Engagement (the law's gauges) | **Verdict (box)** |
|---|---|---|---|---|---|
| **2** | `sym-tarx` — `tar -x` of `linux-7.2.3/fs` (2,468 entries) by a JOINED writer in a netns at netem 125 µs/end (250 µs RTT) into a directory it created, vs the manager-local S0; each leg A-B-B-A (sym-1 local-1 local-2 sym-2) | r1: sym 2.14 / 1.90 s, local 1.94 / 1.83 s → **1.07×**; r2: sym 2.19 / 1.85 s, local 1.94 / 1.95 s → **1.04×** | **1.04–1.07× of S0** (≤ 1.10×); ≈ 1,125–1,350 entries/s; the FIRST sym leg pays the join over the shaped wire + its 30 manager verbs, the second is at par with local (1.85–1.90 vs 1.83–1.95 s) | `wire_verbs_per_entry` **0.0122 / 0.0000** (< 0.05; 30 verbs = the join + grant refills, then none), `xv`/`ship`/`pub` 0, `slot_handovers` 0, `dlm_rpcs` 0, oracle clean (fsck 0, C8 0, must-stay-0 flat) on both positions | **MET** |
| **3** | `sym-scale` — N = 1/2/4/8 RW mounts each creating 40,000 files (4 threads) in its OWN directory (`tests/mdstorm.c create`), then ingesting 1 GiB (`dd bs=4M conv=fsync`) | r1 only (the row set stopped — below): N=1 **5,058 c/s · 1,382 MiB/s**; N=2 **9,599 (1.90×) · 2,816 (2.04×)**; N=4 **16,340 (3.23×) · 4,074 (2.95×)**; N=8 **21,595 (4.27×) · 7,366 (5.33×)** | law ≥ 0.7 × N: N=2 ✓✓ (≥ 1.4), N=4 ✓✓ (≥ 2.8), **N=8 ✗✗ (needs ≥ 5.6×)**; per writer at N = 8: 2,700 c/s vs 5,058 alone (the manager's own storm included) | `appenders == N` per row, `slot_handovers` **0**, `slot_ships` 0 / 1 / 3 / 5 (≤ N — each joiner's `mkdir` under `/`), Σ `dlm_rpcs` **0**, `manager_load_pct` 3 / 1 / 1 / 0 %, the manager's process CPU 1,322–1,625 % (its OWN storm); **`appender_flush_ceiling_overruns` +1 on m60 (joiner) at N = 2 — 1,132 ms vs the 1,100 ms landing ceiling — and +1 on m0 during the leg (1,101 ms)**; the leg RED'd on it BEFORE its deleted-stays-deleted arm and fsck oracle (not run for this row) | **MISS at N = 8 (rate) + the must-stay-0 VERDICT (product finding F-B1)** — the row set stopped at r1 |
| **3b** | `sym-shared-dir` — 8 creators × 5,000 files into ONE directory held by m60 (the flip to K = 64 stripes on the holder's observed creator count); `-ls`: a COLD token reader's `readdir + stat` of the 40,000 children | r1: **11.17 s, 3,581 creates/s**; `ls -l` **65.32 s**. r2 (fresh fleet): **11.76 s, 3,400 creates/s**; `ls -l` **65.26 s** | 3,400–3,581 creates/s aggregate into one directory (= 425–450 per creator — a cross-owner create costs ≈ 2.2–2.4 ms: one S3.5 intent + one shipped step + the retirement, each a durable barrier; 6× below the own-directory storm's 21,595 at N = 8); `ls -l` 1.63 ms per child (one token grant round trip each) | **`dir_stripe_flips` = 1** at the holder (K = 64, `striped_dirs` 1), `xv_shipped ≡ xv_served` = 34,982 (r1) / both positions, `dir_stripe_ships` 34,901 (the 1/64 own-stripe lands are the difference), `slot_handovers` **0**; `-ls`: **`dlm_token_grants` 40,067 = K + C + 3** (64 + 40,000 + the directory, its parent, the root), `readdir_merges` 83, `node_cache_misses` 32 / 29 (the poll's dropped images over 26 epoch steps + ≤ K tree-0 reads — **0 data-leaf reads**), `token_hits` 21.7 M; oracle clean both positions | **MET on every law** (the design's "vs today's authority + co-writers" A arm has no leg — §3.9's stated gap; the rate stands as the box's number for the flip's ledger) |
| **3c** | `sym-foreign-touch` — holder m60 (A), requester m61, the manager (C); beat 10 s, bursts of 64 touches, 3 rounds per phase; LIVE / IDLE / PAUSED | r1 LIVE phase only | the LIVE phase's 192 touches SHIPPED (m60 `slot_ships` 0 → 192) but **`slot_handovers` = 1**: the manager's ledger `slot_offers` 1 / `slot_offers_busy` 63 / `slot_recall_notices` 1 / `slot_handovers` 1, m60 `slot_offers_dominated` 1, `slot_handover_phase_ns` total **13.4 ms** (flush 9.4, tree 0 3.8, page 0.14), `slot_leases_held` 64 → 63; **`slot_offer_n_floor` on the box = 2** at the phase start (the laptop's read 5) → 23 after the handover fed the EWMA | the law "a LIVE holder is never recalled by a touch" (§8 gate 3c; `a_live_holder_is_never_recalled`) — 63 of 64 offer evaluations judged the holder BUSY, one judged it DOMINATED on a slot the live storm wrote too little in the window (§7 item 10's premise: `ops_h` counts PER SLOT and the storm's children spill to the rotor at the `A_max` floor, so the touched slot reads idle beside a burst of 64) | **MISS (mechanism) — product finding F-B2**; the row set stopped (IDLE / PAUSED not run) |
| **5** | `sym-readers` — 1 writer × 31 `-o ro` token readers (`create N=32 --symmetric --writers=1 --token-readers`) | the fleet **never came up**: the create died at member 13 | at the 14th mount (13 token readers up, ≈ 5 wire sessions each to the manager) the manager logged **`cluster wire: refusing 10.181.177.194:36258 — 64 concurrent connections is the cap`** (02:22:28 and :31); the new reader logged `InvalidOperation("cluster wire: expected a Challenge first, got None")` twice and its `.stats` read answered **EINVAL** to the rig's readiness check (`membership_readers` 13 at the manager) | the cap: `cluster_wire::max_connections_from(process_parallelism())` = `(cpus × 16).clamp(64, 1024)` with `cpus` = the FLEET-SHARE-DIVIDED root (KD-MW-14): 32 / 32 = 1 → **64** on a 32-member fleet on 32 cores — the listener's cap SHRINKS as the fleet it serves grows; no knob overrides it | **BLOCKED — product finding F-B3**; the 1 × 31 broadcast, the token-grant / recall-ack p99 at N = 32 and `free_grace_hold_ms` under 31 readers are NOT measured on PR 13b |
| **7 (a)** | `sym-walls` row (a) — the relocated FREE wall: 7 joiners each pre-write then REWRITE 16 × 64 MiB in place at once; every displaced block ships to the allocation holder (the manager) | r1 only | **983 frees/s** served at the holder; rewrite wall **1.85 s** for 7 GiB (3.8 GB/s aggregate into zram); `manager_verbs` 329, `manager_service_ns` total **571 ms** (exec 570), `manager_load_pct` **1**, `manager_verbs_per_s` 4, the manager's process CPU 20 % | **`free_shipped_blocks` 1,824 ≡ `free_served_blocks` 1,824 ≥ 1,792 displaced** (+32 = the write-through residue), `free_ship_failures` 0, `free_refused_blocks` 0; the holder's bitmap `set_bits` 1,570 (the mints) / `clear_bits` 1,824 (the frees); device write bytes **7.00 GiB ≡ 6.95 GiB user (1.0×)**; `block_grant_topups` 326 across the joiners; **m60 `appender_flush_ceiling_overruns` +1** during the rewrite (1,132 ms) | **MET on its law; the must-stay-0 VERDICT tripped (F-B1)** — the row set stopped at r1 |
| **7 (b)** | `sym-walls` row (b) — the JOIN STORM: every joiner leaves, all rejoin at once, each `mkdir /jobs/<j>` shipping to `/jobs`'s holder | r1, **N = 7** (the design's N = 32 is F-B3's) | **3.92 s** to the last armed joiner; `manager_verbs` 55, `manager_service_ns` total **278 ms**, the manager's CPU 41 % over the storm; `manager_failover_bound_ms` 45,013 | `/jobs` ships **7 / 7**, `appenders_known` 8 after the storm | **MET at N = 7**; the N = 32 storm cannot run (F-B3) |

**F-B1 — `appender_flush_ceiling_overruns` trips on the acceptance
venue** (PR 2's KD-SYM-10 landing-ceiling audit; the §4.5 / §4.4aa
class PR 13 called "venue-attributed pending the box"): the box read it
**four times in 12 minutes** on the 7-joiner fleet — m60 at 02:14:10
(N = 2 of `sym-scale`: region 1's oldest dirty leaf 1,132 ms at the
covering barrier vs the 1,100 ms ceiling), m0 at 02:15:15 (region 0,
1,101 ms), m60 at 02:19:51 (the `sym-walls` rewrite, 1,132 ms) — with no
recovery in flight (the extension of defect 33 does not apply), a 47–48 °C
box at load 2–5. The venue law made the gauge a VERDICT on the box, and
the verdict is: **the fixed 2-tick margin over `CHECKPOINT_MAX_AGE_MS`
is too small for a joined appender under a create storm or a rewrite
burst — by 1–32 ms.** This is PR 14's owed derivation ("derive the margin
from the measured pass wall", §7 item 3), now with the box's number.
Consequence for the rows: the gate-3 and gate-7 row sets stopped at r1
(the legs die on it; `--venue=box` is the brief's word), and every later
leg on the SAME fleet died at its door until the driver recreated the
fleet per leg (harness finding H-B2).
**→ fixed in PR 13c** (`fix/sym-box-campaign`, the F-B1 commit + review
round 1's Issue 1): the audit judges a leaf on the time it aged with NO
structural hold of the SMO mutex by ANOTHER actor — the Σ of hold time
per class (`NodeEnv::holds`, stamped on the leaf at its dirty transition)
that OVERLAPPED the leaf's dirty window is excluded: **an overlap-bounded
exclusion, not the delay the pass suffered** (passes and holds serialize
on the mutex, so a hold spanning the pass's due tick delays it by its
remainder — the over-excuse is ≤ one cadence tick per hold), each class
CAPPED at a published bound — a RECOVERY's at `appender_recovery_bound_ms`
(defect 33's law kept), a SERVICE hold's (the manager's wire slot grant /
release, a transfer's adoption, a projection refresh, a region release) at
ONE landing ceiling (`appender_flush_ceiling_service_cap_ms`: the
contract the audit excuses against — a longer hold is the stall class the
ceiling's consumers, the free-grace qualify term and the `=0` reader,
must SEE, so the excess counts) — each excused leaf on its class's gauge
(`appender_flush_ceiling_{recovery,service}_extensions`), the excused Σ
and the largest single exclusion published
(`appender_flush_ceiling_excused_{ns,max_ms}`). **The pass's OWN wall is
never excused**: its device time, and its wait on a peer — the joiner's
in-pass reactive wire refill, which round 1 had registered as a service
hold (Issue 1b), is the pass waiting, not another actor holding. **This is
an exclusion, not the MARGIN's derivation** — §7 item 3 ("derive the
margin from the measured pass wall") stays PR 14's. Pin: `sym_appender_
tests::a_leaf_that_aged_under_a_service_hold_of_the_smo_mutex_is_an_
extension_not_an_overrun` (a hold inside the window excuses up to the cap;
a hold outside it excuses nothing and the parked device's overrun counts;
a hold PAST the cap leaves its excess as the overrun, `excused_max_ms ==
cap`). The box re-run of gates 3 / 7 is owed on PR 13c's binary.

**F-B2 — a LIVE holder was recalled by a touch (gate 3c's law).** The
holder m60 ran the harness's LIVE job (a storm under `job-wA/`) while the
requester m61 touched 64 foreign names per burst; PR 4's dominance rule
(`ops_q ≥ 2 × ops_h ∧ ops_q ≥ N_floor` over one `T_idle` window) judged
the holder BUSY on 63 evaluations and DOMINATED on one — the handover
followed (13.4 ms). On the laptop the same leg read 192 ships / 0
handovers with `N_floor` = 5; the box seeded `N_floor` = 2 (the cold-start
`max(2, ceil(ewma_handover / ewma_ship))` — a faster handover EWMA against
the ship EWMA). The premise PR 13 §7 item 10 named for the PAUSED phase
holds for LIVE too: `ops_h` is counted PER SLOT, the storm's children spill
to the rotor at the `A_max` floor, so the touched directory's slot reads
nearly idle beside a 64-touch burst. A design-rule finding (the two
levers §7 item 10 prices), not a harness one — the LIVE job is a real
storm under the slot's directory.
**→ fixed in PR 13c** (the F-B2 commit; design §5.1.4 amended as built):
`ops_h(S)` counts the holder's namespace ops on the SUBTREE rooted in S's
directories — the commit door notes every dentry-bearing commit on the
parent's ANCESTOR slots too (`RoutedMetaBackend::install_liveness_ancestry`
over the `dir_parents` memo, consulted behind the armed plane's `Option`),
so a job live anywhere below a directory keeps that directory's slot; the
IDLE arm, the trickle-holder reclaim and the crowd law are unchanged. Pin
(the box's exact shape, `N_floor` forced to 2): `sym_slot_transfer_tests::
a_holder_live_below_a_directory_is_never_recalled_by_a_burst_into_it` —
RED on the base at ship 2 (`OfferDominated`), GREEN with the fix. **The
fleet leg found the first build ONE VOLUME wide**: `sym-foreign-touch`
GREEN from zero on the laptop (LIVE 192 ships / 0 handovers), but its
PAUSED phase read the touched slot IDLE at the manager with the job's
storm one second old — the fleet has `MDS_COUNT=2`, `pick_mint_volume`
mints a directory's children round-robin, and the resolver SKIPPED an
ancestor on another volume. Second pin `…_on_another_volume_keeps_the_
directorys_slot` (a two-volume set, the storm directory on the other
volume): RED at ship 2 with the box's verdict, GREEN once an ancestor on
another volume is credited on THAT volume's plane
(`KvMetaBackend::note_subtree_holder_op`). The box re-run of gate 3c is
owed.

**F-B3 — the cluster-wire connection cap derives to 64 under
`SQUEEZEFS_FLEET_SHARE=32` and a 32-member fleet cannot come up.**
`RpcListenerConfig::max_connections` = `max_connections_from(cpus)` =
`(cpus × 16).clamp(64, 1024)` with `cpus = process_parallelism()` = the
raw parallelism ÷ the fleet share (KD-MW-14): on 32 cores with 32
co-located members the manager's listener admits 64 connections, and 13
token readers (each ≈ 5 sessions: per volume a token channel + the grant
session pool, plus membership) fill it — the 14th mount's dials are
refused at accept, its `.stats` read answers EINVAL to the rig, the create
dies. The derivation is inverted for a LISTENER: its load is the fleet's
width, which is exactly what the fleet-share divisor shrinks it by (the
laptop's 9-member fleets sat at the 64 floor by luck: 32 / 9 × 16 = 48 →
64). No knob overrides it. **Gate 5's 1 × 31 broadcast and gate 7's N = 32
join storm are not measurable on PR 13b's binary**; the 31-joiner fleet C
(`walls32`) was not attempted for the same reason. Also read on the way:
a token reader's `.stats` read answered EINVAL while its wire dial was
refused — a `.stats` read must never fail on a transient wire error.
**→ fixed in PR 13c** (the F-B3 commit): the cap derives from the RAW
affinity mask × 16 (the fleet-width exemption class —
`crate::cpu::raw_parallelism`, its consumer census pinned), floored at the
shipped 64 and ceilinged by the fd budget (`RLIMIT_NOFILE / 8`: two fds per
connection, a quarter share; the daemon raises its soft limit to the hard
one at startup); `SQUEEZEFS_CLUSTER_WIRE_MAX_CONNS` is the lever, railed
by the fd budget; `member_session_demand_from(cpus, volumes)` is the
tie-tested per-member demand (32 members × 14 = 448 ≤ 512 at share 32 on
32 CPUs — its control term COUNTED IN CODE since review round 1's Issue 3:
six standing sessions, each dial site marked and the count pinned); a dial
refused at accept retries with a doubling backoff from the accept tick,
clipped to the dial bound and then surfaces the TYPED `RefusalClass::ListenerRefused`
(EAGAIN), never the first-EOF `InvalidOperation`. The `.stats` defect: the
kernel's `default_permissions` walk GETATTRs the mount ROOT before
`/.stats`, and the root's token fetch surfaced the wire error; a token
reader whose plane answers a transient class now serves the ROOT's attrs
from its projection (`dlm_token_root_projection_serves` — R-SYM-4's one
named exception; every child resolve stays fail-closed). The class is
STRUCTURAL (the 42-suite matrix's flat leg caught the first build serving
a reader past `T_self`: the plane's law words were `Io(other)`, which
`is_transport_failure` classes as the wire's) — the plane's LAW refusals
are typed `Refused { EIO }`, a recall channel with no fresh session wears
`Io(NotConnected)` (EIO, the shipped errno; the wire's word). Pins:
`derivation_sweep_tests` (the cap rows + the fleet-demand row),
`cluster_wire_tests` (the retry lands / fails typed inside the bound),
`sym_coherence_tests::a_readers_root_attr_survives_the_holders_connection_cap_so_stats_never_fail`.
The 32-member fleet forming on the laptop's tcp devsub is PR 13c's real-mount
face; gates 5 and 7@N=32 on the box are owed on PR 13c's binary.

**Harness findings (fixed in the tree, both on the box's first pass):**
H-B1 — the netns joiner's `JoinAppender` dial to the manager's advertised
`10.181.177.194:35247` timed out (`sym-tarx` r1 of pass 1): the box has
SOURCE-BASED policy routing (`from 10.181.177.194 lookup 301`, a table
holding the fabric routes only), so the manager's SYN-ACK to the veth
subnet left by the fabric gateway; `mw_fleet.sh netns_setup` now adds
`from <src> to <veth subnet> lookup main` per such source and removes it at
teardown (`4b1e1b55`). H-B2 — the legs judge the must-stay-0 set on
ABSOLUTE gauges at their door, so F-B1's +1 on the fleet killed
`sym-shared-dir` r1 (and would have killed every later leg) at entry; the
driver's `FRESH_FLEET_PER_LEG=1` recreates the fleet before every leg
(≈ 35 s on the box — `06c9ea58`); pass 2 ran gates 2 and 3b that way. H-B3
— the devsub's default zram algorithm `zstd` is absent on the box's sqz
kernel (`lzo-rle` / `lzo`); the driver names `SQZ_DEVSUB_OSS_ALGO`.
**PR 13c's two, found by the FIRST 32-member fleet to form (the laptop,
F-B3 fixed — shapes the box never reached behind the 64-cap):** H-C1 —
`mw_fleet.sh mount_member` dispatched every index ≥ 20 as a PARTIAL
AUTHORITY whatever the fleet's shape, so `create N=32 --symmetric
--writers=1 --token-readers` died at member 20 ("partial-authority members
need an ASSIGNED set"); the dispatch reads `OWNERS_ASSIGNED` and `create`
bounds N by the co-writer slice, loud (`f1bc93d1`). H-C2 — `sym-readers`
judged `dlm_token_recall_fanout_p99 == readers`, but the p99 is
`QueueDepthHistogram::percentile`'s log-bucket UPPER BOUND (31 readers →
the `<=32` bucket → 32), a law only a power-of-two reader count could meet
— the box's N = 32 fleet has 31 readers too; the leg compares to the
count's bucket edge, the exact per-batch count already being `recalls ≡
mutations × holders` (`f483e5f5`). With both, gate 5's leg ran GREEN from
zero on the laptop's 32-member fleet (0 misses over 31 readers, 155
recalls ≡ 5 × 31, `timeouts_live` 0, recall RTT 485 µs — scoping; the
box row is owed).

**Write-amplification faces of the N-writer rows** (the daemons' ledgers
÷ user bytes; the fleet's zram namespaces carry no `/proc/diskstats`
column in the legs): gate 7 (a) `rewrite_device_write_bytes` Σ 7.00 GiB
over 6.95 GiB user (**1.0×**), no reclaim commands at the joiners (the
frees ship to the holder), `write_through_bytes` 0.14 GiB; gate 3's ingest
(`dd bs=4M conv=fsync` of 1 GiB per writer): `overlay_store_bytes`
0.81–0.92× user (the B4 ack-early DMA) + `durable_upload_bytes_escalation`
0.15–0.28× (the fsync escalation) ≈ **1.1–1.2× device writes**, and
`flush_seed_read_bytes` = `read_fill_dma_bytes` **0.15–0.28× user of
device READS** (the escalation's seed of partially covered 4 MiB blocks —
`write_path_seed_read_bytes` stayed 0), 0 reclaim commands, 0 discards,
`block_grants` 8 / 11 / 27 / 44 = `block_grant_topups` (the joiners' pull);
gate 3b's creates and gate 2's `tar -x` are metadata rows (no data bytes
of note).

#### 3.9.3 Gate 3's N = 8 term, ATTRIBUTED from the row's own `.stats` (PR 13c — no box row run): the co-located venue's per-core slowdown, not a product wall

The `sym-scale` r1 snapshots (`m*_pn{N}{0,1}.json` — per writer, before
the create storm and after the ingest) read per N:

| N | create c/s | Σ daemon CPU (s, create + ingest) | daemon cores busy | creates per daemon-CPU-s | `meta_op_phase_ns.create.total` µs (m0 / joiner) | `lock_phase_ns.leaf_lock_hold` µs | `meta_txpass_phase_ns` `pass_total` / `window_total` µs (m0) | journal `uring_fs_write_phase_ns.device` µs (m0) |
|---|---|---|---|---|---|---|---|---|
| 1 | 5,062 | 9.8 | 1.2 | 4,085 | 105 | 21.8 | 27.7 / 35.8 | 8.4 |
| 2 | 9,607 | 20.9 | 2.5 | 3,829 | 109 / 97 | 21.7 / 12.4 | 27.1 / 34.8 | 8.1 |
| 4 | 16,352 | 50.9 | 5.2 | 3,144 | 129 / 114 | 24.3 / 14.5 | 30.7 / 39.5 | 9.4 |
| 8 | 21,606 | 123.7 | 8.4 | 2,586 | 160 / 143 | 32.0 / 21.5 | 40.7 / 51.3 | 11.4 |

* **The daemons are not CPU-bound**: 8.4 of the box's 32 cores at N = 8,
  1.0–1.2 cores per daemon (`daemon_cpu_ns` ≡ the per-class Σ). The
  table's `MGR_CPU` column (1,322 %) was a HARNESS bug — the create +
  ingest CPU divided by the INGEST wall alone (9.8 CPU-s ÷ 0.74 s); fixed
  in PR 13c (the whole row's wall).
* **No product wall**: the conveyor's ρ ≈ 0.3 (`pass_total` 40.7 µs ×
  ≈ 7.5 k passes/s per daemon), `slot_door_parks` 0, `dlm_guard_wait` 11–15
  events per 60 k, `leaf_lock_wait` 0.3 µs; the `stripe_lock_wait`s
  (523–1,188 events at 1.8–4.8 ms) are the INGEST's per-ino 4 MiB merges,
  not the creates'.
* **Every RAM-only phase grew +40–58 % per op UNIFORMLY** — `leaf_lock_hold`
  21.8 → 32.0 µs, `pass_leaf_locks` 24.2 → 35.0, the create's meta op
  105 → 160 (m0) / 97 → 143 (a joiner), the journal write's device leg
  8.4 → 11.4 (nvmet-tcp on the same box) — the per-core slowdown of a
  co-located venue (8 daemons + 32 mdstorm threads on 2 × 16 cores, 2 NUMA
  nodes, all-core vs single-core turbo, a shared LLC), never one phase's
  queue.
* **≈ 600 of the ≈ 700 µs per-create latency growth is OUTSIDE the
  daemon's meta op** (790 → 1,480 µs per client thread at 4 threads;
  the meta op grew 55 µs): the FUSE transport + the kernel + the client
  threads' scheduling on shared cores. The fleet legs do not arm
  `SQUEEZEFS_OP_PROFILE=1`, so `fuse_op_phase_ns` is absent from these
  snapshots and the outside term is not decomposed further.

**Decision**: the venue term. The design's gate-3 law presumes N
independent NODES; on ONE box the writers share the cores with their
clients and the wall-clock multiple is bounded by the box. Design §8's
gate-3 row is amended ("per node; on a co-located venue the law is judged
on creates per daemon-CPU-second, or the row needs one node per writer"),
and `sym-scale` gains the `C/CPU-S` column — the create phase's creates per
daemon-CPU-second, read off a snapshot between the create and the ingest
(never the ingest's CPU) — beside the corrected `MGR_CPU`. The re-run on
PR 13c's binary reads both; per daemon-CPU-second the box's own numbers
above fall 4,085 → 2,586 (0.63×) INCLUDING the ingest's CPU — the create
phase's own reading is the re-run's.

#### 3.9.4 The re-run on PR 13c's binary (`77f4da1d`) — the box-rerun rung, `perf/sym-box-rerun`, 2026-09-22 11:02 → 13:52 UTC: **gate 1 MISS narrowed to `rename` / `unlink` and attributed (`rr4k` PAR, `mkdir` closed); gate 3 B 4.27× / 5.37× at N = 8 with the A arm (the shipped MW posture) measured at 0.07×; gate 3c LIVE law MET ×2 (F-B2 fixed), IDLE MET ×2, PAUSED NOT RUN (a harness defect since PR 13, §4.4aj — owed); gate 5 MET ×2 (F-B3 fixed); gate 7@N=32 both rows MET; F-B1 NOT fixed on the box — six increments on five writers, 0 ns excused; two new armed-plane findings (F-R3, F-R4) and one shipped-path finding (F-R2)**

**The counted-run law**: every row set below ran FROM ZERO on PR 13c's
binary; nothing from §3.9.1 / §3.9.2 is creditable. Gates 2 and 3b (MET
on `7b2ef9e9`) were NOT re-run (minimum count — they rerun once more on
PR 14's flip binary).

**Venue (re-verified 11:02 UTC before the first leg):** `squeeze-test`
(`memp-s3ds-aqs-37`), 32-core Xeon, 251 GiB, Rocky 8.10, **kernel
`6.19.14-sqz`** (the sqz series incl. patch 0031 — the per-queue bg
budget, since 2026-09-06), up 9 h 54 at the first leg, load 0.00, no
Lustre / lnet modules, docker inactive, no daemon, only `fusectl`
mounted, no `/run/squeezefs-mwfleet*` / `-devsub-*` (`/run/squeezefs/`
itself holds the box-rows rung's four stale IL sockets, `sqz-il0-*.sock`
of 01:43–02:04 UTC — not a daemon, not this rung's), no netns / veth /
`pref 40` rules, 249 G free on `/scratch`; the reset-v5 converged fabric (5 storage nodes ×
(1 meta + 2 data) memory-backed null_blk namespaces over nvme-tcp, two
paths each, the client's 15 controllers connected as PR 1 left them;
the storage nodes on `4.18.0-553.123.1.el8_lustre.ddn17` holding
`squeezefs 1.1.0 (19888503…)` for the reset script's `nvmeof`
share/unshare/list verbs — NOT refreshed, as the previous rung did not:
target-side configfs verbs only, the client binary does format + mount).
Gate 1 ran on the fabric via the reset script; the N-writer gates on the
box's own tcp devsub (option (a) of §3.9 — nvmet-tcp on `127.0.0.1`,
`resv_enable=1`, `lzo-rle` zram, `SQZ_MWFLEET_OSS_GB=16`, `--venue=box`
on every leg). **Arms:** **A** = `3228fcb8` (reused, sha256
`993100757bde…d69b1` verified on the box); **B** = `77f4da1d` — built on
the laptop from a DETACHED worktree at the gated code sha (`task
build:rocky8`, the `release` profile, 3 m 25 s, artifact checks passed):
`squeezefs 1.2.4 (77f4da1dc0fa / 77f4da1dc0fa95ce170ba53fc0708d077f0576ad)
built 2026-09-22T10:55:12Z profile release`, sha256
`42438ccde4cb12f02bc418866f46523c0f57dfefbd9d5dd56e54f0b01d0088a1`, placed
as `/scratch/tmp/sym-box/squeezefs-B-77f4da1d` and as
`/scratch/tmp/squeezefs` (the reset script's client binary; the previous
`squeezefs-B` kept for provenance). Both arms `release` (the two-profile
law). **Instrument:** PR 1's rig at its 2026-09-22 revision — **RT = 60 s
honoured** through the per-row job copy (§3.9.1b's fix: 2.0 TiB per
`wfresh` row against §3.9.1's 1.0 TiB at 30 s), the data namespaces'
`/proc/diskstats` snapshotted per row — so the fio rows are 60 s windows
and NOT window-comparable to §3.9.1's or PR 1's 30 s rows (the A arm is
in the same bracket; within-bracket comparability is what the verdict
rule needs); the reducer verbatim; `fio-3.36`, the box's standing job
files (libaio, `direct=1`, 24 jobs, `ramp_time=10`).

##### 3.9.4.1 Gate 1 — the solo re-gate, flat A vs flat B (11:06 → 11:39 UTC bracket 1, A B B A; 11:42 → 11:51 the reversed bracket, B A A B, mdstorm only): **`rr4k` PAR (0.999 — the −1.8 % residual is GONE), `mkdir` within noise (0.976 / 0.984 — the previous MISS closed), `create` / `stat` / `manydirs` / `rmdir` / `w_fresh` / mount / remount within noise; `rw4k` 0.970 (within noise by the 3 % floor, a reproducible −3.0 % at the floor); `rename` 0.960 / 0.967 and `unlink` 0.956 / 0.967 DELTA in BOTH orders — the rename lock-set fix's priced cost plus a per-op future-move term, ATTRIBUTED below**

**Verdict table (the two brackets, the PR 1 rule: within noise iff |B/A − 1| ≤ max(band, 3 %)):**

| row | bracket 1 (A B B A) B/A · band | bracket 2 (B A A B) B/A · band | positions (bracket 1 ; bracket 2) | **verdict** |
|---|---|---|---|---|
| `wfresh-kern` MiB/s (60 s) | **0.990 · 0.9 %** | — | A 34,420 / 34,265 — B 33,865 / 34,163 | **within noise** (−1.0 %) |
| `rr4k-kern` IOPS (60 s) | **0.999 · 0.8 %** | — | A 589,579 / 594,567 — B 589,787 / 592,996 | **PAR — the §3.9.1 residual (−1.7…−1.8 % in both brackets) is GONE on PR 13c's binary** |
| `rw4k-kern` IOPS (60 s) | **0.970 · 0.8 %** | — | A 539,387 / 540,132 — B 521,706 / 525,789 | **within noise (the 3 % floor) — REPRODUCIBLE −3.0 %**: every B position 521.7–525.8k, every A 539.4–540.1k; §3.9.1 read 0.962 / 0.975, so the W1 patch arm's residual STANDS at the floor (attributed below) |
| `mount` / `remount` / `umount` s | 1.132 / 1.106 / 0.706 | — | 0.41–0.59 s / 0.53–0.67 s / 7.6–13.5 s | within noise (0.4–0.7 s events, 18–43 % bands; A1's 13.5 s clean unmount after `rw4k` is the post-reboot outlier — the other three 7.6–8.7 s) |
| `mdstorm mkdir` ops/s | **0.976 · 4.1 %** | **0.984 · 4.6 %** | A 6,801 / 6,598 ; 6,831 / 6,527 — B 6,672 / 6,401 ; 6,539 / 6,599 | **within noise (−2.4 % / −1.6 %) — §3.9.1's MISS (0.942 / 0.965) CLOSED** |
| `mdstorm create` | 0.992 · 2.1 % | 0.983 · 1.5 % | A 5,881 / 5,756 ; 5,870 / 5,780 — B 5,824 / 5,718 ; 5,721 / 5,736 | within noise (−0.8 % / −1.7 %) |
| `mdstorm stat` | 0.987 · 1.1 % | 1.006 · 1.1 % | 187–194k both arms | within noise |
| `mdstorm rename` | **0.960 · 3.0 % DELTA** | **0.967 · 0.1 % DELTA** | A 4,567 / 4,432 ; 4,469 / 4,472 — B 4,339 / 4,304 ; 4,327 / 4,323 | **DELTA (B −4.0 % / −3.3 %; every B below every A) — the PR-4 rename lock-set fix's priced cost (+1 DLM guard + one lookup per rename ≈ +2.6 µs) + the `handle_setattr` future term below; §3.9.1 read 0.966 / 0.948** |
| `mdstorm unlink` | **0.956 · 1.2 % DELTA** | **0.967 · 1.6 % DELTA** | A 5,284 / 5,220 ; 5,272 / 5,188 — B 5,038 / 5,000 ; 5,075 / 5,042 | **DELTA (B −4.4 % / −3.3 %; every B below every A) — the `handle_setattr` / `handle_unlink` future-move term below; §3.9.1 read 0.965 / 0.960** |
| `mdstorm manydirs` | 0.999 · 2.6 % | 0.997 · 3.0 % | 10.7–11.3k both arms | within noise |
| `mdstorm rmdir` | 0.971 · 3.9 % | 0.985 · 4.7 % | 5.7–6.0k both arms | within noise |

**Gate 1 as the design states it ("within noise of `dev` tip on mdstorm,
rand-4k, `w_fresh`, and mount time; `dlm_rpcs == 0`") reads on PR 13c's
binary: `dlm_rpcs` 0 ✓ (all 12 mount legs + 8 mdstorm legs), mount time
✓, `w_fresh` ✓, rand-4k ✓ by the rule (`rr4k` PAR; `rw4k` −3.0 % at the
floor — reproducible, not convicted), mdstorm: `mkdir` / `create` /
`stat` / `manydirs` / `rmdir` ✓, **`rename` ✗ / `unlink` ✗ (−3.3…−4.4 %
in both orders of two brackets on 0.1–3.0 % bands).** PR 13c's three
unarmed-cost deletions CLOSED the `mkdir` miss and the `rr4k` residual
and left `create` at par; the two phases that carry the kernel's SETATTR
echo per op stay DELTA. **Verdict: MISS on `rename` / `unlink`, narrowed
from §3.9.1's three phases to two, attributed to two named terms (one
priced and kept, one new) — reported; nothing in the tree changed for it.**

**Engagement / tripwires — every leg of both arms:** `dlm_mode` `solo`,
`dlm_rpcs` 0, `mount_posture` `writer`, Δ`invariant_tripwires` (+ the
eight sibling tripwires) 0 on every row, Δ`fsck_findings` 0,
Δ`meta_kv_block_refs_drift` 0, the five `meta_kv_forest_*` gauges **0**
on every B mount (bit 17 absent — the shipped path), `features_incompat`
bit 17 = 0 on every meta volume of both arms, dmesg the five boot-time
lines only. Thermal: the hottest hwmon sensor 47–51 °C at every row
start AND end (no heat soak). The metadata economy identical arm to arm:
`Δjournal_entries` 60.0–60.3k (`wfresh`, 60 s), 66 (`rr4k`), 7.07–7.12k
(`rw4k`), 547.6–547.8k (mdstorm); checkpoints 345–350 / 92–95 / 354–355 /
70–73; node appends 9.1–9.8k / 55–58 / 428–491 / 24.4–24.8k; node splits
97 on every mdstorm leg.

**Write-amplification columns (the AGENTS instrument — `/proc/diskstats`
on the TEN data namespaces per row, ramp-inclusive: fio counts the 60 s
window, the device the 70 s incl. ramp, so ≈ 1.11–1.15× reads as 1.0×
device/user; beside it the daemon's own ledger and the reclaim family):**

| row / position | user GiB (60 s) | **device write bytes ÷ user** (`/proc/diskstats`) | `wareq-sz` | device read ÷ user | ledger `rewrite_device_write_bytes` ÷ user | `patch_write_bytes` ÷ user | reclaim `commands` / `discards` / trim bytes ÷ user | `write_through_blocks` |
|---|---|---|---|---|---|---|---|---|
| `wfresh` A1 / A4 | 2,031 / 2,011 | **1.116 / 1.116** | 1,037 KiB | ≈ 0 (1.0 / 0.9 GiB) | 1.021 / 1.021 | 0 | 61,398 / 62,200 / 0.120 ; 54,892 / 55,455 / 0.108 | 7,753 / 7,333 |
| `wfresh` B2 / B3 | 1,991 / 2,005 | **1.116 / 1.114** | 1,038 / 1,036 KiB | ≈ 0 | 1.020 / 1.019 | 0 | 39,168 / 39,575 / 0.078 ; 58,370 / 59,160 / 0.115 | 7,701 / 6,526 |
| `rw4k` A1 / A4 | 123.5 / 123.6 | **1.141 / 1.139** | 5 KiB | 0.003 / 0.004 | 0.155 / 0.147 | **0.979 / 0.985** | 0 / 0 / 0 | 4,896 / 4,652 |
| `rw4k` B2 / B3 | 119.4 / 120.4 | **1.146 / 1.134** | 5 KiB | 0.008 / 0.003 | 0.152 / 0.143 | **0.982 / 0.985** | 0 / 0 / 0 | 4,649 / 4,398 |
| `rr4k` A1 / A4 | 134.9 / 136.1 | 0 (no writes) | — | **1.172 / 1.172** (`rareq-sz` 4 KiB) | 0 | 0 | 34,033 / 27,433 (the prep's discards draining) | 0 |
| `rr4k` B2 / B3 | 135.0 / 135.7 | 0 | — | **1.170 / 1.169** | 0 | 0 | 22,604 / 24,950 | 0 |

Device write bytes ≡ user bytes on both arms and both write rows (the
ramp explains the 1.11–1.15×; `wareq-sz` = the write size — 1 MiB on
`wfresh`, the 4 KiB in-place W1 patch on `rw4k`, so no request-size
collapse), device read ≡ user on `rr4k`. **§3.9.1's flat-path delta —
B's reclaimer at 2.4–3.7× A's discard commands — did NOT reproduce:**
39–58k (B) vs 55–61k (A) commands per `wfresh` row, 0.08–0.12× user in
trim bytes on both arms.

**Attribution of the DELTA rows (the `.stats` pairs + the phase-targeted
`perf record` legs — `2026-09-22-sym-box-perf-phases.sh`, one `perf.data`
per mdstorm PHASE per arm at 999 Hz with frame-pointer chains, then one
DWARF-unwound leg per arm for `rename` / `unlink` at 299 Hz; scale 100,
`/dev/shm`, the same daemon across its phases):**

| leg | daemon µs/op (540k ops) | `fuse3-tpc` µs/op | `sqz-jrnl` / `fuse3-ur` / `sqz-meta` | conveyor `pass_total` / `window_total` µs | **`dlm_guard_hold` count** | journal entries / checkpoints / appends / splits |
|---|---|---|---|---|---|---|
| A1 / A4 / A2 / A3 | 225.5 / 226.6 / 226.2 / 230.4 | **123.5 / 125.3 / 124.0 / 127.1** | 36.2–37.2 / 33.1–33.8 / 17.6–18.2 | 38.1–39.2 / 56.9–58.6 | **889.3–889.8k** | 547.6–547.7k / 70–72 / 24.4–24.5k / 97 |
| B2 / B3 / B1 / B4 | 231.0 / 231.1 / 231.0 / 233.8 | **128.5 / 129.2 / 128.4 / 129.7** | 36.8–37.4 / 33.4–34.1 / 17.6–18.5 | 38.3–39.1 / 56.5–58.3 | **989.5–989.9k** | 547.7–547.8k / 73 / 24.6–24.8k / 97 |

* The conveyor pass, the journal lane and the meta lanes are IDENTICAL
  arm to arm; the whole daemon shift is the handler lanes: `fuse3-tpc`
  +3.2–5.0 µs per op. **B takes exactly +100,000 4a DLM guards per storm**
  (one per rename — the PR-4 round-2 rename lock-set fix, `I{moved}` +
  `I{dest}` beside the two parents and the two `D{}` keys, a shipped-bug
  fix pinned by `tests/rename_lock_set_tests.rs`; PR 13c priced it at
  ≈ +2.6 µs per `rename.backend` and KEPT it).
* **Per phase (the fp legs; `fuse3-tpc*` samples ÷ 999 Hz ÷ ops):** rename
  A 203.6 → B 208.3 µs/op (+4.7, +2.3 %), unlink 217.7 → 227.4 (+9.6,
  +4.4 %), create 196.6 → 200.3 (+3.7, +1.9 %), mkdir 203.1 → 214.8 on
  20k ops. **The ONE dominant term in both DELTA phases is
  `__memmove_avx512_unaligned_erms`: +295 samples on rename (720 → 1,015,
  ≈ +3.0 µs/op) and +531 on unlink (1,081 → 1,612, ≈ +5.3 µs/op)** —
  then `RecordIndex::group_bounds` (+98, rename), the `arc_swap` debt
  loads (+122 rename / +71 unlink — `RouteTable` loads at the routed
  entry points), `DlmLockManager::lock_stripe` (+50, create); the
  `sqz_channel::RecvFut` / `FuseReply` unbounded-receiver pair (+300 /
  +250) is a RENAME of A's `futures_util::stream::next` +
  `sqz_time::Timeout` samples (−280 / −256), not a cost; `memcmp` +177 on
  rename, −69 on unlink.
* **The DWARF legs NAME the memmove term (glibc resolves it as
  `__memcpy_avx512_unaligned_erms` there): it is the `handle_setattr`
  FUTURE's construction and move into the lane — `Box::pin` (`…E0E3new` /
  `…E0E3pin`) + the `LaneExec::run` handoff (`…E09squeezefs <-
  LaneExec::run`)** — A 178 → B 255 of the lane samples on rename (95 +
  83 → 144 + 111), A 198 → B 280 on unlink (109 + 89 → 151 + 129), plus
  the `handle_unlink` future itself 77 → 105 on unlink and `handle_lookup`
  9 → 11 / `handle_rename` 7 → 8; the leaf's share of the lane's samples
  3.35 % → 5.12 % (rename), 4.82 % → 6.63 % (unlink). **Provenance:**
  four invocations of the on-box `perf-callers2.py <perf.data> fuse3-tpc
  memcpy_avx512 4` over `perf-dwarf-{A,B}/{rename,unlink}.data` (the
  caller chain of the four frames above the leaf, samples whose comm
  starts with `fuse3-tpc`), read from the session's stdout — their
  outputs were NOT saved beside the data (review Issue 5; §8 states the
  owed `callers-*.txt` files and the rig now writes them on every DWARF
  leg through `2026-09-22-sym-box-perf-agg.py callers`). The kernel sends a
  ctime/mtime SETATTR echo after every rename and unlink (the D4
  "absorbed, not committed" echo — `meta_kv_times_echo_absorbed` 299.2–
  299.4k per storm on BOTH arms), so the SETATTR handler runs once per
  op in exactly these two phases and not in `mkdir` / `create`: **the
  per-op cost is the SIZE of the SETATTR handler's future state — the
  memmove of a larger `async fn` frame at `Box::pin` and at the lane
  handoff — grown across PRs 5–13b at the routed `setattr` entry (PR 13b's
  `record_ship` resolver, PR 9's custody words, PR 7b's stripe checks, the
  served-mutation sink) even where every arm is an `Option` probe on the
  unarmed path.** This is the term §3.9.1 called "nameable only with a
  dwarf-unwound leg"; it is named. The fix shape (a PR 14 item, never this
  rung's): shrink the `setattr` future's state on the unarmed path (box the
  armed-plane words behind one `Option<Box<…>>`, or split the unarmed fast
  path into its own smaller future) — the same economy the kernel READ
  handler took in R-5.
* **→ FIXED in PR 13f (`perf/setattr-future-economy`, 2026-09-22; pins
  `tests/meta_op_future_economy_tests.rs` + `tests/sym_read_divert_economy_tests.rs`).**
  `size_of_val` at the FUSE entry (test profile, rustc 1.98.1 —
  same-profile tripwire; the type-size dumps are test-profile too (`cargo rustc --lib --profile test`); no release-profile size was measured):
  `SqueezefsFilesystem::setattr` **18,960 B (`3228fcb8`) → 26,016 B
  (`77f4da1d`) → 896 B**; `unlink` **7,616 → 11,088 → 280 B**; `rename`
  392 B on all three (it never grew); the routed `setattr` / `getattr`
  `async_trait` boxes the echo mints per op 1,536 / 1,088 → 5,552 / 5,552
  → 1,720 / 1,192 B (rustc `-Zprint-type-sizes`, both trees). The
  attribution above is corrected by the type-size dumps: the growth is
  NOT at the routed `setattr` entry (a 16-byte box at the FUSE call site)
  — it is the TRUNCATE arm's two layout publishes (`write_file_staged`
  17,784 → 24,816, `truncate_layout` 7,224 → 10,672) and the unlink
  handler's overlay drain (`drain_device_overlays_for_ino` 7,392 →
  10,864), every publish site of which grew ≈ 3.5–4.6 KiB from ONE root:
  `KvMetaBackend::commit_tx` 176 → 4,816 B — PR 4's door
  `ensure_leases_for_tx`, whose two FIRST-TOUCH acquire arms
  (`manager_acquire_slots` 4.5 KiB, `joined_acquire_slot` 2.3 KiB) sat
  inline in every commit's future (PR 13b's `publish_target` pair and PR
  7's forest ref ops are ≈ 300–400 B of residue: `merge_layout_and_size`
  +272, `commit_block_refs` +416). The echo never takes either arm and
  moved their state twice per op. The fix boxes each arm INSIDE its
  branch (`setattr_truncate`, `drain_unlink_target_overlays`, the door's
  two acquires, the PR 13b ship behind the sync `slot_is_foreign`,
  `note_served`'s tail, `getattr_local`'s striped fold,
  `token_serve_armed` behind the sync `writer_reads_locally`): `commit_tx`
  408 B, `open` 26,224 → 2,064 B (its O_TRUNC fold runs the setattr arm),
  every publish site within ≈ 300–400 B of pre-program; no behaviour
  change. **Where the armed arms allocate now**: the door's acquires (once
  per slot per mount — a durable control write), the PR 13b ship (a wire
  round trip), `note_served`'s tail (inside a served verb), the
  striped-directory fold (once per armed directory `getattr`, beside its
  own `stripe_map` KV read), and the divert's box ONLY for a read a plane
  will serve — an armed solo writer's own-object read verb and every read
  verb of a `SQUEEZEFS_SYMMETRIC_META=0` forest take the sync exit and
  allocate nothing for it (review round 1, Issue 1: the first build boxed
  before deciding, one allocation per read verb on the flip's default
  path — 8,800 → 7,600 allocations over 400 × `getattr` + `lookup` on the
  armed writer and the `=0` forest alike; the remaining 9 per round over
  flat are PR 1's forest key framing, owed). Still carrying the term:
  `write` 26,592 → 20,320 B (the striped-write arm's own growth — the
  `rw4k` −3.0 % row's follow-on, same fix shape), `fallocate` /
  `copy_file_range` (19.1–19.2 KiB), `fsync` / `read` / `flush` (8.6–11.7
  KiB). The gate-1 bracket on the flip binary re-reads the two rows; the
  laptop scoping rows (direction only: `fuse3-tpc` C 80.3 / 83.9 vs B
  85.8 / 86.4 µs/op at scale 25, the counts identical B vs C —
  `times_echo_absorbed` 74.8k, `journal_entries` 136.9k, `dlm_guard_hold`
  246.7k per leg) are in the PR 13f note.
* `rw4k`'s reproducible −3.0 %: daemon µs/op 36.5 / 36.6 (A) → 38.1 /
  37.6 (B) — `fuse3-ur` 29.6 / 29.7 → 30.6 / 30.4 (+0.8 µs/op, §3.9.1
  read +1.2), `fuse3-tpc` 6.1 / 6.2 → 6.8 / 6.5 (+0.4); `write_transport_
  phase_ns.transport_total` 530–534 → 550–557 µs; clat p50 251–253 → 257
  µs (+2 %); the W1 arm's own `patch_write_bytes` / `write_through_blocks`
  identical. The write handler's per-op term on the unarmed path (PR 9's
  `custody_use_enter` + `cached_lease_token`, the served-mutation / recall
  sinks, `refuse_foreign_slot_open` at the write-intent open) — priced
  here at ≈ +1.2 µs of daemon CPU per 4 KiB write; the same fix shape.
* `rr4k`: daemon µs/op 28.1 → 28.6 (+0.5: `fuse3-tpc` 11.5 → 11.8,
  `fuse3-ur` 16.5 → 16.7), IOPS PAR — the box has the CPU headroom (74 %
  busy) and the zc bridge's hops are unchanged (`msg_hop` 24.4 → 25.0,
  `wake_hop` 26.8 → 28.1, `device_cq` 96 both).

##### 3.9.4.2 Gate 3 — `sym-scale` (B, the armed plane) vs `mw-scale` (A, the SHIPPED authority + co-writers on the same binary) (12:24 → 13:06 UTC): **B N = 8 4.27× creates / 5.37× ingest on the wall (MISS vs ≥ 5.6×, exactly §3.9.2's read), 0.61× per daemon-CPU-second; A bounded at 351–391 creates/s at EVERY N (0.07×) by the authority's verb service — and the F-B1 tripwire TRIPPED THREE TIMES on the B fleet with PR 13c's exclusion excusing NOTHING; the B row set stopped at r1**

**The A arm exists on the box for the first time** — the box-rows rung's
Finding 3 was the harness gap; this rung wrote `run_mw_matrix.sh
mw-scale` (the SAME storm on `mw_fleet.sh create N=1 --cowriters=7`: the
S9 authority + N − 1 co-writers each in its own directory, the same
`C/CPU-S` column, the engagement law the S8/S9 ledgers' closure — Σ
`shipped_verbs` + the S10 create-intent lane's `meta_ship_intent_verbs`
≡ the authority's `served_verbs` (H-R1: the first launch died on
131,156 shipped vs 139,347 served, the +8,192 the intents lane the owner
counts and the client's `shipped_verbs` does not), publish ships ≡
served, refusals 0, `local_commit_refusals` flat, `mount_posture`
co-writer) and fleet M / the `mwscale` gate in the driver. Order: **A1**
(fleet M) → **B2** (fleet A, fresh); the B row set stopped on the
tripwire at r1, and a second A position was NOT run (at a 60×
separation it decides nothing — the minimum-count law).

| N | **B (armed): create/s · ×N=1** | B `C/CPU-S` (create phase) | B ingest MiB/s · × | B `MGR_LOAD` / `MGR_CPU` / handovers / ships / rpcs | **A (shipped MW): create/s · ×** | A `C/CPU-S` | A ingest MiB/s · × | A `MGR_CPU` / shipped + intents ≡ served / publish ships |
|---|---|---|---|---|---|---|---|---|
| 1 | **5,029 · 1.00×** | 4,281 | 1,284 · 1.00× | 3 % / 114 % / 0 / 0 / 0 | **5,288 · 1.00×** | 4,287 | 1,112 · 1.00× | 116 % / — / 0 |
| 2 | **9,446 · 1.88×** | 3,944 (0.92×) | 2,719 · 2.12× | 2 % / 120 % / 0 / 1 / 0 | **351 · 0.07×** | 332 | 2,347 · 2.11× | 101 % / 131,396 + 8,192 ≡ 139,587 / 1,657 |
| 4 | **16,348 · 3.25×** | 3,314 (0.77×) | 4,251 · 3.31× | 1 % / 123 % / 0 / 3 / 0 | **343 · 0.06×** | 234 | 3,910 · 3.52× | 138 % / 391,144 + 24,576 ≡ 415,720 / 4,350 |
| 8 | **21,486 · 4.27×** | **2,627 (0.61×)** | **6,898 · 5.37×** | 0 % / 111 % / 0 / 6 / 0 | **391 · 0.07×** | 206 | **6,715 · 6.04×** | 176 % / 902,335 + 57,856 ≡ 960,191 / 8,987 |

**Verdicts.** *B, the wall law (≥ 0.7 × N × the N = 1 rate on BOTH
rows):* N = 2 ✓✓, N = 4 ✓✓, **N = 8 ✗ on creates (4.27× vs ≥ 5.6×), ✓
on ingest (5.37× — the previous read 5.33×)** — the number §3.9.2 read
on PR 13b, reproduced to 0.01× on PR 13c: the co-located venue's term
(§3.9.3) is unchanged by the fixes, as it should be. *B, the amended
row's per-daemon-CPU-second face (design §8 gate 3, PR 13c):* 4,281 →
3,944 → 3,314 → 2,627 creates per daemon-CPU-s = **0.92× / 0.77× /
0.61×** — the create phase ALONE reads what §3.9.3 read with the ingest
folded in (0.63×), so the per-core slowdown is the create phase's own:
every RAM-only phase grows again uniformly (`leaf_lock_hold` 20.5 →
24.3 µs on the manager, 12.5 → 15–18 on a joiner; `pass_total` 26 → 30 /
20 → 22–29), `slot_door_parks` 0, per-writer create walls 13.5–14.9 s at
N = 8 (2,687–2,973 c/s each vs 5,029 alone), the manager's `MGR_CPU`
111–123 % (its own storm). **The per-NODE law stays UNMEASURED on any
venue (PR 15's cloud row).** *A, the shipped posture:* **bounded by the
authority at every N — 351 / 343 / 391 creates/s aggregate (0.07×)**:
the authority's own storm finishes at 4,020–5,240 c/s while every
co-writer's 40,000 creates take 228 s (N = 2: 175 c/s), 466 s (N = 4)
and 810–818 s (N = 8: **49 c/s each**); ingest scales (6.04× at N = 8 —
the data DMA is the co-writer's own under its custody lease). This is
the design's "vs today's authority + co-writers" comparison, MEASURED:
**at N = 8 the armed plane creates 55× faster than the shipped posture
on the same binary and the same box.**

**F-R2 — PRODUCT FINDING on the SHIPPED S8/S10 path (the A arm; reported,
not fixed here):** the authority burns **1.66 ms of `sqz-meta` CPU per
served create** — the PER-ROW deltas (`pn{n}0 → pn{n}c`, `sum_ns ÷
count`): `meta_ship_owner_dispatch_ns.run` **1,658 µs** against
`meta_ship_owner_phase_ns.execute` **18 µs** at N = 2 (n = 130,926 /
131,317); **1,817 / 237 µs at N = 4** (n = 389,299 / 390,997); **2,429 /
869 µs at N = 8** (n = 897,427 / 902,086) — the served `execute` term
itself GROWS 18 → 237 → 869 µs with N (the owner's dispatch contention
under 3 / 7 concurrent lanes) while `run − execute` (the readdir below)
stays 1.5–1.6 ms; `daemon_cpu_ns_by_class.sqz-meta` **217 of the
authority's 231 CPU-s** over the N = 2 create phase (621 of 645 at N =
4, **1,392 of 1,443** at N = 8), the co-writer's `meta_ship_phase_ns.rtt`
1.7 ms per verb with `queue_wait` 235 µs at N = 2. The site: `MetaShipService::
issue_update_grant` (`src/meta_ship/service.rs:1216`) — for EVERY shipped
`CreateWithRdev` the owner reads the parent's census with
`readdir_local(dir, 0, census_max + 1)` BEFORE the over-budget decline,
and `intent_census_max()` = `CONTROL_MAX_FRAME_BYTES / 2 / 64` = 8,192,
so once a co-writer's directory passes 8,192 entries every further
create pays an 8,193-entry readdir at the owner and is then DECLINED
(`meta_ship_intent_update_declines` 31,784 / 95,335 / 221,931 = every
create past the first 8,192 + 24 per co-writer; `update_grants` 25–26;
`meta_ship_intent_verbs` 8,192 per co-writer — the one supply chunk that
was granted). The remedy shape (PR 14 / the S10 owner): decide the
over-budget class from a cached per-directory census word (the
delegation table already keys the directory) instead of a bounded
readdir per create, or check `supply_remaining` / the holder's own
census before the read. Not a symmetric-plane finding — the armed plane
never runs this path (its creates are local mints in the creator's own
slot tree) — but the shipped co-writer posture the flip retires is what
every customer runs today.

**F-B1 on the box, on PR 13c's binary — the VERDICT:**
`appender_flush_ceiling_overruns` **moved FOUR increments on THREE
writers of the 8-writer fleet inside the 8-minute leg — m0 (the manager)
[1, 1] (one per metadata volume, BETWEEN the rows: `[0, 0]` at `pn41` →
`[1, 1]` at `pn80`, the window of the joins of m63..m66 for N = 8 — m61 / m62 were already mounted for N = 4), m61
[0, 1] (between the rows too: `[0, 0]` at `pn4c` → `[0, 1]` at `pn80`),
m63 [0, 1] during N = 8's INGEST (`[0, 0]` at `pn8c` → `[0, 1]` at
`pn81`; `dd bs=4M conv=fsync`, 6.9 GB/s aggregate into the zram) — and
PR 13c's exclusion excused NOTHING**:
`appender_flush_ceiling_excused_ns` 0, `…_excused_max_ms` 0,
`…_service_extensions` 0, `…_recovery_extensions` 0 on every one of the
eight writers, `…_service_cap_ms` 1,100 and `appender_recovery_bound_ms`
1,207 published, `appender_recoveries` 0. So the overruns the box reads
are NOT another actor's structural hold of the SMO mutex (the class the
exclusion closes) — they are the pass's own wall or the tick's lateness
under the co-located load, i.e. **exactly what §7 item 3's MARGIN
derivation must price, and F-B1's fix leaves the box's reading where it
was: the tripwire is a VERDICT on the box, and it trips.** The audit's
WARN lines (age / excused / cap) for these four increments were lost
with the fleet's teardown (H-R2 — the driver now keeps every member's
daemon log per leg), so their AGES are unknown; the `.stats` snapshots
above are the evidence (the increments, the windows, `excused_ns` 0). Per the counted-run law the B
row set stopped at r1 (no second position); the numbers above stand as
the row's. Deleted-stays-deleted and the fsck oracle were NOT reached
(the leg dies on the tripwire before them, as §3.9.2's did). Ingest
amplification at N = 8 (the daemons' LEDGERS ÷ 8 GiB user — not the
`/proc/diskstats` face, which the N-writer legs do not snapshot, and no
`wareq-sz`; §3.9.4.5 states the owed harness item): `overlay_store_
bytes` 0.92× + `durable_upload_bytes_escalation` 0.16× + `write_through_
bytes` 0.02× ≈ **1.10× submitted writes**, `flush_seed_read_bytes` 0.17×
user of submitted READS (`write_path_seed_read_bytes` 0), `block_grant_
topups` 46, 0 reclaim commands.

##### 3.9.4.3 Gate 3c — `sym-foreign-touch`, two positions on fresh fleets (13:07 → 13:25 UTC): **the LIVE law MET as a VERDICT both positions (192 ships / 0 handovers — F-B2's fix holds on the box); IDLE moved after 4 / 5 bursts in 7.5 / 6.7 ms; the PAUSED law NOT RUN — its "live job" never ran (a harness defect since PR 13's `8b7cc418`, §4.4aj), so both positions' PAUSED cells are INVALID and the law is owed to the next box session; position 2 RED at its oracle on F-B1 (1,116 ms); and TWO product findings on the cross-owner path (F-R3, F-R4)**

| position | LIVE (3 bursts × 64 into the live holder's tree) | IDLE (bursts of 64 at the 10 s beat until the handover) | `slot_handover_phase_ns` (the departing holder) | PAUSED (3 single touches over 3 × 5 s inside the half-window) | own create rate (A) | oracle |
|---|---|---|---|---|---|---|
| r1 (13:07) | **192 ships, 0 handovers** ✓ | handed over after **4 bursts** (50.3 s; 238 ships, 2 offers), 0.020 handovers/s | **7.48 ms** = flush 3.99 / tree 0 3.36 / page 0.13 / grant 0 | **NOT RUN / INVALID** — the phase's storm died at its first `mkdir` (`paused-c.txt`: `mdstorm: mkdir failed on …/job-w0/paused/d1`), the holder's journal moved 15,873 → 15,882 over the phase (the three touches' served steps, no job); the recorded outcome (handovers 0 with `slot_offers_idle` +1) is the IDLE arm's offer not yet completed, not the paused-job law | 5,047 c/s | clean (fsck 0, C8 0, must-stay-0 flat) |
| r2 (13:16) | **192 ships, 0 handovers** ✓ | handed over after **5 bursts** (63.7 s; 320 ships, 1 offer), 0.016 handovers/s | **6.67 ms** = flush 2.04 / tree 0 4.49 / page 0.13 | **NOT RUN / INVALID** — the same (`…/paused/d3`; journal 15,940 → 15,947); the recorded "moved by the IDLE arm" was the IDLE arm moving a slot NOBODY wrote — there was no job whose children could spill, so the §7-item-10 attribution written here before this review is STRUCK | 5,342 c/s | **RED — F-B1: m60 `appender_flush_ceiling_overruns` 1** |

`N_floor(A)` seeded 2 on the box both positions (the value under which
§3.9.2 read the LIVE handover), beat 10 s. **F-B2 — the box's verdict on
the fix: MET** — 384 touches into a LIVE holder's tree across two fleets,
0 handovers (63/64 → 64/64 evaluations BUSY); the subtree law credited
the storm under `job-w60/live/r*` to `job-w60`'s slot as designed.

**The PAUSED law (design §8 row 3c's third law — "a PAUSED live job's
tree stays", engagement `a_paused_live_job_keeps_its_tree`) has never
been exercised by this leg, on any venue** (§4.4aj): `run_mw_matrix.sh`
launched the paused job's storm into `job-w<C>/paused` without creating
it, mdstorm's `mkdir` phase never creates its own root, the first
`mkdir` failed `ENOENT`, every worker stopped, and the storm was
backgrounded and waited with `|| true` — so the touched slot read IDLE
(`slot_offers_idle` +1 in EVERY position, both on the box and in PR 13's
§3.4 laptop rows) and with `ops_h = 0` a DOMINATED offer is LEGAL by the
rule (`ops_q ≥ 2 × 0 ∧ ops_q ≥ N_floor`); the idle arm merely fired
first. "Dominated offers 0" therefore tested nothing. The harness is
fixed on this branch (the root is created, the job must be a STOPPED
live process at the pause and COMPLETE its 40,000 mkdirs after the
resume, the holder's journal must move by them — a storm that dies is
an idle holder and the phase dies loud), and **the fixed phase RUNS on
the laptop** ("it works" — 2026-09-22 11:03 local, `7.2.6-cachyos-lto`,
`create N=2 --symmetric --writers=3 --lease-ttl-ms=15000`, the binary
`fe960985` = `77f4da1d`'s source): `mkdir ops=40000 wall_s=9.639
ops_s=4150`, the holder's journal **+60,129** entries over the phase,
and — with a live job for the first time — **handovers 0, `slot_offers_
idle` 0, `slot_offers_dominated` 0** (LIVE 192 ships / 0 handovers and
IDLE after 3 bursts / 5.5 ms beside it, oracle clean, torn down to zero
residue). The PAUSED law's BOX row is owed to the next box session (PR
13e's binary, where 3c re-runs anyway for F-R3 / F-R4) — not run now
(the minimum-count law). Gate 3c on this binary therefore reads: **LIVE
law MET ×2, IDLE MET ×2 (moved in 4–5 bursts), PAUSED NOT RUN.**

**F-B1 (position 2's oracle)** — m60's kept daemon log at 13:19:26:
`flush ceiling OVERRUN — appender region(s) [(1, 1116)] (id, oldest
dirty leaf's age in ms at the covering barrier) exceeded the 1100 ms
landing ceiling with every structural hold's capped overlap excluded` —
16 ms past the ceiling with NOTHING excluded (the same class §3.9.4.2
read as four increments on the scale fleet). **It fell in the QUIET
window, not under a storm**: m60's `plive1`, `pidle1` AND `ppaused1`
snapshots all read `[0, 0]` — the LIVE storm ended ≈ 13:17:30, the IDLE
handover (slot 131 released) landed at 13:18:35, the manager's
PAUSED-phase idle move at 13:18:47, and the trip is 13:19:26, after the
last phase snapshot and before the 13:20:42 `rm -rf`; m60's own job was
two minutes finished and the "paused job" never ran (§4.4aj) — the one
thing in its log near the trip is a wire refill nine seconds earlier
(`dropped 1 stale projection node(s) inside a fresh extent grant`). A
covering barrier landing late on a joiner with almost nothing dirty is
a DIFFERENT input to §7 item 3's margin derivation than a storm's pass
wall — the tick's / barrier's own lateness — and is stated as such in
§3.9.4.6. The leg's
`sym_zero_set` at the oracle judges the ABSOLUTE gauge and RED'd the
position (the LIVE / IDLE laws had already printed their verdicts; the
PAUSED cell is INVALID — above). The row set's two positions stand.

**F-R3 — PRODUCT FINDING (the ARMED plane — PR 6's cross-owner unlink ×
PR 12b's N daemons; both positions, named by the kept daemon logs the
driver keeps since H-R2):** *every* cross-owner UNLINK of a child ANOTHER
appender minted judges the child "no inode record". m60's end-of-leg
`rm -rf /mnt/…/m60/job-w60` logged **430 (r1) / 512 (r2)** lines of
`cross-volume unlink of "touch-live-r…" / "touch-idle-r…" in parent 132:
child ino 53086xx has no inode record — removing the dangling name and
accounting nothing (run squeezefs fsck)` — exactly the 192 + 238 / 192 +
320 touches m61 CREATED into m60's directory (m61's children live in
m61's rotor slot — PR 6 mints in the creator's rotor); m0 logged 3 (the
paused-phase touches into its `job-w0`); m61 logged 0. The plan builder
`RoutedMetaBackend` (`src/meta_backend/mod.rs:5499`) reads the child's
`(pre, post)` nlink witness with `read_inode_value_routed(local_child)`
— `KvMetaBackend::read_inode_value`, a LOCAL KV read — and on a slot
another appender LEASES that is this daemon's PROJECTION of the lessee's
tree, which by KD-SYM-3 never sees the leased root (it rides the
lessee's page) — so the witness reads `None`, the `SetNlink` step is
DROPPED, the `RemoveDentry` step ships and lands, and the child stays
`nlink 1` with zero names at its creator: **one orphaned inode per
cross-owner unlink of a foreign-minted child — 430 / 512 leaked inode
records (+ their slot-tree bytes) in this leg alone**, and `rm -rf`
reports success. PR 6's stated deviation (3) — "the plan's foreign READS
are exact in one process and the S5 projection on the wire until PR 5's
tokens" — was never closed for the unlink witness on the N-daemon fleet
(PR 12b's writer read divert covers `token_reader_for`'s five verbs, not
`read_inode_value_routed`). **fsck cannot see it while the lessee lives**:
the inode plane scopes out live foreign lessees' slots (PR 12b round 1,
`fsck_inode_plane_foreign_dentry_scoped`), so both positions' oracles
read `findings: 0` over hundreds of orphans; the manager's `stat` of a
removed name through m60 would read `ENOENT` (the name IS gone) — the
loss is the creator's inode + its blocks, invisible until the lessee
leaves and the next census walks its tree. Fix shape (PR 14 — a rung
item, reported not fixed): the witness read through the writer's read
divert (`writer_read_plane_for` — the holder's token plane, one grant)
or a `LookupExact`-class verb at the child's holder; the unlink path's
"dangling name" arm should refuse to drop the count step for a child
whose slot another LIVE appender leases. Pin shape: the two-backend
fixture — a joiner creates into the manager's directory, the manager
unlinks, the child's record at the joiner must read `nlink 0` (or be
destroyed), fsck's C9 over the joiner's tree after its leave 0.

**F-R4 — PRODUCT FINDING (r1; PR 6 × PR 4's handover — defects 29 / 30's
family):** create #46 of the IDLE burst that TRIGGERED the handover
answered **`ENOENT` to the application** — `create touch-idle-r4-46:
[Errno 2] No such file or directory: /mnt/…/m61/job-w60/touch-idle-r4-
0000046` at 13:08:27, the instant m60 logged `slot 131 released by
appender 1 (g 1, root 0x3400000, cursor 44, 3 extents) — flush 3987 µs,
page 134 µs, tree 0 3356 µs` and m61 logged five `the manager answered
NotHolder { 0 } for object 144036023238658 — this joiner's lease
projection lagged the manager's checkpoint; retried at the holder the
manager named`. **The mechanism below is a WORKING HYPOTHESIS from that
timing correlation — no log line names the refusing site** (the
requester's daemon logs the redirects, not the errno's origin): the
requester's create into the moving directory was answered by the OLD
holder's live-witness refusal after its release (`ENOENT` — the parent
no longer in m60's leased set) and surfaced as the op's errno instead of
re-dispatching (the `SlotBusy` / stale-holder classes defects 29 / 30 /
35 made retryable; an `ENOENT` witness for a directory that EXISTS at
its new holder would be the same class wearing the wrong word). The
alternatives the evidence does not exclude: the new holder's (m61's)
first local lookup of the parent right after `adopt_transferred_slot_
tree`, or the initiator's own re-dispatch landing on a stale parent
read. **The pin that settles it** (the fix's, PR 13e / 14): the
two-backend fixture with a foreign create held at the served step
(`TEST_XV_SEAM_AFTER_STEPS` / a hold before the holder's witness read)
while the slot is released TO the creator, then the served step
resumed — the create must LAND (or re-dispatch and land), never answer
`ENOENT` for a directory that exists at its new holder; the seam names
the site the errno came from. Once in 558 touches across the two
positions (the 46 creates before it in that burst succeeded — and are
F-R3's orphans).

##### 3.9.4.4 Gate 5 — `sym-readers`, the 1 × 31 broadcast on the 32-member fleet, two positions on fresh fleets (13:26 → 13:33 UTC): **MET on every law, both positions — F-B3's fix is a VERDICT on the box: the fleet that could not form on PR 13b FORMS (the manager's listener at `max 512 connections`, `RLIMIT_NOFILE soft raised 1024 → 262144`), exactness 0 misses over 31 readers, `dlm_token_recalls` 155 ≡ 5 × 31, recall RTT 300 µs, `free_grace_hold_ms` 0 under tokens**

| position | fleet | exactness (create / rename / setattr at the NEXT resolve, 31 readers) | broadcast (5 publishes × 31 holders) | recall RTT (the writer's `dlm_token_recall_rtt_ns`, exact-sum) | token grant RTT (Σ 31 readers — the EXACTNESS window `pex0 → pbc0` / SINCE MOUNT at `pfg1`, the fleet-formation probes included) | free-grace | oracle |
|---|---|---|---|---|---|---|---|
| r1 (13:28) | 1 manager + 1 joined writer (m60) + 31 `-o ro` token readers, `FLEET_SHARE=32`; the manager's listener `max 512 connections` | **0 misses** | **`dlm_token_recalls` 155 ≡ 155**, acks 155, readers received 155 / acked 155, `fanout_p99` 32 (= the 31-reader bucket edge, `fanout_p50` 32), **`timeouts_live` 0** | mean **299.6 µs** = send 157 / drain 90 / ack 53; p50 ≤ 512 µs, p99 ≤ 512 µs (n = 5 batches) | exactness window: 31 grants, mean 99.2 µs, p50 ≤ 128 µs, **p99 ≤ 512 µs**; since mount: **155 grants** (5 per reader — the arm probe, the root and the leg's fetches), mean **229 µs**, p50 ≤ 128 µs, **p99 ≤ 1,024 µs** (5 grants in the ≤ 1,024 µs bucket — the fleet-formation probes while 31 readers dialed at once); the log-bucket edges | `free_grace_recall_gated_frees` 5 (the row) / 8 (the leg) — every displaced block published DIRECTLY; `free_grace_hold_ms` **0**, deferrals ≡ releases + offsets = 0; the S5 composite was 2,724 ms | clean (fsck 0, C8 0, must-stay-0 flat on both writers) |
| r2 (13:31) | fresh fleet, same shape | **0 misses** | **155 ≡ 155**, acks 155, `fanout_p99` 32, `timeouts_live` 0 | mean **300.3 µs** | exactness window: 31 grants, mean 98.8 µs, p99 ≤ 512 µs; since mount: 155 grants, mean 235 µs, p99 ≤ 1,024 µs | `recall_gated_frees` 5, hold 0 | clean |

`reader_staleness_bound_ms` **0** on all 31 readers both positions
(R-SYM-4), `dlm_token_reader_holder_planes` 1 per volume per reader,
each reader's `.stats` readable (the F-B3 `.stats` EINVAL closed), 95 /
94 s per position incl. the exactness + broadcast + free-grace legs, the
32-member fleet formed in ≈ 3 min each time. **Gate 5 as the design
states it reads MET on the box.** The design's "token grant p99 and
recall-ack p99 at N = 32 members" are the bucket edges above (the
histograms are log-bucketed by design; the exact means beside them);
**the verdict reads the SINCE-MOUNT population for the grant p99 (155
grants, ≤ 1,024 µs — "at N = 32 members" includes the fleet's formation,
which is where the five slowest grants fell) and the exactness window
(31 grants, ≤ 512 µs) as the row's steady-state face**; the recall-ack
p99 is the writer's `ack` phase ≤ 128 µs (n = 5 batches). Both
populations are stated because the first write of this section labelled
the exactness window "the leg" (review Issue 7).

##### 3.9.4.5 Gate 7 — `sym-walls` at the design's N = 32 (fleet C: the manager + 31 joined writers; `--walls-files=4`), two launches (13:34 → 13:41 and 13:45 → 13:52 UTC): **row (a) the relocated FREE wall MET on its law both times (1,299 / 1,002 frees/s at the holder, `shipped 1,984 ≡ served 1,984 ≡ displaced`, the daemons' ledger `rewrite_device_write_bytes ÷ rewrite_user_bytes` 1.000× — the `/proc/diskstats` face and `wareq-sz` OWED on the N-writer legs); row (b) the JOIN STORM MET — 32 mounts in 3.66 / 3.68 s (the design's 10 s), 250 / 269 manager verbs, `manager_service_ns` 14.6 / 12.3 s; the second launch's row (a) tripped F-B1 on m65 (1,206 ms, nothing excused) — the row set stopped at r1**

| launch | row (a): 31 joiners × 4 × 64 MiB pre-written then REWRITTEN in place at once | row (b): 31 joiners leave, then all rejoin at once + `mkdir /jobs/<j>` each | oracle |
|---|---|---|---|
| 1 (13:34, `nw3/walls32-r1`) | displaced **1,984 ≡ shipped 1,984 ≡ served 1,984**, minted 0 (the grant windows covered the rewrite), **1,299 frees/s**, rewrite wall **1.53 s** (7.75 GiB → 5.1 GB/s into the zram), `MGR_CPU` 14 %, 3 manager verbs (`SVC_TOTAL_NS` 42,151 = **42 µs**), `manager_load_pct` 0, `free_ship_failures` 0, `free_refused_blocks` 0, the holder's bitmap `clear_bits` 1,984, 0 reclaim commands at the joiners, **the daemons' ledger Σ `rewrite_device_write_bytes` 8,321,499,136 B ≡ Σ `rewrite_user_bytes` (1.000× — what the 31 daemons SUBMITTED; NOT the `/proc/diskstats` device face, which the N-writer legs do not snapshot)**, `write_through_bytes` 0 — **MET** | join wall **3.66 s** to the 32nd Live page, **250 manager verbs, `manager_service_ns` Σ 14.63 s** (execute 14.63 s — 58 ms per verb under 31 concurrent joins: `extent_grants` 62, `slot_grants` 3,968 = 31 × 128 rotor slots), `MGR_CPU` 169 %, `manager_load_pct` [4, 3], `manager_failover_bound_ms` 45,033, `appenders_known` 32, `membership_members` 31; the `/jobs` ships: **28 served at the manager, `dir_stripe_flips` +1 — `/jobs` STRIPED mid-storm** (`directory 5 STRIPED into 64 stripes (4 supplied by creators [1, 7, 17, 31], 60 minted by the holder)`; `/` had striped during row (a)'s 31 `mkdir /walls-*`), so the harness law `steps_served ≥ n` AT THE MANAGER read RED — **H-R3**, the law made fleet-wide | clean (fsck 0, C8 0, the must-stay-0 set flat on all 32 writers — **no F-B1 trip on this launch**) |
| 2 (13:45, `nw4/walls32-r1`, the fixed law, from zero) | 1,984 ≡ 1,984 ≡ 1,984, minted 332, **1,002 frees/s**, rewrite **1.98 s**, `MGR_CPU` 13 %, 29 verbs (15.4 ms), the ledger **1.000×** (submitted bytes ≡ user bytes), failures 0, refused 0 — **MET on its law; F-B1: m65 `appender_flush_ceiling_overruns` +1 during the rewrite** (`region (6, 1206)` — 106 ms past the ceiling "with every structural hold's capped overlap excluded", 13:47:55; `excused_ns` 0 on all 32 writers) | join wall **3.68 s**, **269 verbs, Σ 12.32 s** service (execute 12.32 s), `MGR_CPU` 210 %, load [3, 2]; **`JOBS_SHIPPED` 31 ≡ `served_fleet` 31 (28 at the manager + 3 at the stripe holders), `JOBS_LOCAL` 0, flips during the storm 1 — MET** | clean (fsck 0, C8 0) |

**The AGENTS amplification instrument is NOT on these rows.** Gate 1's
fio rows carry it (`/proc/diskstats` on the ten fabric data namespaces,
`wareq-sz`, the reclaim family — §3.9.4.1); the N-writer legs (gates 3 /
7) snapshot the daemons' `.stats` alone, so their write columns are the
daemons' own LEDGERS — `rewrite_device_write_bytes ÷ rewrite_user_bytes`
here (what the daemons submitted), `overlay_store_bytes + durable_
upload_bytes_escalation + write_through_bytes` on gate 3's ingest —
never the device's byte count, and no `wareq-sz` exists for them. Owed
(a harness item for the driver, the next box session): snapshot the
devsub's data namespaces' `/proc/diskstats` per leg and print the
device ÷ user and `wareq-sz` columns beside the ledgers.

The design's row (b) names "WERO registers" in the storm: on this
co-located fleet the 31 joiners ADOPT the manager's holds (KD-SYM-22,
`join_wero_as_appender` CO-LOCATED), so `pr_registrants_per_namespace`
stays 1 on each of the four namespaces — no device registration rides
the storm on a one-box fleet; the wire's own cost is the 250–269 manager
verbs above. **Gate 7 as the design states it reads MET on both rows at
N = 32 on the box**, with the tripwire's verdict beside it: F-B1 tripped
on one of the two launches' rewrite rows (a joiner at 1,206 ms). The
first launch's row (b) number stands although its verdict word read RED
on the pre-fix law (the law was the harness's, the mechanism the
design's own striping arm engaging on 31 creators into one directory —
the 3b shape, unplanned in the join storm).

##### 3.9.4.6 The three fixes, judged on the box (F-B1 / F-B2 / F-B3)

| finding | PR 13c's fix | **the box's verdict on PR 13c's binary** |
|---|---|---|
| **F-B3** — the cluster-wire cap derived to 64 under `FLEET_SHARE=32`; the 32-member fleet never formed; a token reader's `.stats` EINVAL | the cap from the RAW root × 16 ceilinged by the fd budget, the startup `RLIMIT_NOFILE` raise (listener caps alone), a refused dial retries then `ListenerRefused`, the reader's root attr off its projection under a transient wire class | **FIXED — a VERDICT**: fleet B (1 manager + 1 writer + 31 token readers) formed twice in ≈ 3 min each (`max 512 connections`, `RLIMIT_NOFILE soft raised 1024 → 262144`), fleet C (manager + 31 joined writers) twice; every reader's `.stats` readable; gate 5 MET ×2, gate 7's N = 32 storm MET ×2 (§3.9.4.4 / §3.9.4.5). 0 accept refusals in any log. |
| **F-B2** — a LIVE holder recalled once by a 64-touch burst (`OfferDominated`, `N_floor` 2) | `ops_h` counts the holder's work on the slot's SUBTREE across volumes | **FIXED — a VERDICT**: 384 touches into a LIVE holder's tree over two fresh fleets at `N_floor` 2, **0 handovers** (§3.9.4.3); the IDLE arm still moves an idle tree in 4–5 bursts (6.7–7.5 ms). The PAUSED law was NOT exercised on the box (§4.4aj — its job never ran); it runs on the laptop with the fixed harness and is owed to the next box session. |
| **F-B1** — `appender_flush_ceiling_overruns` tripped 4× in 12 min on PR 13b (1–32 ms past the 1,100 ms ceiling, no recovery in flight) | the audit EXCLUDES the SMO mutex's structural holds (an overlap-bounded exclusion, capped and published); **the MARGIN's derivation stays §7 item 3, PR 14's** | **NOT FIXED on the box — the tripwire still trips, and the exclusion excused NOTHING**: on PR 13c's binary the gauge moved **six increments on five writers in 45 minutes of fleet time**, each placed by its snapshots — the 8-writer scale fleet **four** (m0 [1, 1] between the rows at the N = 8 joins, m61 [0, 1] between the rows, m63 [0, 1] in N = 8's ingest — §3.9.4.2), the 8-writer touch fleet **one** (m60 at **1,116 ms**, in the QUIET window after the phases — §3.9.4.3), the 32-writer walls fleet **one** (m65 at **1,206 ms**, in the rewrite — above). **Two of the six have a WARN line** (m60 / m65 — the daemon logs kept since H-R2; both read `… exceeded the 1100 ms landing ceiling with every structural hold's capped overlap excluded`), so the AGE range 16 / 106 ms past the ceiling is theirs alone; **the scale fleet's four have NO age reading** (their logs died with the teardown). What every one of the six HAS is the `.stats` reading `appender_flush_ceiling_excused_ns` 0, `…_service_extensions` 0, `…_recovery_extensions` 0, `…_excused_max_ms` 0 on every writer of every fleet, `…_service_cap_ms` 1,100 published — so the conclusion holds for all six: **what the exclusion did NOT explain is EVERYTHING the box reads**, the class is not another actor's hold of the SMO mutex. **One of the six (m60) is established as NOT under a storm** — its snapshots and log place it in the quiet window after the phases; the other five are under load: m63's ingest, m65's rewrite, and the three "between the rows" trips (m0 ×2, m61) fall in `sym-scale`'s per-N teardown, where every writer `rm -rf`s its 20,000-file tree (`run_mw_matrix.sh`, the `scale-*-n$n-w$idx` removal — a 20k-unlink storm per writer) before the next row's joins, the joins themselves carrying the manager's grant / checkpoint work — so those three are under their own unlink storms + the m63..m66 joins, not quiet; the attribution "the pass's own wall OR the tick's / covering barrier's lateness" is by ELIMINATION, because **no pass-wall / checkpoint-cycle histogram exists in the stats** (no `checkpoint_phase_ns`-class key — the age at the covering barrier is the only word the audit records) — **that instrument is the first thing PR 13e's / §7 item 3's margin derivation must add: a margin "derived from the measured pass wall" needs the pass wall measured, per cycle, on every writer.** Until the derivation lands, every N-writer row set on the box stops at its first trip, as the counted-run law requires, and the design's gates 3 / 3c / 7 cannot read MET as row sets whatever their rates say. **The gauge's verdict on the flip: NOT YET.** |

#### 3.9.5 The third pass — PR 13e / 13f's binary (`b377cbb8`) — `perf/sym-box-13e`, 2026-09-23 01:53 → 03:57 UTC (the box left as found at 04:02): **gate 1 MET as the design states it — `rename` PAR (1.012 / 0.998) and `unlink` B AHEAD (1.054 / 1.042, both orders; the setattr / unlink future term of PR 13f is a VERDICT on the box), `rr4k` PAR, `rw4k` / `w_fresh` / remount within noise, `mount` +0.1 s on a 0.4 s event (within noise in bracket 1, DELTA by 0.03 over a 30 % band in bracket 2), a NEW, UNATTRIBUTED +0.5–0.9 s on the clean UNMOUNT after `rw4k` (DELTA in both brackets; the re-run's B was FASTER there — a flat-path change of the flip candidate, OWED with its instrument, §7 item 16); gate 3c MET on all three laws ×2 (the PAUSED law's first real run; F-R3 FIXED / F-R4 FIXED as far as the leg reaches); gate 7 at N = 32 MET ×2 with 0 trips on 32 writers and the `/proc/diskstats` face 1.000×; gate 3's row set STOPPED at r1 — F-B1 NOT FIXED: two manager trips inside the joiners' create-storm grant bursts with the derivation engaged (F-R5 beside it)**

**The counted-run law**: every row set below ran FROM ZERO on PR 13e /
13f's binary; nothing from §3.9.4 is creditable. Gates 2 / 3b / 5 (MET
on the previous binaries) were NOT re-run (minimum count — they rerun
once on PR 14's flip binary). The row sets: gate 1 (ONE A-B-B-A + the
reversed bracket for the rows that read DELTA), gate 3 (`sym-scale`
N = 1/2/4/8 as ONE row set, judged on F-B1), gate 3c (`sym-foreign-touch`
×2 on fresh fleets — the PAUSED law's first real run on any venue, F-R3's
and F-R4's verdicts), gate 7 (`sym-walls` at N = 32 ×2 on fresh fleets,
the `/proc/diskstats` face on row (a) for the first time).

**Venue (re-verified 01:53 UTC, 2026-09-23, before the first leg):**
`squeeze-test` (`memp-s3ds-aqs-37`), 32-core Xeon, 251 GiB, Rocky 8.10,
**kernel `6.19.14-sqz`** (the sqz series incl. patch 0031 — the per-queue
bg budget, since 2026-09-06), up 1 d 0 h 45 at the first leg, load 0.00,
no Lustre / lnet modules, docker inactive, lnet failed (inactive), no
daemon, only `fusectl` mounted, no `/run/squeezefs-mwfleet*` /
`-devsub-*` (`/run/squeezefs/` holds the box-rows rung's four stale IL
sockets, as the re-run found them), no netns / veth / `pref 40` rules,
246 G free on `/scratch`; the reset-v5 converged fabric (5 storage
nodes × (1 meta + 2 data) memory-backed null_blk namespaces over
nvme-tcp, the client's 15 controllers connected as the re-run left them;
the storage nodes' `squeezefs 1.1.0` copies serve the reset script's
`nvmeof` verbs — NOT refreshed, as before). Gate 1 ran on the fabric via
the reset script; the N-writer gates on the box's own tcp devsub
(nvmet-tcp on `127.0.0.1`, `resv_enable=1`, `lzo-rle` zram,
`SQZ_MWFLEET_OSS_GB=16`, `--venue=box` on every leg). **Arms:** **A** =
`3228fcb8` (reused — sha256 `993100757bde…d69b1` verified on the box);
**B** = `b377cbb8` (= the code tip `7d8d9807` + PR 13f + 13d + PR 15's rig
+ the box re-run record; the batch `task check` GREEN on it) — built by
the orchestrator (`task build:rocky8`, the `release` profile, from a
detached checkout at that sha), staged at `/tmp/grok-justin/box-13e/arms/`,
`sha256sum -c` OK on both sides: **`squeezefs 1.2.4 (b377cbb8696e /
b377cbb8696ead8d45b483b2931620cbe86979d3) built 2026-09-23T01:43:06Z
profile release`**, sha256
`b0ec7d65ec7d9d657ca28863ab51e36d01ff8906a466e83139258cc025128435`
(shim `6789dd591c3d…689fc5dd`), placed as
`/scratch/tmp/sym-box/squeezefs-B-b377cbb8` and as `/scratch/tmp/squeezefs`
(the reset script's client binary; `squeezefs-B-77f4da1d` and
`squeezefs-B` kept beside it for provenance). Both arms `release` (the
two-profile law). **Instrument:** PR 1's rig at its 2026-09-22 revision
(RT = 60 s honoured, the ten data namespaces' `/proc/diskstats` per row),
the reducer verbatim; `fio-3.36`, the box's standing job files (libaio,
`direct=1`, 24 jobs, `ramp_time=10`); for the N-writer legs the driver
`2026-09-21-sym-box-brackets.sh` + `run_mw_matrix.sh` at THIS branch's
revision (`8c2da0dc`: the sym WRITE rows bracket their writes with the
data namespaces' `/proc/diskstats` and print device ÷ user + `wareq-sz`;
every sym row prints F-B1's faces per writer —
`appender_flush_ceiling_overruns`, `meta_kv_checkpoint_{term,trigger}_ms`,
the excused Σ; the foreign-touch leg keeps every touch's per-create
status and dies on an errno to the application; `xv_cross_owner_
dangling_names` joins `SYM_ZERO_KEYS`). The laptop stayed idle through
the session (the batch gate on `b377cbb8` was running on it — nothing of
this rung ran there).

##### 3.9.5.1 Gate 1 — the solo re-gate, flat A vs flat B (01:59 → 02:31 UTC bracket 1, A B B A, all rows at RT = 60; 02:35 → 02:57 the reversed bracket, B A A B, `ROWS="mdstorm mount rw4k-kern remount"` — the shapes of the two rows that read DELTA): **`rename` 1.012 / 0.998 PAR and `unlink` 1.054 / 1.042 — B AHEAD in BOTH orders (every B above every A) — PR 13f's fix CLOSES the re-run's two DELTA rows on the box; `rr4k` 1.005 PAR; `rw4k` 0.984 / 1.015, `w_fresh` 0.991, remount 0.947 / 0.931, `mkdir` / `stat` / `manydirs` / `rmdir` within noise; `create` 0.981 / 0.972 within noise by the 3 % floor (a reproducible −1.9…−2.8 %, every B below every A); `mount` 1.251 (within a 31.5 % band) / 1.326 (DELTA by 0.03 over a 29.7 % band) — B's FIRST mount of a fresh set ≈ +0.1 s on a 0.4 s event; `umount` 1.109 / 1.388 DELTA in both brackets — the clean unmount after `rw4k` +0.5–0.9 s (B 8.1–8.5 s vs A 6.4–7.7 s outside the first-position outliers) — NEW on this binary (the re-run's B read FASTER than A there) and UNATTRIBUTED, routed as owed**

**Verdict table (the two brackets, PR 1's rule: within noise iff |B/A − 1| ≤ max(band, 3 %)):**

| row | bracket 1 (A B B A) B/A · band | bracket 2 (B A A B) B/A · band | positions (bracket 1 ; bracket 2) | **verdict** |
|---|---|---|---|---|
| `wfresh-kern` MiB/s (60 s) | **0.991 · 3.0 %** | — | A 34,789 / 33,764 — B 33,591 / 34,375 | **within noise** (−0.9 %) |
| `rr4k-kern` IOPS (60 s) | **1.005 · 0.5 %** | — | A 593,990 / 596,858 — B 598,916 / 598,049 | **PAR** (the re-run's 0.999 again; daemon 27.9–28.1 µs/op on both arms, the zc bridge hops identical to 0.1 µs) |
| `rw4k-kern` IOPS (60 s) | **0.984 · 1.0 %** | **1.015 · 4.3 %** | A 538,061 / 535,956 ; 538,072 / 515,573 — B 525,563 / 531,051 ; 537,547 / 532,309 | **within noise** — the re-run's reproducible −3.0 % narrowed to −1.6 % in bracket 1 and did NOT reproduce in bracket 2 (B1 537.5k ≡ A2 538.1k; A3's 515.6k is the bracket's outlier); the `write` handler's future (20 KiB, PR 14's follow-on) reads as +0.4–0.6 µs/op of `fuse3-ur` in bracket 1 and 0 in bracket 2 |
| `mount` s (first mount of the fresh set) | 1.251 · 31.5 % | **1.326 · 29.7 % DELTA** | A 0.382 / 0.525 ; 0.401 / 0.371 — B 0.533 / 0.602 ; 0.588 / 0.436 | within noise in bracket 1, DELTA by 0.03 over the band in bracket 2 — **UNCONVICTED by the standing A-B-B-A law (a single-order delta is an ordering artifact until the reversed bracket reproduces it), NOT within noise**; **directionally consistent across the THREE box brackets on two binaries — 1.132 (`77f4da1d`: A 0.483 / 0.411, B 0.424 / 0.588), 1.251, 1.326 — B's first mount of a fresh set slower by 0.06–0.12 s of median each time** (here B 0.44–0.60 s vs A 0.37–0.53 s; every B ≥ every A but one position, B4 0.436 vs A4 0.525); a 0.4 s event the rig resolves at 30 % bands — stated, the ms-grained mount tape is PR 14's instrument |
| `remount` s | 0.947 · 16.2 % | 0.931 · 12.9 % | 0.54–0.64 s both arms | within noise (B ahead) |
| `umount` s (the clean unmount after `rw4k`) | **1.109 · 1.3 % DELTA** | **1.388 · 33.7 % DELTA** | A 7.617 / 7.602 ; 6.411 / 7.704 — B 8.385 / 8.497 ; 11.446 / 8.142 | **DELTA in both brackets** — outside the first-position outliers (B1 11.4 s here; the re-run's A1 13.5 s) B 8.1–8.5 s vs A 6.4–7.7 s, +0.5–0.9 s; the whole unmount is the `Force flushing all in-memory write buffers` step (8 → 9 s at the ladder's second precision; nothing else in the ladder moves); **the re-run's own positions on `77f4da1d` read the OTHER way — A4 8.693 / B2 8.060 / B3 7.615 (A1 13.517 the outlier): B 0.6–1.1 s FASTER there — so the +0.5–0.9 s is NEW on `b377cbb8` relative to the re-run's read, and its attribution is OPEN** (13e's cadence change claims decision-identity on a bit-17-absent volume — the shutdown fixpoint's final cycles are where that claim meets this row; 13f boxed the handler futures; neither is convicted, neither excluded). Not a gate-1 law (the design names MOUNT time) but a flat-path change the flip would ship — **OWED (§7 item 16)** with its instrument: a phase tape on the `Force flushing all in-memory write buffers` step (the shutdown ladder has second-precision INFO lines and nothing per step); the end-of-`rw4k` state is the same shape on both arms (`patch_ineligible_overlay` 5.4–5.7 M, 24 open rewrite epochs, `write_through_blocks` 4.4–4.8k, no parked extents) |
| `mdstorm mkdir` ops/s | 0.993 · 4.8 % | 0.983 · 1.6 % | A 6,505 / 6,775 ; 6,621 / 6,575 — B 6,755 / 6,436 ; 6,431 / 6,536 | within noise |
| `mdstorm create` | 0.981 · 0.8 % | 0.972 · 0.9 % | A 5,871 / 5,883 ; 5,866 / 5,919 — B 5,787 / 5,743 ; 5,738 / 5,722 | within noise by the 3 % floor — **a reproducible −1.9 % / −2.8 %**, every B below every A (the re-run read 0.992 / 0.983; the fp legs there named `lock_stripe` +0.5 µs/op on create); the routed `create` phase 155–163 (B) vs 154–159 µs (A) |
| `mdstorm stat` | 0.996 · 3.1 % | 1.004 · 2.7 % | 185–192k both arms | within noise |
| `mdstorm rename` | **1.012 · 2.0 %** | **0.998 · 1.5 %** | A 4,447 / 4,472 ; 4,418 / 4,460 — B 4,560 / 4,468 ; 4,463 / 4,396 | **PAR in both orders — the re-run's 0.960 / 0.967 DELTA CLOSED**; the PR-4 rename lock-set fix's +1 guard STAYS (`dlm_guard_hold` 889.5–889.7k → 989.7–990.0k per storm, the routed `rename` phase 87–89 vs 83–86 µs — its priced ≈ +2.6 µs) inside a PAR row |
| `mdstorm unlink` | **1.054 · 0.5 % DELTA (B ahead)** | **1.042 · 2.6 % DELTA (B ahead)** | A 5,195 / 5,220 ; 5,205 / 5,252 — B 5,497 / 5,482 ; 5,375 / 5,518 | **B FASTER by 5.4 % / 4.2 % in both orders, every B above every A — the re-run's 0.956 / 0.967 DELTA REVERSED** (the DELTA word is the reducer's — a change past the band; here in B's favour) |
| `mdstorm manydirs` | 1.019 · 3.8 % | 0.981 · 2.0 % | 10.9–11.4k both arms | within noise |
| `mdstorm rmdir` | 0.998 · 5.3 % | 0.976 · 1.7 % | 5.6–6.1k both arms | within noise |

**Gate 1 as the design states it ("within noise of `dev` tip on mdstorm,
rand-4k, `w_fresh`, and mount time; `dlm_rpcs == 0`") reads on PR 13e /
13f's binary: `dlm_rpcs` 0 ✓ (all 16 mount legs + 8 mdstorm legs),
mdstorm ✓ (`rename` PAR, `unlink` B ahead, `create` −2…−3 % at the floor,
the rest within noise), rand-4k ✓ (`rr4k` PAR, `rw4k` within noise both
orders), `w_fresh` ✓, mount time — within noise in bracket 1, DELTA by
0.03 over a 30 % band in bracket 2 on a 0.4 s event (a +0.1 s mean shift
on B's FIRST mount of a fresh set; remount PAR): **MET by the rule — PR
1's band rule (`|B/A − 1| ≤ max(band, 3 %)` on the medians of the same-arm
positions) composed with the standing A-B-B-A law ("a single-order delta
is an ordering artifact until the reversed bracket reproduces it") — with
the mount row UNCONVICTED, not within noise**: it read within noise in
A B B A and DELTA in B A A B, and across the three box brackets on two
binaries it is directionally consistent (1.132 on `77f4da1d`, 1.251,
1.326 — B slower by 0.06–0.12 s of median each time, every B ≥ every A
but one position); a 0.4 s event at 30 % bands, named for PR 14's
ms-grained mount tape. The `umount`
row (DELTA in both brackets, B slower) is not a gate-1 law but it IS a
flat-path change relative to the re-run's read (B faster there) — owed
with its instrument, never waved off (§7 item 16). **The gate-1
setattr term (§3.9.4.1's attribution) — the box's VERDICT: FIXED.** The
kernel's SETATTR echo runs once per rename and unlink on both arms
(`meta_kv_times_echo_absorbed` 299.3–299.9k per storm, both arms, both
brackets), and the handler lanes read **`fuse3-tpc` 120.5 / 122.3 /
121.5 / 120.6 µs per op on B against 125.6 / 125.2 / 125.5 / 126.0 on A**
(B −4 µs/op; the re-run read B +3.2–5.0), the daemon 221.8–225.6 vs
226.4–228.1 µs/op, with the conveyor pass (38.2–39.1 µs), the durability
window (57.4–58.6), the journal lane (35.9–37.6) and the meta lanes
(17.4–18.1) identical arm to arm and the metadata economy identical
(`Δjournal_entries` 547.6–547.7k, checkpoints 70–71, node appends
24.3–24.8k, splits 96–97, `Δfsck_findings` 0, tripwires 0, the five
`meta_kv_forest_*` gauges 0 on every B mount — bit 17 absent on every
meta volume of both arms). PR 13f's fix (setattr 26,016 → 896 B, unlink
11,088 → 280 B, `commit_tx` 4,816 → 408 B) took out more than the PR-4
guard's +2.6 µs put in.

**Write-amplification columns (the AGENTS instrument — `/proc/diskstats`
on the TEN data namespaces per row, ramp-inclusive — beside the daemon's
ledger and the reclaim family):**

| row / position | user GiB (60 s) | **device write ÷ user** | `wareq-sz` | device read ÷ user | ledger `rewrite_device_write_bytes` ÷ user | `patch_write_bytes` ÷ user | reclaim `commands` / `discards` / trim bytes ÷ user | `write_through_blocks` |
|---|---|---|---|---|---|---|---|---|
| `wfresh` A1 / A4 | 2,047 / 1,979 | **1.113 / 1.118** | 1,037 / 1,038 KiB | ≈ 0 | 1.019 / 1.021 | 0 | 27,096 / 27,469 / 0.052 ; 59,965 / 60,546 / 0.119 | 7,130 / 7,859 |
| `wfresh` B2 / B3 | 1,978 / 2,021 | **1.113 / 1.115** | 1,038 / 1,038 KiB | ≈ 0 | 1.016 / 1.020 | 0 | 91,781 / 92,850 / 0.183 ; 10,490 / 10,620 / 0.021 | 8,010 / 8,046 |
| `rw4k` A1 / A4 (bracket 1) | 123.2 / 122.7 | **1.139 / 1.142** | 5 KiB | 0.007 / 0.004 | 0.148 / 0.152 | **0.981 / 0.982** | 0 / 0 / 0 | 4,675 / 4,764 |
| `rw4k` B2 / B3 (bracket 1) | 120.3 / 121.6 | **1.147 / 1.139** | 5 KiB | 0.006 / 0.009 | 0.153 / 0.145 | **0.984 / 0.981** | 0 / 0 / 0 | 4,701 / 4,514 |
| `rr4k` A1 / A4 | 136.0 / 136.6 (reads) | 0 (no writes) | — | **1.171 / 1.168** (`rareq-sz` 4 KiB) | 0 | 0 | 29,833 / 11,806 (the prep's discards draining) | 0 |
| `rr4k` B2 / B3 | 137.1 / 136.9 | 0 | — | **1.171 / 1.171** | 0 | 0 | 31,424 / 27,459 | 0 |

Device write bytes ≡ user bytes on both arms and both write rows (the
ramp explains the 1.11–1.15×; `wareq-sz` = the write size — 1 MiB on
`wfresh`, the 4 KiB in-place W1 patch on `rw4k` — no request-size
collapse), device read ≡ user on `rr4k`; the reclaim command counts on
`wfresh` scatter by position on both arms (10–92k, 0.02–0.18× user in
trim bytes — the elision's timing, not an arm term; the re-run read
39–61k). Thermal: the hottest hwmon sensor 47–51 °C at every row start
and end (no heat soak). Artifacts:
`/scratch/tmp/sym-box/rows-gate1-13e-20260923-015902{,-rev}/` (+ `.log`,
`REDUCED.md`).

##### 3.9.5.2 Gate 3 — `sym-scale` (B, the armed plane; N = 1/2/4/8 as ONE row set on fleet A, 03:05:59 → 03:15:26 UTC): **F-B1 TRIPPED TWICE on the manager — `appender_flush_ceiling_overruns` +1 in the N = 4 row and +1 in the N = 8 row, both on its second metadata volume, 1,127 / 1,125 ms (25–27 ms past the ceiling), with PR 13e's derivation ENGAGED but its horizon EMPTY at both trips — `meta_kv_checkpoint_term_ms` read 11 / 4 ms (trigger 989 / 996) when each row's joiner storm began, the overrunning cycle's own ≈ 130 ms term entering the horizon only AFTER (133 / 127 at the rows' ends), and the 199 quiet cycles between the rows had forgotten the N = 4 storm's term before N = 8's first storm cycle; the storm's STEADY STATE (62–83 grants/s over the heavier seconds that followed) did NOT trip again — the class is the FIRST storm cycle after a quiet horizon; nothing excused; the row set stopped at r1 (the counted-run law). Both trips sit inside the first seconds of a JOINER CREATE STORM while the manager served an `ExtentGrant` / `ReturnExtents` burst of 50–80 verbs/s — the joiners' 512 KiB floor rings checkpointing ≈ 8×/s under the storm and refilling in ≤ 8-extent grants — F-R5: the manager derives a WIRE joiner's grant from an EWMA it never receives (`ewma = 0` → the floor 8), so §5.3.3's derivation is inert for every production joiner; the wall multiple 3.53× at N = 8 (per-writer storms 9.2–14.4 s; an INFERRED ≈ 3.9 s of launch skew — wall 18.37 − the longest storm 14.44 — with the root's STRIPING at the row's mkdirs the hypothesised cause; the storms' own concurrency ≤ 4.5×, an upper bound), `C/CPU-S` 0.67×, ingest 5.50×**

The A arm (`mw-scale`) was NOT re-run: its 0.07× (§3.9.4.2, F-R2) is the
shipped posture's number and 13e / 13f change nothing on that path.
Order: the fleet fresh (manager + 7 joined writers + 1 token reader,
`create N=2 --symmetric --writers=7 --token-readers`), ONE row set.

| N | **create/s · ×N=1** | `C/CPU-S` (create phase) | **ingest MiB/s · ×** | `MGR_LOAD` / `MGR_CPU` / handovers / ships / rpcs | ingest amplification (`/proc/diskstats`, the 2 data namespaces; user N × 1 GiB): device write ÷ user · `wareq-sz` · device read ÷ user | per-writer create storms (s) | verdict (the leg's) |
|---|---|---|---|---|---|---|---|
| 1 | **4,928 · 1.00×** | 4,133 | 1,327 · 1.00× | 3 % / 114 % / 0 / 0 / 0 | **1.239** · 1,518 KiB · 0.239 | 8.1 | MET |
| 2 | **9,102 · 1.85×** | 3,860 (0.93×) | 2,406 · 1.81× | 1 % / 115 % / 0 / 1 / 0 | **1.194** · 1,366 KiB · 0.194 | 8.8 / 7.8 | MET |
| 4 | **15,645 · 3.17×** | 3,241 (0.78×) | 4,613 · 3.48× | 0 % / 126 % / 0 / 3 / 0 | **1.139** · 1,308 KiB · 0.139 | 9.2–10.2 | the wall law MET (≥ 2.8×); **`MISS(must-stay-0: m0 appender_flush_ceiling_overruns +1)`** |
| 8 | **17,415 · 3.53×** | 2,782 (0.67×) | **7,292 · 5.50×** | 0 % / 88 % / 0 / 4 / 0 | **1.129** · 1,266 KiB · 0.129 | 9.2–14.4 (2,770–4,346 c/s each) | the wall law MISS on creates (3.53× vs ≥ 5.6×), MET on ingest; **`MISS(must-stay-0: m0 +1)`** |

**F-B1 — the box's VERDICT on PR 13e's derivation: NOT FIXED — the
tripwire trips, twice, and the derivation was live when it did.** The
manager's kept log (`13e-nw-20260923-030537-scale/scale-r1/daemon-logs/m0.log`):
`03:08:22Z WARN … meta volume /dev/nvme31n1: flush ceiling OVERRUN —
appender region(s) [(0, 1127)] … exceeded the 1100 ms landing ceiling with
every structural hold's capped overlap excluded` and the same at
`03:10:56Z` with `(0, 1125)` — region 0 (the manager's own), the SECOND
metadata volume both times, 27 and 25 ms past the ceiling. **The
derivation's faces, TIMED against the trips (the row-START snapshots
beside the row-END faces — review round 1, Issue 2):** at N = 4's start
(`m0_pn40.json`) m0 read `meta_kv_checkpoint_term_ms` **[23, 11]** /
trigger **[977, 989]** — volume 1's cadence firing at 989 ms, the
shipped 1,000 ms to within 1 %, when the joiners m61 / m62's first storm
began; after the row (`pn4c`) **[81, 133]** / **[919, 867]** — the
overrunning cycle's OWN term, in the horizon only once it had run (age
1,127 ≈ (1,000 − 11) + a ≈ 138 ms cycle). Between N = 4's end and N = 8's
start the manager ran **199 checkpoints** (`pn41` 267 → `pn80` 466, ≈ 1.5
per second over the inter-row `rm -rf` + joins window of ≈ 135 s), so at
N = 8's start (`pn80`) volume 1's term read **[36, 4]** / trigger
**[964, 996]** — **the 64-cycle horizon had FORGOTTEN the N = 4 storm's
133 ms** before m63..m66's first storm cycle tripped (age 1,125 ≈ (1,000
− 4) + ≈ 129 ms); after the row (`pn8c`) **[36, 127]** / **[964, 873]**.
The joiners' end-of-row terms 4–136 ms (m61 [136, 9], m63 [5, 131], m66
[15, 120]), triggers 864–996; `appender_flush_ceiling_excused_ns` 0 /
`…_service_extensions` 0 / `…_recovery_extensions` 0 on every writer,
the ceiling 1,100 published. So each overrunning cycle's term sat
≈ 120 ms ABOVE its horizon maximum (≈ 130 vs 4–11 ms) — and **once the
term WAS in the horizon, the storm's steady state did not trip: the
heavier seconds that followed each trip (62–83 grants/s over
03:10:57–03:11:05, 83 at :57) ran under a trigger of 867–873 ms and
landed inside the ceiling.** The class the box reads is therefore **the
FIRST storm cycle after a QUIET horizon** — a 64-CYCLE memory decays in
≈ 40–45 s on a manager checkpointing ≈ 1.5×/s between rows, shorter than
the inter-row window — not a burst the derivation cannot see in general
(the sustained shape it priced). **What the burst was (the kept log,
second by second):**
both trips fall in the first seconds of a joiner CREATE storm — the N = 4
storm (the joiners m61 / m62's first storm, ≈ 03:08:15 → 03:08:26) and
the N = 8 storm (m63..m66's first, ≈ 03:10:50 → 03:11:07) — while the
manager served an **`ExtentGrant` burst: 62 grants logged in the second
03:08:16, 85 over 03:08:16–19, 30 over 03:08:20–22 (the trip at :22);
69 / 20 / 9 / 42 / 23 / 18 per second over 03:10:51–56 (the trip at :56),
83 at 03:10:57, 50 at 03:11:00** — 1,483 grants in the leg, sized
**845 × 4, 180 × 3, 147 × 2, 2 × 1 (the reactive class: the flush pass
that exhausts the grant asks `needed.max(SMO_IMAGES_MAX)` = 4, which the
`GRANT_RUNS_MAX` coalescing loop trims to 1–3 on a fragmented heap by
releasing the smallest all-new run) and 103 × 5, 54 × 6, 44 × 7, 108 × 8
(the cadence's PROACTIVE refill — `refill_due()` → `want == 0` → the
DERIVED size, which is 8 for every wire joiner, F-R5 below)** — the log
line prints the NEW extents carved (`claimed.len()`); in the N = 8 storm
window (03:10:49–03:11:08) 449 grants, 283 of them × 4 and 29 × 8.
Over the N = 8 create the manager's volume 1
served **344 grants + 318 returns = 662 verbs in ≈ 13 s (≈ 51/s)** with
`manager_service_ns.execute` **+2.36 s** (3.6 ms per verb — each a ring-0
control entry + its barrier on the same journal lane the checkpoint
cycle's barrier #1 queues on), volume 0 +0.47 s over 167 verbs (+220 over the whole row, `pn80` → `pn81`);
`manager_verbs` [824, 1229] → [991, 1891] (Σ since mount 2,935 by the
leg's end, `extent_grants` [274, 702], `extent_returns` [208, 723]);
`manager_load_pct` read [0, 1] — the load gauge's window does not see a
13 s burst. **Why the joiners ask that often — a finding beside F-B1
(F-R5, reported not fixed):** every joiner's ring is at the **512 KiB
floor** on both volumes (`appender_ring_bytes` 524,288, `appender_ring_
grows` 0, `joined_ring_grow_declined` [0, 1] — PR 2's drain-then-grow
bound, PR 12b's "ring growth is DECLINED on a joiner"), so under a 40k-file
create storm a joiner's ring fills every ≈ 120 ms: m60 ran
**109 checkpoints in the 13 s N = 8 create** (`appender_pressure_cycles`
+79 on volume 1), each cycle RETURNING the images its barrier RETIRED
(`take_returnable()` — the §4.7 tail-gated retirements, never the
unclaimed remainder, which returns only at a release or the leave:
`extent_grant_returned` [68, 896] → [68, 1,089], **+193**) and asking the
manager again as its grant runs dry — `joined_wire_extent_grants` **+47**
(the reactive `needed.max(4)` asks AND the cadence's proactive
`refill_due()` asks — 309 of the leg's 1,483 grants are the derived-size
class, so the 50 % refill DOES engage), `extent_grant_claimed` 64 → 72,
`unclaimed` 8 → 4 — ≈ 105 wire verbs on m60's volume 1 in one 13 s storm:
**claim-and-retire churn at the SMO grain**, every grant ≤ 8 extents
(≤ 2 MiB), ≈ 100 manager verbs per joiner per storm. Over the wire every
joiner `ExtentGrant` is served by `manager_extent_grant` in the USER class
(`meta_ship/manager.rs`), clamped to the derived cap; the INTERNAL-class
arm is the in-process regions' (the manager's own, the test seam's).
**The root cause (the code — review round 1, Issue 3):**
`KvMetaBackend::grant_extents_for` (`kv/backend.rs:9148`) derives the
grant from `set.region(appender_id).smo_ewma_milli` — **`None` for a WIRE
joiner** (the manager holds no `AppenderRegion` for it; the same
`region(id).is_none()` is the wire-joiner discriminant a few lines below),
so **`ewma = 0` and `grant_extents_derived(0, …)` answers the FLOOR
(`GRANT_EXTENTS_FLOOR` = 8) for every production joiner whatever its SMO
rate**; the joiner folds its own EWMA locally (`AppenderRegion::fold_smo_
rate` from `joined_grant_cadence`) and nothing carries it on `ExtentGrant
{ appender_id, want }`. At m60's ≈ 8 SMO/s the design's §5.3.3 derivation
(2 × 8,000 milli × 45,033 ms / 10⁶ ≈ 720 extents, capped by `free_heap /
(4 × appenders)`) would answer HUNDREDS, not 8 — the derivation is inert
for the only non-manager appender production has. The manager's term
under that service is F-B1's residue: §7 item 3's derivation prices the
cycle's OWN past terms, not the verb service the joiners' cadence puts on
ring 0 in the same second. Remedy shapes (**PR 13g is the fix rung**):
carry the joiner's EWMA on the ask (or its page) so the manager derives
the real size — or let the joiner ask its own derived size, screened —
(the lead lever: ≈ 100 verbs per joiner per storm → ≈ 1); beside it the
joiner's ring past the floor (PR 2's owed drain-then-grow, or the
EWMA-sized join — the second mechanism: a 512 KiB ring is what makes a
storm ≈ 8 cycles/s), and/or the
manager's term folding the in-flight verb service, and — the reading the
trips' timing adds — the horizon's MEMORY: a term forgotten after 64
quiet cycles re-trips at the next storm's first cycle, so the term needs
a time bound or a floor beside its cycle count (§7 item 3).

**The rates (the wall law re-read):** N = 2 / 4 MET (1.85× / 3.17×
creates, 1.81× / 3.48× ingest); **N = 8 3.53× creates (MISS vs ≥ 5.6×) /
5.50× ingest (MET)**. The create multiple is LOWER than the re-run's
4.27× while the per-writer storms were FASTER: 9.2–14.4 s (2,770–4,346
c/s each) against the re-run's uniform 13.5–14.9 s (2,687–2,973) — the
leg's wall counts from the row's first `mkdir` to the last storm's end
(`run_mw_matrix.sh`: `t0` before the sequential `mkdir -p` + spawn loop,
`t1` at the last `wait`; NO per-storm launch stamp), so the row's launch
skew is **INFERRED, not measured: wall 18.37 s − the longest storm
14.44 s = 3.93 s**, which equals the skew only if the longest storm (m63,
launched fifth) also finished last. Its CAUSE is a hypothesis: the eight
`mkdir /scale-…-n8-w*` into `/` by eight creators are the 3b shape, and
**the ROOT STRIPED at 03:10:48** (`directory 1 STRIPED into 64 stripes (3
supplied by creators [1, 2, 3], 61 minted by the holder)`, migration
complete at :48) two seconds before the first joiner grant burst
(03:10:50–51) — the flip holds `I{D}` + the marker keys across its
intent, so the later `mkdir`s parking on them is plausible, not shown;
the flip's own cost sat inside the row's wall either way (the design's
striping arm engaging on the row's setup — a venue-of-the-harness term
beside §3.9.3's co-located term). The storms' own concurrency, had they
started together, is bounded ABOVE by 320,000 / 14.44 s = 22,160 c/s =
**≤ 4.5×** (eight storms launched together contend more than staggered
ones — an upper bound, not a read). The harness item: a per-writer
launch stamp (so the skew is measured) and the wall law's clock at the
LAST storm's launch. The
`C/CPU-S` face 4,133 → 3,860 → 3,241 → 2,782 (0.93× / 0.78× / 0.67× —
the re-run read 0.92× / 0.77× / 0.61×). **The per-NODE law stays PR
15's.** Engagement: handovers 0, ships 0 / 1 / 3 / 4 (≤ N), `dlm_rpcs` 0
on every writer, `appenders_known` == N at every row, `manager_load_pct`
≤ 3 %, `MGR_CPU` 88–126 %. Deleted-stays-deleted and the fsck oracle
were NOT reached (the leg dies on the must-stay-0 set before them, as
§3.9.4.2's did). **Amplification (the first `/proc/diskstats` read on an
N-writer leg — the §3.9.4.5 owed item):** the ingest's device writes
1.239× → 1.129× user from N = 1 to 8 with `wareq-sz` 1,518 → 1,266 KiB
and device READS 0.24× → 0.13× of the user bytes — the device-overlay
vehicle's kernel-split segments (a 4 MiB `dd` write arrives as ≤ 1 MiB
FUSE writes; the overlay stores each and settles with a ranged gap seed
— the re-run's ledger read `flush_seed_read_bytes` 0.17× at N = 8), the
same shape on every N, ≈ 1.1–1.2× device ÷ user.

##### 3.9.5.3 Gate 3c — `sym-foreign-touch`, two positions on fresh fleets (03:20:50 → 03:33:59 UTC; `13e-nw-20260923-032027-foreign-touch/foreign-touch-r{1,2}`): **every law MET as a VERDICT in BOTH positions — LIVE 192 ships / 0 handovers ×2; IDLE moved after 4 bursts ×2 (17.9 / 17.6 ms); the PAUSED law's FIRST REAL RUN ON ANY VENUE — a live STOPPED job through the touches, resumed to its 40,000 mkdirs, handovers 0 / idle offers 0 / dominated offers 0 ×2; F-R3 FIXED — zero "no inode record" lines, `xv_cross_owner_dangling_names` 0 on every writer, the post-leave census AND the offline census (`current_era_exempted` 0, findings 0) clean ×2; F-R4 FIXED as far as the leg reaches — 0 errnos on 451 / 451 touch creates (902; the slot-moved class unexercised, retries 0); F-B1 0 on the fleet, every position's oracle clean, `rc=0`**

| position | LIVE (3 bursts × 64 into the live holder's tree) | IDLE (bursts of 64 at the 10 s beat until the handover) | `slot_handover_phase_ns` (the departing holder m60) | **PAUSED (3 single touches at 5 s over the half-window; the job STOPPED then resumed)** | own create rate (A) | **F-R3 / F-R4** | oracle + census |
|---|---|---|---|---|---|---|---|
| r1 (03:20) | **192 ships, 0 handovers** ✓ | handed over after **4 bursts** (50.9 s; 253 ships, 2 offers), 0.020 handovers/s | **17.94 ms** = flush 13.75 / tree 0 4.10 / page 0.10 / grant 0 | **MET** — the job a live STOPPED process (`State: T`) through the three touches, resumed and COMPLETED (`mkdir ops=40000 wall_s=23.330 ops_s=1715`), the holder's journal **+60,133**; **handovers 0, holder `slot_offers_idle` +0, `slot_offers_dominated` +0** | 5,320 c/s | zero `no inode record` lines in m0 / m60 / m61's logs after the `rm -rf`; `xv_cross_owner_dangling_names` 0 ×3, `xv_cross_owner_witness_refusals` 0 ×3 (m60 served 445 steps, m61 shipped 449); **0 touch creates answered an errno** (451 creates: LIVE 3 × 64 + IDLE 4 × 64 + PAUSED 3 × 1; m61's `steps_shipped` 1 → 193 → 449 → 452 is the SAME creates' shipped steps + the job-dir mkdir, not a second population) | fsck 0, C8 0, must-stay-0 flat; **post-leave census** (every joiner left): inode plane 2 of 2 volumes, C9 = C10 = 0; **offline census** (the manager left too): covered 2 of 2, `current_era_exempted` **0**, `unreferenced_intent_exempted` 0, `inode_plane_slots_covered` 1,026, findings **0**; the manager's grace window waited out (44 s), every member re-admitted |
| r2 (03:27, fresh fleet) | **192 ships, 0 handovers** ✓ | handed over after **4 bursts** (50.8 s; 256 ships, 2 offers), 0.020 handovers/s | **17.59 ms** = flush 14.06 / tree 0 3.40 / page 0.13 | **MET** — `mkdir ops=40000 wall_s=23.539`, journal **+60,131**; handovers 0 / idle +0 / dominated +0 | 5,369 c/s | zero lines; gauges 0 ×3 (served 448 / shipped 452); **0 errnos** (451 creates) | clean; post-leave 2 / 2, C9 = C10 = 0; offline covered 2 / 2, exempted 0, findings 0; grace 44 s |

`N_floor(A)` seeded 2 on the box both positions, beat 10 s, `T_idle`
45,000 ms (the PAUSED touches at 5 s — inside the holder's half window).
The must-stay-0 set flat on all three writers through both positions,
`appender_flush_ceiling_overruns` **0** on m0 / m60 / m61 (the F-B1 faces:
terms 0–8 ms, triggers 992–1,000 — no storm on this fleet reaches the
class §3.9.5.2 reads), `dlm_rpcs` 0, `invariant_tripwires` 0,
`xv_cross_owner_intents_{open,stuck}` 0. **The PAUSED law (design §8 row
3c's third law; §4.4aj's harness defect) is EXERCISED for the first time
on any venue and reads MET twice**: the touched slot never read IDLE and
never read DOMINATED while the holder's job stood SIGSTOPped — the
subtree liveness credit (F-B2's fix, `install_liveness_ancestors`) held
the slot for the phase's whole window. **F-R3 — the box's VERDICT on PR
13e's fix: FIXED.** The re-run's leg logged 430 / 512 `no inode record —
removing the dangling name` lines per position at the same `rm -rf`;
this binary logs none, the must-stay-0 gauge reads 0 on every writer,
and the two census arms that were VACUOUS for the joiner-minted class
before PR 13e's era-floor fix (§4.4al, review round 1 Issue 2) now judge
it: the OFFLINE probe covers every slot (`inode_plane_slots_covered`
1,026) with NOTHING exempted and finds nothing. **F-R4 — the box's
VERDICT: FIXED as far as this leg reaches** — the leg's new per-create
ledger (`touch-errors.txt`) is EMPTY in both positions over 451 / 451
touches (902 in all — the record's first write added the ships gauge to
the count, review round 1 Issue 4), the IDLE handover included (the re-run's one `ENOENT` was on
the IDLE burst that moved the slot); the slot-moved class is
`xv_cross_owner_step_slot_moved_retries` (0 here — no create met the
window in these two handovers, so the retry path was not exercised; the
in-process pin is its proof). **The handover's cost grew**: 17.9 / 17.6
ms against the re-run's 7.5 / 6.7 ms, all of it in the departing
holder's FLUSH phase (13.75 / 14.06 ms vs 3.99 / 2.04; tree 0 3.4–4.1 and
the page 0.1 unchanged) — the flush-then-transfer's `checkpoint_now`
cycles until the region's tail passes the slot's frontier; on this
binary the joiner's cadence is PR 13e's (the anticipated-term trigger)
and the ring the 512 KiB floor's — the flush's cycle count is not
instrumented per handover (a `slot_handover_phase_ns.flush` cycle count
beside the wall is the instrument); at 0.02 handovers/s and 18 ms per
handover the row's cost law (handovers/s × cost vs the node's own rate:
5,320–5,369 c/s) stands — stated for PR 14's bracket. Gate 3c as the
design states it reads **MET on all three laws, twice, on the box**.

##### 3.9.5.4 Gate 7 — `sym-walls` at N = 32 (fleet C: the manager + 31 joined writers; `--walls-files=4`), two launches on fresh fleets (03:37:32 → 03:52:21 UTC; `13e-nw-20260923-033631-walls32/walls32-r{1,2}`): **row (a) MET ×2 — 1,269 / 1,344 frees/s at the holder, `displaced 1,984 ≡ shipped 1,984 ≡ served 1,984`, and for the first time the `/proc/diskstats` face: device write bytes 8,321,499,136 ≡ the daemons' ledger 8,321,499,136 ≡ 1.000× user at `wareq-sz` 1,204 / 1,192 KiB, device reads ≈ 0 (9 IOs / 73,728 B = 72 KiB, 0.000× user); row (b) MET ×2 — 32 mounts in 3.80 / 4.06 s, 263 / 291 manager verbs, Σ service 15.1 / 16.0 s, `JOBS_SHIPPED` 31 ≡ served fleet-wide 31; `appender_flush_ceiling_overruns` 0 on ALL 32 writers through BOTH launches (the re-run's launch 2 tripped on m65) — with the rewrite's terms READ and PRICED: row (a)'s faces anticipate 50–119 ms on the busiest joiners (r1 m71 119, m66 81, m65 65, m84 57 ms; r2 m75 101, m86 74, m87 62 … 9 writers ≥ 30 ms) and no writer trips — the derivation working where the horizon HOLDS the term; row (b)'s ≤ 7 ms; oracle clean ×2, `rc=0`**

| launch | row (a): 31 joiners × 4 × 64 MiB pre-written then REWRITTEN in place at once | **amplification (`/proc/diskstats`, the 2 data namespaces; user 7,936 MiB)** | row (b): 31 joiners leave, then all rejoin at once + `mkdir /jobs/<j>` each | F-B1 / oracle |
|---|---|---|---|---|
| 1 (03:37) | displaced **1,984 ≡ shipped 1,984 ≡ served 1,984**, minted 196, **1,269 frees/s**, rewrite wall **1.56 s** (7.75 GiB → 5.0 GB/s into the zram), `MGR_CPU` 13 %, 7 manager verbs (7.3 ms service), `manager_load_pct` 0, `free_ship_failures` 0, `free_refused_blocks` 0, the holder's `data_alloc_bitmap_clear_bits` +1,984, 0 reclaim commands at the manager | **device write ÷ user 1.000** (8,321,499,136 B — byte-exact with Σ `rewrite_device_write_bytes` ≡ Σ `rewrite_user_bytes` 8,321,499,136), `wareq-sz` **1,204 KiB**, device read ÷ user **0.000** (9 read IOs, 73,728 B = 72 KiB — the literal reading) | join wall **3.80 s** to the 32nd Live page (the design's 10 s ✓), **263 manager verbs, `manager_service_ns` Σ 15.11 s** (execute 15.11 s: volume 0 9.98 s / volume 1 5.13 s — `extent_grants` +31 / +31, `slot_grants` +1,984 / +1,984 = 31 × 64 rotor slots per volume), `MGR_CPU` 229 %, `manager_load_pct` [4, 3], `manager_failover_bound_ms` 45,033, `appenders_known` 32, `membership_members` 31; the `/jobs` ships: **31 shipped ≡ 31 served fleet-wide** (30 at the manager + 1 at a stripe holder; `/jobs` STRIPED once mid-storm), `JOBS_LOCAL` 0 — **MET** | **0 overruns on all 32 writers**; row (a)'s F-B1 faces: m71 `checkpoint_term_ms` **119**, m66 81, m65 65, m84 57 ms (5 of 32 writers ≥ 30 ms; m0 [0, 30]; triggers down to 881), excused 0 — the 31-way in-place rewrite's term anticipated and landed; row (b)'s faces m0 [6, 0], every joiner ≤ 6 ms; fsck 0, C8 0, the must-stay-0 set flat on every writer |
| 2 (03:45, fresh fleet) | 1,984 ≡ 1,984 ≡ 1,984, minted 0 (the grant windows covered the rewrite), **1,344 frees/s**, rewrite **1.48 s**, `MGR_CPU` 14 %, 6 verbs (89 µs), failures 0, refused 0 | **1.000** (8,321,499,136 ≡ 8,321,499,136), `wareq-sz` **1,192 KiB**, reads ≈ 0 (72 KiB, 0.000× user) | join wall **4.06 s**, **291 verbs, Σ 16.04 s** (execute: 9.61 / 6.44 s), `MGR_CPU` 228 %, load [4, 3], `appenders_known` 32; **31 ≡ 31** (30 at the manager + 1), flips 1 — **MET** | **0 overruns on all 32 writers**; row (a)'s faces: m75 **101**, m86 74, m87 62, m83 59, m90 56, m71 50, m61 47, m72 40, m88 32 ms (9 of 32 ≥ 30 ms; m0 [0, 0]), excused 0; row (b)'s ≤ 7 ms; oracle clean |

**The AGENTS amplification instrument is ON the N-writer legs now** (the
§3.9.4.5 owed item, `8c2da0dc`): row (a)'s device face reads EXACTLY the
daemons' ledger — 31 joiners rewrote 7,936 MiB in place and the two data
namespaces took 7,936 MiB of writes at 1.2 MiB per request (the 4 MiB
`dd` writes arriving as ≤ 1 MiB FUSE writes into the W1 whole-block
path) and 72 KiB of reads (9 IOs — 0.000× user; the rewrite of a fully
written file needs no seed read). The
design's row (b) names "WERO registers" in the storm: the 31 co-located
joiners ADOPT the manager's holds (KD-SYM-22), `pr_registrants_per_
namespace` 1 on each of the four namespaces both launches — no device
registration rides the storm on a one-box fleet; the wire's own cost is
the 263–291 manager verbs (≈ 52–55 ms of service per verb under 31
concurrent joins — the extent grants + 1,984 slot grants per volume).
**F-B1 on this fleet: 0 — and row (a) is the one box row where the
derivation is SEEN pricing a real term to 0 trips.** The 31-way in-place
rewrite put `meta_kv_checkpoint_term_ms` at **50–119 ms on the busiest
joiners** (r1: m71 119, m66 81, m65 65, m84 57 — 5 of 32 writers ≥ 30 ms;
r2: m75 101, m86 74, m87 62, m83 59, m90 56, m71 50, m61 47, m72 40, m88
32 — 9 of 32) — exactly m65's class from the re-run's 1,206 ms trip in
the same row — and **no writer tripped**: the horizon HELD the term
through the row and the trigger fired early by it. Row (b)'s faces read
≤ 6 / 7 ms (the join storm dirties little). The class §3.9.5.2 reads on
the manager — the FIRST storm cycle after a quiet horizon — needs a
joiner CREATE storm's grant burst on an emptied horizon, which this leg
does not run. **Gate 7 as the design states it reads MET on
both rows at N = 32 on the box, twice, with the tripwire 0.**

##### 3.9.5.5 The four fixes, judged on the box (F-B1 / F-R3 / F-R4 / the gate-1 setattr term), and what this pass found

| finding | PR 13e / 13f's fix | **the box's verdict on `b377cbb8`** |
|---|---|---|
| **the gate-1 setattr term** (§3.9.4.1: `rename` 0.960 / 0.967, `unlink` 0.956 / 0.967 — the `handle_setattr` future's construction + lane move on the kernel's per-op SETATTR echo) | PR 13f: the setattr / unlink futures 26,016 → 896 B / 11,088 → 280 B, `commit_tx` 4,816 → 408 B (the door's first-touch acquires boxed inside their branches) | **FIXED — a VERDICT**: `rename` **1.012 / 0.998** PAR and `unlink` **1.054 / 1.042** — B AHEAD in both orders (every B above every A); `fuse3-tpc` **120.5–122.3 µs/op on B vs 125.2–126.0 on A** across both brackets (the re-run read B +3.2–5.0); the PR-4 rename guard's +100k `dlm_guard_hold` per storm STAYS inside a PAR row (§3.9.5.1) |
| **F-R3** — every cross-owner unlink of a foreign-minted child read its witness off the PROJECTION and orphaned the child (430 / 512 per leg) | PR 13e: `read_inode_witness` at the child's HOLDER (the writer's divert), the dangling-name arm the must-stay-0 `xv_cross_owner_dangling_names`, fsck C9's era floor consulting tree 0 (the post-leave + offline census arms) | **FIXED — a VERDICT**: zero `no inode record` lines across the writers' logs after the same `rm -rf`, `xv_cross_owner_dangling_names` 0 on every writer of both fleets, the post-leave online census C9 = C10 = 0 covering every volume, the OFFLINE census after the manager's leave covering 2 / 2 volumes with **`current_era_exempted` 0** (1,026 slots covered) and findings 0 — twice (§3.9.5.3) |
| **F-R4** — a create into a directory whose slot moved TO the creator mid-burst answered `ENOENT` (once in 558) | PR 13e: a served step for a slot the holder no longer leases is the typed `SlotMoved` class; the wire grant adopts the tree before naming its lessee | **FIXED as far as the leg reaches**: 0 errnos on 451 / 451 touch creates (902 over two positions), the IDLE handovers' bursts included (the leg's new per-create ledger); the retry class itself (`xv_cross_owner_step_slot_moved_retries`) read 0 — no create met the window in these two handovers, so the pin is its proof (§3.9.5.3) |
| **F-B1** — `appender_flush_ceiling_overruns` tripping on the box with 0 ns excused (six increments on the re-run) | PR 13e: the cadence fires `max_age − term` from the last collection, the term = the horizon maximum of the measured cycle terms (§4.4an) — a derivation, never a widened constant | **NOT FIXED on the box — the tripwire trips with the derivation ENGAGED and its horizon EMPTY**: **two increments on the MANAGER's second metadata volume inside `sym-scale`** (1,127 / 1,125 ms — 25–27 ms past; excused 0), each at the FIRST cycle of a JOINER CREATE STORM: volume 1's `meta_kv_checkpoint_term_ms` read **11 / 4 ms** (trigger 989 / 996) when the storms began and the trip cycle's own **133 / 127 ms** only after; 199 quiet cycles between the rows had emptied the 64-cycle horizon of N = 4's term before N = 8's storm; the steady state that followed each trip (62–83 grants/s) did NOT trip — the derivation prices the sustained shape and forgets it across a quiet window (§3.9.5.2). The manager served 50–80 `ExtentGrant` / `ReturnExtents` verbs per second through both. **Zero increments on the 3-writer touch fleets and the 32-writer walls fleets** (four legs, 70 writer-legs) — the re-run's quiet-joiner trip (m60, 1,116 ms) and its rewrite trip (m65, 1,206 ms) did NOT recur: the walls rewrite's faces read 50–119 ms on the busiest joiners (m71 119, m75 101 …) with 0 trips — the derivation working where the horizon HOLDS the term. **What stands for §7 item 3**: the manager's cycle term under the joiners' grant / return SERVICE, which starts inside the cycle — and beside it **F-R5**, the reason the service is a storm at all: the manager derives a WIRE joiner's grant from an EWMA it never receives (`grant_extents_for` reads the in-process region's word — `None` → `ewma = 0` → the floor 8), so every joiner grant is ≤ 8 extents whatever its SMO rate, and a joiner's ring at the 512 KiB floor checkpoints ≈ 8×/s under a create storm, retiring and re-claiming at the SMO grain (§3.9.5.2). Every N-writer row set that runs a joiner create storm on the box stops at r1 until it is priced; the row sets that do not (3c, 7) read clean |

**Product findings of this pass (each REPORTED with its evidence; none
fixed here):** **F-B1 stands** as above (2 trips; the kept log
`13e-nw-20260923-030537-scale/scale-r1/daemon-logs/m0.log` at 03:08:22 /
03:10:56 and the row-end snapshots `symscale-1790132759/m*_pn{4,8}{c,1}.json`).
**F-R5 (new — PR 3 × PR 12b, the armed plane; PR 13g is the fix rung):
the manager derives a WIRE joiner's extent grant from an EWMA it never
receives, so §5.3.3's derivation is the FLOOR for every production
joiner** — `grant_extents_for` (`kv/backend.rs:9148`) reads
`set.region(appender_id).smo_ewma_milli`, `None` for a wire joiner →
`ewma = 0` → `grant_extents_derived` = `GRANT_EXTENTS_FLOOR` 8 whatever the
joiner's SMO rate (the joiner folds its EWMA locally and nothing carries
it on `ExtentGrant { appender_id, want }`; at ≈ 8 SMO/s the design's
derivation would answer ≈ 720 extents). The leg's 1,483 grants: 845 × 4,
180 × 3, 147 × 2, 2 × 1 (the reactive `needed.max(SMO_IMAGES_MAX)` ask,
trimmed by the `GRANT_RUNS_MAX` coalescing loop on a fragmented heap) and
103 × 5, 54 × 6, 44 × 7, 108 × 8 (the cadence's PROACTIVE `refill_due()`
ask — the 50 % refill engages, 309 times — answering the derived size,
which is 8); every one served in the USER class over the wire. The second
mechanism: a joiner's ring at the 512 KiB floor (`appender_ring_bytes`
524,288 on both volumes of every joiner, `appender_ring_grows` 0,
`joined_ring_grow_declined` [0, 1]) checkpoints ≈ 8×/s under a 40k-file
storm (m60 +109 in 13 s, `appender_pressure_cycles` +79), returning the
images each barrier RETIRED (`take_returnable()` — `extent_grant_returned`
+193; the unclaimed remainder returns only at a release or the leave) and
re-claiming at the SMO grain (+47 grants) — claim-and-retire churn, ≈ 105
wire verbs per joiner-volume per storm, so the manager serves ≈ 50–80
control entries + barriers per second on ring 0 at N = 8
(`manager_service_ns.execute` +2.36 s over a 13 s storm on volume 1,
3.6 ms per verb) — the service that F-B1's remaining term rides, and
≈ 100 manager verbs per joiner per storm that a right-sized grant would
make ≈ 1. Remedy shapes (PR 13g): carry the joiner's EWMA on the ask or
its page (or let the joiner ask its derived size, screened) — the lead
lever; PR 2's owed drain-then-grow (or the EWMA-sized join) so a storming
joiner's ring leaves the floor; the manager's anticipated term folding
the verb service in flight (item 3). **The gate-3 wall law's N = 8 read carried a
harness-shaped term** (not a defect): an INFERRED ≈ 3.9 s of launch skew
(wall − the longest storm; the leg stamps no per-storm launch), its
hypothesised cause the root's STRIPING at the row's eight `mkdir`s two
seconds before the storms (the 3b shape; plausible, not shown) — 3.53×
read against a storms'-own-concurrency UPPER BOUND of ≤ 4.5× — the
harness item is a per-writer launch stamp + the wall law's clock at the
LAST storm's launch (the per-writer storm walls are in the row).
**Gate 1's two sub-second rows** (mount +0.1 s on B's first mount of a
fresh set; the post-`rw4k` clean unmount +0.5–0.9 s — NEW on this binary,
the re-run's B read FASTER there — UNATTRIBUTED) are stated in §3.9.5.1;
the mount row is UNCONVICTED (a single-order DELTA under the A-B-B-A law; B slower in all three box brackets, 1.13 / 1.25 / 1.33), the unmount
row is a DELTA in both brackets owed with its instrument (§7 item 16). **Harness (fixed on the
branch, placed on the box)**: **H-13E-1** — PR 15's `daemon_pid_for_mnt`
anchor (`squeezefs mount .* <mnt>( |$)`) matched no arm-suffixed binary
(`squeezefs-B-<sha> mount …`), so the first gate-3 fleet create died at
member 0 with the manager mounted (`6e9c602e`: the name up to the next
space, the whole-word mountpoint kept; the teardown's stray sweep the
same; the failed launch kept as `13e-nw-20260923-030202-scale.H1-pidanchor`).

#### 3.9.6 The fourth pass — gate 3 on PR 13g's binary (`230e95dd`) — `perf/sym-box-13g`, 2026-09-24 04:05 → 04:57 UTC (the box left as found): **`sym-scale` N = 1/2/4/8 ran TWICE from zero on fresh fleets — the first row set is the FIRST gate-3 row set ever to complete to its ORACLE on the box (`appender_flush_ceiling_overruns` 0 on the manager and every joiner through all four rows, deleted-stays-deleted 0 / 3,000 ×2, fsck clean); the second (a harness re-run — the row's setup taken out of its clock) read ONE trip on the manager, 1,101 ms — 1 ms past the ceiling — at the N = 4 storm's END under NO verb service; F-R5's laws MET on every joiner in both sets (rings 768 KiB–2.3 MiB, returns ≪ compactions, 1–3 wire grants per joiner per row, reactive 0, the manager's verbs 10× and its service 26× down at N = 8); the launch skew MEASURED for the first time — 9.1 s at N = 8, three 3.0 s `mkdir`s by the fresh joiners into the freshly STRIPED root — and with the setup outside the clock N = 8 reads 4.61× creates (Σ per-writer rates 5.25×, the bound); one new armed-plane finding (F-R6: a joiner's FORGET-driven reclaim prices destroys for the HOLDER's inos through its stale projection — 6,782 / 7,266 withheld per set, defect 18's root-seq loop (defect 34's family) under it) and one log-volume finding (272 k / 275 k `may reply interrupted fuse request` WARNs per set — 21–46 % of the served invalidations, `session.rs:1092`)**

**The counted-run law**: the brief's ONE row set ran from zero on PR 13g's
binary and completed (`13g-nw-20260924-041135-scale`); it measured a
harness term — the N = 8 row's eight `mkdir`s under `/` ran INSIDE the
storms' clock and three of them took 3.0 s each — so the leg was fixed
(the setup before the clock, its walls stamped) and ONE more row set ran
from zero on a fresh fleet (`13g-nw-20260924-044235-scale`). Both are
reported; the second is the wall law's honest read, the first the
oracle's. No third row set (the minimum-count law). Gates 1 / 2 / 3b / 3c
/ 5 / 7 were NOT re-run (13g changes the joiner's supply and the
manager's cadence; the third pass's rows on `b377cbb8` stand for the rest
— they rerun once on PR 14's flip binary).

**Venue (re-verified 04:05 UTC, 2026-09-24):** `squeeze-test`
(`memp-s3ds-aqs-37`), 32-core Xeon, 251 GiB, Rocky 8.10, **kernel
`6.19.14-sqz`** (the sqz series incl. patch 0031), up 2 d 2 h 58, load
0.00, modules loaded = `nvme_tcp nvme_fabrics nvme_core fuse` only (no
Lustre / lnet, the devsub's `nvmet` / `zram` / `null_blk` unloaded as the
third pass left them), docker inactive, lnet failed (inactive), only
`fusectl` mounted, no daemon, no `/run/squeezefs-mwfleet*` / `-devsub-*`
(`/run/squeezefs/` = the box-rows rung's four stale IL sockets), no netns,
0 `pref 40` rules, no `/dev/shm/sqz*`, 245 G free; the fabric's 15
controllers connected as found. Both row sets on the box's own tcp devsub
(nvmet-tcp on `127.0.0.1`, `resv_enable=1`, `lzo-rle` zram,
`SQZ_MWFLEET_OSS_GB=16`, fleet A = `create N=2 --symmetric --writers=7
--token-readers`, `--venue=box`, `REPEATS=1`). **Arm B** = `230e95dd`
(= `dev`'s code tip: PR 13g over the third pass's record; the batch `task
check` GREEN — 407 suites / 5,610 tests) — built by the orchestrator
(`task build:rocky8`, the `release` profile, from a detached checkout at
that sha), staged at `/tmp/grok-justin/box-13g/arms/`, `sha256sum -c` OK
on both sides: **`squeezefs 1.2.4 (230e95dd57da /
230e95dd57da324b03aab0646e00d275799e73d0) built 2026-09-24T03:58:38Z
profile release`**, sha256
`099477dcd1c919cdf492addeb1bcd61cd318187758001ff66318e63e7e49d8b2` (shim
`fd4a02ac8cc88dec…1f1a82b7`), placed as
`/scratch/tmp/sym-box/squeezefs-B-230e95dd` and as `/scratch/tmp/squeezefs`
(the reset script's client binary; the earlier B arms kept beside it). No
A arm this pass (gate 3's A arm — the shipped MW posture at 0.07× — is
unchanged by 13g and cited from §3.9.4.2). **Instrument:** the driver
`2026-09-21-sym-box-brackets.sh` + `run_mw_matrix.sh` at THIS branch's
revision — `76028d5f` (every storm's LAUNCH and END stamped; F-R5's
per-writer faces; F-B1's projection / lateness / unit faces) for the
first set, `6108e8c1` (the row's directories created BEFORE the clock
with their walls stamped; the Σ-of-per-writer-rates bound; the hygiene
faces; an end-of-leg snapshot; PR 13e's post-leave census at the leg's
end) for the second (its end-of-leg snapshot's label fixed after the
run, `cea35691`). **Neither the end-of-leg FACES nor the post-leave
CENSUS has a reading on `230e95dd`** (review round 1, Issue 5): set 1
predates `6108e8c1`, and set 2's leg died on the `pend1` label after its
table, before both (and one line before the must-stay-0 die it would have
taken) — the end-of-leg SNAPSHOTS themselves exist under the first
build's names, `m*_pend.json` (set 2), and are cited where they are
read; the faces and the census (0 exempted / findings 0 after every
member's leave) are the flip binary's gate-3 row's. The laptop ran
nothing of this rung.

**The two row sets** (the row's clock law, stated once: the multiple is
measured over the N STORMS' concurrent window — the design's "aggregate
create/s scale with N" — and the row's SETUP, N creates into the shared
root with the 3b flip it triggers and the fresh joiners' 3 s cliffs, is
stamped and stated beside it, never inside it; a job launching N fresh
writers into one root pays it once):

| set / N | **create/s · ×N=1** | `C/CPU-S` (×) | **ingest MiB/s · ×** | `MGR_LOAD` / `MGR_CPU` / handovers / ships / rpcs | ingest amplification (`/proc/diskstats`, 2 data namespaces): device write ÷ user · `wareq-sz` · device read ÷ user | per-writer storms (s) · the launch term | verdict (the leg's) |
|---|---|---|---|---|---|---|---|
| **1** (`041135`, the mkdirs inside the clock) · 1 | **4,997 · 1.00×** | 4,216 | 1,506 · 1.00× | 3 % / 115 % / 0 / 0 / 0 | 1.228 · 1,648 KiB · 0.228 | 8.0 · skew 0.005 s | MET |
| 1 · 2 | **9,354 · 1.87×** | 4,027 (0.96×) | 2,632 · 1.75× | 1 % / 112 % / 0 / 1 / 0 | 1.147 · 1,338 KiB · 0.147 | 7.8–8.5 · 0.012 s | MET |
| 1 · 4 | **16,620 · 3.33×** | 3,570 (0.85×) | 4,495 · 2.98× | 0 % / 113 % / 0 / 3 / 0 | 1.175 · 1,346 KiB · 0.175 | 8.4–9.6 · 0.025 s | MET |
| 1 · 8 | **17,620 · 3.53×** | 3,061 (0.73×) | **7,294 · 4.84×** | 0 % / 70 % / 0 / 4 / 0 | 1.110 · 1,255 KiB · 0.110 | 9.0–13.6 (2,937–4,435 c/s each; Σ 28,424 = **5.69×**, the upper bound) · **skew 9.135 s** — m0, m60–m63 launched within 36 ms, m64 / m65 / m66 at +3.07 / +6.09 / +9.13 s (each `mkdir -p` 3.03 s) | MISS on both wall laws (creates 3.53×, ingest 4.84× vs ≥ 5.6×); **the must-stay-0 set held; the oracle reached: deleted-stays-deleted 0 / 3,000 at the manager, 0 / 3,000 at the remounted joiner, fsck clean** |
| **2** (`044235`, the mkdirs BEFORE the clock) · 1 | **4,981 · 1.00×** | 4,244 | 2,215 · 1.00× | 3 % / 112 % / 0 / 0 / 0 | 1.005 · 1,254 KiB · 0.005 | 8.0 · setup 0.007 s, skew 0.001 s | MET |
| 2 · 2 | **9,270 · 1.86×** | 4,004 (0.94×) | 2,637 · 1.19× | 1 % / 112 % / 0 / 1 / 0 | 1.128 · 1,433 KiB · 0.128 | 7.8–8.6 · 0.015 / 0.003 s | the create law MET; ingest MISS against a 2,215 MiB/s base (below) |
| 2 · 4 | **16,283 · 3.27×** | 3,488 (0.82×) | 4,105 · 1.85× | 0 % / 113 % / 0 / 3 / 0 | 1.107 · 1,284 KiB · 0.107 | 9.0–9.8 · 0.027 / 0.006 s | the create law MET; **`MISS(must-stay-0: m0 appender_flush_ceiling_overruns +1)`** — 1,101 ms at 04:45:22Z |
| 2 · 8 | **22,941 · 4.61×** | 2,820 (0.66×) | **7,594 · 3.43×** | 0 % / 104 % / 0 / 4 / 0 | 1.090 · 1,263 KiB · 0.090 | 10.8–13.9 (2,871–3,697 c/s each; Σ 26,172 = **5.25×**, the bound) · **setup 9.120 s OUTSIDE the clock** (m64 3.021 / m65 3.018 / m66 3.014 s, m63 0.039 s, the other five 4–5 ms), skew inside the clock 0.012 s | MISS on both wall laws (creates 4.61× vs ≥ 5.6×; ingest 3.43×); **set 2's `MGR_CPU` (104 %) and `C/CPU-S` (0.66×) still carry the setup's CPU** — the start snapshots and `cpu0` were sampled BEFORE the setup while `t0` followed it (review round 1, Issue 4; moved to `t0` on the branch after the run), so the manager's flip + the fresh joiners' waits sit in their numerators over the storms' wall alone (set 1's 70 % / 0.73× are the same two terms on the old clock) |

**The wall law, re-read with the launch MEASURED.** The third pass
INFERRED ≈ 3.9 s of skew at N = 8; the leg now stamps every storm's launch
and end. Set 1 measured **9.135 s**: the five writers already mounted
launched within 36 ms of the row's first `mkdir`, and the three joiners
mounted FOR the row (m64 / m65 / m66 — the leg keeps exactly N appenders
live) launched at +3.07 / +6.09 / +9.13 s because each one's `mkdir -p
/scale-…-w6x` into `/` took **3.03 s** — right after the ROOT STRIPED at the
fifth creator's mkdir (m0.log 04:16:02Z `directory 1 STRIPED into 64
stripes (3 supplied by creators [1, 2, 3], 61 minted by the holder)`; at
N = 4 the two fresh joiners' mkdirs took 6 ms with the root unstriped).
Set 2, with the same eight mkdirs stamped BEFORE the clock, reproduced it
to the millisecond: m64 3.021 s, m65 3.018 s, m66 3.014 s (m63's — the
fifth, the flip's own — 39 ms; the other five 4–5 ms) — **a fresh joiner's
first create into a STRIPED root costs ≈ 3.0 s on this binary** (three
≈ 1 s waits by shape; the standing hypothesis is the lazily dialed
per-holder token planes' first rounds — `TokenReaderPlane::
await_channel_fresh` waits the standing recall poll's first round, which
the holder parks for its whole `DELEG_PARK_DEFAULT_MS` = 1,000 ms window
when nothing is recalled; the manager admitted three new sessions from
m64 at 04:16:04 / :05 / :05, inside its mkdir; `xv_cross_owner_phase_ns
.total` for the mkdir's own intent 1.7 ms — the 3 s is BEFORE the
intent). An INFO log cannot split the three seconds; the instrument is
`SQUEEZEFS_OP_PROFILE=1` on a fresh joiner's first create into a striped
root (PR 14 / 15's item — a latency cliff on the flip's default path,
paid once per fresh writer per striped directory's holder set, not a
throughput term). With the setup outside the clock the storms' own
multiple at N = 8 is **4.61×** (the leg's law; per-writer storms 10.8–13.9
s = 2,871–3,697 c/s each, launch skew 12 ms) against an upper bound of
5.25× (Σ per-writer rates; exact only at zero skew) — **the wall law
MISSES ≥ 5.6× on this venue by the storms' own concurrency, not by the
launch** (§3.9.3's co-located term: eight daemons and eight 16-thread
clients on 32 cores; `C/CPU-S` 4,244 → 2,820 = 0.66× — set 2's CPU faces
include the setup's CPU, Issue 4 above; the flip binary's row reads them
clean). The per-NODE law
stays PR 15's. **The ingest law's N = 1 base is a sub-second
measurement**: one writer's 1 GiB `dd conv=fsync` lands in 0.46–0.77 s
and read 1,327 / 1,506 / 2,215 MiB/s across the three box row sets on two
binaries, while the N = 8 aggregate is STABLE at 7,292 / 7,294 / 7,594
MiB/s — the two-volume zram bus's ceiling — so the ingest multiple
(5.50× / 4.84× / 3.43×) is the base's noise, not a scaling reading; the
leg's `--ingest-mb` needs ≥ 4 GiB per writer for a base the law can
divide by (a harness item, stated; the N = 8 absolute is the row's
number). Engagement: handovers 0, ships 0 / 1 / 3 / 4 (≤ N), `dlm_rpcs` 0
on every writer, `appenders_known` = N at every row, `manager_load_pct`
≤ 3 %, `MGR_CPU` 70–115 %. Amplification: the ingest's device writes
1.005–1.228× user at `wareq-sz` 1.2–1.6 MiB with device reads 0.005–
0.228× user (the N = 1 row's short window scatters it; N = 8 reads
1.090–1.110× at 1.25 MiB, the third pass's 1.129).

##### 3.9.6.1 F-B1 — the box's VERDICT on PR 13g's projection: **the class the third pass read is GONE; the tripwire is NOT 0 on this binary**

**Set 1: 0 increments on every writer at every N — the first gate-3 row
set to run its four rows and reach its oracle on the box.** The faces on
the manager (its two volumes): term [6, 4] ms at N = 1's end; **[24, 6] /
trigger [976, 994] at N = 2's START and [26, 7] / [974, 993] at N = 4's
START** — the third pass's class (an almost-empty horizon on volume 1
when a joiner storm begins) exercised twice and NOT tripped; **[145,
183] / [855, 817] at N = 8's START** (the between-rows `rm -rf` + join
window put a real term in the window — 148 manager cycles between N = 4's
end and N = 8's start); [14, 27] at the end; `meta_kv_checkpoint_late_max_
ms` 25–49 on the manager and 7–48 on the joiners (inside the 100 ms
margin); the projection read 0–12 ms at every snapshot instant (a quiet
tick's dirty count — the snapshots never catch a storm cycle); node units
15–235 µs, image units 0.4–4.9 ms; `excused_ns` 0 everywhere; the
joiners' end-of-row terms 4–129 ms with 0 trips (m60 [129, 12], m61 [71,
0] at N = 8). Under the manager's volume 1 across the N = 8 create: ONE
`ExtentGrant`-class verb per second at most — the grant burst of §3.9.5.2
does not exist on this binary (F-R5 below).

**Set 2: +1 on the MANAGER's second metadata volume — `04:45:22Z WARN …
meta volume /dev/nvme31n1: flush ceiling OVERRUN — appender region(s)
[(0, 1101)] … exceeded the 1100 ms landing ceiling with every structural
hold's capped overlap excluded`** — **1 ms past**, at the END of the N = 4
create storm (the four storms' `end_ts` 04:45:21.16 / 21.22 / 21.35 /
21.996 — the manager's own last), 0 excused, no recovery / service
extension. **The snapshots' PLACEMENT against the trip (review round 1,
Issue 1 — the third pass's Issue 2 reversed):** the create-end snapshot
`m0_pc41.json` ≡ `m0_pn4c.json` was taken right after the last `wait`
(≈ 04:45:22.0x) and reads **`appender_flush_ceiling_overruns` [0, 0]** at
checkpoint 250 — it PRECEDED the trip's covering barrier by less than a
second; the row-end snapshot `m0_pn41.json` (checkpoint 256, after the
ingest) reads [0, 1]. So the faces split as: at the row's START volume 1's
term **6 ms** / trigger **994** / projection 0 (the quiet horizon — the
third pass's class); at the create's end, BEFORE the trip, term **52** /
trigger **916** / **projection 84 ms** / lateness 15 (the projection
ENGAGED — 84 ms off the tick's dirty count against node / image units of
92.9 µs / 2.04 ms; the 52 ms term = the storm's earlier cycles'
pre-barrier maximum); and the trip cycle's OWN words entered the horizon
AFTERWARDS — by the row's end volume 1's term read **151** and `late_max`
**35**. Volume 1 served **one manager verb across the whole row**
(`manager_verbs` 517 → 518; volume 0's 26 verbs = the N = 4 joiners'
sizing asks: 68 / 69 / 107 / 141 extents in single runs at 04:45:12–13,
two `GrowRing` carves) — **no verb service, no grant burst: the third
pass's mechanism is absent at this trip.** The covering barrier is not a
term the faces support: the manager's own barrier faces are ms-class
(`fsync_phase_ns.meta_barrier` mean 1.27–1.86 ms, `uring_fs_write_phase_
ns.device` mean 15 µs) — no ≈ 40 ms exists in any reading. **The
decomposition the faces DO support: the decision at ≈ 916 ms (the 84 ms
projection engaged) + a lateness of ≤ 35 ms (the `late_max` that appeared
with the trip) + a pre-barrier wall of ≈ 151 ms (the term that appeared
with it) + a barrier of ≈ 0–2 ms ≈ 1,101 ms — the LIVE projection
UNDER-PRICED the storm-end cycle's own wall by ≈ 65 ms (84 → ≈ 150 ms: the
dirt the storm's last second adds AFTER the tick decides, and/or the node
unit under-measuring at the tail) and the decision's lateness reached
35 ms.** The trip cycle's pre-barrier wall is bounded, not read: in
[52, 151] — 151 (the storm's END cycle, the last second's dirt of a
40k-create storm, is the natural owner of a ≈ 150 ms wall; the row's
later ingest publishes are a few nodes at 2–4 ms image units) against 52
(the reading that leaves the residue to the barrier — which no face
measures above 2 ms); the discriminating instrument is a per-cycle tape
of the decision instant, the pre-barrier wall and the barrier wall,
which does not exist and which §7 item 3 names. **The verdict: PR 13g's
derivation prices the STORM (every onset in both sets, every joiner's
storm, the manager's own 40k-file storms, the walls' 50–119 ms terms of
the third pass — 0 trips at every one); what it under-prices is the
storm's END cycle — the projection is read at the tick's decision and
the flush pass writes what has accumulated by the time it runs — and the
100 ms margin absorbed the growth plus the lateness by ≈ 0–10 ms at this
venue.** §7 item 3's next piece is that residue: the projection's growth
between the decision and the flush (the dirt admitted after the tick
reads its count — a projection off the admission rate, or a re-read at
the pass), and the lateness term (35 ms of the 100 ms margin) — never a
widened constant.
The tripwire's rate on this binary at this venue: 1 increment in 8 rows
across two row sets (16 writer-rows at the manager, 44 joiner-rows), 1 ms
past. Set 2's leg died on the `pend1` label one line before the must-stay-0 die, after its table and before its
oracle (as the leg is built); nothing else moved on the set.

##### 3.9.6.2 F-R5 — the box's VERDICT on PR 13g's supply: **FIXED — every law MET on every joiner in both sets**

The faces per joiner at the leg's end (set 1; set 2 in parentheses where
it differs), the deltas over the N = 8 CREATE phase:

| joiner | `appender_ring_bytes` KiB (grows) | Δ`joined_wire_extent_grants` · Δreactive | Δ`extent_grant_returned` vs Δcompactions | Δ`appender_pressure_cycles` vs Δcheckpoints | pool at the end claimed / returned / unclaimed | the new hygiene set |
|---|---|---|---|---|---|---|
| m60 | [1,280, 2,304] ([1, 2]) — set 2 [1,792, 1,792] ([2, 2]) | [1, 1] · 0 | [38, 0] vs 180 (set 2 [0, 0] vs 118) | [0, 6] vs 42 | [64, 77] / [240, 795] / [120, 106] | 0 |
| m61 | [2,304, 768] ([2, 0]) — set 2 [1,280, 2,304] | [1, 0] · 0 | [13, 0] vs 184 | [8, 0] vs 44 | [78, 0] / [339, 0] / [115, 72] | 0 |
| m62 | [2,048, 1,536] ([2, 2]) — set 2 [2,048, 768] | [1, 0] · 0 | [17, 0] vs 128 | [11, 0] vs 40 | [79, 64] / [261, 144] / [111, 72] | 0 |
| m63 | [1,280, 1,280] ([1, 1]) — set 2 [768, 1,280] | [0, 2] · 0 | [0, 2] vs 119 | [1, 25] vs 53 | [66, 72] / [13, 2] / [81, 110] | 0 |
| m64 | [768, 1,280] ([0, 1]) — set 2 [1,024, 1,280] ([1, 1], declined [2, 0]) | [0, 3] · 0 | [0, 5] vs 118 | [1, 25] vs 52 | [64, 83] / [0, 5] / [8, 107] | 0 |
| m65 | [768, 1,792] ([0, 2]) | [0, 2] · 0 | [0, 9] vs 119 | [3, 25] vs 56 | [65, 72] / [0, 9] / [95, 105] | 0 |
| m66 | [768, 1,280] ([0, 1], declined [0, 1]) | [0, 2] · 0 | [0, 5] vs 118 | [3, 25] vs 59 | [64, 72] / [0, 5] / [8, 103] | 0 |

**Law 1 — the ring grows**: every joiner above the 512 KiB floor on both
volumes at the leg's end in both sets (768 KiB is the join's DERIVED ring
— `resolve_sym_ring_bytes_hinted` — not the floor; the fresh joiners
m64–m66 grow it once to 1.25 MiB inside their first storm, the joiners
with a hint start at 1.0–2.3 MiB); **29 / 28 `GrowRing` carves per leg
with 1 / 7 short-run DECLINES** (`ring grows by one segment` / `GrowRing
for appender … declined` lines in `m0.log`): `joined_ring_grow_declined` 0
everywhere but m66 [0, 1] (set 1) and m64 — [2, 0] at set 2's `pn81`, **[6,
1] at its leg's END** (`m64_pend.json`; the manager's `appender_grow_ring_
short_declines` [0, 1] at set 1's `pn81`, **[6, 1] at set 2's end**,
`m0_pend.json`): m64's volume-0 ring stayed at 1,048,576 B through six
consecutive `GrowRing for appender 5 declined — the heap's longest
adjacent run is 1 extents against a floor of 2 (half the ask of 3)`
answers at 04:47:42–47Z (the N = 8 between-rows window) and one more on
volume 1 at 04:50:57Z, set 1's one at 04:16:19Z — counted, no table slot
spent, as built. **The reading behind the counts is a WATCH ITEM for PR
14 (review round 1, Issue 6)**: the 1 GiB metadata heap FRAGMENTS as a
leg proceeds (declines 0 → 1 → 7 across the two legs' rows), and the
drain-then-grow's adjacent-run requirement then PINS a ring at its size
— F-R5's verdict stands (every ring above the floor, the verbs 10× down),
but a longer storm or a smaller volume reaches the pinned-ring posture
sooner; the remedy is the heap's (a coalescing return / carve order, or a
segment carved from non-adjacent extents), not the cadence's. **Law 2 —
returns bounded**: `extent_grant_
returned` +0…+38 over a joiner's N = 8 storm against 118–184 compactions
(the third pass: +193 ≈ the compactions — claim-and-retire churn); the
surplus above the pool's target returns at the quiet cadences between the
rows (m60's volume 1: 501 extents in 16 `ReturnExtents` between N = 2 and
N = 4, 795 by N = 8 — the shrink toward `joined_pool_target`), never inside
a storm. **Law 3 — the ask is the joiner's**: `joined_wire_extent_grants`
1–3 per joiner per row (the third pass: +47 per joiner per storm), every
one a derived-size carve — 58 extents per grant at N = 8 (`extent grant to
appender 4: 66 / 46 / 88 extent(s)`; the third pass's 845 × 4 … 108 × 8),
`joined_wire_reactive_grants` 0 on every joiner in both sets (the third
pass's reactive one-SMO asks were 93–96 per joiner in the pin's RED).
**Law 4 — the cadence**: `appender_pressure_cycles` +0…+11 per storm on
the sized rings, +25 on the fresh joiners' 768 KiB volume-1 rings (one
growth inside the storm), against 40–63 checkpoints per 10–13 s storm
(4–6/s; the third pass: +79 pressure of +109 checkpoints at ≈ 8/s).
**The manager's economy (the verdict's number)**: over the N = 8 row
Δ`manager_verbs` [67, 21] = **88** (set 2: 81) against the third pass's
892, Δ`extent_grants` 15 carving 877 extents (13 / 730) against 449 grants
of ≤ 8, Δ`extent_returns` 13 (9) against 387, Δ`manager_service_ns.execute`
[0.072, 0.042] = **0.114 s** (0.114 s) against 2.93 s — **10× fewer verbs,
26× less service**; the whole leg 276 / 244 verbs, 44 / 39 grants,
0.49 s of execute (the third pass's leg: ≈ 2,700 verbs, 976 grants). **The
must-stay-0 / hygiene set of PR 13g read 0 on every writer in both sets**:
`extent_return_run_cap_refusals`, `appender_stale_page_words_dropped`,
`appender_pool_restored_extents`, `appender_pending_segments_returned`,
`appender_join_residue_returned`, `joined_control_refusals`,
`joined_wire_failures`, `joined_wire_words_rejected`, `manager_verb_
refusals`, `manager_verb_replays`, `manager_verb_rejected`, `dlm_rpcs`,
`invariant_tripwires`, the PR 13e tripwires. **The closure holds
EXACTLY, set-wide**: the manager's `extent_grant_extents` counts every
carve including the seven first-incarnation join grants that LEFT with
the N = 1 setup (the leg keeps N appenders live — every joiner leaves and
rejoins; 7 × 72 = 504 per volume), so `[2,375, 2,579] − 504 = [1,871,
2,075] ≡ Σ over the live joiners of claimed + returned + unclaimed =
[1,871, 2,075]` at set 1's end (set 2: `[1,944, 2,579] − 504 = [1,440,
2,075] ≡ [1,440, 2,075]`) — a joiner's own `granted` is not a published
`.stats` face (the `AppenderStats` word exists; stated for PR 14's stats
sweep), so the closure is read against the manager. The joiners' rejoin
faces — `appender_pool_restored_extents`, `appender_pending_segments_
returned`, `appender_join_residue_returned`, `appender_stale_page_words_
dropped` — 0 on every one of the 14 rejoins the two legs ran (a clean
lifecycle; the `appender_hint` sizing visible as the 1.0–2.3 MiB rings
the rejoined m60–m62 start their later rows with).

##### 3.9.6.3 Findings of this pass (each REPORTED with its evidence; none fixed here)

**F-R6 (new — PR 12b's reclaim path × PR 5's token planes; the armed
plane): a joined writer's FORGET-driven reclaim prices DESTROYS for inos
in slots it does NOT lease, reading its own stale PROJECTION of the
holder's trees.** Both sets: m60's log carries **6,782 / 7,266 `WARN
squeezefs::routing reclaim of ino …: destroy WITHHELD — the entry
carrying its 0 reference release(s) did not commit (pricing the destroy
failed: … corrupt KV encoding …)`** lines (`reclaim_destroy_refused_
release_failed` 0 → 121 / 742), every one for an ino of the MANAGER's
rotor (ino 41025607 → routing slot 69, 59899929 → 23, 59834453 → 83 —
forest 70 / 24 / 84, the even forest slots the manager mints in; never
m60's own), in the between-rows windows where the MANAGER removes its
own 40k-file tree. Three faces of one stale projection: (a) 04:14:52Z
(set 1) node `0x2180000` read as ZEROS (`bad node magic 0x00000000`) — the
extent the manager had carved into appender 3's ring at 04:14:17Z
(`m0.log:20134 GrowRing — appender 3's ring grows … at 0x2180000`, zeroed by
the carve) after its own leaf there retired; (b) 04:17:29–48Z `tree 0
(slot Some(24) …, a PROJECTION here): traversal retry budget exhausted
descending to level 0 (routing loop — SMO protocol bug) restarts
[root-retired, root-seq, routing-hole, child-retired, child-seq] = [0,
256, 0, 0, 0]` on 60+ of the manager's slots — 6,205 / 6,088 such lines:
**defect 18's `root-seq` class on a recycled root, defect 34's family —
PR 13b's "9/10" (tree 0's `[0, 0, 0, 0, 256]` child-seq) in production shape on SLOT trees** (the refresh finding no newer ledger seq); (c) 27 / 45
CONSECUTIVE extents `0x1a40000 … 0x20c0000` `screened FOREIGN by rule 4
(appender 4, slot generation 1; the lessee's current generation is 1) —
the log ends before it` — the manager's former leaves re-granted to m63
(appender 4), which wrote its frames there. The mechanism: the leg's
acked-writes check reads the manager's tree THROUGH m60 (its kernel
instantiates 40k of the manager's inodes), the manager's `rm -rf` recalls
m60's tokens → m60's recall sink invalidates + PRUNES → the kernel FORGETs
→ **m60's reclaim prices a destroy for a foreign-slot ino by reading its
projection** — pointers into extents the holder has since retired, freed
and re-granted. Nothing was destroyed — and the WITHHOLD is NOT the
belt that proves it (review round 1, Issue 3): the withhold is the
pricing read FAILING (`destroy_entry_bytes` walking an undecodable
projection — `bad node magic`, the rule-4 screen, the 256-restart loop),
an accident of staleness; a DECODABLE stale projection (a fresher one, or
a record the holder had unlinked but not yet destroyed) prices the
destroy and the tx reaches m60's commit DOOR, where PR 4 / 12b's
foreign-slot refusal (`SlotBusy` → `slot_door_refusals`) is the
structural belt. That belt is what proves the claim for this run:
**`slot_door_refusals` [0, 0] on m60 at every snapshot in both sets,
including set 2's end-of-leg `m60_pend.json`** — no priced foreign
destroy reached the door — beside the refuse arm's own semantics
(`refuse_reclaim` → `finish_reclaim(plan, ReclaimFreeGate::Nothing,
false)`: no tx, nothing freed), the holder's own reclaim being the
lifecycle's law (the manager's log carries no corrupt read), and set 1's
every acked-writes / deleted-stays-deleted / fsck arm passing; the cost is
a CPU + log storm on the joiner (6,205 ×
256 restarts, 6,782 WARN lines per window) on a path a token client must
never take — a non-holder prices no destroy for an ino it merely cached
— with defect 18's root-seq loop (defect 34's family) reachable under it. Fix shape (PR 14), the
sites named: the gate belongs at `SqueezefsFilesystem::queue_reclaim_
inode` / `reclaim_orphaned_batch` (`src/fuse_client.rs:10544` — every
non-open FORGET enqueues a reclaim — / `:25489` — the `getattr` nlink-0
gate, no slot check): a FORGET of an ino whose slot this mount does not
lease enqueues no reclaim (PR 13f's sync `slot_is_foreign`,
`src/meta_backend/record_ship.rs:321`, is the predicate; the holder's
reclaim owns the lifecycle, a token client drops its cache entry and
nothing else); and `KvMetaBackend::destroy_entry_bytes` (`kv/backend.rs:
24087` — `range_kind(TREE_XATTRS, …)` on the LOCAL KV) is a second face,
a read of a foreign slot's tree that never takes the writer's divert.
Defect 34's loop stays on its own item (its recipe here: a projection
whose root the holder recycled between two of the joiner's ledger
polls). **The row's oracle cannot see this class**:
the online census scopes live lessees' slots out, deleted-stays-deleted
judges names, and the two gauges that moved sit outside the must-stay-0
set and outside every per-row snapshot pair — the leg now prints them
per row and once more after the last removals and ends with the
post-leave census (`6108e8c1` — landed AFTER set 1 and never reached in
set 2, so neither has a reading on this binary: the flip binary's row's).
Evidence: `13g-nw-20260924-0{41135,44235}-scale/scale-r1/daemon-logs/
{m0,m60,m62,m63}.log`, the ino → slot arithmetic, `m60_pn{41,80}.json`
(set 1: `reclaim_destroy_refused_release_failed` 0 → 121) and set 2's
end-of-leg snapshot `m60_pend.json` (the first build's label: withheld
**7,266**, `meta_kv_projection_root_refreshes` **79**,
`foreign_frames_screened` **45**, `slot_door_refusals` [0, 0]).

**A log-volume finding (PR 13b's served-mutation kernel hook)**: **272,071
/ 275,372 `WARN fuse3::raw::session may reply interrupted fuse request,
ignore this error No such file or directory (os error 2)`** per set
across the daemon logs (m0 222,423 / 223,831; m60 49,177 / 51,481; the
other joiners 77–81 / 9–12 each). The site is `crates/fuse3/src/raw/
session.rs:1092` (`reply_fuse`): the hook's detached notify frames
(`Notify::invalid_inode_detached` / `prune_detached` → `ReplyTx::
send_detached` → `write_vectored`) travel the reply channel, so a kernel
`-ENOENT` on a `FUSE_NOTIFY_INVAL_INODE` / `FUSE_NOTIFY_PRUNE` for an inode
it does not hold (the expected "not cached" word) lands on the reply
path's "interrupted request" WARN. The WARNs are a SUBSET of the
notifies, not a multiple: m0 222,423 against `meta_ship.served_mutation_
{invals,prunes}` 243,806 + 243,806 = 487,612 (**46 %**), m60 49,177
against 114,716 × 2 = 229,432 (**21 %**) at set 1's `pn81` (set 2: 46 % /
22 %) — the fraction of the invalidations the kernel had already dropped
the inode for. A hygiene item (PR 14): the notify's `ENOENT` is a counted
outcome, never a WARN per call.

**The fresh joiner's 3.0 s `mkdir` into a striped root** (above — the
launch term, reproduced ×3 in both sets; an `OP_PROFILE` tape is its
instrument; PR 14 / 15). **The ingest law's sub-second N = 1 base**
(above — `--ingest-mb` ≥ 4,096 on the box; a harness item). **Two
`.stats` faces stated for PR 14's sweep**: a joiner's `granted`
(`AppenderStats::grant_granted`) is not exported, so the pool closure is
read set-wide; the writer's per-holder read planes report on no `dlm_
token_reader_*` face (the `-o ro` reader's family), so a joiner's divert
cost is visible only as `dlm_token_records_bytes` and `meta_ship.dlm_
token_cache_grants`. **Harness (fixed on the branch, placed on the
box)**: `76028d5f` (the launch / end stamps, F-R5's faces, F-B1's
projection / lateness / unit faces), `6108e8c1` (the setup before the
clock — its walls stamped; the "wall from the LAST launch" reading of the
first commit REPLACED by the Σ-of-per-writer-rates bound, since most of
a staggered row's work runs before that instant; the hygiene faces; the
end-of-leg snapshot; the post-leave census), `cea35691` (the end-of-leg
snapshot's label — set 2's leg died on the missing `m0_pend1.json` after
its table; it would have died on the must-stay-0 set one line later
either way, so nothing of set 2 is lost to it), and after the review the
start snapshots + `cpu0` moved to `t0` with the clock's law logged once
(Issue 4). Artifacts:
`/scratch/tmp/sym-box/13g-nw-20260924-0{41135,44235}-scale/` (+ `.log`s;
per set `scale-r1/symscale-*/` — the table, `symscale-faces.txt`,
`launch-n*.tsv`, `create-n*-w*.txt` with `launch_ts= end_ts=`,
`disk_pin*.tsv`, `m*_pn*.json`, `fsck-sym-scale.out` (set 1) — and
`scale-r1/daemon-logs/m*.log`), pulled to
`/tmp/grok-justin/box-13g/nw/` with `REDUCED.txt`.

### 3.10 The cloud row (PR 15 Phase B) — run 1, 2026-09-24: **LAUNCHED with the owner's expressed approval, ASSEMBLED on 8 real nodes after one failed attempt, FAILED on its FIRST row (gate 2 `sym-tarx`, arm sym-1 — the joined writer's `mkdir` under the root answered `EINVAL`) with THREE product findings, TORN DOWN at 26.5 min ≈ $5.2; the row is INCOMPLETE and UNMEASURED — no per-node number exists, the pulled evidence is LOST with the dev machine, and the re-run needs a NEW expressed owner approval after PR 13i lands**

**The approved shape (the owner's approval recorded in the run log, `9fa066b0`, 2026-09-24 14:45 wall — "Owner APPROVED the PR 15 cloud launch: S2 + 8 oss (17 × i4i.2xlarge on-demand, ≈ $15–17, guard 4 h) — launching on `aad50a1f`'s artifact"):** `PRESET=mw SYMMETRIC=1 N_MDS=1 N_OSS=8 N_CLIENT=8 MAX_CLUSTER_HOURS=4` — **17 × i4i.2xlarge, us-east-1a, on-demand**, a cluster placement group: 1 metadata storage node, 8 data storage nodes (one 1,875 GB instance-store namespace each over nvmet-tcp, `resv_enable=1`), 8 client nodes = **one symmetric writer per node** (the MANAGER on `client0`, a JOINED writer on `client1..client7` through the join ladder over the real wire, a `--read-only` token reader on `client0`), the set formatted `--symmetric` CACHE-LESS. Cluster `sqzbench-20260924-150215`. **The owner rule stated 2026-09-24 21:20 (wall, EDT — after this run and after the reinstall; the resume note's "Laws added this session" and the run log's 20:55 row):** nothing runs on AWS until PR 13i has landed, and any later launch needs the owner's expressed permission AGAIN for that specific run — the AGENTS §Benchmarks mandate restated with the landing as its precondition; the S2 + 8 oss approval is SPENT; this branch touched `aws` only under `--dry-run`.

**Known vs inferred — the ledger this section is written against.** The run's pulled evidence is LOST (below), so three classes of statement are kept apart here and in every place that repeats them: **(S) SOURCED** — in a surviving source: the resume note `~/sym-run-state/RESUME-2026-09-24-omarchy.md` (its "The cloud row's findings" bullet is the agent's contemporaneous summary of the pulled `.stats` + logs, written before the loss) and the run log's 15:55 and 20:55 rows (`b88bfa54`, `b87d3f57`); **(D) DERIVED** — what the rig's own scripts (the base tree's `tests/cloud_bench_cluster.sh`) or the code make necessarily true of a run that reached the stated point; **(R) RECALLED** — the agent's recollection, in no surviving source, UNVERIFIABLE. The sourced set is exactly: the cluster id; the shape and the owner's approval line; the two instants `19:02:14` / `19:28:44` UTC; ≈ $5.2; "nothing billing (verified ×3)"; "attempt 1 failed on the cloned `/etc/machine-id` (= the node token)"; attempt 2's `appenders_known 8`, `membership_writers 7`, "device registrants 8/8"; row 1's identity (gate 2 `sym-tarx`, arm `sym-1`), the failing op (`mkdir -p <mount>/s8a-sym-1` under the unstriped root → `EINVAL`), the daemon line, the four gauges + "one ask per cadence tick, every one a verbatim replay"; the three findings with their code sites; the four rig fixes as a list; the lost evidence directory's path and the lost branch's sha. **Everything else below is (D) or (R) and says so.**

**Timeline (UTC):**

| instant | event | class |
|---|---|---|
| 19:02:14 | `launch` — 17 instances (the deadline guard the rig arms at `MAX_CLUSTER_HOURS` = +4 h) | S (the instant, the count); D (the guard) |
| — | `deploy` — the `aad50a1f` `release` artifact (`task build:ubuntu2604`) sha256-verified on every node (the rig's deploy asserts it). The apt-hygiene rig fix (`deploy` drains + masks `unattended-upgrades`, the timers off) is in the resume note's list of the four fixes; **WHAT the unattended apt did during the run — the CPU it took, whether anything restarted, what it interrupted — is in no surviving source: recalled, unverifiable, and NOT stated here** | D (the verify); S (the fix's existence) |
| — | `assemble-sym` **attempt 1 — FAILED "on the cloned `/etc/machine-id` (= the node token)"** (S, verbatim). That the failure was at the MOUNTS (the rig's step 7/8 as it then stood — 8/9 since this branch) is D: the machine-id is read by nothing the rig runs before the daemons, and the identity equality (same file → the same node token, `src/writer_scope.rs`; the same mount path → the same slot) is D. **Where in the join ladder it failed is NOT known** (the manager's `JoinAppender` screen, the joiner's `symmetric_join_target` probe, the class PR 14's cloned-identity door names — candidates, no evidence). → fixed on this branch: a new step 3/9 asserts `/etc/machine-id` DISTINCT and regenerates a clone before any identity-bearing step | S + D |
| — | `assemble-sym` **attempt 2 — "assembled 8 real nodes (`appenders_known 8`, `membership_writers 7`, device registrants 8/8)"** (S, verbatim). That the clones were regenerated LIVE between the attempts is D (the 15:55 row: "assembled … after the rig regenerated the baked AMI's cloned `/etc/machine-id`"). That attempt 2's `format` met attempt 1's superblock is D (the fabric script formats on every assemble; `format_preflight` refuses a formatted volume without `--force`) → fixed on this branch: `format --force` (the live-client refusal kept) | S + D |
| — | `bench-sym` row 1 — **gate 2 `sym-tarx`, arm `sym-1`: FAILED** — the joined writer m60 (`client1` — D, the driver's node table) `mkdir -p <mount>/s8a-sym-1` under the unstriped root → **`EINVAL`** (S). That the per-node `.stats` and daemon logs were pulled before the teardown is S (the 20:55 row names the pulled directory) | S |
| 19:28:44 | `teardown` — "torn down … nothing billing (verified ×3)" (S). The three verifications are the rig's own teardown shape (the tag-scoped sweep and its re-checks) — D, the methods not recorded | S + D |

**No other instant of the run survives** — the deploy's and row 1's wall times are not in any source and are not stated. ≈ 26.5 min is the two sourced instants' difference.

**Cost:** ≈ **$5.2** (S). D: 26.5 min × 17 × ≈ $0.686/hr ≈ $5.2 — the arithmetic agrees; the rig's own estimate at that shape UNDER-COUNTED (its typed-YES line priced `3 + N_CLIENT` = 11 nodes ≈ $7.55/hr against the 17 launched — D from the base script) → fixed on this branch: `EST_CLUSTER_HOURLY` prices every node (≈ $11.66/hr here). On-demand throughout (S — the shape); no spot interruption (D — an interruption aborts the count and marks the results dir, which the run's log row does not report).

**Venue block:** AMI **`ami-0c40b68421a1fcd8e`** — the rig prefers the newest self-owned image tagged `squeezefs-bench-base=mw`, and the tree names that bake (`squeezefs-mw-base-v2`, `.benchmarks/2026-08-20-fabric-confirm-sessions.md` §5); that THIS run launched from it is R (the agent's recollection), and "the clone of `/etc/machine-id` is that bake's" is D from it. Build **`aad50a1f` `release`** on every node (D — the deploy's sha256 + `--version` and the assemble's `build_commit` ritual are asserts; S that the launch was "on `aad50a1f`'s artifact"). **The node kernel version and the node-to-node RTT are in no surviving source and are not stated** (the rig's `bench-sym` measures the RTT into the row label and the kernel floors are probed by `assemble-sym` — both readings left with the evidence). Instrument: `tests/cloud_bench_cluster.sh` + `tests/cloud_sym_rows.sh` at the lost branch's head `3ce1395a` (S), whose four rig fixes this branch redoes; the driver's flags are the rig's `bench-sym` defaults (`--venue=cloud`, `--size-to-rt=auto`, `--rt=60` — D from the base script).

**What failed, read off the pulled `.stats` and logs (S — the agent's contemporaneous summary; the numbers below are the only ones that survive; the READINGS beside them are D from the code):**

* **m60's daemon log** carried, at the `mkdir`, **`appender 1's extent grant is exhausted (0 unclaimed, 1 needed) and the manager has not refilled it — retry (EAGAIN)`** — the retryable `KvError::GrantExhausted` — "surfaced through `Corrupt`" to `mkdir(2)` as **`EINVAL`** (S; the path `Corrupt` → `InvalidOperation` → `EINVAL` is D, F-C3 below).
* **m60's pre-row snapshot:** `extent_grant_granted 72 = claimed 72, unclaimed 0` (S). The reading (D): `RegionGrant::recover(record.extents(), &page.grant)` with an EMPTY page word classifies every extent the tree-0 record names as CLAIMED with no mint at all — the code's own comment at `backend.rs:20437–20441` ("a `Free` page names no remainder, so everything the record still names is a live image and lands CLAIMED") — so `claimed 72` is the recover's classification of a stale-empty page word, exactly F-C1 ⊕ F-C2's shape, and consistent with the 15:55 row's "rebuilds its grant from the (stale, empty) page". (A joiner that had MINTED 72 extents would show them on `slot_tree_extents`, a gauge the source does not carry; the mint reading is not supported.) `joined_wire_verbs 356` — "one ask per cadence tick, every one a verbatim replay" (S).
* **The manager's pre-row snapshot:** `extent_grants 7`, `manager_verbs 2135` of which **`manager_verb_replays 2083`** (S). The reading (D): seven carves = one per joiner at its join; the joiners' asks answered idempotently under §5.3.5 — `unclaimed_remainder_of` (`backend.rs:9286`) answers the page's grant word, the remainder the manager itself had written — while every joiner's own RAM grant read empty: the 15:55 row's **"all 7 joiners write-dead from their join, the manager replaying every ask verbatim"** (S); WHY the two sides disagreed is F-C1's reading (D). A shape no co-located venue (the laptop, squeeze-test, the 2026-09-12 cloud `mw` row — every one of them ONE kernel, ONE page cache) could show (D).

**The three findings (each REPORTED here and in §4.4ar–at with its code site; every one routed to PR 13i `fix/sym-shared-lun-coherence`, the rung that gates PR 14's flip and this row's re-run):**

* **F-C1 — DESIGN-LEVEL, a flip blocker: cross-host page-cache incoherence on the shared metadata LUN.** Every metadata read and write goes through `uring_fs` BUFFERED I/O — the uring-path opens carry `O_CLOEXEC` only (`src/uring_fs.rs:1368/1586/1609`) and the `blocking_fallback_loop`'s opens (`uring_fs.rs:2222/2247/2258/2274`) carry no custom flags at all — while the DATA path opens `O_DIRECT` at its four sites (`src/nvme_dev.rs:744` the worker's device fd, 1576 the DUR-2 probe, 1686 / 1717 the read- and write-side direct-leg fds) — i.e. metadata rides each HOST's block-device page cache on both arms. Two hosts over nvme-tcp ⇒ a joiner reads ITS kernel's stale cache of a block the manager wrote: the appender page's two images (`Live` with the grant cleared at `backend.rs:10510–10514`, then the grant word via `write_wire_joiner_page_grant` ≈ `backend.rs:11096`, durable at the next barrier), and the same for the tree-0 / ring-0 projections, foreign slot-tree nodes, the appender directory and the ledger. Every co-located venue shared ONE cache and was structurally blind to it. **Remedy — the shared-LUN rule (GPFS / Lustre's):** `O_DIRECT` (or explicit invalidation) on every shared-LUN metadata read AND write on every host, with sector-aligned staging buffers for the ring's byte-positioned entries and the 4 KiB page / ledger writes on both paths.
* **F-C2 — `open_joined_appender` DISCARDS the `Joined` reply's grant word.** `src/meta_backend/kv/backend/joined.rs:751` destructures `ManagerReply::Joined { appender_id, already, node_seq_base, .. }` and rebuilds the RAM grant from the DEVICE page (`RegionGrant::recover(record.extents(), &page.grant)`, `backend.rs` ≈ 20450–20465) — while the manager's own doc on `write_wire_joiner_page_grant` states the contract the joiner must honour (the grant's runs reach the joiner ON THE REPLY), and `unclaimed_remainder_of` (`backend.rs:9286`) reads the page word for the §5.3.5 replay. Under F-C1 the page the joiner reads is stale and empty → the joiner is write-dead from its join and the manager replays every ask verbatim.
* **F-C3 — the conveyor's batch-failure fan-out flattens every error class but `Io` / `NoSpace` to `Corrupt`.** `clone_kv_error` (`backend.rs:23709–23727`) clones `Io` and `NoSpace` by class and turns EVERY other `KvError` into `KvError::Corrupt(other.to_string())` → `InvalidOperation` → `EINVAL`: the retryable `GrantExhausted` (EAGAIN) surfaced to `mkdir(2)` as `EINVAL`, and `SlotBusy`, `GrantDeferred`, `Busy` and `ManagerUnreachable` lose their class the same way.

**The evidence, and its loss (stated for what it is):** the row pulled the per-node `.stats` snapshots and every node's daemon logs to `.benchmarks/cloud/2026-09-24-152527/` on the dev machine before the teardown — **that directory, the rig branch that carried it (`perf/sym-cloud-row-run` @ `3ce1395a`, never pushed) and its worktree were LOST when the dev machine was reinstalled the same evening** (the run log's 20:55 row `b87d3f57`; the resume note `~/sym-run-state/RESUME-2026-09-24-omarchy.md`). The conclusions above rest on the agent's contemporaneous summary — quoted in the run log's 15:55 row `b88bfa54` and carried verbatim in the resume note's findings bullet — and on the code sites, which are re-verified on this tree (`clone_kv_error` at `backend.rs:23709`, `unclaimed_remainder_of` at `:9286`, `write_wire_joiner_page_grant` at `:11096`, the three `uring_fs` `O_CLOEXEC`-only opens, and `nvme_dev.rs`'s `O_DIRECT` opens — 744 the worker's device fd, 1576 the DUR-2 probe, 1686 the read-only direct-leg fd, 1717 the D14 write-side direct-leg fd; beside them `uring_fs`'s `blocking_fallback_loop` opens at 2222 / 2247 / 2258 / 2274 with no custom flags at all — buffered too, so F-C1's "every metadata read/write is buffered" holds on the fallback arm as well, and PR 13i's fix must cover that arm or refuse it on a shared LUN). **No per-node number of this run exists; gate 2 has no ratio, gate 3 no multiple, gate 3b no reading — the row is INCOMPLETE, not a MISS and not a MET.** The re-run must pull its evidence again. This branch redoes the four rig fixes and this record; the lost evidence is not reconstructible. **How the four rig fixes are proven:** fixes 2 (`format --force`) and 4 (the every-node estimate) by `--dry-run` at the approved shape (RED before, GREEN after); fixes 1 (the machine-id step) and 3 (the apt hygiene) — whose LOGIC a dry run cannot reach, since it prints the node script — by the shell pin `tests/cloud_bench_cluster_units.sh` over `tests/cloud_bench_node_scripts.sh` (fake `systemctl` with the real `is-active` semantics, fake `fuser` / `pgrep` / `sleep` / `systemd-machine-id-setup` / `remote`; no cargo, no root, no aws; < 1 s), which read RED on the first build (review round 1: the always-active `unattended-upgrades` waiter polled for the whole 60 × 5 s bound on every node and the upgrader's unit `stop`ped — ≈ 85 billed minutes at this shape and a mid-dpkg kill; a regular-file dbus id restoring the clone through `systemd-machine-id-setup`) and GREEN on the fixed scripts (33 / 33). **Every review-stage branch is pushed to `origin` as a backup ref from now on** (the law the loss added).

**What the run PROVED (mechanism, not number — S where quoted, D otherwise):** the PR 15 instrument works end to end on 8 REAL nodes — a run that "assembled 8 real nodes" (S) has, by the rig's own asserts, passed `launch` / `deploy` (the artifact's sha256 + `--version` on every node) / the fabric (every storage node's nvmet share with `resv_enable=1`, every client's connect + `nvme resv-report` PR verify on every data namespace) / `format --symmetric` on one node / the join ladder over a real wire on seven others (`joined_registrant_posture registrant` is a die on each joiner; `appenders_known 8` and `membership_writers 7` are S; "device registrants 8/8" is S — step 8b's per-namespace `resv-report -e` count) / the token reader's posture / the `build_commit` ritual — and the FIRST user mutation from a joined writer (S) found a class every co-located venue was blind to (D). That is the venue doing its job.

**Owed to the re-run (PR 15 Phase B, run 2 — after PR 13i lands, with a NEW expressed owner approval for that run, on the flip candidate's binary):** the three row sets from zero on the approved shape; the per-node law of gate 3 read as written; the evidence pulled and committed under `.benchmarks/cloud/<ts>/` BEFORE any verdict is written.


### 3.10 The cloud row (2026-09-24) — FAILED on row 1 with three product defects; the two-host fixture; PR 13i

Written on `fix/sym-shared-lun-coherence` against the run-log row `2026-09-24 15:55` of [`2026-09-12-sym-pr-run.md`](2026-09-12-sym-pr-run.md) and the contemporaneous readings in `~/sym-run-state/RESUME-2026-09-24-omarchy.md` — the rig branch `perf/sym-cloud-row-run` that carried the row's own §3.10 / §4.4ar–at draft was LOST with the machine's reinstall (it never reached `origin`), so these are the surviving facts, not the raw logs.

**The row.** Cluster `sqzbench-20260924-150215`, 17 × i4i.2xlarge us-east-1a, assembled on 8 REAL nodes (the rig regenerated the baked AMI's cloned `/etc/machine-id` = the daemon's node token), gate 2 `sym-tarx` arm 1: the joined writer's `mkdir` under the root answered **`EINVAL`**; torn down at ≈ 26.5 min ≈ $5.2. Three findings from the pulled logs + code, each named to its rung: **F-C1** (design-level, the flip blocker — cross-host page-cache incoherence on the shared metadata LUN: every metadata read/write rode `uring_fs` BUFFERED, the issuing host's page cache; the joiner read the FIRST image of its appender page — `Live`, grant cleared — and never the 72-extent grant), **F-C2** (the joiner discarded the `Joined` reply's grant word and rebuilt its grant from the stale page — `granted 72 = claimed 72, unclaimed 0`, all 7 joiners write-dead from their join), **F-C3** (the conveyor's batch-failure fan-out flattened every error class but `Io`/`NoSpace` to `Corrupt` — `GrantExhausted`'s `EAGAIN` reached `mkdir(2)` as `EINVAL`). **Every venue before this row was ONE kernel** — the N-daemon fixture is one process, the netns fleet one box, squeeze-test's 32 members one page cache — so the venues were structurally blind to F-C1; the laptop can reproduce it only with a SECOND KERNEL on the same LUN.

**The two-host fixture (PR 13i — `tests/mw_fleet.sh create N=1 --symmetric --vm=1 --vm-net=tap` + `tests/run_mw_matrix.sh sym-two-host`).** A qemu/KVM guest booting THIS host's kernel (7.2.5-7-omarchy; `tests/mw_guest_image.sh SQZ_MWGUEST_KERNEL_SRC=host`, ready in ≈ 1 s) on a host-only TAP (`sqz-mwtap`, 10.99.0.1 ↔ 10.99.0.2 — slirp cannot route the manager's dial to the joiner's listener), the laptop's nvmet-tcp devsub exported on the tap's host address, the guest connecting the fleet's meta NQN under its own identity (its LUN's logical block size reads 4096). **Phase 1 — the pin, RED ×4 on the base binary (`aad50a1f`'s code, `/var/tmp/squeezefs-base-fc1`):** the guest's `squeezefs appenders --json` listed page 0 at generation 48 / 57 / 61 / 65 AFTER the host manager had taken ≥ 2 checkpoint cycles and quiesced at 49 / 58 / 62 / 66 — the guest's second read was its own first image, exactly the cloud row's shape driven by the product's verb; GREEN on PR 13i's binary (§4.4ar). **Phase 2 — the join:** the guest mounts as a JOINED writer over the wire (under its own per-mount identity — its daemon connects the DATA namespace itself off the durable `fabric_endpoint:` record, the rung-2 daemon-owned connect a second host performs in the field), `mkdir` + 200 creates + `rm -rf` land, the host reads every acked byte through the divert, the guest leaves clean, the host's listing shows the region `Free`, the online fsck is clean, the guest's controller disconnects. **GREEN from zero on PR 13i's binary (2026-09-25 03:12 UTC, `/run/squeezefs-twohost/rows/twohost-1790305935`):** pin `guest 9 → 12 ≡ host 12`, `GUEST_ENABLE_URING_BEFORE=N` (the fresh-kernel premise of §4.4au met), 200 / 200 creates acked and read at the host, `invariant_tripwires` 0 on the guest, post-leave census `{}`; the manager's `meta_io` read `direct_paths 1, buffered_fallback 0, unaligned_refusals 0, read_widened 0, bounce_bytes 5,505,024` (1,344 page/node images bounced — §4.4ar's economy item) and `meta_kv_journal_pad_entries 252 / pad_bytes 975,063` (≈ 3,870 B per window on the 4096-grain LUN, the law's shape) against `journal_entries 252 / journal_bytes 59,225`; `appender_flush_ceiling_overruns` 0, `slot_handovers` 0, `dlm_token_grants_served` 6. **This fixture is the venue the cloud re-run's shape is proven on before money is spent:** no paid row launches on a binary whose `sym-two-host` is not GREEN.

## 4. Issues found (each with its PR and its red pin)

### 4.1 Fixed on this branch

1. **PR 12 — the `-o ro` reader's per-holder plane served before its recall
   channel's first round** (`e0133a07`). `EIO "the recall channel to the
   holder is not fresh"` on the FIRST resolve of a joiner's object. Found by
   the first `--token-readers` fleet. Pin: `sym_mount_posture_tests::
   a_readers_first_resolve_of_a_freshly_dialed_holder_serves_without_a_hand_wait`.
2. **PR 12 — the reader-side `dlm_token_*` stats face folded the manager's
   plane only** (`502aa859`); gate 5's engagement law was unreadable. Folds
   every plane now.
3. **PR 12b — the granted-extents cache barrier (`drop_nodes_in_extents`)
   refused a grant over a DIRTY projection node** (`502aa859`): a joiner that
   opens over a non-empty ring-0 window folds the manager's records into its
   projection, which reads dirty; the manager compacting such a leaf,
   retiring and re-granting the extent is the ordinary lifecycle → `EINVAL`
   on the joiner's create (the fleet: a rejoined joiner's 27th create). Refuses
   only a dirty node of a tree this mount WRITES. Pin:
   `sym_n_daemon_tests::a_joiners_dirty_projection_of_a_retired_manager_leaf_never_refuses_its_grant`.
4. **PR 12b (P0) — a JOINED appender's threshold maintenance ran over its
   PROJECTIONS** (`c2c5e663`). The checkpoint task's `maintenance_pass`, its
   wake check and `tick` step 1 walked `all_trees()`, so a joiner compacted
   its projection of the manager's tree 0 (and other appenders' slot trees)
   into successor images claimed off its stale projected bitmap — extents the
   manager had since granted elsewhere. The fleet's N = 8 row: finding 41's
   bounds refusal, the tail pinned, D1.b fail-stop. In process: the
   four-writer storm pin reproduced it in 0.2 s and its ATTRIBUTION output
   named tree 0 of every daemon at the corrupt address. Fix:
   `maintains_slot(None)` = `is_manager()` under an armed plane; the three
   maintenance loops walk `maintainable_trees()`. Two tripwires beside it
   (`extent_grant_conflicts` must-stay-0; `extent_grant_stale_page_words`) and
   the threshold pass's §5.3.3 reactive refill (`maintenance_grant_refill` —
   before it every threshold wake on a drained grant failed at WARN per entry,
   66/s in the pin). Pin: `sym_n_daemon_tests::
   concurrent_storms_on_four_writers_never_cross_a_record_into_another_appenders_leaf`.

### 4.2 Defect 5 — FIXED (both halves, PR 12b, P0, the flip-blocking class): two appenders' frames under ONE `node_seq`

**The evidence** (fleet r4, N = 8, joiner m62 = appender 3, meta volume 0 =
`/dev/nvme1n1`, extent `0x5a80000`, read raw off the device after the row):

```
header  node_seq=4057602432666822354 level=0 min='' max=0c35:ec (slot 0xc35's leftmost leaf)
frame@4096    node_seq_at_write=…354 SAME padded=241664 bset=237806 appender=3 g=1   ← m62's base bset
frame@245760  node_seq_at_write=…354 SAME padded=8192   bset=4160   appender=4 g=1   ← m63 APPENDED
frame@253952  node_seq_at_write=…354 SAME padded=4096   bset=848    appender=4 g=1
frame@258048  node_seq_at_write=…354 SAME padded=4096   bset=3440   appender=4 g=1
```

The instrumented refusal (`179ad23e` — the finding-41 message now names the
tree, the node's stamp and each offender's slot + provenance) read: `tree slot
Some(3125) … source node 0x5a80000 stamped Some(3125) level 0 … records
outside the bounds: [slot 3203 …, disk]` — i.e. appender 4's dentries of ITS
slot 0xc83 (`squeezefs appenders`: 3202/3203 is appender 4's rotor) folded
from the DEVICE into appender 3's slot-0xc35 leaf. `extent_grant_conflicts` /
`extent_grant_stale_page_words` 0 on the manager — the double custody is not
a double grant RECORD.

**Two halves, one class.**

(a) *The design half — one node-seq space per VOLUME* (FIXED on this branch,
red-first, `2a94abbc`). Every appender seeded its node-seq handle from the
same ledger word at its open (`backend.rs`, `ledger.seq.max(ledger.
node_seq_watermark)`), so under N daemons every seq guard the CoW law rests
on — the §4.2 child-pointer and root-pointer checks, the §4.5
frame-incarnation check that ends a recycled extent's log at a previous
node's frames, PR 11's residue-seq ceiling — was void ACROSS appenders:
lockstep storms mint the same `node_seq` in every daemon (the pin below: two
joiners' first mints carried ONE seq on `8af38eda`). **The law now**
(`kv::node_seq`, design §5.3.2 amended): the volume's uuid base `B` starts
incarnation 0 — the manager's and every flat / unarmed mount's legacy space,
seeded and raised exactly as before (`NodeSeqHandle::shared` IS the old
`AtomicU64`; a forest manager's is bounded by `B + 2^K`); every `JoinAppender`
(a rejoin included) is minted a fresh ordinal `o ≥ 1` from the durable
tree-0 counter `node_seq_incarnations` (one control entry, barriered before
the reply) and the joiner mints in the disjoint `[B + o·2^K, B + (o+1)·2^K)`,
its handle never raised. `K = 38` derived from the 63 usable bits (2^38
mints per incarnation = 200 SMOs/s for 43 years; 2^25 incarnations = 15 k
mounts re-joining daily for six years; either exhausted refuses loud, never
wraps — tie test `node_seq_incarnation_space_partitions_the_63_usable_bits`).
Every order comparison classified: the pointer checks / frame walk / frame
screen compare equality (unaffected), PR 10's root choice is by generation
(unaffected), `install_recovered_root`'s "older → re-read" became a mismatch
test, every raise goes through `raise_to` (a joined handle ignores it, the
shared handle confines it to its own space — PR 10's raise to a dead
joiner's root / residue stamps is the disjointness now). Pin:
`sym_n_daemon_tests::two_joined_appenders_never_mint_an_equal_node_seq`
(manager in span 0, joiner 1 in span 1, joiner 2 in span 2, a rejoin in span
3 — never back in its dead space). The wire's `Joined` reply carries the base;
the page layout is unchanged (a root installs by pointer + seq equality).

(b) *The grant half — a refill run that partially overlaps HELD extents
re-unclaimed them* (FIXED on this branch, red-first). The manager's §5.3.5
answer is the caller's page word verbatim (or its coalesced carve), and the
page word is the remainder at the caller's last page WRITE; `wire_extent_
refill`'s `fresh` filter kept a run unless EVERY extent of it was held, and
`RegionGrant::add_runs` inserted every extent of a kept run into `unclaimed`
— a live image claimable again by the next mint, or trimmed into the return
batch and RETURNED while its node stood. `add_runs` now skips claimed /
pending / returnable extents. Pin: `sym_manager_tests::
a_grant_run_overlapping_held_extents_adds_only_the_extents_the_grant_does_not_hold`.

**The fleet's verdict** (§3.1 r5): with (a) + (b) landed the N = 8 row runs
to completion — 2/2 red before, 1/1 green after, on the same fleet shape
and the same instrument. The two halves' contributions are NOT separated
(a (b)-only fleet row was not run — the counted-run law forbade spending
another red-first row on it, and the in-process storm never reproduced the
fleet's contamination on either tree): the on-disk evidence — two
appenders' frames under one `node_seq` in one extent — is (a)'s class by
construction, and (b)'s partially-held-run re-unclaiming is pinned by its
unit contract. With distinct seq spaces a second custodian's frames are
now `StaleIncarnation` at the frame walk (seen as harmless residue in the
defect-6 dumps) instead of folded.

### 4.3 Defect 6 — FIXED (KV core, PR K6/§4.6 — an acked-loss class every layout can reach; found at N = 8): a checkpoint flush that appended a PARKED frozen delta took the dirty floor of the records applied since

**The symptom.** The eight-writer in-process storm pin
(`sym_n_daemon_tests::concurrent_storms_on_eight_writers_at_the_fleets_
depth`, `--ignored`, ~25 s) with the fleet's row boundary — every writer
UNLINKS its previous round's 24,000 files before the next round — read,
after a joiner's CLEAN LEAVE, 21–280 of the 192,000 unlinked names
resolving at the manager at `nlink 0`, always the LAST unlinks of one
leaving daemon's directory; N = 1 and N = 2 clean, N = 8 red 5/5 on the
tree carrying defects 7–14 (the durable state lacked the tombstones: a
fresh writer resolved the same names — the pin's DUAL verdict, added for
exactly this attribution). The fleet's `sym-scale` row never showed it.

**The attribution** (three instruments, each added to the pin this round):
(1) the per-frame CENSUS of the leaf the released tree routes the name to
— its base a compaction output (e.g. 59 puts / 383 dels), ONE appended
frame of 8 dels, and the remaining tombstones in NO frame; (2)
`LEAVE-DIFF` — the leaving daemon's own tree, read BEFORE its leave,
routes the name to the SAME leaf (same address, same `node_seq`) and its
RAM fold says `Tombstone`; the manager's post-leave read of that leaf says
`Live` — the tombstone sat in the leaf's OPEN overlay and the leave
released the tree without appending it; (3) the Heisenbug that named the
step: a 192,000-name walk before the leave delayed it by seconds, the
daemon's own cadence flushed the leaf first, and the pin went green.

**The mechanism** (`KvTree::checkpoint_flush_node`, the checkpoint's
per-node step — flat code, every layout): `freeze_locked` answers a
PRE-EXISTING frozen delta when one is parked — an SMO froze the node for
its fold (`freeze_for_smo`, a REAL freeze-swap that leaves the dirty
floor intact) and then FAILED before its swap: a merge or compaction
refused an extent (`GrantExhausted` — the joined appender's common case
under the N = 8 grant storm: 100–150 wire refills refused per joiner per
run, `merge_after_flush`'s `claim_internal`). Commits keep applying into
the OPEN delta (`mark_dirty` admits an apply while FREEZING; only
SUPERSEDED refuses). The next flush step got the parked delta back,
`take_dirty_floor()` cleared the WHOLE floor, and `append_frozen` wrote
the parked delta alone — the newer records stayed in the overlay with
`dirty_floor == MAX`: no later dirty walk saw them, nothing clamped the
tail, `flush_slot_clear_of_region`'s `tail ≥ frontier` held, the release
recorded the leaf's tail at the parked frame's end, and the tree moved
without them (the joiner's RING — their only durable home — released
with the region). Why flat volumes never showed it: an SMO fails mid-way
there only on `NoSpace` / `JournalReserveExhausted` (rare); a joiner's
`GrantExhausted` is routine. Why N = 1/2 never showed it: no grant
pressure.

**The fix** (a FLAT-PATH behaviour change, admissible under law 1's
second clause as a SHIPPED-BUG fix — its two conditions met: a red-first
pin on both layouts (`kv_freeze_wedge_tests` runs flat and stamped in the
matrix since fix round 1) and the 1.3.0 `RELEASE_NOTES.md` bullet, written
in fix round 1 — Issue 5): in the same lock window, when the freeze
returned a delta and the open overlay still holds records, the floor of
THOSE records is
restored (`NodeDirty::overlay_floor` — `min(entry_floor, seq)` over the
open delta, the exact per-record stamp `apply_locked` set) so the node
stays dirty for them and the next pass appends them; engagement
`meta_kv_flush_floor_kept` (0 on a solo mount whose SMOs never fail
mid-way). Pins: `kv_freeze_wedge_tests::a_flush_of_a_parked_frozen_delta_
keeps_the_floor_of_the_records_applied_since` (the exact shape by hand on
the flat harness: a parked freeze, newer deletes, the flush step —
red-first: the node read CLEAN with the deletes only in RAM; green: dirty
until the second pass, every record on the device) and the eight-writer
storm pin (`--ignored`, the fleet's depth; §3 lists its from-zero count on
the fix). The two instruments — `LEAVE-DIFF` and the per-frame census —
stay in the pin's attribution.

### 4.4 Defect 7 — FIXED (PR 12b): a joined holder's lease projection is loaded once and never refreshed on its own — every joiner→joiner cross-owner create into a slot a LATER joiner minted was refused

Found by `sym-shared-dir` on a fresh fleet: m61's very first create into
m60's directory `EINVAL`, m60's log `cross-owner step … insert names child
ino …, which has no inode record and lives in a slot no appender leases — a
dentry nobody could have minted a target for is refused
(xv_cross_owner_steps_rejected)`, the intent left open and re-refused by
the roll-forward cadence every second. `sym-foreign-touch` read the same
class as "only 92 shipped steps for 192 foreign creates". The served
insert's Issue-8a screen (`screen_insert_child`) judges the child's slot
by the SERVING mount's lease table — on a joiner a PROJECTION of tree 0,
loaded at its open and advanced only by an event (a re-dial, a divert
failure, a `NotHolder` redirect, the join arm) — so every slot a LATER
joiner acquired read `Unleased` at every earlier joiner for as long as no
event fired; the second clause (the child's record on its volume) reads
through the same stale table (an "unleased" slot is read at the manager,
which has no such record). Fix: `KvMetaBackend::resolve_slot_holder_fresh`
— the table's answer; on a joined appender that reads `Unleased` the
projection is refreshed ONCE (`refresh_control_projection`), and when the
refreshed table STILL says `Unleased` — the ledger lags a grant by up to
one checkpoint (a grant is a ring-0 control entry; tree 0's moved root
reaches the ledger at the manager's next cycle, so a re-read of the ledger
right after the grant names the OLD root; the pin's first run read
`Unleased { g: 0 }` after the refresh) — ONE wire `ResolveSlot` asks the
manager's table, the lease's own word (`slot_resolve_rpcs` at the
manager); the screen reads it, the manager's table is never refreshed. Pin
(red-first): `sym_n_daemon_tests::a_joined_holder_resolves_a_later_
joiners_slot_at_a_served_step` (the raw table's `Unleased` as the premise,
the fresh resolve's `Holder { 2 }`, joiner 2's create into joiner 1's
seeded directory SERVED at joiner 1's own listener — the dentry lands in
joiner 1's tree, the record at its creator, `steps_rejected` unmoved).
PR 12b's sym-storm `--cross-owner` never saw it because its mover renames
into a directory the MANAGER holds (whose table is authoritative); this is
the first joiner→joiner cross-owner row.

### 4.4a Defect 8 — FIXED (PR 7b under N daemons): a foreign create into a STRIPED directory judged the stripe's record by the initiator's projection and refused `ENOENT`

Found by `sym-shared-dir` on the fixed defect-7 binary: every foreign
writer created 1–3 files into m60's directory, m60 flipped it to 64
stripes ("1 supplied by creators [0, 6, 7], 63 minted by the holder" —
appenders 6 and 7 DECLINED "no endpoint bound on this mount", a
`SupplyStripeIno` path that does not run `bind_holder_endpoint_on_demand`;
noted, benign: the holder mints the remainder), and every later foreign
create answered `ENOENT` "directory … was removed (no record)" with
`dir_stripe_dying_refusals` +1 at each initiator (m0, m61, m62 — the
manager too, whose RAM tree of m60's slot is a stale image like any
non-holder's). `refuse_dying_parent` (R26's closer, at every insert path)
read the KEY parent — the stripe, minted in the holder's rotor AFTER the
other daemons' projections loaded — through `read_inode_value_routed`,
the cross-volume plan's OWN-record witness, i.e. this mount's projection
of the slot: no record. PR 7b's fixture (declared regions, one process,
one RAM tree) could not see it. Fix: `key_parent_nlink` reads the record
through `getattr` — the writer's read divert (PR 12b: the holder's token
plane, exact until recalled); an own-slot parent and every unarmed mount
take the local read verbatim (`token_serve` answers `None`); the same
read serves `dying_parent_errno`, whose local read had a second face — a
REAL `EEXIST` on a foreign striped parent was reported as `ENOENT` when
the projection lacked the stripe. Pin: `sym_n_daemon_tests::
a_foreign_create_into_a_striped_directory_reads_the_stripes_record_at_its_
holder` (joiner 2 the process's reading writer under PR 9's custody arm —
its reads of joiner 1's slot are token reads at joiner 1's listener, the
fleet's shape; joiner 1 flips its directory after joiner 2's projection
loaded; 12 post-flip foreign creates land in their stripes,
`dying_refusals` unmoved).

### 4.4b Defect 9 — FIXED (PR 6 + PR 12b): the served ship never fed the holder's dominance window, and a JOINED holder's offer refused — no idle tree ever moved on a fleet

Found by `sym-foreign-touch`'s IDLE phase (gate 3c) on the same binary:
12 dominating bursts of 64 foreign creates over 133 s into m60's idle
tree, `slot_offers` 0 at every daemon, no handover — while the LIVE phase
passed (192 ships, 0 handovers; defect 7's class gone). Four defects
under it (the fourth found by the pin: `slot_handovers` counted the
in-process accept's release + grant alone — a WIRE holder's release on
its recall ran uncounted, so the leg's handover signal could never move
even with the offer landing; the manager now counts a `ReleaseSlot` that
SPENDS a standing recall as the handover and the requester sets the
never-thrash cooldown on ITS plane at the accept — `joined_accept_offers`):
(1) PR 4's `note_slot_ship(slot, requester, ship_ns)` — "the
holder decides who moves a slot … at the served ship" — had NO product
caller: PR 4's contracts drove it directly and PR 6's served step
(`xv_serve_step`, the production ship) never called it, so `ops_q` never
accumulated on any fleet and neither offer arm could fire; (2) a JOINED
holder's verdict reached `manager_offer_slot`, a manager verb's local
executor, which `manager_gate` refuses on a joined appender
(`joined_control_refusals`) — the offer never reached the manager's
table; (3) found by the pin once (1) counted: the door's holder-op note
(PR 12b round 1 Issue 11 — INSIDE the door, for every commit naming the
slot) counted the SERVED step's own commit as the holder's activity, so
`ops_h` grew 1 : 1 with `ops_q` and `ops_q ≥ 2 × ops_h` held for no
requester ever — the law's premise is the holder's OWN work. Fix: the
served step notes the ship after its apply with the served wall as
`ship_ns` (the requester = the shipping mount's appender: its member id's
identity where the plane learnt it — `SlotLeasePlane::appender_of_
identity` — else, for an insert, the creator through the child's slot,
the stripe census's own resolution; the wire `ResolveSlot` answer of
defect 7's fresh resolve is LEARNT into the projection so that read
answers without another verb), a joined holder's offer travels as the
wire `OfferSlot` (`joined_offer_slot`), and `KvTx::served_step` — set by
`xv_apply_step(.., served = true)` at the served side alone — skips the
door's holder-op note; the accept, the recall on the holder's carriage
and the flush-then-transfer are PR 12b's existing member side. Pin:
`sym_n_daemon_tests::a_dominating_requester_earns_an_idle_joined_holders_
tree_through_served_ships` (joiner 2 ships `N_floor × 4` creates into
joiner 1's idle directory served at joiner 1's listener: the holder's
`ships` count every one, ONE idle offer, the manager's table `Offered`
to joiner 2 on its carriage, `joined_control_refusals` 0; the accept
recalls, the holder's carriage sink hands over, joiner 2 holds the slot
at `g + 1` with every acked name).

### 4.4c Defect 10 — FIXED (PR 4 under N daemons): a joined holder's `N_floor` sat at its absolute floor of 2 for the mount's life

Found by `sym-shared-dir` on the defect-8/9 binary: every create landed
(20,000 in 4.49 s) but `slot_handovers` read 1 — the directory moved to
the first requester whose 2 ships beat the holder's second own op, in
the storm's first milliseconds. `N_floor = max(2, ceil(ewma_handover /
ewma_ship))` is seeded ONCE at the plane's arm from the always-on tables
(`seed_n_floor_inputs`: the barrier EWMA and the S8 ship RTT); a JOINED
appender arms before its first device write and before any S8 ship, so
both read 0, the seed took nothing, and `n_floor(0, 0)` = 2 — the
"single touch never moves anything" floor doing duty as the handover
price. The leg's own log said so: `N_floor(A)=2`. And a WIRE holder's
handover cost was never folded: `fold_handover_ns` ran at the manager's
in-process accept alone, so the holder that PAYS the flush-then-transfer
learnt nothing from it. Fix: `note_slot_ship` re-seeds while
`ewma_handover_ns` is 0 (the tables are populated by the first served
ship), and `transfer_slot_locked` folds `flush + page + tree 0` at the
holder. The dominance law itself is unchanged (a dominator wins over a
12,000-creator crowd by design — `dominance_over_a_common_window_decides_
every_offer`); what changed is that its price is the DERIVED one from the
first ship, and the measured one after the first handover (5.5 ms on the
fleet against ≈ 0.3 ms ships ⇒ ≈ 18).

### 4.4d Defect 11 — FIXED (PR 6 + PR 12b): a stale holder view at the travelling guards surfaced EAGAIN to the application

Found by `sym-shared-dir` on the defect-10 binary: m61 created 52 of
2,500 then `EAGAIN`; its log: `cross-owner guards for scope … name forest
slot 3, which this mount does not lease — the initiator's holder view is
stale; it re-resolves through tree 0`. The manager had handed its stripe's
slot 3 to a dominating requester between two of m61's creates (a legal
verdict — §4.4e below is what makes it moot for stripes); m61's
projection still named the manager, the manager refused the `XvGuards`
with the text above, and NOTHING re-resolved: `acquire_guards_leased`
returned the refusal and the create failed. Fix: the refusal IS the
re-resolve's trigger — every key's slot is re-resolved at the manager
(`KvMetaBackend::reresolve_slot_holder`: one wire `ResolveSlot`, its
`Holder` / `Unleased` answer LEARNT into the projection — table, gate,
holder cache) and the acquisition retried, bounded at 2
(`xv_cross_owner_guard_stale_reresolves`; a third stale answer is the
retryable class the caller sees). Pin: `sym_n_daemon_tests::
a_stale_holder_view_at_the_guards_re_resolves_and_lands_the_create`
(joiner 2's projection names the manager for a directory's slot; the
manager hands it to joiner 1; joiner 2's create lands after ONE
re-resolve, its projection names joiner 1, the dentry is in joiner 1's
tree).

### 4.4d' Defect 12 — FIXED (PR 12b): a joiner's projection dirt refused every transfer-in of a manager slot

Found by defect 11's pin (RED at its premise): joiner 1's accept of the
manager's offer failed `corrupt KV encoding: slot 4's cache barrier: node
… is dirty or locked on the recoverer — a mount wrote to a slot it did not
lease`. The cross-daemon adoption barrier (`adopt_transferred_slot_tree`
→ `NodeCache::drop_slot_nodes`) rests on "a foreign slot is never dirty
here — the door refused every commit"; on a JOINED appender that is
false: its open REPLAYS ring 0's un-checkpointed window into its
projection (the manager's records, dirty in RAM as every replayed record
is), so any joiner that joined while the manager's window stood held
dirty projection nodes of the manager's slots, and every later grant of
such a slot to it — an accepted offer, a first touch after the manager
released it — refused, for the mount's life (PR 12b's fixtures
checkpointed the manager before the joins). On a joined appender the
barrier DISCARDS the slot's cached nodes (`discard_slot_nodes`, PR 10's
failed-recovery inverse — the dirt is never this mount's own); the
manager's barrier keeps the refusal (its projections are never dirty).

### 4.4e Defect 13 — FIXED (PR 4 + PR 7b): ships into a STRIPED directory fed the handover's dominance window

`sym-shared-dir` on the defect-10 binary: `slot_handovers` 1 — the
manager's supplied stripe (slot 3) moved to the first requester whose
few ships into that 1/K shard beat the manager's own few. A legal verdict
of §5.1.4's law over a slot the striping already spread K ways, and the
gate-3b law says "no handover (aggregate never triggers)": §5.6.5's own
split — ONE dominating creator is a handover candidate, MANY are a
striping one. `xv_serve_step` feeds no dominance window for a ship into a
known stripe or a directory whose map this mount has read
(`RoutedMetaBackend::is_striping_domain` — two `scc` probes); an ordinary
directory's ships are unchanged (gate 3c).

### 4.4g Defect 14 — FIXED (PR 7b + PR 12's `-o ro` reader): a token reader listed a striped directory's RAW tree

Found by `sym-shared-dir-ls` once every create law of gate 3b held
(20,000 creates, 4,610/s, flip at the holder, 17,459 stripe ships,
closure `shipped ≡ served`, 0 handovers): the token reader's `ls -l`
statted 0 of 20,000 children — it listed 65 entries: the 64 nameless
stripe directories and the NUL-named markers rendered as empty names.
PR 7b's `stripes_armed` = `slot_lease_armed()`, and a `-o ro` reader has
no slot-lease plane, so the map was never read there, the markers never
filtered, the K-way merge never run — the row the design's gate 3b
`-ls` leg exists to price ("K stripe tokens + C inode tokens, 0 leaf
reads") could not run on a reader at all (PR 7b's owed "the wire reader's
per-stripe token merge", PR 12's). Fix: `KvMetaBackend::striping_plane_
armed()` = the plane OR a token reader; the map, the merge, the marker
filter and the `stat` fold key on it (never a mutation gate — the reader
writes nothing, `persist_striped_times` is holder-only). The reader's
map, stripes and children come as tokens from their holders. Pin:
`sym_coherence_tests::a_token_reader_lists_a_striped_directory_as_the_
merge_of_its_stripes` (48 names over 4 stripes: the reader lists exactly
the user names, resolves and stats every child, `stat D` folds, ≥ K + C
grants).

### 4.4h Defect 15 — FIXED (PR 12's `-o ro` reader / PR 5's plane): a token reader's plane never followed a manager failover — every read `EIO` for the rest of the reader's life

Found by `sym-crash` round 1 on the fixed-defect-6 binary: after the
manager's `kill -9` + remount the token reader (m1) answered `EIO` to
every op — `.stats` unreadable, `membership_readers` never 1 again
(attempt 1 read "never reached 1 within 129 s"). Its log: `token recall
channel to 192.168.86.52:40325 could not connect … retry in 5s` for ever,
`read token unavailable: the recall channel to the holder is not fresh`
— the DEAD manager's ephemeral listener, while the successor had armed
on `:34261` and the reader's membership had already re-pointed to the
successor (era 3). The manager's plane is the reader's DEFAULT
(`tokens_reader`, a `OnceLock` with a fixed `cfg.endpoint`, dialed at the
arm on the endpoint appender 0's claim-set entry named); PR 12b gave the
WRITER's per-holder planes a follow arm (`data_grant::foreign_read_plane`
→ `rebind_holder_endpoint_if_moved`: "a manager failover keeps appender
0's identity and publishes a NEW listener") and the READER's planes —
the default and the per-holder ones alike — got none. Fix (`KvMetaBackend::
reader_plane_follow_holder`, `TokenReaderPlane::repoint`): a resolve whose
plane's channel FAILED (its dial refused — `ChannelWait::Failed`, early,
never the whole window at a dead address) or never freshened re-resolves
the holder's endpoint off DURABLE state (`sym_join::resolve_holder_
endpoint`: its page identity → its claim-set entry, a control xattr read
off the reader's own S5 projection — no wire; the poll refreshes it from
the successor's checkpoints), and a MOVED endpoint re-points the plane
IN PLACE: identity, gauges, data sink and R5 registration kept; the
endpoint word and its generation swapped (every pooled grant session and
the channel's session were dialed under the old one and drop before
their next call), every cached token dropped with its purge (a holder
that moved may have re-granted — PR 5's dead-holder law), the channel
task woken out of its backoff to dial the successor at once; the binding
for the holder moves with it. A per-holder plane (a rejoined joiner at a
new port) is stopped dead and the successor's dialed. The same address
keeps the shipped fail-closed window verbatim. Gauge `dlm_token_holder_
repoints` (0 on a fleet that never failed over). Pin (RED-first with the
fleet's exact text against the shipped shape, GREEN in 4.2 s): `sym_mount_
posture_tests::a_token_reader_follows_a_manager_failover_to_the_successors_
listener` — the manager enrolled at A, the reader served from A; the
manager dies, a successor at the same identity enrolls B, the reader's
poll adopts it; the next `getattr` SERVES from B, the plane `Arc::ptr_eq`
the one armed, `grants` continuous, `holder_repoints` 1, holder 0's
binding moved.

### 4.4i Defect 16 — FIXED (PR 5's `-o ro` reader): a token reader's `listxattr` served the token's CARRIED names alone — no `client:` registration was ever discoverable on a reader, so its census shard never re-enrolled at a successor's coordinator

The same `sym-crash` round, the gate after defect 15: with the reader's
planes following the successor (both volumes re-pointed `:33703 →
:34493`, the acked-writes oracle GREEN, `self_fences 0`), the round
failed "no fleet worker enrolled at the successor's coordinator 80 s
after its arm (`job_remote_workers=0`)" — the reader's worker logged
`no coordinator endpoint published yet` for the rest of the round. The
worker's discovery is `cluster_wire::discover_endpoint` →
`mount_registrations` = `listxattr(1)` + one `getxattr` per `client:`
record; on a token reader `KvMetaBackend::listxattr` DIVERTED to the
token serve and returned the token's carried names (the user-visible
class) ALONE, so ino 1 listed as `[]` and no registration existed on the
reader — the worker enrolled once at mount (its first discovery ran
before the token arm) and never again. `getxattr` of a control name
already read the projection (`token_carried_xattr` is the one list);
the listing now does too: the token's names + the CONTROL names off the
reader's own trees. Pin: the failover contract's second half — the
successor's `client:` record with its job endpoint, checkpointed, the
reader's poll adopts it, `listxattr(1)` names it, `mount_registrations`
carries it, `discover_endpoint` answers the successor's coordinator
(RED: `[]`). The standing PR 12b finding stays: a dead incarnation's
record is heartbeat-fresh for `CLIENT_STALE_TTL` (45 s) and sorts first
by id, so re-enrollment lands within 45 s + the retry grain; the leg's
80 s bound covers it.

### 4.4j Defect 17 — FIXED (PR 7b's read paths on PR 12's `-o ro` reader): a token reader took the HOLDER's arms in the stripe-map read — its negative "unstriped" hint outlived the holder's flip, and the root listed EMPTY

The same `sym-crash` round, its last gate: with defects 15/16 fixed the
round reached "the reader m1 does not read joined writer m60's
post-failover name 60 s after it landed" — `ls /` on the reader listed
NOTHING, no error (`stat /r1`: ENOENT), while every joiner listed the
same root through the successor's token plane correctly. The successor's
log named it: `directory 1 STRIPED into 64 stripes (4 supplied by
creators [1, 2, 3, 4], 60 minted by the holder)` — seven joiners'
post-failover `mkdir /after-failover-*` had striped the ROOT under the
reader's cached root token. `dir_stripe::served_here` answered `true`
for a mount with NO slot-lease plane (the unarmed writer's law, where the
striping paths are off anyway), so on a token reader `stripe_map` ran
the holder's arms: the reader's first (pre-flip) read of `/` cached the
negative "unstriped" hint under lease `(0, 0)`, and the hint is
invalidated only by the holder's OWN flip — the successor's flip cleared
nothing at the reader, every later `stripe_map(1)` answered `None` off
the hint, and the listing fell to the token's raw dentry set with the
markers filtered: EMPTY (every name had migrated into stripes). Fix: a
plane-less mount serves everything it WRITES and nothing it only READS
(`served_here` → `!is_read_only()` without a plane) — a reader is never
a holder: no negative hint, no migration kick, the markers re-read off
its cached token (one `find` per marker, no wire). Beside it: the recall
channel's reconnect backoff resets at a completed ROUND, not at the dial
— a successor that accepts the connection and refuses every frame (the
failover's membership re-assertion window: `holds no live membership
lease with this set's owner`) was re-dialed at the 50 ms floor 20× a
second. Pin (RED-first, `left: []` — the fleet's shape in one process):
`sym_coherence_tests::a_token_reader_holding_a_directorys_token_across_its_
flip_lists_the_merge` — the reader lists the unstriped ROOT (12 names,
its token cached), the holder flips root to 4 stripes and migrates, 7
more names land, the flip's inserts recall the token; the reader's next
listing is the exact 19-name merge and every name resolves.

### 4.4k Defect 18 — FIXED (PR 12b's projection on PR 13's granted-extent barrier): a joined appender's PROJECTION tree spun its whole restart budget on a RECYCLED root — `restarts [root-seq] = 256`, EIO on the user op

Found by `sym-scale` N = 8 on the defect-17 binary (attempt 3 — attempts
1/2 passed N = 8: a race): joiner m63 (appender 4, joined 11 s earlier)
read `traversal retry budget exhausted descending to level 0 (routing
loop — SMO protocol bug) restarts [root-retired, root-seq, routing-hole,
child-retired, child-seq] = [0, 256, 0, 0, 0]` twice on user ops one
second after `joined appender 4 dropped 1 stale projection node(s)
inside a fresh extent grant` ×3, and its create storm died. The shape: a
joiner holds tree 0 (and the manager's native slot tree) as a PROJECTION
— a `KvTree` whose root pointer is the one it adopted at open or at its
last `refresh_control_projection` (keyed on the ledger seq, run at the
cadence and at F3's re-dials). The manager compacts the tree (a root
swap), frees the old root's extent at the covering checkpoint, and the
free extent is RE-GRANTED — here to m63 itself, whose granted-extent
barrier (defect 3's `drop_nodes_in_extents`) dropped the stale image and
whose next mint wrote a fresh node there (a peer's write landing on the
device is the same shape). The image under the projection's root address
now carries ANOTHER node's seq, and `KvTree::descend` — which re-reads the
root from the tree's own pointer at every restart — exhausts its budget
on `root-seq`: nothing between restarts moves a projection's root. Before
the barrier the stale image was served (a bounded-staleness read, the S5
law); the barrier turned it into a loop. Fix: `NodeCache::install_
projection_refresh` (a JOINED appender installs `refresh_control_
projection` behind the SMO mutex's `try_lock` — a holder of the mutex
on a joiner that reaches tree 0 is a refresh already in flight, whose
root the next restart reads; the flush pass never traverses a
projection), and `descend` runs it every `PROJECTION_REFRESH_EVERY` = 8
root-pointer restarts of a tree this mount does not WRITE
(`NodeCache::is_projection`: tree 0 on a non-manager, a slot tree whose
structural verdict is not this mount's) — the walk restarts on the root
it installs (the ledger record that named the new root is the
checkpoint whose coverage freed the old extent). A writer's own trees
never take the arm (their root is live; a loop there IS the SMO protocol
bug the budget names); the exhaustion error now names the tree, its
slot, the root pointer and "a PROJECTION here". Gauge `meta_kv_
projection_root_refreshes` (0 on the manager and every flat mount). Pins
(new suite `tests/sym_projection_refresh_tests.rs`, tree-level and
deterministic where the fleet's race is not): the recycled-root shape
stood by hand (a projection-posture cache, tree A's root image dropped by
the barrier, its extent released and re-claimed by tree B under a fresh
seq) exhausts the budget with no refresh installed and names the shape;
follows B's root through the installed refresh (one refresh, the gauge
+1, the next lookup restarts nothing) — RED-first against the base
traversal (`if false &&` on the arm: the budget exhausted); a writer's
own tree never consults the hook.

### 4.4l Defect 19 — FIXED (PR 12b's F2 liveness word): a fresh SUCCESSOR read every live joiner as DEAD inside its re-assertion window — the acked-writes oracle lost 1,318 of 1,318

`sym-crash` round 4 (rounds 1–3 GREEN): right after the manager's
`kill -9` + remount the oracle read every joined writer's acked file
through the SUCCESSOR and every read was refused `EAGAIN` — `object …'s
slot is leased to appender 4, which the membership owner lists DEAD —
its slots are the recovery's within the ledger poll` — for every joiner,
1,318 of 1,318 "lost". The successor's S6 census is EMPTY until the
joiners' renewals re-assert (a beat, up to 4.3 s here), and
`foreign_slot_holder_live` read `MembershipOwner::member_is_live` — the
`RecordDeath` screen's word, where "not listed" is `false` on purpose
(a peer's death word never overrules a member held LIVE; an unknown
member proves nothing). Rounds 1–3 read after the beat landed. Fix:
`MembershipOwner::member_liveness` — THREE-valued: `Live` inside its
deadline, `Dead` past it / departed here (the departed memo, inside its
retention) / absent with the re-assertion window CLOSED (the durable
roster re-asserted or was recorded dead at the deadline), `Unknown` while
the window is open (the successor has not heard from it YET) — and the
F2 predicate refuses only `Dead`: an `Unknown` lessee is dialed once
(bounded; the manager's word if the dial fails), never refused. Pin:
`dlm_membership_tests::a_successors_liveness_word_is_unknown_inside_its_
reassertion_window` (a successor with its window open: an unheard-of
member `Unknown` while `member_is_live` stays `false`; a joined one
`Live`; after its clean leave `Dead`; past the deadline an absent one
`Dead`). The fleet round was the RED.

### 4.4n Defect 20 — FIXED (S9's served free under PR 12b's plane): the manager's count for a joiner's shipped free walked EVERY slot tree — its PROJECTION of the joiner's tree included — a leak class, and a routing loop once the projection's root was recycled

Found by the new `sym-walls` leg's first run (gate 7 row (a), attempt 4):
7 joiners × 16 × 64 MiB rewritten in place — 1,792 displaced blocks —
and `shipped = 0`, `served = 0`: every joiner's terminal free was
ABANDONED after 3 attempts (`free_ship_failures` +405 on one joiner,
`free_replays` +5,426 at the manager — the retries answered from the
dedup window): `durable block-reference count failed on /dev/nvme2n1
while serving a shipped free: … tree 0 (slot Some(1042), root …, a
PROJECTION here): traversal retry budget exhausted`. The served free
(`cowriter::durable_block_refcounts_with` → `KvMetaBackend::block_ref_
count`, S9's owner-side validation "the ledger, not the peer's claim")
counted the block's references over EVERY slot tree of the volume — on
the manager that includes the trees JOINERS lease, which are PROJECTIONS
there (the grant-time image; the lessee appends into its images and
moves its root under its own page). Two faces: (a) STALE — a reference
the joiner RELEASED still read as held in the manager's image, so the
free was `NonTerminal` for ever and the block leaked (pinned); (b) the
loop — the joiner's SMO retired the projection's root, `ReturnExtents`
returned the extent, the manager re-granted it, and the manager's
traversal of the projection spun the budget (defect 18's shape at the
MANAGER, which installs no refresh for a lessee's tree). PR 7 §5.4.3 law
2 is the law: an unshared block's references live in its OWNER's slot
tree and the lessee's terminal free carries that tree's verdict; a block
two slots share is the index's (`ReleaseShared`), never a count's. Fix:
`KvMetaBackend::block_ref_count_maintained` — the count over the slot
trees this mount WRITES (`!gate.is_foreign(slot)`; `SlotTrees::refs_
window_where`), the served free's word; unarmed and on a flat volume ≡
`block_ref_count`. Pin: `sym_n_daemon_tests::the_served_frees_refcount_
skips_a_joiners_projection_tree` — the manager publishes a block on a
file in one of its rotor slots, releases the slot, the joiner acquires it
(the transfer barrier adopts the live root) and RELEASES the block in its
ring; the manager's union count still reads the STALE 1 (asserted — the
old executor's word), the maintained count 0, and a reference in a tree
the manager writes counts on both.

### 4.4o Defect 21 — FIXED (PR 5's reader plane): a single-flight token fetch LOSER could lose the winner's wake — a joiner's `lookup(1)` parked 455 s

Found by `sym-walls` on the defect-20 binary (`/tmp/grok-justin/
pr13-walls2`): row (a)'s rewrite wedged on joiner m64 — the FUSE watchdog
named ONE `lookup(1)` overdue at 455 s and climbing while a fresh
`lookup` of the same name on the same daemon served at once; the census
was clean (no conveyor window, no pipeline permit, no ring park, every
other op on m64 served). `TokenReaderPlane::fetch` is single-flight per
object: the loser reads the in-flight entry, then `n.notified()`, then
awaits. The winner that FINISHED between the loser's entry read and its
`notified()` removed the entry and bumped the epoch BEFORE the loser
registered — and `sqz_notify` registers at creation, so a
`notify_waiters` before creation is lost: the loser parked for ever
(the tick re-polls the epoch-gated future alone, which never fires
again for a gone entry). The register-recheck-await idiom (the
`long-running` law every other parked wait in the tree already follows):
register FIRST, re-check the entry is still the winner's
(`fetching.read_sync(&object, |_, v| Arc::ptr_eq(v, &n))`), await only
then; gone ⇒ the winner finished ⇒ re-read the cache. Seam
`TEST_FETCH_LOSER_HOLD` parks the loser exactly in the window. Pin:
`sym_coherence_tests::a_single_flight_fetch_loser_registers_before_it_
rechecks_the_winner` — RED at its 5 s bound on the base ordering, green
with one grant (the loser re-read the cache).

### 4.4p Defect 22 — FIXED (PR 12b's joiner under PR 8's allocation lease): the W1 ladders ran a non-holder's eligible overwrite into the allocator's ERROR-logging gate

The same m64 log: a burst of `W1 in-place sub-block patch refused: this
armed symmetric writer does not hold the ALLOCATION LEASE …`
(`plane_gate` from `begin_patch_sole_owner`, reached from
`try_inplace_rewrite` / `try_sole_owner_patch` during the row's in-place
rewrite), one ERROR line + one `cowriter_accounting_refusals` — the
must-stay-≈0 tripwire — per eligible overwrite on every joiner. The
2026-08-19 mw-fleet storm fix made exactly this class a counted
DECISION for the CO-WRITER posture (`patch_ineligible_posture`, checked
before any allocator arm); PR 12b's joiner is a `writer` posture whose
allocation plane is PER VOLUME (the lease, never the posture word), so
the posture clause never fired for it. Fix: `BlockAllocator::
holds_ownership_plane` (the gate's armed question — `alloc_lease::
holding(vol_tag)` on a grant-armed allocator — answered without its
refusal), `SoleOwnerVerdict::NonHolder` decided FIRST in
`DataRouter::sole_owner_verdict` (before the custody clause and before
any probe), and all three W1 sites take it: the sub-block patch's match,
the whole-block `try_inplace_rewrite` (which now runs the durable clause
too — it had relied on the RAM predicate alone on an armed set, PR 7's
gap), and the dd probe's armed face (one relaxed load unarmed). The gate
stays defense-in-depth (pinned: reached directly it refuses and counts).
Pin: `sym_shared_refs_tests::the_w1_ladders_decline_a_non_holders_patch_
as_a_counted_posture_decision`.

### 4.4q Defect 23 — FIXED (PR 12b's joiner under PR 8's grant window): a joiner's never-published mint was abandoned INTO the allocator's terminal-free gate — an ERROR per abandon and a leaked grant block

Found by the first green `sym-walls` run on the defect-22 binary
(`/tmp/grok-justin/pr13-walls3`, both rows' mechanism laws GREEN — the
rates venue-attributed, §1): the fleet's ERROR census
read ONE class left — `block free refused: this armed symmetric writer
does not hold the ALLOCATION LEASE …` (`free_block`'s `plane_gate`), 2
across two joiners, mid-rewrite. `BlockAllocator::abandon_unpublished_
offset` — the ACK-early overlay's superseded destination / a
failed-publish upload, a mint NO ledger ever named — has the co-writer's
lane recycle and the quiet counted abandon, then falls to
`self.free_block(offset)`; a JOINED appender is the `writer` posture,
so it took the terminal-free ladder and the gate refused (Err, the
caller's `let _ =`): one ERROR per abandoned mint and the block left SET
in the holder's bitmap with no reference and no window naming it — the
deferred leak release converges on it only after this mount's LEAVE
(PR 12b round 5's law: a live peer's window is adopted, the rest released
once every live peer declared). Fix: the recycle arm's GRANT-WINDOW face
— on a grant-armed allocator whose plane this mount does not hold, the
RAM reference goes, the incarnation word is retired, and the block is
given back to the window (`GrantWindow::give_back`: merged onto an
adjacent range, `consumed` un-counted, `installed` untouched — the next
lowest-first mint takes it, the leave's remainder returns it, a renewal
declares it inside the window); counted `block_grant_window_recycles`
(0 on every holder and every unarmed mount); a fenced era keeps the
quiet counted abandon; a second give-back of one block is the
double-handout lineage (refused, `cowriter_unpublished_abandons`). Pins:
`block_grant::tests::a_given_back_block_is_the_next_mint_and_merges_
onto_its_neighbours` and `sym_block_grant_tests::a_joined_appenders_
never_published_mint_returns_to_its_grant_window` (RED on the base: the
abandon's `Err` from the gate).

### 4.4r Defect 24 — FIXED (PR 7b under PR 12b): a joiner's `stat` of a striped directory folded every stripe's record off its PROJECTION — 256 `root-seq` restarts, EIO on the storm's create

`sym-scale` N = 8 from zero on `ef95bedc` (`pr13-batch5`): the create
storm on joiner m62 failed `EINVAL` at its 4,931st file — `tree 0 (slot
Some(103), root 0x1e00000@…, a PROJECTION here): traversal retry budget
exhausted … restarts [root-seq] = 256` — 1 s after the manager
auto-STRIPED `/` (the seven joiners' `mkdir`s were seven foreign creates
from seven creators — the PR 7b trigger; 61 stripes minted in the
manager's rotors, 3 supplied by m60/m61/m62). Defect 18's shape at a
DIFFERENT tree: `fold_striped_attrs` (`stat D` — every kernel attr
revalidation of `/`) and the rmdir's stripe count probe read every
stripe's record through `read_inode_value_routed` — the joiner's
projection of the manager's rotor slot trees, loaded at its join; the
lessee's compaction had retired a projected root's extent, `ReturnExtents`
+ a re-grant handed it to m62 (its barrier dropped the stale image at
09:05:03, the mint wrote there), and the pointer named another node's
seq. Defect 18's refresh cannot heal it: KD-SYM-3 — a LEASED slot's root
rides its lessee's PAGE, never tree 0, so `refresh_control_projection`
re-adopts tree 0 and the native tree only. The rule is defect 8's:
**a stripe is minted in ANOTHER appender's slot, so at every non-holder
its record is a FOREIGN read** — `dir_stripe::stripe_record` (`getattr`
through the writer's read divert, the holder's token plane; an own-slot
stripe and every unarmed mount read locally) is the ONE read the fold
and the rmdir probe run. Pin: `sym_n_daemon_tests::a_joiners_stat_of_a_
striped_directory_folds_the_stripes_at_their_holder` — RED `nlink 2 vs
3` (the base's fold saw no stripe record: the stripes were minted after
the reader's projection loaded). Standing, same class, not per-op:
`is_stripe`'s reverse dentry scan (`find_parent_of_child`) walks every
slot tree on the flip candidate's holder — over projections on a joiner
(§7 Owed).

### 4.4r' Defect 24's second face — FIXED: the fold read at the holder made the MANAGER's `stat /` fail for a dead supplier's whole death window

`sym-storm` from zero on `8d7fd3c0` (`pr13-batch7`, the batch whose
`sym-crash` ran 10/10 GREEN): the seven joiners killed; `/` had been
auto-striped under their `mkdir`s with THREE stripes they supplied; and
the harness's first `cat /mnt/…/m0/.stats` — the MANAGER's — read `EIO`:
`read token unavailable: the recall channel to the holder is not fresh`.
Defect 24 made every per-stripe record read a token read at the
stripe's holder — three holders were dead for the 15 s before the
recovery, and the fold failed the whole `stat /`; the kernel revalidates
`/`'s attrs on every path walk, so EVERY op under `/` at the manager
(`.stats` included) failed for the window. The fold is a DERIVED
attribute of an object this mount HOLDS, and a dead lessee's stripe
cannot move: a stripe whose holder cannot be reached contributes NOTHING
for the window (its `nlink` term, its times — bounded by the recovery,
which makes the slot the manager's), counted `dir_stripe_fold_
unreachable`; the stripe's DENTRIES stay exact-or-nothing (R-SYM-4 is a
law about a foreign object's user-visible metadata, not about a derived
term of an own object). Pin: `sym_n_daemon_tests::a_striped_directorys_
stat_at_the_holder_survives_a_suppliers_death` — RED with the fleet's
exact text. Harness: the storm's death window is exactly where the
harness reads every daemon's `.stats`.

### 4.4s Defect 25 — FIXED (PR 5's ledger poll vs PR 2/10/12b's checkpoint-class steps): a consumed checkpoint seq left a LEDGER GAP, and every token reader's poll stopped on it for a whole ring of checkpoints

`sym-storm` round 1 from zero on `ef95bedc`: the seven joiners killed
at 09:13:09; their regions recovered 09:13:22–27 (`Unleased` in tree 0,
`64 slot(s) released` × 7 × 2 volumes); the acked-writes oracle GREEN;
then at 09:14:08 the token reader m1 could not `stat` a recovered file —
`read token unavailable: the recall channel to the holder is not fresh`
— its tree 0 STILL naming appender 1 as the lessee 41 s after the
release; live 27 minutes later it resolved. The mechanism, verified on
the code: `release_recovered_regions` (PR 10), `grow_stalled_regions`
(PR 2), the in-process leave and the wire `LeaveAppender` (PR 12b) each
CONSUME a checkpoint seq for their bitmap write ("ledger slots are seq %
32, so the gap is harmless") — and PR 5's predicted-slot poll
(`read_newest_ledger_from`) reads slot `(adopted + 1) % 32` and STOPS on
an older record there ("the writer has not written that seq"): a gap of
one parks the reader until the writer's seq wraps the ring (32
checkpoints ≈ 32 s at the shipped cadence — UNBOUNDED on a quiet
writer); the storm's seven releases at 09:13:23 parked m1 before the
09:13:27 releases landed, and every read of the recovered slots dialed
the dead lessee for the whole window. Two halves, one law: **a consumed
seq is a ledger seq** — `KvMetaBackend::consume_checkpoint_seq_for_
bitmap` writes the bitmap at `ckpt_seq`, then a record at `ckpt_seq`
RESTATING the last cycle's word (the roots as they stand, the last
record's tail, `next_ino`, the watermark) under the SMO mutex (no cycle
mid-flight: content-equivalent to the record it follows, so a crash
after it replays exactly what a crash after the last cycle would);
`meta_kv_ledger_restatements` counts them (0 on a flat mount). And **the
belt**: a crash between a bitmap write and its record leaves one gap for
the volume's life, so after `ROOT_LEDGER_SLOTS` consecutive stopped polls
the reader reads the whole ledger once and adopts the newest record
anywhere (`meta_kv_revalidate_gap_scans`; a truly idle writer costs one
128 KiB read per 32 idle polls). Pins: `sym_n_daemon_tests::a_readers_
ledger_poll_walks_across_a_consumed_checkpoint_seq` (a joiner's wire
leave then the manager's next cycle; RED `None` between seqs 11 and 13
— the walk stopped) and `sym_coherence_tests::a_readers_poll_scans_the_
whole_ledger_after_a_ring_of_stopped_polls` (a forged gap; RED 0 scans).
The reader's own staleness bound (S5's `interval + ceiling`, PR 5's 0
for metadata) holds again.

### 4.4t Defect 26 — FIXED (PR 12b's mount path): a successor remounting inside a killed manager's exit window JOINED the dying listener and the mount refused

`sym-crash` round 1 from zero on `ef95bedc`: `mount 0` after the
manager's kill -9 — `dialing the manager at …:39623 for JoinAppender
failed: Connection reset by peer`, the successor remount FAILED.
`symmetric_join_target` calls a heartbeat-fresh claim whose pid is not
yet provably dead a LIVE manager (the D0 ladder's own word); a `kill -9`
returns before a daemon with gigabytes of dirty pages has exited, the
harness remounted inside that window, and the dying process's listener
accepted-then-reset the dial (RST, not ECONNREFUSED — the process was
still there). The join's transport failure was `KvError::Busy` — the
class of a manager that REFUSED — so the mount refused. Fix: the dial and
the `JoinAppender` call itself (the open's first act — nothing of the
join exists yet) answer `KvError::ManagerUnreachable` (errno
`EHOSTUNREACH`, `meta_backend::join_dial_failed`), and the mount path
re-reads the join target ONCE on it: a manager the probe no longer calls
live (the pid gone — the dead-pid proof) makes this mount the D0
ladder's; a manager still live-looking keeps the refusal (a cross-host
crash waits the claim's TTL exactly as the D0 ladder always did).
Harness: `mw_fleet.sh kill` waits for the victim's pid to vanish
(bounded 60 s, the exit wall logged) — a supervisor's restart never
starts inside the exit window. Pin: `sym_n_daemon_tests::a_join_at_an_
unreachable_manager_is_the_transport_class_the_mount_path_retries`.

### 4.4u Defect 27 — FIXED (PR 8/12b, under PR 14's owed terminal-free-engine swap): a former lessee's blocks were "lost" to fsck at the manager, and a local free of one REFUSED — leaked

`sym-scale`'s fsck oracle from zero on `f38776a7` (`pr13-batch8`,
attempt 8 — attempt 7's oracle had recorded NO block-plane verdict: `0
block(s), 0 refcount(s) checked`): after every joiner's clean leave the
manager's online fsck raised **2,816 C2 "lost block … referenced offset
is not allocator-tracked"** on `nvme3n1`, every one an ingest block of a
departed joiner (128 per 512 MiB file). The manager's RAM refcount map
knows its OWN mints and its mount-time by-block census alone (PR 7 kept
the scan as the free list's derivation; PR 12b's joined open performs
none); a block a joiner minted from its grant window is SET in the
holder's bitmap (PR 8 — the bitmap IS the free list there) and durably
referenced, and once the joiner's slot is released, handed over or
recovered to the manager, its tree is the manager's to read and its
blocks are untracked THERE. Two faces: fsck's C2 read RAM alone — a
FALSE finding class on every N-daemon set with departed writers; and the
LEAK — `begin_free` on such a block is the double-release REFUSAL (an
ERROR per block, `block_untracked_free_refusals` — the must-stay-≈0
tripwire — and the bit SET for ever), so every `rm` at the manager of a
file a departed joiner wrote leaked its blocks (unexercised by the legs:
their deleted files are inline). Fix: (i) `BackendRouter::untracked_free_
gate` — a terminal free at the allocation HOLDER of an untracked offset
runs `cowriter::execute_shipped_frees`' ladder LOCALLY (finding 13's law
for a shipped free of an untracked offset: the durable ledger population
decides; 0 ⇒ seed one reference and run the ladder; > 0 ⇒ non-terminal;
already free ⇒ the refusal), installed by `arm_shared_refs` beside the
shared-block gate, counted `block_untracked_free_adjudicated`; (ii) fsck's
C2 on a grant-armed HOLDER reads a SET bit in the held bitmap as TRACKED
(`fsck_alloc_bitmap_tracked_exempted`). Pin: `sym_shared_refs_tests::
a_holders_free_of_a_former_lessees_block_runs_the_owner_ladder_instead_
of_refusing` (RED: the refusal, the bit SET). The engine swap itself —
the bitmap as the terminal-free engine everywhere, the zero-census open —
stays PR 14's (§7).

### 4.4v Defect 28 — FIXED (PR 5/12's reader under PR 12b): a token reader failed closed on `NotHolder` for the whole poll interval after every grant

`sym-storm` round 4 from zero on `f38776a7` (rounds 1–3 GREEN — the
recalled-reader arm now GREEN with defect 25 landed): the reader's
pre-kill resolve of the rejoined m63's first acked file read `EIO` —
`NotHolderRedirect { object, holder: 2 }` / `{ holder: 6 }` in m1's log.
A rejoined joiner's slots are granted at `g + 1` by ring-0 control
entries; the reader's tree 0 carries them only after the manager's
checkpoint and the reader's next poll, and PR 5 made the reader FAIL
CLOSED on the redirect until then ("R-SYM-4: exact or nothing") while PR
12b round 3 made the WRITER's divert follow it once. The redirect's
`holder` is the LEASE's word — fresher than any ledger record — so the
reader follows it once too: `KvMetaBackend::reader_plane_for_holder`
(the per-holder half of `token_reader_for`, factored) dials the named
holder's plane through the reader's per-holder binding (holder 0 = the
manager's own plane; a dead lessee answers `HolderDead`, never a
redirect, so the dial is bounded), `dlm_token_reader_redirects_followed`
counts it. Pin: `sym_n_daemon_tests::a_token_reader_follows_a_not_holder_
redirect_to_the_lessee` — the joiner's slots granted AFTER the reader's
last poll, its file resolves at the reader without a poll (RED: `EIO`).

### 4.4w Defect 29 — FIXED (PR 6 under PR 12b): a shipped step's `SlotBusy` at a holder whose slot had just moved to the INITIATOR was classified as a local device error — the initiator FAIL-STOPPED both volumes

`sym-storm` round 1 from zero on `649d80f7` (`pr13-batch9`; `sym-crash`
10/10 GREEN before it): the "deleted stays deleted" arm read
`storm-w64-r1` still resolving through SEVEN mounts — the rejoined m64's
`rm -rf` of its recovered round directory had FAILED, and m64's own
`stat` answered `EIO` ("Metadata volume 0 is disabled"), which the arm
counted as "gone". m64's log: `cross-volume transaction … failed at step
0 of 2 (forest slot 679 is leased by appender 5 (g 3) — a mutation of a
foreign slot ships to its holder … EAGAIN) — volume(s) [0, 1] are
fail-stopped` — appender 5 IS m64. The directory's slot 679 had been
recovered to the manager at m64's death; m64's removals into it shipped
to the manager, whose dominance rule (defects 9/10's window, fed by the
served ships) handed the slot to the dominating requester — m64 itself —
mid-plan (`slot 679 released by appender 0 (g 2)` one line before); the
manager's commit door then answered the shipped step `SlotBusy { 679,
holder 5 }`, and the initiator's classifier `is_ship_failure` RE-RESOLVED
the step's home to decide the class — which now read `Local` — so the
shipped refusal was taken for a local mid-plan device error, the S3.5
lattice latch fired and both volumes fail-stopped (`crossvol_tx_midplan_
escalations`); every later op at m64 read `Metadata volume 0 is
disabled`. Fix: (i) `apply_or_ship_step_retrying` — a holder's
`SlotBusy` at a SHIPPED step is defect 11's class at the step (the slot
moved between the plan and the apply): re-resolve it at the manager and
dispatch again, locally when it is ours now, to the new holder otherwise,
bounded at 2 (`xv_cross_owner_step_slot_moved_retries`); (ii) a step's
failure is classified by the mode it was DISPATCHED in (read before the
dispatch), never by re-resolving its home after the fact — in `execute`
and in the roll-forward; `is_ship_failure` is deleted. Seam `TEST_XV_
SERVE_SLOT_BUSY_ONCE`; pin `sym_cross_owner_tests::a_shipped_step_
refused_slot_busy_at_its_holder_is_redispatched` (RED: `EAGAIN` with the
intent left open — the fixture's home does not move, so the base
classified it as a ship failure; on the fleet the same refusal was the
fail-stop). Harness: the "deleted stays deleted" arm now reads ONLY
`ENOENT` as deleted — an `EIO`/`EAGAIN` from a mount is a red naming it.

### 4.4x Defect 30 — FIXED (PR 6/12b — defect 29's LOCAL arm): a namespace op dispatched LOCALLY whose slot another appender took between the plan and the door surfaced the door's `SlotBusy` as `EAGAIN` to the application

`sym-storm` round 2 from zero on `5b0ec0be` (`pr13-batch10`; every other
leg GREEN, `sym-crash` 10/10 GREEN — the fourth from-zero 10/10 — and
round 1 GREEN: 10,608 acked, 14 regions recovered in 47 s, the reader
arm exact): `mkdir /mnt/sqz-mwfleet/m63/storm-w63-r2` answered
`Resource temporarily unavailable`; m63's log has exactly one line —
`Refused { errno: 11, msg: "forest slot 409 is leased by appender 0 (g
3) — a mutation of a foreign slot ships to its holder … retry (EAGAIN)"
}`. The schedule: `/` was auto-striped in round 1; the name's stripe
lives in slot 409, which m62 held at `g 2` and LRU-released two seconds
earlier (`slot 409 released by appender 3` at 13:28:19 in a wave of ≈ 40
releases per joiner — round 1's stripe and directory slots past the page
budget after the recovery and rejoin); the manager first-touched it at
`g 3`; m63's projection still read it UNLEASED, so `spans_foreign_slot`
said "local" and the create took the plain path — its commit door's wire
first touch (`joined_acquire_slot`) lost to the manager, LEARNT the
holder into the projection (PR 12b's `SlotRefused` arm), and returned
`KvError::SlotBusy` → `EAGAIN` straight to `mkdir(2)`. Defect 29 made a
SHIPPED step's `SlotBusy` re-dispatch; the LOCAL step's twin was never
retried anywhere. Fix: `RoutedMetaBackend::redispatch_once_on_slot_
moved` at the four namespace verbs' trait entries (`create`/`mkdir`
through `create_with_rdev_preset`'s non-preset arm, `unlink`/`rmdir`
(`unlink_body`), `link` (`link_body`), `rename` (around `rename_body`
inside the lease loop)): the door refuses BEFORE ring admission and any
node lock (nothing applied; a fresh mint burned — §4.8's law), and its
reply taught the projection the holder, so the op run again reads the
slot foreign and takes the cross-owner arm — ONE re-run, counted on
`xv_cross_owner_op_slot_moved_redispatches`; a second refusal is the
retryable class the caller sees. Pin `sym_n_daemon_tests::a_locally_
dispatched_create_whose_slot_the_manager_took_is_redispatched_through_
the_cross_owner_arm` (the joiner's projection reads the seeded slot
unleased, the manager first-touches it, the joiner's create — RED:
`EAGAIN "forest slot 4 is leased by appender 0"`; GREEN: lands at the
manager after one re-dispatch, the next `mkdir` into it ships on its
first run).

### 4.4y Defect 31 — FIXED (PR M6's pending-times drain under PR 4's slot leases): one parked refinement on an ino whose slot had MOVED refused the manager's every drain — every `fsync` of the manager's own files answered `EAGAIN`

The same round's manager log: `kv pending-times drain failed on
/dev/nvme2n1: forest slot 474 is leased by appender 3 (g 2)` — 519 WARNs
in 8 s at the drain cadence — and **70 `FUSE Fsync failed for ino …`**
with the same text: the manager's `dd conv=fsync` files were REFUSED
their fsync (the acked-writes oracle counts only fsynced names, so it
read no loss; the manager's writes were not durable-on-demand for the
rest of the round). Mechanism: `drain_pending_times_now` stages every
parked refinement of the volume into ONE `KvTx`; PR 4's door judges the
transaction by its records' slots and refuses the WHOLE batch on one
foreign slot (`SlotBusy`), so one refinement on an ino whose slot moved
to another appender (the pending map is RAM — a slot handover, an LRU
release or a dead appender's recovery moves the slot and leaves the
refinement behind) wedged every later drain until a remount emptied the
map. Fix, two laws: (a) **the flush-then-transfer drains the departing
slot's refinements FIRST** (`transfer_slot_locked` step 0, before the
gate goes `Releasing` — after it the door would park the drain's commit
on this very handover): the ordinary path leaves nothing behind; (b)
**the drain PARTITIONS by slot** — a refinement whose slot another
appender leases is retired and counted (`meta_kv_times_echo_foreign_
dropped`, 0 on every unarmed mount), never staged, and the own
refinements beside it commit. Pin `sym_n_daemon_tests::a_released_slots_
pending_times_are_drained_first_and_a_foreign_slots_are_dropped_not_
wedged` (RED before on both laws: `pending_times_len() == 1` after the
release; `Err(SlotBusy)` from the drain with the own refinement never
durable).

### 4.4z Defect 32 — FOUND, NOT FIXED HERE (PR 6/12's owed record-level metanode arm; a FLIP BLOCKER): a file's `setattr` / `setxattr` / DATA WRITE from a mount that does not lease the file's slot is refused or silently not durable

> **Status (PR 13b, `feat/sym-metanode-ship`, 2026-09-21): FIXED — the record-level metanode ship landed in `cd85f701` (`src/meta_backend/record_ship.rs`; the S9 publish plane's `PublishTarget`; the interim `EREMOTE` refusal, its write belt and `foreign_file_mutation_refusals` deleted; the `#[ignore]`d pin `a_joiners_setattr_of_the_managers_file_lands_at_the_holder` un-ignored — RED on the base, GREEN on the fix — beside the reverse and the three-daemon pins; the fidelity `sym-join-ladder` contract flipped to the landing law in `53ffb626`). **The `sym-foreign-file` fleet leg then found two more defects on the SAME face, each fixed red-first:** (a) `85e42408` — an ACKED-WRITE LOSS: a WRITER is a token client of the holders it writes to (PR 12b's divert), and `MountRecallSink::purge_scoped` dropped the recalled object's layout entry UNCONDITIONALLY (PR 5's reader law) — the kernel's post-write times echo shipped as a `Setattr`, the holder's commit recalled the writer's own token, the purge dropped the inline append's `layout_dirty` entry (its `data_key` the only copy of the acked bytes), and the `fsync` found nothing to save and returned 0; the sink now keeps a dirty entry under the ino's non-parking (3.5) guard (`dlm_token_recall_{dirty_kept,discard_skipped}`); (b) `d0872ace` — the HOLDER read a served publish's OLD bytes for the inode's life: its own caches (router entry, attr cache) and, under `FUSE_WRITEBACK_CACHE`, the kernel's inode (size/mtime/ctime are the kernel's for a cached regular inode — `fuse_get_cache_mask`); a served mutation now invalidates the holder's view and pushes `FUSE_NOTIFY_INVAL_INODE` + `FUSE_NOTIFY_PRUNE` (uapi 7.45; the fork gained the notification) over the classical sideband (`served_mutation_{invals,prunes}`). The leg reads GREEN ×3 from zero on `494ba885` (ships 768 ≡ served 768, refusals 0, publish 192 ≡ 192, fsck clean). The text below is the finding as recorded.**

Found by a scoping probe on the live fleet while attributing defects
30/31 (not by a gate — no leg mutates a foreign-slot FILE; the legs'
cross-owner ops are the namespace verbs, which PR 6 ships). Joiner m60
`mkdir /probe-a && echo hello > /probe-a/f60 && sync -f` (the directory
and the file in m60's slot); from joiner m61: `chmod 640 /probe-a/f60`
→ **`ENOENT`**; `touch /probe-a/f60` → **`ENOENT`**; `setfattr -n user.x
-v 1 /probe-a/f60` → **`EOPNOTSUPP`**; `dd … conv=fsync,notrunc` → the
write acked, `dd: closing output file: No such file or directory` (the
fsync's publish refused), m61's own view 8,201 bytes, **m60's and the
device's 6 bytes**; `echo appended >> /probe-a/f60` → **rc 0 and the bytes
gone** (no fsync, the close's error unread by the shell). The namespace
half beside it is exact: `echo x > /mnt/…/m61/probe-a/f61` (a create INTO
m60's directory) lands and reads at m60. The design states the arm
twice — the door's own refusal text ("a mutation of a foreign slot ships
to its holder — the metanode arm; PR 6/12") and §5.10's row **"`write` to
a FOREIGN-owned file (holder live): 1 custody grant + 1 publish ship per
layout publish"** — and AGENTS carried it as owed from PR 5 ("the
metanode ship for a foreign slot's mutation (PR 6/12)") until PR 12b's
owed list, which no longer names it: an owed item fell off the ledger,
and PR 9 built the custody GRANT at the slot holder without the PUBLISH
ship that makes the grant useful. What exists: the S8 `MetaShipRouter`
verbs (`setattr`/`setxattr`/`removexattr`), the S9 publish plane
(`PublishClient` → `PublishService` under the owner's custody scope) and
PR 12's per-holder endpoint binding — all keyed today on the S8 `OwnerMap`
by VOLUME, which PR 12's `arm_authority_planes` sets all-local under the
plane (`dlm_rpcs == 0` by construction). The fix shape: `daemon_verb_
router` and the publish shipper resolve a FOREIGN-slot ino's holder
through PR 6's `step_home` (tree 0's lessee + the endpoint table) and
ship the record-level verbs there; the served side applies under the
holder's lease and door, recalling the object's tokens (the writer's own
included) — the row §5.10 already prices at 3.6 verbs/MiB. Its venue is
this suite's two-backend fixture (a joiner's `setattr`/`write` on the
manager's file, and the reverse). **Not built in this rung**: a rung-sized
item (two shippers re-keyed by slot holder, the served publish's custody
composition with PR 9's grant, the un-share of PR 7 beside it), stated
here as the FIRST flip blocker (§9) — the flip cannot ship `chmod` of a
colleague's file answering `ENOENT` and `>>` losing bytes. **Fix round 1
(Issue 12):** the contract PR 13b lands against is written and
`#[ignore]`d RED — `sym_n_daemon_tests::a_joiners_setattr_of_the_managers_
file_lands_at_the_holder` (the two-backend fixture: a joiner's `setattr`
of the manager's file lands and the manager reads it) — and the armed
plane REFUSES the class LOUD until then: `RoutedMetaBackend::refuse_
foreign_slot_file_mutation` answers the typed `SqueezefsError::
ForeignSlotFileMutation` (`EREMOTE` since fix round 2 — §4.4ai; round 1's
`EOPNOTSUPP` is the class coreutils' `chmod`/`chown` swallow — naming PR
13b, the slot and its holder) at the routed `setattr` / `setxattr` / `removexattr` entries
BEFORE any read of the record, at both layout publish entries and per
member of the publish group, and in the FUSE `write` handler before a
byte is accepted — so `chmod`/`touch` never answer `ENOENT` for a file
that exists and `setfattr` answers the deliberate word;
the kernel's SETATTR times ECHO (ctime-only, mtime unchanged — what
every writeback lands) is ABSORBED against the holder's record instead
of refused (`foreign_slot_setattr_gate`, `4523f25e`; gauge
`foreign_file_times_echo_absorbed` — the storm's oracle had read 14 k
refusals per round of exactly this echo); gauge
`foreign_file_mutation_refusals` (0 unarmed by construction); pinned
red-first by `a_foreign_slot_files_record_mutation_refuses_loud_naming_
pr_13b_until_it_ships`. **Fix round 2 (Issue 22 — the `write` gate's
claim was FALSE on the default mount):** the default mount negotiates
`FUSE_WRITEBACK_CACHE` (`--no-writeback` is the opt-out; interception
mounts force write-through), so an application's `write(2)` lands in the
kernel's page cache and RETURNS 0 — the daemon sees the `FUSE_WRITE`
only at writeback (`fuse_flush` at `close`, `fsync`, dirty pressure) and
the `write` gate's refusal reaches the application through the kernel's
errseq at `fsync(2)`/`close(2)` (POSIX-16's class); a shell `>>` whose
close status nobody reads still printed rc 0 with the bytes gone, the
exact pre-fix symptom above. **The interim gate therefore also sits
where a shell CAN see it:** the FUSE `open` handler refuses an `open(2)`
of a FOREIGN-slot file that carries WRITE INTENT (`O_WRONLY` / `O_RDWR`
/ `O_TRUNC` / `O_APPEND` / `O_CREAT` — `meta_backend::OPEN_WRITE_INTENT`,
the S5 reader gate's own mask) with the same typed refusal naming
PR 13b (`RoutedMetaBackend::refuse_foreign_slot_open`, behind the plane;
an own-region slot exempt per `b8c4c92a`; a read-only open untouched; a
`-o ro` reader unchanged — the kernel refuses its writes first), so `>>`,
`dd`, `truncate` and every `O_WRONLY` open fail LOUD at the open and no
write is ever acked; the `write` / `setattr` / `setxattr` / publish gates
stay as the BELT for an fd opened before the slot moved and for the il
shim's ring WRITES (its fd was opened through the FUSE `open` gate; only
the writes bypass the `write` handler). The honest
statement of the class: **the open refuses; a write that reaches the
`write` handler is refused at `write(2)` only on a `--no-writeback`,
`O_DIRECT` or `O_SYNC` path, else at `fsync`/`close` through the kernel's
errseq — and a shell redirection that ignores `close`'s status reports
success**. Pinned on the two-backend fixture (the same contract's OPEN
face: five write-intent flag sets refused and counted, `O_RDONLY` passes
uncounted, an own-slot `O_RDWR|O_APPEND` passes) and LIVE on the fidelity
tier's `sym-join-ladder` leg (three real daemons on kernel nvmet: joiner
2's `echo >> f7` into joiner 3's file fails at the open with "Object
is remote", `chmod` refuses AND exits nonzero (§4.4ai — its first run
exited 0 on `EOPNOTSUPP`), the mode stays 644 at all three daemons,
both mounts still read the six bytes, `foreign_file_mutation_refusals`
≥ 2 at the refusing joiner) — the
`sym_convert_fuse_tests` precedent is a ONE-mount suite and cannot stand
a second daemon, which is why the live pin rides the fidelity leg. **Two
shapes the interim refusal answers `EREMOTE` for a slot legitimately
about to be THIS mount's — both transient, both PR 13b's:** a slot
mid-handover TO this mount (`Offered` / `Releasing` resolve to the
DEPARTING holder for the handover's milliseconds, where `EAGAIN` would be
the honest word), and a JOINER whose lease PROJECTION lags a release (the
slot reads leased-to-the-old-lessee until the projection refreshes; the
door's first touch would have acquired it). Both vanish with PR 13b's
ship, whose served side applies under the holder's lease and needs no
local verdict.

### 4.4aa Defect 33 — FIXED (PR 2's KD-SYM-10 audit × PR 10's recovery): a manager leaf dirty when a dead appender's recovery took the SMO mutex aged past the landing ceiling BY DESIGN — every recovery that met one tripped the must-stay-0 gauge

`sym-storm` round 4 from zero on `8c992af6` (`pr13-batch11`; rounds 1–3
GREEN, `sym-crash` 10/10 GREEN — the fifth from-zero 10/10 — every
other leg GREEN incl. `sym-walls` with 0 overruns): every law of round 4
GREEN (7 region sets recovered in 16 s, 17,094 acked files present, the
reader arm exact) and then `appender_flush_ceiling_overruns=1 on m0`;
the manager's log: `flush ceiling OVERRUN — appender region(s) [(0,
1101)] … exceeded the 1100 ms landing ceiling` — ONE millisecond past,
on the manager's own region, during the seven-region recovery. This is
§4.5's class with its mechanism finally named: the recovery driver
holds the volume's SMO mutex through its per-region steps 4–7
(`recover_region`: the release marks, the replay, the flush cycles, the
tails, tree 0 — under the mutex so no manager SMO on the trees runs
while their custody moves), and the flush pass that would cover a
manager leaf dirty at that instant WAITS for the mutex; the recovery's
own published bound (`appender_recovery_bound_ms` = 1,207 ms at the
fleet's shape — the ring ÷ 230 B entries × three leaf passes + the
landing ceiling) exceeds the landing ceiling's fixed 100 ms margin
(`CHECKPOINT_MAX_AGE_MS` + 2 ticks) by an order of magnitude, so a leaf
that went dirty ≤ 100 ms before a recovery began lands late by the
recovery's wall — a BOUNDED, PUBLISHED amount, and every earlier
overrun of this rung (`sym-walls` row (a) at 1,105 and 1,227 ms while
the manager served the joiners' frees and grants under the same mutex)
is the same shape. The must-stay-0 law and the recovery's mutex hold
were in direct conflict on the manager. Fix: the audit
(`note_flush_ceiling`) judges a leaf whose dirty window a recovery hold
OVERLAPPED (a hold in flight at the barrier, or one that ended after the
leaf went dirty — `KvMetaBackend::recovery_hold`, an RAII the driver
takes right after the mutex and drops before it) against **`ceiling +
appender_recovery_bound_ms`** — both derived, both published — counting
it on `appender_flush_ceiling_recovery_extensions` inside that bound;
past it the barrier is still an overrun, and a barrier no recovery
explains keeps the shipped law verbatim. No reader guarantee moves:
under the plane every reader is a token client (exact), and the `=0`
posture has no recovery. Pin `sym_crash_matrix_tests::a_manager_leaf_
that_aged_under_a_recoverys_hold_is_a_counted_extension_not_an_overrun`
(the recovery parked under its hold, a manager commit under it aged
1.3 s, the covering cycle — RED before: `flush_ceiling_overruns == 1`).
The `sym-walls` overruns at 1,105 / 1,227 ms were under the manager's
grant/free service, not a recovery: the SMO-mutex holds there are PR 3's
grant carve and PR 4's transfer — the same class the box row will read;
if it trips there, the margin derives from the measured pass wall (PR
14's item stays).

### 4.4ab Defect 34 — FIXED (PR 12b's projection refresh — defect 18's second face): a joiner's PROJECTION spun its budget on `child-seq` restarts against a recycled CHILD, and the refresh arm keyed on ROOT restarts alone never fired

`sym-storm` round 4 from zero on `7ac89d24` (`pr13-batch12`; rounds 1–3
GREEN — 15,460 / 12,427 / 8,917 acked, recoveries 16–91 s; every law of
round 4 GREEN: 7 region sets recovered in 16 s, 11,062 acked present,
the reader arm exact): the "deleted stays deleted" arm's `stat` of the
removed `storm-w62-r4` through m60 answered **`EINVAL`**, and m60's log
has the class's exact tally — `tree 8 (slot None, root 0x19580000@…, a
PROJECTION here): traversal retry budget exhausted … restarts
[root-retired, root-seq, routing-hole, child-retired, child-seq] = [0, 0,
0, 0, 256]`. Defect 18's fix (`KvTree::descend`'s refresh arm) counted
ROOT restarts (`root-retired` + `root-seq`) toward the refresh cadence,
on the reasoning that a projection's staleness is a stale root POINTER.
It is also a stale root IMAGE: the manager's compaction of a CHILD leaf
of tree 0 (a dead round's directory records folded, the leaf rewritten
into a fresh extent) flips the parent pointer IN THE ROOT'S OWN LOG — the
same node, the same `node_seq` — so the joiner's cached root image
passes both root checks and its old child pointer names an extent the
manager freed at its checkpoint and re-granted (the storm's 7 rejoins
claimed their grants there), where a node under ANOTHER seq now sits:
every restart is `child-seq`, the root is re-read from the projection's
own cache (`try_get` serves the stale image), and nothing refreshes it.
Fix: on a PROJECTION every restart class counts toward `PROJECTION_
REFRESH_EVERY` (the sum of the five tallies); a writer's own tree keeps
the root-restart tally alone (a child restart there is a racing SMO's
window, converging). The refresh (`refresh_control_projection`) drops
the projection's images WHOLE and re-reads the root — the remedy for a
stale image exactly as for a stale pointer. Pins in `tests/sym_
projection_refresh_tests.rs` on the child shape (TWO `NodeCache`s over
one file — the manager's writes the interior tree, compacts the leaf,
checkpoints the flipped root; the joiner's holds the pre-flip root image
and drops its leaf; tree B re-claims the leaf's extent under a fresh
seq): `a_projection_whose_child_was_recycled_exhausts_its_budget_
without_a_refresh` reproduces the fleet's `[0, 0, 0, 0, 256]` exactly,
and `…_follows_the_manager_through_the_refresh` is RED on the root-only
count (the hook never consulted, `Corrupt`) and GREEN on the sum;
`KvTree::descend_leaf_addr_for_test` is the harness accessor.

### 4.4ac Harness — the PAUSED phase's premise, and the one law re-learnt

> **Superseded in part by §4.4aj (2026-09-22):** the "paused job" of this
> phase never ran — its storm died at its first `mkdir` on every attempt
> (the root was never created) — so the touched slot's IDLE reading below
> came from an EMPTY job, not from `T_idle` expiring over a live one; the
> pacing fix this entry describes stands (it fits the phase inside the
> holder's window), but the premise attribution ("the job was IDLE by the
> design's own definition") is void: nothing was ever live.

`sym-foreign-touch` PAUSED on attempt 12 read `slot_handovers=1` — "a
single touch per beat moved a paused job's tree": the manager's slot 24
(its `job-w0` tree, 1,518 inos) was OFFERED on the idle arm
(`slot_offers_idle` +1) at the requester's third touch. The design's
premise for "a paused LIVE job keeps its tree" is `ops_h(T_idle) ≫ 2 ×
ops_q` — a pause SHORTER than `T_idle`; the fleet's `T_idle` is its
membership lease TTL (15 s, `--lease-ttl-ms=15000`), and the phase ran
its three touches at the 10 s membership beat (31 s), so at the third
touch the job was IDLE by the design's own definition and `N_floor`
decided alone — a derived ratio (`ewma_handover / ewma_ship`) that reads
2–5 on this box (it moved 7 → 5 across the phase; a served ship that
costs as much as a handover on a hot box makes moving the right call).
Eleven earlier runs passed on a higher `N_floor`. Not a product term:
the phase now paces its touches inside the holder's window (`paused_beat
= min(beat, (T_idle − 3 s) / (rounds + 1))`, read off
`membership_lease_ttl_ms`) and refuses a fixture that cannot fit. The
fix was written while attempt 12's `sym-crash` ran the script and
reverted within a minute — the shifted-tail exit 2 after a GREEN verdict
(`line 9184: syntax error near unexpected token ')'`) is the harness law
§4.6 states.

### 4.4ad Defect 35 — FIXED (PR 6 under PR 12b — defects 29/30's MID-PLAN arm): a LOCAL step's `SlotBusy` inside a cross-owner plan was a "device error" — the S3.5 lattice FAIL-STOPPED the initiator's volumes

`sym-storm` round 4 from zero on `87461d56` (`pr13-batch13`; rounds 1–3
GREEN — 30,857 / 11,507 / 17,682 acked, recoveries 16–46 s; `sym-crash`
10/10 GREEN — the seventh from-zero 10/10): the round's explicit stripe
flip on a joiner refused, its every op `Metadata volume 1 is disabled`,
and the joiner's log: `cross-volume transaction … failed at step 1 of 3
(forest slot 10 is leased by appender 7 (g 3) — a mutation of a foreign
slot ships to its holder … EAGAIN) — … volume(s) [0, 1] are fail-stopped
until then (crossvol_tx_midplan_escalations)`. The `--cross-owner`
mover's rename into the manager's directory planned three steps; step
1's destination slot read UNLEASED in the joiner's projection (the
manager had released it — `slot 10 released by appender 0 (g 1)` a
moment before — and appender 7 first-touched it at `g 3`), so the step
was dispatched LOCALLY and the door's wire first touch lost; defect 29's
classifier ("by the mode the step was DISPATCHED in") read a local
failure as a device error, and the S3.5 lattice latched both volumes.
Defect 30 covered the op-level arm (a create/unlink/link/rename whose
FIRST commit hit the door) and defect 29 the SHIPPED step's; the local
step INSIDE a plan was the third face. Fix: `apply_or_ship_step_
retrying` re-dispatches a LOCAL step's slot-moved refusal exactly as a
shipped one's (re-resolve → the step ships to the holder the door
named; bounded), and `execute`'s classifier treats a slot-moved refusal
as the RETRYABLE class whatever the dispatch mode (the door refused
before any effect; the applied steps stand; the intent's roll-forward
completes the plan) — the lattice guards device errors, never the plane
moving a slot. Pin `sym_n_daemon_tests::a_local_steps_slot_busy_mid_
plan_redispatches_and_never_fail_stops_the_initiator` (the joiner's
rename from the manager's directory into one whose slot the manager
first-touched after the projection loaded — RED: `EAGAIN` and
`Metadata volume 0 is disabled` at the joiner; GREEN: the name lands at
the manager after one step re-dispatch, `crossvol_tx_midplan_
escalations` flat, the joiner still writes). The PAUSED phase's
harness premise (§4.4ac) is narrowed once more: the holder's window is
two half-`T_idle` buckets on an absolute clock (`HolderOps::total`), so
the phase fits `T_idle / 2`.

### 4.4ae Defect 36 — FIXED (PR 12b's projection refresh, made reachable by defect 34's fix): the refresh walked tree 0 itself and a restart INSIDE that walk fired the refresh again — three rejoined joiners ABORTED on a stack overflow

`sym-storm` round 5 from zero on `24bbb195` (`pr13-batch14`; rounds 1–4
GREEN — 12,315 / 10,649 / 20,148 / 17,254 acked, recoveries 16–46 s;
`sym-crash` 10/10 GREEN — the eighth from-zero 10/10; `sym-walls` GREEN
with 0 overruns): the deleted-stays-deleted `stat` through m64 answered
`Transport endpoint is not connected` — m64, m65 and m66 (the round's
rejoined victims) had ABORTED: `thread 'fuse3-tpc26m0' (…) has
overflowed its stack — fatal runtime error: stack overflow, aborting`,
each right after `AcquireSlot failed on the wire (the coordinator closed
the session) — re-dialing the manager and retrying once`. The re-dial
refreshes the projection (`refresh_control_projection`), and the refresh
WALKS tree 0 itself — `load_slot_leases` → `KvTree::range` → `descend`;
with defect 34's fix every restart class inside that walk counted toward
the refresh cadence, so a walk that met 8 racing restarts (a manager
SMO under the storm's 7 rejoins) fired the refresh AGAIN from inside the
refresh, which walked again — unbounded recursion until the handler
lane's stack was gone. (Before defect 34 the same recursion existed for
ROOT restarts alone and never met one inside the fresh-rooted walk.)
Fix: the projection refresh is SINGLE-FLIGHT per cache
(`NodeCache::begin_projection_refresh` / `end_projection_refresh`, a CAS
around the hook): a restart inside a refresh in flight — its own walk,
or a concurrent walker's — skips the arm and keeps restarting on the
root the refresh installs. Pin `sym_projection_refresh_tests::a_
projection_refresh_never_nests_inside_its_own_walk` (a hook that walks
the stale projection before re-installing the root, counting its
nesting depth — RED: `max_depth == 2`, the nested refresh observed and
cut where the product overflowed; GREEN: 1, the lookup served).

### 4.4af Fix-round finding 1 — FOUND, NOT FIXED (the death path under `--victims=7 --cross-owner --striped`; a FLIP BLOCKER beside defect 32): 25 of 17,376 acked files lost across a seven-victim kill — every one a moved file of ONE writer, at neither its source nor its destination

> **Status (PR 13b, 2026-09-21): ATTRIBUTED and FIXED in `cc642b6a`.** The stride was ONE rotor slot's population (with two metadata volumes a joiner's files alternate volumes, and the volume where the parent stripe does not live mints by the rotor round-robin — `rotor_mints = [940, 65]` on m60; 64 rotor slots ⇒ stride 128); the slot, 3549, was recovered with "NO page entry — the grant-time root 0x0 stands" and an empty window, and `slot_roots_shipped` read 18 of that volume's 19 overflow slots. The 5 misses whose `mv` had not run prove a CREATE-record loss, not the rename's. Cause: a PAGE-published root pushed off the page by a lower slot's first touch kept its `published` mark, so nothing shipped it and nothing named it (PR 4/12b's page-budget overflow law; the cut is dynamic). Fix: a publication remembers its home, the page prefers the slots tree 0 does not name, an off-page page-homed publication is demoted and shipped to tree 0 BEFORE the page that drops it is written (`meta_kv_forest_page_publications_demoted`). Pin on the two-backend fixture, RED on the base at the durable level and at the acked-writes level: `sym_n_daemon_tests::a_page_published_root_pushed_off_the_page_by_a_lower_first_touch_rides_tree_zero_before_the_lessee_dies`. The storm ×10 count restarts from zero on PR 13b's binary. The text below is the finding as recorded.**

`sym-storm` batch 2 (`7c62428b`), round 4 from zero (`/tmp/grok-justin/
fix1-fleet2/sym-storm-rows/symstorm-1789948411/lost-r4.txt`, the
daemon logs beside it): the seven joiners killed at once at phase 6.2 s;
14 regions recovered by the manager 19 s later; the acked-writes oracle
at the manager read 25 misses over 17,376 fsynced files — **all writer
m60's** (`storm-w60-r4/f000086`, `f000216`, `f000344`, `f000472`,
`f000600`, `f000728`, `f000856`, `f000984`, `f001112`, `f001240`,
`f001368`, `f001496`, `f001624`, `f001752`, `f001880` — a stride of
exactly 128 names, 15 of m60's 1,904 acked), each `src=0 dst=0`: not at
`/storm-w60-r4/` and not at `/storm-xo-r4/w60-…` (the manager's
cross-owner directory, striped K = 64 by the movers' inserts); 10 of the
15 are also in the mover's RETURNED ledger (`moved-w60-r4.ledger` — the
`mv` returned 0 before the kill). Rounds 1–3 of the same shape were GREEN
(16,086 / 17,418 / 18,388 acked, 0 lost), as were batch 1's rounds 1–7 on
`16408a2f`. **What the shape says**: an acked `dd conv=fsync` (m60's own
slot) followed by an acked cross-owner `mv` into a striped foreign
directory (PR 6's intent: `RemoveDentry` at m60's own directory,
`InsertDentry` shipped to the stripe's holder — another joiner, killed in
the same instant — or local when the stripe is m60's) and then EVERY
participant dead at once: the source name gone, the destination name
absent after the manager recovered every ring (§5.9 per region, then
`roll_forward_open_intents`), the child's record unreachable by name. The
128-stride over sequentially named files is a schedule artefact of the
mover's passes (one `mv` per file per pass, `sleep 0.05` between passes)
— the names each pass's FIRST or LAST rename touched — not a hash class
(the stripe of `w60-f…` is `hash54 % 64` over the seeded dentry hash).
Candidates, unattributed here: the recovery's replay of the stripe
holder's ring dropping the shipped inserts (a `Lease` / frame-screen
class the must-stay-0 set did not count — every violation gauge read 0),
the intent's roll-forward at the manager applying `RemoveDentry` without
the insert (the initiator dead mid-plan with the insert's holder ALSO
dead — the two-process dead-initiator shape §7 routes to PR 13b, met
here for real), or the mover's `mv` acked on a rename whose insert step
never reached durability at the holder before the kill (PR 6's ack law
would then be the defect). **The two populations (fix round 2, Issue
25):** the oracle's 25 = the 15 names + the 10 RETURNED ones counted a
second time by the moved ledger's own pass ("a RETURNED mv is not at its
destination"); the 15 lost names are TWO classes the oracle must judge apart —
**10 whose `mv` had RETURNED** (in `moved-w60-r4.ledger`: an acked rename
whose insert PR 6's law says was durable at its holder before the ack;
absent at both homes after every ring's recovery, this is the P0 signal
whatever the oracle's timing) and **5 whose `mv` had NOT returned** (the
kill caught the rename mid-plan; a name at NEITHER home while its intent
is still OPEN is the S3.5 lattice's DESIGNED transient — PR 10's driver
runs `roll_forward_open_intents` AFTER the per-region recoveries that
move `appender_recoveries`, and the round's oracle ran the instant
`appender_recoveries ≥ want`, so those five may be the roll-forward's
window read too early, never a loss). **The timing premise the recipe
gains**: the oracle settles on `xv_cross_owner_intents_open == 0` at the
manager (bounded by the landing ceiling × a few + the stuck grace; past
it the harness dies naming `xv_cross_owner_intents_stuck`) before it
judges, snapshots `xv_cross_owner_intents_{open,stuck}` and
`recovery_intents_rolled_forward` per round, and a lost name whose `mv`
did not return with an intent still open is reported "in flight", not
LOSS — the harness carries this since fix round 2 (`tests/run_mw_matrix.
sh`, the storm's recovery wait). **Not fixed in this round**: the fix
loop's turn ended at the finding; it is a P0 class (acked loss on the
death path) and joins defect 32 as a flip blocker — PR 13b's first item,
with the attribution recipe: re-run the shape with `SQUEEZEFS_XV_TRACE`
on the initiator and the stripe holders, settle on the intent gauges as
above, correlate the lost names' intent ids against the manager's
roll-forward log (`recovery_intents_rolled_forward`), and read the
recovered stripe tree's frames for the inserts — the 10 RETURNED names
first. The harness die that should have named it
was itself red (`survivors[0]` unbound under `set -u` when every joiner
is a victim) — fixed in `4523f25e`; the lost list and the ledgers are
the evidence.

### 4.4ag Fix-round finding 2 — FOUND, NOT FIXED (PR 12b / PR 5, the failover window): a joiner's read one second into a manager failover answers `EIO` — the successor's token plane refuses a member whose reclaim has not landed

> **Status (PR 13b, 2026-09-21): FIXED in `53ffb626` + `31519ceb`.** The holder's membership screen answers the TYPED `TokenReply::NotMember`, minted at the client as `RefusalClass::MembershipPending` (EAGAIN); a token fetch and the serve gate over a channel the screen refused park on this member's grant adoption (bounded by `reassertion_wait_bound()` = 2 × the renewal beat) and retry — never `EIO` — while this process holds a member session (a ghost keeps the fail-closed word); a `CustodyGrant` answered `NotMember` surfaces the typed class for the acquire ladder's retry. Gauge `dlm_token_membership_waits`. Pin: `sym_coherence_tests::a_read_refused_not_a_member_parks_on_the_members_reclaim_and_never_answers_eio`. Rounds 3–4 (`a595c35a`, `30ed26dc`, found by `sym-crash --rounds=3` on PR 13b's binary — the class still reached a joiner's `stat` of a removed directory): the park's bound starts at the FIRST refusal (a dead dial ahead of it had consumed it), and a fresh per-holder plane's arm PROBE parks like a fetch (`data_grant::foreign_read_plane`); legs 3–4 of the same pin, `sym-crash` GREEN 3/3 from zero after. The text below is the finding as recorded.**

`sym-crash --rounds=1` on `7c62428b` (batch 2), the WIDENED
deleted-stays-deleted arm (Issue 8 — `sym_stat_deleted`: only `ENOENT` is
deleted): joiner m60's `stat /acked-r1` (the round directory the
successor had just removed) answered **`Input/output error`**. m60's log:
00:06:39 the membership reclaim against the dead manager's listener
refused `Connection refused`, the custody client fenced its own custody
at `T_self` and re-dialed the successor; 00:06:40 the successor's token
plane refused m60's read frame — "client `node_…m6674fc98` holds no live
membership lease with this set's owner — a read token is granted to
members only" (PR 5 round 3 Issue 27's law: the token dispatch checks
the caller's lease FIRST) — and `data_grant` surfaced it as
`Refused { errno: 5 }` to the FUSE op. The joiner's lease was one beat
from re-asserting at the successor (PR 12b round 5's re-assertion half
admits it); the read did not wait for it. Before the widened arm this
EIO read as "deleted" and the round was GREEN (every one of the nine
10/10 runs' deleted arms could have hidden the same window). Every other
law of the round held. **The class**: a user read at a joiner inside the
failover window fails `EIO` for the beat until the joiner's reclaim
lands, instead of parking on the reclaim (bounded by the renewal beat /
`T_owner`) and retrying the token fetch. Not fixed here (the fix loop's
turn ended); PR 13b's item beside the token plane's failover follow
(defect 15): the per-holder token client's "no live membership lease"
refusal is the retryable class until the member's lease is re-asserted
or expires — never `EIO` inside the window.

### 4.4ah Fix-round finding 3 — FIXED (the PIN's own seam schedule, not the product; defect 21's pin): `a_single_flight_fetch_loser_registers_before_it_rechecks_the_winner` deadlocked itself whenever the second-spawned fetcher won the single flight

Twice in the fix round's matrix runs the `sym_coherence_tests` suite
stopped on this test until the runner's 600 s watchdog killed it — the
first matrix's FLAT leg (with a standalone instance of the same test
running in another process beside it) and the second matrix's STAMPED
leg (nothing else on the box); the same suite run alone passed on both
legs and the pin alone passed in 2 s, so it was filed as a product
finding until the stacks said otherwise. **The hang hunt (2026-09-20,
the tree at `4094cb40`) reproduced it, read the stacks, and attributed
it to the PIN — class (d), the seam's own schedule; the product's
`TokenReaderPlane::fetch` was correct throughout.**

**Reproduction.** The suite in the runner's order (one process,
`--test-threads=1`, `CARGO_INCREMENTAL=0`, the two legs run beside each
other as the first hang's shape) passed 4/4 on the `4094cb40` binary
(stamped ×2, flat ×2, 41/41 in 75 s each) — the in-suite rate is low.
**The pin ALONE, the test binary run directly, HUNG 10 of 29 runs on the
`4094cb40` binary** (loops of 3/4/3/4 with 2/2/1/1 hangs on the idle box;
then 15 runs interleaved with the fixed binary under a concurrent clippy
build: 4 hangs — `/tmp/grok-justin/hang-hunt/pin-red*.stacks`); the
record's "the pin alone passes in 2 s" was one lucky sample of a ≈ 35 %
race. At every hang the stacks were captured before the kill
(`sudo gdb -p`; `ptrace_scope` 1 refuses an unprivileged `eu-stack`):

- **The parked frame.** Every worker thread of the pin's 4-worker runtime
  is parked idle (`parking_lot_core::…::futex_wait` under
  `tokio::runtime::…::park`) — no task is runnable; the test thread
  (`Thread "a_single_flight"`) is parked in `tokio::runtime::park::
  CachedParkThread::block_on` ← `Runtime::block_on` ←
  `a_single_flight_fetch_loser_registers_before_it_rechecks_the_winner ()
  at tests/sym_coherence_tests.rs:1915` — the `#[tokio::test]` block_on
  of the test's future, i.e. the test's OWN await, not a product task.
- **The statics name the future** (gdb on the symbols, identical at all
  four captured hangs): `TEST_FETCH_LOSER_HOLD = 1` — `test_fetch_loser_
  release()` has NOT run, so the test sits BEFORE it, at the one
  unbounded await between the seam's park and the release:
  `winner.await`; `TEST_FETCH_LOSER_PARKED = 1` — exactly one fetcher is
  parked at the seam; `TEST_FETCH_LOSER_RELEASE.state = { permit: false,
  epoch: 0, waiters.len: 1 }` — the seam's `Notify` holds ONE registered
  waiter and its `notify_waiters` never fired. So the seam-parked future
  (`released.await` in `fetch`, the Occupied arm) is the task the test
  calls `winner` — the FIRST-spawned one — and the task it calls `loser`
  won the single flight, fetched, removed its entry, notified nobody (the
  seam holds the other before its registration) and finished. The test
  then awaited `winner` unbounded while the release `winner` needed sat
  behind that await: a deadlock of the pin with itself.

**The mechanism.** `#[tokio::test(flavor = "multi_thread")]` runs the
test's future under `block_on` on the test THREAD (the stack above); both
`tokio::spawn`s therefore land on the runtime's inject queue and are
picked by two different workers — which of the two reaches
`fetching.entry_sync` first is a race of two worker wake-ups, won by the
second-spawned task ≈ 35 % of the time alone here and more often under
load (the matrix, a sibling process — the two matrix hangs' shapes). The
pin assumed spawn order = station order. Not class (a): `sqz_notify`
registers at creation and `notify_waiters` bumps the epoch every poll
re-checks (read again for this finding); not (b): the fix round touched
neither `token_plane.rs`, the pin, `sqz_notify.rs` nor `data_grant.rs`
(`git diff --stat dcc0e1af^..4094cb40` on those paths is empty — the
correlation with the fix round was the round's extra matrix runs and
their load); not (c): no projection-refresh flight is on this path. The
predecessor bisect the round tried could not have converged: the defect
predates every fix-round commit (it landed with the pin, `86559cf3`).

**The fix** (pin only — `tests/sym_coherence_tests.rs`, the test and its
doc comment state the mechanism): the roles are the STATION's. After the
seam reports a parked loser, the winner is **whichever task FINISHES**
(`tokio::select!` over the two `JoinHandle`s, bounded at the file's 20 s
— a genuine product park now fails LOUD instead of hanging the matrix),
the release follows, and the other handle — the seam's loser — must
serve within the 5 s bound as before. The pinned law is unchanged:
register-recheck-await serves the loser off the winner's cache with ONE
grant.

**Proof (RED → GREEN, same box, interleaved).** RED: the `4094cb40`
binary's pin alone 15 runs → 4 hangs (statics as above), cumulative 10 of
29. GREEN: the fixed binary's pin alone **15 / 15** in the same
interleaved loops (0 hangs), and the suite in the runner's order **5 + 5
GREEN — stamped 5/5, flat 5/5 (41/41 each, 75–77 s wall), the two legs
run beside each other** (+ one more pass per leg on the committed tree
after the doc comment's final wording). `cargo fmt --check` 0; `cargo clippy --all-targets
--all-features -- -D warnings` 0; `cargo clippy --all-targets -- -D
warnings` 0. Artifacts: `/tmp/grok-justin/hang-hunt/` (`run_leg.sh` the
watched suite attempt with stack capture, `pin_loop.sh` the bounded pin
loop, `pin-red*.stacks` the four captured hangs, `{stamped,flat}-N.log`).
Recipe for the class: a pin that names its actors by SPAWN ORDER and
awaits one of them unbounded before releasing a seam is this deadlock
waiting for load — every such await in a seam-driven pin is bounded, and
the actors are named by what the SEAM observed.

### 4.4ai Fix-round-2 finding — FIXED (PR 13's own interim refusal, §4.4z): the real-mount contract read `chmod` exiting 0 on a REFUSED `SETATTR` — the errno class round 1 chose (`EOPNOTSUPP`) is the one class coreutils' `chmod`/`chown` are entitled to swallow

**Found by the fidelity `sym-join-ladder` leg's new §4.4z contract on
its first run** (fix round 2, the N = 3 real-daemon leg, `PASS=122
FAIL=1`): `SYMJOIN/N: joiner 2's chmod of joiner 3's file: rc=0 ''
(want EOPNOTSUPP)` — while the `>>` half beside it PASSED (the open
refused, bash printed the error, rc 1). **Attribution** (an instrumented
copy of the leg run alone, kept outside the tree — `/tmp/grok-justin/
attrib-fideli.sh`, its run `/tmp/grok-justin/fix2-attrib-run1.log`):
joiner 2's daemon logged `FUSE SetAttr { mode: Some(33152),
ctime: Some(..) }` for the file and REFUSED it with the typed class
(`foreign_file_mutation_refusals` 1 → 2, the ERROR line naming slot 137
/ appender 2); `strace` on the `chmod` read **`fchmodat(AT_FDCWD, ".../
w3-dir/f7", 0600) = -1 EOPNOTSUPP`**; the mode read **644 at joiner 2,
joiner 3 and the manager** before and after. So the daemon was right on
every count — the errno reached the syscall and nothing moved — and
**`chmod(1)` exited 0 and printed nothing**: coreutils ≥ 9.6
(`src/chmod.c`, `process_file`: `if (! is_ENOTSUP (errno)) { error(…);
ch.status = CH_FAILED; } /* else treat not supported as not applied
*/`; `src/chown-core.c` the same for `lchownat` — "Ignore any error due
to lack of support") classifies `ENOTSUP` / `EOPNOTSUPP` from the mode
and owner syscalls as NOT AN ERROR, its accommodation for
`AT_SYMLINK_NOFOLLOW` on Linux. Not (a) a different handler path (the
one FUSE `setattr` ran, its `SetAttr` shape is the chmod's — mode + the
writeback cache's `trust_local_cmtime` ctime), not (b) the own-region
exemption (`b8c4c92a`'s predicate answered "foreign", the refusal
fired), not (c) a lost errno (the syscall returned it): **the WORD was
wrong**. "Not supported" is exactly what a permission-preserving tool is
entitled to ignore (`cp -p`, `rsync -p`, `tar -p` would have "preserved"
a colleague's file's mode silently too), and the in-process pin of round
1 asserted the typed class and the errno's VALUE — never what the
syscall's caller does with it: its premise was too narrow.

**Fix** (`src/error.rs`, the one `to_errno` row; every site that named
the word): `SqueezefsError::ForeignSlotFileMutation` → **`EREMOTE`**
("Object is remote" — the record lives at another appender, which is
what the daemon is saying; the S8 owner service's own not-the-owner word
for an intent naming a volume this node holds no authority over). Never
`ENOENT` (the file exists), never `EIO` (nothing broke), never `EAGAIN`
(nothing is transient until PR 13b), never `EPERM` (collides with
`default_permissions`' class — an operator could not tell "not the
owner" from "not the lessee" without the log), never `EXDEV` (S8's
rename/link word, whose text misleads a `chmod`; `mv` falls back to
copy-and-unlink on it). No tool masks or retries `EREMOTE`; neither the
FUSE kernel module nor the VFS special-cases it (grep of `fs/fuse`,
`namei.c`, `open.c`, `attr.c`, `xattr.c` on the 7.2.3 tree: no hit). The
typed class, the gauge, the message naming PR 13b / the slot / the
holder, the open-for-write gate, the times-echo absorb and the
own-region exemption are unchanged; a flat/unarmed mount never
constructs the class.

**Pinned red-first** — `sym_n_daemon_tests::a_chmods_setattr_shape_on_a_
foreign_slot_file_refuses_with_an_errno_no_tool_swallows` (the
two-backend fixture): the premise is the FUSE layer's SHAPES (a `chmod`
is `mode + ctime`, a `chown` is `uid/gid + ctime`, a `>>` is the
`O_WRONLY | O_APPEND | O_CREAT` open) and the law is the CALLER's — the
errno is outside gnulib's `is_ENOTSUP` set (`ENOTSUP`, `EOPNOTSUPP`),
never `ENOENT`, never `EAGAIN`, `EREMOTE` exactly, the message names PR
13b, the record untouched at BOTH daemons (mode, uid/gid, the holder's
ctime), the ctime-only echo still absorbed; RED on the `EOPNOTSUPP` tree
with the exact message ("errno 95 is in coreutils' is_ENOTSUP set —
chmod(1)/chown(1) exit 0 and print nothing on it"), GREEN on the fix.
The round-1 pin asserts `EREMOTE` now; `posix_errno_tests`' table
carries the row. The fidelity contract greps `Object is remote`,
requires `chmod`'s exit status nonzero, and reads the mode at all three
daemons after the refusal (644).

### 4.4aj Harness — FOUND by the box re-run's review, FIXED on `perf/sym-box-rerun` (2026-09-22): the `sym-foreign-touch` PAUSED phase's "live job" never ran — every PAUSED outcome since PR 13's `8b7cc418` (the laptop's §3.4 greens, §4.4ac's attempt-12 handover, the box re-run's two positions) is VOID

`tests/run_mw_matrix.sh` launched the paused job's storm as
`"$SYM_STORM" "$(mnt_of "$c")/job-w$c/paused" … mkdir >… 2>&1 &` without
creating `job-w$c/paused`. `tests/mdstorm.c`'s `mkdir` phase never
creates its own root (`worker`: `mkdir("%s/d%ld")` on `g_dir`), so the
first `mkdir(job-w0/paused/dN)` failed `ENOENT`, `fail()` set
`g_failed`, every worker stopped, and the storm exited — backgrounded
and later `kill`ed / `wait`ed with `|| true`, nothing noticed. The LIVE
phase had both halves (`mkdir -p "$live_root/r$i" || exit 1` and the
`kill -0 $live_pid || die` liveness check — §4.4f's fix); the PAUSED
phase had neither. Evidence (the box re-run, both positions):
`paused-c.txt` holds exactly one line — `mdstorm: mkdir failed on
/mnt/sqz-mwfleet/m0/job-w0/paused/d1` (r1) / `…/d3` (r2) — and the
holder's `meta_kv_journal_entries` moved 15,873 → 15,882 (r1) / 15,940
→ 15,947 (r2) across the whole phase: the three touches' served steps,
no storm. **Consequence:** with no live work on the touched slot the
holder's `ops_h` is 0 and the slot reads IDLE (`slot_offers_idle` +1 in
every position, laptop and box); a DOMINATED offer is then LEGAL by the
rule (`ops_q ≥ 2 × 0 ∧ ops_q ≥ N_floor`) and the idle arm simply fires
first — "dominated offers 0" tested nothing, and design §8 row 3c's
third law (`a_paused_live_job_keeps_its_tree`) was never exercised by
this leg on any venue. **Fix (harness, no product change):** the root
is created first (`mkdir -p … || die`); the job must be a live STOPPED
process at the pause (`/proc/<pid>/status` `State: T` — a `kill -0` on
the exited job's zombie would still succeed) with no failure line
written; after the touches it is RESUMED and must COMPLETE its
`SYM_FILES` mkdirs (its `mkdir ops=… ` row is the proof) and the
holder's journal must move by at least that many entries; a storm that
dies at any point dies the leg loud, exactly as the LIVE phase's does.
**Proof the fixed phase runs (laptop, "it works", 2026-09-22 11:03 —
`7.2.6-cachyos-lto`, the tcp devsub recreated after the reboot, `create
N=2 --symmetric --writers=3 --lease-ttl-ms=15000`, `sym-foreign-touch
--venue=laptop`, the binary `fe960985` = `77f4da1d`'s source):** `mkdir
ops=40000 wall_s=9.639 ops_s=4150` after the resume, the holder's
journal **+60,129** entries over the phase, and — for the first time —
**handovers 0, `slot_offers_idle` 0, `slot_offers_dominated` 0, `slot_
offers` 0** on the manager (m60 / m61 all 0); LIVE 192 ships / 0
handovers and IDLE after 3 bursts (35.7 s, 5.54 ms) beside it, oracle
clean, the fleet and the devsub torn down to zero residue. Artifacts:
`/tmp/grok-justin/box-rerun/fix1-local/` (the leg log, `paused-c.txt`,
the `m*_ppaused{0,1}.json` snapshots, the three daemons' logs). **Owed:**
the PAUSED law's BOX row — the next box session (PR 13e's binary; not
run in this round by the minimum-count law). §3.4, §3.9.4.3, §7 item
10, design §8 row 3c and §9 are re-worded on this branch.

### 4.4ak PR 13d finding — ATTRIBUTED, NO PRODUCT CHANGE, the LAW MOVED (the `-ls` law's fleet-state term): `sym-shared-dir-ls` read `2K + C + 3` tokens on the gated `77f4da1d` — the ROOT was striped, and a token reader's first `stat /` AFTER its lookup learnt the root's map pays one records-only grant per root stripe

**Found by PR 15's local functional pass** (2026-09-22, three runs on one
fleet — the driver twice and the matrix's own `sym-shared-dir` leg once:
`[mwmatrix] ERROR: sym-shared-dir-ls: dlm_token_grants=20131 ∉ [K + C,
K + C + 4]`, `/tmp/grok-justin/pr15-local/matrix-shared.log`, snapshots
`rows-shared/m1_pls{0,1}.json`), the binary product-identical to
`77f4da1d`; every PR 13-era run of the leg — laptop batches 4–15 and the
box's two positions (§3.3, §3.9.2) — read exactly `K + C + 3`
(20,067 / 40,067). PR 13d's brief was the 16 product commits of
`7b2ef9e9..77f4da1d`.

**Attribution — fleet STATE, not a commit.** The reader's snapshots say
the extra 64 are 64 DISTINCT `(object, plane)` entries, never re-fetches
(`dlm_token_cached` +10,064 / +10,065 per volume for +20,131 grants: two
re-grants — `D` records-only then with dentries — and 20,129 distinct
objects = 20,000 children + 64 stripes + `D` + **64 more**). The
manager's snapshot BEFORE the leg names them: `m0_psd0.json` reads
**`dir_striped_dirs 1`, `dir_stripe_flips 1`, `dir_stripe_supply_rpcs
63`** — the manager had already flipped ONE directory it holds, `/`:
PR 15's driver ran `sym-tarx` + `sym-scale` + `sym-shared-dir` on ONE
fleet whose writers each `mkdir` their per-leg directories into `/` —
cross-owner creates served at the manager from > 1 creator, `flip_due`
(`≥ 2 creators ∧ total > N_floor`) — and that fleet's `/` reads striped
from its FIRST kept snapshot on (`rows-full/m0_plocal-10.json`, the tarx
legs, before `sym-scale`; every later `m0_*` too). PR 13's OWN binary
stripes `/` the same way: `pr13-batch10/sym-scale-rows/*/m0_pn81.json`
reads `dir_striped_dirs 0 → 1` across `sym-scale`'s N = 8 row (eight
writers' `mkdir /scale-…-w<idx>` at once), and on the box bracket
`021250`'s `scale-r1/symscale-1790043202/m0_pn81.json` reads 1 after its
N = 8 row. **Every kept `-ls` row correlates the manager's root state
with the reading, 100 %:** the eleven PR 13 laptop batches
(`pr13-batch{4,5,7,8,9,10,11,12,13,14,15}/sym-shared-dir-rows/*/
m0_psd0.json` `dir_striped_dirs 0` — each `sym-shared-dir` on a FRESH
fleet, `create-rows.log` between the scale and the rows legs) ↔ 20,067 =
`+3`; **the box's two `-ls` rows** (bracket `022526`, `shared-dir-r1` /
`shared-dir-r2`: `box.log` shows a fleet `create` and `teardown` around
EACH leg — pass 2's `FRESH_FLEET_PER_LEG=1`, H-B2 — and BOTH rows' kept
`m0_psd0.json` AND `m0_psd1.json` read `dir_striped_dirs 0`) ↔ 40,067 =
`+3`; PR 13d's own fleet run 1 (below, root unstriped) ↔ `+3`; PR 15's
run (`dir_striped_dirs 1`) ↔ 20,131 = `+3 + 64` and PR 13d's run 2 (the
root flipped explicitly) ↔ 40,131 = `+3 + 64`. **No box `-ls` reading on
a striped root exists**: the ONE box shared-dir leg that shared a fleet
with `sym-scale` (bracket `021250`, root striped by its N = 8 row) died
AT ENTRY on `appender_flush_ceiling_overruns=1 on m0` (H-B2's class,
`shared-dir-r1.log`, wall 27 s) before its `-ls` half ran. The box
evidence therefore AGREES with the attribution at every kept row.

**Mechanism (pre-existing since PR 13's `stripe_map_cached`, priced by §7
item 7).** A `-o ro` token reader's `ls -l D`: the kernel walks the path
with every TTL 0 under tokens, so it `lookup(/, D)`s — the routed lookup's
`stripe_route(/, D)` reads `/`'s map through the root's token and
caches it (`stripe_map` → `read_map_from_markers`; `stripe_locate`
fetches the ONE root stripe naming `D`, with dentries) — and it
revalidates the mount ROOT's attrs: `getattr_local(1)` →
`stripe_map_cached(1)` (the CACHE alone — known once the lookup learnt
it) → `fold_striped_attrs` → `stripe_record(D_i)` → `getattr(D_i)` →
`token_serve(default)` — ONE records-only grant per root stripe not
already cached (`K_root − 1`), then hits for the mount's life. **The fold
lands on the first `stat /` AFTER a lookup under `/` learnt the map**: the
kernel's `default_permissions` walk GETATTRs `/` BEFORE `LOOKUP(/, D)`,
and that pre-lookup `stat /` pays one records-only root grant and folds
NOTHING (`stripe_map_cached` reads `None`). So the constant beside
`K_D + K_root + C` is the walk order's: a truly cold reader in kernel
order reads `+4` (the root's records grant, then its dentry-bearing
re-grant at the lookup's marker read, `D`'s record, `D`'s re-grant) —
the law's ceiling — and the fleet reads `+3` only because the harness's
pre-leg `snap … ls0` (a `.stats` read → `GETATTR(1)`) absorbs the root's
records grant BEFORE the leg's first reading (PR 15's `m1_pls0.json`:
`dlm_token_grants [2, 0]`). Total `K_D + K_root + C + 3` on the fleet;
with `K_root = K_D = 64`, `2K + C + 3` — the fleet's 20,131 to the
token. The listing of `D` itself still pays exactly `K_D + C + 3`.

**Pinned (every arithmetic, in-process, the leg's shape):**
`sym_n_daemon_tests::a_cold_ls_of_a_striped_directory_at_a_token_reader_
pays_one_token_per_stripe` — two metadata volumes, the manager holding
`D`, two stripes SUPPLIED by joiners over the S8 wire, children on both
volumes, a cold reader dialing every holder's plane, the routed verbs the
FUSE handlers call (`lookup(/, D)`, `stat /`, `stat D`, the paged merge,
`lookup + stat` per child, `stat D`): phase 1 (root unstriped,
lookup-first) EXACTLY `K_D + C + 3`; phase 2 (root striped over
**`K_root = 6 ≠ K_D = 4`** — the discriminating shape, since at `K_root =
K_D` the reading equals `2K + C + 3`, which a "second token per `D`
stripe" theory passes too; a second cold reader, lookup-first) EXACTLY
`K_D + C + 3 + K_root`, AND the reader's per-volume `dlm_token_cached`
delta between the phases is `K_root` on the ROOT's volume and 0 elsewhere
(the fleet's split made a law); phase 3 (the KERNEL's order — `GETATTR /`
before the lookup — on the striped root, a third cold reader) EXACTLY
`K_D + C + 4 + K_root`, the law's ceiling (review round 1, Issues 2–3).
**The lookup-first arithmetics read the same on `7b2ef9e9` and on
`77f4da1d`'s code** (39 / 43 for K = 4, C = 32, K_root = 4 — the base
tree at `/tmp/grok-justin/pr13d-base-7b2ef9e9`, removed after; every
API the pin uses exists at `7b2ef9e9`, so a checkout re-runs it), so no
commit in the range moves the reader's token economy and `git bisect`
has nothing to find; the range's reader-side diffs (`stripes_armed_any` /
`token_serve`'s early returns, F-B3's root-attr arm on the `Err` path,
the typed `fail_closed`, the armed-only `dir_parents` feed) are inert on
an armed reader by inspection. The solo contract in `sym_coherence_tests`
(`a_token_reader_lists_a_striped_directory_as_the_merge_of_its_stripes`)
pinned only `≥ K + C`; it pins `K + C + 1` exactly now.

**Reproduced on the fleet, same binary, both ways (laptop — "it works"
evidence; `/tmp/grok-justin/pr13d-fleet/`):** a fresh `mw_fleet.sh create
N=2 --symmetric --writers=3 --token-readers`, `run_mw_matrix.sh
sym-shared-dir --venue=laptop` from zero — the root unstriped (`m0`
`dir_striped_dirs 0`): **`dlm_token_grants` 40,067 = K + C + 3** for
K = 64, C = 40,000, `readdir_merges` 83, 16 misses = the poll's, oracle
clean, the leg GREEN (`rows/symshared-1790083203`); then the ROOT flipped
at the manager by the explicit `setfattr -n user.squeezefs.stripes -v 64`
on its mount root (`dir_striped_dirs 1`, `dir_stripe_supply_rpcs 63` —
PR 15's `m0_psd0` shape exactly) and the same leg on the same fleet:
**40,131 = 2K + C + 3**, the leg's own `ERROR … ∉ [40064, 40068]`
(`rows/symshared-1790083306`); the reader's per-volume `dlm_token_cached`
deltas put the 64 extra objects on the ROOT's volume beside its 20,000
children (`+20,064`), the shared directory's own on the other
(`+20,065` = 20,000 + 64 + D) — PR 15's split to the entry. Fleet and
substrate torn down to zero residue.

**Adjudicated (orchestrator, 2026-09-22) — option (c), the LAW moved:**
design §8 row 3b's `-ls` law is **`K_D + K_root + C + [0, 4]`** — `K_D`
the listed directory's stripe count, `K_root` the mount root's (0 while
`/` is unstriped; read off the manager's root as `getfattr -n
user.squeezefs.stripes` / the `dir_striped_dirs` census). The root-stripe
fold's records-only grant (paid at the reader's first `stat /` AFTER a
lookup learnt the map; a cold reader in the kernel's order reads the
`+4` ceiling, the fleet's `+3` is the harness's pre-leg snapshot
absorbing the root's records grant) is a design-conformant cost, paid
once per token lifetime, and the law must hold on ANY fleet state — the leg's
former text `[K + C, K + C + 4]` presumed a fresh fleet (an unstriped
root), which is the harness's premise, not the design's. The in-process
pin above carries both arithmetics already (`K + C + 3` at `K_root = 0`,
`+ K_root` otherwise). **The LEG's code change lands in PR 15** —
`tests/sym_rows_lib.sh`, the shared law library PR 15 extracts from the
matrix, so the law lives in ONE place; `tests/run_mw_matrix.sh`'s `-ls`
body is PR 15's to move (untouched here). No product change: a records-
only stripe grant riding something cheaper than a token would be a new
economy, not a fix.

### 4.4al PR 13e — F-R3, FIXED (PR 6 × PR 12b, the armed plane; P0): a cross-owner unlink / rmdir / link / rename-over / directory move of a FOREIGN-minted child read its `(pre, post)` witness off this daemon's PROJECTION — `None`, the count step dropped, the name removed alone, the inode orphaned

**Found by the box re-run's `sym-foreign-touch` leg** (the box-rerun
record's §3.9.4.3 on `77f4da1d`: the end-of-leg `rm -rf` of the job trees
logged 430 of 512 children "no inode record — removing the dangling
name", one orphaned inode per cross-owner unlink of a child ANOTHER
appender minted). **Mechanism:** PR 6's plan builders (`unlink_at`,
`link_body`, `rename_body`'s overwritten destination and parent shift,
the compensation's inverse insert, `dir_stripe`'s shadowed migrate loser)
read the child's inode record with `read_inode_value_routed` — a LOCAL
read of this daemon's tree of the child's slot, which under KD-SYM-3 is a
stale PROJECTION whenever another appender leases that slot (a leased
slot's root rides its lessee's PAGE; the projection knows nothing minted
after the last checkpoint it refreshed from). A create the touching
requester minted in ITS rotor and shipped the dentry for lands in the
requester's slot tree; the holder of the parent later removes the name
through its own mount, reads the child's record from its projection of
the requester's slot — `None` — and PR 6's `None ⇒ no count step` arm
(written for "the record is genuinely gone") ships `RemoveDentry` alone:
the name is gone, the record stands at `nlink 1` in the requester's tree
for ever — and, AS FOUND, invisible to fsck C9 at EVERY censusing mount:
while the requester lives the inode plane scopes its slots out (PR 12b
round 1's law), and after it LEAVES C9's era floor EXEMPTED every ino it
minted as current-era (review round 1, Issue 2 — below: the first
build's post-leave census was VACUOUS for exactly this class). **Fix
(`a5e80886`):**
`KvMetaBackend::read_inode_witness` is the ONE witness read of every
plan-builder site — through the writer's read divert (`token_serve` →
the holder's token plane, one grant, already held from the `lookup` that
precedes an `rm`, recalled by the very step the plan then ships) for a
slot another appender leases; the local record verbatim for a slot this
mount leases or maintains and on every unarmed / flat mount (PR 1's law —
pinned: `sym_cross_owner_tests::the_unarmed_and_flat_paths_ship_nothing_
and_lock_nothing` reads the witness ≡ `read_inode_value_routed` byte for
byte at every step, the last unlink's `nlink 0` record and an absent
record's `Ok(None)` included, both F-R3 gauges 0). A local `None` on a
slot a LIVE foreign appender leases — no divert reached the holder, or
the slot moved under the read — REFUSES the retryable class
(`xv_cross_owner_witness_refusals`, the belt: a projection's absence is no
witness); the "no inode record — removing the dangling name" arm counts
`xv_cross_owner_dangling_names`, a **must-stay-0 tripwire on an armed
mount**. A found accounting defect beside it (`f53d77bd`): a joined
holder's `OfferSlot` answered `Busy` was a `joined_wire_failure` — the
manager's legal refusal (`STATUS_REFUSED`) is `Ok(false)`, while the wire
screen's `STATUS_REJECTED` (an offeree no page names — the two share the
`Refused` reply and differ in the frame's status word alone, which
`ManagerClient::call` had discarded; `call_with_status` carries it) stays
an error — the matrix's screen pin caught the first build serving past
it. **Pins (`ec005930`, RED on the
base with the exact log line, GREEN on the fix):**
`sym_n_daemon_tests::a_cross_owner_unlink_of_a_foreign_minted_child_reads_
its_witness_at_the_holder` (the MANAGER + ONE joiner, both directions —
A: the joiner creates into the manager's directory and the manager `rm`s
the joiner-minted children; B: the manager creates into the joiner's
directory and the joiner `rm`s — plus a joiner-minted directory `rmdir`ed
by the manager; every child reads `nlink` 0 at its holder and every
mount, no dangling-name line, the offline census after every writer left
reads no C9 and exempts nothing) and `…_link_rename_over_and_directory_
move_read_their_witnesses_at_the_holder` (the same two daemons: `link`
into the foreign directory, a rename OVER a foreign-minted destination, a
directory move across holders). **Fleet proof (laptop, "it works"):**
`tests/run_mw_matrix.sh sym-foreign-touch` gained `sym_post_leave_census`
— zero "no inode record" lines across the writers' logs after the leg's
`rm -rf`, the dangling-name gauge 0 on every writer, then EVERY joiner
LEAVES and the manager's online fsck `--json` covers the inode plane on
every volume and reads C9 = C10 = 0 (the live oracle was never a C9
verdict, by the harness's own note); since review round 1 the census
also unmounts the MANAGER and runs the OFFLINE fsck over the set,
asserting `findings == 0` AND `current_era_exempted == 0` (below); the
run's numbers are in PR 13e's report.

**Review round 1, Issue 2 — the post-leave census was VACUOUS for the
joiner-minted class (the box's 430), FIXED (`807f9435`, pins
`799834a9`):** fsck C9's zero-FP shield is the writer era's ino floor
(`KvMetaBackend::minted_in_prior_era` — a record at or above its
keyspace's open-time cursor is current-era: a live create commits the
inode before the dentry). The guest floors were seeded from the
manager's STAMP and its replay, and a JOINED appender's rotor slots have
their cursors in tree 0 alone — never in the manager's stamp, never in a
probe's — so every joiner-minted ino read as current-era and was
EXEMPTED at the online manager AND at an offline probe (the reviewer ran
the offline `fsck_clean` at the F-R3 pin's RED commit: the 4
manager-minted orphans reported, `current_era_exempted = 7` for all 7
joiner-minted ones — the census as first built could not see the class
it was added for, and its "C9 = C10 = 0" was no verdict). The fix reads
tree 0 as a witness beside the floor: an UNLEASED slot has no minter, so
every record in it is a prior-era candidate whatever this mount's floor
says (the armed plane's live lease table where one exists, else the
`slot_state` words read once at open — `forest_slot_word`); a PROBE has
nothing in flight (an offline census refuses a live set), so every guest
record it reads without a floor is a candidate too; a slot a LIVE foreign
appender leases stays fail-closed (the dentry verdict scopes it out
anyway); and the ino bitmaps' ceiling (`max_local_ino_watermark`) folds
every slot word's cursor, since a joiner-minted ino sat ABOVE the
manager's ceiling and was never indexed (the fix's first cut found 0
findings AND 0 exemptions for that reason). Pin
`sym_n_daemon_tests::a_joiner_minted_orphan_is_a_c9_finding_at_the_
offline_census_after_every_writer_left` (a joiner-minted child's dentry
deleted underneath it, both writers leave, the offline probe reports
exactly that ino as C9 with `current_era_exempted` 0 and its named
sibling not a finding — RED on the base with 0 findings / 7 exempted),
the two F-R3 pins assert `current_era_exempted == 0` after their leaves
(`common::sym::fsck_clean_no_exempt`), and the fleet arm above asserts
it on the offline census after the MANAGER leaves too.

**Review round 2, Issue 10 — C9 exempts the inos an OPEN cross-owner
plan names, FIXED (`73a9fe31`, pin `27434b48`):** the era witness above
widened a pre-existing class — C9 had no open-intent exemption (C10's
`open_intent_inos` was read by C10 alone). A cross-owner create commits
the child's record (step 0, the creator's rotor) under one door token
and SHIPS the `InsertDentry` afterwards; a ship the holder refuses
leaves the intent OPEN for the roll-forward cadence, and once the
child's slot reads UNLEASED (a forced rotor shrink, a dominance
handover, the creator's LEAVE) tree 0 makes the record a prior-era
candidate with no name — so the census REPORTED it as C9 while the plan
that names it stood, report-only by default, and `--repair` would have
run `delete_file` + destroy on the record the roll-forward re-names (a
fresh mint holds no `I{ino}` guard the repair's exclusive lease could
wait on; the roll-forward's insert then dangles — C10). C9 now reads the
same set in its evaluate (lazily, after the era floor — a healthy volume
reaches the line for no candidate), its confirm, and the repair planner
(a C9 action on an ino a plan names is REFUSED), counted on
`unreferenced_intent_exempted` / `fsck_unreferenced_intent_exempted` —
0 on a healthy set, growth = plans open at census time, read beside
`xv_cross_owner_intents_{open,stuck}`. Pin `sym_n_daemon_tests::a_cross_
owner_creates_child_is_never_a_c9_finding_while_its_intent_stands`: the
joiner ships two creates into the manager's directory under
`TEST_XV_SERVE_REFUSE` (both intents open, both records at nlink 1 with
no name), `pending-b`'s intent is deleted at its lessee with the name
never landed (the kill-before-the-inverse window — the shape C9 exists
for), the joiner LEAVES; the manager's census reports `pending-b` ALONE
(`pending-a` exempted, counted 1); the manager's cadence then rolls
`pending-a` FORWARD (an abandoned intent with no live owner) — its name
lands, the census exempts nothing and `pending-b` stays the finding; the
offline probe after the manager leaves agrees. RED on `d06c07c2`: both
children reported at the first census. **Found by the pin, fixed
(`e7ae10d7`):** the per-process intent REGISTER (whose population is
`xv_cross_owner_intents_open`, an abandoned entry past the grace window
the must-stay-0 `_stuck`) was emptied only by THIS process's retirement
— on a fleet another appender's roll-forward retires an abandoned intent
as readily (the manager's poll adopting a joiner's, or the reverse;
`intent_in_flight` is per process), and the abandoning daemon's register
kept the entry for the mount's life: open, then STUCK, on a healthy set
(the in-process fixtures share one register, so only the pin's
`pending-b` plant — a record gone without this process's protocol, the
other daemon's exact shape — met it, and left the two F-R3 pins reading
`intents_open == 1` in suite order). `roll_forward_open_intents` now
reconciles the register against its SUCCESSFUL durable scan
(`forget_abandoned_absent`: an abandoned entry the scan does not list is
forgotten, counted retired where it was counted minted — `minted ≡
retired + open` holds; an `InFlight` entry is never touched; a failed
scan reconciles nothing); the pin asserts the register at `s0` after the
roll-forward.

**Review round 2, Issue 11 — the rebuilt census arm RAN on a fleet
(`646ec726`; laptop, "it works" — no number holds merit):** 3-writer
symmetric fleet on the round-2 binary `eaa0bff6` (`create N=2
--symmetric --writers=3 --lease-ttl-ms=15000`, tcp devsub,
`SQZ_MWFLEET_OSS_GB=24`), `run_mw_matrix.sh sym-foreign-touch
--venue=laptop`. The arm's own lines, second run from zero:
```
[mwmatrix] sym-foreign-touch: zero 'no inode record' lines across the writers' logs (since 1790119013.008426566)
[mwfleet] member 60 unmounted
[mwfleet] member 61 unmounted
[mwfleet] member 62 unmounted
sym-foreign-touch post-leave census: inode plane covered 2 of 2 volume(s), findings by class {}
[mwmatrix] sym-foreign-touch: post-leave census clean — the inode plane judged WHOLE at the manager, C9 = C10 = 0 (every joiner left; the F-R3 orphan class would read C9 here)
[mwfleet] member 1 unmounted
[mwfleet] member 0 unmounted
sym-foreign-touch offline census: inode plane covered 2 of 2 volume(s), current_era_exempted 0, findings by class {}
[mwmatrix] sym-foreign-touch: OFFLINE census clean — every writer left, the probe judged every inode (current_era_exempted 0), C9 = C10 = 0
[mwmatrix] sym-foreign-touch: the remounted manager's grace window closed (waited 15s); re-admitting the members
[mwfleet] member 60 joined-writer posture engaged (appender 1, manager_lease=peer:0x1c4e7f3e0e89c39d, 64 slot(s), registrant adopted)
[mwfleet] member 61 joined-writer posture engaged (appender 2, …)
[mwfleet] member 62 joined-writer posture engaged (appender 3, …)
```
(the offline census JSON: `inode_plane_volumes_covered 2`,
`current_era_exempted 0`, `unreferenced_intent_exempted 0`,
`inodes_scanned 1` — the `rm -rf`'d tree's one live inode; the 157 k
records the recovery scan still counts are `nlink 0` corpses the census
excludes by design — `findings []`; rows kept at
`/tmp/grok-justin/pr13e-logs/symtouch-rows-r2`). The FIRST run found
the arm's own ordering defect: the remounted manager is a SUCCESSOR S6
owner inside its failover grace window (T_owner, reclaim only — §6.7), a
remounted reader is a FRESH acquire the window refuses, and a reader
takes the refusal as final (the shipped S5 posture) — the fleet's
`membership_mode=member` gate read `off` on the reader's remount; the
arm now waits `membership_grace_remaining_ms` out on the manager before
re-admitting anyone (the s10-delegation leg's posture). LIVE 192 ships /
0 handovers, IDLE handed over after 3 bursts, PAUSED 0/0/0, the oracle
clean, fleet + devsub torn down to zero residue.

**Box verdict (the third pass, `b377cbb8`, 2026-09-23 — §3.9.5.3): FIXED.**
Two fresh 8-writer fleets, the same `rm -rf` of the job trees the re-run
orphaned 430 / 512 children on: zero `no inode record` lines across the
writers' logs, `xv_cross_owner_dangling_names` 0 and
`xv_cross_owner_witness_refusals` 0 on every writer (m60 served 445 / 448
steps, m61 shipped 449 / 452), the post-leave online census C9 = C10 = 0
over 2 / 2 volumes, and the OFFLINE census after the manager's leave
covering 2 / 2 volumes with `current_era_exempted` **0**
(`inode_plane_slots_covered` 1,026) and findings 0 — twice.

### 4.4am PR 13e — F-R4, FIXED (PR 6 × PR 4's handover): a create into a directory whose slot moved TO the creator mid-plan answered `ENOENT` — the old holder's live-witness refusal after its release surfaced as the op's errno

**Found by the box re-run's `sym-foreign-touch` IDLE phase** (§3.9.4.3 of
the box-rerun record: one `ENOENT` on a 64-touch burst into a directory
whose slot the burst was moving to the toucher). **Mechanism (the
hypothesis confirmed by the pin):** the initiator resolved the parent's
slot to the OLD holder and shipped `InsertDentry`; the holder had released
the slot (its lease gate `Releasing` → the door drained → tree 0
`Unleased` → the requester's grant) between the resolve and the serve, so
`xv_serve_step`'s `refuse_dying_parent` read the parent from a tree it no
longer leased — `NotFound` — and answered the PR 7b WITNESS refusal
(`ForeignSkipped`, "the parent is dying"), which the initiator's
`foreign_skipped_errno` turned into the op's `ENOENT`; defects 29 / 30 /
35's family, at the one site that judged a witness under a lease it had
lost. **Fix (`9c1794ef`):** `xv_serve_step` checks the lease FIRST and
again inside the `InsertDentry` arm's refusal — a served step for a slot
this holder does not lease is the typed `RefusalClass::SlotMoved { slot,
holder }` (EAGAIN on the wire) naming tree 0's lessee, never the op's
errno; `apply_or_ship_step` answers the arm it TOOK beside the outcome
(`Dispatch::{Local, Shipped}` — review round 1, Issue 3: the first build
read a `step_home` word AHEAD of the dispatch, a two-read window in which
a release landing between them dispatched SHIPPED under a `Local` word),
the retrying wrapper passes it through, and `execute` (and `recover_one`)
classify a witness refusal as the op's errno only when it was judged HERE
or at a holder that still leases the slot (the slot-moved class
re-resolves through tree 0 and re-dispatches — defect 29's arm).
Beside it, `install_wire_grant` adopts the transferred tree
(`adopt_transferred_slot_tree`) BEFORE it names the new lessee in the
RAM table (the first order let a served step on the new holder read the
pre-transfer projection for one window). Seams:
`TEST_XV_SERVE_PARK_AFTER_LEASE_CHECK` (the holder parked after its lease
check), `TEST_WIRE_GRANT_PARK_MID_INSTALL` (the grant parked between the
adoption and the words). **Pin (`4441005b`, RED on the base with the
exact `ENOENT`; GREEN ×3 on the fix):** `sym_n_daemon_tests::a_create_into_
a_directory_whose_slot_moves_to_the_creator_mid_plan_never_answers_enoent`
— the create lands, `nlink` 2 at the holder, the name resolves at the
old holder's mount. **Suite isolation (`59449ea6`):** the pin's ONE
deliberately parked ship (≈ 700 ms) lands in the process-global
`meta_ship_phase_ns.rtt` mean the slot-lease plane's `N_floor` cold-start
seed reads, and the next armed plane in the same process seeded
`ewma_handover` at 2.09 s — `N_floor` climbed 4 → 26 across the dominance
contract's 16-ship burst (green alone, red in suite order; the product
right on both counts); the suite's `reset_process_state()` gained
`meta_ship::test_reset_ship_phases()`, and the dominance verdict and the
seed gained their debug tapes.

**Box verdict (the third pass, `b377cbb8`, 2026-09-23 — §3.9.5.3): FIXED
as far as the leg reaches.** 451 / 451 touch creates (902 over two positions;
three phases each — LIVE 192 + IDLE 256 + PAUSED 3 — the IDLE handovers'
bursts included) answered 0
errnos to the application (the leg's per-create ledger, empty in both
positions); the retryable class `xv_cross_owner_step_slot_moved_retries`
read 0 — no create met the window in these two handovers, so the pin
above is the class's proof and the box the leg's.

### 4.4an PR 13e — F-B1, FIXED as a DERIVATION (PR 2's KD-SYM-10 cadence): `appender_flush_ceiling_overruns` tripped on the box with PR 13c's exclusion excusing NOTHING — the excess was the cadence's OWN term, which the ceiling's `trigger + 2 ticks` never priced

**Found on the box** (§3.9.2 four times in 12 min at 1–32 ms past the
1,100 ms landing ceiling; the box-rerun record's §3.9.4 six increments on
five writers in 45 min, the two with a WARN line 16 / 106 ms past it,
`excused_ns` 0 on every writer, one trip on a QUIET joiner, three
"between the rows" in each writer's `rm -rf` + the joins). **Attribution
(the kept `.stats` `scale-r1/symscale-1790081892/m*_pn*.json` + the
code):** the device writes are µs-class (p99 ≤ 1 ms),
`free_grace_checkpoint_cycle_ms` 0–7, no recovery, no service hold — no
other actor. KD-SYM-10's ceiling prices the tick wait and ONE period of
the tick's work; two terms sat outside it: (1) the cycle's own PRE-BARRIER
WALL — `publish_forest_roots`, the flush pass (an SMO barriers its
successor images, so a pass wall is `(SMOs + 1) × barrier`), the bitmap
pages, barrier #1 (at N regions their page writes) — and (2) the age
decision `last_checkpoint.elapsed() ≥ 1000` ran from the previous cycle's
END, so its post-barrier tail (the grant cadence's `ReturnExtents` /
refills per region — control entries, page writes and barriers;
`grow_stalled_regions`; the merge sweep's one-tick budget) ate the margin
too; the pin's debug tape added a third — the tick's own device work AHEAD
of its decision (the deferred-flush barrier + a maintenance item's SMO
barrier past the drain deadline), 26–272 ms against a 50 ms tick. A leaf
dirtied right after a cycle's collection aged `tail + trigger + late +
wall` at its covering barrier and the audit — correctly — counted it.
**Fix (`470f680a` + `be7ae847`, a derivation, never a widened constant —
§7 item 3):** on a volume with an appender set (every bit-17 forest — the
population the audit judges) the age law runs from the LAST COLLECTION
(`checkpoint_collected_ns`, set by every cycle path — a leaf dirtied after
a collection is the next cycle's) against
`checkpoint::checkpoint_trigger_ms(max_age, term)` = `max_age − term`
(the input is the MAX AGE the tick fires at — `CHECKPOINT_MAX_AGE_MS`,
the elastic ceiling under a live reader ask — never the landing ceiling
`max_age + 2 × tick`, which would eat the margin; review round 1, Issue
7's rename + tie), where the anticipated `term` is the MAXIMUM over the last
`TERM_HORIZON_CYCLES` = `COVER_CYCLES_MAX` (64) cycles
(`checkpoint::CycleTermWindow` — a bound anticipated by a bound: the first
build's EWMA mean left 3 of 6 cycles overrunning, and a decayed high-water
mark leaked one eighth per quiet cycle and landed a burst one step above
it a tick short; the horizon is the ONE cycle-count bound every cover loop
runs to, so a burst is remembered exactly as long as a cover loop would
wait on it — a count of CYCLES, never a duration: about a minute at the
full trigger, seconds at a trigger of 0 or inside a handover's
`checkpoint_now` loop) of ONE cycle's landing TERM
(`checkpoint_cycle_term_ns(wall, late, tick, cap)` = `wall + (min(late,
cap) − tick)⁺`): its pre-barrier wall plus the age decision's lateness
past the trigger BEYOND one tick — the tick
quantization IS the ceiling's first priced tick; the excess is the tick's
own pre-decision device work the second tick bounds at one period — with
the tick's WAIT for the SMO mutex left out (`tick` measures it; a wait
behind another holder is that holder's hold — a structural hold the audit
excuses, an online fsck's census or a wire service it judges, never a term
for the cadence to anticipate: the fleet proof's manager read a 7 s wait
behind its own census as a term and a trigger of 0 for the mark's memory
before this rule). The published ceiling never widens (a cycle slower than
its anticipated term still trips the audit — the tripwire keeps its
teeth); a term at or past the ceiling makes a cycle due every tick, the
honest response to a device that cannot land the promise. The dispatch
is `appenders().is_some()` — every bit-17 forest, ARMED OR NOT (so
`SQUEEZEFS_SYMMETRIC_META=0` on a bit-17 volume takes the new cadence:
the population the audit judges); a bit-17-ABSENT volume keeps the
shipped DECISION verbatim (`tick` keeps `last_checkpoint.elapsed() ≥
max_age`) and pays the term bookkeeping beside it (two atomic stores, one
mutex, a 64-word max per cycle) — "decision-identical", the honest word
(review round 1, Issue 9) for the AGE law's VERDICT. **Two changes of the
F-B1 fix (`8c5eac4d`) ride EVERY layout, stated (review round 2, Issue
17):** (a) the tick's ORDER — the checkpoint DECISION is read BEFORE the
threshold drain and a tick whose cycle is due skips the drain (the
cycle's flush pass appends every dirty node; the drain's leftovers
re-arm at the next tick), and the drain runs under ONE pass-wide
`checkpoint::DrainBudget` over the trees ROTATED (`maintainable_trees_
rotated`) — finding 49's law re-affirmed and tightened: every threshold
drain is bounded by one period, at least one item per PASS (the first
item unconditionally, the rest while the deadline stands), leftovers
re-arm — where the previous per-tree form admitted one free item per
tree and put a forest's tick `trees × one item` late; a flat volume's
tick takes the same order and the same budget; (b) the projection's
image unit is FLOORED at the node unit (`projected_flush_wall_ns` — an
image is one node write at least; consumed by the forest cadence alone,
its units measured on every layout). Published per volume:
`meta_kv_checkpoint_term_ms`, `meta_kv_checkpoint_trigger_ms`; the
per-cycle instrument the box-rerun's item 13 named is the debug tape
`checkpoint: cycle … pre-barrier wall N ms = publish + flush (dirty, SMOs)
+ pages + barrier` and `cycle due by age … decided N ms past the trigger …
the tick's wait for the SMO mutex N ms left out`. **Pin (`56194301` +
`7bf7d284` + `be7ae847`):** `sym_appender_tests::the_cadence_anticipates_
the_measured_cycle_wall_so_a_slow_barrier_lands_inside_the_ceiling` — the
box's shape: the ARMED plane with the box's affinity order
(`Knobs::armed().affinity_mb("16")`; the 64 MiB fixture's derived ceiling
is the one-extent floor, which a one-leaf tree sits AT and spills to the
64-rotor — 60–68 dirty leaves + 1–2 SMOs per cycle and 64 per-tree
maintenance items ahead of every decision), the shipped 256 KiB node /
8 MiB ring, ONE directory leaf, one creator PACED at a tick (a STATIONARY
storm — an unpaced creator's compaction count grows with the leaf, a
bursts-larger-than-any-before shape the tripwire is designed to catch), a
60 ms barrier armed after a clean checkpoint (the term reads 60–120 ms —
the box's 16–106 ms class; a 25 ms barrier's 26 ms term sits inside the
margin on the base too and pins nothing, 0/5), two warm cycles, five
intervals: **RED on the TRUE base (`56194301`'s src) 5/5 — every cycle
1,121–1,174 ms, 21–74 ms past the ceiling; GREEN with the fix 12/12 (terms
63–73 ms, the trigger 927–937 ms), the four ceiling contracts 10/10**. The
first shape (64 KiB nodes, the unarmed 64-rotor, four creators, a 150 ms
barrier) was retired in the rung: its walls were 355–1,204 ms — past the
ceiling ITSELF, a geometry × latency verdict no cadence can land, and not
the box's bounded 16–106 ms. PR 13c's four ceiling contracts stay green
(the service-hold pin dirties its step-3 leaf UNDER the hold: its step-2
parked device teaches the cadence a term past the ceiling, and a leaf
dirtied before the hold is then flushed inside one tick); the fns and the
window are tie-tested in `derivation_sweep_tests`. **What the pin does
NOT reproduce (review round 1, Issue 8):** the box's trips rode the JOINS
between the rows and the N = 8 ingest — shapes no in-process fixture
runs; the pin reproduces the CLASS (a pre-barrier wall + a decision
lateness past the 2-tick margin) with a PARKED DEVICE, and the derivation
prices whatever act produced the measured wall, so the join trips'
coverage is by construction. **The box re-run on this binary is what
says the derivation priced the box's term** — the laptop readings above
are the mechanism's.

**Review round 1, Issue 1 — a BUG in the first build's age law, FIXED
(`0c4b6be8`, pin `2a5709f9`):** `checkpoint_due_by_age` STORED the
decision's lateness on every `due` tick — the ticks that ran NO cycle
included — while `checkpoint_collected_ns` never advanced on an idle
volume, so the first cycle after an idle span adopted `wall + idle` as
its term, the 64-cycle MAXIMUM held it, the trigger saturated to 0 and
every later cycle's own ledger record made the chain self-sustaining at
one cycle per tick (the reviewer reproduced 4 s idle → 29 paced creates →
31 cycles in 1.5 s, term 2,489 ms, trigger 0 — the box's
between-rows→row shape on every writer). Now the verdict RECORDS nothing:
the tick hands the lateness to the cycle it RUNS
(`note_checkpoint_decision`, consumed by `note_checkpoint_cycle_term`)
and an idle `due` tick with nothing to cover ADVANCES the collection
instant (`note_checkpoint_collected` — an empty collection is a
collection: a leaf dirtied after it is the next cycle's); the belt caps
`late` at ONE landing ceiling (`AppenderSet::flush_ceiling_ms`, the
`late_cap_ns` of `checkpoint_cycle_term_ns`) — a decision later than the
whole ceiling is a stall the audit counts on the cycle it happens, never
a term the next 64 cycles anticipate. Pin
`sym_appender_tests::an_idle_span_is_never_a_cycles_term_so_the_first_
burst_after_it_runs_at_the_cadence` (4 s idle, then a paced burst: RED on
`dcc015e4` with the term 2,425 ms and the cycles at one per tick, GREEN
with the term 9 ms / trigger 991 and the cycles bounded), and the F-B1
pin gained its UPPER cycle bound (`max_cadence_cycles` — the first pin's
`cycles ≥ 4` alone would have passed the storm).

**Box verdict (the third pass, `b377cbb8`, 2026-09-23 — §3.9.5.2 /
§3.9.5.5): NOT FIXED — the tripwire trips with the derivation ENGAGED.**
`sym-scale`'s manager read `appender_flush_ceiling_overruns` +1 in the
N = 4 row and +1 in the N = 8 row, both on its second metadata volume,
region 0, at **1,127 / 1,125 ms** (25–27 ms past the ceiling, "with every
structural hold's capped overlap excluded", excused 0 everywhere) while
`meta_kv_checkpoint_term_ms` on volume 1 read **11 ms** (trigger 989) at
N = 4's start and **4 ms** (trigger 996) at N = 8's start — the horizon
EMPTY when each storm began — and **133 / 127 ms** (trigger 867 / 873)
only AFTER each trip: the overrunning cycle's OWN term (age 1,127 ≈
(1,000 − 11) + ≈ 138; 1,125 ≈ (1,000 − 4) + ≈ 129), read into the window
once it had run. Between the two rows the manager checkpointed 199 times
(≈ 1.5/s over ≈ 135 s), so the 64-cycle horizon had FORGOTTEN N = 4's
133 ms before N = 8's first storm cycle; and once the term was IN the
window the storm's steady state (62–83 grants/s over the heavier seconds
that followed) did NOT trip again. **The class is the FIRST storm cycle
after a quiet horizon** — the horizon's memory (a cycle count, ≈ 40–45 s
of quiet cadence at the manager) is shorter than the inter-row window —
not a burst the derivation cannot see in general: it priced the
sustained shape. **The burst is named**: both trips fall in the first
seconds of a JOINER CREATE STORM (the joiners m61 / m62's first storm at
N = 4, m63..m66's at N = 8) while the manager served an `ExtentGrant`
burst of **50–80 verbs per second** (62 grants logged in the second
03:08:16; 69 / 83 / 50 per second at 03:10:51 / :57 / 11:00 — 1,483 in the
leg, every one ≤ 8 extents: 845 × 4 / 180 × 3 / 147 × 2 / 2 × 1 the
reactive `needed.max(SMO_IMAGES_MAX)` asks and 103 × 5 / 54 × 6 / 44 × 7 /
108 × 8 the cadence's proactive `refill_due()` asks answered the derived
size, which is the FLOOR 8 for every wire joiner): over
the N = 8 create the manager's volume 1 served 344 grants + 318 returns
in ≈ 13 s with `manager_service_ns.execute` +2.36 s — 3.6 ms per verb,
each a ring-0 control entry + its barrier on the journal lane the
cycle's barrier #1 queues on. The joiners ask that often because —
**F-R5** (PR 13g the fix rung) — `grant_extents_for` derives a WIRE
joiner's grant from `set.region(id).smo_ewma_milli`, `None` for a wire
joiner → `ewma = 0` → §5.3.3's derivation is the floor 8 whatever the
joiner's SMO rate (the EWMA the joiner folds locally never travels on
`ExtentGrant`), and their rings sit at the **512 KiB floor**
(`appender_ring_bytes` 524,288, `appender_ring_grows` 0,
`joined_ring_grow_declined` [0, 1] — PR 2's drain-then-grow bound, PR
12b's declined growth on a joiner), so a joiner checkpoints ≈ 8×/s under
a 40k-file storm (m60 +109 checkpoints in 13 s, `appender_pressure_
cycles` +79), returns the images each barrier RETIRED (`take_returnable`,
`extent_grant_returned` +193) and re-claims at the SMO grain (+47 grants)
— claim-and-retire churn, ≈ 100 manager verbs per joiner per storm. **On
the fleets that run no joiner create
storm the derivation priced every term**: 0 increments on the two
3-writer touch fleets and the two 32-writer walls fleets (70
writer-legs) — the walls rewrite's faces read 50–119 ms on the busiest
joiners (r1 m71 119, m66 81, m65 65, m84 57; r2 m75 101, m86 74, m87 62 …)
with no trip — the derivation working where the horizon HOLDS the term;
the re-run's quiet-joiner trip (m60,
1,116 ms) and its rewrite trip (m65, 1,206 ms) did not recur. What §7
item 3 still owes: the manager's cycle term under the joiners' verb
service that starts INSIDE the cycle (a margin derived from past pass
walls cannot see it), and F-R5's supply grain beside it (a right-sized
grant makes the service ≈ 1 verb per joiner per storm). Every N-writer
row set that runs a joiner create storm on the box stops at r1 until
then; the row sets that do not (3c, 7) read clean.

**PR 13g (F-B1, the third box campaign's re-read — `fix/sym-joiner-supply-and-manager-term`, 2026-09-23): the CLASS the derivation above could not price, and the second term that prices it.** The box record's review (Issue 2) re-timed the third campaign's two trips: at each trip the anticipated term read **11 / 4 ms** (triggers 989 / 996) — the 127–133 ms was the trip cycle's OWN term read after — and 199 quiet cycles between the rows had pushed the previous row's 133 ms out of the 64-CYCLE horizon; the storm's steady state (the term in the window) did NOT trip again. The class is **the FIRST storm cycle after a quiet horizon**: a horizon measured in CYCLES forgets a burst that quiet cycles push out, and no horizon of PAST terms can price a cycle whose work exceeds every cycle in it. **The remedy is a second DERIVATION beside the horizon term, never a widened constant or a longer memory:** at every tick the cadence anticipates `max(horizon term, LIVE projection)`, the projection = the dirty nodes the pass will write × the measured per-node append wall + the images the pending commits PROMISED (§4.7's admission — `heap_promised` on the manager's heap, a region grant's `promised` on a leased slot's leaves) × the measured per-image SMO wall (`checkpoint::projected_flush_wall_ns`; `KvMetaBackend::checkpoint_trigger_ms_for(max_age, dirty)` takes the tick's own dirty count — one walk per tick, `dirty_node_count`). The units are measured per flush pass by CLASS (`checkpoint::FlushPassSample` — a node whose flush wrote fresh images is SMO work priced per image, every other node an append priced per node; attributed PER VOLUME through `SmoContext::images_written`, because the process-wide SMO / image counters fold every volume's — a mount's volumes and a fixture's daemons share them), and the unit in force is the horizon MAXIMUM over the passes that ran the class (`CycleTermWindow` per class — the term's own law: a bound anticipated by a bound; a mean unit under-prices every above-mean pass, and a machine that slows under a storm raises the bound at the first slow pass); a pass that ran none of the class measures nothing and the unit KEEPS what the last passes measured — what survives quiet by construction; a unit nothing has measured is 0 (a fresh mount's first storm cycle is the horizon's alone, the shipped posture until its first pass with the class). The ceiling never widens; a bit-17-absent volume's trigger is the max age verbatim. Published `meta_kv_checkpoint_projected_ms`, `meta_kv_checkpoint_{node,image}_unit_ns`, `meta_kv_node_images`. **The pin reproduces THIS shape** (`sym_n_daemon_tests::a_storms_onset_after_a_quiet_horizon_lands_inside_the_managers_ceiling` — the box's row sequence in process on the box's 32 MiB ring, `format_stamped_set_with_ring_len`: the fixtures' 1 MiB ring made every manager cycle the PRESSURE law's, so the age law never decided there): a `sym-scale` row (the manager + three joiners, one unpaced creator each, 3 s — the manager's units measured, a term of 31–65 ms in the horizon), 72 quiet `checkpoint_now` cycles (the term → 0–2 ms, the premise asserted: the horizon FORGOT), then the next row's onset for 4 s under the product cadence. At that shape the pin read **RED 1/3 on the MANAGER's law** with the projection off (age 1,132 ms = 1,022 before the collection + 110 ms cycle: the box's exact shape, on one run; the other two runs tripped the JOINERS' onsets, a term the pin no longer asserts — review round 1, Issue 6). The pin was then RESHAPED to force the class deterministically under PR 13e's method (a PARKED device at 3 ms/write): the manager storms `MINT_SPREAD` = 64 fresh directories per row with one PACED creator each (64 slot trees → 64 dirty root leaves per storm tick ≈ 260 ms of appends, two and a half margins — unpaced creators dirty hundreds of split leaves per cycle, the steady-state shape rather than the class), one joiner, a warm-up that measures both units and puts a term in the horizon, row 1 with the horizon holding it (the premise, no trip), 72 quiet test-side cycles (asserted forgotten), row 2's onset into fresh directories: **RED 3/3 on the manager's law at the pin's commit** (the onset lands its leaves 1,426 / 1,266 / 1,299 ms old), **GREEN 6/6 + 3/3 at the fix's** (the onset decisions read the projection over the dirty nodes + the promised images, the onset cycles landing inside the ceiling) — **and 1 in 9 RED at the rebased HEAD `4fd1db2f` (review round 2, Issue 19): one overrun with the onset term 471 ms against a 557 ms projection and a 443 ms trigger, the age decision ≈ 190 ms past its trigger — the dev profile's tick under CPU saturation, past the ceiling's two-tick margin (100 ms), a term the box does not have.** The pin now attributes it INSIDE itself: the decision's raw lateness rides the horizon as `meta_kv_checkpoint_late_max_ms` (the same 64-cycle window as the term), an overrun beside a lateness past the margin VOIDS the sample — stated with its lateness, the row re-drawn behind its own quiet horizon into a fresh directory set, bounded at three draws, a run with no valid sample failing loud naming the venue — and an overrun INSIDE the margin is the cadence's, the pin's failure; never a silent retry. At the pin's reshape: GREEN 3/3 at the first draw, the decision at most 40–61 ms late (0 void of 3 samples). **The pins' venue, stated:** `cargo test` is the dev profile, whose SMO costs 5–14 ms and whose per-node append doubles under the laptop's heat soak (node units 200 → 750–1,300 µs across one afternoon); the two cadence-timing pins home their volume on `/dev/shm` where it exists (`cadence_venue_dir` — the fixtures' btrfs file's `fdatasync` is 100+ ms and variable, the 165× substrate bracket), pace their creators to the venue, and assert the ceiling law on the MANAGER (the box's trip site) with the joiners' supply gauges beside it — a joiner's reading here carried the dev profile's tick lateness under CPU saturation (a decision 328 ms past its trigger with the SMO mutex free — the tick's own pre-decision work, PR 13e's "26–272 ms against a 50 ms tick"), a venue term the box does not have; the joiners' ceiling is the fleet proof's read. Every timing here is the mechanism's — the box's bracket on the flip binary is what judges the derivation. **Found by the touched suites and fixed in its OWN commit (`39f35963` — split from F-R5's pool commit at review round 1, Issue 6):** PR 3's carve trim — releasing the smallest NEW run while the union exceeded the page's `GRANT_RUNS_MAX` — starved a recycled pool whose singles held the page's four runs: every carve was released to nothing and answered as the remainder VERBATIM, and `maintenance_grant_refill` read "landed" off the non-empty verbatim answer and `continue`d the threshold drain for ever on the SMO the pool could not cover (`sym_slot_transfer_tests::a_leased_leafs_split_wider_than_the_one_smo_constant_is_refilled_to_its_need` hung); the whole carve is granted (the page names the pool's largest runs, the rest stay in the RAM pool — recovered as a dead appender's orphans by the record that names them) and "landed" means the region's pool GREW.

**Box verdict (the fourth pass, `230e95dd`, 2026-09-24 — §3.9.6 / §3.9.6.1): the third pass's CLASS is GONE; the tripwire is NOT 0 on this binary.** Two `sym-scale` row sets from zero on fresh fleets: set 1 read `appender_flush_ceiling_overruns` **0 on the manager and every joiner through N = 1/2/4/8** — the first gate-3 row set to reach its oracle on the box — with the onset class exercised (volume 1's term 6–7 ms / trigger 993–994 at N = 2's and N = 4's starts, the joiners' storms beginning) and not tripped, and N = 8's onset firing at 817–855 ms off a held between-rows term; set 2 read **+1 on the manager's volume 1 at 1,101 ms (1 ms past) at the N = 4 storm's END** — no verb service on that volume (one manager verb across the row; the grant burst of the third pass does not exist on this binary, F-R5's fix). **The snapshots against the trip (review round 1, Issue 1):** the create-end snapshot (`m0_pc41.json`, checkpoint 250) PRECEDED the trip's barrier by < 1 s and reads overruns [0, 0] with term 52 / trigger 916 / **projection 84** / lateness 15 — the PRE-trip state, the projection engaged; the trip cycle's own words entered afterwards — by the row's end (`m0_pn41.json`, checkpoint 256) term **151** / lateness **35**. No barrier face reads above 2 ms on the manager (`fsync_phase_ns.meta_barrier` mean 1.3–1.9 ms). The decomposition the faces support: **decision ≈ 916 + lateness ≤ 35 + pre-barrier ≈ 151 + barrier ≈ 0–2 ≈ 1,101 — the LIVE projection under-priced the storm-end cycle's own wall by ≈ 65 ms (the dirt the storm's last second adds after the tick decides, and/or the node unit under-measuring at the tail), the decision's lateness at 35 ms**; the cycle's pre-barrier wall is bounded in [52, 151], not read (the per-cycle tape of decision instant / pre-barrier wall / barrier wall is the instrument that discriminates, §7 item 3). The derivation prices the storm — every onset in both sets, the joiners' storms, the manager's own, the walls' 50–119 ms terms — and under-prices the storm's END cycle; the fixed two-tick margin absorbed the growth plus the lateness by ≈ 0–10 ms at this venue and was 1 ms short once in eight rows. §7 item 3's next piece: the projection's growth between the decision and the flush (a projection off the admission rate, or a re-read at the pass) and the lateness term.

**PR 13h (F-B1 — the fourth box pass's trip, `fix/sym-box-pass4-findings`, 2026-09-24): the trip's class READ OFF ITS OWN SNAPSHOTS — a wave of promised SMO images priced at a per-image unit the small passes before it under-measured — and the per-cycle TAPE that attributes the next one.** The fourth pass (`perf/sym-box-13g`'s record, §3.9.6.1 there) read ONE trip in eight rows on PR 13g's binary `230e95dd`: the manager's volume 1 at **1,101 ms**, at the N = 4 create storm's END, one manager verb on that volume across the row. **The box-pass review (Issue 1) overturned that record's attribution** — its create-end snapshot `m0_pc41.json` reads `appender_flush_ceiling_overruns` [0, 0] (taken < 1 s BEFORE the trip; the storms ended 04:45:21.16–21.996, the WARN is stamped :22Z), so its "term 52 / projection 84 / lateness 15" were the PRE-trip words and its "≈ 40 ms covering barrier" no face measured (the manager's `fsync_phase_ns.meta_barrier` mean 1.3–1.9 ms). **The decomposition the faces support:** `pc41` volume 1 — `meta_kv_heap_promised` **38**, image unit **2,042,480 ns** (2.04 ms), node unit 92.9 µs, projection **84** (≈ 38 × 2.04 + ≈ 65 dirty × 0.093 ✓), trigger 916, `pending_free` 44; `pn41` (after the trip) — image unit **3,971,074 ns** (3.97 ms), term **151**, `late_max` 35, `node_compactions` +45 over the six cycles between; and **38 × 3.97 = 151 — the trip cycle's own wall**: the wave of promised compactions cost what the trip's pass then measured per image, twice what the projection had multiplied by; `916 + 35 + 151 ≈ 1,101`. **The under-measurement is a bound violation in the code:** PR 13g's `flush_unit_ns` spread a pass's wall over a GRAIN of four (`wall / max(count, 4)`, review round 1 Issue 10b's hiccup bound), so a two-image pass read HALF its per-image cost and a one-image pass a QUARTER; under a create storm the passes that measure the image unit are the threshold DRAIN's one-or-two-compaction ticks (the cycles' own passes carried ≈ 0 images in the steady state — terms ≤ 52 ms), while the manager's 64 rotor leaves fill in LOCKSTEP (round-robin mints → equal bytes → every log crosses the node size in one interval) — a wave the cycle inherited at 38, priced at the halved unit. (Whether the box's 2.04 was a halved 4 ms or an exact 2 ms grown to 3.97 under the storm's end is what the tape below decides on the next trip; the floor is removed either way — a bound must bound.) **The fix (a derivation, no widened constant):** the unit is the pass's mean per item, `flush_unit_ns(wall, count) = wall / count`; `FLUSH_UNIT_COUNT_FLOOR` deleted; a hiccup on a small pass now OVER-prices the horizon (an earlier trigger, saturating to one cycle per tick when the unit is noise — the shipped posture's cost, self-healing when the pass leaves the horizon) where the floor UNDER-priced the wave against the ceiling, which is the promise. **The pin** (`sym_n_daemon_tests::a_wave_of_promised_images_is_priced_at_the_per_image_cost_the_passes_measured`, the box's 32 MiB ring on `/dev/shm`, every device BARRIER parked 30 ms — every SMO barriers its successor image (§4.10), so an image costs its barrier where an append costs a write, the box's 43× ratio between the two units): a PILOT leaf filled and compacted alone is the small pass (one image, 32–34 ms); twelve leaves in twelve rotor slots are filled in LOCKSTEP by rounds of one sub-threshold same-key xattr put (3 KiB — below the drain's 4 KiB enqueue, so the drain never visits and the cycle owns the compaction) + one explicit cycle until the round whose puts promise all twelve at once (`heap_promised` 0 → 12 in one round — the box's lockstep, pinned); the CADENCE's next cycle is judged off its tape. **RED on `bcf4ede5`**: the pilot's pass read **8,514 µs** per image against its **34,000**; the wave's decision priced 12 promised at 8,514 → projection 169, trigger 831; the cycle paid 405 ms of images (33,750 µs each) → landing 438 → **1 overrun**. **GREEN**: the pilot's pass read 32,297 against 32,000; projection 457, trigger 543, the same 438 ms cycle, **0 overruns**. Ties restated in `derivation_sweep_tests` (one image at 30 ms IS 30 ms per image; 38 × 3.97 ms = 150). PR 13g's onset pin, the landing pin below and `kv_backend_tests` (37/37 — the flat verdict untouched) GREEN on the fixed law. **The TAPE (the review's instrument):** `checkpoint::CycleTape` — every cycle records at its landing fold what its age decision DECIDED with (the dirty count it priced, the promised images, the two units, the projection, the horizon term, the trigger, the lateness) and what it PAID (pre-start, publish, flush by class with the counts the pass found, pages, barrier, landing, term); `.stats meta_kv_checkpoint_last_cycle` (one object per volume) and the overrun WARN line carry the same words, so a trip attributes itself with no snapshot to read on its right side (pinned: an explicit cycle carries no decision words and its walls sum to its landing; a cadence cycle carries the trigger, the dirty count and the projection). **The deferred-barrier fix landed in the same rung, stated for what it is (`bcf4ede5`, a real structural gap — NOT the box's term, ≈ 1–2 ms there):** the tick's step-2 deferred-mode flush barrier runs between the age decision and the cycle's clock since PR 13g's re-order, priced by neither `late` nor the term; the term is clocked from the DECISION now (`CheckpointDecision::decided_at_ns`) and the projection prices the covering barriers (`checkpoint::covering_barriers(strict)` × the measured barrier unit, `meta_kv_checkpoint_barrier_ms`); pinned RED 3/3 → GREEN on a 150 ms parked barrier (`a_deferred_flush_barrier_between_the_decision_and_its_cycle_is_priced_into_the_term`). A half-built "fixed term" (the cycle's publish + pages as a measured rest window) was DROPPED unbuilt — no face names the fixed part as the box's term, and the tape now measures it (`publish_ms`, `pages_ms`, `barrier_ms`). The ceiling never widens; the flat age verdict is unchanged. **Stated, not built:** the deferred barrier is NOT skipped on a due tick (a deferred-mode timing change on every layout); the projection's node term counts a promised leaf as an append too (a conservative double count, the safe direction). Every laptop timing here is the mechanism's; the box bracket on the flip binary judges.

### 4.4ao PR 13g — F-R5, FIXED (PR 2 / PR 3 / PR 12b, the armed plane): a joiner's extent supply under a create storm ran at the ONE-SMO grain on a floor-sized ring — the manager derived every wire joiner's grant off a rate it never saw

**Found on the box (the third campaign, `sym-scale` N = 8, 40k creates per writer in ≈ 13 s):** every joiner's ring sat at the 512 KiB floor (`appender_ring_grows` 0, growth DECLINED on a joined appender — PR 2's drain-then-grow owed), so a storming joiner checkpointed ≈ 8×/s on the ring's pressure law; 1,483 `extent grant` lines in the manager's log (845 × 4, 180 × 3, 147 × 2, 103 × 5, 54 × 6, 44 × 7, 108 × 8 — the ≤ 4 class the flush pass's reactive one-SMO ask, the 5–8 class the cadence's proactive 50 % refill answered at the derived size, which was the FLOOR), `extent_grant_returned` +193 ≈ the compactions (claim-and-retire churn at the SMO grain — `take_returnable` ships RETIRED images), ≈ 105 manager verbs per joiner per storm, each a ring-0 control entry + barrier at the manager (3.6 ms), volume 1's `manager_service_ns.execute` +2.36 s. **The root (the box record's review, Issue 3 v):** `KvMetaBackend::grant_extents_for` derived the grant from `set.region(appender_id).smo_ewma_milli` — `None` for a WIRE joiner (the manager holds no `AppenderRegion` for it) — so `ewma = 0` and §5.3.3's derivation answered the floor (8) for every production joiner regardless of its SMO rate; the joiner folded its own EWMA locally and never fed `smos_this_cycle` on its own flush pass, and the rate never travelled on `ExtentGrant { appender_id, want }`. **RED (`a635cb5f`, `sym_n_daemon_tests::a_joiners_extent_supply_under_a_create_storm_grows_its_ring_and_recycles_its_grant` — two real joiners on one volume, unpaced creators, the PRODUCT cadence alone):** the box's shape in process — 56 cycles in 8 s (7/s), the floor ring standing (`ring_grows` 0, `joined_ring_grow_declined` 43–46), `extent_grant_returned` +525 / +542 against 1,052 compactions per joiner, 147 wire grants per joiner of which 93–96 REACTIVE, the manager +390 verbs in 8 s. **Fixed in three mechanisms, each red-first:** **(2 + 3, `568bcc91`) the grant is a POOL and the ask is the joiner's own** — the joiner feeds its SMO rate on ITS flush pass and every ask names ITS derived size explicitly (`ExtentGrant { want }` — screened at the manager against `grant_extents_wire_cap`, the heap-share cap, PR 3's bounded-execution law: clamping to "the derivation's cap as today" would be the floor again, since the manager's own derivation for a wire appender is rate 0), the carve tops the pool up to the derived size, retired images RECYCLE into the region's own unclaimed set up to the derived size (a pressure-driven cycle returns nothing; the surplus above the pool alone returns), and the flush pass's reactive ask is a LADDER (the derived grant in the USER class on a healthy heap; one SMO's images in the INTERNAL class on the space class alone); **(1, `d65a6bd1`) the ring** — a joiner whose cadence is pressure-driven derives its ring off the commit rate it measures (PR 2's `clamp(2 × ewma bytes/s × max_age, floor, ceiling)`) and asks the manager for the next SEGMENT over the new verb pair `ManagerCall::GrowRing` / `ManagerReply::RingGrown` (the manager carves the longest adjacent run of the ask, refuses a run another appender holds, ZEROES it — PR 3's re-carve law — and journals the alloc deltas with the identity's hint as one control entry); growth is a REPLACEMENT under the joiner's closed gate, one step at least a doubling toward the derived size (the table holds eight segments — EWMA-sized steps spent them on a ramp), the drain wait running cover cycles between its waits (a pass parked at ring admission needs a cycle of this task to advance the tail — the first build held the gate while a pass parked) bounded by one landing ceiling, the joiner's page naming the grown table in both directory slots BEFORE any position is written under the new map (the directory-first law), then `JournalRing::grown_with`; **the rejoin** — tree 0's per-identity `appender_hint:` record (ring bytes + the largest derived grant the manager served that identity; written by `GrowRing` and by every derived-size wire grant above the floor, read at `JoinAppender` — PR 3's owed "persisted EWMA as the ring-size input") so a joiner that dies and rejoins starts at the ring and the pool its storm earned; the join's initial grant is `GRANT_EXTENTS_FLOOR + mint_slots` (the rotor it mints at its first touches plus the SMO floor — the join's known cost class) floored at the hint; the cadence's refill is promise-aware (`derived + promised`, capped by the wire cap). **GREEN (the venue-shaped load — two joiners × one creator paced to the venue × 12 s, the volume on `/dev/shm`, the ceiling law on the manager with the joiners' supply gauges beside it — §4.4an's PR 13g paragraph states the venue; 10 of the last 11 runs GREEN, the one red the strict cadence law since relaxed to the trigger bound):** rings 512 KiB → 2.9–3.6 MiB (3 grows, `grow_declined` 0), the cycle rate falling to the trigger in force once the ring is sized, landed extents per wire grant ≥ 2 × the floor, `extent_grant_returned` +0…+3 against ≈ 1,900 compactions, reactive asks 0, wire grants 3–10 per joiner, **the manager +58…+73 verbs per 12 s for TWO joiners (≈ 30–36 per joiner) against the base's 197 per joiner per 8 s (≈ 295 per 12 s) — an order of magnitude**, every acked name resolving at every daemon, fsck clean after every joiner left. The `appender_hint` codec is fuzzed (`slot_state_record`) with its proptest mirror; `GrowRing` / `RingGrown` ride `manager_call_frame` + the mirror. **Found by the touched suites (its own commit `39f35963`, split from the F-B1 fix at review round 1, Issue 6):** the pool law and PR 3's carve trim conflicted — see §4.4an's PR 13g paragraph (the whole carve is granted; "landed" means the pool grew). **Owed to the box (the flip binary's bracket):** the fleet's `sym-scale` reads — `appender_ring_bytes` above the floor on every joiner, `extent_grant_returned` flat, `joined_wire_extent_grants` ≈ the derived asks, `appender_flush_ceiling_overruns` 0 — the laptop's fleet proof is "it works".

**Box verdict (the fourth pass, `230e95dd`, 2026-09-24 — §3.9.6.2): FIXED on every joiner in both row sets.** Rings 768 KiB (the join's derived ring) to 2.3 MiB at the leg's end on every joiner and volume (29 / 28 `GrowRing` carves per leg with 1 / 7 short-run declines; the fresh joiners grow once inside their first storm; at set 2's leg END m64's volume-0 ring sat pinned at 1 MiB behind six consecutive `longest adjacent run is 1 extents` declines — the 1 GiB heap fragmenting as the leg proceeds, a PR 14 watch item), `extent_grant_returned` +0…+38 over a joiner's N = 8 storm against 118–184 compactions (the third pass's +193 ≈ the compactions), `joined_wire_extent_grants` 1–3 per joiner per row at 58 extents per grant (the third pass's +47 at ≤ 8), `joined_wire_reactive_grants` 0, pressure cycles +0…+11 on the sized rings (+25 on a fresh joiner's first storm) against 40–63 checkpoints per storm; the manager's N = 8 row **88 verbs / 15 grants / 13 returns / 0.114 s of `manager_service_ns.execute`** against the third pass's 892 / 449 / 387 / 2.93 s; the PR 13g hygiene set (`extent_return_run_cap_refusals`, `appender_stale_page_words_dropped`, `appender_pool_restored_extents`, `appender_pending_segments_returned`, `appender_join_residue_returned`) 0 across 14 rejoins; the closure exact set-wide once the departed first incarnations' join grants are subtracted from the manager's `extent_grant_extents` (a joiner's `granted` is not a published face — PR 14's stats sweep).

### 4.4ap PR 13h — F-R6, FIXED (PR 12b's reclaim path × PR 5's token planes, the armed plane): a joined writer's FORGET-driven reclaim priced and drove DESTROYS for inos in slots it does not lease, off its own stale projection

**Found on the box (the fourth pass, `perf/sym-box-13g`'s record §3.9.6.3):** m60 — a joined writer whose kernel had instantiated 40 k of the MANAGER's inodes as a token client (the leg's acked-writes check read the manager's tree through it) — met the manager's `rm -rf`: **6,782 / 7,266 `WARN squeezefs::routing reclaim of ino …: destroy WITHHELD — … pricing the destroy failed: corrupt KV encoding …`** per row set (`reclaim_destroy_refused_release_failed` +121 / +742), every one an ino of the manager's rotor slots (ino 41025607 → routing slot 69, 59899929 → 23, 59834453 → 83), three faces of one stale projection — a node read as zeros (an extent the manager had carved into appender 3's ring after its own leaf there retired), 27 / 45 consecutive extents `screened FOREIGN by rule 4` (the manager's former leaves re-granted to m63), and `tree 0 (slot Some(24) …, a PROJECTION here): traversal retry budget exhausted … restarts [root-seq] = 256` on 60+ slots (6,205 lines — defect 18 / 34's recycled-root class, PR 13b's "9/10" loop in production shape). Nothing was destroyed — and the fail-safe is NOT the withhold (the pricing read failing is an accident of the projection being UNDECODABLE): a DECODABLE stale projection prices the destroy and its tx reaches the joiner's commit DOOR, where PR 4 / 12b's foreign-slot refusal (`SlotBusy` → `slot_door_refusals`) is the structural belt — `slot_door_refusals` read **[0, 0]** on m60 at every snapshot through `m60_pend.json` (no priced foreign destroy reached the door in either set), beside the refuse arm's semantics (`finish_reclaim(plan, ReclaimFreeGate::Nothing, false)`: no tx, no free) and set 1's acked-writes oracle; the manager's log carries no corrupt read — a CPU + log storm on a path a token client must never take. **The mechanism, read off the code:** the unlink commits recalled m60's tokens, its recall sink invalidated + pruned, the kernel FORGOT, and `reclaim_orphaned_batch` ADMITTED every foreign ino — its divert `getattr` at the holder answered `nlink 0` — then `prepare_reclaim` read the layout through the divert and `reclaim_destroy_local` → `RoutedMetaBackend::destroy_entry_bytes` → `KvMetaBackend::destroy_entry_bytes` walked THIS daemon's projection of the holder's xattr tree (`range_kind(TREE_XATTRS)`: the LOCAL read the `corrupt KV encoding` came from), and where the pricing read anything `destroy_inodes_releasing` either judged the corpse LIVE off the same stale projection (`destroy_inodes: … has nlink 1, skipping`) or met the joiner's own door's `SlotBusy` — every outcome a withhold + WARN. **The law (design §5.1 — the slot is the ownership unit):** a non-holder never prices, releases or destroys a foreign slot's object; a FORGET of one is a TOKEN CLIENT's forget — the attr cache and the side maps go (as every FORGET's do), the token stays under the plane's own recall / eviction law, and the reclaim accounts NOTHING; the holder's own FORGET reclaims its corpse (and the mount-time corpse sweep, which already took this predicate). **The fix (the sites the review named — `SqueezefsFilesystem::reclaim_orphaned_batch`, the FORGET entry `queue_reclaim_inode` feeds, and `KvMetaBackend::destroy_entry_bytes`, the second face: a read of a foreign slot's tree that never takes the writer's divert):** `RoutedMetaBackend::owns_inode_reclaim(ino)` — `KvMetaBackend::inode_plane_owns_slot` (leased here, or unleased on the manager; `true` unarmed in one relaxed load) — at `reclaim_orphaned_batch`'s admission BEFORE `is_open`, the divert `getattr`, the plan's layout read and the pricing walk, so `destroy_entry_bytes` is never reached for a foreign slot's ino; a dropped forget counts `reclaim_foreign_slot_forgets` (`.stats`). The projection walk's budget exhaustion is COUNTED — `meta_kv_projection_walk_exhaustions`, `KvTree::descend`'s bounded `Corrupt` refusal on a projection (0 on the manager and every flat mount; growth names a path that still reads a projection) — defect 34's loop stays its own board item (item 13 / 18). **The pin** (`sym_n_daemon_tests::a_joiners_forget_of_a_foreign_slots_ino_prices_no_destroy_and_reclaims_nothing`): two sets of the manager's corpses, one reclaim batch at the joiner through the FUSE layer's `reclaim_orphaned_batch` — the FRESH set (unlinked + checkpointed BEFORE the join: the joiner's projection exact, the base's pricing succeeds into its door, `forest slot … is leased by appender 0 … SlotBusy`) and the TOKEN set (read through the divert — the manager's `grants_served` moves — unlinked at the holder: recalled; the base's pricing walks the STALE projection and `destroy_inodes` reads the corpse as `nlink 1` there). **RED on `230e95dd`**: `reclaim_destroy_refused_release_failed` +8 (the fresh set's `destroy WITHHELD` ×8), the token set's foreign tree walked. **GREEN**: refused 0, `reclaim_foreign_slot_forgets` +16, every corpse standing at the holder with `nlink 0` until the MANAGER's own FORGET path destroys every one (pinned), the joiner's own-slot corpse (its first touch of a second seeded directory) reclaimed as before; ×4 runs. **The unarmed law pinned byte-identical** (`a_forget_on_an_unarmed_mount_reclaims_every_corpse_as_shipped` — a FLAT volume and an UNARMED forest volume: every corpse destroyed by the FORGET path, the gauge unmoved). **Deviation from the brief's "drop the cache entry (the token plane's release)", stated:** the token is NOT released per FORGET — a FORGET is not a recall: a `Release` per forgotten object would be a wire storm under `drop_caches` / `rm -rf` (the box: 40 k), and PR 5's economy IS the token surviving the kernel's forget (the next lookup re-serves at 0 RPCs); the entry stays under the plane's recall / byte-budget eviction law. **The fleet proof** (laptop, "it works"): §10's `sym-scale` row on this binary. **Stated (review Issue 5):** neither the end-of-leg faces nor PR 13e's post-leave census RAN on `230e95dd` (set 1 predates `6108e8c1`; set 2's leg died on the `pend1` label) — the end-of-leg snapshots exist as `m*_pend.json` (`m60_pend.json`: withheld 7,266, `meta_kv_projection_root_refreshes` 79, `foreign_frames_screened` 45); the census's 0 exempted / findings 0 reading is the flip binary's row's.

**Review round 1, Issue 1 (Major, a bug in the law above — FIXED red-first, `edfd1852`): "the holder's own FORGET reclaims its corpse" had no live reclaimer on the UNLEASED arm.** `owns_inode_reclaim` answered "not mine" for an unleased slot on a joiner and DROPPED the forget, but the manager owns an unleased slot by the corpse sweep's law and sweeps it only at MOUNT (`sweep_unlinked_corpses`), and its kernel never FORGETs an inode it never held — so a joiner's unlinked-but-open file whose slot the cadence RELEASED before the close (the forced shrink of idle rotor slots as `writers_known` grows, the LRU release past the page budget, the region release — legal schedules) leaked its record, and on a data volume its blocks, until the manager's next remount, where the pre-PR door's first touch had destroyed it; the leased-foreign arm shared the hole one step later (a slot handed to another appender between the unlink and the FORGET — the new holder's kernel never held the ino). **The corpse-reclaimer law (design §5.1.3, as built):** the reclaimer is the slot's holder — the manager for a slot nobody leases — and a FORGET whose reclaimer is a peer TRAVELS there as a **reclaim hint**: the FORGET batch resolves each forgotten ino's home ONCE per slot (`RoutedMetaBackend::reclaim_home` — the MANAGER's word for the lessee via `reresolve_slot_holder`, never this mount's projection, which still named the departed lessee (itself) after its own release in the pin's first build; the endpoint through PR 6's `step_home_bound`; appender 0 for an unleased slot), reads nothing for a peer's ino, and after its own admitted work ships one `MetaCall::ReclaimHint { inos, hops }` per reclaimer (`MetaVerb::ReclaimHint` = 0xA0; ≤ `RECLAIM_HINT_MAX_INOS` = the FORGET pool's own `INODE_RECLAIM_BATCH_MAX`, tie-tested — a longer frame is rejected at the served side before any EXECUTION proportional to it — the S8 decoder allocates the hint's `Vec` up to the CONTROL class cap (1 MiB) first, so the class cap bounds the allocation and the screen bounds the execution (PR 3's bounded codec = bounded execution)); the served side (`serve_reclaim_hint`) hands every own ino to the FUSE layer's `queue_reclaim_inode` (the sink `main.rs` installs on every armed set — the reclaimer's own admission, plan and destroy, in its own ring under its own lease), FORWARDS an ino whose slot moved again for at most `RECLAIM_HINT_MAX_HOPS` = 1 hop (a count, so a lease ping-pong never loops a hint), and counts the rest misrouted; a hint that cannot travel is counted and the corpse stays for its reclaimer's mount-time sweep. **Gauges:** the forgetter's class split `reclaim_foreign_slot_forgets` (a slot another appender leases) / `reclaim_unleased_slot_forgets` (an unleased slot on a joiner), `reclaim_hints_shipped` / `reclaim_hint_inos_shipped` / `reclaim_hint_failures`; the reclaimer's `reclaim_hints_served` / `reclaim_hints_forwarded` / `reclaim_hints_misrouted`. **The classes and their pins (RED on `00edad1a`, GREEN on `edfd1852`, the two-daemon fixture):** (i) the UNLEASED corpse — `a_joiners_forget_of_a_corpse_in_a_released_slot_is_reclaimed_by_the_manager`: the joiner's file (its ROTOR slot — the creator's mint, not the directory's), unlinked (`nlink 0`), its slot released over the wire (`Unleased` at tree 0), the joiner's forget counts `reclaim_unleased_slot_forgets` +1 and ships one hint of one ino pricing nothing, the MANAGER's record is gone within its reclaim cadence with the destroy committed (nothing withheld), `reclaim_hints_served` +1, nothing misrouted, the post-leave fsck clean; (ii) the MOVED-slot corpse — `a_corpse_whose_slot_moved_between_the_unlink_and_the_forget_is_reclaimed_by_its_new_holder`: the manager first-touches the released slot (`setattr` of the corpse) before the forget; the forget counts `reclaim_foreign_slot_forgets` +1 and the hint lands at the NEW holder, which destroys it, nothing forwarded; (iii) the mid-`Releasing` corpse — a destroy parked at the door then met by `SlotBusy` and WITHHELD — is pre-existing, stated in §5.1.3, its remedy (the same hint at the withhold) owed. **The fixture is metadata-only**, so the record and the census are the in-process witnesses; the data-volume witness is the fleet leg's — **`tests/run_mw_matrix.sh sym-reclaim-hint` (review round 2, Issue 9 — the hint's PRODUCTION wiring: rung 7's step shipper, the S8 listener, `main.rs`'s sink, a real cross-process `ReclaimHint`), run ONCE from zero on the laptop, 2026-09-24, on `d99647a4` + the leg (`31f728d4`), fleet `create N=2 --symmetric --writers=3 --token-readers` on the tcp devsub with `SQUEEZEFS_SYM_AFFINITY_MAX_MB=64` (the leg's stated premise: a fresh joiner's derived `A_max` is one extent, exactly what a one-extent directory tree reads, so its files would mint into the ROTOR and no storm into the directory could move their slot; the parent-slot affinity is asserted per create off `affinity_mints`, tried up to 2 × V creates because a directory's children mint round-robin over the set's metadata volumes); GREEN, exit 0, zero residue after** (`/tmp/grok-justin/13h-r2-symrh.log`, rows `symrh-1790253251`): **(A) the box's shape** — m60 read the manager's 128-file tree as a token client, the manager `rm -rf`ed it: m60 `reclaim_foreign_slot_forgets` +128 → `reclaim_hints_shipped` 3 / `reclaim_hint_inos_shipped` 128 → the manager `reclaim_hints_served` +128, nothing withheld; the token reader m1 `reclaim_reader_forgets` 256 (its RELEASE-path probes and its FORGETs), `reclaim_hints_shipped` 0 there, refused 0 — Issue 2's posture on the wire; **(B) the MOVED-slot corpse (class ii)** — m60's 8 MiB file in `rh-ja` (its directory's slot by affinity), unlinked while OPEN; m61's 64-create bursts into the directory earned the offer and m60's release (`slot_handovers` +1 at the manager after 2 rounds; the offer had LAPSED — `slot_offers_expired` — because it stands 10 s while m60's recall rode its 10 s renewal beat, and m61's next ship first-touched the released slot at the door, `joined_wire_acquires` +1: the requester must stay LIVE for the slot to land at it, which the first run of the leg had not done — m61 idle, the slot sat Unleased, m60's close took the UNLEASED arm and the manager destroyed the corpse: the corpse-reclaimer law held, and the shape is PR 4 × PR 12b's offer-lapse derivation gap, §7 item 18 — the design's handover to the REQUESTER did not happen, and the closure `slot_offers ≡ slot_handovers + slot_offers_expired` double-counted the offer); then m60's close: `reclaim_foreign_slot_forgets` +1 → 1 hint / 1 ino → m61 `reclaim_hints_served` +1 and DESTROYED it — m61 `meta_kv_block_refs_released` +2, the manager `free_served_blocks` +2 (the joiner's terminal frees ship to the allocation holder), `data_alloc_bitmap_population` 127 → 125 (**the block witness: −2 for the corpse's 2 blocks**); **(C) the UNLEASED corpse (class i)** — the same into `rh-jb`, the slot handed to m61, then m61 LEFT cleanly (the slot Unleased at tree 0, `appenders_known` 3), then m60's close: `reclaim_unleased_slot_forgets` +1 → 1 hint / 1 ino → the MANAGER `reclaim_hints_served` +1 and destroyed it — `meta_kv_block_refs_released` +2 at the manager, `block_untracked_free_adjudicated` 2 (the blocks were m60's mints — PR 13 defect 27's gate), population 124 → 122; **the closure at rest**: Σ `reclaim_hint_inos_shipped` 258 ≡ Σ `reclaim_hints_served` 258 + forwarded 0 + misrouted 0 (m61's pre-leave 1 carried — a remounted member's counters restart), `reclaim_hint_failures` 0, `reclaim_destroy_refused_release_failed` 0 on every member, 0 `corrupt KV encoding` / `destroy WITHHELD` lines in any log since the leg began; the oracle clean (fsck findings 0, C8 drift 0, the must-stay-0 set flat on every writer); the post-leave census C9 = C10 = 0 over both volumes and the OFFLINE census findings 0 / `current_era_exempted` 0. **A cost face the leg read — misattributed in this round's first statement, corrected by review round 3 (Issue 17) and FIXED red-first:** 128 of A's 256 hinted inos came from the RELEASE handler's unlink-while-open probe (`queue_reclaim_inode` at every last close) on the joiner's `cat` of the manager's LIVE files. Pre-PR that probe's `getattr` on a joiner went through the read divert to the per-holder token plane, which served the object it had just read from its cache — a token HIT, 0 RPCs. Post-PR (round 2's build) each such close resolved the slot's home off the MANAGER (`reresolve_slot_holder` → `ResolveSlot` on a joiner, deduped per slot per batch) and shipped a hint the holder answered with a local read: the leg's own snapshots priced it at `slot_resolve_rpcs` +64 / `manager_verbs` +67 across the 128 inos of the read phase — ≈ one manager verb per foreign close over a 64-slot rotor at the default 64-ino batch, the verb class PR 13g's F-R5 had just cut to ≈ 30 per storm. **The arm (`d9a3cb7a` RED, the fix commit after it):** (a) the forgetter SKIPS the hint for a foreign-slot ino whose STANDING TOKEN reads `nlink ≥ 1` (`TokenReaderPlane::standing_nlink` — a LIVE cache entry under a live lease and a fresh recall channel, a pure cache probe; `data_grant::standing_token_nlink` walks the arm's per-holder planes; an unlink at the holder recalls the token before it commits, so a standing token with `nlink ≥ 1` is proof the file is live and there is no corpse) — counted `reclaim_hint_skipped_live`, the cache entry dropped as on every forget; (b) `reclaim_home` reads this mount's PROJECTION first and asks the manager only where the word cannot be trusted (it names THIS mount — the stale-self case after its own release — or `Unleased`); a stale third-party word is covered by the served side's one forward hop. Pinned in `a_joiners_forget_of_a_foreign_slots_ino_prices_no_destroy_and_reclaims_nothing`: 8 closes of the manager's live files → `reclaim_hint_inos_shipped` unmoved, the manager's `slot_resolve_rpcs` unmoved, `reclaim_hint_skipped_live` +8 (RED on `6a5b0ff1`: 8 hinted); the corpse shapes still hint. **The leg re-run from zero on the arm (2026-09-24, the round-3 binary; `/tmp/grok-justin/13h-r3-symrh.log`, rows `symrh-1790255754`): GREEN, exit 0** — the read phase's 128 live-file closes at m60 → `reclaim_hint_skipped_live` +128, hinted inos +0, the manager's `slot_resolve_rpcs` +0 and `manager_verbs` FLAT ([18, 12] across the whole A phase, where round 2's build read +64 / +67); the corpse shapes intact — A 128 forgets → 128 served at the manager, B foreign +1 → m61 served 1 (refs +2, `free_served_blocks` +2, population 127 → 125), C unleased +1 → the manager served 1 (refs +2, population 124 → 122); the closure Σ 130 ≡ 130 + 0 + 0 (258 on round 2's build — the 128 live closes no longer travel), failures 0, refused 0, 0 WARN lines, oracle clean, post-leave and offline census 0 / 0. The manager verbs the corpse forgets themselves cost fell to 0 as well: the projection-first resolve (arm (b)) trusts a third-party word, and a joiner's projection names the manager as the rotor's lessee. Torn down to zero residue. The wire: `MetaVerb::ALL` 20 → 21, the fuzz target `cluster_wire_frame` + the proptest mirror carry the call. **Review round 1, Issue 2 (Minor — FIXED red-first, `848c7362`): the `-o ro` token reader was OUTSIDE the law** — its lease gate is never armed, so the gate answered `true` for every ino and its FORGET ran the shipped reclaim: a divert `getattr` (a Grant RPC per forgotten corpse at the holder), the layout fetch, the pricing over its own image, and the read-only destroy refused into `destroy WITHHELD` + a WARN — under the holder's `rm -rf` of a cached tree, the wire storm the token deviation avoids, at the reader. A reader reclaims nothing by posture (S5): `reader_forget` stops the reclaim at `queue_reclaim_inode` and at the batch entry, counted `reclaim_reader_forgets`; pin `a_readers_forget_of_a_corpse_takes_no_grant_and_withholds_nothing` (RED: `grants_served` +1 at the holder; GREEN: unmoved, refused unmoved).

### 4.4aq PR 13h — the served-mutation hook's WARN storm (PR 13b's kernel hook × the fork's reply task): a notification's `ENOENT` is a counted outcome, never the "interrupted request" WARN

**Found on the box (the fourth pass, §3.9.6.3 there):** **272,071 / 275,372 `WARN fuse3::raw::session may reply interrupted fuse request, ignore this error No such file or directory (os error 2)`** per row set (set 2: m0 223,831 + m60 51,481 + 9–12 on each of the rest) — a SUBSET of `meta_ship.served_mutation_{invals,prunes}`, never twice them: m0 222,423 WARNs against 243,806 + 243,806 = 487,612 notify frames (**46 %**), m60 49,177 against 114,716 × 2 = 229,432 (**21 %**) — the fraction of PR 13b's hook's `FUSE_NOTIFY_INVAL_INODE` + `FUSE_NOTIFY_PRUNE` answered `ENOENT` by the kernel for an inode it does not hold (already forgotten, or never instantiated there — the expected outcome of a served mutation's invalidation reaching every object the holder's peers touched), logged per frame at `crates/fuse3/src/raw/session.rs:1092` (`reply_fuse` — the detached notify frames travel the reply channel via `ReplyTx::send_detached`) by the reply task's one `NotFound` arm, which could not tell a notification from a request's reply. **The fix (the fork, `crates/fuse3`):** `notify::frame_is_notify` — the out header: every notification carries `unique == 0` and its notify code in `error` (positive); a request's reply carries its `unique` and `0` / `-errno`; a frame shorter than a header is none — and `session::reply_write_verdict(is_notify, kind)`: `NotifyEnoent` → counted (`read_phase::notify_enoent`, `.stats` **`fuse3_notify_enoent`**), `NotifyFailed` (any other errno on a notification) → one WARN and the reply task GOES ON — a notification owes the kernel nothing; before, an unexpected errno on a notification ENDED the reply task with the session's replies behind it — `InterruptedRequest` → the shipped WARN, `Fatal` → the shipped `return Err`. **The pin** (`crates/fuse3 notify::tests::a_notifications_enoent_is_counted_never_the_interrupted_request_warn`): the classifier over the shipping encoders' frames (`inval_inode_frame`, `prune_frame`, a request reply's success / `-ENOENT` headers, a notify code under a unique, a short frame) and the verdict's four arms; RED on the shipped verdict (`InterruptedRequest` for a notification's `ENOENT`), GREEN after; the fork's clippy clean. No mount-class contract added (`tests/served_mutation_kernel_tests.rs`'s frame-order / form pins stand; the reply task's law is a pure function). **The two `.stats` faces the fourth pass stated:** `extent_grant_granted` exported (the closure's fourth term, `AppenderStats::grant_granted` — trivial; the pass had read the closure set-wide against the manager for lack of it); the writer's per-holder read planes on the `dlm_token_reader_*` fold need a census API on `data_grant::SlotCustodyArm`'s holder slots — NOT trivial, PR 14's stats sweep.

> The cloud row's FINDING record for F-C1 / F-C2 / F-C3 as first landed on `dev` (`662887a7`, the PR 15 redo — "FOUND, NOT FIXED, routed to PR 13i") is superseded here by the entries that carry both the finding and the fix; the known-vs-inferred evidence ledger (S/D/R) of the run itself lives in §3.10 and is unchanged.
**Rails on the final binary (`dde917f0` code; `8acbde32` the two-host protocol re-read), laptop = "it works", no number a verdict:** `cargo fmt --check`, `clippy --all-targets --all-features -D warnings`, `clippy --all-targets -D warnings`, `RUSTDOCFLAGS=-D warnings cargo doc` GREEN at every commit; `task check:fuse3` (244) / `check:fuzz` / `check:loom` GREEN at the fork fix; **the 43-suite matrix `tests/run_sym_forest_suites.sh` flat THEN stamped GREEN from zero (run 11 of 11 — runs 1–10 each stopped on one of the findings above, every one fixed red-first or attributed and re-run from zero; stamped/flat wall ratio ≈ 1.0 per suite)**; `run_nvmeof_fidelity.sh quick` **PASS=130 FAIL=0** (a21710c0's release binary); the 23 mount-class suites AS ROOT with `SQUEEZEFS_TEST_REQUIRE_MOUNT=1` — 21 GREEN, `mount_owner_override_tests` 6/7 root (its one red a non-root premise that passes unprivileged, where its mount half is what needs root here), `transport_geometry_tests` 2/4 root (two pre-existing zc-venue premises: `fuse3_zc_replies must stay 0 until the zc serve integration lands` — zc serves DO land on this kernel as root — and the 4 MiB-ent negotiation row, which kmbuf's 1 MiB ents do not take); **the UNPRIVILEGED gate fails every mount on this box**: the kernel's sqz zc surface is Present, every mount arms `IORING_REGISTER_KMBUF_RING(1 MiB × 32)` and refuses on `ENOMEM` under the user's 8 MiB memlock rlimit ("no silent downgrade") — a venue posture, root passes; the zc-capability gate AS ROOT with `SQUEEZEFS_TEST_REQUIRE_CAPABILITY=1` **PASS** (5 suites + the zcrx probes, skip ledger EMPTY); the fleet on the tcp devsub (`create N=2 --symmetric --writers=3 --lease-ttl-ms=15000`, from zero each): **`sym-crash` GREEN 2/2** (acked-writes oracle GREEN, every joiner followed the failover, deferred leaks converged, tripwires 0, fsck clean), **`sym-storm` GREEN 2/2** (ACKED 6,721 / 5,777, LOST 0, intents settled); **`sym-two-host` GREEN** on the final binary under the corrected comparison (9 → 12 ≡ 12), RED on the pre-F-C1 base (8 → 11 vs 12). **Perf scoping (the venue law — heat soak Tctl 40–92 °C across the rows; no verdict):** the in-process flat-path microbench A-B-B-A via the seam (A = `SQUEEZEFS_TEST_META_BUFFERED=1`, the base's buffered/unpadded shape; B = O_DIRECT + the pad law) at 100k ops × 4 threads, two brackets — medians B/A: create 20.7/20.6 µs, mkdir 26.0/22.5, rename 31.2/30.6, unlink 26.6/23.3, lookup/getattr equal to the 10 ns — inside the legs' own thermal spread (a leg's create read 16 or 25 µs on EITHER arm by clock state); `run_mdstorm.sh` A-B-B-A at 50 % scale on /dev/shm (A = the pre-F-C1 release binary `319f1f6b`, B = the final): create 3,873 → 3,335 ops/s (−14 %), mkdir 5,912 → 5,424 (−8 %), rename 2,670 → 2,410 (−10 %; the A legs themselves spread 2,979 / 2,360), unlink 3,279 → 2,855 (−13 %), stat par, manydirs 7,185 → 10,178 (+42 %), rmdir 3,018 → 4,225 (+40 %) — the serial one-tx-per-window phases carry the pad's page per window, the directory-heavy phases the park-kick drain; `sym-scale --scale-ns=1` A-B-B-A on fresh single-writer symmetric fleets: creates/s 2,604 / 5,057 / 3,996 / 7,759 (A1 / B1 / B2 / A2 — a 3× spread across the four legs by clock state alone), ingest 371 / 403 / 417 / 520 MiB/s, every row MET its own gate, `appender_flush_ceiling_overruns` 0 on every leg; the box bracket on the flip binary is the number (§7 (a)). Harness nit seen, not fixed: `run_mw_matrix.sh sym-scale` on a fleet with NO joiners exits `joiners[0]: unbound variable` after its table and gates (line 4578).

### 4.4ar PR 13i — F-C1, FIXED (design-level — `uring_fs` × every metadata door; the flip blocker): every metadata read and write rode the issuing HOST's page cache — a second kernel on the same LUN read stale blocks

**Found on the cloud row (§3.10), reproduced on the two-host fixture:** the guest's `squeezefs appenders --json` re-read page 0 one generation behind the host's own listing (48 / 57 / 61 / 65 against 49 / 58 / 62 / 66, ×4 on the base binary) after the host manager had taken ≥ 2 checkpoint cycles and quiesced — the product verb reading a whole, checksum-valid, STALE page out of its kernel's cache. Every cross-host read shares the shape: a joiner's projection of tree 0 and ring 0, another lessee's slot-tree nodes on the divert's fallback, the appender directory, the ledger a `-o ro` reader polls, the superblock.

**The fix ([design §5.12](../docs/design-symmetric-metadata.md)):** metadata device I/O is **`O_DIRECT` on every host from the first byte** — the device path REGISTERED at every door's first read (`superblock::classify_volume_slot`), at `format_v3_inner` and at `ImageBuilder::build`; the grain DERIVED (`statx(STATX_DIOALIGN)` → the block device's `logical_block_size` → 4096), the worker's fd cache posture-aware; a product write at a misaligned offset/length REFUSED (`meta_io_unaligned_refusals`, must-stay-0), a misaligned buffer bounced (`meta_io_bounce_bytes`), a narrow read widened into an aligned buffer (`meta_io_read_widened`). **The inventory verdict** — every on-disk unit but ONE was already 4 KiB-granular (superblock, appender pages, ledger slots, bitmap pages, node images AND bset appends — `append_bset`'s 4 KiB tail granularity is the sector grain's ceiling, so the brief's node-append RMW concern never held); the slot-tails SPILL image (`count × 12` bytes) is padded to the grain (`pad_to_grain`); **the journal ring is the one aligned-form change**: every window's END padded to the next sector boundary with an explicit checksummed PAD entry (`PAD_TAG` 0xFF + zeros, `seq == position`) that replay walks as a chain link, the admission claiming `max_pad = PAD_MIN + grain` of slack beside the entry and releasing the unused part at reserve (the CAS loop on a padded ring; the modelled `fetch_add` verbatim on an unpadded one — every ring before PR 13i and the buffered fallback), a window's ops coalesced into ONE aligned run per physically contiguous span, the ring's coverage arithmetic on `Reservation::padded_end()`, a writer's mid-sector replayed head aligned by ONE recovery pad (`align_head_for_writing`). **The ring-capacity cost, derived:** on a 4096-grain device every window pads to its page end (the 24 B page header makes the page end the only aligned in-page position) — ≤ 4,093 B per WINDOW, ≈ 2 KiB mean; on a 512-grain device ≤ 532 B; a serial workload (one tx per window) pays a page of ring per commit — the ring's runway in serial commits is its page count, which the cadence's pressure law absorbs (and a committer PARKED at ring admission now wakes the checkpoint task at once, never waiting for a long cadence tick — the `pending_free_pinned_floor_at_cap…` contract with its 60 s cadence and 8 MiB ring found the 40× faster fill parking a commit past the D1.b escalation); faces `meta_kv_journal_pad_{entries,bytes}`. The preflight and batch-bytes liveness clamps subtract the slack (a floor-size ring's `preclaim_ring_recovery` was unsatisfiable on a DRAINED ring otherwise). **The buffered posture survives as:** the loud, counted fallback for a REGULAR FILE on a filesystem that refuses `O_DIRECT` (`meta_io_buffered_fallback`, must-stay-0 on any block device — a block device that refuses `O_DIRECT` REFUSES the open) and the test seam `SQUEEZEFS_TEST_META_BUFFERED=1` (the pin's control arm). Harness: the byte-planting contracts (torn headers, flipped padding, smashed records) ride `uring_fs::patch_at` (a read-modify-write of the covering span — never a product path).

**What the matrix found under the padded law (each a fixed premise or a product law, red-first on the matrix's own leg):** (1) `kv_leaf_merge_tests` — a 1 MiB ring held its 3,000 serial creates at ≈ 230 B each and now filled at 192 with the suite's cadence parked at 60 s; the parked committer's wake reached the checkpoint task as a maintenance wake (appends, nothing reclaimed) and the D1.b escalation fail-stopped the volume twice over — **product law: a PARKED committer's wake under ring pressure is a TICK** (`ring_park_kick` set by `admit_user_budget`, consumed by the task; `ring_under_pressure` is `decide_checkpoint`'s own term; the cycle runs with `barrier_now`), a threshold wake keeps the shipped maintenance pass (the first cut promoted EVERY wake under pressure and cycled a half-full ring continuously — the storm the fixed cadence deadline exists to prevent; `sym_slot_transfer_tests`' first-touch contract caught it: "the ring never filled"); the suite's sandbox ring is sized for the padded population (16 MiB on 40 MiB — it drives every checkpoint by hand). (2) `sym_n_daemon_tests`' F-R5 storm — a joiner asked `GrowRing` every cycle once its derived ring (≈ 18× the byte-grain shape's) exceeded a limit the manager had already named (the per-appender ceiling, the set-wide `heap/16` budget, PR 13g's short-run class): 15 declines per joiner in 12 s — **product law: `grow_declined_at` latches the ring size a decline named**, the joiner re-asks only after its ring changed or a fresh stall arrived; the contract's law reads declines ≤ stalls + the manager's short-run declines. (3) `sym_crash_matrix_tests`' striped dead-lessee row — the flip's map + the seed's migration + 24 creates are ≈ 100 pages of the declared region's floor ring (64 windows), so the parked committer's kick covered the window before the kill (`entries: 0`): the fixture sizes the region's ring (`Knobs::ring_kb`). (4) `sym_mount_posture_tests`' release-bound contract — the padded storm stalled the floor ring fast enough for PR 2's growth to REPLACE it mid-storm, the test's ring handle went stale and the stale-hint arm read the GROWN page's length: the join-time bound reads the join-time ring length. (5) `readonly_mount_tests` — a create + checkpoint completes inside the poller's 1 ms interval under `O_DIRECT` on tmpfs; the second poll is placed one interval later. (6) The preflight (`preclaim_ring_recovery`) and the batch-bytes liveness clamp subtract the pad slack (a floor-size ring's preflight was unsatisfiable on a DRAINED ring — `kv_smo_crash_completeness_tests`' ring-full row). (7) `sym_n_daemon_tests`' four-writer storm — a wire verb (`GrowRing`, an `ExtentGrant`) whose ring-0 control entry found the manager's USER window full at that instant (`EntryAdmission::Try` — a verb holds the verb mutex and may not park; under the padded law the manager's own storm fills ring 0 a page per commit, so the window is full for a beat every cycle) was answered `Refused` with `JournalReserveExhausted`'s text and the joiner counted a wire FAILURE (`joined_wire_failures` 2 on two writers) — **product law: the manager answers the transient class `Deferred` (`STATUS_DEFERRED` → EAGAIN) and kicks its own checkpoint task with the park's mark** (the verb IS a committer parked at ring admission), **the joiner types it `KvError::WireDeferred`** (EAGAIN; a new variant beside `Busy`, in the errno table pin) and counts `joined_wire_deferrals`, never a failure; the joiner's growth log for the class is debug. (8) The F-R5 storm contract's laws re-read under the padded law: the joiners' rings reach the per-appender CEILING inside the storm (≈ 10 MB/s of ring per joiner at ≈ 2,500 creates/s against a 12.5 MiB ceiling on the fixture's volume) — the pressure law IS the steady state there (the sized-cadence bound is judged only under the ceiling), the rings' heap takes the wire cap (`free heap / (4 × appenders)`) down to the pool, so the pool's returns are the cap's (the churn law is judged only with the cap slack), and the declines are one probe per fresh stall + the manager's short-run class. (9) ONE venue flake, attributed, no product change: matrix run 5's flat leg read PR 13g/13h's F-B1 onset pin (`sym_n_daemon_tests::a_storms_onset_after_a_quiet_horizon_lands_inside_the_managers_ceiling`) RED once — 1 overrun with the decision 54 ms late (inside the pin's 100 ms margin, so its own law named the cadence), term 452 / projected 708 ms — on a laptop that had read Tctl 93 °C an hour before; the pin alone on the cooled box (65–71 °C) is 10/10 GREEN, 5 under the direct posture + 5 under `SQUEEZEFS_TEST_META_BUFFERED=1` (the base's shape), with indistinguishable faces (term 341/341/350 vs 364/304/312 ms, lateness 48/58/80 vs 63/45/40 ms, projection 522/485/580 vs 411/485/609 ms): the flush pass's per-node cost moved with the heat between the decision and the pass — PR 13g's stated venue term on the dev profile, independent of the posture; the matrix re-ran from zero with nothing beside it. (10) **F-C4 (§4.4av), a SHIPPED lock-order deadlock found by the growth contract's re-read:** `AppenderSet::stats` took `page` then `grant`, every grant writer `grant` then `page` — the `.stats` reader and the checkpoint task's reactive refill parked each other for ever (1 run in 25 once the park-kick cycle ran the refill beside the poll); one order now, `grant` before `page`; the growth contract's premises re-read (a growth DELTA, the doomed write under a test-held SMO mutex, a second storm's stall for the closing growth — 100/100 alone). (11) **§4.4aw, pre-existing, NOT this rung's:** the stamped leg's `fsck_c12_tests` storm contract chased the creator's tail for 281 s and filled its data volume — RED on the BASE binary ×2 and under the buffered seam alike (the venue's storm rate decides); the storm's lead over the census is bounded in the contract (400 files per run), the census-liveness remedy filed for PR 14. (12) **Matrix run 8's stamped leg — `kv_backend_tests::v3_ring_full_liveness_storm_drains` (R10) at 181 s against its 180 s bound (116–130 s flat, 179 s in run 7):** the park-kick promotion was gated on the pressure law `distance > logical_len / 2`, UNREACHABLE on a ring whose reserve is half of it (R10's 512 KiB ring: `checkpoint_reserve_bytes` = 256 KiB; user admission parks at `distance ≈ logical_len / 2`), so every park waited for the 1 Hz cadence and the storm's ≈ 80 laps drained at a lap per second — the pad law's extra laps on the forest volume pushed it past the bound. **Product law (red-first): the park's MARK alone makes the cycle due** (`decide_checkpoint` folds `park_kick` into `due` with an immediate barrier; the promotion needs no pressure test — a park IS the proof the admissible window is exhausted); pin `kv_backend_tests::v3_a_parked_committer_on_a_half_reserve_ring_is_drained_by_its_kick` (a 400-pair serial storm, three laps of a 512 KiB ring, the cadence parked at 60 s: RED at its 25 s bound on both legs unfixed, GREEN < 1 s); R10 reads **1 s flat / 2 s stamped** on the fixed task. (13) Matrix run 9's flat leg then read `sym_appender_tests::a_stalled_appender_ring_grows_a_segment_and_its_content_survives` RED 1 in ~15 (2/30 alone): its premise "two barriered cycles drain the ring" — a cycle whose flush pass compacts a slot leaf writes ONE entry into the region's ring, a whole PAGE under the pad law, so the cycle's own tail lags it by that entry (the decline log the growth decision now carries: `not drained (head 1221600 reusable_upto 1156448)` then `head 1225672 reusable_upto 1217528` — +4,072 B per cycle) and a storm whose leftover compactions span three cycles never drains in two; both growth contracts drive cycles until the decision fires, bounded at 16 (60/60 and 30/30 alone). The growth decision logs each decline's reason at debug (a pass inside / a window in flight; not drained with the three words; no internal extent — the stall consumed). (15) **The two-host pin's comparison re-read on the final binary:** the leg compared the guest's second listing against the host's word taken AFTER it, and a host cycle landing between the two listings (the forest's trailing root publications / the slot-lease cadence's page writes — none a `meta_kv_checkpoints` step the quiesce loop counts) read as RED on `dde917f0` (guest 9 → 12, host 13 after); the host's word is now taken BEFORE the guest's read and the law is `guest_after ≥ host_before` — GREEN on the final binary (9 → 12 ≡ 12, the host's word after 12 too), RED on the pre-F-C1 base under the same law (8 → 11 while the host had 12 before the read: the guest kernel's page cache one image behind). (14) Matrix run 10 reached the stamped leg's suite 41 of 43: the F-R5 storm contract's steady-state law read joiner 0 at 20 pressure cycles in the last quarter with its ring at 5.5 MB and ONE decline — the manager's `heap / 16` set-wide ring budget went to joiner 1 (12.5 MB) first, and the `grow_declined_at` latch held joiner 0 at its declined size; the law gains the arm the ceiling already had (a DECLINED ring under pressure is the pressure law's steady state — the budget is the same bound reached another way); 5/5 stamped alone.

**Pins:** `tests/run_mw_matrix.sh sym-two-host` phase 1 (RED ×4 on the base, GREEN on this binary — two kernels, one LUN), `tests/meta_io_direct_tests.rs` (6 mechanism contracts: the pad law over both field grains, the registration posture + derived grain, refusals / bounce / widening / `pad_to_grain` / `patch_at`, a padded ring's aligned runs + a replay walking its pads, an unpadded ring replaying direct + the writer's recovery pad, the seam), the loom model `journal_core_padded_reserve_lands_every_end_sector_aligned` (the CAS loop: disjoint back-to-back padded ranges, every end sector-aligned, `head == Σ padded lengths`, `admitted → 0`), and the whole 42-suite matrix flat + stamped under the direct posture (every KV sandbox registers at format; this kernel's tmpfs accepts `O_DIRECT` at the 4096 default grain).

### 4.4as PR 13i — F-C2, FIXED (PR 12b's join door): `open_joined_appender` discarded the `Joined` reply's grant word and rebuilt its RAM grant from the device page it read next — under F-C1 the STALE page

**Found on the cloud row (§3.10):** every joiner `granted 72 = claimed 72, unclaimed 0` from its join — write-dead — with the manager replaying each ask verbatim (`ExtentGrant`'s idempotency answered the page's word). The manager's own doc on `write_wire_joiner_page_grant` states the contract: the grant's runs reach the joiner ON THE REPLY; the page write is the durable home for the joiner's own restart. `open_joined_appender` (`backend/joined.rs`) ignored `ManagerReply::Joined { grant }` and ran `RegionGrant::recover(record, &page.grant)` off the device — honest only while the page read is (F-C1). **Fix:** `JoinedOpen.grant` carries the reply's runs from `wire_join_appender` into `open_appender_regions`, which recovers `record ∩ the reply's word`; the RAM page carries the reply's word; `unclaimed_remainder_of`'s page read for the §5.3.5 replay is honest under F-C1's fix and its doc states the dependency. **Pin (RED on the base with the cloud row's numbers — `grant_unclaimed: 0, grant_claimed: 72`):** `sym_n_daemon_tests::a_joiner_honours_the_joined_replys_grant_word_not_its_page_read` with the seam `TEST_JOIN_STALE_PAGE_READ` (a joined region open reads its page's PRE-GRANT image).

### 4.4at PR 13i — F-C3, FIXED (the conveyor's batch-failure fan-out, every layout): `clone_kv_error` flattened every error class but `Io` / `NoSpace` into `Corrupt` — a retryable `GrantExhausted` reached `mkdir(2)` as `EINVAL`

**Found on the cloud row (§3.10):** the joined writer's `mkdir` under the root answered `EINVAL` where the lazy mint's `GrantExhausted` is `EAGAIN` by the errno table; `SlotBusy`, `GrantDeferred`, `Busy` and `ManagerUnreachable` lost their class the same way — the batch's members were handed a hand-rolled clone of the failing member's error. **Fix:** exhaustive `impl Clone for KvError` (`kv/mod.rs`) and `impl Clone for SqueezefsError` (`error.rs` — `Io` rebuilt errno-first; `MockRedisError` derives `Clone`/`Copy`); `clone_kv_error` DELETED; `fail_batch` and the durability lane's per-window verdict use `.clone()`. **Pins (RED on the base with the cloud row's exact text — `Invalid operation: kv metadata: corrupt KV encoding: appender 1's extent grant is exhausted (0 unclaimed, 1 needed) … retry (EAGAIN)`):** `sym_manager_tests::a_batch_failed_at_resolve_by_grant_exhaustion_fans_out_the_grant_class_to_every_member` (four commits held behind the pass into an un-minted slot with the grant drained — one batch, the mint's `GrantExhausted` fails it — every member `EAGAIN`) and the table pin `posix_errno_tests::every_kv_error_class_survives_the_fan_out_clone_with_its_errno` (every class: discriminant, words, errno and `RefusalClass` survive the clone; `SlotBusy` → `SlotMoved`).

### 4.4au PR 13i — the two-host venue's fork finding, FIXED (the fuse3 fork's INIT ladder, every layout): `fuse.enable_uring` was set AFTER the INIT reply — a fresh kernel's FIRST mount registered nothing

**Found on the two-host fixture's phase 2 (the guest's FIRST mount ever on its freshly booted kernel):** `fuse-over-uring: kernel rejected protocol err=22` on every queue, `mark_ready called before all queues REGISTERed (2/2)`, dmesg `FUSE_IO_URING_CMD_REGISTER failed err=-22`, the required transport failed and the mount was refused — while the very next mount in the same guest succeeded. The fork's `ensure_kernel_fuse_uring_enabled()` (the auto-enable of `/sys/module/fuse/parameters/enable_uring`) ran inside `FuseOverUring::try_start`, which the session calls AFTER writing the classical INIT reply; since kernel 7.2.4 (stable `303b6eeedf29` — "decouple fuse_ring creation from ent registration", composed into the sqz series' patch 0014) the connection's FUSE ring is created at INIT-REPLY time iff the parameter reads Y at that instant, and `fuse_uring_register` on a ring-less connection answers `-EINVAL` — so the first mount after boot (the parameter at its default N) had no ring to register into, and the first mount's write of Y is what made the second succeed. Every venue before this one — the laptop, squeeze-test, the netns fleets — had the parameter at Y from an earlier mount, which is why the required transport never failed there; a fresh field host's first mount after a reboot would have. (A first reading of the failure blamed the sqz zc arm on qid 1 — a misattribution the fresh-boot re-run corrected: with zc declined every queue failed the same way.)

**Fix (`crates/fuse3/src/raw/session.rs`, `init_filesystem`):** the enable runs as step 0, BEFORE the INIT reply is written (a refused write is a WARN naming the remedy; `try_start`'s own call stays for the paths that reach it another way — idempotent). **Pins:** the fork's static rail `init_negotiation_tests::the_kernel_uring_enable_precedes_the_init_reply` (source order: the enable precedes the reply write) and `sym-two-host` phase 2, which records `GUEST_ENABLE_URING_BEFORE=N` and requires `FUSE-over-io_uring transport armed` on the guest's FIRST mount (RED on `3a4e9397`'s binary — the run above; GREEN on the fixed one). **A teardown nit seen only on the refused mount's path, stated not fixed:** after the failed arm's teardown ran the joiner's clean LEAVE (every slot released, the WERO registrant departed), a cross-owner transaction was attempted and refused `cross-owner transaction: this mount leases no rotor slot on the coordinator volume to home its intent in` — a mount-path op ordered after the leave on the FAILURE ladder; unreachable once the transport arms, PR 14's teardown-order item.

### 4.4av PR 13i — F-C4, FIXED (PR 3's surface, every layout): the `.stats` reader and the manager's extent-grant page update took a region's two mutexes in opposite orders — a deadlock that parked the operator's stats poll AND the checkpoint task for ever

**Found by matrix run 6's growth contract (`sym_appender_tests::growth_never_swaps_a_ring_with_a_stage_b_window_in_flight`), re-read under the park-kick law:** once the storm's drain was the checkpoint task's own kick cycle, the task's maintenance pass — its §5.3.3 reactive refill, `manager_extent_grant_class` — ran BESIDE the test's `stats` poll, and ~1 run in 25 hung for ever: the test thread in `Mutex::lock` inside `AppenderRegion::grant` ← `AppenderSet::stats`, the holder `sqz-meta0` at `manager_extent_grant_class` (`backend.rs:9703`) ← `maintenance_grant_refill` ← `maintenance_pass` (attributed by a temporary last-acquirer backtrace on `grant()` + `sudo gdb -p`; every other thread idle). `AppenderSet::stats` took a region's `page` mutex and THEN its `grant`; every grant WRITER — the `ExtentGrant`'s page update (grant runs added, the page's `grant` word rewritten from `page_runs()`), the joiner's `name_remainder_on_page` (`joined.rs:515`), the shrink's page naming (`joined.rs:2778`) — takes `grant` and THEN `page`. A lock-order inversion between the stats reader (the operator's instrument, polled every second by every fleet leg and every `.stats` read) and the manager's checkpoint task: both parked in `Mutex::lock` for ever, the volume's cadence dead behind them — a WEDGE the D1.b lattice cannot see (no committer parks; the task simply never ticks again). Shipped since PR 3 on every layout that grants (a declared partition, every joined writer); reachable in the field whenever a `.stats` read lands inside the ≈ 1 µs window of a grant's page update — the fleet legs never read it because their polls are 1 Hz against a handful of grants per storm.

**Fix (`appender.rs`):** ONE order — `grant` before `page` — is the region's lock law (stated on `AppenderRegion`'s doc; neither guard is ever held across an await); `AppenderSet::stats` follows the writers. **Pin:** `sym_appender_tests::the_stats_reader_never_deadlocks_against_a_grants_page_update` — a std thread polls `appender_stats` in a tight loop while the manager carves and returns a one-extent grant 300× (the answered runs returned before every ask so §5.3.5's idempotency never answers verbatim); RED 3/3 on the unfixed order within its 60 s watchdog (the verdict on the UNCAPTURED stderr + `exit(101)` — a panic would hang the runtime's drop on the worker parked in `Mutex::lock`; the driver is SPAWNED so the watchdog is polled by a thread that is not it), GREEN 5/5 fixed. **The growth contract itself was re-read** (its premises, not its law): growth is judged as a DELTA across the in-flight window (the storm's kick cycles may legally grow the ring before the doomed write), the doomed write reserves under a test-held SMO mutex (`test_try_hold_smo` — a maintenance-pass SMO racing it for the ring's head took the armed sector once in ~25 runs: the SMO's write was the one the shim held, under the mutex the cycle needs, a second hang shape), and the closing growth is driven by a second storm's own stall; 100/100 alone after the rewrite.

### 4.4aw PR 13i — FOUND, NOT FIXED (pre-existing on `b88bfa54`, every layout; PR 14's): an ONLINE fsck's census under a creator that outpaces its walk chases the tree's tail for the creator's whole life

**Found by matrix run 7's STAMPED leg (`fsck_c12_tests::a_healthy_packed_population_runs_c12_empty_ten_times_under_live_promotion`): run 5 of the ten online passes took 281 s (`scan_secs 281`), ended only when the storm's 4 GiB data volume filled (`write: Errno(28)` — 213,799 files by then), the test RED.** Attributed by a live sample: the census thread at 99.5 % CPU in `fsck::evaluate_c8` → `verify_durable_block_refs` → `derived_block_census` → `range_kind(TREE_INODES)` → `forest::range` → `tree::range` → `NodeSnapshot::next_live` — the walk pages the inode tree 512 records a fetch with a `getxattr(ino, "layout")` per ino, and the storm (≈ 760 creates/s in the dev profile on this box = ≈ 3,800 forest records/s into the SAME hot leaves) added more records behind each page than the page consumed; the cursor never reaches the end while the creator runs. Runs 0–4 (≤ 500 inos, one leaf) finished in 120–240 ms; run 5 lost the race and chased the tail until the creator died. **Independent of PR 13i:** RED with the direct posture (340 s), RED under `SQUEEZEFS_TEST_META_BUFFERED=1` (316 s — the pre-PR-13i I/O shape), and **RED ×2 on the BASE binary `b88bfa54` (316 s / 332 s)** built in a separate worktree — the venue decides the race (the pre-reinstall laptop's slower storm let the walk win at PR 13h's matrix; this kernel's does not). The class is a product LIVENESS item: an online `squeezefs fsck` on a volume with a hot creator has no bounded duration (and its census memory follows — RSS 4.3 GB at the sample), the "settle" window judging nothing while the walk never ends. **Remedy (PR 14):** bound every online census walk at the ino WATERMARK captured at its start — per slot on a forest (the slot's mint cursor; a record above it is current-era by definition, exactly what the C9 era floor and the C2/C3 allocation-epoch side map already exempt), the durable-refs side of the C8 oracle bounded by the same owner watermark so the two sides stay comparable — so a census costs O(records at start) whatever the creator does. **Harness (this rung, `bd…`):** the contract's storm keeps a bounded LEAD over the census (400 files per fsck run, then yields until the run completes) — the law under test is C12 under LIVE promotion, which the lead keeps; stamped 5–6 s, flat 2 s, 3/3 each.

### 4.4m Defect 16's regression, caught by the same batch and narrowed

`sym-shared-dir-ls` on the defect-16 binary read `meta_kv_node_cache_
misses = 1,219` on the reader against the law's `dropped + 8 × epochs +
K = 324` — "a DATA leaf was read for the listing". `ls -l` probes the
ACL names per file (`getxattr` / `listxattr` of `system.posix_acl_*`),
and defect 16's fix read the CONTROL names off the reader's projection
at EVERY `listxattr` — one projection leaf per listed file. The control
class lives on ino 1 alone (and its slot-0 guest keyspace after a
migration): the merge is now confined to those inos; every other
object's listing is its token's, no leaf read. The failover pin's second
half (ino 1) stands; the `-ls` law is judged again in the from-zero
batch.

### 4.4f Harness — `sym-foreign-touch` LIVE's storm died at launch

On the defect-10 binary the LIVE phase read `slot_handovers` 1 "a live
holder was recalled by a touch": the holder's storm was launched BEFORE
its target directory's `mkdir -p` and died on its first `mkdir` (ENOENT,
`live-a.txt`), so the "live" holder was IDLE and the law moved the slot to
B correctly (`N_floor` 15, the dominated arm, `rotor_mints` +1 at A over
the phase). The leg creates the directory first, runs the storm in
ROUNDS (a fresh subdirectory each — one storm of `SYM_FILES` ends in
seconds at the holder's own rate) for the phase's whole length, and
refuses the LIVE verdict if the storm is not alive at the last touch.

### 4.5 `appender_flush_ceiling_overruns` (must-stay-0) — 2 overruns, venue-attributed pending the box

At the tail of the N = 8 (and once the N = 4) create storm the manager's
volume 1 counted 2 overruns: the oldest dirty slot-tree leaf aged 1,110 /
1,129 ms at the covering barrier against the 1,100 ms landing ceiling
(trigger 1,000 + 2 × 50 ms ticks) — the checkpoint task's pass ran 10–29 ms
past its 2-tick margin under 64 creator threads + 8 daemons on the throttling
32-CPU laptop (`manager_load_pct` ≤ 2 %). The margin is
`checkpoint_landing_ceiling_ms`'s fixed 2 ticks, not a measured pass time. If
the box row trips it too it is a PR 14 item (derive the margin from the
measured pass wall); the sym-scale leg reports it per row in the VERDICT
column (per-row deltas — a previous row's count never bleeds into the next).
On the final binary's from-zero batches the class moved to `sym-walls`
row (a) — seven joiners rewriting 7 GB into zram over nvmet-tcp at once:
attempt 5 +1 on m61, attempt 9 **+1 on m61 at 1,105 ms (5 ms past the
ceiling)**, attempt 10 **+1 on m0 (the manager, the allocation holder
serving 2,723 frees) at 1,227 ms**, attempts 7 and 8 none; `sym-scale`
N = 8 read 0 on every final-binary run. The two laws of the row (the free
wall, the join storm) held on every run; **the gauge's reading on this
box is venue-attributed pending the box** (`51bf21e1` names this exact
gauge — "a flush-ceiling overrun … is venue-attributed pending the box,
never a MISS and never a MET"), so since fix round 1 the harness's venue
word (`run_mw_matrix.sh --venue=laptop|box`, Issue 2) REPORTS it per
round on the laptop instead of failing the leg, and it stays must-stay-0
on the box — with the PR 14 item (derive the margin from the measured
pass wall) written up in §7.

### 4.6 Harness findings

* A joiner whose volume fail-stopped WEDGES the product `umount`
  (`mw_fleet.sh unmount` hangs; `fusermount3 -uz` is the teardown) — an
  operability note for PR 14.
* The manager's `ExtentGrant` control entry admits `Try` in the USER class;
  under eight reactive refills at once on a small ring it answers
  `JournalReserveExhausted` (the caller's retry class, `joined_wire_failures`
  — 140–154 per joiner in the in-process two-round storm), retried at the
  joiner's cadence. Not a defect; stated because the in-process heavy pin
  reports it.
* `kv_freeze_wedge_tests::a_dropped_forced_compaction_leaves_the_node_
  freezable` was RED under `SQUEEZEFS_TEST_STAMP_SYMMETRIC=1` (green flat)
  on this tree AND on the base: its census probe was flat-shaped —
  `candidates.iter().find(|(t, _)| *t == TREE_INODES)`, while every forest
  slot tree's id is 0 (PR 1: the kind bytes are the tree ids; PR 4 round 4
  made the census resolve a leaf by id AND slot). Not a product finding.
  **Closed in fix round 1 (Issue 17)**: the probe resolves its tree
  through the ONE locator and arms the SMO seam by id or by SLOT; the
  suite is in the matrix (41 suites), green 6/6 on both legs.
* **The harness law** (stated ONCE here; §3.8 and §4.4ac refer to it):
  never edit `run_mw_matrix.sh` while a batch runs it — bash reads the
  running script incrementally, so an edit (a revert within a minute
  included) shifts the tail the running leg has not yet read: attempts
  12–14's `sym-crash` exited 2 (`syntax error near unexpected token`)
  AFTER their 10/10 GREEN verdict lines. A batch's harness is frozen for
  its whole run.
* The `SupplyStripeIno` path declines a creator "no endpoint bound on this
  mount" instead of binding it on demand (`bind_holder_endpoint_on_demand`
  runs at the guards and the steps, not at the supply): the holder mints
  the remainder, so the flip is unaffected — 63 of 64 stripes land in the
  holder's rotor instead of the creators'. Owed to PR 14 (one call at the
  supply).

## 5. SIM-1 (gate 8) — `SimConfig { clients: 12_500, shards: 64 }`, release, dev box (measured-simulated, tier (ii))

`sym_block_grant_tests::sim1_at_the_operating_point_12500_members_over_64_shards`
(`--ignored`; `run_sharded` with the slot-lease carriage, the broadcast token
recall, the death ledger and the free-grace V-fan-in — landed in `502aa859`):

```
clients=12500 beats=2 renewals=25000 wall=0.077s (324121 renewals/s offered)
volume-0 journal tx/s: 0.000 (delta 0 entries)                      ← the S6 gate: liveness off the journal
renewal latency (WITH the slot-lease carriage): p50 0.41 µs, p99 0.58 µs, max 10.21 µs
grace completion: 547.78 µs; shards=64 parked=196 reclaimed=196 park_expiries=0
slot-lease carriage: 50000 lease word(s) on the grants (M = 2 per member per beat)
token recall fan-out: 12500 holder(s) of one object recalled in 24656 µs, 12500 ack(s)  (in-process — no wire RTT)
death ledger: 12 record(s), sink ≤ 15.16 µs, poll cadence 1100 ms, read by 64 shard(s)
free-grace fan-in: every shard closed on the label in 5925 µs
```

Envelope: beat p99 0.58 µs (S6-a's envelope is the wire RTT-dominated one; the
in-process number is the CPU term), eviction fan-out 12 deaths → 64 shards
within one poll cadence (1,100 ms), the V-fan-in closing in 5.9 ms on the
on-change cadence, parked ≡ reclaimed, expiries 0.

## 6. The D1 arithmetic at fleet N vs §5.10, and the 15 k arithmetic re-derived

Every row is design §5.10's per-op law read off the fleet's gauges (the
dev-box tcp substrate — tier (ii) measured-simulated; the box brackets of
§8 are the counted rows, and every number here is a constant the box
rows re-measure, never a verdict):

| §5.10 row | Design | Measured (fleet, this record) | Holds? |
|---|---|---|---|
| `create`/`mkdir` under an OWN directory | 0 wire verbs | `sym-tarx`: **0.0122 wire verbs per entry** on 2,468 entries (30 manager verbs = the join + the extent-grant refills; `xv`/`ship`/`pub` 0), `slot_handovers` 0, `dlm_rpcs` 0 | yes — the ≈ 0 law (gate 2's bound 0.05) |
| `create` in a SHARED (striped) directory | 1 ship + 1 barrier per foreign create; never a handover | `sym-shared-dir`: 20,000 creates by 8 writers, `xv_shipped ≡ xv_served` (17,537), `dir_stripe_ships` 17,204 (the 1/64 own-stripe lands are the difference), **`slot_handovers` 0** after defect 13 | yes |
| `lookup`/`stat`/`readdir` of foreign objects | 1 token per object first touch, 0 while cached; `readdir + stat` of a striped directory = `K + C` tokens cold | `sym-shared-dir-ls`: **20,067 grants for K = 64 + C = 20,000** (K + C + 3: the directory, its parent, the root), `dir_stripe_readdir_merges` 43, 0 data-leaf reads (the reader's `node_cache_misses` = the S5 poll's re-reads + one tree-0 lessee read per stripe slot), `dlm_token_hits` 284,483 | yes |
| foreign touch of an IDLE tree (`mkdir /jobs/X`'s shape) | 1 handover if idle, then 0 | `sym-foreign-touch` IDLE: handed over after 1–2 bursts of 64, `slot_handover_phase_ns` total **5.5–7.8 ms** (flush 1.9–4.1, page 0.15–0.18, tree 0 3.5) | yes; the cost inside §1.6's 5–20 ms |
| foreign touch of a LIVE tree | ships, never a handover | `sym-foreign-touch` LIVE: 192 ships, 0 handovers with the holder's storm alive | yes |
| Manager verbs per volume (per-op work = 0) | steady ≪ 100/s | `manager_load_pct` 0–2 % at N = 8; the eight-writer in-process storm's grant/return refills 100–150 per joiner per run under `JournalReserveExhausted` (the retry class) | yes; the manager's ring window is the N = 8 storm's only pressure point (§4.6) |
| Recall fan-out (holder side) | R recalls per mutated object, batched per pass | `sym-readers` (1 reader): `recalls ≡ mutations × holders` 5/5, `fanout_p99` 1, `recall_rtt_mean` 125.5 µs; SIM-1: 12,500 holders recalled in 24.7 ms in process | yes (the N = 32 reader shape is §7's) |
| Handover reclaim by a bursty owner | ≤ 1 handover per burst per direction | `N_floor` ≈ 18 after the first measured handover (5.5 ms ÷ ≈ 0.3 ms ships) — defect 10's seed made it 2 for a joiner's life before | yes after defect 10 |
| Aggregate creates scale with N | ≥ 0.7 × N × the N = 1 rate | `sym-scale` r5 (2a94abbc): 6,874 / 19,192 (2.79×) / 27,909 (4.06×) / 39,778 (5.79×) creates/s at N = 1/2/4/8; ingest 2,481 / 5,679 / 8,015 / 10,309 MiB/s — laptop readings, SCOPING | **venue-attributed pending the box** (`51bf21e1`) — neither law takes a verdict here; the box row decides |

**The 15 k arithmetic re-derived** (§1.6's table, the constants this
record moved): `N_floor`'s cold start is no longer 2 on a joiner (defect
10) — the handover price at the operating point is the MEASURED 5.5–7.8 ms
÷ the served ship's ≈ 0.3 ms ⇒ ≈ 18–26 ships, so a `/jobs/X` touch never
moves a stripe (defect 13 makes it moot: a striped directory's ships feed
no window) and an idle tree moves only to a requester past that floor;
the per-holder ship cost stands at the §5.10 shape (1 ship + 1 barrier;
`meta_ship_owner_phase_ns` on the fleet ≈ 0.3 ms served) so 12,500
`mkdir /jobs/X` over 64 stripe holders is 195 × 0.3 ms ≈ **60 ms of each
holder's time** per wave (§1.6 wrote ≈ 6 ms at a 30 µs verb — the served
INSERT is a commit + its durability lane, not a verb: the 10× is the
barrier, and it is per WAVE); a reader's `ls -l` of the result is `K + C
+ 3` grants (§5.7.5's `K + C`, exact to the constant); death propagation,
the join storm and the ring budget are untouched by this record (their
rows are gates 7/8b's — §7). Nothing in §1.6 breaks first at a different
resource than it did.

## 7. Owed (what PR 14 / PR 15 inherit)

> **Status (PR 13i, `fix/sym-shared-lun-coherence`, 2026-09-24 — the cloud row's three findings, each FIXED red-first; §3.10, §4.4ar–at):** **F-C1** — metadata device I/O is `O_DIRECT` on every host with a derived grain and an aligned form for every shape (the journal ring's sector-pad law the one aligned-form change; design §5.12), pinned by the two-host qemu/KVM fixture (`sym-two-host`, RED ×4 on the base) and `tests/meta_io_direct_tests.rs`; **F-C2** — the joiner honours the `Joined` reply's grant word; **F-C3** — the fan-out keeps the error class (`KvError` / `SqueezefsError` are `Clone`; the flattening helper deleted). **New PR 14 inputs:** (a) the ring-capacity cost of the pad law on a 4 KiB-grain device — a page per WINDOW, so a SERIAL workload's ring runway is its page count (8,192 windows on 32 MiB); the flat-path scoping rows on the laptop are this rung's, the box bracket on the flip binary judges the serial-commit shapes (`mdstorm`, `w_fresh`); (b) the joiner floor ring (512 KiB = 64 pages of windows) churns cycles under serial creates until PR 13g's growth catches up — `appender_pressure_cycles` on the two-host leg reads it; (c) **the cloud re-run runs on a binary whose `sym-two-host` is GREEN first** — the fixture is the cloud row's shape on two kernels for the price of a laptop. (d) **§4.4aw — the online fsck census's liveness under a creator that outpaces its walk** (pre-existing; RED on the base): bound every online census walk at the ino watermark captured at its start (per slot on a forest; the C8 oracle's durable side by the same owner watermark) so a census costs O(records at start); until then an online `squeezefs fsck` on a volume with a hot creator has no bounded duration and `fsck_c12_tests`' storm contract keeps a bounded lead over the census. (e) **§4.4av F-C4** is FIXED here (the region's `grant`-before-`page` lock law) — a lock-order rail over the appender region's mutexes (the `kv_loader_lock_style_tests` shape) is the cheap belt PR 14 could add.

> **Status (PR 13b, `feat/sym-metanode-ship`, 2026-09-21): the three flip-blocking items below are CLOSED on that branch — (i) §4.4z in `cd85f701`, (ii) §4.4af in `cc642b6a` (attributed: a page-published slot root pushed off the page — not the rename/intent machinery), (iii) §4.4ag in `53ffb626` + `31519ceb`; the fleet legs on that branch's final binary (`b806b9a5`, laptop — "it works" evidence): `sym-foreign-file` GREEN ×3 from zero, `sym-crash --rounds=3` GREEN 3/3 from zero, `sym-storm --rounds=3 --victims=7 --cross-owner --striped` GREEN 3/3 from zero; **the storm ×10 is NOT MET** — three attempts read 8/10 (the rig's 1 GiB metadata namespace exhausted by the rejoin slot-tree economy — owed to PR 14, stated), 2/10 (a harness precondition, fixed) and 9/10 (a joiner's tree-0 projection routing loop, defect 34's class — owed), LOST 0 in every one of the 19 seven-victim rounds, so §4.4af's class did not recur. Three further defects the legs found were fixed red-first on the branch (the writer's dirty layout under a token recall — an acked-write loss; the holder's caches + writeback-cache kernel after a served mutation; a step whose slot moved TO the initiator mid-plan applying unguarded). PR 7's un-share of a surviving sole owner stays PR 14's (the reason is stated in `docs/operations.md`'s PR 13b section). The text below is the ledger as recorded.**

**Flip-blocking (three items — §4.4z defect 32, §4.4af finding 1, §4.4ag
finding 2; §9 lists them as the flip's preconditions):**

**(i) §4.4z, defect 32:** the record-level
metanode arm — a foreign-slot FILE's `setattr` / `setxattr` / layout
publish from a mount that does not lease its slot ships to the holder
(design §5.10's "1 custody grant + 1 publish ship per layout publish";
the door's own "ships to its holder" text). Today (the interim posture
since fix rounds 1–2): the ARMED plane REFUSES the class LOUD with the
typed `EREMOTE` naming PR 13b (§4.4ai — never `EOPNOTSUPP`, the class
coreutils' `chmod`/`chown` swallow as "not applied") — the `open(2)` for
write of a colleague's file fails at the open (so `>>` / `dd` /
`truncate` never ack a byte), `chmod`/`touch`/`setfattr` refuse at the
syscall AND their exit status says so, and the
`write` / publish gates stay as the belt for an fd opened before the slot
moved or the il shim's ring path (there a write is refused at `write(2)`
only on a `--no-writeback` / `O_DIRECT` / `O_SYNC` path, else at
`fsync`/`close` through the kernel's errseq); the kernel's ctime-only
times echo is absorbed; two transient shapes (a slot mid-handover to
this mount, a joiner's lagging lease projection) answer `EREMOTE`
where `EAGAIN` would be honest. Fix shape: `daemon_verb_
router` + the publish shipper keyed by SLOT HOLDER through PR 6's
`step_home` (tree 0's lessee + the endpoint table) for the record-level
verbs only (the namespace verbs keep PR 6's intent arm — the S8 router's
`create` would bypass the creator's-rotor mint); the served side under
the holder's lease and door, composing the publish's custody scope with
PR 9's grant at that holder and recalling the object's tokens (the
writer's own included); PR 7's un-share beside it. Venue: the two-backend
fixture (`sym_n_daemon_tests` — a joiner's `setattr`/`write` on the
manager's file and the reverse) + a fleet leg (`sym-foreign-file`: N
writers `chmod`/`touch`/append a colleague's files under the acked-writes
oracle). A rung-sized item; PR 14 cannot flip before it lands.

**(ii) §4.4af, fix-round finding 1 (item 13 below):** the acked-writes
LOSS across a seven-victim kill with the cross-owner mover into striped
destinations — 25 of 17,376, one writer's moved files, unattributed; a
default cannot flip on an unattributed acked loss. PR 13b's first item;
the storm ×10 count restarts from zero on its fix.

**(iii) §4.4ag, fix-round finding 2 (item 14 below):** a joiner's user
read answering `EIO` for one beat inside a manager failover — a gate-4
PRECONDITION (its "refusals 0" law reads a user op failing on the death
path as a violation, whatever the window's width) that PR 13b clears
beside defect 15's follow.

Product (each named to its rung, none flip-blocking — every one has a
counted decline, a bounded window or a stated venue):

0. **The N ≥ 4 create-rate band on the laptop** (§3.1): the same code
   read N = 4 at 4.57 × and 2.20 ×, N = 8 at 3.17 × and 5.99 × on
   consecutive from-zero runs — a SCOPING read, venue-attributed pending
   the box (`51bf21e1`); no local read narrows it, the box row is the
   number. **PR 13c:** the box read 4.27× at N = 8 and §3.9.3 attributes
   it to the CO-LOCATED venue (8.4 of 32 cores, every RAM-only phase
   +40–58 % per op uniformly, no queue); the per-daemon-CPU-second face
   is that venue's proxy — **the per-NODE law ("bounded by no node") is
   UNMEASURED on any venue: a multi-node venue, PR 15's cloud row (one
   node per writer), is its instrument** — owed there, never a box row.
   **PR 15 (2026-09-22): the instrument is BUILT — `tests/cloud_bench_cluster.sh
   PRESET=mw SYMMETRIC=1 N_CLIENT=<n>` (`assemble-sym` / `bench-sym`, one
   symmetric writer per client node, the manager on client0 and a JOINED
   writer per other node through the ladder over the real wire) +
   `tests/cloud_sym_rows.sh` (gates 2 / 3 / 3b with the matrix's own laws
   via `tests/sym_rows_lib.sh`); the local functional pass ran on the
   laptop's fleet as "it works" evidence; NO cloud minute spent — the
   launch awaits the owner's expressed approval for that run (S1 =
   `N_CLIENT=3`, 6 × i4i.2xlarge ≈ $4.1/hr; S2 = `N_CLIENT=8`, 11 nodes
   ≈ $7.5/hr; the cost table is in the PR 15 summary), and only after the
   squeeze-test re-run on the same binary reads clean.** **PR 15 Phase B,
   run 1 (2026-09-24, §3.10): the instrument is BUILT and was EXERCISED
   on 8 REAL nodes** — the owner approved the S2 + 8 oss shape
   (`N_MDS=1 N_OSS=8 N_CLIENT=8`, 17 × i4i.2xlarge), the cluster
   launched, deployed, assembled on the second attempt (the first met the
   baked AMI's cloned `/etc/machine-id` — the daemon's node token; four
   rig fixes landed on the redo branch: the machine-id step, `format
   --force`, the apt hygiene, the every-node cost estimate) with
   `appenders_known 8` / `membership_writers 7` / 8 of 8 device
   registrants — **and FAILED on its first row** (gate 2's `sym-1` arm:
   the joined writer's `mkdir` under the root `EINVAL`) on three product
   findings: **F-C1** (cross-host page-cache incoherence on the shared
   metadata LUN — DESIGN-LEVEL, §4.4ar, item 20 below, a flip blocker),
   **F-C2** (the joiner discards the `Joined` reply's grant word,
   §4.4as), **F-C3** (the conveyor's fan-out flattens every retryable
   class to `Corrupt` → `EINVAL`, §4.4at) — all three routed to **PR
   13i** `fix/sym-shared-lun-coherence`. Torn down at 26.5 min ≈ $5.2,
   nothing billing. **The per-NODE law stays UNMEASURED: the row is
   INCOMPLETE — no per-node number exists, and the pulled evidence
   (`.benchmarks/cloud/2026-09-24-152527/`) was LOST with the dev machine
   the same evening.** **The row is owed to the RE-RUN — after PR 13i
   lands, on the flip candidate's binary, with a NEW expressed owner
   approval for that specific run (the owner rule of 2026-09-24 21:20:
   nothing runs on AWS until PR 13i has landed, and any later launch
   needs the owner's permission again for that run).**
1. **`is_stripe`'s reverse dentry scan over projections** (PR 7b on a
   joiner): `find_parent_of_child` walks every slot tree of the flip
   candidate's volume — a projection on a joiner, defect 24's class once
   per flip candidate, never per op. **Reached by fix round 1's storm
   (§3.8b, batch 1 round 8 — the explicit flip of a rejoined joiner's own
   fresh directory refused `EINVAL` on a recycled projected root) and
   closed for every directory THIS mount made** (`7c62428b`: the
   directory-parent memo, fed at every directory mint, answers the check
   first; pinned). What stays owed is the COLD case — a memo miss, a
   directory another incarnation made — whose fix shape is a divert-aware
   reverse scan or the `known_stripes` set fed at every map read on every
   mount; the candidate's own `stripe_map` read already learns it.
2. **`SupplyStripeIno`'s on-demand binding** (§4.6): the supply declines a
   creator with no endpoint bound instead of `bind_holder_endpoint_on_
   demand`; the holder mints the remainder, so a flip still lands — with
   the stripes in the holder's rotor instead of the creators'.
3. **`appender_flush_ceiling_overruns`** (§4.5, §4.4aa): the landing
   ceiling's fixed 2-tick margin against the manager's pass wall under
   its grant / ship / free SERVICE (the SMO mutex) — the recovery's hold
   is now an extension (defect 33), the service's is not: +1 at 5–127 ms
   past the ceiling on `sym-walls` row (a) in attempts 5 / 9 / 10 / 12 /
   13 and on the storm's start in the attempt-15 rerun; derive the
   margin from the measured pass wall (PR 14). The laptop's readings of
   the gauge are venue-attributed (`51bf21e1`) and, since fix round 1,
   the harness's venue word reports them per round instead of failing
   the leg (Issue 2) — the margin derivation is a PR 14 item, never a
   gate-4 dependency. **PR 13c (F-B1):** the audit now EXCLUDES a service
   hold's overlap with the leaf's dirty window, capped at one landing
   ceiling and published (`appender_flush_ceiling_service_cap_ms`,
   `…_excused_{ns,max_ms}`) — an exclusion of ANOTHER actor's hold, which
   closes the box's "no recovery in flight" class as far as such holds
   explain it; **the margin's derivation from the measured pass wall
   stays THIS item, PR 14's**, and the box re-run on PR 13c's binary
   says what remains for it to price. **PR 13e (F-B1, §4.4an — the
   derivation LANDED):** a forest volume's cadence fires
   `checkpoint_trigger_ms(max_age, term)` = `max_age − term` from the LAST
   COLLECTION (the input is the MAX AGE the tick fires at, never the
   landing ceiling `max_age + 2 × tick`), the term = the horizon MAXIMUM
   (64 CYCLES) of the cycle's measured pre-barrier wall + the decision's
   lateness beyond one tick (the tick's wait for the SMO mutex left out;
   the lateness rides the cycle it RUNS, capped at one ceiling — review
   round 1, Issue 1); the published ceiling never widens, the
   bit-17-absent cadence is decision-identical in the age law's verdict
   (the tick's ORDER — decision before drain, one pass-wide rotated drain
   budget — changed on every layout: §4.4an); RED-first on the box's
   shape, published `meta_kv_checkpoint_{term,trigger}_ms`. **What
   stays for the box re-run (PR 14):** the counted rows on this binary —
   the derivation is judged on whether the box's 16–106 ms terms land
   inside the ceiling there (the laptop reads the mechanism only). **The third box pass (§3.9.5, `b377cbb8`): the derivation did NOT
   price the box's term at the FIRST storm cycle after a quiet horizon —
   two trips on the MANAGER inside `sym-scale` (1,127 / 1,125 ms, 0
   excused) with volume 1's `meta_kv_checkpoint_term_ms` at **11 / 4 ms**
   (trigger 989 / 996) when each joiner storm began and the trip cycle's
   own ≈ 130 ms term in the horizon only AFTER (133 / 127); the 199
   quiet cycles between the rows (≈ 1.5/s, ≈ 135 s) had emptied the
   64-cycle horizon of N = 4's term before N = 8's first storm cycle,
   and the storm's STEADY STATE (62–83 grants/s in the seconds after
   each trip) did NOT trip — the derivation prices the sustained shape;
   both trips inside a joiner create storm's `ExtentGrant` burst (50–80
   verbs/s, 3.6 ms of ring-0 control-entry service each); zero trips on
   the touch and walls fleets (70 writer-legs; the walls rewrite's
   50–120 ms terms anticipated to 0 trips). The item's next pieces:
   (a) the horizon's MEMORY — a term forgotten after 64 quiet cycles
   re-trips at the next storm's first cycle, so the anticipated term
   needs a time bound or a floor beside its cycle count (the walls
   rewrite shows the derivation working when the horizon HOLDS the
   term); (b) the manager's anticipated term folding the verb service
   in flight (or the service moved off the cycle's barrier); AND item
   16 (F-R5) — the supply grain that makes the service a storm at all.
   PR 13g is building on this reading.**
   **PR 13g (F-B1, §4.4an's PR 13g paragraph):** the third campaign's
   two trips re-read as the FIRST storm cycle after a QUIET horizon (the
   terms 11 / 4 ms at the trips — the 64-cycle window had forgotten the
   previous row's 133 ms across 199 quiet cycles), and the cadence gained
   the second term that prices it — a LIVE projection of the next cycle's
   flush wall off its PENDING work (dirty nodes × the per-node unit +
   promised images × the per-image unit, the units horizon maxima per
   class, attributed per volume) anticipated beside the horizon term;
   RED-first on the box's row sequence in process, GREEN with it; the
   ceiling unchanged. What stays for the box: the flip binary's bracket
   (every laptop timing here is the mechanism's).
   **The fourth pass (§3.9.6.1, PR 13g's `230e95dd`, 2026-09-24 — two
   `sym-scale` row sets from zero): the third pass's CLASS is GONE — set 1
   read 0 on every writer through N = 1/2/4/8 (the first gate-3 row set
   to reach its oracle on the box; the onset class exercised at N = 2 / 4
   with volume 1's term 6–7 ms and not tripped), the joiners' storms 0
   trips in 44 joiner-rows — and set 2 read ONE trip on the manager's
   volume 1: 1,101 ms, 1 ms past, at the N = 4 storm's END with ONE verb
   served on that volume across the row (no grant burst — F-R5's fix).
   The faces, placed against the trip (review round 1, Issue 1): the
   create-end snapshot preceded it by < 1 s (overruns 0 there) with the
   projection ENGAGED at 84 ms (term 52, lateness 15); the trip cycle's
   own term 151 and lateness 35 entered the horizon afterwards; no
   barrier face on the manager reads above 2 ms. The decomposition the
   faces support — decision ≈ 916 + lateness ≤ 35 + pre-barrier ≈ 151 +
   barrier ≈ 0–2 ≈ 1,101 — says **the LIVE projection under-priced the
   storm's END cycle by ≈ 65 ms** (the projection is read at the tick's
   decision; the flush pass writes what has accumulated by the time it
   runs — the storm's last second of dirt — and/or the node unit
   under-measures at the tail) with the lateness at 35 ms of the 100 ms
   margin; the cycle's pre-barrier wall is bounded in [52, 151], not read.
   THE NEXT PIECE (PR 14): (i) the projection's growth between the
   decision and the flush — a projection off the admission rate over the
   decision-to-pass interval, or the dirty count re-read at the pass;
   (ii) the lateness term; (iii) the instrument that discriminates —
   a per-cycle tape of the decision instant, the pre-barrier wall and the
   barrier wall (nothing publishes it today) — never a widened constant.
   The tripwire's rate on this binary at this venue: 1 in 8 rows, 1 ms
   past.**

   **PR 13h (§4.4an's PR 13h paragraph — the trip's class read off its
   own snapshots):** the fourth box pass's one trip (1,101 ms, no
   service, a storm's END) was a WAVE of 38 promised compactions priced
   at an image unit of 2.04 ms and paid at 3.97 (`pc41` / `pn41`; 38 ×
   3.97 = 151 = the trip cycle's term) — the unit's grain floor read the
   drain's one-or-two-image passes at a half / quarter of their per-image
   cost, and the lockstep-filling rotor leaves handed the cycle the
   wave. The unit is the pass's mean per item now (the floor deleted —
   a bound must bound); RED → GREEN in process on a barrier-priced
   image. The deferred-flush barrier between the decision and the cycle
   is ALSO priced (the term clocked from the decision, the covering
   barriers at the measured barrier unit) — a real gap, ≈ 2 ms on the
   box. **The instrument:** every cycle's TAPE (decision words + paid
   walls) rides `.stats meta_kv_checkpoint_last_cycle` and the overrun
   WARN line — the next trip attributes itself. **The item's remaining
   pieces:** what the tape says at the next trip (the projection's
   inputs the wave did not reach: the count at the decision vs the
   images the pass wrote, the dirt added after the decision, the per-
   image cost's growth with the tree); the promised-leaf double count in
   the node term (conservative, stated); the deferred barrier's
   redundancy with barrier #1 on a due tick (a deferred-mode timing
   change on every layout, a shipped-behaviour item); and the box
   bracket on the flip binary, which judges all of it.
3b. **`sym-storm` ×10 from zero on the final binary** (§3.8): re-run in
   fix round 1 under `--venue=laptop` — §3.8b carries the count and the
   per-round venue-attributed readings; whatever count stands there is
   the rung's, owed no further than §3.8b says.
4. **The manager's zero-census open** (PR 14 by design — the RAM refcount
   map's mount-time by-block scan replaced by PR 8's bitmap as the
   terminal-free engine) and **PR 7's un-share of a surviving sole
   owner**: unchanged from PR 12b's owed list.
5. **A second SHARD** (a home ≠ volume 0 — the owner-role fusion, the
   `shared_ref:` migration, the cross-shard rejoin retirement, the
   departure sink's cross-shard face; PR 12b's owed list) — the fleet
   rig stands one shard; the N = 32-member reader broadcast (gate 5's
   1 × 31 row) and the 32-mount join storm (gate 7's wall (b) at N = 32)
   are BOX rows (§8).
6. **The joiner dead-manager checkpoint contract's 1-in-N flake** (§4.6)
   — a harness item for the matrix's next widening. (`kv_freeze_wedge_
   tests`' flat-shaped census probe was closed in fix round 1, Issue 17:
   the probe is layout-blind and the suite rides the matrix.)
11. **Gate 8b's `FAIL = 0` line** (§3.7, Issue 19) — CLOSED in fix round
   1: `sudo tests/run_nvmeof_fidelity.sh full` from zero on a quiet box
   (no fleet up) read PASS = 190 / FAIL = 0 (`fix1-post3`); the rung's
   own FAIL 1 was the concurrent fleet's zram teardown between the tier's
   snapshots.
12. **Defect 32's arm — PR 13b** (§4.4z; flip-blocking item (i) above):
   the interim loud refusal and the `#[ignore]`d RED contract landed in
   fix round 1 (Issue 12); fix round 2 (Issue 22) moved the interim gate
   to the `open(2)` for write — where a writeback-cached shell can see it
   — and gave the contract its three faces (`setattr` + `setxattr`, the
   layout publish, the data write + `fsync` reading back at the manager;
   Issue 27); PR 13b builds the ship and un-ignores the contract.
13. **Fix-round finding 1 — the acked-writes LOSS across a seven-victim
   kill with the cross-owner mover and striped destinations** (§4.4af; a
   FLIP BLOCKER beside defect 32): 25 of 17,376, one writer's moved files
   at a 128-name stride, at neither source nor destination after the
   recovery — unattributed, the recipe in §4.4af; PR 13b's first item.
   The storm ×10 count restarts from zero on its fix.
14. **Fix-round finding 2 — a joiner's read inside the failover window
   answers `EIO`** (§4.4ag): the successor's token plane refuses a member
   whose reclaim has not landed and the read does not wait; PR 13b, beside
   defect 15's follow — the refusal is the retryable class inside the
   window.
15. **Fix-round finding 3 — defect 21's pin hangs intermittently in the
   suite's order** (§4.4ah): **FIXED by the hang hunt (2026-09-20)** —
   the pin's own seam schedule (it named the winner by SPAWN ORDER and
   awaited that handle unbounded before releasing the seam; the
   second-spawned fetcher wins the station ≈ 35 % of the time alone and
   more under load), attributed from the parked stacks + the seam's
   statics, no product change. The pin names the winner by what the seam
   observed and bounds the await; suite 10/10 in the runner's order.

**Routed to PR 13 and NOT run here** (the brief's §3 list off PR 12b's,
PR 7's, PR 4/8/9's and the gate's ledgers — each with its next venue,
so nothing falls off a ledger the way defect 32 did between PR 5 and PR
13):

- (a) **The SECOND SHARD** (a home ≠ volume 0 — the owner-role fusion,
  the `shared_ref:` migration at a home change, the departure sink's
  cross-shard face, PR 10's cross-shard rejoin retirement): NOT run.
  The product cannot stand one today — the joined open writes
  `page.home_volume = 0` (`kv/backend/joined.rs`, the join's page write)
  and `tests/mw_fleet.sh` has no home word for a joiner — so this is a
  PRODUCT gap of PR 12b's (the fusion is the rung-sized piece its ledger
  named) with a harness half (a `--home=V` per joiner). Venue: PR 13b's
  two-home fleet leg or PR 14, fixed red-first naming PR 12b; every
  PR 13 leg ran one shard.
- (b) **The dead-INITIATOR-mid-plan pin on the two-process venue**: the
  in-process half is PR 12b round 1's `TEST_XV_SEAM_INITIATOR` pin; on
  the fleet, `sym-storm --cross-owner` kills every joiner at a RANDOM
  instant of its mover's plans, so the class is exercised
  probabilistically per round and judged by the oracle's "every name at
  exactly one of source / destination, every returned `mv` at the
  destination" — the current kill window COVERS it by chance, never by
  construction. A deterministic two-process pin (an initiator parked at
  the seam, then killed) → PR 13b.
- (c) **7b's remote `IsEmpty { scope: 0 }`** under a striped `rmdir` from
  a NON-holder process: NOT run (no leg runs a striped rmdir from a
  non-holder) → PR 13b, beside the record-level ship it shares the
  travelling-guard context with.
- (d) **PR 9's per-object epoch capture + the owner's grace at the arm**
  priced against a real fleet's custody churn: NOT priced (the legs'
  custody churn is the movers' and the storm's; no per-object capture
  row exists) → PR 13b/14.
- (e) **The served `MarkShared` on a mid-cutover slot**: NOT run (no leg
  clones across a slot handover) → PR 14.
- (f) **A joiner's durable `client:` heartbeat** (a manager-less set with a
  parked joiner is the offline `appender clear` window): NOT run → PR
  13b.
- (g) **The census shard's re-enrol after a failover**: stated with its
  number in §4.4i (up to `CLIENT_STALE_TTL` 45 s + the retry grain — the
  ONE routed item this record addresses).
- **PR 7's C8-drift sweep** (the 46 layout-publishing suites under
  `SQUEEZEFS_TEST_STAMP_BLOCK_REFS=1` on the flip candidate): NOT run
  here → PR 14's flip candidate.
- **The `sym_slot_transfer_tests` / `sym_block_grant_tests` load-shaped
  signatures**: did NOT fire in this rung — the matrix ran both suites
  green flat and stamped at `pr13-post15` and again in fix round 1
  (§10), and no fleet attempt was instrumented to attribute them; their
  venue is PR 14's matrix-under-load run (attribute per the PR-1 law
  where they fire).
- **`reader_free_grace_tests` contract 28** (`the_tightened_bound_is_
  published_and_never_crosses_the_ack_cycle`): green under the gate's
  `--test-threads=1` rail on every run of this rung; order-dependent
  under a PARALLEL run because its renewal-wake precondition is a
  process-global word a sibling test's session can satisfy first (PR 12b
  review round 3's reading — pre-existing). Not fixed here → PR 14 (fix
  the pin's precondition to its own session's wake).
8. **`A_max` is inert on a MANAGER** (§3.6 reading 4): PR 3 keeps the
   manager's own images untracked in the slot-extent ledger `A_max`
   reads, so the soft cap sits at `node_size` and every mint past 256
   KiB per tree spills to the rotor with the most headroom — the
   designed outcome (uniform trees) by a different rule, a misleading
   `affinity_a_max_bytes`, an O(M) headroom pick per spill. Feed the
   manager's images into the ledger or derive `A_max` off `slot_tree_
   bytes` Σ — one accessor.
10. **The PAUSED-job premise on a slot whose children spilled** (§4.4ac
   and the `sym-foreign-touch` PAUSED phase, attempts 12–14): the
   design's rule counts `ops_h` PER SLOT, and at the `A_max` floor a
   one-extent tree already spills its children to the rotor
   (`mint_choice`'s strict `<` against `max(used/64, node_size)`), so a
   job live under `job-wC/paused/…` leaves the touched `job-wC` slot
   idle and the IDLE arm moves it at `N_floor` (2–3 on this box) touches
   — the rule working on a slot nobody wrote. Two levers to price: `≤`
   at the floor (a tree holds one extent's children before spilling —
   what "never less than one extent" reads as) and the window's two
   half-`T_idle` buckets (a guarantee of `T_idle / 2`, not `T_idle`).
   The harness gates on the DOMINATED arm alone now and reports the
   idle one. **Void as an OBSERVATION (§4.4aj, 2026-09-22): the PAUSED
   phase's job never ran on attempts 12–14 or on the box — the slot read
   idle because it WAS idle, not because the job's children spilled; the
   spill premise itself stands as design text (PR 13c's subtree law
   closes it), but no leg has yet observed a live paused job, and the
   fixed harness's first local run read 0 handovers / 0 idle offers / 0
   dominated offers. The box row is owed.**
9. **The per-slot extent floor at small populations** (§3.6 reading 1,
   risk R9's number): 16 MiB per volume (64 rotor trees × one 256 KiB
   node) — 736 MiB for 20,000 files over 46 volumes against the flat's
   47 MB. Amortizes with population; `--meta-node-kib 64` quarters it;
   a rotor that mints only past a per-volume population threshold is the
   design-level lever if the box's small-set rows price it in.
7. **The reader's per-op cost on a striped root**: the fold of a striped
   `/` is `K` token serves per `stat /` from the plane cache (one grant
   each per holder per token lifetime); a `stat`-heavy reader of a
   64-stripe root pays 64 cache hits per attr revalidation — measured
   nothing on the legs, stated for the box's `ls -l` row. **Measured by
   PR 15's fleet (§4.4ak, PR 13d): the `K_root` first-touch grants land
   INSIDE the `-ls` leg's `dlm_token_grants` when the same fleet ran
   `sym-scale` first (`2K + C + 3`), and the leg's law does not carry
   the term — the adjudication stands in §4.4ak.**

Records the box owes (§8): gate 1's solo re-gate A-B-B-A on the flip
binary; gates 2 / 3 / 3b / 3c / 5 / 7's counted brackets — every local
number in §3 is a dev-box RATE reading, venue-attributed pending the box
(the mechanism rows are GREEN; the rates are the box's).
11. **PR 13c review nit (routed to the board): the dial-site census as a
   TOKEN census** — `derivation_sweep_tests` counts the `S8-LISTENER
   CONTROL SESSION` markers against `MEMBER_CONTROL_SESSIONS`; the
   census should count the dial SITES themselves (the `RpcClient::connect`
   / `ManagerClient::connect` call sites reaching the S8 listener) so a
   new standing session cannot land un-marked. PR 14.
12. **PR 13c review nit (routed to the board): tie `handshake_timeout ≡
   DIAL_TIMEOUT`** in `derivation_sweep_tests` — the refused-dial retry's
   clip and the listener's pre-authentication reaper are stated as one
   bound; the tie test is what keeps them one. PR 14.
13. **The box re-run's findings (§3.9.4, PR 13c's binary — each REPORTED,
   none fixed there):** **F-B1 stands** — six increments on five writers
   in 45 min across three fleets (one of them — m60, a quiet joiner —
   established as NOT under a storm; the three "between the rows" trips,
   m0 ×2 and m61, fall in each writer's `rm -rf` of its 20k-file tree
   before the next row's joins), the two with a WARN line 16 /
   106 ms past the 1,100 ms ceiling (the scale fleet's four have no age
   reading — H-R2), `excused_ns` 0 everywhere: item 3's margin derivation
   is the whole answer (PR 14 / 13e, a flip precondition — every N-writer
   row set on the box stops at its first trip), and its FIRST piece is
   the missing instrument — a per-cycle pass-wall / checkpoint-cycle
   histogram on every writer (no `checkpoint_phase_ns`-class key exists;
   the audit records the age at the covering barrier alone), without
   which the attribution stays by elimination. **F-R2** (the SHIPPED S8/S10 path): `issue_update_grant` runs an
   8,193-entry `readdir_local` on the parent of EVERY shipped create
   before its over-budget decline — 1.66 ms of `sqz-meta` CPU per create,
   the authority bound at ≈ 1,200 verbs/s / ≈ 350–390 creates/s
   fleet-wide (a cached per-directory census word is the fix; the shipped
   co-writer posture the flip retires). **F-R3** (the armed plane, PR 6 ×
   PR 12b): the cross-owner unlink's child witness `read_inode_value_
   routed` reads this daemon's PROJECTION of a foreign lessee's slot,
   finds `None`, drops the `SetNlink` step and removes the name —
   **one orphaned inode per cross-owner unlink of a foreign-minted child**
   (430 / 512 in one leg), invisible to fsck while the lessee lives (the
   inode plane scopes out live foreign lessees' slots); the witness must
   read at the child's holder (the writer's divert / a `LookupExact`).
   A flip precondition (an ordinary `rm -rf` leaks). **F-R4** (PR 6 × the
   handover): a create into a directory whose slot moves TO the creator
   mid-burst answered `ENOENT` once — the working HYPOTHESIS (a timing
   correlation, no log names the site) is the old holder's live-witness
   refusal after its release surfaced instead of re-dispatched (defects
   29 / 30's family); the pin shape that settles it is in §3.9.4.3. **Gate 1's rename / unlink DELTA** (−3.3…
   −4.4 %): the PR-4 rename lock-set fix's priced cost STAYS; the
   `handle_setattr` future's construction + lane move (the kernel's
   SETATTR echo per rename / unlink) is the named per-op term — **FIXED
   in PR 13f** (setattr 26,016 → 896 B, unlink 11,088 → 280 B, the root
   `commit_tx`'s inline door arms 4,816 → 408 B; §3.9.4.1); the bracket
   re-reads on the flip binary. The same term in `write` / `fallocate` /
   `copy_file_range` / `fsync` / `read` / `flush` is PR 14's (the `rw4k`
   row's shape).

16. **The third box pass's findings (§3.9.5, PR 13e / 13f's binary
   `b377cbb8` — REPORTED, none fixed there):** **F-B1 stands** (item 3 —
   two manager trips under the joiners' grant storm, the derivation
   engaged); **F-R5 (new — PR 3 × PR 12b, the armed plane; PR 13g is the fix
   rung): the manager derives a WIRE joiner's extent grant from an EWMA
   it never receives — `grant_extents_for` (`kv/backend.rs:9148`) reads
   `set.region(id).smo_ewma_milli`, `None` for a wire joiner → `ewma = 0`
   → `grant_extents_derived` = the FLOOR 8 whatever the joiner's SMO rate
   (the joiner folds its EWMA locally, nothing carries it on
   `ExtentGrant { appender_id, want }`; at ≈ 8 SMO/s the §5.3.3 derivation
   would answer ≈ 720 extents) — so every production joiner refills in
   ≤ 8-extent grants: the leg's 1,483 = 845 × 4 / 180 × 3 / 147 × 2 /
   2 × 1 (the reactive `needed.max(SMO_IMAGES_MAX)` ask, trimmed by the
   `GRANT_RUNS_MAX` coalescing loop) + 103 × 5 / 54 × 6 / 44 × 7 / 108 × 8
   (the cadence's proactive `refill_due()` ask — the 50 % refill
   engages, 309 times — answering the derived 8), all served in the
   USER class; the second mechanism a joiner's ring at the 512 KiB
   floor (`appender_ring_bytes` 524,288, `appender_ring_grows` 0,
   `joined_ring_grow_declined` [0, 1]) checkpointing ≈ 8×/s under a
   40k-file storm, returning the images each barrier RETIRED
   (`take_returnable()`, +193) and re-claiming at the SMO grain (+47) —
   ≈ 105 wire verbs per joiner-volume per storm, the manager serving
   50–80 ring-0 control entries + barriers per second at N = 8; remedy
   shapes: the EWMA on the ask or the page (or the joiner's own derived
   ask, screened) — the lead lever; PR 2's owed drain-then-grow (or the
   EWMA-sized join) so a storming joiner's ring leaves the floor; the
   manager's term folding the in-flight service (item 3). **Gate 1's two sub-second rows**
   (§3.9.5.1): B's FIRST mount of a fresh set +0.10–0.12 s on a 0.4 s
   event (1.251 within a 31.5 % band / 1.326 DELTA by 0.03 over 29.7 % —
   UNCONVICTED by the A-B-B-A law, not within noise; directionally
   consistent across the three box brackets on two binaries — 1.132 on
   `77f4da1d`, 1.251, 1.326, B slower by 0.06–0.12 s of median each time;
   a ms-grained mount tape is the instrument) and
   **the post-`rw4k` clean unmount +0.5–0.9 s — a NEW, UNATTRIBUTED
   DELTA on the flip candidate's flat path** (B 8.1–8.5 vs A 6.4–7.7 s
   outside the position-1 outliers, DELTA in both brackets; the re-run's
   `77f4da1d` read the OTHER way — A4 8.693 / B2 8.060 / B3 7.615, B
   faster; the whole unmount is the `Force flushing all in-memory write
   buffers` step, 8 → 9 s at second precision, and NOTHING instruments
   its phases — the owed instrument is a per-step tape on the shutdown
   ladder (the flush of the write LRU / active blocks, the overlay
   drain, the rewrite epochs' close, the reclaim drain); the candidates
   are 13e's cadence bookkeeping at the shutdown fixpoint's final cycles
   and 13f's boxed handler futures, neither convicted; the flip's box
   pass re-reads the row with that tape). **`create` −1.9 /
   −2.8 % at the floor** (both brackets, every B below every A; the fp
   legs of the re-run named `lock_stripe` +0.5 µs/op on create) — within
   noise by the rule, stated. **The gate-3c handover's flush phase**
   13.75 / 14.06 ms (the re-run 3.99 / 2.04) — the flush-then-transfer's
   cycle count per handover is not instrumented. **Harness**: the
   `sym-scale` leg needs a per-writer LAUNCH stamp (the skew at N = 8 is
   INFERRED — wall 18.37 − the longest storm 14.44 = 3.93 s — and its
   cause, the root's STRIPING at the row's eight `mkdir`s, hypothesised)
   and the wall law's clock at the LAST storm's launch (3.53× read vs a
   storms'-own-concurrency upper bound of ≤ 4.5×); H-13E-1 (the fleet rig's daemon-pid anchor vs
   arm-suffixed binaries) fixed on the branch. **The fourth pass
   (§3.9.6) MEASURED the skew — 9.135 s at N = 8, three 3.03 s `mkdir`s
   by the fresh joiners into the freshly striped root — and moved the
   setup out of the clock (`6108e8c1`): 4.61× on the storms' own clock
   (Σ per-writer rates 5.25×, the bound); F-R5 FIXED on the box
   (§3.9.6.2) — item 16's F-R5 half is CLOSED.**

17. **The fourth box pass's findings (§3.9.6, PR 13g's binary
   `230e95dd`, 2026-09-24 — REPORTED, none fixed there):** **F-B1's
   residue** (item 3 — the LIVE projection under-pricing the storm's END
   cycle by ≈ 65 ms + the 35 ms lateness; 1 trip in 8 rows, 1 ms past);
   **F-R6 (new — PR 12b's
   reclaim path × PR 5's token planes): a joined writer's FORGET-driven
   reclaim prices DESTROYS for inos in slots it does not lease, reading
   its stale PROJECTION of the holder's trees** — 6,782 / 7,266 `destroy
   WITHHELD` WARNs on m60 per set (`reclaim_destroy_refused_release_
   failed` 121 / 742), all for the MANAGER's rotor inos in the windows
   where the manager removes its own tree (the leg's acked-writes check
   had read that tree through m60; the manager's `rm -rf` recalls m60's
   tokens, the recall sink prunes, the kernel FORGETs, the reclaim prices
   a destroy off the projection): a zeroed ring segment (`0x2180000`,
   carved for appender 3 at 04:14:17Z, read at 04:14:52Z), 27 / 45
   consecutive extents re-granted to m63 and screened by rule 4, and
   defect 18's 256-restart `root-seq` loop (defect 34's family) on 60+
   slot trees (6,205 / 6,088 lines — PR 13b's "9/10", tree 0's child-seq
   shape, here root-seq on slot trees); nothing
   destroyed — the belt is the commit DOOR's foreign-slot refusal, not the
   withhold: `slot_door_refusals` [0, 0] on m60 at every snapshot incl.
   set 2's `m60_pend.json` (the refuse arm itself commits no tx; the
   holder's own reclaim is the law) — the oracle blind (live lessees
   scoped out; the gauges outside the must-stay-0 set and the per-row
   snapshots) — fix sites: `queue_reclaim_inode` / `reclaim_orphaned_
   batch` (`src/fuse_client.rs:10544` / `:25489` — a FORGET of an ino
   whose slot this mount does not lease enqueues no reclaim; the
   predicate `slot_is_foreign`) and `destroy_entry_bytes` (`kv/backend.
   rs:24087` — a foreign slot's tree read without the divert), defect
   34's loop its own item; **the served-
   mutation kernel hook's `ENOENT` at WARN** — 272 k / 275 k `may reply
   interrupted fuse request` lines per set (`crates/fuse3/src/raw/
   session.rs:1092`, the detached notify frames on the reply channel; 46 %
   of the manager's and 21 % of m60's invals + prunes — the inodes the
   kernel had already dropped) — a counted outcome, never a WARN; **a fresh joiner's first create into a STRIPED root costs
   ≈ 3.0 s** (×3 in both sets; the per-holder token planes' 1 s first-round
   waits the hypothesis; an `OP_PROFILE` tape the instrument; a latency
   cliff on the flip's default path, not a throughput term); **the ingest
   law's N = 1 base is a sub-second `dd`** (1,327 / 1,506 / 2,215 MiB/s
   across three passes while N = 8 holds 7.3–7.6 GB/s — `--ingest-mb` ≥
   4,096 on the box; a harness item); two `.stats` faces (a joiner's
   `granted`; the writer's per-holder read planes) for PR 14's sweep;
   **the heap-fragmentation watch item** (§3.9.6.2 law 1 — 1 → 7 short-run
   `GrowRing` declines across the two legs, m64's ring pinned at 1 MiB
   at set 2's end; the remedy is the heap's carve / return order, not
   the cadence's).
   Harness landed on the branch and placed on the box: `76028d5f`,
   `6108e8c1`, `cea35691` (§3.9.6.3).

Records the box owes (§8): after the re-run, NONE of gates 1 / 3 / 3c /
5 / 7's rows is owed on PR 13c's binary — §3.9.4 carries them; the
remaining box rows are PR 14's (the flip binary's A-B-B-A of every gate,
gates 2 / 3b included) after items 3 / 13 land.
After the THIRD pass (§3.9.5, PR 13e / 13f's binary): gates 1 / 3 / 3c /
7@N=32 re-read on `b377cbb8` — none owed on it; the remaining box rows
are PR 14's flip binary's (every gate, 2 / 3b / 5 included), after item 3
(+ item 16's F-R5) lands.
After the FOURTH pass (§3.9.6, PR 13g's binary): gate 3 re-read TWICE on
`230e95dd` — nothing owed on it; F-R5's half of item 16 CLOSED on the box;
item 3's remaining piece (the projection's growth between the decision and
the flush + the lateness) and item 17's F-R6 are
the box's input to PR 14; the remaining box rows are the flip binary's.

17. **The fourth box pass's findings (`perf/sym-box-13g`'s record
   §3.9.6, PR 13g's binary `230e95dd`, 2026-09-24) — as PR 13h leaves
   them:** **F-B1's trip class FIXED red-first** (item 3's PR 13h
   paragraph, §4.4an — the unit's grain floor deleted; the deferred
   barrier priced; the per-cycle tape landed); **F-R6 FIXED red-first**
   (§4.4ap — a joined
   writer's FORGET-driven reclaim of an ino in a slot it does not lease
   is a token client's forget: dropped at the reclaim's entry before any
   read, counted `reclaim_foreign_slot_forgets`; the projection walk's
   exhaustion counted `meta_kv_projection_walk_exhaustions`); **the
   served-mutation hook's `ENOENT`-at-WARN FIXED** (§4.4aq —
   `fuse3_notify_enoent`; the WARNs were a 46 % / 21 % subset of the
   notify frames, 275,372 in set 2); `extent_grant_granted` exported.
   **Still
   standing from that pass:** the fresh joiner's ≈ 3.0 s first `mkdir`
   into a freshly STRIPED root (the lazily dialed per-holder token
   planes' first rounds — `TokenReaderPlane::await_channel_fresh` waits
   the standing recall poll's first round, which the holder parks for
   its whole 1,000 ms `DELEG_PARK_DEFAULT_MS` window when idle — the
   standing hypothesis; `SQUEEZEFS_OP_PROFILE=1` on a fresh joiner's
   first create is the instrument; PR 14 / 15's latency cliff on the
   flip's default path); gate 3's wall law at N = 8 (the co-located
   venue's term; the per-NODE law is PR 15's); the ingest law's
   sub-second N = 1 base (`--ingest-mb` ≥ 4,096 on the box — a harness
   item); defect 34's projection loop (item 13 / 18 — now counted, its
   reclaim-path caller gone); the writer's per-holder read planes on
   the `dlm_token_reader_*` fold (PR 14's stats sweep).

18. **The offer's stand cannot outlast the wire recall's delivery bound
   (PR 4 × PR 12b — PR 13h review round 3, Issue 16; filed, not fixed):**
   PR 4's offer stands `min(10 s, T_idle / 3)`, derived for the in-process
   model where the requester's acceptance ran the handover directly; under
   PR 12b the HOLDER learns the accepted offer's RECALL only on its renewal
   beat (`slot_recall_notices`, 10 s) and releases after its
   flush-then-transfer, so on the wire the offer's lifetime can NEVER
   outlast the recall's delivery bound — the requester's acquire-on-offer
   never runs, the slot goes `Unleased`, and it lands at the requester
   only if the requester ships AGAIN (the door's first touch). The
   `sym-reclaim-hint` leg's first run read exactly this: the offer LAPSED
   (`slot_offers_expired` +1), m60 released, m61 sat idle, the slot stood
   `Unleased`, and m60's close took the UNLEASED arm to the manager (the
   corpse-reclaimer law held — the corpse was destroyed — but it was not
   the design's handover to the requester; the leg keeps its requester
   live until its wire acquire lands, a legitimate premise for the corpse
   law that masks this term). It also breaks the design's own closure law
   `slot_offers ≡ slot_handovers + slot_offers_expired` (§11 "Closure
   laws"): one offer counts as `expired` AND — the manager counting the
   wire `ReleaseSlot` that spent its recall as a handover — as a
   `handover`. **The fix (PR 14 / 15):** derive the offer's stand from
   the wire recall's delivery bound (≥ the renewal beat + the handover
   wall), or prod the holder's renewal at the acceptance (the free-grace
   prod's precedent), and restate or pin the closure law on the wire
   shape.
19. **A reclaim hint whose resolved lessee is DEAD has no reclaimer until
   the manager remounts (PR 13h review round 4, Issue 20; filed, not
   fixed — PR 14):** from a lessee's death until PR 10's recovery releases
   its slots (`T_owner` + the recovery bound) BOTH words — the joiner's
   projection and the manager's table — name the dead appender, so the
   forgetter's `step_home_bound` binds the dead endpoint, the dial fails
   and `ship_reclaim_hint` counts `reclaim_hint_failures` per ino; after
   the recovery the slots are UNLEASED at the manager, whose corpse sweep
   runs only at ITS mount (the recovery runs no sweep over the trees it
   releases) — the corpse and its blocks leak until the manager remounts.
   Not a regression of PR 13h's law (the manager-first build shared the
   death window) but a class the family's `failures` face names without a
   reclaimer. The arm: a corpse sweep of the recovered slots inside PR 10's
   recovery step (the mount-time sweep's body over
   `release_recovered_slots`' trees), plus one re-resolve off the manager's
   word on a failed ship (the forward hop's shape) to close the
   ≤ one-refresh window after recovery. Beside design §5.1.3's class (iii).
20. **The shared-LUN rule — F-C1 (§3.10, §4.4ar; DESIGN-LEVEL; a FLIP
   BLOCKER; PR 13i `fix/sym-shared-lun-coherence`):** every metadata read
   and write goes through `uring_fs` BUFFERED I/O — the uring-path opens
   carry `O_CLOEXEC` only (`src/uring_fs.rs:1368/1586/1609`), the
   `blocking_fallback_loop`'s opens (`:2222/2247/2258/2274`) no custom
   flags at all — while the data path opens `O_DIRECT` at its four sites
   (`src/nvme_dev.rs:744`, 1576, 1686, 1717) — i.e. metadata rides each
   HOST's block-device page cache on both arms. The symmetric plane's premise is N
   hosts sharing one metadata LUN, and on two hosts a joiner reads ITS
   kernel's stale image of a block the manager wrote (the appender page's
   two images, `backend.rs:10510–10514` then `write_wire_joiner_page_grant`
   ≈ `:11096`; the tree-0 / ring-0 projections; foreign slot-tree nodes;
   the directory; the ledger). Every co-located venue — the laptop,
   squeeze-test, the 2026-09-12 cloud `mw` row, ONE kernel each — shared
   one cache and was structurally blind; the first two-kernel venue found
   it on the first user mutation. **The rule (GPFS / Lustre's):**
   `O_DIRECT` (or explicit invalidation) on EVERY shared-LUN metadata read
   and write on EVERY host, with sector-aligned staging buffers on both
   paths (the ring's byte-positioned entries, the 4 KiB page / ledger
   writes) — PR 13i's fix: design §5.8 states it as the shared-disk
   premise's coherence law and the `uring_fs` metadata opens take the
   posture the data path already holds. **The fixture** is TWO kernels on one block device — a qemu/KVM guest as a
   second member, guest and host both mounting an nvmet-tcp namespace the
   laptop exports (network namespaces on one laptop share a page cache
   and cannot show it); it is also where the flip binary is proven on two
   hosts before the next paid cloud row. F-C2 (§4.4as — the joiner
   honours the `Joined` reply's grant word) and F-C3 (§4.4at — the
   conveyor's fan-out clones every class by class) ride the same rung,
   F-C3 first (the smallest, and what hid the retryable class behind
   `EINVAL`). PR 13i gates PR 14's flip AND the cloud re-run (a NEW
   expressed owner approval for that run).


## 8. Box footprint

**No file was placed on `squeeze-test` in this rung; no box row ran.**
The decision (the venue law + the counted-run law): §4.4z's defect 32
makes the flip wait for one more product rung, and PR 14's own gate-1
B arm is the DEFAULT-ON binary by definition — every bracket run on this
tree would be re-run on the flip binary, so the box's minimum-count
budget is spent there. The box owes, on THAT binary: gate 1's solo
re-gate A-B-B-A (arm A `3228fcb8`'s rocky8 `release` build at
`/tmp/pr13-armA/dist/…` stays valid as the pre-program reference), the
counted brackets of gates 2 / 3 / 3b / 3c / 5 / 7 (the N = 32 reader
broadcast and join storm included), the fabric reset
(`/scratch/tmp/cluster_reset_v4.sh`) first, the box left unmounted after.
Every local rate in §3 is dev-box scoping evidence for those rows.

**The box-rows rung (`perf/sym-box-rows`, 2026-09-22): again nothing was
placed on `squeeze-test` or the storage nodes, and nothing on them was
changed** — the box was found in another party's role (§3.9: the DDN
Lustre kernel as the grub default since 2026-09-19 21:21 UTC, `/s3ds`
mounted, the `s3ds` container up). Read only: `uname -r`, `grubby
--info=ALL` / `--default-kernel`, `journalctl -b -1/-2`, `last`,
`/root/.bash_history`, `mount`, `systemctl is-active docker lnet`,
`docker ps`, `s3ds status`, `rpm -q lustre kmod-lustre`, `ls /scratch/tmp`,
and over the reset script's root ssh on each storage node `uname -r`,
`uptime`, `ls /sys/kernel/config/nvmet/subsystems`, `ls -la
/scratch/tmp/squeezefs`, plus one `ls` of `aqr37-d0`'s namespace attrs
(no `resv_enable`). PR 1's footprint (`/scratch/tmp/{squeezefs,rigs,
sym-pr1,logs,fio_jobs,cluster_reset_v4.sh}`) is intact. The two arms and
their shims sit on the LAPTOP at `/tmp/grok-justin/box-rows/arms/`
(`squeezefs-A` `9931007…`, `squeezefs-B` `ea41d9e…`, `SHA256SUMS.local`);
the box copy `/scratch/tmp/sym-box/` does not exist yet.

**After the owner restored the venue (01:07 UTC) the rung placed, all
under `/scratch/tmp/` (root-owned unless noted; the artifacts are the
evidence and stay):**

| path | what |
|---|---|
| `squeeze-test:/scratch/tmp/sym-box/{squeezefs-A,squeezefs-B,libsqueezefs_il-A.so,libsqueezefs_il-B.so,SHA256SUMS.local}` | the two arms + shims + checksums (`sha256sum -c` OK on the box; `--version` verified) |
| `squeeze-test:/scratch/tmp/squeezefs` | **REPLACED** by arm B (`088e8c4a` = `7b2ef9e9`'s code, 857,023,904 B) — the reset script's client binary (`SQZ=`); PR 1 had left `a9827378` there |
| `squeeze-test:/scratch/tmp/sym-box/repo/{tests,.benchmarks/rigs}/` | the worktree's `tests/` tree (88 rigs beside it) — the fleet rig + matrix the driver runs; `tests/mw_fleet.sh` carries the H-B1 policy-routing fix (replaced by `mv` mid-pass, the running processes on the old inode) |
| `squeeze-test:/scratch/tmp/sym-box/linux/linux-7.2.3/fs` | the `tar -x` corpus (2,468 entries, 51 MB) — gate 2's instrument; the box has no internet |
| `squeeze-test:/scratch/tmp/rigs/{2026-09-13-sym-pr1-solo-regate.sh,2026-09-13-sym-pr1-solo-regate-reduce.py,2026-09-21-sym-box-brackets.sh}` | PR 1's pair re-placed (diff-identical to the tree's) + the N-writer driver (its final revision) |
| `squeeze-test:/scratch/tmp/sym-box/reset-verify.log` | the one hand-run fabric reset (rc 0, 15 namespaces, format complete) |
| `squeeze-test:/scratch/tmp/sym-box/rows-gate1-20260922-011258/` (+ `.log`, `REDUCED.md`) | **gate 1 bracket 1** (A B B A): per row `.stats0/1`, `.fio.json/.txt`, `_bw.*.log`, `.procstat*`, `.thermal*`, `.dmesg`; per arm `reset-*.log`, `features-*.txt`, `*.mount.*`/`*.remount.*`/`*.umount.*` (timed legs + daemon logs + `.stats`), `*-mdstorm.*`, `prep-*.fio.json` |
| `squeeze-test:/scratch/tmp/sym-box/rows-gate1-20260922-011258-rev/` (+ `.log`, `REDUCED.md`) | **gate 1 bracket 2** (B A A B, the DELTA rows + `rr4k`) |
| `squeeze-test:/scratch/tmp/sym-box/brackets-20260922-021250/` (+ `.log`; `.log.zstd-refused` = the first launch's devsub refusal) | **N-writer pass 1**: `SUMMARY.txt`, `box.log`, `fleet-A.{create,teardown}.log`, `fleet-B.create.log` (the 32-member create that died at member 13), `fleet-B-partial-logs/{m0,m12,m13}.log + m0.stats.json`, per leg `<gate>-r1.log` + `<gate>-r1/` (the leg's `$STATE/rows`: tables, verdicts, `m*_p*.json` snapshots) + `.thermal*`/`.loadavg`/`.dmesg` |
| `squeeze-test:/scratch/tmp/sym-box/brackets-20260922-022526/` (+ `.log`) | **N-writer pass 2** (gates 2 and 3b, two positions each, a fresh fleet per leg): `SUMMARY.txt`, `box.log`, `fleet-A-{1..4}.{create,teardown}.log`, `tarx-r{1,2}`, `shared-dir-r{1,2}` |
| `squeeze-test:/scratch/tmp/sym-box/{GATE1_OUT,GATE1_REV_OUT,BRACKETS_OUT,BRACKETS2_OUT}` | four one-line pointer files |
| `squeeze-test:/scratch/tmp/logs/` | the reset script's `mkdir -p` (empty) |
| `squeeze-test:/dev/shm/sqz_mdstorm/`, `/run/squeezefs-mwfleet`, `/run/squeezefs-devsub-tcp-mwfleet`, the fleet's netns / veth / `pref 40` policy rules, `/mnt/sqz-mwfleet/` mounts | **all removed** — the mdstorm substrate per leg, every fleet torn down to zero residue (asserted by `mw_fleet.sh teardown`), verified at the end: no daemon, no netns, no rule, `/scratch/tmp/test` unmounted |
| the 5 storage nodes | **nothing placed**; the reset rebuilt their null_blk backings and nvmet shares per gate-1 arm (8 resets), leaving the fabric in the converged shape it found; the client's fabric controllers stay connected (the standing shape PR 1 left too) |

Laptop-side: `/tmp/grok-justin/box-rows/{arms,gate1,gate1-rev,nw}` (the
pulled artifacts), `/tmp/grok-justin/box-rows/smoke1` (the driver's
plumbing smoke), the arm-B build log; `/tmp/pr13-armA` (PR 13's arm-A
worktree, reused, left as found).

**The PR 13c box campaign (`fix/sym-box-campaign`, 2026-09-22 05:00–05:12
UTC — the ONE approved `perf record` leg per arm, not listed before this
rung):** `squeeze-test:/scratch/tmp/sym-box/{box-perf-leg.sh,perf-agg.py,
perf-callers.py,perf-raw.py}` (the leg + its three aggregators),
`/scratch/tmp/sym-box/perf-{A,B}/` (`perf.data`, `leg.log`, `tpc-agg*.txt`,
`tpc-raw.txt`, `comms.txt`), `/scratch/tmp/sym-box/perf-{A,B}.run.log`.

**The box RE-RUN rung (`perf/sym-box-rerun`, 2026-09-22 11:02 → 13:52
UTC) placed, all under `/scratch/tmp/` (root-owned; the artifacts are the
evidence and stay):**

| path | what |
|---|---|
| `squeeze-test:/scratch/tmp/sym-box/{squeezefs-B-77f4da1d,libsqueezefs_il-B-77f4da1d.so,SHA256SUMS.rerun}` | arm B = PR 13c's gated tip (`77f4da1d`, `release`, sha256 `42438ccd…0088a1`; the shim `581126de…`), `sha256sum -c` OK + `--version` verified on the box; the previous `squeezefs-B` (`7b2ef9e9`'s code) kept beside it |
| `squeeze-test:/scratch/tmp/squeezefs` | **REPLACED** by arm B `77f4da1d` (the reset script's client binary) |
| `squeeze-test:/scratch/tmp/sym-box/repo/{tests,.benchmarks/rigs}/` | refreshed from this worktree (`mw-scale` leg, the walls row (b) fleet-wide law, the mw-scale intents closure; 445 / 88 files; the copies diff-identical after each refresh) |
| `squeeze-test:/scratch/tmp/rigs/{2026-09-13-sym-pr1-solo-regate.sh,-reduce.py,2026-09-21-sym-box-brackets.sh,2026-09-22-sym-box-perf-phases.sh}` | PR 1's pair (the RT + diskstats revision), the driver (fleet M / `mwscale`, daemon logs kept per leg), the NEW phase-targeted perf leg (its fix-round revision — the aggregator `2026-09-22-sym-box-perf-agg.py` beside it, the flat table and the DWARF caller tables persisted per phase — is on the branch and NOT yet on the box: the box was unreachable at fix-round time; place both files with the next session's arms) |
| `squeeze-test:/scratch/tmp/sym-box-rerun-stage/` | the user-writable staging dir the `rsync`s landed in before the root `install`s (the arms, `repo/`, the rigs) |
| `squeeze-test:/scratch/tmp/sym-box/rows-gate1-rerun-20260922-110641/` (+ `.log`, `REDUCED.md`) | **gate 1 bracket 1** (A B B A, RT = 60): per row `.stats0/1`, `.diskstats0/1`, `.job`, `.fio.json/.txt`, `_bw.*.log`, `.procstat*`, `.thermal*`, `.dmesg`; per arm `reset-*.log`, `features-*.txt`, the timed mount / remount / umount legs, `*-mdstorm.*`, `prep-*.fio.json` |
| `squeeze-test:/scratch/tmp/sym-box/rows-gate1-rerun-20260922-110641-rev/` (+ `.log`, `REDUCED.md`) | **gate 1 bracket 2** (B A A B, mdstorm only — the DELTA rows) |
| `squeeze-test:/scratch/tmp/sym-box/perf-phases-{A,B}/` + `perf-phases.log` | the fp-chain perf legs: `{mkdir,create,rename,unlink}.{data,row,comms.txt,perf-record.log}`, `stats_{pre,post}.json`, `leg.log`; **`<phase>.tpc-flat.txt` is EMPTY on every leg** (the rig's first build filtered `perf report --comm fuse3-tpc`, an EXACT match that no `fuse3-tpcN` comm meets — review Issue 5; the rig now runs the sibling aggregator instead); **`<phase>.tpc-agg.txt`** = the fp leaf tables §3.9.4.1's per-phase numbers come from, produced BY HAND with the previous rung's `/scratch/tmp/sym-box/perf-agg.py <data> fuse3-tpc 400` (its prefix-match logic is what `2026-09-22-sym-box-perf-agg.py flat` now carries) |
| `squeeze-test:/scratch/tmp/sym-box/perf-dwarf-{A,B}/` + `perf-dwarf.log` + `perf-callers2.py` | the DWARF-unwound legs (rename / unlink, 299 Hz; 177–219 MB `perf.data` each) + the caller aggregator (on the box only — its logic is `2026-09-22-sym-box-perf-agg.py callers` on the branch). **The caller tables §3.9.4.1 quotes came from FOUR invocations whose stdout was NOT saved beside the data** — `python3 perf-callers2.py perf-dwarf-{A,B}/{rename,unlink}.data fuse3-tpc memcpy_avx512 4` — so a reader cannot re-check them without re-running `perf script`; **OWED (the box unreachable at fix-round time): re-run the four through the rig's aggregator and save `perf-dwarf-{A,B}/{rename,unlink}.callers-memcpy_avx512.txt` (+ `-memmove`, `-memcmp`) beside the data — the rig does this itself on every DWARF leg from now on** |
| `squeeze-test:/scratch/tmp/sym-box/rerun-nw-20260922-121141-mwscale-r1/` (+ `.log`) | the FIRST `mw-scale` launch (H-R1: died at N = 2 on the intents term) — kept as the finding's evidence |
| `squeeze-test:/scratch/tmp/sym-box/rerun-nw-20260922-122434-mwscale-r1/` (+ `.log`) | **gate 3's A arm** (fleet M, `mwscale-r1/mwscale-1790079886/`: the table, per-writer `create-n*-w*.txt`, `m*_pn*{0,c,1}.json`, `fsck-mw-scale.out`) |
| `squeeze-test:/scratch/tmp/sym-box/rerun-nw-20260922-125749-scale-B/` (+ `.log`) | **gate 3's B arm** (fleet A, `scale-r1/symscale-1790081892/`) — RED on F-B1 at N = 8; daemon logs NOT kept (H-R2 found here) |
| `squeeze-test:/scratch/tmp/sym-box/rerun-nw-20260922-130638-touch-readers-walls32/` (+ `.log`) | **gates 3c ×2, 5 ×2, 7@N=32 launch 1** — per leg `<gate>-r<i>/` (rows + `daemon-logs/m*.log`), `fleet-{A,B,C}-<n>.{create,teardown}.log`, `SUMMARY.txt` |
| `squeeze-test:/scratch/tmp/sym-box/rerun-nw-20260922-134449-walls32/` (+ `.log`) | **gate 7@N=32 launch 2** (the fleet-wide law; RED on F-B1 at row (a)) |
| `squeeze-test:/scratch/tmp/sym-box/{GATE1_RERUN_OUT,GATE1_RERUN_REV_OUT,RERUN_NW{1,2,3,4}_OUT}` | pointer files |
| `/dev/shm/sqz_mdstorm{,_perf}/`, `/run/squeezefs-mwfleet`, `/run/squeezefs-devsub-tcp-mwfleet`, the fleets' netns / veth / `pref 40` rules, `/mnt/sqz-mwfleet/` | **all removed** — every fleet torn down to zero residue (asserted by `mw_fleet.sh teardown` ×8), verified at 13:53 UTC: 0 daemons, no `/run/squeezefs-mwfleet*` / `-devsub-*` (`/run/squeezefs/` keeps the box-rows rung's four IL sockets, as found), no netns, no rule, no veth, `/scratch/tmp/test` unmounted, no `/dev/shm/sqz*`; the devsub's `nvmet_tcp` / `nvmet` / `zram` / `null_blk` modules unloaded (only `fuse` + the fabric's host `nvme_tcp` stack loaded, as found); the fabric's 15 namespaces connected as found; 246 G free; `/scratch/tmp/sym-box` 3.3 G |
| the 5 storage nodes | **nothing placed** (their `squeezefs 1.1.0` copies serve the reset's `nvmeof` verbs); the reset rebuilt their null_blk backings + shares per gate-1 arm (4 resets), leaving the fabric converged |

Laptop-side: `/tmp/grok-justin/box-rerun/{arms,gate1,gate1-rev,perf-phases,nw}`
(the pulled artifacts incl. `SHA256SUMS.local`), the arm-B build log
`/tmp/grok-justin/box-rerun-armB-build.log`; the build worktree
`/tmp/box-armB-77f4da1d` REMOVED after placement.

**The THIRD pass (`perf/sym-box-13e`, 2026-09-23 01:53 → 03:57 UTC)
placed, all under `/scratch/tmp/` (root-owned; the artifacts are the
evidence and stay):**

| path | what |
|---|---|
| `squeeze-test:/scratch/tmp/sym-box/{squeezefs-B-b377cbb8,libsqueezefs_il-B-b377cbb8.so,SHA256SUMS.13e}` | arm B = PR 13e / 13f's tip (`b377cbb8`, `release`, sha256 `b0ec7d65ec7d9d657ca28863ab51e36d01ff8906a466e83139258cc025128435`; the shim `6789dd591c3d…689fc5dd`), built by the orchestrator, `sha256sum -c` OK + `--version` verified on the box; `squeezefs-B-77f4da1d` and `squeezefs-B` kept beside it |
| `squeeze-test:/scratch/tmp/squeezefs` | **REPLACED** by arm B `b377cbb8` (the reset script's client binary) |
| `squeeze-test:/scratch/tmp/sym-box/repo/{tests,.benchmarks/rigs}/` | refreshed from this worktree (449 / 90 files): PR 13e's `sym_post_leave_census` + the PAUSED-phase fix, PR 15's rig, this branch's `8c2da0dc` (the sym write rows' `/proc/diskstats` columns, the F-B1 faces, the touch ledger, `xv_cross_owner_dangling_names` in `SYM_ZERO_KEYS`) and `6e9c602e` (`mw_fleet.sh`'s daemon-pid anchor for arm-suffixed binaries — placed mid-session after H-13E-1) |
| `squeeze-test:/scratch/tmp/rigs/{2026-09-13-sym-pr1-solo-regate.sh,-reduce.py,2026-09-21-sym-box-brackets.sh,2026-09-22-sym-box-perf-phases.sh,2026-09-22-sym-box-perf-agg.py}` | PR 1's pair, the driver, **the perf rig's fix-round revision + the aggregator (the §8 owed placement — done)**; each `cmp`-identical to the repo copy |
| `squeeze-test:/scratch/tmp/sym-box/perf-dwarf-{A,B}/{rename,unlink}.callers-{memcpy_avx512,memmove,memcmp}.txt` (+ `.err`, empty) and `{rename,unlink}.tpc-agg.txt` | **the OWED caller tables, regenerated** from the re-run's DWARF `perf.data` through `2026-09-22-sym-box-perf-agg.py callers … 4` / `flat … 60` (03:54–03:57 UTC, no fleet up): rename `memcpy_avx512` leaf 213 of 6,365 `fuse3-tpc*` samples on A (3.35 %) vs 330 of 6,441 on B (5.12 %), the top chains `handle_setattr`'s `Box::pin` (95 → 144) and `LaneExec::run` (83 → 111) — §3.9.4.1's hand-read numbers confirmed by the persisted tables |
| `squeeze-test:/scratch/tmp/sym-box-13e-stage/` | the staging dir the `scp` / `rsync`s landed in before the root `install`s (the arm, the shim, `SHA256SUMS.13e`, `repo/`) |
| `squeeze-test:/scratch/tmp/sym-box/launch-13e-nw.sh` + `launch-13e-{scale,touch,walls32}.out` + `NW_13E_OUTS` | the N-writer launcher (waits for load1 ≤ 1 — the driver's quiet-box preflight — then one driver pass per gate) and its pointer file |
| `squeeze-test:/scratch/tmp/sym-box/rows-gate1-13e-20260923-015902/` (+ `.log`, `REDUCED.md`) | **gate 1 bracket 1** (A B B A, RT = 60): per row `.stats0/1`, `.diskstats0/1`, `.job`, `.fio.json/.txt`, `_bw.*.log`, `.procstat*`, `.thermal*`, `.dmesg`; per arm `reset-*.log`, `features-*.txt`, the timed mount / remount / umount legs + daemon logs, `*-mdstorm.*`, `prep-*.fio.json` |
| `squeeze-test:/scratch/tmp/sym-box/rows-gate1-13e-20260923-015902-rev/` (+ `.log`, `REDUCED.md`) | **gate 1 bracket 2** (B A A B, `ROWS="mdstorm mount rw4k-kern remount"` — the two DELTA rows' shapes) |
| `squeeze-test:/scratch/tmp/sym-box/13e-nw-20260923-030202-scale.H1-pidanchor{,.log}` | the FIRST gate-3 launch — died at member 0 on H-13E-1 (kept as the finding's evidence) |
| `squeeze-test:/scratch/tmp/sym-box/13e-nw-20260923-030537-scale/` (+ `.log`) | **gate 3** (fleet A; `scale-r1/symscale-1790132759/`: the table, `symscale-faces.txt` (the amplification + F-B1 lines), `disk_pin*.tsv`, per-writer `create-n*-w*.txt`, `m*_pn*{0,c,1}.json`; `scale-r1/daemon-logs/m*.log` — m0's two `flush ceiling OVERRUN` lines at 03:08:22 / 03:10:56) — RED on F-B1 at N = 4 / 8 |
| `squeeze-test:/scratch/tmp/sym-box/13e-nw-20260923-032027-foreign-touch/` (+ `.log`) | **gate 3c ×2** (two fresh fleet As; per position `foreign-touch-r{1,2}/symtouch-*/` — the table, `touch-errors.txt` (empty), `fsck-sym-foreign-touch-{post-leave,offline}.json`, the per-phase snapshots — + `daemon-logs/m*.log`), `SUMMARY.txt` |
| `squeeze-test:/scratch/tmp/sym-box/13e-nw-20260923-033631-walls32/` (+ `.log`) | **gate 7 at N = 32 ×2** (two fresh fleet Cs; `walls32-r{1,2}/symwalls-*/symwalls-{a,b}.txt` with the amplification + F-B1 lines, `disk_pwa*.tsv`, the snapshots, `daemon-logs/m*.log` ×32) |
| `squeeze-test:/scratch/tmp/sym-box/{GATE1_13E_OUT,GATE1_13E_REV_OUT}` | pointer files |
| `/dev/shm/sqz_mdstorm/`, `/run/squeezefs-mwfleet`, `/run/squeezefs-devsub-tcp-mwfleet`, the fleets' netns / veth / `pref 40` rules, `/mnt/sqz-mwfleet/` | **all removed** — every fleet torn down to zero residue (asserted by `mw_fleet.sh teardown` ×6, incl. the H-13E-1 half-formed fleet's sweep), verified at the end: 0 daemons, no `/run/squeezefs-mwfleet*` / `-devsub-*`, no netns, no rule, no veth, `/scratch/tmp/test` unmounted, no `/dev/shm/sqz*`; the devsub's `nvmet_tcp` / `nvmet` / `zram` / `null_blk` unloaded at the end (as found); the fabric's 15 namespaces connected as found |
| the 5 storage nodes | **nothing placed**; the reset rebuilt their null_blk backings + shares per gate-1 arm (8 resets), leaving the fabric converged |

Laptop-side: `/tmp/grok-justin/box-13e/{arms,stage,gate1,gate1-rev,nw/{scale,touch,walls32}}`
(the orchestrator's arm-B build + this rung's pulled artifacts; the
laptop ran nothing of this rung — the batch gate on `b377cbb8` owned it).

**The FOURTH pass (`perf/sym-box-13g`, 2026-09-24 04:05 → 04:57 UTC —
gate 3 only) placed, all under `/scratch/tmp/` (root-owned; the artifacts
are the evidence and stay):**

| path | what |
|---|---|
| `squeeze-test:/scratch/tmp/sym-box/{squeezefs-B-230e95dd,libsqueezefs_il-B-230e95dd.so,SHA256SUMS.13g}` | arm B = PR 13g's landed tip (`230e95dd`, `release`, sha256 `099477dcd1c919cdf492addeb1bcd61cd318187758001ff66318e63e7e49d8b2`; the shim `fd4a02ac8cc88dec…1f1a82b7`), built by the orchestrator, `sha256sum -c` OK + `--version` verified on the box (`built 2026-09-24T03:58:38Z profile release`); the three earlier B arms kept beside it |
| `squeeze-test:/scratch/tmp/squeezefs` | **REPLACED** by arm B `230e95dd` (the reset script's client binary; it was `b0ec7d65…` = `b377cbb8`) |
| `squeeze-test:/scratch/tmp/sym-box/repo/{tests,.benchmarks/rigs}/` | refreshed from this worktree ×3 (471 / 91 files): `76028d5f` before the first row set, `6108e8c1` before the second, `cea35691` after it (`run_mw_matrix.sh` md5 `a9d6d337…` identical both sides at the end) |
| `squeeze-test:/scratch/tmp/rigs/{2026-09-13-sym-pr1-solo-regate.sh,-reduce.py,2026-09-21-sym-box-brackets.sh,2026-09-22-sym-box-perf-phases.sh,2026-09-22-sym-box-perf-agg.py}` | re-placed, each `cmp`-identical to the repo copy (unchanged this rung) |
| `squeeze-test:/scratch/tmp/sym-box-13g-stage/` | the staging dir the `scp` / `rsync`s landed in before the root `install`s (the arm, the shim, `SHA256SUMS`, `repo/`) |
| `squeeze-test:/scratch/tmp/sym-box/launch-13g-nw.sh` + `launch-13g-scale{,-2}.out` + `NW_13G_OUTS` | the N-writer launcher (`BIN=…squeezefs-B-230e95dd`, waits for load1 ≤ 1, one driver pass per gate) and its pointer file |
| `squeeze-test:/scratch/tmp/sym-box/13g-nw-20260924-041135-scale/` (+ `.log`) | **row set 1** (fleet A; `scale-r1/symscale-1790223117/`: the table, `symscale-faces.txt` (the launch stamps, the amplification, the F-B1 and F-R5 faces per writer per row and per create phase), `symscale-verdict.txt`, `launch-n*.tsv`, `create-n*-w*.txt` (each with `launch_ts= end_ts=`), `disk_pin*.tsv`, `m*_pn*{0,c,1}.json` + the `pc*` copies, `removed-sample.txt`, `fsck-sym-scale.out`; `scale-r1/daemon-logs/m*.log` — m60's F-R6 windows, m0's `GrowRing` carves) — rc=0, the oracle reached |
| `squeeze-test:/scratch/tmp/sym-box/13g-nw-20260924-044235-scale/` (+ `.log`) | **row set 2** (a fresh fleet A; the setup outside the clock; `scale-r1/symscale-1790224978/` — the same shape, `m*_pend0.json` copied and the end-of-leg snapshots under the first build's label `m*_pend.json` — `m60_pend.json` withheld 7,266 / projection refreshes 79 / screened 45 — with no end-of-leg FACES and no post-leave CENSUS run; `daemon-logs/m0.log:22448` the 1,101 ms `OVERRUN` line) — RED on the trip; the leg died after its table on the end-of-leg snapshot's label (fixed `cea35691`), one line before the must-stay-0 die |
| `/run/squeezefs-mwfleet`, `/run/squeezefs-devsub-tcp-mwfleet`, `/mnt/sqz-mwfleet/` mounts | **all removed** — both fleets torn down to zero residue (asserted by `mw_fleet.sh teardown`), verified at 04:57 UTC: 0 daemons, no fleet state, no netns, no `pref 40` rule, no `/dev/shm/sqz*`, nvmet configfs empty, the devsub's `nvmet_tcp` / `nvmet` / `zram` / `null_blk` UNLOADED (only `nvme_tcp nvme_fabrics nvme_core fuse` loaded, as found), the fabric's 15 namespaces connected as found, `/run/squeezefs/` the same four stale IL sockets, 243 G free; `/scratch/tmp/sym-box` 5.1 G |
| the 5 storage nodes | **nothing placed**, nothing run (no gate-1 reset this pass) |

Laptop-side: `/tmp/grok-justin/box-13g/{arms,launch-13g-nw.sh,nw/{13g-nw-20260924-041135-scale,13g-nw-20260924-044235-scale,REDUCED.txt}}`
(the orchestrator's arm-B build + this rung's pulled artifacts and the
reduction; the laptop ran nothing of this rung but the reductions).

## 9. The flip decision (for PR 14)

> **Status (PR 13b, 2026-09-21): the three PRODUCT blockers below are closed on `feat/sym-metanode-ship` (`cd85f701` defect 32, `cc642b6a` §4.4af, `53ffb626`+`31519ceb` §4.4ag); the box brackets remain the orchestrator's; the storm ×10 count is NOT MET on that branch (see §7's status line — two owed items, no acked loss in 19 seven-victim rounds). The decision text below is as recorded at PR 13.**
>
> **Status (the box-rows rung, 2026-09-22 — the re-read the brief asked for, with the box's numbers in hand):** with PR 13b landed (`dev` @ `7b2ef9e9`, the batch gate GREEN), blockers (0) §4.4af, (1) defect 32 and (1b) §4.4ag read CLOSED; blocker (2) — the box brackets — RAN (§3.9.1 / §3.9.2) and the decision stays **NOT YET**, for the box's own reasons. **The exact list of design gates that do NOT read MET on the box:**
> * **gate 1** — MISS: PR 13b's FLAT path is 3.4–5.8 % slower than the pre-program tip on mdstorm `mkdir` / `rename` / `unlink` (both brackets, both orders), with a reproducible rand-4k residual (rr4k −1.8 %, rw4k −2.5…−3.8 %) at the floor; a flat-path regression the flip would ship to every solo mount (+100k DLM guards per storm — the PR-4 rename lock-set fix's shape — and +3–5 µs/op on the handler lanes; the `mkdir`/`unlink` sites need a `perf` A/B);
> * **gate 3** — MISS at N = 8 (4.27× creates / 5.33× ingest vs ≥ 5.6×) on a one-box fleet whose manager runs its own storm (the design's "measured-simulated" caveat, now with the box's number); N = 2 / 4 MET;
> * **gate 3c** — MISS (mechanism): a LIVE holder recalled once by a 64-touch burst (F-B2: the dominance rule's per-slot `ops_h` against a storm whose children spill to the rotor — §7 item 10's premise, now on LIVE);
> * **gate 5** and **gate 7 at N = 32** — BLOCKED (F-B3): the manager's cluster-wire connection cap derives to 64 under `FLEET_SHARE=32`; a 32-member fleet never comes up on PR 13b's binary — the design's own N = 32 rows are unmeasurable until the listener's cap derives from the width it serves;
> * **the must-stay-0 tripwire `appender_flush_ceiling_overruns`** trips on the acceptance venue (F-B1: four times in 12 minutes, 1–32 ms past the 1,100 ms ceiling, no recovery in flight) — PR 14's margin derivation is no longer a laptop reading.
>
> **Reading MET on the box:** gate 2 (1.04–1.07× of S0), gate 3b (one flip, `shipped ≡ served`, `K + C + 3` tokens, 3,400–3,581 creates/s into one directory), gate 7 at N = 8 (983 frees/s, `shipped ≡ served ≥ displaced`; the 8-mount join storm 3.92 s) — each with the tripwire caveat above where it applies. **What PR 14 flips on, restated:** the gate-1 regression attributed and closed (or adjudicated as the shipped-bug fixes' price with the owner's word), F-B3's cap re-derived (then the N = 32 rows run), F-B1's margin derived, F-B2's rule adjudicated, and the storm ×10 count from zero on that binary — then the flip. The decision text below is as recorded at PR 13.
>
> **Status (the box RE-RUN on PR 13c's binary `77f4da1d` — `perf/sym-box-rerun`, 2026-09-22 11:02 → 13:52 UTC; §3.9.4 — the re-read the brief asked for, with the fixed binary's numbers in hand). The decision stays NOT YET.** With these numbers, the design gates the flip requires read as follows. **Reading MET on the box (this binary):** **gate 3c's LIVE and IDLE laws** (a LIVE holder never recalled — 384 touches, 0 handovers over two fresh fleets, F-B2 FIXED; an idle tree moved in 4–5 bursts) — **its third law, the PAUSED job, NOT RUN: the phase's job never ran on any venue (§4.4aj, a harness defect fixed on this branch and proven locally), so gate 3c as a WHOLE reads LIVE ✓ / IDLE ✓ / PAUSED owed to the next box session**, **gate 5** (the 1 × 31 broadcast: exact at the next resolve, 155 ≡ 155, recall RTT 300 µs, hold 0 — twice; F-B3 FIXED), **gate 7 at N = 32** (row (a) 1,002–1,299 frees/s with `shipped ≡ served ≡ displaced` and the ledger's 1.000× submitted/user; row (b) 32 mounts in 3.66–3.68 s, 250–269 verbs, 12.3–14.6 s of service — twice), and — from the previous pass, not re-run — **gate 2** (1.04–1.07× of S0) and **gate 3b** (one flip, `shipped ≡ served`, `K + C + 3` tokens). **The exact list that does NOT read MET:**
> * **gate 1** — MISS on mdstorm `rename` (0.960 / 0.967) and `unlink` (0.956 / 0.967), both orders of two brackets; `mkdir` CLOSED (0.976 / 0.984), `rr4k` PAR (0.999), everything else within noise, `rw4k` −3.0 % at the floor. ATTRIBUTED (§3.9.4.1): the PR-4 rename lock-set fix's priced +1 guard (kept) and the `handle_setattr` future's construction + lane move on the kernel's per-op SETATTR echo (named by the DWARF legs); **FIXED in PR 13f** (the unarmed setattr future 26,016 → 896 B, unlink 11,088 → 280 B — §3.9.4.1), the bracket re-reads on the flip binary;
> * **gate 3** — MISS on the WALL law at N = 8 (4.27× creates / 5.37× ingest vs ≥ 5.6×; `C/CPU-S` 0.61×) — the co-located venue's term re-read to 0.01× of §3.9.2's; N = 2 / 4 MET; the per-NODE law UNMEASURED (PR 15). The A arm is measured for the first time: the shipped authority + co-writers are bounded at 0.07× at every N — the armed plane creates 55× faster at N = 8 on the same binary (F-R2 names why the shipped path is that slow);
> * **the must-stay-0 tripwire `appender_flush_ceiling_overruns`** — NOT closed: six increments on five writers across three fleets in 45 minutes (the two with a WARN line 16 / 106 ms past the 1,100 ms ceiling; the scale fleet's four have no age reading; one of the six — m60 — established as not under a storm, the "between the rows" three under each writer's `rm -rf` of its 20k-file tree + the joins), **with PR 13c's exclusion excusing 0 ns on every writer** — the margin derivation (§7 item 3) is the whole remaining item, its first piece the missing per-cycle pass-wall instrument, and it is a FLIP PRECONDITION: every N-writer row set on the box stops at its first trip, so gates 3 / 3c / 7 cannot read MET as ROW SETS however their rates read;
> * **two NEW product findings on the armed plane** (§3.9.4.3, §7 item 13): **F-R3** — every cross-owner unlink of a child another appender minted orphans the child's inode (the witness read off the projection; 430 / 512 leaked in one leg; invisible to fsck while the lessee lives) — a FLIP PRECONDITION (an ordinary `rm -rf` leaks on the default the flip would ship); **F-R4** — a create into a directory whose slot moves to the creator answered `ENOENT` once (defects 29 / 30's family; a correlation-based hypothesis until pinned — §3.9.4.3);
> * **gate 4** — the kill matrix ×10 from zero: NOT MET — `sym-crash` 10/10 GREEN on nine consecutive from-zero runs on PR 13's binaries but `sym-storm` ×10 never reached (7 + 3 GREEN then §4.4af's loss; on PR 13b's tree 8/10 · 2/10 · 9/10 — the rejoin slot-tree economy's `ENOSPC`, a harness precondition, a joiner's tree-0 projection loop); the counts restart from zero on the flip binary (LOCAL by the venue law — not a box row).
>
> **The gates this re-run does not move, placed (design §8 rows 4 / 6 / 8 / 8b / 9 — so the list is exhaustive against the table):** **gate 6** (format cost: the 46-volume width row + the N = 8 / 32 appender rows) RUN locally, VALID (§3.6; LOCAL by design — no rate law); **gate 8** (SIM-1 12,500 × 64) MET (§5; tier (ii), the design's own class); **gate 8b** (fidelity `full`) PASS 190 / FAIL 0 (§3.7; LOCAL); **gate 9** (the cloud `mw` preset — gate 2 + 3b on four real nodes) NOT RUN — PR 15's, expressed approval per run (the per-NODE law of gate 3 is its instrument too).
>
> **What PR 14 flips on, restated once more:** F-R3 fixed and pinned (the witness at the child's holder), F-B1's margin DERIVED from a pass wall that is MEASURED first (the per-cycle instrument), with the box's two logged ages (16 / 106 ms past) and the four unlogged increments as its input and the six re-read as 0, F-R4 in the retryable class, gate 1's setattr future shrunk (or the −3.3…−4.4 % on rename / unlink adjudicated as the shipped-bug fixes' price with the owner's word), the storm ×10 from zero on that binary — then the flip binary's box brackets of EVERY gate (2 and 3b included). Nothing in this rung's product findings is a class the design did not state: F-R3 is PR 6's deviation (3) left standing on the N-daemon fleet; F-B1 is §7 item 3 exactly; F-R2 is the shipped path the flip retires.
>
> **Status (PR 13c, the box campaign — `fix/sym-box-campaign`, 2026-09-22): what LANDED against the list above, and what still stands.** *Landed, red-first:* **F-B3** — the listener cap derives from the RAW root × the shipped factor, ceilinged by the fd budget (`RLIMIT_NOFILE / 8`), a refused dial retries from the accept tick to the dial bound then surfaces the typed `ListenerRefused`, a token reader's ROOT attr survives a transient wire class (the `.stats` EINVAL), and the first 32-member fleet FORMED on the laptop (§3.9.2's "→ fixed"; H-C1 / H-C2 the harness shapes it uncovered); **F-B2** — the dominance rule's `ops_h` counts the holder's work on the slot's SUBTREE across volumes (design §5.1.4 as built, the hint's two error directions and the root-ward consequence stated); **F-B1** — the flush-ceiling audit EXCLUDES the SMO mutex's structural holds: an overlap-bounded exclusion (over-excuse ≤ one cadence tick per hold), each class CAPPED at a published bound (a recovery's at `appender_recovery_bound_ms`, a service hold's at one landing ceiling — `appender_flush_ceiling_service_cap_ms`), the excused Σ published (`appender_flush_ceiling_excused_{ns,max_ms}`), the pass's own wall (its device time, its wait on a peer) never excused; **the gate-1 flat path** — three unarmed-path costs deleted (`stripes_armed_any`, the memo fed on an armed set only, `token_serve`'s two-probe return), the rename lock-set fix's +1 guard priced and kept, and the startup `RLIMIT_NOFILE` raise confined to the listener caps (`uring_fs::fd_cache_cap` derives from the limit AS FOUND — review round 1, Issue 8: the first build had grown the flat path's fd cache 32×). *Still standing (the flip's list, restated for PR 14):* (a) **the box re-runs** of gates 1 / 3 / 3c / 5 / 7@N=32 on PR 13c's binary — no box row ran in PR 13c (the counted-run law); (b) **the flush-ceiling MARGIN's derivation from the measured pass wall (§7 item 3) — NOT done**: F-B1 landed an exclusion of another actor's holds, which is not a derivation of the margin, and the box's 1–32 ms overruns are what that derivation must price; (c) **gate 3's per-NODE law ("bounded by no node") is UNMEASURED on any venue**: the box's N = 8 MISS is attributed to the co-located venue (§3.9.3), the per-daemon-CPU-second face (0.63× at N = 8 with the ingest CPU folded in; the create phase alone is `sym-scale`'s new `C/CPU-S` column, unrun on the box) is that venue's PROXY, and **a multi-node venue — PR 15's cloud row — is the law's instrument**; (d) the storm ×10 count from zero on PR 13c's binary.
>
> **Status (PR 13e, the box re-run's armed-plane findings — `fix/sym-box-rerun-findings`, 2026-09-22): NOT YET, the list restated.** *Landed, red-first:* **F-R3 (P0)** — a cross-owner plan's inode witness is read AT THE HOLDER of the ino's slot (`read_inode_witness` — the writer's read divert; the local record verbatim on every unarmed / flat mount), the dangling-name arm is the must-stay-0 `xv_cross_owner_dangling_names`, a projection's `None` on a live foreign lessee's slot refuses the retryable class, and fsck C9's era floor on a forest consults tree 0 — an UNLEASED slot's records are prior-era candidates, so the post-leave census (joiners, then the manager; the offline fsck asserting nothing exempted) is a verdict on the joiner-minted class the first build's census exempted whole (review round 1, Issue 2) (§4.4al); **F-R4** — a served step for a slot the holder no longer leases is the typed slot-moved class, never the op's `ENOENT`, and a wire grant adopts the tree before naming its lessee (§4.4am); **F-B1** — the flush-ceiling MARGIN's derivation LANDED: a forest volume's cadence fires `max_age − term` from the last collection, the term = the horizon maximum (64 cycles) of the measured pre-barrier wall + the decision's lateness beyond one tick (the tick's wait for the SMO mutex left out; the lateness recorded on the cycle it RUNS alone and capped at one ceiling — review round 1, Issue 1: the first build made an idle span the next cycle's term and the trigger 0 for 64 cycles), the published ceiling never widened, the bit-17-absent cadence decision-identical, published `meta_kv_checkpoint_{term,trigger}_ms` (§4.4an; item (b) above is DONE as a mechanism — the box re-run judges whether it priced the box's 16–106 ms). *Still standing:* (a) **the box re-runs** of gates 1 / 3 / 3c / 5 / 7@N=32 on THIS binary — F-B1's derivation and F-R3 / F-R4 are judged there (every N-writer row set stopped at its first trip on PR 13c's); (c) gate 3's per-NODE law (PR 15's venue); (d) the storm ×10 from zero on this binary; **F-R2** (the SHIPPED authority's per-create `readdir` — the co-writer posture the flip retires) and **gate 1's `handle_setattr` economy** (PR 13f, in parallel) are not this rung's; the PAUSED law of gate 3c stays owed to the next box session (§4.4aj of the box-rerun record).

> **Status (PR 13d, `fix/sym-stripe-ls-token-economy`, 2026-09-22): the `2K + C + 3` reading PR 15's local pass took on gate 3b's `-ls` half is ATTRIBUTED to fleet state (a ROOT striped by the preceding `sym-scale` leg on the same fleet — one records-only grant per root stripe at the reader's first `stat /` after its lookup learnt the map, §7 item 7's class), identical on `7b2ef9e9` and `77f4da1d`, pinned in-process both ways (§4.4ak); no product change; **adjudicated option (c): design §8 row 3b's `-ls` law is `K_D + K_root + C + [0, 4]`**, the leg's code change PR 15's (`tests/sym_rows_lib.sh`, the one law library).**

> **Status (the THIRD box pass — PR 13e / 13f's binary `b377cbb8`, `perf/sym-box-13e`, 2026-09-23 01:53 → 03:52 UTC; §3.9.5 — the re-read the brief asked for, with the fixed binary's numbers in hand). The decision stays NOT YET, on ONE remaining box item.** **Reading MET on the box (this binary):** **gate 1** — MET by the rule (`rename` 1.012 / 0.998 PAR, `unlink` 1.054 / 1.042 B AHEAD both orders — the setattr term FIXED; `rr4k` 1.005, `rw4k` 0.984 / 1.015, `wfresh` 0.991, the other mdstorm phases within noise with `create` −2…−3 % at the floor; mount time within noise in bracket 1 and DELTA by 0.03 over a 30 % band in bracket 2 — UNCONVICTED by the A-B-B-A law (PR 1's band rule composed with "a single-order delta reproduces before it convicts"), directionally consistent across three box brackets (1.13 / 1.25 / 1.33 — B slower by 0.06–0.12 s of median), a +0.1 s term on a 0.4 s event, stated; the post-`rw4k` clean unmount's +0.5–0.9 s is not a gate law but a NEW, unattributed flat-path DELTA — owed with its instrument, §7 item 16); **gate 3c** — MET on ALL THREE laws twice, the PAUSED law's first real run included (F-R3 FIXED as a verdict; F-R4 FIXED as far as the leg reaches — 0 errnos on 902 touch creates, the slot-moved retry class unexercised, the pin its proof); **gate 7 at N = 32** — MET on both rows twice with `appender_flush_ceiling_overruns` 0 on all 32 writers and the `/proc/diskstats` face 1.000× user; and from the previous passes, not re-run — **gate 2** (1.04–1.07× of S0), **gate 3b** (one flip, `shipped ≡ served`, `K + C + 3` tokens), **gate 5** (the 1 × 31 broadcast exact, 155 ≡ 155, RTT 300 µs, hold 0). **The exact list that does NOT read MET:**
> * **gate 3** — the WALL law at N = 8 (3.53× creates vs ≥ 5.6×; ingest 5.50× MET; N = 2 / 4 MET) — the co-located venue's term plus an INFERRED ≈ 3.9 s of launch skew in this row (wall − the longest storm; the root STRIPED at the row's eight mkdirs is the hypothesised cause; the storms' own concurrency ≤ 4.5×, an upper bound); `C/CPU-S` 0.67×; **the per-NODE law UNMEASURED (PR 15's cloud row, its instrument)**; **and its ROW SET stopped at r1 on the tripwire (below)**;
> * **the must-stay-0 tripwire `appender_flush_ceiling_overruns`** — NOT closed: **two increments on the MANAGER inside `sym-scale` (1,127 / 1,125 ms — 25–27 ms past the ceiling) WITH PR 13e's derivation ENGAGED but its horizon EMPTY** (volume 1's `meta_kv_checkpoint_term_ms` 11 / 4 ms at each storm's start, the trip cycle's own 133 / 127 ms in the window only after; 199 quiet cycles between the rows emptied the 64-cycle horizon; the storm's steady state did not trip; 0 ns excused), both at the FIRST cycle of a joiner create storm's `ExtentGrant` / `ReturnExtents` burst of 50–80 verbs/s — the class is the first storm cycle after a quiet horizon (the horizon's memory + the in-flight service, §7 item 3); **0 increments on the touch and walls fleets** (70 writer-legs — the re-run's quiet-joiner and rewrite trips did not recur). **A flip precondition still**: the manager's term must fold the verb service in flight (§7 item 3) and/or the service must stop being a storm — **F-R5** (§7 item 16; PR 13g the fix rung): the manager derives a WIRE joiner's grant from an EWMA it never receives (`ewma = 0` → the floor 8), so every production joiner refills in ≤ 8-extent grants, and its 512 KiB floor ring checkpoints ≈ 8×/s under a create storm, retiring and re-claiming at the SMO grain (≈ 100 manager verbs per joiner per storm; the EWMA on the ask the lead lever, PR 2's drain-then-grow beside it);
> * **gate 4** — the kill matrix ×10 from zero on this binary: NOT RUN in this rung (LOCAL by the venue law; the counts restart on the flip binary).
>
> **The gates this pass does not move, placed (design §8 rows 6 / 8 / 8b / 9 — so the list is exhaustive against the table): unchanged from the re-run's placement** — **gate 6** (format cost) RUN locally, VALID (§3.6; LOCAL by design, no rate law); **gate 8** (SIM-1 12,500 × 64) MET (§5; tier (ii)); **gate 8b** (fidelity `full`) PASS 190 / FAIL 0 (§3.7; LOCAL); **gate 9** (the cloud `mw` preset — gate 2 + 3 + 3b on N real nodes, the per-NODE law's instrument) NOT RUN — PR 15's, expressed approval per run.
>
> **What PR 14 flips on, restated once more:** F-B1's remaining term priced — the manager's anticipated term under the joiners' verb service (§7 item 3) with F-R5's supply grain fixed beside it — and `sym-scale` on the box reading 0 trips through N = 8 (the row set completing to its oracle for the first time on the box); the storm ×10 from zero on that binary; then the flip binary's box brackets of EVERY gate (2 / 3b / 5 included, at the minimum count). Gate 1's mount row and the unmount row are stated for that bracket's ms-grained tape; nothing else this pass found is a class the design did not state (F-R5 is PR 3's §5.3.3 derivation reading an EWMA the wire never carries — the floor for every joiner — meeting PR 2's own "drain-then-grow stays owed" on a storming joiner).
>
> **Status (PR 13g, `fix/sym-joiner-supply-and-manager-term`, 2026-09-23 — the third box campaign's two findings, both FIXED red-first; the box bracket on the flip binary owed):** **F-R5** (§4.4ao) — the joiner's extent supply: the manager derived every WIRE joiner's grant off a rate it never saw (the floor), the ring never grew, and the storm ran at the one-SMO grain (≈ 105 manager verbs per joiner per storm); now the joiner's own rate sizes its asks, the grant is a pool that recycles its retired images, the ring grows over the wire under the closed gate (PR 2's drain-then-grow), and a rejoin starts at the sized ring and pool (`appender_hint`) — the manager's verbs per joiner per storm fall an order of magnitude in process (≈ 30 per 12 s against ≈ 295). **F-B1** (§4.4an's PR 13g paragraph) — the class re-read as the FIRST storm cycle after a QUIET horizon (terms 11 / 4 ms at the trips), priced by the LIVE projection off the pending work beside the horizon term; RED-first on the box's row sequence in process. **The tripwire's box verdict is the flip binary's bracket** — every N-writer row set stops at its first trip, so gates 3 / 3c / 7 read MET as row sets only there.

> **Status (the FOURTH box pass — gate 3 on PR 13g's binary `230e95dd`, `perf/sym-box-13g`, 2026-09-24 04:05 → 04:57 UTC; §3.9.6 — the re-read the brief asked for). The decision stays NOT YET; the list shortens by one item and grows by one finding.** **Reading MET on the box on this binary:** **F-R5** — FIXED as a verdict on every joiner in two row sets (rings 768 KiB–2.3 MiB, 1–3 wire grants per joiner per row, reactive 0, returns ≪ compactions, the manager's N = 8 row 88 verbs / 0.114 s of service against the third pass's 892 / 2.93 s, the closure exact set-wide); **gate 3's ROW SET completes to its oracle on the box for the first time** (set 1: 0 trips through N = 1/2/4/8, deleted-stays-deleted 0 / 3,000 ×2, fsck clean); and from the third pass, not re-run — gate 1 (by the rule), gate 3c (all three laws ×2), gate 7@N=32 (×2), gate 2, gate 3b, gate 5. **The exact list that does NOT read MET:**
> * **the must-stay-0 tripwire `appender_flush_ceiling_overruns`** — NOT closed: **0 in set 1 (8 writer-rows at the manager, 44 joiner-rows in the pass), +1 in set 2 — the manager's volume 1 at 1,101 ms, 1 ms past, at the N = 4 storm's END with ONE verb served on that volume across the row.** The third pass's class (the onset after a quiet horizon under the joiners' grant storm) is GONE — exercised at N = 2 / 4 in both sets (terms 6–7 ms at the storms' starts) and not tripped, the grant burst absent; what tripped is the derivation's RESIDUE: the LIVE projection under-priced the storm's END cycle by ≈ 65 ms (84 ms at the tick's decision against the cycle's ≈ 150 ms pre-barrier wall — the create-end snapshot preceded the trip with overruns 0; the cycle's own term 151 and lateness 35 entered the horizon after it; no barrier face reads above 2 ms), and with the 35 ms lateness the fixed two-tick margin was 1 ms short. §7 item 3's next piece (the projection's growth between the decision and the flush + the lateness term, with the per-cycle tape that discriminates) — a FLIP PRECONDITION still, now a 1-in-8-rows, 1 ms class with no service term behind it;
> * **gate 3's WALL law at N = 8** — 4.61× creates on the storms' own clock (set 2, the setup outside the clock; per-writer storms 10.8–13.9 s, the launch skew 12 ms; Σ per-writer rates 5.25× the bound) vs ≥ 5.6× — the co-located venue's term (§3.9.3; `C/CPU-S` 0.66× — a reading that still folds the setup's CPU in, Issue 4), no longer a launch artifact: the third pass's inferred 3.9 s was a MEASURED 9.1 s of three fresh joiners' 3.0 s `mkdir`s into the freshly striped root, taken out of the row's clock; the ingest multiple is its sub-second N = 1 base's noise (the N = 8 absolute 7.3–7.6 GB/s across three passes) — **the per-NODE law UNMEASURED (PR 15's cloud row, its instrument)**;
> * **F-R6 (new, §3.9.6.3 / §7 item 17)** — a joined writer's FORGET-driven reclaim prices destroys for the HOLDER's inos through its stale projection (6,782 / 7,266 withheld per set; nothing destroyed — the belt is the commit door's foreign-slot refusal, `slot_door_refusals` [0, 0] on m60 through the end of both legs; defect 18's 256-restart `root-seq` loop under it (defect 34's family; PR 13b's "9/10" was tree 0's child-seq); the fix sites `queue_reclaim_inode` / `reclaim_orphaned_batch` and `destroy_entry_bytes`) — a CPU + log storm on a path a token client must not take, invisible to the row's oracle; a PR 14 item beside the served-mutation hook's `ENOENT`-at-WARN (272 k / 275 k lines per set — 21–46 % of the served invalidations, `session.rs:1092`) and the fresh joiner's 3.0 s first create into a striped root;
> * **gate 4** — the kill matrix ×10 from zero on this binary: NOT RUN in this rung (LOCAL by the venue law; the counts restart on the flip binary).
>
> **The gates this pass does not move, placed: unchanged from the third pass's placement** — gate 6 RUN locally, VALID; gate 8 MET; gate 8b PASS 190 / 0; gate 9 NOT RUN (PR 15's).
>
> **What PR 14 flips on, restated:** F-B1's residue priced (§7 item 3 — the projection's growth between the decision and the flush + the lateness, the box's 1 ms the input) and `sym-scale` on the box reading 0 trips through TWO row sets; F-R6's reclaim guard; the storm ×10 from zero on that binary; then the flip binary's box brackets of EVERY gate (2 / 3b / 5 included, at the minimum count) — gate 3's row with the setup outside its clock and an ingest base the law can divide by; the `umount` post-`rw4k` DELTA and the mount row's ms-grained tape ride that bracket; the per-NODE law is PR 15's.
>
> **Status (PR 13h, `fix/sym-box-pass4-findings`, 2026-09-24 — the fourth box pass's three findings, each FIXED red-first; the box bracket on the flip binary owed):** **F-R6** (§4.4ap) — a joined writer's FORGET-driven reclaim priced and drove destroys for the manager's corpses off its own stale projection (6,782 / 7,266 `destroy WITHHELD` per row set on the box, defect 18 / 34's loop under it; the structural belt was the commit door's `SlotBusy` — `slot_door_refusals` [0, 0] on m60 through `m60_pend.json`); now a FORGET of an ino in a slot this mount does not reclaim is a token client's forget — dropped at the reclaim's entry before any read (`RoutedMetaBackend::owns_inode_reclaim`, the corpse sweep's law), counted `reclaim_foreign_slot_forgets`; since review round 1 (Issue 1, `edfd1852`) that forget TRAVELS to the slot's reclaimer (its holder — the manager for an unleased slot) as a reclaim hint (`MetaCall::ReclaimHint`) the reclaimer runs as its own FORGET, because the manager sweeps unleased slots only at mount and its kernel never FORGETs an inode it never held (the released-slot corpse leaked until a remount); the unarmed / flat law byte-identical. **F-B1's trip** (§4.4an's PR 13h paragraph) — read off `pc41` / `pn41`: a wave of 38 promised compactions priced at 2.04 ms per image and paid at 3.97 (38 × 3.97 = 151 = the trip cycle's term); the unit's grain floor (a half / quarter reading on the drain's small passes) is deleted — the unit is the pass's mean per item; the deferred-flush barrier between the decision and the cycle is priced too (≈ 2 ms on the box); every cycle's TAPE rides `.stats meta_kv_checkpoint_last_cycle` and the overrun WARN, so the next trip attributes itself; RED → GREEN in process. **The served-mutation hook's WARN storm** (§4.4aq) — a notification's `ENOENT` is counted (`fuse3_notify_enoent`), never the interrupted-request WARN (`session.rs:1092`); its other errnos never end the reply task. **What PR 14 flips on, restated:** `sym-scale` on the box reading 0 trips through TWO row sets on THIS binary (the tape read at any trip), the storm ×10 from zero on it, then the flip binary's brackets of every gate; the fresh joiner's 3.0 s first create into a striped root stays a PR 14 / 15 latency item.

> **Status (PR 15 Phase B, run 1 — the cloud row, 2026-09-24; §3.10 — the FIRST venue where two kernels shared one metadata LUN). The decision stays NOT YET, and the list GAINS a design-level blocker.** The owner-approved S2 + 8 oss shape (17 × i4i.2xlarge, one symmetric writer per node) launched, deployed and — on its second assemble, after the baked AMI's cloned `/etc/machine-id` was regenerated — assembled 8 REAL nodes (`appenders_known 8`, `membership_writers 7`, 8 of 8 device registrants), then **FAILED on its first row**: gate 2's `sym-1` arm, the joined writer's `mkdir` under the root `EINVAL`, every joiner write-dead from its join with the manager replaying every grant ask verbatim (2,083 of 2,135 verbs — the contemporaneous summary's gauges; the evidence is lost, §3.10's ledger). **The exact list that does NOT read MET, as this run leaves it:**
> * **F-C1 — the shared-LUN rule (§4.4ar, §7 item 20): a FLIP BLOCKER of the DESIGN class.** Every metadata read and write is BUFFERED through each host's block-device page cache (`uring_fs` opens `O_CLOEXEC` only; the data path alone `O_DIRECT`), so on two hosts a joiner reads its own stale image of the manager's writes — the appender page, the projections, the directory, the ledger. Every co-located venue was structurally blind. A default cannot flip on a plane whose cross-host reads are incoherent by construction; the remedy (`O_DIRECT` or explicit invalidation on every shared-LUN metadata read and write, sector-aligned staging) and its two-kernel fixture (a qemu/KVM guest member over a laptop-exported nvmet-tcp namespace) are **PR 13i**'s, which now gates PR 14 ahead of everything above;
> * **F-C2** (§4.4as — the joiner rebuilds its grant from the device page instead of the `Joined` reply's word) and **F-C3** (§4.4at — `clone_kv_error` flattens every retryable class to `Corrupt` → `EINVAL`, every layout) — PR 13i's, F-C3 first;
> * **gate 9 / gate 3's per-NODE law — INCOMPLETE, UNMEASURED:** the run produced no per-node number (the first row died at its first `mkdir`), and the evidence it pulled (`.benchmarks/cloud/2026-09-24-152527/`) was LOST with the dev machine the same evening; the conclusions rest on the contemporaneous summary in the run log. The re-run is owed on the flip candidate's binary after PR 13i lands, **with a NEW expressed owner approval for that specific run (the owner rule of 2026-09-24 21:20: nothing runs on AWS until PR 13i has landed)**;
> * everything the fourth box pass left standing (above) — `sym-scale` on the box through two row sets on PR 13h's binary, the storm ×10 from zero, then the flip binary's brackets of every gate.
>
> **What the run PROVED:** the PR 15 instrument end to end on 8 real nodes — the fabric (17 nvmet-tcp namespaces, PR verified end to end), the ladder over a real wire (every joiner `joined_registrant_posture registrant`), the device's 8 / 8 registrant report, the driver's preflight — and that the venue finds what no co-located venue can. Four rig defects it found are FIXED on the redo branch (the machine-id step, `format --force`, the apt hygiene, the every-node estimate); the row's cost was ≈ $5.2, nothing billing. **What PR 14 flips on, restated:** PR 13i landed (F-C3 → F-C2 → F-C1, red-first, F-C1 on the two-kernel fixture); then the list above as the fourth pass left it; then the cloud re-run's per-node reading of gate 3.

**Decision: NOT YET — four blockers, three of them product (§7's
flip-blocking items (i)–(iii) + the box).** (0) **Fix-round
finding 1 (§4.4af)**: an acked-writes LOSS on the death path under the
seven-victim + cross-owner + striped shape — 25 of 17,376 fsynced files,
one writer's moved files, at neither source nor destination after the
recovery (10 of them renames whose `mv` had RETURNED — the P0 signal; 5
whose `mv` had not, which Issue 25's oracle now judges against the intent
gauges) — found by the fix round's storm and NOT attributed; the ×10
storm count restarts from zero on its fix. A default cannot flip on an
unattributed acked loss. (1)
**Defect 32 (§4.4z, §7's flip-blocking item (i))**: a file created on one
mount cannot be `chmod`ed, `touch`ed, `setfattr`ed or WRITTEN from a
mount that does not lease its slot — as found `ENOENT` / `EOPNOTSUPP` / a
refused publish, and an append without an fsync acked bytes that never
landed. The design states the arm (§5.10's "1 custody grant + 1 publish
ship"); PR 9 built the grant, nobody built the ship, and the owed item
left the ledger at PR 12b. A default that flips on this loses data on an
ordinary POSIX op; it is the rung the flip waits for first. The interim
posture (fix rounds 1–2): the class refuses LOUD and typed —
`EREMOTE` naming PR 13b (§4.4ai: round 1's `EOPNOTSUPP` let coreutils'
`chmod` exit 0 on a refused syscall) at the `open(2)` for write (so `>>`
fails at the open and never acks a byte), at `setattr`/`setxattr`/the publish, and
as a belt in the `write` handler (there the refusal reaches an
application at `write(2)` only on a `--no-writeback` / `O_DIRECT` /
`O_SYNC` path, else at `fsync`/`close` through the kernel's errseq —
POSIX-16's class); never `ENOENT` for a file that exists; PR 13b's
contract stands `#[ignore]`d RED with its three faces (§4.4z). (1b)
**Fix-round finding 2 (§4.4ag)**: a joiner's user read answering `EIO`
for one beat inside a manager failover — a gate-4 PRECONDITION: gate 4's
law is "refusals 0" on the death path, and a user op failing `EIO` there
violates it whatever the window's width (a one-beat window is NOT
admissible as a default — the S6 reclaim is designed so a live member
never observes its own re-assertion as an error); PR 13b clears it beside
defect 15's follow. (2) The box brackets: the mechanism half of
every gate is GREEN on the dev box from zero on the final binaries (§3)
— the fleet legs complete, every must-stay-0 tripwire but the
venue-attributed flush-ceiling overrun reads 0, the kill matrix
(`sym-crash` 10/10 on nine consecutive from-zero runs, attempts 7–15;
`sym-storm`'s count per §3.8) loses nothing acked, deletes stay deleted,
fsck is clean after every round. The RATE half of gates 1 / 2 / 3 / 3b /
3c / 5 / 7 is the box's by the venue law (`51bf21e1`), and no box row
ran in this rung (§8): every local rate is venue-attributed pending the
box — gate 3's create law at N = 8 read 2.9–6.0 × across five from-zero
runs on this laptop (§3.1), a band only the box can settle before the
default flips.

Gates, exactly:

| Gate | Mechanism (dev, from zero, final binary) | Rate (box) |
|---|---|---|
| 1 solo re-gate | not this rung's (PR 1's rig; the flat path takes ONE behaviour change — defect 6's shipped-bug fix, pinned red-first flat — and behaviour-identical per-op atomic loads; every other PR 13 change is behind bit 17 + the knob, pinned per fix) | OWED — arm B = the flip binary (PR 14's default-on tree; no arm-B SHA stands in this rung) vs arm A `3228fcb8` |
| 2 `tar -x` | mechanism GREEN (verbs/entry 0.012, handovers 0, `dlm_rpcs` 0, oracle clean) | OWED — laptop read 0.96–1.02 × of S0 at netem 250 µs, SCOPING, venue-attributed |
| 3 scale | mechanism GREEN at every N every run (complete, `appenders == N`, tripwires flat, fsck clean, deleted stays deleted) | OWED (the deciding row) — the 0.7 × N law read 2.9–6.0 × at N = 8 across five laptop runs, SCOPING, venue-attributed |
| 3b shared dir (+ `ls`) | mechanism GREEN (20,000 creates, one flip, ships ≡ served, handovers 0; `K + C + 3` tokens, 0 data-leaf reads) | OWED — the laptop's creates/s and `ls -l` wall SCOPING, venue-attributed |
| 3c foreign touch | mechanism GREEN (LIVE never moved, IDLE moved in 1–2 bursts; the PAUSED green VOID — its job never ran, §4.4aj) | OWED — the laptop's 5–8 ms handover wall SCOPING, venue-attributed |
| 4 kill matrix | **`sym-crash` 10/10 on nine consecutive from-zero runs (attempts 7–15) + 1/1 then 0/1 on the widened arm (finding 2, §4.4ag — a gate-4 precondition PR 13b clears: a user read failing `EIO` on the death path is a "refusals 0" violation); `sym-storm` ×10 NOT REACHED — 7 + 3 GREEN rounds from zero under `--venue=laptop`, stopped by finding 1 (§4.4af, an acked-writes LOSS — open); both counts restart from zero on PR 13b's binary** | n/a (LOCAL by the venue law; the flush-ceiling gauge venue-attributed) |
| 5 readers | mechanism GREEN (exactness, `recalls ≡ mutations × holders`, `fanout_p99 ≡ readers`, `reader_staleness_bound_ms` 0) on the 1-reader fleet | OWED (the 1 × 31 broadcast) — the laptop's recall RTT SCOPING, venue-attributed |
| 6 format cost | see §3.6 | n/a |
| 7 walls | mechanism GREEN (row (a) `shipped ≡ served ≥ displaced`; row (b) `/jobs` ships 7/7) — the flush-ceiling gauge's +1 readings are the ruling's named venue reading (§4.5) | OWED (N = 32) — the laptop's ≈ 1,000 frees/s and 2.2–3.2 s join wall SCOPING, venue-attributed |
| 8 SIM-1 | **MET** (§5) | n/a (tier (ii)) |
| 8b fidelity | PASS = 190 / FAIL = 0 from zero on a quiet box (fix round 1; §3.7) | n/a |
| 9 cloud (PR 15, the per-NODE law) | **run 1 (2026-09-24, §3.10): the instrument exercised on 8 REAL nodes — assembled on the second attempt, FAILED on its first row (F-C1 / F-C2 / F-C3 → PR 13i); the evidence lost with the dev machine** | **INCOMPLETE / UNMEASURED** — no per-node number exists; the re-run after PR 13i lands, with a NEW expressed owner approval for that run |

What PR 14 flips on: **finding 1 attributed and fixed with the storm ×10
GREEN from zero on its binary (§4.4af), defect 32's arm landed and
pinned** (the rung before the flip), then the box rows of gates 1 / 2 / 3 / 3b / 3c / 5 / 7
on THAT binary (§8's footprint procedure), plus §7's product items 1–2
(both counted declines today). Nothing found in this rung's thirty-five
FIXED defects is a class the design did not already state, and every
one is fixed red-first here (defects 10 and 13's pins landed in fix round
1); the one it did not fix is the class the design stated and the program
never built — the flip inherits exactly that item and the box's numbers.

> **Status (PR 13i, `fix/sym-shared-lun-coherence`, 2026-09-24 — the cloud row's findings; §3.10, §4.4ar–at): the decision stays NOT YET — the list gains one PRECONDITION and closes the cloud row's three defects.** *Landed, red-first:* **F-C1** (the flip's blocker of this row: a binary the flip ships must read a shared LUN coherently from every host — it does now, `O_DIRECT` metadata I/O with aligned forms, pinned on two kernels by the qemu/KVM fixture), **F-C2**, **F-C3**. *The precondition added:* **`sym-two-host` GREEN on the flip binary BEFORE the cloud re-run** (the fixture is the cloud row's shape on two kernels; no paid row launches without it) — and the cloud re-run itself stays owner-approved per launch. *What this rung does NOT move:* the box brackets (every rate row on the laptop is scoping — the ring's pad cost on serial-commit shapes is the new term the gate-1 bracket must read), gate 4's ×10 counts (restart from zero on the flip binary), the F-B1 tripwire's box verdict.

## 10. Verification on the final tree

**The rung's tree (`702ab502`'s code; docs commits after it)** —
`pr13-post15/`: `cargo fmt --check` 0 · `cargo clippy --all-targets
--all-features -- -D warnings` 0 · `cargo clippy --all-targets -- -D
warnings` 0 · `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps` 0 (two
private-item links found RED on the way and made plain code) · the fuzz
workspace type-check + fmt 0 · **`tests/run_sym_forest_suites.sh`: PASS,
40 suites flat THEN 40 stamped (33 m 11 s)** · the fidelity tier `full`
§3.7 (PASS 189 / FAIL 1, the FAIL attributed).

**Fix round 1's tree (`b8c4c92a`; the final code)** — every rail below
was RUN, none assumed (`/tmp/grok-justin/fix1-post3/SUMMARY` and its logs;
the gate lines ran on `4523f25e` and the matrix on `b8c4c92a`, whose one
difference is the six-line own-region exemption in
`foreign_slot_file_mutation_refusal` — the gate lines are re-stated for
it below):

* `cargo fmt --check` 0 · `cargo clippy --all-targets --all-features -- -D
  warnings` 0 · `cargo clippy --all-targets -- -D warnings` 0 ·
  `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps` 0 (one intra-doc link
  found RED on the way — `ForeignSlotFileMutation` unqualified in
  `mod.rs` — and qualified) · the fuzz workspace `cargo check --bins` +
  `fmt --check` 0 (`RUSTFLAGS=-D warnings`).
* **The three rails Issue 4 asked to be RUN:** `cargo test --all-features
  --test env_knob_convention_tests --test docs_parity_tests --test
  derivation_sweep_tests -- --test-threads=1` — **22 ok / 5 ok / 63 ok**,
  run twice (`fix1-rails.log` after the knob deletion, `fix1-post3/
  rails.log` on the final tree).
* **The touched suites, both legs** (`posix_errno_tests`,
  `meta_ship_tests`, `decoder_property_tests`,
  `sym_projection_refresh_tests`, `sym_n_daemon_tests`,
  `kv_freeze_wedge_tests`, `sym_cross_owner_tests`,
  `sym_slot_transfer_tests` — `--test-threads=1`): flat 8/8 ok (254 s),
  stamped 8/8 ok (149 s).
* **Every new pin RED-first** (each verified by neutering its fix and
  watching the pin fail, then restoring): Issue 6's forged reply (the
  screen bypassed), Issue 10's two (the re-seed / the striping exemption
  neutered), Issue 12's (the gate disabled — the door's EAGAIN surfaced),
  Issue 13's (the shipped-only arm), Issue 15's (the paired-end shape),
  the flip pin (the memo check removed — `meta_parent_scans` +1); Issue
  14's pin fails by construction on a re-take (a self-deadlock the
  bounded wait catches) and on a dropped guard (the mutex reads FREE
  while the refresh is parked).
* **The fidelity tier `full` from zero on a QUIET box** (Issue 19):
  **PASS = 190 / FAIL = 0** (19 m 58 s; §3.7).
* **`tests/run_sym_forest_suites.sh` — 41 suites flat THEN 41 stamped**:
  the first run on `4523f25e` read the FLAT leg RED at `sym_custody_tests`
  (5 of 27 — the Issue-12 gate judging the in-process two-holder model's
  declared region as foreign; fixed in `b8c4c92a`, the own-region
  exemption; 27/27 flat and stamped by hand), then the matrix re-ran on
  `b8c4c92a` alone on the box (`fix1-post3/matrix2.log`, 31 m): **the
  FLAT leg PASS — 41 suites** (every suite of the rung's 40 plus
  `kv_freeze_wedge_tests`); **the STAMPED leg HUNG at
  `sym_coherence_tests`** — the per-suite watchdog killed it at 600 s
  with the last test line `a_single_flight_fetch_loser_registers_before_
  it_rechecks_the_winner ...` (defect 21's pin, added in this rung; green
  in the rung's own 40/40 matrix), the same line the first matrix run's
  FLAT leg had stalled on while a standalone instance ran beside it. The
  suite run alone on either leg passes — flat 41/41 in 75 s (twice, once
  through the runner), stamped 41/41 in 75 s — and the pin alone passes
  in 2 s on both legs, so the hang is INTERMITTENT and in the SUITE
  ORDER: recorded as fix-round finding 3 (§4.4ah) — **since attributed
  and FIXED as the pin's own seam-schedule deadlock (§4.4ah; the pin
  alone hung 10 of 29 runs on this binary)**. The matrix
  verdict for this tree is therefore: flat PASS 41/41; stamped 22 suites
  PASS then a HUNG suite the runner stopped on (the 19 suites after it
  in the stamped order did not run in that pass; every one of them ran
  green stamped in the rung's own matrix, and the eight touched suites
  ran green stamped in this round's own leg). A third full pass was not
  run — the turn ended.
* **The fleet, from zero, as root** (`/tmp/grok-justin/fix1-fleet/`,
  `fix1-fleet2/`): batch 1 on `16408a2f` — `sym-scale` exit 0 (the widened
  deleted arm 0 / 3,000 ×2), `sym-crash --rounds=1` GREEN, `sym-storm
  --venue=laptop` 7/10 then round 8 (the flip walk — fixed); batch 2 on
  `7c62428b` — `sym-storm --venue=laptop` 3/10 then round 4 (finding 1,
  §4.4af — OPEN), `sym-crash --rounds=1` RED through the widened arm
  (finding 2, §4.4ag — OPEN), `sym-scale` exit 0 (0 / 3,000 ×2). The
  later fixes (`4523f25e`'s echo absorb, `b8c4c92a`'s exemption) change
  the interim refusal's classification only and did not re-run the
  fleet — the storm count is blocked on finding 1 either way. The fleet
  is torn down; nothing was placed on `squeeze-test`.

**Fix round 2's tree (`a8ce78eb` — the code; `34014306` docs after it)** —
every rail below was RUN (`/tmp/grok-justin/fix2-*.log`):

* `cargo fmt --check` 0 · `cargo clippy --all-targets --all-features -- -D
  warnings` 0 · `cargo clippy --all-targets -- -D warnings` 0 ·
  `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps` 0.
* **The three rails:** `derivation_sweep_tests` 63 ok ·
  `docs_parity_tests` 5 ok · `env_knob_convention_tests` 22 ok
  (`fix2-doc-rails.log`).
* **The touched pins, both legs** (`sym_n_daemon_tests foreign_slot` —
  the round-1 contract with its OPEN face + §4.4ai's chmod-shape pin,
  flat 4/4 and stamped 4/4; `posix_errno_tests` 14/14). **§4.4ai's pin
  RED first** on the `EOPNOTSUPP` tree with its exact message
  (`fix2-pin-red.log`: "errno 95 is in coreutils' is_ENOTSUP set —
  chmod(1)/chown(1) exit 0 and print nothing on it"), GREEN on the fix
  (`fix2-pin-green.log`).
* **The fidelity tier `quick` from zero on the final binary
  (`fix2-fidelity-quick2.log`): PASS = 124 / FAIL = 0** (2 m 28 s;
  `sym-join-ladder` 81/81 — the N = 3 leg's §4.4z/§4.4ai contract lines:
  joiner 2's `>>` into joiner 3's file fails AT THE OPEN with `EREMOTE`
  rc 1; its `chmod` refuses `EREMOTE` AND `chmod(1)` exits nonzero; the
  mode reads 644 at joiner 2, joiner 3 and the manager after it; both
  mounts read the six bytes; `foreign_file_mutation_refusals` = 2 at
  joiner 2). The previous run of the same leg on the `EOPNOTSUPP` binary
  (`fix2-fidelity-quick.log`, PASS = 122 / FAIL = 1) is §4.4ai's finding.
* **`tests/run_sym_forest_suites.sh` — PASS, 41 suites flat THEN 41
  stamped (33 m 45 s, `fix2-matrix.log`; no suite HUNG, no
  stamped/flat ratio at or above the 2.0 note)** — the first complete
  both-legs pass since the rung's own 40/40 matrix (fix round 1's
  stamped leg had hung at defect 21's pin, §4.4ah, fixed in `48fdf7d5`).
