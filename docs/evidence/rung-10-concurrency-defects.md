# Rung 10 — five concurrency defects (2026-10-13)

The races the pack-read agent stated for their owners (`rung-10-pack-read-tls-teardown.md` "Found
on the way") and the two flaky rows its gates met. Decision 0003 §W.1–W.4; decision 0002 rows
512–513. Every loop below ran on the venue (9 vCPU) under an 8-way `yes` load beside the other
agents' work; each failing run's output was kept.

## 1. The holder's free-list publication clobbered a racing carve's block

**Root cause.** `publishFreeList` let the holder's finish-free sink clear the bitmap bit (under
the holder's ordering lock) and only after the sink returned stored the cell's `free` state and
credited the budget. A carve (the same lock: `DataBitmap.carveRun` → `holderTakeBit`) and the
holder's mint landing between the two took the block from `limbo` (no debit), minted it
`allocated` — and the late stores overwrote it to `free` and added a credit no clear bit backs.
A dead-epoch quarantine of that live block then asserted on its lost take (`quarantineBlock`'s
`assert(took)`). The bit-first consumers had the twin: `claimFreeForTrim`, `quarantineBlock`'s
free arm and the fsck free-list eviction took a bit, then debited a credit the publication had
not made yet (the debit's underflow assert). `returnFromTrim` and `holderReturnBit` stored the
state after the bit too. **No double hand-out was reachable**: every take goes through the bit
(`tryTakeBit`, the unique 0→1 winner); the defect was the cell's word and the budget lying about
the block — a lie that ended in an assert (the quarantine, the underflow) and in an allocated
block that never deposited into the read tiers (`fillDepositable` reads `free`).

**Fix** (0003 §W.1, 0002 row 512). Every release path stores the state FIRST (`free` below the
cursor, `virgin` at/above), then clears the bit — the clear's acq_rel is the release a
claimant's acq_rel `tryTakeBit` acquires, so the claimant's own state store lands after ours —
then credits (bit before budget, as before). On the holder the bit and the credit move in ONE
critical section (`holderPublishFree`, the sink's half, was `holderClearBitRaw`): the carve is
the one consumer that takes its bit before its debit, and under the same lock its credit is
always there. Every other consumer wins its entitlement before its bit and refunds a lost take;
a quarantine's lost take is the flag alone (as the limbo arm).

**Model** (`src/data/free_list_model.zig`, new). Model 1 — the local free list: the publisher
(state → bit → budget, the state store buffered until the bit's release) against a
budget-then-bit claim (which meets the published block first) and a trim claim of it. Model 2 —
the holder: the publisher (state → lock{bit, credit}) against the carve (lock{take, debit if
`free`, `virgin`}) and the mint's `allocated`. Both hold. Weakenings, each proven to fail:
model 1 `bit_first` (the shipped order: the claim's `allocated` overwritten — FinalViolated),
`state_buffered` (the state store not ordered before the clear — FinalViolated), `trim_bit_first`
(the shipped trim: an absent credit debited — InvariantViolated); model 2 `bit_first` (the
explorer's first schedule is the carve between the state and the credit — an underflow; the
replayed schedule `{P, K, K, P, P}` ends `MintedBlockClobbered` with a phantom credit) and
`credit_outside_lock` (an underflow).

**Red → green.** `data.block_alloc`: `the holder's free-list publication: a carve and a mint
landing the instant the sink's critical section ends find the block published …` — the seam is
the sink itself (the holder's bit half, then the carve's take and the window's mint, before
`publishFreeList` resumes). Red: `expected .allocated, found .free`. The seams row's double-free
arm now reads `free` (the state stored before the bit; nothing credited).

## 2. A member's cached copy of another member's block outlived the block's re-use

**Root cause.** The designed cleanup is the token recall's purge (spec §1.7 step 2: A's delete
recalls every reader's token on the file, each reader drops the file's block keys before it
acks, and A frees only after the acks — "the recall IS the qualification"). The wire plane's
`RecallSink` answered `no_entry` and its census purged nothing; and the token cache destroyed the
revoked entry before the purge ran, so no sink could have enumerated the keys anyway. A block
this member never touched has word 0 here whatever its owner does, so nothing else ends its tier
entries. A second window rode along: a fill whose DMA lands before a recall's purge and whose
deposit lands after it leaves the old lifetime's bytes behind — the reference closed it with the
observed drain of in-flight serves; this port's serve stamps cover token serves, not block fills.

**Fix** (0003 §W.2, 0002 row 513; the spec's §1.7 port note).
- `token_cache`: a revoked entry stays pinned through the purge and `RecallSink.discardLayout`
  receives it — the inline ack, the drain lane's deferred ack, a voluntary release or eviction,
  a stop, a re-point; a revoke that cannot allocate purges over no entries (the census).
- `wire_plane.RecallSink`, bound to the data plane at mount (the wire and the `-o ro` reader
  plane): the entry's carried `layout` → every key it names through the ONE purge
  (`DataPlane.recallPurgeLayout` → `ReadPath.purgeRecalledLayout`); no entry, a block-map tree
  head or an undecodable value → `no_entry` → the census once per recall.
- `purge.purgeCensus`: a structural census face on every tier (`nextKey`, a cursor walk under
  each shard lock, nothing allocated — a recall's fallback cannot fail half-done); every picked
  key goes through `purgeBlockKey`. The recall's census picks FOREIGN keys only
  (`ReadPath.purgeForeignCensus`): a key inside a local lifetime is its incarnation's and its own
  free's.
- `ReadPath`: a binding loss after a fetch that may have deposited a foreign key purges it
  (`loseForeign`) — the recheck after the deposit is the read that sees the revoked token (a
  revoked entry misses and refetches; a re-grant before the owner's commit is parked or recalled
  again, `grant_gate_model`); a speculative fill rechecks a foreign deposit the same way; the
  deposit-only paths with no recheck after them (a victim's dehydration, the hold-serve
  ceremony) never deposit a foreign key.

No model: the window is a protocol over the token cache, the tiers and the read path rather than
one lock-free core; the rows below hold each interleaving with a seam.

**Red → green.**
- `data.read_path`: `two members over one volume: B caches A's block (a cell B never touched), A
  deletes the file, frees the block and re-mints it with new bytes …` — red: `expected 195,
  found 0` (the old file's bytes).
- `data.read_path`: `… a fill of A's block whose DMA lands BEFORE the recall's purge and whose
  deposit lands AFTER it …` (the fixture's `after_read` seam runs the recall inside the fill) —
  green; with `loseForeign` removed it fails (the tier still holds the key).
- `data.read_path`: `the recall's census fallback drops every FOREIGN key and keeps every key of
  this member's lifetimes; a block-map tree head is not enumerable …`; `data.purge`: the census
  drops exactly what its selector picks, from every tier, through the ONE purge.
- `meta.sym.tokens`: `the recall's purge reads the REVOKED entry's carried layout …` — the inline
  ack, the deferred drain-lane ack and a voluntary release each hand the sink the entry's
  `layout`; a shed object's recall runs the census once.
- `smoke: A MEMBER'S CACHED COPY OF ANOTHER MEMBER'S BLOCKS NEVER OUTLIVES THE BLOCKS' RE-USE`
  (two daemons, two 192 MiB data volumes of 1 MiB blocks; B joined with the dest lease off so
  its block-straddling O_DIRECT window read is the lane's whole-block fill that deposits; A fills
  the set one block per file, `rm f`, writes `g` into `f`'s blocks): on pristine `dev`
  (`f94a587`, the row applied alone) B reads `g` as **`stale-f`**; on this branch `equal`, B's
  recall of `f` counting scoped purges +2 and purged keys +2, census 0 — 5 of 5 runs.

## 3. `Listener.retire` read the listener after the decrement that lets `stop` free it

**Root cause.** A connection thread's last act was `fetchSub` on the live count, then
`self.live.load()` for the gauge and the futex wake. The decrement is what `stop` waits for; at 0
it frees the listener — so a connection retiring during a stop read freed memory. Plaintext and
TLS alike.

**Fix** (0003 §W.3). The station and the word's address are read before the decrement; the gauge
takes `fetchSub`'s own result; the wake names the word by its address (a private futex is keyed
by the address — the kernel reads no memory there: the waker-after-free shape every futex-backed
release has).

**Red → green.** `wire.tcp_e2e_linux`: `a connection retiring while the listener stops reads
nothing of the freed listener …` — `Listener.RetireStation` (null in the product, read before
the decrement) parks the retiring thread past its decrement while the test stops (frees) the
listener. Red: the live gauge read `1431655765` (0x55555555, the testing allocator's overwrite of
the freed listener), never 0. **Loop: 200 of 200** (this row and the 10k-verb row in one binary,
under the 8-way load).

## 4. `exec.lane`'s stack-trimmer row: SIGABRT

**Reproduced.** The trimmer-race row alone, 300 runs under the 8-way load on `dev`: **179 pass,
121 fail — 118 of them SIGABRT** (rc 134), 3 rc 1. Every failure: a spawn answered
`ConcurrencyUnavailable` with free slots in the pool; the row's early return then freed the lane
while its trimmer thread still ran, whose `tryPop` on the freed ring panicked (`index out of
bounds: index 6148914691236517205` — the 0x55 fill) → the abort the gate saw.

**Root cause** — a product defect in the pool, not a trimmed running stack. The trimmer can
never trim a stack a fiber runs on (ring ownership is exclusive: a slot is in the ring, a
spawner's, or the trimmer's). What it did was short the pool: it POPPED each free slot and pushed
it back, so (a) a spawn found no slot while the trimmer held the last free one, and (b) a
trimmer descheduled inside its push — between its enqueue claim and its cell's sequence store —
hid every slot queued behind that cell (Vyukov's head answers empty): the module doc's "at most
one slot short" was false under load. And `recycle` dropped `live` before its push, so `live ==
0` did not mean the pool was whole (the quiescence barrier `finalize` and the tests' waits read).

**Fix** (0003 §W.4). One claim word per slot (`Lane.claims`, outside the `Fiber` so a recycle's
reset never writes it under a trimmer's CAS): FREE / ARMED / TRIMMING. The trim claims a FREE
slot IN PLACE (acquire: the recycle's reset and stamp) and releases after the madvise (release:
the spawn sees `trimmed` and re-arms) — the ring never sees the trimmer. `spawn` claims the slot
it popped (FREE → ARMED) and waits out a trim (`exec_spawn_trim_waits`), never refusing. A
recycle releases the claim after the reset, pushes, then decrements `live`. The row stops and
joins its trimmer on every exit path.

**Model** (`src/exec/slot_pool_model.zig`, new): the ring as a set plus the claim words; a
recycler finishing slot 0's fiber, the trimmer's pass over both slots, a quiescer that waits for
`live == 0` and spawns the whole capacity. Holds. Weakenings, each proven to fail:
`trimmer_pops` (the shipped trimmer — the quiescer finds the pool short), `decrement_before_push`
(the shipped recycle — the pool short), `free_before_switch_out` (a running stack trimmed),
`spawn_skips_claim` (a running stack trimmed).

**Red → green.** `exec.lane`: `the idle trim never takes a slot out of the spawn pool …`
(`Lane.TrimStation`, null in the product, counts what a spawn could pop while the pass holds a
slot) — red: `expected 2, found 1`. New: `a spawn that pops a slot the idle trim holds waits the
trim out and lands (counted), never refused`. **Loop after the fix: 300 of 300** under the same
load.

## 5. `wire.tcp_e2e_linux`'s 10k-verb row read the accept thread's name too early

**Root cause.** A test race: the row probed `/proc/self/task/*/comm` for `sqz-wire-acc` right
after `Listener.start`, which spawns the thread and returns; the thread names itself as its first
act. **Reproduced**: the row alone on `dev` under the 8-way load (it runs first in its binary):
**17 pass, 183 fail of 200**, every failure at that line.

**Fix.** The rendezvous is causal, not a wait: the first call's reply happened after the accept
thread named itself (before its first `accept`), the connection thread (before its handshake)
and the mux reader (before its first read) — the name is read after c1's reply. No sleep, no
poll. **Loop: 200 of 200.**

## Commits

| item | red | fix |
|---|---|---|
| 1 free-list publication | `1aef1f1` (+ `free_list_model.zig`) | `2e5324b` |
| 3 `Listener.retire` | `1232d5c` | `0139aa1` |
| 5 accept-thread name | — (the loop above is the red) | `70a98b5` |
| 2 recall purge | `5bdd942` | `cda69e0`; the two-daemon row `7eb1b97` |
| 4 slot pool | `34ce37b` (+ `slot_pool_model.zig`) | `b27fcdd` |

Decisions `1f06746` (0002 row 513, 0003 §W.2–W.4; row 512 and §W.1 rode `2e5324b`); the literal
allowlist's moved lines `b833817`.

## Gates

- **Venue** (`zig build gate` on the rebased branch, a cache-busted build, load 6–8 with other
  agents' work): **46/46 steps; 2,774 of 2,844 passed, 70 skipped** — unit 2,535 pass / 68 skip,
  rails 16, shim-census 3, smoke 86 pass / 2 skip (the re-use row among them), bench-smoke 41,
  shim-smoke 40, nvmet-smoke 53. No red row.
- **macOS** (`zig build gate`, portable modules): **28/28 steps; 2,170 of 2,197 passed, 27 skipped** (the four models, the block
  allocator, read path, purge and token rows run here; the wire, exec lane and smoke rows are
  the venue's).

## Found on the way, stated, not changed

- **A quarantine admitted after a finish passed its gate leaves the block free-listed with the
  flag** (pre-existing: the limbo arm; now also `quarantineBlock`'s free arm on a lost take): the
  `finishFree` quarantine check runs before the publication, so an admission in between is the
  flag alone — the block reaches the free list flagged and the flag outlives the cohort. Owner:
  the custody plane.
- **The jobs coordinator's quarantine sink never finishes its cohort's frees**
  (`ops/jobs/coordinator.zig` `AllocSink.releaseFn`): `releaseQuarantine(epoch, &.{}, &out)`
  drops `out` (every OWED publish moved to `limbo` and never `finishFree`d — the blocks stay
  allocated forever) and passes no live admissions (a flag-alone admission is never released).
  Owner: the jobs fabric.
- **Lock-free bit takes on an armed holder race the carve's scan** (`claimFreeForTrim`,
  `quarantineBlock`): `carveRun` scans the lowest clear run under the holder's lock and asserts
  every `holderTakeBit` of it succeeds; a take outside the lock between the two trips the
  assert. Pre-existing; the entitlement-first order changes nothing there.
- **`sync -f` is no durability point for the daemon's write custody** (no `FUSE_SYNCFS`
  handler): while building the smoke row a peer read a 1 MiB file A had written, closed and
  `sync -f`'d as 1 MiB of zeros — the size committed, the data not yet published. The row writes
  with `conv=fsync`. Owner: the FUSE handler.
- **On the uring transport a member's aligned single-block reads never deposit**: the warm
  ladder's direct device leg and the leased window DMA straight into the reply; a foreign block
  reaches the tiers only through the lane's whole-block fills (why the smoke row's B runs with
  `SQUEEZEFS_READ_DEST_LEASE=0`).
