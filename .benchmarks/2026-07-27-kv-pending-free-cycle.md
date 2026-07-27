# 2026-07-27 — The §4.7 pending-free pinned-floor cycle: verified, broken structurally, field-recovered

Branch `fix/kv-pending-free-cycle` off dev `88946b4` (left unmerged for
review). Charter: `.benchmarks/2026-07-26-fuse-per-op-economy.md` §9 (the
P2 campaign's timeboxed analysis + preserved evidence). USER LAW: we
can't have wedges — this was a live wedge vector with three faces, all
closed here.

## 1. The mechanism, verified independently (contract item 1)

**Flake reproduction (dev `88946b4`, debug build, 24 CPU-spinner load,
declared rate gathering):** `kv_smo_crash_completeness_tests::
pending_free_at_cap_forced_cycle_completes_and_conserves_extents`
**3/60 failures** (runs 23/32/48; §9 reported 1/40 idle, 7/60 under
load). Failure signature identical every time: point-B quiesce leaves
`pending == 2`.

**Own-instrumentation confirmation (KVDIAG probes, working-tree only,
stripped before commit — recipe: env-gated `eprintln!` at the flush
pass's `PendingFreeFull` skip arm and at the §4.6 pt 2 tail
computation):** failing-run trace, 12 identical cycles:

```
[KVDIAG] flush skip-defer node=0x870000 tree=1 pending=2
[KVDIAG] cycle tail=34431 h=183259 inflight=MAX dying=MAX
         min_floor=34431 tree=1 node=0x870000 pending=2
```

Exactly §9's shape: the two parked frees carry gates ≈79–118k; the tail
is pinned at 34431 by dentries-leaf `0x870000`'s dirty floor; that
node's flush needs a compaction SMO; the at-cap FIFO refuses it at
admission (clause a); the flush pass skip-defers (clause c), restoring
the ancient floor. Closed dependency cycle:

```
{parked frees await durable tail > gate}
→ {tail awaits node 0x870000's floor}
→ {floor awaits that node's compaction SMO}
→ {SMO awaits FIFO headroom (clause-a admission)}
→ {headroom awaits the parked frees}
```

`checkpoint_now()` cycles complete forever without progress (the
clause-b audit lived only on the `run_maintenance` arms — the audit
bypass §9 named); the maintenance arms fail the volume loud after 8
stalls **on a shape that is resolvable by construction**; and the wedge
is **persistent**: remounts re-park the in-window frees and replay the
same log-full floor.

**Field reproduction (the preserved image; COPY decompressed to
`~/sqz-wedge-work/wedged-copy.img`, original untouched and
`zstd -t`-verified):** full `KvMetaBackend::open` of a copy on dev
`88946b4` **fails after 92.3 s**: `"commit aborted while parked for ring
space"` — the mount's writer-claim commit (the volume's first
post-replay mutation, issued before the checkpoint task exists) parks
against a replay window of **12,203,688 B** (head 2096813200, reusable
2063979668) with **no drain source**, escalates 3× through the
journal-failure lattice, and the mount is refused. Additionally the
image's claimable budget is **zero** (`free=0`, reserve=243): the
pre-crash pinned tail parked **134 retirements** whose held bits are the
missing budget — so even a cycle that could run died on `NoSpace` at its
first compaction claim.

So the one §4.7 cycle has **three faces**: (a) the live at-cap livelock
/ loud-terminal-on-resolvable-shape, (b) the second closer found by the
red tests — successor floor inheritance (below), (c) the mount faces:
ring exhaustion at the claim gate + heap exhaustion aborting recovery
cycles.

## 2. The fix (three commits; progress GUARANTEED, argument in code)

**`4eeab64` test(meta)** — red-first (contract item 3): a deterministic
constructor (`construct_pinned_floor_at_cap`: ancient log-full XATTRS
floor + FIFO saturated with exactly cap younger INODES retirements; no
fault injection, no seams beyond `TEST_PENDING_FREE_CAP`) and four
contracts, **all four verified RED on dev src** (stash-run, deterministic
single-run failures) and GREEN with the fix:

| test | dev (red) | fixed |
|---|---|---|
| `pending_free_pinned_floor_at_cap_checkpoint_now_converges` | pending pinned at 2 after 8 direct cycles | converges in 2 cycles, extents conserved |
| `pending_free_pinned_floor_at_cap_maintenance_converges_never_fails_loud` (replaces the retired `pending_free_wedged_tail_fails_volume_loud_never_livelocks` — loud-on-resolvable was a wedge with a log line, and the remount re-wedged) | `is_failed()` latches | 120 at-cap rounds healthy, drains, conserves |
| `pending_free_wedged_shape_reopen_recovers_and_drains` | replayed frees never drain | remount IS recovery |
| `ring_full_crash_mount_recovers_via_preclaim_drain` | mount refused via the lattice | pre-claim drain engages (checkpoint-counter-asserted), custody whole |

**`4068c29` fix(meta)** — the structural break, four legs:

1. **Forced retirement for the flush pass** (§9 fix direction a):
   `ExtCore::free_pending_forced` — at cap the retirement parks in an
   unbounded mutex-guarded overflow (`overflow_len` keeps hot paths
   lock-free when empty; same gate seqs, same non-decreasing push order,
   same durable-tail release clock). Threshold SMOs keep the clause-a
   valve — pressure still forces checkpoints; **the checkpoint itself is
   never refused the retirements it must park**. Engagement counter
   `meta_kv_pending_free_overflow` (stats inode; ≈ 0 steady state).
2. **Exact successor floors** — the cycle's SECOND closer, exposed by
   the red maintenance test: the SMO leftover-overlay transfer inherited
   the PREDECESSOR's floor, so under continuous same-leaf churn the
   ancient floor chained through every successor generation (KVDIAG:
   tail fixed at 120686 across 9 cycles through nodes 0x88..0x92 while
   pending grew +1/cycle) — leg 1 alone did NOT unpin the tail.
   `OwnedRec.entry_floor` (containing entry's start, stamped at
   `apply_locked`) makes each successor's floor the min over the records
   it actually receives. FIND-SMO-TAIL §1b holds: stamps ARE entry
   starts.
3. **Audit centralized** (§9 fix direction b): the clause-b progress
   audit moved into `checkpoint_cycle` — every barriered cycle rides it,
   `checkpoint_now`/`checkpoint_past`/shutdown included. Progress = a
   release OR a ledger-tail advance; 8 consecutive stalls with
   retirements parked ⇒ loud. `force_pending_free_cycle` deleted.
4. **Remount recovery**: mount-side re-parking of replayed in-window
   frees is forced (a pre-crash pinned tail legitimately parks more than
   the cap — the old loud refusal made such images UNMOUNTABLE), and the
   claim gate preflights ring headroom (clamped to what the geometry can
   ever admit) and runs bounded barriered recovery cycles inline before
   the claim commit.

**Progress argument** (in full on `KvTree::smo_replace`; why no schedule
re-closes the cycle): in any barriered cycle at head `H`, the flush pass
visits every dirty node once and no visit is refused for FIFO reasons —
every floor `< H` is discharged (appended, or compacted with the old
floor dying into that cycle's dying-floor clamp). Every floor live at
the next cycle belongs to records `≥ H`, so the next barriered cycle's
tail is `≥ min(H, oldest in-flight reservation)` — past every gate
parked before `H` (gates are journal seqs `< H` by monotonicity). Any
parked retirement therefore drains within **two barriered cycles**;
overflow occupancy is bounded by ~one flush pass of SMOs (≤ the
dirty-node checkpoint cap). The only remaining stall is a tail pinned by
something no flush can discharge (a stuck in-flight reservation) — which
is exactly what the centralized audit fails loud, bounded.

**`03e8936` fix(meta)** — the ENOSPC ratchet (face c2, found ON the
image): a flush-pass compaction that cannot claim its successor extent
(`NoSpace` — the parked retirements hold the budget) skip-and-defers
like the reserve arm instead of aborting the cycle: the cycle still
barriers and releases what its tail covers, returning budget for the
next cycle's claim. Plus release-on-error for the whole SMO build (a
mid-build claim failure leaked earlier claims' bits — poison exactly
when extents are scarcest).

**Crash contract untouched:** zero on-disk change — same free records,
same journal entries, same historical gate tags byte-for-byte; the §2-A
coverage gate applies identically to both containers; whole-tx atomicity
and torn-write immunity are not touched (no change to entry format,
reservation protocol, or replay). Lock order 4b untouched: the taker
populations and their ordering are unchanged — only the flush pass's
refusal arm and RAM floor bookkeeping moved; the overflow mutex is a
cold-pressure-path container owned by the same serialized producer.

## 3. Loom (contract item 5)

New model `alloc_ext_forced_overflow_gate_and_conservation` (43rd):
forced push at cap racing `advance_durable` — the forced retirement is
never claimable before a durable tail covers its gate **wherever it
parked** (ring or overflow), racing claims of covered entries prove
their gates, and the population settles exactly (no leak in either
container, no double release). `tests/run_loom.sh`: **43/43 green**.

## 4. Field-recovery proof on the preserved image (contract item 4)

Out-of-tree probe (`~/sqz-wedge-work/probe`, not part of the repo):
full-opens an image copy (the same writer-claim + replay path a mount
runs), drives explicit cycles, and reports convergence.

- dev `88946b4`: **OPEN FAILED after 92.3 s** ("commit aborted while
  parked for ring space") — the field mount refusal, reproduced.
- fix (pre-ENOSPC-ratchet): open failed at 1.25 s with
  `NoSpace { free: 0, reserve: 243 }` — face c2, which drove `03e8936`.
- **fix (final): the image RECOVERS.** Open in **1.35 s**: the pre-claim
  preflight detects the exhausted ring, one recovery cycle defers 3
  compactions on the zero heap, releases **128 parked frees** (budget
  0 → 128), converges, claim taken. Post-open: cycle 0 releases the
  rest (parked 134 / released 134), cycle 1 lands `pending_free=0`,
  `window=0`; mutation probe acks; clean shutdown. Verdict: CONVERGED.
- Determinism: a second fresh copy run converges identically.

**Operator recovery path (documented for wedged volumes in the field):**
upgrade the binary and **mount the volume** (or `KvMetaBackend::open`
via any guarded verb). No fsck, no flags, no manual steps: the claim
gate detects the exhausted ring, logs
`"recovered replay window exhausts the journal ring … running pre-claim
recovery checkpoint cycles"`, ratchets the parked retirements back into
budget, and completes the mount. `meta_kv_pending_free` drains to 0
within the first cycles; `meta_kv_pending_free_overflow` counts the
cycle-break engagements.

## 5. Soaks (counted from zero on the fixed binary; contract item 3)

- The former flaky test as soak witness, **×60 under the same 24-spinner
  load, from zero: 60/60 GREEN** (dev baseline under identical load:
  3/60 red).
- The four new contracts ×5 each: **20/20 green**.
- Full `kv_smo_crash_completeness_tests` binary (12 tests incl. the four
  new contracts): green.

## 6. Residuals (recorded, not chased)

- **The reserve-arm twin**: `JournalReserveExhausted` skip-defer could in
  principle form the analogous cycle if one pinned window accumulated
  > 256 KiB of SMO entries (~2,000+ log-full nodes in one cycle). With
  the tail now guaranteed to advance every ≤ 2 barriered cycles, the
  accumulation cannot build up; the centralized audit bounds any
  remaining schedule loud. Not separately exercised.
- The `checkpoint_past` helper keeps its own 8-cycle bound (now
  redundant with the centralized audit but harmless).
- Superblock `plan()`'s journal floor (256 KiB + `MAX_ENTRY_LEN` raw)
  does not account for page-header exclusion, so a floor-size ring's
  user slice is slightly under one max entry — pre-existing; the
  pre-claim preflight clamps to the geometry so it cannot misfire on
  such rings. A format-side floor correction is future hygiene.
- `crash_kill_tests::test_kill9_remount_soak_v3_batched` (P2 §10's
  recorded pre-existing 1/8 kill-9 flake on dev) is in this
  neighborhood but distinct; not addressed here.

## 7. Gates (branch tip = the three commits + this note)

- `cargo clippy --all-targets --all-features -- -D warnings`: clean.
- `cargo fmt --check`: clean (root; the fork untouched).
- `cargo test --all-features -- --test-threads=1`: the from-zero
  acceptance run is **green end-to-end — 139 test binaries, 0 failures**
  (`--no-fail-fast`, rc 0). One earlier aborted attempt is recorded per
  the multi-run discipline, adjudicated NOT-this-branch:
  `crash_kill_tests::test_kill9_remount_soak_v3_batched` — the
  **pre-existing kill-9 timing flake P2 §10 already recorded at 1/8 on
  UNTOUCHED dev** ("acked create lost after a mid-batch kill-9", same
  signature verbatim); **10/10 green isolated on this branch binary**
  and green in the from-zero acceptance run. It still needs its own
  red-first loop in the kv program (P2 §10's standing note).
- `cargo doc --no-deps`: 3 warnings + summary — byte-identical to the
  P2-recorded pre-existing intra-doc-link nits (ipc_service
  `handoff_spawn` ×2, `GhostTable`); zero delta.
- `cargo bench --benches -- --test`: green (bench smoke).
- loom (`tests/run_loom.sh`): **43/43 green** (42 + the new
  `alloc_ext_forced_overflow_gate_and_conservation`).
- `sudo FSTESTS_QUICK=1 tests/run_fstests.sh` (the standing regression
  set, fail-fast): **exit 0 — 41 ran, 39 clean, 2 expected-shape
  (the adjudicated noatime 003/192 class, pinned shapes matched
  exactly), 0 unexpected**. The metadata-heavy rows (mount-cycle,
  million-entry storms via the cargo suite, fsx/fsstress soaks incl.
  generic/795 at 174 s) exercise the conveyor/checkpoint paths on the
  fixed binary.
- Soak ×60 under load, from zero: **60/60 green** (§5).
- KVDIAG probes: working-tree only during verification, stripped —
  `grep -rn KVDIAG src/ tests/` is empty at the branch tip.
