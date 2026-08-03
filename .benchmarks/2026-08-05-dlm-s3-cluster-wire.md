# DLM S3 — `cluster_wire`: the codec delta and the RTT row that prices S8

**Date:** 2026-08-05 · **Branch:** `feat/dlm-s3-cluster-wire` · **Base:** `dev` @ `a703ce3f`
**Scope:** spec [§6.7 *Transport*](../docs/pre-rc-engineering-spec.md) / §6.9 stage **S3**; execution plan [§6.3](../docs/pre-rc-execution-plan.md); rulings **D2** (open listener, auto-discovered peers ⇒ zero-config authn), **D8**, **D10** (R1 accepted).
**Box:** `strixhalo` (the thermally-capped dev box — **same-box relative** truth only, per the bench-baseline law). No cluster access this session: a live mount holds the venue.

Two questions this note answers with numbers:

1. **Was replacing the codec justified, and by how much?** (VAL-6 measured `serde_json`
   frame decode at **32.6 µs** for a 64-checksum mover shard against §6.5's **10 µs**
   custody budget.)
2. **What does one authenticated round trip on this wire cost?** — the single number that
   prices **S8** (function-shipped metadata) before S8 is designed, per §6.10 risk **R1**,
   accepted as ruling **D10**.

---

## 1. Codec: measured A/B, in one bench invocation

Instrument: `cargo bench --bench ipc_hop_bench -- job_wire_frame` (criterion, warm-up 1 s,
measurement 3 s, medians below). The `*_json_control` rows run the **retired** `serde_json`
codec over the **identical** frames in the same process, so the ratio is reproducible on any
box from one command instead of resting on a remembered number. Field-derived shapes,
unchanged from VAL-6's group: the enroll hello (~300 B — the only pre-authentication shape)
and a 64-`BlockChecksum` result proposal (the VL4 shard granularity; blocks move over shared
storage, only their checksums ride the wire).

| Row | S3 binary (bincode) | `serde_json` control | Delta |
|---|---|---|---|
| `decode_result_submit_64` | **771 ns** | 16.64 µs | **21.6× faster** |
| `encode_result_submit_64` | **627 ns** | 3.87 µs | **6.2× faster** |
| `decode_enroll` | **113 ns** | 269.5 ns | **2.4× faster** |
| frame **bytes**, 64-checksum shard | **1,542 B** | 4,043 B | **2.62× smaller** |

The custody-budget verdict: the row that motivated the change lands at **0.77 µs against a
10 µs budget** — 7.7 % of it, where JSON was 166 % of it on this box (and 326 % on VAL-6's
recorded 32.6 µs row). The enroll shape matters for a different reason: it is what an
*unauthenticated* peer makes the coordinator spend, and it got cheaper too.

Hostile shapes (the DoS unit cost per connection, VAL-6's bounds carried forward, not
re-derived):

| Row | Cost |
|---|---|
| `refuse_oversize_prefix` (4 bytes claiming past the cap) | **64 ns** — a comparison, never an allocation |
| `refuse_lying_prefix_16mib` (bulk-cap claim, 4 KiB body) | **454 ns**, chunk-bounded (VAL-6 recorded 813 ns for the same shape pre-port) |

### What S3 *added*, priced honestly

Authentication no longer stops at enrollment: every post-enrollment frame carries a session
MAC (HMAC-SHA256 over `direction ‖ sequence ‖ length ‖ body`).

| Row | Cost |
|---|---|
`mac_roundtrip_result_submit_64` (encode → MAC → verify → decode, both directions) | **2.97 µs** |

Decomposition: 0.63 µs encode + 0.77 µs decode ⇒ **≈ 1.6 µs for the two HMAC passes** over a
1.5 KiB body plus framer setup. So the whole custody frame — encoded, authenticated,
verified, decoded — costs **≈ 3 µs where the old JSON decode alone cost 16.6–32.6 µs**. We
bought per-frame authentication *and* came in at 30 % of the budget. On the small shapes
(handshake, S4 verbs) the MAC term is proportionally smaller still, since HMAC cost tracks
body length.

---

## 2. The RTT row — loopback FLOOR, and what the fabric row needs

Instrument (shipped, in-tree — this is the thing to point at a real fabric):

```bash
# loopback floor
cargo test --release --test cluster_wire_tests -- --ignored --nocapture rtt_row

# a REAL peer, same instrument, authenticating exactly like any other peer
SQZ_CLW_RTT_ENDPOINT=<host:port> SQZ_CLW_RTT_SECRET_HEX=<job:enroll secret> \
SQZ_CLW_RTT_SAMPLES=5000 SQZ_CLW_RTT_PAYLOAD=0 \
  cargo test --release --test cluster_wire_tests -- --ignored --nocapture rtt_row
```

Measured (release profile, `PingService` server on one pinned service lane, qd1 — one
outstanding request, which is the shape a serial metadata stream has; one warm-up sample
discarded and separately accounted; `requests_served` closes exactly against samples + 1 on
every run, so no row is a local loop):

| Run | Samples | Payload | min | **median** | p99 | max |
|---|---|---|---|---|---|---|
| A | 2,000 | 0 B | 8.44 µs | **9.33 µs** | 14.64 µs | 197 µs |
| B | 5,000 | 0 B | 9.61 µs | **12.58 µs** | 16.83 µs | 64 µs |
| C | 5,000 | 4 KiB | 25.05 µs | **31.94 µs** | 60.63 µs | 370 µs |
| criterion `cluster_wire_rtt/authenticated_ping_qd1` | — | 0 B | — | **11.05 µs** | — | — |
| criterion `cluster_wire_rtt/authenticated_ping_4k` | — | 4 KiB | — | **30.32 µs** | — | — |

**The loopback floor is 9–13 µs at qd1** (the A/B spread is honest run-to-run variance on a
thermally-capped box; cite the range, not one figure). That is the *transport's own* term:
framing + MAC + two wakes + scheduler, with the fabric term at ≈ 0.

### Reading it against §6.5 item 1 (this is the S8 input)

§6.5's arithmetic: an uncontended acquire sits inside a 64 µs under-lock span that 86.9 % of
creates already fit into, and adding one **250 µs** fabric RTT takes the create wall from
110 µs to 360 µs — **9,090 → 2,778 creates/s, a 69 % regression**. Substituting the measured
floor into the same formula:

| Added RTT | Create wall | Creates/s | vs 9,090/s |
|---|---|---|---|
| 0 (today, solo) | 110 µs | 9,090 | — |
| **11 µs** (this floor — same-host / shared-memory-class peer) | 121 µs | 8,264 | **−9 %** |
| 50 µs (a good datacentre fabric) | 160 µs | 6,250 | −31 % |
| 150 µs | 260 µs | 3,846 | −58 % |
| 250 µs (§6.5's figure) | 360 µs | 2,778 | −69 % |

So the wire's own overhead is **not** what decides S8 — the fabric is. At the floor, function
shipping costs ~9 %; every µs of fabric latency past that is the product cost, and R1's
"honest product statement" fallback (*remote clients are throughput-oriented; latency-sensitive
metadata work runs on the owner*) becomes the right answer somewhere between the 50 µs and
150 µs rows. **This is exactly the number D10 accepted the risk against, and it now exists.**

### What the REAL row needs (do not fabricate it)

A fabric row is owed and is **not** produced here. Its requirements, stated so the next
session can run it unattended:

1. **Venue: two real hosts on the target fabric** — the coordinator on one, the probe on the
   other, over the production NIC. Loopback and same-host containers are floors by
   construction. The wire is TCP/TLS (deliberately not io_uring), so the venue's softirq and
   interrupt-steering posture is part of the measurement; record `ethtool -c`, the NIC model,
   MTU, and whether the two hosts share a switch or cross a spine.
2. **Substrate discipline** (the two-substrate rule, adapted honestly): the rule exists
   because nvmet-**loop** hides network-stack effects. For this wire the network *is* the
   substrate, so the row must run on the real fabric; a `SQZ_DEVSUB_TRANSPORT=tcp` localhost
   substrate is a plumbing check, not a row. Say which one produced the number.
3. **Both channel classes**: plaintext (storage-trust authn + session MAC) and CA-pinned
   mTLS — the mTLS row carries the TLS record and handshake-exporter cost, and S8's
   deployment posture depends on which one it pays.
4. **Medians of 3, and A-B-B-A when anything ages across the runs** (e.g. before/after
   binaries against one coordinator, or a store whose free-list shape changes). A
   single-order delta on an aging store is an ordering artifact until the reversed bracket
   reproduces it.
5. **A sustained ≥ 60 s row** alongside the burst rows, flat across the window (no decay
   beyond noise between first and last third) — the standing sustained-state rule. A burst
   RTT that degrades under sustained offered load is a FAILED row, not a result.
6. **Engagement, exact**: `RpcStats::requests_served` must equal samples + 1 (the discarded
   warm-up) and `mac_failures` must be 0. A row whose counters do not close is INVALID.
7. **Depth beyond qd1**: qd1 is the serial-stream shape R1 cares about, but S8 will pipeline;
   record a batched/pipelined row too so S10's delegation recovery has a baseline to beat.

Evidence tier for the numbers above: **measured-real** for the loopback floor and the codec
rows on this box; the fabric row is **owed** and must not be interpolated from the floor.

---

## 3. Bounds and behavior (not perf, but part of the same landing)

Carried onto the transport rather than re-derived per protocol: three frame classes with
their own caps and deadlines (handshake 8 KiB / control 1 MiB / bulk 16 MiB), chunk-bounded
body commit (64 KiB), per-class body deadlines, an RAII connection cap claimed **before any
task exists**, the exponential accept-backoff ladder, and — structurally better than
pruning — no per-connection handle retention on the RPC listener at all.

The authn ladder exists once (`AuthnGate::verify`): schema → identity bound → constant-time
proof compare → single-use/freshness nonce consume → session-key derivation. The order is
load-bearing: consuming the nonce *after* the MAC check means a peer that cannot produce a
valid proof can never spend the challenge of an honest connection it raced.

`job_wire`'s 27 VAL-6 tests pass on their original assertions, with exactly one deliberate
change of outcome: a CA-less `ClusterSecurityConfig` now **refuses the listener** instead of
being admitted as a "TLS-unauthenticated" class, because the accept-everything certificate
verifier it described has been deleted from the tree.
