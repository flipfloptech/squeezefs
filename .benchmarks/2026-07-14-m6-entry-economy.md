# PR M6 acceptance — single-entry rename + unlink journal economy (G4 closed)

| | |
|---|---|
| **Program** | metadata-throughput (`docs/design-metadata-throughput.md`), PR M6 — §5.4 D4 revised by M2's measurements |
| **Branch** | `perf/meta-entry-economy` (off dev `5314d82`) |
| **Box / rails** | same 3.5 GHz-capped box as the baseline/M2; storms quiet-gated (3×15 s streak, Tctl < 80 °C), daemons caged (`systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=0`), binaries named `sqm6` / `sqm6dev` (kill-pattern immunity), kills by PID only |
| **Substrates** | **B1 null_blk** (`/dev/mdb_fast`, configfs recipe from the baseline: 3 GiB, `memory_backed=1`, `cache_size=1024`, `completion_nsec=0`, `irqmode=0` — `fua=1`, `write_cache=write back`; barrier work measures on real-block substrates only) + **A2 nocow file** context (`chattr +C` sandbox on the btrfs home fs) |
| **Shape** | mdstorm 8 threads × 100 k, one dir (`create → stat → rename → unlink`), per-phase `.stats` snapshots; dev-tip (`sqm6dev` @ 5314d82) vs branch-tip (`sqm6`) same-session pairs, default + strict (`SQUEEZEFS_META_FLUSH_INTERVAL_MS=0`) on B1 |

## Verdicts up front

1. **G4 CLOSED and measured through the mount**: rename **1.0022–1.0041**
   and unlink **1.0178–1.0190** journal entries/op (gate: ≤ 1.02 / ≤ 1.05;
   same-session dev-tip rows read 2.0021 / 2.0190, byte-agreeing with the
   baseline's 2.002 / 2.020). Strict-mode barriers: rename **2.000 →
   1.000/op exactly**, unlink 2.017 → 1.016. The second entry — the
   kernel's post-op ctime writeback echo, exactly 1.000/op per M2 — is
   **absorbed, not committed** (`meta_kv_times_echo_absorbed` = 1.0000/op
   on both storms while `meta_updates` stays 2.0/op: the wire message
   still arrives; the daemon just stopped paying an entry for it).
2. **The echo cannot be suppressed at the protocol level; the daemon must
   absorb it.** Trigger analysis (kernel source, `fs/fuse/`): with writeback
   cache (squeezefs default), `fuse_iget` skips `S_NOCMTIME` for regular
   files, so the kernel is the cmtime authority. After every unlink
   (`fuse_entry_unlinked`), rename (`fuse_rename_common`), link and setxattr,
   `fuse_update_ctime` stamps ctime from the **kernel's coarse clock**
   (`inode_set_ctime_current`), marks the inode dirty, and **synchronously**
   flushes it (`fuse_flush_time_update` → `sync_inode_metadata(inode, 1)` →
   `fuse_write_inode` → `fuse_flush_times`) as
   `FUSE_SETATTR(FATTR_MTIME|FATTR_CTIME[|FATTR_FH])` with **mtime unchanged**
   (the daemon's own round-tripped value) and ctime = kernel-now. Rename and
   unlink replies carry no attr surface, so no reply/TTL shaping can pre-empt
   the dirtying; attr invalidation does not un-dirty a VFS inode. An
   idempotent byte-match skip cannot fire either: the kernel's
   jiffy-resolution coarse clock stamps a different — on this box, usually
   *earlier* — instant than the daemon's fine-grained in-tx ctime (measured
   live: echoes arrive at-or-behind the staged ctime and absorb as monotonic
   no-ops).
3. **D4.b stands as designed**: `routed_rename_local` is ONE `KvTx` carrying
   dentry surgery + both parents' Δtime merge records (folded into the
   dir-move nlink `Put`s where those already rewrite the parents) + the moved
   inode's Δctime + dest accounting. The POSIX parent-mtime-on-rename gap
   closes as a side effect (test-pinned), the crash window between the old
   fragments is gone (power-cut test: naming AND times revert or commit
   together), and strict mode pays one barrier where it paid two.
4. **Crash contract preserved**: whole-tx atomicity, torn-write immunity and
   replay idempotence untouched (the drain is an ordinary `commit_tx`);
   kill-9 soak 100/100 rounds green, 0 torn drops. Absorbed refinements are
   µs-grade ctime polish pending a drain; kill-9 loses at most the
   since-last-drain window of them — never the op tx, whose own
   daemon-authored ctime (same instant class) is journaled exactly as before.

## The mechanism shipped (absorb → fold → drain)

- **Absorb** (`setattr_locked`): a times-only SETATTR whose mtime is
  absent-or-equal to the *folded* stored mtime parks its ctime refinement in
  a latch-free per-volume `pending_times` map (`scc::HashMap<Ino,(mtime,ctime)>`,
  O(1) count) — zero journal entries, counted in
  `meta_kv_times_echo_absorbed`. A refinement at-or-behind the folded view
  persists nothing (ctime never regresses). Everything else — changed mtime
  (the buffered-write `fuse_flush_times` shape carrying kernel-authored write
  times), explicit utimes (atime present, exact-set), chmod/chown/truncate —
  commits exactly as before, folding and then retiring any pending
  refinement so a dead echo never max-clamps an intentional exact set.
- **Fold** (read side): every inode read (`getattr` → lookup / routed reads /
  readdirplus attrs) overlays the pending refinement with monotone
  per-field max — stat after unlink-of-hardlink shows the echoed ctime while
  it is still pending (POSIX visibility test-pinned).
- **Drain** (durability): batched Δctime transactions — ONE entry per batch
  (≤ 128 inos), per-ino DLM exclusive guards through `lock_many`'s canonical
  order, staged only for inos the refinement still advances (destroyed /
  superseded inos GC recordlessly). Triggers: a dedicated per-volume drain
  task on the flush cadence (100 ms in strict mode; deliberately NOT a
  checkpoint-tick step — a drain parked on ring admission inside the
  checkpoint task would deadlock the drain that frees the ring), cap
  crossings (512 pending), fsync/fsyncdir (`sync_device_for_ino` /
  `sync_all_devices` drain before the barrier so the barrier covers the
  refinement), and clean unmount (drains while the write gate is open, then
  joins the task — no leaked tasks).
- **Counters**: `meta_kv_times_echo_{absorbed,drain_commits,drained}` +
  per-volume `meta_kv_times_echo_pending` gauge on the stats inode.

## Journal-entry economy through the mount (G4 table)

100 k ops/phase, 8 threads, one dir, real kernel + FUSE-over-io_uring;
`.stats` deltas per phase. `absorbed/op` = `meta_kv_times_echo_absorbed`;
`meta_upd/op` = FUSE mutation handlers (the wire SETATTR **still arrives**
— 2.0/op on rename/unlink for both tips; the daemon absorbs it).

| tag | phase | entries/op | barriers/op | absorbed/op | drain commits | drained |
|---|---|---:|---:|---:|---:|---:|
| B1 default, dev tip | rename | **2.0021** | 0.0029 | 0 | 0 | 0 |
| B1 default, dev tip | unlink | **2.0190** | 0.0048 | 0 | 0 | 0 |
| B1 default, **M6 r1** | rename | **1.0029** | 0.0035 | 1.0000 | 84 | 226 |
| B1 default, **M6 r1** | unlink | **1.0180** | 0.0044 | 1.0000 | 12 | 12 |
| B1 default, **M6 r2** | rename | **1.0031** | 0.0035 | 1.0000 | 110 | 769 |
| B1 default, **M6 r2** | unlink | **1.0179** | 0.0044 | 1.0000 | 6 | 6 |
| A2 nocow, dev tip | rename | 2.0021 | 0.0028 | 0 | 0 | 0 |
| A2 nocow, dev tip | unlink | 2.0190 | 0.0046 | 0 | 0 | 0 |
| A2 nocow, **M6** | rename | **1.0041** | 0.0034 | 1.0000 | 203 | 1142 |
| A2 nocow, **M6** | unlink | **1.0179** | 0.0043 | 1.0000 | 7 | 8 |

- **G4: rename ≤ 1.02 → measured 1.0022–1.0041; unlink ≤ 1.05 → measured
  1.0178–1.0190** (from 2.002 / 2.019 same-session dev rows, which
  byte-agree with the baseline). Unlink decomposes exactly as modeled:
  `1 (op tx) + 1/fill (destroy batches: 1,563–1,564 ≤64-bucket fills per
  100 k — gather still healthy, M2's verdict re-confirmed) + ambient`.
- Create is untouched: 1.0054 entries/op, 5.17–5.18 fuse_ops/op on both
  tips (G7's surface unchanged).
- The drain terms: only 0.2–1.1 % of echoes carry a ctime that actually
  advances the stored value (the kernel's coarse clock usually stamps
  at-or-behind the daemon's fine-grained in-tx ctime on this box); those
  land in 6–203 batched drain commits per 100 k ops — the 0.0001–0.002/op
  amortized residual inside the totals above. `meta_kv_times_echo_pending`
  reads [0] at every session end (cadence/unmount drains converge).
- **One honest cost**: rename's fuse_ops/op rose 4.97 → 5.18–5.20 (+0.21).
  The parents' mtime/ctime now actually change on every rename (the POSIX
  gap this PR closes), so the kernel revalidates parent attrs it used to
  serve from a stale-but-unchanging cache. Create/unlink fuse_ops are
  unchanged (5.17/5.88).

## Strict/default paired rows on B1 null_blk (barriers are real there)

`SQUEEZEFS_META_FLUSH_INTERVAL_MS=0`, same session, same substrate;
`barriers/op` = `meta_device_syncs` delta / ops (real `fdatasync` on a
`fua=1, write back` null_blk — the substrate class the design mandates for
barrier work).

| tag | phase | entries/op | **barriers/op** | absorbed/op |
|---|---|---:|---:|---:|
| B1 strict, dev tip | create | 1.0053 | 1.0009 | 0 |
| B1 strict, dev tip | rename | 2.0022 | **2.0003** | 0 |
| B1 strict, dev tip | unlink | 2.0190 | **2.0165** | 0 |
| B1 strict, **M6 r1** | create | 1.0054 | 1.0008 | 0 |
| B1 strict, **M6 r1** | rename | 1.0022 | **1.0004** | 1.0000 |
| B1 strict, **M6 r1** | unlink | 1.0184 | **1.0167** | 1.0000 |
| B1 strict, **M6 r2** | rename | 1.0022 | **1.0006** | 1.0000 |
| B1 strict, **M6 r2** | unlink | 1.0178 | **1.0161** | 1.0000 |

**Strict rename lands at 1.000 barriers/op (from 2.000)** — the M6 target
"approach 1 barrier/op" is met exactly; strict unlink at 1.016 (op barrier
+ 1/64 amortized destroy-batch barriers — the same decomposition as its
entry count). The absorbed echo pays no barrier in strict mode; its
refinements ride the drain task's own batched commits (100 ms cadence
there), 3–52 per 100 k ops.

**Throughput rows: context only, flagged.** A co-tenant experiment
(juicefs-vs-squeezefs, load 19–28) occupied the box for the whole session
window; every timed row carries the harness's `DIRTY` post-probe and the
dev-vs-M6 pairs ran at *different points of a rising load ramp*, so
ops/s are not comparable this session (recorded in `results.tsv` with
per-row load/tctl headers — e.g. under that load B1 strict unlink read
3,708/s dev vs 4,263–4,284/s M6, strict rename 5,694 dev vs 5,345–5,356
M6; treat both as load-confounded). The per-op **counter ratios above are
load-invariant** — they are exact integer deltas over exact op counts —
and they are the G4 gate. The baseline's quiet-gated B1 rows remain the
throughput reference; the entry/barrier halving predicts its throughput
effect on barrier-bound substrates per the design's lever-4 row
(single-digit % deferred; ×~2 barrier-count reduction strict).

## Red→green evidence (the TDD trail)

- Test commit `0ec7159` (RED): `mount_shaped_rename_meets_g4_one_entry_per_op`
  measured **2.002 entries/op** in-sandbox — byte-agreement with the
  baseline row — with the second committer site-attributed to the setattr
  tx; the unlink echo leg landed 320 commits for 320 echoes; the regressive
  echo committed; drain counters dead; stripe-test rename left parent mtime
  unchanged; the crash test's fully-new leg lacked the old parent's time
  updates in the entry.
- Implementation commit `339760d` (GREEN): all four flip; mount-shaped
  rename measures 1.001–1.002 entries/op in-test; unlink model closes at
  ~1.02 (`1 + 1/fill + drain noise`); the pending-refinement visibility,
  monotonicity, exact-set-retirement, and drain-durability-across-remount
  contracts all pass. Full suite 766/766 green (`--test-threads=1`), clippy
  `-D warnings` clean, fmt clean, `doc --no-deps` zero warnings, bench smoke
  green.

## External verification

- **fstests (root, singles per the M6 verify row)**: `generic/001`,
  `generic/013` (QUICK-set rename/unlink churn) and `generic/074` (run by
  judgment: the times-flush eligibility branch sits adjacent to the
  buffered-write mtime path) — **3/3 pass** at the branch tip.
- **Kill-9 soak** (`SQUEEZEFS_CRASH_ROUNDS=100`): 100/100 rounds green —
  acked-durability, whole-tx atomicity, replay-twice digest equality, empty
  replay window after clean shutdown; `replay_dropped_torn = 0` throughout.
  (Run although the reclaim gather is untouched — the unlink tx shape
  changed.)

## What M2's evidence changed (recorded per the mission)

The design's round-1 D4.c target — "fix the reclaim batch-fill
degeneration" — is **superseded by measurement**: M2 pinned destroy-batch
fill ≈ 62 (healthy, cap-dominated close reasons), contributing only
1/fill ≈ 0.016/op. The reclaim gather machinery
(`queue_reclaim_inode` / `drain_reclaim_batch` / `reclaim_semaphore`) is
therefore **untouched** by this PR; its fill-vs-window attribution counters
and the zero-commit inline-teardown pin (`delete_file` on an inline/empty
corpse lands ZERO entries) remain as regression guards from M2, re-verified
green here.

## Honest residuals (→ M7)

1. **The drain-commit residual** (`meta_kv_times_echo_drain_commits`):
   6–203 entries per 100 k ops (0.0001–0.002/op) — already inside the G4
   totals. If M7's conveyor wants it, drains can enqueue as ordinary
   conveyor transactions and amortize into user batches; the counter is
   the hand-off. Nothing further is *required* for G4.
2. **Refinement durability window**: an absorbed refinement is RAM-only
   until its drain (≤ flush cadence / 100 ms strict / 512-entry cap /
   fsync / unmount). Kill-9 inside that window reverts ctime to the op
   tx's own daemon-authored stamp (µs–ms earlier, same logical instant) —
   POSIX makes no crash promise for un-fsync'ed timestamps, fsync drains
   first, and the kill-9 soak stays green. Recorded, not hidden.
3. **Rename fuse_ops +0.21/op** (parent-attr revalidation after real
   parent-time updates): a correctness cost, listed for the G7
   round-trip accounting (create — G7's row — is unchanged at 5.17).
4. **Same-session quiet throughput rows**: not obtainable this session
   (co-tenant load, every row DIRTY-flagged); counters carried the gate.
   The next quiet B1 session (M7's acceptance is the natural slot) should
   collect the clean before/after ops/s pair.

## Artifacts

`~/tmp/m6_econ_20260714/`: `results.tsv` (quiet-gated rows), `stats/<tag>/`
(per-phase pre/post `.stats` snapshots), `logs/` (daemon logs), `session.log`,
harness (`lib.sh`, `run_m6_storm.sh`, `run_session.sh`, `analyze_m6.py`).
Substrates via the baseline's `setup_substrates.sh` / `teardown_substrates.sh`
(B1 recipe unchanged), torn down at session end.
