# Random-small-write program — closing report (PR RW5, 2026-07-17)

**Charter**: PR RW5 of `docs/design-random-small-writes.md` — the program's
CLOSING PR: the one sanctioned full acceptance sweep (fstests `-g auto` +
full LTP + elbencho + unmount-kill/SMO/lz4 kill-9 soaks + loom + full cargo
gate), the closing 3-regime scoreboard with the G-RW3 mixed rand-R/W and
hybrid-io warm rows and the FIND-RW2-B measurement, final adjudication of
every program gate, and the doc flips (design → Implemented; AGENTS/README).
The program's product work is `89d2663..ca09568` on dev (RW1→RW2→RW3→RW3b→
RW4→FIND-RW4-A fix); this PR is measurement + docs only — the binary under
test is dev `ca09568` product code, byte-identical.

## The before → after story (one table)

| Axis | Before (inaugural scoreboard, 2026-07-15) | After (this close) |
|---|---|---|
| rand_write_4k, all regimes | **354–397 IOPS** (0.08–0.09× JuiceFS — the only genuine product loss) | **59,348 / 64,600 / 65,470 IOPS = 12.64× / 15.93× / 14.29× JuiceFS, W in every regime** (RW2 acceptance family 61.5–66.7 k — in-band) |
| Device cost per 4 KiB write (patch shape) | ~12 MiB/op (~850× R / ~1,700× W — whole-block RMW + inline spill + durable upload) | **4 KiB-class/op** (1 aligned DMA; live rows ≤ 3.2× W / ≤ 0.03× R; `patch_writes ≈ ops`, `patch_edge_rmw_reads = 0`) |
| Compressed-volume rand_write_4k | same ~2,500×-class RMW (measured base 687–785×, 411–501 IOPS) | **15–17× amp, 17.7–18.3 k IOPS** (RW4); **20–26×, 29–42 k** on fully-random payloads post-FIND-RW4-A |
| ≥13-writer O_DIRECT convoy (FIND-L1-A) | −25 % to −34 % at t16 mb256 (`squeezefs bench` instrument), 3 sessions unexplained | **dead** — def/mb12 = 1.006–1.026, t16/t8 ≥ 1.10, mechanism ledger zeroed (RW3b) |
| Incompressible data on lz4/zstd volumes | unreadable (frame expansion past the chunk; silent neighbor overflow) | round-trips (store-raw escape + headroom + geometry gate; FIND-RW4-A) |
| Scoreboard allowlist (`SQUEEZEFS_VS_ALLOW_LOSS`) | 5 rows (3 rand_write + 2 seq ACK-semantics) | **2 rows** (`R1.seq_write_1m,R3.seq_write_1m` — the ACK-semantics artifact pair; G-RW5) |

## Provenance

| | |
|---|---|
| Tree | `docs/rand-write-program-close` off dev `ca09568` (== origin/dev at session start); release binary md5 `e53739b6cecd9170fada585be87492fb`, built `taskset -c 0-15 CARGO_BUILD_JOBS=12` — **product code identical to dev `ca09568`** (docs-only branch; same md5 as the FIND-RW4-A acceptance binary) |
| Box | the phase-1 box (25 online CPUs @ 3.5 GHz cap, 109 GiB RAM, nvme0n1 1.9T PC SN8000S, kernel 7.1.3-2-cachyos). **Host incident mid-sweep** (§4 face 3): generic/650's CPU-hotplug storm tripped a firmware fault — 15 even-numbered cores refuse to re-online (`failed to report alive state`) until a reboot; the box ran 16–17 online CPUs from the sweep's generic/650 onward. Every timed acceptance row (scoreboard, mixed, hybrid) completed BEFORE the incident on the full box; post-incident suites are correctness-tiered (LTP/soaks/loom/gate exact-shape green) or annotated (elbencho §6) |
| Rails | sandboxes under `/var/tmp/squeezefs_rw5` + the harnesses' own (`/var/tmp/squeezefs_vs_juicefs`, fstests/LTP `/dev/shm` volumes, `/mnt/squeezefs_test|_scratch` harness mountpoints) — never `/mnt/squeezefs`, `/mnt/juicefs`, `~/tmp/nvme`; daemons in `systemd-run` memcg cages per harness; scoreboard rows `taskset -c 0-15` + 3-poll quiet gate + per-row honesty lines; kills by PID only; user's standing `/mnt/juicefs` mounts + redis containers untouched (the scoreboard manages its own sandboxed JuiceFS + redis-free sqlite3 meta) |
| Measurement serialization | sole box owner; sessions strictly serial: scoreboard → mixed/hybrid → fstests full → LTP → elbencho → soaks → loom + cargo gate; no cargo/rustc during any timed row |
| Artifacts (preserved) | `/var/tmp/squeezefs_rw5/` — `scoreboard/` (36 rows + counter/device evidence), `mixed/artifacts/` (mixed + hybrid + FIND-RW2-B rows), `fstests_full.log` + results archive, `ltp_full.log`, `elbencho_*.log`, `smo_soak_*`, `kill9_lz4/`, `unmount_kill_soak.log`, `loom.log`, `cargo_gate_*.log` |

## 1. Program summary (per-PR SHAs + gate verdicts)

| PR | Landed (dev) | What it delivered | Gate outcome |
|---|---|---|---|
| RW1 — rig + red gates | `89d2663..94b8a01` | write-path attribution rig, always-on device-byte ledger, G-RW2 standing-RED, Issue-19 mapping tripwire, `tests/l1a_sweep.sh` | §1.2 confirmed live (7.7–7.9 MiB/op, −36 % revisit discount); baseline artifact (`2026-07-16-rw1-write-rig.md`) |
| RW2 — W1 sole-owner patch | `f8d2c63..e0f9f39` | in-place aligned sub-block DMA, clone/patch SeqCst fence + composed loom model, lock-free staged probe, adjacency guard, CLI-clone refusal | **G-RW1 PASSED FINAL** (13.26×/15.04×/14.57×, 61.5–66.7 k); **G-RW2 PASSED** (1.19–3.19× W, ≤ 0.03× R live); allowlist −3 rows (`2026-07-17-rw2-sole-owner-patch.md`) |
| RW3 — FIND-L1-A forensics | `7b7a877..b1d7a28` | convoy convicted ALIVE + instrument-coupled; mechanism = write-through completion-trigger proxy (none of H1–H4/H2b); historical-pair proof | G-RW4 deliberately NOT closed (fix owed) (`2026-07-17-rw3-find-l1a-forensics.md`) |
| RW3b — coverage-trigger fix | `9d17383..a070836` | coverage-union write-through trigger; inline seed fetches deleted; covered flush-seed elision structural | **G-RW4 CLOSED** (def/mb12 1.026/1.006; ledger zeroed; both instruments) (`2026-07-17-rw3b-write-through-coverage-fix.md`) |
| RW4 — W2 extent overlay | `4766886..22d652a` | ExtentOverlay + byte budget + versioned staged extent records + batched fold + recovery/downgrade matrix + staged rider + FIND-RW2-A fix | **G-RW6 CLOSED** (15–17× ≤ 150×; fold_fill mean 80.2; hole soak green); G-RW3 spot rows unmoved (`2026-07-17-rw4-extent-overlay.md`) |
| FIND-RW4-A fix | `25918cb..ca09568` | store-raw frame escape + chunk headroom + widened read window + mount geometry gate | incompressible round-trip closed; G-RW6-shaped row without the crutch: 20–26×, 29–42 k IOPS (`2026-07-17-find-rw4a-incompressible-fix.md`) |
| RW5 — this PR | docs-only | the one sanctioned full sweep + closing scoreboard + FIND-RW2-B measurement + doc flips | **G-RW3 CLOSED, G-RW5 CLOSED** (this report) |

## 2. Closing scoreboard (G-RW5 gate run + G-RW3 final)

Full 3-regime × 6-workload `tests/run_vs_juicefs.sh` session, 20 min wall,
`SQUEEZEFS_VS_ALLOW_LOSS="R1.seq_write_1m,R3.seq_write_1m"` (the shrunk
ledger — exactly the two seq ACK-semantics rows), **exit 0 = GATE GREEN**.
Every one of the 36 rows ran `quiet` (Tctl 52.1–58.2 °C, zero DIRTY flags,
zero remounts, zero INVALID); R2 device-true rows counter-VERIFIED per row.
Provenance line (verbatim from `scoreboard.md`): sqz @ `ca09568`
(md5 `e53739b6…`) | jfs `1.5.0-dev+2026-07-14.292f44cd` (meta=sqlite3 —
same engine as the inaugural + RW2 sessions) | elbencho 3.1-9 | 4096 MiB
matched budgets | 16 GiB dataset | cages 16 G (R2 jfs 2 G).

| Row | Workload | JFS | SQZ | SQZ/JFS | Verdict |
|---|---|---:|---:|---:|:--:|
| R1.seq_write_1m | seq write 1M (MiB/s) | 10,737 | 4,459 | 0.42× | L (**allowed** — ACK-semantics artifact; SQZ device-true 4,422 MiB/s vs JFS device 2,145 MiB/s during the row) |
| R1.seq_read_1m | seq read 1M (MiB/s) | 6,609 | 6,649 | 1.01× | TIE |
| R1.rand_read_4k | rand read 4k (IOPS) | 120,468 | 231,733 | 1.92× | **W** |
| **R1.rand_write_4k** | rand write 4k (IOPS) | 4,696 | **59,348** | **12.64×** | **W** |
| R1.stat_storm | stat (files/s) | 118,494 | 338,109 | 2.85× | **W** |
| R1.del_storm | del (files/s) | 3,413 | 32,195 | 9.43× | **W** |
| R2.seq_write_1m | seq write 1M (MiB/s) | 2,103 | 4,437 | **2.11×** | **W** (the device-true class) |
| R2.seq_read_1m | seq read 1M (MiB/s) | 6,523 | 6,593 | 1.01× | TIE |
| R2.rand_read_4k | rand read 4k (IOPS) | 80,087 | 283,370 | **3.54×** | **W** (VERIFY=device-true-OK both sides; SQZ 1.00× amp: `device_true_readsΔ = 4,194,304` = ops) |
| **R2.rand_write_4k** | rand write 4k (IOPS) | 4,055 | **64,600** | **15.93×** | **W** |
| R2.stat_storm | stat (files/s) | 118,004 | 336,238 | 2.85× | **W** |
| R2.del_storm | del (files/s) | 3,586 | 32,851 | 9.16× | **W** |
| R3.seq_write_1m | seq write 1M (MiB/s) | 10,053 | 4,608 | 0.46× | L (**allowed** — same artifact; SQZ device-true 4,558 MiB/s) |
| R3.seq_read_1m | seq read 1M (MiB/s) | 6,562 | 6,594 | 1.00× | TIE |
| R3.rand_read_4k | rand read 4k (IOPS) | 126,257 | 246,406 | 1.95× | **W** |
| **R3.rand_write_4k** | rand write 4k (IOPS) | 4,582 | **65,470** | **14.29×** | **W** |
| R3.stat_storm | stat (files/s) | 119,329 | 337,411 | 2.83× | **W** |
| R3.del_storm | del (files/s) | 3,319 | 37,172 | **11.20×** | **W** |

**G-RW5 adjudication: PASSED / CLOSED.** The allowlist is exactly
`{R1.seq_write_1m, R3.seq_write_1m}` (pinned in the harness header since
RW2); every other row is W or TIE; the gate run exits 0 under precisely
that list. The three rand_write rows that left the ledger at RW2 hold the
W verdict at close (12.64–15.93×, RW2 band 13.26–15.04× — in-family).
RW6 (harness durability-leveled seq-write mode) remains unexercised by
choice — the two remaining rows are measurement-semantics debt, not
product debt (SqueezeFS puts 4.4–4.6 GiB/s on the device during those rows
vs JuiceFS's 2.1–3.2 GiB/s page-cache drain; the honest device-true
comparison is R2's 2.11× W).

**G-RW1 final confirmation**: W in every regime, 59.3–65.5 k IOPS —
12.6× above the ≥ 4.7 k gate floor, 6× above the 10 k stretch, 3× above
the §4 20 k arithmetic expectation. No < 10 k attribution clause owed.

**G-RW2 on the closing rows** (the Loss-2 protocol's own instruments):
R1/R2/R3 rand_write device evidence `dev_r/s = 24/466/483` (≈ 0–8 MiB/s
reads ≈ **0.00–0.03× user**), `dev_w = 429/309/304 MiB/s` vs ~232–256
MiB/s user ≈ **1.19–1.85× writes**; `get_objΔ = 24/25/25` per ~1.8–2.0 M-op
row. Ledger truth on the R3 row's `.stats` snaps: `patch_writes` ≈ row ops,
`patch_edge_rmw_reads = 0`. The 12 MiB/op pipeline stays dead at close.

**G-RW3 scoreboard clauses (zero regression)** vs the RW2 acceptance and
inaugural baselines: device-true seq-write class 4,437 MiB/s @ 2.11× W
(RW2: 4,278 @ 1.87×; inaugural: 4,388 @ 1.65×); seq_read TIE 1.00–1.01×
everywhere (unchanged); rand_read 1.92×/3.54×/1.95× W (RW2 1.96/2.83/2.14;
the R2 cell *rose* to 283 k — best recorded); stat 2.83–2.85× W (RW2
2.84–2.95×); del 9.16–11.20× W (RW2 9.47–10.95×). Every row W/TIE within
band or better; the two allowed rows keep their attributed ACK-semantics
class (0.42×/0.46× vs RW2's 0.40×/0.44×).

## 3. G-RW3 mixed rand-R/W + hybrid-io warm rows; FIND-RW2-B measured

Session `/var/tmp/squeezefs_rw5/rw5_mixed_hybrid_session.sh` (artifacts
`…/mixed/artifacts/20260717T075844Z` + supplementary `20260717T081011Z`):
passthrough volumes, `taskset 0-15`, systemd memcg cages, 3-poll quiet
gate, fresh volume per leg, per-row `.stats`+diskstats snaps, Tctl
50–54 °C every gate. The FIND-RW2-B A/B lever is the design's own:
`SQUEEZEFS_PATCH_MAX_BYTES=0` (patch off ⇒ folds/CoW mint fresh block
keys; patch on ⇒ keys stable across overwrites — the state-outliving
condition under test).

### 3a. Mixed rand-R/W (RW4-comparable: 8 G cage, 16×1 GiB, 8 readers f0–7 ∥ 8 writers f8–15, t8+t8 qd8, 30 s)

| leg | read IOPS | write IOPS | writer ledger |
|---|---:|---:|---|
| **default (patch ON)** | **197,220** | **49,331** | `patch_writes` 1,478,806 ≈ W ops (+0.08 % snapshot skew), `patch_edge_rmw_reads = 0`, `extent_parks = 0` |
| RW4 reference row | 168,002 | 48,117 | (8 G cage, same shape) |
| A/B: patch OFF | 47,538 | 25,815 | `extent_parks` 773,255 ≈ W ops, `fold_passes` 5,580, `get_obj` 1,001,296 — the fold pipeline's seed+upload device stream |

**G-RW3 mixed clause: PASSED, unmoved-or-better** — the default row beats
the RW4 acceptance row on both legs (+17 % read, +2.5 % write); no
starvation, no read-path tax. The A/B leg doubles as attribution: with the
patch disabled, the writers' fold traffic (1.0 M device reads in-row)
craters both legs to 47.5 k/25.8 k — **W1 is what protects the mixed
shape**, and W2's overlay (extent parks ≈ ops, zero 4 MiB buffers) is what
keeps patch-off writes from the old 12 MiB/op cliff.

### 3b. Hybrid-io warm-row recheck (768 MiB fitting set, 16 G cage, default mount — the 2026-07-15 acceptance row-(a) shape)

Each "row" is one full-coverage rand-read pass over the set (t16 qd16
O_DIRECT, sub-second at these speeds — the acceptance's pass-N protocol).

| pass | default leg | patch-off leg | supplementary default leg |
|---|---:|---:|---:|
| warm1 (cold+admission) | 382,352 | 379,249 | 369,303 |
| warm2 | 532,655 | 540,299 | 546,201 |
| **warm3 (steady)** | **545,436** | **557,471** | **558,159** |

**Warm-row recheck: PASSED** — warm3 at 545–558 k IOPS with the
steady-state pass serving entirely from tier (`read_odirect_tier_serves`
≈ 188 k = every op of the pass; `ranged_reads` 225–481 residue; device
≈ 0 r/s class) — at/above the 2026-07-15 acceptance's 536,193–536,983
row-(a) band. The `read_odirect_tier_serves` class is intact at close.

### 3c. FIND-RW2-B — the measurement (patched keys carry ghost/cooldown state across overwrites)

Sequence per leg: warm passes (above) → 10 s rand-write 4 KiB over the
same files (default: `patch_writes` 192,568 ≈ all writes, every patched
key purged from all 4 tiers but **key unchanged**; patch-off: folds mint
fresh keys) → immediate re-read passes. postA lands ~0.5 s after the
write burst (inside the ~32–64 s `EscalationCooldown` window); postB
after a 70 s sleep (window expired); postC/postD back-to-back after.

| pass | default (patch ON) | patch OFF | mechanism evidence (default leg) |
|---|---:|---:|---|
| postA (in-window) | 271,935 / 258,052 | **355,153** | escalations **7–9** vs the 192-block population — **suppressed** (cooldown entries from warm-up still hot on the *same* keys); 165.9–180.7 k ranged device reads = the intended device-true degraded mode |
| postB (post-window, +70 s) | 315,407 / 299,141 | 457,175 | escalations **139** ≈ population — the re-admission burst; the row pays the documented one-window admission tax (the hybrid acceptance's cold-pass class, 316–322 k) |
| postC (next pass) | **557,179** | — | escalations 0, `ranged_reads` 0, `get_obj` 0, tier serves 196,608 = every op — **fully converged, zero device traffic** |
| postD | 561,319 | — | flat at warm steady state |

**The measured FIND-RW2-B statement (bounded-impact, with numbers):**
the interaction is real and behaves exactly as RW2 §7 predicted. On a
purpose-built adversarial shape — a hybrid O_DIRECT re-read burst landing
within ~64 s of overwriting the *same* blocks — the patch path's stable
keys suppress tier re-admission for at most one cooldown window: the
in-window pass runs at **258–272 k IOPS (~47 % of warm)** where the CoW
path's fresh keys would give **~355 k (~63 %)** — a worst-case gap of
**~97 k IOPS (≈ 27 % of warm), lasting ≤ 64 s per key, once**. After the
window: one admission-tax pass (299–315 k — identical to the class every
cold hybrid warm-up already pays), then **full reconvergence to 557–561 k
with zero device traffic** — equal to the pre-write steady state and to
the patch-off leg's. Correctness is untouched throughout (all serves are
validated ranged device reads; the patch purge preceded them).
**Disposition: bounded, self-healing, no code change owed at close** —
and the scoreboard/mixed rows (§2, §3a) show no trace of it on
non-adversarial shapes. The one-line lever (cooldown/ghost reset inside
the patch's tier purge) stays on record for the read-path owner
(§10 residual 5) should a live workload ever present this shape at
damaging scale.

## 4. THE full sweep — fstests `-g auto`: 45 failed of 784, ZERO program-attributable

`sudo SQUEEZEFS_FSTESTS_MEMMAX=8G bash tests/run_fstests.sh` (the M11
conditions verbatim: 8G memcg rail, /dev/shm-backed 1G meta + 8G data per
fs, stock helpers), 3 h 56 m wall, exit 1 (failures present). Raw counts:
**45 fail / 560 notrun / 784 ran** vs M11's 46/560/784. Log
`/var/tmp/squeezefs_rw5/fstests_full.log`; classifier
`/var/tmp/squeezefs_rw5/classify_rw5.py` (the M11 v2 lineage — parses
`_check_dmesg`-only failures).

### Classification (every one of the 45, diffed against the M11 attributed inventory)

| Class | Count | Tests |
|---|---|---|
| **same-as-M11 (a) documented platform/FUSE-semantics** | **33** | 003 020 035 062 078 099 128 131 192 213 258 294 306 319 375 426 444 452 467 477 478 504 525 **590** 631 683 684 685 688 697 732 756 777 (590 = the 8 GiB-fallocate-vs-8 GiB-volume memcg/thin face, M11 class (c) — same signature, bucketed platform-side by the v3 classifier) |
| **same-as-M11 (b) pre-existing, A/B-verified** | **5** | 209 451 533 647 729 (the aio-dio + DIO-hole-pread families — the prior gates' A/B carries forward) |
| **same-as-M11 (c) harness/environment (8G-rail memcg)** | **3** | 529 568 751 (529/568 = the documented KV-core allocation-flood fingerprint on aged daemons; 751 = the fio-soak rail artifact) |
| **M11 (d) recurrences** | **0** | — (013 AND 014 now PASS in-sweep: the FIND-M11-A fix is verified at full-sweep scale, the exact incubator that bred it) |
| **new-vs-M11, attributed this session** | **4** | 464 618 650 651 — every one dispositioned below; **none attributable to the program's commits** |

**M11 failures now NOT failing** (cured or flake-absent this roll): 013,
014 (FIND-M11-A fixed), 126, 133 (M11's own A/B called them in-sweep
flakes — standalone 3/3 PASS both binaries then; absent this roll), 551
(aio-dio flaky-by-run both binaries — absent this roll).

### The four new-vs-M11 faces (fingerprints + attribution)

1. **generic/464 — pre-existing, A/B-verified on the pre-program binary
   (⇒ NOT a program regression; recorded as FIND-RW5-A).** Signature:
   3–7× `line 46: echo: write error: Input/output error` — appends
   returning EIO under 464's 16-proc delalloc/append/sync_range storm over
   200 files on the 500 MB-staging harness config (the ring is
   structurally oversubscribed by the shape); daemon log shows the
   staging ring refusing whole-image admits
   (`StorageFull: … cannot admit N bytes without destroying live staged
   entries`) with never-lossy custody held (dismount folds re-park loudly,
   `recovered at next mount`). Ran + PASSED at M11 (186 s) ⇒ fails-by-run,
   environment/state-coupled. **Targeted loop per the fix-loop
   discipline**: tip `ca09568` standalone FAIL 3/3 (in-sweep + r1 + r2,
   identical signature, bounded — test completes, volume stays live);
   pre-program `04889b6` (fresh worktree build, md5 `dd2edf3c…`) FAIL
   with the IDENTICAL EIO signature on leg 1, and leg 2 **wedged
   outright** — writes in flight 620 s+, `waiting = 56`, watchdog firing,
   recovered only by fusectl abort. **The tip strictly improves the
   shape** (bounded loud EIO, no wedge, custody preserved) and the EIO
   class pre-dates the program. Charter filed in §10 (residual 8):
   staged-write-storm ring-pressure EIO — the never-lossy write path
   should degrade to the durable-spill escalation on this arm instead of
   propagating StorageFull to the user write. QUICK grows generic/464
   with an expected-fail row (the standing rule: it caught a real —
   pre-existing — bug).
2. **generic/618 — host GPU-driver dmesg noise, not SqueezeFS.**
   `_check_dmesg` tripped on an **amdgpu display WARN**
   (`dc_dmub_srv_apply_idle_power_optimizations` — the box's known DRM
   warn class); zero SqueezeFS lines in the captured dmesg. 618's own
   test body passed (3 s). The QUICK-comment's documented 618 face
   (8G-rail tier-tail OOM flake) did not fire; this is a different,
   SqueezeFS-free face. Platform/host class.
3. **generic/650 — host firmware/cpuhp incident (platform), with a
   kernel-side fuse-over-uring interaction note.** 650 (fsstress under
   CPU offline/online cycling; PASSED at M11 in 252 s) wedged ~57 min in:
   the box's firmware began refusing to re-online cores
   (`smpboot: … CPUN failed to report alive state`, dozens of cores,
   10–20 s each, one online write stuck **in-kernel** in
   `cpuhp_bringup_ap` for minutes), and 33 FUSE requests stranded
   kernel-side while every armed over-uring queue idled in
   `io_cqring_wait` (the daemon was never handed the requests — the
   kernel's per-CPU fuse-uring dispatch under mass CPU-offline; no
   SqueezeFS transport code changed in this program). Intervention
   (recorded): forensics captured to
   `/var/tmp/squeezefs_rw5/incident_650/`, then fusectl **abort** of the
   wedged connection (the README supervisor-escalation mechanism) +
   SIGKILL of the post-abort lingering daemon by PID; the sweep resumed.
   **The box permanently lost half its cores to the firmware fault**
   (16/32 online for the sweep tail — every even core except 0 refuses
   `failed to report alive state` until a reboot; re-online retried ×3
   post-sweep, bounded). Sweep-tail results (652+) ran on 16–17 CPUs —
   the three (c)-class memcg faces and the platform set were unaffected
   (identical signatures to M11); no timed scoreboard/mixed/hybrid row is
   contaminated (all completed before the incident).
4. **generic/651 — pure collateral of the 650 intervention**: the aborted
   TEST daemon lingered through the next test's mount attempts
   (`mount.fuse.squeezefs: previous daemon … still running after 60s` ×3
   — the documented 452/732 drain-timeout class shape). Not a product
   failure.

### QUICK-provenance cross-check inside the full run

PASS: 001 008 **013** 069 074 075 091 112 127 263 285 469 616 617 ·
NOTRUN: 009 316 (fiemap canaries intact) · FAIL: 003 213 (documented
platform, diffs unchanged — 213 reproduced the exact expected-table
signature) · 618 = the amdgpu dmesg-noise face above (test body passed).
The RW4/FIND-RW4-A-era `FSTESTS_QUICK=1` runs had already reproduced the
expected table exactly on this program's binaries. Zero ZEROS-signature,
zero stale-fill-signature, zero 074-family recurrences anywhere in 784
tests; zero `corrupt KV encoding` lines in either harness daemon log.

## 5. Full LTP

`sudo bash tests/run_ltp_syscalls.sh` (stock: fresh /dev/shm-backed 1G+1G
volume, 500 MB staging, `--allow-other`), 7.8 min wall:
**PASS 174 / FAIL 0 / BROKEN 0 / SKIPPED 9** — the exact expected shape,
identical to the M11 and prior-gate tallies (incl. the historically-broken
writev03 staying green). Zero delta. Log
`/var/tmp/squeezefs_rw5/ltp_full.log`. (Ran post-incident on the
17-CPU-degraded box — LTP is correctness-tiered; the tally is
CPU-count-insensitive and matched exactly.)

## 6. elbencho (stock `tests/run_elbencho_mount.sh`) ×3 — parity-or-better

Stock harness (/dev/shm 128M meta + 2G data, 1 GiB over 4 threads, 4 MiB
blocks), first-done MiB/s:

| MiB/s | r1 | r2 | r3 | median | M11 rows (median) | K7 lineage |
|---|---|---|---|---|---|---|
| WRITE | 3,361 | 3,724 | 3,987 | **3,724** | 3,707–3,998 (~3,917) | 3,585 |
| READ | 33,096 | 33,010 | 38,292 | **33,096** | 31,171–33,988 (~32,231) | 27,263 |

WRITE parity-class with the M11 band (−5 % of its median **on a box
running half its cores** — the §4 incident left 17/32 CPUs online for
this row; K7 lineage still beaten), READ above the M11 median. Verdict:
**parity-or-better**, no regression signal. Log
`/var/tmp/squeezefs_rw5/elbencho_x3.log`.

## 7. Crash soaks (SMO acceptance shape + unmount-kill + lz4 extent-record round)

- **SMO acceptance soak** (`.agents/findvsa/smo_acceptance_soak.sh` —
  16-worker acked-create storm, kill -9 at peak, in-place remount, joint
  audit per round), the PR-5 mask split: **full mask 5/5 + on-rail
  (taskset 0-15) 5/5, all rounds jointly green** — acked-loss **0**
  (48,827–61,356 acked creates/round, 0 missing every round), mount
  refusals **0**, `meta_kv_replay_dropped_torn = [0,0,0,0]` every round
  (torn-zero shape), `pending_free = [0,0,0,0]` with no drain residue.
  The on-rail count was restarted from zero after a harness-side
  interruption of its first attempt (my own command timeout killed the
  wrapper mid-round-5; runs before the restart are not counted — the
  multi-run discipline). Logs `smo_soak_{full,rail}.log`.
- **Unmount-kill teardown soak** (`sudo tests/run_unmount_kill_soak.sh`,
  the K7-era SIGABRT regression check): **30/30 cycles clean** —
  0 coredumps, 0 dmesg SIGABRT lines, 0 daemon panic/abort lines.
- **lz4 extent-record kill-9 round** (RW4's storm shape:
  fio rand-4k compressible storm on an lz4 volume, SIGKILL at storm
  depth, remount, orphan detection + full read-back + drain; ROUNDS=1
  per the RW5 charter): **GREEN** — the remount forward-detected its
  orphan population loudly (`EXTENT RECORDS AT MOUNT: 155 recovered,
  0 discarded (stale fencing)` — the §5.2 honest-mechanism line), all 8
  files fully readable post-crash, `extent_records_torn_discarded = 0`,
  `extent_records_future_refused = 0`, and the final clean mount reported
  **0 orphan records** (the clean-unmount drain mandate held live).
  Artifacts `/var/tmp/squeezefs_rw5/kill9_lz4/artifacts/20260717T130502Z`.

## 8. Loom + full cargo gate (branch tip)

- **Loom** (`tests/run_loom.sh`): **27/27 models ok** — including the RW2
  composed two-word model
  (`patch_clone_composed_never_mutates_a_validated_pin`) and both
  incarnation fill models
  (`incarnation_validated_fill_never_serves_mid_patch_bytes` /
  `…_dead_bytes`). Log `/var/tmp/squeezefs_rw5/loom.log`.
- **Full cargo gate on the branch tip** (docs/harness-comment diff over
  dev `ca09568` — product code byte-identical):
  `cargo clippy --all-targets --all-features -- -D warnings` clean ·
  `cargo fmt --check` clean ·
  `cargo test --all-features -- --test-threads=1` **929 passed / 0 failed
  (98 targets, serial)** · `cargo doc --no-deps` 0 warnings ·
  `cargo bench --benches -- --test` ok. Log
  `/var/tmp/squeezefs_rw5/cargo_gate_tip.log`.

## 9. Gate adjudication — final form

| Gate | Text (abbreviated) | Final verdict | Closing evidence |
|---|---|---|---|
| **G-RW1** | rand_write_4k ≥ JuiceFS (W verdict) every regime; <10 k ⇒ attribution clause; convoy-shaped t16 ⇒ defer | **PASSED FINAL (closed at RW2, reconfirmed here)** — RW2: 13.26×/15.04×/14.57× (61.5–66.7 k); close: 12.64×/15.93×/14.29× (59.3–65.5 k). Stretch 10 k+ exceeded 6×; §4 20 k expectation exceeded 3×; no attribution clause owed; deferral clause was moot (not-convoy-shaped, RW2 §4 + RW3 forensics §5) | §2; RW2 §5 |
| **G-RW2** | patch-shape device amp ≤ 4× W / ≤ 1× R; `patch_writes ≈ ops`; `patch_edge_rmw_reads = 0` | **PASSED (closed at RW2, per-commit since)** — cargo tier 1.0× W / 0.0× R (4,096 B/op); live rows ≤ 3.2× W / ≤ 0.03× R; closing rows 1.19–1.85× W / ≤ 0.03× R with `get_objΔ ≤ 25` per 2 M-op row; tripwires exact at RW2, RW3b, RW4, and this close | §2; RW2 §1/§4 |
| **G-RW3** | zero regression: seq/rand read+write rows, stat/del, mixed rand-R/W, hybrid warm recheck, QUICK table, crash soaks, never-lossy, loom | **PASSED / CLOSED (this PR)** — closing scoreboard W/TIE everywhere outside the 2 allowed ACK rows; mixed 197 k R ∥ 49 k W (> RW4 row); hybrid warm3 545–558 k tier-served; QUICK expected table exact inside the sweep; LTP 174/0/0; SMO ×10 joint green + unmount-kill + lz4 kill-9 round green; loom 27/27; full serial cargo gate green | §2–§8 |
| **G-RW4** | FIND-L1-A closed: t16 mb256 within ±5 % of mb12; t16 ≥ 0.95× t8 | **PASSED (closed at RW3b)** — def/mb12 1.026 (off-rail) / 1.006 (on-rail); t16/t8 1.128/1.102; mechanism ledger zeroed (`get_obj = 0`, seed bytes 0, `write_through_blocks` = dataset blocks, ms-tail 951 → 9–36); elbencho fence not-convoy-shaped both masks | RW3b §2–§4 |
| **G-RW5** | allowlist = the two seq ACK rows only; rand rows deleted the day W1 lands | **PASSED / CLOSED (this PR)** — deleted at RW2 (`e0f9f39`), pinned in the harness header; closing gate run exits 0 under exactly `R1.seq_write_1m,R3.seq_write_1m` | §2 |
| **G-RW6** | patch-ineligible floor: compressed rand_write_4k ≤ 150× amp; fold_fill median ≥ 16; hole soak green | **PASSED (closed at RW4; strengthened by FIND-RW4-A fix)** — 15–17× combined (fold_fill mean 80.2, median bucket ≤ 128); hole soak 0.5× read amp; post-fix fully-random re-run 20–26× / 29–42 k IOPS with zero frame errors | RW4 §1; FIND-RW4-A §5 |

**Program verdict: CLOSED — all six gates green in final form.** The
scoreboard's only genuine product loss is dead in every regime, the
convoy is dead on both instruments, the patch-ineligible floor is ~10×
inside its gate, and the sweep/crash/loom fences hold at the program tip.

## 10. Residuals (all named, one board)

Everything the program leaves open, in one place. None gate the close;
each carries its owner-of-record note.

| # | Residual | Class | Standing disposition |
|---|---|---|---|
| 1 | **H1 sibling-hop** — the per-checkout `spawn_blocking` staged-sibling probe/remove (145 ms-class/8,192 tail on the RW3 slow tapes; still ≈ 2 hops/write under kernel-split streams) | perf, real-but-secondary (RW3 forensics §4; RW3b §8.1) | candidate fix = extend RW2's lock-free staged-existence probe to elide the hop on every striped write; **files behind its own measured row** — not folded into this close |
| 2 | **Bench-instrument aligned-buffer charter** — `squeezefs bench`'s O_DIRECT write phase delivers tokio-copied UNALIGNED buffers, under-reading seq O_DIRECT by the kernel split-write tax (~2 FUSE WRITEs / 1 MiB) | tool honesty (RW3 forensics §5; RW3b §8.2) | fix shape = aligned pwrite via `spawn_blocking` or `crate::uring_fs`; the daemon's correctness under split/reordered WRITEs is pinned regardless (`tests/write_through_coverage_tests.rs`); the instrument-alignment lesson is now standing AGENTS.md doctrine |
| 3 | **RW4 crash-window stale-duplicate record** — kill-9 in the µs window between a whole-image commit and its record retire leaves a stale-duplicate extent record; re-application is idempotent unless the same range is overwritten post-remount before any fold | bounded crash-window rider (RW4 §6) | bounded by the checkout/absorb gaps-only law (newer bytes always win in RAM) + the first fsync draining it; kill-9 soaks (RW4 10/10, this close §7) never tripped it |
| 4 | **`jobs.rs::BlockMove` sizing note** — copies raw stored images at caller-specified `len`; only test producers exist today (defrag is a stub) | forward constraint (FIND-RW4-A fix §7) | when a live producer appears it must size moves by the stored image (`bk:0:len` / chunk), not `block_size` — recorded so the owner inherits it |
| 5 | **FIND-RW2-B** — per-key ghost/`EscalationCooldown` state outlives patched overwrites (patch keeps keys stable; CoW minted fresh keys) | measured this close (§3) | **bounded and dispositioned no-code-change**: the measured statement + numbers in §3; the one-line lever (cooldown/ghost reset in the patch purge) stays on record for the read-path owner if a live workload ever presents the shape at damaging scale |
| 6 | **RW6 harness durability-leveled seq-write mode** — optional, harness-only | unexercised by choice (design §5.4/PR RW6) | the allowlist floor stays at the two ACK-semantics rows; RW6 remains free-floating and never blocks product gates |
| 7 | **Pre-existing full-sweep families** (aio-dio 209/451/551, DIO-hole 647/729, in-sweep flakes 126/133, KV-flood memcg faces, platform/FUSE-semantics set) | pre-program standing punch list (M11 inventory; re-verified unchanged in §4) | owned by their existing charters (metadata-closing residual board rows 6–7); not of this program |
| 8 | **FIND-RW5-A — staged-write-storm ring-pressure EIO (generic/464)**: under a staging ring structurally oversubscribed by live staged files (200 × ~4 MB vs a 500 MB budget), some whole-image staged replaces propagate `StorageFull` to the user write as EIO instead of taking the durable-spill escalation the sibling arm already has (`routing.rs:4499`); the pre-program binary fails the same class AND wedges (620 s+ in-flight writes, 56 waiters) where the tip fails bounded-loud | **pre-existing** (A/B on `04889b6`: identical EIO signature + a wedge the tip no longer has), first honest full-sweep exposure | **CHARTER LANDED 2026-07-21** (`fix/write-wedge-and-074`; evidence `.benchmarks/2026-07-21-wedge-and-074-fixes.md`): the never-lossy StorageFull escalation now covers every staged whole-image/fold arm (staged replace + `fold_rider_record` re-stage + `clone_file` staged dest — the "extend-replace leg" turned out to BE the fold-rider re-stage; other ring writes are never-lossy by construction), counted `staged_spill_escalations`; the storm additionally convicted and fixed a rebind-exhaustion face, a RELEASE lease-drop face, a duplicate-reclaim double-free, a merge dirty-authority violation, and an unconditional untracked-free arm (now refused-and-counted). generic/464 flipped to expected-PASS in `SQUEEZEFS_FSTESTS_QUICK`, counted ×10 fully green; pins `tests/rw5a_never_lossy_tests.rs`. Original charter text: extend the never-lossy StorageFull escalation to every staged whole-image/fold arm (candidates: the `fold_rider_record` stage_write propagation and the extend-replace leg). Tip-vs-pre tapes preserved under `/var/tmp/squeezefs_rw5/` + `/tmp/xfstests-dev/results/generic/464.*` |
| 9 | **Host platform incident (generic/650, this box)**: the HP ZBook's firmware began refusing CPU re-onlines mid-hotplug-storm (`CPUN failed to report alive state`; one online write stuck in-kernel in `cpuhp_bringup_ap` for minutes), stranding 33 FUSE requests kernel-side (fuse-over-uring per-CPU dispatch under mass offline — no daemon-side defect: every armed queue idled in `io_cqring_wait`) | host firmware/kernel class, not SqueezeFS (650 passed at M11 in 252 s; no transport code changed in this program) | box needs a reboot to restore the 15 dead cores; forensics `/var/tmp/squeezefs_rw5/incident_650/`; if 650 wedges again post-reboot, the kernel-side fuse-uring-under-hotplug interaction deserves an upstream-facing note — filed as observation, not a SqueezeFS charter |
