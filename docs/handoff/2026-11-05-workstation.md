# Handoff — 2026-11-05

## Current state and next action

The owner requested a transferable handoff, then asked to continue and approved
landing on `dev`. The recovered **metadata Write Exclusive–All Registrants (WEAR,
reservation type 5)** preparatory work, its review fixes and the independently
reviewed CPU-accounting repair have landed. **The actual metadata WEAR switch remains PR-5.** Product metadata still
uses Write Exclusive–Registrants Only (WERO, type 3) and the intermediate fencing
registrant; data namespaces already use WEAR.

Read `AGENTS.md`, `docs/charter.md`, decision
`0004-metadata-wear-preempt-all.md` (revision 27), and the applicable decision/evidence
sections before changing core code. PR-N denotes the planned deliveries in decision
0004; these are local branches.

**Published implementation checkpoint:** `18e0bf978c1267ae0ce476bca985c6e6b31c4102`
was fast-forwarded onto `dev`; the live GitHub `ls-remote` check and local head
matched, and the hook logged a successful push. Its full gate passed **75/75 steps,
3,813/4,031 tests, 218 skipped, exit 0**, with **zero integration safety findings**.
All implementation, review, build and monitor tasks have completed. DO.33 re-admits
PR-4 on this host. PR-5 remains behind PR-4. The fatal teardown leg stays unrun.

**Approval and documentation:** the owner approved “yes, land them on dev” on
2026-11-05. This handoff accompanies that landing. Documentation-only commits may
follow the verified implementation checkpoint; re-check actual Git heads on resume.
Future publication, hazardous target experiments and host changes need their own
applicable approval.

## Local branches — preserve these

Workspace root: `/home/justin/Source/squeezefs-zig`.

| Worktree | Branch | Verified implementation/review checkpoint |
|---|---|---|
| `.wt-pr-integration` | `integrate/metadata-pr2-pr3` | `18e0bf978c1267ae0ce476bca985c6e6b31c4102` |
| `.wt-pr2` | `feat/nvmet-preempt-all-drain` | `2bbd84217d89ccad175693ed40e5725490d3dad1` |
| `.wt-pr3` | `fix/keyed-deaths-page-v2` | `deeeeea7bd47a5b8fcc36849646108e1fda56754` |
| `.wt-stats-cpu` | `fix/stats-cpu-sample` | `eff66002b786a087223039090a974831dae4f016` |
| `.wt-train` | detached, idle at recovery | `7f8cef6` |

The four feature/integration trees were clean at their latest checks. Re-check
status and live venue ownership on resume. The original branches retain their
red/fix histories; never delete/recreate them because an earlier agent failed.

Final census found no userspace build/test/load, venue lock holder, SqueezeFS
mount, or nvmet subsystem/port. The gate monitor completed successfully. A new
session must re-check current task/process/tree state before starting writers;
every implementer and reviewer in this continuation has reported completion.

## What was recovered and implemented

### PR-2: preempt-all/drain substrate and native safety fixes

`pr.Client` now supplies `preemptAllAndAbort`, `drainBarrier`
(`registered` / `unregistered` / `unknown`) and the `admittedHere` rename. The fake
models nvmet's registration order, holder pointer, generation, admitted commands,
per-victim kill/unregister/drain and conformant-target answer mode. Real loop/TCP
legs cover the PR matrix/canary, 50-preempt storm, slow null_blk drain and graceful
disconnect. The new preempt-all/barrier APIs have test/leg callers; PR-5 wires the
WEAR discipline into the product.

Initial runtime/compile reds: `3ae3a52`, `7b78d17`; implementation `9fe11a4`.
The freed-NQN harness regression/fix is separate: `8717a74` → `7ff8ded`.

Independent review then found seven software defects, all fixed and re-reviewed:

- Native waits preserve command/buffer ownership through EINTR and other errors.
  Unconsumed SQEs retract on the Device's non-SQPOLL ring; accepted requests retain
  the issue lock and payload until their own unique-tag completion. Unexpected
  CQEs are diagnosed and consumed while the owned request stays live. Ring128
  retries re-read SQ consumption and SQ-poller wake state. A completed-CQE errno
  remains a transport error. Reds/fixes include `f6b6569` → `bc2da3b`,
  `78c95be` → `bbe6151`, and `57beccb` → `91cd3d4`.
- Shared-Client barriers use local answers and sharded counts, preserving sequence
  diagnostics without taking the sequence/member-progress lock.
- Fake abort bookkeeping allocates before holder/ref/semaphore mutations.
- Tickets, threads, descriptors, write handles, Client arenas and NQN transfers
  have success/error ownership. The existing manifest-removal helper is reused.
- The slow-drain witness requires actual native SQ consumption, unreaped CQ state
  and a retained write; false pre-issue `done` flags supply no proof.
- Unavailable/erroring/overrun kernel-log observation fails its verification;
  EAGAIN ends the currently observed stream. Native tripwires are exported through
  the real stats source, with inverse-census rails passing.

Both native correctness and simplicity reviews have **zero open findings** at
`2bbd842`; source identity is `91cd3d4` (the later commit changes docs only).
Durable record: decision 0003 **DO.18–26**, its two-sided audit/software-fix sections,
and `docs/evidence/rung-10-nvmet-preempt-all.md`.

### PR-3: keyed deaths, page v2 and one owned census

The appender page carries `host_key` at byte 296: fixed part 304 bytes, slot budget
108; layout 1 temporarily decodes key zero. `JoinAppenderReq` grows 40 → 48 bytes.
The successor expects durable rows ∪ Live pages across volumes, carries page keys
into grace-reaped deaths, and ships proven-dead same-host identities before
listener start. Recovery cross-checks durable keys before fencing/releasing.

Runtime red `58e726a`, compile red `1d7d3b3`, fix `5e40b0a`. Follow-up
`f1ee836` → `6be7ab4` proves each identity once across volumes. Failed-open leak red
`059bea1` exposed two allocations; `deeeeea` owns one enriched immutable census on
both success and failure and preserves counts/ID membership/identity proofs.
Both final reviews have **zero open findings**.

Limits deliberately remain documented:

- The key report proves some registered host's key; the wire supplies no trusted
  NVMe host-id binding to the requester. A lying authenticated host-A member can
  claim live host B's key. This is the recorded requester-binding residue.
- Frozen/dead layout-1 pages remain keyless until rewritten; a keyless death can
  still reach no fence during migration. PR-5 conversion/PR-8 retirement remain.
- Conflicting durable keys hold indefinitely. The log/help names `appender clear`
  and host-wide fencing. Actual reserved-NVMe CLI admission is later PR-5 work;
  the keyless-record unit does not certify that escape end to end.

In the integrated ledger PR-3 uses **DO.27–32**. Its source/test/evidence references
were renumbered together. Record: `rung-10-metadata-wear-model.md`, PR-3 section.

### CPU accounting: two proven double-billing defects

A broad post-fix unit run failed the unledgered eight-renderer/one-sampler stress:
126/127 passed, failed counter 16. A passing isolated retry did not adjudicate it.
The counter's error branches can add at most eight terminal errors, so CPU-sum
violations occurred; the exact historical tids/interleaving are unrecoverable.

`dbbd153` → `2e48c2b` proved proc iteration can omit live tails: retiring every unseen
entry double-bills a reappearing task. Presence/proof failures preserve the last
publication; only positive task absence retires an entry. A cursor regression
waits for actual proc disappearance, with owned threads.

Review then found class changes and schedstat/stat granularity decreases falsely
inferred reuse. `e7bc686` → `0901406` holds one proc task-directory anchor per known
PID object, reads relative to that identity, and attributes only new class deltas.
Known source/high-water baselines survive coarse observations. Anchors close on
replacement, absence, refusal, OOM and teardown; they retain PID identity without
keeping tasks alive. There is no class/total clamp or changed publication model.
`eff6600` removes only duplicate fixture setup. Both final reviews have **zero open
findings** at that head.

Record: decision 0003 **CC.7–8**, decision 0002 **row 570**, and
`docs/evidence/rung-10-stats-cpu-sample.md`. Schedstat fallback remains permitted
when valid; other live read/proof failures refuse publication. Stdlib ESRCH
messages on real disappearance remain a separate, unsuppressed diagnostic.

## Verification — keep source identities distinct

Counts below are per invocation and overlap; do not sum them as unique tests.

| Check | Verified result |
|---|---|
| PR-2 final focused source `91cd3d4` | 113/113; rails 34/34; fmt/diff clean |
| PR-2 safe matrix/slow/disconnect at `00a3693` | each 42/42; loop/TCP exercised |
| PR-3 final `deeeeea` full gate | 75/75 steps; 3,736/3,947 passed, 211 skipped, exit 0 |
| PR-3 affected units/F9-F9b | 259/260, one seam-only skip; 42/42 |
| CPU B1 source `0901406` | 72/72; 20 idle + 20 loaded repetitions; 12,800 documents |
| CPU final fixture `eff6600` | worker 41/41; reviewer 48/48; fmt clean |
| Combined pre-CPU `d6551cb` | 328/329, one seam-only skip; rails 34/34; fmt clean |
| Combined pre-CPU F9/F9b | 42/42; each recovery and metadata/fencer/data preempt 1 |
| Final combined `18e0bf9` full gate | 75/75 steps; 3,813/4,031 passed, 218 skipped, exit 0 |
| Final combined integration safety review | Zero findings at `18e0bf9`; reviewed source identities and merge resolutions verified |

The integrated tree preserves PR-2's allocation-safe `Rig.unshared` implementation;
PR-3's duplicate `bb3b480` contributed no additional change and was skipped. All
other PR-3/CPU commits were replayed with origin references. CPU/native source
identity against the reviewed branches was checked before the final gate.

Archived pre-interruption evidence remains under `/home/justin/tmp/pr2*`,
`pr3-status.md`, `pr3-review.md`, and `gate-pr3.log`. Those notes predate some final
fixes. Fresh session-owned temporary reports/logs and helper/build scratch were removed
at checkpoint. Durable tests, commits, commands/counts and source reasoning remain
in the repository evidence/decisions; all original branches/caches were preserved.
The complete final gate transcript is retained in the CLI session tool log:
`/home/justin/.grok/sessions/%2Fhome%2Fjustin%2FSource%2Fsqueezefs-zig/01a185b4-a2e4-7703-baae-439f1f684edc/terminal/call_lzYPlF7mZnVrBEM5IZoyS9Qh.log`.

## Kernel premises: DO.33 re-admits PR-4

Both hazards survived independent, two-sided investigation of NVMe/blk-mq sources
matching the cached pristine kernel.org 7.3.1 archive. Fatal manifestations and
frequency remain **unreproduced**.

**TCP cycle:** an admitted non-inline victim write needs R2T/data. Linux permits
write capsule → PR capsule/payload → write data. The PR handler can block its
queue's receive/send worker on `pr_sem`, while abort retains that semaphore waiting
for the write's ref. Queue teardown waits in `cancel_work_sync` before releasing
incomplete writes. Namespace URING reservations share write-class hardware queues;
separate userspace rings and the Device lock provide no transport isolation.
Existing beat guards narrow exposure, and new PR-4 completion hooks need a valid
progress premise. The recovered harmless trace used a timer-completed 4 KiB write
on another queue and supplies no fatal reproduction.

**P17 lifetime:** `do_abort` retains a raw victim ref across its wait/resurrection;
controller destruction erases/exits/frees it under a different lock. Graceful host
I/O drain and a successful disconnect do not order resurrection before free.
Keyed data fences already exercise the common abort mechanism.

**DO.33 (2026-11-05) resolves the gate.** The owner rejected a multi-guest KASAN
laboratory. The TCP cycle is real in the source and was not reproduced: preempt-and-abort
returns to its queue worker after queueing the abort, and the wedge is a later
reservation command blocked in `down(pr_sem)` on the same TCP queue as a write whose
data has not arrived. PR-4 may proceed on the fake and on this host's existing loop
and TCP legs. Its own commands avoid that schedule: no `pr_sem` command on a
controller until that controller's already-submitted writes have completions, and the
park has stopped new submissions. P17 stays unreproduced. PR-4 does not disconnect a
controller and does not run the fatal teardown leg here; R12 stands. linux-nvme 09/19
and 19/19 name the two defects and were unmerged in the snapshots checked on this
date. The private proposal `test/disposable-nvmet-validation` at `bab3b80` built no
guest. A fatal teardown leg still needs its own approval and one disposable target.
That leg is not part of PR-4. Report/read remains not a drain barrier. No fatal leg,
patched module or upstream message was sent or run.

## Resume and approvals

1. Re-check heads, dirty work, active tasks, mounts/configfs and venue lock holders.
   Continue from `dev`, which contains the approved integration; preserve the
   original branches and their red/fix histories.
2. The unchanged `18e0bf9` implementation has its final gate and review. A changed
   source needs its own final verification; failing rows require analysis and
   actual defects need red-first fixes. Keep untested platform limits explicit.
3. This landing was approved and its code push verified. Obtain applicable owner
   approval for future publications; never push a feature branch or force-push.
   Verify remote SHAs after hooks publish; SSH has failed intermittently.
   No unmerged worktree/branch cleanup is authorized here.
4. PR-4 is re-admitted by DO.33, pending owner objection. On
   `feat/member-readmit`: the pure contract in `member_fence.zig`, and the
   journal lane's `Hooks.readmit` and the checkpoint binding's
   `DeviceLoader.readmit`. A `.held` answer on a write or barrier refusal
   returns `error.Held` and skips the fence latch, the failure count, the
   rollback and the hole checkpoint. The checkpointer does not count a held
   barrier toward the generic latch. Null hook is today's path (the
   acked-window fence test and the loader conflict without a hook still
   latch). `error.Held` is retryable and replies EAGAIN.
   `LoaderError` and the cycle's `IoError` include `Held`.
   `JoinedSet.writePages` and `AppenderSet.writePages` name `Held` and keep
   it distinct from `Io`. A held page write during a slot handover becomes
   `HandoverDeferred`. The volume vtable, tails spill, and the KVDA page
   writer name `Held` (a spill or a manager verb maps it to `Deferred`).
   `CycleError` includes `Held`, so a held checkpoint is "not this tick".
   A joined mount installs `Readmit` on the journal lane and the device
   loader. A reservation conflict is held and `ParkGate.parkFor(.succession)`
   runs. `extendTo` grants one longer deadline per observed bind. The
   checkpoint tick's `censusBeat` reads the appender pages into `Readmit.obs`:
   a successor page holds, a new recovering sibling or this incarnation's
   page leaving Live latches (`noteFence`), and a readable registered report
   clears the resolution word. The tick reads that report from the joiner's
   own metadata client (`JoinedSet.fence.client` via `host_fence.reportFacts`,
   which also returns `rtype`). A joined mount has no guard. No fence, or a
   failed report, stays unreadable. An unregistered current member
   (successor page, own page Live) re-registers through
   `registerUnderRule` once the succession park is up and
   `pool.writesOutstanding()` is 0 (DO.33). A fresh registration whose
   page left Live, or whose word is already terminal, is withdrawn.
   A completion hook captures the resolution generation before it
   classifies. A held journal-lane write or barrier waits on that
   generation (`awaitPast`) and is re-issued once a later `clear`
   advances it; a terminal word returns `error.Held` without fencing.
   A `.retry` answer spends one credit and the lane re-issues the write
   or barrier once; a second refusal with no credit left latches.
   A hook without a word still returns `error.Held` immediately.
   KD20: a joined dismount waits for a pending succession (only while
   the park is up) up to `T_park_max`, then stores `terminal`; lane
   shutdown stores `terminal` first so a waiter cannot hang.
   `squeezefs umount --force` asks the admin lane (`admin_dismount_force`,
   0x0707) to store `terminal` on every pending joined resolution word,
   then unmounts as today. `--lazy --force` is accepted. A mount with no
   admin lane (no wire, no `.stats`) has nothing to force. If the admin
   call fails, umount continues and the succession wait still applies.
   Pages
   already seen while registered are not a later sibling death. A manager
   mount installs no hook. Do not build
   the KASAN fleet and do not run the fatal teardown leg on this host. PR-5
   depends on
   PR-2/3/4 and adds WEAR, manager-page currency, windows/debt, fencer retirement and
   `claim convert-pr`. PR-6 is load/soak; PR-7 is multipath-Y coverage; PR-8 cleanup
   requires explicit later owner go-ahead.

```sh
git status --short --branch
git worktree list --porcelain
cd /home/justin/Source/squeezefs-zig
scripts/venue 'timeout 3900 zig build --cache-dir "$SQZ_BUILD_ROOT"/zig-cache-v2 gate --summary all'
```

`-Dnvmet-filter` selects fidelity legs; `scripts/venue test "<filter>"` selects units.
The gate owns the venue slots; agents use targeted checks. Start no duplicate gate
while the current one runs. Keep new scratch under `/tmp/`.

## Venue, owner rulings and other owed work

- Verified venue: Linux `7.3.1-1-omarchy-mainline`, Zig 0.17.0,
  `fuse.enable_uring=Y`, `nvme_core.multipath=N`, 32 threads/8 slots. No SqueezeFS
  kernel patches; Omarchy display/scheduler patches leave NVMe sources unchanged.
  Stock Linux ≥7.3 and modern interfaces are requirements; the owner will run an
  RC kernel before accepting deprecated code. No production-Y or aarch64 run here.
- Every Linux command goes through `scripts/venue`; heavy non-build work takes a
  slot. No host kernel/sysctl/boot/config install, reboot or cloud run is approved.
  RDMA and LICENSE remain on hold. Preserve unrelated work.
- KD22 scopes the WEAR design to nvmet acquire semantics with a runtime canary/
  named refusal for conformant targets. It is an orchestrator decision pending
  owner objection, as recorded in decision 0004.
- Fencer udev installation remains unapproved; reconsider its need with retirement.
  `/home/justin/tmp/nvmet-pr-preempt-bug.md` remains an unsent draft; the owner sends.
- Benchmarks require AC, a quiet host and governor level 0 before **every** mdtest;
  use existing `/home/justin/tmp/abba-gov.sh`, then clean only the owned RAM rig
  with its `ramnvme.sh down`. No timing benchmark was performed in this continuation.
- Unassigned prior follow-ups: successful real-device re-join with data fence
  (§DN.10); §DM stale-row/leave race, >1,025 census paging, linear renewal lookup;
  §DL slow-member rule on writer thread; other inode-keyed removal maps, reclaim
  starvation, loaded `transport_lease_overlong`, §CI.8, strict flush-zero no-open
  close (§CO.36). Earlier notes retain their details.
- Owner checks remain: s2idle resume and the ported display fixes on 7.3.1.

The original PR-2/PR-3 fixer agents failed HTTP 404 for an unavailable model.
Their filesystem work survived and was recovered; that service failure was separate
from test outcomes. This note and the local branches let a new LLM resume in the
same checkout without recreating their work.
