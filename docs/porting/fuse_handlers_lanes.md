# Porting spec — FUSE request lifecycle, handler lanes, the per-inode op guard

Scope: the layer between the FUSE-over-io_uring transport (its own spec) and the
metadata/data planes — how a delivered request becomes a handler invocation on a
per-core lane, what guard it takes, how it replies exactly once, how FORGET/INTERRUPT
and the open count are accounted, what the watchdog and the per-op instruments see.
Charter anchors: §2 (lock order level 1), §4 (READ dispatch law, watchdog, no per-op
timeout wrappers), §6.2 (shared-mode admission), §9 (per-core lanes, handoff targets),
§10 (observability), §13 rung 3 (first mount) / rung 4 (data path).

Reference (read-only, `~/Source/squeezefs`): `crates/fuse3/src/raw/{session,filesystem,
reply,request,op_trace,read_phase}.rs`, `crates/fuse3/src/raw/connection/{fast_dispatch,
fused,fuse_over_uring}.rs`, `crates/squeezefs-ipc/src/{op_trace_core,sqz_exec}.rs`,
`src/fuse_client.rs` (the ONE `impl Filesystem for SqueezefsFilesystem`,
`fuse_client.rs:26199` — there is no `src/fuse_fs.rs` or `src/inode_guard.rs`; the
guard, classifier, watchdog and op registry all live in `fuse_client.rs`),
`src/stripe_locks.rs`, `src/sqz_sync.rs`, `src/sqz_sync_core.rs`, `src/op_trace.rs`,
`docs/design-write-inode-convoy.md`. `docs/design-transaction-lock-removal.md` is
SUPERSEDED (its own header, line 3: "the sector-commit protocol described here no
longer exists in the tree; historical context only") — only its lock-order statement
(§3.7, lines 369) survives, and the charter §2 already restates it.

What the Zig metadata plane already offers a handler (skimmed): `KvBackend.begin /
commit / commitGroup / lookup / range / fsync / allocateIno / isFailed`
(`src/meta/backend.zig:818-914`); the 4a object locks `dlm.Manager.lockMany /
tryLockMany / lockInode / lockDentry` with the canonical order, derived width and
collision census (`src/meta/dlm.zig:1-37, 296-378`); the journal-economy staging
shapes `economy.stage{Create,Mkdir,Symlink,Link,Unlink,Rmdir,Rename,Setattr,
ParentUpdate}`, `GuardPlan` (the 4a request set derived from the records) and
`PendingTimes.absorb` (`src/meta/economy.zig:134-966`). The handler layer's job is to
sit ABOVE those as lock-order level (1), never to re-implement them.

Conventions: `R:` = reference quirk flagged under decision 0002 (ledger candidate);
`PORT:` = what the Zig port does instead. Every number carries its source line.

---

## 1. The request lifecycle (ordered procedure)

### 1.1 Delivery

Two venues deliver requests (`session.rs:960-992`):

* **Ring** (hot path): one worker session per uring queue (`for qid in 0..nqueues`,
  `session.rs:971-986`); the queue worker reaps the delivery CQE and either (a) serves
  a READ inline, (b) polls a fused handler itself, or (c) pushes an
  `InboundUringReq{header_and_op, payload, unique, slot: Ring{qid, ent_idx,
  commit_id}, arrived_ns}` onto the per-queue inbound channel the session's dispatch
  loop reads (`fuse_over_uring.rs:7295-7300` shows the shape at the FORGET site).
* **Classical sideband**: the primary session, armed as `set_classical_sideband()`
  (`session.rs:988`), reads `/dev/fuse` via io_uring `Readv` forever: FORGET/INTERRUPT/
  resends/switchover stragglers (`session.rs:963-968`). Its `ReplySlot` is
  `Classical` (`request.rs:17-19`).

`read_fuse_request` (`session.rs:1254-1363`) yields `ReadResult::Destroy` on a dead
connection — errnos `ENODEV | ECONNABORTED | ENOTCONN | EPIPE | EBADF | ESHUTDOWN` or
kind `NotConnected | BrokenPipe` (`session.rs:1280-1292`) — else
`ReadResult::Request{in_header, data_buffer, uring_payload, filled, reply_slot}` where
`filled = n − 40` is what the transport actually WROTE (`session.rs:1352-1362`; the
FUSE-3g law). A short read (`n < 40`) or an undecodable header is a `Request` with
`in_header: Err` and `filled: 0`; the dispatch loop `continue`s (no reply is owed for
an unparseable header — there is no `unique` to address; `session.rs:1317-1349,
1408-1411`).

### 1.2 Body bounds (FUSE-3g)

`validated_body(header_len, filled, payload_len, buf_len)` (`session.rs:5846-5863`):

```
declared = header_len − 40          (checked_sub; below 40 ⇒ ShortHeader)
available = filled + payload_len    (payload_len = out-of-band WRITE lease length, or
                                     the zc held-slot length `zc_write_held_len(slot)`)
declared > available ⇒ Overdeclared{declared, available}
else Valid(min(declared, filled, buf_len))
```

Refusal replies `EINVAL` through the request's own `ReplyTx` and continues
(`session.rs:1474-1488`). WHY (on the line, `session.rs:1439-1453`): `in_header.len` is
kernel-supplied; below 40 the historical subtraction UNDERFLOWED to a ~2^64 slice
(instant panic of the dispatch task = every in-flight request on the session loses its
reply); above `40 + filled` the slice covered the PREVIOUS request's bytes still in the
reused buffer. The held-slot WRITE arm presents an EMPTY placeholder and the
connection's held table carries the authoritative length (`session.rs:1454-1473`).

Tests: `a_header_shorter_than_the_in_header_never_underflows`,
`an_overdeclared_body_is_refused_not_clamped_over_stale_bytes`,
`the_write_lease_body_rides_the_payload_and_bounds_the_data_slice`,
`the_data_buffer_capacity_is_the_last_bound` (`session.rs:6865-6940`).

### 1.3 Request identity

`Request{unique, uid, gid, pid, slot}` (`request.rs:37-54`), built from the header and
then stamped with the delivery's `ReplySlot` (`session.rs:1420-1422`). Carrying the
slot IN the request is what deleted the transport's sharded `unique → (qid, ent_idx,
commit_id)` map ("no mutex, no hash, nothing to miss and nothing to collide in",
`request.rs:7-11`). `unique` is also the op-trace op id (`op_trace.rs:30-33`; kernel
uniques step by `FUSE_REQ_ID_STEP = 2`, `op_trace_core.rs:44-45`).

PORT: identical. `Request` is a plain value struct; `ReplySlot` is a tagged union
`{classical, ring: {qid: u16, ent_idx: u16, commit_id: u64}}`.

### 1.4 Opcode dispatch

`dispatch_with_max_write` (`session.rs:1365-1729`) is one `match opcode` over 40 arms
(`session.rs:1490-1727`). Per arm the pattern is: deserialize the op-in struct —
failure ⇒ `error!` + `reply_error_in_place(EINVAL)` + return (e.g. `session.rs:
2115-2125`); for no-reply opcodes (FORGET/BATCH_FORGET/INTERRUPT) a parse failure
just returns (`session.rs:2079-2087, 4291-4298, 4622-4630`) — then parse the trailing
name(s) (`get_first_null_position`; missing NUL ⇒ EINVAL, `session.rs:2007-2016`),
mint the one `ReplyTx` (`self.reply_tx(&request)`, `session.rs:716-731`), and `spawn`
the handler future onto a lane. Unknown opcode ⇒ `ENOSYS` reply (`session.rs:
1424-1434`). Duplicate `FUSE_INIT` ⇒ `warn!` and re-handle (`session.rs:1491-1496`).
`FUSE_DESTROY` ⇒ `fs.destroy(req)` INLINE, header-only success reply written
synchronously BEFORE `teardown_over_uring()` (shutdown clears the pending map, so a
reply routed after it would be dropped and the kernel would wait forever,
`session.rs:1498-1528`), then the loop returns. `ReadResult::Destroy` ⇒
`fs.destroy(Request::default())` and return (`session.rs:1388-1392`).

Body parsing runs on the DISPATCH task, so a panic there kills every in-flight request
on the session (`session.rs:5780-5783`) — hence `notify_retrieve_body` /
`batch_forget_body` return `Option` instead of slicing (`session.rs:5784-5795`).

PORT: one `comptime` opcode table `[opcode]{ in_size, has_name(s), reply_kind,
handler }`; every parse refusal is `EINVAL` through the same reply discipline; the
decoders are total (fuzzed per charter §11) and run on the worker thread, which never
panics (Zig has no unwinding — row 9 of decision 0002: a parse refusal is an error
union, never an abort).

### 1.5 Lane selection

The handler venue is the `TpcScheduler` (`session.rs:5922-5947`): `lanes: Vec<Lane>`
(one `sqz_exec::LaneExec` + an `alive: AtomicBool` each, `session.rs:5913-5920`),
`next_idx` global round-robin cursor, `node_lanes[node]` groups + per-node cursors,
and `lane_of_cpu[cpu] → lane` (the lane HOMED on that CPU; `None` for core 0, taskset
holes, out-of-range; `session.rs:5938-5946`). Construction (`session.rs:5950-6076`):
CPU ids from the PROCESS affinity mask (`affinity::process_cpus`; never the first
toucher's — the Hang-1 lesson, `session.rs:5952-5955`); core 0 reserved when more
than one CPU (`session.rs:5956-5957`); one lane per remaining CPU; thread named
`fuse3-tpc{i}` (`session.rs:6036`); affinity scope `node` by default, `core` under
`SQUEEZEFS_FUSE_PIN_SCOPE=core` (`session.rs:6005-6014`); the thread sets
`IS_TPC_LANE`/`CURRENT_LANE` thread-locals and runs `exec.run()` (`session.rs:
6054-6064`); a `DeadMark` drop guard flips `alive = false` on ANY exit path
(`session.rs:6040-6047`).

Dispatch choices, from the handler's point of view:

| Site | Target | Source |
|---|---|---|
| `spawn` (every non-READ opcode) | global round-robin `next_idx % lanes` | `session.rs:6078-6098, 6291-6303` |
| `spawn_read` (READ from the dispatch loop) | SAME lane if the dispatcher runs on a lane thread and `SQUEEZEFS_FUSE_SAME_LANE_DISPATCH` is on (default on), else the rotation | `session.rs:6266-6287, 6239-6252` |
| `tpc_spawn_on_node(node)` | round-robin WITHIN the node's lanes, else the rotation | `session.rs:6107-6119, 6331-6336` |
| `tpc_dispatch_boxed(cpu, node)` (R-2 demote arm: the worker mints a READ handler at the CQE) | the lane homed on `cpu` (= the queue's qid), else node-local, else the rotation | `session.rs:6127-6150, 6342-6344` |
| zero lanes (never production) | a transient named OS thread `fuse3-tpc-fallback` runs it to completion | `session.rs:6082-6092` |

**Dead-lane re-dispatch** (`TpcScheduler::dispatch`, `session.rs:6166-6197`): walk
lanes from `first_idx`, skip any with `alive == false`, count the skips on
`tpc_lane_redispatches` (`session.rs:6203-6208`) with an `error!`; ALL dead ⇒
`std::process::abort()` ("a daemon that can never again answer a FUSE request must fail
loud, not wedge every mount user", `session.rs:6186-6196`). Test
`dead_lane_redispatches_loudly_healthy_lanes_never` (`session.rs:7432`).

**Lane queues are unbounded ON PURPOSE** (RES-18, `session.rs:5875-5907`): a ring
request holds its ent slot from delivery to commit, so resident ring handler futures
≤ `queues × q_depth` — the transport's own geometry is the bound and the kernel stops
delivering when every slot is out. The sideband MUST stay unbounded: it carries
FORGET/BATCH_FORGET AND INTERRUPT on one serialized reader; a full bounded channel
would stall the reader and delay exactly the INTERRUPTs that exist to unstick
requests — "turning a memory-pressure event into a liveness failure". If residency
ever matters the bound to add is `queues × q_depth` on the SIDEBAND alone.

PORT (charter §9): `sqz-fuse-lane<N>` threads, one per queue CPU (decision 0002 row
21 renames `fuse3-tpc`); lane selection is the same three-tier `home cpu → node →
rotation`; the executor is our `std.Io` implementation (decision 0001). The
dead-lane walk and the all-dead abort carry over verbatim; `tpc_lane_redispatches`
keeps its name as a must-stay-0 tripwire.

### 1.6 The handler task

`spawn`/`spawn_read` wrap the future in `handler_scope(unique, fut)` =
`op_trace::scope_with_entry(unique, Some(HandlerEntry), fut)` (`session.rs:
6309-6315`): bound to the op iff traced, stamping `handler_entry` on the first poll
(§7). Inside, the daemon handler's prologue is uniform (e.g. `write`, `fuse_client.rs:
27814-27829`): `METRICS.fuse_ops += 1`; `OpProf::begin(kind, ino)` registers the op
with the watchdog registry (always) and arms phase stamps (gated; §6/§7);
`WriteInflight::enter()` (rig-armed depth sample); `custody_use_enter(ino)` (symmetric
plane, inert solo); then the op-guard admission (§2) → the op → the reply value.

### 1.7 The reply path (FUSE-2: exactly one reply)

`ReplyTx{inner: UnboundedSender<FuseReply>, slot, unique, owed: bool, conn:
Option<Arc<FuseConnection>>}` (`session.rs:125-134`). `owing(...)` sets `owed = true`;
`conn` is `Some` only for ring slots (classical replies "have nowhere to go but the
reply task's device write and pay no Arc clone", `session.rs:721-729`). `no_reply()` is
the handle for FORGET/BATCH_FORGET/INTERRUPT/notifications: it may carry an error reply
but owes nothing and its drop synthesizes nothing (`session.rs:154-167`).

Three delivery arms:

1. **In-place** (READ/WRITE on an armed session; `reply_conn.filter(over_uring_ready)`):
   `mark_replied()` then `conn.write_vectored(header, body, slot)` — a synchronous
   COMMIT enqueue from the handler task, one task wake per op saved (`session.rs:
   5229-5247` WRITE; `5413-5452` READ; the zc prefilled arm `submit_reply_prefilled(slot,
   hdr, n)` at `5346-5361`; the fd-source arm `submit_reply_fd_body(slot, hdr, body, fd,
   off)` at `5417-5433`). `NotFound` from the commit = interrupted/double reply
   (benign WARN); anything else `error!` — the dispatch task sees a dead connection
   through its own read path. Counted `fuse3_{read,write}_inplace_replies`
   (`read_phase.rs:323-355`) — the gauge that found the READ arm "structurally
   disengaged (0 forever)" for weeks because the dispatch loop `take()`d the
   connection handle (`session.rs:1365-1373`).
2. **Channel** (INIT phase / classical / no connection handle): `ReplyTx::send(data)`
   stamps `reply_commit` for traced ops, flips `owed = false`, `unbounded_send`s to the
   per-queue reply task `reply_fuse` (`session.rs:1113-1175`), which writes it with
   `write_vectored` (COMMIT_AND_FETCH for ring slots, classical write otherwise). A
   failed write is classified by `reply_write_verdict(is_notify, kind)`
   (`session.rs:116-123`): notify+ENOENT ⇒ counted `fuse3_notify_enoent`, never logged;
   notify+other ⇒ counted `fuse3_notify_failed`, WARN once per errno class (8 classes,
   `read_phase.rs:386-409`), the task goes on; request+ENOENT ⇒ "may reply interrupted
   fuse request" WARN; request+other ⇒ the reply task ENDS (fatal). The notify arm
   never takes part in teardown (`session.rs:97-115`).
3. **Direct commit** (rows 2/3): when the reply channel is gone, a ring reply still has
   a live address — `conn.commit_reply_direct(slot, header, body)`; success counts
   `note_reply_direct_commit` (into `transport_requests_failed_synthetic`), failure
   `error!` + `note_request_abandoned` (the must-stay-0 `transport_requests_abandoned`,
   `session.rs:227-253`, `fuse_over_uring.rs:2462-2476`).

**Drop guard** (row 1, `session.rs:256-294`): a `ReplyTx` dropped while `owed` —
panic, early return, cancelled future — logs `error!` and synthesizes a header-only
`-EIO` (counted `note_reply_synthesized_by_guard`), through the channel or the direct
commit. `Clone` never carries the obligation (`session.rs:296-310`). Tests
`dropping_an_owing_handle_synthesizes_the_reply`,
`a_replied_handle_synthesizes_nothing_on_drop`, `reply_error_in_place_is_the_one_reply`,
`a_no_reply_handle_stays_silent`, `a_dead_reply_task_is_reported_not_ignored`
(`session.rs:6677-6793`).

Error replies: `reply_error_in_place(err, request, sender)` builds `fuse_out_header{len:
40, error: -errno, unique}` and `send`s it — "the error IS the reply" (`session.rs:
5746-5758`). The handler-value arms encode `Err(errno)` the same way inline (e.g.
`session.rs:2029-2039`).

R: the UNBOUNDED reply channel + a per-queue reply TASK + a `select!` tying the reply
task's death to the dispatch task's (`session.rs:940-948`) exist only because the
async runtime could not commit from the handler's thread; the in-place arm already
bypasses them for the hot opcodes, leaving two code paths for one reply.
PORT: ONE reply path — every handler commits through the transport's
`submit_reply(slot, header, body)` (publish → arm the coalescer → eventfd iff armed;
transport spec), on whatever thread it runs; the "channel arm" does not exist. The
exactly-once obligation is a value type `Reply{slot, unique, owed}` whose
`deinit` commits header-only `-EIO` and counts `transport_requests_failed_synthetic`
if still owed; `transport_requests_abandoned` stays the must-stay-0 tripwire.

### 1.8 Guard release and accounting

The op guard (§2) drops at the class's drop point INSIDE the handler, before the
reply value is returned (write: `fuse_client.rs:28132-28137, 28189, 28232`; the
setattr/fallocate/cfr guards drop at scope end before `Ok(())`). `OpProf::drop`
releases the watchdog slot and records the four phase histograms (`fuse_client.rs:
1916-1932`). `WriteInflight` / `custody_use` RAII release at handler return. The
transport-phase records close on ONE clock read shared with the op-trace
`reply_commit` stamp (`session.rs:5253-5272`).

### 1.9 Failure arms (every state)

| Arm | Reply | Count/log | Source |
|---|---|---|---|
| dead connection on read | none; `fs.destroy`; session ends | debug | `session.rs:1293-1298` |
| short read / bad header | none (no unique) | error | `session.rs:1317-1349` |
| unknown opcode | ENOSYS | debug | `session.rs:1424-1434` |
| body bounds refused | EINVAL | error | `session.rs:1474-1488` |
| op-in deserialize failed | EINVAL (no-reply ops: nothing) | error | per handler |
| name without NUL | EINVAL | error | `session.rs:2007-2016` |
| WRITE size ≠ payload len (and not held) | EINVAL | error | `session.rs:3033-3063` |
| handler `Err(errno)` | `-errno` header-only | — | `session.rs:5746-5758` |
| handler panic / drop | `-EIO` synthesized | error + `requests_failed_synthetic` | `session.rs:256-294` |
| reply channel gone, ring slot | direct commit | `requests_failed_synthetic` | `session.rs:227-239` |
| reply channel gone, no address | nothing | error + `transport_requests_abandoned` (must-stay-0) | `session.rs:240-252` |
| commit returns NotFound | — | WARN "interrupted" | `session.rs:5237-5241` |
| oversize reply | header-only `-EIO` (transport) | `transport_replies_oversize` | charter §4 |
| zc prefilled with no armed conn | EIO | error ("a bug") | `session.rs:5362-5368` |
| fast-read mint races teardown (`Weak` upgrade fails) | none — the worker's drain owns the slot | — | `session.rs:5508-5513` |
| DESTROY | inline success before teardown | debug | `session.rs:1498-1528` |

### 1.10 The READ dispatch law from the handler's view

At the delivery CQE the queue worker applies, in order (`fast_dispatch.rs:52-82`):
**(a)** `fast_dispatch_candidate(opcode, enabled, registered)` (`fast_dispatch.rs:
122-124`) and `Filesystem::read_fast_probe(ino, fh, offset, size, flags, dest)` →
`Served(bytes) | ServedFd{body, fd, off} | Demote` (`reply.rs:250-272`): a `Served`
commits inline through the worker's own `commit_ready_reply`, `record_served` books
`queue_wait = dispatch_lag = 0` exactly and `transport_total = committed − arrived`,
plus the op-trace stamps `transport_recv`, `fast_dispatch` (aliasing dispatch +
handler_entry) and `reply_commit` (`fast_dispatch.rs:167-186`); **(b)** on a zc session
with `size ≤ fusion_ceiling(payload_sz) = max(payload_sz/8, 4096)` (`fused.rs:
393-401`) and `SQUEEZEFS_FUSE_ZC_READ_FUSION` on (default OFF by measurement), the
SAME `mint` future is polled on the worker's fused lane; **(c)** else
`tpc_dispatch_boxed(Some(qid), node, mint(req))` — `fast_read_future` (`session.rs:
5496-5588`): re-parses header + `fuse_read_in` (every refusal EINVAL through a
`ReplyTx` it mints), stamps `transport_recv` and `dispatch` at the SAME mint instant
(`queue_wait` reads 0), and runs `read_handler_body`. **Partition law:** `serves +
zc_read_fusions + demotes ≡ READs` on an armed session, and `zc_read_fusions +
demotes ≡ fuse3_read_inplace_replies` (`fast_dispatch.rs:72-76`). The reap thread
never blocks on any arm.

The probe's contract (`filesystem.rs:57-81`): SYNC, TRY-ONLY, non-blocking — a
contended per-inode lock is a `Demote`, never a spin; never touch a device; never
allocate on the served path beyond a refcount clone; return exactly the bytes `read`
would (same EOF clamp, same coherence source); `dest = (addr, cap)` is the reply
window and a serve may land its bytes there (the commit elides the copy by pointer
equality). The daemon's implementation (`fuse_client.rs:26345-26477`): virtual ino or
`size == 0` ⇒ Demote; O_DIRECT under `direct_device_true` ⇒ Demote; retained extents ⇒
Demote; `size > cap` ⇒ Demote ("the reply window must hold the WHOLE request",
`26368-26375`); `lock.try_read()` else Demote (`26383-26386`);
`ipc_read_probe_locked` (`16195-16288`) walks: attr-cache size (miss ⇒ Miss: the async
`backend.getattr` fallback is unreachable sync) → metadata-cache size override → EOF →
single-block only (multi-block ⇒ Miss) → no live device overlay on the block → active
buffer snapshot slice (extent-repr or uncovered range ⇒ Miss) else
`router.try_read_range_sync` over staging ring → hot tier → read-lane hold → NVMe read
cache; **the guard drops BEFORE the enqueue** (`26388-26391`: "nothing continues under
the guard — the served bytes are already captured").

### 1.11 The port's procedure (what differs mechanically)

```
worker reaps CQE (qid = requester CPU)
 ├─ decode header + op-in with the comptime table (total decoders; refusal ⇒ EINVAL commit)
 ├─ FORGET/BATCH_FORGET: nlookup accounting enqueued task-free; empty reply committed
 │   by the worker at once (fuse_over_uring.rs:7283-7329 — "this IS its one commit")
 ├─ READ: (a) try-only warm ladder → inline commit; (b) fuse (zc, ≤ ceiling, lever on);
 │   (c) hand `Handler{req, op_in, lease?}` to the lane homed on qid
 └─ every other opcode: hand to the lane homed on qid (never the global rotation —
     the kernel already spread the load by CPU; see R-lane below)
lane runs the handler on its Io: prologue (fuse_ops, op registry claim, custody use)
 → op guard admission (§2) → the op (4a guards through dlm.Manager, tx through
 KvBackend) → guard drop at the class's point → reply value → ONE submit_reply
 → Reply.deinit asserts owed == false (else synthesize -EIO + count) → registry release
```

R-lane: the reference rotates every non-READ opcode across ALL lanes (`spawn`,
`session.rs:6078-6098`) and only READ got the same-lane/home-lane treatment "until
their venues are measured" (`session.rs:6256-6257`) — the rotation pays one
cross-thread wake per op for metadata ops too. PORT: every handoff targets the lane
homed on the destination CPU/node (charter §9); the rotation survives only as the
fallback for a homeless CPU. `SQUEEZEFS_FUSE_SAME_LANE_DISPATCH` is deleted (the lever
measured a win, the behaviour becomes the law; row for decision 0002).

---

## 2. The per-inode FUSE op guard (lock order level 1)

### 2.1 Structure

`active_inode_locks: Arc<StripeLocks<SqzRwLock<()>>>` (`fuse_client.rs:9256`), built
`StripeLocks::new(INODE_GUARD_STRIPES)` with `INODE_GUARD_STRIPES = 4096` ("the shipped
4096; data-path holds, not commit parks", `fuse_client.rs:2275-2277, 9799`). The same
width serves `lease_locks` (level 2) and `attr_publish_locks` (KD-5 leaf domain)
(`9798-9801`). `StripeLocks<L>` is a fixed power-of-two array indexed by
`splitmix64(ino) & mask` (`stripe_locks.rs:159-232`; the mix at `167-175`) — zero
allocation after construction, one mask never a division. Two inos may share a stripe:
a collision WIDENS a critical section, never under-locks (`dlm.zig:8-10` says the same
for 4a). The RW primitive is `SqzRwLock` (§2.3). `get_inode_lock_ref(ino)` is the ONE
accessor (`fuse_client.rs:15090-15092`).

The lock-order statement every holder obeys (`stripe_locks.rs:30-158`; charter §2):
(1) `active_inode_locks` → (2) `lease_locks` → (3) `BLOCK_FLUSH_LOCKS` (4096,
`fuse_client.rs:2273, 2344`) → (3.5) `INODE_META_LOCKS` (routing, never held across a
terminal block free) → (4a) `I{ino}`/`D{parent,name}` → (4b) leaf node locks → (4c) the
journal reservation. "Do not hold (1) write-guard across long backend I/O when a finer
lock suffices. Do not acquire (1) while holding (3)." (`stripe_locks.rs:155-157`).

PORT: `StripeTable(RwLock)` over `src/sync/rwlock.zig` — 4096 stripes is a
never-regress floor; unlike 4a the width is NOT derived from `cpus × q_depth` in the
reference (`fuse_client.rs:2275-2277`: "data-path holds, not commit parks"). Carry the
reason on the line; the `stripe_census_entries` shape (`fuse_client.rs:2288-2342`:
`<table>_stripes / _stripe_collisions / _key_waits`) is NOT wired for this table in the
reference (only 4a, serve_ino, inode_meta, block_flush, lease_waiter) — the port adds
`inode_guard_{stripes,stripe_collisions,key_waits}` so the 1/16 load-factor argument
can be checked here too.

### 2.2 Classes and the ONE classifier

```rust
pub enum InodeWriteLockScope {       // fuse_client.rs:4563-4581
    EntireOp,      // exclusive, whole op: inline RMW / layout transition / staged→striped
    MetaPrepOnly,  // exclusive, dropped before data I/O: striped shapes publishing size/map
    Shared,        // READ guard, dropped at the SAME point (KD-1): cache-resident striped write
}
```

`inode_write_lock_scope(file_type, expected_new_size, block_size, size_floor,
range_end, range_fully_mapped, narrow)` (`fuse_client.rs:4720-4763`), verbatim logic:

```
if file_type == "striped":
    floor = size_floor else ⇒ MetaPrepOnly          // any probe miss: never a fetch, never an error (KD-2)
    if narrow ⇒ Shared                              // W-2: a RESIDENT striped snapshot is the whole predicate
    if range_fully_mapped == Some(true) && range_end <= floor ⇒ Shared   // v1: mapped within-EOF (closed bound)
    ⇒ MetaPrepOnly
if file_type == "staged" && expected_new_size <= block_size ⇒ MetaPrepOnly   // the folded staged bypass
⇒ EntireOp                                          // inline / staged-transition / unknown
```

Helpers: `write_touched_blocks(offset, end, bs) = (offset/bs, (end−1)/bs)` on a
validated `end > offset` ("no `len − 1` underflow anywhere", `4770-4773`);
`cached_range_fully_mapped(meta, offset, end, bs)` = `None` when no resident map
(anomaly), `Some(false)` on any hole or `last > u32::MAX`, O(touched) (`4785-4802`);
`shared_size_floor(ino) = min(metadata_cache.size, attr_cache.size)`, `None` on
either miss — the CONSERVATIVE floor, the opposite direction of `freshest_size`'s
`max` (`22380-22394`). `shared_write_probe(ino, offset, end) → Option<size>`
(`15115-15140`) is the RAM-only probe: pure `peek_with` on both caches, zero awaits,
delegates to the classifier, and returns the metadata-cache size as the admission
SNAPSHOT (KD-6).

Design clauses (design-write-inode-convoy §4.1, lines 67-119): the Shared predicate is
cache-only, "any failure falls through; never an error, never a second admission";
alignment / W1 eligibility / payload size are deliberately NOT in it ("those choose the
data-plane vehicle; Shared is about the inode plane"). KD-3's v1 mapped-only class
survives as the `SQUEEZEFS_WRITE_GUARD_NARROW=0` A/B leg; the default class since
2026-09-05 (W-2) is EVERY cache-resident striped write — extending stream and
hole-fills included — because every inode-plane mutation they perform
(hole allocation, `merge_block_mappings`, size publish) runs AFTER the drop point on
BOTH modes under (3) + (3.5), so "the exclusive meta-prep protected nothing the read
guard does not" (design §4.4 row 15; `fuse_client.rs:4834-4848`).

### 2.3 The write-preferring law (the primitive)

`SqzRwLock` (`sqz_sync.rs:135-226`) over `LockCore` (`sqz_sync_core.rs`): (1) **no
ownership assignment to sleeping waiters** — release flips FREE and wakes the queue
front; woken waiters re-contend (`sqz_sync.rs:7-11`); (2) **FIFO courtesy / bounded
barging** — a FRESH attempt (`my_id == None`) does not barge: shared yields to ANY
queued exclusive waiter (**write preference**), exclusive yields to ANY queued waiter;
a QUEUED waiter re-contends unconditionally (`sqz_sync_core.rs:79-84, 92-123`); (3)
**every unbounded park is a 2 s ticked wait** (`TICK = 2 s`, `sqz_sync.rs:39`;
`acquire_ticked`, `109-128`; counts `lock_ticked_reregisters`); (4) grant-or-enqueue is
ONE critical section (`poll_acquire`, `sqz_sync_core.rs:128-170`). Consequence for the
guard (design KD-2, line 45; §4.2 lines 140-143): "a waiting truncate excludes
newly-arriving Shared writers — no starvation, no upgrade livelock"; a rand-4k storm
cannot starve setattr-size.

PORT: already landed as `src/sync/rwlock.zig` (laws 1-4 in its header, `rwlock.zig:
1-33`; `rwlock_model.zig` proves the split shape fails). The guard uses
`acquireGuard(io, .shared | .exclusive)` and `tryAcquireGuard` (the probe's
`try_read`).

### 2.4 The shared-mode admission algorithm (verbatim)

From design §4.2 (lines 121-151) and the code (`fuse_client.rs:27786-28075`):

```
validate range                                   [no locks]  fuse_client.rs:27785-27788
   wlen == 0 ⇒ Ok(0), no lease/dirty/time side effects (R2-M2)
   write_end = offset.checked_add(wlen); None or > max_file_size ⇒ EFBIG
   killpriv (FUSE_WRITE_KILL_SUIDGID) applied BEFORE the lock         :27795-27797
probe caches without awaiting                    [no locks]  :27847-27848
   admit_shared = write_shared_enabled() && shared_write_probe(ino, offset, end).is_some()
acquire AS CLASSIFIED                                        :27855-27893
   admit_shared ⇒ lock.read().await          (write_lock_wait + write_lock_wait_shared)
   else         ⇒ lock.write().await         (write_lock_wait + write_lock_wait_exclusive)
revalidate UNDER the guard (Shared arm only)                 :27862-27884
   shared_write_probe again on a fresh snapshot
   ├─ Some(size) ⇒ shared_snapshot = size; guard = Shared
   └─ None ⇒ drop(read guard); write_lock_scope_shared_upgrades += 1;
             lock.write().await  (the ONE upgrade — never a loop, never a downgrade)
orphan probe (exclusive arm only, KD-7)                      :27916-27939
   !is_open && no meta cache && no attr cache && backend.getattr == ENOENT
   ⇒ writeback_orphan_discards += 1; drop(guard); Ok(wlen)   (a counted discard ack)
lease (level 2): acquire_write_lease_for_span(ino, offset, wlen)   :27946-27951
resolve (old_size, file_type):                               :27970-28003
   shared_snapshot ⇒ (size, "striped") — NO fetch/refill/await under Shared
   else metadata cache → attr cache (< 1 s) → fetch_metadata (exclusive arm only)
candidate = classifier(...)  → write_lock_candidate_{shared,metaprep,entire}  :28025-28052
final scope = Shared iff candidate Shared ∧ shared_snapshot.is_some(),
              a Shared-shaped candidate on the exclusive arm executes MetaPrepOnly
              → write_lock_scope_{shared,metaprep,entire}; guard.classify(scope)  :28053-28075
dispatch:
   router route (inline/staged): MetaPrepOnly ⇒ drop(guard) before write_file;
                                 EntireOp ⇒ held across the router commit   :28132-28137
   striped: drop(guard) — read or write, the SAME pre-dispatch point (KD-1) :28228-28232
            → dispatch_striped_write under BLOCK_FLUSH_LOCKS (3) + INODE_META_LOCKS (3.5)
postlude (no guard held):                                    :28270-28310
   size floor bump iff landed_striped ∧ publish_expected > effective_old_size
   publish_attr(WriteTimes{mtime, ctime, size_claim: growth only})   (KD-5 door, KD-6 law)
   park_write_times (durable Δtime refinement); DIO coherence at the reply edge
```

Must-not (design §4.2 lines 145-151; `fuse_client.rs:4574-4575`): a Shared or
MetaPrepOnly holder never re-enters `active_inode_locks` (read or write) on the same
ino. KD-6 (design line 49): the admission record `{scope, size_floor, map snapshot,
fencing_token}` survives the drop; a `FencingTokenExpired` retry re-runs the FULL
protocol and re-resolves the size (`fuse_client.rs:28164-28185`); the Shared postlude
publishes times only unless the claim was genuine growth. KD-7 (line 50): the
lifecycle boundary (orphan probe, cold/no-open writes) is exclusive BY CONSTRUCTION —
Shared requires both caches resident.

### 2.5 "Drop the inode guard before data I/O" — mechanically

`HeldWriteGuard{mode: Shared{read guard} | Exclusive{write guard}, since, hold_hist}`
(`fuse_client.rs:4642-4695`) is a local whose explicit `drop(guard)` precedes the
first `await` that can touch a device: the router arm drops it before
`router.write_file` for MetaPrepOnly (`28132-28137`) and after the commit loop for
EntireOp (`28189`); the striped arm drops it before `dispatch_striped_write`
(`28232`). The hold histogram the final scope selected records `since.elapsed()` at
drop — drop-based so every exit after classification records truthfully, and an
unclassified exit (orphan discard, lease error) records nothing, keeping
`Σ hold ≡ Σ final scope` (`4651-4658`). The acquisition's end clock read IS the hold's
start (one read serves wait + hold, `27852-27855`). WHY the drop (design KD-1, line
44): holding across the DMA window would block truncate/fallocate for full device
windows (a new convoy) and make any nested inode-lock acquisition a self-deadlock;
truncate-vs-in-flight I/O is handled by `drop_active_block_overlays_beyond` +
`truncate_layout` + block locks.

### 2.6 Taker list

| Taker | Mode | Source |
|---|---|---|
| WRITE | as classified (§2.4) | `fuse_client.rs:27855-27893` |
| SETATTR iff size | exclusive, whole truncate | `28541-28545` |
| FALLOCATE punch/zero and extend | exclusive, BEFORE the lease (P1-9) | `30270, 30300` |
| COPY_FILE_RANGE | src read + dest write, or ONE write when same stripe | `29378-29400` |
| ioctl (GDS) | read | `30448` |
| READ handler | read, SHORT section: size bound + active-buffer hit only | `33867-33874` |
| `read_fast_probe` | `try_read`, dropped before the enqueue | `26383-26391` |
| writeback worker / teardown `flush_single_active_block` | read ("fsync-vs-write ordering courtesy") | `33014-33025` |
| fsync | NONE ("fsync does not take the inode write guard today and its mode does not change", design §4.3 lines 158-161; charter §6.4) | `30064` |
| flush, release, forget, lookup, getattr, mkdir/… | NONE (4a guards via the backend) | — |

Two-ino order (`inode_pair_lock_order`, `15080-15088`): ascending SHARD INDEX, raw ino
tie-break — "two distinct ino pairs can map onto the same two stripes in opposite
raw-ino order (the ABBA the VL8 item-2 wedge capture implicated)"; equal shards
collapse to a single exclusive guard (`29384-29394`). Test
`cfr_pair_guard_order_is_shard_monotone` (`fuse_watchdog_teardown_tests.rs:780`).

### 2.7 Measured numbers

* The convoy: ≈ **1.12 ms/op** `write_lock_wait` under the exclusive guard at 32 × qd32
  while data devices idled at qd ≈ 1–1.5; kern 391 k / il 205 k IOPS; lease layer
  n = 32 acquisitions over 23.5 M writes; W1 engagement 99.96 %; write amp ≈ 1.03;
  the per-inode interval was 82 µs = wake + cache-hit meta-prep (design §1 lines
  15-24, §3 lines 55-63; evidence `.benchmarks/2026-08-10-write-inode-convoy-diagnosis.md`).
* Shared ON vs OFF (A-B-B-A, both orders, kern/il): **523.3–523.4 k / 529.6–530.3 k
  vs 489.4–490.1 k / 491.3–492.0 k**, +7–8 % order-independent once the attr-bucket
  spin stopped masking it (`fuse_client.rs:4807-4810`).
* Attr-publish identical-elision: at 100 k+ ops/s per hot ino every publish inside a
  ~4 ms coarse tick merged byte-identical; the stripe-mutex + bucket-writer traffic
  (32 workers × 32 hot inos) was double-digit worker CPU (`10086-10100`).

### 2.8 Quirks in the guard layer

* R: the READ handler calls `backend.getattr(ino).await` INSIDE its inode read guard
  on an attr-cache miss (`fuse_client.rs:33874-33899`) — backend I/O under level (1),
  the thing the Shared law forbids for writes. PORT: resolve the size before the guard
  (miss ⇒ fetch, then take the guard and re-read the caches), or classify the read as
  the probe does (a miss demotes to a fetch outside the guard).
* R: three parallel guard wrappers for one lock (`get_inode_lock`, `get_inode_lock_ref`,
  bare `active_inode_locks.get_inode_lock`) and two test-override tri-state atomics
  (`WRITE_SHARED_OVERRIDE`, `WRITE_GUARD_NARROW_OVERRIDE`, `4826-4868`) beside the
  memoized knobs. PORT: one accessor; test posture through the knob `Environ.Map` the
  test passes (AGENTS.md: knobs are read ONLY through `knobs.*` with the process map).
* R: `write_shared_enabled()` and `write_guard_narrow_enabled()` are two levers whose
  OFF legs are measured losers (§2.7). PORT: keep ONE A/B lever (`SQUEEZEFS_WRITE_SHARED`,
  default on, `=0` = the exclusive control arm); the narrow/v1 class is deleted — a
  second lever that only reproduces a superseded class is dead code (charter §1.9).

---

## 3. INTERRUPT

`FUSE_INTERRUPT` is a NO-REPLY request (`fs/fuse/dev.c` has no write case for it;
libfuse's `do_interrupt` returns without a reply; `session.rs:4277-4288`). The daemon
used to reply anyway; the kernel treated that as a reply to an UNKNOWN unique, so its
`fc->no_interrupt` latch never engaged "and every interruptible wait keeps paying
interrupt traffic forever" (FUSE-3i). Now: parse `fuse_interrupt_in{unique}` (failure ⇒
return, nothing sent), spawn `fs.interrupt(req, unique)` on a lane with NO `ReplyTx`
(the hook's `Err` is a debug line, `session.rs:4289-4319`). The daemon does NOT
implement `interrupt` — the trait default is `Err(ENOSYS)` (`filesystem.rs:489-493`),
which the session discards. INTERRUPT arrives on the classical sideband only (`session.
rs:963-968`); the ring never carries it.

How a pending op would be found: nothing is indexed by `unique` in the daemon — the
op registry (§6) is keyed by slot, not unique, and the reply address rides the request.
**Nothing is interruptible**: a handler parked at ring admission, inside a conveyor
pass, or on a device CQE is never dropped (charter §4: "dropping a handler mid-commit
is the ring-wedge vector"; §1.9 audit `fuse_client.rs:440-466`: an `Admission` dropped
between admission and `reserve_registered` leaks ring budget forever; a registered
`Reservation` whose future dies before `complete()` stalls `completed_upto` for
every later committer). The kernel's own `fuse_request_end` on a signal already lets
the application return `EINTR`; the daemon's reply then lands on a forgotten unique
(the `NotFound` arm, §1.7 — "may reply interrupted fuse request", benign).

PORT: INTERRUPT is decoded (totality), counted (`fuse_interrupts_received`), and
ignored — no hook, no reply, no lane dispatch (the reference spawns a future per
INTERRUPT only to call a stub). No pending-op table is built: an op's reply slot is
in the request, nothing else needs `unique`.

---

## 4. The handler-lane model from the handler's view

* **One lane = one OS thread = one executor** (`sqz_exec::LaneExec`, `sqz_exec.rs:
  1-40`): a `Mutex<VecDeque>` ready queue + condvar; wakers push + notify from any
  thread; a wake landing mid-poll flips NOTIFIED and re-enqueues (the loom-modelled
  never-lost law); the park is `TICK = 2 s` bounded (`sqz_exec.rs:51`) and a tick that
  finds runnable work counts `lane_exec_tick_rescues` (≈ 0 healthy); a panicking task
  completes-by-panic and counts `lane_exec_task_panics`; the parker is pluggable — a
  lane owning a ring parks IN the ring (`LanePark`, `sqz_exec.rs:26-36`).
* **What may block**: any `await` through the first-party primitives (`SqzRwLock`,
  channels, the conveyor's completion, device CQEs via oneshots) — the lane parks the
  TASK, never the thread; the thread keeps polling other handlers. No OS-blocking call
  on a lane: the one offload venue is the blocking pool (`sqz_blocking::run_blocking`,
  used only for mount-time `read_dir`, `session.rs:742-750`).
* **What must hand off**: anything that must run on a ring OWNER's thread —
  `SINGLE_ISSUER | DEFER_TASKRUN` restricts submission to the ring's owner, the zc
  payload pages exist only as THAT ring's sparse bvec, and the ACK must follow the
  DMA's CQE (`fused.rs:17-27`). So a handler on a foreign lane sends a
  `WorkerMsg::{ZcStore, ZcExtract, Commit}` to the queue worker (two cross-thread wakes
  per 4 KiB op — the term the fused lane kills by polling the SMALL handler on the
  worker itself, `fused.rs:1-15, 28-40`). Device I/O on a data lane: the NVMe worker's
  ring, completion by oneshot back to the lane (`zc_bridge_phase_ns` splits the four
  hops, `read_phase.rs:639-689`).
* **Handoff targets**: the lane homed on the destination CPU (`lane_of_cpu[qid]`),
  else node-local round-robin, else the rotation (§1.5). Service threads (the IPC
  shim) demote into the SAME per-core lanes, never a runtime inject queue
  (~130 µs/op measured, charter §7). Detached work (publish pass, writeback, reclaim)
  runs on `sqz-meta`, never a FUSE lane (`spawn_meta`, `fuse_client.rs:2220`).
* **Thread naming is the CPU accounting contract**: `fuse3-tpc{i}` (`session.rs:6036`)
  — an unnamed lane "inherits the comm of whichever thread first touched the lazy
  scheduler" and showed up as `sqz-ipc-svcN` in pidstat (`6029-6034`). PORT:
  `sqz-fuse-lane<N>` via `thread_name.nameCurrentThread`.
* **Stop law**: a lane never exits on its own; `alive` flips only on thread death and
  the dispatcher re-routes (§1.5). Handlers dispatched before a reply task died still
  reach the kernel (FUSE-2 row 3, `session.rs:72-74`).

---

## 5. The `Filesystem` interface

Trait `filesystem.rs:19-642` (`#[trait_make::make(Send)]`; default bodies return
`Err(ENOSYS)` unless noted). `Result<T> = Result<T, Errno>`; `Errno` encodes as
`fuse_out_header.error = -errno`. Reply structs `reply.rs`. Columns: **R3** = the
port's first mount (rung 3: inline files over the KV backend; data beyond
`inline_max` arrives with rung 4), **R4** = the data path, **never** = not implemented
by the daemon.

| Op (opcode) | Args (after `req`) | Reply | Errno conventions | Daemon impl | Rung |
|---|---|---|---|---|---|
| init | — | `ReplyInit{max_write}` | — | `fuse_client.rs:26479` | R3 |
| destroy | — | () | once per process (`dismount_once`); teardown on a spawned task so external umount cannot cancel it | `27059-27087` | R3 |
| lookup | parent, name | `ReplyEntry{ttl, attr, generation}`; **negative** = nodeid 0 + `negative` TTL (`reply.rs:115-138`) | ENAMETOOLONG > 255; ENOENT → negative entry when `negative_timeout ≠ 0` (`27222-27229`); `.`/`..` reconnect (`27103-27127`); `.stats`/`.config`/`.trace` virtual gens (`27139-27176`) | `27088-27236` | R3 |
| forget | ino, nlookup | none | never replies | `30322-30348` | R3 |
| batch_forget | `[(ino, nlookup)]` | none | FUSE-3k: per-entry nlookup | `30357-30384` | R3 |
| getattr | ino, fh?, flags | `ReplyAttr{ttl, attr}` | ESTALE for a retired virtual gen | `27238` | R3 |
| setattr | ino, fh?, `SetAttr{mode,uid,gid,size,atime,mtime,ctime,…,kill_suidgid}` | `ReplyAttr` | size ⇒ exclusive guard; times-only echo absorbed (economy) | `28437` | R3 |
| readlink | ino | `ReplyData` | — | `28730` | R3 |
| symlink | parent, name, link | `ReplyEntry` | ENAMETOOLONG; target ≤ 4095 | `28643` | R3 |
| mknod | parent, name, mode, rdev | `ReplyEntry` | — | `27301` | R3 |
| mkdir | parent, name, mode, umask | `ReplyEntry` | — | `28336` | R3 |
| unlink | parent, name | () | — | `28812` | R3 |
| rmdir | parent, name | () | ENOTEMPTY | `28385` | R3 |
| rename / rename2 | parent, name, new_parent, new_name [, flags] | () | NOREPLACE ⇒ EEXIST, EXCHANGE, WHITEOUT | `28862, 28958` | R3 |
| link | ino, new_parent, new_name | `ReplyEntry` | EMLINK at 65 000 | `28757` | R3 |
| open | ino, flags, open_flags | `ReplyOpen{fh, flags}` | EROFS write intent on a reader; ENOENT if reclaim claimed the slot (`27588-27592`); O_TRUNC routed through setattr(size 0) (`27560-27578`); virtual inos `FOPEN_DIRECT_IO` | `27451-27604` | R3 |
| read | ino, fh, offset, size, flags | `ReplyData{data, backing, zc_prefilled, zc_fd_body}` | size clamp; short read = EOF | `27631` | R3 inline / R4 |
| write | ino, fh, offset, `Bytes`, write_flags, flags | `ReplyWrite{written}` | EFBIG; ENOSPC counted `write_enospc_refusals` | `27674-28334` | R3 inline / R4 |
| statfs | ino | `ReplyStatFs{blocks,bfree,bavail,files,ffree,bsize,namelen,frsize}` | — | `29791` | R3 |
| release | ino, fh, flags, lock_owner, flush | () | errors never reach close(2) | `29931-30062` | R3 |
| fsync | ino, fh, datasync | () (trait default `Ok`) | — | `30064` | R3 |
| setxattr | ino, name, value, flags, position | () | — | `30606` | R3 |
| getxattr | ino, name, size | `ReplyXAttr::Size(n)` for a size probe (a SUCCESS with `fuse_getxattr_out`; "ERANGE here is a malformed reply", `session.rs:3433-3443`) or `Data(bytes)`; `Err(ERANGE)` only when the caller's buffer is too small | `30686` | R3 |
| listxattr | ino, size | same | `30785` | R3 |
| removexattr | ino, name | () | — | `30826` | R3 |
| flush | ino, fh, lock_owner | () | **ENOSYS on a clean handle** = the kernel's `fc->no_flush` latch (`29891-29896`); latched writeback error outranks it | `29852-29929` | R3 |
| opendir | ino, flags | `ReplyOpen` | — | `27606` | R3 |
| readdir | parent, fh, offset: i64 | `ReplyDirectory{entries: Stream}` → `fuse_dirent` buffer ≤ `size` | ENOSYS under `force_readdir_plus` | `29064` | R3 |
| readdirplus | parent, fh, offset, lock_owner | `ReplyDirectoryPlus{Stream}` (each entry is a lookup reference) | — | `29138` | R3 |
| releasedir | ino, fh, flags | () (default `Ok`) | — | `27620` | R3 |
| fsyncdir | ino, fh, datasync | () | — | `30186` | R3 |
| access | ino, mask | () | never called: `default_permissions` is always set (`fuse_client.rs:31511`) | default ENOSYS | R3 (ENOSYS) |
| create | parent, name, mode, flags | `ReplyCreated{ttl, attr, generation, fh, flags}` | — | `27354` | R3 |
| interrupt | unique | () — no reply ever | — | default | R3 (ignored) |
| fallocate | ino, fh, offset, length, mode | () | — | `30209` | R4 |
| lseek | ino, fh, offset, whence | `ReplyLSeek{offset}` | — | `29703` | R4 |
| copy_file_range | ino, fh_in, off_in, ino_out, fh_out, off_out, len, flags | `ReplyCopyFileRange{copied}` | — | `29257` | R4 |
| ioctl | ino, fh, flags, cmd, arg, in_size, out_size | `ReplyIoctl` | GDS only | `30397` | R4 |
| getlk / setlk | — | — | never: `FUSE_POSIX_LOCKS`/`FLOCK_LOCKS` never advertised | — | never |
| bmap, poll, notify_reply | — | — | default ENOSYS | — | never |
| `zc_write_hold_eligible` (sync) | ino, offset, len, odirect | bool (default false) | queue-worker thread; a stale `true` extracts late once (`fuse3_zc_write_lazy_extractions`) | `26231` | R4 |
| `read_fast_probe` (sync) | ino, fh, offset, size, flags, dest | `FastReadProbe` (default Demote) | §1.10 | `26345` | R4 |

Encoding notes the port must honour: `fuse_entry_out` carries `entry_valid`/`attr_valid`
as `(secs, nsecs)` from one `ttl` (`reply.rs:146-160`); `fuse_attr` seconds are cast
from `i64` (`reply.rs:60-66`); `fuse_dirent.type = mode >> 12` (`session.rs:3834`);
dirents are 8-byte padded and the loop stops at the first entry that would exceed
`size` (`session.rs:3820-3845`).

R: `ReplyCreated → (fuse_entry_out, fuse_open_out)` writes `subsec_MICROS()` into the
`*_valid_nsec` fields (`reply.rs:433-434`) while `ReplyEntry` writes `subsec_nanos()`
(`reply.rs:154-156`) — a 1000× TTL under-report on every create. PORT: one encoder for
the entry body shared by lookup/create/mknod/mkdir/symlink/link/readdirplus.
R: the dirent size check excludes the padding (`session.rs:3824-3826` then `3843`), so a
reply can exceed `size` by up to 7 bytes. PORT: check `aligned(dirent_size) ≤ remaining`.
R: `readdir` is a `Stream` the session awaits per entry and re-encodes into a `Vec`
("TODO find a way to avoid multi allocate", `session.rs:3847`). PORT: the handler fills
`fuse_dirent` records straight into the reply window through a `DirentSink` with a
remaining-bytes bound; the KV range scan (`KvBackend.range`) yields cookie-ordered
dentries (`3 + (hash54 << 8 | coll)`, charter §5.1).

---

## 6. The deadline watchdog

**What it watches**: the op registry — every handler claims a slot at entry
(`OpProf::begin`, `fuse_client.rs:1817-1833`): `OpSlot{state: CAS 0→1, kind, ino,
start_ns, station}` `#[repr(align(64))]` ("a FAILING `lock cmpxchg` still takes the
line exclusive", `1442-1461`). Geometry `op_registry_derived_geometry(possible_cpus) =
(max(possible_cpus, ceil(256 / (2 × Q_DEPTH_DESIRED))), 2 × Q_DEPTH_DESIRED)`
(`1418-1422`): shards = kernel possible CPUs (the queue population), slots/shard =
2 × the depth ceiling (headroom for sideband/ipc registrations), total ≥ 2× the max
delivered ring capacity, never below the shipped 256. WHY: the fixed 256-slot slab was
4× under the field rig's 1024 in flight; every claim walked 256 failing CASes from one
global cursor — 6.65 % of fused-worker self-cycles at 525 k IOPS (`1397-1417`). Claims
are thread-AFFINE (home shard first, round-robin assigned) but any thread may release
(futures migrate; `1481-1485, 1527-1558`). Exhaustion ⇒ the op runs unregistered
(`1551`). The op's start is the COARSE clock `COARSE_NOW_NS`, refreshed once per tick
— zero clock syscalls on the per-op path ("the whole point of retiring `timeout()`'s
timer-wheel + vdso tax, 2.75 % of daemon cycles", `1667-1674`); staleness ≤ one tick
overstates ages by ≤ 5 s against a 30 s threshold. Stations (`op_station`, `1773-1802`):
`entry, inode-lock-held, orphan-probe-done, lease-held, meta-fetched, route-dispatched,
device-dma-done, publish-done, replying, payload-materialized, dio-coherence-entered` —
one relaxed store each, so an overdue report names the await it is parked at.

**Thresholds**: `get_fuse_timeout()` = `SQUEEZEFS_TIMEOUT` seconds, default 30
(`197-211`); tick `OP_WATCHDOG_TICK = 5 s` (`2140`; design §5.1 D1.b "one per-daemon
task ticks every 5 s").

**What it logs** (`op_watchdog_tick`, `1988-2047`): per overdue slot an `error!` "FUSE op
watchdog: {op} (ino {ino}) in flight for {age} ms (> {threshold} ms), parked past
station [{station}] — op is NOT cancelled (D1.b semantics: no ETIMEDOUT synthesis)";
counts `fuse_op_watchdog_overdue` per OBSERVATION (one per scan per stuck op, "so a
still-stuck op keeps the signal alive", `1977-1980`). Exception: an op whose station
precedes `PUBLISH` and whose age ≤ `appender park age + threshold` is WARNed as PARKED
(the symmetric park-and-reclaim, never counted overdue; `2010-2029`). On any overdue
the task also prints the named-holder lock census (`log_lock_wait_census`), the write
phase census and the one-line **wedge census** built from process-global atomics only
("readable from the watchdog task no matter what is wedged — the zcws-9 W4 capture had
16 k overdue lines, a 3-entry lock census, and a `.stats` read that hung", `2142-2205`):
conveyor passes/queued/windows/durability passes, commit/publish parked, journal
entries, checkpoints, pipeline permits/waits, reclaim bytes, open epochs, leases
outstanding, parked/unparked commits, `lock_ticked_reregisters`,
`lane_exec_tick_rescues`, `lane_exec_task_panics`. The task runs on `sqz-meta`
(`spawn_meta("op_watchdog")`, "must survive a broken scheduler — it IS the wedge
instrument", `2215-2244`), spawned once per process from FUSE `init`.

**Per-op timeout wrappers are BANNED — why** (the D1.b audit, `440-496`): `timeout()`
expiry DROPS the handler future at whatever await it is parked on; reachable through
every mutation handler is `commit_tx`, whose accounting is unforgiving — an
`Admission` dropped between ring admission and `reserve_registered` leaks ring budget
forever, a registered `Reservation` whose future dies before `complete()` permanently
stalls `completed_upto` and wedges every later committer. "The 30 s per-op timeout was
therefore a live volume-wedge vector on any commit that crossed the threshold." The
audit's six await classes and their dispositions: (1) fsync/fsyncdir barrier waits →
bounded INSIDE the coalescer (the one drop-safe layer); (2) ring-admission parking →
watchdog + escalation (cumulative park ≥ threshold trips `note_journal_failure`;
3 crossings latch FAILED → EIO; another committer's success resets); (3)
`wait_completed_upto` → watchdog-only; (4) demand-paged node reads → watchdog-only;
(5) stripe/DLM locks → watchdog-only ("deadlock-freedom is the lock-order contract's
job"); (6) data-path device I/O → watchdog-only (the old `max(TIMEOUT, 30 s)` read/write
timeouts "synthesized ETIMEDOUT while leaving the device wedged"). Kept bounded waits
that are NOT per-op timeouts: destroy's staged-drain, the 2 s device health probe, the
DLM lease acquire's 5 s, the reclaim gather window.

PORT: the registry, stations, coarse clock, 5 s tick, overdue log and wedge census carry
over; the threshold is the documented constant `conveyor.PARK_ESCALATION_NS = 30 s`
(decision 0002 row 17 retired `SQUEEZEFS_TIMEOUT`; `src/meta/conveyor.zig:131`) — ONE
number for "a park this long is a volume event". Device reads already wait unbounded
and `Device.overdueCensus` feeds the same tick (row 34). Overdue reporting never
touches a reply.

---

## 7. `op_trace` and `SQUEEZEFS_OP_PROFILE`

### 7.1 `op_trace` (always compiled, armed by `SQUEEZEFS_OP_TRACE`)

Storage (`op_trace.rs`): a leaked `Pool{gen, rings: [TraceRing], pushed[], dropped[],
dropped_unringed, next_ring, threshold, divisor, epoch_instant, epoch_mono_ns}`
published through ONE `AtomicPtr POOL` (non-null ⇔ armed; "pools are leaked so a raw
pointer read is valid for the process lifetime", `132-135`). `arm(cfg)` reuses a pool of
equal geometry (updating threshold/divisor in place — an admin toggling never leaks),
else allocates a fresh one; rings are allocated HERE, never on the hot path
(`160-187`). `disarm()` nulls the pointer; undrained samples stay readable (`192-194`).

**Zero cost off — how** (`14-27`): disarmed, `stamp / stamp_now / traced` are ONE
relaxed-class load of the pool pointer (null ⇒ return); `stamp_current` is one
thread-local read (0 ⇒ return); `OpScope::poll` is a plain field compare (`op_id == 0 ⇒
fut.poll`, `385-387`). No clock read, no allocation, no RMW. Armed: one multiply (the
selection), one thread-local ring-index read, three relaxed slot stores + one Release
head store, one relaxed per-ring counter RMW; a full ring DROPS and counts
(`op_trace_dropped`), never parks or allocates.

Sampling law (`op_trace_core.rs:519-538`): `selected(op_id, threshold) = (op_id ×
0x9E3779B97F4A7C15 >> 32) < threshold`, `threshold = u32::MAX / divisor` (1 ⇒ all) —
deterministic per id so every hook on one op agrees (a traced op's chain is complete),
stride-independent, one multiply. The divisor is DERIVED by the daemon
(`src/op_trace.rs:110-125`): `rings = clamp(cpus × 8, 16, 4096)`; `pool_bytes =
max(budget >> 10, rings × 1024 × 24)`; `ring_capacity = prev_pow2(pool_bytes / rings /
24)`; `divisor = ceil(cpus × 40 000 × 32 × 1 / (rings × ring_capacity))` — the smallest N
such that one 1 s drain interval of the machine's op ceiling at the longest chain fits
the pool. Constants: `OPS_PER_CORE_PER_S = 40 000` (measured 1.03–1.04 M IOPS flat on
32 cores ≈ 32 k/core, rounded up), `STAGES_PER_OP = 32` (24 longest chain → next pow2),
`DRAIN_INTERVAL_S = 1`, `SAMPLE_BYTES = 24`, `RING_MIN_CAPACITY = 1024` (32 longest
chains — "the physical minimum, never a tuning floor") (`src/op_trace.rs:52-73`). The
pool is an R5 component (`op_trace_rings`, non-sheddable, weight 1, floor 0).

The ring (`op_trace_core.rs:424-514`): Lamport SPSC over `[AtomicU64; 3]` slots; the
producer's Release store of `head` publishes the three words to the consumer's Acquire
load — weakening either drains an unwritten `(0,0,0)` (loom-verified 2026-09-02); the
producer's Acquire load of `tail` against the consumer's Release store is the reuse
edge (stands on the Lamport argument, not a model).

The scope (`op_trace.rs:339-397`): `scope_with_entry(unique, Some(HandlerEntry), fut)`
binds `CURRENT_OP` (a `Cell<u64>` swap per poll, restored on every exit path including
a panicking poll via `Restore`), stamps `handler_entry` on the first poll; detached work
carries the originating op id on its unit and re-enters a scope where it runs;
`TracedBatch` (cap 8, `399-452`) stamps pass-level stages for every traced member of a
conveyor batch. `drain()` sorts by `(op_id, mono_ns)` and empties the rings under the
consumer lock (`457-467`); the `.trace` lookup drains (`fuse_client.rs:27166-27176`).

**Stage vocabulary** (`op_trace_core.rs:62-283`, snake names `330-406`; each stage is
the END of the phase named beside it, so per-op deltas ARE the phase spans):

| Family | Stages (id) |
|---|---|
| transport | `transport_recv` 1, `dispatch` 2, `handler_entry` 3, `handler_return` 4, `reply_commit` 5, `fast_dispatch` 6 |
| read serve | `read_routed` 16, `meta_resolved` 17, `keys_resolved` 18, `tier_probed` 19, `cohort_served` 20, `block_fetched` 21, `binding_checked` 22, `serve_copied` 23, `read_validated` 24, `read_return` 25 |
| device + fill | `dev_submit` 32, `dev_complete` 33, `fetch_done` 34, `decoded` 35, `admitted` 36, `deposited` 37, `fill_done` 38, `bridge_sent` 39, `bridge_taken` 40 |
| write pipeline | `write_admitted` 48 … `write_done` 57 |
| publish conveyor | `publish_enqueue` 64 … `publish_done` 76 |
| meta journal conveyor | `meta_enqueue` 80, `pass_begin` 81, `pass_admitted` 82, `pass_locked` 83, `journal_written` 84, `journal_prefix_done` 85, `barrier` 86, `pass_end` 87, `fanout` 88 |
| metadata op | `meta_backend_start` 96, `meta_backend_done` 97, `meta_op_return` 98 |
| il ring | `ipc_ingress` 112 … `ipc_complete` 117 |
| uring_fs write | `ufs_submit` 120 … `ufs_observed` 123 |
| locks | `stripe_lock_acquired` 128, `stripe_lock_released` 129, `dlm_guard_acquired` 130, `dlm_guard_released` 131 |

Join recipe (`op_trace.rs:53-80`): `mono_ns` is CLOCK_MONOTONIC — the clock
`bpftrace`'s `nsecs` and `perf record -k CLOCK_MONOTONIC` use, so the `unique`-keyed
join against `fuse:fuse_request_send/end` is a MEASUREMENT (`tests/op_trace_stitch.py`).

PORT: the port's `core/clock.monotonicNs` IS CLOCK_MONOTONIC, so the `(Instant,
mono_ns)` epoch-pair conversion (`ArmConfig.epoch_*`, `op_trace.rs:103-108`) is
unnecessary — stamps are the clock. The stage table is a `comptime` enum with the same
ids (the wire vocabulary `.trace` ships). The "current op" is a thread-local the lane
sets around each handler poll.

### 7.2 `SQUEEZEFS_OP_PROFILE` (per-op phase histograms)

Gate `op_profile_enabled()` memoized once (`fuse_client.rs:530-533`; `std::env::var`
takes the process env lock and allocates — never per op). Off: `OpProf::begin` for a
data op is a memoized load + the registry CAS, no clock read (`1817-1833`; the M2 cost
contract, `519-524`). On: four `Instant` reads per op — `t0` at entry,
`mark_backend_start` (first backend/router touch), `mark_backend_done` (`commit_tx`
return), drop — recorded into `fuse_op_phase_ns[op][phase]` with phases
`handler_to_backend, backend, backend_to_reply, total` (`1384-1390`, `record_op_phases`
`1737-1755`, drop-based so error exits record truthfully). The METADATA op classes
(`META_OP_KINDS` = lookup, getattr, setattr, mkdir, create, unlink, rename, readdir,
readdirplus, `1691-1701`) carry the stamps ALWAYS into the ungated `meta_op_phase_ns`
(`entry_to_backend, backend, backend_to_reply, total`, `1712-1718`) and the same reads
feed the op-trace `meta_backend_start/done/meta_op_return` stamps (`1870-1875,
1881-1886, 1923-1926`). Also profile-only: `fuse_create_under_lock_ns` — the preceding
LOOKUP (paired by `xxh3(parent ‖ name) | 1` in a 1024-slot evict-on-collision table,
`1582-1624`) to CREATE-reply span, the direct measurement of the under-`i_rwsem` cost
model (`1889-1913`). The stats JSON fields `fuse_op_phase_ns`,
`fuse_create_under_lock_ns`, `fuse_op_profile_inflight`, `fuse_write_phase_ns`,
`block_lock_wait_by_site`, `block_lock_stripe_audit`, `fuse_write_inflight` exist ONLY
when armed — "a disabled mount's stats surface is byte-identical" (`14774-14800`).

Transport-side always-on families (`read_phase.rs`): `read_transport_phase_ns` /
`write_transport_phase_ns` with phases `queue_wait, dispatch_lag, reply_commit,
transport_total, commit_flush` (`42-73`), sharded per recording thread (`shard_count =
clamp(next_pow2(available_parallelism), 1, 64)`, `172-181`; PERF-3: a tight
distribution puts every op in the SAME bucket word — true sharing, not false), exact
`count`/`sum_ns` words beside the buckets (`85-123`); `transport_reap_gap_ns` (`blind,
blind_cqe, park`, `535-545`); `zc_bridge_phase_ns` (`msg_hop, sq_wait, device_cq,
wake_hop, total`, `672-689`); the fused timeline (`wake_to_poll, bridge_rtt` +
`fused_{midpass,passbottom}_reaps`, `458-497`). Error replies record nothing
(`read_phase.rs:31`).

PORT: `counters.ShardedLatencyHistogram` (exact-sum) for every family; the profile gate
is `knobs.boolKnob(SQUEEZEFS_OP_PROFILE)` read once at mount into a `comptime`-typed
`bool` the handler prologue branches on; the `Instant` reads are `clock.monotonicNs`.

---

## 8. Open count, FORGET, `open_count_stranded`

`open_inodes: DashMap<ino, OpenEntry{count, dirty: AtomicBool, lookups}>`
(`fuse_client.rs:9454, 8739-8752`). `add_open` (open/opendir/create; `count 0→1`
clears `dirty` — a fresh open generation starts clean, `9965-9974`), `remove_open`
(release/releasedir, saturating at 0, `9976-9982`), `is_open` (`9984-9990`).
`note_lookup(ino)` records ONE kernel lookup reference per entry reply the daemon sends
— LOOKUP, CREATE, MKNOD, MKDIR, SYMLINK, LINK, each READDIRPLUS entry; root (`ino ≤ 1`)
never counted (`9992-10006`). `return_lookups(ino, nlookup)` (`10025-10053`):
`n = max(nlookup, 1)` ("the kernel never sends 0; treat it as one so a malformed forget
can never make an inode unevictable"); `lookups −= n` saturating; evict iff it reached 0
(an UNTRACKED inode — `lookups == 0` before — evicts on its first forget: a miscount
can only cost cache retention, never correctness, `8747-8750`); **POSIX-14**: reaching
zero is the kernel certifying no `struct file` is open (it cannot evict an inode whose
`i_count` a file holds), so `count > 0` here is a LOST RELEASE (a panicked handler:
FUSE-2 synthesized the reply but `remove_open` never ran) — `open_count_stranded += 1`,
`error!`, and the count is NOT zeroed ("an unlinked-open file's live data must never be
destroyed on the strength of an accounting we already know is wrong — the generic/795
lesson"); the entry is removed only if `lookups == 0 && count == 0`.

`forget` (`30322-30348`): virtual gen ino ⇒ retire its payload; `!return_lookups ⇒
return` (a partial forget — `fuse_force_forget(1)` on a readdirplus entry the kernel
could not link — returns one reference while the kernel keeps the rest);
else `attr_cache.invalidate`, `forget_side_maps` (RES-13: FORGET bounds every
per-inode side structure), `queue_reclaim_inode`. `batch_forget` = exactly N forgets
with the per-entry nlookup (`30357-30384`; the trait default was a NO-OP that "leaked
every batch-evicted orphan's inode slot for the LIFE OF THE MOUNT"). `queue_reclaim_
inode` (`10222-10234`): skip `ino ≤ 1`/virtual, skip on a `-o ro` mount (counted
`reclaim_reader_forgets`), skip while `is_open`, else the task-free `ReclaimEnqueue`
(RES-20: spawns nothing per FORGET). Reclaim is per-ino single-drive (`reclaim_inflight`
guard: duplicate drives double-freed blocks and stole offsets from new owners,
`9476-9489`). The open/reclaim handshake (`27580-27592`): open counts FIRST, then refuses
`ENOENT` if a reclaim already claimed the slot; the reclaim side claims then re-checks
the count — every interleaving safe.

Release (`29931-30062`): never-dirtied handle ⇒ `fuse_release_clean_fastpath` (no
lease, no flush task); dirty ⇒ a BACKGROUND flush on `spawn_bg` whose results latch
for the next fsync/flush (POSIX-16, never dropped); `remove_open` FIRST so "last close"
is exact, then the lease/range leases drop only at the last close (releasing earlier
fenced siblings into `FencingTokenExpired` storms — generic/464); `queue_reclaim_inode`.

The transport's side: FORGET/BATCH_FORGET ents are committed by the queue worker at
delivery (`fuse_over_uring.rs:7283-7329` — "this IS its one commit"); the session only
accounts. On a lost session the accounting is lost but the ent still commits (`7301-
7309`). Teardown's final FORGET sweep is where `open_count_stranded` fires (charter §4:
"a final FORGET with open handles counted = a lost RELEASE").

PORT: `OpenTable` = a concurrent map keyed by ino (sized once from the geometry, no
per-op allocation) with `{count: u32, lookups: u64, dirty: bool}`; the same three laws
(count-first handshake, saturating returns, never zero a stranded count). Zig has no
unwinding, so a "lost RELEASE" can only come from a logic error — the tripwire stays and
must read 0 on every teardown.

---

## 9. Every knob read by this layer

| Knob | Kind / default | Read where | Class | PORT |
|---|---|---|---|---|
| `SQUEEZEFS_TIMEOUT` | Int s, 30 | `get_fuse_timeout`, `fuse_client.rs:197-211` (memoized) | tuning | deleted → `conveyor.PARK_ESCALATION_NS` (decision 0002 row 17) |
| `SQUEEZEFS_OP_PROFILE` | Bool, off | `op_profile_enabled`, `530-533` | switch | kept; rung 5 `.stats` (ledger) |
| `SQUEEZEFS_OP_TRACE` | Bool, off | `op_trace::arm_from_env`, `src/op_trace.rs:189-190` | switch | kept |
| `SQUEEZEFS_STATS_KEY_CENSUS` | Bool, off | `stats_key_census_enabled`, `566-568` (live, not memoized) | switch | kept; rung 5 |
| `SQUEEZEFS_WRITE_SHARED` | Bool, on | `write_shared_enabled`, `4813-4824` | A/B lever | kept (the one lever; §2.8) |
| `SQUEEZEFS_WRITE_GUARD_NARROW` | Bool, on | `write_guard_narrow_enabled`, `4849-4858` | A/B lever | deleted (§2.8) |
| `SQUEEZEFS_FUSE_SAME_LANE_DISPATCH` | Bool, on | `same_lane_dispatch_enabled`, `session.rs:6239-6252` | A/B lever | deleted — home-lane dispatch is the law (§1.11) |
| `SQUEEZEFS_FUSE_READ_FAST_DISPATCH` | Bool, on | `fast_dispatch_enabled`, `fast_dispatch.rs:130-143` | A/B lever | kept (transport spec owns it) |
| `SQUEEZEFS_FUSE_ZC_READ_FUSION` | Bool, OFF (measured) | fused lane | A/B lever | kept (transport) |
| `SQUEEZEFS_FUSE_ZC_WRITE_FUSION` | Bool, on | fused lane | A/B lever | kept (transport) |
| `SQUEEZEFS_FUSE_PIN_SCOPE` | Enum node/core, node | `TpcScheduler::new`, `session.rs:6005-6014` | enum select | kept |
| `SQUEEZEFS_FUSE_{ATTR,ENTRY,DIR_ENTRY,NEGATIVE}_TTL_MS` | Int ms, 1000 (mount options override) | `kernel_ttls`, `fuse_client.rs:9455-9458` | tuning | the four per-class TTLs stay as mount options; the env rows become derived defaults with the `-o` override (charter §4) |
| `SQUEEZEFS_TEST_ROUTER_DISPATCH_STALL_MS` | Int, 0 | `fuse_client.rs:28143-28154` | test seam | kept |
| `SQUEEZEFS_TEST_ATTR_PUBLISH_STALL_MS` | Int, 0 | `28250-28257` | test seam | kept |

R: `patch_max_bytes_cell` parses `SQUEEZEFS_PATCH_MAX_BYTES` with a bare
`trim().parse()` (`fuse_client.rs:581-590`) — a second parser beside the registry
(decision 0002 row 14 class). PORT: the ONE parser.

---

## 10. Gauges, counters, histograms

| Name | Kind | Meaning | Must-stay-0 | Source |
|---|---|---|---|---|
| `fuse_ops` | counter | every handler entry | — | `fuse_client.rs:11278` |
| `fuse_op_watchdog_overdue` | counter | overdue OBSERVATIONS (one per scan per stuck op) | 0 healthy | `11279`, `2036-2038` |
| `fuse_op_profile_inflight` | gauge (armed only) | live registry slots | — | `1958-1960` |
| `fuse_op_phase_ns` / `meta_op_phase_ns` | hist `[op][phase]` | §7.2 | — | `11958, 14780` |
| `fuse_create_under_lock_ns` | hist (armed) | LOOKUP arrival → CREATE reply | — | `14782` |
| `fuse_release_clean_fastpath`, `fuse_flush_clean_fastpath` | counter | never-dirtied handle elisions | — | `11281, 29892-29894` |
| `fuse_lookup_negative_replies` | counter | ENOENT → negative entry | — | `11282` |
| `writeback_orphan_discards` | counter | verified-reclaimed ino write acked as a discard | — | `11765` |
| `write_enospc_refusals` | counter | WRITE replied ENOSPC | — | `11576` |
| `write_lock_wait` (+ `_shared`, `_exclusive`) | hist | guard acquisition wait (aggregate kept) | — | `12627, 12639-12640` |
| `write_lock_candidate_{shared,metaprep,entire}` | counter | classifier verdict (preview ledger) | — | `12632-12634` |
| `write_lock_scope_{shared,metaprep,entire}` | counter | FINAL scope held; closure Σ ≡ WRITE ops | — | `12635-12637` |
| `write_lock_scope_shared_upgrades` | counter | the ONE upgrade fired | 0 on fully-mapped rows; nonzero around truncate storms = working | `12638` |
| `write_lock_hold_{shared,metaprep,entire}` | hist | guard hold; Σ hold ≡ Σ final scope | — | `12643-12645` |
| `open_count_stranded` | counter | final FORGET with open handles = lost RELEASE | **0** | `11411` |
| `tpc_lane_redispatches` | counter | dispatches re-routed off a dead lane | **0** | `session.rs:6203-6208` |
| `lane_exec_tick_rescues`, `lane_exec_task_panics` | counter | lane executor backstop / completed-by-panic | ≈0 / 0 | `11633-11634` |
| `lock_ticked_reregisters` | counter | 2 s tick absorbed a lost wake | 0 healthy | `11606` |
| `fuse3_read_inplace_replies`, `fuse3_write_inplace_replies` | counter | engagement: ≈ every armed READ/WRITE | — | `12026, 12112` |
| `transport_fast_dispatch_serves`, `transport_fast_dispatch_demotes` | sharded counter | §1.10 partition | — | `12020-12021` |
| `fuse3_zc_read_fusions`, `fuse3_zc_read_fusion_demotions` | counter | arm (b) / refused at capacity | — / ≈0 | `12084-12085` |
| `fuse3_notify_enoent`, `fuse3_notify_failed` | counter | notify write outcomes | — | `12117-12118` |
| `transport_requests_failed_synthetic` | counter | guard-synthesized EIO + direct commits | — (honest count) | `12353` |
| `transport_requests_abandoned` | counter | no path could answer | **0** | `12354` |
| `transport_replies_refused_stale`, `_dropped_no_slot`, `_oversize`, `transport_slots_overdue` | counter | FUSE-3 integrity | — | `12355-12358` |
| `read_transport_phase_ns`, `write_transport_phase_ns`, `transport_reap_gap_ns`, `zc_bridge_phase_ns`, fused timeline | sharded hist | §7.2 | — | `11996-12012` |
| `op_trace_{armed,samples,dropped,divisor}` | gauge | §7.1 | — | `11973-11976` |
| `invariant_tripwires` | counter (per-site label) | every "cannot happen" | **0** | `11600` |
| `daemon_cpu_ns_by_class` | gauge map | CPU per thread class (the naming contract) | — | `11967` |

Closure identities to test: `write_lock_scope_* Σ ≡ WRITE ops`; `Σ hold ≡ Σ final
scope`; `serves + fusions + demotes ≡ READs`; `fusions + demotes ≡
fuse3_read_inplace_replies`; `queue_wait.count ≡ dispatch_lag.count ≡ READ population`.

---

## 11. Constants with WHY and measured numbers

| Constant | Value | WHY | Source |
|---|---|---|---|
| `INODE_GUARD_STRIPES` | 4096 | shipped width; "data-path holds, not commit parks" (never-regress floor) | `fuse_client.rs:2275-2277` |
| `BLOCK_LOCK_STRIPES` | 4096 | one block's checkout/flush hold; `block_flush_stripe_collisions` would convict it | `2268-2273` |
| `FUSE_NAME_MAX` | 255 | POSIX `NAME_MAX`; `f_namelen` and every name op (ENAMETOOLONG) | `2252-2266` |
| `PATH_MAX_WITH_NUL` | 4096 | symlink target ≤ 4095 | `2135` |
| `OP_WATCHDOG_TICK` | 5 s | design D1.b; also the coarse clock refresh ⇒ age error ≤ one tick | `2137-2140` |
| watchdog threshold | 30 s | `SQUEEZEFS_TIMEOUT` default; PORT: `PARK_ESCALATION_NS` | `197-211` |
| `OP_REGISTRY_SHIPPED_SLOTS` | 256 | never-regress floor; field rig needed 1024 (32 CPUs × depth 32) | `1392-1417` |
| registry geometry | `(max(possible_cpus, ceil(256/(2·Q)), 2·Q_DEPTH_DESIRED)` | shadows delivered concurrency; 6.65 % of worker cycles at 525 k IOPS otherwise | `1418-1422` |
| `OpSlot` align | 64 | ~2.7 packed slots per line made a FAILING CAS take the line exclusive | `1442-1447` |
| `LOOKUP_PAIR_SLOTS` | 1024 | pairing distance in a create storm is one op | `1582-1590` |
| `sqz_sync::TICK`, `sqz_exec::TICK` | 2 s | liveness backstop, unreachable by healthy waits | `sqz_sync.rs:35-39`, `sqz_exec.rs:51` |
| `fusion_ceiling(payload)` | `max(payload/8, 4096)` | handler per-poll CPU bounded at small sizes; 4096 the physical floor | `fused.rs:393-401` |
| core 0 reserved | when `cpus > 1` | OS kernel tasks | `session.rs:5955-5957` |
| `shard_count()` | `clamp(next_pow2(cpus), 1, 64)` | 256-core host must not spend 3 MiB on histograms | `read_phase.rs:168-181` |
| `NOTIFY_FAILED_CLASSES` | 8 | a notification's errnos are a handful (ENODEV, EBADF, EINVAL) | `read_phase.rs:386-388` |
| `TracedBatch::CAP` | 8 | fixed-size; members past the cap are a sampling artifact, never an allocation | `op_trace.rs:403-417` |
| `OPS_PER_CORE_PER_S` | 40 000 | 1.03–1.04 M IOPS on 32 cores ≈ 32 k/core, rounded up | `src/op_trace.rs:52-55` |
| `STAGES_PER_OP` | 32 | 5 + 10 + 7 + 2 = 24 → next pow2 | `57-60` |
| `RING_MIN_CAPACITY` | 1024 | 32 longest chains — physical minimum | `69-73` |
| convoy | 1.12 ms/op at 32 × qd32, devices qd ≈ 1.5 | §2.7 | design §1, §3 |
| Shared A/B | 523/530 k ON vs 489/491 k OFF (+7–8 %) | §2.7 | `fuse_client.rs:4807-4810` |
| READ ingress killed by fast dispatch | `queue_wait` 69 µs + `dispatch_lag` 89 µs of a 439 µs 4 KiB random read (36 %); device 40 µs; bench 6.7 µs hop vs 4.0 µs mint vs 53 ns inline | `fast_dispatch.rs:10-15` |
| zc bridge wakes | three cross-thread wakes of 35–47 µs each around one 40 µs DMA | `fast_dispatch.rs:54-58` |
| env read cost | `std::env::var` per op ≈ 0.7 % of daemon cycles | `fuse_client.rs:198-201` |
| `timeout()` tax | timer-wheel + vdso 2.75 % of daemon cycles | `1667-1673` |
| ENOENT notify WARN storm | 272 k / 275 k lines per row set, a 46 % / 21 % subset of invals + prunes | `session.rs:100-105` |

---

## 12. Tests (name — one sentence; ⚑ = needs a real mount)

Session / transport side (`crates/fuse3/src/raw/session.rs`):
* `dropping_an_owing_handle_synthesizes_the_reply` (6677) — an owed `ReplyTx` dropped produces one `-EIO` header.
* `a_replied_handle_synthesizes_nothing_on_drop` (6699) — `mark_replied` disarms the guard.
* `reply_error_in_place_is_the_one_reply` (6723) — the error reply consumes the obligation.
* `a_no_reply_handle_stays_silent` (6742) — FORGET/INTERRUPT handles synthesize nothing.
* `a_dead_reply_task_is_reported_not_ignored` (6756) — a closed channel falls to the direct commit / abandoned tripwire.
* `short_notify_reply_body_is_rejected_not_split`, `short_batch_forget_body_is_rejected_not_split` (6800, 6830) — dispatch-task slices never panic.
* `a_header_shorter_than_the_in_header_never_underflows`, `an_overdeclared_body_is_refused_not_clamped_over_stale_bytes`, `the_write_lease_body_rides_the_payload_and_bounds_the_data_slice`, `the_data_buffer_capacity_is_the_last_bound` (6865-6932) — the FUSE-3g bounds table.
* `init_reply_never_advertises_posix_locks` / `_flock_locks` / `_splice` / `_posix_acl` (6985-7054) — capability negotiation pins (transport spec).
* `dead_lane_redispatches_loudly_healthy_lanes_never` (7432) — healthy lanes never pay the counter; a dead lane re-routes loudly.
* `tests/tpc_tests.rs::test_tpc_scheduler_threads_and_no_migration` — a lane task never migrates threads across yields/sleeps.
* `tests/tpc_scheduler_tests.rs::test_tpc_pool_sizes_from_process_affinity_despite_pinned_first_touch` — pool width from the PROCESS mask even when a 1-CPU-pinned thread touches first (own binary).
* `fast_dispatch.rs::candidate_is_read_and_lever_and_registration`, `record_served_accounts_zero_ingress_and_the_exact_total` (223, 245).
* `read_phase.rs::families_are_phase_exact_and_independent`, `shard_fold_is_exact_to_the_ns`, `reap_cadence_records_blind_park_and_per_cqe_weight_exactly`, `zc_bridge_family_is_phase_exact_and_folds_across_threads` (726-905).
* `op_trace.rs::disarmed_is_inert_and_armed_records`, `scope_binds_and_restores_even_across_a_panicking_poll`, `pool_is_reused_on_same_geometry_and_counts_drops`, `traced_batch_stamps_every_member` (521-630).
* `reply.rs::negative_entry_encodes_nodeid_zero_with_entry_ttl`, `positive_entry_keeps_nodeid` (554-578).

Guard / admission (`tests/write_lock_scope_tests.rs`, `write_shared_scope_tests.rs`, `write_stream_guard_tests.rs`, `attr_publish_tests.rs`):
* `narrowed_class_admits_every_resident_striped_shape`, `striped_fully_mapped_within_floor_is_shared`, `every_doubt_routes_to_a_write_guard_class`, `folded_staged_bypass_and_entireop_arms_are_byte_identical`, `touched_blocks_derive_from_the_exclusive_end`, `map_probe_is_snapshot_exact_and_anomaly_safe` — the pure classifier table.
* `zero_length_write_has_no_side_effects`, `range_overflow_and_max_bounds_are_exact`, `mapped_overwrite_counts_a_shared_candidate` — R2-M2 range law + the preview ledger.
* `mapped_overwrite_dispatches_on_the_shared_guard`, `scope_ledger_closes_against_classified_writes`, `truncate_between_probe_and_guard_upgrades_exactly_once`, `cold_cache_write_routes_exclusive`, `posture_off_maps_shared_candidates_to_exclusive_dispatch` — the §2.4 protocol end to end over an in-process backend.
* `stream_write_hold_excludes_the_block_io_window`, `hold_ledger_closes_class_by_class_against_final_scope`, `concurrent_stream_writers_to_one_file_overlap_their_block_windows`, `n_fresh_files_qd_k_streams_are_parallel_and_byte_exact`, `extending_stream_and_hole_fill_dispatch_on_the_shared_guard`, `extending_write_completes_under_a_held_read_guard`, `evicted_snapshot_between_probe_and_guard_upgrades_exactly_once`, `cold_cache_extend_still_routes_exclusive`, `narrow_off_restores_the_pre_campaign_class`, `rows_in_process_release`, `write_shared_off_dominates_the_narrow_posture` — W-2's class, hold ledger and in-process A/B rows.
* `lookup_republish_never_regresses_acked_size`, `postlude_stamp_matches_the_durable_fold`, `concurrent_write_postludes_cannot_regress_mtime`, `truncate_is_never_resurrected_by_a_size_neutral_postlude`, `fenced_write_retries_rerun_and_publish_coherently`, `first_touch_writer_storm_mints_one_lease`, `attr_publications_route_through_the_one_door` — KD-5/KD-6.

Watchdog / registry (`tests/fuse_watchdog_teardown_tests.rs`, `op_registry_shard_tests.rs`):
* `watchdog_tick_reports_overdue_with_op_detail_and_clears_on_exit` — a forced slot past a tiny threshold is reported with op/ino/age and vanishes at drop.
* `slow_commit_completes_and_watchdog_logs_instead_of_timeout_drop` — a stalled commit is logged, never cancelled.
* `admission_park_past_threshold_escalates_to_disabled_volume`, `admission_park_escalation_holds_for_the_smallest_committer_class` — class 2 escalation.
* `bounded_barrier_errs_on_hung_leader_and_coalescer_recovers`, `bounded_barrier_bounds_followers_behind_a_slow_leader` — class 1's inside-the-coalescer bound.
* `cfr_pair_guard_order_is_shard_monotone`, `cfr_registers_with_watchdog_while_guard_blocked`, `fallocate_registers_with_watchdog_while_guard_blocked` — two-ino order; the VL8 blind classes register.
* `write_to_reclaimed_ino_is_a_counted_discard_not_enoent` — KD-7's orphan probe.
* `registry_geometry_ties_to_the_transport_derivation`, `derivation_covers_field_and_floor_shapes`, `op_slot_owns_its_cache_line`, `registry_registers_the_full_delivered_ring_capacity`, `exhaustion_degrades_to_unregistered_and_recycles`, `release_from_a_foreign_thread_recycles_the_slot`, `watchdog_scan_covers_all_shards`.

Open count / FORGET (`tests/lookup_refcount_tests.rs`, `forget_sweep_tests.rs`):
* `entry_replies_count_and_a_partial_forget_does_not_evict`, `batch_forget_carries_per_entry_nlookup`, `an_uncounted_inode_still_evicts_on_its_first_forget`, `a_lost_release_leaves_a_stranded_open_count_that_is_detected` — FUSE-3k + POSIX-14.
* `forget_sweeps_dir_gen_and_killpriv_clean`, `batch_forget_sweeps_exactly_like_n_forgets` — RES-13 side maps.

Fast dispatch / trace (`tests/read_fast_dispatch_tests.rs`, `op_trace_tests.rs`):
* `warm_active_buffer_read_is_served_byte_exact_and_eof_clamped`, `cold_block_demotes_then_the_tier_resident_block_serves_into_the_dest_window`, `a_writer_holding_the_inode_lock_demotes_in_bounded_time`, `virtual_inodes_demote`, `served_op_records_zero_ingress_and_the_fast_dispatch_trace_chain`; ⚑ `live_armed_session_reads_are_all_fast_dispatched_and_close_against_inplace_replies` (real mount: the partition law on a live session).
* `disarmed_hooks_record_nothing`, `armed_explicit_stamps_land_sorted_and_drain_empties`, `sampling_divisor_is_deterministic_and_stride_independent`, `scope_binds_the_current_op_across_awaits`, `ring_overflow_drops_and_counts_never_blocks`, `concurrent_producers_fold_exactly`, `stage_table_is_total_and_unique`, `armed_write/read/create_yields_an_ordered_*_chain_for_its_unique`, `trace_json_shape_is_pinned_and_drains`, `trace_inode_is_owner_only_and_stats_carry_the_gauges`, `stitch_tool_joins_a_real_dump_against_the_histograms`, `op_trace_knob_is_registered_bool_default_off`, `derived_geometry_fits_one_drain_interval`.

INTERRUPT has NO dedicated test in the reference (the only `interrupt` hit is a
symmetric-plane join test). PORT red-first: ⚑ a mounted test that sends a signal to a
reader parked on a stalled device seam and asserts (a) no reply is ever written for the
INTERRUPT unique, (b) the stalled op still commits exactly once, (c) `fuse_interrupts_
received` moved by one.

---

## 13. Reference quirks flagged (decision 0002 ledger candidates)

| # | Quirk | Why poor | PORT |
|---|---|---|---|
| L1 | Unbounded reply channel + per-queue reply task + `select!` teardown coupling, alongside the in-place commit arm (`session.rs:619-625, 940-948, 1113-1175`) | Two reply paths for one obligation, forced by the async runtime; a reply-task death is a session death | ONE `submit_reply` from any thread; the obligation is a value with a `deinit` synthesizer |
| L2 | Non-READ opcodes rotate across ALL lanes; same-lane only for READ "until measured" (`session.rs:6078-6098, 6256-6257`) | A cross-thread wake per metadata op the kernel's CPU spread already made unnecessary | Home-lane dispatch for every opcode (charter §9); `SQUEEZEFS_FUSE_SAME_LANE_DISPATCH` deleted |
| L3 | A lane task spawned per INTERRUPT to call a stub whose `Err(ENOSYS)` is discarded (`session.rs:4289-4319`) | Work with no effect | Decode, count, ignore |
| L4 | `ReplyCreated` encodes `subsec_micros()` into the `_nsec` TTL fields (`reply.rs:433-434`) | 1000× TTL error on every create vs lookup's `subsec_nanos()` | One entry encoder |
| L5 | Dirent overflow check ignores padding (`session.rs:3824-3845`); readdir as a `Stream` re-encoded into a `Vec` | Reply may exceed `size`; per-entry await + allocation | Fill the reply window directly with an aligned remaining bound |
| L6 | READ handler performs `backend.getattr().await` under the inode READ guard (`fuse_client.rs:33874-33899`) | Backend I/O under level (1) — the thing the Shared law forbids | Resolve outside the guard; miss ⇒ fetch then re-take |
| L7 | Two levers (`WRITE_SHARED`, `WRITE_GUARD_NARROW`) whose OFF legs are measured losers, plus test-override tri-state atomics beside them (`4813-4868`) | Dead control arms; knobs read outside the registry's `Environ.Map` | One A/B lever; test posture through the knob map |
| L8 | `SQUEEZEFS_TIMEOUT` as the watchdog threshold (`197-211`) | A free-floating constant with a knob on top (row 17) | `conveyor.PARK_ESCALATION_NS` |
| L9 | `patch_max_bytes_cell` hand-parses its env (`581-590`) | Second parser (row 14 class) | The ONE parser |
| L10 | `catch_unwind`-per-poll lane survival + `ReplyTx::drop` EIO as the error boundary (`sqz_exec.rs:22-25`, `session.rs:256-294`) | Hides programming errors behind a counter (row 9) | Error unions + tripwires; a true bug aborts loud; the synthesizer remains for the non-bug "handler returned without replying" class and must read 0 |
| L11 | The op guard table has no stripe census while every other striped table does (`2288-2342`) | The 1/16 load-factor argument cannot be checked at level (1) | `inode_guard_{stripes,stripe_collisions,key_waits}` |
| L12 | `OpProf` phase stamps in `AtomicU64`s because the future may migrate lanes (`1683-1687`) | Atomics for a value one task owns | Plain fields on the handler frame; a Zig handler does not migrate mid-op |
| L13 | `Instant`-to-`CLOCK_MONOTONIC` epoch pair in `ArmConfig` (`op_trace.rs:103-108`) | A conversion only because `std::time::Instant` is opaque | `clock.monotonicNs` IS the stamp |
| L14 | `FuseOpKind` covers 22 of ~40 opcodes (`1296-1323`); the rest never register with the watchdog | An unregistered op is watchdog-blind (the VL8 lesson, `1316-1320`) | Every opcode registers (the comptime table carries its kind) |

Rung 3's first mount needs §1 (lifecycle, one reply), §5's R3 column over
`KvBackend` + `dlm.Manager` + `economy.*`, §6 (registry + watchdog + wedge census), §8
(open table + FORGET), the §2 guard in its EXCLUSIVE-only form (every write is
inline/`EntireOp` until rung 4 brings striped layouts), and §7's zero-cost-off hooks.
The shared-mode classifier, `read_fast_probe`, the hold gate and the data-path
stations arrive with rung 4 and must land with the §2.4 protocol's red tests first.
