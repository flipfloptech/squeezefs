# PORTING SPEC — `sym_wire_membership_recovery` (the cluster wire, sharded membership + PARK-AND-RECLAIM, dead-appender recovery, WERO fencing + the frame screen, the join ladder / N-daemon posture, the flush-ceiling audit)

Charter: §5.5 "Membership is sharded by home volume" / "the death ledger" / "A manager's death is its home
shard's outage" / "Death and recovery (§5.9)" / "Fencing" / "Flush ceiling (KD-SYM-10)" / C13–C17 / the
Gauges paragraph; §5.7 (the D0 winner is the manager; a later RW mount JOINS over the wire; a clean umount +
remount is a same-node successor); §3 host-scoped fabric subsystems; §8 (`appenders`, `appender clear`); §9
(thread-per-connection for wires, `sqz-lease-io`); §1.6/§1.7 (one knob convention; unknown wire schema
refuses). Rung 6 of §13 — the "dead-appender recovery → join ladder → N-daemon posture" rungs.

Reference files (`/Users/joberdorf/Source/squeezefs`): `CW:` `src/cluster_wire.rs` (3,760), `M:`
`src/membership.rs` (4,150), `MW:` `src/membership_wire.rs` (957), `MI:` `src/member_id.rs` (50), `LC:`
`src/lease_clock_core.rs` (344, ported), `MGR:` `src/meta_ship/manager.rs` (2,163), `W:`
`src/meta_ship/wire.rs` (1,303), `E:` `src/error.rs`, `R:` `src/meta_backend/reservation.rs` (1,576),
`REC:` `src/meta_backend/kv/backend/recovery.rs` (3,657), `JN:` `src/meta_backend/kv/backend/joined.rs`
(3,694), `AL:` `src/meta_backend/kv/alloc_lease.rs`, `AP:` `src/meta_backend/kv/appender.rs` (3,484),
`SS:` `src/meta_backend/kv/slot_state.rs`, `CK:` `src/meta_backend/kv/checkpoint.rs`, `B:`
`src/meta_backend/kv/backend.rs`, `PG:` `src/park_gate.rs` (441), `PC:` `src/park_core.rs` (258), `SJ:`
`src/sym_join.rs` (821), `DC:` `src/data_custody.rs`, `DG:` `src/data_grant.rs`, `JW:` `src/job_wire.rs`,
`EK:` `src/env_knobs.rs`, `SIM:` `src/membership_sim.rs` (867), `D:` `docs/design-symmetric-metadata.md`.
Tests: `T-CW:` `tests/cluster_wire_tests.rs`, `T-CM:` `tests/sym_crash_matrix_tests.rs`, `T-ND:`
`tests/sym_n_daemon_tests.rs`, `T-F:` `tests/sym_fence_tests.rs`, `T-MG:` `tests/sym_manager_tests.rs`,
`T-BG:` `tests/sym_block_grant_tests.rs`, `T-DM:` `tests/dlm_membership_tests.rs`, `T-CO:`
`tests/sym_coherence_tests.rs`, `T-AP:` `tests/sym_appender_tests.rs`, `T-MP:`
`tests/sym_mount_posture_tests.rs`, `T-ML:` `tests/membership_liveness_tests.rs`, `T-WR:`
`tests/wero_rtype_tests.rs`, `T-CF:` `tests/mw_colocated_fence_tests.rs`.

Port files read (not modified): `Z-bset:` `src/meta/bset.zig` (`FrameScreen` L587-631), `Z-node:`
`src/meta/node.zig` (`verifyNodeExtentScreened` L625-712), `Z-claim:` `src/meta/guard/claim.zig`
(`WriterClaimV1` L67-107, `Registration`/`RegistrationSource` L347-388), `Z-lc:`
`src/meta/guard/lease_clock.zig`, `Z-pr:` `src/meta/guard/pr.zig` (`CapLearner` L70-95, `preemptAndAbort`
L279), `Z-nvme:` `src/meta/guard/nvme_cmd.zig` (`RTYPE_WERO = 3` L128), `Z-err:` `src/core/errors.zig`,
`src/exec/*` (lanes, fibers, `fdwatch.zig`), `src/sync/*` (`notify`, `completion`, `mpmc`, `tick`).

## 0. Rung map and the one law this spec serves

| Mechanism | Where in the reference | Port home (proposed) | Lands |
|---|---|---|---|
| THE cluster wire (framing, authn, listener, dialer, mux) | CW | `src/wire/{frame,authn,listener,dialer,mux}.zig` | rung 6 wave 1 (every later wave dials it) |
| Manager verb block + statuses + retryable classes | MGR, W, E | `src/wire/verbs.zig`, `src/meta/sym/manager_service.zig` | wave 1 (with the manager-lease spec's executors) |
| Membership shards, records, eviction, death sinks | M, MW, MI | `src/meta/sym/membership.zig`, `member_id.zig` | wave 1 |
| PARK-AND-RECLAIM (gate + core + model) | PG, PC, M:3587-3808 | `src/meta/sym/park_gate.zig`, `park_core.zig`, `park_core_model.zig` | wave 1 |
| Death ledger records + recovery driver | AL, REC | `src/meta/sym/death_ledger.zig`, `recovery.zig` | wave 2 |
| WERO manager/registrant split, cap gate, frame screen | R, DC, Z-bset, Z-node | `guard/pr.zig` (landed primitive), `bset.FrameScreen` (one field added) | wave 2 |
| Join ladder, joined door, N-daemon posture, same-node successor | SJ, JN, AL:3520-3647 | `src/meta/sym/join_ladder.zig`, `joined.zig` | wave 3 |
| Flush-ceiling audit (appender side) | AP:2433-2467, 2797-2830; CK:1038-1099 | `checkpoint_cycle.zig` (the port already derives the ceiling) | wave 2 |
| `appenders`, `appender clear` | REC:3084-3375 | `cli/appenders.zig` | wave 3 |

**One law.** Every verb is idempotent against DURABLE state, every wire word is SCREENED against durable or
derived state before any effect ("bounded codec = bounded execution", MGR:850-860), and every refusal is
TYPED: a witness refusal (`STATUS_REFUSED`, must-stay-0), a wire-invalid word (`STATUS_REJECTED`, the
buggy/hostile-peer class, off the must-stay-0 gauge), or "not now" (`STATUS_DEFERRED` and the seven
`RefusalClass` words → `EAGAIN`, never `EIO`/`EINVAL`) (MGR:70-89; E:8-57, 360).

---

## 1. THE CLUSTER WIRE

### 1.1 Transport

- **TCP, blocking sockets, `TCP_NODELAY` on BOTH ends** (CW:1126-1133 dial side; CW:2347-2352 accept side):
  one `write_all` per frame; a pipelined session's back-to-back frames must not wait for the peer's
  delayed ACK — measured **40 ms steps per reply** without it (CW:1131).
- **Deliberately not io_uring** (CW:100-102): "TLS peers and network TCP/TLS stacks are the sanctioned
  non-uring exception" — charter §1.2 says the same.
- **Channel classes** (CW:987-1008): `Plaintext` (storage-trust authn makes it *authenticated*: identity +
  per-frame integrity; not confidential) and `MutualTls` (CA-pinned, authenticated AND confidential). A
  CA-less TLS config is REFUSED, never downgraded to an accept-everything verifier (CW:63-71, 1054-1078,
  1146-1156). The verification-strength ladder keys on `SessionAuthn.authenticated()` (proof verified ∧
  MAC engaged), never on "transport == tls" (CW:1010-1051); verify-read sampling below 100 % is admissible
  only on authenticated ∧ confidential (CW:1032-1041) — the job fabric's law (rung 8).
- **Port scope for rung 6:** plaintext + storage-trust authn + per-frame MAC (every symmetric verb's class).
  mTLS is the job fabric's need (rung 8) and Zig 0.17's `std.crypto.tls` ships a client only; the
  `ChannelClass` enum reserves `.mutual_tls` and the exporter-binding slot in the session key (§1.3), so
  rung 8 adds it without a wire change. Charter §9: **thread-per-connection** — the reference's venue
  since the rip-tokio conversion (CW:76-85).

### 1.2 Framing — the reference's bytes, and the port's v4 frame

**Reference** (CW:13-34, 280-321, 370-433, 556-607): `u32 BE length ‖ body ‖ [MAC 32 B]` where `body` is
**bincode** (varint integers, little-endian, trailing bytes refused, decode limit = delivered bytes —
"a length in a frame is a claim, never an allocation authority", CW:286-321) of the `RpcFrame` enum
(CW:1399-1451): `Challenge{schema u32, server_nonce String, freshness_ms u64}`, `Prove{schema, peer_id
String, server_nonce, peer_nonce, mac String(hex)}`, `Admitted{schema}`, `Refused{reason String}`,
`Call{id u64, verb u16, body Vec<u8>}`, `Reply{id, status u16, body}`. Three classes with caps
(CW:206-220): Handshake 8 KiB (what an UNAUTHENTICATED peer may cost), Control 1 MiB, Bulk 16 MiB; bodies
stream in `FRAME_CHUNK_BYTES = 64 KiB` rounds so a lying prefix costs one chunk (CW:222-224, 334-363);
the body wait is deadline-bounded, the length-prefix wait is NOT (an idle authenticated session
legitimately waits between frames; its bound is the session idle timeout, CW:365-369). Measured WHY
binary: a `serde_json` frame decode cost **32.6 µs** against the 10 µs custody budget (CW:16-21).

**Port v4 (decision 0002 row 2's class: a serialization-library artifact is not a wire):** one fixed
48 B little-endian header, payload, optional 32 B MAC trailer:

```
pub const FrameHeaderWire = extern struct {            // 48 B, LE, comptime size/offset-asserted
    magic: [4]u8,            // "SQZW" in disk order (0002 row 38)
    version: u16,            // FRAMING version = 1 (changes only with a transport change, CW:163-168)
    flags: u16,              // bit0 MAC_PRESENT (0 on the four handshake frames, 1 on every session frame)
                             // bit1 DIR (0 = coordinator→peer, 1 = peer→coordinator; CW:493-494's tag)
                             // bit2 REPLY (0 call, 1 reply); bits 3..15 reserved = 0 (checked)
    len: u32,                // payload bytes; ≤ the verb block's class cap (§1.6), checked BEFORE any read
    verb: u16,               // block-coded verb number (§1.6); 0xFFF0..0xFFF3 = Challenge/Prove/Admitted/Refused
    status: u16,             // 0 on a call; the ONE status table's word on a reply (§1.7)
    correlation: u64,        // caller-chosen, echoed verbatim (CW:1370-1371, 3011-3016)
    seq: u64,                // per-direction session sequence 0,1,2,… (CW:496-511); 0 on handshake frames
    schema: u32,             // the VOCABULARY schema of `verb`'s block (CW:159-204 two-level versioning)
    reserved: u32,           // = 0 (checked)
    xxh3: u64,               // xxh3-64 over header[0..40]: a garbage or misframed header is refused
                             // before the payload is read or the HMAC computed (core/checksum.zig)
};
// frame = header ‖ payload[len] ‖ (flags.MAC_PRESENT ? HMAC-SHA256(session_key, header[0..48] ‖ payload) : ∅)
```

The MAC covers `dir`, `seq`, `len`, `verb`, `status`, `correlation` and the payload because they are IN
the header it covers — the reference MACs `dir ‖ seq ‖ len ‖ body` (CW:533-543) and the same tamper /
reorder / replay / reflection refusals follow (CW:584-607; T-CW:538). Receive discipline: read 48 B →
verify `xxh3` → check `magic/version/reserved` → check `len ≤ cap(verb block)` → stream the payload in
64 KiB rounds under the body deadline → read the MAC → verify under the expected `(dir, seq)` → a
verification failure is a SESSION error, never a skipped frame (CW:584-586, 2743-2769). `std.crypto.auth.hmac.sha2.HmacSha256`
is the primitive; the constant-time compare is `std.crypto.timing_safe.eql` (CW:545-554's `tag_eq`).

### 1.3 Authentication and identity

**Root of trust = storage** (CW:36-43; charter §8 "HMAC enrollment against a `job:enroll` secret"): whoever
can read the set's `job:enroll` record is inside the trust domain. The record (JW:1306-1330) is a 32-byte
secret minted by the FIRST coordinator (`None ⇒ mint`), a present-but-undecodable record REFUSES loud
(never overwritten; the rotation lever is removing it on a quiesced set), an I/O error PROPAGATES (a
re-mint on a transient read brings the `mac invalid` class back). Port v4: `JobEnrollV1 = extern struct
{magic "SQZE", version u16, reserved u16, secret [32]u8, xxh3 u64}` on volume 0's control tree
(`set_config`'s neighbour, 0002 row 121), drawn from `Io.randomSecure` (0002 row 119's law).

**The ladder, ONE implementation** (`AuthnGate::verify`, CW:853-975; order load-bearing CW:917-923):

1. Coordinator speaks FIRST: `Challenge{server_nonce, freshness_ms}` — a SERVER-issued single-use nonce
   inside a freshness window, so a captured hello is not a credential (CW:46-50, 895-904).
2. Peer answers `Prove{peer_id, server_nonce, peer_nonce, mac}` with `proof_mac = HMAC-SHA256(secret,
   peer_id ‖ server_nonce ‖ peer_nonce ‖ "hello")` (CW:618-630).
3. Verify order: schema (refuse "wire_schema N not supported … refusing rather than interpreting a
   foreign schema") → `peer_id` length cap (256 B, CW:727-730) → constant-time MAC compare → ONLY THEN
   consume the nonce (`Fresh` / `Expired` / `UnknownOrReplayed`; CW:823-833) — a peer that cannot
   produce a valid MAC never spends an honest connection's nonce (CW:919-923).
4. Both sides derive `session_key = HMAC(secret, "squeezefs-cluster-wire/v1/session" ‖ peer_id ‖ 0 ‖
   server_nonce ‖ 0 ‖ peer_nonce ‖ 0 ‖ binding)` (CW:451-480), `binding` = the RFC 5705 exporter
   (`"squeezefs-cluster-wire/v1/binding"`, 32 B) on TLS, empty on plaintext (CW:2934-2951).
5. `Admitted{schema}` → every later frame carries the per-frame MAC (CW:2505-2522).

Nonce registry: bounded by `nonce_cap = max(4 × max_connections, 64)` (CW:2197), pruned by age and cap
(CW:834-851); freshness 30 s (CW:741, 1557).

**Identity presentation.** The `peer_id` of a production appender IS its durable member id
`node_{16 hex}[.m{8 hex}]` = `(node_token, mount_slot)` (MI:1-30; MGR:829-842): the identity-carrying
manager verbs (`PublishEndpoint`, `LeaveAppender` strictly; `JoinAppender` when the peer is a member id)
REJECT a frame whose `identity` derives another member id — "an appender speaks for itself alone"
(`screen_identity_peer`, MGR:1383-1403, `manager_verb_rejected`). The appender identity on the wire is
`WireIdentity{node_token u64, mount_slot u32, writer_id u128}` (MGR:93-97); the node token comes from
`SQUEEZEFS_NODE_ID_FILE → /etc/machine-id → /var/lib/dbus/machine-id → /etc/squeezefs/node-id`, refusing
loud rather than synthesizing (guard_claims.md §3.4; EK:426). The `(node_token, mount_slot)` pair is the
death key's identity; `writer_id` changes per mount and never binds (REC:563-567).

Port v4: `WireIdentity = extern struct {node_token u64, mount_slot u32, reserved u32 = 0, writer_id u64}`
(24 B) — the port's `writer_id` is the 64-bit incarnation hash (0002 row 185; Z-claim:72-77); `peer_id`
on the wire is the 12 B `node_token BE ‖ mount_slot BE` (the text form is a log rendering); nonces are
16 random bytes, not uuid strings (CW:817, 3226 — a quirk, §9). The HMAC labels keep the reference's
strings byte-for-byte so a future interop oracle is cheap; nothing else is owed (0002 §2).

### 1.4 The listener

- **Accept thread** `sqz-clw-accept` waits ON the non-blocking listening socket with `poll(2)` and a
  100 ms tick that bounds only how long `shutdown()` can wait for the latch (`ACCEPT_POLL_TICK`,
  CW:1162-1172, 2297-2314) — before D-5 the thread SLEPT the tick and every sequential dial paid
  100.09–100.71 ms (CW:1167-1170). Accept errors ride a doubling backoff 5 ms → 1 s (CW:229-245; a bare
  `continue` turned EMFILE into a busy loop).
- **Connection cap, claimed BEFORE anything is spawned** (`ConnGate.try_admit`, CW:1763-1829, 2327-2342):
  an over-cap peer costs one accept + one close. `max_connections = clamp(raw_cpus × 16, 64, nofile /
  (2 × 4))` (CW:1632-1637): the root is the RAW affinity mask, never the fleet-share-divided one — "a
  listener's load is the FLEET's width × each member's session demand, which N co-located daemons each
  serve WHOLE; the divisor shrank the cap exactly as the fleet grew — 64 on a 32-member fleet on 32
  cores, the 14th member refused at accept" (CW:1566-1572). `CONNECTION_FDS = 2` (socket + its
  shutdown-nudge dup), `LISTENER_FD_SHARE = 4` (a quarter of `RLIMIT_NOFILE`, the `fd_cache_cap` law),
  floor 64 (the shipped posture), `CONNS_PER_CORE = 16` "the shipped factor kept — nothing has measured
  the thread-per-connection wake storm at another factor" (CW:1606-1626). The soft `RLIMIT_NOFILE` is
  raised to the hard limit once per process, the as-found value recorded FIRST (CW:1691-1736).
  `SQUEEZEFS_CLUSTER_WIRE_MAX_CONNS` (int 64..=65536) wins verbatim, railed by the fd ceiling only, the
  rail ANNOUNCED (CW:1573-1600; EK:385).
- **Per-connection thread** `sqz-clw-conn` (CW:2360-2383): handshake under socket read/write timeouts =
  `handshake_timeout = DIAL_TIMEOUT = 10 s` (CW:2400-2410, 1548-1558 defaults: `frame_body_timeout 30 s`,
  `session_idle_timeout 60 s`, `enroll_freshness 30 s`); then **the connection IS a lane**
  (`SessionPark`, CW:2586-2607): its park `poll(2)`s the socket and a wake eventfd through ONE wait, the
  `service` hook reads every READY frame and spawns its serve as a lane task, replies are written under
  the session's I/O lock when each verb completes — a PIPELINING peer is served concurrently (K frames in
  flight cost one connection), a stop-and-wait peer costs exactly one thread with the serve polled on it
  (CW:2593-2599). Liveness: idle bound = the park's timeout while nothing is in flight; EOF / read error /
  MAC failure / the listener's `shutdown(Both)` nudge on the dup'd socket flip `closing`; the lane exits
  when the last in-flight serve has replied (CW:2600-2606, 2622-2639). Owner-side RPC never runs on the
  conveyor's task (CW:73-76 — the conveyor is a serialized ~0.78 ms server at ρ ≈ 0.92).
- **Counters** (`RpcStats`, CW:1737-1761): `live_connections`, `connections_refused`, `accept_backoffs`,
  `sessions_admitted`, `admissions_refused`, `requests_served`, `mac_failures` (must-stay-0),
  `service_refusals` (must-stay-0), `outstanding_challenges`; the membership sweep logs the two
  must-stay-0s on its cadence (M:3233-3246).
- **Endpoint publication** (`advertised_endpoint`, CW:1313-1332 — THE endpoint law): an explicit bind IP is
  advertised VERBATIM, an unspecified bind (`auto` = `0.0.0.0:0`) advertises the default route's IP
  (a UDP connect to TEST-NET-1 for the route lookup, loopback fallback; CW:1334-1341) with the bound
  port either way. Under the symmetric plane the joiner's listener is written by the MANAGER into the
  joiner's claim-set entry on every volume it appends to (`PublishEndpoint`, MGR:297-311) and read by
  `resolve_holder_endpoint(page, claim_set)` (SJ:543-557, 614-624): page identity → member id → that
  member's `endpoint`. Charter §5.5: "endpoints published into each writer's claim-set entry".

**Port shape.** `wire/listener.zig`: one `std.Thread` `sqz-wire-acc` (12 chars) on `accept(2)` + the
`sync` stop latch; one `exec.Lane{.class = .wire}` thread `sqz-wire-conn` per admitted connection whose
park is the lane's own ring (`exec/fdwatch.zig` `POLL_ADD` one-shot on the socket + the lane's inbox —
decision 0003 §D Q-B: a parked lane is woken only by its ring), the frame reader as the lane's service
hook, each verb a lane fiber (an async executor — a manager verb commits through the conveyor and parks
as a fiber, never blocks the thread). `ConnGate` = a `counters.Gauge` + CAS admit with an RAII permit.
The two thread names are new charter §9 rows (the 15-char comm budget holds; owner's amendment as Q6).

### 1.5 The dialer, reconnect, `ListenerRefused`, the mux

- `dial_tcp(endpoint, DIAL_TIMEOUT)` resolves and `connect_timeout`s each address in order (CW:1116-1144).
  `RpcClient::connect_once` (CW:3160-3279): socket timeouts = `DIAL_TIMEOUT`; read `Challenge` (schema
  mismatch ⇒ refuse naming both), send `Prove`, read `Admitted`/`Refused`, derive the key, framers as
  `Role::Peer`. **EOF before any frame = the listener accepted and closed = its connection cap or a
  shutdown** ⇒ `ConnectRefusal::BeforeChallenge` (CW:3216-3218).
- **The bounded retry** (`connect_sync`, CW:3113-3158): repeat the dial on `BeforeChallenge` with a
  doubling backoff from `ACCEPT_POLL_TICK` (the finest grain at which a freed slot can be observed) until
  `DIAL_TIMEOUT` is spent — `DIAL_TIMEOUT` is ALSO the listener's handshake deadline, the lifetime of the
  shortest slot-holding session, so a dialer that retried one bound has outlived every pre-authn
  straggler (CW:3052-3058, 3120-3127); past it `RefusalClass::ListenerRefused` (EAGAIN), never the
  PR-13b `InvalidOperation` at the first EOF ("the 14th member of the box's 32-member fleet died on that
  word", CW:3125-3127). Pinned T-CW:917, 970.
- `is_transport_failure(e)` = `Io` or a `cluster wire:` prefixed message (CW:2956-2970) — the resend
  discipline of a verb client: a transport failure RE-DIALS (the joined wire's `redial` flag,
  JN:165-168), a peer's refusal does not.
- `dead_on_arrival()` (CW:3286-3329): a non-blocking `MSG_PEEK` before a ONE-ATTEMPT verb's single send —
  EOF queued (the 60 s idle reaper closed it) or unsolicited bytes (desynchronized) ⇒ replace the session
  before the send; `EAGAIN` ⇒ alive; any other errno ⇒ broken. "The no-retry law binds a frame once SENT —
  the true sent-then-lost ambiguity keeps refusing."
- `call_timed_on(verb, body, lane)` stamps `sent`/`replied` on the venue's thread; `lane = Some(sqz-lease-io)`
  is the liveness-class venue the membership renewal rides (CW:3337-3371; MW:40-59, 112-116).
- **`MuxSession`** (CW:3386-3633): one dialed session split into a write half under a mutex (the MAC
  sequence is per direction) and a dedicated reader thread `sqz-clw-mux` demultiplexing replies by
  `correlation` onto parked oneshots; per-call reply bound `DIAL_TIMEOUT` enforced on the reader's
  `DIAL_TIMEOUT / 4` tick (CW:3437-3442, 3474-3492); a transport failure on either half POISONS the whole
  session and fails every parked call (CW:3503-3518); a dropped session poisons itself and the reader exits
  on its weak upgrade (CW:3444-3452, 3590-3596). K in flight costs ONE connection (DLM #8).
- `MEMBER_CONTROL_SESSIONS = 6` standing sessions per joined writer, COUNTED IN CODE by a marker comment
  and pinned by a census test (CW:1670-1683): the manager wire, the meta-ship lane's stop-and-wait + mux,
  the per-holder custody client, the intents lane, the block-grant sink. `member_session_demand_from(cpus,
  volumes) = volumes × (depth + 1) + depth + 6` (CW:1655-1668) is what the cap is judged against.

Port: `wire/dialer.zig` (`Client` with `connect(io, …)`, `call(io, …)`, the bounded retry on
`error.ListenerRefused`), `wire/mux.zig` (reader = one `std.Thread` `sqz-wire-mux`, pending table a fixed
`sync.Completion` cell pool sized at dial, poison = one `atomic bool` + `shutdown(SHUT_RDWR)` nudge).

### 1.6 Verb families and their envelopes

**Reference numbering** — blocks, so parallel stages never collide (MW:14-21): `VERB_PING = 0`
(CW:1360); S8 metadata `VERB_META_BATCH = 16`, `VERB_RECLAIM = 17` (W:102-106); membership
`0x0100..=0x01FF` — `JOIN 0x0100, RENEW 0x0101, LEAVE 0x0102, CENSUS 0x0103` (MW:184-195); custody
`0x0200`; publish `0x0300` (the cross-owner steps `InsertDentry / RemoveDentry / SetNlink / Rmdir /
MarkShared` ride `PublishCall` with a `request_id`, D:862); tokens/delegation `0x0400` — `DELEG_RECALL
0x0400` (the holder's standing recall channel: the client's call IS the channel, parked until recalls are
pending, the NEXT call carries the acks — owner-initiated push over a dial-only wire), `DELEG_REASSERT
0x0401` (grace re-assertion), `DELEG_INTENT 0x0402` (W:134-154); manager `0x0500..=0x05FF` with ONE
verb `VERB_MANAGER_CALL = 0x0500` carrying `ManagerRequestFrame{schema u32 = MANAGER_SCHEMA 1, request_id
u64, volume u16, call: ManagerCall}` (MGR:53-62, 467-478) — bincode encodes the enum's declaration index,
so the "documented verb codes" (MGR:355-361, 374-383) are the identities peers name: slot/clone/rename
blocks 0x70/0x60, alloc + death ledger 0x80..0x86, leave/endpoint/roots/grow 0xB0..0xB4, 0xA0..0xAF
RESERVED for the recovery driver and deliberately EMPTY ("the death ledger and the `recovered:` records
are a SHARED-DISK projection every manager reads off tree 0 of volume 0, never a verb", MGR:280-285).

**Port v4: one `verb: u16` per call, the inner enum gone** (each `ManagerCall` variant is its own verb in
the `0x0500` block, low byte = the reference's documented code where one exists). The envelopes are
fixed-width `extern struct`s with length-framed vectors `count u16 ‖ items` checked against `len` before
any allocation; strings (endpoint, reason) are `len u16 ‖ utf8`. `volume: u16` rides every manager call
(one listener serves every volume a node manages; a frame naming an ordinal the set lacks is
`STATUS_NOT_MANAGER` naming the width, MGR:1471-1483).

| Verb (port number) | Request payload | Reply payload | Reference |
|---|---|---|---|
| **Manager — region** `JoinAppender` 0x0500 | `identity WireIdentity, ring_want_bytes u64` (0 = derived; clamped to floor/ceiling) | `Joined{appender_id u32, page_addr u64, n_segments u8 ‖ (start u64, len u64)…, n_grant u8 ‖ (start u64, len u32)…, already u8, node_seq_base u64}` | MGR:137-141, 483-497 |
| `ExtentGrant` 0x0501 | `appender_id u32, want u32` (0 = derived) | `Granted{runs}` | MGR:143-144, 498-500 |
| `ReturnExtents` 0x0502 | `appender_id, n_runs u16 ‖ runs` — travel as RUNS, intersected with the record run by run | `Returned{cleared u64, already u64}` | MGR:145-149, 501-505 |
| `LeaveAppender` 0x05B0 | `identity, appender_id, unclaimed runs` | `Left{already}` | MGR:286-296, 607-611 |
| `PublishEndpoint` 0x05B1 | `identity, appender_id, endpoint str, pr_key u64` | `Published{already}` | MGR:297-311, 612-616 |
| `ResolveEndpoint` 0x05B2 | `appender_id` | `Endpoint{present u8, endpoint str}` | MGR:312-319, 617-621 |
| `PublishRoots` 0x05B3 | `appender_id, n ‖ WireSlotRoot{slot u16, g u32, root (addr u64, seq u64), cursor u64, slot_tree_extents u32}` | `RootsPublished{published u32, already u32}` | MGR:320-337, 363-372, 622-627 |
| `GrowRing` 0x05B4 | `appender_id, want_bytes u64` (clamped to the per-appender ceiling less the ring's size) | `RingGrown{present u8, segment (start, len)}` | MGR:338-353, 628-633 |
| **Manager — slots** `AcquireSlots` 0x0503 / `AcquireSlot` 0x0504 / `OfferSlot` 0x0505 / `ReleaseSlot` 0x0506 / `ResolveSlot` 0x0507 | `appender_id, want u16` / `appender_id, slot u16` / `appender_id, slot, to u32` / `appender_id, slot, g u32, words WireSlotWords{root, cursor, slot_tree_extents u32, seq_floor u64}, n_tails u32 ‖ (leaf addr u64, tail u32)…` / `slot` | `SlotsGranted{n ‖ WireSlotGrant{slot, g, words}, already}` \| `SlotRefused{slot, holder u32, g}` / `Offered` / `Released{already}` / `Holder{appender_id, g}` \| `Unleased{g}` | MGR:150-176, 386-427, 506-532 (the slot-lease spec's executors) |
| **Manager — clone** `MarkShared` 0x0570 / `ShareBlock` 0x0571 / `ReleaseShared` 0x0572 | `vol_tag u64, block_idx u64, owner_ino u64, block_index u32` / `vol_tag, block_idx, n ‖ (owner_ino, block_index)` / `vol_tag, block_idx, present u8, (owner_ino, block_index)` | `Marked{already}` \| `SharedGone` / `Shared{inserted u32, already u32}` / `SharedReleased{shared u8, remaining u32}` | MGR:177-210, 545-565 |
| **Manager — rename** `DirRenameLock` 0x0560 / `DirRenameUnlock` 0x0561 (volume 0 only) | `appender_id` | `DirRenameLocked{already}` \| `DirRenameBusy{holder}` / `DirRenameUnlocked{already}` | MGR:211-217, 566-579 |
| **Manager — alloc** `BlockGrant` 0x0580 / `ReturnBlocks` 0x0581 / `AllocLeaseAcquire` 0x0582 / `AllocLeaseBitmap` 0x0583 / `AllocLeaseRelease` 0x0584 | `vol_tag, writer WireIdentity, want u32, held_unconsumed u64` / `vol_tag, writer, start u64, len u32` / `vol_tag, identity, appender_id, home_vol u16, control_ino u64, blocks u64` / `vol_tag, identity, term u64, n ‖ (vol u16, (start u64, len u64))` / `vol_tag, identity, term` | `BlocksGranted{n ‖ (start, len), already}` \| `BlocksFull` / `BlocksReturned{present, cleared u64}` / `AllocLeaseGranted{term, already, predecessor_bitmap, predecessor_blocks}` / `Recorded{already}` | MGR:223-266, 580-606 (the alloc-lease spec) |
| **Manager — death ledger** `RecordDeath` 0x0586 | `member WireIdentity, epoch u64, pr_key u64` (0 = none) | `Recorded{already}` | MGR:270-279, 1214-1225; §2.5 |
| `RecordRecovered` 0x0585 | `member, vol u16` | `Recorded{already}` | MGR:267-269, 1202-1207 |
| **Membership** `Join` 0x0100 | `JoinRequest{id str, role u8 {writer 0, reader 1}, endpoint opt str, pid u32, boot str, prior_epoch opt u64, pr_key u64, mount opt str}` | `Grant{epoch u64, term u64, t_owner_ms, skew_max_ms, d_purge_ms, renew_ms, granted_at_owner_ms, checkpoint_ceiling_ms, slot_leases_ack{prior_epoch u64, n ‖ (slot u16, g u32)}, slot_release_notices n ‖ u16, offered_slots n ‖ (u16, u32)}` | M:913-936, 940-1006; MW:325-343 |
| `Renew` 0x0101 | `RenewFrame{id, epoch, acked_free_epoch u64, block_grant_windows n ‖ WindowDecl}` | `Grant` (the same frame) | MW:218-232, 344-379 |
| `Leave` 0x0102 | `LeaveFrame{id}` | `u8 left` | MW:235-239, 380-390 |
| `Census` 0x0103 | `CensusRequestFrame{cursor u64, limit u32}` (clamped to `CENSUS_PAGE_MAX = 1024`) | `CensusPageFrame{n ‖ MemberSnapshot{id, role, endpoint, epoch, pid, boot, age_ms, expires_in_ms, acked_free_epoch, state str, mount}, next opt u64}` | MW:208-210, 242-257, 391-402; M:1172-1200 |
| **Token** `Grant / Recall / RecallAck / Release / CustodyGrant` (0x0400 block) | the token spec's | — | charter §5.5 "the token wire has its own verb block"; W:134-154 |
| **XV** `XvStep / LookupExact / XvGuards / XvRelease` (0x0300 block) | the cross-owner spec's | — | charter §5.5; D:862 |

Port identities: `id str` in the membership frames becomes the 12 B `(node_token, mount_slot)` for
writers and a 16 B random reader id (readers hold no region, REC:3408-3411); `boot str` becomes the
16 B boot id; `mount opt str` stays a string (it names a path for the OQ-5 collision refusal,
M:1566-1604); `endpoint` = `EndpointWire{family u8, port u16, addr [16]u8}` (20 B, IPv4 in the low 4).

### 1.7 Statuses and the typed retryable classes → EAGAIN

**Reference status words** (one u16 space, scattered by stage): `RPC_OK 0`, `RPC_UNKNOWN_VERB 1`
(CW:1363-1365); membership `0x11 REFUSED` (body `RefusedFrame{reason, retry_after_ms}`), `0x12
UNKNOWN_LEASE` ("the member's correct response is self-fence then re-join, not retry"), `0x13 BAD_REQUEST`
(MW:197-206); meta-ship `32 SCHEMA, 33 STALE_TERM (refused WHOLE, nothing executed), 34 IN_GRACE, 35
MALFORMED, 36 NOT_OWNER, 37 PANIC (must-stay-0), 38 DELEG_FENCED, 39 INTENT_LEASE` (W:109-129, 156-161);
manager `48 REFUSED` (the durable witness contradicts the caller — `manager_verb_refusals`, must-stay-0),
`49 REJECTED` ("the frame's wire integers name what the durable state cannot — REJECTED at the service
edge before any allocation proportional to them", `manager_verb_rejected`, the buggy/hostile-peer class),
`50 DEFERRED` ("not now — the durable state is healthy, nothing was written, the requester RETRIES";
MGR:64-89). The manager's reply body is a `ManagerReplyFrame` under all four (MGR:1557-1579).

**The classifier, ONE place** (MGR:1282-1337): `KvError::Rejected ⇒ REJECTED`; `GrantDeferred`,
`LeaseDeferred` (the alloc-lease re-grant waiting on the home recovery), `HandoverDeferred` (a live
custody grant), `JournalReserveExhausted` (ring 0's USER window full at that instant — the verb holds the
verb mutex and may not park; the checkpoint task is KICKED with the park's mark and the joiner's cadence
retries) `⇒ DEFERRED`; anything else `⇒ REFUSED`. Charter §5.5 states the same: "a wire verb whose ring-0
control entry finds the window full answers `Deferred` and kicks the checkpoint".

**The typed retryable classes** (`RefusalClass`, E:8-57; wire word E:60-86; `WireError{errno i32, msg,
class u8}` W:743-768 — "a typed word beside the errno so no classifier on either side reads the prose"):

| Word | Class | Meaning | Minted by |
|---|---|---|---|
| 1 | `SlotMoved{slot, holder}` | the commit door refused a slot ANOTHER appender leases (`SlotBusy`): re-resolve and re-dispatch | the holder |
| 2 | `StaleHolderView` | a served travelling guard refused a key whose slot the serving mount does not lease: re-resolve through tree 0, ship again | the holder |
| 3 | `HolderUnreachable{holder}` | a record-level verb on an object whose holder has no published endpoint (not yet published, or dead until recovery re-leases) | the shipper |
| 4 | `MembershipPending` | the token holder's membership screen refused this member (a failover's re-assertion window) and no grant re-asserted within the park bound | the token plane |
| 5 | `ListenerRefused` | the peer's listener closed before its challenge and the bounded dial retry ran out | the DIALER (§1.5) |
| 6 | `StaleOwnerEra` | `STATUS_STALE_TERM`: the owner failed over and its successor bumped `term` before arming; the lane learned the new era, the SAME request is admissible on the retry | the owner |
| 7 | `HolderDeferred` | the allocation-lease holder answered "not this beat" (`Deferred` on the wire; `Busy` in-process — the lease in motion) | the holder |

`SqueezefsError::Retryable ⇒ EAGAIN` (E:360). The charter's list names `Deferred` where the reference
has `HolderDeferred` (7) + the manager's `STATUS_DEFERRED` (50): the port's ONE table (`core/errors.zig`)
adds `error.Deferred, error.SlotMoved, error.StaleHolderView, error.HolderUnreachable,
error.MembershipPending, error.ListenerRefused, error.StaleOwnerEra` to `Retryable` (→ `.AGAIN`), and a
wire reply carries `WireErrorWire{errno i32, class u8, reserved [3]u8, reason str}`; `class 0` = an
ordinary errno (`refused(errno)`); an unknown class word decodes as `None` — never a guessed class
(E:73-86). `STATUS_PANIC` does not exist in the port (0002 row 9: no unwinding).

### 1.8 "Bounded codec = bounded execution"

Every executor validates the frame's integers against DURABLE state before anything proportional to them
is allocated (MGR:850-860): `want` clamped to the derivation's cap; return runs checked run by run against
`total_extents` and the caller's record (`validate_return_runs`, AP:1100-1108 — pure, fuzzed) and
COALESCED first (AP:1110-1118: the un-coalesced intersection built an 8 GiB list from 250 K copies of one
run); the slot verbs' identity and words screened at their wire faces; `RecordDeath`'s key word screened
(§2.5); a `Joined` reply's `node_seq_base` screened by the JOINER (`screen_incarnation_base`; JN:33-38
`joined_wire_words_rejected`, must-stay-0; T-ND:1301). A decode limit equals the delivered bytes
(CW:313-321); the manager's decode limit is the CONTROL cap (MGR:644-648). Port: every `n ‖ items` count
is checked `n × @sizeOf(item) ≤ remaining` before the slice is formed; every verb's `screen(frame,
durable) → Rejected | Ok` is a pure function with a fuzz test ("never panic on arbitrary bytes").

### 1.9 Versioning

Two levels (CW:163-168): the FRAMING (`version` in the header) changes only with a transport change; each
vocabulary versions itself (`schema` in the header = the block's: `CLUSTER_WIRE_SCHEMA 6` for the
membership bodies that ride the wire's own number, `MANAGER_SCHEMA 1`, `META_SHIP_SCHEMA 3`). The
reference bumped `CLUSTER_WIRE_SCHEMA` 1 → 6 because bincode is POSITIONAL (a missing trailing field is
EOF, an extra one silently ignored), so every grant-frame change needed a handshake refusal (CW:171-204).
The port's fixed-width envelopes make a schema mismatch a `len`/layout refusal at decode as well, but the
LAW stands: unknown schema refuses at the handshake naming both numbers (CW:925-930, 3207-3212); a manager
reply in another vocabulary refuses (MGR:1566-1571); a reply echoing the wrong `request_id` refuses
(MGR:1572-1577); KD-7 same-commit fleets. Charter §1.7: "unknown wire schema refuses".

---

## 2. Membership

### 2.1 Sharded by home volume (KD-SYM-15)

Volume `v`'s manager is the S6 owner for the mounts HOMED on `v` (`appender page.home_volume`,
AP:291; D:522-523); `squeezefs clients` unions V shards; a shard fails over with its manager lease.
`membership_shards` is the shards THIS owner serves (1 per manager, M:3892-3901). The renewal venue is
the home volume manager's listener; a member learns its successor through the home volume's fixed
ledger (`writer_claim` poll → `observe_successor`, M:3845; `note_successor_observed` wakes a parked
renewal loop instead of pacing at the beat, M:3814-3834).

### 2.2 Records — the reference, and what the port reuses

- `membership_owner` (M:129, 553-645): ONE xattr on local ino 1 beside `writer_claim`, written at the
  owner's arm and deleted at its disarm, never per beat (the WHOLE point of S6: per-beat `client:` commits
  serialized at **455 beats/s** on volume 0 against the 1,500/s 15 k clients need, M:7-20). Fields `{v,
  id (incarnation uuid), term, endpoint, owner_claim_id (durable node id), ttl_ms, ts, pid, boot}`;
  `is_fresh` = armed within its own `ttl_ms` (M:593-600).
- `claim_set` (M:137, 222-343; incompat bit 14): the durable WRITER roster `{v, term, members[{identity{id,
  role, pid, boot, endpoint, pr_key}, ts}]}`, rewritten on CHANGE only; un-engaged volumes read the
  singleton projection of `writer_claim`. The claim-set identity is the DURABLE node id so a successor of
  the same node REPLACES its dead predecessor's entry (never accretes a phantom writer per crash,
  M:3108-3116, 3266-3290).
- `client:{uuid}` rows (guard_claims.md §3.1): written by non-joined mounts every 10 s; a JOINED appender
  writes none (REC:3218-3220).
- The RAM lease table `MembershipOwner` (M:1269-1289): `scc::HashMap<id, MemberState{role, endpoint, pid,
  boot, epoch, join_seq, joined_ms, renewed_ms, deadline_ms, acked_free_epoch, mount, pr_key}>`,
  `next_epoch`, `grace`, `departed_keys` (a departure's registrant key kept for `2 × T_owner`, the age the
  death record can stand, M:1277-1289).

**Port (answering "reuse the port's claim record?"):**
1. **Member registration rows reuse `WriterClaimV1`** with the reserved `kind` byte (Z-claim:63-64 "rung 6
   adds the client row on the same shape"): `KIND_MEMBER = 1` under the key `member:{node_token BE ‖
   mount_slot BE}` — carries `boot`, `pid`, `start_ticks` so the dead-pid proof applies to member rows
   (guard_claims.md Q1), `writer_id`, `ts`; a joined appender still writes NONE (its liveness is the
   lease).
2. **The rendezvous folds into the manager's `writer_claim`**: under KD-SYM-15 the owner IS the manager and
   both records share one lifetime (arm/disarm = claim/unclaim, M:122-128), so `WriterClaimV1` gains
   `endpoint EndpointWire (20 B), ttl_ms u32, flags u32 (bit0 serves_membership)` at version 2 (88 →
   120 B; the port has no release, 0002 §2). The member's rendezvous read is the claim it already polls
   (`observe_successor`'s law, one record, O(1)).
3. **`claim_set` becomes `ClaimSetV1`**, a length-framed binary record on ino 1 `{magic "SQZC", version
   u16, n u16, term u64, members[n]{identity WireIdentity, role u8, reserved [3]u8, pid u32, boot [16]u8,
   endpoint EndpointWire, pr_key u64, ts_ns i64}, xxh3}` — needed for the grace `expected` set and the
   registrant keys a non-reclaimer's death record carries (M:2043-2069). Recorded option (§9 Q-M2): fold
   it into the appender directory (pages already carry identity + state; adding `endpoint` + `pr_key` to
   the page's fixed part costs one slot entry of `SLOT_PAGE_BUDGET = 108`, a charter number) — the owner's
   call with the appender-region spec.

### 2.3 The lease clocks, `T_owner`, eviction

`T_owner = 45 s` = `CLIENT_STALE_TTL_SECS` (M:832-836; the port's `lease_clock.T_OWNER_MS = claim.STALE_TTL_S × 1000`, Z-lc:29 — Q9 landed: no knob on top). `skew_max = max(T_owner × 500 ppm, observed
RTT)` = 22.5 ms at 45 s — two oscillators at ±500 ppm over temperature, a PHYSICAL bound (M:773-782;
Z-lc:26); the owner derives with RTT 0 (no member to measure yet; 22.5 ms covers the measured 0.05–0.25
ms wire RTTs, M:3070-3077). `D_purge = max(2 × revalidation cadence, RTT)` (M:845-857). `T_self = T_owner
− 2·skew_max − D_purge`, REFUSED when not positive (M:865-887; Z-lc:36-40, 70-72). `renew_interval =
clamp(T_self / 3, 1 ms, 10 s)` (M:889-894; Z-lc:43-45). `grace = T_owner` (M:901).

The member anchors on its own SEND instant (never the owner's clock value, M:954-957; Z-lc:10-13). The
owner's sweep `expire_due` runs on the owner's cadence (`renew_interval`), evicting every member with
`now ≥ deadline_ms` — "`>=`: the lease expires AT the deadline, the instant the member's own (strictly
earlier) deadline was measured against" (M:2008-2033, 3209-3225). `evict` (M:1961-2001): remove from the
census, `remember_departed_key`, `declare_dead_epoch` (S7's quarantine cohort), `membership_evictions`++,
`refresh_free_grace_bound`, `note_departure`, **`note_death(DeadMember{id, epoch, pr_key})`** — "a member
the owner evicts here passed `T_owner` under a LIVE owner — the successor's grace window never lists it,
so this is never a reclaimer" (M:1982-1992). The port's `MemberLeaseWords` + models are landed (Z-lc).

### 2.4 Join, renew, grace — the owner's RAM authority

- `arm(id, term, prior_term)` REFUSES `term ≤ prior_term` (M:1326-1340): the successor bumps `term`
  durably BEFORE arming (the claim barrier publishes it; `max_recorded_era` reads the claim set's and the
  rendezvous record's terms, M:686-697).
- `join` (M:1492-1651): empty id refused; `reclaim = prior_epoch.is_some() || grace_expects(id)`
  (M:1510 — an EXPECTED member's first join is a re-assertion even without an epoch); grace active ∧
  !reclaim ⇒ `Refused{retry_after = grace_remaining}` (`membership_grace_refusals`); a reclaim whose epoch
  this owner does not hold ∧ the re-assertion half closed ⇒ `UnknownLease` ("the prior lease is not custody
  — self-fence (purge) first, then re-join fresh", M:1541-1565); the OQ-5 mount-slot collision (same id,
  LIVE at another mount point) refuses naming both mount points and `-o client_slot=` (M:1566-1604); else
  a NEW epoch replaces any prior state, `free_grace::publish_bound(0, …)` at the join, the `Grant` with
  `carry_slot_leases` (M:1606-1650).
- `renew(id, epoch, acked_free_epoch)` is ONE `scc` probe + two stores (M:1656-1670) — the plane's hot
  op, commits nothing.
- **Grace, two halves on one deadline** (M:1244-1263, 2039-2069, 2126-2159): the FRESH-REFUSAL half
  closes EARLY once every `expected` member re-asserted; the RE-ASSERTION half admits a prior epoch until
  `until_ms` whatever the first did (a `-o ro` reader is in no claim set, so never in `expected`; closing
  both at the early close fenced the acceptance fleet's reader at every failover). At the deadline the
  NON-RECLAIMERS are DEAD: `reap_grace_deadline` → `act_on_grace_deadline` → `note_death(DeadMember{id,
  epoch 0, pr_key = the claim set's})` — "shipped NOW — after grace, never at `T_owner`" (M:2071-2124;
  D:560-561).
- `member_liveness` is THREE-valued (M:1300-1311; D:564): `Live` inside the deadline, `Dead` past it /
  departed / absent with the window closed, `Unknown` absent while the window is open — the F2 predicate
  refuses `Dead` alone; before it the successor read every live joiner as dead and refused their objects
  `EAGAIN` for a beat after every failover (1,318 of 1,318 acked files "lost" on the oracle, D:564).
  `member_is_live` keeps the `RecordDeath` screen's word ("not listed" = "not proven live").

### 2.5 The death ledger — records byte-exact, the writer, the screen, the poll

**Keys on tree 0 of VOLUME 0** (AL:66-71): `dead_member:` ‖ `node_token u64 BE` ‖ `mount_slot u32 BE`
(AL:117-123); `recovered:` ‖ `node_token` ‖ `mount_slot` ‖ `vol u16 BE` (AL:145-152); `custody_quarantine:`
‖ `slot u32 BE` on the RECOVERING volume's tree 0 (SS:258-271). Values (`ALLOC_LEASE_VERSION = 1` at byte
0, AL:73): `DeadMemberRecord` = `version u8 ‖ epoch u64 LE ‖ ts_ms u64 LE ‖ pr_key u64 LE ‖ early u8` =
26 B, the decoder TOTAL over three lengths 17/25/26 (AL:276-343 — a quirk, §9); `RecoveredRecord` =
`version ‖ by_term u64 ‖ ts_ms u64` = 17 B (AL:346-375); custody quarantine = `version ‖ until_ms u64 LE`
= 9 B (SS:302-316). Port v4: ONE length each, `extern struct`, reserved bytes zero-checked, decode total
(the bset/entry checksums cover the bytes; no per-record xxh3, as every other tree-0 record):
`DeadMemberWire{version u8 = 1, early u8, reserved u16, epoch u64, ts_ms u64, pr_key u64}` (32 B),
`RecoveredWire{version u8, reserved [7]u8, by_term u64, ts_ms u64}` (24 B), `CustodyQuarantineWire{version
u8, reserved [7]u8, until_ms u64}` (16 B).

**The writer** = the home shard's eviction through a `DeathSink` (`install_death_ledger_writer`,
REC:3399-3450): parse the member id (a reader's uuid holds no region → ignored), `record_death_with_key`
on THIS process's volume 0 (the wire form `RecordDeath` is a peer manager's); a write that fails (device
error — the admission itself PARKS on a full ring) is PARKED in RAM (`defer_death_record`, dedup by
identity, `dead_member_write_deferrals`, AL:1035-1044) and the next ledger poll drains it
(`drain_pending_deaths`, REC:3505-3512). Charter §5.5.2 "retried idempotently".

**The `RecordDeath` screen** (REC:581-634; `screen_death_key` REC:353-370, pure): REJECT (a) the member is
THIS mount's own identity, (b) the installed owner lists it LIVE, (c) the key word names a key this
process or a live member holds, or contradicts the key the census REGISTERED for the member; ACCEPT key 0
or the registered key; a member no census knows is UNVALIDATED → stored with key 0 ("the tail scan is the
fence"). Every verdict before anything is written.

**The join RETIRES the identity's standing death record at the ledger volume BEFORE any page goes Live
under it** (`retire_joiners_death_record`, MGR:734-767): the join is the identity's NEWER incarnation; a
poll that saw the Live page with the record standing would recover a live rejoiner's region. The arm's
`retire_own_death_records` does the same for this writer's own identity (REC:3459-3477).

**The poll** (`spawn_ledger_poll`, REC:3614-3636): one projection per `tick = checkpoint_landing_ceiling_derived()`
= **1,100 ms** (REC:3652-3655) — the §5.5.2 bound `dead_member_propagation_ms ≤ poll +
CHECKPOINT_MAX_AGE` (≤ 2 s; D:553). Each projection (`recover_dead_appenders_set`, REC:3486-3569): probe
volume 0 reachable (every other volume `note_vol0_ledger_probe`; unreachable for `> T_owner` ⇒ the manager
RELEASES its role rather than act on a stale ledger — `manager_should_release_role`, AP:1090-1092,
AL:2421-2451, `manager_vol0_unreachable`) → drain parked deaths → per volume `release_recovered_regions`
THEN `recover_dead_appenders` (one observable step per state) → note recovered slots for the corpse
sweep → the RETIREMENT sweep: a record past `DEATH_RECORD_RETIRE_TTLS = 2` × `T_owner` (one TTL for every
manager to have projected and acted, one for the successor's grace in which a parked member reclaims —
AL:1053-1070) whose identity has no `Live`/`Recovering` page on ANY volume and that no lease names →
retired (`dead_member_records_retired`) → `roll_forward_open_intents` iff anything was recovered. The
mount path runs one projection BEFORE serving (`mount_path_custody_gate`, REC:3575-3609 — the C15 arm; a
`Recovering` page the ledger does NOT name REFUSES the mount naming `appender clear`).

Gauges (AL:989-1021): `dead_members_recorded`, `dead_members_acted` (per region; closure `acted ≡ recorded
× regions held`), `recovered_records`, `dead_member_propagation_ms` (`fetch_max` of `now − ts_ms` at each
act, AL:1074-1077), `dead_members_quarantined` (the allocation holder's arm, a different closure),
`dead_member_write_deferrals` (must-stay-0 on a healthy manager), `dead_member_records_retired`.

### 2.6 PARK-AND-RECLAIM, step by step (KD-SYM-15 extended; PG, PC, M:3587-3808)

Why: a manager's death is its home shard's outage — ≈ N/V members cannot renew for `manager_failover_bound_ms`
(≈ 46.5 s) while their `T_self` (≈ 40–44 s) expires FIRST; the shipped Writer `self_fence` poisoned
`data_custody` — 195 poisoned mounts per routine death (D:556; PG:4-11; R25).

**The word** (PC:14-21): `RUNNING 0 → PARKED 1 → RUNNING` (a reclaim) or `PARKED → EXPIRED 2` (sticky; the
shipped poison, reached by the one terminal signal). **Two model-checked invariants** (PC:22-34; loom
`park_gate_models`): (1) a commit is never both admitted-in-flight and parked — the Dekker pair
`try_enter` (`inflight.fetch_add; fence(SeqCst); load state`, PC:120-134) vs `park` (store `PARKED`;
fence; read `inflight`): either the committer sees `PARKED` and withdraws or the parker sees it in flight
and its ack is held; the weakening (no fence) lets a committer slip through unseen by both — the
acked-then-lost shape; (2) release and expiry are exclusive — exactly one of two racing calls moves the
word off `PARKED`, so a held ack is released OR failed, never both/neither.

1. **The tick fails to renew** (M:3587-3590) and `self_fence_due()` (T_self passed): `self_fence_as(FenceClass::SymmetricAppender)`
   — "THIS lease — the membership lease of a symmetric appender — is the one object whose `T_self` action
   is the PARK; every other caller of `self_fence` is remote custody and poisons" (M:3600-3609;
   `fence_at_t_self`, PG:185-210: `RemoteCustody ⇒ data_custody::poison`, `SymmetricAppender ⇒ GATE.park(now)`,
   `appender_parks`++, `PARKED_AT_MONO_NS` stamped for the FUSE watchdog, the log names `inflight`).
2. **While parked** (D:558; PG:12-27): `pre_admission()` — a DISTINCT pre-admission STATE consulted BEFORE
   ring admission, register-recheck-await on `ADMISSION_WAKE`, never feeding `note_journal_failure` (the
   D1.b escalation cannot turn a park into the fail-stop it exists to avoid; the reference's
   `SQUEEZEFS_TIMEOUT = 30 s < T_park_max` by construction — the port deleted that knob, 0002 row 17, so the
   conveyor's `PARK_ESCALATION_NS` must likewise never count a parked committer) (PG:248-266); in-flight
   entries LAND, their acks HELD on the durability lane in journal order (`hold_acks`: every entry still
   owns its `AdmissionPass`, so the parker's in-flight count IS the acks the hold keeps; at expiry every
   `Ok` becomes `EIO`; at the LEAVE handed back as they are) (PG:268-313); reads serve; token grants and
   recalls continue (`admits_token_service` = `!expired`, PG:229-236 — a parked lessee is still the lock
   master); DMA to granted blocks continues (a grant's liveness is the death ledger, never a TTL); **the
   checkpoint task CONTINUES** (the flush ceiling holds THROUGH a park, T-BG:2388); the page stays `Live`,
   ring/leases/`g` stay in RAM; the FUSE watchdog labels parked ops (`is_parked`), the barrier/`fsync`
   bounded-error timers are SUSPENDED (`bounded_timers_suspended`, PG:214-227); posture word
   `appender_parked`.
3. **Every beat the parked loop** (`member_renewal_tick_decided`, M:3614-3626): `expire_if_due(now)`
   (PG:342-360) — past `T_park_max` ⇒ `GATE.expire` ⇒ `data_custody::poison("… outlived T_park_max")`,
   wake admission + acks (they fail `EIO`), `appender_park_expiries`++ (**must-stay-0 on a healthy
   failover**), `RenewalTick::Fenced` (loop-terminal); else `reclaim_under_park` (M:3766-3808): venue =
   `successor_endpoint()` (the home volume's ledger observation, D:560 item 2) else the spawn-time venue;
   `join` with `prior_epoch = Some(epoch)`; `Ok ⇒ park_gate::release(wait_successor_ns = park age,
   reclaim_rtt_ns)` (PG:320-337: `RUNNING`, wake all, `slot_lease_reclaims`, `appender_park_ns` exact-sum
   `wait_successor / reclaim_rtt / total`) ⇒ `Rejoined`; `UnknownLease ⇒ expire_now + self_fence ⇒ Fenced`
   (terminal: the owner's own verdict says the lease is gone; a park would stand for nothing,
   M:3793-3799, 3689-3697); other ⇒ `Parked` (the park stands; paced at the renewal beat or the
   `SUCCESSOR_WAKE` notify — "never a 1 ms spin against the dead venue", M:3815-3818; T-ML:707).
4. **The successor** admits the reclaim in grace (§2.4), carries `slot_leases_ack{prior_epoch, slots}`
   (M:1649), treats a `Live` page whose owner reclaims as LIVE — no recovery, `g` unchanged, tree 0
   unchanged (D:560) — and ships `RecordDeath` only for NON-reclaimers at the grace deadline (§2.4).
5. **`T_park_max = manager_failover_bound_ms + grace_ms`** (`t_park_max_ms`, PG:90-95; `t_park_max_for`
   reads `LeaseClocks::grace`, PG:105-110), armed by `arm_symmetric_appender(home_volume, t_park_max)`
   (PG:97-103). `manager_failover_bound_ms = T_owner + ladder_ms + replay_ms` (AP:1070-1083: the TTL the D0
   ladder waits out + the ladder's measured wall + the ring replay's measured wall, published live;
   ≈ 46.5 s on the design's row). With `grace = T_owner`: `T_park_max ≈ 90 s + ladder + replay`.
6. **A leave or shutdown over a standing park is REFUSED set-wide** (`LEAVE_REFUSED` latch, PG:112-121;
   T-BG:2154, 2206): a parked appender's dismount is the crash shape on every volume.
7. **The terminal poison splits by class** (`FenceClass`, PG:33-36, 68-77): poison at `T_self` only for
   objects whose lease is NOT reclaimable (S9 remote custody); park for slot leases and the appender
   region (T-BG:2468).

Port: `park_core.zig` (the plain value core with the two invariants) + `park_core_model.zig`
(`expectHolds` both; `expectFails` the fence-less `try_enter`/`park` pair and a load-then-store
`release`/`expire`), `park_gate.zig` on `sync.Notify` (`admission_wake`, `ack_wake`) with the gauges as
`counters.*`; `T_park_max` a pure derivation with a tie test.

---

## 3. Death and recovery — the §5.9 procedure as the reference runs it

Driver: `recover_dead_appenders(vol0, vol_ordinal)` (REC:1164-1260), a no-op on a flat volume, a
non-writer, an unarmed forest, or a volume whose manager lease is not `Held`; per `Live`/`Recovering` page
whose identity the ledger names dead and that is not this mount's own (`set.region(id)` or `same_mount`).
A `Recovered` page the ledger names WITHOUT its `recovered:` record (the recoverer died between the two
writes) gets the record written here, idempotent, under the handover mutex (REC:1199-1235, `completed`).
Then `recover_region` (REC:1281-2157), **under the handover mutex (serialized with the cadence's releases
and the leave) and — from the root install on — the SMO mutex** (the structural door: no manager SMO on
the trees is in flight while their custody moves):

0. **Decide on a FRESH read under the mutex** (REC:1303-1359): re-read the directory; the page must still be
   `Live`/`Recovering` with the SAME identity and term as the poll's snapshot (else "moved under the poll
   — skipped", `skipped`); the death record must still stand (a rejoin retired it); a member the installed
   owner lists LIVE again ⇒ retire the record, leave the region to its holder (`Ok(None)`).
1. **Preempt** (REC:1372-1406; phase `preempt`): `pr_key ≠ 0 ∧ is_own_registrant_key(pr_key)` ⇒ NO preempt
   ("a CO-LOCATED appender shares this host's registrant — a preempt-and-abort of one's own key takes down
   one's own fence; the full tail scan is the fence", KD-SYM-22); else `preempt_meta_registrant(victim)`
   (RACQA 2 = preempt-and-abort under WERO; the port's `pr.Client.preemptAndAbort`, Z-pr:279) on this
   volume's namespace + `data_custody::preempt_dead_registrant(victim)` on every data namespace this
   process holds WERO on; **`pr_fenced = meta_pr_wero ∧ the meta preempt LANDED (== 1)`** — a key-less
   record (`appender clear`'s), a failed preempt or a detection-grade substrate all take the full tail
   scan (REC:1373-1377); `appender_recovery_preempts += meta + data`.
2. **Page `Live ⇒ Recovering{recovered_by_term = writer_term()}`**, written to BOTH directory slots and
   barriered (`write_foreign_page`, REC:1145-1156, 1409-1413); a re-run keeps the term. From here the
   lessee is mid-recovery: `plane.recovering_lessees.insert(id)` SUSPENDS rule 4 of the frame screen on
   its slots (the recoverer's `(0, g)` frames on them are the recovery's own; REC:1414-1418; D:674).
3. **Read the ring from the page's tail** (`read_dead_ring`, REC:1114-1139; phase `read`): segments from
   the page's table (segment 0 minus the two ring-side page slots), `JournalRing::recover_segments(…,
   page.ledger_tail_seq)`, `seq_offset = max(page.seq_offset, seq_offset_of_window(entries))`. Read tree 0's
   lease map and the dead id's `extent_grant` record. A RE-RUN past its tree-0 step finds slots the window
   names that tree 0 already holds `Unleased` — ABSORBED: judged legal, never re-applied; only the page's
   `Recovered` and the `recovered:` record are owed (REC:1439-1476). **The three violation classes over
   the window** (`detect_appender_violations` — key / lease / extent, judged against the LEASE MAP and
   `granted = appender == id ∧ grant_record.contains(extent)`) REFUSE loud: "no merge order is correct"
   (REC:1477-1517; `meta_kv_replay_{key,lease,extent}_violations`). Torn tail records past the head are
   dropped and logged (never acked).
4. **The RAM lease table marks the dead slots `Releasing{dead}` NOW** (REC:1533-1804; still phase
   `replay`): for every slot tree 0 leases to the dead id (minus absorbed): `table.begin_release(slot,
   id)` — a first-touch acquire at the commit door is refused `SlotBusy` naming the holder, exactly like a
   slot mid-handover (T-CM:3931); the gate's `foreign` bit is CLEARED so the STRUCTURAL class (the replay,
   the flush pass's SMOs) is the manager's from here (REC:1631); stale page entries tree 0 no longer
   leases to the appender are DROPPED and counted (`slot_lease_stale_entries`, REC:1550-1557). Then, under
   the SMO mutex with the `RecoveryRollback` RAII declared (§3.1) and the recovery hold recorded for the
   flush-ceiling audit (REC:1585-1601):
   - **Root** = the lessee's PAGE entry when `root.addr ≠ 0 ∧ se.g ≥ lease.g`, else tree 0's grant-time
     root — **never a node-seq comparison** ("a joiner's node seqs are ITS handle's; 'the newer by seq'
     picked the manager's grant-time root over the dead joiner's page root on four of 64 slots and every
     record the joiner had FLUSHED there was lost — 84 acked files absent", REC:1636-1673). Installed
     WRITER-LEGAL at `floor = ring 0's head` (`install_recovered_root` — the root node's seq verified
     against the pointer, the node-seq handle raised to it; `open_unpublished_slot_tree` + `adopt_guest_unpublished`
     when this mount holds no tree) after DROPPING every cached node of the slot — the cache barrier: a
     stale image would fold the window onto a base missing the lessee's flushed bsets; for a WIRE lessee
     the drop runs whatever the roots read, since a lessee of another daemon appends into node IMAGES
     under an unchanged root pointer (REC:1674-1731; the F1 pin's `keep` slot: 40 records appended into the
     grant-time root leaf were invisible at the recoverer, REC:1690-1694).
   - A dead RECOVERER's stashed interior records for the slot (`recovering_structure` — ring 0's window
     carried the manager's flips for a tree whose lessee's page was `Recovering` at this mount's open; the
     open stashed them rather than fold them into a foreign tree) apply HERE onto the installed root,
     `(level DESC, seq)`, before the dead window; the rollback puts them back (REC:1733-1784).
   - **Cursor = `max(recorded, page entry, window max local ino + 1)`** — never lowered (REC:1786-1790;
     window max = the highest local ino per slot over the window's level-0 slot-tree records,
     REC:1558-1580); **extents = max(recorded, page, plane ledger)**; **`seq_floor = max(recorded, dead
     ring's seq frontier)`** (REC:1791-1803).
5. **Two-phase replay under the structural door** (`replay_dead_window(recovery, floor, absorbed)`,
   REC:1807-1813, 2164): phase 1 interior records `(level DESC, seq)`, phase 2 content by key through the
   final routing (charter §5.3); the records' floor is ring 0's head (the replay journals NOTHING here;
   the flush makes them durable; the window itself stays the dead ring's, kept by its `Recovering` page
   until step 8). **The data-bitmap arm** (REC:1816-1858; §5.5.1): for every `alloc_lease:` record whose
   holder is the dead identity with THIS volume as `home_vol`, read its bitmap image, fold the kept
   replayed deltas + the window's kind-4 `vol_tag`-prefixed deltas at the lease's term, write the dirty
   pages at `checkpoint_seq + 1`, barrier (`data_bits_changed`).
6. **Flush** (REC:1860-1886; phase `flush`): barriered `checkpoint_cycle(smo, true)` until no recovered
   slot holds a dirty node, bounded by `COVER_CYCLES_MAX` (64) — "a stuck tail is a defect, never a longer
   wait". The moved roots stay UNPUBLISHED through the cycles (a slot mid-recovery is skipped by
   `publish_forest_roots` like a slot mid-handover; step 7's `Unleased` records ARE their publication).
6b. **Tails** (`recovery_leaf_tails(tree, pr_fenced)`, REC:1888-1901, 2254-2274; phase `tails`): every
   leaf address WITHOUT loading a leaf (`leaf_addrs_unloaded` walks the interior population, REC:2277-2285);
   **on a non-PR substrate EVERY leaf** (a non-resident one costs ONE extent read through
   `peek_tail_offset`, counted in `recovery_full_tail_scan_bytes`; ≈ 3.3 GB / 1–3 s for a 10 M-file tree,
   D:683); **under a device fence the RESIDENT ones only** (`pr_fenced ∧ !resident ⇒ skip`) — the preempt
   makes the scan unnecessary.
7. **Tree 0** (`release_recovered_slots`, REC:1903-1933, 2485-2600+; phase `tree0`), admitted
   DRAIN-AND-RETRY under the SMO mutex the recovery holds (a park here would wait on the checkpoint task,
   which waits on this mutex; `admit_control_drain_and_retry`, REC:2442): per slot, in ONE control entry
   (chunked by the entry cap — "108 complete sets do not fit one 128 KiB entry", D:682): `slot_state:{s} →
   Unleased{root (as FLUSHED — the tree's current root, REC:2504-2516), cursor, g, slot_tree_extents,
   last_written = lease_seq, seq_floor}` + `slot_tails:{s} → {g, tails}` (inline ≤ the value cap, spilled
   to heap extents claimed in the internal class above it, barriered before the record names them; the
   previous spill retired by the superseding entry, D:682; REC:2552-2559) + **`custody_quarantine:{s} →
   until_ms`** iff the record is EARLY (§3.3; REC:1912-1924, 2533-2545) + the dead appender's grant record
   MINUS the trees' images (the leave's own entries). Then (REC:1984-2088, still under the mutex): the
   UNCLAIMED remainder = the page's runs minus every extent the window's `alloc` records claimed, returned
   (`return_extents_inner`; a failure only warns — the region release retries, fsck C13 reclaims); the
   ORPHAN images the grant still claims that no root reaches (`dead_appender_orphans`, C13 for a dead
   appender; never an extent an `alloc_lease:` names) returned AFTER flooring the node-seq handle above
   every stamp they carry (`residue_seq_ceiling`, one extent read per orphan); a `GrowRing` segment the
   manager carved that the page never named is settled (`settle_dead_pending_segment`; a refusal ABORTS
   the step — the page stays `Recovering`, the re-run resumes; REC:2070-2087).
7b. **The RAM lease table LAST** (REC:1935-1983): `table.release(slot, id, g, words, lease_seq)` must
   answer `Released | Already` (else `Corrupt`: "the RAM table and tree 0 disagree"); `gate.revoke(slot)`;
   `ring.raise_seq_floor(words.seq_floor)` (ring 0's offset above the dead ring's frontier — the round-5
   law); **`rollback.begun.clear()` — from here nothing is rolled back**; `refresh_holders`,
   `publish_slot_owners` (the S4 lock plane follows the table: a custody acquire on a recovered slot's file
   is the local arbiter's); the custody quarantine installed in RAM (`quarantine_slot_custody`) and logged.
   Then `drop(smo)`.
8. **Page `Recovered`** (REC:2090-2118): the page RE-READ (its generation moved under the flush cycles'
   own writes), `state = Recovered`, `recovered_by_term`, `slots.clear()`, `recovering_lessees.remove(id)`
   (rule 4 is inert on `Unleased` slots by the lease alone), `head_hint = ledger_tail_seq = the head the
   ring was read to`, `seq_offset` kept; both slots + barrier. **A `Recovered` ring is never replayed**
   (D:690-693): the dead node's remount starts a fresh head; anything a non-PR zombie appended after the
   recoverer's head read is unreachable by construction and — by `T_self` — never acked.
9. **`recovered:{X, v}`** on volume 0 (`manager_record_recovered`, idempotent; gates the allocation-lease
   re-grant); on volume 0 a `dir_rename` lock the dead appender held is released; `handover_done.notify_waiters()`
   wakes the parked doors; phases `tree0`/`total` recorded, `appender_recoveries`++,
   `note_dead_member_acted(ts_ms)` (REC:2120-2157). The set-level projection then rolls forward the
   recovered slots' open intents (REC:3556-3567, `recovery_intents_rolled_forward`).

The region stays `Recovered` until no `alloc_lease:` record names it as a holder homed here
(`release_recovered_regions`, REC:2935, the poll's FIRST act): then its ring returns to the heap and the
page goes `Free`, id and term kept (`recovered_regions_released`). Phases: `appender_recovery_phase_ns`
exact-sum `preempt / read / replay / flush / tails / tree0 / total` (REC:87-99, 377-384).

### 3.1 The RAII rollback's exact scope (`RecoveryRollback`, REC:189-320)

Declared AFTER the SMO mutex is taken (so it drops before the guard; the discard runs under the mutex —
no flush pass mid-walk), disarmed by clearing `begun` once tree 0's records are durable and the table
released (step 7b). On ANY `Err` between step 4 and 7b, per slot begun: (1) `table.abort_release(slot,
id)` — `Releasing{dead}` → `Leased{dead}` (a first-touch acquire is refused in either state; the re-run
begins the release again); (2) the gate's `foreign` bit RESTORED if it was set (so the merge sweep, the
heap-full recovery and the D4 arm SKIP the tree again instead of running an SMO on a tree tree 0 still
leases to the dead appender — `merge_sweep_foreign_skips`; T-CM:3166); (3) the stashed structure put
back, the nodes it dirtied discarded; (4) **the slot's RAM TREE**: every cached node of the slot DISCARDED
(dirty ones included — the window's records the failed replay folded, which the ring still holds and the
re-run folds again; `NodeCache::discard_slot_nodes`) and the tree restored to its pre-install `(root,
floor)` (`restore_root`) or REMOVED from the forest when the run adopted it fresh (`remove_guest`) —
otherwise the installed root's floor, published nowhere, clamps ring 0's checkpoint tail until a re-run
succeeds, and a recovery that never re-runs pins the tail for ever: the ring fills, every commit parks,
the wedge class (REC:204-221; T-CM:3247); (5) `handover_done.notify_waiters()`. **What stays:** the
per-slot extent ledger (a max-only word), the page `Recovering`, the ring-0 seq offset (raised only at
7b). Pinned: `TEST_RECOVERY_FAIL_AT_STEP` 3..9 (REC:117-124) in T-CM:2950 "a recovery that fails at any
step resumes from the durable state without loss".

### 3.2 The recovery bound (`appender_recovery_bound_with_ceiling_ms`, REC:417-466), derived

```
entries        = ring_bytes ÷ 230            // the measured ≈ 230 B journal entry per create (design §1.4)
files_per_leaf = 800 × node_size ÷ 256 KiB   // ≈ 330 B per inode + dentry + layout
leaves         = ceil(entries ÷ files_per_leaf)
load_ms        = ceil(leaves × 120 µs × 3 ÷ 1000)   // 120 µs = the upper end of the measured 85–120 µs cold
                                                     // leaf read; 3 passes = the flush's cold loads, the
                                                     // tail scan's extent reads, the orphan census's reads
fold_ms        = ceil(entries × 1 µs ÷ 1000)         // a per-key LWW insert
bound          = load_ms + fold_ms + checkpoint_landing_ceiling_ms   // ONE barriered cycle = 1,100 ms
```
At the 512 KiB ring floor: 2,279 entries, 3 leaves, 2 + 3 + 1,100 ≈ **1,105 ms**; at 4 MiB: 18,236 / 23 /
9 + 19 + 1,100 ≈ 1,128 ms; at the 32 MiB solo ceiling: 145,889 / 183 / 66 + 146 + 1,100 ≈ **1,312 ms —
the charter's "≈ 1.3 s shipped"**. Published `appender_recovery_bound_ms` (REC:850); the flush-ceiling
audit's recovery extension is capped at it (§6). Time-to-reclaim = `T_owner` 45 s + propagation ≤ 2 s +
the bound ≈ 47–48 s; regions on every volume recover in PARALLEL from the one record (D:742-744).

### 3.3 Custody quarantine (`T_owner + 2 × skew_max`)

An EARLY recorder (`appender clear`, the same-node takeover — `dead.early`) can write a death record
INSIDE a surviving custody writer's `T_self` (the plane's own recorders cannot: `T_self < T_owner`), so
the recovery grants nothing fresh on the recovered slots' files until `ts_ms + custody_quarantine_bound_ms`
where `bound = T_owner + 2 × skew_max` — `T_owner` is the OWNER's view of a custody lease (the instant a
holder may re-grant; `T_self` is the member's side, wrong for an owner-side quarantine), `2 × skew_max`
absorbs the recorder's wall clock against ours (DG:6187-6209; REC:1912-1924). Durable
`custody_quarantine:{slot}` beside the slot's `Unleased` in the SAME entry; RAM map keyed `(volume uuid,
slot)`, a longer quarantine extends, a shorter never shortens (DG:6172-6185); re-derived from tree 0 at
every mount, expired ones retired (`load_custody_quarantines`, REC:684, 3590-3593); refusals answer the
remaining duration (DG:6223-6245); `slot_custody_quarantine_refusals`, `slot_custody_quarantined`.
T-CM:1571, 1756.

### 3.4 `squeezefs appender clear <uri> <id>` (REC:3084-3375) — refusals in order, then the writes

1. Layer-A flock on `path` HELD ⇒ "refusing — the volume is live-mounted on this host. Unmount it first";
   same for volume 0 when distinct (REC:3090-3120).
2. A `writer_claim` on `path` OR volume 0 heartbeated within 45 s ⇒ refuse naming the holder and the age
   ("a live manager holds the set") — from a probe read BEFORE any writer open (REC:3121-3157;
   T-CM:1917).
3. Page `Free`/`Recovered`/absent ⇒ `NothingToClear` (REC:3164-3172).
4. The page is this node's OWN residue (`page_is_own(id, identity, node, slot, writer_open = true)`) ⇒
   refuse ("the next mount of this node recovers it itself") (REC:3180-3199).
5. **`appender_id == 0` ⇒ refuse**: "appender 0's page is the MANAGER's — a dead manager's region is
   recovered by the D0 ladder's successor: mount the set writable once its writer claim aged past the TTL
   (or `claim clear` it) … `appender clear` attests joined appenders' pages (ids ≥ 1) only" (REC:3200-3217).
6. **The parked-joiner bound**: the manager's claim went stale only `stale_for_ms < T_park_max` ago ⇒
   refuse ("a LIVE joined appender parked at T_self reclaims against the successor for up to T_park_max …
   a clear inside that window would recover a live writer's ring") — derived, never a knob (REC:3218-3254).
7. A fresh `client:` registration naming the appender's node ⇒ refuse (REC:3255-3283).
Writes, in THIS order (REC:3284-3358): (1) the page `Recovering{recovered_by_term 0}` — a raw write to both
slots + sync, FIRST, so its `Live` entries attest nothing at the bring-up's C14 settle; (2)
`writer_bring_up` (the frame fence primed, the own pools, the cover, the JOIN — every frame the writes
below flush carries the leases' generations; T-CM:1428); (3) the death record `{epoch 0, ts_ms now,
pr_key 0, early = true}` as a `Try`-admitted control entry on volume 0 + sync + `checkpoint_now` (a
guarded open of volume 0 when distinct, walking the same bring-up); (4) the clean leave (page 0 `Free`).
A crash between (1) and (3) leaves a `Recovering` page the ledger does not name: the next mount refuses it
naming this verb; the re-run is idempotent (`Cleared{was: Recovering}`). `appender_clear_runs`.

`squeezefs appenders <uri>` lists pages: id, identity, state, term, ring segments, leased slots (D:833).

---

## 4. Fencing

### 4.1 WERO by the manager, every appender a registrant — the duty split

- The MANAGER holds **WERO (rtype 3)** on the metadata namespace (`meta_wero` at the open, SJ:16; B:3577)
  and on the data namespaces (`data_custody::acquire_wero`, DC:844); `pr_fencing_group` posture words
  retired (D:825). The port's primitive is landed (`pr.Client` rtype 1/3, `preemptAndAbort`, the two-step
  REGCTL report, Z-pr; `RTYPE_WERO = 3`, Z-nvme:128 — "2 = Exclusive Access refused a registrant's READS",
  T-WR:116).
- A JOINER (`join_wero_as_appender(paths, colocated, enrolled_keys, key)`, DC:1315-1340) **NEVER acquires**:
  co-located (the manager's D0 flock or its claim's boot is this host's — one head, one association, the
  device sees ONE host) ⇒ `adopt_wero_colocated` — ZERO device mutation at join and leave, the standing
  holder cross-checked against the enrolled keys (the data namespaces: the claim set's writer keys; the
  metadata namespace: the key the manager's `writer_claim` derives) — a register here is the rung-9
  finding #1 (the ladder's own-stale proof named the LIVE manager's holder key and unregistering it
  destroyed the fence: `rtype 0, regctl 0`, DC:995-1010); remote ⇒ `join_wero_as_registrant_keyed(key)`
  under ONE key for every namespace class, so the ledger's preempt of it fences the member everywhere.
  A manager holding rtype 1 refuses the join naming WERO (JN:12).
- **`writer_term`** (`WRITER_TERM_XATTR`, B:755-764): the per-volume durable era, written and barriered by
  the mount gate, NEVER deleted (the claim is deleted at clean unmount; the era ladder must outlive it or
  a mount/unmount cycle would reset the term and re-issue tokens a crashed predecessor used); every
  fencing token is `(term << 40) | seq` (B:783-789); the page's `term` is bumped at every (re)join and
  `recovered_by_term = writer_term()` names the recoverer (AP:284-288; REC:1411). The port's claim reserves
  `term_le` (Z-claim:88).
- **The death preempt**: §3 step 1 — `preempt_and_abort` of the dead key on the metadata namespace (nvmet
  executes it as a plain preempt, no host I/O to abort, guard_claims.md §1.3) and every data namespace;
  the own-key guard for co-located appenders.

### 4.2 The registrant cap — gate and learn rule (R:249-453; landed in the port's `CapLearner`)

Every registrant join reads the report SIZED BY `REGCTL` (two steps; the fixed 4 KiB read truncated at the
63rd extended registrant, R:131-140; T-F:405) and `registrant_cap_gate(namespace, report, bytes)`
(R:429-453) refuses LOUD at or past the cap in force, naming the count, the namespace and the remedy
("put the kernel nvmet target in front of the array — its registrant list is unbounded, the ONE
supported target, R-SYM-8 — or use fewer hosts per namespace"; `pr_registrant_cap_refusals`, 0 on nvmet
by construction). Cap in force = declared `SQUEEZEFS_PR_REGISTRANT_CAP` (int 1..=65535) else LEARNED else
0 = unbounded (R:374-381). **Learn ONLY on the REPEAT of the same learnable device-answered status at
the same `regctl`** — one refusal is a candidate, not a cap (`learn_registrant_cap`, R:388-405);
learnable = command-specific (SCT 1) or vendor-specific (SCT 7) class, never the conflict, never
generic/transient (aborts 0x07–0x0a, 0x21, 0x22, Namespace Not Ready 0x82, path-related 0x300..0x37F),
never an errno (R:310-365). UNLEARN when a REGISTER later succeeds at or past the learned count
(R:410-420). `pr_registrants_per_namespace`, `pr_report_bytes` recorded per namespace at every gate
(R:213-247). `registrant_identity_shared` (another rkey under OUR wire host id) ⇒ `pr_registrant_shared`
(R:104-129). Port: `CapLearner.learn/unlearnOnSuccess/effective(declared)` (Z-pr:70-95) is the gate's
state; the gate itself is one function the join door calls after the report.

### 4.3 Co-located mounts — the guarantee row (KD-SYM-22)

N daemons on one host share the node token and the configured `SQUEEZEFS_HOSTNQN`/`HOSTID` pair
(pair-or-neither, guard_claims.md §1.4), so the device cannot tell them apart: a co-located mount's death
is fenced by the kernel-arbitrated **`flock` + `T_self` + the frame screen** (D:663; charter §5.5). The
recovery's own-key guard (§3 step 1) and the adopt-not-register join (§4.1) are its two faces; the full
tail scan is its fence (`pr_fenced = false`). T-CF:112 pins a co-located zombie's DMA refused by the
EPOCH gate where the device cannot. The host-scoped fabric subsystems patch (charter §3,
`nvme_core.fabrics_host_scoped_subsystems`) is what keeps two per-mount identities from merging under one
multipath head — the non-co-located row's precondition.

### 4.4 The frame screen — four rules, every loader, reconciled with the port

Reference rules (D:672-676) over the v2 frame's `(appender_id u32, g u32)` where `g` is the SLOT's lease
generation (never the appender's term): FOREIGN iff (1) `g > g_current` — impossible, loud
(`appender_fence_breach` on PR, `foreign_frames_screened` otherwise); (2) `g < g_current` AND the frame
sits at or past the recorded log tail of that leaf for generation `g`; (3) `g` lower than an earlier
frame's `g` in the same log (non-monotone); (4) `g == g_current` from an appender that is NOT the slot's
lessee — inert while UNLEASED (a release kept `g`; the former lessee's frames stand beside the maintaining
manager's `(0, g)`), **SUSPENDED while the lessee's page is `Recovering`** (the recoverer's `(0, g)`
frames are the recovery's own; rules 1–3 stay armed; on PR the preempt fences the dead lessee anyway).
Applied by EVERY loader: the lessee's `append_frozen` destination-page probe and recovery replay
(readers never load foreign leaves under tokens, D:672).

**The port already has rules 1–4** in `bset.FrameScreen.foreignRule` (Z-bset:620-630): `g > g_current ⇒ 1`;
`recorded_tail{g, tail}: stamp.g ≤ t.g ∧ pos ≥ t.tail ⇒ 2` (reads generation `g`'s tails while the slot is
leased at `g + 1`, D:681); `prev_g: stamp.g < prev ⇒ 3`; `appender_current: g == g_current ∧ id ≠ lessee ⇒
4`, inert when `appender_current == null`. `isBreachRule(1|4) ∧ pr_fenced ⇒ appender_fence_breach`
tripwire, else `meta_kv_foreign_frames_screened` + WARN (Z-node:684-696); the stride scan past the
screened frame for a CURRENT-generation own frame ⇒ `meta_kv_foreign_frame_overwrite_detected` and the
load REFUSES (`zombie_overwrite` — "acked loss, refused loud rather than served with a hole", Z-node:697-712;
D:685 class (ii); T-CO:398). **Reconciliation:** the Recovering SUSPENSION is the ONE missing input — the
recoverer supplies the screen with `appender_current = null` for the dead lessee's slots from step 2 to
step 8 (the semantics the design states: rule 4 fully inert, rules 1–3 armed; REC:1414-1418, 2111), so no
field is added; `verifyNodeExtentScreened` is already the ONE loader both the lessee's append probe and
the recovery's loads call. The reference's screen-at-g-of-the-recorded-tail (rule 2 with `≤`) and the
port's agree. Pins to port: T-CO:152 pre-handover frames never screened, 184 past-the-tail older-g
screened, 229 non-monotone, 309 current-g foreign appender, 398 overwrite; T-CM:2423 zombie on an
untouched leaf (the full tail scan).

### 4.5 The `(0, 0)` priming law

Every frame a WRITER's open appends carries its stamp FROM THE OPEN'S FIRST FRAME WRITER (`prime_frame_stamps`,
B:5245: tree 0's lease population loaded, the fence installed; a re-adopted `Leased{0, g}` stamps `g`, an
`Unleased{g}` native slot stamps the arm's `g + 1`); a frame written with NO fence carries the
structural `(0, 0)` — rule 3's non-monotone `g` on a leaf whose earlier frames sit at `g ≥ 1`, ENDING THE
LOG at the next load. Found twice as an acked-writes loss: PR 5 round 4 primed at the bring-up (before the
cover flushed the D0 claim's leaf); PR 14's inverted matrix found the claim gate's ring-recovery preflight
(`preclaim_ring_recovery` — a replay window that exhausts the ring's user slice runs bounded checkpoint
cycles INSIDE the gate, before the claim; one kill-9 in five at a 1 MiB ring) appending the dead writer's
replayed records unstamped — present on the recovering mount, ABSENT after its clean leave (D:680;
B:5220-5225; T-CO:2795). The joined door needs no prime: its first frames follow `arm_joined_leases`' fence
install. Port: `prime_frame_stamps` is the first statement of EVERY writer door including the gate's
preflight (guard_claims.md §2.2 step 5) and the offline verbs (`appender clear`'s bring-up), with a rails
row: every path that can call `appendBset` on a forest volume has the fence installed (a `FrameStamp` of
`(0, 0)` on a leased leaf is a tripwire `appender_unstamped_frame`, must-stay-0, at the ONE encoder).

---

## 5. The join ladder and the N-daemon posture

### 5.1 The ladder (SJ:1-43; the D0 winner is the manager, every later RW mount joins)

| Rung | What | Refuses naming |
|---|---|---|
| 1 declaration | the plane is the default; `SQUEEZEFS_SYMMETRIC_META=0` on a stamped volume refuses the writable open ("once armed, stays armed", D:1021; EK:422) | the knob (`-o ro` is the read posture) |
| 2 bits | every volume carries the required incompat set incl. 17 (`check_bits`, SJ:74-104) | the volume, the bit, the stamping verb |
| 3 membership | this mount is a member of its HOME shard — the shard's S6 owner when it holds the manager lease; armed at `auto` when no bind is declared; an explicit `off` REFUSES ("a writer that cannot be SEEN cannot be EVICTED, and the S6 eviction is what records a death", SJ:212-220) | `SQUEEZEFS_MEMBERSHIP_BIND` |
| 4 registrant | WERO on the metadata namespaces (the manager's hold; the joiner adopts/registers, §4.1) AND the data namespaces; the cap probed on a registrant JOIN | the namespace, the cap |
| 5 `JoinAppender` | the region goes `Live` under this mount's identity | the page, the recovery driver |
| 6 `AcquireSlots` | the native slot (the manager) / `M` rotor slots | the manager |
| 7 the planes | the custody owner, the publish/meta/manager/token services on ONE listener, the ownership plane, the cadence | `SQUEEZEFS_MW_BIND=off` |

`mount_posture` reads `writer` on every RW mount of a forest set; WHICH leases it holds is what the role
gauges say (`manager_lease`, `alloc_lease`, `slot_leases_held`, `membership_mode`) (SJ:21-23).

### 5.2 The joined door (JN:1-21) — what changes when N > 1 daemons share a set

| Step of `open` | Joined appender |
|---|---|
| Layer A `flock(LOCK_EX)` | **not taken** — the manager holds it on a shared host; N joiners coexist |
| DUR-5 superblock repair | **skipped** — sector 0 is the manager's |
| bootstrap replay of the FIXED ring | as a PROJECTION: tree 0 + the manager's slot trees replayed like a non-writer's (no mint, unpublished slots skipped) |
| Layer B2/B1/`writer_claim` | **none** — the manager's claim stands; rung 4's metadata half runs here BEFORE any wire verb or device write (adopt/register, §4.1) |
| `JoinAppender` | **over the wire**, BEFORE `open_inner`: the region (page + ring + grant) the manager mints is stood up beside the projection as this mount's own; a rejoin over an own `Live` page replays its ring as own residue (T-ND:3415) |
| `AcquireSlots` | over the wire: `M` rotor slots into the own region; every first touch of an unleased slot a wire `AcquireSlot`; a slot another appender leases refuses `SlotBusy` at the commit door |
| checkpoint + times-drain tasks | **spawned** — the JOINED cycle: its slot trees' flushes, its page, its tail, its SMOs in its ring under its grant, its extent returns/refills over the wire; never the ledger, the bitmap, tree 0 or page 0 |

`regions[0]` is the MANAGER's region held for its replay window and tail only; every control write and
every manager verb REFUSES loud on a joiner ("the joiner trusts nothing it did not read durably or derive,
and writes nothing another process owns", JN:16-21). The `Joined` reply's grant word — not the page read —
builds the RAM grant (the manager writes the page twice; a second host's cache may land the first image,
JN:110-119; T-ND:1588). The joiner's wire (`JoinedWire`, JN:154-200): one `ManagerClient` session, a
`redial` flag set by any failed verb (the next verb re-dials the manager at its CURRENT endpoint — a
failover moved it), gauges `joined_wire_{verbs, acquires, releases, extent_grants, extent_returns,
reactive_grants, ring_grows, failures, deferrals, redials, words_rejected}`.

**Operationally, N > 1 means:** (a) one listener per daemon serving `ManagerSetService` (dispatch by the
frame's `volume` ordinal to that volume's service; a node manages the volumes it won; MGR:1414-1486);
(b) `page_is_own` narrows to page 0 for the flock proof — every other page is judged by the EXACT `(node,
mount_slot)` (AP:231-253: "under N daemons on one host [the old rule] would have adopted a LIVE joiner's
ring at the manager's remount"); (c) the identity screen on identity-carrying verbs (§1.3); (d) the cap
derives from RAW cpus (§1.4); (e) a dead joiner's region is the LEDGER's to recover and a third daemon
takes its slot (T-ND:3610); (f) co-located daemons share one registrant (§4.3); (g) per-incarnation
node-seq spaces (`node_seq_base` minted durable at every join; two joiners never mint an equal seq,
T-ND:1475); (h) `member_session_demand_from` × N is the listener's load (§1.5).

### 5.3 The same-node successor rule (AL:3520-3647; AP:2786-2794)

A clean `umount` + remount at ANOTHER mount point of the same host is a DIFFERENT mount slot
(`mount_slot` derives from the mount point). The allocation-lease arm's `Busy` (a live holder record)
resolves it: **the D0 flock this open holds proves dead exactly ONE same-node holder — the predecessor
MANAGER — the identity page 0 carried when this open READ it** (`page0_at_open`; our join has since
rewritten it); a same-node holder whose page is any other is a JOINED appender of this host (alive as
readily as dead; its death is the ledger's), every other node's is a live foreign holder — the arm joins
neither (AL:3581-3604). For the predecessor manager: `record_death(holder, durable_term())` (an EARLY
record, `early = true` — the quarantine of §3.3 applies, AL:1755), replay the window's data-alloc deltas
onto its pages, `manager_record_recovered`, `note_dead_member_acted`, then succeed it at `term + 1`
(AL:3605-3631). Bounded: one same-node takeover + one deferred retry (AL:3531). T-BG:3124 "a clean leave
then a mount at another mount point of the same host arms". Charter §5.7's sentence is exactly this.

### 5.4 The N-daemon harness

- `membership_sim` (SIM:1-48): the D1 simulated-client harness — `Direct` drives the owner's RAM API
  in-process (15 k members = 15 k RAM entries; the SCALE instrument, honest about excluding framing/authn/
  wake), `Wired` drives real `cluster_wire` sessions at small N (the PATH instrument); the four figures:
  volume-0 journal tx/s (~0 at 15 k), renewal latency p50/p99/max, revoke fan-out wall, failover grace
  completion; the 15 k run deferred by D11; victims of the revoke leg are READERS (a writer's fence is a
  process-wide side effect). The sharded sim (T-BG:2511, 2565 "12,500 members over 64 shards") drives
  park-and-reclaim at the core's API.
- `sym_n_daemon_tests` (T-ND:61-128): N `KvMetaBackend`s in ONE process against one file-backed set, the
  manager's real `RpcListener` + `ManagerSetService` dialed by the joiners over loopback; storms on 1/2/4/8
  writers at the fleet's depth with zero replay violations (T-ND:873-902, 4535, 4673).

Port: `src/meta/sym/membership_sim.zig` (Direct = the owner value type; Wired = `wire/*` over loopback,
Linux-only) and an N-backend harness in `tests/` over `std.testing.io` files; the venue (`scripts/venue
test`) runs them; nvmet is needed only for the PR legs (§8).

---

## 6. The flush-ceiling audit — the appender side (KD-SYM-10)

- **Landing ceiling** `checkpoint_landing_ceiling_ms = CHECKPOINT_MAX_AGE_MS + 2 × tick` = **1,100 ms** at
  the shipped 50 ms flush (CK:1038-1040; AP:2433-2451): KD-SYM-10's "within `CHECKPOINT_MAX_AGE_MS`" names
  the cadence TRIGGER the tick fires AT, so a leaf dirtied ε after a checkpoint is `trigger + pass − ε` old
  at its covering barrier and the trigger alone read a healthy mount as overrunning (3 overruns in 3.5 s,
  ages 1,013–1,060 ms; AP:2444-2446). The port derives it already (`checkpoint.zig`).
- **The anticipating trigger** `checkpoint_trigger_ms(max_age, anticipated_term) = max_age − term`
  (CK:1042-1075), the input the age the tick fires AT — NEVER the landing ceiling (feeding it the ceiling
  lands every cycle `2 × tick` past the promise). The TERM = the maximum of `checkpoint_cycle_term_ns` over
  the last `TERM_HORIZON_CYCLES` (64, charter) cycles, where one cycle's term = its wall from the age
  DECISION to barrier #1's completion + `(late − tick)⁺` (CK:1077-1099: one tick of lateness is the wake
  quantization; the excess is the tick's own device work ahead of its decision; the wait for the SMO
  mutex is NOT in `late` — a wait behind another holder is that holder's hold). Charter §5.5 adds the LIVE
  projection (`dirty nodes × per-node unit + promised images × per-image unit + covering barriers × the
  barrier unit`) under `max(horizon term, live projection)`; every cycle's tape rides `.stats`. Measured
  WHY: 16–106 ms past 1,100 on the box with `excused_ns 0` (CK:1063-1064).
- **Exclusions** (AP:2797-2830): `flush_ceiling_recovery_extensions` — barriers past the ceiling but inside
  `ceiling + appender_recovery_bound_ms` while a dead appender's recovery held the SMO mutex (steps 4–7;
  the recovery hold recorded at REC:1585-1591); `flush_ceiling_service_extensions` — late barriers
  explained by a SERVICE hold of the mutex (a wire appender's slot grant/release, a transfer's adoption, a
  projection refresh, a region's release — another actor's hold, never this pass waiting on a peer), the
  excluded time = the MEASURED overlap of such holds with the leaf's dirty window, capped at ONE landing
  ceiling (`appender_flush_ceiling_service_cap_ms`, AP:2453-2467: a hold longer than the ceiling is the
  stall class the cross-plane consumers must SEE, so the excess counts); `flush_ceiling_excused_ns`
  (exact-sum Σ excluded), `flush_ceiling_excused_max_ms`; **`appender_flush_ceiling_overruns` must-stay-0**.
  The audit stamps `dirty_since` on forest slot nodes only (T-AP:3258). Holds through a park (T-BG:2388),
  the steady stream never overruns (T-AP:2880), a slow barrier lands inside the ceiling by anticipation
  (T-AP:2407), an idle span is never a cycle's term (T-AP:2301).

---

## 7. Knobs, gauges, constants

### 7.1 Knobs (reference row → port class per `knobs.Class`)

| Knob | Kind / range / default | Reference | Port |
|---|---|---|---|
| `SQUEEZEFS_CLUSTER_WIRE_MAX_CONNS` | int 64..=65536; derived `clamp(raw_cpus × 16, 64, nofile/8)`, railed by the fd ceiling, the rail announced | EK:385; CW:1573-1637 | `derived_override`, `wire.resolveMaxConnections(explicit, raw_cpus, nofile)` + tie test |
| `SQUEEZEFS_CLUSTER_WIRE_SVC_THREADS` | int 1..=4096; derived `clamp(ceil(cpus/8), 1, 8)` — "retained … no longer allocates lanes" | EK:327; CW:1199-1229 | **delete**: thread-per-connection has no lane count (0002 §1.9 dead code) |
| `SQUEEZEFS_MW_BIND` | str `auto\|addr:port\|off`; `off` refuses the arm; malformed refuses | EK:332; SJ:19 | `config` — THE one listener bind (manager + membership + tokens + custody on it) |
| `SQUEEZEFS_MEMBERSHIP_BIND` | str `off\|auto\|addr:port`, default off; the ladder arms `auto`, explicit `off` refused | EK:352; M:2832-2850; SJ:212-220 | **delete** (quirk Q-K1: a second listener and a second bind for one daemon; membership rides `MW_BIND`'s listener via the verb router, MW:412-424) |
| `SQUEEZEFS_MEMBERSHIP_LEASE_TTL_MS` | int 1000.., default 45000 | EK:353 | deleted (Q9 landed: `T_OWNER_MS` const) |
| `SQUEEZEFS_MEMBERSHIP_SKEW_MAX_MS` / `_PURGE_MS` / `_GRACE_MS` | int; derived `max(T_owner × 500 ppm, RTT)` / `max(2 × cadence, RTT)` / `= T_owner` | EK:354-356; M:838-862 | `derived_override` ×3 (`lease_clock.derive*`, landed) |
| `SQUEEZEFS_MEMBERSHIP_RENEW_LANE` | bool, default on (dedicated `sqz-lease-io` thread vs the shared pool) | EK:357; MW:40-59 | **delete**: charter §9 names `sqz-lease-io` as the contract; the off arm is a measured loser (finding 15) |
| `SQUEEZEFS_NODE_ID_FILE` | path | EK:426 | `config` |
| `SQUEEZEFS_HOSTNQN` / `SQUEEZEFS_HOSTID` | str, pair-or-neither | EK:142-143 | `config` (landed) |
| `SQUEEZEFS_PR_REGISTRANT_CAP` | int 1..=65535; unset = learn | EK:420; R:249-253 | `derived_override` (landed `CapLearner.effective`) |
| `SQUEEZEFS_META_PR_WERO`, `SQUEEZEFS_SYM_ALLOW_NON_PR` | bool | EK:418-419 | deleted (Q8 / 0002 row 188): the manager holds WERO where `RESCAP ≠ 0`; a regular file is the single-kernel class; a non-PR block device refuses loud |
| `SQUEEZEFS_SYMMETRIC_META` | bool, default 1; `=0` on a stamped volume refuses the writable open | EK:422; charter §5.5 | `config` (the one flat opt-out is `--single-writer` at format) |
| `SQUEEZEFS_JOB_WIRE_BIND` | str, default off | EK:380 | rung 8 |
| `-o client_slot=<hex8>` | the explicit mount slot (the OQ-5 collision remedy) | M:1593-1596 | `fuse/mount_options.zig` row |
| `SQUEEZEFS_SYM_{RING_KB, GRANT_EXTENTS, MINT_SLOTS, AFFINITY_MAX_MB, T_IDLE_MS, DIR_STRIPES}` | int, derived | EK:416-427 | the sibling specs' rows |

Test seams the reference keeps as in-process atomics, never env knobs (REC:117-187: `TEST_RECOVERY_FAIL_AT_STEP`,
`_HOLD_BEFORE_TREE0`, `_HOLD_BEFORE_REREAD`, `_HALT_AFTER_RECOVERING_PAGE`, `_SETTLE_REFUSE_N`,
`_SETTLE_EXHAUST_N`, `TEST_CLAIM_CLOCK_SKEW_SECS`; M:3523 `TEST_RENEW_TICK_HOLD_MS`; MGR:50-51
`TEST_JOIN_FORGE_NODE_SEQ_BASE`; `WireClock::Manual` CW:686-713, `LeaseClock::Manual` M:741-767): port them
as fields on the driver / gate / clock structs (`Seams`), and as `SQUEEZEFS_TEST_*` rows only where a
process boundary needs them (the two-daemon venue legs).

### 7.2 Gauges and counters (names as exported; FC/M/MGR/REC/AL/PG)

- **Wire** (`RpcStats`, CW:1737-1761): `cluster_wire_{live_connections, connections_refused, accept_backoffs,
  sessions_admitted, admissions_refused, requests_served, mac_failures (must-stay-0), service_refusals
  (must-stay-0), outstanding_challenges}` per listener; the port adds `cluster_wire_frames_{sent,received}`
  (the framer sequences, CW:577-581, 609-612) and `cluster_wire_dial_retries` (the bounded retry's
  engagement) — charter §10's engagement rule.
- **Membership** (M:4106-4150; MW:85-110; FC): `membership_mode` (`off\|owner\|member`), `membership_term`,
  `membership_members/readers/writers`, `membership_lease_ttl_ms`, `membership_self_deadline_ms`,
  `membership_grace_remaining_ms`, `membership_min_acked_free_epoch`, `membership_role`, `membership_epoch`,
  `membership_self_fenced`, `membership_acked_free_epoch`, `membership_renew_cadence_ms`;
  counters `membership_{joins, renewals, renew_refusals, evictions, self_fences, census_serves,
  grace_refusals, grace_reclaims, reclaim_refusals, registration_commits (grows per CHANGE only — growth per
  beat is the regression), renew_sched_lag_ms, renew_lane_calls}`, `membership_renew_phase_ns` (exact-sum
  `carry_wait / rtt / total`), `membership_renew_serve_ns`, `membership_shards`.
- **Manager** (MGR:70-89; AP:2999-3020): `manager_verbs`, `manager_verbs_per_s`, `manager_service_ns`
  (`admit / execute / reply / total`), `manager_load_pct`, `manager_verb_replays`, `manager_verb_refusals`
  (must-stay-0), `manager_verb_rejected`, `manager_lease` (`held\|peer:<id>\|vacant`),
  `manager_failover_bound_ms`, `manager_dependency_stalls` (must-stay-0), `manager_dependency_stall_bound_ms`,
  `manager_vol0_unreachable`, `appenders_known`.
- **Death ledger / recovery** (AL:989-1021; REC:86-116): `dead_members_recorded`, `dead_members_acted`,
  `dead_member_propagation_ms`, `dead_members_quarantined`, `dead_member_write_deferrals` (must-stay-0),
  `dead_member_records_retired`, `recovered_records`, `appender_recoveries`, `appender_recovery_phase_ns`
  (exact-sum 7), `appender_recovery_bound_ms`, `appender_recovery_preempts`, `recovery_full_tail_scan_bytes`,
  `recovered_regions_released`, `appender_clear_runs`, `recovery_ledger_polls`,
  `recovery_intents_rolled_forward`, `recovery_corpses_swept`, `slot_lease_stale_entries`,
  `slot_custody_quarantine_refusals`, `slot_custody_quarantined`.
- **Park** (PG:38-42): `appender_parked` (0/1), `appender_parks`, `appender_park_ns` (exact-sum
  `wait_successor / reclaim_rtt / total`), `appender_park_expiries` (**must-stay-0**), `appender_park_acks_held`,
  `slot_lease_reclaims`.
- **Fencing** (R:213-268; D:1008): `pr_registrants_per_namespace`, `pr_report_bytes`, `pr_registrant_cap`,
  `pr_registrant_cap_refusals` (must-stay-0 on nvmet), `pr_registrant_shared`, `pr_key`,
  `foreign_frames_screened`, `appender_fence_breach` (must-stay-0 on PR), `foreign_frame_overwrite_detected`
  (must-stay-0 on PR), `meta_kv_replay_{key,lease,extent}_violations` (must-stay-0).
- **Flush ceiling** (AP:2797-2830, 2880-2885): `appender_flush_ceiling_overruns` (must-stay-0),
  `appender_flush_ceiling_recovery_extensions`, `appender_flush_ceiling_service_extensions`,
  `appender_flush_ceiling_excused_ns`, `appender_flush_ceiling_excused_max_ms`, `appender_flush_ceiling_ms`,
  `appender_flush_ceiling_service_cap_ms`.
- **Joined** (JN:154-200): `joined_wire_{verbs, acquires, releases, extent_grants, extent_returns,
  reactive_grants, ring_grows, failures, deferrals, redials, words_rejected (must-stay-0)}`.
Every one reads 0/`solo` on a `--single-writer` mount (charter §5.5).

### 7.3 Constants, with WHY

| Constant | Value | WHY | Ref |
|---|---|---|---|
| `CLUSTER_WIRE_SCHEMA` | 6 | bumped per positional-bincode grant change (port framing v1, per-block schema) | CW:159-204 |
| class caps | 8 KiB / 1 MiB / 16 MiB | what an unauthenticated peer may cost / a whole token set / shard descriptors — blocks never move on the wire | CW:206-220 |
| `FRAME_CHUNK_BYTES` | 64 KiB | a lying length costs one chunk | CW:222-224 |
| `MAC_BYTES` | 32 | HMAC-SHA256 untruncated | CW:226 |
| `ACCEPT_BACKOFF` | 5 ms → 1 s doubling | EMFILE busy loop | CW:229-245 |
| `ACCEPT_POLL_TICK` | 100 ms | latch observation only; before D-5 every dial paid 100.09–100.71 ms | CW:1162-1172 |
| `DIAL_TIMEOUT` = handshake deadline = call reply bound | 10 s | ONE bound: a dialer that retried one bound outlived every pre-authn straggler | CW:3052-3058, 3080-3089 |
| `frame_body_timeout` / `session_idle_timeout` / `enroll_freshness` | 30 s / 60 s / 30 s | a dribbling peer is an error; the idle reaper; the challenge window | CW:1548-1558 |
| `max_peer_id_bytes` / `nonce_cap` | 256 / `max(4 × conns, 64)` | attacker-chosen fields bounded | CW:727-734, 2197 |
| `CONNS_PER_CORE` / `MAX_CONNS_FLOOR` / `CONNECTION_FDS` / `LISTENER_FD_SHARE` | 16 / 64 / 2 / 4 | the shipped factor kept; never regress; socket + nudge dup; a quarter of nofile | CW:1606-1626 |
| `MEMBER_CONTROL_SESSIONS` | 6 | counted in code, census-pinned | CW:1670-1683 |
| mux `reader_tick` | `DIAL_TIMEOUT / 4` | an expired call fails within 1.25 × bound | CW:3437-3442 |
| `CENSUS_PAGE_MAX` | 1024 rows | ~120 B/row stays far inside the 1 MiB cap | MW:207-210 |
| `T_owner` | 45 s | the one staleness law; port tie `TTL > nvme_io_timeout 30 s + beat 10 s` | M:832-836; Z-claim:37-41 |
| `MONOTONIC_RATE_DRIFT_PPM` | 500 → 22.5 ms | two oscillators over temperature | M:773-782 |
| `renew_interval` | `clamp(T_self/3, 1 ms, 10 s)` | three attempts before the deadline; never below the shipped beat | M:889-894 |
| `grace` | `= T_owner` | a full lease period to re-assert | M:901 |
| `manager_failover_bound_ms` | `T_owner + ladder + replay` ≈ 46.5 s | the TTL the D0 ladder waits + two measured walls | AP:1070-1083; D:556 |
| `T_park_max` | `failover_bound + grace` | the longest a live successor can take to arm and admit reclaims | PG:90-95 |
| `DEATH_RECORD_RETIRE_TTLS` | 2 × `T_owner` | one for every poll to act, one for the successor's grace | AL:1053-1070 |
| ledger poll tick | 1,100 ms (the landing ceiling) | propagation ≤ `poll + CHECKPOINT_MAX_AGE` ≤ 2 s | REC:3652-3655; D:553 |
| recovery bound terms | 230 B / 800 files / 120 µs / 1 µs / 3 passes / + 1,100 | §3.2 | REC:446-456 |
| `COVER_CYCLES_MAX` | 64 | a stuck tail is a defect | REC:1868-1882 |
| custody quarantine | `T_owner + 2 × skew_max` | owner-side bound + recorder clock disagreement | DG:6187-6209 |
| landing ceiling | `CHECKPOINT_MAX_AGE + 2 × tick` = 1,100 ms | tick wait + one period of the tick's work | CK:1038-1040 |
| `TERM_HORIZON_CYCLES` | 64 | the audit's own measurement interval | CK:1065-1067; charter §5.5 |
| measured: JSON frame decode | 32.6 µs vs 10 µs budget | why binary | CW:16-19 |
| measured: wire RTT loopback | 0.05–0.25 ms | the skew term's RTT input | M:3073-3075 |
| measured: per-beat xattr commits | 455/s vs 1,500/s | why RAM leases | M:11-16 |
| measured: lost acked files | 84 (root by seq); 1,318/1,318 refused (two-valued liveness) | why page-root-by-g; why `Unknown` | REC:1644-1646; D:564 |

---

## 8. Tests (name — one sentence; venue)

**Wire** (T-CW; portable except the socket legs, which are Linux loopback): `framing_is_binary_and_smaller_than_the_json_it_replaces`
(:129) — the frame size row; `frames_round_trip_through_every_class` (:185); `class_caps_are_ordered_and_named`
(:217); `oversize_frame_refuses_on_both_sides_naming_the_cap` (:241); `lying_length_prefix_allocates_one_chunk_not_the_cap`
(:271) — peak allocation pinned; `stalled_frame_body_hits_the_class_deadline` (:291);
`accept_backoff_ladder_doubles_and_caps` (:327); `the_nonce_is_server_issued_single_use_and_freshness_windowed`
(:364, manual clock); `proof_requires_the_storage_secret_and_bounds_the_peer_id` (:435);
`mac_eq_stays_byte_for_byte_and_length_checked` (:493); `session_key_binds_the_secret_both_nonces_the_peer_and_the_channel`
(:504); `per_frame_mac_rejects_tamper_reorder_replay_and_reflection` (:538); `the_ladder_keys_on_an_authenticated_channel_never_on_tls_presence`
(:622); `a_ca_less_security_config_is_refused_not_downgraded` (:664); `discovery_enumerates_fresh_endpoints_skips_self_and_dedupes`
(:741); `an_authenticated_peer_round_trips_rpc_over_the_wire` (:848); `a_peer_without_the_storage_secret_is_refused`
(:879); `a_dial_refused_at_the_cap_retries_bounded_and_lands_when_a_slot_frees` (:917);
`a_cap_that_never_frees_fails_the_dial_typed_inside_the_bound` (:970) — `ListenerRefused`;
`a_hostile_peer_costs_the_listener_one_bounded_handshake` (:1035); `rpc_service_calls_execute_on_the_connections_own_named_thread`
(:1081) — the thread name is the contract; `a_dial_never_waits_for_the_accept_tick` (:1156);
`pipelined_calls_on_one_session_are_served_concurrently_and_demuxed_by_id` (:1203);
`rtt_instrument_reports_a_counted_loopback_floor` (:1007). Manager frames: `the_manager_call_frames_round_trip_and_the_wire_schema_is_six`
(T-MG:682); `wire_integers_are_never_allocation_authority` (T-MG:1722); `a_join_storm_of_32_completes_inside_the_bound_and_grows_the_directory_chain`
(T-MG:1079); `a_join_replayed_against_a_successor_manager_answers_already` (T-MG:905). Fuzz:
`manager_call_frame` incl. the poisoned-identity arm (MGR:1375-1377).

**Membership** (T-DM; portable with the manual clock): `a_heartbeat_costs_zero_journal_transactions` (:233);
`the_owner_record_is_written_once_at_arm_not_per_beat` (:283); `read_side_cost_is_independent_of_member_count`
(:342); `readers_become_visible_without_a_metadata_write` (:405); `the_client_lease_clock_is_strictly_stricter_than_the_owners`
(:492); `the_client_fail_stops_before_the_owner_can_regrant` (:518); `an_unsafe_clock_configuration_refuses_loud`
(:577); `lease_expiry_declares_a_dead_epoch_that_quarantines_offsets` (:612); `a_successor_that_did_not_bump_the_term_cannot_arm`
(:1042); `owner_failover_opens_a_grace_window_admitting_only_reclaim` (:1058); `a_successors_liveness_word_is_unknown_inside_its_reassertion_window`
(:1136); `a_ram_only_members_reclaim_is_admitted_until_the_deadline_after_the_writers_closed_the_window`
(:1188); `an_expected_members_first_join_is_a_grace_reassertion_not_a_stranger` (:1284);
`membership_verbs_round_trip_over_cluster_wire` (:1394); `renewing_an_unknown_lease_is_refused_with_its_own_status`
(:1435); `the_verb_router_composes_services_additively` (:1459); `many_clients_renew_while_one_is_evicted`
(:1514); `member_id_grammar_matches_pairs_and_bare_node_wildcards` (:1677); `a_mount_slot_collision_within_one_claim_set_refuses_loud`
(:1718); `an_evicted_reclaim_is_not_granted_as_a_silent_fresh_lease` (:1793); `a_frozen_member_whose_lease_died_fences_and_purges_before_rejoining`
(:1831); `a_successor_authoritys_claim_entry_replaces_its_dead_predecessor_never_accretes` (:1943);
`a_writer_member_fenced_by_its_own_deadline_never_re_presents` (:2229); `a_grace_window_reclaim_is_continuity_and_never_fences`
(:2274); `an_explicit_membership_bind_advertises_the_bound_address` (:2359); `a_clean_member_disarm_leaves_the_census_before_process_exit`
(:2390). Liveness (T-ML): `membership_heartbeat_survives_meta_lane_occupancy_past_t_self` (:178);
`a_wedged_renewal_attempt_never_occupies_the_lease_venue_past_its_bound` (:546); `a_refused_reclaim_beats_at_the_cadence_law_never_a_spin`
(:707); `a_refused_reclaim_lands_at_the_successor_the_observation_names` (:776).

**Park-and-reclaim** (T-BG): `a_parked_member_reclaims_against_the_successor_the_ledger_names` (:1010);
`a_not_custody_answer_before_t_self_fences_terminally_and_never_parks` (:1182); `a_park_never_granted_expires_at_t_park_max_with_eio_never_silent`
(:1226); `a_parked_leave_is_refused_on_every_volume_of_the_set` (:2154); `a_shutdown_over_a_standing_park_is_refused_and_fails_the_parked_committers`
(:2206); `an_s9_custody_clients_fence_poisons_while_the_appenders_lease_parks` (:2248);
`a_park_longer_than_squeezefs_timeout_never_escalates` (:2344); `the_flush_ceiling_holds_through_a_park`
(:2388); `a_parked_members_grants_stay_valid_and_its_token_service_continues` (:2425);
`data_custody_poison_splits_by_fence_class` (:2468); `the_sharded_sim_parks_and_reclaims_a_dead_shards_members_at_the_cores_api`
(:2511); `manager_vol0_unreachable_releases_the_role` (:2807); `the_wire_serves_pr8s_verbs_and_record_death`
(:2859); loom `park_gate_models` → the port's `park_core_model.zig` (two holds, two weakenings).

**Recovery** (T-CM; file volumes; the preempt leg needs nvmet): `a_dead_appenders_ring_is_recovered_by_the_ledger_poll_with_no_acked_loss`
(:234); `the_mount_path_recovers_a_ledgered_dead_appender_before_serving` (:372); `a_recovery_that_died_after_the_recovering_page_write_is_re_run_by_the_successor`
(:417); `an_unledgered_live_page_is_never_recovered_and_an_unledgered_recovering_page_refuses` (:471);
`the_owners_eviction_and_the_grace_deadline_write_the_ledger_never_a_reclaimer` (:531);
`a_node_holding_regions_on_three_volumes_is_recovered_on_all_three` (:743); `eight_simultaneous_deaths_are_recovered_by_one_projection`
(:807); `a_dead_wire_lessees_slots_grant_and_dir_rename_lock_are_released_by_the_ledger` (:945);
`set_wide_roles_resume_after_a_volume_0_manager_failover` (:1107); `a_parked_members_reclaim_venue_is_the_successor_the_home_volumes_rendezvous_names`
(:1190); `a_manager_that_released_its_role_under_the_vol0_rule_stops_heartbeating` (:1250);
`appender_clear_refuses_a_live_lease_and_attests_a_dead_one` (:1299); `appender_clear_over_a_crashed_managers_window_stamps_the_leases_generation`
(:1428); `an_early_death_record_quarantines_the_recovered_slots_custody_for_t_self` (:1571);
`a_custody_quarantine_survives_a_manager_restart_inside_the_window` (:1756); `appender_clear_refuses_a_fresh_writer_claim_on_volume_0`
(:1917); `a_c14_conflict_is_reported_refused_and_cleared_by_the_attestation` (:1982);
`a_dead_holders_open_intent_is_rolled_forward_by_its_recovery` (:2322); `a_zombie_frame_on_an_untouched_leaf_is_screened`
(:2423); `a_recoverer_whose_ram_tree_is_stale_installs_the_dead_lessees_newer_root` (:2901);
`a_recovery_that_fails_at_any_step_resumes_from_the_durable_state_without_loss` (:2950);
`a_failed_recovery_leaves_the_dead_lessees_tree_foreign_to_the_managers_sweep` (:3166);
`a_failed_recovery_with_no_re_run_leaves_no_floor_on_ring_0` (:3247); `a_recoverer_dying_after_its_flush_leaves_a_mount_the_next_open_admits`
(:3486); `the_recovered_root_install_raises_the_node_seq_handle_and_refuses_a_stale_pointer` (:3653);
`a_first_touch_acquire_during_a_recovery_is_refused_and_never_regresses_tree_0` (:3931);
`a_custody_grant_of_a_slot_mid_recovery_is_the_dead_holders_never_the_recoverers` (:4040);
`a_page_that_moves_under_the_polls_snapshot_is_skipped_not_recovered` (:4216); `a_rejoined_member_is_never_recovered_and_its_record_is_retired`
(:4298); `a_deferred_death_record_lands_at_the_next_ledger_poll` (:4394); `a_flat_volume_arms_no_recovery_driver`
(:4453); `scoping_recovery_phase_ns_vs_window` (:2544, the bound's row).

**Fencing** (T-F, T-WR, T-CO, T-CF): `the_manager_holds_wero_on_the_metadata_namespace_and_a_flat_mount_keeps_write_exclusive`
(T-F:184; fake); `a_reservation_report_with_more_than_sixty_three_registrants_decodes_completely` (T-F:405);
`a_declared_registrant_cap_refuses_the_next_join_loud` (T-F:469); `below_the_declared_cap_the_join_lands_and_the_gauges_read_the_namespace`
(T-F:517); `the_cap_is_learned_only_from_a_repeated_status_refusal_and_unlearned_on_success` (T-F:643);
`wero_acquire_puts_nvme_rtype_3_on_the_wire` (T-WR:80); `wero_admits_registered_non_holders_and_refuses_unregistered_hosts`
(T-WR:147); `live_wero_admits_registered_second_association_and_refuses_unregistered` (T-WR:371, **nvmet**);
`a_preempted_registrant_cannot_append` (D:1033, **nvmet**); the five frame-screen pins (T-CO:152-398,
§4.4); `a_writer_recovering_an_exhausted_ring_inside_its_gate_stamps_the_leases_generation` (T-CO:2795);
`a_co_located_zombies_dma_is_refused_by_the_epoch_gate_where_the_device_cannot` (T-CF:112).

**Join ladder / N-daemon** (T-MP, T-ND): `rung_2_names_the_missing_bit_and_rung_3_the_membership_plane`
(T-MP:161); `rung_3_refuses_an_explicit_membership_off_and_arms_auto_only_when_unset` (:218);
`a_second_rw_open_through_the_plain_door_is_refused_at_d0_naming_the_join` (:274); `a_solo_armed_mount_walks_the_ladder_to_its_planes_with_dlm_rpcs_zero`
(:375); `a_default_writer_holds_wero_on_a_pr_capable_metadata_namespace` (:541); `the_ladder_binds_every_live_appenders_published_endpoint_and_installs_the_step_shipper`
(:656); `a_token_reader_follows_a_manager_failover_to_the_successors_listener` (:1132);
`a_second_daemon_joins_over_the_wire_and_commits_into_its_own_ring` (T-ND:328); `a_joiners_first_shipped_step_after_a_failover_retries_the_stale_era_once`
(:586); `a_clean_leave_and_rejoin_of_one_identity_never_regrants_a_live_image` (:706);
`concurrent_storms_on_{one,two,four,eight}_writers…` (:873-902); `a_forged_node_seq_base_in_the_join_reply_is_refused_before_anything_is_installed`
(:1301); `two_joined_appenders_never_mint_an_equal_node_seq` (:1475); `a_joiner_honours_the_joined_replys_grant_word_not_its_page_read`
(:1588); `a_joiners_rejoin_over_its_live_page_replays_its_window_as_own_residue` (:3415);
`a_dead_joiners_region_is_recovered_by_the_manager_and_a_third_daemon_takes_its_slot` (:3610);
`a_rejoined_identitys_fresh_region_is_never_recovered_under_its_predecessors_death_record` (:3733);
`a_two_daemon_storm_survives_the_managers_remount_with_zero_violations` (:4535); `three_daemons_hold_disjoint_slots_and_commit_into_three_rings`
(:4673); `a_clean_leave_then_a_mount_at_another_mount_point_of_the_same_host_arms` (T-BG:3124).

**Flush ceiling** (T-AP): `the_flush_ceiling_is_the_checkpoint_age_and_a_parked_device_moves_the_overrun_counter`
(:2001); `a_leaf_that_aged_under_a_service_hold_of_the_smo_mutex_is_an_extension_not_an_overrun` (:2105);
`an_idle_span_is_never_a_cycles_term_so_the_first_burst_after_it_runs_at_the_cadence` (:2301);
`the_cadence_anticipates_the_measured_cycle_wall_so_a_slow_barrier_lands_inside_the_ceiling` (:2407);
`a_steady_write_stream_at_the_default_cadence_never_overruns_the_flush_ceiling` (:2880);
`a_manager_leaf_that_aged_under_a_recoverys_hold_is_a_counted_extension_not_an_overrun` (T-ND:3315).

**What needs nvmet** (rung 8 brings the target; the fake answers everything else on file volumes): the
real preempt-and-abort landing `pr_fenced = true` (and the tail scan SKIPPED), a zombie's post-preempt
write refused at the device (`a_preempted_registrant_cannot_append`), a second association under WERO,
the registrant cap on a vendor-capped array (emulated: `SQUEEZEFS_PR_REGISTRANT_CAP=8` refuses the 9th,
D:1047), `/dev/ngXnY` resolution, two daemons on two hosts (the cloud row). The N-daemon posture itself
needs no fabric: N backends in one process over loopback.

---

## 9. QUIRKS per decision 0002 (add a row to its ledger before diverging)

| # | Reference quirk | Why poor | Better design |
|---|---|---|---|
| Q-W1 | The wire frame is `u32 BE len ‖ bincode(RpcFrame enum) ‖ HMAC`; every verb body is bincode with varint ints; the manager's verb is ONE number carrying an inner enum whose variant INDEX is the identity ("the documented verb codes", MGR:355-383) | A serialization library as the wire (0002 row 2); the schema bumped six times because a positional codec cannot refuse a changed frame (CW:171-204); "verb codes" that are not on the wire | §1.2's 48 B `FrameHeaderWire` + fixed-width envelopes; one `verb u16` per call with the block codes AS the numbers; decode total and fuzzed |
| Q-W2 | Nonces and MACs as uuid/hex STRINGS (CW:817, 623-629, 3226); `peer_id` a string `node_{16hex}.m{8hex}` parsed back with `split_once(".m")` (MI:34-49); endpoints `ip:port` strings parsed per use | Text where the codec is binary; a 36-char uuid for 16 B of entropy; string parsing on identity words | 16 B nonces, 32 B raw MACs, `WireIdentity` 24 B, `EndpointWire` 20 B; the text forms are log renderings |
| Q-W3 | The listener's lane is a hand-rolled `poll(2)` over socket + eventfd with a `parking_lot::Mutex<SessionIo>` "never contended, exists because a task must be Send" (CW:2565-2573, 2843-2889) | A second reactor beside the executor's | The connection thread is an `exec.Lane{.class = .wire}`; readiness via `fdwatch` on the lane's own ring (0003 §D Q-B); the session I/O is lane-owned, no mutex |
| Q-W4 | `MuxSession` reply slots in a `HashMap<u64, MuxPending>` + a heap oneshot per call (CW:3374-3379, 3547-3555) | Per-op allocation on a control path (charter §12) | A fixed `sync.Completion` cell pool sized at dial (the per-session in-flight bound); correlation = cell index ‖ generation |
| Q-W5 | `STATUS_PANIC = 37` "the owner-side execution PANICKED (must-stay-0)" (W:124); `InflightGuard` retires a serve that unwinds (CW:2622-2639) | Unwinding as the error boundary (0002 rows 9, 66) | No such status; a verb's error is its typed outcome; a true bug aborts loud |
| Q-W6 | Retryable classes live in THREE places: `RefusalClass` words 1–7 (E:60-71), the manager's `STATUS_DEFERRED 50` (MGR:89), the membership `0x11 REFUSED` with `retry_after_ms` (MW:197-199); `SQUEEZEFS_TIMEOUT`'s D1.b escalation exempted by a flag (PG:12-17) | Three spellings of "not now"; the charter names `Deferred` once | ONE `Retryable` set in `core/errors.zig` (§1.7) → `.AGAIN`; one reply word `WireErrorWire.class`; `STATUS_DEFERRED` stays the manager reply's status but maps to `error.Deferred` |
| Q-W7 | `SQUEEZEFS_CLUSTER_WIRE_SVC_THREADS` kept "for the callers that size `service_threads` with it" though thread-per-connection allocates no lanes (CW:1207-1211) | Dead knob (§1.9) | Deleted |
| Q-K1 | Two listeners and two bind knobs per daemon: `SQUEEZEFS_MEMBERSHIP_BIND` (its own `RpcListener`, M:3118-3124) beside `SQUEEZEFS_MW_BIND`'s owner listener | Two sockets, two caps, two endpoints to publish for one daemon | ONE listener (`MW_BIND`) with the additive verb router (MW:412-424 already composes); `MEMBERSHIP_BIND` deleted; the ladder's rung 3 refusal names `MW_BIND=off` |
| Q-K2 | `SQUEEZEFS_MEMBERSHIP_RENEW_LANE` A/B (EK:357) whose off arm is the measured loser; charter §9 names `sqz-lease-io` | A lever over a contract thread | Deleted; the renewal's venue is the `sqz-lease-io` thread by charter |
| Q-M1 | `membership_owner`, `claim_set`, `client:`, `JoinRequest.boot/id`, `MemberSnapshot` as tolerant JSON / Strings (M:295-343, 603-645) | 0002 rows 2, 181 | §2.2's binary records; `WriterClaimV1.kind = 1` member rows |
| Q-M2 | THREE durable homes for the writer roster: `claim_set` (members + endpoints + pr_keys), the appender directory (identity + state), `writer_claim` (the manager) — `PublishEndpoint` writes the claim-set entry while the page is the lease's home (MGR:297-311) | Two truths that can disagree (0002 rows 41, 46's class) | Recorded OPTION for the owner: the page carries `endpoint` + `pr_key` and `claim_set` goes; the grace `expected` set = the home volume's `Live` pages; costs one slot entry of the charter's 108 — decide with the appender-region spec |
| Q-M3 | `DeadMemberRecord::decode` total over THREE lengths (17/25/26) for one version byte (AL:308-343) | A version that does not version | §2.5's fixed 32 B record, one length |
| Q-M4 | Death sinks, departure sinks, the installed owner/member, `SUCCESSOR_ENDPOINT`, the park gate, `PENDING_DEATHS`, the custody quarantine map — process-wide statics (`arc_swap`/`once_cell`/`static Mutex`: M:2732-2770, 3813-3818; PG:47-66; AL:1031; DG:6165-6170) | Process globals for per-set state; two sets in one process (the harness) collide | Fields on the set's `Membership`/`ParkGate`/`DeathLedger` values owned by the routed set; the harness runs N sets in one process |
| Q-M5 | `poll(2)` + `std::thread::sleep(cadence)` loops for the owner's sweep and the ledger poll (M:3211-3214; REC:3617-3618) | Sleep as a cadence (AGENTS.md) | `timer_heap.Interval` on a meta-lane fiber (0002 row 69: next = previous DEADLINE + period) |
| Q-R1 | Recovery seams as process statics consumed by CAS (REC:117-187) | Globals; one recovery at a time per process | A `Seams` struct on the driver (the test constructs it) |
| Q-R2 | `screen_record_death` logs a WARN and stores key 0 for an UNVALIDATED word (REC:624-633) | A peer's unverifiable key silently becomes "no fence — tail scan" on every volume | Keep the verdict (the tail scan IS safe) but count it: `dead_member_keys_unvalidated` — a growing count under a live census is a wiring defect |
| Q-R3 | The manager's `appender clear` refusal #2 reads `writer_claim` JSON ages and `client:` JSON `ts` by hand (REC:3139-3153, 3264-3271) | The one staleness law re-spelled at the verb | The verb reads through `claim.Registration.fromClaim` (Z-claim:359-372): one classifier (`live/stale/dead`), the dead-pid proof included |
| Q-F1 | `SQUEEZEFS_META_PR_WERO=0` / `SYM_ALLOW_NON_PR=1` weaker postures (EK:418-419) | 0002 row 188 | Deleted; a regular file is the single-kernel class, a non-PR block device refuses loud |
| Q-F2 | `report_gauges()` keyed by `PathBuf` under a global mutex; the cap learner as process statics (R:213-268) | 0002 row 191 | Per-`pr.Client` fields (landed) |
| Q-J1 | The join ladder's rung 3 reads `membership_mode() == "off"` — a STRING compare on a posture word (SJ:212) | A string deciding a refusal (charter §12) | A typed `Membership.Mode` enum |
| Q-J2 | `owner_claim_identity` falls back to the incarnation uuid LOUDLY when no stable node id resolves (M:3277-3290) | A silent-ish weaker identity: phantom accretion returns | Refuse the writable open: `member_id.resolve` already refuses loud (MI:16-19); no fallback arm |

Not quirks (keep as designed): the coordinator-speaks-first single-use nonce; the verify ORDER
(MAC before nonce consume); per-frame MAC with direction + sequence; thread-per-connection with the
connection as a lane; the connection cap claimed before any spawn; the dial's bounded retry = the
handshake deadline; `TCP_NODELAY`; the three frame classes; the identity screen; "bounded codec = bounded
execution"; the two-halves grace window; three-valued liveness; `RecordDeath` after grace, never at
`T_owner`; the key screen; the join retiring the death record; park-and-reclaim with its two invariants;
recover-before-grant with the RAM table released LAST; the RAII rollback incl. the tree discard; the
page root by `g`, never by seq; the full tail scan on non-PR; a `Recovered` ring never replayed; the
quarantine bound; `appender clear 0` refusing; the own-key preempt guard; adopt-not-register for
co-located joiners; the anticipating trigger with capped exclusions.
