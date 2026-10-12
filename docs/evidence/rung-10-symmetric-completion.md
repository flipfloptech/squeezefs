# Rung 10 — the symmetric plane's owed items closed

Charter §5 (the symmetric plane: appenders, the manager lease, slot leases,
the appender directory, KVDA, the death ledger; §5.4 `enable-symmetric`;
§5.9 recovery). The owed list this rung inherited is
`docs/evidence/rung-06-symmetric-plane.md` "Not done — the owed list",
`rung-08-operations.md` (the `enable-symmetric` recipe),
`rung-09-brackets.md` F17 "Not done / owed" (the joiner-side grant ledger)
and the meta512 agent's note on the handover dropping parked stamps. Every
item landed red-first; the decision rows are 0002 rows 483–485 and 492 and
0003 §N ("Rung-10 design calls"). Venue: the macOS `container`
(`7.2.6-sqz`, 9 vCPU, 16 GiB), shared live with the `.wt-ops` and `.wt-mem`
agents' gates.

## 1. `volume enable-symmetric` (charter §5.4) — landed `69bf78a`

**Design.** An offline conversion of a `--single-writer` volume, in place:
the door is the flock plus a fresh foreign writer claim (both refusals, typed
and loud); a non-empty replay window refuses `WindowNotEmpty` naming the
remedy (mount and unmount cleanly — the verb never replays); an already
symmetric volume refuses typed. The appender-directory link is claimed at the
lowest FREE heap extent (never extent 0 — a flat volume's extent 0 holds a
tree); the fixed ring is zeroed end to end and re-based under appender 0's
four page slots; the bitmap and the restated ledger land at the next
generation BEFORE the superblock stamp (`symmetric_plane` + `appender_dir` in
ONE A/B write, the stamp last — a crash between leaves a flat volume with one
claimed KVAD-headed extent the re-run finds by CONTENT and resumes onto). The
next `mount` is the manager; later RW mounts join it over the wire.

**Red tests.** `enable_symmetric` unit rows (the three refusals, the
offline conversion, the resume by content); the smoke stage "enable-symmetric
…: flat tree → convert → the manager → B joins → fsck clean" (`cli/smoke.zig`),
0 findings from `fsck --offline --scrub` over the converted volume after the
two-daemon pass.

## 2. The KVDA page sink (0002 row 292) — landed `9fa2d0d`

**Design.** The holder's bitmap pages live in claimed extents of its home
volume (`claimInternal` + kind-4 `alloc` deltas in ONE ring-0 control entry —
the format's and `growDirectory`'s discipline), published in `alloc_lease:`.
Every carve's SET and every shipped free's CLEAR is a kind-5 delta of ONE
ring-0 control entry BEFORE the reply; the dirty pages are rewritten at the
checkpoint cycle's C6 before barrier #1 (`RecordSource.dataPages`),
newest-valid-wins per page slot. The mount's replay keeps every kind-5 frame
of the fixed ring (`KvBackend.kept_data_deltas`) because the bring-up
checkpoint advances the tail past them; the same-node successor folds that
KEPT WINDOW onto the predecessor's pages (`foldOntoRegion`) and LOADS them at
its arm, deferring what the pages carry beyond the refs census to the live
writers' declarations: a declared SET candidate is adopted, a CLEAR one
re-learnt (`block_grant_relearned_from_windows` — 0 with the pages, the
unconsumed window without them). The renewal declares the writer's windows
(member side) and the holder folds them (screen → adopt/relearn →
`syncWriter`). Deferred candidates are RELEASED only under the sole writer's
exact census (every peer departed or recovered: `live == 0 ∧ foreignSlots ==
0`); while peers are live the remainder is reported once and left SET for
fsck C6 (0002 row 484). The block-refs range is k-way merged over every slot
tree (the forest's census, 0002 row 485: the port had routed a refs range by
its bounds' owner ino to the native tree alone, so the census missed every
guest's refs).

Two defects the gate surfaced on the first full run were fixed inside the
item: (a) the holder's grant drained AFTER it carved, so a free storm's
per-block CLEARs (a truncate's 1,436 frees against a 192-entry ring) left
the ring full and the next carve's SET fired `data_alloc_bitmap_set_dropped`
— the grant now drains BEFORE the carve, a ring still full defers the grant
without moving a bit (the writer's retry class), and the ring coalesces a
run's continuation (a truncate's frees and a sequential carve are one
record each); (b) `convergeDeferred` released on `all_declared` and the
"pages held" row showed `released 50` — the release is now the exact census
alone.

**Red tests / kill rows.** `data_bitmap`: "THE PAGE SINK's two device acts",
"the ring coalesces a run's continuation", the fold rows (wrong term → 0;
dead term → 66 bits / 2 pages; empty window → 0); `block_grant`: "the grant
drains BEFORE it carves", the deferred-leak protocol rows; `plane_arm`
arm/declare rows; the two smoke kill rows "THE KVDA PAGE SINK — … (pages
held)" and "… 's fallback — (pages SKIPPED, the seam)": holder dies with 3
grants / 192 blocks outstanding → the successor loads 2 pages / folds 2 /
192 bits, adopts 142 from B's declaration, re-learns 0 (fallback: loads 0,
re-learns 142); both shas equal from A; after B's leave the sole writer's
census converged the remainder (referenced 50, released 0); fsck
C6/C13 clean.

**Counters.** `block_grant_relearned_from_windows` 142 → 0 with the pages;
`data_alloc_bitmap_successor_loads` 2, `_predecessor_folds` 2,
`_bits_loaded` 192; `data_alloc_bitmap_leaks_adopted` 142;
`data_alloc_bitmap_set_dropped` fired 1 on the block-map row before the
drain-before-carve, 0 after.

## 3. The joiner-side grant ledger (F17's sibling) — landed `2e37fea`

**Design.** The member's renewal beat declares its held windows (the
unconsumed window + the in-flight registrations per data volume,
`RenewReq.encodeWindows`); the owner service screens them against the
plane's volumes (`screenWindowDecl`) and the holder folds them
(`adoptDeclared` then `syncWriter` under `decl_lock`) so every CONSUMED grant
retires at the declaration — the same `table_full ⇒ Deferred` class that
caused F17 on the holder closes on the joiner side. The ledger capacity has a
test seam (`SQUEEZEFS_TEST_BLOCK_GRANT_LEDGER_CAP`, 2…1024) so a row drives a
joiner three times past the cap.

**Red test.** "smoke: THE JOINER-SIDE GRANT LEDGER — the holder's ledger
capped at 8 (the seam), 1 MiB blocks; B writes 24 files of one floor grant
each": 26 grants carved, consumed retired 24, `open_grants` peaked at 3–4
(≤ the cap), `block_grant_deferred` 0 at A, `topup_deferred` /
`exhausted` / `topup_refused` 0 at B, every file whole from A. With the
declaration withheld (the red run): deferred 31. The retire assertion is the
structural bound `retired ≥ files − cap` (the table holds ≤ cap open grants,
so every grant past the cap had to retire for `deferred` to read 0); the
declaration counts are asserted ≥ 1 on both sides (one per renewal beat per
volume — a count, never a per-file law; the first gate run caught the
per-file version as a timing flake).

## 4. The handover drains parked write stamps — `04fa094`

**Design.** Step 0 of `handover.transfer` is now the filesystem's
SLOT-SCOPED times drain: `PendingTimes.parkedOfSlot` collects the departing
slot's parked inos, `kv_fs.drainPendingTimesOfSlot` commits them as Δ
entries of `dlm.max_requests` inos each, before the gate (the door still
admits the holder's commits), through the plane's `TimesDrain` seam bound at
`kv_fs.init` (`MetaSet.bindTimesDrain`; a `-o ro` reader has no slot plane
and binds nothing — the first gate run aborted the reader there, pinned by a
regression row). Counted `meta_kv_times_handover_drained`; the drop counter
`meta_kv_times_echo_foreign_dropped` is what the row holds at 0.

The holder-decided handover over the WIRE did not fire before this item (the
judge was fed by in-process door refusals alone; the rung-6 owed "shared-dir
handover row"): every served step and record verb now feeds the slot's judge
with the peer's appender id (`Manager.appenderOfIdentity`: a RAM map by the
stable identity `(node, mount slot)` — the part a wire peer id carries — kept
by the join and the leave, re-read from the directory's Live pages once on a
miss); the judge reads the holder's ops NET of the slot's served ships
(`DominanceWindow.noteServed` — a served apply commits under the holder's own
door and the door counts it as a holder op, so the rule could never hold
gross); an offer to a wire requester rides its next renewal grant
(`Verbs.offer_push` → `Owner.pushOffer`, `slot_offers_pushed`); the joiner's
existing `carriageOffered` accepts with `AcquireSlot`. 0002 row 492, 0003 §N
(four rows).

**Red tests.** `bind.zig` "the handover's step 0 drains the departing slot's
parked stamps": a bound `TimesDrain` is called once, for the slot handed
over, while the door still ADMITS the holder's commit (the probe enters and
backs out), its count credited; unbound drains nothing. `kv_fs`
"drainPendingTimesOfSlot commits the parked stamps of ONE routing slot in one
Δ entry": 4 stamps → one commit, a guest-slot stamp untouched, the slot again
→ 0 commits, the seam with its own op context. `handover.zig` "a SERVED wire
ship … the judge reads the holder's ops net of the slot's served ships":
13 ships each counted at the door still reach `offer_dominated` (gross 16,
net 3); the holder's own 20 ops still defend. `meta_set.zig` "a `-o ro`
reader … the handover binds are no-ops" (red: a segfault at
`bindTimesDrain`). The two-daemon row "smoke: THE HANDOVER DRAINS PARKED
STAMPS": A (the manager, `SQUEEZEFS_META_FLUSH_INTERVAL_MS` 60 000 — the
times-drain cadence IS the flush interval, held a minute away so the stamp
is still parked when the handover runs; B joins by `--join` since A's tree-0
endpoint record rides the same cadence) mints 72 files over its rotor (past
M ≤ 64: the pigeonhole puts two on one slot), parks a size-neutral write's
mtime on one, B's 48 setattr ships on the OTHER file of that slot dominate
(`slot_ships` 48, `n_floor` 17–21, offers 1 dominated, pushed 1) → the
handover (`slot_handovers` 1) → step 0 drained 1 stamp → A's ordinary drain
(`sync d` → `fsyncdir`) after the move drops nothing → B serves exactly the
mtime A had promised (`stat -c %.9Y` equal on both). Red without step 0 (the
probe `if (slot != 0xFFFF) return;`): drained 0 at the first assertion. A
served op on the parked file ITSELF commits its record and folds the stamp
in (the ordinary retire) — the row's first shape found that and moved the
ships to the slot's other file.

**Counters.** `meta_kv_times_echo_foreign_dropped` 0 on both daemons across
the handover; `meta_kv_times_handover_drained` 1; `slot_offers_pushed` 1;
`slot_handovers` 1; `record_served` 48 = `slot_ships` 48.

## 5. Rung-6 leftovers, in order

### 5a. The parked joiner's `.stats` under a dead manager — `19f985a`

The token divert remembers the root's attributes as last served under a
token (`Divert.last_root`, kept past the cache's shed on a lapsed lease);
`kv_fs.getattr(1)` serves them when the root's holder refuses retryable
(`HolderUnreachable` / `MembershipPending` / `TokenChannelStale`) — the
exemption the `-o ro` fail-stop gate grants `getattr(1)`: a stale root
attribute misleads no reader, the root pin never moves. Counted
`dlm_token_root_served_last`; any other ino refuses as before. Red rows:
`tokens.zig` "the parked joiner's root … past T_self the serve refuses
MembershipPending and the cache is shed, yet `lastRoot` still answers them";
`kv_fs` "getattr(1) whose token serve refuses retryable is served off the
divert's last root entry … with no root ever served the refusal stands
(EAGAIN); another ino never falls back". Kill row (b) reads the parked
joiner's `.stats` and asserts the parked word (`appender_parked` 1). Finding
along the way: while the member lease still stands B serves the root from
its cached token, so the fallback engages once the lease lapses or the
channel fails — the kill row cannot wait for that without outliving
`T_park_max`, which is why its witness is the parked word and the unit rows
carry the exact contract.

### 5b. `slot_tails` past the inline cap — `928a89b`

`slots/tails_spill.zig`: a release whose flushed-tail set exceeds the tree-0
value cap writes the entry images into heap extents claimed in the internal
class (one run per extent, `node_size / 16` entries each), barriered BEFORE
the entry that names them; the record is `spilled{g, runs, hash64 over the
images}` with the extents' `alloc` deltas in the SAME entry; a superseded
spilled record's extents ride the superseding entry as `free` deltas. The
plane's arm folds a spilled set back through the volume's page reader
(images failing their hash read as ABSENT — `slot_tails_spill_unreadable`,
the leaves screen by the lease alone); the recovery's step 7 spills through
the same core (`volume_bind` binds it; a RAM fixture without a spill home
refuses as before); a spill the heap refuses lands the release WITHOUT its
tails (`slot_tails_spill_refused`, loud). Red rows: `tails_spill` "600
entries at 4 KiB nodes → 3 extents, the read folds every entry back in
order, a flipped byte reads as ABSENT (the tripwire)", "a spill the heap
cannot hold claims nothing durable"; `acquire.zig` "ReleaseSlot past the
inline cap SPILLS: 1,100 tails at the 64 KiB node (cap 1,039) → one extent,
every tail indexed, the next inline release gives the extent back". 0003 §N
row.

### 5c. The paged-resident dentry set (spec Q13) — `8943326`

A directory's token entry holds ONE cookie WINDOW: the records strictly after
the serve's `after` up to the set's end or `dirWindowBytes = max(records
budget / 4, one wire page)`, the cut INSIDE a wire page (at least one record
per window); the probe hits only a window covering the wanted cookie
(`coversAfter`), a miss fetches the next entry version from the window's
end (the old one pinned until released). `kv_fs.findDentry` asks for the
window around the name's cookies (`Wants.forName(hash54)` — the cookies ARE
the dentry keys, ordered by the name's hash) and judges absence by
`coversName`, re-serving from the window's end past a cut mid-run; readdir
and the striping pager resume per window (`dir_complete` ends the reply; the
kernel's next call pages on). A directory larger than the budget is never
refused; `dlm_token_dir_windows` counts the cuts. Red row: `tokens.zig`
"the paged-resident dentry set: 5,000 dentries under a 1,500-record window
walk in ≥ 2 windows in cookie order, every record once; a lookup lands in
one targeted window and hits the cache after; absence judged; oversize
refusals 0". 0003 §N row.

### 5d. The dead recoverer's stashed interior records — NOT DONE, stops here

The hazard (spec `sym_wire_membership_recovery.md` step 4, REC:1733-1784):
manager M1 recovering appender X's region journals its SMOs on X's slot
tree into ring 0; M1 dies mid-recovery (X's page `Recovering`); successor
M2's open replays ring 0 and `openGuestTrees` opens X's slot tree at tree 0's
grant-time root, so M1's interior records FOLD onto a stale base. Where it
stops: the criterion is identified — at M2's open, a ring-0 interior record
(`level > 0`, a slot-tree kind) for a slot whose tree-0 `slot_state` names
ANOTHER appender (`leased`/`releasing`, not us) is exactly the dead
recoverer's structure and must be STASHED, not routed (`backend.zig`'s
second `tree.replayEntries` pass; `openGuestTrees` must not open that tree);
the recovery's step 4 (`recovery.zig`, between `installRoot` and the dead
window's `replay`) applies the stash `(level DESC, seq)`; the RAII rollback
puts it back. Nothing of this is written; the RAM recovery fixture does not
model a dead recoverer, and the crash-window row ("M1 dies after N SMOs on
X's tree; M2 recovers; every record X acked present") is the red test to
write first.

### 5e. The un-share protocol — NOT DONE, untouched

`squeezefs nvmeof unshare` / the set-level un-share (the data volume's
removal from the fabric under live members) was not started in this rung;
`.wt-ops` landed `volume add-data` / `remove-data` learning at the joiner's
renewal, which is the natural home for the un-share's membership word.

## 6. Gates

Each landing ran `zig build gate` on both venues at the landed commit; the
rail's allowlist was rebased per commit (line shifts only). Linux totals at
the final tip ride the landing commit's gate log (`/build/tmp/sym-gate-9.log`);
macOS 28/28 steps. Flakes attributed against the shared venue, each re-run
green alone: the shim smoke's `sessions1 > sessions0` under load (item 1);
the three `mem.budget` tier rows reading a divided root (`0.4 ×` instead of
`0.8 ×` the cgroup cage) while another agent's daemon was live on the box.
