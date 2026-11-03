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
| lapse | a heartbeat that finds no reservation re-acquires only while the device's claim names its incarnation, else fail-stops (symmetric and flat) |
| the rule | `guard/host_fence.zig`: a register only when the host is unregistered; before it, stale siblings (a page of theirs Recovering/Recovered/Free their own `r <writer> <appender> <term>` does not cover; a manager `p`/`m` while a reservation stands that our key does not hold, or at no reservation unless appender 0's Live page names it; a leaving `l` joiner by its pages) proven dead or pidfd-killed and their writes drained; after it, the report and pages judged again under the lock; an unreadable report refuses |
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

## nvmet 7.3's holder preempt — detection only, with possible loss

`nvmet_pr_preempt` answers `holder == reg` before it compares PRKEY: the WERO holder's preempt of a
member's key succeeds and fences nothing (fidelity leg `holder_preempt`: the victim stays
registered; under rtype 5 it goes, the hold kept). Rtype 5 fenced members window-free but the device
no longer kept ONE manager (review round 3 N1). The chosen design keeps WERO: the device arbitrates
the manager; a member's death preempt on nvmet is detected as ignored (`.ignored`, counted, never re-fenced) and its
host is NOT fenced: a frozen member resumed after the tail scan writes into recovered blocks — the
frame screen detects at load, and metadata frames written after the scan are lost. Data namespaces
have no registrants at all (owed since rung 6). Pinned by the fidelity row "a member's death on
nvmet 7.3 is DETECTED, not fenced" (`appender_recovery_preempts_ignored` 1, regctl 2, the full tail
scan ran). The fencer is the owner's decision. Draft upstream report: `~/tmp/nvmet-pr-preempt-bug.md`.

## The model

`src/meta/guard/host_fence_model.zig` (round 4: the code's inputs, the rule's read/register/re-judge
as steps under the registry lock): `Election` 255,611 states (depth 28) and `Member` 1,537 (depth 17)
hold. "A fenced member never writes" holds under spec semantics and FAILS under nvmet 7.3's (stated).
The accepted windows (PTPL-less lapse, a successor's clean last release, register→re-judge) are each
shown reachable. Every weakening fails — the invariant each breaks is §DF.14's table; majors 1–4 each
have one (round 3's `p` judgement → `KilledCurrent`; register-before-claim → `WroteUnheld`; no lapse
claim check → `FencedWrote`; no `l` line → `FencedMemberWrote`).

## Checks (7.3.1-1-omarchy-mainline, `nvme_core.multipath=N`)

See the final report for the counts at the final head (nvmet tier ×3, the 27 eviction/re-join/KILL
rows ×3, the unit filters, fmt, rails). Not run: the full gate (the orchestrator's); anything under
`nvme_core.multipath=Y`. Limits: §DF.13. Owed: a fence-triggered retire-and-rejoin of a
non-evicted sibling (§DF.8); the data namespaces' registrants (`data_pr`, owed since rung 6).
