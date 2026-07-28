# 2026-07-28 — op-economy-window writeback hang: root cause, deterministic repro, fix

Branch `fix/opeconomy-writeback-hang` off dev `346be8d`. Commits:
red `7281b95` (schedule-forced repro), green `c2a713d` (the fix).
Found by the ingest-economy campaign's from-zero gate (its evidence
note carries the live capture, §7); bisected by the orchestrator;
root-caused and fixed here. **Release train frozen on this until
merged.**

## 1. The bisect (orchestrator's rig — loaded rolls, `statfs_tests` ×6
## per commit, parallel cargo release build as load, 180 s timeout)

| commit | loaded result |
|---|---|
| `78b9498` | 6/6 green |
| `7396ffd` (write-pipeline) | 6/6 green |
| `22c31ac` (killpriv + write-times + fork cleanup) | 6/6 green |
| `346be8d` (dev tip; delta = op-economy merge `88f2075` + script + test-only fix) | **3 hangs** (exactly while the load build ran) |

## 2. The captured hang (ingest-economy gate, preserved live)

`statfs_tests::test_statfs_free_tracks_write_then_delete_reclaim`: test
thread 16+ min in **uninterruptible D-state**, kernel stack
`fuse_fsync → file_write_and_wait_range → folio_wait_writeback`; the
(debug-build) daemon's fusectl connection showed **`waiting=28`** — 28
FUSE requests whose replies were never sent. The daemon log: repeated
`fuse3-tpcN` panics at `fuse_over_uring.rs:342` —
`debug_assert!(lease age < 1 s)` in `EntPayloadLease::drop`. `umount`
joined the wedge (it syncs the superblock behind the same stuck
writeback); only `echo 1 > /sys/fs/fuse/connections/<id>/abort`
released the D-state threads.

## 3. Mechanism — the racing actor and the lost completion

1. **The racing actor is writeback backpressure stretching the
   FUSE_WRITE handler invocation past 1 s** while the §5.4 transport
   payload lease (whose lifetime is, by design, exactly one handler
   invocation) is still alive. Write-pipeline admission waits are
   honest backpressure; debug builds and loaded/slow substrates stretch
   them. Load does not cause this schedule — it selects it.
2. The lease-severance watchdog — `debug_assert!(age_ms < 1000)` in
   `EntPayloadLease::drop`, armed in every debug/test build — then
   **panics inside the handler task**: the FUSE reply for that WRITE is
   never sent, the kernel's writeback folio never completes, and
   `fsync(2)`/`sync(2)`/`umount(2)` park in D-state forever. That is
   the lost writeback completion.
3. The unwind also skips `state.release()` below the assert: the ring
   ent's deferred COMMIT_AND_FETCH re-arm parks forever — **permanent
   transport queue-depth loss** stacking per firing (the captured run
   burned ents repeatedly before wedging).

## 4. Why the bisect flips at the op-economy merge

The merge's kernel-write-path delta was audited hunk-by-hunk
(`22c31ac..88f2075`: fuse_client 63 lines, routing 164, cache/nvme 33,
tiering/nvme 5, lib +80): every reachable change is a clone/alloc
economy (CachedMetadata `Arc<str>`/CompactString retyping, single
metadata get, StackKey formatting, `get_static` borrowed keys,
`peek_original_size`, serve-into-arena) on the **ring-serve** path;
none adds blocking, an error path that skips a reply, or a
lookup-shape change on the kernel write/writeback path. What the merge
does change is **timing distribution** around an armed 1 s trip line
that was already nearly exhausted:

- Instrumented rolls (this box, 22 CPUs, stated load recipe = looping
  fat-LTO `cargo build --release` in a sibling worktree;
  `transport_lease_max_age_ms` polled at 300 ms):
  **`22c31ac` high-water 417/553/544 ms; `346be8d` 484/595/494 ms** —
  both endpoints live within ~2× of the panic line under load, and the
  orchestrator's (hotter) rig pushed `346be8d` past it 3/10 while
  `22c31ac` stayed under 6/6. The merge shifts scheduling/latency
  enough to cross the line on that rig; the FAILURE MODE (a watchdog
  that kills the data plane when crossed) predates it and is the bug.
- The wedge reproduces at ANY commit once the line is crossed: the
  identical seam + red test fails on `88f2075` and on this branch's
  parent alike (evidence runs recorded in the ingest-economy note §7.4).

## 5. Deterministic repro (schedule forcing, not statistics)

`SQUEEZEFS_TEST_WRITE_STALL_MS` — a documented test seam at the top of
the FUSE write handler (read once, zero cost unset): stalls the
invocation while the payload lease is held, i.e. forces the exact
schedule load selects. `tests/transport_lease_overlong_tests.rs`
(`overlong_write_lease_is_a_tripwire_not_a_lost_reply`): real binary,
real unprivileged FUSE-over-io_uring mount, buffered 256 KiB write +
fsync against a 1.2 s stall.

- **Pre-fix: red every run** (fsync never returns; 60 s deadline turns
  the forever-hang into a loud assertion; the harness unwedges itself —
  connection-abort BEFORE umount, because umount recurses into the
  stuck sync).
- Post-fix: green in ~2.8 s.

## 6. The fix (`c2a713d`)

`EntPayloadLease::drop` never panics. Overlong (≥ 1 s) leases count in
the new **`transport_lease_overlong`** tripwire
(`transport_lease_stats` 4-tuple → 5-tuple; exported on the stats
inode) with a loud error log, and `release()` always runs. A genuine
§5.4 escape (a payload parked toward a long-lived cache) manifests as
unbounded `transport_lease_max_age_ms` + growing
`transport_parked_commits` — adjudicated by counter, never by killing
the data plane. Release builds are behaviorally unchanged except the
counter (the assert only ever compiled into debug builds — which is
exactly where every gate runs).

**Op-economy wins survive untouched**: the fix touches only the
watchdog's action; `ipc_op_economy_tests` (the alloc-economy contracts)
green on this branch.

## 7. Acceptance (this branch, counted)

- Repro test: red pre-fix (every run), **green post-fix** (2.8 s), ×3
  more green under the load recipe.
- **`statfs_tests` ×10 under the stated load recipe: results below —
  requirement 0 hangs**, plus a zero-waiting-connections residue sweep.
- Full cargo gate from zero: clippy `-D warnings` (root + fuse3
  crates), fmt, `cargo test --all-features -- --test-threads=1`,
  `cargo doc --no-deps`, bench smoke.
- (Campaign-side evidence, same mechanism: statfs ×10 + repro ×3 under
  load already green with this fix on the ingest branch.)

### Results

- Loaded soak (this box, load loop verified live for the full window):
  **`statfs_tests` ×10 = 30/30 tests green, 0 hangs, 0 timeouts**
  (61.8–62.2 s per roll — the file's nominal duration), residue sweep
  clean (no fusectl connection left `waiting`).
- Repro: pre-fix red every run (this branch's parent AND `88f2075` with
  the same seam+test); post-fix **green (2.8 s)**, ×3 more under load.
- `ipc_op_economy_tests` 3/3 green (the op-economy alloc contracts
  survive; no economy sacrificed — the fix never touches the serve
  paths).
- clippy `-D warnings` clean (root + fuse3), fmt clean; full
  `cargo test --all-features -- --test-threads=1` from zero + doc +
  bench smoke recorded in the final report.
