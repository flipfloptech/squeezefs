# 2026-07-29 — Read saturation: large-op ring reads, ring-fed stream classification, and the prefetch window economy

Branch `perf/read-saturation` off dev tip `b26c293`. Commits: red
`a5ba2db` (large-op ring READ economy contracts), green `b1e4b16`
(multi-slab pipelined `ring_pread`), red `<fix2-red>` (ring reads must
feed the classifier/pipeline), green `af342aa` (ring-side lane feed +
`lane_pre_fed` + runtime-agnostic `spawn_bg`), red `9829d02` (window-
economy decision tables), green `d50b119` (budget-derived cap + split
issue bounds + overrun growth), plus the docs/evidence commits.
Design amendments: `docs/design-read-path.md` §5.5 (2026-07-29 note),
`docs/design-preload-interception.md` Rev 15.

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

<!-- BRACKET TABLE -->

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

<!-- RESIDUALS -->
