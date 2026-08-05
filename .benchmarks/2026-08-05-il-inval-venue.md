# 2026-08-05 — The W1 inval dispatch venue: global-inject spawn deleted (il write residual, D12 item 2)

**Branch:** `perf/il-write-residual` (off `integrate/zcrx-wave` `f2c2b8c3`).
**Commits:** RED `205e387d` (venue pins + verbatim hook factoring + fuse3
test seam), fix `04e044bd`. Measured binary B was `c7ec27e7` — identical
to `04e044bd` on every shipped code path (the amends were test-file
formatting + one fuse3 doc-comment line).
**Parent evidence:** `.benchmarks/2026-08-05-fleet-parity-writes.md` (the
~0.8× field write residual, prime candidate flagged there).

## The venue, confirmed

Every size-growing sequential ring write fires the POSIX-8 attrs-only
invalidation (`Invalidator::on_write` → `fire` → hook —
`src/ipc_service.rs`), and the production hook ran
`Handle::spawn` onto the **multi-thread runtime's global inject queue**
(`hook_runtime.spawn`, formerly `src/fuse_client.rs`) — the exact venue
the 2026-07-26 handoff-economy fix banned for ring handoffs (~130 µs/op
measured queueing term, `.benchmarks/2026-07-26-ipc-handoff-economy.md`)
— plus one `Notify` clone and one task alloc per op. The write handoff
itself already rides the fuse3 tpc lanes; the inval spawn was the ONE
remaining per-op global-inject item on the il ring write path (audited:
sever → deferred `handoff_spawn_on` → handler → completion → inval).

The spawn existed only to enter async context for an await that never
suspends: `Notify::invalid_inode` → `ReplyTx::send` is one
`futures_channel::mpsc::unbounded_send` (synchronous, thread-safe,
wake-carrying). Fix: `Notify::invalid_inode_detached(&self, …)` — the
same ONE shared encoding (`inval_inode_frame`, generic/451's no-drift
law) enqueued synchronously in the caller's context; the hook
(`ipc_service::make_inval_hook`) is now one arc-swap load + one channel
send. W1 policy/window/POSIX-8 law and the fire-and-forget ordering
adjudication are UNTOUCHED (ledger identical per row, see below); the
notify still reaches the kernel via the reply task's classical device
write, off every request lane.

Pins: `tests/ipc_inval_venue_tests.rs` (×10 green; all 3 RED at the
parent — the pre-fix hook could not even be constructed outside a
runtime) + fuse3 `notify::tests` (sync enqueue from a plain OS thread;
`now_or_never` proof the async form never suspends — the premise made
loud). Weakening-verified: restoring any spawn fails the
synchronous-delivery asserts and the no-ambient-runtime construction.

## Local repro + A/B (tcp devsub, `tests/fleet_width_bracket.sh`)

**Instrument:** fio 3.42 psync process-fleet qd1 bs=1M zero-buffers,
w256 (the local knee), fresh format per rig run, il/kernel alternating
pairs (A-B-B-A per run), medians of 3, engagement exact (all cited rows
`ok`; one B attempt with dirty-stamp binaries self-refused client-side
per KD-7 — those runs are discarded, which is the screen working).
**Substrate:** nvmet-tcp devsub, localhost (25-CPU box, zram oss).

Per-row inval ledger, create shape (~identical both binaries — only the
venue changed): `ipc_inval_notifies ≈ 14.1–14.3 k` ≈ `ipc_ops_write`
(~13.2–14.7 k of 16,384; ~50 budget admission refusals dilute il
engagement to ~80–90 % at this width locally), `attrs_only ≈ 12.7–13.0 k`,
`suppressed ≤ 5`. Kernel rows fire zero. Sustained-overwrite rows invert:
`suppressed ≈ 225 k`, fires ≈ 21 k / 45 s — the create/growing shape is
the inval-heavy one (≈ 7 k global-inject spawns/s locally; the field's
23 GB/s fleet ≈ 20 k+/s).

il/kernel medians per rig run (chronological; A = pre-fix venue,
B = fix):

| shape | A | B |
|---|---|---|
| create (growing) | 0.897, 0.905, 0.898 | 0.906, 0.930 |
| sustained 45 s (overwrite) | 1.158, 1.073 | 1.093, 1.021 |

**Verdict (honest):** the venue term prices LOCALLY as a small,
order-robust improvement on the inval-heavy create shape (B ≥ A in both
brackets, median 0.898 → 0.918) and noise on the sustained shape (fire
rate ~470/s there; kernel medians swing ±25 % run-to-run on this box).
The local ~0.9 create ratio persists post-fix — locally that shape is
dominated by fleet session establishment + partial engagement (2 s
rows, 207–210 sessions each), NOT the inval venue. **The ~0.8× field
residual is therefore NOT adjudicated closed by local evidence.** The
fix stands on the venue law (banned venue deleted; strictly less work
per op: −1 task alloc, −1 inject-lock acquisition, −1 worker wake, −1
Notify clone per growing write) and the local no-regression brackets.

## Requested field row (orchestrator)

`tests/fio/fleet_parity_row.sh`, write bs=1M ×256, binary `04e044bd`
vs wave `649af374`, same venue/instrument as the parent note (K-I/I-K
pairs, engagement exact, settle, mountpoint gate). Read beside the row:
`ipc_inval_{notifies,attrs_only,suppressed}` deltas (the ledger must be
unchanged vs `649af374`) and the il write VARIANCE across valid rows —
the variance face (14.7–23.1 GB/s) is the field signature the venue
term predicts; if the residual survives with the venue gone, the next
named candidate is fleet-launch session-establishment jitter (parent
note, candidate 2).
