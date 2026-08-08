# 2026-08-08 — THE L3 COHERENCE CAMPAIGN (moka read-path bookkeeping → read-mostly caches)

| | |
|---|---|
| **Charter** | Build the fix for the field-named term: the moka+hash family = **54.6 % of svc / 33.4 % of dd cycles** ≈ 12 of the 17.96 µs/op svc service time at 706 k IOPS on the 2-socket field box (`.benchmarks/2026-08-08-field-ingress-profile.md` §2–3). Mechanisms in blast-radius order per the charter; local = correctness + no-regression only (a 1-node venue structurally underprices the term — local family share ~7 %); the COUNTED acceptance is the FIELD row, spec §6 below. squeeze-test untouched this session (window closed; host left default-posture by the profile session). |
| **Branch** | `perf/moka-coherence` off dev `0ce0c140`. |
| **The term's mechanism** | The §5.5.1 serve prelude + the 795 completion revalidate pay 2–3 process-global moka gets/op (`CachedMetadata`), plus the §5.3 lane feed's String-keyed `stream_lanes` get + moka's internal `record_read_op`, plus the FileAttr cache get. Every moka **get** performs shared-cache-line RMWs — read-ring push, TinyLFU frequency-sketch touch, entry-timestamp CAS, LRU deque maintenance in `do_run_pending_tasks` — and 24 svc/dd threads over ~32 hot inos turn each RMW into a UPI ping-pong. NOT a hashing problem (the u64 caches already ride ahash) — a **coherence** problem. |

## 1. Mechanism ledger (what each deleted)

### Mechanism 1 — the read-mostly store (`src/read_mostly_cache.rs`, `ReadMostlyCache<K,V,H>`)

The charter's arc-swap read-mostly-snapshot class, per-key (the D7 fold-overlay
/ VL4b ArcSwap `PlacementTable` precedent): values live in an
`scc::HashIndex`; **reads are `peek_with` — pure loads under an EBR guard,
ZERO shared-line writes**. Capacity/TTI/TTL eviction rides a **moka policy
shell** (`Cache<K, u64>`, the u64 = insert generation) built with exactly the
capacity/TTI/TTL derivations the moka value cache carried; its eviction
listener removes store entries **generation-exactly** (a racing fresh insert
is never clobbered by a stale eviction verdict) and under the **pin
predicate**. Writes (insert/remove) publish to the store first, then the
policy — one value source for every reader.

Wired (all three per-op global caches the field ladder named):

| cache | keys | policy | hot reads converted |
|---|---|---|---|
| `metadata_cache` (routing) | u64 ino | capacity RAM/200 KB (unchanged), TTI 300 s, **dirty pin** `\|m\| m.layout_dirty` | ddt probe + 795 revalidate + `placed_sever_for` → **zero-clone `peek_with`** (the former gets each cloned five Arcs per call — contended refcount lines deleted too); `ipc_read_probe_locked` + all ~55 other sites → store reads (clone-out, moka-get parity) |
| `stream_lanes` (routing) | String path | capacity 100 k (leak rail), TTL 30 s from insert (unchanged, incl. the mid-stream re-mint semantics), std hasher kept both arms | `pipeline_touch` get + `ranged_eligible` get + `get_with` → store reads / `get_or_insert_with` (expired entries reconstruct FRESH — moka parity) |
| `attr_cache` (fuse_client) | u64 ino | capacity RAM/100 KB, TTL `daemon_cache_ttl` = 300 s (reader mounts: the staleness-bound TTL, unchanged) | §5.5.1 probe's size read + all sites → store reads (TTL filtered at read) |

Deleted from the per-op path (field-shape row, default mount): **~4 moka
read ceremonies + ~10 contended Arc refcount RMWs per op** (2× CachedMetadata
clone × 5 Arc bumps), plus the read-driven share of `do_run_pending_tasks`
(2.35 %/3.24 % svc/dd) — maintenance is now insert-driven only.

TTL filtering rides the entry's insert stamp against `CLOCK_MONOTONIC_COARSE`
(vdso loads, no rdtsc, no shared-line write — the r5 single-read clock law's
budget is not re-spent; ±1 kernel tick against 30 s/300 s horizons).

### Mechanism 2 — the sampled policy touch (TTI caches)

The metadata cache's residency law is access-refreshed TTI ("any observed
entry stays resident"). With reads no longer recording, each entry carries a
CAS'd coarse touch stamp: at most once per horizon — **derived TTI/4 = 75 s**
(`derived_touch_secs`, tie-tested; `SQUEEZEFS_CACHE_TOUCH_SECS` explicit-wins,
`0` = touch on EVERY read, the un-sampled isolation lever) — one read performs
one policy-shell `get` (moka's read recording at ~1/75 s per hot ino instead
of per op). An entry read at least once per horizon keeps residency with 4×
margin inside the 300 s TTI; one not read within the TTI idles out exactly as
before (listener-lagged — residency, never a correctness contract). The
honest moka-API adjudication the charter asked for: moka 0.12 has **no
recording-free read** on `sync::Cache` (`get` always records;
`contains_key` returns no value), so "a cheaper get path" inside moka does
not exist — the sampling has to live outside it, which is what the policy
shell is.

### Mechanism 3 — per-NUMA-node instances: NOT BUILT (priced out for now)

Deferred per the charter's own rule (build only if (1)+(2) leave a counted
residual): after mech-1 the hot read path performs no shared-line writes at
all, so there is no coherence term left for node partitioning to delete —
only the S-state read sharing of the value lines, which is what caches are
for. If the field re-profile still shows a metadata-read term, the
partition (via `numa_core`'s topology-general map) is the next arm; hit-rate
splitting and W1-inval fan-out are its priced costs.

### The sweep's adjudicated leftovers

* `NvmeCache::get_static` (5.76 % field svc): NOT moka — a sharded
  `parking_lot` read lock + xxh3 over the mmap index. Its per-probe RMW is
  the shard lock itself; a read-mostly conversion means replacing the shard
  container, a different (bigger) campaign. Left on the table, named.
* `dir_entry_cache_v3` / `parent_memo`: kernel-lane dentry surfaces, not on
  the il read path — untouched.

## 2. The staleness-composition argument (why serves stay correct)

1. **One value source, same edges.** The probe/revalidate/probe-locked reads
   now peek the SAME store every mutation site publishes to (the wrapper is
   the only door — the field is private-by-type). The write path's
   "synchronous RAM update" authority, the W1 `notify_inval` pair, and the
   1 s `metadata_entry_fresh_or_dirty` horizon on the `fetch_metadata` path
   are all untouched; scc publishes with release stores and peeks through an
   acquire guard, the same ordering class moka provided.
2. **A snapshot serve never outlives the binding checks.** The 795 protocol
   (custody epoch + binding equality + fill incarnation, both sides of the
   DMA) and the rebind-ladder backstop are unchanged — they exist precisely
   because ANY RAM read races movement, and they bound my store's staleness
   exactly as they bounded moka's.
3. **Expiry semantics preserved where observable.** TTL caches filter at
   read (moka's get behavior) on the coarse clock; TTI caches never filtered
   at read (residency-only), and the listener's lag changes only WHEN memory
   is reclaimed, not what a read can observe on a live mount (D0: no remote
   writer can make a resident entry stale without going through this mount's
   own write path).
4. **The dirty pin is strictly stronger than before.** moka could evict a
   `layout_dirty` entry (the only authority for acked layout state) under
   capacity/TTI; the policy shell CANNOT — the listener declines (gauge
   `read_mostly_dirty_pins`, ≈ 0 steady state since the persist cadence
   outruns the 300 s horizon) until the persist path's clean re-insert
   restores policy presence. The R5 Red shed (`invalidate_all`) keeps its
   existing declared-safe posture verbatim.
5. **No new lock-free algorithm.** The wrapper composes scc + moka; its only
   raw atomics are the insert-generation counter (fetch_add on the write
   path) and the advisory touch stamp (a lost CAS costs one extra policy
   touch). Nothing ordering-sensitive exists to loom; the 12-contract suite
   (incl. a 12-thread never-torn stress) + the ipc/read blast-radius suites
   are the verification surface.

## 3. Local mechanism instrument (bench `read_mostly`, 1-node dev box)

`benches/high_concurrency_bench.rs::bench_read_mostly_cache` — field shape
(32 hot striped `CachedMetadata`, 256-entry block maps, 8 reader threads):

| row | read-mostly | classic moka | Δ |
|---|---|---|---|
| borrowed peek, 1 thread | **12.5 ns** | 99.2 ns | −87 % |
| clone-out get, 1 thread | **55.1 ns** | 137.3 ns | −60 % |
| contended 8 threads × 4096 peeks | **155.3 µs** | 6.495 ms | **41.8×** |

(Final-binary numbers — the peek carries the coarse-clock TTL/touch screen
and the stamp election; the alloc-fix did not change its shape.)

The contended row is the mechanism's own face even on one node: moka's
bookkeeping RMWs serialize the readers; the read-mostly peek is pure loads
and scales flat. The 2-socket UPI amplification (the 54.6 % svc share) only
the field venue manufactures — these rows bound the mechanism, not the win.

## 4. Local no-regression rows (same binary, `SQUEEZEFS_READ_MOSTLY_CACHE` A/B)

Rig: `.benchmarks/rigs/2026-08-08-drainfunnel-rig.sh` (fresh format + fileset
+ remount per leg, ddt mounts, rand-4k il 32×qd32, 60 s + 5 s ramp,
engagement FATAL, Tctl ≤ 70 °C per leg). ON = default (read-mostly), OFF =
`SQUEEZEFS_READ_MOSTLY_CACHE=0` (classic moka verbatim). A-B-B-A both venues.

All cited rows are the FROM-ZERO pass on the final binary (the PERF-12 fix
below restarted every count per the counted-run discipline; the pre-fix
brackets — same directional verdict — live in the run logs, never cited).

### Un-emulated plain nvmet-tcp devsub (32×32)

| leg | IOPS | clat mean | p50 | p99 | ingress mean | svc µs/op | pass mean |
|---|---|---|---|---|---|---|---|
| ON1 | 1,331,842 | 768.0 | 501.8 | 4,080 | 129.4 | 8.90 | 184.4 |
| OFF1 | 1,144,898 | 893.5 | 553.0 | 5,145 | 180.0 | 10.37 | 224.2 |
| OFF2 | 1,116,093 | 916.6 | 544.8 | 5,734 | 179.2 | 10.58 | 221.5 |
| ON2 | 1,123,721 | 910.3 | 501.8 | 6,128 | 145.2 | 10.29 | 192.3 |

ON median 1,227.8 k vs OFF 1,130.5 k = **+8.6 %**; svc µs/op 8.90–10.29 vs
10.37–10.58; ingress mean −19…−28 %; pass mean −13…−18 %.

### Calibrated emulator venue (240 µs null_blk, the r5 §1 field-decomposition match)

| leg | IOPS | clat mean | p50 | p99 | ingress mean | svc µs/op | pass mean |
|---|---|---|---|---|---|---|---|
| ON1 | 765,463 | 1,336.7 | 675.8 | 12,517 | 208.8 | 15.25 | 260.5 |
| OFF1 | 773,769 | 1,322.3 | 725.0 | 10,420 | 225.1 | 15.33 | 271.0 |
| OFF2 | 913,433 | 1,120.1 | 766.0 | 5,538 | 202.1 | 13.19 | 249.5 |
| ON2 | 998,146 | 1,025.0 | 725.0 | 4,620 | 167.4 | 12.01 | 217.9 |

The emu venue warms across a from-cold sequence (the r5-noted convergence
horizon: ON1/OFF1 ran depressed with 10–12 ms p99 tails, then the venue
settled); the settled adjacent pair reads **ON2 +9.3 % over OFF2** with svc
µs/op 12.01 vs 13.19 and ingress −17 %, and the earlier thermally-settled
ON-OFF-ON bracket on the pre-fix binary read the same direction (ON 989.2 k
/ 971.1 k around OFF 942.5 k). This venue is INFLIGHT-bound (r5 §5.5's
regime note) — svc-side deltas read compressed here by design.

**Local verdict: no regression anywhere, and a measurable 1-node WIN in
both venues** — the svc-side instruments (svc µs/op, ingress mean, pass
mean) favor ON in EVERY adjacent pair on both venues, and the composed row
is +8.6 % at median on the svc-sensitive plain venue. The 1-node
bookkeeping term (the local ledger's ~7–9 % svc face) is exactly what
deleted; the field's 5–8× UPI amplification remains the owed acceptance
(§6). Every leg engagement-exact (`ops ≡ dd_serves`, ingress n ≡ ops —
analyzer FATAL gates), `read_mostly_dirty_pins` 0 on every leg,
`read_mostly_policy_touches` = 33 (one per hot ino + stats) only on legs
whose window crossed the 75 s horizon — sampling engaged, never
proportional to the 60–88 M ops/leg.

### The PERF-12 alloc regression the full gate caught (fixed, counted)

`tests/kernel_op_economy_tests.rs` failed the first full gate: warm kernel
WRITE prelude 31.33 allocs/op vs the 30.00 budget. Counted attribution
(same test, three arms): classic moka arm **28.10**; the scc store with ALL
policy inserts skipped **24.00** (the store is 4.1/op CHEAPER than moka);
the regression was the Arc'd touch stamp (+1.33) plus the policy shell's
moka insert paid per op (+6.4 — the attr TTL cache's per-write refresh and
the metadata republish). Fix: inline `AtomicU64` stamp (snapshot Clone) +
**horizon-elided policy inserts** (the payment horizon derives from the
cache's residency horizon ÷ 4 for TTL caches too — their observable expiry
is the store's read filter, so only physical cleanup lags by ≤ one
horizon), with the listener gone **stamp-exact** (removes on
`idle ≥ horizon_base − horizon` against the entry's own stamps; declines
reset the stamp so the next activity re-arms policy presence; the
generation token deleted). Result: **23.00 allocs/op — 5.1 BELOW the
pre-campaign arm.** The warm write path now pays LESS allocation than the
moka posture it replaced.

### Two more things the full gate surfaced (both adjudicated, both fixed)

1. **`script_exec_bit_tests` red at dev `0ce0c140`** (inherited): the r4/r5
   rig scripts `2026-08-08-fabric-emu.sh` + `2026-08-08-fused-predicate-rig.sh`
   landed mode 100644 (committed from a `core.fileMode=false` worktree
   without `git update-index --chmod=+x` — exactly the class the test
   catches). Fixed here so the gate can go green.
2. **`rebind_starvation_tests::read_survives_stripe_free…` — a hot-box
   engagement flake, arm-independent**: failed 2/2 inside the full gate,
   0/33 standalone (both lever arms, `--all-features`, 28-way CPU load);
   a manufactured **Tctl ≥ 100 °C soak** reproduced it on BOTH arms (ON
   2/8, OFF 1/8) — a throttled box can stall the test's STORM
   (alloc/free/displace cycle) past the 100 ms seam window inside a round,
   so a ladder attempt sees a stable binding and legally serves early;
   with a fixed 3-round window the settle-arm engagement assertion then
   read 0→0. The PRODUCT halves (never-EIO, legal-generation serves) held
   in every run. Pre-existing venue sensitivity, not a campaign change;
   fixed by bounded engagement-retry rounds (12) with the product
   assertions pinned verbatim per round — same-soak verdict 12/12 green
   at 100.5 °C.

## 5. Gates

* Contracts: `tests/read_mostly_cache_tests.rs` — 12 tests (visibility,
  borrowed keys, zero-clone peek, exactly-once construction, TTL read-filter
  + expired reconstruct, TTI eviction, touch-every-read residency, the dirty
  pin, invalidate-all, entry gauge, the TTI/4 derivation tie, 12-thread
  never-torn stress). RED-first (committed failing), then green.
* **×10 green from zero on the final binary** (`--test-threads=1`, 11
  suites per run): read_mostly_cache (12), ipc_direct_drive (18), ipc_host
  (37), ipc_op_economy (3 — the `SQZ_ALLOC_TRACE` warm-prelude 0-alloc law
  still pinned), preload_parity (16), attr_refresh (12),
  read_prefetch_pipeline, data_path_correctness (27),
  staged_dirty_layout_refill, env_knob_convention (the two new registry
  entries), kernel_op_economy (the PERF-12 budgets — WRITE now 23.00/op).
* Knobs: `SQUEEZEFS_READ_MOSTLY_CACHE` (bool, default ON — D17 bet posture)
  + `SQUEEZEFS_CACHE_TOUCH_SECS` (int 0..=86400, default derived TTI/4);
  registry entries + convention test; gauges `read_mostly_cache` (posture),
  `read_mostly_policy_touches`, `read_mostly_dirty_pins` on the stats inode.
* Full gate: **`task check` GREEN from zero on the final tree** (both-config
  clippy `-D warnings`, fmt, full `--all-features` suite `--test-threads=1`,
  doc, bench smoke, fuse3 workspace, docs check, audit — the bincode-
  unmaintained residue stays the adjudicated rc-manifest §5 allowance).
  Two earlier gate runs each caught one real thing (the PERF-12 alloc
  regression; the hot-box rebind flake + inherited exec bits) — the
  counted-run discipline restarted every count after each fix; the ×10,
  the A/B rows, and this gate are all from-zero passes of the final
  binary.
* loom: not applicable — no new lock-free core (see §2 pt 5; the only raw
  atomic is the advisory touch stamp — a lost CAS costs one extra policy
  op, weakening-irrelevant by construction).

## 6. THE FIELD ACCEPTANCE SPEC (owed — the orchestrator schedules the window)

The local venue cannot price the win (1-node); this row is the campaign's
counted acceptance. On squeeze-test, dev tip containing this branch:

1. **Deploy** per the standing sequence (task build:rocky8, rsync no-inplace
   + retries, md5 both ends; ssh stderr filter unchanged). Fresh
   default-posture mount (no env vars — read-mostly is the default arm).
2. **The composed row**: 32×32 rand-4k il (libaio, direct=1, 60 s + 10 s
   ramp, norandommap) on the standing 32×1g fileset — vs the profile
   session's **706 k / clat 1,450 µs / ingress 1,165 µs / svc 17.96 µs/op**
   equilibrium (same binary lineage, same venue, `.benchmarks/2026-08-08-field-ingress-profile.md` §1).
   One `SQUEEZEFS_READ_MOSTLY_CACHE=0` remount leg = the control (A-B-B-A if
   the window allows; the knob makes it one-binary).
3. **The mechanism instruments** (each must move for the row to be
   attributable): the profile rig re-run (svc+dd dwarf mid-row) — the
   moka+hash family's within-population share is THE mechanism gauge
   (54.6 %/33.4 % → the residual should be the `get_static`/engine ladder);
   `ipc_drain_pass_ns`-derived **svc µs/op** (17.96 → the ceiling arithmetic
   says ~8–10 µs if the ~12 µs family share deletes at field amplification);
   `read_mostly_policy_touches` (≈ 32 inos / 75 s — sampling engaged, NOT
   proportional to ops); `read_mostly_dirty_pins` ≈ 0; the standing
   engagement gates (ops ≡ serves, ingress n ≡ ops, tripwires 0) FATAL.
4. **The projection, honestly bounded**: svc service ceiling 12 threads ÷
   svc µs/op → at 8–10 µs/op the ceiling is 1.2–1.5 M; composed with the
   measured device inflight (308 µs) and the remaining ingress queue, the
   row should land in the **0.9–1.3 M class** (the field-profile §3 ceiling
   arithmetic). BELOW ~0.85 M with the family share collapsed ⇒ the next
   term (get_static shard locks / engine bodies / kernel net) is the new
   head — profile names it, same methodology.
5. **Leave the host** mounted default-posture, healthy (P0 smoke ×3), same
   close-out as the profile session.

## 7. Carried debts

* `tests/run_bench_baseline.sh save` — still owed on a quiet local window
  (now also carries the `read_mostly` group's inaugural reference).
* Mechanism 3 (per-node instances) — only if the field re-profile leaves a
  counted metadata-read residual.
* `NvmeCache::get_static` shard-lock RMW (5.76 % field svc) — named, not
  taken.
