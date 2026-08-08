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

U1-TABLE

## 5. Default verdict

DEFAULT

## 6. Gates

GATES
