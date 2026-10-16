# Porting spec — kmbuf buffer rings, FUSE zero-copy, payload retention, FUSE_TIME_LIMITS, per-queue bg accounting (squeezefs → Zig)

Scope: the sqz-kernel half of the FUSE-over-io_uring transport (charter §3, §4
"Buffer-mode lattice", §6.2 item 1, §6.3 "zc serve"). Every claim cites `file:line` in
`/Users/joberdorf/Source/squeezefs` (daemon) or a patch in
`/Users/joberdorf/Source/squeezefs/docker/kernel-sqz/patches-7.2/` (the 7.2.6-sqz
series the venue container boots — `docker/kernel-sqz/README.md:226-231`; the
6.19/7.1 series live in `patches/` and `patches-7.1/` with the same FUSE values,
`README.md:182-187`). Nothing here is advice; it is the contract. Reference quirks are
flagged `QUIRK n` and collected in §13 with the better design each should take
(decision 0002).

Kernel-side ABI is defined ONLY by the patches; the daemon mirrors it. The venue
kernel `dist/kernel-sqz-container-arm64/` ships no README — the contract documents are
the patches + `docker/kernel-sqz/{README,SERIES}.md` + the two design notes.

---

## 0. The two strata, the one degrade point

- Exactly two strata: **stock-graceful** (no kmbuf, no zc, no retention, no time
  limits — the userspace-ent path byte-identical) and the **full `-sqz` kernel**
  (`README.md:60-66`, "the kmod-sqzfuse stratum is KILLED").
- Every feature is PROBE-detected at runtime; **the probe is the only degrade
  point** — a post-probe refusal fails the mount loud (`kmbuf.rs:1249-1251`,
  `fuse_over_uring.rs:4736-4742`). Never a kernel-version check (`kmbuf.rs:14-16`).
- The 7.2.6 series is 26 patches (`README.md:243-260`, renumbered from the 31-patch
  field series: TIME_LIMITS / retention / nvme / bg-accounting = 0023 / 0024 / 0025 /
  0026 on 7.2). The 7.2 track resolves on the SAME probe rung as 7.1 (opcodes 38/39)
  and carries the same `KmbufTrack::Sqz71` identity (`README.md:302-309`).

---

## 1. uapi constants (all tracks identical for FUSE; io_uring opcode pair per track)

### 1.1 io_uring (`include/uapi/linux/io_uring.h`, patches 0003/0004)

| Constant | Value | Source |
|---|---|---|
| `IORING_REGISTER_KMBUF_RING` | **38** on 7.1/7.2-sqz; 37 on 6.19-sqz | `0003:46-54` (7.1 took 37 for `IORING_REGISTER_BPF_FILTER`); `kmbuf.rs:148-162` |
| `IORING_UNREGISTER_KMBUF_RING` | **39** (7.1/7.2); 38 (6.19) | same |
| `IORING_OFF_KMBUF_RING` | `0x88000000` | `0004:41-42`; `kmbuf.rs:170` |
| `IORING_OFF_KMBUF_SHIFT` | 16 (bgid shifted into the offset) | `0004:42`; `kmbuf.rs:172` |
| `IORING_OFF_MMAP_MASK` | `0xf8000000` (unchanged; KMBUF offset free on 7.2) | `README.md:300-302` |
| `struct io_uring_buf_reg` | 40 B: `union { u64 ring_addr; u32 buf_size; }` ‖ `u32 ring_entries` ‖ `u16 bgid` ‖ `u16 flags` ‖ `u64 resv[3]` | `0003:63-76`; `kmbuf.rs:207-227`, size pinned `kmbuf.rs:1583-1584` |
| `IOBL_KERNEL_MANAGED` | 4 (kernel-internal flag on the buffer list) | `0003:172-174` |
| `IORING_CQE_F_BUFFER` / `IORING_CQE_BUFFER_SHIFT` | 1 / 16 | `kmbuf.rs:203-205`; set by `__io_uring_cmd_done` (`0012:35-40`) |

Register validation (`io_validate_buf_reg(&reg, 0)` + kmbuf checks): `resv` all zero,
`flags` ⊆ permitted (= 0 for kmbuf), `ring_entries` a power of two and `< 65536`
(`0001:48-59`); `buf_size != 0` and `PAGE_ALIGNED` (`0003:142-143`); a live bufring at
the bgid ⇒ `EEXIST` (`0001:75-80`). The kernel allocates `ring_entries × buf_size` of
page memory (`0003:295-334`), accounts it against the user's locked memory when
`ctx->user` (`0003:314-318`) — hence the `RLIMIT_MEMLOCK` finding (§11).

### 1.2 FUSE (`include/uapi/linux/fuse.h`, patches 0014/0019/0023/0024)

| Constant | Value | Source |
|---|---|---|
| `FUSE_IO_URING_CMD_REGISTER` / `COMMIT_AND_FETCH` | 1 / 2 | `fuse_over_uring.rs:114-115` |
| `FUSE_IO_URING_CMD_RELEASE_PAYLOAD` | **3** | `0024:535-537`; `fuse_over_uring.rs:119` |
| `FUSE_URING_BUF_RING` | `1 << 0` (init.flags) | `0014:741-742`; `kmbuf.rs:176` |
| `FUSE_URING_ZERO_COPY` | `1 << 1` | `0019:510`; `kmbuf.rs:179` |
| `FUSE_URING_PAYLOAD_RETENTION` | `1 << 2` | `0024:540-543`; `kmbuf.rs:188` |
| `FUSE_URING_COMMIT_RETAIN` | `1 << 0` (commit.flags) | `0024:545-546`; `kmbuf.rs:194` |
| `FUSE_TIME_LIMITS` | `1ULL << 62` (init flag; `flags2` bit 30) | `0023:79-85`; `session.rs:7072-7074` |
| `FUSE_URING_RINGBUF_GROUP` | 0 — the bgid FUSE hardcodes | `0014:56`; `kmbuf.rs:196` |
| `FUSE_URING_FIXED_HEADERS_OFFSET` | 0 — headers fixed-table index (bufring); `+ zero_copy_depth` under zc | `0014:57`, `0019:376-389`; `kmbuf.rs:200, 247-249` |
| `sizeof(fuse_uring_req_header)` | 288 = `in_out[128]` ‖ `op_in[128]` ‖ `fuse_uring_ent_in_out` (32) | `kmbuf.rs:1196-1198`; `fuse_over_uring.rs:111-112,125-141` |
| `fuse_uring_ent_in_out` | `{ u64 flags; u64 commit_id; u32 payload_sz; u32 padding; u64 reserved }` | `fuse_over_uring.rs:125-133` |

`struct fuse_uring_cmd_req` stays **24 bytes** in the 80 B SQE128 command area
(`BUILD_BUG_ON`, `0024:443-446`): `u64 flags @0` ‖ `u64 commit_id @8` ‖ `u16 qid @16`
‖ union @18 { `init { u16 flags @18; u16 queue_depth @20 }` | `commit { u16 flags
@18 }` } ‖ `u8 padding[2] @22` (`0019:514-524`, `0024:553-560`;
`fuse_over_uring.rs:153-169`). The opcode selects the union member: zeros on
COMMIT_AND_FETCH keep it wire-compatible with pre-series kernels
(`fuse_over_uring.rs:9510-9525`). The SQE's `cmd_op` is the FUSE opcode, the cmd area
begins at SQE offset 48, `buf_index` is the u16 at offset 40
(`fuse_over_uring.rs:8610-8638`).

Upstream minor stays 45 (`README.md:311-313`); negotiation rides the init flags, never a
version bump (`kmbuf.rs:174-176`).

---

## 2. PROBE ladders

### 2.1 kmbuf surface — the opcode LADDER (`kmbuf.rs:554-715`)

Resolved ONCE per process (`OnceLock`, `kmbuf.rs:712-715`). Rungs in order:
`[37/38 (6.19-sqz), 38/39 (7.1-sqz = 7.2-sqz)]` (`kmbuf.rs:167`). First `Confirmed`
rung wins and short-circuits; every other outcome falls through; all rungs exhausted ⇒
`Absent` (`kmbuf.rs:554-578`).

One rung (`probe_rung_live`, `kmbuf.rs:583-677`):

| Step | Syscall / args | Expected | Else |
|---|---|---|---|
| 0 | `io_uring_setup(8, plain params)` — a FRESH scratch ring (side effects die with the fd) | fd | `Refused(errno)` (loud) |
| 1 | `io_uring_register(fd, pair.register, arg, nr_args=1)`; `arg` = `io_uring_buf_reg{buf_size = page, ring_entries = 8, bgid = 7, flags = 0, resv = 0}` embedded in a **zeroed 256 B buffer** (a foreign opcode reading a wider struct — 7.1's `io_uring_bpf` reads 72 B — sees zeros, never stack garbage) | `0` | `Refused(errno)`: `EINVAL` (stock dispatch at `IORING_REGISTER_LAST`, or BPF_FILTER's `cmd_type` import — a page-aligned `buf_size` has low 12 bits zero so its first u16 ≠ 1) and `ENOENT` (kmbuf-UNREGISTER crossed: resv/flags pass, bgid lookup fails) are QUIET; any other errno is loud; all read NOT-kmbuf |
| 2 | identical `io_uring_register` again | `EEXIST` (live bufring at the bgid) | `ForeignSuccess` (loud, never arms, falls through) |
| 3 | `mmap(NULL, 8 × page, PROT_READ\|PROT_WRITE, MAP_SHARED, fd, IORING_OFF_KMBUF_RING \| (7 << 16))` | a mapping → `munmap` | `ForeignSuccess` |
| 4 | drop the ring (fd close releases the registration) | `Confirmed` | — |

Verdict: `Present(pair)` carries the resolved pair + track id; `Absent` ⇒ UserEnts.
False Present is unreachable (the conjunction register==0 ∧ repeat==EEXIST ∧
kmbuf-offset mmap succeeds); false Absent is unreachable on the sqz tracks because the
rung runs the exact registration `KmbufQueue::setup` performs (`kmbuf.rs:680-711`).
Decision table pinned with injected outcomes (`kmbuf.rs:1966-2024`); the BPF
discriminator pinned as arithmetic for pages 4K/8K/16K/64K (`kmbuf.rs:2034-2046`).
Log line at arm: `kmbuf_ops=38/39 (7.1-sqz)` / `absent` (`kmbuf.rs:716-724`).

On the venue (7.2.6) the ladder resolves on rung 2: rung 1's occupant is
`IORING_REGISTER_BPF_FILTER = 37` (import-EINVAL), `IORING_REGISTER_LAST = 38`
upstream so 38/39 are free (`README.md:294-302`).

### 2.2 zc capability — CAP_SYS_ADMIN, probed at resolution

The kernel gates zc at queue creation: `if (!capable(CAP_SYS_ADMIN)) goto error`
(EINVAL) and `zero_copy_depth == 0 ⇒ EINVAL` (`0019:120-134`). The daemon pre-probes
the capability as `geteuid() == 0` inside `resolve_buffer_mode` so the refusal is loud
AT RESOLUTION instead of a per-queue REGISTER EINVAL (`kmbuf.rs:776-808`). Decision:
kmbuf `Present` ∧ `SQUEEZEFS_FUSE_KMBUF` on ∧ `SQUEEZEFS_FUSE_ZC` on ∧ euid 0 ⇒
`ZeroCopy`; euid ≠ 0 ⇒ warn + `BufRing`; kmbuf lever off ⇒ `UserEnts` (warn, and a
second warn if zc was wanted); `Absent` ⇒ `UserEnts` (`kmbuf.rs:793-830`).

**QUIRK 1** — `euid == 0` is a proxy for `capable(CAP_SYS_ADMIN)`; the real check is
`CapEff` bit 21 in `/proc/self/status` (the module already parses `CapEff` for
`CAP_IPC_LOCK` bit 14, `kmbuf.rs:464-471`). A root daemon with a dropped bounding set
reads `ZeroCopy` and then dies at REGISTER. Better: read `CapEff` once, test bit 21.

**QUIRK 2** — `kmbuf.rs:86-89` and `:780-782` claim "a kernel that refuses the zc
REGISTER degrades to BufRing loudly"; the worker actually treats a REGISTER CQE
`EINVAL/ENOSYS/ENOTSUP` as protocol-fatal (`fuse_over_uring.rs:6781-6785`) — which is
the charter's law (post-probe refusals fail loud). The doc comment is stale; the Zig
port keeps the fail-loud behaviour and does not write the downgrade.

### 2.3 Retention surface — PROBE by OPCODE (`fuse_over_uring.rs:8534-8579`, `kmbuf.rs:280-318`)

`fuse_uring_register()` ignores unknown init bits, so bit 2 cannot self-negotiate (an
old kernel would accept and silently ignore it — the worst class;
`kmbuf.rs:283-286`, `design-zc-write-kernel-v2.md:360-363`). Probe:

| Step | Detail |
|---|---|
| when | AFTER the INIT reply is written (the kernel's `fuse_uring_cmd` requires `fch->initialized`, else `-EAGAIN` — `session.rs:1958-1968`; 7.2.6 refuses `-EOPNOTSUPP` on a connection whose INIT reply lacked `FUSE_OVER_IO_URING`, `README.md:378-386`) and BEFORE the REGISTER pass (so the verdict composes the REGISTER flags) — `fuse_over_uring.rs:3007-3013` |
| only if | `buffer_mode == ZeroCopy` (`fuse_over_uring.rs:3012`) |
| ring | a scratch `IoUring<Entry128>` of 8 entries, SQE128 |
| SQE | `IORING_OP_URING_CMD` on `Fd(fuse_fd)` (plain fd, not fixed), `cmd_op = 3`, cmd = `fuse_uring_cmd_req{flags 0, commit_id = u64::MAX, qid 0, union 0}` zero-padded to 80 B, `user_data = 1` |
| wait | `submit_with_args(1, timeout 1 s)`; `ETIME` or any submit error ⇒ `Err(())` |
| verdict | `from_probe_errno`: `-ENOTCONN` (opcode present, queue 0 not yet created — the pre-REGISTER shape; on 7.2.4+ the ring exists at INIT reply but `ring->queues[0]` is NULL → `-ENOTCONN`, `0024:363-368`) or `-ENOENT` (armed, id unknown) ⇒ `Present`; `-EINVAL` / `-EOPNOTSUPP` (pre-0029 opcode switch default / disabled) ⇒ `Absent`; 0, positive, any foreign errno (`EBUSY`, `EAGAIN`), `Err` ⇒ `Absent` fail-safe (`kmbuf.rs:300-318`, table test `kmbuf.rs:1829-1860`) |

Measured live ×5 on both sqz tracks: pre-arm `-ENOTCONN`, armed `-ENOENT`
(`.benchmarks/2026-08-09-kernel-zc-write-v2.md:253-261`).

Posture (`resolve_retention`, `kmbuf.rs:324-349`): zc mode ∧ `Present` ∧
`SQUEEZEFS_FUSE_ZC_RETENTION` (default on) ⇒ arm bit 2 on every REGISTER; `Present` ∧
lever off ⇒ warn; `Absent` ⇒ info ("ACK-after-CQE posture, byte-identical"). Arming
alone is bit-identical to a zc queue until a commit carries RETAIN (`0024:67-68`).

### 2.4 FUSE_TIME_LIMITS — the init-flag bit IS the probe

The kernel advertises bit 62 in `fuse_send_init` (`0023:52-58`). The daemon echoes it
iff offered and fills `time_min/time_max`; absent ⇒ fields zero, flag unechoed,
bit-identical INIT reply (`session.rs:6376-6386, 1802-1818`). No syscall, no scratch
ring. Details §8.1.

---

## 3. The kmbuf buffer ring (BufRing mode)

### 3.1 What the kernel expects (patch 0014)

Per QUEUE: (a) a **fixed buffer at index 0** holding every ent header back to back;
(b) a **kernel-managed buffer ring** at bgid 0 (`0014:17-22`, `0021:36-37`). At the
first REGISTER of a queue with `init.flags & FUSE_URING_BUF_RING`, `fuse_uring_create_queue`
pins the bufring (`io_uring_buf_ring_pin(cmd, 0, …)`) and refuses `EINVAL` unless
`io_uring_is_kmbuf_ring` (`0014:66-87`); a later REGISTER with a different mode on the
same queue is refused `EINVAL` (`0014:566-579`). The pin lasts the connection's life
(`0014:21-22`). In bufring mode a REGISTER carries **no iovecs**; the kernel reads
`ent->fixed_buf_id = sqe->buf_index` (`0014:506-510`).

Headers: at send/commit the kernel maps `offset = fixed_buf_id × 288` into fixed
buffer index 0 (`0014:435-457`), copies `fuse_in_header`/`op_in`/`ent_in_out` through
`copy_to_iter`/`copy_from_iter` on a kernel iov (`0014:152-243`). Payload: the kernel
selects a buffer from the ring when the request has copyable payload
(`in_numargs > 1 || out_numargs`, `0014:277-282`), stores `payload_kvec = {kaddr,
len}` + `ringbuf_buf_id` (`0014:284-306`), and `cs->is_kaddr = true` short-circuits
`fuse_copy_fill`'s per-page lock→GUP→unlock (`0014:266-270`; the 12.8 % + 2.6 % term,
§11).

### 3.2 Daemon registration (`KmbufQueue::setup`, `kmbuf.rs:1252-1424`)

Preconditions: `payload_sz % page == 0` (refuse before any syscall — a planner bug,
`kmbuf.rs:1268-1273`); `kmbuf_surface()` must be `Present` (else refuse,
`kmbuf.rs:1276-1285`). Then, on the QUEUE's ring:

1. `headers_span = round_up(depth × 288, page)`; `mmap(NULL, headers_span, RW,
   MAP_PRIVATE|MAP_ANONYMOUS)` (`kmbuf.rs:1286-1304`).
2. `IORING_REGISTER_BUFFERS` with the table: bufring = `[headers_iov]` (index 0); zc =
   `depth` zeroed iovecs (sparse) + `headers_iov` at index `depth`
   (`kmbuf.rs:1309-1337`). Sparse zeroed entries are legal since 5.19
   (`kmbuf.rs:1310-1311`).
3. `ring_entries = next_pow2(depth).max(1)`; `io_uring_register(fd, ops.register,
   &io_uring_buf_reg{buf_size = payload_sz, ring_entries, bgid 0, flags 0}, 1)` through
   `register_with_retry` (ENOMEM-only, §11) (`kmbuf.rs:1339-1387`). Refusal message
   names opcode, bgid, buf_size, entries, the track, and the memlock note on ENOMEM.
4. `region_span = ring_entries × payload_sz`; `mmap(NULL, region_span, RW,
   MAP_SHARED|MAP_POPULATE, fd, IORING_OFF_KMBUF_RING | (0 << 16))`
   (`kmbuf.rs:1389-1411`).
5. `KMBUF_PINNED_BYTES += ring_entries × payload_sz` (process-wide, `kmbuf.rs:1413`).

Layout law: header of ent `i` at `headers_base + i × 288` (`kmbuf.rs:1511-1514`);
buffer `bid` at `region_base + bid × buf_size`, `bid < ring_entries` else `None`
(`kmbuf.rs:1517-1520`). The kernel's own `io_uring_buf_ring` (head/tail/`bufs[]`) is
kzalloc'd kernel memory and is NEVER mmapped by the daemon — only the buffer REGION is
(`0003:88-125`); the daemon never touches a ring tail. Teardown: `munmap` both regions;
kernel pins release at ring-fd close (`kmbuf.rs:1562-1573`). The daemon never issues
`IORING_UNREGISTER_KMBUF_RING` (`kmbuf.rs:138-141`).

### 3.3 The REGISTER SQE in kmbuf/zc modes (`fuse_over_uring.rs:5348-5410`)

`UringCmd80(Fixed(0) /* fuse fd */, cmd_op = REGISTER)`, `init.flags` = `init_flags(true,
false)` (BufRing) or `init_flags_with_retention(true, true, retention)` (ZeroCopy),
`init.queue_depth = depth` in zc only (kernel refuses zc with zero depth), no iovecs,
`buf_index = ent_idx` (`fuse_over_uring.rs:5352-5378`). Wire pinned byte-exact at
`fuse_over_uring.rs:9467-9508`. The SAME flags serve every re-REGISTER (the kernel's
consistency check includes the retention bit, `0024:433-437`).

### 3.4 How a delivered payload's kernel address reaches the daemon — the attachment law

The delivery CQE (the REGISTER's or COMMIT_AND_FETCH's completion) carries
`IORING_CQE_F_BUFFER | (bid << 16)` **only on fresh selection** (`0012:35-40`); the
kernel REUSES the attached buffer across consecutive payload-carrying requests and
recycles it (`io_uring_kmbuf_recycle`) when a payload-less request is fetched
(`0014:306-348`). Daemon law (`KmbufQueue::note_delivery`, `kmbuf.rs:1540-1549`): a
flagged CQE re-points `attached[ent] = bid` (Release); an unflagged one KEEPS the
current attachment; `bid ≥ ring_entries` ⇒ `None` ⇒ protocol breach → loud shutdown
(`fuse_over_uring.rs:7099-7108`). A briefly-stale attachment can only precede
payload-LESS deliveries (header-only replies never write the payload region), so no
body write can target a recycled buffer (`kmbuf.rs:63-68`). A payload announced with
no attachment (non-zc) ⇒ shutdown (`fuse_over_uring.rs:7183-7196`).

The ent's `payload_ptr/len` are re-pointed per attachment and NUMA node re-derived
(`fuse_over_uring.rs:7090-7098`). The §5.4 FUSE_WRITE lease rides the attached kmbuf
buffer unchanged: the kernel recycles only at fetch, fetch is triggered only by our
COMMIT_AND_FETCH, and the commit gate defers that until the last lease drops —
**deferred re-arm ≡ deferred recycle** (`kmbuf.rs:69-77`).

### 3.5 Width-1 drain groups

`drain_group_plan` forces width 1 for `BufRing | ZeroCopy` (an explicit
`SQUEEZEFS_FUSE_DRAIN_GROUP` never overrides it): the fixed headers buffer, the bgid-0
bufring and the sparse table are per-RING resources and FUSE hardcodes bgid 0 / index
0, so one ring serves exactly one queue (`fuse_over_uring.rs:1006-1026`; debug-asserted
`g == 1` at setup, `fuse_over_uring.rs:4757-4760`). Consequence: one worker thread per
possible CPU on kmbuf/zc sessions (charter §4 "kmbuf/zc sessions keep width 1").

---

## 4. Zero-copy (ZeroCopy mode)

### 4.1 Kernel contract (patches 0019, 0020)

Table shape the kernel requires: a sparse buffer per queue entry (`0..depth`), headers
at index `depth`, plus the kmbuf ring for non-zc'd args (`0019:13-21`, `0021:66-75`).
At delivery `fuse_uring_set_up_zero_copy` builds a bvec per request folio and
`io_buffer_register_bvec(cmd, bvs, nr, total, ddir, index = ent->fixed_buf_id, …)`;
`ddir = ITER_DEST` for reads (`out_pages`), `ITER_SOURCE` for writes (`in_pages`)
(`0019:197-237`). `can_zero_copy_req = queue->use_zero_copy && (in_pages ||
out_pages)` — **no per-request daemon opt-out** (`0019:76-81`). The folio copy is
skipped at COMMIT in both directions (`cs->skip_folio_copy`, `0019:56-66,190`). For an
in-paged request the announced `payload_sz` still INCLUDES the zero-copied bytes
(`ent_in_out.payload_sz += in_args[last].size`, `0019:263-267`) even though they are
not in the kmbuf buffer. Non-paged args still select a kmbuf buffer
(`fuse_uring_req_has_copyable_payload`, `0019:293-309`). The slot is unregistered at
`fuse_uring_req_end` (COMMIT) unless retained (`0019:93-97`, `0024:143-148`); the
unregister is guarded on `ent->cmd` (7.2's `set_commit` WARN arm reaches `req_end`
with no cmd — the slot is then released at ring-fd teardown, `README.md:270-275`).

Slot addressing from the daemon: bvec-registered buffers carry `imu->ubuf = 0`, so
`sqe.addr` is the BYTE OFFSET into the slot (the ublk convention) and `sqe.buf_index`
= the ent index on that queue's ring (`zc.rs:50-53`). A registered slot has a
direction: `READ_FIXED` into an `ITER_SOURCE` (write) slot is refused `-EFAULT`
(`kmbuf_smoke.c:47-50`).

**Abort-race folio references (patch 0020)** — what the daemon relies on: page
lifetime follows the io_uring registration, request lifetime follows FUSE, neither
trusts the other's ordering (`0020:55-59`). `fuse_uring_set_up_zero_copy` `folio_get`s
every folio into a `GFP_KERNEL_ACCOUNT` carrier; the imu release callback
(`fuse_zc_pages_release`) puts them when the table slot is gone AND every in-flight
fixed op has put its rsrc node (`0020:83-104,131-147`). Closes three windows: abort
without unregister (now a bounded page-pin leak released by ring death), in-flight
FIXED I/O across COMMIT, and the retention steady state (`0020:20-44`). Teardown
deliberately gains NO unregister (no uring-cmd issue context, `0020:57-59`). So the
daemon may: issue slot bridges that outlive the request (retention), and never fears a
late DMA into freed pages — but a late DMA into a slot that was RE-REGISTERED for a
new request is the aliasing the bridge-deadline ladder exists to prevent (§7.4).

### 4.2 The opcode mirror (track-keyed) — `zc.rs:81-140`

The daemon must mirror the kernel's per-opcode `in_pages`/`out_pages` choice, which is
a property of the kernel's FUSE tree, not the series:

| Direction | 6.19-sqz | 7.1/7.2-sqz |
|---|---|---|
| out-paged (reply body rides the slot) | {READ, READLINK} | {READ, READLINK, READDIR, READDIRPLUS} (`fuse_readdir_alloc_buf` sets `out_pages`) |
| in-paged (payload lives in the slot) | {WRITE} | {WRITE} |

(`zc.rs:109-120, 134-140`; pinned `zc.rs:652-683`). Misses are loud, never
corrupting: an op bridged but served copyable fails its slot fetch and falls back to
the kmbuf attachment (`fuse3_zc_fallbacks`, `fuse_over_uring.rs:5221-5251`); an op
copied but zc'd by the kernel hits the no-attachment EIO guard naming op + track
(`fuse_over_uring.rs:8365-8397`); an in-direction announced payload with neither
attachment nor extraction arm (data IOCTL class) delivers EMPTY, counted
(`fuse3_zc_slot_payload_skips`, `fuse_over_uring.rs:7163-7182`).

**QUIRK 3** — a hand-maintained opcode table keyed to the probe rung is an ABI
guess the kernel could state. Better (sqz patch, charter §1.4: the kernel is a
product surface): set a bit in `fuse_uring_ent_in_out.flags` (the u64 is unused —
`fuse_over_uring.rs:128`) at `fuse_uring_args_to_ring` when `skip_folio_copy` is set,
e.g. `FUSE_RING_ENT_F_ZC_PAYLOAD = 1`, and have the daemon read the bit per delivery.
The mirror table, `KmbufTrack`, `fuse3_zc_fallbacks` and `fuse3_zc_slot_payload_skips`
become dead; the Zig port keeps the table only until that patch lands, and writes the
bit-read as the primary arm with the table as the fallback.

### 4.3 Paged replies — the direct device leg vs the memfd bounce leg

Routing point `commit_ready_reply` (`fuse_over_uring.rs:4967-5137`), shared by the
fresh drain and the unpark scan:

- **Prefilled (direct device leg)**: `CommitMsg.prefilled = Some(n)` ⇒ `apply_reply_zc`
  writes header + `payload_sz = n` only — the bytes already sit in the request's pages;
  `fuse3_zc_replies += 1`; plain COMMIT (`fuse_over_uring.rs:4998-5003, 8451-8462`).
  The handler got there by `ZcFetchMsg` → worker pushes `READ_FIXED(Fd(dev_fd), addr =
  NULL, len, buf_index = ent_idx).offset(dev_off)` tagged `RingOp::Fetch`, parks
  `ZcPend::HandlerFetch{done, trace_id}`, stamps the deadline; the CQE's raw `res` is
  forwarded verbatim on the oneshot — the handler validates and only then commits
  (`fuse_over_uring.rs:5760-5839, 5166-5173`). Eligibility lives in routing
  (`routing.rs:20065-20071`): zc session ∧ not `direct_device_true` ∧ `slice_len > 0` ∧
  `slice_start % 4096 == 0` ∧ `slice_len % 4096 == 0` ∧ within one block ∧ passthrough
  crypto; and `routing.rs:20471-20480`: COLD (no in-flight single-flight fill for the
  key) ∧ device resolvable (`zc_device_resolve`: incarnation ok, backend healthy, the
  device's `zc_read_fd`, `routing.rs:4107-4125`). Fill discipline mirrors the raw dest
  leg: incarnation snapshot before, still-check + binding re-check after; any movement
  or short/failed fetch falls through to the ordinary ladder whose bounce serve
  OVERWRITES whatever landed (`routing.rs:20453-20470, 20495-20530`). Full-length ⇒
  `read_zc_serve_bytes += n`, reply `Bytes::new()` + `zc_prefilled(n)`.
- **Bounce leg** (every other out-paged body): stage the body into the ent's
  `ZcBounce` slot (a `memcpy` elided when the body already IS the bounce slot —
  dest-armed serves wrote there because `get_payload_buffer` hands out the bounce in zc
  mode, `fuse_over_uring.rs:5047-5076`), then `READ_FIXED(Fd(bounce_fd), NULL,
  body_len, ent_idx).offset(ent_idx × stride)` (`fuse_over_uring.rs:5078-5088`); the
  commit PARKS (`on_commit_parked`) as `ZcPend::BounceFetch{header, body, commit_id,
  len, fd_source}` until the CQE; `res == len` ⇒ `apply_reply_zc` + COMMIT
  (`fuse_over_uring.rs:5189-5195`).
- **fd-source leg** (`SQUEEZEFS_READ_ZC_SERVE`): `CommitMsg.body_fd = Some((fd, off))`
  ⇒ the bridge SOURCE is the filesystem's memfd fill pool itself
  (`READ_FIXED(fd @ off → slot)`), the staging copy skipped, `reply_body` kept as the
  keepalive; `fuse3_zc_fd_body_replies += 1`; a failed fd-source bridge RE-STAGES the
  same body through the bounce (`PendDone::Restage`, `fuse3_zc_fd_body_fallbacks`)
  (`fuse_over_uring.rs:5056-5060, 5196-5219`). The pool: ONE memfd slab
  (`memfd_create("sqz-read-fill-pool")`, `ftruncate(capacity × 4 MiB)`, `mmap MAP_SHARED`,
  no `MAP_POPULATE`) so every slot has an `(fd, offset)` (`cache/pool.rs:521-560`).
- Refusals, all header-only EIO + plain commit, never a truncated body: out-paged reply
  with header-extra bytes; no bounce slot; `body_len > stride` (`transport_replies_oversize`);
  bridge push failure (`fuse3_zc_fallbacks`) (`fuse_over_uring.rs:5010-5046,
  5104-5115`). Zero-length paged reply (EOF, empty dir page) is header-only — **never a
  zero-length READ_FIXED** (`fuse_over_uring.rs:5117-5119`; a zero-length `READ_FIXED` on
  an O_DIRECT file NULL-derefs in `iov_iter_alignment_bvec` on btrfs/f2fs,
  `probes/zero-len-readfixed-report.md:1-80`).

### 4.4 Paged WRITE payloads — HELD in the slot, W1 gate, extraction

Delivery of an in-paged WRITE (`zc_slot_payload = in_paged(opcode) ∧ payload_sz > 0`,
`fuse_over_uring.rs:7159-7164`) delivers a `capped_sz = 0` placeholder payload
(`fuse_over_uring.rs:7201-7205`) and takes one of two arms
(`fuse_over_uring.rs:7510-7691`):

1. `payload_sz > bounce.stride()` ⇒ `fail_ent(EIO)` (`:7522-7542`).
2. Parse `fuse_write_in` from `op_in`: `fh @0`, `offset @8`, `size @16`,
   `write_flags @20`, open `flags @32`; `odirect = (open_flags & O_DIRECT) ∧
   !(write_flags & FUSE_WRITE_CACHE)`; `nodeid` from `in_out[16..24]`
   (`:7543-7573`).
3. **HOLD** iff `zc::hold_candidate(offset, size, payload_sz_cfg)` ∧ the registered
   hold gate says yes (`g(nodeid, offset, size, odirect)`; no gate ⇒ never hold)
   (`:7574-7579`). Then `zc_write_held.set(qid, ent, payload_sz)` and dispatch
   immediately (fused onto the worker's lane when `fuse_candidate(payload_sz,
   fusion_max, fusion_on)` and capacity allows, else the classic lane) (`:7580-7614`).
4. **Else extract AT DELIVERY** (the streaming arm): clear the held cell;
   `WRITE_FIXED(Fd(bounce_fd), NULL, payload_sz, ent_idx).offset(ent_idx × stride)`,
   pend `ZcPend::WriteExtract{header_and_op, unique, commit_id, len}`; on `res == len`
   the inbound push happens with a §5.4 lease over the BOUNCE slot
   (`fuse3_zc_write_extractions/_bytes`); failure ⇒ synthesized EIO
   (`:7615-7684, 5283-5311`). A pending bridge on the ent ⇒ `fail_ent(EIO)`.
5. A non-held zc delivery clears the ent's held cell (belt against stale reads,
   `:7685-7691`).

`hold_candidate` (`zc.rs:355-370`): `size > 0 ∧ size < bound ∧ offset % 4096 == 0 ∧
size % 4096 == 0`, `bound = payload_sz / 2` (STRICT — half-payload is streaming class;
kernel-split streaming writes arrive payload-sized and the W1 cap `block/8 ≤
payload/2` on every shipped geometry) or `payload_sz + 1` when streaming holds are
armed (`SQUEEZEFS_DEVICE_OVERLAY` on: eligible streaming segments consume the slot by
direct DMA, `zc.rs:356-368, 374-383`). Pinned `zc.rs:690-709`.

The **W1 eligibility gate** (`Filesystem::zc_write_hold_eligible`,
`fuse_client.rs:26231-26290`, wired through `session.rs:1032` and
`set_zc_write_hold_gate`, `fuse_over_uring.rs:3710`): the overlay arm holds eligible
fresh/hole aligned single-block segments ONLY for page-cache deliveries (`return
!odirect`; O_DIRECT/GUP holds forced a handler-side late materialize — the zcws-8 tax
that pinned the 1 MiB A-leg at 23 GiB/s); else the `try_patch` shape half as LOCK-FREE
SYNC PROBES (this runs on the queue worker): `len > 0`, not a virtual ino, `len ≤
patch_max_bytes()` (`block/8`), 4 KiB-aligned offset and len, no write verification,
passthrough crypto, within one block, striped ∧ non-extending (`end ≤ meta.size`) ∧
undecorated whole-block mapping from the lock-free metadata cache (a cache MISS reads
ineligible — the authoritative fetch is an await the probe must not pay). Skipped on
purpose: `refcount == 1` and byte-range custody (handler-time discovery = the bounded
`fuse3_zc_write_lazy_extractions` path). Measured WHY: holding W1-INELIGIBLE shapes
paid hold + fused poll + LATE extraction serialized at fabric RTT — **0.45×** armed
rand-4k (386k unarmed / 300k fusion-off / 175k fused at ~300 µs RTT), signature
`fusions ≈ ops ∧ extractions ≈ ops` (`fuse_zc_write_fusion_tests.rs:5-16`,
`.benchmarks/2026-08-08-fused-lane-predicate.md:5-6`); all-lazy extraction collapsed
durable streaming to **0.61×** (armed legs 38–42 % busy vs 74–75 % control) while
at-delivery batching priced 0.998×/0.909× (`zc.rs:348-354`).

Held-payload table `ZcHeldTable` (`zc.rs:560-620`): `nqueues × depth` `AtomicU32`
cells, 0 = nothing held; SET at an armed WRITE's delivery, READ by size validation and
the handler's slot-source mint, CLEARED by a completed extraction, OVERWRITTEN by the
ent's next delivery; queries only ever race their own request's window (handler ≺
reply ≺ COMMIT ≺ next delivery). Out-of-range coordinates: `get` = `None`, `set` =
loud no-op (`zc.rs:596-603`).

The handler consumes a held slot through `ZcWriteSlot` (`routing.rs:6952-6992`): the
**direct leg** `store` ⇒ `ZcStoreMsg{qid, ent, fd, dev_off}` ⇒ worker pushes
`WRITE_FIXED(Fd(dev_fd), NULL, held_len, ent_idx).offset(dev_off)` on the FUSE queue
ring that registered the slot, pend `HandlerStore{done}`, `note_zc_store_qid(qid)`;
CQE forwarded verbatim; full-length success is counted AT THE CONSUMING SITE
(`fuse3_zc_write_directs/_bytes`) so a caller-side validation failure never leaves a
phantom count (`fuse_over_uring.rs:5840-5906, 5174-5180`, `kmbuf.rs:1079-1091`).
Refusals on the oneshot: bad slot `EINVAL`, no held payload `ENOENT`, pending bridge
`EBUSY`, push failure `EIO` — the handler falls back to the extraction vehicle. The
**lazy extraction** `materialize` ⇒ `ZcExtractMsg` ⇒ `WRITE_FIXED(Fd(bounce_fd), NULL,
held_len, ent_idx).offset(ent × stride)`, pend `LazyExtract{done, len}`; success mints
the §5.4 lease over the bounce bytes and counts `fuse3_zc_write_extractions` +
`fuse3_zc_write_lazy_extractions` (the staleness gauge, ≈ 0) (`fuse_over_uring.rs:5907-5975,
5253-5281`). Memoized: the fencing-retry loop never pays a second extraction
(`routing.rs:6950-6957`).

Slot lifetime law: pages unregister at COMMIT, and every slot-sourcing DMA is
handler-awaited strictly before the reply exists, so ordering holds by construction
(`zc.rs:43-46`) — except under retention (§5).

### 4.5 The bounce arena (`ZcBounce`, `zc.rs:142-261`)

Per queue: `memfd_create("sqz-fuse-zc-bounce", MFD_CLOEXEC)`, `ftruncate(depth.max(1)
× stride)`, `stride = round_up(payload_sz, page)`, `mmap(MAP_SHARED|MAP_POPULATE)`
(`zc.rs:174-224`). Two faces of one memory: VA for CPU serves/leases, FD for the ring
ops (`zc.rs:146-154`); `buf_ptr(idx)`, `offset_of(idx) = idx × stride`, both `None`
for `idx ≥ depth` (`zc.rs:232-241`). Dual-face pinned `zc.rs:864-906`. Any refusal
fails the worker → the mount (`fuse_over_uring.rs:4766-4774`).
The port (`worker.Slab`): one VA span for every queue at `(qid × depth + ent) × stride`, and each
queue's memfd mapped in place (its fd face at `ent × stride`), lazily populated (0002 row 467). It
stays per queue because an extraction is a shmem write under the file's `i_rwsem`, and one file
shared by all rings made their io-wq spin on it (`docs/evidence/rung-10-slabcopy.md`).

---

## 5. Payload retention — the ACK-early accelerator (patch 0024)

### 5.1 Kernel semantics

- Arming: `FUSE_URING_PAYLOAD_RETENTION` accepted ONLY with `FUSE_URING_ZERO_COPY`
  (`retention && !zero_copy ⇒ EINVAL`, `0024:163-165`); `queue->use_retention`
  (`0024:173`); re-REGISTER consistency includes the bit (`0024:433-437`).
- COMMIT with `commit.flags & FUSE_URING_COMMIT_RETAIN`: valid only when
  `queue->use_retention ∧ ent->zero_copied ∧ ent->zc_dir_source` (write); otherwise
  the commit fails `-EINVAL` **without consuming the request** (`0024:291-297`).
  Read-direction retention refused by design (`0024:92-94`). On a valid RETAIN the
  request ends normally (write(2) returns), `io_buffer_unregister` is skipped, the ent
  goes `FRRS_RETAINED` with `retained_commit_id = commit_id`, and the COMMIT_AND_FETCH
  cmd stays PENDING — no CQE, no fetch (`0024:316-339`). A copy/header error clears
  retain and falls through to a plain fetch (`0024:270-272, 335`).
- `RELEASE_PAYLOAD` (opcode 3) looks up by `commit_id` on `qid`:
  `!ring ⇒ -ENOTCONN`; `qid ≥ nr_queues ⇒ -EINVAL`; no queue / not connected / stopped
  ⇒ `-ENOTCONN`; a LIVE request with that id (`fuse_request_find`) ⇒ `-EBUSY`; no
  retained ent with that id ⇒ `-ENOENT`; found ⇒ `io_buffer_unregister(parked cmd,
  fixed_buf_id)`, re-enter the fetch path (`fuse_uring_get_next_fuse_req` +
  `fuse_uring_send` on the PARKED cmd — i.e. the next delivery arrives as the parked
  COMMIT's CQE), RELEASE's own cmd completes `0` (`0024:347-411`).
- Pin accounting is structural: one retained slot = one ent out of the fetch pool,
  bounded by `zero_copy_depth`; no kernel timer; a leaked retention is reclaimed at
  teardown with a ratelimited `pr_warn` (`0024:38-56, 212-217`). Teardown drains
  `FRRS_RETAINED` like userspace-held ents whose request already ended; the parked cmd
  completes through the cancel-safe path (`fuse_uring_cancel` treats `FRRS_RETAINED`
  like `FRRS_AVAILABLE`, `0024:236-240`); pages survive until the ring releases them
  (0020's law).

### 5.2 The stability law (kernel documents; daemon enforces)

After COMMIT-with-RETAIN the application legally owns its buffer again; bytes are
sampled at DMA time — unstable-write semantics (NFS UNSTABLE/COMMIT class)
(`0024:57-64, 109-115`). **Page-cache folios are sound** by composition (a post-ACK
modification is a redirty = a new WRITE; overlay newest-wins). **O_DIRECT/GUP pages are
NOT** — snapshot before ACK-early. A read intersecting an ACKed-but-incomplete store
must wait for it or serve from the retained source, never treat the range as an
uncovered old-map gap (`0024:116-119`). How the daemon tells them apart: the class is
stamped at mint from the write's flags — `odirect = (open_flags & O_DIRECT) ∧
!(write_flags & FUSE_WRITE_CACHE)` (`fuse_over_uring.rs:7563-7573`), carried as
`ZcWriteSlot.ack_early_sound = !odirect` (`routing.rs:6959-6965`). Decision
(`fuse_client.rs:19270-19330`): `ack_early_sound ∧ commit_retain()` ⇒ RETAIN commit,
reply now, a detached `finish_ack_early_store` continuation owns the DMA + RELEASE,
`overlay_ack_early_stores/_bytes`; `!ack_early_sound ∧ SQUEEZEFS_ZC_ACK_EARLY_ODIRECT`
⇒ `materialize()` (extract off the GUP slot — bytes sampled at ACK), then ACK on the
owned bytes; else ACK-after-CQE. Production O_DIRECT overlay does not HOLD at all (the
hold gate returns `!odirect`, `fuse_client.rs:26241-26243`), so it extracts at
delivery and rides the Bytes ACK-early arm without the knob
(`env_knobs.rs:255`).

### 5.3 Daemon slot machine (`fuse_over_uring.rs:374-397, 589-620`)

`Delivered/Parked → Retained{commit_id}` on `submit_commit_retain(retain = true)`
(`on_commit_submitted_retained`); `Retained → Replied` on `push_release`
(`on_release_submitted`); `Retained → Delivered{unique 0}` on a kernel `-EINVAL`
refusal (`on_retain_refused`) followed by a plain re-commit (`fuse3_zc_retain_refused`,
must stay 0 — the submit site mirrors the kernel checks) (`fuse_over_uring.rs:6755-6779`);
a transient `EAGAIN/EINTR` on a RETAIN commit re-commits WITH retain after
`retract_zc_retain_commit` (the kernel never parked it) (`:6798-6831`). A retained slot
owes NO reply: `owing()`/`any_owing()` exclude it, `fail_ent` is a no-op on it, teardown
synthesizes nothing (`:736-759, 8841-8853`) — but it DOES owe a RELEASE, which the
reference leaves to the kernel's reclaim. NOT carried (decision 0002 row 263): the Zig
drain pushes `RELEASE_PAYLOAD` for every `retained` slot after every commit of the drain
(a RETAIN an unparked cell pushed precedes its RELEASE in SQ order), reaps the verdicts — the
kernel answers inline in the submitting enter — and only then publishes `drained` for the
session's `umount2`. The kernel's `pr_warn` ("teardown with retained zc payloads") is the
line an orderly teardown must never add: on 7.2.6-sqz the reclaimed ent's fixed buffer
pinned the exiting process in D state. Counted `transport_retained_released_at_teardown`;
a drain that finds the connection already gone (a `-ENOTCONN` verdict, or `disconnected`
before the pass) counts `transport_retained_kernel_drained` — never the release tripwire.

Arming a RETAIN: the handler calls `zc_commit_retain(slot)` strictly before replying —
`retention ∧ zc_armed()` required, sets `retain_next[qid × depth + ent]`
(`:3944-3958`); the CommitMsg mint consumes it (`take_retain_next`, `:3516, 3961-3968`);
delivery belt-clears it (`:7244`). The RETAIN COMMIT_AND_FETCH SQE is the ordinary
commit with `commit.flags = FUSE_URING_COMMIT_RETAIN` at cmd offset 18 (`:8224-8242`,
wire pinned `:8919-8945`); `fuse3_zc_retain_commits += 1`, `fuse3_zc_retained_outstanding += 1`.

RELEASE SQE (`push_release`, `:8263-8287`): `UringCmd80(Fixed(0), cmd_op = 3)`, cmd =
`{flags 0, commit_id = the retained commit id, qid, union 0}`, `user_data =
(UD_TAG_RELEASE=5 << 32) | gent`; counted at SUBMISSION (`fuse3_zc_releases`); its CQE is
accounting-only — `0` expected, nonzero ⇒ `fuse3_zc_release_failures` (must stay 0:
`ENOENT` = double release, `EBUSY` = raced a live commit) (`:6743-6753`).

**Ordering protocol** (release can never precede its commit, so the kernel never sees
`-EBUSY`): the store continuation sends `WorkerMsg::ZcRelease{qid, ent}`
(`zc_release_payload`, `:3975-4004`, a no-op once `active` is false — teardown owns
retained ents). The worker: `Retained` ⇒ push RELEASE now; `Delivered | Parked` (the
store CQE beat the reply through the channels) ⇒ `set_release_pending(ent)`, and
`submit_commit_retain` fires the pending release in the SAME batch right after the
RETAIN commit SQE so the kernel processes them in order (`:5976-6010, 8243-8248`);
any other state ⇒ stale, inert. Closure law: `retain_commits − releases ≡ outstanding`,
0 at quiesce (`kmbuf.rs:917-924`).

What a RETAIN means for the slot's pages: the request ended, but the slot stays
registered ⇒ the `WRITE_FIXED(device ← slot)` bridge may run AFTER write(2) returned;
only 0020's folio refs make that sound (`0024:15-18`).

---

## 6. The buffer-mode lattice — decision table

Resolution (`resolve_buffer_mode`, `kmbuf.rs:783-831`; retention `kmbuf.rs:324-349`):

| kmbuf probe | `SQUEEZEFS_FUSE_KMBUF` | `SQUEEZEFS_FUSE_ZC` | euid 0 | retention probe ∧ `…_ZC_RETENTION` | Mode |
|---|---|---|---|---|---|
| Absent | any | any (warn if on) | any | not probed | UserEnts |
| Present | off | any (warn if on) | any | not probed | UserEnts (warn) |
| Present | on | off | any | not probed | BufRing |
| Present | on | on | no | not probed | BufRing (warn) |
| Present | on | on | yes | Absent or lever off | ZeroCopy, bit 2 clear |
| Present | on | on | yes | Present ∧ on | ZeroCopy + PAYLOAD_RETENTION |

What each mode changes:

| Facet | UserEnts | BufRing | ZeroCopy |
|---|---|---|---|
| Per-queue ring setup | fixed files `[fuse_fd, wake_efd]`; anon payload arena | + fixed buffer index 0 (headers) + `IORING_REGISTER_KMBUF_RING` bgid 0 + region mmap | + sparse table `0..depth` (headers at `depth`) + `ZcBounce` memfd arena |
| REGISTER SQE | 2 iovecs `[headers, payload]`, `init.flags 0` | no iovecs, `init.flags = BUF_RING`, `buf_index = ent` | no iovecs, `init.flags = BUF_RING\|ZERO_COPY[\|RETENTION]`, `init.queue_depth = depth`, `buf_index = ent` |
| Drain-group width | derived `max(node_cpus/4, 1)` or explicit | 1 (forced) | 1 (forced) |
| Delivery, copyable payload | in the ent's arena buffer | in the attached kmbuf buffer (CQE bid law) | same (non-paged args) |
| Delivery, paged WRITE | arena buffer, §5.4 lease | kmbuf buffer, §5.4 lease | slot: HELD (W1 gate) or extracted at delivery to the bounce; lease over the bounce |
| Reply, paged body | `apply_reply` copy into arena | copy into kmbuf buffer | prefilled (direct leg) / `READ_FIXED(memfd\|pool fd → slot)` bridge, commit parked until CQE |
| Reply, header/other | `apply_reply` | same | same (kmbuf attachment) |
| Commit | plain | plain | plain, or RETAIN on armed page-cache writes |
| Teardown | drains synthesize errors for owed slots | + munmap regions; pins die with the ring fd | + bounce unmapped; retained ents RELEASEd by the drain before the unmount (§5.3; the reference left them to the kernel's reclaim — 0002 row 263); pends die with the worker |
| Gauges | `fuse3_kmbuf_negotiated 0`, `fuse3_zc_negotiated 0` | `kmbuf 1` | `kmbuf 1`, `zc 1`, `retention 0/1` |

`uses_kmbuf()` = BufRing | ZeroCopy (`kmbuf.rs:755-758`). Gauges are set after the arm
barrier (`fuse_over_uring.rs:3223-3225`).

---

## 7. Bridges

### 7.1 SQE shapes (all on the FUSE queue ring that registered the slot; `user_data` tag `Fetch` = 3)

| Bridge | SQE | Pend | Who |
|---|---|---|---|
| bounce reply | `READ_FIXED(Fd(bounce_fd), addr 0, len = body_len, buf_index = ent).offset(ent × stride)` | `BounceFetch` | worker, at commit routing (`fuse_over_uring.rs:5078-5088`) |
| fd-source reply | `READ_FIXED(Fd(pool_fd), 0, len, ent).offset(pool_off)` | `BounceFetch{fd_source}` | worker (`:5056-5060`) |
| direct read | `READ_FIXED(Fd(dev_fd), 0, len, ent).offset(dev_off)` | `HandlerFetch` | worker on `ZcFetchMsg` (`:5795-5805`) |
| direct store (W1 / overlay) | `WRITE_FIXED(Fd(dev_fd), 0, held_len, ent).offset(dev_off)` | `HandlerStore` | worker on `ZcStoreMsg` (`:5880-5885`) |
| lazy extract | `WRITE_FIXED(Fd(bounce_fd), 0, held_len, ent).offset(ent × stride)` | `LazyExtract` | worker on `ZcExtractMsg` (`:5947-5957`) |
| at-delivery extract | same as lazy | `WriteExtract` | worker at delivery (`:7640-7650`) |
| deadline cancel | `ASYNC_CANCEL(target user_data = Fetch\|gent)`, `user_data = Cancel\|gent` | — | worker's deadline scan (`:6201-6205`) |

`user_data = (tag << 32) | gent`, tags `Register 1, Commit 2, Fetch 3, Cancel 4, Release
5`, `UD_POLL = u64::MAX` (`fuse_over_uring.rs:797-836`). Pushes go through
`push_fetch_batched` (SQ full ⇒ flush then retry once) (`:8519-8532`). At most ONE pend
per ent (an ent serves one request between two commits); a second bridge on a pending
ent is refused `EBUSY` (`:5785-5793, 5870-5878, 5933-5941`). Device fds are
pool-lifetime; bounce memfds are worker-owned (`:8520-8523`).

### 7.2 Completion ordering with the commit

- Reply bridges: the commit is PARKED (`on_commit_parked`) when the bridge is pushed and
  SUBMITTED only from the bridge's full-length CQE (`:5090-5101, 5189-5194`). Short read
  (`res ≠ len`) has no resume protocol: fd-source ⇒ restage; bounce ⇒ fallback to the
  kmbuf attachment or header-only EIO (`:5196-5251`).
- Handler bridges (`HandlerFetch/Store`): the CQE result is forwarded on the oneshot;
  the handler validates full length, THEN replies — the slot's reply commit comes later
  through the normal path (`:5166-5180`, `zc.rs:295-315`).
- Extractions: the inbound push (at-delivery) or the lease mint (lazy) happens AT the
  CQE; the handler never sees the request/bytes before (`:5253-5311`).
- Mid-pass reap: finished bridges are reaped between SQE pumps so a fused handler can
  resume inside the same pass; CQEs the narrow arm does not resolve are deferred to the
  pass-bottom drain in arrival order (`:5442-5447`; charter §4 "mid-pass reap").
- RETAIN: a `WRITE_FIXED(device ← slot)` may complete AFTER the RETAIN commit
  (the request already ended); the RELEASE follows the store CQE (§5.3).

### 7.3 The deadline (`zc.rs:385-513`, `fuse_over_uring.rs:1644-1660, 6120-6214`)

`SQUEEZEFS_ZC_BRIDGE_TIMEOUT_MS` default **30 000** (range 100..=600 000), the D1.b
op-watchdog threshold's transport twin (`:1644-1649`). Per-worker ledger
`BridgeDeadlines{born[], submitted[], cancelled[], outstanding}` ent-indexed: `stamp` at
every pend SET (a re-stamp on a live pend is NOT fresh — no gauge double count),
`note_submitted` at the first flush after the push, `clear` at every resolution,
`overdue(now, timeout)` yields each overdue ent EXACTLY ONCE per pend lifetime (the
cancel-once latch, reset by `clear`/`stamp`) (`zc.rs:425-513`; pinned `zc.rs:746-807`).
The scan runs every `timeout / 1024` (≥ 1 ns) of enter cadence while
`zc_bridge_pends > 0`, also on idle queues (the watch thread wakes parked workers while
any pend is outstanding) (`:5482, 6128-6130`); it publishes `scan_pends_seen`,
`scan_orphans_seen`, `scan_max_age_ms`, `scan_cancel_latched`, and re-stamps any LIVE
pend with no ledger entry (`fuse3_zc_bridge_orphans`, must stay 0 — the Stage-1b field
wedge: `zc_bridge_pends = 4` with every ledger 0) (`:6165-6190`). Each overdue ent gets
ONE `AsyncCancel` + `fuse3_zc_bridge_cancels += 1` (must stay 0) (`:6191-6213`).

### 7.4 The cancel CQE ladder (`zc.rs:519-558`, pinned `zc.rs:829-858`)

| cancel `res` | pend live? | Action |
|---|---|---|
| any | no | `Nothing` — the original resolved already |
| `0` | yes | `AwaitOriginal` — its `-ECANCELED` CQE is coming and resolves the pend |
| `-ENOENT` | yes | `SynthesizeLost` — the kernel has no such op, its completion was POSTED and the CQ is FIFO, so it preceded this CQE: the completion was lost (zcws-9 W4 class); synthesize the resolution — safe exactly because no kernel op still references the slot; `fuse3_zc_bridge_lost += 1` (must stay 0) |
| `-EALREADY` / anything else | yes | `Restamp` — the op still runs kernel-side; synthesizing would let a late DMA alias a recycled slot/bounce; re-stamp (loud every period), the slot stays watchdog-named |

Worker-side handling at `:6585-6665`. The 100 ms park backstop fires iff any zc
bridge is pending (charter §4; `:6346`).

---

## 8. FUSE_TIME_LIMITS and per-queue background accounting

### 8.1 FUSE_TIME_LIMITS (patch 0023)

Kernel: `fuse_init_out` carves `int64_t time_min, time_max` (seconds) out of
`unused[11]` → `uint16_t unused[3]` FIRST so both i64s stay naturally aligned at byte
offsets 48/56 and the struct stays 64 B (`0023:15-18, 88-95`; pinned
`session.rs:7125-7162`). Advertised in `fuse_send_init` as `FUSE_TIME_LIMITS (1ULL <<
62)` (`0023:52-58`). `process_init_reply`: `if ((flags & FUSE_TIME_LIMITS) &&
arg->time_max) { sb->s_time_min = time_min; sb->s_time_max = time_max; }` beside the
`time_gran` block (`0023:40-50`) — VFS `timestamp_truncate()` then clamps
setattr/utimes exactly where the daemon clamps durable state (`0023:20-23`). Both
gates (echoed bit AND nonzero `time_max`) make absence bit-identical (`0023:30-33`).

Daemon: `negotiate_time_limits(kernel_capabilities) -> Option<(i64, i64)>` =
`Some((−9_223_372_036, 9_223_372_036))` iff bit 62 offered, else `None`
(`session.rs:6380-6386`). The range is the whole-second interior of the i64-ns word,
deliberately one second conservative on the floor (exact floor −9,223,372,037 s +
145,224,192 ns) so every kernel-clamped `(sec, nsec = 0)` is representable
(`session.rs:6365-6374`, `×1e9` fits pinned `session.rs:7105-7106`). Echo: `flags2 |=
(FUSE_TIME_LIMITS >> 32)` (bit 30 of `flags2`), which requires `FUSE_INIT_EXT` in the
reply (`session.rs:1802-1818, 6389-6400`). The venue's `fuse_init_out.unused[11]` is
still the carve-out and the watermark is still bit 42 (`README.md:313-318`).

### 8.2 Per-queue background accounting (patch 0026) — the daemon's half

No uapi change (`0026:65`). The daemon PUBLISHES `max_background = clamp(nqueues ×
depth, 64, u16::MAX)` in the INIT reply so each queue's share
`max_background / nr_queues` (+ remainder to the lowest qids, floor 1) is exactly its
ent count — "the only behavior change on that daemon is the locking"
(`0026:115-121`, `design-kernel-bg-per-queue.md:181-189`). `transport_max_background`
is the published gauge (`fuse_client.rs:12340`). A queue whose share is exhausted
blocks ITS submitters even while other queues have room (`0026:115-116`). Nothing new is
OBSERVED by the daemon; `fuse_chan_num_background()` (fusectl, congestion checks) is
the classical ledger + a lockless sum of the queues (`0026:58-60`). fusectl writes to
`/sys/fs/fuse/connections/<id>/max_background` re-gate every queue
(`fuse_uring_bg_limit_changed`, `0026:61-62`); `squeezefs tune` reads that file and
RAISES ONLY (`< 256 → 256`, congestion 192 — never lowers a live connection)
(`main.rs:8809-8830`). Measured: worker 17.4 → 14.3 µs/op (−18 %),
`native_queued_spin_lock_slowpath` 12.8 % → 0.11 %, rand-4k 508k → 556k IOPS (+9 %),
p99 −13…−17 % (`README.md:45-53`).

---

## 9. Every knob read (registry `src/env_knobs.rs`)

| Knob | Kind / default | Read at | Meaning |
|---|---|---|---|
| `SQUEEZEFS_FUSE_KMBUF` | Bool, on | `kmbuf.rs:787` | `0` = UserEnts even when Present (A/B lever; probe + gauges stay alive) (`env_knobs.rs:241`) |
| `SQUEEZEFS_FUSE_ZC` | Bool, on (ruling D16: reads +40–75 %) | `kmbuf.rs:792` | arm `FUSE_URING_ZERO_COPY`; declines loud without kmbuf/lever/euid 0 (`env_knobs.rs:242`) |
| `SQUEEZEFS_FUSE_ZC_RETENTION` | Bool, on | `kmbuf.rs:328` | arm bit 2 where the RELEASE probe is Present; arming alone is bit-identical (`env_knobs.rs:248`) |
| `SQUEEZEFS_ZC_BRIDGE_TIMEOUT_MS` | Int 100..=600000, 30000 | `fuse_over_uring.rs:1653-1657` | the bridge deadline (`env_knobs.rs:256`) |
| `SQUEEZEFS_READ_ZC_SERVE` | Bool, on | `cache/pool.rs:774` | build the whole-block fill pool as ONE memfd slab so warm serves bridge `READ_FIXED(fd → slot)` (`env_knobs.rs:245`) |
| `SQUEEZEFS_FUSE_ZC_WRITE_FUSION` | Bool, on | `fused.rs:422-423` | run small armed WRITE handlers on the worker's fused lane (`env_knobs.rs:243`) |
| `SQUEEZEFS_FUSE_ZC_READ_FUSION` | Bool, off (measured −6.5 % behind R-2) | `fused.rs` | fuse demoted READs at/under the ceiling (`env_knobs.rs:244`) |
| `SQUEEZEFS_FUSE_ZC_FUSION_MAX` | Int 4096..=1 GiB, derived `payload/8` | `fused.rs:394` | fused-dispatch payload ceiling (`env_knobs.rs:246`) |
| `SQUEEZEFS_ZC_ACK_EARLY` | Bool, on | `device_overlay.rs:272-274` | ACK-early for overlay stores (page-cache ⇒ RETAIN; O_DIRECT ⇒ extract-at-delivery Bytes arm) (`env_knobs.rs:249`) |
| `SQUEEZEFS_ZC_ACK_EARLY_ODIRECT` | Bool, off | `device_overlay.rs:277-279` | the held-GUP snapshot-then-ACK opt-in (`env_knobs.rs:255`) |
| `SQUEEZEFS_DEVICE_OVERLAY` | Bool | `set_zc_hold_streaming` arms the streaming hold bound (`zc.rs:372-379`) | widens `hold_candidate` to `payload + 1` |
| `SQUEEZEFS_FUSE_DRAIN_GROUP` | Int | `drain_group_width_env` (`fuse_over_uring.rs:3047`) | explicit width — IGNORED under kmbuf/zc (forced 1) |
| `SQUEEZEFS_TEST_ZC_DROP_WRITE_CQES` | TEST seam, Int | `fuse_over_uring.rs:1668-1683` | consume-and-drop N WRITE-class bridge CQEs (the lost-completion interleave) |
| `SQUEEZEFS_TEST_OVERLAY_BYTES` | TEST seam, Bool | `device_overlay.rs:241` | force the Bytes vehicle on |

**QUIRK 4** — `env_bool` in `kmbuf.rs:765-770` and the raw `std::env::var` parses in
`fuse_over_uring.rs:1653-1657, 1668-1675` are second readers beside the registry (the
startup gate refused malformed values, so they silently keep the default) — the
charter's ONE parser (decision 0002 rows 14/16): the Zig port reads every row through
`knobs.boolKnob/intKnob` on the registry row.

**QUIRK 5** — `SQUEEZEFS_READ_ZC_SERVE`'s doc string says "Default OFF until the field
r_cold/r_warm A-B-B-A adjudicates" while the row default is `"on"`
(`env_knobs.rs:245`); the charter says default ON (−24 % CPU/GiB). Doc drift; the Zig
row's doc states the measured default.

---

## 10. Every gauge / counter

### 10.1 Negotiation levels (0/1), set after the arm barrier (`fuse_over_uring.rs:3223-3225`)

`fuse3_kmbuf_negotiated` (`kmbuf.rs:859-864`), `fuse3_zc_negotiated` (`:874-879`),
`fuse3_zc_retention_negotiated` — the PRECONDITION gauge for ACK-early, not an
engagement counter (`:890-897`).

### 10.2 zc read/reply family

| Name | Kind | Meaning |
|---|---|---|
| `fuse3_zc_replies` | sharded counter | paged replies whose payload rode the slot (prefilled or bridged) — K1 skipped (`kmbuf.rs:977-990`) |
| `fuse3_zc_fallbacks` | counter ≈ 0 | a bridge failed and degraded to the kmbuf attachment / header-only EIO — the opcode-mirror safety net (`:992-998, 1034-1038`) |
| `fuse3_zc_slot_payload_skips` | must stay 0 (outside data ioctls) | announced payload on a zc queue with no attachment and no extraction arm (`:1040-1052`) |
| `fuse3_zc_fd_body_replies` | sharded counter | replies bridged straight from the fill pool fd (`:1000-1019`) |
| `fuse3_zc_fd_body_fallbacks` | ≈ 0 | fd-source bridge failed and re-staged through the bounce (`:1021-1032`) |
| `read_zc_serve_bytes` | counter | direct device leg bytes — a closure term (`fuse_client.rs:5715-5726`) |
| `read_zc_pool_serve_bytes` / `_warm_bytes` | counter | fd-source serve bytes / its hot+hold subset (`:5727-5745`) |
| `read_copy_dest_bytes`, `read_copy_bounce_bytes`, `read_dest_dma_bytes`, `read_dest_lease_bytes`, `read_copy_warm_serve_bytes` (+ `hot/hold/cache` partition), `read_copy_fill_slice_bytes` | sharded | the READ copy ledger (`:5629-5714`) |
| `read_dest_overruns` | must stay 0 | a zero-copy dest refused for not fitting the registered window (`:5684-5693`) |

**Read copy ledger closure** (per row): `dest + bounce + dest_dma ≈ user bytes served`
(`+ arena` for il); on an armed row `zc_serve + zc_pool_serve + dest + bounce +
dest_dma ≈ served` (`fuse_client.rs:5631-5632, 5722, 5734-5735`); `warm_serve ⊆ dest`,
`hot + hold + cache ≡ warm_serve`, `dest_lease ⊆ dest_dma`. An armed row whose READ
bytes `read_zc_serve_bytes` does not account is INVALID (`:5723-5724`).

### 10.3 zc write family

| Name | Meaning |
|---|---|
| `fuse3_zc_write_extractions` / `_extract_bytes` | COMPLETED slot→memfd extractions (at-delivery + lazy) and their bytes; a write row whose op count these do not account is INVALID (`kmbuf.rs:1054-1077`) |
| `fuse3_zc_write_lazy_extractions` | the LATE split — hold-gate staleness, ≈ 0 (`fused.rs`; `fuse_over_uring.rs:5260-5270`) |
| `fuse3_zc_write_directs` / `_direct_bytes` | COMPLETED `WRITE_FIXED(device ← slot)` DMAs, counted at the consuming site; `direct + extract ≈ user bytes` on an armed row (`kmbuf.rs:1079-1139`) |
| `overlay_store_submits_qids` | per-qid zc store census — the fabric-queue spread instrument (`kmbuf.rs:1092-1119`) |
| `fuse3_zc_write_fusions` / `_bytes` / `_demotions`, `fuse3_zc_read_fusions` / `_demotions` | fused-lane engagement (`fused.rs`) |

### 10.4 Retention family (`kmbuf.rs:903-976`)

`fuse3_zc_retain_commits` (RETAIN commits submitted — the ACK-early engagement counter),
`fuse3_zc_releases` (RELEASE submitted), `fuse3_zc_retained_outstanding` (gauge =
`retain_commits − releases`, 0 at quiesce), `fuse3_zc_release_failures` (must stay 0:
ENOENT double release / EBUSY raced a live commit), `fuse3_zc_retain_refused` (must stay
0: kernel `-EINVAL`, re-committed plain). Root side: `overlay_ack_early_stores/_bytes`,
`overlay_ack_early_retries` (acked custody retries forever) (`env_knobs.rs:249`).

### 10.5 Bridge family

`fuse3_zc_bridge_cancels` (must stay 0 — a bridge the ring never completed inside the
deadline, `kmbuf.rs:1141-1172`), `fuse3_zc_bridge_lost` (must stay 0 — a posted
completion the transport never reaped, synthesized, `:1174-1190`),
`fuse3_zc_bridge_orphans` (must stay 0 — a live pend that lost its ledger entry,
`:1150-1165`), pool gauges `zc_bridge_pends`, `scan_pends_seen`, `scan_orphans_seen`,
`scan_passes`, `scan_max_age_ms`, `scan_cancel_latched`, `zc_msgs_sent/taken`
(`fuse_over_uring.rs:3120-3128`), `zc_bridge_phase_ns` exact-sum phases `msg_hop /
sq_wait / device_cq / wake_hop` (`fuse_over_uring.rs:5807-5822, 8491-8515`; `zc.rs:263-271`).

### 10.6 Transport integrity tripwires touched here

`transport_requests_abandoned`, `transport_replies_oversize`,
`transport_replies_refused_stale`, `transport_requests_failed_synthetic`,
`transport_ents_retired` (`fuse_over_uring.rs:568, 645, 663, 680, 692, 8412`).

---

## 11. Constants with WHY, and the measured numbers

| Constant | Value | WHY (on the line) |
|---|---|---|
| kmbuf probe geometry | `buf_size = page`, `ring_entries = 8`, `bgid = 7`, arg span 256 B | a tiny valid registration; 256 B ≥ any foreign reader's struct (`io_uring_bpf` 72 B, compile-time `≥ 128` tripwire) (`kmbuf.rs:387-394, 2048-2060`) |
| ENOMEM retry budget | 5 retries, 50 ms doubling to 400 ms cap | the 2026-08-10 gate capture: page-cache churn refused 32 × 1 MiB ONCE with ample `available`; bounded so a truly exhausted box refuses within ~1 s; same registration each attempt (never a downgrade); non-ENOMEM refuses at once (`kmbuf.rs:396-433`, pinned `:2187-2237`) |
| `REQ_HEADER_SZ` | 288 | `sizeof(fuse_uring_req_header)` = 128 + 128 + 32 (`kmbuf.rs:1196-1198`) |
| `ring_entries` | `next_pow2(depth).max(1)` | kernel requires a power of two `< 65536` (`0001:55-60`) |
| headers span | `round_up(depth × 288, page)` | a page-granular anonymous mapping to register (`kmbuf.rs:1286`) |
| region mmap flags | `MAP_SHARED \| MAP_POPULATE` | the pbuf-ring convention; populate so no fault on the hot path (`kmbuf.rs:1393-1399`) |
| bounce stride | `round_up(payload_sz, page)`; span `depth.max(1) × stride` | ring ops need page-aligned offsets; zero depth degenerates to one stride (`zc.rs:183-188, 908-916`) |
| hold bound | `payload/2` strict; `payload + 1` under streaming holds | kernel-split streaming writes arrive payload-sized; W1 cap `block/8 ≤ payload/2` on every shipped geometry (`zc.rs:341-368`) |
| hold alignment | 4096 | LBA alignment for the direct DMA (`zc.rs:369`) |
| bridge deadline | 30 000 ms | the D1.b op-watchdog threshold's twin (`fuse_over_uring.rs:1644-1649`) |
| deadline scan period | `timeout / 1024` | runs ~1024 scans per deadline (`:5482`) |
| retention probe wait | 1 s | a parked probe cmd is ambiguous ⇒ Absent fail-safe (`:8540-8541, 8568-8573`) |
| `TIME_LIMITS_{MIN,MAX}_SEC` | ∓9 223 372 036 | i64-ns whole-second interior, floor conservative by 1 s (`session.rs:6365-6374`) |
| `patch_max_bytes` | `block/8` = 512 KiB at 4 MiB | charter §6.2 W1 `PATCH_MAX` (`fuse_client.rs:26248-26251`) |
| memlock explanation threshold | `already + this_queue > RLIMIT_MEMLOCK soft`, exempt if euid 0 or `CapEff` bit 14 | the limit binds the process's CUMULATIVE pins (512 MiB limit on 32 queues registers 16 and refuses the 17th) (`kmbuf.rs:486-549`, pinned `:2073-2185`) |

Measured numbers (bar to meet or beat):

- kmbuf deletes the per-4 KiB `unlock_request → GUP → lock_request` term: **12.8 %
  (locks) + 2.6 % (fill/GUP/unpin)** of ALL client cycles on the kern EXA read row
  (`kmbuf.rs:28-35`; `design-zero-copy-write-path.md:523-528`).
- zc serve (fd-source fill pool): daemon **CPU/GiB −24 % cold / −25 % warm**, `fuse3-ur`
  −37…−46 %, throughput ≥ par both orders (`.benchmarks/2026-09-05-r4-read-zc-serve.md:277-278`,
  `design-e2e-perf-audit.md:510`); passes/byte 2.0 → 1.0 warm, 2.69 → 1.69 cold-slice
  (`env_knobs.rs:245`).
- zc reads +40–75 % armed (ruling D16, `env_knobs.rs:242`); upstream's own 1 MiB fio:
  direct randread ~2100 → ~2600 MB/s (+20 %), buffered +25 %, buffered randwrite +10 %
  (`0019:23-33`).
- Hold of W1-ineligible shapes: **0.45×** (386k / 300k / 175k) (`fuse_zc_write_fusion_tests.rs:5-8`);
  all-lazy extraction **0.61×** durable streaming (`zc.rs:349-352`); at-delivery batch
  0.998× seq-1M / 0.909× durable (`.benchmarks/2026-08-06-fuse-zc-write-bracket.md:87, 111`).
- Known caveat: un-shimmed kernel-lane rand-4k writes pay ~20 % (the extraction hop)
  until fusion (`env_knobs.rs:242`).
- Per-queue bg accounting: −18 % µs/op, slowpath 12.8 % → 0.11 %, +9 % IOPS (§8.2).
- Fusion ceiling `payload/8` = 128 KiB at 1 MiB brackets the hop-vs-inline-copy
  crossover (~2 cross-thread wakes + 2 schedules vs a DRAM-bandwidth merge)
  (`env_knobs.rs:246`); read fusion measured −6.5 % behind fast dispatch ⇒ default OFF
  (`env_knobs.rs:244`).
- Retention probe verdicts measured live ×5 (`.benchmarks/2026-08-09-kernel-zc-write-v2.md:240-261`).

---

## 12. Tests (name — sentence; venue)

Pure / any host (the Zig port's portable suite):

- `kmbuf::test_buf_reg_layout` — `io_uring_buf_reg` is 40 B with `buf_size` in the leading union (`kmbuf.rs:1582-1594`).
- `test_init_flags_and_zc_headers_index` — zc never rides without BUF_RING; headers index = depth under zc (`:1599-1614`).
- `test_ladder_rungs_carry_their_track_id` — each rung names its track; `resolved_track()` agrees with the verdict (`:1623-1634`).
- `test_kmbuf_queue_indexing_math` — header stride 288, `region + bid × buf_size`, out-of-range bid refused; the attachment law over flagged/unflagged CQEs (`:1661-1706`).
- `test_kmbuf_setup_refuses_unaligned_payload` — refused before any registration (`:1811-1818`).
- `test_retention_verdict_table` — ENOTCONN/ENOENT Present; EINVAL/EOPNOTSUPP/0/EBUSY/Err Absent (`:1829-1860`).
- `test_retention_init_flags_composition` — bit 2 only on the zc arm; zc without retention = today's pair (`:1867-1887`).
- `test_retention_gauge`, `test_gauges`, `test_mode_uses_kmbuf` (`:1892-1950`).
- `test_ladder_pins_the_two_tracks` — 37/38 and 38/39 pinned, field first (`:1957-1965`).
- `test_opcode_ladder_decision_table` — Confirmed short-circuits; EINVAL then Confirmed ⇒ rung 2; all refused ⇒ Absent; ForeignSuccess never arms; loud errnos fall through (`:1977-2024`).
- `test_probe_arg_first_u16_never_reads_as_bpf_cmd_filter`, `test_probe_arg_padding_covers_foreign_readers` (`:2035-2060`).
- `test_memlock_refusal_names_the_limit_and_remedy`, `…_attributes_the_nth_queue_against_the_cumulative_pin` (`:2073-2185`).
- `test_register_retry_policy_bounds_and_classes` — ENOMEM retries bounded; other errnos refuse at once (`:2197-2237`).
- `zc::test_paged_opcode_mirror_by_track` — the per-track out/in-paged sets incl. the readdir divergence (`zc.rs:652-683`).
- `test_hold_candidate_bounds` — strict half-payload bound, alignment, zero length (`:690-709`).
- `test_zc_held_table` — per-(qid, ent) cells, clear, out-of-range never panics (`:718-732`).
- `test_bridge_deadline_ledger`, `test_bridge_deadline_restamp_rearms_cancelled_pend` — cancel-once, re-stamp not fresh, fresh deadline on recycle (`:746-807`).
- `test_cancel_cqe_action_ladder` — the four-arm cancel CQE table (`:829-858`).
- `test_bounce_dual_face`, `test_bounce_zero_depth` — VA↔FD reachability; zero depth refuses indexing (`:864-916`; needs `memfd_create` — Linux).
- `slot_state_tests::*` — the retained arc: RETAIN ⇒ `Retained`, release ⇒ `Replied`, `fail_ent` no-op on retained, release on unretained inert; the RETAIN/RELEASE SQE bytes (`fuse_over_uring.rs:8828-8945`).
- `cmd_entry` wire test — classical vs kmbuf vs zc REGISTER vs COMMIT byte offsets (`:9446-9526`).
- `apply_reply_tests` — oversize ⇒ header-only EIO, never truncated (`:8676-8762`).
- `session::init_negotiation_tests` — TIME_LIMITS bit 62, flags2 fold, 64 B layout with offsets 48/56, zeros inert (`session.rs:7065-7162`).

Linux, any kernel (capability-conditional — green on both strata, the branch IS the lattice):

- `test_probe_is_deterministic_and_cached` — Absent ⇒ UserEnts (`kmbuf.rs:1641-1654`).
- `test_kmbuf_setup_follows_the_capability_lattice` — Present: real setup over a 16-entry SQE128 ring, bid 0 is real memory; Absent: refusal names `KMBUF` (`:1714-1743`).
- `test_zc_setup_table_shape` — Present: zc setup; Absent: the sparse-table half still registers on stock 5.19+ (`:1753-1805`).

sqz kernel + root (CAP_SYS_ADMIN) — the `run_zc_capability_gate.sh` suites (`tests/run_zc_capability_gate.sh:1-50`; unprivileged runs self-skip class=capability, root turns a decline into a failure):

- `fuse_zc_write_fusion_tests::{w1_eligible_overwrites_fuse_without_double_pay, ineligible_growth_writes_extract_at_delivery_and_never_fuse, fusion_ceiling_bounds_inline_work_and_explicit_wins, fusion_lever_off_keeps_the_classic_dispatch, fused_write_lost_bridge_cqe_resolves_through_the_ladder}` — live mounts; the hold/fuse predicate, at-delivery extraction, the ceiling, lever off, lost-CQE recovery (`tests/fuse_zc_write_fusion_tests.rs:299-505`).
- `zc_bridge_cqe_wedge_tests::{lost_write_bridge_cqes_resolve_through_the_deadline_ladder, healthy_armed_mount_fires_no_bridge_tripwires, lost_overlay_store_cqe_resolves_through_the_deadline_ladder}` — `SQUEEZEFS_TEST_ZC_DROP_WRITE_CQES` drops N CQEs; the ladder resolves them; a healthy mount fires no tripwire (`tests/zc_bridge_cqe_wedge_tests.rs:278-403`).
- `zc_bridge_phase_tests::{cold_zc_reads_decompose_into_four_exact_hops_with_a_per_op_chain, composed_dispatch_law_partitions_every_read_on_a_zc_session}` — the direct-leg phase exact-sum + the READ partition law (`tests/zc_bridge_phase_tests.rs:316-557`).
- `bench_tests` zc/kmbuf rows.

In-process with injected slot sources (no kernel surface; Linux + mount support):

- `fuse_zc_serve_tests::{zc_direct_leg_engagement_and_fallbacks, zc_fetch_failure_falls_through_to_the_ordinary_ladder}` (`tests/fuse_zc_serve_tests.rs:245, 412`).
- `fuse_zc_write_tests::{direct_store_engagement_and_materialize_fallbacks, failed_store_falls_back_to_the_pooled_patch, materialization_is_memoized_across_retries}` (`tests/fuse_zc_write_tests.rs:302-476`).
- `overlay_ack_early_tests::{ack_early_write_returns_before_store_cqe, fsync_awaits_acked_inflight_store, read_waits_for_acked_inflight_store, failed_acked_store_retries_until_landed, odirect_class_needs_the_unstable_write_opt_in, odirect_ack_early_must_not_observe_post_ack_reuse, odirect_overlay_is_not_hold_eligible, bytes_overlay_ack_early_returns_before_device_cqe, dead_ring_store_falls_back_to_device_write, dead_ring_and_dead_extract_terminate_loud_never_wedge, …}` — the stability law and the retention lifecycle through the slot seam (`tests/overlay_ack_early_tests.rs:325-1143`).
- `read_zc_pool_serve_tests::{armed_fill_pool_is_fd_addressable_both_ways, fd_source_serve_elides_the_copy_on_pool_backed_arms_only, fast_probe_answers_served_fd_for_pool_backed_hot_blocks}` (`tests/read_zc_pool_serve_tests.rs:281-572`).
- `read_copy_ledger_tests::{ledger_closes_and_cold_none_dest_slice_is_zero_copy, warm_tier_serves_split_out_of_the_dest_bucket, warm_arms_and_the_cold_fill_slice_are_attributed_per_arm}` — the closure laws (`tests/read_copy_ledger_tests.rs:306-713`).

Kernel probe program (C, root, sqz kernel): `probes/kmbuf_smoke.c` — default ladder,
`--signatures`, `--fuse-rungs` (toy FUSE daemon: negotiation errno split; retention
round-trip: WRITE_FIXED samples the slot, READ_FIXED refused -EFAULT on the ITER_SOURCE
slot, COMMIT+RETAIN parks while write(2) returns, slot re-sampled byte-exact post-end,
RELEASE 0 → double ENOENT → live EBUSY; abort-race KASAN rung SKIP) (`kmbuf_smoke.c:1-55`).

---

## 13. Reference QUIRKS — ledger rows for decision 0002

| # | Reference quirk | Why poor | What the Zig port does |
|---|---|---|---|
| 1 | `euid == 0` as the zc capability pre-probe (`kmbuf.rs:796-799`) | the kernel gate is `capable(CAP_SYS_ADMIN)`; root with a dropped bounding set resolves ZeroCopy then dies at REGISTER | read `CapEff` bit 21 once (the module already parses `CapEff` for bit 14); refuse loud at resolution |
| 2 | Doc comment "a kernel that refuses the zc REGISTER degrades to BufRing loudly" (`kmbuf.rs:86-89, 780-782`) vs the code's protocol-fatal EINVAL (`fuse_over_uring.rs:6781-6785`) | a stated downgrade the charter forbids and the code does not do | fail-loud only; no downgrade path written |
| 3 | A hand-maintained per-track opcode mirror (`zc.rs:81-140`) with two tripwires for its misses | the kernel KNOWS whether it zero-copied a request; guessing it from the probe rung is an ABI inference | sqz patch: a `FUSE_RING_ENT_F_ZC_PAYLOAD` bit in `fuse_uring_ent_in_out.flags` set when `skip_folio_copy`; the daemon reads it per delivery; the table survives only as the pre-patch fallback |
| 4 | Second knob readers (`kmbuf::env_bool`, raw `std::env::var` + `parse` in `fuse_over_uring.rs:1653-1657, 1668-1675`) | two parsers for one convention (decision 0002 rows 14/16) | every row read through the ONE registry reader |
| 5 | `SQUEEZEFS_READ_ZC_SERVE` doc says default OFF, row says on (`env_knobs.rs:245`) | a row's doc disagreeing with its default | doc states the measured default (ON, −24 %) |
| 6 | The retention probe's own scratch ring + the kmbuf probe's scratch ring per rung, `std::thread::sleep` in the ENOMEM retry (`kmbuf.rs:426`) | ad-hoc rings and a blocking sleep on the arm path | probes take `io: std.Io` (decision 0001) and run on one scratch ring; the retry waits through `io` |
| 7 | `TransportBufferMode` resolved from THREE knobs + euid inside one function that also logs (`kmbuf.rs:783-831`) | mode resolution and announcement interleaved; not a pure decision table | a pure `resolveBufferMode(probe, caps, knobs) -> Mode` with a tie test over the §6 table; logging at the caller |
| 8 | `fuse3_*` gauge prefix (the Rust crate's name) | dashboards would bake a crate name in (decision 0002 row 21) | `transport_zc_*` / `transport_kmbuf_*` names, one registry row each |
| 9 | Field-track-first ladder order (37/38 before 38/39) chosen for the 6.19 fleet (`kmbuf.rs:163-167`) | order is a deployment fact, not a correctness one; probing 38 on 6.19 hits kmbuf-UNREGISTER (ENOENT, characterized side-effect-free but "the ladder must not rely on it", `kmbuf.rs:1988-1991`) | keep both rungs; put the venue's pair (38/39) first since the Zig product targets 7.2.x; both orders are correct by the conjunction argument |
| 10 | The W1 hold gate reads the daemon's lock-free metadata cache on the queue-worker thread (`fuse_client.rs:26279-26290`) | a filesystem probe on the transport thread; a cache miss is silently "ineligible" | keep the seam shape (the transport cannot decide alone), but make it a pure predicate over a published per-inode `HoldHint{striped, size, whole_block_map_bits}` snapshot the filesystem maintains — no map lookup on the worker |
| 11 | `hold_candidate`'s bound switches on a process-global `AtomicBool` armed from another knob (`zc.rs:372-383`) | hidden global state inside a "pure" predicate | the bound is a field of the resolved transport geometry, passed in |

---

## 14. Settled details the Zig port carries verbatim

1. Probe order at bring-up: INIT read → geometry → `fuse.enable_uring=Y` → INIT reply
   WRITTEN → kmbuf ladder (process-once) → retention probe (zc only, after the reply —
   `-EAGAIN` before `initialized` reads Absent, `-EOPNOTSUPP` on 7.2.6 without
   `FUSE_OVER_IO_URING` reads Absent) → per-queue setup → REGISTER pass.
2. Per-queue setup order: fixed files `[fuse_fd, wake_efd]` → headers region + fixed
   buffer table (sparse under zc) → `IORING_REGISTER_KMBUF_RING` (ENOMEM retry) →
   region mmap → bounce memfd (zc) → `depth` REGISTERs → PollAdd → one `submit`.
3. The attachment law: flagged CQE re-points, unflagged keeps, out-of-range ⇒ shutdown.
4. Never a zero-length `READ_FIXED`/`WRITE_FIXED`; never a `READ_FIXED` into a write slot.
5. One pend per ent; every pend stamped; every stamp cleared; one cancel per overdue pend;
   the cancel CQE ladder; the 100 ms park backstop while any pend is live.
6. Reply bridges park the commit until the full-length CQE; handler bridges forward the
   raw result; extractions deliver/mint at the CQE.
7. RETAIN only for page-cache writes the W1/overlay vehicle will consume; RELEASE after
   the store CQE; release-before-commit parks per ent and fires in the commit's batch.
8. `max_background = clamp(nqueues × depth, 64, u16::MAX)` so each queue's share is its
   ent count; `time_min/max = ∓9 223 372 036 s` echoed iff bit 62 offered.
9. Teardown: owed slots get synthesized errors; retained slots are the kernel's;
   pends die with the worker and return their gauge share; regions unmapped after the
   ring fd closes.
