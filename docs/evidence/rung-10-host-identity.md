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
| associations | users counted in `<run>/hosts/fabric-<key>/<pid>` under one flock; reuse-or-connect + entry as one step; the last user deletes the controllers |
| key | `host_fence.hostKey(hostid, set_uuid)`; a kill -9 remount adopts the standing registration |
| symmetric PR | the manager holds WERO; a joiner adopts (host registered under the key) or registers before its JoinAppender; its membership join carries the key, so a death preempt fences its host |
| the rule | `guard/host_fence.zig`: before a register that admits the host afresh, every live entry of `<run>/hosts/set-<key>/` is judged against the volume's pages — current / retired / joining spared, stale proven dead (`proc.isWriterIncarnation`) or SIGKILLed through a pidfd; else the registration is refused by name |
| nvmet quirk | nvmet 7.3's `nvmet_pr_preempt` lets the holder's preempt only re-type its hold: a victim still registered is preempted with the reservation released, then the hold re-acquired (`appender_recovery_holder_preempt_cycles`) |
| death screen | a key a live member shares is the dead member's host's — accepted; the departed-key table deleted |
| venue | `provision-local-venue` checks multipath=N live and persisted; `--apply`/`provision-ssh-venue` write the Limine/GRUB drop-in when absent |

## The reds (`src/ops/nvmet/fidelity.zig`, committed alone)

- **co-located mounts** — red: `expected 3, found 1` (the manager held WE); and a sibling's
  umount deleted the shared data controller. Green: one controller, regctl 1 under WERO,
  A registered (`pr_host_registrations 1`), B adopted (`pr_host_adoptions 1`, same `pr_key`),
  B's umount leaves both standing, A's releases both (no controller, regctl 0).
- **stale sibling** — red: B (another host's manager M under WE) died at its first barrier,
  `ReservationConflict`. Green: B registers, A adopts (regctl 2, rtype 3); B SIGSTOPped, evicted,
  M's recovery preempts the host key (`appender_recoveries 1`, `appender_recovery_preempts 1`,
  regctl 1); A's fsync in its own directory refused; a mount under
  `SQUEEZEFS_TEST_SIBLING_KILL_REFUSED` exits 1 naming B's pid, B alive, regctl 1; A2 kills B
  (`pr_stale_sibling_kills 1`, B dead), registers, its writes land and M reads them.
  First green run found two defects: a create in the root is the manager's slot's (the leg now
  writes in the members' own directories); nvmet's holder-preempt quirk (above).

## Checks (7.3.1-1-omarchy-mainline, `nvme_core.multipath=N`)

- nvmet fidelity tier ×3: 56 pass, 1 skip (multipath-tcp: no head under multipath=N), 0 fail each.
- Units (`host_fence`, `meta.guard`, `membership.`, `death_ledger`, `wire.screen`, `nvmet`,
  `core.knobs`, `recovery`, `wire_plane`): 271 pass, 24 skip, 0 fail.
- Smoke, THE KILL ROWS and every eviction/re-join row (27 rows, 15 filters) ×3: 71/71 each.
- `zig fmt --check`, `zig build rails` (32/32): clean.

Not run: the full gate (the orchestrator's); multipath-tcp and any co-location row under
`nvme_core.multipath=Y` (the venue boots N). Owed: a fence-triggered retire-and-rejoin of a
non-evicted sibling (§DF.8); the data namespaces' registrants (`data_pr`, owed since rung 6).
