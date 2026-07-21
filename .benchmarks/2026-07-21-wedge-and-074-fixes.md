# The writes-only wedge + generic/074 stale-unit fixes (VL8 catalog items 2 & 1)

Branch `fix/write-wedge-and-074` off `dev` @ `6ff2b3a`, 2026-07-21. User
ruling: "we can't have wedges" — the two OPEN rows of
`.benchmarks/2026-07-21-vl8-stabilization-catalog.md` (item 2 capture 2,
item 1) were overruled as documented exceptions and root-caused-and-fixed
before the VL10 release gate. Tests-first red→green throughout; every
externally-found failure's fix carries its cargo repro (the repro-port
mandate); counted runs restarted from zero after every fix.

Instrument note: fstests runs via `sudo tests/run_fstests.sh generic/NNN`
(file-backed `/dev/shm` volumes, tmpfs staging, release binary); cargo
repros are the in-process harnesses named per item.

## Target A — the generic/464 writes-only wedge (catalog item 2, capture 2)

**Signature** (capture `/tmp/vl8_fstests/wedge464/*`): 24 WRITE handlers on
10 inodes permanently in flight (2200+ s), watchdog lines only, no
cfr/fallocate/open involvement, transport/conveyor/device healthy, every
thread idle-parked. Onset in the first second of a remount that had
recovered 3 extent records after a StorageFull dismount, under the 464
staging-ring oversubscription storm.

**Root cause** (named by the capture's own gdb stacks — three
`tokio-rt-worker` threads blocked at `cache/nvme.rs:1143`, one
blocking-pool thread holding the ledger bucket inside
`reserve_and_write@685`, shard writers queued in `wait_for_readers`): the
staged-budget ledger (`NvmeStaging::staged_ledger`, an scc map) is a
SECOND Hang-1 lock population that no rule governed. By design a
re-stage's blocking-pool closure holds a file_id's ledger ENTRY lock
across `reserve_and_write`'s staging-shard WRITE-lock wait, and that wait
is unbounded while a §5.5 read guard rides an await. Any ledger `*_sync`
scc op from an async executor thread then BLOCKS THE THREAD on the
bucket — and when the §5.5 guard holder's future lives on that same
executor (fuse3 TPC current-thread LocalSets pin handler futures), the
four-edge cycle closes:

```
H (executor thread, stage_write prior-cost read_sync)  blocks on
B (ledger bucket, held by a re-stage closure on the blocking pool)  waits
S (staging shard WRITE lock)                                        waits
A (§5.5 read guard held across an await)                whose wake needs
H's blocked executor thread.                → writes wedge forever
```

Kernel-side effect: the victims' WRITEs never complete, `nr_writeback`
pins, and every subsequent write to a victim ino queues forever — the
writes-only census.

**Fix — the ledger-lock invariant** (`8e72255`): executor-side ledger
access parks the TASK, never the thread (scc `*_async`):
`stage_write`'s prior-cost read (`read_sync` → `read_async` — the exact
captured convict), `kick_promotion` (`iter_sync` → async + `iter_async`),
`staged_generation` (→ async; all three callers were async-context).
Same-class rule-2 escapes closed with it: `dispose_bad_extent_record`'s
torn-record ring removal is now a detached blocking-pool task; live-lane
fsck's custody discard rides `remove_active_block_async`.

**Named-holder census** (`cefb3c5`, permanent watchdog improvement): the
overdue-op list alone could not draw the cycle (capture 2 needed gdb).
The D1.b watchdog now appends a `lock-wait census` — live contended
waiters on `BLOCK_FLUSH_LOCKS` (all ten sites, including the item-7 read
escalation) and `INODE_META_LOCKS`, plus each blocked stripe's
last-holder `(site, ino, block)` word — whenever overdue ops exist.
Contended-path-only slab claim; the uncontended fast path pays one
`try_lock` CAS.

**Repro** (red `85e5f0e`): `tests/staging_shard_deadlock_tests.rs::
test_ledger_read_never_blocks_executor_while_bucket_holder_waits_shard_write`
— a deterministic in-process reconstruction of the four-edge cycle on one
current-thread LocalSet (the §5.5 guard holder, the same-file_id re-stage
on its own OS thread, the executor-side third stage, the DMA-shaped
external gate). Pre-fix: 30 s deadline expiry, every run. Post-fix: green
in ~1.3 s.

**Counted runs (final binary, count started after the fix landed)**:
`generic/464` ×10 — **zero wedge signatures** (zero watchdog lines in all
20 daemon logs), every run completing in ~3 min. All 10 runs show exactly
the chartered FIND-RW5-A EIO signature (`echo: write error:
Input/output error` × N + the missing "Silence is golden") — the
documented expected-fail (rand-write closing §10 residual 8), logged per
the count discipline, not count-aborting. Artifacts:
`/tmp/wedge464/fixed464_run{1..10}*`.

## Target B — generic/074 fstest.4 sub-page staleness (catalog item 1)

**Signature** (074_run9 + vl4 run_3 + this session's pre-fix
`base_run9`): ~1/20 runs, fstest.4 (`-n 3 -F -l 10 -f 5 -s 10485760 -b
512 -mS`) verify finds a ≥512-B run starting at a PAGE-ALIGNED offset
reading exactly one loop stale (`8d`-for-`8e`, `7c`-for-`7d` at offset
188416 = page 46 in this session's own pre-fix reproduction), daemon logs
clean, all counters silent.

**Root cause** (red `b0dec08`): **`open(O_TRUNC)` never truncated daemon
state.** The fuse3 fork blindly echoes `FUSE_ATOMIC_O_TRUNC` back to the
kernel at INIT, so the kernel sends O_TRUNC as a flag on `FUSE_OPEN`,
truncates its OWN page cache/i_size, and never sends the SETATTR(size=0)
fallback — and `SqueezefsFilesystem::open` ignored `flags` entirely.
Every fstest loop's `open(O_TRUNC)` was therefore a daemon-side no-op:
the previous generation's ENTIRE state survived — size authority (the
un-truncated durable size can even resurrect after the 1 s attr TTL),
block map, staged ring images, parked overlays, staged extent records.
fstest `-F` (`do_frags=2`) writes only every OTHER 512-B unit through
mmap, so its stores FAULT each page in first — a read the daemon serves
from the un-truncated previous generation — and the whole
cross-generation compose surface (record-over-base ordering, fold seeds
resolved through the never-pruned old map, W1 in-place patches into old
keys, RMW seeds) stayed live across loops. The page-aligned
stale-by-one-loop verify hit is that residue surfacing through a
daemon-served read (page-cache eviction / attr-inval window).

**Fix** (`2909cfa`): `open` routes O_TRUNC through the setattr size-0
path — same inode-guard order, same overlay/staged/extent-record prune,
same backend commit as an explicit truncate-to-zero — before the open
completes. (`create` needs nothing: the backend refuses EEXIST, so
CREATE never opens an existing file.)

**Repros** (`tests/mmap_writeback_staleness_tests.rs`, all red pre-fix /
green post-fix — bidirectional-verified by stashing the fix in this
session):
- `open_o_trunc_truncates_daemon_state` — FUSE_OPEN(O_TRUNC) must zero
  the size authority and leave holes (no SETATTR ever follows).
- generation soaks (default + adversarial fold/spill knobs, 7 seeds, 3
  concurrent files each): per-loop `open(O_TRUNC)` → `ftruncate(N)` →
  the `-F` stride-2 faulting mmap-writeback model (fault-must-read-zeros
  is the truncation assertion) → seeded out-of-order coalesced
  concurrent page WRITEs → FLUSH → per-512-B verify.
- A 512-seed × 2-knob-profile sweep of the same soak ran green post-fix
  (declared rate-gathering, not acceptance).

**Counted runs (final binary)**: `generic/074` ×20 — recorded below at
completion. Pre-fix incidence was re-confirmed live this session
(`base_run9` fired the exact signature on the 9th pre-fix run).

## Cargo gates

- Per fix: clippy `-D warnings` clean, `fmt --check` clean, targeted
  suites green (staging_shard_deadlock 3/3, mmap_writeback_staleness 3/3
  + 1 ignored probe, staged_generation_aba, staging_budget, writeback,
  staging_generation).
- Full `cargo test --all-features -- --test-threads=1`, `cargo doc
  --no-deps`, `cargo bench --benches -- --test`: recorded below.

## Counted-run results

- generic/464 ×10 (fixed binary): 10/10 wedge-free; EIO-signature rows
  logged (expected, chartered). COUNT COMPLETE.
- generic/074 ×20 (fixed binary): PENDING AT WRITE TIME — final tally
  appended below.
