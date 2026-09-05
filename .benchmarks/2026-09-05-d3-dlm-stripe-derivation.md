# D-3 — DLM stripe derivation (`perf/dlm-stripe-derivation`, e2e perf audit ladder row 9 / DLM board #4)

**Status: LANDED — mechanism + in-process rows + the mdstorm field row (§7),
A-B-B-A in both orders: collisions −76 %, 4a wait −58 %/op, throughput par.**

## 1. The finding

The 4a `DlmLockManager` guard (`I{ino}` / `D{parent,name}`) is held across the
whole commit park by design (M7 D5: a queue entry co-owns its tx's
`DlmGuard`s until the terminal outcome), so a false-sharing collision on a 4a
stripe — two distinct objects hashing onto one lock — costs the colliding op
a FULL commit. The board's ledger said "4096 stripes" and then named the
DLM's OWN 1024-way tables (`src/dlm.rs` `LAST_GRANT_FLOOR` / `LOCK_WAITERS`)
as the suspects, with the 4096-way tables being the 3.5 `INODE_META_LOCKS`
and `BLOCK_FLUSH_LOCKS`. No table had a histogram that could tell a
false-sharing wait from a same-key one, so the first deliverable was the
instrument.

## 2. The instrument — the stripe-collision census

Every striped lock table now has ONE acquisition door that classifies its
CONTENDED acquires (the try-acquire refused) against the stripe's
last-acquirer key word (the RW1 H2b audit's shape, always-on — one relaxed
store per acquisition, one load + one counter bump on the contended arm
only):

| table (stats-inode prefix) | order | width | hold class | `*_stripe_collisions` = | `*_key_waits` = |
|---|---|---|---|---|---|
| `dlm_inode` | 4a I-class | derived (shipped 4096) | the whole commit park | a stripe-mate's guard | the same ino |
| `dlm_dentry` | 4a D-class | derived (shipped 4096) | the whole commit park | a stripe-mate's guard | the same `(parent, name)` |
| `serve_ino` | S9 owner serve stripe (`meta_ship/publish.rs`) | derived (shipped 1024) | a served publish's read→compose→commit | a stripe-mate's serve | the same ino |
| `inode_meta` | 3.5 `INODE_META_LOCKS` | 4096 (unchanged) | one RAM-only read→merge→save | a stripe-mate's merge | the same ino |
| `block_flush` | 3 `BLOCK_FLUSH_LOCKS` | 4096 (unchanged) | one block's checkout / flush | a stripe-mate's block | the same `(ino, block)` |
| `lease_waiter` | `dlm::LOCK_WAITERS` (notify fan-out) | derived (shipped 1024) | **none** — the custody table is per key | a stripe-mate's release woke this waiter (a SPURIOUS WAKE) | its own key's release |

Plus `<table>_stripes` (the width in force), `lease_waiters_parked` (live
parked lease asks), and the 4a WAIT phase the audit-D family lacked:
`lock_phase_ns.dlm_guard_wait` — one sample per CONTENDED 4a acquire, so
**`dlm_guard_wait.count ≡ Σ dlm_{inode,dentry}_{stripe_collisions,key_waits}`**
(pinned; the closure law is what makes the census a decomposition of the
wait histogram rather than a second opinion).

Contracts: `tests/stripe_census_tests.rs` — per table, two keys that hash to
one stripe under a holder ⇒ exactly `(1 collision, 0 key waits)`; the same
key twice ⇒ `(0, 1)`; a foreign stripe ⇒ `(0, 0)` and no wait sample;
`lock_many` dedupes BEFORE the census (an in-group stripe collision is one
acquire, never a self-collision); the lease waiter's spurious wake vs own-key
wake; the flat export of all 18 words. Counted: `stripe_census_tests` +
`dlm_stripe_storm_tests` (gate config, `--test-threads=1`) 20/20 green from
zero after the final edit (the storm's many-dirs unlink bound was tightened
once mid-count on a 3-vs-8 tiny-sample row and the count restarted — the
multi-run discipline). The census contracts are latch-driven (no sleep
synchronization); the storm's shape contracts are bounds, not equalities,
because the census under-reports by construction (below).

Classification is diagnostic-grade by construction (the same posture the RW1
audit word took): "last acquirer" is stamped AFTER the grant, so a
first-touch stripe can read 0 while its holder is between grant and stamp —
a bounded `spin_loop` (256 iterations, sub-µs) absorbs that; a word still 0
classifies as a key wait, and a wait caused by a QUEUED stripe-mate (FIFO
courtesy refusing a fresh acquire) reads the previous holder — which may be
the asker itself. Both misclassify in the SAME direction: the census
UNDER-reports collisions, never over-reports them. The many-dirs rows below
carry that residue as a few key waits per 10⁴ ops.

## 3. Conviction — the in-process metadata storm

`tests/dlm_stripe_storm_tests.rs` (`storm_census_convicts_the_colliding_table`):
the mdstorm shape over the routed backend directly — T concurrent
committers, create → rename → unlink, the ONE-DIR interleave (writer w takes
names `w, w+T, …` in one parent — the parent-lock crucible) and the MANY-DIRS
mfcreate shape (per-writer parents). One file-backed v3 volume (256 KiB
nodes, 32 MiB ring) in `/tmp`. `cargo test --release`, this box: 32 kernel
possible CPUs (26 in the affinity mask), **load average 17–38 throughout —
shared with four sibling campaigns, so `ops/s` is LABEL-ONLY; the census and
the wait sums are exact counts and stand**. Width lever: `SQUEEZEFS_DLM_STRIPES`
(read once per process; one process per width).

### 3.1 Which table collides (32 writers × 512 ops, 16,384 ops per phase)

Collisions / key waits **per op**, by table. `inode_meta`, `block_flush`,
`serve_ino`, `lease_waiter` = 0.0000 / 0.0000 on every row (pinned: a
metadata storm touches no block stripe, arms no publish plane, takes no
lease) — the board's 1024-way suspects are ACQUITTED on this shape; the
4a `DlmLockManager` tables are the ones that collide.

| phase | W = 1024 | W = 4096 (shipped) | W = 16,384 (derived here) |
|---|---|---|---|
| manydirs/create — 4a D | 0.0189 / 0 | 0.0048 / 0 | 0.0011 / 0 |
| manydirs/rename — 4a D | **0.1110** / 0 | **0.0271** / 0 | **0.0071** / 0 |
| manydirs/unlink — 4a I | 0.0623 / 0.0073 | 0.0182 / 0.0019 | 0.0037 / 0.0004 |
| manydirs/unlink — 4a D | 0.0201 / 0.0031 | 0.0049 / 0.0006 | 0.0010 / 0.0000 |
| onedir/create — 4a D | 0.0202 / 0 | 0.0045 / 0 | 0.0011 / 0 |
| onedir/rename — 4a I | 0 / **0.9999** | 0 / **0.9999** | 0 / **0.9999** |
| onedir/unlink — 4a I | 0.0236 / 0.0336 | 0.0076 / 0.0161 | 0.0013 / 0.0016 |

Reading: **collisions scale as 1/W exactly** (rename D: 0.111 → 0.027 →
0.0071 for 1024 → 4096 → 16,384; each rename holds two D-stripes across its
commit, 32 in flight ⇒ ≈ 64 held stripes ⇒ 64/W per acquire × 2 acquires —
the arithmetic matches to the second digit). At the SHIPPED width **2.7 %
of many-dirs renames and 1.8 % of many-dirs unlinks pay a whole-commit
wait for a different object's commit**. The one-dir rename row is the
census doing its other job: 0.9999 KEY waits per op and ZERO collisions —
every same-dir rename takes `I{parent}` EXCLUSIVE, that is the workload's
own serialization (design §3.8; the parent-lock crucible the mdstorm rig was
built around), and no width will move it. The instrument does not misfile
it as the table's.

### 3.2 In-flight scaling (64 writers × 256 ops, 16,384 ops per phase)

| phase | W = 4096 | W = 16,384 |
|---|---|---|
| manydirs/rename — 4a D | **0.0598** / 0 | **0.0156** / 0 |
| manydirs/unlink — 4a I | 0.0354 / 0.0042 | 0.0090 / 0.0009 |
| manydirs/unlink — 4a D | 0.0124 / 0.0011 | 0.0019 / 0.0002 |
| `dlm_guard_wait` Σ, manydirs/rename | 1,273 ms over 0.638 s × 64 writers = **3.1 % of writer time** | 330 ms over 0.632 s × 64 = **0.8 %** |
| `dlm_guard_wait` Σ, manydirs/unlink | 1,300 ms = 3.8 % | 300 ms = 0.9 % |

Doubling the writers doubles the collision rate (0.027 → 0.060 at 4096):
the false-sharing probability per acquire is `held ÷ W`, i.e. the table's
load factor — which is exactly why the width must follow the in-flight
population the transport can present.

### 3.3 The 4a hold histograms (32 writers)

| row | `dlm_guard_wait` n / Σ / p50 / p99 | `dlm_guard_hold` n / p50 / p99 |
|---|---|---|
| manydirs/rename W=1024 | 1,818 / 1,439 ms / ≤ 1 ms / ≤ 4 ms | 16,384 / ≤ 2 ms / ≤ 4 ms |
| manydirs/rename W=4096 | 444 / 245 ms / ≤ 1 ms / ≤ 2 ms | 16,384 / ≤ 1 ms / ≤ 4 ms |
| manydirs/rename W=16,384 | 116 / 66 ms / ≤ 1 ms / ≤ 2 ms | 16,384 / ≤ 2 ms / ≤ 4 ms |
| onedir/rename W=4096 | 16,383 / 30,540 ms / ≤ 2 ms / ≤ 4 ms | 16,384 / ≤ 64 µs / ≤ 256 µs |

The hold (acquire → drop at the terminal outcome) IS the commit latency on
this shape — p50 1–2 ms in-process under load — and every collision waits
one of those. The one-dir rename row's hold is short (≤ 64 µs p50) because
the conveyor groups the 32 serialized renames' commits; its 30.5 s of
waiting is the 32-deep queue on `I{1}`.

### 3.4 ops/s (label-only on this box)

32 writers, many-dirs rename: 21.7 k (1024) / 30.5 k (4096) / 28.4 k
(16,384); 64 writers: 25.7 k (4096) / 25.9 k (16,384). The 3.1 % → 0.8 %
writer-time delta is inside this box's noise (load 17–38, sibling builds);
the wall-clock verdict is the field row's.

## 4. The 4a hold scope — what could be narrowed, and what cannot

Audited every 4a acquisition site (14 in `kv/backend.rs`, the routed
`create`/`unlink`/`rename`/`link` in `meta_backend/mod.rs`, `crossvol_tx.rs`,
`fsck.rs`): each is acquire → RAM reads (a node-cache miss under the guard
is the read-your-own-writes tx's own cost) → stage → `hold_guards` →
commit. The slot gate parks BEFORE any 4a acquisition (§5.5.2a), no 4a guard
spans a data-plane DMA, and the routed regular-file `create`/`unlink`
already take the parent SHARED (§3.8).

**Cannot be narrowed: releasing at the stage-A apply instead of the
stage-B terminal outcome.** The D-2 durability lane's failed-write arm is a
seq-conditional ROLLBACK, not a fail-stop (the volume latches `failed` only
after `JOURNAL_FAILURE_LATCH = 3` consecutive failures); the module doc's
"every other same-key writer is still excluded by the guards the failed
window's entries hold" is exactly what keeps a follower from building its
tx on an applied-then-rolled-back RAM state. Releasing earlier needs either
fail-stop-on-any-failed-write (a policy change with its own board) or a
follower revalidation protocol. Left as designed.

**Narrowed: the hold no longer extends past the terminal outcome into the
ack.** `fan_out` sent each member's oneshot and dropped the entry (with its
guards) AFTER — so a committer answered while its guards were still held
could re-ask for the same key (its next op on the same parent) and park on
its OWN previous tx for the send→drop gap. The census caught it before the
fix: many-dirs rename, where NO two committers share a key, recorded 4a I
KEY waits of **0.0055 / 0.1091 / 0.0251 per op** at W = 1024 / 4096 /
16,384 (p50 ≤ 16 µs — the gap's length). The guards now drop BEFORE the
send; post-fix the same rows read **0.0000 / 0.0000 / 0.0000**, and a solo
sequential committer (2,048 mkdirs under one parent, `I{1}` exclusive every
time) records zero 4a waits and exactly one hold per op — pinned
(`a_solo_sequential_committer_never_waits_on_its_own_previous_tx`).

Named residual (not this campaign's): `mkdir`/`rmdir` take the parent
EXCLUSIVE for the nlink RMW — a Δnlink merge record (the Δtime precedent)
would let directory creates take the parent SHARED like regular creates.
The one-dir mkdir storm is that row.

## 5. The derivation law

`stripe_locks::derived_stripe_width(shipped, possible_cpus, q_depth)` =
**`next_power_of_two(max(shipped, possible_cpus × q_depth × 16))`**, where

* `possible_cpus × q_depth` is the concurrency the FUSE-over-io_uring
  transport can present — one queue per kernel possible CPU, `q_depth`
  entries each (the explicit `SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH` clamped
  as the transport clamps it, else the desired 32 — the L1 policy's ceiling;
  the mount-time degraded depth is unknowable at volume open, and the
  ceiling is the conservative direction). Fleet-share EXEMPT like the op
  registry: each co-located daemon faces its own full ring;
* `× 16` is the load-factor target (`STRIPE_LOAD_FACTOR_INV`): α =
  in-flight ÷ stripes IS the false-sharing probability per acquire (§3
  measured it), and a false-sharing wait on the 4a tables costs a whole
  commit, so the table is sized for ≤ 1 acquire in 16 at FULL ring
  occupancy; each stripe is one ~80 B `Arc<RwLock>`;
* `shipped` (4096 for the 4a classes, 1024 for the waiter/grant-floor pair
  and the serve stripe) is the never-regress floor — the `Q_DEPTH_FLOOR`
  house law.

| shape | 4a width | waiter/serve width | memory (4a, both classes, per volume) |
|---|---|---|---|
| floor box 2 CPUs × depth 4 | 4096 (floor) | 1024 (floor) | 640 KiB |
| field 32 × 32 (this box) | **16,384** | 16,384 | 2.6 MiB |
| field at the depth floor 32 × 4 | 4096 (floor) | 2048 | 640 KiB |
| 192 CPUs × 32 | 131,072 | 131,072 | 21 MiB |

Explicit override `SQUEEZEFS_DLM_STRIPES` (registry: int `[1, 2^24]`;
explicit wins verbatim, a non-power-of-two rounds UP — the index is a
mask; `4096` = the shipped-4a control, `1` = the everything-serializes
crucible). Tie tests: `tests/derivation_sweep_tests.rs`
(`dlm_stripe_width_derives_from_possible_cpus_times_q_depth` over cpus ∈
{1,2,4,8,16,32,64,128,192} × depth ∈ {4,8,16,32}: power of two, ≥ shipped,
≥ 16·cpus·depth, the NEXT power of two, monotone in both;
`dlm_stripes_knob_is_explicit_over_derived_and_rounds_up_to_pow2` incl. the
live-table equality). `src/stripe_locks.rs` joined the pinned
`possible_cpus` consumer census as a kernel-mandated-geometry shadow.

`StripeLocks<L, const N>` became runtime-sized `StripeLocks<L>` (`new(width)`,
power-of-two asserted at construction; `hash & mask` — the modulo index of
every shipped width bit-for-bit, pinned) so the 4a / serve / waiter tables
can be built at the derived width; the 1/2/3/3.5 tables keep their shipped
4096 (the census showed 0 on them here — they hold no commit park; they are
the same one-line change if a data-path row ever convicts them). The dead
`StripeLocks::remove` no-op and its two FORGET-path callers were deleted.

## 6. What this does NOT claim

* No wall-clock win is claimed from §3.4 — the box was shared and loaded.
* The lever's payoff is proportional to in-flight × collision cost: the
  8-thread field mdstorm has ≈ 16 stripes held at a time (0.4 % of renames
  at 4096 → 0.1 % at 16,384), so its wall-clock delta will be small; the
  shape this widens for is the fan-out authority (D-1b/D-1c: 24 co-writers
  × 24 streams = 576 publishes in flight on the 4a I-class ⇒ ≈ 14 % of
  acquires false-share at 4096, 3.5 % at 16,384; and the 1024-way serve
  stripe at 576 in flight ⇒ ≈ 56 % → 3.5 % at 16,384). Those rows are the
  fleet rig's.

## 7. Field row — mdstorm A-B-B-A, MEASURED 2026-09-05 (dev box, root)

`sudo tests/run_mdstorm.sh leg` ×4, same binary (`d551f1ba`, release —
the five-campaign stack), `SQZ_MDSTORM_THREADS=64`, `/dev/shm` substrate
(fresh format per leg), **A = `SQUEEZEFS_DLM_STRIPES=4096` (the shipped 4a
width) / B = derived (16,384 at 32 possible CPUs × q_depth 32)**, order A B
B A. Load 8 at A1 (cold start), 30–37 for the other three — A1 is the cold
leg on every phase and is read as such. Analyzer
`.benchmarks/rigs/2026-09-05-d3-mdstorm-analyze.py`; artifacts
`.benchmarks/rows-d3-mdstorm-20260905/`.

| leg | width | rename/s | unlink/s | create/s | manydirs/s | 4a collisions/op | 4a key waits/op | `dlm_guard_wait` µs/op | Σ 4a wait |
|---|---|---|---|---|---|---|---|---|---|
| A1 | 4,096 | 5,440 | 6,149 | 6,995 | 10,508 | **0.01987** | 0.01672 | **67.5** | 36.4 s |
| B1 | 16,384 | 6,355 | 7,138 | 8,146 | 10,903 | **0.00477** | 0.01466 | **28.8** | 15.6 s |
| B2 | 16,384 | 6,243 | 7,200 | 8,210 | 11,212 | **0.00483** | 0.01489 | **27.9** | 15.0 s |
| A2 | 4,096 | 6,221 | 7,254 | 8,375 | 10,777 | **0.02041** | 0.01682 | **67.8** | 36.6 s |

`serve_ino`, `inode_meta`, `block_flush`, `lease_waiter` collisions: 0 on
every leg (the storm is a metadata row — the board's suspects stay
acquitted here). 540,000 ops per leg; every leg's row was clean.

**Verdict — LANDS (derivation law).** The mechanism removes exactly what
it claims, in both orders: 4a false-sharing collisions **−76 %** (the 4×
width ratio, to the third digit), the 4a guard wait **−58 %** per op
(67 → 28 µs), key waits −12 % (the fan-out ack-after-drop fix). Throughput
is **par**: against the clean A2 leg the B legs are within ±2 % on every
phase (rename +2 %/+0.4 %, unlink −1.6 %/−0.7 %, create −2.7 %/−2.0 %,
manydirs +1.2 %/+4 %) — the 4a wait is < 1 % of this venue's ~10 ms/op
storm budget, so its removal cannot show in ops/s here; the note's
in-process rows already said so. What lands is the derivation (a free
4096 replaced by `next_pow2(max(shipped, 16 × possible_cpus × q_depth))`,
2.6 MiB per volume at the field's 32 × 32), the always-on census, and the
fan-out fix; `SQUEEZEFS_DLM_STRIPES` stays the explicit override / A-B
lever. The fan-out rig row (24 co-writers × 24 streams — where the
in-process census predicts 4a ≈ 14 % and the 1024-way serve stripe ≈ 56 %
collisions at shipped widths) stays owed to a fleet session; the storm
row is the campaign's acceptance.
