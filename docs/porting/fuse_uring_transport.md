# PORTING SPEC — the FUSE-over-io_uring transport (rung 3: ring setup, slot state machine, lease + commit gate, drain groups, worker pass, teardown)

Reference tree: `/Users/joberdorf/Source/squeezefs/crates/fuse3/src/raw/connection/` unless a path says
otherwise. Short names used in cites:

| cite | file |
|---|---|
| `fou.rs` | `fuse_over_uring.rs` (11171 lines — the whole transport) |
| `lc.rs` | `lease_core.rs` (228; loom-checked, `#[path]`-included by `loom-models`) |
| `wc.rs` | `wake_core.rs` (144; loom-checked) |
| `fused.rs` | `fused.rs` (759) |
| `fd.rs` | `fast_dispatch.rs` (280) |
| `spin.rs` | `spin.rs` (270) |
| `uc.rs` | `uring_conn.rs` (2278 — `/dev/fuse` classical connection, INIT read, sideband) |
| `zc.rs`, `kmbuf.rs` | the zc / kmbuf modules (protocol surface only — buffer modes are another spec) |
| `session.rs` | `crates/fuse3/src/raw/session.rs` (INIT reply + bring-up order) |
| `loom.rs` | `loom-models/src/lib.rs` |
| `tests/…` | `/Users/joberdorf/Source/squeezefs/tests/…` (root suite) |

Port-side anchors: charter §4 (the law), §9 (model checks), §10 (observability); decision 0002 (quirks);
`src/io/ring_park.zig` (the `ParkedRing` primitive every worker composes); `src/model/explorer.zig` (the
checker the lease/gate models are written for); `src/io/nvme_derive.zig:316` (`WakeCoalescer`, already the
ONE coalescer).

---

## 0. Scope in one paragraph

After the classical INIT exchange on `/dev/fuse`, one worker thread per DRAIN GROUP (a NUMA-node-membership
chunk of qids) owns one SQE128 io_uring, REGISTERs `depth` ring entries ("ents") per member queue, arms a
level-triggered PollAdd on a per-group eventfd, and then runs a single loop (fou.rs:5492–7821): drain the
eventfd → disarm the wake coalescer → pump commit/bridge messages into SQEs (eager non-blocking GETEVENTS
flushes in between, one fused handler poll per message) → route every commit through the per-ent payload-lease
gate → unpark scan → arm the backstop → ONE `io_uring_enter` that both flushes the batch and waits → reap CQEs
(deliveries, commit echoes, bridges, REGISTER refusals) → dispatch each delivery (READ dispatch law, FORGET
auto-commit, WRITE lease) → re-arm the poll → re-REGISTER / retry backoffs. A per-ent SLOT STATE MACHINE owned
by the worker thread enforces "exactly one COMMIT_AND_FETCH per delivered request" (fou.rs:353–372); other
threads address a slot by value `(qid, ent_idx, commit_id)` carried in the request (fou.rs:180–185) and post to
the group's commit channel. The payload-lease + commit-gate word protocol (lc.rs) and the wake coalescer
(wc.rs) are the two lock-free cores; both are loom-checked and both must be re-modelled for `explorer.zig`.

---

## 1. Ring setup per drain-group worker

### 1.1 Setup flags and the probe ladder
- Ring type: `IoUring<squeue::Entry128, cqueue::Entry>` (fou.rs:4191) — SQE128 is REQUIRED because the FUSE
  uring cmd carries an 80-byte cmd area (`UringCmd80`, fou.rs:8610); a build failure names it:
  `"… need IORING_SETUP_SQE128"` (fou.rs:3313–3317).
- `cqsize = sq_entries × 2` (fou.rs:3297). WHY: a worst pass can push `depth` commits + `depth` re-REGISTERs +
  1 poll re-arm (fou.rs:2282–2285, :4722–4724); the overflow tripwire `transport_cq_overflows` PROVES the
  geometry holds instead of assuming it (§13).
- SQ entries: `sq_entries = next_pow2(group_depth + 8).max(16)` with `group_depth = width × depth`
  (fou.rs:4702–4703). The `+8` is headroom for the PollAdd re-arm, cancels and releases riding one pass.
- Posture ladder (`QueueRingPosture`, fou.rs:4280–4283): `Modern` = SQE128 + cqsize +
  `SINGLE_ISSUER | DEFER_TASKRUN | TASKRUN_FLAG` (fou.rs:3308–3311); `Plain` = SQE128 + cqsize only.
  - PROBE once per process (`modern_queue_ring_flags_probed`, fou.rs:4330–4350): build a scratch ring
    `cqsize 8, entries 4` with the three flags; EINVAL ⇒ `false` (pre-6.1 kernel). Probe, never a version
    check (charter §3).
  - `build_plain_queue_ring_with` (fou.rs:4360–4379): probe-present ⇒ try Modern first, a refusal at build
    time (seccomp/lockdown) WARNS and degrades to Plain — an accelerator never fails the mount; probe-miss ⇒
    Plain silently, zero extra syscalls.
  - WHY each flag: SINGLE_ISSUER is free (one thread builds, submits, reaps — fou.rs:4268–4271);
    DEFER_TASKRUN moves completion task-work onto the worker's own `io_uring_enter` instead of IPIs
    (fou.rs:4271–4273); TASKRUN_FLAG makes the kernel raise `IORING_SQ_TASKRUN` in `sq_flags` so a spinning
    worker can see a completion with one acquire load, no syscall (fou.rs:3302–3307; 5.19+, inside
    DEFER_TASKRUN's 6.1 floor).
  - Consequence that shapes the whole pass: on a DEFER_TASKRUN ring a plain `submit()` never sets
    `IORING_ENTER_GETEVENTS`, so completions posted by other contexts stay INVISIBLE until a GETEVENTS enter
    (fou.rs:2110–2119). Every mid-pass flush is therefore `submit_with_args(want=1, ts=0)` — the non-blocking
    GETEVENTS idiom; ETIME ⇒ `Ok(0)` (fou.rs:2120–2142). Pinned by
    `getevents_flush_materializes_deferred_completions_without_parking` (fou.rs:9192–9254).
- Port: `ParkedRing.initProbing(gpa, entries, ladder, &rung)` (ring_park.zig:116) IS this ladder:
  `ladder = [SQE128|SINGLE_ISSUER|DEFER_TASKRUN|TASKRUN_FLAG|CQSIZE, SQE128|CQSIZE]`; log the rung once at arm.
  `cqsize` must be set via `params.cq_entries` + `IORING_SETUP_CQSIZE` on the Zig side (std's `IoUring.init`
  takes flags only — use `init_params`, `IoUring.zig:39`).
- **BLOCKING PORT FACT — Zig 0.17 `std.os.linux.IoUring` does not support SQE128.** `init_params` mmaps the
  SQE array as `sq_entries × @sizeOf(io_uring_sqe)` = 64 B per entry and indexes `[*]io_uring_sqe` at a 64-byte
  stride (`std/os/linux/IoUring.zig:1543–1553`); under `IORING_SETUP_SQE128` the kernel's SQE region is
  `sq_entries × 128` and every SQE is 128 bytes with the 80-byte cmd area at offset 48 (`io_uring_sqe.zig:7–21`
  ends at `addr3/resv` = 64 B; `cmd` is the SQE's second half). Pushing a `URING_CMD` through std's `get_sqe` on
  an SQE128 ring would corrupt the ring. Charter §12 already mandates "Zig's own liburing-equivalent bindings
  (SQE128/CQE32, …)": rung 3 ships `src/io/uring128.zig` — the same `ParkedRing` protocol (eventfd wake, park,
  dead-ring poll, quiesce) over a ring whose `sqes` is `[*]align(64) [128]u8` (or an `extern struct Sqe128 {
  base: io_uring_sqe, cmd: [64]u8 }`, size-asserted 128), `get_sqe` indexing at 128, and `prepUringCmd(fd,
  cmd_op, cmd: *const [80]u8)` writing `opcode = URING_CMD`, `cmd_op` as the u32 at +8 (`linux/io_uring.h`: the
  `off/addr2` union's `struct { __u32 cmd_op; __u32 __pad1; }`), `fd` (+4, with `IOSQE_FIXED_FILE` for index 0),
  `user_data` (+32), `buf_index` (+40), and the 80 cmd bytes at +48 (the `addr3/__pad2` tail becomes `cmd[]`
  under SQE128). The iovec REGISTER form additionally writes `addr` (+16) and `len` (+24) — fou.rs:8615–8626. The sideband/INIT ring stays a plain
  64-byte `ParkedRing`. Either extract the park protocol into a comptime-generic over the SQE type or embed a
  `ParkedRing` core that takes `*anyopaque` SQE slots — the wake/park/dead logic must not be copied (decision 0002
  row 12).
- SQPOLL (fou.rs:4193–4262, :4393–4469): knob-only (`SQUEEZEFS_FUSE_IO_URING_SQPOLL_IDLE_MS` / `_CPU`), ONE
  shared kernel poller (qid-0 group = leader creates it, every other ring `ATTACH_WQ`s to the leader's fd;
  followers wait ≤ `SQPOLL_LEADER_WAIT = 10 s`, fou.rs:4261), never composed with DEFER_TASKRUN (the poller owns
  task-work, fou.rs:4287–4290). "Measured not recommended (M10)" (fou.rs:4290). Port: carry the knob rows
  (knobs_and_rails rows 89–90), implement the leader/attach only if a counted A/B ever asks; the default path
  must not know SQPOLL exists.

### 1.2 Fixed files `[fuse_fd, wake_eventfd]`
- `ring.submitter().register_files(&[pool.fuse_fd, wake_fd])` (fou.rs:4727–4734); a refusal fails the worker →
  the mount (no raw-fd fallback here, unlike `ring_park.registerFile`'s degrade — the uring cmd needs
  `types::Fixed(0)`, fou.rs:8610, and the PollAdd uses `types::Fixed(1)`, fou.rs:2080, :5400).
- Port: register BOTH indices at init; `ParkedRing` today registers only index 0 (`registerFile`,
  ring_park.zig:215) and arms its wake as a READ on the raw eventfd (ring_park.zig:206). The transport worker
  can keep the READ-based wake (the port's primitive) — see §8.6 for why a READ and a PollAdd are equivalent
  here — but the fuse fd MUST be fixed index 0.

### 1.3 Header buffers
- One `FuseUringReqHeader` per ent = `in_out[128] ‖ op_in[128] ‖ fuse_uring_ent_in_out{flags u64, commit_id
  u64, payload_sz u32, padding u32, reserved u64}` = 288 bytes (fou.rs:127–141; size-asserted at
  `test_header_sizes`, fou.rs:9404). `FUSE_URING_IN_OUT_HEADER_SZ = 128`, `FUSE_URING_OP_IN_OUT_SZ = 128`
  (fou.rs:111–112). Classical (`UserEnts`) mode: a `Box` per ent (fou.rs:4865–4867); the ent's
  `ring_ent_in_out.payload_sz` is pre-set to `payload_sz` (fou.rs:4866) — the kernel reads it at REGISTER.
- kmbuf/zc: the header lives in the queue's registered headers region (`KmbufQueue::header_ptr`, fou.rs:4891) —
  other spec.
- Port: `extern struct FuseUringReqHeader` with `comptime` size assert 288; one per ent in a worker-owned slab.

### 1.4 The payload arena
- `PayloadArena::new(depth, payload_sz, wake_fd, coalescer, node)` (fou.rs:1163–1239): ONE anonymous
  `MAP_PRIVATE|MAP_ANONYMOUS` RW mmap of `span = stride × depth`, `stride = payload_sz.next_multiple_of(4096)`
  (fou.rs:1176–1180); `MADV_HUGEPAGE` over the span (fou.rs:1203–1205); when NUMA placement is active,
  `bind_region_preferred(node)` BEFORE first touch then `MADV_POPULATE_WRITE` (fou.rs:1207–1218); per-buffer
  actual node recorded once via `get_mempolicy` for the locality instrument (fou.rs:1220–1227).
  `bufs[i] = base + i × stride` (fou.rs:1220). Buffers are born zeroed, 4 KiB-aligned, non-aliasing
  (`test_payload_arena_buffers`, fou.rs:9870–9901).
- The arena holds a `dup(2)` of the group wake eventfd + the group's `WakeCoalescer` (fou.rs:1142–1147,
  :1170–1175) so a LEASE DROP that outlives the worker still has a live fd to write (fou.rs:1113–1117: "a leaked
  lease degrades to a leaked buffer, never a dangling pointer").
- Registration: in UserEnts mode the arena is NOT a registered buffer — each ent's REGISTER names it through
  `iov[1]` (§1.5). kmbuf/zc register differently (other spec).
- Port: arena per MEMBER queue, owned by the worker, refcounted only by live leases (a lease holds the arena
  alive). In Zig, "Arc" = the lease's reference on an arena whose lifetime the worker extends until
  `leases_outstanding == 0` at teardown (§11.3) — no per-op allocation (charter §12).

### 1.5 The REGISTER SQE, byte-exact (`build_cmd_entry`, fou.rs:8586–8641)
```
SQE128 (128 bytes):
  opcode   = IORING_OP_URING_CMD, fd = Fixed(0) (the fuse fd), cmd_op = FUSE_IO_URING_CMD_REGISTER (=1, fou.rs:114)
  +16 addr = (u64) &ent.iov[0]          // UserEnts only: the 2-iovec ARRAY pointer (libfuse fuse_uring_register_ent)
  +24 len  = 2                           // UserEnts only
  +40 buf_index (u16) = 0                // UserEnts; kmbuf/zc = ent_idx (fixed_buf_id)
  user_data = encode_user_data(Register, gent)
  +48 .. +128: the 80-byte cmd area, starting with struct fuse_uring_cmd_req (24 B, fou.rs:160–169):
      +48  flags      u64 = 0
      +56  commit_id  u64 = 0 (REGISTER)
      +64  qid        u16 = member qid
      +66  init.flags u16 = 0 (UserEnts) | FUSE_URING_BUF_RING (1<<0) | FUSE_URING_ZERO_COPY (1<<1) | FUSE_URING_PAYLOAD_RETENTION (1<<2)
      +68  init.queue_depth u16 = 0 (UserEnts) | depth (zc only)
      +70  padding[2]
      +72 .. +128 zero
  ent.iov[0] = { header_ptr, 288 }  ; ent.iov[1] = { payload_ptr, payload_sz }   (fou.rs:4915–4925)
```
Pinned byte-for-byte by `test_cmd_entry_wire_encoding` (fou.rs:9421–9526) and
`commit_entry_carries_retain_in_the_union_bytes` (fou.rs:8908–8950: the union u16 sits at SQE offset 66).
The kernel's REGISTER acceptance bound: `iov[1].iov_len ≥ ring->max_payload_sz` where
`max_payload_sz = max(FUSE_MIN_READ_BUFFER=8192, fc->max_write, fc->max_pages × PAGE)` (fou.rs:9570–9595) —
the geometry in §1.7 is constructed so `payload_sz == max_payload_sz` exactly.

### 1.6 The COMMIT_AND_FETCH SQE
Same entry, `cmd_op = FUSE_IO_URING_CMD_COMMIT_AND_FETCH (=2)`, `commit_id = the delivered request's
commit_id`, `qid`, NO iovecs, `buf_index = 0`, union u16 at +66 = `0` or `FUSE_URING_COMMIT_RETAIN (1<<0)`
(fou.rs:8225–8242, kmbuf.rs:194). Reply bytes travel IN THE ENT: `in_out[0..16] = fuse_out_header`, header
extra + body in the payload buffer, `ring_ent_in_out.payload_sz = bytes placed` (`apply_reply`,
fou.rs:8352–8444). The kernel reads only `commit_id`/`qid` from the SQE and copies the body from the
REGISTER-time payload VA (tests/read_dest_lease_tests.rs module doc, lines 7–12).
`RELEASE_PAYLOAD (=3)` (fou.rs:119) is the same shape with `commit_id` of the retained ent (fou.rs:8263–8287).
`user_data` tags (fou.rs:799–804): high 32 bits `1 Register | 2 Commit | 3 Fetch | 4 Cancel | 5 Release`, low 32
bits the group-local ent id `gent = member_slot × depth + ent_idx` (fou.rs:4962–4965); `u64::MAX` = the poll
marker (fou.rs:794). An unknown tag decodes as Register "rather than dropping the CQE" (fou.rs:831–834).

### 1.7 Geometry (resolved ONCE before the INIT reply is serialized)
`TransportGeometry::plan` (fou.rs:2809–2894), inputs `(kernel_nqueues, env_queues, env_depth,
desired_max_write, max_pages_limit, page, buffer_cap, mb_override, ct_override)`:
- `page = max(sysconf PAGE_SIZE, 512)`; `limit = clamp(/proc/sys/fs/fuse/max_pages_limit, 1, 65535)` with
  fallback 256 when the sysctl is absent (fou.rs:2650–2656).
- `pages_for(mw) = clamp(ceil(mw/page), 1, limit)`; `payload_for(mw) = max(8192, mw, pages_for(mw) × page)`
  (fou.rs:2828–2829) — THE kernel mirror.
- `max_write_target = clamp(desired, max(page, 4096), limit × page)` (fou.rs:2834).
- `nqueues = env ? clamp(env, 1, 512) : sysconf(_SC_NPROCESSORS_CONF)` (fou.rs:2836, :2675–2684) — kernel
  POSSIBLE cpus; fewer never becomes ready (the kernel's `is_ring_ready` needs every queue armed, fou.rs:2728–2733).
- `depth_at(mw) = clamp(cap / (nqueues × payload_for(mw)), 4, 32)` (fou.rs:2838–2843); `Q_DEPTH_DESIRED = 32`
  (measured-best: QD32 + mb256 = 316k IOPS on the decomposition box, fou.rs:2603–2606), `Q_DEPTH_FLOOR = 4`
  (the pre-L1 shipped posture, fou.rs:2607–2610).
- Env depth ⇒ `(target, clamp(d, 1, 32))`, bypassing the budget (fou.rs:2845–2848). Else the LADDER: depth leg
  first; only when `depth == 4 ∧ floor_arena > cap ∧ target > min(target, 1 MiB)` does the payload leg
  engage: `mw = clamp((cap / (nqueues × 4)) / page × page, base, target)` with `base = min(target,
  PAYLOAD_BASE = 1 MiB)` (fou.rs:2849–2868). Never below 1 MiB (fou.rs:2611–2617).
- `max_pages = pages_for(mw)`, `payload_sz = payload_for(mw)` (fou.rs:2871–2872).
- `max_background = mb_override > 0 ? mb : clamp(nqueues × depth, 64, 65535)` (fou.rs:2874–2879;
  `MAX_BACKGROUND_FLOOR = 64`, fou.rs:2622); `congestion_threshold = ct > 0 ? ct : mb / 4 × 3` (fou.rs:2880–2883).
- `default_buffer_cap = phys_pages × page / 8` (fou.rs:2634–2642); the port's cap is `mem_budget / 8` through
  `SQUEEZEFS_TRANSPORT_MEM_MAX/PCT` (knobs.zig:101–102) — explicit > pct > derived.
- Tie tests to port verbatim (fou.rs:9614–9864): `(32q, 1 MiB, limit 256, 2 GiB) → depth 32, payload 1 MiB,
  max_pages 256, mb 1024, ct 768, arena 1 GiB`; `(32q, 4 MiB, 1024, 2 GiB) → mw 4 MiB, pages 1024, depth 16,
  mb 512`; `(32q, 4 MiB, 256, …) → mw 1 MiB`; `(32q, 1 MiB, 64, …) → mw 64×page`; `(32q, 4 MiB, 1024, 819 MiB) →
  depth 6, mw 4 MiB`; `(…, 256 MiB) → depth 4, mw 2 MiB, pages 512`; `(…, 64 MiB) → depth 4, mw 1 MiB`;
  `(…, cap 0) → depth 4, mw 1 MiB`; `(8q, 4096, 256, 32 MiB) → mw 4096, pages 1, payload 8192, depth 32`;
  `(32q, 819 MiB) → depth 25, mb 800`; `(32q, 128 MiB) → depth 4, mb 128, ct 96`; `(4q, 179 MiB) → depth 32, mb
  128`; `(256q, 2 GiB) → depth 8, mb 2048`; `(256q, 9.5 GiB) → depth 32, arena 8 GiB`; `(1q, depth 4) → mb 64, ct
  48`; env queues 4096 → 512, 0 → 1; env depth 64 → 32, 0 → 1; mb/ct overrides 96/80 verbatim, 100 → ct 75,
  0/0 → policy. Every planned geometry satisfies `payload_sz == kernel_ring_max_payload_sz(limit, g)`.
- Published gauges (fou.rs:3014–3019, :2907–2950): `transport_{queues,q_depth,payload_buffer_bytes,
  max_background,max_write,max_pages,max_readahead}` + `transport_payload_buffers = queues × depth`.

---

## 2. Bring-up order, the arm barrier, REGISTER refusal backoff and retirement

### 2.1 Order (session.rs:1860–1983; charter §4 "Bring-up")
1. Classical INIT read via io_uring `Readv` on `/dev/fuse` (uc.rs:1294–1403: a per-connection read ring with a
   registered eventfd; QUIRK Q1 below).
2. `TransportGeometry::resolve(desired = reply.max_write, cap, mb, ct)` BEFORE serializing the reply
   (session.rs:1874–1886); the reply advertises `max_write = negotiated`, `max_pages = planned`, `max_background`,
   `congestion_threshold` from the plan (session.rs:1895–1914) — so "the limits the kernel was told always match
   the rings that got registered" (fou.rs:2687–2690).
3. `ensure_kernel_fuse_uring_enabled()` (fou.rs:2954–2976: read `/sys/module/fuse/parameters/enable_uring`,
   write `Y` if not) BEFORE the reply lands — since 7.2.4 the kernel creates the ring at INIT-reply time iff the
   parameter reads Y then; a later REGISTER on a ring-less connection answers -EINVAL (session.rs:1933–1944).
   Pinned by `the_kernel_uring_enable_precedes_the_init_reply` (session.rs:6955).
4. Write the INIT reply classically (`ReplySlot::Classical`, session.rs:1960–1968) — the kernel's
   `fuse_uring_cmd` requires `fch->initialized` before REGISTER (-EAGAIN otherwise).
5. `try_start(fuse_fd, geom)` (fou.rs:2983–3281): resolve buffer mode + retention probe (fou.rs:3007–3013),
   publish geometry gauges, build inbound queues + per-queue `lease_states` (`depth` words per queue,
   fou.rs:3026–3035), the drain-group plan (§3), one `(commit channel, eventfd, coalescer, owed count)` per group
   (fou.rs:3054–3077), the `slot_watch` cells (`nqueues × depth`, every cell of a queue sharing its group's
   `owed`, fou.rs:3079–3085), spawn one worker thread per group (fou.rs:3148–3182) and the connection-watch
   thread (fou.rs:3231–3240).
6. ARM BARRIER (fou.rs:3187–3218): main thread spins at 1 ms until `queues_registered ≥ nqueues` (worker adds
   `g` = its member count after pushing every REGISTER, fou.rs:5424) or a worker error / 30 s deadline fails
   the mount. Then `mark_ready()` (session.rs:1979; fou.rs:3408–3421), `set_classical_sideband()` on the primary
   connection (session.rs:988) and `nqueues` worker sessions each pulling `recv_inbound(qid)` (session.rs:971–986).
7. Register the fused-write dispatcher, hold gate and READ fast-dispatch pair after INIT (session.rs:994–1010;
   fou.rs:3703–3720: first set wins; deliveries before registration ride the classic dispatch).

### 2.1a The INIT reply word (session.rs:1780–1830, :6412–6640)
- `flags2` ALWAYS carries `FUSE_OVER_IO_URING_FLAGS2 = 1 << 9` ("required: no opt-out", session.rs:1792–1797);
  plus `FUSE_TIME_LIMITS >> 32` iff the kernel offered folded bit 62, with `time_min/time_max =
  ±9_223_372_036 s` (session.rs:1810–1818, :6362–6386 — the whole-second interior of the i64-ns word,
  conservative by one second on the floor).
- `FUSE_INIT_EXT` rides the reply iff the kernel offered it ∧ `flags2 != 0` ∧ `reply_minor ≥ 36`
  (`negotiate_init_ext`, session.rs:6412–6426); `reply_minor = min(kernel_minor, FUSE_KERNEL_MINOR_VERSION)`.
- `negotiate_reply_flags` (session.rs:6451–6640) echoes only implemented offers: `ASYNC_READ, FILE_OPS,
  ATOMIC_O_TRUNC, EXPORT_SUPPORT, BIG_WRITES, DONT_MASK (opt), HAS_IOCTL_DIR, AUTO_INVAL_DATA, DO_READDIRPLUS /
  READDIRPLUS_AUTO (opt), ASYNC_DIO, WRITEBACK_CACHE (opt), NO_OPEN_SUPPORT / NO_OPENDIR_SUPPORT (opt),
  PARALLEL_DIROPS, HANDLE_KILLPRIV / HANDLE_KILLPRIV_V2 (opt), CACHE_SYMLINKS, MAX_PAGES`; NEVER `POSIX_LOCKS`,
  `FLOCK_LOCKS`, `POSIX_ACL`, any `SPLICE_*` (pinned by `init_reply_never_advertises_{splice,flock_locks,
  posix_locks,posix_acl}`, session.rs:6985–7054; charter §4: advisory locks stay kernel-local).
- `max_readahead` echoed VERBATIM and published as `transport_max_readahead` (session.rs:6441–6445); the R2
  prefetch window is deliberately independent of it.
- `max_write / max_pages / max_background / congestion_threshold` from the plan (§1.7); `time_gran`,
  `map_alignment` defaults; `max_stack_depth = request_timeout = 0`.
- Port: the INIT codec is an `extern struct fuse_init_in/out` with the `flags2` word read at byte 16 only when
  `FUSE_INIT_EXT` is set (session.rs:1780–1784); `KernelInit` and `NEGOTIATED_REPLY_FLAGS` are published BEFORE
  the filesystem's init hook so capability probes can read them (session.rs:1776–1790, :1825–1829).

### 2.2 The barrier counts SUBMISSIONS (fou.rs:5411–5424)
"Adjudicated UNIMPLEMENTABLE AS WRITTEN — do not 'fix' this to count completions": a REGISTER's CQE is not an
ack — the kernel parks the ent and posts its completion only when it DELIVERS a request on it (the delivery
handling reads the inbound header straight out of a successful `RingOp::Register` CQE, fou.rs:7081–7118), and
it routes requests to the ring only once `is_ring_ready()` observes EVERY queue armed. Counting completions
deadlocks the arm (no readiness ⇒ no delivery ⇒ no CQE ⇒ no readiness). A refused REGISTER is covered by §2.3.

### 2.3 REGISTER refusal: backoff then retirement (FUSE-3a)
- Constants (fou.rs:441–450): `REGISTER_RETRY_MAX = 8`, `REGISTER_BACKOFF_BASE = 1 ms`,
  `REGISTER_BACKOFF_MAX = 128 ms` ("a wedged ent must not spin a core, and must not take minutes to recover").
- `note_register_failure(ent, now)` (fou.rs:686–702): `failures += 1`; `≥ 8 ⇒ Retired` (+1
  `transport_ents_retired`); else `after = min(1 ms << (failures−1), 128 ms)`, `retry_at[ent] = now + after`
  ⇒ `Retry{after}`. Sequence 1, 2, 4, 8, 16, 32, 64 ms then retire on the 8th.
- `note_register_success` resets both (fou.rs:705–709) — an occasional EBUSY never accumulates
  (`register_success_resets_the_backoff`, fou.rs:9123–9140).
- Worker side (fou.rs:6859–6910): on a REGISTER CQE error first `fail_ent` (row 5: an owing slot gets its
  synthesized reply BEFORE reclaim; a `true` ⇒ the synthesized commit re-arms the ent, no re-REGISTER); else
  `Retry{after}` (zero ⇒ push now, else served by the backoff pass at fou.rs:7795–7820 which pushes every ent in
  `register_retries_due(now)`); `Retire` ⇒ if the MEMBER's `all_retired()` ⇒ `pool.shutdown()` + `Err` —
  per member, not per group, because the kernel routes by `task_cpu` so one fully-retired queue strands its CPU
  (fou.rs:6894–6908).
- The park is BOUNDED by the nearest `next_retry_deadline()` while any backoff is outstanding (fou.rs:6251–6254,
  :6416–6447) — an idle queue has no CQE to wake it.
- Pinned by `register_failures_back_off_then_retire` (fou.rs:9090–9118).

---

## 3. Drain groups

- Plan (`drain_group_plan`, fou.rs:1012–1072): pass 1 partitions `0..nqueues` by NUMA node MEMBERSHIP
  (`node_of(qid)` consulted only when `qid_is_cpu = nqueues == kernel_possible_cpus`, fou.rs:3040, :1045),
  order-preserving, `BTreeMap` for deterministic node order; unknown-node qids (offline/isolated possible CPUs
  — sysfs cpulists carry only online CPUs — or the whole range under a testing queue override) go to a RESIDUAL
  set. Pass 2 chunks each node set at `width = forced ?? drain_group_width(set.len())`, the residual at
  `drain_group_width(nqueues)` (machine span).
- `drain_group_width(n) = max(n / 4, 1)` (fou.rs:944–946). WHY: the cpus/4 drain-parallelism slope; measured
  2026-08-05 on a 32-possible single-node box (slope = 8): widths 1–8 at IOPS par on a ~340k ceiling, width 8
  halving drain cost — commit batch mean 1.48 → 2.98 SQEs/enter (−54 % enters), eventfd wakes/op 0.687 →
  0.340 (elide 31 % → 66 %); whole-node width collapsed −15 % on the single-thread drain ceiling
  (fou.rs:923–943, :1004–1008). One context per possible CPU anti-scaled: 32×8 = 273k vs 8×32 = 357k IOPS,
  pinning the same submitters to 8 CPUs recovered +21 % (fou.rs:966–975).
- `forced_width`: `BufRing | ZeroCopy ⇒ Some(1)` (registration is per ring per queue, fou.rs:1019–1026);
  `UserEnts ⇒ explicit SQUEEZEFS_FUSE_DRAIN_GROUP (1..=512)` (fou.rs:953–962). An explicit width never spans a
  node ("node containment is structural, not tuning", fou.rs:1010–1011).
- MEMBERSHIP not contiguity (fou.rs:976–986): 2-socket boxes number CPUs round-robin (node0 even, node1 odd), so
  a run-based plan silently produced 32 singletons (the A0 posture) in the field.
- Exactly one group leads with qid 0 (the SQPOLL leader election keys on it, fou.rs:983–985).
- Per-group shared state (`GroupHandle`, fou.rs:864–891): unbounded `mpsc` commit channel ("a bounded channel
  can block the reply task … freezing all fuse replies"), the wake eventfd (`EFD_CLOEXEC|EFD_NONBLOCK`,
  fou.rs:3056), ONE `WakeCoalescer`, the member qids, the group's OWED count (`AtomicU32`), the ring fd for
  forensics.
- Gauges: `transport_drain_groups`, `transport_drain_group_width` (fou.rs:900–920). `groups == queues` ⇒
  the lever is structurally inert on this session.
- Thread naming: reference `f3-ur<qid>` (singleton) / `f3-ur<first>-<last>` (multi-member, a membership
  LABEL not a range) + `comm_name`'s per-mount suffix (fou.rs:3155–3165); watch thread `f3-ur-watch`
  (fou.rs:3236). PORT RENAMES (decision 0002 rows 20–21, charter §9): `sqz-fuse-ur<qid>` of the group's FIRST
  member; no mount suffix. A multi-member group is still ONE thread named by its first qid — the member set is
  in the log line at arm.
- Affinity (fou.rs:4652–4698): default `node` scope = any process-mask CPU of the group's home node (`SQUEEZEFS_FUSE_PIN_SCOPE=core`
  pins to the first member's core; outside the mask ⇒ node set). Port: same two postures through
  `derive.Facts` (decision 0002 row 22: the process affinity, never the first toucher's).
- Tie tests to port (fou.rs:10940–11170): interleaved `node = cpu % 2`, 32q ⇒ 8 groups of 4, `plan[0] =
  [0,2,4,6]`; `node = cpu / 16` ⇒ 8 groups of 4 contiguous; one node 64q ⇒ 4 groups of 16; 32q ⇒ 4 of 8; 4q ⇒
  4 singletons; BufRing ⇒ singletons; width 3 on nodes of 4 ⇒ `[[0,1,2],[3],[4,5,6],[7]]`; `qid_is_cpu=false`
  must NEVER call `node_of` (5q ⇒ singletons, 32q ⇒ 4 of 8); holes `{4,6,14,16,22,24,30}` on one node ⇒ 5 node
  chunks (6,6,6,6,1) + ONE residual group `[4,6,14,16,22,24,30]`; coverage: every qid exactly once,
  qid-ascending; `gent` round-trips through `user_data` for every tag.

---

## 4. THE SLOT STATE MACHINE (`SlotTable`, fou.rs:353–760)

### 4.1 Ownership and addressing
One `SlotTable` per MEMBER queue, owned exclusively by its group worker thread (fou.rs:364–372); every
transition runs there, so no synchronization and no model (debug-asserted `assert_owner`, fou.rs:505–513).
Other threads address a slot by VALUE: `ReplySlot::Ring{qid, ent_idx: u16, commit_id}` rides the request
(fou.rs:180–185) and comes back inside the `CommitMsg` (fou.rs:190–197). The historical `unique → slot` map is
DELETED, not wrapped (fou.rs:370–372, :1536–1544). Port: `extern`-free plain struct; the `(qid, ent_idx,
commit_id)` triple is the Zig reply handle; `commit_id` is the kernel's (it sets `commit_id = unique` at
delivery, fou.rs:7113; zero ⇒ fall back to `unique`, fou.rs:7115–7119).

### 4.2 States (fou.rs:374–397)
```
Registered                       REGISTER submitted; the kernel owns the ent; no request out.
Delivered{unique, commit_id}     a request was handed out; EXACTLY ONE commit must follow.
Parked{unique, commit_id}        the admitted commit is parked behind a live payload lease (§6); still owed.
Replied{commit_id}               COMMIT_AND_FETCH submitted; the kernel owns the ent again (its CQE = next delivery).
Retained{commit_id}              COMMIT_AND_FETCH WITH RETAIN submitted (zc ACK-early): request ended kernel-side,
                                 slot pages stay registered, cmd parked (no CQE); owes one RELEASE_PAYLOAD.
Retired                          8 consecutive REGISTER failures; out of rotation.
```

```
                 REGISTER push                      delivery CQE (unique, cid)
   ┌──────────── on_register_submitted ◄──────┐   ┌─────────────────────────────┐
   ▼                                          │   ▼                             │
Registered ──delivery CQE──► Delivered{u,cid} ─┼─► admit_commit(cid)==Accept     │ commit CQE = next delivery
   ▲                           │   ▲          │     │ try_commit()               │
   │ 8th REGISTER error        │   │          │     ├─ Ready ──► submit_commit ──┼──► Replied{cid} ──┘
   │                           │   │          │     │              (retain) ─────┼──► Retained{cid} ──RELEASE push──► Replied
   │                           │   │          │     └─ Parked ──► Parked{u,cid} ─┼──► (unpark scan / bounce CQE) ──► submit_commit
   │                           │   └── on_commit_retry (EAGAIN/EINTR on a Replied)   / on_retain_refused (EINVAL on a Retained)
   │                           └── fail_ent(errno) ──► header-only -errno COMMIT ──► Replied        [every non-reply exit]
Retired                        └── abandon ──► Registered  (+transport_requests_abandoned — only when even the push failed)
```

### 4.3 Transitions, who drives them, counters touched
| transition | fn (fou.rs) | driver | notes |
|---|---|---|---|
| any → Registered | `on_register_submitted` :636–649 | REGISTER push | if from Delivered/Parked: +1 `transport_requests_abandoned` ("exactly row 5's loss" — caller must `fail_ent` first); clears `retry_at` |
| Registered/Replied → Delivered | `on_deliver(unique, cid)` :520–540 | delivery CQE | from Delivered/Parked ⇒ `DisplacedRequest{prev}` + +1 abandoned (row 10 / `fuse_resend` double delivery); resets failures/retry |
| → Delivered{unique 0, cid} | `on_deliver_degenerate` :547–553 | delivery CQE with `unique == 0, cid != 0` | so the forced EIO commit is the machine's normal exit (an ent left in USERSPACE = `waiting ≥ 1`, umount EBUSY) |
| admit | `admit_commit(cid)` :556–572 | CommitMsg pumped | `Accept` iff `Delivered{commit_id == cid}`; everything else (Parked, Replied, Retained, foreign cid) ⇒ `RefuseStale` + +1 `transport_replies_refused_stale` (FUSE-3e: never an overwrite, never a 2nd COMMIT) |
| Delivered → Parked | `on_commit_parked` :575–580 | gate said Parked, or a zc bounce bridge is in flight | |
| Delivered/Parked → Replied | `on_commit_submitted(cid)` :584–587 | `submit_commit_retain(retain=false)` | THE one exit from owing (fou.rs:8182–8255) |
| Delivered/Parked → Retained | `on_commit_submitted_retained` :592–595 | `submit_commit_retain(retain=true)` | fires a parked release (`take_release_pending`) in the SAME batch so the kernel never sees RELEASE-before-COMMIT (-EBUSY) (fou.rs:8243–8248) |
| Retained → Replied | `on_release_submitted` :601–606 | RELEASE_PAYLOAD push | inert on any other state (teardown race) |
| Retained → Delivered{0, cid} | `on_retain_refused` :612–620 | commit CQE `-EINVAL` while Retained | recover by plain re-commit (the ent still holds its applied reply) |
| Replied → Delivered{0, cid} | `on_commit_retry` :625–633 | commit CQE `EAGAIN`/`EINTR` | row 6: re-push the SAME commit, never re-REGISTER (that discarded the applied reply) |
| Delivered/Parked → Replied | `fail_ent` :658–669 | every non-reply exit | returns `Synthesize{unique, cid}` + +1 `transport_requests_failed_synthetic`; `Nothing` otherwise — a second `fail_ent` can never produce a second commit |
| Delivered/Parked → Registered | `abandon` :674–683 | commit push itself failed | +1 abandoned |
| → Retired | `note_register_failure` :686–702 | 8th consecutive REGISTER error | +1 `transport_ents_retired` |

Queries: `owing()` = Delivered ∪ Parked (fou.rs:738–747); `any_owing()` (fou.rs:755–759: Replied deliberately
excluded — an idle queue is all Replied); `all_retired()` (fou.rs:732–734); `register_retries_due(now)`
consumes deadlines (fou.rs:713–723); `next_retry_deadline()` (fou.rs:727–729); `release_pending[ent]`
set/take (fou.rs:490–499).

### 4.4 THE LAW (fou.rs:353–362; charter §4)
"For every request delivered on a ring slot, exactly one COMMIT_AND_FETCH carrying either a reply or a
synthesized error is submitted before the slot leaves Delivered, and the slot leaves that state only by that
submission." A lost reply = an application in uninterruptible sleep + `umount` EBUSY. Every non-reply exit
routes through `fail_ent` (fou.rs:8299–8341): CQE-error reclaim, a failed inbound push (row 4), a re-REGISTER
over a live request (row 5), the teardown drain (row 8), a failed at-delivery extraction, a shutdown-time
residency delivery (ENOTCONN). `fail_ent` applies `error_out_header(unique, errno)` — a bare 16-byte
`fuse_out_header{len=16, error=-errno, unique}` (fou.rs:8174–8180) that touches ONLY the separately-allocated
header struct, hence legal against a live lease — then `submit_commit`; if even the push fails:
`note_request_abandoned()` + `abandon` + clear the watch cell (fou.rs:8327–8339).

### 4.5 Oversize reply ⇒ header-only -EIO (FUSE-3d, `apply_reply`, fou.rs:8400–8417)
`want = header.len() − 16 + body.len()`; `want > ent.payload_len` ⇒ `in_out = {16, -EIO, unique}`,
`payload_sz = 0`, +1 `transport_replies_oversize`, loud log — NEVER a truncated body under a header claiming
the full length ("a corrupt reply, not a small one"). A header shorter than 16 bytes ⇒ the same EIO shape
(fou.rs:8356–8362). A body-carrying reply on an ent with no payload buffer (kmbuf attachment law / zc mirror
miss) ⇒ EIO (fou.rs:8365–8398). Pinned by `an_oversize_reply_becomes_eio_not_a_truncated_body`
(fou.rs:8746–8761) and `a_fitting_reply_is_applied_whole` (fou.rs:8726–8740: `payload_sz == body.len()`).
Zero-copy elision: when `body.as_ptr() == payload_ptr + extra_len` the copy is skipped (fou.rs:8428) — the
serve-into-payload path (`get_payload_buffer`, fou.rs:3558–3584) relies on it.

### 4.6 Slot-state unit tests to port verbatim (fou.rs:8770–9166)
`deliver_then_commit_walks_the_state_machine`; `second_delivery_on_an_owing_slot_reports_the_displaced_request`
(abandoned +1, new delivery accepted); `retain_commit_parks_then_release_rearms` (Retained refuses a 2nd
commit, `fail_ent` ⇒ Nothing, release ⇒ Replied); `retain_refused_recovers_to_plain_recommit`;
`release_on_unretained_slot_is_inert`; `commit_entry_carries_retain_in_the_union_bytes` (only bytes 66–67
differ); `double_reply_is_refused_not_overwritten`; `commit_for_a_foreign_commit_id_is_refused` (owed request
intact); `parked_commit_still_owes_and_refuses_a_second` (`owing() == [(0,77,5)]`);
`fail_ent_synthesizes_for_an_owing_slot` (2nd call ⇒ Nothing); `fail_ent_synthesizes_for_a_parked_slot`;
`abandon_counts_the_tripwire_and_clears_the_debt`; `register_over_an_owing_slot_is_an_abandonment`;
`register_failures_back_off_then_retire` (monotonic, ≥ base, ≤ max, retire on the 8th, `all_retired`);
`register_success_resets_the_backoff`; `user_data_distinguishes_register_from_commit` (for ents 0,1,31,255;
`UD_POLL` decodes to None).

---

## 5. Delivery

### 5.1 Parsing (fou.rs:7081–7236)
On a successful `Register` or `Commit` CQE for `gent`: `note_register_success` if Register; kmbuf attachment
(other spec); `unique = LE u64 in_out[8..16]`; `commit_id = ring_ent_in_out.commit_id` (0 ⇒ `unique`);
`unique == 0 ∧ cid != 0` ⇒ `on_deliver_degenerate` + header-only EIO commit (fou.rs:7120–7147); `unique == 0 ∧
cid == 0` ⇒ re-REGISTER (fou.rs:7148–7153). `opcode = LE u32 in_out[4..8]`; `payload_sz =
ring_ent_in_out.payload_sz` (the kernel's announced length); `ent.last_opcode = opcode` (commit-flush
attribution). `header_and_op = in_out[0..40] ‖ op_in[0..128]` (fou.rs:7197–7200; `FUSE_IN_HEADER_SIZE = 40`,
fou.rs:120). `capped_sz = min(payload_sz, ent.payload_len)` (fou.rs:7201–7205). Requester pid = `in_out[32..36]`
(fou.rs:7273–7278; the scoped test seam keys on it).
- Session-side reconstruction of classical framing (uc.rs:843–972): `[fuse_in_header 40][arg0 from
  op_in][arg1+ from payload]` sized by `in_header.len`; FUSE_WRITE copies only `fuse_write_in` from `op_in` and
  hands the payload `Bytes` through untouched (uc.rs:861–931) — the second 1 MiB memcpy per write was audit #2.
  PORT: the Zig codec decodes `fuse_in_header ‖ op_in` directly into typed `extern struct`s per opcode — no
  classical-framing reconstruction exists (QUIRK Q2).

Wire offsets the worker reads directly (all LE; the port's `extern struct`s):
```
in_out  (fuse_in_header, 40 B): len u32 @0 | opcode u32 @4 | unique u64 @8 | nodeid u64 @16 | uid @24 | gid @28 | pid u32 @32 | padding @36
op_in   fuse_read_in : fh u64 @0 | offset u64 @8 | size u32 @16 | read_flags u32 @20 | lock_owner u64 @24 | flags u32 @32 | padding @36
        fuse_write_in: fh u64 @0 | offset u64 @8 | size u32 @16 | write_flags u32 @20 | lock_owner u64 @24 | flags u32 @32 | padding @36
                       (FUSE_WRITE_CACHE in write_flags; O_DIRECT in flags — the hold gate's `odirect`, fou.rs:7567–7573)
out     fuse_out_header (16 B): len u32 @0 | error i32 @4 | unique u64 @8          (error_out_header, fou.rs:8174–8180)
ring_ent_in_out (32 B @ 256 in the ent header): flags u64 | commit_id u64 | payload_sz u32 | padding u32 | reserved u64
```

### 5.2 FUSE_WRITE payload = a LEASE (refs 0 → 1); everything else copies (fou.rs:7206–7237)
`opcode == FUSE_WRITE (16) ∧ capped_sz > 0` ⇒ `state.acquire()` (must return 0 — "delivery on a still-leased
ent" is a debug assert, fou.rs:7218–7219), +1 `transport_payload_leases`, +1 `transport_leases_outstanding`,
NUMA pass classified (`numa_classify_pass(queue node, buffer node, bytes)`, fou.rs:7227), and the payload is a
`Bytes::from_owner(EntPayloadLease{arena, state, ptr, len, born})` whose drop runs §6.2. Every other opcode:
`Bytes::copy_from_slice(payload[..capped_sz])` — FORGET must be committed before the session consumes it, and
non-write payloads are small (names, xattrs) (fou.rs:7206–7215).

### 5.3 FORGET / BATCH_FORGET auto-commit (fou.rs:7283–7331)
`opcode ∈ {2, 42}` ⇒ push inbound with `ReplySlot::Classical` for nlookup accounting ONLY (a failed push is a
warn; the ent is committed anyway), then `apply_reply(error_out_header(unique, 0), empty)` + `submit_commit`
right there (+1 `fuse_over_uring_replies`). WHY: over-uring holds the ent in USERSPACE until COMMIT even for a
"no reply" opcode. A `debug_assert!(!leased())` guards it (fou.rs:7315).

### 5.4 THE READ DISPATCH LAW (fou.rs:7333–7508; fd.rs:52–82)
Candidate iff `opcode == FUSE_READ ∧ SQUEEZEFS_FUSE_READ_FAST_DISPATCH (default on) ∧ dispatcher registered`
(`fast_dispatch_candidate`, fd.rs:122–124). `fuse_read_in` rides `op_in`: `fh u64 ‖ offset u64 ‖ size u32 ‖
read_flags u32 ‖ lock_owner u64 ‖ flags u32` LE (fou.rs:7372–7380); `nodeid = in_out[16..24]`. `dest` = the
reply window the probe may serve INTO: the bounce slot on zc, else `(payload_ptr, payload_len)` (fou.rs:7386–7394).
ONE clock read (`now_ns`) for arrival + fast-dispatch + mint instants (fou.rs:7398).
- (a) `probe(nodeid, fh, off, size, flags, dest)` — the filesystem's SYNC, try-only warm ladder (per-inode
  `try_read()` → active buffer → staging ring → hot tier → read-lane hold → NVMe read cache; fd.rs:21–25).
  `Served(body)` / `ServedFd{body, fd, off}` ⇒ clamp body to `size`, build `fuse_out_header{16+len, 0, unique}`,
  `commit_ready_reply(... CommitMsg{body, body_fd, prefilled None, retain false})` in THIS pass, +1
  `fuse_over_uring_replies`, `record_served(unique, now, committed)` (fd.rs:167–186: `queue_wait ≡ dispatch_lag ≡ 0`
  exact, `transport_total = arrived → committed`, op-trace stamps). A would-block is a `Demote`, never a wait.
- (b) `Demote` ⇒ `fut = mint(InboundUringReq{…, arrived_ns: now})`; `fusible = read_fusion_on ∧
  fuse_candidate(size, fusion_max, true)` with `fusion_max = fusion_ceiling(payload_sz_cfg) = max(payload/8,
  4096)` or the explicit `SQUEEZEFS_FUSE_ZC_FUSION_MAX ≥ 4096` (fused.rs:393–402; = 128 KiB at 1 MiB). `fusible ∧
  fused_lane.len() < group_depth` ⇒ `fused_lane.spawn(fut, unique)`, +1 `fuse3_zc_read_fusions`. `fusible` but
  refused ⇒ +1 `fuse3_zc_read_fusion_demotions`. `read_fusion_on = zc_mode ∧ SQUEEZEFS_FUSE_ZC_READ_FUSION`
  (default OFF by measurement: composed behind (a) it read −6.5 % IOPS, p50 +70 µs, p99 −66 % — the two levers
  converge on the reap thread's per-op serialization; standing alone it won +15.5 %; fused.rs:454–466).
- (c) else `note_fast_dispatch_demote()` + `tpc_dispatch_boxed(qid_is_cpu ? Some(qid) : None, queue_node, fut)`
  — the handler lane HOMED on the queue's CPU (one lane per queue; the first field bracket's node round-robin
  woke a different parked lane per op, fou.rs:7491–7504; session.rs:6338–6344).
- PARTITION LAW: `transport_fast_dispatch_serves + fuse3_zc_read_fusions + transport_fast_dispatch_demotes ≡`
  READs delivered on an armed session with the lever on; `zc_read_fusions + demotes ≡ fuse3_read_inplace_replies`
  (fd.rs:72–75). The reap thread never blocks on any arm (fd.rs:76). Pinned live by
  `composed_dispatch_law_partitions_every_read_on_a_zc_session` (tests/zc_bridge_phase_tests.rs:557) and
  `live_armed_session_reads_are_all_fast_dispatched_and_close_against_inplace_replies`
  (tests/read_fast_dispatch_tests.rs:646).
- The interface the filesystem provides (fd.rs:97–115): `ReadFastDispatch{ probe: fn(nodeid, fh, off, size,
  flags, dest: ?(addr, len)) → FastReadProbe{Served(body) | ServedFd{body, fd, off} | Demote}, mint:
  fn(InboundUringReq) → FusedFuture }`. Port: a vtable `{ctx, probe, mint}`; `mint` returns a handler task the
  executor (decision 0001) can poll on the worker's lane or hand to `sqz-fuse-lane<qid>`.

### 5.5 The non-READ dispatch (fou.rs:7693–7733)
Push `InboundUringReq{header_and_op, payload, unique, slot: Ring{qid, ent_idx, cid}, arrived_ns}` onto
`inbound[qid]`; a failed push (closed session queue) is row 4 ⇒ `fail_ent(EIO)`. zc WRITE hold/extract arms
(fou.rs:7510–7691) are the buffer-mode spec's.

### 5.6 Accounting at delivery
`on_deliver` (displaced ⇒ loud error), `publish_slot_owed(qid, ent, unique)` (`SlotWatch::set`: `since_ns`
store + `unique.swap`; only the 0→unique transition bumps the group's `owed`, fou.rs:316–321), +1
`fuse_over_uring_requests` (sharded), belt-clear of the ent's `retain_next` flag (fou.rs:7239–7246).

---

## 6. THE PAYLOAD LEASE + COMMIT GATE (lc.rs; fou.rs:1356–1462)

### 6.1 Word layout (lc.rs:71–78)
```
EntLeaseState { refs: AtomicU32,     // live leases over this ent's payload buffer (0/1 for WRITE payload; N for dest-DMA tokens)
                parked: AtomicBool } // the worker has a commit message parked waiting on refs == 0
```
One per `(qid, ent)`, POOL-level (`QueueHandle.lease_states`, fou.rs:848–852) so dest-DMA claims
(`lease_dest_window`, fou.rs:3903–3930) acquire against the very words the worker's gate checks.

### 6.2 Protocol — every atomic and its ordering
- `acquire()` (lc.rs:92–94): `refs.fetch_add(1, AcqRel)`; returns prev; worker-only at delivery; caller asserts
  prev == 0 ("an ent is only re-armed after a commit the gate proved lease-free").
- `acquire_dest()` (lc.rs:106–108): same RMW, refs may already be nonzero (one token per in-flight SQE); sound
  by program order — every claim precedes its request's reply (mint → claim → submit → await).
- `release()` (lc.rs:116–123), ANY thread: `if refs.fetch_sub(1, Release) == 1 { fence(SeqCst); return
  parked.load(Acquire) } else false`. `true` ⇒ the caller MUST wake the worker.
- `try_commit()` (lc.rs:136–150), worker: `if refs.load(Acquire) == 0 → Ready; parked.store(true, Release);
  fence(SeqCst); if refs.load(Acquire) == 0 { parked.store(false, Release); Ready } else Parked`.
- `try_unpark()` (lc.rs:160–166), worker: `if refs.load(Acquire) == 0 { parked.store(false, Relaxed); true }
  else false`. The Acquire pairs with release's `fetch_sub(Release)` so the payload write that follows a
  `true` happens-after every access the dropped lease made; once refs == 0 is observed no new lease can appear
  (delivery requires a prior commit).
- `leased()` (lc.rs:170–172): `refs.load(Acquire) != 0`.
- WHY BOTH SeqCst FENCES (lc.rs:27–48): release/drop and park/publish are the store-buffer (Dekker) litmus
  `releaser: refs=0; FENCE; r1=parked` ∥ `worker: parked=true; FENCE; r2=refs`. Under Release/Acquire alone
  `r1 == false ∧ r2 == 1` is permitted: the releaser skips the wake AND the worker parks — a deterministic hang
  at depth 4. SeqCst fences are totally ordered, so one side's post-fence load observes the other's pre-fence
  store. Explicit fences (not SeqCst accesses) because the fence axioms are exactly the guarantee and are what
  loom models; "the loom run of this exact file rejected a SeqCst-accesses-only draft with a concrete
  missed-wake interleaving" (lc.rs:44–47). Port: `@fence(.seq_cst)` at both sites with a comment naming the
  pair; `explorer.zig`'s `Dekker` self-test (explorer.zig:222–286) is the exact shape.
- The wake after release (`lease_release_and_wake`, fou.rs:1422–1434): `if state.release() { if
  coalescer.arm() { write(eventfd, 1); +1 transport_wake_writes } else { +1 transport_wakes_elided } }` —
  publish (the `fetch_sub`) BEFORE `arm` (the coalescer's producer law, §7.2). Shared by `EntPayloadLease::drop`
  and `DestDmaLease::drop` — one wake law.

### 6.3 Where the gate runs in the pass
- Fresh commit (fou.rs:6042–6070): `admit_commit` first (stale ⇒ refuse, no gate), then `try_commit()`: `Ready`
  ⇒ `commit_ready_reply` (apply + push); `Parked` ⇒ +1 `transport_parked_commits`, `on_commit_parked`,
  `parked_msgs[ent] = Some(msg)`.
- Unpark scan (fou.rs:6073–6107), EVERY pass before the worker can sleep: for every `parked_msgs[ent].is_some()
  ∧ try_unpark()` ⇒ +1 `transport_unparked_commits`, `commit_ready_reply` (same routing as the fresh drain).
- Delivery asserts `!leased()` on the ent (fou.rs:7132, :7219, :7315); the `apply_reply` payload write is legal
  only under a `Ready`/`try_unpark() == true` proof (fou.rs:4532–4536, :8346–8351).

### 6.4 The `EntPayloadLease` drop (fou.rs:1373–1406): the 1 s rule
`age_ms = born.elapsed()`; `transport_lease_max_age_ms.fetch_max(age)`; `transport_leases_outstanding −= 1`; `age
≥ 1000 ⇒ +1 transport_lease_overlong + error log` — NEVER a panic. WHY (fou.rs:1378–1397): this used to be
`debug_assert!(age < 1 s)`; a firing panicked the write handler mid-flight so (a) the reply was never sent —
the kernel's writeback folio parked forever and `fsync(2)` sat in D-state (captured live: `waiting=28`), and
(b) the unwind skipped `release()`, parking the ent's re-arm forever (permanent depth loss). ≥ 1 s is not proof
of escape: admission backpressure legitimately exceeds it on slow substrates. A GENUINE §5.4 escape shows as
unbounded ages + growing `transport_parked_commits`, adjudicated by counter. Then `lease_release_and_wake`.
Pinned by `overlong_write_lease_is_a_tripwire_not_a_lost_reply` (tests/transport_lease_overlong_tests.rs:191,
needs a mount).

### 6.5 Closure and shutdown with a live lease
- `parked_commits ≡ unparked_commits` at quiesce (fou.rs:2150–2159): the gap IS the wedge count. The shutdown
  drain's header-only fallback deliberately does NOT count an unpark — "that is the genuine §5.4 escape, and it
  must remain visible as an unclosed ledger".
- Shutdown (fou.rs:7923–7999): publish `parked` for every final message whose ent is still leased
  (`try_commit() == Parked`), ONE bounded event-driven wait for the whole group (`drain_await_leases`,
  fou.rs:2435–2456: `drain eventfd → disarm → retain(!try_unpark) → poll(eventfd, left)`, budget
  `DRAIN_LEASE_BUDGET = 100 ms`, fou.rs:2418 — was 1 ms…100 ms PER ENT in series = 3.2 s at depth 32); an ent
  still in `waiting` after the budget gets `error_out_header(unique, EIO)` header-only — a leased payload region
  is never written at shutdown (fou.rs:7964–7986). `drain_wait_tests` (fou.rs:10820–10930): budget shared not
  per-ent (8 ents leased forever cost < 3 × 60 ms); a lease dropped at 10 ms resolves in < 500 ms through the
  eventfd; an unleased queue waits 0.
- A lease never escapes the handler; a leased buffer is never re-posted (no REGISTER while leased — a REGISTER
  push is only reached after `fail_ent`, which is header-only) or written (charter §4).

### 6.6 What the loom models assert (loom.rs:910–1084, :1086–1256) and which weakenings fail
| model | threads | asserts |
|---|---|---|
| `ent_lease_commit_exactly_once_never_while_leased` (:919) | A: `release()` → wake flag; B (worker): `try_commit()` | commit exactly once; never while `leased()`; if B parked then A's release MUST have returned true (wake fired) and `try_unpark()` succeeds |
| `ent_lease_shutdown_header_only_never_writes_leased_payload` (:983) | A: late `release()`; drain: one `try_unpark()` probe | a `true` probe ⇒ `!leased()`; lease never outlives its drop |
| `ent_lease_dest_tokens_multi_ref_commit_exactly_once` (:1027) | two token droppers + worker gate at refs ∈ {2,1,0} | exactly once; never while any token live; a non-final drop never wakes; parked ⇒ ≥ 1 wake |
| `wake_coalescer_publication_never_stranded` (:1105) | 2 producers (publish → arm → write) + worker (≤ 3 passes of drain → disarm → scan, park on counter 0) | counter 0 at park ⇒ both publications consumed; else the re-wake pass sees both |
| `wake_coalescer_lease_drop_parked_commit_never_stranded` (:1181) | lease dropper (release ∧ arm) + reply submitter (publish, arm) sharing ONE coalescer + one concurrent worker pass + ≤ 2 wake-driven passes | parked commit committed exactly once; the reply publication observed |
Weakenings recorded as failing (lc.rs:44–47; wc.rs:56–59; loom.rs:1098–1103): (i) lease gate with SeqCst
ACCESSES but no fences ⇒ missed wake; (ii) worker pass permuted to scan-before-disarm ⇒ "consumed 1 of 2"
strand; (iii) `disarm` as a plain store (no RMW reading the predecessor) ⇒ both coalescer models strand.
PORT: write three explorer models — `LeaseGate` (2 threads, TSO store buffer on `parked`/`refs`, fence = drain
step; weakening = drop the drain ⇒ `expectFails(InvariantViolated)` on "parked ∧ no wake"), `LeaseGateMultiRef`
(3 threads), `WakeCoalescer` (3 threads; weakening = disarm-before-drain order / plain-store disarm). The
reference's third reorder (scan before disarm) must also be an `expectFails`.

Model skeletons for `src/model/explorer.zig` (one atomic action per `step`; store buffering explicit):
```
LeaseGate(with_fence):  shared { refs: u32 = 1, parked: bool = false }            // acquire() already ran
  thread 0 (releaser): pc0: pending_refs0 = true (store refs=0 buffered)          // the weakest model: treat the fetch_sub's
                                                                                   // store half as buffered (explorer's Dekker
                                                                                   // convention; an RMW that drains is stronger)
                       pc1: if with_fence drain()      (fence SeqCst)
                       pc2: r1 = parked;  woke = r1
                       pc3: drain(); done
  thread 1 (worker):   pc0: r0 = refs; if r0 == 0 { committed = true; done }
                       pc1: pending_parked = true     (store parked=true buffered)
                       pc2: if with_fence drain()      (fence SeqCst)
                       pc3: r2 = refs; if r2 == 0 { parked_flag = false; committed = true } else parked = true
                       pc4: drain(); done
  invariant: !(committed and refs != 0)                                            // never while leased
  final:     committed xor (parked and woke)  ;  parked ⇒ woke                      // exactly once; no missed wake
  weakening: with_fence = false ⇒ FinalViolated (parked ∧ !woke)
WakeCoalescer(order):   shared { armed: bool, efd: u32, produced: u32 }
  threads 0,1 (producers): produced += 1 (RMW); r = swap(armed, true); if !r: efd += 1
  thread 2 (worker, ≤ 3 passes): per pass: efd = 0 | swap(armed,false) | consumed = produced   in the chosen ORDER;
                                 park when efd == 0
  final: efd == 0 ⇒ consumed == 2 ; weakenings: order = scan-before-disarm, or disarm as a plain store ⇒ FinalViolated
```

---

## 7. Reply submission

### 7.1 Message (`CommitMsg`, fou.rs:190–217)
`{qid, ent_idx: u16, commit_id, header: Vec<u8> (16 + extra), reply_body: Bytes, prefilled: Option<u32>,
retain: bool, body_fd: Option<(fd, off)>}`. Variants on the channel (`WorkerMsg`, fou.rs:262–278): `Commit`,
`ZcFetch`, `ZcStore`, `ZcExtract`, `ZcRelease` — the last four are bridge protocol (§10).

### 7.2 `submit_reply` / `submit_reply_inner` (fou.rs:3449–3556)
1. Refuse `ReplySlot::Classical` (NotFound: "reply has no ring slot") and `!active` (NotFound: "session torn
   down" — the worker's drain owns every owed slot from here; accepting would race it).
2. `retain = take_retain_next(qid, ent)` (swap false, AcqRel; the handler armed it strictly before replying).
3. `commit_tx.send(Commit(msg))` — THE PUBLICATION.
4. `if coalescer.arm() { write(eventfd, 1); +1 transport_wake_writes } else { +1 transport_wakes_elided }`
   (the test seam `SQUEEZEFS_TEST_DROP_COMMIT_WAKES[_TID]` skips the write while leaving the coalescer armed —
   the exact generic/795 posture, fou.rs:3535–3550).
5. +1 `fuse_over_uring_replies` (sharded).
- `submit_reply_fd_body(slot, header, body, fd, off)` (fou.rs:3466–3475): same with `body_fd = Some` — on a zc
  queue the worker bridges `READ_FIXED(fd @ off → slot)` from the filesystem's MAP_SHARED fill pool with no
  bounce copy; `body` is the keepalive + fallback; on a non-zc queue the fd is ignored.
- `submit_reply_prefilled(slot, header, n)` (fou.rs:3835–3889): header only, `payload_sz = n`, the bytes already
  sit in the request's pages (the zc direct device leg); clears any stale retain flag.
- Session side (uc.rs:1109–1190): `write_vectored(data, body, slot)` routes by SLOT — `Ring` ⇒
  `pool.submit_reply`; a body whose pointer equals `get_payload_buffer(slot)` is wrapped without copy
  (uc.rs:1133–1152); NotFound ⇒ `note_reply_dropped_no_slot()` and the error is returned, NEVER a classical
  fallback write (row 9: a post-shutdown ring reply falling through to a classical write returned ENOENT and
  lost the reply). `Classical` ⇒ the device `Writev`.
- Wake economy ratio: `writes/(writes+elided) ≈ 1` under saturation = regression (the pre-L3 1.67 eventfd
  writes/op posture, fou.rs:2163–2168).

### 7.3 Port shape
`Transport.submitReply(slot, header, body)` / `submitReplyFdBody` / `submitReplyPrefilled` push onto the
group's `sync/mpmc.zig` queue (preallocated cells, decision 0002 row 13 — the reference's unbounded `mpsc`
allocates per message, QUIRK Q5) and call `ParkedRing.Wake.wake()` (ring_park.zig:73–77: arm → write iff true).

---

## 8. THE WORKER PASS (fou.rs:5492–7821), verbatim order

Loop while `active`:
1. **Rescue capture** (fou.rs:5500–5509): `after_tick = take(tick_rescue_pending)`; snapshot `wake_cqe_seen`,
   `coalescer.is_armed()`, `group_owed`; fdinfo snapshot of the eventfd only when post-tick ∧ armed ∧ a log slot
   is eligible (`RESCUE_LOG_INTERVAL = 60 s`, fou.rs:347).
2. **Drain the eventfd** iff `wake_cqe_seen ∨ coalescer.is_armed()` (fou.rs:5522–5524): ONE non-blocking
   `read(8)` (`drain_wake_eventfd`, fou.rs:2380–2399 — a non-semaphore eventfd read returns and zeroes the whole
   counter; the retired read-until-EAGAIN loop paid 2.07 reads/op against 1.01 wakes/op). WHY drain FIRST
   (fou.rs:5510–5520): every producer publishes BEFORE writing (channel send → arm → write; refs release → arm →
   write; `active=false` → write), so a wake consumed here means its state is visible to the scans below; a wake
   after this drain leaves the counter nonzero and the level-triggered poll completes the moment it is re-armed.
   Peek economy (wc.rs:103–116): only an economy, never a correctness gate.
3. **Disarm** the coalescer (fou.rs:5525–5534): after the drain, before any scan — drain → disarm → scan is the
   loom-verified order (§6.6). Port: `ParkedRing.service()` (ring_park.zig:296–301) is exactly this step.
4. **Pump ∥ fused interleave** (`next_wmsg!`, fou.rs:5570–5762): take one `WorkerMsg` (`try_recv`); when none
   and the fused lane is non-empty: if `batch.pending > 0 ∨ live_pends > 0` ⇒ `flush_submit_getevents` (eager
   non-blocking GETEVENTS so bridge DMAs launch under the NEXT handler's CPU — the serial shape capped rand-4k
   at ~7.5 ops/pass × 32 queues / ~600 µs ≈ 420k IOPS with devices busy ~50 µs per pass, fou.rs:5536–5549) and
   settle bridge `submitted_ns` stamps; MID-PASS REAP (fou.rs:5619–5722): `cq.sync()`, overflow watch, resolve
   finished HANDLER bridges (`HandlerFetch`/`HandlerStore`: oneshot send + deadline clear) NOW, defer every other
   CQE verbatim to the pass bottom (arrival order preserved via `deferred_cqes`); then poll fused tasks in a
   BURST (`while fused_lane.drain_one()`, checking the channel after each poll, fou.rs:5731–5754 — one
   flush+GETEVENTS per burst, not per poll: T2 priced per-poll enters at ~2.7 syscalls/op).
   Each `Commit` msg: member lookup (bad slot ⇒ +1 refused_stale), `admit_commit`, `try_commit` → `Ready` ⇒
   `commit_ready_reply` / `Parked` ⇒ park (§6.3). Bridge messages: §10.
5. **Unpark scan** (fou.rs:6073–6107).
6. Exit if `!active`; fold `pend_delta` into the pool pend gauge ONCE per pass (fou.rs:6114–6118; the
   steady-state pass stamps one and clears one — net 0, no RMW).
7. **Bridge-deadline scan** on a PERIOD = `zc_bridge_timeout / 1024` (29 ms at 30 s, fou.rs:5473–5483), only while
   `zc_bridge_pends > 0`: orphan re-stamp, `overdue(now, timeout)` ⇒ ONE `AsyncCancel` per pend lifetime (§10).
8. **Note flush** (`batch.note_flush()`: records the commit batch histogram; fou.rs:6223) and sample
   `commit_flush` only when provably non-blocking (CQ already non-empty, fou.rs:6224–6229).
9. **Pre-wait PollAdd re-arm** if `need_repoll_sticky` (the mid-pass reap may have consumed the poll CQE; a
   lease-drop/commit wake landing on an unarmed poll strands until an unrelated CQE, fou.rs:6235–6250).
10. **Backstop** (fou.rs:6255–6289): `park_backstop = (fused_lane.len() > 0 ∨ zc_bridge_pends > 0 ∨
    group_owed > 0) ? Some(PARK_BACKSTOP_TICK = 100 ms) : None` (fou.rs:346). WHY: field captures proved the
    cross-thread wake chains lose exactly one wake under load — `zc_bridge_pends=4` with zero cancels for 435 s
    (2026-08-13), then two delivered LOOKUPs whose handlers REPLIED sitting unpumped in queue 16's `commit_rx`
    for 23 min while every worker slept in cq-wait (generic/795, 2026-09-08). Gate on the POOL pend counter,
    not local ledgers (a pend was pool-visible with every local ledger reading 0). Idle workers keep the
    zero-cost unbounded park.
11. **Rescue ledger, commit half** (fou.rs:6308–6325): `after_tick ∧ pass_msgs > 0 ∧ !tick_wake_seen` ⇒
    `transport_park_tick_commit_rescues += pass_msgs` + the attribution WARN (once per 60 s per worker).
12. **Work in hand?** `work_in_hand = !deferred_cqes.is_empty() ∨ pass_polls > 0 ∨ pass_msgs > 0`
    (fou.rs:6326). Spin-before-park (§8.4) only when nothing is in hand.
13. **THE ENTER** (fou.rs:6396–6449): `work_in_hand ∨ spin_caught` ⇒ non-blocking `submit_with_args(1, ts=0)`
    (ETIME ⇒ Ok(0)) and loop; else BLOCKING: `(no retry, no backstop) ⇒ submit_and_wait(1)`; otherwise
    `bound = min(retry_left, backstop)` ⇒ `submit_with_args(1, ts=bound)`; ETIME with a backstop ⇒ +1
    `transport_park_backstop_ticks`, `tick_rescue_pending = true`. ONE syscall carries every SQE pushed this pass
    and the wait. EINTR ⇒ continue; a disconnect errno (ENOTCONN, ECONNABORTED, ENODEV, EPIPE, EBADF, ESHUTDOWN;
    fou.rs:4136–4146) ⇒ `shutdown` + break; other ⇒ `Err`.
    PORT NOTE: `ring_park.park()` splits submit and timed wait into two enters because a submitting enter
    discards the wait's ETIME (ring_park.zig:20–26). The reference's `submit_with_args(1, ts)` is the io-uring
    crate's `io_uring_enter(to_submit, min_complete=1, GETEVENTS|EXT_ARG)` in ONE call — it returns the
    submitted count and its ETIME verdict is lost when `to_submit > 0`… except the reference reads
    `Err(ETIME)` only on passes that submitted nothing. The rescue counters tolerate this (a pass that
    submitted and timed out simply looks like `Ok(n)` and re-loops). The port keeps the two-enter form for
    the TIMED park (correct tick accounting) and the one-enter `submit_and_wait(1)` for the untimed park;
    the "ONE io_uring_enter" economy is preserved on the saturated path because a saturated pass has
    `work_in_hand` and never waits.
14. **Reap** (fou.rs:6472–6521): settle bridge submits; `cq.sync()`; overflow watch (`CqDropWatch`,
    fou.rs:2312–2340: wrapping delta of `cq.overflow()`, charged to `transport_cq_overflows`); `completed =
    deferred_cqes ‖ fresh CQEs`; `tick_rescue_pending ∧ !completed.is_empty()` ⇒ `transport_park_tick_cqe_rescues
    += len` (posted kernel-side, the worker never woken).
    PORT DIVERGENCE (decision 0002 row 511): the reference's CQE half counts only LATE completions —
    `io_cqring_wait` returns -ETIME only after it ran the deferred completions past its timer and found the CQ
    empty (0 otherwise), so a CQE the reap surfaces after a tick landed after the deadline. The port splits it:
    the tick's RE-CHECK (the CQ read at the verdict, before any further enter) finding completions in a CQ that
    was empty at the park, on a `DEFER_TASKRUN` ring (its CQ moves only inside the worker's own enters), fires
    the must-stay-0 tripwire `transport_park_tick_cqe_lost_wakes`; the reap's remainder is
    `transport_park_tick_late_cqes` (a plain counter, read beside).
15. **Per CQE** (fou.rs:6527–7734): poll marker ⇒ `need_repoll_sticky = wake_cqe_seen = true`; out-of-range gent
    ⇒ warn; `res < 0` ⇒ the error ladder (§8.2); else Cancel/Release ⇒ debug; Fetch ⇒ bridge resolution (§10);
    Register/Commit ⇒ DELIVERY (§5).
16. **Pass-bottom PollAdd re-arm** (sticky, fou.rs:7742–7755), **re-REGISTER list** (each `fail_ent` first,
    fou.rs:7757–7794), **backoff retries due** (fou.rs:7795–7820).

### 8.1 The pass as pseudocode (every step cited above; the port's `sqz-fuse-ur<qid>` body)
```
init:  ring = ParkedRing.initProbing(ladder)            // §1.1
       register_files([fuse_fd, wake_efd])              // §1.2
       for m in members: for e in 0..depth: push REGISTER(m.qid, gent(m,e)); m.slots.on_register_submitted(e)
       arm wake op; submit();  pool.queues_registered += members.len   // §2.2: SUBMISSIONS
       batch = {}; need_repoll = false; deferred = []; wake_cqe_seen = true; tick_rescue_pending = false

loop while pool.active:
  after_tick = take(tick_rescue_pending); tick_wake_seen = wake_cqe_seen; tick_armed = coalescer.armed
  if wake_cqe_seen or coalescer.armed: read(efd) once; wake_cqe_seen = false       // drain   (port: the READ CQE drained)
  coalescer.disarm()                                                                // disarm  (port: ring.service())
  pass_msgs = pass_polls = 0; fused_more = lane.len > 0
  loop:                                                                             // pump ∥ fused interleave
    msg = commit_rx.try_pop()
    if msg == null and fused_more:
      if batch.pending > 0 or live_pends > 0: flush_submit_getevents(); settle bridge stamps
      midpass reap: cq.sync; overflow watch; HandlerFetch/HandlerStore CQEs resolved now; others → deferred; poll marker → need_repoll, wake_cqe_seen
      fused_more = false; while lane.drain_one(): fused_more = true; pass_polls++; if (msg = try_pop()) break
      if msg == null: continue if fused_more else break
    if msg == null: break
    pass_msgs++
    match msg:
      Commit(c):  m = member_of(c.qid) else refuse_stale; if m.slots.admit_commit(e, c.cid) == RefuseStale: warn; continue
                  match m.lease[e].try_commit(): Ready → commit_ready_reply(c)  | Parked → parked_commits++; slots.on_commit_parked(e); m.parked[e] = c
      ZcFetch/ZcStore/ZcExtract/ZcRelease → §10
  for m, e: if m.parked[e] and m.lease[e].try_unpark(): unparked_commits++; commit_ready_reply(take m.parked[e])   // unpark scan
  if !active: break
  fold pend_delta into pool.zc_bridge_pends
  if pool.zc_bridge_pends > 0 and now - last_scan ≥ deadline/1024: orphan sweep; for e in overdue(): push AsyncCancel  // §10
  (cf_reads, cf_writes) = batch.note_flush()                                        // commit batch histogram
  if need_repoll: push wake re-arm; need_repoll = false                             // pre-wait re-arm
  next_retry = min over members of slots.next_retry_deadline()
  backstop = (lane.len > 0 or pool.zc_bridge_pends > 0 or group_owed > 0) ? 100 ms : null
  if after_tick and pass_msgs > 0 and !tick_wake_seen: park_tick_commit_rescues += pass_msgs; warn (≤ 1/60 s)
  work_in_hand = deferred.len > 0 or pass_polls > 0 or pass_msgs > 0
  spin_caught = !work_in_hand and spin_window(...)                                  // §8.4, off by default
  if work_in_hand or spin_caught: submit_getevents_nonblocking()                    // and loop
  else:
    bound = min(next_retry - now, backstop)
    verdict = bound ? ring.park(bound) : ring.park(null)                            // ONE wait (port: submit, then wait)
    if verdict == tick and backstop: park_backstop_ticks++; tick_rescue_pending = true
    on EINTR: continue; on disconnect errno: shutdown; break; on other error: return err
  settle bridge submits; cq.sync; overflow watch; completed = deferred ++ cq
  if tick_rescue_pending and completed.len > 0: park_tick_cqe_rescues += completed.len   // port: lost (re-check) + late (reap), §8 step 14
  resubmit = []; disconnect = false
  for (ud, res, flags) in completed:
    if ud == wake_ud: need_repoll = true; wake_cqe_seen = true; continue
    (op, gent) = decode(ud); (m, e) = (gent / depth, gent % depth)
    if res < 0: error ladder §8.2 (may push resubmit, fail_ent, re-commit, set disconnect); continue
    match op: Cancel | Release → debug; Fetch → zc_fetch_complete → PendDone handling; Register | Commit → DELIVERY §5
  if disconnect: shutdown; break
  if need_repoll: push wake re-arm (sticky on failure)
  for gent in resubmit: fail_ent(EIO) first; push REGISTER; slots.on_register_submitted(e)
  for m: for e in m.slots.register_retries_due(now): push REGISTER; on_register_submitted(e)
teardown: §11.2
```

### 8.2 The CQE error ladder (fou.rs:6566–6912), in order
`+1 fuse_over_uring_cqe_errors`; `Cancel` ⇒ `cancel_cqe_action` (§10); `Fetch` ⇒ `zc_fetch_complete` (never
protocol-fatal); disconnect errno ⇒ `disconnect = true` (break; `shutdown` after the loop); `Release` ⇒ warn +
`fuse3_zc_release_failures`; `Commit ∧ EINVAL ∧ Retained` ⇒ `on_retain_refused` + plain re-commit
(`fuse3_zc_retain_refused`); `ENOTSUP|EINVAL|ENOSYS` ⇒ "kernel rejected protocol" ⇒ `shutdown` + `Err`;
`Commit ∧ (EAGAIN|EINTR)` ⇒ re-commit the same cid (row 6; `Replied ⇒ on_commit_retry`, `Retained ⇒
on_retain_refused` + `retract_zc_retain_commit`), no cid in flight ⇒ re-REGISTER; other `Commit` error ⇒
re-REGISTER ("the kernel discarded the reply"); `Register` ⇒ `fail_ent(EIO)` first, then backoff/retire (§2.3).

### 8.3 Commit batching (fou.rs:1893–2108)
Every push routes through `push_cmd_batched` / `push_poll_batched` / `push_fetch_batched` — NO submit; SQ-full
⇒ `flush_submit` (records the partial batch) and retry the push once (fou.rs:2020–2071). `SubmitBatch{pending,
commits, commit_reads, commit_writes, bridge_pending}`; `note_flush` records `transport_commit_batch` (exact
buckets 1–8, ≤16, ≤32, >32; `COMMIT_BATCH_LABELS`, fou.rs:1812–1814) + `transport_commit_batch_flushes` /
`_commits`. Measured: 31 → 9 `io_uring_enter` per create (`test_commit_pushes_defer_to_one_flush`,
fou.rs:9957–10004: "the 31-syscalls/create shape"). `commits / flushes ≈ 1` under storm = regression
(fou.rs:1876–1877). Pinned by `test_batched_push_sq_full_submits_and_continues` (10 pushes through a 4-deep SQ
⇒ ≥ 3 flushes, all 10 counted once, 10 CQEs arrive, fou.rs:10011–10057).

### 8.4 Spin-before-park (spin.rs; fou.rs:6327–6395) — knob OFF by default
Where the pass would block: `in_flight = any member any_owing() ∨ outstanding() > 0`; `busy = /proc/stat`
sampled on a shared 100 ms cadence; `spin_ns = worker_window_ns(cap, gap_ewma, in_flight, busy)` (2 × EWMA of
observed park gaps, refused past `WORKER_BUSY_CEILING_PCT`, 0 when idle or cap 0). Spin watches
`IORING_SQ_TASKRUN`, the CQ tail, `coalescer.is_armed()`, `active`. Caught ⇒ `transport_spin_absorbed` + the
non-blocking GETEVENTS enter; expired ⇒ `transport_spin_expired`. `SQUEEZEFS_FUSE_IO_URING_SPIN_US` cap
(0..=100000 µs), absent/0 = never spin — the shipped default ON MEASUREMENT: a 200 µs cap absorbed 69 % of parks
and moved no hop by a µs at +5.5 % CPU/op (spin.rs:36–45). Port: carry the knob row (knobs_and_rails row 91) and
the governor as an A/B lever; default path never spins.

### 8.5 Connection watch thread (fou.rs:4549–4614)
Every 250 ms `poll(fuse_fd, POLLERR|POLLHUP|POLLNVAL)` ⇒ `shutdown` on any; every `SLOT_WATCHDOG_INTERVAL =
5 s` (fou.rs:340) run `scan_overdue_slots` (fou.rs:4056–4121: a cell owing ≥ `SLOT_OVERDUE_NS = 5 s` ⇒ +1
`transport_slots_overdue` + a WARN carrying the fused residency, bridge gauges, park/rescue counters and the
group's wake snapshot: `owed`, `wake_armed`, eventfd `eventfd-count`, ring `SqHead/SqTail/CachedSqHead/
CqHead/CqTail` + armed `PollList` count from `/proc/self/fdinfo`, fou.rs:2224–2268, :4035–4052); while
`zc_bridge_pends > 0` tick every group's eventfd through its coalescer so parked workers run the deadline scan
(fou.rs:4566–4585). Port: fold into the rung-3 watchdog thread (charter §4 "deadline watchdog"); the `/proc`
reads stay (forensics only, off the hot path).

### 8.6 Composition on the port's `ParkedRing`
`ParkedRing` already owns: the eventfd (NONBLOCK, level-triggered), the armed wake op at `wake_ud = u64::MAX`,
`consumeWake` (= the poll-marker branch setting `wake_cqe_seen`), `service()` (= disarm), `Wake.wake()` (=
arm → write), `park(tick)` (= the bounded/unbounded enter), `markDead`/`parkDead` (the dead-ring park the
reference lacks — a disconnect today just `break`s), `quiesce`/`deinit` (cancel the standing wake op before the
ring dies). The reference arms a `PollAdd(Fixed(1), POLLIN)` and drains with `read(2)`; the port arms a
`READ(eventfd)` which both wakes AND drains (the counter is zeroed by the completed read). Equivalence: a READ on
a level-triggered counter completes iff the counter is nonzero, exactly like POLLIN; a write landing between
the READ's completion and re-arm leaves the counter nonzero and completes the re-armed READ at once. The
"drain iff wake CQE seen or armed" economy then collapses to: the READ CQE IS the drain; `service()` disarms.
Keep `wake_cqe_seen`'s role only for the rescue ledger's `tick_wake_seen`.

---

## 9. The fused lane (fused.rs) and the filesystem interfaces

- `FusedLane{tasks: Slab<FusedTask>, rq: Arc<FusedRunQueue>, capacity = group_depth, watch, panics}`
  (fused.rs:218–248): one per worker, owned exclusively by it; only the run queue (`Mutex<VecDeque<(id,
  push_ns)>>` + a wake sink) is shared with wakers (fused.rs:103–160).
- Waker (fused.rs:194–206): `rq.push(id)` = `push_local` (lock, push_back with a stamp) then `sink.wake()` (arm
  → eventfd write) UNLESS the pusher IS the lane's own thread (`CURRENT_FUSED_LANE` thread-local matches
  `lane_id`, fused.rs:134–141) — a self-wake is a wasted syscall since the worker drains before parking.
- `spawn(fut, unique)` (fused.rs:270–282): refuse at capacity (`false` ⇒ the caller demotes, never refuses the
  request); `push_local` only (admission runs on the worker).
- `drain_one()` (fused.rs:296–341): pop; stale id (completed slab slot) ⇒ skip; record `wake_to_poll` span;
  poll under `catch_unwind`; `Ready` ⇒ remove + `note_retire`; panic ⇒ remove, +1 `panics`, loud error (the
  `ReplyTx` drop-guard synthesizes the reply). PORT: no unwinding (decision 0002 row 9) — a handler task's reply
  obligation is the slot's `fail_ent`, not a drop-guard; a bug aborts loud.
- Capacity is structural: a fused task exists only while its ent owes a reply (fused.rs:52–57).
- Teardown drops the lane FIRST so parked futures release their leases and the row-8 `owing()` pass synthesizes
  each fused slot's EIO exactly once (fou.rs:7838–7850).
- Interfaces (fused.rs:432–437, :510–519; fd.rs:105–115): `FusedWriteDispatch{mint}`; `ZcHoldGate = fn(ino,
  offset, len, odirect) → bool` (cheap, sync, lock-free; unregistered ⇒ never hold); `ReadFastDispatch{probe,
  mint}` (§5.4). Registered once after INIT, first set wins (fou.rs:3703–3720).
- Levers: `SQUEEZEFS_FUSE_ZC_WRITE_FUSION` default ON (the 2026-08-08 falsification was the PREDICATE, not the
  lane; with the hold gated on W1 eligibility the fused arm reads ≥ fusion-off, +11.6 % on the W1 shape,
  fused.rs:403–419); `SQUEEZEFS_FUSE_ZC_READ_FUSION` default OFF (§5.4); `SQUEEZEFS_FUSE_ZC_FUSION_MAX` derived
  `payload/8` floor 4096 (fused.rs:381–402; tie test: 1 MiB → 128 KiB, 4 MiB → 512 KiB, 8 KiB → 4096,
  fused.rs:744–758). `fuse_candidate(len, ceiling, on) = on ∧ len > 0 ∧ len ≤ ceiling` (fused.rs:377–379;
  ceiling inclusive, zero-length never).
- Gauges: `fuse3_zc_write_fusions/_bytes/_demotions`, `fuse3_zc_read_fusions/_demotions`,
  `fuse3_zc_write_lazy_extractions`, `fuse3_fused_midpass_reaps`, `fuse3_fused_passbottom_reaps`,
  `fuse3_fused_timeline_ns`.
- Fused-lane tests to port (fused.rs:617–730): ready future completes on first drain; parked future resumes on a
  cross-thread wake AND arms the eventfd; capacity refusal is a demotion signal; stale wakes harmless. (The
  panic-containment test is not ported — row 9.)

---

## 10. Bridges — protocol only (`READ_FIXED`/`WRITE_FIXED` to/from slots)

- Ops (fou.rs:5763–5975): `ZcFetch{fd, off, len}` ⇒ `ReadFixed(Fd(fd), null, len, buf_index = ent_idx).offset(off)`
  tagged `Fetch/gent`; `ZcStore{fd, dev_off}` ⇒ `WriteFixed(Fd(fd), null, held_len, ent_idx).offset(dev_off)`;
  `ZcExtract` ⇒ `WriteFixed(Fd(bounce_fd), null, held_len, ent_idx).offset(bounce_off)`; the worker's own
  bounce bridge (`commit_ready_reply` zc arm, fou.rs:5078–5088) ⇒ `ReadFixed(Fd(src_fd), null, body_len,
  ent_idx).offset(src_off)` with `src = (body_fd, off)` or `(bounce_fd, bounce_off)`. Refusals answer the
  oneshot: bad slot EINVAL, pending bridge EBUSY, no held payload ENOENT, push failed EIO.
- Per-ent pend (`ZcPend`, zc.rs:277–339): at most one per ent (an ent serves one request between two commits):
  `BounceFetch{header, body, cid, len, fd_source}`, `HandlerFetch{done, trace_id}`, `HandlerStore{done}`,
  `LazyExtract{done, len}`, `WriteExtract{header_and_op, unique, cid, len}`. Resolution
  (`zc_fetch_complete`, fou.rs:5145–5313): handler pends forward the raw CQE `{res, popped_ns}`;
  `BounceFetch` full-length ⇒ `apply_reply_zc` + commit; fd-source short ⇒ `Restage` through the bounce
  (`fuse3_zc_fd_body_fallbacks`); bounce short ⇒ kmbuf attachment fallback or EIO (`fuse3_zc_fallbacks`);
  extractions full-length ⇒ mint the lease over the bounce and deliver / answer.
- Deadline ledger (`BridgeDeadlines`, zc.rs:402–513): `stamp(ent, now)` at every pend SET (true = new ⇒
  pool gauge +1), `note_submitted` at the carrying enter, `clear` at any resolution, `overdue(now, timeout)`
  yields each ent EXACTLY ONCE per pend lifetime (cancel-once latch; a re-stamp resets it).
- Deadline: `SQUEEZEFS_ZC_BRIDGE_TIMEOUT_MS` 100..=600000, default 30 000 (fou.rs:1644–1660 — "the D1.b
  op-watchdog threshold's transport twin"). Overdue ⇒ +1 `fuse3_zc_bridge_cancels` (must-stay-0) +
  `AsyncCancel(user_data = Fetch/gent)` tagged `Cancel/gent` (fou.rs:6191–6213).
- The cancel's OWN CQE (`cancel_cqe_action`, zc.rs:547–557): pend gone ⇒ `Nothing`; `0` ⇒ `AwaitOriginal`
  (-ECANCELED is coming); `-ENOENT` with a live pend ⇒ `SynthesizeLost` — THE proven lost-completion shape (the
  CQ is FIFO, the op's CQE preceded this one, a live pend means it never reached us) ⇒ `fuse3_zc_bridge_lost` +
  resolve with `-ETIMEDOUT` (fou.rs:6587–6650); `-EALREADY`/unknown ⇒ `Restamp` (still running kernel-side; a
  synthesis would let a late DMA alias a recycled slot).
- Orphan sweep (fou.rs:6165–6190): a live pend with `born == 0` is re-stamped loud (`fuse3_zc_bridge_orphans`).
- Must-stay-0: `fuse3_zc_bridge_cancels`, `fuse3_zc_bridge_lost`, `fuse3_zc_bridge_orphans`. Pinned live by
  `lost_write_bridge_cqes_resolve_through_the_deadline_ladder` /
  `lost_overlay_store_cqe_resolves_through_the_deadline_ladder` / `healthy_armed_mount_fires_no_bridge_tripwires`
  (tests/zc_bridge_cqe_wedge_tests.rs:278, :403, :339; sqz kernel + root).
- Teardown: parked `BounceFetch` pends rejoin the drain as header-only EIO commits (their bodies never reached
  the pages); handler pends unblock by sender drop; `WriteExtract` slots are still OWED and the row-8 pass
  synthesizes them (fou.rs:7856–7904).

---

## 11. Teardown (fou.rs:7822–8167) and every must-stay-0 tripwire

### 11.1 `shutdown()` (fou.rs:4148–4182)
`ready = false`; `active.swap(false)` gates the side effects (idempotent — `shutdown_is_idempotent`,
fou.rs:9309–9319): `fuse_over_uring_sessions_active −= 1`, a WARN per owed cell ("the queue drain owns it"); wake
every parked session pull (`notify_waiters` after the `active` store); UNCONDITIONAL eventfd write to every
group (no coalescer: the worker must wake to see `active == false` whatever the flag says — `Wake.force()`,
ring_park.zig:79–83). Triggers: a worker `Err`, the watch thread's POLLERR/HUP, a disconnect CQE, kernel
protocol rejection, every ent of a member retired, `Drop`, the session's disconnect/DESTROY.

### 11.2 The worker's drain, in order
1. Drop the fused lane (leases release; §9).
2. `final_msgs = parked_msgs (was_parked = true) ‖ parked BounceFetch pends as header-only EIO (true) ‖ every
   Commit still in commit_rx (false)`; bridge/release messages are dropped (sender drop unblocks the handler;
   a teardown-raced ZcRelease is moot — the kernel drains retained ents with -ECONNABORTED).
3. Return every bridge pend's share of the pool pend gauge (fou.rs:7905–7917).
4. `drain_await_leases` on the group's eventfd for every final message whose ent still reads `Parked`
   (§6.5), budget 100 ms total.
5. Apply + `submit_commit` each final message: freed ⇒ full reply (+1 `transport_unparked_commits` if it had
   parked); still leased ⇒ header-only EIO (NOT counted as an unpark).
6. Row 8 (fou.rs:8001–8042): `fail_ent(EIO)` for every slot still `owing()`; a push failure ⇒ `abandon` + clear
   the watch cell (the honest tripwire case).
7. `note_flush`; `submit_and_wait(final_commits)` or a plain `submit()` (fou.rs:8049–8056).
8. REPLY RESIDENCY (finding 32, fou.rs:8058–8164): stay resident up to 30 s, non-blocking reap every 10 ms,
   answering every FRESH delivery on a still-registered ent with `fail_ent(ENOTCONN)` (the unmount's own
   FLUSH/RELEASE against this session arrives AFTER the drain — the cloud assemble's 59-minute wedge); exit on a
   disconnect CQE/enter error; the backstop lapsing with slots owing ⇒ `transport_requests_abandoned += owed`.
9. Retained slots drain kernel-side (-ECONNABORTED on the parked cmd) — nothing to do (fou.rs:386–393).
10. Worker exit; `try_start`'s `workers` handles are joined by the owner; the fuse fd is closed by the connection.
    PORT: `ParkedRing.quiesce()` on the worker thread (cancel the standing wake op, reap its CQE), then
    `deinit` from the joiner (ring_park.zig:158–165).

### 11.3 Must-stay-0 list (charter §4 + this module)
`transport_requests_abandoned` (fou.rs:2170–2175, :2351–2352), `transport_cq_overflows` (fou.rs:2270–2286),
`transport_replies_oversize` (fou.rs:2361–2363), `fuse3_zc_bridge_cancels`, `fuse3_zc_bridge_lost`,
`fuse3_zc_bridge_orphans`, `fuse3_zc_release_failures` (fou.rs:6743–6753), `fuse3_zc_retain_refused`
(fou.rs:6754–6779), `read_dest_overruns` and `open_count_stranded` (charter §4 — filesystem-side, rung 4).
≈ 0 (lost-wake tripwires, benign only in the µs race between tick expiry and the pump):
`transport_park_tick_commit_rescues`, `transport_park_tick_cqe_rescues` (fou.rs:2191–2203; the port's CQE half
is the tripwire `transport_park_tick_cqe_lost_wakes` beside the plain `transport_park_tick_late_cqes`, §8 step 14),
`transport_slots_overdue`, `transport_ents_retired`, `fuse3_zc_write_fusion_demotions`,
`fuse3_zc_read_fusion_demotions`, `fuse3_zc_write_lazy_extractions`. Closure identities: `parked_commits ≡
unparked_commits` at quiesce; `leases_outstanding → 0` at quiesce; `serves + fusions + demotes ≡ READs`;
`spin_absorbed + spin_expired ≡ spins run`; `commit_batch_commits / flushes` = mean batch.

---

## 12. Every knob read by this module (registry rows in `docs/porting/knobs_and_rails.md`)

| knob | kind / range | default | class (port) | read at |
|---|---|---|---|---|
| `SQUEEZEFS_FUSE_OVER_IO_URING_QUEUES` | Int 1..=512 | kernel possible CPUs | **test seam** (rename `SQUEEZEFS_TEST_FUSE_QUEUES`; fewer never becomes ready — "testing only", fou.rs:30–33) | `resolve` fou.rs:2735 |
| `SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH` | Int 1..=32 | derived (32 → budget → floor 4) | derived override | fou.rs:2738 |
| `SQUEEZEFS_TRANSPORT_MEM_MAX` / `_PCT` | Int MiB / 1..=100 | derived budget/8 | derived override (already knobs.zig:101–102) | cap for `plan` |
| `SQUEEZEFS_FUSE_DRAIN_GROUP` | Int 1..=512 | derived `max(node_cpus/4, 1)` | derived override | fou.rs:953 |
| `SQUEEZEFS_FUSE_PIN_SCOPE` | Enum node,core | node | enum select | fou.rs:4661 |
| `SQUEEZEFS_FUSE_READ_FAST_DISPATCH` | Bool | on | A/B lever | fd.rs:130 (once per process) |
| `SQUEEZEFS_FUSE_ZC_WRITE_FUSION` | Bool | on | A/B lever | fused.rs:420 (per worker at arm) |
| `SQUEEZEFS_FUSE_ZC_READ_FUSION` | Bool | off | A/B lever | fused.rs:467 |
| `SQUEEZEFS_FUSE_ZC_FUSION_MAX` | Int 4096..=2^30 | derived payload/8 | derived override | fused.rs:394 |
| `SQUEEZEFS_FUSE_IO_URING_SPIN_US` | Int 0..=100000 | 0 (off) | A/B lever | spin.rs:74 |
| `SQUEEZEFS_FUSE_IO_URING_SQPOLL_IDLE_MS` / `_CPU` | Int | off / unpinned | A/B lever ("measured not recommended") | fou.rs:4229–4233 |
| `SQUEEZEFS_ZC_BRIDGE_TIMEOUT_MS` | Int 100..=600000 | 30000 | **constant** on the port (`zc.BRIDGE_DEADLINE_NS = 30 s`, "the op-watchdog threshold's transport twin") unless a mechanism needs the lever — decision 0002 row 17 (no `tuning` class) | fou.rs:1650 |
| `SQUEEZEFS_TRANSPORT_DEBUG` | Bool | off | switch | fou.rs:1764 |
| `SQUEEZEFS_TEST_ZC_DROP_WRITE_CQES` | Int 0..=1e6 | 0 | test seam | fou.rs:1668 |
| `SQUEEZEFS_TEST_DROP_COMMIT_WAKES` / `_TID` | Int | 0 / unset | test seam | fou.rs:1692, :1716 |
| `SQUEEZEFS_FUSE_KMBUF`, `SQUEEZEFS_FUSE_ZC`, `SQUEEZEFS_FUSE_ZC_RETENTION` | Bool | on | A/B lever | buffer-mode spec |
| `SQUEEZEFS_FUSE_IO_URING_ENTRIES` | Int 64..=4096 | 1024 | **delete** (the classical rings' SQ depth; see Q1 — the port's INIT/sideband ring is a `ParkedRing` whose entries derive from the sideband's single in-flight Readv + notify fan-out) | uc.rs:31 |
Every read is through `knobs.boolKnob/intKnob/enumKnob` with the comptime row (decision 0002 row 16); the
reference's "trust the gate, keep the default on parse failure" (fd.rs:127–143 etc.) becomes the row's
default by construction.

---

## 13. Every gauge / counter / histogram (stats-inode key → meaning)

| key | type | meaning |
|---|---|---|
| `fuse_over_uring_requests` / `_replies` | sharded counters (fou.rs:1789–1790) | deliveries dispatched; replies submitted (incl. FORGET auto-commits and inline serves) |
| `fuse_over_uring_cqe_errors` / `_registers` | counters | negative-result CQEs; REGISTER SQEs pushed at arm |
| `fuse_over_uring_sessions_active` | gauge | `try_start` +1 / `shutdown` −1 |
| `transport_queues/q_depth/payload_buffer_bytes/payload_buffers/max_background/max_write/max_pages/max_readahead` | level gauges | the armed geometry (§1.7) |
| `transport_drain_groups` / `transport_drain_group_width` | gauges | plan shape; `groups == queues` ⇒ lever inert |
| `transport_commit_batch` (buckets 1–8, ≤16, ≤32, >32), `transport_commit_batch_flushes`, `_commits` | histogram + counters | COMMIT SQEs per flush; mean ≈ 1 under storm = regression |
| `transport_wake_writes` / `transport_wakes_elided` | counters | eventfd writes paid vs elided by the group coalescer (reply, lease drop, bridge msgs, fused wakes, watch ticks) |
| `transport_payload_leases` | counter | FUSE_WRITE payloads delivered as leases (adoption proof) |
| `transport_dest_dma_leases` | counter | read-destination owner tokens claimed (≈ one per dest-bearing device READ SQE) |
| `transport_parked_commits` / `transport_unparked_commits` | counters | commits parked behind a lease / later committed; `parked − unparked` at quiesce = wedge count |
| `transport_leases_outstanding` | gauge | live leases; 0 at quiesce |
| `transport_lease_max_age_ms` | high-water | severance-boundary mark (bounded by one handler invocation) |
| `transport_lease_overlong` | counter | leases ≥ 1 s (loud, never fatal) |
| `transport_requests_failed_synthetic` | counter | header-only errors the transport synthesized (CQE reclaim, failed push, teardown, guard) — honest, not a bug by itself |
| `transport_requests_abandoned` | **must-stay-0** | a delivered request left its slot with no commit |
| `transport_replies_refused_stale` | counter | double / late / foreign-cid replies refused |
| `transport_replies_dropped_no_slot` | counter | replies whose slot no longer holds the request (kept OUT of the reply gauge) |
| `transport_replies_oversize` | **must-stay-0** | body > ent payload ⇒ EIO instead of truncation |
| `transport_ents_retired` | counter | ents retired after 8 REGISTER failures |
| `transport_slots_overdue` | counter | slots seen owing ≥ 5 s by the watchdog |
| `transport_cq_overflows` / `transport_cq_nodrop` | **must-stay-0** / probe | kernel-dropped CQEs; `IORING_FEAT_NODROP` probed |
| `transport_park_backstop_ticks` | counter | bounded parks that ticked out (nonzero under traffic = backstop WORKING) |
| `transport_park_tick_commit_rescues` | ≈ 0 tripwire | messages pumped by the pass after a tick with no wake delivered |
| `transport_park_tick_cqe_lost_wakes` | **must-stay-0** (port) | a tick's re-check found completions in a CQ empty at the park (`DEFER_TASKRUN`) |
| `transport_park_tick_late_cqes` | counter (port) | completions the reap after a tick surfaced: landed after the deadline (the reference's `_cqe_rescues`) |
| `transport_fast_dispatch_serves` / `_demotes` | sharded counters | READ law arms (a) / (c) |
| `fuse3_zc_read_fusions` / `_demotions`, `fuse3_zc_write_fusions` / `_bytes` / `_demotions`, `fuse3_zc_write_lazy_extractions` | counters | fused lane engagement (§9) |
| `fuse3_fused_midpass_reaps` / `_passbottom_reaps` / `fuse3_fused_timeline_ns` | counters / histogram | where handler bridges resolved; issue→resolution spans |
| `transport_reap_gap_ns` | histogram (read_phase) | blind window between completion-surfacing enters; `blind` mean 6 µs on the field row (spin.rs:11–12) |
| `read_transport_phase_ns` / `write_transport_phase_ns` (`queue_wait`, `dispatch_lag`, `commit_flush`, `transport_total`) | exact-sum phase histograms | ingress residence; served READs record exact zeros |
| `zc_bridge_phase_ns` (`msg_hop`, `sq_wait`, `device_cq`, `wake_hop`, `total`; Σ hops ≡ total) | histogram | the bridge decomposition (field: msg_hop 28, device_cq−device ≈ 50, wake_hop 36 µs) |
| `transport_spin_absorbed/expired/ns/refused_busy/window_us` | counters / gauge | spin governor ledger |
| `transport_classical_sideband` | counter | post-arm classical deliveries serviced (0 after an unlink storm = stranding) |
| `fuse3_numa_local_bytes` / `_remote_bytes` | counters | payload bytes whose CPU pass was / was not a minimal-distance choice |
| `fuse3_zc_bridge_cancels` / `_lost` / `_orphans`, `fuse3_zc_release_failures`, `fuse3_zc_retain_refused` | **must-stay-0** | bridge ladder + retention tripwires |
Port names: keep every `transport_*` and `fast_dispatch_*` key; the `fuse3_*` prefix is a crate name (decision
0002 row 21 spirit) — rename to `transport_zc_*` / `transport_fused_*` / `transport_numa_*` and record the row.

---

## 14. Constants with WHY, and measured numbers

| constant | value | WHY (cite) |
|---|---|---|
| `FUSE_URING_IN_OUT_HEADER_SZ`, `OP_IN_OUT_SZ` | 128, 128 | kernel ABI (fou.rs:111–112) |
| `fuse_uring_cmd_req` | 24 B, union u16 at +18 | ABI; `init.flags`/`commit.flags` share bytes (fou.rs:153–169) |
| `FUSE_IO_URING_CMD_{REGISTER,COMMIT_AND_FETCH,RELEASE_PAYLOAD}` | 1, 2, 3 | ABI; 3 is the sqz series (fou.rs:114–119) |
| `FUSE_IN_HEADER_SIZE` | 40 | ABI (fou.rs:120) |
| `FUSE_OVER_IO_URING_FLAGS2` | 1<<9 | `1ULL<<41` → flags2 bit 9 (fou.rs:109) |
| `FUSE_MIN_READ_BUFFER` | 8192 | kernel floor in `max_payload_sz` (fou.rs:2820) |
| `Q_DEPTH_DESIRED` / `Q_DEPTH_FLOOR` | 32 / 4 | measured-best / pre-L1 shipped (fou.rs:2603–2610) |
| `PAYLOAD_BASE` | 1 MiB | yesterday's ent size; never regress below (fou.rs:2611–2617) |
| `MAX_BACKGROUND_FLOOR` | 64 | the historical `max_background=64` intent (fou.rs:2618–2622) |
| default buffer cap | RAM/8 | scale-free fraction; no fixed ceiling (a 2 GiB ceiling bound depth at ≥ 64 CPUs for no physical reason, fou.rs:2624–2633) |
| `max_pages_limit` fallback | 256 | the compiled-in default of kernels lacking the sysctl (fou.rs:2644–2649) |
| REGISTER backoff | 1 ms ×2 → 128 ms, 8 attempts | no core spin, no minutes to recover (fou.rs:441–450) |
| `SLOT_WATCHDOG_INTERVAL` / `SLOT_OVERDUE_NS` | 5 s / 5 s | name a WEDGE, not a slow handler (fou.rs:336–341) |
| `PARK_BACKSTOP_TICK` | 100 ms | the ipc dd-reaper's shipped backstop cadence (fou.rs:342–346) |
| `RESCUE_LOG_INTERVAL` | 60 s | per-worker attribution WARN rate limit (fou.rs:347) |
| `DRAIN_LEASE_BUDGET` | 100 ms, whole group | was 100 ms per ent serially = 3.2 s at depth 32 (fou.rs:2414–2418) |
| arm barrier deadline | 30 s | `try_start` (fou.rs:3193) |
| residency backstop | 30 s, 10 ms poll | teardown only (fou.rs:8077, :8163) |
| `SQPOLL_LEADER_WAIT` | 10 s | inside the 30 s barrier (fou.rs:4257–4261) |
| cqsize | sq × 2 | worst pass `2·depth + 1` (fou.rs:2282–2285) |
| sq entries | `next_pow2(width·depth + 8).max(16)` | fou.rs:4702–4703 |
| arena stride | `payload_sz` rounded up to 4096 | page-aligned ents, one vma (fou.rs:1176) |
| bridge deadline / scan period | 30 s / deadline÷1024 (29 ms) | op-watchdog twin; fixed fraction of the bound (fou.rs:1644–1650, :5473–5483) |
| fusion ceiling | payload/8, floor 4096 | 2 wakes + 2 schedules ≈ 5–10 µs ≈ 50–100 KiB at ~10 GB/s merge copy (fused.rs:385–392) |
| drain width | node_cpus/4, floor 1 | §3 (fou.rs:923–946) |
| commit-batch buckets | 1–8 exact, ≤16, ≤32, >32 | a "≈ 1" verdict never rides rounding (fou.rs:1816–1819) |
Measured numbers quoted in comments: QD32+mb256 = 44k → 316k IOPS (7.2×, fou.rs:2596–2598); drain width 8 on
32 CPUs: batch 1.48 → 2.98, wakes/op 0.687 → 0.340 (fou.rs:938–942); 32×8 = 273k vs 8×32 = 357k IOPS, +21 %
pinned (fou.rs:967–969); whole-node width −15 % (fou.rs:931–932); 31 → 9 enters/create (fou.rs:9980–9982);
1.67 eventfd writes/op pre-L3 (wc.rs:7–9); 2.07 read(2)/op vs 1.01 wakes/op (fou.rs:2385–2386); 1.8
read(2)/op pass-top drain (fou.rs:5453–5454); ~7.5 ops/pass × 32 / 600 µs ≈ 420k IOPS, devices busy ~50 µs,
in-flight ≈ 21 (fou.rs:5540–5544); T2 per-poll enter 2.7 syscalls/op, bridge RTT ÷10, row −6 % (fou.rs:5725–5728);
`queue_wait` 69 + `dispatch_lag` 89 = 158 µs of a 439 µs 4 KiB read, device 40 µs (fd.rs:10–13); ingress
mechanism 6.7 µs hop / 4.0 µs lane mint / 53 ns inline (fd.rs:13–15); bridge hops 35–47 µs each around a 40 µs
DMA (fused.rs:447–450); read fusion alone +15.5 %, composed −6.5 % (481k vs 514k), p50 +70 µs, p99 1.5 vs
4.3 ms (fused.rs:456–465); spin 200 µs cap absorbed 0.50 parks/op = 69 %, +5.5 % CPU/op, no hop moved
(spin.rs:38–43); worker parks 0.76×/op, 84 % ≤ 32 µs, pass work 6 µs (spin.rs:11–14); fused +11.6 % on the W1
shape, field falsification 0.45× at ~300 µs RTT (fused.rs:405–417); hold-all-lazy 0.61× durable streaming
(zc.rs:349–352); `waiting=28` live hung gate (fou.rs:1385); 23 min unpumped LOOKUPs, 435 s pends=4 (fou.rs:6264–6269);
59-minute cloud wedge (fou.rs:8066); 200 ms `pop_timeout` 2.2 % clock (fou.rs:10620–10621).

---

## 15. Tests (name → one sentence; venue)

In-module (portable, no mount): the slot-state suite (§4.6, 16 tests); `apply_reply_tests` ×2 (§4.5);
`test_header_sizes` (288 / 24); `test_write_opcode_matches_abi` (16); `test_flags2_bit`;
`test_cmd_entry_wire_encoding` (§1.5); `test_commit_batch_histogram_buckets`; the 13 geometry plan tests
(§1.7); `test_payload_arena_buffers` (Linux: mmap + eventfd dup); `test_lease_drop_wakes_parked_worker` (a drop
with a parked commit fires the eventfd; clone keeps the owner alive); `test_dest_dma_lease_multi_token_parks_until_last_drop`
(only the last token wakes); `test_dest_window_index_resolution_and_straddle_refusal`; `lease_core` ×3
(`gate_parks_while_leased_and_unparks_after_drop`, `gate_ready_when_lease_already_dropped`,
`recheck_ready_clears_parked`); `wake_core` ×2 (`first_arm_writes_rest_elide_until_disarm`,
`disarm_without_arm_is_harmless`); `drain_group_tests` ×9 (§3); `cq_overflow_tests` ×5 (quiet ring 0; deltas
reported once; wrapping delta `u32::MAX−1 → 1` = 3; tripwire moves; nodrop published); `drain_wait_tests` ×3
(§6.5); `inbound_queue_tests` ×3 (FIFO; a parked popper wakes < 100 ms; shutdown → None < 100 ms — replaced on
the port by the lane handoff's own tests); fused-lane ×5 (§9); `fast_dispatch` ×2 (`candidate_is_read_and_lever_and_registration`,
`record_served_accounts_zero_ingress_and_the_exact_total`); `spin` ×4; sim-pool tests
(`teardown_hands_owed_slots_to_the_drain_so_late_replies_report`: pre-shutdown Ok, post NotFound;
`classical_slot_never_commits`; `shutdown_is_idempotent`; `group_owed_count_follows_the_slot_watch_transitions`;
`slot_watch_clear_reports_whether_the_cell_held_a_unique`; `is_ready_requires_ready_and_active`).
Linux ring tests (venue, no mount): `getevents_flush_materializes_deferred_completions_without_parking` (needs
DEFER_TASKRUN; pipe read made ready after submit is invisible to plain submits, visible after GETEVENTS);
`test_commit_pushes_defer_to_one_flush`; `test_batched_push_sq_full_submits_and_continues` (a uring cmd on an
eventfd fixed file completes as an error CQE — enough for accounting); the SQPOLL suite (×6; skipped unless
the knob exists).
Loom → explorer models: the five in §6.6.
Session side: `reply_guard_tests` ×5 (session.rs:6631–6793: dropping an owing handle synthesizes; a replied
handle synthesizes nothing; in-place error is the one reply; a no-reply handle stays silent; a dead reply task
is reported) — on the port these become slot-table `fail_ent` facts, no drop-guards; `init_negotiation_tests`
(`the_kernel_uring_enable_precedes_the_init_reply`, never advertise splice / flock / posix locks / posix acl;
echoes implemented caps; minor never exceeds the offer); `over_uring_slot_tests` ×5 (uc.rs:2093–2263:
empty slot routes classical; install once; racing installs admit one; venue follows pool liveness; teardown keeps
the pool reachable).
Root suite needing a real mount (Linux venue, root, fusectl; `sqz` kernel where noted):
`test_every_request_gets_exactly_one_reply` (abandoned = overdue = retired = 0 after a mixed 8-thread storm; a
clean unmount is the kernel's verdict); `test_default_mount_transport_geometry_and_init_limits`,
`test_small_budget_mount_degrades_q_depth_gracefully`, `test_env_q_depth_override_wins_over_budget`,
`test_mount_option_max_background_override` (tests/transport_concurrency_tests.rs); the four geometry gauge
tests (tests/transport_geometry_tests.rs); `a_lost_commit_wake_resolves_within_the_bounded_park` (the
`SQUEEZEFS_TEST_DROP_COMMIT_WAKE_TID` seam: a stat answered ENOENT within 3 s, `park_tick_commit_rescues ≥ 1`),
`a_healthy_mount_ticks_nothing_it_owes`, `a_fresh_mount_issues_no_request_to_itself`
(tests/commit_wake_loss_tests.rs); `overlong_write_lease_is_a_tripwire_not_a_lost_reply`;
`warm_active_buffer_read_is_served_byte_exact_and_eof_clamped`, `cold_block_demotes_then_the_tier_resident_block_serves_into_the_dest_window`,
`a_writer_holding_the_inode_lock_demotes_in_bounded_time`, `virtual_inodes_demote`,
`served_op_records_zero_ingress_and_the_fast_dispatch_trace_chain`,
`live_armed_session_reads_are_all_fast_dispatched_and_close_against_inplace_replies`
(tests/read_fast_dispatch_tests.rs — the first five on an in-process filesystem harness, the last on a mount);
`cold_zc_reads_decompose_into_four_exact_hops_with_a_per_op_chain`,
`composed_dispatch_law_partitions_every_read_on_a_zc_session` (tests/zc_bridge_phase_tests.rs; sqz kernel +
CAP_SYS_ADMIN); the three zc-bridge-wedge tests (sqz kernel); `dest_lease_serves_cold_windows_by_dma_and_stands_the_fill_machinery_down`
(tests/read_dest_lease_tests.rs); `default_posture_is_node_scoped_lever_restores_core_pins`
(tests/transport_ingress_tests.rs).
Reference-only, not ported: panic containment (fused.rs:696–717; row 9), `pop_timeout` timing tests (the
inbound queue is replaced), the SQPOLL poller-census tests unless the lever is built.

---

## 16. QUIRK LEDGER — reference choices NOT carried (decision 0002; each needs a row before diverging)

| # | reference quirk | why it is poor | what the port does |
|---|---|---|---|
| Q1 | The classical `/dev/fuse` connection builds TWO io_urings per connection (read + write) with `IORING_REGISTER_EVENTFD`, a `Mutex<IoUring>` around each, and parks on an epoll fd-watch of the ring eventfd (uc.rs:1206–1403); `SQUEEZEFS_FUSE_IO_URING_ENTRIES` = 1024 (uc.rs:31–37) sizes them | libfuse/tokio lineage: a ring per direction, mutexed, driven by an async fd watcher — three mechanisms for "one Readv in flight"; a free-floating 1024 | ONE `ParkedRing` for the INIT read + sideband (Readv armed like the wake, notify Writev on the same ring); entries derived (1 Readv + notify fan-out + wake); knob deleted |
| Q2 | Delivered requests are re-framed into CLASSICAL `[header 40][arg0][arg1+]` bytes (uc.rs:843–972) with a "header len wrong/zero" heuristic (payload-first, then a NUL-terminated op_in) so libfuse-shaped handlers parse them | A codec built to feed a vendored libfuse dispatcher; a heuristic in a wire decoder | Typed `extern struct` decode of `fuse_in_header ‖ op_in` per opcode (charter §12); payload is a separate slice/lease; no reconstruction, no heuristic — a malformed header is a counted refusal |
| Q3 | Handler futures carry a `ReplyTx` drop-guard that synthesizes EIO when dropped/panicked (session.rs rows 1–3; fused.rs `catch_unwind`) | Rust unwinding as the error boundary (row 9) | The reply obligation is the SLOT's: a handler that cannot answer returns an error the dispatcher turns into `fail_ent(errno)`; no guards, no unwinding |
| Q4 | `SQUEEZEFS_FUSE_OVER_IO_URING_QUEUES` as a `tuning` knob that documents itself as "testing only" | A product knob that can only break the mount (fewer queues never becomes ready) | `SQUEEZEFS_TEST_FUSE_QUEUES` test seam |
| Q5 | Unbounded `std::sync::mpsc` commit channel + `Vec<u8>` header per `CommitMsg` + `Bytes` refcounts (fou.rs:190–197, :866–868) | Per-reply heap allocations on the hot path (charter §12); unbounded = the reply task can never backpressure | `sync/mpmc.zig` queue over preallocated cells sized `group_depth` (a reply exists only for a delivered slot — the bound is structural, like the fused lane's); header in a fixed `[16 + max_extra]u8` cell; a full queue is a tripwire, not a block |
| Q6 | `decode_user_data` maps an UNKNOWN tag to `Register` "rather than dropping the CQE" (fou.rs:831–834) | A protocol breach treated as the most common case | Unknown tag ⇒ `transport_cqe_tag_unknown` tripwire + ignore; it cannot happen on a ring only we push to |
| Q7 | The level-triggered PollAdd wake + a separate `read(2)` drain, with a "drain iff seen or armed" peek economy (fou.rs:2380–2399, :5453–5524) | Two syscalls to consume one wake; an economy heuristic to skip one of them | The port's armed READ on the eventfd both wakes and drains (§8.6); `service()` disarms; no peek |
| Q8 | `submit_with_args(1, ts)` as one enter for submit + timed wait (fou.rs:6430–6446) | A submitting enter discards the wait's ETIME — the tick is unobservable on a pass that pushed anything (ring_park.zig:20–26 found this red-first) | Timed park = submit, then wait (`ParkedRing.park`); untimed = `submit_and_wait(1)`; saturated passes never wait |
| Q9 | `SQUEEZEFS_ZC_BRIDGE_TIMEOUT_MS` as a `tuning` row (knobs row 111) | A product bound with a knob on top (row 17) | A documented constant `30 s` ("the op-watchdog threshold's transport twin"); re-add as an A/B lever only with a control arm |
| Q10 | The group OWED count is a THIRD copy of slot state (`SlotWatch` cells + `GroupHandle.owed` beside the `SlotTable`) maintained by swap/fetch_add pairs (fou.rs:291–333) | Two shadow ledgers of one authority so a foreign thread (the watchdog) can read it | ONE per-ent `owed_since_ns: atomic u64` (0 = owes nothing) written by the worker at deliver/commit; `owed` is the worker's own `owing().count()` (it is the only reader of its park gate); the watchdog folds the cells |
| Q11 | Thread names `f3-ur<qid>` / `f3-ur<a>-<b>` + per-mount `m<x>` suffix (fou.rs:3155–3165) | Crate name in dashboards; 15-char comm overflow (rows 20–21) | `sqz-fuse-ur<qid>` of the first member; no suffix |
| Q12 | `fuse3_*` stats prefixes | Crate name as a metric namespace | `transport_zc_*`, `transport_fused_*`, `transport_numa_*` |
| Q13 | Wake-fd and fuse-fd both fixed, but a `register_files` failure fails the mount while the classical ring's `try_register_fuse_fd` degrades to a raw fd (uc.rs:68–76) | Two postures for one fact | Fixed files are required on the transport ring (the cmd needs `Fixed(0)`); the sideband ring follows `ParkedRing.registerFile`'s degrade |
| Q14 | A 1 ms sleep-poll arm barrier on the main thread (fou.rs:3194–3218) and a 10 ms sleep-poll residency loop (fou.rs:8163) | Polling where a signal exists (charter §11: no sleeps for synchronization) | The barrier is a `sync/completion.zig` cell the last worker completes (the registration count is still submissions, §2.2); the residency loop is a bounded `ParkedRing.park(10 ms)` on the worker's own ring |
| Q15 | The inbound queue (`InboundQueue`: unbounded channel + an async mutex around the receiver, popped by a per-queue session task, fou.rs:1464–1517) between the worker and the handler lane | A tokio-era hop (channel push + eventfd + a dispatch task) that R-2 bypassed for READs only | Every delivery hands off directly to `sqz-fuse-lane<qid>` through the executor's lane queue (decision 0001; the READ law's arm (c) generalized); FORGET accounting rides the same handoff with `ReplySlot.classical` |
| Q16 | Transport-epoch stamps via `read_phase::transport_now_ns()` + `Instant::now()` mixed (fou.rs:1376, :2102, :6228) | Two clocks | `core/clock.monotonicNs` everywhere; no `std.Io` on the pass |

---

## 17. Port architecture map (reference piece → Zig module), and what this spec leaves to others

| reference | port module | notes |
|---|---|---|
| `TransportGeometry::plan/resolve` (fou.rs:2692–2900) | `src/fuse/geometry.zig` | pure `plan(Facts, cap, …)`; every tie test in §1.7; `cap` from `mem/budget.zig` through `SQUEEZEFS_TRANSPORT_MEM_MAX/PCT` |
| `drain_group_plan` + `drain_group_width` (fou.rs:944–1072) | `src/fuse/drain_groups.zig` | pure over `node_of: fn(cpu) ?usize`; tests §3 |
| `SlotTable` + `RingOp`/`user_data` (fou.rs:353–837) | `src/fuse/slot_table.zig` | plain struct, worker-owned; the 16 tests of §4.6; `gent` encode/decode |
| `lease_core.rs` | `src/fuse/lease_core.zig` (+ `lease_core_model.zig`) | `@fence(.seq_cst)` ×2 named as a pair; `explorer.expectHolds/expectFails` per §6.6 |
| `wake_core.rs` | already `nvme_derive.WakeCoalescer` (nvme_derive.zig:316) | move to `src/sync/wake_coalescer.zig` (shared by nvme lanes, journal lane, transport) + a model |
| `PayloadArena`, `Ent`, header buffers | `src/fuse/arena.zig` | one mmap per member; `extern struct FuseUringReqHeader` (288 B) |
| `build_cmd_entry`, `push_cmd_batched`, `SubmitBatch`, `CommitBatchHistogram` | `src/io/uring128.zig` (ring) + `src/fuse/batch.zig` | SQE128 ring binding (§1.1 blocking fact); `counters.ShardedCounter` + an 11-bucket histogram |
| `queue_worker` (fou.rs:4616–8167) | `src/fuse/worker.zig` | the pass of §8.1; thread `sqz-fuse-ur<qid>`; embeds the ring, members, fused lane |
| `FuseOverUring` (pool) + `GroupHandle` + `QueueHandle` | `src/fuse/transport.zig` | `submitReply*`, `recv`/handoff, `shutdown`, geometry gauges; the sim-inert venue for slot/liveness tests |
| `connection_watch` + `scan_overdue_slots` | the rung-3 watchdog thread | 5 s cadence; `/proc` forensics off the hot path |
| `fused.rs` | `src/fuse/fused_lane.zig` | over the decision-0001 executor's task type; no `catch_unwind` |
| `fast_dispatch.rs` | `src/fuse/read_dispatch.zig` | the law + the `ReadFastDispatch` vtable; counters |
| `spin.rs` | `src/fuse/spin.zig` | A/B lever only; governor shared with the shim's `spin_governor_core` when that lands (rung 7) |
| `uring_conn.rs` (INIT read, sideband, notify) | `src/fuse/sideband.zig` | ONE `ParkedRing` (Q1); `Readv` armed like the wake; notify `Writev` |
| `session.rs` INIT negotiation | `src/fuse/init.zig` | §2.1a; `extern struct fuse_init_in/out`, flags2 at byte 16 |
| `zc.rs`/`kmbuf.rs` bridges, bounce, hold table, retention probe | buffer-mode spec (`fuse_buffer_modes.md`) | this spec fixes only the slot/pend/deadline PROTOCOL (§10) |

Out of scope here (other specs): the kmbuf/zc REGISTER variants and attachment law, the bounce memfd arena,
the hold gate's filesystem probe, retention/ACK-early semantics beyond the slot transitions, NUMA topology
discovery (`numa_core`), the handler lanes' executor (decision 0001), notify frame codecs, the filesystem's
warm-ladder probe implementation (rung 4).

Delivery order inside rung 3 (red-first, each with its tie/contract tests): `uring128` ring binding →
geometry → drain groups → slot table → lease core + coalescer models → arena + REGISTER/COMMIT codec (wire
tests) → worker pass on a stock kernel with UserEnts (the exactly-one-reply storm test as the gate) → INIT
negotiation + sideband → fused lane + READ law → watchdog + rescue ledgers → teardown/residency → then the
buffer-mode spec behind probes.
