# zc write handler/worker fusion — the D16 fast-track (2026-08-07)

Branch `perf/zc-write-fusion` off dev tip `d10e5a22` (the D16 default-ON
flip). Charter: ruling **D16**'s second half — "flip, and fast-track
fusion" — kill the extraction task-hop that costs UN-shimmed kernel-lane
rand-4k writes ~20 % on zc-armed queues (the documented
`SQUEEZEFS_FUSE_ZC` caveat, `docs/operations.md` §Environment knobs;
zcws-10 measured 0.796× rand4k / 0.924× rand4kow at 99.5 % direct
engagement — `.benchmarks/2026-08-07-zc-bridge-cqe-wedge.md` §6.2).

Commits: `6a2b5c52` (red: the fusion contracts) → `148a4f1d` (green:
the fused lane) → this note + rigs.

---

## 1. The re-adjudication — what was structural, what never was

The prior verdict ("fusion priced and declined as structural —
SINGLE_ISSUER + per-ring bvec + ACK-after-DMA", zc-bridge-cqe-wedge §8)
was re-read against source (`fuse_over_uring.rs:3474` ring setup;
`zc.rs` slot addressing; the W1 store ordering contract). Verdict:

**Still structural (all three, verified):**
1. `IORING_SETUP_SINGLE_ISSUER` + `DEFER_TASKRUN` — only the ring's
   owning thread may submit; a handler thread cannot legally push the
   `WRITE_FIXED` itself (the counted submit-economy posture holds its
   slot).
2. The payload pages exist ONLY as THAT ring's sparse-table bvec
   (`io_buffer_register_bvec`, patches 0023/0024) — `buf_index`
   resolves per-ring; zc means there is no daemon VA to source from
   anywhere else. Equally: the kernel never copies in-paged payloads on
   a zc queue (`can_zero_copy_req` has no per-request opt-out), so the
   extraction bridge RING OP itself — one `WRITE_FIXED(slot → memfd)`
   per extracted WRITE — is irreducible from userspace.
3. ACK-after-DMA — the reply must follow the store's CQE (the W1 patch
   law); the wait itself cannot be deleted.

**Never structural: WHERE the handler future runs.** The prior verdict
priced *dispatching the DMA from the handler's thread*; it never priced
*moving the handler to the ring's thread*. A small write's handler poll
is bounded CPU (parse + locks + a ≤ceiling merge), so the queue worker
can poll it itself — the shim-iops reaper/drain fusion (the owning
thread consumes its own CQ) and the §5.5.1 inline warm serves are the
house patterns. Fused, the hop chain

  dispatch spawn → handler lane → `WorkerMsg::ZcStore/Extract` send +
  eventfd wake → worker pass → CQE → oneshot send → handler-lane wake →
  handler resumes → `WorkerMsg::Commit` + wake

collapses to: mint on the worker → poll inline → same-thread channel
send (drained by the SAME pass) → CQE resumes the future inline on the
next pass → commit drained same pass. **Zero cross-thread wakes per
fused op** (was ≥ 2 wakes + 2 schedules in the op's serialized window —
the term the zcws-8/10 brackets named). What REMAINS on the fused path
is exactly the structural residue: the bridge ring op (extraction
vehicle) or device DMA (direct vehicle) + its CQE wait, now served by
the worker's own `submit_and_wait` cadence.

## 2. The machinery (commit `148a4f1d`)

* **`crates/fuse3/src/raw/connection/fused.rs`** — `FusedLane`: a
  bounded per-drain-group-worker executor (slab + `Mutex<VecDeque>` run
  queue). Wakers publish → `WakeCoalescer::arm()` → eventfd — the SAME
  loom-verified producer protocol `submit_reply` rides; **no new
  lock-free core, no new loom model owed**. Worker pass order: eventfd
  drain → coalescer disarm → **fused drain** (producer-observable state
  scanned after disarm, per the wake_core law) → commit drain (so a
  poll's WorkerMsg sends are picked up by the same pass). Poll wrapped
  in `catch_unwind`: a panicked future drops and its `ReplyTx`
  drop-guard synthesizes — the TPC-lane blast radius, verbatim.
* **Delivery**: a HELD (hold-candidate) WRITE at/under the fusion
  ceiling mints its handler future straight onto the worker's lane; a
  small at-delivery extraction fuses at its completion CQE (lease
  already minted on the worker). Above the ceiling / lever off /
  dispatcher unregistered / lane at capacity → the classic dispatch,
  the last two counted as `fuse3_zc_write_fusion_demotions`.
* **Session**: `handle_write`'s body extracted (`write_handler_body`)
  and run from BOTH venues verbatim; `fused_write_future` replicates
  the dispatch prelude (header/`fuse_write_in` parse, FUSE-3g bounds,
  the held-length agreement) and is registered as the pool dispatcher
  at arm (Weak conn breaks the Arc cycle).
* **Ceiling**: `SQUEEZEFS_FUSE_ZC_FUSION_MAX`, default derived
  **payload/8** (128 KiB at the shipped 1 MiB geometry) — the
  hop-vs-inline-copy crossover (2 cross-thread wakes + 2 schedules ≈
  5–10 µs ≈ 50–100 KiB at DRAM-bandwidth merge), documented in the
  registry entry. Explicit wins verbatim. **Lever**:
  `SQUEEZEFS_FUSE_ZC_WRITE_FUSION=0` = the pre-campaign dispatch (the
  bracket's control).
* **Engagement**: `fuse3_zc_write_fusions`/`_bytes`/`_demotions` under
  `metrics` (pinned by `metrics_tests`); the vehicle ledgers
  (direct/extraction) keep counting at their consuming sites — fusion
  changes VENUE attribution, never vehicle attribution.

Constraints re-proven (red-first suite
`tests/fuse_zc_write_fusion_tests.rs`, live armed 7.1.6-1-cachyos-sqz,
root): engagement on both vehicles byte-exact (incl. the W1 direct DMA
from the fused venue), ceiling + explicit override, lever-off control,
and the **bounded-outcome composition** — a fused waiter's lost bridge
CQE (the zcws-9 drop seam) resolves through the deadline ladder on the
SAME worker that polls the future (no self-deadlock; cancels counted;
post-recovery serviceable). `transport_lease_overlong_tests` venue
pinned to `SQUEEZEFS_FUSE_ZC=0`: the suite instruments the
DELIVERY-time §5.4 lease, and on an armed mount a held WRITE stalls
before any lease exists — lever bracket proved the failure pre-existing
at the dev tip (FUSE_ZC=1 red with fusion on OR off; FUSE_ZC=0 green),
D16-flip fallout in the test's selection mechanism, not fusion.

## 3. Local acceptance (tcp devsub — the fabric-sensitive substrate)

Venue: nvmet-tcp devsub (4× nullb meta + 4× 8 GiB zram data,
localhost), 7.1.6-1-cachyos-sqz, 32 CPUs, `fs.fuse.max_pages_limit=256`
(payload 1 MiB ⇒ derived ceiling 128 KiB), release binary `148a4f1d`,
default-armed mounts. Instrument: fio 3.42 libaio `direct=1`, 60 s
sustained + 10 s ramp per row (the sustained-state law), medians of the
two legs per side, A-B-B-A. Rows sized to the 32 GiB devsub (16 jobs ×
256 MiB rand filesets vs the field rig's 32 × 1 GiB — deviation
stated). Rigs: `.benchmarks/rigs/2026-08-07-zc-write-fusion-rig.sh`
(fusion lever, both sides armed) +
`…-zcaxis-rig.sh` (the caveat's own axis: armed vs unarmed) + the
table script. P0 smoke (O_DIRECT+fsync md5 + cp+sync-file ×3) per leg;
bridge tripwires asserted flat per leg; engagement gates FATAL per row.

### 3.1 The fusion-lever bracket (F1 on / F2 off / F3 off / F4 on)

Engagement per armed fusion leg: rand rows `fusions` ≈ every op (≥ 95 %
gate), `demotions` 0 everywhere, vehicle split preserved (rand4kow
direct share 93–94 % of ops here; the fresh-fileset rand4k splits
~42/58 direct/extract as its mappings publish mid-row), seq/dur rows
`fusions` ≡ 0 (the ceiling law — 1 MiB writes never fuse), control legs
`fusions` ≡ 0, bridge tripwires flat 0 on every leg, P0 smoke green ×4.

```
row        leg     GB/s      IOPS   p50ms    p99ms  dCPU s   amp   fusions   directs   extract  dem
rand4k     F1     0.532    129995    0.43    17.17   714.6  1.00  10076417   4219081   5857336    0
rand4k     F2     0.453    110640    0.52    19.27   866.1  0.90         0   3185613   5575659    0
rand4k     F3     0.500    122163    0.47    17.17   883.5  0.94         0   3591235   5652796    0
rand4k     F4     0.538    131297    0.45    16.71   730.2  0.99  10036137   4285150   5750987    0

rand4kow   F1     0.509    124355    0.71     3.29   702.1  1.14   8983923   8416304    567619    0
rand4kow   F2     0.395     96455    0.87     4.82   801.2  1.14         0   6536116    419508    0
rand4kow   F3     0.533    130206    0.63     3.56   831.9  1.11         0   8561215    546959    0
rand4kow   F4     0.623    152045    0.57     2.74   735.8  1.12  10754819   9984717    770102    0

seqwr      F1     1.204      1148    0.26   367.00    34.2  1.28         0         0     82088    0
seqwr      F2     0.944       900    0.31   480.25    33.0  1.34         0         0     67147    0
seqwr      F3     1.211      1154    0.26   358.61    35.5  1.31         0         0     83830    0
seqwr      F4     0.991       944    0.27   446.69    30.6  1.33         0         0     69654    0

dur        F1     1.175      1120    0.36   329.25    40.6  1.24         0         0     81532    0
dur        F2     0.959       914    0.41   379.58    34.7  1.24         0         0     66893    0
dur        F3     1.211      1154    0.34   304.09    40.0  1.21         0         0     82176    0
dur        F4     1.213      1156    0.35   308.28    38.7  1.19         0         0     80966    0

A-vs-B (median of the two legs per side; brackets = both A/B pairings):
  rand4k     A=0.535 GB/s  B=0.477 GB/s  ratio=1.122x (brackets 1.175x / 1.075x)  dCPU/GB A=22.49s B=30.63s
  rand4kow   A=0.566 GB/s  B=0.464 GB/s  ratio=1.219x (brackets 1.289x / 1.168x)  dCPU/GB A=21.33s B=29.90s
  seqwr      A=1.097 GB/s  B=1.077 GB/s  ratio=1.019x (brackets 1.276x / 0.818x)  dCPU/GB A=0.49s B=0.54s
  dur        A=1.194 GB/s  B=1.085 GB/s  ratio=1.100x (brackets 1.225x / 1.001x)  dCPU/GB A=0.55s B=0.57s
```

**Fusion verdict (both sides armed — the attribution instrument):
rand4k +12.2 % (brackets 1.175×/1.075×), rand4kow +21.9 %
(1.289×/1.168×) — both brackets > 1 in both orders — at −27 %/−29 %
daemon CPU per byte; p50 improves on every rand row (0.52→0.43 ms /
0.87→0.71 ms class); seqwr 1.019× and dur 1.100× at median (sentinels:
the seq rows on this venue are reclaim-churn-noisy — p99 300–480 ms —
and their brackets straddle 1; fusions ≡ 0 on them, so fusion cannot be
their mover).**

### 3.2 The caveat-axis bracket (Z1 armed / Z2 unarmed / Z3 unarmed / Z4 armed)

Both armed legs fused (default lever); unarmed legs' zc/fusion ledgers
identically 0 (the control gate). Same instrument, same sizes.

```
row        leg     GB/s      IOPS   p50ms    p99ms  dCPU s   amp   fusions   directs   extract  dem
rand4k     Z1     0.560    136828    0.42    16.19   709.0  0.99  10524640   4649152   5875488    0
rand4k     Z2     0.513    125200    0.44    17.43   768.9  0.85         0         0         0    0
rand4k     Z3     0.539    131517    0.43    16.58   821.4  0.87         0         0         0    0
rand4k     Z4     0.463    112936    0.51    17.17   655.1  0.88   8745457   3021232   5724225    0

rand4kow   Z1     0.605    147812    0.58     2.80   701.3  1.13  10573064   9821549    751515    0
rand4kow   Z2     0.542    132249    0.54     4.23   912.0  1.10         0         0         0    0
rand4kow   Z3     0.533    130228    0.55     4.23   936.6  1.12         0         0         0    0
rand4kow   Z4     0.470    114654    0.65     5.28   537.7  1.06   7628214   7212477    415737    0

seqwr      Z1     1.210      1153    0.25   362.81    33.9  1.32         0         0     84688    0
seqwr      Z2     1.305      1244    0.20   325.06    31.2  1.27         0         0         0    0
seqwr      Z3     1.272      1213    0.21   333.45    30.8  1.27         0         0         0    0
seqwr      Z4     1.012       965    0.28   450.89    32.6  1.33         0         0     71036    0

dur        Z1     1.350      1287    0.37   278.92    42.1  1.20         0         0     90864    0
dur        Z2     1.298      1237    0.28   283.12    32.8  1.21         0         0         0    0
dur        Z3     0.907       865    0.33   408.94    24.9  1.25         0         0         0    0
dur        Z4     1.088      1037    0.40   333.45    36.4  1.19         0         0     72951    0

A-vs-B (median of the two legs per side; brackets = both A/B pairings):
  rand4k     A=0.512 GB/s  B=0.526 GB/s  ratio=0.973x (brackets 1.093x / 0.859x)  dCPU/GB A=22.33s B=25.20s
  rand4kow   A=0.538 GB/s  B=0.538 GB/s  ratio=1.000x (brackets 1.118x / 0.880x)  dCPU/GB A=19.19s B=28.65s
  seqwr      A=1.111 GB/s  B=1.288 GB/s  ratio=0.862x (brackets 0.927x / 0.795x)  dCPU/GB A=0.50s B=0.40s
  dur        A=1.219 GB/s  B=1.103 GB/s  ratio=1.105x (brackets 1.040x / 1.199x)  dCPU/GB A=0.54s B=0.43s
```

**Caveat-axis verdict: armed rand-4k = 0.973× unarmed at median
(brackets 1.093× fresh-store / 0.859× aged-store) and rand4kow = 1.000×
(1.118×/0.880×) — the ~20 % armed tax is GONE at median on this venue,
but the reversed bracket does not reproduce the win: the Z4 (most-aged)
leg decays on EVERY row including the never-fused seq rows (seqwr Z4
1.012 vs Z1 1.210 GB/s), i.e. the residual spread is store-aging noise
on a 32 GiB zram venue, not a fusion or zc term. Armed daemon CPU/GB is
19–22 s vs 25–29 s unarmed (−12 %/−33 %) — on a real-latency fabric the
armed side has strictly more headroom than this localhost row shows.**

Cross-check against the pre-fusion posture on the same venue: unfused
armed (the F-bracket's B side, 0.477/0.464 GB/s) vs unarmed (Z2/Z3,
0.526/0.538 GB/s) reads ≈ 0.90×/0.86× — the zcws-10 caveat class
reproduced locally — and fused armed recovers it to 0.973×/1.000×.

## 4. Verdict

* **The hop is dead where it was named**: with fusion on, the armed
  small-write path pays zero cross-thread wakes per op, proven three
  ways — the +12/+22 % same-arm A-B-B-A (both orders), the −27..29 %
  daemon CPU/byte, and the engagement ledger (fusions ≈ ops, demotions
  0, vehicle split unchanged).
* **The D16 caveat at LOCAL scope**: armed rand-4k reads 0.973× (hole) /
  1.000× (overwrite) of unarmed at median — the documented ~20 % tax is
  recovered at median on this venue; the aged-store bracket (0.859×)
  keeps the claim honest, and its mover is venue aging (it moves the
  never-fused seq rows equally), not the write vehicle.
* **The caveat SUNSET is a field decision** (rc-manifest §3f: the rule
  is written on the field bracket) — squeeze-test was busy at check
  time (live daemon, fresh deploy, active operator sessions), so the
  sunset row is specified below and the operations.md caveat text is
  left standing (its "until handler/worker fusion lands" clause now
  points at machinery that is BUILT, engaged by default, and
  lever-escapable).
* Sentinels: seqwr/dur at-or-above par at median with fusions ≡ 0 —
  fusion is structurally incapable of moving them (the ceiling law),
  and the D3.a commit-batch economics are untouched (fused commits ride
  the same channel + loop-bottom flush).

## 5. Field spec (squeeze-test was BUSY — the user's live daemon +
fresh deploy + active sessions at check time; per charter, local-only)

The D16-caveat sunset row, to run when the host is free:

1. Deploy a rocky8 build of the merged fusion commit (same
   `go-task build:rocky8` + checksum discipline as `68f10c0f`).
2. `.benchmarks/rigs/2026-08-06-zc-write-side-rig.sh` verbatim (W1 zc /
   W2 W3 control / W4 zc — A-B-B-A, medians of 3 where re-run), PLUS
   the per-row fusion columns (`fuse3_zc_write_fusions` deltas — a
   fusion-armed rand row is INVALID unless fusions ≈ ops).
3. Sunset rule (rc-manifest §3f, D16): **armed rand-4k write ≥ 0.97×
   unarmed** on BOTH rand rows (hole + overwrite). If it clears, update
   `docs/operations.md` §Environment knobs (the SQUEEZEFS_FUSE_ZC
   caveat sentence) + rc-manifest §3f D16 (caveat SUNSET, dated) in the
   landing commit. The fusion A/B lever
   (`SQUEEZEFS_FUSE_ZC_WRITE_FUSION=0`) attributes any residue.

## 6. Gates

* Red-first: `6a2b5c52` verified red (metrics pin FAILED; live armed
  engagement leg FAILED on the missing counter) → `148a4f1d` green.
* **×10 (consecutive, final binary `148a4f1d`, root, live armed
  7.1-sqz venue): ALL GREEN** across {fuse_zc_write_fusion (4 tests:
  both-vehicle engagement + direct-DMA-from-fused-venue byte-exact,
  ceiling + explicit override, lever-off control, lost-CQE ladder
  composition), zc_bridge_cqe_wedge, fsync_writeback_tail_loss (P0),
  fuse_zc_write, write_through_coverage, transport_lease_overlong} —
  60 suite executions, zero failures.
* Fork suite 160 green (incl. the 8 new fused-lane executor tests:
  run-to-completion, park/resume with the parked-worker eventfd arm,
  capacity demotion, panic containment, stale wakes, eligibility +
  ceiling derivation); root targeted suites green (metrics, env-knob
  convention incl. the two new registry entries, skip ledger,
  derivation sweep, wedge census, multi-queue).
* Both workspaces `clippy --all-targets -D warnings` (root also
  `--all-features`) + `fmt --check` clean; markdown link check PASS.
* No new loom model owed: the fused lane's cross-thread wake rides the
  EXISTING loom-verified wake_core producer protocol (publish → arm →
  eventfd; drain → disarm → scan order preserved — the fused drain runs
  after disarm, before the commit drain); the run queue itself is a
  mutex, not a lock-free core.
* Bench legs' P0: O_DIRECT+fsync md5 + `cp && sync file` ×3 green on
  every leg of both brackets (12 legs total incl. the rig smoke);
  bridge tripwires (cancels/lost) flat 0 on every leg; unmount clean,
  zero residue (all mounts torn down; devsub left healthy).
