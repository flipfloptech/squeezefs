# Rung 10 — a live member evicted re-joins in place when it holds nothing un-landed (2026-10-24)

The owner's ruling (b): a joined mount its home shard evicted at `T_owner` (a partition, a frozen
daemon) re-joins IN PLACE — a fresh join under the same live FUSE mount — when it holds no un-landed
state; otherwise it fail-stops as before. Charter §5.5 (amended), decision 0003 §BS, 0002 row 558.
Venue: this workstation, `7.3.0-1.7-mainline-sqz`; branch `feat/rejoin-in-place` off dev `885727b`.

## The law

**Eligibility — "nothing un-landed", judged on the RETIRED incarnation** (`WirePlane.judgeUnlanded`,
`RejoinHost.unlanded`): every window it admitted landed and no reservation is open; its ring's landed
head equals the head the successor's recovery read (its own page `Recovered{ledger_tail_seq}` off the
device, polled within `T_park_max`); no cross-owner intent open; no data block in the writer's in-flight
registry; no staged write-cache resident; no write-side custody (parked block, open overlay, open or
saving rewrite epoch); no packed tenant between reserve and commit; no parked time refinement. Any
word set ⇒ the park's expiry, the poison, EIO until remount; the refusal names the word.

**The sequence** (on the beat that met `UnknownLease`; `shard.Member.tick` answers `evicted`):
1. the follower ends; the writer's block-grant windows are forgotten (never returned);
2. every volume RETIRES (`JoinedSet.retireIncarnation`): the latch refuses new mutations `Retired`
   (the conveyor's `Deferred`), the park gate takes RETIRED (one CAS off RUNNING/PARKED), the growth
   gate closes and drains — passes, then windows — within one landing ceiling, then the latch fails
   under the SMO lock: no cycle, page or landing writes again;
3. the eligibility (above);
4. every volume joins afresh through the mount's own door (`KvBackend.openJoined`: census, endpoint
   poll, `awaitManagerListener`, `JoinAppender`, rotor, first checkpoint); its projection's rows naming
   the predecessor load unleased;
5. every token dropped; the successors published (bindings re-applied, one pointer store per member,
   the retired incarnation kept mapped until the unmount); the member joins afresh (new epoch, its
   listener re-published under the new appender id); the read cache and the kernel's whole view drop;
6. the move epoch bumps.

**Bounds and in-flight ops:** the drain ≤ one landing ceiling (1.1 s); the recovery wait and the
membership join ≤ `T_park_max`; measured end to end 160–200 ms after the beat's release at the smoke's
`T_owner` of 3 s. A read in flight completes off the retired RAM (the recovered state); a commit
answers `Deferred` and the move gate re-issues it on the successor (≤ `T_park_max`); only a refused
re-join answers EIO (the poison closes the move epoch at once).

## The lock-free core and its model

`park_core_model.zig`: invariant 2 now races release, expiry AND retirement off PARKED — holds under
the CAS, the load-then-store weakening lets two act; invariant 4 (`RetireModel`) — a pass enters the
growth gate (an RMW), reads the word, tracks its window, leaves; the retirer CASes RETIRED, waits the
passes, then the windows, judges. Holds; **weakenings that fail:** the pass's fence removed (its
increment buffered while it reads RUNNING), and the window count read before the pass count.

## Defects found on the way (fixed here)

- **The successor's stale projection.** Its checkpoint can predate the recovery's tree-0 release, so
  slots it names the PREDECESSOR the lessee of were foreign holders no one is: a shipped write publish
  to A met its coherence hold for every retry. `JoinOptions.predecessor` → `joinedProjectionRowOf`.
- **The owner's fresh join (`graceExpects`).** A predecessor-roster member stayed "expected" for the
  owner's life: after it re-asserted and was evicted (or left, or remounted), every join without an
  epoch took the reclaim path and answered `UnknownLease` — the re-join's membership join under a
  remounted manager was refused (the dead-manager eviction row). `graceAwaitsReassertion`.
- **The double join of the follower** on a refused re-join (SIGSEGV at umount): `follower: ?Thread`.

## Red tests

| Test | dev `885727b` | branch |
|---|---|---|
| smoke "A LIVE MEMBER EVICTED WITH NOTHING UN-LANDED RE-JOINS IN PLACE" (idle-clean B, a file held open read-write, beat held past `T_owner`) | `PoisonedInsteadOfRejoined` | re-joined; the held fd writes on the successor; A and B byte-equal; create + rename in A's dir; exit 0 both; fsck 0 findings |
| smoke "… WITH UN-LANDED STATE FAIL-STOPS" (64 KiB held in custody, `SQUEEZEFS_WRITEBACK_POSTURE=close`) | poisoned, no judge | "cannot re-join in place — write-side custody on 1 ino(s)", EIO in 2 ms, exit 1 (crash shape), fsck clean |
| unit `park_core_model` invariants 2 (three exits) and 4 (`RetireModel`) | — | hold; the weakenings fail |
| unit "THE RETIREMENT …" (park gate), "the retired latch …", "THE RE-JOIN IN PLACE's verdict …" (`Member`) | — | green |
| unit "a predecessor-roster member that re-asserted, then left or was evicted, joins FRESH …" | `UnknownLease` at both arms | green |

The eviction row under the dead manager's striped directory (F4) now holds custody at the eviction:
its verdict is the fail-stop deterministically (its storm alone left B eligible in 1 run of 3).

## Counts (on `8dba8fa`, before the rebase)

- The three eviction rows (the re-join, its negative arm, the dead-manager eviction row) **20/20**,
  two copies in parallel × 10 rounds beside the membership batch loop (and a full gate in the main
  tree): re-joined 121–202 ms after the beat's release, A evicting B 4.5–4.9 s into the hold.
- The membership/recovery/kill batch (26 rows: N-DAEMON, the clean rejoin, the three manager-remount
  joins, TWO MOUNTS OF ONE SET, KILL ROWS a/b/c, KVDA ×2, SHARED HOT, DEAD MANAGER, the four
  live-member rows, the `-o ro` reader, the ordered law's kill row, ONLINE FSCK) **4/4** runs green
  (+ 2 single runs before).
- Unit filters `membership`, `reclaim`, `lattice`, `joined` (+ `RE-JOIN`, `retire`): 249 pass, 24
  skipped, **20/20** runs; `journal`, `conveyor`, `lease_table` with them 433/462 (29 skipped).
- `zig build rails` 32/32; `zig fmt --check` clean.
- Rebased onto dev `eeec3e8` (the data-plane answers, §BO–§BQ): the three eviction rows 11/11
  (2 × 5 + 1), unit filters with `journal`/`conveyor`/`lease_table` 442/472 (30 skipped), rails 32/32.

## Follow-ups (branch `fix/rejoin-followups`, 0003 BS.10–BS.15)

- **The member joins before the publication.** A refused join closes the unpublished successors (a
  fresh join's clean leave, page `Free`) and poisons over the predecessor — no leave against `FAILED`
  latches at the unmount (seam `SQUEEZEFS_TEST_REJOIN_MEMBER_REFUSED`).
- **The holder's grants** over the retired slots dropped (`Holder.dropAll`).
- **The retired node cache, released** once every FUSE op (registry slot + unique) and every
  interception session in flight at the publication answered; a held node dies at its last release; a
  background reader past the drain re-reads seq-checked children (typed mismatch, never wrong
  content). Measured: `kv_node_cache` 17,735 KiB → 17,641 KiB released → 262 KiB; VmRSS 295 MiB ≤
  the idle-RSS bound 336 MiB (64 MiB budget, 156 threads).
- **Every binding re-applied:** the writer plane's grant verbs and the fleet worker through
  `WirePlane.liveManagerCall`; the job node's store/world/rekey, the stripe floor and the xv plane's
  `own_id` follow `volumes[0]`. A joiner's job fabric is never the coordinator (submit refuses
  `NotCoordinator`, adopt is a no-op), so "a job reads a frozen tree 0" has no joiner path; the
  observable red was elsewhere (next item).
- **The death record of a member live again retires** (`Driver.retireRejoined`; counted
  `dead_members_retired_rejoined`). Standing, it named the successor's Live page: A's `scrub --online`
  after the re-join reported `[C15] … appender 2 — dead` (the job after the re-join, red).
- **Two-volume joiners:** members past volume 0 join with volume 0's `job:enroll` secret (a joiner of
  a two-member meta set could not mount at all).
- **A joined ring's death replay is judged** (BS.16): the chain is the appender's own page off the
  device's directory, which must be Live and name the ring. A failed joined replay is still not held:
  the region's successor is the manager's recovery off the device.
- **The interposer:** a `dd` writer and reader keep their ring sessions across the re-join (sessions
  2 → 2; after it the ring served 8 writes and 15 reads), no error reaching the app, A byte-equal.

| Test | dev `23db6c2` (+ the refusal seam on its ordering) | branch |
|---|---|---|
| main re-join row (+ holder grants, held-fd and fresh appends, the scrub job) | `dlm_token_outstanding` 2 | 0; appends equal; scrub 0 findings (before the retirement: C15) |
| "A RE-JOINED MEMBER RELEASES ITS RETIRED INCARNATION'S NODE CACHE" | no release (timeout) | released, RSS in bound |
| "A TWO-VOLUME SET'S JOINER RE-JOINS IN PLACE" | B's mount refused (no `job:enroll` on vol2) | 2 incarnations re-published, fsck clean |
| "A RE-JOIN WHOSE MEMBERSHIP JOIN IS REFUSED …" | best-effort leave (page check passed this run; no clean-leave line) | successor left at once, page `Free` |
| shim "A RE-JOIN IN PLACE UNDER THE INTERPOSER" | green with overwrites (a guard) | green |
| unit "a re-join in place drops every grant its holder issued" | — | green |
| unit "a JOINED ring's death replay is JUDGED too" (BS.16, after the custody follow-ups) | judged false: C and D written past B's hole | nothing past the hole; a page not Live naming the ring writes nothing |

Counts (on `c678245`, dev `9b081ae`): the re-join rows (the main row, its negative arm, the memory,
two-volume and refused rows, both dead-manager eviction rows) **20/20** — two loops in parallel × 10
beside the membership batch and a unit loop, one of them with the shim smoke (its interposer row
10/10); re-joined 161–182 ms after the beat's release. The membership/recovery/fleet/jobs batch (68
rows) **3/3**. Unit filters (`re-join`, `recovery`, `death`, `xv`, `token`, `membership`, `reclaim`,
`lattice`, `joined`, `stripe`, `ipc`, `job`, `retire`, `journal`, `conveyor`, `lease_table`) 803 pass,
62 skipped, 4/5 runs — the fifth failed `data.reclaim` "drain_defers_under_foreground…" (a supply-drain
counter under load; untouched here). `zig build rails` 32/32; `zig fmt --check` clean.

## Seen, not fixed

- **A joiner's APPEND to a foreign-slot file reads back zeros on both mounts** — dev, no eviction
  (repro `~/tmp/rejoin/smoke-with-control.zig`). After a re-join the class reaches B's OWN
  pre-re-join files through the interposer: an append through a ring session lands zeros on A and B
  (A: `slot_door_coherence_refusals` 780); through FUSE (a held `O_APPEND` fd, a fresh open) the bytes
  are whole. Seen on bases `23db6c2` and `9b081ae`, before the foreign-append fix (`8bb2f94`) landed; a re-check on it is owed. The interposer row overwrites.
- An online fsck within one ledger poll (1.1 s) of a re-join can still read the predecessor's death
  record (C15); the main row waits for the retirement.
- The predecessor's `Recovered` page is not released while an allocation lease names the pair — the
  successor's own leases name it too (one identity per member pair).
- Between the eviction and the beat's verdict the evicted member serves reads off its stale RAM
  (bounded by `T_owner − T_self` plus the hold); B's checkpoint task may still write its page in that
  window on a non-PR substrate (a zombie page write would read Live and refuse the re-join: safe).
