# Rung 10 — the gate blockers: the release window, the ladder's evidence order, rows that asserted a schedule (2026-10-14)

The release gate's acceptance (charter §11) is three consecutive clean runs on one binary. After
§AG (`rung-10-coherence-defects.md`) these rows could still redden a gate under load: the online
fsck's `[C2] leaked` window item 3 there left owed, the write pipeline's admission row and the
conveyor's §L window-fill row (macOS at load 18–20), and the cache-path data smoke's promotion
count (twice on the venue). Decision 0003 §AH; 0002 row 532. Loads below: "14 × `yes`" is this
agent's own `yes` processes on the Mac beside whatever the other agents ran (the box read
load 60–160 for most of the loops); "gate-shaped" is the five macOS unit shards of the full
binary running in parallel beside the loop.

## 1. The online fsck's `[C2] leaked` window

**Baseline.** Pristine `dev` (`2d53547`), the storm row alone, macOS under 14 × `yes`: **1/150**
`[C2] leaked`. On the venue under 4 × `yes`: **14/150** — 6 `[C2] leaked` (blocks 0 and 4), 7
`storm.ops > 20` and 1 `total_suspects > 0` (the row's own schedule assumptions, below).

**Three windows, not one.**

1. *The release window* (the one §AG.3 named). A release's tx — the record, the layout and the
   `−ref` gone in one commit — lands, and the block's terminal free runs after it. Between the
   two the allocator tracks a block no durable reference and no record names: the departed owner
   cannot be locked, so a census and its re-check inside the window confirm a leak the free
   resolves a moment later. The same window is C3's for a shared block.
2. *The ladder's evidence order.* Phase A and Phase C ran interleaved per suspect, and the
   fresh census Phase C reads is memoized once per pass (§AC.6). A census read for an earlier
   suspect could predate the publish that ended a later suspect's mint (registered, so exempt at
   the census — but its Phase A ran after the publish and its guard drop): the stale census
   confirmed the mint `[C2] leaked`. And the C2-leaked re-check read the allocator BEFORE its
   derivation, so a leaked block freed and minted to a new writer between the derivation and the
   verdict confirmed the new writer's block.
3. *C6's double count* — exposed by the release law itself: a release held past its free (the
   hold drops after `freeBlock` returns, the block already free-listed) was subtracted once as
   free and again as an untracked in-flight block: `[C6] … 1 set-but-unreferenced (leak)`,
   **11/150** on the venue once the storm held its releases. (A trim claim's guard sits on a
   `trimClaimed` block the same way.)

**The law (0003 §AH.1).** A release is a live owner of its block from before its releasing
commit until its free: every product site that commits a reference release holds the released
blocks in the in-flight registry (`holdInflight / dropInflight`, the list-shaped twin of
`InflightAllocGuard` on the same per-cell count) from before the commit until after the free,
and drops them on every path that leaves without the free — a release that never freed is then a
leak the census sees, never a shield. The sites: the handler's `AfterCommit` flows (corpse
destroy, SETATTR truncate, O_TRUNC, the staged write's displaced copy, the promotion to an own
block — `holdAfter` before the commit, `dropAfter` deferred), the plane's merges (inline, the
crossing, the tree-mapped window — `MergeOutcome.holds`, dropped by the write path's four callers
after their frees), the extend barrier and the sweep chunks, the packed promotion's displaced
copy, the mover's republished source, the fsck repair's destroy. The epoch's save now frees its
parked keys BEFORE popping its B guards (an intermediate B it superseded is an unpublished mint
until its free), and the own block a promotion mints stays registered until its publish lands
(`ownBlockPublished` / `abandonOwnBlock`) — the "staging own-block arm". The reclaim's elided
debt and its queue need nothing: `beginFree` drops the count before either, and a queued block
carries the queue's own registration. Two must-stay-0 tripwires keep the pairing: a drop with
none held (`block_inflight_unheld_drops`, refused — the count never wraps) and a released binding
reaching `DataPlane.freeBindings` unheld (`release_free_unheld`). Offline nothing is in flight:
the offline arm is untouched and an offline leak stays a finding.

**The ladder (0003 §AH.2, §AH.3).** Every Phase A runs before the first Phase C; C2-leaked and C3
confirm only while, re-read AFTER the fresh derivation, the block is still tracked (at the judged
count for C3), no live owner holds it in flight and no mint under this scan replaced it — registry
absence first, then the references, then the tracking. C6's in-flight term skips a free-listed
and a trim-windowed block.

**Red rows.**

| Row | Red on | Red reading |
|---|---|---|
| `fsck` "the ladder reads its fresh evidence after EVERY suspect's registry check …" (a mint whose publish lands at its own Phase-A check through `Host.recheck_hook`) | the interleaved ladder | `FALSE POSITIVE: [C2] … block 55 — leaked` |
| `fsck` "the C2-leaked re-check reads the allocator AFTER its fresh derivation …" (free and re-mint at the derivation hook) | the read-before-derive re-check | `FALSE POSITIVE: [C2] … block 50 — leaked` |
| `fsck` "a release is a live owner of its block …" (an online run inside a destroy's commit→free window: unheld, the gate's line; held, clean; freed with the hold not yet dropped, clean; a hold dropped without the free, a genuine leak online and offline) | C6 before §AH.3 | `[C6] … 0 referenced-but-clear (LOSS), 1 set-but-unreferenced (leak)` |
| `kv_fs` "a release is a live owner of its block …" (`SettlingData` records every hold with the commit count at its hold; the destroy, a truncate, an O_TRUNC, a promotion) | the handler before §AH.1 | 3 of 4 frees unheld (`expected 0, found 3`); the own block never settled |
| `write_epoch` "the save's frees are live owners' …", the save-swap row, the overlay's live-mapped overwrite row (the RAM map holds displaced bindings like the product merge; the frees ledger counts an unheld free) | the write path before §AH.1 | an intermediate B freed unheld (`expected 0, found 1`); the A keys held for ever |
| `block_alloc` "a release's in-flight registrations …" | — (the primitive's contract) | a drop with none held refused and counted |

**The storm row's own assumptions.** It asserted `storm.ops > 20` and `total_suspects > 0` after
ten runs; on the loaded venue the ten runs finished before the storm's threads were scheduled
(pristine `dev`: 8 of its 14 failures). The row now judges runs that overlapped the storm's
progress (an op completed during the run) and runs until its shields engaged: ten overlapping
runs, then the first exemption or clear — waiting for the observable, never a schedule.

**Loops (the storm row alone).** macOS, 14 × `yes` beside load 12–160: **300/300** on the ladder
fix and the storm's release law, **250/250** after the C6 fix and the row's restatement, **220/220**
on the landed tree (rebased over the noopen and symdefects landings). Venue, 4 × `yes` beside the
other agents' load 9–14: **150/150** — where pristine `dev` read 14/150 and the branch before §AH.3
read 23/150 (11 `[C6]`, the rest the row's schedule assumptions). 0 false positives.

## 2. Three rows that failed under load

Neither the admission row nor the window-fill row failed again here: 0 of 800 admission runs
and 0 of 340 window-fill runs on the original rows — macOS at load 60–160 with up to four loops
in parallel, two gate-shaped runs of the full unit binary — and the coherence agent's log of the
one failing gate (`10:48`, load 18–20) cut the assertion line. Both rows assert counts the
product does not guarantee; each is restated on what it does.

**The write pipeline's admission row** — a test assumption. `admission_waits == +1` held only if
the completer ran after the admitter's re-check and within the 5 ms backstop tick: a completion
inside the park window (registered, re-check not yet run) admits with 0 parks counted, a
completer slower than the tick adds a ticked re-park, and `quiesce` carried a 5 s wall bound.
Restated: the park window is held exactly (`Governor.test_park_window` — the completion lands
there, the re-check admits, nothing counted: a lost wake would sleep to the tick); the parked case
completes once the admitter counted its park and judges `ticks ≤ waits ≤ 1 + ticks` (every park
ends in the one completion's wake or a tick, none spurious) and that the admission returned only
after the completion began (acq_rel with the release its CAS reads); `quiesce` waits for the
drain unbounded. Loops: **520/520** under load (four parallel loops).

**The conveyor's §L window-fill row** — a test assumption. The row judged one run whose first
160 eligible windows were the worth gate's learning, and WHEN the gate opens is the schedule's: a
probe burst whose ten holds meet a stalled host collects too little and keeps the gate off for
another period, failing the pad, holds and not-worth bounds together. Restated: rounds until the
gate opened (the rendezvous: a hold the gate paid itself), then the 96 measured rounds; the
gate's learning law is the worth-gate tie row's. The steady slack is the row's own 12 pp
restricted to the windows now judged (12 × (2K + P − 1)/2K = 21.9 pp — neither loosened nor
tightened), the windows bound drops the probe term, and the gate may close once and re-open
(< one period of declines). Measured: steady pad 59–62 % against the 54.8 % pair bound, 213–232
windows against 248. Loops: **120/120** under load with the final slack (and 40/40 before it,
with a 12 pp steady slack — stricter than the row ever was).

**The cache-path data smoke's promotion count** — a test assumption, and behind it a second one.
A staged file leaves the ring by its next write past the block (the filesystem's promotion,
`fuse_write_promotions_to_striped`) OR by the plane's promotion dispatch at ring pressure or fsync
(`layout_promoted_block`), whichever runs first; the 8-way storm's pressure promotes residents
before their writers' next write. The noopen agent's failing gate printed 11 own-block DMAs with
fewer than 10 counted by the write path; this agent's gates read 6 + 5 and 3 + 8 (write path +
dispatch). Restated: the sum of both arms ≥ 1 + 1 + storm_files, both printed. Once the count held,
the same schedule failed the row's next step: the dispatch also packs small residents at that
pressure, so after the big files' `rm` df carried their pack blocks (`8192 KiB used vs 0`), and the
pack counters do not name the live ones (41 opened, 4 blocks held). On the staging set the rm is now
judged by what the product guarantees: every removed file destroyed (`fuse_reclaim_destroys` to the
eleven — a file is a corpse, its blocks referenced, until its kernel FORGET), the reclaim drained,
`fsck --online` 0 findings (a big file's block left tracked is `[C2] leaked`, a count off C3/C6 —
the §AH census is exact here), and df within the pack census's bound; the set without staging keeps
the exact df. The LOOPED repro (`SQUEEZEFS_TEST_SMOKE_STAGE_LOOPS`) writes fresh storm files each
pass (the previous pass's removed — the storm's destroys and their release holds ride the loop) and
judges the promotion identity per pass. Loops on the venue under 8 × `yes`: **114** storm passes
(one run of 39 before the daemon log reached the smoke's 16 MiB read cap — a harness bound, not a
finding — then 3 × 25), every pass 8 + 0, every must-stay-0 gauge 0 (the two §AH.1 tripwires among
them); the full row in the gates below, among them both the 6 + 5 / 3 + 8 schedules.

## 3. Gates

The landed tree (`2b2409c`), each run from a clean cache: macOS with the worktree's `.zig-cache`
removed; the venue uncached (`src/core/clock.zig` touched from the host), through `scripts/venue`'s
gate lock (`40bf775`), no background load (the orchestrator withdrew the 4 × `yes` after concurrent
gates livelocked the VM — below).

| Run | Wall | Steps | Tests | Verdict |
|---|---|---|---|---|
| venue F1 | 761 s | 45/49 | 2,897 / 2,976 (78 skipped) | the interception host row (below) |
| venue F2 | 1,040 s (a lock wait included) | 49/49 | 2,899 / 2,978 (79 skipped) | green |
| venue F3 | 693 s | 49/49 | 2,899 / 2,978 (79 skipped) | green |
| venue F4 | 379 s | 49/49 | 2,899 / 2,978 (79 skipped) | green |
| macOS 1 | 253 s | 29/29 | 2,279 / 2,308 (29 skipped) | green |
| macOS 2 | 218 s | 25/29 | 2,278 / 2,308 | the jobs fabric's duty-cycle row (below) |
| macOS 3 | 216 s | 29/29 | 2,279 / 2,308 (29 skipped) | green |
| macOS 4 | 226 s | 25/29 | 2,278 / 2,308 | the duty-cycle row |
| macOS 5 | 203 s | 29/29 | 2,279 / 2,308 (29 skipped) | green |
| macOS 6 | 198 s | 29/29 | 2,279 / 2,308 (29 skipped) | green |
| macOS 7 | 197 s | 29/29 | 2,279 / 2,308 (29 skipped) | green |

Three consecutive green on each venue: venue F2–F4, macOS 5–7. In F1–F4 the cache-path row read
11 + 0 promotions (write path + dispatch), its census 0 findings and 0 KiB above the baseline.

**Failures in rows this work did not touch** (each attributed off this branch):

- `ops.jobs.fabric` "duty cycle adheres within ten percent at 25/50/75 over a noop run"
  (`fabric.zig:1285`, `duty + 10 >= pct`: a wall-clock duty measured under host load, the sleep
  side waking late): pristine `dev` fails it 1/40 alone at the same load, this branch 1/40. Owed:
  the row judged on what the throttle guarantees, not the wall clock.
- `ipc.host.e2e_linux` "THE INTERCEPTION HOST" (`e2e_linux.zig:533`: the kernel `pread` of the
  eight no-wait ticket pages read 0 of 32,768 bytes — the kernel's size, not the data plane's): the
  row failed in another agent's gate on 2026-10-13 (`/build/tmp/umount-gate.log`), before this
  work.
- On the earlier trees of this branch (each re-gated after a rebase or a smoke restatement):
  "THE ONLINE VOLUME ROW ACROSS DAEMONS" (`cluster_wire_service_refusals` = 1 on A — the same
  row and tripwire failed the noopen agent's gate 6 at 16:40, on the tree that is now `dev`);
  "THE `-o ro` READER OF A LIVE SET" (C's daemon not exiting 32 s after its umount signal;
  the row failed the noopen agent's gates 3 and 6 and its cost runs 2 and 3); "A CLOSE BURST
  COALESCES ITS DRAINS" and the unit row "no-open close bursts" (the noopen agent's own runs
  `noopen-costs2`, `noopen-final2`–`4`); the nvmet fidelity rows right after the venue's restart
  (`share … failed part-way (FileNotFound)`, green in every later gate); the unit compile
  OOM-killed beside two other agents' builds; the duty-cycle row once more (the macOS gate before
  the rebase).
- The cache-path row's own two follow-ups were found by these gates: the dispatch's packs in the
  df (`8192 KiB used vs 0` with 6 + 5 promotions), then a census run before the rm'd files'
  destroys (181 blocks in use, 0 findings) — both restated in §2, neither a product defect.

**On the landed tree** (`352a16f`: the above rebased over the second wave of gate-blocker fixes,
`0003 §AI`). macOS: 183 s, 208 s, 200 s — 29/29, 2,282 / 2,311 (29 skipped), three consecutive
green. Venue: F1 401 s red on "CUSTODY NO CLOSE COVERS IS DRAINED ONCE IDLE" (the zero-message
opens' §AF row, `smoke.zig:7263`: `parked_buffers` 0 read in the same stats tail as the idle drain's
count — not a path this work changed, whose write-path edit drops the merge's holds after the
frees), F2 384 s and F3 377 s green (49/49, 2,904 / 2,983, 79 skipped), F4 541 s red on "THE SHARED
HOT DIRECTORY ACROSS DAEMONS" (`expected 0, found 11` open intents after B's kill with
`xv_cross_owner_intent_stuck` — the signature `rung-10-symmetric-defects.md` item 2 recorded on its
tree). Three consecutive venue greens stand on `2b2409c` (F2–F4 above), not yet on the landed tree.

**The venue livelock.** Concurrent full gates (this agent's under 4 × `yes`, and others')
over-committed the VM's 16 GiB until not even `exec echo` started; the orchestrator stopped
`sqz-zig` from the host (venue gate 7 on `dc130f2` died with it) and serialized full gates
(`40bf775`). Every run in the table above went through the lock.

## Owed

- The jobs fabric's duty-cycle row and the interception host's kernel-size read (above).
- The mover's pin and its unpublished destination are live owners the registry does not see: an
  online fsck concurrent with a move job reads C3 (`rc = owners + pin`) for the move's duration.
  §AH.1 holds the republished source's release only.
- A mint's gap between `allocateBlock` returning and its registration: a mint made before the
  scan latched, its thread descheduled in that gap through the whole pass, reads as leaked
  (two adjacent statements; not met in 920 storm runs).
