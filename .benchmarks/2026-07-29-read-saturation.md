# 2026-07-29 — Read saturation: large-op ring reads, ring-fed stream classification, the prefetch window economy, and the transient stream window

Branch `perf/read-saturation` off dev tip `b26c293`, rebased onto
`fcde1dc` for landing (§8.8 — commit map, semantic rebase check, and
the confirmation bracket vs the current base). Commits: red
`a5ba2db` (large-op ring READ economy contracts), green `b1e4b16`
(multi-slab pipelined `ring_pread`), red `fde3da0` (ring reads must
feed the classifier/pipeline), green `af342aa` (ring-side lane feed +
`lane_pre_fed` + runtime-agnostic `spawn_bg`), red `9829d02` (window-
economy decision tables), green `d50b119` (budget-derived cap + split
issue bounds + overrun growth), red `e3db374` (transient stream window
contracts), green `8a5d2e4` (governor-arbitrated stream re-fill
admission — §8 below), plus the docs/evidence commits.
Design amendments: `docs/design-read-path.md` §5.5 (2026-07-29 note) +
§5.3 admission-decision amendment (part 2, the transient stream
window), `docs/design-preload-interception.md` Rev 15.

## 0. The governing law (user directive, verbatim)

> "We need to be able to saturate a 200Gb/s fabric. If fio pulls
> 16 GB/s and Lustre pulls that + some easily, we should be at the
> 15/16 GB/s mark up and down using the IL shim."

The write side is at 13.7 GB/s field and closing (the probe-up governor
campaign, sibling). This campaign is the read side: il cold streaming
reads vs the raw fio READ ceiling on the same substrate, il ≥ kernel
per the parity law, and the cold small-op IOPS ceiling raised with its
limiter named.

## 1. The field signature (motivating data)

User's 4-node 2×200GbE cluster, nullblk targets, binary `b26c293`, il
path, sequential 4k reads: **900k–1.2M IOPS for a few seconds, then
collapse to 200–300k sustained.** Interpretation confirmed by rig
reproduction (§3): the burst is the warm RAM-tier ceiling; the collapse
floor is the cold-miss fabric-RTT ceiling — every 4k op paying one
ranged round trip via direct-drive, forever.

**The live field capture (2026-07-29, same cluster, 4-wide nullblk
plane, binary `b26c293`, elbencho `-r -t 32 -b 4k --direct` sequential
infloop over 32×512 MiB — 32 GiB ≫ the RAM budget):** sustained
**307k IOPS / 1.2 GB/s** after the warm burst; device truth during the
collapse: each of 4 heads serving 40–50k reads/s at **exactly ~4.1 KB**
(`rareq-sz` 4.08–4.18) — every user 4k read one 4k fabric round trip,
no whole-block fetches, no batching. Mount-lifetime counters:
`read_admission_governor_denials` ≈ 9.1 M, `ranged_reads` ≈ 19.6 M vs
**`prefetch_issued` ≈ 97 total** (the pipeline never engages on this
path), `prefetch_foreground_waits` ≈ 1.5 k,
`read_admission_wasted_bytes` ≈ 659 GB. The mechanism chain:
beyond-budget sequential stream → the scan-resistant admission governor
(2026-07-26) correctly refuses admission → governor-denied O_DIRECT
misses take the P1.5 direct-drive path → each 4k op device-true,
bypassing BOTH the 1024×-per-block amortization AND the R2 pipeline.
Two individually-correct mechanisms composing badly on exactly one
shape. **Reference proof of available headroom on the same mount:** the
EXA validation harness (fio 32 jobs × 1 MiB × QD8 libaio, KERNEL path,
no shim) pulled **16.9 GB/s reads** — the hardware and the FS read path
saturate when the app supplies its own deep parallelism; elbencho psync
t32×1 MiB il reads sustained only 6.9 GB/s (4.66 ms/op — Little-bound;
prefetch not hiding fabric latency there either, `b26c293` having no il
classification at all). The 4k collapse and the 1 MiB 6.9 GB/s are the
same disease at two block sizes.

## 2. Substrate & instruments (labeled)

22-CPU box, 109 GiB RAM, kernel 7.1.4-1-cachyos — SHARED for the whole
campaign with two sibling workloads (a running fstests release gate and
the probe-up write-governor campaign): per-side loadavg is logged in
the bracket log and cited with the verdicts; the A-B-B-A alternating
order is the defense, and the kernel twins are the canaries.

Substrate: **devsub tcp instance `rsat`** (`SQZ_DEVSUB_TRANSPORT=tcp
SQZ_DEVSUB_INSTANCE=rsat SQZ_DEVSUB_OSS_GB=12 tests/dev_substrate.sh
create`): meta `/dev/nvme9-12n1` (memory null_blk), data
`/dev/nvme13-16n1` (12 GiB zram each) over nvmet-tcp on localhost — the
fabric-sensitive venue (two-substrate rule). Filesystem: cache-less
format, 4 MiB blocks; mounts `--daemon --allow-other --interception
--mem-cache-size 1GB` ⇒ **hot-block tier budget 128 MiB (32 blocks)**.
Dataset: 16 × 1.5 GiB of zeros (24 GiB ≫ budget; zeros ≈ free on zram —
the device is not the wall, the client path is: the field's nullblk
analog) + 16 × 8 MiB fit-small warm set. Instrument: **fio 3.42**
(psync for 1 MiB rows, libaio qd16/qd32 for 4k rows), stated per row.
Engagement per charter rule 4 on every il row: `.stats` deltas printed
per row (`ipc_ops_read` accounts the row's ring ops; `read_device_true_
reads = 0` on every default-posture row).

**Raw fio READ ceilings (the finish line, measured on the rig's 4 data
namespaces, libaio direct):** seq-1M 16×qd16 = **33.2 GiB/s**; rand-4k
16×qd16 = **895k IOPS**; seq-4k 16×qd16 single-namespace = 598k.
(Raw seq write for context: 35 GiB/s — zero pages.)

## 3. Baseline (BASE = dev `b26c293`) — the convictions, measured

| row | result | the smoking gun (per-row `.stats` deltas) |
|---|---|---|
| il seq-4k cold, t16 qd16, 25 s | **burst 1.68–1.82M for ~4 s → collapse to ~560–600k sustained** (the field signature, exact) | `read_streams_classified≈0-ish`, `prefetch_issued=162`, **11.7M direct-drive serves** = one 4k ranged fabric RTT per op; 6.9M warm serves |
| il seq-1M cold, t16 psync, 20 s looped | 8,585 MiB/s (0.26× raw) | `ipc_ops_read/user-op = 16.02` — **16 serial 64 KiB slab RTTs per MiB** (`ring_pread` chunked at the arena slab); `classified=0`, `prefetch_issued=0`; 2.18M ranged 64 KiB window reads |
| kern seq-1M cold (twin) | 10,536 MiB/s (0.32× raw) | prefetch covered **3,701 of 58,073** fetches (6 %) — `effective_window = share%×budget/block/streams` = **1 block/lane** at the default shape, and in-flight fetches were charged against it: a structurally depth-1 pipeline |
| kern seq-4k cold (twin) | 590k | classification unstable at qd16 (171 classify events, ranged 8M ops) — kernel context, pre-existing |
| il rand-4k warm fit-small (t16 qd8) | 1.15M IOPS | the warm ceiling to protect (bar d) |

Three convictions, three mechanisms (suspects 1, 2 and the collapse
face of 1 from the mission brief — all verified by measurement above):

1. **`ring_pread` slab chunking** (suspect 1's round-trip half): a
   1 MiB il read = 16 serial 64 KiB fabric RTTs; writes have ridden ONE
   multi-slab window since DIALED P3 (Rev 11 named reads the pre-agreed
   follow-on).
2. **Ring ops never fed the §5.3 stream classifier** (suspect 1's
   routing half + the collapse row): `pipeline_touch` was handler-only;
   il streams never classified, the §5.6 streaming veto never engaged,
   the R2 pipeline never ran, and every il 4k miss direct-drove one
   ranged window read. The burst = warm tier; the collapse = the
   fabric-RTT floor. (Governor denials are NOT the mechanism — the il
   collapse row shows `denials=0`.)
3. **The §5.5 window economy self-limited** (suspect 2): fixed cap 16,
   AND the per-lane budget share charged in-flight fetches (transient
   R5-gauged DMA buffers) as if they were hot-tier residents — depth-1
   pipelines at the default 16-stream shape, on the kernel path too.

Suspect 4 (refetch spiral): present as a *consequence* — under the
depth-1 economy `prefetch_evicted_unconsumed` fired and quiesced lanes;
the detector itself is correct and untouched. Suspect 5 (read-side
alloc classes): the op-economy alloc pin
(`warm_fast_path_serves_are_allocation_free`) is the standing guard —
it caught this campaign's own first cut adding a second per-op moka
get, which was removed (the probe now shares its metadata entry).

## 4. The mechanisms (landed)

1. **Multi-slab pipelined `ring_pread`** (`crates/squeezefs-preload/
   src/session.rs`, green `b1e4b16`) — the read twin of ring_pwrite's
   P3 economy: contiguous `claim_run` windows sized to `max_op_bytes`
   (1 ring op per MiB at default geometry), every chunk submitted
   before any is waited on (flights reaped in offset order, batch
   doorbell), POSIX short-read prefix semantics. Read-specific custody
   rule: arena copy-out strictly AFTER the completion's Acquire and
   strictly BEFORE `release_run` (a released run is claimable by
   sibling threads). Daemon side unchanged (multi-slab windows were
   always admitted). Pinned: 6 read twins of the P3 pins in
   `tests/preload_session_tests.rs`.
2. **Ring-side stream feed** (`src/routing.rs` +
   `src/ipc_service.rs` + `src/fuse_client.rs`, green `af342aa`):
   `DataRouter::ring_read_lane_touch` — every ring read observes into
   the §5.3 lanes once, at the sink (warm serves after completion:
   silent consumption advances the consume edge; misses before the
   P1.5 ladder: the 4th contiguous op's classification vetoes
   direct-drive for the 5th). Ring handoffs carry
   `ReadClassHint::lane_pre_fed` (handler never double-observes — 16
   double-observations would declassify). `bg_admit::spawn_bg` is
   runtime-agnostic (foreign service threads spawn prefetch onto the
   fuse3 TPC lanes — the handoff-economy venue). Alloc-free warm path:
   the §5.5.1 probe returns its metadata entry for reuse; the lanes
   moka lookup is borrowed-key-first. The ddt posture is untouched
   (device-true stays the measurement escape). Pinned:
   `tests/read_saturation_tests.rs` (3 contracts, red at parent).
3. **Window economy** (`src/routing.rs`, green `d50b119`): the fixed
   cap 16 retired — default cap = `share% × hot_budget / block_size`
   railed [4, 4096] (`derived_prefetch_window_cap`; explicit env wins
   verbatim, 0 = kill switch); issue admission split
   (`prefetch_issue_admits`): landed-unconsumed ≤ per-lane resident
   share (zero-share never speculates), in-flight + unconsumed ≤ the
   AIMD window; growth (`prefetch_window_grows`) on foreground-wait OR
   clean plan overrun (the silent-consumption regime's shallowness
   signal), refused under an evicted-unconsumed streak, Green-gated.
   The reactive spiral controls (AIMD halving, progress-clocked
   quiescence, spawn shedding, R5 gates) untouched and pinned. Pinned:
   `tests/read_prefetch_window_tests.rs` (8 table tests, 4 red at the
   scaffolding commit).

## 5. Counted A-B-B-A bracket (CAMP `d50b119` pair vs BASE `b26c293` pair)

Order CAMP-BASE-BASE-CAMP; fresh format + interception mount + dataset
per side; fresh mount per cold row; KD-7 same-commit daemon+shim pairs
(clean identities, no dev override); medians of 3; engagement exact on
every il row (`ipc_ops_read` Δ accounts the row's ring ops;
`read_device_true_reads = 0` throughout — default posture). Raw CSV +
per-second logs + per-side loadavg: `/tmp/rsat/bracket-counted/`
(preserved with the run).

Contention, labeled honestly: the whole bracket ran concurrently with
the sibling write-governor campaign's own fio matrix (its daemon at
1–2 cores + row bursts; per-side loadavg logged 11–34, of which our own
rows contribute ~15–40 runnable threads while running). The alternating
order and the kernel twins are the noise defense; verdicts below cite
both CAMP windows.

Sides: CAMP = `0e984a8` pair, BASE = dev `b26c293` pair (both clean
identities — an earlier bracket attempt with a `-dirty` CAMP pair was
DISCARDED whole: the KD-7 identity screen passthrough'd every il row of
one side, the per-row engagement enforcement now aborts the rig on any
`ipc_ops_read Δ = 0` il row, and the count restarted from zero).

| row (median IOPS) | CAMP1 | BASE1 | BASE2 | CAMP2 | camp/base (medians) |
|---|---|---|---|---|---|
| **il seq-4k cold 25 s (the collapse row)** | 1,213,505 | 654,986 | 604,907 | 1,420,330 | **2.09×** |
| — sustained window (10–24 s, per-second logs) | 1,300,362 | 543,073 | 524,816 | 1,432,714 | **2.5×** — BASE sits ON the collapse floor (525–543k ≈ the baseline's 560k); CAMP sustains 1.28–1.46M ≈ the warm serve ceiling. The field's burst-then-collapse is CLOSED; the new limiter is the sync fast-path serve rate (service-thread CPU), named below |
| kern seq-4k cold (twin/canary) | 94,079 | 218,356 | 95,250 | 470,897 | kernel context — wildly contention-sensitive (its qd16 classification instability is pre-existing); il beats its kernel twin on every CAMP window |
| il seq-1M single-pass cold (GiB/s) | 9.1 | 4.2† | 9.8 | 9.7 | 1.34× by medians; †BASE1's reps (3.5/7.3/4.2) took the worst sibling burst — by the quiet windows this row is par-to-modest-gain ON THIS RIG (localhost RTTs hide the round-trip economy; the op ledger below is the transferable proof) |
| kern seq-1M single-pass (twin) | 5.0 | 6.3 | 4.5 | 7.4 | il ≥ kernel on both CAMP windows (9.1 vs 5.0, 9.7 vs 7.4) |
| il warm rand-4k fit-small | 1,003,210 | 1,138,260 | 1,248,906 | 1,089,742 | 0.88× in-bracket (contention-tainted: both CAMP windows ran hotter). Same-binary interleaved kill-switch A/B on the final pair (`SQUEEZEFS_READ_PREFETCH_WINDOW=0` vs armed, fresh mounts): **−0.4 % and −8 %** across two load windows — the ring-touch tax after the warm-laneless fix is bounded by the moka lanes get + 4-lane scan; residual filed (§7) |
| il rand-4k t32qd32 churn (flagship) | 405,424 | 388,713 | 457,549 | 349,977 | 0.89× — inside this bracket's own BASE spread (389k vs 458k = ±9 %) under the sibling's variable load; governor behavior identical (denials ≈ serves, escalations trickle-bounded, `read_device_true_reads = 0`) |

**The per-op economy ledger (engagement-exact, from the row deltas):**

- il seq-1M: CAMP `ipc_ops_read = 24,576` for 24,576 user ops (**1.00
  ring op / MiB**) vs BASE `393,216` (**16.0** — the slab-chunk
  collapse). CAMP `get_obj = 7,511` device read ops for 6,144 unique
  blocks (1.22×, prefetch-deduped); classified = 16/16 streams,
  prefetch engaged (~1,500 issues).
- il seq-4k (CAMP2 r1): 35.8M ring ops → **35.1M sync fast-path serves
  (98 %)**, 670k handoffs (1.9 %), **34 direct-drive serves** (vs
  BASE's 11.7M per-op ranged RTTs on the baseline row), `ranged_reads =
  332`, `get_obj = 44,108` (whole-block fetches ≈ passes × 6,144
  blocks), governor denials 0.
- Amplification: CAMP seq rows fetch whole blocks once per pass
  (`get_obj` ≈ blocks + prefetch dedupe ≤ 1.3×); no seed reads;
  `read_device_true_reads = 0` everywhere (default posture).

**Bars adjudicated:**

- (a) Raw fio READ ceiling measured: 33.2 GiB/s seq-1M / 895k rand-4k
  (zram zero-page reads — a memcpy-class ceiling this daemon's
  2-copy-per-byte serve path cannot reach on 22 CPUs by construction).
- (b) il seq-1M cold streaming: **9.1–9.8 GiB/s ≈ 0.29× of the raw
  memcpy ceiling** on this rig, il ≥ kernel on every matched window,
  ring ops/MiB 16 → 1. The transferable claims for the field's
  2×200GbE wall are the op ledger (1 RTT/MiB instead of 16) and the
  engaged pipeline — the absolute 15/16 GB/s mark needs the field's 32
  CPUs and real NICs, and the remaining rig-side residual is
  §7's serve-path copy economy.
- (c) il seq-4k cold sustained: **525–543k → 1.28–1.46M sustained
  (2.5×)**, order-independent; limiter now the sync fast-path serve
  rate (service-thread CPU on 5 derived threads), not fabric RTTs.
- (d) warm rows: −0.4 %/−8 % by same-binary kill-switch interleave
  (contention-bounded); never below 1M in any counted window.
- (e) rand-4k t32qd32: in-band (±9 % BASE self-spread), governor
  posture identical.
- (f) write rows: write_matrix armed-odirect parity sweep — see §6.


## 6. Gates

- **Full cargo gate from zero** (branch tip): `cargo fmt --check`
  clean; `cargo clippy --all-targets --all-features -- -D warnings`
  clean; `cargo test --all-features -- --test-threads=1` exit 0;
  `cargo doc --no-deps` generated; bench smoke
  (`cargo bench --benches -- --test`) 26/26.
- **Loom**: not owed — no house lock-free protocol changed
  (`ring_pread` reuses the P3 slot-core protocol incl.
  `release_claimed`, whose loom model shipped with Rev 11; the lane
  atomics are the §5.3 racy-tolerant heuristics class; `spawn_bg` is a
  venue change).
- **Preload gate**: legs 1+2 PASSED end-to-end on the final pair —
  incl. mount parity + engagement, dup/close_range/lseek rows, fio
  libaio verify, foreign-netns rendezvous, kill-9 soak (5 cycles, zero
  session/arena residue), fork-kill-parent, libaio lifecycle ×3
  orderings, direct-drive kill-9 soak (engaged +15,360 serves, zero
  residue).
- **statfs ×10 loaded soak**: 30/30 green, 0 hangs (stated load
  recipe: looping fat-LTO `cargo build --release` with `touch
  src/lib.rs` per iteration in a separate target dir, live for the
  whole window, plus the sibling campaigns' ambient load).

## 7. Residuals (recorded, not chased)

- **The warm-touch residual**: after the warm-laneless fix the ring
  touch still costs a moka `stream_lanes` get + 4-lane scan per warm
  op (kill-switch interleave bounds: −0.4 %..−8 % across load
  windows). Named next lever if the fleet wants it back: gate warm
  feeds on a leak-proof active-streams hint (the `stream_gauge`
  pattern) — the design must keep the granted-regime classification
  path (warm ops 3–4 CONTINUING a miss-started run), which a naive
  gauge gate would break.
- **Classification churn under qd16 reordering**: the seq-4k rows
  re-classify ~4–7×/file/25 s (lane contiguity vs out-of-order
  arrivals) — delivery is already at the serve ceiling, so this is
  bookkeeping noise; a block-granular lane matcher is the refinement
  if the field's deeper queues make it visible.
- **Kernel-path seq-4k qd16 instability** (its own twin rows: 94k–471k
  under load): pre-existing, untouched by this campaign; the same
  block-granular matcher would serve it.
- **Prefetch coverage on 24 GiB single-pass streams** is ~20–30 % of
  fetches (resident-share = 1 block/lane at 16 streams × 128 MiB hot
  budget; `prefetch_evicted_unconsumed` ≈ 700/pass keeps the AIMD
  honest): the reader fronts most fetches as single-flight joiners.
  Raising the hot budget (or fewer/deeper streams) deepens it; the
  split-bounds change makes that a budget decision instead of a
  structural depth-1 cap.
- **This rig's absolute streaming ceiling is serve-path memcpy-bound**
  (~10 GiB/s at 16 streams: one hot-tier copy into the arena + one
  shim copy-out per byte on il; the raw 33 GiB/s ceiling is zram
  zero-page memcpy). The field's 15/16 GB/s bar rides the op economy
  + pipeline landed here plus the field's CPU/NIC budget; if the
  fleet's rig-measured wall lands on this term, the next campaign is
  the read twin of the placed-sever/lease copy economy (serve into
  the arena without the hot-tier bounce, or lease the tier bytes).
- The bracket ran against sibling-campaign load throughout (labeled
  per side); a quiet-box confirmation pass of the warm/rand rows is
  cheap insurance when the box frees up.

## 8. The transient stream window (continuation session, 2026-07-29)

The §1 live field capture arrived after §5's bracket: the SUSTAINED
face of the same disease. Verification + fix, all on the rsat rig
(instance `rsat`, nvmet-tcp — substrate/instrument stated per row).

### 8.1 The conviction, reproduced (the field shape, verbatim)

Row: dynamic elbencho 3.1-10, `-r -t 32 -b 4k --direct --infloop
--timelimit 120`, 32×512 MiB (16 GiB ≫ the 128 MiB hot tier), il, fresh
format + cold mount. Artifacts `/tmp/rsat/field-repro/`.

| side (pair) | sustained (per-second series) | device truth | the counters |
|---|---|---|---|
| BASE (`b26c293` ≡ dev tip `778d6d0` code) | burst ~976k → **~280k flat floor** (the field's 307k), with ~900k spikes at each 32 s cooldown-epoch roll | `rareq-sz` **4.2 KiB** (field: 4.08–4.18) | 30.5 M of 44.2 M ops = per-op 4k ranged direct-drive RTTs; `classified = 1` of 32, `prefetch_issued = 2`, escalations cooldown-cycled (~13/s trickle + epoch-roll bursts) |
| CAMP (`0e984a8`, pre-§8) | **1.32 M flat ×120 s** (4.7× the floor) — the collapse is closed by the §4 ring-fed classifier | whole-block (`get_obj` ≈ 2.3 k/s × 4 MiB) | direct-drive 56 of 158.8 M ops; **but `read_admission_wasted_bytes` +454 GB/120 s ≈ 4 GB/s = 42 % of device reads** — the field mount's 659 GB lifetime face, still growing |

The rig face of the deny-and-direct-drive chain is the 32 s escalation
COOLDOWN (denials = 0 here because the clamp never engages on the
25–120 s single-mount window); the field face is the governor clamp
(9.1 M denials on a long-lived mount). Same composition, same floor:
either denial source ⇒ per-op device-true 4k RTTs.

### 8.2 The residual mechanism (measured, then pinned red)

At `0e984a8` the §5.3 admission table's **Streaming row was never wired
at the fill site** — the ghost decision sees only the block key. On a
sustained loop every pass-2+ stream refill ghost-hits and admits
**protected + published**: the waste ledger runs at 4 GB/s (42 % of
device bandwidth; `wasted_bytes ÷ device-bytes ≤ ~5 %` is the
documented bounded-waste verdict), the within-pass `get_serving` credit
(~58 % payback) dilutes the clamp ratio so the governor can never
engage on it — re-opening the 2026-07-26 scan-resistance hole for
co-tenants — and a cache-ful mount would re-pay the R1b publish tax
once per block per pass. Red contracts: `tests/read_stream_transient_
tests.rs` (`e3db374`) — the beyond-budget loop's wasted delta measured
exactly 48 refills × 128 KiB shortfall at the parent.

### 8.3 The fix (green `8a5d2e4`)

`FillClass::{Demand, DemandStream, Prefetch}` provenance at the fill
site (one lanes probe per FILL, never per warm op); a streaming fill's
ghost hit admits only through `AdmissionGovernor::allow_stream_
admission` — the same clamp + token reservation as the ranged site
(one grant economy), refusals on the NEW counter `read_admission_
stream_transients` (never `governor_denials`: a held-transient fill
still serves its reader from hot probation). Granted stream admissions
enter marked (`put_protected_stream`): `get_serving` credits them
nothing (the admission's marginal value is cross-pass retention only),
victims report full shortfall (the clamp finally SEES beyond-budget
stream admissions) but are exempt from the `evicted_unhit` tripwire.
`DemandStream` fills feed `note_foreground` (their whole-block fetch IS
the workload's own device spend; `Prefetch` never self-funds), so the
clamped grant trickle is `fill_pct` % (default 5) of the stream's own
bandwidth. The 9.2-vs-16.6 GiB/s lineage (disk-tier re-read
convergence) is pinned green: convergence = device-flat tier service
(hot-RAM residents + published grants), reached through the unclamped
start + the trickle + the 2-epoch window release.

### 8.4 Fixed pair on the field shape (`0b60160` pair, 120 s)

**1.2 M IOPS sustained flat ×120 s** (per-second series 1.13–1.32 M, no
decay trend), `rareq-sz` **4,090 KiB** (whole blocks), direct-drive 334
of 145.8 M ops, `read_device_true_reads = 0`, engagement exact. The
ledger: **wasted 447 MB/s ÷ device 8.6 GB/s = 5.2 %** — the `fill_pct`
bound holding on the streaming shape (vs 42 % unfixed, an 8×
reduction); `read_admission_stream_transients` = 234,828 carried ~95 %
of refills; `read_admission_evicted_unhit` = **1** in 120 s (tripwire
semantics preserved). A KD-7 note: the first fixed-pair attempt ran a
`-dirty` shim against a clean daemon — the identity screen
passthrough'd every il op and the rig's per-row engagement enforcement
aborted loudly (`ipc_ops_read Δ = 0`), exactly as designed; the pair
was rebuilt clean and the row re-run.

### 8.5 Sustained counted A-B-B-A brackets (the ≥60 s law)

Two counted brackets, order CAMP-BASE-BASE-CAMP, fresh format + dataset
per side, fresh mount per row, medians of 3 per row, engagement exact
(`ipc_ops_read` Δ accounts every il row; `read_device_true_reads = 0`
throughout), per-second series + diskstats per row (artifacts
`/tmp/rsat/bracket2-final/`, `/tmp/rsat/bracket3-final/`). Substrate:
rsat nvmet-tcp devsub. Contention labeled per side (the sibling
campaign's nvmet-tcp load came and went across the window — loadavg
7–26 logged per side; alternating order + kernel twins are the
defense). BASE = `b26c293` pair (≡ dev tip `778d6d0` code).

**Raw fio READ ceilings, sustained 60 s (the finish line):** seq-1M
16×qd16 across the 4 namespaces = **26.9 GiB/s**; rand-4k = **842k
IOPS** (zram zero-page memcpy-class ceilings).

**Bracket A (CAMP = `0b60160`, the transient window):** medians of 3
per side, camp/base of side-medians:

| row (median IOPS / MiB/s) | CAMP1 | BASE1 | BASE2 | CAMP2 | camp/base |
|---|---|---|---|---|---|
| **il field row** (elbencho t32 4k O_DIRECT seq infloop 32×512 MiB) | 1,013,443 | 288,866 | 384,591 | 1,259,403 | **3.37×** — the field's 307k floor raised to 1.26 M flat (CAMP2 thirds −0.6 %; CAMP1's −22 % decay tracks the just-finished cargo gate's decaying load, labeled) |
| il seq-4k fio twin (t16 qd16, 24 GiB set) | 1,071,658 | 536,436 | 651,042 | 1,328,926 | **2.02×** |
| il seq-1M sustained (psync t16 looped) | 9,736 | 4,965 | 8,505 | 10,767 | **1.52×** (MiB/s) — BASE's sustained loop collapses to per-64 KiB-chunk direct-drive RTTs (`dd_serves` ≈ 16/user-op); CAMP2 10.8 GiB/s = 0.40× the raw memcpy ceiling |
| kern seq-1M twin | 8,740 | 9,684 | 10,272 | 9,442 | 0.91× — the one soft row: denied-transient fills entered plain probation and lost the clock race mid-consumption (fetch ratio 1.19× vs 1.07×); fixed by the grace refinement (`3623139`, Bracket B) |
| kern seq-4k twin | 278,961 | 397,509 | 546,788 | 539,903 | 0.87× (qd16 kernel classification instability, pre-existing — same residual as §7) |
| il rand-4k t32qd32 churn (flagship) | 350,744 | 323,868 | 477,150 | 449,759 | **1.00×** — governor posture identical (denials ≈ dd serves both sides) |
| il warm rand-4k fit-small | 1,072,613 | 979,427 | 1,115,709 | 1,059,736 | **1.02×** — never below 979k in any counted window |

**The bounded-waste verdict (wasted_bytes ÷ device read bytes, medians
— the AGENTS ≤ ~`fill_pct` % law):** CAMP holds **≤ 5.0 % on every row
family** (field 5.0, seq-1M 4.9, seq-4k 4.8, flagship 4.2, warm 5.0);
BASE runs **27.8 % on kern seq-1M** (the unfixed fill-site pollution, ~3
GB/s of ledger waste + `evicted_unhit` ≈ 5.5 k/row) and shows low il
waste only because its il streams never classify at all (the collapse).
Flagship random churn: CAMP 4.2 % vs BASE 4.9 % — **the scan-resistance
bound did not regress** (the fix's contract).

**Bracket B (final pair `3623139`, the grace refinement; CAMPA-BASEA-
BASEB-CAMPB — fully contended window, sibling nvmet-tcp load loadavg
8–26 across all four sides, labeled):**

| row (median IOPS / MiB/s) | CAMPA | BASEA | BASEB | CAMPB | camp/base |
|---|---|---|---|---|---|
| **il field row** | 1,250,985 | 369,304 | 378,264 | 1,108,604 | **3.16×** |
| il seq-4k fio twin | 1,396,632 | 614,319 | 704,682 | 1,335,410 | **2.07×** |
| il seq-1M sustained (MiB/s) | 8,858 | 8,397 | 8,538 | 9,971 | **1.11×** (bracket A: 1.52× — BASE's own row is bimodal 4.9–8.5 GiB/s across windows; CAMP spans 8.9–10.8; il ≥ its kernel twin on every CAMP window in both brackets) |
| kern seq-1M twin (MiB/s) | 8,810 | 9,841 | 9,877 | 8,674 | 0.89× — **attributed** (below) |
| kern seq-4k twin | 525,964 | 532,173 | 539,443 | 422,175 | 0.88× (±11 % CAMP self-spread under the load; pre-existing qd16 instability) |
| il rand-4k t32qd32 churn | 455,373 | 471,473 | 449,579 | 364,276 | 0.89× (CAMPB's window ran hottest; bracket A: 1.00× — BASE self-spread across the two brackets 324–477k) |
| il warm rand-4k fit-small | 945,959 | 1,141,766 | 1,013,969 | 852,550 | 0.83× (bracket A: 1.02×; the warm serve rate is service-thread-CPU-bound — the §5 named limiter — and CAMPB's window carried the heaviest sibling load; BASE self-spread 979–1,142k across brackets) |

Waste ratios, bracket B medians: CAMP ≤ **4.9 %** on every family
(flagship 4.3 vs BASE 4.8 — scan resistance intact); BASE kern-seq-1M
**27.8 %** with `evicted_unhit` ≈ 5.5 k/row (the outlawed pollution,
reproduced in both brackets).

**The kern seq-1M −10 %, attributed (order-independent in both
brackets, CAMP and BASE each self-consistent):** BASE admits every
pass-2+ stream fill PROTECTED — under 16-way mutual eviction pressure
those fills carry two clock lives mid-consumption where the transient
window's grace carries one, so BASE loses fewer blocks to sibling
streams (fetch ratio 1.07× vs CAMP 1.16–1.19×) — a real retention
benefit BASE buys with the 27.8 %-of-device-bandwidth waste ledger, the
broken co-tenant clamp, and (on cache-ful mounts) a per-pass 4 MiB
publish per block. The grace refinement recovered part of it (CAMP3
directional row hit par at 10.1 GiB/s); the remaining −10 % is the
price of the honest ledger on this one twin and is recorded as a
residual (§8.7) — the il face of the same row is a WIN in both
brackets (il ≥ kern on every CAMP window).

### 8.6 Gates (continuation session)

- **Full cargo gate from zero** (`8a5d2e4` tree + docs): clippy
  `-D warnings` clean, `fmt --check` clean, `cargo test --all-features
  -- --test-threads=1` **156/156 suites ok** (an earlier run had a
  single defrag_tests failure while my own elbencho field row ran
  concurrently — attributable load flake; the count restarted from
  zero on the quiet box per the multi-run discipline and passed clean
  end-to-end), `cargo doc --no-deps` generated, bench smoke 26/26,
  exit 0 (`/tmp/rsat/gate2.log`).
- **Preload gate**: leg 1 (unprivileged) PASSED; leg 2 (root — mount
  parity + §3 rule-4 engagement, dup/close_range/lseek, fio libaio
  verify, kill-9 + fork-kill-parent soaks, direct-drive kill-9 soak
  engaged +15,360 serves, zero residue) PASSED.
- **statfs ×10 loaded soak**: **30/30 green, 0 hangs** on the rebased
  tip `817f4af` (load recipe: looping fat release build with `touch
  src/lib.rs` per iteration in a second worktree, live for the whole
  window — loadavg 2.3–5.3; 82–89 s/roll under the 2.0–2.2 GHz
  governor vs the 62 s quiet nominal), fusectl waiting-connections
  residue sweep clean, zero squeezefs mounts left behind.
- **Loom**: not owed — the new tier/governor state is single-word
  `Relaxed` atomics with no cross-word invariant (the GhostTable /
  EntryState racy-tolerance class, documented on the types).

### 8.7 Residuals (continuation session)

- **kern seq-1M twin −10 %**: the transient window's one-lap grace
  retains less mid-consumption than BASE's outlawed protected
  pollution under 16-way stream pressure (attribution above). Named
  lever if the fleet wants it back: a consume-cursor-aware eviction
  hint (spare blocks AHEAD of their lane's consume edge, evict blocks
  behind it) — strictly better than class inflation and ledger-honest.
- **Borderline fit-small sets under spurious classification** (the
  warm row's shape): randread on small files occasionally lands 4
  contiguous ops, classifies a lane, and the zero-credit basis then
  reports genuinely-reused stream-granted blocks as full-shortfall
  waste (CAMP warm wasted ≈ 5 % of device instead of BASE's 0.1 %) and
  can engage the clamp on a set that pays back. IOPS impact sits
  inside the known warm-touch band (§7); the ledger noise is bounded
  by the same 5 % law. Refinement if needed: clear `stream_admitted`
  on plain-`get` block-level re-access (real reuse evidence) so
  borderline sets earn their credit back.
- The two brackets ran under materially different ambient load
  (labeled per side); the quiet-box confirmation pass of the
  warm/flagship rows was owed — **delivered in §8.8** (flag 1.00×,
  warm 1.03× on the campaign-only window, vs the CURRENT dev base).

### 8.8 Rebase onto `fcde1dc` + the confirmation bracket vs the CURRENT base (closing session, 2026-07-30)

Brackets A+B (§8.5) ran vs the `b26c293`/`778d6d0`-class base. Dev
then landed the probe-up write governor + the async-reclaim ENOSPC
valve (`778d6d0..fcde1dc`) and the branch was **rebased onto
`fcde1dc`** — textually clean (no overlapping hunks; the dev range
touches `src/routing.rs`/`src/fuse_client.rs` only in write-path
regions). Semantic check, both named suspects:

- **Stream/prefetch window vs `ProbeCore`** (the probe-up governor):
  same posture family — growth only on demand evidence
  (foreground-wait / clean overrun ↔ saturated-epoch delivery
  response), refusal under waste evidence (evicted-unconsumed streak ↔
  dead-gain backoff), both R5-Green-gated — but **fully disjoint
  state**: read-lane AIMD windows vs the write pipeline's depth
  multiplier share no code, counters, or feedback loop. No coupling to
  re-verify beyond the bracket below.
- **Async valve blocking-pool usage** (`a9f50a6`): the valve's
  `spawn_blocking` drains live in `src/block_reclaim.rs` /
  `write_pipeline` — untouched by this campaign. The campaign's
  `spawn_bg` never touches the blocking pool: caller's runtime when
  present, else `fuse3::raw::tpc_spawn` (the 2026-07-26
  handoff-economy venue rule), with the admitted/shed verdict rolling
  back issue-side accounting on shed.

**Commit map** (the §§1–8.7 evidence hashes are the pre-rebase
identities of the binaries that ran those brackets; the rebased
branch): `a5ba2db→d3d43ea`, `b1e4b16→9b38b95`, `fde3da0→2725dbd`,
`af342aa→175ebdb`, `9829d02→e7d30e1`, `d50b119→d97864b` (+
`f60a6dc`/`d76b809` warm-laneless pair), `e3db374→bae7768`,
`8a5d2e4→84579b3`, `3623139→d09440f`; bracket build points `0e984a8`
(§5) and `0b60160` (§8.4) are superseded by the rebased tip.

**The confirmation bracket** (`/tmp/rsat/confirm_bracket.sh`,
artifacts `/tmp/rsat/confirm-final/`): CAMP = rebased tip `817f4af`
pair vs BASE = **dev tip `fcde1dc`** pair (both clean KD-7
identities, `--version` verified per build). rsat nvmet-tcp devsub,
fresh format + il-written dataset per side, fresh mount per cold rep,
order CAMP1-BASE1-BASE2-CAMP2, medians of 3, ≥60 s per row (field
90 s), engagement enforced per row by the rig (abort on
`ipc_ops_read Δ = 0` or `read_device_true_reads ≠ 0` — zero aborts).
**Campaign-only window**: no sibling campaign load (per-row loadavg
logged is dominated by our own runnable threads — field CAMP rows
28–34 = their own 32 elbencho threads actually running; BASE field
rows 12–15 because theirs sit in fabric RTTs).

| row (median, side-median means) | CAMP1 | BASE1 | BASE2 | CAMP2 | camp/base |
|---|---|---|---|---|---|
| **field** (elbencho t32 4k O_DIRECT seq infloop 32×512 MiB, 90 s) | 1,250,902 | 253,472 | 254,971 | 1,220,372 | **4.86×** — BASE sits ON the field floor (253–255k ≈ the field's 307k, `rareq-sz` 7 KiB, 13.8–14.0M direct-drive RTTs of ~22.9M ops); CAMP serves whole blocks (`rareq-sz` ~4,090 KiB, dd ≤ 247 of ~110–117M ops), flat (first-done vs last-done IOPS within 0.01 %) |
| il seq-1M sustained (fio psync t16 looped, 60 s) | 7,477 | 5,925 | 5,715 | 7,412 | **1.28×** (MiB/s) — CAMP 1.00 ring op/MiB + engaged pipeline (~530 issues/row); BASE per-64 KiB-chunk direct-drive (dd ≈ 16/user-op, `rareq` 68 KiB); 7.4 GiB/s ≈ 0.28× of §8.5's 26.9 GiB/s raw memcpy ceiling (the §7 serve-path copy residual owns the gap) |
| il rand-4k t32qd32 churn (flagship) | 324,000 | 328,000 | 325,000 | 330,000 | **1.00×** — governor posture identical (denials 14.8–15.9M vs BASE 13.6–17.1M against ~19–20M dd serves/row both sides); `evicted_unhit` 131–750/row both sides |
| il warm rand-4k fit-small (t16 qd8) | 1,033,000 | 1,048,000 | 890,000 | 969,000 | **1.03×** — never below 941k in any CAMP rep; the quiet-box read on the row brackets A/B saw at 1.02×/0.83× under variable load |

**The ledger on the confirmation rows** (per-row deltas, medians):
field CAMP wasted 34.0–34.6 GB / 90 s ÷ device 7.4–7.8 GB/s = **5.0 %**
(the `fill_pct` bound, holding vs 42 % unfixed); transients carried
~144k grants/row, denials ≤ 205, `evicted_unhit` ≤ 1. Flagship: CAMP
wasted 4.2–4.7 % of device vs BASE 4.2–4.6 % — **scan resistance not
regressed on random churn** (the fix's contract, reconfirmed vs the
current base). Warm: the §8.7 borderline-fit-small residual's ledger
face reproduced exactly as recorded — CAMP classifies the fit-small
set (~17–19k classify events) and pays ~3.4 GB/s of device re-reads
(`rareq` 42–50 KiB) vs BASE's 0.6 GB/s, with the clamp engaging on
both sides (CAMP 108–144k denials first-rep decaying to 0.8–6k; BASE
278–365k first-rep then 0) — IOPS lands 1.03× because the row is
service-thread-CPU-bound (§5's named limiter); the §8.7 refinement
(clear `stream_admitted` on real block reuse) remains the named
lever.

**Thermal validity, labeled:** the external 2.0–2.2 GHz governor ran
throughout (as for every row in this note). seq1m/flagship reps: 0
governor steps. Field CAMP reps each overlapped exactly 1 step (plus
warm CAMP1 r3 / CAMP2 r1): the row **self-heats** — 1.2M IOPS across
32 threads trips the 85 °C step within its own window, so a re-take
can never produce a step-free rep at this clock (BASE at the 254k
floor never trips it). The error direction is strictly against CAMP
(part of each row at 2.0 GHz), within-side rep spread is ±2 %, and
the 4.86× verdict stands with the step counted against it.

**Gates on the rebased tip `817f4af`, from zero:** first attempt
aborted per the fail-fast law — `async_block_reclaim_tests::field_
rewrite_free_list_regime_runs_off_the_write_path` (dev's own
`aa8c5e9` valve-liveness contract, wall-clock deadlines) failed while
the BASE pair's 9m54s fat release build saturated the box;
attributable load flake, count restarted from zero on the quiet box
(`/tmp/rsat/gate4-rebased.log`): clippy `-D warnings` clean, `fmt
--check` clean, `cargo test --all-features -- --test-threads=1` exit
0, `cargo doc --no-deps` generated, bench smoke 26/26. **Loom**
(`tests/run_loom.sh`, the full model suite from zero on the tip):
**52/52 ok** — cheap insurance on top of the §8.6 not-owed ruling; no
model touches campaign state. **Preload gate legs 1+2** PASSED
end-to-end on the final pair (`/tmp/rsat/preload-gate3.log` — mount
parity + engagement, dup/close_range/lseek, fio libaio verify,
foreign-netns rendezvous, kill-9 + fork-kill-parent soaks, libaio
lifecycle ×3, direct-drive kill-9 soak engaged +15,360 serves, zero
residue). **statfs ×10 loaded soak**: §8.6's 30/30 on this tip.
Substrates torn down after the last row (`dev_substrate.sh status`
reports none); no stray mounts.

