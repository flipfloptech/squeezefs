# 2026-09-05 — W-4: the reclaim derivation (event-driven at-cap park; derived queue cap + park bound)

**Branch** `perf/reclaim-derivation` (worktree off dev `27a396e1`). RED
`dc60dff2` → mechanism `5efd45d9` → refinements + docs (this note's
commit). Campaign: `docs/design-e2e-perf-audit.md` §3.3 ladder **row 14**
(write board **#8** — "reclaim constants at fleet rates"), discharging the
write-wall campaign's standing OQ-5 (`.benchmarks/2026-07-31-write-wall.md`
§7: "`SQUEEZEFS_RECLAIM_CAP_PARK_MS` (default 1000) and the cap (4096
blocks) are liveness bounds, not measurements — derive the cap from
drain-rate × acceptable-lag"). Contracts `tests/async_block_reclaim_tests.rs`
§14–16; tie tests `tests/derivation_sweep_tests.rs` §W-4. Target release:
**1.2.1**.

Status: **mechanism landed, in-process rows measured, FIELD ROWS OWED**
(§5).

## 1. The finding

The 2026-09-01 field rewrite rows (4 MiB blocks, ~19 GB/s) carried a tail
fingerprint the memory of that session recorded verbatim: **`w_rewrite`
p99.9 ≈ 893 ms / max 3.08 s — multiples of the reclaim queue's 1-second
at-capacity park**. Two constants in `src/block_reclaim.rs` produced it:

| Constant | Shipped value | Why it was the tail |
|---|---|---|
| `SQUEEZEFS_RECLAIM_QUEUE_MAX_BLOCKS` | 4096 | the deferred-space budget; at ~4,750 displaced blocks/s (19 GB/s ÷ 4 MiB) it fills in ≈ 0.9 s, at the 1 MiB-block shape in ≈ 0.2 s |
| `SQUEEZEFS_RECLAIM_CAP_PARK_MS` | 1000 | the at-cap park polled a 5 ms tick until room OR this bound; a parked producer is the **detached pipeline upload task holding its `PipelinePermit`** (`fuse_client.rs` `pipeline_upload_parked_block` → `free_block` on the displaced keys, `displaced_free` phase), so in the pinned regime (displacement > the target's deallocate service — write-wall E2: ~1,700–1,800 cmd/s under load at ANY client width) every permit sat parked for the full bound and the pipeline's service time WAS the quantum |

Both were filed at the write-wall campaign as "time horizon, not a
resource cap" with the derivation owed. Two further shapes the read of
the mechanism turned up, both fixed here because they sat under the
same tail:

- **A pop opened room before any block was reclaimed.** The cap counted
  `len` (queued only); `take_batch` moved up to `batch × lanes = 2048`
  entries into `processing` at pass start, so 2048 slots opened at once
  while the drain had returned nothing, the queue refilled in ~0.1 s at
  fleet rates, and every later producer parked until the NEXT pass
  start — the whole pass wall (≈ 1.1 s under target-bound load: 2048
  blocks at ~1,800/s). The `queue_bytes` gauge had always counted
  queued + in-flight; the cap did not.
- **The park's tick sat behind the worker's deferral tick.** A park's
  `notify_one` stored a permit the worker consumed only after its 50 ms
  manners sleep ended, so a bound below ~50 ms + room latency would have
  tripped spuriously — the reason any derived bound needed the worker
  to be wakeable at cap.

## 2. The mechanism

`src/block_reclaim.rs` (module doc §"The park is EVENT-DRIVEN and both
liveness constants DERIVE"):

1. **Population = queued + in-flight.** `ReclaimQueue::population()` =
   `len + processing`; the enqueue cap check and the worker's `at_cap`
   read it. The cap now governs exactly the deferred-space count the
   byte gauge reports.
2. **Room edges per coalesced range.** `process_entries` coalesces the
   sorted device group, then for each range: `issue_range` (one device
   command) → `finish_free` its k blocks → `processing -= k` →
   `note_drained(k)` → `room.notify_waiters()`. Under target-bound load
   room flows at the drain rate (one edge per command) instead of one
   2048-slot burst per pass. The `BatchGuard`'s unwind, a cap RAISE, a
   bound SHRINK and the fence latch are edges too (every population
   decrement and every re-check-relevant change passes through
   `room_made`).
3. **The park is event-driven.** `park_at_cap` mirrors the write
   pipeline's PERF-13 admission park verbatim: `room.notified_raw()` →
   `enable()` → re-check (`population < cap || fence_halted`) → `race2(edge,
   sleep(min(remaining, 50 ms)))`. The 50 ms tick is only the lost-edge
   backstop and is counted (`block_free_reclaim_park_tick_wakes`, ≈ 0
   while room flows). The bound is measured from the park's START
   (re-parks never reset it) and **re-read live on every wake** — a park
   that began cold collapses to the derived bound the instant it is
   learned (§4 shows why that mattered).
4. **The at-cap park wakes a deferred worker.** A dedicated `cap_wake`
   Notify, signalled ONLY by the park path (a per-enqueue signal would
   re-evaluate manners 19k×/s), races the worker's 50 ms deferral sleep.
   The manners law itself is untouched: below cap + foreground ⇒ defer;
   at cap ⇒ drain regardless; idle ⇒ full width. The two contracts that
   exercise deferral (12, 12b) and the parked-relief contract (13) are
   green unchanged.
5. **The issue engine split** into `open_reclaim_target` (open + classify
   once per device group) and `issue_range` (one command + the per-block
   ledger); the trim venue keeps `issue_device_ranges` over them. Per-
   block counting, the fence-once-per-batch law, pop-ownership and the
   `finish_free`-after-reclaim law are byte-for-byte the same.

Nothing lock-free changed: the `SegQueue` pop/`processing` protocol is
untouched; the edges are `sqz_notify` + atomics (no loom model needed —
the same posture the write-wall iteration recorded for manners/park).

## 3. The derivation law

Pure functions in `src/block_reclaim.rs`, tied on the field/floor shapes
in `tests/derivation_sweep_tests.rs`:

```
room_ms      = batch_blocks × 1000 ÷ drain_rate          (cold: 1000 = the shipped assumption)
park_bound   = clamp(4 × room_ms,            50 ms,  1000 ms)
queue_cap    = clamp(arrival_rate × room_ms ÷ 1000, 4096, budget/1024 ÷ entry_ram ≤ 2^20)
```

| Term | What it is | How it is measured |
|---|---|---|
| `drain_rate` (blocks/s) | the drain's aggregate room-making rate | EWMA (α ¼) of blocks `finish_free`d per window at the room edge — first window one manners tick (50 ms), then 250 ms (the write pipeline's `WINDOW_MS` scale). Exported `block_free_reclaim_drain_rate`. **Width-blind by construction**: under target-bound load the aggregate is what the target delivers at any client width (write-wall E2), so it never inflates with the lane count the way a per-lane wall does — a per-lane service time at width 32 reads ≈ 1.1 s on the fleet and would have derived the shipped 1 s right back |
| `room_ms` | the time the drain needs to free ONE batch of slots | `batch ÷ drain_rate`; fleet under load (~1,800/s, batch 64) = 35 ms; idle catch-up (~5,900/s) = 10 ms |
| `arrival_rate` (blocks/s) | the write side's displacement rate | the queue's own enqueue counter, sampled by the WORKER at every inner-loop iteration (deferral ticks and passes alike) and peak-held with the pipeline's `rolled_bw_peak` law (a storm registers on its first window, a lull relaxes over ~8). No clock on the enqueue path. `queued ≡ displaced blocks` is the ledger identity, so this IS the displacement rate. Before the first sample: the write pipeline's Σ `bw_peak` in blocks (`WritePipeline::peak_bandwidth_bps` ÷ block size, wired at mount as `set_displacement_seed`) — an upper bound (fresh writes displace nothing), so the seeded cap errs large within its RAM ceiling. Exported `block_free_reclaim_arrival_rate` |
| park bound | the stalled-drain SAFETY bound | ×4: a producer that saw no edge in four batch-times is waiting on a stalled drain, not a slow one — room is made per range, so a live drain at any rate frees a batch within one batch-time. Floor 50 ms = the manners tick (the worker's coarsest guaranteed cadence; `cap_wake` short-circuits it, but a bound below it could trip on a healthy worker whose wake sat behind a saturated blocking pool). **Ceiling 1000 ms = the shipped constant**: the derived bound never parks a producer longer than the shipped posture did — never-regress applied to a tail bound — and is the cold value. Fleet under load: **140 ms**. Exported `block_free_reclaim_park_bound_ms` |
| queue cap | the blocks displaced while the drain makes one batch of room | the buffer a keeping-pace drain needs so producers never park. Floor 4096 = the shipped posture. **Ceiling is RAM**: entries are bookkeeping (`size_of::<ReclaimEntry>()` + the device-path heap + the `SegQueue` slot ≈ 104 B; the deferred BYTES live on the device and are gauged as `queue_bytes`), so the queue may hold budget/1024 — 0.1 %, negligible against every gauged data component by construction and never worth an R5 component — capped at the registry's 2^20 admissible maximum. Field shape (176 GiB budget) ⇒ 2^20; floor box (2.8 GiB) ⇒ ~28 k. Exported `block_free_reclaim_queue_cap` |

**What the arithmetic says about the fleet.** At 4 MiB blocks the cap
derives to the FLOOR: ~4,750 blocks/s × 35 ms ≈ 166 ≪ 4096. That is the
honest verdict, not a failure of the derivation: **the cap was never the
fleet's lever** — with displacement above the target's deallocate service
no finite cap avoids the pinned regime, it only delays it by
`cap ÷ (arrival − drain)` seconds. The cap lifts above the floor on the
shapes where it IS the lever (small-block volumes: 64 KiB blocks at the
same bandwidth ≈ 300k/s × 35 ms = 10,500; slow drains: room 640 ms ×
19k/s ≈ 12k). What fixes the fleet tail is the **bound** (1000 → 140 ms
there) composed with the **room edge** (a producer that CAN get room gets
it at the drain rate, not at the next pass start). The pinned regime's
steady state is unchanged in kind — `cap_overflow` becomes the NORM there
(each expiry costs the write path exactly one bound; the queue grows past
the cap RAM-only, conservation preserved by the valve/unmount/idle drains,
the thin-space face governed by finding 40's supply-pressure arm and the
ENOSPC valve exactly as before) — and changed in degree: the price of an
overflow drops from 1 s to the derived bound.

Precedence: explicit `SQUEEZEFS_RECLAIM_QUEUE_MAX_BLOCKS` /
`SQUEEZEFS_RECLAIM_CAP_PARK_MS` win verbatim (`4096` / `1000` restore the
shipped constants exactly — the A/B levers; `0` for the bound = never
park); a malformed or out-of-range value refuses at startup (ENG-10) and
falls through to the derivation in-process (`resolve_*`). Registry
defaults now read `derived (floor 4096)` / `derived (50..1000)`; the
`documented_defaults_match_the_shipped_ones` pin was updated the way the
two earlier intentional changes in that list were.

## 4. In-process rows (release, `cargo test --release`, thin-LTO dev profile)

Rig: `reclaim_park_rows` (`tests/async_block_reclaim_tests.rs`, `#[ignore]`
— run explicitly with `--ignored --nocapture`). 32 concurrent producers ×
24 terminal frees against cap 64 (pinned so the pinned regime is
reachable in-process), a SERIAL drain (`LANES_PER_DEV=1`, batch 8) priced
at 20 ms per lane batch by the stall seam = 400 blocks/s — the fleet's
shape (displacement ≫ drain) in miniature; foreground moving through the
storm, idle after. A = `SQUEEZEFS_RECLAIM_CAP_PARK_MS=1000` (the shipped
constant), B = unset (derived). Per-free wall = the producer-visible park.
A-B-B-A, one process, same box (shared with four sibling campaigns).

| Leg | frees / wall | park wall p50 | p99 | **p99.9** | max | `cap_parks` | `cap_overflow` | `drain_rate` | `park_bound_ms` |
|---|---|---|---|---|---|---|---|---|---|
| A pinned 1000 | 768 / 1.774 s | 5 µs | 847 ms | **1,000 ms** | 1,000 ms | 240 | 5 | 399 | 1000 |
| B derived | 768 / 1.774 s | 76 ms | 80.6 ms | **80.6 ms** | 80.6 ms | 678 | 618 | 404 | 76 |
| B derived | 768 / 1.775 s | 76 ms | 80.8 ms | **80.8 ms** | 80.8 ms | 676 | 626 | 405 | 76 |
| A pinned 1000 | 768 / 1.774 s | 4 µs | 886 ms | **1,000 ms** | 1,000 ms | 240 | 5 | 405 | 1000 |

Reading:

- **The tail IS the bound, both legs.** A: p99.9 = max = 1,000 ms (the
  shipped constant, exactly the field fingerprint's shape). B: p99.9 =
  max = 80.6–80.8 ms = the derived bound (`4 × 8 ÷ 400 = 80 ms`; the gauge
  read 76 as the EWMA settled). **12.4× shorter**, order-independent.
- **The instrument is exact**: `drain_rate` 399–405 blocks/s against the
  seam's 400; the bound gauge equals the arithmetic.
- **The designed tradeoff is visible**: B's `cap_overflow` 618–626 vs A's
  5 — in the pinned regime the shipped bound made producers wait ~1 s
  for room that arrived at 400/s; the derived bound lets them soft-
  overflow at 80 ms. Row wall is identical (1.774–1.775 s) because the
  drain — not the park — bounds the row; what moved is where the
  producers' time went.
- **Two refinements the first row shape forced**: (a) the park read its
  bound once at start, so parks that began COLD carried 1 s even after
  the live bound derived to 80 ms — B's first shape read p99.9 = 1,000 ms
  with `bound_ms 80` on the same line; the bound is now re-read on every
  wake and a shrink is a room edge → p99.9 285 ms; (b) the first
  drain-rate window was 250 ms, so ~285 ms of cold parks preceded the
  first estimate; the first window is now one manners tick (50 ms) →
  p99.9 80.8 ms. Both are in the mechanism commit's successor (this
  note's).

Contract timings (release, alone): `at_cap_park_ends_on_the_room_made_edge`
2.05 s (a 1 s-stalled lane + three drain rounds), `derived_park_bound_never_
overflows` 0.47 s, `derived_queue_cap_never_regresses` 0.01 s, contract 13
`at_cap_enqueue_parks_until_drain_relieves` 0.25 s, contract 10
`displacement_storm_never_caps_queue` 0.51 s; whole suite 7.05 s (21
passed, 1 ignored).

## 5. Field rows — OWED

`w_rewrite` A-B-B-A on the **tcp devsub** (fabric-sensitive row — the
two-substrate rule), SAME binary, `SQUEEZEFS_RECLAIM_QUEUE_MAX_BLOCKS=4096
SQUEEZEFS_RECLAIM_CAP_PARK_MS=1000` (the shipped constants, pinned) vs
both unset (derived). Columns per leg: GiB/s sustained ≥ 60 s, p99.9,
`block_free_reclaim_cap_parks`, `block_free_reclaim_cap_overflow`,
`block_free_reclaim_drain_rate`, `block_free_reclaim_arrival_rate`,
`block_free_reclaim_park_bound_ms`, `block_free_reclaim_queue_cap`,
`block_free_reclaim_park_tick_wakes` (≈ 0 is the lost-wake tripwire),
`write_pipeline_phase_ns.displaced_free` (the park's residence face) and
`write_pipeline_admission_waits`. Instrument stated per row (the standing
lesson); amplification columns (device bytes ÷ user bytes, `wareq-sz`,
`block_free_*`) per the write-row requirement. Expected shape from §3–4:
p99.9 collapses from the ~1 s multiple to ≈ the derived bound (fleet
under load ≈ 140 ms), `cap_overflow` rises in the pinned regime, ingest
par-or-better (the permit is released ~7× sooner). Then the same bracket
on the field cluster the 2026-09-01 fingerprint came from. The PARENT
runs these.

## 6. Gates (final tree)

- `cargo fmt --check` PASS; `cargo clippy --all-targets --all-features -- -D
  warnings` PASS; `cargo clippy --all-targets -- -D warnings` (the shipped
  config) PASS.
- `async_block_reclaim_tests` 21/21 (+1 ignored rows harness), debug and
  release; `derivation_sweep_tests` 42/42; `env_knob_convention_tests`
  21/21; `no_tokio_convention_tests` 2/2; `write_through_coverage_tests`
  8/8; `discard_elision_tests` 8/8; `block_free_reclaim_tests` 6/6.
- Counted **20× green** on the final release binary (restarted from zero
  after the §4 refinements), each run = contracts 14–16 + contract 10
  (`displacement_storm`) + contract 13 (`at_cap_enqueue_parks`).
- Loom: not run — no lock-free core changed (`SegQueue` pop/`processing`
  protocol untouched; the edges are `sqz_notify` + atomics).
- NOT run here (four sibling campaigns share the box, per the brief):
  `task check`, root rigs, fstests. The full gate is the merge's.

## 7. Open

- The **pinned-regime economics** are now explicit: overflow at the
  derived bound vs park to the drain rate. The alternative posture —
  never overflow, i.e. throttle the rewrite honestly to the target's
  deallocate service (~7 GB/s on the write-wall cluster) with a tail of
  `pipeline depth ÷ drain rate` — is a one-line lever (bound → ∞) and a
  field question, not decided here.
- `ReclaimEntry` RAM is estimated (104 B); a measured allocator census
  would sharpen the ceiling, which only binds on tiny-budget boxes.
- The DebtDrainer's own 50 ms tick + `IDLE_CONFIRM_TICKS` literal were
  left as they were (out of scope).
