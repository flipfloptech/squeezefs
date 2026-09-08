# 2026-09-08 — W-5: the fsync economy (`fsync_phase_ns`; touched-namespace barriers; joined legs)

**Branch** `perf/fsync-economy` (worktree off dev `36d517f3`). RED
`741f0b8f` → mechanism `36668b7c` → rig `aca595c3` → docs (this note's
commit).
Campaign: `docs/design-e2e-perf-audit.md` §3.3 ladder **row 15** (write
board **#9** — "fsync flushes ALL data namespaces + 3 serialized meta
legs; no `fsync_phase_ns`"). Contracts `tests/fsync_economy_tests.rs`
(11 pinned + the `fsync_storm_rows` rig); the durability matrix
(`tests/durability_matrix_tests.rs`, power-cut harness) re-run green
under the new default. Target release: **1.2.1**.

Status: **LANDED — instrument + both levers default ON on in-process
evidence (§4). The `w_durable` + small-file fsync-storm row on
squeeze-test (B-9, tcp + field) is the parent's and is OWED; nothing in
this note is a field claim.**

## 1. The ledger's finding, read at the code

The audit ledger's row said three things about `fsync`
(`src/fuse_client.rs` `async fn fsync`); all three held at `36d517f3`:

| Claim | At the code |
|---|---|
| **flushes ALL data namespaces** | `flush_inode_to_backend` → `BackendRouter::flush_data_devices()` → `for dev in distinct_data_devices() { dev.flush().await? }` — every registered data device, serially, one `NvmeBlockDev::flush` (cross-lane drain + one io_uring `Fsync{DATASYNC}`, coalesced per device by its `SyncCoalescer`) each, regardless of where the ino's blocks live and regardless of `data_volume_write_cache` (a write-through device paid the drain + the round trip for a flush the kernel answers without a device command). A 1-block file on an 8-namespace field mount barriered 8 namespaces per fsync. A CLEAN fsync (nothing written since the last one) barriered all 8 too. |
| **3 serialized meta legs** | after the data barrier: `close_rewrite_epoch` → `persist_dirty_layout_if_needed` → `sync_device_for_ino` (the coalesced meta `fdatasync`), then — in the handler — the S11 `extent_ship::flush_ino` force, one after another; the S10 intent barrier ahead of everything. |
| **no `fsync_phase_ns`** | the handler carried `OpProf` (the gated `fuse_op_phase_ns` family) and nothing always-on; no instrument could say whether a slow fsync was the data flush, a device Fsync, the meta barrier or a co-writer's authority round trip. |

What the ledger did NOT say, and the read established before any lever
was cut: which orderings carry semantics.

- **Intent before everything** (S10 rung 13): the file's existence must
  be durable for its data durability to mean anything, and a DESTROYED
  mint's fsync must answer the owner's latched errno — the barrier's
  outcome short-circuits the op. One relaxed load on every mount
  without pending intents; the layout publish inside the flush
  barriers the same lane anyway (`publish::intent_barrier_inos`), so
  overlapping it would buy only the DMA leg of a co-writer's rare
  intent-pending fsync. **Kept first, sequential.**
- **Data barrier strictly before the meta legs that name the blocks**
  (DUR-1, `tests/fsync_durability_contract_tests.rs`
  `test_data_barrier_precedes_the_metadata_barrier`): on power loss the
  alternative is durable metadata naming a block whose bytes were still
  in the device's volatile cache. **Kept: the WHOLE barrier step — every
  leg `Ok` — precedes `close_rewrite_epoch`; a failed leg fails the
  fsync and the meta barrier never runs** (re-pinned on the new ladder:
  `a_failed_data_barrier_still_fails_the_fsync_before_the_meta_barrier`).
- **The meta barrier last** — it covers the publishes above it.
- **The per-namespace device Fsyncs and the staged-payload `sync_key`
  are mutually independent** (different devices / a different file), and
  **the S11 `FlushExtents` force is independent of the local ladder**
  (the retained extents' bytes are at the AUTHORITY; the local flush
  DMAs this mount's own blocks). These are the legs the lever joins.

## 2. The instrument — `fsync_phase_ns` (always-on, exact-sum)

`src/fsync_economy.rs` `FsyncPhase` / `FsyncProf`: the handler's steps as
CONSECUTIVE wall spans off shared boundary instants, so `Σ legs ≡ total`
to the nanosecond whatever the levers do, every phase recording once per
fsync (0 ns for an absent leg), recorded on drop on every exit:

| Phase | Span |
|---|---|
| `intent_barrier` | entry → the S10 barrier done |
| `data_flush` | → overlay drain + memory-buffer flush + staged active-block uploads done (every DMA this fsync owns has completed and published), lease acquire |
| `data_barrier` | → the barrier step done: staged-payload sync + one device Fsync per touched volatile namespace (joined: ≈ max(legs), not Σ) |
| `meta_publish` | → `close_rewrite_epoch` + `persist_dirty_layout_if_needed` (journal commits, no barrier) |
| `meta_barrier` | → `sync_device_for_ino` (the coalesced meta `fdatasync`) |
| `extent_barrier` | → the S11 force's RESIDUAL past the local ladder |
| `total` | entry → last boundary (≡ Σ) |

Counters (stats inode): `fsync_calls`, `fsync_noop_clean` (data legs
found nothing — no staged payload, no touched namespace; the meta barrier
still runs), `fsync_data_namespaces_touched` / `_flushed` (Σ per fsync —
against `fsync_calls × |data_volume_write_cache|` the measured "flushes
ALL namespaces" ratio), `fsync_write_through_skips`,
`fsync_touched_unresolved` (should stay 0), `fsync_parallel_joins`.
Cost: 6–7 `Instant::now()` + relaxed adds per fsync — invisible against
any barrier.

## 3. The levers

### 3.1 `SQUEEZEFS_FSYNC_TOUCHED_NAMESPACES` (default on)

**The set.** Per-ino "data namespaces written since the last covering
barrier" as ONE atomic word per stripe (`fsync_economy::TouchedTable`
over `stripe_locks::StripeLocks<AtomicU64>` — the splitmix64 mix and the
D-3 DLM width, 8 B/stripe = 128 KiB on the field box): low 32 bits are
per-device ORDINAL bits (`NvmeBlockDev::fsync_ordinal`, assigned by the
owning `BackendRouter` at registration; bit 31 = ALL, the unresolvable
fallback), high 32 a stamp generation. `stamp` = one CAS that ORs the
bits AND bumps the generation; `observe` before the barriers start;
`clear_observed` = CAS `observed → observed & !bits` after every leg
succeeded — a stamp that landed meanwhile changed the generation, the
clear fails, the bits stay for the next fsync (conservative, never a
missed barrier). A failed leg never clears.

**Why a SHARED stripe is correct here, not a compromise.** A device
barrier covers every write that completed before it started, whoever
issued it. Ino X stamps device A (after its DMA completed and its map
merged); ino Y in the same stripe fsyncs, observes A's bit, barriers A
(covering X's bytes), clears. X's later fsync finds the stripe clean and
skips A — correctly. The false-share cost is one extra COALESCED barrier
on Y's fsync; the collision rate is the dirty-population ÷ width.

**Where the stamps are (the completeness argument).** Every layout
publish — write-through, overlay settle, staged promotion, fold, mover
republish, clone, truncate's CoW edge, the S11 assembler's fold at the
authority — runs `DataRouter::block_ref_ops(ino, changes)` to translate
its `(index, key, taken)` delta into durable block-reference ops; the
stamp rides that ONE translation for every `taken` key. Its completeness
is the same property the fsck **C8** durable-vs-derived oracle proves
(a site whose delta the ledger missed drifts C8). Two DMA shapes change
no map key and are stamped at the DMA: the W1 sole-owner patch
(`try_sole_owner_patch`, and the il direct-drive patch in
`ipc_direct.rs`) and the in-place full-block overwrite
(`try_inplace_rewrite`). The durability matrix's power-cut rows (W1
patch, W2 fold, in-place overwrite, rewrite-shadow close, striped
write-through, striped via staging) run green under the lever — every
acked byte covered, `volatile_writes == 0` after fsync.

**Ordering.** The word is observed AFTER `flush_active_blocks_with_retry`
(every DMA this fsync owns is published — a detached pipeline upload
either finished under the block lock, merge → stamp → retire, or the
fsync stole the parked custody and uploaded it itself) and strictly
before any barrier starts. The stamp is `AcqRel`, the observe `Acquire`.

**Write-through namespaces skip the barrier** (`data_volume_write_cache`
= `write-through` ⇒ `!is_volatile()`): acknowledged writes are power-
safe on completion, and this fsync awaited every DMA it owns; a device
Fsync there paid the cross-lane drain + the ring round trip for a flush
the kernel answers without a device command. `file-backed` and
`unknown` stay volatile (conservative).

**Lever off** = the shipped shape verbatim: every listed device,
write-through included (`lever_off_barriers_every_namespace`).

### 3.2 `SQUEEZEFS_FSYNC_PARALLEL_LEGS` (default on)

The barrier step's legs — the staged `sync_key` and the per-namespace
`NvmeBlockDev::flush`es — are boxed and `join_all`ed; **every leg runs
to completion and the first error wins**. Never `try_join`: a cancelled
`flush()` future drops the device `SyncCoalescer`'s LEADER mid-barrier,
whose `LeaderGuard` then fails every OTHER fsync queued on that device
("leader dropped — outcome unknown"). On an S11 co-writer with retained
extents the `FlushExtents` force is `join!`ed with the local ladder; the
local outcome is answered first (the shipped precedence).
`fsync_parallel_joins` counts steps that joined ≥ 2 legs. Lever off =
the shipped serial order.

### 3.3 The meta barrier (c) — measured, not changed

An fsync storm's meta barriers collapse through the per-volume
`SyncCoalescer` (`meta_device_syncs` ≪ `meta_sync_requests`): 16
concurrent small-file fsyncs with a 20 ms seamed meta barrier request 16
barriers and issue < 8 (`an_fsync_storm_coalesces_its_meta_barriers`;
the in-process rows below issue 48–50 for 192 fsyncs on 8 streams — one
per stream-wide batch, 4 : 1). On the
deferred cadence (the default) the commit conveyor's durability lane
issues NO barrier of its own (`run_windows` step 10 is strict-only), so
fsync's `sync_device_for_ino` IS the meta barrier and the coalescer is
what amortizes it; on the strict cadence the lane's barrier and fsync's
share the same coalescer. Nothing to move here — pinned.

## 4. In-process rows (release, `cargo test --release`, thin-LTO dev profile)

Rig: `fsync_storm_rows` (`tests/fsync_economy_tests.rs`, `#[ignore]` —
`--ignored --nocapture`). Two data volumes (file-backed), one meta
volume; **192 two-block (128 KiB) files created + written + fsynced by 8
concurrent streams**; placement alternates the volumes by fill so every
fsync's blocks sit on ONE of the two namespaces; both data devices carry
a 3 ms seamed barrier latency (`dev_power_cut::arm_barrier_latency` — a
SLOW device, never a parked one; the flush class of a fabric namespace)
and the meta volume a 1 ms seamed `fdatasync`. A = both levers off (the
shipped shape), B = both on; A-B-B-A plus the two single-lever legs, one
process, same box. **Dev-box rows: scoping evidence** (the heat-soak
rule) — the acceptance pair is the box's.

Box: the dev box, load average 24–38 across the runs (shared), release
(thin-LTO, default features), two runs of the sequence; a warm-up leg
(the process's cold start) discarded. Per-fsync wall is the stream's
own clock around the handler call; the phase means are the family's
deltas ÷ 192.

| Leg (run 1 / run 2) | fsyncs/s | fsync p50 | p99 | `data_barrier` mean | `meta_barrier` mean | `total` mean | namespaces flushed (of 384) | device Fsyncs issued | meta Fsyncs issued |
|---|---|---|---|---|---|---|---|---|---|
| **A** shipped (off/off) | 459 / 445 | 16.4 / 16.6 ms | 16.9 / 17.8 ms | 13.19 / 13.46 ms | 2.98 / 3.07 ms | 16.18 / 16.55 ms | 384 / 384 | 96 / 96 | 48 / 48 |
| **B** levers (on/on) | **708 / 678** | **9.6 / 10.0 ms** | 10.4 / 10.3 ms | **6.75 / 6.91 ms** | 2.91 / 3.02 ms | 9.68 / 9.95 ms | 298 / 300 | 98 / 98 | 50 / 50 |
| **B** levers (on/on) | **671 / 701** | **10.0 / 10.0 ms** | 16.0 / 10.4 ms | **7.06 / 6.89 ms** | 3.06 / 3.06 ms | 10.13 / 9.97 ms | 282 / 266 | 104 / 96 | 57 / 48 |
| **A** shipped (off/off) | 421 / 441 | 16.7 / 17.0 ms | 32.5 / 18.0 ms | 13.75 / 13.22 ms | 3.81 / 3.25 ms | 17.58 / 16.48 ms | 384 / 384 | 96 / 99 | 48 / 50 |
| touched only (on/off) | 582 / 490 | 11.9 / 12.0 ms | 29.9 / 52.7 ms | 9.51 / 10.53 ms | 2.82 / 2.92 ms | 12.35 / 13.47 ms | 284 / 298 | 147 / 169 | 100 / 124 |
| parallel only (off/on) | 724 / 670 | 9.7 / 10.3 ms | 10.3 / 10.6 ms | 6.75 / 6.99 ms | 2.92 / 3.26 ms | 9.69 / 10.28 ms | 384 / 384 | 96 / 96 | 48 / 48 |

`intent_barrier` 0 µs, `data_flush` 13–15 µs, `meta_publish` 2–5 µs,
`extent_barrier` 1 µs on every leg; `fsync_touched_unresolved` 0;
`fsync_noop_clean` 0 (every fsync had fresh blocks — by construction of
the storm).

Reading:

- **The ladder is barrier wait.** On this venue 99.8 % of an fsync is
  `data_barrier + meta_barrier`; the flush, the publish and the intent
  barrier are tens of µs. The instrument names it; before W-5 nothing
  could.
- **A-B-B-A, both orders, both runs: B is +47–58 % fsyncs/s and −40 %
  p50** (16.4–17.0 → 9.6–10.0 ms), `data_barrier` −48 % (13.2–13.8 →
  6.75–7.06 ms). Two 3 ms devices barriered serially under 8 streams
  cost ≈ 2 × (3 ms + the coalescer's next-batch wait) ≈ 13.4 ms; joined
  they cost one device's ≈ 6.9 ms.
- **The physical barrier count is UNCHANGED** (96–104 device Fsyncs, 48–57
  meta Fsyncs per 192 fsyncs on every leg): the per-device
  `SyncCoalescer` already amortized the storm 4 : 1 (8 streams → one
  batch per stream-wide wave). What the levers move is each fsync's
  WAIT — how many coalesced waves it sits through — not how many
  commands the device sees. On a fabric namespace the wave is the round
  trip, which is the field's term.
- **Touched namespaces per fsync = 1.4–1.6 of 2**, not 1: a two-block file
  on two equal-fill volumes gets its blocks placed by the §5.9 rotor,
  which alternates per BLOCK, so half the files span both. The lever's
  saving scales with placement granularity: a sub-block-size small file
  (one 4 MiB block in the field) touches ONE of N namespaces and pays
  one round trip instead of N; a streaming file touches all N and the
  lever is inert (the joined legs carry that case).
- **The composed default is the right one.** "Touched only" (serialized
  legs) reads WORSE tails than the shipped shape (p99 30–53 ms, device
  Fsyncs 147–169 — the serialized all-namespace order had aligned the
  eight streams into one wave per device; barriering different subsets
  in series breaks the alignment into more, smaller waves); "parallel
  only" already recovers most of B on this shape because both
  namespaces are touched half the time. Both on: the joined step pays
  one wave whether one or two namespaces are touched, and a
  one-namespace fsync pays one round trip.

## 5. What is and is not claimed

- **Claimed**: the instrument (exact-sum, pinned), the two levers'
  correctness (11 contracts + the durability matrix's power-cut rows
  under the new default), and the in-process shape above: on a
  two-namespace mount a small-file fsync issues ONE device Fsync instead
  of two, a clean fsync issues none, the barrier step pays max not Σ.
- **Not claimed**: any field number. The `w_durable` (seq write +
  `end_fsync`, 5 namespaces) and small-file fsync-storm rows on
  squeeze-test (B-9: tcp devsub + field) are the parent's. On
  `w_durable` the touched set is EVERY namespace (a 4 MiB-block stream
  spreads across all five), so that row measures only the joined legs
  and the write-through skip is inert (write-back namespaces); the
  small-file storm is where the touched-namespace lever pays.
- **Not built**: eliding the META barrier for an ino whose last commit
  is already durable (needs a per-ino commit position against a
  ring-position durable frontier — a later rung); clearing the touched
  table on the dismount/OQ-5 all-device barriers (precision only, never
  correctness); the fsyncdir ladder (unchanged: intents + the meta
  barriers).

## 6. Gate

`cargo fmt --check`; `cargo clippy --all-targets --all-features` and the
shipped-features config, both `-D warnings`; `RUSTDOCFLAGS="-D warnings"
cargo doc --no-deps`; the fsync-path suites single-threaded, all-features
(every `tests/*` file that calls `fsync` / `flush_inode_to_backend` /
counts device barriers — 115 suites, ≈ 1,090 tests green, incl. the
durability matrix's power-cut rows, `fsync_durability_contract_tests`,
`fsync_single_barrier_tests`, `fsync_coalescing_tests`,
`fsync_writeback_tail_loss_tests`, `crash_contract_tests`,
`audit_instruments_tests`, `env_knob_convention_tests`,
`derivation_sweep_tests`, the write-path, overlay, placement, lifecycle
and multi-writer suites). Not run here: the full `task check` (the
parent's), the root/fleet rigs, any box row. One pre-existing flake
observed once in the sweep and 7/7 green on re-run:
`write_pipeline_phase_tests::pipeline_write_through_records_every_residence_phase`
(`write_pipeline.quiesce` returns when the last permit drops, which is
BEFORE that task records its `Total` sample — a race in the test's
witness, not in the pipeline; untouched by this branch). The one shape
change outside the new suite: `durability_matrix_tests`' engagement
assertion for the two zero-device-exposure rows (inline, staged) now
asserts the fsync requests NO device barrier — the row's whole claim.
