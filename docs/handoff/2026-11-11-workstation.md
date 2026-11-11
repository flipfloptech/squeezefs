# Handoff — 2026-11-11

Supersedes `docs/handoff/2026-11-10-workstation.md`. That note still says `feat/pr4`,
metadata WERO, and a merge of PR-4. Do not follow it. PR-4 is on `dev`. This work is PR-5.

## Current state and next action

**Branch `feat/pr5`.** The marker commit's parent is `1fffe4ea`. No upstream. Not pushed.
Not merged. `dev` is `b40ac017`.

Decision 0004 is the spec. PR-5 is not finished. W_end, the fence debt, `identityHolds`,
the registration lock, and rule markers are committed. The re-acquire mark is not.

**Next action, in this order. Do not skip ahead, and do not start PR-6 or PR-7.**

1. The re-acquire mark (DC24), written by the lapse acquire and by the hold check's acquire.
   Then the frozen-joiner kill keyed on a marker's pid (every thread, not just the main
   stat), report-first recovery (`appender_recovery_keys_absent`), blind-rule refusals
   (F15, F16, F16b), incompat bit 4 and `claim convert-pr`, retire the fencing registrant,
   the fidelity rows that are not yet written.

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
| registration lock | `.register` is its own flock. A rule and a manager mutation hold it across the page check and the command. See below. |
| markers | A metadata rule's `.reg` / `.wd` / `.abandoned` marker. A section refuses while one stands. The hold check acquires and then removes that volume's judged markers. See below. |

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
- Not in this slice: the frozen-joiner kill, every-thread state (the aged counter reads the
  main stat only), the rule's "unregister only while my `.wd` still stands" test, and the
  no-manager prune.

## Explicitly not in this branch

- F9 and F10 have not been re-run on this commit. F1 has, after markers.
- The re-acquire mark and the frozen-joiner kill. Recovery's `fence(k)` does not call `identityHolds`.
- Report-first recovery (`appender_recovery_keys_absent`). F9's metadata fence is the preempt-all; the keyed recovery preempt finds the key already gone (`appender_recovery_preempts` 0).
- Incompat bit 4 `META_PR_ALL_REGISTRANTS`, `Mode.convert_pr`, `claim convert-pr`, `claim posture-reset`.
- Deleting `src/meta/guard/fencer.zig`. `stand()` uses WEAR. `forTest` stays WERO. `acquireManagerHold` still forces WERO. Flat `pr_rtype` 1 and the WERO pr legs stay 1.
- `meta_pr_succession_parks`, `meta_pr_succession_reregistrations`, `meta_pr_refusal_readmissions`. Do not invent them. Parks increment `stats.appender_parks`. `writer_guard_fenced` increments in `journal.noteFence`.
- Fidelity rows F2, F3, F4, F5, F6, F7a, F7b, F8, F11, F12, F13, F14, F15, F16, F16b. F12/F14 need `SQUEEZEFS_TEST_SUCCESSION_REASSERT_HOLD`, which does not exist.
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
git status -sb    # clean, 697e5f35, no upstream

# Unit: census, debt, live-page skip, the old admission reconcile.
scripts/venue 'zig build --cache-dir /build/zig-cache-v2 test -Dtest-filter="pr5 debt" -Dtest-filter="pr5 live page" -Dtest-filter="succession census" --summary all'

# Device pins. Run one filter at a time. "pr5 F9" is F9 and F9b.
scripts/venue 'zig build --cache-dir /build/zig-cache-v2 --prefix zig-out-linux nvmet-smoke --summary all -Dnvmet-filter="pr5 F1"'
scripts/venue 'zig build --cache-dir /build/zig-cache-v2 --prefix zig-out-linux nvmet-smoke --summary all -Dnvmet-filter="pr5 F9"'
scripts/venue 'zig build --cache-dir /build/zig-cache-v2 --prefix zig-out-linux nvmet-smoke --summary all -Dnvmet-filter="fidelity: F10"'
```

Last green results, 2026-11-11, on this host:

- F1 (`~/tmp/pr5-f1-debt.log`): `data_pr_debt_preempts` 1, `data_pr_reconcile_preempts` 0, 41/41, about 381 s.
- F9 and F9b before the Live-page skip (`~/tmp/pr5-f9-debt.log`): reconcile 1, `data_pr_preempts` 0.
- F9 and F9b after the skip (`~/tmp/pr5-f9-live.log`): `data_pr_preempts` 1, reconcile 0, 42/42, about 330 s.
- F10 after the acked-count fix (`~/tmp/pr5-f10-debt2.log`): 41/41, about 380 s. The run before that failed only on the racy count.

## Resume rules

- Read `docs/charter.md` before core logic, then decision 0004, then this note. `docs/porting/*.md` are the porting specs. The Rust tree at `~/Source/squeezefs` is evidence, not law (decision 0002).
- Branch from `dev`. Conventional commits. Red test in its own commit when the pin is a new bug. These feature slices were committed green, test and fix together. `--ff-only` merges only. Delete the branch after merge.
- Venue is local Linux ≥ 7.3 through `scripts/venue`, as root. Do not reboot the laptop. Do not install a kernel.
- Scratch and run logs go under `~/tmp` (`/home/justin/tmp`), not the repo.
- One full gate at a time: `scripts/venue 'timeout 3900 zig build --cache-dir /build/zig-cache-v2 gate --summary all'`, serialized by `flock /build/.gate.lock`.
- Memory file `squeezefs-zig-metadata-wear.md` is stale (it still says metadata is WERO). Do not trust it. Do not update it unless a slice has landed and the fact is stable.
