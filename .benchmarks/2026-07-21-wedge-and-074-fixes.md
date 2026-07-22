# The writes-only wedge + generic/074 stale-unit + FIND-RW5-A fixes (VL8 catalog items 2 & 1; rand-write closing §10 residual 8)

Branch `fix/write-wedge-and-074` off `dev` @ `6ff2b3a`, 2026-07-21. Two
user rulings drove this branch: (1) "we can't have wedges" — the two OPEN
rows of `.benchmarks/2026-07-21-vl8-stabilization-catalog.md` (item 2
capture 2, item 1) were overruled as documented exceptions and
root-caused-and-fixed before the VL10 release gate; (2) mid-branch, the
FIND-RW5-A EIO class (generic/464, previously an accepted
documented-expected-fail) was ruled **no longer an acceptable
adjudication** — its charter (rand-write closing §10 residual 8) was
landed on this same branch. Tests-first red→green throughout; every
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

**Follow-up (found by the FIRST ×20 count, which run 2 aborted with a
NEW signature — `fstest.0 … Input/output error`, daemon
`FencingTokenExpired{1,2}`):** setattr's size path presented a bare
`get_fencing_token_ino` snapshot instead of holding the shared cached
op lease; a background acquirer re-acquiring after release dropped the
lease bumped the token between the snapshot and `save_metadata`'s
fence, and — with truncate now running on every `open(O_TRUNC)` — the
transient surfaced as EIO to `open(2)`. Fix (`ea16535`): truncate rides
`get_or_acquire_lease` (the write path's discipline — shared cache ⇒ no
bump, or the acquisition serializes behind the transient holder) +
invalidate-on-fenced hygiene. Race-loop repro (red `775b84a`,
`open_o_trunc_never_races_lease_churn_into_eio`) fired at iteration 1
pre-fix (Errno(5)); green ×300 post-fix. The ×20 count was RESTARTED
FROM ZERO per the multi-run discipline.

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

## Target C — FIND-RW5-A, the generic/464 EIO class (charter landing)

**The aborted first campaign (the honest record):** after Targets A and B
closed, a final-binary re-count of generic/464 ×10 was started. Runs 1–2
failed with exactly the chartered FIND-RW5-A EIO signature (`echo: write
error: Input/output error`); the user killed the count deliberately,
ruling that looping a slow test that fails-by-charter is pointless, and
redirected: **fix the charter itself.** Those two runs verified nothing
and count for nothing; the campaign below restarted from zero on the
final binary.

**The charter** (§10 residual 8): under a staging ring structurally
oversubscribed by live staged files (464's 200 × ~4 MB files vs the
harness's 500 MB `--disk-cache-size`), staged whole-image arms propagated
`StorageFull` to the user write as EIO instead of degrading to the
durable-spill escalation a sibling arm already had. A user write must
NEVER see EIO because the staging ring is full.

**The arm sweep (every writer into the staging ring, enumerated):**

| Arm | Pre-fix behavior | Disposition |
|---|---|---|
| A. `write_file` staged whole-image replace | already spilled durably on refusal | kept; now counted (`staged_spill_escalations`) |
| B. `fold_rider_record` same-key re-stage (fold of spilled extent records back into the staged image) | **propagated StorageFull → user EIO** | FIXED: durable spill — process_write → allocate → `bk:0:len` → write_block → publish, revalidated still-ours under the ino meta lock (orphan freed on loss), committed as `block_map[0]` with the CURRENT DLM generation, superseded staging released, rider record retired |
| C. `clone_file` staged dest stage | **propagated StorageFull → clone EIO** | FIXED: durable spill into the dest's `block_map[0]` (caller commits) |
| D. `put_extent_record` spill | never-lossy by construction (refusal falls through to the whole-image/records path) | unchanged |
| E. `put_active_block` park | never-lossy by construction (RAM park; R5-gauged) | unchanged |
| F. shrink/clip + in-place staged updates | in-place (no new ring admission) | unchanged |
| G. write-through fallback | parks on refusal by construction | unchanged |

The "extend-replace leg" named in the charter turned out to BE the
fold-rider re-stage (B) — the file's post-fold image re-admission.

**What the storm then convicted (seven faces, each red→green):** fixing
the StorageFull propagation removed the loudest failure and exposed six
more lineages under the same 16-proc storm, hunted with an env-gated
double-free forensics tape (`SQUEEZEFS_FREE_FORENSICS`, backtrace pairs
naming BOTH call sites of a double-release) and the named-holder census:

1. **StorageFull propagation** (arms B and C above). Red `2104a4a`, fix
   `2aadb83`. Counter: `staged_spill_escalations`.
2. **Binding-rebind exhaustion EIO** ("did not settle after N binding
   rebinds"): reader cohorts inherited a sibling's fill against an
   UNCHANGED binding under perpetual patch/replace churn, defeating the
   item-7 stripe escalation. Fix (in `2aadb83`): rebind bound 8→24 with
   exponential backoff after 4, and escalated attempts (≥2 losses) take
   the block stripe AND fetch device-true (bypassing the cohort
   single-flight). Red: `reader_cohort_survives_perpetual_patch_storm`.
3. **RELEASE lease-churn storm**: RELEASE dropped the shared op lease
   while other handles were open → `FencingTokenExpired{N,N+1}` storms
   under 464's open/close churn. Fix (in `2aadb83`): last-close-only
   lease drop + one fresh-lease retry in the write handler. Red:
   `release_while_other_handles_open_keeps_the_lease`,
   `write_never_surfaces_transient_lease_churn_as_eio`.
4. **Duplicate-reclaim double-free**: RELEASE and FORGET both enqueue
   reclaim for the same ino; two concurrent batches both admitted it →
   `delete_file` twice → block double-free → one device offset minted to
   two live owners (permanent incarnation churn on both = face-2's EIO
   engine). Convicted by the `block_double_frees` tripwire (df=38 on
   probe 13). Red `5f41e14`, fix `b6438de`: `reclaim_inflight`
   single-drive per-ino guard (leak-safe: failed destroys stay guarded).
5. **Merge dirty-authority violation**: `merge_block_mappings_if_epoch`
   based its RMW on the LAGGING backend while the RAM entry was DIRTY —
   resurrecting (and later re-freeing) displaced keys. Convicted by
   run17 forensics pairs (fold_upload_block / flush_one_active_block
   displaced-free pairs of one key). Red `6e7bdd7`, fix `7b23200`: a
   dirty RAM entry is the merge base, never the backend.
6. **Untracked-free containment (the alias-impossible pin)**: after faces
   4–5, residual merge-driven double-release pairs remained (run26 tape:
   10× fold↔fold, 3× DataRouter↔DataRouter, 5× mixed). `begin_free`'s
   untracked arm freed unconditionally — the second half of EVERY
   double-release re-entered the free list. At steady state no
   legitimate caller frees an untracked offset (allocation seeds the
   refcount; `recover_block` seeds every live reference; fsck's
   C2Leaked apply refuses untracked itself), so the arm now
   refuses-and-counts (`block_untracked_free_refusals`) — leak-safe
   (fsck C6 reconciles a genuine limbo), alias-impossible. Red
   `2e24339`, fix `80274a2`. The deep unification of the staged-layout
   commit families (promote / spill / truncate-clip / fold each
   displacing under different serialization — the source of the
   remaining benign double-release ATTEMPTS the refusal now absorbs)
   stays **chartered** as the successor item; `block_double_frees`
   stays a must-stay-0 tripwire and the refusal counter is its early-
   warning gauge.
7. **Staged-layout recovery seeding — the remount double-owner mint
   (the residual EIO engine, convicted by tape)**: the second counted
   campaign's run 1 still failed (12 settle-exhaustion EIOs, df=0 —
   the face-6 refusal held) and the full-forensics diagnostic runs
   named the real engine. generic/464 is a **dirty-dismount/remount
   cycler** (~every 15 s the scratch fs dismounts with 30–60 live
   staged files and remounts; every refusal/EIO cluster sits right
   after a "Running block allocator recovery" line, and every refused
   release paired with `<no recorded first free>` — a fresh process).
   The mount-time refcount walk (`recover_from_layout`) gated seeding
   on `file_type == "striped"`: staged files' durable `block_map`
   entries (the truncate-clip `bk:0:len` shape and the face-1 spills)
   stayed UNTRACKED on the fresh allocator and their offsets landed on
   the free list via the recovery gap-fill — `allocate_block` then
   minted the SAME offset to a second owner while the staged file's
   map still bound it. Reads of the stale-live binding can never
   settle (the 24-rebind EIO exhaust), and every later free of it is
   the refused-untracked signature. Fix: seed striped AND staged
   layouts (fsck's census already counted staged maps ungated;
   defrag's striped-only gate is movement policy, untouched). Red
   `5b1c5cb` (planted staged layout + cold remount: refcount-live,
   not free-listed, not double-allocatable), fix `46edb98`. Post-fix
   forensics probes: 464 ×3 PASS with settle=0, refused=0, claim=0,
   df=0 (pre-fix: 25–41 refusals + 0–12 settle-EIOs per run).
   Diagnosis tape committed permanent+env-gated (`943b03e`): refusal
   pairing, MERGE FORENSICS lines, alloc-source tags, the always-on
   CLAIM ANOMALY tripwire.

**Repro suite:** `tests/rw5a_never_lossy_tests.rs` (9 tests, all faces +
the never-lossy arms, in-process oversubscribed-ring harness with
rider-record-pinned fillers). Test hardening that rode along:
`9582c99` (crash_kill ready-file read race). Cross-checks: the earlier
O_TRUNC lease fix (`ea16535`) re-verified under 464's shape (green
race-loop + the counted 074 ×20 + 464 campaign below); zero-copy
write-path contract intact (spills ride the existing write-through
machinery — no new copies, no staging detours); lock lattice preserved
(spill commits take the same ino-meta → 4a/4b order; no new inversions).

**Probe chronology (honest record):** after face 6, generic/464 probed
×2 PASS with 0 DOUBLE FREE but the refusal containment "engaging"
22–34×/run (`/tmp/rw5a_probe/run29*,run30*`) — at the time read as
absorbed double-releases; in hindsight those refusals WERE the face-7
signature (post-remount frees of never-seeded staged-map keys). The
second counted campaign's run-1 failure forced the full diagnosis;
after face 7 the probes read settle=0, **refused=0**, claim=0, df=0
(×3) — the containment counters are quiet because nothing wrongful
happens anymore, which is the correct steady state.

## Cargo gates (final tree `943b03e`)

- `cargo clippy --all-targets --all-features -- -D warnings`: clean.
- `cargo fmt --check`: clean.
- `cargo test --all-features -- --test-threads=1`: **green, all 131
  test targets, 0 failures** (idle box). Two contract adjustments rode
  the branch: torn-record physical discard is eventually-consistent
  (disposal detached; unreadability immediate), and
  `write_through_tests`' fencing-expiry pin was ported to the face-3
  contract (`6da6fd0`: one transient adjacent bump converges; the
  router layer stays loud on genuinely stale tokens). Honest flake
  record: while the 464 campaign's 16-proc storm loaded the box, a
  concurrent suite run flaked
  `kv_smo_crash_completeness_tests::pending_free_at_cap_forced_cycle…`
  (1/3 in-isolation-under-load too) — a dev-lineage KV timing test this
  branch never touches (`src/meta_backend` delta vs dev: none); idle
  box 5/5 green and the counted gate run is the idle one.
- `cargo doc --no-deps`: 0 warnings.
- `cargo bench --benches -- --test`: green (22 harness smokes + lib
  bench targets, 0 failures).

## Cargo gates (per-fix detail)

- Per fix: clippy `-D warnings` clean, `fmt --check` clean, targeted
  suites green (staging_shard_deadlock 3/3, mmap_writeback_staleness 3/3
  + 1 ignored probe, staged_generation_aba, staging_budget, writeback,
  staging_generation).
- Full `cargo test --all-features -- --test-threads=1`, `cargo doc
  --no-deps`, `cargo bench --benches -- --test`: recorded below.

## Counted-run results

Historical counts (superseded — they verified EARLIER binaries and are
recorded per the multi-run discipline, never credited to the final one):

- generic/464 ×10 (post-ledger-fix binary `8e72255`+`cefb3c5`): 10/10
  wedge-free (zero watchdog lines across all 20 daemon logs); every
  run's only diff was the then-chartered FIND-RW5-A EIO signature.
- generic/074 ×20 (post-`ea16535` binary): 20/20 PASS — zero corruption
  signatures, zero .out.bad artifacts (`/tmp/loop074/fixed2_*`).
  Pre-fix contrast: the wild signature fired on run 9 of the pre-fix
  baseline loop in this same session (`base_run9`, offset 188416 = page
  46, 7c-for-7d).
- generic/464 re-count on that same binary: **ABORTED at run 2 by user
  ruling** — runs 1–2 failed with the chartered FIND-RW5-A EIO
  signature; looping a fails-by-charter test was ruled pointless and the
  charter was fixed instead (Target C).
- Second counted campaign (binary `7692621f…` @ `6da6fd0`, faces 1–6
  landed): **ABORTED at run 1** — 12 settle-exhaustion EIOs (df=0: the
  face-6 containment held; the refusals fingered a FIRST wrongful free).
  Per the multi-run discipline the count stopped, face 7 was convicted
  and fixed, and the campaign restarted from zero. Artifacts:
  `/tmp/campaign_aborted1/`.

**THE campaign — ONE final binary (`918b8ef2…` @ `943b03e`, faces 1–7),
all counts from zero (2026-07-22, artifacts `/tmp/campaign/`):**

- generic/464 ×10: **10/10 PASS, FULLY GREEN** — zero diffs of any kind
  (no `.out.bad` produced on any run), zero wedges/watchdog lines, and
  every forensic counter zero on every run: `did-not-settle` 0,
  `REFUSED untracked` 0, `CLAIM ANOMALY` 0, `DOUBLE FREE` 0. The EIO
  signature is GONE. COUNT COMPLETE.
- generic/074 ×20: **20/20 PASS** — zero corruption signatures, zero
  `.out.bad`, same all-zero counter row on every run. This also
  re-verifies the `ea16535` O_TRUNC lease fix under 464's own
  open(O_TRUNC) storm shape (the 464 leg exercises it 200-files-wide
  per run). COUNT COMPLETE.
- Instrument note: one harness incident during the restart — a stray
  artifact `mv` relocated the first restarted run's ACTIVE log file
  mid-run, making the wrapper mis-score a run whose fstests verdict
  was "Passed all 1 tests" (`/tmp/campaign_c1_hijacked_run1_PASS.log`).
  Scored as an instrument error, campaign restarted from zero anyway
  per the discipline; the counted 10+20 above are consecutive,
  untouched runs.
