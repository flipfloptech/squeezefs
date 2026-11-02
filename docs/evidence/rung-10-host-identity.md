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
| symmetric PR | the manager holds **WERO** (the device keeps one manager); a writer registers its host before its first device write; a joiner adopts (host registered) or registers before its JoinAppender; its membership join carries the key |
| the rule | `guard/host_fence.zig`: a register only when the host is unregistered; before it, stale siblings (a page of theirs Recovering/Recovered/Free their own `r <writer> <appender> <term>` does not cover, a superseded or pending manager) proven dead or pidfd-killed and their writes drained; after it, judged again under the lock (a sibling turned stale withdraws the registration, then dies) |
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

## nvmet 7.3's holder preempt

`nvmet_pr_preempt` answers `holder == reg` before it compares PRKEY: the WERO holder's preempt of a
member's key succeeds and fences nothing (fidelity leg `holder_preempt`: the victim stays
registered; under rtype 5 it goes, the hold kept). Rtype 5 fenced members window-free but the device
no longer kept ONE manager (review round 3 N1). The chosen design keeps WERO: the device arbitrates
the manager; a member's death preempt on nvmet is detected as ignored (`.failed`, counted) — its host
is not fenced (the tail scan and the frame screen) and no window opens. Draft upstream report:
`~/tmp/nvmet-pr-preempt-bug.md` (not sent).

## The model

`src/meta/guard/host_fence_model.zig` — 356 states hold: one manager, no fenced write, never
unreserved under a manager. Weakenings fail: rtype 5 as held (two managers), no rule / term-less
retirement (a fenced write or two managers), the B3 departure and the release-cycle death preempt
(unreserved under a manager).

## Checks (7.3.1-1-omarchy-mainline, `nvme_core.multipath=N`)

See the final report for the counts at the final head (nvmet tier ×3, the 27 eviction/re-join/KILL
rows ×3, the unit filters, fmt, rails). Not run: the full gate (the orchestrator's); anything under
`nvme_core.multipath=Y`. Limits: §DF.13. Owed: a fence-triggered retire-and-rejoin of a
non-evicted sibling (§DF.8); the data namespaces' registrants (`data_pr`, owed since rung 6).
