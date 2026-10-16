# Agent memory snapshot (2026-10-16)

The agent's working notes for this repo, copied from the Grok workspace memory on the Mac
(`~/.grok/memory-v2/workspaces/squeezefs-zig-86202243/topics/`) so that they travel with the
repository to the Linux workstation without touching anyone's `~/.grok`. They are the
agent's own notes: what was decided and why, how the venues behave, what was learned the
hard way. `docs/charter.md`, `docs/decisions/` and `docs/evidence/` remain the authority;
where a note and the repo disagree, the repo wins.

| Note | What it holds |
|------|---------------|
| `progress-ledger.md` | Delivery order, every rung and fix agent with its commits, the release-gate state, the performance levers and their measured gains, the post-release list |
| `venue-and-workflow.md` | Venues (Mac VM, kamrui over ssh), locks and build slots, the gate train, landing tiers, disk/cache pruning, owner rules |
| `squeezefs-charter-and-conventions.md` | Charter and code conventions |
| `squeezefs-decisions-and-format.md` | Design decisions and the on-disk format |
| `squeezefs-data-plane.md` | Data plane, format limits, lock order |
| `squeezefs-executor-and-fuse.md` | Executor and FUSE library (rung 3) |
| `squeezefs-client-bypass.md` | Client bypass (rung 7): shm session, IPC host, LD_PRELOAD shim, SDK |
| `squeezefs-performance.md` | Performance findings |
| `sqzhash128.md` | The sqzhash128 hash and its adoption |
| `zig-0-17.md` | Zig 0.17 API and build-system facts |

## Mac-era details

Written while the controller was a Mac. Paths like `/Users/joberdorf/Source/squeezefs-zig`,
"the VM" (`sqz-zig`, the macOS `container` venue, aarch64) and "kamrui" (an x86_64 box
over ssh) describe that setup. On the workstation the repo path differs and the venue is
`SQZ_VENUE=local` (see `AGENTS.md`); task and subagent ids in the ledger are from Mac
sessions and mean nothing elsewhere.

## Recent, not yet folded into the notes

- kamrui's clocksource is HPET: the firmware's TSC calibration (refined 2,111.56 MHz) disagrees
  0.74 % with the HPET watchdog (~2,096 MHz, which chrony confirms within ~83 ppm), so the
  kernel drops the TSC — the stock Ubuntu kernel too. HPET reads cost ~1 µs vs ~20 ns, so
  kamrui's absolute numbers look worse than SqueezeFS is; A/B comparisons on the box stay
  valid. Forcing the TSC (`tsc=nowatchdog`/`reliable`) would run the clock 0.74 % fast;
  decided: keep HPET, re-measure after the clock-read cuts (landed `c689c54`), try the
  parameter only with chrony tracking checked if HPET still dominates.
- The move: development pauses after the running agents land, then moves to the owner's
  Linux workstation, where Grok runs directly and the workstation is the local venue
  (gates, perf and kernel builds native). aarch64 coverage then needs a second venue (the
  Mac VM or an arm64 box over ssh).

## Using these notes on another machine

- Nothing to do: `AGENTS.md` points here, so an agent reads the matching note before working
  in an area.
- To make them Grok workspace memory as well: start Grok once in the clone with memory on;
  Grok scopes workspace memory per repository (clones share a scope), and if `/memory` shows
  no workspace topics, copy these files (not this README) into
  `~/.grok/memory-v2/workspaces/<the squeezefs-zig scope>/topics/`. Only that folder is
  touched — global memory and other projects' memory stay as they are.
- Keep this snapshot current by hand when a note changes materially; it is a copy, not a
  live link.
