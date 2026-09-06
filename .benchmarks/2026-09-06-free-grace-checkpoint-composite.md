# 2026-09-06 — the writer→member checkpoint composite (finding 15 term 1, adjudication item 4 — USER DECISION 2026-09-06)

| | |
|---|---|
| **Branch** | `perf/free-grace-checkpoint-composite` off `dev` `1b41697e` |
| **Commits** | `ac33a03c` (red: contracts 32–36 + the L2b law re-stated + the loop's cost ledger) · the implementation and docs commits listed in the parent's report |
| **Design** | `docs/design-free-grace-sustain.md` §"Hold-time campaign" → "The writer→member checkpoint composite"; KD-FG-11 amended |
| **Program input** | `.benchmarks/2026-09-06-free-grace-hold-time.md` §4.4 (lever (a) measured INERT alone, the composite it pays as) and §7 item 4; the user's 2026-09-06 approval of the composite knowing its cost (2× lease-lane load and 2× checkpoint cycles under demand, and L2b's pinned floor law's INPUT becoming demand-elastic) |
| **Instrument** | `tests/reader_free_grace_tests.rs` contracts 32–36 — the D-4 closed loop on the manual owner clock at the fleet cadences (8 members, lane-0 80 MiB/s against a 500 blk/s storm, spare 256, 300 s, staggered), whose writer now checkpoints on the product's own decision (`elapsed ≥ the ceiling in force`) and whose members pass on the product's L2b resolver; every row carries a cost ledger (checkpoints/s, renewals/s = the product's `membership_renewals`, elastic cycles, pass prods, ceiling min, worst staleness). Release build: `cargo test --release --all-features --test reader_free_grace_tests -- --nocapture --test-threads=1 the_checkpoint_composite_halves an_ask_in_force the_member_adopts the_checkpoint_task_runs a_prodded_grant`. No substrate, no wire, no wall clock in the loop (contract 36 alone runs a real `KvMetaBackend` on the wall clock); deterministic — the loop rows are bit-identical across runs |
| **Evidence tier (rc-manifest)** | every number below is *measured-real in-process* — one process, product code deciding every step. **The fleet row is OWED (parent)** |
| **Class** | perf — the release latency of the freed-offset grace loop (the sustained shared-file rewrite ceiling on a lane-partitioned fleet is `spare ÷ hold`) |

## 1. What the hold-time note left, and what the user decided

Contract 31 of the hold-time campaign ran the shipped H3 configuration with
the writer checkpointing at 1,000 / 500 / 1,650 ms and read `bound_age`
7,724 / **7,724** / 8,386: a faster writer checkpoint alone moves NOTHING
(only the shadowed `defer→checkpointed` stage shortens), because the
reader qualifies a label on a TIME bound — a pass beginning ≥
`learned + staleness + skew` — derived from the checkpoint CEILING
constant, never on observing the checkpoint. The note's §4.4 named where
it pays: as the **composite** — the writer's live ceiling under demand
(`P/2`, the Nyquist bound that puts a new root in every pass window)
CARRIED to the members as the prod floor AND L2b's pass floor, so passes
and beats run at `P/2` too, halving the learn, qualify-rounding and carry
terms (≈ −0.6 s predicted) — and filed it as adjudication item 4 because it
changes L2b's pinned floor law (KD-FG-11: "an ask below
`CHECKPOINT_MAX_AGE_MS` clamps up — physics") by making the law's INPUT
demand-elastic, at 2× the lease-lane load and 2× the checkpoint cycles.
The user approved it on 2026-09-06 knowing that cost.

## 2. The mechanism, in the order the number travels

### 2.1 The writer's demand-elastic ceiling

`src/free_grace.rs::checkpoint_ceiling_in_force_ms` →
`src/meta_backend/kv/checkpoint.rs::tick`. The KV checkpoint task decides
`elapsed ≥ CHECKPOINT_MAX_AGE_MS` once per flush-cadence tick (50 ms
shipped); while the freed-offset valve is **asking** — a prod cadence in
force, deposited by rung (a) off the space runway or rung a′ off the L4
demand mark into the one `PROD_RENEW_MS` word — it compares against the
elastic ceiling instead:

```
elastic = clamp(max(P/2, 2 × measured checkpoint cycle), 1 ms, routine writer ceiling)
routine writer ceiling = max(flush tick, CHECKPOINT_MAX_AGE_MS)       (= 1,000 on the shipped 50 ms flush)
P = the reader's ROUTINE poll interval (reader_revalidate_interval)   (= 1,000 here)
```

Every term derives (tie test `free_grace_elastic_checkpoint_ceiling_derives_from_the_poll_and_the_cycle`
in `derivation_sweep_tests`): half the reader's poll is the Nyquist bound
(a checkpoint every `P/2` puts at least one new root in every `P`-long
pass window however the two cadences phase); twice the cycle's own
measured cost is lever (d)'s law for the bound scan applied to the
checkpoint (a cycle may not run more than half the time); the routine
ceiling is the cap (never slower than routine — and `resolve_revalidate_interval_ms`
with no override is the SAME derivation the reader's poll uses, so the two
cannot drift). The task's tick tightens to the ceiling only where the
flush cadence is coarser than it (a slow-flush venue: the decision is per
tick, so a finer ceiling would be unreachable). The decision compares the
ceiling against the ELAPSED time since the last cycle, so a ceiling that
tightens mid-interval fires at once — which is what lets an advertised
ceiling be a promise about commits that PRECEDED the grant (§2.2). No ask
⇒ `None` ⇒ the shipped constant and tick, untouched; a plane-less mount
pays one `ArcSwap` load per tick.

**Why the prod, and not the L4 demand mark alone.** The task said "reuse
the existing L4 demand signal / valve pressure — do not invent a second
signal". The demand mark alone would leave the composite dark on the very
shape it exists for: site 0's age arm fires when the ring's front has aged
past the physics floor (`qualify + drain + 2 floors` = 8,022 ms on the
shipped derivation), and a healthy coupled loop never gets there — the
fleet-cadence shape holds 7.7 s (H3's `demand_waits` 3,766 are all in the
fill, before the first acks land), and the fleet row's demand came from
the ENOSPC refusal edges the campaign exists to remove. Rung (a)'s space
arm, by contrast, asks at the floor on every renewal of the H3 row
(`prods` 3,203 = `renewals` 3,203). Both arms deposit into one word, so
"a prod in force" is the ask the valve is making, whichever arm made it —
and the writer does its half of the ask exactly while the members are
asked for theirs.

### 2.2 Carriage — the grant, and the promise it makes

`membership::Grant::checkpoint_ceiling_ms` (u64), set in the ONE grant
constructor (`MembershipOwner::grant_for` →
`free_grace::advertise_checkpoint_ceiling`): the ceiling in force — the
routine one with no ask, `P/2` under one, `0` from a membership owner with
no grace plane (it has nothing honest to say about checkpoints; the member
falls back to the constant). KD-FG-11's "no wire field — the prod is the
signal" clause is retired: the prod stays the signal, the ceiling is its
number, and the option of deriving it losslessly from the prodded
`renew_ms` was rejected — the prod is `clamp(want, floor, routine)` and
carries the ceiling only when it sits AT the floor and the skew is below
it, which is a coincidence of the shipped clocks, not a law.

**The advertisement is a PROMISE**: "every commit before this grant is
checkpointed within `c` ms of it." A writer that relaxed its ceiling the
instant the ask lapsed would break it for the labels learned just before
(a label learned at `t₁` under 500 whose dereference the writer, back at
1,000, checkpoints at `t₁ + 1,000`). So an elastic advertisement records a
**promise pair** — the smallest ceiling advertised on a grant whose window
is open, and the instant the window closes: one routine ceiling past the
LAST elastic grant — and the task's in-force reading is the minimum of the
live derivation and the pair. The writer therefore relaxes no sooner than
every advertised window has closed, whatever the ask or the lever did
since (contract 33 pins the window to the ms: honoured at `t_g +
routine − 1`, closed at `t_g + routine`). Two relaxed atomics, no lock in
the renewal hot op (KD-FG-4 stands); the one race — two grants concurrently
opening a lapsed window — can keep the larger of two derivations taken µs
apart, which differ by at most the EWMA cycle floor's drift between them.

**Wire versioning.** The membership vocabulary (`membership_wire`) has no
schema of its own — its bincode bodies ride the transport's — so the field
bumps **`CLUSTER_WIRE_SCHEMA` 1 → 2**. A mixed fleet fails loud at the
cluster-wire handshake (KD-7's law), never a member running the constant
against a writer that advertised half of it (bincode is positional: a
peer speaking 1 would read a missing trailing field as EOF, an extra one
silently — the exact failure a schema exists to refuse). `lane_supply_blocks`,
added to the same grant on the same day without a bump, rides it too.

### 2.3 The member — three floors follow the advertised ceiling

`MemberSession::checkpoint_ceiling_ms(&self) -> u64` (the method the
sibling campaign adds too, returning the constant; this branch returns the
composite's live value — the parent reconciles): the ceiling advertised on
the grant that carried the label `learned_label` reports (the join, then
every ROUTINE renewal; a carriage renewal learns neither the label nor
the ceiling), falling back to `CHECKPOINT_MAX_AGE_MS` when none was
advertised or under `CHECKPOINT_COMPOSITE=0`. It is the input of:

1. **the owner's live prod floor** — `ProdParams::floor_for(ceiling) =
   max(min(P, ceiling), skew)`: a member's pass now runs at
   `clamp(ask, ceiling, P)`, so its answer can change every
   `min(P, ceiling)`, floored at the clock-skew bound like the routine
   floor. At the routine ceiling this IS the shipped `max(P, skew)` — the
   identity that makes "no ask ⇒ the shipped shape" structural (contract
   32). `note_pressure` (the ask), `refresh_bound_on_dirty` (lever (d)'s
   rate limit) and `release_on_ack` (the lane push's) read it through one
   `live_prod_floor_ms`. The first ask under a fresh storm is computed at
   the routine floor and puts the ask in force; the next harvest reads the
   halved one (contract 33: prod 1,000 → 500 across two harvests);
2. **L2b's pass floor** — `reader_pass_interval`: `clamp(ask, advertised
   ceiling, routine)`, the ceiling deposited beside the ask by
   `note_prodded_renewal(renew_ms, checkpoint_ceiling_ms, now)` from both
   `renewed` and `renewed_carriage` (the pass floor is about NOW, so the
   carriage's grant informs it even though it learns no label). The law
   re-stated: **an ask below the WRITER'S ADVERTISED ceiling clamps up** —
   the physics is unchanged, the input is live; nothing advertised = the
   constant verbatim (the pinned test's shipped arms are kept as-is and
   the composite arms added — a halved ceiling with a halved ask runs the
   pass at `P/2` on the floor venue, L2b's first engagement THERE);
3. **the ack pipeline's depth input** — `reader_refresh_floor_ms(P,
   ceiling, skew) = max(min(P, ceiling), skew)` (the constant term absent
   under the lever off, so a slow-flush venue's `P` above the constant
   keeps its shipped depth on the A/B control). **Measured in, not
   designed in**: the first cut left the depth at the routine floor and
   the composite read the hold **+800 ms WORSE** (`bound_age` 8,520 vs
   7,724 — labels arrived at the halved beat, the 8-slot queue saturated
   and displaced unqualified candidates; `acks` barely moved and lever
   (d)'s refreshes collapsed 1,431 → 263). With the depth following the
   floor the cap doubles and the acknowledgement rate doubles with it
   (§3).

### 2.4 The lever and the gauges

`SQUEEZEFS_FREE_GRACE_CHECKPOINT_COMPOSITE` (bool, default on; registry
entry + `docs/operations.md` row): `0` = the writer's constant/tick, the
routine ceiling on every grant, the constant as the member's floor — the
shipped shape exactly (KD-FG-7), read on BOTH sides. Gauges:
`free_grace_checkpoint_ceiling_ms` (the DECISION ceiling IN FORCE on the
authority — the `fence_bound_ms` precedent), `free_grace_advertised_ceiling_ms`
(on a member's `.stats`: the LANDING ceiling learned with its label — at
integration with the ladder re-derivation the grant advertises
`checkpoint_landing_ceiling_for_elastic(c)` = `c + 2 × min(tick, c)`, the
routine `trigger + 2 × tick` with no ask, so the member's qualify term and
pass floor read one number that is an honest promise; see §7 below) and
`free_grace_checkpoint_elastic_cycles`
(checkpoint cycles run under the elastic ceiling — 0 without an ask, 0
under the lever off). The member-side engagement is the existing
`free_grace_pass_prods`, which the composite makes nonzero on the floor
venue for the first time.

## 3. The rows (release, deterministic; the fleet-cadence shape)

H3 = the hold-time campaign's shipped pair (levers (b) + (d), the D-4
pipeline + demand on); H4 = H3 + the composite. Every row: closure
`deferrals ≡ releases + held` exact, `forced_releases` = `laggard_fences`
= 0, `alloc_stalls` 0, the lane-0 stream at its offered 20.00 blk/s
(80 MiB/s), `hold_unplaced` 0.

| Row | `bound_age` mean / max | residence | `defer→ckpt` / `ckpt→min_acked` / `min_acked→rel` | `hold_ms` | ack lag max / mean | acks | **checkpoints/s** | **renewals/s** | `elastic_cycles` / `pass_prods` | ceiling min | worst staleness |
|---|---|---|---|---|---|---|---|---|---|---|---|
| H3 (composite off — the hold-time note's row) | 7,724 / 7,750 | 7,841 | 498 / 7,339 / 3 | 7,695 | 7,500 / 7,000 | 1,601 | 1.00 | 15.58 | 0 / 0 | 1,000 | 2,000 |
| H4 = H3 + composite, depth at the ROUTINE floor (the first cut — NOT shipped) | 8,520 / 8,750 | 8,602 | 248 / 8,348 / 6 | 8,359 | 8,250 / 7,062 | 1,777 | 2.00 | 24.31 | 600 / 4,778 | 500 | 1,500 |
| **H4 = H3 + composite (shipped)** | **6,750 / 6,750** | **6,975** | 248 / 6,723 / 4 | **6,732** | 6,750 / 6,500 | 3,202 | **2.00** | **31.12** | 600 / 4,778 | **500** | 1,500 |

**−974 ms on `bound_age` (−12.6 %), −866 ms on the residence, −963 ms on
the live hold gauge** — beyond the −0.6 s forecast, because the forecast
counted the three cadence terms halving and not the acknowledgement RATE
doubling with the depth (acks 1,601 → 3,202): the min over 8 members
tightens with it (ack lag mean 7,000 → 6,500, max 7,500 → 6,750). The
`defer→checkpointed` stage halves as contract 31 predicted (498 → 248),
and this time the downstream stage follows (7,339 → 6,723) because the
passes and beats halved with the checkpoint.

**The cost, measured, is EXACTLY the accepted 2×**: checkpoints 1.00 →
2.00/s (600 elastic cycles = every cycle of the steady state), lease-lane
renewals 15.58 → 31.12/s (the product's `membership_renewals` counter:
every beat is a prod at 500 ms and every promotion a carriage renewal, so
both terms double). Prods 3,203 → 6,406, lever (d)'s refreshes 1,431 →
1,712 (rate-limited by `floor ÷ members` = 62.5 ms now), tightenings
unchanged (rung (b) is space-runway-only — constraint 2 by construction).

**The published staleness bound never moves and is honoured**: the worst
`pass + ceiling` any pass ran under is 1,500 ms (a 500 ms pass under a
500 ms ceiling, plus the constant's rounding on the first prodded pass)
against the published `reader_staleness_bound_ms` 2,000, which reads no
prodded state (contracts 35 and the re-stated L2b law both pin it).

Contract 36, a real `KvMetaBackend` with its checkpoint task ticking on
the shipped 50 ms flush, a manual owner clock, one member: no ask ⇒ 0
elastic cycles over 1.25 routine ceilings of commits and the gauge at the
routine; an ask (two pressure harvests at the allocation cliff) ⇒ the
in-force reading `Some(500)` and **2 cycles in 1,250 ms, both elastic**;
the lever off under the same ask ⇒ 0 further elastic cycles.

The three rows above are bit-identical across runs and against the
hold-time note (H3's 7,724 / 498 / 7,339 / 3 / 2,315 / 1,431 are that
note's numbers to the unit); the whole suite's pre-existing rows (A0–A3,
H1–H3, the capacity probe's 8.01 s hold, contract 31's 7,724 / 7,724 /
8,386) are unchanged by the loop's new writer and pass models.

## 4. Capacity through the §4.6 law

Fleet numbers from the hold-time note: 4 GiB lanes (1,024 blocks), a
co-writer's live share ≈ 320 blocks, churn ≈ 50 blk/s. At H3's fleet hold
(7,936 ms measured 2026-09-06 09:57): needed = 397 + 320 = 717 (30 %
headroom). At the composite's in-process ratio (−12.6 %) applied to it,
≈ 6.9 s: 347 + 320 = 667 (**35 %**). The windows re-derivation (items 1–3,
the sibling campaign) is what moves it to the ≈ 50 % the note's §7
arithmetic names; the composite is the item that composes with it —
every term it halves (learn, qualify-rounding, carry) is a term the
windows re-derivation leaves.

## 5. Gate (this side)

* `reader_free_grace_tests` **53/53** (48 + the 5 composite contracts;
  release, `--test-threads=1`), the sibling suites named in the report
  verbatim (the wire-schema bump, the knob registry, the membership grant,
  the checkpoint task, the derivation tie).
* `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D
  warnings` — verbatim in the report.
* No `task check`, no root rigs (the parent's).

## 6. What this does NOT do, and what the parent must know

1. **The fleet row is OWED (parent)** — the from-zero s11-mpiio row with
   the composite on and off. Under the composite the authority's
   `meta_kv_checkpoints` and `membership_renewals` both run at ≈ 2× while
   asking: on the fleet that is ≈ 2 checkpoint cycles/s (each a
   `checkpoint_cycle` — the hold ledger reads its cost as
   `free_grace_checkpoint_cycle_ms`) and ≈ 8 members × (2 beats + 2
   carriage renewals)/s ≈ 32 lease-lane frames/s. Both are far inside the
   §5.8 economy table; the checkpoint side is the one to read first on the
   fleet (the cycle EWMA floor caps the ceiling at 2× the measured cost, so
   a slow device pins it at the routine rather than saturating the task).
2. **The qualify and drain windows are untouched** (items 1–3, the sibling
   `perf/free-grace-ladder-rederivation`; `reader_pass_completed`'s lag
   computation is theirs). The one shared symbol is
   `MemberSession::checkpoint_ceiling_ms`: theirs returns the constant,
   this branch's returns the advertised ceiling (constant fallback). When
   the parent takes this branch's version under their qualify lag
   (`ceiling + skew`), the qualify window becomes demand-elastic too —
   which is SOUND under this branch's two laws: (i) the checkpoint
   decision compares the ceiling against the ELAPSED time, so a ceiling
   that tightens after a label was learned still checkpoints that label's
   dereference by `max(last + new ceiling, tighten instant)` ≤ the
   qualify instant; (ii) the promise pair holds a relaxing writer at the
   advertised ceiling for one routine ceiling past the last elastic grant,
   so a label learned under 500 is qualified against a writer still at
   500. What NEITHER law covers is the shipped argument's own slop — the
   50 ms tick granularity and the cycle's wall time sit outside
   `ceiling + skew` (they always did; the routine `S + skew` had 1,000 ms
   of double-counted `P` hiding them, which item 1 removes) — an
   adjudication the sibling's note owes, sharper under a 500 ms ceiling.
3. **The routine ADVERTISED ceiling is the writer's effective one**,
   `max(flush tick, CHECKPOINT_MAX_AGE_MS)` — the constant on the shipped
   50 ms flush, the tick on a slow-flush venue (a writer at
   `SQUEEZEFS_META_FLUSH_INTERVAL_MS=5000` checkpoints every 5 s: the
   constant alone would be a lie there, and any qualify lag built on it
   unsound). One consequence, stated: on such a venue L2b's pass floor
   becomes 5,000 under the composite lever with no ask (`clamp(ask,
   5000, 5000)`) where the shipped resolver would have honoured an ask of
   1,000 — a pass every second against a writer checkpointing every five
   observed nothing four times in five, and (§6 item 4) the owner never
   sent such an ask anyway.
4. **L2b was structurally inert everywhere before this** — not only on the
   floor venue. The owner's routine floor is `max(P, skew)` and the ask is
   clamped into `[floor, routine]`, so no honest owner ever asked below
   `P`, and the pass resolver's `clamp(ask ≥ P, constant, P)` is always
   `P`. The design's "it pays on slow-flush venues (5 s → 1 s)" needed an
   ask the owner could not send. The composite is what makes L2b pay: the
   live floor `max(min(P, P/2), skew)` = `P/2` is an ask below `P`, and
   `pass_prods` reads 4,778 on H4 against 0 on every prior row.
5. **The `CLUSTER_WIRE_SCHEMA` bump** covers `lane_supply_blocks` too — the
   lane-visible campaign added it to the grant without one; a 1-speaking
   peer of that binary would have decoded a 2026-09-06-morning grant with
   the field silently dropped (bincode positional). Fixed by this bump.
6. **Not measured**: the composite on the `SQUEEZEFS_META_FLUSH_INTERVAL_MS=5000`
   venue (P = 5,000, elastic 2,500, the tick tightening to 2,500 — the
   derivation is tie-tested, the loop model was not run at that shape), and
   the promise pair's behaviour under a fleet whose members renew at
   heterogeneous cadences (in-process every member is prodded).

## 7. Integration with the ladder re-derivation (the parent, same day)

Items 1–3 (`.benchmarks/2026-09-06-free-grace-ladder-rederivation.md`)
made the qualify term the writer's checkpoint LANDING ceiling —
`CHECKPOINT_MAX_AGE_MS + 2 × tick`, the two tick-granularity terms the
task's `tick` evaluates its decision behind (item 1 of that note; the slop
§2 of this note left owed). Composed at integration: **the grant
advertises the landing ceiling of the decision in force** —
`checkpoint_landing_ceiling_ms(flush)` routine (1,100 ms on the shipped
50 ms flush), `checkpoint_landing_ceiling_for_elastic(c, flush)` =
`c + 2 × min(tick, c)` while the valve asks (the task tightens its tick to
the decision, so 600 ms for a 500 ms decision) — and
`MemberSession::checkpoint_ceiling_ms()` reads that value with the
member's own routine landing derivation as the fallback (nothing
advertised, or the lever off). The promise pair stays in DECISION terms
(it is what the task compares elapsed time against). The pass floor
therefore clamps at the landing value (600 rather than 500 on the shipped
shape — conservative by two ticks; the in-process rows above were read
before the composition and are re-read on the fleet by the parent). The
member-side gauge is `free_grace_advertised_ceiling_ms`, kept apart from
the writer's decision-ceiling gauge so the two cannot be mistaken for a
disagreement.

