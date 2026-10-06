# 0002 — No compatibility obligation with the Rust reference; quirks are not requirements

Status: accepted 2026-10-05 (owner's ruling: "let's remember not to bring poor design decisions
over into the new code base / filesystem; the old application doesn't have to be able to mount the
new version").

## Decision

1. **The charter is the specification; the reference is evidence.** We port the charter's goals,
   mechanisms, invariants, laws and measured derivations. Where the reference's code embodies a
   choice the charter does not require and a better design exists, we take the better design and
   record it here.
2. **No on-disk or wire compatibility with the reference.** The Rust binary never has to mount a
   Zig-formatted volume, and vice versa. Byte-identity with the reference's images (achieved for
   `format` in rung 2a) was a correctness oracle of convenience, never a requirement. The format is
   still forward-only among OUR releases (charter §1.7): once a Zig release ships, later Zig
   binaries refuse older incompat bits loud — compatibility obligations begin at our first release,
   not before.
3. **The reference binary stays an oracle where it is cheap.** While our format still matches,
   `/build/ref/squeezefs fsck --offline` remains a free second opinion. The moment a decision here
   changes the format, the cross-checks against the reference that touch that unit are retired and
   OUR fsck (rung 8) becomes the oracle; the Zig-side invariants (replay ≡ RAM, SMO completeness,
   the kill matrix) already stand on their own.
4. **Every departure is recorded below with its reason**, so a later wave never "re-imports" a
   quirk by reading the reference.

## Ledger of reference quirks NOT carried (and what we do instead)

| # | Reference quirk | Why it is poor | What we do |
|---|---|---|---|
| 1 | Journal admission as a byte-count word CAS'd up and decremented on transfer | ABA-blind: over-commits under a 3-party race (rung 2a, model 6, 450 states) | Monotonic totals (`admitted_total` as the CAS word); landed |
| 2 | Layout-delta base (`0x4C31`) encoded with Rust `bincode` (trailing bytes allowed, any map order) | A serialization-library artifact as an on-disk format; not canonical, not total in the charter's sense | Rung 4 defines the layout record as an `extern struct` with a canonical total codec; the bincode decoder in `record.zig` is retired then |
| 3 | Two delta classes (Δinode vs layout delta) with class-specific fold arms | Two code paths for one algebra | One `Delta(field_mask, fields)` kind over a field table generated per record type (rung 4) |
| 4 | Inode value: varint version tag + `rdev` stored in the old `flags2` slot (57 B) | Legacy slot reuse that the charter's "≈ 96 B packed inode" never asked for | A fixed-width `extern struct` inode value with explicit fields incl. `rdev` and `flags2`, size-asserted (rung 4, with the layout record) |
| 5 | DUR-8c `next_ino` recovery raises the LOCAL watermark with a dentry's GLOBAL child ino | Mixes keyspaces; inflates the watermark (`next_ino 1245187` after 20 locals) | Fold per keyspace: route the child ino through `inos.routeIno` and raise only its own slot's watermark (fix in `inos.zig` with a red test) |
| 6 | The journal lane and the conveyor pass on different threads (cross-thread wake hop measured 59–62 % of the write round trip) — the charter already corrects this | — | Both stages on the ONE lane thread once the executor lands (rung 3) |
| 7 | `std::sync::Mutex`-class locks inside hot-ish paths of the reference's cache (eviction ring, retired set) | The charter bans blocking locks on the data hot path | Miss-path only in `node_cache.zig` (documented); hot reads are latch-free through `ArcSwap` |
| 8 | fsck refuses a volume with no `format_config` xattr ("is this volume formatted?") | A CLI bootstrap requirement smuggled into the format | Our fsck judges the superblock, not a config blob |
| 9 | Panic-guarded tasks (`catch_unwind`) as the error boundary | Hides programming errors behind a counter | Zig has no unwinding: recoverable outcomes are error unions + tripwires; a true bug aborts loud |
| 24 | Threshold writeback: the SMO task's maintenance pass pops each tree's `VecDeque` under the backend mutex (the port had grown a second, task-private queue the commit path never filled, so the one-bset-page writeback never ran on a mounted volume) | A locked queue on the commit path's threshold push; two queues that never meet | ONE queue family — the trees' lock-free Vyukov rings (model-checked) — drained by the checkpoint task round-robin through `checkpoint_cycle.MaintenanceSource`; landed 2026-10-05 (runtime audit #4) |
| 25 | The SMO's images-before-pointers barrier failure swallowed (`catch {}` on the hook; the reference's `j.barrier()?` propagated) | A pointer record could publish to an image that never reached media — the one durable-order obligation the SMO protocol rests on | `SmoJournal.barrier` returns `BarrierFailed`, counted (`meta_kv_smo_barrier_failures`) and on the latch's barrier class; the build is abandoned with its extents released, nothing published; landed 2026-10-05 (#6) |
| 26 | ONE `sync_device` (a blocking pool `fdatasync`) run by both the tick's deferred flush and the cycle, so every barrier drained reclamation (the port split the streams and only the checkpointer's fed DUR-3) | A blocking `fsync(2)` on the pool stalls every CQE behind it; a second barrier issuer that reclamation cannot see | The lane's `FSYNC\|DATASYNC` SQE on its own ring takes a DUR-3 ticket before the device op and lands it at the CQE (`Checkpointer.barrierStarted`/`barrierLanded`, two lane hooks); the tick issues no barrier of its own; `coveringBarriers` is the constant 1; landed 2026-10-05 (#3) |
| 27 | Admission parkers woken by the checkpoint task's loop after a run that moved `reusable_upto`, with the 10 ms park tick as the transport on every other path | A timer-dependent liveness (charter §9: the tick is a backstop, not a transport); `full_stalls` counted ticks, not stalls | The Checkpointer owns the ONE `sync.Notify` and `afterDurableBarrier` is its one notifier whichever thread's barrier moved the watermark; the tick is a counted lost-wake backstop (`meta_conveyor_park_tick_rescues` ≈ 0); one stall per park entry; landed 2026-10-05 (#9) |
| 28 | Rollback / compensation exits on allocation failure, an unresolvable leaf or an exhausted lock budget returning silently (the reference's `PassSentinel` latched FAILED whenever `applied_unrolled` could be true — the port kept the sentinel's name and lost its latch) | RAM ≠ replay with no signal: records in an overlay whose entry never lands, or a compensation the ring never carries | `divergedFromReplay`: a per-site tripwire (`conveyor_divergence_{rollout,rollback,compensation}`) + `failStop` at every such exit, the unwritten compensation entry included; landed 2026-10-05 (#20) |
| 29 | A lane whose ring cannot `io_uring_enter` fails its in-flight windows and refuses later ones, and latches FAILED only after three later window verdicts | "Dead lane, not FAILED" on a quiet volume; charter §1.8 names a device fence terminal | `failAll` fail-stops the latch at once and counts `journal_lane_rings_dead`; a `kill_ring` seam pins it; landed 2026-10-05 (#21) |
| 30 | The coalesced flush interval `50` as "the legacy v2 flusher cadence reused", defined in the lane, the checkpointer and the registry row | A hand-me-down with no reason on the line, in three places (charter §1.5) | `checkpoint.DEFAULT_FLUSH_INTERVAL_MS = CHECKPOINT_MAX_AGE_MS / FLUSHES_PER_CHECKPOINT_WINDOW` (20 barriers per window bound acked-but-volatile metadata to 5 % of the cadence); the lane imports it; the registry row is the explicit override (0 = strict) whose default string a tie test parses against the derivation; landed 2026-10-05 (#1, #2) |
| 31 | One `heap_full` posture word on the backend plus `heap_promised` on the cache (the port had grown a second posture inside the pure space audit and a second promise ledger, both reached only by their own tests) | Two sources of truth for one posture; a future wiring would double-count `meta_kv_heap_full_cycles` | `heap_law.HeapFullPosture` is THE posture; `alloc_core.SpaceAudit` keeps the wedge rung alone; `alloc_ext.HeapPromises`/`recordCycle` deleted, their assertions alive on the surviving API; landed 2026-10-05 (#5) |
| 32 | The in-flight window map grown under its spin lock (a Rust `HashMap::insert` allocates under the mutex) | An allocation under a lock on the commit path (charter §1.3) | Sized once at init for the whole population (`queueCapacity(pages) + 1`), `putAssumeCapacity`, a population past it refused loud; landed 2026-10-05 (#15) |
| 33 | Shutdown waits the journal head stable (64 rounds on the registry's completed prefix) under the SMO mutex | Deadlock-free only by the close ordering nobody asserted; a bound with no reason on the line | The final tick asserts the registry quiet with the must-stay-0 tripwire `checkpoint_final_tick_open_reservations` and proceeds (its tail clamps below an open start; the next mount replays) — the ordering conveyor quiesce → lane shutdown → task stop made executable; landed 2026-10-05 (#31) |
| 34 | A 30 s caller-side timeout on every device READ (`sqz_time::timeout(read_timeout_ms, rx)`), its default in a `SQUEEZEFS_TEST_*` row, answering `ETIMEDOUT` per op | A per-op timeout wrapper (charter §4 bans them); a device that stops answering is a VOLUME event, not 10 000 app-level errors while the writes behind them park forever; a product bound in a test seam | Reads wait unbounded like writes and barriers; `Device.overdueCensus` folds every lane's submit stamps into `{in_flight, overdue, oldest_age_ns}` for the rung-3 watchdog, which logs loudly and fences the volume through the lattice; a read's device ERROR classifies through `classifyDmaOutcome`; the knob row and `ReadTimedOut` deleted; landed 2026-10-05 (decision 0003 Q5) |

When a wave meets a new quirk it adds a row here before diverging.
