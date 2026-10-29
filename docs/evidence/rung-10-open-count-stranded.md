# Rung 10 — `open_count_stranded` under fsstress: a RELEASE answered EAGAIN by a full handler lane (2026-10-29; decision 0003 §CP)

**Symptom.** The must-stay-0 tripwire fired 64–1,311 times per `fsstress -p 32 -n 10000` on dev `74d419c` (with or
without the preload), never in 50 runs on `0778bb4`. Rig `~/tmp/open-count/run.sh`: 2 MDT × 4 GiB + 2 OST × 32 GiB
sparse images, `--supervise`, `-o default_permissions,allow_other,interception`, pid-tagged paths, no full store. On
`46fe48a`: stranded 7 / 36 / 15, and 200 / 488 / 385 inodes still counted open after fsstress exited.

**Bisect** (`0778bb4..46fe48a`, a build and two runs a step): `b4b0610` bad (96 / 81), `beb3d9d` bad (its `src/` is
`74cd347`'s; 750 refused dispatches), `0778bb4` good (0 stranded, 0 open, 0 refused). **First bad: `74cd347`** (§CD).

**Cause, with proof.** A kernel census (bpftrace on `fuse_request_end`, `fuse_chan_queue_forget`) of one run:
OPEN + CREATE answered 0 — 1,218,501; RELEASE answered 0 — 1,218,039; **RELEASE answered -EAGAIN — 452**; CREATE
-EAGAIN — 3. For each of the 15 stranded inodes the kernel's opens minus its answered releases equals the daemon's
stranded handle count. `worker.Group.dispatch` answered a refused spawn (the handler lane at its fiber capacity)
`failEnt(.AGAIN)` without the handler, so `removeOpen` never ran; the kernel discards a RELEASE's answer and never
re-sends it, and the inode's last FORGET found the count. `transport_dispatch_refused`: 0 on every good run, 238–750
on every bad one. The lanes were full because §CD.7's bulk passes (`staging.runPass`, `plane.drainTask`) fan out one
`Io.Group.async` member per CPU, which `SqzIo` places on the caller's lane: a syncfs handler's pass took up to 31
fibers of a FUSE lane holding its slots' handlers plus a 16-fiber slack (RES-18).

**Leak or miscount.** The census alone: the corpse destroy keys on the kernel's lookup count (§AF.6) — no space
leaked, fsck clean after every run. Lost with each RELEASE: its table entry and gauges for the mount's life, a virtual
inode's pin, the close posture's close drain; and any data-class request met EAGAIN (three `creat(2)` in one run).

**Red** (`6364531`). session e2e "A RELEASE NEVER MEETS A FULL LANE" (the closer pinned to a CPU, that lane held at
capacity): `transport_dispatch_refused` +1, the RELEASE never reaches EchoFs. sqz_io "optional fan-out from a fiber on
a FUSE lane …": a handler's 2 × capacity `Group.async` members parked → the lane refuses the next handler spawn.

**Fix** (`97820bc`). (1) `Group.spawnHandler`: a full lane hands the handler to the worker's own lane (one fiber per
slot of its group plus the slack — a slot's handler finds room), then the rotation; EAGAIN only if every lane refuses
(`transport_dispatch_fallbacks`); the sideband's straggler walks the rotation. (2) `Lane.spawnOptional`: `Io.async` /
`Io.Group.async` reserve a unit of the lane's `optional_cap` (one RMW; recycle returns it) and run inline past it
(`exec_fanout_inline`); a FUSE lane's cap is one per CPU (`fanoutSlack`, the pass's width) and its capacity grows by
it, so a lone pass keeps its width and no fan-out holds a handler's fiber; `concurrent` keeps the whole lane.
`fanout_slack_model` holds; `check_then_add` and `no_bound` (the shipped spawn) refuse the handler.

**Checks.** Both rows green; unit `exec.`, `fuse.transport` 128 (19 kernel-feature skips), `data.staging` 65,
`data.plane` 51, `data.fsync` 49, `fuse.handler` 211; fmt, rails. fsstress on `97820bc`: **20 of 20** without the
preload (0 stranded, 0 open left, 0 refused, 0 fallbacks, 0 tripwires, fsck clean; `exec_fanout_inline` 703–2,146 a
run); under the preload 15 runs, 10 clean and 5 the shim's wedge (seen (a)), 0 tripwires in all; with that wedge's fix
cherry-picked (§CK, `eee0b89`), **20 of 20** clean. Smoke: the POSIX pass, THE UNLINK STORM'S INODE LAW, the data plane WITH the write
cache, kill -9 mid-tar -x (both cadences), 32 CONCURRENT READERS OF .stats. xfstests generic/001 and 013, and 013
under `FSTESTS_PRELOAD=1`: pass, fsck 0 findings.

**Seen, not changed.** (a) The shim's preload wedge, §CK (its fix landed on dev as `eabdff3` after this branch's base `46fe48a`): ops parked at
`meta_backend_start` / `write_admitted` until the kernel's 135 s request timeout, the supervisor's SIGABRT. (b) The
fstests test set's teardown logged `residency backstop lapsed` once per group (96) — also on another branch's run
(`db6e47d`), absent on others. (c) Explicit spawns (`spawnGroupOn`: prefetch, hold ceremony, IPC handoffs, FIEMAP)
are not counted against the slack; a handler they crowd out takes the fallback (0 fallbacks in 50 runs). (d) A rig
writes ≈ 19 GiB of real blocks (fsstress's gap zeros): two rigs at once filled `/build/tmp` and failed the metadata
devices ENOSPC (the harness's) — two bisect runs and two of ours stalled so.
