# 2026-08-03 — il hold probe: the read-lane hold becomes the §5.5.1 sync fast path's fourth leg

Branch `perf/il-hold-probe` (off dev tip `f6532fd`, **unmerged — the
orchestrator merges**). Charter: `.benchmarks/2026-08-02-il-anomalies.md`
§2 — the il randread deficit's counted composition (cold rand-4k libaio
qd16: kernel 231.7k vs il 211.0k IOPS, −8 %/+110 µs) named ~220 k/row
missing warm serves: the kernel path's probe ladder serves ~294 k
ops/row from the read-lane hold / hot tier at ~µs (hot → hold → NVMe
tier, before the ranged dispatch) while the il DIALED-P1.5 prelude
direct-drove those governor-DENIED misses device-true — its only warm
source was the sync fast path's hot leg. Both prior retune suspects
(service-thread ceiling, reap park max) were exonerated there by flat
A/Bs and were NOT revisited.

## 1. What shipped (commits `2ad907c` red → `1d3f63f` impl → `dd6c7ea` docs)

One site covers both law surfaces the charter named:
`DataRouter::try_read_range_sync` — the §5.5.1 sync serve engine every
non-ddt ring read crosses BEFORE `try_direct_drive_default` can submit
device-true — gains the hold as its **fourth leg**, in the handler
ladder's order (staging → hot → **hold** → NVMe read-cache; hot
strictly first so warm hot entries keep funding the governor's payback
basis).

* **Foreign-thread discipline**: the probe is
  `ReadLaneHold::serve_with_provenance` — lock-free scc, no new locks,
  no blocking on the `sqz-ipc-svc*` thread. Binding currency is leg 3's
  own structural argument (the caller holds the inode read guard;
  `b_key` comes from the CURRENT map, which writers update under the
  write lock). A probe miss falls through unchanged (leg 3 →
  demote/direct-drive — fallback-is-correctness).
* **R1b ledger law** (the `3cd528b` fix's THIRD serve site): a
  demand-deposited entry's serve carries the admission ceremony (ghost
  touch → second-touch publish → protected hot re-landing,
  `hold_serve_admission`) — dispatched to the **fuse3 per-core handler
  lanes** (`tpc_spawn`, the 2026-07-26 handoff-economy venue; never
  `Handle::spawn` from the foreign thread), exactly-once per serve
  (concurrent same-key duplication stays in the GhostTable's
  racy-tolerant class, identical to the kernel serve sites).
  Lane-fetch deposits stay ledger-invisible end to end (the
  scan-resistance verdict).
* **Credit law**: the sync hot leg now credits the hold copy (the
  kernel handler hot arm's rule mirrored) — full consumption retires
  the entry, so `hold_evicted_unconsumed` keeps meaning starvation on
  il rows.
* **Gauges**: `ipc_hold_probe_{serves,misses}` (stats inode) — a
  bracket row is INVALID unless the serves delta accounts for its hold
  serves; `serves + misses` ≈ executed probes. 0 by construction under
  `SQUEEZEFS_READ_LANE=0` (A0) and on `direct_device_true` mounts
  (kernel parity: ddt skips every warm leg by POLICY).
* **Contracts** (`tests/ipc_hold_probe_tests.rs`, red-first — 4 red on
  the missing probe, A0 absence green by construction): bytes-exact
  serve + exact gauges; ceremony exactly-once (planted-provenance
  fixtures, both halves); miss counts + demote preserved; A0
  structurally inert (planted entry never consumed); hot-leg credit
  retirement. No new lock-free protocol ⇒ no loom owed (documented).

Gates: clippy `-D warnings` + fmt clean; read family
(`read_tier_admission_tests` 10, `read_lane_tests` 8,
`read_serve_phase_tests` 5, `mem_budget_tests` 16) + ipc family
(`ipc_op_economy_tests` 3, `ipc_host_tests` 23, `preload_session_tests`
24) + `preload_parity_tests` 14 + `read_saturation_tests` 4 +
`ranged_read_tests` 4 + `read_copy_ledger_tests` 2 + the new suite 5 —
all green at `--test-threads=1`. `cargo doc --no-deps`: the one warning
(`read_severed_bytes` → private `SeveredPool`) pre-exists on dev tip in
untouched `ipc_host.rs`. Rocky 8 pair built via the docker recipe at
`dd6c7ea` — in-container KD-7 asserts passed (`--version` carries the
commit, shim embeds the same commit, glibc ≤ 2.28); dev-tip control
pair built identically at `f6532fd`.

## 2. Venue disclosure — the field bracket is EPOCH-BLOCKED (measured)

The bastion was down for most of the session (100 % ping loss,
connection timeouts, ~00:30–02:00Z); when it returned (~02:05Z) the
survey found the standing pair `109d7bc` mounted on `/scratch/tmp/test`
with the user's fileset present (`exa_perf/` + `f1..16` — untouched
throughout). **The blocker is format-epoch, not connectivity**: the
standing store was formatted by `109d7bc` (pre-dynamic-meta-routing),
and every post-`8eb3d73` binary — this branch's `dd6c7ea` AND the
dev-tip control `f6532fd` — carries the bit-6 `KV_DYNAMIC_ROUTING`
presence-REQUIRED gate. Measured live (journaled SESSION START/END in
`/scratch/tmp/agent_runs.log`): the deploy ceremony ran to the mount
step and the holdprobe binary **refused loud, pre-write** ("v3 volume
formatted with a frozen routing width … reformat required") — exactly
the designed refusal, zero store mutation. Reformats are HARD-LIMITED
and the user's fileset lives on that store, so the chartered A-B-B-A
cannot run on this epoch. Standing pair restored and verified (pid
fresh, `109d7bc`, `transport_queues=32`, fileset intact); the retained
pair is staged sha-verified at `/scratch/tmp/{squeezefs,
libsqueezefs_il.so}.holdprobe` for the next reformat/reshape window,
when the acceptance bracket (A-B-B-A vs a REBUILT dev-tip-epoch
control, + A0, + soak) becomes runnable. No resets, no reformats, no
raw writes, no storage-node or NIC changes.

In its place: a **devsub-tcp local scoping bracket** (labeled per the
two-substrate rule — SCOPING, never field acceptance). Venue: 24-CPU /
117 GiB dev box, `tests/dev_substrate.sh` nvmet-tcp on 127.0.0.1
(4 × null_blk mds + 4 × zram-zstd oss), cache-less format, fio-3.39
libaio 4k qd16 nj8, 16 GiB fileset (8 × 2 GiB) vs
`SQUEEZEFS_MEM_BUDGET_MB=4096` (cold-by-overflow, set = 4× budget),
cold-by-remount per row, 60 s time_based rows, per-row `.stats` deltas.
Known venue caveat (measured, disclosed): the zram store decays
MONOTONELY across legs even with per-leg reformat (kernel rows 211.8 →
210.0 → 189.0 → 140.7k across the A-B-B-A sequence — slot-replace
economics), so cross-pair absolutes are order-contaminated; the
A-B-B-A discipline and same-leg ratios are what carry meaning here.

## 3. The composition — the missing warm serves accounted

The charter's term, reproduced and closed at local scale (per-row
`.stats` deltas; A = probe pair `dd6c7ea`, B = dev tip `f6532fd`):

| row (order) | IOPS | clat µs | ops | warm serves (hold + hot) | warm % of ops | `read_tier_admissions` |
|---|---|---|---|---|---|---|
| A1 kern | 211,834 | 600 | 12.71 M | 201,550 (101,727 lane + 99,823 hot) | **1.59 %** | 100,800 |
| A1 il | 289,387 | 442 | 17.36 M | 277,125 (**139,606 probe** + 137,519 hot) | **1.60 %** | 138,580 |
| B1 kern | 209,974 | 606 | 12.60 M | 198,581 | **1.58 %** | 98,151 |
| B1 il | 282,335 | 453 | 16.94 M | 132,978 (0 probe + hot only) | **0.78 %** | 838 |
| B2 kern | 189,007 | 673 | 11.34 M | 178,662 | **1.58 %** | 89,458 |
| B2 il | 277,703 | 460 | 16.66 M | 148,148 (0 probe) | **0.89 %** | 958 |
| A2 kern | 140,711 | 905 | 8.44 M | 132,105 | **1.56 %** | 65,050 |
| A2 il | 246,788 | 518 | 14.81 M | 261,910 (**128,373 probe** + 133,537 hot) | **1.77 %** | 126,371 |

**The warmth asymmetry is closed**: dev-tip il rows see HALF the warm
fraction their kernel twins see (0.78–0.89 % vs 1.58 %) — exactly the
charter's mechanism at this venue's scale; probe-pair il rows see the
SAME fraction (1.60–1.77 % vs 1.56–1.59 %). The R1b ledger now
converges through the ring: il-row admissions 838/958 (dev tip — the
ring was invisible to the ledger) → 126–139 k (probe — the ceremony
lands per ledger-visible serve, second-touch publishes + hot
re-landings running exactly as the kernel path's).

**Engagement is exact on every row** (the charter's validity law):
`ipc_fast_path_serves + ipc_direct_drive_serves + ipc_async_handoffs ≡
ipc_ops_read` EXACT on all 6 il rows; `ipc_hold_probe_serves + hot hits
≡ ipc_fast_path_serves` exact (±5 EOF short-circuits);
`ipc_hold_probe_serves ≡ read_lane_serves` exact; kernel rows move NO
ipc gauge (0 across the board).

## 4. The verdict rows (this venue)

* **Parity law**: il ≥ kernel on the same state on EVERY leg, both
  pairs (same-leg il/kern: A1 1.366, B1 1.345, B2 1.469, A2 1.754 —
  the ratio inflates as the store decays because the kernel row decays
  faster; localhost fabric ≈ 10 µs underweights the field's 235 µs
  warm-vs-device gap, so the probe's absolute win is structurally
  smaller here).
* **The lever bracket (adjacent legs, fresh substrate)**: A3 probe-hot
  253,045 IOPS / 505 µs vs A0 (`SQUEEZEFS_READ_LANE=0`) 250,297 / 510
  — **+1.1 % IOPS** with the probe serving 121,836 ops the A0 leg sent
  to the device. A0 gauges pinned 0 live (probe structurally inert).
* **No-regression rows**: warm-4k A 244.8k vs B 235.2k (A ahead at
  adjacent venue age); seq-1M A 6,359 MiB/s vs B 5,960 (A ahead;
  **probe engagement structurally 0 on seq** — multi-block requests
  demote before the ladder, zero probe tax by construction).
* **Sustained**: every row 60 s time_based; the 120 s leg ran 259,219
  IOPS with first-third → last-third −2.4 % (flat within this venue's
  noise; no decay trend).
* **Soak**: §5 below.

The FIELD verdict on the charter's −8 % row remains OWED to the field
bracket (same-state il ≥ kernel, engagement-exact, kernel rows
untouched) — the expected win there is bounded ≈ the gap, and this
venue's mechanism-level evidence (composition parity + exact
engagement + no-regression) is the strongest local proxy available.

## 5. Soak (local, 600 s, probe hot)

Mixed read-heavy on the probe pair: il rand-4k (nj6 qd16) + kernel
rand-4k (nj2 qd8) + kernel 1 MiB writer (nj1), 600 s concurrent, one
mount, probe hot throughout (169,305 probe serves; 168,693 ceremony
admissions; engagement closure EXACT: 350,920 fast + 16,265,350
direct + 2,530 handoffs ≡ 16,618,800 il ops). il job 27.7k IOPS
sustained; writer 358 MiB/s; clean umount. Wedge indicators:
`fuse_op_watchdog_overdue` 0, `ipc_sessions_poisoned` 0,
`ipc_descriptor_rejects` 0, `write_pipeline_fence_drops` 0,
`fsck_findings` 0. `transport_lease_overlong` read **21/600 s** — the
documented loud-never-fatal write-backpressure tripwire
(ingest-economy 2026-07-28), and it is NOT probe-attributable: a 150 s
control soak of the same shape on the DEV-TIP pair read **11/150 s**
(≈ 44/600 s — the control is worse per unit time; the probe touches
only the read path; no reply was lost — fio completed, watchdog 0,
umount clean). Venue mechanism: the aged zram store + a saturating
1 MiB writer legitimately push write-handler invocations past 1 s.
(Side note, load shape not wedge: the concurrent kernel-lane rand-4k
job was starved to ~171 IOPS by the il job + writer at this venue's
saturation — it completed normally.)

## 6. Residuals

1. **Field acceptance bracket** (epoch-blocked, owed): the standing
   squeeze-test store predates bit 6 (`KV_DYNAMIC_ROUTING`) and every
   post-`8eb3d73` binary refuses it loud pre-write (measured live,
   §2). At the next reformat/reshape window: deploy the retained pair
   (`/scratch/tmp/*.holdprobe`, sha-verified), run the chartered
   A-B-B-A vs a bit-6-epoch control + A0 + soak, journal, restore.
   Acceptance: il ≥ kernel randread on same state, engagement-exact,
   kernel rows untouched, warm/seq no-regression, settle hygiene.
   NOTE for that window: the dev-tip CONTROL must also be a bit-6
   binary (`f6532fd` works) — the pre-campaign `109d7bc` pair cannot
   mount a reformatted store, so before/after brackets against it are
   impossible on one store; the A/B lever there is `f6532fd` vs
   `dd6c7ea` (this campaign's only delta on top of dev tip).
2. The sync fast path still has no `read_lru` leg (the ≤ 256 KiB
   population) — out of this charter's scope; the hold was the named
   term.
3. Venue note for future local brackets: per-leg reformat does NOT
   reset zram aging; recreate the substrate between sequences (this
   session did) and treat cross-pair absolutes as order-contaminated.
