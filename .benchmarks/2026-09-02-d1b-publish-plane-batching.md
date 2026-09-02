# 2026-09-02 — D-1b: the S9 publish plane frames concurrent publishes and pipelines bounded depth (DLM #8)

**Branch** `perf/d1b-publish-plane-batching` (worktree off dev
`4c596ec2`). Measure `2faface1` → RED `189521d2` → fix `41e474f6` → rig
`ec921b87` → row labels `abc902a6`. Campaign: `docs/design-e2e-perf-audit.md`
§3 board **DLM #8** (stop-and-wait frame depth 1), opened by D-1's scoping
finding (`.benchmarks/2026-09-02-d1-owner-concurrent-dispatch.md` §Owed 2);
contracts `tests/publish_plane_batching_tests.rs`.

## The conviction (measured first, dev tip 4c596ec2)

D-1 collapsed the METADATA-verb frame (64 verbs: 64 conveyor passes → 3),
but the LAYOUT-PUBLISH plane the co-writer ingest wall rides is a different
wire. `src/meta_ship/publish.rs` at the tip: `PublishRequestFrame` carried
ONE `PublishCall`, and `PublishClient::ship` took the endpoint's session
mutex around a `RpcClient::call` (`cluster_wire.rs` `roundtrip`: write,
then block on the reply). Every publish was therefore a full stop-and-wait
round trip, and the owner saw one call per frame — F-A's owner-side
co-queuing (`dependency_chains`) had nothing to co-queue.

The in-process row that pinned it (two-node harness: one authority = custody
owner + publish service on a real `cluster_wire` listener over `127.0.0.1`,
one co-writer with a REAL custody join so every publish carries a live lease
epoch, 24 distinct inos = the streaming co-writer whose 24 per-block saves
arrive concurrently; instruments = the listener's `requests_served` delta
(wire frames), `META_CONVEYOR_LEADER_PASSES` delta (owner passes),
`META_KV_JOURNAL_ENTRIES` delta, spawn-to-join wall):

| dev tip `4c596ec2` | frames / publish | owner passes / publish | journal entries | wall / publish |
|---|---|---|---|---|
| debug ×6 | **1.00** (24) | **1.00** (24) | 24 | 291–378 µs |
| release ×6 | **1.00** (24) | **1.00** (24) | 24 | 104–173 µs (median 122) |

Arithmetic face of the S9-a wall: a co-writer publishing per 4 MiB block at
one stop-and-wait RTT + one owner pass each (~235 µs fabric RTT on the AWS
venue + ~1 ms saturated pass) caps near 3–4 GB/s per co-writer with no
device in the loop — the "≈ 2.6 GiB/s, writer-count-independent" number
the S9-a note published, read from the other side.

## The fix — framing + bounded depth on the client, chains on the owner

**Client** (`PublishClient`): S8's lane shape per authority endpoint
(`router.rs`'s `LaneDrain`, one layer over). Callers enqueue `{call,
oneshot}` on a bounded queue (`batch_max × 8`, S8's law) and park. ONE
drain per endpoint: it receives the head, acquires a **depth permit** (a
semaphore of `depth`), and only THEN takes everything queued into one
`PublishRequestFrame` — so the frame that goes out when a slot frees
carries everything that queued while the pipe was full. No timer, no
artificial delay: a serial stream still pays one RTT per publish (spec
§6.10 R1, accepted as D10 for the S8 plane; the same cost here). A frame is
cut at the call cap (`batch_max()`), at the CONTROL byte budget (a per-call
`wire_size_hint` upper bound — bincode varints ≤ 9 B — so a frame is never
encoded twice and a single over-cap call still ships alone and refuses at
encode as before), and **where the resend class changes**: a frame is
homogeneous in `transport_resend_safe`, so a transport failure re-sends the
SAME frame with the SAME request ids (the owner's `(lease_epoch,
request_id)` window absorbs it) or refuses every call (the un-witnessed
mutators' true sent-then-lost ambiguity). Each frame ships on its own task
holding its permit and a session popped from the lane's idle pool (≤
`depth` sessions by construction — only a permit holder ever dials one;
finding 14's `dead_on_arrival` screen before the send is kept).

**Depth** (`SQUEEZEFS_PUBLISH_SHIP_DEPTH`, derived `clamp(ceil(cpus/8), 2,
8)`): the wire is request/reply per connection on BOTH ends (the owner's
session loop reads a `Call`, serves it to completion, writes the `Reply`),
so "K frames in flight" = K sessions. The derivation is the owner's own
RPC-lane slope (`service_threads_from` = ceil(cpus/8) clamped [1, 8]): a
client's sessions toward one authority scale with the box the way the
authority's serving lanes do; floor 2 = the minimum at which a frame's RTT
overlaps a sibling's owner pass at all; ceiling 8 = the RPC-lane ceiling
(no session farm per authority on a 256-core client). `1` is the
stop-and-wait A/B control (frames still batch, nothing pipelines). Ties in
`tests/derivation_sweep_tests.rs`. Single-connection multiplexing (true
pipelining on one socket, reply demux by id) is the audit's **D-5** and
would also need the owner's session loop to read ahead — not this landing.

**Wire** (`PUBLISH_SCHEMA` 12 → **13**, KD-7 same-commit): `calls:
Vec<PublishCall>`; the reply answers `outcomes: Vec<PublishCallOutcome>` in
call order — `Done(Result<PublishReply, WireError>)` or `Refused { status,
detail }` — so the per-call statuses (`NOT_OWNER` / `STALE_LEASE` /
`LANE_REFUSED` / `PANIC` / executor-missing `MALFORMED`) moved from the
wire status into the body; the wire status now covers only frame-level
refusals (schema, undecodable, empty frame).

**Owner** (`PublishService::serve`): decode, screen the schema, partition
the frame by named-inode overlap (D-1's relation, now
`service::chains_by_named_inos` shared by both planes), run chains
concurrently (`join_all`) and same-ino calls serially in submission order,
re-slot outcomes into call order. Every per-call law lives in ONE
`serve_call`: the not-owner screen, the era gate (finding #6 — refused
BEFORE the witness window), the witness window for the layout/extent
classes, the lane/free validations, the unwind record (`PUBLISH_PANIC` per
call, cached in the slot so a replay answers the same failure). The
per-call sqz-meta hop (`spawn_meta_join`) is unchanged — the chain futures
only sequence and await those hops — so a one-call frame costs exactly what
it always cost. `SERVE_INO_LOCKS` (rung 17's per-ino serve stripe) is
untouched: two same-ino serves still never interleave; the chain discipline
adds ORDER within a frame.

What is preserved, and how:

* **Exactly-once under retries** — per call, unchanged: a resent frame
  carries the original request ids; the owner's window answers each from
  the winner's outcome (`replays += N`, journal unchanged — pinned).
* **Never-lossy refill (f38)** — the routing save's `Err` arm refills ITS
  ino's accumulator; the per-call outcome is what keeps a sibling's
  accounting from being refilled for a failure it did not have (pinned: 8
  siblings apply, the doomed call errors in its slot, entries += 8).
* **Era gates per call**; a current-epoch `STALE_LEASE` on ANY call in a
  frame composes the client's full fence (`note_publish_era_refused`) —
  the pull-based revocation law at the publish round trip, as before.
* **RETRIED vs REFUSED classes per call**; `recomputed` (f36/f36b) per
  call — the `PublishReply` shapes are byte-identical.
* **Same-ino order** — production frames cannot carry two same-ino layout
  publishes from one co-writer (every save holds its ino's 3.5 stripe
  across the publish, and delete/destroy order against in-flight saves is
  `delete_file`'s serialization point), but the owner keeps submission
  order for any same-ino pair in a frame anyway (pinned: hot@1000 then
  hot@2000 in one frame → 2000).
* **Backpressure** — the queue is bounded (`batch_max × 8` submissions;
  senders park), in-flight frames are bounded by `depth`, and a stuck owner
  surfaces as the wire's 10 s reply timeout on the frame (every call in it
  errors) — never unbounded queueing.
* **Solo mounts** — untouched: `owner_of` is one relaxed load and returns
  `None`; nothing here runs.

Ledger (`meta_ship_publish`): `ship_frames` / `ship_framed_calls` (calls ÷
frames = the live coalesce factor; ≈ 1 on a serial stream by design),
`ship_depth_waits` (the K+1'th frame parking — saturation at the current
depth), `ship_session_dials` (≤ depth per endpoint at steady state; growth
= sessions dying between frames), `served_frames` / `served_frame_calls` /
`served_chains` (chains ÷ frames ≈ frame width = independent calls; ≈ 1 =
one hot object). `shipped`/`served` keep their per-call meaning.

## Measured (in-process, two-node harness, 32-CPU dev box, file-backed KV sandbox, loopback wire)

24 concurrent layout publishes (`set_layout_and_size`, 24 distinct inos)
from one co-writer to one authority; pool warmed (no in-row session dials
on any quoted roll — a dial pays the cluster wire's 100 ms
`ACCEPT_POLL_TICK`, see Observations); ×6 per row.

| Binary / posture | frames / publish | owner passes / publish | entries | wall / publish |
|---|---|---|---|---|
| **debug** dev tip (stop-and-wait) | 1.00 (24) | 1.00 (24) | 24 | 291–378 µs |
| **debug** fix, derived depth (4 here) | 0.04–0.21 (1–5) | 0.08–0.21 (2–5) | 24 | 70–73 µs |
| **debug** fix, depth 1 | 0.08 (2) | 0.12–0.21 (3–5) | 24 | 83–96 µs |
| **release** dev tip (stop-and-wait) | 1.00 (24) | 1.00 (24) | 24 | 104–173 µs (median 122) |
| **release** fix, derived depth (4) | 0.04–0.21 (1–5) | 0.12–0.21 (3–5) | 24 | 13–38 µs (median 26) |
| **release** fix, depth 1 | 0.04–0.08 (1–2) | 0.08–0.21 (2–5) | 24 | 21–27 µs (median 24) |

Release wall per publish **≈ 4.7× at the median** (122 → 26 µs), frames per
publish **1.0 → 0.04–0.21**, owner passes per publish **1.0 → 0.08–0.21**;
journal entries stay 24 on every roll (one tx = one checksummed entry — the
count that never collapses). The natural (seam-free) frame count is 1–5 at
the derived depth because a busy pipe forms a frame per free slot as the
burst arrives; the seam-controlled contract (every arrival queued before
the drain takes the queue) is ONE frame and ≤ 4 passes. Depth 1 vs 4 is a
wash on this loopback venue (RTT ≈ 0, so there is no RTT to overlap) — the
depth lever's row is the fabric venue's.

**Contracts pinned** (`tests/publish_plane_batching_tests.rs`, 6): 24
queued publishes ride ≤ 4 frames and ≤ 4 owner passes with every caller's
own ino landing; the seam-free rows self-batch under ½ frame and ½ pass per
publish at both depths; a failed call isolates in ITS slot beside 8
applied siblings (entries += 8) and its bounded witnessed re-ships are the
only extra frames; a replayed frame is 8 witness hits with identical
outcomes and no journal movement; two same-ino calls in one frame keep
submission order beside independent siblings (current-thread runtime for a
deterministic enqueue order); depth 2 = two frames in flight on two
sessions, the K+1'th neither reaches the owner nor dials a third session
until a slot frees, then ships on the freed one (`ship_depth_waits` = 1).

Suites green on the fix (all `--test-threads=1`): the new suite (6),
`mw_cowriter_free_tests` (49), `mw_cowriter_lane_tests` (26),
`dlm_cowriter_tests` (18), `kvmap_mw_hazard_tests` (7),
`kvmap_crossing_tests` (9), `meta_ship_owner_dispatch_tests` (4),
`durable_block_refs_tests` (17), `mw_widthn_refs_tests` (15),
`f46_kvmap_stream_publish_tests` (2), `mw_publish_era_gate_tests` (5),
`mw_authority_assembler_tests` (21), `pv_shipped_free_ledger_tests` (2),
`meta_ship_tests` (15), `dlm_multi_writer_tests` (16),
`mw_ranged_lease_ladder_tests` (15), `pv_partial_arm_tests` (10),
`rebind_starvation_tests` (5), `mw_delegation_tests` (14),
`mw_intent_batch_tests` (20), `mw_colocated_fence_tests` (1),
`derivation_sweep_tests` (37), `env_knob_convention_tests` (21). `cargo
clippy --all-targets --all-features -- -D warnings` and the shipped-config
clippy clean, `cargo fmt --check` clean. `task check` deferred (batched by
the user — the box's own batch gate ran throughout this session).

## Observations

* **The cluster wire's accept loop polls at 100 ms** (`cluster_wire.rs`
  `ACCEPT_POLL_TICK` — a non-blocking listener slept between attempts so
  the accept thread observes the shutdown latch). Every session DIAL pays
  up to 100 ms of accept latency: the in-process rows showed 98–100 ms
  walls whenever a row dialed a session. For D-1b it is a one-time cost per
  (co-writer, authority, slot) — the pool keeps sessions warm — but it is
  the same tax on every custody/S8/publish reconnect after the 60 s idle
  reaper, and on the S6 renewal plane's first beats. A `poll(2)` on the
  listener fd with the tick as timeout removes the latency while keeping
  the latch observation. Boarded for **D-5** (the connection campaign).
* **Which fleet rows can see the lever**: the S9-a `s9-fanout` row is ONE
  dd stream per member, and a single ino's publishes serialize on its 3.5
  stripe (with the publish coalescer already folding blocks per save) — a
  frame there carries one call by construction, so frames/publish reads
  ≈ 1 whatever the binary. The lever engages on CONCURRENT publishes from
  one co-writer (many files, or the write pipeline's per-block saves of
  many inos), which is the field's 24-file streaming shape and what the
  new rig runs.

## Owed

1. **The fleet field row** — `.benchmarks/rigs/2026-09-02-d1b-fleet-row.sh`
   (committed `ec921b87`): 8 co-writers × 24 concurrent `dd bs=1M
   conv=fsync` streams × 128 MiB from `/dev/zero` (zram: the device term
   removed by design, exposing the publish-plane ceiling — labeled so),
   A-B-B-A over `A_BIN` = dev tip `4c596ec2` release and `B_BIN` = this
   branch release (both built: `/tmp/squeezefs.dev`, `/tmp/squeezefs.d1b`),
   one fleet at a time (`tests/mw_fleet.sh create N=1 --multi-writer
   --cowriters=8` with `SQZ_MWFLEET_OSS_GB=64`, row, teardown to zero
   residue per leg). Columns: aggregate co-writer ingest GiB/s,
   frames/publish (control reads 1.0 by construction), calls/frame, owner
   passes/publish, chains/frame, ledger closure, tripwires. **Not run**:
   the box's `task check` batch gate ran for the whole session (the
   measured-row law: the gate's CPU load invalidates a fleet row), and the
   rig refuses on a non-quiet box. Recipe, on a quiet box:
   ```
   A_BIN=/tmp/squeezefs.dev B_BIN=/tmp/squeezefs.d1b COWRITERS=8 STREAMS=24 MB=128 \
     sudo -n -E env "PATH=$PATH" bash .benchmarks/rigs/2026-09-02-d1b-fleet-row.sh
   ```
   plus the D-1 note's `s9-fanout` + `s8a` verb-storm rows on the same
   fleet for the single-stream posture (expect frames/publish ≈ 1 there —
   see Observations — and no regression).
2. **The depth lever on a fabric venue** (squeeze-test, ~25 µs RTT; AWS,
   ~235 µs): depth 1 vs derived, same rig — the loopback rows cannot rank
   them.
3. **D-5**: single-connection multiplexing (frames pipelined on one
   socket, reply demux by id, owner read-ahead) + the accept-tick fix
   above; F-B's connection cap is what makes depth × co-writers a
   capability term at scale (8 co-writers × 4 = 32 sessions per authority
   here; 340 co-writers × 8 exhausts the 1,024 cap).
4. `task check` (the full gate) — deferred by instruction.
