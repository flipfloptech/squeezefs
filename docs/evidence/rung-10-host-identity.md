# Rung 10 — one NVMe host identity per host (2026-11-02, decision 0003 §DF)

**The ruling** (the owner, on §DB.4, option C): one NVMe host identity — hostnqn/hostid and the
registration key — for every mount of a set on a host; PR fencing per host (the GPFS node model);
co-located mounts supported; the kernel's own multipath in production. Safety rule: no
registration that re-admits a host while a stale sibling of the set lives there. The venue boots
`nvme_core.multipath=N` and keeps two-host rows through explicit pairs (or the per-mount seam).

## What changed

| | change |
|---|---|
| identity | the data connects resolve the host's identity (explicit pair → `/etc/nvme` → machine-id, unsalted); `SQUEEZEFS_TEST_PER_MOUNT_IDENTITY` salts the derived one by the mount point |
| associations | users counted in `<run>/hosts/fabric-<key>/<pid>` under one bounded flock; the last user deletes the controllers |
| key | `hostKey(wire host id, set)` (FID 0x81, not the configured text) |
| symmetric PR | the manager holds **WERO** (the device keeps one manager); a writer open reads the claim off a private replay first (a refusable claim: no registration, no write), then registers its host and takes the hold before its first device write, and re-reads the claim under the hold; a joiner adopts (host registered) or registers before its JoinAppender; its membership join carries the key |
| lapse | a heartbeat that finds no reservation re-acquires only while the device's claim names its incarnation; another's fail-stops (symmetric and flat), an unreadable one skips the beat |
| release | a hold taken by a preempt or inherited stays at the incarnation's last clean unmount (the next mount adopts or preempts it); only a fresh hold with no preempt under it releases |
| the rule | `guard/host_fence.zig`: a register only when the host is unregistered; before it, stale siblings (a page of theirs Recovering/Recovered/Free their own `r <writer> <appender> <term>` does not cover; a manager `p`/`m` while a reservation stands that our key does not hold, or at no reservation unless appender 0's Live page names it; a joined `j`/`l` line by the page it names — appender id, term) proven dead or pidfd-killed and their writes drained; after it, the report and pages judged again under the lock; an unreadable report refuses |
| recovery | the report re-read once the page is Recovering: a host registered meanwhile is preempted again |
| death screen | the departed-key table kept; a key held on another node contradicts, the dead member's own node's hold does not |

## The reds

- **co-located mounts** — red `expected 3, found 1`; and a sibling's umount deleted the shared
  data controller. Green: one controller, regctl 1 under WERO, A registered, B adopted (same key),
  B's umount leaves both, A's releases both (regctl 0).
- **stale sibling** (round 3, `0c86614`) — three hosts: host A's manager B frozen past its claim's
  TTL, two successors (P1 parked after arbitrating, `SQUEEZEFS_TEST_GATE_ARBITRATED_HOLD`; P2):
  exactly one binds; host A fenced by the takeover; a mount whose kill is refused exits naming a
  stale sibling; A2 kills the superseded B, registers, joins P2. Red on e18d273: P1's open refused at
  its first device write (no registration before the gate). Under rtype 5 the review's N1: two
  successors bind (the model's weakening reproduces it).
- **review B1/B2/B3** (units, `98822ec`): a successor's Recovering page spared by its predecessor's
  retirement; a sibling turned stale between rule and register left alive; recovery never re-reading
  the report; a departure unregistering the holder's key. Round 3 adds the reused-appender-id case
  (the term), the reboot death-screen case, the withdraw-kill-refuse sequence.

- **review 3, round 4** — reds committed alone: `d83ff74` (units: a live manager's `p`+`m` entry
  with its page Live read stale at a lapse; no `l` line for a sibling mid-leave — 2 fail) and
  `74f461e` (fidelity: a remote joiner's writer attempt moved the report's generation by 3, not 1;
  a superseded manager resumed after its successor's clean last unmount never fail-stopped — 2
  fail). Green after the fix commit.

- **review 4, round 5** — the models first (`b2e1f1e`: today's code fails — a joined line without
  its page spares a reused pair's member, the `l` Free exception spares a released recovery's page,
  the hold released at a takeover's last unmount lets M0's checkpoint and a paused open overwrite
  acked metadata, `OverwroteAcked`); reds alone: `a737216` (blockers 1/1b: "expected .stale, found
  .joining / .retired"), `4e434e3` (minor 3: ".failed, found .fenced"), `e4747f8` (minor 5: the live
  manager fail-stopped), `f17567a` (blocker 2: regctl 0 after the takeover's unmount), `94944ba`
  (minor 4: the open never ready). Green after `7c9ddef`, `f25afcf`, `4cb616c`, `9c79170`, `cf2258c`.

- **review 5 + the gate, round 6** — the gate's sigstop-preempt red was the journal lane running the
  conveyor's acked-window rollback (it fail-stops) BEFORE classifying the write's reservation conflict:
  the fence was never counted (reproduced 1/10 under `cpu+smoke`; red `86cf813` "expected 1, found 0",
  fix `d8bac78`; the row then 15/15 under load, 5/5 idle). Major 1 (a live manager preempted on a claim
  reading older than the rule): red `ae81450` (X's `writer_guard_preempts` "expected 0, found 1"), fix
  `5cf4d64` (the claim read right before the hold). Minors 1–3: reds `0b08a17`, fix `bb932a4`.

## The probe cost (counter deltas, the probe-cost row)

Every NVMe writer open replays the claim privately three times (round 6; each: both superblock copies,
the 128 KiB ledger, the WHOLE fixed ring). On a 256 MiB ring: 805,675,008 B per open, 0 frames on a
clean first open, 12,021–12,030 on a kill -9 remount (≈ 4,000 per probe; six runs, two under load). The derived ring caps at 32 MiB
(≈ 96 MiB per open); `--meta-journal-mb` reaches 1 GiB (≈ 3 GiB per open). A lapse adds up to three
more per beat. Documented (review 5 minor 4); a knob or a lighter probe is later work.

## nvmet 7.3's holder preempt — detection only, with possible loss

`nvmet_pr_preempt` answers `holder == reg` before it compares PRKEY: the WERO holder's preempt of a
member's key succeeds and fences nothing (fidelity leg `holder_preempt`: the victim stays
registered; under rtype 5 it goes, the hold kept). Rtype 5 fenced members window-free but the device
no longer kept ONE manager (review round 3 N1). The chosen design keeps WERO: the device arbitrates
the manager; a member's death preempt on nvmet is detected as ignored (`.ignored`, counted, never re-fenced) and its
host is NOT fenced: a frozen member resumed after the tail scan writes into recovered blocks — the
frame screen detects at load, and metadata frames written after the scan are lost. Data namespaces
have no registrants at all (owed since rung 6). Pinned by the fidelity row "a member's death on
nvmet 7.3 is DETECTED, not fenced" (`appender_recovery_preempts_ignored` 1, no preempt or re-fence
landed — the unfenced tail scan — regctl 2). Superseded 2026-11-03 by the fencing registrant (decision 0003 §DG, `rung-10-fencing-registrant.md`): the row now reads the host FENCED. Draft upstream report: `~/tmp/nvmet-pr-preempt-bug.md`.

## The model

`src/meta/guard/host_fence_model.zig` (round 4: the code's inputs, the rule's read/register/re-judge
as steps under the registry lock; round 5: a joined line's page, `releaseRecovered` and pair reuse,
`OverwroteAcked`, the kept preempting hold): `Election` 219,189 states (depth 28; round 6: the claim read right before the hold,
`PreemptedLive`) and `Member` 7,216 (depth 21) hold. "A fenced member never writes" holds under spec semantics and FAILS under nvmet 7.3's (stated).
The accepted windows (PTPL-less lapse, register→re-judge) are each shown reachable; round 4's release
window is closed (a hold taken by a preempt or inherited stays at the last unmount). Every weakening fails — the invariant each breaks is §DF.14's table; majors 1–4 each
have one (round 3's `p` judgement → `KilledCurrent`; register-before-claim → `WroteUnheld`; no lapse
claim check → `FencedWrote`; no `l` line → `FencedMemberWrote`).

## Checks (7.3.1-1-omarchy-mainline, `nvme_core.multipath=N`)

See the final report for the counts at the final head (nvmet tier ×3, the 27 eviction/re-join/KILL
rows ×3, the unit filters, fmt, rails). Not run: the full gate (the orchestrator's); anything under
`nvme_core.multipath=Y`. Limits: §DF.13. Owed: a fence-triggered retire-and-rejoin of a
non-evicted sibling (§DF.8); the data namespaces' registrants (`data_pr`, owed since rung 6).

## The death proof (2026-11-04; decision 0003 §DF.17–DF.18; branch `fix/guard-proc-thread-group` off `96c0b2a`)

The guard's dead-pid proof (`proc.startTicksOf`, under every caller: the claim prover, the §DA writer-id
proof, the registry's live entries, the stale-sibling rule, the co-located kill, the joined census, the
membership rows) read a stat whose state is Z or X as dead: "a zombie holds no fd". A main thread can exit
before the rest of its group, though, and the other threads still hold the fd table and can still issue
I/O. The proof is now the thread group's. A running or stopped main thread is a live group. A Z or X stat
is judged on a pidfd pinned by the start ticks (`proc.pin`, shared with the umount verb), which is readable
only once no thread is left. A pidfd refused other than by ESRCH reads alive, never dead. The dead set only
shrinks, so decision 0004's rules need no design change.

| check | result |
|---|---|
| red `457207c`: main thread a zombie, a second thread holding the claim's flock | `probe.provablyDead` read it dead (`proc.zig:874`); `killColocatedMember` killed nothing (`host_fence.zig:1918`), 5/5 each |
| fix `80f7c72`: both tests, no noise of this work's (load 2) / beside the full smoke's 16 shards + 16 busy loops | 10/10 / 40/40 (load 30–35) |
| proc, host_fence, claim, membership, joined, cli.umount filters | 215 + 7 skipped of 222 |
| the 27 eviction/re-join/KILL rows ×1 | 71/71 |
| the nvmet tier ×1, alone | 72 + 1 skipped of 73 |
| full `zig build test`; fmt; rails | 3,367 + 195 skipped of 3,562 (23/23 steps); clean; 34/34 |

Seen, not changed: the nvmet tier's `expectNoResidue` reads an NQN that `Rig.unshare` already freed. It
segfaults only beside another tier's share with an NQN of the same length: a first run beside two other
agents' tiers crashed 4 rows at `configfs.zig:621`.
