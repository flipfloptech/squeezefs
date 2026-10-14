# Rung 10 — the coherence defects §9.8 of the dev-perf note owed (2026-10-14)

The five items `rung-10-dev-perf.md` left open: §9.4's "host's disk filled" paragraph (an EIO under
the handover MOVE MATRIX panicking in the allocator) and §9.8's "Owed" and "not this lever's"
paragraphs (the negative-dentry coherence question AB.2 left open, the macOS online fsck false
positive, `KmbufRingEnomem` after hours of gates, the `-o ro` reader's refused remount). Decision
0003 §AE. Every row below was red before its fix on the tree it landed on.

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
