# FIND-SMO-TAIL — mid-entry tail floors fixed (Branch 2, 2026-07-16)

**Branch** `fix/kv-mid-entry-tail-floors` off dev `89ea158`. **Charter**:
`docs/design-smo-replay-currency.md` §1b + the PR 3 row (= PR 1(d)
test-commit + PR 3 fix-commit) — the mid-entry-tail finding chartered in
review round 1 and observed **live** in Branch 1's post-fix round 3
(`dropped_torn = [1,1,0,0]` with zero loss,
`.benchmarks/2026-07-16-smo-two-phase-replay.md`; images preserved at
`~/tmp/smo_b1_postfix_*` — not needed here: the deterministic RED landed
first-try, so the fixture-replay fallback stayed unused). Option A
(pending-free coverage, PR 4) and the ×10 storm acceptance soak (PR 5)
are **not** this branch's rows.

Rails: unique sandbox `~/tmp/smo_b2_1784203879` (deleted post-merge);
kills by PID; systemd-run cages; storm on the full CPU mask; builds on
`taskset -c 0-15`; Tctl peaked 51.5 °C.

---

## Mechanism (verified at source, §1b verbatim)

Floors folded **raw record seqs** (`dirty_floor.fetch_min(rec.seq)`,
`node_cache.rs`) while the conveyor stamps `entry_start + i` across a
multi-leaf tx (`backend.rs`). Flush the leaf holding `rec[0]` while the
leaf holding `rec[j>0]` stays floor-restored un-flushed (log-full freeze
restore `tree.rs`, or the reserve-skip `checkpoint.rs`) and the §4.6 pt 2
tail computes to `entry_start + j` — **strictly inside a journal entry**,
violating the `checkpoint.rs:34-41` "tail is an entry boundary" claim.
Replay parses AT the tail (`journal.rs`), fails mid-entry, resyncs at the
next page's first-entry offset, and drops the entry's ≥-tail acked
records plus collateral entries to the resync point (`dropped_torn ≥ 1`
once a later entry parses — the newest-entry edge is why the contract is
the JOINT no-loss AND `== 0` assertion, never the counter alone).

## RED (PR 1(d), commit `5a82ff7`)

`mid_entry_tail_multi_leaf_tx_partial_flush_survives_crash`
(`tests/kv_smo_crash_completeness_tests.rs`): an **unlink** is the
multi-leaf shape (dentry Delete → DENTRIES leaf; parent Δ + child Put →
INODES leaf, seqs `S+1`/`S+2`). Deterministic with **no byte calibration
and no new seam**: same-key setattr bursts fill the INODES root leaf's
log (compactions = lifecycle boundaries, measured in-run), pre-position
near-full, then acked unlink **probes** — each landed probe shrinks the
remaining log area, so a bounded probe count forces one probe's freeze to
fail: floor `S+1` restored, compaction retires the leaf into the
dying-floor fold, ledger tail = `S+1` (mid-entry). Post-SMO acked commits
(> one journal page) are the collateral + drop confirmers. On dev
`89ea158`, ×3 on `taskset -c 0-15` AND ×3 full mask, identical every run:

```
FIND-SMO-TAIL (§1b): the checkpoint tail landed strictly inside the
victim unlink's journal entry ('probe001'), so replay parsed mid-entry,
resynced at the next page, and dropped acked records
(dropped_torn = 1, acked losses = 0): []
```

(The counter half fires deterministically; the loss half is
page-position-dependent — exactly the Branch-1 round-3 wild signature.)

## Fix (PR 3, commit `8500ce7`) — floors round DOWN to entry starts

The floor is **ring-position domain** (what the tail must keep in the
replay window), not fold domain. `CachedNode::apply_locked` takes the
floor explicitly; callers pass entry starts:

- conveyor pass members → their computed `entry_start`;
- compensation txs → `res.start`;
- mount replay (both phases) → the `ReplayedEntry` start (post-replay
  checkpoints cannot re-mint a mid-entry tail from the window's own
  records; replayed K6a-era entries legitimately carry fold-domain seqs
  below their entry position — a first-cut `debug_assert!(seq ≥ floor)`
  tripped `v3_mount_replays_journal_window_into_the_cache` and was
  removed as asserting a coincidence of the production stamping);
- fresh single-record applies → their minted seq (own entry start);
- SMO flips → `recs[0].seq == res.start` (semantics unchanged, explicit);
- SMO leftover moves → the predecessor's floor (itself entry-start-
  rounded), raw-min-seq backstop — never weaker than pre-fix.

Restores the `checkpoint.rs:34-41` claim globally (module doc updated).
Safety per the reviewed design (§1b): lower tail = longer replay window,
absorbed idempotently by the per-key LWW gate; ring runway lags ≤ one
entry; floors still clear on flush. Loom untouched (models cover
`*_core.rs` only — the floor domain lives outside them). Zero on-disk
format change.

## Verification (fix commit `8500ce7`)

- RED→GREEN: ×3 `taskset -c 0-15` + ×3 full mask on the final code
  (count restarted post-assert-removal per the multi-run discipline).
- `cargo clippy --all-targets --all-features -- -D warnings` — clean.
- `cargo fmt --check` — clean. `cargo doc --no-deps` — clean (also fixed
  the pre-existing dangling private intra-doc link on the Branch-1 seam
  static). `cargo bench --benches -- --test` — smoke green.
- `cargo test --all-features -- --test-threads=1` — **851 passed / 0
  failed** on the committed SHA. Suites by name: fsync_coalescing 1,
  fsync_single_barrier 1, conveyor 10, crash_contract 25, crash_kill 8,
  kv_smo_crash_completeness 4 (incl.
  `replay_twice_digest_stable_across_smo_windows` — digest equality holds
  over the floor change by construction: floors never enter the fold),
  writeback_fencing_livelock 5.

## Storm rate-gathering (declared rate-gathering, NOT acceptance)

Four `recapture.sh` rounds on the fixed release binary (16-worker acked
create storm, kill -9 at peak, full mask, in-place remount). The ×10
acceptance count stays PR 5's gate after Option A lands.

| round | acked | missing | refusals | dropped_torn | replay entries/vol |
|---|---:|---:|---:|---|---|
| 1 | 63,595 | **0** | 0 | **[0,0,0,0]** | 6459/5880/4900/4703 |
| 2 | 65,352 | **0** | 0 | **[0,0,0,0]** | 5886/5239/2635/3170 |
| 3 | 65,588 | **0** | 0 | **[0,0,0,0]** | 3295/5456/6299/3614 |
| 4 | 65,552 | **0** | 0 | **[0,0,0,0]** | 4307/3242/5727/3834 |

260,087 acked creates, zero lost, zero refusals, zero torn-drops — vs
Branch 1's 1-in-3-rounds §1b hit rate on the pre-fix binary. Windows were
non-trivial (2.6k–6.5k replayed entries per volume per round).

## What remains for the program

Branch 3 (`fix/kv-pending-free-coverage`, PR 1(b)/(c) + PR 4 Option A)
and PR 5's ×10 joint acceptance soak (acked-loss == 0 AND refusals == 0
AND dropped_torn == 0) on the final binary.
