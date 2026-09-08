# 2026-09-08 — D-5: the owner's dispatch hop, the single-connection publish depth, the accept tick (DLM #7 / #8)

**Branch** `perf/owner-hop-and-depth` (worktree off dev `36d517f3`). RED
`697227b3` → instrument + lever `8aa7dece` → accept `a7334067` → isolation
contract + A/B harness `35fbab7c` → the single-connection half (this note's
commit train). Campaign: `docs/design-e2e-perf-audit.md` §3.4 **DLM #7**
(the owner's `spawn_meta_join` hop) + **DLM #8**'s single-connection half,
§5.3 **row 18**, Appendix D #7/#8; opened by the C-2 note's Owed 3
(`.benchmarks/2026-09-03-c2-uring-fs-completion-hop.md`) and the D-1b
note's Owed 4 (`.benchmarks/2026-09-02-d1b-publish-plane-batching.md`).
Contracts: `tests/owner_dispatch_hop_tests.rs` (8 + the A/B rows by name),
`tests/cluster_wire_tests.rs` (`a_dial_never_waits_for_the_accept_tick`,
`pipelined_calls_on_one_session_are_served_concurrently_and_demuxed_by_id`),
`tests/publish_plane_batching_tests.rs` (the D-1b depth contract, pinned to
the pool arm). Target release **1.2.1**.

## The conviction (C-2's fleet attribution, not re-derived here)

A shipped frame is admitted on the connection's own thread (`sqz-clw-conn`,
one OS thread per connection), then DISPATCHED onto the two shared
`sqz-meta` lanes via `spawn_meta_join` and awaited. The ledger measured the
hop at ≤ 32 µs quiet; on the fleet `meta_ship_owner_phase_ns.dispatch` read
**2.0–2.3 ms per verb** — a cross-thread wake into lanes the co-writers'
publish storms saturate, and one back — the owner's largest remaining term
once D-2/C-2 stopped the conveyor binding. The same lanes also carried the
D-1b publish plane's per-call/group hops and the client-side lane drains.

## The instrument (landed first, `8aa7dece`)

`meta_ship_owner_dispatch_ns` — always-on, exact-sum, zero-alloc (the
`uring_fs_write_phase_ns` pattern: the lane-side instants ride the join's
oneshot payload, `meta_exec::spawn_meta_join_stamped`), recorded by the ONE
door every owner-side dispatch on BOTH planes goes through
(`meta_ship::owner_dispatch` — the S8 verb frame; the S9 publish call,
group, free, harvest):

| phase | span |
|---|---|
| `queue_hop` | submitted → the lane's first poll (spawn → lane queue → pop, behind whatever the lanes hold) |
| `run` | first poll → the work's last instruction — INCLUDING every wake the work takes back onto a lane while it runs there (the conveyor's fan-out, a 4a guard) |
| `wake_hop` | done → the awaiting connection thread resumed (oneshot waker → `thread::unpark` → dispatch) |
| `total` | submitted → observed; ≡ the S8 frame's `meta_ship_owner_phase_ns.dispatch` on a single-chain frame, to the ns (pinned) |

Engagement `meta_ship.owner_dispatch_{inline,hops}` (`inline + hops ≡
total.count`; an unwound HOP records no split — its lane-side instants die
with the task).

**What the split said, quiet (debug, 16 one-verb frames):** hop arm
`queue_hop` 8.4 + `run` 25.7 + `wake_hop` 2.6 = 36.7 µs. **Under a 4 × 200 µs
serve hog on the lanes (release, below):** `queue_hop` 347–350 µs, `run`
414–428 µs, `wake_hop` 4.5–5.4 µs of 768–781. The attribution the campaign
needed: the `wake_hop` — an unpark of a parked OS thread — is NEVER the
term (≤ 5 µs at every load); the `queue_hop` is the lane's FIFO wait; and
the largest term is inside `run`: every wake the work takes (the commit
fan-out from the journal lane) lands on the hogged lane queue and waits
behind the bursts again. A lever that only removed the two hops would have
left half the loss; the lever that removes the LANE from the dispatch
removes all of it.

## The lever (`8aa7dece`) — execute on the accepting venue

`SQUEEZEFS_META_SHIP_INLINE_SERVE` (default **on**): the dispatch's future
is polled on the accepting connection's own thread (`sqz_blocking::block_on`
was already its executor; since the session lane below, the lane on that
thread). Both hops are 0 by construction and every wake inside the work
unparks that thread directly instead of queueing behind the lanes. `0` =
the shipped `spawn_meta_join` hop (the same-binary A/B control).

Why the hop's reason is gone (the module doc's argument, `service.rs`): it
existed while `commit_tx` spawned the volume's conveyor pass task on the
committer's AMBIENT runtime — a verb executed inline on a lane would have
given the conveyor a lane-lifetime venue. Since rip-tokio-total every task a
served verb touches spawns on an explicit process-global venue (the pass on
the volume's `sqz-jrnl` lane, the checkpoint/times tasks on the `sqz-meta`
pool — never the caller's), task-locals are executor-agnostic (a thread-
local stack pushed per poll), and the connection thread is dedicated and
parked for exactly this reply. What the hop's `contain` gave — panic
containment — is applied per dispatch: an unwinding verb answers
`STATUS_PANIC` / the cached `PUBLISH_PANIC` outcome inside its witness
window, and the session serves on (pinned: a panicking free executor, the
next call on the SAME pooled session lands with no re-dial, the replay
answers the cached failure). The S8 multi-chain frame's chains are polled
concurrently in-task (`join_all`, each contained — the publish plane's
shape) on the inline arm; per-chain `spawn_meta_join` stays on the control.

Preserved and pinned on the inline venue: same-ino chain order + the
independent siblings' co-queue (62-verb frame: ≤ FRAME/4 passes, FRAME
journal entries), the dedup window (a replayed frame is N hits, zero
journal movement, identical outcomes), one conveyor group per publish frame
(D-1c), STATUS_PANIC containment, and the control arm byte-identical
(`hops` grows, `inline` stays 0, the hops read > 0).

## In-process A/B (release, 32-CPU dev box, file-backed KV sandbox, loopback wire)

`cargo test --release --all-features --test owner_dispatch_hop_tests -- --ignored --nocapture ab_rows`.
**Dev-box rows are scoping evidence** (user directive 2026-09-07 — heat soak;
the acceptance pair runs on squeeze-test, the parent's). Box load 14–17
during both rolls (another agent's substrate present). Four legs A-B-B-A
per venue (inline / hop / hop / inline); two rolls, both quoted.

**S8 verb plane — 4 clients × 256 one-verb `setattr` frames against one
authority** (`meta_ship_owner_phase_ns.dispatch` mean / bucket p99; the
split's means; verbs/s):

| venue | arm | verbs/s | dispatch mean | p99 | queue_hop | run | wake_hop |
|---|---|---|---|---|---|---|---|
| quiet | inline | 47,138 / 43,935 | 41.4 / 45.7 µs | 128 / 128 | 0 | 41.4 / 45.7 | 0 |
| quiet | hop | 43,876 / 46,108 | 50.4 / 46.8 | 256 / 256 | 4.9 / 4.8 | 42.4 / 38.8 | 3.2 / 3.2 |
| quiet | hop | 41,968 / 41,508 | 55.2 / 54.2 | 256 / 256 | 7.4 / 5.9 | 44.6 / 44.9 | 3.1 / 3.3 |
| quiet | inline | 44,208 / 45,700 | 49.6 / 45.8 | 256 / 128 | 0 | 49.6 / 45.8 | 0 |
| hog 4 × 200 µs | inline | 4,772 / 4,912 | 64.5 / 42.2 | 512 / 256 | 0 | 64.5 / 42.2 | 0 |
| hog 4 × 200 µs | hop | 2,549 / 2,543 | **779.5 / 780.5** | 2048 / 2048 | 347.3 / 348.2 | 427.7 / 426.9 | 4.5 / 5.4 |
| hog 4 × 200 µs | hop | 2,565 / 2,570 | **768.0 / 768.4** | 1024 / 1024 | 348.3 / 349.5 | 415.1 / 413.7 | 4.6 / 5.1 |
| hog 4 × 200 µs | inline | 4,866 / 4,909 | 40.6 / 47.9 | 128 / 128 | 0 | 40.6 / 47.9 | 0 |

Quiet: dispatch −10…−18 % (the two hops ≈ 8–10 µs of ≈ 50), verbs/s par
(inside the legs' spread). Under the hog — the fleet's saturated lanes at a
controlled burst — dispatch **768–781 → 40–65 µs (−92…−95 %)**, p99 1–2 ms →
128–512 µs, verbs/s **2.54–2.57 k → 4.77–4.91 k (+1.9×)**. The verbs/s ceiling
on the inline arm is the HARNESS's: the client routers' lane drains ride the
same hogged `sqz-meta` pool in this one-process rig (the client is hogged
too), so the owner-side split is the clean instrument and the throughput
delta is a floor. Engagement exact on every leg (1,024 dispatches; `inline`
or `hops` carries all of them; the two hops read 0.0 to the ns on inline).

**S9 publish plane — 24 concurrent layout publishes × 8 rounds from one
co-writer** (the D-1b rig; `dispatch` here is per served call/group):

| venue | arm | publishes/s | wall/round | dispatch total | queue_hop | run | wake_hop |
|---|---|---|---|---|---|---|---|
| quiet | inline | 40,533 / 46,889 | 592 / 512 µs | 298 / 250 µs | 0 | 298 / 250 | 0 |
| quiet | hop | 38,457 / 45,843 | 624 / 524 | 333 / 266 | 10.3 / 10.3 | 317 / 252 | 4.9 / 3.9 |
| quiet | hop | 39,878 / 40,797 | 602 / 588 | 328 / 425 | 6.4 / 4.8 | 317 / 416 | 4.5 / 4.2 |
| quiet | inline | 46,692 / 42,533 | 514 / 564 | 314 / 363 | 0 | 314 / 363 | 0 |
| hog 4 × 200 µs | inline | 15,548 / 14,142 | 1,544 / 1,697 | 446 / 523 | 0 | 446 / 523 | 0 |
| hog 4 × 200 µs | hop | 11,520 / 10,399 | 2,083 / 2,308 | **1,062 / 1,130** | 293 / 336 | 765 / 787 | 4.1 / 6.9 |
| hog 4 × 200 µs | hop | 11,645 / 10,581 | 2,061 / 2,268 | **1,001 / 1,062** | 288 / 290 | 709 / 764 | 4.1 / 8.5 |
| hog 4 × 200 µs | inline | 15,628 / 13,789 | 1,536 / 1,741 | 441 / 554 | 0 | 441 / 554 | 0 |

Quiet: par (frames per round vary 12–31 by natural batching). Under the
hog: dispatch **1.00–1.13 ms → 441–554 µs (−50 %)**, publishes/s **+33–35 %**,
wall/round −26 %. Every leg: 192 publishes landed, `frame_groups` grew
(D-1c intact), engagement exact.

**The isolation contract (gate, debug):** under four 2 ms bursts saturating
both lanes, 2 clients × 32 frames — hop `dispatch` 7,933 µs (= queue_hop
3,689 + run 4,224 + wake_hop 21), inline **195 µs** (< burst/4 by law).

## The accept tick (`a7334067`)

The accept loop slept `ACCEPT_POLL_TICK` (100 ms) between non-blocking
accepts. Measured RED: **every sequential dial paid the WHOLE tick** —
min 100.09 / median 100.17 / max 100.71 ms over 16 loopback dials — because
the loop re-polled right after an accept, found nothing, and slept, so the
next arrival always landed inside the sleep (the D-1b note's 98–100 ms
walls on every dialing row). The thread now parks in `poll(2)` on the
listening fd with the tick as the timeout (an arrival wakes it at once; the
tick keeps only its liveness role — the bound on `shutdown()` observing the
latch). Same row after: **128 µs / 201 µs / 840 µs** (min / median / max).
Contract `a_dial_never_waits_for_the_accept_tick` (median < 10 ms, p90 <
40 ms).

## The single-connection half (DLM #8) — landed

**Owner**: the authenticated session is a LANE on the connection's own
thread (`cluster_wire::SessionPark` — a `LaneExec` whose park is one
`poll(2)` over the socket and a wake eventfd): every ready frame is read
and its serve spawned as a lane task; the task writes its reply when its
verb completes; the idle bound is the park's timeout while nothing is in
flight; EOF / error / MAC failure / the shutdown nudge flip `closing` and
the lane exits when the last in-flight serve has replied (an `InflightGuard`
retires a serve on every exit path, an unwind included). A peer that
pipelines calls on one session is served CONCURRENTLY; a stop-and-wait peer
costs exactly what it did — one thread, the serve polled on it, no extra
hop (the inline venue above IS this lane). No wire change, no extra thread.
mTLS: rustls may hold a decrypted record beyond the frame just read, so
readiness is `socket readable ∨ buffered plaintext`
(`ClusterStream::has_buffered_plaintext`).

**Client**: `cluster_wire::MuxSession` — the same dial + proof, then the
stream split: a reader thread (`sqz-clw-mux`, holding the session WEAKLY
between reads so a dropped session ends it within one read tick)
demultiplexes replies by id onto parked oneshots; sends serialize under the
writer lock (the per-direction MAC sequence) on the blocking pool; the
per-call reply bound (`DIAL_TIMEOUT`) is enforced on the reader's read tick;
a transport failure on either half poisons the WHOLE session and fails every
parked call (the pooled "drop on error" law, one session wide), so the
publish plane's resend discipline is unchanged.

**The publish plane**: `SQUEEZEFS_PUBLISH_SHIP_MULTIPLEX` (default **on**)
rides the D-1b depth on ONE `MuxSession` per authority; `0` = the D-1b
session pool. Engagement `meta_ship_publish.ship_mux_frames` (≡
`ship_frames` on the default), and `ship_session_dials` reads 1 per
authority instead of `depth`.

**Measured**: 8 pipelined 40 ms calls on one session serve in **41 ms**
(`live_connections` 1, `requests_served` 8, every reply to its own caller);
WITHOUT `TCP_NODELAY` the same row read **123 ms** — the owner's back-to-back
replies waited for the client's delayed ACK (Nagle), 40 ms steps — so both
wire sockets now run `TCP_NODELAY` (one write per frame; nothing on this
wire benefited from Nagle). The publish contract: depth 4 with the owner's
pass held — 5 frames, `ship_depth_waits` 1, multiplexed **1 dial** vs the
pool's **4**, every publish lands, 24 journal entries, both arms.

**A/B at equal depth (in-process, release) — see the addendum below**: the
mechanism is landed on its contracts and its connection-count evidence; the
latency-at-equal-depth adjudication needs a quiet box (both rolls so far ran
under a foreign build at load 23–48, and the same-arm spread exceeded the
arm difference).

## Laws written into the code

1. **The dispatch's `run` is where a hogged lane's cost hides.** The split
   found the two hops at ≈ 350 + 5 µs of a 780 µs dispatch under the hog;
   the other ≈ 420 µs was the work's own wakes re-queueing behind the
   bursts. "Execute on the accepting venue" removes the lane from every
   wake of the dispatch, not just from its ends.
2. **The connection thread is the execution venue.** A multiplexed session
   therefore serves its K in-flight frames on ONE owner thread (interleaved
   at await points) where the pool served them on K threads. The CPU part
   of a publish serve (decode / compose / encode) is small beside its
   conveyor wait, so the concurrency that matters — the waits — is kept;
   the trade is real on a CPU-saturated authority and is why the lever
   keeps its control.
3. **A pipelined session needs `TCP_NODELAY`.** Request/reply never showed
   the interaction (one write, then a read); pipelining did, at 40 ms per
   reply.

## Not claimed

- **The fleet row** (squeeze-test, `meta_ship_owner_phase_ns.dispatch`
  mean/p99 + verbs/s + `meta_ship_owner_dispatch_ns.{queue_hop,run,wake_hop}`,
  A-B-B-A via `SQUEEZEFS_META_SHIP_INLINE_SERVE`; the publish rows via
  `SQUEEZEFS_PUBLISH_SHIP_MULTIPLEX`) is the parent's. Every number above is
  in-process on a loaded dev box: scoping evidence.
- The in-process verbs/s under the hog is a FLOOR (the client side shares
  the hogged pool in one process).
- The multiplex lever's latency-at-equal-depth verdict (addendum).
- `task check` (the full gate) — deferred by instruction; the targeted
  suites, fmt, both clippy configs and rustdoc ran (below).

## Suites (all `--test-threads=1`, all-features)

`owner_dispatch_hop_tests` 8 (+1 by name), `cluster_wire_tests` 25 (+1 by
name), `publish_plane_batching_tests` 8, `meta_ship_owner_dispatch_tests` 4,
`meta_ship_tests` 15, `dlm_membership_tests` 51, `membership_liveness_tests`
4, `membership_renewal_isolation_tests` 5, `dlm_cowriter_tests` 18,
`dlm_multi_writer_tests` 16, `dlm_range_custody_tests` 41, `mw_arm_s8_tests`
4, `mw_delegation_tests` 14, `mw_intent_batch_tests` 20,
`mw_publish_era_gate_tests` 5, `mw_recall_valve_tests` 12,
`mw_slot_placement_tests` 9, `mw_cowriter_free_tests` 50,
`pv_partial_arm_tests` 10, `pv_shipped_free_ledger_tests` 2,
`reader_free_grace_tests` 61, `job_wire_tests` 17, `job_wire_bounds_tests`
11, `env_knob_convention_tests` 21, `derivation_sweep_tests` 47.
