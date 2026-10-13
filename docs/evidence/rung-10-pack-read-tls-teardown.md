# Rung 10 — a member's read of another member's packed tenant; the MutualTls teardown (2026-10-13)

Two defects the jobs agent stated (`rung-10-jobs.md` §3b, §4). Decision 0003 §V; decision 0002
row 510.

## 1. A manager's read of a live joiner's packed tenant answered EIO, intact or not

**Red.** `smoke: THE FLEET'S MERGED BLOCK CENSUS` gained the read its seam-off half owed: after
B (joined) packs 8 staged 64 KiB files by the fsync lever, `rm`s one and `fsck --online` reads
0 findings, A (the manager) reads B's seven remaining tenants while B is live
(`mergedPackReadOn`). On `dev` every one answered `Input/output error`; B's own reads after a
remount were sha-equal — the bytes were intact. A two-daemon set WITHOUT the row's seam-on half
(the same fill, `rm`, `fsck --online`, B's remount) read sha-equal on A every time: the EIO
needed history.

**Root cause.** The incarnation word is a member's own transition ledger: `gen << 1 | stable`,
unstable while a transition runs HERE (a claim before its publish, a W1 patch, a free in
flight). A terminal free retired the word (`beginFree`: gen + 1, unstable) and nothing ever
re-stabilized it: the next STABLE was this member's own next mint of the block. On a set the
free list is the holder's bitmap, and the carve's frontier falls back to block 0 once every
open grant retired — so a block A freed is the next carve, very often for ANOTHER member. B
mints it on B's allocator; A's cell never hears of that lifetime. The row's seam-on half has A
free and punch the pack's block 1 (the reintroduced defect); the seam-off half's pack lands on
block 1 again; A's `readMappingWindow` takes `fillIncarnation(key) == null` ("mid-write") and
answers EIO — for every tenant, intact or not, forever (until A itself re-mints the block).
The same history kills a STRIPED block: the ranged loop rebinds 8 times, the whole-block walk
escalates, the serialized settle reads `serve_valid = false` four times, fires
`read_settle_lost_serialized` (must-stay-0) and answers EIO. A write into such a packed file
from the other member fails the same way (`DataPlane.readMappingImage`, the re-stage source).
Not candidates after all: the tenant's layout (A reads B's record through the token, the right
version), the window table (the window law ran clean, `packed_mapping_refusals` 0), custody
(no foreign-custody refusal exists on the read leg).

A second gap rode along: the packed read validated the window by the word ALONE — no binding
recheck — so a tenant rebound between the resolve and the read (a re-pack, a delete with its
block re-minted) served the old window's bytes. The read path's own law is (a) incarnation ∧
(b) the binding after the bytes; the window leg had only (a).

**Fix** (0003 §V.1; 0002 row 510).
- `BlockAllocator.endLifetime`: a local lifetime's END re-stabilizes the word under the
  generation the retire minted — at the free-list publication (`publishFreeList`, FIRST, while
  the cell is still `limbo` and before the bit makes the block reusable, so no claim can
  interleave) and at a give-back to the grant window. Unstable means a transition in flight
  here, nothing else; a block another member re-mints reads valid on this member and the
  binding proves the serve.
- `BlockAllocator.fillDepositable` + `ReadPath.depositable`/`depositIncarnation`: a stable word
  over an ended lifetime covers bytes of no lifetime of this member's, and no foreign publish
  purges this member's tiers — so a cell outside a local lifetime (`limbo`, `free`, `virgin`,
  `trim_windowed`) with a non-zero word NEVER deposits (the fill, the hold-serve ceremony, a
  victim's dehydration); it serves on its `still` and the caller's binding recheck. A
  never-touched cell (word 0) deposits as before (its foreign lifetime's end is the token
  recall's purge — see "Found on the way" below).
- `abandonUnpublished` (a mint given back to the grant window): the cell leaves its lifetime —
  the count released, the word retired and re-stabilized, `virgin` — BEFORE the window takes the
  block back. The window's head is the next mint: a claim taking it between the give-back and
  those stores met a live count (`block_claim_anomalies`, forced to 1) that the release then
  zeroed — a live block at refcount 0 reading `virgin` (pre-existing; this change's
  re-stabilize would also have published that claim's word mid-DMA). A give-back the window
  refuses restores the count and the state for the terminal free.
- `ReadPath.readPacked`: the window, then the binding recheck (the same mapping still current
  after the bytes), a move re-resolves (`stale_binding_rebinds`, bounded by `IDENTITY_LOOP`, a
  kind change re-enters the entry); the window answers a verdict (`served` / `moved` — the word
  moved under the read / `corrupt` — a frame that did not decode: EIO only when the binding
  stands). The staged durable copy's identity recheck takes the same verdicts.

**Model 7** (`incarnation_model.zig`, `an_ended_lifetime_never_deposits_under_its_restabilized_word`):
the free (retire → purge → the `limbo` store, buffered → the re-stabilizing publish → the free
list) against a fill (snapshot → payload → still → the state load → the tiers' insert-time
validate). Holds; the WEAKENING `publish_relaxed` (the publish without its release — the `limbo`
store still buffered when the stable bit lands) fails: the fill reads the stable generation-2
word, generation-1 bytes, an `allocated` state, and deposits an ended lifetime's residue.

**Red → green tests.**
- `data.block_alloc`: `a local lifetime that ENDED leaves the word stable under its retired
  generation …` (red: the snapshot after the free was null); the grant-window seams test's
  abandoned mint (red: unstable after the give-back).
- `data.read_path`: `two members over one volume: blocks THIS member freed and ANOTHER member
  re-minted …` — A's allocator freed blocks 5 and 6, B's allocator minted them (8 packed tenants,
  one striped block): red at the first tenant (EIO in `readMappingWindow`); with the packed arm
  skipped, red on the striped block (EIO after the settle, `read_settle_lost_serialized` fired);
  green: every tenant and the block byte-exact, whole and ranged, nothing in any tier, the
  next read fetches again, no tripwire. `a packed tenant whose binding moves between its window
  DMA and the recheck re-resolves …` (red: the old tenant's bytes served).
- `data.block_alloc`: `an abandoned mint leaves its lifetime BEFORE the grant window can hand it
  out …` (red: the racing claim fired `block_claim_anomalies` and was left at refcount 0).
- `smoke: THE FLEET'S MERGED BLOCK CENSUS` (red on `dev`: 7 × EIO): `A while B is live: 7 packed
  tenant(s) of fixed.sha sha-equal`.
- The read-path fixtures' `bind` modelled a block as published-never-claimed (a cell state the
  product never makes); it now claims first (`Fixture.mint`), so their fills keep depositing.

### Found on the way, stated, not changed

- **A never-touched cell's tier entry outlives the foreign lifetime it holds.** A member caches
  another member's block (word 0, deposited as before this change); that member frees and
  re-mints it; a later read of the new file hits the old entry. The designed purge is the token
  recall's (`token_cache.Reader.purge` → `RecallSink.discardLayout` / `purgeCensus`, spec §1.7
  step 2); the wire plane's `RecallSink` answers `no_entry` and purges nothing ("the planes'
  device-path arm is owed", `wire_plane.zig`). Packed tenants never use the tiers (the window
  leg), so the merged-census row cannot see it; a striped block can. Owner: the symmetric
  plane's device-path arm.
- **The holder's free-list publication clobbers a racing claim's cell.** On the holder,
  `publishFreeList` clears the bitmap bit through the sink and THEN stores `free` and credits the
  budget: a carve and a claim in between leave an allocated block reading `free` and one extra
  budget credit (pre-existing; the give-back's twin above is fixed). `endLifetime` runs before
  the bit clears, so the word never re-stabilizes a racing claim's mid-DMA lifetime there.
