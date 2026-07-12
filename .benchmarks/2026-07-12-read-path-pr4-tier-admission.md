# Read-path PR 4 — scan-resistant tier admission + O_DIRECT no-publish + dehydration flip (R1b)

**Design:** `docs/design-read-path.md` §5.3 / PR 4 entry. **Commits:** `bfd1656` (red) →
`b5f3d8d` (mechanism) → `1737fbf` + `3a5c576` (measured liveness triage). **Base:** dev @
`2e188b8`.

## What landed

Ghost table (2¹⁶ slots, fill-count epoch N=2¹⁵, two-epoch sliding match — OQ #2 default),
K=4 offset-lane stream classifier (coexists with the legacy prefetch cursor until PR 5),
O_DIRECT read-flag visibility through the vendored fuse3 (`fuse_read_in.flags` → both trait
surfaces + bridge; clean signature break, 34 call sites across 21 test files swept —
forward-only, no shim), the >256 KiB admission decision at the fill site (first-touch skip
+ ghost-record ⇒ hot probation; ghost-hit ⇒ protected hot insert + today's validated
publish; ≤256 KiB verbatim incl. the 256 KiB-block-size volume boundary pin), the
dehydration gate flip (probation-never-read victims dropped at the source and counted;
protected victims dehydrate via the validated non-owner publish; ≤256 KiB `read_lru`
population bit-identical), `SQUEEZEFS_READ_TIER_ADMISSION` (unrecognized ⇒ refuse loud) +
the hot-budget-0 ⇒ auto-`always` interaction, and the §Observability stats fields.

## The default decision (measured, honest — the headline of this note)

**`SQUEEZEFS_READ_TIER_ADMISSION` defaults to `always` (today's behavior verbatim) until
PR 5**, pinned by `default_admission_is_always_until_pr5`. Defaulting to `second-touch`
now is exactly the design's **R-5 evict-before-consume spiral**, measured live: the legacy
9-block fire-and-forget prefetcher × 8 streams overruns the 256 MiB hot budget, probation
fills evict before their own sub-reads, the refetches ghost-hit as spurious "second
touches" (**515 of 1,024 unique keys on ONE cold pass at t=2** —
`read_tier_admission_ghost_hits`), and the resulting publish storm + protected-victim
dehydration floods OOM-killed the 8 GiB-caged daemon on five consecutive row-2 attempts
(kill records: anon-rss ≈ 8.33 GiB). The design assigns the control (per-lane
resident-unconsumed accounting + AIMD) to PR 5; the default flips there.

**The tax kill is real and available today, opt-in** (`SQUEEZEFS_READ_TIER_ADMISSION=never`,
fresh format, 8 GiB cage, 2 runs):

| Ledger (16 GiB cold seq read, row 2) | PR 1 baseline | PR 4 `never` |
|---|---|---|
| Throughput | 787 MiB/s | **3,616 / 3,640 MiB/s (4.6×)** — 0.92× of same-session row 1 (3,953) |
| Device WRITES during the read (the tax) | **16.5 GiB** | **0 GiB** — collapse to zero ✓ |
| Device reads | 15.97 GiB (1.00×) | 29.8 GiB (1.86× — hot-evict refetch overshoot; `get_obj/unique > 1` documented, the PR 5 target) |
| Daemon | survives | survives (2/2; peak anon ≈ 6.1 GiB) |

## Default-mode (`always`) matrix — perf-neutral vs lineage, everything survives

| Row | PR 1 / PR 3 lineage | PR 4 default | Verdict |
|---|---|---|---|
| 1 fresh create | 3817–4282 | 3806 / 3953 / 4124 | flat ✓ |
| 2 cold seq read | 787 | 772 | flat ✓ (tier writes 16.9 GiB — by design under `always`) |
| 3 rand-4k read | 326–331 | 325 | flat ✓ |
| 4 overwrite | 649–869 band | 762 | flat ✓ |
| 5 rand-4k write | 63–146 band | 126/130 | in-band (state-noisy row) ✓ |
| warm-re-read | 16.6–17.6 GiB/s | 15.9–16.7 GiB/s | within 10% of PR 3 ✓ |
| **interleaved-scan variant** | PR 3: 12.1→10.0→11.7 GiB/s (no collapse) | **0.87→1.8→0.44 GiB/s** | **regressed vs PR 3 — recorded, attributed**: the liveness correction makes consumption non-promoting, so slice A's hot entries are probation and the concurrent scan's probation churn displaces them (PR 3's accidental scan-resistance came from consumption-promotion — the same promotion that OOM'd cages). PR 1 lineage (758→451 collapse) is roughly matched. PR 5's per-lane accounting owns restoring this without the OOM; the row stays in the gate set. |

## Liveness fixes landed while triaging (each independently measured)

1. **Single-flight registry `scc::HashIndex` → `scc::HashMap`** (documented deviation from
   the doc's "container stays" note): HashIndex defers value drops via epoch reclamation,
   and since R1a the value's broadcast ring owns the cohort's multi-MiB `FillResult` —
   dead flights retained gigabytes under cold streams. HashMap drops synchronously.
2. **Clock shard `scc::HashIndex` → `scc::HashMap`**: eviction now MOVES the multi-MiB
   value out; no epoch-deferred payload garbage (latent since PR 3; first exercised by a
   full-length cold stream through the hot tier).
3. **Non-promoting consumption** (`get_no_promote` on the fast path, single-flight loop
   head, waiter pre-recheck) + **source-drop of probation victims** (no channel parking:
   16,384 slots × 4 MiB victims was a 64 GiB exposure).
4. **jemalloc `_rjem_malloc_conf`: `background_thread:true,dirty_decay_ms:1000`** — 4 MiB
   churn at GiB/s rates retained multi-GiB freed-dirty pages for the default 10 s decay
   inside cages (dhat cross-check: system-allocator live peak ≤ 0.83 GiB for the same
   workload whose jemalloc RSS hit 8.3 GiB).

## Gates

- Cargo gate at tip: clippy `-D warnings` clean, fmt clean, **62/62 suites** serial, doc 0
  warnings, bench smoke ok. Loom: not required — ghost table + lanes + sticky bit are
  single-word Relaxed with no cross-word invariant (documented on the types); loom-models
  scope untouched.
- fstests QUICK (`SQUEEZEFS_FSTESTS_MEMMAX=8G`): **{003, 213}** — exactly the documented
  platform expected-fail set; **no 616/618 cage-OOM cascade on the final tip** (earlier
  PR 4 iterations reproduced the pre-existing cascade; the jemalloc decay bound also
  relieves the fstests cage). Trio + 074: generic/074, 075, 091, 616 **pass**.
- LTP: **174 PASS / 0 FAIL / 9 SKIPPED**.
- Suites: admission 8/8 (incl. the default pin), churn 2/2 (get_obj assertions
  byte-identical; visibility helpers generalized to `hot_or_tier_has`; phase H
  ghost-primed), hot 11/11, reused_key 6/6, staged_identity 7/7.

## Rails

Quiet-gated timed runs; Tctl 39–59 °C; 3.5 GHz cap untouched; 8 GiB cages (the OOMs were
in-cage by design — that is what the cage is for); `/mnt/squeezefs` untouched; nothing
pushed. Raw artifacts: `~/tmp/sqperf/results/p4*`, diag scripts `~/tmp/sqperf/diag*.sh`.
