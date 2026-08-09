# Approach A — the FUSE placed-merge assembly (bounded falsification, 2026-08-09)

Branch `perf/fuse-placed-merge` off dev `71f2e967` (the write-bandwidth
program adjudication — rc-manifest §3f is this campaign's law). Charter:
port the IPC placed-sever design to FUSE delivery so the armed 1 MiB
streaming path stops paying an extraction destination (slot → memfd
bounce) before its real accumulation destination (NT merge →
`ActiveBlockBuf`); WRITE_FIXED cannot copy fixed→fixed, so the assembly
is a **memfd whose mmap view is adopted as the ActiveBlockBuf backing**.
Bounded falsification: exact engagement without throughput conversion is
a REPORTED falsification that redirects to Approach B.

## 1. Step 0 — the mandatory red gate (term confirmed on `71f2e967`)

Venue: tcp devsub (nvmet-tcp on lo, 4× nullb meta + 4× 8 GiB zram data),
armed default mount, fio 3.42 libaio direct=1 `bs=1M iodepth=8 numjobs=8
nrfiles=4 size=512m runtime=45 ramp=0` (one accounting window — device
deltas and fio io_bytes must share it). Rig:
`.benchmarks/rigs/2026-08-09-placed-merge-step0.sh` (all stats reads are
python over the `.stats` JSON — the colon-space law; artifacts
`/run/sqz-placed/step0-red2`, persisted copy in the artifacts dir).

| gauge | value | verdict |
|---|---|---|
| row | io 60.18 GB, **1.332 GB/s** | the red baseline |
| `fuse3_zc_write_extract_bytes` | 60,181,970,944 = **100.0 %** of user bytes | ✓ every streaming byte pays the bounce destination |
| `fuse3_zc_write_direct_bytes` | 0 = **0.0 %** | ✓ the direct vehicle never engages on streaming |
| `nt_copy_bytes` | 60,148,416,512 = **99.9 %** | ✓ every byte pays the second (NT merge) copy |
| placed severs / adoptions / elides | 0 / 0 / 0 | kernel lane has no placed machinery (the term this campaign builds) |
| `write_pipeline_phase_ns.dma` mean | 115.7 ms (n=14.8k) | saturation queue residence (baseline attribution) |
| `write_pipeline_phase_ns.total` mean | 295.0 ms | 〃 |
| `write_transport_phase_ns.transport_total` mean | 43.9 ms (queue_wait 5.5 µs) | 〃 |
| amplification / wareq-sz | **1.044** / 4,089 KiB | ✓ ≤ 1.05, no request-size collapse |
| `data_write_lanes` | 8 | — |
| `data_write_lane_submits` | 4 devices × 8 lanes, **all 8 moving per device, max share 13–14 %** | ✓ **SPREAD — the campaign-stop precondition passes** |

## 2. The design — cohort-capture mechanics

The IPC placed-sever design ported to FUSE delivery (the adjudication's
eight steps):

1. **Root gate** (`Filesystem::zc_write_place`, sync/lock-free on the
   queue worker): page-aligned offset+len, single block, NOT the W2
   small class (`len × 4 ≥ block_size` — the extent path's own `small`
   predicate inverted, so small-write rows can never inflate into
   full-buffer backings), cache-less v1 bound (striped routing is then
   structural), and **no live overlay entry** — the §5.2 isolation
   law's ROUTING face: a block whose (possibly adopted) overlay is
   parked never accepts another placement.
2. **Claim**: `PlacedSeverRegistry::begin_placement` — the existing
   page-granular `PlacedClaims` bitmap; the writer window OPENS AT
   SUBMIT and closes at the bridge CQE (once-only `end_write`), so the
   loom-modeled seal Dekker gives the isolation law for free:
   `take_for_adoption` refuses while any bridge write is in flight, and
   an ADOPTED assembly leaves the registry — no later bridge can target
   snapshot-visible memory (unit pins:
   `placement_writer_window_blocks_adoption`,
   `placement_payload_is_the_memfd_region`).
3. **The assembly is a memfd** (`SharedBlock::alloc_memfd` — a new
   `BlockBacking::Memfd` arm of the overlay allocation type):
   `MAP_SHA`1cbfb53b` (red: the three live contracts — verified red against
`71f2e967` on the missing placement ledger) → `a3dce222` (the build;
the isolation-law unit pins landed red-first inside its dev loop —
`placement_writer_window_blocks_adoption` failed until the
begin-at-submit/end-at-CQE writer window and registry-removal law were
wired). Live green (armed 7.1-sqz, root): the barrier-cohort
places/adopts/elides byte-exact with single-vehicle accounting; the
post-adoption routing face (kept-open partial-cohort venue — a covering
cohort write-throughs and retires the entry, which is itself the law
working); the lever-off control. Blast-radius sanity green:
write_pipeline ×2, write_through_coverage, fsync_writeback_tail_loss
(P0), fuse_zc_write + fusion, rw5a_never_lossy, metrics, env-knob
convention; fork 160; both-workspace clippy `-D warnings` + fmt.|MAP_POPULATE` mmap, fd-reachable for the bridge and
   VA-reachable for adoption/payload views (the ZcBounce dual-face law
   per block). WRITE_FIXED cannot copy fixed→fixed — this is the
   corrected form.
4. **Bridge**: the OWNING queue worker pushes
   `WRITE_FIXED(slot → assembly fd @ rel)` instead of the bounce
   extraction; the pend (`ZcPend::PlacedWrite`) rides the
   bounded-outcome `BridgeDeadlines` ladder and the drop-CQE seam.
5. **Cohort quiescence** (`PlacedCohort`, pool-level): a completed
   placement's dispatch HOLDS until no sibling bridge on the assembly
   is in flight, so the first merge's adoption finds the cohort
   claim-complete (whole-cohort capture is the design bar).
6. **Dispatch** carries the assembly-region payload (`Bytes` over the
   mmap view at `rel` — the pointer-proof input).
7. **Merge**: the EXISTING `record_write` coverage union + the existing
   pointer-proof adoption/elision (`ActiveBlockBuf::adopted` /
   `placed_merge_elides`) — never a second union.
8. **Failure ladder**: short/errored bridge CQE (or a synthesized loss)
   falls back to the at-delivery extraction for the SAME ent (payload
   intact in the slot until COMMIT), counted
   (`fuse3_zc_write_place_fallbacks`); teardown balances writer windows
   and releases claims; owed slots ride the row-8 drain.

Engagement: `fuse3_zc_write_placements`/`_bytes`/`_place_fallbacks` +
`placed_fuse_claims`/`placed_adoption_refusals` (the cohort-break
gauge); assemblies ride the existing `placed_assembly_bytes` gauge, R5
`placed_assemblies` component and cap. Lever
`SQUEEZEFS_FUSE_PLACED_MERGE` (registry + convention test).

## 3. Red contracts

`1cbfb53b` (red: the three live contracts — verified red against
`71f2e967` on the missing placement ledger) → `a3dce222` (the build;
the isolation-law unit pins landed red-first inside its dev loop —
`placement_writer_window_blocks_adoption` failed until the
begin-at-submit/end-at-CQE writer window and registry-removal law were
wired). Live green (armed 7.1-sqz, root): the barrier-cohort
places/adopts/elides byte-exact with single-vehicle accounting; the
post-adoption routing face (kept-open partial-cohort venue — a covering
cohort write-throughs and retires the entry, which is itself the law
working); the lever-off control. Blast-radius sanity green:
write_pipeline ×2, write_through_coverage, fsync_writeback_tail_loss
(P0), fuse_zc_write + fusion, rw5a_never_lossy, metrics, env-knob
convention; fork 160; both-workspace clippy `-D warnings` + fmt.

## 4. Brackets

All rows: fixed binary `a3dce222`, tcp devsub, RESET-PER-LEG (fresh
format — the venue ages), fio libaio direct=1, 60 s sustained, ramp 0,
engagement + ledger + tripwire gates FATAL (`f2`, the from-zero counted
pass after two rig-gate re-derivations: the cross-window amp instrument
and the small-row amp floor — posture-independence probe: place-off
measured amp 1.086 on the same randw4k venue vs 1.055 place-on, so the
1.05 line is the hole shape's spill floor, not a placement term). P0
smoke per leg. Artifacts:
`~/tmp/sqz-placedmerge-artifacts-2026-08-09/{step0-red2,f2,probes}`.

### 4.1 The streaming row (seqwr 1 MiB ×8 jobs ×4 files, qd8)

| leg | posture | GB/s | placement share | extract | nt_copy | fallbacks | adoption_refusals |
|---|---|---|---|---|---|---|---|
| A1 | armed+place | 1.228 | **25.2 %** | 74.8 % | 75.0 % | 0 | 17 of 17,785 |
| B1 | armed, place=0 | 1.200 | 0 | 100 % | 100 % | 0 | 0 |
| B2 | armed, place=0 | 1.189 | 0 | 100 % | 100 % | 0 | 0 |
| A2 | armed+place | 1.011 | **25.2 %** | 74.8 % | 75.0 % | 0 | 8 of 14,664 |
| U1 | unarmed | 1.232 | 0 | 0 (kmbuf) | 100 % | — | — |
| U2 | unarmed | 1.160 | 0 | 0 | 100 % | — | — |

A-vs-B: brackets **1.023× / 0.850×** (0.937× at median — the A2 leg is
the late-bracket store-decay face every campaign on this venue shows);
A-vs-unarmed 0.997×/0.872×. Mechanism health is EXACT throughout
(fallbacks 0, adoptions ≈ placements, amp ≤ 1.05, wareq ≈ 4 MiB,
tripwires 0) — and the design bar is falsified byte-exactly at the
adjudication's own named number: **25.2 % = first-chunk-only**.

### 4.2 The W2 small-write row (randwrite 4k ×16 jobs, qd8)

Placements ≡ 0 on every leg (the `len × 4 ≥ block_size` gate);
`parked_full_buffer_bytes` deltas are posture-identical (the
pre-existing escalation class) — **no full-buffer inflation**. Amp
1.006–1.086 across ALL postures incl. place-off and unarmed (the
fresh-store hole shape's spill floor; gated at 1.10 with the probe
recorded).

### 4.3 The capture-mechanism probes (why 25 % is a CEILING here)

| shape | capture | why |
|---|---|---|
| growth stream (fio seq / dd, extending O_DIRECT) | **25.1 %**, adoption_refusals 0 | the kernel takes `i_rwsem` EXCLUSIVE for extending direct writes — chunk n+1 is not even DELIVERED until chunk n replies: cohorts of ONE, capture = the block's first chunk exactly |
| non-extending overwrite stream (parallel direct writes live) | **29.1 %**, refusals 109 | deliveries overlap across the qd window, but the first merge parks the overlay before the block's siblings arrive; the isolation-law routing gate then (correctly) refuses them |
| buffered streaming (dd + writeback) | **0.1 %** | the per-inode flusher serializes writeback WRITEs end-to-end |

The premise the port needed — sibling chunks of one block delivered
CONCURRENTLY — is destroyed upstream of the transport by per-inode
write serialization. The IPC ring lane never sees this because the shim
bypasses the kernel inode lock entirely (ring submissions from the
application, severed synchronously at dequeue); the FUSE lane cannot.
The one lever that would raise capture — placing into blocks with live
overlays — is by definition the §5.2 isolation violation this design
exists to prevent.

## 5. Verdict

**FALSIFIED → Approach B** (the bounded-falsification charter's honest
exit, a success of the campaign):

* The whole-cohort-capture design bar (≥ 90–95 %) is unreachable on
  every streaming shape POSIX can deliver through FUSE on this kernel:
  capture ceilings at 25.1/29.1/0.1 % with the mechanism named
  (per-inode write serialization upstream of delivery), not a tuning
  residue — `adoption_refusals ≈ 0` proves the quiescence machinery is
  not the limiter; the deliveries simply never coexist.
* Throughput does not convert (A/B 1.023×/0.850×, median 0.937×) —
  extract/nt fall only by the placed 25 %.
* **The redirect evidence for Approach B is exactly this**: B's
  device-backed visible overlay needs NO cohort — each chunk's direct
  store targets its own device offset via `zc_write_fd` on the FUSE
  queue rings, per-chunk, serialization-immune; the memfd
  assembly/writer-window/registry machinery built here (and the §5.2
  writer-window law's unit pins) carry forward as B's building blocks.
* Default per D17 (the measured posture): `SQUEEZEFS_FUSE_PLACED_MERGE`
  ships **OFF** (`1e3663ce`); the lever stays the counted A/B
  instrument with correctness green (the live suite arms it
  explicitly).

**Field spec (next window; squeeze-test untouched this campaign):** one
armed leg of `.benchmarks/rigs/2026-08-09-placed-merge-bracket.sh`
(LEGS="A1:1:1 B1:1:0", PM_DESIGN_BAR=report) on the field host purely
as a confirmation row that the capture ceiling reproduces at ~330 µs
RTT (expected: identical 25 % class — the serialization is kernel-side,
not latency-side). No acceptance rides on it; Approach B is the
program's next build.

## 6. Gates

* Step-0 red gate PASSED before any build (§1) — term confirmed, lanes
  spread (no campaign stop).
* Red-first: `1cbfb53b` verified red (missing placement ledger, live
  armed venue); the §5.2 isolation unit pin landed red inside the impl
  loop.
* **×10 blast radius (consecutive, final binary `1e3663ce`, root, live
  armed 7.1-sqz): ALL GREEN** — {fuse_zc_write_place (3 contracts),
  write_pipeline, write_through_coverage, fsync_writeback_tail_loss
  (P0), fuse_zc_write, fuse_zc_write_fusion, rw5a_never_lossy} × 10 =
  70 suite executions, zero failures.
* Both workspaces `clippy --all-targets -D warnings` (root also
  `--all-features`) + `fmt --check` clean; fork suite 160; metrics +
  env-knob convention green (registry entry updated with the verdict).
* No new loom model owed: the claims/seal Dekker is the EXISTING
  loom-modeled `placed_core` (the placement reuses `begin_claim`/
  `end_write`/`seal_for_adoption` verbatim — only the writer window's
  DURATION changed, from a sync memcpy to submit→CQE); the cohort map
  is a mutex.
* Deviations recorded: the counted bracket restarted from zero twice
  for rig-gate re-derivations (cross-window amp accounting; the
  small-row posture-independent amp floor — probes in-note); one stale
  writer-claim recovery via the attested `claim clear` after a pkill'd
  probe daemon (dead-pid proof + TTL expiry — the D0 ladder working);
  venue left clean (mounts torn down, tcp devsub healthy, no netem
  this campaign).
