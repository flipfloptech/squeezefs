# Rung 10 — the custody follow-ups: the grant lands the holder's custody, the death replay is judged, format sweeps the held copy

Four items were left by the supervisor-custody landing (`rung-10-supervisor-custody.md` "Seen, not fixed";
decision 0003 §BR.10). This note covers three fixes and one owed item (§BR.11).

**What changed.**
- *(a) The cache-less coherence gap.* A grant reply carries the holder's records, and the reader then reads
  the device. The prepare seam only promoted write-cache residents (`d0fcaf8`), which does nothing when there
  is no staging ring. So a cache-less holder's grant carried the DURABLE size. Its parked custody and its size
  claim (§AL.5) landed only with the write-back timer. The fix is ONE hook, `KvFs.landForGrant`, which landed
  with the foreign-append fix (`a81c2b7`, the same law): the prepare seam runs over the filesystem, the recall
  first, then the ino's custody lands and its parked refinements journal behind the data barrier. It is counted
  `fuse_grant_landings` / `fuse_grant_landing_drain_failures`. This branch carried the same hook; at the merge it
  kept the landed one and added this posture's row. An ino with neither pays two spin-locked probes.
- *(b) format.* The metadata preflight keeps each member's prior identity. Once that member is overwritten,
  `removeHeld` removes the copy whose header names that volume, and the format logs the removal. A copy whose
  header names another identity is left in place.
- *(c) The death replay.* `replayFd` on a fixed, unsegmented ring is now `judgedReplay`, the same law a held
  copy follows. If the device does not name the tail's volume, the replay answers `DeviceForeign` and the tail
  is held. Windows past a hole are counted and logged as lost. A joined ring is still written whole (§BR.5).
- *(d) Owed: cache-less packing.* A cache-less mount gives each small file a whole block. Its landing cannot
  flip a striped file to packed. A striped writer holds no (3.7), and a within-EOF writer drops the level-1 guard
  before its data phase. So a flip that lands between a writer's kind read and its record leaves that write's
  buffer under a packed layout, and the merge discards it as a dead generation (`mergeLocked`). The reason is
  stated on the packer's line in `plane.zig`. The arm needs a writer/flip exclusion on the ino
  (register-then-check, model-checked).

**Red (origin/dev `0661074`, the rows only).**

| row | red |
|---|---|
| smoke: A JOINED PEER READS THE HOLDER'S UNLANDED WRITES | B: `stat` 0, `cmp: EOF on 'd/a' which is empty` |
| unit: the death replay is JUDGED | the device's scan after the replay: `expected 0, found 1` (dropped) |
| smoke: FORMAT REMOVES THE HELD JOURNAL TAIL … | `held copy LEFT … the format said so: false` |
| smoke (not landed): 1,000 × 64 KiB on a cache-less 2 × 2 GiB set | `sync: error syncing 'p/f0': No space left on device` |

**Numbers (branch `fix/custody-followups`, on `23db6c2`).**

| run | result |
|---|---|
| the (a) and (b) rows, 20 passes beside the unit loop and other agents' builds and suites (load 9–14), with this branch's own hook | 20/20 each; every pass: grants served after their custody landed A 3, B 1, refusals 0 |
| the (c) unit row with `replayFd` and the format sweep's unit row, 20 passes | 20/20 |
| held-tail (both arms), custody, unsupervised, O_TRUNC ×2, the write cache's recall, invalidation coherence, the two-daemon matrix, the replay proof, the shared hot directory, the joiner's rejoin, the handover's stamps, the write-back timer, the ordered law, the cache-less data plane, a join after the manager's kill, the kill matrix | 59/59 (an earlier pass on base `029e8e9`: 50/50) |
| unit filters `tokens` / `recall` / `supervisor` / `journal_tail` / `pack` / `format` | 97 pass 4 skip / 69 pass 9 skip / 59 pass 12 skip / 50 pass / 100 pass 9 skip / 91 pass 24 skip; 0 fail |
| `zig build rails`, `zig fmt --check` | 18/18 steps, 32/32 tests; clean |

**Gate 127 (two rows red on the branch).**
- *The striped eviction row* (`A LIVE MEMBER EVICTED AT T_OWNER UNDER THE DEAD MANAGER'S STRIPED HOT DIRECTORY`)
  wrote its custody in the ROOT, A's directory. A witnesses a child B creates there at B, and that grant landed
  B's 64 KiB first: a diagnostic named the ino (claim 65536, custody held, landed at the grant), and B re-joined in
  place holding nothing in 2 runs of 3. The re-join is lawful (§BS.3). The row's premise was stale, so the custody
  moved into `own/`, a directory B holds and no peer asks about, as in the negative re-join row
  (`fix/eviction-row-premise`, `4aabb2d`).
- *The 753 row* never met a SIGKILL. The `wait status 0x9` line in the gate log is the fsck row's deliberate kill
  (pid 3120008), and `still serves it` is the negative re-join row's poisoned B. The row read the supervisor's
  `exited (wait status 0x…); done` off the log snapshot taken at the daemon's death. The supervisor writes that line
  after its replay of the tail, and the judged replay read the superblock, the ledger and the whole ring even with
  no window held. Now a tail holding no window reads nothing (red: an empty tail's replay against a blank device
  answered `Foreign`), and the row waits for the line.

| run | result |
|---|---|
| both rows, 20 passes side by side on the branch | 20/20 each: B poisoned every pass; all 8 cycles every pass |
| `4aabb2d` on `9b081ae`, and on `fix/foreign-append-bytes` (its own grant hook), the striped row with the negative re-join row, 10 passes each | 10/10 each: B poisoned, the negative row's custody fail-stop, no re-join |

**Seen, not fixed.** The death replay of a JOINED ring is still unjudged. Its chain needs the appender page's
`ledger_tail_seq` (the newest of four slots, two of them in the appender directory). That is the re-join owner's
domain (§BR.5). The judged replay rescans the ring once per held window, the same cost a held copy pays at the
next mount.
