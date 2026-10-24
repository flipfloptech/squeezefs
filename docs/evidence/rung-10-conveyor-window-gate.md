# Rung 10 — the window gate: a pass never meets a full lane (2026-10-24)

**The red.** The full smoke ran beside the row 4-wide (the staged-lost agent's loop,
`keep-load2-two-daemon-tripwire.log`). THE TWO-DAEMON MATRIX failed on daemon **B, the joiner**, during `tar -x` into
A's directory: `INVARIANT TRIPWIRE 'meta_conveyor_pass_panics' … (ConveyorFull, 1 members)`, and tar's chmod of
`tree/t45` returned EAGAIN. The window's handoff (`laneSubmit` → `submitWriteBatchDetached`) found no free lane
cell. `LaneQueueFull` was mapped to `ConveyorFull`, `abandonWindow` ran, and the tripwire fired.

**The interleaving.** At spawn, the lane's cells (`inboundCap(pages)`) and the conveyor's in-flight map were sized as
one per ring page. The reasoning was that "on a padded ring every window occupies a page, so the ring's admission
bounds the windows". B joins with the 512 KiB floor ring: 126 pages, 128 cells. Its 2,000-file storm stalls ring
admission, and `growRingIfDue` grows the ring. A diagnostic print in the row (not landed) showed B's ring grew
**3–4×** in every one of 20 loaded runs. The cells stay at 128. A ring grown to 4 MiB or more admits about
900 page-sized windows in its user slice. With acks at the apply (§BB.1), each of tar's ops returns before its window lands, so
single-op windows pile up in the lane whenever landings slow down under load. Under the load the flight reached
**58–63** windows. The pass panic came at the 129th. 948a324's note ("a throughput cap, not a correctness matter")
was wrong. The local venue's `/build/tmp` is ext4 on zram (4096 B logical blocks): the floor ring alone holds ≤ 61
user windows, so on this venue the growth is the path. Two more paths reach the same panic:

- a sub-page grain (the buffered posture's 1, a 512 B device) packs several windows into a page, even on the
  fixed ring (the backend red below);
- barriers and structural entries drew from the same pool. An fsync storm behind an un-landed window took every
  cell (the lane red below).

**The law (0003 §BN).** The lane splits its cells. `windowCells = inboundCap / 2` serve windows from a free list of
their own; barriers and structural entries take the other half. The pass is the only tracker of windows. It
reserves a window only while fewer than `windowCap = window_cells − 1` are in flight (`awaitWindowCell`, A1¼). The
−1 is there because one untracked window may still hold its cell: the lane runs the terminal hook (which untracks)
before `freeCell`. A full flight parks the pass holding nothing: it registers on `landed`, re-reads the count, and
waits. The ring's grain, geometry and growth no longer bound anything in the window path. The memory cost is zero
(same cell count). The ungrown floor ring at a 4 KiB grain admits ≤ 61 user windows, under the share's 63.

| red test (before → after) | before | after |
|---|---|---|
| conveyor "THE WINDOW GATE" (RamLane with 4 window cells, held) | 5th window: `pass_panics` (ConveyorFull, 1 members) | parks once before its reservation; resumes at the first landing |
| backend "THE WINDOW GATE on the device lane" (BUFFERED, landing-hold seam, 257 kernel creates) | pass panic | 127 in flight, 1 gate park, all 257 land |
| journal_lane "the windows' cells are their own" | barriers took 255 of 256 cells; the windows refused | 128 barriers; 127 windows beside them |
| model `conveyor_window_gate_never_meets_a_full_lane` (378 states, depth 34) | weakenings: gate at `W`, no gate, barrier on a window cell → `WindowMetAFullLane`; check-before-register, notify-before-untrack → `GateOutlivedTheLanding` | holds |

**Numbers (local venue, kernel 7.3.0-1.7-mainline-sqz).**

- The three red tests: 100/100 twice, the second run beside the full-smoke load.
- THE TWO-DAEMON MATRIX: **20/20** 4-wide beside the full smoke (this branch). Gate parks occurred in 2 runs
  (hwm 63); pass panics 0.
- SHARED HOT DIRECTORY ACROSS DAEMONS, the MutualTls matrix, VIRTUAL INODES, and ONLINE FSCK: 12/12 runs 4-wide
  under the same load.
- The full smoke as load: 10 iterations, 9 green; 1 red in the read-cache reuse row, not the conveyor (below).
- Unit filters `conveyor`, `admission`, `heap`, `joined`, `journal_lane`: 213/217 (4 skipped). `zig build rails`
  is green, and so is `zig fmt --check`.
- mdtest A/B owed: the host was never quiet (load 7–23, other agents' gates). The hot-path cost is one relaxed
  load and one compare per pass.

**Seen, not fixed.**

- (1) `SQUEEZEFS_META_PASS_VENUE=committer` with `SQUEEZEFS_META_APPLY=self` (two non-shipped levers): a
  committer-run pass that parks cannot take its own member's turn, and every later window's verdict waits behind
  that member's window (the completed-prefix law). Today the park is at ring admission, which escalates after
  `PARK_ESCALATION_NS`. Now it is also at the gate, after 63 windows. The fix belongs to the levers' owner: either a
  committer-run pass applies its own turn in place, or the startup gate refuses the pair.
- (2) A barrier whose request share is exhausted still answers fsync `EAGAIN` (`laneBarrier` → `ConveyorFull`).
  That is pre-existing, now reachable only past 64 concurrent fsyncs on one floor-ring lane.
- (3) `queueCapacity(pages)` (the committer queue) still says the ring bounds it. It does not: committers queue
  before ring admission. A 129th simultaneous committer gets `ConveyorFull` (EAGAIN), not a tripwire.
- (4) The load's one red: "A MEMBER'S CACHED COPY … NEVER OUTLIVES THE BLOCKS' RE-USE" (`keys >= keys0 + 2`:
  purged keys 4 → 4). This is the read cache (dev `e2e3c94`'s area), not this change.
