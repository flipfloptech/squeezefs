# 2026-09-06 — the reader ack ladder's windows, RE-DERIVED (finding 15 term 1, adjudication items 1–3)

| | |
|---|---|
| **Branch** | `perf/free-grace-ladder-rederivation` off `dev` `1b41697e` |
| **Commits** | `74cdd880` (red) · `31ec8e45` — item 1 · `9f4c4dc0` (red) · `92ee9094` — item 2 · `4fdc15ae` (red) · `324677ba` — item 3 · the docs commit |
| **Decision** | USER, 2026-09-06: re-derive the ladder's two windows (the four items of `.benchmarks/2026-09-06-free-grace-hold-time.md` §7 — items 1–3 here, item 4 the sibling campaign's). Supersedes the 2026-08-25 pinning of the windows' derivations (KD-FG-11, now amended in `docs/design-free-grace-sustain.md`); the windows stay pinned against DEMAND |
| **Design** | `docs/design-free-grace-sustain.md` §"The remaining 6 s" (the landed table) + KD-FG-11 (amended); the ladder's proof is `src/free_grace.rs` (`ReaderAckLadder` rustdoc, gates 2 and 3) and `src/ro_coherence.rs` (the generation, the layout-cache gate, the serve ledger's ordering block) |
| **Program input** | `.benchmarks/2026-09-06-free-grace-hold-time.md` §3/§6/§7: 6.0 of the 8.6 s in-process hold (8.0 s on the fleet) are the two derived windows; 4 GiB lanes at ≈ 31 % headroom by the capacity law; s11-mpiio FAILS |
| **Instrument** | `tests/reader_free_grace_tests.rs` contracts 32–38 (the fleet-cadence closed loop, the D-4/hold-time harness — every decision product code, the windows now computed through the product's own rules), `tests/reader_layout_step_gate_tests.rs` (the cache gate on a real router + KV backend), `tests/derivation_sweep_tests.rs` (the ceiling's tie test). Release build, deterministic (bit-identical across runs and against the debug profile) |
| **Evidence tier (rc-manifest)** | every number below is *measured-real in-process* — one process, product code deciding every step. **The fleet row is OWED (parent)** |
| **Class** | coherence-proof change (not tuning): each window's DERIVATION moved from a poll interval standing in for the writer's ceiling, a cache TTL and a lease-clock reserve to the writer's own landing ceiling, an epoch-step invalidation and an observed in-flight count. Safety argued per term below; the pre-change shape is restorable per term (three bool levers, default on) |

## 1. What each window WAS, why it was slack, what it IS

The reader's ladder (`ReaderAckLadder`, spec §6.8 item 3) emits an
acknowledgement for label `L` — "I have FINISHED with everything freed at
or before `L`" — only after (1) an epoch-step purge ran, (2) in a pass
that BEGAN late enough for the writer's dereference to be in the record it
adopted (qualify), and (3) nothing on the reader can still serve a
pre-step binding or be mid-serve on one (drain). On the fleet the two
windows were **6,022 ms** of an ≈ 8 s hold.

| Gate | Was (fleet) | Why it was slack | Is | Lever (`0` = the retired term verbatim) |
|---|---|---|---|---|
| (2) qualify | `S + skew` = `P + CHECKPOINT_MAX_AGE_MS + skew` = **2,022 ms** | the argument rests on the WRITER checkpointing the dereference within its ceiling; `S` added the READER's poll interval `P` on top — "how stale can a reader be between polls" — but for qualification the pass itself IS the poll: its ledger read is the observation the window exists to place after the landing. `P` bounded nothing here | **the writer's checkpoint LANDING ceiling + skew** — `checkpoint_landing_ceiling_ms = CHECKPOINT_MAX_AGE_MS + 2 × tick period` (the cadence trigger is evaluated once per checkpoint-task tick, behind a maintenance drain bounded by one period — finding 49; **1,100 ms** on the shipped 50 ms flush) — through the ONE accessor `MemberSession::checkpoint_ceiling_ms` (item 4's writer-advertised, demand-elastic value lands there). `qualify_lag_ms` = **1,122 ms** | `SQUEEZEFS_FREE_GRACE_QUALIFY_CEILING` |
| (3a) drain, the `S` term | `S` = **2,000 ms** | a TTL: the daemon layout cache (`metadata_cache`, 1 s freshness horizon) and the attr/dentry caches (reader TTL = `S`) could serve a PRE-STEP block binding after the R-6 purge dropped the block-key census — so the ladder waited the TTL out | **an epoch-step invalidation**: every `CachedMetadata` carries `reader_step_gen`, the reader's purge generation read BEFORE its backend read; the purge sink's LAST act bumps the generation; every binding-serving read refuses an older stamp (`layout_entry_pre_step` — the handler resolve, the post-fetch binding rechecks, the il sync probe, the direct-drive prelude, the R2/read-lane fill tasks' map snapshots). No TTL to wait: the term is **0** | `SQUEEZEFS_FREE_GRACE_DRAIN_EPOCH_STAMP` |
| (3b) drain, the `D_purge` term | `D_purge` = `2 × P` = **2,000 ms** | §6.7's lease-clock fail-stop reserve ("observe, then finish", sized for the member's self-fence) reused as "serves in flight when the purge ran must finish" — milliseconds of work waited out for two poll intervals; and no timer bounds a serve a fabric timeout can stretch, so a measured p99.9 would have been no bound either | **an OBSERVED drain**: every read serve counts itself in the purge generation it started under (`ServeStamp`), and a candidate qualified after the step that left generation `G` promotes once every slot below `G` reads zero (`serve_drained_below`) — at the qualifying pass when nothing is in flight, else on the completion wake of the last such serve. `D_purge` survives ONLY as the must-stay-0 tripwire `free_grace_drain_overdue`; it never shortens the wait. The term is the serve residence (**ms**) | `SQUEEZEFS_FREE_GRACE_DRAIN_OBSERVED` |

**6,022 ms → 1,122 ms + the serve residence.** The windows stay pinned
against DEMAND (KD-FG-11 as amended): no pass cadence, prod or pressure
signal may shorten them; the derivations moved, the terms did not become
tunable.

## 2. The safety argument, per term

**Qualify (item 1).** The dereference commit precedes the free (the reclaim
queue sits between), and the free's label is the owner's instant at the
free, so the dereference was ACKED before the label. `checkpoint.rs::tick`
evaluates the `CHECKPOINT_MAX_AGE_MS` trigger once per tick (period = the
flush interval, strict mode 100 ms — `checkpoint_tick_period_ms`, now the
ONE function the task and the reader's poll cadence both ride), behind a
maintenance drain bounded by one period; so a commit's checkpoint is
DECIDED within `trigger + 2 × period` of its ack, and a ledger record
containing the dereference exists by owner instant `label + ceiling`. A
pass whose ledger READ begins after that adopts it or a newer one (gate 1
then requires the pass to have advanced). The member measures from the
instant it LEARNED the label — its send anchor — and the label was minted
no later than one trip after that anchor; `skew_max ≥ the observed RTT`
covers the trip plus the clocks' rate drift, so a member-clock pass start
of `learned + ceiling + skew` is an owner-clock instant ≥ `label +
ceiling`. A learn instant LATER than the label's (a slow beat) only pushes
the pass later — conservative, never unsafe. **What stays unstated is
device time** — the cycle's own writes and the flush barrier — the same
residue the published S5 staleness bound (`interval + 1 s`) carries; the
writer knows that term (`free_grace_checkpoint_cycle_ms`) and item 4's
writer-advertised ceiling is where it belongs. The honest landing ceiling
is 100 ms longer than the adjudicated `CHECKPOINT_MAX_AGE_MS + skew`
(1,022): the tick terms are derivable and were kept. On a slow-flush venue
(5 s) the tick IS the landing term (11,000 ms) — which the retired
`P + 1 s` window never covered.

**Drain, `S` (item 2).** The hazard is a serve resolving `b → K` from a
map the writer has superseded and reading `K` from the device after the
writer reused it. The gate makes a pre-step resolution structurally
impossible after the step: the stamp is read BEFORE the backend read
(`Acquire`), the bump is `SeqCst` and sequenced after the volume's root
adoption, so a fetch whose capture read the bumped value synchronizes with
the bump and its KV read sees the adopted roots (stamped new, content
new), while a fetch that read the old value is stamped old and misses
after the bump regardless of what its read saw — a pre-step binding never
carries a post-step stamp. The attr/dentry caches keep their TTL: they map
names to inos (never reused) and inos to sizes, never a block to an
offset, and every binding they lead to resolves through the gated layout
cache. Background fills (R2 prefetch, read-lane ahead) that deposit bytes
keyed by `K` are safe on the same argument plus the purge: a NEW binding
to `K` is visible only after a later step, which purges `K`; and their
task-start gate refuses a pre-step map snapshot. A save's republish stamps
the CURRENT generation — the persisted state is at least as new as the
step, and the next step makes it a miss like everything else.

**Drain, `D_purge` (item 3).** "Every slot below `G` reads zero" is a
proof, in three parts (`src/ro_coherence.rs`, the serve-ledger block):
(i) pairs land on ONE word — the stamp increments
`shards[its thread's shard][gen % 64]` and its drop decrements the SAME
word (the stamp carries its shard), so every word is a non-negative count
in its own modification order and a decrement is never observed ahead of
its increment; a sum of zero means every observed start has completed.
(ii) A start the ladder did not observe is post-step (Dekker): the step's
bump is a `SeqCst` RMW on the revalidation task, sequenced before the
ladder's `SeqCst` slot loads on that task; a stamp is a `SeqCst` increment
followed by a `SeqCst` re-read of the generation; in the single total
order, if the ladder's load precedes the increment then the bump precedes
the re-read, so the re-read sees the bump — and a load that reads the bump
synchronizes with it, hence with the root adoption sequenced before it:
everything that serve resolves afterwards sees the adopted roots and the
current generation (the layout gate's relaxed load is coherent after it).
Either the ladder counts the serve or the serve is post-step.
(iii) Slot aliasing — a serve alive across 64 steps (≥ 64 s at one step
per second per volume) — is detected at the re-stamp, POISONS the slot
(counted pre-step for every candidate until it reads zero: over-
conservative, a saturated reader may then wait for the whole slot; never
unsafe) and trips `reader_serve_slot_overruns`. The wake is lost-free by
the same discipline: the ladder stores its wait generation `SeqCst` before
its loads, so the last pre-step completion in the total order reads zero
and wakes; the ticked park is the backstop regardless. **An observed drain
is strictly safer than any timer** — a timer bounds nothing a fabric
timeout can stretch, the count is the serves themselves — and it
completes in the serve residence instead of two poll intervals. A pre-step
serve outliving the `D_purge` budget is a wedged I/O: counted once per
candidate (`free_grace_drain_overdue` + `invariant_tripwires`), reported,
and WAITED ON.

**The whole:** a reader never resolves a reallocated block through a
stale binding (item 2: no pre-step binding is servable after the step;
item 1: the step's record carries the dereference) or serves stale bytes
for one (item 3: every serve that could have resolved pre-step has
completed before the ack; the block-key purge at the step handles cached
bytes, as before).

## 3. The in-process rows (release, the fleet-cadence shape, deterministic)

8 members, checkpoint 1 s, pass 1 s, prodded beat 1 s (the floor), lane-0
stream 80 MiB/s against a 500 blk/s storm, spare 256 blocks, 300 s owner
clock, phases staggered — the hold-time note's shape and harness, the
windows now computed through the product's rules. Every row: closure
`deferrals ≡ releases + held` OK, `forced = fences = 0`,
`free_grace_drain_overdue` 0, `hold_unplaced` 0, stalls 0, the stream at
its offered 80 MiB/s.

| Config | `bound_age` mean / max | residence | `defer→ckpt` | `ckpt→min_acked` | `min_acked→rel` | `hold_ms` | ack lag max / mean |
|---|---|---|---|---|---|---|---|
| A3 (2026-09-05 binary) | 8,646 / 8,750 | 8,396 | 498 | 7,888 | 9 | 8,010 | 7,750 / 7,375 |
| **H3** (hold-time shipped — the before) | **7,724** / 7,750 | 7,841 | 498 | 7,339 | 3 | 7,695 | 7,500 / 7,000 |
| R1 = H3 + item 1 (qualify ceiling) | 6,724 / 6,750 | 6,840 | 498 | 6,338 | 3 | 6,695 | 6,500 / 6,000 |
| R2 = R1 + item 2 (epoch stamp) | 4,724 / 4,750 | 4,839 | 498 | 4,337 | 3 | 4,695 | 4,500 / 4,000 |
| **R3 = R2 + item 3 (observed drain) — SHIPPED** | **2,724** / 2,750 | **2,838** | 498 | **2,336** | 3 | 2,695 | 2,500 / 2,000 |

**−5,000 ms on `bound_age` (−65 %), −5,003 ms on the residence.** Item 1
cuts 1,000 (the 900 ms window cut rounds to one pass on the 1 s grid),
item 2 cuts 2,000 (`S`), item 3 cuts 2,000 (`D_purge`). What remains of the
2.7 s: the qualify window rounded to the pass grid (≈ 1.5–2.0 s after
learn), ≈ 0.5 s label learn, the carry and the refresh, the min over 8
members — all cadence terms; the sibling's item 4 (the writer→member `P/2`
composite) is the one that moves those.

**What the model does NOT contain:** serves. Its drain is the step itself,
so R3's drain term reads 0; on a mount the term is the serve residence
(`read_serve_phase_ns.total`, milliseconds) plus one wake — the promotion
lands at the qualifying pass when the pre-step serves have completed by
the pass's end, else on the straggler's completion wake (contract 37 pins
both). The prediction the task named, ≈ 3–3.5 s, assumed a next-pass
quantization of the drain; the wake removes it.

Through the capacity law (`alloc_lane_share_needed_blocks = ceil(churn ×
hold) + live`): 4 GiB lanes, 1.25 GiB live, 200 MiB/s of displacement — at
the 7.7 s hold 385 + 320 = 705 of 1,024 blocks (31 % headroom, the fleet's
edge); at 2.7 s, **135 + 320 = 455 (56 %)**. The fleet row that would
confirm it is the parent's.

## 4. Per-op hot-path cost

| Site | Added | Notes |
|---|---|---|
| every layout-cache read that serves a binding (`metadata_entry_fresh_or_dirty`, `current_block_binding`, `block_binding_is`, the il probe, the dd prelude, the fill tasks) | one relaxed load + one compare | structurally false on a mount that never steps (the generation is 0 for life); no lock, no copy |
| every read serve (FUSE handler, R-2 fast probe, il `serve_read`, the dd snapshot to its CQE, prefetch/read-lane fill tasks, cfr source) | start: one relaxed armed load, one relaxed generation load, one thread-local shard index, one `SeqCst` RMW, one `SeqCst` load; end: one `SeqCst` RMW (same word), one relaxed load | on x86 a `lock xadd` is the same instruction at every ordering; the ARM fence is the price of the proof. **Unarmed (every plain writer): one relaxed load.** The dd completion RMWs the submitting thread's shard word (the same-word law) — one cross-core line per dd op, the class of the existing per-op counters |
| the epoch step (once per volume per pass) | one `SeqCst` RMW + one slot re-stamp + one 64-shard slot sum | cold |
| the ladder (once per pass, plus one early pass per straggler wake) | ≤ 64 slots × ≤ 64 shards `SeqCst` loads | cold |
| re-resolves | one backend layout read per cached ino per epoch step | against the 1 s horizon that already refetched every hot ino once a second: at most a doubling under a storming writer, ≈ nothing on a quiet one; `reader_layout_step_misses` is the ledger. On a co-writer with an armed ownership plane that read is a shipped `getxattr` — the same class it already paid at the horizon |

Memory: the serve ledger is `shards × 64 × 8 B` (32 KiB at 64 shards),
allocated on first use on an armed mount only.

## 5. Levers, gauges, contracts

| Lever (bool, default on) | `0` restores | Gauges |
|---|---|---|
| `SQUEEZEFS_FREE_GRACE_QUALIFY_CEILING` | qualify = `staleness_bound + skew` | `free_grace_qualify_lag_ms` (the window in force) |
| `SQUEEZEFS_FREE_GRACE_DRAIN_EPOCH_STAMP` | the caches serve to their TTL; drain keeps `S` | `free_grace_drain_lag_ms` (the timer terms still in force: 0 / `D_purge` / `S + D_purge`), `reader_layout_step_gen`, `reader_layout_step_misses` |
| `SQUEEZEFS_FREE_GRACE_DRAIN_OBSERVED` | the `D_purge` timer; the ledger unarmed | `free_grace_drain_observed` (promotions decided by observation ≈ `free_grace_reader_acks`), `free_grace_drain_overdue` (**must stay 0**), `free_grace_drain_wakes`, `reader_serves_inflight`, `reader_serve_step_races` (the re-read engaging — conservative), `reader_serve_slot_overruns` (**must stay 0**) |

All three are registry entries (`src/env_knobs.rs`, ENG-10) with
`docs/operations.md` rows; the gauges ride the reader's `free_grace_mode:
"reader"` stats block.

Contracts (`tests/reader_free_grace_tests.rs` 32–38, `tests/reader_layout_step_gate_tests.rs` 1–4,
`tests/derivation_sweep_tests.rs::free_grace_qualify_ceiling_derives_from_the_checkpoint_trigger_and_tick`):
the three derivations pinned (qualify carries no `P`; drain carries no
`S`; the drain completes when the pre-step in-flight count is 0 and NOT
before — not at `D_purge`, not far past it); the coherence contracts
unchanged and green (a pass beginning 1 ms before `learned + ceiling +
skew` never qualifies; a step with a pre-step serve in flight never acks;
a post-step serve never blocks; the tripwire fires once and the wait
continues; `forced_releases = laggard_fences = 0` on every loop row); each
lever's `0` pinned against the same session/loop; the cache gate on a
real router + KV backend (handler resolve, il probe, dd prelude, the
before-the-read stamp, the never-stepping writer); the in-process
before/after above. Every red commit fails to compile against its parent
(the new surfaces do not exist), the green commit passes.

## 6. Gate (this side)

* Suites, `--all-features -- --test-threads=1`: `reader_free_grace_tests`
  **55/55** (48 + 7), `reader_layout_step_gate_tests` **4/4** (new),
  `derivation_sweep_tests` 46/46 (+1), `env_knob_convention_tests` 21/21,
  `free_grace_lane_visible_tests` 9/9, `kv_node_cache_coherence_tests`
  21/21, `dlm_membership_tests` 51/51, `dlm_cowriter_tests` 18/18,
  `readonly_mount_tests` 28/28, `mw_cowriter_free_leak_tests` 9/9,
  `cowriter_enospc_wedge_tests` 9/9, `read_tier_refetch_churn_tests` 3/3,
  `read_fingerprint_tests` 13/13, `read_lane_tests` 18/18,
  `read_prefetch_pipeline_tests` 1/1, `ipc_op_economy_tests` 5/5,
  `audit_instruments_tests` 26/26, `no_tokio_convention_tests` 2/2.
* `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D
  warnings`: clean.
* The release-profile loop rows are bit-identical to the debug rows.
* No `task check`, no root/fleet rigs (the parent's).

## 7. What is NOT claimed

* **The fleet row.** Every number here is in-process; the s11-mpiio
  from-zero row on the finding-15 venue, the capacity-law reading on its
  lanes, and the fleet's `free_grace_hold_phase_ns` after this change are
  the parent's. The model's drain is 0 by construction (no serves); the
  fleet's is the serve residence — expected ms, measured by the parent.
* **Item 4** (the writer→member `P/2` composite: the checkpoint cadence,
  `ack_refresh_floor`, L2b's pass floor law) is the sibling's; the one
  accessor `MemberSession::checkpoint_ceiling_ms` is where its advertised
  value lands, and `checkpoint_landing_ceiling_ms` is the derivation it
  replaces or extends.
* **The device-time residue in the qualify ceiling** (the cycle's own
  writes, the flush barrier) is not derived on the reader; it is the same
  unstated term the published S5 staleness bound carries, and belongs to
  the writer-advertised ceiling.
* **A 64-step-stale generation load** is assumed impossible (a relaxed
  load on real hardware is coherent within microseconds; 64 steps are
  ≥ 64 s). Aliasing is otherwise detected and conservative.
* **The S5 metadata-staleness contract** (`reader_staleness_bound_ms`, the
  kernel/daemon reader TTLs) is untouched: this change re-derives the
  acknowledgement ladder's windows, not the staleness statement.
