# Design: Shared-Mode Write Admission — the Write-Inode-Convoy Campaign

Status: **DESIGN — awaiting review** (2026-08-10)
Branch (on approval): `perf/write-inode-convoy`
Evidence basis: the 2026-08-10 squeeze-test rand-4k diagnosis rows (§2) — captured
against dev tip `e4f756a9` on the production-shaped fabric rig (memp-s3ds-aqs-37,
32 CPUs, 5×2 NVMe volume set, `--interception` mount).

---

## 1. Charter

Random small **writes** on this filesystem are ceilinged by the per-inode
`active_inode_locks` **write** guard, not by the device, the shim, the meta
plane, or the W1 patch machinery. Remove the ceiling by admitting the
**non-mutating-layout write class** under the inode **read** (shared) guard —
a third class in the existing one-path lock-scope classifier — so that
same-inode writes serialize only where bytes actually conflict (the block
plane), the way reads already do.

**Non-goals (charter rails):**

* **No benchmark fork.** The change extends `inode_write_lock_scope()` — the
  classifier every write already passes through (`EntireOp` vs `MetaPrepOnly`
  today). One admission, always the cheapest *legal* guard
  (`docs/design-one-path.md`). Any shape in doubt routes to the exclusive
  class; the decision ledger (§7) makes classifier rot visible, exactly the
  `patch_ineligible_*` pattern.
* **No DLM/lease changes.** The cross-writer custody layer already implements
  the Lustre-style hold-until-conflict model this campaign was compared
  against (§3.1) — measured at **32 lease acquisitions for 23.5M writes** on
  the diagnosis row. It is not the bottleneck and is not touched.
* **No block-lock changes.** `BLOCK_FLUSH_LOCKS` + the W1 §5.1 fence protocol
  are precisely what make shared-mode admission safe; they stay verbatim.

---

## 2. Evidence: the four diagnosis rows (2026-08-10, squeeze-test)

Workload: rand-4k overwrite of 32 preconditioned 2 GiB files, 32 jobs ×
iodepth 32 (elbencho 3.1-11 dynamic, libaio), 60 s sustained, `--direct`.
Engagement verified per charter rule 4 on every row (`ipc_ops_write` /
`patch_writes` deltas account for the row's ops; write amplification 1.027).

| Row | IOPS | avg lat | Device qd (10 data vols) | Note |
|---|---|---|---|---|
| il (shim) 32×32 | 205k | 5.0 ms | ~1.0 each | 99.96 % W1 patch engagement |
| kernel 32×32 | 391k | 2.6 ms | ~1.5 each | same engagement; **il < kernel = write_matrix parity violation** |
| il, `SQUEEZEFS_IL_SESSIONS=16` | 206k | 5.0 ms | ~1.0 | sessions lever = no-op |
| kernel, 8 files | 266k | 3.8 ms | — | sub-linear in file count |

Phase decomposition of the kernel row (23.5M ops, always-on histograms):

| Term | mean | verdict |
|---|---|---|
| **`write_lock_wait`** (per-inode write guard) | **≈ 1,120 µs** | **the ceiling** |
| `write_transport_phase_ns.queue_wait` | 60 µs | healthy |
| `block_lock_wait` | 24 µs | healthy |
| `lease_lock_wait` | n = **32** total | lease layer already hold-until-conflict |
| dispatch / reply commit | ~1 µs | healthy |

The mechanism: with ~32 writers queued per inode, every guard **handoff pays a
tokio wake** (~15–30 µs); the guard's *held* time is only ~µs (the
`MetaPrepOnly` scope drops it before device I/O per P1-8), but the serialized
wake chain prices ~1.1 ms of queueing per op. The devices — capable of
38 GB/s on the same rig — idle at qd ≈ 1.5 while ~1,000 ops wait upstream.
Offered depth is irrelevant to a convoy: this is why the row's IOPS is
`files ÷ (wake chain)` and not `depth ÷ (device service)`.

Little's-law closure: kernel 391k × 2.6 ms ≈ 1,024 = the offered depth
(nothing lost in the client); 391k ÷ 32 inodes ≈ 12.2k/inode ≈ 1 ÷ 82 µs —
one op per inode per wake+service interval. The model predicts the measured
number to within noise on all four rows.

## 3. The three lock layers (and where the Lustre analogy lands)

| Layer | Today | Lustre analog | This campaign |
|---|---|---|---|
| **3.1 Lease/DLM** (cross-writer custody) | Cached per-ino lease, held across ops, fencing tokens minted from it; S9 custody leases extend this cross-mount (renewal + pull revocation) | LDLM lock caching: hold until conflict callback / LRU age-out | **Untouched** — already the model; n=32 proves it |
| **3.2 Per-inode RwLock** (process-local op ordering) | Every WRITE takes **write** mode; reads take read mode | local `i_rwsem` — and XFS's shared-mode DIO writes are the exact precedent | **The change**: eligible writes take **read** mode |
| **3.3 Block/extent locks** (`BLOCK_FLUSH_LOCKS`, 3.5 `INODE_META_LOCKS`) | Serialize same-block mutation; W1 §5.1 fence closes clone/patch races | LDLM extent locks | **Untouched** — the safety basis for §3.2's change |

The user-raised "acquire and hold until someone else wants it or it ages out"
is layer 3.1's law and it is already in force. Layer 3.2's contention is
*self*-contention among sibling ops of one client — there is no other client
to yield to; the correct classical resolution is compatible-mode concurrency
(shared admission), not longer holds.

## 4. Design

### 4.1 The classifier (one path, three classes)

`inode_write_lock_scope()` gains a third variant:

```rust
pub enum InodeWriteLockScope {
    /// Inline / layout-transition / RMW shapes: write guard for the whole op.
    EntireOp,
    /// Striped shapes that publish size/attrs: write guard, dropped before I/O.
    MetaPrepOnly,
    /// NEW — the non-mutating-layout write class: read (shared) guard.
    Shared,
}
```

**`Shared` predicate (every clause must hold; any failure falls to the next
class down — never an error, never a second admission):**

1. `file_type == "striped"` at the cached-layout probe (the same authority the
   W1 request-shape half reads).
2. The write is **within EOF**: `offset + len <= live size floor` — the
   *conservative* size read (`min` of attr/meta caches; a stale-low size can
   only route to the exclusive class, never admit wrongly). Extending writes
   mutate size ⇒ `MetaPrepOnly`.
3. No layout transition possible: the write cannot cross the staged→striped
   or inline threshold by construction of (1) + (2).
4. The ino has no in-flight `EntireOp`-class holder — guaranteed by the
   RwLock itself (shared acquisition waits out exclusive holders; no new
   protocol).

Deliberately **not** in the predicate: alignment, W1-patch eligibility, block
count, payload size. The Shared class is about what the op does to the
*inode plane* (nothing), not which data-plane vehicle serves it. A Shared
write may ride W1 patch, extent park, accumulation merge, or the overlay —
each already owns its block-plane serialization.

### 4.2 What the read guard still excludes

Truncate, punch/zero-range, setattr-size, rename-over, and the fsync flush
sweeps take the inode **write** guard today and keep it. Every one of them
continues to exclude the whole Shared population — the read guard is not a
weakening against the ops that actually conflict with within-EOF writes.

### 4.3 The invariant audit (the campaign's real work)

Every site that today relies on **writer–writer** exclusion via the inode
guard must be classified into one of: **(a)** already block-plane-covered,
**(b)** migrate to an atomic/synchronized form, **(c)** grounds to route that
shape out of `Shared`. Inventory from the current handler prelude and data
path — each row becomes a red-first test in PR 2:

| # | Site | Today's cover | Disposition (proposed) |
|---|---|---|---|
| 1 | `acquire_write_lease` per-ino mint/refresh | inode wr guard serializes | (b) — the lease cache is already an scc map with a per-ino entry; concurrent Shared writers converge on one lease (verify no double-mint; `lease_locks` (P1-9 layer 2) already exists for exactly this) |
| 2 | `note_last_write_end` stream stamp (W1 predicate 6) | wr guard orders the swap | (a/b) — already an atomic swap; under Shared it becomes advisory-ordered. Audit: a mis-ordered stamp can only mis-classify stream-adjacency → routes a write to a *heavier* path (fine) or declines a patch (fine). Never a correctness edge |
| 3 | Attr-cache mtime/size refresh post-write | wr guard orders updates | (b) — monotonic merge (size = max, times = latest); within-EOF writes don't move size at all |
| 4 | Coverage-union `record_write` + ActiveBlockBuf merge | block guard (already) | (a) — pinned by `write_through_coverage_tests` under kernel-split OOO segments |
| 5 | W1 patch decision + §5.1 fence | block guard + fence | (a) — the fence protocol was loom-verified for exactly concurrent clone/patch |
| 6 | Extent park / escalation maps | block guard scope (audit) | (a) after audit — `try_extent_park` runs inside the per-block future under the held block guard |
| 7 | Seed/park classification live re-derivation (the generic/551 fix) | under block guard, post-settle | (a) — deliberately built under the block guard, not the inode guard |
| 8 | Overlay install/settle one-authority screen | block guard | (a) — same |
| 9 | Orphan-discard probe (VL8 item 9 prelude) | wr guard | (c)/(b) — probe only fires when no handle/meta/attr is live; a Shared-class write on an open handle never probes. Keep the probe on the exclusive classes only |
| 10 | `mark_handle_dirty` / open-generation | atomic already | (a) |
| 11 | Rewrite-epoch/supersession registration | block guard + global epoch | (a) — CQE-supersession law is lock-serialized at the block, epoch-unique globally |

Any row that resists a clean (a)/(b) verdict during implementation becomes a
predicate clause — shrinking `Shared` is always legal; silently widening it
never is.

### 4.4 The il parity follow-up

The il row (205k) trails kernel (391k) beyond the convoy's share; the handoff
venue (`handoff_spawn` → handler lanes) joins the same per-inode queue today.
After the convoy falls, re-measure il-vs-kernel on the same rows; if the
parity verdict (il ≥ kernel at minimum) still fails, that residual is PR 4's
charter (likely the §5.5.2 sever + handoff pipeline depth per inode), *not*
part of this design's core.

## 5. Lock-order statement (P1-9 delta)

Order is unchanged: (1) `active_inode_locks` → (2) `lease_locks` →
(3) `BLOCK_FLUSH_LOCKS` → (3.5) `INODE_META_LOCKS` → (4) meta-tx locks.
The only delta is the **mode** taken at layer (1) for the Shared class.
Shared-mode holders acquire nothing new; exclusive-class ops are unchanged;
no new wait-for edges, so the acyclicity argument carries verbatim. The
`AGENTS.md` lock-order table gains one sentence, no reordering.

## 6. What could go wrong (adversarial review seeds)

* **Torn multi-block writes become visible interleaved.** Already true today
  between *different* writers on POSIX and on this FS (per-block futures under
  per-block guards); Shared admission makes same-fd concurrent writes equally
  interleavable. POSIX does not promise write/write atomicity across
  concurrent writers; pjdfstests/LTP/fstests will adjudicate (battery gate).
* **Size-read race admits a write past a concurrent truncate.** Truncate takes
  the write guard: it cannot run concurrently with a Shared holder at all.
  A truncate *between* classification and acquisition re-checks: classify
  under the held guard (the acquisition IS the fence — classify-then-acquire
  re-validates size after the shared guard lands, route out if it moved).
* **A hidden writer-writer invariant not in §4.3's table.** The mitigation is
  the audit method itself: PR 2 lands each row as a red-first concurrency test
  *before* PR 3 flips any admission, and `invariant_tripwires` stays the
  runtime backstop.
* **Wake-convoy merely moves to the block locks.** Only same-block collisions
  queue there; rand-4k across 512 blocks/file makes collisions rare. The A/B
  gate (§8) measures it rather than argues it.

## 7. Observability

* `write_lock_scope_{shared,metaprep,entire}` — the decision ledger
  (counters, stats inode). A rand-overwrite row classifying below ~99 %
  `shared` = predicate rot; the engagement instrument for every future row.
* `write_lock_wait` stays the ceiling instrument (the histogram that convicted
  the convoy); expected post-change: the ms-class population collapses on
  Shared-dominant rows.
* `invariant_tripwires` — any §4.3 migration adds its tripwire rather than a
  `debug_assert!` (the `transport_lease_overlong` law).

## 8. Gates (all must pass; house rules apply verbatim)

1. **Red-first**: every §4.3 row lands as a failing concurrency test before
   its migration; the classifier's own contract tests pin all three classes
   plus the any-doubt-exclusive default.
2. **Loom** on any migrated protocol that gains a fence/atomic (expected: none
   new; the campaign reuses existing verified protocols — if one appears, it
   gets the weakening-verified treatment).
3. **Counted A-B-B-A on squeeze-test** (the aging-store rule): the four §2
   rows re-run both orders, plus the seq-1m throughput rows (38/39 GB/s must
   not regress), sustained-60 s discipline, engagement exact.
   Success bar: kernel rand-4k write ≥ 3× baseline (>1.2M at depth if the
   device-service model holds), il ≥ kernel (parity restored), `write_lock_wait`
   ms-population gone.
4. **write_matrix parity verdict** green (il ≥ kernel at minimum, ahead in the
   majority).
5. **The full battery regime** (this session's): `task check` from zero, then
   pjdfstests + LTP + fstests `-g auto` fail-fast from zero — a lock-order
   change re-earns the whole certificate.
6. Bench baseline (`tests/run_bench_baseline.sh`) pre-merge — this is a
   hot-path perf PR by definition.

## 9. PR plan

| PR | Content | Gate |
|---|---|---|
| 1 | `Shared` variant + classifier + decision-ledger counters + classifier contract tests (classification only — nothing takes the read guard yet; `Shared` maps to `MetaPrepOnly` behavior behind `SQUEEZEFS_WRITE_SHARED=0/1` default OFF) | full cargo gate |
| 2 | §4.3 audit rows as red-first tests + their (a)/(b)/(c) migrations | full cargo gate + loom where touched |
| 3 | Flip `Shared` to the read guard; default ON; the knob becomes the A/B lever (never an operational escape) | gates §8.3–8.6 |
| 4 | il parity residual (only if the §4.4 re-measure still fails parity) | write_matrix verdict |

## 10. Open questions for review

1. Should the fsync flush sweep *also* gain a shared-mode fast path for its
   read-only census pass, or stay fully exclusive? (Proposed: stay exclusive —
   flush is not the hot path and its exclusivity is a §4.3 safety anchor.)
2. `Shared` for the **il ring-write handoff** path: same classifier at the
   sink's handler dispatch, or classify once in the handler only? (Proposed:
   handler-only — one classification point, the sink never guesses.)
3. Does the mmap writeback path (`mmap_writeback_staleness_tests` surface)
   carry any writer-writer assumption not in §4.3? Flagged for the audit.
