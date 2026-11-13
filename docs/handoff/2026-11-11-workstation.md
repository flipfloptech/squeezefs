# Handoff — 2026-11-11

Supersedes `docs/handoff/2026-11-10-workstation.md`. That note still says `feat/pr4`,
metadata WERO, and a merge of PR-4. Do not follow it. PR-4 is on `dev`. This work is PR-5.

## Current state and next action

**Branch `feat/pr5`.** F7b is in `b1f87fa7`. This note is the docs commit on top
of it. F7a is `bd0d3f5f`. F6 is `88003de0`. F5 is `2a8f4a2b`. F2, F3, and F4 are in `bc7ae7c4`. The fencing registrant is retired in
`281bd07b`. Incompat
bit 4 and `claim convert-pr` are `694b43c2`. The blind-rule refusals are
`eea93412`. Report-first recovery is `d75445b9`. Do not amend those. No upstream.
Not pushed. Not merged. `dev` is `b40ac017`.

Decision 0004 is the spec. PR-5 is not finished. W_end, the fence debt, `identityHolds`,
the registration lock, rule markers, the re-acquire mark, the frozen-joiner kill,
report-first recovery, the blind-rule refusals, and incompat bit 4 are committed.
The hashes are in the table below.

**Next action, in this order. Do not skip ahead, and do not start PR-6 or PR-7.**

1. The fidelity rows that are not yet written: F8, F11, F13.
   F8 is next (migration without bit 4, refused by name, `claim convert-pr` converts, a second run is a no-op). F12 and F14 need
   `SQUEEZEFS_TEST_SUCCESSION_REASSERT_HOLD`, which does not exist. Do not build
   that seam before F8, F11, and F13. `claim posture-reset` is the other offline
   door; it is not one of these rows.

A full venue gate has not been run on this branch. Do not merge to `dev` until that gate is
green and the owner asks. Landing a fast-forward of `dev` is pushed by the local post-commit
hook. Never push `feat/pr5`. Never force-push. Never amend these commits.

## Commits on `feat/pr5` (oldest first)

| Commit | What landed |
|---|---|
| `474f27f5` | F1 succession re-registration, committed red on WERO. Do not amend. |
| `942351e0` | Symmetric metadata hold is one preempt-all under WEAR (rtype 5). `writer_guard_preempt_alls`, victims, unseen. Flat volumes stay WRITE_EXCLUSIVE (rtype 1). |
| `cc73f139` | A joiner classifies a device refusal from a fresh reservation report (`Readmit.observeReport`). This does not help an idle joiner. Leave it. |
| `46f0380e` | A parked reclaim re-points the membership link. The joiner's holder-0 token link rechecks `managerEndpoint()` while the TCP session still stands. SIGSTOP does not poison TCP; a reply timeout does not either. |
| `fdc136f6` | The succession window opens on the early hold and settles when every expected member is back and registered. F1 pins `writer_guard_succession_early_settles` 1. |
| `c258ac61` | Admission pays the predecessor's host key (`data_pr_debt_preempts`) while the window is open. The general reconcile runs when the window settles. F1: debt 1, `data_pr_reconcile_preempts` 0. |
| `697e5f35` | Reconcile does not preempt a key a Live appender page on volume 0 names. A recovered page does not spare the key. F9's data fence moved to `data_pr_preempts` 1, reconcile 0. |
| `ef1c472c` | W_end evicts a listed writer whose key is missing from a managed namespace, then reconciles and clears the window. `writer_guard_succession_evictions`. The reconcile no longer runs at settle. |
| `938cdb9f` | Volume 0's claim records `meta_fence_debt`. Admission pays every key it names, then deletes the record. F1: `data_pr_debt_preempts` 1, `data_pr_reconcile_preempts` 0, 41/41. |
| `51f0187b` | Appender 0's page is ours, usurped, or unreadable (§4.4.2). See below. |
| `1fffe4ea` | `.register` is its own flock. A rule and a manager mutation hold it across the page check and the command. See below. |
| `415fbdef` | A metadata rule's `.reg` / `.wd` / `.abandoned` marker. A section refuses while one stands. The hold check acquires and then removes that volume's judged markers. See below. |
| `ef271bf2` | `a <boottime_ns> <generation>` on the manager's registry line, written under the registration lock. The re-judge kills an unmarked manager when a restart filled in after the re-read. See below. |
| `7c7feecc` | The same holder across one member-death bound, every thread stopped or the cgroup frozen, and an unjoined joiner line. See below. |
| `d75445b9` | A death fence whose key the report already shows absent is `.fenced` and issues no preempt. `appender_recovery_keys_absent`. See below. |
| `eea93412` | A sibling the stale-sibling rule cannot judge is refused before registration. Registry root via `openat2`, and the node-token reasons. F15, F16, F16b. See below. |
| `694b43c2` | Incompat bit 4 `META_PR_ALL_REGISTRANTS` and `squeezefs claim convert-pr --all-mounts-down`. A mount does not stamp the bit. See below. |
| `281bd07b` | The fencing registrant is gone. The manager's own client fences a dead member. A kept key is `meta_pr_preempt_kept`. See below. |
| `bc7ae7c4` | A parked mint stays Held and waits at the park gate. A dismount terminates a pending resolution word before `Listener.stop`. F2, F3, F4. See below. |
| `2a8f4a2b` | F5: a lapse inside the succession window. The joiner re-registers through the lapse path, `writer_guard_preempt_alls` stays 1, the window settles, one reconcile. |
| `88003de0` | F6: two metadata volumes. Each peer is parked for the succession. The write is released only after both volumes have re-registered. |
| `bd0d3f5f` | F7a: a joiner's node read during the preempt-all drain. One barrier, one retry, and the drain's length. |
| `b1f87fa7` | F7b: a joiner's write during the preempt-all drain. EBADE, held, re-registered, retried. `writer_guard_fenced` stays 0. No product change. |

## What is true in the code

Product metadata on a symmetric volume is WEAR. Takeover is `pr.Client.preemptAllAndAbort`, not a keyed preempt. `host_fence.takeSuccession` returns `.acquired` only when the namespace was unreserved, no other host, and the generation gap is 1. Everything else, including a real successor, is `.preempted`.

The successor's preempt-all does **not** run in `Guard.hold`. `KvBackend.open` step 3b calls `Early.takeHold` first. `resolvePrClient` (`src/meta/guard/gate.zig`) adopts that hold and, when `e.hold == .preempted`, sets `g.succession`. `arbitrateBlocking` skips `hold()` when `early_fresh` is set. Opening the window only inside `hold()` leaves the product window null. Both sites set it today. Keep both.

`Owner.successionCensus` (`src/meta/sym/membership/shard.zig`): `.none` with no grace, `.pending` until every expected id has reasserted or the deadline was reaped, otherwise `.complete` with the distinct non-zero keys of members who came back. `Grace.completed_early` must be written through `if (r.grace) |*gp|`. A copy of the optional does not stick. A standing death record does **not** complete the census. The reap does.

`WirePlane.tick` reaps, settles every volume, then calls `endSuccession`. The import is `guard_gate`. A file-level `const gate` shadows `var gate = ParkGate.arm` and does not compile. `keyMissing`'s parameter is `host_key`: a parameter named `key` shadows the file's `const key`.

The predecessor key is `AppenderSet.predecessor_host_key`, captured in `joinRegion0` from the Live appender-0 page **before** `words.host_key = self.hostKey()`. Zero and this mount's own key are not debts (same-host remount, F9). On volume 0 the claim transaction also unions that key into the durable `meta_fence_debt` record (uncommitted; see below). `DataFence.payDebt` preempts every key that record names, plus the RAM predecessor if the record does not, and counts `data_pr_debt_preempts` only when the key was present. It runs whether or not the window is open. The general reconcile still waits for W_end while the window is open. `Guard.markSettled` only sets `settled` and counts. The reconcile is `Guard.endSuccession`, which the wire plane calls after the eviction. That reconcile skips Live-page host keys (`data_pr_reconcile_live_page_skips`) and still preempts any other data key absent from volume 0's metadata report.

F9 and F9b: M and M2 are the same host, so the debt pays nothing. X is SIGSTOPped, never reasserts, and is reaped at the grace deadline. X's page is still Live when the window settles, so the reconcile skips X. Recovery's data preempt then fences X (`data_pr_preempts`). `expectReapedHostFenced` sums `data_pr_preempts + data_pr_reconcile_preempts`. Do not weaken that sum.

F10 waits on `data_pr_debt_preempts`, not `data_pr_reconcile_preempts`. JA is co-located with M, so the predecessor key is JA's host key. The acked-file count is read **after** the writer process has exited. Sampling it when the stop file is created races one more fsync (the red was expected 12, found 13).

## W_end, committed `ef1c472c`

Decision §4.6, on the wire-plane cadence. `WirePlane.tick` reaps, settles, then `endSuccession`:

1. Reap. `Owner.reapGraceDeadline`, at the start of the tick, unchanged.
2. Evict. `Owner.evictUnregistered`. A listed writer who re-asserted, whose key is not 0, and whose key `keyMissing` says is absent from some managed namespace. The death carries the key the member holds now (`MemberState.pr_key`), its row is deleted outside the lock, and the sink runs outside the lock. Someone who never re-asserted is the reap's. `writer_guard_succession_evictions` counts the writers, once, in the stats JSON.
3. Reconcile. `Guard.endSuccession` runs `settle_hook` (`DataFence.endSuccession`). `markSettled` no longer does.
4. Clear. The same call sets `g.succession` null and logs. `endSuccession` is idempotent (`reconciled_succession`), and a second call finds the window null.

Before the grace deadline, one unsettled volume holds every volume. At the deadline it does not: the reap has run. A volume that never opened a window holds nothing.

Two unit tests, both green on 2026-11-11 (`-Dtest-filter="W_end eviction"` and `-Dtest-filter="W_end (decision 0004"`, 42/42 in 88 ms): the owner's eviction (kept, evicted with its key, the silent one left to the reap, a key of 0 kept, a second pass evicts nobody) and the guard's order (no reconcile at settle, none before it, one at the end, the window null after, a second end reconciles nothing).

Not pinned, and not worth a fixture unless a later row goes red:

- No test drives `WirePlane.endSuccession` over two namespaces. A `MetaSet` open takes one `guard` option, so both volumes share one host face and one registration; a key cannot be on one and not the other. `keyMissing` returns false when a report cannot be read, so an unreadable namespace evicts nobody.
- The eviction removes the member and deletes its row. A member evicted this way is not also evicted at `T_owner`. The death record is what recovers it (§4.11).

## Fence debt, committed `938cdb9f`

Decision §6.3 and §4.4 step 10. Debug unit tests green on 2026-11-11
(`-Dtest-filter="fence debt"` and `-Dtest-filter="pr5 debt"`, 43 pass, 2 skip, 575 ms).
F1 on the safe exe after the `owesFenceDebt` fix: `data_pr_debt_preempts` 1,
`data_pr_reconcile_preempts` 0, 41/41. The safe link was about 5 minutes; the test about
1 minute.

- Tag `meta_fence_debt = 16`, singleton, volume 0 only. Value: magic `MFDB`, version 1,
  `n` ≤ 64, reserved, keys `u64` LE, xxh3-64. `encodeFenceDebt` / `decodeFenceDebt` /
  `mergeFenceDebt`. A 65th key is `ChainTooLong`.
- `Guard.commitClaim` stages `debt ∪ {page 0's Live host key} \ {own key, 0}` in the
  claim transaction. The page still names the predecessor; region 0 is rewritten after
  this commit. An unreadable page or record refuses the mount. A chain past 64 refuses
  with `error.Busy` and `log.err`.
- `DataFence.payDebt` pays that set on every data namespace, then `retirePaidDebt` deletes
  the record and barriers, only after a namespace was actually paid. `owesFenceDebt` is a
  file-level function. Call it as `owesFenceDebt(df)`, not `df.owesFenceDebt()`.
- A fixture with no backend still pays `predecessor_key` alone and removes nothing.
- The gate test opens A, then B, then C on one fake namespace. B's claim records A's key.
  C's records A's and B's. C's remount does not add C's own key. `storeFenceDebt(be, &.{})`
  deletes the record.

## identityHolds, committed `51f0187b`

Decision §4.4.2. Unit tests `pr5 identity` + `pr5 debt` + `lapse grace`: 47 pass, 1 skip.
F1 after the probe fix (`~/tmp/pr5-f1-ident2.log`): `regctl` 3, `writer_guard_identity_failures` 0,
`writer_guard_identity_unreadable_beats` 0, `data_pr_debt_preempts` 1, `data_pr_reconcile_preempts` 0,
41/41. One F1 before that fix was red: `regctl` 2 and `appender_parked` 1, while J's `cat` still
returned the cached bytes. Do not treat that as J never coming back.

- `noteBoundIdentity` runs from `joinRegion0` after the page write. `bound_term == 0` means the
  check does not apply (a joiner, or before region 0 joins). A joiner's `screenIdentity` is a no-op.
- `identityHolds`: Live page names this writer and this term → ours. Another non-zero writer, or
  this writer at another term → usurped (fail-stop, `writer_guard_identity_failures`). Anything
  else, including a read error → unreadable. Not a guess.
- The beat calls `screenIdentity` before the reservation re-check and the claim refresh. Unreadable
  skips the refresh. The fifth consecutive unreadable beat fail-stops. One readable beat resets
  the count (`writer_guard_identity_unreadable_beats`).
- The probe is `pool.readAtRaw`, not `readRange`. `readRange` enters `MetaReadmit`. A conflict
  there runs the manager arm and `classifyFence`s the volume; an EIO becomes `Held` and then `Io`.
  Either one turns a probe into a fail-stop or a 10 s sleep. Raw does neither.
- The admission's bounded re-read is `provePageForDebt`, **before** the data-fence lock. It
  re-reads at the beat interval, up to 5, and refuses only on usurped or an exhausted budget.
  Tests do not sleep. `payDebt` then calls `identityHolds` once under the lock and does not
  sleep. `reconcile` checks once. `reacquireBlocking` and the kept-law release call `screenIdentity`.
- Stats JSON is two `print`s. A format call allows 32 arguments.
- F1 waits up to 12 s for `appender_parked` 0. J's `cat` can hit the page cache while the
  reclaim is still in flight. The membership cadence is 5 s.
- Recovery's `fence(k)` still does not call `identityHolds`.

## Registration lock, on top of `34ace6e3`

Decision DC20 and DC25's order, not the markers. Unit test `registration lock` plus
`pr5 identity`, `pr5 debt`, `lapse grace`, and the B2 re-judge: 53 pass, 1 skip.
F1 (`~/tmp/pr5-f1-reglock.log`): `regctl` 3, identity counters 0, `data_pr_debt_preempts` 1,
`data_pr_reconcile_preempts` 0, 41/41.

- `Registry.lockRegistration` opens `<dir>/.register` on a fresh file description and flocks
  it. `wait_ms` 0 waits `REGISTRATION_LOCK_WAIT_MS` (30 s, one admin timeout). Closing the
  description releases it. Two descriptions exclude each other, including two threads of one
  process. The registry flock stays `.lock`.
- A rule (`registerUnderRule`, `registerDataUnderRule`) holds it from the re-read before
  `register` through the re-judge's reads, then releases it before a withdrawal. The kill
  and the drain stay outside. Callers already hold the registry flock.
- A manager data mutation (`payDebt`, `reconcile`) holds the data-fence lock, then this
  lock, across the page check and the command. No registry (a fixture, the seam): the page
  check alone. The admission's bounded re-read still runs before the data-fence lock.
- The lapse re-acquire takes the flock, lets the rule take and drop the registration lock,
  then takes it again around the page check and `acquire`. The sequence lock is inside that
  section. A refused lock returns `.ours` so this beat still refreshes the claim; the lapse
  stays open.
- The kept-law release takes the flock, then the registration lock, then the sequence lock
  around `leaveRegistration`. A refused lock leaves the registration and still drops the client.
- Taking the registry flock while this thread holds the registration lock logs an error
  (DC25). It does not refuse the flock.
- Not in that slice: markers (now the next section), the re-acquire mark, the frozen-holder
  kill, DC24's stale-manager kill under the lock, and the five-lock rank rail.

## Markers, on top of `1fffe4ea`

Decision §4.4.2 revisions 22–27, the marker itself. Not the re-acquire mark and not the
frozen-joiner kill. Unit tests `marker `, `registration lock`, `B2 (review)`, `pr5 identity`:
60 pass, 1 skip (the filter also matches older "marker" rows). F1 (`~/tmp/pr5-f1-marker.log`):
`regctl` 3, identity counters 0, `data_pr_debt_preempts` 1, `data_pr_reconcile_preempts` 0, 41/41.

- `src/meta/guard/marker.zig`. A metadata rule, under the registration lock, creates an empty
  `.reg-<boot>-<pid>-<ticks>-<volume>-<sequence>` after the re-read and before `register`.
  The boot id is the dashed text, so the name is parsed from the right. `EEXIST` trips
  `host_fence_marker_exists` and refuses the rule. Data rules create none.
- A clean re-judge unlinks `.reg` before the lock drops. A refusal after the register writes
  the boottime and an xxh3-64 over it, then renames to `.wd-`. The unregister is outside the
  lock. Success unlinks `.wd`. Failure renames `.wd` to `.abandoned-` under the lock.
- `MARKER_WITHDRAW_MAX_S` is 50: 10 s withdraw + 30 s `io_timeout` + one 10 s beat. The tie
  test restates it and states the `io_timeout ≤ 30 s` assumption. A `.wd` past that, or with
  a bad checksum, is abandoned. The three aged counters split a still-alive rule by its main
  `/proc/<pid>/stat` state (`T`/`t`, `D`, other). The other counter logs. One judgement per
  marker, not per beat.
- A section lists the directory while it holds the registration lock. A live `.reg` there
  trips `host_fence_marker_live_reg_locked` and refuses. A live `.wd` refuses every section,
  including the lapse acquire. A dead `.reg` refuses mutations, and refuses the lapse acquire
  until 30 s after this process first saw it dead. An abandoned marker refuses mutations and
  not the lapse acquire.
- The beat, when not in a lapse, and only on a symmetric volume: if a dead or abandoned
  marker is ready, it takes the registration lock, reads the report, acquires when this host
  holds WEAR, and removes the ready markers of **this volume**. Another volume's marker stays
  until that volume's guard does the same. The re-acquire mark is not written. A conflict
  retries next beat and leaves the markers.
- Not in that slice: the frozen-joiner kill, every-thread state (the aged counter reads the
  main stat only), the rule's "unregister only while my `.wd` still stands" test, and the
  no-manager prune.

## Re-acquire mark, on top of `415fbdef`

Decision DC24. Unit tests `re-acquire mark`, `registration lock`, `B2 (review)`, `pr5 identity`,
`lapse grace`: 52 pass, 1 skip. F1 (`~/tmp/pr5-f1-mark.log`): `regctl` 3, identity counters 0,
`data_pr_debt_preempts` 1, `data_pr_reconcile_preempts` 0, 41/41.

- The line is `a <boottime_ns> <generation>` on the manager's registry file. `noteReacquire`
  replaces an older mark and keeps the region lines. `markedSince(since)` is true only for a
  mark at or after the re-read's `CLOCK_BOOTTIME`.
- The lapse acquire and the hold check's acquire write it before they release the registration
  lock. The generation is the report's after the acquire.
- The re-judge, when its re-read saw the namespace unreserved and the report after `register`
  is reserved: a live manager entry other than this process with no mark since the re-read is
  signalled under the lock and awaited outside it, and the fresh registration is withdrawn.
  A mark, or no live manager, leaves the register. This process's own lapse rule does not
  judge itself.
- Not the frozen-joiner kill (now the next section), and not "reserved and unregistered at
  the re-read" as its own kill before the register. That re-read still registers. The mark
  clause is the half that runs when the re-read was unreserved.

## Frozen joiner, on top of `ef271bf2`

Decision §4.4.2, revisions 20–22 and the aged-marker half of 26–27. Unit tests
`frozen joiner`, `frozen proof`, `marker `, `registration lock`, `re-acquire mark`,
`pr5 identity`, `B2 (review)`: 64 pass, 1 skip. The death-proof filter: 41 pass.
F1 (`~/tmp/pr5-f1-frozen.log`): `regctl` 3, identity counters 0,
`data_pr_debt_preempts` 1, `data_pr_reconcile_preempts` 0, 41/41.

- A joiner's unjoined `j` line is written before `registerUnderRule`, so a freeze inside
  the section has a line with no page. A failed rule removes that line. The rule still
  skips this process's own entry.
- `FrozenJoiner.note` starts over on a different holder or an unnamed refusal.
  `MEMBER_DEATH_BOUND_NS` is `T_owner` (the claim TTL, 45 s). The tie test restates it.
- The proof is `proc.ruleState`: every `/proc/<pid>/task/*/stat` is `T` or `t`, or
  `cgroup.events` says `frozen 1`. A partial scan is not frozen. One thread in `D` outside
  that is uninterruptible. Anything else, including a stopped leader with a running
  thread, is other and is not killed. The aged `.wd` counters use this, not the main stat.
- The beat notes a live marker's holder, lock-free. A refused registry flock, registration
  lock, or live marker on a manager path (the lapse rule, the lapse acquire, the release,
  a data mutation) notes the flock holder or the marker's holder. Taking the lock with no
  live marker clears the count. Our own pid is not a candidate.
- The kill reads the line lock-free. It signals only an unjoined joiner: no manager, no
  pending line, no page. No drain. `host_fence_frozen_joiner_kills` counts it. A kill that
  does not land retries next beat. A joined page or a manager line is left alone.
- Not in this slice: the rule's "unregister only while my `.wd` still stands" test, and
  the no-manager prune.

## Report-first recovery, on top of `7c7feecc`

Decision §4.8 step 4. Not the page check. `.ignored` and the fencing registrant were retired in `281bd07b`.
Unit tests `report-first`, `death preempt`, `review 4 minor 3`, `decision 0004 §4.8`:
56 pass. F1 (`~/tmp/pr5-f1-report.log`): `regctl` 3, identity counters 0,
`data_pr_debt_preempts` 1, `data_pr_reconcile_preempts` 0, 41/41.

- `preemptDeadRegistrant`, after `admittedHere` and before the preempt, reads the
  report again. The key absent returns `.fenced` and counts `appender_recovery_keys_absent`.
  No preempt is issued, and `appender_recovery_preempts` does not move. A report that
  fails there is still `.failed` (the tail scan). A key the report names is preempted
  as before.
- The re-fence still calls the same function only when the key came back, so an absent
  key at step 2b stays a no-op. A key that vanishes between that check and the preempt
  takes this skip.
- Not in this slice: `identityHolds` on the recovery fence. `.ignored` was retired with the fencing registrant (`281bd07b`).

## Blind-rule refusals, committed `eea93412`

Decision §4.4.2 and KD19. The registry is created first, then compared. The pid-1 side is
`openat2` of the path with no leading slash, rooted at an `O_PATH` fd of `/proc/1/root`,
with `RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS`. Do not walk it in userspace. `ENOSYS` is
a refusal. The pid-1 side's `ENOENT`, and a different `(st_dev, st_ino)`, are
`registry_root`. Every other failure, including an unreadable `/proc/1/root`, is
`registry_root_unreadable` with the errno named. A private mount namespace that keeps
the host's `/run` is the same inode and is accepted.

The node token is `hash64(boot_id ‖ hostname)` (`Identity.nodeToken`; 0 becomes 1).
The UTS discriminator is the inode of `/proc/<pid>/ns/uts`. A different namespace, or an
unreadable one, is `node_token_hostname`. The same namespace with another token is
`node_token_renamed`. No token is `node_token_missing`. The refusal runs at every
admission, including adoption. Each reason has its own counter beside
`pr_stale_sibling_refusals`. The log names the reason as `(reason …)` and states the remedy.

`SQUEEZEFS_TEST_HOST_REGISTRY_DIR` set to a path exempts the comparison, because the rig
shares one scratch root. `off` runs the real check. `SQUEEZEFS_TEST_PID1_ROOT` replaces
`/proc/1/root`; absent and `off` both mean `/proc/1/root`. `HeldDir.runtimeOf(.hosts)`
does not read the seam: as root it is always `/run/squeezefs/hosts`.

F15 turns the seam off, so it writes the host's `/run/squeezefs` and deletes that tree
after the daemons are gone. A failed run can leave root-owned lines there. Remove them
before the next seam-off row. C's FUSE mount is in C's private mount namespace, so the
parent's `onMount` cannot see it. `onMountNs` forks, `setns` into the daemon's mount
namespace, and execs. F16 and F16b do not set `mount_ns`; their mounts stay in the
parent namespace.

Device rows, 2026-11-12, this host. The nvmet summary count is one empty passing stub
per source file plus the real rows. A green F15 line that says 41/41 is the one real test.

- Units of the comparison (`~/tmp/pr5-blind-unit.log`): 45 pass, 3 skip. The three skips
  are F15, F16, and F16b, which have no `./squeezefs` in that binary.
- F15 (`~/tmp/pr5-f15-smoke2.log`): `EXIT:0`, 41/41, the test step 3s. The run before the
  namespace observation (`~/tmp/pr5-f15-smoke.log`) refused B correctly and failed at
  `cat f15/m` from the parent namespace.
- F16 and F16b (`~/tmp/pr5-f16-smoke.log`): `EXIT:0`, 42/42, the test step 9s. The product
  exe was cached.

## META_PR_ALL_REGISTRANTS, committed `694b43c2`

Decision 0004 §6.4 and decision 0002 row 571. Bit 4 is presence required.
`features_incompat_known` is `0x1F`. `features_incompat_implemented` is the plane,
the block-map tree, and bit 4. Layout deltas and the volume lifecycle stay refused.

Format and enable-symmetric set the bit on the symmetric class.
`SQUEEZEFS_TEST_FORMAT_WITHOUT_META_WEAR=1` omits it. A single-writer volume stays
features 0. The missing-bit refusal is writer mode only, before
`registerBeforeWrites`, and the log names `squeezefs claim convert-pr
sqmeta://<path> --all-mounts-down`. A reader and `claim clear` still open.
`Mode.convert_pr` returns `UnsupportedFormat` before replay. The verb is
`guard.convert_pr.claimConvert`, which does not open a backend.

The verb requires `--all-mounts-down`. It disconnects controllers whose hostnqn
starts with `nqn.2026-10.io.squeezefs:fencer:` before any volume. Per volume: the
writer flock, a single-writer volume is left untouched, a fresh claim or any other
live registry line of the set refuses, then register, stamp bit 4 on both
superblock copies, retype to Write Exclusive — All Registrants, restamp, and
rewrite layout-1 directory pair slots and appender 0's four slots to layout 2
with `host_key` 0. A foreign page keeps the stamp and the hold. A later volume's
refusal leaves an earlier volume converted.

`convertReservation` sets `client.rtype` to WEAR only after the confirming report.
`preempt_alls` and `preempt_all_victims` move only then. `.acquired` is only an
unreserved namespace, no other host, and a generation gap of 1.

Units on this host, 2026-11-12: `scripts/venue test convert-pr`
(`~/tmp/convert-pr-direct.log`, 51 pass) and the superblock, format, args,
enable-symmetric, and device-format filters (`~/tmp/convert-pr-related.log`,
52 pass, 714 ms). No full gate.

## No fencing registrant, committed `281bd07b`

Decision 0004 §4.8 and §4.14. `src/meta/guard/fencer.zig`,
`src/meta/guard/host_fence_model.zig`, and `scripts/udev/60-squeezefs-fencer.rules`
are deleted. The manager's own client issues the death preempt and the re-fence.
`preemptDeadRegistrant` and `refenceAfterRecovering` take the client and the victim key.

A key the report still names after a successful preempt-and-abort fires
`meta_pr_preempt_kept` and returns `.failed`. `.ignored` is gone. The own-key
guard stays. A preempt is issued only while a report shows this host holding.
`leaveRegistration` keeps the hold when `keep_hold` or `c.stats.preempts > 0`.

`acquireManagerHold` still forces WERO. Do not change it. The product's symmetric
hold is `host_fence.takeSuccession` under WEAR. Flat `pr_rtype` 1 and the WERO
pr-legs measurement stay 1. `DataRtype.wero_holder` and the meta-fence weakenings
that name a WERO holder stay.

A mount on a node whose controller hostnqn starts with
`nqn.2026-10.io.squeezefs:fencer:` is still refused (`error.FencerNode`, until
PR-8). `convert-pr` still disconnects those controllers.

The recovery-ladder step 2 unit is a flat volume. A symmetric hold refuses a
reservation that is not WEAR instead of preempting its one holder.

Units on this host, 2026-11-12: the preempt, judgement, fencer-prefixed,
co-located, stale-sibling, and stats-JSON filters
(`~/tmp/fencer-retire-unit.log`) were 112 pass, 16 skip, 1 fail. The failure
was that ladder test opening a symmetric volume onto a Write Exclusive holder.
It is a flat volume now (`~/tmp/ladder-step2.log`, 41 pass). The device rows
in the unit binary skip without `./squeezefs`. No full gate.

## Parked mint and the dismount join, committed `bc7ae7c4`

Decision 0004 §4.5.5, §4.5.6, §4.12, §4.16, §8.2 F2–F4. A joiner's first guest-tree
mint during a succession park was collapsed to `Io`, so the post-takeover create
returned EIO. `mintGuestTree` drops `trees_lock` on `Held` and waits in
`preAdmission` while the gate is parked. `MetaSet` keeps `Held`, `Deferred`, and
`VolumeFailed`. A wire serve sets `serve_must_not_wait`, so `preAdmission` answers
`Retired` and leaves the gate parked. That flag does not cover a serve already
inside `Conveyor.commit`. `WirePlane.stop` terminates a pending resolution word
after the membership beat is joined and before `listener.stop`, so `awaitPast`
returns `.terminal` and the conn can retire. A cleared word is left alone. No
poison, no park release, and the 32s umount budget is unchanged.

Device, 2026-11-12, cached daemon except F2's link: F2 `~/tmp/pr5-f2j.log` (41/41,
about 1 min of test after a 5 min link); F3 `~/tmp/pr5-f3.log` (41/41, 55s); F4
`~/tmp/pr5-f4.log` (41/41, 8s). Each binary's only fidelity name was that row.
The 41 includes one empty stub per source file. No full gate.

## Lapse inside the window, committed `2a8f4a2b`

Decision 0004 §4.9 and §8.2 F5, tcp. J is SIGSTOPped before S mounts, M past the
claim TTL. S preempts-all once (`SQUEEZEFS_TEST_LAPSE_GRACE_S` 600). J's log has
no lapse line while it is stopped. Both subsystems are wiped and restored; the
metadata namespace reads rtype 0. J continued re-registers through the lapse path
(`meta_pr_lapse_reregistrations` 1). `writer_guard_preempt_alls` stays 1,
`writer_guard_succession_settles` is 1, and S's log has one
`succession reconcile over` line.

Device: `~/tmp/pr5-f5.log` (`EXIT:0`, 41/41, about 1 min of test after a 5 min
link). The resv-report retries while the controller's char node is gone print
stderr; the row still exits 0. No full gate.

## Two metadata volumes, committed `88003de0`

Decision 0004 §8.2 F6, model W26, tcp, `twoMetaSet`. M manages both, J joins
both and spreads directories. M is SIGSTOPped past the claim TTL. S preempts-all
on both namespaces (`writer_guard_preempt_alls` 2, victims 4). A volume with no
commit of its own never took the T_self park, so `successionReregister` bailed
and that key stayed gone; the held write then waited out the grace. The fix
parks each peer for succession and leaves the resolution word pending while any
peer key is outstanding. The succession line is logged before that word clears.
A single volume's peer loop is empty, so its beat still clears as before.

Device: `~/tmp/pr5-f6.log` (`EXIT:0`, 41/41, about 1 min of test after a 5 min
link). Pins printed: preempt_alls 2, victims 4, succession lines 2, paths 2.
J's log has one line on each metadata device, and a succession park on home
volume 1. No full gate.

## A node read during the drain, committed `bd0d3f5f`

Decision 0004 §8.2 F7a, §4.5.3, A7. Metadata is null_blk at 50 ms. J's node
cache is 4 MiB, so an xattr leaf is evicted. S's preempt-all parks on a FIFO
until J has a write in flight, then J reads that leaf. The read returns.
`meta_pr_drain_barriers` is 1, `meta_pr_read_retries_after_drain` is 1, and
`writer_guard_preempt_all_ns` is 50038341. An unregistered barrier parks the
succession. A single volume still clears its beat.

Device: `~/tmp/pr5-f7a.log` (`EXIT:0`, 41/41, about 3 min of the row after the
link). No full gate.

## A write during the drain, committed `b1f87fa7`

Decision 0004 §8.2 F7b, §4.5.3, A7. Same 50 ms null_blk as F7a. No product
change: a write EBADE already holds, re-registers, and retries. P9c's write-EIO
instant stays the §8.1 unit. Do not add `meta_pr_write_eio_reclassified` for
this row.

J's `T_self` is the manager's grant. `SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS=600000`
is on M. This run logged `T_owner 600000` and `T_self 597200`. A knob on J does
not move it. The claim TTL stays 45 s.

The write is `fsetxattr` on an fd opened while M is alive. A path `setxattr`
after the stop readdirs ino 1 and never reaches the journal. It is released
only after a flood write has completed EBADE with commands still in flight
(this run: 26).

Pins: the succession park, re-registered, `writer_guard_fenced` 0, no
reservation-conflict latch. The park line says 0 commits because that gate's
commit counter is not incremented on the product path.

Teardown is `kill -9`. Each slot release barriers the 50 ms device (~300 ms);
J's 64 rotor slots miss `umount`'s 32 s budget.

Device: `~/tmp/pr5-f7b.log` (`EXIT:0`, 41/41, about 3 min of the row after the
link). No full gate.

## Explicitly not in this branch

- F9 and F10 have not been re-run on this commit. F1 has, after report-first recovery.
  F9's metadata fence is the preempt-all; the keyed recovery preempt should now find
  the key already gone (`appender_recovery_preempts` 0, `appender_recovery_keys_absent` 1).
  That row has not been run.
- Recovery's `fence(k)` does not call `identityHolds`.
- `claim posture-reset --all-mounts-down` (`Mode.posture_reset`).
- `meta_pr_succession_parks`, `meta_pr_succession_reregistrations`, `meta_pr_refusal_readmissions`. Do not invent them. Parks increment `stats.appender_parks`. `writer_guard_fenced` increments in `journal.noteFence`.
- Fidelity rows F8, F11, F13, F12, F14. F2, F3, F4, F5, F6, F7a, F7b, F15, F16, and F16b are committed. F12/F14 need `SQUEEZEFS_TEST_SUCCESSION_REASSERT_HOLD`, which does not exist. P9c's write-EIO instant is the §8.1 unit, not a device pin. Do not add `meta_pr_write_eio_reclassified` until that unit.
- An offline writer still never ticks the cadence, so it pays the debt at its own admission and does not run the later reconcile. The shortcut comment in `data_fence.zig` `endSuccession` still says so.
- `data_pr_reconciles_deferred` and the `data_pr_reconcile_in_window` tripwire.
- A full `zig build gate` on this branch.

## Invariants that were learned the hard way

- Decision §4.4 step 6 (an open that is not registered refuses) wins over §4.4.1's row that says a frozen open between probe 2 and preempt-all resumes and takes over. Do not "fix" the early-hold park by re-registering so the parked open wins. The open that finishes preempt-all first wins. The other joins.
- An idle joiner never calls `joinedReadmit`. Root getattr needs the manager's token. Do not shorten the move-gate deadline. Do not make `selfLive` false while parked. `wantsRegister` stays false until a reclaim grant stores `grant_term` above `mgr_term` and page 0's term matches.
- `memberId` is `writerMemberId`: node token and mount slot. `writer_id` is not part of it. Expected ids and join ids already match.
- P2: under WEAR, a non-registrant can read. P6: preempt-all on an unreserved namespace conflicts, so acquire first. Keyed preempt of an absent key under all-registrants conflicts.
- `fenceDataNamespacesCounted` increments `data_pr_preempts` only when the key was present before the preempt.
- `leaveRegistration` uses `keep_hold or c.stats.preempts > 0`, not `preempt_alls`.
- Stats JSON for `writer_guard_*` is a hand-written format string **and** the `load()` argument list in `src/meta/guard.zig`. They must stay in the same order. The name list in the stats-JSON test must gain the new key. `data_fence.stats` is a `pub var` namespace and is exported automatically.
- Zig 0.17: `if (payload) |x| if (cond) stmt` does not parse when `stmt` is an assignment. Use braces. Do not shadow an import with a local named `gate`, `fake`, or `nvme`. `const g = r.grace orelse` copies the grace.
- `log.err` in the test process fails the test. `log.err` in the daemon does not. `log.warn` and `log.info` do not fail tests.
- nvmet-smoke has no cost table. Renaming those tests does not stale one. A test with no row hangs out at 900 s.
- Non-TTY `scripts/venue` is silent until the step ends. A "failed command:" footer that still says `N/N steps succeeded` is the warn footer, not a failure. A real failure is `error: '…' failed`. An empty log with `EXIT:0` in well under a second is a no-op, not a pass. Keep the filter inside one venue argument: `scripts/venue 'zig build … -Dnvmet-filter="pr5 F1"'`. `-Dnvmet-filter` is a single string. Bare `F1` also matches F10 and F11.

## How to check the pins that are already green

Cache is `/build/zig-cache-v2`. One `zig build` takes a build slot. The nvmet exe link is LTO `-Osafe`, about five minutes when sources changed. Unit tests are Debug and much faster. Logs from this session, if they survived, are under `/home/justin/tmp/pr5-*.log`.

```bash
git checkout feat/pr5
git status -sb    # clean, no upstream; F7b is b1f87fa7

# Unit: census, debt, live-page skip, the old admission reconcile.
scripts/venue 'zig build --cache-dir /build/zig-cache-v2 test -Dtest-filter="pr5 debt" -Dtest-filter="pr5 live page" -Dtest-filter="succession census" --summary all'

# Device pins. Run one filter at a time. "pr5 F9" is F9 and F9b.
scripts/venue 'zig build --cache-dir /build/zig-cache-v2 --prefix zig-out-linux nvmet-smoke --summary all -Dnvmet-filter="pr5 F1"'
scripts/venue 'zig build --cache-dir /build/zig-cache-v2 --prefix zig-out-linux nvmet-smoke --summary all -Dnvmet-filter="pr5 F9"'
scripts/venue 'zig build --cache-dir /build/zig-cache-v2 --prefix zig-out-linux nvmet-smoke --summary all -Dnvmet-filter="fidelity: F10"'
scripts/venue 'zig build --cache-dir /build/zig-cache-v2 nvmet-smoke --summary all -Dnvmet-filter=F15'
scripts/venue 'zig build --cache-dir /build/zig-cache-v2 nvmet-smoke --summary all -Dnvmet-filter=F16'
```

Last green results, 2026-11-11, on this host:

- F1 (`~/tmp/pr5-f1-debt.log`): `data_pr_debt_preempts` 1, `data_pr_reconcile_preempts` 0, 41/41, about 381 s.
- F9 and F9b before the Live-page skip (`~/tmp/pr5-f9-debt.log`): reconcile 1, `data_pr_preempts` 0.
- F9 and F9b after the skip (`~/tmp/pr5-f9-live.log`): `data_pr_preempts` 1, reconcile 0, 42/42, about 330 s.
- F10 after the acked-count fix (`~/tmp/pr5-f10-debt2.log`): 41/41, about 380 s. The run before that failed only on the racy count.
- Blind rules, 2026-11-12: F15 `~/tmp/pr5-f15-smoke2.log` (`EXIT:0`, 41/41, 3s); F16 and F16b `~/tmp/pr5-f16-smoke.log` (`EXIT:0`, 42/42, 9s). The 41 and 42 include one empty stub per source file.
- F2 `~/tmp/pr5-f2j.log` (`EXIT:0`, 41/41, about 1 min). F3 `~/tmp/pr5-f3.log` (`EXIT:0`, 41/41, 55s). F4 `~/tmp/pr5-f4.log` (`EXIT:0`, 41/41, 8s).
- F5 `~/tmp/pr5-f5.log` (`EXIT:0`, 41/41, about 1 min). Pins printed: lapse rereg 1, preempt_alls 1, settles 1, reconcile lines 1.
- F6 `~/tmp/pr5-f6.log` (`EXIT:0`, 41/41, about 1 min). Pins printed: preempt_alls 2, victims 4, succession lines 2, paths 2.
- F7a `~/tmp/pr5-f7a.log` (`EXIT:0`, 41/41, about 3 min). Pins printed: preempt_all_ns 50038341, barriers 1, retries 1, read ok.
- F7b `~/tmp/pr5-f7b.log` (`EXIT:0`, 41/41, about 3 min). Pins printed: EBADE with 26 writes in flight, write ok, fenced 0, latched false.

## Resume rules

- Read `docs/charter.md` before core logic, then decision 0004, then this note. `docs/porting/*.md` are the porting specs. The Rust tree at `~/Source/squeezefs` is evidence, not law (decision 0002).
- Branch from `dev`. Conventional commits. Red test in its own commit when the pin is a new bug. These feature slices were committed green, test and fix together. `--ff-only` merges only. Delete the branch after merge.
- Venue is local Linux ≥ 7.3 through `scripts/venue`, as root. Do not reboot the laptop. Do not install a kernel.
- Scratch and run logs go under `~/tmp` (`/home/justin/tmp`), not the repo.
- One full gate at a time: `scripts/venue 'timeout 3900 zig build --cache-dir /build/zig-cache-v2 gate --summary all'`, serialized by `flock /build/.gate.lock`.
- Memory file `squeezefs-zig-metadata-wear.md` is stale (it still says metadata is WERO). Do not trust it. Do not update it unless a slice has landed and the fact is stable.
