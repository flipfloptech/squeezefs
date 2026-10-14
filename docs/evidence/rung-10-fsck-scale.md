# Rung 10 — fsck is linear in what it visits; four determinism leftovers

Owner ruling (decision 0003): performance first — a measured superlinear cost is a product defect,
fixed red-first with a derived bound. The gate-speed note (`rung-10-gate-speed.md` §2.2) measured
the shared-hot-directory row's closing `fsck --offline --scrub` at **83.5 s for 52,994 entries** in
one 64-stripe directory and a flat directory near-quadratic (0.04 → 1.13 s for 2 k → 16 k), and
listed three load-sensitive tests and two real-timer waits (§2.2, §2.3, §5.3). Decisions:
0003 §AC; the bound is `ops_fsck.md` §6.1.

## 1. The measurement

A per-pass timer and a per-call trace (`MetaSet.range`, `KvTree.range`, the leaf's `nextLive`)
over `fsck --offline` of a `--single-writer` set (one 2 GiB metadata image, one data volume,
`/build` ext4) holding N empty files in one directory, on the venue:

- every walk — the census's inode and layout kinds, the dentry pass, C1's raw walk, C12's overrun
  scan — cost the same per PAGE, and the per-page cost grew with N: 1.1 ms / page at 8 k entries,
  2.4 at 16 k, 5.1 at 32 k;
- a page of the inode pager (`range(kind, cursor, end-of-space, 512)`) merged **64 slot trees**
  (a one-member set: the native slot and 63 rotor slots; files mint round-robin over them), reading
  up to 512 entries from EACH (`trees 64, nonempty 64` per call; 8,451 → 385 leaf candidates per
  call as the cursor moved), then sorting and keeping 512;
- C11's owner skip-scan probed once per ino (`max = 1`), each probe descending all 64 trees.

### 1.1 The root cause

`MetaSet.range`'s merged path (any window spanning slots — every pager's: its end is the space's
last key) read `max` from every slot tree per call and kept `max`. A page cost
`Σ_trees min(max, the tree's remainder)`: up to T × page = 32,768 entries for a 512-entry page —
the remainder while N < T × page (a full walk quadratic: Σ remainder ≈ N² / 2·page), the cap past
it (linear, 64× amplified). The reference walks the slot trees in slot order and scans refs one
cursor per tree (`forest.rs` `range` / `refs_window`) — linear; the port's own merge was the defect.
The striped directory added nothing of its own: its stripes are directories in rotor slots like
any file, and the hot-directory set (three daemons' slots, then recovered) carries more slot
trees, which extends the quadratic regime to T × page.

## 2. The bound and the fix

`meta/merged_walk` (portable): a k-way merge that consults a tree only once the least head in
hand reaches the tree's routing-law bound (`key.slotTreeLowerBound`: an ino-major key of forest
slot f lies in `[f << 40, (f + 1) << 40)`; a refs key leads with 0x06, above every ino-major key),
reading each consulted tree from `⌈max / T⌉` doubling per refill, capped at what the call still
needs. Per call over T trees, consulting p, taking e:

- reads ≤ 2e + p·⌈max / T⌉ (each refill is taken whole before the next; a source's last chunk
  wastes at most its first chunk plus what it gave), **exactly e** when the call ends short of
  `max` (every consulted tree reached the window's end);
- fetches ≤ p·(1 + ⌈log2(T + 1)⌉).

Tie tests: `readBound`, `fetchBound`; the paging-equivalence row (16 trees × 300 keys, inode,
dentry and interleaved refs keys, paged at 1 / 3 / 64 / 512 — every page the first `max` of the
union, both bounds per call, law-bound and law-free); an ino-major page over 64 trees of ~100 keys
consults ≤ ⌈512 / 100⌉ + 1 trees; a refs window consults all 64.

fsck's run-level bound (`census.walkReadBound`, `walkNodeBound`, tie test): entries read ≤
3 × visited + trees consulted; node reads ≤ height × (visited + calls). An fsck over N entries in D
directories is O(N) entries read and O(N·height / page) node reads for its walks plus O(height) per
point probe (C11's per-ino skip-scan) — whatever D, however the names are striped.

C1's raw walk takes the law-free merge (`MetaSet.rangeRawCounted`, `census.Store.range_raw`: every
tree's bound is the start), so a record a tree holds outside its slot's window — unreachable by any
lookup, and so by the routed walk — is still C1's to see (0003 §AC.2): ≤ 2·max + T reads per page.

The report carries the walk (`census.WalkTally` through the store face; `KvTree`/`MetaSet`
`rangeCounted`/`lookupCounted` count at the tree): `nodes_walked` (set at last — the pager bumped
the global per page and the report's field stayed 0), `entries_visited`, `entries_read`,
`store_calls`, `trees_consulted`, and `census_ms`, `dentries_ms`, `c1_ms`, `classes_ms`,
`recheck_ms`, `scrub_ms`; the text form's `walk:` line; `.stats` `fsck_entries_visited`,
`fsck_entries_read`.

### 2.1 Red first

`fsck/scale.zig` (Linux; a real one-member set through `meta_set.TestSet`, the offline engine bound
over it): 2 k names in one flat directory, then 8 k; the same over a 64-stripe directory built as a
flip leaves it (stripe records, a marker per stripe, the commit marker, every name in the stripe its
hash routes it to). Both bounds at each size, and 4N ≤ 4.5 × N on reads and on nodes (scaled by a
level the trees may grow). Counters, not wall time: the verdict is the same on an idle host and a
saturated one. Red at the first size before the fix:

| row | entries visited | calls | trees consulted | entries read | nodes | node bound |
|---|---:|---:|---:|---:|---:|---:|
| flat 2 k, before | 16,054 | 2,032 | 130,048 | 109,795 | **132,082** | 36,172 |
| striped 2 k, before | 22,399 | 2,109 | 134,976 | 159,557 | **134,976** | 24,508 |

### 2.2 The curves

In-process (the scaling rows as first written — one create per entry; 64 slot trees; height 2
flat, 1 striped):

| row | entries read before → after | per entry visited | nodes before → after | per entry visited |
|---|---|---|---|---|
| flat 2 k | 109,795 → 17,308 | 6.84 → 1.08 | 132,082 → 3,782 | 8.23 → 0.24 |
| flat 8 k | 757,597 → 62,239 | 13.05 → 1.07 | 526,774 → 11,621 | 9.07 → 0.20 |
| striped 2 k | 159,557 → 24,156 | 7.12 → 1.08 | 134,976 → 4,237 | 6.03 → 0.19 |
| striped 8 k | 1,559,080 → 95,519 | 17.75 → 1.09 | 526,592 → 12,696 | 5.99 → 0.14 |

4N / N on reads: flat 6.9 → 3.6, striped 9.8 → 3.95.

The binary (`fsck --offline` of a `--single-writer` set, one flat directory, venue; `scan_ms` is
the engine's wall clock, the process adds the open and the close, ~0.2 s):

| entries | scan_ms before → after | node reads / entry visited | entries read / entry visited |
|---:|---|---|---|
| 2 k | 29 → 8 | 8.1 → 0.23 | 6.8 → 1.08 |
| 8 k | 235 → 34 | 9.1 → 0.20 | 13.0 → 1.07 |
| 32 k | 2,713 → 152 | 9.3 → 0.18 | 36.5 → 1.07 |
| 64 k | 5,736 → 319 | 9.4 → 0.18 | 37.1 → 1.07 |
| 100 k | 9,435 → 510 | 9.4 → 0.18 | 37.1 → 1.07 |

After: 4–5 µs per entry at every size (census 76 ms, dentries 106, C1 59, classes 267 — C11's
per-ino probes the largest — at 100 k). The hot-directory row's closing `fsck --offline --scrub`:
**83.5 s → 0.787 s** for 52,871 entries (99,549 nodes walked); the row 76 s → 35 s.

### 2.3 Online fsck

The online job (`runner.linux.JobBody`) and the fleet's member residues run the same engine over
the same `MetaSet` store, so they take the same walk and the same accounting. Correctness: the merge
returns exactly the records the old merge returned for a forest that keeps the routing law (the
paging-equivalence row), the forest scoping (foreign-live slots leave the forest at the grant and
are walked by neither) is untouched, and the C9 scoping law (`752836b`: `names_complete` declines
the name arms while a slot is foreign-live) reads the same dentry pass. The online rows — `ONLINE
FSCK THROUGH THE ADMIN LANE`, `THE JOBS MATRIX`, the fleet rows, `FSCK AND SCRUB` — are green in
the gates below.

## 3. The committer's frame (a defect the loops found)

Looping the journal-lane storm under 8 × `yes` (item 4) segfaulted once in 100 runs at a stack
address in `conveyor.answer` → `Done.send`: the send put the outcome in the channel, THEN stamped
`acked_ns` — by which time the committer could have taken the outcome and returned, its `Done` (a
frame of its stack) reused, or — a storm thread that had exited — unmapped. In the product the
same store is 8 bytes of silent corruption in a committing thread's or fiber's stack. Red row:
`conveyor` "Done.send writes nothing once the receiver can take the outcome" (the `hold_after_send`
seam holds the sender past the channel send, the receiver poisons its Done, the stamp is found).
Fix: the stamp before the send (a spin hit now means "on its way"; the recv takes it after the
sender's channel lock). 0003 §AC.4.

## 4. The determinism leftovers

| test | the schedule assumption | the fix | loops |
|---|---|---|---|
| `meta.backend … fiber parks per window ≤ 2` | `parks ≤ 2 × windows + 1` counted windows alone; the storm's leaf splits (structural entries) and a barrier cost wakes too, absorbed only while windows coalesced — under load the windows grew (125 instead of 134) and each cost its two wakes: 256 parks > 251 | every park ends on one counted wake: `parks ≤ publishes (windows + structural + barriers) + CQEs (runs pushed — the lane snapshot's new `runs` — + barriers) + structural completion kicks + backstop ticks + rescues + 1` | 8 × `yes`: 3 red + 1 segfault (§3) in 100 before; **200/200** after |
| `exec.lane … 1 ms timer parks under a foreign spawn storm count 0 tick rescues` | the storm ran 300 ms of wall clock: past it the ticker had ended and a producer starved between its push and its wake met a full 2 s tick (a "rescue"); and > 100 spawns had to land inside the 300 ms | the ticker keeps every park timer-bounded until the producer has finished its LAST spawn (push and wake); the producer's 2,000 spawns all land (`hits == spawns`) | 18 × `yes`: **150/150 red** before; 8 × `yes`: **200/200** after; idle 0.33 → 0.23 s |
| `sync.notify` tick accounting, `sync.rwlock` dead waiter, tick backstop | waited real 2 s ticks (≈ 2.5 s per executor each); the backstop test asserted a re-register after a fixed tick + 200 ms sleep; the notify rescue was timed from a read AFTER the waiter registered (a slow main thread ate into the tick — invisible at 2 s, 3/200 red under 8 × `yes` at the shorter tick) | `tick.test_tick_ns` (a test seam; product builds read the constant) driven at `test_scale_tick_ns` = 100 ms; the backstop test waits for the re-register it asserts; the rescue is timed from before the waiter exists (its park's tick starts after that read: a whole tick on any host) | 8 × `yes`: **200/200**; the notify row 4.50 s → 0.34 s, the rwlock rows ≈ 2.5 s → 0.2 s each (cost table) |
| `umount --lazy of a BUSY mount` (smoke) | waited up to three 5 s supervisor probes for the switch to the heartbeat | `SQUEEZEFS_TEST_SUPERVISOR_PROBE_MS` (registry row, read by `Policy.fromEnv` on every `--supervise` mount) at 250 ms; the probe's answer window (`probe_timeout_ns`) and the heartbeat's staleness stay the watchdog tick's | the row 15.5 s → 1.6 s (cost table); green in every gate below |

## 5. Gates

| gate | venue | result | wall |
|---|---|---|---|
| Linux, the code commits, dev's cost tables | venue | 49/49 steps, 2,846 / 2,919 passed (73 skipped) | 6 m 11 s |
| macOS, the same tree | dev box | 29/29 steps, 2,213 / 2,242 passed (29 skipped) | 2 m 57 s |
| Linux, uncached, the final tree (regenerated cost tables) | venue (recreated, below) | 49/49 steps, **2,846 / 2,919 passed (73 skipped)** | **6 m 37 s** |
| macOS, the final tree | dev box | 29/29 steps, **2,238 / 2,267 passed (29 skipped)** | **3 m 00 s** |

In between, four Linux gates on the final tree failed on heavy smoke rows — the shared hot
directory (an intent stuck past the membership TTL after the metadata heap refused a commit; the
joiner's grant-closure and the recovery's extent tripwires once each in standalone runs) and the
block-map tree (`lock_ticked_reregisters` 2 with its 1.5 GiB rewrite at 47 MB/s against its usual
142–187) — while the host's disk filled: `/build` is a sparse image whose freed blocks return only
on `fstrim`, the session's curves, loops and gates grew it to 116 MiB free on the host, and the
venue's root fs went `emergency_ro` (the last two gates: every socket/memfd e2e `OpenFailed`).
After `fstrim /build` (107 GiB returned) and the container's recreation the uncached gate is
green; the hot-directory row passed 12 of 14 standalone runs on this branch (both failures in
that window) and 13 of 13 on pristine `dev`, interleaved, idle and under 4 × `yes`. An auto-trim
monitor (`fstrim /build` below 40 GiB free on the host) now runs beside the session.

## 6. Owed

- C11's owner skip-scan probes once per ino (one descent each): linear, the largest pass after the
  fix; a paged owner scan would make it O(N / page) descents.
- Damage-proportional in-memory scans (not measured, not walks): C9's per-candidate owner lookup
  and C2-lost's `firstOwner` scan the census's owners per candidate (O(candidates × owners)); C17's
  `verifyStripes` loops every view per striped directory (O(D_striped × views × K)); `dedupedNames`
  is quadratic in the collected names. Each is linear with an index built once; they need a
  counter-based red row first.
