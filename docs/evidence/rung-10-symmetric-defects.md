# Rung 10 — the symmetric plane's open defects after the tripwire hunt

`docs/evidence/rung-10-tripwire-hunt.md` §5, §5.1 and §8 left four items on the post-perf3 base:
C's `ls` listing one more name than `find` in the shared hot directory row's phase 3, 11 cross-owner
intents still open at A after C's kill in phase 4, `dlm_token_nonmember_refusals` firing in a
passing run, and the owed chunked `extent_grant:` record behind the custody move's refusal past one
tree-0 value. Decisions: 0003 §AE; 0002 row 525.

## 4. The grant record as a family of chunks (item 4)

### 4.1 The limit

The custody move of 0003 §AD.1 writes the grantee's `extent_grant:{a}` record gaining a moved
tree's live images in the lease's ONE control entry (T1) and the departing record losing them in
the release's (T5). The record was ONE tree-0 value: `extentGrantMaxRuns(value cap)` runs — 4,111 at
the default 256 KiB node, 1,039 at 64 KiB — and a move past it refused (`Rejected`,
`extent_grant_custody_cap_refusals`): the lease did not move. A tree a joiner did not grow carries
its images scattered (the holder's SMOs allocated them among everything else), so a slot whose tree
holds more than ≈ 4 k scattered images could never be handed over or released into a lessee's
record — an availability limit a long-lived slot meets at scale. The same cap cut carves and refused
returns that would split a run past it (`extent_return_run_cap_refusals`).

A record value is not the only bound: one control entry carries at most `MAX_ENTRY_LEN` = 128 KiB,
so even a record split into several values cannot be rewritten whole in one entry once it passes
≈ 8 k runs, and a release entry that already carries an inline `slot_tails` record near the value
cap had no room for a 64 KiB record beside it (the entry refused `ValueTooLarge` → `Corrupt`).

### 4.2 The format and the law (0003 §AE.4, 0002 row 525)

- **Key** `kind ‖ tag 3 ‖ appender u32 BE ‖ id u64 BE` (14 B): one appender's chunks are ONE range
  walk (`cr.extentGrantFamily`), the move marker at id `u64::MAX` sorts last.
- **Value** `ExtentGrantChunkHead` (48 B: version 2, kind runs/marker, `n_runs`, the key's appender
  and id repeated, a marker's staged range, the `checksum.stamp` over the whole value) ‖ 1 … 253
  `GrantRunWire` (16 B each). 253 runs = one 4 KiB value: one journal page, the grain every entry
  pays — and below the smallest node's value cap (16,640 B at 64 KiB; `grant_record.zig` asserts it
  at comptime). The decoder (`decodeGrantChunk`) refuses a stamp that fails, a head naming another
  key, a non-zero reserved word, an empty, unnormalized or over-long run list, a marker away from
  its id or staging an empty range; it is total under the tree-0 fuzz target (the census rail).
- **The record** is the union of the effective chunks — pairwise disjoint, adjacent runs across a
  chunk bound coalesced (`grant_record.Family.load`).
- **A change is a replacement** (`Family.plan`): each effective chunk owns the extent range from its
  first run to the next chunk's first run (the first chunk from 0); the target run set is cut at
  those bounds and every chunk whose share changed is deleted, its share written as fresh chunks
  under ids the family never used. A carve or a return touches one or two chunks; a custody move
  touches the chunks its images fall in.
- **A replacement that does not fit its change's entry is STAGED** (`grant_record.Writes`): the
  first staged entry writes the MOVE MARKER naming the fresh ids `[lo, hi]` — a chunk inside a
  present marker's range counts for no reader — the fresh chunks follow in entries of half the
  entry payload (`stagedEntryRoom`), and the change's own entry (the slot state, the carve's
  `alloc` deltas, the return's frees) deletes the replaced chunks and the marker. Before that entry
  lands every reader sees the old record; at it, the whole new one. A change whose own entry never
  lands leaves the marker: the next load of that record (`Manager.readGrantRecord`, the recovery's
  `loadGrant`) deletes its staged chunks, then the marker (`extent_grant_markers_swept`, a warning);
  readers meanwhile never count them. Counters `extent_grant_staged_changes`,
  `extent_grant_staged_chunks`.
- **The envelope**, stated: the change's own entry carries one 32 B delete per replaced chunk —
  ≈ 2,000 chunks (≈ 500 k runs touched) beside the largest change record; past it the change
  answers `Rejected`, loud. A carve is cut to `carveExtentsMax()` = 2,298 extents: each claimed
  extent costs its `alloc` frame and, at worst, one chunk delete in the entry that lands it (the
  old run-cap cut, now the entry's own bound; the old code's single entry overflowed at ≈ 4 k
  claimed extents anyway).
- **Readers**: the manager (`readGrantRecord`, `heldElsewhere` over every family), the recovery
  (`Volume.grantFamily`: step 3's screen and kept-extents, step 7's rewrite as one replacement —
  staged when large; a re-run after a crash between its entries sweeps the marker, so the record
  never names less than the old or more than the new), the volume binding's return census, and
  fsck: the forest census reads every volume's grant records whole and a family that does not
  assemble is `C14GrantRecordCorrupt{vol, appender}` (report-only — the record is the manager's
  only attestation of the grant).
- The refusals are gone: `extent_grant_custody_cap_refusals` and `extent_return_run_cap_refusals`
  are retired with the run cap.

### 4.3 The red row and the rows beside it

- **Red** (`c372f34`, portable, `slots/acquire.zig` "a slot whose tree holds more images than one
  tree-0 value carries MOVES"): the manager's RAM volume at 64 KiB nodes; slot 5's tree unleased
  under a root; a `treeImages` seam answering 1,500 scattered extents (every other one of
  [1000, 4000)); appender 2's `AcquireSlots`. On `dev`: `error.Rejected` — the custody cap. Green
  (`ea8ed23`): the grant lands, `extent_grant_images_transferred_in` + 1,500, the record holds
  exactly the even extents in ⌈1,500 / 253⌉ = 6 chunks, and appender 2's `ReleaseSlot` takes all
  1,500 back out (`_out` + 1,500, the family empty).
- `manager` "a custody move whose chunks do not fit the lease's entry is STAGED": 1,400 scattered
  images against an entry with 8 KiB of room — the marker and the six fresh chunks land first and
  the record reads exactly as before them; the lease's entry makes all 1,400 effective at once; a
  second move whose entry never lands is swept (`abandonStaged`) and the record stands as it was; a
  marker left behind is swept by the next verb's read.
- `grant_record` (RAM tree 0): a replacement rewrites only the chunks whose share changed (one run
  grown: one delete, one put), ids never reused while the family stands, the family gone when the
  record empties; 12,000 scattered images staged across entries each within the staged room, no
  reader counting a staged chunk until the final entry; a family that does not assemble is
  `Corrupt` (overlapping chunks, a chunk under another appender's key, a failing stamp); adjacent
  runs across a chunk bound read as one.
- `control_records` "extent_grant chunks": round trip, the key binding, every refused spelling
  byte-wise even re-stamped, the marker, the 253-run tie; `fsck` "C14 reads every grant record
  whole"; `extent_grant` "subtractRuns".
