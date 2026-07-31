# 2026-07-31 — Write wall: reclaim drain-rate lanes + pipeline residence decomposition

Branch `perf/write-wall` (off dev `c9921f1`, **unmerged — do not
merge/push without orchestrator review**; the orchestrator runs the
field protocol after these gates). The release-gating write-wall
campaign: the user's bar is 15–17 GB/s on the field cluster; today's
field numbers on `c9921f1` are **fresh 10.6 / rewrite 6.3 GB/s**.

Commits: red contracts `fb9328f` (drain-rate contracts 10–11 +
phase-residence contracts), conviction-1 fix `0eedb14` (parallel drain
lanes + inline-spill/command counters), conviction-2 instrument
`93af447` (always-on `write_pipeline_phase_ns`), bracket rig `cc2cb14`
+ analyzer fix `6c59b9c`, docs/evidence (this note).

## 1. Conviction 1 — rewrite wall: reclaim drain rate (MEASURED, field, c9921f1)

Mid-rewrite at 6.3–6.8 GB/s on the 4-node cluster:
`block_free_reclaim_queue_bytes` = **17,418,944,512** — exactly
4,153 × 4 MiB ≈ the 4,096-block queue cap — with **196,747 queued vs
589 worker batches**. At cap, the enqueue path's designed
inline-backpressure arm processes displacement discards SYNCHRONOUSLY
on the write path (conservation over latency — designed as the last
resort, engaged as the steady state): the single serial worker drains
~500 blocks/s while the rewrite displaces ~1,650/s. Meta plane free
(~500 w/s, 1.4 % util — the write-commit-economy campaign's win held),
daemon threads cool, devices aqu 2.5–3.

Read-only re-verification on the live field daemon (this campaign,
idle): `queued = discards = 238,471` cumulative vs `batches = 741` —
at 64 blocks/batch the worker drained ≤ 47,424, so **≥ 191 k of the
238 k displaced blocks' reclaims ran inline on the write path**, and
`c9921f1` had no counter that could say so (`sync_drains = 0`
throughout — the valve never engaged; the at-cap arm was invisible).

### The fix (`0eedb14`, `src/block_reclaim.rs`)

The worker fans each popped span across **demand-derived parallel
blocking-pool lanes**: group by device → sort by offset → chunk
CONSECUTIVE slices of the sorted order (adjacency runs survive into
each lane's coalescer) → one lane per `batch_blocks` of that device's
queued demand, capped at `SQUEEZEFS_RECLAIM_LANES_PER_DEV` (default 8,
clamp 1..=64). Width = Σ over backends of min(demand, cap) — backends ×
demand, never a fixed funnel; a shallow queue keeps the old single-lane
shape. Takes grow to a full fan-out's worth (`batch_blocks × lanes`),
so deep queues coalesce MORE adjacency than per-batch windows could.

**Invariants untouched:** pop = exactly-once ownership; the
`processing` reservation spans each lane's lifetime (valve/pending/
drain predicates exact); `finish_free` strictly after the device
reclaim (offsets non-reallocatable until reclaimed — the window law);
the fence check runs per lane (a fenced holder issues zero device
commands — contract 6 re-verified); per-block ledger counters
unchanged (`queued ≡ displaced ≡ discards`).

**New instruments:** `block_free_reclaim_inline_spills` (at-cap inline
engagement — **must stay 0**; the engagement instrument this campaign's
field conviction lacked) and `block_free_reclaim_commands` (device
commands issued; blocks ÷ commands = the live coalesce factor).

### Red-first contracts (`fb9328f` → green at `0eedb14`)

`tests/async_block_reclaim_tests.rs` contracts 10–11:

* **Contract 10 (drain rate):** a 256-block displacement storm paced at
  ~4× the SERIAL drain rate (deterministic `SQUEEZEFS_TEST_RECLAIM_
  STALL_MS=50` seam pricing each lane pass; batch 8, cap 96) must never
  cap the queue — zero inline spills — with conservation (per-block
  ledger, exactly-once, gauge convergence) unchanged. **Behavioral red
  proven:** pinning `SQUEEZEFS_RECLAIM_LANES_PER_DEV=1` (the old serial
  shape) fails with 11 inline spills; the default fan-out passes.
* **Contract 11 (coalescing survives the fan-out):** 64 contiguous
  displaced blocks in one accumulation window merge into ≤ 8 device
  commands while the per-block punch ledger stays exact (64).
* Kill-9 deep-queue soak re-run ×10 rounds green
  (`SQUEEZEFS_RECLAIM_CRASH_ROUNDS=10`); fence contracts 6a/6b green;
  sibling `block_free_reclaim_tests` ledger suite green.

## 2. Conviction 2 — fresh wall: ~13 ms/block unaccounted residence (INSTRUMENTED)

Fresh 10.6 GB/s with ~45 blocks in-pipe ⇒ ~17 ms residence/block
(Little's law); devices hold each block ~3–4 ms; threads cool; meta
free. ~13 ms/block hides between admission and completion — and the
meta hypothesis died twice this week, so this campaign ships the
INSTRUMENT first: numbers name the term, not another guess.

### The instrument (`93af447`, `src/fuse_client.rs`)

`write_pipeline_phase_ns` — an **always-on** ten-phase histogram family
(the `fuse_op_phase_ns` pattern, deliberately NOT
`SQUEEZEFS_OP_PROFILE`-gated: one `Instant` read + one relaxed
`fetch_add` per phase per 4 MiB-class block, and the field needs the
decomposition on production mounts without a remount-to-arm round
trip), surfaced ungated on the stats inode:

`admit_wait` (governor park) → `detach_lag` (tpc-lane scheduling) →
`lock_wait` (block stripe) → `crypto` → `allocate` (incl. ENOSPC-valve
engagements — reclaim leaking onto fresh paths shows HERE) → `dma` →
`publish` (conveyor wait + commit) → `displaced_free` (reclaim
enqueues — ~0 on fresh paths by design) → `inval_tail` → `total`
(admit → release, the Little's-law numerator).

Contracts (`tests/write_pipeline_phase_tests.rs`): exact ten-key family
present ungated on the stats inode; every phase records per pipelined
block through the real striped write path; phase-exact recording.

### Rig decomposition (dialed fabric-latency rig — see §3 substrate)

Medians of 3 A-reps, mean-ms per phase (bucket-midpoint estimator):

| phase | fresh | rewrite | sustained |
|---|---|---|---|
| admit_wait | 7.18 | 13.60 | 13.93 |
| detach_lag | 0.47 | 0.57 | 0.77 |
| lock_wait | 0.002 | 0.010 | 0.013 |
| crypto | 0.001 | 0.001 | 0.001 |
| allocate | 0.008 | 0.006 | 0.006 |
| dma | 7.42 | 9.90 | 11.43 |
| publish | 0.65 | 0.66 | 0.93 |
| displaced_free | 0.001 | 0.007 | 0.005 |
| inval_tail | 0.032 | 0.034 | 0.048 |
| **total** | **15.86** | **25.06** | **27.49** |

**On this venue residence is FULLY attributed:** in-pipe residence
(total − admit_wait) ≈ dma + ~1.2 ms of accounted small terms on every
row — no hidden 13 ms class exists here (the rig's device leg IS the
residence). `displaced_free` ≈ 0 confirms reclaim stays off the
fresh/rewrite ACK path with the new lanes. **The field's unattributed
~13 ms does not reproduce on this rig** — the field runs 4 data
namespaces on real 2×200GbE fabric at 10× this venue's bandwidth. The
kill step therefore needs the FIELD table: **a mid-campaign field
deploy of this branch is required for conviction 2** (the phase family
was verified absent on the live field daemon — read-only check,
journaled in `/scratch/tmp/agent_runs.log`). Once deployed, the same
bucket-midpoint table names the term; the candidates the field table
will adjudicate: completion→release bookkeeping (`inval_tail`),
detach-lane scheduling (`detach_lag`), per-ino publish serialization
(`publish`), admission-target starvation (`admit_wait` vs
`depth_target`), device leg (`dma`).

## 3. Rig proof (dialed nvmet-tcp rig, A-B-B-A vs c9921f1)

**Substrate (stated per the two-substrate rule):** the DIALED
fabric-latency rig on **nvmet-tcp** — data = configfs null_blk 24 GiB
`memory_backed=1, completion_nsec=235000, irqmode=2, discard=1` (the
field's ~235 µs RTT modeled at the device; fio psync qd1 randread clat
avg **253 µs** verified; BLKDISCARD verified end-to-end), meta = fast
null_blk 3 GiB with 256 MiB write-back cache; both exported over
nvmet-tcp on `127.0.0.1:54131` (TCP slice 54100–54199), `nvme connect
-i 8`. Recipe scripted at `/tmp/dialed_rig.sh` (the
async-block-reclaim §1 recipe). **Instrument:** elbencho 3.1-10
(dynamic), kernel FUSE path, `--direct`, 16 t × 512 MiB × 1 MiB blocks
(8 GiB/pass = 2,048 × 4 MiB blocks); fresh format per mount;
`tests/write_wall_rig.sh` (committed). **Ordering:** A-B-B-A + B-A —
A₁B₁ B₂A₂ B₃A₃, medians of 3. A = branch `6c59b9c` lineage (`93af447`
binary — later commits are harness-only), B = dev `c9921f1`.
**Thermal validity:** the external governor held 2.4 GHz from 13:00
through the entire bracket window (no frequency events in
`/tmp/thermal_governor.log` during the runs); box otherwise quiet (no
builds/suites concurrent). Raw CSVs + phase snapshots + qbytes samples:
`~/tmp/ww_bracket/`.

### 3.1 Write rows (MiB/s; medians of 3; per-bracket deltas)

| Row | A (tip) | B (c9921f1) | Δ med | (A₁,B₁) | (B₂,A₂) | (B₃,A₃) |
|---|---|---|---|---|---|---|
| fresh | **4,719** (4666/4860/4719) | 4,521 (4634/4521/4465) | **+4.4 %** | +0.7 % | +7.5 % | +5.7 % |
| rewrite | 3,132 (3943/3132/3015) | 3,316 (3435/3316/3139) | −5.5 % | +14.8 % | −5.5 % | −3.9 % |
| sustained 60 s rewrite | 3,153 (3502/3153/3076) | 3,207 (3317/3207/3084) | −1.7 % | +5.6 % | −1.7 % | −0.3 % |

* **Fresh: A ≥ B in BOTH bracket orders on every pair** (+4.4 % med).
* **Rewrite/sustained: parity within the venue's spread** — mixed
  per-bracket directions (+14.8/−5.5/−3.9; +5.6/−1.7/−0.3) with
  overlapping ranges. Expected shape: on THIS rig a discard is one
  ~250 µs localhost round-trip — 2,048 of them spread over 16 lanes ≈
  a ≤ 2 % term either way, so the venue cannot price the field's
  drain-rate win (same limitation the async-block-reclaim campaign
  recorded for its venue). The performance face of conviction 1 is
  carried by the deterministic contract-10 storm (red at 1 lane, green
  at default) and the field ledger; **the ledger identities below are
  the acceptance.**
* Amp = 0.999–1.002, `wareq` = 4,096 KiB (block-sized requests) on
  every write row, both binaries (standing amplification columns).

### 3.2 Conviction-1 engagement (A rows; the acceptance instrument)

| Row·rep | inline_spills | qbytes_max (cap = 16 GiB) | queued ≡ discards | commands | sync_drains |
|---|---|---|---|---|---|
| rewrite r1/r2/r3 | **0 / 0 / 0** | 388 / 520 / 148 MiB | 2,048 exact ×3 | 1,995 / 1,951 / 2,023 | 0 |
| sustained r1/r2/r3 | **0 / 0 / 0** | 1,232 / 1,608 / 936 MiB | 52,556 / 47,314 / 46,170 exact | 51,866 / 46,865 / 45,814 | 0 |

The queue never comes within 10× of the cap while displacing ~950
blocks/s for 60 s straight; **zero inline spills anywhere** (the
must-stay-0 tripwire, live); ledger identity `queued ≡ discards` exact
on every row; coalesce factor ≈ 1.02–1.03 (16-lane interleaved
displacement order carries little adjacency at this arrival — the
contract-11 pin proves the coalescer against contiguous runs).

### 3.3 Non-regression rows (medians of 3)

| Row | A | B | Δ | Verdict |
|---|---|---|---|---|
| rand-4k `--direct` write, prefilled, 30 s (IOPS) | 31,638 | 31,305 | +1.1 % | parity (W1 patch path; publishes correctly not engaged: `pub_blocks` 0 both) |
| seq read, prefilled (MiB/s) | 3,039 | 3,073 | −1.1 % | parity |

**write_matrix il-vs-kernel parity sweep** (full sweep, branch tip,
fio psync t16, engagement exact on every shim row — `ipc_ops_write`
accounts all ops): **18 pairs = 5 WIN / 12 PAR / 1 flagged row**. Wins:
rand-4k ×2 (1.28×/1.66×), rand-64k-buffered (1.14×), seq-4k ×2
(1.21×/1.34×). The flagged row (`seq-1m-odirect`, il/kern 0.896 vs the
0.90 band edge) was adjudicated with alternating one-row A/B samples on
the same rig: branch 0.896/0.887/0.888/0.971/0.954/0.930 (median
**0.925**) vs baseline c9921f1 0.916/0.980/0.910/1.009/0.916 (median
**0.916**) — a PRE-EXISTING band-edge venue row (kernel medians
themselves swing 3.3–5.1 k IOPS across runs on this thermally-governed
box; a 93 °C → 2.0 GHz governor event fired during the full sweep's
window), **not branch-attributed** (the branch median sits above the
baseline median). Raw: `~/tmp/ww_matrix*`.

## 4. Gates

All at the final code tip `6c59b9c` (clean tree; the two commits after
it are docs-only — this note + AGENTS.md counter-family updates):

- `cargo clippy --all-targets --all-features -- -D warnings` PASS;
  `cargo fmt --check` PASS.
- `cargo test --all-features -- --test-threads=1` **full suite from
  zero: PASS** (exit 0, 26.4 min on the throttled box; 1,605 tests
  across the workspace binaries, 0 failed). Campaign contracts
  included: `async_block_reclaim_tests` (15 — contracts 1–11 incl. the
  storm + coalescing pins and the kill-9 deep-queue soak),
  `write_pipeline_phase_tests` (3), `block_free_reclaim_tests` (6),
  `write_pipeline_tests` (full suite).
- `cargo doc --no-deps` builds; the 4 pre-existing intra-doc-link
  warnings on dev-tip surfaces this branch does not touch
  (`ipc_host.rs`, `ipc_service.rs` ×2, `AdmissionGovernor`) — none
  introduced by this campaign.
- `cargo bench --benches -- --test` bench smoke PASS (26/26 in the
  final bench binary; all benches one-iteration clean).
- **Loom:** no lock-free core changed (the SegQueue pop/`processing`
  reservation protocol is untouched — `dispatch_lanes` only
  re-partitions already-popped batches; `write_pipeline_core` /
  `ConveyorCore` untouched, verified by diff). The full existing model
  suite was re-run anyway: **52/52 pass** (`tests/run_loom.sh`,
  `--cfg loom`).
- **statfs ×10 loaded soak: 10/10 green** (real FUSE-over-io_uring
  mounts, 3 contracts/run; load = 8 CPU spinners + fsync-dd loop,
  85–89 s/run; counted-run discipline, zero aborts).
- **pjdfstests (full, from zero, final tip, run alone): PASS** — 238
  test files, 8,798 tests, Result: PASS (176 s wallclock).
- **full LTP (from zero, final tip, run alone): PASS** — 174 passed,
  0 failed, 0 broken, 9 skipped (TCONF — continue by design);
  fail-fast armed and never fired.
- Preload gate legs: NOT run per the tier table — this diff touches
  none of `crates/squeezefs-preload/`, `src/ipc_host.rs`,
  `src/ipc_service.rs`, or the fuse3 notify surface; the il surface
  was instead exercised end-to-end by the §3.3 write_matrix sweep
  (engagement exact on every shim row).
- Suite order (serialized heavy phases, no overlap with builds or the
  rig): cargo gate from zero → loom → statfs soak → pjdfstests → LTP;
  substrates existed only during §2/§3 measurement and were torn down
  to zero residue before the gate (`/tmp/dialed_rig.sh teardown`
  verified empty).

## 5. Projected field impact (PROJECTIONS — the orchestrator runs the field verdict)

* **Rewrite wall:** at the field's observed ~1,650 displaced blocks/s
  and ~500 blocks/s serial drain (~2 ms effective per serialized
  discard on the zram-lz4 targets), 8 lanes/device × 4 devices raise
  the drain ceiling to ≥ 8× the serial rate — the queue stops capping,
  the ≥ 80 % of reclaims that ran inline on the write path go
  background, and the rewrite stream stops paying the discard stream's
  latency. The counter-verifiable post-deploy checks:
  `block_free_reclaim_inline_spills = 0` under full rewrite load,
  `queue_bytes` never pinned at cap, `batches` scaling with `queued`.
* **Fresh wall:** conviction 2's kill step runs on the field's phase
  table (`write_pipeline_phase_ns` on the stats inode, always-on). The
  13 ms names itself in one sustained fresh window; whatever phase
  carries it is the next fix, red-first.

## 6. Iteration loop (post-merge; branch `perf/write-wall-2` off dev `f629b46`)

The orchestrator's **field verdict v2** (f629b46, single-order, dirty
sequence): fresh 8,824 (down from 10,616) / rewrite 7,017 (up from
6,347) / read 19,786; mid-rewrite `inline_spills = 11,265`,
`queue_bytes = 18.6 GB`, coalesce ≈ 1.05. Its fresh bracket ran minutes
after a 128 GiB `rm`; its phase-residence grep matched nothing because
the stats JSON is pretty-printed (python `json` is the verified capture
— every table below uses it).

### 6.1 Iteration-1 field measurement (f629b46, settled, hygiene-controlled)

Settle protocol: reclaim `queue_bytes → 0` ×3 before every row; every
row fill+order-labeled; 1 Hz gauge sampler per row. Instrument:
elbencho 3.1-10, kernel path, `--direct -t 32 -b 4m`, 16 × 8 GiB
(32,768 × 4 MiB blocks/pass); artifacts `~/tmp/ww2_iter1/`.

| Row (order) | MiB/s | Key gauges |
|---|---|---|
| rm 128 GiB + idle drain | — | 32,768 queued drained in **12.1 s** (≈ 2,700 cmd/s at width 32); 1,128 spills during the rm burst itself |
| fresh (settled, fill 0→51 %) | **12,275** | spills 0; **verdict-v2's 8,824 was the rm-storm artifact** |
| rewrite ×1 (settled) | 7,798 | arrival ~1,950/s vs drain ~1,700/s ⇒ queue → 15.3 GB, **7,749 spills** |
| sustained rewrite 60 s | 7,502 | 36,795 spills (33 % of displaced); queue pinned ~18 GB |
| read (dirty vs settled) | 16,740 / 16,431 | shape-consistent pair (this instrument ≠ the orchestrator's read row) |

**Phase tables (the conviction-2 instrument, field):** fresh total
19.4 ms = admit 3.3 + **dma 15.0** + publish 0.9 (+ ~0.1 bookkeeping) —
the "13 ms" lives in the DEVICE leg (fabric queueing above the 3–4 ms
service), not client bookkeeping. Rewrite total 51.7 ms = admit 3.5 +
dma 16.8 + **publish 28.8** + displaced_free 1.8 (spill poison — the
at-cap arm ran its 12–22 ms ioctl ON the tpc lane).

### 6.2 Width/depth experiments (env levers, same binary — the decisive negatives)

| Experiment | Result | Verdict |
|---|---|---|
| E1: idle rm-drain at `LANES_PER_DEV=32` (width 128) | 12.2 s — burst 5,900 cmd/s vs 2,700 at width 32 | idle drain scales sub-linearly with width |
| E2: rewrite at width 128 | 7,656 MiB/s; under-load drain ~1,600–1,800 cmd/s — **identical to width 32** | **the TARGET deallocate service is the under-load drain ceiling; client width buys nothing under load** |
| E3/E3b: fresh at pinned depth 96 vs governed | 12,030 vs 12,231; dma 32.3 ms vs 15.2 (2× latency, zero rate) | fresh is NOT depth-bound: devices at ~3.05 GB/s each, util ~100 %, svctm ~1.1 ms — **the per-device downstream write-service ceiling** (4 × 3.05 ≈ 12.2 GB/s) |

### 6.3 Iteration-1 fixes (commits `43b1c20` red / `d7f31c4` impl / `8ac446d` doc)

1. **Default in-place full-block overwrite** (`SQUEEZEFS_INPLACE_OVERWRITE`,
   the contract-9 brim machinery promoted to the default; same-key merge
   on the coalescing conveyor; W1 crash/concurrency class;
   clone-shared/transformed keep CoW — `tests/inplace_overwrite_tests.rs`).
2. **Reclaim manners — the deferred-drain law** (foreground device-byte
   movement + below-cap ⇒ zero device commands; at-cap ⇒ drain
   regardless; idle ⇒ full-width catch-up; contracts 12–13).
3. **Park-don't-spill** (at-cap enqueue parks bounded, NEVER issues
   device commands from the enqueue context; `cap_parks`/`cap_overflow`
   replace `inline_spills`; lanes default 8 → 32, idle-engaged only).

### 6.4 Iteration-1 field verdict (A-B-B-A ×3, settled rows, A = 8ac446d, B = f629b46)

| Row | A med | B med | Δ |
|---|---|---|---|
| fresh (settled) | 12,106 | 11,959 | +1.2 % |
| rewrite | 6,054 | 7,604 | **−20.4 %** |
| sustained 60 s | 6,043 | 7,425 | −18.6 % |
| read (settled) | 16,300 | 16,333 | −0.2 % |

Engagement PERFECT on every A row (`inplace_ow ≡ wt_blocks` 32,768,
queued 0, parks 0, queue 0) — and rewrite went DOWN 20 %. Phase tables
name it: A rewrite dma 14.0 ms / publish 15.3 vs B dma 15.2 / publish
20.0 — the client got FASTER per phase at lower throughput, i.e. the
loss is downstream: **on zram-lz4 targets an in-place slot-replace
write costs ≈ 2× a fresh-slot write** (A rewrite 6,054 ≈ fresh 12,106 ÷
2, exactly; B's CoW writes fresh slots at fresh speed and pays dealloc
LATER, deferred off the row). The dealloc work is conserved on this
substrate — in-place just moves it inline into every target write.
**Iteration-2 consequence: the in-place default flips to opt-in**
(`SQUEEZEFS_INPLACE_OVERWRITE=1` for substrates where in-place rewrite
is cheap — real-SSD DSM fleets); CoW + deferred discard + manners +
park is the right default HERE.

### 6.5 Iteration-2 field verdict (A′-B-B-A′ ×3; A′ = 8ac446d + `INPLACE=0` ≡ the final tip `57959a3` defaults, B = f629b46)

Same hygiene; each leg adds the **dirty-fresh row** (prefill → rm
128 GiB → fresh IMMEDIATELY, no settle — the verdict-v2 artifact venue
and the manners acceptance row). Fill 51 % labeled on every non-fresh
row; artifacts `/scratch/tmp/ww2_verdict2/` + `~/tmp/ww2_verdict2.csv`.

| Row | A′ med | B med | Δ | Reading |
|---|---|---|---|---|
| **dirty-fresh** | **6,444** (6965/6436/6444) | 5,431 (5352/7166/5431) | **+18.7 %** | THE manners row: A′ defers the 32 k-block backlog (cmds ≈ 18 k of 22–24 k queued land AFTER the row); B drains concurrently into the foreground |
| rewrite ×1 | 4,890 (7226/4890/4659) | 5,706 (5706/7048/5136) | −14.3 % | NOT adjudicable: both binaries swing 4.6–7.2 GB/s leg-to-leg (per-bracket +27 %/−31 %/−9 % — mixed) — the aging 51 %-fill store dominates single-pass rows |
| **sustained 60 s** | 5,036 (5190/5017/5036) | 5,092 (5935/4681/5092) | **−1.1 %** | the converged rewrite verdict: PARITY (overlapping spreads) |
| read (settled) | 15,478 | 15,655 | −1.1 % | parity — no read regression |

A′ engagement: `cap_parks` 27–2,684/row with `cap_overflow` 0–201
(bounded, RAM-safe — the park law working as designed; B has no
counters and no parks, its at-cap arm spills inline). Note the store
HYSTERESIS across verdicts (B's own sustained fell 7.4 → 5.1 GB/s
between verdict-1 and verdict-2 sequences at identical binaries/fill):
single-order cross-sequence comparisons are invalid on this store —
only within-sequence A-B-B-A rows are cited.

### 6.6 The bar, adjudicated (named structural limits on THIS substrate)

The user's bar — fresh AND rewrite ≥ 15 GB/s sustained, reads ≥ the
19.8–20.2 floor — is **not reachable client-side on this cluster's
zram-lz4 targets**, with three independent measurements naming the
walls:

1. **Fresh ≈ 12.1–12.3 GB/s ceiling** = 4 × ~3.05 GB/s per-device
   downstream write service (util ~100 %, svctm ~1.1 ms; pinned depth
   96 vs governed ~50: rate IDENTICAL, dma latency 15 → 32 ms — the
   textbook saturated-server signature). More/faster data namespaces or
   target-side write path work is the only move.
2. **Rewrite ≈ 5–7.6 GB/s** — target-side work is CONSERVED both ways:
   CoW pays the deallocate stream (target service ~2,700 cmd/s idle /
   ~1,700 under load at ANY client width — §6.2), in-place pays the
   ~2× slot-replace write (§6.4). No client-side scheduling can beat a
   conserved target cost.
3. **Reads 19.8–20.25** (the orchestrator's floor instrument) — A′ at
   parity on every read row here (−0.2 % / −1.1 % on this note's
   instrument); the floor is not regressed.

What the iteration loop DID deliver on this substrate: the dirty-fresh
+18.7 % (manners), the verdict-v2 fresh artifact eliminated (settled
fresh 12.1–12.3 both binaries), the write-path spill poison
structurally removed (park-don't-spill), the phase instrument that
names residence on live mounts, and the opt-in in-place lever for
substrates whose rewrite economics invert.

### 6.7 Iteration-loop gates (final tip `e423072`)

Iteration commits: `43b1c20` (red contracts) → `d7f31c4` (in-place +
manners + park) → `eb8301e`/`5014423` (CoW-venue pins) → `8ac446d`
(doc-link) → `57959a3` (default flip, iteration-2 verdict) → `d7e24a5`
(fsck C6 in-flight exemption — the manners law legitimately widens the
begin_free limbo, caught red in-tree) → `e423072` (recovery-fixture
reclaim rendezvous). Field deploys used the containerized rocky8 build
(`go-task build:rocky8`, glibc ≤ 2.28 verified, KD-7 pair) under temp
names on `/scratch/tmp`; the dev binary was restored after every
bracket; every action journaled in `/scratch/tmp/agent_runs.log`.

- clippy `-D warnings` PASS; fmt PASS (final tip).
- `cargo test --all-features -- --test-threads=1` **full suite from
  zero: PASS** (exit 0; counted restart after each fix per the
  discipline — the two interaction failures the sweep caught red,
  fsck C6 and the recovery snapshot race, are §6.3's manners law
  meeting pre-existing settle assumptions, both fixed above).
- doc: the 4 pre-existing lib warnings only; bench smoke PASS; loom
  52/52 (no lock-free core changed — the SegQueue pop/`processing`
  protocol is untouched; manners/park are plain async + atomics).
- **pjdfstests (final tip, from zero, alone): PASS** (238 files /
  8,798 tests). **Full LTP (final tip, from zero, alone): PASS**
  (174/0/0, 9 TCONF).

## 7. Open questions

* ~~OQ-1: field deploy for conviction-2's table~~ — DONE (iteration
  loop, §6.1): fresh residence is the device leg (dma 15 ms), fully
  attributed; the phase family is live on production mounts.
* ~~OQ-3: per-command deallocate cost~~ — MEASURED (§6.1/§6.2): ~12 ms
  effective at width 32 idle, target-service-bound (~2,700 cmd/s idle /
  ~1,700 under load at any width); lanes default now 32 (idle-engaged
  under the manners law).
* **OQ-2 (standing):** the overwrite `fuse_ops` doubling
  (async-block-reclaim OQ-1) — unchanged, still a metadata-economy
  candidate, though on this substrate it is shadowed by the target
  walls (§6.6).
* **OQ-4 (new):** the 15 GB/s bar on THIS cluster needs substrate work
  (§6.6): more/faster data namespaces (fresh scales at ~3.05 GB/s per
  device) and a cheaper target dealloc/rewrite path. Client-side, the
  next honest lever is contiguity-aware allocation (raise the field's
  coalesce factor above 1.05 so the drain's command count collapses) —
  filed, unpursued (bounded win: dealloc looked per-byte-ish on zram,
  §6.2 E1's sublinear width scaling).
* **OQ-5 (new):** `SQUEEZEFS_RECLAIM_CAP_PARK_MS` (default 1000) and
  the cap (4096 blocks) are liveness bounds, not measurements — if a
  substrate shows sustained `cap_overflow` growth, derive the cap from
  drain-rate × acceptable-lag there.
