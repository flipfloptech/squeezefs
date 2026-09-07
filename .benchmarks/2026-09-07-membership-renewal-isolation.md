# Membership renewal isolation — where the renewal's 10 s went (finding 15 phase B1)

**Date:** 2026-09-07 · **Branch:** `fix/membership-renewal-isolation` (off `dev` @ `a7cab076`) ·
**Input:** `.benchmarks/2026-09-07-f15-day2-fleet-pair.md` §2 (the day-2 fleet pair, run
`keep-day2-B`: authority `m0` + 8 co-writers `m50–m57`, samples at 1 Hz) ·
**Contracts:** `tests/membership_renewal_isolation_tests.rs` ·
**Design:** `docs/design-free-grace-sustain.md` §Renewal-isolation campaign ·
**Venue for every number below:** in-process, the dev box, `cargo test` (debug) — scoping
evidence. **The fleet re-read is the parent's, on squeeze-test** (§7).

## 1. The question, and the three candidates the parent named

During the file-per-proc phase (t = 100–150 s of the samples; wall 14:33:49Z–14:43:22Z)
every co-writer's OWN `free_grace_acked_lag_ms` read ≤ 2,300 ms, yet the authority's
`free_grace_member_ack_lag_ms.max` read 10,000–11,600 and its served `membership_renewals`
collapsed from 22/s to 0–12/s. The bound stalled, `free_grace_offsets` climbed to 3,200,
every lane starved. Candidates: **(a)** the authority's serve blocks on something the
harvest/free storm holds; **(b)** the member's renewal task cannot send (clogged venue);
**(c)** the wire's own round trip. The parent's rule: decide by instrument, not argument.

## 2. What the samples and logs say — none of the three

The authority's per-second rows (`samples/m0.jsonl`, `i` = seconds since file start):

| i | served renewals Δ/s | `member_ack_lag.max` | `.mean` | `free_grace_offsets` | `prod_renew_ms` |
|---|---|---|---|---|---|
| 98 | 22 | 2,350 | 1,554 | 175 | 500 |
| 99 | 20 | 2,347 | 1,552 | 24 | 500 |
| **100** | 11 | 2,021 | 1,628 | **0** | 500 |
| 101 | 3 | 2,799 | 2,372 | 0 | 500 |
| 102 | 2 | 3,801 | 3,060 | 23 | 500 |
| 103 | **0** | 4,797 | 4,056 | 633 | 500 |
| 104 | 0 | 5,801 | 5,060 | 1,340 | 500 |
| 105 | 0 | 6,798 | 6,057 | 2,044 | 500 |
| 106 | 0 | 7,805 | 7,064 | 2,303 | 500 |
| 107 | 0 | 8,837 | 8,096 | 2,743 | 500 |
| 108 | 0 | 9,805 | 9,064 | 3,210 | 500 |
| 109 | 8 | 10,804 | 10,063 | 3,247 | 500 |
| 110 | 15 | 11,649 | 8,374 | 3,247 | 500 |
| 111 | 16 | 11,360 | 3,999 | 3,247 | 500 |
| 112 | 10 | 2,460 | 1,811 | 3 | 500 |

Three facts pin the mechanism:

1. **The `mean` walks 1 s/s together with the `max`** from i = 100 to 108: EVERY member
   stopped delivering renewals at the same instant, for ~9–10 s, and they all resumed at
   i = 109–111. A stall in one connection thread, one lock, or one member's venue does
   not do that; a cadence handed to all of them at once does.
2. **The instant is the ring DRAINING** (`free_grace_offsets` 24 → 0 → 0 at i = 99–101),
   with the ask still in force on the authority (`free_grace_prod_renew_ms` = 500
   throughout — readings ≤ 1 s old). The ring refilled two seconds later
   (23 → 633 → … → 3,210) and nobody could be told.
3. **Every member's own `free_grace_acked_lag_ms` FROZE** over the same window
   (`m50`: 1,500 from i = 100 to 109, then 786; `m53`: 1,433 from 100 to 111). That gauge
   is stored AT a promotion (`now − learned_at`), so a frozen value means NO promotion for
   10 s — the "≤ 2.3 s" reading was the last promotion's age, not evidence of a recent
   one. The members had nothing new to acknowledge because they had learned no label
   past the refill: a member's label source is the ROUTINE renewal grant (a carriage
   renewal learns none — the hold-time lever (b) design), and the routine beat is
   `renewing every 10000 ms` (every co-writer's arm line, `m5*.log`).

And the negative evidence: across all nine daemon logs there are **zero** `renewal
failed`, `re-asserting as a reclaim`, `exceeded its … ms deadline`, `SELF-FENCED`,
`session I/O lost` or `re-joined FRESH` lines. The renewal RPC never stalled anywhere
near its 10 s socket timeout or its ~13 s attempt bound. The 63 `no reply to call …
within 10s` lines in the run are ALL against `192.168.86.52:45999` — the PUBLISH plane's
listener (`meta_ship::publish`, a different `RpcAsyncService` on the meta lanes) — the
concurrent storm the parent named, being fixed in parallel, and not the membership
plane (which listened on `:45981`).

**So the 10 s went into (d): the beat.** The mechanism in the code:
`MembershipOwner::renew` → `free_grace::take_prod_cadence(acked)`; its caught-up arm
`acked_free_epoch >= PROD_LABEL → None` ("a member that has already acknowledged past
the label the writer is waiting on is not holding the free list and is not asked") hands
the member `grant.renew_ms = routine` = 10,000. At a full drain every member is caught up
(its ack ≥ the newest label the ring held), so all eight drew the routine beat within the
same second; the refill's labels (t + 2 s) could not be learned until that beat fired.
Same signature at i = 112–121 (one member, `m50`, frozen 1,390 for 10 s while `m53`
kept chaining — the ring touched 3 offsets at i = 112, so only the member that renewed
in that instant drew routine) and at 122–133.

## 3. The instrument (so the fleet decides, not the reader of samples)

Always-on, the standard 26-bucket shape (`crates/squeezefs-ipc/src/latency_core.rs`):

* **Member** — `membership_renew_phase_ns`: `carry_wait` (the DECISION — a routine
  beat's due instant, or the promotion that asked for carriage — → the frame's send
  instant, stamped on the blocking venue's thread: lane scheduling + venue wait), `rtt`
  (send → reply), `total` (decision → grant adopted). Plus `membership_renew_cadence_ms`
  — the beat in force, the label source's cadence.
* **Authority** — `membership_renew_serve_ns`: frame in → reply built, inside
  `MembershipService::call`'s RENEW arm.
* **Reading rule**: all three member spans small and the serve small while the member's
  cadence reads 10,000 and the authority's `free_grace_prod_renew_ms` reads 500 is (d),
  the beat hole. `carry_wait` large with `rtt` small is (b). `rtt` large with the serve
  small is (c). The serve large is (a).

Cost: one `Instant::now()` per boundary and three relaxed `fetch_add`s per span — the
`meta_ship_phase_ns` contract.

## 4. In-process reproduction (red-first) and the fixes

All rows: `cargo test --test membership_renewal_isolation_tests -- --test-threads=1 --nocapture`,
dev box, debug profile. "Pre-fix" = the same suite with both mechanisms forced off (the
contracts' control arms assert the shipped shape explicitly, so the pre-fix shape stays
pinned after the fix).

### 4.1 Contract 1 — the drain-instant grant (the fleet's 10 s) — RED pre-fix

`Storm`-driven owner on a manual clock (the `reader_free_grace_tests` harness): storm
until the ask is in force, drain the ring by an honest full acknowledgement, probe the
owner's renewal path for the caught-up member while the ask is live.

| arm | ask in force | routine | grant to the caught-up member |
|---|---|---|---|
| shipped (`SQUEEZEFS_FREE_GRACE_CAUGHT_UP_RELAX=0`) | 1,659 ms | 10,000 ms | **10,000 ms** — the hole |
| fix (default) | 1,659 ms | 10,000 ms | **3,318 ms** — one doubling step |

Pre-fix failure line: `a caught-up member under a live ask must be relaxed ONE step
(≤ 3318 ms), never handed the routine hole — got 10000 ms (routine 10000)`.

**The fix** (`src/free_grace.rs`, `take_prod_cadence`): while the ask is LIVE, a
caught-up member is relaxed one doubling step of the cadence in force — finding 18's
decay unit — capped strictly below routine, instead of being snapped to routine. Being
caught up at one beat says nothing about the writer's next free; the refill is now
learned within `2 × cadence`, a caught-up member still beats half as often as a laggard
(the economy's intent kept), and the ask's LIFETIME is unchanged — past the reading TTL
a drained ring retires it and routine returns (the existing contract
`a_drained_ring_retires_the_expired_ask_immediately` is green, and contract 1b
`the_relaxed_step_exists_only_while_the_ask_is_live` pins the lapse). Lever
`SQUEEZEFS_FREE_GRACE_CAUGHT_UP_RELAX` (default on; `0` = the snap, the A/B control);
gauge `free_grace_prods_caught_up` (disjoint from `free_grace_prods`).

### 4.2 Contract 2 — the renewal's wire hop (candidate (b), the venue) — RED pre-fix

`RpcClient::call` ran every socket round trip on the SHARED `sqz-blk` pool, FIFO. Finding
2 (2026-08-20) isolated the renewal's POLL onto `sqz-lease`; its SEND still queued behind
every parked RPC round trip / reclaim lane / crypto job of a co-writer. Loopback plane,
member joined, then the pool parked solid (`pool_cap_from(cpus) + 16` = 528 jobs
sleeping 600 ms):

| arm | renewal wall | `carry_wait` |
|---|---|---|
| shipped (`SQUEEZEFS_MEMBERSHIP_RENEW_LANE=0`) | 591 ms | **590 ms** — the bulk work's own duration |
| fix (default, `sqz-lease-io`) | 0.58 ms | **64 µs** |

Pre-fix failure line: `the renewal must not queue behind the parked pool: wall
599.915144ms against a 600ms hold`.

**The fix**: `squeezefs_ipc::sqz_blocking::DedicatedWorker` — one named OS thread with
its own FIFO, the same job wrapper and `RunBlocking` future as the pool — and
`RpcClient::call_timed_on(verb, body, Some(lane))`; `MemberClient::renew_decided` runs
the membership renewal on `sqz-lease-io` (spawned on the first renewal of an armed
member — the `sqz-jrnl` shape: liveness I/O owning its thread). Lever
`SQUEEZEFS_MEMBERSHIP_RENEW_LANE` (default on; `0` = the shared pool); gauge
`membership_renew_lane_calls`. The custody renewal (S9) keeps its own path on purpose:
one io thread for both cadences would re-couple them.

**Honesty:** the fleet samples show no venue wait — the beat explains the whole 10 s —
so this is a structural completion of finding 2's law, convicted in-process, not a
fleet-convicted term. Whether the field's parked publish calls (63 × 10 s timeouts in the
run, each holding a pool thread) ever saturated a co-writer's 64-thread pool is exactly
what `carry_wait` now says.

### 4.3 Contract 3 — serve and RTT under an authority storm (candidates (a)/(c)) — GREEN

Loopback plane; 8 storm clients hammering the membership listener (77,470–79,702 census
RPCs in 1.2 s ≈ 65k RPC/s) while the authority's `sqz-meta` lanes sit parked; the member
renews 57 times meanwhile:

| run | serve mean | RTT mean | RTT max |
|---|---|---|---|
| 1 | 41.7 µs | 149.9 µs | 389 µs |
| 2 | 37.0 µs | 141.4 µs | 459 µs |
| 3 | 28.5 µs | 130.7 µs | 273 µs |

Green before and after: the serve is one `scc` probe plus atomics on the connection's
own thread, and nothing the storm holds is on its path. (a) and (c) are not where 10 s
can go on this plane.

### 4.4 Contract 4 — the instruments' shape

Three standard histograms + one, exact-sum containment `total ≥ carry_wait + rtt`, the
member's `membership_renew_cadence_ms` = `MemberSession::renew_interval_ms()`.

## 5. Suites (this worktree, `--test-threads=1`)

| suite | result |
|---|---|
| `membership_renewal_isolation_tests` (new) | 5 passed |
| `dlm_membership_tests` | 51 passed |
| `reader_free_grace_tests` | 61 passed |
| `free_grace_lane_visible_tests` | 10 passed |
| `mw_cowriter_lane_tests` | 26 passed |
| `dlm_cowriter_tests` | 18 passed |
| `cluster_wire_tests` | 23 passed, 1 ignored |
| `derivation_sweep_tests` | 47 passed |
| `env_knob_convention_tests` | 21 passed |
| `audit_instruments_tests` | 26 passed |
| `membership_liveness_tests` | 4 passed |
| `squeezefs-ipc` `sqz_blocking` unit tests | 5 passed |

Plus `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`,
`cargo clippy --all-targets -- -D warnings`, `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps`
(the report states their outcome).

## 6. What is NOT claimed

* **The fleet re-read.** The instruments exist so the parent's squeeze-test run decides
  (a)/(b)/(c)/(d) live; the samples above were read after the fact, and the dev box is
  scoping only (heat soak). PASS on the re-read: the authority's served
  `membership_renewals` never drops to 0/s while `free_grace_offsets` refills within one
  reading TTL of a drain; `free_grace_member_ack_lag_ms.max` ≤ `2 × prod cadence` +
  the qualify window (≈ 2.3 s at a 500 ms ask) across a drain-and-refill;
  `free_grace_prods_caught_up` > 0 around every drain; `membership_renew_phase_ns.carry_wait`
  mean ≤ 1 ms and `membership_renew_serve_ns` mean ≤ 100 µs on every co-writer; plus the
  A/B legs `SQUEEZEFS_FREE_GRACE_CAUGHT_UP_RELAX=0` (the 10 s shape) and
  `SQUEEZEFS_MEMBERSHIP_RENEW_LANE=0` (the shared-pool shape, `carry_wait` the readout).
* **The first-ask-late cost of a genuinely quiet plane.** A ring that stays drained past
  one reading TTL retires the ask (the finding-18 economy half, unchanged), and a burst
  after that still pays one routine beat before the first ask lands — a pull model's
  inherent cost, named here as an open item, not touched.
* **The publish plane's 10 s timeouts** (`:45999`) — a sibling's fix.
* **The custody renewal's venue** (S9) — left on its own path by decision.

## 7. Files

`src/free_grace.rs` (the caught-up arm, lever, gauge), `src/membership_wire.rs` (the
instruments, the `sqz-lease-io` venue, `renew_decided`), `src/membership.rs` (the
loop's decision instants, `membership_renew_cadence_ms`), `src/cluster_wire.rs`
(`call_timed_on`, `CallTiming`), `crates/squeezefs-ipc/src/sqz_blocking.rs`
(`DedicatedWorker`), `src/fuse_client.rs` (stats keys), `src/env_knobs.rs` (two
registry entries), `tests/membership_renewal_isolation_tests.rs`, `docs/operations.md`,
`docs/design-free-grace-sustain.md`, `AGENTS.md`.
