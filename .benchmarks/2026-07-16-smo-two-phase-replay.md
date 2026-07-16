# SMO replay-currency Branch 1 — C′ two-phase replay lands (2026-07-16)

**Branch** `fix/kv-smo-two-phase-replay` off dev `44d14d6`. **Charter**: PR 1
shared scaffolding + PR 1(a)/(c) tests + PR 2 (Option C′) of
`docs/design-smo-replay-currency.md` (Status: Approved — writer/reviewer
consensus, 2 rounds, 11 issues resolved) — the load-bearing fix for
**sub-mechanism (i) stranding** of the FIND-VS-A acked-loss residual
(`.benchmarks/2026-07-16-find-vs-a-fix.md:157-183`). Sub-mechanism (ii)
(root-swap + below-tail-flip reuse edges → Option A) and FIND-SMO-TAIL
(§1b mid-entry tails → PR 3) are **explicitly not this branch's rows** —
they land on `fix/kv-pending-free-coverage` and
`fix/kv-mid-entry-tail-floors`; the ×10 storm acceptance soak
(acked-loss == 0 AND refusals == 0 AND dropped_torn == 0) is PR 5's gate
after all three branches, per the decision doc.

Rails: unique sandbox `~/tmp/smo_b1_1051210/` (preserved); kills by PID;
systemd-run cages; repro on the FULL CPU mask (FIND-VS-B precedent);
builds on `taskset -c 0-15` with `CARGO_BUILD_JOBS=12`; Tctl stayed < 45 °C.

---

## First act — crash images re-captured (the originals no longer existed)

`.agents/findvsa/recapture.sh` (committed) reruns the repro4.sh shape —
16-worker acked-create storm, `kill -9` at peak (KILL_AFTER=4), page-cache-
coherent meta/data copies **before** any remount — looping rounds until a
loss round. On dev `44d14d6` (release), round 1:

| metric | value |
|---|---|
| acked creates at kill | 67,887 |
| acked names ENOENT after in-place remount | **311 (0.46 %)** |
| `replay_dropped_torn` | `[0, 0, 0, 0]` — clean replay |
| remount refusals | 0 this round (the (ii) face; refusal rounds are kept + tagged when they occur) |

Raw images: `~/tmp/smo_b1_1051210/capture_round1/` (sparse 1 GiB meta ×4 +
4 GiB data ×4 — out of repo by size; regeneration is one script run).
Committed subsets: `tests/fixtures/findvsa2_stranding_window.jsonl` (86 KB,
kvparse-derived window records for every lost name + covering flips) and
`.agents/findvsa/capture-2026-07-16-expectations.txt` (full kvparse.py
output: ledger slots, ring census, chain walks, per-name adjudication).

**Adjudication** (`.agents/findvsa/extract_stranding_fixture.py`, importing
kvparse.py's checksummed parsers; window walk cross-asserted against
`kvparse.chain_walk` entry counts):

- **311/311** lost names have their acked Put **IN the replay window**
  (`seq ≥ mounted tail` on their volume) — the loss is not a durability
  hole; replay saw every record and the walk routed around them.
- **192/311** carry the direct **(i) stranding signature**: an in-window,
  higher-seq interior flip covering the record's key (every signature hit
  is on the **inode tree** — "dentry resolved, inode record unreachable",
  the original forensics shape). The remaining 119 are the flip-less
  faces (root-swap windows / reuse edges) — exactly the decision matrix's
  Option-A rows.

## The deterministic seam (shared scaffolding, inherited by Branches 2/3)

`TEST_SMO_BUILD_PAUSE_TREE` / `TEST_SMO_BUILD_PAUSED` /
`test_smo_build_pause_release()` (`src/meta_backend/kv/tree.rs`): arm with a
tree id and every `smo_replace` on that tree parks **between successor
load-back and the §4.6 lock window** — successor images fixed on disk, no
locks held, flip reservation not yet taken — publishing the paused node's
`(tree_id, level, is_root, min_key, max_key)`. Racing commits injected
while parked reserve **below** the flip's seq and reach the successors only
as `take_overlay()` RAM leftovers: sub-mechanism (i), held open on demand.
Register-recheck wake discipline (the `TEST_CONVEYOR_HOLD` pattern);
unarmed cost = one relaxed load per SMO.

## RED → GREEN

**RED** (`stranded_build_window_commits_survive_crash`, on dev `44d14d6`
class code, deterministic ×3):

```
thread 'stranded_build_window_commits_survive_crash' panicked:
32 of 32 ACKED build-window commits stranded across a crash-equivalent
reopen (clean replay — single-pass seq-order replay routed them to the
abandoned predecessor, then the higher-seq flip unrouted them;
design-smo-replay-currency §1 sub-mechanism (i)): [(1176, 33188),
(1177, 33188), (1178, 33188), ...]
```

(33188 = 0o100644 — the reopened getattr serves the **pre-race** mode; the
acked MARKER 0o100751 commits fold to the predecessor's overlay and the
post-replay walk routes around them.) The test drives the SMO through the
**threshold-maintenance path** (no ledger write, no post-SMO flush pass) —
the exact production shape where the kill beats the next checkpoint cycle;
a full `checkpoint_now` after the SMO was measured to mask the RED (its
fresh dirty-collection flushes the successor overlays — which is precisely
why the production loss is a race, not a certainty).

**Fix** (PR 2, `KvMetaBackend::open` step 5b): two-phase replay — phase 1
applies every window interior-pointer record (level DESC, then seq; upper
flips route lower ones), phase 2 applies content records (original seqs,
per-key LWW unchanged) through the **final** routing.
`apply_replayed_interior` / `apply_replayed` reused verbatim; allocator
records stay consumed by the K4 load; commit path, checkpoint task, and
on-disk format untouched. §4.4 pt 4 holes stay dropped in both phases by
construction (both walk the same checksummed-chain `recovery.entries`).

**GREEN**: the stranding test passes ×3 on `taskset -c 0-15` **and** ×1 on
the full mask; `replay_twice_digest_stable_across_smo_windows` pins digest
equality over the phased order (two crash-equivalent reopens of the same
bytes); `recaptured_window_pins_the_stranding_signature` re-asserts the
capture's mechanism pins from the committed fixture bytes.

## Gate (fix commit)

- `cargo clippy --all-targets --all-features -- -D warnings` — clean.
- `cargo fmt --check` — clean.
- `cargo test --all-features -- --test-threads=1` — **850 passed / 0
  failed** (suites by name: crash_contract 25, crash_kill 8, conveyor 10,
  kv_smo_crash_completeness 3, findvsa_fold_fixture 2 — incl.
  `test_rollback_race_seq_conditional` and
  `poisoned_tx_fails_alone_batch_survives`, the §4.4 pt 4 hole pins).
- `cargo doc --no-deps` — clean. `cargo bench --benches -- --test` — smoke green.
- loom: untouched (models cover the `*_core.rs` files only; this branch
  changes `tree.rs` seam + `backend.rs` replay — neither is loom-included).

## Post-fix storm spot-check (declared rate-gathering, NOT acceptance)

Three `recapture.sh` rounds on the **fixed** release binary (same shape,
same sandbox rails, full CPU mask). The ×10 acceptance count belongs to
PR 5 after Options A + FIND-SMO-TAIL land (multi-run discipline:
acceptance restarts post-fix on the final binary).

| round | binary | acked | missing | dropped_torn | note |
|---|---|---:|---:|---|---|
| capture | dev `44d14d6` | 67,887 | **311 (0.46 %)** | [0,0,0,0] | the captured RED round |
| 1 | fixed (C′) | 56,764 | **0** | [0,0,0,0] | clean remount |
| 2 | fixed (C′) | 49,613 | **0** | [0,0,0,0] | clean remount |
| 3 | fixed (C′) | 60,983 | **0** | **[1,1,0,0]** | zero loss, but a live **§1b FIND-SMO-TAIL signature** (`dropped_torn ≥ 1` — mid-entry tail) — Branch 2's row, observed in the wild exactly as chartered |

167,360 acked creates across the three fixed rounds, zero lost, zero
refusals. Not acceptance: the (ii) reuse/root-swap faces and §1b fire at
lower per-round probability than (i) did — their absence in 3 rounds
proves nothing (dev's own capture loop hit loss on round 1 of 6); round
3's `dropped_torn` hit is the standing §1b evidence that PR 5's joint
assertion (no-loss AND == 0) exists for a reason.

## What Branches 2/3 inherit

- **The seam** (`TEST_SMO_BUILD_PAUSE_TREE` + pause info + release):
  PR 4's reserve-exhaustion/reuse RED (1(b)) can park SMOs the same way.
- **The images + regen**: `~/tmp/smo_b1_1051210/capture_round1/` +
  `.agents/findvsa/recapture.sh` (loss rounds AND refusal rounds are
  captured/tagged — the (ii) refusal face is PR 4's fixture source);
  `extract_stranding_fixture.py` for kvparse-derived expectation dumps.
- **Test scaffolding**: the `reopen()` crash-equivalent helper and the
  cadence-parked + seam-driven SMO harness in
  `tests/kv_smo_crash_completeness_tests.rs`; the fixture-row format of
  `tests/fixtures/findvsa2_stranding_window.jsonl`.
- **The C′ carve-outs, stated in code**: root-swap windows and
  below-tail-flip edges replay through old structure by design — Option
  A's quarantine is what keeps them sound (backend.rs 5b comment block).
