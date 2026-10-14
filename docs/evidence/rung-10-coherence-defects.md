# Rung 10 — the coherence defects §9.8 of the dev-perf note owed (2026-10-14)

The five items `rung-10-dev-perf.md` left open: §9.4's "host's disk filled" paragraph (an EIO under
the handover MOVE MATRIX panicking in the allocator) and §9.8's "Owed" and "not this lever's"
paragraphs (the negative-dentry coherence question AB.2 left open, the macOS online fsck false
positive, `KmbufRingEnomem` after hours of gates, the `-o ro` reader's refused remount). Decision
0003 §AG. Every row below was red before its fix on the tree it landed on.

## 1. A device EIO under the handover move matrix panicked instead of failing loud

**What §9.4 saw.** In the window the Mac's disk was full (the venue's sparse images answering EIO),
a venue gate's handover MOVE MATRIX died with `incorrect alignment` in the allocator after a node
write's EIO; the same binary passed 12/12 on a healthy disk. The gate's log did not survive, so the
matrix was swept again under injected failures: a temporary harness ran every step × op of the
matrix (`offered`, `drained`, `granted`, `reopened` × create, chmod, unlink, rename, setattr) with
every metadata write failing — the node writes (a seam in `device_loader`), the journal windows and
every barrier (`journal_lane.test_seams`, `device_loader.test_seams`) — armed at four phases (before
A's open, before A's tree, before B's join, at the handover step), five pass counts each.

**Root cause 1 — a transaction freed twice.** Every store's `commit(t)` CONSUMES the transaction on
every path, its refusals included (`KvBackend.commit` destroys it in a `defer`; `MetaSet`,
`MemStore`, the fsck stores alike). The writer claim's commit at open (`gate.Guard.commitClaim`)
staged under `errdefer g.be.abandon(t)` and kept that errdefer armed across `g.be.commit(t) catch
|e| return …`: a failed commit (a journal window's EIO) freed the transaction, then the errdefer
deinit-ed the freed memory and called through its guard set's function pointer — `Bus error at
0x5555…` from `tx.deinit` in this run; with the Debug allocator's `0xaa` poison in the freed
transaction's `gpa` field the same call is `@alignCast`'s `incorrect alignment` inside
`DebugAllocator.free` — §9.4's panic. Phase 0 of the sweep (every write failing from A's open)
crashed this way on every pass. The pattern was not alone: 33 sites held an unconditional
`errdefer <store>.abandon(t)` across the `commit(t)` that consumes `t` — the claim's clear
(`gate.zig`), the fsck CLI's injection seam, the fsck repair engine (six actions), the striped-file
seam of `kv_fs`, and the fsck harnesses (`ram_set` 16, `tests` 5, `scale` 1). One more defect sat in
the repair engine's `flip-damaged` action: a publish-law refusal returned an outcome (not an error),
so its errdefer never ran and the transaction leaked.

**Root cause 2 — the intent register overran its reserved map.** With root cause 1 fixed, phase 3
(every write failing at the handover step) panicked `integer overflow` in
`hash_map.getOrPutAssumeCapacity` under `intent.Register.noteInFlight` on every pass. A cross-owner
op registers its intent IN FLIGHT before step 0's commit; when that commit failed (the record never
durable) `leaveOpen` marked the entry ABANDONED — a ghost only a cadence scan reconciles — and the
move gate re-issues an op refused by a move class until the lattice's deadline, minting one intent
per attempt: the 1,025th insert overran the register's 1,024-entry reservation, a panic in a FUSE
handler.

**Fixes.**
- The 33 sites close the errdefer's block before the commit (`{ errdefer abandon(t); stage… }
  commit(t)`); `flip-damaged` abandons its transaction before answering the refusal.
- **The rail** (`tests/rails.zig` "tx ownership rail"): an unconditional `errdefer X.abandon(t);`
  whose enclosing block reaches a `commit…(t)` of the same transaction is red (tokenizer scan;
  `errdefer if (owned) …` and a closed staging block pass). It listed the 33 sites before the fix.
- The register forgets an intent whose record never became durable (`ops.leaveOpen(…, durable,
  …)`); an insert past the reservation refuses (`Register.InsertError.RegisterFull` →
  `error.Deferred` to the op, `xv_register_full_refusals` counted; the cadence's adoption ends that
  home's pass and resumes at the next).

**Red rows.**
- `gate` "the writer claim's commit failing EIO at open refuses the mount loud (Io), its transaction
  freed exactly once …": one injected journal-window EIO at open. Red: `terminated with signal ABRT`
  (`Bus error at 0x55555555555555`, `tx.zig:194 deinit` ← `backend.abandon` ← `commitClaim`'s
  errdefer). Green: `error.Io`, the refusal's one error line, no claim landed, the next open claims.
- `two_holders` "an intent whose step-0 commit fails EIO (the record never durable) leaves NOTHING
  on the register …": 2,048 creates into a foreign directory, each step-0 commit failing EIO (a
  `fail_commits` knob on the harness store). Red: `expected 0, found 1` (the ghost) after the first;
  green: the register empty after every attempt, the closure intact, the next create lands; a
  2-entry register refuses the third insert and an adoption, counted.
- The rail row and its scanner row (`tests/rails.zig`).

After both fixes the sweep (20 combinations × 5 pass counts × 4 phases, node writes alone and every
write) ran with no panic: creates, chmods and setattrs answer EIO at once, the journal latch engages
(`3 consecutive journal_write failures — latching the volume FAILED`), the checkpointer retries per
tick and the device funnel logs every failure. A cross-holder unlink or rename whose slot was
mid-move when the device failed parks on the move gate until the fail-stop lattice's deadline
(90 s, below the kernel's 135 s request timeout) and answers EIO — bounded and loud, the designed
lattice.

**The sibling paths, swept.**
- `ring_park` fdatasync answering −5: the ring_park unit row "the barrier on the same ring"
  asserts `cqe.res == 0` for an fdatasync of a file on `/build` — the venue disk itself answered
  EIO in that window; the CQE carried the device's errno to the assertion exactly. No product path
  is involved and nothing to fix.
- The cache-path data smoke's FUSE_WRITE leases living 2–3.5 s behind EIO'd staging writes: the
  overlay's ACK-early continuation and the write arm's settle retry every device error but the fence
  on the never-lossy ladders (`ACK_EARLY_LADDER` to 1 s, `SETTLE_LADDER` to 50 ms; acked custody is
  never dropped — charter §1.10), and a write to a block whose record is settling waits on that
  settle with its payload lease held. The window's EIO was transient (the host disk recovered): the
  writes landed when it did, and the 1 s lease rule logged each long lease
  (`transport_lease_overlong`) and the ladder barked — the loud, never-lossy outcome by design. A
  data device that never recovers holds such a write until the kernel's request timeout fail-stops
  the connection (decision 0003 §Z). Not changed.
- A device EIO at a holder answers the wire's `refused` with EIO, and the must-stay-0 tripwires
  that count a holder's device failures on a healthy fleet (`cluster_wire_service_refusals`,
  `record_refusals`, `xv_midplan_escalations` — "the fail-stop class: must stay 0") fired in the
  sweep, loud as designed. Not changed.

## 2. The negative-dentry coherence question (AB.2's open question)

**What §9.8 saw.** The smoke `INVALIDATION COHERENCE ACROSS DAEMONS` second step (B's kernel holds a
negative `d/y`, A creates `d/y`, B looks again) read `no` once, in a loaded cached gate. Two
mechanisms answer `no` there, and the row could not tell them apart:

- **No token, no recall.** AB.2's law reaches a peer's kernel through the recall, and a holder
  recalls only the readers its table lists. A reader's token cache RELEASES an entry when it makes
  room (`evictToBudget` → `Release`): from then on the holder's changes to the object recall nobody
  there, while the reader's kernel may still hold entries answered from that token — a negative
  `d/y`, a positive `d/x`, attributes — for their whole TTL (1 s peered, 60 s in the row). The
  token was the coherence boundary and the kernel's cache outlived it.
- **Notify speed.** The reader's recall service QUEUES each changed name on its sideband before it
  acks (`notifyEntryOrEpoch`, fire-and-forget: the `INVAL_ENTRY` write takes the parent's
  `i_rwsem`, which a request in flight may hold while it waits on this very recall — the ack cannot
  wait for the write). The creator's syscall returns after the ack, so the reader's kernel learns
  the name one notifier round later. The row asserted at once and raced that round.

**The law (decision 0003 §AG.2; 0002 row 527).** A kernel cache entry answered under a token never outlives the
token's coherence: every serve stamps its entry `kernel_pin_until = now + the mount's longest
kernel TTL` (`Divert.setKernelPin(peered_ttls.longestNs())`, set by the mount before it serves);
room-making DROPS a kernel-pinned entry without a release (`Cache.dropOrRetireLocked`,
`dlm_token_kernel_pinned_drops`) — the holder keeps listing this reader, so a change still recalls
it, and the recall's names and objects reach the kernel exactly as for a held entry — and releases
only an entry past its pin (the holder's table stays the reader's live interest set; the memory
bound is unchanged: a drop frees the entry's bytes like a release). The authority's shed was
already a drop. Until a mount names its TTL the pin is unbounded (coherent by default: one needless
recall per dropped object, never a stale kernel entry). Of the two shapes the task named, this is
(a) — the negative is cached only while the node holds the parent's token, the token KEPT at the
holder for the TTL — realized on the token table that already is the per-parent reader set, with
no new structure and no wire change. "At notify speed" is a peer's change visible once the reader's
notifier wrote it: `transport_notify_pending` (a gauge: fire-and-forget sideband writes filled and
not yet answered by the kernel) is what the change still owes.

**Red rows.**
- `tokens` "a reader whose kernel may still cache an answer served under a directory's token keeps
  hearing the holder's changes after its cache EVICTS the token …": B serves directory 2's dentries
  under a 60 s pin, its cache makes room for the file's token, A creates `five`. Red without the
  pin law: `expected 1, found 0` (no pinned drop: the token was released, the holder recalls
  nobody, the name never reaches B's sink); green: dropped not released, the name noted; with a 0
  pin the next eviction releases and A's next change recalls nobody.
- `token_cache` "the kernel pin …": one pinned and one cold entry, room for both — the pinned one
  drops (counted), the cold one is retired for release.
- The smoke row now runs B with `SQUEEZEFS_TEST_TOKEN_EVICT_AFTER_SERVE` (every foreign serve's
  entry evicted under the room law at once: B holds NO entry when A's change lands) and every
  step's judgement after A's change waits — bounded, 5 s — for B's `transport_notify_pending` to
  read 0, then asserts at once. Red without the pin law: the second step's `yes` (smoke.zig:1960 —
  B's negative stood). Green: **20/20** consecutive venue runs; B's recalled names 5, epochs 0,
  `dlm_token_kernel_pinned_drops` ≥ 4.

## 5. The `-o ro` reader's remount refused by the home shard's join

**What §9.8 saw.** The smoke `THE -o ro READER OF A LIVE SET`: after A and B left and A mounted
again, C's remount (`-o ro`) was refused by the home shard's join, once.

**Root cause.** A manager's successor arms its membership shard with the predecessor's writer
roster — every `member:` row in tree 0 — as the grace window's expectation; while any expected
member has not re-asserted, the window's fresh half refuses every stranger (`grace_refused`,
`MembershipPending`), and both the reader plane and a joiner refuse the MOUNT on it. A row outlives
its member when the clean leave does not land: `Member.leave` is best effort (a failed call is
ignored) and the owner's row delete is too (`a departed member's row could not be deleted`). B's row
survived, A's successor expected B — a process that had exited and could never re-assert — and the
fresh half held C out for the whole window. Reproduced deterministically with
`SQUEEZEFS_TEST_MEMBERSHIP_LEAVE_LOST` (B's clean stop sends no leave): C's remount answered
`reader plane: the home shard refused the reader's join (grace_refused)` →
`the reader plane could not arm` — §9.8's line.

**Fix (decision 0003 §AG.5).** The successor's roster applies the clients classifier's dead-pid
proof: a same-host row whose pid is proven dead (`MemberRow.pidAlive` false — the pid absent, or
another incarnation of it) is not expected (`MemberRow.expectsReassertion`,
`membership_grace_dead_rows`); its next mount is a new incarnation (recovered through the death
ledger), never a re-assertion. A live row, another host's row (unknowable here) and an undecodable
one stay expected. This is the reference's law the port had missed: its roster PRUNES same-host
provably-dead writer entries (`src/membership.rs` :464, "Rung-8 finding #3"), so no 0002 row. A lost leave of a member on ANOTHER host still holds the fresh half to the
deadline — the window the grace exists for; the mount's typed refusal names it ("retry the mount").

**Red rows.**
- The smoke row now runs B with the lost-leave seam and asserts A's successor excluded B's dead
  row (`membership_grace_dead_rows ≥ 1`) before C's remount. Red without the exclusion (the counter
  bumped, the row still expected): C's remount refused `grace_refused` (smoke.zig:4803, C exited
  code 1). Green with it.
- `shard` "a successor's grace expects no same-host predecessor whose pid is proven dead …": a dead
  row is not expected, a live one, another host's and a torn one are; a roster of dead rows opens no
  grace and a reader's fresh join is granted, where the roster that expected the dead row refuses
  the same reader.

## 4. `KmbufRingEnomem` after hours of gates

**What §9.8 saw.** Daemon A's mount refused on `KmbufRingEnomem` after four hours of gates; the
note read the refusal's own text ("order-8 contiguous pages") as "no order-8 pages free".

**What the kernel does (the evidence).** The venue kernel's kmbuf region allocator (the reference
recipe `44752ce5`, `docker/kernel-sqz/patches-7.2/0003-io_uring-kbuf-add-support-for-kernel-managed-buffer-.patch`,
`io_region_allocate_pages_multi_buf`) tries ONE compound block per buffer (`io_mem_alloc_compound`)
and, when no block of that order is free, falls back to `buf_size / PAGE_SIZE` order-0 pages
(`alloc_pages_bulk_node`). A fragmented page cache therefore never fails the registration, and
"more, smaller buffers" would ask for exactly the same order-0 pages: there is no smaller grain to
fall back to that changes the outcome. The ENOMEM is (a) the bulk fallback coming up short — it
draws on the free lists without reclaim and is all-or-nothing per region (one queue's region is
`ring_entries × buf_size`: 32 × 1 MiB = 8,192 pages on this venue), so free memory short of the
region while gigabytes are merely reclaimable refuses it — the "ample `available`" capture of
2026-08-10 the retry ladder's comment records — or (b) `__io_account_mem`'s `RLIMIT_MEMLOCK`
charge when the ring was created without `CAP_IPC_LOCK`. Separately, a smaller per-buffer order
could not be adopted here anyway: one FUSE request's payload lands in ONE buffer, and the INIT
reply commits `max_write` before the workers register their rings.

**Fix (decision 0003 §AG.4).** The refusal names the real resource and the levers: the region's
pages at once, the buffers × size, the no-reclaim order-0 fallback, and what shrinks the region
(`SQUEEZEFS_FUSE_MAX_WRITE` the buffer, `SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH` the buffers per
queue) or avoids it (`SQUEEZEFS_FUSE_BUFFER_MODE=user`), plus the memlock lever when the process
lacks `CAP_IPC_LOCK` (`CapEff` bit 14, `probes.capBitFromStatus`); the worker's log line says the
same. `kmbuf.pageOrder` (the false premise) is gone. **Owed, the substrate:** a kernel patch that
completes a short bulk fallback with per-page allocations (which reclaim) would make the mount
succeed whenever the memory is reclaimable — it needs a venue kernel rebuild and swap, not done
here.

**Red row.** `session` e2e "the kmbuf ring answering ENOMEM after every retry (the seam) …" now
asserts the region's pages (`regionPages(buffers, buf_size, page)`) and every lever in the detail
and that it never claims a contiguous-page shortage — red against the old detail
(`kmbuf ring registration ENOMEM (order-8 contiguous pages); retry, or …`). `kmbuf` "regionPages".

## 3. The macOS online fsck false positive under a live writer storm

**What §9.8 saw.** `fsck` "zero false positives: a live writer storm …" reported
`[C8] durable refs 0 vs derived 1` on one shard of one macOS gate.

**Reproduced.** The test binary looped on macOS: 0/100 on an idle Mac, **11/60 on pristine `dev`
and 8/60 on the item-1 tree under 14 × `yes`** (the earlier 50/200 ran beside a gate) — 46 of the
first 50 failures `[C8] … durable refs N vs derived M`, 4 `[C2] leaked`.

**Root cause.** C8's Phase C re-checked a suspect with the WHOLE-VIEW comparison: a fresh census
(one walk of every layout), then a fresh refs scan (a second walk), compared per block. The storm
moves a block's reference and its layout together in ONE tx under the owner's exclusive lease
(the publish: layout + `taken`; the destroy: `released` + the layout's destroy) and re-mints a freed
block to the next file at once — so a destroy between the two re-check walks reads `durable 0 vs
derived 1`, a publish `1 vs 0`, exactly as in the first round: the "confirmation" re-ran the race.

**Fix (decision 0003 §AG.3).** C8's Phase C is per block and consistent: the candidate owners
(the first round's census owners binding the block, plus the owners its durable references name
now) are locked TOGETHER under their exclusive 4a leases (`Host.lockInos`, one canonical
acquisition), the block's durable references are re-read under them — an owner that appeared
since (a re-mint) joins the set and the round repeats, bounded at 4 (`C8_SETTLE_ROUNDS`; past it
the suspect clears, counted `c8_owner_set_moving`) — and every candidate's CURRENT bindings
(`Host.currentBindings`) are counted under the same leases. A writer's tx on the block cannot fall
between the two reads. An owner set wider than one lock plan, or a layout the codec refuses, keeps
the whole-view comparison.

**Red row.** `fsck` "C8's online re-check reads a block's durable references and its owners'
layouts in ONE state …" schedules the race through the pass's C8 hook (`Host.c8_hook`): the owner of
block 40 destroyed between the first round's census and refs scan (the suspect), the block
re-minted to a second owner as the re-check starts, that owner destroyed between the re-check's
two reads try-only (a writer meeting the owner's lease). Red on the whole-view re-check:
`FALSE POSITIVE: [C8] vol-0000000000000010 block 40 — durable refs 0 vs derived 1` — the gate's
line. Green: the re-check holds the second owner's lease, the destroy waits (it lands after the run,
and the next run is clean), the suspect clears.

**Loops after the fix** (the storm test, macOS, 14 × `yes`): **0/60** C8 (was 11/60 on pristine
`dev`); a 150-run loop: **0 C8, 4 `[C2] leaked`**.

**Owed — the second window, `[C2] leaked`.** A release's tx (the destroy: `released` + the layout's
destroy) commits, and the block's terminal free (`freeBlock` / `Reclaimer.freeBlock`) runs after it
— in the product exactly as in the storm. Between them the allocator still tracks a block nothing
references, and the owner that could be locked is gone with its records: a census and its re-check
that both fall inside that window (the destroyer preempted for their whole length) confirm a leak
that the free resolves a moment later. Census scoping cannot close it — no record names the owner.
The shield is the producer's: the in-flight registry (`InflightAllocGuard`, Phase A's
`inflightContains`, today held only from a mint to its publish) held from before every releasing
commit until its terminal free, at every release site (the destroy and corpse sweep, truncate, the
overwrite's old block, the pack release). Not done here; 4/150 under 14 × `yes` (0 in 100 idle).

## The queued sideband abort (`Sideband.completeCell → recycle`, `session.zig:642` on `5ababa8`)

Not reproduced, and no log of it was reachable (none under `/build/tmp`, `/build` or the Mac's
`/tmp`). On `5ababa8` line 642 is `completeCell`'s `self.recycle(idx)`: the abort is `recycle`'s
`assert(free.tryPush(idx))` (or `nb_free`'s) — a cell recycled while already free, so its queue
fills one push early. Every path that recycles a cell was audited: the fire-and-forget CQE (one
recycle per write), a blocking reply (its waiter recycles after `done`; the loop never does), the
no-SQE `EAGAIN` completion, `finish()`'s drains (each cell either in flight or in `ready`, never
both), `fillNotify`'s own recycle on a refused frame (its callers do not recycle again — except
`writeOwedEpoch`'s `catch return self.recycle(idx)`, a double recycle reachable only when
`notifyIncEpoch` refuses a frame, which it cannot: a 16 B header into a cell). The SQ cannot
overflow (`sidebandEntries(cells)` = 2 × the power-of-two pool ≥ pool + the 8 reserved cells + 3).
Owed: the next occurrence's stack and test name; the item-2 change (`notify_pending`) adds a counter
on these paths and moves no recycle.

## Gates

Item 1 (`5ababa8`): venue 49/49 steps, 2,870 / 2,943 passed (73 skipped), 9 m 43 s; macOS 29/29,
2,258 / 2,287 (29 skipped). Item 2 (`695973b`, rebased over the symdefects agent's membership
change): venue 49/49, 2,875 / 2,948 (73 skipped), 8 m 21 s; macOS 29/29, 2,263 / 2,292 (29
skipped); an earlier run of the same change failed the cache-path data smoke's promotion count
(`smoke.zig:6329`, `promotions ≥ 1 + 1 + storm_files`) under load and passed 3/3 alone and on the
rerun. Items 5, 4 and 3 (`26f6c15`, `db3ae04`, `9259378`, one tree): venue 49/49, 2,877 / 2,950
(73 skipped), 8 m 49 s; macOS 29/29 (2,262 / 2,294 with 29 skipped on the full run). Their first
runs met load: the venue's unit compile was OOM-killed (`signal KILL`, load 10) and the cache-path
data smoke's promotion count failed again (`smoke.zig:6352`); on macOS (load 18–20 from parallel
gates) the write-pipeline admission row, the conveyor's §L window-fill row and the fsck storm's
`[C2] leaked` (the owed window above) failed; every one passed on the rerun of the same tree.

