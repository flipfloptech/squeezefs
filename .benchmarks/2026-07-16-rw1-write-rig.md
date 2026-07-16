# RW1 — rand-write attribution rig + red gates (2026-07-16)

**Charter**: PR RW1 of `docs/design-random-small-writes.md` — the write-path
attribution rig (sub-phases, per-site `block_lock_wait`, H2b stripe audit,
in-flight WRITE histogram), the §1.2 device-byte ledger with its standing-RED
G-RW2 gate, the Issue-19 mapping-form tripwire, and the FIND-L1-A sweep
harness (`tests/l1a_sweep.sh`). **No product behavior change** — the W1 patch
is PR RW2. This note is the program's measured §1.2 confirmation: the first
attributed per-op ledger and the L1-A sweep baseline every later PR quotes.

## Provenance

| | |
|---|---|
| Tree | `test/rw1-write-rig` (docs `89d2663`, tests-RED `69f2e9b`, rig `43b618a`, harness `bbf82ec`) off dev `04889b6`; release binary md5 `e15c14f7f0a31b6c73f9ce7b95caef8d` |
| Box | the phase-1 box (25 online CPUs, 109 GiB RAM, nvme0n1 1.9T, kernel 7.1.3-2-cachyos) |
| Rails | sandboxes under `/var/tmp/squeezefs_l1a` (never `/mnt/squeezefs` / `~/tmp/nvme`); daemons in `systemd-run --user --scope` 16 GiB memcg cages; on-rail rows `taskset -c 0-15`, off-rail rows full 25-CPU mask; 3-poll quiet gate + hard Tctl ≥ 88 °C pause (observed 51–69 °C, all rows `quiet`); kills by PID; elbencho 3.1-9 |
| Artifacts | `/var/tmp/squeezefs_l1a/artifacts/rw1-rand` (rand grid, 12 rows) + `…/rw1-seq` (seq grid, 48 rows): per-row elbencho output, `.stats` before/after, diskstats snaps, honesty lines |
| Measurement serialization | sole box owner for the session; no concurrent 📊 branches |

## TDD evidence

- **RED** (`69f2e9b`): behavioral contracts failed as declared (phase
  histograms/ledger/probe pin recorded nothing); rig-core units (mapping-form
  classifier, stripe-audit classification, in-flight histogram) green.
- **GREEN** (`43b618a`): full suite green ×2 (`--test-threads=1`; suites by
  name: write_through, writeback(+fencing_livelock), data_path_correctness,
  small_write_zero_copy, staging_budget all green). One first-run failure of
  `kv_smo_crash_completeness_tests::pending_free_at_cap_forced_cycle_…`
  attributed as a load-timing flake in an untouched subsystem: 5×/5×
  standalone green, green in both full-suite re-runs; RW1's diff touches no
  KV code. clippy `-D warnings` / fmt / doc / bench-smoke green every commit.
- **Standing-red handling (stated choice)**: the G-RW2 gate test
  `standing_red_g_rw2_device_byte_ledger_on_rand_write_shape` is
  `#[ignore = "standing-RED … flips green when PR RW2 lands"]` — the
  kv_scale nightly-case convention — so the per-commit gate stays green while
  the red gate runs by name in acceptance sessions
  (`cargo test --test rand_write_amp_tests -- --ignored --nocapture`).
  The threshold is G-RW2 verbatim (≤ 4× writes / ≤ 1× reads); RW2 removes
  the `#[ignore]` when it flips.
- **Disabled-cost pin** (`tests/rand_write_rig_off_tests.rs`): rig off ⇒
  zero recorded samples anywhere, `WriteInflight` no-op, stats JSON carries
  NO rig families (byte-identical M2 contract), always-on ledger still
  counts, storm data reads back byte-identical, gate memoized. Green.

## 1. The cargo-tier standing-RED gate (sandbox, 64 KiB blocks)

384-block striped fixture, 2 shuffled passes of aligned mid-block 4 KiB
overwrites + the parked drain (768 ops, 3 MiB user bytes):

```
read leg : spill_seed 8,454,144 (129 reads) | flush_seed 16,711,680 | write_path_seed 0   => 8.0x user
write leg: spill_staging 16,842,752 (257 puts) | drain 16,777,216 | flush 0 | teardown 0
           | wt_fallback 0 | durable(wb 0 / self 0 / esc 0) | write_through 0             => 10.7x user
drivers  : writeback_enqueued drain 256 / flush 0 / teardown 0 / wt_fallback 0
           | restage_churn 129 removes | revisits 384 | sibling_probes 768
per-op   : 76,544 B/op (block 65,536 B; scoreboard scale 4 MiB => ~12 MiB/op class)
R:W      : 1:1.34 counted, 1:2.00 with the enqueued durable leg
mapping-form sanity: block 0 mapping = "0" form = undecorated-2part
```

**RED as designed**: write 10.7× (gate ≤ 4×), read 8.0× (gate ≤ 1×) — the
§1.2 pipeline reproduced from counters at sandbox scale, buckets fully
attributed, foreground writeback enqueues **zero** (the Issue-4 correction,
assertable). The Issue-19 polarity tripwire is pinned green and permanent:
freshly-striped fixtures map **undecorated-2part** (a predicate demanding
`exact == true` would patch nothing).

## 2. The measured §1.2 confirmation (live mounts, scoreboard rand shape)

`tests/l1a_sweep.sh SHAPE=rand`, on-rail, 16 GiB dataset over 16 files,
`elbencho -w --rand -t {8,16} -b 4k --iodepth 16 --direct --timelimit 30`,
mb ∈ {12, 256} (verified landing: `transport_max_background` 12 vs 256),
rig armed, n=3. IOPS medians: mb12 t8=396 t16=335; mb256 t8=466 t16=406 —
the scoreboard's 354–397 loss family, reproduced.

Per-op ledger, the three t16/mb256 rows (diskstats adjudicate, counters
attribute):

| row | user | dev R | dev W | R:W | per-op | ledger-R coverage | ledger-W coverage |
|---|---|---|---|---|---|---|---|
| r1 | 55 MiB / 14,080 ops | 28.2 GiB (524×) | 78.0 GiB (1,453×) | 1:2.77 | **7.7 MiB** | **100 %** | 62 % |
| r2 | 46 MiB / 11,776 ops | 23.7 GiB (528×) | 65.4 GiB (1,456×) | 1:2.76 | **7.7 MiB** | **100 %** | 62 % |
| r3 | 47 MiB / 12,032 ops | 25.3 GiB (551×) | 67.9 GiB (1,480×) | 1:2.69 | **7.9 MiB** | 96 % | 62 % |

Bucket split (r1 representative; MiB):

- **Read leg 28,768 = 100 % of device R**: spill_seed 7,000 (1,750 reads —
  bucket 1) + flush_seed 14,388 (drain/self-flush exits) + write_path_seed
  7,380 (gap materializations — concurrent revisits mid-accumulation).
- **Write leg 49,388 = 62 % of device W**: spill_staging 16,780 (4,195 puts
  — bucket 1) + drain 2,768 + flush 1,468 + durable(writeback 4,180 /
  **self_flush 22,948** / escalation 1,188) + write_through 56.
- **Drivers**: `wt_fallback = 0` everywhere (the §1.2 Issue-4 correction
  confirmed LIVE — the foreground write path enqueues nothing);
  `parked_gate_waits ≈ 5.0–6.0 k`, `parked_gate_self_flushes ≈ 4.9–5.7 k`
  — in the 16 GiB cage the budget authority sits Red and **the parked-gate
  self-flush is the dominant durable driver** (bucket 3), with the parked
  drain (bucket 2) and re-stage churn (bucket 4: 3,381–4,110 removes,
  13.5–16.4 GiB discarded staged images) alongside. `upload_dma n=14` —
  write-through never fires on this shape, as modeled.

**The block-revisit discount, quantified** (the rig deliverable the design
demands stated honestly): revisits/ops ≈ 51–52 % (6,039–7,368 revisits) ⇒
measured **7.7–7.9 MiB/op vs the 12 MiB/op model — a −36 % discount**,
larger than the scoreboard row's +12–26 %-over-ops-model gloss because (a)
the revisit rate here is ~2.9 writes/block at 12–14 k ops/row and (b) the
Red parked-gate self-flush short-circuits a staging round-trip for gated
blocks (one direct upload instead of put+drain+upload). The ratio and the
flatness carry the attribution; the totals match diskstats.

**Ledger-W residual (FIND-RW1-A, informational)**: ~38 % of device write
bytes are unattributed by the per-put ledger — staged bytes reach the device
**more than once per put** (staging-ring mmap page writeback + same-key
segment recycling under 13–16 GiB/row churn) plus meta/journal cadence. The
rig makes this visible for the first time; RW2's G-RW2 measurement should
keep diskstats authoritative (as the gate text already does) and RW4's
extent records will collapse the churn term structurally.

**OQ2 answered** (in-flight WRITE histogram, t16 × iodepth 16, mb256): the
kernel dispatches deep concurrency — depth samples peak in the ≤64 (5,707)
and ≤256 (5,482) buckets: `FOPEN_PARALLEL_DIRECT_WRITES` + mb256 deliver
≈ t×iodepth in-flight WRITEs; W1's parallelism-not-batching throughput story
has its premise.

**Write sub-phase anatomy** (t16/mb256 r1; n = samples, ms = ms-class):
route_classify 14,560/583ms · checkout 14,560/**4,351ms** · sibling_remove
14,560/**7,409ms** (the H1 hop is ms-class on half the ops under storm) ·
merge_copy 14,560/8ms · seed_fetch 7,192/7,174ms · park_spill
14,546/**9,484ms** (the inline victim spill = the ACK latency) · staging_put
17,817/8,081ms. Sites: write_checkout 620ms-class/14,560 · flush_exit
251/1,632 · spill_victim 16,490 acquisitions, 0 waits (try_lock, by design).

## 3. FIND-L1-A sweep baseline — **the −25 % convoy does NOT reproduce on this tree**

`tests/l1a_sweep.sh` seq grid: t ∈ {8,12,13,16} × mb ∈ {12,256} × {on-rail,
off-rail}, n=3, fresh volume per (rail, mb), rig armed, mb landing verified.
Medians (MiB/s):

| rail | mb | t8 | t12 | t13 | t16 |
|---|---|---|---|---|---|
| on | 12 | 2,510 | 2,567 | 2,534 | 2,560 |
| on | 256 | 2,438 | 2,546 | 2,530 | **2,593** |
| off | 12 | 2,373 | 2,474 | 2,477 | 2,459 |
| off | 256 | 2,370 | 2,480 | 2,456 | 2,441 |

```
SIGNATURE rail=on  shape=seq t16/t8@mb256=1.06 mb256/mb12@t16=1.01 blw_ms_tail@t16=3 @t8=0 verdict=not-convoy-shaped
SIGNATURE rail=off shape=seq t16/t8@mb256=1.03 mb256/mb12@t16=0.99 blw_ms_tail@t16=3 @t8=0 verdict=not-convoy-shaped
SIGNATURE rail=on  shape=rand t16/t8@mb256=0.87 mb256/mb12@t16=1.21 blw_ms_tail@t16=505 @t8=386 verdict=not-convoy-shaped
```

Both L1 boundaries are absent on `04889b6`+RW1: t16 ≥ t8 at mb256 (1.03–1.06
vs the L1 report's −25 %), mb256 ≈ mb12 at t16 (0.99–1.01 vs 1,212–1,243 :
1,623–1,652), and the `block_lock_wait` ms-tail is 0–5 samples/row against
the L1 241-sample class — on-rail AND off-rail. The rand shape also fails the
signature: its t16 deficit (0.87) comes with an **inverted** mb boundary
(mb256 1.21× better — more admission helps the RMW pipeline) and a tail that
does not collapse at t8 (it tracks the RMW pipeline itself: flush_exit +
write_checkout sites).

**Disposition**: the convoy-shaped signature is defined, falsifiable — and it
falsified today. The G-RW1 deferral clause has **no live trigger** on this
tree. Between the L1 report (branch off `c56ec6a`, 2026-07-15) and dev
`04889b6` landed FIND-VS-B's staging-shard replace-headroom fix, FIND-VS-A,
and the SMO program — the leading suspects for the convoy's disappearance —
and the L1 isolation ran a different sandbox config (8/16 GiB cages with
explicit `--mem-budget`/`--disk-cache-size`; this harness mounts uncapped
budget in a 16 GiB cage, and its absolute band ~2.4–2.6 GiB/s sits ABOVE the
L1 mb12 band). **RW3's forensics must therefore start by attempting a
re-repro under the L1 report's exact cage/budget config** (the harness knobs
support it) before adjudicating H1–H4/H2b; if it stays unreproducible, RW3
closes as "fixed-by-interim + baseline re-established" citing this table, and
G-RW4's grid gate is already green. H-signals the rig did surface on the
rand shape, for whenever the tape is next read: the H1 sibling-remove hop at
ms-class on ~50 % of storm ops, `aligned_pool_misses` ≈ 3.0–4.0 k/row (H3),
and a ~40 % cross-key share in the contended-lock split (H2b watch item —
cross_key 246–349 vs same_key 290–564 at t16).

## 4. Deliverable inventory

- **Rig** (`SQUEEZEFS_OP_PROFILE=1`, M2 memoized zero-cost contract, pinned):
  `fuse_write_phase_ns` (9 phases), `block_lock_wait_by_site` (all 8 taker
  sites), `block_lock_stripe_audit` (cross-key/same-key + waiters-at-arrival
  + spill-victim skips), `fuse_write_inflight`; armed-only stats JSON.
- **Always-on §1.2 ledger**: spill seed/staging, driver-split staging puts +
  writeback requests (drain/flush/teardown/wt_fallback), durable-upload bytes
  (writeback/self-flush/escalation), re-stage churn, block revisits, H1
  sibling probes, H3 `aligned_pool_misses`.
- **Red gates**: G-RW2 standing-red (`#[ignore]`, runs in acceptance);
  green pins: bucket/driver reconciliation, probe-cost equality (H1, flips in
  RW3), mapping-form sanity (Issue 19, permanent), disabled-cost/byte-identity.
- **Harness**: `tests/l1a_sweep.sh` (t×mb×rail grid, seq + rand shapes,
  CONVOY-SHAPED signature emitter — G-RW1's deferral citation and G-RW4's
  acceptance instrument).
