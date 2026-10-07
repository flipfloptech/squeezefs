# Rung 3 (FUSE library) — the first real POSIX mount

Status: landed 2026-10-06 on `dev` (three waves: `09cc671` executor + uring128 + ABI/codec/geometry
+ mount options, `fbfa5ac` transport + handler + `KvFs`, this commit the CLI and the smoke). Specs:
`docs/porting/{exec,fuse_abi_session,fuse_uring_transport,fuse_handlers_lanes,fuse_kmbuf_zc}.md`;
decisions 0001, 0003 §D; 0002 ledger rows 51–61 (ABI/session), 62–70 (executor), 71–91 (transport,
handlers), 92–94 (this commit). The executor's own note is `rung-03-executor.md`.

## What landed

| Layer | Modules (tests) | Contents |
|---|---|---|
| executor | `src/exec/*` (see `rung-03-executor.md`) | `SqzIo`: per-core fuse lanes, the `sqz-meta` pool, journal lanes (`meta/journal_lane` cut over, 0002 row 6), `main` as a ring-parked lane (`run`), per-lane timers + `POLL_ADD`, ONE `sqz-timer` |
| ring | `io/uring128.zig` (20) | first-party SQE128/CQE32 binding + `ParkedRing128` (decision 0003 §D SQE128): the 80 B command area at offset 48 std's 64 B stride cannot address |
| ABI/codec | `fuse/abi.zig` (12 + fuzz), `fuse/codec.zig` (22 + fuzz) | every uapi struct through 7.45 as `extern struct` with `comptime` offset asserts; ONE `Delivery{header, op_in, payload}` for both venues (row 54), per-opcode parse, reply encoders, `errorHeader` always negated (row 55), notify frames with the uapi length law (row 57), `EntryReply{entry_ttl, attr_ttl}` (rows 51–52) |
| geometry | `fuse/geometry.zig` (23) | `plan` over `derive.Facts` + budget: `max_write`/`max_pages`/`payload_sz` as the kernel mirror, depth `clamp(cap/(nq×payload), 4, 32)`, `max_background`, drain groups by NUMA membership, every tie tested; the `transport_*` level gauges |
| mount options | `fuse/mount_options.zig` (8) | ONE `-o` parser (TTLs canonical decimal seconds, integers through `knobs.parseIntIn`, unknown refuses — row 56); root `mount(2)` data and the `fusermount3` string |
| transport | `fuse/transport/{slot,slot_model,commit_queue,worker,session,filesystem_vtable}.zig` (34) | the per-ent slot machine with the ONE-COMMIT law, the payload-lease commit gate (two SeqCst fences), the wake coalescer, preallocated commit cells (row 73), `sqz-fuse-ur<qid>` drain-group workers with the model-checked pass order, the arm barrier counting SUBMISSIONS, the sideband on ONE `ParkedRing` (row 71) carrying FORGET/INTERRUPT/stragglers and notify writes, teardown drains that synthesize every owed reply, residency, 60-field `writeStatsJson` |
| handler | `fuse/handler/{filesystem,lifecycle,op_guard,op_trace,open_table,watchdog,transport_seam}.zig` (36) | the `Filesystem` vtable (ENOSYS defaults — row 90), the lifecycle on the lane homed on the requester's CPU (row 82) with the reply obligation + the three must-stay-0 tripwires, the level-1 op guard with shared-mode admission and the stripe census (row 89), the op registry + deadline watchdog with the DERIVED per-class threshold `max(4 × p99, 5 s)` (row 86), the open table with `open_count_stranded` |
| filesystem | `fuse/handler/kv_fs.zig` (14), `backend_e2e.zig` (1) | `KvFs(Store)` — the POSIX must-implement set over the KV records (inode/dentry/xattr/inline layout), 4a guards as ONE canonical set per op, KILLPRIV_V2, `inline_max = page` with EFBIG past it (counted), the batched corpse reclaim; `BackendFs = KvFs(KvBackend)` |
| CLI | `cli/{cli,args,uri,format,mount,umount,status,daemon,supervisor,fusermount,log_sink,smoke}.zig` (30) | the verbs below |

### The CLI (charter §8)

- ONE grammar (`args.zig`) and ONE URI parser (`uri.zig`: `sqmeta://`/`sqdata://`, absolute paths,
  comma-separated sets; a relative path REFUSES naming the fix — row 92). `main.zig`: knob gate →
  `--version` → `cli.run`; exit 0 / 1 (refused or failed, the reason on stderr) / 2 (usage).
- `format [--single-writer] [--meta-node-kib N] [--meta-journal-mb N] [--force] sqmeta://p [sqdata://…]`:
  the preflight through the O_DIRECT pool (a live writer's flock refuses even with `--force`; Blank
  formats; anything else names `--force`), three independent `getrandom` identities, the invoking
  owner from `SUDO_UID:SUDO_GID`. **Metadata-only at this rung:** `sqdata://` URIs are parsed and
  NAMED in the summary, neither formatted nor recorded — the volume-set configuration that records
  them is rung 4's (no `format_config` xattr to smuggle them into, row 46). `--single-writer` and the
  plain form produce the same v4 one-member forest today (the symmetric plane is rung 6's, stamped on
  a live volume); the summary says which.
- `mount [-o opts] [--daemon --log-file PATH [--supervise]] sqmeta://p <mp>`: the bring-up in the
  charter's order INSIDE `SqzIo.run` (`cli/mount.zig` module doc): budget → backend (layer A flock;
  every `OpenError` loud through the ONE errno table) → sampler → `KvFs` + registry/guards/open table
  + `ring.Binding` → the FUSE fd (root: `mount(2)` in the session; else `fusermount3` over the
  `_FUSE_COMMFD` socketpair protocol, `cli/fusermount.zig`) → `Session.mount` → watchdog + reclaim
  cadence (one tick, 5 s) on `spawnMeta` → `ready` (log + the supervisor pipe) → serve on the signal
  eventfd through the main lane ring (SIGTERM/SIGINT latch stop; SIGUSR1 dumps every stats family to
  the log — the `.stats` inode's stand-in) → teardown: session close → watchdog/cadence cancelled →
  session deinit → backend close (final checkpoint) → exit 0, or 1 naming a FAILED volume.
- `--daemon`: the double fork with one pipe per hop (`cli/daemon.zig`): the invoker exits 0 only after
  `ready pid=N`, else prints the daemon's own failure text and exits 1; `--log-file` opened `0600
  O_WRONLY|O_CREAT|O_APPEND|O_NOFOLLOW|O_CLOEXEC` BEFORE the fork (a symlink is refused ELOOP), `std.log`
  routed through `cli/log_sink.zig` (one `write(2)` per line), stdio of both children pointed at it so
  the shell's streams are released at ready. `--daemon` REQUIRES `--log-file` (row 93).
- `--supervise`: the intermediate process stays as the external supervisor (`cli/supervisor.zig`): the
  pure `State.observe` machine (escalates exactly once, a success re-arms) over a `stat(mountpoint)`
  probe every 5 s on a detached thread (the `.stats` stand-in: the attr TTL is 1 s, so every probe
  reaches the daemon); after 30 s: `/proc/<pid>/{status,task/*/{comm,wchan,stack}}` to the log →
  SIGABRT → 2 s (one lock-plane tick) → SIGKILL → reap → `/sys/fs/fuse/connections/<minor>/abort` iff
  `waiting > 0` (the charter's ladder; the reference killed nothing — spec Q10). Exits 0 when the
  daemon exited on its own (a clean umount), 1 after an escalation.
- `umount <mp>`: `umount2(mp, 0)` as root (EBUSY retried one tick), else `fusermount3 -u` (10 × 100 ms,
  then `-u -z`); the daemon's DESTROY arm clears `mounted` (its own `umount2` could detach a NEW mount
  at the path) and tears down. `status sqmeta://p`: both superblock copies, the newest ledger record,
  a one-line summary with the writer-guard probe (`MOUNTED by a live daemon` / `not mounted`).

## The smoke (`zig build smoke`, part of the gate; Linux venue, root, 7.2.6-sqz, 8 vCPU aarch64, debug build)

`cli/smoke.zig` drives the built binary as a child process (`std.process.Child` under
`std.testing.io`) against a 256 MiB file volume on the venue's ext4 and a real mount at
`/build/tmp/mnt-<pid>`, through the shell's own tools. Three tests, 16 with the root blocks, ≈ 6 s:

1. `format` → a second `format` refuses (`already formatted … --force`) → `status` (`next_ino 2`, `not
   mounted`) → `mount` (foreground child) → `ready` → a second mount of the same volume refuses
   `single-writer guard (flock) … BUSY` → `status` says `MOUNTED by a live daemon` → the POSIX pass:
   `mkdir`, `mkdir -p`, `touch`, `echo >`, `cat`, `ls -la`, `mv`, `ln` (nlink 2, same inode), `ln -s`,
   `readlink`, `stat`, `chmod`, `setfattr/getfattr` (absent on the venue image — skipped by name),
   `cp -r` of a 7-file tree + `diff -r`, `tar -xf` of its tarball + `diff -r` + the symlink intact,
   `find` (≥ 20 entries), a 4097-byte write → `File too large` (EFBIG, `fuse_write_beyond_inline_refused`
   ≥ 1), a 4096-byte write succeeds, `rm -rf` → SIGUSR1 → the four `stats` families in the log with
   `transport_requests_abandoned=0`, `open_count_stranded=0`, `transport_park_tick_{commit,cqe}_rescues=0`,
   `exec_lane_tick_rescues=0`, `volume_failed=false` → `squeezefs umount` → the daemon exits 0 with
   `clean unmount` and the teardown line `open_count_stranded=0 transport_requests_abandoned=0
   tick_rescues=0/0` → remount: the backend reports `, clean unmount` and `0 torn dropped` → the tree
   intact (`hl` still nlink 2, `sl → d1/f2m`, mode 640) → `umount` → `status`:
   `summary: v4, 256 MiB, checkpoint seq 3, 30 inode(s) minted, not mounted`.
   Measured over the pass: **377 fuse ops = 377 ring requests, 1,113 `io_uring_enter` (2.95 per
   request), wake writes 369 / elided 8** (a serial single-client pass: little to coalesce).
2. `kill -9` mid-`tar -x` of 1,500 × 1,000 B files: the daemon is SIGKILLed once ≥ 200 files are
   visible (204 in the run), `tar` fails, `umount2` of the dead connection succeeds, the remount is
   NOT a clean unmount; every file present is whole (205 present, **205 whole, 0 empty**, 1,295 never
   created; a body is checked byte-for-byte against its name) — the coalesced-mode contract: an acked
   mutation is a LANDED entry, whole-transaction atomic, nothing acked lost, nothing partial. The
   window replayed 0 entries in the run (the 1 s checkpoint cadence had covered everything landed);
   the gauges on the recovered mount read 0 stranded / 0 abandoned; a clean cycle after it reports
   `, clean unmount` and `0 torn dropped`.
3. `--daemon --supervise --log-file`: a daemon mount of an UNFORMATTED volume returns 1 with `not
   formatted` on the invoker's stderr; after `format` the invoker returns 0 after `ready`, the log file
   is 0600 and carries `ready: … (pid N …)` and `supervisor: watching daemon N`, the mount serves the
   shell, `umount` ends the daemon (status 0x0) and the supervisor logs `exited (wait status 0x0);
   done`, no fuse mount and no held writer guard remain.

Transport e2e (`session.zig`, EchoFs, same venue, same run): the 10 s four-thread storm — **198,277
ring requests, 548,000 enters (2.76 per request), wake writes 186,906 / elided 11,453 (5 %), backstop
ticks 0, rescues 0/0**, parked ≡ unparked, leases 0, nothing abandoned/overflowed; the single-client
e2e 3.30 enters per op. Dev-box rows, scoping only (charter §11).

## Buffer modes (the final wave of rung 3; venue 7.2.6-sqz, root, CapEff with CAP_SYS_ADMIN, 8 vCPU aarch64, debug)

**Verdicts observed** (`src/fuse/transport/probes.zig`, every ladder a pure machine over injected
syscall results + a Linux driver on a scratch SQE128 ring; gauges `transport_probe_*`):

| Probe | Mechanism | Venue verdict |
|---|---|---|
| kmbuf opcode pair | register(`buf_size = page`, 8 entries, bgid 7) → 0 ∧ repeat → EEXIST ∧ mmap at `0x88000000 \| (7 << 16)` succeeds; pairs 38/39 then 37/38 | **Present, 38/39** (`kmbuf_ops=38/39`) |
| zero-copy surface | kmbuf present ∧ `CapEff` bit 21 (`/proc/self/status`, never `euid`) ∧ a sparse table of `depth + 1` slots registers | **Present** |
| payload retention | `RELEASE_PAYLOAD(commit_id = u64::MAX, qid 0)` on the plain fuse fd AFTER the INIT reply, BEFORE any REGISTER; ENOTCONN/ENOENT ⇒ present | **Present** (`-ENOTCONN`) |
| `FUSE_TIME_LIMITS` | init flag bit 62 offered | **Present**; echoed with `time_min/max = ∓9 223 372 036 s` |

Lattice (`resolveBufferMode`, a pure table): `auto` ⇒ **ZeroCopy** on the venue (width 1, retention
armed, no user arena, `transport_kmbuf_pinned_bytes` = queues × next_pow2(depth) × payload); `bufring`
⇒ BufRing (no arena; every delivery's payload read at the kernel address through the CQE bid —
`transport_kmbuf_attached_deliveries > 0`); `user` ⇒ UserEnts; an explicit `zc` with the cap forced
absent (`SQUEEZEFS_TEST_FUSE_CAP_SYS_ADMIN_ABSENT`) **fails the mount loud** naming the zero-copy probe
(`error.BufferModeRefused`), `auto` on the same seam settles on BufRing. The probe is the only degrade
point; no downgrade path exists (a post-probe refusal in `kmbuf.Kmbuf.setup` or a REGISTER EINVAL fails
the mount).

**What runs under each mode** (`worker.zig`): UserEnts the two-iovec REGISTER into the arena; BufRing
the headers as fixed buffer 0 + the kernel-managed ring at bgid 0 (`io_uring_buf_reg` 40 B, pow2
entries, the region mmap) with the attachment law (`zc.Attachment`: flagged CQE re-points, unflagged
keeps, out-of-range bid = `transport_kmbuf_attachment_breach` and shutdown); ZeroCopy + the sparse
table (headers at index `depth`, REGISTER with `init.queue_depth`), paged READ bodies through the slot
by the direct device leg (`Filesystem.direct_leg`, EchoFs null), the fd-source leg
(`replyFdBody(pool fd, off)` → `READ_FIXED(fd → slot)`) or the bounce leg (ONE MAP_SHARED memfd slab
for every queue, a non-sheddable budget component), paged WRITE payloads HELD when `zc.w1Shape ∧
holdCandidate ∧ Filesystem.write_hold_eligible` (a pure predicate; `PayloadLease.held/heldSource/
extract`) else extracted at delivery by `WRITE_FIXED(slot → slab)`; the slot direction law
(`transport_zc_direction_refusals`); bridges tagged `bridge`, deadline-bounded (30 s constant,
`SQUEEZEFS_TEST_ZC_BRIDGE_DEADLINE_MS` seam), ONE AsyncCancel, the four-arm cancel-CQE ladder, the
commit FOLLOWING the bridge CQE (the slot stays owed); retention as `COMMIT_RETAIN` on a held
`.page_cache` WRITE (`replyRetain`), `RELEASE_PAYLOAD` later (`releaseRetained`, ordered after the
pass's commits, a racing release parked behind its RETAIN commit), the stability class stamped at
delivery (`O_DIRECT ∧ !FUSE_WRITE_CACHE ⇒ .direct_gup`, refused the retain and counted); the fused poll
(`SQUEEZEFS_FUSE_READ_FUSION`, default OFF): a bounded run-queue of READ frames re-polled through the
filesystem's try-only ladder once per pass, ≤ 4 polls then the lane.

**Live contracts pinned** (session e2e, real mounts, nothing left behind): fd-source READ of `/big`:
`zc_serve_bytes ≡ user bytes` (1 048 576), daemon copies **0**, every reply rode the slot; with
`SQUEEZEFS_READ_ZC_SERVE=0` the bounce leg serves with exactly one copy per READ and no worker memcpy;
a W1-eligible aligned WRITE is HELD and extracted lazily by the handler (1 hold, 1 lazy extraction), an
unaligned one extracted at delivery, a filesystem that never declares eligibility extracts at delivery;
a stalled bridge (`SQUEEZEFS_TEST_ZC_BRIDGE_STALL`: a writer-less pipe as the source) is cancelled at
the deadline (312–315 ms at a 300 ms seam), `transport_zc_bridge_cancels == 1`, lost/orphans 0, the
reader gets EIO (header-only commit — THE LAW) and the next READ serves; retention: write(2) returned
while the slot was `Retained` (ACK-early observed: `retain_commits = 1`, `releases = 0`, `retained_
outstanding = 1`), the handler extracted AFTER the ACK from the still-registered slot, released,
`retain_commits − releases ≡ 0`; an O_DIRECT write classified `.direct_gup` had its retain refused
(`transport_zc_retain_refused_class`) and committed plain with the bytes snapshotted first; teardown
with a retained slot drained kernel-side and `mount | grep fuse` is empty afterwards; TIME_LIMITS:
`utimensat` far past the range read back as ±9 223 372 036 s at the daemon AND at `stat`; fused poll:
16 READs whose probe declined once were all served by the re-poll (fusions 16, late demotions 0),
inert on BufRing; the 10 s storm now runs under ZeroCopy: 171 661 ring requests, 3.27 enters/request,
backstop ticks 0, rescues 0/0, parked ≡ unparked, nothing abandoned.

**Side by side** (64 × 16 KiB preads of `/big`, one client, debug build — scoping only):

| mode | ring requests | enters | enters/request | daemon READ copies | zc_serve bytes |
|---|---|---|---|---|---|
| UserEnts | 64 | 64 | 1.00 | 64 (1.00/READ) | 0 |
| BufRing | 64 | 64 | 1.00 | 64 (1.00/READ) | 0 |
| ZeroCopy (fd-source) | 64 | 255 | 3.98 | **0** | 1 048 576 |

The zc leg's win is the copy and the kernel's GUP (not visible from userspace), not syscalls: a bridged
reply costs the bridge's own round trip (push → enter → CQE → commit) on top of the delivery's; the
reference's fused lane paid the same (bridge hops 35–47 µs). `transport_kernel_max_background` reads
126 = the negotiated word through fusectl (mounted on the venue during this wave); the kernel exposes
no per-queue background counters (patch 0026 changes the locking, not the uapi), so only
`max_background` is published.

**Found and fixed on the way:** the first run of the retention test left a D-state process: a lazily
detached mount whose connection the ring queues' devs keep alive never aborts when the daemon's fd
closes, so a FLUSH on an fd open across the detach waited forever — `unmountDetached` now writes
fusectl's `abort`; its first version stat'ed our own mountpoint for the minor (a GETATTR to ourselves,
blocking forever before the INIT reply on the refused-mode path) — the minor comes from
`/proc/self/mountinfo` (0002 row 137).

## Models (explorer, default limits; `slot_model.zig`)

| Model | States (holds) | Weakening | Fails |
|---|---|---|---|
| `LeaseGate(1)` — the WRITE lease vs the commit gate | 19 | the SeqCst pair as release/acquire (the park store buffered, the releaser's `parked` read hoisted) | `FinalViolated`: parked ∧ never woken — the depth-4 hang |
| `LeaseGate(2)` — two dest-DMA tokens | 81 | same | `FinalViolated` |
| `LeaseGateShutdown` — the drain vs a live lease | 11 | the drain writes the PAYLOAD on a false probe | `InvariantViolated`: a leased buffer written |
| `WakeCoalescer` drain → DISARM → scan | 161 | scan before disarm, or the disarm as a plain store | `FinalViolated`: a publication stranded at counter 0 |
| `WakeCoalescerLeaseDrop` | 288 | the gate's fence removed | `FinalViolated`: the drop's wake skipped, the commit never made |

The executor's six models are in `rung-03-executor.md`.

## Gate totals

- Linux venue (`scripts/venue zig build gate`): unit **1,023 pass, 4 skip** (the 3 smoke tests skip in
  the unit run — no `./squeezefs` in the cwd — plus one venue-conditional), rails 3/3, smoke 16/16,
  fmt clean; `pgrep squeezefs` and `mount | grep fuse` empty afterwards.
- macOS (`zig build gate`): unit **836 pass, 8 skip** (venue-only), rails 3/3, the smoke artifact's
  root blocks 13/13 (every smoke test is Linux-gated).

## Found and fixed on the way (red first)

- **`Io.fiber.contextSwitch` under LTO** (`exec/fiber.zig`): the daemon segfaulted at the first 5 s
  watchdog wake with a code address in the sleeper's `f` — only in the LTO'd binary (thin AND full),
  never in the test binaries. std's switch asm declares every register clobbered except the AArch64
  platform register x18, which Linux leaves allocatable; the LTO link runs the O2 pipeline and a caller
  with the switch inlined kept a live value in x18 across it. `switchIn`/`switchToLane` are `noinline`:
  a call boundary is the ABI's fence (no caller value lives in a caller-saved register across it, and
  nothing in the two bodies is live past the switch). The WHY is on the declaration; the gate profile
  keeps thin LTO.
- The session learned three things the daemon needs (tiny accessors, `session.zig`): a pre-mounted fd
  + an `Unmounter` (the fusermount3 path), an `on_shutdown` hook fired once from the idempotent
  `shutdown`, and `mounted` cleared at DESTROY / a disconnect (which now also ends the session, so an
  external umount or a sysfs abort reaches the daemon's teardown).

## Deviations (this rung) and owed items

- Buffer-mode lattice: landed (section above). Owed inside it: the direct device leg and the held
  slot's `WRITE_FIXED(device ← slot)` consumer are rung 4's (`Filesystem.direct_leg` returns null and
  `write_hold_eligible` false in the lifecycle binding; EchoFs exercises the hold/extract/retain arms);
  the fused poll runs the filesystem's SYNC try-only probe path per frame (the worker is a dedicated
  SQE128 thread, not an `exec.Lane`; a fiber-polling fused lane is the lane/ring-face cutover); the
  `FUSE_RING_ENT_F_ZC_PAYLOAD` bit-read waits for its kernel patch (the readdir class is probed by its
  first bridge meanwhile); the zc bounce slab is `queues × depth × payload` of populated memfd memory
  beside the kmbuf regions — its budget derivation is the data plane's call once the fill pool exists.
  The reference's `fuse3_zc_write_directs`, `overlay_*` and `read_zc_pool_serve_*` rows are rung 4's.
- Layouts: **inline only** (`inline_max = page`; a write past it is EFBIG, counted) — rung 4's
  staged/packed/striped layouts, the data volumes and the layout record as an `extern struct` (row 2)
  lift it; `format` names its `sqdata://` URIs and formats none.
- No `.stats` inode yet (rung 5): SIGUSR1 → the log, and the supervisor's control inode is
  `stat(mountpoint)` itself.
- `KvFs` serves ONE metadata volume per mount (`sqmeta://` sets of more than one member are refused
  loud); routing and the D0 ladder's layers B1/B2 are rungs 5–6.
- Owed: the `_FUSE_COMMFD` path has its module test (root may call fusermount3 too) but the
  unprivileged `mount` verb itself is untested on the venue (root only); the kill-matrix smoke pins
  the coalesced contract but did not catch a non-empty replay window in the run (the 1 s cadence
  covered it) — a `SQUEEZEFS_TEST_*` seam that holds the checkpoint would make the replay itself
  observable; `setfattr`/`getfattr` are absent on the venue image (the in-process xattr suite covers
  the records; the smoke checks them by name when present); the supervisor's escalation rungs are
  tested only as the pure machine (a wedged-daemon smoke needs a `SQUEEZEFS_TEST_*` park seam in the
  handler lane).
