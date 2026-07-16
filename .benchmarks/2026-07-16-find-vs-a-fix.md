# FIND-VS-A — teardown SIGBUS closed; acked-loss root-caused, hardened, residual chartered (2026-07-16)

**Branch** `fix/teardown-sigbus-acked-loss` off `4421f06`. **Charter**: the
vs-JuiceFS scoreboard's Loss 3 (`.benchmarks/2026-07-15-vs-juicefs-scoreboard.md`
FIND-VS-A: R3 `stat_storm`/`del_storm` INVALID) + the L1 report's standing
"bench+umount teardown SIGBUS" defect (`.benchmarks/2026-07-15-l1-transport-concurrency.md`,
reproduced there on baseline `c56ec6a`).

All repro runs on the FULL CPU mask (FIND-VS-B precedent: the 0-15 rail can
mask geometry bugs); unique sandboxes under `/var/tmp/sqz_findvsa*`; kills by
PID; systemd-run cages; `/mnt/squeezefs`, `/mnt/juicefs`, `~/tmp/nvme` untouched.
Repro/forensic tooling preserved in `.agents/findvsa/` (repro scripts, the
offline v3 image parser `kvparse.py`, fixtures under `tests/fixtures/findvsa_*`).

---

## Part 1 — teardown SIGBUS: root-caused and CLOSED

### Root cause (exact)

`squeezefs umount` (the CLI, a separate process) "counted staged files" by
constructing an `NvmeCache` **over the live daemon's
`staging/<fs>/<mnt>/staging_segment/` directory** with a hardcoded
100 MiB budget (`src/main.rs` Umount arm). `NvmeShard::new` unconditionally
`file.set_len(capacity)` — **truncating every existing 128 MiB segment file to
6.25 MiB (100 MiB/16 shards) while the daemon held full-size `MmapMut` maps**.
Every subsequent daemon access beyond the new EOF raised `SIGBUS (BUS_ADRERR)`:
the teardown drain (`flush_all_staged_blocks_to_backend` →
`NvmeStaging::{get_staged_fencing_token, remove_active_block}` → mmap read)
died mid-flush. The truncation also **destroyed the staged payload bytes** —
data loss on disk, not just a crash.

Evidence chain:
- 8 scoreboard-session coredumps, e.g. `coredumpctl` PID 222865 (19:12:40):
  `BUS_ADRERR`, fault addr `0x7f5264803fc0` = offset 0x803fc0 (8.4 MiB) into
  `staging_segment/segment_6.bin` per the core's NT_FILE table — **within** the
  128 MiB map but **beyond** the 6.55 MB truncated file. Stacks:
  `NvmeStaging::remove_active_block` / `NvmeShard::get ← get_staged_fencing_token
  ← flush_one_active_block ← flush_all_staged_blocks_to_backend`.
- Live repro on dev `4421f06` (`.agents/findvsa/repro.sh`: dataset + rand-write
  residue + 131,072-file tree storm → `squeezefs umount`): segment files
  measured 64 MiB → **6,553,600 B** across the umount window; daemon SIGBUS
  (coredump PID 482252, same stack), reproduced 3/3.
- The scoreboard's R3 INVALID rows: the recovered-orphan drain of the remount
  daemons kept hitting the same truncation on every subsequent `squeezefs
  umount` (crash chain 19:11:46 / 19:12:12 / 19:12:40).

### Fix (two independent layers, red-first)

1. **`NvmeShard::new` never shrinks an existing segment file**
   (`src/tiering/nvme.rs`): `set_len` only when the file is smaller than the
   requested capacity. A larger file keeps its length (the ring uses the first
   `capacity` bytes); growth stays legal. No second mapper can invalidate live
   mmap pages or destroy staged bytes again — the whole class, not the one
   caller. Pinned by `tests/staging_second_mapper_tests.rs`
   (`second_mapper_with_smaller_capacity_never_shrinks_segment_files` — RED on
   the old code via the file-length assertion; `opening_with_larger_capacity_
   grows_the_segment_file`).
2. **The umount CLI no longer opens segment files at all** (`src/main.rs`):
   staged/active counts + drain progress now come from the mounted
   filesystem's daemon-authoritative `.stats` virtual file
   (`nvme_staged_write_file_ids`, `active_writes`,
   `metrics.nvme_staging_current_bytes`); when the mount is gone there is
   nothing to poll and the unmount proceeds. The interactive "discard staged
   data from the CLI" path is **gone** — mutating the live daemon's segments
   from a second process was the crash+loss vector; staged custody belongs to
   the daemon teardown drain and next-mount recovery (never-lossy contract).

### Acceptance

- SIGBUS repro shape (storm → immediate `squeezefs umount`) green **10/10 full
  mask + 10/10 `taskset -c 0-15`** on the fixed binary (see the transcript
  table below; pre-fix: fires reliably with staged residue present).
- Churn-unmount soak (the FIND-M11-A pattern: mount → create/write/unlink
  churn + staged blocks → clean unmount, ×10): **10/10 clean** — no SIGBUS, no
  ENOTCONN residue, daemon exits, zero new coredumps.
- Post-unmount remount serves the full tree (`missing files: 0` in every
  repro round) — the truncation-destroys-staged-bytes half is gone with it.

## Part 2 — the acked-create loss: adjudicated, hardened; residual = charter

### Verdict on the finding's hypotheses

Reproduced without any SIGBUS: **kill -9 at create-storm peak** loses
1–4 % of ACKED creates across remount (`.agents/findvsa/repro3.sh` /
`repro4.sh`; e.g. 2,549/66,381 acked names ENOENT after a clean-replay
remount, `replay_dropped_torn = 0` — the exact scoreboard signature).

- **NOT the 50 ms cadence / ring-parking window**: for a *process* crash,
  every completed buffered write survives in page cache regardless of
  barriers; ack-to-media latency is not the mechanism. (Power-crash bounds
  are a different, pre-existing conversation — deferred mode's documented
  contract.)
- **NOT ack-before-write in the conveyor**: `commit_tx` parks on the pass
  oneshot; the pass writes (`write_entries_batch`, completion-awaited) and
  `wait_completed_upto(res.end())` before fan-out. Verified in code and by
  forensics: acked records' bytes were present in the post-kill images.
- **THE ACTUAL MECHANISM — v3 KV SMO routing currency vs the checkpoint
  tail**: under storm-rate leaf compactions/splits (~1 SMO per ~1k creates
  per volume at 64 KiB-node scale; every few ms at 256 KiB), the ledger's
  `journal_tail_seq` advances past record seqs whose durable *reachability*
  still depends on state the mounted ledger record does not name:
  - floors that die with their `CachedNode` when an SMO **retires** the
    mapping (`take_dirty_floor` on the flush path + retire), when a clean
    node **evicts**, or when `publish` replaces a mapping;
  - **root swaps**, the one routing change with no journaled pointer record
    (durable form = exclusively the next ledger record's `tree_roots`);
  - interior pointer images referencing successor incarnations across
    **extent reuse** (the residual class — see the charter below).
  Offline dissection of post-kill images (`kvparse.py`: ledger slots, ring
  chain walk + union census, heap frame walks, tree descents) showed, run
  after run: missing names byte-present in retired/unrouted leaf images,
  `BELOW` the mounted tail, with the routing (interior separators / roots)
  lagging — e.g. a split predecessor with 4,334 live keys whose two
  successors carried 4,574 keys but dropped 14 (the target among them), and
  an inode-tree interior whose live separators skipped the key range entirely
  (`inode 46399`: dentry resolved, inode record unreachable → the observed
  ENOENT).

### Hardening landed (each independently correct; loss narrowed ~10×)

- **Dying-floor clamp** (`node_cache.rs` + `checkpoint.rs`): a per-cache
  accumulator folds the `dirty_floor` of every departing mapping (SMO
  retire, eviction, publish-replace) plus every **root swap's** journal
  position; the checkpoint tail computation drains it and clamps
  `journal_tail_seq`, and folds it back if the ledger write fails. Records
  whose coverage died with a mapping stay inside the replay window until a
  ledger record written afterwards covers them.
- **Flush-path floor restore before SMO** (`tree.rs`): `checkpoint_flush_node`
  re-arms the taken floor before entering `smo_replace`, so the retire-side
  fold sees the true floor (it previously died silently in a local).
- **SMO fold-source guards** (`tree.rs`): `smo_replace` refuses (loud
  `KvError::Corrupt`, floor restored, next cycle retries) when the loaded
  disk image's `node_seq` differs from the live object's, or when the disk
  walk's tail offset disagrees with the object's append cursor — the
  stale-fold shapes can no longer silently drop acked records into a
  successor.
- **Hole discipline reconciled** (`backend.rs` `checkpoint_past(pos)`): the
  §4.4 pt 4 unwritten-hole checkpoints now cycle until the written tail
  actually clears the hole end (the clamp can legitimately hold the first
  cycle's tail below it) — pinned by the existing
  `crash_kill_tests::test_rollback_race_seq_conditional` +
  `conveyor_tests::poisoned_tx_fails_alone_batch_survives`.
- **Publish-replace of a non-clean mapping now logs loud** (`node_cache.rs`)
  — never-papered visibility for the displaced-overlay shape.

Pinned by:
- `tests/kv_smo_crash_completeness_tests.rs` — acked creates × interleaved
  checkpoints × SMO churn × crash-equivalent reopen ⇒ zero loss (the
  contract test for this class at cargo scale).
- `tests/findvsa_fold_fixture_tests.rs` — the production `compact()` over the
  REAL captured predecessor bset + frozen-extra records from a loss run
  (fixtures in `tests/fixtures/findvsa_*`): every live input key must
  survive the fold.

### Residual (measured honestly) and the STOP condition

kill-9-at-peak ×5 on the final binary: 2 rounds mounted with losses of
1,218 and 587 acked names (~0.9 % / ~0.4 % of ~130k, down from 1.5–4 %
pre-hardening), 3 rounds refused the remount loud
(`traversal retry budget exhausted … child-seq` — **also reproduced on
unmodified dev `4421f06`** in the pre-fix forensics runs, so the refusal
class predates this branch).
Forensics pin the remaining mechanism inside **extent reuse vs interior
pointer currency** (a mounted interior image can reference a successor
node_seq at an extent whose current tenant is a different lineage — the
child-seq loop — or route key ranges to leaves that predate the newest
flips). Widening the clamp to EVERY SMO reservation was tried and measured
**worse** (replay windows grew into territory the level-routed replay was
not designed for: refusals amplified) — reverted.

Closing this fully means re-specifying how a mount reconstructs routing
across un-checkpointed SMO bursts — a `docs/design-cow-kv-metadata.md`
§4.6/§4.7 contract decision (options include: pointer-currency epochs in
the ledger; quarantining freed extents until the freeing interior's image
is flushed **and** covered; or replay-time interior reconstruction from
journaled SMO records with stale-pointer tolerance). Per the FIND-VS-A
rails ("if the judgment requires a contract change — STOP and report the
options"), that charter is **reported, not unilaterally implemented**.
Consequence bound: the residual requires kill-9/crash *at storm peak
mid-SMO-burst*; clean unmounts (incl. the scoreboard's R3 protocol) drain
and checkpoint to `tail == head` and are unaffected (repro rounds:
`missing files: 0` ×10×2 masks; churn soak 10/10).

## Scoreboard R3 re-run

`SQUEEZEFS_VS_REGIMES=R3 SQUEEZEFS_VS_WORKLOADS="stat_storm del_storm"
tests/run_vs_juicefs.sh` @ `e8a90f4` (artifacts
`/var/tmp/squeezefs_vs_juicefs/artifacts/20260716T060353Z/`): the two
formerly-INVALID rows produce numbers and WIN —

| Row | JFS | SQZ | SQZ/JFS | Verdict |
|---|---:|---:|---:|:--:|
| R3.stat_storm (files/s) | 119,771 | 348,555 | 2.91× | **W** |
| R3.del_storm (files/s) | 3,378 | 35,979 | 10.65× | **W** |

`GATE: GREEN — no loss rows` for the partial run; zero ENOENT, zero
SIGBUS, rc=0 both rows (the cold protocol's unmounts no longer crash
mid-drain and the remounted tree is complete).

## Gate

- `cargo clippy --all-targets --all-features -- -D warnings` — clean.
- `cargo fmt --check` — clean.
- `cargo test --all-features -- --test-threads=1` — **847 passed / 0
  failed** (incl. the crash-contract, crash-kill, conveyor,
  dismount-teardown suites and the three new FIND-VS-A test files).
- `cargo doc --no-deps` — clean. `cargo bench --benches -- --test` — smoke
  green.
- Gate test leg re-run under `taskset -c 0-15` (FIND-VS-B precedent: both
  masks) — green.

## Soak / churn note

The `did not settle after 8 binding rebinds` EIO the first soak shapes hit
on multi-MiB / sparse rand-writes is **pre-existing on dev `4421f06`** (the
baseline binary fails the identical shape at round 1; it is the standing
rand_write family already chartered as scoreboard Loss 2) and is excluded
from this charter's soak. The teardown soak (`.agents/findvsa/
churn_unmount_soak.sh`, the FIND-M11-A pattern) is 10/10 clean on the fixed
binary.
