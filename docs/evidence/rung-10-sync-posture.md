# Rung 10 — a frozen daemon never hangs the host's `sync` (2026-10-13)

The definitive gate after the syncfs work (`/tmp/gate-r10.log` on the venue) hung 23 min in the
nvmet fidelity row `sigstop-preempt`; everything else in it was green. Decision 0003 §Z.1–Z.5;
decision 0002 row 519. Every red was run on its red commit before the fix.

## 1. The hang

**Root cause** (three layers).
- **The rig.** The row SIGSTOPs daemon A, waits out `T_owner`, mounts B under a second identity and
  runs `echo after > g && sync && cat f` on B. Since §Y.1 the daemon negotiates FUSE_HAS_SYNCFS, so
  a global `sync(2)` sends FUSE_SYNCFS to EVERY FUSE superblock — A's included — and waits for
  each. A was stopped; the row resumes A only after that script returns.
- **The daemon.** Its INIT reply asked for no request timeout (`request_timeout = 0`, the
  reference's value), so nothing bounded a request a frozen daemon was handed. The waiter sits in
  `request_wait_answer` uninterruptibly once FR_PENDING is cleared (the request was assigned to a
  ring entry): `kill -9` of the `sync` frees nothing.
- **The kernel** (found once the daemon asked for a bound). With `request_timeout` = 15 s the
  connection DID abort (the daemon's log said so after SIGCONT), but the `sync` stayed held: the
  FUSE_SYNCFS sat in `ent_w_req_queue` (FRRS_FUSE_REQ), handed to the ring entry by a foreign
  submitter and waiting for the server task's task work (`fuse_uring_send_in_task`), which a
  `DEFER_TASKRUN` ring runs only at its owner's next `io_uring_enter`. Neither
  `fuse_uring_abort_end_requests` (fuse_req_queue + the background queue) nor the entry teardown
  (in-userspace, available, retained) reaches that list. dmesg logged ` ent-req-queue … state=3`
  every 50 ms (`fuse_uring_log_ent_state` sets `stop_debug_log`; nothing reads it).

**Reproduced first** (no product change): `default_request_timeout` cannot be set inside the
container (`/proc/sys` is read-only), so a hand mount + SIGSTOP + global `sync` on the d42a24dd
kernel showed the plain hang: `sync` blocked 90 s in `request_wait_answer ← fuse_chan_send ←
fuse_sync_fs ← sync_fs_one_sb` until SIGCONT.

## 2. The posture (0003 §Z.1–Z.3)

`request_timeout = T_owner + T_park_max`, whole seconds rounded up (`geometry.requestTimeoutS`):
**135 s** at the constants — 45 s + (45 s failover + 45 s grace). A wire member or an `-o ro` reader
takes its membership's `T_owner` and the move gate's `T_park_max`; a `--single-writer` mount the
same bound at the constant `T_owner`.

- **Why that value.** The longest a healthy daemon holds a request is a wait on a peer the lattice
  is giving up: ≤ `T_owner` for the set to declare it dead (a member's own park starts at
  `T_self < T_owner`), then the park, ≤ `T_park_max`, past which the op answers the terminal EIO
  (§X.1). Everything else a handler can wait on is shorter: the journal's FAILED latch (3 ×
  `PARK_ESCALATION_NS` = 90 s), the coherence hold (20 s), a token recall (≤ one lease TTL), the zc
  bridges (30 s), the syncfs drain (parked custody ≤ budget / 16: seconds at device rates). A
  request older than 135 s waits on nothing the lattice sanctions — the daemon is stopped, wedged
  or its substrate stalled past every deadline: the supervisor's abort rung (charter §4), done by
  the kernel, armed with or without `--supervise` and when a SIGSTOP freezes the supervisor too.
  No handler is dropped (the charter's ban on per-op timeouts stands).
- **The price** (§Z.2): a data device that holds a request past 135 s (nvme-tcp's own give-up is
  `ctrl_loss_tmo`, 600 s) now ends the mount ENOTCONN instead of resuming.
- **What the daemon does.** INIT sends FUSE_REQUEST_TIMEOUT iff offered (flags2 bit 10;
  `request_timeout` at byte 40); FUSE_HAS_SYNCFS rides only beside it (§Z.3 — the transport tests'
  EchoFs, which asks for no timeout, declines syncfs). `transport_request_timeout_s` is the value
  the kernel applies (`/proc/sys/fs/fuse/max_request_timeout` caps it — a cap below 135 s warns
  at mount; the kernel's 15 s check period floors it; it reads 0 on this venue). A connection that
  ends while the mount's id is still in the table is the abort (an unmount removes the entry
  before the superblock ends the connection): the daemon logs it as an error, tears down in
  order and exits 1; the mount stays attached, ENOTCONN, until it is unmounted.
- **Seam.** `SQUEEZEFS_TEST_FUSE_REQUEST_TIMEOUT_S` [15, 65535] (15: the kernel's floor).

## 3. The kernel (0003 §Z.4)

`scripts/kernel/patches-7.2/0028-sqz-fuse-uring-abort-ends-requests-a-stopped-server-never-claimed.patch`
on the 0001–0027 series: dispatch (`fuse_uring_queue_fuse_req`, `_queue_bq_req`) marks the entry
`tw_pending` under the queue lock; `fuse_uring_send_in_task` claims its request under the lock
before it touches it; `fuse_uring_abort_end_requests` ends every still-pending entry's request
(-ECONNABORTED, the queue's background charge credited) and leaves the entry without one, which
the task hands to the teardown (cmd done -ENOTCONN) when it finally runs — a dying task frees it as
before. The synchronous paths (COMMIT_AND_FETCH, the task work's own next fetch) never set the
mark. The expiry check tolerates a request-less entry; the stuck-entry dump logs once.

Built with `scripts/kernel/build-venue-kernel /tmp/ksqz-abort` in 5 min at 4 CPUs, no warnings:
`Image-7.2.6-sqz` sha256 `26555485075157cb…` (config sha `f6dfc7317983…`, unchanged). Boot-tested in
a throwaway container, installed as the container default (`container system kernel set --force`),
`sqz-zig` recreated on it (`scripts/venue up`, the `squeezefs-dev` image of `b652702`). The previous
kernel (d42a24dd, 0001–0027) stays at `/tmp/ksqz-syncfs/dist/kernel-sqz-container-arm64/`.

## 4. The rigs (0003 §Z.5)

Every rig syncs only what it wrote: `sync -f .` in three smoke scripts and the six nvmet fidelity
scripts (the sigstop-preempt helper among them); `sync -f "${SUITE_SETDIR[$tag]}"` /
`sync -f "$SET_DIR"` before the suites' kmbuf-ENOMEM compaction retry (common.sh, mount.squeezefs);
`sync -f "$RESULTS"` at `suite_cleanup`; `sync -f /build` in `scripts/venue trim` and the bracket's
scoping loop; the bracket's `settle` reports a failed `sync -f <mnt>` instead of falling back to a
host-wide `sync`. The rails' **rig sync rail** fails on a host-wide `sync` (the word in command
position with no operand) in any string literal under `src/` and `tests/` (one line per
multiline-literal token) and any shell script under `scripts/`.

## 5. Red → green

| row | red (its commit) | green |
|---|---|---|
| smoke `A FROZEN DAEMON NEVER HANGS THE HOST'S \`sync\` (0003 §Z) — …` | `5878539`: `HostSyncHeld` — the sync still held 45,010 ms after it began waiting on the frozen mount (bound 45 s); on the fix without 0028: 45,026 ms, the daemon's abort line printed only after SIGCONT | the sync returned **15,068 / 15,120 / 15,182 / 15,023 ms** after it began waiting (two rows alone, two gates); `.stats` → ENOTCONN, `sync -f <mnt>` reports "Transport endpoint is not connected", the mount still in the table; SIGCONT → the abort line, exit 1; the remount negotiates 135 s and reads `f` back |
| rails `rig sync rail: no rig script runs a host-wide \`sync\` …` | `6b952f8`: 9 rig literals (smoke ×3, fidelity ×6) + 6 script lines (suites ×3, venue trim, bracket ×2) | `6d0bf19` |
| rails `the sync scanner: …` | — (the scanner's own pins) | green |
| fuse.codec `INIT reply carries the request timeout …`, `INIT reply asks for FUSE_SYNCFS … only beside the request timeout …`, the never-advertised sweep (2,000 random offers × wants) | new | green |
| fuse.geometry `request timeout (decision 0003 §Z): T_owner + T_park_max …` (the tie: 135) | new | green |

The frozen-daemon row is robust to its host: before it freezes its daemon it skips
(`external_interference`) when any other task on the venue is stopped (a host-wide sync would
measure THAT server), its bound runs from the instant the `sync` is seen in `fuse_sync_fs`
(`/proc/<pid>/stack`) — the host's own writeback comes before — and it never waits unbounded on its
`sync` child.

## Commits

| item | red | fix |
|---|---|---|
| the posture | `5878539` | `4f688b4` (0003 §Z, 0002 row 519), `3910ff0` (kernel 0028), `737346d` (daemon), `e1e7472` (the row's host census) |
| the rigs | `6b952f8` | `6d0bf19` |

## Loops

- **sigstop-preempt** (`zig build nvmet-smoke -Dnvmet-filter=sigstop-preempt` on `7bb2d09`, the
  fixed rigs and daemon on the 0028 kernel): **10 of 10** green, 56–57 s each (the row sleeps
  `T_owner` + 5 s); one more green run before the loop (217 s with the build), and green in both
  venue gates below.

## Gates

- **macOS**: on `7bb2d09` (before the rebase) **28/28 steps; 2,192 of 2,219 passed, 27 skipped**;
  on `e1e7472` (rebased on `dev` at `93fdfae`) **28/28 steps; 2,214 of 2,241 passed, 27 skipped**
  (unit 2,088 / 27 skip, rails 18).
- **Venue gate 1** (`46accd9`, before the rebase; the 0028 kernel, an idle venue): **46/46 steps;
  2,809 of 2,881 passed, 72 skipped** — unit 2,566 / 70 skip, rails 18, shim-census 3, smoke 88 /
  2 skip (the frozen row: 15,182 ms), bench-smoke 41, shim-smoke 40, nvmet-smoke 53
  (sigstop-preempt among them), suites-smoke clean.
- **Venue gate 2** (`e1e7472`, the landed tree; a `.wt-perf` gate alongside, load ≤ 2.8): **46/46
  steps; 2,815 of 2,887 passed, 72 skipped** — unit 2,572 / 70 skip, rails 18, shim-census 3,
  smoke 88 / 2 skip (the frozen row: 15,023 ms), bench-smoke 41, shim-smoke 40, nvmet-smoke 53,
  suites-smoke clean.

## Found on the way, stated, not changed

- **A concurrent gate on an older tree hung in the same row on the new kernel.** A `.wt-perf`
  gate (pre-fix tree: its binary asks for no timeout and its helper runs the global `sync`) sat
  with its A stopped and its B's `sync` in `fuse_sync_fs` — the row resumes A only after that sync,
  so the row (and every later host-wide sync on the venue) would have waited for its 65-minute
  timeout and past it. Its A was resumed by pid (SIGCONT, nothing killed) — twice, in two of its
  gates (20:31 and 21:13 venue time); the row proceeded each time.
  Every tree carrying an older rig is exposed until it rebases on this one.
- **Unmounting a frozen uring daemon's mount waits for that daemon.** `fuse_conn_destroy` →
  `fuse_chan_wait_aborted` → `fuse_uring_wait_stopped_queues` waits for the ring's queue refs, and
  an entry whose task work never ran keeps one until the task runs or dies — with 0028 its
  REQUEST is ended, the entry's ref still waits. A `kill -9` of the frozen daemon frees it. Owner:
  the kernel series.
