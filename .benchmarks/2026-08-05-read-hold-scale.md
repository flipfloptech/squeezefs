# 2026-08-05 — Hold-churn root cause: the coverage-credit bug (retirement death), the marginal-issue walk, and landing-zone-pressure headroom

Branch `perf/read-hold-scale` (worktree off `integrate/zcrx-wave` tip
`d3e9f595` — the deployed field pair, **unmerged — the orchestrator
merges**). Charter: root-cause the field acceptance row's hold churn
(pair `d3e9f595`, fio libaio 1M nj16 qd8 cold: **28.03 GB/s** vs the
35.5 bar; engage-governor dead-gain retreats `ups == backoffs == 14`,
`read_lane_fetches` 1,912, **`read_lane_hold_evicted_unconsumed`
9,025**), red-first, cluster READ-ONLY.

Commits: red `5d6ebabf` (contracts 11–13) · green `cda6f8e7` (the three
fixes) · this note. Local artifacts `/tmp/rbw2_rows/` (per-row fio
JSON, stats before/after).

## 1. THE ROOT CAUSE — three stacked, all empirically pinned

### 1.1 The coverage-credit bug (retirement death) — the dominant term

The fetch-loop tail credit computed
`min(slice_len, served.len() − slice_start)` from the **post-slice**
buffer: on every dest-armed serve (`fuse3_read_inplace_replies ≡ ops`
on the field row — ALL kernel serves are dest-armed) `served.len() ==
slice_len`, so **every sub-read past a block's first credited ZERO**.
Coverage retirement died on cohort-dominated rows:

| evidence (local S1: 16j qd8, dest-armed) | base `d3e9f595` | fixed `cda6f8e7` |
|---|---|---|
| holds / retired | 122,813 / **666 (0.5 %)** | 124,787 / **124,785 (100 %)** |
| evicted_unconsumed | **120,035** | **0** |
| `read_lane_hold_bytes` at row end | **8.86 GB** (pinned at budget) | **8.4 MB** |

The hold became a budget-pinned FIFO (~8–11 GB local, ~7.5 GiB on the
field box) whose evictions were mostly *consumed-but-uncredited*
transit — `evicted_unconsumed` stopped meaning starvation. Red
evidence: contract 12 under the reverted credit —
`served_bytes = Some(0)` vs the contracted `Some(131072)`.

### 1.2 Skip-dominated issue — why probe delivery could never respond

The lane cursor rebases onto the qd-covered demand front; at depth 1
every issue raced into a registry-owned block and skip-settled through
FULL task latency while occupying the lane's only depth slot. Field
arithmetic: ~25k issue opportunities across the 14 probe epochs (7 s
armed), **1,912 real fetches (~8 %)** ≈ 137/epoch × 4 MiB ≈ 573
MB/epoch ≈ **+7.7 % vs the +6.25 % adopt threshold** — riding AT the
threshold, drowned by thrash noise. Fix: the marginal-issue walk
(`lane_block_covered` — registry/hold/hot/LRU/tier/hole, all sync RAM
probes): covered blocks advance the cursor for one probe, never a task
or depth slot; walk bound = the derived §5.5 window cap (never a
constant); fetched-ahead bytes stay depth-bounded (the round-2
EOF-sprint law). Contract 13; engagement gauge
`read_lane_covered_skips`.

### 1.3 Unguarded probing into thrash

Headroom ignored landing-zone pressure: a probe launched into a hold
evicting its ahead deposits measures its own thrash as dead gain
forever. Fix: `read_lane_hold_ahead_evictions` (the ahead-class subset
of evicted-unconsumed, classed in `trim_to`) moving since the last
epoch reads as ZERO headroom (contract 11). **Item-2 adjudication
(separate ahead priority): NOT built, by measurement of the
mechanism** — the FIFO is oldest-first and ahead entries are the
newest deposits, so demand transit structurally evicts first (pinned:
`hold_trim_retains_ahead_entries_and_classes_its_evictions`); with
retirement truthful, demand transit retires by consumption and rarely
reaches trim at all. The classed counter is what keeps the pressure
signal clean without a second queue.

## 2. THE FIELD-SHAPE HOLD ARITHMETIC (item 1)

176 GiB box: R5 cap = mem/8 = **22 GiB**; deposit-site budget =
max(cached, fallback) where fallback = 4 × streams(~30) ×
window_depth(16) × 4 MiB ≈ **7.5 GiB**. The SHAPE's true working set
with truthful retirement: in-flight fills (21–60) + ahead (streams ×
depth ≤ 16 × 4) + cohort/retirement lag ≈ **0.3–0.7 GiB** — the byte
budget was never the binding failure; the retirement LEDGER was (the
9,025 evictions are dead credit + the ~1,912 dead-raced ahead entries,
not a too-small window). The cached derivation nonetheless now carries
item 1's stated form — `hold_budget_bytes(mem, block, streams, depth +
derived cohort window)` (ahead pipeline + flow-through retirement lag),
max()'d with the fallback — and the governor's headroom now sees
ahead-class pressure, so a genuinely R5-starved shape refuses to probe
instead of thrashing.

## 3. PROBE-SIGNAL ADJUDICATION (item 3 — honest)

* Epoch 500 ms = **85 fill RTTs** (5.85 ms) — adequate settling.
* The +1-block/stream quantum is NOT undetectable, because the read
  response is **concurrency-multiplied, not additive-fetch-bytes**:
  each marginal ahead fill unblocks a whole 4-op cohort. At the field
  shape, depth-1 × 16 streams of MARGINAL fills moves in-flight fills
  21 → ~37 ≈ **+50–75 % expected delivery response = 8–12× the +6.25 %
  adopt threshold**. The write-side ¼-multiple concern does not
  transfer.
* The field's dead gain was structural, not quantum-size: only ~8 % of
  issues were marginal (§1.2), putting the real signal AT the
  threshold, under ±4 %/epoch thrash noise (9,025 evictions ≈ 36
  GB/row of refetch-class noise). With the walk + retirement truth the
  signal is an order of magnitude above threshold. **No bigger probe
  step and no epoch change shipped; none is warranted until a field
  row shows adoption failing with `covered_skips` engaged and
  `hold_ahead_evictions = 0`.**
* The retreat arm stays honest at a genuine device wall: every local
  device-saturated row ran `ups == backoffs` with ≤ par cost.

## 4. LOCAL A/B (tcp devsub; cold remount/rep; medians of 3; base = the deployed `d3e9f595` binary)

**The churn shape (S1-loop: 16j qd8 over 16×1 GiB, the field
signature's local repro) — the item-4 acceptance: fetches CONSUMED,
not evicted:** gauge flip in §1.1's table (retired 0.5 % → 100 %,
evicted 120k → 0, hold 8.86 GB → 8.4 MB); throughput 7.33 vs 7.55 GB/s
(−2.9 %, the artifact handback below; the box's device wall ~7.3 GB/s,
clat 18.5 ms both sides).

**HONEST FINDING — the retirement-dead hold was an accidental cache,
and truthful retirement hands it back.** On LOOPING sets that
partially fit mem/8, the bug's budget-pinned FIFO functioned as a
GB-class, ledger-invisible, admission-bypassing RAM tier:

| venue (looping) | base (dead hold) | fixed | Δ | attribution |
|---|---|---|---|---|
| S2-loop 4j qd2 over 4 GiB (fits) | 10.79 GB/s med (hold 2.05 GiB ≈ ½ the set; 115k hold serves + 425k hot hits bootstrapped via serve ceremonies; get_obj 103.8k) | 7.11 med (hold 16 MiB; device-bound; get_obj 123.2k) | **−34 %** | the accidental cache, byte-for-byte |
| S1-loop 16j qd8 over 16 GiB | 7.55 med | 7.33 med | −2.9 % | same class, residual (residency 1.26 s < 2.2 s pass) |
| S2′ 4j qd2 over 16 GiB, fresh store, BOTH orders (A-B-B-A) | 6.75 med | 6.48 med | **−4.0 %, order-independent** | partial fit (2.15 GiB window over 16 GiB) |

Adjudication: this is a BUG's side effect, not a contract — the hold's
design is explicitly coverage-retired/converge-by-consumption,
retention warmth belongs to the R1b-governed tiers (the dead hold
bypassed the scan-resistance governor entirely and pinned mem/8 = 22
GiB on the field box for 0.5 % retirement), and the pre-lane A0
posture never had this cache either. The prior campaign's local S2
"+7.9 %" rode this artifact venue (both sides dead-held), which is why
it transferred to only +2 % on the field. **The lawful remedy for
fleets wanting GB-class RAM re-read warmth is the governed hot tier**
(`SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB` / `--read-mem-cache-size` — the
derivation exists) — flagged to the orchestrator as the explicit
trade. On the FIELD target shape (1.6 TB set, minutes/pass, residency
≤ 0.6 s) the cross-pass artifact contributes ≈ 0, so the acceptance
row loses nothing and gains the probe unlock.

## 5. Verification

* Red-first: contracts 11–13 (`5d6ebabf`) — compile-red (new APIs) +
  behavioral red on the credit truth (`Some(0)` vs `Some(131072)`
  under the reverted posture, demonstrated on this branch).
* Suites green (serial): read_lane (17), read_prefetch_pipeline,
  read_prefetch_window, mem_budget, read_tier_admission,
  read_admission_governor, hot_block_tier, read_copy_ledger,
  read_tier_refetch_churn, read_saturation, read_stream_transient,
  hybrid_io, ranged_read, rebind_starvation, read_serve_phase.
* ×10 consecutive on the touched async suites (read_lane +
  read_prefetch_pipeline): 10/10.
* clippy `-D warnings` both feature configs; fmt clean.
* Venue incidents (journaled): the shared zram store exhausted at 28
  GiB of fill (rows during exhaustion discarded); substrate recreated
  fresh; one driver run terminated by the session wrapper (its
  orphaned row completed and was harvested); the S2′ bracket was
  completed in both orders on the fresh store.

## 6. EXPECTED FIELD GAUGES (pair from `cda6f8e7`, the same acceptance row)

* `read_lane_hold_retired` ≈ `read_lane_holds` (was 0.5–95 % mixed);
  **`read_lane_hold_evicted_unconsumed` ≈ 0** (was 9,025) and
  `read_lane_hold_ahead_evictions` ≈ 0; `read_lane_hold_bytes`
  MB-class (was riding the 7.5 GiB budget).
* `read_lane_covered_skips` ≫ 0 (the walk crossing the qd front);
  `read_lane_fetches` ≫ 1,912 with `read_lane_fetch_bytes` ≈ the
  ahead share of device reads.
* `read_lane_depth_probe_ups > backoffs`, `read_lane_depth_target`
  settling 2–4 (the §2 arithmetic: shortfall ≈ (58 − 21)/16 streams).
* `block_fetch` est-mean collapsing from 5.44 ms as in-flight fills
  move 21 → 55+; seq-read from 28.0 toward the 35.5 bar (fill-side
  bound clears at ≈ 58 in-flight fills; past it the transport-ingress
  3.25 ms/op term owns the residual — the serve-decomposition §6.1
  campaign).
