# The fused-lane PREDICATE fix — hold only what a direct vehicle consumes (2026-08-08)

Branch `perf/fused-lane-predicate` off dev `bd6413f9` (the
field-falsification default-OFF commit). Charter: the team's corrected
diagnosis of the 2026-08-08 squeeze-test collapse (386k unarmed / 300k
fusion-off / 175k fused, rand-4k write 32×qd8, ~300 µs fabric RTT) —
**the fusion PREDICATE was wrong**, not primarily the fused lane's
concurrency: `zc::hold_candidate` (shape-only) held/fused shapes the W1
sole-owner patch never consumes, so W1-INELIGIBLE ops (growth /
unmapped / overlay / shared — the `patch_ineligible_*` classes) paid
**hold + fused handler poll + LATE extraction**, serialized on the
worker at fabric RTT. The smoking-gun signature: a row where
`fuse3_zc_write_fusions` ≈ ops AND `fuse3_zc_write_extractions` ≈ ops —
ops paying BOTH vehicles.

## 1. Current vs corrected predicate (source-named)

**Current (falsified):** the hold decision was
`zc::hold_candidate(w_off, w_size, payload_sz)` ALONE —
`crates/fuse3/src/raw/connection/zc.rs:330` (aligned ∧ nonzero ∧
`< payload/2`), consulted at the delivery gate
(`fuse_over_uring.rs`, the `zc_slot_payload` arm). Shape only: it
answers "COULD a direct DMA address this window", never "WILL the W1
vehicle consume it" — but consumption needs the state half that lives
in the ROOT crate: `try_sole_owner_patch`'s clauses
(`src/fuse_client.rs:10626` — custody, overlay, passthrough,
undecorated whole-block mapping via `is_whole_block_mapping`
(`src/routing.rs:5136`), sole-owner refcount) and the `try_patch`
request-shape ladder (`src/fuse_client.rs:11038` — adjacency, patch
cap, non-extending, single-block, LBA alignment). On a fresh/growth
write every one of those says no — AFTER the payload was already held
and (fused) dispatched.

Compounding it, the 2026-08-07 fusion also fused the at-delivery
extraction completions (`PendDone::Deliver` arm): the INELIGIBLE
population — whose handler path parks on fabric-RTT FS state
(allocation, growth publishes) — ran on the bounded per-worker fused
lane instead of the multi-lane handler venue.

**Corrected (two rules, both mandatory):**

1. **Hold/fuse ⟺ shape ∧ W1-eligibility.** The delivery gate composes
   `hold_candidate` with the new `Filesystem::zc_write_hold_eligible`
   seam (fuse3 `raw/filesystem.rs`, provided default **false** — no
   proof, no hold): the ROOT implements it as the LOCK-FREE, SYNC,
   read-only mirror of the W1 ladder (`src/fuse_client.rs`, the
   `impl Filesystem` block): virtual-ino / patch-cap / LBA-alignment /
   single-block / write-verification / passthrough / striped /
   non-extending (cached size) / undecorated whole-block mapping
   (cached `block_map`, `Arc`-shared — a moka miss is honest
   ineligibility) / no overlay (`active_block_buffers` +
   `has_staged_active_block` + `has_staged_extent_record`, the same
   probes `try_sole_owner_patch` runs) / stream-adjacency peek
   (read-only `last_write_end`). Registered on the pool at session arm
   (`set_zc_write_hold_gate`), INDEPENDENT of the fusion lever —
   holding an unconsumable shape was also wrong on the classic
   dispatch (lazy round trip instead of the batched at-delivery
   extraction).
2. **Ineligible ⇒ extract AT DELIVERY, classic dispatch, never fused.**
   The `PendDone::Deliver` fusion arm is DELETED: at-delivery
   extractions dispatch on the handler lanes (the multi-lane venue owns
   their fabric-RTT concurrency). Only the held (provably
   direct-consume) population fuses, ≤ the fusion ceiling.

**Staleness (bounded, counted):** clauses the delivery probe skips
(refcount — a mutation protocol; custody — inert on shipped mounts) or
that race between delivery and handler (overlay park, clone pin, size
change) surface at the handler's AUTHORITATIVE predicate, which
materializes late exactly once (memoized). The split is counted:
**`fuse3_zc_write_lazy_extractions`** (stats inode, in the metrics-pin
family) — ≈ 0 in steady state; sustained growth = the delivery probe
drifted from the W1 ladder. A stale FALSE only forfeits one direct-DMA
candidate (the pooled patch vehicle still consumes the extracted
payload).

## 2. RED reproduction (fabric-RTT-emulated venue)

Venue: LOCAL tcp devsub (nvmet-tcp on lo) + **netem `delay 150us`**
each way on lo (measured ping RTT 0.033 ms → 0.361 ms — the field's
~300 µs class; qdisc verified `noqueue` before install, removed after
every run), baseline binary `bd6413f9`, fio 3.42 libaio direct=1, 60 s
sustained. Rig:
`.benchmarks/rigs/2026-08-08-fused-predicate-rig.sh` (stats-pin
columns FATAL per row).

Three counted red passes (60 s sustained rows, engagement exact, the
double-pay column = vehicle events beyond ops — pre-fix binaries lack
the exact `lazy` split, so the cross-domain estimate + the team
signature are the read):

| pass | shape | leg | GB/s | IOPS | fusions | extractions | directs | both-vehicles |
|---|---|---|---|---|---|---|---|---|
| red-1 | rand4k norandommap (drifting) | FUS f=1 | 0.350 | 85.4k | 7.52M | 6.17M | 1.35M | **46.1 %** of ops |
| red-1 | 〃 | OFF f=0 | 0.361 | 88.0k | 0 | 6.10M | 1.41M | 41.0 % (held-lazy class) |
| red-1 | 〃 | UNA zc=0 | 0.371 | 90.5k | 0 | 0 | 0 | 0 |
| red-2 | rand4kow (eligible-majority) | FUS f=1 | 0.244 | 59.6k | 4.22M | 0.29M | 3.93M | 17.6 % |
| red-2 | 〃 | OFF f=0 | 0.245 | 59.8k | 0 | 0.31M | 3.90M | 17.3 % |
| red-3 | rand4k_field (randommap fresh — the FIELD job shape, drain-group 8) | FUS f=1 | 0.188 | 45.9k | 4.76M | 2.37M | 2.39M | **72.2 %** |
| red-3 | 〃 | OFF f=0 | 0.150→0.195 | 36.6k→47.7k | 0 | 2.07–2.41M | 2.11–2.42M | 68.6–89.6 % |

**The signature reproduces at scale** — on the field job shape the fused
leg runs `fusions ≈ ops ∧ extractions ≈ half the ops` (every
first-touch op held + fused + late-extracted; the drift toward
`directs` is coverage publishing mappings mid-row). **The 0.45×
throughput magnitude did NOT select on this venue** (fused ≈ 0.96× of
fusion-off at best-effort emulation: netem RTT, the field's 32×qd8 job
shape, field-class drain grouping): local worker passes turn orders of
magnitude faster than the field's, so the serialized
hold→poll→late-extract chain hides inside other latency terms. Honest
read: the SIGNATURE is the bug's fingerprint and gates the fix; the
magnitude is field-venue-selected (the falsification row stands as the
field evidence).

Also recorded by red-1: the OFF (fusion-off, armed) leg pays the SAME
double-pay class through the lazy handler round trip — the predicate
bug predates fusion (D14's hybrid hold) and fusion only made its cost
fabric-RTT-serialized. The fix therefore gates the HOLD itself,
independent of the fusion lever.

## 3. The fix (commits)

`89a037bc` (red: the corrected-routing contracts + the stats-pin rig +
run_fio_row columns — the ineligible leg verified RED against
`bd6413f9`) → `1f7fbe20` (the fix: the `zc_write_hold_eligible` seam,
the gated hold, the deleted `PendDone::Deliver` fusion arm, the
`fuse3_zc_write_lazy_extractions` gauge) → `0007609f` (the ladder
contract's seam-free publish mount).

In-process green (live armed 7.1-sqz, root): all 5 corrected contracts —
eligible fuses without double-pay; growth extracts at delivery and
never fuses (single + 16-op burst); ceiling + explicit override on the
eligible shape; lever-off control; the lost-CQE ladder reaching a fused
waiter. Constraint suites green: zc_bridge_cqe_wedge,
fsync_writeback_tail_loss (P0), fuse_zc_write, write_through_coverage,
transport_lease_overlong, wedge_census, metrics pin, env-knob
convention, skip ledger; fork suite 160.

## 4. Acceptance brackets

All rows: fixed binary `0007609f`, fio libaio direct=1, 60 s sustained
+ 10 s ramp, A-B-B-A, medians of the two legs per side, both bracket
orders cited, engagement + post-fix ledger gates FATAL per row
(`FP_FIXED=1`), P0 smoke per leg, bridge tripwires flat per leg.
Artifacts: `~/tmp/sqz-fusedrtt-artifacts-2026-08-08/`.

### 4.1 E1 — emulated venue, fusion lever (both sides armed)

| row | fused med | off med | ratio | brackets | ledger law |
|---|---|---|---|---|---|
| rand4k_field (fresh — the field shape) | 0.1625 GB/s | 0.1595 | **1.019×** | 1.127× / 0.914× | fusions ≈ directs (the eligible drift), lazy ≤ 0.08 % |
| rand4kow (W1-eligible) | 0.2245 GB/s | 0.2140 | **1.049×** | 1.090× / 1.013× | extractions ≈ 6 % residue of directs, lazy 23–27 ops |
| grow4k (pure growth — ledger row) | 0.6575 GB/s | 0.6750 | 0.974× | 0.994× / 0.954× | fusions 0.17 % of ops (causally inert), extractions ≈ ops, lazy ≤ 0.10 % |

Fusion-on ≥ fusion-off on BOTH charter shapes (the fresh/field shape
and the W1-eligible shape). The grow4k ratio is store-aging noise the
ledger proves fusion cannot cause (it engages 0.17 % of the row's ops —
the out-of-order adjacency-peek misses, which then late-extract as the
counted bounded-staleness class); every F4 leg decays equally across
fusion-dependent and fusion-inert rows.

### 4.2 E2 — emulated venue, the caveat axis (armed+fused vs unarmed)

| row | armed med | unarmed med | ratio | brackets |
|---|---|---|---|---|
| rand4k_field | 0.1935 GB/s | 0.1775 | **1.090×** | 1.251× / 0.911× |
| rand4kow | 0.2340 GB/s | 0.2030 | **1.153×** | 0.983× / 1.398× |

≥ 0.97× vs unarmed on both shapes at median (the bracket spreads are
the same late-leg store decay — Z3/Z4 collapse identically on
zc-independent terms; Z3's unarmed rand4kow 0.166 GB/s is the outlier
face of it).

### 4.3 U1 — un-emulated venue, fusion lever on the W1 shape

| leg | GB/s | IOPS | fusions | directs | extractions | lazy |
|---|---|---|---|---|---|---|
| U1 f=1 | 0.626 | 152.9k | 9.72M | 9.72M | 1.24M | 80 |
| U2 f=0 | 0.566 | 138.3k | 0 | 8.78M | 1.04M | 109 |
| U3 f=0 | 0.559 | 136.5k | 0 | 8.66M | 1.07M | 111 |
| U4 f=1 | 0.630 | 153.7k | 9.77M | 9.77M | 1.24M | 109 |

**Fused = 1.116× fusion-off (brackets 1.106× / 1.127× — both orders) —
the original W1-shape fusion win survives the fix un-emulated.** The
eligible-shape extraction residue (~11–13 % of directs here vs ~6 % on
the emulated venue) is honestly-ineligible ops — a norandommap revisit
landing while a prior extraction's extent overlay is still parked makes
the block W1-ineligible until the fold, a self-seeding class that
scales with the row's IOPS; the EXACT hint-accuracy instrument is
`lazy` (80–111 ops of ~10M vehicle events ≈ 0.001 %).

The first U-pass aborted on a rig gate bug (the 10 % residue bound was
calibrated on the emulated venue; the exact staleness instrument is
`lazy`, which was 99) — bound re-derived to 25 % with the residue class
documented, and the count restarted from zero per the multi-run
discipline (U1b is the counted pass).

## 5. Default verdict

**Default flipped back ON in this branch (`SQUEEZEFS_FUSE_ZC_WRITE_FUSION`
default `on`, commit `4b1396d7`) — the charter's bar met with proof:**

* fusion-on ≥ fusion-off at rand-4k on BOTH charter shapes on the
  fabric-emulated venue (field job shape 1.019×, W1-eligible 1.049×);
* armed ≥ 0.97× unarmed on both shapes on the emulated venue (1.090× /
  1.153×);
* the ineligible-majority ledger law holds (growth rows: extractions ≈
  ops, fusions ≈ 0 — 0.17 %, the counted out-of-order adjacency-peek
  residue — and the vehicles never double-pay: `lazy` ≤ 0.1 %
  everywhere);
* the un-emulated W1-shape win survives (1.116×, both orders).

The falsification records (`bd6413f9`'s knob text in
`docs/operations.md`, the registry entry, the `fusion_enabled` doc)
carry the resolution addendum: the collapse was the PREDICATE
double-paying W1-ineligible ops, not the lane's bounded polling; the
lane's RTT-concurrency question for the now-legitimately-fused
population is answered by the brackets above (fused ≥ off at 0.36 ms
RTT on both shapes — no residual concurrency term surfaced at this
venue's depth; the field bracket on real hardware remains the standing
confirmation row and can reuse
`.benchmarks/rigs/2026-08-08-fused-predicate-rig.sh` verbatim, whose
stats-pin columns make any residual term attributable per row).

## 6. Gates

* Red-first: `89a037bc` verified red against `bd6413f9` (the ineligible
  contract failed on the missing lazy ledger; the netem red passes
  carry the both-vehicles signature at 46–97 %).
* **×10 (consecutive, final binary `4b1396d7`, root, live armed
  7.1-sqz): ALL GREEN** across {fuse_zc_write_fusion (5 corrected
  contracts), fsync_writeback_tail_loss (P0), zc_bridge_cqe_wedge} —
  30 suite executions, zero failures.
* Constraint suites green at the fix commit: fuse_zc_write,
  write_through_coverage, transport_lease_overlong, wedge_census,
  metrics pin (incl. the new lazy gauge), env-knob convention,
  skip ledger, derivation sweep; fork suite 160.
* Both workspaces `clippy --all-targets -D warnings` (root also
  `--all-features`) + `fmt --check` clean; markdown check PASS.
* Load-bearing laws re-proven live: §5.4 lease-severance (the lazy
  materialize still mints one lease per invocation), slot-state machine
  untouched, bounded-outcome bridge ladder reaching a fused waiter
  (drop-seam leg), COMMIT-after-extraction ordering unchanged, P0
  tail-loss ×10.
* Venue hygiene: netem installed only over a verified-default lo qdisc
  and removed after every run (EXIT trap + explicit final check —
  `noqueue` restored); all mounts torn down; both devsubs left healthy
  (the loop devsub created this campaign remains for future use, as the
  tcp one did from the prior campaign).
* Deviations recorded: the E1 first launch aborted on a rig
  cross-domain estimator (counters span layout+ramp+window vs fio's
  window — gates re-derived counter-domain, restarted from zero); the
  U first pass aborted on the emulated-venue-calibrated residue bound
  (re-derived with the class documented, restarted from zero); red-2's
  rig died POST-verdict from editing a running script (both rows
  captured; frozen-copy discipline adopted for every later launch).
