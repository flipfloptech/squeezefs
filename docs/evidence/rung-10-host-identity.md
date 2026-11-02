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
| key | `hostKey(wire host id, set)` (FID 0x81, not the configured text); the V2 writer claim names it |
| symmetric PR | the manager holds **Write Exclusive – All Registrants (rtype 5)**; a joiner adopts (host registered) or registers before its JoinAppender; its membership join carries the key |
| the rule | `guard/host_fence.zig`: a register only when the host is unregistered; before it, stale siblings (a page of theirs Recovering/Recovered/Free their own `r <writer> <appender>` does not cover, or a superseded manager) proven dead or pidfd-killed and their writes drained; after it, judged again under the lock (a sibling turned stale withdraws the registration, then dies) |
| recovery | the report re-read once the page is Recovering: a host registered meanwhile is preempted again |
| death screen | the departed-key table kept; a key held on another node contradicts, the dead member's own node's hold does not |

## The reds

- **co-located mounts** — red `expected 3, found 1`; and a sibling's umount deleted the shared
  data controller. Green: one controller, regctl 1 under rtype 5, A registered, B adopted (same
  key), B's umount leaves both, A's releases both (regctl 0).
- **stale sibling** — red: B died at its first barrier (`ReservationConflict` under M's WE).
  Green: B registers, A adopts (regctl 2, rtype 5); B SIGSTOPped, evicted, M's preempt removes the
  host (regctl 1); A's fsync in its own directory refused; a mount under
  `SQUEEZEFS_TEST_SIBLING_KILL_REFUSED` exits 1 naming B; A2 kills B (`pr_stale_sibling_kills 1`),
  registers, writes, M reads.
- **review B1/B2/B3** (units, `98822ec`, 4 red): a successor incarnation's Recovering page spared
  by its predecessor's retirement; a sibling turned stale between the rule and the register left
  alive; recovery never re-reading the report; a departure unregistering the holder's key.

## nvmet 7.3's holder preempt

`nvmet_pr_preempt` answers `holder == reg` before it compares PRKEY: a WERO holder's preempt of a
member's key succeeds and fences nothing. The fidelity leg `holder_preempt`, on this venue: under
WERO the victim stays registered; under rtype 5 the victim's registration goes, the hold stays
(rtype 5, regctl 1) and its write is refused (-EBADE). The first wiring's release → preempt →
re-acquire window is gone. Draft upstream report: `~/tmp/nvmet-pr-preempt-bug.md` (not sent).

## Checks (7.3.1-1-omarchy-mainline, `nvme_core.multipath=N`)

See the final report for the counts at the final head (nvmet tier ×3, the 27 eviction/re-join/KILL
rows ×3, the unit filters, fmt, rails). Not run: the full gate (the orchestrator's); anything under
`nvme_core.multipath=Y`. Limits: §DF.13. Owed: a fence-triggered retire-and-rejoin of a
non-evicted sibling (§DF.8); the data namespaces' registrants (`data_pr`, owed since rung 6).
